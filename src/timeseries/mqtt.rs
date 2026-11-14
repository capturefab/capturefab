//! Bounded MQTT 3.1.1 subscriptions over TCP or verified TLS.
//! Wire format: OASIS MQTT 3.1.1, sections 2 and 3:
//! <https://docs.oasis-open.org/mqtt/mqtt/v3.1.1/os/mqtt-v3.1.1-os.html>.
use super::net as transport;
use super::{Reading, Source};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::{Read as _, Write as _},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

const MAX_PACKET: usize = 1024 * 1024;
const KEEP_ALIVE_SECS: u16 = 30;
const PING_AFTER: Duration = Duration::from_secs(15);
const PING_WAIT: Duration = Duration::from_secs(15);
// One bit per possible MQTT packet identifier, independent of publication size
// or broker behavior. QoS 2 Method B needs only IDs after onward delivery.
const IDENTIFIER_WORDS: usize = 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    /// Numeric IP and port; DNS is excluded so connecting has a bounded deadline.
    pub broker: SocketAddr,
    pub topics: Vec<String>,
    #[serde(
        default = "default_qos",
        deserialize_with = "crate::config_number::unsigned"
    )]
    pub qos: u8,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub username_env: Option<String>,
    #[serde(default)]
    pub password_env: Option<String>,
    #[serde(default)]
    pub tls: Option<transport::TlsConfig>,
}
const fn default_qos() -> u8 {
    1
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    if config.broker.port() == 0 {
        return Err(Usage("MQTT broker port must be nonzero".into()).into());
    }
    if config.qos > 2 {
        return Err(Usage("MQTT qos must be 0, 1 or 2".into()).into());
    }
    if config.topics.is_empty() || config.topics.len() > 256 {
        return Err(Usage("MQTT topics must contain between 1 and 256 filters".into()).into());
    }
    for topic in &config.topics {
        validate_filter(topic)?;
    }
    if config
        .topics
        .iter()
        .map(|topic| topic.len() + 3)
        .sum::<usize>()
        + 2
        > MAX_PACKET
    {
        return Err(Usage("MQTT subscription packet exceeds 1 MiB".into()).into());
    }
    let client_id = if let Some(client_id) = &config.client_id {
        client_id.clone()
    } else {
        // Multiple sources in one process must not evict one another from a
        // broker. A new clean session gets a compact, unique identifier.
        let mut random = [0; 8];
        getrandom::fill(&mut random)
            .map_err(|_| protocol_error("could not generate an MQTT client identifier"))?;
        // This alphanumeric identifier fits the mandatory 1..=23 byte
        // broker acceptance range in MQTT-3.1.3-5.
        format!("capfab{:016x}", u64::from_be_bytes(random))
    };
    if !valid_string(&client_id) {
        return Err(Usage("MQTT client_id must be a nonempty UTF-8 string, at most 65535 bytes, without NUL or noncharacters".into()).into());
    }
    if config.password_env.is_some() && config.username_env.is_none() {
        return Err(Usage("MQTT password_env requires username_env".into()).into());
    }
    for name in config.username_env.iter().chain(&config.password_env) {
        if name.is_empty() || name.contains(['=', '\0']) {
            return Err(Usage("MQTT credential environment variable names must be nonempty and contain neither '=' nor NUL".into()).into());
        }
    }
    let tls = transport::prepare(config.tls.as_ref())?;
    Ok(Box::new(Subscriber {
        config: config.clone(),
        client_id,
        connection: None,
        pending: None,
        tls,
    }))
}

fn valid_string(value: &str) -> bool {
    !value.is_empty()
        && u16::try_from(value.len()).is_ok()
        && !value.chars().any(|character| {
            let code = u32::from(character);
            character == '\0' || (0xfdd0..=0xfdef).contains(&code) || code & 0xffff >= 0xfffe
        })
}

