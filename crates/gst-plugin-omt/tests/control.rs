//! Tally, sender info, statistics and drift correction through the elements.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gst::prelude::*;

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        gst::init().unwrap();
        gstomt::plugin_register_static().expect("register plugin");
    });
}

fn pipeline(desc: &str) -> gst::Pipeline {
    gst::parse::launch(desc)
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap()
}

/// Waits up to 10 s for an element message named `name` on `p`'s bus that
/// satisfies `want`.
fn wait_message(
    p: &gst::Pipeline,
    name: &str,
    mut want: impl FnMut(&gst::StructureRef) -> bool,
) -> gst::Structure {
    let bus = p.bus().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Error(e) => panic!("{} ({:?})", e.error(), e.debug()),
            gst::MessageView::Element(e) => {
                if let Some(s) = e.structure() {
                    if s.name() == name && want(s) {
                        return s.to_owned();
                    }
                }
            }
            _ => {}
        }
    }
    panic!("no {name} message matching within 10 s");
}

fn wait_for(mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn tally_and_sender_info_flow_between_omtsrc_and_omtsink() {
    init();
    let port = 9794;
    let sender = pipeline(&format!(
        "videotestsrc is-live=true ! video/x-raw,format=UYVY,width=320,height=180,framerate=30/1 ! s.video \
         omtsink name=s omt-name=gst-tally port={port} advertise=false \
         product-name=Helm manufacturer=Meros version=1.0 stats-interval=200"
    ));
    sender.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let receiver = pipeline(&format!(
        "omtsrc name=r source=127.0.0.1:{port} stats-interval=200 r.video ! fakesink sync=false"
    ));
    receiver.set_state(gst::State::Playing).unwrap();
    let r = receiver.by_name("r").unwrap();
    let s = sender.by_name("s").unwrap();

    // Sender info: message on the receiver's bus, then the properties.
    let info = wait_message(&receiver, "omt-sender-info", |_| true);
    assert_eq!(info.get::<&str>("product-name").unwrap(), "Helm");
    assert_eq!(info.get::<&str>("manufacturer").unwrap(), "Meros");
    assert_eq!(r.property::<String>("sender-product-name"), "Helm");
    assert_eq!(r.property::<String>("sender-version"), "1.0");

    // The application puts the source on program at the receiver...
    r.set_property("tally-program", true);
    // ...the sender sees it (message and property)...
    let t = wait_message(&sender, "omt-tally", |s| s.get::<bool>("program").unwrap());
    assert!(!t.get::<bool>("preview").unwrap());
    wait_for(
        || s.property::<bool>("tally-program"),
        "omtsink tally-program",
    );
    // ...and the combined tally comes back to the receiver.
    wait_message(&receiver, "omt-tally", |s| {
        s.get::<bool>("program").unwrap()
    });

    r.set_property("tally-program", false);
    r.set_property("tally-preview", true);
    wait_for(
        || !s.property::<bool>("tally-program") && s.property::<bool>("tally-preview"),
        "tally moved to preview",
    );

    receiver.set_state(gst::State::Null).unwrap();
    // A receiver that leaves takes its tally with it.
    wait_for(
        || !s.property::<bool>("tally-preview"),
        "tally cleared on disconnect",
    );
    sender.set_state(gst::State::Null).unwrap();
}

#[test]
fn statistics_are_posted_and_readable() {
    init();
    let port = 9795;
    let sender = pipeline(&format!(
        "videotestsrc is-live=true ! video/x-raw,format=UYVY,width=320,height=180,framerate=30/1 ! s.video \
         audiotestsrc is-live=true ! audio/x-raw,format=F32LE,channels=2,rate=48000 ! s.audio \
         omtsink name=s omt-name=gst-stats port={port} advertise=false stats-interval=250"
    ));
    sender.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let receiver = pipeline(&format!(
        "omtsrc name=r source=127.0.0.1:{port} stats-interval=250 \
         r.video ! fakesink sync=false r.audio ! fakesink sync=false"
    ));
    receiver.set_state(gst::State::Playing).unwrap();

    let rx = wait_message(&receiver, "omt-stats", |s| {
        s.get::<u64>("video-frames").unwrap() > 15 && s.get::<u64>("bitrate").unwrap() > 0
    });
    assert!(rx.get::<bool>("connected").unwrap());
    assert!(rx.get::<u64>("audio-frames").unwrap() > 0);
    assert!(rx.get::<u64>("bytes-received").unwrap() > 0);
    for field in ["drift-ppm", "phase-error-ms", "jitter-ms"] {
        assert!(rx.get::<f64>(field).is_ok(), "{field} missing");
    }

    let tx = wait_message(&sender, "omt-stats", |s| {
        s.get::<u64>("video-frames").unwrap() > 15
    });
    assert!(tx.get::<bool>("connected").unwrap());
    assert!(
        tx.get::<u32>("connections").unwrap() >= 2,
        "video and audio connections"
    );
    assert!(tx.get::<u64>("bitrate").unwrap() > 0);
    assert!(tx.get::<u64>("audio-frames").unwrap() > 0);

    // The same numbers as properties.
    let prop = receiver
        .by_name("r")
        .unwrap()
        .property::<gst::Structure>("stats");
    assert!(prop.get::<u64>("video-frames").unwrap() > 0);
    let prop = sender
        .by_name("s")
        .unwrap()
        .property::<gst::Structure>("stats");
    assert!(prop.get::<u64>("bytes-sent").unwrap() > 0);

    receiver.set_state(gst::State::Null).unwrap();
    sender.set_state(gst::State::Null).unwrap();
}

/// A sender whose clock runs 800 ppm fast (exaggerated, so it shows within
/// seconds): omtsrc measures the drift, keeps audio continuous (no gaps,
/// no overlaps) and keeps audio and video together.
#[test]
fn drift_is_measured_and_audio_stays_continuous() {
    init();
    let ppm = 800.0;
    let mut config = omt::SenderConfig::new("gst-drift", 320, 180, (30, 1));
    config.port = 9796;
    config.advertise = false;
    let sender = Arc::new(Mutex::new(omt::Sender::new(config).unwrap()));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pump = {
        let (sender, stop) = (sender.clone(), stop.clone());
        std::thread::spawn(move || {
            let start = Instant::now();
            let mut n: i64 = 0;
            let tone: Vec<f32> = (0..1600 * 2)
                .map(|i| ((i % 1600) as f32 / 1600.0 * std::f32::consts::TAU).sin() * 0.5)
                .collect();
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                // 30 fps of real time, stamped by a clock running fast.
                let real = n as f64 / 30.0;
                let ts = (real * (1.0 + ppm * 1e-6) * 1e7) as i64;
                let mut px = vec![128u8; 320 * 180 * 2];
                let mut s = sender.lock().unwrap();
                s.send_video(omt::PixelFormat::Uyvy, &mut px, 640, ts)
                    .unwrap();
                s.send_audio(48_000, 2, &tone, ts).unwrap();
                drop(s);
                n += 1;
                let next = Duration::from_secs_f64(n as f64 / 30.0);
                if let Some(wait) = next.checked_sub(start.elapsed()) {
                    std::thread::sleep(wait);
                }
            }
        })
    };

    let receiver = pipeline(
        "omtsrc name=r source=127.0.0.1:9796 stats-interval=500 \
         r.video ! appsink name=v sync=false r.audio ! appsink name=a sync=false",
    );
    let audio: Arc<Mutex<Vec<(gst::ClockTime, gst::ClockTime, bool)>>> = Arc::default();
    let video: Arc<Mutex<Vec<gst::ClockTime>>> = Arc::default();
    {
        let a = receiver
            .by_name("a")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .unwrap();
        let store = audio.clone();
        a.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let b = sample.buffer().unwrap();
                    store.lock().unwrap().push((
                        b.pts().unwrap(),
                        b.duration().unwrap(),
                        b.flags().contains(gst::BufferFlags::DISCONT),
                    ));
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        let v = receiver
            .by_name("v")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .unwrap();
        let store = video.clone();
        v.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    store
                        .lock()
                        .unwrap()
                        .push(sample.buffer().unwrap().pts().unwrap());
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
    }
    receiver.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_secs(20));
    let stats = receiver
        .by_name("r")
        .unwrap()
        .property::<gst::Structure>("stats");
    receiver.set_state(gst::State::Null).unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    pump.join().unwrap();

    // Sender fast => our clock is slow relative to it => negative drift. In
    // 20 s the loop is correcting in the right direction; the settled
    // estimate (drift-ppm) takes minutes, as the library's 8-hour
    // simulations show.
    let correction = stats.get::<f64>("correction-ppm").unwrap();
    let drift = stats.get::<f64>("drift-ppm").unwrap();
    assert!(
        correction < -200.0 && drift < 0.0,
        "correcting {correction:.1} ppm (drift {drift:.1}), expected towards -{ppm}"
    );

    let audio = audio.lock().unwrap();
    assert!(audio.len() > 300, "only {} audio buffers", audio.len());
    for w in audio.windows(2).skip(1) {
        let (pts, dur, _) = w[0];
        let (next, _, discont) = w[1];
        assert!(!discont, "audio discontinuity");
        let gap = next.nseconds() as i64 - (pts + dur).nseconds() as i64;
        assert!(gap.abs() <= 1_000, "audio not continuous: {gap} ns gap");
    }
    // Audio and video were stamped together; they must still line up.
    let video = video.lock().unwrap();
    let (v_last, a_last) = (*video.last().unwrap(), audio.last().unwrap().0);
    let skew = (v_last.nseconds() as i64 - a_last.nseconds() as i64).abs();
    assert!(
        skew < 1_000_000_000 / 30 * 2,
        "A/V skew {} ms",
        skew / 1_000_000
    );
}
