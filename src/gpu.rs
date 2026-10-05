//! GPU presentation of camera frames for the glow (OpenGL) renderer.
//!
//! Frames are uploaded in their sensor layout as integer textures and decoded
//! in a fragment shader that reproduces `frame::convert` bit for bit, followed
//! by the same gamma-space bilinear filtering and dithering egui applies to its
//! own textures. The CPU only hands sensor bytes to the driver: no demosaic,
//! no RGBA expansion, and a quarter of the upload for 8-bit mono and Bayer.
//! Contexts without integer textures (GL < 3.0, GLES 2) and the wgpu renderer
//! keep the CPU path.
#![allow(unsafe_code)]

use crate::types::{Frame, MONO8, RGB8};
use eframe::{
    egui, egui_glow,
    glow::{self, HasContext},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
};

const VERTEX: &str = r#"
out vec2 v_uv;
void main() {
    // A viewport-filling quad from gl_VertexID; egui sets the viewport to the
    // callback rectangle.
    vec2 corner = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));
    v_uv = vec2(corner.x, 1.0 - corner.y);
    gl_Position = vec4(corner * 2.0 - 1.0, 0.0, 1.0);
}
"#;

const FRAGMENT: &str = r#"
uniform usampler2D u_frame;
uniform ivec2 u_size;
uniform int u_kind;
uniform int u_shift;
uniform int u_cfa[4];
uniform int u_dither;
in vec2 v_uv;
out vec4 f_color;

int cfa(ivec2 p) {
    return u_cfa[(p.y & 1) * 2 + (p.x & 1)];
}

// Exactly frame::convert for one pixel: unpacked mono is shifted and clamped;
// Bayer averages the in-bounds 3x3 neighbors of each missing color with
// truncating integer division.
uvec3 decode(ivec2 p) {
    uvec4 t = texelFetch(u_frame, p, 0);
    if (u_kind == 1) return t.rgb;
    if (u_kind == 2) return t.bgr;
    if (u_kind == 0) return uvec3(min(t.r >> uint(u_shift), 255u));
    uvec3 sum = uvec3(0u);
    uvec3 n = uvec3(0u);
    for (int dy = -1; dy <= 1; dy++) {
        for (int dx = -1; dx <= 1; dx++) {
            ivec2 q = p + ivec2(dx, dy);
            if (any(lessThan(q, ivec2(0))) || any(greaterThanEqual(q, u_size))) continue;
            uvec3 mask = uvec3(equal(ivec3(cfa(q)), ivec3(0, 1, 2)));
            sum += mask * texelFetch(u_frame, q, 0).r;
            n += mask;
        }
    }
    uvec3 own = uvec3(equal(ivec3(cfa(p)), ivec3(0, 1, 2)));
    uvec3 avg = uvec3(
        n.x > 0u ? sum.x / n.x : t.r,
        n.y > 0u ? sum.y / n.y : t.r,
        n.z > 0u ? sum.z / n.z : t.r);
    return own * t.r + (uvec3(1u) - own) * avg;
}

// egui_glow's dither, so GPU and CPU-texture previews quantize identically.
float interleaved_gradient_noise(vec2 n) {
    float f = 0.06711056 * n.x + 0.00583715 * n.y;
    return fract(52.9829189 * fract(f));
}

void main() {
    // GL_LINEAR with clamp-to-edge over decoded 8-bit pixels, as egui samples
    // a TextureOptions::LINEAR texture.
    vec2 pos = v_uv * vec2(u_size) - 0.5;
    vec2 base = floor(pos);
    vec2 f = pos - base;
    ivec2 i0 = ivec2(base);
    ivec2 hi = u_size - 1;
    vec3 c00 = vec3(decode(clamp(i0, ivec2(0), hi)));
    vec3 c10 = vec3(decode(clamp(i0 + ivec2(1, 0), ivec2(0), hi)));
    vec3 c01 = vec3(decode(clamp(i0 + ivec2(0, 1), ivec2(0), hi)));
    vec3 c11 = vec3(decode(clamp(i0 + ivec2(1, 1), ivec2(0), hi)));
    vec3 rgb = mix(mix(c00, c10, f.x), mix(c01, c11, f.x), f.y) / 255.0;
    if (u_dither != 0) {
        float noise = (interleaved_gradient_noise(gl_FragCoord.xy) - 0.5) * 0.95;
        rgb += noise / 255.0;
    }
    f_color = vec4(rgb, 1.0);
}
"#;

