//! Video over SPI (VoSPI) protocol for the Lepton 3.x.
//!
//! A video frame is delivered as 4 segments of 60 packets. Every packet is
//! 164 bytes: a 2-byte ID, a 2-byte CRC, and 160 bytes of payload (80 pixels
//! of 16-bit video). When the camera has nothing to send it emits discard
//! packets, which must be read and dropped to keep the stream in sync.
//!
//! [`Assembler`] is the whole protocol as a state machine over packets, with no
//! SPI in sight, so it can be driven from host tests. The caller's only duties
//! are reading packets and, on [`Progress::Desync`], dropping chip select for
//! [`RESYNC_DELAY_MS`] before calling [`Assembler::resync`].

use crate::frame::{HEIGHT, WIDTH};

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

/// Bytes in an assembled frame, as 16-bit little-endian pixels.
pub const FRAME_LEN: usize = WIDTH * HEIGHT * 2;

/// How long chip select must stay idle to resynchronise, which the datasheet
/// puts at longer than five frame periods.
pub const RESYNC_DELAY_MS: u32 = 185;

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

/// Lookup table for [`crc16`], one entry per byte value.
///
/// The CRC runs on every packet in the capture loop, and on a Cortex-M0+ the
/// bit-at-a-time form costs about as long as the SPI transfer of the packet
/// itself. Reading slower than the camera produces corrupts the stream, so the
/// 512 bytes of flash are worth it.
static CRC_TABLE: [u16; 256] = {
    let mut table = [0u16; 256];
    let mut byte = 0;
    while byte < 256 {
        let mut crc = (byte as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[byte] = crc;
        byte += 1;
    }
    table
};

/// CRC-16/CCITT over `bytes`, the XMODEM variant: polynomial 0x1021, initial
/// value zero, no reflection and no final xor.
fn crc16_over(bytes: impl Iterator<Item = u8>) -> u16 {
    bytes.fold(0, |crc, byte| {
        (crc << 8) ^ CRC_TABLE[usize::from((crc >> 8) as u8 ^ byte)]
    })
}

/// CRC-16/CCITT over a slice.
pub fn crc16(data: &[u8]) -> u16 {
    crc16_over(data.iter().copied())
}

/// CRC the camera would have computed for `packet`.
///
/// The camera zeroes the four most significant bits of the ID and the whole CRC
/// field before computing it, so those bytes are substituted on the way through
/// rather than by copying the packet.
pub fn packet_crc(packet: &[u8]) -> u16 {
    let (head, payload) = packet.split_at(4.min(packet.len()));
    let head = head.iter().enumerate().map(|(index, &byte)| match index {
        0 => byte & 0x0F,
        2 | 3 => 0,
        _ => byte,
    });
    crc16_over(head.chain(payload.iter().copied()))
}

/// What a packet did to the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// A discard packet, or a packet consumed while resynchronising.
    Discarded,
    /// A video packet was stored; the frame is not complete yet.
    InProgress,
    /// The camera marked the segment invalid, so it was dropped. The camera
    /// resends it; nothing is wrong.
    SegmentDropped,
    /// A full frame is now in the caller's buffer.
    Frame,
    /// The stream is out of sync. Drop chip select, wait [`RESYNC_DELAY_MS`],
    /// then call [`Assembler::resync`].
    Desync,
}

/// Where the assembler is in the packet stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Waiting for the caller to idle chip select and resynchronise.
    Desynced,
    /// Scanning for the end of a frame, so assembly can start on a boundary.
    FindFrameEnd,
    /// Expecting packet 0 of the next segment.
    AwaitSegment,
    /// Part way through a segment.
    InSegment,
    /// Discarding the rest of a segment the camera marked invalid.
    SkipSegment,
}

/// Reassembles the VoSPI packet stream into frames.
pub struct Assembler {
    state: State,
    next_packet: u16,
    next_segment: u8,
}

impl Default for Assembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Assembler {
    /// A newly synchronised assembler, which starts by finding a frame
    /// boundary rather than trusting the first packet it sees.
    pub const fn new() -> Self {
        Self {
            state: State::FindFrameEnd,
            next_packet: 0,
            next_segment: 1,
        }
    }

    /// Restarts synchronisation, after the caller has idled chip select for
    /// [`RESYNC_DELAY_MS`].
    pub fn resync(&mut self) {
        *self = Self::new();
    }

