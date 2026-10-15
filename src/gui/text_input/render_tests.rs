//! Exercise actual iced editor drawing and software-shaped text, without a
//! second editor, measurement paragraph, synthetic focus, or duplicate text.
use super::*;
use crate::gui::{prefs::UiScale, style};
use iced::advanced::text::Paragraph as _;
use iced::{Background, Color, Font, Point, Transformation, keyboard, window};
use std::time::Instant;

type Paragraph = <iced::Renderer as text::Renderer>::Paragraph;

#[derive(Clone, Debug)]
struct DrawnQuad {
    bounds: Rectangle,
    visible: Option<Rectangle>,
    color: Background,
}

#[derive(Clone, Debug)]
struct DrawnText {
    position: Point,
    paragraph: usize,
    width: f32,
    graphemes: Vec<f32>,
    glyphs: Vec<Glyph>,
}

#[derive(Clone, Debug)]
struct Glyph {
    start: usize,
    end: usize,
    x: f32,
    width: f32,
    rtl: bool,
    id: u16,
}

struct Recorder {
    inner: iced::Renderer,
    quads: Vec<DrawnQuad>,
    text: Vec<DrawnText>,
    transforms: Vec<Transformation>,
    clips: Vec<Option<Rectangle>>,
}

impl Recorder {
    fn new() -> Self {
        Self {
            inner: crate::gui::destinations::layout_tests::renderer(),
            quads: Vec::new(),
            text: Vec::new(),
            transforms: vec![Transformation::IDENTITY],
            clips: Vec::new(),
        }
    }

    fn transform(&self) -> Transformation {
        *self.transforms.last().unwrap()
    }

    fn clip(&self, bounds: Rectangle) -> Option<Rectangle> {
        self.clips.last().map_or(Some(bounds), |clip| {
            clip.and_then(|clip| clip.intersection(&bounds))
        })
    }

    fn selection(&self) -> Option<&DrawnQuad> {
        self.quads.iter().find(
            |quad| matches!(quad.color, Background::Color(color) if (color.a - 0.3).abs() < 0.0001),
        )
    }

    fn selections(&self) -> impl Iterator<Item = &DrawnQuad> {
        self.quads.iter().filter(
            |quad| matches!(quad.color, Background::Color(color) if (color.a - 0.3).abs() < 0.0001),
        )
    }

    fn artifact(&mut self, name: &str, size: Size, p: &'static style::Palette) {
        use iced::advanced::renderer::Headless;
        let Ok(directory) = std::env::var("CAPTUREFAB_LAYOUT_ARTIFACTS") else {
            return;
        };
        let physical = Size::new(size.width.ceil() as u32, size.height.ceil() as u32);
        let pixels = self.inner.screenshot(physical, 1.0, p.base);
        std::fs::create_dir_all(&directory).unwrap();
        let file =
            std::fs::File::create(std::path::Path::new(&directory).join(format!("{name}.png")))
                .unwrap();
        let mut encoder = png::Encoder::new(file, physical.width, physical.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&pixels)
            .unwrap();
    }

    fn assert_pixels_inside_clip(&mut self, size: Size, clip: Rectangle) {
        use iced::advanced::renderer::Headless;
        let physical = Size::new(size.width.ceil() as u32, size.height.ceil() as u32);
        let pixels = self.inner.screenshot(physical, 1.0, Color::WHITE);
        for y in 0..physical.height {
            for x in 0..physical.width {
                // Include edge pixels touched by fractional physical bounds.
                if (x as f32) >= clip.x.floor()
                    && (x as f32) < (clip.x + clip.width).ceil()
                    && (y as f32) >= clip.y.floor()
                    && (y as f32) < (clip.y + clip.height).ceil()
                {
                    continue;
                }
                let at = ((y * physical.width + x) * 4) as usize;
                assert_eq!(
                    &pixels[at..at + 4],
                    &[255, 255, 255, 255],
                    "drawing escaped the ancestor clip at ({x}, {y})"
                );
            }
        }
    }
}

impl renderer::Renderer for Recorder {
    fn start_layer(&mut self, bounds: Rectangle) {
        // Match iced_graphics::layer::Stack::push_clip: a new layer replaces
        // prior clipping. Widgets must explicitly intersect their viewport.
        self.clips.push(Some(bounds * self.transform()));
        self.inner.start_layer(bounds);
    }

    fn end_layer(&mut self) {
        self.clips.pop();
        self.inner.end_layer();
    }

    fn start_transformation(&mut self, transformation: Transformation) {
        self.transforms.push(self.transform() * transformation);
        self.inner.start_transformation(transformation);
    }

    fn end_transformation(&mut self) {
        self.transforms.pop();
        self.inner.end_transformation();
    }

    fn fill_quad(&mut self, quad: renderer::Quad, background: impl Into<Background>) {
        let color = background.into();
        let bounds = quad.bounds * self.transform();
        self.quads.push(DrawnQuad {
            bounds,
            visible: self.clip(bounds),
            color,
        });
        self.inner.fill_quad(quad, color);
    }

    fn reset(&mut self, bounds: Rectangle) {
        self.quads.clear();
        self.text.clear();
        self.transforms = vec![Transformation::IDENTITY];
        self.clips = vec![Some(bounds)];
        self.inner.reset(bounds);
    }

    fn allocate_image(
        &mut self,
        handle: &iced::advanced::image::Handle,
        callback: impl FnOnce(Result<iced::advanced::image::Allocation, iced::advanced::image::Error>)
        + Send
        + 'static,
    ) {
        self.inner.allocate_image(handle, callback);
    }
}

impl text::Renderer for Recorder {
    type Font = Font;
    type Paragraph = Paragraph;
    type Editor = <iced::Renderer as text::Renderer>::Editor;

    const ICON_FONT: Font = <iced::Renderer as text::Renderer>::ICON_FONT;
    const CHECKMARK_ICON: char = <iced::Renderer as text::Renderer>::CHECKMARK_ICON;
    const ARROW_DOWN_ICON: char = <iced::Renderer as text::Renderer>::ARROW_DOWN_ICON;
    const SCROLL_UP_ICON: char = <iced::Renderer as text::Renderer>::SCROLL_UP_ICON;
    const SCROLL_DOWN_ICON: char = <iced::Renderer as text::Renderer>::SCROLL_DOWN_ICON;
    const SCROLL_LEFT_ICON: char = <iced::Renderer as text::Renderer>::SCROLL_LEFT_ICON;
    const SCROLL_RIGHT_ICON: char = <iced::Renderer as text::Renderer>::SCROLL_RIGHT_ICON;
    const ICED_LOGO: char = <iced::Renderer as text::Renderer>::ICED_LOGO;

    fn default_font(&self) -> Font {
        self.inner.default_font()
    }

    fn default_size(&self) -> Pixels {
        self.inner.default_size()
    }

