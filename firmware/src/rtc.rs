//! PCF85063ATL real-time clock support.
//!
//! The RTC is a separate I2C device at address `0x51`. It shares GPIO14/15
//! with touch and the other board peripherals; its interrupt output is routed
//! to GPIO13 but is not needed for ordinary clock/calendar operation.

use claudial_icd::ClockSync;
use pcf85063a::{BitFlags, Error, PCF85063, Register};
use time::{Date, Duration, Month, OffsetDateTime, PrimitiveDateTime, Time};

const OSCILLATOR_STOP_FLAG: u8 = 1 << 7;
// The high bit distinguishes our offset from RAM left by older firmware. The
// low seven bits hold quarter-hours above UTC-12, covering UTC-12..UTC+14.
const OFFSET_FORMAT_MARKER: u8 = 1 << 7;
const OFFSET_MINUTES_PER_UNIT: i16 = 15;
const MIN_UTC_OFFSET_MINUTES: i16 = -12 * 60;
const MAX_UTC_OFFSET_MINUTES: i16 = 14 * 60;

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
    /// The calendar stored in the chip is always UTC.
    pub datetime: DateTime,
    pub clock_valid: bool,
    pub utc_offset_minutes: Option<i16>,
}

impl Snapshot {
    /// True only for a running clock written by the host-sync protocol.
    pub fn synchronized(self) -> bool {
        self.clock_valid && self.utc_offset_minutes.is_some()
    }

    /// UTC Unix timestamp suitable for comparing absolute reset deadlines.
    pub fn unix_timestamp(self) -> Option<i64> {
        if !self.synchronized() {
            return None;
        }
        Some(
            self.datetime
                .to_primitive()
                .ok()?
                .assume_utc()
                .unix_timestamp(),
        )
    }

    /// Current local civil time using the offset captured by the host.
    pub fn local_datetime(self) -> Option<DateTime> {
        let offset = self.utc_offset_minutes?;
        if !self.clock_valid {
            return None;
        }
        let local = self
            .datetime
            .to_primitive()
            .ok()?
            .checked_add(Duration::minutes(i64::from(offset)))?;
        Some(DateTime::from_primitive(local))
    }
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
    let offset = rtc.read_ram_byte().await?;
    Ok(Snapshot {
        datetime: DateTime::from_primitive(datetime),
        clock_valid: seconds & OSCILLATOR_STOP_FLAG == 0,
        utc_offset_minutes: decode_utc_offset(offset),
    })
}

/// Store a host-supplied UTC instant and local offset, then read both back.
///
/// Invalidating the RAM marker before touching the calendar makes an
/// interrupted write fail closed: old local time can never be mistaken for
/// newly synchronized UTC.
pub async fn synchronize<I2C, E>(
    rtc: &mut PCF85063<I2C>,
    sync: ClockSync,
) -> Result<Snapshot, Error<E>>
where
    I2C: embedded_hal_async::i2c::I2c<Error = E>,
{
    let encoded_offset =
        encode_utc_offset(sync.utc_offset_minutes).ok_or(Error::InvalidInputData)?;
    let utc = OffsetDateTime::from_unix_timestamp(sync.unix_seconds)?;
    if !(2000..=2099).contains(&utc.year()) {
        return Err(Error::InvalidInputData);
    }
    let datetime = PrimitiveDateTime::new(utc.date(), utc.time());

    rtc.write_ram_byte(0).await?;
    rtc.set_datetime(&datetime).await?;
    rtc.write_ram_byte(encoded_offset).await?;

    let snapshot = read(rtc).await?;
    let Some(observed) = snapshot.unix_timestamp() else {
        return Err(Error::InvalidInputData);
    };
    if snapshot.utc_offset_minutes != Some(sync.utc_offset_minutes)
        || observed.abs_diff(sync.unix_seconds) > 1
    {
        return Err(Error::InvalidInputData);
    }
    Ok(snapshot)
}

fn encode_utc_offset(minutes: i16) -> Option<u8> {
    if !(MIN_UTC_OFFSET_MINUTES..=MAX_UTC_OFFSET_MINUTES).contains(&minutes)
        || minutes % OFFSET_MINUTES_PER_UNIT != 0
    {
        return None;
    }

    let units = (minutes - MIN_UTC_OFFSET_MINUTES) / OFFSET_MINUTES_PER_UNIT;
    Some(OFFSET_FORMAT_MARKER | units as u8)
}

fn decode_utc_offset(encoded: u8) -> Option<i16> {
    if encoded & OFFSET_FORMAT_MARKER == 0 {
        return None;
    }
    let units = i16::from(encoded & !OFFSET_FORMAT_MARKER);
    let minutes = MIN_UTC_OFFSET_MINUTES + units * OFFSET_MINUTES_PER_UNIT;
    (minutes <= MAX_UTC_OFFSET_MINUTES).then_some(minutes)
}
