//! State shared between elements: one OMT sender per published name, and the
//! time base that maps a sender's OMT timestamps onto pipeline running time.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use gst::glib;

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

pub type SharedSender = Arc<Mutex<omt::Sender>>;

/// Live senders by (name, port); weak, so a sender closes with its last sink.
type SenderRegistry = HashMap<(String, u16), Weak<Mutex<omt::Sender>>>;

static SENDERS: LazyLock<Mutex<SenderRegistry>> = LazyLock::new(Default::default);

/// The sender publishing `name` (on `port`, 0 = default), created on first
/// use. Video and audio sinks with the same name share it, so a receiver sees
/// one source carrying both.
pub fn sender(
    name: &str,
    port: u16,
    quality: Quality,
    advertise: bool,
) -> Result<SharedSender, omt::Error> {
    let mut senders = SENDERS.lock().unwrap();
    let key = (name.to_string(), port);
    if let Some(existing) = senders.get(&key).and_then(Weak::upgrade) {
        return Ok(existing);
    }
    let mut config = omt::SenderConfig::new(name, 0, 0, (30, 1));
    config.port = port;
    config.profile = quality.profile();
    config.advertise = advertise;
    let sender = Arc::new(Mutex::new(omt::Sender::new(config)?));
    senders.insert(key, Arc::downgrade(&sender));
    senders.retain(|_, w| w.strong_count() > 0);
    Ok(sender)
}

/// Maps a sender's OMT timestamps (100 ns ticks, its own clock) onto pipeline
/// running time: the first frame either element sees fixes the offset, and
/// later frames keep the sender's spacing. Shared by the video and audio
/// sources in one `omtsrc`, which is what keeps them in sync.
#[derive(Default)]
pub struct TimeBase {
    origin: Mutex<Option<(i64, gst::ClockTime)>>,
}

impl TimeBase {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Running time for `omt_ts`; `now` is the running time of arrival, used
    /// for the first frame only.
    pub fn running_time(&self, omt_ts: i64, now: gst::ClockTime) -> gst::ClockTime {
        let mut origin = self.origin.lock().unwrap();
        let (ts0, rt0) = *origin.get_or_insert((omt_ts, now));
        let delta_ns = (omt_ts - ts0).saturating_mul(100);
        if delta_ns >= 0 {
            rt0 + gst::ClockTime::from_nseconds(delta_ns as u64)
        } else {
            rt0.saturating_sub(gst::ClockTime::from_nseconds(delta_ns.unsigned_abs()))
        }
    }

    pub fn reset(&self) {
        *self.origin.lock().unwrap() = None;
    }
}
