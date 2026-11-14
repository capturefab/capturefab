//! Explicit read-only multicast monitoring; the external controller owns acquisition.
use super::{
    Assembler, DEVICE_MODE, MAX_PAYLOAD, MAX_XML, PACKET_SIZE_MAX, SCPS_BIG_ENDIAN, STREAM_ADDRESS,
    STREAM_CAPABILITY, STREAM_PACKET_SIZE, STREAM_PORT, STREAM_SOURCE_PORT, TIMESTAMP_HIGH,
    TIMESTAMP_LOW, bootstrap::Interface,
};
use crate::types::{
    Backend, CameraAccess, CameraInfo, Categorized, ErrorKind, Frame, RegisterIo, Transport,
    TransportStats,
};
use anyhow::{Context as _, Result, ensure};
use serde_json::json;
use std::{
    net::{Ipv4Addr, SocketAddrV4},
    sync::Arc,
    time::{Duration, Instant},
};

mod control;
pub(crate) mod presence;
mod receive;
use control::ReadControl;

const PREFIX: &str = "gige-monitor://";
const MAX_FOREIGN_BYTES: usize = 64 * 1024;
const MAX_FOREIGN_PACKETS: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MonitorSpec {
    pub camera: Ipv4Addr,
    pub interface: Ipv4Addr,
    pub mac: [u8; 6],
}
pub(crate) fn is_selector(selector: &str) -> bool {
    selector.starts_with("gige-monitor:")
}
fn usage(message: &'static str) -> anyhow::Error {
    Categorized::error(ErrorKind::Usage, message)
}
fn incomplete(message: &'static str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, message)
}
fn denied() -> anyhow::Error {
    Categorized::error(
        ErrorKind::Unsupported,
        "read-only multicast monitor cannot change the camera",
    )
}
fn before(until: Instant) -> Result<Duration> {
    let left = until.saturating_duration_since(Instant::now());
    ensure!(
        !left.is_zero(),
        Categorized::error(
            ErrorKind::Timeout,
            "multicast monitor operation deadline expired"
        )
    );
    Ok(left)
}
const fn unicast(ip: Ipv4Addr) -> bool {
    !ip.is_unspecified() && !ip.is_multicast() && !ip.is_broadcast()
}
impl MonitorSpec {
    pub(crate) fn parse(selector: &str) -> Result<Self> {
        ensure!(
            selector.len() <= 256
                && selector.is_ascii()
                && !selector
                    .bytes()
                    .any(|b| b <= b' ' || b == 127 || b == b'%' || b == b'#'),
            usage("invalid multicast monitor selector characters or length")
        );
        let rest = selector
            .strip_prefix(PREFIX)
            .ok_or_else(|| usage("monitor selector must start gige-monitor://"))?;
        let (camera, query) = rest
            .split_once('?')
            .ok_or_else(|| usage("monitor selector needs interface and mac"))?;
        let camera: Ipv4Addr = camera.parse().map_err(|_| {
            usage("monitor camera must be numeric IPv4 without port, path or credentials")
        })?;
        let mut interface = None;
        let mut mac = None;
        for field in query.split('&') {
            let (key, value) = field
                .split_once('=')
                .ok_or_else(|| usage("invalid monitor selector field"))?;
            match key {
                "interface" if interface.is_none() => {
                    interface = Some(
                        value
                            .parse::<Ipv4Addr>()
                            .map_err(|_| usage("monitor interface must be numeric IPv4"))?,
                    );
                }
                "mac" if mac.is_none() => {
                    ensure!(
                        value.len() == 12 && value.bytes().all(|b| b.is_ascii_hexdigit()),
                        usage("monitor mac must contain exactly 12 hexadecimal digits")
                    );
                    let mut parsed = [0_u8; 6];
                    for (to, from) in parsed.iter_mut().zip(value.as_bytes().as_chunks::<2>().0) {
                        *to = u8::from_str_radix(std::str::from_utf8(from)?, 16)?;
                    }
                    mac = Some(parsed);
                }
                _ => return Err(usage("duplicate or unknown monitor selector field")),
            }
        }
        let interface = interface.ok_or_else(|| usage("monitor selector requires interface"))?;
        let mac = mac.ok_or_else(|| usage("monitor selector requires mac"))?;
        ensure!(
            unicast(camera) && unicast(interface) && mac != [0; 6] && mac[0] & 1 == 0,
            usage("monitor requires unicast camera/interface IPv4 and nonzero unicast MAC")
        );
        Ok(Self {
            camera,
            interface,
            mac,
        })
    }
    pub(crate) fn canonical(&self) -> String {
        format!(
            "{PREFIX}{}?interface={}&mac={}",
            self.camera,
            self.interface,
            crate::types::hex(self.mac)
        )
    }
    pub(crate) fn physical_id(&self) -> String {
        format!("gige:{}", crate::types::hex(self.mac))
    }
    fn resolve(&self) -> Result<Interface> {
        ensure!(
            unicast(self.camera)
                && unicast(self.interface)
                && self.mac != [0; 6]
                && self.mac[0] & 1 == 0,
            usage("invalid typed monitor specification")
        );
        let mut found = None;
        for entry in if_addrs::get_if_addrs()? {
            let if_addrs::IfAddr::V4(address) = entry.addr else {
                continue;
            };
            if address.ip != self.interface {
                continue;
            }
            let candidate = Interface {
                name: entry.name,
                index: entry.index.filter(|i| *i != 0).ok_or_else(|| {
                    Categorized::error(
                        ErrorKind::Unsupported,
                        "monitor interface has no usable OS index",
                    )
                })?,
                address: address.ip,
                netmask: address.netmask,
            };
            ensure!(
                found.as_ref().is_none_or(|old| old == &candidate),
                usage("monitor interface address is ambiguous")
            );
            found = Some(candidate);
        }
        let interface = found
            .ok_or_else(|| usage("monitor interface must identify a current local IPv4 address"))?;
        let mask = u32::from(interface.netmask);
        ensure!(
            mask != 0 && (!mask).wrapping_add(1).is_power_of_two(),
            usage("monitor interface has an invalid subnet mask")
        );
        ensure!(
            u32::from(self.camera) & mask == u32::from(interface.address) & mask,
            usage("monitor camera must be on the selected interface subnet")
        );
        let host = u32::from(self.camera) & !mask;
        ensure!(
            !mask <= 1 || (host != 0 && host != !mask),
            usage("monitor camera cannot be the subnet network or broadcast address")
        );
        Ok(interface)
    }
}

