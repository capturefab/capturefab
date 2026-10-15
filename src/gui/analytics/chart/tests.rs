//! Actual composite widgets, native shaped paragraphs and software rasterization.
use super::*;
use crate::analytics::{AnalyticsPoint, AnalyticsTotals, CameraAnalytics};
use crate::gui::{prefs::UiScale, style};
use iced::advanced::{
    Layout, layout,
    renderer::Headless,
    widget::{Tree, tree},
};
use iced::advanced::{renderer::Renderer as _, svg::Renderer as _};
use iced::{Color, Element, Point, Rectangle, Size, Theme, Transformation};

type NativeText = iced::advanced::text::paragraph::Plain<
    <iced::Renderer as iced::advanced::text::Renderer>::Paragraph,
>;
const WIDTHS: [u32; 6] = [180, 272, 400, 839, 840, 1012];
const PALETTES: [(&str, &Palette); 4] = [
    ("light", &style::LIGHT),
    ("dark", &style::DARK),
    ("light-contrast", &style::LIGHT_CONTRAST),
    ("dark-contrast", &style::DARK_CONTRAST),
];
fn active_report() -> AnalyticsSnapshot {
    AnalyticsSnapshot {
        retention_seconds: crate::analytics::RETENTION_SECONDS,
        sample_interval_seconds: 1,
        started_at_ms: 1_000,
        collected_at_ms: 86_401_000,
        samples: 8,
        truncated: true,
        totals: AnalyticsTotals {
            frames: u64::MAX,
            dropped: 129,
            bytes: u64::MAX,
        },
        cameras: (0..5)
            .map(|index| CameraAnalytics {
                id: format!("sim:long-camera-identity-{index}"),
                name: format!("Camera {index} · 镜头🚀<&>WWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWW"),
                frames: 10_000,
                dropped: index,
                bytes: u64::MAX >> index,
                fps: 20.0 + index as f64,
                bytes_per_second: 1.0e14,
                drop_percent: index as f64,
                streaming: index % 2 == 0,
                stale: false,
            })
            .collect(),
        points: (0..8)
            .map(|index| AnalyticsPoint {
                timestamp_ms: 1_000 + index * 86_400_000 / 7,
                frames: 1_000,
                dropped: index,
                bytes: 1_000_000,
                fps: 10.0 + index as f64 * 7.0,
                bytes_per_second: 1.0e14 + index as f64 * 1.0e13,
                drop_percent: index as f64 * 0.5,
            })
            .collect(),
    }
}

fn screenshot(
    renderer: &mut iced::Renderer,
    view: &Element<'_, Message>,
    tree: &Tree,
    node: &layout::Node,
    canvas: Size,
    clip: Rectangle,
    scale: f32,
) -> Vec<u8> {
    renderer.reset(Rectangle::with_size(canvas * scale));
    renderer.with_transformation(Transformation::scale(scale), |renderer| {
        renderer.with_layer(clip, |renderer| {
            view.as_widget().draw(
                tree,
                renderer,
                &Theme::Light,
                &iced::advanced::renderer::Style::default(),
                Layout::new(node),
                iced::mouse::Cursor::Unavailable,
                &clip,
            );
        });
    });
    renderer.screenshot(
        Size::new(
            (canvas.width * scale).ceil() as u32,
            (canvas.height * scale).ceil() as u32,
        ),
        1.0,
        Color::WHITE,
    )
}

fn assert_pixels_inside(pixels: &[u8], canvas: Size, clip: Rectangle, scale: f32) {
    let width = (canvas.width * scale).ceil() as u32;
    let height = (canvas.height * scale).ceil() as u32;
    let clip = clip * Transformation::scale(scale);
    assert_eq!(pixels.len(), (width * height * 4) as usize);
    let mut painted = 0;
    for y in 0..height {
        for x in 0..width {
            let pixel = &pixels[((y * width + x) * 4) as usize..][..4];
            if x as f32 >= clip.x.floor()
                && (x as f32) < (clip.x + clip.width).ceil()
                && y as f32 >= clip.y.floor()
                && (y as f32) < (clip.y + clip.height).ceil()
            {
                painted += usize::from(pixel != [255, 255, 255, 255]);
            } else {
                assert_eq!(
                    pixel,
                    [255, 255, 255, 255],
                    "SVG escaped the ancestor clip at {x},{y}"
                );
            }
        }
    }
    assert!(
        painted > 100,
        "the real software SVG renderer paints chart marks"
    );
}

