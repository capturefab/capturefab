//! Direct SLC 5/05 `CSPv4` read-only TCP. The 28-byte envelope and local LSAP
//! follow `PCCCComm` commit 223cfa29. The registration frame is also in Redpoint
//! commit 23ef36bf. Rockwell 1770-RM516 §7-17 defines the PCCC command.
use super::{
    Address, Reading, Signal, Source, io_error, max_read_bytes, read, read_signals, remaining,
    signal_addresses, typed_request, typed_response, validate_read_width, write,
};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;
use std::{
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

const HEADER_BYTES: usize = 28;
const REGISTER: u8 = 1;
const PCCC: u8 = 7;
const REGISTER_CONTEXT: [u8; 16] = [0, 4, 0, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
const MAX_BODY: usize = 4 + 4 + 224;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub address: SocketAddr,
    #[serde(default)]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub lsap_control: u8,
    #[serde(default = "max_read_bytes")]
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub max_read_bytes: u16,
    pub signals: Vec<Signal>,
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    Ok(Box::new(Client::new(config)?))
}

struct Client {
    config: Config,
    addresses: Vec<Address>,
    connection: Option<Connection>,
    retired: bool,
}

impl Client {
    fn new(config: &Config) -> Result<Self> {
        if config.address.port() == 0
            || !matches!(config.lsap_control, 0 | 5)
            || !(4..=224).contains(&config.max_read_bytes)
            || !(1..=256).contains(&config.signals.len())
        {
            return Err(Usage(
                "CSP requires a nonzero TCP port, lsap_control 0 or 5, max_read_bytes 4–224 and 1–256 signals".into(),
            )
            .into());
        }
        let addresses = signal_addresses(&config.signals)?;
        validate_read_width(&addresses, config.max_read_bytes)?;
        for (signal, address) in config.signals.iter().zip(&addresses) {
            if !matches!(address.file_type, 0x85 | 0x86 | 0x87 | 0x89 | 0x8a | 0x8d)
                || !(3..=255).contains(&address.file)
                || u32::from(address.element) + u32::from(signal.elements) > 256
            {
                return Err(Usage(
                    "CSP requires SLC N/B/F/T/C/ST files 3–255 and ranges within elements 0–255"
                        .into(),
                )
                .into());
            }
        }
        Ok(Self {
            config: config.clone(),
            addresses,
            connection: None,
            retired: false,
        })
    }

    fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        let config = &self.config;
        let slot = &mut self.connection;
        let readings = read_signals(
            &config.signals,
            &self.addresses,
            config.max_read_bytes,
            deadline,
            |address, element, bytes, data| {
                let connection = Connection::for_read(slot, config.address, deadline)?;
                connection.transaction += 1;
                let mut request = Vec::with_capacity(HEADER_BYTES + 18);
                request.extend(header(PCCC, connection.session));
                request.extend([0, config.lsap_control, 0, 0]);
                typed_request(
                    &mut request,
                    address,
                    element,
                    bytes,
                    connection.transaction,
                )?;
                let length = u16::try_from(request.len() - HEADER_BYTES)?;
                request[2..4].copy_from_slice(&length.to_be_bytes());
                connection.read_request(&request, bytes, deadline, data)
            },
        )?;
        remaining(deadline)?;
        Ok(readings)
    }
}

impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        if self.retired {
            return Err(Categorized::error(
                ErrorKind::Disconnected,
                "CSP source retired after a failed poll",
            ));
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("CSP timeout is too large".into()))?;
        let result = self.poll(deadline);
        if result.is_err() {
            self.connection = None;
            self.retired = true;
        }
        result
    }
}

struct Connection {
    stream: TcpStream,
    session: u32,
    transaction: u16,
}

impl Connection {
    fn for_read(
        slot: &mut Option<Self>,
        address: SocketAddr,
        deadline: Instant,
    ) -> Result<&mut Self> {
        if slot
            .as_ref()
            .is_some_and(|connection| connection.transaction == u16::MAX)
        {
            // TNS is unique within each TCP connection, including across polls.
            *slot = None;
        }
        if slot.is_none() {
            *slot = Some(Self::connect(address, deadline)?);
        }
        slot.as_mut().ok_or_else(|| protocol("session missing"))
    }

    fn connect(address: SocketAddr, deadline: Instant) -> Result<Self> {
        let mut stream = TcpStream::connect_timeout(&address, remaining(deadline)?)
            .map_err(|error| io_error(&error))?;
        stream.set_nodelay(true).map_err(|error| io_error(&error))?;
        let mut register = header(REGISTER, 0);
        register[12..].copy_from_slice(&REGISTER_CONTEXT);
        write(&mut stream, &register, deadline)?;
        let response = read_header(&mut stream, REGISTER, None, deadline)?;
        let session = session_id(&response);
        if response[2..4] != [0, 0] || session == 0 || response[12..] != REGISTER_CONTEXT {
            return Err(protocol("invalid registration length, session or context"));
        }
        Ok(Self {
            stream,
            session,
            transaction: 0,
        })
    }

