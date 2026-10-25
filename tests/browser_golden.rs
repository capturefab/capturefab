//! Golden values for the in-browser build (`web-try/`), computed by the real
//! native code: frame measurements, conversions, encoders and the `sim:0`
//! simulator. `web-try/tests/golden.rs` checks the Rust port against the same
//! file and `scripts/try-tests/engine.test.mjs` checks the shipped wasm.
//!
//! Every frame is described by a deterministic generator plus the FNV-1a-64
//! hash of its bytes, so each consumer proves it measured identical input.
//! Simulator pixels go through `sinf`/`hypotf`, whose last bit differs
//! between C libraries, so they compare within ±1 per sample.
//!
//! `CAPTUREFAB_BLESS=1 cargo test --test browser_golden` rewrites the file.
use capturefab::{
    camera::Camera,
    camera_profile::CameraKind,
    frame, inspect,
    types::{Frame, MONO8, RGB8},
};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};

const BGR8: u32 = 0x0218_0015;
const MONO10: u32 = 0x0110_0003;
const MONO12: u32 = 0x0110_0005;
const MONO16: u32 = 0x0110_0007;
const COORD3D_C8: u32 = 0x0108_00b1;
const COORD3D_C16: u32 = 0x0110_00b8;
const COORD3D_C32F: u32 = 0x0120_00bf;
const CONFIDENCE8: u32 = 0x0108_00c6;
const CONFIDENCE16: u32 = 0x0110_00c7;
const BAYER_GR8: u32 = 0x0108_0008;
const BAYER_RG8: u32 = 0x0108_0009;
const BAYER_GB8: u32 = 0x0108_000a;
const BAYER_BG8: u32 = 0x0108_000b;
const UNKNOWN16: u32 = 0x0110_0099;

const STALE: &str =
    "web-try/tests/golden.json is stale; run CAPTUREFAB_BLESS=1 cargo test --test browser_golden";

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web-try/tests/golden.json")
}

