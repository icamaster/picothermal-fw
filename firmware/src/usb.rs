//! WebUSB device: descriptors, the video stream and the command loop.

use core::sync::atomic::{AtomicU8, Ordering};

use defmt::{info, warn};
use embassy_futures::select::{Either, select};
use embassy_rp::peripherals::USB;
use embassy_rp::rom_data;
use embassy_rp::usb::{Driver, Endpoint, In, Out};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::zerocopy_channel::Receiver;
use embassy_time::Timer;
use embassy_usb::control::{InResponse, Recipient, Request, RequestType};
use embassy_usb::descriptor::capability_type;
use embassy_usb::driver::Endpoint as _;
use embassy_usb::driver::{EndpointIn, EndpointOut};
use embassy_usb::msos::{self, windows_version};
use embassy_usb::{Builder, Config, Handler};
use thermal_core::frame::{self, TestPattern};
use thermal_core::protocol::{Command, FrameHeader, Packetizer, PixelFormat, usb_device_release};

use crate::Frame;

/// Full-speed bulk endpoints are capped at 64 bytes per packet.
pub const MAX_PACKET_SIZE: u16 = 64;

/// Prototype identifiers from pid.codes; swap for an allocated PID before this
/// ships, because Windows caches MS OS 2.0 descriptors per VID/PID.
const VID: u16 = 0x1209;
const PID: u16 = 0x0001;

/// The firmware's version, as the device release number hosts read from the
/// device descriptor. Windows keys its cached driver choice on it too, so a new
/// version is re-examined rather than trusted from an older one.
const DEVICE_RELEASE: u16 = usb_device_release(env!("CARGO_PKG_VERSION"));

/// bRequest the host uses for MS OS 2.0 and WebUSB vendor control transfers.
const VENDOR_CODE: u8 = 0x01;

/// Identifies the device to WinUSB. Hosts may filter on it; changing it after a
/// machine has enumerated the device means clearing that machine's driver cache.
const DEVICE_INTERFACE_GUID: &str = "{67A9617C-EDAF-43A2-9ED0-1D4D3A882660}";

/// Page Chrome offers to open when the device is plugged in, without its
/// `https://` prefix: the URL descriptor carries the scheme as a separate byte.
const LANDING_PAGE: &str = "www.cezarchirila.com/webusb/picothermal.html";

/// WebUSB URL descriptor for [`LANDING_PAGE`] (WebUSB spec 4.3.1).
const LANDING_PAGE_DESCRIPTOR: [u8; 3 + LANDING_PAGE.len()] = url_descriptor();

/// wIndex of the WebUSB GET_URL request (WebUSB spec 3.4).
const GET_URL: u16 = 0x02;

/// Streamed pixel format, owned here because the command loop writes it and the
/// stream loop reads it. Only load and store are used: thumbv6m has no atomic
/// compare-and-swap.
static FORMAT: AtomicU8 = AtomicU8::new(PixelFormat::Y16 as u8);

/// Buffers the USB stack borrows for the life of the device.
pub struct Buffers {
    config_descriptor: [u8; 96],
    bos_descriptor: [u8; 96],
    msos_descriptor: [u8; 256],
    control_buf: [u8; 64],
    landing_page: LandingPage,
}

impl Default for Buffers {
    fn default() -> Self {
        Self::new()
    }
}

impl Buffers {
    /// Creates zeroed buffers.
    pub const fn new() -> Self {
        Self {
            config_descriptor: [0; 96],
            bos_descriptor: [0; 96],
            msos_descriptor: [0; 256],
            control_buf: [0; 64],
            landing_page: LandingPage,
        }
    }
}

/// Answers the WebUSB GET_URL request for the landing page.
///
/// MS OS 2.0 shares the vendor code, but embassy answers that one itself
/// (wIndex 7) before handlers are asked, so the two never collide.
struct LandingPage;

impl Handler for LandingPage {
    fn control_in<'a>(&'a mut self, req: Request, _buf: &'a mut [u8]) -> Option<InResponse<'a>> {
        let ours = req.request_type == RequestType::Vendor
            && req.recipient == Recipient::Device
            && req.request == VENDOR_CODE
            && req.index == GET_URL
            && req.value == 1;
        ours.then_some(InResponse::Accepted(&LANDING_PAGE_DESCRIPTOR))
    }
}

