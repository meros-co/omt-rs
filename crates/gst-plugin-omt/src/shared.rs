//! State shared between elements:
//!
//! * one [`SinkShared`] per published name - the OMT sender that video and
//!   audio sinks of that name both feed, its tally and its statistics;
//! * one [`SourceShared`] per received source - the clock recovery that keeps
//!   video and audio on one drift-corrected timeline, the tally the
//!   application sets, what the sender says about itself, and statistics.
//!
//! Both post element messages on the bus from their "owner" (the `omtsink` /
//! `omtsrc` bin, or the single element when used alone):
//!
//! | Message           | When                       | Fields                                    |
//! |-------------------|----------------------------|-------------------------------------------|
//! | `omt-stats`       | every `stats-interval` ms  | see `SinkShared::stats` / `SourceShared::stats` |
//! | `omt-tally`       | the combined tally changes | `program`, `preview` (booleans)           |
//! | `omt-sender-info` | the source's info arrives  | `product-name`, `manufacturer`, `version` |

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};

use gst::glib;
use gst::prelude::*;

pub static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "omt",
        gst::DebugColorFlags::empty(),
        Some("Open Media Transport"),
    )
});

/// VMX quality tier for senders. Never changes the picture size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, glib::Enum, Default)]
#[repr(u32)]
#[enum_type(name = "GstOmtQuality")]
pub enum Quality {
    #[enum_value(name = "Low: least bandwidth", nick = "low")]
    Low = 0,
    #[default]
    #[enum_value(name = "Standard", nick = "standard")]
    Standard = 1,
    #[enum_value(name = "High: best fidelity", nick = "high")]
    High = 2,
}

impl Quality {
    pub fn profile(self) -> omt::vmx::VmxProfile {
        match self {
            Self::Low => omt::vmx::VmxProfile::OmtLq,
            Self::Standard => omt::vmx::VmxProfile::OmtSq,
            Self::High => omt::vmx::VmxProfile::OmtHq,
        }
    }

    /// The matching "suggested quality" a receiver asks a sender for.
    pub fn request(self) -> Option<omt::receive::Quality> {
        match self {
            Self::Low => Some(omt::receive::Quality::Low),
            Self::Standard => None,
            Self::High => Some(omt::receive::Quality::High),
        }
    }
}

pub const DEFAULT_STATS_INTERVAL_MS: u32 = 1000;

/// Which element posts bus messages for a shared object: the first to claim
/// it, preferring an `omtsink`/`omtsrc` bin parent over the element itself.
#[derive(Default)]
struct Owner(Mutex<Option<glib::WeakRef<gst::Element>>>);

impl Owner {
    fn claim(&self, element: &gst::Element) {
        let mut owner = self.0.lock().unwrap();
        if owner.as_ref().and_then(|w| w.upgrade()).is_some() {
            return;
        }
        let bin = element
            .parent()
            .and_then(|p| p.downcast::<gst::Element>().ok())
            .filter(|p| {
                let name = p
                    .factory()
                    .map(|f| f.name().to_string())
                    .unwrap_or_default();
                name == "omtsink" || name == "omtsrc"
            });
        *owner = Some(bin.unwrap_or_else(|| element.clone()).downgrade());
    }

    fn post(&self, structure: gst::Structure) {
        let element = self.0.lock().unwrap().as_ref().and_then(|w| w.upgrade());
        if let Some(element) = element {
            let _ = element.post_message(
                gst::message::Element::builder(structure)
                    .src(&element)
                    .build(),
            );
        }
    }
}

/// Runs `tick` every `interval_ms` (re-read each time; 0 pauses) until the
/// returned flag is set or `target` is gone.
fn spawn_ticker<T: Send + Sync + 'static>(
    target: Weak<T>,
    interval_ms: Arc<AtomicU32>,
    tick: impl Fn(&T) + Send + 'static,
) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let _ = std::thread::Builder::new()
        .name("omt-stats".into())
        .spawn(move || {
            let mut last = Instant::now();
            loop {
                std::thread::sleep(Duration::from_millis(50));
                if flag.load(Ordering::Relaxed) {
                    return;
                }
                let Some(target) = target.upgrade() else {
                    return;
                };
                let ms = interval_ms.load(Ordering::Relaxed);
                if ms > 0 && last.elapsed() >= Duration::from_millis(ms as u64) {
                    last = Instant::now();
                    tick(&target);
                }
            }
        });
    stop
}