fn fnv1a64(bytes: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// RGB8 and BGR8 carry three samples per pixel; everything else one.
fn channels(format: u32) -> u32 {
    if matches!(format, RGB8 | BGR8) { 3 } else { 1 }
}
/// Bytes per sample: the 16-bit formats (and the unknown test code) two.
fn sample_bytes(format: u32) -> usize {
    match format {
        MONO10 | MONO12 | MONO16 | COORD3D_C16 | CONFIDENCE16 | UNKNOWN16 => 2,
        COORD3D_C32F => 4,
        _ => 1,
    }
}
fn full_scale(format: u32) -> u32 {
    match format {
        MONO10 => 1023,
        MONO12 | UNKNOWN16 => 4095,
        MONO16 | COORD3D_C16 | CONFIDENCE16 => 65535,
        _ => 255,
    }
}

/// The frame generators shared with `web-try/tests/golden.rs` and
/// `scripts/try-tests/engine.test.mjs`; keep the three in step.
fn generate(generator: &Value, format: u32, width: u32, height: u32) -> Vec<u8> {
    let kind = generator["kind"].as_str().unwrap();
    let (channels, bytes) = (channels(format), sample_bytes(format));
    let len = (width * height * channels) as usize * bytes;
    let mut data = match kind {
        "bytes" => (0..generator["hex"].as_str().unwrap().len() / 2)
            .map(|i| {
                u8::from_str_radix(&generator["hex"].as_str().unwrap()[2 * i..2 * i + 2], 16)
                    .unwrap()
            })
            .collect(),
        "noise" => {
            // inspect.rs' xorshift32 test noise, one byte per step.
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

fn kind_name(kind: CameraKind) -> &'static str {
    match kind {
        CameraKind::Unknown => "unknown",
        CameraKind::AreaScan => "area_scan",
        CameraKind::LineScan => "line_scan",
        CameraKind::Depth => "depth",
        CameraKind::Microscope => "microscope",
        CameraKind::Thermal => "thermal",
        CameraKind::DocumentScanner => "document_scanner",
    }
}

/// A conversion's output hash, or its error text.
fn outcome(result: anyhow::Result<Vec<u8>>) -> Value {
    match result {
        Ok(bytes) => json!({ "fnv1a64": fnv1a64(&bytes), "len": bytes.len() }),
        Err(error) => json!({ "error": format!("{error:#}") }),
    }
}

fn frame_case(
    name: &str,
    format: u32,
    (width, height): (u32, u32),
    generator: Value,
    kinds: &[CameraKind],
) -> Value {
    let data = generate(&generator, format, width, height);
    let frame = Frame {
        id: 7,
        width,
        height,
        pixel_format: format,
        timestamp_ns: 99,
        data,
    };
    // Large frames skip the corner and quad-phase regions; small ones cover them.
    let small = width <= 64;
    let mut measures = Vec::new();
    for &kind in kinds {
        for (region, every_size) in [
            ([0, 0, width, height], true),
            ([width / 4, height / 3, width / 2 + 1, height / 2 + 3], true),
            (
                [width.saturating_sub(1), height.saturating_sub(1), 5, 5],
                false,
            ),
            ([3, 1, 2, 2], false),
        ] {
            let whole = region == [0, 0, width, height];
            if (kind != CameraKind::AreaScan && !whole) || !(every_size || small) {
                continue;
            }
            measures.push(json!({
                "kind": kind_name(kind),
                "region": region,
                "result": inspect::measure(&frame, kind, region),
            }));
        }
    }
    let mut inspections = Vec::new();
    for region in [
        None,
        Some([0.1, 0.2, 0.35, 0.5]),
        Some([0.5, 0.5, 0.5, 0.5]),
    ] {
        // The whole frame matches the first measurement; large frames skip it.
        if !small && region != Some([0.1, 0.2, 0.35, 0.5]) {
            continue;
        }
        let entry = match inspect::inspect(
            &frame,
            CameraKind::AreaScan,
            region,
            0,
            inspect::PreviewFormat::Auto,
        ) {
            Ok(inspection) => json!({ "region": region, "result": inspection }),
            Err(error) => json!({ "region": region, "error": format!("{error:#}") }),
        };
        inspections.push(entry);
    }
    let gray8 = match frame::gray8(&frame) {
        Ok(Some(bytes)) => outcome(Ok(bytes)),
        Ok(None) => Value::Null,
        Err(error) => outcome(Err(error)),
    };
    json!({
        "name": name,
        "pixel_format": format,
        "width": width,
        "height": height,
        "generator": generator,
        "fnv1a64": fnv1a64(&frame.data),
        "len": frame.data.len(),
        "measure": measures,
        "inspect": inspections,
        "conversions": {
            "rgb": outcome(frame::rgb(&frame)),
            "gray8": gray8,
            "rgba": outcome(frame::rgba(&frame)),
            "raw": outcome(frame::encode(&frame, "raw")),
            "ppm": outcome(frame::encode(&frame, "ppm")),
            "pgm": outcome(frame::encode(&frame, "pgm")),
        },
    })
}

fn format_label(format: u32) -> &'static str {
    match frame::pixel_format_name(format) {
        "Unknown" => "Unknown16",
        name => name,
    }
}

fn frames() -> Vec<Value> {
    let main = [MONO8, RGB8, MONO12, BAYER_RG8];
    let solid = |format: u32| match format {
        RGB8 => json!({"kind": "solid", "value": [255, 128, 0]}),
        MONO12 => json!({"kind": "solid", "value": [2048]}),
        BAYER_RG8 => json!({"kind": "solid", "value": [200]}),
        _ => json!({"kind": "solid", "value": [120]}),
    };
    let area = [CameraKind::AreaScan];
    let mut cases = Vec::new();
    for (width, height) in [(33, 17), (64, 32), (640, 480)] {
        for format in main {
            let cell = if width < 64 { 4 } else { 8 };
            let generators = match width {
                33 => vec![
                    solid(format),
                    json!({"kind": "ramp"}),
                    json!({"kind": "noise"}),
                    json!({"kind": "checker", "cell": cell}),
                    json!({"kind": "stripes", "period": 3}),
                ],
                64 => vec![
                    solid(format),
                    json!({"kind": "ramp"}),
                    json!({"kind": "noise"}),
                ],
                _ => vec![
                    json!({"kind": "ramp"}),
                    json!({"kind": "noise"}),
                    json!({"kind": "checker", "cell": cell}),
                ],
            };
            for generator in generators {
                let name = format!(
                    "{}-{}-{width}x{height}",
                    format_label(format),
                    generator["kind"].as_str().unwrap()
                );
                cases.push(frame_case(&name, format, (width, height), generator, &area));
            }
        }
    }
    for (width, height) in [(1, 1), (2, 3), (4, 1)] {
        for format in main {
            let name = format!("{}-noise-{width}x{height}", format_label(format));
            let generator = json!({"kind": "noise"});
            cases.push(frame_case(&name, format, (width, height), generator, &area));
        }
    }
    for format in main {
        for generator in [json!({"kind": "ramp"}), json!({"kind": "noise"})] {
            let name = format!(
                "{}-{}-1280x720",
                format_label(format),
                generator["kind"].as_str().unwrap()
            );
            cases.push(frame_case(&name, format, (1280, 720), generator, &area));
        }
    }
    for format in [
        BGR8,
        MONO10,
        MONO16,
        COORD3D_C8,
        COORD3D_C16,
        CONFIDENCE8,
        CONFIDENCE16,
        BAYER_GR8,
        BAYER_GB8,
        BAYER_BG8,
        COORD3D_C32F,
        UNKNOWN16,
    ] {
        let name = format!("{}-noise-33x17", format_label(format));
        cases.push(frame_case(
            &name,
            format,
            (33, 17),
            json!({"kind": "noise"}),
            &area,
        ));
    }
    // measure.md's worked example B, frame.rs' exposure fixture.
    cases.push(frame_case(
        "RGB8-example-4x1",
        RGB8,
        (4, 1),
        json!({"kind": "bytes", "hex": "000000ffffff808080ff0000"}),
        &area,
    ));
    cases.push(frame_case(
        "Mono8-truncated-8x8",
        MONO8,
        (8, 8),
        json!({"kind": "noise", "length": 10}),
        &area,
    ));
    cases.push(frame_case(
        "Mono8-ramp-kinds-128x64",
        MONO8,
        (128, 64),
        json!({"kind": "ramp"}),
        &[
            CameraKind::Unknown,
            CameraKind::AreaScan,
            CameraKind::LineScan,
            CameraKind::Depth,
            CameraKind::Thermal,
            CameraKind::Microscope,
        ],
    ));
    cases
}

fn png_chunks(bytes: &[u8]) -> Vec<Value> {
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    let mut chunks = Vec::new();
    let mut at = 8;
    while at < bytes.len() {
        let length = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        let kind = std::str::from_utf8(&bytes[at + 4..at + 8])
            .unwrap()
            .to_string();
        let whole = &bytes[at..at + 12 + length];
        let hex: Option<String> =
            (kind != "IDAT").then(|| whole.iter().map(|b| format!("{b:02x}")).collect());
        chunks.push(json!({ "type": kind, "hex": hex }));
        at += 12 + length;
    }
    chunks
}

fn pngs() -> Vec<Value> {
    let meta_with_context = json!({
        "camera": {"model": "Pattern camera", "vendor": "Capturefab"},
        "capturefab": "browser",
        "context": {"station": "5", "line": "L1", "count": 3},
        "frame_id": 3,
        "height": 17,
        "pixel_format": "Mono8",
        "settings": {"ExposureTime": 10000.0, "Gain": 0.0},
        "timestamp_ns": 123,
        "wall_ms": 1_792_017_024_123_u64,
        "width": 33,
        "note": "naïve “quotes” \\ \" \u{1}"
    });
    let meta_plain = json!({
        "capturefab": "browser",
        "context": {"count": 3},
        "frame_id": 1,
        "pixel_format": "RGB8",
    });
    let mut cases = Vec::new();
    for (format, generator, meta) in [
        (MONO8, json!({"kind": "ramp"}), Some(&meta_with_context)),
        (RGB8, json!({"kind": "noise"}), Some(&meta_plain)),
        (MONO12, json!({"kind": "checker", "cell": 4}), None),
        (BAYER_RG8, json!({"kind": "ramp"}), Some(&meta_plain)),
    ] {
        let (width, height) = (33, 17);
        let frame = Frame {
            id: 1,
            width,
            height,
            pixel_format: format,
            timestamp_ns: 0,
            data: generate(&generator, format, width, height),
        };
        let bytes = frame::encode_with_meta(&frame, "png", meta).unwrap();
        // Decoded through the png crate: the pixels a viewer sees.
        let mut reader = png::Decoder::new(bytes.as_slice()).read_info().unwrap();
        let mut pixels = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut pixels).unwrap();
        pixels.truncate(info.buffer_size());
        cases.push(json!({
            "pixel_format": format,
            "width": width,
            "height": height,
            "generator": generator,
            "meta_json": meta.map(|meta| serde_json::to_string(meta).unwrap()),
            "chunks": png_chunks(&bytes),
            "pixels_fnv1a64": fnv1a64(&pixels),
        }));
    }
    let too_big = json!({ "note": "x".repeat(frame::MAX_META) });
    let frame = Frame {
        id: 1,
        width: 2,
        height: 1,
        pixel_format: MONO8,
        timestamp_ns: 0,
        data: vec![1, 2],
    };
    cases.push(json!({
        "pixel_format": MONO8,
        "width": 2,
        "height": 1,
        "generator": {"kind": "bytes", "hex": "0102"},
        // `{"note":"xx…"}` with this many x: just over the limit.
        "meta_note_length": frame::MAX_META,
        "error": format!("{:#}", frame::encode_with_meta(&frame, "png", Some(&too_big)).unwrap_err()),
    }));
    cases
}

fn jpegs() -> Vec<Value> {
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let mut cases = Vec::new();
    for (jpeg, payload) in [
        (
            vec![
                0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 1, 2, 0xFF, 0xC0, 0, 2, 0xFF, 0xD9,
            ],
            b"{\"a\":1}".to_vec(),
        ),
        (
            vec![
                0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 1, 2, 0xFF, 0xE1, 0, 3, 9, 0xFF, 0xDB, 0, 2, 0xFF,
                0xD9,
            ],
            "{\"camera\":\"é\"}".as_bytes().to_vec(),
        ),
        (vec![0xFF, 0xD8, 0xFF, 0xD9], b"x".to_vec()),
        (b"\x89PNG".to_vec(), b"x".to_vec()),
        (
            vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 40, 0xFF, 0xD9],
            b"x".to_vec(),
        ),
    ] {
        cases.push(match capturefab::jpeg::insert_com(&jpeg, &payload) {
            Ok(out) => json!({ "jpeg": hex(&jpeg), "payload": hex(&payload), "out": hex(&out) }),
            Err(error) => {
                json!({ "jpeg": hex(&jpeg), "payload": hex(&payload), "error": format!("{error:#}") })
            }
        });
    }
    cases
}

