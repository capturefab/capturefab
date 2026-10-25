//! The port against `golden.json`, which the main crate's
//! `tests/browser_golden.rs` computes from the real native code:
//! measurements, conversions and encoders exactly, the Pattern camera's
//! feature writes exactly and its pixels within one level.
use capturefab_try::types::{CameraKind, Frame};
use capturefab_try::{frame, inspect, jpeg, png, simulator::Simulator};
use serde_json::{Value, json};

fn golden() -> Value {
    let text = include_str!("golden.json");
    serde_json::from_str(text).expect("golden.json parses")
}

fn fnv1a64(bytes: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .map(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

const MONO10: u32 = 0x0110_0003;
const MONO12: u32 = 0x0110_0005;

fn channels(format: u32) -> u32 {
    if matches!(format, 0x0218_0014 | 0x0218_0015) {
        3
    } else {
        1
    }
}
fn sample_bytes(format: u32) -> usize {
    match format {
        MONO10 | MONO12 | 0x0110_0007 | 0x0110_00b8 | 0x0110_00c7 | 0x0110_0099 => 2,
        0x0120_00bf => 4,
        _ => 1,
    }
}
fn full_scale(format: u32) -> u32 {
    match format {
        MONO10 => 1023,
        MONO12 | 0x0110_0099 => 4095,
        0x0110_0007 | 0x0110_00b8 | 0x0110_00c7 => 65535,
        _ => 255,
    }
}

/// The generators of `tests/browser_golden.rs`.
fn generate(generator: &Value, format: u32, width: u32, height: u32) -> Vec<u8> {
    let kind = generator["kind"].as_str().unwrap();
    let (channels, bytes) = (channels(format), sample_bytes(format));
    let len = (width * height * channels) as usize * bytes;
    let mut data = match kind {
        "bytes" => unhex(generator["hex"].as_str().unwrap()),
        "noise" => {
            let mut state = 0x2545_f491_u32;
            let mut data: Vec<u8> = (0..len)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    state as u8
                })
                .collect();
            if bytes == 2 {
                let top = (full_scale(format) >> 8) as u8;
                for high in data.iter_mut().skip(1).step_by(2) {
                    *high &= top;
                }
            }
            data
        }
        _ => {
            let max = full_scale(format);
            let mut data = Vec::with_capacity(len);
            for y in 0..height {
                for x in 0..width {
                    for c in 0..channels {
                        let value = match kind {
                            "solid" => generator["value"][c as usize].as_u64().unwrap() as u32,
                            "ramp" => {
                                let level = x * max / (width - 1).max(1);
                                let level = if x % 8 < 2 { level / 2 } else { level };
                                match c {
                                    0 => level,
                                    1 => y * max / (height - 1).max(1),
                                    _ => max - level,
                                }
                            }
                            "checker" => {
                                let cell = generator["cell"].as_u64().unwrap() as u32;
                                let on = (x / cell + y / cell) % 2 == 1;
                                match (on, c) {
                                    (true, 1) => max / 2,
                                    (false, 1) => max / 4,
                                    (true, _) => max,
                                    (false, _) => 0,
                                }
                            }
                            "stripes" => {
                                let period = generator["period"].as_u64().unwrap() as u32;
                                let on = (x / period) % 2 == 1;
                                let value = if on { max * 200 / 255 } else { max * 40 / 255 };
                                if c == 2 { value / 2 } else { value }
                            }
                            other => panic!("unknown generator {other}"),
                        };
                        match bytes {
                            1 => data.push(value as u8),
                            2 => data.extend_from_slice(&(value as u16).to_le_bytes()),
                            _ => data.extend_from_slice(&value.to_le_bytes()),
                        }
                    }
                }
            }
            data
        }
    };
    if let Some(length) = generator.get("length").and_then(Value::as_u64) {
        data.truncate(length as usize);
    }
    data
}

fn frame_of(case: &Value) -> Frame {
    let (format, width, height) = (
        case["pixel_format"].as_u64().unwrap() as u32,
        case["width"].as_u64().unwrap() as u32,
        case["height"].as_u64().unwrap() as u32,
    );
    let data = generate(&case["generator"], format, width, height);
    Frame {
        id: 7,
        width,
        height,
        pixel_format: format,
        timestamp_ns: 99,
        data,
    }
}