const fn url_descriptor() -> [u8; 3 + LANDING_PAGE.len()] {
    const URL_DESCRIPTOR_TYPE: u8 = 0x03;
    const SCHEME_HTTPS: u8 = 0x01;

    let url = LANDING_PAGE.as_bytes();
    let mut desc = [0; 3 + LANDING_PAGE.len()];
    desc[0] = desc.len() as u8;
    desc[1] = URL_DESCRIPTOR_TYPE;
    desc[2] = SCHEME_HTTPS;
    let mut i = 0;
    while i < url.len() {
        desc[3 + i] = url[i];
        i += 1;
    }
    desc
}

/// The endpoints the streaming and command loops run on.
pub struct Endpoints<'d> {
    /// Bulk IN: video frames.
    pub video: Endpoint<'d, USB, In>,
    /// Bulk OUT: host commands.
    pub commands: Endpoint<'d, USB, Out>,
}

/// Builds the WebUSB device and its bulk endpoints.
pub fn build<'d>(
    driver: Driver<'d, USB>,
    buffers: &'d mut Buffers,
) -> (embassy_usb::UsbDevice<'d, Driver<'d, USB>>, Endpoints<'d>) {
    let mut config = Config::new(VID, PID);
    config.device_release = DEVICE_RELEASE;
    config.manufacturer = Some("picothermal");
    config.product = Some("Lepton 3.5 thermal camera");
    config.serial_number = Some("0001");
    config.max_power = 250;
    config.max_packet_size_0 = 64;

    let mut builder = Builder::new(
        driver,
        config,
        &mut buffers.config_descriptor,
        &mut buffers.bos_descriptor,
        &mut buffers.msos_descriptor,
        &mut buffers.control_buf,
    );
    builder.handler(&mut buffers.landing_page);

    // Without these two, Windows leaves the device unbound and the browser
    // cannot open it: WINUSB selects the driver, and the GUID is what WinUSB
    // registers the device interface under. They must be device-level: with a
    // single interface Windows treats the device as non-composite and ignores
    // function subsets, which left it with no driver (code 28) on hardware.
    builder.msos_descriptor(windows_version::WIN8_1, VENDOR_CODE);
    builder.msos_feature(msos::CompatibleIdFeatureDescriptor::new("WINUSB", ""));
    builder.msos_feature(msos::RegistryPropertyFeatureDescriptor::new(
        "DeviceInterfaceGUIDs",
        msos::PropertyData::RegMultiSz(&[DEVICE_INTERFACE_GUID]),
    ));

    let mut function = builder.function(0xFF, 0x00, 0x00);

    let mut interface = function.interface();
    let mut alt = interface.alt_setting(0xFF, 0x00, 0x00, None);

    // The WebUSB platform capability, written by hand rather than through
    // embassy's WebUsb class: that class builds an interface of its own with no
    // endpoints, which would leave a second, driverless vendor interface on the
    // device.
    #[rustfmt::skip]
    alt.bos_capability(
        capability_type::PLATFORM,
        &[
            // PlatformCapabilityUUID from the WebUSB spec.
            0x00, 0x38, 0xb6, 0x08, 0x34, 0xa9, 0x09, 0xa0,
            0x47, 0x8b, 0xfd, 0xa0, 0x76, 0x88, 0x15, 0xb6, 0x65,
            // bcdVersion 1.0, the vendor code, and iLandingPage, the value
            // GET_URL is asked for to fetch LANDING_PAGE.
            0x00, 0x01, VENDOR_CODE, 0x01,
        ],
    );

    let video = alt.endpoint_bulk_in(None, MAX_PACKET_SIZE);
    let commands = alt.endpoint_bulk_out(None, MAX_PACKET_SIZE);
    drop(function);

    (builder.build(), Endpoints { video, commands })
}

/// How long to wait for a camera frame before falling back to the test
/// pattern, so a board with no working camera still streams something.
///
/// It must outlast the camera's normal pauses: no frames for up to ~2 s during
/// a flat-field correction, and up to ~4 s while the stream syncs after
/// power-up. A shorter wait filled those pauses with test pattern frames,
/// which the host drew as if they were the scene. It stays well under the
/// capture watchdog, so a camera that has really stopped is still restarted.
const FRAME_TIMEOUT_MS: u64 = 5000;

