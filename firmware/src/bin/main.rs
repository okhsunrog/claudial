#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use alloc::boxed::Box;
use alloc::format;
use claudial_firmware::ble::{ble_task, clock_sync_task, usage_task};
use claudial_firmware::board::{DISPLAY_HEIGHT, DISPLAY_SPI_MHZ, DISPLAY_WIDTH};
use claudial_firmware::co5300::{self, Co5300, brightness_register};
use claudial_firmware::events::{
    BleSignal, BleState, PmicChannels, PmicEvent, RtcChannels, RtcEvent, SettingsAction,
    SettingsChannel, TouchChannels, TouchState, UiChannels, UiEvent, UsageSignal, next_ui_event,
};
use claudial_firmware::frame_stats::{FrameStats, FrameTiming};
use claudial_firmware::pmic::PowerKey;
use claudial_firmware::rtc::Snapshot as RtcSnapshot;
use claudial_firmware::settings_store::SettingsStore;
use claudial_firmware::slint_platform::EspPlatform;
use claudial_firmware::tasks::{SharedI2cBus, pmic_task, rtc_task, touch_task};
use claudial_firmware::transport::{BLE_MTU, BLE_OUTQ, Stack};
use claudial_firmware::ui::{
    self, MainWindow, dispatch_touch_state, update_clock, update_settings,
};
use claudial_icd::pace::Pace;
use claudial_icd::settings::DisplaySettings;
use claudial_icd::{UsageSnapshot, UsageStatus, minutes_until};
use defmt::{error, info};
use embassy_executor::Spawner;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Instant, Ticker, Timer};
use ergot::NetStack;
use ergot::interface_manager::profiles::direct_edge::DirectEdge;
use ergot::interface_manager::utils::framed_stream;
use esp_hal::clock::CpuClock;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::spi::Mode;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::{Instant as HalInstant, Rate};
use esp_hal::timer::timg::TimerGroup;
use esp_radio::ble::controller::BleConnector;
use panic_rtt_target as _;
use slint::platform::software_renderer::{
    DirtyRegionAlignment, MinimalSoftwareWindow, RepaintBufferType,
};
use static_cell::StaticCell;

extern crate alloc;

// These live in statics so spawned tasks can be handed `&'static` references,
// but every task takes the group it uses as a parameter, so this is the only
// place the wiring is decided.
static TOUCH: TouchChannels = TouchChannels::new();
static PMIC: PmicChannels = PmicChannels::new();
static RTC: RtcChannels = RtcChannels::new();
static SETTINGS: SettingsChannel = SettingsChannel::new();
static BLE: BleSignal = BleSignal::new();
static USAGE: UsageSignal = UsageSignal::new();

/// Two missed host polls make the displayed data stale.
const DATA_FRESHNESS_TIMEOUT: Duration = Duration::from_secs(150);
/// Whole-percent usage staying unchanged this long is worth saying explicitly.
const USAGE_UNCHANGED_SECONDS: u32 = 5 * 60;
/// Coalesce a burst of settings taps into one append-only flash write.
const SETTINGS_SAVE_DEBOUNCE: Duration = Duration::from_secs(2);

/// Render a compact reset countdown, or `--` when the host has nothing.
fn format_reset(minutes: u16) -> alloc::string::String {
    if minutes == 0 {
        return "--".into();
    }
    if minutes >= 24 * 60 {
        return format!("{}d {}h", minutes / (24 * 60), minutes % (24 * 60) / 60);
    }
    if minutes >= 60 {
        return format!("{}h {:02}m", minutes / 60, minutes % 60);
    }
    format!("{minutes}m")
}

fn update_data_state(ui: &MainWindow, ble_connected: bool, last_usage_at: Option<Instant>) {
    ui.set_ble_connected(ble_connected);
    ui.set_data_fresh(
        ble_connected
            && last_usage_at.is_some_and(|received| received.elapsed() < DATA_FRESHNESS_TIMEOUT),
    );
}

fn format_usage_age(received: Instant) -> alloc::string::String {
    let seconds = received.elapsed().as_secs();
    if seconds < 60 {
        return "JUST NOW".into();
    }
    if seconds < 60 * 60 {
        return format!("{}M AGO", seconds / 60);
    }
    if seconds < 24 * 60 * 60 {
        return format!("{}H AGO", seconds / (60 * 60));
    }
    format!("{}D AGO", seconds / (24 * 60 * 60))
}

