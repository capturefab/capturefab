//! GPU presentation of camera frames.
//!
//! Frames are uploaded in their sensor layout as integer textures and decoded
//! by a shader that reproduces `frame::convert` bit for bit, followed by the
//! bilinear filtering iced applies to its own images. Bayer frames shown near
//! full size are decoded once per frame into an 8-bit RGBA texture, so redraws
//! at high refresh rates and during animations filter it instead of repeating
//! the demosaic for every screen pixel; see `decode_once`. The CPU only hands
//! sensor bytes to the driver: no demosaic, no RGBA expansion, and a quarter of
//! the upload for 8-bit mono and Bayer. When iced falls back to its software
//! renderer the shader never runs, `available` stays false, and the workbench
//! keeps converting on the CPU.
use crate::types::{Frame, MONO8, RGB8};
use iced::widget::shader::{self, Viewport};
use iced::{Rectangle, mouse, wgpu};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex, PoisonError, Weak,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

const SHADER: &str = r#"
struct Uniforms {
    // Image corners in clip space: left, top, right, bottom.
    rect: vec4<f32>,
    size: vec2<i32>,
    kind: i32,
    shift: i32,
    cfa: vec4<i32>,
    // x: the target is sRGB, so output linear light.
    // y: texels in a row of a folded frame. z: bytes in a row of the frame.
    // w: how a folded frame packs its samples: 1 PFNC lsb, 2 GigE Vision.
    flags: vec4<i32>,
};

@group(0) @binding(0) var frame: texture_2d<u32>;
@group(0) @binding(1) var<uniform> u: Uniforms;

