//! DINOv2 ViT-S/14 image embeddings for auto capture. Meta's weights
//! (Apache-2.0, `facebook/dinov2-small`) are bundled: release builds embed the
//! float16 file `scripts/fetch-dinov2.py` produces, so nothing is downloaded at
//! run time. Frames are embedded on the GPU in an isolated helper process when
//! one is usable (`gpu.rs`, `helper.rs`) and on the CPU here otherwise; both
//! compute the same network in float32.
use crate::{frame, types::Frame};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, OnceLock},
};

pub mod gpu;
pub mod helper;

/// Frames are squashed to this many pixels square: 16 x 16 patches of 14.
pub const INPUT: usize = 224;
const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const STD: [f32; 3] = [0.229, 0.224, 0.225];
const MAX_WEIGHT_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_HEADER_BYTES: u64 = 16 * 1024 * 1024;

/// The fields of a Hugging Face `Dinov2Config` that change the forward pass.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct Config {
    pub model_type: String,
    pub hidden_size: usize,
    pub num_hidden_layers: usize,
    pub num_attention_heads: usize,
    pub mlp_ratio: usize,
    pub patch_size: usize,
    pub image_size: usize,
    pub layer_norm_eps: f64,
    pub hidden_act: String,
    pub qkv_bias: bool,
    pub use_swiglu_ffn: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model_type: String::new(),
            hidden_size: 768,
            num_hidden_layers: 12,
            num_attention_heads: 12,
            mlp_ratio: 4,
            patch_size: 14,
            image_size: 224,
            layer_norm_eps: 1e-6,
            hidden_act: "gelu".into(),
            qkv_bias: true,
            use_swiglu_ffn: false,
        }
    }
}

pub(crate) struct Linear {
    /// Row-major `[out, in]`, exactly as PyTorch stores `nn.Linear.weight`.
    pub weight: Vec<f32>,
    pub bias: Vec<f32>,
    pub inputs: usize,
    pub outputs: usize,
}

pub(crate) struct Norm {
    pub weight: Vec<f32>,
    pub bias: Vec<f32>,
}

pub(crate) struct Layer {
    pub norm1: Norm,
    /// Query, key and value stacked into one `[3 * hidden, hidden]` product.
    pub qkv: Linear,
    pub output: Linear,
    pub scale1: Vec<f32>,
    pub norm2: Norm,
    pub fc1: Linear,
    pub fc2: Linear,
    pub scale2: Vec<f32>,
}

pub struct Model {
    pub(crate) config: Config,
    pub(crate) patch: Linear,
    pub(crate) cls: Vec<f32>,
    /// The pretrained `[1 + grid * grid, hidden]` table, CLS position first.
    positions: Vec<f32>,
    pub(crate) layers: Vec<Layer>,
    pub(crate) norm: Norm,
    fingerprint: String,
}

/// The bundled weights: embedded in release builds, read from
/// `target/models/dinov2-small` in place by dev and test builds.
pub fn bundled_weights() -> Option<&'static [u8]> {
    #[cfg(dinov2_embedded)]
    {
        Some(include_bytes!(env!("CAPTUREFAB_DINOV2_EMBED")))
    }
    #[cfg(not(dinov2_embedded))]
    {
        static WEIGHTS: OnceLock<Option<Vec<u8>>> = OnceLock::new();
        WEIGHTS
            .get_or_init(|| {
                let path = option_env!("CAPTUREFAB_DINOV2_PATH")?;
                std::fs::read(path).ok()
            })
            .as_deref()
    }
}

