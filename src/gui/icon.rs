//! Phosphor icons (MIT, assets/fonts) drawn as font glyphs, so they stay crisp
//! at any size and take their color from the palette. The app mark is drawn.
use crate::types::Transport;
use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke};
use iced::widget::{container, text, text_input};
use iced::{Color, Element, Font, Point, Rectangle, Renderer, Size, Theme, alignment, mouse};

pub const REGULAR_BYTES: &[u8] = include_bytes!("../../assets/fonts/Phosphor.ttf");
pub const FILL_BYTES: &[u8] = include_bytes!("../../assets/fonts/Phosphor-Fill.ttf");
const REGULAR: Font = Font::with_name("Phosphor");
const FILL: Font = Font::with_name("Phosphor-Fill");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Play,
    Stop,
    Camera,
    Refresh,
    Plus,
    Minus,
    Close,
    Grid,
    Chart,
    Activity,
    Help,
    Copy,
    Sun,
    Moon,
    Contrast,
    ChevronRight,
    ChevronDown,
    Broadcast,
    Folder,
    Eject,
    Search,
    Recent,
    Plug,
    Network,
    Usb,
    Video,
    Cube,
    Check,
    Warning,
    Keyboard,
    Sidebar,
    Sliders,
    CornersOut,
    CornersIn,
    /// The app mark: a rounded lens body.
    Mark,
}

impl Icon {
    fn glyph(self) -> (Font, char) {
        match self {
            Icon::Play => (FILL, '\u{e3d0}'),
            Icon::Stop => (FILL, '\u{e46c}'),
            Icon::Camera => (REGULAR, '\u{e10e}'),
            Icon::Refresh => (REGULAR, '\u{e036}'),
            Icon::Plus => (REGULAR, '\u{e3d4}'),
            Icon::Minus => (REGULAR, '\u{e32a}'),
            Icon::Close => (REGULAR, '\u{e4f6}'),
            Icon::Grid => (REGULAR, '\u{e464}'),
            Icon::Chart => (REGULAR, '\u{e150}'),
            Icon::Activity => (REGULAR, '\u{e000}'),
            Icon::Help => (REGULAR, '\u{e3e8}'),
            Icon::Copy => (REGULAR, '\u{e1ca}'),
            Icon::Sun => (REGULAR, '\u{e472}'),
            Icon::Moon => (REGULAR, '\u{e330}'),
            Icon::Contrast => (REGULAR, '\u{e18c}'),
            Icon::ChevronRight => (REGULAR, '\u{e13a}'),
            Icon::ChevronDown => (REGULAR, '\u{e136}'),
            Icon::Broadcast => (REGULAR, '\u{e0f2}'),
            Icon::Folder => (REGULAR, '\u{e24a}'),
            Icon::Eject => (REGULAR, '\u{e212}'),
            Icon::Search => (REGULAR, '\u{e30c}'),
            Icon::Recent => (REGULAR, '\u{e1a0}'),
            Icon::Plug => (REGULAR, '\u{e946}'),
            Icon::Network => (REGULAR, '\u{edde}'),
            Icon::Usb => (REGULAR, '\u{e956}'),
            Icon::Video => (REGULAR, '\u{e4da}'),
            Icon::Cube => (REGULAR, '\u{e1da}'),
            Icon::Check => (REGULAR, '\u{e184}'),
            Icon::Warning => (REGULAR, '\u{e4e2}'),
            Icon::Keyboard => (REGULAR, '\u{e2d8}'),
            Icon::Sidebar => (REGULAR, '\u{ec24}'),
            Icon::Sliders => (REGULAR, '\u{e434}'),
            Icon::CornersOut => (REGULAR, '\u{e1d0}'),
            Icon::CornersIn => (REGULAR, '\u{e1ce}'),
            Icon::Mark => (REGULAR, '\u{e00a}'),
        }
    }

    pub fn transport(transport: Transport) -> Icon {
        match transport {
            Transport::GigE => Icon::Network,
            Transport::Usb3 => Icon::Usb,
            Transport::Simulator => Icon::Cube,
            Transport::Media => Icon::Video,
        }
    }
}

pub fn input_icon(kind: Icon) -> text_input::Icon<Font> {
    let (font, code_point) = kind.glyph();
    text_input::Icon {
        font,
        code_point,
        size: Some(14.0.into()),
        spacing: 8.0,
        side: text_input::Side::Left,
    }
}

pub fn icon<'a, Message: 'a>(icon: Icon, size: f32, color: Color) -> Element<'a, Message> {
    if icon == Icon::Mark {
        return canvas::Canvas::new(Mark { color })
            .width(size)
            .height(size)
            .into();
    }
    let (font, code) = icon.glyph();
    container(
        text(code)
            .font(font)
            .size(size)
            .line_height(1.0)
            .color(color)
            .shaping(text::Shaping::Basic),
    )
    .width(size)
    .height(size)
    .align_x(alignment::Horizontal::Center)
    .align_y(alignment::Vertical::Center)
    .into()
}

struct Mark {
    color: Color,
}

impl<Message> canvas::Program<Message> for Mark {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let s = bounds.width.min(bounds.height) / 16.0;
        let p = |x: f32, y: f32| Point::new(x * s, y * s);
        frame.fill(
            &Path::rounded_rectangle(p(0.5, 0.5), Size::new(15.0 * s, 15.0 * s), (4.0 * s).into()),
            self.color,
        );
        frame.stroke(
            &Path::circle(p(8.0, 8.0), 3.8 * s),
            Stroke::default()
                .with_color(Color::WHITE)
                .with_width(1.6 * s),
        );
        frame.fill(&Path::circle(p(8.0, 8.0), 1.3 * s), Color::WHITE);
        vec![frame.into_geometry()]
    }
}
