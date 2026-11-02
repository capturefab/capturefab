#[cfg(feature = "jpeg")]
use capturefab::testing::jpeg;
use capturefab::testing::{
    auto, frame,
    session::{SessionCommand, SessionHandle},
    storage::{self, SyncMode},
    types::{Frame, MONO8, RGB8},
};
use sha2::{Digest, Sha256};
use std::{
    hint::black_box,
    path::PathBuf,
    time::{Duration, Instant},
};

const FORMATS: [(&str, u32); 20] = [
    ("Mono8", MONO8),
    ("RGB8", RGB8),
    ("BGR8", 0x0218_0015),
    ("Mono10", 0x0110_0003),
    ("Mono12", 0x0110_0005),
    ("Mono16", 0x0110_0007),
    ("BayerRG8", 0x0108_0009),
    ("BayerRG12", 0x0110_0011),
    ("Mono10p", 0x010a_0046),
    ("Mono12Packed", 0x010c_0006),
    ("Mono14p", 0x010e_0104),
    ("BayerRG12p", 0x010c_0059),
    ("BayerRG14p", 0x010e_0106),
    ("RGBa8", 0x0220_0016),
    ("RGB12", 0x0230_001a),
    ("RGB12p", 0x0224_005d),
    ("RGB8_Planar", 0x0218_0021),
    ("RGB16_Planar", 0x0230_0024),
    ("YUV8_UYV", 0x0218_0020),
    ("YCbCr709_422_8", 0x0210_0041),
];

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn scene(width: u32, height: u32) -> Vec<u8> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
    for y in 0..height {
        for x in 0..width {
            let (fx, fy) = (x as f32 / width as f32, y as f32 / height as f32);
            let texture = 18.0 * (x as f32 * 0.21).sin() * (y as f32 * 0.17).cos();
            for base in [
                90.0 + 70.0 * (fx * 3.1 + fy * 1.7).sin(),
                110.0 + 60.0 * (fy * 4.3 - fx * 0.9).cos(),
                80.0 + 50.0 * (fx * fy * 9.0).sin(),
            ] {
                let noise = (rng.next() % 5 + rng.next() % 5) as f32 - 4.0;
                rgb.push((base + texture + noise).clamp(0.0, 255.0) as u8);
            }
        }
    }
    rgb
}

/// Fixture packing, outside the timed operations. Every row starts on a byte.
fn packed_rows(samples: &[u16], width: usize, bits: usize, gige: bool) -> Vec<u8> {
    let mut output = Vec::new();
    for row in samples.chunks_exact(width) {
        if gige {
            let shift = bits - 8;
            let mask = (1 << shift) - 1;
            for pair in row.chunks(2) {
                output.extend_from_slice(&[(pair[0] >> shift) as u8, (pair[0] & mask) as u8]);
                if let Some(&second) = pair.get(1) {
                    *output.last_mut().unwrap() |= ((second & mask) << 4) as u8;
                    output.push((second >> shift) as u8);
                }
            }
        } else {
            let (mut word, mut present) = (0u32, 0);
            for &sample in row {
                word |= u32::from(sample) << present;
                present += bits;
                while present >= 8 {
                    output.push(word as u8);
                    word >>= 8;
                    present -= 8;
                }
            }
            if present > 0 {
                output.push(word as u8);
            }
        }
    }
    output
}

