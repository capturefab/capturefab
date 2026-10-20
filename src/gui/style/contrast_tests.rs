//! Regressions for the styles actually returned to iced, including layered
//! backgrounds. Disabled action labels remain readable, while disabled
//! outlines and entrance/exit fades are excluded from functional checks.
use super::*;

fn composite(front: Color, back: Color) -> Color {
    Color {
        r: front.r * front.a + back.r * (1.0 - front.a),
        g: front.g * front.a + back.g * (1.0 - front.a),
        b: front.b * front.a + back.b * (1.0 - front.a),
        a: 1.0,
    }
}

fn solid(background: Background) -> Color {
    match background {
        Background::Color(color) => color,
        Background::Gradient(_) => panic!("expected a flat control background"),
    }
}

fn backdrop(background: Option<Background>, surface: Color) -> Color {
    background.map_or(surface, |background| composite(solid(background), surface))
}

fn ratio(front: Color, back: Color) -> f32 {
    let luminance = |color: Color| {
        let linear = |value: f32| {
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
    };
    let (front, back) = (luminance(composite(front, back)), luminance(back));
    (front.max(back) + 0.05) / (front.min(back) + 0.05)
}

fn reads(front: Color, back: Color, minimum: f32, context: &str) {
    let contrast = ratio(front, back);
    assert!(
        contrast >= minimum,
        "{context}: {front:?} over {back:?} is {contrast:.2}:1, needs {minimum}:1"
    );
}

fn panel_surfaces(p: &Palette, theme: &Theme) -> [Color; 6] {
    [
        p.base,
        p.sidebar,
        p.field,
        p.selected,
        p.hover,
        solid(well(theme).background.unwrap()),
    ]
}

const BUTTON_STATES: [button::Status; 3] = [
    button::Status::Active,
    button::Status::Hovered,
    button::Status::Pressed,
];

fn window_appearances() -> impl Iterator<Item = (Theme, &'static Palette)> {
    [false, true].into_iter().flat_map(|dark| {
        [false, true].into_iter().flat_map(move |increased| {
            [false, true].into_iter().map(move |active| {
                (
                    window_theme_appearance(dark, increased, active),
                    Palette::window_appearance(dark, increased, active),
                )
            })
        })
    })
}

#[test]
fn disabled_actions_keep_their_names_readable() {
    for (theme, p) in window_appearances() {
        let status = button::Status::Disabled;
        for style in [
            primary(&theme, status),
            secondary(&theme, status),
            stop(&theme, status),
            plain(&theme, status),
            link(&theme, status),
            danger(&theme, status),
        ] {
            for surface in [p.base, p.sidebar] {
                reads(
                    style.text_color,
                    backdrop(style.background, surface),
                    4.5,
                    "unavailable action label",
                );
            }
        }
        let track = backdrop(segment_track(&theme).background, p.base);
        let thumb = backdrop(segment_thumb(&theme).background, track);
        reads(
            segment_label(0.0)(&theme, status).text_color,
            track,
            4.5,
            "unavailable segment label",
        );
        reads(
            segment_label(1.0)(&theme, status).text_color,
            thumb,
            4.5,
            "unavailable selected segment label",
        );
    }
}

#[test]
fn enabled_input_styles_keep_labels_selection_and_boundaries_visible() {
    for (theme, p) in window_appearances() {
        for status in [
            text_input::Status::Active,
            text_input::Status::Hovered,
            text_input::Status::Focused { is_hovered: false },
            text_input::Status::Focused { is_hovered: true },
        ] {
            let mut styles = vec![
                input(&theme, status),
                input_dirty(&theme, status),
                input_invalid(&theme, status),
            ];
            styles.extend([0.01, 0.25, 0.5, 0.75, 1.0].map(|t| input_flash(t)(&theme, status)));
            for style in styles {
                for surface in [p.base, p.sidebar, solid(well(&theme).background.unwrap())] {
                    let fill = composite(solid(style.background), surface);
                    reads(style.value, fill, 4.5, "input value");
                    reads(style.placeholder, fill, 4.5, "input placeholder");
                    reads(style.icon, fill, 3.0, "input icon");
                    reads(
                        style.value,
                        composite(style.selection, fill),
                        4.5,
                        "selected input text",
                    );
                    assert!(
                        style.border.width > 0.0,
                        "enabled field needs a visible outline"
                    );
                    reads(style.border.color, fill, 3.0, "input outline against field");
                    reads(
                        style.border.color,
                        surface,
                        3.0,
                        "input outline against panel",
                    );
                }
            }
        }
    }
}

#[test]
fn pick_list_and_menu_outputs_keep_choices_and_placeholders_readable() {
    for (theme, p) in window_appearances() {
        for status in [
            pick_list::Status::Active,
            pick_list::Status::Hovered,
            pick_list::Status::Opened { is_hovered: false },
            pick_list::Status::Opened { is_hovered: true },
        ] {
            let mut styles = vec![
                pick(&theme, status),
                pick_invalid(&theme, status),
                pick_dirty(&theme, status),
            ];
            styles.extend([0.01, 0.25, 0.5, 0.75, 1.0].map(|t| pick_flash(t)(&theme, status)));
            for style in styles {
                for surface in [p.base, p.sidebar, solid(well(&theme).background.unwrap())] {
                    let fill = composite(solid(style.background), surface);
                    reads(style.text_color, fill, 4.5, "pick value");
                    reads(style.placeholder_color, fill, 4.5, "pick placeholder");
                    reads(style.handle_color, fill, 3.0, "pick arrow");
                    assert!(
                        style.border.width > 0.0,
                        "pick list needs a visible outline"
                    );
                    reads(style.border.color, fill, 3.0, "pick outline against field");
                    reads(
                        style.border.color,
                        surface,
                        3.0,
                        "pick outline against panel",
                    );
                }
            }
        }
        let style = menu(&theme);
        reads(
            style.text_color,
            solid(style.background),
            4.5,
            "menu option",
        );
        reads(
            style.selected_text_color,
            solid(style.selected_background),
            4.5,
            "selected menu option",
        );
    }
}

#[test]
fn enabled_action_styles_read_in_every_interaction_state() {
    for (theme, p) in window_appearances() {
        for status in BUTTON_STATES {
            let styles = [
                primary(&theme, status),
                secondary(&theme, status),
                stop(&theme, status),
                plain(&theme, status),
                link(&theme, status),
                danger(&theme, status),
                card(&theme, status),
                toggle(false)(&theme, status),
                toggle(true)(&theme, status),
            ];
            for style in styles {
                for surface in panel_surfaces(p, &theme) {
                    reads(
                        style.text_color,
                        backdrop(style.background, surface),
                        4.5,
                        "action text",
                    );
                }
            }
        }
        for color in [p.live, p.warn, p.danger, p.accent] {
            let style = pill(color)(&theme);
            for surface in panel_surfaces(p, &theme) {
                let fill = backdrop(style.background, surface);
                reads(style.text_color.unwrap(), fill, 4.5, "status pill text");
            }
            // The title bar's status_pill supplies an explicit dot, which
            // does not inherit the container's contrast-adjusted text color.
            for surface in [p.base, p.sidebar] {
                reads(
                    color,
                    backdrop(style.background, surface),
                    3.0,
                    "status pill dot",
                );
            }
        }
        let track = backdrop(segment_track(&theme).background, p.base);
        let thumb = backdrop(segment_thumb(&theme).background, track);
        for status in BUTTON_STATES {
            reads(
                segment_label(0.0)(&theme, status).text_color,
                track,
                4.5,
                "unselected segment",
            );
            reads(
                segment_label(1.0)(&theme, status).text_color,
                thumb,
                4.5,
                "selected segment",
            );
        }
    }
}

#[test]
fn checkboxes_sliders_and_progress_have_visible_functional_marks() {
    for (theme, p) in window_appearances() {
        for checked in [false, true] {
            for status in [
                checkbox::Status::Active {
                    is_checked: checked,
                },
                checkbox::Status::Hovered {
                    is_checked: checked,
                },
            ] {
                let style = check(&theme, status);
                let fill = solid(style.background);
                if checked {
                    reads(style.icon_color, fill, 3.0, "checkbox check");
                }
                for surface in [p.base, p.sidebar, solid(well(&theme).background.unwrap())] {
                    reads(style.border.color, surface, 3.0, "checkbox outline");
                }
            }
        }
        for status in [
            slider::Status::Active,
            slider::Status::Hovered,
            slider::Status::Dragged,
        ] {
            let style = slide(&theme, status);
            let rails = [
                solid(style.rail.backgrounds.0),
                solid(style.rail.backgrounds.1),
            ];
            for surface in [p.base, p.sidebar, solid(well(&theme).background.unwrap())] {
                for rail in rails {
                    reads(rail, surface, 3.0, "slider rail");
                }
                for adjacent in rails.into_iter().chain([surface]) {
                    let fill = ratio(solid(style.handle.background), adjacent);
                    let edge = if style.handle.border_width > 0.0 {
                        ratio(style.handle.border_color, adjacent)
                    } else {
                        0.0
                    };
                    assert!(
                        fill.max(edge) >= 3.0,
                        "slider handle against {adjacent:?}: fill {fill:.2}, edge {edge:.2}"
                    );
                }
            }
        }
        let style = progress(&theme);
        for surface in [p.base, p.sidebar, solid(well(&theme).background.unwrap())] {
            reads(solid(style.bar), surface, 3.0, "progress fill");
            reads(solid(style.background), surface, 3.0, "progress track");
        }
    }
}

#[test]
fn stage_layers_read_over_extreme_image_colors_in_both_appearances() {
    for (theme, _) in window_appearances() {
        for picture in [
            Color::WHITE,
            Color::BLACK,
            rgb(0xFF0000),
            rgb(0x00FF00),
            rgb(0x0000FF),
        ] {
            let badge = backdrop(badge(&theme).background, picture);
            for solid in [false, true] {
                let glass = backdrop(glass(1.0, solid)(&theme).background, picture);
                for surface in [badge, glass] {
                    for text in [STAGE.text, STAGE.secondary, STAGE.accent_text] {
                        reads(text, surface, 4.5, "stage text on image overlay");
                    }
                    for color in [STAGE.live, STAGE.warn, STAGE.danger] {
                        reads(STAGE.ink(color), surface, 4.5, "stage semantic text/glyph");
                    }
                    for status in BUTTON_STATES {
                        for active in [false, true] {
                            let style = on_glass(active, 1.0)(&theme, status);
                            reads(
                                style.text_color,
                                backdrop(style.background, surface),
                                4.5,
                                "stage overlay button",
                            );
                            // Native preview controls supply explicit icon
                            // colors. Hovering changes the fill, while their
                            // glyphs keep these colors rather than inheriting
                            // on_glass's hovered text color.
                            for icon in [STAGE.secondary, STAGE.accent_text] {
                                reads(
                                    icon,
                                    backdrop(style.background, surface),
                                    3.0,
                                    "explicit stage button icon",
                                );
                            }
                        }
                        for selected in [false, true] {
                            let style = glass_segment(selected, 1.0)(&theme, status);
                            reads(
                                style.text_color,
                                backdrop(style.background, surface),
                                4.5,
                                "stage segment",
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn fallback_widgets_inherit_readable_theme_colors() {
    for (theme, _) in window_appearances() {
        for status in BUTTON_STATES {
            let style = button::primary(&theme, status);
            reads(
                style.text_color,
                solid(style.background.unwrap()),
                4.5,
                "default primary button",
            );
        }
        let style = menu::default(&theme);
        reads(
            style.text_color,
            solid(style.background),
            4.5,
            "default menu option",
        );
        reads(
            style.selected_text_color,
            solid(style.selected_background),
            4.5,
            "default selected menu option",
        );
    }
}
