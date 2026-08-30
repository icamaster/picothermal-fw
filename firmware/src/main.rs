//! RP2040 thermal camera firmware.
//!
//! Bring-up skeleton: blinks the LED on GPIO10 and emits a defmt heartbeat
//! over RTT, proving the toolchain, probe, flashing, and logging all work.

#![no_std]
#![no_main]

use defmt::info;
use embassy_executor::Spawner;
use embassy_rp::gpio::{Level, Output};
use embassy_time::Timer;
use {defmt_rtt as _, panic_probe as _};

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    info!("rust-thermal firmware booted");

    let mut led = Output::new(p.PIN_10, Level::Low);
    loop {
        led.toggle();
        info!("heartbeat");
        Timer::after_millis(500).await;
    }
}
