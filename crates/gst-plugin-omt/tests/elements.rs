//! The elements against each other over loopback: omtsink publishes test
//! video and audio, omtsrc receives both through appsinks.

use std::time::Duration;

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

fn pull(sink: &gst_app::AppSink, what: &str) -> gst::Sample {
    sink.try_pull_sample(gst::ClockTime::from_seconds(10))
        .unwrap_or_else(|| panic!("no {what} sample within 10 s"))
}

fn check_errors(p: &gst::Pipeline) {
    let bus = p.bus().unwrap();
    while let Some(msg) = bus.pop() {
        if let gst::MessageView::Error(e) = msg.view() {
            panic!(
                "{}: {} ({:?})",
                e.src().map(|s| s.path_string()).unwrap_or_default(),
                e.error(),
                e.debug()
            );
        }
    }
}

#[test]
fn omtsink_to_omtsrc_carries_video_and_audio() {
    init();
    let port = 9790;
    let sender = pipeline(&format!(
        "videotestsrc is-live=true pattern=smpte ! video/x-raw,format=UYVY,width=640,height=360,framerate=30/1 ! s.video \
         audiotestsrc is-live=true wave=sine freq=1000 ! audio/x-raw,format=F32LE,channels=2,rate=48000 ! s.audio \
         omtsink name=s omt-name=gst-loopback port={port} advertise=false"
    ));
    sender.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_millis(500));

    let receiver = pipeline(&format!(
        "omtsrc name=r source=127.0.0.1:{port} \
         r.video ! appsink name=v sync=false \
         r.audio ! appsink name=a sync=false"
    ));
    receiver.set_state(gst::State::Playing).unwrap();
    let v = receiver
        .by_name("v")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    let a = receiver
        .by_name("a")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();

    let video = pull(&v, "video");
    let caps = video.caps().unwrap().structure(0).unwrap().to_owned();
    assert_eq!(caps.get::<i32>("width").unwrap(), 640);
    assert_eq!(caps.get::<i32>("height").unwrap(), 360);
    assert_eq!(caps.get::<&str>("format").unwrap(), "UYVY");
    assert_eq!(
        caps.get::<gst::Fraction>("framerate").unwrap(),
        gst::Fraction::new(30, 1)
    );
    let buf = video.buffer().unwrap();
    assert_eq!(buf.size(), 640 * 360 * 2);
    assert!(buf.pts().is_some());

    let audio = pull(&a, "audio");
    let caps = audio.caps().unwrap().structure(0).unwrap().to_owned();
    assert_eq!(caps.get::<i32>("rate").unwrap(), 48_000);
    assert_eq!(caps.get::<i32>("channels").unwrap(), 2);
    let map = audio.buffer().unwrap().map_readable().unwrap();
    let peak = map
        .as_slice()
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]).abs())
        .fold(0.0f32, f32::max);
    assert!(peak > 0.1, "audio arrived silent (peak {peak})");
    drop(map);

    // Video and audio share a time base, so their timestamps are close.
    let (vt, at) = (
        pull(&v, "video").buffer().unwrap().pts().unwrap(),
        pull(&a, "audio").buffer().unwrap().pts().unwrap(),
    );
    let skew = if vt > at { vt - at } else { at - vt };
    assert!(
        skew < gst::ClockTime::from_mseconds(500),
        "video {vt} vs audio {at}"
    );

    check_errors(&sender);
    check_errors(&receiver);
    receiver.set_state(gst::State::Null).unwrap();
    sender.set_state(gst::State::Null).unwrap();
}

#[test]
fn bgra_with_alpha_round_trips() {
    init();
    let port = 9791;
    let sender = pipeline(&format!(
        "videotestsrc is-live=true ! video/x-raw,format=BGRA,width=320,height=180,framerate=25/1 ! \
         omtvideosink omt-name=gst-bgra port={port} advertise=false"
    ));
    sender.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let receiver = pipeline(&format!(
        "omtvideosrc source=127.0.0.1:{port} alpha=true ! appsink name=v sync=false"
    ));
    receiver.set_state(gst::State::Playing).unwrap();
    let v = receiver
        .by_name("v")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    let sample = pull(&v, "video");
    let caps = sample.caps().unwrap().structure(0).unwrap().to_owned();
    assert_eq!(caps.get::<&str>("format").unwrap(), "BGRA");
    assert_eq!(sample.buffer().unwrap().size(), 320 * 180 * 4);
    check_errors(&receiver);
    receiver.set_state(gst::State::Null).unwrap();
    sender.set_state(gst::State::Null).unwrap();
}

#[test]
fn a_missing_source_errors_instead_of_hanging() {
    init();
    // Nothing listens on this port.
    let receiver = pipeline("omtvideosrc source=127.0.0.1:9799 ! fakesink");
    let result = receiver.set_state(gst::State::Playing);
    let bus = receiver.bus().unwrap();
    let error =
        bus.timed_pop_filtered(gst::ClockTime::from_seconds(10), &[gst::MessageType::Error]);
    assert!(
        result.is_err() || error.is_some(),
        "no error for an unreachable source"
    );
    receiver.set_state(gst::State::Null).unwrap();
}

/// Non-interleaved (planar) audio into omtaudiosink arrives intact.
#[test]
fn planar_audio_is_accepted() {
    init();
    let port = 9793;
    let sender = pipeline(&format!(
        "audiotestsrc is-live=true wave=sine freq=440 ! audio/x-raw,format=F32LE,channels=2,rate=48000,layout=non-interleaved !          omtaudiosink omt-name=gst-planar port={port} advertise=false"
    ));
    sender.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let receiver = pipeline(&format!(
        "omtaudiosrc source=127.0.0.1:{port} ! appsink name=a sync=false"
    ));
    receiver.set_state(gst::State::Playing).unwrap();
    let a = receiver
        .by_name("a")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    let sample = pull(&a, "audio");
    let map = sample.buffer().unwrap().map_readable().unwrap();
    let samples: Vec<f32> = map
        .as_slice()
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    // Interleaved out: left and right carry the same sine.
    assert!(
        samples
            .chunks_exact(2)
            .all(|lr| (lr[0] - lr[1]).abs() < 1e-6)
    );
    assert!(samples.iter().any(|s| s.abs() > 0.1));
    drop(map);
    check_errors(&sender);
    receiver.set_state(gst::State::Null).unwrap();
    sender.set_state(gst::State::Null).unwrap();
}

/// The device provider lists an advertised omtsink. Opt-in: needs multicast.
///
///     OMT_TEST_MDNS=1 cargo test -p gst-plugin-omt device_provider
#[test]
fn device_provider_lists_an_advertised_sink() {
    if std::env::var("OMT_TEST_MDNS").is_err() {
        eprintln!("OMT_TEST_MDNS not set - skipping");
        return;
    }
    init();
    omt::discovery::set_machine_name("GSTTEST");
    let sender = pipeline(
        "videotestsrc is-live=true ! video/x-raw,format=UYVY,width=320,height=180 ! omtvideosink omt-name=DeviceCheck port=9792",
    );
    sender.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_secs(1));
    let provider = gst::DeviceProviderFactory::by_name("omtdeviceprovider").expect("provider");
    let found = (0..3).any(|_| {
        provider
            .devices()
            .iter()
            .any(|d| d.display_name() == "GSTTEST (DeviceCheck)")
    });
    sender.set_state(gst::State::Null).unwrap();
    assert!(found, "omtdeviceprovider did not list the sink");
}
