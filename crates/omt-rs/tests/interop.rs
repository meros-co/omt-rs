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
use omt::{SenderInfo, Tally};

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

/// `OMTTally` from libomt.h.
#[repr(C)]
#[derive(Default, Clone, Copy, Debug, PartialEq)]
struct RefTally {
    preview: c_int,
    program: c_int,
}

const MAX_STRING: usize = 1024;

/// `OMTSenderInfo` from libomt.h.
#[repr(C)]
struct RefSenderInfo {
    product_name: [c_char; MAX_STRING],
    manufacturer: [c_char; MAX_STRING],
    version: [c_char; MAX_STRING],
    reserved: [[c_char; MAX_STRING]; 3],
}

impl RefSenderInfo {
    fn zeroed() -> Box<Self> {
        Box::new(unsafe { std::mem::zeroed() })
    }

    fn from(info: &SenderInfo) -> Box<Self> {
        let mut r = Self::zeroed();
        let put = |dst: &mut [c_char; MAX_STRING], s: &str| {
            for (d, b) in dst.iter_mut().zip(s.bytes()) {
                *d = b as c_char;
            }
        };
        put(&mut r.product_name, &info.product_name);
        put(&mut r.manufacturer, &info.manufacturer);
        put(&mut r.version, &info.version);
        r
    }

