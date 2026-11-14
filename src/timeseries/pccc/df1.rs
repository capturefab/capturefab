//! Rockwell DF1 full duplex over direct RS-232, with PCCC 0F/A2 reads.
//! See 1770-RM516 chapters 4–7 and AG-UM008 appendix B for link controls.
use super::{Signal, max_read_bytes, signal_addresses, validate_read_width};
use crate::{
    timeseries::{Source, serial},
    types::{Categorized, ErrorKind, Usage},
};
use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub device: String,
    #[serde(default = "baud")]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub baud: u32,
    #[serde(default = "parity")]
    pub parity: serial::Parity,
    #[serde(default = "stop_bits")]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub stop_bits: u8,
    #[serde(default)]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub source_node: u8,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub destination_node: u8,
    #[serde(default)]
    pub checksum: Checksum,
    #[serde(default = "ack_timeout_ms")]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub ack_timeout_ms: u64,
    #[serde(default = "max_read_bytes")]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub max_read_bytes: u16,
    pub signals: Vec<Signal>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Checksum {
    #[default]
    Crc,
    Bcc,
}

const fn baud() -> u32 {
    19_200
}
const fn parity() -> serial::Parity {
    serial::Parity::None
}
const fn stop_bits() -> u8 {
    1
}
const fn ack_timeout_ms() -> u64 {
    1000
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    if config.device.is_empty()
        || config.source_node == 255
        || config.destination_node == 255
        || config.signals.is_empty()
        || config.signals.len() > 256
    {
        return Err(
            Usage("DF1 requires a serial device, node IDs 0–254 and 1–256 signals".into()).into(),
        );
    }
    if !matches!(config.parity, serial::Parity::None | serial::Parity::Even)
        || !(20..=10_000).contains(&config.ack_timeout_ms)
        || !(4..=224).contains(&config.max_read_bytes)
    {
        return Err(Usage(
            "DF1 requires none/even parity, ack_timeout_ms 20–10000 and max_read_bytes 4–224"
                .into(),
        )
        .into());
    }
    serial::validate(config.baud, config.stop_bits, 8)?;
    let addresses = signal_addresses(&config.signals)?;
    validate_read_width(&addresses, config.max_read_bytes)?;
    #[cfg(any(unix, windows))]
    {
        wire::open(config, addresses)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (addresses, config.checksum);
        Err(Categorized::error(
            ErrorKind::Unsupported,
            "DF1 requires a Unix terminal or Windows COM port",
        ))
    }
}

#[cfg(any(unix, windows))]
mod wire {
    use super::super::{Address, io_error, read_signals, typed_request, typed_response};
    use super::{Categorized, Checksum, Config, ErrorKind, Source, Usage, serial};
    use crate::timeseries::Reading;
    use anyhow::Result;
    use std::time::{Duration, Instant};

    const DLE: u8 = 0x10;
    const STX: u8 = 0x02;
    const ETX: u8 = 0x03;
    const ENQ: u8 = 0x05;
    const ACK: u8 = 0x06;
    const NAK: u8 = 0x15;
    const MAX_BODY: usize = 6 + 224;
    const MAX_WIRE_BYTES: usize = 8192;
    const RETRIES: u8 = 3;

    pub(super) fn open(config: &Config, addresses: Vec<Address>) -> Result<Box<dyn Source>> {
        let port = serial::open(
            &config.device,
            config.baud,
            config.parity,
            config.stop_bits,
            8,
        )?;
        port.discard_input().map_err(|error| io_error(&error))?;
        let mut random = [0; 2];
        getrandom::fill(&mut random)
            .map_err(|error| anyhow::anyhow!("DF1 transaction seed: {error}"))?;
        Ok(Box::new(Client {
            config: config.clone(),
            addresses,
            link: Some(Link {
                port,
                decoder: Decoder::new(config.checksum),
                input: [0; 256],
                cursor: 0,
                filled: 0,
                last_response: NAK,
                transaction: u16::from_le_bytes(random),
            }),
        }))
    }

