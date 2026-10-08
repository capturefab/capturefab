//! Small building blocks shared by the views.
use super::*;
use iced::widget::{Row, Stack, column};

pub(super) use super::oneline::one_line;

/// Room a sheet's pinned header takes: padding around the title row.
const SHEET_HEADER: f32 = 24.0 + 29.0 + 14.0;
/// Room a sheet's pinned footer takes: hairline, padding and a button row.
const SHEET_FOOTER: f32 = 1.0 + 12.0 + 32.0 + 12.0;
/// Inset of a sheet's content from its edges.
const SHEET_INSET: f32 = 24.0;

pub(super) fn focus_address() -> Task<Message> {
    Task::batch([
        operation::focus("connect-address"),
        operation::select_all("connect-address"),
    ])
}

/// Tooltip in the workbench style, opening below `content`.
pub(super) fn tip<'a, M: 'a>(
    content: impl Into<Element<'a, M>>,
    tip: impl ToString,
) -> Element<'a, M> {
    tip_at(content, tip, tooltip::Position::Bottom)
}

/// Tooltip opening above `content`, for controls along the window's bottom
/// edge, where one below would land on the control itself.
#[allow(dead_code)] // adopted by the area packages
pub(super) fn tip_above<'a, M: 'a>(
    content: impl Into<Element<'a, M>>,
    tip: impl ToString,
) -> Element<'a, M> {
    tip_at(content, tip, tooltip::Position::Top)
}

/// Tooltip at `position`. Long text wraps at 360 px.
pub(super) fn tip_at<'a, M: 'a>(
    content: impl Into<Element<'a, M>>,
    tip: impl ToString,
    position: tooltip::Position,
) -> Element<'a, M> {
    tooltip(
        content,
        container(text(tip.to_string()).size(style::SMALL))
            .max_width(360)
            .padding([5, 9])
            .style(style::tooltip),
        position,
    )
    .gap(6)
    .delay(Duration::from_millis(500))
    .into()
}

/// `content` under a fill of `surface` at `1 - t`, so it seems to fade in as
/// `t` goes from 0 to 1. Only for content on an opaque `surface` of exactly
/// that color; `radius` matches its corners. The fill takes no input, and
/// `content` keeps its widget state (focus, scrolling) through the fade.
#[allow(dead_code)] // adopted by the area packages
pub(super) fn veil<'a, M: 'a>(
    content: impl Into<Element<'a, M>>,
    surface: Color,
    radius: f32,
    t: f32,
) -> Element<'a, M> {
    let veiled = Stack::new().push(content);
    if t >= 0.999 {
        return veiled.into();
    }
    veiled
        .push(
            container(space().width(Fill).height(Fill)).style(move |_| container::Style {
                background: Some(fade(surface, 1.0 - t.max(0.0)).into()),
                border: iced::border::rounded(radius),
                ..container::Style::default()
            }),
        )
        .into()
}

pub(super) fn checkbox<'a, M: 'a>(
    label: &'a str,
    checked: bool,
    on: impl Fn(bool) -> M + 'a,
) -> Element<'a, M> {
    iced::widget::checkbox(checked)
        .label(label)
        .on_toggle(on)
        .size(16)
        .spacing(8)
        .text_size(style::BODY)
        .style(style::check)
        .into()
}

/// Single-line text that is cut off at its container's edge.
pub(super) fn clipped<'a, M: 'a>(content: impl Into<Element<'a, M>>) -> Element<'a, M> {
    container(content).clip(true).into()
}

pub(super) fn fade(color: Color, amount: f32) -> Color {
    Color {
        a: color.a * amount,
        ..color
    }
}

pub(super) fn dot<'a, M: 'a>(color: Color, size: f32) -> Element<'a, M> {
    container(space().width(size).height(size))
        .style(move |_| container::Style {
            background: Some(color.into()),
            border: iced::border::rounded(size / 2.0),
            ..container::Style::default()
        })
        .into()
}

/// A section title: a small label above its group.
pub(super) fn heading<'a, M: 'a>(title: &'a str, p: &'static Palette) -> Element<'a, M> {
    container(
        text(title)
            .size(style::CAPTION)
            .font(style::SEMIBOLD)
            .color(p.secondary),
    )
    .padding(iced::Padding {
        top: 6.0,
        ..iced::Padding::ZERO
    })
    .into()
}

pub(super) fn disclosure<'a, M: Clone + 'a>(
    title: String,
    open: bool,
    on: M,
    p: &'static Palette,
) -> Element<'a, M> {
    button(
        row![
            icon(
                if open {
                    Icon::ChevronDown
                } else {
                    Icon::ChevronRight
                },
                11.0,
                p.secondary,
            ),
            text(title)
                .size(style::BODY)
                .font(style::MEDIUM)
                .color(p.text),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .padding([3, 4])
    .style(style::plain)
    .on_press(on)
    .into()
}

/// A label on the left and its control on the right.
pub(super) fn field<'a>(
    label: &'a str,
    control: Element<'a, Message>,
    p: &'static Palette,
) -> Element<'a, Message> {
    row![
        text(label).size(style::BODY).color(p.secondary).width(Fill),
        control,
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .into()
}

