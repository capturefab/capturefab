//! USB3 Vision transport using native OS USB APIs through nusb (no libusb).
//! UVCP/UVSP wire fields and bootstrap register addresses follow USB3 Vision.
#![deny(clippy::missing_const_for_fn)]

use super::chunks::{ByteOrder, ChunkData, ImageLayout};
use crate::{
    framebuf::{PoolVec, pool},
    genicam::decode_xml,
    types::{Backend, CameraInfo, Categorized, ErrorKind, Frame, RegisterIo, Transport},
};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use nusb::{
    DeviceInfo, Endpoint, Interface, MaybeFuture as _,
    descriptors::{TransferType, language_id::US_ENGLISH},
    transfer::{
        Buffer, Bulk, Completion, ControlOut, ControlType, In, Out, Recipient, TransferError,
    },
};
use std::{
    collections::VecDeque,
    num::NonZeroU8,
    time::{Duration, Instant},
};

const MAGIC: u32 = 0x4356_3355;
const LEADER_MAGIC: u32 = 0x4c56_3355;
const TRAILER_MAGIC: u32 = 0x5456_3355;
const READ_CMD: u16 = 0x0800;
const WRITE_CMD: u16 = 0x0802;
const PENDING_ACK: u16 = 0x0805;
const MAX_FRAME: usize = 256 * 1024 * 1024;
const SIRM_CONTROL: u64 = 0x04;

const fn is_u3v_interface(class: u8, subclass: u8, protocol: u8) -> bool {
    class == 0xef && subclass == 0x05 && protocol == 0
}
/// Names nusb could not take from the operating system's cache: Windows
/// keeps no manufacturer string, and some cameras have no USB serial number.
#[derive(Default)]
struct DeviceNames {
    vendor: Option<String>,
    model: Option<String>,
    serial: Option<String>,
    guid: Option<String>,
}
/// The iDeviceGUID, iVendorName, iModelName and iSerialNumber string indexes
/// of a USB3 Vision device info descriptor (type 0x24, subtype 1).
const fn name_indexes(descriptor: &[u8]) -> Option<[u8; 4]> {
    match descriptor {
        [
            _,
            0x24,
            0x01,
            _,
            _,
            _,
            _,
            _,
            _,
            _,
            _,
            guid,
            vendor,
            model,
            _,
            _,
            _,
            serial,
            ..,
        ] => Some([*guid, *vendor, *model, *serial]),
        _ => None,
    }
}
/// Read the strings the control interface's device info descriptor names,
/// falling back to the device descriptor's, as Aravis does.
fn device_names(d: &DeviceInfo) -> DeviceNames {
    let Ok(device) = d.open().wait() else {
        return DeviceNames::default();
    };
    let [guid, vendor, model, serial] = device
        .configurations()
        .find_map(|c| {
            c.interface_alt_settings()
                .filter(|i| is_u3v_interface(i.class(), i.subclass(), i.protocol()))
                .find_map(|i| i.descriptors().find_map(|desc| name_indexes(&desc)))
        })
        .unwrap_or_default();
    let descriptor = device.device_descriptor();
    let read = |index: Option<NonZeroU8>| {
        let text = device
            .get_string_descriptor(index?, US_ENGLISH, Duration::from_millis(100))
            .wait()
            .ok()?;
        Some(text.trim().to_owned()).filter(|text| !text.is_empty())
    };
    DeviceNames {
        vendor: read(NonZeroU8::new(vendor))
            .or_else(|| read(descriptor.manufacturer_string_index())),
        model: read(NonZeroU8::new(model)).or_else(|| read(descriptor.product_string_index())),
        serial: read(NonZeroU8::new(serial))
            .or_else(|| read(descriptor.serial_number_string_index())),
        guid: read(NonZeroU8::new(guid)),
    }
}
/// A camera's ID. One without a serial number is told apart by its GUID,
/// and by its USB address only when it has neither, or when reading them
/// failed, as while another program holds the camera.
fn camera_id(vendor: u16, product: u16, names: &DeviceNames, location: &str) -> String {
    let unique = names
        .serial
        .as_deref()
        .or(names.guid.as_deref())
        .unwrap_or(location);
    format!("usb:{vendor:04x}:{product:04x}:{unique}")
}
/// The names a device's descriptors gave, without the control characters
/// that could steer the terminal that shows them.
fn printable_names(names: DeviceNames) -> DeviceNames {
    let clean = |text: Option<String>| {
        text.map(|text| crate::types::printable_line(&text).trim().to_owned())
            .filter(|text| !text.is_empty())
    };
    DeviceNames {
        vendor: clean(names.vendor),
        model: clean(names.model),
        serial: clean(names.serial),
        guid: clean(names.guid),
    }
}
fn camera_info(d: &DeviceInfo) -> CameraInfo {
    let location = format!("{}:{}", d.bus_id(), d.device_address());
    let cached = |text: Option<&str>| text.filter(|text| !text.is_empty()).map(String::from);
    let mut names = DeviceNames {
        vendor: cached(d.manufacturer_string()),
        model: cached(d.product_string()),
        serial: cached(d.serial_number()),
        guid: None,
    };
    if names.vendor.is_none() || names.model.is_none() || names.serial.is_none() {
        let read = device_names(d);
        names.vendor = names.vendor.or(read.vendor);
        names.model = names.model.or(read.model);
        names.serial = names.serial.or(read.serial);
        names.guid = read.guid;
    }
    let names = printable_names(names);
    CameraInfo {
        id: camera_id(d.vendor_id(), d.product_id(), &names, &location),
        transport: Transport::Usb3,
        vendor: names.vendor.unwrap_or_else(|| "Unknown".into()),
        model: names.model.unwrap_or_else(|| "USB3 Vision camera".into()),
        serial: names.serial.unwrap_or_default(),
        address: Some(location),
        profile: crate::camera_profile::CameraProfile::default(),
    }
}
pub(crate) fn discover() -> Result<Vec<CameraInfo>> {
    let mut result = Vec::new();
    for d in nusb::list_devices()
        .wait()
        .context("cannot enumerate USB devices")?
    {
        let matched = d
            .interfaces()
            .any(|i| is_u3v_interface(i.class(), i.subclass(), i.protocol()));
        // Some platforms omit interface summaries. Inspect cached descriptors after
        // opening an IAD device; the miscellaneous device class alone is ambiguous.
        let matched = matched
            || (d.class() == 0xef
                && d.subclass() == 0x02
                && d.protocol() == 0x01
                && d.open().wait().is_ok_and(|dev| {
                    dev.configurations().any(|c| {
                        c.interface_alt_settings()
                            .any(|i| is_u3v_interface(i.class(), i.subclass(), i.protocol()))
                    })
                }));
        if matched {
            result.push(camera_info(&d));
        }
    }
    result.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(result)
}
/// Where opening a camera stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Open,
    ClaimControl,
    ClaimStream,
}

/// What a failed open or interface claim means on `os`, in plain words with
/// the one next step, and how scripts see it. Permission, a busy device and
/// a missing driver are all states of this computer the user can change, so
/// they are `unavailable` (exit 3), the Windows driver case included.
/// `node` is the usbfs path on Linux.
fn access_error(
    stage: Stage,
    kind: nusb::ErrorKind,
    name: &str,
    node: Option<&str>,
    os: &str,
) -> (ErrorKind, String) {
    use nusb::ErrorKind as Usb;
    let doing = match stage {
        Stage::Open => "open",
        Stage::ClaimControl => "claim the control interface of",
        Stage::ClaimStream => "claim the stream interface of",
    };
    let at = node.map(|node| format!(" ({node})")).unwrap_or_default();
    match (kind, os) {
        (Usb::PermissionDenied, "linux") => (
            ErrorKind::PermissionDenied,
            format!(
                "No permission to {doing} USB3 Vision camera {name}{at}. Run `capturefab doctor` for the udev rule that fixes it."
            ),
        ),
        (Usb::PermissionDenied, _) => (
            ErrorKind::PermissionDenied,
            format!("No permission to {doing} USB3 Vision camera {name}. Run `capturefab doctor`."),
        ),
        (Usb::Busy, _) => (
            ErrorKind::CameraBusy,
            format!(
                "USB3 Vision camera {name} is in use by another application (for example a vendor viewer or SDK). Close it and connect again."
            ),
        ),
        (Usb::Disconnected, _) => (
            ErrorKind::Disconnected,
            format!(
                "USB3 Vision camera {name} disconnected while opening; reconnect it and discover again."
            ),
        ),
        (Usb::Unsupported | Usb::NotFound, "windows") => (
            ErrorKind::Unavailable,
            format!(
                "Windows is not using the WinUSB driver for the USB3 Vision interface of camera {name}. Assign WinUSB to it; see https://capturefab.com/docs.html#winusb."
            ),
        ),
        // Other is any unclassified failure: a camera that already uses
        // WinUSB must not be sent to replace its driver.
        (Usb::Other, "windows") => (
            ErrorKind::OperationFailed,
            format!(
                "Cannot {doing} USB3 Vision camera {name}. If Windows is not using the WinUSB driver for its USB3 Vision interface, see https://capturefab.com/docs.html#winusb."
            ),
        ),
        (Usb::Unsupported | Usb::Other, "macos") => (
            ErrorKind::Unavailable,
            format!(
                "macOS did not give Capturefab the USB interface of camera {name}; another driver or app may hold it. Close other camera software and run `capturefab doctor`."
            ),
        ),
        _ => (
            ErrorKind::OperationFailed,
            format!("Cannot {doing} USB3 Vision camera {name}{at}."),
        ),
    }
}

/// The error for an open or claim the OS refused: [`access_error`]'s words
/// with the OS's own after them for whoever debugs it, and on Linux the
/// udev rule that fixes a refused permission as its hint.
fn refusal(
    stage: Stage,
    kind: nusb::ErrorKind,
    detail: &dyn std::fmt::Display,
    name: &str,
    node: Option<&str>,
    os: &str,
) -> anyhow::Error {
    let (category, message) = access_error(stage, kind, name, node, os);
    let error = Categorized::new(category, format!("{message} (USB: {detail})"));
    if kind == nusb::ErrorKind::PermissionDenied && os == "linux" {
        error.hint(crate::doctor::usb_access_fix()).into()
    } else {
        error.into()
    }
}

/// The camera as errors name it after "USB3 Vision camera": its product
/// string, or without one its USB IDs, which tell cameras apart.
fn display_name(product: Option<&str>, vendor_id: u16, product_id: u16) -> String {
    product
        .map(|product| crate::types::printable_line(product).trim().to_owned())
        .filter(|product| !product.is_empty())
        .unwrap_or_else(|| format!("{vendor_id:04x}:{product_id:04x}"))
}

#[derive(Debug, Clone, Copy)]
struct InterfaceEndpoints {
    number: u8,
    alternate: u8,
    input: u8,
    output: Option<u8>,
}

pub(crate) fn open(info: &CameraInfo, timeout: Duration) -> Result<Box<dyn Backend>> {
    ensure!(
        info.transport == Transport::Usb3,
        "camera is not a USB3 Vision device"
    );
    // Reading names may open a device, so only cameras of this model are.
    let (device_info, observed_info) = nusb::list_devices()
        .wait()?
        .filter(|d| {
            let model = format!("usb:{:04x}:{:04x}:", d.vendor_id(), d.product_id());
            info.id.starts_with(&model)
        })
        .find_map(|device| {
            let observed = camera_info(&device);
            (observed.id == info.id).then_some((device, observed))
        })
        .context("USB3 Vision camera disconnected or its USB address changed; discover again")?;
    let name = display_name(
        device_info.product_string(),
        device_info.vendor_id(),
        device_info.product_id(),
    );
    let node = cfg!(target_os = "linux").then(|| {
        format!(
            "/dev/bus/usb/{}/{:03}",
            device_info.bus_id(),
            device_info.device_address()
        )
    });
    let refused = |stage: Stage| {
        let name = name.clone();
        let node = node.clone();
        move |error: nusb::Error| {
            refusal(
                stage,
                error.kind(),
                &error,
                &name,
                node.as_deref(),
                std::env::consts::OS,
            )
        }
    };
    let device = device_info.open().wait().map_err(refused(Stage::Open))?;
    let config = device
        .active_configuration()
        .context("USB camera has no active configuration")?;
    let mut control = None;
    let mut data = None;
    for interface in config.interface_alt_settings() {
        if interface.class() != 0xef || interface.subclass() != 0x05 {
            continue;
        }
        let input = interface
            .endpoints()
            .find(|e| e.transfer_type() == TransferType::Bulk && e.address() & 0x80 != 0)
            .map(|e| e.address());
        let output = interface
            .endpoints()
            .find(|e| e.transfer_type() == TransferType::Bulk && e.address() & 0x80 == 0)
            .map(|e| e.address());
        if let Some(input) = input {
            let ep = InterfaceEndpoints {
                number: interface.interface_number(),
                alternate: interface.alternate_setting(),
                input,
                output,
            };
            match interface.protocol() {
                0 if output.is_some() && control.is_none() => control = Some(ep),
                2 if data.is_none() => data = Some(ep),
                _ => {}
            }
        }
    }
    let control = control.context("USB3 Vision control bulk endpoints not found")?;
    let data = data.context("USB3 Vision stream bulk endpoint not found")?;
    let control_interface = device
        .claim_interface(control.number)
        .wait()
        .map_err(refused(Stage::ClaimControl))?;
    if control.alternate != 0 {
        control_interface
            .set_alt_setting(control.alternate)
            .wait()?;
    }
    let data_interface = if data.number == control.number {
        control_interface.clone()
    } else {
        device
            .claim_interface(data.number)
            .wait()
            .map_err(refused(Stage::ClaimStream))?
    };
    if data.alternate != 0 {
        data_interface.set_alt_setting(data.alternate).wait()?;
    }
    let channel = ControlEndpoints {
        input: control_interface.endpoint::<Bulk, In>(control.input)?,
        output: control_interface.endpoint::<Bulk, Out>(control.output.unwrap())?,
    };
    let mut backend = UsbBackend {
        control: Control::new(channel, timeout.max(Duration::from_millis(20))),
        stream_in: data_interface.endpoint::<Bulk, In>(data.input)?,
        stream_interface: data_interface,
        _control_interface: control_interface,
        sirm: 0,
        stream: None,
        pixel_encodings: Vec::new(),
        ids_vendor: crate::types::IdsPixelFormat::vendor_matches(&observed_info.vendor),
    };
    let response_ms = backend.u32(0x01cc)?;
    ensure!(
        response_ms <= 60000,
        "camera reports an invalid maximum response time"
    );
    let control = &mut backend.control;
    control.response = Duration::from_millis(u64::from(response_ms));
    control.timeout = control.timeout.max(control.response);
    let sbrm = backend.u64(0x01d8)?;
    let max_command = backend.u32(sbrm + 0x14)? as usize;
    let max_ack = backend.u32(sbrm + 0x18)? as usize;
    ensure!(
        (24..=65548).contains(&max_command) && (16..=65548).contains(&max_ack),
        "invalid USB3 Vision control transfer limits"
    );
    backend.control.max_command = max_command;
    backend.control.max_ack = max_ack;
    backend.sirm = backend.u64(sbrm + 0x20)?;
    ensure!(
        backend.sirm != 0,
        "camera has no USB3 Vision stream register map"
    );
    Ok(Box::new(backend))
}

struct UsbBackend {
    control: Control<ControlEndpoints>,
    stream_in: Endpoint<Bulk, In>,
    stream_interface: Interface,
    _control_interface: Interface,
    sirm: u64,
    stream: Option<Stream>,
    pixel_encodings: Vec<crate::types::PixelEncoding>,
    ids_vendor: bool,
}
/// Longest stream transfer, as in Aravis: payload transfers are this long at
/// most.
const MAX_TRANSFER: usize = 1024 * 1024;
/// Fewest stream transfers kept submitted.
const MIN_TRANSFERS: usize = 4;
/// Linux's errno when usbfs memory cannot cover another transfer.
const ENOMEM: u32 = 12;
const USBFS_EXHAUSTED: &str = "USB stream transfers cannot be submitted because usbfs memory is exhausted; raise /sys/module/usbcore/parameters/usbfs_memory_mb";

/// Bytes of stream transfers to keep submitted, unbounded without a usbfs
/// limit. Linux charges every usbfs transfer, mapped or copied, against one
/// limit that all USB devices on the computer share, so each USB3 Vision
/// camera takes an equal share of half of it.
fn window(usbfs_memory_mb: Option<u64>, cameras: usize) -> usize {
    match usbfs_memory_mb {
        Some(mb) if mb > 0 => ((mb.min(4095) as usize) << 20) / 2 / cameras.max(1),
        _ => usize::MAX,
    }
}
#[cfg(any(target_os = "linux", target_os = "android"))]
fn usbfs_window() -> usize {
    let limit = std::fs::read_to_string("/sys/module/usbcore/parameters/usbfs_memory_mb")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(16);
    let cameras = nusb::list_devices().wait().map_or(1, |devices| {
        devices
            .filter(|d| {
                d.interfaces()
                    .any(|i| is_u3v_interface(i.class(), i.subclass(), i.protocol()))
            })
            .count()
    });
    window(Some(limit), cameras)
}
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn usbfs_window() -> usize {
    window(None, 1)
}

/// The longest transfer that may receive a whole image. nusb turns on
/// `WinUSB`'s `RAW_IO`, under which a transfer must fit the pipe's
/// `MAXIMUM_TRANSFER_SIZE`, which nusb does not report, so Windows keeps to
/// the length every other transfer uses: only images up to 1 MiB arrive
/// whole there. macOS and Linux take a transfer of any length, Linux
/// within its usbfs window.
#[cfg(windows)]
const WHOLE_TRANSFER: usize = MAX_TRANSFER;
#[cfg(not(windows))]
const WHOLE_TRANSFER: usize = MAX_FRAME;
/// The share of the memory the process may still use that whole-image
/// transfers may hold: `MIN_TRANSFERS` frames, all submitted at once.
const WHOLE_SHARE: u64 = 8;

