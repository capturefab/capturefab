//! DINOv2 on the GPU through wgpu compute shaders (Metal, Vulkan or DX12; no
//! vendor SDK). Every dispatch of the forward pass is recorded once at start-up
//! with fixed buffers, so a frame is one 600 KB upload, one submission and a
//! 1.5 KB readback. Only `helper.rs` runs this in production: a driver fault or
//! hang then cannot take the camera worker down.
use super::Model;
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::{
    future::Future,
    sync::{Arc, Mutex},
};

/// The attention kernel keeps one head of a query in workgroup memory.
pub const MAX_HEAD: usize = 64;
/// Bounds the per-head score matrices (`heads x tokens x tokens`).
const MAX_TOKENS: usize = 2048;
const SHADER: &str = include_str!("dinov2.wgsl");

/// Matrix-product epilogues, matching `mode` in dinov2.wgsl.
#[derive(Clone, Copy)]
enum Epilogue {
    Bias = 0,
    Gelu = 1,
    /// out += layer_scale * (A W^T + b): the residual stream update.
    Residual = 2,
    /// out = A W^T + b + positions: patch embeddings into token rows 1...
    Positions = 3,
}

pub struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    columns: wgpu::Buffer,
    readback: wgpu::Buffer,
    commands: Vec<Command>,
    cls: wgpu::Buffer,
    x: wgpu::Buffer,
    out: wgpu::Buffer,
    pipelines: [wgpu::ComputePipeline; 4],
    dims: usize,
    column_values: usize,
    adapter: String,
    error: Arc<Mutex<Option<String>>>,
}

struct Command {
    pipeline: usize,
    group: wgpu::BindGroup,
    workgroups: (u32, u32, u32),
}

/// Block on a wgpu future; native backends resolve without an event loop.
fn block_on<F: Future>(future: F) -> F::Output {
    use std::task::{Context as TaskContext, Poll, Wake, Waker};
    struct Thread(std::thread::Thread);
    impl Wake for Thread {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Thread(std::thread::current())));
    let mut context = TaskContext::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

