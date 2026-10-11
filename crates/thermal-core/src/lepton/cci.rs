//! Camera Control Interface (CCI): the Lepton's I2C command protocol.
//!
//! Every exchange goes through the same four registers: the host waits for
//! BUSY to clear, stages data and its length, writes a command ID, then waits
//! for BUSY to clear again and reads the result. [`Cci`] wraps that handshake
//! and exposes the commands the firmware needs.
//!
//! Each 16-bit register is big-endian on the wire, but values wider than one
//! register are split least significant word first, so a 32-bit enum occupies
//! DATA_0 (low half) then DATA_1 (high half). Confirmed on hardware: the other
//! order made the camera reject VSYNC mode 5 with a range error.

use embedded_hal_async::delay::DelayNs;
use embedded_hal_async::i2c::I2c;

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

/// Data words that fit in the contiguous data registers.
pub const MAX_DATA_WORDS: usize = 16;

/// How long the camera may take to boot after reset is released.
const BOOT_TIMEOUT_MS: u32 = 2000;

/// How long a single command may stay busy.
const COMMAND_TIMEOUT_MS: u32 = 1000;

/// Interval between BUSY polls.
const POLL_INTERVAL_MS: u32 = 10;

/// STATUS bit that is set while a command is in progress.
const STATUS_BUSY: u16 = 1 << 0;

/// STATUS bit that is set once the camera has finished booting.
const STATUS_BOOT_DONE: u16 = 1 << 2;

/// Which of the three operations a command ID selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
enum CommandType {
    Get = 0,
    Set = 1,
    Run = 2,
}

// Command IDs come from the FLIR Lepton Software IDD. All but
// SYS_FPA_TEMPERATURE have been exercised on hardware; a wrong ID shows up as
// Error::Camera rather than as a bus failure.

/// A CCI command, identified by its module base and offset.
///
/// The wire value is the module base, plus the command offset, plus the
/// operation, so one [`Command`] covers the get, set and run forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command(u16);

impl Command {
    /// SYS module, unprotected.
    const SYS: u16 = 0x0200;
    /// AGC module, unprotected.
    const AGC: u16 = 0x0100;
    /// OEM module. The 0x4000 bit is the protection bit the camera requires on
    /// OEM and RAD commands; without it the camera answers with an error.
    const OEM: u16 = 0x4800;
    /// RAD module, likewise protected.
    const RAD: u16 = 0x4E00;

    /// Answers if the camera is alive. Run only.
    pub const SYS_PING: Self = Self::at(Self::SYS, 0x0000);
    /// FLIR's 64-bit serial number. Get only, 4 words.
    pub const SYS_FLIR_SERIAL: Self = Self::at(Self::SYS, 0x0008);
    /// Focal plane array temperature in kelvin, hundredths. Get only, 1 word.
    pub const SYS_FPA_TEMPERATURE: Self = Self::at(Self::SYS, 0x0014);
    /// State of the flat-field correction the camera runs on its own. Get
    /// only, 2 words.
    pub const SYS_FFC_STATUS: Self = Self::at(Self::SYS, 0x0044);
    /// Whether AGC is applied to the video output. 2 words.
    pub const AGC_ENABLE: Self = Self::at(Self::AGC, 0x0000);
    /// Which function the camera's GPIO3 pin serves; 5 selects VSYNC. 2 words.
    pub const OEM_GPIO_MODE: Self = Self::at(Self::OEM, 0x0054);
    /// Whether radiometry outputs TLinear centikelvin pixels. 2 words.
    pub const RAD_TLINEAR_ENABLE: Self = Self::at(Self::RAD, 0x00C0);

    /// A command at `offset` within `module`.
    const fn at(module: u16, offset: u16) -> Self {
        Self(module + offset)
    }

    /// The wire value of this command for `operation`.
    fn id(self, operation: CommandType) -> u16 {
        self.0 | operation as u16
    }
}

/// GPIO mode that makes the camera drive VSYNC.
const GPIO_MODE_VSYNC: u32 = 5;

/// Where the camera is in a flat-field correction.
///
/// The Lepton closes its shutter to recalibrate every few minutes, and after
/// large temperature changes. While it does, its video shows the shutter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfcStatus {
    /// No correction in progress.
    Ready,
    /// The shutter is closed and a correction is running.
    Busy,
    /// The camera is averaging frames to finish the correction.
    Collecting,
    /// The camera reported a failed correction.
    Error,
}

impl FfcStatus {
    /// Whether the video is showing the shutter rather than the scene.
    pub fn in_progress(self) -> bool {
        matches!(self, Self::Busy | Self::Collecting)
    }