/// How the camera splits a frame's image into payload transfers, and the
/// transfers kept submitted to receive it.
struct Layout {
    block: usize,
    count: usize,
    tail: usize,
    /// Length every transfer requests.
    request: usize,
    transfers: usize,
    /// One transfer receives a whole image, into a buffer that becomes the
    /// frame's: where the platform takes a transfer that long, and
    /// `MIN_TRANSFERS` of them fit the usbfs window and a share of the
    /// memory left.
    whole: bool,
}
fn layout(
    expected: usize,
    leader: usize,
    trailer: usize,
    alignment: usize,
    window: usize,
    memory: Option<u64>,
) -> Result<Layout> {
    let image = aligned(expected, alignment)?;
    let request = image.max(leader).max(trailer);
    let room = memory.map_or(usize::MAX, |memory| {
        usize::try_from(memory / WHOLE_SHARE).unwrap_or(usize::MAX)
    });
    if request <= WHOLE_TRANSFER && request.saturating_mul(MIN_TRANSFERS) <= window.min(room) {
        return Ok(Layout {
            block: image,
            count: expected / image,
            tail: if expected == image { 0 } else { image },
            request,
            transfers: MIN_TRANSFERS,
            whole: true,
        });
    }
    let block = aligned(
        expected
            .min(MAX_TRANSFER)
            .min(window / MIN_TRANSFERS)
            .max(1),
        alignment,
    )?;
    let count = expected / block;
    let tail = if expected.is_multiple_of(block) {
        0
    } else {
        aligned(expected % block, alignment)?
    };
    let request = block.max(leader).max(trailer);
    // At most two frames of transfers, as many as fit in the window.
    let per_frame = count + usize::from(tail > 0) + 2;
    Ok(Layout {
        block,
        count,
        tail,
        request,
        transfers: (window / request).clamp(MIN_TRANSFERS, 2 * per_frame),
        whole: false,
    })
}

/// Whether Linux refused to submit a transfer for lack of usbfs memory. The
/// transfer never reached the camera, so no data is missing.
fn usbfs_refused(status: Result<(), TransferError>) -> bool {
    (cfg!(any(target_os = "linux", target_os = "android")) || cfg!(test))
        && status == Err(TransferError::Unknown(ENOMEM))
}

/// `SET_FEATURE(ENDPOINT_HALT)` for an endpoint.
fn halt(endpoint: u8) -> ControlOut<'static> {
    ControlOut {
        control_type: ControlType::Standard,
        recipient: Recipient::Endpoint,
        request: 0x03,
        value: 0,
        index: endpoint.into(),
        data: &[],
    }
}

/// Whether a completed transfer holds one whole leader or trailer: the
/// header's magic, and a size field the transfer covers.
fn is_header(bytes: &[u8], magic: u32, min: usize, max: usize) -> bool {
    bytes.len() <= max
        && le32(bytes, 0).is_ok_and(|m| m == magic)
        && le16(bytes, 6).is_ok_and(|size| (min..=bytes.len()).contains(&(size as usize)))
}

/// Receives frames through a bounded window of transfers that all request
/// the same length. A payload transfer fills one exactly, while a leader,
/// a trailer and a short last payload transfer each end theirs with a short
/// packet, so a leader always starts a transfer. Frames are assembled from
/// what each transfer holds rather than from its place in the queue. Short
/// packets/ZLPs or the configured maximum establish a trailer boundary. The
/// stream resynchronizes at metadata boundaries without scanning pixel bytes.
///
/// Where one transfer can take a whole image (`Layout::whole`), the
/// transfers' buffers come from the frame pool, and the buffer that
/// receives an image becomes the frame's, replaced by another from the
/// pool: the image is never copied. Anything else is assembled by copying,
/// as with smaller transfers.
struct Stream {
    /// Configured upper bound of image and optional chunk bytes per frame.
    expected: usize,
    pixel_encodings: Vec<crate::types::PixelEncoding>,
    /// Longest leader and trailer the camera sends.
    leader: usize,
    trailer: usize,
    /// The transfer length, when transfers receive whole images into
    /// buffers from the pool.
    whole: Option<usize>,
    /// Buffers not submitted.
    idle: Vec<Buffer>,
    /// Transfers submitted and not yet returned.
    submitted: usize,
    /// The frame being received: its leader's fields, and its image so
    /// far, in a buffer taken once image bytes must be assembled.
    frame: Option<PendingImage>,
    /// Frames, or why one was lost, taken in ahead of `next`.
    taken: VecDeque<Result<Frame>>,
    /// Linux refused a submit for lack of usbfs memory: hold buffers back
    /// until a transfer completes and frees some.
    starved: bool,
}
/// Where a stream's transfers are submitted: the camera's stream endpoint.
trait Transfers {
    fn allocate(&mut self, len: usize) -> Buffer;
    fn submit(&mut self, buffer: Buffer);
    fn wait(&mut self, timeout: Duration) -> Option<Completion>;
    fn cancel_all(&mut self);
}
impl Transfers for Endpoint<Bulk, In> {
    fn allocate(&mut self, len: usize) -> Buffer {
        Self::allocate(self, len)
    }
    fn submit(&mut self, buffer: Buffer) {
        Self::submit(self, buffer);
    }
    fn wait(&mut self, timeout: Duration) -> Option<Completion> {
        self.wait_next_complete(timeout)
    }
    fn cancel_all(&mut self) {
        Self::cancel_all(self);
    }
}
impl Stream {
    const fn new(expected: usize, leader: usize, trailer: usize, whole: Option<usize>) -> Self {
        Self {
            expected,
            pixel_encodings: Vec::new(),
            leader,
            trailer,
            whole,
            idle: Vec::new(),
            submitted: 0,
            frame: None,
            taken: VecDeque::new(),
            starved: false,
        }
    }
    /// Allocate and submit `count` transfers of `len` bytes. Allocating maps
    /// memory, so this happens only when acquisition starts; whole-image
    /// transfers take their buffers from the pool.
    fn queue(&mut self, transfers: &mut impl Transfers, count: usize, len: usize) {
        for _ in 0..count {
            let buffer = self
                .whole
                .map_or_else(|| transfers.allocate(len), whole_buffer);
            self.idle.push(buffer);
        }
        self.fill(transfers);
    }
    fn fill(&mut self, transfers: &mut impl Transfers) {
        while let Some(buffer) = self.idle.pop() {
            transfers.submit(buffer);
            self.submitted += 1;
        }
    }
    /// Wait for the next whole frame. A lost frame returns one error, and
    /// the transfers after it go on to the next frame.
    fn next(&mut self, transfers: &mut impl Transfers, deadline: Instant) -> Result<Frame> {
        if let Some(taken) = self.taken.pop_front() {
            return taken;
        }
        loop {
            if self.submitted == 0 {
                // Linux refused every submit: retry shortly, as Aravis does.
                let remaining = deadline.saturating_duration_since(Instant::now());
                ensure!(!remaining.is_zero(), USBFS_EXHAUSTED);
                std::thread::sleep(remaining.min(Duration::from_millis(10)));
                self.fill(transfers);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let completion = transfers
                .wait(remaining)
                .context("USB frame capture timed out")?;
            self.submitted -= 1;
            let received = self.receive(completion);
            if !self.starved {
                self.fill(transfers);
            }
            if let Some(frame) = received? {
                return Ok(frame);
            }
        }
    }
    /// Take in the transfers that already completed, keeping what they
    /// hold for `next`. Returns how many frames it holds that `next` has
    /// not returned, whole or in part.
    fn take_in(&mut self, transfers: &mut impl Transfers) -> u32 {
        while self.submitted > 0 {
            let Some(completion) = transfers.wait(Duration::ZERO) else {
                break;
            };
            self.submitted -= 1;
            let received = self.receive(completion);
            if !self.starved {
                self.fill(transfers);
            }
            match received {
                Ok(Some(frame)) => self.taken.push_back(Ok(frame)),
                Ok(None) => {}
                Err(error) => self.taken.push_back(Err(error)),
            }
        }
        let frames = self.taken.iter().filter(|taken| taken.is_ok()).count();
        u32::try_from(frames + usize::from(self.frame.is_some())).unwrap_or(u32::MAX)
    }
    fn receive(&mut self, completion: Completion) -> Result<Option<Frame>> {
        let Completion {
            mut buffer, status, ..
        } = completion;
        let received = if usbfs_refused(status) {
            self.starved = true;
            Ok(None)
        } else if let Err(error) = status {
            self.frame = None;
            Err(error).context("USB stream transfer failed")
        } else {
            self.starved = false;
            let full = buffer.len() == buffer.requested_len();
            self.take(&mut buffer, full)
        };
        buffer.clear();
        self.idle.push(buffer);
        received
    }
    /// Returns the frame a trailer completes. A whole image in a transfer
    /// of the pool becomes the frame's buffer, `transfer` replaced.
    fn take(&mut self, transfer: &mut Buffer, full: bool) -> Result<Option<Frame>> {
        let bytes: &[u8] = transfer;
        let held = |data: &Option<PoolVec>| data.as_ref().map_or(0, PoolVec::len);
        // A ZLP may end a reduced payload on an exact packet boundary. A
        // short/ZLP completion proves a boundary, but not a complete frame.
        if bytes.is_empty() {
            if let Some(pending) = &mut self.frame
                && held(&pending.data) > 0
            {
                pending.payload_ended = true;
            }
            return Ok(None);
        }
        // Never inspect pixels for metadata while their payload is undecided.
        // Recovery leaders are recognized only at an established boundary.
        let metadata = self
            .frame
            .as_ref()
            .is_none_or(|pending| pending.payload_ended || held(&pending.data) == self.expected);
        if metadata && is_header(bytes, LEADER_MAGIC, 52, self.leader) {
            let open = self.frame.take().is_some();
            let mut leader = parse_leader(bytes)?;
            leader.frame.pixel_encoding = self
                .pixel_encodings
                .iter()
                .copied()
                .find(|encoding| encoding.ids(leader.frame.pixel_format).is_ok());
            ensure!(
                !crate::types::IdsPixelFormat::requires_qualification(leader.frame.pixel_format)
                    || leader.frame.pixel_encoding.is_some(),
                Categorized::error(
                    ErrorKind::Unsupported,
                    "USB vendor pixel code requires verified IDS XML layout"
                )
            );
            let maximum = ImageRows::new(&leader.frame, leader.frame.height, leader.x_padding)?;
            ensure!(
                maximum.image_bytes <= self.expected,
                "USB leader image exceeds the configured payload ceiling"
            );
            self.frame = Some(PendingImage {
                leader,
                data: None,
                payload_ended: false,
            });
            ensure!(!open, "USB stream image ended without a trailer");
            return Ok(None);
        }
        // Retire oversized metadata before allocating a trailer copy. Valid
        // leaders were handled above and have their own negotiated bound.
        if metadata && self.frame.is_some() && bytes.len() > self.trailer {
            self.frame = None;
            bail!("USB trailer exceeds its negotiated bound");
        }
        // Without an open image, the data belongs to one already dropped or
        // was left over from before acquisition started.
        let Some(pending) = self.frame.as_mut() else {
            return Ok(None);
        };
        let trailer = if metadata {
            bytes.to_vec()
        } else {
            let remaining = self
                .expected
                .checked_sub(held(&pending.data))
                .context("USB payload exceeds its configured ceiling")?;
            let image = remaining.min(bytes.len());
            if bytes.len() - image > self.trailer {
                self.frame = None;
                bail!("USB trailer exceeds its negotiated bound");
            }
            let trailer = match self.whole {
                // A single payload completion, including a reduced image,
                // takes its pool buffer without copying. A trailer suffix is
                // split ONLY at the independently known maximum byte count.
                Some(len) if pending.data.is_none() => {
                    let mut received = std::mem::replace(transfer, whole_buffer(len)).into_vec();
                    let trailer = received.split_off(image);
                    pending.data = Some(pool().adopt(received));
                    trailer
                }
                _ => {
                    crate::copies::record(crate::copies::Site::UsbAssemble, image);
                    pending
                        .data
                        .get_or_insert_with(|| pool().take_empty(self.expected))
                        .extend_from_slice(&bytes[..image]);
                    bytes[image..].to_vec()
                }
            };
            pending.payload_ended = !full || held(&pending.data) == self.expected;
            if trailer.is_empty() {
                return Ok(None);
            }
            trailer
        };
        let PendingImage { leader, data, .. } =
            self.frame.take().context("USB stream frame missing")?;
        let mut frame = leader.frame;
        let mut data = data.unwrap_or_else(|| pool().take_empty(0));
        ensure!(
            trailer.len() <= self.trailer,
            "USB trailer exceeds its negotiated bound"
        );
        validate_trailer(&trailer, frame.id, self.expected, data.len())?;
        let trailer_size = le16(&trailer, 6)?;
        if leader.chunks {
            ensure!(trailer_size >= 36, "truncated USB extended chunk trailer");
        }
        ensure!(
            trailer_size == 28 || trailer_size >= 32,
            "truncated USB image trailer"
        );
        // Common-only legacy trailers cannot independently authorize a
        // reduced height; their payload must match the complete leader image.
        let height = if trailer_size >= 32 {
            le32(&trailer, 28)?
        } else {
            frame.height
        };
        ensure!(
            (1..=frame.height).contains(&height),
            "USB trailer height does not fit image leader"
        );
        let rows = ImageRows::new(&frame, height, leader.x_padding)?;
        ensure!(
            if leader.chunks {
                rows.image_bytes <= data.len()
            } else {
                rows.image_bytes == data.len()
            },
            "USB image size differs from its confirmed row geometry"
        );
        frame.height = height;
        if leader.chunks {
            let mut chunks = ChunkData::parse(
                &data,
                rows.image_bytes,
                rows.image_bytes,
                ByteOrder::Little,
                le32(&trailer, 32)?,
            )?;
            chunks.image_layout = Some(ImageLayout {
                pfnc_code: frame.pixel_encoding.is_none().then_some(frame.pixel_format),
                pixel_encoding: frame.pixel_encoding,
                width: frame.width,
                height,
                offset: 0,
                row_bytes: rows.row_bytes,
                row_stride: rows.row_stride,
                padding_y: 0,
                byte_order: Some(ByteOrder::Little),
                continuous_packing: false,
            });
            frame.data = if leader.x_padding == 0 {
                chunks.retain_shared(data.freeze())
            } else {
                // Keep every original row and chunk byte immutable; copy only
                // the pixels, rather than copying and then compacting padding.
                chunks.payload = data.freeze();
                let mut pixels = pool().take_stale(rows.pixel_bytes);
                crate::copies::record(crate::copies::Site::ChunkImage, rows.pixel_bytes);
                for (source, target) in chunks.payload[..rows.image_bytes]
                    .chunks_exact(rows.row_stride)
                    .zip(pixels.chunks_exact_mut(rows.row_bytes))
                {
                    target.copy_from_slice(&source[..rows.row_bytes]);
                }
                pixels.freeze()
            };
            frame.chunks = Some(chunks.into());
        } else {
            if leader.x_padding > 0 {
                crate::copies::record(
                    crate::copies::Site::UsbAssemble,
                    rows.pixel_bytes - rows.row_bytes,
                );
                for row in 1..frame.height as usize {
                    let source = row * rows.row_stride;
                    data.copy_within(source..source + rows.row_bytes, row * rows.row_bytes);
                }
                data.truncate(rows.pixel_bytes);
            }
            frame.data = data.freeze();
        }
        Ok(Some(frame))
    }
    /// Cancel every submitted transfer and take its buffer back.
    fn drain(&mut self, transfers: &mut impl Transfers, deadline: Instant) -> Result<()> {
        transfers.cancel_all();
        while self.submitted > 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let mut completion = transfers
                .wait(remaining)
                .context("USB transfer cancellation timed out")?;
            self.submitted -= 1;
            completion.buffer.clear();
            self.idle.push(completion.buffer);
        }
        self.frame = None;
        self.starved = false;
        Ok(())
    }
}
/// A transfer of `len` bytes whose buffer is the frame pool's, so a frame
/// can take it.
fn whole_buffer(len: usize) -> Buffer {
    let mut buffer = Buffer::from(pool().take_vec(len));
    // An IN transfer requests `len` bytes and starts empty.
    buffer.clear();
    buffer
}
fn aligned(size: usize, alignment: usize) -> Result<usize> {
    ensure!(
        alignment.is_power_of_two(),
        "invalid USB transfer alignment"
    );
    Ok(size
        .checked_add(alignment - 1)
        .context("USB transfer size overflow")?
        & !(alignment - 1))
}
fn le16(data: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        data.get(offset..offset + 2)
            .context("truncated USB3 Vision packet")?
            .try_into()?,
    ))
}
fn le32(data: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        data.get(offset..offset + 4)
            .context("truncated USB3 Vision packet")?
            .try_into()?,
    ))
}
fn le64(data: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        data.get(offset..offset + 8)
            .context("truncated USB3 Vision packet")?
            .try_into()?,
    ))
}
/// A command packet. Every command asks for an acknowledgement; a resend
/// keeps the original request ID and sets the `CommandResend` flag.
fn command(cmd: u16, id: u16, resend: bool, payload: &[u8]) -> Result<Vec<u8>> {
    let length =
        u16::try_from(payload.len()).context("UVCP command payload exceeds 65535 bytes")?;
    let flags = if resend { 0xc000u16 } else { 0x4000 };
    let mut packet = Vec::with_capacity(12 + payload.len());
    packet.extend_from_slice(&MAGIC.to_le_bytes());
    packet.extend_from_slice(&flags.to_le_bytes());
    packet.extend_from_slice(&cmd.to_le_bytes());
    packet.extend_from_slice(&length.to_le_bytes());
    packet.extend_from_slice(&id.to_le_bytes());
    packet.extend_from_slice(payload);
    Ok(packet)
}
/// An acknowledgement status of error severity.
#[derive(Debug)]
struct UvcpStatus(u16);
impl std::fmt::Display for UvcpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "camera rejected USB3 Vision command with status 0x{:04x}",
            self.0
        )
    }
}
impl std::error::Error for UvcpStatus {}
fn ack(packet: &[u8], id: u16) -> Result<(u16, &[u8])> {
    ensure!(
        le32(packet, 0)? == MAGIC,
        "invalid UVCP acknowledgement magic"
    );
    ensure!(le16(packet, 10)? == id, "unexpected UVCP request ID");
    let status = le16(packet, 4)?;
    // A warning or info status, with bit 15 clear, means GenCP executed the
    // command, for example after rounding a written value.
    if status & 0x8000 != 0 {
        return Err(UvcpStatus(status).into());
    }
    let length = le16(packet, 8)? as usize;
    ensure!(
        packet.len() >= 12 + length,
        "truncated UVCP acknowledgement payload"
    );
    Ok((le16(packet, 6)?, &packet[12..12 + length]))
}
/// Check a `WriteMem` acknowledgement. `GenCP` lets it carry the written length
/// only when the camera's Device Capability bit 11 is set; otherwise it is
/// empty.
fn write_acknowledged(response: &[u8], len: usize) -> Result<()> {
    ensure!(
        response.is_empty() || (response.len() == 4 && le16(response, 2)? as usize == len),
        "camera returned invalid UVCP write acknowledgement"
    );
    Ok(())
}
/// The camera's control endpoints, which carry UVCP commands out and their
/// acknowledgements back.
trait Channel: Send {
    /// Returns how many bytes were sent.
    fn send(&mut self, packet: Vec<u8>, timeout: Duration) -> Result<usize, TransferError>;
    fn receive(&mut self, len: usize, timeout: Duration) -> Result<Vec<u8>, TransferError>;
    fn clear_halt(&mut self);
}
struct ControlEndpoints {
    input: Endpoint<Bulk, In>,
    output: Endpoint<Bulk, Out>,
}
impl Channel for ControlEndpoints {
    fn send(&mut self, packet: Vec<u8>, timeout: Duration) -> Result<usize, TransferError> {
        let completion = self.output.transfer_blocking(packet.into(), timeout);
        completion.status.map(|()| completion.actual_len)
    }
    fn receive(&mut self, len: usize, timeout: Duration) -> Result<Vec<u8>, TransferError> {
        let size = aligned(len, self.input.max_packet_size())
            .map_err(|_| TransferError::InvalidArgument)?;
        self.input
            .transfer_blocking(Buffer::new(size), timeout)
            .into_result()
            .map(Buffer::into_vec)
    }
    fn clear_halt(&mut self) {
        let _ = self.output.clear_halt().wait();
        let _ = self.input.clear_halt().wait();
    }
}
/// Sends per command: `GenCP` asks a sender to retry a failed command three
/// times.
const ATTEMPTS: u32 = 4;
const STATUS_BUSY: u16 = 0x8007;
const STATUS_RESEND_NOT_SUPPORTED: u16 = 0xa001;
/// UVCP commands over a camera's control channel.
struct Control<C> {
    channel: C,
    id: u16,
    /// How long a command may take, at least the camera's maximum response
    /// time.
    timeout: Duration,
    /// The camera's maximum response time, once read from it.
    response: Duration,
    max_command: usize,
    max_ack: usize,
}
impl<C: Channel> Control<C> {
    const fn new(channel: C, timeout: Duration) -> Self {
        Self {
            channel,
            id: 0,
            timeout,
            response: Duration::ZERO,
            max_command: 65548,
            max_ack: 65548,
        }
    }
    /// Send a command and wait for its acknowledgement within the command
    /// timeout. As `GenCP` describes, a lost or corrupt acknowledgement or a
    /// failed transfer resends the command after the response time with the
    /// same request ID and the `CommandResend` flag, so a late acknowledgement
    /// still answers it. A busy camera, which did not run the command, gets
    /// it again as a new command, as does one that cannot take a resend,
    /// which may then run a write whose acknowledgement was lost twice.
    fn transact(&mut self, cmd: u16, payload: &[u8], caller: Option<Instant>) -> Result<Vec<u8>> {
        ensure!(
            12 + payload.len() <= self.max_command,
            "UVCP command exceeds camera transfer limit"
        );
        if let Some(deadline) = caller {
            crate::types::control_remaining(deadline)?;
        }
        let started = Instant::now();
        let deadline = caller.map_or(started + self.timeout, |stop| {
            stop.min(started + self.timeout)
        });
        let hard_deadline = caller.map_or_else(
            || started + self.timeout.max(Duration::from_secs(60)),
            |stop| stop.min(started + self.timeout.max(Duration::from_secs(60))),
        );
        let window = self.response.max(self.timeout / ATTEMPTS);
        self.id = self.id.wrapping_add(1).max(1);
        let mut resend = false;
        let mut sent = 0;
        let mut failure = anyhow!("USB3 Vision command was not sent");
        while sent < ATTEMPTS {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            sent += 1;
            let window = window.min(remaining);
            let error = match self.attempt(
                cmd,
                payload,
                resend,
                window,
                hard_deadline,
                caller.is_some(),
            ) {
                Ok(data) => return Ok(data),
                Err(error) => error,
            };
            match error.downcast_ref::<UvcpStatus>().map(|status| status.0) {
                Some(STATUS_BUSY) => {
                    let pause = caller.map_or(Duration::from_millis(10), |stop| {
                        Duration::from_millis(10)
                            .min(stop.saturating_duration_since(Instant::now()))
                    });
                    std::thread::sleep(pause);
                    self.id = self.id.wrapping_add(1).max(1);
                    resend = false;
                }
                Some(STATUS_RESEND_NOT_SUPPORTED) if resend => {
                    self.id = self.id.wrapping_add(1).max(1);
                    resend = false;
                }
                Some(_) => return Err(error),
                None if error.chain().any(|cause| {
                    cause.downcast_ref() == Some(&TransferError::Disconnected)
                        || (caller.is_some() && cause.downcast_ref() == Some(&TransferError::Stall))
                }) =>
                {
                    return Err(error);
                }
                None => resend = true,
            }
            failure = error;
        }
        if let Some(deadline) = caller {
            crate::types::control_remaining(deadline)?;
        }
        let plural = if sent == 1 { "" } else { "s" };
        Err(failure.context(format!(
            "USB3 Vision command failed after {sent} attempt{plural}"
        )))
    }
    fn attempt(
        &mut self,
        cmd: u16,
        payload: &[u8],
        resend: bool,
        window: Duration,
        hard_deadline: Instant,
        scoped: bool,
    ) -> Result<Vec<u8>> {
        let packet = command(cmd, self.id, resend, payload)?;
        let len = packet.len();
        if scoped {
            crate::types::control_remaining(hard_deadline)?;
        }
        let sent = self.channel.send(packet, window);
        if scoped {
            crate::types::control_remaining(hard_deadline)?;
        }
        let sent = self
            .unhalt(sent, scoped)
            .context("USB control write failed or timed out")?;
        ensure!(sent == len, "short USB control write");
        let mut deadline = (Instant::now() + window).min(hard_deadline);
        for _ in 0..64 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Categorized::error(
                    ErrorKind::Timeout,
                    "USB camera control acknowledgement timed out",
                ));
            }
            let response = self.channel.receive(self.max_ack, remaining);
            if scoped {
                crate::types::control_remaining(hard_deadline)?;
            }
            let response = self
                .unhalt(response, scoped)
                .context("USB control read failed or timed out")?;
            // A stale response may remain after a timed-out operation.
            if response.len() >= 12 && le16(&response, 10)? != self.id {
                continue;
            }
            let (response_cmd, data) = ack(&response, self.id)?;
            if response_cmd == PENDING_ACK {
                ensure!(data.len() >= 4, "truncated UVCP pending acknowledgement");
                let extension = Duration::from_millis(u64::from(le16(data, 2)?));
                ensure!(
                    !extension.is_zero(),
                    "camera sent zero pending acknowledgement timeout"
                );
                deadline = (Instant::now() + extension).min(hard_deadline);
                continue;
            }
            ensure!(
                response_cmd == cmd + 1,
                "unexpected UVCP acknowledgement command 0x{response_cmd:04x}"
            );
            return Ok(data.to_vec());
        }
        bail!("too many pending or stale UVCP acknowledgements")
    }
    /// Clear a halt the camera signalled before the command is sent again.
    fn unhalt<T>(
        &mut self,
        result: Result<T, TransferError>,
        scoped: bool,
    ) -> Result<T, TransferError> {
        // nusb clear_halt().wait() has no timeout. Named bounded operations
        // fail on Stall rather than entering unbounded endpoint recovery.
        if !scoped && matches!(result, Err(TransferError::Stall)) {
            self.channel.clear_halt();
        }
        result
    }
}
impl<C: Channel> Control<C> {
    fn read_scoped(
        &mut self,
        address: u64,
        length: usize,
        caller: Option<Instant>,
    ) -> Result<Vec<u8>> {
        if let Some(deadline) = caller {
            crate::types::control_remaining(deadline)?;
        }
        ensure!(length <= MAX_FRAME, "USB memory request exceeds size limit");
        address
            .checked_add(length as u64)
            .context("USB memory range overflow")?;
        let chunk_size = self.max_ack.saturating_sub(12).min(u16::MAX as usize);
        ensure!(chunk_size > 0, "invalid USB read transfer limit");
        let mut result = Vec::with_capacity(length);
        while result.len() < length {
            let len = (length - result.len()).min(chunk_size);
            let mut request = [0u8; 12];
            request[..8].copy_from_slice(&(address + result.len() as u64).to_le_bytes());
            request[10..].copy_from_slice(
                &u16::try_from(len)
                    .context("USB read chunk exceeds u16")?
                    .to_le_bytes(),
            );
            let bytes = self.transact(READ_CMD, &request, caller)?;
            ensure!(bytes.len() == len, "camera returned short UVCP memory data");
            result.extend_from_slice(&bytes);
        }
        if let Some(deadline) = caller {
            crate::types::control_remaining(deadline)?;
        }
        Ok(result)
    }
    fn write_scoped(&mut self, address: u64, data: &[u8], caller: Option<Instant>) -> Result<()> {
        if let Some(deadline) = caller {
            crate::types::control_remaining(deadline)?;
        }
        ensure!(
            data.len() <= MAX_FRAME,
            "USB memory write exceeds size limit"
        );
        address
            .checked_add(data.len() as u64)
            .context("USB memory range overflow")?;
        let size = self
            .max_command
            .saturating_sub(20)
            .min(u16::MAX as usize - 8);
        ensure!(size > 0, "invalid USB write transfer limit");
        let mut request = Vec::new();
        for (i, chunk) in data.chunks(size).enumerate() {
            request.clear();
            request.reserve(8 + chunk.len());
            request.extend_from_slice(&(address + (i * size) as u64).to_le_bytes());
            request.extend_from_slice(chunk);
            let response = self.transact(WRITE_CMD, &request, caller)?;
            write_acknowledged(&response, chunk.len())?;
        }
        if let Some(deadline) = caller {
            crate::types::control_remaining(deadline)?;
        }
        Ok(())
    }
}
impl<C: Channel> RegisterIo for Control<C> {
    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        self.read_scoped(address, length, None)
    }
    fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
        self.write_scoped(address, data, None)
    }
    fn read_memory_until(
        &mut self,
        address: u64,
        length: usize,
        deadline: Instant,
    ) -> Result<Vec<u8>> {
        self.read_scoped(address, length, Some(deadline))
    }
    fn write_memory_until(&mut self, address: u64, data: &[u8], deadline: Instant) -> Result<()> {
        self.write_scoped(address, data, Some(deadline))
    }
}

