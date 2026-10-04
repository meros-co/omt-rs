//! Tally, sender information and statistics over loopback.

use std::time::{Duration, Instant};

use omt::receive::{BlockingReceiver, Frame, ReceiverOptions};
use omt::send::{PixelFormat, Sender, SenderConfig};
use omt::{SenderInfo, Tally};

const W: i32 = 160;
const H: i32 = 90;

fn sender(name: &str) -> Sender {
    let mut config = SenderConfig::new(name, W, H, (30, 1));
    config.advertise = false;
    Sender::new(config).expect("sender")
}

fn connect(sender: &Sender, options: ReceiverOptions) -> BlockingReceiver {
    let rx = BlockingReceiver::connect(&format!("127.0.0.1:{}", sender.port()), options)
        .expect("connect");
    rx.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    rx
}

fn wait_for(mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Reads frames (discarding them) until `cond` holds for the receiver.
fn read_until(
    rx: &mut BlockingReceiver,
    mut cond: impl FnMut(&BlockingReceiver) -> bool,
    what: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !cond(rx) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        let _ = rx.next_frame();
    }
}

#[test]
fn receivers_tally_is_combined_and_reported_back() {
    let tx = sender("tally");
    let changes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = changes.clone();
    tx.on_tally_changed(move |t| seen.lock().unwrap().push(t));

    let opts = ReceiverOptions {
        metadata: true,
        ..Default::default()
    };
    let mut a = connect(&tx, opts);
    let mut b = connect(&tx, opts);
    wait_for(|| tx.connections() == 2, "two connections");

    a.control()
        .unwrap()
        .send_tally(Tally {
            program: true,
            preview: false,
        })
        .unwrap();
    wait_for(|| tx.tally().program, "program tally at the sender");
    b.control()
        .unwrap()
        .send_tally(Tally {
            program: false,
            preview: true,
        })
        .unwrap();
    wait_for(
        || {
            tx.tally()
                == Tally {
                    program: true,
                    preview: true,
                }
        },
        "combined tally",
    );

    // Both receivers hear the combined tally back.
    for rx in [&mut a, &mut b] {
        read_until(
            rx,
            |r| {
                r.sender_tally()
                    == Tally {
                        program: true,
                        preview: true,
                    }
            },
            "tally broadcast",
        );
    }

    // A receiver leaving takes its tally with it.
    drop(a);
    wait_for(
        || {
            tx.tally()
                == Tally {
                    program: false,
                    preview: true,
                }
        },
        "tally after disconnect",
    );
    // The callback runs just after the change becomes visible; wait for it.
    let preview_only = Tally {
        program: false,
        preview: true,
    };
    wait_for(
        || changes.lock().unwrap().last() == Some(&preview_only),
        "tally callback after disconnect",
    );
}

#[test]
fn sender_info_reaches_receivers_on_connect_and_on_change() {
    let tx = sender("info");
    let first = SenderInfo {
        product_name: "Helm".into(),
        manufacturer: "Meros".into(),
        version: "1.0".into(),
    };
    tx.set_sender_info(Some(&first));

    // On connect, before any subscription.
    let mut rx = connect(
        &tx,
        ReceiverOptions {
            video: false,
            audio: false,
            metadata: true,
            quality: None,
        },
    );
    read_until(
        &mut rx,
        |r| r.sender_info() == Some(&first),
        "sender info on connect",
    );

    // Changed while connected.
    let second = SenderInfo {
        version: "2.0".into(),
        ..first.clone()
    };
    tx.set_sender_info(Some(&second));
    read_until(
        &mut rx,
        |r| r.sender_info() == Some(&second),
        "updated sender info",
    );
}

#[test]
fn statistics_count_frames_drops_and_bytes() {
    let mut tx = sender("stats");
    let mut rx = connect(
        &tx,
        ReceiverOptions {
            audio: false,
            ..Default::default()
        },
    );
    let mut got = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut ts = 0;
    while got < 10 && Instant::now() < deadline {
        let mut px = vec![60u8; (W * H * 4) as usize];
        tx.send_video(PixelFormat::Bgra, &mut px, W * 4, ts)
            .unwrap();
        tx.send_audio(48_000, 2, &[0.0; 96], ts).unwrap();
        ts += 333_333;
        if let Ok(Frame::Video(_)) = rx.next_frame() {
            got += 1;
        }
    }
    assert_eq!(got, 10);
    let s = tx.statistics();
    assert_eq!(s.connections, 1);
    assert!(s.video_frames >= 10 && s.audio_frames >= 10, "{s:?}");
    assert!(s.bytes_sent > 0);
    let r = rx.statistics();
    assert_eq!(r.video_frames, 10);
    assert_eq!(r.audio_frames, 0, "audio was not subscribed");
    assert!(r.bytes_received > 0);
}
