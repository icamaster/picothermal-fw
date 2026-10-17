//! Lepton bring-up and VoSPI capture, split across the RP2040's two cores.
//!
//! Core 0 owns the camera's power lines and its CCI bus: it brings the camera
//! up, applies settings the host sends through [`CONTROL`], and watches for
//! flat-field corrections so frames showing the shutter can be flagged.
//!
//! Core 1 does nothing but read VoSPI packets and assemble frames, on an
//! executor of its own that also owns the DMA interrupt. The camera must be
//! read promptly or it desyncs, and on a shared core USB traffic delayed the
//! reads enough to break capture.
//!
//! Reads go through DMA rather than CPU polling: the polling read waits for
//! each byte before clocking the next, which measured 130 us per packet
//! against 84 us for a continuous transfer, too little margin once the flash
//! cache was under pressure from core 0. DMA keeps the clock running
//! whatever the CPU is doing.
//!
//! The cores coordinate through two flags: [`CAMERA_READY`], set by core 0
//! once the camera is configured, and [`RESTART_REQUESTED`], set by core 1 when
//! frames stop and the camera needs powering up again.

use core::sync::atomic::{AtomicBool, Ordering};

use defmt::{debug, info, warn};
use embassy_futures::select::{Either, select};
use embassy_rp::gpio::Output;
use embassy_rp::i2c::{self, Async, I2c};
use embassy_rp::peripherals::{I2C1, SPI0};
use embassy_rp::spi::{self, Spi};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::zerocopy_channel::Sender;
use embassy_time::{Delay, Duration, Instant, Timer};
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

/// Whether the camera is in a flat-field correction. Written by core 0's FFC
/// polling, read by core 1 as it finishes each frame. Only load and store are
/// used, which thumbv6m supports without compare-and-swap, from either core.
static FFC_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Set by core 0 once the camera is powered and configured; core 1 only reads
/// packets while it is set.
static CAMERA_READY: AtomicBool = AtomicBool::new(false);

/// Set by core 1 when frames have stopped; core 0 answers by powering the
/// camera up again, and clears it once the camera is back.
static RESTART_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Camera commands from the host, handed over by the USB side because the CCI
/// bus belongs to the camera control loop.
pub static CONTROL: Channel<CriticalSectionRawMutex, Command, 4> = Channel::new();

/// Settings the host has chosen, kept so they survive the camera being powered
/// up again.
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

/// The camera's control side, run on core 0.
pub struct CameraControl<'d> {
    /// RESET_L and PWR_DWN_L.
    pub power: Power<Output<'d>, Output<'d>>,
    /// CCI, the I2C control interface.
    pub cci: Cci<I2c<'d, I2C1, Async>, Delay>,
}

/// The camera's video side, run on core 1.
pub struct CameraCapture {
    /// VoSPI, receive only: the Lepton never reads MOSI.
    pub spi: Spi<'static, SPI0, spi::Async>,
    /// VoSPI chip select, driven by hand so it can be dropped to resynchronise.
    pub cs: Output<'static>,
    /// Where finished frames go; the USB side receives them.
    pub frames: Sender<'static, CriticalSectionRawMutex, Frame>,
    /// Where frames are assembled when both channel buffers are taken.
    ///
    /// The camera has to be read continuously, so a host that is slow or not
    /// reading at all must cost dropped frames rather than a stalled capture
    /// loop. This buffer absorbs those frames.
    pub scratch: &'static mut [u8; FRAME_LEN],
}

/// Keeps the camera powered, configured and responsive to the host, forever.
/// Runs on core 0.
///
/// Bring-up failures are logged and retried rather than fatal: a board with no
/// camera attached should still enumerate and stream.
pub async fn run_control(mut camera: CameraControl<'_>) -> ! {
    let mut settings = Settings::DEFAULT;
    loop {
        // Core 1 checks this between packets, so it has released chip select
        // well within the settling time the power sequence starts with.
        CAMERA_READY.store(false, Ordering::Release);
        FFC_ACTIVE.store(false, Ordering::Relaxed);

        match bring_up(&mut camera, &settings).await {
            Ok(()) => {
                RESTART_REQUESTED.store(false, Ordering::Release);
                CAMERA_READY.store(true, Ordering::Release);
                control(&mut camera.cci, &mut settings).await;
            }
            Err(()) => Timer::after_millis(RETRY_MS).await,
        }
    }
}

/// Powers the camera, waits for it to boot and configures it for radiometry.
async fn bring_up(camera: &mut CameraControl<'_>, settings: &Settings) -> Result<(), ()> {
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

/// Applies host commands and polls the camera's FFC status until core 1 asks
/// for the camera to be restarted.
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
                    Command::RunFfc | Command::SetPixelFormat(_) | Command::RebootToBootloader => {}
                }
                apply(cci, command).await;
            }
            Either::Second(()) => {
                if RESTART_REQUESTED.load(Ordering::Acquire) {
                    return;
                }
                poll_ffc(cci).await;
            }
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
            cci.run_ffc().await
        }
        // Handled on the USB side; they never reach the camera.
        Command::SetPixelFormat(_) | Command::RebootToBootloader => return,
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