    fn from_raw(raw: i32) -> Self {
        match raw {
            0 => Self::Ready,
            1 => Self::Busy,
            2 => Self::Collecting,
            _ => Self::Error,
        }
    }
}

/// Errors from a CCI exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error<E> {
    /// The I2C transfer itself failed.
    I2c(E),
    /// The camera reported an error in its status register.
    Camera(i8),
    /// The camera stayed busy, or never finished booting, in time.
    Timeout,
    /// More data words were passed, or returned, than the registers hold.
    DataLength,
}

impl<E> From<E> for Error<E> {
    fn from(error: E) -> Self {
        Self::I2c(error)
    }
}

/// Async driver for the Lepton's control interface.
pub struct Cci<I2C, D> {
    i2c: I2C,
    delay: D,
}

impl<I2C: I2c, D: DelayNs> Cci<I2C, D> {
    /// Wraps an I2C bus and a delay source.
    pub fn new(i2c: I2C, delay: D) -> Self {
        Self { i2c, delay }
    }

    /// Releases the bus and delay.
    pub fn release(self) -> (I2C, D) {
        (self.i2c, self.delay)
    }

    /// Waits for the camera to finish booting and become idle.
    ///
    /// Call this after releasing reset, before any other command.
    pub async fn wait_until_ready(&mut self) -> Result<(), Error<I2C::Error>> {
        let mut waited = 0;
        loop {
            // A camera still holding the bus down answers nothing useful, so a
            // failed read here is treated as "not ready yet" rather than fatal.
            if let Ok(status) = self.read_register(REG_STATUS).await
                && status & STATUS_BOOT_DONE != 0
                && status & STATUS_BUSY == 0
            {
                return Ok(());
            }

            if waited >= BOOT_TIMEOUT_MS {
                return Err(Error::Timeout);
            }
            self.delay.delay_ms(POLL_INTERVAL_MS).await;
            waited += POLL_INTERVAL_MS;
        }
    }

    /// Checks that the camera responds.
    pub async fn ping(&mut self) -> Result<(), Error<I2C::Error>> {
        self.run(Command::SYS_PING).await
    }

    /// Reads FLIR's serial number for this camera.
    pub async fn flir_serial_number(&mut self) -> Result<u64, Error<I2C::Error>> {
        let mut words = [0u16; 4];
        self.get(Command::SYS_FLIR_SERIAL, &mut words).await?;
        Ok(words
            .iter()
            .rev()
            .fold(0u64, |serial, &word| (serial << 16) | u64::from(word)))
    }

    /// Reads where the camera is in a flat-field correction.
    pub async fn ffc_status(&mut self) -> Result<FfcStatus, Error<I2C::Error>> {
        let mut words = [0u16; 2];
        self.get(Command::SYS_FFC_STATUS, &mut words).await?;
        let raw = u32::from(words[0]) | (u32::from(words[1]) << 16);
        Ok(FfcStatus::from_raw(raw as i32))
    }

    /// Reads the focal plane array temperature, in hundredths of a kelvin.
    pub async fn fpa_temperature(&mut self) -> Result<u16, Error<I2C::Error>> {
        let mut word = [0u16; 1];
        self.get(Command::SYS_FPA_TEMPERATURE, &mut word).await?;
        Ok(word[0])
    }

    /// Turns the camera's AGC on or off.
    ///
    /// AGC produces the 8-bit display image; it must be off for radiometric
    /// output to mean anything.
    pub async fn set_agc(&mut self, enabled: bool) -> Result<(), Error<I2C::Error>> {
        self.set_enum(Command::AGC_ENABLE, u32::from(enabled)).await
    }

    /// Turns TLinear radiometry on or off.
    ///
    /// With it on, pixels are absolute temperatures in centikelvin.
    pub async fn set_tlinear(&mut self, enabled: bool) -> Result<(), Error<I2C::Error>> {
        self.set_enum(Command::RAD_TLINEAR_ENABLE, u32::from(enabled))
            .await
    }

    /// Makes the camera drive its VSYNC output, which paces VoSPI capture.
    pub async fn enable_vsync(&mut self) -> Result<(), Error<I2C::Error>> {
        self.set_enum(Command::OEM_GPIO_MODE, GPIO_MODE_VSYNC).await
    }

    /// Runs a command that takes no data and returns none.
    pub async fn run(&mut self, command: Command) -> Result<(), Error<I2C::Error>> {
        self.wait_idle().await?;
        self.write_register(REG_DATA_LEN, 0).await?;
        self.write_register(REG_COMMAND, command.id(CommandType::Run))
            .await?;
        self.finish().await
    }