/// The bundled model, decoded once per process. Unit tests get the tiny
/// committed fixture instead, so they neither need nor depend on the release
/// weights.
pub fn bundled() -> Result<Option<Arc<Model>>> {
    static MODEL: OnceLock<Result<Option<Arc<Model>>, String>> = OnceLock::new();
    MODEL
        .get_or_init(|| {
            #[cfg(test)]
            return Model::load(
                &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/autocapture/fixtures/dinov2-tiny"),
            )
            .map(|model| Some(Arc::new(model)))
            .map_err(|error| format!("{error:#}"));
            #[cfg(not(test))]
            bundled_weights()
                .map(|bytes| Model::from_safetensors(bytes, None).map(Arc::new))
                .transpose()
                .map_err(|error| format!("{error:#}"))
        })
        .clone()
        .map_err(anyhow::Error::msg)
}

/// Why `bundled()` is `None`, for diagnostics.
pub const MISSING: &str = "DINOv2 auto capture is not supported by this build: it does not bundle the model weights (run scripts/fetch-dinov2.py and rebuild)";

impl Model {
    /// A model directory as transformers writes it: config.json and
    /// model.safetensors (the test fixtures).
    pub fn load(directory: &Path) -> Result<Self> {
        let config = std::fs::read_to_string(directory.join("config.json"))
            .with_context(|| format!("read {}", directory.join("config.json").display()))?;
        let weights = std::fs::read(directory.join("model.safetensors"))
            .with_context(|| format!("read {}", directory.join("model.safetensors").display()))?;
        Self::from_safetensors(&weights, Some(&config))
    }

    /// Decode safetensors weights. The bundled file carries its configuration
    /// in the header metadata; otherwise `config` is a config.json.
    pub fn from_safetensors(bytes: &[u8], config: Option<&str>) -> Result<Self> {
        ensure!(
            bytes.len() as u64 <= MAX_WEIGHT_BYTES,
            "DINOv2 weights are too large"
        );
        let tensors = Tensors::parse(bytes)?;
        let config: Config = serde_json::from_str(
            config
                .or(tensors.metadata.get("config").map(String::as_str))
                .context("DINOv2 weights carry no configuration")?,
        )
        .context("parse DINOv2 configuration")?;
        validate(&config)?;
        // Embeddings depend on the weights and on every configuration value
        // the forward pass reads, so both identify the model a session used.
        let mut fingerprint = Sha256::new();
        fingerprint.update(b"capturefab-dinov2-v1\n");
        fingerprint.update(Sha256::digest(bytes));
        fingerprint.update(serde_json::to_vec(&config)?);
        let fingerprint = format!("{:x}", fingerprint.finalize());
        let (d, c, p) = (config.hidden_size, config.num_channels(), config.patch_size);
        let hidden = d * config.mlp_ratio;
        let linear = |name: &str, inputs: usize, outputs: usize| -> Result<Linear> {
            Ok(Linear {
                weight: tensors.get(&format!("{name}.weight"), &[outputs, inputs])?,
                bias: tensors.get(&format!("{name}.bias"), &[outputs])?,
                inputs,
                outputs,
            })
        };
        let norm = |name: &str| -> Result<Norm> {
            Ok(Norm {
                weight: tensors.get(&format!("{name}.weight"), &[d])?,
                bias: tensors.get(&format!("{name}.bias"), &[d])?,
            })
        };
        let layers = (0..config.num_hidden_layers)
            .map(|n| -> Result<Layer> {
                let at = |part: &str| format!("encoder.layer.{n}.{part}");
                let [q, k, v] = ["query", "key", "value"]
                    .map(|part| linear(&at(&format!("attention.attention.{part}")), d, d));
                let (q, k, v) = (q?, k?, v?);
                Ok(Layer {
                    norm1: norm(&at("norm1"))?,
                    qkv: Linear {
                        weight: [q.weight, k.weight, v.weight].concat(),
                        bias: [q.bias, k.bias, v.bias].concat(),
                        inputs: d,
                        outputs: 3 * d,
                    },
                    output: linear(&at("attention.output.dense"), d, d)?,
                    scale1: tensors.get(&at("layer_scale1.lambda1"), &[d])?,
                    norm2: norm(&at("norm2"))?,
                    fc1: linear(&at("mlp.fc1"), d, hidden)?,
                    fc2: linear(&at("mlp.fc2"), hidden, d)?,
                    scale2: tensors.get(&at("layer_scale2.lambda1"), &[d])?,
                })
            })
            .collect::<Result<_>>()?;
        let grid = config.image_size / p;
        Ok(Self {
            patch: Linear {
                weight: tensors.get(
                    "embeddings.patch_embeddings.projection.weight",
                    &[d, c, p, p],
                )?,
                bias: tensors.get("embeddings.patch_embeddings.projection.bias", &[d])?,
                inputs: c * p * p,
                outputs: d,
            },
            cls: tensors.get("embeddings.cls_token", &[1, 1, d])?,
            positions: tensors.get("embeddings.position_embeddings", &[1, 1 + grid * grid, d])?,
            layers,
            norm: norm("layernorm")?,
            config,
            fingerprint,
        })
    }

