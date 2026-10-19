//! Shared light and dark colors: blue for interaction and selection, green for
//! success/live acquisition, amber for attention, and red for errors or removal.
use iced::theme::Base;
use iced::widget::{
    button, checkbox, container, overlay::menu, pick_list, progress_bar, rule, scrollable, slider,
    text_input,
};
use iced::{Background, Border, Color, Font, Shadow, Theme, Vector, border, font};
use std::sync::OnceLock;

const fn rgb(hex: u32) -> Color {
    Color::from_rgb8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

const fn alpha(color: Color, a: f32) -> Color {
    Color { a, ..color }
}

/// Named colors of one window appearance. Views take these from
/// `Palette::for_window`, styles from the theme, so both always agree.
pub struct Palette {
    pub dark: bool,
    window_active: bool,
    /// Stage controls need to read over composited glass on any camera image.
    on_image: bool,
    /// Main content and inspector.
    pub base: Color,
    pub sidebar: Color,
    /// Rows under the pointer, subtle buttons and fields.
    pub hover: Color,
    pub selected: Color,
    pub field: Color,
    pub stage: Color,
    pub hairline: Color,
    /// Functional outlines and inactive tracks; at least 3:1 against fields
    /// and panels. Hairlines remain quieter for decorative separators.
    pub control_border: Color,
    pub text: Color,
    /// Text that informs: hints, captions, values. At least 4.5:1 on every
    /// panel surface, including hovered and selected rows, fields, wells,
    /// segment tracks and gray buttons.
    pub secondary: Color,
    /// Decorative marks. Text that informs, including placeholders and
    /// unavailable action labels, uses `secondary`.
    pub tertiary: Color,
    /// Rings, checks, sliders and icons.
    pub accent: Color,
    /// Accent for text and small marks; darker in light mode for contrast.
    pub accent_text: Color,
    /// Filled buttons with white labels, deep enough for 4.5:1.
    pub accent_fill: Color,
    pub accent_soft: Color,
    /// Status colors, for marks: dots, glyphs, tints and borders. Text and
    /// small icons in them go through `ink`, which keeps them at 4.5:1.
    pub live: Color,
    pub warn: Color,
    pub danger: Color,
    pub scrim: Color,
    pub shadow: Color,
}

pub const LIGHT: Palette = Palette {
    dark: false,
    window_active: true,
    on_image: false,
    base: rgb(0xFFFFFF),
    sidebar: rgb(0xF5F5F7),
    hover: rgb(0xEDEDF0),
    selected: rgb(0xE3EDFC),
    field: rgb(0xF1F1F4),
    stage: rgb(0x1A1A1C),
    hairline: rgb(0xE5E5E9),
    control_border: rgb(0x87878E),
    text: rgb(0x1D1D1F),
    secondary: rgb(0x5E5E65),
    tertiary: rgb(0x8E8E93),
    accent: rgb(0x246CD6),
    accent_text: rgb(0x1D5FBC),
    accent_fill: rgb(0x246BDC),
    accent_soft: alpha(rgb(0x246CD6), 0.12),
    live: rgb(0x248A3D),
    // Amber, a clear step from `danger`'s red; as a mark it reads like the
    // accent (3:1 on a selected row), and `ink` deepens it for text.
    warn: rgb(0xC86400),
    danger: rgb(0xD70015),
    scrim: alpha(rgb(0x000000), 0.18),
    shadow: alpha(rgb(0x000000), 0.12),
};

pub const DARK: Palette = Palette {
    dark: true,
    window_active: true,
    on_image: false,
    base: rgb(0x1E1E20),
    sidebar: rgb(0x29292C),
    hover: rgb(0x323236),
    selected: rgb(0x243B5B),
    field: rgb(0x2C2C30),
    stage: rgb(0x111113),
    hairline: rgb(0x343438),
    control_border: rgb(0x85858F),
    text: rgb(0xEDEDF0),
    secondary: rgb(0xB9B9C2),
    tertiary: rgb(0x7C7C83),
    accent: rgb(0x4C8DF8),
    accent_text: rgb(0x95BFFF),
    accent_fill: rgb(0x2971E6),
    accent_soft: alpha(rgb(0x4C8DF8), 0.18),
    live: rgb(0x32D158),
    warn: rgb(0xFF9F0A),
    danger: rgb(0xFF453A),
    scrim: alpha(rgb(0x000000), 0.42),
    shadow: alpha(rgb(0x000000), 0.45),
};

/// Increase Contrast strengthens useful edges and secondary labels while
/// preserving the user's light or dark appearance and familiar status hues.
pub const LIGHT_CONTRAST: Palette = Palette {
    text: Color::BLACK,
    secondary: rgb(0x3C3C43),
    tertiary: rgb(0x66666D),
    control_border: rgb(0x66666D),
    hairline: rgb(0x707078),
    accent_text: rgb(0x144B99),
    accent_fill: rgb(0x1755AA),
    ..LIGHT
};

pub const DARK_CONTRAST: Palette = Palette {
    text: Color::WHITE,
    secondary: rgb(0xEDEDF0),
    tertiary: rgb(0x9999A2),
    control_border: rgb(0xB0B0B8),
    hairline: rgb(0xA9A9B0),
    accent_text: rgb(0xAED0FF),
    ..DARK
};

/// Everything drawn on the stage and its glass: tiles, captions, overlays and
/// the controls floating over the image. The stage is dark in both
/// appearances. Its `text` and `secondary` are `ON_STAGE` and
/// `ON_STAGE_SECONDARY`, which read over glass on a bright picture; its
/// `tertiary` is for marks only, and glyphs that show an active state use
/// `accent_text`, not `accent`.
pub const STAGE: &Palette = &Palette {
    on_image: true,
    text: ON_STAGE,
    secondary: ON_STAGE_SECONDARY,
    accent_text: rgb(0xA8CAFF),
    ..DARK
};

/// A background window retains its selected content, but selection no longer
/// promises keyboard input. macOS also removes the color from control accents;
/// Windows and Linux keep those accents distinct from inactive selections.
const fn inactive(mut p: Palette, neutral_controls: bool) -> Palette {
    p.window_active = false;
    p.selected = if p.dark { rgb(0x3D3D43) } else { rgb(0xE1E1E6) };
    let increased = p.text.r == 1.0 || p.text.r == 0.0;
    if !increased {
        // Neutral selection is slightly stronger than the blue tint. Keep
        // functional outlines visible against that surface as well as fields.
        p.control_border = if p.dark { rgb(0x8A8A94) } else { rgb(0x7E7E86) };
    }
    if neutral_controls {
        p.accent = if p.dark {
            if increased {
                rgb(0xC0C0C8)
            } else {
                rgb(0xA0A0A8)
            }
        } else if increased {
            rgb(0x44444C)
        } else {
            rgb(0x64646B)
        };
        p.accent_text = if p.dark {
            if increased {
                rgb(0xE0E0E8)
            } else {
                rgb(0xCACAD2)
            }
        } else if increased {
            rgb(0x3C3C43)
        } else {
            rgb(0x515159)
        };
        p.accent_fill = if p.dark {
            rgb(0x74747C)
        } else if increased {
            rgb(0x47474F)
        } else {
            rgb(0x66666D)
        };
        p.accent_soft = alpha(p.accent, p.accent_soft.a);
    }
    p
}

const INACTIVE: [Palette; 4] = [
    inactive(LIGHT, cfg!(target_os = "macos")),
    inactive(DARK, cfg!(target_os = "macos")),
    inactive(LIGHT_CONTRAST, cfg!(target_os = "macos")),
    inactive(DARK_CONTRAST, cfg!(target_os = "macos")),
];

const STAGE_INACTIVE: &Palette = &Palette {
    window_active: false,
    accent: rgb(0xA0A0A8),
    accent_text: ON_STAGE_SECONDARY,
    ..*STAGE
};

/// Explicit preview-control glyphs use the same inactive accent as the styles.
/// Image data, acquisition status and analytic overlays keep their own hues.
pub fn stage_palette(active: bool) -> &'static Palette {
    if !active && cfg!(target_os = "macos") {
        STAGE_INACTIVE
    } else {
        STAGE
    }
}