/// An error caption under the field it concerns. Pair it with
/// `style::input_invalid` on the field.
#[allow(dead_code)] // adopted by the area packages
pub(super) fn field_error<'a, M: 'a>(
    message: impl text::IntoFragment<'a>,
    p: &'static Palette,
) -> Element<'a, M> {
    let ink = p.ink(p.danger);
    row![
        // Centered on the first line, should the message wrap.
        container(icon(Icon::Warning, 11.0, ink))
            .height(Length::Fixed(style::CAPTION * 1.3))
            .align_y(Alignment::Center),
        text(message).size(style::CAPTION).color(ink),
    ]
    .spacing(5)
    .into()
}

pub(super) fn unit<'a>(
    control: Element<'a, Message>,
    unit: &'a str,
    p: &'static Palette,
) -> Element<'a, Message> {
    row![
        control,
        text(unit)
            .size(style::SMALL)
            .color(p.secondary)
            .width(Length::Fixed(28.0)),
    ]
    .spacing(6)
    .align_y(Alignment::Center)
    .into()
}

pub(super) fn code_block<'a>(command: String, p: &'static Palette) -> Element<'a, Message> {
    container(
        row![
            // Filling the room left by the button, so a long command ends in
            // "…" rather than pushing the button out.
            one_line(command.clone(), style::SMALL, style::MONO, p.text).width(Fill),
            tip(
                button(icon(Icon::Copy, 13.0, p.secondary))
                    .padding(4)
                    .style(style::plain)
                    .on_press(Message::Copy(command)),
                "Copy",
            ),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .padding(iced::Padding {
        top: 4.0,
        right: 4.0,
        bottom: 4.0,
        left: 10.0,
    })
    .width(Fill)
    .style(style::code)
    .into()
}

pub(super) fn segment<'a>(
    label: &'a str,
    selected: bool,
    on: impl Into<Option<Message>>,
) -> Element<'a, Message> {
    button(
        text(label)
            .size(style::SMALL)
            .font(if selected {
                style::SEMIBOLD
            } else {
                style::SANS
            })
            .width(Fill)
            .align_x(Alignment::Center),
    )
    .width(Fill)
    .padding([4, 12])
    .style(style::segment(selected))
    .on_press_maybe(on.into())
    .into()
}

/// A segmented control of equal segments, each a label, its message (`None`
/// disables it) and a hint (empty for none). One thumb marks the selection at
/// `thumb`, a segment index the caller animates: whole numbers at rest,
/// fractions while sliding. Labels keep one weight, so nothing reflows, and
/// brighten as the thumb passes under them.
#[allow(dead_code)] // adopted by the area packages
pub(super) fn segmented<'a, M: Clone + 'a>(
    items: Vec<(&'a str, Option<M>, String)>,
    thumb: f32,
) -> Element<'a, M> {
    let last = items.len().saturating_sub(1) as f32;
    let thumb = thumb.clamp(0.0, last);
    // Spacers on either side place the thumb; a zero portion would not be
    // fluid at all, so it is left out.
    let before = (thumb * 1000.0).round() as u16;
    let after = (last * 1000.0).round() as u16 - before;
    let mut slide = row![];
    if before > 0 {
        slide = slide.push(space().width(Length::FillPortion(before)));
    }
    slide = slide.push(
        container(space())
            .width(Length::FillPortion(1000))
            .height(Fill)
            .style(style::segment_thumb),
    );
    if after > 0 {
        slide = slide.push(space().width(Length::FillPortion(after)));
    }
    let labels = Row::with_children(items.into_iter().enumerate().map(|(i, (label, on, hint))| {
        let lit = (1.0 - (thumb - i as f32).abs()).max(0.0);
        let segment = button(
            text(label)
                .size(style::SMALL)
                .font(style::MEDIUM)
                .width(Fill)
                .align_x(Alignment::Center),
        )
        .width(Fill)
        .padding([4, 12])
        .style(style::segment_label(lit))
        .on_press_maybe(on);
        if hint.is_empty() {
            segment.into()
        } else {
            tip(segment, hint)
        }
    }));
    container(Stack::new().push(labels).push_under(slide))
        .padding(2)
        .style(style::segment_track)
        .into()
}

pub(super) fn icon_button<'a>(
    kind: Icon,
    size: f32,
    color: Color,
    on: Message,
) -> Element<'a, Message> {
    button(icon(kind, size, color))
        .padding(6)
        .style(style::plain)
        .on_press(on)
        .into()
}

