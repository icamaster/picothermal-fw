//! RP2040 thermal camera firmware.
//!
//! Streams synthetic frames over WebUSB while the Lepton driver is being
//! written, and blinks the LED as a sign of life.

#![no_std]
#![no_main]

mod usb;

use defmt::info;
use embassy_executor::Spawner;
use embassy_futures::join::{join, join3};
use embassy_rp::Peri;
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{PIN_10, USB};
use embassy_rp::usb::{Driver, InterruptHandler};
use embassy_time::Timer;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => InterruptHandler<USB>;
});

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    info!("rust-thermal firmware booted");

    let driver = Driver::new(p.USB, Irqs);
    let mut buffers = usb::Buffers::new();
    let (mut device, mut endpoints) = usb::build(driver, &mut buffers);

    // The USB stack borrows buffers that live in this function, so everything
    // runs as joined futures here rather than as spawned tasks, which would
    // need 'static.
    join3(
        device.run(),
        join(
            usb::stream(&mut endpoints.video),
            usb::commands(&mut endpoints.commands),
        ),
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