    /// SHA-256 over the weights and the effective configuration.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    pub fn dims(&self) -> usize {
        self.config.hidden_size
    }
    pub fn label(&self) -> String {
        let size = match self.config.hidden_size {
            384 => "S",
            768 => "B",
            1024 => "L",
            1536 => "g",
            _ => "custom",
        };
        format!(
            "DINOv2 ViT-{size}/{} embedding, model {}",
            self.config.patch_size,
            &self.fingerprint[..12]
        )
    }
    /// The square input side frames are squashed to: 224 pixels, or the
    /// pretraining size for smaller models, in whole patches.
    pub fn input(&self) -> usize {
        INPUT.min(self.config.image_size) / self.config.patch_size * self.config.patch_size
    }

    /// Position embeddings for a `rows x columns` patch grid: the pretrained
    /// table resampled as transformers does (bicubic, align_corners=False).
    pub(crate) fn positions(&self, rows: usize, columns: usize) -> Vec<f32> {
        let d = self.config.hidden_size;
        let grid = self.config.image_size / self.config.patch_size;
        let mut out = Vec::with_capacity((1 + rows * columns) * d);
        out.extend_from_slice(&self.positions[..d]);
        if rows == grid && columns == grid {
            out.extend_from_slice(&self.positions[d..]);
            return out;
        }
        let table = &self.positions[d..];
        let (ys, xs) = (bicubic(grid, rows), bicubic(grid, columns));
        for (y_index, y_weights) in &ys {
            for (x_index, x_weights) in &xs {
                let start = out.len();
                out.resize(start + d, 0.0);
                for (i, wy) in y_index.iter().zip(y_weights) {
                    for (j, wx) in x_index.iter().zip(x_weights) {
                        let weight = wy * wx;
                        let source = &table[(i * grid + j) * d..][..d];
                        for (value, s) in out[start..].iter_mut().zip(source) {
                            *value += weight * s;
                        }
                    }
                }
            }
        }
        out
    }

    /// Patch columns (im2col) for a normalized channel-major image, so the
    /// patch embedding is one matrix product on either backend.
    pub(crate) fn columns(&self, pixels: &[f32], height: usize, width: usize) -> Result<Vec<f32>> {
        let c = self.config.num_channels();
        let p = self.config.patch_size;
        ensure!(
            height.is_multiple_of(p) && width.is_multiple_of(p) && height > 0 && width > 0,
            "DINOv2 input must be a whole number of patches"
        );
        ensure!(
            pixels.len() == c * height * width,
            "DINOv2 input has the wrong number of values"
        );
        let columns = width / p;
        let k = self.patch.inputs;
        let mut out = vec![0.0; (height / p) * columns * k];
        for (index, patch) in out.chunks_exact_mut(k).enumerate() {
            let (py, px) = (index / columns, index % columns);
            for channel in 0..c {
                for ky in 0..p {
                    let source = channel * height * width + (py * p + ky) * width + px * p;
                    let target = channel * p * p + ky * p;
                    patch[target..target + p].copy_from_slice(&pixels[source..source + p]);
                }
            }
        }
        Ok(out)
    }

    /// Embed an `input() x input()` RGB image on the CPU.
    pub fn embed_rgb(&self, rgb: &[u8]) -> Result<Vec<f32>> {
        let size = self.input();
        self.forward(&normalize(rgb, size)?, size, size)
    }

    /// The unit-length CLS token for a normalized `[3, height, width]` image.
    pub fn forward(&self, pixels: &[f32], height: usize, width: usize) -> Result<Vec<f32>> {
        let c = &self.config;
        let (d, heads) = (c.hidden_size, c.num_attention_heads);
        let (rows, columns) = (height / c.patch_size, width / c.patch_size);
        let patches = rows * columns;
        let tokens = 1 + patches;
        let head = d / heads;
        let cols = self.columns(pixels, height, width)?;
        let positions = self.positions(rows, columns);
        let mut x = Vec::with_capacity(tokens * d);
        x.extend_from_slice(&self.cls);
        x.extend(linear(&cols, patches, &self.patch));
        x.iter_mut().zip(&positions).for_each(|(v, p)| *v += p);

        let scale = (head as f32).powf(-0.5);
        let mut scores = vec![0.0; tokens * tokens];
        let mut attended = vec![0.0; tokens * d];
        for layer in &self.layers {
            let h = layer_norm(&x, d, &layer.norm1, c.layer_norm_eps);
            let qkv = linear(&h, tokens, &layer.qkv);
            for h in 0..heads {
                let offset = h * head;
                // scores = scale * Q_h K_h^T, each head a strided view of [T, 3D].
                gemm(
                    (tokens, head, tokens),
                    (&qkv[offset..], 3 * d, 1),
                    (&qkv[d + offset..], 1, 3 * d),
                    (&mut scores, tokens, 1),
                    scale,
                );
                for row in scores.chunks_exact_mut(tokens) {
                    softmax(row);
                }
                gemm(
                    (tokens, tokens, head),
                    (&scores, tokens, 1),
                    (&qkv[2 * d + offset..], 3 * d, 1),
                    (&mut attended[offset..], d, 1),
                    1.0,
                );
            }
            let o = linear(&attended, tokens, &layer.output);
            residual(&mut x, &o, &layer.scale1);
            let h = layer_norm(&x, d, &layer.norm2, c.layer_norm_eps);
            let mut hidden = linear(&h, tokens, &layer.fc1);
            hidden.iter_mut().for_each(|v| *v = gelu(*v));
            let down = linear(&hidden, tokens, &layer.fc2);
            residual(&mut x, &down, &layer.scale2);
        }
        unit(layer_norm(&x[..d], d, &self.norm, c.layer_norm_eps))
    }
}