/// The strongest tint text sits on: a pressed button or selected text.
const TINT: f32 = 0.30;

impl Palette {
    pub(super) fn increased(&self) -> bool {
        self.text == Color::WHITE || self.text == Color::BLACK
    }

    pub fn of(dark: bool) -> &'static Palette {
        Self::appearance(dark, super::motion::increase_contrast())
    }

    pub fn for_window(dark: bool, active: bool) -> &'static Palette {
        Self::window_appearance(dark, super::motion::increase_contrast(), active)
    }

    fn appearance(dark: bool, increased: bool) -> &'static Palette {
        match (dark, increased) {
            (false, false) => &LIGHT,
            (true, false) => &DARK,
            (false, true) => &LIGHT_CONTRAST,
            (true, true) => &DARK_CONTRAST,
        }
    }
    fn from(theme: &Theme) -> &'static Palette {
        let dark = theme.extended_palette().is_dark;
        Self::window_appearance(
            dark,
            theme.palette().text == Self::appearance(dark, true).text,
            !theme.name().ends_with(" Inactive"),
        )
    }

    fn window_appearance(dark: bool, increased: bool, active: bool) -> &'static Palette {
        if active {
            Self::appearance(dark, increased)
        } else {
            &INACTIVE[usize::from(dark) + 2 * usize::from(increased)]
        }
    }

    /// `color` for text and icons: deepened (light) or lightened (dark) just
    /// enough to read at 4.5:1 on any panel surface (`grounds`), and on a
    /// tint of itself up to 30% over a panel, including pressed buttons and
    /// selected text. Hue stays consistent as contrast increases.
    pub fn ink(&self, color: Color) -> Color {
        let toward = if self.dark {
            Color::WHITE
        } else {
            Color::BLACK
        };
        let tint = Color { a: 1.0, ..color };
        let tinted = self
            .grounds()
            .map(|surface| luminance(mix(surface, tint, TINT)));
        let mut amount = 0.0;
        let plain = self.grounds().map(luminance);
        loop {
            let ink = Color {
                a: color.a,
                ..mix(tint, toward, amount)
            };
            let lum = luminance(ink);
            if amount >= 1.0
                || tinted
                    .iter()
                    .chain(&plain)
                    .all(|&ground| contrast(lum, ground) >= 4.5)
            {
                return ink;
            }
            amount = (amount + 0.02).min(1.0);
        }
    }

    /// Every control surface, including selected/hovered rows and the
    /// brightest image behind stage glass.
    fn grounds(&self) -> [Color; 11] {
        [
            self.base,
            self.sidebar,
            self.field,
            well_fill(self),
            self.hover,
            self.selected,
            gray(self),
            segment_fill(self),
            thumb_fill(self),
            mix(gray(self), self.text, 0.12),
            if self.on_image {
                // Bright white image, then glass, then a pressed glass button.
                mix(mix(Color::WHITE, rgb(0x1C1C1E), 0.84), Color::WHITE, 0.2)
            } else {
                self.base
            },
        ]
    }
}