fn frame(width: u32, height: u32, pixel_format: u32) -> Frame {
    let rgb = scene(width, height);
    let pixels = rgb.as_chunks::<3>().0;
    let luma =
        |p: &[u8; 3]| ((p[0] as u32 * 54 + p[1] as u32 * 183 + p[2] as u32 * 19) >> 8) as u16;
    let data = match pixel_format {
        MONO8 => pixels.iter().map(|p| luma(p) as u8).collect(),
        RGB8 => rgb.clone(),
        0x0218_0015 => pixels.iter().flat_map(|p| [p[2], p[1], p[0]]).collect(),
        0x0220_0016 => pixels
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        0x0218_0021 => (0..3)
            .flat_map(|c| pixels.iter().map(move |p| p[c]))
            .collect(),
        0x0230_001a => rgb
            .iter()
            .enumerate()
            .flat_map(|(i, &v)| ((u16::from(v) << 4) | (i as u16 & 15)).to_le_bytes())
            .collect(),
        0x0230_0024 => (0..3)
            .flat_map(|c| {
                pixels.iter().enumerate().flat_map(move |(i, p)| {
                    ((u16::from(p[c]) << 8) | (i as u16 & 255)).to_le_bytes()
                })
            })
            .collect(),
        0x0224_005d => {
            let samples: Vec<u16> = rgb
                .iter()
                .enumerate()
                .map(|(i, &v)| (u16::from(v) << 4) | (i as u16 & 15))
                .collect();
            packed_rows(&samples, width as usize * 3, 12, false)
        }
        0x010a_0046 | 0x010c_0006 | 0x010e_0104 | 0x010c_0059 | 0x010e_0106 | 0x0110_0011 => {
            let bits = match pixel_format {
                0x010a_0046 => 10,
                0x010c_0006 | 0x010c_0059 | 0x0110_0011 => 12,
                _ => 14,
            };
            let bayer = matches!(pixel_format, 0x010c_0059 | 0x010e_0106 | 0x0110_0011);
            let samples: Vec<u16> = pixels
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let (x, y) = (i % width as usize, i / width as usize);
                    let value = if bayer {
                        u16::from(p[[0, 1, 1, 2][(y & 1) * 2 + (x & 1)]])
                    } else {
                        luma(p)
                    };
                    (value << (bits - 8)) | (i as u16 & ((1 << (bits - 8)) - 1))
                })
                .collect();
            if pixel_format == 0x0110_0011 {
                samples.iter().flat_map(|v| v.to_le_bytes()).collect()
            } else {
                packed_rows(&samples, width as usize, bits, pixel_format == 0x010c_0006)
            }
        }
        0x0218_0020 | 0x0210_0041 => {
            let limited = pixel_format == 0x0210_0041;
            let yuv = |p: &[u8; 3]| {
                let [r, g, b] = p.map(f64::from);
                let values = if limited {
                    [
                        16.0 + 0.18259 * r + 0.61423 * g + 0.06201 * b,
                        128.0 - 0.10064 * r - 0.33857 * g + 0.43922 * b,
                        128.0 + 0.43922 * r - 0.39894 * g - 0.04027 * b,
                    ]
                } else {
                    [
                        0.299 * r + 0.587 * g + 0.114 * b,
                        128.0 - 0.16874 * r - 0.33126 * g + 0.5 * b,
                        128.0 + 0.5 * r - 0.41869 * g - 0.08131 * b,
                    ]
                };
                values.map(|v| v.round().clamp(0.0, 255.0) as u8)
            };
            if limited {
                pixels
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .flat_map(|p| {
                        let (a, b) = (yuv(&p[0]), yuv(&p[1]));
                        let mean = |c: usize| (u16::from(a[c]) + u16::from(b[c])).div_ceil(2) as u8;
                        [a[0], mean(1), b[0], mean(2)]
                    })
                    .collect()
            } else {
                pixels
                    .iter()
                    .flat_map(|p| {
                        let [y, cb, cr] = yuv(p);
                        [cb, y, cr]
                    })
                    .collect()
            }
        }
        0x0110_0003 | 0x0110_0005 | 0x0110_0007 => {
            let shift = match pixel_format {
                0x0110_0003 => 2,
                0x0110_0005 => 4,
                _ => 8,
            };
            let mut rng = Rng(7);
            pixels
                .iter()
                .flat_map(|p| {
                    ((luma(p) << shift) | (rng.next() as u16 & ((1 << shift) - 1))).to_le_bytes()
                })
                .collect()
        }
        0x0108_0009 => (0..height as usize)
            .flat_map(|y| {
                let pixels = &pixels;
                (0..width as usize).map(move |x| {
                    pixels[y * width as usize + x][[0, 1, 1, 2][(y & 1) * 2 + (x & 1)]]
                })
            })
            .collect(),
        _ => panic!("missing benchmark fixture for {pixel_format:#010x}"),
    };
    Frame {
        id: 1,
        width,
        height,
        pixel_format,
        device_ts_ns: None,
        wall_ms: 0,
        data: data.into(),
    }
}

