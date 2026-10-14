//! Exercise actual iced editor drawing and software-shaped text, without a
//! second editor, measurement paragraph, synthetic focus, or duplicate text.
use super::*;
use crate::gui::{prefs::UiScale, style};
use iced::advanced::text::Paragraph as _;
use iced::{Background, Color, Font, Point, Transformation, window};
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