    fn fill_paragraph(
        &mut self,
        paragraph: &Paragraph,
        position: Point,
        color: Color,
        bounds: Rectangle,
    ) {
        self.text.push(DrawnText {
            position: position * self.transform(),
            paragraph: std::ptr::from_ref(paragraph) as usize,
            width: paragraph.min_width(),
            graphemes: (0..=128)
                .map(|index| {
                    paragraph
                        .grapheme_position(0, index)
                        .unwrap_or(Point::ORIGIN)
                        .x
                })
                .collect(),
            glyphs: paragraph
                .buffer()
                .layout_runs()
                .flat_map(|run| run.glyphs.iter())
                .map(|glyph| Glyph {
                    start: glyph.start,
                    end: glyph.end,
                    x: glyph.x,
                    width: glyph.w,
                    rtl: glyph.level.is_rtl(),
                    id: glyph.glyph_id,
                })
                .collect(),
        });
        self.inner
            .fill_paragraph(paragraph, position, color, bounds);
    }

    fn fill_editor(
        &mut self,
        editor: &Self::Editor,
        position: Point,
        color: Color,
        bounds: Rectangle,
    ) {
        self.inner.fill_editor(editor, position, color, bounds);
    }

    fn fill_text(
        &mut self,
        text: text::Text<String, Font>,
        position: Point,
        color: Color,
        bounds: Rectangle,
    ) {
        self.inner.fill_text(text, position, color, bounds);
    }
}

fn editor(tree: &mut Tree) -> &mut text_input::State<Paragraph> {
    tree.children[0].state.downcast_mut()
}

fn update(
    input: &mut TextInput<'_, String, Recorder>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &Recorder,
    event: Event,
) -> bool {
    let mut messages = Vec::new();
    let mut shell = Shell::new(&mut messages);
    input.update(
        tree,
        &event,
        Layout::new(node),
        mouse::Cursor::Unavailable,
        renderer,
        &mut iced::advanced::clipboard::Null,
        &mut shell,
        &Rectangle::with_size(Size::new(360.0, 70.0)),
    );
    let requests_ime = !matches!(shell.input_method(), iced::advanced::InputMethod::Disabled);
    assert!(
        messages.is_empty(),
        "window and redraw events cannot edit the value"
    );
    requests_ime
}

fn redraw(
    input: &mut TextInput<'_, String, Recorder>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &Recorder,
) -> bool {
    update(
        input,
        tree,
        node,
        renderer,
        Event::Window(window::Event::RedrawRequested(Instant::now())),
    )
}

fn draw(
    input: &TextInput<'_, String, Recorder>,
    tree: &Tree,
    node: &layout::Node,
    renderer: &mut Recorder,
    active: bool,
    scale: f32,
    clip: Rectangle,
) {
    use renderer::Renderer as _;
    let viewport = Rectangle::with_size(Size::new(360.0 * scale, 70.0 * scale));
    renderer.reset(viewport);
    renderer.with_transformation(Transformation::scale(scale), |renderer| {
        renderer.with_layer(clip, |renderer| {
            Widget::draw(
                input,
                tree,
                renderer,
                &style::window_theme(false, active),
                &renderer::Style::default(),
                Layout::new(node),
                mouse::Cursor::Unavailable,
                &clip,
            );
        });
    });
    assert_eq!(
        renderer.text.len(),
        1,
        "the value paragraph is drawn exactly once"
    );
}

fn near(actual: f32, expected: f32) {
    assert!((actual - expected).abs() < 0.001, "{actual} != {expected}");
}

fn press_key(
    input: &mut TextInput<'_, String, Recorder>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &Recorder,
    named: keyboard::key::Named,
    modifiers: keyboard::Modifiers,
) -> Vec<String> {
    use keyboard::key::{Code, Named, Physical};
    update(
        input,
        tree,
        node,
        renderer,
        Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)),
    );
    let physical_key = Physical::Code(match named {
        Named::ArrowLeft => Code::ArrowLeft,
        Named::ArrowRight => Code::ArrowRight,
        Named::Home => Code::Home,
        Named::End => Code::End,
        Named::Backspace => Code::Backspace,
        Named::Delete => Code::Delete,
        _ => unreachable!(),
    });
    let mut messages = Vec::new();
    input.update(
        tree,
        &Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(named),
            modified_key: keyboard::Key::Named(named),
            physical_key,
            location: keyboard::Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        }),
        Layout::new(node),
        mouse::Cursor::Unavailable,
        renderer,
        &mut iced::advanced::clipboard::Null,
        &mut Shell::new(&mut messages),
        &node.bounds(),
    );
    redraw(input, tree, node, renderer);
    messages
}

fn click_at(
    input: &mut TextInput<'_, String, Recorder>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &Recorder,
    point: Point,
) {
    let mut messages = Vec::new();
    input.update(
        tree,
        &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
        Layout::new(node),
        mouse::Cursor::Available(point),
        renderer,
        &mut iced::advanced::clipboard::Null,
        &mut Shell::new(&mut messages),
        &node.bounds(),
    );
    assert!(messages.is_empty());
    update(
        input,
        tree,
        node,
        renderer,
        Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
    );
    redraw(input, tree, node, renderer);
}

#[test]
fn inactive_selection_retains_the_actual_paragraph_direction_offset_and_secure_geometry() {
    let mut renderer = Recorder::new();
    for (value, secure, width) in [
        (
            "A long output name with many words and characters.png",
            false,
            140.0,
        ),
        ("a🇨🇦e\u{301}👩‍🚀zz", false, 240.0),
        ("a🇨🇦e\u{301}👩‍🚀zz", true, 240.0),
    ] {
        let length = Value::new(value).len();
        for (start, end) in [(1, length), (length, 1)] {
            for alignment in [Alignment::Start, Alignment::Center, Alignment::End] {
                let mut input = TextInput::<String, Recorder>::new("Value", value)
                    .secure(secure)
                    .width(width)
                    .align_x(alignment)
                    .style(style::input)
                    .on_input(|value| value);
                let mut tree = Tree::new(&input as &dyn Widget<String, Theme, Recorder>);
                let node = Widget::layout(
                    &mut input,
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, Size::new(360.0, 70.0)),
                );
                let text_bounds = Layout::new(&node).children().next().unwrap().bounds();
                editor(&mut tree).focus();
                editor(&mut tree).select_range(start, end);
                assert!(redraw(&mut input, &mut tree, &node, &renderer));
                draw(
                    &input,
                    &tree,
                    &node,
                    &mut renderer,
                    true,
                    1.0,
                    node.bounds(),
                );
                let active_quad = renderer.selection().unwrap().bounds;
                let active_text = renderer.text[0].clone();
                let anchor_x = active_text.graphemes[start.min(end)];
                let focus_x = active_text.graphemes[end];
                let aligned = if active_text.width > text_bounds.width {
                    0.0
                } else {
                    match alignment {
                        Alignment::Start => 0.0,
                        Alignment::Center => (text_bounds.width - active_text.width) / 2.0,
                        Alignment::End => text_bounds.width - active_text.width,
                    }
                };
                let offset = (focus_x + 5.0 - text_bounds.width).max(0.0);
                near(active_quad.x, text_bounds.x + anchor_x + aligned - offset);
                near(
                    active_quad.width,
                    active_text.graphemes[start.max(end)] - anchor_x,
                );
                update(
                    &mut input,
                    &mut tree,
                    &node,
                    &renderer,
                    Event::Window(window::Event::Unfocused),
                );
                assert!(!redraw(&mut input, &mut tree, &node, &renderer));
                draw(
                    &input,
                    &tree,
                    &node,
                    &mut renderer,
                    false,
                    1.0,
                    node.bounds(),
                );
                assert_eq!(renderer.selection().unwrap().bounds, active_quad);
                assert_eq!(renderer.text[0].position, active_text.position);
                assert_eq!(renderer.text[0].paragraph, active_text.paragraph);
                assert_eq!(
                    renderer.selection().unwrap().color,
                    style::input(&style::window_theme(false, false), Status::Active)
                        .selection
                        .into()
                );
                assert!(
                    editor(&mut tree).is_focused(),
                    "deactivation retains real editor focus"
                );
                assert_eq!(
                    editor(&mut tree).cursor().state(&Value::new(value)),
                    cursor::State::Selection { start, end }
                );
                update(
                    &mut input,
                    &mut tree,
                    &node,
                    &renderer,
                    Event::Window(window::Event::Focused),
                );
                assert!(redraw(&mut input, &mut tree, &node, &renderer));
                draw(
                    &input,
                    &tree,
                    &node,
                    &mut renderer,
                    true,
                    1.0,
                    node.bounds(),
                );
                assert_eq!(renderer.selection().unwrap().bounds, active_quad);
            }
        }
    }
}

