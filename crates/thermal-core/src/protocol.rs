//! Wire protocol between the firmware and WebUSB hosts.
//!
//! Video flows over the bulk IN endpoint as frames, each preceded by a
//! fixed-size [`FrameHeader`]. Hosts send [`Command`]s on the bulk OUT
//! endpoint to configure the camera at runtime.
//!
//! All multi-byte fields are little-endian.
//!
//! A frame is header plus payload in one bulk transfer, and its total length is
//! always 16 bytes past a multiple of the 64-byte packet size, so the transfer
//! always ends on a short packet. Hosts can therefore ask for the largest frame
//! they support and let the short packet terminate the read; the device never
//! has to send a zero-length packet.

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
    pub const fn bytes_per_pixel(self) -> usize {
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
    /// Bit flags describing the frame, see [`FrameHeader::FLAG_FFC`].
    pub flags: u8,
}

impl FrameHeader {
    /// The camera was running a flat-field correction when this frame was
    /// captured, so its pixels show the shutter rather than the scene.
    pub const FLAG_FFC: u8 = 1 << 0;

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
        // Byte 14 was reserved and always zero before flags existed, so hosts
        // that predate them read every frame as unflagged.
        bytes[14] = self.flags;
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
            flags: bytes[14],
        })
    }
}

/// Commands sent by the host on the bulk OUT endpoint.
///
/// Every command is [`Command::LEN`] bytes: an opcode, then a little-endian
/// 16-bit argument, which commands without one leave as zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Switch the streamed pixel format starting with the next frame.
    SetPixelFormat(PixelFormat),
    /// Select the camera's gain mode, trading temperature range for
    /// resolution.
    SetGainMode(GainMode),
    /// Run a flat-field correction now instead of waiting for the camera's
    /// own schedule.
    RunFfc,
    /// Set the scene emissivity used by radiometry, scaled so that 8192 is
    /// 1.0, the camera's own representation.
    SetEmissivity(u16),
}

impl Command {
    /// Encoded size of a command in bytes.
    pub const LEN: usize = 3;

    /// Emissivity of 1.0 in the scaled form [`Command::SetEmissivity`] uses.
    pub const EMISSIVITY_ONE: u16 = 8192;

    /// Lowest emissivity accepted, 0.01. Below that radiometry is meaningless.
    pub const EMISSIVITY_MIN: u16 = 82;

    /// Encodes the command into its fixed-size wire representation.
    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let (opcode, argument) = match *self {
            Self::SetPixelFormat(format) => (0x01, format as u16),
            Self::SetGainMode(mode) => (0x02, mode as u16),
            Self::RunFfc => (0x03, 0),
            Self::SetEmissivity(scaled) => (0x04, scaled),
        };
        let [low, high] = argument.to_le_bytes();
        [opcode, low, high]
    }

    /// Decodes a command from the start of `bytes`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let [opcode, low, high, ..] = *bytes else {
            return Err(Error::TooShort);
        };
        let argument = u16::from_le_bytes([low, high]);
        let byte = || u8::try_from(argument).map_err(|_| Error::BadValue);

        match opcode {
            0x01 => Ok(Self::SetPixelFormat(PixelFormat::from_u8(byte()?)?)),
            0x02 => Ok(Self::SetGainMode(GainMode::from_u8(byte()?)?)),
            0x03 => Ok(Self::RunFfc),
            0x04 if (Self::EMISSIVITY_MIN..=Self::EMISSIVITY_ONE).contains(&argument) => {
                Ok(Self::SetEmissivity(argument))
            }
            _ => Err(Error::BadValue),
        }
    }
}

/// The camera's gain mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GainMode {
    /// Finest resolution, roughly -10 to 140 degrees C. The camera's default.
    High = 0,
    /// Extended range up to roughly 400 degrees C, at coarser resolution.
    Low = 1,
    /// The camera switches between the two as the scene requires.
    Auto = 2,
}

impl GainMode {
    fn from_u8(value: u8) -> Result<Self, Error> {
        match value {
            0 => Ok(Self::High),
            1 => Ok(Self::Low),
            2 => Ok(Self::Auto),
            _ => Err(Error::BadValue),
        }
    }
}

