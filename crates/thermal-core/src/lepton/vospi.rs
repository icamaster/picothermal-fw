//! Video over SPI (VoSPI) protocol for the Lepton 3.x.
//!
//! A video frame is delivered as 4 segments of 60 packets. Every packet is
//! 164 bytes: a 2-byte ID, a 2-byte CRC, and 160 bytes of payload (80 pixels
//! of 16-bit video). When the camera has nothing to send it emits discard
//! packets, which must be read and dropped to keep the stream in sync.

/// Size of a VoSPI packet in bytes.
pub const PACKET_LEN: usize = 164;

/// Payload bytes per packet.
pub const PAYLOAD_LEN: usize = 160;

/// Video packets per segment on the Lepton 3.x.
pub const PACKETS_PER_SEGMENT: usize = 60;

/// Segments per video frame on the Lepton 3.x.
pub const SEGMENTS_PER_FRAME: usize = 4;

/// Packet number that carries the segment ID in its packet ID field.
pub const SEGMENT_ID_PACKET: u16 = 20;

/// Header of a single VoSPI packet, parsed from its first four bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketHeader {
    /// Raw 16-bit packet ID field.
    pub id: u16,
    /// Raw 16-bit CRC field.
    pub crc: u16,
}

impl PacketHeader {
    /// Parses a packet header from the start of a packet buffer.
    ///
    /// Returns `None` if the buffer is shorter than a packet header.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let id = u16::from_be_bytes([*bytes.first()?, *bytes.get(1)?]);
        let crc = u16::from_be_bytes([*bytes.get(2)?, *bytes.get(3)?]);
        Some(Self { id, crc })
    }

    /// Whether this is a discard packet (no video data, must be dropped).
    pub fn is_discard(self) -> bool {
        self.id & 0x0F00 == 0x0F00
    }

    /// Packet number within the segment (0..=59 for video packets).
    pub fn packet_number(self) -> u16 {
        self.id & 0x0FFF
    }

    /// Segment number (1..=4), only meaningful in packet 20; 0 means the
    /// segment is invalid and the whole segment must be dropped.
    pub fn segment(self) -> u8 {
        (self.id >> 12) as u8 & 0x07
    }
}

// TODO(vospi): synchronization state machine (idle -> resync backoff ->
// streaming), CRC-16/CCITT validation, and segment/frame assembly into a
// caller-provided buffer.

#[cfg(test)]
mod tests {
    use super::{PACKET_LEN, PacketHeader};

    fn header(id: u16) -> PacketHeader {
        let mut packet = [0u8; PACKET_LEN];
        packet[..2].copy_from_slice(&id.to_be_bytes());
        packet[2..4].copy_from_slice(&0xABCDu16.to_be_bytes());
        PacketHeader::parse(&packet).unwrap()
    }

    #[test]
    fn parses_id_and_crc() {
        let h = header(0x1014);
        assert_eq!(h.id, 0x1014);
        assert_eq!(h.crc, 0xABCD);
    }

    #[test]
    fn detects_discard_packets() {
        assert!(header(0x0F00).is_discard());
        assert!(header(0x3FFF).is_discard());
        assert!(!header(0x0000).is_discard());
        assert!(!header(0x0E3B).is_discard());
    }

    #[test]
    fn extracts_packet_number() {
        assert_eq!(header(0x0000).packet_number(), 0);
        assert_eq!(header(0x103B).packet_number(), 0x3B);
    }

    #[test]
    fn extracts_segment_from_packet_20() {
        // Packet 20 with segment 3 in the TTT bits.
        let h = header(0x3014);
        assert_eq!(h.packet_number(), 20);
        assert_eq!(h.segment(), 3);
        // Segment 0 marks an invalid segment.
        assert_eq!(header(0x0014).segment(), 0);
    }

    #[test]
    fn rejects_short_buffers() {
        assert_eq!(PacketHeader::parse(&[0x00, 0x14, 0xAB]), None);
    }
}