struct Bench {
    filter: Vec<String>,
}
impl Bench {
    fn selected(&self, name: &str) -> bool {
        self.filter.is_empty() || self.filter.iter().any(|f| name.contains(f.as_str()))
    }
    /// The median time in seconds, None when `name` is filtered out.
    fn run<T>(&self, name: &str, bytes: usize, mut f: impl FnMut() -> T) -> Option<f64> {
        if !self.selected(name) {
            return None;
        }
        black_box(f());
        let mut samples = Vec::new();
        let start = Instant::now();
        while samples.len() < 7
            || (start.elapsed() < Duration::from_millis(800) && samples.len() < 500)
        {
            let t = Instant::now();
            black_box(f());
            samples.push(t.elapsed().as_secs_f64());
        }
        samples.sort_by(f64::total_cmp);
        let median = samples[samples.len() / 2];
        println!(
            "{name:<40} median {:>9.3} ms  min {:>9.3} ms  {:>7.0} MB/s  n={}",
            median * 1e3,
            samples[0] * 1e3,
            bytes as f64 / median / 1e6,
            samples.len()
        );
        Some(median)
    }
}

/// Packed formats and the unpacked format each must convert within
/// `PACKED_LIMIT` times the time of, when both are benchmarked. Packed
/// samples are unpacked a group at a time per row; reading each one on its
/// own made `rgb` of Mono10p 15 times slower than Mono10.
const PACKED_BASELINES: [(&str, &str); 5] = [
    ("Mono10p", "Mono10"),
    ("Mono12Packed", "Mono12"),
    ("Mono14p", "Mono12"),
    ("BayerRG12p", "BayerRG12"),
    ("BayerRG14p", "BayerRG12"),
];
const PACKED_LIMIT: f64 = 6.0;

fn png(
    rgb: &[u8],
    width: u32,
    height: u32,
    color: png::ColorType,
    compression: png::Compression,
    filter: Option<png::FilterType>,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, width, height);
    encoder.set_color(color);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(compression);
    match filter {
        Some(filter) => encoder.set_filter(filter),
        None => encoder.set_adaptive_filter(png::AdaptiveFilterType::Adaptive),
    }
    encoder
        .write_header()
        .unwrap()
        .write_image_data(rgb)
        .unwrap();
    bytes
}