struct Varyings {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

fn corner(index: u32) -> vec2<f32> {
    return vec2<f32>(f32(index & 1u), f32(index >> 1u));
}

// Decode pass: covers the decoded texture, one fragment per sensor pixel.
@vertex
fn vs_decode(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    return vec4<f32>(corner(index) * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fs_decode(@builtin(position) position: vec4<f32>) -> @location(0) vec4<u32> {
    return vec4<u32>(decode(vec2<i32>(position.xy)), 255u);
}

// Drawing: the image rectangle on screen.
@vertex
fn vs(@builtin(vertex_index) index: u32) -> Varyings {
    let c = corner(index);
    var out: Varyings;
    out.position = vec4<f32>(
        mix(u.rect.x, u.rect.z, c.x),
        mix(u.rect.y, u.rect.w, c.y),
        0.0,
        1.0,
    );
    out.uv = c;
    return out;
}

// Four bytes of a folded frame from byte `at`, the first in the low bits:
// the frame's bytes in order, four to a texel, folded into rows of u.flags.y
// texels.
fn bytes(at: i32) -> u32 {
    let fold = u.flags.y;
    let word = at >> 2;
    let next = min(word + 1, (u.flags.z * u.size.y - 1) >> 2);
    let a = textureLoad(frame, vec2<i32>(word % fold, word / fold), 0);
    let b = textureLoad(frame, vec2<i32>(next % fold, next / fold), 0);
    let low = a.r | (a.g << 8u) | (a.b << 16u) | (a.a << 24u);
    let high = b.r | (b.g << 8u) | (b.b << 16u) | (b.a << 24u);
    let i = u32(at & 3) * 8u;
    return select((low >> i) | (high << (32u - i)), low, i == 0u);
}

// The sample at p shifted to 8 bits and clamped: one texel, or unpacked from
// a folded frame's row of u.shift + 8 bit samples.
fn texel(p: vec2<i32>) -> u32 {
    var t = 0u;
    if (u.flags.w == 0) {
        t = textureLoad(frame, p, 0).r;
    } else if (u.flags.w == 1) {
        let bits = u.shift + 8;
        let bit = p.x * bits;
        t = (bytes(p.y * u.flags.z + (bit >> 3u)) >> u32(bit & 7)) & ((1u << u32(bits)) - 1u);
    } else {
        // Two samples in three bytes: their high bits outside, low ones in
        // the middle byte, the first sample's in its low nibble.
        let b = bytes(p.y * u.flags.z + (p.x >> 1u) * 3);
        let odd = (p.x & 1) == 1;
        let high = select(b, b >> 16u, odd) & 0xffu;
        let low = select(b >> 8u, b >> 12u, odd) & ((1u << u32(u.shift)) - 1u);
        t = (high << u32(u.shift)) | low;
    }
    return min(t >> u32(u.shift), 255u);
}

fn cfa(p: vec2<i32>) -> i32 {
    return u.cfa[(p.y & 1) * 2 + (p.x & 1)];
}

fn channel(c: i32) -> vec3<u32> {
    return select(vec3<u32>(0u), vec3<u32>(1u), vec3<bool>(c == 0, c == 1, c == 2));
}

// Full-range BT.601 Y'CbCr 4:2:2 in 16-bit fixed point, as frame::ycbcr;
// an odd last pixel has no Cr and takes it as neutral.
fn yuv(p: vec2<i32>) -> vec3<u32> {
    let b = bytes(p.y * u.flags.z + (p.x & ~1) * 2);
    // Bit offsets of the first luma, Cb, the second luma and Cr.
    let at = select(vec4<u32>(8u, 0u, 24u, 16u), vec4<u32>(0u, 8u, 16u, 24u), u.kind == 6);
    let y = i32((b >> select(at.x, at.z, (p.x & 1) == 1)) & 0xffu);
    let cb = i32((b >> at.y) & 0xffu) - 128;
    let cr = select(i32((b >> at.w) & 0xffu), 128, (p.x | 1) >= u.size.x) - 128;
    let rgb = vec3<i32>(
        y + ((91881 * cr + 32768) >> 16u),
        y + ((-22554 * cb - 46802 * cr + 32768) >> 16u),
        y + ((116130 * cb + 32768) >> 16u),
    );
    return vec3<u32>(clamp(rgb, vec3<i32>(0), vec3<i32>(255)));
}

// Exactly frame::convert for one pixel: mono samples are shifted and clamped;
// RGB/BGR is three folded bytes per pixel and RGBa/BGRa one texel; YUV is
// converted per pair of pixels; Bayer averages the in-bounds 3x3 neighbors of
// each missing color with truncating integer division.
fn decode(p: vec2<i32>) -> vec3<u32> {
    if (u.kind == 1 || u.kind == 2) {
        let b = bytes(p.y * u.flags.z + p.x * 3);
        let a = vec3<u32>(b & 0xffu, (b >> 8u) & 0xffu, (b >> 16u) & 0xffu);
        return select(a.zyx, a, u.kind == 1);
    }
    if (u.kind == 4 || u.kind == 5) {
        let a = textureLoad(frame, p, 0).rgb;
        return select(a.zyx, a, u.kind == 4);
    }
    if (u.kind == 6 || u.kind == 7) {
        return yuv(p);
    }
    let t = texel(p);
    if (u.kind == 0) {
        return vec3<u32>(t);
    }
    var sum = vec3<u32>(0u);
    var n = vec3<u32>(0u);
    for (var dy = -1; dy <= 1; dy++) {
        for (var dx = -1; dx <= 1; dx++) {
            let q = p + vec2<i32>(dx, dy);
            if (any(q < vec2<i32>(0)) || any(q >= u.size)) {
                continue;
            }
            let mask = channel(cfa(q));
            sum += mask * texel(q);
            n += mask;
        }
    }
    let own = channel(cfa(p));
    let avg = select(vec3<u32>(t), sum / max(n, vec3<u32>(1u)), n > vec3<u32>(0u));
    return own * t + (vec3<u32>(1u) - own) * avg;
}

fn linear(c: vec3<f32>) -> vec3<f32> {
    return select(pow((c + 0.055) / 1.055, vec3<f32>(2.4)), c / 12.92, c <= vec3<f32>(0.04045));
}

// One decoded pixel: from the decode pass, or decoded here when `direct`.
fn pixel(p: vec2<i32>, direct: bool) -> vec3<f32> {
    let q = clamp(p, vec2<i32>(0), u.size - vec2<i32>(1));
    if (direct) {
        return vec3<f32>(decode(q));
    }
    return vec3<f32>(textureLoad(frame, q, 0).rgb);
}

// Linear filtering with clamp-to-edge over decoded 8-bit pixels, as iced
// samples its own images.
fn filtered(uv: vec2<f32>, direct: bool) -> vec4<f32> {
    let pos = uv * vec2<f32>(u.size) - 0.5;
    let base = floor(pos);
    let f = pos - base;
    let i0 = vec2<i32>(base);
    let c00 = pixel(i0, direct);
    let c10 = pixel(i0 + vec2<i32>(1, 0), direct);
    let c01 = pixel(i0 + vec2<i32>(0, 1), direct);
    let c11 = pixel(i0 + vec2<i32>(1, 1), direct);
    var rgb = mix(mix(c00, c10, f.x), mix(c01, c11, f.x), f.y) / 255.0;
    if (u.flags.x != 0) {
        rgb = linear(rgb);
    }
    return vec4<f32>(rgb, 1.0);
}

// Draws from the decode pass's texture.
@fragment
fn fs(in: Varyings) -> @location(0) vec4<f32> {
    return filtered(in.uv, false);
}

// Draws from the sensor texture.
@fragment
fn fs_direct(in: Varyings) -> @location(0) vec4<f32> {
    return filtered(in.uv, true);
}
"#;

/// The shader's `kind` for Bayer mosaics.
const BAYER: i32 = 3;

/// How a pixel format is stored on the GPU and decoded by the shader.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Layout {
    format: wgpu::TextureFormat,
    bytes_per_texel: u32,
    /// The frame's bytes in order, four to a texel, folded into rows as wide
    /// as a texture allows: RGB/BGR, YUV and packed samples. Any width fits,
    /// wider than the texture itself, up to four bytes for every texel it
    /// can hold.
    folded: bool,
    /// The PFNC code, which gives a folded frame's bytes per row.
    pixel_format: u32,
    kind: i32,
    shift: i32,
    cfa: [i32; 4],
    /// How a folded frame packs its samples: 0 not, 1 PFNC lsb, 2 GigE Vision.
    packing: i32,
}

impl Layout {
    fn of(pixel_format: u32) -> Option<Self> {
        let gray = |format, bytes_per_texel, shift| Self {
            format,
            bytes_per_texel,
            folded: false,
            pixel_format,
            kind: 0,
            shift,
            cfa: [0; 4],
            packing: 0,
        };
        // 16-bit texels are read in host byte order; PFNC data is little-endian.
        let wide = |shift| {
            cfg!(target_endian = "little").then(|| gray(wgpu::TextureFormat::R16Uint, 2, shift))
        };
        let folded = |kind| Self {
            folded: true,
            kind,
            ..gray(wgpu::TextureFormat::Rgba8Uint, 4, 0)
        };
        let mosaic = |layout: Self| Self {
            kind: BAYER,
            cfa: crate::frame::bayer_pattern(pixel_format).map(|c| c as i32),
            ..layout
        };
        match pixel_format {
            MONO8 | 0x0108_00b1 | 0x0108_00c6 => Some(gray(wgpu::TextureFormat::R8Uint, 1, 0)),
            0x0110_0003 => wide(2),
            0x0110_0005 => wide(4),
            0x0110_0025 => wide(6),
            0x0110_0007 | 0x0110_00b8 | 0x0110_00c7 => wide(8),
            RGB8 => Some(folded(1)),
            0x0218_0015 => Some(folded(2)),
            0x0220_0016 | 0x0220_0017 => Some(Self {
                kind: if pixel_format == 0x0220_0016 { 4 } else { 5 },
                ..gray(wgpu::TextureFormat::Rgba8Uint, 4, 0)
            }),
            0x0210_0032 | 0x0210_003b => Some(folded(6)),
            0x0210_001f | 0x0210_0043 => Some(folded(7)),
            0x0108_0008..=0x0108_000b => Some(mosaic(gray(wgpu::TextureFormat::R8Uint, 1, 0))),
            0x0110_000c..=0x0110_000f => wide(2).map(mosaic),
            0x0110_0010..=0x0110_0013 => wide(4).map(mosaic),
            0x0110_002e..=0x0110_0031 => wide(8).map(mosaic),
            _ => {
                let (_, bits, packing) = crate::frame::packing(pixel_format)?;
                let layout = Self {
                    shift: bits as i32 - 8,
                    packing: match packing {
                        crate::frame::Packing::Lsb => 1,
                        crate::frame::Packing::GigE => 2,
                    },
                    ..folded(0)
                };
                Some(if crate::frame::is_bayer(pixel_format) {
                    mosaic(layout)
                } else {
                    layout
                })
            }
        }
    }

    /// Bytes in a texture row of folded bytes, or in a row of texels.
    fn row(&self, width: u32) -> usize {
        if self.folded {
            crate::frame::row_bytes(width, self.pixel_format)
        } else {
            width as usize * self.bytes_per_texel as usize
        }
    }

    /// The texture a `width` × `height` frame takes, if its sides fit in
    /// `max` texels.
    fn extent(&self, (width, height): (u32, u32), max: u32) -> Option<(u32, u32)> {
        let (width, height) = if self.folded {
            let bytes = self.bytes((width, height));
            // The shader counts bytes in 32-bit integers.
            if i32::try_from(bytes).is_err() {
                return None;
            }
            let words = (bytes as u64).div_ceil(4);
            let row = words.clamp(1, u64::from(max.max(1)));
            (row, words.div_ceil(row))
        } else {
            (u64::from(width), u64::from(height))
        };
        let side = |v: u64| u32::try_from(v).ok().filter(|&v| v <= max);
        Some((side(width)?, side(height)?))
    }

    /// Bytes of a `width` × `height` frame.
    fn bytes(&self, (width, height): (u32, u32)) -> usize {
        self.row(width) * height as usize
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    rect: [f32; 4],
    size: [i32; 2],
    kind: i32,
    shift: i32,
    cfa: [i32; 4],
    flags: [i32; 4],
}

/// Set once wgpu builds the pipeline; never under the software renderer.
static AVAILABLE: AtomicBool = AtomicBool::new(false);
static MAX_SIDE: AtomicU32 = AtomicU32::new(0);

/// Whether wgpu built the preview pipeline. Still false after the main
/// window's first draws means iced's software renderer draws.
#[cfg_attr(test, allow(dead_code))]
pub fn available() -> bool {
    AVAILABLE.load(Ordering::Relaxed)
}

#[derive(Default)]
struct Shared {
    /// Newest frame per view awaiting upload; held by reference, never copied.
    pending: HashMap<String, Arc<Frame>>,
    /// Views no longer on screen, whose textures the pipeline frees.
    retired: HashSet<String>,
}

/// The app's side of GPU presentation: it submits frames by view key and
/// draws them with `view`.
#[derive(Clone, Default)]
pub struct GpuFrames {
    shared: Arc<Mutex<Shared>>,
    disabled: bool,
    /// A texture limit standing in for a built pipeline's, in tests.
    #[cfg(test)]
    limit: Option<u32>,
}

impl std::fmt::Debug for GpuFrames {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GpuFrames")
    }
}

impl GpuFrames {
    /// `CAPTUREFAB_GPU_PREVIEW=0` keeps CPU conversion, e.g. to rule out a driver.
    pub fn new() -> Self {
        Self {
            disabled: std::env::var_os("CAPTUREFAB_GPU_PREVIEW").is_some_and(|value| value == "0"),
            ..Self::default()
        }
    }

    /// Decodes on the GPU as if its pipeline were built with this texture
    /// limit, whichever renderer the tests run under.
    #[cfg(test)]
    pub fn limited(limit: u32) -> Self {
        Self {
            limit: Some(limit),
            ..Self::default()
        }
    }

    /// Converts every frame on the CPU, whichever renderer the tests run under.
    #[cfg(test)]
    pub fn cpu_only() -> Self {
        Self {
            disabled: true,
            ..Self::default()
        }
    }

    /// The frame awaiting upload under `key`.
    #[cfg(test)]
    pub fn pending(&self, key: &str) -> Option<Arc<Frame>> {
        self.shared().pending.get(key).cloned()
    }

    #[cfg(test)]
    pub fn is_retired(&self, key: &str) -> bool {
        self.shared().retired.contains(key)
    }

    /// The largest texture side, once the pipeline is built.
    fn limit(&self) -> Option<u32> {
        #[cfg(test)]
        if self.limit.is_some() {
            return self.limit;
        }
        AVAILABLE
            .load(Ordering::Relaxed)
            .then(|| MAX_SIDE.load(Ordering::Relaxed))
    }

    fn shared(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The largest texture side frames are decoded in, while the GPU
    /// decodes them at all.
    pub fn texture_side(&self) -> Option<u32> {
        self.limit().filter(|_| !self.disabled)
    }

    /// Whether this frame can be shown without CPU conversion.
    pub fn accepts(&self, frame: &Frame) -> bool {
        let Some(max) = self.texture_side() else {
            return false;
        };
        let size = (frame.width, frame.height);
        Layout::of(frame.pixel_format).is_some_and(|layout| {
            frame.width > 0
                && frame.height > 0
                && layout.extent(size, max).is_some()
                && frame.data.len() >= layout.bytes(size)
        })
    }

    /// Queue `frame` for display under `key`; it is uploaded when next drawn.
    pub fn submit(&self, key: &str, frame: Arc<Frame>) {
        let mut shared = self.shared();
        shared.retired.remove(key);
        shared.pending.insert(key.to_owned(), frame);
    }

    /// Forget views no longer shown; their textures are freed on the next draw.
    pub fn retire(&self, key: &str) {
        let mut shared = self.shared();
        shared.pending.remove(key);
        shared.retired.insert(key.to_owned());
    }

    /// Draws the frame shown under `key`, scaled by `scale` (None fits it)
    /// and centered in the widget.
    pub fn view(&self, key: &str, native: iced::Size, scale: Option<f32>) -> View {
        View {
            gpu: self.clone(),
            key: key.to_owned(),
            native,
            scale,
        }
    }

    /// A 1×1 view that makes wgpu build the pipeline, which is what turns the
    /// GPU path on. It draws nothing.
    pub fn probe(&self) -> View {
        self.view("", iced::Size::ZERO, None)
    }
}

pub struct View {
    gpu: GpuFrames,
    key: String,
    native: iced::Size,
    scale: Option<f32>,
}

impl<Message> shader::Program<Message> for View {
    type State = ();
    type Primitive = Primitive;

    fn draw(&self, _state: &(), _cursor: mouse::Cursor, bounds: Rectangle) -> Primitive {
        Primitive {
            gpu: self.gpu.clone(),
            key: self.key.clone(),
            image: super::preview::placement(bounds, self.native, self.scale),
        }
    }
}

#[derive(Debug)]
pub struct Primitive {
    gpu: GpuFrames,
    key: String,
    /// Where the image goes, in the same coordinates as the widget bounds.
    image: Rectangle,
}

struct Slot {
    /// Sensor bytes as uploaded.
    texture: wgpu::Texture,
    /// Allocated texture size and format.
    storage: (u32, u32, wgpu::TextureFormat),
    uniforms: wgpu::Buffer,
    /// Reads the sensor texture, to draw directly or to decode.
    bind_group: wgpu::BindGroup,
    /// The frame decoded once, while that is cheaper than per redraw.
    decoded: Option<Decoded>,
    /// The sensor texture holds a frame not yet decoded.
    stale: bool,
    /// Draw from `decoded` this frame.
    once: bool,
    layout: Layout,
    size: (u32, u32),
}

/// A frame decoded to 8-bit RGB, one texel per pixel.
struct Decoded {
    view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
}

/// Format of the decoded texture: exact 8-bit values, filtered in the shader.
const DECODED: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Uint;

/// Whether to decode each frame once into a texture rather than per screen
/// pixel on every redraw. Only the Bayer demosaic is worth it, at nine texel
/// reads per decoded pixel against one to three for the other formats. Drawing
/// directly decodes the four pixels each screen pixel filters, so decoding the
/// whole frame instead pays off unless the image is shown at under half size
/// each way, as in overview tiles of high-resolution cameras.
fn decode_once(layout: Layout, size: (u32, u32), shown: f32) -> bool {
    layout.kind == BAYER && f64::from(size.0) * f64::from(size.1) <= f64::from(shown) * 4.0
}

fn texture(
    device: &wgpu::Device,
    (width, height): (u32, u32),
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("capturefab frame"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | usage,
        view_formats: &[],
    })
}

fn bind(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    view: &wgpu::TextureView,
    uniforms: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("capturefab frame"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: uniforms.as_entire_binding(),
            },
        ],
    })
}