/// WCAG relative luminance of an sRGB color. The renderer blends in sRGB
/// (iced's `web-colors`), so composites are mixed in the same space.
fn luminance(color: Color) -> f32 {
    let channel = |c: f32| {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
}

/// WCAG contrast ratio between two relative luminances.
fn contrast(a: f32, b: f32) -> f32 {
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// The iced theme for an appearance; widgets left unstyled still match.
#[cfg(test)]
pub fn theme(dark: bool) -> Theme {
    theme_appearance(dark, super::motion::increase_contrast())
}

pub fn window_theme(dark: bool, active: bool) -> Theme {
    window_theme_appearance(dark, super::motion::increase_contrast(), active)
}

#[cfg(test)]
fn theme_appearance(dark: bool, increased: bool) -> Theme {
    window_theme_appearance(dark, increased, true)
}

fn window_theme_appearance(dark: bool, increased: bool, active: bool) -> Theme {
    static THEMES: OnceLock<[Theme; 8]> = OnceLock::new();
    let themes = THEMES.get_or_init(|| {
        [
            &LIGHT,
            &DARK,
            &LIGHT_CONTRAST,
            &DARK_CONTRAST,
            &INACTIVE[0],
            &INACTIVE[1],
            &INACTIVE[2],
            &INACTIVE[3],
        ]
        .map(|p| {
            let name = match (p.dark, p.increased()) {
                (false, false) => "Capturefab Light",
                (true, false) => "Capturefab Dark",
                (false, true) => "Capturefab Light Increased Contrast",
                (true, true) => "Capturefab Dark Increased Contrast",
            };
            Theme::custom_with_fn(
                if p.window_active {
                    name.to_owned()
                } else {
                    format!("{name} Inactive")
                },
                iced::theme::Palette {
                    background: p.base,
                    text: p.text,
                    primary: p.accent_fill,
                    success: p.live,
                    warning: p.warn,
                    danger: p.danger,
                },
                |palette| {
                    let mut extended = iced::theme::palette::Extended::generate(palette);
                    // The default generator brightens hover blue while keeping
                    // the resting label, which loses contrast. Reuse our actual
                    // filled-button colors for widgets without an explicit style.
                    extended.primary.base = iced::theme::palette::Pair {
                        color: p.accent_fill,
                        text: Color::WHITE,
                    };
                    extended.primary.strong = iced::theme::palette::Pair {
                        color: mix(p.accent_fill, Color::BLACK, 0.08),
                        text: Color::WHITE,
                    };
                    extended.primary.weak = iced::theme::palette::Pair {
                        color: mix(p.base, p.accent, p.accent_soft.a),
                        text: p.ink(p.accent),
                    };
                    extended
                },
            )
        })
    });
    themes[usize::from(dark) + 2 * usize::from(increased) + 4 * usize::from(!active)].clone()
}

/// The platform's interface font. Linux uses fontconfig's generic families,
/// configured by `initialize_fonts`, instead of choosing a distribution's font.
pub const SANS: Font = if cfg!(target_os = "macos") {
    Font::with_name("System Font")
} else if cfg!(target_os = "windows") {
    Font::with_name("Segoe UI")
} else {
    Font::DEFAULT
};

pub const MONO: Font = if cfg!(target_os = "macos") {
    Font::with_name("Menlo")
} else if cfg!(target_os = "windows") {
    Font::with_name("Consolas")
} else {
    Font {
        family: font::Family::Monospace,
        ..Font::DEFAULT
    }
};

/// Resolve Linux's fontconfig preferences once before any text is laid out.
/// cosmic-text otherwise replaces its fontconfig mappings with fixed fonts.
pub(super) fn initialize_fonts() {
    #[cfg(target_os = "linux")]
    {
        use iced::advanced::graphics::text::font_system;
        let matched = ["sans-serif", "monospace"].map(|family| {
            std::process::Command::new("fc-match")
                .args(["--format=%{family[0]}", family])
                .output()
                .ok()
                .filter(|output| output.status.success() && output.stdout.len() <= 512)
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .filter(|name| !name.trim().is_empty())
        });
        let mut system = font_system()
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let db = system.raw().db_mut();
        if let Some(family) = &matched[0] {
            db.set_sans_serif_family(family.trim());
        }
        if let Some(family) = &matched[1] {
            db.set_monospace_family(family.trim());
        }
    }
}

pub const MEDIUM: Font = Font {
    weight: font::Weight::Medium,
    ..SANS
};
pub const SEMIBOLD: Font = Font {
    weight: font::Weight::Semibold,
    ..SANS
};
/// Titles. Semibold, because macOS registers its variable system font as a
/// single regular face that text matching only stretches up to semibold.
pub const BOLD: Font = Font {
    weight: if cfg!(target_os = "macos") {
        font::Weight::Semibold
    } else {
        font::Weight::Bold
    },
    ..SANS
};

/// Type scale. Every text size in the workbench is one of these.
pub const DISPLAY: f32 = 22.0;
pub const TITLE: f32 = 15.0;
pub const HEADING: f32 = 13.0;
pub const BODY: f32 = 13.0;
pub const SMALL: f32 = 12.0;
pub const CAPTION: f32 = 11.0;

/// The height of one line of text at `size`: iced's default line height,
/// `LineHeight::Relative(1.3)`. For layout heights that hold text, so they
/// follow the type scale.
pub const fn line_height(size: f32) -> f32 {
    size * 1.3
}

/// Corner radii. Fields, plain and glass buttons, segments, tooltips and code.
pub const RADIUS_SMALL: f32 = 6.0;
/// Buttons, list rows, menus and segment tracks.
pub const RADIUS: f32 = 8.0;
/// Tiles, the selection ring, wells and cards.
pub const RADIUS_MEDIUM: f32 = 10.0;
/// Glass overlays floating over the stage.
pub const RADIUS_LARGE: f32 = 12.0;
/// Sheets.
pub const RADIUS_SHEET: f32 = 14.0;

/// Text on the stage and its overlays, which are dark in both appearances.
pub const ON_STAGE: Color = rgb(0xF5F5F7);
pub const ON_STAGE_SECONDARY: Color = rgb(0xC2C2C9);

fn border(radius: f32) -> Border {
    Border {
        radius: radius.into(),
        ..Border::default()
    }
}

fn hairline(color: Color, radius: f32) -> Border {
    Border {
        color,
        width: if super::motion::increase_contrast() {
            1.5
        } else {
            1.0
        },
        radius: radius.into(),
    }
}

fn track(p: &Palette) -> Color {
    p.control_border
}

/// `a` moved toward `b` by `t`, alpha included.
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

// Containers

pub fn sidebar(theme: &Theme) -> container::Style {
    let p = Palette::from(theme);
    container::Style::default()
        .background(p.sidebar)
        .color(p.text)
}

pub fn base(theme: &Theme) -> container::Style {
    let p = Palette::from(theme);
    container::Style::default().background(p.base).color(p.text)
}

pub fn stage(theme: &Theme) -> container::Style {
    let p = Palette::from(theme);
    container::Style::default()
        .background(p.stage)
        .color(ON_STAGE)
}

/// A camera tile's picture.
pub fn tile(theme: &Theme) -> container::Style {
    let p = Palette::from(theme);
    container::Style {
        background: Some(p.stage.into()),
        border: border(RADIUS_MEDIUM),
        text_color: Some(ON_STAGE),
        ..container::Style::default()
    }
}

/// The accent ring drawn around the selected tile, with corners of
/// `radius`; `shown` fades it.
pub fn ring(shown: f32, emphasized: bool, radius: f32) -> impl Fn(&Theme) -> container::Style {
    move |theme| container::Style {
        border: Border {
            color: alpha(
                if emphasized && Palette::from(theme).window_active {
                    STAGE.accent
                } else {
                    STAGE_INACTIVE.accent
                },
                shown,
            ),
            width: 2.5,
            radius: radius.into(),
        },
        ..container::Style::default()
    }
}

/// The capture flash over a captured picture at full level: a white wash
/// of this alpha, unless movement is reduced (see `flash_wash`)...
pub const FLASH_WASH: f32 = 0.22;
/// ...and a white edge of this alpha and width.
pub const FLASH_EDGE: f32 = 0.9;
pub const FLASH_WIDTH: f32 = 2.0;

/// Whether the capture flash washes the picture: not while movement is
/// reduced, so only its edge flashes.
pub fn flash_wash() -> bool {
    !super::motion::reduce_motion()
}

/// The capture flash over a tile at `level`; the single view draws the same
/// over its picture.
pub fn capture_flash(level: f32) -> impl Fn(&Theme) -> container::Style {
    let wash = flash_wash();
    move |_theme| container::Style {
        background: wash.then(|| alpha(Color::WHITE, FLASH_WASH * level).into()),
        border: Border {
            color: alpha(Color::WHITE, FLASH_EDGE * level),
            width: FLASH_WIDTH,
            radius: RADIUS_MEDIUM.into(),
        },
        ..container::Style::default()
    }
}

/// Dark glass behind controls that float over the image; `shown` fades it.
/// Opaque while the system asks to reduce transparency.
pub fn overlay(shown: f32) -> impl Fn(&Theme) -> container::Style {
    glass(
        shown,
        super::motion::reduce_transparency() || super::motion::increase_contrast(),
    )
}

/// `overlay` with its transparency chosen: opaque when `solid`.
pub fn glass(shown: f32, solid: bool) -> impl Fn(&Theme) -> container::Style {
    let increased = super::motion::increase_contrast();
    let (fill, edge) = if increased {
        (1.0, 0.65)
    } else if solid {
        (1.0, 0.14)
    } else {
        (0.84, 0.08)
    };
    move |_theme| container::Style {
        background: Some(alpha(rgb(0x1C1C1E), fill * shown).into()),
        border: Border {
            color: alpha(Color::WHITE, edge * shown),
            width: if increased { 1.5 } else { 1.0 },
            radius: RADIUS_LARGE.into(),
        },
        shadow: Shadow {
            color: alpha(Color::BLACK, 0.35 * shown),
            offset: Vector::new(0.0, 6.0),
            blur_radius: 20.0,
        },
        text_color: Some(alpha(ON_STAGE, shown)),
        ..container::Style::default()
    }
}

/// How dark the shade under a tile's caption is, from its bottom edge (0)
/// to its top (1): held deep across the text row, then fading out.
const CAPTION_SHADE: [(f32, f32); 3] = [(0.0, 0.88), (0.75, 0.82), (1.0, 0.0)];

/// The darkening under a tile's caption, so its text reads on any image:
/// Primary and secondary labels read throughout the caption's text rows,
/// even over a white image; only the top quarter fades out.
pub fn caption(_theme: &Theme) -> container::Style {
    let fade = CAPTION_SHADE
        .iter()
        .fold(iced::gradient::Linear::new(0.0), |fade, &(at, dark)| {
            fade.add_stop(at, alpha(Color::BLACK, dark))
        });
    container::Style {
        background: Some(Background::Gradient(fade.into())),
        border: Border {
            radius: border::Radius::default().bottom(RADIUS_MEDIUM),
            ..Border::default()
        },
        text_color: Some(ON_STAGE),
        ..container::Style::default()
    }
}

/// A rounded status label tinted with `color`, like "Streaming".
pub fn pill(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |theme| container::Style {
        background: Some(alpha(color, 0.14).into()),
        border: border(999.0),
        text_color: Some(Palette::from(theme).ink(color)),
        ..container::Style::default()
    }
}

/// The fill of a `well`.
fn well_fill(p: &Palette) -> Color {
    if p.dark { p.field } else { rgb(0xF7F7F9) }
}

/// A grouped block within a panel, like the auto exposure controls.
pub fn well(theme: &Theme) -> container::Style {
    let p = Palette::from(theme);
    container::Style {
        background: Some(well_fill(p).into()),
        border: border(RADIUS_MEDIUM),
        ..container::Style::default()
    }
}

pub fn code(theme: &Theme) -> container::Style {
    let p = Palette::from(theme);
    container::Style {
        background: Some(p.field.into()),
        border: border(RADIUS_SMALL),
        text_color: Some(p.text),
        ..container::Style::default()
    }
}

/// A pane floating over the stage, like the inspector in a narrow window.
pub fn floating(theme: &Theme) -> container::Style {
    let p = Palette::from(theme);
    container::Style {
        background: Some(p.base.into()),
        shadow: Shadow {
            color: p.shadow,
            offset: Vector::new(-4.0, 0.0),
            blur_radius: 24.0,
        },
        text_color: Some(p.text),
        ..container::Style::default()
    }
}

pub fn sheet(theme: &Theme) -> container::Style {
    let p = Palette::from(theme);
    container::Style {
        background: Some(p.base.into()),
        border: hairline(
            if p.dark {
                p.hairline
            } else {
                Color::TRANSPARENT
            },
            RADIUS_SHEET,
        ),
        shadow: Shadow {
            color: p.shadow,
            offset: Vector::new(0.0, 12.0),
            blur_radius: 40.0,
        },
        text_color: Some(p.text),
        ..container::Style::default()
    }
}

pub fn scrim(theme: &Theme, shown: f32) -> container::Style {
    let scrim = Palette::from(theme).scrim;
    container::Style::default().background(alpha(scrim, scrim.a * shown))
}

pub fn tooltip(theme: &Theme) -> container::Style {
    let p = Palette::from(theme);
    container::Style {
        background: Some(if p.dark { rgb(0x3A3A3E) } else { rgb(0x2C2C2E) }.into()),
        border: border(RADIUS_SMALL),
        text_color: Some(rgb(0xF5F5F7)),
        shadow: Shadow {
            color: alpha(p.shadow, 0.2),
            offset: Vector::new(0.0, 2.0),
            blur_radius: 8.0,
        },
        ..container::Style::default()
    }
}

/// A small rounded label on a frame, such as "Last frame".
pub fn badge(theme: &Theme) -> container::Style {
    let _ = theme;
    container::Style {
        background: Some(alpha(rgb(0x000000), 0.82).into()),
        border: border(5.0),
        text_color: Some(Color::WHITE),
        ..container::Style::default()
    }
}

/// The groove of a segmented control.
pub fn segment_track(theme: &Theme) -> container::Style {
    let p = Palette::from(theme);
    container::Style {
        background: Some(segment_fill(p).into()),
        border: border(RADIUS),
        ..container::Style::default()
    }
}

/// The raised thumb marking a segmented control's selection.
pub fn segment_thumb(theme: &Theme) -> container::Style {
    let p = Palette::from(theme);
    container::Style {
        background: Some(thumb_fill(p).into()),
        border: border(RADIUS_SMALL),
        shadow: Shadow {
            color: alpha(Color::BLACK, if p.dark { 0.3 } else { 0.1 }),
            offset: Vector::new(0.0, 1.0),
            blur_radius: 2.0,
        },
        ..container::Style::default()
    }
}

fn segment_fill(p: &Palette) -> Color {
    if p.dark { p.field } else { rgb(0xEDEDF0) }
}

fn thumb_fill(p: &Palette) -> Color {
    if p.dark { rgb(0x4A4A50) } else { p.base }
}

// Buttons

fn button_base(background: Option<Color>, text: Color, radius: f32) -> button::Style {
    let outlined = super::motion::increase_contrast() && background.is_some();
    button::Style {
        background: background.map(Background::Color),
        text_color: text,
        border: if outlined {
            hairline(text, radius)
        } else {
            border(radius)
        },
        ..button::Style::default()
    }
}

pub fn primary(theme: &Theme, status: button::Status) -> button::Style {
    let p = Palette::from(theme);
    let fill = match status {
        button::Status::Active => p.accent_fill,
        button::Status::Hovered => mix(p.accent_fill, Color::BLACK, 0.08),
        button::Status::Pressed => mix(p.accent_fill, Color::BLACK, 0.16),
        button::Status::Disabled => p.field,
    };
    let mut style = button_base(Some(fill), Color::WHITE, RADIUS);
    if status == button::Status::Disabled {
        style.text_color = p.secondary;
    }
    style
}

/// The resting fill of `secondary` buttons.
fn gray(p: &Palette) -> Color {
    if p.dark { rgb(0x3B3B40) } else { rgb(0xECECEF) }
}

/// A gray, borderless button for secondary actions.
pub fn secondary(theme: &Theme, status: button::Status) -> button::Style {
    let p = Palette::from(theme);
    let rest = gray(p);
    let fill = match status {
        button::Status::Active | button::Status::Disabled => rest,
        button::Status::Hovered => mix(rest, p.text, 0.06),
        button::Status::Pressed => mix(rest, p.text, 0.12),
    };
    let text = if status == button::Status::Disabled {
        p.secondary
    } else {
        p.text
    };
    button_base(Some(fill), text, RADIUS)
}

/// Stopping acquisition is a routine action. Red belongs to removal/errors.
pub fn stop(theme: &Theme, status: button::Status) -> button::Style {
    secondary(theme, status)
}

/// Text or an icon with no chrome until hovered.
pub fn plain(theme: &Theme, status: button::Status) -> button::Style {
    let p = Palette::from(theme);
    match status {
        button::Status::Active => button_base(None, p.secondary, RADIUS_SMALL),
        button::Status::Hovered => button_base(Some(p.hover), p.text, RADIUS_SMALL),
        button::Status::Pressed => button_base(Some(p.selected), p.text, RADIUS_SMALL),
        button::Status::Disabled => button_base(None, p.secondary, RADIUS_SMALL),
    }
}

/// An accent-colored text action.
pub fn link(theme: &Theme, status: button::Status) -> button::Style {
    let p = Palette::from(theme);
    match status {
        button::Status::Active => button_base(None, p.accent_text, RADIUS_SMALL),
        button::Status::Hovered => button_base(Some(p.accent_soft), p.ink(p.accent), RADIUS_SMALL),
        button::Status::Pressed => {
            button_base(Some(alpha(p.accent, 0.24)), p.ink(p.accent), RADIUS_SMALL)
        }
        button::Status::Disabled => button_base(None, p.secondary, RADIUS_SMALL),
    }
}

pub fn card(theme: &Theme, status: button::Status) -> button::Style {
    let p = Palette::from(theme);
    let rest = if p.dark { p.field } else { p.base };
    let (fill, edge) = match status {
        button::Status::Hovered => (rest, alpha(p.accent, 0.55)),
        button::Status::Pressed => (mix(rest, p.accent, 0.08), p.accent),
        _ => (rest, p.hairline),
    };
    button::Style {
        border: hairline(edge, RADIUS_MEDIUM),
        ..button_base(Some(fill), p.text, RADIUS_MEDIUM)
    }
}

pub fn danger(theme: &Theme, status: button::Status) -> button::Style {
    let p = Palette::from(theme);
    let soft = alpha(p.danger, 0.12);
    let ink = p.ink(p.danger);
    match status {
        button::Status::Active => button_base(Some(soft), ink, RADIUS),
        button::Status::Hovered => button_base(Some(alpha(p.danger, 0.18)), ink, RADIUS),
        button::Status::Pressed => button_base(Some(alpha(p.danger, 0.26)), ink, RADIUS),
        button::Status::Disabled => button_base(Some(p.field), p.secondary, RADIUS),
    }
}

/// A sidebar list row, with the selected fill drawn in by `selected`, from
/// 0 to 1, so the highlight can cross-fade from one row to the next.
pub fn row(selected: f32, emphasized: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let p = Palette::from(theme);
        let selection = if emphasized {
            p.selected
        } else {
            Palette::window_appearance(p.dark, p.increased(), false).selected
        };
        let rest = match status {
            button::Status::Hovered => p.hover,
            button::Status::Pressed => selection,
            _ => alpha(selection, 0.0),
        };
        let fill = mix(rest, selection, selected.clamp(0.0, 1.0));
        button_base((fill.a > 0.0).then_some(fill), p.text, RADIUS)
    }
}

/// A label of a `widgets::segmented` control, over its sliding thumb. `lit`
/// is how much of the thumb sits under it, from 0 to 1, which brings its text
/// from `secondary` up to `text`.
pub fn segment_label(lit: f32) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let p = Palette::from(theme);
        let text = match status {
            button::Status::Hovered | button::Status::Pressed => p.text,
            button::Status::Disabled => mix(p.secondary, p.text, lit),
            button::Status::Active => mix(p.secondary, p.text, lit),
        };
        button_base(None, text, RADIUS_SMALL)
    }
}

