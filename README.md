# rust-thermal

Plug-and-play USB thermal camera: RP2040 + FLIR Lepton 3.5, embedded Rust
firmware built on [embassy](https://embassy.dev). Streams raw radiometric
video over WebUSB with no drivers to install (MS OS 2.0 descriptors bind
WinUSB automatically on Windows).

## Layout

| Path | What it is |
| --- | --- |
| `crates/thermal-core` | Chip-agnostic `no_std` core, host-testable: `lepton` module (async Lepton 3.5 driver: CCI over I2C, VoSPI over SPI) and `protocol` module (wire protocol shared with WebUSB hosts) |
| `firmware/` | RP2040 binary (embassy). Own workspace, cross-compiled to `thumbv6m-none-eabi` |

## Setup

Works the same on Windows and Linux. You need:

1. [rustup](https://rustup.rs) — the pinned stable toolchain and the
   `thumbv6m-none-eabi` target install automatically on first build.
2. [just](https://github.com/casey/just) — task runner:
   `winget install Casey.Just` or `cargo install just`.
3. [probe-rs](https://probe.rs/docs/getting-started/installation/) — only
   needed to flash/debug real hardware over SWD: `cargo install probe-rs-tools`.

## Commands

| Command | What it does |
| --- | --- |
| `just fmt` | Format all code |
| `just lint` | Clippy on host crates and firmware, warnings are errors |
| `just test` | Host-side tests (driver + protocol, no hardware needed) |
| `just build` | Cross-compile the firmware |
| `just run` | Flash via SWD probe and stream defmt logs (RTT) |
| `just check` | Everything CI would run: format check, lint, test, build |

## Debugging

Logging is `defmt` over RTT through the SWD probe (`just run`). Step
debugging works with `probe-rs` through VS Code's Debugger extension or
`probe-rs gdb`. There is intentionally no USB serial console — all USB
bandwidth goes to video.
