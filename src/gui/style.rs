//! The workbench's look: a quiet, Things-like palette of white space, hairlines
//! and one blue accent, with matching light and dark variants.
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

/// Named colors of one appearance. Views take these from `Palette::of(dark)`,
/// styles from the theme, so both always agree.
pub struct Palette {
    pub dark: bool,
    /// Main content and inspector.
    pub base: Color,
    pub sidebar: Color,
    /// Rows under the pointer, subtle buttons and fields.
    pub hover: Color,
    pub selected: Color,
    pub field: Color,
    pub stage: Color,
    pub hairline: Color,
    pub text: Color,
    pub secondary: Color,
    pub tertiary: Color,
    pub accent: Color,
    /// Accent for text and small marks; darker in light mode for contrast.
    pub accent_text: Color,
    pub accent_soft: Color,
    pub live: Color,
    pub warn: Color,
    pub danger: Color,
    pub scrim: Color,
    pub shadow: Color,
}

pub const LIGHT: Palette = Palette {
    dark: false,
    base: rgb(0xFFFFFF),
    sidebar: rgb(0xF5F5F7),
    hover: rgb(0xEDEDF0),
    selected: rgb(0xE3E4E9),
    field: rgb(0xF1F1F4),
    stage: rgb(0xF0F0F3),
    hairline: rgb(0xE5E5E9),
    text: rgb(0x1D1D1F),
    secondary: rgb(0x86868B),
    tertiary: rgb(0xB5B5BA),
    accent: rgb(0x2F7BF5),
    accent_text: rgb(0x1F6AE0),
    accent_soft: alpha(rgb(0x2F7BF5), 0.12),
    live: rgb(0x2BB24C),
    warn: rgb(0xD97706),
    danger: rgb(0xE5372C),
    scrim: alpha(rgb(0x000000), 0.18),
    shadow: alpha(rgb(0x000000), 0.12),
};

pub const DARK: Palette = Palette {
    dark: true,
    base: rgb(0x1E1E20),
    sidebar: rgb(0x29292C),
    hover: rgb(0x323236),
    selected: rgb(0x3B3B40),
    field: rgb(0x2C2C30),
    stage: rgb(0x151517),
    hairline: rgb(0x343438),
    text: rgb(0xEDEDF0),
    secondary: rgb(0x98989F),
    tertiary: rgb(0x5F5F66),
    accent: rgb(0x4C8DF8),
    accent_text: rgb(0x6AA1FA),
    accent_soft: alpha(rgb(0x4C8DF8), 0.18),
    live: rgb(0x32D158),
    warn: rgb(0xFF9F0A),
    danger: rgb(0xFF453A),
    scrim: alpha(rgb(0x000000), 0.42),
    shadow: alpha(rgb(0x000000), 0.45),
};

impl Palette {
    pub fn of(dark: bool) -> &'static Palette {
        if dark { &DARK } else { &LIGHT }
    }
    fn from(theme: &Theme) -> &'static Palette {
        Self::of(theme.extended_palette().is_dark)
    }
}

/// The iced theme for an appearance; widgets left unstyled still match.
pub fn theme(dark: bool) -> Theme {
    static THEMES: OnceLock<[Theme; 2]> = OnceLock::new();
    let themes = THEMES.get_or_init(|| {
        [&LIGHT, &DARK].map(|p| {
            Theme::custom(
                if p.dark {
                    "Capturefab Dark"
                } else {
                    "Capturefab Light"
                },
                iced::theme::Palette {
                    background: p.base,
                    text: p.text,
                    primary: p.accent,
                    success: p.live,
                    warning: p.warn,
                    danger: p.danger,
                },
            )
        })
    });
    themes[usize::from(dark)].clone()
}

/// The platform's interface font, by family name.
pub const SANS: Font = Font::with_name(if cfg!(target_os = "macos") {
    "System Font"
} else if cfg!(target_os = "windows") {
    "Segoe UI"
} else {
    "Noto Sans"
});

pub const MONO: Font = Font::with_name(if cfg!(target_os = "macos") {
    "Menlo"
} else if cfg!(target_os = "windows") {
    "Consolas"
} else {
    "DejaVu Sans Mono"
});

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

/// Type scale.
pub const TITLE: f32 = 26.0;
pub const HEADING: f32 = 15.0;
pub const BODY: f32 = 13.0;
pub const SMALL: f32 = 12.0;
pub const CAPTION: f32 = 11.0;

pub const RADIUS: f32 = 7.0;

fn border(radius: f32) -> Border {
    Border {
        radius: radius.into(),
        ..Border::default()
    }
}

fn hairline(color: Color, radius: f32) -> Border {
    Border {
        color,
        width: 1.0,
        radius: radius.into(),
    }
}