#[test]
fn inactive_selection_respects_ancestor_clipping_and_interface_scale_once() {
    let mut renderer = Recorder::new();
    let value = "A long recording file name with many selected characters.mp4";
    let mut input = TextInput::<String, Recorder>::new("Output", value)
        .width(220.0)
        .style(style::input)
        .on_input(|value| value);
    let mut tree = Tree::new(&input as &dyn Widget<String, Theme, Recorder>);
    let node = Widget::layout(
        &mut input,
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(360.0, 70.0)),
    )
    .move_to(Point::new(20.0, 20.0));
    let text_bounds = Layout::new(&node).children().next().unwrap().bounds();
    editor(&mut tree).focus();
    editor(&mut tree).select_all();
    for active in [false, true] {
        update(
            &mut input,
            &mut tree,
            &node,
            &renderer,
            Event::Window(if active {
                window::Event::Focused
            } else {
                window::Event::Unfocused
            }),
        );
        assert_eq!(redraw(&mut input, &mut tree, &node, &renderer), active);
        let clip = Rectangle::new(Point::new(35.0, 22.0), Size::new(145.0, 13.0));
        for scale in UiScale::ALL {
            let scale = scale.factor();
            draw(&input, &tree, &node, &mut renderer, active, scale, clip);
            let quad = renderer.selection().unwrap();
            let visible = quad
                .visible
                .expect("part of the retained selection is visible");
            let clipped = (clip * Transformation::scale(scale))
                .intersection(&(text_bounds * Transformation::scale(scale)))
                .unwrap();
            // Rectangle::contains intentionally excludes bottom/right edges.
            // Compare the complete clipped rectangle instead of a boundary point.
            let expected = quad.bounds.intersection(&clipped).unwrap();
            near(visible.x, expected.x);
            near(visible.y, expected.y);
            near(visible.width, expected.width);
            near(visible.height, expected.height);
            near(quad.bounds.height, text_bounds.height * scale);
            renderer.assert_pixels_inside_clip(
                Size::new(360.0 * scale, 70.0 * scale),
                clip * Transformation::scale(scale),
            );
            renderer.artifact(
                &format!(
                    "{}-selection-{scale}",
                    if active { "active" } else { "inactive" }
                ),
                Size::new(360.0 * scale, 70.0 * scale),
                style::Palette::for_window(false, active),
            );
        }
    }
}

#[test]
fn inactive_selection_opt_in_preserves_default_behavior_and_never_draws_an_inactive_caret() {
    let mut renderer = Recorder::new();
    for enabled in [false, true] {
        let mut input = TextInput::<String, Recorder>::new("Value", "abcdef")
            .style(style::input)
            .on_input(|value| value);
        if !enabled {
            // A bare toolkit caller uses the unchanged default; only the
            // app's wrapper opts in. Do not set false explicitly here.
            input.inner = text_input::TextInput::new("Value", "abcdef")
                .style(style::input)
                .on_input(|value| value);
        }
        let mut tree = Tree::new(&input as &dyn Widget<String, Theme, Recorder>);
        let node = Widget::layout(
            &mut input,
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(240.0, 40.0)),
        );
        editor(&mut tree).focus();
        editor(&mut tree).select_range(1, 4);
        update(
            &mut input,
            &mut tree,
            &node,
            &renderer,
            Event::Window(window::Event::Unfocused),
        );
        assert!(!redraw(&mut input, &mut tree, &node, &renderer));
        draw(
            &input,
            &tree,
            &node,
            &mut renderer,
            false,
            1.0,
            node.bounds(),
        );
        assert_eq!(renderer.selection().is_some(), enabled);
        editor(&mut tree).select_range(2, 2);
        draw(
            &input,
            &tree,
            &node,
            &mut renderer,
            false,
            1.0,
            node.bounds(),
        );
        assert_eq!(
            renderer.quads.len(),
            1,
            "collapsed inactive selection has only its field, no caret"
        );
        editor(&mut tree).select_range(1, 4);
        editor(&mut tree).unfocus();
        draw(
            &input,
            &tree,
            &node,
            &mut renderer,
            false,
            1.0,
            node.bounds(),
        );
        assert!(renderer.selection().is_none());
    }
    let mut input = TextInput::<String, Recorder>::new("Value", "abcdef")
        .style(style::input)
        .on_input(|value| value);
    let mut tree = Tree::new(&input as &dyn Widget<String, Theme, Recorder>);
    editor(&mut tree).focus();
    editor(&mut tree).select_all();
    input = TextInput::new("Value", "abcdef").style(style::input);
    input.diff(&mut tree);
    let node = Widget::layout(
        &mut input,
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(240.0, 40.0)),
    );
    assert!(!redraw(&mut input, &mut tree, &node, &renderer));
    draw(
        &input,
        &tree,
        &node,
        &mut renderer,
        false,
        1.0,
        node.bounds(),
    );
    assert!(
        renderer.selection().is_none(),
        "disabled inputs have no retained highlight"
    );
}

