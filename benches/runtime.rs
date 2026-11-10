#![deny(clippy::doc_markdown)]
#![deny(clippy::missing_const_for_fn)]
#[cfg(feature = "jpeg")]
use capturefab::testing::jpeg;
use capturefab::testing::{
    auto,
    camera_profile::CameraKind,
    frame,
    session::{SessionCommand, SessionHandle},
    storage::{self, SyncMode},
    types::{Frame, MONO8, RGB8},
};
use sha2::{Digest as _, Sha256};
use std::{
    hint::black_box,
    path::PathBuf,
    time::{Duration, Instant},
};

const FORMATS: [(&str, u32); 21] = [
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
    ("yuv420p", 0x820c_0002),
];

struct Rng(u64);
impl Rng {
    const fn next(&mut self) -> u64 {
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
                #[expect(
                    clippy::cast_sign_loss,
                    clippy::cast_possible_truncation,
                    reason = "the clamped benchmark scene retains saturating float-to-byte conversion"
                )]
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
                output.extend_from_slice(&[
                    u8::try_from(pair[0] >> shift).unwrap(),
                    u8::try_from(pair[0] & mask).unwrap(),
                ]);
                if let Some(&second) = pair.get(1) {
                    *output.last_mut().unwrap() |= u8::try_from((second & mask) << 4).unwrap();
                    output.push(u8::try_from(second >> shift).unwrap());
                }
            }
        } else {
            let (mut word, mut present) = (0u32, 0);
            for &sample in row {
                word |= u32::from(sample) << present;
                present += bits;
                while present >= 8 {
                    output.push(word.to_le_bytes()[0]);
                    word >>= 8;
                    present -= 8;
                }
            }
            if present > 0 {
                output.push(word.to_le_bytes()[0]);
            }
        }
    }
    output
}

