//! The native workbench. Camera I/O lives on the session worker, never the UI thread.
use crate::{
    auto::{AutoChange, AutoStatus},
    frame,
    genicam::FeatureInfo,
    gpu::GpuFrames,
    session::{SessionCommand, SessionHandle, SessionSnapshot},
    storage::StoragePolicy,
    types::{Frame, MONO8, RGB8, TransportStats},
};
use anyhow::Result;
use eframe::egui::{self, Color32, RichText, Stroke, Vec2};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, TryRecvError},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const AQUA: Color32 = Color32::from_rgb(77, 218, 202);
const MUTED: Color32 = Color32::from_rgb(139, 157, 177);

pub fn run(
    handle: SessionHandle,
    session_label: String,
    simulated: bool,
    use_wgpu: bool,
) -> Result<()> {
    run_capture(handle, session_label, simulated, use_wgpu, None, 0)
}

pub fn run_capture(
    handle: SessionHandle,
    session_label: String,
    simulated: bool,
    use_wgpu: bool,
    screenshot: Option<PathBuf>,
    demo_cameras: u32,
) -> Result<()> {
    let outcome: Arc<Mutex<Option<String>>> =
        Arc::new(Mutex::new(screenshot.as_ref().map(|_| {
            "Window closed before the renderer screenshot was saved".into()
        })));
    let app_outcome = outcome.clone();
    let screenshot_size = if screenshot.is_some() {
        std::env::var("CAPTUREFAB_SCREENSHOT_SIZE")
            .ok()
            .map(|size| -> Result<[f32; 2]> {
                let (width, height) = size.split_once('x').ok_or_else(|| {
                    anyhow::anyhow!("CAPTUREFAB_SCREENSHOT_SIZE must be WIDTHxHEIGHT")
                })?;
                let width: u32 = width.parse()?;
                let height: u32 = height.parse()?;
                anyhow::ensure!(
                    (900..=8192).contains(&width) && (620..=8192).contains(&height),
                    "screenshot size must be at least 900x620 and at most 8192x8192"
                );
                Ok([width as f32, height as f32])
            })
            .transpose()?
    } else {
        None
    };
    #[cfg(feature = "wgpu")]
    let renderer = if use_wgpu {
        eframe::Renderer::Wgpu
    } else {
        eframe::Renderer::Glow
    };
    #[cfg(not(feature = "wgpu"))]
    let renderer = eframe::Renderer::Glow;
    let options = eframe::NativeOptions {
        renderer,
        viewport: egui::ViewportBuilder::default()
            .with_title("Capturefab — camera workbench")
            .with_inner_size(screenshot_size.unwrap_or([1280.0, 840.0]))
            .with_min_inner_size([900.0, 620.0]),
        ..Default::default()
    };
    #[cfg(not(feature = "wgpu"))]
    let _ = use_wgpu;
    let dithering = options.dithering;
    eframe::run_native(
        "Capturefab",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::ThemePreference::System);
            configure_theme(&cc.egui_ctx, false);
            configure_theme(&cc.egui_ctx, true);
            configure_fonts(&cc.egui_ctx);
            // ⌘+ / ⌘− / ⌘0 zoom the camera image, as in image viewers, not the whole interface.
            cc.egui_ctx
                .options_mut(|options| options.zoom_with_keyboard = false);
            let capturing = screenshot.is_some();
            let mut app = Workbench::new(handle, session_label, simulated || capturing);
            // CAPTUREFAB_GPU_PREVIEW=0 keeps CPU conversion, e.g. to rule out a driver.
            if std::env::var_os("CAPTUREFAB_GPU_PREVIEW").is_none_or(|value| value != "0")
                && let Some(gl) = cc.gl.as_ref()
            {
                match GpuFrames::new(gl, dithering) {
                    Ok(gpu) => app.gpu = Some(gpu),
                    Err(error) => eprintln!("capturefab: GPU preview unavailable: {error}"),
                }
            }
            app.dark = cc.egui_ctx.theme() == egui::Theme::Dark;
            if let Some(path) = screenshot {
                let count = demo_cameras.clamp(1, 16);
                app.screenshot = Some(ScreenshotRequest {
                    path,
                    cameras: count,
                    started: Instant::now(),
                    streams_started: None,
                    requested: false,
                    save: None,
                    outcome: app_outcome,
                });
                for index in 0..count {
                    app.send(
                        "Connecting demo camera",
                        SessionCommand::Connect {
                            camera: format!("sim:{index}"),
                            timeout_ms: 2000,
                        },
                    );
                }
            }
            if !capturing {
                app.send(
                    "Discovering cameras",
                    SessionCommand::Discover {
                        timeout_ms: 700,
                        simulated,
                    },
                );
            }
            Ok(Box::new(app))
        }),
    )
    .map_err(|err| anyhow::anyhow!("Could not start the native window: {err}"))?;
    if let Some(error) = outcome
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
    {
        anyhow::bail!(error);
    }
    Ok(())
}

fn configure_theme(ctx: &egui::Context, dark: bool) {
    let theme = if dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    };
    let mut style = (*ctx.style_of(theme)).clone();
    style.visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    if dark {
        style.visuals.weak_text_color = Some(MUTED);
        style.visuals.panel_fill = Color32::from_rgb(16, 24, 37);
        style.visuals.window_fill = Color32::from_rgb(21, 31, 47);
        style.visuals.extreme_bg_color = Color32::from_rgb(9, 16, 27);
        style.visuals.faint_bg_color = Color32::from_rgb(24, 36, 51);
        style.visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(24, 36, 51);
        style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(31, 44, 60);
        style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(40, 61, 78);
        style.visuals.widgets.active.bg_fill = Color32::from_rgb(45, 78, 89);
        style.visuals.selection.bg_fill = Color32::from_rgb(34, 82, 85);
        style.visuals.selection.stroke = Stroke::new(1.0_f32, AQUA);
        style.visuals.hyperlink_color = AQUA;
    } else {
        style.visuals.weak_text_color = Some(Color32::from_rgb(82, 99, 116));
    }
    style.spacing.item_spacing = Vec2::new(9.0, 8.0);
    style.spacing.button_padding = Vec2::new(12.0, 7.0);
    style.spacing.interact_size.y = 31.0;
    style
        .text_styles
        .insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, egui::FontId::proportional(13.0));
    style
        .text_styles
        .insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
    style
        .text_styles
        .insert(egui::TextStyle::Monospace, egui::FontId::monospace(12.0));
    style
        .text_styles
        .insert(egui::TextStyle::Heading, egui::FontId::proportional(20.0));
    ctx.set_style_of(theme, style);
}