/// An icon or text button on dark glass; `shown` fades it with its overlay.
pub fn on_glass(active: bool, shown: f32) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let rest = if active {
            stage_palette(Palette::from(theme).window_active).accent_text
        } else {
            ON_STAGE_SECONDARY
        };
        let (fill, text) = match status {
            button::Status::Active => (None, rest),
            button::Status::Hovered => (Some(alpha(Color::WHITE, 0.12)), ON_STAGE),
            button::Status::Pressed => (Some(alpha(Color::WHITE, 0.2)), ON_STAGE),
            button::Status::Disabled => (None, alpha(ON_STAGE, 0.35)),
        };
        button_base(
            fill.map(|fill| alpha(fill, fill.a * shown)),
            alpha(text, text.a * shown),
            RADIUS_SMALL,
        )
    }
}

/// A segment of a segmented control on dark glass.
pub fn glass_segment(
    selected: bool,
    shown: f32,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        if selected {
            button_base(
                Some(alpha(Color::WHITE, 0.2 * shown)),
                alpha(ON_STAGE, shown),
                RADIUS_SMALL,
            )
        } else {
            on_glass(false, shown)(theme, status)
        }
    }
}

/// A secondary button that stays tinted in accent while `on`, like Auto.
pub fn toggle(on: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        if !on {
            return secondary(theme, status);
        }
        let p = Palette::from(theme);
        let fill = match status {
            button::Status::Hovered => alpha(p.accent, 0.2),
            button::Status::Pressed => alpha(p.accent, 0.28),
            _ => p.accent_soft,
        };
        button_base(Some(fill), p.ink(p.accent), RADIUS)
    }
}

