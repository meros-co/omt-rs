//! Open Media Transport (OMT) in Rust.
//!
//! * [`send::Sender`] - publish video and audio; receivers connect over TCP.
//! * [`receive::Receiver`] (tokio) / [`receive::BlockingReceiver`] - connect to
//!   a sender by name or address and read frames.
//! * [`discovery`] - find senders on the LAN (mDNS `_omt._tcp`) and advertise.
//! * [`vmx`] - the VMX video codec (vendored libvmx, MIT).
//! * [`metadata`] - tally and sender information, as libomt writes them.
//! * [`protocol`] - the wire format.
//! * [`net`] - pin OMT to one network interface on a multi-homed machine.
//!
//! The protocol layer is pure Rust, written against the open spec and checked
//! against the reference implementation (`openmediatransport/libomt`) where
//! the two disagree; see the README for the cases that matter.
//!
//! Timestamps are OMT ticks: 100 ns units ([`protocol::TICKS_PER_SECOND`]).

pub mod discovery;
pub mod metadata;
pub mod net;
pub mod protocol;
pub mod receive;
pub mod send;
pub mod vmx;

pub use metadata::{SenderInfo, Tally};
#[cfg(feature = "tokio")]
pub use receive::Receiver;
pub use receive::{BlockingReceiver, Frame, ReceiverControl, ReceiverOptions, ReceiverStats};
pub use send::{PixelFormat, SendOutcome, Sender, SenderConfig, SenderStats};

/// Errors from sending, receiving and the codec.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// The peer sent something that is not valid OMT.
    Protocol(String),
    /// VMX failed to encode or decode.
    Codec(String),
    /// A source name did not resolve.
    NotFound(String),
    /// The connection closed.
    Disconnected(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Protocol(m) | Self::Codec(m) | Self::NotFound(m) => f.write_str(m),
            Self::Disconnected(m) => write!(f, "disconnected: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<protocol::ProtocolError> for Error {
    fn from(e: protocol::ProtocolError) -> Self {
        Self::Protocol(e.to_string())
    }
}

/// Smoke test: the protocol layer and the codec both work on this target.
pub fn probe() -> String {
    let header_ok = protocol::FrameHeader {
        version: 1,
        frame_type: protocol::FrameType::Video,
        timestamp: 0,
        metadata_length: 0,
        data_length: 0,
    }
    .to_bytes()
    .len()
        == protocol::FrameHeader::SIZE;
    format!(
        "OMT protocol: {}\nVMX codec: {}",
        if header_ok {
            "OK ✓"
        } else {
            "header size mismatch"
        },
        vmx::probe()
    )
}
