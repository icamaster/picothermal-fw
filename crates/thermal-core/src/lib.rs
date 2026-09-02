//! Chip-agnostic core of the thermal camera.
//!
//! Everything in this crate is `no_std`, free of MCU-specific dependencies,
//! and fully testable on the host:
//!
//! - [`frame`]: frame geometry and a synthetic frame source for bring-up.
//! - [`lepton`]: async driver for the FLIR Lepton 3.5 camera module.
//! - [`protocol`]: wire protocol between the firmware and WebUSB hosts.
//!
//! The firmware binary wires these up to real RP2040 peripherals; host tests
//! drive them with mocks.

#![no_std]

pub mod frame;
pub mod lepton;
pub mod protocol;
