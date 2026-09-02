//! A mock Lepton for host tests.
//!
//! [`MockCamera`] implements the async I2C traits the driver is written
//! against and answers CCI register traffic the way the camera does, including
//! staying busy for a few polls after each command. [`MockPin`] and
//! [`MockDelay`] record what the power sequence did into a shared [`Trace`], so
//! a test can assert the order of the steps.

use core::cell::RefCell;
use core::convert::Infallible;

use embedded_hal_async::delay::DelayNs;
use embedded_hal_async::i2c::{ErrorType, I2c, Operation, SevenBitAddress};

use crate::lepton::cci::{I2C_ADDRESS, MAX_DATA_WORDS, REG_COMMAND, REG_DATA_LEN, REG_STATUS};

/// Errors a [`MockCamera`] can raise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockError {
    /// A transfer addressed something other than the Lepton.
    WrongAddress,
    /// A transfer was not shaped like a register access.
    Malformed,
}

impl embedded_hal_async::i2c::Error for MockError {
    fn kind(&self) -> embedded_hal_async::i2c::ErrorKind {
        embedded_hal_async::i2c::ErrorKind::Other
    }
}

/// A delay that returns immediately, so tests do not wait in real time.
pub struct NoDelay;

impl DelayNs for NoDelay {
    async fn delay_ns(&mut self, _ns: u32) {}
}

/// Register-level stand-in for the Lepton's control interface.
pub struct MockCamera {
    status: u16,
    data_len: u16,
    data: [u16; MAX_DATA_WORDS],
    /// How many more status reads report BUSY before the command completes.
    busy_polls: u8,
    /// Polls to stay busy after each command, imitating processing time.
    busy_after_command: u8,
    /// The last command ID written, for tests to assert on.
    pub last_command: u16,
    /// Error code the camera returns in its status register.
    pub error_code: i8,
}

impl Default for MockCamera {
    fn default() -> Self {
        Self::new()
    }
}

impl MockCamera {
    /// A camera that has booted, is idle, and answers every command happily.
    pub fn new() -> Self {
        Self {
            // Booted, not busy.
            status: 1 << 2,
            data_len: 0,
            data: [0; MAX_DATA_WORDS],
            busy_polls: 0,
            busy_after_command: 0,
            last_command: 0,
            error_code: 0,
        }
    }

    /// Makes each command report BUSY for `polls` status reads first.
    pub fn with_busy_polls(mut self, polls: u8) -> Self {
        self.busy_after_command = polls;
        self
    }

    /// Starts the camera as still booting, becoming ready after `polls` reads.
    pub fn still_booting(mut self, polls: u8) -> Self {
        self.status = 0;
        self.busy_polls = polls;
        self
    }

    /// Stages the words a get command will return.
    pub fn stage_data(&mut self, words: &[u16]) {
        self.data[..words.len()].copy_from_slice(words);
        self.data_len = words.len() as u16;
    }

    /// The words the driver last wrote with a set command.
    pub fn written_data(&self) -> &[u16] {
        &self.data[..self.data_len as usize]
    }

    /// Value of the register at `address`, applying the busy countdown.
    fn read(&mut self, address: u16) -> u16 {
        match address {
            REG_STATUS => {
                if self.busy_polls > 0 {
                    self.busy_polls -= 1;
                    // Booted flag stays clear while booting, and BUSY is set
                    // while a command is still running.
                    return self.status | 1;
                }
                // Once ready, the camera reports booted plus the result code.
                (1 << 2) | ((self.error_code as u8 as u16) << 8)
            }
            REG_DATA_LEN => self.data_len,
            address => {
                let index = (address - crate::lepton::cci::REG_DATA_0) as usize / 2;
                self.data.get(index).copied().unwrap_or(0)
            }
        }
    }

    /// Applies a register write.
    fn write(&mut self, address: u16, value: u16) {
        match address {
            REG_COMMAND => {
                self.last_command = value;
                self.busy_polls = self.busy_after_command;
            }
            REG_DATA_LEN => self.data_len = value,
            address => {
                let index = (address - crate::lepton::cci::REG_DATA_0) as usize / 2;
                if let Some(word) = self.data.get_mut(index) {
                    *word = value;
                }
            }
        }
    }
}

impl ErrorType for MockCamera {
    type Error = MockError;
}

