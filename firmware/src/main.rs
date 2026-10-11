//! RP2040 thermal camera firmware.
//!
//! Captures VoSPI frames from the Lepton on one side and streams them over
//! WebUSB on the other, with a synthetic pattern standing in whenever the
//! camera is not producing frames.

#![no_std]
#![no_main]

mod camera;
mod usb;

use defmt::info;
use embassy_executor::Spawner;
use embassy_futures::join::{join, join4};
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, I2C1, PIN_10, USB};
use embassy_rp::usb::Driver;
use embassy_rp::{Peri, bind_interrupts, dma, i2c, spi, usb as rp_usb};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::zerocopy_channel::Channel;
use embassy_time::{Delay, Timer};
use static_cell::ConstStaticCell;
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

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    info!("rust-thermal firmware booted");

    let mut channel: Channel<'_, NoopRawMutex, Frame> = Channel::new(FRAMES.take());
    let (sender, mut receiver) = channel.split();

    let camera = camera::Camera {
        power: Power::new(
            Output::new(p.PIN_24, Level::Low),
            Output::new(p.PIN_25, Level::Low),
        ),
        cci: Cci::new(
            i2c::I2c::new_async(p.I2C1, p.PIN_7, p.PIN_6, Irqs, camera::i2c_config()),
            Delay,
        ),
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
    };

    let driver = Driver::new(p.USB, Irqs);
    let mut buffers = usb::Buffers::new();
    let (mut device, mut endpoints) = usb::build(driver, &mut buffers);

    // The USB stack and the frame channel both borrow buffers that live in this
    // function, so everything runs as joined futures here rather than as
    // spawned tasks, which would need 'static.
    join4(
        device.run(),
        join(
            usb::stream(&mut endpoints.video, &mut receiver),
            usb::commands(&mut endpoints.commands),
        ),
        camera::run(camera, sender),
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
