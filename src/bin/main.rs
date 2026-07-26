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
use defmt::info;
use embassy_executor::Spawner;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::delay::Delay;
use esp_hal::dma::{DmaRxBuf, DmaTxBuf};
use esp_hal::dma_buffers;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::spi::Mode;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::ble::controller::BleConnector;
use panic_rtt_target as _;
use slint::platform::software_renderer::{
    DirtyRegionAlignment, MinimalSoftwareWindow, RepaintBufferType, Rgb565BigEndianPixel,
};
use trouble_host::prelude::*;
use waveshare_esp32s3_amoled_2_16::board::{DISPLAY_HEIGHT, DISPLAY_SPI_MHZ, DISPLAY_WIDTH};
use waveshare_esp32s3_amoled_2_16::co5300::Co5300;
use waveshare_esp32s3_amoled_2_16::slint_platform::EspPlatform;

extern crate alloc;

slint::include_modules!();

const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 1;
const DISPLAY_WIDTH_USIZE: usize = DISPLAY_WIDTH as usize;
const FRAMEBUFFER_PIXELS: usize = DISPLAY_WIDTH_USIZE * DISPLAY_HEIGHT as usize;
const DISPLAY_DMA_BUFFER_SIZE: usize = DISPLAY_WIDTH_USIZE * 8 * 2;

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
    let _ = spawner;

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
    display.set_brightness(0xd0).unwrap();
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
    let started_at = Instant::now();
    let mut displayed_second = u64::MAX;
    let mut rendered_frames = 0_u32;

    loop {
        slint::platform::update_timers_and_animations();

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

        Timer::after(Duration::from_millis(16)).await;
    }

    // for inspiration have a look at the examples at https://github.com/esp-rs/esp-hal/tree/esp-hal-v1.1.0/examples
}
