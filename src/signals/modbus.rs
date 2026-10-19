//! Modbus TCP: a small synchronous MBAP client over `std::net` for function
//! codes 1 to 4 (read coils, discrete inputs, holding and input registers).
//! Nothing is ever written to a device.
use super::{
    Mode, Quality, Sample, TagInfo, Value,
    helper::Ctx,
    sanitize_number,
    uri::{ModbusTarget, Order, RegType, Target},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};

/// Largest MBAP length field a response may carry.
const MAX_LENGTH: usize = 260;

pub struct Client {
    stream: TcpStream,
    tid: u16,
}

impl Client {
    pub fn connect(host: &str, port: u16, timeout: Duration) -> Result<Self> {
        let addresses: Vec<_> = (host, port)
            .to_socket_addrs()
            .with_context(|| format!("resolve Modbus server {host}"))?
            .collect();
        let mut last = None;
        for address in addresses {
            match TcpStream::connect_timeout(&address, timeout) {
                Ok(stream) => {
                    stream.set_read_timeout(Some(timeout))?;
                    stream.set_write_timeout(Some(timeout))?;
                    stream.set_nodelay(true)?;
                    return Ok(Self { stream, tid: 0 });
                }
                Err(error) => last = Some(error),
            }
        }
        Err(last.map_or_else(
            || anyhow::anyhow!("Modbus server {host} has no address"),
            |e| anyhow::Error::new(e).context(format!("connect to Modbus server {host}:{port}")),
        ))
    }

    /// Read `quantity` bits or registers; returns the response's data bytes.
    pub fn read(&mut self, unit: u8, function: u8, address: u16, quantity: u16) -> Result<Vec<u8>> {
        self.tid = self.tid.wrapping_add(1);
        self.stream
            .write_all(&request(self.tid, unit, function, address, quantity))
            .map_err(|e| io_error(e, "send Modbus request"))?;
        let mut header = [0u8; 7];
        self.stream
            .read_exact(&mut header)
            .map_err(|e| io_error(e, "read Modbus response"))?;
        let length = u16::from_be_bytes([header[4], header[5]]) as usize;
        ensure!(
            (2..=MAX_LENGTH).contains(&length),
            "Modbus response length {length} is out of range"
        );
        let mut body = vec![0u8; length - 1];
        self.stream
            .read_exact(&mut body)
            .map_err(|e| io_error(e, "read Modbus response"))?;
        parse_response(self.tid, unit, function, quantity, &header, &body)
    }
}

/// Name socket failures so `cli::error_code` classifies them: a read
/// timeout surfaces as `WouldBlock` ("Resource temporarily unavailable") on
/// Unix, which would otherwise be a generic failure instead of a timeout.
fn io_error(error: std::io::Error, action: &str) -> anyhow::Error {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => {
            anyhow::anyhow!("{action}: Modbus server did not answer in time (timed out)")
        }
        ErrorKind::UnexpectedEof => {
            anyhow::anyhow!("{action}: Modbus server closed the connection")
        }
        _ => anyhow::Error::new(error).context(action.to_string()),
    }
}

pub fn request(tid: u16, unit: u8, function: u8, address: u16, quantity: u16) -> [u8; 12] {
    let mut frame = [0u8; 12];
    frame[0..2].copy_from_slice(&tid.to_be_bytes());
    // Protocol 0, then the length of unit + PDU.
    frame[4..6].copy_from_slice(&6u16.to_be_bytes());
    frame[6] = unit;
    frame[7] = function;
    frame[8..10].copy_from_slice(&address.to_be_bytes());
    frame[10..12].copy_from_slice(&quantity.to_be_bytes());
    frame
}