impl Config {
    fn num_channels(&self) -> usize {
        3
    }
}

fn validate(c: &Config) -> Result<()> {
    ensure!(
        c.model_type == "dinov2",
        "unsupported DINOv2 model type {:?}",
        c.model_type
    );
    ensure!(
        (1..=2048).contains(&c.hidden_size)
            && (1..=48).contains(&c.num_hidden_layers)
            && (1..=32).contains(&c.num_attention_heads)
            && (1..=8).contains(&c.mlp_ratio)
            && (1..=32).contains(&c.patch_size)
            && (c.patch_size..=1024).contains(&c.image_size)
            && c.hidden_size.is_multiple_of(c.num_attention_heads)
            && c.hidden_size / c.num_attention_heads <= gpu::MAX_HEAD,
        "unsupported DINOv2 model dimensions"
    );
    ensure!(
        c.hidden_act == "gelu" && c.qkv_bias && !c.use_swiglu_ffn,
        "unsupported DINOv2 variant: only GELU MLPs with query/key/value biases load"
    );
    ensure!(
        c.layer_norm_eps.is_finite() && c.layer_norm_eps > 0.0,
        "invalid DINOv2 normalization epsilon"
    );
    Ok(())
}

/// Squash a frame to `size x size` RGB with an antialiased triangle filter
/// (the reference processor's bilinear resize, without its center crop, so
/// events at the frame edges still count).
pub fn prepare(frame: &Frame, size: usize) -> Result<Vec<u8>> {
    // Decimate large sensors to a few times the input first; the triangle
    // filter then averages that preview rather than single sensor samples.
    let (w, h, rgba) = frame::preview_rgba(frame, 4 * size as u32, 4 * size as u32)?;
    let (w, h) = (w as usize, h as usize);
    ensure!(
        w > 0 && h > 0 && rgba.len() == w * h * 4,
        "invalid DINOv2 preview dimensions"
    );
    let horizontal = filter(w, size);
    let vertical = filter(h, size);
    let mut rows = vec![0.0_f32; h * size * 3];
    for y in 0..h {
        let line = &rgba[y * w * 4..][..w * 4];
        for (x, (first, weights)) in horizontal.iter().enumerate() {
            let mut sum = [0.0_f32; 3];
            for (i, weight) in weights.iter().enumerate() {
                let pixel = &line[(first + i) * 4..][..3];
                for channel in 0..3 {
                    sum[channel] += f32::from(pixel[channel]) * weight;
                }
            }
            rows[(y * size + x) * 3..][..3].copy_from_slice(&sum);
        }
    }
    let mut out = vec![0_u8; size * size * 3];
    for (y, (first, weights)) in vertical.iter().enumerate() {
        for x in 0..size {
            for channel in 0..3 {
                let value: f32 = weights
                    .iter()
                    .enumerate()
                    .map(|(i, weight)| rows[((first + i) * size + x) * 3 + channel] * weight)
                    .sum();
                out[(y * size + x) * 3 + channel] = value.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    Ok(out)
}

/// Interleaved RGB bytes to an ImageNet-normalized `[3, size, size]` image.
pub(crate) fn normalize(rgb: &[u8], size: usize) -> Result<Vec<f32>> {
    ensure!(
        rgb.len() == size * size * 3,
        "DINOv2 input must be {size} x {size} RGB"
    );
    let mut out = vec![0.0; rgb.len()];
    for (i, pixel) in rgb.as_chunks::<3>().0.iter().enumerate() {
        for channel in 0..3 {
            out[channel * size * size + i] =
                (f32::from(pixel[channel]) / 255.0 - MEAN[channel]) / STD[channel];
        }
    }
    Ok(out)
}

pub(crate) fn unit(mut v: Vec<f32>) -> Result<Vec<f32>> {
    let length = v.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
    ensure!(
        length.is_finite() && length > 0.0,
        "DINOv2 produced a non-finite embedding"
    );
    v.iter_mut()
        .for_each(|x| *x = (f64::from(*x) / length) as f32);
    Ok(v)
}

/// A validated view of a safetensors file: an 8-byte little-endian header
/// length, a JSON header of dtype/shape/offsets, then the raw tensor bytes.
struct Tensors<'a> {
    data: &'a [u8],
    entries: HashMap<String, (String, Vec<usize>, usize, usize)>,
    metadata: HashMap<String, String>,
}

impl<'a> Tensors<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self> {
        ensure!(
            bytes.len() >= 8,
            "DINOv2 weights are not a safetensors file"
        );
        let header = u64::from_le_bytes(bytes[..8].try_into()?);
        ensure!(
            header <= MAX_HEADER_BYTES && 8 + header <= bytes.len() as u64,
            "DINOv2 weights have an invalid safetensors header"
        );
        let (header, data) = bytes[8..].split_at(header as usize);
        #[derive(Deserialize)]
        struct Entry {
            dtype: String,
            shape: Vec<usize>,
            data_offsets: [usize; 2],
        }
        let raw: HashMap<String, serde_json::Value> =
            serde_json::from_slice(header).context("parse safetensors header")?;
        let mut entries = HashMap::new();
        let mut metadata = HashMap::new();
        for (name, value) in raw {
            if name == "__metadata__" {
                metadata = serde_json::from_value(value).context("safetensors metadata")?;
                continue;
            }
            let entry: Entry = serde_json::from_value(value)
                .with_context(|| format!("invalid safetensors entry {name}"))?;
            let [start, end] = entry.data_offsets;
            ensure!(
                start <= end && end <= data.len(),
                "safetensors tensor {name} is out of bounds"
            );
            entries.insert(name, (entry.dtype, entry.shape, start, end));
        }
        Ok(Self {
            data,
            entries,
            metadata,
        })
    }

    /// An f32 copy of a tensor with an exact shape.
    fn get(&self, name: &str, shape: &[usize]) -> Result<Vec<f32>> {
        let (dtype, stored, start, end) = self
            .entries
            .get(name)
            .with_context(|| format!("DINOv2 weights are missing tensor {name}"))?;
        ensure!(
            stored == shape,
            "DINOv2 tensor {name} has shape {stored:?}, expected {shape:?}"
        );
        let count = shape
            .iter()
            .try_fold(1_usize, |n, d| n.checked_mul(*d))
            .context("tensor size overflow")?;
        let bytes = &self.data[*start..*end];
        let width = match dtype.as_str() {
            "F32" => 4,
            "F16" | "BF16" => 2,
            other => bail!("DINOv2 tensor {name} has unsupported dtype {other}"),
        };
        ensure!(
            bytes.len() == count * width,
            "DINOv2 tensor {name} has the wrong byte length"
        );
        let values: Vec<f32> = match dtype.as_str() {
            "F32" => bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect(),
            "F16" => bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| half::f16::from_le_bytes(*b).to_f32())
                .collect(),
            _ => bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| half::bf16::from_le_bytes(*b).to_f32())
                .collect(),
        };
        ensure!(
            values.iter().all(|v| v.is_finite()),
            "DINOv2 tensor {name} contains non-finite values"
        );
        Ok(values)
    }
}