fn update_pace(
    ui: &MainWindow,
    pace: &Pace,
    snapshot: UsageSnapshot,
    session_reset_mins: u16,
    ble_connected: bool,
    last_usage_at: Option<Instant>,
) {
    if !ble_connected {
        ui.set_pace_summary(
            last_usage_at
                .map(|received| format!("HOST OFFLINE · {}", format_usage_age(received)))
                .unwrap_or_else(|| "HOST OFFLINE".into())
                .into(),
        );
        ui.set_pace_warning(false);
        return;
    }

    let Some(last_usage_at) = last_usage_at else {
        ui.set_pace_summary("WAITING FOR USAGE".into());
        ui.set_pace_warning(false);
        return;
    };

    if last_usage_at.elapsed() >= DATA_FRESHNESS_TIMEOUT {
        ui.set_pace_summary(format!("USAGE STALE · {}", format_usage_age(last_usage_at)).into());
        ui.set_pace_warning(false);
        return;
    }

    if snapshot.status == UsageStatus::Limited || snapshot.session_pct >= 100 {
        ui.set_pace_summary("LIMIT REACHED".into());
        ui.set_pace_warning(true);
        return;
    }

    let Some(rate) = pace.rate_per_hour() else {
        let minutes = pace.readiness_remaining_seconds().div_ceil(60);
        ui.set_pace_summary(format!("PACE READY IN {minutes}M").into());
        ui.set_pace_warning(false);
        return;
    };

    let unchanged_seconds = pace.unchanged_seconds();
    if unchanged_seconds >= USAGE_UNCHANGED_SECONDS {
        ui.set_pace_summary(format!("USAGE UNCHANGED · {}M", unchanged_seconds / 60).into());
        ui.set_pace_warning(false);
        return;
    }
    if rate == 0 {
        ui.set_pace_summary("USAGE UNCHANGED".into());
        ui.set_pace_warning(false);
        return;
    }
    if session_reset_mins == 0 || snapshot.status == UsageStatus::Unknown {
        ui.set_pace_summary(format!("PACE {rate}%/h").into());
        ui.set_pace_warning(false);
        return;
    }

    let remaining = u32::from(100_u8.saturating_sub(snapshot.session_pct));
    let minutes_to_exhaust = remaining * 60 / u32::from(rate);
    let exhausts_early = minutes_to_exhaust < u32::from(session_reset_mins);
    ui.set_pace_summary(
        if exhausts_early {
            format!("PACE {rate}%/h · TOO FAST")
        } else {
            format!("PACE {rate}%/h · ON TRACK")
        }
        .into(),
    );
    ui.set_pace_warning(exhausts_early);
}

fn update_time_dependent_usage(
    ui: &MainWindow,
    pace: &Pace,
    usage: UsageSnapshot,
    rtc: Option<RtcSnapshot>,
    ble_connected: bool,
    last_usage_at: Option<Instant>,
) {
    let now = rtc.and_then(RtcSnapshot::unix_timestamp);
    let session_reset_mins = now
        .map(|now| minutes_until(usage.session_reset_at, now))
        .unwrap_or(0);
    let weekly_reset_mins = now
        .map(|now| minutes_until(usage.weekly_reset_at, now))
        .unwrap_or(0);

    update_pace(
        ui,
        pace,
        usage,
        session_reset_mins,
        ble_connected,
        last_usage_at,
    );
    ui.set_usage_session_reset(format_reset(session_reset_mins).into());
    ui.set_usage_weekly_reset(format_reset(weekly_reset_mins).into());
}

fn update_usage_state(
    ui: &MainWindow,
    pace: &Pace,
    usage: UsageSnapshot,
    rtc: Option<RtcSnapshot>,
    ble_connected: bool,
    last_usage_at: Option<Instant>,
) {
    update_data_state(ui, ble_connected, last_usage_at);
    update_time_dependent_usage(ui, pace, usage, rtc, ble_connected, last_usage_at);
}

fn next_idle_deadline(settings: DisplaySettings, usb_connected: bool) -> Option<Instant> {
    settings
        .should_dim(usb_connected)
        .then(|| Instant::now() + Duration::from_secs(u64::from(settings.idle_timeout_seconds)))
}