fn validate_filter(filter: &str) -> Result<()> {
    if !valid_string(filter) {
        return Err(Usage("MQTT topic filters must be nonempty UTF-8 strings, at most 65535 bytes, without NUL or noncharacters".into()).into());
    }
    let levels: Vec<_> = filter.split('/').collect();
    for (index, level) in levels.iter().enumerate() {
        if (level.contains('#') && (*level != "#" || index + 1 != levels.len()))
            || (level.contains('+') && *level != "+")
        {
            return Err(Usage(
                "MQTT '+' must occupy an entire topic level; '#' must occupy the final level"
                    .into(),
            )
            .into());
        }
    }
    Ok(())
}

struct Subscriber {
    config: Config,
    client_id: String,
    connection: Option<Connection>,
    pending: Option<Reading>,
    tls: transport::Prepared,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Connack,
    Suback,
    Ready,
}

struct Connection {
    stream: Box<dyn transport::Transport>,
    received: Vec<u8>,
    stage: Stage,
    last_sent: Instant,
    ping_sent: Option<Instant>,
    awaiting_pubrel: Box<[u64; IDENTIFIER_WORDS]>,
}

impl Source for Subscriber {
    fn ready(&self) -> bool {
        self.connection
            .as_ref()
            .is_some_and(|connection| connection.stage == Stage::Ready)
    }

    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        if timeout.is_zero() {
            return Ok(Vec::new());
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("MQTT timeout is too large".into()))?;
        if self.connection.is_none() {
            let body = connect_packet(&self.config, &self.client_id)?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Categorized::error(
                    ErrorKind::Timeout,
                    "MQTT connection deadline expired",
                ));
            }
            let stream = TcpStream::connect_timeout(&self.config.broker, remaining)
                .map_err(|error| io_error("connect", &error))?;
            stream.set_nodelay(true)?;
            let mut connection = Connection {
                stream: self.tls.connect(stream, deadline)?,
                received: Vec::new(),
                stage: Stage::Connack,
                last_sent: Instant::now(),
                ping_sent: None,
                awaiting_pubrel: Box::new([0; IDENTIFIER_WORDS]),
            };
            connection.send(0x10, &body, deadline)?;
            self.connection = Some(connection);
        }
        // A partial packet remains buffered over ordinary idle timeouts. Any
        // actual protocol/I/O error terminates this connection, so a caller
        // cannot reuse bytes from an invalid session.
        let result = self.receive(deadline);
        if result.is_err() {
            self.connection = None;
            self.pending = None;
        }
        result
    }
}

