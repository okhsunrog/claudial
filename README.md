# Claudial

Claudial is a desk instrument for Claude subscription usage. It shows the
five-hour session and seven-day limits as two concentric dials, together with
their reset times, current spending pace, battery state, and data freshness.

It runs on the [Waveshare ESP32-S3-Touch-AMOLED-2.16][board-docs]. A host
daemon reads usage from Claude and sends snapshots to the display over BLE.
The complete stack is Rust:

- `claudial-host` — Tokio daemon that obtains usage and publishes it.
- `claudial-icd` — shared ergot topics, payloads, and pace calculation.
- `firmware` — `esp-hal`/Embassy firmware with a Slint UI and a
  `trouble-host` Nordic UART Service peripheral.

The project is working end to end on the physical board. It was inspired by
[Clawdmeter][clawdmeter], but is an independent implementation and does not
reuse its artwork.

## Build

The host crates form the root Cargo workspace:

```sh
cargo build --release --workspace
```

The firmware is deliberately excluded because it uses the `esp` toolchain and
the `xtensa-esp32s3-none-elf` target. Build it from inside its directory so
Cargo picks up the correct toolchain and target configuration:

```sh
cd firmware
cargo build --release
```

With the board connected over USB-JTAG, build and flash through the configured
probe-rs runner:

```sh
cargo run --release
```

## Host daemon

The default `direct` backend reads Claude Code's OAuth credentials from
`~/.claude/.credentials.json` and obtains the current rate-limit headers:

```sh
cargo run --release -p claudial-host
```

The optional `proxy` backend reads the cached snapshot from a
[claude-proxy-rs][proxy] instance:

```sh
CLAUDIAL_PROXY_URL=https://aiproxy.example.com \
CLAUDIAL_PROXY_USERNAME=admin \
CLAUDIAL_PROXY_PASSWORD=... \
  cargo run --release -p claudial-host \
    --no-default-features --features proxy
```

The daemon scans for a BLE peripheral named `Claudial`, connects to its Nordic
UART Service, and publishes a fresh snapshot once a minute. Credentials remain
on the host; they are never sent to or stored on the device.

[board-docs]: https://docs.waveshare.com/ESP32-S3-Touch-AMOLED-2.16
[clawdmeter]: https://github.com/HermannBjorgvin/Clawdmeter
[proxy]: https://github.com/okhsunrog/claude-proxy-rs
