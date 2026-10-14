//! Exercise the actual SVG widget and software rasterizer, with explicit
//! appearance palettes instead of changing process-wide accessibility settings.
use super::*;
use crate::analytics::{AnalyticsPoint, AnalyticsSnapshot, AnalyticsTotals, CameraAnalytics};
use crate::gui::{Message, prefs::UiScale, style};
use iced::advanced::{Layout, layout, renderer::Headless, widget::Tree};
use iced::advanced::{renderer::Renderer as _, svg::Renderer as _};
use iced::widget::svg;
use iced::{Color, Element, Point, Rectangle, Size, Theme, Transformation};

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

fn document(profile: Profile, report: &AnalyticsSnapshot) -> String {
    crate::analytics::render_svg_with_options(report, profile.options)
        .expect("the pure report renders without a session or database")
}

fn number(node: roxmltree::Node<'_, '_>, attribute: &str) -> f64 {
    node.attribute(attribute)
        .unwrap_or_else(|| panic!("missing {attribute} on {}", node.tag_name().name()))
        .parse()
        .unwrap()
}

fn caption_position(document: &roxmltree::Document<'_>, caption: &str) -> (f64, f64) {
    let node = document
        .descendants()
        .find(|node| node.has_tag_name("text") && node.text().unwrap_or_default().trim() == caption)
        .unwrap_or_else(|| panic!("missing chart caption {caption}"));
    assert!((number(node, "font-size") - 16.0).abs() < 0.001);
    (number(node, "x"), number(node, "y"))
}

fn color(value: &str) -> [u8; 3] {
    assert_eq!(value.len(), 7);
    assert!(value.starts_with('#'));
    [
        u8::from_str_radix(&value[1..3], 16).unwrap(),
        u8::from_str_radix(&value[3..5], 16).unwrap(),
        u8::from_str_radix(&value[5..7], 16).unwrap(),
    ]
}

fn luminance(color: [u8; 3]) -> f64 {
    let linear = color.map(|channel| {
        let channel = f64::from(channel) / 255.0;
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    });
    0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2]
}

fn contrast(foreground: [u8; 3], background: [u8; 3]) -> f64 {
    let foreground = luminance(foreground);
    let background = luminance(background);
    (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05)
}

#[test]
fn profile_floors_logical_width_and_keeps_interface_scale_out_of_the_svg() {
    assert_eq!(profile(400.9, &style::LIGHT).width, 400);
    assert_eq!(profile(12.0, &style::LIGHT).width, 180);
    assert_eq!(profile(9_000.0, &style::LIGHT).width, 2048);
    let logical_width = (640.0 / UiScale::Larger.factor() - 48.0).clamp(320.0, 1060.0) - 48.0;
    let larger = profile(logical_width, &style::LIGHT);
    assert_eq!(larger.width, 396);
    assert_eq!(larger.height, 1040);
    for (name, palette) in PALETTES {
        let normal = profile(400.0, palette);
        assert_eq!(
            normal.options.size(),
            (normal.width, normal.height),
            "{name}"
        );
        assert_ne!(
            normal.options,
            profile(
                400.0,
                if palette.dark {
                    &style::LIGHT
                } else {
                    &style::DARK
                }
            )
            .options,
            "appearance changes the chart profile"
        );
    }
}