    /// Feeds one 164-byte packet, writing pixels into `frame`.
    ///
    /// Panics if `packet` is not [`PACKET_LEN`] bytes or `frame` is shorter
    /// than [`FRAME_LEN`].
    pub fn push(&mut self, packet: &[u8], frame: &mut [u8]) -> Progress {
        assert_eq!(packet.len(), PACKET_LEN);
        assert!(frame.len() >= FRAME_LEN);

        let Some(header) = PacketHeader::parse(packet) else {
            return self.desync();
        };

        // Discard packets carry no data and no meaningful CRC, so they are
        // dropped before anything else looks at them.
        if header.is_discard() {
            return Progress::Discarded;
        }

        if self.state == State::Desynced {
            return Progress::Desync;
        }

        if header.crc != packet_crc(packet) {
            return self.desync();
        }

        let number = header.packet_number();
        if number >= PACKETS_PER_SEGMENT as u16 {
            return self.desync();
        }

        match self.state {
            State::Desynced => Progress::Desync,

            // The segment number only appears in packet 20, so the safe place
            // to start is the last segment of a frame: whatever follows it is
            // packet 0 of segment 1.
            State::FindFrameEnd => {
                if number == SEGMENT_ID_PACKET && header.segment() == SEGMENTS_PER_FRAME as u8 {
                    self.state = State::AwaitSegment;
                    self.next_segment = 1;
                }
                Progress::Discarded
            }

            State::AwaitSegment | State::SkipSegment => {
                if number != 0 {
                    return Progress::Discarded;
                }
                self.store(packet, frame, 0);
                self.state = State::InSegment;
                self.next_packet = 1;
                Progress::InProgress
            }

            State::InSegment => {
                if number != self.next_packet {
                    return self.desync();
                }

                if number == SEGMENT_ID_PACKET {
                    match header.segment() {
                        // Zero means the camera invalidated the segment; it
                        // will send it again, so drop it without desyncing.
                        0 => {
                            self.state = State::SkipSegment;
                            return Progress::SegmentDropped;
                        }
                        segment if segment != self.next_segment => return self.desync(),
                        _ => {}
                    }
                }

                self.store(packet, frame, number);

                if number as usize + 1 < PACKETS_PER_SEGMENT {
                    self.next_packet = number + 1;
                    return Progress::InProgress;
                }

                self.state = State::AwaitSegment;
                if self.next_segment as usize == SEGMENTS_PER_FRAME {
                    self.next_segment = 1;
                    Progress::Frame
                } else {
                    self.next_segment += 1;
                    Progress::InProgress
                }
            }
        }
    }

    /// Copies a packet's payload into its place in the frame.
    ///
    /// VoSPI pixels are big-endian and the frame protocol is little-endian, so
    /// the halves are swapped on the way in.
    fn store(&self, packet: &[u8], frame: &mut [u8], number: u16) {
        let index = (self.next_segment as usize - 1) * PACKETS_PER_SEGMENT + number as usize;
        let offset = index * PAYLOAD_LEN;

        let payload = &packet[PACKET_LEN - PAYLOAD_LEN..];
        for (pixel, target) in payload
            .chunks_exact(2)
            .zip(frame[offset..offset + PAYLOAD_LEN].chunks_exact_mut(2))
        {
            target[0] = pixel[1];
            target[1] = pixel[0];
        }
    }

    /// Marks the stream desynchronised.
    fn desync(&mut self) -> Progress {
        self.state = State::Desynced;
        Progress::Desync
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Assembler, FRAME_LEN, PACKET_LEN, PACKETS_PER_SEGMENT, PAYLOAD_LEN, PacketHeader, Progress,
        SEGMENTS_PER_FRAME, crc16, packet_crc,
    };

    /// Builds a valid video packet whose payload counts up from `seed`.
    fn packet(segment: u8, number: u16, seed: u8) -> [u8; PACKET_LEN] {
        let mut packet = [0u8; PACKET_LEN];
        // The segment number only occupies the ID's top nibble in packet 20.
        let id = if number == 20 {
            (u16::from(segment) << 12) | number
        } else {
            number
        };
        packet[..2].copy_from_slice(&id.to_be_bytes());
        for (index, byte) in packet[4..].iter_mut().enumerate() {
            *byte = seed.wrapping_add(index as u8);
        }
        let crc = packet_crc(&packet);
        packet[2..4].copy_from_slice(&crc.to_be_bytes());
        packet
    }

