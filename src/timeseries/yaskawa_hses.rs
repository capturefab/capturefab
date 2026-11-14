//! Finite read-only Yaskawa HSES polling under an explicit UDP reply-horizon policy.
use super::{Reading, Source};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    sync::Arc,
    time::{Duration, Instant},
};

mod packet;
#[cfg(test)]
mod tests;

const MAX_RECEIVES: usize = 1024;
const MAX_RETIRED_PORTS: usize = 1024;
const MAX_BATCH: usize = (1 << 20) - 16 * 1024;
const MAX_RAW: usize = 1 << 20;

#[derive(Debug, Clone, Copy, Deserialize)]
enum Profile {
    #[serde(rename = "yrc1000_hw1483358_rev12")]
    Yrc1000Hw1483358Rev12,
}
impl Profile {
    const fn name(self) -> &'static str {
        match self {
            Self::Yrc1000Hw1483358Rev12 => "yrc1000_hw1483358_rev12",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Resource {
    Status,
    BasePosition,
    Byte,
    Integer,
    DoubleInteger,
    RealRaw,
    String32,
}
impl Resource {
    const fn command(self) -> u16 {
        match self {
            Self::Status => 0x72,
            Self::BasePosition => 0x75,
            Self::Byte => 0x7a,
            Self::Integer => 0x7b,
            Self::DoubleInteger => 0x7c,
            Self::RealRaw => 0x7d,
            Self::String32 => 0x8c,
        }
    }
    const fn bytes(self) -> usize {
        match self {
            Self::Status => 8,
            Self::BasePosition => 52,
            Self::Byte => 1,
            Self::Integer => 2,
            Self::DoubleInteger | Self::RealRaw => 4,
            Self::String32 => 32,
        }
    }
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    signal: String,
    resource: Resource,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    index: Option<u16>,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    group: Option<u8>,
}
impl Selection {
    fn instance(&self) -> Result<u16> {
        match self.resource {
            Resource::Status if self.index.is_none() && self.group.is_none() => Ok(1),
            Resource::BasePosition if self.index.is_none() => self
                .group
                .filter(|group| (1..=8).contains(group))
                .map(|group| 100 + u16::from(group))
                .ok_or_else(|| Usage("HSES base_position requires robot group1..8".into()).into()),
            Resource::Byte
            | Resource::Integer
            | Resource::DoubleInteger
            | Resource::RealRaw
            | Resource::String32
                if self.group.is_none() =>
            {
                self.index.ok_or_else(|| {
                    Usage("HSES variable reads require an explicit u16 index".into()).into()
                })
            }
            _ => Err(Usage(
                "HSES index/group fields do not match the selected read resource".into(),
            )
            .into()),
        }
    }
    fn request(&self, id: u8) -> Result<packet::Request> {
        let all = matches!(self.resource, Resource::Status | Resource::BasePosition);
        Ok(packet::Request {
            id,
            command: self.resource.command(),
            instance: self.instance()?,
            attribute: u8::from(!all),
            service: if all { 1 } else { 0x0e },
            bytes: self.resource.bytes(),
        })
    }
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    profile: Profile,
    address: SocketAddrV4,
    #[serde(default)]
    bind: Option<SocketAddrV4>,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    reply_horizon_ms: u64,
    reads: Vec<Selection>,
}
impl Config {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.address.port() == 0
            || self.address.ip().is_unspecified()
            || self.address.ip().is_multicast()
            || *self.address.ip() == Ipv4Addr::BROADCAST
        {
            return Err(Usage(
                "HSES address must be an explicit numeric unicast IPv4 endpoint with nonzero port"
                    .into(),
            )
            .into());
        }
        if self
            .bind
            .is_some_and(|bind| bind.ip().is_multicast() || *bind.ip() == Ipv4Addr::BROADCAST)
        {
            return Err(
                Usage("HSES bind must be an IPv4 local or unspecified address".into()).into(),
            );
        }
        if !(1..=30_000).contains(&self.reply_horizon_ms) {
            return Err(Usage("HSES requires explicit reply_horizon_ms1..30000 as an operator network-delay assumption".into()).into());
        }
        if self.reads.is_empty() || self.reads.len() > 32 {
            return Err(Usage("HSES requires1..32 named reads".into()).into());
        }
        let mut names = BTreeSet::new();
        for read in &self.reads {
            super::validate_name(&read.signal, "HSES signal")?;
            if !names.insert(&read.signal) {
                return Err(Usage("HSES signal names must be unique".into()).into());
            }
            read.instance()?;
        }
        Ok(())
    }
    pub(crate) fn validate_for(&self, timeout_ms: u64) -> Result<()> {
        self.validate()?;
        if timeout_ms == 0 || self.reply_horizon_ms < timeout_ms {
            return Err(
                Usage("HSES reply_horizon_ms must be at least source timeout_ms".into()).into(),
            );
        }
        Ok(())
    }
}
pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    config.validate()?;
    let bind = config
        .bind
        .unwrap_or_else(|| SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));
    Ok(Box::new(Client {
        profile: config.profile,
        reads: config.reads.clone(),
        datagrams: Datagrams {
            address: config.address,
            bind,
            horizon: Duration::from_millis(config.reply_horizon_ms),
            socket: None,
            id: 0,
            binding: if bind.port() == 0 {
                Binding::Ephemeral {
                    rotate: false,
                    retired: BTreeMap::new(),
                }
            } else {
                Binding::Fixed(Box::new([None; 256]))
            },
        },
        failure: None,
        observed: None,
        ready: false,
    }))
}
struct Client {
    profile: Profile,
    reads: Vec<Selection>,
    datagrams: Datagrams,
    failure: Option<Arc<anyhow::Error>>,
    observed: Option<u64>,
    ready: bool,
}
#[derive(Debug)]
struct Retired(Arc<anyhow::Error>);
impl std::fmt::Display for Retired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HSES source retired after its failed poll")
    }
}
impl std::error::Error for Retired {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}
fn incomplete(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, format!("HSES: {message}"))
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "HSES original poll deadline expired",
        ));
    }
    Ok(left)
}
fn clock() -> Result<u64> {
    crate::host::monotonic_ns().ok_or_else(|| {
        Categorized::error(
            ErrorKind::Unavailable,
            "HSES host observation clock is unavailable",
        )
    })
}
fn io_error(error: &io::Error) -> anyhow::Error {
    Categorized::error(
        match error.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
            io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
            io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted => ErrorKind::Disconnected,
            _ => ErrorKind::Unavailable,
        },
        format!("HSES UDP I/O: {error}"),
    )
}
fn socket(bind: SocketAddrV4, address: SocketAddrV4, deadline: Instant) -> Result<UdpSocket> {
    remaining(deadline)?;
    let socket = UdpSocket::bind(bind).map_err(|error| io_error(&error))?;
    socket.connect(address).map_err(|error| io_error(&error))?;
    remaining(deadline)?;
    Ok(socket)
}
fn port(socket: &UdpSocket) -> Result<u16> {
    Ok(socket
        .local_addr()
        .map_err(|error| io_error(&error))?
        .port())
}
struct Datagrams {
    address: SocketAddrV4,
    bind: SocketAddrV4,
    horizon: Duration,
    socket: Option<UdpSocket>,
    id: u8,
    binding: Binding,
}
enum Binding {
    Ephemeral {
        rotate: bool,
        retired: BTreeMap<u16, Instant>,
    },
    Fixed(Box<[Option<Instant>; 256]>),
}
#[derive(Default)]
struct Work {
    received: usize,
    ignored: usize,
    raw: usize,
}
impl Work {
    fn admission(&self, deadline: Instant) -> Result<()> {
        remaining(deadline)?;
        if self.received >= MAX_RECEIVES {
            return Err(incomplete("poll reached its1024-datagram work cap"));
        }
        Ok(())
    }
    fn received(&mut self, bytes: usize) -> Result<()> {
        self.received += 1;
        self.raw = self
            .raw
            .checked_add(bytes)
            .filter(|sum| *sum <= MAX_RAW)
            .ok_or_else(|| incomplete("poll raw UDP work exceeds1MiB"))?;
        Ok(())
    }
}
struct Received {
    raw: Vec<u8>,
    value: Value,
    request: packet::Request,
    source_port: u16,
    start: u64,
    end: u64,
}
impl Datagrams {
    fn prepare(&mut self, deadline: Instant) -> Result<()> {
        let Some(current) = self.socket.as_ref() else {
            self.socket = Some(socket(self.bind, self.address, deadline)?);
            return Ok(());
        };
        let Binding::Ephemeral { rotate, retired } = &mut self.binding else {
            return Ok(());
        };
        if !*rotate {
            return Ok(());
        }
        let previous = port(current)?;
        let now = Instant::now();
        retired.retain(|_, until| *until > now);
        if retired.len() >= MAX_RETIRED_PORTS {
            return Err(incomplete(
                "retired UDP source-port bound reached within the operator horizon",
            ));
        }
        // Keep the previous socket open while choosing its replacement. This
        // protects only this source epoch and the declared delay horizon.
        for _ in 0..16 {
            let replacement = socket(self.bind, self.address, deadline)?;
            let next = port(&replacement)?;
            if next == previous || retired.contains_key(&next) {
                continue;
            }
            retired.insert(previous, Instant::now() + self.horizon);
            self.socket = Some(replacement);
            self.id = 0;
            *rotate = false;
            return Ok(());
        }
        Err(Categorized::error(
            ErrorKind::Unavailable,
            "HSES could not obtain a fresh source port within16 attempts",
        ))
    }
    fn drain(socket: &UdpSocket, deadline: Instant, work: &mut Work) -> Result<()> {
        socket
            .set_nonblocking(true)
            .map_err(|error| io_error(&error))?;
        let result = (|| {
            let mut data = vec![0; 65_535];
            loop {
                work.admission(deadline)?;
                match socket.recv(&mut data) {
                    Ok(bytes) => {
                        work.received(bytes)?;
                        work.ignored += 1;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                    Err(error) => return Err(io_error(&error)),
                }
            }
        })();
        let restored = socket
            .set_nonblocking(false)
            .map_err(|error| io_error(&error));
        result.and(restored)
    }
    fn read(
        &mut self,
        selection: &Selection,
        deadline: Instant,
        work: &mut Work,
    ) -> Result<Received> {
        let start = clock()?;
        self.prepare(deadline)?;
        let socket = self
            .socket
            .as_ref()
            .ok_or_else(|| incomplete("UDP owner absent"))?;
        let id = self.id;
        let request = selection.request(id)?;
        if let Binding::Fixed(reusable) = &self.binding
            && let Some(at) = reusable[usize::from(id)]
        {
            if at >= deadline {
                return Err(Categorized::error(
                    ErrorKind::Timeout,
                    "HSES request ID cannot be reused within its original deadline and operator horizon",
                ));
            }
            if let Some(wait) = at.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            Self::drain(socket, deadline, work)?;
        }
        remaining(deadline)?;
        socket
            .set_write_timeout(Some(remaining(deadline)?))
            .map_err(|error| io_error(&error))?;
        let sent = socket.send(&request.encode());
        self.id = id.wrapping_add(1);
        match &mut self.binding {
            Binding::Ephemeral { rotate, .. } => *rotate = self.id == 0,
            Binding::Fixed(reusable) => {
                reusable[usize::from(id)] = Some(Instant::now() + self.horizon);
            }
        }
        let sent = sent.map_err(|error| io_error(&error))?;
        remaining(deadline)?;
        if sent != 32 {
            return Err(incomplete("short read-request datagram send; no retry"));
        }
        let source_port = port(socket)?;
        let mut data = vec![0; 65_535];
        loop {
            work.admission(deadline)?;
            socket
                .set_read_timeout(Some(remaining(deadline)?))
                .map_err(|error| io_error(&error))?;
            let (bytes, peer) = socket
                .recv_from(&mut data)
                .map_err(|error| io_error(&error))?;
            let end = clock()?;
            work.received(bytes)?;
            remaining(deadline)?;
            if peer != SocketAddr::V4(self.address)
                || data
                    .get(11)
                    .filter(|_| bytes >= 12)
                    .is_some_and(|value| *value != id)
            {
                work.ignored += 1;
                continue;
            }
            let raw = &data[..bytes];
            let reply = packet::reply(request, raw)?;
            let value = packet::value(selection.resource, request.instance, reply.body)?;
            remaining(deadline)?;
            if end < start {
                return Err(incomplete("host monotonic observation clock reversed"));
            }
            return Ok(Received {
                raw: raw.to_vec(),
                value,
                request,
                source_port,
                start,
                end,
            });
        }
    }
}
impl Client {
    fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        let mut work = Work::default();
        let mut size = Size(2);
        let mut readings = Vec::with_capacity(self.reads.len());
        let mut observed = None;
        for selection in &self.reads {
            // A closed read has at most84 raw bytes, eight coordinates and
            // fixed metadata. Reserve8KiB before retaining/decoding it; charge
            // the actual escaped Reading below before retaining the batch.
            if size.0.checked_add(8192).is_none_or(|size| size > MAX_BATCH) {
                return Err(incomplete("Reading reservation exceeds the batch budget"));
            }
            let received = self.datagrams.read(selection, deadline, &mut work)?;
            let response = packet::reply(received.request, &received.raw)?;
            let value = json!({"kind":"yaskawa_hses","profile":self.profile.name(),"resource":selection.resource,
                "correlation":{"scope":"source_epoch_only","basis":"operator_declared_reply_horizon","reply_horizon_ms":self.datagrams.horizon.as_millis(),"source_port":received.source_port},
                "request":{"request_id":received.request.id,"command":received.request.command,"instance":received.request.instance,"attribute":received.request.attribute,"service":received.request.service},
                "response":{"service_raw":response.service,"status_raw":response.status,"added_status_words":response.added_words,"added_status_raw":response.added,"added_status_bytes_hex":crate::types::hex(response.added_bytes)},
                "raw_response_hex":crate::types::hex(&received.raw),"host_window_basis":"request_admission_through_datagram_received_including_quarantine","host_window_ns":{"start":received.start,"end":received.end},"data":received.value});
            let reading = Reading {
                signal: selection.signal.clone(),
                value,
                source_time: None,
            };
            serde_json::to_writer(&mut size, &reading).map_err(|_| {
                incomplete("serialized Reading batch exceeds1MiB with16KiB wrapper reserve")
            })?;
            size.write_comma()?;
            observed = Some(received.end);
            readings.push(reading);
        }
        serde_json::to_writer(Size(0), &readings)
            .map_err(|_| incomplete("serialized Reading batch limit"))?;
        remaining(deadline)?;
        self.observed = observed;
        self.ready = true;
        Ok(readings)
    }
}
impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        if let Some(error) = &self.failure {
            return Err(Retired(error.clone()).into());
        }
        let started = Instant::now();
        let result = (|| {
            let deadline = started
                .checked_add(timeout)
                .ok_or_else(|| Usage("HSES timeout overflows the host clock".into()))?;
            remaining(deadline)?;
            if timeout > self.datagrams.horizon {
                return Err(Usage(
                    "HSES operator reply horizon is shorter than the admitted read timeout".into(),
                )
                .into());
            }
            self.poll(deadline)
        })();
        result.map_err(|error| {
            let error = Arc::new(error);
            self.datagrams.socket = None;
            self.failure = Some(error.clone());
            Retired(error).into()
        })
    }
    fn observed_ns(&self) -> Option<u64> {
        self.observed
    }
    fn ready(&self) -> bool {
        self.ready
    }
}
struct Size(usize);
impl Size {
    fn write_comma(&mut self) -> Result<()> {
        io::Write::write_all(self, b",").map_err(|_| incomplete("Reading batch budget"))
    }
}
impl io::Write for Size {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        if self.0 > MAX_BATCH {
            return Err(io::Error::other("HSES batch limit"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