fn mix(a: Color, b: Color, t: f32) -> Color {
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
    container::Style {
        background: Some(p.stage.into()),
        border: border(12.0),
        ..container::Style::default()
    }
}

/// A grouped block within a panel, like the auto exposure controls.
pub fn well(theme: &Theme) -> container::Style {
    let p = Palette::from(theme);
    container::Style {
        background: Some(if p.dark { p.field } else { rgb(0xF7F7F9) }.into()),
        border: border(10.0),
        ..container::Style::default()
    }
}

pub fn code(theme: &Theme) -> container::Style {
    let p = Palette::from(theme);
    container::Style {
        background: Some(p.field.into()),
        border: border(6.0),
        text_color: Some(p.text),
        ..container::Style::default()
    }
}

pub fn tile(selected: bool) -> impl Fn(&Theme) -> container::Style {
    move |theme| {
        let p = Palette::from(theme);
        container::Style {
            background: Some(p.base.into()),
            border: if selected {
                Border {
                    color: p.accent,
                    width: 2.0,
                    radius: 12.0.into(),
                }
            } else {
                hairline(p.hairline, 12.0)
            },
            shadow: Shadow {
                color: alpha(p.shadow, if p.dark { 0.25 } else { 0.05 }),
                offset: Vector::new(0.0, 1.0),
                blur_radius: 4.0,
            },
            ..container::Style::default()
        }
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
            14.0,
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

pub fn scrim(theme: &Theme) -> container::Style {
    container::Style::default().background(Palette::from(theme).scrim)
}

pub fn tooltip(theme: &Theme) -> container::Style {
    let p = Palette::from(theme);
    container::Style {
        background: Some(if p.dark { rgb(0x3A3A3E) } else { rgb(0x2C2C2E) }.into()),
        border: border(6.0),
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
        background: Some(alpha(rgb(0x000000), 0.55).into()),
        border: border(5.0),
        text_color: Some(Color::WHITE),
        ..container::Style::default()
    }
}

/// The selected segment of a segmented control.
pub fn segment_track(theme: &Theme) -> container::Style {
    let p = Palette::from(theme);
    container::Style {
        background: Some(if p.dark { p.field } else { rgb(0xEDEDF0) }.into()),
        border: border(8.0),
        ..container::Style::default()
    }
}

// Buttons

fn button_base(background: Option<Color>, text: Color, radius: f32) -> button::Style {
    button::Style {
        background: background.map(Background::Color),
        text_color: text,
        border: border(radius),
        ..button::Style::default()
    }
}

pub fn primary(theme: &Theme, status: button::Status) -> button::Style {
    let p = Palette::from(theme);
    let fill = match status {
        button::Status::Active => p.accent,
        button::Status::Hovered => mix(p.accent, Color::BLACK, 0.08),
        button::Status::Pressed => mix(p.accent, Color::BLACK, 0.16),
        button::Status::Disabled => alpha(p.accent, 0.35),
    };
    let mut style = button_base(Some(fill), Color::WHITE, RADIUS);
    if status == button::Status::Disabled {
        style.text_color = alpha(Color::WHITE, 0.8);
    }
    style
}

/// A gray, borderless button for secondary actions.
pub fn secondary(theme: &Theme, status: button::Status) -> button::Style {
    let p = Palette::from(theme);
    let rest = if p.dark { p.selected } else { rgb(0xECECEF) };
    let fill = match status {
        button::Status::Active | button::Status::Disabled => rest,
        button::Status::Hovered => mix(rest, p.text, 0.06),
        button::Status::Pressed => mix(rest, p.text, 0.12),
    };
    let text = if status == button::Status::Disabled {
        p.tertiary
    } else {
        p.text
    };
    button_base(Some(fill), text, RADIUS)
}

/// Text or an icon with no chrome until hovered.
pub fn plain(theme: &Theme, status: button::Status) -> button::Style {
    let p = Palette::from(theme);
    match status {
        button::Status::Active => button_base(None, p.secondary, 6.0),
        button::Status::Hovered => button_base(Some(p.hover), p.text, 6.0),
        button::Status::Pressed => button_base(Some(p.selected), p.text, 6.0),
        button::Status::Disabled => button_base(None, p.tertiary, 6.0),
    }
}

/// An accent-colored text action.
pub fn link(theme: &Theme, status: button::Status) -> button::Style {
    let p = Palette::from(theme);
    match status {
        button::Status::Active => button_base(None, p.accent_text, 6.0),
        button::Status::Hovered => button_base(Some(p.accent_soft), p.accent_text, 6.0),
        button::Status::Pressed => button_base(Some(alpha(p.accent, 0.24)), p.accent_text, 6.0),
        button::Status::Disabled => button_base(None, p.tertiary, 6.0),
    }
}

pub fn danger(theme: &Theme, status: button::Status) -> button::Style {
    let p = Palette::from(theme);
    let soft = alpha(p.danger, 0.12);
    match status {
        button::Status::Active => button_base(Some(soft), p.danger, RADIUS),
        button::Status::Hovered => button_base(Some(alpha(p.danger, 0.18)), p.danger, RADIUS),
        button::Status::Pressed => button_base(Some(alpha(p.danger, 0.26)), p.danger, RADIUS),
        button::Status::Disabled => button_base(Some(soft), p.tertiary, RADIUS),
    }
}

/// A sidebar list row; `selected` keeps the highlight.
pub fn row(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let p = Palette::from(theme);
        let fill = match (selected, status) {
            (true, _) => Some(p.selected),
            (false, button::Status::Hovered) => Some(p.hover),
            (false, button::Status::Pressed) => Some(p.selected),
            _ => None,
        };
        button_base(fill, p.text, RADIUS)
    }
}

pub fn segment(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let p = Palette::from(theme);
        if selected {
            let mut style = button_base(
                Some(if p.dark { rgb(0x4A4A50) } else { p.base }),
                p.text,
                6.0,
            );
            style.shadow = Shadow {
                color: alpha(Color::BLACK, if p.dark { 0.3 } else { 0.1 }),
                offset: Vector::new(0.0, 1.0),
                blur_radius: 2.0,
            };
            style
        } else {
            let text = match status {
                button::Status::Hovered | button::Status::Pressed => p.text,
                button::Status::Disabled => p.tertiary,
                button::Status::Active => p.secondary,
            };
            button_base(None, text, 6.0)
        }
    }
}

