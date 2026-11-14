//! Independent Compact TCP recipes and bounded CLI/archive observations.
mod support;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    io::{Read as _, Write as _},
    net::{Shutdown, SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::{Child, Stdio},
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use support::{
    cli::{Home, Run},
    time,
};

fn args() -> [&'static str; 9] {
    static WINDOW: LazyLock<String> = LazyLock::new(|| time::arg(1200));
    [
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        WINDOW.as_str(),
        "-o",
        "run.jsonl",
    ]
}
const CORPUS: &str = include_str!("fixtures/sick_compact_tcp/literals.json");
const CORPUS_SHA: &str = "c1bb32d0cbde59f0ccd7432dfe9dbcdef88da80f23f3a46f817015fc0e476af2";
const RESERVE: usize = 4 * 1024 * 1024 + 16 * 1024;
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn number(value: &Value) -> usize {
    usize::try_from(value.as_u64().unwrap()).unwrap()
}
fn unhex(text: &str) -> Vec<u8> {
    assert!(text.is_ascii());
    let (pairs, tail) = text.as_bytes().as_chunks::<2>();
    assert_eq!(tail, &[] as &[u8]);
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn u16_at(raw: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(raw[offset..offset + 2].try_into().unwrap())
}
fn u32_at(raw: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(raw[offset..offset + 4].try_into().unwrap())
}
fn u64_at(raw: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(raw[offset..offset + 8].try_into().unwrap())
}
// Independent table-free ISO-HDLC calculation, never a product encoder.
fn crc32(raw: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in raw {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
fn fix_crc(raw: &mut [u8]) {
    let end = raw.len() - 4;
    let crc = crc32(&raw[..end]).to_le_bytes();
    raw[end..].copy_from_slice(&crc);
}
fn vector(name: &str) -> Value {
    assert_eq!(digest(CORPUS.as_bytes()), CORPUS_SHA);
    let corpus: Value = serde_json::from_str(CORPUS).unwrap();
    corpus["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == name)
        .unwrap()
        .clone()
}
fn segment(value: &Value) -> Vec<u8> {
    if let Some(hex) = value["hex"].as_str() {
        return unhex(hex);
    }
    if let Some(hex) = value["repeat_hex"].as_str() {
        return unhex(hex).repeat(number(&value["repeat"]));
    }
    let width = match value["encoding"].as_str().unwrap() {
        "u8" => 1,
        "u16_le" => 2,
        "u32_le" | "ieee_u32_le" => 4,
        "u64_le" => 8,
        other => panic!("unknown independent recipe {other}"),
    };
    let count = number(&value["count"]);
    let prefix = value["prefix"].as_array().unwrap();
    let suffix = value["suffix"].as_array().unwrap();
    assert!(prefix.len() + suffix.len() <= count);
    let mut words = Vec::with_capacity(count);
    words.extend(prefix.iter().map(|n| n.as_u64().unwrap()));
    words.resize(count - suffix.len(), value["fill"].as_u64().unwrap());
    words.extend(suffix.iter().map(|n| n.as_u64().unwrap()));
    let mut raw = Vec::with_capacity(count * width);
    for word in words {
        raw.extend_from_slice(&word.to_le_bytes()[..width]);
    }
    raw
}
fn packet(name: &str) -> Vec<u8> {
    let case = vector(name);
    let mut raw = Vec::new();
    for piece in case["recipe"].as_array().unwrap() {
        raw.extend(segment(piece));
    }
    assert_eq!(u64::from(crc32(&raw)), case["crc32"].as_u64().unwrap());
    raw.extend(unhex(case["crc32_le_hex"].as_str().unwrap()));
    assert_eq!(case["packet_bytes"], raw.len());
    assert_eq!(case["packet_sha256"], digest(&raw));
    if let Some(hex) = case["packet_hex"].as_str() {
        assert_eq!(raw, unhex(hex));
    }
    raw
}
fn rows(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}
fn samples(home: &Home) -> Vec<Value> {
    rows(home)
        .into_iter()
        .filter(|r| r["kind"] == "sample")
        .collect()
}
fn parts(home: &Home) -> Vec<Value> {
    samples(home)
        .into_iter()
        .filter(|r| r["value"]["kind"] == "sick_compact_tcp_part")
        .collect()
}
fn wait(mut ready: impl FnMut() -> bool) {
    let end = Instant::now() + time::secs(8);
    while !ready() {
        assert!(Instant::now() < end, "bounded Compact TCP fixture state");
        thread::sleep(Duration::from_millis(2));
    }
}
enum Action {
    Send(Vec<u8>),
    LateSend(Vec<u8>),
    Pause(Duration),
    Prefix(PathBuf, usize),
    Eof,
}
struct Peer {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    owner: Option<JoinHandle<()>>,
}
impl Peer {
    fn new(actions: Vec<Action>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let owner = thread::spawn(move || {
            let end = Instant::now() + time::secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if flag.load(Ordering::Acquire) {
                            return;
                        }
                        assert!(Instant::now() < end, "bounded TCP accept");
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(e) => panic!("accept {e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_write_timeout(Some(time::secs(3))).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            'actions: for action in actions {
                if flag.load(Ordering::Acquire) {
                    break;
                }
                match action {
                    Action::Send(raw) => stream.write_all(&raw).unwrap(),
                    Action::LateSend(raw) => match stream.write_all(&raw) {
                        Ok(()) => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::BrokenPipe
                                    | std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::ConnectionAborted
                            ) => {}
                        Err(error) => panic!("late fixture send: {error}"),
                    },
                    Action::Pause(delay) => thread::sleep(delay),
                    Action::Prefix(path, count) => {
                        let end = Instant::now() + time::secs(6);
                        loop {
                            if flag.load(Ordering::Acquire) {
                                break 'actions;
                            }
                            let n = fs::read_to_string(&path)
                                .unwrap_or_default()
                                .lines()
                                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                                .filter(|r| {
                                    r["value"]["kind"] == "sick_compact_tcp_part"
                                        && number(&r["value"]["telegram_index"]) == count
                                })
                                .count();
                            if n > 0 {
                                break;
                            }
                            assert!(Instant::now() < end, "valid prefix published before fault");
                            thread::sleep(Duration::from_millis(2));
                        }
                    }
                    Action::Eof => stream.shutdown(Shutdown::Write).unwrap(),
                }
            }
            let mut byte = [0; 1];
            loop {
                match stream.read(&mut byte) {
                    Ok(0) => break,
                    Ok(n) => panic!("passive client wrote {n} application bytes"),
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::Interrupted
                        ) =>
                    {
                        assert!(Instant::now() < end, "bounded passive connection teardown");
                        if flag.load(Ordering::Acquire) {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
                    Err(e) => panic!("passive read {e}"),
                }
            }
            // finish() can set stop after the CLI exits. Inspect already queued
            // bytes independently of that flag before claiming passivity.
            stream.set_nonblocking(true).unwrap();
            match stream.read(&mut byte) {
                Ok(0) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::ConnectionReset
                    ) => {}
                other => panic!("queued application write or unexpected state: {other:?}"),
            }
            drop(stream);
            match listener.accept() {
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                other => panic!("unexpected reconnect: {other:?}"),
            }
        });
        Self {
            address,
            stop,
            owner: Some(owner),
        }
    }
    fn finish(mut self) {
        self.stop.store(true, Ordering::Release);
        wait(|| self.owner.as_ref().unwrap().is_finished());
        self.owner.take().unwrap().join().unwrap();
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(owner) = self.owner.take() {
            let end = Instant::now() + time::secs(4);
            while !owner.is_finished() && Instant::now() < end {
                thread::sleep(Duration::from_millis(2));
            }
            if owner.is_finished() {
                drop(owner.join());
            }
        }
    }
}
struct Capture(Option<Child>);
impl Capture {
    fn start(home: &Home) -> Self {
        Self(Some(
            home.command()
                .args(args())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        ))
    }
    fn finish(mut self) -> Run {
        wait(|| self.0.as_mut().unwrap().try_wait().unwrap().is_some());
        Run::of_output(self.0.take().unwrap().wait_with_output().unwrap(), &args())
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            drop(child.kill());
            drop(child.wait());
        }
    }
}
fn config(
    home: &Home,
    address: SocketAddr,
    profile: &str,
    stream: &str,
    context: Option<&[u8]>,
    patch: Value,
) {
    let mut source = json!({"id":"lidar","protocol":"sick_compact_tcp","address":address,"profile":profile,"stream":stream,"output_directory":"capture","max_bytes":100_000_000,"timeout_ms":time::ms(1000)});
    if let Some(context) = context {
        fs::write(home.path("configured-context"), context).unwrap();
        source["context"] = json!("configured-context");
    }
    let Value::Object(patch) = patch else {
        panic!("object patch")
    };
    for (key, value) in patch {
        source[key] = value;
    }
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":[source]}).to_string(),
    )
    .unwrap();
}
fn exports(home: &Home, incomplete: bool) {
    for format in ["jsonl", "csv"] {
        let output = format!("export.{format}");
        let mut args = vec![
            "data",
            "export",
            "run.jsonl",
            "--format",
            format,
            "-o",
            &output,
        ];
        if incomplete {
            args.push("--allow-incomplete");
        }
        home.ok(&args);
    }
    let original = samples(home);
    let projected: Vec<_> = original
        .iter()
        .map(|r| {
            let mut r = r.clone();
            r.as_object_mut().unwrap().remove("kind");
            r
        })
        .collect();
    let exported: Vec<Value> = fs::read_to_string(home.path("export.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(exported, projected);
    let mut expected = String::from(
        "seq,source,signal,time,elapsed_ns,read_duration_ns,delivery_latency_ns,source_time,value_json\n",
    );
    for row in &original {
        let quote = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        writeln!(
            expected,
            "{},{},{},{},{},{},{},{},{}",
            row["seq"].as_u64().unwrap(),
            quote(row["source"].as_str().unwrap()),
            quote(row["signal"].as_str().unwrap()),
            quote(row["time"].as_str().unwrap()),
            row["elapsed_ns"].as_u64().unwrap(),
            row["read_duration_ns"].as_u64().unwrap(),
            row["delivery_latency_ns"].as_u64().unwrap(),
            quote(""),
            quote(&row["value"].to_string())
        )
        .unwrap();
    }
    assert_eq!(
        fs::read_to_string(home.path("export.csv")).unwrap(),
        expected
    );
}
fn clocks(home: &Home) {
    let all = rows(home);
    let started =
        chrono::DateTime::parse_from_rfc3339(all[0]["started_at"].as_str().unwrap()).unwrap();
    let mut previous = 0;
    for (n, row) in samples(home).iter().enumerate() {
        assert_eq!(row["seq"], n + 1);
        assert!(row.get("source_time").is_none());
        let elapsed = row["elapsed_ns"].as_u64().unwrap();
        assert!(elapsed >= previous);
        previous = elapsed;
        assert!(elapsed <= 1_200_000_000);
        let time = chrono::DateTime::parse_from_rfc3339(row["time"].as_str().unwrap()).unwrap();
        assert_eq!(
            time,
            started + chrono::Duration::nanoseconds(i64::try_from(elapsed).unwrap())
        );
        assert!(serde_json::to_vec(row).unwrap().len() < 1024 * 1024);
    }
}
#[derive(Clone, Copy)]
struct Layout {
    base: usize,
    rows: usize,
    items: usize,
    width: usize,
    item_stride: usize,
    row_stride: usize,
}
impl Layout {
    fn bytes(self, raw: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for row in 0..self.rows {
            for item in 0..self.items {
                let start = self.base + row * self.row_stride + item * self.item_stride;
                out.extend_from_slice(&raw[start..start + self.width]);
            }
        }
        out
    }
    fn offset(self, byte: usize) -> usize {
        let word = byte / self.width;
        self.base
            + (word / self.items) * self.row_stride
            + (word % self.items) * self.item_stride
            + byte % self.width
    }
    fn from_json(value: &Value) -> Self {
        Self {
            base: number(&value["base_byte_offset"]),
            rows: number(&value["rows"]),
            items: number(&value["items_per_row"]),
            width: number(&value["item_width"]),
            item_stride: number(&value["item_stride"]),
            row_stride: number(&value["row_stride"]),
        }
    }
}
struct Column {
    bytes: Vec<u8>,
    layout: Layout,
    encoding: &'static str,
    order: &'static str,
    shape: Value,
}
type Columns = BTreeMap<(Option<usize>, String), Column>;
fn add_column(
    out: &mut Columns,
    raw: &[u8],
    key: (Option<usize>, &str),
    layout: Layout,
    shape: Value,
    encoding: &'static str,
    order: &'static str,
) {
    assert!(
        out.insert(
            (key.0, key.1.to_owned()),
            Column {
                bytes: layout.bytes(raw),
                layout,
                encoding,
                order,
                shape
            }
        )
        .is_none()
    );
}
fn contiguous(
    out: &mut Columns,
    raw: &[u8],
    id: &str,
    range: [usize; 2],
    shape: Value,
    encoding: &'static str,
    order: &'static str,
) {
    let [base, width] = range;
    let count = shape
        .as_array()
        .unwrap()
        .iter()
        .map(number)
        .product::<usize>();
    add_column(
        out,
        raw,
        (None, id),
        Layout {
            base,
            rows: 1,
            items: count,
            width,
            item_stride: width,
            row_stride: count * width,
        },
        shape,
        encoding,
        order,
    );
}
fn expected_columns(case: &Value, raw: &[u8]) -> Columns {
    let mut out = Columns::new();
    let kind = case["telegram_type"].as_u64().unwrap();
    if kind == 1 {
        let version = case["telegram_version"].as_u64().unwrap();
        let mut base = 32;
        let mut size = usize::try_from(u32_at(raw, 28)).unwrap();
        let mut module = 0;
        while size > 0 {
            let l = usize::try_from(u32_at(raw, base + 20)).unwrap();
            let b = usize::try_from(u32_at(raw, base + 24)).unwrap();
            let e = usize::try_from(u32_at(raw, base + 28)).unwrap();
            for (id, offset, width, encoding) in [
                ("timestamp_start", 32, 8, "base64_le_u64"),
                ("timestamp_stop", 32 + 8 * l, 8, "base64_le_u64"),
                ("phi", 32 + 16 * l, 4, "ieee754_le_u32_bits"),
                ("theta_start", 32 + 20 * l, 4, "ieee754_le_u32_bits"),
                ("theta_stop", 32 + 24 * l, 4, "ieee754_le_u32_bits"),
            ] {
                add_column(
                    &mut out,
                    raw,
                    (Some(module), id),
                    Layout {
                        base: base + offset,
                        rows: 1,
                        items: l,
                        width,
                        item_stride: width,
                        row_stride: l * width,
                    },
                    json!([l]),
                    encoding,
                    "line",
                );
            }
            let next_offset = base + 32 + 28 * l + if version == 4 { 4 } else { 0 };
            let echo = raw[next_offset + 5];
            let beam = raw[next_offset + 6];
            let echo_width = usize::from(echo & 1 != 0) * 2 + usize::from(echo & 2 != 0) * 2;
            let stride =
                e * echo_width + usize::from(beam & 1 != 0) + usize::from(beam & 2 != 0) * 2;
            let data = next_offset + 8;
            for (id, mask, offset) in [
                ("distance", 1, 0),
                ("rssi", 2, usize::from(echo & 1 != 0) * 2),
            ] {
                if echo & mask != 0 {
                    add_column(
                        &mut out,
                        raw,
                        (Some(module), id),
                        Layout {
                            base: data + offset,
                            rows: b * l,
                            items: e,
                            width: 2,
                            item_stride: echo_width,
                            row_stride: stride,
                        },
                        json!([b, l, e]),
                        "base64_le_u16",
                        "beam_line_echo",
                    );
                }
            }
            let props = data + e * echo_width + if version == 3 && beam & 2 != 0 { 2 } else { 0 };
            let azimuth = data + e * echo_width + usize::from(version == 4 && beam & 1 != 0);
            for (id, mask, offset, width, encoding) in [
                ("properties", 1, props, 1, "base64_u8"),
                ("azimuth", 2, azimuth, 2, "base64_le_u16"),
            ] {
                if beam & mask != 0 {
                    add_column(
                        &mut out,
                        raw,
                        (Some(module), id),
                        Layout {
                            base: offset,
                            rows: b * l,
                            items: 1,
                            width,
                            item_stride: width,
                            row_stride: stride,
                        },
                        json!([b, l]),
                        encoding,
                        "beam_line",
                    );
                }
            }
            base += size;
            size = usize::try_from(u32_at(raw, next_offset)).unwrap();
            module += 1;
        }
    } else if kind == 6 || kind == 3 {
        let c = if kind == 6 {
            number(&case["oracle"]["C"])
        } else {
            number(&case["oracle"]["columns"])
        };
        let l = if kind == 6 {
            number(&case["oracle"]["L"])
        } else {
            number(&case["oracle"]["layers"])
        };
        let e = if kind == 6 {
            number(&case["oracle"]["E"])
        } else {
            1
        };
        for recipe in case["recipe"].as_array().unwrap() {
            let field = recipe["field"].as_str().unwrap();
            let tuple = match field {
                "ambient_light_data" => Some((
                    "ambient_light",
                    2,
                    "base64_le_u16",
                    "wire_flat_order_unqualified",
                    json!([if kind == 6 {
                        number(&case["oracle"]["A"]) * c
                    } else {
                        l * c
                    }]),
                )),
                "elevation_angles" => {
                    Some(("elevation", 4, "ieee754_le_u32_bits", "layer", json!([l])))
                }
                "azimuth_angles" => {
                    Some(("azimuth", 4, "ieee754_le_u32_bits", "column", json!([c])))
                }
                "relative_time_stamps" => Some((
                    "relative_timestamp",
                    4,
                    "base64_le_u32",
                    "column",
                    json!([c]),
                )),
                "column_properties" => Some((
                    "column_properties",
                    2,
                    "base64_le_u16",
                    "column",
                    json!([c]),
                )),
                "distance" => Some((
                    "distance",
                    2,
                    "base64_le_u16",
                    "column_layer_echo",
                    json!([c, l, e]),
                )),
                "intensity_packed" => Some((
                    "intensity_packed",
                    1,
                    "opaque_packed_u12",
                    "wire_packed_bytes",
                    json!([c * l * e * 3 / 2]),
                )),
                "pulse_width" => Some((
                    "pulse_width",
                    1,
                    "base64_u8",
                    "wire_flat_order_unqualified",
                    json!([c * l * e]),
                )),
                "echo_properties" => Some((
                    "echo_properties",
                    1,
                    "base64_u8",
                    "column_layer_echo",
                    json!([c, l, e]),
                )),
                _ => None,
            };
            if let Some((id, width, encoding, order, shape)) = tuple {
                let range = &case["field_wire_ranges"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|r| r["field"] == field)
                    .unwrap()["wire_byte_range"];
                let base = number(&range[0]);
                contiguous(&mut out, raw, id, [base, width], shape, encoding, order);
                let expected = segment(recipe);
                assert_eq!(
                    out[&(None, id.to_owned())].bytes,
                    expected,
                    "independent recipe column {field}"
                );
            }
        }
    } else {
        assert_eq!(kind, 7);
        contiguous(
            &mut out,
            raw,
            "acceleration",
            [44, 4],
            json!([3]),
            "ieee754_le_u32_bits",
            "x_y_z",
        );
        contiguous(
            &mut out,
            raw,
            "angular_velocity",
            [56, 4],
            json!([3]),
            "ieee754_le_u32_bits",
            "x_y_z",
        );
        contiguous(
            &mut out,
            raw,
            "orientation",
            [68, 4],
            json!([4]),
            "ieee754_le_u32_bits",
            "w_x_y_z",
        );
    }
    out
}
fn metadata(actual: &Value, case: &Value, raw: &[u8]) {
    let kind = u32_at(raw, 4);
    let version = u32_at(raw, 24);
    if kind == 1 {
        assert_eq!(actual["size_module_0"], u32_at(raw, 28));
        let modules = actual["modules"].as_array().unwrap();
        let mut base = 32;
        let mut size = usize::try_from(u32_at(raw, 28)).unwrap();
        for (index, module) in modules.iter().enumerate() {
            assert!(size > 0);
            assert_eq!(module["module_index"], index);
            assert_eq!(module["module_bytes"], size);
            for (name, offset) in [("segment_counter", 0), ("frame_number", 8)] {
                assert_eq!(module[name], u64_at(raw, base + offset));
            }
            for (name, offset) in [
                ("sender_id", 16),
                ("line_count", 20),
                ("beams_per_line", 24),
                ("echoes_per_beam", 28),
            ] {
                assert_eq!(module[name], u32_at(raw, base + offset));
            }
            let l = usize::try_from(u32_at(raw, base + 20)).unwrap();
            let next = base + 32 + l * 28 + if version == 4 { 4 } else { 0 };
            assert_eq!(module["next_module_size"], u32_at(raw, next));
            assert_eq!(
                module["reserved_raw"],
                json!([raw[next + 4], raw[next + 7]])
            );
            assert_eq!(module["echo_mask_raw"], raw[next + 5]);
            assert_eq!(module["beam_mask_raw"], raw[next + 6]);
            assert_eq!(module["reflector_mask"], 1);
            assert_eq!(module["property_reserved_mask"], 254);
            if version == 4 {
                assert_eq!(module["distance_scale"]["bits"], u32_at(raw, next - 4));
                assert_eq!(module["distance_relation"], "raw_times_distance_scale_mm");
            } else {
                assert!(module.get("distance_scale").is_none());
                assert_eq!(module["distance_relation"], "raw_mm");
            }
            base += size;
            size = usize::try_from(u32_at(raw, next)).unwrap();
        }
        assert_eq!(size, 0);
        assert_eq!(base + 4, raw.len());
    } else {
        assert_eq!(actual["sender_id"], u32_at(raw, 32));
        assert_eq!(actual["payload_length"], u32_at(raw, 28));
        let oracle = &case["oracle"];
        if kind == 6 {
            let scan = &actual["scan"];
            assert_eq!(scan["frame_number"], u64_at(raw, 36));
            assert_eq!(scan["frame_timestamp"]["raw"], u64_at(raw, 44));
            for (name, offset) in [
                ("segment_index", 52),
                ("segments_per_frame", 54),
                ("columns_in_segment", 56),
                ("columns_per_frame", 58),
                ("layers", 60),
                ("ambient_layers", 63),
            ] {
                assert_eq!(scan[name], u16_at(raw, offset));
            }
            assert_eq!(scan["echoes"], raw[62]);
            for (name, offset) in [
                ("interlace_steps", 65),
                ("interlace_index", 66),
                ("scan_configuration_identifier", 67),
                ("data_content_echo_raw", 72),
            ] {
                assert_eq!(scan[name], raw[offset]);
            }
            assert_eq!(
                scan["distance_scale"]["bits"],
                oracle["distance_scale_word"]
            );
            assert_eq!(
                STANDARD
                    .decode(scan["reserved_payload_base64"].as_str().unwrap())
                    .unwrap(),
                unhex(oracle["reserved_payload_hex"].as_str().unwrap())
            );
            assert_eq!(
                scan["column_properties_status"],
                "raw_u16_no_flags_inferred"
            );
            assert_eq!(scan["echo_properties_status"], "raw_u8_no_flags_inferred");
        } else if kind == 3 {
            let ambient = &actual["ambient"];
            assert_eq!(ambient["frame_number"], oracle["frame_number"]);
            assert_eq!(
                ambient["timestamp_start"]["raw"],
                oracle["timestamp_start_raw"]
            );
            assert_eq!(
                ambient["timestamp_stop"]["raw"],
                oracle["timestamp_stop_raw"]
            );
            assert_eq!(ambient["layers"], oracle["layers"]);
            assert_eq!(ambient["columns"], oracle["columns"]);
            assert_eq!(ambient["encoding_raw"], 0);
            assert_eq!(ambient["ordering_status"], "wire_flat_order_unqualified");
            for (index, name) in [
                "horizontal_angle_start",
                "horizontal_angle_stop",
                "vertical_angle_start",
                "vertical_angle_stop",
            ]
            .iter()
            .enumerate()
            {
                assert_eq!(ambient[name]["bits"], oracle["angle_words"][index]);
            }
        } else {
            assert_eq!(kind, 7);
            assert_eq!(actual["timestamp"]["raw"], oracle["timestamp_raw"]);
        }
    }
}
fn native(case: &Value, raw: &[u8], all: &[Value]) {
    assert_ne!(all, [] as [Value; 0]);
    assert!(all.len() <= 64);
    let oracle = &case["oracle"];
    let elements = match u32_at(raw, 4) {
        1 => oracle["native"]["modules"].as_array().map_or_else(
            || {
                number(&oracle["line_count"])
                    * number(&oracle["beams_per_line"])
                    * number(&oracle["echoes_per_beam"])
            },
            |modules| {
                modules
                    .iter()
                    .map(|m| {
                        number(&m["line_count"])
                            * number(&m["beams_per_line"])
                            * number(&m["echoes_per_beam"])
                    })
                    .sum()
            },
        ),
        6 => number(&oracle["N"]),
        3 => number(&oracle["layers"]) * number(&oracle["columns"]),
        7 => 1,
        other => panic!("unqualified oracle type {other}"),
    };
    let expected = expected_columns(case, raw);
    let mut recovered: BTreeMap<(Option<usize>, String), Vec<u8>> = BTreeMap::new();
    for (index, part) in all.iter().enumerate() {
        assert_eq!(part["kind"], "sick_compact_tcp");
        assert_eq!(part["telegram_type"], u32_at(raw, 4));
        assert_eq!(part["telegram_version"], u32_at(raw, 24));
        assert_eq!(part["telegram_counter"], u64_at(raw, 8));
        assert_eq!(part["transmit_timestamp"]["raw"], u64_at(raw, 16));
        assert_eq!(part["transmit_timestamp"]["unit"], "us");
        assert_eq!(
            part["transmit_timestamp"]["basis"],
            "unix_microseconds_declared"
        );
        assert_eq!(part["part_index"], index);
        assert_eq!(part["part_count"], all.len());
        assert_eq!(part["native_elements_declared"], elements);
        assert_eq!(
            part["native_elements_scope"],
            "complete_telegram_not_per_part"
        );
        assert_eq!(part["telegram_sha256"], digest(raw));
        assert_eq!(part["clock_sync"], "not_verified");
        assert_eq!(
            part["native_assembly"],
            "all_parts_share_one_verified_telegram"
        );
        assert_eq!(part["byte_order"], "little_endian");
        assert_eq!(
            part["checksum"],
            json!({"algorithm":"crc32_iso_hdlc","scope":"frame_without_crc","stored":u32_at(raw,raw.len()-4),"calculated":crc32(&raw[..raw.len()-4]),"status":"verified"})
        );
        assert!(serde_json::to_vec(part).unwrap().len() <= 768 * 1024);
        if index == 0 {
            metadata(&part["native_metadata"], case, raw);
        } else {
            assert!(part.get("native_metadata").is_none());
        }
        for column in part["native_columns"].as_array().unwrap() {
            let key = (
                column.get("module_index").map(number),
                column["id"].as_str().unwrap().to_owned(),
            );
            let wanted = expected
                .get(&key)
                .unwrap_or_else(|| panic!("unexpected column {key:?}"));
            let bytes = STANDARD.decode(column["data"].as_str().unwrap()).unwrap();
            let out = recovered.entry(key.clone()).or_default();
            let start = number(&column["global_byte_range"][0]);
            let end = number(&column["global_byte_range"][1]);
            assert_eq!(start, out.len());
            assert_eq!(end, start + bytes.len());
            assert!(end <= wanted.bytes.len());
            assert_eq!(bytes, wanted.bytes[start..end]);
            let source = Layout::from_json(&column["source_layout"]);
            assert_eq!(source.width, wanted.layout.width);
            assert_eq!(
                source.rows * source.items * source.width,
                wanted.bytes.len()
            );
            for byte in start..end {
                assert_eq!(
                    source.offset(byte),
                    wanted.layout.offset(byte),
                    "absolute wire address for {key:?}/{byte}"
                );
            }
            assert_eq!(column["encoding"], wanted.encoding);
            assert_eq!(column["shape"], wanted.shape);
            assert_eq!(column["order"], wanted.order);
            let unit = match (u32_at(raw, 4), key.1.as_str()) {
                (_, "timestamp_start" | "timestamp_stop" | "relative_timestamp") => Some("us"),
                (1, "phi" | "theta_start" | "theta_stop") | (6, "elevation" | "azimuth") => {
                    Some("rad")
                }
                (1, "azimuth") => Some("raw_theta_uint16"),
                (_, "distance") => Some("raw"),
                (_, "rssi") => Some("dimensionless"),
                (_, "pulse_width") => Some("0.125_ns_raw_units"),
                (_, "acceleration") => Some("m/s^2"),
                (_, "angular_velocity") => Some("rad/s"),
                _ => None,
            };
            if let Some(unit) = unit {
                assert_eq!(column["unit"], unit);
            } else {
                assert!(column.get("unit").is_none());
            }
            if matches!(key.1.as_str(), "timestamp_start" | "timestamp_stop") {
                assert_eq!(column["basis"], "device_timebase_microseconds_declared");
            } else if key.1 == "relative_timestamp" {
                assert_eq!(column["basis"], "relative_to_frame_timestamp_declared");
            } else {
                assert!(column.get("basis").is_none());
            }
            assert_eq!(
                number(&column["total_elements"]),
                wanted.bytes.len() / wanted.layout.width
            );
            if key.1 == "intensity_packed" {
                assert!(column.get("global_element_range").is_none());
                let n = number(&case["oracle"]["N"]);
                assert_eq!(
                    column["packed_rssi"],
                    json!({"declared_samples":n,"declared_sample_bit_count":12*n,"available_bit_count":wanted.bytes.len()*8,"global_bit_range":[8*start,8*end],"interpretation":"unqualified_no_values_or_padding_inferred"})
                );
            } else {
                assert_eq!(
                    column["global_element_range"],
                    json!([start / wanted.layout.width, end / wanted.layout.width])
                );
            }
            out.extend_from_slice(&bytes);
        }
    }
    for (key, column) in expected {
        if column.bytes.is_empty() {
            assert_eq!(recovered.get(&key).map_or(0, Vec::len), 0);
        } else {
            assert_eq!(recovered[&key], column.bytes, "all bytes in {key:?}");
        }
    }
    if case["name"] == "multiscan200_type6_full_geometry" {
        assert!(all.len() > 1, "full geometry exercises split native parts");
    }
}
fn archive(
    home: &Home,
    peer: SocketAddr,
    profile: &str,
    stream: &str,
    context: Option<&[u8]>,
    names: &[&str],
) {
    let raw = fs::read(home.path("capture/telegrams.sickcompacttcp")).unwrap();
    let context_raw = context.unwrap_or_default();
    assert_eq!(&raw[..8], b"CFSCTP01");
    assert_eq!(u16_at(&raw, 8), 1);
    assert_eq!(u16_at(&raw, 10), 80);
    assert_eq!(
        usize::try_from(u32_at(&raw, 12)).unwrap(),
        context_raw.len()
    );
    assert_eq!(u32_at(&raw, 16), 4 * 1024 * 1024);
    assert_eq!(u16_at(&raw, 20), if profile == "lrs4000" { 1 } else { 2 });
    assert_eq!(u16_at(&raw, 22), u16::from(context.is_some()));
    assert_eq!(&raw[24..28], &[127, 0, 0, 1]);
    assert_eq!(u16_at(&raw, 28), peer.port());
    assert_eq!(u16_at(&raw, 30), 0);
    assert!(u64_at(&raw, 32) > 0);
    assert_eq!(&raw[40..72], Sha256::digest(context_raw).as_slice());
    assert_eq!(
        u16_at(&raw, 72),
        match stream {
            "points" => 1,
            "imu" => 2,
            "ambient" => 3,
            _ => panic!("stream"),
        }
    );
    assert_eq!(
        u16_at(&raw, 74),
        if profile == "lrs4000" {
            36
        } else {
            match stream {
                "points" => 168,
                "imu" => 88,
                _ => 86,
            }
        }
    );
    assert_eq!(&raw[76..80], &[0; 4]);
    assert_eq!(&raw[80..80 + context_raw.len()], context_raw);
    let all = parts(home);
    let mut offset = 80 + context_raw.len();
    let mut stream_offset = 0;
    let mut released = 0;
    let mut origin = None;
    let mut previous = u64_at(&raw, 32);
    for (index, name) in names.iter().enumerate() {
        let wire = packet(name);
        assert_eq!(&raw[offset..offset + 4], b"TGM1");
        assert_eq!(
            usize::try_from(u32_at(&raw, offset + 4)).unwrap(),
            wire.len()
        );
        assert_eq!(u64_at(&raw, offset + 8), u64::try_from(index + 1).unwrap());
        let receive = u64_at(&raw, offset + 16);
        assert!(receive >= previous);
        previous = receive;
        assert_eq!(
            u64_at(&raw, offset + 24),
            u64::try_from(stream_offset).unwrap()
        );
        assert_eq!(&raw[offset + 32..offset + 32 + wire.len()], wire);
        let footer = offset + 32 + wire.len();
        assert_eq!(&raw[footer..footer + 8], b"CFSCTC1\0");
        assert_eq!(u64_at(&raw, footer + 8), u64::try_from(index + 1).unwrap());
        assert_eq!(
            u64_at(&raw, footer + 16),
            u64::try_from(32 + wire.len()).unwrap()
        );
        assert_eq!(
            &raw[footer + 24..footer + 56],
            Sha256::digest(&raw[offset..footer]).as_slice()
        );
        let length = 32 + wire.len() + 56;
        let selected: Vec<_> = all
            .iter()
            .filter(|r| number(&r["value"]["telegram_index"]) == index + 1)
            .collect();
        assert_ne!(selected, [] as [&Value; 0]);
        let mut natives = Vec::new();
        for (part, row) in selected.iter().enumerate() {
            let value = &row["value"];
            assert_eq!(value["part_index"], part);
            assert_eq!(value["part_count"], selected.len());
            assert_eq!(value["format_version"], 1);
            assert_eq!(value["received_monotonic_ns"], receive);
            assert_eq!(value["source_endpoint"], peer.to_string());
            assert_eq!(value["stream_offset"], stream_offset);
            let path = Path::new(value["archive"]["path"].as_str().unwrap());
            let resolved = if path.is_absolute() {
                path.to_path_buf()
            } else {
                home.path(path.to_str().unwrap())
            };
            assert_eq!(
                resolved.canonicalize().unwrap(),
                home.path("capture/telegrams.sickcompacttcp")
                    .canonicalize()
                    .unwrap()
            );
            assert_eq!(
                value["archive"],
                json!({"path":value["archive"]["path"],"telegram_index":index+1,"offset":offset,"length":length,"sha256":digest(&raw[offset..offset+length]),"raw_offset":offset+32,"raw_length":wire.len()})
            );
            assert_eq!(
                value["sensor"],
                json!({"profile":profile,"profile_source":"configured","stream":stream,"identity_status":"configured_endpoint_only","context_status":if context.is_some(){"configured_unverified"}else{"absent"}})
            );
            if context.is_some() {
                assert_eq!(
                    value["context"],
                    json!({"offset":80,"length":context_raw.len(),"sha256":digest(context_raw)})
                );
            } else {
                assert!(value.get("context").is_none());
            }
            let this_origin = receive - row["elapsed_ns"].as_u64().unwrap();
            if let Some(origin) = origin {
                assert_eq!(this_origin, origin);
            }
            origin = Some(this_origin);
            natives.push(value["native"].clone());
        }
        native(&vector(name), &wire, &natives);
        released += selected.len();
        offset += length;
        stream_offset += wire.len();
    }
    assert_eq!(released, all.len());
    assert_eq!(offset, raw.len());
    let sample_rows = samples(home);
    if names.is_empty() {
        assert_eq!(sample_rows, Vec::<Value>::new());
    } else {
        let terminal = &sample_rows.last().unwrap()["value"];
        assert_eq!(terminal["kind"], "sick_compact_tcp_file");
        assert_eq!(terminal["telegrams"], names.len());
        assert_eq!(terminal["parts_admitted"], all.len());
        assert_eq!(terminal["part_readings_released"], all.len());
        assert_eq!(terminal["pending_parts"], 0);
        assert_eq!(terminal["committed_bytes"], raw.len());
        assert_eq!(terminal["sha256_committed_prefix"], digest(&raw));
        assert_eq!(terminal["archive_committed_prefix_only"], true);
        assert_eq!(terminal["jsonl_publication_not_proven"], true);
        assert_eq!(terminal["part_delivery_crash_gap_possible"], true);
        assert_eq!(terminal["storage_finalized"], true);
        assert_eq!(terminal["uncommitted_tail_bytes"], 0);
    }
    clocks(home);
}
fn rejected(home: &Home, error: &Value, offered: &[u8], offset: usize, complete: bool) {
    let prefix = &error["details"]["sick_compact_tcp"];
    let evidence = &prefix["rejected_stream_prefix"];
    let raw = fs::read(home.path("capture/rejected-stream.raw")).unwrap();
    let encoded = fs::read(home.path("capture/rejected-stream.json")).unwrap();
    assert_eq!(&raw, &offered[..raw.len()]);
    if complete {
        assert_eq!(&raw, offered);
    }
    assert_eq!(evidence["complete"], true);
    assert_eq!(evidence["raw"]["length"], raw.len());
    assert_eq!(evidence["raw"]["sha256"], digest(&raw));
    assert_eq!(evidence["metadata"]["length"], encoded.len());
    assert_eq!(evidence["metadata"]["sha256"], digest(&encoded));
    assert!(encoded.len() <= 16 * 1024);
    let metadata: Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(metadata["kind"], "sick_compact_tcp_rejected_stream_prefix");
    assert_eq!(metadata["format_version"], 1);
    assert_eq!(metadata["stream_offset"], offset);
    assert_eq!(metadata["raw_length"], raw.len());
    assert_eq!(metadata["sha256"], digest(&raw));
    assert_eq!(metadata["cause"]["code"], error["code"]);
    assert_eq!(metadata["checksum_status"], "not_accepted");
    assert_eq!(prefix["diagnostic_bytes"], raw.len() + encoded.len());
    assert_eq!(prefix["failure_reserved_bytes"], RESERVE);
    assert_eq!(prefix["stream_bytes_consumed"], offset + raw.len());
    assert!(raw.len() <= 4 * 1024 * 1024);
    assert!(metadata.get("received_monotonic_ns").is_none());
    if prefix["telegrams"] == 0 {
        assert_eq!(
            metadata["context_evidence_archive"]["valid_telegram_count"],
            0
        );
        assert_eq!(
            prefix["context_evidence_archive"]["valid_telegram_count"],
            0
        );
    }
}
fn fragmented(raw: &[u8]) -> Vec<Action> {
    let mut actions = Vec::new();
    let mut offset = 0;
    for count in [1, 2, 5, 17, 31] {
        if offset + count > raw.len() {
            break;
        }
        actions.push(Action::Send(raw[offset..offset + count].to_vec()));
        offset += count;
        actions.push(Action::Pause(Duration::from_millis(1)));
    }
    for chunk in raw[offset..].chunks(32 * 1024) {
        actions.push(Action::Send(chunk.to_vec()));
    }
    actions
}
#[test]
fn independent_predecoder_recipes_preserve_headers_crc_and_full_geometry() {
    assert_eq!(digest(CORPUS.as_bytes()), CORPUS_SHA);
    let corpus: Value = serde_json::from_str(CORPUS).unwrap();
    assert_eq!(corpus["cases"].as_array().unwrap().len(), 11);
    for case in corpus["cases"].as_array().unwrap() {
        let wire = packet(case["name"].as_str().unwrap());
        assert_eq!(&wire[..4], &[2; 4]);
        assert_eq!(case["telegram_type"], u32_at(&wire, 4));
        assert_eq!(case["telegram_version"], u32_at(&wire, 24));
        assert_eq!(
            u32_at(&wire, wire.len() - 4),
            crc32(&wire[..wire.len() - 4])
        );
        if u32_at(&wire, 4) != 1 {
            assert_eq!(usize::try_from(u32_at(&wire, 28)).unwrap() + 40, wire.len());
        }
    }
    assert_eq!(packet("lrs_full_9000_v3").len(), 135_104);
    assert_eq!(packet("lrs_full_9000_v4").len(), 135_108);
    assert_eq!(packet("multiscan200_type6_full_geometry").len(), 1_768_648);
    assert_eq!(packet("multiscan200_type3_full_geometry").len(), 227_414);
    assert_eq!(packet("multiscan200_type7_timestamp_first").len(), 88);
    let odd = vector("multiscan200_type6_odd_count_opaque");
    assert_eq!(odd["oracle"]["rssi_declared_sample_bits"], 36);
    assert_eq!(odd["oracle"]["rssi_available_bits"], 32);
}
#[test]
fn fragmented_and_coalesced_full_native_geometries_archive_before_all_parts_and_export_exactly() {
    let scenarios: [(&str, &str, &[&str]); 4] = [
        (
            "lrs4000",
            "points",
            &[
                "lrs_v3_two_unequal_modules",
                "lrs_full_9000_v3",
                "lrs_v4_two_unequal_modules",
                "lrs_full_9000_v4",
            ],
        ),
        (
            "multiscan200",
            "points",
            &[
                "multiscan200_type6_small_all_channels",
                "multiscan200_type6_cross_column_opaque",
                "multiscan200_type6_odd_count_opaque",
                "multiscan200_type6_full_geometry",
            ],
        ),
        (
            "multiscan200",
            "ambient",
            &[
                "multiscan200_type3_small",
                "multiscan200_type3_full_geometry",
            ],
        ),
        (
            "multiscan200",
            "imu",
            &["multiscan200_type7_timestamp_first"],
        ),
    ];
    for (profile, stream, names) in scenarios {
        let home = Home::new("sick-tcp-native");
        let context = b"\0opaque configured calibration\xff";
        let first = packet(names[0]);
        let mut actions = fragmented(&first);
        let mut coalesced = Vec::new();
        for name in &names[1..] {
            coalesced.extend(packet(name));
        }
        if !coalesced.is_empty() {
            actions.push(Action::Send(coalesced));
        }
        let peer = Peer::new(actions);
        config(
            &home,
            peer.address,
            profile,
            stream,
            Some(context),
            json!({"timeout_ms":time::ms(1000)}),
        );
        let capture = Capture::start(&home);
        capture.finish().success();
        archive(&home, peer.address, profile, stream, Some(context), names);
        assert!(!home.path("capture/rejected-stream.raw").exists());
        exports(&home, false);
        peer.finish();
    }
}
#[test]
fn complete_crc_version_encoding_and_module_link_failures_keep_exact_valid_prefix_and_consumed_evidence()
 {
    for fault in [
        "crc",
        "type6_length",
        "type3_geometry",
        "type3_encoding",
        "type3_version",
        "type7_version",
        "module_link",
    ] {
        for prefix in [false, true] {
            let (profile, stream, name) = match fault {
                "module_link" => ("lrs4000", "points", "lrs_v3_two_unequal_modules"),
                "type3_geometry" | "type3_encoding" | "type3_version" => {
                    ("multiscan200", "ambient", "multiscan200_type3_small")
                }
                "type7_version" => ("multiscan200", "imu", "multiscan200_type7_timestamp_first"),
                _ => (
                    "multiscan200",
                    "points",
                    "multiscan200_type6_small_all_channels",
                ),
            };
            let home = Home::new("sick-tcp-bad");
            let good = packet(name);
            let mut bad = good.clone();
            match fault {
                "crc" => {
                    bad[40] ^= 1;
                }
                "type6_length" => {
                    let n = u32_at(&bad, 28) + 1;
                    bad[28..32].copy_from_slice(&n.to_le_bytes());
                    fix_crc(&mut bad);
                }
                "type3_geometry" => {
                    let layers = u16_at(&bad, 60) + 1;
                    bad[60..62].copy_from_slice(&layers.to_le_bytes());
                    fix_crc(&mut bad);
                }
                "type3_encoding" => {
                    bad[80..82].copy_from_slice(&1_u16.to_le_bytes());
                    fix_crc(&mut bad);
                }
                "type3_version" | "type7_version" => {
                    bad[24..28].copy_from_slice(&2_u32.to_le_bytes());
                    fix_crc(&mut bad);
                }
                "module_link" => {
                    bad[92..96].copy_from_slice(&1_u32.to_le_bytes());
                    fix_crc(&mut bad);
                }
                _ => unreachable!(),
            }
            let mut actions = Vec::new();
            if prefix {
                actions.push(Action::Send(good.clone()));
                actions.push(Action::Prefix(home.path("run.jsonl"), 1));
            }
            actions.push(Action::Send(bad.clone()));
            if fault == "type6_length" {
                actions.push(Action::Eof);
            }
            let peer = Peer::new(actions);
            let context = b"independent failure context";
            config(
                &home,
                peer.address,
                profile,
                stream,
                Some(context),
                json!({"timeout_ms":time::ms(250)}),
            );
            let error = Capture::start(&home).finish().failure(
                if matches!(fault, "type3_encoding" | "type3_version" | "type7_version") {
                    5
                } else {
                    1
                },
            );
            assert_eq!(
                error["code"],
                if matches!(fault, "type3_encoding" | "type3_version" | "type7_version") {
                    "unsupported"
                } else {
                    "incomplete"
                }
            );
            rejected(
                &home,
                &error,
                &bad,
                if prefix { good.len() } else { 0 },
                matches!(fault, "crc" | "type3_geometry" | "type3_encoding"),
            );
            let expected = if prefix { vec![name] } else { Vec::new() };
            archive(
                &home,
                peer.address,
                profile,
                stream,
                Some(context),
                &expected,
            );
            assert_eq!(
                error["details"]["sick_compact_tcp"]["telegrams"],
                usize::from(prefix)
            );
            exports(&home, true);
            peer.finish();
        }
    }
}
#[test]
fn eof_at_each_structural_stage_is_terminal_without_resync_or_second_accept() {
    let wire = packet("lrs_v4_two_unequal_modules");
    for cut in [0, 3, 8, 31, 32, 60, 104, wire.len() - 1] {
        let home = Home::new("sick-tcp-eof");
        let peer = Peer::new(vec![Action::Send(wire[..cut].to_vec()), Action::Eof]);
        config(
            &home,
            peer.address,
            "lrs4000",
            "points",
            None,
            json!({"timeout_ms":time::ms(250)}),
        );
        let error = Capture::start(&home)
            .finish()
            .failure(if cut == 0 { 3 } else { 1 });
        assert_eq!(
            error["code"],
            if cut == 0 {
                "disconnected"
            } else {
                "incomplete"
            }
        );
        rejected(&home, &error, &wire[..cut], 0, true);
        archive(&home, peer.address, "lrs4000", "points", None, &[]);
        exports(&home, true);
        peer.finish();
    }
}
#[test]
fn progressing_partial_frame_uses_original_deadline_and_finish_never_reads_fresh_suffix() {
    let name = "multiscan200_type7_timestamp_first";
    let good = packet(name);
    let home = Home::new("sick-tcp-cutoff");
    let actions = vec![
        Action::Send(good.clone()),
        Action::Prefix(home.path("run.jsonl"), 1),
        Action::Send(good[..36].to_vec()),
        Action::Pause(time::millis(80)),
        Action::Send(good[36..44].to_vec()),
        Action::Pause(time::millis(80)),
        Action::Send(good[44..48].to_vec()),
        // At ~310ms this suffix is past the original 250ms cutoff, but
        // before a wrongly renewed 250ms budget after the ~160ms progress.
        Action::Pause(time::millis(150)),
        Action::LateSend(good[48..].to_vec()),
    ];
    let peer = Peer::new(actions);
    config(
        &home,
        peer.address,
        "multiscan200",
        "imu",
        None,
        json!({"timeout_ms":time::ms(250)}),
    );
    let error = Capture::start(&home).finish().failure(4);
    assert_eq!(error["code"], "timeout");
    rejected(&home, &error, &good[..48], good.len(), true);
    archive(&home, peer.address, "multiscan200", "imu", None, &[name]);
    exports(&home, true);
    peer.finish();
}
#[test]
fn archive_quota_floor_and_collision_preserve_only_owned_synced_prefix() {
    let name = "multiscan200_type7_timestamp_first";
    let good = packet(name);
    for mode in ["quota", "floor", "exists"] {
        let home = Home::new("sick-tcp-storage");
        if mode == "exists" {
            fs::create_dir(home.path("capture")).unwrap();
            fs::write(home.path("capture/foreign"), b"operator").unwrap();
        }
        let mut actions = vec![Action::Send(good.clone())];
        if mode == "quota" {
            actions.push(Action::Prefix(home.path("run.jsonl"), 1));
            actions.push(Action::Send(good.clone()));
        }
        let peer = Peer::new(actions);
        let patch = match mode {
            "quota" => json!({"max_bytes":RESERVE+80+32+good.len()+56}),
            "floor" => json!({"min_free_bytes":u64::MAX}),
            _ => json!({}),
        };
        config(&home, peer.address, "multiscan200", "imu", None, patch);
        let error = Capture::start(&home)
            .finish()
            .failure(if mode == "exists" { 1 } else { 6 });
        assert_eq!(
            error["code"],
            if mode == "exists" {
                "exists"
            } else {
                "storage_full"
            }
        );
        if mode == "quota" {
            archive(&home, peer.address, "multiscan200", "imu", None, &[name]);
            assert_eq!(error["details"]["sick_compact_tcp"]["telegrams"], 1);
            exports(&home, true);
        } else {
            assert_eq!(samples(&home), Vec::<Value>::new());
            assert!(!home.path("capture/telegrams.sickcompacttcp").exists());
            if mode == "exists" {
                assert_eq!(fs::read(home.path("capture/foreign")).unwrap(), b"operator");
                assert_eq!(fs::read_dir(home.path("capture")).unwrap().count(), 1);
            }
        }
        peer.finish();
    }
}
#[test]
fn invalid_static_config_refuses_before_output_or_connect() {
    for (patch, exit) in [
        (json!({"profile":"unknown"}), 2),
        (json!({"stream":"unknown"}), 2),
        (json!({"profile":"lrs4000","stream":"imu"}), 5),
        (json!({"profile":"lrs4000","stream":"ambient"}), 5),
        (json!({"address":"localhost:2115"}), 2),
        (json!({"address":"0.0.0.0:2115"}), 2),
        (json!({"max_bytes":0}), 2),
        (json!({"max_bytes":4_210_943}), 2),
        (json!({"packets_per_group":1}), 2),
        (json!({"output_directory":"-"}), 2),
    ] {
        let home = Home::new("sick-tcp-preopen");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        config(
            &home,
            listener.local_addr().unwrap(),
            "multiscan200",
            "imu",
            None,
            patch,
        );
        Capture::start(&home).finish().failure(exit);
        assert!(!home.path("capture").exists());
        assert!(!home.path("run.jsonl").exists());
        match listener.accept() {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("preopen connected: {other:?}"),
        }
    }
}