/// How a pixel format is stored on the GPU and decoded by the shader.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Layout {
    internal_format: u32,
    format: u32,
    ty: u32,
    bytes_per_pixel: usize,
    kind: i32,
    shift: i32,
    cfa: [i32; 4],
}
impl Layout {
    fn of(pixel_format: u32) -> Option<Self> {
        let gray = |internal_format, ty, bytes_per_pixel, shift| Self {
            internal_format,
            format: glow::RED_INTEGER,
            ty,
            bytes_per_pixel,
            kind: 0,
            shift,
            cfa: [0; 4],
        };
        let wide = |shift| {
            // 16-bit texels are read in host byte order; PFNC data is little-endian.
            cfg!(target_endian = "little")
                .then(|| gray(glow::R16UI, glow::UNSIGNED_SHORT, 2, shift))
        };
        let packed = |kind| Self {
            internal_format: glow::RGB8UI,
            format: glow::RGB_INTEGER,
            ty: glow::UNSIGNED_BYTE,
            bytes_per_pixel: 3,
            kind,
            shift: 0,
            cfa: [0; 4],
        };
        match pixel_format {
            MONO8 => Some(gray(glow::R8UI, glow::UNSIGNED_BYTE, 1, 0)),
            0x0110_0003 => wide(2),
            0x0110_0005 => wide(4),
            0x0110_0007 => wide(8),
            RGB8 => Some(packed(1)),
            0x0218_0015 => Some(packed(2)),
            0x0108_0008..=0x0108_000b => Some(Self {
                kind: 3,
                cfa: crate::frame::bayer_pattern(pixel_format).map(|c| c as i32),
                ..gray(glow::R8UI, glow::UNSIGNED_BYTE, 1, 0)
            }),
            _ => None,
        }
    }
}

struct Program {
    program: glow::Program,
    vao: glow::VertexArray,
    frame: Option<glow::UniformLocation>,
    size: Option<glow::UniformLocation>,
    kind: Option<glow::UniformLocation>,
    shift: Option<glow::UniformLocation>,
    cfa: Option<glow::UniformLocation>,
    dither: Option<glow::UniformLocation>,
}

struct Slot {
    texture: Option<glow::Texture>,
    /// Allocated texture storage: width, height and internal format.
    storage: Option<(i32, i32, u32)>,
    /// Newest frame awaiting upload; held by reference, never copied.
    pending: Option<Arc<Frame>>,
    layout: Option<(Layout, i32, i32)>,
}

struct State {
    program: Option<Program>,
    slots: HashMap<String, Slot>,
    /// Textures of removed slots, deleted on the next callback with a context.
    retired: Vec<glow::Texture>,
    dithering: bool,
}

/// Shared between the app (which submits frames) and paint callbacks (which
/// run on the render thread with the GL context current).
#[derive(Clone)]
pub struct GpuFrames {
    state: Arc<Mutex<State>>,
    max_side: u32,
}