impl UsbBackend {
    fn u32(&mut self, address: u64) -> Result<u32> {
        le32(&self.read_memory(address, 4)?, 0)
    }
    fn u64(&mut self, address: u64) -> Result<u64> {
        le64(&self.read_memory(address, 8)?, 0)
    }
    fn put32(&mut self, offset: u64, value: usize) -> Result<()> {
        let value = u32::try_from(value).context("stream register value exceeds u32")?;
        self.write_memory(self.sirm + offset, &value.to_le_bytes())
    }
    fn start_stream(&mut self) -> Result<()> {
        // The stream registers must not change while the stream interface
        // is enabled, as a program streaming from the camera may have left
        // it when it ended abruptly.
        self.put32(SIRM_CONTROL, 0)?;
        let info = self.u32(self.sirm)?;
        let alignment = 1usize
            .checked_shl(info >> 24)
            .context("invalid USB camera stream alignment")?
            .max(self.stream_in.max_packet_size());
        ensure!(
            alignment.is_power_of_two() && alignment <= MAX_TRANSFER,
            "unsupported USB camera stream alignment"
        );
        let expected = self.u64(self.sirm + 0x08)?;
        ensure!(
            expected > 0 && expected <= MAX_FRAME as u64,
            "USB stream required payload is outside 1..256 MiB"
        );
        let expected =
            usize::try_from(expected).context("USB payload exceeds host address size")?;
        let leader = aligned((self.u32(self.sirm + 0x10)? as usize).max(52), alignment)?;
        let trailer = aligned((self.u32(self.sirm + 0x14)? as usize).max(28), alignment)?;
        ensure!(
            leader <= MAX_TRANSFER && trailer <= MAX_TRANSFER,
            "USB stream header transfer is too large"
        );
        let layout = layout(
            expected,
            leader,
            trailer,
            alignment,
            usbfs_window(),
            crate::host::memory_budget(),
        )?;
        self.put32(0x18, leader)?;
        self.put32(0x2c, trailer)?;
        self.put32(0x1c, layout.block)?;
        self.put32(0x20, layout.count)?;
        self.put32(0x24, layout.tail)?;
        self.put32(0x28, 0)?;
        self.reset_stream_endpoint()?;
        // Submit the transfers before AcquisitionStart.
        let whole = layout.whole.then_some(layout.request);
        let mut stream = Stream::new(expected, leader, trailer, whole);
        stream.pixel_encodings.clone_from(&self.pixel_encodings);
        stream.queue(&mut self.stream_in, layout.transfers, layout.request);
        self.stream = Some(stream);
        self.put32(SIRM_CONTROL, 1)
    }
    /// Halt the stream endpoint, then clear the halt, as Aravis and the NI
    /// driver reset it, so the camera drops any partly sent block. A host
    /// or camera may ignore the halt; clearing it still resets the endpoint.
    fn reset_stream_endpoint(&mut self) -> Result<()> {
        let _ = self
            .stream_interface
            .control_out(
                halt(self.stream_in.endpoint_address()),
                Duration::from_millis(500),
            )
            .wait();
        self.stream_in
            .clear_halt()
            .wait()
            .context("cannot reset USB stream endpoint")
    }
    /// Restart the stream after the camera halted its endpoint: disable the
    /// stream interface, take back every transfer, reset the endpoint, and
    /// submit the transfers again before enabling it.
    fn restart_stream(&mut self) -> Result<()> {
        self.put32(SIRM_CONTROL, 0)?;
        let deadline = Instant::now() + self.control.timeout;
        let stream = self.stream.as_mut().context("USB acquisition stopped")?;
        stream.drain(&mut self.stream_in, deadline)?;
        self.reset_stream_endpoint()?;
        let stream = self.stream.as_mut().context("USB acquisition stopped")?;
        stream.fill(&mut self.stream_in);
        self.put32(SIRM_CONTROL, 1)
    }
}
impl RegisterIo for UsbBackend {
    fn read_memory_until(
        &mut self,
        address: u64,
        length: usize,
        deadline: Instant,
    ) -> Result<Vec<u8>> {
        self.control.read_memory_until(address, length, deadline)
    }
    fn write_memory_until(&mut self, address: u64, data: &[u8], deadline: Instant) -> Result<()> {
        self.control.write_memory_until(address, data, deadline)
    }
    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        self.control.read_memory(address, length)
    }
    fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
        self.control.write_memory(address, data)
    }
}
/// Validate the wire count before converting it to a host-sized allocation.
fn manifest_size(count: u64) -> Result<usize> {
    ensure!(
        (1..=64).contains(&count),
        "invalid USB3 Vision manifest entry count"
    );
    Ok(usize::try_from(count).context("USB manifest count exceeds host address size")? * 64)
}

/// Each manifest entry for a raw or zipped device XML, with its address
/// and size, newest version first. `GenCP` puts the file type in bits 0-2 of
/// the entry's second word, where 1 is a Buffer XML holding only chunk
/// nodes, and the 6-bit file format in bits 10-15.
fn device_xml_entries(entries: &[u8]) -> Result<Vec<([u8; 64], u64, u64)>> {
    let mut candidates = Vec::new();
    for entry in entries.as_chunks::<64>().0 {
        let schema = le32(entry, 4)?;
        if schema & 7 == 0 && (schema >> 10) & 0x3f <= 1 {
            candidates.push((le32(entry, 0)?, *entry, le64(entry, 8)?, le64(entry, 16)?));
        }
    }
    candidates.sort_by_key(|e| std::cmp::Reverse(e.0));
    Ok(candidates
        .into_iter()
        .map(|(_, entry, address, size)| (entry, address, size))
        .collect())
}
#[derive(Debug)]
struct ImageLeader {
    frame: Frame,
    chunks: bool,
    x_padding: usize,
}

/// An open frame owns both its bytes and the boundary proof for its trailer.
/// Dropping it after an error or drain cannot leave state on the next frame.
struct PendingImage {
    leader: ImageLeader,
    data: Option<PoolVec>,
    payload_ended: bool,
}