fn artifact(name: &str, pixels: &[u8], canvas: Size) {
    let Ok(directory) = std::env::var("CAPTUREFAB_CHART_ARTIFACTS") else {
        return;
    };
    std::fs::create_dir_all(&directory).unwrap();
    let file = std::fs::File::create(std::path::Path::new(&directory).join(format!("{name}.png")))
        .unwrap();
    let mut encoder = png::Encoder::new(
        file,
        canvas.width.ceil() as u32,
        canvas.height.ceil() as u32,
    );
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(pixels)
        .unwrap();
}

fn text_states<'a>(tree: &'a Tree, out: &mut Vec<&'a NativeText>) {
    if tree.tag == tree::Tag::of::<NativeText>() {
        out.push(tree.state.downcast_ref::<NativeText>());
    }
    for child in &tree.children {
        text_states(child, out);
    }
}

fn number(node: roxmltree::Node<'_, '_>, attribute: &str) -> f64 {
    node.attribute(attribute).unwrap().parse().unwrap()
}
fn color(value: &str) -> [u8; 3] {
    assert!(value.starts_with('#'));
    [1, 3, 5].map(|at| u8::from_str_radix(&value[at..at + 2], 16).unwrap())
}
fn contrast(a: [u8; 3], b: [u8; 3]) -> f64 {
    let luminance = |rgb: [u8; 3]| {
        let [r, g, b] = rgb.map(|v| {
            let v = f64::from(v) / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        });
        0.2126 * r + 0.7152 * g + 0.0722 * b
    };
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[test]
fn profile_floors_logical_width_and_keeps_interface_scale_out_of_the_svg() {
    assert_eq!(profile(400.9, &style::LIGHT).width, 400);
    assert_eq!(profile(12.0, &style::LIGHT).width, 180);
    assert_eq!(profile(9000.0, &style::LIGHT).width, 2048);
    let logical_width = (640.0 / UiScale::Larger.factor() - 48.0).clamp(320.0, 1060.0) - 48.0;
    assert_eq!(profile(logical_width, &style::LIGHT).width, 396);
    for width in WIDTHS {
        let p = profile(width as f32, &style::LIGHT);
        assert_eq!(p.height, if width < 840 { 780 } else { 520 });
        assert_eq!(
            p.panel_width,
            if width < 840 { width } else { (width - 16) / 2 }
        );
        assert_ne!(p, profile(width as f32, &style::DARK));
    }
}

#[test]
fn numeric_plots_keep_readable_fonts_and_quantized_contrast_without_camera_identity_in_svg() {
    let report = active_report();
    for width in WIDTHS {
        for (name, palette) in PALETTES {
            let profile = profile(width as f32, palette);
            let sources = crate::analytics::render_interface_time_plots(
                &report,
                profile.panel_width,
                profile.colors,
            )
            .unwrap();
            for (index, source) in sources.iter().enumerate() {
                let doc = roxmltree::Document::parse(source).unwrap();
                let root = doc.root_element();
                assert_eq!(
                    root.attribute("width").unwrap().parse::<u32>().unwrap(),
                    profile.panel_width
                );
                assert_eq!(root.attribute("height"), Some("260"));
                assert!(!source.contains("sim:long-camera"));
                assert!(!source.contains("镜头"));
                assert!(!doc.descendants().any(|node| node.has_tag_name("script") || node.attribute("href").is_some()));
                let caption = ["Frame rate", "Throughput", "Missed frame rate"][index];
                let caption = doc
                    .descendants()
                    .find(|node| {
                        node.has_tag_name("text")
                            && node.text().unwrap_or_default().trim() == caption
                    })
                    .unwrap();
                assert!((number(caption, "font-size") - 16.0).abs() < 0.001);
                for node in doc.descendants().filter(|node| node.has_tag_name("text")) {
                    assert!(number(node, "font-size") >= 12.0 - 0.001);
                    assert!(
                        contrast(
                            color(node.attribute("fill").unwrap()),
                            profile.colors.background
                        ) >= 4.5,
                        "{name}/{width}"
                    );
                }
                let line = doc
                    .descendants()
                    .find(|node| {
                        node.has_tag_name("polyline")
                            && node.attribute("stroke-width")
                                == Some(if palette.increased() { "3" } else { "2" })
                    })
                    .unwrap();
                assert!(
                    contrast(
                        color(line.attribute("stroke").unwrap()),
                        profile.colors.background
                    ) >= 3.0
                );
            }
        }
    }
}

#[test]
fn actual_composite_keeps_native_full_names_and_pixels_inside_ancestor_clip() {
    let report = active_report();
    let mut renderer = crate::gui::destinations::layout_tests::renderer();
    for width in WIDTHS.into_iter().chain([396]) {
        for (name, palette) in PALETTES {
            let profile = profile(width as f32, palette);
            let graphic = render(&report, profile).unwrap();
            for handle in &graphic.plots {
                assert_eq!(
                    renderer.measure_svg(handle),
                    Size::new(profile.panel_width, 260)
                );
            }
            let state = ViewState::default();
            let mut view = view(&report, &graphic, profile, profile, palette, &state);
            let mut tree = Tree::new(view.as_widget());
            let node = view
                .as_widget_mut()
                .layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, Size::new(width as f32, 5000.0)),
                )
                .move_to(Point::new(8.0, 8.0));
            assert_eq!(node.size().width, width as f32, "no second image scaling");
            let mut native = Vec::new();
            text_states(&tree, &mut native);
            for camera in &report.cameras {
                let text = native
                    .iter()
                    .find(|text| text.content() == camera.name)
                    .expect("full native camera identity, without elision");
                assert!(
                    text.min_width() <= profile.panel_width as f32 - 28.0 + 0.01,
                    "native wrapping stays in its panel"
                );
                assert!(native.iter().any(|text| text.content() == camera.id));
                for expected in [
                    camera.name.clone(),
                    camera.id.clone(),
                    format!(
                        "{} · {} bytes",
                        human_bytes(camera.bytes),
                        grouped(camera.bytes)
                    ),
                ] {
                    assert_eq!(
                        native
                            .iter()
                            .filter(|text| text.content() == expected)
                            .count(),
                        1,
                        "full name, canonical ID and exact bytes each have one native paragraph"
                    );
                }
            }
            let canvas = node.size() + Size::new(16.0, 16.0);
            let clip = node.bounds();
            let pixels = screenshot(&mut renderer, &view, &tree, &node, canvas, clip, 1.0);
            assert_pixels_inside(&pixels, canvas, clip, 1.0);
            artifact(&format!("analytics-native-{name}-{width}"), &pixels, canvas);
            // Isolate each numeric caption above all data marks. Linux's missing
            // generic font used to leave these bands blank despite visible lines.
            for index in 0..3 {
                let x = 8 + if width >= 840 && index == 1 {
                    profile.panel_width + 16
                } else {
                    0
                };
                let y = 8 + if width >= 840 {
                    u32::from(index == 2) * 260
                } else {
                    index as u32 * 260
                };
                let mut ink = 0;
                for py in y + 10..y + 35 {
                    for px in x..x + profile.panel_width {
                        let at = ((py * canvas.width as u32 + px) * 4) as usize;
                        ink += usize::from(pixels[at..at + 3] != rgb(palette.base));
                    }
                }
                assert!(
                    ink > 20,
                    "{name}/{width}: real numeric caption glyph pixels"
                );
            }
            if width == 400 && name == "dark" {
                for scale in UiScale::ALL {
                    let clip = Rectangle::new(Point::new(31.0, 67.0), Size::new(180.0, 113.0));
                    let pixels = screenshot(
                        &mut renderer,
                        &view,
                        &tree,
                        &node,
                        canvas,
                        clip,
                        scale.factor(),
                    );
                    assert_pixels_inside(&pixels, canvas, clip, scale.factor());
                }
            }
        }
    }
}