fn identity(spec: &MonitorSpec, control: &mut ReadControl, until: Instant) -> Result<CameraInfo> {
    let raw = control.memory(0, 0xf8, until)?;
    let mut info = super::discovery_info(&raw, spec.camera)?;
    ensure!(
        info.id == spec.physical_id(),
        incomplete("monitor bootstrap MAC differs from the explicitly selected camera")
    );
    info.id = spec.canonical();
    before(until)?;
    Ok(info)
}
pub(crate) fn identify(spec: &MonitorSpec, until: Instant) -> Result<CameraInfo> {
    before(until)?;
    let _presence = presence::acquire_monitor(&spec.physical_id())?;
    let interface = spec.resolve()?;
    before(until)?;
    let mut control = ReadControl::new(spec, &interface, until)?;
    identity(spec, &mut control, until)
}

#[derive(Clone)]
struct Route {
    interface: Interface,
    group: Ipv4Addr,
    port: u16,
    source: SocketAddrV4,
    scps: u32,
    payload: usize,
    big_endian: bool,
    mono16_big_endian: bool,
    frequency: u64,
}
fn xml(control: &mut ReadControl, until: Instant) -> Result<String> {
    let mut last = None;
    for address in [0x200, 0x400] {
        before(until)?;
        let result = (|| {
            let bytes = control.memory(address, 512, until)?;
            let url = super::fixed_text(&bytes);
            let (name, raw) = if url
                .split_once(':')
                .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("local"))
            {
                let local = super::parse_local_url(&url)?;
                (
                    local.name,
                    control.memory(local.address, local.length, until)?,
                )
            } else {
                super::xml_http::get_until(&url, until)?
            };
            ensure!(
                raw.len() <= MAX_XML,
                incomplete("monitor XML exceeds its bounded extent")
            );
            let decoded = super::decode_xml(&name, raw)?;
            before(until)?;
            Ok::<_, anyhow::Error>(decoded)
        })();
        match result {
            Ok(xml) => return Ok(xml),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| incomplete("monitor camera has no usable XML URL")))
}
pub(crate) fn open(
    spec: &MonitorSpec,
    expected: &CameraInfo,
    until: Instant,
) -> Result<Box<dyn Backend>> {
    before(until)?;
    ensure!(
        expected.transport == Transport::GigE
            && expected.id == spec.canonical()
            && expected
                .address
                .as_deref()
                .and_then(|ip| ip.parse::<Ipv4Addr>().ok())
                == Some(spec.camera),
        usage("monitor identity does not match the explicit selector")
    );
    let presence = presence::acquire_monitor(&spec.physical_id())?;
    let interface = spec.resolve()?;
    before(until)?;
    let mut control = ReadControl::new(spec, &interface, until)?;
    let observed = identity(spec, &mut control, until)?;
    ensure!(
        observed.vendor == expected.vendor
            && observed.model == expected.model
            && observed.serial == expected.serial,
        incomplete("monitor camera identity changed before open")
    );
    control.register(DEVICE_MODE, until)?;
    let group = Ipv4Addr::from(control.register(STREAM_ADDRESS, until)?);
    let port = u16::try_from(control.register(STREAM_PORT, until)? & 0xffff)?;
    let capability = control.register(STREAM_CAPABILITY, until)?;
    ensure!(
        capability & 0x8000_0000 != 0,
        Categorized::error(
            ErrorKind::Unsupported,
            "monitor requires an advertised stream source port"
        )
    );
    let source_port = u16::try_from(control.register(STREAM_SOURCE_PORT, until)? & 0xffff)?;
    let flags = control.register(STREAM_PACKET_SIZE, until)?;
    let scps = flags & 0xffff;
    ensure!(
        group.is_multicast()
            && port != 0
            && source_port != 0
            && (576..=PACKET_SIZE_MAX).contains(&scps),
        Categorized::error(
            ErrorKind::Unsupported,
            "monitor requires an existing multicast group, destination/source ports and supported packet size"
        )
    );
    let text = xml(&mut control, until)?;
    control.legacy = super::legacy_registers(&text);
    let nodes = crate::genicam::NodeMap::parse(&text)?;
    let payload = nodes
        .get(&mut control, "PayloadSize")?
        .as_u64()
        .and_then(|p| usize::try_from(p).ok())
        .filter(|p| *p > 0 && *p <= MAX_PAYLOAD)
        .ok_or_else(|| incomplete("monitor PayloadSize must be 1..128 MiB"))?;
    let frequency = match (
        control.register(TIMESTAMP_HIGH, until),
        control.register(TIMESTAMP_LOW, until),
    ) {
        (Ok(high), Ok(low)) => (u64::from(high) << 32) | u64::from(low),
        _ => 0,
    };
    before(until)?;
    let route = Route {
        interface,
        group,
        port,
        source: SocketAddrV4::new(spec.camera, source_port),
        scps,
        payload,
        big_endian: flags & SCPS_BIG_ENDIAN != 0,
        mono16_big_endian: super::big_endian_mono16(&observed.vendor, &observed.model),
        frequency,
    };
    let receiver = receive::Receiver::open(&route, until)?;
    let assembler = assembler(&route, false, receiver.buffer_size());
    before(until)?;
    Ok(Box::new(Monitor {
        control,
        _presence: presence,
        route,
        xml: text,
        receiver: Some(receiver),
        assembler,
        active: false,
        multipart: false,
        failed: None,
        buffer: vec![0; 65_536],
    }))
}
fn assembler(route: &Route, multipart: bool, buffer: Option<u64>) -> Assembler {
    let mut assembler = Assembler::new(route.payload, route.scps as usize, route.frequency);
    assembler.big_endian = route.big_endian;
    assembler.big_endian_mono16 = route.mono16_big_endian;
    assembler.multipart_projection = multipart;
    assembler.stats.packet_size = Some(route.scps);
    assembler.stats.receive_buffer_bytes = buffer;
    assembler.stats.notes.push("read-only multicast monitor; externally stable configuration required; no ownership or synchronization proof".into());
    assembler
}
#[derive(Default)]
struct ForeignWork {
    packets: usize,
    bytes: usize,
}
impl ForeignWork {
    fn selected(&mut self, packet: receive::Datagram, route: &Route) -> Result<bool> {
        if packet.source == route.source
            && packet.destination == route.group
            && packet.interface_index == route.interface.index
        {
            return Ok(true);
        }
        self.packets = self
            .packets
            .checked_add(1)
            .ok_or_else(|| incomplete("monitor foreign packet count overflow"))?;
        self.bytes = self
            .bytes
            .checked_add(packet.bytes)
            .ok_or_else(|| incomplete("monitor foreign byte count overflow"))?;
        ensure!(
            self.packets <= MAX_FOREIGN_PACKETS && self.bytes <= MAX_FOREIGN_BYTES,
            incomplete("monitor foreign datagrams exceeded bounded allowance")
        );
        Ok(false)
    }
}
fn decode_error(error: anyhow::Error) -> anyhow::Error {
    let classified = crate::types::classify(&error);
    if classified.kind == ErrorKind::OperationFailed {
        // This must be a real error node: an anyhow context's display value
        // is not exposed by the standard source chain used by classify.
        let mut category = Categorized::new(
            ErrorKind::Incomplete,
            format!("malformed monitored GVSP frame: {error:#}"),
        );
        category.hint = classified.hint;
        category.details = classified.details;
        category.into()
    } else {
        error
    }
}

