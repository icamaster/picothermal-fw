//! RP2040 thermal camera firmware.
//!
//! Core 1 captures VoSPI frames from the Lepton on an executor of its own;
//! core 0 runs everything else (USB, the camera's control bus, the LED). Frames
//! pass between them through a double-buffered zero-copy channel, and a
//! synthetic pattern stands in whenever the camera is not producing frames.

#![no_std]
#![no_main]

mod camera;
mod usb;

use defmt::info;
use embassy_executor::{Executor, Spawner};
use embassy_futures::join::{join, join4};
use embassy_rp::gpio::{Level, Output};
use embassy_rp::interrupt::InterruptExt;
use embassy_rp::multicore::{Stack, spawn_core1};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, I2C1, PIN_10, USB};
use embassy_rp::usb::Driver;
use embassy_rp::{Peri, bind_interrupts, dma, i2c, interrupt, spi, usb as rp_usb};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::zerocopy_channel::Channel;
use embassy_time::{Delay, Timer};
use static_cell::{ConstStaticCell, StaticCell};
use thermal_core::lepton::cci::Cci;
use thermal_core::lepton::power::Power;
use thermal_core::lepton::vospi::FRAME_LEN;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => rp_usb::InterruptHandler<USB>;
    I2C1_IRQ => i2c::InterruptHandler<I2C1>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>;
});

/// One captured frame, as 16-bit little-endian pixels.
pub struct Frame {
    /// Sequence number, wrapping, so the host can spot dropped frames.
    pub seq: u32,
    /// Captured during a flat-field correction, so it shows the shutter.
    pub ffc: bool,
    /// Pixel data, already in the byte order the wire protocol uses.
    pub pixels: [u8; FRAME_LEN],
}

impl Frame {
    /// An empty frame, used to place the buffers in `.bss` at link time rather
    /// than building 75 KB of them on the stack.
    const ZERO: Self = Self {
        seq: 0,
        ffc: false,
        pixels: [0; FRAME_LEN],
    };
}

/// Capture and streaming run at their own pace, so frames pass between them
/// double-buffered: one is filled while the other is sent.
static FRAMES: ConstStaticCell<[Frame; 2]> = ConstStaticCell::new([Frame::ZERO, Frame::ZERO]);

/// The frame channel, shared by both cores, so its lock must work across
/// cores: `CriticalSectionRawMutex` on the RP2040 is a hardware spinlock.
static CHANNEL: StaticCell<Channel<'static, CriticalSectionRawMutex, Frame>> = StaticCell::new();

/// Scratch frame for capture when both channel buffers are taken.
static SCRATCH: ConstStaticCell<[u8; FRAME_LEN]> = ConstStaticCell::new([0; FRAME_LEN]);

/// Core 1's stack. The RP2040 has no guard against overflowing it, so it is
/// sized well beyond what the capture loop needs.
static CORE1_STACK: ConstStaticCell<Stack<8192>> = ConstStaticCell::new(Stack::new());

/// Core 1's executor, running only the capture task.
static CORE1_EXECUTOR: StaticCell<Executor> = StaticCell::new();

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    info!("picothermal firmware booted");

    let channel = CHANNEL.init(Channel::new(FRAMES.take()));
    let (sender, mut receiver) = channel.split();

    let capture = camera::CameraCapture {
        spi: spi::Spi::new_rxonly(
            p.SPI0,
            p.PIN_2,
            p.PIN_0,
            p.DMA_CH0,
            p.DMA_CH1,
            Irqs,
            camera::spi_config(),
        ),
        cs: Output::new(p.PIN_1, Level::High),
        frames: sender,
        scratch: SCRATCH.take(),
    };

    // Interrupts are enabled per core, and init enabled the DMA one here. Move
    // it to core 1, so a finished packet wakes capture directly instead of
    // waiting for core 0 to be free to take the interrupt.
    interrupt::DMA_IRQ_0.disable();
    spawn_core1(p.CORE1, CORE1_STACK.take(), move || {
        // SAFETY: the DMA handler is bound through `Irqs`, and core 0 no
        // longer takes this interrupt, so the handler only ever runs here.
        unsafe { interrupt::DMA_IRQ_0.enable() };
        CORE1_EXECUTOR.init(Executor::new()).run(|spawner| {
            // The task pool holds one capture task and nothing else spawns it,
            // so this cannot be busy.
            spawner.spawn(camera::run_capture(capture).expect("capture task already running"));
        })
    });

    let control = camera::CameraControl {
        power: Power::new(
            Output::new(p.PIN_24, Level::Low),
            Output::new(p.PIN_25, Level::Low),
        ),
        cci: Cci::new(
            i2c::I2c::new_async(p.I2C1, p.PIN_7, p.PIN_6, Irqs, camera::i2c_config()),
            Delay,
        ),
    };

    let driver = Driver::new(p.USB, Irqs);
    let mut buffers = usb::Buffers::new();
    let (mut device, mut endpoints) = usb::build(driver, &mut buffers);

    // The USB stack borrows buffers that live in this function, so everything
    // runs as joined futures here rather than as spawned tasks, which would
    // need 'static.
    join4(
        device.run(),
        join(
            usb::stream(&mut endpoints.video, &mut receiver),
            usb::commands(&mut endpoints.commands),
        ),
        camera::run_control(control),
        heartbeat(p.PIN_10),
    )
    .await;
}

/// Blinks the LED so a board with no host attached still shows it is running.
async fn heartbeat(pin: Peri<'static, PIN_10>) -> ! {
    let mut led = Output::new(pin, Level::Low);
    loop {
        led.toggle();
        Timer::after_millis(500).await;
    }
}
