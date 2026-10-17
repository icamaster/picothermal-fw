# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The version lives in `firmware/Cargo.toml`; the firmware reports it to hosts as the USB device release number.

## [Unreleased]

### Added

- WebUSB landing page: when the camera is plugged in, Chrome offers to open https://www.cezarchirila.com/webusb/picothermal.html.
- Firmware update from the page: an Update button restarts the camera into the RP2040's USB bootloader, so a new `.uf2` can be copied onto it without holding BOOTSEL.
- The page warns on load when the browser cannot use WebUSB, and on Linux shows the udev rule needed to connect.

## [0.1.0] - 2026-10-11

Initial release.

### Added

- Firmware for the RP2040 + FLIR Lepton 3.5 thermal camera board, streaming radiometric 160x120 frames (centikelvin) at the camera's full 8.7 fps over WebUSB, with no driver install on Windows, Linux or Android.
- Lepton driver in `thermal-core`: CCI control over I2C, VoSPI capture with CRC validation and resynchronisation, and the power-up sequence, all host-tested against mocks.
- Two-core runtime: capture runs alone on core 1 with DMA reads, USB and camera control on core 0, so streaming never disturbs capture. A watchdog power-cycles the camera if frames stop.
- Camera control from the host: gain mode (high, low, auto), on-demand flat-field correction, and scene emissivity, re-applied automatically if the camera restarts.
- Frames captured during a flat-field correction are flagged, so hosts can hold the last good image instead of showing the shutter.
- WebUSB page (`webapp/picothermal-webusb.html`) for desktop and Android: colour maps, auto and manual range with a temperature legend, spot readout, frame averaging, mirror and flip, saving the view as a PNG with its legend and settings, and the camera controls above.