fn kind(name: &str) -> CameraKind {
    match name {
        "area_scan" => CameraKind::AreaScan,
        "line_scan" => CameraKind::LineScan,
        "depth" => CameraKind::Depth,
        "microscope" => CameraKind::Microscope,
        "thermal" => CameraKind::Thermal,
        "document_scanner" => CameraKind::DocumentScanner,
        _ => CameraKind::Unknown,
    }
}

fn outcome(result: Result<Vec<u8>, String>) -> Value {
    match result {
        Ok(bytes) => json!({ "fnv1a64": fnv1a64(&bytes), "len": bytes.len() }),
        Err(error) => json!({ "error": error }),
    }
}

#[test]
fn measurements_and_conversions_match_native() {
    let golden = golden();
    let frames = golden["frames"].as_array().unwrap();
    assert!(frames.len() > 50);
    let mut checked = 0;
    for case in frames {
        let name = case["name"].as_str().unwrap();
        let frame = frame_of(case);
        assert_eq!(fnv1a64(&frame.data), case["fnv1a64"], "{name}: input bytes");
        for entry in case["measure"].as_array().unwrap() {
            let region: [u32; 4] = serde_json::from_value(entry["region"].clone()).unwrap();
            let measured = inspect::measure(&frame, kind(entry["kind"].as_str().unwrap()), region);
            let actual: Value = serde_json::from_str(&measured.to_json()).unwrap();
            assert_eq!(actual, entry["result"], "{name} measure {region:?}");
            checked += 1;
        }
        for entry in case["inspect"].as_array().unwrap() {
            let region: Option<[f64; 4]> = serde_json::from_value(entry["region"].clone()).unwrap();
            match inspect::inspect(&frame, CameraKind::AreaScan, region) {
                Ok(inspection) => {
                    let actual: Value = serde_json::from_str(&inspection.to_json()).unwrap();
                    assert_eq!(actual, entry["result"], "{name} inspect {region:?}");
                }
                Err(error) => assert_eq!(error, entry["error"], "{name} inspect {region:?}"),
            }
            checked += 1;
        }
        let conversions = &case["conversions"];
        assert_eq!(
            outcome(frame::rgb(&frame)),
            conversions["rgb"],
            "{name} rgb"
        );
        assert_eq!(
            outcome(frame::rgba(&frame)),
            conversions["rgba"],
            "{name} rgba"
        );
        let gray8 = match frame::gray8(&frame) {
            Ok(Some(gray)) => outcome(Ok(gray)),
            Ok(None) => Value::Null,
            Err(error) => outcome(Err(error)),
        };
        assert_eq!(gray8, conversions["gray8"], "{name} gray8");
        for format in ["raw", "ppm", "pgm"] {
            assert_eq!(
                outcome(frame::encode(&frame, format)),
                conversions[format],
                "{name} {format}"
            );
        }
    }
    assert!(checked > 300, "{checked}");
}