/// C = alpha * A B for row/column-strided views; C is overwritten.
fn gemm(
    (m, k, n): (usize, usize, usize),
    (a, a_row, a_col): (&[f32], usize, usize),
    (b, b_row, b_col): (&[f32], usize, usize),
    (c, c_row, c_col): (&mut [f32], usize, usize),
    alpha: f32,
) {
    let last = |rows: usize, columns: usize, rs: usize, cs: usize| {
        (rows - 1) * rs + (columns - 1) * cs + 1
    };
    assert!(m > 0 && k > 0 && n > 0);
    assert!(a.len() >= last(m, k, a_row, a_col));
    assert!(b.len() >= last(k, n, b_row, b_col));
    assert!(c.len() >= last(m, n, c_row, c_col));
    // SAFETY: the assertions above keep every strided element of A, B and C
    // inside its slice, and C is uniquely borrowed for the whole call.
    unsafe {
        matrixmultiply::sgemm(
            m,
            k,
            n,
            alpha,
            a.as_ptr(),
            a_row as isize,
            a_col as isize,
            b.as_ptr(),
            b_row as isize,
            b_col as isize,
            0.0,
            c.as_mut_ptr(),
            c_row as isize,
            c_col as isize,
        );
    }
}

/// `rows x in` times the transposed `[out, in]` PyTorch weight, plus bias.
fn linear(x: &[f32], rows: usize, layer: &Linear) -> Vec<f32> {
    let mut out = vec![0.0; rows * layer.outputs];
    gemm(
        (rows, layer.inputs, layer.outputs),
        (x, layer.inputs, 1),
        (&layer.weight, 1, layer.inputs),
        (&mut out, layer.outputs, 1),
        1.0,
    );
    for row in out.chunks_exact_mut(layer.outputs) {
        row.iter_mut().zip(&layer.bias).for_each(|(v, b)| *v += b);
    }
    out
}

