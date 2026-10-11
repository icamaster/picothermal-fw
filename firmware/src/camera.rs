//! Lepton bring-up and VoSPI capture.
//!
//! Owns the camera's power lines, its CCI bus and the VoSPI SPI port, and
//! hands finished frames to the USB side through a zero-copy channel. While
//! capturing, the CCI bus is used only to watch for flat-field corrections, so
//! frames showing the shutter can be flagged to the host.

use core::sync::atomic::{AtomicBool, Ordering};

use defmt::{debug, info, warn};
use embassy_futures::select::select;
use embassy_rp::gpio::Output;
use embassy_rp::i2c::{self, Async, I2c};
use embassy_rp::peripherals::{I2C1, SPI0};
use embassy_rp::spi::{self, Spi};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::zerocopy_channel::Sender;
use embassy_time::{Delay, Duration, Instant, Timer};
use static_cell::ConstStaticCell;
use thermal_core::lepton::cci::Cci;
use thermal_core::lepton::power::Power;
use thermal_core::lepton::vospi::{Assembler, FRAME_LEN, PACKET_LEN, Progress, RESYNC_DELAY_MS};

use crate::Frame;

/// VoSPI clock. The Lepton tolerates up to 20 MHz; a little under that leaves
/// margin on a hand-built board without starving the packet rate.
pub const SPI_HZ: u32 = 16_000_000;

/// How long to wait before retrying a camera that failed to come up.
const RETRY_MS: u64 = 1000;

/// How often the FFC status is polled. A correction keeps the shutter closed
/// for over a second, so this catches it within half a frame.
const FFC_POLL_MS: u64 = 50;

/// Whether the camera is in a flat-field correction. Written by the FFC
/// watcher, read by capture as it finishes each frame. Only load and store are
/// used, which thumbv6m supports without compare-and-swap.
static FFC_ACTIVE: AtomicBool = AtomicBool::new(false);

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
    let scratch = SCRATCH.take();
    loop {
        match bring_up(&mut camera).await {
            // SPI and I2C are separate buses, so watching FFC runs alongside
            // capture instead of pausing the packet reads, which would desync.
            // The watcher never finishes; capture returning ends both.
            Ok(()) => {
                select(
                    capture(&mut camera.spi, &mut camera.cs, &mut frames, scratch),
                    watch_ffc(&mut camera.cci),
                )
                .await;
            }
            Err(()) => Timer::after_millis(RETRY_MS).await,
        }
    }
}

/// Powers the camera, waits for it to boot and configures it for radiometry.
async fn bring_up(camera: &mut Camera<'_>) -> Result<(), ()> {
    camera.cs.set_high();
    FFC_ACTIVE.store(false, Ordering::Relaxed);

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

/// Where frames are assembled when both channel buffers are taken.
///
/// The camera has to be read continuously or it desyncs, so a host that is slow
/// or not reading at all must cost dropped frames rather than a stalled
/// capture loop. This buffer absorbs those frames.
static SCRATCH: ConstStaticCell<[u8; FRAME_LEN]> = ConstStaticCell::new([0; FRAME_LEN]);

/// How often capture statistics are logged.
const STATS_INTERVAL: Duration = Duration::from_secs(5);

/// Reads packets until the stream cannot be recovered, filling frames as it
/// goes. Returns so the caller can bring the camera up again.
///
/// Chip select stays low throughout: the camera keeps producing packets
/// regardless, so pausing the reads is what causes a desync in the first place.
async fn capture(
    spi: &mut Spi<'_, SPI0, spi::Async>,
    cs: &mut Output<'_>,
    frames: &mut Sender<'_, NoopRawMutex, Frame>,
    scratch: &mut [u8; FRAME_LEN],
) {
    let mut assembler = Assembler::new();
    let mut packet = [0u8; PACKET_LEN];
    let mut seq: u32 = 0;
    let mut stats = Stats::default();
    let mut last_report = Instant::now();

    cs.set_low();

    loop {
        // Never wait for a buffer here: if USB still holds both, this frame is
        // assembled into scratch and thrown away.
        let mut slot = frames.try_send();
        // A frame that overlaps a correction at either end shows the shutter
        // for part of its height, so both ends count.
        let ffc_at_start = FFC_ACTIVE.load(Ordering::Relaxed);
        let pixels: &mut [u8] = match &mut slot {
            Some(frame) => &mut frame.pixels,
            None => &mut scratch[..],
        };

        loop {
            if spi.read(&mut packet).await.is_err() {
                warn!("camera: SPI read failed");
                cs.set_high();
                return;
            }

            match assembler.push(&packet, pixels) {
                Progress::Frame => break,
                Progress::Desync => {
                    stats.desyncs += 1;
                    debug!("camera: VoSPI desync, resynchronising");
                    cs.set_high();
                    Timer::after_millis(u64::from(RESYNC_DELAY_MS)).await;
                    assembler.resync();
                    cs.set_low();
                }
                // The Lepton 3.5 emits segments faster than it exports unique
                // frames and marks the surplus invalid, so this is routine.
                Progress::SegmentDropped | Progress::Discarded | Progress::InProgress => {}
            }
        }

        match slot {
            Some(frame) => {
                frame.seq = seq;
                frame.ffc = ffc_at_start || FFC_ACTIVE.load(Ordering::Relaxed);
                frames.send_done();
                stats.sent += 1;
            }
            None => stats.dropped += 1,
        }
        seq = seq.wrapping_add(1);

        if last_report.elapsed() >= STATS_INTERVAL {
            info!(
                "camera: {=u32} frames sent, {=u32} dropped for lack of a buffer, {=u32} desyncs",
                stats.sent, stats.dropped, stats.desyncs
            );
            stats = Stats::default();
            last_report = Instant::now();
        }
    }
}

/// Polls the camera's FFC status forever, publishing it in [`FFC_ACTIVE`].
async fn watch_ffc(cci: &mut Cci<I2c<'_, I2C1, Async>, Delay>) {
    loop {
        match cci.ffc_status().await {
            Ok(status) => {
                let active = status.in_progress();
                if active != FFC_ACTIVE.load(Ordering::Relaxed) {
                    info!(
                        "camera: FFC {}",
                        if active { "started" } else { "finished" }
                    );
                }
                FFC_ACTIVE.store(active, Ordering::Relaxed);
            }
            // Keep the last known state: a missed poll should not flash the
            // host's image back to a shutter frame or hide a real correction.
            Err(error) => debug!(
                "camera: FFC status read failed: {}",
                defmt::Debug2Format(&error)
            ),
        }
        Timer::after_millis(FFC_POLL_MS).await;
    }
}

/// Counters for one statistics interval.
#[derive(Default)]
struct Stats {
    sent: u32,
    dropped: u32,
    desyncs: u32,
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