/// Check a response's header and PDU against its request.
pub fn parse_response(
    tid: u16,
    unit: u8,
    function: u8,
    quantity: u16,
    header: &[u8; 7],
    body: &[u8],
) -> Result<Vec<u8>> {
    let got_tid = u16::from_be_bytes([header[0], header[1]]);
    ensure!(
        got_tid == tid,
        "Modbus transaction ID mismatch (sent {tid}, got {got_tid})"
    );
    ensure!(
        header[2] == 0 && header[3] == 0,
        "Modbus response has a nonzero protocol ID"
    );
    ensure!(
        header[6] == unit,
        "Modbus response came from unit {} instead of {unit}",
        header[6]
    );
    let (&fc, rest) = body.split_first().context("Modbus response is empty")?;
    // Some servers and gateways answer an unknown unit with function 0x80
    // rather than `function | 0x80`; report any exception's code.
    if fc & 0x80 != 0 {
        let code = rest.first().copied().unwrap_or(0);
        bail!("modbus exception {code} ({})", exception_name(code));
    }
    ensure!(
        fc == function,
        "Modbus response has function {fc} instead of {function}"
    );
    let (&count, data) = rest.split_first().context("Modbus response is short")?;
    let expected = if function <= 2 {
        (quantity as usize).div_ceil(8)
    } else {
        quantity as usize * 2
    };
    ensure!(
        count as usize == expected && data.len() == expected,
        "Modbus response has {} data bytes; expected {expected}",
        data.len()
    );
    Ok(data.to_vec())
}

fn exception_name(code: u8) -> &'static str {
    match code {
        1 => "illegal function",
        2 => "illegal data address",
        3 => "illegal data value",
        4 => "server device failure",
        5 => "acknowledge",
        6 => "server device busy",
        10 => "gateway path unavailable",
        11 => "gateway target device failed to respond",
        _ => "unknown exception",
    }
}

/// Put one value's wire bytes into big-endian (ABCD...) order.
fn reorder(bytes: &[u8], order: Order) -> Vec<u8> {
    let mut words: Vec<[u8; 2]> = bytes.as_chunks::<2>().0.to_vec();
    if matches!(order, Order::Cdab | Order::Dcba) {
        words.reverse();
    }
    if matches!(order, Order::Badc | Order::Dcba) {
        for word in &mut words {
            word.swap(0, 1);
        }
    }
    words.concat()
}

/// Turn a response's data bytes into values, with scale and offset applied.
pub fn decode(target: &ModbusTarget, data: &[u8]) -> Vec<(String, Value, Quality)> {
    let count = target.count as usize;
    if target.table.is_bits() {
        return (0..count)
            .map(|i| {
                let bit = data.get(i / 8).is_some_and(|b| b >> (i % 8) & 1 == 1);
                (target.tag(i), Value::Bool(bit), Quality::Good)
            })
            .collect();
    }
    let size = target.kind.words() * 2;
    data.chunks_exact(size)
        .take(count)
        .enumerate()
        .map(|(i, raw)| {
            let b = reorder(raw, target.order);
            let tag = target.tag(i);
            let x = match target.kind {
                RegType::Bool => {
                    return (
                        tag,
                        Value::Bool(u16::from_be_bytes([b[0], b[1]]) != 0),
                        Quality::Good,
                    );
                }
                RegType::U16 => u16::from_be_bytes([b[0], b[1]]) as f64,
                RegType::I16 => i16::from_be_bytes([b[0], b[1]]) as f64,
                RegType::U32 => u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64,
                RegType::I32 => i32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64,
                RegType::F32 => f32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64,
                RegType::F64 => f64::from_be_bytes(b[..8].try_into().unwrap_or_default()),
            };
            let (value, quality) = sanitize_number(x * target.scale + target.offset);
            (tag, value, quality)
        })
        .collect()
}

fn target(ctx: &Ctx) -> &ModbusTarget {
    match &ctx.uri.target {
        Target::Modbus(target) => target,
        _ => unreachable!(),
    }
}