fn layer_norm(x: &[f32], d: usize, norm: &Norm, eps: f64) -> Vec<f32> {
    let mut out = Vec::with_capacity(x.len());
    for row in x.chunks_exact(d) {
        let mean = row.iter().map(|v| f64::from(*v)).sum::<f64>() / d as f64;
        let variance = row
            .iter()
            .map(|v| (f64::from(*v) - mean).powi(2))
            .sum::<f64>()
            / d as f64;
        let inverse = 1.0 / (variance + eps).sqrt();
        out.extend(
            row.iter()
                .zip(&norm.weight)
                .zip(&norm.bias)
                .map(|((v, w), b)| ((f64::from(*v) - mean) * inverse) as f32 * w + b),
        );
    }
    out
}

fn residual(x: &mut [f32], update: &[f32], scale: &[f32]) {
    for (row, update) in x
        .chunks_exact_mut(scale.len())
        .zip(update.chunks_exact(scale.len()))
    {
        for ((v, u), s) in row.iter_mut().zip(update).zip(scale) {
            *v += u * s;
        }
    }
}

fn softmax(row: &mut [f32]) {
    let max = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let mut sum = 0.0_f64;
    for v in row.iter_mut() {
        *v = (*v - max).exp();
        sum += f64::from(*v);
    }
    let inverse = (1.0 / sum) as f32;
    row.iter_mut().for_each(|v| *v *= inverse);
}