#[test]
fn native_panel_grows_with_wrapped_identity_and_camera_count_without_reserved_rows() {
    let fixture = active_report();
    let renderer = crate::gui::destinations::layout_tests::renderer();
    for width in [180, 396, 498] {
        let mut heights = Vec::new();
        for count in [0, 1, 2, 5] {
            let mut report = fixture.clone();
            report.cameras.truncate(count);
            let state = ViewState::default();
            let mut view = camera_panel(&report, width as f32, &style::LIGHT, &state);
            let mut tree = Tree::new(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(width as f32, 5000.0)),
            );
            assert_eq!(node.size().width, width as f32);
            heights.push(node.size().height);
            assert_eq!(state.cameras.borrow().len(), count);
        }
        assert!(heights[0] < 100.0, "empty panel has no phantom camera rows");
        assert!(
            heights.windows(2).all(|pair| pair[0] < pair[1]),
            "{width}: {heights:?}"
        );
    }
}

#[test]
fn camera_native_group_ids_follow_identity_across_rank_changes_and_cache_is_bounded() {
    let mut report = active_report();
    report.cameras[1].name = report.cameras[0].name.clone();
    let renderer = crate::gui::destinations::layout_tests::renderer();
    let state = ViewState::default();
    let snapshot = |report: &AnalyticsSnapshot| {
        let mut view = camera_panel(report, 396.0, &style::LIGHT, &state);
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(396.0, 5000.0)),
        );
        accessibility::snapshot_widget(
            view.as_widget_mut(),
            &mut tree,
            Layout::new(&node),
            &renderer,
        )
        .0
    };
    let first = snapshot(&report);
    let headings: Vec<_> = first
        .nodes
        .iter()
        .filter(|(_, node)| node.role() == accessibility::Role::Heading)
        .collect();
    assert_eq!(headings.len(), 1);
    assert_eq!(headings[0].1.label(), Some("Camera data volume"));
    let retained = state.cameras.borrow().clone();
    report.cameras[0].bytes = 1;
    report.cameras[4].bytes = u64::MAX;
    let reordered = snapshot(&report);
    for camera in &report.cameras {
        let id = retained[&camera.id];
        let expected = format!("Camera data volume: {} ({})", camera.name, camera.id);
        assert_eq!(
            first
                .nodes
                .iter()
                .find(|(node, _)| *node == id)
                .unwrap()
                .1
                .label(),
            Some(expected.as_str())
        );
        assert_eq!(
            reordered
                .nodes
                .iter()
                .find(|(node, _)| *node == id)
                .unwrap()
                .1
                .label(),
            Some(expected.as_str())
        );
    }
    report.cameras.retain(|camera| camera.id.ends_with('0'));
    let _ = snapshot(&report);
    assert_eq!(state.cameras.borrow().len(), 1);
    report.cameras.clear();
    let _ = snapshot(&report);
    assert!(state.cameras.borrow().is_empty());
}