/// Fixed sample points: the simulator report's hand-checked pixels, the
/// marker line, then xorshift32 points inside the frame.
fn sample_points(width: u32, height: u32) -> Vec<[u32; 2]> {
    let mut points: Vec<[u32; 2]> = [
        [0, 0],
        [5, 5],
        [100, 100],
        [320, 100],
        [319, 239],
        [width - 1, height - 1],
        [width / 2, height / 2],
        [7, height / 2],
    ]
    .into_iter()
    .filter(|[x, y]| *x < width && *y < height)
    .collect();
    let mut state = 0x2545_f491_u32;
    while points.len() < 64 {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        points.push([state % width, (state >> 16) % height]);
    }
    points
}

/// Per-pixel 8-bit levels: Mono12 samples shifted to their top byte, other
/// formats byte by byte.
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

fn sim() -> Value {
    let timeout = Duration::from_secs(5);
    let mut camera = Camera::open("sim:0", &[], timeout).unwrap();
    let info = json!({
        "id": camera.info.id,
        "vendor": camera.info.vendor,
        "model": camera.info.model,
        "serial": camera.info.serial,
        "transport": camera.info.transport,
    });
    let features = camera.features();
    // Feature writes and their re-read values or error texts, in order.
    let mut ops = Vec::new();
    let steps: &[(&str, &str)] = &[
        ("Width", "9999"),
        ("Width", "63"),
        ("Width", "abc"),
        ("Width", "800.5"),
        ("Width", ""),
        ("Width", "0x320"),
        ("Width", "1e3"),
        ("Width", "-5"),
        ("Height", " 600 "),
        ("Height", "1080"),
        ("Height", "480"),
        ("Gain", "-1"),
        ("Gain", "24.5"),
        ("Gain", "nan"),
        ("Gain", "inf"),
        ("Gain", "1e-400"),
        ("Gain", "6"),
        ("Gain", "0x2"),
        ("Gain", "3.25"),
        ("ExposureTime", "9.99"),
        ("ExposureTime", "1000001"),
        ("ExposureTime", "5000"),
        ("ExposureTime", "12345.678"),
        ("AcquisitionFrameRate", "0"),
        ("AcquisitionFrameRate", "120"),
        ("AcquisitionFrameRate", "60.5"),
        ("PixelFormat", "rgb8"),
        ("PixelFormat", "Mono16"),
        ("PixelFormat", "RGB8"),
        ("PixelFormat", " Mono8"),
        ("DeviceScanType", "Linescan"),
        ("DeviceScanType", "nope"),
        ("AcquisitionStart", "1"),
        ("AcquisitionStop", ""),
        ("Nope", "1"),
        ("Root", "1"),
        ("WidthReg", "10"),
        ("WidthReg", "800"),
        ("ExposureReg", "5"),
        ("PayloadSize", "5"),
        ("AcqReg", "2"),
        ("Width", "640"),
        ("PixelFormat", "Mono8"),
        ("start", ""),
        ("Width", "800"),
        ("Width", "9999"),
        ("PixelFormat", "Mono8"),
        ("Height", "100"),
        ("WidthReg", "640"),
        ("Gain", "12"),
        ("ExposureTime", "20000"),
        ("AcquisitionFrameRate", "30"),
        ("AcqReg", "1"),
        ("features", ""),
        ("stop", ""),
        ("Width", "640"),
        ("Gain", "0"),
        ("ExposureTime", "10000"),
        ("AcquisitionFrameRate", "30"),
        ("features", ""),
    ];
    for &(name, value) in steps {
        let op = match name {
            "start" => {
                camera.start().unwrap();
                json!({ "op": "start" })
            }
            "stop" => {
                camera.stop().unwrap();
                json!({ "op": "stop" })
            }
            "features" => json!({ "op": "features", "result": camera.features() }),
            _ => {
                let result = camera
                    .validate_set(name, value)
                    .and_then(|()| camera.set(name, value))
                    .and_then(|()| camera.get(name));
                match result {
                    Ok(read) => json!({
                        "op": "set", "name": name, "value": value,
                        "result": { "name": name, "value": read },
                    }),
                    Err(error) => json!({
                        "op": "set", "name": name, "value": value,
                        "error": format!("{error:#}"),
                    }),
                }
            }
        };
        ops.push(op);
    }
    drop(camera);

    let mut runs = Vec::new();
    for (format, settings) in [
        ("Mono8", vec![]),
        ("RGB8", vec![]),
        ("Mono12", vec![]),
        ("BayerRG8", vec![]),
        ("RGB8", vec![("ExposureTime", "5000"), ("Gain", "6")]),
        (
            "Mono8",
            vec![
                ("Width", "800"),
                ("Height", "600"),
                ("ExposureTime", "20000"),
                ("Gain", "3.5"),
            ],
        ),
        (
            "BayerRG8",
            vec![("Width", "333"), ("Height", "65"), ("Gain", "24")],
        ),
    ] {
        let mut camera = Camera::open("sim:0", &[], timeout).unwrap();
        camera.set("AcquisitionFrameRate", "120").unwrap();
        camera.set("PixelFormat", format).unwrap();
        for (name, value) in &settings {
            camera.set(name, value).unwrap();
        }
        camera.start().unwrap();
        let mut frames = Vec::new();
        for index in 0..=10 {
            let frame = camera.next_frame(timeout).unwrap();
            if ![0, 1, 10].contains(&index) {
                continue;
            }
            let bytes = (frame.data.len() / (frame.width * frame.height) as usize).max(1);
            let samples: Vec<Value> = sample_points(frame.width, frame.height)
                .into_iter()
                .map(|[x, y]| {
                    let at = (y * frame.width + x) as usize * bytes;
                    let pixel = &frame.data[at..at + bytes];
                    if frame.pixel_format == MONO12 {
                        // The 8-bit level, then the texture nibble.
                        let v = u16::from_le_bytes([pixel[0], pixel[1]]);
                        json!([x, y, [v >> 4, v & 15]])
                    } else {
                        json!([x, y, pixel])
                    }
                })
                .collect();
            let levels = levels(frame.pixel_format, &frame.data);
            frames.push(json!({
                "index": index,
                "id": frame.id,
                "width": frame.width,
                "height": frame.height,
                "pixel_format": frame.pixel_format,
                "len": frame.data.len(),
                "samples": samples,
                "level_count": levels.len(),
                "level_sum": levels.iter().map(|&v| u64::from(v)).sum::<u64>(),
            }));
        }
        camera.stop().unwrap();
        let settings: Vec<Value> = std::iter::once(("AcquisitionFrameRate", "120"))
            .chain(std::iter::once(("PixelFormat", format)))
            .chain(settings)
            .map(|(name, value)| json!([name, value]))
            .collect();
        runs.push(json!({ "settings": settings, "frames": frames }));
    }
    json!({ "info": info, "features": features, "ops": ops, "runs": runs })
}

