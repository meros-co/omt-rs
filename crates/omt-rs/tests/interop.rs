//! Interop against the official libomt (openmediatransport/libomt), loaded at
//! run time from `OMT_LIB_DIR` (libomt + libvmx shared libraries, built by
//! `scripts/build-libomt.sh` / `scripts/build-libomt.ps1`).
//!
//!     OMT_LIB_DIR=/path/to/libs cargo test --features interop --test interop
//!
//! Without the `interop` feature this file compiles to nothing. With it and no
//! `OMT_LIB_DIR`, the test fails rather than silently passing, so CI cannot
//! lose the check by accident.
//!
//! One test function runs every scenario in sequence, then calls
//! `omt_shutdown`: libomt's embedded .NET runtime keeps threads alive that can
//! crash the process during exit unless it is shut down first.
#![cfg(feature = "interop")]

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use libloading::{Library, Symbol};
use omt::receive::{BlockingReceiver, Frame, ReceiverOptions};
use omt::send::{PixelFormat, Sender, SenderConfig};
use omt::vmx::{DecodeFormat, VmxDecoder};

const W: i32 = 640;
const H: i32 = 360;

const FRAME_VIDEO: c_int = 2;
const FRAME_AUDIO: c_int = 4;
const CODEC_BGRA: c_int = 0x4152_4742;
const CODEC_FPA1: c_int = 0x3141_5046;
const PREFERRED_BGRA: c_int = 2;
const QUALITY_DEFAULT: c_int = 0;
const COLOR_BT709: c_int = 709;

/// `OMTMediaFrame` from libomt.h (natural alignment).
#[repr(C)]
struct MediaFrame {
    frame_type: c_int,
    timestamp: i64,
    codec: c_int,
    width: c_int,
    height: c_int,
    stride: c_int,
    flags: c_int,
    frame_rate_n: c_int,
    frame_rate_d: c_int,
    aspect_ratio: f32,
    color_space: c_int,
    sample_rate: c_int,
    channels: c_int,
    samples_per_channel: c_int,
    data: *mut c_void,
    data_length: c_int,
    compressed_data: *mut c_void,
    compressed_length: c_int,
    frame_metadata: *mut c_void,
    frame_metadata_length: c_int,
}

impl Default for MediaFrame {
    fn default() -> Self {
        // The header says: zero this struct before use.
        unsafe { std::mem::zeroed() }
    }
}

struct LibOmt {
    lib: Library,
    _vmx: Library,
}

impl LibOmt {
    fn load() -> Self {
        let dir = PathBuf::from(std::env::var("OMT_LIB_DIR").expect(
            "OMT_LIB_DIR must point at a folder holding the official libomt and libvmx \
             (scripts/build-libomt.sh or .ps1)",
        ));
        let (omt, vmx) = if cfg!(windows) {
            ("libomt.dll", "libvmx.dll")
        } else if cfg!(target_os = "macos") {
            ("libomt.dylib", "libvmx.dylib")
        } else {
            ("libomt.so", "libvmx.so")
        };
        // libomt's .NET NativeAOT code P/Invokes "libvmx", and its resolver is
        // anchored to the executable's directory, not libomt's. Loading libvmx
        // by full path first lets the OS satisfy that import from the module
        // already in memory.
        let vmx = unsafe { Library::new(dir.join(vmx)) }.expect("load libvmx");
        let lib = unsafe { Library::new(dir.join(omt)) }.expect("load libomt");
        Self { lib, _vmx: vmx }
    }

    fn sym<T>(&self, name: &[u8]) -> Symbol<'_, T> {
        unsafe { self.lib.get(name) }
            .unwrap_or_else(|e| panic!("{}: {e}", String::from_utf8_lossy(name)))
    }
}

fn gradient_bgra() -> Vec<u8> {
    let mut px = vec![0u8; (W * H * 4) as usize];
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            px[i] = (x * 255 / W) as u8;
            px[i + 1] = (y * 255 / H) as u8;
            px[i + 2] = 96;
            px[i + 3] = 255;
        }
    }
    px
}

#[test]
fn interop_with_the_official_libomt() {
    let lib = LibOmt::load();
    our_sender_to_their_receiver(&lib);
    their_sender_to_our_receiver(&lib);
    let shutdown: Option<Symbol<unsafe extern "C" fn()>> =
        unsafe { lib.lib.get(b"omt_shutdown\0") }.ok();
    if let Some(shutdown) = shutdown {
        unsafe { shutdown() };
    }
}