/// Compare actual drawn quads with the *cached shaper's* UTF-8 glyph clusters,
/// independently of the application's grapheme-cell/interval implementation.
fn assert_glyph_membership(
    renderer: &Recorder,
    value: &str,
    start: usize,
    end: usize,
    text_bounds: Rectangle,
    scale: f32,
    require_gap: bool,
) {
    use unicode_segmentation::UnicodeSegmentation;
    let bytes = value
        .grapheme_indices(true)
        .map(|(byte, _)| byte)
        .chain(std::iter::once(value.len()))
        .collect::<Vec<_>>();
    let selected = bytes[start.min(end)]..bytes[start.max(end)];
    let text = &renderer.text[0];
    let quads = renderer.selections().collect::<Vec<_>>();
    assert!(!quads.is_empty());
    assert!(quads.iter().all(|quad| quad.bounds.width > 0.0));
    let left = quads
        .iter()
        .map(|quad| quad.bounds.x)
        .fold(f32::INFINITY, f32::min);
    let right = quads
        .iter()
        .map(|quad| quad.bounds.x + quad.bounds.width)
        .fold(f32::NEG_INFINITY, f32::max);
    let mut unselected_gap = 0;
    let mut visible_selected = 0;
    for glyph in &text.glyphs {
        if glyph.width <= 0.0 {
            continue;
        }
        assert_ne!(glyph.id, 0, "the test's actual script glyph exists");
        let center = Point::new(
            text.position.x + (glyph.x + glyph.width / 2.0) * scale,
            (text_bounds.y + text_bounds.height / 2.0) * scale,
        );
        if !(text_bounds * Transformation::scale(scale)).contains(center) {
            continue;
        }
        let expected = glyph.start < selected.end && glyph.end > selected.start;
        let painted = quads
            .iter()
            .any(|quad| quad.visible.is_some_and(|bounds| bounds.contains(center)));
        assert_eq!(
            painted, expected,
            "selection membership of cached glyph {glyph:?}"
        );
        visible_selected += usize::from(expected);
        if !expected && center.x > left && center.x < right {
            unselected_gap += 1;
        }
    }
    assert!(visible_selected > 0);
    if require_gap {
        assert!(
            unselected_gap > 0,
            "a real unselected glyph separates selected visual ranges"
        );
    }
}

#[test]
fn mixed_direction_selection_paints_separate_visual_ranges_and_retains_real_shape_when_inactive() {
    let mut renderer = Recorder::new();
    for value in ["abc אבג def", "אבג abc דהו"] {
        for (start, end) in [(1, 5), (5, 1)] {
            let mut input = TextInput::<String, Recorder>::new("Value", value)
                .width(240)
                .style(style::input)
                .on_input(|value| value);
            let mut tree = Tree::new(&input as &dyn Widget<String, Theme, Recorder>);
            let node = Widget::layout(
                &mut input,
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(360.0, 70.0)),
            );
            let text_bounds = Layout::new(&node).children().next().unwrap().bounds();
            editor(&mut tree).focus();
            editor(&mut tree).select_range(start, end);
            redraw(&mut input, &mut tree, &node, &renderer);
            draw(
                &input,
                &tree,
                &node,
                &mut renderer,
                true,
                1.0,
                node.bounds(),
            );
            assert!(
                renderer.selections().count() >= 2,
                "a logical range has disconnected visual regions: {value}"
            );
            assert_glyph_membership(&renderer, value, start, end, text_bounds, 1.0, true);
            let active = renderer
                .selections()
                .map(|quad| quad.bounds)
                .collect::<Vec<_>>();
            let paragraph = renderer.text[0].paragraph;
            let origin = input
                .inner
                .text_origin(&tree.children[0], Layout::new(&node));
            assert_eq!(
                origin, renderer.text[0].position,
                "public origin is the actual paragraph origin"
            );
            update(
                &mut input,
                &mut tree,
                &node,
                &renderer,
                Event::Window(window::Event::Unfocused),
            );
            assert!(!redraw(&mut input, &mut tree, &node, &renderer));
            draw(
                &input,
                &tree,
                &node,
                &mut renderer,
                false,
                1.0,
                node.bounds(),
            );
            assert_eq!(
                renderer
                    .selections()
                    .map(|quad| quad.bounds)
                    .collect::<Vec<_>>(),
                active
            );
            assert_eq!(
                renderer.text[0].paragraph, paragraph,
                "the exact cached paragraph is reused"
            );
            assert_eq!(renderer.text[0].position, origin);
            assert_glyph_membership(&renderer, value, start, end, text_bounds, 1.0, true);
            assert!(renderer.selections().all(|quad| {
                quad.color
                    == style::input(&style::window_theme(false, false), Status::Active)
                        .selection
                        .into()
            }));
            assert_eq!(
                editor(&mut tree).cursor().state(&Value::new(value)),
                cursor::State::Selection { start, end }
            );
            renderer.artifact(
                &format!(
                    "bidi-{}-{start}-{end}",
                    if value.starts_with('a') { "ltr" } else { "rtl" }
                ),
                Size::new(360.0, 70.0),
                style::Palette::for_window(false, false),
            );
        }
    }
}

fn ime_cursor(
    input: &mut TextInput<'_, String, Recorder>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &Recorder,
) -> Option<Rectangle> {
    let mut messages = Vec::new();
    let mut shell = Shell::new(&mut messages);
    input.update(
        tree,
        &Event::Window(window::Event::RedrawRequested(Instant::now())),
        Layout::new(node),
        mouse::Cursor::Unavailable,
        renderer,
        &mut iced::advanced::clipboard::Null,
        &mut shell,
        &node.bounds(),
    );
    match shell.input_method() {
        iced::advanced::InputMethod::Enabled { cursor, .. } => Some(*cursor),
        iced::advanced::InputMethod::Disabled => None,
    }
}

#[test]
fn reversed_bidi_focus_controls_scroll_pointer_conversion_and_ime_anchor() {
    use unicode_segmentation::UnicodeSegmentation;
    let mut renderer = Recorder::new();
    let value = "abc אבג def";
    let mut origins = Vec::new();
    for (start, end) in [(1, 5), (5, 1)] {
        let mut input = TextInput::<String, Recorder>::new("Value", value)
            .width(40)
            .style(style::input)
            .on_input(|value| value);
        let mut tree = Tree::new(&input as &dyn Widget<String, Theme, Recorder>);
        let node = Widget::layout(
            &mut input,
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(360.0, 70.0)),
        );
        let text_bounds = Layout::new(&node).children().next().unwrap().bounds();
        editor(&mut tree).focus();
        editor(&mut tree).select_range(start, end);
        redraw(&mut input, &mut tree, &node, &renderer);
        draw(
            &input,
            &tree,
            &node,
            &mut renderer,
            true,
            1.0,
            node.bounds(),
        );
        let origin = renderer.text[0].position;
        origins.push(origin.x);
        assert_eq!(
            input
                .inner
                .text_origin(&tree.children[0], Layout::new(&node)),
            origin
        );
        assert_glyph_membership(&renderer, value, start, end, text_bounds, 1.0, false);
        // Basic Hebrew/Latin test clusters each represent a full grapheme, so
        // this endpoint oracle needs neither proportional ligature fallback
        // nor the production selection adapter.
        let endpoint = renderer.text[0]
            .glyphs
            .iter()
            .find(|glyph| {
                let index = value[..glyph.start].graphemes(true).count();
                if start < end {
                    index + 1 == end
                } else {
                    index == end
                }
            })
            .unwrap();
        let focus = if (start < end) != endpoint.rtl {
            endpoint.x + endpoint.width
        } else {
            endpoint.x
        };
        let candidate = ime_cursor(&mut input, &mut tree, &node, &renderer).unwrap();
        assert!(
            (candidate.x - (origin.x + focus)).abs() < 1.001,
            "IME uses the same logical focus endpoint and scroll"
        );
        assert!(
            candidate.x >= text_bounds.x - 1.0 && candidate.x <= text_bounds.x + text_bounds.width,
            "focus is visible"
        );
        let glyph = renderer.text[0]
            .glyphs
            .iter()
            .find(|glyph| {
                let x = origin.x + glyph.x + glyph.width * 0.25;
                glyph.width > 1.0
                    && x > text_bounds.x + 1.0
                    && x < text_bounds.x + text_bounds.width - 1.0
            })
            .unwrap()
            .clone();
        let point = Point::new(
            origin.x + glyph.x + glyph.width * 0.25,
            text_bounds.y + text_bounds.height / 2.0,
        );
        let byte = if glyph.rtl { glyph.end } else { glyph.start };
        let expected = value[..byte].graphemes(true).count();
        let mut messages = Vec::new();
        input.update(
            &mut tree,
            &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Layout::new(&node),
            mouse::Cursor::Available(point),
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &node.bounds(),
        );
        assert!(
            messages.is_empty(),
            "pointer selection doesn't edit the value"
        );
        assert_eq!(
            editor(&mut tree).cursor().state(&Value::new(value)),
            cursor::State::Index(expected),
            "pointer conversion subtracts the same horizontal scroll"
        );
        update(
            &mut input,
            &mut tree,
            &node,
            &renderer,
            Event::Window(window::Event::Unfocused),
        );
        assert!(ime_cursor(&mut input, &mut tree, &node, &renderer).is_none());
    }
    assert!(
        (origins[0] - origins[1]).abs() > 1.0,
        "reversing the logical focus changes scrolling, not the selected characters"
    );
}