fn retired_cause(error: anyhow::Error) -> Arc<anyhow::Error> {
    Arc::new(crate::types::with_details(
        error,
        json!({"monitor_connection":{"state":"retired","reason":"receive_failed"}}),
    ))
}
fn publish(
    frame: Frame,
    ready: &mut std::collections::VecDeque<Frame>,
    until: Instant,
) -> Result<Frame> {
    if let Err(error) = before(until) {
        ready.push_front(frame);
        return Err(error);
    }
    Ok(frame)
}
#[derive(Debug)]
struct SharedFailure(Arc<anyhow::Error>);
impl std::fmt::Display for SharedFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self.0.as_ref(), f)
    }
}
impl std::error::Error for SharedFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}
struct Monitor {
    control: ReadControl,
    _presence: presence::Presence,
    route: Route,
    xml: String,
    receiver: Option<receive::Receiver>,
    assembler: Assembler,
    active: bool,
    multipart: bool,
    failed: Option<Arc<anyhow::Error>>,
    buffer: Vec<u8>,
}
impl Monitor {
    fn failure(&self) -> Option<anyhow::Error> {
        self.failed
            .as_ref()
            .map(|e| SharedFailure(e.clone()).into())
    }
    fn remember(&mut self, error: anyhow::Error) -> anyhow::Error {
        if self.failed.is_none() {
            self.failed = Some(retired_cause(error));
            self.receiver = None;
        }
        self.failure()
            .unwrap_or_else(|| incomplete("monitor terminal cause missing"))
    }
    fn take(&mut self, until: Instant) -> Result<Frame> {
        let mut foreign = ForeignWork::default();
        loop {
            before(until)?;
            if let Some(frame) = self.assembler.ready.pop_front() {
                return Ok(frame);
            }
            if let Some(error) = self.failure() {
                return Err(error);
            }
            self.assembler.poll(Instant::now());
            let socket = self
                .receiver
                .as_ref()
                .context("monitor receiver is stopped")?;
            let packet = match socket.receive(&mut self.buffer) {
                Ok(Some(packet)) => packet,
                Ok(None) => {
                    std::thread::sleep(before(until)?.min(Duration::from_millis(1)));
                    continue;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                    return Err(incomplete("malformed monitor datagram evidence").context(error));
                }
                Err(error) => return Err(error.into()),
            };
            before(until)?;
            if !foreign.selected(packet, &self.route)? {
                continue;
            }
            let raw = self
                .buffer
                .get(..packet.bytes)
                .ok_or_else(|| incomplete("monitor datagram exceeds receive storage"))?;
            let parsed =
                super::parse_gvsp(raw, crate::copies::Site::GvspAssemble).map_err(decode_error)?;
            if let Some(frame) = self
                .assembler
                .take(parsed, Instant::now())
                .map_err(decode_error)?
            {
                return publish(frame, &mut self.assembler.ready, until);
            }
        }
    }
}
impl RegisterIo for Monitor {
    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        if let Some(error) = self.failure() {
            return Err(error);
        }
        self.control.read_memory(address, length)
    }
    fn read_memory_until(
        &mut self,
        address: u64,
        length: usize,
        until: Instant,
    ) -> Result<Vec<u8>> {
        if let Some(error) = self.failure() {
            return Err(error);
        }
        self.control.read_memory_until(address, length, until)
    }
    fn write_memory(&mut self, _address: u64, _data: &[u8]) -> Result<()> {
        Err(denied())
    }
    fn write_memory_until(&mut self, _address: u64, _data: &[u8], _until: Instant) -> Result<()> {
        Err(denied())
    }
}
impl Backend for Monitor {
    fn access(&self) -> CameraAccess {
        CameraAccess::ReadOnlyMonitor
    }
    fn finish_initialization(&mut self) -> Result<()> {
        if let Some(until) = self.control.initial {
            before(until)?;
            self.control.initial = None;
        }
        Ok(())
    }
    fn xml(&mut self) -> Result<String> {
        if let Some(until) = self.control.initial {
            before(until)?;
        }
        Ok(self.xml.clone())
    }
    fn stream_payload_size(&mut self) -> Result<Option<usize>> {
        Ok(Some(self.route.payload))
    }
    fn enable_multipart_projection(&mut self) -> Result<()> {
        self.multipart = true;
        self.assembler.multipart_projection = true;
        Ok(())
    }
    fn start(&mut self, payload_size: usize) -> Result<()> {
        if let Some(error) = self.failure() {
            return Err(error);
        }
        ensure!(
            payload_size == self.route.payload,
            incomplete("monitor payload differs from its immutable setup snapshot")
        );
        if self.active {
            return Ok(());
        }
        if self.receiver.is_none() {
            self.receiver = Some(receive::Receiver::open(
                &self.route,
                Instant::now() + self.control.timeout,
            )?);
        }
        let stats = self.assembler.stats.clone();
        self.assembler = assembler(
            &self.route,
            self.multipart,
            self.receiver
                .as_ref()
                .and_then(receive::Receiver::buffer_size),
        );
        self.assembler.stats = stats;
        self.active = true;
        Ok(())
    }
    fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
        if self.assembler.ready.is_empty()
            && let Some(error) = self.failure()
        {
            return Err(error);
        }
        ensure!(
            self.active,
            incomplete("monitor host receiver has not started")
        );
        let until = Instant::now()
            .checked_add(timeout)
            .context("monitor frame timeout overflow")?;
        match self.take(until) {
            Ok(frame) => Ok(frame),
            Err(error) if crate::types::classify(&error).kind == ErrorKind::Timeout => Err(error),
            Err(error) => Err(self.remember(error)),
        }
    }
    fn stop(&mut self) -> Result<()> {
        self.active = false;
        if let Some(receiver) = self.receiver.take() {
            receiver.leave()?;
        }
        Ok(())
    }
    fn terminal_error(&self) -> Option<anyhow::Error> {
        if self.assembler.ready.is_empty() {
            self.failure()
        } else {
            None
        }
    }
    fn stop_after_frame_error(&self) -> bool {
        self.failed.is_some()
    }
    fn stats(&self) -> Option<TransportStats> {
        Some(self.assembler.stats.clone())
    }
    fn buffered_frames(&mut self) -> u32 {
        u32::try_from(self.assembler.ready.len()).unwrap_or(u32::MAX)
    }
    fn optimize_network(&mut self, _payload_size: usize) -> Result<crate::network::NetworkTuning> {
        Err(denied())
    }
    fn restore_settings(&mut self, _settings: &serde_json::Value) -> Result<()> {
        Err(denied())
    }
}

// The reservation must outlive socket close even on constructor/worker error.
impl Drop for Monitor {
    fn drop(&mut self) {
        if let Some(receiver) = self.receiver.take() {
            let _ = receiver.leave();
        }
    }
}

#[cfg(test)]
mod tests;
