//! Camera Control Interface (CCI): the Lepton's I2C command protocol.
//!
//! The async driver itself lands with the CCI milestone; this module
//! currently defines the protocol constants it will be built on.

/// 7-bit I2C address of the Lepton CCI.
pub const I2C_ADDRESS: u8 = 0x2A;

/// CCI status register address.
pub const REG_STATUS: u16 = 0x0002;

/// CCI command ID register address.
pub const REG_COMMAND: u16 = 0x0004;

/// CCI data length register address (length in 16-bit words).
pub const REG_DATA_LEN: u16 = 0x0006;

/// First CCI data register address; data registers are contiguous from here.
pub const REG_DATA_0: u16 = 0x0008;

// TODO(cci): async `Cci<I2c>` driver — poll BUSY in REG_STATUS, issue
// get/set/run commands, boot handshake, VSYNC enable, AGC and radiometry
// (TLinear) mode control.