#[test]
fn pure_rtl_and_disconnected_selection_obey_real_ancestor_clip_at_each_interface_scale() {
    let mut renderer = Recorder::new();
    for (value, start, end) in [("אבגדהוזחטיכלמנסעפצקרשת", 1, 8), ("abc אבג def", 1, 5)]
    {
        let mut input = TextInput::<String, Recorder>::new("Value", value)
            .width(90)
            .style(style::input)
            .on_input(|value| value);
        let mut tree = Tree::new(&input as &dyn Widget<String, Theme, Recorder>);
        let node = Widget::layout(
            &mut input,
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(360.0, 70.0)),
        );
        let text_bounds = Layout::new(&node).children().next().unwrap().bounds();
        editor(&mut tree).focus();
        editor(&mut tree).select_range(start, end);
        update(
            &mut input,
            &mut tree,
            &node,
            &renderer,
            Event::Window(window::Event::Unfocused),
        );
        for scale in UiScale::ALL {
            let factor = scale.factor();
            let clip = Rectangle::new(Point::new(11.0, 3.0), Size::new(64.0, 18.0));
            draw(&input, &tree, &node, &mut renderer, false, factor, clip);
            assert!(
                renderer
                    .selections()
                    .all(|quad| quad.visible.is_none_or(|bounds| {
                        let expected = clip * Transformation::scale(factor);
                        bounds.x >= expected.x
                            && bounds.y >= expected.y
                            && bounds.x + bounds.width <= expected.x + expected.width + 0.01
                            && bounds.y + bounds.height <= expected.y + expected.height + 0.01
                    }))
            );
            renderer.assert_pixels_inside_clip(
                Size::new(360.0 * factor, 70.0 * factor),
                clip * Transformation::scale(factor),
            );
            assert!(
                renderer.text[0]
                    .glyphs
                    .iter()
                    .any(|glyph| glyph.rtl && glyph.id != 0)
            );
            assert_eq!(editor(&mut tree).paragraph_text(), value);
            // Text is unscaled in widget state; only the renderer transforms it.
            let origin = input
                .inner
                .text_origin(&tree.children[0], Layout::new(&node));
            near(renderer.text[0].position.x, origin.x * factor);
            near(renderer.text[0].position.y, origin.y * factor);
            assert!(text_bounds.width > 0.0);
        }
    }
}

#[test]
fn actual_arabic_ligature_pointer_hits_share_the_proportional_rtl_grapheme_cells() {
    use unicode_segmentation::UnicodeSegmentation;
    let mut renderer = Recorder::new();
    let value = "abc لا xyz";
    let mut input = TextInput::<String, Recorder>::new("Value", value)
        .width(240)
        .style(style::input)
        .on_input(|value| value);
    let mut tree = Tree::new(&input as &dyn Widget<String, Theme, Recorder>);
    let node = Widget::layout(
        &mut input,
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(360.0, 70.0)),
    );
    editor(&mut tree).focus();
    redraw(&mut input, &mut tree, &node, &renderer);
    draw(
        &input,
        &tree,
        &node,
        &mut renderer,
        true,
        1.0,
        node.bounds(),
    );
    let glyph = renderer.text[0]
        .glyphs
        .iter()
        .find(|glyph| {
            glyph.rtl
                && glyph.width > 0.0
                && value[glyph.start..glyph.end].graphemes(true).count() >= 2
        })
        .expect("the installed Arabic fallback font shapes a real multi-grapheme Lam-Alef ligature")
        .clone();
    assert_ne!(glyph.id, 0);
    let origin = renderer.text[0].position;
    let first = value[..glyph.start].graphemes(true).count();
    let count = value[glyph.start..glyph.end].graphemes(true).count();
    let advance = glyph.width / count as f32;
    let bounds = Layout::new(&node).children().next().unwrap().bounds();
    for logical in 0..count {
        for fraction in [0.25, 0.75] {
            // This oracle comes directly from the actual shaped ligature's
            // advance and RTL level, not the production cell adapter. Exact
            // font GDEF caret positions remain outside this fallback's claim.
            let x =
                origin.x + glyph.x + (count - logical - 1) as f32 * advance + fraction * advance;
            let point = Point::new(x, bounds.y + bounds.height / 2.0);
            let mut messages = Vec::new();
            input.update(
                &mut tree,
                &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                Layout::new(&node),
                mouse::Cursor::Available(point),
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &node.bounds(),
            );
            assert!(messages.is_empty());
            assert_eq!(
                editor(&mut tree).cursor().state(&Value::new(value)),
                cursor::State::Index(first + logical + usize::from(fraction < 0.5)),
                "both halves of each logical RTL subcluster use the drawn geometry"
            );
            // Release and reset the click tracker so the next nearby sample
            // is an independent single click rather than a word selection.
            update(
                &mut input,
                &mut tree,
                &node,
                &renderer,
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            );
            editor(&mut tree).unfocus();
            tree = Tree::new(&input as &dyn Widget<String, Theme, Recorder>);
            let _ = Widget::layout(
                &mut input,
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(360.0, 70.0)),
            );
            editor(&mut tree).focus();
        }
    }
}