/// Splits one frame into bulk endpoint packets.
///
/// The device writes each packet as it is produced, so a frame never has to
/// exist in RAM. Ending the transfer is the caller's only subtlety: a bulk
/// transfer ends on a packet shorter than the endpoint's maximum, so when a
/// frame happens to be an exact multiple of the packet size, a zero-length
/// packet has to follow it.
pub struct Packetizer {
    header: [u8; FrameHeader::LEN],
    payload_len: usize,
    packet_size: usize,
    sent: usize,
    started: bool,
    zlp_pending: bool,
}

impl Packetizer {
    /// Starts packetizing the frame described by `header`.
    ///
    /// Panics if `packet_size` cannot hold a [`FrameHeader`].
    pub fn new(header: FrameHeader, packet_size: usize) -> Self {
        assert!(packet_size >= FrameHeader::LEN);
        let payload_len = header.payload_len();
        Self {
            header: header.to_bytes(),
            payload_len,
            packet_size,
            sent: 0,
            started: false,
            zlp_pending: (FrameHeader::LEN + payload_len).is_multiple_of(packet_size),
        }
    }

    /// Writes the next packet into `buf` and returns its length, or `None` once
    /// the frame is complete.
    ///
    /// `fill` is handed the payload offset to write from and the slice to write
    /// into, so the payload can be generated or copied on demand.
    pub fn next_packet<F>(&mut self, buf: &mut [u8], fill: F) -> Option<usize>
    where
        F: FnOnce(usize, &mut [u8]),
    {
        let capacity = buf.len().min(self.packet_size);
        assert!(capacity >= FrameHeader::LEN);

        if !self.started {
            self.started = true;
            buf[..FrameHeader::LEN].copy_from_slice(&self.header);
            let len = (capacity - FrameHeader::LEN).min(self.payload_len);
            fill(0, &mut buf[FrameHeader::LEN..FrameHeader::LEN + len]);
            self.sent = len;
            return Some(FrameHeader::LEN + len);
        }

        if self.sent < self.payload_len {
            let len = capacity.min(self.payload_len - self.sent);
            fill(self.sent, &mut buf[..len]);
            self.sent += len;
            return Some(len);
        }

        if self.zlp_pending {
            self.zlp_pending = false;
            return Some(0);
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, Error, FrameHeader, GainMode, Packetizer, PixelFormat, VERSION};
    use crate::frame::{self, TestPattern};

    fn lepton_header() -> FrameHeader {
        FrameHeader {
            seq: 0xDEAD_BEEF,
            width: 160,
            height: 120,
            format: PixelFormat::Y16,
            flags: FrameHeader::FLAG_FFC,
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
        let commands = [
            Command::SetPixelFormat(PixelFormat::Y16),
            Command::SetPixelFormat(PixelFormat::Agc8),
            Command::SetGainMode(GainMode::High),
            Command::SetGainMode(GainMode::Low),
            Command::SetGainMode(GainMode::Auto),
            Command::RunFfc,
            Command::SetEmissivity(Command::EMISSIVITY_MIN),
            Command::SetEmissivity(7782),
            Command::SetEmissivity(Command::EMISSIVITY_ONE),
        ];
        for cmd in commands {
            assert_eq!(Command::from_bytes(&cmd.to_bytes()), Ok(cmd));
        }
    }

    #[test]
    fn command_argument_is_little_endian() {
        assert_eq!(
            Command::SetEmissivity(0x1234).to_bytes(),
            [0x04, 0x34, 0x12]
        );
    }

    #[test]
    fn command_rejects_invalid_input() {
        assert_eq!(Command::from_bytes(&[]), Err(Error::TooShort));
        assert_eq!(Command::from_bytes(&[0x01, 0x00]), Err(Error::TooShort));
        assert_eq!(
            Command::from_bytes(&[0x01, 0xFF, 0x00]),
            Err(Error::BadValue)
        );
        assert_eq!(
            Command::from_bytes(&[0x02, 0x03, 0x00]),
            Err(Error::BadValue)
        );
        assert_eq!(
            Command::from_bytes(&[0x7F, 0x00, 0x00]),
            Err(Error::BadValue)
        );
        // Emissivity outside 0.01..=1.0.
        assert_eq!(
            Command::from_bytes(&[0x04, 0x00, 0x00]),
            Err(Error::BadValue)
        );
        assert_eq!(
            Command::from_bytes(&[0x04, 0x01, 0x20]),
            Err(Error::BadValue)
        );
        // A byte-sized argument must not have its high byte set.
        assert_eq!(
            Command::from_bytes(&[0x02, 0x00, 0x01]),
            Err(Error::BadValue)
        );
    }

    /// Largest transfer the real geometry produces, and the packet count that
    /// needs, so the mock host can work from fixed buffers like the device does.
    const MAX_TRANSFER: usize = FrameHeader::LEN + frame::PIXELS * 2;
    const MAX_PACKETS: usize = MAX_TRANSFER / 64 + 2;

    /// Stands in for a host: consumes packets the way a bulk transfer does,
    /// stopping at the first packet shorter than the endpoint maximum.
    struct MockHost {
        packet_sizes: [usize; MAX_PACKETS],
        packets: usize,
        bytes: [u8; MAX_TRANSFER],
        len: usize,
    }

    impl MockHost {
        fn drain(
            packetizer: &mut Packetizer,
            packet_size: usize,
            pattern: &TestPattern,
            format: PixelFormat,
        ) -> Self {
            let mut host = MockHost {
                packet_sizes: [0; MAX_PACKETS],
                packets: 0,
                bytes: [0; MAX_TRANSFER],
                len: 0,
            };
            let mut buf = [0u8; 64];

            while let Some(len) = packetizer.next_packet(&mut buf[..packet_size], |offset, dst| {
                pattern.fill(format, offset, dst)
            }) {
                host.packet_sizes[host.packets] = len;
                host.packets += 1;
                host.bytes[host.len..host.len + len].copy_from_slice(&buf[..len]);
                host.len += len;
                if len < packet_size {
                    break;
                }
            }
            host
        }
    }

    #[test]
    fn mock_host_reassembles_a_frame() {
        const PACKET: usize = 64;

        for format in [PixelFormat::Y16, PixelFormat::Agc8] {
            let pattern = TestPattern::new();
            let header = FrameHeader {
                seq: 7,
                width: frame::WIDTH as u16,
                height: frame::HEIGHT as u16,
                format,
                flags: 0,
            };
            let mut packetizer = Packetizer::new(header, PACKET);
            let host = MockHost::drain(&mut packetizer, PACKET, &pattern, format);

            // The host sees exactly one frame: a valid header, then a payload
            // identical to what the source would have rendered in one go.
            assert_eq!(FrameHeader::from_bytes(&host.bytes).unwrap(), header);
            assert_eq!(host.len, FrameHeader::LEN + header.payload_len());

            let mut expected = [0u8; MAX_TRANSFER];
            let payload_len = header.payload_len();
            pattern.fill(format, 0, &mut expected[..payload_len]);
            assert_eq!(
                host.bytes[FrameHeader::LEN..host.len],
                expected[..payload_len]
            );

            // Every packet is full except the last, which is what ends the
            // transfer on the host side.
            let sizes = &host.packet_sizes[..host.packets];
            let (last, rest) = sizes.split_last().unwrap();
            assert!(rest.iter().all(|&len| len == PACKET));
            assert!(*last < PACKET);
        }
    }

    #[test]
    fn a_frame_that_fills_its_last_packet_ends_with_a_zero_length_packet() {
        const PACKET: usize = 64;

        // 24 x 2 pixels of Agc8 is 48 payload bytes, so header plus payload is
        // exactly one packet and the transfer needs a ZLP to terminate.
        let header = FrameHeader {
            seq: 0,
            width: 24,
            height: 2,
            format: PixelFormat::Agc8,
            flags: 0,
        };
        assert_eq!(FrameHeader::LEN + header.payload_len(), PACKET);

        let mut packetizer = Packetizer::new(header, PACKET);
        let mut buf = [0u8; PACKET];
        let mut sizes = [0usize; 4];
        let mut count = 0;
        while let Some(len) = packetizer.next_packet(&mut buf, |_, dst| dst.fill(0)) {
            sizes[count] = len;
            count += 1;
        }
        assert_eq!(&sizes[..count], &[PACKET, 0]);
    }

    #[test]
    fn the_real_geometry_never_needs_a_zero_length_packet() {
        for format in [PixelFormat::Y16, PixelFormat::Agc8] {
            let total = FrameHeader::LEN + frame::payload_len(format);
            assert_ne!(total % 64, 0);
        }
    }
}