/// omt-rs sends; libomt receives, decodes, and reports what it got. This is
/// the check for two of the interop fixes: libomt opens separate video and
/// audio connections and only gets the right frames on each if the sender
/// honours per-connection subscriptions.
fn our_sender_to_their_receiver(lib: &LibOmt) {
    let create: Symbol<unsafe extern "C" fn(*const c_char, c_int, c_int, c_int) -> *mut c_void> =
        lib.sym(b"omt_receive_create\0");
    let receive: Symbol<unsafe extern "C" fn(*mut c_void, c_int, c_int) -> *mut MediaFrame> =
        lib.sym(b"omt_receive\0");
    let destroy: Symbol<unsafe extern "C" fn(*mut c_void)> = lib.sym(b"omt_receive_destroy\0");

    let mut config = SenderConfig::new("omt-rs interop", W, H, (30, 1));
    config.advertise = false;
    let mut tx = Sender::new(config).expect("sender");
    let address = CString::new(format!("omt://127.0.0.1:{}", tx.port())).unwrap();
    let rx = unsafe {
        create(
            address.as_ptr(),
            FRAME_VIDEO | FRAME_AUDIO,
            PREFERRED_BGRA,
            0,
        )
    };
    assert!(!rx.is_null(), "omt_receive_create failed");

    let src = gradient_bgra();
    let planar: Vec<f32> = (0..1600)
        .map(|i| if i < 800 { 0.5 } else { -0.5 })
        .collect();
    let (mut got_video, mut got_audio) = (false, false);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut ts = 0i64;
    while !(got_video && got_audio) && Instant::now() < deadline {
        let mut px = src.clone();
        tx.send_video(PixelFormat::Bgra, &mut px, W * 4, ts)
            .unwrap();
        tx.send_audio(48_000, 2, &planar, ts).unwrap();
        ts += 333_333;
        for kind in [FRAME_VIDEO, FRAME_AUDIO] {
            let f = unsafe { receive(rx, kind, 30) };
            if f.is_null() {
                continue;
            }
            let f = unsafe { &*f };
            match f.frame_type {
                FRAME_VIDEO => {
                    assert_eq!((f.width, f.height), (W, H));
                    assert_eq!(f.codec, CODEC_BGRA);
                    let px = unsafe {
                        std::slice::from_raw_parts(f.data as *const u8, f.data_length as usize)
                    };
                    let mid = (((H / 2) * f.stride) + (W / 2) * 4) as usize;
                    let want = &src[((H / 2 * W + W / 2) * 4) as usize..][..3];
                    for c in 0..3 {
                        assert!(
                            px[mid + c].abs_diff(want[c]) < 16,
                            "pixel {:?} vs {:?}",
                            &px[mid..mid + 3],
                            want
                        );
                    }
                    got_video = true;
                }
                FRAME_AUDIO => {
                    assert_eq!(f.codec, CODEC_FPA1);
                    assert_eq!(
                        (f.sample_rate, f.channels, f.samples_per_channel),
                        (48_000, 2, 800)
                    );
                    let s = unsafe { std::slice::from_raw_parts(f.data as *const f32, 1600) };
                    assert_eq!((s[0], s[800]), (0.5, -0.5));
                    got_audio = true;
                }
                other => panic!("unexpected frame type {other}"),
            }
        }
    }
    unsafe { destroy(rx) };
    assert!(got_video, "libomt received no video from omt-rs");
    assert!(got_audio, "libomt received no audio from omt-rs");
}