impl Subscriber {
    fn receive(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        let Some(connection) = self.connection.as_mut() else {
            return Err(protocol_error("MQTT connection is missing"));
        };
        let mut readings: Vec<Reading> = self.pending.take().into_iter().collect();
        let mut batch_bytes = serde_json::to_vec(&readings)?.len();
        loop {
            let now = Instant::now();
            if connection
                .ping_sent
                .is_some_and(|sent| now.saturating_duration_since(sent) >= PING_WAIT)
            {
                return Err(Categorized::error(
                    ErrorKind::Timeout,
                    "MQTT broker did not answer PINGREQ in 15 seconds",
                ));
            }
            if now >= deadline {
                return Ok(readings);
            }
            if connection.stage != Stage::Ready
                && now.saturating_duration_since(connection.last_sent) >= PING_WAIT
            {
                return Err(Categorized::error(
                    ErrorKind::Timeout,
                    "MQTT broker did not complete the handshake in 15 seconds",
                ));
            }
            if connection.stage == Stage::Ready
                && connection.ping_sent.is_none()
                && now.saturating_duration_since(connection.last_sent) >= PING_AFTER
            {
                connection.send(0xc0, &[], deadline)?;
                connection.ping_sent = Some(Instant::now());
            }
            // Wake for keep-alive even when read() has a long deadline.
            let wake = if !readings.is_empty() {
                // Drain complete packets already in this bounded buffer, then
                // yield immediately. A polling interval must not limit MQTT
                // acquisition to one publication per tick.
                now
            } else if let Some(sent) = connection.ping_sent {
                deadline.min(sent + PING_WAIT)
            } else if connection.stage == Stage::Ready {
                deadline.min(connection.last_sent + PING_AFTER)
            } else {
                deadline.min(connection.last_sent + PING_WAIT)
            };
            let Some((header, body)) = connection.packet(wake)? else {
                if !readings.is_empty() {
                    return Ok(readings);
                }
                continue;
            };
            match header >> 4 {
                2 if connection.stage == Stage::Connack => {
                    if header != 0x20 || body.len() != 2 || body[0] != 0 || body[1] > 5 {
                        return Err(protocol_error("invalid MQTT CONNACK"));
                    }
                    if body[1] != 0 {
                        let kind = if matches!(body[1], 4 | 5) {
                            ErrorKind::AuthRejected
                        } else {
                            ErrorKind::Unavailable
                        };
                        return Err(Categorized::new(kind, "MQTT broker rejected CONNECT")
                            .details(json!({"return_code": body[1]}))
                            .into());
                    }
                    let mut subscribe = vec![0, 1];
                    for topic in &self.config.topics {
                        push_string(&mut subscribe, topic)?;
                        subscribe.push(self.config.qos);
                    }
                    connection.send(0x82, &subscribe, deadline)?;
                    connection.stage = Stage::Suback;
                }
                9 if connection.stage == Stage::Suback => {
                    if header != 0x90
                        || body.len() != self.config.topics.len() + 2
                        || body[..2] != [0, 1]
                        || body[2..]
                            .iter()
                            .any(|code| *code != 0x80 && *code > self.config.qos)
                    {
                        return Err(protocol_error("invalid MQTT SUBACK"));
                    }
                    if body[2..].contains(&0x80) {
                        return Err(Categorized::error(
                            ErrorKind::Unavailable,
                            "MQTT broker rejected a topic subscription",
                        ));
                    }
                    connection.stage = Stage::Ready;
                }
                3 if connection.stage != Stage::Connack => {
                    let (reading, packet_id) = publish(header, &body)?;
                    let encoded = serde_json::to_vec(&reading)?.len() + 1;
                    if encoded + 2 > MAX_PACKET {
                        return Err(protocol_error("MQTT reading exceeds the 1 MiB JSON limit"));
                    }
                    if let Some(id) = packet_id {
                        let qos = (header >> 1) & 3;
                        if qos == 1 {
                            if connection.awaiting_pubrel(id) {
                                return Err(protocol_error(
                                    "MQTT reused an active QoS 2 packet identifier for QoS 1",
                                ));
                            }
                            connection.send(0x40, &id.to_be_bytes(), deadline)?;
                        } else {
                            // MQTT 4.3.3 Method B: deliver at initial PUBLISH,
                            // retaining only its identifier until PUBREL. Any
                            // subsequent PUBLISH with this ID is acknowledged
                            // again without onward delivery, even with DUP=0.
                            let duplicate = connection.awaiting_pubrel(id);
                            connection.set_awaiting_pubrel(id, true);
                            connection.send(0x50, &id.to_be_bytes(), deadline)?;
                            if duplicate {
                                continue;
                            }
                        }
                    }
                    if batch_bytes + encoded > MAX_PACKET {
                        self.pending = Some(reading);
                        return Ok(readings);
                    }
                    batch_bytes += encoded;
                    readings.push(reading);
                    if readings.len() >= 256 {
                        return Ok(readings);
                    }
                }
                6 if connection.stage != Stage::Connack => {
                    let Some(bytes) = body.as_slice().first_chunk::<2>() else {
                        return Err(protocol_error("invalid MQTT PUBREL length"));
                    };
                    let id = u16::from_be_bytes(*bytes);
                    if header != 0x62 || body.len() != 2 || id == 0 {
                        return Err(protocol_error(
                            "invalid MQTT PUBREL flags, length or identifier",
                        ));
                    }
                    // A repeated PUBREL must still receive PUBCOMP, even when
                    // its previous PUBCOMP already released the identifier.
                    connection.send(0x70, &id.to_be_bytes(), deadline)?;
                    connection.set_awaiting_pubrel(id, false);
                }
                13 if header == 0xd0 && body.is_empty() => {
                    if connection.ping_sent.take().is_none() {
                        return Err(protocol_error("unsolicited MQTT PINGRESP"));
                    }
                }
                _ => return Err(protocol_error("unexpected MQTT control packet")),
            }
        }
    }
}

