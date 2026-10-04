//! OMT sender: a TCP server that receivers connect to, plus its mDNS
//! advertisement.
//!
//! Each connected receiver gets a reader thread (which records the frame types
//! it subscribes to) and a writer thread fed by a small bounded queue. Sending
//! never blocks the caller: a receiver whose queue is full simply misses that
//! frame, and one whose socket has failed is dropped. A slow receiver can
//! therefore never stall the application's own loop - the property a live
//! production tool needs most.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::Error;
use crate::discovery::Advertiser;
use crate::protocol::{AudioHeader, FrameHeader, FrameType, VideoHeader, commands, video_flags};
use crate::vmx::{VmxColorSpace, VmxInstance, VmxProfile};

/// The port a sender listens on when its config asks for 0. A fixed default
/// lets a receiver reconnect to the same address across sender restarts; if it
/// is taken the sender falls back to an OS-assigned port and advertises that.
pub const DEFAULT_PORT: u16 = 6960;

/// Frames queued per receiver before newer ones are dropped for it.
const CLIENT_QUEUE: usize = 4;

/// How long a write to a stalled receiver may block its writer thread before
/// the receiver is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Uncompressed pixel layouts the sender can encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 8-bit B, G, R, A per pixel. Alpha is not transmitted.
    Bgra,
    /// 4:2:2 U Y V Y.
    Uyvy,
    /// UYVY followed by a full-resolution 8-bit alpha plane; sent with the
    /// alpha flag so receivers can key on it.
    Uyva,
}

#[derive(Debug, Clone)]
pub struct SenderConfig {
    /// Source name; published as `MACHINE (name)`.
    pub name: String,
    /// TCP port, or 0 for [`DEFAULT_PORT`].
    pub port: u16,
    pub width: i32,
    pub height: i32,
    /// Frame rate as numerator / denominator.
    pub frame_rate: (i32, i32),
    /// VMX bitrate/fidelity tier. Never changes the resolution.
    pub profile: VmxProfile,
    pub color_space: VmxColorSpace,
    /// Publish an mDNS record so receivers can find the sender by name.
    pub advertise: bool,
}

impl SenderConfig {
    pub fn new(name: &str, width: i32, height: i32, frame_rate: (i32, i32)) -> Self {
        Self {
            name: name.to_string(),
            port: 0,
            width,
            height,
            frame_rate,
            profile: VmxProfile::OmtSq,
            color_space: VmxColorSpace::Bt709,
            advertise: true,
        }
    }
}

/// What happened to one sent frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SendOutcome {
    /// Receivers subscribed to this frame type that were handed the frame.
    pub delivered: usize,
    /// Receivers subscribed to this frame type that were too far behind to
    /// take it. Sustained non-zero values mean a receiver cannot keep up.
    pub dropped: usize,
}

/// A connected receiver plus the frame types it subscribed to. Per the OMT
/// spec a sender must not send a type until the receiver subscribes, and the
/// reference libomt opens *separate* video-only and audio-only connections -
/// so sending every type to every connection corrupts the video channel's
/// ordering and starves the audio channel.
struct Client {
    tx: SyncSender<Arc<Vec<u8>>>,
    video: Arc<AtomicBool>,
    audio: Arc<AtomicBool>,
    /// Kept only to shut the socket down on eviction or teardown, which
    /// unblocks the client's reader and writer threads.
    stream: TcpStream,
}

pub struct Sender {
    /// Created on the first video frame (and again after a format change), so
    /// a sender can exist before its video format is known.
    encoder: Option<VmxInstance>,
    encoded: Vec<u8>,
    clients: Arc<Mutex<Vec<Client>>>,
    config: SenderConfig,
    port: u16,
    _advertiser: Option<Advertiser>,
    stop: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
}

impl Sender {
    pub fn new(config: SenderConfig) -> Result<Self, Error> {
        // Fail fast on a configured size the codec rejects; a 0x0 config is
        // allowed and means "set later with set_video_format".
        let encoder = if config.width > 0 && config.height > 0 {
            Some(Self::encoder_for(&config)?)
        } else {
            None
        };
        // Advertise the port actually bound: advertising a requested 0 would
        // publish port 0, and receivers would hang at "connecting".
        let want = if config.port == 0 {
            DEFAULT_PORT
        } else {
            config.port
        };
        let bind = crate::net::bind_ip();
        let listener = TcpListener::bind((bind, want)).or_else(|_| TcpListener::bind((bind, 0)))?;
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(want);
        listener.set_nonblocking(true)?;
        let advertiser = if config.advertise {
            Advertiser::new(&config.name, port)
        } else {
            None
        };

        let clients: Arc<Mutex<Vec<Client>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let accept_thread = {
            let clients = clients.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("omt-accept".into())
                .spawn(move || accept_loop(listener, clients, stop))?
        };
        Ok(Self {
            encoder,
            encoded: Vec::new(),
            clients,
            config,
            port,
            _advertiser: advertiser,
            stop,
            accept_thread: Some(accept_thread),
        })
    }