impl GpuFrames {
    /// Compile the decode shader, or return why the GPU path is unavailable.
    pub fn new(gl: &glow::Context, dithering: bool) -> Result<Self, String> {
        let version = egui_glow::ShaderVersion::get(gl);
        if !version.is_new_shader_interface() {
            return Err(format!(
                "{version:?} lacks integer textures; using CPU conversion"
            ));
        }
        let prefix = format!(
            "{}{}",
            version.version_declaration(),
            if version.is_embedded() {
                "precision highp float;\nprecision highp int;\nprecision highp usampler2D;\n"
            } else {
                ""
            }
        );
        // SAFETY: called with the renderer's current GL context; every object
        // created here is owned by `Program` and deleted in `destroy`.
        unsafe {
            let program = gl.create_program()?;
            let mut shaders = Vec::new();
            for (kind, source) in [
                (glow::VERTEX_SHADER, VERTEX),
                (glow::FRAGMENT_SHADER, FRAGMENT),
            ] {
                let shader = gl.create_shader(kind)?;
                gl.shader_source(shader, &format!("{prefix}{source}"));
                gl.compile_shader(shader);
                if !gl.get_shader_compile_status(shader) {
                    let log = gl.get_shader_info_log(shader);
                    gl.delete_shader(shader);
                    gl.delete_program(program);
                    return Err(format!("compile preview shader: {log}"));
                }
                gl.attach_shader(program, shader);
                shaders.push(shader);
            }
            gl.link_program(program);
            for shader in shaders {
                gl.detach_shader(program, shader);
                gl.delete_shader(shader);
            }
            if !gl.get_program_link_status(program) {
                let log = gl.get_program_info_log(program);
                gl.delete_program(program);
                return Err(format!("link preview shader: {log}"));
            }
            let vao = match gl.create_vertex_array() {
                Ok(vao) => vao,
                Err(e) => {
                    gl.delete_program(program);
                    return Err(e);
                }
            };
            let uniform = |name| gl.get_uniform_location(program, name);
            let program = Program {
                program,
                vao,
                frame: uniform("u_frame"),
                size: uniform("u_size"),
                kind: uniform("u_kind"),
                shift: uniform("u_shift"),
                cfa: uniform("u_cfa"),
                dither: uniform("u_dither"),
            };
            let max_side = gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE).max(0) as u32;
            Ok(Self {
                state: Arc::new(Mutex::new(State {
                    program: Some(program),
                    slots: HashMap::new(),
                    retired: Vec::new(),
                    dithering,
                })),
                max_side,
            })
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether this frame can be shown without CPU conversion.
    pub fn accepts(&self, frame: &Frame) -> bool {
        Layout::of(frame.pixel_format).is_some_and(|layout| {
            frame.width > 0
                && frame.height > 0
                && frame.width <= self.max_side
                && frame.height <= self.max_side
                && frame.data.len()
                    >= frame.width as usize * frame.height as usize * layout.bytes_per_pixel
        })
    }

    /// Queue `frame` for display under `key`; it is uploaded when next painted.
    pub fn submit(&self, key: &str, frame: Arc<Frame>) {
        let mut state = self.state();
        let slot = state.slots.entry(key.to_owned()).or_insert(Slot {
            texture: None,
            storage: None,
            pending: None,
            layout: None,
        });
        slot.pending = Some(frame);
    }

    /// Drop slots no longer shown; their textures are freed on the next paint.
    pub fn retain(&self, keep: impl Fn(&str) -> bool) {
        let mut state = self.state();
        let State { slots, retired, .. } = &mut *state;
        slots.retain(|key, slot| {
            let kept = keep(key);
            if !kept {
                retired.extend(slot.texture.take());
            }
            kept
        });
    }

    /// Draw the frame shown under `key` into `rect`.
    pub fn paint(&self, painter: &egui::Painter, key: &str, rect: egui::Rect) {
        let this = self.clone();
        let key = key.to_owned();
        painter.add(egui::PaintCallback {
            rect,
            callback: Arc::new(egui_glow::CallbackFn::new(move |_, painter| {
                this.draw(painter.gl(), &key);
            })),
        });
    }

    fn draw(&self, gl: &glow::Context, key: &str) {
        let mut state = self.state();
        let State {
            program,
            slots,
            retired,
            dithering,
        } = &mut *state;
        // SAFETY: paint callbacks run on the render thread with the context
        // current; textures and the program are only used through this state.
        unsafe {
            for texture in retired.drain(..) {
                gl.delete_texture(texture);
            }
            let (Some(program), Some(slot)) = (program.as_ref(), slots.get_mut(key)) else {
                return;
            };
            if let Some(frame) = slot.pending.take()
                && let Err(e) = upload(gl, slot, &frame)
            {
                eprintln!("capturefab: GPU preview upload failed: {e}");
                return;
            }
            let (Some(texture), Some((layout, width, height))) = (slot.texture, slot.layout) else {
                return;
            };
            gl.use_program(Some(program.program));
            gl.bind_vertex_array(Some(program.vao));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.uniform_1_i32(program.frame.as_ref(), 0);
            gl.uniform_2_i32(program.size.as_ref(), width, height);
            gl.uniform_1_i32(program.kind.as_ref(), layout.kind);
            gl.uniform_1_i32(program.shift.as_ref(), layout.shift);
            gl.uniform_1_i32_slice(program.cfa.as_ref(), &layout.cfa);
            gl.uniform_1_i32(program.dither.as_ref(), i32::from(*dithering));
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            gl.bind_vertex_array(None);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.use_program(None);
        }
    }

    /// Free every GL object; call from `App::on_exit` with the context.
    pub fn destroy(&self, gl: &glow::Context) {
        let mut state = self.state();
        // SAFETY: on_exit runs with the context still current.
        unsafe {
            let retired = std::mem::take(&mut state.retired);
            for texture in retired
                .into_iter()
                .chain(state.slots.drain().filter_map(|(_, slot)| slot.texture))
            {
                gl.delete_texture(texture);
            }
            if let Some(program) = state.program.take() {
                gl.delete_vertex_array(program.vao);
                gl.delete_program(program.program);
            }
        }
    }
}

/// Upload `frame` into the slot's texture, reallocating only when the size or
/// storage format changes.
///
/// # Safety
/// The GL context must be current.
unsafe fn upload(gl: &glow::Context, slot: &mut Slot, frame: &Frame) -> Result<(), String> {
    let layout = Layout::of(frame.pixel_format).ok_or("unsupported pixel format")?;
    let (width, height) = (frame.width as i32, frame.height as i32);
    let length = frame.width as usize * frame.height as usize * layout.bytes_per_pixel;
    let pixels = frame.data.get(..length).ok_or("truncated frame")?;
    // SAFETY: guaranteed current by the caller; `pixels` covers exactly
    // width * height texels of `layout` with unpack alignment 1.
    unsafe {
        let texture = match slot.texture {
            Some(texture) => texture,
            None => {
                let texture = gl.create_texture()?;
                slot.texture = Some(texture);
                texture
            }
        };
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
        let storage = (width, height, layout.internal_format);
        if slot.storage != Some(storage) {
            // Integer textures cannot be filtered; the shader interpolates.
            for (parameter, value) in [
                (glow::TEXTURE_MIN_FILTER, glow::NEAREST),
                (glow::TEXTURE_MAG_FILTER, glow::NEAREST),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
            ] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, value as i32);
            }
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                layout.internal_format as i32,
                width,
                height,
                0,
                layout.format,
                layout.ty,
                glow::PixelUnpackData::Slice(Some(pixels)),
            );
            slot.storage = Some(storage);
        } else {
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                width,
                height,
                layout.format,
                layout.ty,
                glow::PixelUnpackData::Slice(Some(pixels)),
            );
        }
        gl.bind_texture(glow::TEXTURE_2D, None);
    }
    slot.layout = Some((layout, width, height));
    Ok(())
}
