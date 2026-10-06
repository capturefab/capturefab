use capturefab::{
    frame, jpeg,
    shared_memory::SharedRing,
    storage::{self, StoragePolicy},
    types::{Frame, MONO8, RGB8},
};
use sha2::{Digest, Sha256};
use std::{
    hint::black_box,
    path::PathBuf,
    time::{Duration, Instant},
};

const FORMATS: [(&str, u32); 7] = [
    ("Mono8", MONO8),
    ("RGB8", RGB8),
    ("BGR8", 0x0218_0015),
    ("Mono10", 0x0110_0003),
    ("Mono12", 0x0110_0005),
    ("Mono16", 0x0110_0007),
    ("BayerRG8", 0x0108_0009),
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

fn frame(width: u32, height: u32, pixel_format: u32) -> Frame {
    let rgb = scene(width, height);
    let pixels = rgb.as_chunks::<3>().0;
    let luma =
        |p: &[u8; 3]| ((p[0] as u32 * 54 + p[1] as u32 * 183 + p[2] as u32 * 19) >> 8) as u16;
    let data = match pixel_format {
        MONO8 => pixels.iter().map(|p| luma(p) as u8).collect(),
        RGB8 => rgb.clone(),
        0x0218_0015 => pixels.iter().flat_map(|p| [p[2], p[1], p[0]]).collect(),
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
        _ => (0..height as usize)
            .flat_map(|y| {
                let pixels = &pixels;
                (0..width as usize).map(move |x| {
                    pixels[y * width as usize + x][[0, 1, 1, 2][(y & 1) * 2 + (x & 1)]]
                })
            })
            .collect(),
    };
    Frame {
        id: 1,
        width,
        height,
        pixel_format,
        timestamp_ns: 0,
        data,
    }
}

struct Bench {
    filter: Vec<String>,
}
impl Bench {
    fn selected(&self, name: &str) -> bool {
        self.filter.is_empty() || self.filter.iter().any(|f| name.contains(f.as_str()))
    }
    fn run<T>(&self, name: &str, bytes: usize, mut f: impl FnMut() -> T) {
        if !self.selected(name) {
            return;
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
    }
}

fn gui_old(frame: &Frame) -> ([u32; 64], Vec<[u8; 4]>) {
    let rgb = frame::rgb(frame).unwrap();
    let mut histogram = [0u32; 64];
    let stride = (rgb.len() / 3 / 65_536).max(1);
    for p in rgb.as_chunks::<3>().0.iter().step_by(stride) {
        histogram[((p[0] as u32 * 54 + p[1] as u32 * 183 + p[2] as u32 * 19) >> 10) as usize] += 1;
    }
    (
        histogram,
        rgb.as_chunks::<3>()
            .0
            .iter()
            .map(|p| [p[0], p[1], p[2], 255])
            .collect(),
    )
}

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
        for (name, format) in FORMATS {
            let f = frame(width, height, format);
            b.run(
                &format!("rgb/{name}/{width}x{height}"),
                f.data.len(),
                || frame::rgb(&f).unwrap(),
            );
        }
    }
    for (name, format) in [("Mono8", MONO8), ("RGB8", RGB8), ("BayerRG8", 0x0108_0009)] {
        let f = frame(1920, 1200, format);
        b.run(&format!("gui-old/{name}/1920x1200"), f.data.len(), || {
            gui_old(&f)
        });
        b.run(
            &format!("gui-new/main/{name}/1920x1200"),
            f.data.len(),
            || {
                let (_, _, rgba) = frame::preview_rgba(&f, f.width, f.height).unwrap();
                (frame::luminance_histogram(&rgba), rgba)
            },
        );
        b.run(
            &format!("gui-direct/{name}/1920x1200"),
            f.data.len(),
            || frame::convert(&f, |[r, g, b]| [r, g, b, 255]).unwrap(),
        );
        b.run(
            &format!("gui-new/tile/{name}/1920x1200"),
            f.data.len(),
            || frame::preview_rgba(&f, 640, 480).unwrap(),
        );
        for encoding in ["png", "ppm", "raw"] {
            b.run(
                &format!("encode/{encoding}/{name}/1920x1200"),
                f.data.len(),
                || frame::encode(&f, encoding).unwrap(),
            );
        }
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
            ("BayerRG8-rgb", &rgb, png::ColorType::Rgb),
            ("Mono8-gray", &mono.data, png::ColorType::Grayscale),
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
    // JPEG stills: libjpeg-turbo in process, Apple's JPEG engine, and nvJPEG
    // through its helper once warm (set CAPTUREFAB_EXECUTABLE to the
    // capturefab binary).
    for (name, format) in [("Mono8", MONO8), ("RGB8", RGB8), ("BayerRG8", 0x0108_0009)] {
        let f = frame(1920, 1200, format);
        #[cfg(feature = "jpeg")]
        b.run(
            &format!("jpeg/turbo/{name}/1920x1200"),
            f.data.len(),
            || jpeg::software(&jpeg::Raster::new(&f).unwrap()).unwrap(),
        );
        #[cfg(all(feature = "videotoolbox", target_os = "macos"))]
        if format != MONO8 {
            let label = format!("jpeg/videotoolbox/{name}/1920x1200");
            if jpeg::encode_with_backend(&f).unwrap().1 == jpeg::Backend::VideoToolbox {
                b.run(&label, f.data.len(), || {
                    let (bytes, backend) = jpeg::encode_with_backend(&f).unwrap();
                    assert_eq!(backend, jpeg::Backend::VideoToolbox, "fell back to the CPU");
                    bytes
                });
            } else {
                println!("{label:<40} unavailable on this host");
            }
        }
        #[cfg(feature = "nvjpeg")]
        {
            let label = format!("jpeg/nvjpeg/{name}/1920x1200");
            if b.selected(&label) {
                let start = Instant::now();
                while start.elapsed() < Duration::from_secs(30)
                    && jpeg::encode_with_backend(&f).unwrap().1 != jpeg::Backend::NvJpeg
                {
                    std::thread::sleep(Duration::from_millis(50));
                }
                if jpeg::encode_with_backend(&f).unwrap().1 == jpeg::Backend::NvJpeg {
                    b.run(&label, f.data.len(), || {
                        let (bytes, backend) = jpeg::encode_with_backend(&f).unwrap();
                        assert_eq!(backend, jpeg::Backend::NvJpeg, "fell back to the CPU");
                        bytes
                    });
                } else {
                    println!("{label:<40} unavailable on this host");
                }
            }
        }
    }
    let data = vec![0x5a; 8 << 20];
    b.run("sha256/8MiB", data.len(), || Sha256::digest(&data));
    let dir = temp("ring");
    for (name, format) in [("RGB8", RGB8), ("BayerRG8", 0x0108_0009)] {
        let f = frame(1920, 1200, format);
        let path = dir.join(format!("{name}.shm"));
        let mut writer = SharedRing::create(&path, 16 << 20).unwrap();
        let mut reader = SharedRing::open(&path).unwrap();
        b.run(
            &format!("ring/write+take/{name}/1920x1200"),
            f.data.len(),
            || {
                writer.write(&f).unwrap();
                reader.take_latest().unwrap().unwrap()
            },
        );
    }
    // The GUI's per-frame display pipeline, end to end on the parent side:
    // old = fresh ring copy, cached-frame clone, RGB decode, RGBA expansion;
    // new = ring copy into the reused cache, one direct RGBA decode.
    for (name, format) in [("Mono8", MONO8), ("RGB8", RGB8), ("BayerRG8", 0x0108_0009)] {
        let f = frame(1920, 1200, format);
        let path = dir.join(format!("display-{name}.shm"));
        let mut writer = SharedRing::create(&path, 16 << 20).unwrap();
        let mut reader = SharedRing::open(&path).unwrap();
        b.run(
            &format!("display-old/{name}/1920x1200"),
            f.data.len(),
            || {
                writer.write(&f).unwrap();
                let cached = reader.take_latest().unwrap().unwrap();
                let rgb = frame::rgb(&cached.clone()).unwrap();
                rgb.as_chunks::<3>()
                    .0
                    .iter()
                    .map(|p| [p[0], p[1], p[2], 255])
                    .collect::<Vec<_>>()
            },
        );
        let mut cached = Frame::default();
        b.run(
            &format!("display-new/{name}/1920x1200"),
            f.data.len(),
            || {
                writer.write(&f).unwrap();
                reader.take_latest_into(&mut cached).unwrap();
                frame::convert(&cached, |[r, g, b]| [r, g, b, 255]).unwrap()
            },
        );
    }
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
                    &f,
                    &output.join(format!("{index}.{encoding}")),
                    encoding,
                    &StoragePolicy::default(),
                )
                .unwrap()
            },
        );
    }
    let tiny = Frame {
        id: 1,
        width: 1,
        height: 1,
        pixel_format: MONO8,
        timestamp_ns: 0,
        data: vec![0],
    };
    for entries in [100, 1000] {
        let name = format!("storage/save/raw/1x1/ledger{entries}");
        let mut save = || {
            index += 1;
            storage::save(
                &tiny,
                &output.join(format!("{index}.raw")),
                "raw",
                &StoragePolicy::default(),
            )
            .unwrap();
            index
        };
        if b.selected(&name) {
            while save() < entries {}
        }
        b.run(&name, 1, save);
    }
    for path in [dir, session, output] {
        let _ = std::fs::remove_dir_all(path);
    }
}
