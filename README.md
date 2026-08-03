# Waveshare ESP32-S3 Touch AMOLED 2.16

Rust/`esp-hal` bring-up project for the
[Waveshare ESP32-S3-Touch-AMOLED-2.16][board-docs].

The firmware initializes the CO5300 over QSPI and renders an interactive
480 × 480 Slint UI from a PSRAM framebuffer. Dirty rectangles use the
work-in-progress Slint physical alignment API required by the CO5300. The UI
has overview, display, power, and RTC sections sized for the panel's
rounded-corner safe area.

## Current status

- `esp-hal` 1.1 with Embassy/`esp-rtos`
- generated BLE/`trouble-host` stack retained
- 8 MiB octal PSRAM allocator
- CO5300 reset and official Waveshare initialization sequence
- RGB565 big-endian partial framebuffer uploads over QSPI at 40 MHz
- full-stride rectangles coalesced into whole-DMA-buffer transfers
- 2 × 2 aligned Slint dirty regions
- CST9220 touch input over the shared 400 kHz I²C bus
- interrupt-driven Slint pointer events (`TP_INT=GPIO11`, `TP_RST=GPIO40`)
- a dropped touch release edge is recovered by re-polling the controller
- AXP2101 battery, VBUS, VSYS, state-of-charge, and die-temperature telemetry
- PCF85063ATL battery-backed clock with oscillator-state detection
- on-device date and time editor with write-back verification
- interactive brightness control using the CO5300 `0x51` panel command
- PWR short press toggles CO5300 sleep; 2.5 s hold opens the power menu
- powering on from the AXP2101 off state requires a 2 s PWR hold
- power menu actions for AXP2101 soft power-off and ESP32-S3 restart
- AMOLED-black multi-page UI with overview, display, power, and RTC sections

IMU, audio, SD, and BLE application services are not wired yet; the
generated BLE/`trouble-host` stack remains initialized.
The firmware has been flashed and its full-frame plus aligned partial-frame
paths complete successfully on the physical board according to RTT/defmt.
CO5300, CST9220, AXP2101, and PCF85063 initialization all succeed on the
physical board.
The UI, orientation, colors, CST9220 IRQ/coordinate mapping, press/release
events, page navigation, and brightness controls have been verified on the
physical AMOLED board.

The AXP2101 setup intentionally changes no board-specific regulator rails. It
only enables battery detection and the ADC channels used for telemetry.
Brightness is independent of the PMIC on this AMOLED board.

## Measured frame cost

The firmware times rendering and panel upload separately. The first eight
frames are logged at `info` as a boot benchmark; set `DEFMT_LOG=debug` for
rolling batch summaries. Release build, 240 MHz, 40 MHz QSPI:

| | full frame | typical partial update |
|---|---|---|
| pixels | 230 400 | 700 – 1 800 |
| render | 124 ms | 4.4 – 5.2 ms |
| upload | 47 ms | 0.5 – 0.8 ms |
| DMA transfers | 60 | one per row |

Rendering dominates everywhere: 72 % of a full frame and roughly 88 % of a
small update. Two separate costs make it up — about 0.6 µs per pixel, and a
fixed ~4 ms per frame that does not scale with the dirty region at all, so a
700-pixel update spends under half a millisecond on pixels.

Full-frame upload runs at 9.7 MB/s, near half the 20 MB/s ceiling for four
lines at 40 MHz, and is bandwidth-bound rather than transfer-bound. Row
coalescing cuts a full frame from 480 transfers to 60, which is worth roughly
17 % of the upload; narrow rectangles stay overhead-bound at 20 – 35 µs per
transfer regardless of payload.

## Source layout

The repository is a Cargo workspace of host-side crates, with the firmware
deliberately **excluded** from it: the firmware builds for
`xtensa-esp32s3-none-elf` on the `esp` toolchain channel, so it keeps its own
`rust-toolchain.toml`, its own `.cargo/config.toml` and its own lockfile.

- `claudial-icd/` — wire types shared by firmware and host (workspace member)
- `claudial-host/` — host daemon (workspace member)
- `firmware/` — device firmware (excluded; separate toolchain)

