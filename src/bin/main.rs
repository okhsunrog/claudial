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
use bt_hci::controller::ExternalController;
use cst92xx::CST92xx;
use defmt::{error, info};
use embassy_embedded_hal::shared_bus::asynch::i2c::I2cDevice;
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::delay::Delay;
use esp_hal::dma::{DmaRxBuf, DmaTxBuf};
use esp_hal::dma_buffers;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::spi::Mode;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::ble::controller::BleConnector;
use panic_rtt_target as _;
use slint::PhysicalPosition;
use slint::platform::software_renderer::{
    DirtyRegionAlignment, MinimalSoftwareWindow, RepaintBufferType, Rgb565BigEndianPixel,
};
use slint::platform::{PointerEventButton, WindowEvent};
use static_cell::StaticCell;
use trouble_host::prelude::*;
use waveshare_esp32s3_amoled_2_16::board::{DISPLAY_HEIGHT, DISPLAY_SPI_MHZ, DISPLAY_WIDTH};
use waveshare_esp32s3_amoled_2_16::co5300::{self, Co5300};
use waveshare_esp32s3_amoled_2_16::pmic::{self, PmicStats, PowerKey};
use waveshare_esp32s3_amoled_2_16::rtc::{self, DateTime as RtcDateTime, Snapshot as RtcSnapshot};
use waveshare_esp32s3_amoled_2_16::slint_platform::EspPlatform;

extern crate alloc;

slint::include_modules!();

const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 1;
const DISPLAY_WIDTH_USIZE: usize = DISPLAY_WIDTH as usize;
const FRAMEBUFFER_PIXELS: usize = DISPLAY_WIDTH_USIZE * DISPLAY_HEIGHT as usize;
const DISPLAY_DMA_BUFFER_SIZE: usize = co5300::MAX_TRANSFER_BYTES;
const DEFAULT_BRIGHTNESS_PERCENT: u8 = 80;
const MINIMUM_BRIGHTNESS_PERCENT: u8 = 5;
/// How long the touch controller may stay silent mid-press before the press is
/// re-checked rather than trusted.
const TOUCH_RELEASE_TIMEOUT: Duration = Duration::from_millis(250);

type SharedI2cBus = Mutex<NoopRawMutex, I2c<'static, esp_hal::Async>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum TouchState {
    Released,
    Pressed { x: u16, y: u16 },
}

#[derive(Clone, Copy)]
enum PmicEvent {
    Online(PmicStats),
    Error,
}

#[derive(Clone, Copy)]
enum RtcEvent {
    Online(RtcSnapshot),
    NeedsSetting,
    Saved(RtcSnapshot),
    SaveFailed,
    Error,
}

static TOUCH_EVENTS: Channel<CriticalSectionRawMutex, TouchState, 8> = Channel::new();
static TOUCH_READY_SIGNAL: Signal<CriticalSectionRawMutex, bool> = Signal::new();
static PMIC_SIGNAL: Signal<CriticalSectionRawMutex, PmicEvent> = Signal::new();
static POWER_KEY_SIGNAL: Signal<CriticalSectionRawMutex, PowerKey> = Signal::new();
static POWER_OFF_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static BRIGHTNESS_SIGNAL: Signal<CriticalSectionRawMutex, u8> = Signal::new();
static RTC_SIGNAL: Signal<CriticalSectionRawMutex, RtcEvent> = Signal::new();
static RTC_SET_SIGNAL: Signal<CriticalSectionRawMutex, RtcDateTime> = Signal::new();

fn brightness_register(percent: u8) -> u8 {
    let percent = percent.clamp(MINIMUM_BRIGHTNESS_PERCENT, 100);
    ((u16::from(percent) * u16::from(u8::MAX) + 50) / 100) as u8
}

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
async fn touch_task(
    i2c_bus: &'static SharedI2cBus,
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
        TOUCH_READY_SIGNAL.signal(false);
        return;
    }
    info!("{} touch controller initialized", touch.model_name());
    TOUCH_READY_SIGNAL.signal(true);

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
                TOUCH_EVENTS.send(TouchState::Released).await;
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
                    TOUCH_EVENTS.send(TouchState::Pressed { x, y }).await;
                } else {
                    pressed = false;
                    TOUCH_EVENTS.send(TouchState::Released).await;
                }
            }
            Err(_) => {
                error!("CST92xx read failed");
                pressed = false;
                TOUCH_EVENTS.send(TouchState::Released).await;
            }
        }
    }
}

