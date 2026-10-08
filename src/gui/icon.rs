//! Phosphor icons (MIT, assets/fonts) drawn as font glyphs, so they stay crisp
//! at any size and take their color from the palette. The app mark is drawn.
use crate::types::Transport;
use iced::widget::canvas::{self, Frame, Geometry, LineCap, Path, Stroke};
use iced::widget::{container, text, text_input};
use iced::{
    Color, Element, Font, Point, Rectangle, Renderer, Size, Theme, Vector, alignment, mouse,
};
use std::cell::Cell;

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
    ChevronLeft,
    Broadcast,
    Folder,
    Eject,
    Search,
    Recent,
    Network,
    Usb,
    Video,
    Cube,
    Check,
    /// An error: a mark in a circle. Warnings use the triangle, so severity
    /// reads by shape as well as color.
    Warning,
    /// A warning: a mark in a triangle.
    WarningTriangle,
    Keyboard,
    Sidebar,
    Sliders,
    CornersOut,
    CornersIn,
    Scan,
    Reset,
    Lock,
    CloudArrowUp,
    FolderPlus,
    /// Drives: local, external or network storage.
    HardDrives,
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
            Icon::ChevronLeft => (REGULAR, '\u{e138}'),
            Icon::Broadcast => (REGULAR, '\u{e0f2}'),
            Icon::Folder => (REGULAR, '\u{e24a}'),
            Icon::Eject => (REGULAR, '\u{e212}'),
            Icon::Search => (REGULAR, '\u{e30c}'),
            Icon::Recent => (REGULAR, '\u{e1a0}'),
            Icon::Network => (REGULAR, '\u{edde}'),
            Icon::Usb => (REGULAR, '\u{e956}'),
            Icon::Video => (REGULAR, '\u{e4da}'),
            Icon::Cube => (REGULAR, '\u{e1da}'),
            Icon::Check => (REGULAR, '\u{e184}'),
            Icon::Warning => (REGULAR, '\u{e4e2}'),
            Icon::WarningTriangle => (REGULAR, '\u{e4e0}'),
            Icon::Keyboard => (REGULAR, '\u{e2d8}'),
            Icon::Sidebar => (REGULAR, '\u{ec24}'),
            Icon::Sliders => (REGULAR, '\u{e434}'),
            Icon::CornersOut => (REGULAR, '\u{e1d0}'),
            Icon::CornersIn => (REGULAR, '\u{e1ce}'),
            Icon::Scan => (REGULAR, '\u{ebb6}'),
            Icon::Reset => (REGULAR, '\u{e038}'),
            Icon::Lock => (REGULAR, '\u{e2fa}'),
            Icon::CloudArrowUp => (REGULAR, '\u{e1ae}'),
            Icon::FolderPlus => (REGULAR, '\u{e258}'),
            Icon::HardDrives => (REGULAR, '\u{e2a0}'),
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

/// Spokes in the activity indicator, one lit per step.
const SPOKES: usize = 8;

/// A macOS-style activity indicator: eight spokes fading behind the one lit
/// at `phase`. The caller steps `phase` from a tick it already runs; this
/// draws a still frame and requests no redraws itself.
pub fn spinner<'a, Message: 'a>(size: f32, color: Color, phase: usize) -> Element<'a, Message> {
    canvas::Canvas::new(Spinner {
        color,
        phase: phase % SPOKES,
    })
    .width(size)
    .height(size)
    .into()
}

struct Spinner {
    color: Color,
    phase: usize,
}

/// Opacity of `spoke` while `phase` is lit: full at the head, fading over
/// the steps since each was lit.
fn glow(phase: usize, spoke: usize) -> f32 {
    let age = (phase + SPOKES - spoke % SPOKES) % SPOKES;
    1.0 - age as f32 * 0.1
}

/// The drawn spokes, kept until the phase or color changes.
#[derive(Default)]
struct Spun {
    cache: canvas::Cache,
    key: Cell<Option<(usize, Color)>>,
}

impl<Message> canvas::Program<Message> for Spinner {
    type State = Spun;

    fn draw(
        &self,
        state: &Spun,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let key = Some((self.phase, self.color));
        if state.key.replace(key) != key {
            state.cache.clear();
        }
        let geometry = state.cache.draw(renderer, bounds.size(), |frame| {
            let side = bounds.width.min(bounds.height);
            let center = frame.center();
            let (inner, outer) = (side * 0.24, side * 0.46);
            for spoke in 0..SPOKES {
                let angle = spoke as f32 * std::f32::consts::TAU / SPOKES as f32
                    - std::f32::consts::FRAC_PI_2;
                let along = Vector::new(angle.cos(), angle.sin());
                let color = Color {
                    a: self.color.a * glow(self.phase, spoke),
                    ..self.color
                };
                frame.stroke(
                    &Path::line(center + along * inner, center + along * outer),
                    Stroke::default()
                        .with_color(color)
                        .with_width(side * 0.11)
                        .with_line_cap(LineCap::Round),
                );
            }
        });
        vec![geometry]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_fades_behind_the_lit_spoke() {
        assert_eq!(glow(3, 3), 1.0);
        assert!((glow(3, 2) - 0.9).abs() < 1e-6);
        // The spoke just ahead of the head was lit longest ago.
        assert!((glow(3, 4) - 0.3).abs() < 1e-6);
        assert_eq!(glow(0, 0), glow(SPOKES, 0));
        let total: f32 = (0..SPOKES).map(|spoke| glow(5, spoke)).sum();
        assert!((total - 5.2).abs() < 1e-5);
    }
}