/// Bits per second from successive byte totals.
#[derive(Default)]
struct Rate {
    last: Option<(Instant, u64)>,
    bps: u64,
}

impl Rate {
    fn update(&mut self, bytes: u64) -> u64 {
        let now = Instant::now();
        match self.last {
            Some((t, b)) => {
                let secs = now.duration_since(t).as_secs_f64();
                if secs >= 0.25 {
                    self.bps = (bytes.saturating_sub(b) as f64 * 8.0 / secs) as u64;
                    self.last = Some((now, bytes));
                }
            }
            None => self.last = Some((now, bytes)),
        }
        self.bps
    }
}

fn tally_structure(tally: omt::Tally) -> gst::Structure {
    gst::Structure::builder("omt-tally")
        .field("program", tally.program)
        .field("preview", tally.preview)
        .build()
}

fn info_structure(info: &omt::SenderInfo) -> gst::Structure {
    gst::Structure::builder("omt-sender-info")
        .field("product-name", &info.product_name)
        .field("manufacturer", &info.manufacturer)
        .field("version", &info.version)
        .build()
}

// ───────────────────────────── sinks ─────────────────────────────

pub struct SinkShared {
    pub sender: Mutex<omt::Sender>,
    owner: Owner,
    pub stats_interval: Arc<AtomicU32>,
    rate: Mutex<Rate>,
    ticker: Mutex<Option<Arc<AtomicBool>>>,
}

impl SinkShared {
    pub fn claim_owner(&self, element: &gst::Element) {
        self.owner.claim(element);
    }

    /// `omt-stats` for a sender.
    pub fn stats(&self) -> gst::Structure {
        let (s, tally) = {
            let sender = self.sender.lock().unwrap();
            (sender.statistics(), sender.tally())
        };
        let bitrate = self.rate.lock().unwrap().update(s.bytes_sent);
        gst::Structure::builder("omt-stats")
            .field("connected", s.connections > 0)
            .field("connections", s.connections as u32)
            .field("video-frames", s.video_frames)
            .field("audio-frames", s.audio_frames)
            .field("video-dropped", s.video_dropped)
            .field("audio-dropped", s.audio_dropped)
            .field("bytes-sent", s.bytes_sent)
            .field("bitrate", bitrate)
            .field("tally-program", tally.program)
            .field("tally-preview", tally.preview)
            .build()
    }
}