fn configure_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    #[cfg(target_os = "macos")]
    let candidates = [
        (
            egui::FontFamily::Proportional,
            vec![PathBuf::from("/System/Library/Fonts/SFNS.ttf")],
        ),
        (
            egui::FontFamily::Monospace,
            vec![
                PathBuf::from("/System/Library/Fonts/SFNSMono.ttf"),
                PathBuf::from("/System/Library/Fonts/Menlo.ttc"),
            ],
        ),
    ];
    #[cfg(target_os = "windows")]
    let candidates = {
        let windows =
            PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into()))
                .join("Fonts");
        [
            (
                egui::FontFamily::Proportional,
                vec![windows.join("segoeui.ttf")],
            ),
            (
                egui::FontFamily::Monospace,
                vec![windows.join("consola.ttf")],
            ),
        ]
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let candidates = [
        (
            egui::FontFamily::Proportional,
            vec![
                PathBuf::from("/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf"),
                PathBuf::from("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"),
            ],
        ),
        (
            egui::FontFamily::Monospace,
            vec![PathBuf::from(
                "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            )],
        ),
    ];
    for (family, paths) in candidates {
        for path in paths {
            if let Ok(bytes) = std::fs::read(&path) {
                let name = format!("platform-{family:?}");
                fonts
                    .font_data
                    .insert(name.clone(), Arc::new(egui::FontData::from_owned(bytes)));
                fonts
                    .families
                    .entry(family.clone())
                    .or_default()
                    .insert(0, name);
                break;
            }
        }
    }
    ctx.set_fonts(fonts);
}

struct ScreenshotRequest {
    path: PathBuf,
    cameras: u32,
    started: Instant,
    streams_started: Option<Instant>,
    requested: bool,
    save: Option<Receiver<Result<()>>>,
    outcome: Arc<Mutex<Option<String>>>,
}

struct Pending {
    label: String,
    receiver: Receiver<anyhow::Result<serde_json::Value>>,
}

/// A displayed frame: a CPU-converted egui texture, or sensor bytes the GPU
/// decodes while painting.
enum Shown {
    Texture(egui::TextureHandle),
    Gpu { key: String, size: Vec2 },
}
impl Shown {
    fn size(&self) -> Vec2 {
        match self {
            Self::Texture(texture) => texture.size_vec2(),
            Self::Gpu { size, .. } => *size,
        }
    }
    fn paint(&self, painter: &egui::Painter, gpu: Option<&GpuFrames>, rect: egui::Rect) {
        match self {
            Self::Texture(texture) => {
                painter.image(
                    texture.id(),
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            Self::Gpu { key, .. } => {
                if let Some(gpu) = gpu {
                    gpu.paint(painter, key, rect);
                }
            }
        }
    }
}
/// GPU slot of the single-camera view; camera tiles use their camera IDs.
const MAIN_VIEW: &str = "\0main";
/// Show `frame` under `key`, decoded by the GPU when the renderer supports its
/// format, otherwise converted on the CPU by `convert` into an egui texture.
fn present(
    ctx: &egui::Context,
    gpu: Option<&GpuFrames>,
    shown: &mut Option<Shown>,
    key: &str,
    frame: Arc<Frame>,
    convert: impl FnOnce(&Frame) -> Result<egui::ColorImage>,
) -> Result<()> {
    if let Some(gpu) = gpu.filter(|gpu| gpu.accepts(&frame)) {
        let size = Vec2::new(frame.width as f32, frame.height as f32);
        gpu.submit(key, frame);
        *shown = Some(Shown::Gpu {
            key: key.to_owned(),
            size,
        });
        return Ok(());
    }
    let image = convert(&frame)?;
    match shown {
        Some(Shown::Texture(texture)) => texture.set(image, egui::TextureOptions::LINEAR),
        _ => {
            *shown = Some(Shown::Texture(ctx.load_texture(
                format!("camera {key}"),
                image,
                egui::TextureOptions::LINEAR,
            )))
        }
    }
    Ok(())
}
#[derive(Default)]
struct CameraPreview {
    frame_id: Option<u64>,
    shown: Option<Shown>,
    meta: Option<(u64, u32, u32, u32)>,
    error: Option<String>,
}

struct Workbench {
    handle: SessionHandle,
    session: String,
    observed_camera: Option<String>,
    include_simulator: bool,
    selected: Option<String>,
    address: String,
    search: String,
    inspector_tab: usize,
    edits: HashMap<String, String>,
    edit_sources: HashMap<String, String>,
    output: String,
    count: u32,
    format: String,
    timeout_ms: u64,
    forward_output: String,
    forward_codec: String,
    forward_encoder: String,
    forward_fps: f64,
    forward_bitrate: String,
    forward_file_mib: f64,
    quota_gib: f64,
    quota_files: u32,
    retention_enabled: bool,
    retention_days: u64,
    quota_action: String,
    schedule_enabled: bool,
    schedule_delay_seconds: u64,
    schedule_interval_seconds: f64,
    balance: f64,
    dark: bool,
    theme_preference: egui::ThemePreference,
    screenshot: Option<ScreenshotRequest>,
    logs_open: bool,
    help_open: bool,
    fit: bool,
    zoom: f32,
    /// Scale "Fit" used last frame, so zooming from Fit starts where the image already is.
    fit_scale: f32,
    /// Whether a widget held keyboard focus at the end of last frame. egui drops focus on
    /// Escape before `update` runs, so this keeps that Escape from also acting app-wide.
    focus_held: bool,
    histogram_open: bool,
    focus_camera: bool,
    histogram: [u32; 64],
    shown: Option<Shown>,
    /// GPU decoding for the glow renderer; None means CPU conversion.
    gpu: Option<GpuFrames>,
    frame_meta: Option<(u64, u32, u32, u32, u64)>,
    frame_id: Option<u64>,
    display_error: Option<String>,
    previews: HashMap<String, CameraPreview>,
    pending: Vec<Pending>,
    notice: Option<(String, bool, Instant)>,
}

impl Workbench {
    fn new(handle: SessionHandle, session: String, simulated: bool) -> Self {
        Self {
            handle,
            session,
            observed_camera: None,
            include_simulator: simulated,
            selected: None,
            address: String::new(),
            search: String::new(),
            inspector_tab: 0,
            edits: HashMap::new(),
            edit_sources: HashMap::new(),
            output: "capture.png".into(),
            count: 1,
            format: "png".into(),
            timeout_ms: 5000,
            forward_output: "rtsp://127.0.0.1:8554/capturefab".into(),
            forward_codec: "h264".into(),
            forward_encoder: "auto".into(),
            forward_fps: 30.0,
            forward_bitrate: "4M".into(),
            forward_file_mib: 512.0,
            quota_gib: 10.0,
            quota_files: 10_000,
            retention_enabled: false,
            retention_days: 7,
            quota_action: "stop".into(),
            schedule_enabled: false,
            schedule_delay_seconds: 5,
            schedule_interval_seconds: 60.0,
            balance: crate::auto::DEFAULT_BALANCE,
            dark: true,
            theme_preference: egui::ThemePreference::System,
            screenshot: None,
            logs_open: false,
            help_open: false,
            fit: true,
            zoom: 1.0,
            fit_scale: 1.0,
            focus_held: false,
            histogram_open: false,
            focus_camera: false,
            histogram: [0; 64],
            shown: None,
            gpu: None,
            frame_meta: None,
            frame_id: None,
            display_error: None,
            previews: HashMap::new(),
            pending: Vec::new(),
            notice: None,
        }
    }

    fn send(&mut self, label: impl Into<String>, command: SessionCommand) {
        let label = label.into();
        match self.handle.submit(command) {
            Ok(receiver) => {
                self.notice = None;
                self.pending.push(Pending { label, receiver });
            }
            Err(err) => self.notice = Some((err.to_string(), true, Instant::now())),
        }
    }

    fn send_to(&mut self, camera: &str, label: impl Into<String>, command: SessionCommand) {
        match self.handle.submit_to(camera, command) {
            Ok(receiver) => {
                self.notice = None;
                self.pending.push(Pending {
                    label: label.into(),
                    receiver,
                });
            }
            Err(error) => self.notice = Some((error.to_string(), true, Instant::now())),
        }
    }

    fn update_previews(&mut self, ctx: &egui::Context, snapshot: &SessionSnapshot) {
        self.previews
            .retain(|id, _| snapshot.cameras.iter().any(|camera| &camera.info.id == id));
        for camera in &snapshot.cameras {
            let Some(id) = self.handle.latest_frame_id_for(&camera.info.id) else {
                continue;
            };
            let preview = self.previews.entry(camera.info.id.clone()).or_default();
            if preview.frame_id == Some(id) {
                continue;
            }
            let Some(frame) = self.handle.latest_frame_for(&camera.info.id) else {
                continue;
            };
            preview.frame_id = Some(frame.id);
            let meta = (frame.id, frame.width, frame.height, frame.pixel_format);
            let shown = present(
                ctx,
                self.gpu.as_ref(),
                &mut preview.shown,
                &camera.info.id,
                frame,
                |frame| {
                    let (width, height, rgba) = frame::preview_rgba(frame, 640, 480)?;
                    Ok(egui::ColorImage::from_rgba_unmultiplied(
                        [width as usize, height as usize],
                        &rgba,
                    ))
                },
            );
            match shown {
                Ok(()) => {
                    preview.meta = Some(meta);
                    preview.error = None;
                }
                Err(error) => preview.error = Some(error.to_string()),
            }
        }
    }

    fn poll(&mut self) {
        let mut i = 0;
        while i < self.pending.len() {
            match self.pending[i].receiver.try_recv() {
                Ok(result) => {
                    let pending = self.pending.swap_remove(i);
                    self.notice = Some(match result {
                        Ok(_) => (format!("{} · done", pending.label), false, Instant::now()),
                        Err(err) => (format!("{}: {err:#}", pending.label), true, Instant::now()),
                    });
                }
                Err(TryRecvError::Disconnected) => {
                    self.pending.swap_remove(i);
                    self.notice =
                        Some(("Session worker disconnected".into(), true, Instant::now()));
                }
                Err(TryRecvError::Empty) => i += 1,
            }
        }
        if self
            .notice
            .as_ref()
            .is_some_and(|(_, error, at)| !*error && at.elapsed() > Duration::from_secs(5))
        {
            self.notice = None;
        }
    }

    fn update_frame(&mut self, ctx: &egui::Context) {
        let Some(id) = self.handle.latest_frame_id() else {
            return;
        };
        if Some(id) == self.frame_id {
            return;
        }
        let Some(frame) = self.handle.latest_frame() else {
            return;
        };
        self.frame_id = Some(frame.id);
        let meta = (
            frame.id,
            frame.width,
            frame.height,
            frame.pixel_format,
            frame.timestamp_ns,
        );
        let shown = frame::sampled_histogram(&frame).and_then(|histogram| {
            present(
                ctx,
                self.gpu.as_ref(),
                &mut self.shown,
                MAIN_VIEW,
                frame,
                |frame| {
                    // Decode straight into egui's pixel type: one pass, one allocation.
                    let pixels = frame::convert(frame, |[r, g, b]| Color32::from_rgb(r, g, b))?;
                    let size = [frame.width as usize, frame.height as usize];
                    Ok(egui::ColorImage::new(size, pixels))
                },
            )?;
            Ok(histogram)
        });
        match shown {
            Ok(histogram) => {
                self.histogram = histogram;
                self.frame_meta = Some(meta);
                self.display_error = None;
            }
            Err(err) => self.display_error = Some(err.to_string()),
        }
    }

    fn discover(&mut self) {
        self.send(
            "Discovering cameras",
            SessionCommand::Discover {
                timeout_ms: 700,
                simulated: self.include_simulator,
            },
        );
    }

    fn toggle_stream(&mut self, snapshot: &SessionSnapshot) {
        if self
            .pending
            .iter()
            .any(|p| p.label == "Starting stream" || p.label == "Stopping stream")
        {
            return;
        }
        self.send(
            if snapshot.streaming {
                "Stopping stream"
            } else {
                "Starting stream"
            },
            if snapshot.streaming {
                SessionCommand::Stop
            } else {
                SessionCommand::Start
            },
        );
    }

    fn capture(&mut self) {
        if self.pending.iter().any(|p| p.label == "Saving capture") {
            return;
        }
        self.send(
            "Saving capture",
            SessionCommand::Capture {
                output: self.output.clone(),
                count: self.count,
                timeout_ms: self.timeout_ms,
                format: self.format.clone(),
                storage: self.storage_policy(),
            },
        );
    }

    fn top_bar(&mut self, ctx: &egui::Context, snapshot: &SessionSnapshot) {
        egui::TopBottomPanel::top("header")
            .exact_height(64.0)
            .show(ctx, |ui| {
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.add_space(9.0);
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(27.0), egui::Sense::hover());
                    let p = ui.painter();
                    p.rect_filled(rect.shrink(1.0), 7.0, AQUA);
                    p.circle_stroke(
                        rect.center(),
                        7.0,
                        Stroke::new(2.0_f32, Color32::from_rgb(16, 37, 46)),
                    );
                    p.circle_filled(rect.center(), 2.0, Color32::from_rgb(16, 37, 46));
                    ui.label(RichText::new("capturefab").size(22.0).strong());
                    ui.add_space(5.0);
                    ui.label(
                        RichText::new("CAMERA WORKBENCH")
                            .size(10.0)
                            .color(ui.visuals().weak_text_color()),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button("Help")
                            .on_hover_text(with_shortcut(
                                ctx,
                                "Keyboard shortcuts and quick guide",
                                Action::Help,
                            ))
                            .clicked()
                        {
                            self.help_open = true;
                        }
                        let previous_theme = self.theme_preference;
                        egui::ComboBox::from_id_salt("color-theme")
                            .selected_text(match self.theme_preference {
                                egui::ThemePreference::System => "System",
                                egui::ThemePreference::Light => "Light",
                                egui::ThemePreference::Dark => "Dark",
                            })
                            .width(72.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.theme_preference,
                                    egui::ThemePreference::System,
                                    "System",
                                );
                                ui.selectable_value(
                                    &mut self.theme_preference,
                                    egui::ThemePreference::Light,
                                    "Light",
                                );
                                ui.selectable_value(
                                    &mut self.theme_preference,
                                    egui::ThemePreference::Dark,
                                    "Dark",
                                );
                            });
                        if previous_theme != self.theme_preference {
                            ctx.set_theme(self.theme_preference);
                            self.dark = ctx.theme() == egui::Theme::Dark;
                        }
                        if ui
                            .button("Copy CLI")
                            .on_hover_text(with_shortcut(
                                ctx,
                                "Copy a command that controls this visible session",
                                Action::CopySessionCommand,
                            ))
                            .clicked()
                        {
                            self.copy_session_command(ctx);
                        }
                        ui.label(
                            RichText::new(format!("session  {}", self.session))
                                .monospace()
                                .color(ui.visuals().weak_text_color()),
                        );
                        status_dot(
                            ui,
                            if snapshot.streaming {
                                AQUA
                            } else if snapshot.connected.is_some() {
                                Color32::from_rgb(142, 194, 133)
                            } else {
                                MUTED
                            },
                        );
                    });
                });
            });
    }

    fn devices(&mut self, ctx: &egui::Context, snapshot: &SessionSnapshot) {
        let mut devices = snapshot.devices.clone();
        for camera in &snapshot.cameras {
            if !devices.iter().any(|device| device.id == camera.info.id) {
                devices.push(camera.info.clone());
            }
        }
        egui::SidePanel::left("devices").default_width(225.0).min_width(185.0).max_width(300.0).show(ctx, |ui| {
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Cameras").size(17.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(devices.len().to_string()).color(ui.visuals().weak_text_color()));
                });
            });
            ui.add_space(4.0);
            let discovering = self.pending.iter().any(|p| p.label == "Discovering cameras");
            if ui.add_enabled(!discovering, egui::Button::new(if discovering { "Discovering…" } else { "Discover cameras" }).min_size(Vec2::new(ui.available_width(), 34.0)))
                .on_hover_text(with_shortcut(ui.ctx(), "Find GigE Vision and USB3 Vision devices", Action::Discover)).clicked() { self.discover(); }
            ui.checkbox(&mut self.include_simulator, "Include simulated camera");
            ui.add_space(6.0);
            ui.separator();
            let bottom = 168.0;
            egui::ScrollArea::vertical().id_salt("camera-list").max_height((ui.available_height() - bottom).max(100.0)).show(ui, |ui| {
                if devices.is_empty() {
                    ui.add_space(18.0);
                    ui.label(RichText::new("No cameras found").strong());
                    ui.label(RichText::new("Connect a camera, then discover. Use the simulator to explore the workbench.").small().color(ui.visuals().weak_text_color()));
                }
                for camera in &devices {
                    let active = snapshot.active_camera.as_ref() == Some(&camera.id);
                    let connected = snapshot.cameras.iter().any(|c| c.info.id == camera.id);
                    let selected = active;
                    let fill = if selected { ui.visuals().selection.bg_fill } else { ui.visuals().faint_bg_color };
                    egui::Frame::new().fill(fill).corner_radius(7.0).inner_margin(10.0).show(ui, |ui| {
                        ui.set_min_width((ui.available_width() - 2.0).max(10.0));
                        ui.horizontal(|ui| {
                            status_dot(ui, if connected { AQUA } else { MUTED });
                            ui.label(RichText::new(&camera.model).strong());
                        });
                        ui.label(RichText::new(format!("{} · {}", camera.vendor, camera.transport)).small().color(ui.visuals().weak_text_color()));
                        if let Some(address) = &camera.address { ui.add(egui::Label::new(RichText::new(redact_address(address)).monospace().small()).truncate()); }
                        else { ui.label(RichText::new(format!("S/N {}", camera.serial)).monospace().small()); }
                        ui.add_space(2.0);
                        if connected {
                            ui.horizontal(|ui| {
                                if active { ui.label(RichText::new("Selected").small().color(accent(ui))); }
                                else if ui.small_button("Select").clicked() { self.send("Selecting camera", SessionCommand::Select { camera: camera.id.clone() }); }
                                if ui.small_button("Disconnect").clicked() { self.send_to(&camera.id, "Disconnecting", SessionCommand::Disconnect); }
                            });
                        } else if ui.button("Connect").clicked() {
                            self.selected = Some(camera.id.clone());
                            self.edits.clear();
                            self.edit_sources.clear();
                            self.send("Connecting camera", SessionCommand::Connect { camera: camera.id.clone(), timeout_ms: 5000 });
                        }
                    });
                    ui.add_space(5.0);
                }
            });
            ui.add_space(12.0);
            ui.separator();
            ui.add_space(4.0);
            ui.label(RichText::new("Add a camera or stream").strong());
            let address = ui.add(egui::TextEdit::singleline(&mut self.address).id(egui::Id::new("connect-address")).hint_text("Camera IP, stream or native URI").desired_width(f32::INFINITY))
                .on_hover_text(with_shortcut(ui.ctx(), "Press Enter to connect", Action::ConnectAddress));
            let submitted = address.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.add_enabled(!self.address.trim().is_empty(), egui::Button::new("Connect directly").min_size(Vec2::new(ui.available_width(), 31.0))).clicked() || submitted) && !self.address.trim().is_empty() {
                self.edits.clear();
                self.edit_sources.clear();
                self.send("Connecting camera", SessionCommand::Connect { camera: self.address.trim().to_owned(), timeout_ms: 5000 });
            }
            ui.label(RichText::new("GigE · USB3 · RTSP · HTTP · RTMP · native").small().color(ui.visuals().weak_text_color()));
        });
    }

    fn inspector(&mut self, ctx: &egui::Context, snapshot: &SessionSnapshot) {
        egui::SidePanel::right("inspector").default_width(300.0).min_width(245.0).max_width(440.0).show(ctx, |ui| {
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                for (tab, name, action) in [(0, "Features", Action::FeaturesTab), (1, "Capture", Action::CaptureTab), (2, "Forward", Action::ForwardTab)] {
                    ui.selectable_value(&mut self.inspector_tab, tab, name).on_hover_text(with_shortcut(ui.ctx(), action.label(), action));
                }
            });
            ui.add_space(5.0);
            ui.separator();
            if self.inspector_tab == 1 { egui::ScrollArea::vertical().id_salt("capture-settings-scroll").show(ui, |ui| self.capture_settings(ui, snapshot)); return; }
            if self.inspector_tab == 2 { egui::ScrollArea::vertical().id_salt("forward-settings-scroll").show(ui, |ui| self.forward_settings(ui, snapshot)); return; }
            if snapshot.connected.is_some() { self.auto_card(ui, snapshot); }
            ui.add(egui::TextEdit::singleline(&mut self.search).id(egui::Id::new("feature-search")).hint_text("Search features…").desired_width(f32::INFINITY))
                .on_hover_text(with_shortcut(ui.ctx(), "Filter by name", Action::SearchFeatures));
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} features", snapshot.features.len())).small().color(ui.visuals().weak_text_color()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(snapshot.connected.is_some(), egui::Button::new("Refresh")).clicked() {
                        self.edits.clear();
                        self.edit_sources.clear();
                        self.send("Refreshing features", SessionCommand::Features);
                    }
                });
            });
            ui.separator();
            if snapshot.connected.is_none() {
                ui.add_space(22.0);
                ui.label(RichText::new("Camera settings").strong());
                ui.label(RichText::new("Connect a camera to inspect its GenICam features, configure acquisition, and run commands.").color(ui.visuals().weak_text_color()));
                return;
            }
            let query = self.search.to_lowercase();
            let groups = ["Image", "Acquisition", "Device", "Transport", "Other"];
            egui::ScrollArea::vertical().id_salt("features").auto_shrink([false, false]).show(ui, |ui| {
                let mut visible = 0;
                for group in groups {
                    let features: Vec<_> = snapshot.features.iter().filter(|f| {
                        feature_group(&f.name) == group && (query.is_empty() || f.name.to_lowercase().contains(&query) || f.display_name.to_lowercase().contains(&query))
                    }).collect();
                    if features.is_empty() { continue; }
                    visible += features.len();
                    egui::CollapsingHeader::new(RichText::new(group).strong()).default_open(true).show(ui, |ui| {
                        for feature in features { self.feature(ui, feature, snapshot.streaming, snapshot.auto.as_ref().is_some_and(|auto| auto.managed.contains(&feature.name))); }
                    });
                    ui.add_space(5.0);
                }
                if visible == 0 { ui.label(RichText::new("No matching features").color(ui.visuals().weak_text_color())); }
            });
        });
    }

    fn auto_card(&mut self, ui: &mut egui::Ui, snapshot: &SessionSnapshot) {
        let busy = self.pending.iter().any(|p| {
            matches!(
                p.label.as_str(),
                "Enabling auto mode" | "Switching to manual" | "Updating auto balance"
            )
        });
        let auto = snapshot.auto.as_ref();
        ui.add_space(8.0);
        egui::Frame::new()
            .fill(ui.visuals().faint_bg_color)
            .corner_radius(7.0)
            .inner_margin(10.0)
            .show(ui, |ui| {
                ui.set_min_width((ui.available_width() - 2.0).max(10.0));
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!busy, egui::Button::selectable(auto.is_none(), "Manual"))
                        .on_hover_text(with_shortcut(
                            ui.ctx(),
                            "Toggle auto / manual",
                            Action::ToggleAuto,
                        ))
                        .clicked()
                        && auto.is_some()
                    {
                        self.send(
                            "Switching to manual",
                            SessionCommand::Manual { revert: false },
                        );
                    }
                    if ui
                        .add_enabled(!busy, egui::Button::selectable(auto.is_some(), "Auto"))
                        .on_hover_text(with_shortcut(
                            ui.ctx(),
                            "Toggle auto / manual",
                            Action::ToggleAuto,
                        ))
                        .clicked()
                        && auto.is_none()
                    {
                        self.send(
                            "Enabling auto mode",
                            SessionCommand::Auto {
                                balance: Some(self.balance),
                            },
                        );
                    }
                    if let Some(status) = auto {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new(&status.state).small().color(
                                match status.state.as_str() {
                                    "stable" => accent(ui),
                                    "limited" => ui.visuals().warn_fg_color,
                                    _ => ui.visuals().weak_text_color(),
                                },
                            ));
                        });
                    }
                });
                let Some(status) = auto else {
                    ui.label(
                        RichText::new("Auto tunes exposure, gain and frame rate for this camera.")
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                    return;
                };
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Quality").small());
                    ui.spacing_mut().slider_width = (ui.available_width() - 70.0).max(40.0);
                    let response =
                        ui.add(egui::Slider::new(&mut self.balance, 0.0..=1.0).show_value(false));
                    ui.label(RichText::new("Frame rate").small());
                    if (response.drag_stopped() || (response.changed() && !response.dragged()))
                        && self.balance != status.balance
                    {
                        self.send(
                            "Updating auto balance",
                            SessionCommand::Auto {
                                balance: Some(self.balance),
                            },
                        );
                    } else if !response.dragged() && !busy {
                        self.balance = status.balance;
                    }
                });
                ui.label(
                    RichText::new(auto_summary(status))
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
                for note in &status.notes {
                    ui.label(
                        RichText::new(note)
                            .small()
                            .color(ui.visuals().warn_fg_color),
                    );
                }
                if !status.changes.is_empty() {
                    egui::CollapsingHeader::new(format!("Auto changes ({})", status.changes.len()))
                        .id_salt("auto-changes")
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .id_salt("auto-changes-list")
                                .max_height(160.0)
                                .show(ui, |ui| {
                                    for change in status.changes.iter().rev() {
                                        let unit = snapshot
                                            .features
                                            .iter()
                                            .find(|f| f.name == change.feature)
                                            .and_then(|f| f.unit.as_deref());
                                        ui.label(
                                            RichText::new(change_text(change, unit))
                                                .monospace()
                                                .size(11.0),
                                        )
                                        .on_hover_text(&change.reason);
                                    }
                                });
                        });
                }
            });
        ui.add_space(6.0);
    }

    fn feature(
        &mut self,
        ui: &mut egui::Ui,
        feature: &FeatureInfo,
        streaming: bool,
        managed: bool,
    ) {
        let locked = streaming
            && matches!(
                feature.name.as_str(),
                "Width"
                    | "Height"
                    | "OffsetX"
                    | "OffsetY"
                    | "PixelFormat"
                    | "BinningHorizontal"
                    | "BinningVertical"
                    | "DecimationHorizontal"
                    | "DecimationVertical"
                    | "VideoMode"
            );
        let writable = (feature.writable || managed) && !locked;
        ui.push_id(&feature.name, |ui| {
            let name = if feature.display_name.is_empty() {
                &feature.name
            } else {
                &feature.display_name
            };
            ui.horizontal(|ui| {
                let label = ui.label(RichText::new(name).size(13.0));
                label.on_hover_text(format!("{}\n{}", feature.name, feature.description));
                if managed && !locked {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new("auto").size(10.0).color(accent(ui)))
                            .on_hover_text(
                                "Managed by auto mode. Changing it switches this camera to manual.",
                            );
                    });
                } else if !writable && !feature.kind.eq_ignore_ascii_case("command") {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(if locked {
                                "stop stream to edit"
                            } else {
                                "read only"
                            })
                            .size(10.0)
                            .color(ui.visuals().weak_text_color()),
                        );
                    });
                }
            });
            let kind = feature.kind.to_lowercase();
            if let Some(error) = &feature.error
                && kind != "command"
            {
                ui.label(
                    RichText::new(error)
                        .small()
                        .color(ui.visuals().warn_fg_color),
                );
                ui.add_space(5.0);
                return;
            }
            let current = feature
                .value
                .as_ref()
                .map(value_text)
                .unwrap_or_else(|| "—".into());
            if kind == "command" {
                let enabled = feature.writable
                    && match feature.name.as_str() {
                        "AcquisitionStart" => !streaming,
                        "AcquisitionStop" => streaming,
                        _ => true,
                    };
                let label = match feature.name.as_str() {
                    "AcquisitionStart" => "Start stream",
                    "AcquisitionStop" => "Stop stream",
                    _ => "Execute",
                };
                if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                    self.send(
                        format!("Executing {}", feature.name),
                        match feature.name.as_str() {
                            "AcquisitionStart" => SessionCommand::Start,
                            "AcquisitionStop" => SessionCommand::Stop,
                            _ => SessionCommand::Execute {
                                feature: feature.name.clone(),
                            },
                        },
                    );
                }
            } else if !writable {
                ui.label(
                    RichText::new(format!(
                        "{}{}",
                        current,
                        feature
                            .unit
                            .as_ref()
                            .map(|u| format!(" {u}"))
                            .unwrap_or_default()
                    ))
                    .monospace()
                    .color(ui.visuals().weak_text_color()),
                );
            } else if kind == "boolean" || kind == "bool" {
                let mut checked = feature
                    .value
                    .as_ref()
                    .and_then(|v| v.as_bool())
                    .unwrap_or(current == "true" || current == "1");
                let label = if checked { "Enabled" } else { "Disabled" };
                if ui.checkbox(&mut checked, label).changed() {
                    self.send(
                        format!("Setting {}", feature.name),
                        SessionCommand::Set {
                            feature: feature.name.clone(),
                            value: checked.to_string(),
                        },
                    );
                }
            } else if !feature.choices.is_empty() {
                let mut chosen = current.clone();
                egui::ComboBox::from_id_salt("value")
                    .selected_text(&chosen)
                    .width(ui.available_width() - 6.0)
                    .show_ui(ui, |ui| {
                        for option in &feature.choices {
                            ui.selectable_value(&mut chosen, option.clone(), option);
                        }
                    });
                if chosen != current {
                    self.send(
                        format!("Setting {}", feature.name),
                        SessionCommand::Set {
                            feature: feature.name.clone(),
                            value: chosen,
                        },
                    );
                }
            } else {
                // Keep untouched editors in sync with CLI/agent changes without destroying drafts.
                if self.edit_sources.get(&feature.name).is_some_and(|source| {
                    source != &current && self.edits.get(&feature.name) == Some(source)
                }) {
                    self.edits.insert(feature.name.clone(), current.clone());
                }
                self.edit_sources
                    .insert(feature.name.clone(), current.clone());
                let edit = self
                    .edits
                    .entry(feature.name.clone())
                    .or_insert_with(|| current.clone());
                let mut commit = false;
                ui.horizontal(|ui| {
                    let response = ui.add(
                        egui::TextEdit::singleline(edit)
                            .font(egui::TextStyle::Monospace)
                            .desired_width((ui.available_width() - 72.0).max(60.0)),
                    );
                    commit = ui.small_button("Set").clicked()
                        || (response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                });
                if commit {
                    let value = self.edits.get(&feature.name).cloned().unwrap_or_default();
                    self.send(
                        format!("Setting {}", feature.name),
                        SessionCommand::Set {
                            feature: feature.name.clone(),
                            value,
                        },
                    );
                }
            }
            if feature.min.is_some() || feature.max.is_some() || feature.unit.is_some() {
                let range = match (feature.min, feature.max) {
                    (Some(min), Some(max)) => format!("{} – {}", number(min), number(max)),
                    (Some(min), None) => format!("min {}", number(min)),
                    (None, Some(max)) => format!("max {}", number(max)),
                    _ => String::new(),
                };
                ui.label(
                    RichText::new(format!(
                        "{range} {}",
                        feature.unit.as_deref().unwrap_or_default()
                    ))
                    .size(10.0)
                    .color(ui.visuals().weak_text_color()),
                );
            }
            ui.add_space(4.0);
            ui.separator();
        });
    }

    fn capture_settings(&mut self, ui: &mut egui::Ui, snapshot: &SessionSnapshot) {
        ui.add_space(11.0);
        ui.label(RichText::new("Save frames").size(16.0).strong());
        ui.label(
            RichText::new("Capture to a local file or numbered sequence.")
                .small()
                .color(ui.visuals().weak_text_color()),
        );
        ui.add_space(8.0);
        ui.label("Output path");
        ui.add(egui::TextEdit::singleline(&mut self.output).desired_width(f32::INFINITY));
        ui.add_space(6.0);
        egui::Grid::new("capture-options")
            .num_columns(2)
            .spacing([14.0, 12.0])
            .show(ui, |ui| {
                ui.label("Frames");
                let previous_count = self.count;
                ui.add(egui::DragValue::new(&mut self.count).range(1..=100_000));
                if previous_count == 1 && self.count > 1 && self.output == "capture.png" {
                    self.output = "captures".into();
                } else if previous_count > 1 && self.count == 1 && self.output == "captures" {
                    self.output = "capture.png".into();
                }
                ui.end_row();
                ui.label("Format");
                let previous_format = self.format.clone();
                egui::ComboBox::from_id_salt("capture-format")
                    .selected_text(self.format.to_uppercase())
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.format, "png".into(), "PNG image");
                        ui.selectable_value(&mut self.format, "raw".into(), "Raw pixels");
                        ui.selectable_value(&mut self.format, "pgm".into(), "PGM monochrome");
                        ui.selectable_value(&mut self.format, "ppm".into(), "PPM color");
                    });
                if self.format != previous_format {
                    let mut path = std::path::PathBuf::from(&self.output);
                    if path.extension().and_then(|extension| extension.to_str())
                        == Some(previous_format.as_str())
                    {
                        path.set_extension(&self.format);
                        self.output = path.to_string_lossy().into_owned();
                    }
                }
                ui.end_row();
                ui.label("Timeout");
                ui.add(
                    egui::DragValue::new(&mut self.timeout_ms)
                        .range(100..=60_000)
                        .speed(100)
                        .suffix(" ms"),
                );
                ui.end_row();
            });
        ui.add_space(12.0);
        let capturing = self.pending.iter().any(|p| p.label == "Saving capture");
        if ui
            .add_enabled(
                snapshot.connected.is_some() && !self.output.trim().is_empty() && !capturing,
                egui::Button::new(if capturing {
                    "Saving…"
                } else {
                    "Capture & save"
                })
                .min_size(Vec2::new(ui.available_width(), 38.0)),
            )
            .clicked()
        {
            self.capture();
        }
        ui.label(
            RichText::new(format!(
                "{} · Capture from this session",
                shortcut_text(ui.ctx(), Action::Capture)
            ))
            .small()
            .color(ui.visuals().weak_text_color()),
        );
        ui.add_space(10.0);
        ui.separator();
        ui.add_space(5.0);
        ui.checkbox(&mut self.schedule_enabled, "Capture on a schedule");
        if self.schedule_enabled {
            egui::Grid::new("capture-schedule-options")
                .num_columns(2)
                .spacing([12.0, 10.0])
                .show(ui, |ui| {
                    ui.label("Start after");
                    ui.add(
                        egui::DragValue::new(&mut self.schedule_delay_seconds)
                            .range(0..=31_536_000)
                            .speed(5.0)
                            .suffix(" s"),
                    );
                    ui.end_row();
                    ui.label("Frame interval");
                    ui.add(
                        egui::DragValue::new(&mut self.schedule_interval_seconds)
                            .range(0.1..=86400.0)
                            .speed(1.0)
                            .suffix(" s"),
                    );
                    ui.end_row();
                });
            ui.label(
                RichText::new(format!(
                    "{} frames, one every {} seconds",
                    self.count,
                    number(self.schedule_interval_seconds)
                ))
                .small()
                .color(ui.visuals().weak_text_color()),
            );
            if ui
                .add_enabled(
                    snapshot.connected.is_some() && !self.output.trim().is_empty(),
                    egui::Button::new("Schedule capture")
                        .min_size(Vec2::new(ui.available_width(), 34.0)),
                )
                .clicked()
            {
                self.send(
                    "Scheduling capture",
                    SessionCommand::Schedule {
                        output: self.output.clone(),
                        count: self.count,
                        timeout_ms: self.timeout_ms,
                        format: self.format.clone(),
                        first_at_ms: epoch_ms()
                            .saturating_add(self.schedule_delay_seconds.saturating_mul(1000)),
                        interval_ms: (self.schedule_interval_seconds * 1000.0).round() as u64,
                        storage: self.storage_policy(),
                    },
                );
            }
        }
        ui.add_space(8.0);
        self.storage_settings(ui);
        ui.add_space(8.0);
        self.jobs(ui, snapshot);
        ui.add_space(18.0);
        ui.separator();
        ui.add_space(6.0);
        ui.label(RichText::new("Automation").strong());
        ui.label(RichText::new("Control this same camera from your shell or coding agent. Camera ownership stays in this session.").small().color(ui.visuals().weak_text_color()));
        let command = format!("capturefab --session {} status", shell_quote(&self.session));
        ui.label(RichText::new(&command).monospace().size(11.0));
        if ui.button("Copy session command").clicked() {
            ui.ctx().copy_text(command);
        }
    }

    fn forward_settings(&mut self, ui: &mut egui::Ui, snapshot: &SessionSnapshot) {
        ui.add_space(11.0);
        ui.label(RichText::new("Forward to a recorder").size(16.0).strong());
        ui.label(RichText::new("Publish the selected camera to MediaMTX, an NVR, or another compatible destination.").small().color(ui.visuals().weak_text_color()));
        ui.add_space(9.0);
        ui.label("Destination");
        ui.add(
            egui::TextEdit::singleline(&mut self.forward_output)
                .hint_text("rtsp://localhost:8554/camera")
                .desired_width(f32::INFINITY),
        );
        ui.add_space(8.0);
        egui::Grid::new("forward-options")
            .num_columns(2)
            .spacing([12.0, 12.0])
            .show(ui, |ui| {
                ui.label("Codec");
                egui::ComboBox::from_id_salt("forward-codec")
                    .selected_text(self.forward_codec.to_uppercase())
                    .width(135.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.forward_codec, "h264".into(), "H.264");
                        ui.selectable_value(&mut self.forward_codec, "h265".into(), "H.265 / HEVC");
                    });
                ui.end_row();
                ui.label("Encoder");
                ui.add(
                    egui::TextEdit::singleline(&mut self.forward_encoder)
                        .hint_text("auto")
                        .desired_width(135.0),
                )
                .on_hover_text(
                    "auto selects an available encoder. Enter an FFmpeg encoder name to override.",
                );
                ui.end_row();
                ui.label("Frame rate");
                ui.add(
                    egui::DragValue::new(&mut self.forward_fps)
                        .range(1.0..=240.0)
                        .speed(1.0)
                        .suffix(" fps"),
                );
                ui.end_row();
                ui.label("Bitrate");
                ui.add(
                    egui::TextEdit::singleline(&mut self.forward_bitrate)
                        .desired_width(135.0)
                        .hint_text("4M"),
                );
                ui.end_row();
                ui.label("Maximum file");
                ui.add(
                    egui::DragValue::new(&mut self.forward_file_mib)
                        .range(1.0..=65536.0)
                        .speed(16.0)
                        .suffix(" MiB"),
                )
                .on_hover_text(
                    "For file destinations, recording stops when this file size limit is reached.",
                );
                ui.end_row();
            });
        ui.add_space(14.0);
        let pending = self.pending.iter().any(|command| {
            command.label == "Starting forwarding" || command.label == "Stopping forwarding"
        });
        if let Some(destination) = &snapshot.forwarding {
            ui.label(RichText::new("FORWARDING").size(10.0).color(accent(ui)));
            ui.add(
                egui::Label::new(
                    RichText::new(redact_address(destination))
                        .monospace()
                        .small(),
                )
                .wrap(),
            );
            if ui
                .add_enabled(
                    !pending,
                    egui::Button::new("Stop forwarding")
                        .min_size(Vec2::new(ui.available_width(), 37.0)),
                )
                .clicked()
            {
                self.send("Stopping forwarding", SessionCommand::StopForward);
            }
        } else if ui
            .add_enabled(
                snapshot.connected.is_some() && !pending && !self.forward_output.trim().is_empty(),
                egui::Button::new(if pending {
                    "Starting…"
                } else {
                    "Start forwarding"
                })
                .min_size(Vec2::new(ui.available_width(), 37.0)),
            )
            .clicked()
        {
            self.send(
                "Starting forwarding",
                SessionCommand::Forward {
                    output: self.forward_output.trim().into(),
                    codec: self.forward_codec.clone(),
                    encoder: self.forward_encoder.trim().into(),
                    fps: self.forward_fps,
                    bitrate: self.forward_bitrate.trim().into(),
                    storage: self.storage_policy(),
                    max_file_bytes: (self.forward_file_mib * 1024.0 * 1024.0).round() as u64,
                },
            );
        }
        ui.add_space(12.0);
        self.storage_settings(ui);
        ui.add_space(14.0);
        ui.separator();
        ui.label(RichText::new("Encoder availability").strong());
        ui.label(RichText::new("Automatic selection uses the available hardware or software encoder. Run capturefab doctor to inspect media support and available encoders.").small().color(ui.visuals().weak_text_color()));
        if ui.button("Copy diagnostic command").clicked() {
            ui.ctx().copy_text("capturefab doctor".into());
        }
    }

    fn storage_policy(&self) -> StoragePolicy {
        StoragePolicy {
            max_bytes: (self.quota_gib * 1024.0 * 1024.0 * 1024.0).round() as u64,
            max_files: self.quota_files,
            max_age_seconds: self
                .retention_enabled
                .then(|| self.retention_days.saturating_mul(86400)),
            on_full: self.quota_action.clone(),
        }
    }

    fn storage_settings(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Storage & retention").show(ui, |ui| {
            egui::Grid::new("storage-policy-options").num_columns(2).spacing([10.0, 10.0]).show(ui, |ui| {
                ui.label("Disk budget");
                ui.add(egui::DragValue::new(&mut self.quota_gib).range(0.01..=102400.0).speed(1.0).suffix(" GiB"));
                ui.end_row();
                ui.label("File limit");
                ui.add(egui::DragValue::new(&mut self.quota_files).range(1..=1_000_000));
                ui.end_row();
                ui.label("At capacity");
                egui::ComboBox::from_id_salt("storage-on-full").selected_text(if self.quota_action == "stop" { "Stop" } else { "Delete oldest" }).width(125.0).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.quota_action, "stop".into(), "Stop when full");
                    ui.selectable_value(&mut self.quota_action, "delete-oldest".into(), "Delete oldest captures");
                });
                ui.end_row();
            });
            ui.checkbox(&mut self.retention_enabled, "Limit capture age");
            if self.retention_enabled { ui.add(egui::DragValue::new(&mut self.retention_days).range(1..=3650).suffix(" days")); }
            if self.quota_action == "delete-oldest" { ui.label(RichText::new("At capacity, removes the oldest Capturefab managed files in the output location.").small().color(ui.visuals().warn_fg_color)); }
        });
        ui.label(
            RichText::new(format!(
                "{} GiB · {} files · {}",
                number(self.quota_gib),
                self.quota_files,
                if self.quota_action == "stop" {
                    "stop at capacity"
                } else {
                    "delete oldest at capacity"
                }
            ))
            .size(10.0)
            .color(ui.visuals().weak_text_color()),
        );
    }

    fn jobs(&mut self, ui: &mut egui::Ui, snapshot: &SessionSnapshot) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Scheduled jobs").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(snapshot.connected.is_some(), egui::Button::new("Refresh"))
                    .clicked()
                {
                    self.send("Refreshing capture jobs", SessionCommand::Jobs);
                }
            });
        });
        if snapshot.jobs.is_empty() {
            ui.label(
                RichText::new("No scheduled captures for this camera.")
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
            return;
        }
        let jobs = serde_json::to_value(&snapshot.jobs).unwrap_or_default();
        if let Some(jobs) = jobs.as_array() {
            for job in jobs.iter().rev() {
                let id = job["id"].as_u64().unwrap_or_default();
                let status = job["status"].as_str().unwrap_or("unknown");
                egui::CollapsingHeader::new(format!("Job {id} · {status}"))
                    .id_salt(id)
                    .default_open(matches!(status, "pending" | "running" | "failed"))
                    .show(ui, |ui| {
                        let captured = job["captured"].as_u64().unwrap_or_default();
                        let count = job["count"].as_u64().unwrap_or(1);
                        ui.add(
                            egui::ProgressBar::new(captured as f32 / count.max(1) as f32)
                                .text(format!("{captured} / {count} frames")),
                        );
                        ui.add(
                            egui::Label::new(
                                RichText::new(job["output"].as_str().unwrap_or(""))
                                    .monospace()
                                    .small(),
                            )
                            .wrap(),
                        );
                        if matches!(status, "pending" | "running") {
                            let wait = job["next_at_ms"]
                                .as_u64()
                                .unwrap_or_default()
                                .saturating_sub(epoch_ms());
                            ui.label(
                                RichText::new(format!(
                                    "Next frame in {:.1} s",
                                    wait as f64 / 1000.0
                                ))
                                .small()
                                .color(ui.visuals().weak_text_color()),
                            );
                            if ui.small_button("Cancel job").clicked() {
                                self.send(
                                    "Cancelling capture job",
                                    SessionCommand::CancelJob { id },
                                );
                            }
                        }
                        if let Some(error) = job["error"].as_str() {
                            ui.label(
                                RichText::new(error)
                                    .small()
                                    .color(ui.visuals().error_fg_color),
                            );
                        }
                    });
            }
        }
    }

    fn footer(&mut self, ctx: &egui::Context, snapshot: &SessionSnapshot) {
        egui::TopBottomPanel::bottom("status")
            .exact_height(31.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(
                            self.logs_open,
                            if self.logs_open {
                                "Hide activity"
                            } else {
                                "Activity"
                            },
                        )
                        .on_hover_text(with_shortcut(ui.ctx(), "Session activity log", Action::ToggleActivity))
                        .clicked()
                    {
                        self.logs_open = !self.logs_open;
                    }
                    ui.separator();
                    if let Some((message, error, _)) = &self.notice {
                        ui.add(
                            egui::Label::new(RichText::new(message).small().color(if *error {
                                ui.visuals().error_fg_color
                            } else {
                                MUTED
                            }))
                            .truncate(),
                        );
                    } else if let Some(pending) = self.pending.first() {
                        ui.spinner();
                        ui.label(
                            RichText::new(format!("{}…", pending.label))
                                .small()
                                .color(ui.visuals().weak_text_color()),
                        );
                    } else {
                        ui.label(
                            RichText::new("Ready")
                                .small()
                                .color(ui.visuals().weak_text_color()),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(camera) = &snapshot.connected {
                            ui.label(
                                RichText::new(format!("{} · {}", camera.transport, camera.serial))
                                    .small()
                                    .color(ui.visuals().weak_text_color()),
                            );
                            if let Some(stats) = &snapshot.transport {
                                ui.separator();
                                ui.label(RichText::new(transport_text(stats)).small().color(
                                    if stats.notes.is_empty() {
                                        ui.visuals().weak_text_color()
                                    } else {
                                        ui.visuals().warn_fg_color
                                    },
                                ))
                                .on_hover_text(if stats.notes.is_empty() {
                                    "Packet size · resent packets recovered/requested · incomplete or missing frames".into()
                                } else {
                                    stats.notes.join("\n")
                                });
                            }
                        } else {
                            ui.label(
                                RichText::new("No camera connected")
                                    .small()
                                    .color(ui.visuals().weak_text_color()),
                            );
                        }
                    });
                });
            });
        if self.logs_open {
            egui::TopBottomPanel::bottom("logs")
                .default_height(140.0)
                .min_height(85.0)
                .resizable(true)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Session activity").strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Copy log").clicked() {
                                ui.ctx().copy_text(
                                    snapshot
                                        .logs
                                        .iter()
                                        .map(|l| format!("{} [{}] {}", l.time, l.level, l.message))
                                        .collect::<Vec<_>>()
                                        .join("\n"),
                                );
                            }
                        });
                    });
                    egui::ScrollArea::vertical()
                        .stick_to_bottom(true)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if snapshot.logs.is_empty() {
                                ui.label(
                                    RichText::new("Session events will appear here.")
                                        .small()
                                        .color(ui.visuals().weak_text_color()),
                                );
                            }
                            for entry in &snapshot.logs {
                                ui.horizontal_top(|ui| {
                                    ui.label(
                                        RichText::new(&entry.time)
                                            .monospace()
                                            .small()
                                            .color(ui.visuals().weak_text_color()),
                                    );
                                    let color = if entry.level.eq_ignore_ascii_case("error") {
                                        ui.visuals().error_fg_color
                                    } else {
                                        MUTED
                                    };
                                    ui.label(
                                        RichText::new(&entry.message)
                                            .monospace()
                                            .small()
                                            .color(color),
                                    );
                                });
                            }
                        });
                });
        }
    }

    fn preview(&mut self, ctx: &egui::Context, snapshot: &SessionSnapshot) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(if self.dark {
                        Color32::from_rgb(11, 18, 29)
                    } else {
                        Color32::from_rgb(235, 240, 244)
                    })
                    .inner_margin(16.0),
            )
            .show(ctx, |ui| {
                if snapshot.cameras.len() > 1 && !self.focus_camera {
                    self.preview_grid(ui, snapshot);
                    return;
                }
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(
                                snapshot
                                    .connected
                                    .as_ref()
                                    .map(|c| c.model.as_str())
                                    .unwrap_or("Live preview"),
                            )
                            .size(18.0)
                            .strong(),
                        );
                        ui.label(
                            RichText::new(format!(
                                "{}{}",
                                if snapshot.streaming {
                                    "STREAMING"
                                } else if snapshot.connected.is_some() {
                                    "CONNECTED · READY"
                                } else {
                                    "SELECT A CAMERA TO BEGIN"
                                },
                                snapshot.auto.as_ref().map_or("", |_| " · AUTO")
                            ))
                            .size(10.0)
                            .color(if snapshot.streaming {
                                AQUA
                            } else {
                                MUTED
                            }),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if snapshot.cameras.len() > 1
                            && ui
                                .button("All cameras")
                                .on_hover_text(with_shortcut(
                                    ui.ctx(),
                                    "Back to all cameras",
                                    Action::Overview,
                                ))
                                .clicked()
                        {
                            self.focus_camera = false;
                        }
                        let label = if snapshot.streaming {
                            "Stop stream"
                        } else {
                            "Start stream"
                        };
                        if ui
                            .add_enabled(
                                snapshot.connected.is_some(),
                                egui::Button::new(label).min_size(Vec2::new(115.0, 35.0)),
                            )
                            .on_hover_text(with_shortcut(
                                ui.ctx(),
                                "Start or stop acquisition",
                                Action::ToggleStream,
                            ))
                            .clicked()
                        {
                            self.toggle_stream(snapshot);
                        }
                    });
                });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    stat(ui, "FPS", format!("{:.1}", snapshot.fps), AQUA);
                    ui.separator();
                    stat(
                        ui,
                        "FRAMES",
                        snapshot.frames.to_string(),
                        if self.dark {
                            Color32::WHITE
                        } else {
                            Color32::from_rgb(25, 39, 53)
                        },
                    );
                    ui.separator();
                    let (label, lost) = snapshot
                        .transport
                        .as_ref()
                        .map_or(("DROPPED", snapshot.dropped), |stats| {
                            ("LOST", transport_loss(stats))
                        });
                    stat(
                        ui,
                        label,
                        lost.to_string(),
                        if lost > 0 {
                            Color32::from_rgb(238, 184, 97)
                        } else {
                            MUTED
                        },
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .selectable_label(self.histogram_open, "Histogram")
                            .clicked()
                        {
                            self.histogram_open = !self.histogram_open;
                        }
                    });
                });
                ui.add_space(10.0);
                let histogram_height = if self.histogram_open && self.shown.is_some() {
                    77.0
                } else {
                    0.0
                };
                let view_height = (ui.available_height() - 76.0 - histogram_height).max(100.0);
                let area_size = Vec2::new(ui.available_width(), view_height);
                let (area, _) = ui.allocate_exact_size(area_size, egui::Sense::hover());
                let painter = ui.painter_at(area);
                painter.rect_filled(
                    area,
                    9.0,
                    if self.dark {
                        Color32::from_rgb(6, 12, 21)
                    } else {
                        Color32::from_rgb(214, 222, 230)
                    },
                );
                let grid_color = if self.dark {
                    Color32::from_rgb(14, 24, 37)
                } else {
                    Color32::from_rgb(203, 213, 223)
                };
                let grid = 32.0;
                let mut x = area.left() + grid;
                while x < area.right() {
                    painter.line_segment(
                        [egui::pos2(x, area.top()), egui::pos2(x, area.bottom())],
                        Stroke::new(1.0_f32, grid_color),
                    );
                    x += grid;
                }
                let mut y = area.top() + grid;
                while y < area.bottom() {
                    painter.line_segment(
                        [egui::pos2(area.left(), y), egui::pos2(area.right(), y)],
                        Stroke::new(1.0_f32, grid_color),
                    );
                    y += grid;
                }
                if let Some(shown) = &self.shown {
                    let native = shown.size();
                    self.fit_scale =
                        ((area.width() - 16.0) / native.x).min((area.height() - 16.0) / native.y);
                    let scale = if self.fit { self.fit_scale } else { self.zoom };
                    let image_rect = egui::Rect::from_center_size(area.center(), native * scale);
                    shown.paint(&painter, self.gpu.as_ref(), image_rect);
                    if !snapshot.streaming {
                        painter.text(
                            egui::pos2(area.left() + 12.0, area.top() + 12.0),
                            egui::Align2::LEFT_TOP,
                            "LAST FRAME",
                            egui::FontId::monospace(11.0),
                            AQUA,
                        );
                    }
                } else {
                    let center = area.center() - Vec2::new(0.0, 25.0);
                    let camera = egui::Rect::from_center_size(center, Vec2::new(68.0, 46.0));
                    painter.rect_stroke(
                        camera,
                        9.0,
                        Stroke::new(2.0_f32, MUTED),
                        egui::StrokeKind::Inside,
                    );
                    painter.circle_stroke(center, 13.0, Stroke::new(2.0_f32, MUTED));
                    painter.circle_filled(center, 4.0, AQUA);
                    painter.rect_filled(
                        egui::Rect::from_min_size(
                            camera.left_top() + Vec2::new(10.0, -8.0),
                            Vec2::new(20.0, 9.0),
                        ),
                        3.0,
                        MUTED,
                    );
                    let text = if snapshot.connected.is_some() {
                        "Ready for your first frame"
                    } else {
                        "Your camera, in focus"
                    };
                    painter.text(
                        center + Vec2::new(0.0, 55.0),
                        egui::Align2::CENTER_CENTER,
                        text,
                        egui::FontId::proportional(17.0),
                        if self.dark {
                            Color32::from_rgb(208, 220, 231)
                        } else {
                            Color32::from_rgb(45, 63, 79)
                        },
                    );
                    painter.text(
                        center + Vec2::new(0.0, 79.0),
                        egui::Align2::CENTER_CENTER,
                        if snapshot.connected.is_some() {
                            "Start a stream or capture a frame"
                        } else {
                            "Discover and connect a device in the sidebar"
                        },
                        egui::FontId::proportional(12.0),
                        MUTED,
                    );
                }
                ui.horizontal(|ui| {
                    let ctx = ui.ctx().clone();
                    ui.selectable_value(&mut self.fit, true, "Fit")
                        .on_hover_text(with_shortcut(&ctx, "Zoom to fit", Action::ZoomFit));
                    if ui
                        .selectable_label(!self.fit && (self.zoom - 1.0).abs() < 0.01, "1:1")
                        .on_hover_text(with_shortcut(&ctx, "Actual pixels", Action::ZoomActual))
                        .clicked()
                    {
                        self.fit = false;
                        self.zoom = 1.0;
                    }
                    if ui
                        .small_button("−")
                        .on_hover_text(with_shortcut(&ctx, "Zoom out", Action::ZoomOut))
                        .clicked()
                    {
                        self.zoom_by(1.0 / 1.25);
                    }
                    if ui
                        .small_button("+")
                        .on_hover_text(with_shortcut(&ctx, "Zoom in", Action::ZoomIn))
                        .clicked()
                    {
                        self.zoom_by(1.25);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some((id, width, height, format, _)) = self.frame_meta {
                            ui.label(
                                RichText::new(format!(
                                    "{width} × {height} · {} · #{id}",
                                    pixel_name(format)
                                ))
                                .monospace()
                                .size(11.0)
                                .color(ui.visuals().weak_text_color()),
                            );
                        }
                    });
                });
                if histogram_height > 0.0 {
                    self.draw_histogram(ui);
                }
                if let Some(error) = self.display_error.as_ref().or(snapshot.last_error.as_ref()) {
                    ui.add(
                        egui::Label::new(
                            RichText::new(error)
                                .small()
                                .color(ui.visuals().error_fg_color),
                        )
                        .truncate(),
                    )
                    .on_hover_text(error);
                }
                ui.add_space(5.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            snapshot.connected.is_some(),
                            egui::Button::new("Capture frame"),
                        )
                        .on_hover_text(with_shortcut(
                            ui.ctx(),
                            "Save using the settings in Capture",
                            Action::Capture,
                        ))
                        .clicked()
                    {
                        self.capture();
                    }
                    ui.add(
                        egui::Label::new(
                            RichText::new(&self.output)
                                .monospace()
                                .size(11.0)
                                .color(ui.visuals().weak_text_color()),
                        )
                        .truncate(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button("Save settings")
                            .on_hover_text(with_shortcut(
                                ui.ctx(),
                                "Capture panel",
                                Action::CaptureTab,
                            ))
                            .clicked()
                        {
                            self.inspector_tab = 1;
                        }
                    });
                });
            });
    }

    fn preview_grid(&mut self, ui: &mut egui::Ui, snapshot: &SessionSnapshot) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("Camera overview").size(18.0).strong());
                let streaming = snapshot
                    .cameras
                    .iter()
                    .filter(|camera| camera.streaming)
                    .count();
                ui.label(
                    RichText::new(format!(
                        "{} CONNECTED · {streaming} STREAMING",
                        snapshot.cameras.len()
                    ))
                    .size(10.0)
                    .color(ui.visuals().weak_text_color()),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let start = snapshot.cameras.iter().any(|camera| !camera.streaming);
                if ui
                    .button(if start { "Start all" } else { "Stop all" })
                    .clicked()
                {
                    for camera in &snapshot.cameras {
                        if camera.streaming != start {
                            self.send_to(
                                &camera.info.id,
                                if start {
                                    "Starting streams"
                                } else {
                                    "Stopping streams"
                                },
                                if start {
                                    SessionCommand::Start
                                } else {
                                    SessionCommand::Stop
                                },
                            );
                        }
                    }
                }
                let manual = snapshot.cameras.iter().any(|camera| camera.auto.is_none());
                if ui
                    .button(if manual { "Auto all" } else { "Manual all" })
                    .clicked()
                {
                    for camera in &snapshot.cameras {
                        if camera.auto.is_none() == manual {
                            self.send_to(
                                &camera.info.id,
                                if manual {
                                    "Enabling auto mode"
                                } else {
                                    "Switching to manual"
                                },
                                if manual {
                                    SessionCommand::Auto {
                                        balance: Some(self.balance),
                                    }
                                } else {
                                    SessionCommand::Manual { revert: false }
                                },
                            );
                        }
                    }
                }
            });
        });
        ui.add_space(12.0);
        let count = snapshot.cameras.len();
        let columns = if count == 2 {
            if ui.available_width() >= 460.0 { 2 } else { 1 }
        } else {
            ((ui.available_width() / 220.0).floor() as usize)
                .max(1)
                .min((count as f32).sqrt().ceil() as usize)
        };
        let rows = count.div_ceil(columns);
        let gap = 12.0;
        let tile_width =
            ((ui.available_width() - gap * (columns - 1) as f32) / columns as f32).floor();
        let available_height = (ui.available_height() - 62.0).max(220.0);
        let tile_height = ((available_height - gap * (rows - 1) as f32) / rows as f32).max(220.0);
        egui::ScrollArea::vertical().id_salt("multi-camera-grid").max_height(available_height).auto_shrink([false, false]).show(ui, |ui| {
            for cameras in snapshot.cameras.chunks(columns) {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for camera in cameras {
                        let active = snapshot.active_camera.as_ref() == Some(&camera.info.id);
                        ui.push_id(&camera.info.id, |ui| {
                            ui.allocate_ui_with_layout(Vec2::new(tile_width, tile_height), egui::Layout::top_down(egui::Align::Min), |ui| {
                                egui::Frame::new().fill(ui.visuals().panel_fill).stroke(Stroke::new(if active { 1.5_f32 } else { 1.0_f32 }, if active { AQUA } else { ui.visuals().widgets.noninteractive.bg_stroke.color })).corner_radius(9.0).inner_margin(11.0).show(ui, |ui| {
                                    ui.set_min_width(tile_width - 24.0);
                                    ui.set_max_width(tile_width - 24.0);
                                    ui.horizontal(|ui| {
                                        status_dot(ui, if camera.streaming { AQUA } else { MUTED });
                                        ui.add(egui::Label::new(RichText::new(&camera.info.model).strong().size(14.0)).truncate());
                                        if active { ui.label(RichText::new("SELECTED").size(8.0).color(accent(ui))); }
                                    });
                                    ui.add(egui::Label::new(RichText::new(format!("{} · {}", camera.info.transport, camera.info.serial)).monospace().size(10.0).color(ui.visuals().weak_text_color())).truncate());
                                    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), tile_height - 168.0), egui::Sense::click());
                                    let painter = ui.painter_at(rect);
                                    painter.rect_filled(rect, 5.0, Color32::from_rgb(6, 12, 21));
                                    if let Some(preview) = self.previews.get(&camera.info.id) {
                                        if let Some(shown) = &preview.shown {
                                            let native = shown.size();
                                            let scale = (rect.width() / native.x).min(rect.height() / native.y);
                                            shown.paint(&painter, self.gpu.as_ref(), egui::Rect::from_center_size(rect.center(), native * scale));
                                            if !camera.streaming { painter.text(rect.left_top() + Vec2::splat(9.0), egui::Align2::LEFT_TOP, "LAST FRAME", egui::FontId::monospace(9.0), AQUA); }
                                        } else { painter.text(rect.center(), egui::Align2::CENTER_CENTER, if camera.streaming { "Waiting for a frame…" } else { "Ready to stream" }, egui::FontId::proportional(13.0), MUTED); }
                                    } else { painter.text(rect.center(), egui::Align2::CENTER_CENTER, "Ready to stream", egui::FontId::proportional(13.0), MUTED); }
                                    if response.clicked() { self.send("Selecting camera", SessionCommand::Select { camera: camera.info.id.clone() }); }
                                    response.on_hover_text(format!("Select {} for settings and capture\nAcquisition worker PID {}", camera.info.serial, camera.worker_pid));
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new(format!("{:.1} fps", camera.fps)).monospace().size(11.0).color(if camera.streaming { accent(ui) } else { ui.visuals().weak_text_color() }));
                                        ui.label(RichText::new(format!("{} frames", camera.frames)).size(10.0).color(ui.visuals().weak_text_color()));
                                        if let Some(destination) = &camera.forwarding { ui.label(RichText::new("OUT").size(9.0).color(accent(ui))).on_hover_text(format!("Forwarding to {}", redact_address(destination))); }
                                        if let Some(auto) = &camera.auto { ui.label(RichText::new("AUTO").size(9.0).color(accent(ui))).on_hover_text(format!("Auto mode · {} · balance {:.2}", auto.state, auto.balance)); }
                                        let lost = camera.transport.as_ref().map_or(camera.dropped, transport_loss);
                                        if lost > 0 { ui.label(RichText::new(format!("{lost} lost")).size(10.0).color(ui.visuals().warn_fg_color)); }
                                    });
                                    ui.horizontal(|ui| {
                                        if ui.small_button(if camera.streaming { "Stop" } else { "Start" }).clicked() { self.send_to(&camera.info.id, if camera.streaming { "Stopping stream" } else { "Starting stream" }, if camera.streaming { SessionCommand::Stop } else { SessionCommand::Start }); }
                                        if !active && ui.small_button("Select").clicked() { self.send("Selecting camera", SessionCommand::Select { camera: camera.info.id.clone() }); }
                                        if let Some((_, width, height, format)) = self.previews.get(&camera.info.id).and_then(|preview| preview.meta) {
                                            ui.add(egui::Label::new(RichText::new(format!("{width}×{height} {}", pixel_name(format))).size(9.0).color(ui.visuals().weak_text_color())).truncate());
                                        }
                                    });
                                    if let Some(error) = self.previews.get(&camera.info.id).and_then(|preview| preview.error.as_ref()).or(camera.last_error.as_ref()) {
                                        ui.add(egui::Label::new(RichText::new(error).size(10.0).color(ui.visuals().error_fg_color)).truncate()).on_hover_text(error);
                                    }
                                });
                            });
                        });
                    }
                });
                ui.add_space(gap);
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .button("Capture selected")
                .on_hover_text(with_shortcut(ui.ctx(), "Capture and save", Action::Capture))
                .clicked()
            {
                self.capture();
            }
            if ui
                .button("Focus selected")
                .on_hover_text(with_shortcut(
                    ui.ctx(),
                    "Focus selected camera",
                    Action::FocusCamera,
                ))
                .clicked()
            {
                self.focus_camera = true;
            }
            ui.add(
                egui::Label::new(
                    RichText::new(&self.output)
                        .monospace()
                        .size(11.0)
                        .color(ui.visuals().weak_text_color()),
                )
                .truncate(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("Save settings")
                    .on_hover_text(with_shortcut(ui.ctx(), "Capture panel", Action::CaptureTab))
                    .clicked()
                {
                    self.inspector_tab = 1;
                }
            });
        });
        ui.label(
            RichText::new(format!(
                "Click a preview to inspect its camera settings · {} / {} switches camera",
                shortcut_text(ui.ctx(), Action::PreviousCamera),
                shortcut_text(ui.ctx(), Action::NextCamera)
            ))
            .small()
            .color(ui.visuals().weak_text_color()),
        );
    }

    fn draw_histogram(&self, ui: &mut egui::Ui) {
        ui.label(
            RichText::new("LUMINANCE")
                .size(10.0)
                .color(ui.visuals().weak_text_color()),
        );
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 48.0), egui::Sense::hover());
        let max = self.histogram.iter().copied().max().unwrap_or(1).max(1) as f32;
        let bin_width = rect.width() / 64.0;
        for (i, value) in self.histogram.iter().enumerate() {
            let bar = egui::Rect::from_min_max(
                egui::pos2(
                    rect.left() + i as f32 * bin_width,
                    rect.bottom() - *value as f32 / max * rect.height(),
                ),
                egui::pos2(
                    rect.left() + (i + 1) as f32 * bin_width - 1.0,
                    rect.bottom(),
                ),
            );
            ui.painter()
                .rect_filled(bar, 0.0, AQUA.gamma_multiply(0.65));
        }
    }

    fn screenshot_update(&mut self, ctx: &egui::Context, snapshot: &SessionSnapshot) {
        let Some(mut request) = self.screenshot.take() else {
            return;
        };
        if let Some(receiver) = &request.save {
            match receiver.try_recv() {
                Ok(result) => {
                    *request
                        .outcome
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) = result
                        .err()
                        .map(|error| format!("Save renderer screenshot: {error:#}"));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                }
                Err(TryRecvError::Disconnected) => {
                    *request
                        .outcome
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) =
                        Some("Screenshot writer stopped".into());
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                }
                Err(TryRecvError::Empty) => {}
            }
        } else if request.requested {
            if let Some(image) = ctx.input(|input| {
                input.events.iter().find_map(|event| match event {
                    egui::Event::Screenshot { image, .. } => Some(image.clone()),
                    _ => None,
                })
            }) {
                let path = request.path.clone();
                let (sender, receiver) = mpsc::channel();
                std::thread::spawn(move || {
                    let _ = sender.send(save_screenshot(&image, &path));
                });
                request.save = Some(receiver);
            }
        } else if snapshot.cameras.len() == request.cameras as usize && self.pending.is_empty() {
            if request.streams_started.is_none() {
                for camera in &snapshot.cameras {
                    self.send_to(
                        &camera.info.id,
                        "Starting demo stream",
                        SessionCommand::Start,
                    );
                }
                request.streams_started = Some(Instant::now());
            } else if request
                .streams_started
                .is_some_and(|started| started.elapsed() > Duration::from_millis(1600))
                && snapshot
                    .cameras
                    .iter()
                    .all(|camera| camera.streaming && camera.frames >= 3)
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(
                    "capturefab-release",
                )));
                request.requested = true;
            }
        }
        if request.started.elapsed() > Duration::from_secs(30) {
            *request
                .outcome
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(format!(
                "Renderer screenshot timed out waiting for {} camera(s) and a complete painted frame",
                request.cameras
            ));
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        self.screenshot = Some(request);
        ctx.request_repaint_after(Duration::from_millis(33));
    }

    fn shortcuts(&mut self, ctx: &egui::Context, snapshot: &SessionSnapshot) {
        let keyboard_free = !self.focus_held
            && ctx.memory(|memory| memory.focused().is_none())
            && !egui::Popup::is_any_open(ctx);
        let connected = snapshot.connected.is_some();
        let overview = snapshot.cameras.len() > 1 && !self.focus_camera;
        for action in pressed_actions(ctx, keyboard_free) {
            match action {
                Action::Discover => self.discover(),
                Action::ConnectAddress => {
                    focus_and_select(ctx, egui::Id::new("connect-address"), &self.address)
                }
                Action::NextCamera => self.select_relative_camera(snapshot, 1),
                Action::PreviousCamera => self.select_relative_camera(snapshot, -1),
                Action::FocusCamera if overview => self.focus_camera = true,
                Action::Overview if self.help_open => self.help_open = false,
                Action::Overview if snapshot.cameras.len() > 1 => self.focus_camera = false,
                Action::ToggleStream if connected => self.toggle_stream(snapshot),
                Action::Capture if connected => self.capture(),
                Action::ToggleAuto if connected => self.toggle_auto(snapshot),
                Action::SearchFeatures => {
                    self.inspector_tab = 0;
                    focus_and_select(ctx, egui::Id::new("feature-search"), &self.search);
                }
                Action::FeaturesTab => self.inspector_tab = 0,
                Action::CaptureTab => self.inspector_tab = 1,
                Action::ForwardTab => self.inspector_tab = 2,
                Action::ZoomIn => self.zoom_by(1.25),
                Action::ZoomOut => self.zoom_by(1.0 / 1.25),
                Action::ZoomFit => self.fit = true,
                Action::ZoomActual => {
                    self.fit = false;
                    self.zoom = 1.0;
                }
                Action::ToggleActivity => self.logs_open = !self.logs_open,
                Action::CopySessionCommand => self.copy_session_command(ctx),
                Action::Fullscreen => {
                    let fullscreen = ctx.input(|input| input.viewport().fullscreen);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(
                        !fullscreen.unwrap_or(false),
                    ));
                }
                Action::Help => self.help_open = !self.help_open,
                Action::CloseWindow => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                // Pressed where it does not apply, e.g. Space with no camera connected.
                _ => {}
            }
        }
    }

    fn select_relative_camera(&mut self, snapshot: &SessionSnapshot, step: isize) {
        let count = snapshot.cameras.len();
        if count < 2 {
            return;
        }
        let current = snapshot
            .cameras
            .iter()
            .position(|camera| snapshot.active_camera.as_ref() == Some(&camera.info.id))
            .unwrap_or(0);
        let next = (current as isize + step).rem_euclid(count as isize) as usize;
        self.send(
            "Selecting camera",
            SessionCommand::Select {
                camera: snapshot.cameras[next].info.id.clone(),
            },
        );
    }

    fn zoom_by(&mut self, factor: f32) {
        let from = if self.fit { self.fit_scale } else { self.zoom };
        self.fit = false;
        self.zoom = (from * factor).clamp(0.1, 8.0);
    }

    fn toggle_auto(&mut self, snapshot: &SessionSnapshot) {
        if self.pending.iter().any(|p| {
            matches!(
                p.label.as_str(),
                "Enabling auto mode" | "Switching to manual" | "Updating auto balance"
            )
        }) {
            return;
        }
        if snapshot.auto.is_some() {
            self.send(
                "Switching to manual",
                SessionCommand::Manual { revert: false },
            );
        } else {
            self.send(
                "Enabling auto mode",
                SessionCommand::Auto {
                    balance: Some(self.balance),
                },
            );
        }
    }

    fn copy_session_command(&mut self, ctx: &egui::Context) {
        ctx.copy_text(format!(
            "capturefab --session {} status",
            shell_quote(&self.session)
        ));
        self.notice = Some(("Session command copied".into(), false, Instant::now()));
    }

    fn help(&mut self, ctx: &egui::Context) {
        egui::Window::new("Capturefab quick guide").open(&mut self.help_open).collapsible(false).resizable(false).default_width(620.0).show(ctx, |ui| {
            ui.label("Discover a camera, connect, then start a stream. The simulator is available without camera hardware.");
            ui.add_space(8.0);
            let os = ctx.os();
            ui.columns(2, |columns| {
                for (index, (section, actions)) in Action::SECTIONS.into_iter().enumerate() {
                    let ui = &mut columns[usize::from(index >= 2)];
                    ui.label(RichText::new(section).strong());
                    egui::Grid::new(("keyboard-help", section)).num_columns(2).spacing([14.0, 5.0]).show(ui, |ui| {
                        for action in actions.iter().filter(|action| !action.bindings(os).is_empty()) {
                            ui.label(RichText::new(shortcut_text(ctx, *action)).monospace().color(accent(ui)));
                            ui.label(action.label());
                            ui.end_row();
                        }
                    });
                    ui.add_space(8.0);
                }
            });
            ui.label(RichText::new("Space, Enter and Esc act on the workbench when no field or button has keyboard focus. Press Esc or click the background to release focus.").small().color(ui.visuals().weak_text_color()));
            ui.add_space(10.0);
            ui.separator();
            ui.label(RichText::new("One visible session, many ways to control it").strong());
            ui.label("Use Copy CLI to attach your terminal or coding agent. Session commands update this window and share its camera connection.");
            ui.label(RichText::new(format!("capturefab --session {} status", shell_quote(&self.session))).monospace());
            ui.label(RichText::new("capturefab --help").monospace());
            ui.label(RichText::new("Add stream URLs directly. Native camera sources use avfoundation:// on macOS, v4l2:// on Linux, and dshow:// on Windows.").small().color(ui.visuals().weak_text_color()));
            ui.add_space(8.0);
            ui.label(RichText::new("GigE: use a reachable address on the camera's subnet. USB3: the operating system must allow access to the camera. Activity shows connection and capture errors.").small().color(ui.visuals().weak_text_color()));
        });
    }
}

