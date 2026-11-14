//! Independent testing-only USB producer. Never imports the product codec.
use super::{
    Config,
    native::{Backend, Completion, ControlRequest, Direction, Opened},
    wire::Endpoint,
};
use crate::types::{Categorized, ErrorKind};
use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    env,
    fs::{File, OpenOptions},
    io::{Read as _, Write as _},
    thread,
    time::{Duration, Instant},
};

const WIRE: &str = include_str!("../../../../tests/fixtures/usbtmc/wire.json");
const SELECTOR: &str = include_str!("../../../../tests/fixtures/usbtmc/selector-control.json");

fn failure(kind: ErrorKind, message: &'static str) -> anyhow::Error {
    Categorized::error(kind, message)
}
fn check(until: Instant) -> Result<()> {
    if Instant::now() >= until {
        return Err(failure(
            ErrorKind::Timeout,
            "fixture original deadline expired",
        ));
    }
    Ok(())
}
fn now() -> Result<u64> {
    crate::host::monotonic_ns().context("fixture monotonic clock unavailable")
}
fn unhex(value: &str) -> Result<Vec<u8>> {
    ensure!(value.len().is_multiple_of(2), "fixture odd hex");
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| Ok(u8::from_str_radix(std::str::from_utf8(pair)?, 16)?))
        .collect()
}
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("write fixture hex to String");
    }
    output
}

struct Trace {
    file: File,
    bytes: usize,
    calls: usize,
}
impl Trace {
    fn new() -> Result<Self> {
        let path = env::var_os("CAPTUREFAB_USBTMC_TRACE").context("fixture trace path missing")?;
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        Ok(Self {
            file,
            bytes: 0,
            calls: 0,
        })
    }
    fn event(&mut self, operation: &str, details: Value) -> Result<()> {
        let mut value = json!({"pid":std::process::id(),"operation":operation,"clock_ns":now()?});
        value["details"] = details;
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        self.calls += 1;
        self.bytes += bytes.len();
        ensure!(
            self.calls <= 4096 && self.bytes <= 1_048_576,
            "fixture trace bound exceeded"
        );
        self.file.write_all(&bytes)?;
        self.file.flush()?;
        Ok(())
    }
}

struct ManualUsb {
    case: String,
    trace: Trace,
    owned: bool,
    pending_out: Option<Vec<u8>>,
    pending_in: Option<usize>,
    cancelled_out: bool,
    cancelled_in: bool,
    abort_out: bool,
    abort_in: bool,
    last_tag: u8,
    read_tag: u8,
    read_max: usize,
    query: Vec<u8>,
    query_count: usize,
    response_offset: usize,
    delayed: bool,
    application: Value,
    selection: Value,
}