fn poll(ctx: &Ctx, client: &mut Client) -> Result<()> {
    let t = target(ctx);
    let data = client.read(t.unit_id, t.table.function(), t.address, t.span() as u16)?;
    for (tag, value, quality) in decode(t, &data) {
        ctx.sample(Sample::now(&tag, value, quality));
    }
    Ok(())
}

pub fn run(ctx: &Ctx) -> Result<bool> {
    let t = target(ctx);
    let kind = if t.kind == RegType::Bool {
        "bool"
    } else {
        "number"
    };
    let tags = (0..t.count as usize)
        .map(|i| TagInfo {
            tag: t.tag(i),
            unit: t.unit.clone(),
            kind: Some(kind.into()),
        })
        .collect();
    let connect = || Client::connect(&ctx.uri.host, ctx.uri.port, ctx.timeout());
    match ctx.mode() {
        Mode::Browse { .. } => bail!("browse is not supported for modbus-tcp sources"),
        Mode::Read => {
            let mut client = connect()?;
            ctx.ready(tags);
            poll(ctx, &mut client)?;
        }
        Mode::Log => {
            ctx.ready(tags);
            ctx.poll_loop(connect, |client| poll(ctx, client))?;
        }
    }
    Ok(false)
}

/// A loopback Modbus TCP server for tests: holding and input registers,
/// coils and discrete inputs, and a switch that drops connections.
#[doc(hidden)]
pub mod fake {
    use std::{
        collections::BTreeMap,
        io::{Read, Write},
        net::{SocketAddr, TcpListener, TcpStream},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread,
    };

    #[derive(Default)]
    struct State {
        registers: BTreeMap<(u8, u16), u16>,
        bits: BTreeMap<(u8, u16), bool>,
    }

    #[derive(Clone)]
    pub struct Fake {
        state: Arc<Mutex<State>>,
        drop_connections: Arc<AtomicBool>,
        address: SocketAddr,
    }

