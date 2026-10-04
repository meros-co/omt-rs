//! Sender to receiver over loopback: the whole stack (framing, subscription
//! handling, VMX, audio) against itself.

use std::time::{Duration, Instant};

use omt::protocol::video_flags;
use omt::receive::{BlockingReceiver, Frame, ReceiverOptions};
use omt::send::{PixelFormat, Sender, SenderConfig};
use omt::vmx::{DecodeFormat, VmxDecoder};

const W: i32 = 320;
const H: i32 = 180;

fn sender(name: &str) -> Sender {
    let mut config = SenderConfig::new(name, W, H, (30, 1));
    config.advertise = false;
    Sender::new(config).expect("sender")
}

/// A smooth BGRA gradient (VMX is lossy; smooth content measures the round
/// trip rather than the quantizer).
fn gradient() -> Vec<u8> {
    let mut px = vec![0u8; (W * H * 4) as usize];
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            px[i] = (x * 255 / W) as u8;
            px[i + 1] = (y * 255 / H) as u8;
            px[i + 2] = 128;
            px[i + 3] = 255;
        }
    }
    px
}

fn connect(sender: &Sender, options: ReceiverOptions) -> BlockingReceiver {
    let rx = BlockingReceiver::connect(&format!("127.0.0.1:{}", sender.port()), options)
        .expect("connect");
    rx.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    rx
}

/// Sends `send` repeatedly (the subscribe arrives asynchronously, so the first
/// frames may go nowhere) until `want` matches a received frame.
fn pump<T>(
    rx: &mut BlockingReceiver,
    mut send: impl FnMut(i64),
    mut want: impl FnMut(Frame) -> Option<T>,
) -> Option<T> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut n = 0i64;
    while Instant::now() < deadline {
        send(n * 333_333);
        n += 1;
        match rx.next_frame() {
            Ok(frame) => {
                if let Some(t) = want(frame) {
                    return Some(t);
                }
            }
            Err(omt::Error::Io(_)) => {} // read timeout: send again
            Err(e) => panic!("receive failed: {e}"),
        }
    }
    None
}

#[test]
fn video_arrives_and_decodes_to_the_same_picture() {
    let mut tx = sender("loopback-video");
    let mut rx = connect(&tx, ReceiverOptions::default());
    let src = gradient();
    let frame = pump(
        &mut rx,
        |ts| {
            let mut px = src.clone();
            tx.send_video(PixelFormat::Bgra, &mut px, W * 4, ts)
                .unwrap();
        },
        |f| match f {
            Frame::Video(v) => Some(v),
            _ => None,
        },
    )
    .expect("no video frame arrived");

    assert_eq!(&frame.header.codec, b"VMX1");
    assert_eq!((frame.header.width, frame.header.height), (W, H));
    assert_eq!(
        (frame.header.frame_rate_n, frame.header.frame_rate_d),
        (30, 1)
    );
    assert_eq!(frame.header.color_space, 709);
    assert_eq!(
        frame.timestamp % 333_333,
        0,
        "timestamp must survive unchanged"
    );

    let mut out = vec![0u8; DecodeFormat::Bgra.frame_size(W, H)];
    VmxDecoder::new()
        .decode(&frame.data, W, H, DecodeFormat::Bgra, &mut out, W * 4)
        .expect("decode");
    let mae: f64 = src
        .chunks_exact(4)
        .zip(out.chunks_exact(4))
        .map(|(a, b)| (0..3).map(|c| a[c].abs_diff(b[c]) as f64).sum::<f64>())
        .sum::<f64>()
        / (W * H * 3) as f64;
    // The sender's default profile is OMT SQ (the HQ round trip in the vmx
    // unit test holds a tighter bound).
    assert!(mae < 8.0, "mean absolute error {mae:.2}");
}