impl Drop for SinkShared {
    fn drop(&mut self) {
        if let Some(stop) = self.ticker.lock().unwrap().take() {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

/// Live senders by (name, port); weak, so a sender closes with its last sink.
type SinkRegistry = HashMap<(String, u16), Weak<SinkShared>>;

static SENDERS: LazyLock<Mutex<SinkRegistry>> = LazyLock::new(Default::default);

/// The sender publishing `name` (on `port`, 0 = default), created on first
/// use. Video and audio sinks with the same name share it, so a receiver sees
/// one source carrying both.
pub fn sender(
    name: &str,
    port: u16,
    quality: Quality,
    advertise: bool,
) -> Result<Arc<SinkShared>, omt::Error> {
    let mut senders = SENDERS.lock().unwrap();
    let key = (name.to_string(), port);
    if let Some(existing) = senders.get(&key).and_then(Weak::upgrade) {
        return Ok(existing);
    }
    let mut config = omt::SenderConfig::new(name, 0, 0, (30, 1));
    config.port = port;
    config.profile = quality.profile();
    config.advertise = advertise;
    let shared = Arc::new(SinkShared {
        sender: Mutex::new(omt::Sender::new(config)?),
        owner: Owner::default(),
        stats_interval: Arc::new(AtomicU32::new(DEFAULT_STATS_INTERVAL_MS)),
        rate: Mutex::default(),
        ticker: Mutex::default(),
    });
    let weak = Arc::downgrade(&shared);
    shared
        .sender
        .lock()
        .unwrap()
        .on_tally_changed(move |tally| {
            if let Some(s) = weak.upgrade() {
                s.owner.post(tally_structure(tally));
            }
        });
    let stop = spawn_ticker(
        Arc::downgrade(&shared),
        shared.stats_interval.clone(),
        |s: &SinkShared| s.owner.post(s.stats()),
    );
    *shared.ticker.lock().unwrap() = Some(stop);
    senders.insert(key, Arc::downgrade(&shared));
    senders.retain(|_, w| w.strong_count() > 0);
    Ok(shared)
}

/// The sender already publishing `name` on `port`, if any.
pub fn existing_sender(name: &str, port: u16) -> Option<Arc<SinkShared>> {
    SENDERS
        .lock()
        .unwrap()
        .get(&(name.to_string(), port))
        .and_then(Weak::upgrade)
}

// ───────────────────────────── sources ─────────────────────────────

#[derive(Default)]
struct SourceCounters {
    video_frames: u64,
    audio_frames: u64,
    video_gaps: u64,
    bytes: u64,
    last_video_ts: Option<i64>,
}

/// Everything the video and audio halves of one received source share.
pub struct SourceShared {
    /// One drift-corrected timeline for video and audio.
    pub clock: Mutex<omt::sync::ClockRecovery>,
    pub drift_correction: AtomicBool,
    /// Tally the application has set for this source.
    tally: Mutex<omt::Tally>,
    controls: Mutex<Vec<omt::ReceiverControl>>,
    info: Mutex<Option<omt::SenderInfo>>,
    sender_tally: Mutex<omt::Tally>,
    counters: Mutex<SourceCounters>,
    connected: AtomicU32,
    rate: Mutex<Rate>,
    owner: Owner,
    pub stats_interval: Arc<AtomicU32>,
    ticker: Mutex<Option<Arc<AtomicBool>>>,
}

impl SourceShared {
    pub fn new() -> Arc<Self> {
        let shared = Arc::new(Self {
            clock: Mutex::default(),
            drift_correction: AtomicBool::new(true),
            tally: Mutex::default(),
            controls: Mutex::default(),
            info: Mutex::default(),
            sender_tally: Mutex::default(),
            counters: Mutex::default(),
            connected: AtomicU32::new(0),
            rate: Mutex::default(),
            owner: Owner::default(),
            stats_interval: Arc::new(AtomicU32::new(DEFAULT_STATS_INTERVAL_MS)),
            ticker: Mutex::default(),
        });
        let stop = spawn_ticker(
            Arc::downgrade(&shared),
            shared.stats_interval.clone(),
            |s: &SourceShared| {
                if s.connected.load(Ordering::Relaxed) > 0 {
                    s.owner.post(s.stats());
                }
            },
        );
        *shared.ticker.lock().unwrap() = Some(stop);
        shared
    }

    pub fn claim_owner(&self, element: &gst::Element) {
        self.owner.claim(element);
    }

    /// A connection came up: remember it for tally, and tell the sender the
    /// tally the application has set (libomt's receiver does on connect).
    pub fn connected(&self, control: omt::ReceiverControl) {
        let tally = *self.tally.lock().unwrap();
        let _ = control.send_tally(tally);
        self.controls.lock().unwrap().push(control);
        if self.connected.fetch_add(1, Ordering::Relaxed) == 0 {
            self.clock.lock().unwrap().reset();
        }
    }

    pub fn disconnected(&self) {
        let _ = self
            .connected
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
        if self.connected.load(Ordering::Relaxed) == 0 {
            self.controls.lock().unwrap().clear();
        }
    }

    pub fn set_tally(&self, tally: omt::Tally) {
        *self.tally.lock().unwrap() = tally;
        self.controls
            .lock()
            .unwrap()
            .retain(|c| c.send_tally(tally).is_ok());
    }

    pub fn tally(&self) -> omt::Tally {
        *self.tally.lock().unwrap()
    }

    pub fn sender_info(&self) -> Option<omt::SenderInfo> {
        self.info.lock().unwrap().clone()
    }

    /// Records what a receiver has learned from the sender's metadata,
    /// posting a message when it changes.
    pub fn learn(&self, receiver: &omt::BlockingReceiver) {
        if let Some(info) = receiver.sender_info() {
            let mut known = self.info.lock().unwrap();
            if known.as_ref() != Some(info) {
                *known = Some(info.clone());
                drop(known);
                self.owner.post(info_structure(info));
            }
        }
        let tally = receiver.sender_tally();
        let mut known = self.sender_tally.lock().unwrap();
        if *known != tally {
            *known = tally;
            drop(known);
            self.owner.post(tally_structure(tally));
        }
    }

    /// Counts a received video frame, spotting frames the sender dropped
    /// (timestamp gaps of more than 1.5 frame periods).
    pub fn count_video(&self, ts: i64, frame_ticks: i64, bytes: u64) {
        let mut c = self.counters.lock().unwrap();
        c.video_frames += 1;
        c.bytes += bytes;
        if let (Some(prev), true) = (c.last_video_ts, frame_ticks > 0) {
            let gap = ts - prev;
            if gap > frame_ticks * 3 / 2 && gap < frame_ticks * 600 {
                c.video_gaps += ((gap + frame_ticks / 2) / frame_ticks - 1) as u64;
            }
        }
        c.last_video_ts = Some(ts);
    }

    pub fn count_audio(&self, bytes: u64) {
        let mut c = self.counters.lock().unwrap();
        c.audio_frames += 1;
        c.bytes += bytes;
    }

    /// Maps a sender timestamp onto running time: drift-corrected by default,
    /// or (with drift correction off) a fixed offset from the first frame.
    pub fn running_time(&self, ts: i64, arrival: gst::ClockTime) -> gst::ClockTime {
        let mut clock = self.clock.lock().unwrap();
        let ns = if self.drift_correction.load(Ordering::Relaxed) {
            clock.map(ts, arrival.nseconds() as i64)
        } else {
            match clock.predict(ts) {
                Some(ns) => ns,
                None => clock.map(ts, arrival.nseconds() as i64),
            }
        };
        gst::ClockTime::from_nseconds(ns.max(0) as u64)
    }

    /// `omt-stats` for a source.
    pub fn stats(&self) -> gst::Structure {
        let (video, audio, gaps, bytes) = {
            let c = self.counters.lock().unwrap();
            (c.video_frames, c.audio_frames, c.video_gaps, c.bytes)
        };
        let (drift, correction, phase, jitter) = {
            let clock = self.clock.lock().unwrap();
            (
                clock.drift_ppm(),
                clock.correction_ppm(),
                clock.phase_error_ms(),
                clock.jitter_ms(),
            )
        };
        let bitrate = self.rate.lock().unwrap().update(bytes);
        let tally = self.tally();
        let sender_tally = *self.sender_tally.lock().unwrap();
        gst::Structure::builder("omt-stats")
            .field("connected", self.connected.load(Ordering::Relaxed) > 0)
            .field("video-frames", video)
            .field("audio-frames", audio)
            .field("video-dropped", gaps)
            .field("bytes-received", bytes)
            .field("bitrate", bitrate)
            .field("drift-ppm", drift)
            .field("correction-ppm", correction)
            .field("phase-error-ms", phase)
            .field("jitter-ms", jitter)
            .field("tally-program", tally.program)
            .field("tally-preview", tally.preview)
            .field("source-tally-program", sender_tally.program)
            .field("source-tally-preview", sender_tally.preview)
            .build()
    }
}

impl Drop for SourceShared {
    fn drop(&mut self) {
        if let Some(stop) = self.ticker.lock().unwrap().take() {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

/// The running time "now" for `element` (0 before it has a clock).
pub fn running_time_now(element: &gst::Element) -> gst::ClockTime {
    match (element.clock(), element.base_time()) {
        (Some(clock), Some(base)) => clock.time().saturating_sub(base),
        _ => gst::ClockTime::ZERO,
    }
}