impl eframe::App for Workbench {
    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        if let (Some(gpu), Some(gl)) = (&self.gpu, gl) {
            gpu.destroy(gl);
        }
    }
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.dark = ctx.theme() == egui::Theme::Dark;
        self.poll();
        let snapshot = self.handle.snapshot();
        let camera_id = snapshot.connected.as_ref().map(|camera| camera.id.clone());
        if camera_id != self.observed_camera {
            self.observed_camera = camera_id;
            self.edits.clear();
            self.edit_sources.clear();
            self.frame_id = None;
            self.shown = None;
            self.frame_meta = None;
            self.display_error = None;
        }
        if snapshot.cameras.len() > 1 && !self.focus_camera {
            self.update_previews(ctx, &snapshot);
            self.frame_id = None;
        } else {
            self.update_frame(ctx);
            self.previews.clear();
        }
        if let Some(gpu) = &self.gpu {
            // Free GPU textures of views that are no longer on screen.
            let gpu_shown = |shown: &Option<Shown>| matches!(shown, Some(Shown::Gpu { .. }));
            gpu.retain(|key| {
                if key == MAIN_VIEW {
                    gpu_shown(&self.shown)
                } else {
                    self.previews
                        .get(key)
                        .is_some_and(|preview| gpu_shown(&preview.shown))
                }
            });
        }
        self.shortcuts(ctx, &snapshot);
        self.top_bar(ctx, &snapshot);
        self.footer(ctx, &snapshot);
        self.devices(ctx, &snapshot);
        self.inspector(ctx, &snapshot);
        self.preview(ctx, &snapshot);
        self.help(ctx);
        self.screenshot_update(ctx, &snapshot);
        self.focus_held = ctx.memory(|memory| memory.focused().is_some());
        ctx.request_repaint_after(if snapshot.cameras.iter().any(|camera| camera.streaming) {
            Duration::from_millis(33)
        } else if !self.pending.is_empty() {
            Duration::from_millis(60)
        } else {
            Duration::from_millis(300)
        });
    }
}

fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(9.0, 14.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.0, color);
}

/// Every keyboard command. Bindings, dispatch, tooltips and the help sheet all derive from this
/// one table so they cannot drift apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Discover,
    ConnectAddress,
    NextCamera,
    PreviousCamera,
    FocusCamera,
    Overview,
    ToggleStream,
    Capture,
    ToggleAuto,
    SearchFeatures,
    FeaturesTab,
    CaptureTab,
    ForwardTab,
    ZoomIn,
    ZoomOut,
    ZoomFit,
    ZoomActual,
    ToggleActivity,
    CopySessionCommand,
    Fullscreen,
    Help,
    CloseWindow,
}

impl Action {
    /// Dispatch order. egui ignores extra Shift/Alt, and Ctrl alongside ⌘, when matching, so a
    /// chord must come before any chord it extends (⌥⌘0 before ⌘0, ⌃⌘F before ⌘F).
    const ALL: [Action; 22] = [
        Action::Fullscreen,
        Action::ZoomActual,
        Action::ZoomFit,
        Action::ZoomIn,
        Action::ZoomOut,
        Action::CopySessionCommand,
        Action::ToggleAuto,
        Action::Discover,
        Action::ConnectAddress,
        Action::NextCamera,
        Action::PreviousCamera,
        Action::FocusCamera,
        Action::Overview,
        Action::ToggleStream,
        Action::Capture,
        Action::SearchFeatures,
        Action::FeaturesTab,
        Action::CaptureTab,
        Action::ForwardTab,
        Action::ToggleActivity,
        Action::Help,
        Action::CloseWindow,
    ];

