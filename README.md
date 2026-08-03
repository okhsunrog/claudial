# Claudial

Claudial is a desk instrument for Claude subscription usage. It shows the
five-hour session and seven-day limits as two concentric dials, together with
their reset countdowns, current spending pace, battery state, and data
freshness.

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

The default source uses Claude Code's rotating OAuth credential and obtains the
current rate-limit headers from a minimal Anthropic request:

```sh
cargo run --release -p claudial-host
```

To avoid another upstream request, select the snapshot already cached by
[claude-proxy-rs][proxy]:

```sh
cargo run --release -p claudial-host -- \
  --usage-source claude-proxy
```

On Plasma, this mode reuses the `url`, `username`, and `password` entries saved
by `claude-plasmoid` in KWallet. As a portable fallback, set all three of
`CLAUDIAL_PROXY_URL`, `CLAUDIAL_PROXY_USERNAME`, and
`CLAUDIAL_PROXY_PASSWORD`. Credentials remain on the host.

The daemon scans for a BLE peripheral named `Claudial`, connects to its Nordic
UART Service, and publishes a fresh snapshot once a minute. It also synchronizes
the board's battery-backed RTC, which keeps the local clock and reset countdowns
running between updates.

### User service

```sh
cargo build --release -p claudial-host
install -Dm755 target/release/claudial-host ~/.local/bin/claudial-host
install -Dm644 systemd/claudial-host.service \
  ~/.config/systemd/user/claudial-host.service
systemctl --user daemon-reload
systemctl --user enable --now claudial-host.service
```

The unit uses the default `claude-code` source. To select KWallet-backed
`claude-proxy` for the service, save this as
`~/.config/systemd/user/claudial-host.service.d/usage-source.conf`:

```ini
[Service]
Environment=CLAUDIAL_USAGE_SOURCE=claude-proxy
```

Reload and restart with `systemctl --user daemon-reload` and
`systemctl --user restart claudial-host.service`. Follow logs with
`journalctl --user -u claudial-host.service -f`.

[board-docs]: https://docs.waveshare.com/ESP32-S3-Touch-AMOLED-2.16
[clawdmeter]: https://github.com/HermannBjorgvin/Clawdmeter
[proxy]: https://github.com/okhsunrog/claude-proxy-rs