/// Captures frames for as long as the camera is up, forever. The only task on
/// core 1's executor, so nothing else competes for the core.
#[embassy_executor::task]
pub async fn run_capture(mut camera: CameraCapture) -> ! {
    loop {
        camera.cs.set_high();
        while !CAMERA_READY.load(Ordering::Acquire) || RESTART_REQUESTED.load(Ordering::Acquire) {
            Timer::after_millis(1).await;
        }
        capture(&mut camera).await;
    }
}

/// Reads packets until the camera is taken down or stops producing frames,
/// filling frames as it goes.
///
/// Chip select stays low throughout: the camera keeps producing packets
/// regardless, so pausing the reads is what causes a desync in the first place.
async fn capture(camera: &mut CameraCapture) {
    let mut assembler = Assembler::new();
    let mut packet = [0u8; PACKET_LEN];
    let mut seq: u32 = 0;
    let mut stats = Stats::default();
    let mut last_report = Instant::now();
    let mut last_frame = Instant::now();
    camera.cs.set_low();

    loop {
        // Never wait for a buffer here: if USB still holds both, this frame is
        // assembled into scratch and thrown away.
        let mut slot = camera.frames.try_send();
        // A frame that overlaps a correction at either end shows the shutter
        // for part of its height, so both ends count.
        let ffc_at_start = FFC_ACTIVE.load(Ordering::Relaxed);
        let pixels: &mut [u8] = match &mut slot {
            Some(frame) => &mut frame.pixels,
            None => &mut camera.scratch[..],
        };

        loop {
            let started = Instant::now();
            if camera.spi.read(&mut packet).await.is_err() {
                warn!("camera: SPI read failed");
                RESTART_REQUESTED.store(true, Ordering::Release);
                return;
            }
            stats.record_read(started.elapsed());

            match assembler.push(&packet, pixels) {
                Progress::Frame => break,
                Progress::Desync => {
                    stats.desyncs += 1;
                    debug!("camera: VoSPI desync, resynchronising");
                    camera.cs.set_high();
                    Timer::after_millis(u64::from(RESYNC_DELAY_MS)).await;
                    assembler.resync();
                    camera.cs.set_low();
                }
                // The Lepton 3.5 emits segments faster than it exports unique
                // frames and marks the surplus invalid, so this is routine.
                Progress::SegmentDropped => stats.invalid_segments += 1,
                Progress::Discarded | Progress::InProgress => {}
            }

            // Checked per packet rather than per frame, so all of these still
            // happen when frames have stopped, which is when they matter.
            if last_report.elapsed() >= STATS_INTERVAL {
                stats.log();
                stats = Stats::default();
                last_report = Instant::now();
            }
            if last_frame.elapsed() >= FRAME_TIMEOUT {
                warn!("camera: no frame for 10 s, restarting the camera");
                stats.log();
                RESTART_REQUESTED.store(true, Ordering::Release);
                return;
            }
            if !CAMERA_READY.load(Ordering::Acquire) {
                return;
            }
        }
        last_frame = Instant::now();

        let flagged = ffc_at_start || FFC_ACTIVE.load(Ordering::Relaxed);

        match slot {
            Some(frame) => {
                frame.seq = seq;
                frame.ffc = flagged;
                camera.frames.send_done();
                stats.sent += 1;
            }
            None => stats.dropped += 1,
        }
        seq = seq.wrapping_add(1);
    }
}

/// Counters for one statistics interval.
#[derive(Default)]
struct Stats {
    sent: u32,
    dropped: u32,
    desyncs: u32,
    invalid_segments: u32,
    reads: u32,
    read_us_total: u64,
    read_us_max: u64,
}

impl Stats {
    /// Records how long one packet read took. The camera needs a packet read
    /// every ~157 us on average, so slow reads are the first sign of capture
    /// falling behind.
    fn record_read(&mut self, took: Duration) {
        let us = took.as_micros();
        self.reads += 1;
        self.read_us_total += us;
        self.read_us_max = self.read_us_max.max(us);
    }

    fn log(&self) {
        info!(
            "camera: {=u32} frames sent, {=u32} dropped for lack of a buffer, {=u32} desyncs, {=u32} invalid segments, packet read avg {=u64} us max {=u64} us",
            self.sent,
            self.dropped,
            self.desyncs,
            self.invalid_segments,
            self.read_us_total / u64::from(self.reads.max(1)),
            self.read_us_max
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