#[embassy_executor::task]
#[allow(
    clippy::large_stack_frames,
    reason = "Embassy stores the async task state statically rather than on the runtime call stack"
)]
async fn pmic_task(i2c_bus: &'static SharedI2cBus) {
    let mut axp = match pmic::init(I2cDevice::new(i2c_bus)).await {
        Ok(axp) => axp,
        Err(_) => {
            error!("AXP2101 initialization failed");
            PMIC_SIGNAL.signal(PmicEvent::Error);
            return;
        }
    };
    info!("AXP2101 initialized for telemetry and power-key events");

    let mut first_sample = true;
    let mut last_sample = Instant::now();
    loop {
        if POWER_OFF_SIGNAL.try_take().is_some() {
            info!("Power off requested");
            if axp.power_off().await.is_err() {
                error!("AXP2101 power off failed");
            }
        }

        match pmic::poll_power_key(&mut axp).await {
            Ok(Some(event)) => {
                info!("AXP2101 power key: {}", event);
                POWER_KEY_SIGNAL.signal(event);
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
                    PMIC_SIGNAL.signal(PmicEvent::Online(stats));
                }
                Err(_) => {
                    error!("AXP2101 telemetry read failed");
                    PMIC_SIGNAL.signal(PmicEvent::Error);
                }
            }
        }

        // Power-key status is latched in the PMIC, so polling only determines
        // the response latency and cannot lose a press.
        Timer::after(Duration::from_millis(200)).await;
    }
}

#[embassy_executor::task]
#[allow(
    clippy::large_stack_frames,
    reason = "Embassy stores the async task state statically rather than on the runtime call stack"
)]
async fn rtc_task(i2c_bus: &'static SharedI2cBus) {
    let (mut clock, clock_valid) = match rtc::init(I2cDevice::new(i2c_bus)).await {
        Ok(result) => result,
        Err(_) => {
            error!("PCF85063 initialization failed");
            RTC_SIGNAL.signal(RtcEvent::Error);
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
            RTC_SIGNAL.signal(RtcEvent::Online(snapshot));
        }
        Err(_) => RTC_SIGNAL.signal(RtcEvent::NeedsSetting),
    }

    let mut last_read = Instant::now();
    loop {
        if let Some(request) = RTC_SET_SIGNAL.try_take() {
            let result = match request.to_primitive() {
                Ok(datetime) => clock.set_datetime(&datetime).await,
                Err(_) => {
                    error!("Rejected invalid RTC date/time");
                    RTC_SIGNAL.signal(RtcEvent::SaveFailed);
                    Timer::after(Duration::from_millis(20)).await;
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
                        RTC_SIGNAL.signal(RtcEvent::Saved(snapshot));
                        last_read = Instant::now();
                    }
                    _ => {
                        error!("PCF85063 write verification failed");
                        RTC_SIGNAL.signal(RtcEvent::SaveFailed);
                    }
                },
                Err(_) => {
                    error!("PCF85063 write failed");
                    RTC_SIGNAL.signal(RtcEvent::SaveFailed);
                }
            }
        }

        if last_read.elapsed() >= Duration::from_secs(1) {
            last_read = Instant::now();
            match rtc::read(&mut clock).await {
                Ok(snapshot) => RTC_SIGNAL.signal(RtcEvent::Online(snapshot)),
                Err(_) => {
                    error!("PCF85063 read failed");
                    RTC_SIGNAL.signal(RtcEvent::Error);
                }
            }
        }

        Timer::after(Duration::from_millis(20)).await;
    }
}

/// Length of an editor month, falling back to 31 while the month field is
/// mid-edit and not yet a valid month number.
fn days_in_month(year: i32, month: i32) -> i32 {
    u8::try_from(month)
        .ok()
        .and_then(|month| time::Month::try_from(month).ok())
        .map_or(31, |month| {
            i32::from(time::util::days_in_month(month, year))
        })
}

fn wrap_step(value: i32, delta: i32, minimum: i32, maximum: i32) -> i32 {
    let next = value + delta;
    if next < minimum {
        maximum
    } else if next > maximum {
        minimum
    } else {
        next
    }
}

fn sync_rtc_editor(ui: &MainWindow, datetime: RtcDateTime) {
    ui.set_rtc_edit_year(i32::from(datetime.year));
    ui.set_rtc_edit_month(i32::from(datetime.month));
    ui.set_rtc_edit_day(i32::from(datetime.day));
    ui.set_rtc_edit_hour(i32::from(datetime.hour));
    ui.set_rtc_edit_minute(i32::from(datetime.minute));
    ui.set_rtc_edit_second(i32::from(datetime.second));
}