impl Connection {
    fn awaiting_pubrel(&self, id: u16) -> bool {
        let id = usize::from(id);
        self.awaiting_pubrel[id / 64] & (1_u64 << (id % 64)) != 0
    }

    fn set_awaiting_pubrel(&mut self, id: u16, awaiting: bool) {
        let id = usize::from(id);
        let word = &mut self.awaiting_pubrel[id / 64];
        let mask = 1_u64 << (id % 64);
        if awaiting {
            *word |= mask;
        } else {
            *word &= !mask;
        }
    }

    fn send(&mut self, header: u8, body: &[u8], deadline: Instant) -> Result<()> {
        if body.len() > MAX_PACKET {
            return Err(Usage("MQTT outgoing packet exceeds 1 MiB".into()).into());
        }
        let mut bytes = zeroize::Zeroizing::new(Vec::with_capacity(body.len() + 5));
        bytes.push(header);
        let mut remaining = body.len();
        loop {
            let mut digit = u8::try_from(remaining % 128)?;
            remaining /= 128;
            if remaining != 0 {
                digit |= 128;
            }
            bytes.push(digit);
            if remaining == 0 {
                break;
            }
        }
        bytes.extend_from_slice(body);
        let mut written = 0;
        while written < bytes.len() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Categorized::error(
                    ErrorKind::Timeout,
                    "MQTT write timed out",
                ));
            }
            self.stream.set_deadline(deadline);
            match self.stream.write(&bytes[written..]) {
                Ok(0) => return Err(protocol_error("MQTT broker closed while sending")),
                Ok(count) => written += count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(io_error("write", &error)),
            }
        }
        // TLS writes can consume plaintext before their socket write fails.
        // Do not acknowledge a send until all encrypted output is delivered.
        self.stream.set_deadline(deadline);
        self.stream
            .flush()
            .map_err(|error| io_error("flush", &error))?;
        self.last_sent = Instant::now();
        Ok(())
    }

    fn packet(&mut self, deadline: Instant) -> Result<Option<(u8, Vec<u8>)>> {
        loop {
            if let Some((header_len, body_len)) = packet_length(&self.received)?
                && self.received.len() >= header_len + body_len
            {
                let header = self.received[0];
                let body = self.received[header_len..header_len + body_len].to_vec();
                self.received.drain(..header_len + body_len);
                return Ok(Some((header, body)));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            self.stream.set_deadline(deadline);
            let mut bytes = [0; 8192];
            match self.stream.read(&mut bytes) {
                Ok(0) => {
                    return Err(Categorized::error(
                        ErrorKind::Disconnected,
                        "MQTT broker closed the connection",
                    ));
                }
                Ok(count) => self.received.extend_from_slice(&bytes[..count]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    return Ok(None);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(io_error("read", &error)),
            }
        }
    }
}

fn packet_length(bytes: &[u8]) -> Result<Option<(usize, usize)>> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let mut value = 0usize;
    let mut multiplier = 1usize;
    for index in 1..=4 {
        let Some(&digit) = bytes.get(index) else {
            return Ok(None);
        };
        value += usize::from(digit & 0x7f) * multiplier;
        if digit & 0x80 == 0 {
            if value > MAX_PACKET {
                return Err(protocol_error("MQTT packet exceeds the 1 MiB limit"));
            }
            if index > 1 && digit == 0 {
                return Err(protocol_error(
                    "MQTT remaining length is not minimally encoded",
                ));
            }
            return Ok(Some((index + 1, value)));
        }
        multiplier *= 128;
    }
    Err(protocol_error("MQTT remaining length exceeds four bytes"))
}

