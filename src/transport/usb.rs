//! USB3 Vision transport using native OS USB APIs through nusb (no libusb).
//! UVCP/UVSP wire fields and bootstrap register addresses follow USB3 Vision.
use crate::{
    genicam::decode_xml,
    types::{Backend, CameraInfo, Categorized, ErrorKind, Frame, RegisterIo, Transport},
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use nusb::{
    DeviceInfo, Endpoint, Interface, MaybeFuture,
    descriptors::TransferType,
    transfer::{
        Buffer, Bulk, Completion, ControlOut, ControlType, In, Out, Recipient, TransferError,
    },
};
use std::time::{Duration, Instant};

const MAGIC: u32 = 0x4356_3355;
const LEADER_MAGIC: u32 = 0x4c56_3355;
const TRAILER_MAGIC: u32 = 0x5456_3355;
const READ_CMD: u16 = 0x0800;
const WRITE_CMD: u16 = 0x0802;
const PENDING_ACK: u16 = 0x0805;
const MAX_FRAME: usize = 256 * 1024 * 1024;
const SIRM_CONTROL: u64 = 0x04;

fn is_u3v_interface(class: u8, subclass: u8, protocol: u8) -> bool {
    class == 0xef && subclass == 0x05 && protocol == 0
}
fn camera_info(d: &DeviceInfo) -> CameraInfo {
    let location = format!("{}:{}", d.bus_id(), d.device_address());
    let serial = d.serial_number().unwrap_or("").to_string();
    CameraInfo {
        id: format!(
            "usb:{:04x}:{:04x}:{}",
            d.vendor_id(),
            d.product_id(),
            if serial.is_empty() {
                &location
            } else {
                &serial
            }
        ),
        transport: Transport::Usb3,
        vendor: d.manufacturer_string().unwrap_or("Unknown").into(),
        model: d.product_string().unwrap_or("USB3 Vision camera").into(),
        serial,
        address: Some(location),
        profile: Default::default(),
    }
}
pub fn discover() -> Result<Vec<CameraInfo>> {
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
            ErrorKind::Unavailable,
            format!(
                "No permission to {doing} USB3 Vision camera {name}{at}. Run `capturefab doctor` for the udev rule that fixes it."
            ),
        ),
        (Usb::PermissionDenied, _) => (
            ErrorKind::Unavailable,
            format!("No permission to {doing} USB3 Vision camera {name}. Run `capturefab doctor`."),
        ),
        (Usb::Busy, _) => (
            ErrorKind::Unavailable,
            format!(
                "USB3 Vision camera {name} is in use by another application (for example a vendor viewer or SDK). Close it and connect again."
            ),
        ),
        (Usb::Disconnected, _) => (
            ErrorKind::Unavailable,
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

/// The camera as errors name it after "USB3 Vision camera": its product
/// string, or without one its USB IDs, which tell cameras apart.
fn display_name(product: Option<&str>, vendor_id: u16, product_id: u16) -> String {
    product
        .map(str::trim)
        .filter(|product| !product.is_empty())
        .map_or_else(
            || format!("{vendor_id:04x}:{product_id:04x}"),
            str::to_owned,
        )
}

#[derive(Debug, Clone, Copy)]
struct InterfaceEndpoints {
    number: u8,
    alternate: u8,
    input: u8,
    output: Option<u8>,
}

pub fn open(info: &CameraInfo, timeout: Duration) -> Result<Box<dyn Backend>> {
    ensure!(
        info.transport == Transport::Usb3,
        "camera is not a USB3 Vision device"
    );
    let device_info = nusb::list_devices()
        .wait()?
        .find(|d| camera_info(d).id == info.id)
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
            let (kind, message) = access_error(
                stage,
                error.kind(),
                &name,
                node.as_deref(),
                std::env::consts::OS,
            );
            // The OS's own words stay on the line for whoever debugs it.
            let message = format!("{message} (USB: {error})");
            // On Linux the udev rule fixes it; the desktop app offers to
            // copy that line.
            let message =
                if error.kind() == nusb::ErrorKind::PermissionDenied && cfg!(target_os = "linux") {
                    crate::doctor::with_usb_access_fix(message)
                } else {
                    message
                };
            Categorized::error(kind, message)
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
    let mut backend = UsbBackend {
        control_in: control_interface.endpoint::<Bulk, In>(control.input)?,
        control_out: control_interface.endpoint::<Bulk, Out>(control.output.unwrap())?,
        stream_in: data_interface.endpoint::<Bulk, In>(data.input)?,
        stream_interface: data_interface,
        _control_interface: control_interface,
        timeout: timeout.max(Duration::from_millis(20)),
        id: 0,
        max_command: 65548,
        max_ack: 65548,
        sirm: 0,
        stream: None,
    };
    let response_ms = backend.u32(0x01cc)?;
    ensure!(
        response_ms <= 60000,
        "camera reports an invalid maximum response time"
    );
    backend.timeout = backend
        .timeout
        .max(Duration::from_millis(response_ms as u64));
    let sbrm = backend.u64(0x01d8)?;
    backend.max_command = backend.u32(sbrm + 0x14)? as usize;
    backend.max_ack = backend.u32(sbrm + 0x18)? as usize;
    ensure!(
        (24..=65548).contains(&backend.max_command) && (16..=65548).contains(&backend.max_ack),
        "invalid USB3 Vision control transfer limits"
    );
    backend.sirm = backend.u64(sbrm + 0x20)?;
    ensure!(
        backend.sirm != 0,
        "camera has no USB3 Vision stream register map"
    );
    Ok(Box::new(backend))
}

struct UsbBackend {
    control_in: Endpoint<Bulk, In>,
    control_out: Endpoint<Bulk, Out>,
    stream_in: Endpoint<Bulk, In>,
    stream_interface: Interface,
    _control_interface: Interface,
    timeout: Duration,
    id: u16,
    max_command: usize,
    max_ack: usize,
    sirm: u64,
    stream: Option<Stream>,
}
/// Longest stream transfer, as in Aravis: payload transfers are this long at
/// most.
const MAX_TRANSFER: usize = 1024 * 1024;
/// Stream transfer bytes kept submitted, as Aravis caps them.
const MAX_WINDOW: usize = 8 * 1024 * 1024;
/// Fewest stream transfers kept submitted.
const MIN_TRANSFERS: usize = 4;
/// Linux's errno when usbfs memory cannot cover another transfer.
const ENOMEM: u32 = 12;
const USBFS_EXHAUSTED: &str = "USB stream transfers cannot be submitted because usbfs memory is exhausted; raise /sys/module/usbcore/parameters/usbfs_memory_mb";

/// Bytes of stream transfers to keep submitted. Linux charges every usbfs
/// transfer, mapped or copied, against one limit that all USB devices on
/// the computer share, so each USB3 Vision camera takes an equal share of
/// half of it.
fn window(usbfs_memory_mb: Option<u64>, cameras: usize) -> usize {
    match usbfs_memory_mb {
        Some(mb) if mb > 0 => MAX_WINDOW.min(((mb.min(4095) as usize) << 20) / 2 / cameras.max(1)),
        _ => MAX_WINDOW,
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

/// How the camera splits a frame's image into payload transfers, and the
/// transfers kept submitted to receive it.
struct Layout {
    block: usize,
    count: usize,
    tail: usize,
    /// Length every transfer requests.
    request: usize,
    transfers: usize,
}
fn layout(
    expected: usize,
    leader: usize,
    trailer: usize,
    alignment: usize,
    window: usize,
) -> Result<Layout> {
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
    })
}

/// Whether Linux refused to submit a transfer for lack of usbfs memory. The
/// transfer never reached the camera, so no data is missing.
fn refused(status: &Result<(), TransferError>) -> bool {
    (cfg!(any(target_os = "linux", target_os = "android")) || cfg!(test))
        && *status == Err(TransferError::Unknown(ENOMEM))
}

/// SET_FEATURE(ENDPOINT_HALT) for an endpoint.
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
/// what each transfer holds rather than from its place in the queue, and
/// the stream recovers at the next leader after any lost or extra data.
struct Stream {
    /// Image bytes in each frame.
    expected: usize,
    /// Longest leader and trailer the camera sends.
    leader: usize,
    trailer: usize,
    /// Buffers not submitted.
    idle: Vec<Buffer>,
    /// Transfers submitted and not yet returned.
    submitted: usize,
    frame: Option<Frame>,
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
        Endpoint::allocate(self, len)
    }
    fn submit(&mut self, buffer: Buffer) {
        Endpoint::submit(self, buffer)
    }
    fn wait(&mut self, timeout: Duration) -> Option<Completion> {
        self.wait_next_complete(timeout)
    }
    fn cancel_all(&mut self) {
        Endpoint::cancel_all(self)
    }
}
impl Stream {
    fn new(expected: usize, leader: usize, trailer: usize) -> Self {
        Self {
            expected,
            leader,
            trailer,
            idle: Vec::new(),
            submitted: 0,
            frame: None,
            starved: false,
        }
    }
    /// Allocate and submit `count` transfers of `len` bytes. Allocating maps
    /// memory, so this happens only when acquisition starts.
    fn queue(&mut self, transfers: &mut impl Transfers, count: usize, len: usize) {
        self.idle
            .extend((0..count).map(|_| transfers.allocate(len)));
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
    fn receive(&mut self, completion: Completion) -> Result<Option<Frame>> {
        let Completion {
            mut buffer, status, ..
        } = completion;
        let received = if refused(&status) {
            self.starved = true;
            Ok(None)
        } else if let Err(error) = status {
            self.frame = None;
            Err(error).context("USB stream transfer failed")
        } else {
            self.starved = false;
            let full = buffer.len() == buffer.requested_len();
            self.take(&buffer, full)
        };
        buffer.clear();
        self.idle.push(buffer);
        received
    }
    /// Returns the frame a trailer completes.
    fn take(&mut self, bytes: &[u8], full: bool) -> Result<Option<Frame>> {
        // Some cameras end the last payload transfer with a zero-length packet.
        if bytes.is_empty() {
            return Ok(None);
        }
        if is_header(bytes, LEADER_MAGIC, 52, self.leader) {
            let open = self.frame.take().is_some();
            let mut frame = parse_leader(bytes)?;
            frame.data.reserve(self.expected);
            self.frame = Some(frame);
            ensure!(!open, "USB stream image ended without a trailer");
            return Ok(None);
        }
        // Without an open image, the data belongs to one already dropped or
        // was left over from before acquisition started.
        let Some(frame) = self.frame.as_mut() else {
            return Ok(None);
        };
        let trailer = if is_header(bytes, TRAILER_MAGIC, 28, self.trailer) {
            bytes
        } else {
            let image = (self.expected - frame.data.len()).min(bytes.len());
            frame.data.extend_from_slice(&bytes[..image]);
            if image == bytes.len() {
                if full || frame.data.len() == self.expected {
                    return Ok(None);
                }
                self.frame = None;
                bail!("short USB image payload transfer");
            }
            // The image ended on a packet boundary within the transfer, so
            // the trailer followed in it.
            &bytes[image..]
        };
        let frame = self.frame.take().context("USB stream frame missing")?;
        validate_trailer(trailer, &frame, self.expected)?;
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
fn command(cmd: u16, id: u16, payload: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        payload.len() <= u16::MAX as usize,
        "UVCP command payload exceeds 65535 bytes"
    );
    let mut packet = Vec::with_capacity(12 + payload.len());
    packet.extend_from_slice(&MAGIC.to_le_bytes());
    packet.extend_from_slice(&0x4000u16.to_le_bytes());
    packet.extend_from_slice(&cmd.to_le_bytes());
    packet.extend_from_slice(&(payload.len() as u16).to_le_bytes());
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
/// Check a WriteMem acknowledgement. GenCP lets it carry the written length
/// only when the camera's Device Capability bit 11 is set; otherwise it is
/// empty.
fn write_acknowledged(response: &[u8], len: usize) -> Result<()> {
    ensure!(
        response.is_empty() || (response.len() == 4 && le16(response, 2)? as usize == len),
        "camera returned invalid UVCP write acknowledgement"
    );
    Ok(())
}
impl UsbBackend {
    fn transact(&mut self, cmd: u16, payload: &[u8]) -> Result<Vec<u8>> {
        self.id = self.id.wrapping_add(1).max(1);
        let packet = command(cmd, self.id, payload)?;
        ensure!(
            packet.len() <= self.max_command,
            "UVCP command exceeds camera transfer limit"
        );
        let sent = self
            .control_out
            .transfer_blocking(packet.clone().into(), self.timeout)
            .into_result()
            .context("USB control write failed or timed out")?;
        ensure!(sent.len() == packet.len(), "short USB control write");
        let mut deadline = Instant::now() + self.timeout;
        let hard_deadline = Instant::now() + self.timeout.max(Duration::from_secs(60));
        for _ in 0..64 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "USB camera control acknowledgement timed out"
            );
            let size = aligned(self.max_ack, self.control_in.max_packet_size())?;
            let response = self
                .control_in
                .transfer_blocking(Buffer::new(size), remaining)
                .into_result()
                .context("USB control read failed or timed out")?;
            // A stale response may remain after a timed-out operation.
            if response.len() >= 12 && le16(&response, 10)? != self.id {
                continue;
            }
            let (response_cmd, data) = ack(&response, self.id)?;
            if response_cmd == PENDING_ACK {
                ensure!(data.len() >= 4, "truncated UVCP pending acknowledgement");
                let extension = Duration::from_millis(le16(data, 2)? as u64);
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
    fn u32(&mut self, address: u64) -> Result<u32> {
        le32(&self.read_memory(address, 4)?, 0)
    }
    fn u64(&mut self, address: u64) -> Result<u64> {
        le64(&self.read_memory(address, 8)?, 0)
    }
    fn put32(&mut self, offset: u64, value: usize) -> Result<()> {
        ensure!(
            value <= u32::MAX as usize,
            "stream register value exceeds u32"
        );
        self.write_memory(self.sirm + offset, &(value as u32).to_le_bytes())
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
        let expected = expected as usize;
        let leader = aligned((self.u32(self.sirm + 0x10)? as usize).max(52), alignment)?;
        let trailer = aligned((self.u32(self.sirm + 0x14)? as usize).max(28), alignment)?;
        ensure!(
            leader <= MAX_TRANSFER && trailer <= MAX_TRANSFER,
            "USB stream header transfer is too large"
        );
        let layout = layout(expected, leader, trailer, alignment, usbfs_window())?;
        self.put32(0x18, leader)?;
        self.put32(0x2c, trailer)?;
        self.put32(0x1c, layout.block)?;
        self.put32(0x20, layout.count)?;
        self.put32(0x24, layout.tail)?;
        self.put32(0x28, 0)?;
        self.reset_stream_endpoint()?;
        // Submit the transfers before AcquisitionStart.
        let mut stream = Stream::new(expected, leader, trailer);
        stream.queue(&mut self.stream_in, layout.transfers, layout.request);
        self.stream = Some(stream);
        self.put32(SIRM_CONTROL, 1)
    }
    /// Halt the stream endpoint, then clear the halt, as Aravis and the NI
    /// driver reset it, so the camera drops any partly sent block. A host
    /// may refuse to halt it; clearing the halt still resets it.
    fn reset_stream_endpoint(&mut self) -> Result<()> {
        let _ = self
            .stream_interface
            .control_out(halt(self.stream_in.endpoint_address()), self.timeout)
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
        let deadline = Instant::now() + self.timeout;
        let stream = self.stream.as_mut().context("USB acquisition stopped")?;
        stream.drain(&mut self.stream_in, deadline)?;
        self.reset_stream_endpoint()?;
        let stream = self.stream.as_mut().context("USB acquisition stopped")?;
        stream.fill(&mut self.stream_in);
        self.put32(SIRM_CONTROL, 1)
    }
}
impl RegisterIo for UsbBackend {
    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        ensure!(length <= MAX_FRAME, "USB memory request exceeds size limit");
        address
            .checked_add(length as u64)
            .context("USB memory range overflow")?;
        let chunk_size = self.max_ack.saturating_sub(12).min(u16::MAX as usize);
        ensure!(chunk_size > 0, "invalid USB read transfer limit");
        let mut result = Vec::with_capacity(length);
        while result.len() < length {
            let len = (length - result.len()).min(chunk_size);
            let mut request = Vec::with_capacity(12);
            request.extend_from_slice(&(address + result.len() as u64).to_le_bytes());
            request.extend_from_slice(&0u16.to_le_bytes());
            request.extend_from_slice(&(len as u16).to_le_bytes());
            let bytes = self.transact(READ_CMD, &request)?;
            ensure!(bytes.len() == len, "camera returned short UVCP memory data");
            result.extend_from_slice(&bytes);
        }
        Ok(result)
    }
    fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
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
        for (i, chunk) in data.chunks(size).enumerate() {
            let mut request = Vec::with_capacity(8 + chunk.len());
            request.extend_from_slice(&(address + (i * size) as u64).to_le_bytes());
            request.extend_from_slice(chunk);
            let response = self.transact(WRITE_CMD, &request)?;
            write_acknowledged(&response, chunk.len())?;
        }
        Ok(())
    }
}
/// The address and size of each manifest entry for a raw or zipped device
/// XML, newest version first. GenCP puts the file type in bits 0-2 of the
/// entry's second word, where 1 is a Buffer XML holding only chunk nodes,
/// and the 6-bit file format in bits 10-15.
fn device_xml_entries(entries: &[u8]) -> Result<Vec<(u64, u64)>> {
    let mut candidates = Vec::new();
    for entry in entries.as_chunks::<64>().0 {
        let schema = le32(entry, 4)?;
        if schema & 7 == 0 && (schema >> 10) & 0x3f <= 1 {
            candidates.push((le32(entry, 0)?, le64(entry, 8)?, le64(entry, 16)?));
        }
    }
    candidates.sort_by_key(|e| std::cmp::Reverse(e.0));
    Ok(candidates
        .into_iter()
        .map(|(_, address, size)| (address, size))
        .collect())
}
fn parse_leader(bytes: &[u8]) -> Result<Frame> {
    ensure!(
        le32(bytes, 0)? == LEADER_MAGIC,
        "USB stream is out of sync: expected image leader"
    );
    let size = le16(bytes, 6)? as usize;
    ensure!(
        size >= 52 && size <= bytes.len(),
        "invalid UVSP leader size"
    );
    ensure!(
        le16(bytes, 18)? == 1,
        "USB stream payload type is unsupported (only unchunked images are supported)"
    );
    ensure!(
        le16(bytes, 48)? == 0,
        "USB stream image row padding is not supported"
    );
    let width = le32(bytes, 32)?;
    let height = le32(bytes, 36)?;
    ensure!(
        width != 0 && height != 0,
        "camera returned zero image dimensions"
    );
    Ok(Frame {
        id: le64(bytes, 8)?,
        width,
        height,
        pixel_format: le32(bytes, 28)?,
        timestamp_ns: le64(bytes, 20)?,
        data: Vec::new(),
    })
}
fn validate_trailer(bytes: &[u8], frame: &Frame, expected: usize) -> Result<()> {
    ensure!(
        le32(bytes, 0)? == TRAILER_MAGIC,
        "USB stream is out of sync: expected image trailer"
    );
    let size = le16(bytes, 6)? as usize;
    ensure!(
        size >= 28 && size <= bytes.len(),
        "invalid UVSP trailer size"
    );
    ensure!(le64(bytes, 8)? == frame.id, "USB stream frame ID mismatch");
    ensure!(
        le16(bytes, 16)? == 0,
        "USB stream trailer reports an incomplete frame"
    );
    ensure!(
        le64(bytes, 20)? == expected as u64 && frame.data.len() == expected,
        "USB stream frame size mismatch"
    );
    Ok(())
}
impl Backend for UsbBackend {
    fn xml(&mut self) -> Result<String> {
        let address = self.u64(0x01d0)?;
        let count = self.u64(address)? as usize;
        ensure!(
            (1..=64).contains(&count),
            "invalid USB3 Vision manifest entry count"
        );
        let entries = self.read_memory(
            address
                .checked_add(8)
                .context("manifest address overflow")?,
            count * 64,
        )?;
        let mut last_error = anyhow!("USB manifest contains no raw or ZIP device XML entry");
        for (address, size) in device_xml_entries(&entries)? {
            if size == 0 || size > 32 * 1024 * 1024 {
                last_error = anyhow!("GenICam manifest XML size is outside 1..32 MiB");
                continue;
            }
            match self
                .read_memory(address, size as usize)
                .and_then(|data| decode_xml(&data))
                .and_then(|xml| {
                    crate::genicam::NodeMap::parse(&xml)?;
                    Ok(xml)
                }) {
                Ok(xml) => return Ok(xml),
                Err(error) => last_error = error,
            }
        }
        Err(last_error.context("cannot load USB3 Vision GenICam XML"))
    }
    fn start(&mut self, _payload_size: usize) -> Result<()> {
        ensure!(self.stream.is_none(), "USB acquisition already started");
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
        let deadline = Instant::now() + self.timeout;
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
    use std::collections::VecDeque;

    #[test]
    fn open_failures_name_the_fix_for_this_os() {
        use nusb::ErrorKind as Usb;
        let node = Some("/dev/bus/usb/002/003");
        let cases = [
            (
                Stage::Open,
                Usb::PermissionDenied,
                "linux",
                ErrorKind::Unavailable,
                "/dev/bus/usb/002/003). Run `capturefab doctor` for the udev rule",
            ),
            (
                Stage::Open,
                Usb::PermissionDenied,
                "macos",
                ErrorKind::Unavailable,
                "Run `capturefab doctor`.",
            ),
            (
                Stage::ClaimControl,
                Usb::Busy,
                "linux",
                ErrorKind::Unavailable,
                "in use by another application",
            ),
            (
                Stage::ClaimStream,
                Usb::Busy,
                "windows",
                ErrorKind::Unavailable,
                "Close it and connect again.",
            ),
            (
                Stage::Open,
                Usb::Disconnected,
                "macos",
                ErrorKind::Unavailable,
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
        assert_eq!(crate::cli::error_code(&error), ("unavailable", 3));
        // The desktop app recognizes the refusal by doctor's wording.
        let (_, message) = access_error(
            Stage::Open,
            Usb::PermissionDenied,
            "acA",
            Some("/dev/bus/usb/002/003"),
            "linux",
        );
        let doctor =
            crate::doctor::usb_access_error("acA", std::path::Path::new("/dev/bus/usb/002/003"));
        assert_eq!(format!("{doctor:#}").lines().next(), Some(message.as_str()));
    }
    #[test]
    fn cameras_without_a_product_string_are_named_by_usb_ids() {
        assert_eq!(display_name(Some(" acA1920 "), 0x2676, 0xba02), "acA1920");
        assert_eq!(display_name(Some(""), 0x2676, 0xba02), "2676:ba02");
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
    fn uvcp_little_endian_wire_packet() {
        let payload = [0x12, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 0];
        let packet = command(READ_CMD, 7, &payload).unwrap();
        assert_eq!(
            &packet[..12],
            &[0x55, 0x33, 0x56, 0x43, 0, 0x40, 0, 8, 12, 0, 7, 0]
        );
        let mut response = command(READ_CMD + 1, 7, &[1, 2, 3, 4]).unwrap();
        response[4..6].fill(0);
        assert_eq!(ack(&response, 7).unwrap(), (0x0801, &[1, 2, 3, 4][..]));
        assert!(ack(&response, 8).is_err());
        // Warning severity: the command ran.
        response[4..6].copy_from_slice(&0x4001u16.to_le_bytes());
        assert_eq!(ack(&response, 7).unwrap(), (0x0801, &[1, 2, 3, 4][..]));
        response[4..6].copy_from_slice(&0x8003u16.to_le_bytes());
        let error = ack(&response, 7).unwrap_err();
        assert_eq!(error.downcast_ref::<UvcpStatus>().unwrap().0, 0x8003);
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
        let schema = |file_type: u32, format: u32| 0x0101_0000 | format << 10 | file_type;
        let manifest = [
            entry(0x0200_0000, schema(1, 0), 0x1000, 100),
            entry(0x0100_0002, schema(0, 1), 0x2000, 200),
            entry(0x0100_0001, schema(0, 0), 0x3000, 300),
            entry(0x0300_0000, schema(0, 33), 0x4000, 400),
        ]
        .concat();
        assert_eq!(
            device_xml_entries(&manifest).unwrap(),
            [(0x2000, 200), (0x3000, 300)]
        );
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
        let mut frame = parse_leader(&leader).unwrap();
        frame.data = vec![0; 8];
        let mut trailer = vec![0; 28];
        trailer[..4].copy_from_slice(&TRAILER_MAGIC.to_le_bytes());
        trailer[6..8].copy_from_slice(&28u16.to_le_bytes());
        trailer[8..16].copy_from_slice(&19u64.to_le_bytes());
        trailer[20..28].copy_from_slice(&8u64.to_le_bytes());
        validate_trailer(&trailer, &frame, 8).unwrap();
        assert!(validate_trailer(&trailer, &frame, 7).is_err());
        trailer[16] = 1;
        assert!(validate_trailer(&trailer, &frame, 8).is_err());
        leader[18] = 2;
        assert!(parse_leader(&leader).is_err());
    }
    const MPS: usize = 1024;
    /// Stands in for the stream endpoint and the camera behind it. The
    /// camera's transfers queue as packets, and each submitted transfer in
    /// turn takes packets until it is full or a short packet ends it.
    #[derive(Default)]
    struct Bus {
        allocated: usize,
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
            self.send(&leader(id, image.len() as u32, 1));
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
            let refused = self.refuse > 0;
            self.refuse = self.refuse.saturating_sub(1);
            let status = refused.then_some(TransferError::Unknown(ENOMEM));
            self.submitted.push_back((buffer, status));
        }
        fn wait(&mut self, _timeout: Duration) -> Option<Completion> {
            let (buffer, status) = self.submitted.front()?;
            let status = match status {
                Some(error) => Err(*error),
                None => {
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
                        let buffer = &mut self.submitted.front_mut().unwrap().0;
                        packets.iter().for_each(|p| buffer.extend_from_slice(p));
                        Ok(())
                    }
                }
            };
            let (buffer, _) = self.submitted.pop_front().unwrap();
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
    /// An image trailer with SizeY, as U3V 1.0 cameras send it.
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
        let mut stream = Stream::new(expected, MPS, MPS);
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
        (0..len).map(|i| (i as u64 * 7 + id) as u8).collect()
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
                assert_eq!((frame.id, frame.data), (id, image(id, expected)));
            }
        }
        // Some cameras end the last payload transfer with a zero-length
        // packet before the trailer.
        let (mut stream, mut bus) = streaming(12 * MPS, block);
        for id in 1..=3 {
            bus.send(&leader(id, 12 * MPS as u32, 1));
            for chunk in image(id, 12 * MPS).chunks(block) {
                bus.send(chunk);
            }
            bus.send(&[]);
            bus.send(&trailer(id, 12 * MPS));
            assert_eq!(next(&mut stream, &mut bus).unwrap().id, id);
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
            assert!(next(stream, bus).is_err());
            let frame = next(stream, bus).unwrap();
            assert_eq!((frame.id, frame.data), (id, image(id, expected)));
        };
        // A truncated frame whose trailer arrives early.
        bus.send(&leader(2, expected as u32, 1));
        bus.send(&image(2, block));
        bus.send(&trailer(2, block));
        lost(&mut stream, &mut bus, 3);
        // A truncated frame that keeps its transfer count.
        bus.send(&leader(4, expected as u32, 1));
        bus.send(&image(4, 1000));
        bus.send(&[]);
        bus.send(&trailer(4, 1000));
        lost(&mut stream, &mut bus, 5);
        // A frame without its trailer.
        bus.send(&leader(6, expected as u32, 1));
        bus.send(&image(6, expected));
        lost(&mut stream, &mut bus, 7);
        // A transfer error in the middle of a frame.
        bus.send(&leader(8, expected as u32, 1));
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
        let mut stream = Stream::new(10 * MPS, MPS, MPS);
        let mut bus = Bus {
            refuse: 2,
            ..Bus::default()
        };
        stream.queue(&mut bus, 4, block);
        for id in 1..=3 {
            bus.refuse = 1;
            bus.frame(id, &image(id, 10 * MPS), block);
            let frame = next(&mut stream, &mut bus).unwrap();
            assert_eq!((frame.id, frame.data), (id, image(id, 10 * MPS)));
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
        bus.send(&leader(2, expected as u32, 1));
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
        assert_eq!(window(Some(0), 1), 8 * MIB);
        assert_eq!(window(Some(1000), 3), 8 * MIB);
        assert_eq!(window(None, 4), 8 * MIB);
        let submitted = |layout: Layout| layout.request * layout.transfers;
        // 4000x3000 Mono8 and 5472x3648 Mono8 at Linux's default limit.
        for expected in [12_000_000, 19_961_856] {
            let one = layout(expected, MPS, MPS, MPS, window(Some(16), 1)).unwrap();
            assert_eq!((one.block, one.transfers), (MIB, 8));
            let two = layout(expected, MPS, MPS, MPS, window(Some(16), 2)).unwrap();
            assert_eq!(submitted(two), 4 * MIB);
            let many = layout(expected, MPS, MPS, MPS, window(Some(16), 8)).unwrap();
            assert_eq!((many.block, submitted(many)), (MIB / 4, MIB));
        }
        // Small frames keep two frames of transfers submitted.
        let vga = layout(640 * 480, MPS, MPS, MPS, window(Some(16), 1)).unwrap();
        assert_eq!(
            (vga.block, vga.count, vga.tail, vga.transfers),
            (307_200, 1, 0, 6)
        );
        let tail = layout(2_304_000, MPS, MPS, MPS, MAX_WINDOW).unwrap();
        assert_eq!((tail.count, tail.tail), (2, 206_848));
    }
    #[test]
    fn stream_endpoint_reset_halts_it_first() {
        // Aravis: LIBUSB_ENDPOINT_OUT | LIBUSB_REQUEST_TYPE_STANDARD |
        // LIBUSB_RECIPIENT_ENDPOINT, SET_FEATURE, ENDPOINT_HALT, endpoint.
        let request = halt(0x81);
        let request_type = (request.control_type as u8) << 5 | request.recipient as u8;
        assert_eq!(request_type, 0x02);
        assert_eq!(
            (request.request, request.value, request.index, request.data),
            (0x03, 0, 0x81, &[][..])
        );
    }
    #[test]
    fn malformed_packets_fail_without_panics() {
        for n in 0..12 {
            assert!(ack(&vec![0; n], 1).is_err());
        }
        for n in 0..52 {
            assert!(parse_leader(&vec![0; n]).is_err());
        }
        assert_eq!(aligned(53, 1024).unwrap(), 1024);
        assert!(aligned(53, 3).is_err());
    }
}
