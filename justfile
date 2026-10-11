# All recipes are direct program invocations (no shell pipelines or
# redirects) so they behave identically on Windows and Linux.

set windows-shell := ["cmd.exe", "/c"]

firmware_target := "thumbv6m-none-eabi"

# defmt filters log levels at COMPILE time via DEFMT_LOG. Exporting it here
# guarantees it reaches the compiler for every recipe; relying on
# firmware/.cargo/config.toml's [env] fails because cargo only reads that file
# relative to the working directory, and we build from the workspace root.
export DEFMT_LOG := "debug"

# List available recipes
default:
    just --list

# Format all code (host crates + firmware)
fmt:
    cargo fmt --all
    cargo fmt --all --manifest-path firmware/Cargo.toml

# Check formatting without modifying files
fmt-check:
    cargo fmt --all --check
    cargo fmt --all --check --manifest-path firmware/Cargo.toml

# Lint everything, warnings are errors
lint:
    cargo clippy --workspace --all-targets -- -D warnings
    cargo clippy --manifest-path firmware/Cargo.toml --target {{firmware_target}} -- -D warnings

# Run host-side tests (driver, protocol)
test:
    cargo test --workspace

# Build the firmware image
build:
    cargo build --manifest-path firmware/Cargo.toml --target {{firmware_target}}

# Build the release firmware image
build-release:
    cargo build --manifest-path firmware/Cargo.toml --target {{firmware_target}} --release

# Flash and run firmware with defmt logging over SWD (requires probe-rs)
run:
    cargo run --manifest-path firmware/Cargo.toml --target {{firmware_target}} --release

# Flash the release firmware over SWD and leave it running, without logging
flash:
    cargo flash --manifest-path firmware/Cargo.toml --target {{firmware_target}} --release --chip RP2040

# Everything CI would check: formatting, lints, tests, firmware build
check: fmt-check lint test build