fn publish(header: u8, body: &[u8]) -> Result<(Reading, Option<u16>)> {
    let qos = (header >> 1) & 3;
    if qos > 2 || (qos == 0 && header & 8 != 0) || body.len() < 2 {
        return Err(protocol_error("invalid or unsupported MQTT PUBLISH flags"));
    }
    let Some(prefix) = body.first_chunk::<2>() else {
        return Err(protocol_error("MQTT PUBLISH topic length is truncated"));
    };
    let topic_len = usize::from(u16::from_be_bytes(*prefix));
    let topic_end = topic_len + 2;
    if body.len() < topic_end {
        return Err(protocol_error("MQTT PUBLISH topic is truncated"));
    }
    let topic = std::str::from_utf8(&body[2..topic_end])
        .map_err(|_| protocol_error("MQTT PUBLISH topic is not UTF-8"))?;
    if !valid_string(topic) || topic.contains(['#', '+']) {
        return Err(protocol_error("invalid MQTT PUBLISH topic"));
    }
    let (payload_offset, packet_id) = if qos != 0 {
        if body.len() < topic_end + 2 {
            return Err(protocol_error(
                "MQTT PUBLISH packet identifier is truncated",
            ));
        }
        let id = u16::from_be_bytes([body[topic_end], body[topic_end + 1]]);
        if id == 0 {
            return Err(protocol_error("MQTT PUBLISH packet identifier is zero"));
        }
        (topic_end + 2, Some(id))
    } else {
        (topic_end, None)
    };
    let payload = &body[payload_offset..];
    let value = std::str::from_utf8(payload).map_or_else(
        |_| {
            json!({
                "encoding": "base64",
                "data": base64::engine::general_purpose::STANDARD.encode(payload)
            })
        },
        |text| serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned())),
    );
    Ok((
        Reading {
            signal: topic.to_owned(),
            value,
            source_time: None,
        },
        packet_id,
    ))
}

fn push_string(bytes: &mut Vec<u8>, text: &str) -> Result<()> {
    let len =
        u16::try_from(text.len()).map_err(|_| Usage("MQTT string exceeds 65535 bytes".into()))?;
    bytes.extend_from_slice(&len.to_be_bytes());
    bytes.extend_from_slice(text.as_bytes());
    Ok(())
}

fn credential(name: &str) -> Result<zeroize::Zeroizing<String>> {
    let value = std::env::var(name).map_err(|_| {
        Categorized::error(
            ErrorKind::AuthRequired,
            "MQTT credential environment variable is missing or is not UTF-8",
        )
    })?;
    crate::scrub::register(&value);
    Ok(zeroize::Zeroizing::new(value))
}

fn connect_packet(config: &Config, client_id: &str) -> Result<zeroize::Zeroizing<Vec<u8>>> {
    let mut flags = 2; // Clean session.
    if config.username_env.is_some() {
        flags |= 0x80;
    }
    if config.password_env.is_some() {
        flags |= 0x40;
    }
    let mut body = zeroize::Zeroizing::new(vec![0, 4, b'M', b'Q', b'T', b'T', 4, flags]);
    body.extend_from_slice(&KEEP_ALIVE_SECS.to_be_bytes());
    push_string(&mut body, client_id)?;
    if let Some(name) = &config.username_env {
        let value = credential(name)?;
        // MQTT permits an empty username. Other UTF-8 restrictions still apply.
        if !value.is_empty() && !valid_string(&value) {
            return Err(Usage(
                "MQTT username must be UTF-8, at most 65535 bytes, without NUL or noncharacters"
                    .into(),
            )
            .into());
        }
        push_string(&mut body, &value)?;
    }
    if let Some(name) = &config.password_env {
        // Passwords are binary MQTT fields; UTF-8 environment values may
        // contain characters that MQTT topic/username strings cannot contain.
        push_string(&mut body, &credential(name)?)?;
    }
    Ok(body)
}

fn protocol_error(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, message)
}