fn update_rtc_display(ui: &MainWindow, snapshot: RtcSnapshot) {
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

fn dispatch_touch_state(
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

    // find more examples https://github.com/embassy-rs/trouble/tree/main/examples/esp32
    let transport = BleConnector::new(peripherals.BT, Default::default()).unwrap();
    let ble_controller = ExternalController::<_, 1>::new(transport);
    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> =
        HostResources::new();
    let _stack = trouble_host::new(ble_controller, &mut resources);

    // BLE is intentionally kept from the generated template. Its application
    // task and GATT services will be added after the display/touch bring-up.
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
    spawner.spawn(touch_task(i2c_bus, touch_interrupt, touch_reset).unwrap());
    spawner.spawn(pmic_task(i2c_bus).unwrap());
    spawner.spawn(rtc_task(i2c_bus).unwrap());

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
        DISPLAY_WIDTH as u32,
        DISPLAY_HEIGHT as u32,
    ));
    slint::platform::set_platform(Box::new(EspPlatform::new(slint_window.clone()))).unwrap();

    let ui = MainWindow::new().unwrap();
    ui.set_ble_status("initialized".into());
    ui.set_brightness_percent(i32::from(DEFAULT_BRIGHTNESS_PERCENT));
    let ui_weak = ui.as_weak();
    ui.on_brightness_step(move |delta| {
        if let Some(ui) = ui_weak.upgrade() {
            let brightness = (ui.get_brightness_percent() + delta)
                .clamp(i32::from(MINIMUM_BRIGHTNESS_PERCENT), 100);
            ui.set_brightness_percent(brightness);
            BRIGHTNESS_SIGNAL.signal(brightness as u8);
        }
    });
    ui.on_power_off(|| POWER_OFF_SIGNAL.signal(()));
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
        RTC_SET_SIGNAL.signal(request);
    });
    let started_at = Instant::now();
    let mut displayed_second = u64::MAX;
    let mut rendered_frames = 0_u32;
    let mut last_touch_position = None;
    let mut touch_ready = false;
    let mut pmic_ready = false;
    let mut rtc_ready = false;
    let mut application_ready_logged = false;
    let mut display_on = true;
    let mut current_brightness_percent = DEFAULT_BRIGHTNESS_PERCENT;

    loop {
        slint::platform::update_timers_and_animations();

        if let Some(event) = POWER_KEY_SIGNAL.try_take() {
            match event {
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
            }
        }

        if !display_on {
            while TOUCH_EVENTS.try_receive().is_ok() {}
            Timer::after(Duration::from_millis(50)).await;
            continue;
        }

        if let Some(ready) = TOUCH_READY_SIGNAL.try_take() {
            touch_ready = ready;
            ui.set_touch_status(if ready {
                "CST9220 ready".into()
            } else {
                "touch error".into()
            });
        }

        while let Ok(state) = TOUCH_EVENTS.try_receive() {
            if let TouchState::Pressed { x, y } = state {
                if last_touch_position.is_none() {
                    info!("Touch down at ({}, {})", x, y);
                }
                ui.set_touch_status(format!("touch {},{}", x, y).into());
            } else if last_touch_position.is_some() {
                info!("Touch released");
            }
            dispatch_touch_state(&slint_window, &mut last_touch_position, state);
        }

        if let Some(event) = PMIC_SIGNAL.try_take() {
            match event {
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
            }
        }

        if let Some(event) = RTC_SIGNAL.try_take() {
            match event {
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
            }
        }

        if let Some(percent) = BRIGHTNESS_SIGNAL.try_take() {
            if display.set_brightness(brightness_register(percent)).is_ok() {
                current_brightness_percent = percent;
                info!("Display brightness set to {}%", percent);
            } else {
                error!("Display brightness update failed");
            }
        }

        let elapsed_seconds = started_at.elapsed().as_secs();
        if elapsed_seconds != displayed_second {
            displayed_second = elapsed_seconds;
            ui.set_uptime(format!("{} s", elapsed_seconds).into());
        }

        let mut present_failed = false;
        let rendered = slint_window.draw_if_needed(|renderer| {
            renderer.set_dirty_region_alignment(DirtyRegionAlignment::new(2, 2));
            let region = renderer.render(&mut framebuffer, DISPLAY_WIDTH_USIZE);
            if display
                .write_region(&framebuffer, DISPLAY_WIDTH_USIZE, &region)
                .is_err()
            {
                present_failed = true;
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

        Timer::after(Duration::from_millis(16)).await;
    }

    // for inspiration have a look at the examples at https://github.com/esp-rs/esp-hal/tree/esp-hal-v1.1.0/examples
}
