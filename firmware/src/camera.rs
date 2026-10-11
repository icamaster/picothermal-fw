//! Lepton bring-up and VoSPI capture.
//!
//! Owns the camera's power lines, its CCI bus and the VoSPI SPI port, and
//! hands finished frames to the USB side through a zero-copy channel.

use defmt::{info, warn};
use embassy_rp::gpio::Output;
use embassy_rp::i2c::{self, Async, I2c};
use embassy_rp::peripherals::{I2C1, SPI0};
use embassy_rp::spi::{self, Spi};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::zerocopy_channel::Sender;
use embassy_time::{Delay, Timer};
use thermal_core::lepton::cci::Cci;
use thermal_core::lepton::power::Power;
use thermal_core::lepton::vospi::{Assembler, PACKET_LEN, Progress, RESYNC_DELAY_MS};

use crate::Frame;

/// VoSPI clock. The Lepton tolerates up to 20 MHz; a little under that leaves
/// margin on a hand-built board without starving the packet rate.
pub const SPI_HZ: u32 = 16_000_000;

/// How long to wait before retrying a camera that failed to come up.
const RETRY_MS: u64 = 1000;

/// The camera's control lines and buses.
pub struct Camera<'d> {
    /// RESET_L and PWR_DWN_L.
    pub power: Power<Output<'d>, Output<'d>>,
    /// CCI, the I2C control interface.
    pub cci: Cci<I2c<'d, I2C1, Async>, Delay>,
    /// VoSPI, receive only: the Lepton never reads MOSI.
    pub spi: Spi<'d, SPI0, spi::Async>,
    /// VoSPI chip select, driven by hand so it can be dropped to resynchronise.
    pub cs: Output<'d>,
}

/// Runs the camera forever, sending every assembled frame downstream.
///
/// Bring-up failures are logged and retried rather than fatal: a board with no
/// camera attached should still enumerate and stream.
pub async fn run(mut camera: Camera<'_>, mut frames: Sender<'_, NoopRawMutex, Frame>) -> ! {
    loop {
        match bring_up(&mut camera).await {
            Ok(()) => capture(&mut camera, &mut frames).await,
            Err(()) => Timer::after_millis(RETRY_MS).await,
        }
    }
}

/// Powers the camera, waits for it to boot and configures it for radiometry.
async fn bring_up(camera: &mut Camera<'_>) -> Result<(), ()> {
    camera.cs.set_high();

    if camera.power.up(&mut Delay).await.is_err() {
        warn!("camera: driving the power lines failed");
        return Err(());
    }

    if let Err(error) = camera.cci.wait_until_ready().await {
        warn!(
            "camera: never became ready: {}",
            defmt::Debug2Format(&error)
        );
        return Err(());
    }

    if let Err(error) = camera.cci.ping().await {
        warn!("camera: ping failed: {}", defmt::Debug2Format(&error));
        return Err(());
    }

    match camera.cci.flir_serial_number().await {
        Ok(serial) => info!("camera: up, serial {=u64:#x}", serial),
        Err(error) => warn!("camera: no serial: {}", defmt::Debug2Format(&error)),
    }

    // Radiometric output, so pixels are absolute centikelvin. AGC would
    // overwrite them with display values.
    if let Err(error) = camera.cci.set_agc(false).await {
        warn!(
            "camera: disabling AGC failed: {}",
            defmt::Debug2Format(&error)
        );
        return Err(());
    }
    if let Err(error) = camera.cci.set_tlinear(true).await {
        warn!(
            "camera: enabling TLinear failed: {}",
            defmt::Debug2Format(&error)
        );
        return Err(());
    }
    if let Err(error) = camera.cci.enable_vsync().await {
        warn!(
            "camera: enabling VSYNC failed: {}",
            defmt::Debug2Format(&error)
        );
        return Err(());
    }

    Ok(())
}

/// Reads packets until the stream cannot be recovered, filling frames as it
/// goes. Returns so the caller can bring the camera up again.
///
/// Chip select stays low throughout: the camera keeps producing packets
/// regardless, so pausing the reads is what causes a desync in the first place.
async fn capture(camera: &mut Camera<'_>, frames: &mut Sender<'_, NoopRawMutex, Frame>) {
    let mut assembler = Assembler::new();
    let mut packet = [0u8; PACKET_LEN];
    let mut seq: u32 = 0;

    camera.cs.set_low();

    loop {
        // The assembler writes straight into the channel's buffer, so
        // publishing a frame costs no copy.
        let frame = frames.send().await;
        let mut complete = false;

        while !complete {
            if camera.spi.read(&mut packet).await.is_err() {
                warn!("camera: SPI read failed");
                camera.cs.set_high();
                return;
            }

            match assembler.push(&packet, &mut frame.pixels) {
                Progress::Frame => {
                    frame.seq = seq;
                    complete = true;
                }
                Progress::Desync => {
                    warn!("camera: VoSPI desync, resynchronising");
                    camera.cs.set_high();
                    Timer::after_millis(u64::from(RESYNC_DELAY_MS)).await;
                    assembler.resync();
                    camera.cs.set_low();
                }
                Progress::SegmentDropped => info!("camera: segment dropped"),
                Progress::Discarded | Progress::InProgress => {}
            }
        }

        seq = seq.wrapping_add(1);
        frames.send_done();
    }
}

/// Default configuration for the VoSPI port.
///
/// The Lepton clocks data out on the falling edge and expects it sampled on the
/// rising edge, which is SPI mode 3.
pub fn spi_config() -> spi::Config {
    let mut config = spi::Config::default();
    config.frequency = SPI_HZ;
    config.polarity = spi::Polarity::IdleHigh;
    config.phase = spi::Phase::CaptureOnSecondTransition;
    config
}

/// Default configuration for the CCI bus. The Lepton's I2C runs at 100 kHz.
pub fn i2c_config() -> i2c::Config {
    let mut config = i2c::Config::default();
    config.frequency = 100_000;
    config
}