impl ManualUsb {
    fn new(case: String, mut trace: Trace) -> Result<Self> {
        trace.event("constructed", json!({"case":case}))?;
        let wire: Value = serde_json::from_str(WIRE)?;
        Ok(Self {
            case,
            trace,
            owned: false,
            pending_out: None,
            pending_in: None,
            cancelled_out: false,
            cancelled_in: false,
            abort_out: false,
            abort_in: false,
            last_tag: 0,
            read_tag: 0,
            read_max: 0,
            query: Vec::new(),
            query_count: 0,
            response_offset: 0,
            delayed: false,
            application: wire["application_controls"].clone(),
            selection: serde_json::from_str(SELECTOR)?,
        })
    }
    fn is(&self, case: &str) -> bool {
        self.case == case
    }
    fn capability(&self, name: &str) -> Result<Vec<u8>> {
        let cases = self.selection["capabilities"]
            .as_array()
            .context("fixture capabilities")?;
        let found = cases
            .iter()
            .find(|value| value["name"] == name)
            .context("fixture capability case missing")?;
        unhex(found["hex"].as_str().context("fixture capability bytes")?)
    }
    fn expected_selector(&self, value: &Config) -> Result<()> {
        let expected = &self.selection["selector"];
        ensure!(
            u64::from(value.vendor_id) == expected["vendor_id"].as_u64().unwrap(),
            "fixture VID differs"
        );
        ensure!(
            u64::from(value.product_id) == expected["product_id"].as_u64().unwrap(),
            "fixture PID differs"
        );
        ensure!(
            value.serial_number == expected["serial_number"].as_str().unwrap(),
            "fixture serial differs"
        );
        ensure!(
            u64::from(value.interface_number) == expected["interface_number"].as_u64().unwrap(),
            "fixture interface differs"
        );
        Ok(())
    }
    fn response(&self) -> Result<Vec<u8>> {
        if self.query == b"WAVE?\n" {
            return unhex(
                self.application["definite_block_reply_hex"]
                    .as_str()
                    .unwrap(),
            );
        }
        if self.query == b"VEC?\n" {
            return Ok(b"1.25,-0.0,2.5\n".to_vec());
        }
        ensure!(
            matches!(self.query.as_slice(), b"READ?\n" | b"SECOND?\n"),
            "unexpected fixture application query"
        );
        unhex(self.application["line_reply_hex"].as_str().unwrap())
    }
    fn completion(&mut self, capacity: usize) -> Result<Completion> {
        let payload = self.response()?;
        let left = payload
            .get(self.response_offset..)
            .context("fixture response offset")?;
        let continuation = self.is("continuation") && self.response_offset == 0;
        let count = left
            .len()
            .min(self.read_max)
            .min(if continuation { 2 } else { usize::MAX });
        let mut body = left
            .get(..count)
            .context("fixture response extent")?
            .to_vec();
        self.response_offset += count;
        let eom = self.response_offset == payload.len() && !self.is("missing_eom");
        let declared = if self.is("zero") {
            0
        } else if self.is("over_request") {
            self.read_max + 1
        } else {
            count
        };
        let mut header = [
            2,
            self.read_tag,
            !self.read_tag,
            0,
            0,
            0,
            0,
            0,
            u8::from(eom),
            0,
            0,
            0,
        ];
        header[4..8].copy_from_slice(&u32::try_from(declared)?.to_le_bytes());
        if self.is("bad_tag") || (self.is("second_bad") && self.query_count > 1) {
            header[1] = self.read_tag.wrapping_add(1);
            header[2] = !header[1];
        }
        if self.is("bad_inverse") {
            header[2] ^= 1;
        }
        if self.is("reserved") {
            header[3] = 1;
        }
        if self.is("bad_attribute") {
            header[8] |= 0x80;
        }
        if self.is("false_termchar") {
            header[8] |= 2;
        }
        if self.is("premature") || self.is("premature_release_error") || self.is("native_prefix") {
            body.truncate(body.len().saturating_sub(1));
        }
        let mut data = header.to_vec();
        data.extend_from_slice(&body);
        let pad = if self.is("pad63") {
            63
        } else if self.is("overlong") {
            64
        } else if self.is("exact_mps") {
            (64 - data.len() % 64) % 64
        } else {
            0
        };
        data.extend(std::iter::repeat_n(0xa5, pad));
        ensure!(
            data.len() <= capacity,
            "fixture producer exceeded submitted capacity"
        );
        let observed = now()?;
        self.trace.event("native_in_return", json!({"observed_ns":observed,"query_ordinal":self.query_count,"wire_hex":hex(&data),"payload_hex":hex(&body),"padding_bytes":pad,"eom":eom,"terminating_zlp":self.is("exact_mps")}))?;
        let status = if self.is("native_prefix") {
            Err(failure(
                ErrorKind::Unavailable,
                "fixture native transfer failure",
            ))
        } else {
            Ok(())
        };
        Ok(Completion {
            transferred: data.len(),
            data,
            short_packet: !self.is("no_short") && status.is_ok(),
            observed_ns: observed,
            status,
        })
    }
}

