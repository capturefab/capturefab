//! Real dated Activity layout, native semantics and software glyph pixels.
//! This uses installed interface fonts and native date preferences as read;
//! it does not replace captions or modify locale, time zone or OS settings.
use super::*;
use crate::session::{LogEntry, LogTimestamp, SessionActivity};
use iced::advanced::{
    Layout, layout,
    renderer::{Headless, Renderer as _},
    widget::{
        Id, Operation, Tree,
        operation::{self, Scrollable},
    },
};
use iced::{Rectangle, Vector};

type TextState =
    iced::widget::text::State<<iced::Renderer as iced::advanced::text::Renderer>::Paragraph>;

#[derive(Default)]
struct Geometry {
    headings: Vec<(accessibility::NodeId, String, Rectangle)>,
    text: Vec<(String, Rectangle)>,
    commands: Vec<(String, Rectangle)>,
    copy: Option<(accessibility::NodeId, Rectangle)>,
    scroller: Option<(Rectangle, Rectangle)>,
}
impl Operation for Geometry {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn scrollable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        content: Rectangle,
        _: Vector,
        _: &mut dyn Scrollable,
    ) {
        if id == Some(&Id::new(ACTIVITY_BODY)) {
            self.scroller = Some((bounds, content));
        }
    }
    fn text(&mut self, _: Option<&Id>, bounds: Rectangle, value: &str) {
        self.text.push((value.to_owned(), bounds));
    }
    fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        let Some(control) = state.downcast_ref::<accessibility::Control<Message>>() else {
            return;
        };
        let Some(name) = control.node.label() else {
            return;
        };
        match control.node.role() {
            accessibility::Role::Heading => {
                self.headings.push((control.id, name.to_owned(), bounds))
            }
            accessibility::Role::Group if name.starts_with("Command ") => {
                self.commands.push((name.to_owned(), bounds))
            }
            _ if name == "Copy activity log" => self.copy = Some((control.id, bounds)),
            _ => {}
        }
    }
}

fn paragraphs<'a>(tree: &'a Tree, result: &mut Vec<&'a TextState>) {
    if tree.tag == iced::advanced::widget::tree::Tag::of::<TextState>() {
        result.push(tree.state.downcast_ref::<TextState>());
    }
    for child in &tree.children {
        paragraphs(child, result);
    }
}

fn fixture() -> Workbench {
    let mut app = Workbench::new(
        SessionHandle::new(),
        "activity-date-layout".into(),
        true,
        None,
    );
    let instant = chrono::DateTime::parse_from_rfc3339("2026-09-30T14:30:00.123Z")
        .unwrap()
        .timestamp_millis();
    // September exercises a long localized month. Deliberately retain a later
    // day followed by an earlier arrival; this panel is a journal, not a sort.
    app.snapshot.logs = [instant, instant + 86_400_000, instant - 86_400_000]
        .into_iter().enumerate().map(|(index, instant)| LogEntry {
            timestamp: LogTimestamp::new(instant, 14 * 3600),
            ..LogEntry::legacy("04:30:00", "warn", format!(
                "Diagnostic {index}: retained camera connection details remain readable when the date caption or message wraps."
            ))
        }).collect();
    app.snapshot.activity = [(703, instant), (701, instant + 86_400_000), (702, instant - 86_400_000)]
        .into_iter().map(|(id, instant)| -> SessionActivity {
            serde_json::from_value(serde_json::json!({
                "id":id,"origin":"client","actor":{"id":"date-layout-client","name":"Workflow participant","kind":"agent"},
                "operation":"capture","camera":"sim:0","feature":null,
                "label":format!("Capture {id}: retain the original command order and complete output details"),
                "state":"failed","submitted_at_ms":instant,"started_at_ms":instant,
                "finished_at_ms":instant,"detail":format!("/isolated/date-layout/camera-{id}/capture.png"),
                "file_count":0,"files":[],"job":null
            })).unwrap()
        }).collect();
    app
}

fn theme(name: &str, p: &Palette) -> Theme {
    Theme::custom(
        format!("Date layout {name}"),
        iced::theme::Palette {
            background: p.base,
            text: p.text,
            primary: p.accent_fill,
            success: p.live,
            warning: p.warn,
            danger: p.danger,
        },
    )
}

fn inside(inner: Rectangle, outer: Rectangle) -> bool {
    inner.width > 0.0
        && inner.height > 0.0
        && inner.x >= outer.x - 0.01
        && inner.y >= outer.y - 0.01
        && inner.x + inner.width <= outer.x + outer.width + 0.01
        && inner.y + inner.height <= outer.y + outer.height + 0.01
}

