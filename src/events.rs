//! Events flowing from the peripheral tasks to the UI loop, and the endpoints
//! they travel over.

use embassy_futures::select::{Either, Either3, Either4, select, select3, select4};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Ticker, Timer};

use crate::pmic::{PmicStats, PowerKey};
use crate::rtc::{DateTime as RtcDateTime, Snapshot as RtcSnapshot};

pub type BrightnessSignal = Signal<CriticalSectionRawMutex, u8>;
/// Raised by the UI when the sprite page is tapped.
pub type SpriteSignal = Signal<CriticalSectionRawMutex, ()>;

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
    /// The current sprite frame has been held long enough.
    SpriteFrame,
    /// The user asked for the next sprite animation.
    SpriteNext,
}

/// Every endpoint the UI loop waits on, bundled so the wait keeps a readable
/// signature as sources are added.
pub struct UiChannels {
    pub touch: &'static TouchChannels,
    pub pmic: &'static PmicChannels,
    pub rtc: &'static RtcChannels,
    pub brightness: &'static BrightnessSignal,
    pub sprite_next: &'static SpriteSignal,
}

/// Wait until one of the things the UI cares about happens.
///
/// Each branch resolves straight to a [`UiEvent`], so the nested `select`
/// results collapse with a single or-pattern instead of a match over every
/// coordinate.
///
/// Cancellation is the thing to be careful about: whichever branch wins, every
/// other future is dropped, and this whole wait is rebuilt from scratch on the
/// next iteration. Signals, channels and tickers survive that — a cancelled
/// `Signal::wait` leaves its value latched, a dropped `Channel::receive`
/// consumes nothing, and `Ticker` keeps its own deadline. A relative
/// `Timer::after` does not: it starts counting again from zero.
///
/// So any deadline that has to survive cancellation is passed in as an
/// absolute [`Instant`]. The Slint animation deadline is the one exception
/// that may stay relative, because the caller recomputes it from Slint's own
/// schedule immediately before every call.
pub async fn next_ui_event(
    channels: &UiChannels,
    uptime: &mut Ticker,
    animation: Option<core::time::Duration>,
    sprite_deadline: Option<Instant>,
) -> UiEvent {
    let UiChannels {
        touch,
        pmic,
        rtc,
        brightness,
        sprite_next,
    } = channels;
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

    // The sprite page is the only source with a data-dependent deadline. Keep
    // it absolute: this future is recreated whenever any other event wins the
    // select, and a relative timer would restart the frame hold every time.
    // When the page is not showing there is no deadline, and only a tap can
    // wake this branch.
    let sprite = async {
        match sprite_deadline {
            Some(deadline) => match select(Timer::at(deadline), sprite_next.wait()).await {
                Either::First(()) => UiEvent::SpriteFrame,
                Either::Second(()) => UiEvent::SpriteNext,
            },
            None => {
                sprite_next.wait().await;
                UiEvent::SpriteNext
            }
        }
    };

    match select3(peripherals, rest, sprite).await {
        Either3::First(event) | Either3::Second(event) => match event {
            Either4::First(event)
            | Either4::Second(event)
            | Either4::Third(event)
            | Either4::Fourth(event) => event,
        },
        Either3::Third(event) => event,
    }
}
