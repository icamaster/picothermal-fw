//! Power and reset sequencing for the Lepton.
//!
//! The camera has two active-low control lines, PWR_DWN_L and RESET_L, and the
//! datasheet is specific about the order they are released in. [`Power`] owns
//! both and implements that sequence.
//!
//! The pins are plain [`OutputPin`]s rather than async ones: driving a pin
//! never waits, so `embedded-hal-async` has no output trait. Only the delays
//! between the steps suspend.

use embedded_hal::digital::OutputPin;
use embedded_hal_async::delay::DelayNs;

/// Settling time between sequence steps.
///
/// The datasheet asks for at least 5000 master clock cycles, which is 200 us at
/// the Lepton's 25 MHz clock.
const SETTLE_US: u32 = 200;

/// How long the camera takes to boot once reset is released, before it will
/// answer CCI commands.
const BOOT_MS: u32 = 950;

// TODO(power): the settling time between releasing PWR_DWN_L and RESET_L is
// derived from the 5000-cycle figure rather than a stated interval. Confirm
// against the datasheet if the camera ever fails to come up.

/// Owns the Lepton's two power control lines.
pub struct Power<RST, PWR> {
    reset: RST,
    power_down: PWR,
}

impl<E, RST, PWR> Power<RST, PWR>
where
    RST: OutputPin<Error = E>,
    PWR: OutputPin<Error = E>,
{
    /// Takes the RESET_L and PWR_DWN_L pins.
    ///
    /// Both lines are left as the caller configured them; call [`Self::up`] to
    /// bring the camera into a known, running state.
    pub fn new(reset: RST, power_down: PWR) -> Self {
        Self { reset, power_down }
    }

    /// Returns the pins.
    pub fn release(self) -> (RST, PWR) {
        (self.reset, self.power_down)
    }

    /// Brings the camera up from any state and waits for it to boot.
    ///
    /// Both lines are asserted first, so this doubles as a way to restart a
    /// camera that is already running. On return the camera has had its boot
    /// time; the CCI handshake still has to confirm it is ready.
    pub async fn up<D: DelayNs>(&mut self, delay: &mut D) -> Result<(), E> {
        self.reset.set_low()?;
        self.power_down.set_low()?;
        delay.delay_us(SETTLE_US).await;

        // Power first, then reset: the camera must be powered before it is
        // allowed to start booting.
        self.power_down.set_high()?;
        delay.delay_us(SETTLE_US).await;
        self.reset.set_high()?;
        delay.delay_ms(BOOT_MS).await;

        Ok(())
    }

    /// Holds the camera in reset and powered down.
    pub fn down(&mut self) -> Result<(), E> {
        self.reset.set_low()?;
        self.power_down.set_low()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use core::cell::RefCell;

    use super::{BOOT_MS, Power, SETTLE_US};
    use crate::lepton::mock::{Event, MockDelay, MockPin, Trace};
    use crate::test_util::block_on;

    #[test]
    fn power_up_releases_power_before_reset_and_waits_for_boot() {
        let trace = RefCell::new(Trace::new());
        let mut power = Power::new(MockPin::reset(&trace), MockPin::power_down(&trace));
        let mut delay = MockDelay::new(&trace);

        block_on(power.up(&mut delay)).unwrap();

        assert_eq!(
            trace.borrow().events(),
            &[
                Event::Reset(false),
                Event::PowerDown(false),
                Event::DelayUs(SETTLE_US),
                Event::PowerDown(true),
                Event::DelayUs(SETTLE_US),
                Event::Reset(true),
                Event::DelayUs(BOOT_MS * 1000),
            ]
        );
    }

    #[test]
    fn power_down_asserts_both_lines() {
        let trace = RefCell::new(Trace::new());
        let mut power = Power::new(MockPin::reset(&trace), MockPin::power_down(&trace));

        power.down().unwrap();

        assert_eq!(
            trace.borrow().events(),
            &[Event::Reset(false), Event::PowerDown(false)]
        );
    }
}