/// A frame preview that selects its camera when clicked.
pub fn bare(_theme: &Theme, _status: button::Status) -> button::Style {
    button_base(None, Color::TRANSPARENT, 0.0)
}

// Inputs

pub fn input(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let p = Palette::from(theme);
    let status = input_status(p, status);
    let border = match status {
        text_input::Status::Focused { .. } => Border {
            color: p.accent,
            width: if super::motion::increase_contrast() {
                2.5
            } else {
                1.5
            },
            radius: RADIUS_SMALL.into(),
        },
        text_input::Status::Hovered => hairline(p.secondary, RADIUS_SMALL),
        text_input::Status::Disabled => hairline(p.hairline, RADIUS_SMALL),
        _ => hairline(p.control_border, RADIUS_SMALL),
    };
    text_input::Style {
        background: if matches!(status, text_input::Status::Focused { .. }) {
            p.base
        } else {
            p.field
        }
        .into(),
        border,
        icon: p.secondary,
        placeholder: p.secondary,
        value: if status == text_input::Status::Disabled {
            p.secondary
        } else {
            p.text
        },
        selection: alpha(
            if p.window_active {
                p.accent
            } else {
                p.control_border
            },
            0.3,
        ),
    }
}

/// iced retains editor focus across window activation, including its Focused
/// style status. Keep that saved focus without drawing an active-window ring.
fn input_status(p: &Palette, status: text_input::Status) -> text_input::Status {
    if !p.window_active && matches!(status, text_input::Status::Focused { .. }) {
        text_input::Status::Active
    } else {
        status
    }
}

