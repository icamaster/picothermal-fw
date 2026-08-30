//! Wire protocol between the firmware and WebUSB hosts.
//!
//! Video flows over the bulk IN endpoint as frames, each preceded by a
//! fixed-size [`FrameHeader`]. Hosts send [`Command`]s on the bulk OUT
//! endpoint, e.g. to switch the pixel format at runtime.
//!
//! All multi-byte fields are little-endian.

/// Magic bytes opening every frame header.
pub const MAGIC: [u8; 4] = *b"THRM";

/// Protocol version carried in every frame header.
pub const VERSION: u8 = 1;

/// Errors returned when decoding protocol messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The buffer is too short to contain the message.
    TooShort,
    /// The frame header magic did not match [`MAGIC`].
    BadMagic,
    /// The protocol version is not supported.
    BadVersion(u8),
    /// A field held a value outside its valid range.
    BadValue,
}

/// Pixel format of a frame payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PixelFormat {
    /// Raw radiometric 16-bit pixels (TLinear, centikelvin).
    Y16 = 0,
    /// AGC-processed 8-bit grayscale pixels.
    Agc8 = 1,
}

impl PixelFormat {
    /// Bytes per pixel for this format.
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Y16 => 2,
            Self::Agc8 => 1,
        }
    }

    fn from_u8(value: u8) -> Result<Self, Error> {
        match value {
            0 => Ok(Self::Y16),
            1 => Ok(Self::Agc8),
            _ => Err(Error::BadValue),
        }
    }
}

/// Fixed-size header preceding every frame payload on the bulk IN endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Frame sequence number, wrapping; lets hosts detect dropped frames.
    pub seq: u32,
    /// Frame width in pixels.
    pub width: u16,
    /// Frame height in pixels.
    pub height: u16,
    /// Pixel format of the payload.
    pub format: PixelFormat,
}

impl FrameHeader {
    /// Encoded size of a frame header in bytes.
    pub const LEN: usize = 16;

    /// Length in bytes of the payload following this header.
    pub fn payload_len(&self) -> usize {
        usize::from(self.width) * usize::from(self.height) * self.format.bytes_per_pixel()
    }

    /// Encodes the header into its fixed-size wire representation.
    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut bytes = [0u8; Self::LEN];
        bytes[..4].copy_from_slice(&MAGIC);
        bytes[4] = VERSION;
        bytes[5] = self.format as u8;
        bytes[6..8].copy_from_slice(&self.width.to_le_bytes());
        bytes[8..10].copy_from_slice(&self.height.to_le_bytes());
        bytes[10..14].copy_from_slice(&self.seq.to_le_bytes());
        bytes
    }

    /// Decodes a header from the start of `bytes`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < Self::LEN {
            return Err(Error::TooShort);
        }
        if bytes[..4] != MAGIC {
            return Err(Error::BadMagic);
        }
        if bytes[4] != VERSION {
            return Err(Error::BadVersion(bytes[4]));
        }
        Ok(Self {
            format: PixelFormat::from_u8(bytes[5])?,
            width: u16::from_le_bytes([bytes[6], bytes[7]]),
            height: u16::from_le_bytes([bytes[8], bytes[9]]),
            seq: u32::from_le_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]),
        })
    }
}

/// Commands sent by the host on the bulk OUT endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Switch the streamed pixel format starting with the next frame.
    SetPixelFormat(PixelFormat),
}

impl Command {
    /// Encoded size of a command in bytes.
    pub const LEN: usize = 2;

    /// Encodes the command into its fixed-size wire representation.
    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        match self {
            Self::SetPixelFormat(format) => [0x01, *format as u8],
        }
    }

    /// Decodes a command from the start of `bytes`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        match bytes {
            [0x01, format, ..] => Ok(Self::SetPixelFormat(PixelFormat::from_u8(*format)?)),
            [] | [_] => Err(Error::TooShort),
            _ => Err(Error::BadValue),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, Error, FrameHeader, PixelFormat, VERSION};

    fn lepton_header() -> FrameHeader {
        FrameHeader {
            seq: 0xDEAD_BEEF,
            width: 160,
            height: 120,
            format: PixelFormat::Y16,
        }
    }

    #[test]
    fn frame_header_roundtrips() {
        for format in [PixelFormat::Y16, PixelFormat::Agc8] {
            let header = FrameHeader {
                format,
                ..lepton_header()
            };
            let decoded = FrameHeader::from_bytes(&header.to_bytes()).unwrap();
            assert_eq!(decoded, header);
        }
    }

    #[test]
    fn payload_len_matches_format() {
        assert_eq!(lepton_header().payload_len(), 160 * 120 * 2);
        let agc = FrameHeader {
            format: PixelFormat::Agc8,
            ..lepton_header()
        };
        assert_eq!(agc.payload_len(), 160 * 120);
    }

    #[test]
    fn rejects_bad_magic_version_and_length() {
        let good = lepton_header().to_bytes();

        let mut bad_magic = good;
        bad_magic[0] = b'X';
        assert_eq!(FrameHeader::from_bytes(&bad_magic), Err(Error::BadMagic));

        let mut bad_version = good;
        bad_version[4] = VERSION + 1;
        assert_eq!(
            FrameHeader::from_bytes(&bad_version),
            Err(Error::BadVersion(VERSION + 1))
        );

        assert_eq!(
            FrameHeader::from_bytes(&good[..FrameHeader::LEN - 1]),
            Err(Error::TooShort)
        );
    }

    #[test]
    fn command_roundtrips() {
        for format in [PixelFormat::Y16, PixelFormat::Agc8] {
            let cmd = Command::SetPixelFormat(format);
            assert_eq!(Command::from_bytes(&cmd.to_bytes()), Ok(cmd));
        }
    }

    #[test]
    fn command_rejects_invalid_input() {
        assert_eq!(Command::from_bytes(&[]), Err(Error::TooShort));
        assert_eq!(Command::from_bytes(&[0x01]), Err(Error::TooShort));
        assert_eq!(Command::from_bytes(&[0x01, 0xFF]), Err(Error::BadValue));
        assert_eq!(Command::from_bytes(&[0x7F, 0x00]), Err(Error::BadValue));
    }
}
