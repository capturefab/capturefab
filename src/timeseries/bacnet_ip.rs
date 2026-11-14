//! Direct unicast BACnet/IP `ReadProperty` (ANSI/ASHRAE 135 §15.5 and Annex J).
//! <https://bacnet.org/wp-content/uploads/sites/4/2022/08/Add-1995-135a.pdf>
//! <https://github.com/bacnet-stack/bacnet-stack/blob/master/src/bacnet/rp.c>
use super::{Reading, Source};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Map, Number, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    thread,
    time::{Duration, Instant},
};

const MAX_FRAME: usize = 4 + 2 + 1476;
const INVOKE_COUNT: usize = 256;
const READ_PROPERTY: u8 = 12;
const PRESENT_VALUE: u8 = 85;
const STATUS_FLAGS: u8 = 111;
const UNITS: u8 = 117;
const MAX_INSTANCE: u32 = (1 << 22) - 2;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    address: SocketAddrV4,
    bind: Option<SocketAddrV4>,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    reply_horizon_ms: Option<u64>,
    signals: Vec<Signal>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signal {
    name: String,
    object_type: ObjectType,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    instance: u32,
    #[serde(default)]
    units: bool,
    #[serde(default)]
    status_flags: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u16)]
enum ObjectType {
    AnalogInput = 0,
    AnalogOutput = 1,
    AnalogValue = 2,
    BinaryInput = 3,
    BinaryOutput = 4,
    BinaryValue = 5,
    MultiStateInput = 13,
    MultiStateOutput = 14,
    MultiStateValue = 19,
}

impl ObjectType {
    const fn is_analog(self) -> bool {
        matches!(
            self,
            Self::AnalogInput | Self::AnalogOutput | Self::AnalogValue
        )
    }

    fn decode(self, value: &[u8]) -> Result<Value> {
        match self {
            Self::AnalogInput | Self::AnalogOutput | Self::AnalogValue => {
                let [0x44, bytes @ ..] = value else {
                    return Err(protocol("analog Present_Value must be REAL"));
                };
                let bytes = bytes
                    .try_into()
                    .map_err(|_| protocol("invalid REAL width"))?;
                let number = f64::from(f32::from_be_bytes(bytes));
                Number::from_f64(number)
                    .map(Value::Number)
                    .ok_or_else(|| protocol("analog Present_Value is not finite"))
            }
            Self::BinaryInput | Self::BinaryOutput | Self::BinaryValue => {
                match unsigned(value, 0x90)? {
                    0 => Ok(Value::Bool(false)),
                    1 => Ok(Value::Bool(true)),
                    _ => Err(protocol("binary Present_Value must be inactive or active")),
                }
            }
            Self::MultiStateInput | Self::MultiStateOutput | Self::MultiStateValue => {
                let state = unsigned(value, 0x20)?;
                if state == 0 {
                    return Err(protocol("multi-state Present_Value must be positive"));
                }
                Ok(Value::from(state))
            }
        }
    }
}

pub(crate) fn open(config: &Config, timeout_ms: u64) -> Result<Box<dyn Source>> {
    let address = config.address.ip();
    if config.address.port() == 0
        || address.is_unspecified()
        || address.is_broadcast()
        || address.is_multicast()
    {
        return Err(
            Usage("BACnet/IP requires a unicast IPv4 address and nonzero port".into()).into(),
        );
    }
    let bind = config
        .bind
        .unwrap_or(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));
    if bind.ip().is_multicast() || bind.ip().is_broadcast() {
        return Err(Usage(
            "BACnet/IP bind must be a local unicast or wildcard IPv4 endpoint".into(),
        )
        .into());
    }
    let horizon = config.reply_horizon_ms.unwrap_or(timeout_ms);
    if !(timeout_ms..=30_000).contains(&horizon) {
        return Err(Usage(
            "BACnet/IP reply_horizon_ms must be at least timeout_ms and at most 30000".into(),
        )
        .into());
    }
    if config.signals.is_empty() || config.signals.len() > INVOKE_COUNT {
        return Err(Usage("BACnet/IP requires 1–256 signals".into()).into());
    }
    let mut requests = 0;
    let mut names = BTreeSet::new();
    for signal in &config.signals {
        super::validate_name(&signal.name, "BACnet/IP signal")?;
        if !names.insert(&signal.name) {
            return Err(Usage("BACnet/IP signal names must be unique".into()).into());
        }
        if signal.instance > MAX_INSTANCE {
            return Err(Usage("BACnet/IP instance must be 0–4194302".into()).into());
        }
        if signal.units && !signal.object_type.is_analog() {
            return Err(
                Usage("BACnet/IP units are only supported for analog objects".into()).into(),
            );
        }
        requests += 1 + usize::from(signal.units) + usize::from(signal.status_flags);
    }
    if requests > INVOKE_COUNT {
        return Err(Usage("BACnet/IP supports at most 256 property reads per poll".into()).into());
    }
    let binding = if bind.port() == 0 {
        Binding::Ephemeral {
            rotate: false,
            retired: BTreeMap::new(),
        }
    } else {
        Binding::Fixed(Box::new([None; INVOKE_COUNT]))
    };
    Ok(Box::new(Client {
        signals: config.signals.clone(),
        datagrams: Datagrams {
            address: config.address,
            bind,
            horizon: Duration::from_millis(horizon),
            binding,
            socket: None,
            invoke: 0,
        },
    }))
}

