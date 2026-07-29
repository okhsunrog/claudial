//! The Slint UI: generated types, the helpers that push state into them, and
//! the callback wiring back to the peripheral tasks.

extern crate alloc;

use alloc::format;
use slint::PhysicalPosition;
use slint::platform::software_renderer::MinimalSoftwareWindow;
use slint::platform::{PointerEventButton, WindowEvent};

use crate::co5300::MINIMUM_BRIGHTNESS_PERCENT;
use crate::events::{BrightnessSignal, PmicChannels, RtcChannels, TouchState};
use crate::rtc::{DateTime as RtcDateTime, Snapshot as RtcSnapshot};

slint::include_modules!();

/// Length of an editor month, falling back to 31 while the month field is
/// mid-edit and not yet a valid month number.
pub fn days_in_month(year: i32, month: i32) -> i32 {
    u8::try_from(month)
        .ok()
        .and_then(|month| time::Month::try_from(month).ok())
        .map_or(31, |month| {
            i32::from(time::util::days_in_month(month, year))
        })
}

pub fn wrap_step(value: i32, delta: i32, minimum: i32, maximum: i32) -> i32 {
    let next = value + delta;
    if next < minimum {
        maximum
    } else if next > maximum {
        minimum
    } else {
        next
    }
}

pub fn sync_rtc_editor(ui: &MainWindow, datetime: RtcDateTime) {
    ui.set_rtc_edit_year(i32::from(datetime.year));
    ui.set_rtc_edit_month(i32::from(datetime.month));
    ui.set_rtc_edit_day(i32::from(datetime.day));
    ui.set_rtc_edit_hour(i32::from(datetime.hour));
    ui.set_rtc_edit_minute(i32::from(datetime.minute));
    ui.set_rtc_edit_second(i32::from(datetime.second));
}

pub fn update_rtc_display(ui: &MainWindow, snapshot: RtcSnapshot) {
    let datetime = snapshot.datetime;
    ui.set_rtc_time(
        format!(
            "{:02}:{:02}:{:02}",
            datetime.hour, datetime.minute, datetime.second
        )
        .into(),
    );
    ui.set_rtc_date(
        format!(
            "{}-{:02}-{:02}",
            datetime.year, datetime.month, datetime.day
        )
        .into(),
    );
    ui.set_rtc_clock_valid(snapshot.clock_valid);
    ui.set_rtc_status(if snapshot.clock_valid {
        "Clock ready".into()
    } else {
        "Set date and time".into()
    });
    if !ui.get_rtc_edit_dirty() {
        sync_rtc_editor(ui, datetime);
    }
}

pub fn dispatch_touch_state(
    window: &MinimalSoftwareWindow,
    last_position: &mut Option<slint::LogicalPosition>,
    state: TouchState,
) {
    match state {
        TouchState::Pressed { x, y } => {
            let position =
                PhysicalPosition::new(i32::from(x), i32::from(y)).to_logical(window.scale_factor());
            let event = match last_position.replace(position) {
                Some(previous) if previous != position => WindowEvent::PointerMoved { position },
                Some(_) => return,
                None => WindowEvent::PointerPressed {
                    position,
                    button: PointerEventButton::Left,
                },
            };
            window.dispatch_event(event);
        }
        TouchState::Released => {
            if let Some(position) = last_position.take() {
                window.dispatch_event(WindowEvent::PointerReleased {
                    position,
                    button: PointerEventButton::Left,
                });
                window.dispatch_event(WindowEvent::PointerExited);
            }
        }
    }
}

/// Wire the UI's callbacks to the endpoints the peripheral tasks listen on.
pub fn connect_callbacks(
    ui: &MainWindow,
    brightness: &'static BrightnessSignal,
    pmic: &'static PmicChannels,
    rtc: &'static RtcChannels,
) {
    let ui_weak = ui.as_weak();
    ui.on_brightness_step(move |delta| {
        if let Some(ui) = ui_weak.upgrade() {
            let percent = (ui.get_brightness_percent() + delta)
                .clamp(i32::from(MINIMUM_BRIGHTNESS_PERCENT), 100);
            ui.set_brightness_percent(percent);
            brightness.signal(percent as u8);
        }
    });
    ui.on_power_off(|| pmic.power_off.signal(()));
    ui.on_reboot(|| esp_hal::system::software_reset());
    let ui_weak = ui.as_weak();
    ui.on_rtc_step(move |field, delta| {
        let Some(ui) = ui_weak.upgrade() else {
            return;
        };
        match field {
            0 => ui.set_rtc_edit_year((ui.get_rtc_edit_year() + delta).clamp(2000, 2099)),
            1 => ui.set_rtc_edit_month(wrap_step(ui.get_rtc_edit_month(), delta, 1, 12)),
            2 => {
                let max_day = days_in_month(ui.get_rtc_edit_year(), ui.get_rtc_edit_month());
                ui.set_rtc_edit_day(wrap_step(ui.get_rtc_edit_day(), delta, 1, max_day));
            }
            3 => ui.set_rtc_edit_hour(wrap_step(ui.get_rtc_edit_hour(), delta, 0, 23)),
            4 => ui.set_rtc_edit_minute(wrap_step(ui.get_rtc_edit_minute(), delta, 0, 59)),
            5 => ui.set_rtc_edit_second(wrap_step(ui.get_rtc_edit_second(), delta, 0, 59)),
            _ => return,
        }
        let max_day = days_in_month(ui.get_rtc_edit_year(), ui.get_rtc_edit_month());
        ui.set_rtc_edit_day(ui.get_rtc_edit_day().min(max_day));
        ui.set_rtc_edit_dirty(true);
        ui.set_rtc_status("Unsaved changes".into());
    });
    let ui_weak = ui.as_weak();
    ui.on_rtc_save(move || {
        let Some(ui) = ui_weak.upgrade() else {
            return;
        };
        let request = RtcDateTime {
            year: ui.get_rtc_edit_year() as u16,
            month: ui.get_rtc_edit_month() as u8,
            day: ui.get_rtc_edit_day() as u8,
            hour: ui.get_rtc_edit_hour() as u8,
            minute: ui.get_rtc_edit_minute() as u8,
            second: ui.get_rtc_edit_second() as u8,
        };
        if request.to_primitive().is_err() {
            ui.set_rtc_status("Invalid date".into());
            return;
        }
        ui.set_rtc_status("Saving...".into());
        rtc.set.signal(request);
    });
}