#[test]
fn multiline_values_keep_the_full_cursor_and_exact_upstream_geometry_fallback() {
    use super::selection::ParagraphGeometry;
    let mut renderer = Recorder::new();
    let value = "abc\nאבג def";
    let length = Value::new(value).len();
    for (start, end) in [(1, length), (length, 1)] {
        let mut input = TextInput::<String, Recorder>::new("Value", value)
            .width(48)
            .style(style::input)
            .on_input(|value| value);
        let mut tree = Tree::new(&input as &dyn Widget<String, Theme, Recorder>);
        let node = Widget::layout(
            &mut input,
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(360.0, 70.0)),
        );
        editor(&mut tree).focus();
        editor(&mut tree).select_range(start, end);
        let selected = cursor::State::Selection { start, end };
        let paragraph = editor(&mut tree).paragraph();
        assert_eq!(paragraph.buffer().lines.len(), 2);
        assert_eq!(
            paragraph.buffer().layout_runs().count(),
            1,
            "the field's single-line height hides the second buffer line"
        );
        assert!(paragraph.visual_geometry(selected).is_none());
        assert!(paragraph.visual_cells().is_none());
        assert_eq!(
            editor(&mut tree).cursor().state(&Value::new(value)),
            selected,
            "falling back never truncates the logical focus to the visible line"
        );
        draw(
            &input,
            &tree,
            &node,
            &mut renderer,
            true,
            1.0,
            node.bounds(),
        );
        let origin = renderer.text[0].position;
        let quads = renderer
            .selections()
            .map(|quad| quad.bounds)
            .collect::<Vec<_>>();
        assert_eq!(renderer.text.len(), 1, "one cached paragraph draw");
        // The same real editor State and paragraph are drawn by a bare toolkit
        // input with no geometry callback. This tests the actual fallback,
        // rather than recreating its endpoint/offset calculations in the test.
        input.inner = text_input::TextInput::new("Value", value)
            .show_unfocused_selection(true)
            .width(48)
            .style(style::input)
            .on_input(|value| value);
        draw(
            &input,
            &tree,
            &node,
            &mut renderer,
            true,
            1.0,
            node.bounds(),
        );
        assert_eq!(renderer.text[0].position, origin);
        assert_eq!(
            renderer
                .selections()
                .map(|quad| quad.bounds)
                .collect::<Vec<_>>(),
            quads
        );
        assert_eq!(
            editor(&mut tree).cursor().state(&Value::new(value)),
            selected
        );
        assert_eq!(editor(&mut tree).paragraph_text(), value);
    }
}

#[test]
fn clipped_wrapped_paragraphs_reject_incomplete_visible_grapheme_coverage() {
    use super::selection::ParagraphGeometry;
    use unicode_segmentation::UnicodeSegmentation;
    let renderer = Recorder::new();
    let value = "abc אבג def ghi jkl";
    let paragraph = Paragraph::with_text(text::Text {
        content: value,
        bounds: Size::new(32.0, 1.0),
        size: Pixels(16.0),
        line_height: text::LineHeight::Absolute(Pixels(20.0)),
        font: text::Renderer::default_font(&renderer),
        align_x: text::Alignment::Left,
        align_y: iced::alignment::Vertical::Top,
        shaping: text::Shaping::Advanced,
        wrapping: text::Wrapping::WordOrGlyph,
    });
    assert_eq!(paragraph.buffer().lines.len(), 1);
    assert!(paragraph.buffer().lines[0].layout_opt().unwrap().len() > 1);
    let runs = paragraph.buffer().layout_runs().collect::<Vec<_>>();
    assert_eq!(runs.len(), 1, "only the first wrapped line is visible");
    assert!(
        runs[0].glyphs.iter().map(|glyph| glyph.end).max().unwrap() < value.len(),
        "the real visible layout cannot describe the full source"
    );
    assert!(paragraph.visual_cells().is_none());
    assert!(
        paragraph
            .visual_geometry(cursor::State::Selection {
                start: 0,
                end: value.graphemes(true).count(),
            })
            .is_none()
    );
}

#[test]
fn visual_arrows_follow_actual_ltr_and_rtl_runs_instead_of_logical_index_order() {
    use keyboard::key::Named;
    let mut renderer = Recorder::new();
    // These sequences are independent run-transition oracles: the embedded
    // opposite-direction word is traversed in reverse logical order. They do
    // not call the production caret-position or movement helpers.
    for (value, forward, backward) in [
        ("abc אבג def", Named::ArrowRight, Named::ArrowLeft),
        ("abc אבג e\u{301}🇨🇦👩‍🚀", Named::ArrowRight, Named::ArrowLeft),
        ("אבג abc דהו", Named::ArrowLeft, Named::ArrowRight),
    ] {
        let mut input = TextInput::<String, Recorder>::new("Value", value)
            .width(300)
            .style(style::input)
            .on_input(|value| value);
        let mut tree = Tree::new(&input as &dyn Widget<String, Theme, Recorder>);
        let node = Widget::layout(
            &mut input,
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(360.0, 70.0)),
        );
        editor(&mut tree).focus();
        editor(&mut tree).move_cursor_to_front();
        // Native IME support being enabled is not an active composition. Many
        // platforms keep it opened throughout ordinary keyboard editing.
        update(
            &mut input,
            &mut tree,
            &node,
            &renderer,
            Event::InputMethod(iced::advanced::input_method::Event::Opened),
        );
        redraw(&mut input, &mut tree, &node, &renderer);
        draw(
            &input,
            &tree,
            &node,
            &mut renderer,
            true,
            1.0,
            node.bounds(),
        );
        assert!(
            renderer.text[0]
                .glyphs
                .iter()
                .any(|glyph| glyph.rtl && glyph.id != 0)
        );
        assert!(
            renderer.text[0]
                .glyphs
                .iter()
                .any(|glyph| !glyph.rtl && glyph.id != 0)
        );
        let mut expected = vec![0, 1, 2, 3, 4, 6, 5, 7];
        expected.extend(8..=Value::new(value).len());
        let mut previous = ime_cursor(&mut input, &mut tree, &node, &renderer)
            .unwrap()
            .x;
        for index in expected.iter().copied().skip(1) {
            assert!(
                press_key(
                    &mut input,
                    &mut tree,
                    &node,
                    &renderer,
                    forward,
                    keyboard::Modifiers::empty()
                )
                .is_empty()
            );
            assert_eq!(
                editor(&mut tree).cursor().state(&Value::new(value)),
                cursor::State::Index(index)
            );
            assert_eq!(
                editor(&mut tree).visual_caret().unwrap().affinity,
                text_input::CaretAffinity::Strong
            );
            let x = ime_cursor(&mut input, &mut tree, &node, &renderer)
                .unwrap()
                .x;
            assert!(
                if forward == Named::ArrowRight {
                    x > previous
                } else {
                    x < previous
                },
                "each actual visual stop moves in the requested screen direction"
            );
            previous = x;
        }
        for index in expected.iter().copied().rev().skip(1) {
            assert!(
                press_key(
                    &mut input,
                    &mut tree,
                    &node,
                    &renderer,
                    backward,
                    keyboard::Modifiers::empty()
                )
                .is_empty()
            );
            assert_eq!(
                editor(&mut tree).cursor().state(&Value::new(value)),
                cursor::State::Index(index)
            );
        }
        assert_eq!(editor(&mut tree).paragraph_text(), value);
    }
}