    /// Help sheet layout: section title and its actions in reading order.
    const SECTIONS: [(&'static str, &'static [Action]); 4] = [
        (
            "Cameras",
            &[
                Action::Discover,
                Action::ConnectAddress,
                Action::NextCamera,
                Action::PreviousCamera,
                Action::FocusCamera,
                Action::Overview,
            ],
        ),
        (
            "Acquisition",
            &[
                Action::ToggleStream,
                Action::Capture,
                Action::ToggleAuto,
                Action::SearchFeatures,
            ],
        ),
        (
            "View",
            &[
                Action::FeaturesTab,
                Action::CaptureTab,
                Action::ForwardTab,
                Action::ZoomIn,
                Action::ZoomOut,
                Action::ZoomFit,
                Action::ZoomActual,
                Action::ToggleActivity,
                Action::Fullscreen,
            ],
        ),
        (
            "Window",
            &[
                Action::CopySessionCommand,
                Action::Help,
                Action::CloseWindow,
            ],
        ),
    ];

    fn label(self) -> &'static str {
        match self {
            Action::Discover => "Discover cameras",
            Action::ConnectAddress => "Connect to an address or stream",
            Action::NextCamera => "Select next camera",
            Action::PreviousCamera => "Select previous camera",
            Action::FocusCamera => "Focus selected camera",
            Action::Overview => "Back to all cameras",
            Action::ToggleStream => "Start / stop streaming",
            Action::Capture => "Capture and save",
            Action::ToggleAuto => "Toggle auto / manual",
            Action::SearchFeatures => "Search camera features",
            Action::FeaturesTab => "Features panel",
            Action::CaptureTab => "Capture panel",
            Action::ForwardTab => "Forward panel",
            Action::ZoomIn => "Zoom in",
            Action::ZoomOut => "Zoom out",
            Action::ZoomFit => "Zoom to fit",
            Action::ZoomActual => "Actual pixels (1:1)",
            Action::ToggleActivity => "Show / hide activity",
            Action::CopySessionCommand => "Copy session CLI command",
            Action::Fullscreen => "Toggle full screen",
            Action::Help => "Keyboard shortcuts and help",
            Action::CloseWindow => "Close window",
        }
    }

    /// Key bindings for `os`, primary first. Command chords use ⌘ on macOS and Ctrl elsewhere,
    /// avoiding keys that the OS menu or text fields already own (⌘H, ⌘Q, Ctrl+W, Ctrl+Tab).
    fn bindings(self, os: egui::os::OperatingSystem) -> Vec<egui::KeyboardShortcut> {
        use egui::{Key, KeyboardShortcut as S, Modifiers as M};
        let mac = os == egui::os::OperatingSystem::Mac;
        let cmd = |key| S::new(M::COMMAND, key);
        let bare = |key| S::new(M::NONE, key);
        match self {
            Action::Discover if mac => vec![cmd(Key::R)],
            Action::Discover => vec![cmd(Key::R), bare(Key::F5)],
            Action::ConnectAddress => vec![cmd(Key::L)],
            Action::NextCamera if mac => vec![S::new(M::COMMAND | M::ALT, Key::ArrowRight)],
            Action::NextCamera => vec![S::new(M::CTRL, Key::PageDown)],
            Action::PreviousCamera if mac => vec![S::new(M::COMMAND | M::ALT, Key::ArrowLeft)],
            Action::PreviousCamera => vec![S::new(M::CTRL, Key::PageUp)],
            Action::FocusCamera => vec![bare(Key::Enter)],
            Action::Overview => vec![bare(Key::Escape)],
            Action::ToggleStream => vec![bare(Key::Space)],
            Action::Capture => vec![cmd(Key::S)],
            Action::ToggleAuto => vec![S::new(M::COMMAND | M::SHIFT, Key::A)],
            Action::SearchFeatures => vec![cmd(Key::F)],
            Action::FeaturesTab => vec![cmd(Key::Num1)],
            Action::CaptureTab => vec![cmd(Key::Num2)],
            Action::ForwardTab => vec![cmd(Key::Num3)],
            Action::ZoomIn => vec![cmd(Key::Equals), cmd(Key::Plus)],
            Action::ZoomOut => vec![cmd(Key::Minus)],
            Action::ZoomFit => vec![cmd(Key::Num0)],
            Action::ZoomActual => vec![S::new(M::COMMAND | M::ALT, Key::Num0)],
            Action::ToggleActivity => vec![cmd(Key::J)],
            Action::CopySessionCommand => vec![S::new(M::COMMAND | M::SHIFT, Key::C)],
            Action::Fullscreen if mac => vec![S::new(M::MAC_CMD | M::CTRL, Key::F)],
            Action::Fullscreen => vec![bare(Key::F11)],
            Action::Help => vec![bare(Key::F1), cmd(Key::Slash)],
            Action::CloseWindow if mac => vec![cmd(Key::W)],
            Action::CloseWindow if os == egui::os::OperatingSystem::Nix => vec![cmd(Key::Q)],
            // Windows closes with Alt+F4, which the OS handles.
            Action::CloseWindow => vec![],
        }
    }
}