#[test]
fn responsive_svg_keeps_readable_labels_panel_geometry_and_quantized_contrast() {
    let report = active_report();
    for width in WIDTHS {
        for (name, palette) in PALETTES {
            let profile = profile(width as f32, palette);
            let source = document(profile, &report);
            let doc = roxmltree::Document::parse(&source).unwrap();
            let root = doc.root_element();
            assert_eq!(root.attribute("role"), Some("img"));
            assert_eq!(
                root.attribute("viewBox"),
                Some(format!("0 0 {} {}", profile.width, profile.height).as_str())
            );
            assert!(!doc.descendants().any(|node| node.has_tag_name("script")));
            let background = color(
                doc.descendants()
                    .find(|node| node.has_tag_name("rect"))
                    .unwrap()
                    .attribute("fill")
                    .unwrap(),
            );
            assert_eq!(background, rgb(palette.base));
            let positions = [
                "Frame rate",
                "Throughput",
                "Missed frame rate",
                "Camera data volume",
            ]
            .map(|caption| caption_position(&doc, caption));
            if width < 840 {
                assert_eq!(profile.height, 1040);
                assert!(positions.windows(2).all(|pair| pair[0].1 < pair[1].1));
                assert!(
                    positions
                        .iter()
                        .all(|position| (position.0 - positions[0].0).abs() < 1.0)
                );
            } else {
                assert_eq!(profile.height, 520);
                assert_eq!(positions[0].1, positions[1].1);
                assert!((positions[2].1 - positions[3].1).abs() < 12.0);
                assert!(positions[0].0 < positions[1].0 && positions[2].0 < positions[3].0);
            }
            let mut camera_names = 0;
            let mut camera_values = 0;
            for node in doc.descendants().filter(|node| node.has_tag_name("text")) {
                let label = node.text().unwrap_or_default().trim();
                if label.is_empty() {
                    continue;
                }
                assert!(
                    number(node, "font-size") >= 12.0 - 0.001,
                    "{name}/{width}: {label}"
                );
                assert_eq!(node.attribute("opacity"), Some("1"));
                let ratio = contrast(color(node.attribute("fill").unwrap()), background);
                assert!(ratio >= 4.5, "{name}/{width}: {label} has contrast {ratio}");
                let x = number(node, "x");
                let y = number(node, "y");
                assert!(x >= 0.0 && x <= f64::from(profile.width));
                assert!(y >= 0.0 && y <= f64::from(profile.height));
                if label.starts_with("Camera ") && label != "Camera data volume" {
                    camera_names += 1;
                    assert!(
                        label.ends_with('…')
                            || report.cameras.iter().any(|camera| camera.name == label),
                        "camera names retain their identity or visibly mark elision"
                    );
                    assert!(y > positions[3].1);
                }
                if label.ends_with(" GiB") {
                    camera_values += 1;
                    assert_eq!(node.attribute("text-anchor"), Some("end"));
                    assert_eq!(x, f64::from(profile.width - 14));
                    assert!(y > positions[3].1);
                }
            }
            assert_eq!(camera_names, 5);
            assert_eq!(camera_values, 5);
            let data_lines: Vec<_> = doc
                .descendants()
                .filter(|node| {
                    node.has_tag_name("polyline")
                        && node
                            .attribute("stroke-width")
                            .is_some_and(|width| width == "2" || width == "3")
                })
                .collect();
            assert_eq!(data_lines.len(), 3);
            for line in data_lines {
                assert_eq!(
                    line.attribute("stroke-width"),
                    Some(if palette.increased() { "3" } else { "2" })
                );
                assert_eq!(line.attribute("opacity"), Some("1"));
                let ratio = contrast(color(line.attribute("stroke").unwrap()), background);
                assert!(ratio >= 3.0, "{name}/{width}: data line contrast {ratio}");
            }
            let axis = rgb(palette.control_border);
            assert!(contrast(axis, background) >= 3.0);
            assert!(doc.descendants().any(|node| {
                (node.has_tag_name("line") || node.has_tag_name("polyline"))
                    && node
                        .attribute("stroke")
                        .is_some_and(|value| color(value) == axis)
                    && node.attribute("opacity") == Some("1")
            }));
            for node in doc
                .descendants()
                .filter(|node| node.has_tag_name("polyline") || node.has_tag_name("polygon"))
            {
                for point in node.attribute("points").unwrap().split_whitespace() {
                    let (x, y) = point.split_once(',').unwrap();
                    let (x, y): (f64, f64) = (x.parse().unwrap(), y.parse().unwrap());
                    assert!(x >= 0.0 && x <= f64::from(profile.width));
                    assert!(y >= 0.0 && y <= f64::from(profile.height));
                }
            }
        }
    }
    for (standard, increased) in [
        (&style::LIGHT, &style::LIGHT_CONTRAST),
        (&style::DARK, &style::DARK_CONTRAST),
    ] {
        let background = rgb(standard.base);
        assert!(
            contrast(rgb(increased.secondary), background)
                > contrast(rgb(standard.secondary), background)
        );
        assert!(
            contrast(rgb(increased.control_border), background)
                > contrast(rgb(standard.control_border), background)
        );
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
    let mut encoder = png::Encoder::new(file, canvas.width as u32, canvas.height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(pixels)
        .unwrap();
}

#[test]
fn actual_svg_widget_preserves_intrinsic_size_and_software_pixels_obey_ancestor_clipping() {
    let report = active_report();
    let mut renderer = crate::gui::destinations::layout_tests::renderer();
    for width in WIDTHS.into_iter().chain([396]) {
        for (name, palette) in PALETTES {
            let profile = profile(width as f32, palette);
            let handle = svg::Handle::from_memory(document(profile, &report).into_bytes());
            assert_eq!(
                renderer.measure_svg(&handle),
                Size::new(profile.width, profile.height)
            );
            let size = Size::new(profile.width as f32, profile.height as f32);
            let canvas = size + Size::new(16.0, 16.0);
            let mut view: Element<'_, Message> =
                svg(handle).width(size.width).height(size.height).into();
            let mut tree = Tree::new(view.as_widget());
            let node = view
                .as_widget_mut()
                .layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, canvas),
                )
                .move_to(Point::new(8.0, 8.0));
            assert_eq!(
                node.size(),
                size,
                "the chart has no second aspect-ratio resize"
            );
            let full = node.bounds();
            let pixels = screenshot(&mut renderer, &view, &tree, &node, canvas, full, 1.0);
            assert_pixels_inside(&pixels, canvas, full, 1.0);
            let camera_origin = if width < 840 {
                Point::new(8.0, 788.0)
            } else {
                Point::new(8.0 + (width / 2) as f32, 268.0)
            };
            let background = rgb(palette.base);
            for row in 0..5 {
                // These strips contain camera text, above the value and bar.
                // This fails when SVG text is absent from the software raster,
                // even though the chart's background and series still paint.
                let left = camera_origin.x as u32 + 14;
                let right = profile.width + 8 - 14;
                let top = camera_origin.y as u32 + 40 + row * 42;
                let bottom = top + 14;
                let canvas_width = canvas.width as u32;
                let mut glyph_pixels = 0;
                for y in top..bottom {
                    for x in left..right {
                        let at = ((y * canvas_width + x) * 4) as usize;
                        if pixels[at..at + 3] != background {
                            glyph_pixels += 1;
                        }
                    }
                }
                assert!(
                    glyph_pixels > 15,
                    "{name}/{width}: camera row {row} shapes real text"
                );
            }
            artifact(&format!("analytics-{name}-{width}"), &pixels, canvas);
            if width == 400 && name == "dark" {
                let clip = Rectangle::new(Point::new(31.0, 67.0), Size::new(180.0, 113.0));
                for scale in UiScale::ALL {
                    let factor = scale.factor();
                    let pixels =
                        screenshot(&mut renderer, &view, &tree, &node, canvas, clip, factor);
                    assert_pixels_inside(&pixels, canvas, clip, factor);
                }
            }
        }
    }
}