fn golden() -> Value {
    json!({
        "about": "Generated by tests/browser_golden.rs from the native code; CAPTUREFAB_BLESS=1 cargo test --test browser_golden rewrites it.",
        "version": 1,
        "frames": frames(),
        "png": pngs(),
        "jpeg": jpegs(),
        "sim": sim(),
    })
}

/// JSON with one measurement, operation or short value per line, so diffs
/// stay readable without spreading every histogram over 16 lines.
fn render(value: &Value, indent: usize, out: &mut String) {
    let compact = serde_json::to_string(value).unwrap();
    if compact.len() + indent <= 120 || indent >= 8 || !(value.is_array() || value.is_object()) {
        out.push_str(&compact);
        return;
    }
    let pad = " ".repeat(indent + 2);
    match value {
        Value::Array(items) => {
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                out.push_str(&pad);
                render(item, indent + 2, out);
                out.push_str(if i + 1 < items.len() { ",\n" } else { "\n" });
            }
            out.push_str(&" ".repeat(indent));
            out.push(']');
        }
        Value::Object(fields) => {
            out.push_str("{\n");
            for (i, (key, item)) in fields.iter().enumerate() {
                out.push_str(&pad);
                out.push_str(&serde_json::to_string(key).unwrap());
                out.push_str(": ");
                render(item, indent + 2, out);
                out.push_str(if i + 1 < fields.len() { ",\n" } else { "\n" });
            }
            out.push_str(&" ".repeat(indent));
            out.push('}');
        }
        _ => unreachable!(),
    }
}

