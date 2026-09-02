# AGENTS.md

Guidance for AI agents (e.g. GitHub Copilot, MCP servers, or LLM-based assistants) working in the rust-thermal repository. This is the single source of project context: what the project is, why it is built this way, and how to write code for it.

## Persona

You are an expert Rust programmer. You write safe, efficient, maintainable, and well-tested code.

- Use an informal tone.
- Do not be overly apologetic and focus on clear guidance.
- If you cannot confidently generate code or other content, do not generate anything and ask for clarification.

## What this is

Firmware for a custom PCB: RP2040 + FLIR Lepton 3.5 + USB-C. It turns the Lepton into a plug-and-play thermal camera that needs no drivers installed - the host is a web app talking WebUSB.

Written in embedded Rust (`no_std`, `heapless`, zero heap) on [embassy](https://embassy.dev), pinned to stable Rust.

## Goals

1. Driverless streaming. WebUSB vendor class is the primary path; MS OS 2.0 descriptors auto-bind WinUSB on Windows so nothing has to be installed. USB UVC is a planned second host path (M7), so the camera also works as an ordinary webcam - not the current target.
2. Own the Lepton driver. Async CCI (I2C control) and VoSPI (SPI video), generic over `embedded-hal-async`.
3. Testable without hardware. The driver is mockable and the protocol is host-tested; `just test` must stay hardware-free.
4. No heap, no surprises. `heapless` everywhere, `unsafe` only where unavoidable and documented.
5. Cross-platform dev. Identical workflow on Windows and Linux.

## Layout

| Path | What it is |
| --- | --- |
| `crates/thermal-core/` | Host workspace member. `no_std`, chip-agnostic, host-testable. `lepton::{cci,vospi}` (driver) and `protocol` (WebUSB wire format). |
| `firmware/` | RP2040 binary, its own workspace (excluded from the root one) so cross-compilation to `thumbv6m-none-eabi` never shares a build cache or feature unification with host crates. |

Library code stays in one crate. Do not add new lib crates without asking - the maintainer prefers fewer packages.

## Commands (`just`)

`fmt`, `fmt-check`, `lint` (clippy, `-D warnings`), `test` (host only), `build` / `build-release`, `run` (flash over SWD and stream defmt), `check` (everything CI runs).

Recipes are direct program invocations only - no shell pipelines, no bash or PowerShell scripts (`windows-shell := ["cmd.exe", "/c"]`). Non-trivial logic belongs in a Rust `xtask`, not a script.

## Hardware and debug setup (verified working)

- LED on GPIO10, 12 MHz crystal.
- Debug is RTT-only: `defmt-rtt` and `probe-rs` over SWD. There is deliberately no USB CDC/serial - all USB bandwidth goes to video, and the board has SWD pads plus a probe on hand.
- Probe: CMSIS-DAP "debugprobe on Pico" (USB `2e8a:000c`), probe-rs 0.31 from `cargo install probe-rs-tools`. On Windows the probe must be bound to WinUSB with Zadig. Runner is plain `probe-rs run --chip RP2040`.
- `embassy-rp` supplies its own BOOT2 - never add a manual `rp2040-boot2`, it collides.

### Gotcha: `DEFMT_LOG` is compile-time

defmt strips log statements at compile time based on `DEFMT_LOG`. If it is unset, flashing succeeds, RTT attaches, and you get zero output - a very convincing hardware-looking bug that once ate a whole session. Cargo only reads `.cargo/config.toml` relative to the current working directory, and the justfile builds from the workspace root with `--manifest-path firmware/...`, so the `[env]` table in `firmware/.cargo/config.toml` is never read. Hence `export DEFMT_LOG := "debug"` at the top of the justfile; the `.cargo/config.toml` copy is only a fallback for building from inside `firmware/`.

To tell transport from target-side problems: `RUST_LOG=probe_rs=debug probe-rs run ... --log-file <path>`. "Attached to RTT" plus a `defmt` channel means the transport is fine and the logs were compiled out.

## Design decisions (locked in)

- WebUSB vendor class, not a fork. `embassy-usb` already has WebUSB BOS and MS OS 2.0 descriptor support, so no embassy fork is needed. The streaming class itself is custom (bulk IN video, bulk OUT commands).
- Runtime-selectable pixel format: Y16 radiometric (TLinear, centikelvin) or AGC 8-bit grayscale, switched by `protocol::Command::SetPixelFormat`.
- `lepton_rs` was evaluated and rejected: stale (last update 2024-07), blocking `embedded-hal` traits, CCI-only, no Lepton 3.x VoSPI. Useful only as a CCI reference.
- Planned runtime split: core1 owns VoSPI capture (timing-critical, desync risk), core0 runs the embassy executor and USB; double-buffered frames (~77 KB).
- The web app is out of scope for now - firmware only. A throwaway HTML test page is fine.

## Wire protocol (`thermal_core::protocol`)

Little-endian. Video is a 16-byte `FrameHeader` (`"THRM"` magic, version, format, width, height, wrapping `seq`) followed by the payload on bulk IN. Hosts send fixed 2-byte `Command`s on bulk OUT.

## Status and roadmap

- [x] M1 - Workspace, `just` tooling, protocol module with host tests.
- [x] M2 (2026-07-21) - Firmware flashes and runs on real hardware; defmt heartbeat over RTT. `firmware/src/main.rs` is still a blink and heartbeat skeleton.
- [ ] M3 - CCI (I2C): `Cci<I2c>` async driver - poll BUSY in `REG_STATUS`, get/set/run commands, boot handshake, VSYNC enable, AGC and TLinear control. See the `TODO(cci)` in `crates/thermal-core/src/lepton/cci.rs`.
- [ ] M4 - VoSPI (SPI): sync state machine (idle -> resync backoff -> streaming), CRC-16/CCITT validation, segment and frame assembly. See the `TODO(vospi)` in `crates/thermal-core/src/lepton/vospi.rs`.
- [ ] M5 - WebUSB: embassy-usb vendor class, bulk streaming, MS OS 2.0.
- [ ] M6 - Mock Lepton and end-to-end host tests.
- [ ] M7 - UVC: a custom UVC class written on top of `embassy-usb`, so the camera also enumerates as a standard webcam alongside WebUSB. Design notes, open questions and the host-format constraint live in [docs/uvc.md](docs/uvc.md); read that only when picking up M7.

Blocker for M3 and M4: the PCB pinout is still unknown beyond the GPIO10 LED. Needed: SPI (SCK/MISO/CS), I2C (SDA/SCL), VSYNC, and any power-enable or reset pins. Ask rather than guessing.

Nothing is committed yet - `master` has zero commits and everything is untracked.

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