//! The Slint UI: generated types, the helpers that push state into them, and
//! the callback wiring back to the peripheral tasks.

// `slint::include_modules!` expands generated code that re-qualifies types,
// tripping unused_qualifications on spans no item-level allow can reach.
#![allow(unused_qualifications)]

extern crate alloc;

use alloc::rc::Rc;
use alloc::{format, vec};
use slint::platform::software_renderer::MinimalSoftwareWindow;
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{Model, PhysicalPosition, VecModel};

use crate::co5300::MINIMUM_BRIGHTNESS_PERCENT;
use crate::events::{BrightnessSignal, PmicChannels, TouchState};
use crate::rtc::Snapshot as RtcSnapshot;
use claudial_icd::history::{BUCKETS, History};

slint::include_modules!();

/// Push the clock into the status line.
///
/// Hours and minutes only. Seconds would repaint the dial sixty times more
/// often than any of the data on it changes, and on a page carrying the ring
/// that repaint is the most expensive thing the firmware could do per second.
pub fn update_clock(ui: &MainWindow, snapshot: RtcSnapshot) {
    let datetime = snapshot.datetime;
    if snapshot.clock_valid {
        ui.set_clock(format!("{:02}:{:02}", datetime.hour, datetime.minute).into());
        ui.set_rtc_status("synced".into());
    } else {
        ui.set_clock("--:--".into());
        ui.set_rtc_status("waiting for host".into());
    }
}

/// Backing model for the sparkline: one normalised height per bucket.
pub fn history_model() -> Rc<VecModel<f32>> {
    Rc::new(VecModel::from(vec![0.0_f32; BUCKETS]))
}

/// Push fresh history into the sparkline, writing only the bars that moved.
///
/// The comparison matters for the same reason it did for the sprite grid this
/// replaces: assigning every bar unconditionally would mark all sixty
/// rectangles dirty, so a quiet minute would repaint the whole chart.
pub fn push_history(model: &VecModel<f32>, history: &History) {
    for (i, value) in history.buckets().into_iter().enumerate() {
        if model.row_data(i) != Some(value) {
            model.set_row_data(i, value);
        }
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
}