/// libomt sends; omt-rs receives and decodes. This is the check for the third
/// interop fix: libomt ignores a subscribe that carries a trailing null, so it
/// sends nothing unless omt-rs writes metadata exactly as libomt does.
fn their_sender_to_our_receiver(lib: &LibOmt) {
    let create: Symbol<unsafe extern "C" fn(*const c_char, c_int) -> *mut c_void> =
        lib.sym(b"omt_send_create\0");
    let send: Symbol<unsafe extern "C" fn(*mut c_void, *mut MediaFrame) -> c_int> =
        lib.sym(b"omt_send\0");
    let get_address: Symbol<unsafe extern "C" fn(*mut c_void, *mut c_char, c_int) -> c_int> =
        lib.sym(b"omt_send_getaddress\0");
    let destroy: Symbol<unsafe extern "C" fn(*mut c_void)> = lib.sym(b"omt_send_destroy\0");

    let name = CString::new("omt-rs interop ref").unwrap();
    let tx = unsafe { create(name.as_ptr(), QUALITY_DEFAULT) };
    assert!(!tx.is_null(), "omt_send_create failed");
    let mut buf = vec![0 as c_char; 512];
    unsafe { get_address(tx, buf.as_mut_ptr(), buf.len() as c_int) };
    // libomt reports its source name ("HOST (name)"), not an address.
    let source = unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .to_string();
    let port = their_port(&source);
    let mut rx =
        BlockingReceiver::connect(&format!("127.0.0.1:{port}"), ReceiverOptions::default())
            .expect("connect");
    rx.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();

    let mut src = gradient_bgra();
    let mut planar: Vec<f32> = vec![0.25; 1600];
    let (mut video, mut audio) = (None, None);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut ts = 0i64;
    while (video.is_none() || audio.is_none()) && Instant::now() < deadline {
        let mut vf = MediaFrame {
            frame_type: FRAME_VIDEO,
            timestamp: ts,
            codec: CODEC_BGRA,
            width: W,
            height: H,
            stride: W * 4,
            frame_rate_n: 30,
            frame_rate_d: 1,
            aspect_ratio: W as f32 / H as f32,
            color_space: COLOR_BT709,
            data: src.as_mut_ptr().cast(),
            data_length: src.len() as c_int,
            ..Default::default()
        };
        unsafe { send(tx, &mut vf) };
        let mut af = MediaFrame {
            frame_type: FRAME_AUDIO,
            timestamp: ts,
            codec: CODEC_FPA1,
            sample_rate: 48_000,
            channels: 2,
            samples_per_channel: 800,
            data: planar.as_mut_ptr().cast(),
            data_length: (planar.len() * 4) as c_int,
            ..Default::default()
        };
        unsafe { send(tx, &mut af) };
        ts += 333_333;
        match rx.next_frame() {
            Ok(Frame::Video(v)) => video = Some(v),
            Ok(Frame::Audio(a)) => audio = Some(a),
            Ok(Frame::Metadata { .. }) | Err(omt::Error::Io(_)) => {}
            Err(e) => panic!("omt-rs receive failed: {e}"),
        }
    }
    unsafe { destroy(tx) };

    let video = video.expect("omt-rs received no video from libomt");
    assert_eq!(&video.header.codec, b"VMX1");
    assert_eq!((video.header.width, video.header.height), (W, H));
    let mut out = vec![0u8; DecodeFormat::Bgra.frame_size(W, H)];
    VmxDecoder::new()
        .decode(&video.data, W, H, DecodeFormat::Bgra, &mut out, W * 4)
        .expect("decode libomt's VMX");
    let mid = ((H / 2 * W + W / 2) * 4) as usize;
    for c in 0..3 {
        assert!(
            out[mid + c].abs_diff(src[mid + c]) < 16,
            "pixel {:?} vs {:?}",
            &out[mid..mid + 3],
            &src[mid..mid + 3]
        );
    }
    let audio = audio.expect("omt-rs received no audio from libomt");
    assert_eq!(
        (
            audio.header.sample_rate,
            audio.header.channels,
            audio.header.samples_per_channel
        ),
        (48_000, 2, 800)
    );
    assert!(audio.planar.iter().all(|&s| (s - 0.25).abs() < 1e-6));
}

/// The port libomt's sender listens on. Resolving its advertisement with
/// omt-rs discovery checks that path against libomt too, when multicast works;
/// otherwise (common on CI runners) fall back to finding the one listener in
/// OMT's port range on loopback.
fn their_port(source: &str) -> u16 {
    if std::env::var("OMT_TEST_MDNS").is_ok() {
        let addr = omt::discovery::resolve(source, 5000)
            .unwrap_or_else(|| panic!("omt-rs discovery could not resolve libomt's {source:?}"));
        eprintln!("resolved libomt's advertisement: {source} -> {addr}");
        return addr.rsplit(':').next().unwrap().parse().unwrap();
    }
    (6400..=6600)
        .find(|&p| {
            std::net::TcpStream::connect_timeout(
                &std::net::SocketAddr::from(([127, 0, 0, 1], p)),
                Duration::from_millis(50),
            )
            .is_ok()
        })
        .expect("no libomt listener in 6400-6600")
}