struct ImageRows {
    row_bytes: usize,
    row_stride: usize,
    image_bytes: usize,
    pixel_bytes: usize,
}
impl ImageRows {
    fn new(frame: &Frame, height: u32, x_padding: usize) -> Result<Self> {
        crate::frame::validate_encoding(frame)?;
        if let Some(format) = crate::frame::bicolor::Format::from_code(frame.pixel_format) {
            format.validate_width(frame.width)?;
        }
        let row_bytes = crate::frame::frame_row_bytes(frame);
        ensure!(row_bytes > 0, "invalid USB image pixel format");
        let row_stride = row_bytes
            .checked_add(x_padding)
            .context("USB image row stride overflow")?;
        let pixel_bytes = crate::frame::image_len(
            frame.pixel_format,
            frame.pixel_encoding,
            frame.width,
            height,
        )
        .context("USB normalized image size overflow")?;
        // Odd planar/chroma dimensions can occupy more than their nominal
        // bits-per-pixel row size. Without padding, use the complete PFNC size.
        let image_bytes = if x_padding == 0 {
            pixel_bytes
        } else {
            row_stride
                .checked_mul(height as usize)
                .context("USB image size overflow")?
        };
        ensure!(
            height > 0 && image_bytes <= MAX_FRAME,
            "USB image row geometry exceeds the frame limit"
        );
        if x_padding > 0 && crate::frame::planes(frame.pixel_format, frame.width, height).is_some()
        {
            return Err(Categorized::error(
                ErrorKind::Unsupported,
                "USB row padding for planar pixel formats requires an explicit plane layout",
            ));
        }
        Ok(Self {
            row_bytes,
            row_stride,
            image_bytes,
            pixel_bytes,
        })
    }
}
fn parse_leader(bytes: &[u8]) -> Result<ImageLeader> {
    ensure!(
        le32(bytes, 0)? == LEADER_MAGIC,
        "USB stream is out of sync: expected image leader"
    );
    let size = le16(bytes, 6)? as usize;
    ensure!(
        size >= 52 && size <= bytes.len(),
        "invalid UVSP leader size"
    );
    let payload_type = le16(bytes, 18)?;
    if !matches!(payload_type, 1 | 0x4001) {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            format!(
                "unsupported USB stream payload type 0x{payload_type:04x}; select an image or image extended chunk mode"
            ),
        ));
    }
    let width = le32(bytes, 32)?;
    let height = le32(bytes, 36)?;
    ensure!(
        width != 0 && height != 0,
        "camera returned zero image dimensions"
    );
    Ok(ImageLeader {
        frame: Frame {
            pixel_encoding: None,
            device_timestamp: Some(crate::types::DeviceTimestamp {
                ticks: le64(bytes, 20)?,
                frequency_hz: Some(1_000_000_000),
            }),
            id: le64(bytes, 8)?,
            width,
            height,
            pixel_format: le32(bytes, 28)?,
            device_ts_ns: Some(le64(bytes, 20)?),
            wall_ms: 0,
            received_monotonic_ns: None,
            data: Vec::new().into(),
            jpeg: None,
            chunks: None,
            multipart: None,
        },
        chunks: payload_type == 0x4001,
        // The following 16 bits are reserved, not vertical padding. See NI's
        // U3V image_leader_info and Basler's Line Pitch feature documentation.
        x_padding: usize::from(le16(bytes, 48)?),
    })
}
fn validate_trailer(bytes: &[u8], frame_id: u64, maximum: usize, received: usize) -> Result<()> {
    ensure!(
        le32(bytes, 0)? == TRAILER_MAGIC,
        "USB stream is out of sync: expected image trailer"
    );
    let size = le16(bytes, 6)? as usize;
    ensure!(
        size >= 28 && size <= bytes.len(),
        "invalid UVSP trailer size"
    );
    ensure!(le64(bytes, 8)? == frame_id, "USB stream frame ID mismatch");
    ensure!(
        le16(bytes, 16)? == 0,
        "USB stream trailer reports an incomplete frame"
    );
    ensure!(
        received > 0 && received <= maximum && le64(bytes, 20)? == u64::try_from(received)?,
        "USB stream frame size mismatch"
    );
    Ok(())
}
impl Backend for UsbBackend {
    fn configure_pixel_encodings(
        &mut self,
        encodings: &[crate::types::PixelEncoding],
    ) -> Result<()> {
        ensure!(
            self.stream.is_none() && encodings.len() <= crate::types::IdsPixelFormat::ALL.len(),
            "invalid USB pixel encoding configuration"
        );
        ensure!(
            encodings.is_empty() || self.ids_vendor,
            Categorized::error(
                ErrorKind::Unsupported,
                "IDS encoding requires an IDS USB identity"
            )
        );
        for &encoding in encodings {
            let crate::types::PixelEncoding::Ids {
                code,
                gentl_namespace,
                ..
            } = encoding
            else {
                return Err(Categorized::error(
                    ErrorKind::Unsupported,
                    "USB accepts only qualified IDS vendor layouts",
                ));
            };
            encoding.ids(code)?;
            ensure!(
                gentl_namespace.is_none(),
                "USB pixel encoding contains a GenTL namespace"
            );
        }
        self.pixel_encodings = encodings.to_vec();
        Ok(())
    }
    fn xml(&mut self) -> Result<String> {
        let address = self.u64(0x01d0)?;
        let size = manifest_size(self.u64(address)?)?;
        let entries = self.read_memory(
            address
                .checked_add(8)
                .context("manifest address overflow")?,
            size,
        )?;
        let mut last_error = anyhow!("USB manifest contains no raw or ZIP device XML entry");
        // The manifest entry (version, address, size and a SHA-1 where the
        // device gives one) and the model and device version name the XML
        // on disk; a copy kept there is parsed below like a download.
        let identity = [self.read_memory(0x0044, 64)?, self.read_memory(0x00c4, 64)?].concat();
        for (entry, address, size) in device_xml_entries(&entries)? {
            if size == 0 || size > 32 * 1024 * 1024 {
                last_error = anyhow!("GenICam manifest XML size is outside 1..32 MiB");
                continue;
            }
            let key = crate::xml_cache::key(&[&entry, &identity]);
            if let Some(xml) = crate::xml_cache::load(&key) {
                if crate::genicam::NodeMap::parse(&xml)
                    .is_ok_and(|nodes| nodes.unresolved().is_none())
                {
                    return Ok(xml);
                }
                crate::xml_cache::forget(&key);
            }
            match self
                .read_memory(
                    address,
                    usize::try_from(size).context("USB XML size exceeds host address size")?,
                )
                .and_then(|data| decode_xml(&data))
                .and_then(|xml| {
                    crate::genicam::NodeMap::parse(&xml)?;
                    Ok(xml)
                }) {
                Ok(xml) => {
                    crate::xml_cache::store(&key, &xml);
                    return Ok(xml);
                }
                Err(error) => last_error = error,
            }
        }
        Err(last_error.context("cannot load USB3 Vision GenICam XML"))
    }
    fn start(&mut self, _payload_size: usize) -> Result<()> {
        ensure!(self.stream.is_none(), "USB acquisition already started");
        // Take back transfers that a stop whose cancellation timed out left
        // pending, so the new stream counts only its own.
        self.stop()?;
        let started = self.start_stream();
        if started.is_err() {
            if self.stream.is_none() {
                let _ = self.put32(SIRM_CONTROL, 0);
            }
            let _ = self.stop();
        }
        started
    }
    fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
        let deadline = Instant::now() + timeout;
        let stream = self
            .stream
            .as_mut()
            .context("USB acquisition has not started")?;
        let frame = stream.next(&mut self.stream_in, deadline);
        if let Err(error) = &frame
            && error
                .chain()
                .any(|cause| cause.downcast_ref() == Some(&TransferError::Stall))
        {
            // A control timeout during recovery must not read as a frame wait.
            self.restart_stream().map_err(|restart| {
                Categorized::error(
                    ErrorKind::OperationFailed,
                    format!("cannot recover the stalled USB stream endpoint: {restart:#}"),
                )
            })?;
        }
        frame
    }
    fn buffered_frames(&mut self) -> u32 {
        match self.stream.as_mut() {
            Some(stream) => stream.take_in(&mut self.stream_in),
            None => 0,
        }
    }
    fn stop(&mut self) -> Result<()> {
        let was_started = self.stream.take().is_some();
        let result = if was_started {
            self.put32(SIRM_CONTROL, 0)
        } else {
            Ok(())
        };
        self.stream_in.cancel_all();
        // nusb cancellation completes outstanding requests asynchronously. Drain
        // them before allowing a later start to create a fresh ordered queue.
        let deadline = Instant::now() + self.control.timeout;
        while self.stream_in.pending() != 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(anyhow!("USB transfer cancellation timed out"));
            }
            self.stream_in
                .wait_next_complete(remaining)
                .context("USB transfer cancellation timed out")?;
        }
        result
    }
}
impl Drop for UsbBackend {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_failures_name_the_fix_for_this_os() {
        use nusb::ErrorKind as Usb;
        let node = Some("/dev/bus/usb/002/003");
        let cases = [
            (
                Stage::Open,
                Usb::PermissionDenied,
                "linux",
                ErrorKind::PermissionDenied,
                "/dev/bus/usb/002/003). Run `capturefab doctor` for the udev rule",
            ),
            (
                Stage::Open,
                Usb::PermissionDenied,
                "macos",
                ErrorKind::PermissionDenied,
                "Run `capturefab doctor`.",
            ),
            (
                Stage::ClaimControl,
                Usb::Busy,
                "linux",
                ErrorKind::CameraBusy,
                "in use by another application",
            ),
            (
                Stage::ClaimStream,
                Usb::Busy,
                "windows",
                ErrorKind::CameraBusy,
                "Close it and connect again.",
            ),
            (
                Stage::Open,
                Usb::Disconnected,
                "macos",
                ErrorKind::Disconnected,
                "reconnect it and discover again",
            ),
            (
                Stage::Open,
                Usb::NotFound,
                "windows",
                ErrorKind::Unavailable,
                "Assign WinUSB to it; see https://capturefab.com/docs.html#winusb",
            ),
            (
                Stage::ClaimControl,
                Usb::Unsupported,
                "windows",
                ErrorKind::Unavailable,
                "WinUSB",
            ),
            (
                Stage::ClaimControl,
                Usb::Other,
                "windows",
                ErrorKind::OperationFailed,
                "Cannot claim the control interface of USB3 Vision camera acA. If Windows is not using",
            ),
            (
                Stage::ClaimControl,
                Usb::Other,
                "macos",
                ErrorKind::Unavailable,
                "another driver or app may hold it",
            ),
            (
                Stage::Open,
                Usb::Other,
                "linux",
                ErrorKind::OperationFailed,
                "Cannot open USB3 Vision camera acA (/dev/bus/usb/002/003).",
            ),
        ];
        for (stage, kind, os, category, says) in cases {
            let (got, message) = access_error(stage, kind, "acA", node, os);
            assert_eq!(got, category, "{kind:?} on {os}: {message}");
            assert!(message.contains(says), "{kind:?} on {os}: {message}");
            assert!(message.contains("acA"), "{message}");
        }
        assert!(
            access_error(
                Stage::ClaimStream,
                Usb::PermissionDenied,
                "acA",
                None,
                "linux"
            )
            .1
            .starts_with("No permission to claim the stream interface of USB3 Vision camera acA. ")
        );
        // The category is typed, so the exit code holds whatever the words.
        let (kind, message) =
            access_error(Stage::Open, Usb::PermissionDenied, "acA", node, "linux");
        let error = Categorized::error(kind, message).context("connect usb:2676:ba02:1");
        assert_eq!(crate::cli::error_code(&error), ("permission_denied", 3));
        // On Linux the udev rule doctor writes is the hint, and it survives
        // the trip from the worker to the CLI.
        let error = refusal(
            Stage::Open,
            Usb::PermissionDenied,
            &"Permission denied",
            "acA",
            node,
            "linux",
        );
        let rebuilt =
            crate::types::error_from_tag(crate::types::error_tag(&error), format!("{error:#}"));
        let classified = crate::types::classify(&rebuilt);
        assert_eq!(classified.kind.code(), ("permission_denied", 3));
        assert_eq!(classified.hint, Some(crate::doctor::usb_access_fix()));
        assert!(
            format!("{rebuilt:#}").ends_with("(USB: Permission denied)"),
            "{rebuilt:#}"
        );
        let elsewhere = refusal(
            Stage::Open,
            Usb::PermissionDenied,
            &"Permission denied",
            "acA",
            None,
            "macos",
        );
        assert_eq!(crate::types::classify(&elsewhere).hint, None);
    }
    #[test]
    fn cameras_without_a_product_string_are_named_by_usb_ids() {
        assert_eq!(display_name(Some(" acA1920 "), 0x2676, 0xba02), "acA1920");
        assert_eq!(display_name(Some(""), 0x2676, 0xba02), "2676:ba02");
        assert_eq!(
            display_name(Some("acA\x1b[2J\n"), 0x2676, 0xba02),
            "acA\u{fffd}[2J"
        );
        let name = display_name(None, 0x2676, 0xba02);
        let (_, message) = access_error(Stage::Open, nusb::ErrorKind::Busy, &name, None, "linux");
        assert!(
            message.starts_with("USB3 Vision camera 2676:ba02 is in use"),
            "{message}"
        );
    }
    #[test]
    fn u3v_is_not_every_miscellaneous_device() {
        assert!(is_u3v_interface(0xef, 5, 0));
        assert!(!is_u3v_interface(0xef, 2, 1));
        assert!(!is_u3v_interface(0xef, 5, 2));
    }
    #[test]
    fn device_info_descriptor_names_its_strings() {
        // bLength, type, subtype, GenCP 1.0, U3V 1.0, then iDeviceGUID,
        // iVendorName, iModelName, iFamilyName, iDeviceVersion,
        // iManufacturerInfo, iSerialNumber, iUserDefinedName, bmSpeedSupport.
        let descriptor = [
            0x14, 0x24, 0x01, 0, 0, 1, 0, 0, 0, 1, 0, 4, 1, 2, 0, 5, 0, 3, 6, 0x0c,
        ];
        assert_eq!(name_indexes(&descriptor), Some([4, 1, 2, 3]));
        assert_eq!(name_indexes(&descriptor[..17]), None);
        assert_eq!(name_indexes(&[7, 5, 0x81, 2, 0, 4, 0]), None);
    }
    #[test]
    fn camera_id_prefers_the_serial_then_the_guid() {
        let mut names = DeviceNames {
            serial: Some("40012345".into()),
            guid: Some("2676005E0A1B".into()),
            ..DeviceNames::default()
        };
        let id = |names: &DeviceNames| camera_id(0x2676, 0xba02, names, "3:7");
        assert_eq!(id(&names), "usb:2676:ba02:40012345");
        names.serial = None;
        assert_eq!(id(&names), "usb:2676:ba02:2676005E0A1B");
        names.guid = None;
        assert_eq!(id(&names), "usb:2676:ba02:3:7");
        // Descriptor strings come from the device: no terminal escapes in
        // what `list` shows, and nothing but spaces becomes no name at all.
        let names = printable_names(DeviceNames {
            vendor: Some("Acme\x1b]0;title\x07".into()),
            model: Some(" Cam\x1b[31m ".into()),
            serial: Some("\r\n".into()),
            guid: None,
        });
        assert_eq!(
            names.vendor.as_deref(),
            Some("Acme\u{fffd}]0;title\u{fffd}")
        );
        assert_eq!(names.model.as_deref(), Some("Cam\u{fffd}[31m"));
        assert_eq!(names.serial, None);
    }
    #[test]
    fn uvcp_little_endian_wire_packet() {
        let payload = [0x12, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 0];
        let packet = command(READ_CMD, 7, false, &payload).unwrap();
        assert_eq!(
            &packet[..12],
            &[0x55, 0x33, 0x56, 0x43, 0, 0x40, 0, 8, 12, 0, 7, 0]
        );
        let resend = command(READ_CMD, 7, true, &payload).unwrap();
        assert_eq!(&resend[4..6], &[0, 0xc0]);
        let mut response = command(READ_CMD + 1, 7, false, &[1, 2, 3, 4]).unwrap();
        response[4..6].fill(0);
        assert_eq!(ack(&response, 7).unwrap(), (0x0801, &[1, 2, 3, 4][..]));
        ack(&response, 8).unwrap_err();
        // Warning severity: the command ran.
        response[4..6].copy_from_slice(&0x4001u16.to_le_bytes());
        assert_eq!(ack(&response, 7).unwrap(), (0x0801, &[1, 2, 3, 4][..]));
        response[4..6].copy_from_slice(&0x8003u16.to_le_bytes());
        let error = ack(&response, 7).unwrap_err();
        assert_eq!(error.downcast_ref::<UvcpStatus>().unwrap().0, 0x8003);
    }
    #[test]
    fn uvcp_payload_length_cannot_wrap() {
        let mut payload = vec![0; usize::from(u16::MAX)];
        let packet = command(READ_CMD, 7, false, &payload).unwrap();
        assert_eq!(&packet[8..10], &u16::MAX.to_le_bytes());
        assert_eq!(packet.len(), 12 + payload.len());
        payload.push(0);
        command(READ_CMD, 7, false, &payload).unwrap_err();
    }
    #[test]
    fn write_acknowledgement_may_omit_the_written_length() {
        // WRITEMEM_ACK with an empty SCD, and with length_written = 4.
        let empty = [0x55, 0x33, 0x56, 0x43, 0, 0, 0x03, 0x08, 0, 0, 7, 0];
        let (command, scd) = ack(&empty, 7).unwrap();
        assert_eq!(command, WRITE_CMD + 1);
        write_acknowledged(scd, 4).unwrap();
        let mut counted = empty.to_vec();
        counted[8] = 4;
        counted.extend([0, 0, 4, 0]);
        write_acknowledged(ack(&counted, 7).unwrap().1, 4).unwrap();
        assert!(write_acknowledged(ack(&counted, 7).unwrap().1, 8).is_err());
        assert!(write_acknowledged(&[0, 0], 4).is_err());
    }
    #[test]
    fn manifest_counts_are_checked_before_narrowing() {
        assert_eq!(manifest_size(1).unwrap(), 64);
        assert_eq!(manifest_size(64).unwrap(), 4096);
        for count in [0, 65, (1u64 << 32) + 1, u64::MAX] {
            assert!(manifest_size(count).is_err(), "accepted count {count}");
        }
    }

