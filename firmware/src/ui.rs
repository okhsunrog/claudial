//! The Slint UI: generated types, the helpers that push state into them, and
//! the callback wiring back to the peripheral tasks.

// `slint::include_modules!` expands generated code that re-qualifies types,
// tripping unused_qualifications on spans no item-level allow can reach.
#![allow(unused_qualifications)]

extern crate alloc;

use alloc::format;
use claudial_icd::settings::DisplaySettings;
use slint::PhysicalPosition;
use slint::platform::software_renderer::MinimalSoftwareWindow;
use slint::platform::{PointerEventButton, WindowEvent};

use crate::events::{SettingsAction, SettingsChannel, TouchState};
use crate::rtc::Snapshot as RtcSnapshot;

slint::include_modules!();

/// Push the minute-resolution clock into diagnostics.
///
/// Hours and minutes only. Seconds would repaint the dial sixty times more
/// often than any of the data on it changes, and on a page carrying the ring
/// that repaint is the most expensive thing the firmware could do per second.
pub fn update_clock(ui: &MainWindow, snapshot: RtcSnapshot) {
    if let Some(datetime) = snapshot.local_datetime() {
        ui.set_clock(format!("{:02}:{:02}", datetime.hour, datetime.minute).into());
        ui.set_rtc_status("synced".into());
    } else {
        ui.set_clock("--:--".into());
        ui.set_rtc_status("needs host sync".into());
    }
}

pub fn update_settings(ui: &MainWindow, settings: DisplaySettings) {
    ui.set_brightness_percent(i32::from(settings.brightness_percent));
    ui.set_auto_dim_enabled(settings.auto_dim);
    ui.set_dim_on_usb(settings.dim_on_usb);
    ui.set_idle_timeout_seconds(i32::from(settings.idle_timeout_seconds));
    ui.set_dim_brightness_percent(i32::from(settings.dim_brightness_percent));
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
pub fn connect_callbacks(ui: &MainWindow, settings: &'static SettingsChannel) {
    ui.on_open_settings(move || {
        let _ = settings.try_send(SettingsAction::Open);
    });
    ui.on_close_settings(move || {
        let _ = settings.try_send(SettingsAction::Close);
    });
    ui.on_brightness_step(move |direction| {
        let _ = settings.try_send(SettingsAction::BrightnessStep(direction as i8));
    });
    ui.on_auto_dim_toggle(move || {
        let _ = settings.try_send(SettingsAction::ToggleAutoDim);
    });
    ui.on_dim_on_usb_toggle(move || {
        let _ = settings.try_send(SettingsAction::ToggleDimOnUsb);
    });
    ui.on_idle_timeout_step(move |direction| {
        let _ = settings.try_send(SettingsAction::IdleTimeoutStep(direction as i8));
    });
    ui.on_dim_brightness_step(move |direction| {
        let _ = settings.try_send(SettingsAction::DimBrightnessStep(direction as i8));
    });
    ui.on_power_off(move || {
        let _ = settings.try_send(SettingsAction::PowerOff);
    });
    ui.on_reboot(move || {
        let _ = settings.try_send(SettingsAction::Reboot);
    });
}