/// A field whose contents cannot be used yet, e.g. an out-of-range number.
pub fn input_invalid(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let p = Palette::from(theme);
    text_input::Style {
        border: Border {
            color: p.danger,
            width: 1.5,
            radius: RADIUS_SMALL.into(),
        },
        ..input(theme, status)
    }
}

/// A field holding an edit that has not been applied yet.
pub fn input_dirty(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let p = Palette::from(theme);
    let status = input_status(p, status);
    let mut style = input(theme, status);
    if !matches!(status, text_input::Status::Focused { .. }) {
        style.border = hairline(p.accent, RADIUS_SMALL);
    }
    style
}

/// A field acknowledging an accepted value: a success glow that fades out as
/// `t` goes from 1 to 0.
pub fn input_flash(t: f32) -> impl Fn(&Theme, text_input::Status) -> text_input::Style {
    move |theme, status| {
        let status = input_status(Palette::from(theme), status);
        let mut style = input(theme, status);
        if t <= 0.0 {
            return style;
        }
        let (background, border) = glow(style.background, Palette::from(theme), t);
        style.background = background;
        if !matches!(status, text_input::Status::Focused { .. }) {
            style.border = border;
        }
        style
    }
}

/// The green acknowledgement of an accepted value. The functional outline
/// remains visible as the success color fades back to its neutral border.
fn glow(fill: Background, p: &Palette, t: f32) -> (Background, Border) {
    let t = t.clamp(0.0, 1.0);
    let fill = match fill {
        Background::Color(fill) => mix(fill, p.live, 0.12 * t).into(),
        other => other,
    };
    let edge = Border {
        color: mix(p.control_border, p.live, t),
        width: 1.5,
        radius: RADIUS_SMALL.into(),
    };
    (fill, edge)
}

pub fn pick(theme: &Theme, status: pick_list::Status) -> pick_list::Style {
    let p = Palette::from(theme);
    pick_list::Style {
        text_color: p.text,
        placeholder_color: p.secondary,
        handle_color: p.secondary,
        background: match status {
            pick_list::Status::Active => p.field,
            _ => p.hover,
        }
        .into(),
        border: hairline(p.control_border, RADIUS_SMALL),
    }
}

/// A choice that no longer names an available camera or destination.
pub fn pick_invalid(theme: &Theme, status: pick_list::Status) -> pick_list::Style {
    pick_list::Style {
        border: Border {
            color: Palette::from(theme).danger,
            width: 1.5,
            radius: RADIUS_SMALL.into(),
        },
        ..pick(theme, status)
    }
}

/// A reviewed choice changed by the assistant after the person edited it.
pub fn pick_dirty(theme: &Theme, status: pick_list::Status) -> pick_list::Style {
    pick_list::Style {
        border: hairline(Palette::from(theme).accent, RADIUS_SMALL),
        ..pick(theme, status)
    }
}

/// A `pick` acknowledging an accepted choice, as `input_flash` does a field.
pub fn pick_flash(t: f32) -> impl Fn(&Theme, pick_list::Status) -> pick_list::Style {
    move |theme, status| {
        let mut style = pick(theme, status);
        if t <= 0.0 {
            return style;
        }
        (style.background, style.border) = glow(style.background, Palette::from(theme), t);
        style
    }
}

pub fn menu(theme: &Theme) -> menu::Style {
    let p = Palette::from(theme);
    menu::Style {
        background: if p.dark { rgb(0x2E2E32) } else { p.base }.into(),
        border: hairline(p.hairline, RADIUS),
        text_color: p.text,
        selected_text_color: Color::WHITE,
        selected_background: p.accent_fill.into(),
        shadow: Shadow {
            color: p.shadow,
            offset: Vector::new(0.0, 6.0),
            blur_radius: 18.0,
        },
    }
}

pub fn check(theme: &Theme, status: checkbox::Status) -> checkbox::Style {
    let p = Palette::from(theme);
    let (checked, hovered, disabled) = match status {
        checkbox::Status::Active { is_checked } => (is_checked, false, false),
        checkbox::Status::Hovered { is_checked } => (is_checked, true, false),
        checkbox::Status::Disabled { is_checked } => (is_checked, false, true),
    };
    let fill = if disabled { p.tertiary } else { p.accent_fill };
    checkbox::Style {
        background: if checked { fill } else { p.base }.into(),
        icon_color: Color::WHITE,
        border: Border {
            color: if checked {
                fill
            } else if hovered {
                p.secondary
            } else {
                p.control_border
            },
            width: 1.5,
            radius: 4.5.into(),
        },
        text_color: Some(if disabled { p.secondary } else { p.text }),
    }
}

