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
use alloc::vec;
use claudial_firmware::animation::Player;
use claudial_firmware::animations::ANIMATIONS;
use claudial_firmware::ble::{ble_task, usage_task};
use claudial_firmware::board::{DISPLAY_HEIGHT, DISPLAY_SPI_MHZ, DISPLAY_WIDTH};
use claudial_firmware::co5300::{self, Co5300, brightness_register};
use claudial_firmware::events::{
    BrightnessSignal, PmicChannels, PmicEvent, RtcChannels, RtcEvent, SpriteSignal, TouchChannels,
    TouchState, UiChannels, UiEvent, UsageSignal, next_ui_event,
};
use claudial_firmware::frame_stats::{FrameStats, FrameTiming};
use claudial_firmware::pmic::PowerKey;
use claudial_firmware::slint_platform::EspPlatform;
use claudial_firmware::tasks::{SharedI2cBus, pmic_task, rtc_task, touch_task};
use claudial_firmware::transport::{BLE_MTU, BLE_OUTQ, Stack};
use claudial_firmware::ui::{
    self, MainWindow, dispatch_touch_state, push_sprite_frame, update_rtc_display,
};
use claudial_icd::UsageStatus;
use defmt::{error, info};
use embassy_executor::Spawner;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Instant, Ticker, Timer};
use ergot::NetStack;
use ergot::interface_manager::profiles::direct_edge::DirectEdge;
use ergot::interface_manager::utils::framed_stream;
use esp_hal::clock::CpuClock;
use esp_hal::delay::Delay;
use esp_hal::dma::{DmaRxBuf, DmaTxBuf};
use esp_hal::dma_buffers;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::spi::Mode;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::{Instant as HalInstant, Rate};
use esp_hal::timer::timg::TimerGroup;
use esp_radio::ble::controller::BleConnector;
use panic_rtt_target as _;
use slint::platform::software_renderer::{
    DirtyRegionAlignment, MinimalSoftwareWindow, RepaintBufferType, Rgb565BigEndianPixel,
};
use static_cell::StaticCell;

extern crate alloc;

const DISPLAY_WIDTH_USIZE: usize = DISPLAY_WIDTH as usize;
const FRAMEBUFFER_PIXELS: usize = DISPLAY_WIDTH_USIZE * DISPLAY_HEIGHT as usize;
const DISPLAY_DMA_BUFFER_SIZE: usize = co5300::MAX_TRANSFER_BYTES;
const DEFAULT_BRIGHTNESS_PERCENT: u8 = 80;

// These live in statics so spawned tasks can be handed `&'static` references,
// but every task takes the group it uses as a parameter, so this is the only
// place the wiring is decided.
static TOUCH: TouchChannels = TouchChannels::new();
static PMIC: PmicChannels = PmicChannels::new();
static RTC: RtcChannels = RtcChannels::new();
static BRIGHTNESS: BrightnessSignal = BrightnessSignal::new();
static SPRITE_NEXT: SpriteSignal = SpriteSignal::new();
static USAGE: UsageSignal = UsageSignal::new();

/// Page index of the sprite screen in the Slint nav bar.
const SPRITE_PAGE: i32 = 4;

