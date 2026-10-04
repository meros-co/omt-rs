//! OMT wire protocol: frame headers and metadata commands.
//!
//! All integers are little-endian per the OMT 1.0 spec.
//! Frame layout: [FrameHeader (16)] [extended header] [data] [metadata XML].

/// OMT timestamp unit: 1 second = 10,000,000 ticks.
pub const TICKS_PER_SECOND: i64 = 10_000_000;

/// DNS-SD service type for OMT discovery.
pub const SERVICE_TYPE: &str = "_omt._tcp";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    Metadata = 1,
    Video = 2,
    Audio = 4,
}

impl FrameType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Metadata),
            2 => Some(Self::Video),
            4 => Some(Self::Audio),
            _ => None,
        }
    }
}

/// 16-byte header preceding every OMT frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameHeader {
    pub version: u8,
    pub frame_type: FrameType,
    /// Ticks (1s = 10,000,000). See [`TICKS_PER_SECOND`].
    pub timestamp: i64,
    /// Length of null-terminated UTF-8 XML metadata, including the null byte.
    pub metadata_length: u16,
    /// Extended header + data + metadata length, excluding this header.
    pub data_length: i32,
}

impl FrameHeader {
    pub const SIZE: usize = 16;

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0] = self.version;
        buf[1] = self.frame_type as u8;
        buf[2..10].copy_from_slice(&self.timestamp.to_le_bytes());
        buf[10..12].copy_from_slice(&self.metadata_length.to_le_bytes());
        buf[12..16].copy_from_slice(&self.data_length.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8; Self::SIZE]) -> Result<Self, ProtocolError> {
        let version = buf[0];
        if version != 1 {
            return Err(ProtocolError::UnsupportedVersion(version));
        }
        let frame_type =
            FrameType::from_u8(buf[1]).ok_or(ProtocolError::InvalidFrameType(buf[1]))?;
        Ok(Self {
            version,
            frame_type,
            timestamp: i64::from_le_bytes(buf[2..10].try_into().unwrap()),
            metadata_length: u16::from_le_bytes(buf[10..12].try_into().unwrap()),
            data_length: i32::from_le_bytes(buf[12..16].try_into().unwrap()),
        })
    }
}

/// Video frame flags (bitfield).
pub mod video_flags {
    pub const INTERLACED: i32 = 1;
    pub const ALPHA: i32 = 2;
    pub const PREMULTIPLIED: i32 = 4;
    pub const PREVIEW: i32 = 8;
    pub const HIGH_BIT_DEPTH: i32 = 16;
}

/// 32-byte extended header, mandatory for video frames.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoHeader {
    /// Video codec FourCC (e.g. b"VMX1").
    pub codec: [u8; 4],
    pub width: i32,
    pub height: i32,
    pub frame_rate_n: i32,
    pub frame_rate_d: i32,
    /// Display aspect ratio, e.g. 16.0/9.0.
    pub aspect_ratio: f32,
    /// See [`video_flags`].
    pub flags: i32,
    /// 601, 709, or 0 for undefined.
    pub color_space: i32,
}

impl VideoHeader {
    pub const SIZE: usize = 32;

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..4].copy_from_slice(&self.codec);
        buf[4..8].copy_from_slice(&self.width.to_le_bytes());
        buf[8..12].copy_from_slice(&self.height.to_le_bytes());
        buf[12..16].copy_from_slice(&self.frame_rate_n.to_le_bytes());
        buf[16..20].copy_from_slice(&self.frame_rate_d.to_le_bytes());
        buf[20..24].copy_from_slice(&self.aspect_ratio.to_le_bytes());
        buf[24..28].copy_from_slice(&self.flags.to_le_bytes());
        buf[28..32].copy_from_slice(&self.color_space.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8; Self::SIZE]) -> Self {
        Self {
            codec: buf[0..4].try_into().unwrap(),
            width: i32::from_le_bytes(buf[4..8].try_into().unwrap()),
            height: i32::from_le_bytes(buf[8..12].try_into().unwrap()),
            frame_rate_n: i32::from_le_bytes(buf[12..16].try_into().unwrap()),
            frame_rate_d: i32::from_le_bytes(buf[16..20].try_into().unwrap()),
            aspect_ratio: f32::from_le_bytes(buf[20..24].try_into().unwrap()),
            flags: i32::from_le_bytes(buf[24..28].try_into().unwrap()),
            color_space: i32::from_le_bytes(buf[28..32].try_into().unwrap()),
        }
    }
}

/// 24-byte extended header, mandatory for audio frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioHeader {
    /// Audio codec FourCC; only b"FPA1" (32-bit float planar) is defined.
    pub codec: [u8; 4],
    pub sample_rate: i32,
    pub samples_per_channel: i32,
    pub channels: i32,
    /// Bitfield of channels actually present (silent channels skipped).
    pub active_channels: u32,
    pub reserved1: i32,
}