    /// Runs a get command and fills `out` with the words it returns.
    pub async fn get(
        &mut self,
        command: Command,
        out: &mut [u16],
    ) -> Result<(), Error<I2C::Error>> {
        if out.len() > MAX_DATA_WORDS {
            return Err(Error::DataLength);
        }

        self.wait_idle().await?;
        self.write_register(REG_COMMAND, command.id(CommandType::Get))
            .await?;
        self.finish().await?;

        // The camera reports how much it produced; a short answer means the
        // command was not the one we think it was.
        let words = usize::from(self.read_register(REG_DATA_LEN).await?);
        if words < out.len() {
            return Err(Error::DataLength);
        }

        let mut bytes = [0u8; MAX_DATA_WORDS * 2];
        let bytes = &mut bytes[..out.len() * 2];
        self.i2c
            .write_read(I2C_ADDRESS, &REG_DATA_0.to_be_bytes(), bytes)
            .await?;
        for (word, chunk) in out.iter_mut().zip(bytes.chunks_exact(2)) {
            *word = u16::from_be_bytes([chunk[0], chunk[1]]);
        }
        Ok(())
    }

    /// Runs a set command carrying `data`.
    pub async fn set(&mut self, command: Command, data: &[u16]) -> Result<(), Error<I2C::Error>> {
        if data.len() > MAX_DATA_WORDS {
            return Err(Error::DataLength);
        }

        self.wait_idle().await?;

        // Data registers are contiguous, so one write starting at DATA_0 fills
        // as many as the payload needs.
        let mut buf = [0u8; 2 + MAX_DATA_WORDS * 2];
        buf[..2].copy_from_slice(&REG_DATA_0.to_be_bytes());
        for (word, chunk) in data.iter().zip(buf[2..].chunks_exact_mut(2)) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        self.i2c
            .write(I2C_ADDRESS, &buf[..2 + data.len() * 2])
            .await?;

        self.write_register(REG_DATA_LEN, data.len() as u16).await?;
        self.write_register(REG_COMMAND, command.id(CommandType::Set))
            .await?;
        self.finish().await
    }

    /// Sets a command whose payload is a single 32-bit enum.
    async fn set_enum(&mut self, command: Command, value: u32) -> Result<(), Error<I2C::Error>> {
        self.set(command, &[value as u16, (value >> 16) as u16])
            .await
    }

    /// Waits for BUSY to clear before a command is issued.
    async fn wait_idle(&mut self) -> Result<(), Error<I2C::Error>> {
        let mut waited = 0;
        while self.read_register(REG_STATUS).await? & STATUS_BUSY != 0 {
            if waited >= COMMAND_TIMEOUT_MS {
                return Err(Error::Timeout);
            }
            self.delay.delay_ms(POLL_INTERVAL_MS).await;
            waited += POLL_INTERVAL_MS;
        }
        Ok(())
    }

    /// Waits for a command to complete and reports the error it left behind.
    async fn finish(&mut self) -> Result<(), Error<I2C::Error>> {
        let mut waited = 0;
        loop {
            let status = self.read_register(REG_STATUS).await?;
            if status & STATUS_BUSY == 0 {
                // The result code lives in the top byte, as a signed value
                // where anything below zero is a failure.
                let code = (status >> 8) as i8;
                return if code < 0 {
                    Err(Error::Camera(code))
                } else {
                    Ok(())
                };
            }

            if waited >= COMMAND_TIMEOUT_MS {
                return Err(Error::Timeout);
            }
            self.delay.delay_ms(POLL_INTERVAL_MS).await;
            waited += POLL_INTERVAL_MS;
        }
    }

    /// Reads one 16-bit register.
    async fn read_register(&mut self, register: u16) -> Result<u16, Error<I2C::Error>> {
        let mut bytes = [0u8; 2];
        self.i2c
            .write_read(I2C_ADDRESS, &register.to_be_bytes(), &mut bytes)
            .await?;
        Ok(u16::from_be_bytes(bytes))
    }

    /// Writes one 16-bit register.
    async fn write_register(&mut self, register: u16, value: u16) -> Result<(), Error<I2C::Error>> {
        let [addr_hi, addr_lo] = register.to_be_bytes();
        let [value_hi, value_lo] = value.to_be_bytes();
        self.i2c
            .write(I2C_ADDRESS, &[addr_hi, addr_lo, value_hi, value_lo])
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Cci, Command, Error, FfcStatus, MAX_DATA_WORDS};
    use crate::lepton::mock::{MockCamera, NoDelay};
    use crate::test_util::block_on;