#[test]
fn audio_arrives_planar_and_exact() {
    let mut tx = sender("loopback-audio");
    let mut rx = connect(
        &tx,
        ReceiverOptions {
            video: false,
            ..Default::default()
        },
    );
    // 2 channels x 480 samples, planar, with distinct per-channel values.
    let planar: Vec<f32> = (0..960)
        .map(|i| if i < 480 { i as f32 / 480.0 } else { -0.25 })
        .collect();
    let audio = pump(
        &mut rx,
        |ts| {
            tx.send_audio(48_000, 2, &planar, ts).unwrap();
        },
        |f| match f {
            Frame::Audio(a) => Some(a),
            Frame::Video(_) => panic!("video sent to a receiver that only subscribed to audio"),
            _ => None,
        },
    )
    .expect("no audio arrived");
    assert_eq!(&audio.header.codec, b"FPA1");
    assert_eq!(audio.header.sample_rate, 48_000);
    assert_eq!(audio.header.channels, 2);
    assert_eq!(audio.header.samples_per_channel, 480);
    assert_eq!(audio.header.active_channels, 0b11);
    assert_eq!(audio.planar, planar);
}

/// The reference libomt opens a video-only and an audio-only connection per
/// receiver; each must get only its own type.
#[test]
fn each_connection_gets_only_what_it_subscribed_to() {
    let mut tx = sender("loopback-split");
    let mut video_rx = connect(
        &tx,
        ReceiverOptions {
            audio: false,
            ..Default::default()
        },
    );
    let mut audio_rx = connect(
        &tx,
        ReceiverOptions {
            video: false,
            ..Default::default()
        },
    );
    let planar = vec![0.1f32; 960];
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut saw_video, mut saw_audio) = (false, false);
    let mut ts = 0;
    while !(saw_video && saw_audio) && Instant::now() < deadline {
        let mut px = gradient();
        tx.send_video(PixelFormat::Bgra, &mut px, W * 4, ts)
            .unwrap();
        tx.send_audio(48_000, 2, &planar, ts).unwrap();
        ts += 333_333;
        // Metadata (the tally a sender sends on connect) may arrive on
        // either; media must only go where it was subscribed.
        match video_rx.next_frame() {
            Ok(Frame::Video(_)) => saw_video = true,
            Ok(Frame::Audio(_)) => panic!("video connection got audio"),
            _ => {}
        }
        match audio_rx.next_frame() {
            Ok(Frame::Audio(_)) => saw_audio = true,
            Ok(Frame::Video(_)) => panic!("audio connection got video"),
            _ => {}
        }
    }
    assert!(
        saw_video && saw_audio,
        "video={saw_video} audio={saw_audio}"
    );
}

#[test]
fn uyva_is_flagged_as_alpha() {
    let mut tx = sender("loopback-alpha");
    let mut rx = connect(
        &tx,
        ReceiverOptions {
            audio: false,
            ..Default::default()
        },
    );
    let size = DecodeFormat::Uyva.frame_size(W, H);
    let frame = pump(
        &mut rx,
        |ts| {
            let mut px = vec![128u8; size];
            tx.send_video(PixelFormat::Uyva, &mut px, W * 2, ts)
                .unwrap();
        },
        |f| match f {
            Frame::Video(v) => Some(v),
            _ => None,
        },
    )
    .expect("no video frame arrived");
    assert_ne!(frame.header.flags & video_flags::ALPHA, 0);
}

/// A receiver that never reads must not slow the sender: frames for it are
/// dropped (and reported) instead.
#[test]
fn a_stalled_receiver_never_blocks_the_sender() {
    let mut tx = sender("loopback-stall");
    let _stalled = connect(
        &tx,
        ReceiverOptions {
            audio: false,
            ..Default::default()
        },
    );
    let started = Instant::now();
    let mut dropped = 0;
    for i in 0..300 {
        let mut px = gradient();
        dropped += tx
            .send_video(PixelFormat::Bgra, &mut px, W * 4, i)
            .unwrap()
            .dropped;
    }
    assert!(
        dropped > 0,
        "a receiver that never reads should have had frames dropped"
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "sending blocked on a stalled receiver"
    );
}