    fn discard_packet() -> [u8; PACKET_LEN] {
        let mut packet = [0u8; PACKET_LEN];
        packet[..2].copy_from_slice(&0x0F00u16.to_be_bytes());
        packet
    }

    /// Feeds a whole segment, returning the progress of its last packet.
    fn push_segment(
        assembler: &mut Assembler,
        frame: &mut [u8],
        segment: u8,
        seed: u8,
    ) -> Progress {
        let mut progress = Progress::InProgress;
        for number in 0..PACKETS_PER_SEGMENT as u16 {
            progress = assembler.push(
                &packet(segment, number, seed.wrapping_add(number as u8)),
                frame,
            );
        }
        progress
    }

    /// Gets the assembler past its initial hunt for a frame boundary.
    fn synchronise(assembler: &mut Assembler, frame: &mut [u8]) {
        let end_of_frame = packet(SEGMENTS_PER_FRAME as u8, 20, 0);
        assert_eq!(assembler.push(&end_of_frame, frame), Progress::Discarded);
    }

    #[test]
    fn table_crc_matches_the_bitwise_definition() {
        fn bitwise(data: &[u8]) -> u16 {
            let mut crc: u16 = 0;
            for &byte in data {
                crc ^= u16::from(byte) << 8;
                for _ in 0..8 {
                    crc = if crc & 0x8000 != 0 {
                        (crc << 1) ^ 0x1021
                    } else {
                        crc << 1
                    };
                }
            }
            crc
        }

        let packet = packet(2, 33, 91);
        assert_eq!(crc16(&packet), bitwise(&packet));
        for byte in 0..=255u8 {
            assert_eq!(crc16(&[byte, !byte]), bitwise(&[byte, !byte]));
        }
    }

    #[test]
    fn crc16_matches_the_published_check_value() {
        // The XMODEM CRC of "123456789" is 0x31C3.
        assert_eq!(crc16(b"123456789"), 0x31C3);
    }

    #[test]
    fn packet_crc_ignores_the_id_nibble_and_the_crc_field() {
        let mut a = packet(4, 20, 7);
        let before = packet_crc(&a);

        // Changing the segment nibble or the stored CRC must not change it.
        a[0] |= 0xF0;
        a[2] = 0xAA;
        a[3] = 0x55;
        assert_eq!(packet_crc(&a), before);

        // Changing payload must.
        a[100] ^= 0xFF;
        assert_ne!(packet_crc(&a), before);
    }

    #[test]
    fn assembles_a_whole_frame() {
        let mut assembler = Assembler::new();
        let mut frame = [0u8; FRAME_LEN];
        synchronise(&mut assembler, &mut frame);

        for segment in 1..SEGMENTS_PER_FRAME as u8 {
            let progress = push_segment(&mut assembler, &mut frame, segment, segment);
            assert_eq!(progress, Progress::InProgress);
        }
        let progress = push_segment(&mut assembler, &mut frame, 4, 4);
        assert_eq!(progress, Progress::Frame);

        // Packet 0 of segment 1 lands at the top left, byte-swapped, and the
        // last packet of segment 4 lands at the end of the buffer.
        let first = packet(1, 0, 1);
        assert_eq!(frame[0], first[5]);
        assert_eq!(frame[1], first[4]);

        let last = packet(4, 59, 4u8.wrapping_add(59));
        assert_eq!(frame[FRAME_LEN - PAYLOAD_LEN], last[5]);
    }

    #[test]
    fn discard_packets_pass_through_without_disturbing_the_stream() {
        let mut assembler = Assembler::new();
        let mut frame = [0u8; FRAME_LEN];
        synchronise(&mut assembler, &mut frame);

        assert_eq!(
            assembler.push(&discard_packet(), &mut frame),
            Progress::Discarded
        );
        assert_eq!(
            assembler.push(&packet(1, 0, 0), &mut frame),
            Progress::InProgress
        );
        assert_eq!(
            assembler.push(&discard_packet(), &mut frame),
            Progress::Discarded
        );
        assert_eq!(
            assembler.push(&packet(1, 1, 0), &mut frame),
            Progress::InProgress
        );
    }