impl AudioHeader {
    pub const SIZE: usize = 24;

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..4].copy_from_slice(&self.codec);
        buf[4..8].copy_from_slice(&self.sample_rate.to_le_bytes());
        buf[8..12].copy_from_slice(&self.samples_per_channel.to_le_bytes());
        buf[12..16].copy_from_slice(&self.channels.to_le_bytes());
        buf[16..20].copy_from_slice(&self.active_channels.to_le_bytes());
        buf[20..24].copy_from_slice(&self.reserved1.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8; Self::SIZE]) -> Self {
        Self {
            codec: buf[0..4].try_into().unwrap(),
            sample_rate: i32::from_le_bytes(buf[4..8].try_into().unwrap()),
            samples_per_channel: i32::from_le_bytes(buf[8..12].try_into().unwrap()),
            channels: i32::from_le_bytes(buf[12..16].try_into().unwrap()),
            active_channels: u32::from_le_bytes(buf[16..20].try_into().unwrap()),
            reserved1: i32::from_le_bytes(buf[20..24].try_into().unwrap()),
        }
    }
}

/// Fixed metadata command strings. The spec requires exact string matching
/// (no XML parsing) for these.
pub mod commands {
    pub const SUBSCRIBE_VIDEO: &str = r#"<OMTSubscribe Video="true" />"#;
    pub const SUBSCRIBE_AUDIO: &str = r#"<OMTSubscribe Audio="true" />"#;
    pub const SUBSCRIBE_METADATA: &str = r#"<OMTSubscribe Metadata="true" />"#;
    pub const PREVIEW_ON: &str = r#"<OMTSettings Preview="true" />"#;
    pub const PREVIEW_OFF: &str = r#"<OMTSettings Preview="false" />"#;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    UnsupportedVersion(u8),
    InvalidFrameType(u8),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVersion(v) => write!(f, "unsupported OMT version: {v}"),
            Self::InvalidFrameType(t) => write!(f, "invalid OMT frame type: {t}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_header_round_trip() {
        let h = FrameHeader {
            version: 1,
            frame_type: FrameType::Video,
            timestamp: 123 * TICKS_PER_SECOND,
            metadata_length: 42,
            data_length: 9000,
        };
        let bytes = h.to_bytes();
        assert_eq!(bytes.len(), FrameHeader::SIZE);
        assert_eq!(FrameHeader::from_bytes(&bytes).unwrap(), h);
    }

    #[test]
    fn frame_header_rejects_bad_version() {
        let mut bytes = [0u8; FrameHeader::SIZE];
        bytes[0] = 2;
        bytes[1] = 2;
        assert_eq!(
            FrameHeader::from_bytes(&bytes),
            Err(ProtocolError::UnsupportedVersion(2))
        );
    }

    #[test]
    fn frame_header_rejects_bad_type() {
        let mut bytes = [0u8; FrameHeader::SIZE];
        bytes[0] = 1;
        bytes[1] = 3;
        assert_eq!(
            FrameHeader::from_bytes(&bytes),
            Err(ProtocolError::InvalidFrameType(3))
        );
    }

    #[test]
    fn video_header_round_trip() {
        let h = VideoHeader {
            codec: *b"VMX1",
            width: 1920,
            height: 1080,
            frame_rate_n: 60,
            frame_rate_d: 1,
            aspect_ratio: 16.0 / 9.0,
            flags: video_flags::ALPHA,
            color_space: 709,
        };
        let bytes = h.to_bytes();
        assert_eq!(bytes.len(), VideoHeader::SIZE);
        assert_eq!(VideoHeader::from_bytes(&bytes), h);
    }

    #[test]
    fn audio_header_round_trip() {
        let h = AudioHeader {
            codec: *b"FPA1",
            sample_rate: 48000,
            samples_per_channel: 1024,
            channels: 2,
            active_channels: 0b11,
            reserved1: 0,
        };
        let bytes = h.to_bytes();
        assert_eq!(bytes.len(), AudioHeader::SIZE);
        assert_eq!(AudioHeader::from_bytes(&bytes), h);
    }

    #[test]
    fn headers_are_little_endian() {
        let h = FrameHeader {
            version: 1,
            frame_type: FrameType::Metadata,
            timestamp: 0x0102030405060708,
            metadata_length: 0xAABB,
            data_length: 0x11223344,
        };
        let b = h.to_bytes();
        assert_eq!(b[2], 0x08); // LSB of timestamp first
        assert_eq!(b[10], 0xBB); // LSB of metadata_length first
        assert_eq!(b[12], 0x44); // LSB of data_length first
    }
}