    fn encoder_for(config: &SenderConfig) -> Result<VmxInstance, Error> {
        let (w, h) = (config.width, config.height);
        if w <= 0 || h <= 0 {
            return Err(Error::Codec(format!("invalid video size {w}x{h}")));
        }
        VmxInstance::new(w, h, config.profile, config.color_space)
            .ok_or_else(|| Error::Codec("VMX_Create failed".into()))
    }

    /// Changes the video size and rate, for senders whose format is learned
    /// after creation (or changes mid-stream). Takes effect on the next frame.
    pub fn set_video_format(&mut self, width: i32, height: i32, frame_rate: (i32, i32)) {
        let c = &mut self.config;
        if (c.width, c.height) != (width, height) {
            self.encoder = None;
        }
        c.width = width;
        c.height = height;
        c.frame_rate = frame_rate;
    }

    /// The TCP port receivers connect to.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Currently connected receiver connections (the reference libomt opens
    /// two per receiver: one for video, one for audio).
    pub fn connections(&self) -> usize {
        self.clients.lock().unwrap().len()
    }

    /// Encodes and sends one video frame. `pixels` must be exactly the
    /// configured size in `format`; `stride` is bytes per row of the first
    /// plane. `timestamp` is in OMT ticks (100 ns), on the same clock as the
    /// audio sent alongside it.
    pub fn send_video(
        &mut self,
        format: PixelFormat,
        pixels: &mut [u8],
        stride: i32,
        timestamp: i64,
    ) -> Result<SendOutcome, Error> {
        let (w, h) = (self.config.width, self.config.height);
        if self.encoder.is_none() {
            self.encoder = Some(Self::encoder_for(&self.config)?);
        }
        let encoder = self.encoder.as_mut().unwrap();
        let first_plane = (stride.max(0) * h.max(0)) as usize;
        if pixels.len() < first_plane {
            return Err(Error::Codec(format!(
                "{} bytes is too small for a {w}x{h} frame",
                pixels.len()
            )));
        }
        let result = match format {
            PixelFormat::Bgra => encoder.encode_bgra(pixels, stride),
            PixelFormat::Uyvy => encoder.encode_uyvy(pixels, stride),
            PixelFormat::Uyva => encoder.encode_uyva(pixels, stride),
        };
        result.map_err(|e| Error::Codec(format!("VMX encode failed ({e})")))?;
        // VMX output is bounded by the uncompressed size.
        self.encoded.resize((w * h * 4) as usize, 0);
        let len = encoder.save_to(&mut self.encoded);
        if len <= 0 {
            return Err(Error::Codec("VMX_SaveTo returned no data".into()));
        }
        let flags = if format == PixelFormat::Uyva {
            video_flags::ALPHA
        } else {
            0
        };
        let blob = self.video_blob(len as usize, flags, timestamp);
        Ok(self.broadcast(blob, FrameKind::Video))
    }

    /// Sends an already VMX-encoded frame of the configured size unchanged.
    pub fn send_vmx(
        &mut self,
        vmx: &[u8],
        flags: i32,
        timestamp: i64,
    ) -> Result<SendOutcome, Error> {
        if self.encoded.len() < vmx.len() {
            self.encoded.resize(vmx.len(), 0);
        }
        self.encoded[..vmx.len()].copy_from_slice(vmx);
        let blob = self.video_blob(vmx.len(), flags, timestamp);
        Ok(self.broadcast(blob, FrameKind::Video))
    }

    /// Sends audio as OMT's FPA1: 32-bit float, *planar* (all of channel 0,
    /// then all of channel 1, ...). `planar.len()` must be a multiple of
    /// `channels`.
    pub fn send_audio(
        &mut self,
        sample_rate: i32,
        channels: i32,
        planar: &[f32],
        timestamp: i64,
    ) -> Result<SendOutcome, Error> {
        let channels = channels.clamp(1, 32);
        let samples_per_channel = planar.len() / channels as usize;
        if samples_per_channel == 0 {
            return Ok(SendOutcome::default());
        }
        let used = samples_per_channel * channels as usize;
        let audio_header = AudioHeader {
            codec: *b"FPA1",
            sample_rate,
            samples_per_channel: samples_per_channel as i32,
            channels,
            active_channels: if channels >= 32 {
                u32::MAX
            } else {
                (1u32 << channels) - 1
            },
            reserved1: 0,
        };
        let header = FrameHeader {
            version: 1,
            frame_type: FrameType::Audio,
            timestamp,
            metadata_length: 0,
            data_length: (AudioHeader::SIZE + used * 4) as i32,
        };
        let mut blob = Vec::with_capacity(FrameHeader::SIZE + AudioHeader::SIZE + used * 4);
        blob.extend_from_slice(&header.to_bytes());
        blob.extend_from_slice(&audio_header.to_bytes());
        for s in &planar[..used] {
            blob.extend_from_slice(&s.to_le_bytes());
        }
        Ok(self.broadcast(Arc::new(blob), FrameKind::Audio))
    }