impl Backend for ManualUsb {
    fn open(&mut self, selector: &Config, deadline: Instant) -> Result<Opened> {
        check(deadline)?;
        self.expected_selector(selector)?;
        self.trace.event("open", json!({"vendor_id":selector.vendor_id,"product_id":selector.product_id,"serial_number":selector.serial_number,"interface_number":selector.interface_number}))?;
        if self.is("busy") {
            return Err(failure(ErrorKind::Unavailable, "fixture interface busy"));
        }
        self.owned = true;
        if self.is("open_hang") {
            thread::sleep(Duration::from_secs(30));
        }
        self.trace.event("claim", json!({"plain":true}))?;
        if self.is("claim_hang") {
            thread::sleep(Duration::from_secs(30));
        }
        if self.is("open_partial") {
            return Err(failure(
                ErrorKind::Unavailable,
                "fixture identity changed after partial open",
            ));
        }
        Ok(Opened {
            vendor_id: selector.vendor_id,
            product_id: selector.product_id,
            serial_number: if self.is("descriptor_mismatch") {
                "DIFFERENT".into()
            } else {
                selector.serial_number.clone()
            },
            interface_number: selector.interface_number,
            configuration: 1,
            alternate_setting: u8::from(self.is("alt")),
            class: 254,
            subclass: 3,
            protocol: 1,
            bulk_in: Endpoint {
                address: 131,
                max_packet_size: 64,
            },
            bulk_out: Endpoint {
                address: 2,
                max_packet_size: 64,
            },
            interrupt_in: if self.is("no_interrupt") {
                None
            } else {
                Some(Endpoint {
                    address: 132,
                    max_packet_size: 2,
                })
            },
        })
    }
    fn control_in(&mut self, request: ControlRequest, deadline: Instant) -> Result<Vec<u8>> {
        check(deadline)?;
        self.trace.event("control_in", json!({"request_type":request.request_type,"request":request.request,"value":request.value,"index":request.index,"length":request.length}))?;
        match (request.request_type, request.request) {
            (0xa1, 7) => {
                ensure!(
                    request.value == 0 && request.index == 3 && request.length == 24,
                    "fixture GET_CAPABILITIES setup differs"
                );
                let name = match self.case.as_str() {
                    "cap_failed" => "failure_ignores_trailing",
                    "cap_short" => "failure_still_requires_full_length",
                    "cap_future" => "future_valid_known_subset",
                    "cap_extension" => "unknown_interface_extension_bit",
                    _ => "usb488_all_defined",
                };
                self.capability(name)
            }
            (0xa2, 1) => {
                ensure!(
                    request.index == 2
                        && request.length == 2
                        && request.value == u16::from(self.last_tag),
                    "fixture AbortOUT setup differs"
                );
                ensure!(
                    self.pending_out.is_none(),
                    "OUT must settle before class AbortOUT"
                );
                self.abort_out = true;
                Ok(vec![1, u8::try_from(request.value)?])
            }
            (0xa2, 2) => {
                ensure!(
                    self.abort_out
                        && request.value == 0
                        && request.index == 2
                        && request.length == 8,
                    "fixture CheckOUT setup differs"
                );
                Ok(vec![1, 0, 0, 0, 6, 0, 0, 0])
            }
            (0xa2, 3) => {
                ensure!(
                    request.index == 131
                        && request.length == 2
                        && request.value == u16::from(self.read_tag),
                    "fixture AbortIN setup differs"
                );
                ensure!(
                    !self.cancelled_in,
                    "class AbortIN must precede native IN cancellation"
                );
                self.abort_in = true;
                if self.is("abort_failed") || self.is("cancel_hang") {
                    Ok(vec![0x81, self.read_tag])
                } else {
                    Ok(vec![1, self.read_tag])
                }
            }
            (0xa2, 4) => {
                ensure!(
                    self.abort_in
                        && !self.is("abort_failed")
                        && !self.is("cancel_hang")
                        && request.value == 0
                        && request.index == 131
                        && request.length == 8,
                    "fixture CheckIN unauthorized/setup differs"
                );
                if self.is("abort_pending") {
                    thread::sleep(Duration::from_millis(1));
                    Ok(vec![2, 0, 0, 0, 0, 0, 0, 0])
                } else {
                    Ok(vec![1, 0, 0, 0, 0, 0, 0, 0])
                }
            }
            _ => Err(failure(
                ErrorKind::OperationFailed,
                "forbidden fixture USB control",
            )),
        }
    }
    fn submit_out(&mut self, bytes: Vec<u8>, deadline: Instant) -> Result<()> {
        check(deadline)?;
        ensure!(
            self.owned && self.pending_out.is_none(),
            "fixture OUT ownership/header"
        );
        self.trace
            .event("submit_out", json!({"wire_hex":hex(&bytes)}))?;
        let header: &[u8; 12] = bytes
            .get(..12)
            .context("fixture short OUT header")?
            .try_into()?;
        let tag = header[1];
        let next = if self.last_tag == 255 {
            1
        } else {
            self.last_tag + 1
        };
        ensure!(
            tag == next && tag != 0 && header[2] == !tag && header[3] == 0,
            "fixture tag policy differs"
        );
        self.last_tag = tag;
        let size = usize::try_from(u32::from_le_bytes([
            header[4], header[5], header[6], header[7],
        ]))?;
        ensure!(size > 0, "fixture zero outgoing TransferSize");
        match header[0] {
            1 => {
                let end = 12_usize
                    .checked_add(size)
                    .context("fixture OUT extent overflow")?;
                let payload = bytes.get(12..end).context("fixture short OUT payload")?;
                let padding = bytes.get(end..).context("fixture OUT padding extent")?;
                ensure!(
                    header[8..12] == [1, 0, 0, 0]
                        && bytes.len().is_multiple_of(4)
                        && padding.len() <= 3,
                    "fixture command shape"
                );
                ensure!(
                    padding.iter().all(|byte| *byte == 0),
                    "host OUT padding convention differs"
                );
                self.query = payload.to_vec();
                self.query_count += 1;
                self.response_offset = 0;
                self.trace.event(
                    "query",
                    json!({"bytes_hex":hex(&self.query),"ordinal":self.query_count}),
                )?;
            }
            2 => {
                ensure!(
                    bytes.len() == 12
                        && header[8..12] == [0; 4]
                        && size <= 65_536
                        && !self.query.is_empty(),
                    "fixture request shape"
                );
                self.read_tag = tag;
                self.read_max = size;
            }
            _ => {
                return Err(failure(
                    ErrorKind::OperationFailed,
                    "fixture unexpected OUT MsgID",
                ));
            }
        }
        self.pending_out = Some(bytes);
        Ok(())
    }
    fn submit_in(&mut self, capacity: usize, deadline: Instant) -> Result<()> {
        check(deadline)?;
        ensure!(
            self.pending_in.is_none() && self.read_tag != 0 && self.pending_out.is_none(),
            "fixture IN before completed request"
        );
        ensure!(
            self.abort_in || capacity > 12 + self.read_max + 63,
            "IN capacity must expose final short including maximal padding"
        );
        self.trace.event(
            if self.abort_in {
                "cleanup_submit_in"
            } else {
                "submit_in"
            },
            json!({"capacity":capacity,"credit":self.read_max}),
        )?;
        self.pending_in = Some(capacity);
        Ok(())
    }
    fn wait(&mut self, direction: Direction, deadline: Instant) -> Result<Option<Completion>> {
        check(deadline)?;
        self.trace.event(
            if self.abort_in {
                "cleanup_wait"
            } else {
                "wait"
            },
            json!({"direction":format!("{direction:?}")}),
        )?;
        if direction == Direction::Out {
            if self.is("out_pending") && !self.cancelled_out {
                thread::sleep(Duration::from_millis(1));
                return Ok(None);
            }
            let bytes = self
                .pending_out
                .take()
                .context("fixture OUT wait without pending")?;
            let status = if self.cancelled_out {
                Err(failure(ErrorKind::Unavailable, "fixture OUT cancelled"))
            } else {
                Ok(())
            };
            return Ok(Some(Completion {
                data: Vec::new(),
                transferred: if self.cancelled_out { 0 } else { bytes.len() },
                short_packet: false,
                observed_ns: now()?,
                status,
            }));
        }
        if self.is("wait_hang") {
            thread::sleep(Duration::from_secs(30));
        }
        if self.is("crash") {
            std::process::exit(71);
        }
        if !self.delayed && (self.is("late") || self.is("probe_late")) {
            self.delayed = true;
            thread::sleep(Duration::from_millis(650));
        }
        if matches!(
            self.case.as_str(),
            "in_pending" | "abort_failed" | "abort_pending" | "cancel_hang"
        ) && !self.abort_in
            && !self.cancelled_in
        {
            thread::sleep(Duration::from_millis(1));
            return Ok(None);
        }
        let capacity = self
            .pending_in
            .take()
            .context("fixture IN wait without pending")?;
        if self.cancelled_in || self.abort_in {
            return Ok(Some(Completion {
                data: Vec::new(),
                transferred: 0,
                short_packet: self.abort_in && !self.cancelled_in,
                observed_ns: now()?,
                status: Ok(()),
            }));
        }
        Ok(Some(self.completion(capacity)?))
    }
    fn pending(&self, direction: Direction) -> usize {
        usize::from(if direction == Direction::In {
            self.pending_in.is_some()
        } else {
            self.pending_out.is_some()
        })
    }
    fn cancel(&mut self, direction: Direction, deadline: Instant) -> Result<()> {
        check(deadline)?;
        self.trace
            .event("cancel", json!({"direction":format!("{direction:?}")}))?;
        if self.is("cancel_hang") {
            thread::sleep(Duration::from_secs(30));
        }
        if direction == Direction::In {
            self.cancelled_in = true;
        } else {
            self.cancelled_out = true;
        }
        Ok(())
    }
    fn clear_halt(&mut self, direction: Direction, deadline: Instant) -> Result<()> {
        check(deadline)?;
        ensure!(
            direction == Direction::Out && self.abort_out,
            "unqualified fixture halt clear"
        );
        self.trace.event("clear_halt", json!({"direction":"Out"}))
    }
    fn release(&mut self, deadline: Instant) -> Result<()> {
        check(deadline)?;
        self.trace.event("release", json!({"owned":self.owned,"pending_in":self.pending_in.is_some(),"pending_out":self.pending_out.is_some()}))?;
        if self.is("release_hang") {
            thread::sleep(Duration::from_secs(30));
        }
        if self.is("release_error") || self.is("premature_release_error") {
            return Err(failure(
                ErrorKind::OperationFailed,
                "fixture release failed",
            ));
        }
        self.owned = false;
        Ok(())
    }
}
impl Drop for ManualUsb {
    fn drop(&mut self) {
        let _ = self.trace.event("drop", json!({"owned":self.owned}));
    }
}