/// Plain keys (Space, Enter, Escape) belong to a focused widget; only chords and function keys
/// work while one has focus.
fn needs_free_keyboard(shortcut: &egui::KeyboardShortcut) -> bool {
    shortcut.modifiers.is_none()
        && !matches!(
            shortcut.logical_key,
            egui::Key::F1 | egui::Key::F5 | egui::Key::F11
        )
}

/// Actions pressed this frame, consuming their keys so widgets do not also see them.
fn pressed_actions(ctx: &egui::Context, keyboard_free: bool) -> Vec<Action> {
    let os = ctx.os();
    ctx.input_mut(|input| {
        Action::ALL
            .into_iter()
            .filter(|action| {
                action.bindings(os).iter().any(|shortcut| {
                    (keyboard_free || !needs_free_keyboard(shortcut))
                        && input.consume_shortcut(shortcut)
                })
            })
            .collect()
    })
}

/// All of an action's bindings formatted for this platform, e.g. "F1 / ⌘/".
fn shortcut_text(ctx: &egui::Context, action: Action) -> String {
    action
        .bindings(ctx.os())
        .iter()
        .map(|shortcut| ctx.format_shortcut(shortcut))
        .collect::<Vec<_>>()
        .join(" / ")
}

/// Tooltip text in the workbench's "description · shortcut" style.
fn with_shortcut(ctx: &egui::Context, text: &str, action: Action) -> String {
    match action.bindings(ctx.os()).first() {
        Some(shortcut) => format!("{text} · {}", ctx.format_shortcut(shortcut)),
        None => text.to_owned(),
    }
}