    #[test]
    fn ping_issues_a_run_command() {
        let mut cci = Cci::new(MockCamera::new(), NoDelay);
        block_on(cci.ping()).unwrap();

        let (camera, _) = cci.release();
        // SYS module base 0x0200, offset 0, run.
        assert_eq!(camera.last_command, 0x0202);
    }

    #[test]
    fn waits_out_a_busy_camera() {
        let mut cci = Cci::new(MockCamera::new().with_busy_polls(5), NoDelay);
        assert_eq!(block_on(cci.ping()), Ok(()));
    }

    #[test]
    fn waits_for_boot_to_finish() {
        let mut cci = Cci::new(MockCamera::new().still_booting(20), NoDelay);
        assert_eq!(block_on(cci.wait_until_ready()), Ok(()));
    }

    #[test]
    fn reports_a_camera_error_code() {
        let mut camera = MockCamera::new();
        camera.error_code = -3;
        let mut cci = Cci::new(camera, NoDelay);
        assert_eq!(block_on(cci.ping()), Err(Error::Camera(-3)));
    }

    #[test]
    fn reads_the_serial_number_low_word_first() {
        let mut camera = MockCamera::new();
        camera.stage_data(&[0x6677, 0x4455, 0x2233, 0x0011]);
        let mut cci = Cci::new(camera, NoDelay);

        assert_eq!(
            block_on(cci.flir_serial_number()),
            Ok(0x0011_2233_4455_6677)
        );
        let (camera, _) = cci.release();
        assert_eq!(camera.last_command, 0x0208);
    }

    #[test]
    fn enum_setters_send_a_32_bit_value_low_word_first() {
        let mut cci = Cci::new(MockCamera::new(), NoDelay);
        block_on(cci.enable_vsync()).unwrap();

        let (camera, _) = cci.release();
        assert_eq!(camera.written_data(), &[0x0005, 0x0000]);
        // OEM module with the protection bit, GPIO mode select, set.
        assert_eq!(camera.last_command, 0x4855);
    }

    #[test]
    fn agc_and_tlinear_toggle_the_right_commands() {
        let mut cci = Cci::new(MockCamera::new(), NoDelay);

        block_on(cci.set_agc(true)).unwrap();
        assert_eq!(cci.i2c.last_command, 0x0101);
        assert_eq!(cci.i2c.written_data(), &[0x0001, 0x0000]);

        block_on(cci.set_tlinear(false)).unwrap();
        assert_eq!(cci.i2c.last_command, 0x4EC1);
        assert_eq!(cci.i2c.written_data(), &[0x0000, 0x0000]);
    }

    #[test]
    fn rejects_payloads_larger_than_the_data_registers() {
        let mut cci = Cci::new(MockCamera::new(), NoDelay);
        let too_much = [0u16; MAX_DATA_WORDS + 1];

        assert_eq!(
            block_on(cci.set(Command::AGC_ENABLE, &too_much)),
            Err(Error::DataLength)
        );
        let mut out = [0u16; MAX_DATA_WORDS + 1];
        assert_eq!(
            block_on(cci.get(Command::AGC_ENABLE, &mut out)),
            Err(Error::DataLength)
        );
    }

    #[test]
    fn decodes_ffc_status() {
        for (words, expected) in [
            ([0x0000, 0x0000], FfcStatus::Ready),
            ([0x0001, 0x0000], FfcStatus::Busy),
            ([0x0002, 0x0000], FfcStatus::Collecting),
            // -1, the camera's generic FFC error, as a 32-bit enum.
            ([0xFFFF, 0xFFFF], FfcStatus::Error),
        ] {
            let mut camera = MockCamera::new();
            camera.stage_data(&words);
            let mut cci = Cci::new(camera, NoDelay);
            assert_eq!(block_on(cci.ffc_status()), Ok(expected));
            assert_eq!(cci.i2c.last_command, 0x0244);
        }
        assert!(FfcStatus::Busy.in_progress());
        assert!(FfcStatus::Collecting.in_progress());
        assert!(!FfcStatus::Ready.in_progress());
    }

    #[test]
    fn rejects_a_short_answer() {
        let mut camera = MockCamera::new();
        camera.stage_data(&[0x1234]);
        let mut cci = Cci::new(camera, NoDelay);

        // Asking for four words when the camera produced one is a sign the
        // command was not what the driver assumed.
        let mut out = [0u16; 4];
        assert_eq!(
            block_on(cci.get(Command::SYS_FLIR_SERIAL, &mut out)),
            Err(Error::DataLength)
        );
    }
}
