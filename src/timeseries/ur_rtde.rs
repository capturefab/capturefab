//! Output-only Universal Robots RTDE v2 telemetry with original receive clocks.
//!
//! Output synchronization commands never start, pause or write a robot program.
use super::{Reading, Source, net};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io,
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

mod parser;
const MAX_PENDING: usize = 32;
const MAX_RAW: usize = 64 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    profile: Profile,
    address: SocketAddr,
    #[serde(default = "frequency")]
    frequency_hz: f64,
    #[serde(default = "signal")]
    signal: String,
    fields: Vec<parser::Field>,
}
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Profile {
    RtdeV2,
}
const fn frequency() -> f64 {
    125.0
}
fn signal() -> String {
    "state".into()
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    let Profile::RtdeV2 = config.profile;
    if config.address.port() == 0
        || config.address.ip().is_unspecified()
        || config.address.ip().is_multicast()
        || !config.frequency_hz.is_finite()
        || !(1.0..=500.0).contains(&config.frequency_hz)
    {
        return Err(Usage(
            "RTDE requires a numeric unicast endpoint and finite requested frequency 1–500 Hz"
                .into(),
        )
        .into());
    }
    super::validate_name(&config.signal, "RTDE signal")?;
    Ok(Box::new(Client {
        config: config.clone(),
        plan: parser::Plan::new(&config.fields)?,
        owner: None,
        retired: false,
        started: false,
        version: 1,
        software: None,
        recipe: None,
        pending: VecDeque::new(),
        pending_bytes: 0,
        output_index: 0,
        observed: None,
        cutoff: None,
    }))
}
struct Received {
    bytes: Vec<u8>,
    observed: u64,
    version: u16,
    output_index: Option<u64>,
}
struct Client {
    config: Config,
    plan: parser::Plan,
    owner: Option<Box<dyn net::Transport>>,
    retired: bool,
    started: bool,
    version: u16,
    software: Option<Value>,
    recipe: Option<u8>,
    pending: VecDeque<Received>,
    pending_bytes: usize,
    output_index: u64,
    observed: Option<u64>,
    cutoff: Option<u64>,
}
#[derive(Clone, Copy)]
enum WaitFor {
    Version,
    Software,
    Recipe,
    Start,
    Pause,
}
impl WaitFor {
    const fn command(self) -> u8 {
        match self {
            Self::Version => b'V',
            Self::Software => b'v',
            Self::Recipe => b'O',
            Self::Start => b'S',
            Self::Pause => b'P',
        }
    }
}
impl Client {
    fn send(&mut self, command: u8, payload: &[u8], deadline: Instant) -> Result<()> {
        remaining(deadline)?;
        let packet = parser::packet(command, payload)?;
        let owner = self.owner.as_deref_mut().ok_or_else(closed)?;
        owner.set_deadline(deadline);
        owner.write_all(&packet).map_err(|error| io_error(&error))?;
        remaining(deadline)?;
        Ok(())
    }
    fn receive(&mut self, deadline: Instant, idle: bool) -> Result<Option<Received>> {
        let owner = self.owner.as_deref_mut().ok_or_else(closed)?;
        owner.set_deadline(deadline);
        let mut header = [0; 3];
        if !fill(|bytes| owner.read(bytes), &mut header, deadline, idle)? {
            return Ok(None);
        }
        let length = parser::frame_bytes(header)?;
        let mut bytes = vec![0; length];
        bytes
            .get_mut(..3)
            .ok_or_else(|| malformed("header extent"))?
            .copy_from_slice(&header);
        fill(
            |target| owner.read(target),
            bytes.get_mut(3..).ok_or_else(|| malformed("body extent"))?,
            deadline,
            false,
        )?;
        let observed = receive_clock(crate::host::monotonic_ns())?;
        remaining(deadline)?;
        Ok(Some(Received {
            bytes,
            observed,
            version: self.version,
            output_index: None,
        }))
    }
    fn eligible(&self, received: &Received) -> bool {
        self.cutoff.is_none_or(|cutoff| received.observed <= cutoff)
    }
    fn stash(&mut self, received: Received) -> Result<()> {
        if !self.eligible(&received) {
            return Ok(());
        }
        let size = self
            .pending_bytes
            .checked_add(received.bytes.len())
            .filter(|&size| size <= MAX_RAW)
            .ok_or_else(|| malformed("pending raw queue exceeds 64 KiB"))?;
        if self.pending.len() >= MAX_PENDING {
            return Err(malformed("pending packet count exceeds 32"));
        }
        self.pending.push_back(received);
        self.pending_bytes = size;
        Ok(())
    }
    fn text(received: &Received) -> Result<Value> {
        let (_, payload) = parser::payload(&received.bytes)?;
        let mut text = parser::text(payload, received.version)?;
        text["raw"] = parser::raw(&received.bytes);
        if matches!(text["level_raw"].as_u64(), Some(0 | 1)) {
            return Err(Categorized::new(
                ErrorKind::OperationFailed,
                "RTDE controller reported an exception/error",
            )
            .details(json!({"ur_rtde_text":text}))
            .into());
        }
        Ok(text)
    }
    fn check_output(&mut self, received: &mut Received) -> Result<()> {
        let (_, payload) = parser::payload(&received.bytes)?;
        self.plan.check_data(
            self.recipe
                .ok_or_else(|| malformed("output before recipe setup"))?,
            payload,
        )?;
        received.output_index = Some(self.output_index);
        self.output_index = self
            .output_index
            .checked_add(1)
            .ok_or_else(|| malformed("host output index overflow"))?;
        Ok(())
    }
    fn wait(&mut self, expected: WaitFor, deadline: Instant) -> Result<Vec<u8>> {
        let mut count = 0_usize;
        let mut bytes = 0_usize;
        loop {
            let mut received = self
                .receive(deadline, false)?
                .ok_or_else(|| malformed("idle control reply"))?;
            let (command, payload) = parser::payload(&received.bytes)?;
            if command == expected.command() {
                return Ok(payload.to_vec());
            }
            count += 1;
            bytes = bytes
                .checked_add(received.bytes.len())
                .filter(|&bytes| bytes <= MAX_RAW)
                .ok_or_else(|| malformed("asynchronous control bytes exceed 64 KiB"))?;
            if count > MAX_PENDING {
                return Err(malformed("asynchronous control packet count exceeds 32"));
            }
            match command {
                b'M' => {
                    Self::text(&received)?;
                }
                b'U' if matches!(expected, WaitFor::Start | WaitFor::Pause) => {
                    self.check_output(&mut received)?;
                }
                _ => return Err(malformed("unexpected packet while awaiting control reply")),
            }
            if !matches!(expected, WaitFor::Pause) {
                self.stash(received)?;
            }
            remaining(deadline)?;
        }
    }
    fn setup(&mut self, deadline: Instant) -> Result<()> {
        remaining(deadline)?;
        let stream = TcpStream::connect_timeout(&self.config.address, remaining(deadline)?)
            .map_err(|error| io_error(&error))?;
        stream.set_nodelay(true).map_err(|error| io_error(&error))?;
        remaining(deadline)?;
        self.owner = Some(net::plain(stream, deadline));
        self.send(b'V', &2_u16.to_be_bytes(), deadline)?;
        match self.wait(WaitFor::Version, deadline)?.as_slice() {
            [1] => self.version = 2,
            [0] => {
                return Err(Categorized::error(
                    ErrorKind::Unsupported,
                    "RTDE controller rejected protocol version 2",
                ));
            }
            _ => {
                return Err(malformed(
                    "protocol version acknowledgement requires exact 0/1",
                ));
            }
        }
        self.send(b'v', &[], deadline)?;
        self.software = Some(parser::software(&self.wait(WaitFor::Software, deadline)?)?);
        let packet = self.plan.setup(self.config.frequency_hz)?;
        let (_, payload) = parser::payload(&packet)?;
        self.send(b'O', payload, deadline)?;
        let payload = self.wait(WaitFor::Recipe, deadline)?;
        self.recipe = Some(self.plan.accept_recipe(&payload)?);
        self.send(b'S', &[], deadline)?;
        if self.wait(WaitFor::Start, deadline)?.as_slice() != [1] {
            return Err(malformed(
                "controller did not accept output synchronization start",
            ));
        }
        remaining(deadline)?;
        self.started = true;
        Ok(())
    }
    fn next(&mut self, deadline: Option<Instant>) -> Result<Option<Reading>> {
        while self
            .pending
            .front()
            .is_some_and(|received| !self.eligible(received))
        {
            if let Some(received) = self.pending.pop_front() {
                self.pending_bytes -= received.bytes.len();
            }
        }
        let Some(received) = self.pending.front() else {
            return Ok(None);
        };
        let (command, payload) = parser::payload(&received.bytes)?;
        let mut value = match command {
            b'U' => {
                json!({"kind":"ur_rtde_output","recipe_id":self.recipe.ok_or_else(|| malformed("missing output recipe"))?,
                "requested_frequency_hz":self.config.frequency_hz,
                "host_output_index":received.output_index.ok_or_else(|| malformed("missing host output index"))?,
                "fields":self.plan.decode_data(self.recipe.ok_or_else(|| malformed("missing output recipe"))?,payload)?})
            }
            b'M' => {
                let mut text = Self::text(received)?;
                text["kind"] = json!("ur_rtde_text");
                text
            }
            _ => return Err(malformed("unexpected queued packet")),
        };
        value["protocol_version"] = json!(2);
        value["controller_software"] = self
            .software
            .clone()
            .ok_or_else(|| malformed("missing controller software metadata"))?;
        value["raw"] = parser::raw(&received.bytes);
        let reading = Reading {
            signal: self.config.signal.clone(),
            value,
            source_time: None,
        };
        let mut limit = super::slmp::SizeLimit { bytes: 2 };
        serde_json::to_writer(&mut limit, &reading)
            .map_err(|_| malformed("serialized observation exceeds 1 MiB"))?;
        if let Some(deadline) = deadline {
            remaining(deadline)?;
        }
        self.observed = Some(received.observed);
        let removed = self
            .pending
            .pop_front()
            .ok_or_else(|| malformed("queued observation vanished"))?;
        self.pending_bytes -= removed.bytes.len();
        Ok(Some(reading))
    }
    fn read_until(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        if !self.started {
            self.setup(deadline)?;
        }
        if let Some(reading) = self.next(Some(deadline))? {
            return Ok(vec![reading]);
        }
        let Some(mut received) = self.receive(deadline, true)? else {
            return Ok(Vec::new());
        };
        if !self.eligible(&received) {
            return Ok(Vec::new());
        }
        match parser::payload(&received.bytes)?.0 {
            b'U' => self.check_output(&mut received)?,
            b'M' => {
                Self::text(&received)?;
            }
            _ => return Err(malformed("unexpected packet in output stream")),
        }
        self.stash(received)?;
        let reading = self
            .next(Some(deadline))?
            .ok_or_else(|| malformed("missing received observation"))?;
        Ok(vec![reading])
    }
}
impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        if self.retired {
            return Err(closed());
        }
        let result = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("RTDE timeout too large".into()).into())
            .and_then(|deadline| self.read_until(deadline));
        if result.is_err() {
            self.owner = None;
            self.retired = true;
            if !self.started {
                self.pending.clear();
                self.pending_bytes = 0;
            }
        }
        result
    }
    fn ready(&self) -> bool {
        self.started
    }
    fn observed_ns(&self) -> Option<u64> {
        self.observed
    }
    fn buffered_timestamps(&self) -> bool {
        true
    }
    fn set_acquisition_end(&mut self, end_ns: Option<u64>) {
        self.cutoff = end_ns;
    }
    fn finish(&mut self, timeout: Duration) -> Result<()> {
        // An interruption can finish earlier than the configured acquisition
        // end. Cleanup never extends acquisition by reading toward its ACK.
        let result = (|| {
            if self.retired || !self.started {
                return Ok(());
            }
            let now = receive_clock(crate::host::monotonic_ns())?;
            self.cutoff = Some(self.cutoff.map_or(now, |end| end.min(now)));
            let deadline = Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| Usage("RTDE finish timeout too large".into()))?;
            self.send(b'P', &[], deadline)?;
            if self.wait(WaitFor::Pause, deadline)?.as_slice() != [1] {
                return Err(malformed("controller did not accept output pause"));
            }
            remaining(deadline)?;
            Ok(())
        })();
        self.owner = None;
        self.retired = true;
        result
    }
    fn drain(&mut self) -> Result<Option<Reading>> {
        if !self.started {
            return Ok(None);
        }
        self.next(None)
    }
}
fn fill(
    mut read: impl FnMut(&mut [u8]) -> io::Result<usize>,
    bytes: &mut [u8],
    deadline: Instant,
    idle: bool,
) -> Result<bool> {
    let mut count = 0;
    while count < bytes.len() {
        if Instant::now() >= deadline {
            if count == 0 && idle {
                return Ok(false);
            }
            remaining(deadline)?;
        }
        let buffer = bytes
            .get_mut(count..)
            .ok_or_else(|| malformed("invalid receive extent"))?;
        match read(buffer) {
            Ok(0) => {
                return Err(Categorized::error(
                    ErrorKind::Disconnected,
                    "RTDE peer closed during packet",
                ));
            }
            Ok(received) => {
                count += received;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error)
                if count == 0
                    && idle
                    && matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(io_error(&error)),
        }
        remaining(deadline)?;
    }
    Ok(true)
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let duration = deadline.saturating_duration_since(Instant::now());
    if duration.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "RTDE deadline expired",
        ));
    }
    Ok(duration)
}
fn receive_clock(observed: Option<u64>) -> Result<u64> {
    observed.ok_or_else(|| {
        Categorized::error(
            ErrorKind::OperationFailed,
            "RTDE original host receive clock unavailable",
        )
    })
}
fn malformed(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, format!("RTDE: {message}"))
}
fn closed() -> anyhow::Error {
    Categorized::error(ErrorKind::Disconnected, "RTDE output owner retired")
}
fn io_error(error: &io::Error) -> anyhow::Error {
    let kind = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::Disconnected,
    };
    Categorized::error(kind, format!("RTDE I/O: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    const OUTPUT:&[u8]=b"\x00\x18U\x07\x3f\xf8\x00\x00\x00\x00\x00\x00\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff";
    struct Memory {
        input: io::Cursor<Vec<u8>>,
        sent: Arc<Mutex<Vec<u8>>>,
    }
    impl io::Read for Memory {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            io::Read::read(&mut self.input, bytes)
        }
    }
    impl io::Write for Memory {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.sent
                .lock()
                .map_err(|_| io::Error::other("memory sink poisoned"))?
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl net::Transport for Memory {
        fn set_deadline(&mut self, _deadline: Instant) {}
    }
    fn config() -> Config {
        serde_json::from_value(
            json!({"profile":"rtde_v2","address":"127.0.0.1:9","fields":[
            {"field":"timestamp","wire_type":"DOUBLE"},{"field":"robot_mode","wire_type":"INT32"},
            {"field":"actual_digital_input_bits","wire_type":"UINT64"}]}),
        )
        .unwrap()
    }
    fn memory_client(input: &[u8]) -> (Client, Arc<Mutex<Vec<u8>>>) {
        let config = config();
        let plan = parser::Plan::new(&config.fields).unwrap();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let client = Client {
            config,
            plan,
            owner: Some(Box::new(Memory {
                input: io::Cursor::new(input.to_vec()),
                sent: sent.clone(),
            })),
            retired: false,
            started: true,
            version: 2,
            software: Some(json!({"major":5,"minor":25,"bugfix":0,"build":1234})),
            recipe: Some(7),
            pending: VecDeque::new(),
            pending_bytes: 0,
            output_index: 1,
            observed: None,
            cutoff: None,
        };
        (client, sent)
    }
    fn pending(client: &mut Client) {
        client
            .stash(Received {
                bytes: OUTPUT.to_vec(),
                observed: 17,
                version: 2,
                output_index: Some(0),
            })
            .unwrap();
    }
    #[test]
    fn accepted_pending_frames_survive_expired_read_and_failed_cleanup() {
        let (mut client, sent) = memory_client(&[]);
        pending(&mut client);
        let error = client.read(Duration::ZERO).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
        assert_eq!(client.pending.len(), 1);
        assert_eq!(client.pending_bytes, OUTPUT.len());
        assert!(client.retired);
        client.finish(Duration::ZERO).unwrap();
        let reading = client.drain().unwrap().unwrap();
        assert_eq!(reading.value["host_output_index"], 0);
        assert_eq!(reading.value["fields"][2]["value"], u64::MAX);
        assert_eq!(client.observed_ns(), Some(17));
        assert!(client.drain().unwrap().is_none());
        assert_eq!(*sent.lock().unwrap(), Vec::<u8>::new());
        let error = client.read(Duration::from_secs(1)).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Disconnected);
    }
    #[test]
    fn pause_drops_new_receipts_but_retains_earlier_queue_and_original_clock() {
        for failure in [false, true] {
            let mut input = OUTPUT.to_vec();
            input.extend_from_slice(if failure {
                b"\x00\x06M\x00\x00\x01"
            } else {
                b"\x00\x04P\x01"
            });
            let (mut client, sent) = memory_client(&input);
            pending(&mut client);
            let result = client.finish(Duration::from_secs(1));
            if failure {
                assert_eq!(
                    crate::types::classify(&result.unwrap_err()).kind,
                    ErrorKind::OperationFailed
                );
            } else {
                result.unwrap();
            }
            assert_eq!(*sent.lock().unwrap(), b"\x00\x03P");
            assert!(client.owner.is_none());
            assert_eq!(client.pending.len(), 1);
            let reading = client.drain().unwrap().unwrap();
            assert_eq!(reading.value["host_output_index"], 0);
            assert_eq!(client.observed_ns(), Some(17));
            assert!(client.drain().unwrap().is_none());
        }
    }
    #[test]
    fn clocks_and_partial_receive_fail_without_substituting_delivery_or_idle() {
        let error = receive_clock(None).unwrap_err();
        assert_eq!(
            crate::types::classify(&error).kind,
            ErrorKind::OperationFailed
        );
        assert_eq!(receive_clock(Some(0)).unwrap(), 0);
        let mut output = [0; 3];
        assert!(
            !fill(
                |_| Err(io::Error::from(io::ErrorKind::TimedOut)),
                &mut output,
                Instant::now() + Duration::from_secs(1),
                true
            )
            .unwrap()
        );
        let mut first = true;
        let error = fill(
            |buffer| {
                if first {
                    first = false;
                    let [byte] = buffer
                        .first_chunk_mut::<1>()
                        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
                    *byte = 0;
                    Ok(1)
                } else {
                    Err(io::Error::from(io::ErrorKind::TimedOut))
                }
            },
            &mut output,
            Instant::now() + Duration::from_secs(1),
            true,
        )
        .unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
        let mut source = open(&config()).unwrap();
        source.read(Duration::ZERO).unwrap_err();
        assert_eq!(
            crate::types::classify(&source.read(Duration::from_secs(1)).unwrap_err()).kind,
            ErrorKind::Disconnected
        );
    }
    #[test]
    fn asynchronous_control_wait_and_pending_queue_have_count_and_byte_caps() {
        let warning = b"\x00\x07M\x02old";
        let mut input = warning.repeat(33);
        input.extend(b"\x00\x04V\x01");
        let (mut client, _sent) = memory_client(&input);
        client.version = 1;
        let error = client
            .wait(WaitFor::Version, Instant::now() + Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(
            crate::types::classify(&error).kind,
            ErrorKind::OperationFailed
        );
        assert_eq!(client.pending.len(), 32);
        let (mut client, _sent) = memory_client(&[]);
        for _ in 0..8 {
            client
                .stash(Received {
                    bytes: vec![0; 8192],
                    observed: 17,
                    version: 2,
                    output_index: None,
                })
                .unwrap();
        }
        client
            .stash(Received {
                bytes: vec![0],
                observed: 17,
                version: 2,
                output_index: None,
            })
            .unwrap_err();
        assert_eq!(client.pending_bytes, MAX_RAW);
    }
}