fn frame(width: u32, height: u32, pixel_format: u32) -> Frame {
    let rgb = scene(width, height);
    let pixels = rgb.as_chunks::<3>().0;
    let luma = |p: &[u8; 3]| {
        u16::try_from((u32::from(p[0]) * 54 + u32::from(p[1]) * 183 + u32::from(p[2]) * 19) >> 8)
            .unwrap()
    };
    let data = match pixel_format {
        MONO8 => pixels
            .iter()
            .map(|p| u8::try_from(luma(p)).unwrap())
            .collect(),
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
            .flat_map(|(i, &v)| {
                ((u16::from(v) << 4) | (u16::try_from(i & 15).unwrap())).to_le_bytes()
            })
            .collect(),
        0x0230_0024 => (0..3)
            .flat_map(|c| {
                pixels.iter().enumerate().flat_map(move |(i, p)| {
                    ((u16::from(p[c]) << 8) | (u16::try_from(i & 255).unwrap())).to_le_bytes()
                })
            })
            .collect(),
        0x0224_005d => {
            let samples: Vec<u16> = rgb
                .iter()
                .enumerate()
                .map(|(i, &v)| (u16::from(v) << 4) | (u16::try_from(i & 15).unwrap()))
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
                    (value << (bits - 8))
                        | (u16::try_from(i & ((1usize << (bits - 8)) - 1)).unwrap())
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
            #[expect(
                clippy::cast_sign_loss,
                clippy::cast_possible_truncation,
                reason = "the reference YCbCr matrix retains rounded, clamped float-to-byte conversion"
            )]
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
                        let mean = |c: usize| {
                            u8::try_from((u16::from(a[c]) + u16::from(b[c])).div_ceil(2)).unwrap()
                        };
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
                    ((luma(p) << shift)
                        | (u16::try_from(rng.next() & ((1u64 << shift) - 1)).unwrap()))
                    .to_le_bytes()
                })
                .collect()
        }
        // Limited-range BT.601, as FFmpeg decodes most video: 4:2:0 planes
        // (yuv420p), 4:2:0 with interleaved chroma (NV12), and 4:2:2 as
        // Cb Y Cr Y (UYVY). Chroma is the mean of the pixels it covers.
        0x820c_0002 | 0x820c_0011 | 0x0210_0044 => {
            #[expect(
                clippy::cast_sign_loss,
                clippy::cast_possible_truncation,
                reason = "the reference YCbCr matrix retains rounded, clamped float-to-byte conversion"
            )]
            let ycc = |p: &[u8; 3]| {
                let [r, g, b] = p.map(f64::from);
                [
                    16.0 + 0.25679 * r + 0.50413 * g + 0.09791 * b,
                    128.0 - 0.14822 * r - 0.29099 * g + 0.43922 * b,
                    128.0 + 0.43922 * r - 0.36779 * g - 0.07143 * b,
                ]
                .map(|v| v.round().clamp(0.0, 255.0) as u8)
            };
            let (w, h) = (width as usize, height as usize);
            let (sx, sy) = if pixel_format == 0x0210_0044 {
                (2, 1)
            } else {
                (2, 2)
            };
            let chroma = |c: usize, cx: usize, cy: usize| {
                let (sum, count) = (cy * sy..(cy * sy + sy).min(h))
                    .flat_map(|y| (cx * sx..(cx * sx + sx).min(w)).map(move |x| (x, y)))
                    .fold((0u32, 0u32), |(sum, count), (x, y)| {
                        (sum + u32::from(ycc(&pixels[y * w + x])[c]), count + 1)
                    });
                u8::try_from((sum + count / 2) / count).expect("rounded chroma mean fits u8")
            };
            let (cw, ch) = (w.div_ceil(sx), h.div_ceil(sy));
            let luma = |x: usize, y: usize| ycc(&pixels[y * w + x])[0];
            if pixel_format == 0x0210_0044 {
                (0..h)
                    .flat_map(|y| {
                        (0..cw).flat_map(move |cx| {
                            [
                                chroma(1, cx, y),
                                luma(2 * cx, y),
                                chroma(2, cx, y),
                                luma(2 * cx + 1, y),
                            ]
                        })
                    })
                    .collect()
            } else {
                let mut data: Vec<u8> = pixels.iter().map(|p| ycc(p)[0]).collect();
                let blocks = (0..ch).flat_map(|cy| (0..cw).map(move |cx| (cx, cy)));
                if pixel_format == 0x820c_0011 {
                    data.extend(blocks.flat_map(|(x, y)| [chroma(1, x, y), chroma(2, x, y)]));
                } else {
                    for c in [1, 2] {
                        data.extend(blocks.clone().map(|(x, y)| chroma(c, x, y)));
                    }
                }
                data
            }
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
        jpeg: None,
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
/// How many times as long as RGB8 measuring `Y'CbCr` of the same size may
/// take, when both are benchmarked. With its layout looked up for every
/// pixel read, decoded video took four to five times as long.
const YCBCR_MEASURE_LIMIT: f64 = 3.0;

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
        use std::os::unix::fs::PermissionsExt as _;
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
    // JPEG rows measure libjpeg-turbo unless they name an engine.
    let session = temp("session");
    for (key, value) in [
        ("CAPTUREFAB_JPEG", std::ffi::OsStr::new("cpu")),
        ("CAPTUREFAB_SESSION_DIR", session.as_os_str()),
        (
            "CAPTUREFAB_EXECUTABLE",
            std::ffi::OsStr::new(env!("CARGO_BIN_EXE_capturefab")),
        ),
    ] {
        // SAFETY: the benchmark is single-threaded here.
        unsafe { std::env::set_var(key, value) };
    }
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
        for encoding in ["png", "raw"] {
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
                frame::encode_parts(&f, "raw", None, Some(&meta)).unwrap()
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
                || frame::encode_parts(&f, "jpeg", quality, Some(&meta)).unwrap(),
            );
        }
    }
    // Converted 12 MP stills, inspections and metering: packed Bayer read in
    // place, unpacked and demosaicked once into what each consumer reads.
    for (name, format, width, height) in [
        ("BayerRG12p", 0x010c_0059, 4096, 3000),
        ("BayerRG8", 0x0108_0009, 4096, 3000),
        ("BayerRG8", 0x0108_0009, 1920, 1080),
        ("YCbCr709_422_8", 0x0210_0041, 1920, 1080),
        ("RGB8", RGB8, 1920, 1080),
        ("RGB8", RGB8, 4096, 3000),
    ] {
        let suffix = format!("{name}/{width}x{height}");
        if ![
            "encode/jpeg/",
            "encode/jpeg-rgb/",
            "encode/jpeg-videotoolbox/",
            "encode/png/",
            "inspect/",
            "auto/meter/",
        ]
        .iter()
        .any(|prefix| b.selected(&format!("{prefix}{suffix}")))
        {
            continue;
        }
        let f = frame(width, height, format);
        #[cfg(feature = "jpeg")]
        b.run(&format!("encode/jpeg/{suffix}"), f.data.len(), || {
            frame::encode_parts(&f, "jpeg", None, Some(&meta)).unwrap()
        });
        #[cfg(feature = "jpeg")]
        // Through an RGB image, as converted color stills were compressed
        // before they went to libjpeg-turbo as Y'CbCr planes.
        b.run(&format!("encode/jpeg-rgb/{suffix}"), f.data.len(), || {
            jpeg::software(&jpeg::Raster::new(&f).unwrap()).unwrap()
        });
        // Apple's JPEG engine, where this Mac has one.
        #[cfg(all(feature = "videotoolbox", target_os = "macos"))]
        {
            let label = format!("encode/jpeg-videotoolbox/{suffix}");
            if b.selected(&label) {
                // SAFETY: as above.
                unsafe { std::env::set_var("CAPTUREFAB_JPEG", "videotoolbox") };
                match frame::encode_parts(&f, "jpeg", None, None) {
                    Ok(_) => {
                        b.run(&label, f.data.len(), || {
                            frame::encode_parts(&f, "jpeg", None, Some(&meta)).unwrap()
                        });
                    }
                    Err(error) => eprintln!("{label}: skipped: {error:#}"),
                }
                // SAFETY: as above.
                unsafe { std::env::set_var("CAPTUREFAB_JPEG", "cpu") };
            }
        }
        b.run(&format!("encode/png/{suffix}"), f.data.len(), || {
            frame::encode_parts(&f, "png", None, Some(&meta)).unwrap()
        });
        for (label, region) in [
            ("inspect", None),
            ("inspect/region", Some([0.25, 0.25, 0.5, 0.5])),
        ] {
            b.run(&format!("{label}/{suffix}"), f.data.len(), || {
                capturefab::testing::inspect::inspect(
                    &f,
                    CameraKind::AreaScan,
                    region,
                    1024,
                    capturefab::testing::inspect::PreviewFormat::Auto,
                )
                .unwrap()
            });
        }
        b.run(&format!("auto/meter/{suffix}"), f.data.len(), || {
            auto::meter(&f).unwrap()
        });
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
    // What `inspect` measures of a whole frame (exposure, clipping, the
    // auto meter and focus) in camera layouts and the Y'CbCr that FFmpeg
    // sources deliver, each read where it lies.
    for (width, height) in [(640, 480), (1920, 1080)] {
        let mut rgb8 = None;
        for (name, format) in [
            ("Mono8", MONO8),
            ("RGB8", RGB8),
            ("yuv420p", 0x820c_0002),
            ("NV12", 0x820c_0011),
            ("UYVY", 0x0210_0044),
        ] {
            let label = format!("measure/{name}/{width}x{height}");
            if !b.selected(&label) {
                continue;
            }
            let f = frame(width, height, format);
            let median = b.run(&label, f.data.len(), || {
                capturefab::testing::inspect::measure(
                    &f,
                    CameraKind::AreaScan,
                    [0, 0, width, height],
                )
            });
            match (name, median, rgb8) {
                ("RGB8", _, _) => rgb8 = median,
                ("Mono8", ..) => {}
                (_, Some(time), Some(baseline)) => {
                    let ratio = time / baseline;
                    assert!(
                        ratio <= YCBCR_MEASURE_LIMIT,
                        "{label} takes {ratio:.1} times measure/RGB8/{width}x{height}"
                    );
                }
                _ => {}
            }
        }
    }
    let mono = frame(1920, 1200, MONO8);
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
            ("BayerRG8-rgb", (&*rgb), png::ColorType::Rgb),
            ("Mono8-gray", (&*mono.data), png::ColorType::Grayscale),
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
                    &frame::encode(&f, encoding).unwrap(),
                    &output.join(format!("{index}.{encoding}")),
                    SyncMode::File,
                    None,
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
        storage::save(&five, &path, SyncMode::File, None, false).unwrap();
    });
    // Session snapshots, as status takes them, with eight idle simulated
    // cameras on real workers and a full session log.
    if b.selected("session/snapshot") {
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