fn temp(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("capturefab-bench-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    path
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.iter().any(|a| a == "--bench") {
        return;
    }
    let b = Bench {
        filter: args.into_iter().filter(|a| !a.starts_with("--")).collect(),
    };
    for (width, height) in [(1920, 1200), (640, 480)] {
        let mut medians = std::collections::HashMap::new();
        for (name, format) in FORMATS {
            let label = format!("rgb/{name}/{width}x{height}");
            if !b.selected(&label) {
                continue;
            }
            let f = frame(width, height, format);
            if let Some(median) = b.run(&label, f.data.len(), || frame::rgb(&f).unwrap()) {
                medians.insert(name, median);
            }
        }
        for (packed, baseline) in PACKED_BASELINES {
            if let (Some(packed_time), Some(baseline_time)) =
                (medians.get(packed), medians.get(baseline))
            {
                let ratio = packed_time / baseline_time;
                assert!(
                    ratio <= PACKED_LIMIT,
                    "rgb/{packed}/{width}x{height} takes {ratio:.1} times rgb/{baseline}"
                );
            }
        }
    }
    // Sparse consumers decode the samples they use from the sensor layout.
    // The expanded comparison includes constructing a whole unpacked image,
    // which is avoidable even when only a scope is needed. For wide RGB,
    // expansion is an 8-bit visualization; native metering and clipping
    // also retain the original component precision.
    for (name, format) in FORMATS {
        let suffix = format!("/{name}/2448x2048");
        let prefixes = [
            "pixels/exposure/native",
            "pixels/exposure/expanded",
            "pixels/meter/native",
            "pixels/meter/expanded",
            "pixels/clipping/native",
            "pixels/rgba/reused",
        ];
        if !prefixes
            .iter()
            .any(|prefix| b.selected(&format!("{prefix}{suffix}")))
        {
            continue;
        }
        let f = frame(2448, 2048, format);
        assert_eq!(
            f.data.len(),
            frame::row_bytes(f.width, format) * f.height as usize
        );
        b.run(
            &format!("pixels/exposure/native{suffix}"),
            f.data.len(),
            || frame::exposure(&f).unwrap(),
        );
        b.run(
            &format!("pixels/exposure/expanded{suffix}"),
            f.data.len(),
            || frame::exposure(&frame::unpacked(&f).unwrap()).unwrap(),
        );
        b.run(
            &format!("pixels/meter/native{suffix}"),
            f.data.len(),
            || auto::meter(&f).unwrap(),
        );
        b.run(
            &format!("pixels/meter/expanded{suffix}"),
            f.data.len(),
            || auto::meter(&frame::unpacked(&f).unwrap()).unwrap(),
        );
        b.run(
            &format!("pixels/clipping/native{suffix}"),
            f.data.len(),
            || frame::clipping(&f).unwrap(),
        );
        let mut rgba = Vec::new();
        b.run(&format!("pixels/rgba/reused{suffix}"), f.data.len(), || {
            frame::convert_into(&f, &mut rgba, |[r, g, b]| [r, g, b, 255]).unwrap();
            black_box(&rgba);
        });
    }
    for (name, format) in [("Mono8", MONO8), ("RGB8", RGB8), ("BayerRG8", 0x0108_0009)] {
        let f = frame(1920, 1200, format);
        for encoding in ["png", "ppm", "raw"] {
            b.run(
                &format!("encode/{encoding}/{name}/1920x1200"),
                f.data.len(),
                || frame::encode(&f, encoding).unwrap(),
            );
        }
    }
    // Stills as captures encode them, with their provenance metadata: raw
    // and JPEG from the frame in place where the encoder can read it.
    let meta = serde_json::json!({"camera": "sim:0", "time": "2026-11-01T00:00:00Z"});
    for (name, format, width, height) in [
        ("Mono8", MONO8, 4096, 3000),
        ("BayerRG12p", 0x010c_0059, 4096, 3000),
        ("RGB8", RGB8, 1920, 1080),
    ] {
        let label = format!("encode/raw/{name}/{width}x{height}");
        if b.selected(&label) {
            let f = frame(width, height, format);
            b.run(&label, f.data.len(), || {
                capturefab::testing::frame::encode_parts(&f, "raw", None, Some(&meta)).unwrap()
            });
        }
    }
    #[cfg(feature = "jpeg")]
    if b.selected("encode/jpeg") {
        let f = frame(4096, 3000, MONO8);
        for (label, quality) in [("jpeg", None), ("jpeg-q75", Some(75))] {
            b.run(
                &format!("encode/{label}/Mono8/4096x3000"),
                f.data.len(),
                || {
                    capturefab::testing::frame::encode_parts(&f, "jpeg", quality, Some(&meta))
                        .unwrap()
                },
            );
        }
    }
    // A frame-sized buffer, as a camera's receive loop takes one per frame.
    b.run("pool/vec-zeroed/12MB", 12 << 20, || vec![0u8; 12 << 20]);
    let pool = capturefab::testing::framebuf::pool();
    b.run("pool/take/12MB", 12 << 20, || pool.take_stale(12 << 20));
    // The stage's scopes on a common 5 MP industrial sensor: the histogram,
    // and focus in the default quarter region and over the whole frame.
    let f = frame(2448, 2048, 0x0108_0009);
    b.run("scopes/exposure/BayerRG8/2448x2048", f.data.len(), || {
        frame::exposure(&f).unwrap()
    });
    for (label, region) in [
        ("quarter", [918, 768, 612, 512]),
        ("full", [0, 0, 2448, 2048]),
    ] {
        b.run(
            &format!("scopes/sharpness/{label}/BayerRG8/2448x2048"),
            f.data.len(),
            || frame::sharpness(&f, region).unwrap(),
        );
    }
    let mono = frame(1920, 1200, MONO8);
    b.run("encode/pgm/Mono8/1920x1200", mono.data.len(), || {
        frame::encode(&mono, "pgm").unwrap()
    });
    let bayer = frame(1920, 1200, 0x0108_0009);
    let rgb = frame::rgb(&bayer).unwrap();
    for (label, compression, filter) in [
        (
            "fast-sub",
            png::Compression::Fast,
            Some(png::FilterType::Sub),
        ),
        ("fast-up", png::Compression::Fast, Some(png::FilterType::Up)),
        (
            "fast-avg",
            png::Compression::Fast,
            Some(png::FilterType::Avg),
        ),
        (
            "fast-paeth",
            png::Compression::Fast,
            Some(png::FilterType::Paeth),
        ),
        ("fast-adaptive", png::Compression::Fast, None),
        (
            "default-sub",
            png::Compression::Default,
            Some(png::FilterType::Sub),
        ),
        ("default-adaptive", png::Compression::Default, None),
        ("best-adaptive", png::Compression::Best, None),
    ] {
        for (content, data, color) in [
            ("BayerRG8-rgb", &rgb[..], png::ColorType::Rgb),
            ("Mono8-gray", &mono.data[..], png::ColorType::Grayscale),
        ] {
            let name = format!("png-settings/{label}/{content}");
            if b.selected(&name) {
                println!(
                    "{name:<40} size {:>9} bytes ({:.1}% of raw)",
                    png(data, 1920, 1200, color, compression, filter).len(),
                    png(data, 1920, 1200, color, compression, filter).len() as f64 * 100.0
                        / data.len() as f64
                );
            }
            b.run(&name, data.len(), || {
                png(data, 1920, 1200, color, compression, filter)
            });
        }
    }
    // JPEG stills: libjpeg-turbo in process.
    #[cfg(feature = "jpeg")]
    for (name, format) in [("Mono8", MONO8), ("RGB8", RGB8), ("BayerRG8", 0x0108_0009)] {
        let f = frame(1920, 1200, format);
        b.run(
            &format!("jpeg/turbo/{name}/1920x1200"),
            f.data.len(),
            || jpeg::software(&jpeg::Raster::new(&f).unwrap()).unwrap(),
        );
    }
    let data = vec![0x5a; 8 << 20];
    b.run("sha256/8MiB", data.len(), || Sha256::digest(&data));
    let session = temp("session");
    unsafe { std::env::set_var("CAPTUREFAB_SESSION_DIR", &session) };
    let output = temp("output");
    let mut index = 0;
    for (encoding, format) in [("raw", 0x0108_0009), ("png", 0x0108_0009), ("raw", MONO8)] {
        let f = frame(1920, 1200, format);
        b.run(
            &format!("storage/save/{encoding}/{format:08x}/1920x1200"),
            f.data.len(),
            || {
                index += 1;
                storage::save(
                    &capturefab::testing::frame::encode(&f, encoding).unwrap(),
                    &output.join(format!("{index}.{encoding}")),
                    SyncMode::File,
                    false,
                )
                .unwrap();
            },
        );
    }
    let five = vec![7u8; 5_000_000];
    b.run("storage/save/5MB", five.len(), || {
        index += 1;
        let path = output.join(format!("{index}.raw"));
        storage::save(&five, &path, SyncMode::File, false).unwrap();
    });
    // Session snapshots, as status takes them, with eight idle simulated
    // cameras on real workers and a full session log.
    if b.selected("session/snapshot") {
        unsafe { std::env::set_var("CAPTUREFAB_EXECUTABLE", env!("CARGO_BIN_EXE_capturefab")) };
        let h = SessionHandle::new();
        for camera in 0..8 {
            h.request(SessionCommand::connect(format!("sim:{camera}"), 5000))
                .unwrap();
        }
        for _ in 0..200 {
            let missing = SessionCommand::from_json_slice(
                br#"{"op":"get_values","names":[{"name":"NoSuchFeature"}]}"#,
            )
            .unwrap();
            let _ = h.request_to("sim:0", missing);
        }
        let snapshot = h.snapshot();
        println!(
            "session/snapshot: {} cameras, {} log lines",
            snapshot.cameras.len(),
            snapshot.logs.len()
        );
        b.run("session/snapshot/8-sim-cameras", 0, || h.snapshot());
        h.shutdown();
    }
    for path in [session, output] {
        let _ = std::fs::remove_dir_all(path);
    }
}
