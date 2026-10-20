//! Phosphor icons (MIT, assets/fonts) drawn as font glyphs, so they stay crisp
//! at any size and take their color from the palette. The app mark uses the
//! shared brand artwork.
use crate::types::Transport;
use iced::widget::canvas::{self, Geometry, LineCap, Path, Stroke};
use iced::widget::{container, image, text, text_input};
use iced::{Color, Element, Font, Rectangle, Renderer, Theme, Vector, alignment, mouse};
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
    Sparkle,
    PaperPlaneRight,
    Timer,
    Record,
    Trash,
    CalendarBlank,
    Eye,
    Refresh,
    Plus,
    Minus,
    Close,
    Grid,
    Chart,
    Activity,
    Help,
    Copy,
    Gear,
    ChevronRight,
    ChevronDown,
    ChevronLeft,
    Broadcast,
    Folder,
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
    Sliders,
    Sidebar,
    CornersOut,
    CornersIn,
    Scan,
    Reset,
    Lock,
    CloudArrowUp,
    FolderPlus,
    /// Drives: local, external or network storage.
    HardDrives,
    /// The app mark: a capture-frame C, lens and recording light.
    Mark,
}

impl Icon {
    fn glyph(self) -> (Font, char) {
        match self {
            Icon::Play => (FILL, '\u{e3d0}'),
            Icon::Stop => (FILL, '\u{e46c}'),
            Icon::Camera => (REGULAR, '\u{e10e}'),
            Icon::Sparkle => (REGULAR, '\u{e6a2}'),
            Icon::PaperPlaneRight => (REGULAR, '\u{e396}'),
            Icon::Timer => (REGULAR, '\u{e492}'),
            Icon::Record => (REGULAR, '\u{e3ee}'),
            Icon::Trash => (REGULAR, '\u{e4a6}'),
            Icon::CalendarBlank => (REGULAR, '\u{e10a}'),
            Icon::Eye => (REGULAR, '\u{e220}'),
            Icon::Refresh => (REGULAR, '\u{e036}'),
            Icon::Plus => (REGULAR, '\u{e3d4}'),
            Icon::Minus => (REGULAR, '\u{e32a}'),
            Icon::Close => (REGULAR, '\u{e4f6}'),
            Icon::Grid => (REGULAR, '\u{e464}'),
            Icon::Chart => (REGULAR, '\u{e150}'),
            Icon::Activity => (REGULAR, '\u{e000}'),
            Icon::Help => (REGULAR, '\u{e3e8}'),
            Icon::Copy => (REGULAR, '\u{e1ca}'),
            // Phosphor's gear-six, checked against the font's ligatures.
            Icon::Gear => (REGULAR, '\u{e272}'),
            Icon::ChevronRight => (REGULAR, '\u{e13a}'),
            Icon::ChevronDown => (REGULAR, '\u{e136}'),
            Icon::ChevronLeft => (REGULAR, '\u{e138}'),
            Icon::Broadcast => (REGULAR, '\u{e0f2}'),
            Icon::Folder => (REGULAR, '\u{e24a}'),
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
            Icon::Sliders => (REGULAR, '\u{e434}'),
            Icon::Sidebar => (REGULAR, '\u{ec24}'),
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
            Transport::Scanner => Icon::Scan,
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
        return image(super::brand::mark()).width(size).height(size).into();
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

    /// Whether the font in `bytes` maps `code` to a glyph, read from its
    /// Unicode cmap subtable (format 4 or 12).
    fn has_glyph(bytes: &[u8], code: char) -> bool {
        let u16_at = |at: usize| u16::from_be_bytes([bytes[at], bytes[at + 1]]) as usize;
        let u32_at = |at: usize| {
            u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize
        };
        let code = code as usize;
        let cmap = (0..u16_at(4))
            .map(|table| 12 + table * 16)
            .find(|&record| &bytes[record..record + 4] == b"cmap")
            .map(|record| u32_at(record + 8))
            .expect("the font has a cmap");
        (0..u16_at(cmap + 2)).any(|index| {
            let record = cmap + 4 + index * 8;
            let table = cmap + u32_at(record + 4);
            match u16_at(table) {
                4 => {
                    let segments = u16_at(table + 6) / 2;
                    let ends = table + 14;
                    let starts = ends + segments * 2 + 2;
                    (0..segments).any(|segment| {
                        let (start, end) =
                            (u16_at(starts + segment * 2), u16_at(ends + segment * 2));
                        start <= code && code <= end && code != 0xffff
                    })
                }
                12 => (0..u32_at(table + 12)).any(|group| {
                    let at = table + 16 + group * 12;
                    u32_at(at) <= code && code <= u32_at(at + 4)
                }),
                _ => false,
            }
        })
    }

    #[test]
    fn every_glyph_is_in_its_font() {
        use Icon::*;
        let all = [
            Play,
            Stop,
            Camera,
            Sparkle,
            PaperPlaneRight,
            Timer,
            Record,
            Trash,
            CalendarBlank,
            Eye,
            Refresh,
            Plus,
            Minus,
            Close,
            Grid,
            Chart,
            Activity,
            Help,
            Copy,
            Gear,
            ChevronRight,
            ChevronDown,
            ChevronLeft,
            Broadcast,
            Folder,
            Search,
            Recent,
            Network,
            Usb,
            Video,
            Cube,
            Check,
            Warning,
            WarningTriangle,
            Keyboard,
            Sliders,
            CornersOut,
            CornersIn,
            Scan,
            Reset,
            Lock,
            CloudArrowUp,
            FolderPlus,
            HardDrives,
            Mark,
        ];
        for icon in all {
            let (font, code) = icon.glyph();
            let bytes = if font == FILL {
                FILL_BYTES
            } else {
                REGULAR_BYTES
            };
            assert!(has_glyph(bytes, code), "{icon:?} U+{:04X}", code as u32);
        }
        // Settings uses Phosphor's gear-six, not the plainer gear (U+E270).
        assert_eq!(Gear.glyph(), (REGULAR, '\u{e272}'));
        assert!(
            !has_glyph(REGULAR_BYTES, '\u{f8ff}'),
            "the reader can say no"
        );
    }
}