fn render(
    renderer: &mut iced::Renderer,
    view: &Element<'_, Message>,
    tree: &Tree,
    node: &layout::Node,
    size: Size,
    scale: f32,
    theme: &Theme,
) -> (Vec<u8>, Size<u32>) {
    let viewport = Rectangle::with_size(size);
    renderer.reset(viewport);
    view.as_widget().draw(
        tree,
        renderer,
        theme,
        &iced::advanced::renderer::Style {
            text_color: theme.palette().text,
        },
        Layout::new(node),
        iced::mouse::Cursor::Unavailable,
        &viewport,
    );
    let physical = Size::new(
        (size.width * scale).round() as u32,
        (size.height * scale).round() as u32,
    );
    (
        renderer.screenshot(physical, scale, theme.palette().background),
        physical,
    )
}

fn artifact(name: &str, pixels: &[u8], size: Size<u32>) {
    let Ok(directory) = std::env::var("CAPTUREFAB_LAYOUT_ARTIFACTS") else {
        return;
    };
    std::fs::create_dir_all(&directory).unwrap();
    let file = std::fs::File::create(std::path::Path::new(&directory).join(format!("{name}.png")))
        .unwrap();
    let mut encoder = png::Encoder::new(file, size.width, size.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(pixels)
        .unwrap();
}

fn glyph_pixels(
    pixels: &[u8],
    physical: Size<u32>,
    bounds: Rectangle,
    scale: f32,
    color: iced::Color,
) -> usize {
    let [r, g, b, _] = color.into_rgba8();
    let mut count = 0;
    for y in (bounds.y * scale).ceil() as u32..((bounds.y + bounds.height) * scale).floor() as u32 {
        for x in
            (bounds.x * scale).ceil() as u32..((bounds.x + bounds.width) * scale).floor() as u32
        {
            let at = ((y * physical.width + x) * 4) as usize;
            let pixel = &pixels[at..at + 3];
            count += usize::from(
                pixel[0].abs_diff(r) < 12 && pixel[1].abs_diff(g) < 12 && pixel[2].abs_diff(b) < 12,
            );
        }
    }
    count
}

#[test]
fn dated_activity_captions_fit_real_compact_panels_without_duplicate_native_text() {
    use iced::advanced::text::Paragraph;
    let mut renderer = crate::gui::destinations::layout_tests::renderer();
    let app = fixture();
    let palettes = [
        ("light", &style::LIGHT),
        ("dark", &style::DARK),
        ("light-contrast", &style::LIGHT_CONTRAST),
        ("dark-contrast", &style::DARK_CONTRAST),
    ];
    // Linux can publish a native locale result between the first two reads.
    // Compare one settled clock presentation, without changing preferences.
    let deadline = Instant::now() + Duration::from_secs(2);
    let (raw, command) = loop {
        let raw = time_format::log(&app.snapshot.logs[0]);
        let command = time_format::activity(&app.snapshot.activity[0]);
        if raw.compact == command.compact {
            break (raw, command);
        }
        assert!(
            Instant::now() < deadline,
            "the same instant settles on one clock format"
        );
        std::thread::yield_now();
    };
    assert_eq!(
        raw.compact, command.compact,
        "one instant has one displayed clock"
    );
    assert_eq!(
        raw.day.key, command.day.key,
        "captions and clocks share the viewer's day"
    );
    assert_eq!(raw.copy, command.copy);
    let mut tree = Tree::empty();
    let mut retained_ids = None;
    for scale in prefs::UiScale::ALL {
        for native_width in [360.0, 480.0, 900.0] {
            for (name, palette) in palettes {
                let size = Size::new(native_width / scale.factor(), 171.0);
                let mut view = app.activity(palette);
                tree.diff(view.as_widget());
                let node = view.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, size),
                );
                let mut geometry = Geometry::default();
                view.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut geometry,
                );
                let (viewport, content) = geometry.scroller.unwrap();
                assert_eq!(
                    geometry.headings.len(),
                    6,
                    "each source's three distinct days has a caption"
                );
                let ids = geometry
                    .headings
                    .iter()
                    .map(|(id, _, _)| *id)
                    .collect::<Vec<_>>();
                if let Some(previous) = &retained_ids {
                    assert_eq!(
                        &ids, previous,
                        "resize and appearance retain each date's native identity"
                    );
                }
                retained_ids = Some(ids);
                let mut native_text = Vec::new();
                paragraphs(&tree, &mut native_text);
                for (index, (_, label, bounds)) in geometry.headings.iter().enumerate() {
                    assert!(
                        bounds.width > 100.0
                            && bounds.x >= viewport.x
                            && bounds.x + bounds.width <= viewport.x + viewport.width - 10.0,
                        "{native_width}/{scale}/{name}: caption reserves the overlay scrollbar gutter"
                    );
                    let paragraph = native_text
                        .iter()
                        .find(|state| state.content() == label)
                        .unwrap();
                    let measured = paragraph.raw().min_bounds();
                    assert!(
                        measured.width <= bounds.width + 0.01
                            && measured.height <= bounds.height + 0.01,
                        "{native_width}/{scale}/{name}: every actual shaped caption line fits {measured:?} {bounds:?}"
                    );
                    let row = if index < 3 {
                        geometry
                            .text
                            .iter()
                            .find(|(text, _)| text == &app.snapshot.logs[index].message)
                            .unwrap()
                            .1
                    } else {
                        geometry.commands[index - 3].1
                    };
                    assert!(
                        row.y >= bounds.y + bounds.height + 2.99,
                        "a wrapped date never overlaps its following row"
                    );
                    assert_eq!(
                        geometry
                            .text
                            .iter()
                            .filter(|(text, _)| text == label)
                            .count(),
                        1
                    );
                }
                let raw_positions = app
                    .snapshot
                    .logs
                    .iter()
                    .map(|entry| {
                        geometry
                            .text
                            .iter()
                            .position(|(text, _)| text == &entry.message)
                            .unwrap()
                    })
                    .collect::<Vec<_>>();
                assert!(raw_positions.windows(2).all(|pair| pair[0] < pair[1]));
                assert_eq!(
                    geometry
                        .commands
                        .iter()
                        .map(|(name, _)| name.as_str())
                        .collect::<Vec<_>>(),
                    ["Command 703", "Command 701", "Command 702"]
                );
                let (copy_id, copy_bounds) = geometry.copy.unwrap();
                assert!(
                    inside(copy_bounds, Rectangle::with_size(size)),
                    "Copy is pinned inside the compact panel"
                );
                assert!(
                    copy_bounds.y + copy_bounds.height <= viewport.y,
                    "the scroll body cannot overlap Copy"
                );
                // Reveal the longest actual native caption. Anchor-bottom uses
                // an absolute distance from the bottom, so invert it once.
                let (target_id, target_label, target) = geometry
                    .headings
                    .iter()
                    .max_by_key(|(_, label, _)| label.chars().count())
                    .unwrap();
                let maximum = (content.height - viewport.height).max(0.0);
                let translation = (target.y - viewport.y - 4.0).floor().clamp(0.0, maximum);
                let mut reveal = operation::scrollable::scroll_to::<()>(
                    Id::new(ACTIVITY_BODY),
                    operation::scrollable::AbsoluteOffset {
                        x: None,
                        y: Some(maximum - translation),
                    },
                );
                view.as_widget_mut()
                    .operate(&mut tree, Layout::new(&node), &renderer, &mut reveal);
                crate::gui::destinations::layout_tests::prepare_artifact_frame(
                    &mut view, &mut tree, &node, &renderer, size,
                );
                let (native, _, _) = accessibility::snapshot_widget(
                    view.as_widget_mut(),
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                );
                for (_, label, _) in &geometry.headings {
                    assert_eq!(
                        native
                            .nodes
                            .iter()
                            .filter(|(_, node)| node.label() == Some(label.as_str()))
                            .count(),
                        1,
                        "a caption is one native Heading, without a duplicate plain-text node"
                    );
                }
                let caption = &native
                    .nodes
                    .iter()
                    .find(|(id, _)| id == target_id)
                    .unwrap()
                    .1;
                assert_eq!(caption.role(), accessibility::Role::Heading);
                assert!(caption.live().is_none());
                let b = caption.bounds().unwrap();
                let caption_bounds = Rectangle::new(
                    iced::Point::new(b.x0 as f32, b.y0 as f32),
                    Size::new(b.width() as f32, b.height() as f32),
                );
                assert!(
                    inside(caption_bounds, viewport),
                    "the full dated caption is visible after ordinary scrolling"
                );
                let copy = native
                    .nodes
                    .iter()
                    .find(|(id, _)| *id == copy_id)
                    .unwrap()
                    .1
                    .bounds()
                    .unwrap();
                assert!(
                    (copy.x0 - f64::from(copy_bounds.x)).abs() < 0.01
                        && (copy.y0 - f64::from(copy_bounds.y)).abs() < 0.01,
                    "scrolling dates leaves the pinned action in place"
                );
                let (pixels, physical) = render(
                    &mut renderer,
                    &view,
                    &tree,
                    &node,
                    size,
                    scale.factor(),
                    &theme(name, palette),
                );
                assert!(
                    glyph_pixels(
                        &pixels,
                        physical,
                        caption_bounds,
                        scale.factor(),
                        palette.secondary
                    ) > 20,
                    "the real native-date caption paints shaped glyphs: {target_label}"
                );
                let title = geometry
                    .text
                    .iter()
                    .find(|(text, _)| text == "Activity")
                    .unwrap()
                    .1;
                assert!(
                    glyph_pixels(&pixels, physical, title, scale.factor(), palette.text) > 20,
                    "the title inherits the real base foreground in every appearance"
                );
                artifact(
                    &format!("activity-dates-{name}-{native_width}-{scale}"),
                    &pixels,
                    physical,
                );
            }
        }
    }
}