    #[test]
    fn manifest_selects_the_device_xml_over_a_newer_buffer_xml() {
        let entry = |version: u32, schema: u32, address: u64, size: u64| {
            let mut entry = [0u8; 64];
            entry[..4].copy_from_slice(&version.to_le_bytes());
            entry[4..8].copy_from_slice(&schema.to_le_bytes());
            entry[8..16].copy_from_slice(&address.to_le_bytes());
            entry[16..24].copy_from_slice(&size.to_le_bytes());
            entry
        };
        // Schema 1.1 in the top half; file type in bits 0-2, format at 10.
        let schema = |file_type: u32, format: u32| 0x0101_0000 | (format << 10) | file_type;
        let manifest = [
            entry(0x0200_0000, schema(1, 0), 0x1000, 100),
            entry(0x0100_0002, schema(0, 1), 0x2000, 200),
            entry(0x0100_0001, schema(0, 0), 0x3000, 300),
            entry(0x0300_0000, schema(0, 33), 0x4000, 400),
        ]
        .concat();
        let found = device_xml_entries(&manifest).unwrap();
        assert_eq!(
            found
                .iter()
                .map(|(_, address, size)| (*address, *size))
                .collect::<Vec<_>>(),
            [(0x2000, 200), (0x3000, 300)]
        );
        // The whole entry, its SHA-1 included, names a cached copy.
        assert_eq!(found[0].0, entry(0x0100_0002, schema(0, 1), 0x2000, 200));
    }
    /// A camera's control endpoints, replying to each read from a script.
    #[derive(Default)]
    struct Script {
        sent: Vec<Vec<u8>>,
        replies: VecDeque<Result<Vec<u8>, TransferError>>,
        halts_cleared: usize,
        /// Whether a read with nothing to receive waits out its timeout.
        slow: bool,
    }
    impl Channel for Script {
        fn send(&mut self, packet: Vec<u8>, _timeout: Duration) -> Result<usize, TransferError> {
            let length = packet.len();
            self.sent.push(packet);
            Ok(length)
        }
        fn receive(&mut self, _len: usize, timeout: Duration) -> Result<Vec<u8>, TransferError> {
            // A read with nothing to receive times out.
            self.replies.pop_front().unwrap_or_else(|| {
                if self.slow {
                    std::thread::sleep(timeout);
                }
                Err(TransferError::Cancelled)
            })
        }
        fn clear_halt(&mut self) {
            self.halts_cleared += 1;
        }
    }
    fn reply(status: u16, command: u16, id: u16, scd: &[u8]) -> Vec<u8> {
        let mut packet = MAGIC.to_le_bytes().to_vec();
        packet.extend(status.to_le_bytes());
        packet.extend(command.to_le_bytes());
        packet.extend(u16::try_from(scd.len()).unwrap().to_le_bytes());
        packet.extend(id.to_le_bytes());
        packet.extend(scd);
        packet
    }
    fn control(
        replies: impl IntoIterator<Item = Result<Vec<u8>, TransferError>>,
    ) -> Control<Script> {
        let script = Script {
            replies: replies.into_iter().collect(),
            ..Script::default()
        };
        Control::new(script, Duration::from_secs(1))
    }
    /// The flags and request ID of each command sent.
    fn sent(control: &Control<Script>) -> Vec<(u16, u16)> {
        let field = |packet: &[u8], at| le16(packet, at).unwrap();
        let sent = control.channel.sent.iter();
        sent.map(|p| (field(p, 4), field(p, 10))).collect()
    }
    #[test]
    fn lost_or_corrupt_acknowledgements_resend_the_command() {
        let value = 2000u32.to_le_bytes();
        // No acknowledgement: resend with the same ID and CommandResend.
        let mut lost = control([
            Err(TransferError::Cancelled),
            Ok(reply(0, 0x0801, 1, &value)),
        ]);
        assert_eq!(lost.read_memory(0x1cc, 4).unwrap(), value);
        assert_eq!(sent(&lost), [(0x4000, 1), (0xc000, 1)]);
        // A corrupt acknowledgement, then a stall the endpoint must clear.
        let mut corrupt = reply(0, 0x0801, 1, &value);
        corrupt[0] = 0;
        let mut flaky = control([
            Ok(corrupt),
            Err(TransferError::Stall),
            Ok(reply(0, 0x0801, 1, &value)),
        ]);
        assert_eq!(flaky.read_memory(0x1cc, 4).unwrap(), value);
        assert_eq!(flaky.channel.sent.len(), 3);
        assert_eq!(flaky.channel.halts_cleared, 1);
        // A busy camera gets the command again under a new ID, as does one
        // that cannot take a resend.
        let mut busy = control([
            Ok(reply(STATUS_BUSY, 0x0801, 1, &[])),
            Ok(reply(0, 0x0801, 2, &value)),
        ]);
        assert_eq!(busy.read_memory(0x1cc, 4).unwrap(), value);
        assert_eq!(sent(&busy), [(0x4000, 1), (0x4000, 2)]);
        let mut plain = control([
            Err(TransferError::Cancelled),
            Ok(reply(STATUS_RESEND_NOT_SUPPORTED, 0x0801, 1, &[])),
            Ok(reply(0, 0x0801, 2, &value)),
        ]);
        assert_eq!(plain.read_memory(0x1cc, 4).unwrap(), value);
        assert_eq!(sent(&plain), [(0x4000, 1), (0xc000, 1), (0x4000, 2)]);
        // A camera that writes without reporting the written length.
        let mut write = control([Ok(reply(0, 0x0803, 1, &[]))]);
        write.write_memory(0x10_0004, &[1, 0, 0, 0]).unwrap();
    }
    #[test]
    fn memory_write_chunks_keep_addresses_and_short_final_payloads() {
        let mut control = control((1..=3).map(|id| Ok(reply(0, WRITE_CMD + 1, id, &[]))));
        control.max_command = 24;
        let data = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        control.write_memory(0x1000, &data).unwrap();
        assert_eq!(control.channel.sent.len(), 3);
        for (index, (packet, chunk)) in control.channel.sent.iter().zip(data.chunks(4)).enumerate()
        {
            assert_eq!(packet.len(), 20 + chunk.len());
            assert_eq!(
                le64(packet, 12).unwrap(),
                0x1000 + u64::try_from(index * 4).unwrap()
            );
            assert_eq!(&packet[20..], chunk);
        }
    }