async fn persist_settings(
    store: &mut Option<SettingsStore<'_>>,
    ui: &MainWindow,
    settings: DisplaySettings,
) {
    let Some(store) = store.as_mut() else {
        ui.set_settings_storage_status("storage unavailable".into());
        return;
    };

    // The sequential-storage state machine is large and saving is rare. Keep
    // it out of the permanently resident UI-loop future.
    match Box::pin(store.save(settings)).await {
        Ok(true) => {
            info!("Display settings saved and verified");
            ui.set_settings_storage_status("saved".into());
        }
        Ok(false) => ui.set_settings_storage_status("saved".into()),
        Err(error) => {
            error!("Display settings save failed: {}", error);
            ui.set_settings_storage_status("save failed".into());
        }
    }
}

async fn wake_display(
    display: &mut Co5300<'_>,
    window: &MinimalSoftwareWindow,
    brightness_percent: u8,
) -> bool {
    if display.wake().is_err() {
        error!("CO5300 sleep-out failed");
        return false;
    }

    Timer::after(Duration::from_millis(120)).await;
    if display.display_on().is_err() {
        error!("CO5300 display-on failed");
        return false;
    }
    if display
        .set_brightness(brightness_register(brightness_percent))
        .is_err()
    {
        error!("CO5300 brightness restore failed");
    }

    window.request_redraw();
    true
}

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    rtt_target::rtt_init_defmt!();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // Slint's repeated dial elements are allocation-heavy, while the ESP BLE
    // controller can only allocate from internal RAM. Keep enough internal
    // heap for both instead of letting the UI consume the radio's headroom.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);
    esp_alloc::heap_allocator!(size: 48 * 1024);
    esp_alloc::psram_allocator!(
        peripherals.PSRAM,
        esp_hal::psram,
        esp_hal::psram::PsramConfig {
            mode: esp_hal::psram::PsramMode::OctalSpi,
            ..Default::default()
        }
    );

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    info!("Embassy initialized!");

    let (mut settings_store, mut display_settings, settings_storage_status) =
        match Box::pin(SettingsStore::new(peripherals.FLASH)).await {
            Ok((store, Some(settings))) => {
                info!("Loaded persisted display settings");
                (Some(store), settings, "saved")
            }
            Ok((mut store, None)) => {
                let defaults = DisplaySettings::default();
                match Box::pin(store.save(defaults)).await {
                    Ok(_) => {
                        info!("Persisted and verified default display settings");
                        (Some(store), defaults, "saved")
                    }
                    Err(error) => {
                        error!("Default display settings save failed: {}", error);
                        (Some(store), defaults, "save failed")
                    }
                }
            }
            Err(error) => {
                error!("Display settings storage unavailable: {}", error);
                (None, DisplaySettings::default(), "storage unavailable")
            }
        };

    // Ergot rides a BLE NUS link to the host daemon. This device is an edge
    // node with one interface, so the stack learns the host's net id from the
    // first frame rather than being told.
    let ergot_stack: &'static Stack = {
        static STACK: StaticCell<Stack> = StaticCell::new();
        STACK.init(NetStack::new_with_profile(DirectEdge::new_target(
            framed_stream::Sink::new(BLE_OUTQ.framed_producer(), BLE_MTU),
        )))
    };
    let ble_connector = BleConnector::new(peripherals.BT, Default::default()).unwrap();
    spawner.spawn(ble_task(ergot_stack, ble_connector, &BLE).unwrap());
    spawner.spawn(usage_task(ergot_stack, &USAGE).unwrap());
    spawner.spawn(clock_sync_task(ergot_stack, &RTC).unwrap());

    let i2c = I2c::new(
        peripherals.I2C0,
        I2cConfig::default().with_frequency(Rate::from_khz(400)),
    )
    .unwrap()
    .with_sda(peripherals.GPIO15)
    .with_scl(peripherals.GPIO14)
    .into_async();
    static I2C_BUS: StaticCell<SharedI2cBus> = StaticCell::new();
    let i2c_bus = I2C_BUS.init(Mutex::new(i2c));
    let touch_interrupt = Input::new(
        peripherals.GPIO11,
        InputConfig::default().with_pull(Pull::Up),
    );
    let touch_reset = Output::new(peripherals.GPIO40, Level::High, OutputConfig::default());
    spawner.spawn(touch_task(i2c_bus, &TOUCH, touch_interrupt, touch_reset).unwrap());
    spawner.spawn(pmic_task(i2c_bus, &PMIC).unwrap());
    spawner.spawn(rtc_task(i2c_bus, &RTC).unwrap());

    let display_render_buffer = esp_hal::dma_tx_buffer!(co5300::TILE_BUFFER_BYTES).unwrap();
    let display_transmit_buffer = esp_hal::dma_tx_buffer!(co5300::TILE_BUFFER_BYTES).unwrap();

    let display_spi = Spi::new(
        peripherals.SPI2,
        SpiConfig::default()
            .with_frequency(Rate::from_mhz(DISPLAY_SPI_MHZ))
            .with_mode(Mode::_0),
    )
    .unwrap()
    .with_sio0(peripherals.GPIO4)
    .with_sio1(peripherals.GPIO5)
    .with_sio2(peripherals.GPIO6)
    .with_sio3(peripherals.GPIO7)
    .with_sck(peripherals.GPIO38)
    .with_dma(peripherals.DMA_CH0);

    let display_cs = Output::new(peripherals.GPIO12, Level::High, OutputConfig::default());
    let display_reset = Output::new(peripherals.GPIO39, Level::High, OutputConfig::default());
    let mut display = Co5300::new(
        display_spi,
        display_cs,
        display_reset,
        display_render_buffer,
        display_transmit_buffer,
        DISPLAY_WIDTH,
        DISPLAY_HEIGHT,
    )
    .unwrap();
    display.init(&mut Delay::new()).unwrap();
    display
        .set_brightness(brightness_register(display_settings.brightness_percent))
        .unwrap();
    info!("CO5300 initialized");

    let slint_window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    slint_window.set_size(slint::PhysicalSize::new(
        u32::from(DISPLAY_WIDTH),
        u32::from(DISPLAY_HEIGHT),
    ));
    slint::platform::set_platform(Box::new(EspPlatform::new(slint_window.clone()))).unwrap();

    let ui = MainWindow::new().unwrap();
    ui.set_ble_status("starting".into());
    ui.set_settings_storage_status(settings_storage_status.into());
    update_settings(&ui, display_settings);
    ui::connect_callbacks(&ui, &SETTINGS);

    let channels = UiChannels {
        touch: &TOUCH,
        pmic: &PMIC,
        rtc: &RTC,
        settings: &SETTINGS,
        ble: &BLE,
        usage: &USAGE,
    };

    let mut pace = Pace::new();
    let mut latest_usage = UsageSnapshot::UNKNOWN;
    let mut latest_rtc = None;
    let mut ble_connected = false;
    let mut usb_connected = false;
    let mut last_usage_at = None;
    let mut idle_deadline = next_idle_deadline(display_settings, usb_connected);
    let mut settings_save_deadline = None;
    let mut dimmed = false;
    let started_at = Instant::now();
    let mut displayed_minute = u64::MAX;
    let mut rendered_frames = 0_u32;
    let mut frame_stats = FrameStats::default();
    let mut last_touch_position = None;
    let mut touch_ready = false;
    let mut pmic_ready = false;
    let mut rtc_ready = false;
    let mut application_ready_logged = false;
    let mut display_on = true;
    let mut maintenance_ticker = Ticker::every(Duration::from_secs(30));

    loop {
        slint::platform::update_timers_and_animations();

        // Render before waiting, so whatever the last event changed reaches
        // the panel before the loop goes back to sleep.
        if display_on {
            let mut present_failed = false;
            let mut frame = FrameTiming::default();
            let rendered = slint_window.draw_if_needed(|renderer| {
                renderer.set_dirty_region_alignment(DirtyRegionAlignment::new(2, 2));

                let pipeline_start = HalInstant::now();
                let region = renderer.render_by_line(&mut display);
                frame.pipeline_us = pipeline_start.elapsed().as_micros() as u32;
                (frame.pixels, frame.rects) =
                    region.iter().fold((0, 0), |(pixels, rects), (_, size)| {
                        (
                            pixels + u64::from(size.width) * u64::from(size.height),
                            rects + 1,
                        )
                    });
            });
            if rendered {
                let finish_start = HalInstant::now();
                match display.finish_frame() {
                    Ok(transfers) => frame.transfers = transfers,
                    Err(_) => present_failed = true,
                }
                frame.finish_us = finish_start.elapsed().as_micros() as u32;
            }
            if present_failed {
                error!("CO5300 line upload failed; retrying next frame");
                slint_window.request_redraw();
            }
            if rendered {
                rendered_frames += 1;
                if rendered_frames == 1 {
                    info!("First Slint frame rendered");
                } else if rendered_frames == 2 {
                    info!("First partial Slint frame rendered");
                }
                frame_stats.record(rendered_frames, frame);
            }

            if !application_ready_logged
                && touch_ready
                && pmic_ready
                && rtc_ready
                && rendered_frames >= 2
            {
                application_ready_logged = true;
                info!(
                    "Application ready for touch validation; internal heap free: {} B",
                    esp_alloc::HEAP.free_caps(esp_alloc::MemoryCapability::Internal.into())
                );
            }
        }

        // Sleep until a peripheral reports in, periodic maintenance is due,
        // or an animation is due for its next step. Nothing is polled here.
        let animation = slint::platform::duration_until_next_timer_update();
        // Keep the settings page readable while the user is inspecting it.
        // Closing it starts a fresh idle interval.
        let active_idle_deadline = (display_on && !ui.get_show_settings())
            .then_some(idle_deadline)
            .flatten();
        let event = next_ui_event(
            &channels,
            &mut maintenance_ticker,
            animation,
            active_idle_deadline,
            settings_save_deadline,
        )
        .await;

        match event {
            UiEvent::PowerKey(event) => match event {
                PowerKey::Short if display_on => {
                    dispatch_touch_state(
                        &slint_window,
                        &mut last_touch_position,
                        TouchState::Released,
                    );
                    ui.set_show_power_menu(false);
                    if display.sleep().is_ok() {
                        display_on = false;
                        dimmed = false;
                        idle_deadline = None;
                        info!("Display asleep (PWR short press)");
                    } else {
                        error!("CO5300 sleep failed");
                    }
                }
                PowerKey::Short => {
                    if wake_display(
                        &mut display,
                        &slint_window,
                        display_settings.brightness_percent,
                    )
                    .await
                    {
                        display_on = true;
                        dimmed = false;
                        idle_deadline = next_idle_deadline(display_settings, usb_connected);
                        info!("Display awake (PWR short press)");
                    }
                }
                PowerKey::Long => {
                    if !display_on
                        && wake_display(
                            &mut display,
                            &slint_window,
                            display_settings.brightness_percent,
                        )
                        .await
                    {
                        display_on = true;
                        dimmed = false;
                        idle_deadline = next_idle_deadline(display_settings, usb_connected);
                    }
                    if display_on {
                        ui.set_show_power_menu(true);
                        slint_window.request_redraw();
                        info!("Power menu opened (PWR long press)");
                    }
                }
            },
            UiEvent::TouchReady(ready) => {
                touch_ready = ready;
                ui.set_touch_status(if ready {
                    "CST9220 ready".into()
                } else {
                    "touch error".into()
                });
            }
            // Input is still drained while the panel is asleep, so the touch
            // task never blocks on a full channel, but it is not dispatched.
            UiEvent::Touch(state) if display_on => {
                // Any contact counts as attention: undim and start the
                // countdown again.
                if dimmed
                    && display
                        .set_brightness(brightness_register(display_settings.brightness_percent))
                        .is_ok()
                {
                    dimmed = false;
                }
                idle_deadline = next_idle_deadline(display_settings, usb_connected);

                // A drag queues reports faster than a frame takes to draw, so
                // dispatch everything already waiting before rendering. Without
                // this the loop would render once per report instead of once
                // per batch, which is more work than the old polling version.
                let mut pending = Some(state);
                while let Some(state) = pending {
                    if let TouchState::Pressed { x, y } = state {
                        if last_touch_position.is_none() {
                            info!("Touch down at ({}, {})", x, y);
                        }
                        ui.set_touch_status(format!("touch {x},{y}").into());
                    } else if last_touch_position.is_some() {
                        info!("Touch released");
                    }
                    dispatch_touch_state(&slint_window, &mut last_touch_position, state);
                    pending = TOUCH.events.try_receive().ok();
                }
            }
            UiEvent::Touch(_) => {}
            UiEvent::Pmic(event) => match event {
                PmicEvent::Online(stats) => {
                    pmic_ready = true;
                    let usb_changed = usb_connected != stats.vbus_good;
                    usb_connected = stats.vbus_good;
                    if !display_settings.should_dim(usb_connected) {
                        if dimmed
                            && display_on
                            && display
                                .set_brightness(brightness_register(
                                    display_settings.brightness_percent,
                                ))
                                .is_ok()
                        {
                            dimmed = false;
                        }
                        idle_deadline = None;
                    } else if usb_changed && display_on && !dimmed && !ui.get_show_settings() {
                        idle_deadline = next_idle_deadline(display_settings, usb_connected);
                    }
                    ui.set_pmic_status("AXP2101 online".into());
                    ui.set_battery_known(stats.battery_present);
                    ui.set_battery_percent(i32::from(stats.state_of_charge));
                    ui.set_usb_connected(usb_connected);
                    ui.set_battery_voltage(if stats.battery_present {
                        format!("{} mV", stats.battery_mv).into()
                    } else {
                        "not detected".into()
                    });
                    ui.set_vbus_voltage(if stats.vbus_good {
                        format!("{} mV", stats.vbus_mv).into()
                    } else {
                        "disconnected".into()
                    });
                    ui.set_vsys_voltage(format!("{} mV", stats.vsys_mv).into());
                    ui.set_pmic_temperature(format!("{:.1} C", stats.temperature_c).into());
                    ui.set_power_source(if stats.charging {
                        "USB · charging".into()
                    } else if stats.vbus_good {
                        "USB power".into()
                    } else if stats.battery_present {
                        "Battery".into()
                    } else {
                        "No battery".into()
                    });
                }
                PmicEvent::Error => {
                    ui.set_pmic_status("telemetry error".into());
                }
            },
            UiEvent::Rtc(event) => match event {
                RtcEvent::Online(snapshot) | RtcEvent::Synced(snapshot) => {
                    rtc_ready = true;
                    latest_rtc = Some(snapshot);
                    update_clock(&ui, snapshot);
                    update_usage_state(
                        &ui,
                        &pace,
                        latest_usage,
                        latest_rtc,
                        ble_connected,
                        last_usage_at,
                    );
                }
                RtcEvent::SyncFailed => {
                    latest_rtc = None;
                    ui.set_clock("--:--".into());
                    ui.set_rtc_status("sync failed".into());
                    update_usage_state(
                        &ui,
                        &pace,
                        latest_usage,
                        latest_rtc,
                        ble_connected,
                        last_usage_at,
                    );
                }
                RtcEvent::Error => {
                    rtc_ready = true;
                    latest_rtc = None;
                    ui.set_clock("--:--".into());
                    ui.set_rtc_status("unavailable".into());
                    update_usage_state(
                        &ui,
                        &pace,
                        latest_usage,
                        latest_rtc,
                        ble_connected,
                        last_usage_at,
                    );
                }
            },
            UiEvent::Settings(action) => {
                let mut settings_changed = false;
                let mut normal_brightness_changed = false;
                match action {
                    SettingsAction::Open => {
                        if dimmed
                            && display
                                .set_brightness(brightness_register(
                                    display_settings.brightness_percent,
                                ))
                                .is_ok()
                        {
                            dimmed = false;
                        }
                        ui.set_show_power_menu(false);
                        ui.set_show_settings(true);
                        idle_deadline = None;
                    }
                    SettingsAction::Close => {
                        ui.set_show_settings(false);
                        persist_settings(&mut settings_store, &ui, display_settings).await;
                        settings_save_deadline = None;
                        idle_deadline = next_idle_deadline(display_settings, usb_connected);
                    }
                    SettingsAction::BrightnessStep(direction) => {
                        display_settings.brightness_step(direction);
                        settings_changed = true;
                        normal_brightness_changed = true;
                    }
                    SettingsAction::ToggleAutoDim => {
                        display_settings.auto_dim = !display_settings.auto_dim;
                        settings_changed = true;
                    }
                    SettingsAction::ToggleDimOnUsb => {
                        display_settings.dim_on_usb = !display_settings.dim_on_usb;
                        settings_changed = true;
                    }
                    SettingsAction::IdleTimeoutStep(direction) => {
                        display_settings.idle_timeout_step(direction);
                        settings_changed = true;
                    }
                    SettingsAction::DimBrightnessStep(direction) => {
                        display_settings.dim_brightness_step(direction);
                        settings_changed = true;
                    }
                    SettingsAction::PowerOff => {
                        persist_settings(&mut settings_store, &ui, display_settings).await;
                        settings_save_deadline = None;
                        PMIC.power_off.signal(());
                    }
                    SettingsAction::Reboot => {
                        persist_settings(&mut settings_store, &ui, display_settings).await;
                        esp_hal::system::software_reset();
                    }
                }

                if settings_changed {
                    update_settings(&ui, display_settings);
                    if settings_store.is_some() {
                        ui.set_settings_storage_status("unsaved".into());
                        settings_save_deadline = Some(Instant::now() + SETTINGS_SAVE_DEBOUNCE);
                    }

                    if normal_brightness_changed {
                        if display
                            .set_brightness(brightness_register(
                                display_settings.brightness_percent,
                            ))
                            .is_ok()
                        {
                            dimmed = false;
                            info!(
                                "Display brightness set to {}%",
                                display_settings.brightness_percent
                            );
                        } else {
                            error!("Display brightness update failed");
                        }
                    }

                    if !display_settings.should_dim(usb_connected) {
                        if dimmed
                            && display
                                .set_brightness(brightness_register(
                                    display_settings.brightness_percent,
                                ))
                                .is_ok()
                        {
                            dimmed = false;
                        }
                        idle_deadline = None;
                    } else if ui.get_show_settings() {
                        idle_deadline = None;
                    } else if !dimmed {
                        idle_deadline = next_idle_deadline(display_settings, usb_connected);
                    }
                }
            }
            UiEvent::SaveSettings => {
                persist_settings(&mut settings_store, &ui, display_settings).await;
                settings_save_deadline = None;
            }
            UiEvent::Ble(state) => {
                match state {
                    BleState::Advertising => {
                        ble_connected = false;
                        ui.set_ble_status("advertising".into());
                    }
                    BleState::Connected => {
                        ble_connected = true;
                        ui.set_ble_status("connected".into());
                    }
                    BleState::Error => {
                        ble_connected = false;
                        ui.set_ble_status("error".into());
                    }
                }
                update_usage_state(
                    &ui,
                    &pace,
                    latest_usage,
                    latest_rtc,
                    ble_connected,
                    last_usage_at,
                );
            }
            UiEvent::Maintenance => {
                let elapsed_seconds = started_at.elapsed().as_secs();
                let elapsed_minutes = elapsed_seconds / 60;
                if elapsed_minutes != displayed_minute {
                    displayed_minute = elapsed_minutes;
                    ui.set_uptime(format!("{elapsed_minutes} min").into());
                }
                update_usage_state(
                    &ui,
                    &pace,
                    latest_usage,
                    latest_rtc,
                    ble_connected,
                    last_usage_at,
                );
            }
            UiEvent::Idle => {
                if display_settings.should_dim(usb_connected)
                    && display
                        .set_brightness(brightness_register(
                            display_settings.dim_brightness_percent,
                        ))
                        .is_ok()
                {
                    dimmed = true;
                    info!(
                        "Panel dimmed to {}% after {} s idle",
                        display_settings.dim_brightness_percent,
                        display_settings.idle_timeout_seconds
                    );
                }
                // Dimmed already: nothing left to count down to until a touch
                // restarts it.
                idle_deadline = None;
            }
            UiEvent::Usage(snapshot) => {
                let elapsed_seconds = last_usage_at
                    .map(|received: Instant| received.elapsed().as_secs().min(u64::from(u32::MAX)))
                    .unwrap_or(0) as u32;
                pace.record(snapshot.session_pct, elapsed_seconds);
                latest_usage = snapshot;
                last_usage_at = Some(Instant::now());
                update_usage_state(
                    &ui,
                    &pace,
                    latest_usage,
                    latest_rtc,
                    ble_connected,
                    last_usage_at,
                );
                ui.set_usage_known(true);
                ui.set_usage_session(i32::from(snapshot.session_pct));
                ui.set_usage_weekly(i32::from(snapshot.weekly_pct));
                ui.set_usage_status(
                    match snapshot.status {
                        UsageStatus::Allowed => "allowed",
                        UsageStatus::Limited => "limited",
                        UsageStatus::Unknown => "unknown",
                    }
                    .into(),
                );
                ui.set_usage_limited(snapshot.status == UsageStatus::Limited);
            }
            UiEvent::Animation => {}
        }
    }
}