struct Client {
    signals: Vec<Signal>,
    datagrams: Datagrams,
}

impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("BACnet/IP timeout is too large".into()))?;
        let mut readings = Vec::with_capacity(self.signals.len());
        for signal in &self.signals {
            let value = signal.read(&mut self.datagrams, deadline)?;
            readings.push(Reading {
                signal: signal.name.clone(),
                value,
                source_time: None,
            });
        }
        remaining(deadline)?;
        Ok(readings)
    }
}

impl Signal {
    fn read(&self, datagrams: &mut Datagrams, deadline: Instant) -> Result<Value> {
        let object = (u32::from(self.object_type as u16) << 22) | self.instance;
        let mut frame = [0; MAX_FRAME + 1];
        let payload = datagrams.read(object, PRESENT_VALUE, deadline, &mut frame)?;
        let value = self.object_type.decode(payload)?;
        if !self.units && !self.status_flags {
            return Ok(value);
        }
        let mut data = Map::new();
        data.insert("value".into(), value);
        if self.units {
            let payload = datagrams.read(object, UNITS, deadline, &mut frame)?;
            data.insert("units".into(), Value::from(unsigned(payload, 0x90)?));
        }
        if self.status_flags {
            let payload = datagrams.read(object, STATUS_FLAGS, deadline, &mut frame)?;
            data.insert("status_flags".into(), status_flags(payload)?);
        }
        Ok(Value::Object(data))
    }
}

struct Datagrams {
    address: SocketAddrV4,
    bind: SocketAddrV4,
    horizon: Duration,
    binding: Binding,
    socket: Option<UdpSocket>,
    invoke: u8,
}

enum Binding {
    Ephemeral {
        rotate: bool,
        retired: BTreeMap<u16, Instant>,
    },
    Fixed(Box<[Option<Instant>; INVOKE_COUNT]>),
}

impl Datagrams {
    fn prepare_socket(&mut self, deadline: Instant) -> Result<()> {
        let Some(socket) = self.socket.as_ref() else {
            self.socket = Some(udp_connect(self.bind, self.address, deadline)?);
            return Ok(());
        };
        let Binding::Ephemeral { rotate, retired } = &mut self.binding else {
            return Ok(());
        };
        if !*rotate {
            return Ok(());
        }
        let previous = socket
            .local_addr()
            .map_err(|error| io_error(&error))?
            .port();
        let now = Instant::now();
        retired.retain(|_, expires| *expires > now);
        // Keep the old port bound during rotation and guard recently retired ports.
        for _ in 0..16 {
            let replacement = udp_connect(self.bind, self.address, deadline)?;
            let port = replacement
                .local_addr()
                .map_err(|error| io_error(&error))?
                .port();
            if port == previous || retired.contains_key(&port) {
                continue;
            }
            retired.insert(previous, Instant::now() + self.horizon);
            self.socket = Some(replacement);
            *rotate = false;
            return Ok(());
        }
        Err(protocol(
            "no fresh source port available within reply horizon",
        ))
    }