    #[test]
    fn a_bad_crc_desyncs() {
        let mut assembler = Assembler::new();
        let mut frame = [0u8; FRAME_LEN];
        synchronise(&mut assembler, &mut frame);

        let mut corrupt = packet(1, 0, 0);
        corrupt[10] ^= 0xFF;
        assert_eq!(assembler.push(&corrupt, &mut frame), Progress::Desync);

        // It stays desynced until the caller resynchronises.
        assert_eq!(
            assembler.push(&packet(1, 0, 0), &mut frame),
            Progress::Desync
        );
        assembler.resync();
        synchronise(&mut assembler, &mut frame);
        assert_eq!(
            assembler.push(&packet(1, 0, 0), &mut frame),
            Progress::InProgress
        );
    }

    #[test]
    fn a_skipped_packet_number_desyncs() {
        let mut assembler = Assembler::new();
        let mut frame = [0u8; FRAME_LEN];
        synchronise(&mut assembler, &mut frame);

        assembler.push(&packet(1, 0, 0), &mut frame);
        assert_eq!(
            assembler.push(&packet(1, 2, 0), &mut frame),
            Progress::Desync
        );
    }

    #[test]
    fn the_wrong_segment_number_desyncs() {
        let mut assembler = Assembler::new();
        let mut frame = [0u8; FRAME_LEN];
        synchronise(&mut assembler, &mut frame);

        for number in 0..20 {
            assembler.push(&packet(1, number, 0), &mut frame);
        }
        // Segment 3 where segment 1 was expected means packets were lost.
        assert_eq!(
            assembler.push(&packet(3, 20, 0), &mut frame),
            Progress::Desync
        );
    }

    #[test]
    fn an_invalid_segment_is_dropped_and_retried() {
        let mut assembler = Assembler::new();
        let mut frame = [0u8; FRAME_LEN];
        synchronise(&mut assembler, &mut frame);

        for number in 0..20 {
            assembler.push(&packet(1, number, 0), &mut frame);
        }
        // Segment 0 marks the segment invalid.
        assert_eq!(
            assembler.push(&packet(0, 20, 0), &mut frame),
            Progress::SegmentDropped
        );

        // The rest of the doomed segment is ignored, and the camera's retry
        // picks up from packet 0 of the same segment.
        for number in 21..PACKETS_PER_SEGMENT as u16 {
            assert_eq!(
                assembler.push(&packet(1, number, 0), &mut frame),
                Progress::Discarded
            );
        }
        assert_eq!(
            assembler.push(&packet(1, 0, 0), &mut frame),
            Progress::InProgress
        );
    }

    #[test]
    fn synchronisation_waits_for_the_end_of_a_frame() {
        let mut assembler = Assembler::new();
        let mut frame = [0u8; FRAME_LEN];

        // Mid-frame packets are ignored until segment 4's packet 20 goes by.
        for number in 0..PACKETS_PER_SEGMENT as u16 {
            assert_eq!(
                assembler.push(&packet(2, number, 0), &mut frame),
                Progress::Discarded
            );
        }
        assert_eq!(
            assembler.push(&packet(4, 20, 0), &mut frame),
            Progress::Discarded
        );
        assert_eq!(
            assembler.push(&packet(1, 0, 0), &mut frame),
            Progress::InProgress
        );
    }

    #[test]
    fn parses_id_and_crc() {
        let mut raw = [0u8; PACKET_LEN];
        raw[..2].copy_from_slice(&0x1014u16.to_be_bytes());
        raw[2..4].copy_from_slice(&0xABCDu16.to_be_bytes());
        let header = PacketHeader::parse(&raw).unwrap();
        assert_eq!(header.id, 0x1014);
        assert_eq!(header.crc, 0xABCD);
    }

    #[test]
    fn detects_discard_packets() {
        let header = |id: u16| PacketHeader { id, crc: 0 };
        assert!(header(0x0F00).is_discard());
        assert!(header(0x3FFF).is_discard());
        assert!(!header(0x0000).is_discard());
        assert!(!header(0x0E3B).is_discard());
    }

    #[test]
    fn extracts_packet_number_and_segment() {
        let header = |id: u16| PacketHeader { id, crc: 0 };
        assert_eq!(header(0x0000).packet_number(), 0);
        assert_eq!(header(0x103B).packet_number(), 0x3B);
        assert_eq!(header(0x3014).segment(), 3);
        assert_eq!(header(0x0014).segment(), 0);
    }

    #[test]
    fn rejects_short_buffers() {
        assert_eq!(PacketHeader::parse(&[0x00, 0x14, 0xAB]), None);
    }
}