pub struct Pipeline {
    /// Draws a decoded texture.
    pipeline: wgpu::RenderPipeline,
    /// Draws a sensor texture, decoding as it goes.
    direct: wgpu::RenderPipeline,
    /// Decodes a sensor texture into a decoded one.
    decode: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    srgb: bool,
    slots: HashMap<(usize, String), Slot>,
    owners: HashMap<usize, Weak<Mutex<Shared>>>,
    /// Lets tests check direct drawing of frames `decode_once` would cache.
    #[cfg(test)]
    direct_only: bool,
}

impl shader::Pipeline for Pipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("capturefab frame decode"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("capturefab frame"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Uint,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("capturefab frame"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let pipeline = |vs, fs, format| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("capturefab frame"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some(vs),
                    buffers: &[],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(fs),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..wgpu::PrimitiveState::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            })
        };
        let decode = pipeline("vs_decode", "fs_decode", DECODED);
        let direct = pipeline("vs", "fs_direct", format);
        let pipeline = pipeline("vs", "fs", format);
        MAX_SIDE.store(device.limits().max_texture_dimension_2d, Ordering::Relaxed);
        AVAILABLE.store(true, Ordering::Relaxed);
        Self {
            pipeline,
            direct,
            decode,
            bind_group_layout,
            srgb: format.is_srgb(),
            slots: HashMap::new(),
            owners: HashMap::new(),
            #[cfg(test)]
            direct_only: false,
        }
    }
}