pub(super) fn stream_button<'a>(
    streaming: bool,
    label: &'a str,
    on: Option<Message>,
    p: &'static Palette,
) -> button::Button<'a, Message> {
    let kind = if streaming { Icon::Stop } else { Icon::Play };
    button(
        row![
            icon(kind, 12.0, if streaming { p.danger } else { Color::WHITE }),
            text(label).size(style::BODY).font(style::MEDIUM)
        ]
        .spacing(7)
        .align_y(Alignment::Center),
    )
    .padding([5, 14])
    .style(if streaming {
        style::danger
    } else {
        style::primary
    })
    .on_press_maybe(on)
}

pub(super) fn last_frame<'a, M: 'a>() -> Element<'a, M> {
    container(text("Last frame").size(style::CAPTION).font(style::MEDIUM))
        .padding([3, 8])
        .style(style::badge)
        .into()
}

/// `content` centered over a dimmed copy of `base`, rising in as `shown` goes
/// from 0 to 1; clicking outside sends `on_blur`, if any.
pub(super) fn modal<'a>(
    base: Element<'a, Message>,
    content: Element<'a, Message>,
    on_blur: Option<Message>,
    shown: f32,
) -> Element<'a, Message> {
    let sheet = container(opaque(content))
        .id("sheet")
        .padding(iced::Padding {
            top: motion::rise(24.0, shown),
            ..iced::Padding::ZERO
        });
    let scrim = mouse_area(center(sheet).style(move |theme| style::scrim(theme, shown)));
    stack![
        base,
        opaque(match on_blur {
            Some(message) => scrim.on_press(message),
            None => scrim,
        }),
    ]
    .into()
}

/// The frame every sheet shares: a pinned header with `mark`, `title` and a
/// close button sending `close`, then `body`, scrolling with a visible
/// scroller once it overflows, then an optional pinned `footer` for actions
/// under a hairline. The whole sheet stays within `max_height`, so derive
/// that from the window height. Goes inside `modal`, which keeps the focus
/// scope.
#[allow(dead_code, clippy::too_many_arguments)] // adopted by the area packages
pub(super) fn sheet_frame<'a, M: Clone + 'a>(
    title: &'a str,
    mark: Option<Icon>,
    close: M,
    body: Element<'a, M>,
    footer: Option<Element<'a, M>>,
    width: f32,
    max_height: f32,
    p: &'static Palette,
) -> Element<'a, M> {
    let mut head = row![].spacing(10).align_y(Alignment::Center);
    if let Some(mark) = mark {
        head = head.push(icon(mark, 24.0, p.accent));
    }
    let head = head
        .push(text(title).size(style::DISPLAY).font(style::BOLD))
        .push(space::horizontal())
        .push(tip(
            button(icon(Icon::Close, 14.0, p.secondary))
                .padding(6)
                .style(style::plain)
                .on_press(close),
            Action::Overview.hint("Close", Os::CURRENT),
        ));
    let room = max_height - SHEET_HEADER - if footer.is_some() { SHEET_FOOTER } else { 0.0 };
    let body = scrollable(container(body).width(Fill).padding(iced::Padding {
        top: 0.0,
        right: SHEET_INSET,
        bottom: SHEET_INSET,
        left: SHEET_INSET,
    }))
    .width(Fill)
    .style(style::sheet_scroll);
    let mut sheet = column![
        container(head).padding(iced::Padding {
            top: SHEET_INSET,
            right: SHEET_INSET,
            bottom: 14.0,
            left: SHEET_INSET,
        }),
        // Capped explicitly: a shrinking scrollable would otherwise take the
        // footer's room too.
        container(body).max_height(room.max(120.0)),
    ];
    if let Some(footer) = footer {
        sheet = sheet
            .push(rule::horizontal(1).style(style::line))
            .push(container(footer).width(Fill).padding([12.0, SHEET_INSET]));
    }
    container(sheet).width(width).style(style::sheet).into()
}

/// `history` as a sparkline of `width` × `height`, explained by `about` on
/// hover; blank space of the same size until there is a line to draw.
pub(super) fn spark<'a, M: 'a>(
    history: Option<&'a sparkline::History>,
    color: Color,
    flag: Color,
    size: (f32, f32),
    about: impl Fn(&sparkline::History) -> String,
) -> Element<'a, M> {
    let (width, height) = size;
    match history.filter(|history| history.ready()) {
        Some(history) => tip(
            iced::widget::canvas(sparkline::Sparkline {
                history,
                color,
                flag,
                floor: 1.0,
            })
            .width(width)
            .height(height),
            about(history),
        ),
        None => space().width(width).height(height).into(),
    }
}