/// A frame preview that selects its camera when clicked.
pub fn bare(_theme: &Theme, _status: button::Status) -> button::Style {
    button_base(None, Color::TRANSPARENT, 0.0)
}

// Inputs

pub fn input(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let p = Palette::from(theme);
    let border = match status {
        text_input::Status::Focused { .. } => Border {
            color: alpha(p.accent, 0.8),
            width: 1.5,
            radius: 6.0.into(),
        },
        text_input::Status::Hovered => hairline(p.hairline, 6.0),
        _ => hairline(Color::TRANSPARENT, 6.0),
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
        placeholder: p.tertiary,
        value: if status == text_input::Status::Disabled {
            p.secondary
        } else {
            p.text
        },
        selection: alpha(p.accent, 0.3),
    }
}

/// A field whose contents cannot be used yet, e.g. an out-of-range number.
pub fn input_invalid(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let p = Palette::from(theme);
    text_input::Style {
        border: Border {
            color: p.danger,
            width: 1.5,
            radius: 6.0.into(),
        },
        ..input(theme, status)
    }
}

pub fn pick(theme: &Theme, status: pick_list::Status) -> pick_list::Style {
    let p = Palette::from(theme);
    pick_list::Style {
        text_color: p.text,
        placeholder_color: p.tertiary,
        handle_color: p.secondary,
        background: match status {
            pick_list::Status::Active => p.field,
            _ => p.hover,
        }
        .into(),
        border: hairline(Color::TRANSPARENT, 6.0),
    }
}

pub fn menu(theme: &Theme) -> menu::Style {
    let p = Palette::from(theme);
    menu::Style {
        background: if p.dark { rgb(0x2E2E32) } else { p.base }.into(),
        border: hairline(p.hairline, 8.0),
        text_color: p.text,
        selected_text_color: Color::WHITE,
        selected_background: p.accent.into(),
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
    let fill = if disabled { p.tertiary } else { p.accent };
    checkbox::Style {
        background: if checked { fill } else { p.base }.into(),
        icon_color: Color::WHITE,
        border: Border {
            color: if checked {
                fill
            } else if hovered {
                p.secondary
            } else {
                p.tertiary
            },
            width: 1.5,
            radius: 4.5.into(),
        },
        text_color: Some(if disabled { p.secondary } else { p.text }),
    }
}

pub fn slide(theme: &Theme, status: slider::Status) -> slider::Style {
    let p = Palette::from(theme);
    slider::Style {
        rail: slider::Rail {
            backgrounds: (p.accent.into(), p.hairline.into()),
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
            border_width: 1.0,
            border_color: if p.dark {
                Color::TRANSPARENT
            } else {
                rgb(0xD2D2D7)
            },
        },
    }
}

pub fn progress(theme: &Theme) -> progress_bar::Style {
    let p = Palette::from(theme);
    progress_bar::Style {
        background: p.hairline.into(),
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
    let shown = match status {
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