#[test]
fn split_boundary_pointer_affinity_survives_shift_reversal_and_window_activation() {
    use keyboard::key::Named;
    use text_input::CaretAffinity;
    let mut renderer = Recorder::new();
    let value = "abc אבג def";
    let mut positions = Vec::new();
    for weak in [false, true] {
        let mut input = TextInput::<String, Recorder>::new("Value", value)
            .width(300)
            .style(style::input)
            .on_input(|value| value);
        let mut tree = Tree::new(&input as &dyn Widget<String, Theme, Recorder>);
        let node = Widget::layout(
            &mut input,
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(360.0, 70.0)),
        );
        editor(&mut tree).focus();
        redraw(&mut input, &mut tree, &node, &renderer);
        draw(
            &input,
            &tree,
            &node,
            &mut renderer,
            true,
            1.0,
            node.bounds(),
        );
        let origin = renderer.text[0].position;
        let glyph = renderer.text[0]
            .glyphs
            .iter()
            .find(|glyph| {
                if weak {
                    glyph.start == 4 && glyph.rtl
                } else {
                    glyph.end == 4 && !glyph.rtl
                }
            })
            .unwrap()
            .clone();
        assert_ne!(glyph.id, 0);
        let bounds = Layout::new(&node).children().next().unwrap().bounds();
        click_at(
            &mut input,
            &mut tree,
            &node,
            &renderer,
            Point::new(origin.x + glyph.x + glyph.width * 0.75, bounds.center_y()),
        );
        assert_eq!(
            editor(&mut tree).cursor().state(&Value::new(value)),
            cursor::State::Index(4)
        );
        let affinity = if weak {
            CaretAffinity::Weak
        } else {
            CaretAffinity::Strong
        };
        let retained = editor(&mut tree).visual_selection().unwrap();
        assert_eq!(retained.focus.affinity, affinity);
        assert_eq!(retained.anchor, retained.focus);
        let x = ime_cursor(&mut input, &mut tree, &node, &renderer)
            .unwrap()
            .x;
        assert!(
            (x - (origin.x + glyph.x + glyph.width)).abs() < 1.001,
            "the real clicked edge, not another occurrence of logical index 4, anchors IME and caret"
        );
        draw(
            &input,
            &tree,
            &node,
            &mut renderer,
            true,
            1.0,
            node.bounds(),
        );
        let painted_caret = renderer
            .quads
            .iter()
            .find(|quad| (quad.bounds.width - 1.0).abs() < 0.001)
            .unwrap();
        near(painted_caret.bounds.x, x);
        renderer.artifact(
            if weak {
                "bidi-caret-weak"
            } else {
                "bidi-caret-strong"
            },
            Size::new(360.0, 70.0),
            style::Palette::for_window(false, true),
        );
        positions.push(x);
        for active in [false, true] {
            update(
                &mut input,
                &mut tree,
                &node,
                &renderer,
                Event::Window(if active {
                    window::Event::Focused
                } else {
                    window::Event::Unfocused
                }),
            );
            assert_eq!(editor(&mut tree).visual_selection(), Some(retained));
        }
        let direction = if weak {
            Named::ArrowLeft
        } else {
            Named::ArrowRight
        };
        let reverse = if weak {
            Named::ArrowRight
        } else {
            Named::ArrowLeft
        };
        let first = if weak { 5 } else { 6 };
        for expected in [first, if weak { 6 } else { 5 }] {
            assert!(
                press_key(
                    &mut input,
                    &mut tree,
                    &node,
                    &renderer,
                    direction,
                    keyboard::Modifiers::SHIFT
                )
                .is_empty()
            );
            assert_eq!(
                editor(&mut tree).cursor().state(&Value::new(value)),
                cursor::State::Selection {
                    start: 4,
                    end: expected
                }
            );
            let selection = editor(&mut tree).visual_selection().unwrap();
            assert_eq!(
                selection.anchor, retained.anchor,
                "Shift retains its original affinity and logical anchor"
            );
            assert_eq!(selection.focus.affinity, affinity);
        }
        for expected in [first, 4] {
            assert!(
                press_key(
                    &mut input,
                    &mut tree,
                    &node,
                    &renderer,
                    reverse,
                    keyboard::Modifiers::SHIFT
                )
                .is_empty()
            );
            let state = editor(&mut tree).cursor().state(&Value::new(value));
            assert_eq!(
                state,
                if expected == 4 {
                    cursor::State::Index(4)
                } else {
                    cursor::State::Selection {
                        start: 4,
                        end: expected,
                    }
                }
            );
        }
        assert_eq!(editor(&mut tree).visual_selection(), Some(retained));
        near(
            ime_cursor(&mut input, &mut tree, &node, &renderer)
                .unwrap()
                .x,
            x,
        );
        if weak {
            assert!(
                press_key(
                    &mut input,
                    &mut tree,
                    &node,
                    &renderer,
                    Named::ArrowLeft,
                    keyboard::Modifiers::SHIFT
                )
                .is_empty()
            );
            assert_eq!(
                editor(&mut tree).cursor().state(&Value::new(value)),
                cursor::State::Selection { start: 4, end: 5 }
            );
            let mut messages = Vec::new();
            input.update(
                &mut tree,
                &Event::InputMethod(iced::advanced::input_method::Event::Commit("X".to_owned())),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &node.bounds(),
            );
            assert_eq!(
                messages,
                ["abc Xבג def"],
                "retained visual affinity does not change the actual logical replacement range"
            );
            assert_eq!(editor(&mut tree).paragraph_text(), "abc Xבג def");
            assert!(editor(&mut tree).visual_selection().is_none());
        }
    }
    assert!(
        positions[1] - positions[0] > 8.0,
        "one logical boundary has two genuinely separated shaped positions"
    );
}