#[test]
fn empty_camera_name_uses_its_full_canonical_identity_once() {
    let mut report = active_report();
    report.cameras.truncate(1);
    report.cameras[0].name.clear();
    let renderer = crate::gui::destinations::layout_tests::renderer();
    let state = ViewState::default();
    let mut view = camera_panel(&report, 180.0, &style::LIGHT, &state);
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(180.0, 1000.0)),
    );
    let mut native = Vec::new();
    text_states(&tree, &mut native);
    assert_eq!(
        native
            .iter()
            .filter(|text| text.content() == report.cameras[0].id)
            .count(),
        1
    );
    let (native, _, _) = accessibility::snapshot_widget(
        view.as_widget_mut(),
        &mut tree,
        Layout::new(&node),
        &renderer,
    );
    assert!(native.nodes.iter().any(|(_, node)| node.label()
        == Some(format!("Camera data volume: {}", report.cameras[0].id).as_str())));
}

#[cfg(target_os = "macos")]
#[test]
fn actual_native_camera_paragraphs_shape_mixed_scripts_and_color_emoji_without_last_resort() {
    let mut report = active_report();
    report.cameras.truncate(1);
    let name = "Camera 镜头 e\u{301} العربية ع\u{200d}رب 🇺🇸 👩‍🚀 <&>";
    report.cameras[0].name = name.to_owned();
    let mut renderer = crate::gui::destinations::layout_tests::renderer();
    let state = ViewState::default();
    let mut view = camera_panel(&report, 396.0, &style::LIGHT, &state);
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(396.0, 1000.0)),
    );
    let mut paragraphs = Vec::new();
    text_states(&tree, &mut paragraphs);
    let text = paragraphs
        .into_iter()
        .find(|text| text.content() == name)
        .unwrap();
    // Inspect the actual cached Text widget paragraph, not a separately shaped
    // surrogate and not merely whether the raster contains some ink.
    let mut fonts = iced::advanced::graphics::text::font_system()
        .write()
        .unwrap();
    let db = fonts.raw().db();
    let mut emoji = Vec::new();
    let mut cjk = 0;
    let mut combining = 0;
    let mut arabic = 0;
    for run in text.raw().buffer().layout_runs() {
        for glyph in run.glyphs {
            let cluster = &run.text[glyph.start..glyph.end];
            assert_ne!(
                glyph.glyph_id, 0,
                "missing actual native glyph: {cluster:?}"
            );
            let face = db.face(glyph.font_id).unwrap();
            assert!(
                !face
                    .families
                    .iter()
                    .any(|(name, _)| name.contains("LastResort"))
            );
            if cluster.contains('镜') || cluster.contains('头') {
                cjk += 1;
            }
            if cluster.contains('e') && cluster.contains('\u{301}') {
                combining += 1;
            }
            if cluster
                .chars()
                .any(|ch| ('\u{600}'..='\u{6ff}').contains(&ch))
            {
                arabic += 1;
            }
            if cluster.contains('👩') || cluster.contains('🇺') {
                assert!(
                    face.families
                        .iter()
                        .any(|(name, _)| name.contains("Apple Color Emoji")),
                    "{cluster:?}: {:?}",
                    face.families
                );
                emoji.push(cluster.to_owned());
            }
        }
    }
    drop(fonts);
    assert!(cjk >= 2 && combining >= 1 && arabic >= 4);
    assert!(
        emoji.iter().any(|cluster| cluster == "👩‍🚀"),
        "joined emoji remains one native cluster: {emoji:?}"
    );
    assert!(
        emoji.iter().any(|cluster| cluster == "🇺🇸"),
        "regional flag remains one native cluster: {emoji:?}"
    );
    let canvas = node.size();
    let pixels = screenshot(
        &mut renderer,
        &view,
        &tree,
        &node,
        canvas,
        node.bounds(),
        1.0,
    );
    artifact("analytics-native-mixed-scripts", &pixels, canvas);
    assert!(
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0].abs_diff(p[1]) > 30 && p[1].abs_diff(p[2]) > 30)
            .count()
            > 5,
        "actual color emoji pixels"
    );
}