impl Gpu {
    /// Upload `model` to the first hardware GPU, for `height x width` inputs.
    pub fn new(model: &Model, height: usize, width: usize) -> Result<Self> {
        let c = &model.config;
        let (d, p) = (c.hidden_size, c.patch_size);
        let (rows, columns) = (height / p, width / p);
        let patches = rows * columns;
        let tokens = 1 + patches;
        let heads = c.num_attention_heads;
        ensure!(
            tokens <= MAX_TOKENS && d / heads <= MAX_HEAD,
            "DINOv2 input is too large for the GPU kernels"
        );
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .map_err(|e| anyhow!("no GPU adapter: {e}"))?;
        let info = adapter.get_info();
        // A software rasterizer is slower than the native CPU path.
        ensure!(
            info.device_type != wgpu::DeviceType::Cpu,
            "only a software GPU ({}) is available",
            info.name
        );
        let limits = adapter.limits();
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("capturefab-dinov2"),
            required_features: wgpu::Features::empty(),
            required_limits: limits.clone(),
            ..Default::default()
        }))
        .context("open GPU device")?;
        let error = Arc::new(Mutex::new(None));
        let sink = error.clone();
        device.on_uncaptured_error(Arc::new(move |e: wgpu::Error| {
            let mut slot = sink.lock().unwrap_or_else(|e| e.into_inner());
            slot.get_or_insert_with(|| e.to_string());
        }));
        device.push_error_scope(wgpu::ErrorFilter::Validation);

        // Every parameter in one storage buffer, addressed by f32 offsets.
        let mut weights: Vec<f32> = Vec::new();
        let mut put = |values: &[f32]| -> u32 {
            let offset = weights.len() as u32;
            weights.extend_from_slice(values);
            offset
        };
        let positions = model.positions(rows, columns);
        let cls_row: Vec<f32> = model
            .cls
            .iter()
            .zip(&positions[..d])
            .map(|(c, p)| c + p)
            .collect();
        let patch = (
            put(&model.patch.weight),
            put(&model.patch.bias),
            put(&positions),
        );
        struct LayerOffsets {
            norm1: (u32, u32),
            qkv: (u32, u32),
            output: (u32, u32),
            scale1: u32,
            norm2: (u32, u32),
            fc1: (u32, u32),
            fc2: (u32, u32),
            scale2: u32,
        }
        let layers: Vec<LayerOffsets> = model
            .layers
            .iter()
            .map(|l| LayerOffsets {
                norm1: (put(&l.norm1.weight), put(&l.norm1.bias)),
                qkv: (put(&l.qkv.weight), put(&l.qkv.bias)),
                output: (put(&l.output.weight), put(&l.output.bias)),
                scale1: put(&l.scale1),
                norm2: (put(&l.norm2.weight), put(&l.norm2.bias)),
                fc1: (put(&l.fc1.weight), put(&l.fc1.bias)),
                fc2: (put(&l.fc2.weight), put(&l.fc2.bias)),
                scale2: put(&l.scale2),
            })
            .collect();
        let norm = (put(&model.norm.weight), put(&model.norm.bias));
        let bytes = weights.len() as u64 * 4;
        ensure!(
            bytes <= u64::from(limits.max_storage_buffer_binding_size)
                && bytes <= limits.max_buffer_size,
            "DINOv2 weights exceed this GPU's storage buffer limit"
        );
        let storage = |label: &str, values: usize, extra: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: (values.max(1) * 4) as u64,
                usage: wgpu::BufferUsages::STORAGE | extra,
                mapped_at_creation: false,
            })
        };
        let weight_buffer = storage("weights", weights.len(), wgpu::BufferUsages::COPY_DST);
        queue.write_buffer(&weight_buffer, 0, as_bytes(&weights));
        drop(weights);
        let hidden = d * c.mlp_ratio;
        let column_values = patches * model.patch.inputs;
        let copy = wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC;
        let columns_buffer = storage("columns", column_values, wgpu::BufferUsages::COPY_DST);
        let x = storage("x", tokens * d, copy);
        let h = storage("h", tokens * d, wgpu::BufferUsages::empty());
        let qkv = storage("qkv", tokens * 3 * d, wgpu::BufferUsages::empty());
        let attended = storage("attended", tokens * d, wgpu::BufferUsages::empty());
        let scores = storage(
            "scores",
            heads * tokens * tokens,
            wgpu::BufferUsages::empty(),
        );
        let mlp = storage("mlp", tokens * hidden, wgpu::BufferUsages::empty());
        let out = storage("out", d, wgpu::BufferUsages::COPY_SRC);
        let cls = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cls"),
            size: (d * 4) as u64,
            usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&cls, 0, as_bytes(&cls_row));
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (d * 4) as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("dinov2"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline = |entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: None,
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let pipelines = [
            pipeline("matmul"),
            pipeline("layer_norm"),
            pipeline("gemm"),
            pipeline("softmax"),
        ];
        let uniform = |values: &[u32]| {
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("parameters"),
                size: 64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let mut padded = [0_u32; 16];
            padded[..values.len()].copy_from_slice(values);
            queue.write_buffer(&buffer, 0, as_bytes(&padded));
            buffer
        };
        let group = |pipeline: usize, buffers: &[&wgpu::Buffer]| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &pipelines[pipeline].get_bind_group_layout(0),
                entries: &buffers
                    .iter()
                    .enumerate()
                    .map(|(binding, buffer)| wgpu::BindGroupEntry {
                        binding: binding as u32,
                        resource: buffer.as_entire_binding(),
                    })
                    .collect::<Vec<_>>(),
            })
        };
        let matmul = |a: &wgpu::Buffer,
                      c: &wgpu::Buffer,
                      (m, n, k): (usize, usize, usize),
                      epilogue: Epilogue,
                      (weight, bias, extra): (u32, u32, u32),
                      row_offset: usize| {
            let parameters = uniform(&[
                m as u32,
                n as u32,
                k as u32,
                epilogue as u32,
                weight,
                bias,
                extra,
                row_offset as u32,
            ]);
            Command {
                pipeline: 0,
                group: group(0, &[&parameters, a, &weight_buffer, c]),
                workgroups: (n.div_ceil(64) as u32, m.div_ceil(64) as u32, 1),
            }
        };
        let eps = (c.layer_norm_eps as f32).to_bits();
        let layer_norm = |input: &wgpu::Buffer,
                          output: &wgpu::Buffer,
                          rows: usize,
                          (weight, bias): (u32, u32)| {
            let parameters = uniform(&[d as u32, weight, bias, eps]);
            Command {
                pipeline: 1,
                group: group(1, &[&parameters, input, &weight_buffer, output]),
                workgroups: (rows as u32, 1, 1),
            }
        };
        // Per head h (the batch index): scores_h = scale * Q_h K_h^T, then
        // attended[:, h] = softmax(scores_h) V_h, with Q, K, V views of qkv.
        let head = d / heads;
        let (t, three) = (tokens as u32, 3 * d as u32);
        let scale = ((head as f32).powf(-0.5)).to_bits();
        let scores_parameters = uniform(&[
            t,
            t,
            head as u32,
            scale, //
            0,
            three,
            1,
            head as u32, // A = Q_h: [token][e]
            d as u32,
            1,
            three,
            head as u32, // B = K_h^T: [e][token]
            0,
            t,
            t * t,
            0, // C = scores_h
        ]);
        let scores_group = group(2, &[&scores_parameters, &qkv, &qkv, &scores]);
        let softmax_parameters = uniform(&[t]);
        let softmax_group = group(3, &[&softmax_parameters, &scores]);
        let values_parameters = uniform(&[
            t,
            head as u32,
            t,
            1.0_f32.to_bits(), //
            0,
            t,
            1,
            t * t, // A = P_h: [token][token]
            2 * d as u32,
            three,
            1,
            head as u32, // B = V_h: [token][e]
            0,
            d as u32,
            head as u32,
            0, // C = attended[:, h]
        ]);
        let values_group = group(2, &[&values_parameters, &scores, &qkv, &attended]);
        let attention = [
            Command {
                pipeline: 2,
                group: scores_group,
                workgroups: (
                    tokens.div_ceil(64) as u32,
                    tokens.div_ceil(64) as u32,
                    heads as u32,
                ),
            },
            Command {
                pipeline: 3,
                group: softmax_group,
                workgroups: ((heads * tokens) as u32, 1, 1),
            },
            Command {
                pipeline: 2,
                group: values_group,
                workgroups: (
                    head.div_ceil(64) as u32,
                    tokens.div_ceil(64) as u32,
                    heads as u32,
                ),
            },
        ];
        let mut commands = vec![matmul(
            &columns_buffer,
            &x,
            (patches, d, model.patch.inputs),
            Epilogue::Positions,
            patch,
            1,
        )];
        for l in &layers {
            commands.push(layer_norm(&x, &h, tokens, l.norm1));
            commands.push(matmul(
                &h,
                &qkv,
                (tokens, 3 * d, d),
                Epilogue::Bias,
                (l.qkv.0, l.qkv.1, 0),
                0,
            ));
            commands.extend(attention.iter().map(|command| Command {
                pipeline: command.pipeline,
                group: command.group.clone(),
                workgroups: command.workgroups,
            }));
            commands.push(matmul(
                &attended,
                &x,
                (tokens, d, d),
                Epilogue::Residual,
                (l.output.0, l.output.1, l.scale1),
                0,
            ));
            commands.push(layer_norm(&x, &h, tokens, l.norm2));
            commands.push(matmul(
                &h,
                &mlp,
                (tokens, hidden, d),
                Epilogue::Gelu,
                (l.fc1.0, l.fc1.1, 0),
                0,
            ));
            commands.push(matmul(
                &mlp,
                &x,
                (tokens, d, hidden),
                Epilogue::Residual,
                (l.fc2.0, l.fc2.1, l.scale2),
                0,
            ));
        }
        // Only the CLS row is ever read back.
        commands.push(layer_norm(&x, &out, 1, norm));
        if let Some(error) = block_on(device.pop_error_scope()) {
            bail!("GPU pipeline setup failed: {error}");
        }
        Ok(Self {
            adapter: format!("{} ({:?})", info.name, info.backend),
            device,
            queue,
            columns: columns_buffer,
            readback,
            commands,
            cls,
            x,
            out,
            pipelines,
            dims: d,
            column_values,
            error,
        })
    }

    /// The adapter in use, for example "Apple M4 Max (Metal)".
    pub fn adapter(&self) -> &str {
        &self.adapter
    }

    /// Embed an `input() x input()` RGB image (for a model built for that size).
    pub fn embed_rgb(&mut self, model: &Model, rgb: &[u8]) -> Result<Vec<f32>> {
        let size = model.input();
        let columns = model.columns(&super::normalize(rgb, size)?, size, size)?;
        self.embed_columns(&columns)
    }

    /// Run the network on precomputed patch columns.
    pub fn embed_columns(&mut self, columns: &[f32]) -> Result<Vec<f32>> {
        ensure!(
            columns.len() == self.column_values,
            "DINOv2 GPU input has the wrong size"
        );
        self.queue.write_buffer(&self.columns, 0, as_bytes(columns));
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let row = (self.dims * 4) as u64;
        encoder.copy_buffer_to_buffer(&self.cls, 0, &self.x, 0, row);
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
            for command in &self.commands {
                pass.set_pipeline(&self.pipelines[command.pipeline]);
                pass.set_bind_group(0, &command.group, &[]);
                let (x, y, z) = command.workgroups;
                pass.dispatch_workgroups(x, y, z);
            }
        }
        encoder.copy_buffer_to_buffer(&self.out, 0, &self.readback, 0, row);
        self.queue.submit([encoder.finish()]);
        let slice = self.readback.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .map_err(|e| anyhow!("GPU poll failed: {e}"))?;
        receiver
            .recv()
            .context("GPU readback was dropped")?
            .map_err(|e| anyhow!("GPU readback failed: {e}"))?;
        let values: Vec<f32> = slice
            .get_mapped_range()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        self.readback.unmap();
        if let Some(error) = self.error.lock().unwrap_or_else(|e| e.into_inner()).take() {
            bail!("GPU error: {error}");
        }
        super::unit(values)
    }
}

fn as_bytes<T: Copy>(values: &[T]) -> &[u8] {
    // SAFETY: f32 and u32 have no padding or invalid bit patterns, and the
    // byte view borrows `values` for its whole lifetime.
    unsafe { std::slice::from_raw_parts(values.as_ptr().cast(), std::mem::size_of_val(values)) }
}