#[test]
fn dropping_the_sender_disconnects_receivers() {
    let mut tx = sender("loopback-bye");
    let mut rx = connect(
        &tx,
        ReceiverOptions {
            audio: false,
            ..Default::default()
        },
    );
    // Make sure the connection is fully up first.
    pump(
        &mut rx,
        |ts| {
            let mut px = gradient();
            tx.send_video(PixelFormat::Bgra, &mut px, W * 4, ts)
                .unwrap();
        },
        |f| matches!(f, Frame::Video(_)).then_some(()),
    )
    .expect("connection never came up");
    drop(tx);
    rx.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    loop {
        match rx.next_frame() {
            Ok(_) => continue, // frames already in flight
            Err(omt::Error::Disconnected(_)) => break,
            Err(e) => panic!("expected Disconnected, got {e}"),
        }
    }
}

#[cfg(feature = "tokio")]
#[tokio::test(flavor = "multi_thread")]
async fn the_async_receiver_reads_video_and_audio() {
    use omt::receive::Receiver;
    let tx = std::sync::Arc::new(std::sync::Mutex::new(sender("loopback-async")));
    let port = tx.lock().unwrap().port();
    let mut rx = Receiver::connect(&format!("127.0.0.1:{port}"), ReceiverOptions::default())
        .await
        .unwrap();
    let pumping = tx.clone();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop2 = stop.clone();
    let pump = std::thread::spawn(move || {
        let mut ts = 0;
        while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
            let mut px = gradient();
            let mut s = pumping.lock().unwrap();
            s.send_video(PixelFormat::Bgra, &mut px, W * 4, ts).unwrap();
            s.send_audio(48_000, 2, &[0.5; 960], ts).unwrap();
            drop(s);
            ts += 333_333;
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    let (mut video, mut audio) = (false, false);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !(video && audio) {
        let frame = tokio::time::timeout_at(deadline, rx.next_frame())
            .await
            .expect("timed out")
            .unwrap();
        match frame {
            Frame::Video(v) => {
                assert_eq!((v.header.width, v.header.height), (W, H));
                video = true;
            }
            Frame::Audio(a) => {
                assert_eq!(a.planar.len(), 960);
                audio = true;
            }
            Frame::Metadata { .. } => {}
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    pump.join().unwrap();
}

/// Advertise, discover and resolve over real mDNS. Opt-in: multicast is often
/// unavailable on CI runners.
///
///     OMT_TEST_MDNS=1 cargo test discovery_finds_an_advertised_sender
#[test]
fn discovery_finds_an_advertised_sender() {
    if std::env::var("OMT_TEST_MDNS").is_err() {
        eprintln!("OMT_TEST_MDNS not set - skipping");
        return;
    }
    omt::discovery::set_machine_name("OMTRSTEST");
    let mut config = SenderConfig::new("Discovery Check", W, H, (30, 1));
    config.port = 0;
    let tx = Sender::new(config).unwrap();
    let name = "OMTRSTEST (Discovery Check)";
    let found = (0..4).any(|_| omt::discovery::discover(3000).iter().any(|n| n == name));
    assert!(found, "{name} not discovered");
    let addr = omt::discovery::resolve(name, 5000).expect("resolve");
    assert!(addr.ends_with(&format!(":{}", tx.port())), "{addr}");
}

/// A sender may be created before its video format is known (a GStreamer sink
/// learns it from caps) and may change it mid-stream.
#[test]
fn the_video_format_can_be_set_late_and_changed() {
    let mut config = SenderConfig::new("loopback-late", 0, 0, (30, 1));
    config.advertise = false;
    let mut tx = Sender::new(config).expect("a 0x0 sender is allowed");
    let mut rx = connect(
        &tx,
        ReceiverOptions {
            audio: false,
            ..Default::default()
        },
    );
    for (w, h) in [(W, H), (160, 90)] {
        tx.set_video_format(w, h, (25, 1));
        let frame = pump(
            &mut rx,
            |ts| {
                let mut px = vec![100u8; (w * h * 4) as usize];
                tx.send_video(PixelFormat::Bgra, &mut px, w * 4, ts)
                    .unwrap();
            },
            |f| match f {
                Frame::Video(v) if v.header.width == w => Some(v),
                _ => None,
            },
        )
        .unwrap_or_else(|| panic!("no {w}x{h} frame"));
        assert_eq!((frame.header.height, frame.header.frame_rate_n), (h, 25));
    }
}
