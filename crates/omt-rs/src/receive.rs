//! OMT receiver: connects to a sender, subscribes, and yields frames.
//!
//! Two forms with the same behaviour: [`BlockingReceiver`] on `std::net` (for
//! threads, e.g. a GStreamer source element) and, with the `tokio` feature,
//! [`Receiver`] on tokio. Video arrives VMX-compressed; decode it with
//! [`crate::vmx::VmxDecoder`] or a [`crate::vmx::VmxInstance`] of your own.

use bytes::Bytes;

use crate::Error;
use crate::protocol::{AudioHeader, FrameHeader, FrameType, VideoHeader, commands};

/// Sender-side quality to request ("Suggested Quality"). The sender lowers its
/// bitrate at full resolution; it never changes the picture size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    Low,
    Medium,
    High,
}

impl Quality {
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ReceiverOptions {
    pub video: bool,
    pub audio: bool,
    /// Ask the sender for this quality; `None` keeps the sender's default.
    pub quality: Option<Quality>,
}

impl Default for ReceiverOptions {
    fn default() -> Self {
        Self {
            video: true,
            audio: true,
            quality: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub header: VideoHeader,
    /// OMT ticks (100 ns), on the sender's clock.
    pub timestamp: i64,
    /// The compressed picture (codec in `header.codec`, normally VMX1).
    pub data: Bytes,
    /// Per-frame metadata XML, if the sender attached any.
    pub metadata: Bytes,
}

#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub header: AudioHeader,
    /// OMT ticks (100 ns), on the same clock as video.
    pub timestamp: i64,
    /// 32-bit float, planar: `samples_per_channel` samples of channel 0, then
    /// channel 1, ...
    pub planar: Vec<f32>,
}

#[derive(Debug, Clone)]
pub enum Frame {
    Video(VideoFrame),
    Audio(AudioFrame),
    Metadata { timestamp: i64, xml: String },
}

/// Resolves a receiver target: a literal `host:port` (the last segment is a
/// numeric port) is used as-is; anything else is treated as a discovered
/// source name (`MACHINE (Name)`) and resolved over mDNS. Blocks up to
/// `timeout_ms` for a name.
pub fn resolve_target(target: &str, timeout_ms: u64) -> Result<String, Error> {
    let is_hostport = target
        .rsplit_once(':')
        .is_some_and(|(_, p)| p.parse::<u16>().is_ok());
    if is_hostport {
        return Ok(target.to_string());
    }
    crate::discovery::resolve(target, timeout_ms)
        .ok_or_else(|| Error::NotFound(format!("OMT source '{target}' not found")))
}

/// How long name resolution waits before giving up.
const RESOLVE_TIMEOUT_MS: u64 = 3000;

/// The metadata frames a receiver sends after connecting, in order.
fn hello(options: &ReceiverOptions) -> Vec<Vec<u8>> {
    let mut xml = Vec::new();
    if options.video {
        xml.push(commands::SUBSCRIBE_VIDEO.to_string());
    }
    if options.audio {
        xml.push(commands::SUBSCRIBE_AUDIO.to_string());
    }
    if let Some(q) = options.quality {
        xml.push(format!(r#"<OMTSettings Quality="{}" />"#, q.as_str()));
    }
    xml.iter().map(|x| metadata_frame(x)).collect()
}

/// A metadata frame as the reference libomt writes one: the XML in the DATA
/// region as raw UTF-8 with NO null terminator, and MetadataLength = 0. The
/// 1.0 spec text says "including null character", but the reference sender's
/// subscribe handling is an exact string compare with no null stripping, so a
/// null makes it ignore the subscribe and never send anything.
pub fn metadata_frame(xml: &str) -> Vec<u8> {
    let header = FrameHeader {
        version: 1,
        frame_type: FrameType::Metadata,
        timestamp: 0,
        metadata_length: 0,
        data_length: xml.len() as i32,
    };
    let mut out = header.to_bytes().to_vec();
    out.extend_from_slice(xml.as_bytes());
    out
}

/// Parses a frame body. `Ok(None)` for a frame to skip (a truncated audio
/// frame, which some senders emit as a keep-alive).
fn parse(header: &FrameHeader, body: Vec<u8>) -> Result<Option<Frame>, Error> {
    let meta_len = header.metadata_length as usize;
    match header.frame_type {
        FrameType::Video => {
            if body.len() < VideoHeader::SIZE + meta_len {
                return Err(Error::Protocol("OMT video frame too short".into()));
            }
            let video = VideoHeader::from_bytes(body[..VideoHeader::SIZE].try_into().unwrap());
            let body = Bytes::from(body);
            let data_end = body.len() - meta_len;
            Ok(Some(Frame::Video(VideoFrame {
                header: video,
                timestamp: header.timestamp,
                data: body.slice(VideoHeader::SIZE..data_end),
                metadata: body.slice(data_end..),
            })))
        }
        FrameType::Audio => {
            if body.len() < AudioHeader::SIZE + meta_len {
                return Ok(None);
            }
            let audio = AudioHeader::from_bytes(body[..AudioHeader::SIZE].try_into().unwrap());
            let planar = body[AudioHeader::SIZE..body.len() - meta_len]
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect();
            Ok(Some(Frame::Audio(AudioFrame {
                header: audio,
                timestamp: header.timestamp,
                planar,
            })))
        }
        FrameType::Metadata => {
            let xml = String::from_utf8_lossy(&body)
                .trim_end_matches('\0')
                .to_string();
            Ok(Some(Frame::Metadata {
                timestamp: header.timestamp,
                xml,
            }))
        }
    }
}

/// Largest frame body accepted (8K VMX HQ is well under this).
const MAX_BODY: usize = 256 << 20;

fn body_len(header: &FrameHeader) -> Result<usize, Error> {
    let len = header.data_length.max(0) as usize;
    if len > MAX_BODY {
        return Err(Error::Protocol(format!(
            "OMT frame of {len} bytes is implausibly large"
        )));
    }
    Ok(len)
}

// ───────────────────────────── blocking ─────────────────────────────

/// Receiver on `std::net`.
pub struct BlockingReceiver {
    stream: std::net::TcpStream,
    target: String,
}

impl BlockingReceiver {
    /// Connects to `target` (a `host:port` or a discovered source name) and
    /// subscribes per `options`.
    pub fn connect(target: &str, options: ReceiverOptions) -> Result<Self, Error> {
        use std::io::Write;
        let addr = resolve_target(target, RESOLVE_TIMEOUT_MS)?;
        log::info!("OMT: connecting to '{target}' -> {addr}");
        let mut stream = std::net::TcpStream::connect(&addr).map_err(|e| {
            Error::Io(std::io::Error::new(
                e.kind(),
                format!("OMT connect {addr}: {e}"),
            ))
        })?;
        stream.set_nodelay(true).ok();
        for frame in hello(&options) {
            stream.write_all(&frame)?;
        }
        Ok(Self {
            stream,
            target: addr,
        })
    }

    /// The resolved `host:port`.
    pub fn address(&self) -> &str {
        &self.target
    }

    /// Bounds how long [`next_frame`](Self::next_frame) waits; `None` waits
    /// forever. A timeout surfaces as `Error::Io` with kind `WouldBlock` or
    /// `TimedOut`.
    pub fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> Result<(), Error> {
        Ok(self.stream.set_read_timeout(timeout)?)
    }

    /// The next frame. `Error::Disconnected` when the sender goes away.
    pub fn next_frame(&mut self) -> Result<Frame, Error> {
        use std::io::Read;
        loop {
            let mut buf = [0u8; FrameHeader::SIZE];
            if let Err(e) = self.stream.read_exact(&mut buf) {
                return Err(match e.kind() {
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => Error::Io(e),
                    _ => Error::Disconnected(e.to_string()),
                });
            }
            let header = FrameHeader::from_bytes(&buf)?;
            let mut body = vec![0u8; body_len(&header)?];
            self.stream.read_exact(&mut body)?;
            if let Some(frame) = parse(&header, body)? {
                return Ok(frame);
            }
        }
    }

    /// Unblocks a `next_frame` waiting on another thread (it returns
    /// `Disconnected`).
    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    /// A handle that can [`shutdown`](Self::shutdown) this receiver from
    /// another thread.
    pub fn try_clone_stream(&self) -> Result<std::net::TcpStream, Error> {
        Ok(self.stream.try_clone()?)
    }
}

// ───────────────────────────── tokio ─────────────────────────────

/// Receiver on tokio.
#[cfg(feature = "tokio")]
pub struct Receiver {
    stream: tokio::net::TcpStream,
    target: String,
}

#[cfg(feature = "tokio")]
impl Receiver {
    /// Connects to `target` (a `host:port` or a discovered source name) and
    /// subscribes per `options`. Name resolution runs on a blocking thread.
    pub async fn connect(target: &str, options: ReceiverOptions) -> Result<Self, Error> {
        use tokio::io::AsyncWriteExt;
        let name = target.to_string();
        let addr = tokio::task::spawn_blocking(move || resolve_target(&name, RESOLVE_TIMEOUT_MS))
            .await
            .map_err(|e| Error::NotFound(format!("resolving '{target}': {e}")))??;
        log::info!("OMT: connecting to '{target}' -> {addr}");
        let mut stream = tokio::net::TcpStream::connect(&addr).await.map_err(|e| {
            Error::Io(std::io::Error::new(
                e.kind(),
                format!("OMT connect {addr}: {e}"),
            ))
        })?;
        stream.set_nodelay(true).ok();
        for frame in hello(&options) {
            stream.write_all(&frame).await?;
        }
        Ok(Self {
            stream,
            target: addr,
        })
    }

    pub fn address(&self) -> &str {
        &self.target
    }

    /// The next frame. `Error::Disconnected` when the sender goes away.
    pub async fn next_frame(&mut self) -> Result<Frame, Error> {
        use tokio::io::AsyncReadExt;
        loop {
            let mut buf = [0u8; FrameHeader::SIZE];
            if let Err(e) = self.stream.read_exact(&mut buf).await {
                return Err(Error::Disconnected(e.to_string()));
            }
            let header = FrameHeader::from_bytes(&buf)?;
            let mut body = vec![0u8; body_len(&header)?];
            self.stream.read_exact(&mut body).await?;
            if let Some(frame) = parse(&header, body)? {
                return Ok(frame);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_frames_carry_no_null_and_zero_metadata_length() {
        let f = metadata_frame(commands::SUBSCRIBE_VIDEO);
        let h = FrameHeader::from_bytes(f[..FrameHeader::SIZE].try_into().unwrap()).unwrap();
        assert_eq!(h.frame_type, FrameType::Metadata);
        assert_eq!(h.metadata_length, 0);
        assert_eq!(h.data_length as usize, commands::SUBSCRIBE_VIDEO.len());
        assert_eq!(
            &f[FrameHeader::SIZE..],
            commands::SUBSCRIBE_VIDEO.as_bytes()
        );
    }

    #[test]
    fn hello_subscribes_then_asks_for_quality() {
        let frames = hello(&ReceiverOptions {
            quality: Some(Quality::Low),
            ..Default::default()
        });
        let text: Vec<_> = frames
            .iter()
            .map(|f| String::from_utf8_lossy(&f[FrameHeader::SIZE..]).to_string())
            .collect();
        assert_eq!(
            text,
            [
                commands::SUBSCRIBE_VIDEO,
                commands::SUBSCRIBE_AUDIO,
                r#"<OMTSettings Quality="Low" />"#
            ]
        );
    }

    #[test]
    fn host_port_targets_skip_discovery() {
        assert_eq!(resolve_target("10.0.0.5:6960", 0).unwrap(), "10.0.0.5:6960");
        assert_eq!(
            resolve_target("[2001:db8::1]:6960", 0).unwrap(),
            "[2001:db8::1]:6960"
        );
    }
}
