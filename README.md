# Waveshare ESP32-S3 Touch AMOLED 2.16

Rust/`esp-hal` bring-up project for the
[Waveshare ESP32-S3-Touch-AMOLED-2.16][board-docs].

The first functional milestone initializes the CO5300 over QSPI and renders a
480 × 480 Slint UI from a PSRAM framebuffer. Dirty rectangles use the
work-in-progress Slint physical alignment API required by the CO5300.

## Current status

- `esp-hal` 1.1 with Embassy/`esp-rtos`
- generated BLE/`trouble-host` stack retained
- 8 MiB octal PSRAM allocator
- CO5300 reset and official Waveshare initialization sequence
- RGB565 big-endian partial framebuffer uploads over QSPI at 40 MHz
- 2 × 2 aligned Slint dirty regions

Touch, PMIC telemetry, IMU, RTC, audio, SD, and BLE services are not wired yet.
The firmware has been flashed and its full-frame plus aligned partial-frame
paths complete successfully on the physical board according to RTT/defmt.
Visual output on the AMOLED still needs a human check.

## Source layout

- `src/board.rs` — dimensions, buses, and board pin assignments
- `src/co5300.rs` — CO5300 QSPI transport and aligned region uploads
- `src/slint_platform.rs` — minimal Slint platform adapter
- `ui/main.slint` — initial AMOLED bring-up screen

## Build

The project intentionally uses the sibling Slint checkout:

```text
/home/okhsunrog/code/rust/slint
/home/okhsunrog/code/rust/waveshare-esp32s3-amoled-2-16
```

Check out Slint's `dirty-region-alignment` branch, then:

```sh
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
