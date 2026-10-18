//! Pure chart fixtures avoid database initialization and exercise actual SVG.
use super::*;

#[test]
fn interface_camera_elision_keeps_combining_flags_and_joined_emoji_whole() {
    for (name, budget, expected) in [
        ("e\u{301} label", 3, "e\u{301}…"),
        ("e\u{301} label", 2, "…"),
        ("🇨🇦 camera", 3, "🇨🇦…"),
        ("👩‍🚀 camera", 4, "👩‍🚀…"),
        ("👩‍🚀 camera", 3, "…"),
        ("Camera 镜头", 100, "Camera 镜头"),
    ] {
        assert_eq!(interface_camera_name(name, budget), expected);
    }
}

fn fixture() -> AnalyticsSnapshot {
    AnalyticsSnapshot {
        retention_seconds: RETENTION_SECONDS,
        sample_interval_seconds: 1,
        started_at_ms: 1_000,
        collected_at_ms: 86_401_000,
        samples: 2,
        truncated: true,
        totals: AnalyticsTotals {
            frames: 200,
            dropped: 4,
            bytes: 1_048_576,
            lost: 4,
            preview_skipped: 0,
        },
        cameras: (0..5)
            .map(|index| CameraAnalytics {
                id: format!("sim:{index}"),
                name: format!("<& Wide camera 名称 {} with a very long name", index),
                frames: 40,
                dropped: 1,
                bytes: if index == 0 {
                    u64::MAX
                } else {
                    100_000 * index
                },
                fps: 25.0,
                bytes_per_second: 2048.0,
                drop_percent: 0.5,
                streaming: index == 0,
                stale: false,
                lost: 1,
                preview_skipped: 0,
                lost_percent: 0.5,
            })
            .collect(),
        points: [1_000, 86_401_000]
            .into_iter()
            .map(|timestamp_ms| AnalyticsPoint {
                timestamp_ms,
                frames: 100,
                dropped: 2,
                bytes: 1_048_576,
                fps: 25.0,
                bytes_per_second: 1e19,
                drop_percent: 2.0,
                lost: 2,
                lost_percent: 2.0,
            })
            .collect(),
    }
}

#[test]
fn interface_svg_preserves_readable_logical_fonts_and_responsive_panel_geometry() {
    for width in [180, 272, 400, 839, 840, 1012] {
        let options = SvgOptions::interface(width, SvgColors::default());
        let svg = render_svg_with_options(&fixture(), options).unwrap();
        let doc = roxmltree::Document::parse(&svg).unwrap();
        let (actual_width, height) = options.size();
        assert_eq!(
            doc.root_element().attribute("viewBox"),
            Some(format!("0 0 {actual_width} {height}").as_str())
        );
        assert!(
            doc.descendants()
                .filter(|node| node.has_tag_name("text"))
                .all(|node| {
                    node.attribute("font-size").unwrap().parse::<f64>().unwrap() >= 12.0 - 0.001
                })
        );
        let captions: Vec<_> = [
            "Frame rate",
            "Throughput",
            "Missed frame rate",
            "Camera data volume",
        ]
        .into_iter()
        .map(|caption| {
            let node = doc
                .descendants()
                .find(|node| {
                    node.has_tag_name("text") && node.text().map(str::trim) == Some(caption)
                })
                .unwrap();
            (
                node.attribute("x").unwrap().parse::<f64>().unwrap(),
                node.attribute("y").unwrap().parse::<f64>().unwrap(),
            )
        })
        .collect();
        if width < 840 {
            assert!(captions.windows(2).all(|pair| pair[0].1 < pair[1].1));
        } else {
            assert_eq!(captions[0].1, captions[1].1);
            assert!(captions[0].0 < captions[1].0);
            assert_eq!(captions[2].1, captions[3].1);
        }
        assert!(!doc.descendants().any(|node| node.has_tag_name("script")));
        assert!(svg.contains("Elapsed time"));
        assert!(svg.contains("17179869184.00 GiB"));
        for node in doc.descendants().filter(|node| node.has_tag_name("rect")) {
            let number = |name| node.attribute(name).unwrap().parse::<f64>().unwrap();
            assert!(number("x") >= 0.0 && number("y") >= 0.0);
            assert!(number("x") + number("width") <= actual_width as f64 + 0.001);
            assert!(number("y") + number("height") <= height as f64 + 0.001);
        }
        if let Some(directory) = std::env::var_os("CAPTUREFAB_CHART_ARTIFACTS") {
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                std::path::Path::new(&directory).join(format!("chart-{width}.svg")),
                svg,
            )
            .unwrap();
        }
    }
}

#[test]
fn interface_options_bound_invalid_widths_and_empty_reports_stay_inside_the_canvas() {
    assert_eq!(
        SvgOptions::interface(0, SvgColors::default()).size(),
        (180, 1040)
    );
    assert_eq!(
        SvgOptions::interface(u32::MAX, SvgColors::default()).size(),
        (2048, 520)
    );
    let mut report = fixture();
    report.cameras.clear();
    report.points.clear();
    report.totals = AnalyticsTotals::default();
    let svg =
        render_svg_with_options(&report, SvgOptions::interface(272, SvgColors::default())).unwrap();
    let doc = roxmltree::Document::parse(&svg).unwrap();
    assert!(svg.contains("No retained cameras"));
    for node in doc.descendants().filter(|node| node.has_tag_name("text")) {
        assert!(node.attribute("x").unwrap().parse::<u32>().unwrap() < 272);
        assert!(node.attribute("y").unwrap().parse::<u32>().unwrap() < 1040);
    }
}
