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
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
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
use waveshare_esp32s3_amoled_2_16::co5300::Co5300;
use waveshare_esp32s3_amoled_2_16::pmic::{self, PmicStats};
use waveshare_esp32s3_amoled_2_16::slint_platform::EspPlatform;

extern crate alloc;

slint::include_modules!();

const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 1;
const DISPLAY_WIDTH_USIZE: usize = DISPLAY_WIDTH as usize;
const FRAMEBUFFER_PIXELS: usize = DISPLAY_WIDTH_USIZE * DISPLAY_HEIGHT as usize;
const DISPLAY_DMA_BUFFER_SIZE: usize = DISPLAY_WIDTH_USIZE * 8 * 2;
const DEFAULT_BRIGHTNESS_PERCENT: u8 = 80;
const MINIMUM_BRIGHTNESS_PERCENT: u8 = 5;

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

static TOUCH_SIGNAL: Signal<CriticalSectionRawMutex, TouchState> = Signal::new();
static TOUCH_READY_SIGNAL: Signal<CriticalSectionRawMutex, bool> = Signal::new();
static PMIC_SIGNAL: Signal<CriticalSectionRawMutex, PmicEvent> = Signal::new();
static BRIGHTNESS_SIGNAL: Signal<CriticalSectionRawMutex, u8> = Signal::new();

fn brightness_register(percent: u8) -> u8 {
    let percent = percent.clamp(MINIMUM_BRIGHTNESS_PERCENT, 100);
    ((u16::from(percent) * u16::from(u8::MAX) + 50) / 100) as u8
}

fn display_coordinates(point: cst92xx::Point) -> (u16, u16) {
    // The official Waveshare demo applies setSwapXY(true) followed by
    // setMirrorXY(true, false) for the panel's 0-degree orientation.
    let x = DISPLAY_WIDTH
        .saturating_sub(1)
        .saturating_sub(point.y.min(DISPLAY_WIDTH.saturating_sub(1)));
    let y = point.x.min(DISPLAY_HEIGHT.saturating_sub(1));
    (x, y)
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

    loop {
        if !interrupt.is_low() {
            interrupt.wait_for_falling_edge().await;
        }

        match touch.touches().await {
            Ok(points) => {
                if let Some(point) = points[0] {
                    let (x, y) = display_coordinates(point);
                    info!("Touch down at ({}, {})", x, y);
                    TOUCH_SIGNAL.signal(TouchState::Pressed { x, y });
                } else {
                    TOUCH_SIGNAL.signal(TouchState::Released);
                }
            }
            Err(_) => {
                error!("CST92xx read failed");
                TOUCH_SIGNAL.signal(TouchState::Released);
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
    info!("AXP2101 initialized for telemetry");

    let mut first_sample = true;
    loop {
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
        Timer::after(Duration::from_secs(1)).await;
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
    let started_at = Instant::now();
    let mut displayed_second = u64::MAX;
    let mut rendered_frames = 0_u32;
    let mut last_touch_position = None;
    let mut touch_ready = false;
    let mut pmic_ready = false;
    let mut application_ready_logged = false;

    loop {
        slint::platform::update_timers_and_animations();

        if let Some(ready) = TOUCH_READY_SIGNAL.try_take() {
            touch_ready = ready;
            ui.set_touch_status(if ready {
                "CST9220 ready".into()
            } else {
                "touch error".into()
            });
        }

        if let Some(state) = TOUCH_SIGNAL.try_take() {
            if let TouchState::Pressed { x, y } = state {
                ui.set_touch_status(format!("touch {},{}", x, y).into());
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

        if let Some(percent) = BRIGHTNESS_SIGNAL.try_take() {
            if display.set_brightness(brightness_register(percent)).is_ok() {
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

        let rendered = slint_window.draw_if_needed(|renderer| {
            renderer.set_dirty_region_alignment(DirtyRegionAlignment::new(2, 2));
            let region = renderer.render(&mut framebuffer, DISPLAY_WIDTH_USIZE);
            display
                .write_region(&framebuffer, DISPLAY_WIDTH_USIZE, &region)
                .unwrap();
        });
        if rendered {
            rendered_frames += 1;
            if rendered_frames == 1 {
                info!("First Slint frame rendered");
            } else if rendered_frames == 2 {
                info!("First partial Slint frame rendered");
            }
        }

        if !application_ready_logged && touch_ready && pmic_ready && rendered_frames >= 2 {
            application_ready_logged = true;
            info!("Application ready for touch validation");
        }

        Timer::after(Duration::from_millis(16)).await;
    }

    // for inspiration have a look at the examples at https://github.com/esp-rs/esp-hal/tree/esp-hal-v1.1.0/examples
}