    fn video_blob(&self, len: usize, flags: i32, timestamp: i64) -> Arc<Vec<u8>> {
        let c = &self.config;
        let video_header = VideoHeader {
            codec: *b"VMX1",
            width: c.width,
            height: c.height,
            frame_rate_n: c.frame_rate.0,
            frame_rate_d: c.frame_rate.1.max(1),
            aspect_ratio: c.width as f32 / c.height.max(1) as f32,
            flags,
            color_space: c.color_space as i32,
        };
        let header = FrameHeader {
            version: 1,
            frame_type: FrameType::Video,
            timestamp,
            metadata_length: 0,
            data_length: (VideoHeader::SIZE + len) as i32,
        };
        // Serialized once; each receiver's queue gets a refcounted handle.
        let mut blob = Vec::with_capacity(FrameHeader::SIZE + VideoHeader::SIZE + len);
        blob.extend_from_slice(&header.to_bytes());
        blob.extend_from_slice(&video_header.to_bytes());
        blob.extend_from_slice(&self.encoded[..len]);
        Arc::new(blob)
    }

    fn broadcast(&self, blob: Arc<Vec<u8>>, kind: FrameKind) -> SendOutcome {
        let mut outcome = SendOutcome::default();
        let mut clients = self.clients.lock().unwrap();
        clients.retain(|c| {
            let subscribed = match kind {
                FrameKind::Video => &c.video,
                FrameKind::Audio => &c.audio,
            };
            if !subscribed.load(Ordering::Relaxed) {
                return true; // connected, not (yet) subscribed to this type
            }
            match c.tx.try_send(blob.clone()) {
                Ok(()) => {
                    outcome.delivered += 1;
                    true
                }
                // Behind: drop this frame for it, keep the receiver.
                Err(TrySendError::Full(_)) => {
                    outcome.dropped += 1;
                    true
                }
                // Writer thread gone (socket error): evict.
                Err(TrySendError::Disconnected(_)) => {
                    let _ = c.stream.shutdown(Shutdown::Both);
                    false
                }
            }
        });
        outcome
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for c in self.clients.lock().unwrap().iter() {
            let _ = c.stream.shutdown(Shutdown::Both);
        }
        if let Some(h) = self.accept_thread.take() {
            let _ = h.join();
        }
    }
}

#[derive(Clone, Copy)]
enum FrameKind {
    Video,
    Audio,
}

fn accept_loop(listener: TcpListener, clients: Arc<Mutex<Vec<Client>>>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Some(client) = start_client(stream) {
                    clients.lock().unwrap().push(client);
                }
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn start_client(stream: TcpStream) -> Option<Client> {
    // Accepted sockets inherit the listener's non-blocking flag; OMT writes
    // are blocking (on the writer thread, never the caller's).
    stream.set_nonblocking(false).ok();
    stream.set_nodelay(true).ok();
    stream.set_write_timeout(Some(WRITE_TIMEOUT)).ok();
    let video = Arc::new(AtomicBool::new(false));
    let audio = Arc::new(AtomicBool::new(false));
    let rx_stream = stream.try_clone().ok()?;
    let (rv, ra) = (video.clone(), audio.clone());
    std::thread::Builder::new()
        .name("omt-client-rx".into())
        .spawn(move || read_subscribes(rx_stream, rv, ra))
        .ok()?;
    // Blocking writes are fine on this thread and keep each frame atomic on
    // the wire; the caller drops frames when the queue is full.
    let (tx, rx) = sync_channel::<Arc<Vec<u8>>>(CLIENT_QUEUE);
    let mut wr = stream.try_clone().ok()?;
    std::thread::Builder::new()
        .name("omt-client-tx".into())
        .spawn(move || {
            while let Ok(buf) = rx.recv() {
                if wr.write_all(&buf).is_err() {
                    break;
                }
            }
            let _ = wr.shutdown(Shutdown::Both);
        })
        .ok()?;
    Some(Client {
        tx,
        video,
        audio,
        stream,
    })
}

/// Reads frames a receiver sends and flips its subscription flags when the
/// exact `OMTSubscribe` strings arrive. Exits on disconnect or teardown.
fn read_subscribes(mut stream: TcpStream, video: Arc<AtomicBool>, audio: Arc<AtomicBool>) {
    let mut header = [0u8; FrameHeader::SIZE];
    loop {
        if stream.read_exact(&mut header).is_err() {
            return;
        }
        let Ok(h) = FrameHeader::from_bytes(&header) else {
            return;
        };
        let body_len = h.data_length.max(0) as usize;
        if body_len > 1 << 20 {
            return; // a subscribe is tiny; anything this big is not OMT
        }
        let mut body = vec![0u8; body_len];
        if stream.read_exact(&mut body).is_err() {
            return;
        }
        if h.frame_type == FrameType::Metadata {
            // The reference sends raw UTF-8 with no null; tolerate one anyway.
            let xml = std::str::from_utf8(&body)
                .unwrap_or("")
                .trim_end_matches('\0');
            match xml {
                commands::SUBSCRIBE_VIDEO => video.store(true, Ordering::Relaxed),
                commands::SUBSCRIBE_AUDIO => audio.store(true, Ordering::Relaxed),
                _ => {}
            }
        }
    }
}
