# UVC support (planned, M7)

Design notes for the UVC path. Not started - the roadmap in [AGENTS.md](../agents.md) tracks where it sits. Read this only when working on M7.

The same firmware should also enumerate as a standard USB Video Class camera, so it works in OBS, Zoom, Teams and `getUserMedia` with no web app at all. WebUSB stays the primary path and the only one that carries radiometric data; UVC is the "just works as a webcam" path.

`embassy-usb` ships no UVC class (it has CDC-ACM, CDC-NCM, HID, MIDI and the MS OS 2.0 / WebUSB descriptor helpers), so this means writing one on top of `embassy_usb::Builder` - a custom class living in this repo, not a fork of embassy. Upstreaming it later if it turns out clean and general is a nice-to-have, not a requirement.

What the class has to provide:

- A VideoControl interface (VC header, camera input terminal, processing unit, output terminal descriptors) and a VideoStreaming interface (VS input header, uncompressed format and frame descriptors), tied together by an Interface Association Descriptor.
- Class-specific requests on those interfaces: `GET_CUR` / `SET_CUR` / `GET_MIN` / `GET_MAX` / `GET_DEF` on the Video Probe and Commit controls, which is how the host negotiates format, frame size and frame interval before streaming starts. This is the fiddly part and the usual reason a device enumerates but never streams.
- A payload header on every transfer: 2 bytes minimum (`bHeaderLength`, `bmHeaderInfo`), with the frame ID bit toggling per frame and the EOF bit set on the last packet of a frame.

Transport is an open decision:

- Isochronous is what most webcams use. `embassy-rp` 0.10 does allocate isochronous endpoints (up to 1023-byte packets, see `alloc_endpoint` in its `usb.rs`), but `embassy-usb-driver` 0.2.2 still documents `EndpointType::Isochronous` as "not implemented yet" and the RP2040 path is single-buffered, so this needs verifying on hardware before anything is built on it.
- Bulk streaming is legal in UVC 1.1+, avoids bandwidth reservation, skips the alternate-setting dance (alt 0 zero-bandwidth / alt 1 streaming) that isochronous requires, and reuses the transfer code the WebUSB path already needs. Probably the pragmatic first implementation; confirm the stock Windows and Linux UVC drivers accept a bulk-only device before committing.

Bandwidth is not the constraint: RP2040 USB is full speed only, roughly 1 MB/s in practice, while a 160x120 Lepton 3.5 frame is 38400 bytes as Y16 or 19200 bytes as AGC8, which at ~8.7 fps is ~334 KB/s or ~167 KB/s.

Pixel format is the constraint. Hosts only negotiate formats they know: YUY2, NV12 and MJPEG are universally supported, while raw 16-bit grayscale (the Y16 GUID) is not, and Windows and browsers will generally ignore it. So the UVC path should advertise an 8-bit AGC image in a common format (Y800/L8, or YUY2 with fixed chroma) and leave radiometric Y16 to WebUSB. Do not expect centikelvin data to survive a stock webcam pipeline.

The two paths can coexist in one composite device (UVC function behind an IAD, plus the vendor interface for WebUSB), so no mode switch is needed - worth confirming Windows binds usbvideo and WinUSB to the two functions cleanly.