    fn get(field: &[c_char; MAX_STRING]) -> String {
        unsafe { CStr::from_ptr(field.as_ptr()) }
            .to_string_lossy()
            .to_string()
    }
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
    // Never unloaded: the .NET runtime inside libomt registers a thread-exit
    // destructor on every thread that calls it, and on Linux dlclose really
    // unmaps the code, so dropping the library before this thread exits
    // crashes it (SIGSEGV) after every check has passed.
    let lib: &'static LibOmt = Box::leak(Box::new(LibOmt::load()));
    our_sender_to_their_receiver(lib);
    their_sender_to_our_receiver(lib);
    tally_and_info_with_their_receiver(lib);
    tally_and_info_with_their_sender(lib);
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
    let connections: Symbol<unsafe extern "C" fn(*mut c_void) -> c_int> =
        lib.sym(b"omt_send_connections ");
    let port = their_port(&source, || unsafe { connections(tx) });
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
/// otherwise (common on CI runners) find it in OMT's port range on loopback,
/// confirming each candidate by watching this sender's connection count rise,
/// so another OMT sender on the machine cannot be mistaken for it.
fn their_port(source: &str, connections: impl Fn() -> c_int) -> u16 {
    if std::env::var("OMT_TEST_MDNS").is_ok() {
        let addr = omt::discovery::resolve(source, 5000)
            .unwrap_or_else(|| panic!("omt-rs discovery could not resolve libomt's {source:?}"));
        eprintln!("resolved libomt's advertisement: {source} -> {addr}");
        return addr.rsplit(':').next().unwrap().parse().unwrap();
    }
    for port in 6400..=6600 {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let Ok(probe) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(50))
        else {
            continue;
        };
        let before = Instant::now();
        while before.elapsed() < Duration::from_millis(500) {
            if connections() > 0 {
                drop(probe);
                return port;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    panic!("libomt's listener not found in 6400-6600");
}

fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// omt-rs sends with sender info set; libomt receives. libomt's receiver
/// reads the info, sets program tally, and hears the combined tally back.
fn tally_and_info_with_their_receiver(lib: &LibOmt) {
    let create: Symbol<unsafe extern "C" fn(*const c_char, c_int, c_int, c_int) -> *mut c_void> =
        lib.sym(b"omt_receive_create\0");
    let receive: Symbol<unsafe extern "C" fn(*mut c_void, c_int, c_int) -> *mut MediaFrame> =
        lib.sym(b"omt_receive\0");
    let set_tally: Symbol<unsafe extern "C" fn(*mut c_void, *mut RefTally)> =
        lib.sym(b"omt_receive_settally\0");
    let get_tally: Symbol<unsafe extern "C" fn(*mut c_void, c_int, *mut RefTally) -> c_int> =
        lib.sym(b"omt_receive_gettally\0");
    let get_info: Symbol<unsafe extern "C" fn(*mut c_void, *mut RefSenderInfo)> =
        lib.sym(b"omt_receive_getsenderinformation\0");
    let destroy: Symbol<unsafe extern "C" fn(*mut c_void)> = lib.sym(b"omt_receive_destroy\0");

    let mut config = SenderConfig::new("omt-rs tally", W, H, (30, 1));
    config.advertise = false;
    let mut tx = Sender::new(config).expect("sender");
    let info = SenderInfo {
        product_name: "Helm".into(),
        manufacturer: "Meros".into(),
        version: "1.2.3".into(),
    };
    tx.set_sender_info(Some(&info));
    let address = CString::new(format!("omt://127.0.0.1:{}", tx.port())).unwrap();
    let rx = unsafe {
        create(
            address.as_ptr(),
            FRAME_VIDEO | FRAME_AUDIO,
            PREFERRED_BGRA,
            0,
        )
    };
    assert!(!rx.is_null());

    // Keep frames flowing so libomt's connections come up and stay busy.
    let src = gradient_bgra();
    let mut ts = 0i64;
    let mut pump = |tx: &mut Sender| {
        let mut px = src.clone();
        tx.send_video(PixelFormat::Bgra, &mut px, W * 4, ts)
            .unwrap();
        ts += 333_333;
        unsafe { receive(rx, FRAME_VIDEO, 30) };
    };

    let mut got_info = RefSenderInfo::zeroed();
    wait_until(
        || {
            pump(&mut tx);
            unsafe { get_info(rx, &mut *got_info) };
            RefSenderInfo::get(&got_info.product_name) == "Helm"
        },
        "libomt receiver reading omt-rs sender info",
    );
    assert_eq!(RefSenderInfo::get(&got_info.manufacturer), "Meros");
    assert_eq!(RefSenderInfo::get(&got_info.version), "1.2.3");

    let mut program = RefTally {
        preview: 0,
        program: 1,
    };
    unsafe { set_tally(rx, &mut program) };
    wait_until(
        || {
            pump(&mut tx);
            tx.tally()
                == Tally {
                    program: true,
                    preview: false,
                }
        },
        "omt-rs sender seeing libomt receiver's program tally",
    );
    let mut back = RefTally::default();
    wait_until(
        || {
            pump(&mut tx);
            unsafe { get_tally(rx, 10, &mut back) };
            back == program
        },
        "libomt receiver hearing the combined tally back",
    );

    let mut none = RefTally::default();
    unsafe { set_tally(rx, &mut none) };
    wait_until(
        || {
            pump(&mut tx);
            tx.tally() == Tally::NONE
        },
        "tally cleared",
    );
    unsafe { destroy(rx) };
}

/// libomt sends with sender info set; omt-rs receives the info and sends
/// tally, which libomt's sender reports.
fn tally_and_info_with_their_sender(lib: &LibOmt) {
    let create: Symbol<unsafe extern "C" fn(*const c_char, c_int) -> *mut c_void> =
        lib.sym(b"omt_send_create\0");
    let set_info: Symbol<unsafe extern "C" fn(*mut c_void, *mut RefSenderInfo)> =
        lib.sym(b"omt_send_setsenderinformation\0");
    let get_tally: Symbol<unsafe extern "C" fn(*mut c_void, c_int, *mut RefTally) -> c_int> =
        lib.sym(b"omt_send_gettally\0");
    let connections: Symbol<unsafe extern "C" fn(*mut c_void) -> c_int> =
        lib.sym(b"omt_send_connections\0");
    let destroy: Symbol<unsafe extern "C" fn(*mut c_void)> = lib.sym(b"omt_send_destroy\0");

    let name = CString::new("omt-rs tally ref").unwrap();
    let tx = unsafe { create(name.as_ptr(), QUALITY_DEFAULT) };
    assert!(!tx.is_null());
    let info = SenderInfo {
        product_name: "vMix".into(),
        manufacturer: "StudioCoast".into(),
        version: "29".into(),
    };
    let mut ref_info = RefSenderInfo::from(&info);
    unsafe { set_info(tx, &mut *ref_info) };
    let port = their_port("omt-rs tally ref", || unsafe { connections(tx) });

    let mut rx = BlockingReceiver::connect(
        &format!("127.0.0.1:{port}"),
        ReceiverOptions {
            video: true,
            audio: false,
            metadata: true,
            quality: None,
        },
    )
    .expect("connect");
    rx.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while rx.sender_info() != Some(&info) {
        assert!(
            Instant::now() < deadline,
            "omt-rs never read libomt's sender info (got {:?})",
            rx.sender_info()
        );
        let _ = rx.next_frame();
    }

    rx.control()
        .unwrap()
        .send_tally(Tally {
            program: false,
            preview: true,
        })
        .unwrap();
    let mut t = RefTally::default();
    wait_until(
        || {
            unsafe { get_tally(tx, 20, &mut t) };
            t == RefTally {
                preview: 1,
                program: 0,
            }
        },
        "libomt sender seeing omt-rs receiver's preview tally",
    );
    // And libomt broadcasts the combined tally, which omt-rs records.
    let deadline = Instant::now() + Duration::from_secs(15);
    while rx.sender_tally()
        != (Tally {
            program: false,
            preview: true,
        })
    {
        assert!(
            Instant::now() < deadline,
            "omt-rs never heard libomt's tally broadcast"
        );
        let _ = rx.next_frame();
    }
    drop(rx);
    unsafe { destroy(tx) };
}