    fn read<'a>(
        &mut self,
        object: u32,
        property: u8,
        deadline: Instant,
        frame: &'a mut [u8; MAX_FRAME + 1],
    ) -> Result<&'a [u8]> {
        self.prepare_socket(deadline)?;
        let socket = self
            .socket
            .as_ref()
            .ok_or_else(|| protocol("socket missing"))?;
        let invoke = self.invoke;
        if let Binding::Fixed(available) = &self.binding
            && let Some(reusable_at) = available[usize::from(invoke)]
        {
            if reusable_at >= deadline {
                return Err(Categorized::new(
                    ErrorKind::Timeout,
                    "BACnet/IP invoke ID cannot be safely reused within the poll deadline",
                )
                .details(json!({"invoke_id":invoke,"reply_horizon_ms":self.horizon.as_millis()}))
                .into());
            }
            if let Some(wait) = reusable_at.checked_duration_since(Instant::now()) {
                thread::sleep(wait);
            }
            drain_udp(socket, deadline, frame)?;
        }
        let request = request(object, property, invoke);
        let sent = send_udp(socket, &request, deadline);
        self.invoke = invoke.wrapping_add(1);
        match &mut self.binding {
            Binding::Ephemeral { rotate, .. } => *rotate = self.invoke == 0,
            Binding::Fixed(available) => {
                available[usize::from(invoke)] = Some(Instant::now() + self.horizon);
            }
        }
        sent?;
        loop {
            let count = receive_udp(socket, deadline, frame)?;
            if correlated_apdu(&frame[..count], invoke)? {
                return property_value(&frame[6..count], object, property);
            }
        }
    }
}

fn request(object: u32, property: u8, invoke: u8) -> [u8; 17] {
    let [a, b, c, d] = object.to_be_bytes();
    [
        0x81,
        0x0a,
        0,
        17, // Original-Unicast-NPDU and full datagram length.
        1,
        0x04, // Local NPDU, expecting a reply.
        0,
        5,
        invoke,
        READ_PROPERTY, // Confirmed unsegmented, max APDU 1476.
        0x0c,
        a,
        b,
        c,
        d, // Object identifier [0].
        0x19,
        property, // Property identifier [1], no array index.
    ]
}

fn correlated_apdu(frame: &[u8], invoke: u8) -> Result<bool> {
    let [0x81, 0x0a, high, low, 1, control, apdu @ ..] = frame else {
        return Err(protocol("invalid BACnet/IP Original-Unicast-NPDU"));
    };
    if frame.len() != usize::from(u16::from_be_bytes([*high, *low])) || control & !0x07 != 0 {
        return Err(protocol("invalid BACnet/IP length or direct NPDU control"));
    }
    let [kind, received, ..] = apdu else {
        return Err(protocol("truncated APDU"));
    };
    // Unrelated client requests and responses to older invokes can arrive
    // while a poll is outstanding. Neither can satisfy this exchange.
    if kind & 0xf0 <= 0x10 || *received != invoke {
        return Ok(false);
    }
    if control & 0x04 != 0 {
        return Err(protocol("ReadProperty response cannot request a reply"));
    }
    Ok(true)
}

fn property_value(apdu: &[u8], object: u32, property: u8) -> Result<&[u8]> {
    match apdu {
        [0x30, _, READ_PROPERTY, 0x0c, a, b, c, d, fields @ ..] => {
            if u32::from_be_bytes([*a, *b, *c, *d]) != object {
                return Err(protocol("ReadProperty object differs from request"));
            }
            let Some((&header, rest)) = fields.split_first() else {
                return Err(protocol("ReadProperty property identifier is missing"));
            };
            let width = usize::from(header & 7);
            if header & 0xf8 != 0x18 || !(1..=4).contains(&width) {
                return Err(protocol("invalid ReadProperty property identifier tag"));
            }
            let Some((property_bytes, value)) = rest.split_at_checked(width) else {
                return Err(protocol("truncated ReadProperty property identifier"));
            };
            if decode_unsigned(property_bytes) != u32::from(property) {
                return Err(protocol("ReadProperty property differs from request"));
            }
            match value {
                [0x3e, payload @ .., 0x3f] => Ok(payload),
                _ => Err(protocol("invalid ReadProperty scalar value tags")),
            }
        }
        [0x50, _, READ_PROPERTY, fields @ ..] => {
            let (class, code) = enumerated_pair(fields)?;
            Err(
                Categorized::new(ErrorKind::OperationFailed, "BACnet/IP ReadProperty failed")
                    .details(json!({"bacnet_error_class":class,"bacnet_error_code":code}))
                    .into(),
            )
        }
        [0x60, _, reason] => Err(Categorized::new(
            ErrorKind::OperationFailed,
            "BACnet/IP ReadProperty rejected",
        )
        .details(json!({"bacnet_reject_reason":reason}))
        .into()),
        [0x71, _, reason] => Err(Categorized::new(
            ErrorKind::OperationFailed,
            "BACnet/IP server aborted ReadProperty",
        )
        .details(json!({"bacnet_abort_reason":reason}))
        .into()),
        _ => Err(protocol("unexpected or segmented ReadProperty response")),
    }
}