/// Focuses a single-line text field with its contents selected, like a browser location bar.
fn focus_and_select(ctx: &egui::Context, id: egui::Id, text: &str) {
    use egui::text::{CCursor, CCursorRange};
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    state.cursor.set_char_range(Some(CCursorRange::two(
        CCursor::new(0),
        CCursor::new(text.chars().count()),
    )));
    state.store(ctx, id);
    ctx.memory_mut(|memory| memory.request_focus(id));
}

fn save_screenshot(image: &egui::ColorImage, path: &std::path::Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(file, image.width() as u32, image.height() as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let bytes: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(|pixel| pixel.to_array())
        .collect();
    encoder.write_header()?.write_image_data(&bytes)?;
    Ok(())
}

fn stat(ui: &mut egui::Ui, label: &str, value: String, color: Color32) {
    let color = if color == AQUA { accent(ui) } else { color };
    ui.horizontal(|ui| {
        ui.label(RichText::new(value).size(19.0).monospace().color(color));
        ui.label(
            RichText::new(label)
                .size(9.0)
                .color(ui.visuals().weak_text_color()),
        );
    });
}

fn accent(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        AQUA
    } else {
        Color32::from_rgb(0, 111, 104)
    }
}

fn feature_group(name: &str) -> &'static str {
    if [
        "Width",
        "Height",
        "Offset",
        "Pixel",
        "Binning",
        "Decimation",
        "Reverse",
        "Sensor",
        "TestPattern",
    ]
    .iter()
    .any(|p| name.starts_with(p))
    {
        "Image"
    } else if [
        "Acquisition",
        "Exposure",
        "Gain",
        "Trigger",
        "Balance",
        "Black",
        "Gamma",
        "Analog",
        "Digital",
        "Auto",
    ]
    .iter()
    .any(|p| name.starts_with(p))
    {
        "Acquisition"
    } else if ["Device", "UserSet", "Camera", "Temperature"]
        .iter()
        .any(|p| name.starts_with(p))
    {
        "Device"
    } else if ["Gev", "U3V", "TL", "Stream", "Payload", "Packet"]
        .iter()
        .any(|p| name.starts_with(p))
    {
        "Transport"
    } else {
        "Other"
    }
}

