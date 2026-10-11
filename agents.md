# AGENTS.md

Guidance for AI agents (e.g. GitHub Copilot, MCP servers, or LLM-based assistants) working in the picothermal-fw repository. This is the single source of project context: what the project is, why it is built this way, and how to write code for it.

## Persona

You are an expert Rust programmer. You write safe, efficient, maintainable, and well-tested code.

- Use an informal tone.
- Do not be overly apologetic and focus on clear guidance.
- If you cannot confidently generate code or other content, do not generate anything and ask for clarification.

## What this is

Firmware for a custom PCB: RP2040 + FLIR Lepton 3.5 + USB-C. It turns the Lepton into a plug-and-play thermal camera that needs no drivers installed - the host is a web app talking WebUSB.

Written in embedded Rust (`no_std`, `heapless`, zero heap) on [embassy](https://embassy.dev). The toolchain is pinned to an exact stable release in `rust-toolchain.toml` (currently 1.96.1) so every machine and CI run uses the same compiler; bump it deliberately and rerun `just check`, do not switch back to a floating `stable`.

## Goals

1. Driverless streaming. WebUSB vendor class is the primary path; MS OS 2.0 descriptors auto-bind WinUSB on Windows so nothing has to be installed. USB UVC is a planned second host path (M7), so the camera also works as an ordinary webcam - not the current target.
2. Own the Lepton driver. Async CCI (I2C control) and VoSPI (SPI video), generic over `embedded-hal-async`.
3. Testable without hardware. The driver is mockable and the protocol is host-tested; `just test` must stay hardware-free.
4. No heap, no surprises. `heapless` everywhere, `unsafe` only where unavoidable and documented.
5. Cross-platform dev. Identical workflow on Windows and Linux.

## Layout

| Path | What it is |
| --- | --- |
| `crates/thermal-core/` | Host workspace member. `no_std`, chip-agnostic, host-testable. `lepton::{cci,power,vospi,mock}` (driver plus mocks for host tests), `protocol` (WebUSB wire format) and `frame` (geometry plus the synthetic `TestPattern` source). |
| `docs/` | Notes not needed to work on the current milestone, plus `test-page.html`, the throwaway WebUSB host page. |
| `firmware/` | RP2040 binary, its own workspace (excluded from the root one) so cross-compilation to `thumbv6m-none-eabi` never shares a build cache or feature unification with host crates. |

Library code stays in one crate. Do not add new lib crates without asking - the maintainer prefers fewer packages.

## Commands (`just`)

`fmt`, `fmt-check`, `lint` (clippy, `-D warnings`), `test` (host only), `build` / `build-release`, `run` (flash over SWD and stream defmt), `flash` (flash over SWD only, no logs), `check` (everything CI runs).

Recipes are direct program invocations only - no shell pipelines, no bash or PowerShell scripts (`windows-shell := ["cmd.exe", "/c"]`). Non-trivial logic belongs in a Rust `xtask`, not a script.

## Board

12 MHz crystal. Pin map of the custom PCB:

| Signal | Pin | Peripheral |
| --- | --- | --- |
| Lepton MISO | GPIO0 | SPI0 RX |
| Lepton CS | GPIO1 | SPI0 CSn |
| Lepton SCLK | GPIO2 | SPI0 SCK |
| Lepton MOSI | GPIO3 | SPI0 TX |
| Lepton SDA | GPIO6 | I2C1 SDA |
| Lepton SCL | GPIO7 | I2C1 SCL |
| Lepton VSYNC | GPIO8 | GPIO input, interrupt on edge |
| Lepton RESET_L | GPIO24 | GPIO output, active low |
| Lepton PWR_DWN_L | GPIO25 | GPIO output, active low |
| LED | GPIO10 | GPIO output |
| Switch (active low) | GPIO11 | GPIO input, needs internal pull-up |

Notes that follow from this map:

- The camera lands exactly on the native peripherals: GPIO0-3 are SPI0 `MisoPin`/`CsPin`/`ClkPin`/`MosiPin` and GPIO6-7 are I2C1 `SdaPin`/`SclPin` in `embassy-rp`, so no bit-banging and no PIO needed for either bus.
- VoSPI is read-only - the Lepton never reads MOSI - so the SPI can be opened rx-only and GPIO3 left unused.
- RESET_L and PWR_DWN_L are on separate pins, so the datasheet power-up sequence can be followed as written: release PWR_DWN_L, wait, then release RESET_L.

## Debug setup (verified working)

- Debug is RTT-only: `defmt-rtt` and `probe-rs` over SWD. There is deliberately no USB CDC/serial - all USB bandwidth goes to video, and the board has SWD pads plus a probe on hand.
- Probe: CMSIS-DAP "debugprobe on Pico" (USB `2e8a:000c`), probe-rs 0.31 from `cargo install probe-rs-tools`. On Windows the probe must be bound to WinUSB with Zadig. Runner is plain `probe-rs run --chip RP2040`.
- `embassy-rp` supplies its own BOOT2 - never add a manual `rp2040-boot2`, it collides.

### Gotcha: `DEFMT_LOG` is compile-time

defmt strips log statements at compile time based on `DEFMT_LOG`. If it is unset, flashing succeeds, RTT attaches, and you get zero output - a very convincing hardware-looking bug that once ate a whole session. Cargo only reads `.cargo/config.toml` relative to the current working directory, and the justfile builds from the workspace root with `--manifest-path firmware/...`, so the `[env]` table in `firmware/.cargo/config.toml` is never read. Hence `export DEFMT_LOG := "debug"` at the top of the justfile; the `.cargo/config.toml` copy is only a fallback for building from inside `firmware/`.

To tell transport from target-side problems: `RUST_LOG=probe_rs=debug probe-rs run ... --log-file <path>`. "Attached to RTT" plus a `defmt` channel means the transport is fine and the logs were compiled out.

## Design decisions (locked in)

- WebUSB vendor class, not a fork. `embassy-usb` supplies the MS OS 2.0 descriptor machinery, so no embassy fork is needed. The streaming class itself is custom (bulk IN video, bulk OUT commands).
- The WebUSB BOS platform capability is written by hand in `firmware/src/usb.rs` rather than via `embassy_usb::class::web_usb`. That class builds its own endpoint-less interface, which would leave a second vendor interface on the device that no driver binds to.
- Runtime-selectable pixel format: Y16 radiometric (TLinear, centikelvin) or AGC 8-bit grayscale, switched by `protocol::Command::SetPixelFormat`.
- `lepton_rs` was evaluated and rejected: stale (last update 2024-07), blocking `embedded-hal` traits, CCI-only, no Lepton 3.x VoSPI. Useful only as a CCI reference.
- Capture runs on core 1, everything else on core 0. Core 1 has an embassy executor of its own with a single task, `camera::run_capture`, and owns the DMA interrupt (moved off core 0 at startup), so nothing on core 0 can delay a packet read. Core 0 runs USB, the LED and `camera::run_control` (power, CCI, host commands, FFC polling). Frames cross through a double-buffered `embassy_sync::zerocopy_channel` (~77 KB in `.bss`) with `CriticalSectionRawMutex`, the RP2040's hardware spinlock. The cores coordinate through two atomics: `CAMERA_READY` (core 0: camera configured, capture may run) and `RESTART_REQUESTED` (core 1: no frame for 10 s, power-cycle the camera). Why: on one core, USB streaming delayed reads enough to desync the camera. Reads must stay on DMA: a CPU-polled read waits for each byte before clocking the next and measured 130 us per packet against 93 us with DMA, which left no margin. The camera needs one roughly every 157 us; the stats line logs average and worst read time to keep an eye on it.
- The web app is out of scope for now - firmware only. A throwaway HTML test page is fine.

## Wire protocol (`thermal_core::protocol`)

Little-endian. Video is a 16-byte `FrameHeader` (`"THRM"` magic, version, format, width, height, wrapping `seq`, `flags`) followed by the payload on bulk IN. Hosts send fixed 3-byte `Command`s on bulk OUT (opcode, then a little-endian 16-bit argument): `SetPixelFormat`, `SetGainMode`, `RunFfc` and `SetEmissivity` (scaled so 8192 is 1.0, the camera's own encoding). Camera commands are handed from the USB side to the camera task through `camera::CONTROL`, because the CCI bus belongs to that task; it applies them between FFC polls and re-applies gain and emissivity if the camera has to be powered up again. All of them are verified on hardware. `FrameHeader::FLAG_FFC` marks frames captured during a flat-field correction, which show the closed shutter: the firmware polls `Cci::ffc_status` every 50 ms alongside capture (separate bus, so the VoSPI reads never pause) and flags any frame that overlaps a correction at either end. The Lepton runs one roughly every 3 minutes; hosts should hold the last good frame rather than display these.

## Versioning

Semantic versioning, with the version in `firmware/Cargo.toml` as the single source of truth. The firmware encodes it at compile time into the USB device release number (`protocol::usb_device_release`), and the page reads it back from the device, so the version shown is always the one actually running. That field is BCD: major up to 99, minor and patch up to 9 each, and anything else fails the build. Every version bump gets a matching entry in `CHANGELOG.md` (Keep a Changelog format). Pushing a `vX.Y.Z` tag runs `.github/workflows/release.yml`, which refuses a tag that disagrees with `firmware/Cargo.toml` and uses the changelog section as the release text; `.github/workflows/ci.yml` runs `just check` on pushes to `main` and on pull requests. `thermal-core` keeps its own crate version in the root workspace.

## Status and roadmap

- [x] M1 - Workspace, `just` tooling, protocol module with host tests.
- [x] M2 (2026-07-21) - Firmware flashes and runs on real hardware; defmt heartbeat over RTT.
- [x] M3 (2026-10-11) - CCI (I2C): `Cci` driver, host-tested against `lepton::mock::MockCamera` and verified on hardware: boot handshake, ping, serial number, AGC, TLinear and VSYNC. Values wider than one register are least significant word first - getting that wrong is what made VSYNC fail with a range error. `lepton::power::Power` does the RESET_L and PWR_DWN_L sequence over `embedded_hal::digital::OutputPin`.
- [x] M4 (2026-10-11) - VoSPI (SPI): `vospi::Assembler` is the protocol as a packet state machine, host-tested with synthetic packets. On hardware it holds 8.8 fps with no desyncs after a few at startup. Two things that mattered: the CRC is table-driven, because the bit-at-a-time form was slow enough that the camera's output corrupted; and capture never waits on USB - with no buffer free it assembles into a scratch buffer and drops the frame, since pausing the reads desyncs the camera. Invalid segments are routine on the Lepton 3.5 (it emits more segments than unique frames).
- [x] M5 (2026-10-11) - WebUSB: vendor class, bulk streaming and MS OS 2.0 in `firmware/src/usb.rs`, packetizing in `protocol::Packetizer` (host-tested with a mock host). Verified on Windows with Edge: WinUSB binds with no driver install and frames reach `docs/test-page.html` at full rate. The MS OS 2.0 features must be device-level, not per-function - Windows ignores function subsets on a single-interface device and leaves it without a driver (code 28). The test pattern is only the fallback when no camera frame arrives for 5 s. It must outlast the camera's ~2 s pause during an FFC and the few seconds it can take to sync after power-up: at 500 ms the pause was filled with unflagged test pattern frames, which is what made calibrations look like a glitch on the page.
- [ ] M6 - Mock Lepton and end-to-end host tests. The frame path is already covered: `TestPattern` as the source and the `MockHost` in `protocol`'s tests as the sink. What is not covered on the host is the USB stack itself - descriptors, enumeration, MS OS 2.0 - which would need a mock `embassy_usb::driver::Driver` and moving the descriptor building out of `firmware/`.
- [ ] M7 - UVC: a custom UVC class written on top of `embassy-usb`, so the camera also enumerates as a standard webcam alongside WebUSB. Design notes, open questions and the host-format constraint live in [docs/uvc.md](docs/uvc.md); read that only when picking up M7.

Camera frames are always streamed as Y16; `SetPixelFormat` only affects the test pattern until AGC8 is implemented for the camera path.

## Coding conventions

### Naming

- Use rust standard naming conventions
- Use short, descriptive names for fields, functions, parameters, and variables.

### Imports

- Keep `use` directives at the top of the module in which they are used, and avoid placing them inside functions or blocks unless absolutely necessary.
- In non-test code, prefer using `crate` in `use` directives to refer to types anywhere in the current crate instead of using its name, or relative paths like `super` or `self`.
- Inside `#[cfg(test)] mod tests` modules, it is acceptable (and preferred) to import APIs from `super` as described in the test generation guidance below.
- Prefer merging new `use` directives into existing ones rather than creating new `use` blocks.
- Avoid importing `*` where possible.

### Error handling

- Handle errors using Rust's `Result` type with the `?` operator when the parent function returns a `Result`.
- Use the `?` operator for calls that return an `Option` in a function that returns `Option`.
- Use thiserror or no library at all, but never anyhow

### Documentation

- Document all public APIs using a concise summary, followed by a blank line, then concise details about the API or no details at all where appropriate.
- Public API documentation should use Rust's document comment syntax denoted by `///` and using markdown.

### Comments

Applies to every file, not just Rust: `.toml`, the justfile and config files too.

- Comment the why, never the what. If the line already says what it does - a pinned version, an obvious assignment - adding prose that restates it is noise.
- Do comment a non-obvious choice: a hardware quirk, a datasheet or spec requirement, a workaround, an ordering or timing constraint, a deliberate deviation from the obvious approach. Cite the source (datasheet section, spec, erratum, issue) when it helps the next reader verify it.
- Prefer one comment that explains a real trap over several that narrate the code. The `DEFMT_LOG` note in the justfile is the model: it exists because the behaviour is genuinely surprising and cost real time.
- Do not restate values or state that lives next to the comment, since it drifts as soon as one side changes.

### General

- Write idiomatic embedded Rust code using `no_std` and `heapless`.
- Prioritize safety, efficiency, and correctness.
- Respect Rust's ownership and borrowing rules, avoid unsafe unless absolutely necessary
- Avoid declaring lifetime parameters in public types or functions except when necessary.
- If you have trouble generating safe, efficient, maintainable, and lint-free code, insert a `TODO` comment describing what should happen.
- Use `defmt` for logging (never `println!`/`log`)
- Contributors here come from a C embedded background (main.c / hal.c / driver.c mental model). Explaining Rust concepts by mapping them to C equivalents lands well.
- Expect "why" questions about structure. Justify architecture, do not impose it.
- Prefer one mechanism over redundant options (RTT-only rather than RTT plus USB CDC). Avoid belt-and-suspenders duplication.
- Windows and Linux parity is non-negotiable; see the `just` rules above.
- In markdown, do not hard-wrap paragraphs to a fixed column - one line per paragraph, wrapping is the editor's job. Keep it plain: no emoji or icons, and use `[ ]` / `[x]` for checklists.