/// Streams frames on the bulk IN endpoint, forever.
pub async fn stream(
    ep: &mut Endpoint<'_, USB, In>,
    frames: &mut Receiver<'_, CriticalSectionRawMutex, Frame>,
) -> ! {
    let mut pattern = TestPattern::new();
    let mut fallback_seq: u32 = 0;
    let mut on_fallback = false;

    loop {
        ep.wait_enabled().await;
        info!("host opened the video endpoint");

        loop {
            let sent = match select(frames.receive(), Timer::after_millis(FRAME_TIMEOUT_MS)).await {
                Either::First(frame) => {
                    if on_fallback {
                        info!("usb: camera frames resumed");
                        on_fallback = false;
                    }
                    // Camera frames are always radiometric; the pixel format
                    // command only reaches the test pattern for now.
                    let flags = if frame.ffc { FrameHeader::FLAG_FFC } else { 0 };
                    let header = header(frame.seq, PixelFormat::Y16, flags);
                    let result = send_frame(ep, header, |offset, dst| {
                        dst.copy_from_slice(&frame.pixels[offset..offset + dst.len()])
                    })
                    .await;
                    frames.receive_done();
                    result
                }
                Either::Second(()) => {
                    if !on_fallback {
                        info!(
                            "usb: no camera frame for {=u64} ms, sending the test pattern",
                            FRAME_TIMEOUT_MS
                        );
                        on_fallback = true;
                    }
                    let format = current_format();
                    let header = header(fallback_seq, format, 0);
                    fallback_seq = fallback_seq.wrapping_add(1);
                    let result =
                        send_frame(ep, header, |offset, dst| pattern.fill(format, offset, dst))
                            .await;
                    pattern.advance();
                    result
                }
            };

            if sent.is_err() {
                info!("video endpoint closed");
                break;
            }
        }
    }
}

/// Header for a frame of the camera's geometry.
fn header(seq: u32, format: PixelFormat, flags: u8) -> FrameHeader {
    FrameHeader {
        seq,
        width: frame::WIDTH as u16,
        height: frame::HEIGHT as u16,
        format,
        flags,
    }
}

/// Sends one frame: the header and payload split across 64-byte packets.
///
/// `fill` is handed a payload offset and the slice to write, so the payload can
/// come from a captured frame or be generated on the spot.
async fn send_frame(
    ep: &mut Endpoint<'_, USB, In>,
    header: FrameHeader,
    mut fill: impl FnMut(usize, &mut [u8]),
) -> Result<(), embassy_usb::driver::EndpointError> {
    // Packetizing lives in thermal-core so the host tests can drive it; this
    // loop only moves the packets it produces onto the endpoint.
    let mut packetizer = Packetizer::new(header, MAX_PACKET_SIZE as usize);
    let mut packet = [0u8; MAX_PACKET_SIZE as usize];

    while let Some(len) = packetizer.next_packet(&mut packet, &mut fill) {
        ep.write(&packet[..len]).await?;
    }

    Ok(())
}

/// Applies host commands from the bulk OUT endpoint, forever.
pub async fn commands(ep: &mut Endpoint<'_, USB, Out>) -> ! {
    let mut buf = [0u8; MAX_PACKET_SIZE as usize];

    loop {
        ep.wait_enabled().await;

        while let Ok(len) = ep.read(&mut buf).await {
            match Command::from_bytes(&buf[..len]) {
                Ok(Command::SetPixelFormat(format)) => {
                    FORMAT.store(format as u8, Ordering::Relaxed);
                    info!("pixel format set to {}", format as u8);
                }
                Ok(Command::RebootToBootloader) => {
                    info!("rebooting into the USB bootloader");
                    // Does not return. Nothing needs shutting down first: the
                    // ROM resets the whole chip, camera power pins included,
                    // and nothing is written to flash at runtime.
                    rom_data::reset_to_usb_boot(0, 0);
                }
                // Everything else is for the camera, whose bus this side does
                // not own.
                Ok(command) => {
                    if crate::camera::CONTROL.try_send(command).is_err() {
                        warn!("camera busy, dropping {}", defmt::Debug2Format(&command));
                    }
                }
                Err(_) => warn!("ignoring {} malformed command bytes", len),
            }
        }
    }
}

/// The pixel format the host last asked for.
fn current_format() -> PixelFormat {
    match FORMAT.load(Ordering::Relaxed) {
        x if x == PixelFormat::Agc8 as u8 => PixelFormat::Agc8,
        _ => PixelFormat::Y16,
    }
}