pub fn slide(theme: &Theme, status: slider::Status) -> slider::Style {
    let p = Palette::from(theme);
    // A white thumb against the inactive macOS gray rail needs a separate
    // dark edge; reusing the gray fill would blend both edges into the rail.
    let strong_handle = p.increased() || (!p.window_active && cfg!(target_os = "macos"));
    slider::Style {
        rail: slider::Rail {
            backgrounds: (p.accent.into(), track(p).into()),
            width: 4.0,
            border: border(2.0),
        },
        handle: slider::Handle {
            shape: slider::HandleShape::Circle {
                radius: if status == slider::Status::Dragged {
                    8.0
                } else {
                    7.0
                },
            },
            background: Color::WHITE.into(),
            border_width: if strong_handle { 2.0 } else { 1.0 },
            border_color: if strong_handle {
                Color::BLACK
            } else {
                p.accent_fill
            },
        },
    }
}

pub fn progress(theme: &Theme) -> progress_bar::Style {
    let p = Palette::from(theme);
    progress_bar::Style {
        background: track(p).into(),
        bar: p.accent.into(),
        border: border(2.0),
    }
}

pub fn line(theme: &Theme) -> rule::Style {
    rule::Style {
        color: Palette::from(theme).hairline,
        radius: border::Radius::default(),
        fill_mode: rule::FillMode::Full,
        snap: true,
    }
}

pub fn scroll(theme: &Theme, status: scrollable::Status) -> scrollable::Style {
    let p = Palette::from(theme);
    let always = super::motion::always_show_scrollbars() || super::motion::increase_contrast();
    let shown: f32 = match status {
        scrollable::Status::Active { .. } => 0.0,
        scrollable::Status::Hovered {
            is_vertical_scrollbar_hovered,
            ..
        } => {
            if is_vertical_scrollbar_hovered {
                0.5
            } else {
                0.28
            }
        }
        scrollable::Status::Dragged { .. } => 0.55,
    };
    let shown = shown.max(if always { 1.0 } else { 0.0 });
    let rail = scrollable::Rail {
        background: None,
        border: border(3.0),
        scroller: scrollable::Scroller {
            background: alpha(p.secondary, shown).into(),
            border: border(3.0),
        },
    };
    scrollable::Style {
        container: container::Style::default(),
        vertical_rail: rail,
        horizontal_rail: rail,
        gap: None,
        auto_scroll: scrollable::AutoScroll {
            background: p.base.into(),
            border: hairline(p.hairline, 999.0),
            shadow: Shadow::default(),
            icon: p.secondary,
        },
    }
}

/// `scroll` for sheets: a faint scroller stays visible at rest whenever the
/// content overflows, so it is clear there is more below.
pub fn sheet_scroll(theme: &Theme, status: scrollable::Status) -> scrollable::Style {
    let mut style = scroll(theme, status);
    if !super::motion::always_show_scrollbars()
        && !super::motion::increase_contrast()
        && let scrollable::Status::Active {
            is_vertical_scrollbar_disabled: false,
            ..
        } = status
    {
        style.vertical_rail.scroller.background =
            alpha(Palette::from(theme).secondary, 0.22).into();
    }
    style
}