#[derive(Default)]
struct Geometry {
    controls: HashMap<String, (accessibility::NodeId, Rectangle)>,
    text: HashMap<String, Rectangle>,
}
impl iced::advanced::widget::Operation for Geometry {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation)) {
        operate(self);
    }
    fn text(&mut self, _: Option<&iced::advanced::widget::Id>, bounds: Rectangle, text: &str) {
        self.text.insert(text.to_owned(), bounds);
    }
    fn custom(
        &mut self,
        _: Option<&iced::advanced::widget::Id>,
        bounds: Rectangle,
        state: &mut dyn std::any::Any,
    ) {
        if let Some(control) = state.downcast_ref::<accessibility::Control<Message>>()
            && let Some(name) = control.node.label()
        {
            self.controls.insert(name.to_owned(), (control.id, bounds));
        }
    }
}

#[test]
fn actual_window_pending_resize_keeps_current_width_native_values_history_and_pinned_close() {
    use crate::{
        gui::{Appearance, Workbench},
        session::SessionHandle,
    };
    use std::sync::Arc;
    let renderer = crate::gui::destinations::layout_tests::renderer();
    let mut bench = Workbench::new(
        SessionHandle::new(),
        "chart-pending-geometry".into(),
        true,
        None,
    );
    bench.width = 1200.0;
    bench.appearance = Appearance::Light;
    bench.analytics.open = true;
    let report = Arc::new(active_report());
    let wide = bench.analytics_chart_profile();
    assert!(wide.width >= 840);
    let graphic = render(&report, wide).unwrap();
    bench
        .analytics
        .accept(Ok((report.clone(), graphic, wide)), wide);
    bench
        .analytics
        .history
        .input(super::super::HistoryInput::Toggle, Some(&report));
    let mut tree = Tree::empty();
    let mut previous_ids = None;
    for phase in 0..3 {
        if phase == 1 {
            bench.ui_scale = UiScale::Larger;
            bench.width = 640.0 / bench.ui_scale.factor();
        }
        let current = bench.analytics_chart_profile();
        if phase == 2 {
            let graphic = render(&report, current).unwrap();
            bench
                .analytics
                .accept(Ok((report.clone(), graphic, current)), current);
        }
        let size = Size::new(bench.width, 620.0 / bench.ui_scale.factor());
        let mut view = bench.window_view(bench.main_window);
        tree.diff(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, size),
        );
        let mut geometry = Geometry::default();
        view.as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut geometry);
        let (snapshot, _, _) = accessibility::snapshot_widget(
            view.as_widget_mut(),
            &mut tree,
            Layout::new(&node),
            &renderer,
        );
        let images: Vec<_> = ["Frame rate", "Throughput", "Missed frame rate"]
            .into_iter()
            .map(|caption| {
                let (id, bounds) = geometry.controls[caption];
                assert!(
                    (bounds.width - current.panel_width as f32).abs() < 0.5,
                    "phase {phase}: {caption} has {} rather than current {}",
                    bounds.width,
                    current.panel_width
                );
                assert_eq!(bounds.height, 260.0);
                let image = snapshot.nodes.iter().find(|(node, _)| *node == id).unwrap();
                assert_eq!(image.1.role(), accessibility::Role::Image);
                assert_eq!(image.1.live(), None);
                assert_eq!(
                    image
                        .1
                        .description()
                        .unwrap()
                        .contains("Updating chart layout"),
                    phase == 1
                );
                id
            })
            .collect();
        if let Some(previous) = &previous_ids {
            assert_eq!(
                &images, previous,
                "all three Image IDs survive grid reparenting, placeholder and reply"
            );
        }
        previous_ids = Some(images);
        for camera in &report.cameras {
            let bounds = geometry.text[&camera.name];
            assert!(
                bounds.width > 100.0 && bounds.width <= current.panel_width as f32 - 28.0 + 0.5,
                "phase {phase}: full native identity has useful width {bounds:?}"
            );
            assert!(
                snapshot
                    .nodes
                    .iter()
                    .any(|(_, node)| node.label() == Some(camera.name.as_str()))
            );
        }
        assert!(
            geometry.controls.contains_key("Hide history data"),
            "retained source history remains expanded"
        );
        let close = geometry.controls["Close Session analytics"].1;
        assert!(
            close.width > 0.0
                && close.height > 0.0
                && close.x >= 0.0
                && close.y >= 0.0
                && close.x + close.width <= size.width + 0.5
                && close.y + close.height <= size.height + 0.5,
            "phase {phase}: pinned Close stays in the actual viewport {close:?} {size:?}"
        );
        assert_eq!(
            geometry.text.contains_key("Updating chart layout…"),
            phase == 1
        );
    }
}