fn value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Null => "—".into(),
        _ => value.to_string(),
    }
}

fn auto_summary(status: &AutoStatus) -> String {
    let strategy = match status.strategy.as_str() {
        "firmware" => "Camera auto exposure",
        "software" => "Capturefab exposure",
        "video-mode" => "Video mode",
        "none" => "Frame rate only",
        other => other,
    };
    std::iter::once(strategy.to_owned())
        .chain(status.exposure_us.map(|us| {
            if us < 1000.0 {
                format!("{us:.0} µs")
            } else {
                format!("{:.1} ms", us / 1000.0)
            }
        }))
        .chain(status.gain_db.map(|db| format!("{db:.1} dB")))
        .chain(status.target_fps.map(|fps| format!("{fps:.0} fps")))
        .collect::<Vec<_>>()
        .join(" · ")
}

fn change_text(change: &AutoChange, unit: Option<&str>) -> String {
    format!(
        "{} {} {} → {}{}",
        change.time,
        change.feature,
        value_text(change.from.as_ref().unwrap_or(&serde_json::Value::Null)),
        value_text(&change.to),
        unit.map(|u| format!(" {u}")).unwrap_or_default()
    )
}

fn transport_loss(stats: &TransportStats) -> u64 {
    stats.incomplete_frames + stats.lost_frames
}

fn transport_text(stats: &TransportStats) -> String {
    format!(
        "{}resend {}/{} · {} lost",
        stats
            .packet_size
            .map(|size| format!("{size} B packets · "))
            .unwrap_or_default(),
        stats.resend_recovered,
        stats.resend_requested,
        transport_loss(stats)
    )
}

fn number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.3}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned()
    }
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn pixel_name(pixel_format: u32) -> String {
    match pixel_format {
        MONO8 => "Mono8".into(),
        RGB8 => "RGB8".into(),
        0x0218_0015 => "BGR8".into(),
        0x0110_0003 => "Mono10".into(),
        0x0110_0005 => "Mono12".into(),
        0x0110_0007 => "Mono16".into(),
        0x0108_0008 => "BayerGR8".into(),
        0x0108_0009 => "BayerRG8".into(),
        0x0108_000a => "BayerGB8".into(),
        0x0108_000b => "BayerBG8".into(),
        _ => format!("0x{pixel_format:08x}"),
    }
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
        && !value.is_empty()
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn redact_address(value: &str) -> String {
    let Some((scheme, rest)) = value.split_once("://") else {
        return value.into();
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let safe_tail = tail.split(['?', '#']).next().unwrap_or(tail);
    let suffix = if safe_tail.len() != tail.len() {
        "?[redacted]"
    } else {
        ""
    };
    if let Some((_, host)) = authority.rsplit_once('@') {
        format!("{scheme}://[redacted]@{host}{safe_tail}{suffix}")
    } else {
        format!("{scheme}://{authority}{safe_tail}{suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn auto_feature_grouping_and_text() {
        assert_eq!(feature_group("AutoExposureTimeUpperLimit"), "Acquisition");
        assert_eq!(feature_group("AutoFunctionProfile"), "Acquisition");
        let status = AutoStatus {
            strategy: "firmware".into(),
            exposure_us: Some(8200.0),
            gain_db: Some(3.06),
            target_fps: Some(45.2),
            ..Default::default()
        };
        assert_eq!(
            auto_summary(&status),
            "Camera auto exposure · 8.2 ms · 3.1 dB · 45 fps"
        );
        let status = AutoStatus {
            strategy: "software".into(),
            exposure_us: Some(250.0),
            ..Default::default()
        };
        assert_eq!(auto_summary(&status), "Capturefab exposure · 250 µs");
        let change = AutoChange {
            time: "12:00:01".into(),
            feature: "AutoExposureTimeUpperLimit".into(),
            from: Some(json!(5000)),
            to: json!(19800.5),
            reason: "limits".into(),
        };
        assert_eq!(
            change_text(&change, Some("us")),
            "12:00:01 AutoExposureTimeUpperLimit 5000 → 19800.5 us"
        );
        let change = AutoChange {
            from: None,
            feature: "ExposureAuto".into(),
            to: json!("Continuous"),
            ..change
        };
        assert_eq!(
            change_text(&change, None),
            "12:00:01 ExposureAuto — → Continuous"
        );
    }
    #[test]
    fn transport_line_and_loss() {
        let stats = TransportStats {
            packet_size: Some(1500),
            resend_requested: 14,
            resend_recovered: 12,
            incomplete_frames: 2,
            lost_frames: 1,
            ..Default::default()
        };
        assert_eq!(transport_loss(&stats), 3);
        assert_eq!(
            transport_text(&stats),
            "1500 B packets · resend 12/14 · 3 lost"
        );
        assert_eq!(
            transport_text(&TransportStats::default()),
            "resend 0/0 · 0 lost"
        );
    }

    /// Modifiers the OS reports when the user presses `pattern` (⌘ on Mac, Ctrl elsewhere).
    fn pressed(pattern: egui::Modifiers, mac: bool) -> egui::Modifiers {
        let command = pattern.command || pattern.mac_cmd;
        egui::Modifiers {
            alt: pattern.alt,
            shift: pattern.shift,
            ctrl: pattern.ctrl || (command && !mac),
            mac_cmd: command && mac,
            command: command || (pattern.ctrl && !mac),
        }
    }

    const PLATFORMS: [egui::os::OperatingSystem; 3] = [
        egui::os::OperatingSystem::Mac,
        egui::os::OperatingSystem::Windows,
        egui::os::OperatingSystem::Nix,
    ];

    #[test]
    fn every_binding_dispatches_to_its_own_action() {
        for os in PLATFORMS {
            let mac = os == egui::os::OperatingSystem::Mac;
            for action in Action::ALL {
                for shortcut in action.bindings(os) {
                    let held = pressed(shortcut.modifiers, mac);
                    // egui ignores extra Shift/Alt, so the first match in ALL wins.
                    let winner = Action::ALL.into_iter().find(|candidate| {
                        candidate.bindings(os).iter().any(|other| {
                            other.logical_key == shortcut.logical_key
                                && held.matches_logically(other.modifiers)
                        })
                    });
                    assert_eq!(winner, Some(action), "{shortcut:?} on {os:?}");
                }
            }
        }
    }

    #[test]
    fn bindings_avoid_keys_owned_by_the_os_and_text_fields() {
        use egui::{Key, Modifiers as M};
        for os in PLATFORMS {
            let mac = os == egui::os::OperatingSystem::Mac;
            // macOS app menu (winit), text editing, and egui's Tab focus traversal.
            let mut reserved = vec![
                (M::COMMAND, Key::A),
                (M::COMMAND, Key::C),
                (M::COMMAND, Key::V),
                (M::COMMAND, Key::X),
                (M::COMMAND, Key::Z),
                (M::COMMAND, Key::Y),
                (M::CTRL, Key::Tab),
            ];
            if mac {
                reserved.extend([(M::COMMAND, Key::H), (M::COMMAND, Key::Q)]);
            } else {
                // egui text fields treat these as emacs-style deletions.
                reserved.extend([Key::H, Key::K, Key::U, Key::W].map(|key| (M::CTRL, key)));
            }
            for action in Action::ALL {
                for shortcut in action.bindings(os) {
                    let held = pressed(shortcut.modifiers, mac);
                    for (modifiers, key) in &reserved {
                        assert!(
                            !(shortcut.logical_key == *key
                                && held.matches_exact(pressed(*modifiers, mac))),
                            "{action:?} uses reserved {shortcut:?} on {os:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn help_sheet_lists_every_action_once() {
        let listed: Vec<Action> = Action::SECTIONS
            .iter()
            .flat_map(|(_, actions)| actions.iter().copied())
            .collect();
        assert_eq!(listed.len(), Action::ALL.len());
        for action in Action::ALL {
            assert_eq!(
                listed.iter().filter(|&&a| a == action).count(),
                1,
                "{action:?}"
            );
        }
        for os in PLATFORMS {
            for action in Action::ALL {
                // Only Windows leaves window closing to the OS (Alt+F4).
                assert_eq!(
                    action.bindings(os).is_empty(),
                    action == Action::CloseWindow && os == egui::os::OperatingSystem::Windows,
                    "{action:?} on {os:?}"
                );
            }
        }
    }

    #[test]
    fn shortcut_labels_follow_the_platform() {
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |_| {}); // loads fonts for formatting
        ctx.set_os(egui::os::OperatingSystem::Windows);
        assert_eq!(shortcut_text(&ctx, Action::Capture), "Ctrl+S");
        assert_eq!(shortcut_text(&ctx, Action::Discover), "Ctrl+R / F5");
        assert_eq!(shortcut_text(&ctx, Action::Fullscreen), "F11");
        assert_eq!(
            with_shortcut(&ctx, "Start or stop acquisition", Action::ToggleStream),
            "Start or stop acquisition · Space"
        );
        ctx.set_os(egui::os::OperatingSystem::Mac);
        // egui spells modifiers out when the font lacks the ⌘/⌥ glyphs.
        let discover = shortcut_text(&ctx, Action::Discover);
        assert!(["⌘R", "Cmd+R"].contains(&discover.as_str()), "{discover}");
        let actual = shortcut_text(&ctx, Action::ZoomActual);
        assert!(
            ["⌥⌘0", "Option+Cmd+0"].contains(&actual.as_str()),
            "{actual}"
        );
    }

    #[test]
    fn plain_keys_wait_for_free_keyboard() {
        let ctx = egui::Context::default();
        ctx.set_os(egui::os::OperatingSystem::Windows);
        let press = |key, modifiers| egui::RawInput {
            events: vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            }],
            modifiers,
            ..Default::default()
        };
        let run = |input, free| {
            let mut actions = Vec::new();
            let _ = ctx.run(input, |ctx| actions = pressed_actions(ctx, free));
            actions
        };
        let none = egui::Modifiers::NONE;
        assert_eq!(
            run(press(egui::Key::Space, none), true),
            [Action::ToggleStream]
        );
        assert!(run(press(egui::Key::Space, none), false).is_empty());
        assert_eq!(run(press(egui::Key::F1, none), false), [Action::Help]);
        let ctrl = egui::Modifiers::CTRL | egui::Modifiers::COMMAND;
        assert_eq!(run(press(egui::Key::S, ctrl), false), [Action::Capture]);
        let ctrl_alt = ctrl | egui::Modifiers::ALT;
        assert_eq!(
            run(press(egui::Key::Num0, ctrl_alt), true),
            [Action::ZoomActual]
        );
    }
}