fn io_error(operation: &str, error: &std::io::Error) -> anyhow::Error {
    #[cfg(feature = "mqtt-tls")]
    if error
        .get_ref()
        .and_then(|error| error.downcast_ref::<rustls::Error>())
        .is_some_and(|error| matches!(error, rustls::Error::InvalidCertificate(_)))
    {
        return Categorized::error(
            ErrorKind::AuthRejected,
            "MQTT TLS server certificate verification failed",
        );
    }
    let kind = match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        std::io::ErrorKind::ConnectionRefused
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::ConnectionAborted
        | std::io::ErrorKind::BrokenPipe
        | std::io::ErrorKind::UnexpectedEof => ErrorKind::Disconnected,
        _ => ErrorKind::OperationFailed,
    };
    Categorized::error(kind, format!("MQTT {operation}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::TcpListener, thread};

    fn config(address: SocketAddr) -> Config {
        Config {
            broker: address,
            topics: vec!["line/+/temperature".into()],
            qos: 1,
            client_id: Some("capturefab-test".into()),
            username_env: None,
            password_env: None,
            tls: None,
        }
    }

    fn broker_packet(stream: &mut TcpStream) -> (u8, Vec<u8>) {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut header = [0; 1];
        stream.read_exact(&mut header).unwrap();
        let mut size = 0;
        let mut scale = 1;
        loop {
            let mut digit = [0];
            stream.read_exact(&mut digit).unwrap();
            size += usize::from(digit[0] & 0x7f) * scale;
            if digit[0] < 128 {
                break;
            }
            scale *= 128;
        }
        let mut bytes = vec![0; size];
        stream.read_exact(&mut bytes).unwrap();
        (header[0], bytes)
    }

    fn handshake(stream: &mut TcpStream) {
        let (header, connect) = broker_packet(stream);
        assert_eq!(header, 0x10);
        assert_eq!(&connect[..10], &[0, 4, b'M', b'Q', b'T', b'T', 4, 2, 0, 30]);
        stream.write_all(&[0x20, 2, 0, 0]).unwrap();
        let (header, subscribe) = broker_packet(stream);
        assert_eq!(header, 0x82);
        assert_eq!(&subscribe[..2], &[0, 1]);
        assert_eq!(subscribe.last(), Some(&1));
        stream.write_all(&[0x90, 3, 0, 1, 1]).unwrap();
    }

    fn buffered_subscriber(bytes: Vec<u8>) -> (Subscriber, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stream = TcpStream::connect(address).unwrap();
        let (server, _) = listener.accept().unwrap();
        (
            Subscriber {
                config: config(address),
                client_id: "test".into(),
                connection: Some(Connection {
                    stream: transport::plain(stream, Instant::now()),
                    received: bytes,
                    stage: Stage::Ready,
                    last_sent: Instant::now(),
                    ping_sent: None,
                    awaiting_pubrel: Box::new([0; IDENTIFIER_WORDS]),
                }),
                pending: None,
                tls: transport::prepare(None).unwrap(),
            },
            server,
        )
    }

    #[test]
    fn qos2_identifier_storage_covers_the_full_domain_with_fixed_memory() {
        let (mut subscriber, _server) = buffered_subscriber(Vec::new());
        let connection = subscriber.connection.as_mut().unwrap();
        assert_eq!(size_of_val(connection.awaiting_pubrel.as_ref()), 8192);
        for id in 1..=u16::MAX {
            assert!(!connection.awaiting_pubrel(id));
            connection.set_awaiting_pubrel(id, true);
            assert!(connection.awaiting_pubrel(id));
        }
        for id in 1..=u16::MAX {
            assert!(connection.awaiting_pubrel(id));
            connection.set_awaiting_pubrel(id, false);
            assert!(!connection.awaiting_pubrel(id));
        }
    }

    #[test]
    fn qos2_control_packets_validate_flags_lengths_and_nonzero_identifiers() {
        for bytes in [
            vec![0x60, 2, 0, 7],
            vec![0x6a, 2, 0, 7],
            vec![0x62, 1, 7],
            vec![0x62, 3, 0, 7, 0],
            vec![0x62, 2, 0, 0],
            vec![0x50, 2, 0, 7],
            vec![0x70, 2, 0, 7],
            vec![0x34, 5, 0, 1, b't', 0, 0],
            vec![0x34, 4, 0, 1, b't', 7],
        ] {
            let (mut subscriber, _server) = buffered_subscriber(bytes);
            subscriber.read(Duration::from_secs(1)).unwrap_err();
            assert!(subscriber.connection.is_none());
        }
    }

    #[test]
    fn qos1_cannot_reuse_an_identifier_owned_by_an_inflight_qos2_publication() {
        let (mut subscriber, _server) = buffered_subscriber(vec![0x32, 6, 0, 1, b't', 0, 7, b'2']);
        subscriber
            .connection
            .as_mut()
            .unwrap()
            .set_awaiting_pubrel(7, true);
        subscriber.read(Duration::from_secs(1)).unwrap_err();
    }

    #[test]
    fn rejects_invalid_requested_qos_and_grants_above_the_request() {
        let mut config = config("127.0.0.1:1883".parse().unwrap());
        config.qos = 3;
        let error = open(&config).err().unwrap();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Usage);
        let (mut subscriber, _server) = buffered_subscriber(vec![0x90, 3, 0, 1, 2]);
        subscriber.connection.as_mut().unwrap().stage = Stage::Suback;
        subscriber.read(Duration::from_secs(1)).unwrap_err();
    }

    #[test]
    fn json_numeric_payloads_are_not_rounded_to_machine_precision() {
        for literal in [
            "18446744073709551617",
            "-18446744073709551617",
            "0.123456789012345678901234567890123456789",
            "1.234567890123456789e+100",
        ] {
            let mut body = Vec::new();
            push_string(&mut body, "line/counter").unwrap();
            body.extend_from_slice(literal.as_bytes());
            let (reading, packet_id) = publish(0x30, &body).unwrap();
            assert_eq!(packet_id, None);
            assert!(reading.value.is_number());
            assert_eq!(reading.value.to_string(), literal);
        }
    }

    #[test]
    fn subscribes_and_acknowledges_qos1_json_without_mutating_the_payload() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let broker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            handshake(&mut stream);
            let mut body = Vec::new();
            push_string(&mut body, "line/1/temperature").unwrap();
            body.extend_from_slice(&[0, 7]);
            body.extend_from_slice(br#"{"value":23.5,"unit":"C"}"#);
            stream
                .write_all(&[0x32, u8::try_from(body.len()).unwrap()])
                .unwrap();
            stream.write_all(&body).unwrap();
            assert_eq!(broker_packet(&mut stream), (0x40, vec![0, 7]));
        });
        let mut subscriber = open(&config(address)).unwrap();
        assert!(!subscriber.ready());
        let readings = subscriber.read(Duration::from_secs(2)).unwrap();
        assert!(subscriber.ready());
        assert_eq!(readings.len(), 1);
        assert_eq!(readings[0].signal, "line/1/temperature");
        assert_eq!(readings[0].value, json!({"value":23.5,"unit":"C"}));
        broker.join().unwrap();
    }

    #[test]
    fn incomplete_packets_survive_an_idle_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (resume, ready) = std::sync::mpsc::channel();
        let broker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            handshake(&mut stream);
            stream.write_all(&[0x30, 7, 0, 1, b't']).unwrap();
            ready.recv().unwrap();
            stream.write_all(b"42.5").unwrap();
            // Keep the broker open until its subscriber receives the publication.
            let mut end = [0];
            let _ = stream.read(&mut end);
        });
        let mut subscriber = open(&config(address)).unwrap();
        assert!(
            subscriber
                .read(Duration::from_millis(100))
                .unwrap()
                .is_empty()
        );
        resume.send(()).unwrap();
        let readings = subscriber.read(Duration::from_secs(2)).unwrap();
        assert_eq!(readings[0].value, json!(42.5));
        drop(subscriber);
        broker.join().unwrap();
    }

    #[test]
    fn broker_refusals_preserve_the_typed_reason() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let broker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = broker_packet(&mut stream);
            stream.write_all(&[0x20, 2, 0, 5]).unwrap();
        });
        let mut subscriber = open(&config(address)).unwrap();
        let error = subscriber.read(Duration::from_secs(2)).err().unwrap();
        assert_eq!(
            error.downcast_ref::<Categorized>().unwrap().kind,
            ErrorKind::AuthRejected
        );
        broker.join().unwrap();
    }

    #[test]
    fn limits_and_topic_validation_reject_malformed_inputs() {
        packet_length(&[0x30, 0xff, 0xff, 0xff, 0x80]).unwrap_err();
        packet_length(&[0x30, 0x80, 0x80, 0x80, 1]).unwrap_err();
        packet_length(&[0x30, 0x80, 0]).unwrap_err();
        assert_eq!(packet_length(&[0x30, 0x80]).unwrap(), None);
        for filter in ["", "line/#/x", "line/part+", "line/part#", "a\0b"] {
            validate_filter(filter).unwrap_err();
        }
        for filter in ["#", "line/+", "line/#", "/+/temperature", "/"] {
            validate_filter(filter).unwrap();
        }
        publish(0x30, &[0, 1, b'#']).unwrap_err();
        publish(0x32, &[0, 1, b't', 0, 0]).unwrap_err();
        publish(0x36, &[0, 1, b't', 0, 1]).unwrap_err();
        let (reading, id) = publish(0x34, &[0, 1, b't', 0, 1, b'2']).unwrap();
        assert_eq!(reading.value, json!(2));
        assert_eq!(id, Some(1));
    }

    #[test]
    fn binary_payload_is_losslessly_encoded() {
        let (reading, packet_id) = publish(0x30, &[0, 1, b't', 0xff, 0, 0x80]).unwrap();
        assert_eq!(reading.value, json!({"encoding":"base64","data":"/wCA"}));
        assert_eq!(packet_id, None);
    }

    #[test]
    fn drains_buffered_publications_in_one_bounded_batch() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stream = TcpStream::connect(address).unwrap();
        let (_server, _) = listener.accept().unwrap();
        let mut subscriber = Subscriber {
            config: config(address),
            client_id: "test".into(),
            connection: Some(Connection {
                stream: transport::plain(stream, Instant::now()),
                received: vec![0x30, 4, 0, 1, b't', b'1', 0x30, 4, 0, 1, b't', b'2'],
                stage: Stage::Ready,
                last_sent: Instant::now(),
                ping_sent: None,
                awaiting_pubrel: Box::new([0; IDENTIFIER_WORDS]),
            }),
            pending: None,
            tls: transport::prepare(None).unwrap(),
        };
        let readings = subscriber.read(Duration::from_secs(1)).unwrap();
        assert_eq!(readings.len(), 2);
        assert_eq!(readings[0].value, json!(1));
        assert_eq!(readings[1].value, json!(2));
    }

    #[test]
    fn maintains_idle_connections_with_ping_request_and_response() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stream = TcpStream::connect(address).unwrap();
        let broker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert_eq!(broker_packet(&mut stream), (0xc0, vec![]));
            stream
                .write_all(&[0xd0, 0, 0x30, 4, 0, 1, b't', b'1'])
                .unwrap();
        });
        let mut subscriber = Subscriber {
            config: config(address),
            client_id: "test".into(),
            connection: Some(Connection {
                stream: transport::plain(stream, Instant::now()),
                received: Vec::new(),
                stage: Stage::Ready,
                last_sent: Instant::now().checked_sub(PING_AFTER).unwrap(),
                ping_sent: None,
                awaiting_pubrel: Box::new([0; IDENTIFIER_WORDS]),
            }),
            pending: None,
            tls: transport::prepare(None).unwrap(),
        };
        let readings = subscriber.read(Duration::from_secs(2)).unwrap();
        assert_eq!(readings[0].value, json!(1));
        assert!(subscriber.connection.as_ref().unwrap().ping_sent.is_none());
        broker.join().unwrap();
    }

    #[test]
    fn tcp_connection_without_connack_is_not_ready() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut subscriber = open(&config(listener.local_addr().unwrap())).unwrap();
        assert!(!subscriber.ready());
        // An unaccepted TCP connection can complete its TCP handshake and
        // receive CONNECT in the backlog, but cannot complete MQTT startup.
        assert!(
            subscriber
                .read(Duration::from_millis(20))
                .unwrap()
                .is_empty()
        );
        assert!(!subscriber.ready());
    }
}