#[cfg(test)]
mod contrast_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// `color` laid over `surface` at `amount`, as the renderer blends it.
    fn tint(color: Color, amount: f32, surface: Color) -> Color {
        mix(surface, Color { a: 1.0, ..color }, amount)
    }

    fn ratio(a: Color, b: Color) -> f32 {
        contrast(luminance(a), luminance(b))
    }

    #[test]
    fn contrast_follows_wcag() {
        assert!((ratio(Color::WHITE, Color::BLACK) - 21.0).abs() < 0.01);
        assert!((ratio(rgb(0x777777), Color::WHITE) - 4.48).abs() < 0.01);
    }

    #[test]
    fn increased_contrast_strengthens_labels_and_panel_boundaries() {
        for (normal, increased) in [(&LIGHT, &LIGHT_CONTRAST), (&DARK, &DARK_CONTRAST)] {
            for surface in increased.grounds() {
                assert!(ratio(increased.secondary, surface) >= 6.0);
                assert!(ratio(increased.control_border, surface) >= 3.0);
                assert!(ratio(increased.hairline, surface) >= 3.0);
            }
            assert!(
                ratio(increased.secondary, increased.base) > ratio(normal.secondary, normal.base)
            );
            assert!(
                ratio(increased.hairline, increased.base) > ratio(normal.hairline, normal.base)
            );
            assert!(ratio(Color::WHITE, increased.accent_fill) >= 4.5);
            assert!(ratio(increased.accent_text, increased.base) >= 4.5);
            assert_eq!(
                increased.dark, normal.dark,
                "preserves the selected appearance"
            );
        }
    }

    #[test]
    fn increased_contrast_themes_select_the_matching_widget_palette() {
        for dark in [false, true] {
            for increased in [false, true] {
                for active in [false, true] {
                    let theme = window_theme_appearance(dark, increased, active);
                    let p = Palette::window_appearance(dark, increased, active);
                    assert!(std::ptr::eq(Palette::from(&theme), p));
                    assert_eq!(theme.palette().background, p.base);
                    assert_eq!(theme.palette().text, p.text);
                    assert_eq!(theme.extended_palette().primary.base.color, p.accent_fill);
                    assert_eq!(theme.extended_palette().primary.base.text, Color::WHITE);
                    assert_eq!(Palette::from(&theme).window_active, active);
                }
            }
        }
    }

    #[test]
    fn inactive_platform_policies_keep_status_and_readable_selected_content() {
        for active in [LIGHT, DARK, LIGHT_CONTRAST, DARK_CONTRAST] {
            for neutral_controls in [false, true] {
                let p = inactive(Palette { ..active }, neutral_controls);
                assert!(!p.window_active);
                assert_ne!(p.selected, active.selected);
                assert_eq!((p.text, p.secondary), (active.text, active.secondary));
                assert_eq!(
                    (p.live, p.warn, p.danger),
                    (active.live, active.warn, active.danger)
                );
                assert_eq!(
                    (p.base, p.sidebar, p.field),
                    (active.base, active.sidebar, active.field)
                );
                assert!(ratio(p.text, p.selected) >= 4.5);
                assert!(ratio(p.secondary, p.selected) >= 4.5);
                assert!(ratio(p.control_border, p.selected) >= 3.0);
                assert!(ratio(Color::WHITE, p.accent_fill) >= 4.5);
                if neutral_controls {
                    assert_ne!(p.accent_fill, active.accent_fill);
                    assert!(ratio(p.accent_text, p.base) >= 4.5);
                } else {
                    assert_eq!(
                        (p.accent, p.accent_fill, p.accent_text),
                        (active.accent, active.accent_fill, active.accent_text)
                    );
                }
            }
        }
    }

    #[test]
    fn inactive_editor_focus_is_not_disabled_or_drawn_as_the_key_window() {
        for dark in [false, true] {
            for increased in [false, true] {
                let theme = window_theme_appearance(dark, increased, false);
                let p = Palette::from(&theme);
                let focused = text_input::Status::Focused { is_hovered: false };
                let ordinary = input(&theme, text_input::Status::Active);
                let unfocused = input(&theme, focused);
                assert_eq!(unfocused.border, ordinary.border);
                assert_eq!(unfocused.background, ordinary.background);
                assert_eq!(unfocused.value, p.text);
                assert_ne!(
                    unfocused.value,
                    input(&theme, text_input::Status::Disabled).value
                );
                assert_eq!(input_invalid(&theme, focused).border.color, p.danger);
                assert_eq!(input_dirty(&theme, focused).border.color, p.accent);
                assert_eq!(
                    ring(1.0, true, RADIUS_MEDIUM)(&theme).border.color,
                    STAGE_INACTIVE.accent
                );
                assert_ne!(
                    ring(1.0, true, RADIUS_MEDIUM)(&theme).border.color,
                    STAGE.accent
                );
            }
        }
    }

    #[test]
    fn window_themes_remain_independent_while_another_window_takes_focus() {
        let main = window_theme_appearance(false, false, false);
        let settings = window_theme_appearance(false, false, true);
        assert_ne!(main.name(), settings.name());
        assert_eq!(
            row(1.0, true)(&main, button::Status::Active).background,
            Some(INACTIVE[0].selected.into())
        );
        assert_eq!(
            row(1.0, true)(&settings, button::Status::Active).background,
            Some(LIGHT.selected.into())
        );
        let restored = window_theme_appearance(false, false, true);
        assert_eq!(
            row(1.0, true)(&restored, button::Status::Active).background,
            Some(LIGHT.selected.into())
        );
        // Looking up a restored foreground window must not recolor the
        // already constructed background window or allocate a new theme.
        assert!(!Palette::from(&main).window_active);
        let (Theme::Custom(settings), Theme::Custom(restored)) = (settings, restored) else {
            panic!("window themes must use the cached custom appearances");
        };
        assert!(std::sync::Arc::ptr_eq(&settings, &restored));
    }

    #[test]
    fn ink_reads_on_tints_of_its_color() {
        for p in [&LIGHT, &DARK, STAGE] {
            for color in [p.live, p.warn, p.danger, p.accent, p.accent_text] {
                let ink = p.ink(color);
                for surface in [p.base, p.sidebar] {
                    for amount in [0.12, 0.14, 0.18, 0.22, 0.26, 0.30] {
                        let r = ratio(ink, tint(color, amount, surface));
                        assert!(r >= 4.5, "{color:?} at {amount} (dark {}): {r:.2}", p.dark);
                    }
                }
                for surface in p.grounds() {
                    let r = ratio(ink, surface);
                    assert!(
                        r >= 4.5,
                        "{color:?} on {surface:?} (dark {}): {r:.2}",
                        p.dark
                    );
                }
            }
        }
    }

    /// Hue in degrees, 0 to 360.
    fn hue(color: Color) -> f32 {
        let (r, g, b) = (color.r, color.g, color.b);
        let max = r.max(g).max(b);
        let span = max - r.min(g).min(b);
        let hue = if span == 0.0 {
            0.0
        } else if max == r {
            60.0 * ((g - b) / span)
        } else if max == g {
            60.0 * ((b - r) / span + 2.0)
        } else {
            60.0 * ((r - g) / span + 4.0)
        };
        hue.rem_euclid(360.0)
    }

    #[test]
    fn warnings_read_as_amber_not_as_errors() {
        for p in [&LIGHT, &DARK] {
            let apart = (hue(p.warn) - hue(p.danger)).rem_euclid(360.0);
            assert!(
                (30.0..=60.0).contains(&apart),
                "warn is {apart:.0}° from danger (dark {})",
                p.dark
            );
            // A stalled camera's dot on its selected row reads like the accent.
            let r = ratio(p.warn, p.selected);
            assert!(r >= 3.0, "warn on selected (dark {}): {r:.2}", p.dark);
        }
    }

    #[test]
    fn ink_keeps_colors_that_already_read() {
        assert_eq!(LIGHT.ink(LIGHT.text), LIGHT.text);
        let faded = Color {
            a: 0.5,
            ..LIGHT.danger
        };
        assert_eq!(LIGHT.ink(faded).a, 0.5);
    }

    #[test]
    fn white_reads_on_filled_buttons() {
        for p in [&LIGHT, &DARK] {
            let r = ratio(Color::WHITE, p.accent_fill);
            assert!(r >= 4.5, "dark {}: {r:.2}", p.dark);
        }
    }

    #[test]
    fn secondary_text_reads_on_panels() {
        for p in [&LIGHT, &DARK] {
            let track = if p.dark { p.field } else { rgb(0xEDEDF0) };
            for surface in [
                p.base,
                p.sidebar,
                p.hover,
                p.selected,
                p.field,
                well_fill(p),
                track,
                gray(p),
            ] {
                let r = ratio(p.secondary, surface);
                assert!(r >= 4.5, "{surface:?} (dark {}): {r:.2}", p.dark);
            }
        }
    }

    /// `color`, perhaps translucent, as it shows over `surface`.
    fn over(color: Color, surface: Color) -> Color {
        tint(color, color.a, surface)
    }

    #[test]
    fn stage_text_reads_on_glass_over_bright_pictures() {
        for picture in [Color::WHITE, rgb(0xC0C0C0), rgb(0x808080)] {
            let Some(Background::Color(fill)) = glass(1.0, false)(&theme(true)).background else {
                panic!("glass needs a solid color backing");
            };
            let glass = over(fill, picture);
            for text in [STAGE.text, STAGE.secondary] {
                let r = ratio(over(text, glass), glass);
                assert!(r >= 4.5, "{text:?} over {picture:?}: {r:.2}");
            }
            // Active glyphs are marks: 3:1.
            let r = ratio(STAGE.accent_text, glass);
            assert!(r >= 3.0, "active glyph over {picture:?}: {r:.2}");
        }
    }

    #[test]
    fn caption_text_reads_over_white_pictures() {
        // The text row sits around the middle of the caption's shade.
        let [(_, bottom), (stop, held), _] = CAPTION_SHADE;
        let dark = bottom + (held - bottom) * (0.5 / stop);
        let shade = over(alpha(Color::BLACK, dark), Color::WHITE);
        let r = ratio(STAGE.text, shade);
        assert!(r >= 4.5, "{r:.2}");
    }
}