    struct Client {
        config: Config,
        addresses: Vec<Address>,
        link: Option<Link>,
    }
    impl Source for Client {
        fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
            let deadline = Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| Usage("DF1 timeout is too large".into()))?;
            let link = self
                .link
                .as_mut()
                .ok_or_else(|| protocol("serial link closed"))?;
            let config = &self.config;
            let result = read_signals(
                &config.signals,
                &self.addresses,
                config.max_read_bytes,
                deadline,
                |address, element, bytes, data| {
                    link.transaction = link.transaction.wrapping_add(1);
                    let mut request = vec![config.destination_node, config.source_node];
                    typed_request(&mut request, address, element, bytes, link.transaction)?;
                    let reply =
                        link.exchange(config, &frame(config.checksum, &request), deadline)?;
                    data.extend_from_slice(typed_response(&reply[2..], link.transaction, bytes)?);
                    Ok(())
                },
            )
            .and_then(|readings| {
                check_deadline(deadline)?;
                Ok(readings)
            });
            if result.is_err() {
                self.link = None;
            }
            result
        }
    }

    struct Link {
        port: serial::Port,
        decoder: Decoder,
        input: [u8; 256],
        cursor: usize,
        filled: usize,
        last_response: u8,
        transaction: u16,
    }
    impl Link {
        fn exchange(
            &mut self,
            config: &Config,
            request: &[u8],
            deadline: Instant,
        ) -> Result<Vec<u8>> {
            let mut ack_at = self.transmit(config, request, deadline)?;
            let mut acked = false;
            let mut reply: Option<Vec<u8>> = None;
            let mut inquiries = 0;
            let mut retransmits = 0;
            let mut bad_frames = 0;
            let mut wire_left = MAX_WIRE_BYTES;
            let expected_route = [config.source_node, config.destination_node];
            let expected_tns = self.transaction.to_le_bytes();

            loop {
                check_deadline(deadline)?;
                let wait_until = if acked { deadline } else { ack_at };
                match self.event(wait_until, deadline, &mut wire_left)? {
                    Some(Event::Ack) => acked = true,
                    Some(Event::Nak) if !acked => {
                        if retransmits == RETRIES {
                            return Err(protocol("request NAK retry limit exceeded"));
                        }
                        retransmits += 1;
                        ack_at = self.transmit(config, request, deadline)?;
                    }
                    Some(Event::Nak) => {}
                    Some(Event::Enq) => self.write(&[DLE, self.last_response], deadline)?,
                    Some(Event::Noise) => self.last_response = NAK,
                    Some(Event::BadFrame) => {
                        self.respond(NAK, deadline)?;
                        if bad_frames == RETRIES {
                            return Err(protocol("invalid response frame retry limit exceeded"));
                        }
                        bad_frames += 1;
                    }
                    Some(Event::Frame) => {
                        self.respond(ACK, deadline)?;
                        // Link ACK is independent of the application status. The
                        // typed reply can arrive before the request's link ACK.
                        let body = &self.decoder.body;
                        if body[..2] == expected_route
                            && body[2] & 0x40 != 0
                            && body[4..6] == expected_tns
                        {
                            if let Some(previous) = &reply {
                                if previous != body {
                                    return Err(protocol(
                                        "conflicting replies for one transaction",
                                    ));
                                }
                            } else {
                                reply = Some(body.clone());
                            }
                        }
                    }
                    None if Instant::now() < wait_until => {}
                    None => {
                        check_deadline(deadline)?;
                        if inquiries == RETRIES {
                            return Err(timeout("request ACK inquiry limit exceeded"));
                        }
                        inquiries += 1;
                        ack_at = self.transmit(config, &[DLE, ENQ], deadline)?;
                    }
                }
                if acked && let Some(reply) = reply.take() {
                    check_deadline(deadline)?;
                    return Ok(reply);
                }
            }
        }

        fn event(
            &mut self,
            wait_until: Instant,
            deadline: Instant,
            wire_left: &mut usize,
        ) -> Result<Option<Event>> {
            loop {
                check_deadline(deadline)?;
                if self.cursor == self.filled {
                    let Some(count) = self
                        .port
                        .receive(&mut self.input, wait_until)
                        .map_err(|error| io_error(&error))?
                    else {
                        return Ok(None);
                    };
                    if count == 0 {
                        return Err(Categorized::error(
                            ErrorKind::Disconnected,
                            "DF1 serial port closed",
                        ));
                    }
                    self.cursor = 0;
                    self.filled = count;
                }
                *wire_left = wire_left
                    .checked_sub(1)
                    .ok_or_else(|| protocol("response wire byte limit exceeded"))?;
                let byte = self.input[self.cursor];
                self.cursor += 1;
                if let Some(event) = self.decoder.push(byte) {
                    return Ok(Some(event));
                }
            }
        }

        fn respond(&mut self, response: u8, deadline: Instant) -> Result<()> {
            self.write(&[DLE, response], deadline)?;
            self.last_response = response;
            Ok(())
        }

        fn transmit(
            &mut self,
            config: &Config,
            bytes: &[u8],
            deadline: Instant,
        ) -> Result<Instant> {
            self.write(bytes, deadline)?;
            // Writes may return while the serial driver still has bytes queued.
            let bits = 9
                + u64::from(config.stop_bits)
                + u64::from(!matches!(config.parity, serial::Parity::None));
            let wire_ns = (u64::try_from(bytes.len())? * bits * 1_000_000_000)
                .div_ceil(u64::from(config.baud));
            let wait = Duration::from_millis(config.ack_timeout_ms) + Duration::from_nanos(wire_ns);
            let now = Instant::now();
            Ok(now + wait.min(deadline.saturating_duration_since(now)))
        }

        fn write(&mut self, bytes: &[u8], deadline: Instant) -> Result<()> {
            check_deadline(deadline)?;
            self.port
                .write_all(bytes, deadline)
                .map_err(|error| io_error(&error))
        }
    }

    #[derive(Clone, Copy)]
    enum State {
        Idle,
        Prefix,
        Body,
        Escape,
        Trailer { first: Option<u8> },
    }
    #[derive(Clone, Copy)]
    enum Event {
        Ack,
        Nak,
        Enq,
        Noise,
        BadFrame,
        Frame,
    }
    struct Decoder {
        checksum: Checksum,
        state: State,
        body: Vec<u8>,
        overflow: bool,
    }
    impl Decoder {
        fn new(checksum: Checksum) -> Self {
            Self {
                checksum,
                state: State::Idle,
                body: Vec::with_capacity(MAX_BODY),
                overflow: false,
            }
        }

        fn start(&mut self) {
            self.body.clear();
            self.overflow = false;
            self.state = State::Body;
        }

        fn data(&mut self, byte: u8) {
            if self.body.len() == MAX_BODY {
                self.overflow = true;
            } else {
                self.body.push(byte);
            }
        }

        fn push(&mut self, byte: u8) -> Option<Event> {
            match self.state {
                State::Idle => {
                    if byte == DLE {
                        self.state = State::Prefix;
                    } else {
                        return Some(Event::Noise);
                    }
                }
                State::Prefix => {
                    self.state = State::Idle;
                    match byte {
                        STX => self.start(),
                        ACK => return Some(Event::Ack),
                        NAK => return Some(Event::Nak),
                        ENQ => return Some(Event::Enq),
                        _ => return Some(Event::Noise),
                    }
                }
                State::Body => {
                    if byte == DLE {
                        self.state = State::Escape;
                    } else {
                        self.data(byte);
                    }
                }
                State::Escape => {
                    self.state = State::Body;
                    match byte {
                        DLE => self.data(DLE),
                        ACK => return Some(Event::Ack),
                        NAK => return Some(Event::Nak),
                        ETX => self.state = State::Trailer { first: None },
                        STX => {
                            self.start();
                            return Some(Event::BadFrame);
                        }
                        _ => {
                            self.state = State::Idle;
                            return Some(Event::BadFrame);
                        }
                    }
                }
                State::Trailer { first } => {
                    if matches!(self.checksum, Checksum::Crc) && first.is_none() {
                        self.state = State::Trailer { first: Some(byte) };
                    } else {
                        self.state = State::Idle;
                        let (expected, _) = self.checksum.bytes(&self.body);
                        let valid = first
                            .map_or_else(|| expected[0] == byte, |first| expected == [first, byte]);
                        return Some(if valid && !self.overflow && self.body.len() >= 6 {
                            Event::Frame
                        } else {
                            Event::BadFrame
                        });
                    }
                }
            }
            None
        }
    }

    impl Checksum {
        fn bytes(self, body: &[u8]) -> ([u8; 2], usize) {
            match self {
                Self::Bcc => ([body.iter().copied().fold(0_u8, u8::wrapping_sub), 0], 1),
                Self::Crc => {
                    let mut crc = 0_u16;
                    for byte in body.iter().copied().chain([ETX]) {
                        crc ^= u16::from(byte);
                        for _ in 0..8 {
                            crc = (crc >> 1) ^ if crc & 1 == 0 { 0 } else { 0xa001 };
                        }
                    }
                    (crc.to_le_bytes(), 2)
                }
            }
        }
    }

    fn frame(checksum: Checksum, body: &[u8]) -> Vec<u8> {
        let mut wire = Vec::with_capacity(body.len() * 2 + 6);
        wire.extend_from_slice(&[DLE, STX]);
        for &byte in body {
            wire.push(byte);
            if byte == DLE {
                wire.push(DLE);
            }
        }
        wire.extend_from_slice(&[DLE, ETX]);
        let (checksum, width) = checksum.bytes(body);
        wire.extend_from_slice(&checksum[..width]);
        wire
    }

    fn protocol(message: &str) -> anyhow::Error {
        Categorized::error(ErrorKind::Incomplete, format!("DF1: {message}"))
    }
    fn timeout(message: &str) -> anyhow::Error {
        Categorized::error(ErrorKind::Timeout, format!("DF1: {message}"))
    }
    fn check_deadline(deadline: Instant) -> Result<()> {
        if Instant::now() >= deadline {
            return Err(timeout("poll deadline expired"));
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::{Checksum, frame};

        #[test]
        fn published_df1_envelopes_match_bcc_and_crc() {
            assert_eq!(
                frame(Checksum::Bcc, &[0x08, 0x09, 0x06, 0, 0x02, 0x04, 0x03]),
                [
                    0x10, 0x02, 0x08, 0x09, 0x06, 0, 0x02, 0x04, 0x03, 0x10, 0x03, 0xe0
                ]
            );
            assert_eq!(
                frame(Checksum::Bcc, &[0x08, 0x09, 0x06, 0, 0x10, 0x04, 0x03]),
                [
                    0x10, 0x02, 0x08, 0x09, 0x06, 0, 0x10, 0x10, 0x04, 0x03, 0x10, 0x03, 0xd2
                ]
            );
            // EtherMeter 2.06 printed p. 59, common DF1 envelope and typed read.
            assert_eq!(
                frame(
                    Checksum::Crc,
                    &[4, 0, 0x0f, 0, 0x4f, 0, 0xa2, 0x10, 7, 0x89, 0, 0]
                ),
                [
                    0x10, 0x02, 4, 0, 0x0f, 0, 0x4f, 0, 0xa2, 0x10, 0x10, 7, 0x89, 0, 0, 0x10,
                    0x03, 0x10, 0x64
                ]
            );
        }
    }
}
