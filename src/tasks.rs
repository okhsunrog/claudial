//! Peripheral tasks.
//!
//! Each task takes the channel group it publishes to as a parameter, so the
//! dataflow is visible in the signature rather than only in the body.

use cst92xx::CST92xx;
use defmt::{error, info};
use embassy_embedded_hal::shared_bus::asynch::i2c::I2cDevice;
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Instant, Ticker, Timer};
use esp_hal::gpio::{Input, Output};
use esp_hal::i2c::master::I2c;

use crate::board::{DISPLAY_HEIGHT, DISPLAY_WIDTH};
use crate::events::{PmicChannels, PmicEvent, RtcChannels, RtcEvent, TouchChannels, TouchState};
use crate::pmic;
use crate::rtc;

pub type SharedI2cBus = Mutex<NoopRawMutex, I2c<'static, esp_hal::Async>>;

/// How long the touch controller may stay silent mid-press before the press is
/// re-checked rather than trusted.
const TOUCH_RELEASE_TIMEOUT: Duration = Duration::from_millis(250);

fn display_coordinates(point: cst92xx::Point) -> (u16, u16) {
    // The official Waveshare demo applies setSwapXY(true) followed by
    // setMirrorXY(true, false) for the panel's 0-degree orientation.
    const MAX_X: u16 = DISPLAY_WIDTH - 1;
    const MAX_Y: u16 = DISPLAY_HEIGHT - 1;
    (MAX_X - point.y.min(MAX_X), point.x.min(MAX_Y))
}

#[embassy_executor::task]
#[allow(
    clippy::large_stack_frames,
    reason = "Embassy stores the async task state statically rather than on the runtime call stack"
)]
pub async fn touch_task(
    i2c_bus: &'static SharedI2cBus,
    channels: &'static TouchChannels,
    mut interrupt: Input<'static>,
    mut reset: Output<'static>,
) {
    reset.set_low();
    Timer::after(Duration::from_millis(10)).await;
    reset.set_high();
    Timer::after(Duration::from_millis(30)).await;

    let mut touch = CST92xx::new(I2cDevice::new(i2c_bus));
    if touch.init().await.is_err() {
        error!("CST92xx initialization failed");
        channels.ready.signal(false);
        return;
    }
    info!("{} touch controller initialized", touch.model_name());
    channels.ready.signal(true);

    let mut pressed = false;
    loop {
        // CST9220 presents one report per falling edge. Reading repeatedly
        // while INT is still low races the controller's ACK processing and can
        // turn a valid press into an immediate empty report.
        if pressed {
            // A dropped release edge would latch the UI in a pressed state
            // forever. Once the controller has been quiet for the whole
            // timeout, INT is idle, so re-reading cannot race a report: ask it
            // directly whether the finger is still down instead of guessing.
            if let Either::Second(()) = select(
                interrupt.wait_for_falling_edge(),
                Timer::after(TOUCH_RELEASE_TIMEOUT),
            )
            .await
            {
                if matches!(touch.touches().await, Ok(points) if points[0].is_some()) {
                    continue;
                }
                error!("CST92xx release edge missed; synthesizing release");
                pressed = false;
                channels.events.send(TouchState::Released).await;
                continue;
            }
        } else {
            interrupt.wait_for_falling_edge().await;
        }

        match touch.touches().await {
            Ok(points) => {
                if let Some(point) = points[0] {
                    let (x, y) = display_coordinates(point);
                    pressed = true;
                    channels.events.send(TouchState::Pressed { x, y }).await;
                } else {
                    pressed = false;
                    channels.events.send(TouchState::Released).await;
                }
            }
            Err(_) => {
                error!("CST92xx read failed");
                pressed = false;
                channels.events.send(TouchState::Released).await;
            }
        }
    }
}