    /// Serve on `listener` from a background thread.
    pub fn serve(listener: TcpListener) -> Fake {
        let fake = Fake {
            state: Arc::default(),
            drop_connections: Arc::default(),
            address: listener.local_addr().expect("fake Modbus address"),
        };
        let server = fake.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let server = server.clone();
                thread::spawn(move || server.handle(stream));
            }
        });
        fake
    }

    impl Fake {
        pub fn address(&self) -> SocketAddr {
            self.address
        }
        /// Set holding (function 3) or input (function 4) registers.
        pub fn set_registers(&self, function: u8, address: u16, values: &[u16]) {
            let mut state = self.state.lock().unwrap();
            for (i, value) in values.iter().enumerate() {
                state
                    .registers
                    .insert((function, address + i as u16), *value);
            }
        }
        /// Set coils (function 1) or discrete inputs (function 2).
        pub fn set_bits(&self, function: u8, address: u16, values: &[bool]) {
            let mut state = self.state.lock().unwrap();
            for (i, value) in values.iter().enumerate() {
                state.bits.insert((function, address + i as u16), *value);
            }
        }
        /// Close every connection at its next request.
        pub fn drop_connections(&self, enabled: bool) {
            self.drop_connections.store(enabled, Ordering::SeqCst);
        }
        fn handle(&self, mut stream: TcpStream) {
            let mut frame = [0u8; 12];
            while stream.read_exact(&mut frame).is_ok() {
                if self.drop_connections.load(Ordering::SeqCst) {
                    return;
                }
                let function = frame[7];
                let address = u16::from_be_bytes([frame[8], frame[9]]);
                let quantity = u16::from_be_bytes([frame[10], frame[11]]);
                let pdu = {
                    let state = self.state.lock().unwrap();
                    match function {
                        1 | 2 => {
                            let mut data = vec![0u8; (quantity as usize).div_ceil(8)];
                            for i in 0..quantity as usize {
                                let bit = state
                                    .bits
                                    .get(&(function, address.wrapping_add(i as u16)))
                                    .copied()
                                    .unwrap_or(false);
                                if bit {
                                    data[i / 8] |= 1 << (i % 8);
                                }
                            }
                            [vec![function, data.len() as u8], data].concat()
                        }
                        3 | 4 => {
                            let mut pdu = vec![function, (quantity * 2) as u8];
                            for i in 0..quantity {
                                let value = state
                                    .registers
                                    .get(&(function, address.wrapping_add(i)))
                                    .copied()
                                    .unwrap_or(0);
                                pdu.extend_from_slice(&value.to_be_bytes());
                            }
                            pdu
                        }
                        _ => vec![function | 0x80, 1],
                    }
                };
                let mut reply = Vec::with_capacity(7 + pdu.len());
                reply.extend_from_slice(&frame[0..4]);
                reply.extend_from_slice(&(pdu.len() as u16 + 1).to_be_bytes());
                reply.push(frame[6]);
                reply.extend_from_slice(&pdu);
                if stream.write_all(&reply).is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signals::parse;

    fn target(uri: &str) -> ModbusTarget {
        match parse(uri).unwrap().target {
            Target::Modbus(t) => t,
            _ => panic!(),
        }
    }

    #[test]
    fn mbap_round_trip() {
        let frame = request(0x1234, 7, 3, 100, 2);
        assert_eq!(frame, [0x12, 0x34, 0, 0, 0, 6, 7, 3, 0, 100, 0, 2]);
        let header = [0x12, 0x34, 0, 0, 0, 7, 7];
        let body = [3, 4, 0, 1, 0, 2];
        assert_eq!(
            parse_response(0x1234, 7, 3, 2, &header, &body).unwrap(),
            vec![0, 1, 0, 2]
        );
        // Coils: 10 bits pack into 2 bytes.
        let header = [0, 1, 0, 0, 0, 5, 1];
        assert_eq!(
            parse_response(1, 1, 1, 10, &header, &[1, 2, 0xff, 0x03]).unwrap(),
            vec![0xff, 0x03]
        );
        let error = parse_response(1, 1, 3, 1, &[0, 1, 0, 0, 0, 3, 1], &[0x83, 2]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "modbus exception 2 (illegal data address)"
        );
        // pymodbus answers an unknown unit ID with function 0x80, code 11.
        let error = parse_response(1, 9, 3, 1, &[0, 1, 0, 0, 0, 3, 9], &[0x80, 11]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "modbus exception 11 (gateway target device failed to respond)"
        );
        let error = parse_response(2, 1, 3, 1, &header, &[3, 2, 0, 0]).unwrap_err();
        assert!(error.to_string().contains("transaction ID mismatch"));
        assert!(parse_response(1, 1, 3, 2, &[0, 1, 0, 0, 0, 5, 1], &[3, 2, 0, 0]).is_err());
    }

    #[test]
    fn word_orders() {
        let orders: [(&str, [u8; 4]); 4] = [
            ("abcd", [0x42, 0xF6, 0xE9, 0x79]),
            ("cdab", [0xE9, 0x79, 0x42, 0xF6]),
            ("badc", [0xF6, 0x42, 0x79, 0xE9]),
            ("dcba", [0x79, 0xE9, 0xF6, 0x42]),
        ];
        for (order, bytes) in orders {
            let t = target(&format!(
                "modbus-tcp://h/1/holding/0?type=f32&order={order}"
            ));
            let (_, value, _) = decode(&t, &bytes).remove(0);
            let Value::Number(x) = value else { panic!() };
            assert!((x - 123.456).abs() < 1e-4, "{order}: {x}");
            let t = target(&format!(
                "modbus-tcp://h/1/holding/0?type=u32&order={order}"
            ));
            assert_eq!(decode(&t, &bytes)[0].1, Value::Number(0x42F6E979u32 as f64));
            let t = target(&format!(
                "modbus-tcp://h/1/holding/0?type=i32&order={order}"
            ));
            assert_eq!(decode(&t, &bytes)[0].1, Value::Number(0x42F6E979i32 as f64));
        }
        let wire = 1.5f64.to_be_bytes();
        let mut cdab = Vec::new();
        for word in wire.chunks(2).rev() {
            cdab.extend_from_slice(word);
        }
        let t = target("modbus-tcp://h/1/holding/0?type=f64&order=cdab");
        assert_eq!(decode(&t, &cdab)[0].1, Value::Number(1.5));
        let t = target("modbus-tcp://h/1/input/0?type=i16&count=2&scale=0.1&offset=1&name=temp");
        let values = decode(&t, &[0xff, 0xf6, 0x00, 0x0a]);
        assert_eq!(values[0].0, "temp[0]");
        assert!(matches!(values[0].1, Value::Number(x) if (x - 0.0).abs() < 1e-9));
        assert!(matches!(values[1].1, Value::Number(x) if (x - 2.0).abs() < 1e-9));
    }

    #[test]
    fn fake_server_round_trip_and_reconnect() {
        let fake = fake::serve(std::net::TcpListener::bind("127.0.0.1:0").unwrap());
        fake.set_registers(3, 10, &[0xE979, 0x42F6]);
        fake.set_bits(1, 0, &[true, false, true]);
        let port = fake.address().port();
        let mut client = Client::connect("127.0.0.1", port, Duration::from_secs(2)).unwrap();
        let t = target(&format!(
            "modbus-tcp://127.0.0.1:{port}/1/holding/10?type=f32&order=cdab&scale=2"
        ));
        let data = client.read(1, 3, 10, 2).unwrap();
        let Value::Number(x) = decode(&t, &data)[0].1.clone() else {
            panic!()
        };
        assert!((x - 246.912).abs() < 1e-3);
        let t = target(&format!("modbus-tcp://127.0.0.1:{port}/1/coil/0?count=3"));
        let data = client.read(1, 1, 0, 3).unwrap();
        let bits: Vec<_> = decode(&t, &data).into_iter().map(|v| v.1).collect();
        assert_eq!(
            bits,
            [Value::Bool(true), Value::Bool(false), Value::Bool(true)]
        );
        let error = client.read(1, 5, 0, 1).unwrap_err();
        assert!(error.to_string().contains("illegal function"));
        fake.drop_connections(true);
        assert!(client.read(1, 3, 10, 2).is_err());
        fake.drop_connections(false);
        let mut client = Client::connect("127.0.0.1", port, Duration::from_secs(2)).unwrap();
        assert_eq!(client.read(1, 3, 10, 1).unwrap(), vec![0xE9, 0x79]);
    }

    #[test]
    fn silent_server_times_out_and_closed_connection_is_named() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept = std::thread::spawn(move || {
            let (silent, _) = listener.accept().unwrap();
            let (mut closed, _) = listener.accept().unwrap();
            let mut frame = [0u8; 12];
            closed.read_exact(&mut frame).unwrap();
            drop(closed);
            std::thread::sleep(Duration::from_millis(800));
            drop(silent);
        });
        let mut client = Client::connect("127.0.0.1", port, Duration::from_millis(300)).unwrap();
        let error = format!("{:#}", client.read(1, 3, 0, 1).unwrap_err());
        assert!(error.contains("timed out"), "{error}");
        assert_eq!(crate::cli::error_code(&anyhow::anyhow!(error)).1, 4);
        let mut client = Client::connect("127.0.0.1", port, Duration::from_millis(300)).unwrap();
        let error = format!("{:#}", client.read(1, 3, 0, 1).unwrap_err());
        assert!(error.contains("closed the connection"), "{error}");
        accept.join().unwrap();
    }

    #[test]
    fn closed_port_is_refused() {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let error = Client::connect("127.0.0.1", port, Duration::from_secs(2))
            .err()
            .unwrap();
        assert!(
            format!("{error:#}")
                .to_ascii_lowercase()
                .contains("refused")
        );
    }
}