fn decode_unsigned(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .fold(0, |value, byte| (value << 8) | u32::from(*byte))
}

fn unsigned(data: &[u8], tag: u8) -> Result<u32> {
    let Some((&header, bytes)) = data.split_first() else {
        return Err(protocol("missing numeric application value"));
    };
    if header & 0xf8 != tag
        || !(1..=4).contains(&bytes.len())
        || usize::from(header & 7) != bytes.len()
    {
        return Err(protocol("unexpected numeric application tag or length"));
    }
    Ok(decode_unsigned(bytes))
}

fn enumerated_pair(fields: &[u8]) -> Result<(u32, u32)> {
    let Some(header) = fields.first() else {
        return Err(protocol("missing ReadProperty error class and code"));
    };
    let Some((class, code)) = fields.split_at_checked(1 + usize::from(header & 7)) else {
        return Err(protocol("truncated ReadProperty error class and code"));
    };
    Ok((unsigned(class, 0x90)?, unsigned(code, 0x90)?))
}

fn status_flags(payload: &[u8]) -> Result<Value> {
    let [0x82, 4, flags] = payload else {
        return Err(protocol("Status_Flags must be a four-bit BIT STRING"));
    };
    if flags & 0x0f != 0 {
        return Err(protocol("nonzero Status_Flags padding bits"));
    }
    Ok(json!({
        "in_alarm":flags & 0x80 != 0,
        "fault":flags & 0x40 != 0,
        "overridden":flags & 0x20 != 0,
        "out_of_service":flags & 0x10 != 0
    }))
}

fn protocol(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, format!("BACnet/IP: {message}"))
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "BACnet/IP poll deadline expired",
        ));
    }
    Ok(left)
}

fn io_error(error: &io::Error) -> anyhow::Error {
    let kind = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::Disconnected,
    };
    Categorized::error(kind, format!("BACnet/IP I/O: {error}"))
}

fn udp_connect(bind: SocketAddrV4, address: SocketAddrV4, deadline: Instant) -> Result<UdpSocket> {
    remaining(deadline)?;
    let socket = UdpSocket::bind(bind).map_err(|error| io_error(&error))?;
    remaining(deadline)?;
    socket.connect(address).map_err(|error| io_error(&error))?;
    Ok(socket)
}

fn send_udp(socket: &UdpSocket, request: &[u8], deadline: Instant) -> Result<()> {
    loop {
        socket
            .set_write_timeout(Some(remaining(deadline)?))
            .map_err(|error| io_error(&error))?;
        match socket.send(request) {
            Ok(count) if count == request.len() => return Ok(()),
            Ok(_) => return Err(protocol("incomplete datagram send")),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(io_error(&error)),
        }
    }
}

fn recv_udp(socket: &UdpSocket, frame: &mut [u8; MAX_FRAME + 1]) -> io::Result<usize> {
    // Winsock reports WSAEMSGSIZE for a datagram that fills and exceeds the buffer.
    const WSAEMSGSIZE: i32 = 10040;
    match socket.recv(frame) {
        Err(error) if cfg!(windows) && error.raw_os_error() == Some(WSAEMSGSIZE) => Ok(frame.len()),
        result => result,
    }
}

fn receive_udp(
    socket: &UdpSocket,
    deadline: Instant,
    frame: &mut [u8; MAX_FRAME + 1],
) -> Result<usize> {
    loop {
        socket
            .set_read_timeout(Some(remaining(deadline)?))
            .map_err(|error| io_error(&error))?;
        match recv_udp(socket, frame) {
            Ok(count) if count > MAX_FRAME => return Err(protocol("oversized datagram")),
            Ok(count) => {
                remaining(deadline)?;
                return Ok(count);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(io_error(&error)),
        }
    }
}

fn drain_udp(socket: &UdpSocket, deadline: Instant, frame: &mut [u8; MAX_FRAME + 1]) -> Result<()> {
    socket
        .set_nonblocking(true)
        .map_err(|error| io_error(&error))?;
    let result = (|| {
        loop {
            remaining(deadline)?;
            match recv_udp(socket, frame) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
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
