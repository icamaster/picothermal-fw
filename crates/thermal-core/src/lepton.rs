//! Driver for the FLIR Lepton 3.5 thermal camera module.
//!
//! The Lepton exposes two interfaces, each covered by a submodule:
//!
//! - [`cci`]: the Camera Control Interface, an I2C register protocol used to
//!   configure the camera (AGC, radiometry, VSYNC, ...).
//! - [`vospi`]: the Video over SPI protocol that delivers the pixel stream.
//! - [`power`]: the RESET_L and PWR_DWN_L sequencing the camera boots with.
//!
//! The driver is generic over `embedded-hal-async` traits, so it can run
//! against real hardware or the [`mock`] camera in host tests.

pub mod cci;
pub mod mock;
pub mod power;
pub mod vospi;