#[test]
fn stale_opposite_theme_graphic_cannot_lower_the_native_bar_contrast() {
    let mut report = active_report();
    report.cameras.truncate(1);
    report.cameras[0].name = "Camera 镜头 👩‍🚀".to_owned();
    let mut renderer = crate::gui::destinations::layout_tests::renderer();
    for (_, palette) in PALETTES {
        let stale_palette = if palette.dark {
            &style::LIGHT
        } else {
            &style::DARK
        };
        let accepted = profile(396.0, stale_palette);
        let current = profile(396.0, palette);
        let graphic = render(&report, accepted).unwrap();
        let state = ViewState::default();
        let mut view = view(&report, &graphic, accepted, current, palette, &state);
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(396.0, 2000.0)),
        );
        let pixels = screenshot(
            &mut renderer,
            &view,
            &tree,
            &node,
            node.size(),
            node.bounds(),
            1.0,
        );
        let expected = rgb(data_ink(palette));
        assert!(contrast(expected, rgb(palette.base)) >= 4.5);
        assert_ne!(
            expected, accepted.colors.throughput,
            "fixture proves a stale data color would differ"
        );
        let y = (node.size().height - 17.0).floor() as usize;
        for x in 30..366 {
            assert_eq!(
                pixels[(y * 396 + x) * 4..][..3],
                expected,
                "current appearance determines actual native bar pixels"
            );
        }
    }
}
