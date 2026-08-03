//! Events flowing from the peripheral tasks to the UI loop, and the endpoints
//! they travel over.

use embassy_futures::select::{Either, Either4, select, select4};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Ticker, Timer};

use claudial_icd::{ClockSync, UsageSnapshot};

use crate::pmic::{PmicStats, PowerKey};
use crate::rtc::Snapshot as RtcSnapshot;

pub type SettingsChannel = Channel<CriticalSectionRawMutex, SettingsAction, 8>;
pub type BleSignal = Signal<CriticalSectionRawMutex, BleState>;
/// Latest usage snapshot pushed by the host daemon.
pub type UsageSignal = Signal<CriticalSectionRawMutex, UsageSnapshot>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BleState {
    Advertising,
    Connected,
    Error,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TouchState {
    Released,
    Pressed { x: u16, y: u16 },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsAction {
    Open,
    Close,
    BrightnessStep(i8),
    ToggleAutoDim,
    ToggleDimOnUsb,
    IdleTimeoutStep(i8),
    DimBrightnessStep(i8),
    PowerOff,
    Reboot,
}

#[derive(Clone, Copy)]
pub enum PmicEvent {
    Online(PmicStats),
    Error,
}

#[derive(Clone, Copy)]
pub enum RtcEvent {
    Online(RtcSnapshot),
    Synced(RtcSnapshot),
    SyncFailed,
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

/// What the RTC task publishes, plus the host sync it consumes.
pub struct RtcChannels {
    pub snapshot: Signal<CriticalSectionRawMutex, RtcEvent>,
    pub sync: Signal<CriticalSectionRawMutex, ClockSync>,
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
            sync: Signal::new(),
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
    Settings(SettingsAction),
    Ble(BleState),
    Maintenance,
    Animation,
    /// Nobody has touched the panel for a while; dim it.
    Idle,
    /// The host pushed a fresh usage snapshot.
    Usage(UsageSnapshot),
}

/// Every endpoint the UI loop waits on, bundled so the wait keeps a readable
/// signature as sources are added.
pub struct UiChannels {
    pub touch: &'static TouchChannels,
    pub pmic: &'static PmicChannels,
    pub rtc: &'static RtcChannels,
    pub settings: &'static SettingsChannel,
    pub ble: &'static BleSignal,
    pub usage: &'static UsageSignal,
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
    maintenance: &mut Ticker,
    animation: Option<core::time::Duration>,
    idle_deadline: Option<Instant>,
) -> UiEvent {
    let UiChannels {
        touch,
        pmic,
        rtc,
        settings,
        ble,
        usage,
    } = channels;
    let peripherals = select4(
        async { UiEvent::PowerKey(pmic.power_key.wait().await) },
        async { UiEvent::TouchReady(touch.ready.wait().await) },
        async { UiEvent::Touch(touch.events.receive().await) },
        async { UiEvent::Pmic(pmic.stats.wait().await) },
    );
    let rest = select4(
        async { UiEvent::Rtc(rtc.snapshot.wait().await) },
        async { UiEvent::Settings(settings.receive().await) },
        async {
            maintenance.next().await;
            UiEvent::Maintenance
        },
        async {
            // No animation running means there is no deadline at all, so the
            // loop sleeps until a peripheral or maintenance tick wakes it.
            match animation {
                Some(remaining) => {
                    Timer::after(Duration::from_micros(remaining.as_micros() as u64)).await;
                }
                None => core::future::pending::<()>().await,
            }
            UiEvent::Animation
        },
    );

    // The idle timeout is the one deadline that must be absolute. Every touch
    // pushes it further out, and this future is rebuilt whenever any other
    // event wins the select — a relative timer would restart the countdown on
    // every maintenance tick and the panel would never dim. Already dimmed means
    // there is no deadline, so this branch just sleeps.
    let idle = async {
        match idle_deadline {
            Some(deadline) => Timer::at(deadline).await,
            None => core::future::pending::<()>().await,
        }
        UiEvent::Idle
    };

    let data = async {
        match select(ble.wait(), usage.wait()).await {
            Either::First(state) => UiEvent::Ble(state),
            Either::Second(snapshot) => UiEvent::Usage(snapshot),
        }
    };

    match select4(peripherals, rest, idle, data).await {
        Either4::First(event) | Either4::Second(event) => match event {
            Either4::First(event)
            | Either4::Second(event)
            | Either4::Third(event)
            | Either4::Fourth(event) => event,
        },
        Either4::Third(event) | Either4::Fourth(event) => event,
    }
}
