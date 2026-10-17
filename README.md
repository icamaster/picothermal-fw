# picothermal-fw

![desktop screenshot](docs/screenshot-desktop.png)

Firmware for the [picothermal](https://github.com/icamaster/picothermal) camera. 

Plug-and-play USB thermal camera: RP2040 + FLIR Lepton 3.5, embedded Rust firmware built on [embassy](https://embassy.dev). Streams raw radiometric video over WebUSB with no drivers to install (MS OS 2.0 descriptors bind WinUSB automatically on Windows).

# Usage 

1. Flash the latest uf2/elf release on a [picothermal](https://github.com/icamaster/picothermal) camera.
2. Open the [WebUSB app](https://www.cezarchirila.com/webusb/picothermal.html) or self-host/open the "webapp/picothermal-webusb.html" file locally (also found on the latest release).  

# AI Usage

Disclaimer: Everything below this line and the majority of this repository has been written by agents.

## Platforms

The web app needs WebUSB, which only Chromium-based browsers (Chrome, Edge, Opera, Brave, Vivaldi, Samsung Internet) implement.

| Platform | Browser | Status |
| --- | --- | --- |
| Windows | Edge | Tested |
| Android | Chrome | Tested |
| Windows | Other Chromium browsers | Should work |
| Android | Other Chromium browsers | Should work |
| macOS | Chromium browsers | Should work |
| ChromeOS | Chrome | Should work |
| Linux | Chromium browsers | Should work, after adding the udev rule below |
| Any | Firefox, Safari | Does not work: no WebUSB |
| iPhone, iPad | Any | Does not work: every iOS browser uses Safari's engine |

On Linux, normal users cannot open USB devices by default, so the browser lists the camera but fails to connect. Allow it once per machine with `/etc/udev/rules.d/50-picothermal.rules` containing:

```
SUBSYSTEM=="usb", ATTR{idVendor}=="1209", ATTR{idProduct}=="0001", TAG+="uaccess"
```

Then replug the camera. The IDs must match the ones in [USB identifiers](#usb-identifiers).

## Layout

| Path | What it is |
| --- | --- |
| `crates/thermal-core` | Chip-agnostic `no_std` core, host-testable: `lepton` module (async Lepton 3.5 driver: CCI over I2C, VoSPI over SPI) and `protocol` module (wire protocol shared with WebUSB hosts) |
| `firmware/` | RP2040 binary (embassy). Own workspace, cross-compiled to `thumbv6m-none-eabi` |

## Setup

Works the same on Windows and Linux. You need:

1. [rustup](https://rustup.rs) - the pinned stable toolchain and the `thumbv6m-none-eabi` target install automatically on first build.
2. [just](https://github.com/casey/just) - task runner: `winget install Casey.Just` or `cargo install just`.
3. [probe-rs](https://probe.rs/docs/getting-started/installation/) - only needed to flash/debug real hardware over SWD: `cargo install probe-rs-tools`.

## Commands

| Command | What it does |
| --- | --- |
| `just fmt` | Format all code |
| `just lint` | Clippy on host crates and firmware, warnings are errors |
| `just test` | Host-side tests (driver + protocol, no hardware needed) |
| `just build` | Cross-compile the firmware |
| `just run` | Flash via SWD probe and stream defmt logs (RTT) |
| `just flash` | Flash via SWD probe and leave it running, without logs |
| `just check` | Everything CI would run: format check, lint, test, build |

## USB identifiers

The firmware currently uses VID `0x1209` / PID `0x0001` (`VID` and `PID` in `firmware/src/usb.rs`, and again in `webapp/picothermal-webusb.html`, which only lists devices matching them), which is [pid.codes](https://pid.codes)' shared test ID, meant for prototypes only. Any device built from this project would clash with every other project using it, so it needs its own PID before boards are shared or distributed. pid.codes allocates PIDs under VID `0x1209` to open-source hardware for free.

Windows caches the driver it chose per VID, PID and device release, so after changing the ID, the device is set up fresh on the first plug-in.

## Debugging

Logging is `defmt` over RTT through the SWD probe (`just run`). Step debugging works with `probe-rs` through VS Code's Debugger extension or `probe-rs gdb`. There is intentionally no USB serial console - all USB bandwidth goes to video.

## Releases

GitHub Actions runs `just check` on every push to `main` and every pull request. To release:

1. Bump `version` in `firmware/Cargo.toml` and add a matching section to `CHANGELOG.md`.
2. Commit, then tag and push the tag: `git tag v0.1.0` and `git push origin v0.1.0`.

The release workflow checks the tag matches the firmware version, builds the firmware, and publishes a GitHub release with that version's changelog section and three files: the `.elf` (for `probe-rs` or `just flash`), a `.uf2` (for drag-and-drop flashing through the RP2040's USB bootloader, which needs the board started with BOOTSEL held) and `picothermal-webusb.html`.

## License

[MIT](LICENSE).

## Contributing

[AGENTS.md](agents.md) holds the project context: goals, layout rationale, hardware and debug setup, locked-in design decisions, roadmap, and coding conventions. Read it before making changes, whether you are a human or an agent.
