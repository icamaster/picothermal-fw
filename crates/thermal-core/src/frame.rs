//! Frame geometry and a synthetic frame source.
//!
//! [`TestPattern`] stands in for the Lepton until the VoSPI driver lands, so
//! the USB path can be brought up and tested with no camera attached.

use crate::protocol::PixelFormat;

/// Frame width in pixels for the Lepton 3.5.
pub const WIDTH: usize = 160;

/// Frame height in pixels for the Lepton 3.5.
pub const HEIGHT: usize = 120;

/// Pixels in one frame.
pub const PIXELS: usize = WIDTH * HEIGHT;

/// Payload length in bytes of a frame in `format`.
pub const fn payload_len(format: PixelFormat) -> usize {
    PIXELS * format.bytes_per_pixel()
}

/// Synthetic frame source: a diagonal ramp that drifts by one step per frame.
///
/// Pixels are computed from their index rather than rendered into a buffer, so
/// a caller can fill packet-sized slices at any offset without holding a whole
/// frame in RAM.
pub struct TestPattern {
    phase: u8,
}

impl Default for TestPattern {
    fn default() -> Self {
        Self::new()
    }
}

impl TestPattern {
    /// Creates a pattern at its starting phase.
    pub const fn new() -> Self {
        Self { phase: 0 }
    }

    /// Advances the pattern to the next frame.
    pub fn advance(&mut self) {
        self.phase = self.phase.wrapping_add(2);
    }

    /// Fills `buf` with payload bytes starting `offset` bytes into the frame.
    ///
    /// Bytes past the end of the payload are left untouched.
    pub fn fill(&self, format: PixelFormat, offset: usize, buf: &mut [u8]) {
        let len = payload_len(format);
        for (i, byte) in buf.iter_mut().enumerate() {
            let pos = offset + i;
            if pos >= len {
                break;
            }
            *byte = match format {
                PixelFormat::Agc8 => self.gray(pos),
                // Y16 is little-endian, so even byte positions carry the low
                // half of the pixel at pos / 2 and odd positions the high half.
                PixelFormat::Y16 => {
                    let pixel = self.centikelvin(pos / 2).to_le_bytes();
                    pixel[pos & 1]
                }
            };
        }
    }

    /// 8-bit value of the pixel at `index`.
    fn gray(&self, index: usize) -> u8 {
        let x = index % WIDTH;
        let y = index / WIDTH;
        (x as u8).wrapping_add(y as u8).wrapping_add(self.phase)
    }

    /// Radiometric value of the pixel at `index`, in centikelvin.
    ///
    /// Spans roughly 295 K to 305 K, so a host that maps TLinear values to
    /// temperatures shows a plausible scene rather than nonsense.
    fn centikelvin(&self, index: usize) -> u16 {
        29500 + u16::from(self.gray(index)) * 4
    }
}

#[cfg(test)]
mod tests {
    use super::{HEIGHT, PIXELS, TestPattern, WIDTH, payload_len};
    use crate::protocol::{FrameHeader, PixelFormat};

    #[test]
    fn payload_len_agrees_with_frame_header() {
        for format in [PixelFormat::Y16, PixelFormat::Agc8] {
            let header = FrameHeader {
                seq: 0,
                width: WIDTH as u16,
                height: HEIGHT as u16,
                format,
            };
            assert_eq!(header.payload_len(), payload_len(format));
        }
    }

    #[test]
    fn chunked_fill_matches_contiguous_fill() {
        let pattern = TestPattern::new();
        for format in [PixelFormat::Y16, PixelFormat::Agc8] {
            let len = payload_len(format);
            let mut whole = [0u8; PIXELS * 2];
            pattern.fill(format, 0, &mut whole[..len]);

            // 48 then 64 is what the USB path actually does: the first packet
            // shares its 64 bytes with the frame header.
            let mut chunked = [0u8; PIXELS * 2];
            let mut offset = 0;
            for size in core::iter::once(48).chain(core::iter::repeat(64)) {
                if offset >= len {
                    break;
                }
                let end = (offset + size).min(len);
                pattern.fill(format, offset, &mut chunked[offset..end]);
                offset = end;
            }
            assert_eq!(whole[..len], chunked[..len]);
        }
    }

    #[test]
    fn fill_stops_at_the_end_of_the_payload() {
        let pattern = TestPattern::new();
        let len = payload_len(PixelFormat::Agc8);
        let mut buf = [0xAAu8; 8];
        pattern.fill(PixelFormat::Agc8, len - 4, &mut buf);
        assert_eq!(buf[4..], [0xAA; 4]);
    }

    #[test]
    fn advance_changes_the_image() {
        let mut pattern = TestPattern::new();
        let mut first = [0u8; 64];
        pattern.fill(PixelFormat::Agc8, 0, &mut first);

        pattern.advance();
        let mut second = [0u8; 64];
        pattern.fill(PixelFormat::Agc8, 0, &mut second);

        assert_ne!(first, second);
    }

    #[test]
    fn radiometric_values_are_plausible_temperatures() {
        let pattern = TestPattern::new();
        let mut buf = [0u8; 2];
        for index in [0, PIXELS / 2, PIXELS - 1] {
            pattern.fill(PixelFormat::Y16, index * 2, &mut buf);
            let centikelvin = u16::from_le_bytes(buf);
            assert!((29500..=30520).contains(&centikelvin));
        }
    }
}
