use super::*;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/autocapture/fixtures")
        .join(name)
}

/// Must match `pixels` in scripts/dinov2-fixtures.py.
fn pixels(height: usize, width: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(3 * height * width);
    for c in 0..3 {
        for y in 0..height {
            for x in 0..width {
                let value = (0.37 * x as f64 + 0.11 * y as f64 * (c + 1) as f64).sin() * 1.5
                    + 0.1 * c as f64;
                out.push(value as f32);
            }
        }
    }
    out
}

fn expected(path: &Path) -> (usize, usize, Vec<f32>) {
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let cls = value["cls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap() as f32)
        .collect();
    (
        value["height"].as_u64().unwrap() as usize,
        value["width"].as_u64().unwrap() as usize,
        cls,
    )
}

fn worst(actual: &[f32], expected: &[f32]) -> f32 {
    assert_eq!(actual.len(), expected.len());
    actual
        .iter()
        .zip(expected)
        .map(|(a, e)| (a - e).abs())
        .fold(0.0, f32::max)
}

#[test]
fn forward_matches_transformers_reference() {
    for name in ["dinov2-tiny", "dinov2-native"] {
        let path = fixture(name);
        let model = Model::load(&path).unwrap();
        let (height, width, cls) = expected(&path.join("expected.json"));
        let actual = model
            .forward(&pixels(height, width), height, width)
            .unwrap();
        let difference = worst(&actual, &cls);
        assert!(difference < 2e-5, "{name}: {difference}");
        assert_eq!(model.fingerprint().len(), 64);
    }
}

/// The GPU kernels compute the same network; skipped without a GPU adapter.
#[test]
fn gpu_matches_cpu_when_an_adapter_exists() {
    for name in ["dinov2-tiny", "dinov2-native"] {
        let path = fixture(name);
        let model = Model::load(&path).unwrap();
        let (height, width, cls) = expected(&path.join("expected.json"));
        let mut gpu = match gpu::Gpu::new(&model, height, width) {
            Ok(gpu) => gpu,
            Err(error) => {
                eprintln!("skipping GPU check: {error:#}");
                return;
            }
        };
        let columns = model
            .columns(&pixels(height, width), height, width)
            .unwrap();
        for _ in 0..2 {
            let actual = gpu.embed_columns(&columns).unwrap();
            let difference = worst(&actual, &cls);
            assert!(
                difference < 1e-4,
                "{name} on {}: {difference}",
                gpu.adapter()
            );
        }
    }
}

/// The bundled checkpoint against transformers (CAPTUREFAB_DINOV2_REFERENCE is
/// `scripts/dinov2-fixtures.py --model DIR` output), plus 30 fps throughput.
#[test]
#[ignore]
fn bundled_model_matches_reference_and_runs_at_30_fps_on_the_gpu() {
    let model = Model::from_safetensors(bundled_weights().expect(MISSING), None).unwrap();
    let reference = PathBuf::from(std::env::var_os("CAPTUREFAB_DINOV2_REFERENCE").unwrap());
    let (height, width, cls) = expected(&reference);
    let started = std::time::Instant::now();
    let cpu = model
        .forward(&pixels(height, width), height, width)
        .unwrap();
    eprintln!(
        "{} CPU forward pass: {:?}",
        model.label(),
        started.elapsed()
    );
    assert!(worst(&cpu, &cls) < 1e-4, "CPU {}", worst(&cpu, &cls));
    let mut gpu = gpu::Gpu::new(&model, height, width).unwrap();
    let columns = model
        .columns(&pixels(height, width), height, width)
        .unwrap();
    let actual = gpu.embed_columns(&columns).unwrap();
    let frames = 120;
    let started = std::time::Instant::now();
    for _ in 0..frames {
        gpu.embed_columns(&columns).unwrap();
    }
    let rate = frames as f64 / started.elapsed().as_secs_f64();
    eprintln!(
        "{} on {}: {rate:.0} embeddings/s",
        model.label(),
        gpu.adapter()
    );
    assert!(worst(&actual, &cls) < 1e-4, "GPU {}", worst(&actual, &cls));
    assert!(rate >= 30.0, "{rate:.1} embeddings/s");
}

#[test]
fn bicubic_matches_identity_and_preserves_constants() {
    for (index, weights) in bicubic(10, 10) {
        assert_eq!(weights, [0.0, 1.0, 0.0, 0.0]);
        let _ = index;
    }
    for (input, output) in [(37, 16), (10, 6), (10, 8), (4, 9)] {
        for (index, weights) in bicubic(input, output) {
            assert!(index.iter().all(|i| *i < input));
            let total: f32 = weights.iter().sum();
            assert!((total - 1.0).abs() < 1e-5, "{input}->{output}: {total}");
        }
    }
}

#[test]
fn prepares_camera_frames_and_embeds_unit_vectors() {
    let model = Model::load(&fixture("dinov2-native")).unwrap();
    for (width, height, pixel_format, bytes) in [
        (640, 480, 0x0218_0014, 3),  // RGB8
        (37, 21, 0x0108_0001, 1),    // Mono8, smaller than the input
        (1280, 720, 0x0108_0009, 1), // BayerRG8
    ] {
        let data = (0..width * height * bytes)
            .map(|i| (i * 7 % 251) as u8)
            .collect();
        let frame = Frame {
            id: 1,
            width: width as u32,
            height: height as u32,
            pixel_format,
            timestamp_ns: 0,
            data,
        };
        let rgb = prepare(&frame, model.input()).unwrap();
        assert_eq!(rgb.len(), model.input() * model.input() * 3);
        let embedding = model.embed_rgb(&rgb).unwrap();
        let length: f32 = embedding.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((length - 1.0).abs() < 1e-5, "{length}");
        assert_eq!(embedding, model.embed_rgb(&rgb).unwrap(), "deterministic");
    }
}

#[test]
fn rejects_incomplete_or_mismatched_weights() {
    let source = fixture("dinov2-tiny");
    let config = std::fs::read_to_string(source.join("config.json")).unwrap();
    let weights = std::fs::read(source.join("model.safetensors")).unwrap();
    let error = format!(
        "{:#}",
        Model::from_safetensors(b"\x08\0\0\0\0\0\0\0{}      ", Some(&config))
            .err()
            .unwrap()
    );
    assert!(error.contains("missing tensor"), "{error}");
    let error = format!(
        "{:#}",
        Model::from_safetensors(&weights, Some(r#"{"model_type":"dinov3_vit"}"#))
            .err()
            .unwrap()
    );
    assert!(error.contains("unsupported DINOv2 model type"), "{error}");
    let mut changed: serde_json::Value = serde_json::from_str(&config).unwrap();
    changed["hidden_size"] = 64.into();
    let error = format!(
        "{:#}",
        Model::from_safetensors(&weights, Some(&changed.to_string()))
            .err()
            .unwrap()
    );
    assert!(error.contains("has shape"), "{error}");
    // A different configuration is a different model, even for equal weights.
    let mut eps: serde_json::Value = serde_json::from_str(&config).unwrap();
    eps["layer_norm_eps"] = 1e-5.into();
    let a = Model::from_safetensors(&weights, Some(&config)).unwrap();
    let b = Model::from_safetensors(&weights, Some(&eps.to_string())).unwrap();
    assert_ne!(a.fingerprint(), b.fingerprint());
}

#[test]
fn resize_filter_weights_are_normalized() {
    for (input, output) in [(896, 224), (224, 224), (37, 224), (1, 224), (225, 224)] {
        let taps = filter(input, output);
        assert_eq!(taps.len(), output);
        for (first, weights) in taps {
            assert!(first + weights.len() <= input);
            let total: f32 = weights.iter().sum();
            assert!((total - 1.0).abs() < 1e-5, "{input}->{output}: {total}");
        }
    }
}