#[embassy_executor::task]
#[allow(
    clippy::large_stack_frames,
    reason = "Embassy stores the async task state statically rather than on the runtime call stack"
)]
pub async fn pmic_task(i2c_bus: &'static SharedI2cBus, channels: &'static PmicChannels) {
    let mut axp = match pmic::init(I2cDevice::new(i2c_bus)).await {
        Ok(axp) => axp,
        Err(_) => {
            error!("AXP2101 initialization failed");
            channels.stats.signal(PmicEvent::Error);
            return;
        }
    };
    info!("AXP2101 initialized for telemetry and power-key events");

    let mut first_sample = true;
    let mut last_sample = Instant::now();
    loop {
        if channels.power_off.try_take().is_some() {
            info!("Power off requested");
            if axp.power_off().await.is_err() {
                error!("AXP2101 power off failed");
            }
        }

        match pmic::poll_power_key(&mut axp).await {
            Ok(Some(event)) => {
                info!("AXP2101 power key: {}", event);
                channels.power_key.signal(event);
            }
            Ok(None) => {}
            Err(_) => error!("AXP2101 power-key poll failed"),
        }

        if first_sample || last_sample.elapsed() >= Duration::from_secs(1) {
            last_sample = Instant::now();
            match pmic::read_stats(&mut axp).await {
                Ok(stats) => {
                    if first_sample {
                        first_sample = false;
                        info!("AXP2101 first telemetry sample: {}", stats);
                    }
                    channels.stats.signal(PmicEvent::Online(stats));
                }
                Err(_) => {
                    error!("AXP2101 telemetry read failed");
                    channels.stats.signal(PmicEvent::Error);
                }
            }
        }

        // This interval is the PWR button's latency budget, not the telemetry
        // rate: the sample above is separately gated to one second, and the
        // AXP2101 latches key status so a press is only ever delayed, never
        // lost. It stays short because the button should feel immediate;
        // stretching it saves almost nothing, since one register read costs
        // far less than the wakeup itself.
        Timer::after(Duration::from_millis(200)).await;
    }
}

#[embassy_executor::task]
#[allow(
    clippy::large_stack_frames,
    reason = "Embassy stores the async task state statically rather than on the runtime call stack"
)]
pub async fn rtc_task(i2c_bus: &'static SharedI2cBus, channels: &'static RtcChannels) {
    let (mut clock, clock_valid) = match rtc::init(I2cDevice::new(i2c_bus)).await {
        Ok(result) => result,
        Err(_) => {
            error!("PCF85063 initialization failed");
            channels.snapshot.signal(RtcEvent::Error);
            return;
        }
    };
    info!("PCF85063 initialized; clock valid: {}", clock_valid);

    match rtc::read(&mut clock).await {
        Ok(snapshot) => {
            info!(
                "RTC time: {}-{:02}-{:02} {:02}:{:02}:{:02}",
                snapshot.datetime.year,
                snapshot.datetime.month,
                snapshot.datetime.day,
                snapshot.datetime.hour,
                snapshot.datetime.minute,
                snapshot.datetime.second
            );
            channels.snapshot.signal(RtcEvent::Online(snapshot));
        }
        Err(_) => channels.snapshot.signal(RtcEvent::NeedsSetting),
    }

    let mut ticker = Ticker::every(Duration::from_secs(1));
    loop {
        // Either a set request arrives, or the calendar is due to be re-read.
        // Waiting on both beats polling the request signal on a short timer.
        let request = match select(channels.set.wait(), ticker.next()).await {
            Either::First(request) => request,
            Either::Second(()) => {
                match rtc::read(&mut clock).await {
                    Ok(snapshot) => channels.snapshot.signal(RtcEvent::Online(snapshot)),
                    Err(_) => {
                        error!("PCF85063 read failed");
                        channels.snapshot.signal(RtcEvent::Error);
                    }
                }
                continue;
            }
        };

        {
            let result = match request.to_primitive() {
                Ok(datetime) => clock.set_datetime(&datetime).await,
                Err(_) => {
                    error!("Rejected invalid RTC date/time");
                    channels.snapshot.signal(RtcEvent::SaveFailed);
                    continue;
                }
            };

            match result {
                Ok(()) => match rtc::read(&mut clock).await {
                    Ok(snapshot) if snapshot.clock_valid => {
                        info!(
                            "RTC set and verified: {}-{:02}-{:02} {:02}:{:02}:{:02}",
                            snapshot.datetime.year,
                            snapshot.datetime.month,
                            snapshot.datetime.day,
                            snapshot.datetime.hour,
                            snapshot.datetime.minute,
                            snapshot.datetime.second
                        );
                        channels.snapshot.signal(RtcEvent::Saved(snapshot));
                        // The clock was just read, so restart the interval
                        // rather than re-reading a moment later.
                        ticker.reset();
                    }
                    _ => {
                        error!("PCF85063 write verification failed");
                        channels.snapshot.signal(RtcEvent::SaveFailed);
                    }
                },
                Err(_) => {
                    error!("PCF85063 write failed");
                    channels.snapshot.signal(RtcEvent::SaveFailed);
                }
            }
        }
    }
}