impl I2c<SevenBitAddress> for MockCamera {
    async fn transaction(
        &mut self,
        address: SevenBitAddress,
        operations: &mut [Operation<'_>],
    ) -> Result<(), Self::Error> {
        if address != I2C_ADDRESS {
            return Err(MockError::WrongAddress);
        }

        // The driver only ever issues a write, or a write followed by a read.
        let mut register = None;
        for operation in operations {
            match operation {
                Operation::Write(bytes) => {
                    if bytes.len() < 2 {
                        return Err(MockError::Malformed);
                    }
                    let address = u16::from_be_bytes([bytes[0], bytes[1]]);
                    register = Some(address);

                    for (index, chunk) in bytes[2..].chunks_exact(2).enumerate() {
                        let value = u16::from_be_bytes([chunk[0], chunk[1]]);
                        self.write(address + index as u16 * 2, value);
                    }
                }
                Operation::Read(bytes) => {
                    let Some(address) = register else {
                        return Err(MockError::Malformed);
                    };
                    if bytes.len() % 2 != 0 {
                        return Err(MockError::Malformed);
                    }
                    for (index, chunk) in bytes.chunks_exact_mut(2).enumerate() {
                        let value = self.read(address + index as u16 * 2);
                        chunk.copy_from_slice(&value.to_be_bytes());
                    }
                }
            }
        }
        Ok(())
    }
}

/// One thing the driver did to the power lines, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// RESET_L was driven; `true` is high, which releases reset.
    Reset(bool),
    /// PWR_DWN_L was driven; `true` is high, which releases power-down.
    PowerDown(bool),
    /// The driver waited this many microseconds.
    DelayUs(u32),
}

/// Ordered record of what the driver did, shared by the mock pins and delay.
pub struct Trace {
    events: [Event; Self::CAPACITY],
    len: usize,
}

impl Default for Trace {
    fn default() -> Self {
        Self::new()
    }
}

impl Trace {
    const CAPACITY: usize = 16;

    /// An empty trace.
    pub fn new() -> Self {
        Self {
            events: [Event::DelayUs(0); Self::CAPACITY],
            len: 0,
        }
    }

    /// What has been recorded so far.
    pub fn events(&self) -> &[Event] {
        &self.events[..self.len]
    }

    fn push(&mut self, event: Event) {
        assert!(self.len < Self::CAPACITY, "trace overflowed");
        self.events[self.len] = event;
        self.len += 1;
    }
}

/// Which control line a [`MockPin`] stands for.
#[derive(Clone, Copy)]
enum Line {
    Reset,
    PowerDown,
}

/// An output pin that records what it was driven to.
pub struct MockPin<'a> {
    trace: &'a RefCell<Trace>,
    line: Line,
}

impl<'a> MockPin<'a> {
    /// A pin standing in for RESET_L.
    pub fn reset(trace: &'a RefCell<Trace>) -> Self {
        Self {
            trace,
            line: Line::Reset,
        }
    }

    /// A pin standing in for PWR_DWN_L.
    pub fn power_down(trace: &'a RefCell<Trace>) -> Self {
        Self {
            trace,
            line: Line::PowerDown,
        }
    }

    fn record(&mut self, high: bool) {
        let event = match self.line {
            Line::Reset => Event::Reset(high),
            Line::PowerDown => Event::PowerDown(high),
        };
        self.trace.borrow_mut().push(event);
    }
}

impl embedded_hal::digital::ErrorType for MockPin<'_> {
    type Error = Infallible;
}

impl embedded_hal::digital::OutputPin for MockPin<'_> {
    fn set_low(&mut self) -> Result<(), Self::Error> {
        self.record(false);
        Ok(())
    }

    fn set_high(&mut self) -> Result<(), Self::Error> {
        self.record(true);
        Ok(())
    }
}

/// A delay that records how long it was asked to wait, without waiting.
pub struct MockDelay<'a> {
    trace: &'a RefCell<Trace>,
}

impl<'a> MockDelay<'a> {
    /// A delay recording into `trace`.
    pub fn new(trace: &'a RefCell<Trace>) -> Self {
        Self { trace }
    }
}

impl DelayNs for MockDelay<'_> {
    async fn delay_ns(&mut self, ns: u32) {
        self.trace.borrow_mut().push(Event::DelayUs(ns / 1000));
    }
}