/// zlib framing around stored deflate blocks, so the test needs no
/// compressor; the browser's CompressionStream produces the real stream.
fn stored_zlib(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = if data.is_empty() {
        vec![&[]]
    } else {
        data.chunks(65_535).collect()
    };
    for (i, block) in blocks.iter().enumerate() {
        out.push(u8::from(i + 1 == blocks.len()));
        let len = block.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

/// Inflate stored blocks back (the only kind `stored_zlib` writes).
fn unstored(zlib: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut at = 2;
    loop {
        let last = zlib[at] & 1 == 1;
        let len = u16::from_le_bytes([zlib[at + 1], zlib[at + 2]]) as usize;
        out.extend_from_slice(&zlib[at + 5..at + 5 + len]);
        at += 5 + len;
        if last {
            return out;
        }
    }
}

fn unfilter(rows: &[u8], width: u32, channels: usize) -> Vec<u8> {
    let paeth = |a: u8, b: u8, c: u8| {
        let (a16, b16, c16) = (i16::from(a), i16::from(b), i16::from(c));
        let p = a16 + b16 - c16;
        let (pa, pb, pc) = ((p - a16).abs(), (p - b16).abs(), (p - c16).abs());
        if pa <= pb && pa <= pc {
            a
        } else if pb <= pc {
            b
        } else {
            c
        }
    };
    let row = width as usize * channels;
    let mut out: Vec<u8> = Vec::new();
    for (y, line) in rows.chunks_exact(row + 1).enumerate() {
        assert_eq!(line[0], 4, "Paeth filter");
        for i in 0..row {
            let a = if i >= channels {
                out[y * row + i - channels]
            } else {
                0
            };
            let b = if y > 0 { out[(y - 1) * row + i] } else { 0 };
            let c = if y > 0 && i >= channels {
                out[(y - 1) * row + i - channels]
            } else {
                0
            };
            out.push(line[1 + i].wrapping_add(paeth(a, b, c)));
        }
    }
    out
}

/// `frame.rs` `description`: the context's string tags as `key=value; …`.
fn description(meta: &Value) -> Option<String> {
    let context = meta.get("context")?.as_object()?;
    let tags = context
        .iter()
        .filter_map(|(key, value)| Some(format!("{key}={}", value.as_str()?)))
        .collect::<Vec<_>>();
    (!tags.is_empty()).then(|| tags.join("; "))
}

fn chunks(bytes: &[u8]) -> Vec<Value> {
    assert_eq!(&bytes[..8], &png::SIGNATURE);
    let mut chunks = Vec::new();
    let mut at = 8;
    while at < bytes.len() {
        let length = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        let kind = std::str::from_utf8(&bytes[at + 4..at + 8])
            .unwrap()
            .to_string();
        let whole = &bytes[at..at + 12 + length];
        let hex = (kind != "IDAT").then(|| hex(whole));
        chunks.push(json!({ "type": kind, "hex": hex }));
        at += 12 + length;
    }
    chunks
}

#[test]
fn png_and_jpeg_metadata_match_native() {
    let golden = golden();
    for case in golden["png"].as_array().unwrap() {
        let frame = frame_of(case);
        let meta = match (&case["meta_json"], case.get("meta_note_length")) {
            (Value::String(json), _) => Some(json.clone()),
            (_, Some(length)) => Some(format!(
                "{{\"note\":\"{}\"}}",
                "x".repeat(length.as_u64().unwrap() as usize)
            )),
            _ => None,
        };
        let description = meta
            .as_deref()
            .and_then(|meta| description(&serde_json::from_str(meta).unwrap()));
        let (color, samples) = png::samples(&frame).unwrap();
        let channels = if color == png::GRAYSCALE { 1 } else { 3 };
        let rows = png::filtered_rows(&samples, frame.width, frame.height, channels);
        let zlib = stored_zlib(&rows);
        let result = png::assemble(
            frame.width,
            frame.height,
            color,
            &zlib,
            meta.as_deref(),
            description.as_deref(),
        );
        if let Some(error) = case.get("error") {
            assert_eq!(&result.unwrap_err(), error);
            continue;
        }
        let bytes = result.unwrap();
        assert_eq!(chunks(&bytes), case["chunks"].as_array().unwrap().clone());
        let pixels = unfilter(&unstored(&zlib), frame.width, channels);
        assert_eq!(fnv1a64(&pixels), case["pixels_fnv1a64"]);
    }
    for case in golden["jpeg"].as_array().unwrap() {
        let result = jpeg::insert_com(
            &unhex(case["jpeg"].as_str().unwrap()),
            &unhex(case["payload"].as_str().unwrap()),
        );
        match case.get("out") {
            Some(out) => assert_eq!(hex(&result.unwrap()), out.as_str().unwrap()),
            None => assert_eq!(&result.unwrap_err(), &case["error"]),
        }
    }
}

/// The port's FeatureInfo JSON without the extra `locked_while_streaming`.
fn features(camera: &mut Simulator) -> Value {
    let mut features: Value = serde_json::from_str(&camera.features_json()).unwrap();
    for feature in features.as_array_mut().unwrap() {
        let name = feature["name"].as_str().unwrap().to_string();
        let locked = feature
            .as_object_mut()
            .unwrap()
            .remove("locked_while_streaming")
            .unwrap();
        assert_eq!(
            locked,
            matches!(name.as_str(), "Width" | "Height" | "PixelFormat")
        );
    }
    features
}

fn value_json(value: &capturefab_try::genicam::Value) -> Value {
    let mut json = capturefab_try::json::Json::default();
    value.write(&mut json);
    serde_json::from_str(&json.finish()).unwrap()
}

#[test]
fn pattern_camera_features_and_writes_match_native() {
    let golden = golden();
    let sim = &golden["sim"];
    assert_eq!(
        serde_json::from_str::<Value>(&capturefab_try::simulator::info_json()).unwrap(),
        sim["info"]
    );
    let mut camera = Simulator::default();
    assert_eq!(features(&mut camera), sim["features"]);
    for op in sim["ops"].as_array().unwrap() {
        match op["op"].as_str().unwrap() {
            "start" => camera.start().unwrap(),
            "stop" => camera.stop().unwrap(),
            "features" => assert_eq!(features(&mut camera), op["result"]),
            _ => {
                let (name, text) = (op["name"].as_str().unwrap(), op["value"].as_str().unwrap());
                match camera.set(name, text) {
                    Ok(value) => assert_eq!(
                        json!({ "name": name, "value": value_json(&value) }),
                        op["result"],
                        "{name} {text:?}"
                    ),
                    Err(error) => assert_eq!(error, op["error"], "{name} {text:?}"),
                }
            }
        }
    }
}

fn levels(format: u32, bytes: &[u8]) -> Vec<u32> {
    if format == MONO12 {
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| u32::from(u16::from_le_bytes(*p) >> 4))
            .collect()
    } else {
        bytes.iter().map(|&b| u32::from(b)).collect()
    }
}

#[test]
fn pattern_camera_pixels_match_native_within_one_level() {
    let golden = golden();
    let mut exact = 0;
    let mut samples = 0;
    for run in golden["sim"]["runs"].as_array().unwrap() {
        let mut camera = Simulator::default();
        for setting in run["settings"].as_array().unwrap() {
            camera
                .set(setting[0].as_str().unwrap(), setting[1].as_str().unwrap())
                .unwrap();
        }
        camera.start().unwrap();
        let wanted = run["frames"].as_array().unwrap();
        for index in 0..=10u64 {
            let frame = camera.render(index * 1_000_000).unwrap();
            let Some(expected) = wanted.iter().find(|f| f["index"] == index) else {
                continue;
            };
            for key in ["id", "width", "height", "pixel_format", "len"] {
                let actual = match key {
                    "id" => frame.id,
                    "width" => u64::from(frame.width),
                    "height" => u64::from(frame.height),
                    "pixel_format" => u64::from(frame.pixel_format),
                    _ => frame.data.len() as u64,
                };
                assert_eq!(actual, expected[key], "{key}");
            }
            let bytes = frame.data.len() / (frame.width * frame.height) as usize;
            for sample in expected["samples"].as_array().unwrap() {
                let (x, y) = (
                    sample[0].as_u64().unwrap() as u32,
                    sample[1].as_u64().unwrap() as u32,
                );
                let at = (y * frame.width + x) as usize * bytes;
                let pixel = &frame.data[at..at + bytes];
                let actual: Vec<u64> = if frame.pixel_format == MONO12 {
                    let v = u16::from_le_bytes([pixel[0], pixel[1]]);
                    vec![u64::from(v >> 4), u64::from(v & 15)]
                } else {
                    pixel.iter().map(|&b| u64::from(b)).collect()
                };
                for (a, e) in actual.iter().zip(sample[2].as_array().unwrap()) {
                    let e = e.as_u64().unwrap();
                    assert!(a.abs_diff(e) <= 1, "({x}, {y}) frame {index}: {a} vs {e}");
                    exact += usize::from(*a == e);
                    samples += 1;
                }
            }
            let levels = levels(frame.pixel_format, &frame.data);
            assert_eq!(levels.len() as u64, expected["level_count"]);
            let sum: u64 = levels.iter().map(|&v| u64::from(v)).sum();
            let want = expected["level_sum"].as_u64().unwrap();
            assert!(sum.abs_diff(want) <= want / 256 + 1, "{sum} vs {want}");
        }
        camera.stop().unwrap();
    }
    assert!(samples > 1000);
    // The same math library gives the same pixels; allow a few.
    assert!(exact * 100 >= samples * 99, "{exact} of {samples} exact");
}