/// Render a reset countdown as `2h 05m`, or `--` when the host has nothing.
fn format_reset(minutes: u16) -> alloc::string::String {
    if minutes == 0 {
        return "--".into();
    }
    format!("resets in {}h {:02}m", minutes / 60, minutes % 60)
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
    // generator version: 1.3.0
    // generator parameters: --chip esp32s3 -o unstable-hal -o alloc -o esp -o embassy -o ble-trouble -o probe-rs -o defmt -o panic-rtt-target -o embedded-test -o ci -o vscode -o neovim -o zed

    rtt_target::rtt_init_defmt!();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);
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
    spawner.spawn(ble_task(ergot_stack, ble_connector).unwrap());
    spawner.spawn(usage_task(ergot_stack, &USAGE).unwrap());

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

    let (rx_buffer, rx_descriptors, tx_buffer, tx_descriptors) =
        dma_buffers!(DISPLAY_DMA_BUFFER_SIZE);
    let dma_rx_buffer = DmaRxBuf::new(rx_descriptors, rx_buffer).unwrap();
    let dma_tx_buffer = DmaTxBuf::new(tx_descriptors, tx_buffer).unwrap();

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
    .with_dma(peripherals.DMA_CH0)
    .with_buffers(dma_rx_buffer, dma_tx_buffer);

    let display_cs = Output::new(peripherals.GPIO12, Level::High, OutputConfig::default());
    let display_reset = Output::new(peripherals.GPIO39, Level::High, OutputConfig::default());
    let mut display = Co5300::new(
        display_spi,
        display_cs,
        display_reset,
        DISPLAY_WIDTH,
        DISPLAY_HEIGHT,
    );
    display.init(&mut Delay::new()).unwrap();
    display
        .set_brightness(brightness_register(DEFAULT_BRIGHTNESS_PERCENT))
        .unwrap();
    info!("CO5300 initialized");

    // This allocation is larger than the internal heap and therefore lands in
    // the explicitly initialized 8 MiB PSRAM region.
    let mut framebuffer =
        vec![Rgb565BigEndianPixel::default(); FRAMEBUFFER_PIXELS].into_boxed_slice();

    let slint_window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    slint_window.set_size(slint::PhysicalSize::new(
        u32::from(DISPLAY_WIDTH),
        u32::from(DISPLAY_HEIGHT),
    ));
    slint::platform::set_platform(Box::new(EspPlatform::new(slint_window.clone()))).unwrap();

    let ui = MainWindow::new().unwrap();
    ui.set_ble_status("initialized".into());
    ui.set_brightness_percent(i32::from(DEFAULT_BRIGHTNESS_PERCENT));
    ui::connect_callbacks(&ui, &BRIGHTNESS, &SPRITE_NEXT, &PMIC, &RTC);

    let channels = UiChannels {
        touch: &TOUCH,
        pmic: &PMIC,
        rtc: &RTC,
        brightness: &BRIGHTNESS,
        sprite_next: &SPRITE_NEXT,
        usage: &USAGE,
    };

    let sprite_cells = ui::sprite_model();
    ui.set_sprite_cells(sprite_cells.clone().into());
    let mut sprite_index = 0;
    let mut sprite = Player::new(ANIMATIONS[sprite_index]);
    ui.set_sprite_name(sprite.animation().name.into());
    push_sprite_frame(&sprite_cells, sprite.animation(), sprite.frame());
    let mut sprite_deadline = Instant::now() + sprite.hold();
    let started_at = Instant::now();
    let mut displayed_second = u64::MAX;
    let mut rendered_frames = 0_u32;
    let mut frame_stats = FrameStats::default();
    let mut last_touch_position = None;
    let mut touch_ready = false;
    let mut pmic_ready = false;
    let mut rtc_ready = false;
    let mut application_ready_logged = false;
    let mut display_on = true;
    let mut current_brightness_percent = DEFAULT_BRIGHTNESS_PERCENT;
    let mut uptime_ticker = Ticker::every(Duration::from_secs(1));

    loop {
        slint::platform::update_timers_and_animations();

        // Render before waiting, so whatever the last event changed reaches
        // the panel before the loop goes back to sleep.
        if display_on {
            let mut present_failed = false;
            let mut frame = FrameTiming::default();
            let rendered = slint_window.draw_if_needed(|renderer| {
                renderer.set_dirty_region_alignment(DirtyRegionAlignment::new(2, 2));

                let render_start = HalInstant::now();
                let region = renderer.render(&mut framebuffer, DISPLAY_WIDTH_USIZE);
                let upload_start = HalInstant::now();
                let transfers = display.write_region(&framebuffer, DISPLAY_WIDTH_USIZE, &region);
                let upload_end = HalInstant::now();

                frame.render_us = (upload_start - render_start).as_micros() as u32;
                frame.upload_us = (upload_end - upload_start).as_micros() as u32;
                (frame.pixels, frame.rects) =
                    region.iter().fold((0, 0), |(pixels, rects), (_, size)| {
                        (
                            pixels + u64::from(size.width) * u64::from(size.height),
                            rects + 1,
                        )
                    });
                match transfers {
                    Ok(transfers) => frame.transfers = transfers,
                    Err(_) => present_failed = true,
                }
            });
            if present_failed {
                // The framebuffer holds the frame the panel never received, so the
                // damage must be re-sent rather than dropped.
                error!("CO5300 region upload failed; retrying next frame");
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
                info!("Application ready for touch validation");
            }
        }

        // Sleep until a peripheral reports in, the uptime second rolls over,
        // or an animation is due for its next step. Nothing is polled.
        let animation = slint::platform::duration_until_next_timer_update();
        // Only run the sprite clock while its page is actually showing; off the
        // page there is no deadline and the branch waits for a tap instead.
        let active_sprite_deadline =
            (display_on && ui.get_current_page() == SPRITE_PAGE).then_some(sprite_deadline);
        let event = next_ui_event(
            &channels,
            &mut uptime_ticker,
            animation,
            active_sprite_deadline,
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
                        info!("Display asleep (PWR short press)");
                    } else {
                        error!("CO5300 sleep failed");
                    }
                }
                PowerKey::Short => {
                    if wake_display(&mut display, &slint_window, current_brightness_percent).await {
                        display_on = true;
                        info!("Display awake (PWR short press)");
                    }
                }
                PowerKey::Long => {
                    if !display_on
                        && wake_display(&mut display, &slint_window, current_brightness_percent)
                            .await
                    {
                        display_on = true;
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
                    ui.set_pmic_status("AXP2101 online".into());
                    ui.set_battery_level(if stats.battery_present {
                        format!("{}%", stats.state_of_charge).into()
                    } else {
                        "--".into()
                    });
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
                RtcEvent::Online(snapshot) => {
                    rtc_ready = true;
                    update_rtc_display(&ui, snapshot);
                }
                RtcEvent::NeedsSetting => {
                    rtc_ready = true;
                    ui.set_rtc_clock_valid(false);
                    ui.set_rtc_status("Set date and time".into());
                }
                RtcEvent::Saved(snapshot) => {
                    rtc_ready = true;
                    ui.set_rtc_edit_dirty(false);
                    update_rtc_display(&ui, snapshot);
                    ui.set_rtc_status("Time saved".into());
                }
                RtcEvent::SaveFailed => {
                    ui.set_rtc_status("Save failed".into());
                }
                RtcEvent::Error => {
                    ui.set_rtc_status("RTC unavailable".into());
                }
            },
            UiEvent::Brightness(percent) => {
                if display.set_brightness(brightness_register(percent)).is_ok() {
                    current_brightness_percent = percent;
                    info!("Display brightness set to {}%", percent);
                } else {
                    error!("Display brightness update failed");
                }
            }
            UiEvent::Uptime => {
                let elapsed_seconds = started_at.elapsed().as_secs();
                if elapsed_seconds != displayed_second {
                    displayed_second = elapsed_seconds;
                    ui.set_uptime(format!("{elapsed_seconds} s").into());
                }
            }
            UiEvent::SpriteFrame => {
                sprite.advance();
                push_sprite_frame(&sprite_cells, sprite.animation(), sprite.frame());
                sprite_deadline = Instant::now() + sprite.hold();
            }
            UiEvent::SpriteNext => {
                sprite_index = (sprite_index + 1) % ANIMATIONS.len();
                sprite.set_animation(ANIMATIONS[sprite_index]);
                ui.set_sprite_name(sprite.animation().name.into());
                push_sprite_frame(&sprite_cells, sprite.animation(), sprite.frame());
                sprite_deadline = Instant::now() + sprite.hold();
                info!("Sprite animation: {}", sprite.animation().name);
            }
            UiEvent::Usage(snapshot) => {
                ui.set_usage_session(i32::from(snapshot.session_pct));
                ui.set_usage_weekly(i32::from(snapshot.weekly_pct));
                ui.set_usage_session_reset(format_reset(snapshot.session_reset_mins).into());
                ui.set_usage_weekly_reset(format_reset(snapshot.weekly_reset_mins).into());
                ui.set_usage_status(
                    match snapshot.status {
                        UsageStatus::Allowed => "allowed",
                        UsageStatus::Limited => "limited",
                        UsageStatus::Unknown => "unknown",
                    }
                    .into(),
                );
            }
            UiEvent::Animation => {}
        }
    }

    // for inspiration have a look at the examples at https://github.com/esp-rs/esp-hal/tree/esp-hal-v1.1.0/examples
}