/// PyTorch's exact-erf GELU.
fn gelu(x: f32) -> f32 {
    0.5 * x * (1.0 + libm::erff(x / std::f32::consts::SQRT_2))
}

/// torch `interpolate(mode="bicubic", align_corners=False)` taps for each
/// output sample: four clamped source indices and Keys cubic weights (a=-0.75).
fn bicubic(input: usize, output: usize) -> Vec<([usize; 4], [f32; 4])> {
    let cubic = |x: f32| {
        let a = -0.75_f32;
        let x = x.abs();
        if x <= 1.0 {
            ((a + 2.0) * x - (a + 3.0)) * x * x + 1.0
        } else if x < 2.0 {
            ((a * x - 5.0 * a) * x + 8.0 * a) * x - 4.0 * a
        } else {
            0.0
        }
    };
    let scale = input as f32 / output as f32;
    (0..output)
        .map(|o| {
            let real = scale * (o as f32 + 0.5) - 0.5;
            let floor = real.floor();
            let t = real - floor;
            let base = floor as isize;
            let index = [-1, 0, 1, 2].map(|k| (base + k).clamp(0, input as isize - 1) as usize);
            let weights = [cubic(t + 1.0), cubic(t), cubic(1.0 - t), cubic(2.0 - t)];
            (index, weights)
        })
        .collect()
}

/// For each output sample, its first input index and normalized weights.
fn filter(input: usize, output: usize) -> Vec<(usize, Vec<f32>)> {
    let scale = input as f64 / output as f64;
    let support = scale.max(1.0);
    (0..output)
        .map(|o| {
            let center = (o as f64 + 0.5) * scale;
            let first = ((center - support).floor().max(0.0)) as usize;
            let last = ((center + support).ceil() as usize).min(input);
            let mut weights: Vec<f64> = (first..last)
                .map(|i| (1.0 - ((i as f64 + 0.5 - center) / support).abs()).max(0.0))
                .collect();
            let total: f64 = weights.iter().sum();
            if total <= 0.0 {
                let nearest = (center as usize).min(input - 1);
                return (nearest, vec![1.0]);
            }
            weights.iter_mut().for_each(|w| *w /= total);
            (first, weights.into_iter().map(|w| w as f32).collect())
        })
        .collect()
}

#[cfg(test)]
mod tests;
