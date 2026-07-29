//! Events flowing from the peripheral tasks to the UI loop, and the endpoints
//! they travel over.

use embassy_futures::select::{Either, Either4, select, select4};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Ticker, Timer};

use crate::pmic::{PmicStats, PowerKey};
use crate::rtc::{DateTime as RtcDateTime, Snapshot as RtcSnapshot};

pub type BrightnessSignal = Signal<CriticalSectionRawMutex, u8>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TouchState {
    Released,
    Pressed { x: u16, y: u16 },
}

#[derive(Clone, Copy)]
pub enum PmicEvent {
    Online(PmicStats),
    Error,
}

#[derive(Clone, Copy)]
pub enum RtcEvent {
    Online(RtcSnapshot),
    NeedsSetting,
    Saved(RtcSnapshot),
    SaveFailed,
    Error,
}

/// What the touch task publishes.
pub struct TouchChannels {
    pub events: Channel<CriticalSectionRawMutex, TouchState, 8>,
    pub ready: Signal<CriticalSectionRawMutex, bool>,
}

/// What the PMIC task publishes, plus the power-off request it consumes.
pub struct PmicChannels {
    pub stats: Signal<CriticalSectionRawMutex, PmicEvent>,
    pub power_key: Signal<CriticalSectionRawMutex, PowerKey>,
    pub power_off: Signal<CriticalSectionRawMutex, ()>,
}

/// What the RTC task publishes, plus the set request it consumes.
pub struct RtcChannels {
    pub snapshot: Signal<CriticalSectionRawMutex, RtcEvent>,
    pub set: Signal<CriticalSectionRawMutex, RtcDateTime>,
}

#[allow(
    clippy::new_without_default,
    reason = "const-initialized into statics, where Default cannot be used"
)]
impl TouchChannels {
    pub const fn new() -> Self {
        Self {
            events: Channel::new(),
            ready: Signal::new(),
        }
    }
}

#[allow(
    clippy::new_without_default,
    reason = "const-initialized into statics, where Default cannot be used"
)]
impl PmicChannels {
    pub const fn new() -> Self {
        Self {
            stats: Signal::new(),
            power_key: Signal::new(),
            power_off: Signal::new(),
        }
    }
}

#[allow(
    clippy::new_without_default,
    reason = "const-initialized into statics, where Default cannot be used"
)]
impl RtcChannels {
    pub const fn new() -> Self {
        Self {
            snapshot: Signal::new(),
            set: Signal::new(),
        }
    }
}

/// Something the UI loop needs to wake up for.
pub enum UiEvent {
    PowerKey(PowerKey),
    TouchReady(bool),
    Touch(TouchState),
    Pmic(PmicEvent),
    Rtc(RtcEvent),
    Brightness(u8),
    Uptime,
    Animation,
}

/// Wait until one of the things the UI cares about happens.
///
/// Each branch resolves straight to a [`UiEvent`], so the nested `select`
/// results collapse with a single or-pattern instead of a match over every
/// coordinate. Nothing is swallowed by a losing branch either: a cancelled
/// `Signal::wait` leaves its value latched, a dropped `Channel::receive`
/// consumes nothing, and a dropped `Ticker::next` keeps its deadline.
pub async fn next_ui_event(
    touch: &'static TouchChannels,
    pmic: &'static PmicChannels,
    rtc: &'static RtcChannels,
    brightness: &'static BrightnessSignal,
    uptime: &mut Ticker,
    animation: Option<core::time::Duration>,
) -> UiEvent {
    let peripherals = select4(
        async { UiEvent::PowerKey(pmic.power_key.wait().await) },
        async { UiEvent::TouchReady(touch.ready.wait().await) },
        async { UiEvent::Touch(touch.events.receive().await) },
        async { UiEvent::Pmic(pmic.stats.wait().await) },
    );
    let rest = select4(
        async { UiEvent::Rtc(rtc.snapshot.wait().await) },
        async { UiEvent::Brightness(brightness.wait().await) },
        async {
            uptime.next().await;
            UiEvent::Uptime
        },
        async {
            // No animation running means there is no deadline at all, so the
            // loop sleeps until a peripheral or the uptime tick wakes it.
            match animation {
                Some(remaining) => {
                    Timer::after(Duration::from_micros(remaining.as_micros() as u64)).await
                }
                None => core::future::pending::<()>().await,
            }
            UiEvent::Animation
        },
    );

    match select(peripherals, rest).await {
        Either::First(event) | Either::Second(event) => match event {
            Either4::First(event)
            | Either4::Second(event)
            | Either4::Third(event)
            | Either4::Fourth(event) => event,
        },
    }
}
