//! Lepton bring-up and VoSPI capture.
//!
//! Owns the camera's power lines, its CCI bus and the VoSPI SPI port, and
//! hands finished frames to the USB side through a zero-copy channel. While
//! capturing, the CCI bus applies settings the host sends through [`CONTROL`]
//! and watches for flat-field corrections, so frames showing the shutter can
//! be flagged to the host.

use core::sync::atomic::{AtomicBool, Ordering};

use defmt::{debug, info, warn};
use embassy_futures::select::{Either, select};
use embassy_rp::gpio::Output;
use embassy_rp::i2c::{self, Async, I2c};
use embassy_rp::peripherals::{I2C1, SPI0};
use embassy_rp::spi::{self, Spi};
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use embassy_sync::channel::Channel;
use embassy_sync::zerocopy_channel::Sender;
use embassy_time::{Delay, Duration, Instant, Timer};
use static_cell::ConstStaticCell;
use thermal_core::lepton::cci::Cci;
use thermal_core::lepton::power::Power;
use thermal_core::lepton::vospi::{Assembler, FRAME_LEN, PACKET_LEN, Progress, RESYNC_DELAY_MS};
use thermal_core::protocol::{Command, GainMode};

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

/// Camera commands from the host, handed over by the USB side because the CCI
/// bus belongs to this task.
pub static CONTROL: Channel<CriticalSectionRawMutex, Command, 4> = Channel::new();

/// Settings the host has chosen, kept so they survive the camera being powered
/// up again after a failure.
struct Settings {
    gain: GainMode,
    emissivity: u16,
}

impl Settings {
    const DEFAULT: Self = Self {
        gain: GainMode::High,
        emissivity: Command::EMISSIVITY_ONE,
    };
}

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
    let mut settings = Settings::DEFAULT;
    loop {
        match bring_up(&mut camera, &settings).await {
            // SPI and I2C are separate buses, so watching FFC runs alongside
            // capture instead of pausing the packet reads, which would desync.
            // The watcher never finishes; capture returning ends both.
            Ok(()) => {
                select(
                    capture(&mut camera.spi, &mut camera.cs, &mut frames, scratch),
                    control(&mut camera.cci, &mut settings),
                )
                .await;
            }
            Err(()) => Timer::after_millis(RETRY_MS).await,
        }
    }
}

/// Powers the camera, waits for it to boot and configures it for radiometry.
async fn bring_up(camera: &mut Camera<'_>, settings: &Settings) -> Result<(), ()> {
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

    // A camera that was powered up again starts from its defaults; put back
    // whatever the host had chosen.
    if settings.gain != Settings::DEFAULT.gain {
        apply(&mut camera.cci, Command::SetGainMode(settings.gain)).await;
    }
    if settings.emissivity != Settings::DEFAULT.emissivity {
        apply(&mut camera.cci, Command::SetEmissivity(settings.emissivity)).await;
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

/// How long capture may go without a complete frame before the camera is
/// powered down and brought up again.
///
/// The camera can stop producing valid video and never recover through VoSPI
/// resynchronisation alone; on hardware this followed a commanded FFC. A power
/// cycle is FLIR's recovery for that. The limit sits well above the longest
/// normal gaps: ~2 s without frames during an FFC, and up to ~4 s of desyncs
/// right after boot.
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);

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
    let mut last_frame = Instant::now();
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
                Progress::SegmentDropped => stats.invalid_segments += 1,
                Progress::Discarded | Progress::InProgress => {}
            }

            // Checked per packet rather than per frame, so both still happen
            // when frames have stopped, which is when they matter.
            if last_report.elapsed() >= STATS_INTERVAL {
                stats.log();
                stats = Stats::default();
                last_report = Instant::now();
            }
            if last_frame.elapsed() >= FRAME_TIMEOUT {
                warn!("camera: no frame for 10 s, restarting the camera");
                stats.log();
                cs.set_high();
                return;
            }
        }
        last_frame = Instant::now();

        let flagged = ffc_at_start || FFC_ACTIVE.load(Ordering::Relaxed);

        match slot {
            Some(frame) => {
                frame.seq = seq;
                frame.ffc = flagged;
                frames.send_done();
                stats.sent += 1;
            }
            None => stats.dropped += 1,
        }
        seq = seq.wrapping_add(1);
    }
}

/// Applies host commands and polls the camera's FFC status, forever.
///
/// Both use the CCI bus, so they share one loop: a command is applied as soon
/// as it arrives, and between commands the FFC status is published in
/// [`FFC_ACTIVE`].
async fn control(cci: &mut Cci<I2c<'_, I2C1, Async>, Delay>, settings: &mut Settings) {
    loop {
        match select(CONTROL.receive(), Timer::after_millis(FFC_POLL_MS)).await {
            Either::First(command) => {
                match command {
                    Command::SetGainMode(mode) => settings.gain = mode,
                    Command::SetEmissivity(scaled) => settings.emissivity = scaled,
                    Command::RunFfc | Command::SetPixelFormat(_) => {}
                }
                apply(cci, command).await;
            }
            Either::Second(()) => poll_ffc(cci).await,
        }
    }
}

/// Sends one host command to the camera, logging the outcome.
async fn apply(cci: &mut Cci<I2c<'_, I2C1, Async>, Delay>, command: Command) {
    let result = match command {
        Command::SetGainMode(mode) => cci.set_gain_mode(mode).await,
        Command::SetEmissivity(scaled) => cci.set_emissivity(scaled).await,
        Command::RunFfc => {
            // Flag frames from now: the run command can hold the bus for
            // part of the correction, during which no status poll happens.
            FFC_ACTIVE.store(true, Ordering::Relaxed);
            info!("camera: FFC requested");
            cci.run_ffc().await
        }
        // Handled on the USB side; it never reaches the camera.
        Command::SetPixelFormat(_) => return,
    };
    match result {
        Ok(()) => info!("camera: applied {}", defmt::Debug2Format(&command)),
        Err(error) => warn!(
            "camera: {} failed: {}",
            defmt::Debug2Format(&command),
            defmt::Debug2Format(&error)
        ),
    }
}

/// Reads the FFC status once and publishes it in [`FFC_ACTIVE`].
async fn poll_ffc(cci: &mut Cci<I2c<'_, I2C1, Async>, Delay>) {
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
        // Keep the last known state: a missed poll should not flash the host's
        // image back to a shutter frame or hide a real correction.
        Err(error) => debug!(
            "camera: FFC status read failed: {}",
            defmt::Debug2Format(&error)
        ),
    }
}

/// Counters for one statistics interval.
#[derive(Default)]
struct Stats {
    sent: u32,
    dropped: u32,
    desyncs: u32,
    invalid_segments: u32,
}

impl Stats {
    fn log(&self) {
        info!(
            "camera: {=u32} frames sent, {=u32} dropped for lack of a buffer, {=u32} desyncs, {=u32} invalid segments",
            self.sent, self.dropped, self.desyncs, self.invalid_segments
        );
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
