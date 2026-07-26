# Waveshare ESP32-S3 Touch AMOLED 2.16

Rust/`esp-hal` bring-up project for the
[Waveshare ESP32-S3-Touch-AMOLED-2.16][board-docs].

The firmware initializes the CO5300 over QSPI and renders an interactive
480 × 480 Slint UI from a PSRAM framebuffer. Dirty rectangles use the
work-in-progress Slint physical alignment API required by the CO5300. The UI
has overview, display, and power sections sized for the panel's rounded-corner
safe area.

## Current status

- `esp-hal` 1.1 with Embassy/`esp-rtos`
- generated BLE/`trouble-host` stack retained
- 8 MiB octal PSRAM allocator
- CO5300 reset and official Waveshare initialization sequence
- RGB565 big-endian partial framebuffer uploads over QSPI at 40 MHz
- 2 × 2 aligned Slint dirty regions
- CST9220 touch input over the shared 400 kHz I²C bus
- interrupt-driven Slint pointer events (`TP_INT=GPIO11`, `TP_RST=GPIO40`)
- AXP2101 battery, VBUS, VSYS, state-of-charge, and die-temperature telemetry
- interactive brightness control using the CO5300 `0x51` panel command
- AMOLED-black multi-page UI with overview, display, and power sections

IMU, RTC, audio, SD, and BLE application services are not wired yet; the
generated BLE/`trouble-host` stack remains initialized.
The firmware has been flashed and its full-frame plus aligned partial-frame
paths complete successfully on the physical board according to RTT/defmt.
CO5300, CST9220, and AXP2101 initialization all succeed on the physical board.
The UI, orientation, colors, CST9220 IRQ/coordinate mapping, press/release
events, page navigation, and brightness controls have been verified on the
physical AMOLED board.

The AXP2101 setup intentionally changes no board-specific regulator rails. It
only enables battery detection and the ADC channels used for telemetry.
Brightness is independent of the PMIC on this AMOLED board.

## Source layout

- `src/board.rs` — dimensions, buses, and board pin assignments
- `src/co5300.rs` — CO5300 QSPI transport and aligned region uploads
- `src/pmic.rs` — conservative AXP2101 setup and telemetry
- `src/slint_platform.rs` — minimal Slint platform adapter
- `ui/main.slint` — rounded-safe interactive AMOLED UI

## Build

The project intentionally uses the sibling Slint checkout:

```text
/home/okhsunrog/code/rust/slint
/home/okhsunrog/code/rust/waveshare-esp32s3-amoled-2-16
```

Clone both repositories as siblings and check out Slint's
`dirty-region-alignment` branch:

```sh
git clone https://github.com/okhsunrog/slint.git
git -C slint switch dirty-region-alignment
git clone https://github.com/okhsunrog/waveshare-esp32s3-amoled-2-16.git
cd waveshare-esp32s3-amoled-2-16
cargo build --release
```

The generated runner uses the ESP32-S3 USB-JTAG interface:

```sh
cargo run --release
```

Hardware values and the initialization sequence are based on the
[official Waveshare examples][vendor-repo].

[board-docs]: https://docs.waveshare.com/ESP32-S3-Touch-AMOLED-2.16
[vendor-repo]: https://github.com/waveshareteam/ESP32-S3-Touch-AMOLED-2.16