/// Exact equality, except simulator pixels: ±1 per 8-bit level and a level
/// sum within one per 256 levels, since `sinf`/`hypotf` differ by platform.
fn same(expected: &Value, actual: &Value, path: &str) -> Result<(), String> {
    let sim_pixels = path.starts_with("/sim/runs/");
    if sim_pixels && path.ends_with("/level_sum") {
        let (e, a) = (
            expected.as_u64().unwrap_or(0),
            actual.as_u64().unwrap_or(u64::MAX),
        );
        return if e.abs_diff(a) <= e / 256 + 1 {
            Ok(())
        } else {
            Err(format!(
                "{path}: {a} differs from {e} by more than the tolerance"
            ))
        };
    }
    match (expected, actual) {
        (Value::Array(e), Value::Array(a)) if e.len() == a.len() => {
            for (i, (e, a)) in e.iter().zip(a).enumerate() {
                same(e, a, &format!("{path}/{i}"))?;
            }
            Ok(())
        }
        (Value::Object(e), Value::Object(a)) if e.len() == a.len() => {
            for (key, e) in e {
                let a = a
                    .get(key)
                    .ok_or_else(|| format!("{path}/{key} is missing"))?;
                same(e, a, &format!("{path}/{key}"))?;
            }
            Ok(())
        }
        (Value::Number(e), Value::Number(a))
            if sim_pixels && path.contains("/samples/") && e.is_u64() && a.is_u64() =>
        {
            if e.as_u64().unwrap().abs_diff(a.as_u64().unwrap()) <= 1 {
                Ok(())
            } else {
                Err(format!("{path}: {a} is not within 1 of {e}"))
            }
        }
        _ if expected == actual => Ok(()),
        _ => Err(format!("{path}: expected {expected}, got {actual}")),
    }
}

#[test]
fn browser_build_matches_the_native_measurements() {
    let value = golden();
    let mut text = String::new();
    render(&value, 0, &mut text);
    text.push('\n');
    let path = golden_path();
    if std::env::var_os("CAPTUREFAB_BLESS").is_some_and(|v| v == "1") {
        std::fs::write(&path, text).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}; {STALE}", path.display()));
    let committed: Value = serde_json::from_str(&committed)
        .unwrap_or_else(|error| panic!("{}: {error}; {STALE}", path.display()));
    if let Err(difference) = same(&committed, &value, "") {
        panic!("{difference}\n{STALE}");
    }
}