#[test]
fn selection_collapse_uses_visual_edges_and_retained_scroll_clip_at_all_scales() {
    use keyboard::key::Named;
    let mut renderer = Recorder::new();
    let value = "abc אבג def";
    for (key, expected, weak) in [(Named::ArrowLeft, 6, false), (Named::ArrowRight, 4, true)] {
        let mut input = TextInput::<String, Recorder>::new("Value", value)
            .width(40)
            .style(style::input)
            .on_input(|value| value);
        let mut tree = Tree::new(&input as &dyn Widget<String, Theme, Recorder>);
        let node = Widget::layout(
            &mut input,
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(360.0, 70.0)),
        );
        editor(&mut tree).focus();
        editor(&mut tree).select_range(4, 6);
        assert!(
            press_key(
                &mut input,
                &mut tree,
                &node,
                &renderer,
                key,
                keyboard::Modifiers::empty()
            )
            .is_empty()
        );
        assert_eq!(
            editor(&mut tree).cursor().state(&Value::new(value)),
            cursor::State::Index(expected)
        );
        if weak {
            assert_eq!(
                editor(&mut tree).visual_caret().unwrap().affinity,
                text_input::CaretAffinity::Weak
            );
        }
        let bounds = Layout::new(&node).children().next().unwrap().bounds();
        let ime = ime_cursor(&mut input, &mut tree, &node, &renderer).unwrap();
        assert!(ime.x >= bounds.x - 1.0 && ime.x < bounds.x + bounds.width);
        for scale in UiScale::ALL {
            let factor = scale.factor();
            let clip = Rectangle::new(Point::new(8.0, 3.0), Size::new(20.0, 16.0));
            draw(&input, &tree, &node, &mut renderer, true, factor, clip);
            renderer.assert_pixels_inside_clip(
                Size::new(360.0 * factor, 70.0 * factor),
                clip * Transformation::scale(factor),
            );
            let origin = input
                .inner
                .text_origin(&tree.children[0], Layout::new(&node));
            near(renderer.text[0].position.x, origin.x * factor);
            let glyph = renderer.text[0]
                .glyphs
                .iter()
                .find(|glyph| {
                    glyph.rtl
                        && if weak {
                            glyph.start == 4
                        } else {
                            glyph.end == "abc אב".len()
                        }
                })
                .unwrap();
            // Collapse right reaches Alef's right/leading edge; collapse left
            // reaches Bet's left/trailing edge, from actual cached shaping.
            let expected_x = glyph.x + if weak { glyph.width } else { 0.0 };
            assert!((ime.x - (origin.x + expected_x)).abs() < 1.001);
        }
    }
}

#[test]
fn visual_navigation_preserves_secure_ime_default_and_platform_binding_fallbacks() {
    use keyboard::key::Named;
    let renderer = Recorder::new();
    let value = "abc אבג e\u{301}🇨🇦👩‍🚀";
    // Each pair uses the actual toolkit key handler; the second editor has no
    // optional geometry adapter and is the independent upstream binding oracle.
    let jump = if cfg!(target_os = "macos") {
        keyboard::Modifiers::ALT
    } else {
        keyboard::Modifiers::CTRL
    };
    for (secure, composing, source, key, modifiers) in [
        (
            true,
            false,
            value,
            Named::ArrowRight,
            keyboard::Modifiers::empty(),
        ),
        (
            false,
            true,
            value,
            Named::ArrowRight,
            keyboard::Modifiers::empty(),
        ),
        (
            false,
            false,
            "abc\nאבג def",
            Named::ArrowRight,
            keyboard::Modifiers::empty(),
        ),
        (false, false, value, Named::ArrowLeft, jump),
        (
            false,
            false,
            value,
            Named::ArrowRight,
            jump | keyboard::Modifiers::SHIFT,
        ),
        (false, false, value, Named::Home, keyboard::Modifiers::SHIFT),
        (
            false,
            false,
            value,
            Named::End,
            keyboard::Modifiers::empty(),
        ),
        (
            false,
            false,
            value,
            Named::Backspace,
            keyboard::Modifiers::empty(),
        ),
        (
            false,
            false,
            value,
            Named::Delete,
            keyboard::Modifiers::empty(),
        ),
    ] {
        let mut results = Vec::new();
        for enabled in [true, false] {
            let mut input = TextInput::<String, Recorder>::new("Value", source)
                .secure(secure)
                .width(300)
                .style(style::input)
                .on_input(|value| value);
            if !enabled {
                input.inner = text_input::TextInput::new("Value", source)
                    .secure(secure)
                    .width(300)
                    .style(style::input)
                    .on_input(|value| value);
            }
            let mut tree = Tree::new(&input as &dyn Widget<String, Theme, Recorder>);
            let node = Widget::layout(
                &mut input,
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(360.0, 70.0)),
            );
            editor(&mut tree).focus();
            editor(&mut tree).move_cursor_to(4);
            if composing {
                update(
                    &mut input,
                    &mut tree,
                    &node,
                    &renderer,
                    Event::InputMethod(iced::advanced::input_method::Event::Opened),
                );
                update(
                    &mut input,
                    &mut tree,
                    &node,
                    &renderer,
                    Event::InputMethod(iced::advanced::input_method::Event::Preedit(
                        "候".to_owned(),
                        Some(0.."候".len()),
                    )),
                );
            }
            let messages = press_key(&mut input, &mut tree, &node, &renderer, key, modifiers);
            assert!(
                editor(&mut tree).visual_selection().is_none(),
                "fallback bindings cannot retain a stale visual affinity"
            );
            results.push((
                editor(&mut tree).cursor().state(&Value::new(source)),
                messages,
                editor(&mut tree).paragraph_text().to_owned(),
            ));
        }
        assert_eq!(
            results[0], results[1],
            "secure={secure}, composing={composing}, key={key:?}, modifiers={modifiers:?}"
        );
    }
}

#[test]
fn end_requests_redraw_when_only_the_retained_boundary_affinity_changes() {
    let value = "abc אבג";
    let mut renderer = Recorder::new();
    let mut input = TextInput::<String, Recorder>::new("Value", value)
        .width(300)
        .style(style::input)
        .on_input(|value| value);
    let mut tree = Tree::new(&input as &dyn Widget<String, Theme, Recorder>);
    let node = Widget::layout(
        &mut input,
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(360.0, 70.0)),
    );
    editor(&mut tree).focus();
    redraw(&mut input, &mut tree, &node, &renderer);
    draw(
        &input,
        &tree,
        &node,
        &mut renderer,
        true,
        1.0,
        node.bounds(),
    );
    let glyph = renderer.text[0]
        .glyphs
        .iter()
        .find(|glyph| glyph.rtl && glyph.end == value.len())
        .unwrap()
        .clone();
    let bounds = Layout::new(&node).children().next().unwrap().bounds();
    click_at(
        &mut input,
        &mut tree,
        &node,
        &renderer,
        Point::new(
            renderer.text[0].position.x + glyph.x + glyph.width * 0.25,
            bounds.center_y(),
        ),
    );
    let before = editor(&mut tree).cursor().state(&Value::new(value));
    assert_eq!(before, cursor::State::Index(Value::new(value).len()));
    assert_eq!(
        editor(&mut tree).visual_caret().unwrap().affinity,
        text_input::CaretAffinity::Weak
    );
    let weak_x = ime_cursor(&mut input, &mut tree, &node, &renderer)
        .unwrap()
        .x;
    let mut messages = Vec::new();
    let mut shell = Shell::new(&mut messages);
    input.update(
        &mut tree,
        &Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::End),
            modified_key: keyboard::Key::Named(keyboard::key::Named::End),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::End),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        }),
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &renderer,
        &mut iced::advanced::clipboard::Null,
        &mut shell,
        &node.bounds(),
    );
    assert_eq!(
        shell.redraw_request(),
        window::RedrawRequest::NextFrame,
        "the logical index stayed at end but the painted caret/scroll changed"
    );
    assert!(messages.is_empty());
    assert_eq!(editor(&mut tree).cursor().state(&Value::new(value)), before);
    assert!(editor(&mut tree).visual_selection().is_none());
    let strong_x = ime_cursor(&mut input, &mut tree, &node, &renderer)
        .unwrap()
        .x;
    assert!(strong_x - weak_x > 8.0);
}