impl shader::Primitive for Primitive {
    type Pipeline = Pipeline;

    fn prepare(
        &self,
        pipeline: &mut Pipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bounds: &Rectangle,
        viewport: &Viewport,
    ) {
        let owner = Arc::as_ptr(&self.gpu.shared) as usize;
        pipeline
            .owners
            .entry(owner)
            .or_insert_with(|| Arc::downgrade(&self.gpu.shared));
        pipeline
            .owners
            .retain(|_, shared| shared.strong_count() > 0);
        let owners = &pipeline.owners;
        pipeline
            .slots
            .retain(|(owner, _), _| owners.contains_key(owner));
        let key = (owner, self.key.clone());
        let frame = {
            let mut shared = self.gpu.shared();
            for retired in shared.retired.drain().collect::<Vec<_>>() {
                pipeline.slots.remove(&(owner, retired));
            }
            shared.pending.remove(&self.key)
        };
        let uploaded = match frame.map(|frame| pipeline.upload(device, queue, &key, &frame)) {
            Some(Ok(())) => true,
            Some(Err(error)) => {
                eprintln!("capturefab: GPU preview upload failed: {error}");
                pipeline.slots.remove(&key);
                false
            }
            None => false,
        };
        let Some(slot) = pipeline.slots.get_mut(&key) else {
            return;
        };
        slot.stale |= uploaded;
        let scale = viewport.scale_factor();
        let shown = self.image.width * self.image.height * scale * scale;
        slot.once = decode_once(slot.layout, slot.size, shown);
        #[cfg(test)]
        {
            slot.once &= !pipeline.direct_only;
        }
        // Clip space spans the widget: x right and y up, from -1 to 1.
        let x = |v: f32| (v - bounds.x) / bounds.width * 2.0 - 1.0;
        let y = |v: f32| 1.0 - (v - bounds.y) / bounds.height * 2.0;
        let image = self.image;
        let uniforms = Uniforms {
            rect: [
                x(image.x),
                y(image.y),
                x(image.x + image.width),
                y(image.y + image.height),
            ],
            size: [slot.size.0 as i32, slot.size.1 as i32],
            kind: slot.layout.kind,
            shift: slot.layout.shift,
            cfa: slot.layout.cfa,
            flags: [
                i32::from(pipeline.srgb),
                if slot.layout.folded {
                    slot.storage.0 as i32
                } else {
                    0
                },
                slot.layout.row(slot.size.0) as i32,
                slot.layout.packing,
            ],
        };
        queue.write_buffer(&slot.uniforms, 0, bytemuck::bytes_of(&uniforms));
        if !slot.once {
            slot.decoded = None;
            return;
        }
        if slot.decoded.is_none() {
            let view = texture(
                device,
                slot.size,
                DECODED,
                wgpu::TextureUsages::RENDER_ATTACHMENT,
            )
            .create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = bind(device, &pipeline.bind_group_layout, &view, &slot.uniforms);
            slot.decoded = Some(Decoded { view, bind_group });
            slot.stale = true;
        }
        if let Some(decoded) = &slot.decoded
            && slot.stale
        {
            // Redraws until the next frame only filter the result. Submitting
            // here also flushes the texture and uniform writes first.
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("capturefab frame decode"),
            });
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("capturefab frame decode"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &decoded.view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });
                pass.set_pipeline(&pipeline.decode);
                pass.set_bind_group(0, &slot.bind_group, &[]);
                pass.draw(0..4, 0..1);
            }
            queue.submit([encoder.finish()]);
            slot.stale = false;
        }
    }

    fn draw(&self, pipeline: &Pipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        let key = (Arc::as_ptr(&self.gpu.shared) as usize, self.key.clone());
        if let Some(slot) = pipeline.slots.get(&key) {
            let (draw, bind_group) = match &slot.decoded {
                Some(decoded) if slot.once => (&pipeline.pipeline, &decoded.bind_group),
                _ => (&pipeline.direct, &slot.bind_group),
            };
            render_pass.set_pipeline(draw);
            render_pass.set_bind_group(0, bind_group, &[]);
            render_pass.draw(0..4, 0..1);
        }
        true
    }
}