    fn read_request(
        &mut self,
        request: &[u8],
        expected: usize,
        deadline: Instant,
        output: &mut Vec<u8>,
    ) -> Result<()> {
        write(&mut self.stream, request, deadline)?;
        let response = read_header(&mut self.stream, PCCC, Some(self.session), deadline)?;
        // Data context is opaque. Only registration validates its context.
        let length = usize::from(u16::from_be_bytes([response[2], response[3]]));
        if !(8..=MAX_BODY).contains(&length) {
            return Err(protocol("invalid PCCC reply length"));
        }
        let mut body = [0; MAX_BODY];
        let body = &mut body[..length];
        read(&mut self.stream, body, deadline)?;
        let [0, _, 0, 0, pccc @ ..] = body else {
            return Err(protocol("PCCC reply requires the direct local LSAP"));
        };
        output.extend_from_slice(typed_response(pccc, self.transaction, expected)?);
        Ok(())
    }
}

fn header(submode: u8, session: u32) -> [u8; HEADER_BYTES] {
    let mut bytes = [0; HEADER_BYTES];
    bytes[0] = 1;
    bytes[1] = submode;
    bytes[4..8].copy_from_slice(&session.to_be_bytes());
    bytes
}

fn session_id(header: &[u8; HEADER_BYTES]) -> u32 {
    u32::from_be_bytes([header[4], header[5], header[6], header[7]])
}

fn read_header(
    stream: &mut TcpStream,
    submode: u8,
    expected_session: Option<u32>,
    deadline: Instant,
) -> Result<[u8; HEADER_BYTES]> {
    let mut bytes = [0; HEADER_BYTES];
    read(stream, &mut bytes, deadline)?;
    if bytes[..2] != [2, submode] {
        return Err(protocol("unexpected reply mode or submode"));
    }
    if expected_session.is_some_and(|session| session_id(&bytes) != session) {
        return Err(protocol("PCCC reply session differs from registration"));
    }
    let status = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    if status != 0 {
        return Err(
            Categorized::new(ErrorKind::OperationFailed, "CSP request rejected")
                .details(json!({"csp_status":status}))
                .into(),
        );
    }
    Ok(bytes)
}

fn protocol(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, format!("CSP: {message}"))
}

#[cfg(test)]
mod tests {
    use super::{Client, Config, Connection, Source as _};
    use serde_json::json;
    use std::{
        io::{Read as _, Write as _},
        net::{TcpListener, TcpStream},
        thread,
        time::{Duration, Instant},
    };

    const REGISTER_REQUEST: [u8; 28] = [
        1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 0, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];

    fn accept(listener: &TcpListener) -> TcpStream {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    return stream;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "CSP peer never connected");
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("{error}"),
            }
        }
    }

    fn peer(stream: &mut TcpStream, session: u8, tns: [u8; 2], element: u8) {
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut register = [0; 28];
        stream.read_exact(&mut register).unwrap();
        assert_eq!(register, REGISTER_REQUEST);
        register[0] = 2;
        register[7] = session;
        stream.write_all(&register).unwrap();

        let mut request = [0; 42];
        stream.read_exact(&mut request).unwrap();
        assert_eq!(&request[..8], &[1, 7, 0, 14, 0, 0, 0, session]);
        assert_eq!(&request[8..32], &[0; 24]);
        assert_eq!(
            &request[32..],
            &[0x0f, 0, tns[0], tns[1], 0xa2, 2, 7, 0x89, element, 0]
        );
        let mut reply = request[..36].to_vec();
        reply[0] = 2;
        reply[3] = 10;
        reply[32] = 0x4f;
        reply.extend([element, 0]);
        stream.write_all(&reply).unwrap();
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn csp_uses_a_new_tcp_session_before_reusing_a_transaction() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = thread::spawn(move || {
            peer(&mut accept(&listener), 0x33, [0xff, 0xff], 0);
            peer(&mut accept(&listener), 0x44, [1, 0], 1);
        });
        let config: Config = serde_json::from_value(json!({
            "address": address, "signals": [
                {"name":"last","address":"N7:0"},
                {"name":"first","address":"N7:1"}
            ]
        }))
        .unwrap();
        let mut client = Client::new(&config).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut connection = Connection::connect(address, deadline).unwrap();
        connection.transaction = u16::MAX - 1;
        client.connection = Some(connection);
        let readings = client.read(Duration::from_secs(2)).unwrap();
        assert_eq!(readings[0].value, json!(0));
        assert_eq!(readings[1].value, json!(1));
        drop(client);
        server.join().unwrap();
    }
}