    #[test]
    fn definite_failures_are_not_resent() {
        let mut rejected = control([Ok(reply(0x8003, 0x0801, 1, &[]))]);
        let error = rejected.read_memory(0x1cc, 4).unwrap_err();
        assert_eq!(error.downcast_ref::<UvcpStatus>().unwrap().0, 0x8003);
        assert_eq!(rejected.channel.sent.len(), 1);
        let mut gone = control([Err(TransferError::Disconnected)]);
        gone.read_memory(0x1cc, 4).unwrap_err();
        assert_eq!(gone.channel.sent.len(), 1);
        let mut silent = control([]);
        let error = silent.read_memory(0x1cc, 4).unwrap_err();
        assert!(format!("{error:#}").contains("timed out"));
        assert_eq!(silent.channel.sent.len(), ATTEMPTS as usize);
    }
    /// A channel that returns a valid reply only after the caller budget on
    /// one selected receive. It deliberately violates the requested timeout.
    struct BudgetedChannel {
        script: Script,
        deadline: Instant,
        expire_on: usize,
        received: usize,
        waits: Vec<Duration>,
        sent_at: Vec<Instant>,
    }
    impl Channel for BudgetedChannel {
        fn send(&mut self, packet: Vec<u8>, timeout: Duration) -> Result<usize, TransferError> {
            self.sent_at.push(Instant::now());
            self.script.send(packet, timeout)
        }
        fn receive(&mut self, length: usize, timeout: Duration) -> Result<Vec<u8>, TransferError> {
            self.received += 1;
            self.waits.push(timeout);
            if self.received == self.expire_on {
                std::thread::sleep(
                    self.deadline.saturating_duration_since(Instant::now())
                        + Duration::from_millis(1),
                );
            }
            self.script.receive(length, timeout)
        }
        fn clear_halt(&mut self) {
            self.script.clear_halt();
        }
    }
    fn bounded_script(
        replies: impl IntoIterator<Item = Result<Vec<u8>, TransferError>>,
        expire_on: usize,
    ) -> (Control<BudgetedChannel>, Instant) {
        let deadline = Instant::now() + Duration::from_secs(1);
        (
            Control::new(
                BudgetedChannel {
                    script: Script {
                        replies: replies.into_iter().collect(),
                        ..Default::default()
                    },
                    deadline,
                    expire_on,
                    received: 0,
                    waits: Vec::new(),
                    sent_at: Vec::new(),
                },
                Duration::from_secs(2),
            ),
            deadline,
        )
    }
    #[test]
    fn bounded_memory_reads_and_writes_keep_one_deadline_across_chunks() {
        for write in [false, true] {
            let command = if write { WRITE_CMD } else { READ_CMD };
            let value = [1, 2, 3, 4];
            let replies =
                (1..=3).map(|id| Ok(reply(0, command + 1, id, if write { &[] } else { &value })));
            let (mut control, deadline) = bounded_script(replies, 2);
            control.max_ack = 16; // four read bytes
            control.max_command = 24; // four write bytes
            let error = if write {
                control
                    .write_memory_until(0x1000, &[1; 12], deadline)
                    .unwrap_err()
            } else {
                control.read_memory_until(0x1000, 12, deadline).unwrap_err()
            };
            assert_eq!(crate::types::error_transport_code(&error), Some("timeout"));
            assert_eq!(control.channel.script.sent.len(), 2);
            assert_eq!(control.channel.script.replies.len(), 1);
            assert!(control.channel.sent_at.iter().all(|sent| *sent < deadline));
            assert!(
                control
                    .channel
                    .waits
                    .iter()
                    .all(|wait| *wait <= Duration::from_secs(1))
            );
            let addresses = control
                .channel
                .script
                .sent
                .iter()
                .map(|packet| le64(packet, 12).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(addresses, [0x1000, 0x1004]);
            // There is no persistent deadline scope. A later ordinary call
            // can consume the next reply under its normal fresh budget.
            if write {
                control.write_memory(0x2000, &value).unwrap();
            } else {
                assert_eq!(control.read_memory(0x2000, 4).unwrap(), value);
            }
            assert_eq!(control.channel.script.sent.len(), 3);
        }
    }
    #[test]
    fn a_pending_ack_cannot_extend_the_bounded_caller_budget() {
        let replies = [
            Ok(reply(0, PENDING_ACK, 1, &[0, 0, 0x60, 0xea])),
            Ok(reply(0, READ_CMD + 1, 1, &[1, 2, 3, 4])),
        ];
        let (mut control, deadline) = bounded_script(replies, 2);
        let error = control.read_memory_until(0x1000, 4, deadline).unwrap_err();
        assert_eq!(crate::types::error_transport_code(&error), Some("timeout"));
        assert_eq!(control.channel.script.sent.len(), 1);
        assert_eq!(control.channel.received, 2);
        assert!(
            control
                .channel
                .waits
                .iter()
                .all(|wait| *wait <= Duration::from_secs(1))
        );
    }
    #[test]
    fn expired_bounded_memory_sends_nothing_and_stall_does_not_enter_unbounded_recovery() {
        let mut expired = control([]);
        let deadline = Instant::now();
        let read = expired.read_memory_until(0x1000, 4, deadline).unwrap_err();
        let write = expired
            .write_memory_until(0x1000, &[1; 4], deadline)
            .unwrap_err();
        assert_eq!(crate::types::error_transport_code(&read), Some("timeout"));
        assert_eq!(crate::types::error_transport_code(&write), Some("timeout"));
        assert_eq!(expired.channel.sent, [] as [Vec<u8>; 0]);
        let mut halted = control([
            Err(TransferError::Stall),
            Ok(reply(0, READ_CMD + 1, 2, &[1, 2, 3, 4])),
        ]);
        let error = halted
            .read_memory_until(0x1000, 4, Instant::now() + Duration::from_secs(1))
            .unwrap_err();
        assert!(
            error
                .chain()
                .any(|cause| cause.downcast_ref() == Some(&TransferError::Stall))
        );
        assert_eq!(halted.channel.sent.len(), 1);
        assert_eq!(halted.channel.halts_cleared, 0);
        assert_eq!(halted.read_memory(0x1000, 4).unwrap(), [1, 2, 3, 4]);
    }

    #[test]
    fn resends_stay_within_the_command_timeout() {
        let script = Script {
            slow: true,
            ..Script::default()
        };
        // A camera whose maximum response time is most of the timeout gets
        // one resend in the time that remains.
        let mut slow = Control::new(script, Duration::from_millis(300));
        slow.response = Duration::from_millis(200);
        slow.read_memory(0x1cc, 4).unwrap_err();
        assert!(slow.channel.sent.len() <= 2);
        assert_eq!(sent(&slow)[0], (0x4000, 1));
    }
    #[test]
    fn usb_leader_and_trailer_validation() {
        let mut leader = vec![0; 52];
        leader[..4].copy_from_slice(&LEADER_MAGIC.to_le_bytes());
        leader[6..8].copy_from_slice(&52u16.to_le_bytes());
        leader[8..16].copy_from_slice(&19u64.to_le_bytes());
        leader[18..20].copy_from_slice(&1u16.to_le_bytes());
        leader[28..32].copy_from_slice(&crate::types::MONO8.to_le_bytes());
        leader[32..36].copy_from_slice(&4u32.to_le_bytes());
        leader[36..40].copy_from_slice(&2u32.to_le_bytes());
        let mut frame = parse_leader(&leader).unwrap().frame;
        frame.data = vec![0; 8].into();
        let mut trailer = vec![0; 28];
        trailer[..4].copy_from_slice(&TRAILER_MAGIC.to_le_bytes());
        trailer[6..8].copy_from_slice(&28u16.to_le_bytes());
        trailer[8..16].copy_from_slice(&19u64.to_le_bytes());
        trailer[20..28].copy_from_slice(&8u64.to_le_bytes());
        validate_trailer(&trailer, frame.id, 8, frame.data.len()).unwrap();
        validate_trailer(&trailer, frame.id, 7, frame.data.len()).unwrap_err();
        validate_trailer(&trailer, frame.id, 8, 7).unwrap_err();
        trailer[16] = 1;
        validate_trailer(&trailer, frame.id, 8, frame.data.len()).unwrap_err();
        leader[18] = 2;
        parse_leader(&leader).unwrap_err();
    }
    const MPS: usize = 1024;
    /// Stands in for the stream endpoint and the camera behind it. The
    /// camera's transfers queue as packets, and each submitted transfer in
    /// turn takes packets until it is full or a short packet ends it.
    #[derive(Default)]
    struct Bus {
        allocated: usize,
        /// Where each buffer submitted starts.
        buffers: std::collections::HashSet<usize>,
        submitted: VecDeque<(Buffer, Option<TransferError>)>,
        packets: VecDeque<Vec<u8>>,
        /// Submits to refuse, as Linux does when usbfs memory runs out.
        refuse: usize,
        /// Whether the next transfer to complete loses its data.
        fault: bool,
    }
    impl Bus {
        /// Queue one camera transfer, which ends with a short packet unless
        /// its length is a multiple of the packet size.
        fn send(&mut self, bytes: &[u8]) {
            self.packets.extend(bytes.chunks(MPS).map(<[u8]>::to_vec));
            if bytes.is_empty() {
                self.packets.push_back(Vec::new());
            }
        }
        /// Queue a whole frame as a camera sends it after SIRM setup.
        fn frame(&mut self, id: u64, image: &[u8], block: usize) {
            self.send(&leader(id, u32::try_from(image.len()).unwrap(), 1));
            for chunk in image.chunks(block) {
                self.send(chunk);
            }
            self.send(&trailer(id, image.len()));
        }
        fn stall(&mut self) {
            for (_, status) in &mut self.submitted {
                *status = Some(TransferError::Stall);
            }
        }
    }
    impl Transfers for Bus {
        fn allocate(&mut self, len: usize) -> Buffer {
            self.allocated += 1;
            Buffer::new(len)
        }
        fn submit(&mut self, buffer: Buffer) {
            assert!(buffer.requested_len().is_multiple_of(MPS));
            assert!(buffer.is_empty());
            self.buffers.insert(buffer.as_ptr() as usize);
            let refused = self.refuse > 0;
            self.refuse = self.refuse.saturating_sub(1);
            let status = refused.then_some(TransferError::Unknown(ENOMEM));
            self.submitted.push_back((buffer, status));
        }
        fn wait(&mut self, _timeout: Duration) -> Option<Completion> {
            let (buffer, status) = self.submitted.front_mut()?;
            let status = if let Some(error) = status {
                Err(*error)
            } else {
                let mut room = buffer.requested_len();
                let mut taken = 0;
                loop {
                    let packet = self.packets.get(taken)?;
                    assert!(packet.len() <= room, "babble");
                    room -= packet.len();
                    taken += 1;
                    if packet.len() < MPS || room == 0 {
                        break;
                    }
                }
                let packets: Vec<_> = self.packets.drain(..taken).collect();
                if std::mem::take(&mut self.fault) {
                    Err(TransferError::Fault)
                } else {
                    for packet in &packets {
                        buffer.extend_from_slice(packet);
                    }
                    Ok(())
                }
            };
            let (buffer, _) = self.submitted.pop_front()?;
            Some(Completion {
                actual_len: buffer.len(),
                buffer,
                status,
            })
        }
        fn cancel_all(&mut self) {
            for (_, status) in &mut self.submitted {
                status.get_or_insert(TransferError::Cancelled);
            }
        }
    }
    fn leader(id: u64, width: u32, height: u32) -> Vec<u8> {
        let mut leader = vec![0; 52];
        leader[..4].copy_from_slice(&LEADER_MAGIC.to_le_bytes());
        leader[6..8].copy_from_slice(&52u16.to_le_bytes());
        leader[8..16].copy_from_slice(&id.to_le_bytes());
        leader[18..20].copy_from_slice(&1u16.to_le_bytes());
        leader[28..32].copy_from_slice(&crate::types::MONO8.to_le_bytes());
        leader[32..36].copy_from_slice(&width.to_le_bytes());
        leader[36..40].copy_from_slice(&height.to_le_bytes());
        leader
    }
    /// An image trailer with `SizeY`, as U3V 1.0 cameras send it.
    fn trailer(id: u64, size: usize) -> Vec<u8> {
        let mut trailer = vec![0; 32];
        trailer[..4].copy_from_slice(&TRAILER_MAGIC.to_le_bytes());
        trailer[6..8].copy_from_slice(&32u16.to_le_bytes());
        trailer[8..16].copy_from_slice(&id.to_le_bytes());
        trailer[20..28].copy_from_slice(&(size as u64).to_le_bytes());
        trailer[28..32].copy_from_slice(&1u32.to_le_bytes());
        trailer
    }
    /// A stream set up as `start` sets one up for `expected` bytes, through
    /// four transfers of `block` bytes: fewer than one frame needs.
    fn streaming(expected: usize, block: usize) -> (Stream, Bus) {
        let mut stream = Stream::new(expected, MPS, MPS, None);
        let mut bus = Bus::default();
        stream.queue(&mut bus, 4, block);
        (stream, bus)
    }
    fn next(stream: &mut Stream, bus: &mut Bus) -> Result<Frame> {
        let frame = stream.next(bus, Instant::now() + Duration::from_secs(1));
        assert!(bus.submitted.len() <= 4 && bus.allocated == 4);
        frame
    }
    fn image(id: u64, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u64 * 7 + id).to_le_bytes()[0])
            .collect()
    }
    #[test]
    fn stream_assembles_frames_larger_than_its_transfer_window() {
        let block = 4 * MPS;
        // An unaligned last transfer ends short; an aligned one shares its
        // transfer with the trailer; an image of whole blocks has none.
        for expected in [10 * MPS + 300, 10 * MPS, 12 * MPS] {
            let (mut stream, mut bus) = streaming(expected, block);
            for id in 1..=3 {
                bus.frame(id, &image(id, expected), block);
                let frame = next(&mut stream, &mut bus).unwrap();
                assert_eq!((frame.id, frame.data.to_vec()), (id, image(id, expected)));
            }
        }
        // Some cameras end the last payload transfer with a zero-length
        // packet before the trailer.
        let (mut stream, mut bus) = streaming(12 * MPS, block);
        for id in 1..=3 {
            bus.send(&leader(id, 12 * u32::try_from(MPS).unwrap(), 1));
            for chunk in image(id, 12 * MPS).chunks(block) {
                bus.send(chunk);
            }
            bus.send(&[]);
            bus.send(&trailer(id, 12 * MPS));
            assert_eq!(next(&mut stream, &mut bus).unwrap().id, id);
        }
    }
    fn sized_trailer(id: u64, size: usize, height: u32) -> Vec<u8> {
        let mut bytes = trailer(id, size);
        bytes[28..32].copy_from_slice(&height.to_le_bytes());
        bytes
    }
    fn padded_payload(pixels: &[u8], row: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        for pixels in pixels.chunks_exact(row) {
            bytes.extend_from_slice(pixels);
            bytes.extend_from_slice(&[0xa5, 0x5a]);
        }
        bytes
    }
    #[test]
    fn variable_height_short_packets_and_zlps_preserve_whole_and_split_frames() {
        for (width, maximum_height, height, zlp) in [(3, 600, 400, false), (4, 600, 512, true)] {
            let maximum = width as usize * maximum_height as usize;
            let pixels = image(1, width as usize * height as usize);
            for whole in [false, true] {
                let request = if whole {
                    aligned(maximum, MPS).unwrap()
                } else {
                    MPS
                };
                let (mut stream, mut bus) = requesting(maximum, request, whole);
                bus.send(&leader(1, width, maximum_height));
                bus.send(&pixels);
                if zlp {
                    bus.send(&[]);
                }
                bus.send(&sized_trailer(1, pixels.len(), height));
                let complete = image(2, maximum);
                bus.send(&leader(2, width, maximum_height));
                bus.send(&complete);
                bus.send(&sized_trailer(2, maximum, maximum_height));
                let frame = next_requested(&mut stream, &mut bus).unwrap();
                assert_eq!((frame.id, frame.width, frame.height), (1, width, height));
                assert_eq!(frame.data, pixels);
                assert_eq!(bus.buffers.contains(&(frame.data.as_ptr() as usize)), whole);
                let frame = next_requested(&mut stream, &mut bus).unwrap();
                assert_eq!((frame.id, frame.height), (2, maximum_height));
                assert_eq!(frame.data, complete);
            }
        }
    }
    #[test]
    fn variable_height_plain_padding_and_odd_planes_use_confirmed_geometry() {
        let cases = [
            (
                0x0110_0007_u32,
                2,
                4,
                3,
                2_u16,
                24,
                (0_u8..12).collect::<Vec<_>>(),
            ),
            (0x020c_0112, 3, 5, 3, 0, 27, (0_u8..17).collect::<Vec<_>>()),
        ];
        for (format, width, maximum_height, height, padding, maximum, pixels) in cases {
            let payload = if padding == 0 {
                pixels.clone()
            } else {
                padded_payload(&pixels, 4)
            };
            for whole in [false, true] {
                let (mut stream, mut bus) = requesting(maximum, MPS, whole);
                let mut metadata = leader(1, width, maximum_height);
                metadata[28..32].copy_from_slice(&format.to_le_bytes());
                metadata[48..50].copy_from_slice(&padding.to_le_bytes());
                bus.send(&metadata);
                bus.send(&payload);
                bus.send(&sized_trailer(1, payload.len(), height));
                let frame = next_requested(&mut stream, &mut bus).unwrap();
                assert_eq!(
                    (frame.width, frame.height, frame.pixel_format),
                    (width, height, format)
                );
                assert_eq!(frame.data, pixels);
                assert!(frame.chunks.is_none());
                bus.frame(2, &image(2, maximum), MPS);
                assert_eq!(next_requested(&mut stream, &mut bus).unwrap().id, 2);
            }
        }
    }
    #[test]
    fn variable_height_padded_chunks_preserve_original_rows_and_footer_offsets() {
        for whole in [false, true] {
            let (mut stream, mut bus) = requesting(40, MPS, whole);
            for (id, height) in [(1, 3), (2, 4)] {
                let pixels = image(id, height as usize * 4);
                let mut payload = padded_payload(&pixels, 4);
                let boundary = payload.len();
                payload.extend_from_slice(&0x0123_4567_89ab_cdefu64.to_le_bytes());
                payload.extend_from_slice(&0xffff_1234u32.to_le_bytes());
                payload.extend_from_slice(&8u32.to_le_bytes());
                let mut metadata = leader(id, 2, 4);
                metadata[18..20].copy_from_slice(&0x4001u16.to_le_bytes());
                metadata[28..32].copy_from_slice(&0x0110_0007u32.to_le_bytes());
                metadata[48..50].copy_from_slice(&2u16.to_le_bytes());
                let mut tail = sized_trailer(id, payload.len(), height);
                tail.extend_from_slice(&0xaabb_ccddu32.to_le_bytes());
                tail[6..8].copy_from_slice(&36u16.to_le_bytes());
                bus.send(&metadata);
                bus.send(&payload);
                bus.send(&tail);
                let frame = next_requested(&mut stream, &mut bus).unwrap();
                assert_eq!((frame.id, frame.width, frame.height), (id, 2, height));
                assert_eq!(frame.data, pixels);
                let chunks = frame.chunks.unwrap();
                assert_eq!(&*chunks.payload, &payload);
                assert_eq!(
                    (chunks.image_bytes, chunks.chunk_offset),
                    (boundary, Some(boundary))
                );
                assert_eq!(
                    (chunks.entries[0].offset, chunks.entries[0].length),
                    (boundary, 8)
                );
                assert_eq!(chunks.image_layout.unwrap().height, height);
                assert_eq!(
                    bus.buffers.contains(&(chunks.payload.as_ptr() as usize)),
                    whole
                );
            }
        }
    }
    #[test]
    fn variable_height_final_pixels_that_look_like_headers_remain_pixels() {
        for whole in [false, true] {
            for pixels in [leader(9, 4, 2), trailer(9, 8)] {
                let (mut stream, mut bus) = requesting(128, MPS, whole);
                bus.send(&leader(1, 1, 128));
                bus.send(&pixels);
                bus.send(&sized_trailer(
                    1,
                    pixels.len(),
                    u32::try_from(pixels.len()).unwrap(),
                ));
                let frame = next_requested(&mut stream, &mut bus).unwrap();
                assert_eq!(frame.height as usize, pixels.len());
                assert_eq!(frame.data, pixels);
            }
        }
    }
    #[test]
    fn variable_height_incoherent_trailers_fail_and_keep_the_next_valid_frame() {
        for whole in [false, true] {
            for mode in 0..8 {
                let (mut stream, mut bus) =
                    requesting(1800, if whole { 2 * MPS } else { MPS }, whole);
                let mut tail = sized_trailer(1, 1200, 400);
                match mode {
                    0 => tail[20..28].copy_from_slice(&1201u64.to_le_bytes()),
                    1 => tail[28..32].copy_from_slice(&0u32.to_le_bytes()),
                    2 => tail[28..32].copy_from_slice(&601u32.to_le_bytes()),
                    3 => tail[28..32].copy_from_slice(&399u32.to_le_bytes()),
                    4 => {
                        tail.truncate(28);
                        tail[6..8].copy_from_slice(&28u16.to_le_bytes());
                    }
                    5 => tail[16] = 1,
                    6 => tail[6..8].copy_from_slice(&64u16.to_le_bytes()),
                    7 => tail[8..16].copy_from_slice(&9u64.to_le_bytes()),
                    _ => unreachable!(),
                }
                bus.send(&leader(1, 3, 600));
                bus.send(&image(1, 1200));
                bus.send(&tail);
                let complete = image(2, 1800);
                bus.send(&leader(2, 3, 600));
                bus.send(&complete);
                bus.send(&sized_trailer(2, 1800, 600));
                next_requested(&mut stream, &mut bus).unwrap_err();
                let frame = next_requested(&mut stream, &mut bus).unwrap();
                assert_eq!((frame.id, frame.height), (2, 600));
                assert_eq!(frame.data, complete);
            }
            // Preserve common-only legacy trailers for a complete leader
            // image; the same trailer cannot authorize a reduced image above.
            let (mut stream, mut bus) = requesting(1800, if whole { 2 * MPS } else { MPS }, whole);
            let pixels = image(1, 1800);
            let mut tail = trailer(1, 1800);
            tail.truncate(28);
            tail[6..8].copy_from_slice(&28u16.to_le_bytes());
            bus.send(&leader(1, 3, 600));
            bus.send(&pixels);
            bus.send(&tail);
            let frame = next_requested(&mut stream, &mut bus).unwrap();
            assert_eq!(frame.height, 600);
            assert_eq!(frame.data, pixels);
        }
    }
    #[test]
    fn variable_height_without_a_proven_trailer_boundary_never_publishes() {
        for whole in [false, true] {
            let request = if whole { 3 * MPS } else { MPS };
            let (mut stream, mut bus) = requesting(2400, request, whole);
            bus.send(&leader(1, 4, 600));
            // No short packet or ZLP separates this reduced image and trailer.
            // In whole mode the host sees them in the same completion.
            bus.send(&image(1, 2048));
            bus.send(&sized_trailer(1, 2048, 512));
            let complete = image(2, 2400);
            bus.send(&leader(2, 4, 600));
            bus.send(&complete);
            bus.send(&sized_trailer(2, 2400, 600));
            next_requested(&mut stream, &mut bus).unwrap_err();
            let frame = next_requested(&mut stream, &mut bus).unwrap();
            assert_eq!((frame.id, frame.height), (2, 600));
            assert_eq!(frame.data, complete);
        }
    }
    #[test]
    fn oversized_trailer_completions_retire_the_frame_without_losing_the_next_leader() {
        for whole in [false, true] {
            let (mut stream, mut bus) = requesting(128, MPS, whole);
            stream.trailer = 64;
            // Oversized stale bytes without a frame remain ignored.
            bus.send(&[0xbb; 80]);
            bus.send(&leader(1, 1, 128));
            bus.send(&image(1, 64));
            let mut oversized = sized_trailer(1, 64, 64);
            oversized.resize(80, 0xef);
            bus.send(&oversized);
            // A valid extended leader may exceed the trailer bound.
            let mut metadata = leader(2, 128, 1);
            metadata.resize(80, 0);
            metadata[6..8].copy_from_slice(&80u16.to_le_bytes());
            let complete = image(2, 128);
            bus.send(&metadata);
            bus.send(&complete);
            bus.send(&trailer(2, 128));
            let error = next_requested(&mut stream, &mut bus).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("trailer exceeds its negotiated bound"),
                "{error:#}"
            );
            let frame = next_requested(&mut stream, &mut bus).unwrap();
            assert_eq!(frame.id, 2);
            assert_eq!(frame.data, complete);

            // At an open frame's boundary, that same extended leader seeds
            // recovery before the trailer-size check can reject its length.
            bus.send(&leader(3, 1, 128));
            bus.send(&image(3, 64));
            metadata[8..16].copy_from_slice(&4u64.to_le_bytes());
            bus.send(&metadata);
            let complete = image(4, 128);
            bus.send(&complete);
            bus.send(&trailer(4, 128));
            let error = next_requested(&mut stream, &mut bus).unwrap_err();
            assert!(
                error.to_string().contains("ended without a trailer"),
                "{error:#}"
            );
            let frame = next_requested(&mut stream, &mut bus).unwrap();
            assert_eq!(frame.id, 4);
            assert_eq!(frame.data, complete);
        }
    }
    #[test]
    fn partial_size_y_trailers_fail_then_the_next_full_height_frame_recovers() {
        for whole in [false, true] {
            for (declared, received) in [
                (29_u16, 29_usize),
                (29, 32),
                (30, 30),
                (30, 32),
                (31, 31),
                (31, 32),
                (32, 29),
                (32, 30),
                (32, 31),
            ] {
                let (mut stream, mut bus) = requesting(128, MPS, whole);
                let mut partial = trailer(1, 128);
                partial.truncate(received);
                partial[6..8].copy_from_slice(&declared.to_le_bytes());
                bus.send(&leader(1, 128, 1));
                bus.send(&image(1, 128));
                bus.send(&partial);
                let complete = image(2, 128);
                bus.frame(2, &complete, MPS);
                let error = next_requested(&mut stream, &mut bus).unwrap_err();
                let diagnostic = if declared < 32 {
                    "truncated USB image trailer"
                } else {
                    "invalid UVSP trailer size"
                };
                assert!(
                    error.to_string().contains(diagnostic),
                    "declared {declared}, received {received}: {error:#}"
                );
                let frame = next_requested(&mut stream, &mut bus).unwrap();
                assert_eq!((frame.id, frame.height), (2, 1));
                assert_eq!(frame.data, complete);
            }
        }
    }
    #[test]
    fn oversized_coalesced_trailer_suffixes_retire_before_copying_the_metadata() {
        for whole in [false, true] {
            let (mut stream, mut bus) = requesting(MPS, 2 * MPS, whole);
            stream.trailer = 64;
            bus.send(&leader(1, u32::try_from(MPS).unwrap(), 1));
            // The full payload packet and80-byte trailer fill one host
            // completion, so no preceding short completion proves a boundary.
            bus.send(&image(1, MPS));
            let mut oversized = trailer(1, MPS);
            oversized.resize(80, 0xef);
            bus.send(&oversized);
            let complete = image(2, MPS);
            bus.frame(2, &complete, 2 * MPS);
            let error = next_requested(&mut stream, &mut bus).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("trailer exceeds its negotiated bound"),
                "{error:#}"
            );
            let frame = next_requested(&mut stream, &mut bus).unwrap();
            assert_eq!(frame.id, 2);
            assert_eq!(frame.data, complete);
        }
    }
    #[test]
    fn lost_payload_and_trailer_do_not_turn_the_next_leader_into_a_false_frame() {
        let (mut stream, mut bus) = streaming(8 * MPS, 4 * MPS);
        bus.send(&leader(1, 8 * u32::try_from(MPS).unwrap(), 1));
        bus.send(&image(1, 4 * MPS));
        // Both the rest of frame1 and its trailer are lost. Leader2 arrives
        // inside the still-undecided payload; do not guess at its magic.
        bus.frame(2, &image(2, 8 * MPS), 4 * MPS);
        bus.frame(3, &image(3, 8 * MPS), 4 * MPS);
        next(&mut stream, &mut bus).unwrap_err();
        let frame = next(&mut stream, &mut bus).unwrap();
        assert_eq!(frame.id, 3);
        assert_eq!(frame.data, image(3, 8 * MPS));
    }
    #[test]
    fn variable_height_maximum_leader_geometry_stays_within_the_payload_ceiling() {
        let (mut stream, mut bus) = requesting(1800, MPS, false);
        bus.send(&leader(1, 3, 601));
        bus.send(&image(1, 1200));
        bus.send(&sized_trailer(1, 1200, 400));
        let complete = image(2, 1800);
        bus.send(&leader(2, 3, 600));
        bus.send(&complete);
        bus.send(&sized_trailer(2, 1800, 600));
        let error = next_requested(&mut stream, &mut bus).unwrap_err();
        assert!(error.to_string().contains("payload ceiling"), "{error:#}");
        let frame = next_requested(&mut stream, &mut bus).unwrap();
        assert_eq!(frame.id, 2);
        assert_eq!(frame.data, complete);
    }
    #[test]
    fn image_extended_chunks_retain_measurements_and_original_payload() {
        for whole in [false, true] {
            let pixels = image(1, 8);
            let mut payload = pixels.clone();
            payload.extend(0x0123_4567_89ab_cdefu64.to_le_bytes());
            payload.extend(0xffff_1234u32.to_le_bytes());
            payload.extend(8u32.to_le_bytes());
            let mut metadata = leader(1, 4, 2);
            metadata[18..20].copy_from_slice(&0x4001u16.to_le_bytes());
            let mut tail = trailer(1, payload.len());
            tail.extend(0xaabb_ccddu32.to_le_bytes());
            tail[6..8].copy_from_slice(&36u16.to_le_bytes());
            tail[28..32].copy_from_slice(&2u32.to_le_bytes());
            let (mut stream, mut bus) = requesting(payload.len(), MPS, whole);
            bus.send(&metadata);
            bus.send(&payload);
            bus.send(&tail);
            let frame = stream
                .next(&mut bus, Instant::now() + Duration::from_secs(1))
                .unwrap();
            assert_eq!(frame.data, pixels);
            let chunks = frame.chunks.unwrap();
            assert_eq!(
                (chunks.layout_id, chunks.byte_order),
                (Some(0xaabb_ccdd), Some(ByteOrder::Little))
            );
            assert_eq!(&*chunks.payload, &payload);
            assert_eq!(
                chunks.entries,
                vec![super::super::chunks::Chunk {
                    id: 0xffff_1234,
                    offset: 8,
                    length: 8
                }]
            );
            assert_eq!(chunks.image_layout.unwrap().row_stride, 4);
            if whole {
                assert!(bus.buffers.contains(&(chunks.payload.as_ptr() as usize)));
            }

            // One malformed footer loses one frame, then ordinary capture
            // resumes; it must never deliver an image while losing chunks.
            let end = payload.len();
            payload[end - 4..].copy_from_slice(&u32::MAX.to_le_bytes());
            bus.send(&metadata);
            bus.send(&payload);
            bus.send(&tail);
            let error = stream
                .next(&mut bus, Instant::now() + Duration::from_secs(1))
                .unwrap_err();
            assert!(error.to_string().contains("chunk length exceeds payload"));
            bus.frame(2, &image(2, payload.len()), MPS);
            let frame = stream
                .next(&mut bus, Instant::now() + Duration::from_secs(1))
                .unwrap();
            assert_eq!(frame.id, 2);
            assert!(frame.chunks.is_none());
        }
    }
    #[test]
    fn padded_rows_normalize_whole_and_split_transfers_without_changing_samples() {
        // Basler's Line Pitch feature appends bytes to each image line. The
        // UVSP leader reports their count at48; the word at50 is reserved.
        for (format, width, pair) in [
            (crate::types::MONO8, 3, vec![1, 2, 3, 4, 5, 6]),
            (
                0x0110_0007,
                2,
                vec![0x34, 0x12, 0x78, 0x56, 0xbc, 0x9a, 0xf0, 0xde],
            ),
            (0x010a_0046, 3, vec![1, 8, 48, 0, 4, 20, 96, 0]),
            (0x0210_00a5, 3, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]),
            (0x0210_00a6, 3, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]),
            (
                0x0220_00a7,
                3,
                (1_u16..=12).flat_map(u16::to_le_bytes).collect(),
            ),
            (
                0x0220_00a9,
                3,
                (1_u16..=12).flat_map(u16::to_le_bytes).collect(),
            ),
            (
                0x0220_00ab,
                3,
                (1_u16..=12).flat_map(u16::to_le_bytes).collect(),
            ),
            (
                0x0220_00ad,
                3,
                (1_u16..=12).flat_map(u16::to_le_bytes).collect(),
            ),
        ] {
            let pixels = pair.repeat(300);
            let row_bytes = crate::frame::row_bytes(width, format);
            let mut payload = Vec::new();
            for row in pixels.chunks_exact(row_bytes) {
                payload.extend_from_slice(row);
                payload.extend_from_slice(&[0xa5, 0x5a, 0xef]);
            }
            for whole in [false, true] {
                let mut metadata = leader(1, width, 600);
                metadata[28..32].copy_from_slice(&format.to_le_bytes());
                metadata[48..50].copy_from_slice(&3u16.to_le_bytes());
                metadata[50..52].copy_from_slice(&0xbeefu16.to_le_bytes());
                let request = if whole {
                    payload.len().div_ceil(MPS) * MPS
                } else {
                    MPS
                };
                let (mut stream, mut bus) = requesting(payload.len(), request, whole);
                let mut tail = trailer(1, payload.len());
                tail[28..32].copy_from_slice(&600u32.to_le_bytes());
                bus.send(&metadata);
                bus.send(&payload);
                bus.send(&tail);
                let frame = next_requested(&mut stream, &mut bus).unwrap();
                assert_eq!(
                    (frame.width, frame.height, frame.pixel_format),
                    (width, 600, format)
                );
                assert_eq!(frame.data, pixels);
                assert!(frame.chunks.is_none());
                if format == 0x010a_0046 {
                    assert_eq!(
                        crate::frame::unpacked(&frame).unwrap().data,
                        [1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0].repeat(300)
                    );
                }
            }
        }
    }
    #[test]
    fn padded_chunk_image_keeps_wire_rows_footer_offsets_and_little_endian_words() {
        let pixels = [0x34, 0x12, 0x78, 0x56, 0xbc, 0x9a, 0xf0, 0xde];
        let mut payload = vec![
            0x34, 0x12, 0x78, 0x56, 0xa5, 0x5a, 0xbc, 0x9a, 0xf0, 0xde, 0x11, 0x22,
        ];
        payload.extend_from_slice(&0x0123_4567_89ab_cdefu64.to_le_bytes());
        payload.extend_from_slice(&0xffff_1234u32.to_le_bytes());
        payload.extend_from_slice(&8u32.to_le_bytes());
        for whole in [false, true] {
            let mut metadata = leader(1, 2, 2);
            metadata[18..20].copy_from_slice(&0x4001u16.to_le_bytes());
            metadata[28..32].copy_from_slice(&0x0110_0007u32.to_le_bytes());
            metadata[48..50].copy_from_slice(&2u16.to_le_bytes());
            metadata[50..52].copy_from_slice(&0xbeefu16.to_le_bytes());
            let mut tail = trailer(1, payload.len());
            tail.extend_from_slice(&0xaabb_ccddu32.to_le_bytes());
            tail[6..8].copy_from_slice(&36u16.to_le_bytes());
            tail[28..32].copy_from_slice(&2u32.to_le_bytes());
            let (mut stream, mut bus) = requesting(payload.len(), MPS, whole);
            bus.send(&metadata);
            bus.send(&payload);
            bus.send(&tail);
            let mut frame = next_requested(&mut stream, &mut bus).unwrap();
            assert_eq!(frame.data, pixels);
            let chunks = frame.chunks.as_ref().unwrap();
            assert_eq!(&*chunks.payload, &payload);
            assert_eq!((chunks.image_bytes, chunks.chunk_offset), (12, Some(12)));
            assert_eq!(
                (chunks.entries[0].offset, chunks.entries[0].length),
                (12, 8)
            );
            let layout = chunks.image_layout.unwrap();
            assert_eq!(
                (
                    layout.offset,
                    layout.row_bytes,
                    layout.row_stride,
                    layout.padding_y
                ),
                (0, 4, 6, 0)
            );
            assert_eq!(layout.byte_order, Some(ByteOrder::Little));
            if whole {
                assert!(bus.buffers.contains(&(chunks.payload.as_ptr() as usize)));
            }
            frame.data.make_mut()[0] ^= 0xff;
            assert_eq!(&*frame.chunks.as_ref().unwrap().payload, &payload);
        }
    }
    #[test]
    fn odd_semiplanar_chunk_images_use_the_complete_plane_size() {
        // YCbCr420_8_YY_CbCr_Semiplanar: nine luma bytes and two four-byte
        // chroma rows, rather than three rows of ceil(3*12/8) bytes.
        let pixels: Vec<u8> = (0..17).collect();
        let mut payload = pixels.clone();
        payload.extend_from_slice(&0x0123_4567_89ab_cdefu64.to_le_bytes());
        payload.extend_from_slice(&0xffff_1234u32.to_le_bytes());
        payload.extend_from_slice(&8u32.to_le_bytes());
        for whole in [false, true] {
            let mut metadata = leader(1, 3, 3);
            metadata[18..20].copy_from_slice(&0x4001u16.to_le_bytes());
            metadata[28..32].copy_from_slice(&0x020c_0112u32.to_le_bytes());
            let mut tail = trailer(1, payload.len());
            tail.extend_from_slice(&7u32.to_le_bytes());
            tail[6..8].copy_from_slice(&36u16.to_le_bytes());
            tail[28..32].copy_from_slice(&3u32.to_le_bytes());
            let (mut stream, mut bus) = requesting(payload.len(), MPS, whole);
            bus.send(&metadata);
            bus.send(&payload);
            bus.send(&tail);
            let frame = next_requested(&mut stream, &mut bus).unwrap();
            assert_eq!(frame.data, pixels);
            let chunks = frame.chunks.unwrap();
            assert_eq!(chunks.image_bytes, 17);
            assert_eq!(chunks.chunk_offset, Some(17));
            assert_eq!(chunks.entries[0].offset, 17);
            assert_eq!(&*chunks.payload, &payload);
        }
    }
    #[test]
    fn invalid_padded_geometry_and_ambiguous_planes_fail_then_the_stream_recovers() {
        for (format, width, height, padding, diagnostic) in [
            (crate::types::MONO8, 3, 2, 2u16, "size differs"),
            (
                crate::types::MONO8,
                u32::MAX,
                u32::MAX,
                u16::MAX,
                "overflow",
            ),
            (0x0218_0021, 1, 4, 1u16, "planar"),
        ] {
            let mut metadata = leader(1, width, height);
            metadata[28..32].copy_from_slice(&format.to_le_bytes());
            metadata[48..50].copy_from_slice(&padding.to_le_bytes());
            let (mut stream, mut bus) = requesting(16, MPS, false);
            bus.send(&metadata);
            bus.send(&image(1, 16));
            bus.send(&trailer(1, 16));
            let error = next(&mut stream, &mut bus).unwrap_err();
            assert!(error.to_string().contains(diagnostic), "{error:#}");
            if diagnostic == "planar" {
                assert_eq!(crate::types::classify(&error).kind, ErrorKind::Unsupported);
            }
            bus.frame(2, &image(2, 16), MPS);
            let frame = next(&mut stream, &mut bus).unwrap();
            assert_eq!(frame.id, 2);
            assert_eq!(frame.data, image(2, 16));
        }
    }
    #[test]
    fn frames_already_received_are_taken_in_and_returned_in_turn() {
        let block = 4 * MPS;
        let expected = 10 * MPS;
        let (mut stream, mut bus) = streaming(expected, block);
        bus.frame(1, &image(1, expected), block);
        bus.frame(2, &image(2, expected), block);
        // The third has its leader and part of its image so far.
        bus.send(&leader(3, u32::try_from(expected).unwrap(), 1));
        bus.send(&image(3, expected)[..block]);
        assert_eq!(stream.take_in(&mut bus), 3);
        assert!(bus.packets.is_empty());
        for id in 1..=2 {
            assert_eq!(next(&mut stream, &mut bus).unwrap().id, id);
        }
        for chunk in image(3, expected)[block..].chunks(block) {
            bus.send(chunk);
        }
        bus.send(&trailer(3, expected));
        let third = next(&mut stream, &mut bus).unwrap();
        assert_eq!((third.id, third.data.to_vec()), (3, image(3, expected)));
        assert_eq!(stream.take_in(&mut bus), 0);
    }
    #[test]
    fn image_ending_like_a_header_is_image_data() {
        let block = 4 * MPS;
        // A short last transfer, and a tiny image in one transfer.
        for expected in [8 * MPS + 64, 64] {
            let (mut stream, mut bus) = streaming(expected, block);
            for (id, header) in [(1, leader(9, 4, 2)), (2, trailer(9, 8))] {
                let mut pixels = image(id, expected);
                pixels[expected - 64..][..header.len()].copy_from_slice(&header);
                bus.frame(id, &pixels, block);
                let frame = next(&mut stream, &mut bus).unwrap();
                assert_eq!((frame.id, frame.data.to_vec()), (id, pixels));
            }
        }
    }
    #[test]
    fn stream_drops_one_frame_and_resumes_after_lost_or_extra_data() {
        // Images of whole packets, which never realigned fixed transfer slots.
        let (block, expected) = (4 * MPS, 8 * MPS);
        let (mut stream, mut bus) = streaming(expected, block);
        // A camera left streaming by an earlier program still holds the end
        // of a frame.
        bus.send(&[9; 4 * MPS]);
        bus.send(&trailer(0, expected));
        bus.frame(1, &image(1, expected), block);
        assert_eq!(next(&mut stream, &mut bus).unwrap().id, 1);
        // Each bad frame is one error, and the next frame arrives whole.
        let lost = |stream: &mut Stream, bus: &mut Bus, id: u64| {
            bus.frame(id, &image(id, expected), block);
            next(stream, bus).unwrap_err();
            let frame = next(stream, bus).unwrap();
            assert_eq!((frame.id, frame.data.to_vec()), (id, image(id, expected)));
        };
        // A truncated frame whose trailer arrives early.
        bus.send(&leader(2, u32::try_from(expected).unwrap(), 1));
        bus.send(&image(2, block));
        bus.send(&trailer(2, block));
        lost(&mut stream, &mut bus, 3);
        // A truncated frame that keeps its transfer count.
        bus.send(&leader(4, u32::try_from(expected).unwrap(), 1));
        bus.send(&image(4, 1000));
        bus.send(&[]);
        bus.send(&trailer(4, 1000));
        lost(&mut stream, &mut bus, 5);
        // A frame without its trailer.
        bus.send(&leader(6, u32::try_from(expected).unwrap(), 1));
        bus.send(&image(6, expected));
        lost(&mut stream, &mut bus, 7);
        // A transfer error in the middle of a frame.
        bus.send(&leader(8, u32::try_from(expected).unwrap(), 1));
        bus.send(&image(8, block));
        let waiting = next(&mut stream, &mut bus).unwrap_err();
        assert!(format!("{waiting:#}").contains("timed out"));
        bus.fault = true;
        bus.send(&image(8, block));
        bus.send(&trailer(8, expected));
        lost(&mut stream, &mut bus, 9);
    }
    #[test]
    fn refused_submits_hold_buffers_back_without_losing_data() {
        let block = 4 * MPS;
        let mut stream = Stream::new(10 * MPS, MPS, MPS, None);
        let mut bus = Bus {
            refuse: 2,
            ..Bus::default()
        };
        stream.queue(&mut bus, 4, block);
        for id in 1..=3 {
            bus.refuse = 1;
            bus.frame(id, &image(id, 10 * MPS), block);
            let frame = next(&mut stream, &mut bus).unwrap();
            assert_eq!((frame.id, frame.data.to_vec()), (id, image(id, 10 * MPS)));
        }
        // The transfers already submitted still complete a frame.
        bus.refuse = usize::MAX;
        bus.frame(4, &image(4, 10 * MPS), block);
        assert_eq!(next(&mut stream, &mut bus).unwrap().id, 4);
        bus.frame(5, &image(5, 10 * MPS), block);
        let error = stream
            .next(&mut bus, Instant::now() + Duration::from_millis(50))
            .unwrap_err();
        assert!(format!("{error:#}").contains("usbfs_memory_mb"));
        assert!(!format!("{error:#}").contains("timed out"));
    }
    #[test]
    fn stalled_stream_resumes_after_its_transfers_are_drained() {
        let (block, expected) = (4 * MPS, 10 * MPS);
        let (mut stream, mut bus) = streaming(expected, block);
        bus.frame(1, &image(1, expected), block);
        assert_eq!(next(&mut stream, &mut bus).unwrap().id, 1);
        bus.send(&leader(2, u32::try_from(expected).unwrap(), 1));
        bus.stall();
        let error = next(&mut stream, &mut bus).unwrap_err();
        assert!(
            error
                .chain()
                .any(|cause| cause.downcast_ref() == Some(&TransferError::Stall))
        );
        stream.drain(&mut bus, Instant::now()).unwrap();
        assert_eq!((bus.submitted.len(), stream.idle.len()), (0, 4));
        bus.packets.clear();
        stream.fill(&mut bus);
        bus.frame(3, &image(3, expected), block);
        assert_eq!(next(&mut stream, &mut bus).unwrap().id, 3);
    }
    #[test]
    fn transfer_window_stays_within_a_share_of_usbfs_memory() {
        const MIB: usize = 1024 * 1024;
        assert_eq!(window(Some(16), 1), 8 * MIB);
        assert_eq!(window(Some(16), 2), 4 * MIB);
        assert_eq!(window(Some(4), 1), 2 * MIB);
        assert_eq!(window(Some(256), 1), 128 * MIB);
        assert_eq!(window(Some(0), 1), usize::MAX);
        assert_eq!(window(None, 4), usize::MAX);
        let submitted = |layout: &Layout| layout.request * layout.transfers;
        let at =
            |expected, window, memory| layout(expected, MPS, MPS, MPS, window, memory).unwrap();
        // 4000x3000 Mono8 and 5472x3648 Mono8 at Linux's default limit:
        // transfers recycled within the window, the image assembled.
        for expected in [12_000_000, 19_961_856] {
            let one = at(expected, window(Some(16), 1), None);
            assert_eq!((one.block, one.transfers, one.whole), (MIB, 8, false));
            let two = at(expected, window(Some(16), 2), None);
            assert_eq!(submitted(&two), 4 * MIB);
            let many = at(expected, window(Some(16), 8), None);
            assert_eq!((many.block, submitted(&many)), (MIB / 4, MIB));
        }
        // Frames of which four fit the window, and the memory, arrive whole
        // where the platform takes transfers that long: up to 1 MiB, the
        // length every other transfer requests, everywhere.
        let vga = at(640 * 480, window(Some(16), 1), Some(1 << 30));
        assert_eq!(
            (vga.block, vga.count, vga.tail, vga.request, vga.transfers),
            (307_200, 1, 0, 307_200, MIN_TRANSFERS)
        );
        assert!(vga.whole);
        // An image that is no whole number of packets: one final transfer.
        let odd = at(1000 * MPS + 100, usize::MAX, None);
        assert_eq!(
            (odd.count, odd.tail, odd.request, odd.whole),
            (0, 1001 * MPS, 1001 * MPS, true)
        );
        // Not where four would leave too little memory.
        assert!(!at(640 * 480, window(Some(16), 1), Some(8 * 307_200)).whole);
        let tail = at(2_304_000, window(Some(16), 1), None);
        assert_eq!((tail.count, tail.tail, tail.whole), (2, 206_848, false));
        // Without a usbfs limit, or with a raised one, larger frames arrive
        // whole too, four transfers at a time; Windows, which keeps to 1 MiB
        // transfers, keeps two frames of them submitted.
        for limit in [None, Some(0), Some(256)] {
            let unlimited = at(12_000_000, window(limit, 1), Some(4 << 30));
            let layout = (
                unlimited.block,
                unlimited.count,
                unlimited.transfers,
                unlimited.whole,
            );
            if cfg!(windows) {
                assert_eq!(layout, (MIB, 11, 28, false));
            } else {
                assert_eq!(layout, (12_000_256, 0, MIN_TRANSFERS, true));
            }
        }
    }
    /// A stream whose transfers each request `request` bytes, with whole
    /// images taken by their frames when `whole`, else copied.
    fn requesting(expected: usize, request: usize, whole: bool) -> (Stream, Bus) {
        let mut stream = Stream::new(expected, MPS, MPS, whole.then_some(request));
        let mut bus = Bus::default();
        stream.queue(&mut bus, 4, request);
        (stream, bus)
    }
    /// The requested four-transfer window stays intact; whole-image buffers
    /// come from the frame pool rather than the endpoint allocator.
    fn next_requested(stream: &mut Stream, bus: &mut Bus) -> Result<Frame> {
        let frame = stream.next(bus, Instant::now() + Duration::from_secs(1));
        assert_eq!(bus.submitted.len() + stream.idle.len(), 4);
        assert_eq!(bus.allocated, if stream.whole.is_some() { 0 } else { 4 });
        frame
    }
    /// A frame's ID and image, or the error instead, and whether its image
    /// was a transfer's own buffer.
    type Received = (std::result::Result<(u64, Vec<u8>), String>, bool);
    /// What the stream made of a camera's transfers.
    fn received(stream: &mut Stream, bus: &mut Bus, calls: usize) -> Vec<Received> {
        std::iter::repeat_with(|| {
            let frame = stream.next(bus, Instant::now() + Duration::from_millis(20));
            let own = frame
                .as_ref()
                .is_ok_and(|frame| bus.buffers.contains(&(frame.data.as_ptr() as usize)));
            let frame = frame
                .map(|frame| (frame.id, frame.data.to_vec()))
                .map_err(|error| format!("{error:#}"));
            (frame, own)
        })
        .take(calls)
        .collect()
    }
    /// Images that fill a transfer whole become their frames' buffers; the
    /// frames and errors are those of copying them, whatever the camera
    /// sends.
    #[test]
    fn whole_images_are_taken_by_their_frames_as_copies_would_be() {
        type Script = fn(&mut Bus, usize, usize);
        let scripts: [(&str, Script, usize); 9] = [
            (
                "frames",
                |bus, expected, request| {
                    for id in 1..=3 {
                        bus.frame(id, &image(id, expected), request);
                    }
                },
                4,
            ),
            (
                "zero-length packet",
                |bus, expected, _| {
                    for id in 1..=2 {
                        bus.send(&leader(id, u32::try_from(expected).unwrap(), 1));
                        bus.send(&image(id, expected));
                        bus.send(&[]);
                        bus.send(&trailer(id, expected));
                    }
                },
                3,
            ),
            (
                "image like a header",
                |bus, expected, request| {
                    for (id, header) in [(1, leader(9, 4, 2)), (2, trailer(9, 8))] {
                        let mut pixels = image(id, expected);
                        let at = expected.saturating_sub(64);
                        let n = header.len().min(expected - at);
                        pixels[at..at + n].copy_from_slice(&header[..n]);
                        bus.frame(id, &pixels, request);
                    }
                },
                3,
            ),
            (
                "short image",
                |bus, expected, request| {
                    bus.send(&leader(1, u32::try_from(expected).unwrap(), 1));
                    bus.send(&image(1, expected / 2 + 3));
                    bus.send(&trailer(1, expected / 2 + 3));
                    bus.frame(2, &image(2, expected), request);
                },
                3,
            ),
            (
                "no trailer",
                |bus, expected, request| {
                    bus.send(&leader(1, u32::try_from(expected).unwrap(), 1));
                    bus.send(&image(1, expected));
                    bus.frame(2, &image(2, expected), request);
                },
                3,
            ),
            (
                "wrong size in the trailer",
                |bus, expected, request| {
                    bus.send(&leader(1, u32::try_from(expected).unwrap(), 1));
                    bus.send(&image(1, expected));
                    bus.send(&trailer(1, expected + 1));
                    bus.frame(2, &image(2, expected), request);
                },
                3,
            ),
            (
                "left over from before",
                |bus, expected, request| {
                    bus.send(&image(9, expected / 3));
                    bus.send(&trailer(0, expected));
                    bus.frame(1, &image(1, expected), request);
                },
                2,
            ),
            (
                "transfer error",
                |bus, expected, request| {
                    bus.send(&leader(1, u32::try_from(expected).unwrap(), 1));
                    bus.fault = true;
                    bus.send(&image(1, expected));
                    bus.send(&trailer(1, expected));
                    bus.frame(2, &image(2, expected), request);
                },
                3,
            ),
            (
                "two images in a row",
                |bus, expected, request| {
                    bus.send(&leader(1, u32::try_from(expected).unwrap(), 1));
                    bus.send(&image(1, expected));
                    bus.send(&image(1, expected));
                    bus.send(&trailer(1, expected));
                    bus.frame(2, &image(2, expected), request);
                },
                3,
            ),
        ];
        // An image of whole packets in a transfer of its own, one that ends
        // in a short packet, one the trailer follows in the same transfer
        // (the transfer is longer than the image), and one smaller than a
        // leader.
        let mut taken = 0;
        for (expected, request) in [
            (8 * MPS, 8 * MPS),
            (8 * MPS + 300, 9 * MPS),
            (6 * MPS, 8 * MPS),
            (64, MPS),
        ] {
            for (name, script, calls) in scripts {
                let mut runs = [true, false].map(|whole| {
                    let (mut stream, mut bus) = requesting(expected, request, whole);
                    script(&mut bus, expected, request);
                    received(&mut stream, &mut bus, calls)
                });
                let [whole, copied] = &mut runs;
                // Copied images are never a transfer's buffer; whole ones
                // always are.
                assert!(copied.iter().all(|(_, own)| !own), "{name}");
                assert!(
                    whole.iter().all(|(frame, own)| frame.is_ok() == *own),
                    "{name} at {expected}/{request}: {whole:?}"
                );
                let frames = |run: &[Received]| {
                    run.iter()
                        .map(|(frame, _)| frame.clone())
                        .collect::<Vec<_>>()
                };
                assert_eq!(
                    frames(whole),
                    frames(copied),
                    "{name} at {expected}/{request}"
                );
                taken += whole.iter().filter(|(_, own)| *own).count();
            }
        }
        assert!(taken >= 40, "{taken}");
    }
    #[test]
    fn whole_image_buffers_are_replaced_and_recycled() {
        let (expected, request) = (8 * MPS + 300, 9 * MPS);
        let (mut stream, mut bus) = requesting(expected, request, true);
        let mut kept = Vec::new();
        for id in 1..=6 {
            bus.frame(id, &image(id, expected), request);
            let frame = stream
                .next(&mut bus, Instant::now() + Duration::from_secs(1))
                .unwrap();
            assert_eq!((frame.id, frame.data.to_vec()), (id, image(id, expected)));
            assert!(bus.buffers.contains(&(frame.data.as_ptr() as usize)));
            // Transfers keep their length; four stay submitted.
            assert!(
                bus.submitted
                    .iter()
                    .all(|(buffer, _)| buffer.requested_len() == request)
            );
            assert_eq!(bus.submitted.len() + stream.idle.len(), 4);
            kept.push(frame);
        }
        // Frames that are dropped give their buffers back for transfers.
        let addresses: std::collections::HashSet<_> = kept
            .iter()
            .map(|frame| frame.data.as_ptr() as usize)
            .collect();
        drop(kept);
        bus.buffers.clear();
        for id in 7..=12 {
            bus.frame(id, &image(id, expected), request);
            stream
                .next(&mut bus, Instant::now() + Duration::from_secs(1))
                .unwrap();
        }
        assert!(
            bus.buffers
                .iter()
                .any(|address| addresses.contains(address))
        );
        assert_eq!(bus.allocated, 0, "no endpoint allocations");
    }
    #[test]
    fn stream_endpoint_reset_halts_it_first() {
        // Aravis: LIBUSB_ENDPOINT_OUT | LIBUSB_REQUEST_TYPE_STANDARD |
        // LIBUSB_RECIPIENT_ENDPOINT, SET_FEATURE, ENDPOINT_HALT, endpoint.
        let request = halt(0x81);
        let request_type = ((request.control_type as u8) << 5) | request.recipient as u8;
        assert_eq!(request_type, 0x02);
        assert_eq!(
            (request.request, request.value, request.index, request.data),
            (0x03, 0, 0x81, &[][..])
        );
    }
    #[test]
    fn malformed_packets_fail_without_panics() {
        for n in 0..12 {
            ack(&vec![0; n], 1).unwrap_err();
        }
        for n in 0..52 {
            parse_leader(&vec![0; n]).unwrap_err();
        }
        assert_eq!(aligned(53, 1024).unwrap(), 1024);
        aligned(53, 3).unwrap_err();
    }
    #[test]
    fn ids_grouped_whole_and_split_images_keep_original_chunks_and_encoding() {
        use crate::types::IdsPixelFormat;
        for (format, pair) in [
            (
                IdsPixelFormat::Mono10g40,
                vec![0, 85, 170, 255, 249, 255, 170, 85, 0, 6],
            ),
            (
                IdsPixelFormat::Mono12g24,
                vec![
                    0x12, 0xab, 0xc3, 0x45, 0xde, 0xf6, 0, 0xff, 0xf0, 0x78, 0x13, 0x59,
                ],
            ),
            (
                IdsPixelFormat::BayerRg10g40,
                vec![0xe8, 0x24, 0xb5, 0x78, 0x39, 0x2d, 0x8e, 0x70, 0xe3, 0xc6],
            ),
            (
                IdsPixelFormat::BayerRg12g24,
                vec![
                    0xe7, 0x2a, 0x61, 0xb4, 0x58, 0x3d, 0x36, 0x95, 0xec, 0x74, 0xd1, 0x92,
                ],
            ),
            (
                IdsPixelFormat::BayerGr10g40,
                vec![0xe8, 0x24, 0xb5, 0x78, 0x39, 0x2d, 0x8e, 0x70, 0xe3, 0xc6],
            ),
            (
                IdsPixelFormat::BayerGr12g24,
                vec![
                    0xe7, 0x2a, 0x61, 0xb4, 0x58, 0x3d, 0x36, 0x95, 0xec, 0x74, 0xd1, 0x92,
                ],
            ),
            (
                IdsPixelFormat::BayerGb10g40,
                vec![0xe8, 0x24, 0xb5, 0x78, 0x39, 0x2d, 0x8e, 0x70, 0xe3, 0xc6],
            ),
            (
                IdsPixelFormat::BayerGb12g24,
                vec![
                    0xe7, 0x2a, 0x61, 0xb4, 0x58, 0x3d, 0x36, 0x95, 0xec, 0x74, 0xd1, 0x92,
                ],
            ),
            (
                IdsPixelFormat::BayerBg10g40,
                vec![0xe8, 0x24, 0xb5, 0x78, 0x39, 0x2d, 0x8e, 0x70, 0xe3, 0xc6],
            ),
            (
                IdsPixelFormat::BayerBg12g24,
                vec![
                    0xe7, 0x2a, 0x61, 0xb4, 0x58, 0x3d, 0x36, 0x95, 0xec, 0x74, 0xd1, 0x92,
                ],
            ),
        ] {
            let pixels = pair.repeat(300);
            let row = pair.len() / 2;
            let height = 600;
            for chunks in [false, true] {
                let mut payload = Vec::new();
                for line in pixels.chunks_exact(row) {
                    payload.extend_from_slice(line);
                    payload.extend_from_slice(&[0xa5, 0x5a]);
                }
                let image_bytes = payload.len();
                if chunks {
                    payload.extend_from_slice(&u64::MAX.to_le_bytes());
                    payload.extend_from_slice(&0xffff_1234_u32.to_le_bytes());
                    payload.extend_from_slice(&8_u32.to_le_bytes());
                }
                for whole in [false, true] {
                    let request = if whole {
                        payload.len().div_ceil(MPS) * MPS
                    } else {
                        MPS
                    };
                    let (mut stream, mut bus) = requesting(payload.len(), request, whole);
                    stream.pixel_encodings = vec![format.encoding(None)];
                    let mut metadata = leader(1, 4, height);
                    metadata[28..32].copy_from_slice(&format.code().to_le_bytes());
                    metadata[48..50].copy_from_slice(&2_u16.to_le_bytes());
                    let mut tail = trailer(1, payload.len());
                    tail[28..32].copy_from_slice(&height.to_le_bytes());
                    if chunks {
                        metadata[18..20].copy_from_slice(&0x4001_u16.to_le_bytes());
                        tail.extend_from_slice(&0xaabb_ccdd_u32.to_le_bytes());
                        tail[6..8].copy_from_slice(&36_u16.to_le_bytes());
                    }
                    bus.send(&metadata);
                    bus.send(&payload);
                    bus.send(&tail);
                    let frame = next_requested(&mut stream, &mut bus).unwrap();
                    assert_eq!(&*frame.data, &pixels);
                    assert_eq!(frame.pixel_encoding, Some(format.encoding(None)));
                    crate::frame::validate_source(&frame).unwrap();
                    if chunks {
                        let retained = frame.chunks.unwrap();
                        assert_eq!(&*retained.payload, &payload);
                        assert_eq!(retained.image_bytes, image_bytes);
                        let layout = retained.image_layout.unwrap();
                        assert!(layout.pfnc_code.is_none());
                        assert_eq!(layout.pixel_encoding, Some(format.encoding(None)));
                        assert_eq!(layout.row_stride, row + 2);
                        if whole {
                            assert!(bus.buffers.contains(&(retained.payload.as_ptr() as usize)));
                        }
                    } else {
                        assert!(frame.chunks.is_none());
                    }
                }
            }
        }
    }
    #[test]
    fn ids_grouped_leader_requires_qualification_and_complete_groups_then_recovers() {
        use crate::types::IdsPixelFormat;
        for format in IdsPixelFormat::ALL {
            for qualified in [false, true] {
                let (mut stream, mut bus) = requesting(10, MPS, false);
                if qualified {
                    stream.pixel_encodings = vec![format.encoding(None)];
                }
                let mut metadata = leader(1, if qualified { 3 } else { 4 }, 2);
                metadata[28..32].copy_from_slice(&format.code().to_le_bytes());
                bus.send(&metadata);
                bus.send(&[0; 10]);
                bus.send(&trailer(1, 10));
                let pixels = image(2, 10);
                bus.frame(2, &pixels, MPS);
                let error = next_requested(&mut stream, &mut bus).unwrap_err();
                assert_eq!(crate::types::classify(&error).kind, ErrorKind::Unsupported);
                let frame = next_requested(&mut stream, &mut bus).unwrap();
                assert_eq!(frame.id, 2);
                assert_eq!(frame.data, pixels);
                assert!(frame.pixel_encoding.is_none());
            }
        }
    }

    #[test]
    fn packed_bicolor_whole_and_split_rows_keep_exact_packed_chunks() {
        let literal: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/bicolor/packed-literals.json"
        ))
        .unwrap();
        for (case, codes) in [
            (0, [0x0214_00a8_u32, 0x0214_00aa]),
            (1, [0x0218_00ac, 0x0218_00ae]),
        ] {
            let case = &literal["cases"][case];
            let width = u32::try_from(case["width"].as_u64().unwrap()).unwrap();
            let pair = crate::types::decode_hex(case["tight_hex"].as_str().unwrap(), 128).unwrap();
            let pixels = pair.repeat(300);
            let row_bytes = usize::try_from(case["row_bytes"].as_u64().unwrap()).unwrap();
            for format in codes {
                for chunks in [false, true] {
                    let mut payload = Vec::new();
                    for row in pixels.chunks_exact(row_bytes) {
                        payload.extend_from_slice(row);
                        payload.extend_from_slice(&[0xa5, 0x5a, 0xef]);
                    }
                    let image_bytes = payload.len();
                    if chunks {
                        payload.extend_from_slice(&u64::MAX.to_le_bytes());
                        payload.extend_from_slice(&0xffff_1234_u32.to_le_bytes());
                        payload.extend_from_slice(&8_u32.to_le_bytes());
                    }
                    for whole in [false, true] {
                        let request = if whole {
                            payload.len().div_ceil(MPS) * MPS
                        } else {
                            MPS
                        };
                        let (mut stream, mut bus) = requesting(payload.len(), request, whole);
                        let mut metadata = leader(1, width, 600);
                        metadata[28..32].copy_from_slice(&format.to_le_bytes());
                        metadata[48..50].copy_from_slice(&3_u16.to_le_bytes());
                        let mut tail = trailer(1, payload.len());
                        tail[28..32].copy_from_slice(&600_u32.to_le_bytes());
                        if chunks {
                            metadata[18..20].copy_from_slice(&0x4001_u16.to_le_bytes());
                            tail.extend_from_slice(&0xaabb_ccdd_u32.to_le_bytes());
                            tail[6..8].copy_from_slice(&36_u16.to_le_bytes());
                        }
                        bus.send(&metadata);
                        bus.send(&payload);
                        bus.send(&tail);
                        let frame = next_requested(&mut stream, &mut bus).unwrap();
                        assert_eq!(
                            (frame.width, frame.height, frame.pixel_format),
                            (width, 600, format)
                        );
                        assert_eq!(frame.data.as_ref(), pixels);
                        crate::frame::validate_source(&frame).unwrap();
                        if chunks {
                            let retained = frame.chunks.unwrap();
                            assert_eq!(retained.payload.as_ref(), payload);
                            assert_eq!(retained.image_bytes, image_bytes);
                            let layout = retained.image_layout.unwrap();
                            assert_eq!(layout.pfnc_code, Some(format));
                            assert_eq!(layout.row_bytes, row_bytes);
                            assert_eq!(layout.row_stride, row_bytes + 3);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn odd10p_usb_leader_rejects_then_a_complete_packed_frame_recovers() {
        for whole in [false, true] {
            let (mut stream, mut bus) = requesting(64, MPS, whole);
            let mut bad = leader(1, 3, 2);
            bad[28..32].copy_from_slice(&0x0214_00a8_u32.to_le_bytes());
            let mut good = leader(2, 4, 2);
            good[28..32].copy_from_slice(&0x0214_00a8_u32.to_le_bytes());
            let literal: serde_json::Value = serde_json::from_str(include_str!(
                "../../tests/fixtures/bicolor/packed-literals.json"
            ))
            .unwrap();
            let raw =
                crate::types::decode_hex(literal["cases"][0]["tight_hex"].as_str().unwrap(), 128)
                    .unwrap();
            bus.send(&bad);
            bus.send(&good);
            bus.send(&raw);
            let mut tail = trailer(2, raw.len());
            tail[28..32].copy_from_slice(&2_u32.to_le_bytes());
            bus.send(&tail);
            let error = next_requested(&mut stream, &mut bus).unwrap_err();
            assert_eq!(crate::types::classify(&error).kind, ErrorKind::Unsupported);
            let frame = next_requested(&mut stream, &mut bus).unwrap();
            assert_eq!(frame.id, 2);
            assert_eq!(frame.data.as_ref(), raw);
        }
    }
}