Inside `firmware/`:

- `src/bin/main.rs` — hardware setup, task wiring, and the UI event loop
- `src/animation.rs` — palette-indexed sprite format and frame player
- `src/animations.rs` — generated sprite data (`tools/gen_animations.py`)
- `src/board.rs` — panel dimensions and the board pin table
- `src/co5300.rs` — CO5300 QSPI transport, aligned region uploads, brightness
- `src/events.rs` — task-to-UI events and the wait that drives the loop
- `src/frame_stats.rs` — per-frame render and upload timing
- `src/pmic.rs` — conservative AXP2101 setup and telemetry
- `src/rtc.rs` — PCF85063 probe, calendar reads, and date/time conversion
- `src/slint_platform.rs` — minimal Slint platform adapter
- `src/tasks.rs` — touch, PMIC, and RTC tasks
- `src/ui.rs` — generated Slint types, display helpers, callback wiring
- `ui/main.slint` — rounded-safe interactive AMOLED UI

The UI loop waits rather than polls: it sleeps until a peripheral task reports
in, the uptime second rolls over, or an animation is due. Each task owns its
peripheral and publishes to the channel group it is handed, so `main` is the
only place that decides which task talks to what.

## Build

```sh
git clone https://github.com/okhsunrog/waveshare-esp32s3-amoled-2-16.git
cd waveshare-esp32s3-amoled-2-16
cargo build --release --workspace   # host crates, stable toolchain

cd firmware && cargo build --release   # device firmware, esp toolchain
```

The firmware must be built from inside `firmware/`. Both `rust-toolchain.toml`
and `.cargo/config.toml` are resolved from the current directory, so
`--manifest-path` from the repository root would pick the host toolchain and
the host target.

Cargo fetches everything, including Slint. The dirty-region alignment API the
CO5300 needs is not in a Slint release yet, so `slint` and `slint-build` come
from the branch behind [slint-ui/slint#12656][alignment-pr]; `Cargo.lock` pins
the exact commit. Once the API lands in a release, both can move back to
crates.io.

The generated runner uses the ESP32-S3 USB-JTAG interface:

```sh
cargo run --release
```

Hardware values and the initialization sequence are based on the
[official Waveshare examples][vendor-repo].

## Host daemon

`claudial-host` polls Claude subscription usage once a minute and publishes
it to the device over BLE, as one ergot frame per Nordic UART write. It runs on
a host rather than on the device because every source of these numbers needs a
credential that rotates, and the device has no way to refresh one it was handed
once.

Where the numbers come from is a compile-time choice between two backends.

**`direct`** (default) reads Claude Code's own OAuth token from
`~/.claude/.credentials.json` and makes the smallest possible `/v1/messages`
request, throwing the completion away and keeping the `anthropic-ratelimit-*`
response headers. There is no usage endpoint, so the headers are the payload.

```sh
cargo run --release -p claudial-host
```

**`proxy`** reads the snapshot a [claude-proxy-rs][proxy] instance already
keeps, over the same `/admin/oauth/usage` endpoint [claude-plasmoid][plasmoid]
uses. It spends nothing upstream, works on a machine that has never run Claude
Code, and inherits the proxy's cache and its web-session fallback for
Anthropic's aggressively rate-limited usage endpoint.

```sh
CLAUDIAL_PROXY_URL=https://aiproxy.example.com \
CLAUDIAL_PROXY_USERNAME=admin \
CLAUDIAL_PROXY_PASSWORD=... \
  cargo run --release -p claudial-host --no-default-features --features proxy
```

Cargo features are additive, so `--features proxy` alone leaves `direct`
enabled too. Rather than fail that build, `proxy` simply wins.

[alignment-pr]: https://github.com/slint-ui/slint/pull/12656
[board-docs]: https://docs.waveshare.com/ESP32-S3-Touch-AMOLED-2.16
[plasmoid]: https://github.com/okhsunrog/claude-plasmoid
[proxy]: https://github.com/okhsunrog/claude-proxy-rs
[vendor-repo]: https://github.com/waveshareteam/ESP32-S3-Touch-AMOLED-2.16