impl Pipeline {
    /// Upload `frame` into the view's texture, reallocating only when its size
    /// or storage format changes.
    fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        key: &(usize, String),
        frame: &Frame,
    ) -> Result<(), String> {
        let layout = Layout::of(frame.pixel_format).ok_or("unsupported pixel format")?;
        let size = (frame.width, frame.height);
        let (width, height) = layout
            .extent(size, device.limits().max_texture_dimension_2d)
            .ok_or("frame too large for a texture")?;
        let row = (width * layout.bytes_per_texel) as usize;
        let pixels = frame
            .data
            .get(..layout.bytes(size))
            .ok_or("truncated frame")?;
        let storage = (width, height, layout.format);
        if self
            .slots
            .get(key)
            .is_none_or(|slot| slot.storage != storage || slot.size != size)
        {
            let texture = texture(
                device,
                (width, height),
                layout.format,
                wgpu::TextureUsages::COPY_DST,
            );
            let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("capturefab frame uniforms"),
                size: std::mem::size_of::<Uniforms>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = bind(device, &self.bind_group_layout, &view, &uniforms);
            self.slots.insert(
                key.clone(),
                Slot {
                    texture,
                    storage,
                    uniforms,
                    bind_group,
                    decoded: None,
                    stale: true,
                    once: false,
                    layout,
                    size,
                },
            );
        }
        let slot = self.slots.get_mut(key).ok_or("missing slot")?;
        slot.layout = layout;
        let write = |y: usize, rows: &[u8]| {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &slot.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: 0,
                        y: y as u32,
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                rows,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row as u32),
                    rows_per_image: None,
                },
                wgpu::Extent3d {
                    width,
                    height: (rows.len() / row) as u32,
                    depth_or_array_layers: 1,
                },
            );
        };
        // Whole rows, then what a folded frame leaves of its last row, padded.
        let whole = pixels.len() / row;
        let (rows, rest) = pixels.split_at(whole * row);
        if whole > 0 {
            write(0, rows);
        }
        if !rest.is_empty() {
            let mut last = vec![0; row];
            last[..rest.len()].copy_from_slice(rest);
            write(whole, &last);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_cover_every_cpu_format() {
        // Every PFNC code of 8 to 32 bits a pixel the CPU decodes, and no other.
        let sizes = [
            0x0108, 0x010a, 0x010c, 0x0110, 0x0120, 0x0210, 0x0218, 0x0220,
        ];
        for format in sizes
            .into_iter()
            .flat_map(|size| (0..=0xff).map(move |id| size << 16 | id))
        {
            if cfg!(target_endian = "little") {
                assert_eq!(
                    Layout::of(format).is_some(),
                    crate::frame::convertible(format),
                    "{format:#x}"
                );
            }
        }
        for format in [
            MONO8,
            RGB8,
            0x0110_0025,
            0x0110_0011,
            0x010c_0047,
            0x0210_0032,
        ] {
            assert!(crate::frame::convertible(format), "{format:#x}");
        }
        // Packed bytes fold four to a texel into rows as wide as allowed, so
        // 1080p fits a 2048-texel limit and 4K an 8192 one.
        let packed = Layout::of(0x0218_0015).unwrap();
        assert_eq!(packed.extent((37, 23), 2048), Some((639, 1)));
        assert_eq!(packed.extent((1920, 1080), 2048), Some((2048, 760)));
        assert_eq!(packed.extent((3840, 2160), 8192), Some((8192, 760)));
        assert_eq!(packed.extent((3840, 2160), 2048), None);
        // Packed samples and YUV fold by their own bits a pixel, each row
        // ending on a whole byte.
        let mono12p = Layout::of(0x010c_0047).unwrap();
        assert_eq!((mono12p.row(37), mono12p.bytes((37, 23))), (56, 56 * 23));
        assert_eq!(mono12p.extent((37, 23), 2048), Some((322, 1)));
        assert_eq!(
            Layout::of(0x0210_001f).unwrap().bytes((37, 23)),
            37 * 2 * 23
        );
        // Others take a texel a pixel.
        let mono = Layout::of(MONO8).unwrap();
        assert_eq!(mono.extent((2048, 1536), 2048), Some((2048, 1536)));
        assert_eq!(mono.extent((2049, 1536), 2048), None);
        let rgba = Layout::of(0x0220_0016).unwrap();
        assert_eq!((rgba.folded, rgba.bytes((37, 23))), (false, 37 * 4 * 23));
        assert_eq!(std::mem::size_of::<Uniforms>(), 64);
    }

    #[test]
    fn only_bayer_near_full_size_is_decoded_once() {
        let bayer = Layout::of(0x0108_0009).unwrap();
        let mono = Layout::of(MONO8).unwrap();
        assert!(decode_once(bayer, (1920, 1080), 1920.0 * 1080.0));
        assert!(decode_once(bayer, (1920, 1080), 960.0 * 540.0));
        assert!(!decode_once(bayer, (4000, 3000), 320.0 * 240.0));
        assert!(!decode_once(mono, (1920, 1080), 1920.0 * 1080.0));
    }

    #[test]
    fn shader_validates() {
        let module = naga::front::wgsl::parse_str(SHADER).expect("parse");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        )
        .validate(&module)
        .expect("validate");
    }

    /// Renders every supported format through the real pipeline at 1:1, both
    /// decoded once and directly, and compares each pixel with
    /// `frame::convert`, including packed rows folded over several texture
    /// rows and frames shrunk for the GPU. Skipped without a GPU.
    #[test]
    fn gpu_decode_matches_cpu_conversion() {
        use shader::Pipeline as _;
        let Some((device, queue)) = device() else {
            return;
        };
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut pipeline = Pipeline::new(&device, &queue, format);
        let mut seed = 0x2545_f491_u32;
        let mut noise = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        // Zero bytes a pixel: random bytes, as many as the format takes.
        let mut frame = |pixel_format, (width, height): (u32, u32), bytes_per_pixel, mask| {
            let samples = match bytes_per_pixel {
                0 => Layout::of(pixel_format).unwrap().bytes((width, height)),
                3 => (width * height) as usize * 3,
                _ => (width * height) as usize,
            };
            let data: Vec<u8> = (0..samples)
                .flat_map(|_| {
                    let value = noise() & mask;
                    if bytes_per_pixel == 2 {
                        (value as u16).to_le_bytes().to_vec()
                    } else {
                        vec![value as u8]
                    }
                })
                .collect();
            Frame {
                width,
                height,
                pixel_format,
                data,
                ..Frame::default()
            }
        };
        let formats = [
            (MONO8, 1, 0xff),
            (0x0110_0003, 2, 0x3ff),
            (0x0110_0005, 2, 0xfff),
            (0x0110_0007, 2, 0xffff),
            (RGB8, 3, 0xff),
            (0x0218_0015, 3, 0xff),
            (0x0108_0008, 1, 0xff),
            (0x0108_0009, 1, 0xff),
            (0x0108_000a, 1, 0xff),
            (0x0108_000b, 1, 0xff),
            (0x0110_0025, 2, 0x3fff),
            (0x0110_000c, 2, 0x3ff),
            (0x0110_0011, 2, 0xfff),
            (0x0110_0031, 2, 0xffff),
            (0x0220_0016, 0, 0xff),
            (0x0220_0017, 0, 0xff),
            (0x0210_001f, 0, 0xff),
            (0x0210_0032, 0, 0xff),
            (0x0210_003b, 0, 0xff),
            (0x0210_0043, 0, 0xff),
            (0x010a_0046, 0, 0xff),
            (0x010c_0047, 0, 0xff),
            (0x010c_0004, 0, 0xff),
            (0x010c_0006, 0, 0xff),
            (0x010a_0058, 0, 0xff),
            (0x010c_0053, 0, 0xff),
            (0x010c_0026, 0, 0xff),
            (0x010c_002d, 0, 0xff),
        ];
        // Odd and even sizes exercise the edges of the Bayer neighborhood, row
        // padding, packed pixels split across texels, and half-filled groups
        // of packed samples and YUV pairs at the end of a row.
        for size in [(37, 23), (36, 23)] {
            for direct_only in [false, true] {
                for (pixel_format, bytes, mask) in formats {
                    let frame = frame(pixel_format, size, bytes, mask);
                    let bayer = Layout::of(pixel_format).unwrap().kind == BAYER;
                    let worst = render(&mut pipeline, (&device, &queue), &frame, direct_only);
                    assert_eq!(pipeline.slot("test").once, bayer && !direct_only);
                    assert!(
                        worst <= 1,
                        "{pixel_format:#010x} {size:?} (direct only: {direct_only}): off by {worst}"
                    );
                }
            }
        }
        // Packed rows wider than a third of the texture limit, folded over
        // several texture rows and a part of one.
        let max = device.limits().max_texture_dimension_2d;
        for (pixel_format, bytes) in [
            (RGB8, 3),
            (0x0218_0015, 3),
            (0x010c_0047, 0),
            (0x0210_0032, 0),
        ] {
            let wide = frame(pixel_format, (max - 3, 3), bytes, 0xff);
            assert!(GpuFrames::default().accepts(&wide));
            let worst = render(&mut pipeline, (&device, &queue), &wide, true);
            assert!(worst <= 1, "{pixel_format:#010x} folded: off by {worst}");
            let rows = Layout::of(pixel_format).unwrap().extent((max - 3, 3), max);
            assert_eq!(pipeline.slot("test").storage.1, rows.unwrap().1);
            assert!(bytes == 0 || rows.unwrap().1 == 3);
        }
        // Overview tiles shrink color and mono frames to RGB8 and Mono8.
        for (pixel_format, bytes, mask, factor) in [
            (0x0108_0009, 1, 0xff, 4),
            (0x0218_0015, 3, 0xff, 3),
            (0x0110_0005, 2, 0xfff, 2),
        ] {
            let source = frame(pixel_format, (75, 49), bytes, mask);
            let small = crate::frame::shrink(&source, factor).unwrap();
            assert!(matches!(small.pixel_format, RGB8 | MONO8));
            let worst = render(&mut pipeline, (&device, &queue), &small, false);
            assert!(worst <= 1, "{pixel_format:#010x} shrunk: off by {worst}");
        }
    }

    #[test]
    fn windows_drawing_the_same_view_keep_their_own_textures() {
        use shader::{Pipeline as _, Primitive as _};
        let Some((device, queue)) = device() else {
            return;
        };
        let mut pipeline = Pipeline::new(&device, &queue, wgpu::TextureFormat::Rgba8Unorm);
        let bounds = Rectangle::new(iced::Point::ORIGIN, iced::Size::new(8.0, 2.0));
        let viewport = Viewport::with_physical_size(iced::Size::new(8, 2), 1.0);
        let draw = |pipeline: &mut Pipeline, gpu: &GpuFrames| {
            let primitive = Primitive {
                gpu: gpu.clone(),
                key: "main".into(),
                image: bounds,
            };
            primitive.prepare(pipeline, &device, &queue, &bounds, &viewport);
        };
        let windows = [GpuFrames::default(), GpuFrames::default()];
        for (gpu, width) in windows.iter().zip([4, 8]) {
            gpu.submit(
                "main",
                Arc::new(Frame {
                    width,
                    height: 2,
                    pixel_format: MONO8,
                    data: vec![0; width as usize * 2],
                    ..Frame::default()
                }),
            );
            draw(&mut pipeline, gpu);
        }
        let sizes = |pipeline: &Pipeline| {
            let mut sizes: Vec<_> = pipeline.slots.values().map(|slot| slot.size).collect();
            sizes.sort();
            sizes
        };
        assert_eq!(sizes(&pipeline), [(4, 2), (8, 2)]);
        let [closed, open] = windows;
        drop(closed);
        draw(&mut pipeline, &open);
        assert_eq!(sizes(&pipeline), [(8, 2)]);
    }

    fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
        use futures::executor::block_on;
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let Ok(adapter) =
            block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        else {
            eprintln!("no GPU adapter; skipping");
            return None;
        };
        Some(block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).expect("device"))
    }

    impl Pipeline {
        fn slot(&self, key: &str) -> &Slot {
            self.slots
                .iter()
                .find_map(|((_, slot_key), slot)| (slot_key == key).then_some(slot))
                .expect("a slot for the key")
        }
    }

    /// Draws `frame` at 1:1 through `pipeline` and returns the largest
    /// difference from `frame::convert` in any channel.
    fn render(
        pipeline: &mut Pipeline,
        (device, queue): (&wgpu::Device, &wgpu::Queue),
        frame: &Frame,
        direct_only: bool,
    ) -> u8 {
        use shader::Primitive as _;
        let (width, height) = (frame.width, frame.height);
        let expected = crate::frame::convert(frame, |rgb| rgb).expect("cpu conversion");
        let gpu = GpuFrames::default();
        gpu.submit("test", Arc::new(frame.clone()));
        let bounds = Rectangle::new(
            iced::Point::ORIGIN,
            iced::Size::new(width as f32, height as f32),
        );
        let primitive = Primitive {
            gpu,
            key: "test".into(),
            image: bounds,
        };
        let viewport = Viewport::with_physical_size(iced::Size::new(width, height), 1.0);
        pipeline.direct_only = direct_only;
        primitive.prepare(pipeline, device, queue, &bounds, &viewport);
        // A redraw without a new frame reuses the decoded texture.
        primitive.prepare(pipeline, device, queue, &bounds, &viewport);
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let row = (width * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(row * height),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            assert!(primitive.draw(pipeline, &mut pass));
        }
        encoder.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);
        readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        let pixels = readback.slice(..).get_mapped_range();
        let mut worst = 0;
        for y in 0..height as usize {
            for x in 0..width as usize {
                let got = &pixels[y * row as usize + x * 4..][..3];
                let want = expected[y * width as usize + x];
                for c in 0..3 {
                    worst = worst.max(got[c].abs_diff(want[c]));
                }
            }
        }
        worst
    }
}
