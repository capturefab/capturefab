//! USB3 Vision transport using native OS USB APIs through nusb (no libusb).
//! UVCP/UVSP wire fields and bootstrap register addresses follow USB3 Vision.
use crate::{
    genicam::decode_xml,
    types::{Backend, CameraInfo, Frame, RegisterIo, Transport},
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use nusb::{
    DeviceInfo, Endpoint, Interface, MaybeFuture,
    descriptors::TransferType,
    transfer::{Buffer, Bulk, In, Out},
};
use std::{
    collections::VecDeque,
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
    let device = device_info
        .open()
        .wait()
        .context("cannot open USB camera; check OS USB permissions and driver binding")?;
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
    let control_interface = device.claim_interface(control.number).wait().context("cannot claim camera control interface; close other camera software and check USB permissions/driver")?;
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
            .context("cannot claim camera stream interface")?
    };
    if data.alternate != 0 {
        data_interface.set_alt_setting(data.alternate).wait()?;
    }
    let mut backend = UsbBackend {
        control_in: control_interface.endpoint::<Bulk, In>(control.input)?,
        control_out: control_interface.endpoint::<Bulk, Out>(control.output.unwrap())?,
        stream_in: data_interface.endpoint::<Bulk, In>(data.input)?,
        _interfaces: vec![control_interface, data_interface],
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
    _interfaces: Vec<Interface>,
    timeout: Duration,
    id: u16,
    max_command: usize,
    max_ack: usize,
    sirm: u64,
    stream: Option<Stream>,
}
#[derive(Debug, Clone, Copy)]
enum Part {
    Leader,
    Payload,
    Trailer,
}
#[derive(Debug, Clone, Copy)]
struct Request {
    part: Part,
    size: usize,
}
struct Stream {
    expected: usize,
    schedule: Vec<Request>,
    pending: VecDeque<Request>,
    frame: Option<Frame>,
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
fn ack(packet: &[u8], id: u16) -> Result<(u16, &[u8])> {
    ensure!(
        le32(packet, 0)? == MAGIC,
        "invalid UVCP acknowledgement magic"
    );
    ensure!(le16(packet, 10)? == id, "unexpected UVCP request ID");
    let status = le16(packet, 4)?;
    ensure!(
        status == 0,
        "camera rejected USB3 Vision command with status 0x{status:04x}"
    );
    let length = le16(packet, 8)? as usize;
    ensure!(
        packet.len() >= 12 + length,
        "truncated UVCP acknowledgement payload"
    );
    Ok((le16(packet, 6)?, &packet[12..12 + length]))
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
    fn queue(&mut self, requests: &[Request]) {
        for request in requests {
            let buffer = self.stream_in.allocate(request.size);
            self.stream_in.submit(buffer);
        }
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
            ensure!(
                response.len() == 4 && le16(&response, 2)? as usize == chunk.len(),
                "camera returned invalid UVCP write acknowledgement"
            );
        }
        Ok(())
    }
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
        let mut candidates = Vec::new();
        for entry in entries.as_chunks::<64>().0 {
            let schema = (le32(entry, 4)? >> 10) & 31;
            if schema <= 1 {
                candidates.push((le32(entry, 0)?, le64(entry, 8)?, le64(entry, 16)?));
            }
        }
        candidates.sort_by_key(|e| std::cmp::Reverse(e.0));
        let mut last_error = anyhow!("USB manifest contains no raw or ZIP GenICam XML entry");
        for (_, address, size) in candidates {
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
        let info = self.u32(self.sirm)?;
        let alignment = 1usize
            .checked_shl(info >> 24)
            .context("invalid USB camera stream alignment")?
            .max(self.stream_in.max_packet_size());
        ensure!(
            alignment.is_power_of_two() && alignment <= 1024 * 1024,
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
            leader <= 1024 * 1024 && trailer <= 1024 * 1024,
            "USB stream header transfer is too large"
        );
        let block = aligned(expected.min(1024 * 1024), alignment)?;
        let count = expected / block;
        let tail = if !expected.is_multiple_of(block) {
            aligned(expected % block, alignment)?
        } else {
            0
        };
        self.put32(0x18, leader)?;
        self.put32(0x2c, trailer)?;
        self.put32(0x1c, block)?;
        self.put32(0x20, count)?;
        self.put32(0x24, tail)?;
        self.put32(0x28, 0)?;
        self.stream_in
            .clear_halt()
            .wait()
            .context("cannot reset USB stream endpoint")?;
        let mut schedule = vec![Request {
            part: Part::Leader,
            size: leader,
        }];
        schedule.extend((0..count).map(|_| Request {
            part: Part::Payload,
            size: block,
        }));
        if tail > 0 {
            schedule.push(Request {
                part: Part::Payload,
                size: tail,
            });
        }
        schedule.push(Request {
            part: Part::Trailer,
            size: trailer,
        });
        // Queue two full frame requests before AcquisitionStart to keep the host
        // controller supplied with buffers while the consumer processes a frame.
        self.queue(&schedule);
        self.queue(&schedule);
        let pending = schedule.iter().chain(schedule.iter()).copied().collect();
        self.stream = Some(Stream {
            expected,
            schedule,
            pending,
            frame: None,
        });
        if let Err(error) = self.put32(SIRM_CONTROL, 1) {
            let _ = self.stop();
            return Err(error);
        }
        Ok(())
    }
    fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
        ensure!(self.stream.is_some(), "USB acquisition has not started");
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(!remaining.is_zero(), "USB frame capture timed out");
            let completed = self
                .stream_in
                .wait_next_complete(remaining)
                .context("USB frame capture timed out")?;
            let bytes = completed
                .into_result()
                .context("USB stream transfer failed")?;
            let stream = self.stream.as_mut().unwrap();
            let request = stream
                .pending
                .pop_front()
                .context("USB stream transfer queue exhausted")?;
            match request.part {
                Part::Leader => {
                    ensure!(
                        stream.frame.is_none(),
                        "unexpected USB leader while receiving an image"
                    );
                    let mut frame = parse_leader(&bytes)?;
                    frame.data.reserve(stream.expected);
                    stream.frame = Some(frame);
                }
                Part::Payload => {
                    let frame = stream
                        .frame
                        .as_mut()
                        .context("USB payload arrived before image leader")?;
                    let remaining = stream.expected - frame.data.len();
                    let valid = remaining.min(request.size);
                    ensure!(
                        bytes.len() >= valid && bytes.len() <= request.size,
                        "short USB image payload transfer"
                    );
                    frame.data.extend_from_slice(&bytes[..valid]);
                }
                Part::Trailer => {
                    let frame = stream
                        .frame
                        .take()
                        .context("USB trailer arrived before image leader")?;
                    validate_trailer(&bytes, &frame, stream.expected)?;
                    let schedule = stream.schedule.clone();
                    stream.pending.extend(schedule.iter().copied());
                    self.queue(&schedule);
                    return Ok(frame);
                }
            }
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
        response[4] = 1;
        assert!(ack(&response, 7).is_err());
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