fn read_manual() -> Result<Value> {
    let mut length = [0; 4];
    std::io::stdin().read_exact(&mut length)?;
    let size = usize::try_from(u32::from_le_bytes(length))?;
    ensure!((1..=131_072).contains(&size), "fixture IPC request length");
    let mut body = vec![0; size];
    std::io::stdin().read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}
fn write_manual(value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    let mut out = std::io::stdout().lock();
    out.write_all(&u32::try_from(body.len())?.to_le_bytes())?;
    out.write_all(&body)?;
    out.flush()?;
    Ok(())
}
fn malformed_peer(case: &str, trace: &mut Trace) -> Result<()> {
    let first = read_manual()?;
    trace.event("manual_open", first.clone())?;
    if case == "ipc_bad_length" {
        let mut out = std::io::stdout().lock();
        out.write_all(&131_073_u32.to_le_bytes())?;
        out.flush()?;
        return Ok(());
    }
    let selected: Value = serde_json::from_str(SELECTOR)?;
    let mut ready = json!({"version":1,"request_id":first["request_id"],"result":{"ready":{"descriptor":selected["descriptor"]}}});
    if case == "ipc_wrong_id" {
        ready["request_id"] = json!(2);
    }
    if case == "ipc_bad_version" {
        ready["version"] = json!(2);
    }
    if case == "ipc_unknown" {
        ready["extra"] = json!(0);
    }
    if case == "ipc_null" {
        ready["result"]["ready"]["descriptor"]["interrupt_in"] = Value::Null;
    }
    write_manual(&ready)?;
    if case == "ipc_bad_clock" {
        let query = read_manual()?;
        trace.event("manual_query", query.clone())?;
        write_manual(
            &json!({"version":1,"request_id":query["request_id"],"result":{"query_sent":{}}}),
        )?;
        let request = read_manual()?;
        trace.event("manual_receive", request.clone())?;
        write_manual(
            &json!({"version":1,"request_id":request["request_id"],"result":{"data":{"payload_base64":STANDARD.encode(b"1.25\n"),"eom":true,"observed_ns":0}}}),
        )?;
    }
    Ok(())
}

pub(crate) fn run() -> Result<()> {
    let case = env::var("CAPTUREFAB_USBTMC_CASE").context("fixture case missing")?;
    ensure!(
        case.len() <= 64
            && case
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_' || byte.is_ascii_digit()),
        "fixture case shape"
    );
    let mut trace = Trace::new()?;
    if case.starts_with("ipc_") {
        return malformed_peer(&case, &mut trace);
    }
    super::worker::run_with_backend(ManualUsb::new(case, trace)?)
}
