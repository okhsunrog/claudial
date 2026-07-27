//! PCF85063ATL real-time clock support.
//!
//! The RTC is a separate I2C device at address `0x51`. It shares GPIO14/15
//! with touch and the other board peripherals; its interrupt output is routed
//! to GPIO13 but is not needed for ordinary clock/calendar operation.

use pcf85063a::{BitFlags, Error, PCF85063, Register};
use time::{Date, Month, PrimitiveDateTime, Time};

const OSCILLATOR_STOP_FLAG: u8 = 1 << 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq, defmt::Format)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl DateTime {
    pub fn to_primitive(self) -> Result<PrimitiveDateTime, time::error::ComponentRange> {
        let month = Month::try_from(self.month)?;
        let date = Date::from_calendar_date(i32::from(self.year), month, self.day)?;
        let time = Time::from_hms(self.hour, self.minute, self.second)?;
        Ok(PrimitiveDateTime::new(date, time))
    }

    pub fn from_primitive(datetime: PrimitiveDateTime) -> Self {
        Self {
            year: datetime.year() as u16,
            month: datetime.month().into(),
            day: datetime.day(),
            hour: datetime.hour(),
            minute: datetime.minute(),
            second: datetime.second(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, defmt::Format)]
pub struct Snapshot {
    pub datetime: DateTime,
    pub clock_valid: bool,
}

/// Probe the PCF85063, preserve its calendar, select 24-hour mode, and start it.
pub async fn init<I2C, E>(i2c: I2C) -> Result<(PCF85063<I2C>, bool), Error<E>>
where
    I2C: embedded_hal_async::i2c::I2c<Error = E>,
{
    let mut rtc = PCF85063::new(i2c);

    // The PCF85063 has a battery-backed RAM byte that the similar PCF8563 does
    // not. Toggle one bit and restore it, matching Waveshare SensorLib's
    // non-destructive identity check.
    let saved_ram = rtc.read_ram_byte().await?;
    let probe_ram = saved_ram ^ 0x80;
    rtc.write_ram_byte(probe_ram).await?;
    let observed_ram = rtc.read_ram_byte().await?;
    rtc.write_ram_byte(saved_ram).await?;
    if observed_ram != probe_ram {
        return Err(Error::InvalidInputData);
    }

    let seconds = rtc.read_register(Register::SECONDS).await?;
    let clock_valid = seconds & OSCILLATOR_STOP_FLAG == 0;

    // Do not reset the device: that would destroy a valid battery-backed
    // calendar. Only select 24-hour mode and make sure the oscillator runs.
    rtc.clear_register_bit_flag(Register::CONTROL_1, BitFlags::MODE_12_24 | BitFlags::STOP)
        .await?;

    Ok((rtc, clock_valid))
}

pub async fn read<I2C, E>(rtc: &mut PCF85063<I2C>) -> Result<Snapshot, Error<E>>
where
    I2C: embedded_hal_async::i2c::I2c<Error = E>,
{
    let seconds = rtc.read_register(Register::SECONDS).await?;
    let datetime = rtc.get_datetime().await?;
    Ok(Snapshot {
        datetime: DateTime::from_primitive(datetime),
        clock_valid: seconds & OSCILLATOR_STOP_FLAG == 0,
    })
}
