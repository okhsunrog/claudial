//! AXP2101 telemetry for the Waveshare ESP32-S3 Touch AMOLED 2.16.
//!
//! The AXP2101 shares the board's I2C bus with the CST9220 touch controller.
//! Unlike the M5Stack Core2, display brightness is not supplied by a PMIC LDO:
//! it is controlled by the CO5300's `0x51` command. This module deliberately
//! avoids changing board-specific power rails and only enables battery
//! detection plus the ADC channels used for telemetry.

use axp2101_dd::{AdcChannel, Axp2101Async, AxpError, AxpInterface};

#[derive(Clone, Copy, Debug, Default, defmt::Format)]
pub struct PmicStats {
    pub battery_mv: u16,
    pub vbus_mv: u16,
    pub vsys_mv: u16,
    pub temperature_c: f32,
    pub state_of_charge: u8,
    pub battery_present: bool,
    pub charging: bool,
    pub vbus_good: bool,
}

pub async fn init<I2C, E>(i2c: I2C) -> Result<Axp2101Async<AxpInterface<I2C>, E>, AxpError<E>>
where
    I2C: embedded_hal_async::i2c::I2c<Error = E>,
    E: core::fmt::Debug,
{
    let mut axp = Axp2101Async::new(i2c);

    // Verify that the expected PMIC responds before changing any registers.
    let chip_id = axp.get_chip_id().await?;
    defmt::info!("AXP2101 chip ID: 0x{:02X}", chip_id);
    if chip_id != 0x4a && chip_id != 0x47 {
        defmt::warn!("Unexpected AXP2101 chip ID: 0x{:02X}", chip_id);
    }

    axp.ll
        .battery_detection_control()
        .modify_async(|register| register.set_bat_det_en(true))
        .await?;

    for channel in [
        AdcChannel::BatteryVoltage,
        AdcChannel::VbusVoltage,
        AdcChannel::VsysVoltage,
        AdcChannel::DieTemperature,
    ] {
        axp.set_adc_channel_enable(channel, true).await?;
    }

    Ok(axp)
}

pub async fn read_stats<I2C, E>(
    axp: &mut Axp2101Async<AxpInterface<I2C>, E>,
) -> Result<PmicStats, AxpError<E>>
where
    I2C: embedded_hal_async::i2c::I2c<Error = E>,
    E: core::fmt::Debug,
{
    let battery_present = axp.is_battery_present().await?;
    let vbus_good = axp.is_vbus_good().await?;

    Ok(PmicStats {
        battery_mv: if battery_present {
            axp.get_battery_voltage_mv().await?
        } else {
            0
        },
        vbus_mv: if vbus_good {
            axp.get_vbus_voltage_mv().await?
        } else {
            0
        },
        vsys_mv: axp.get_vsys_voltage_mv().await?,
        temperature_c: axp.get_die_temperature_c().await?,
        state_of_charge: if battery_present {
            axp.get_battery_level().await?
        } else {
            0
        },
        battery_present,
        charging: axp.is_charging().await?,
        vbus_good,
    })
}
