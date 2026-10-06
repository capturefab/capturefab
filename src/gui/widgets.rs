//! Small building blocks shared by the views.
use super::*;

/// Tooltip in the workbench style.
pub(super) fn focus_address() -> Task<Message> {
    Task::batch([
        operation::focus("connect-address"),
        operation::select_all("connect-address"),
    ])
}

pub(super) fn tip<'a, M: 'a>(
    content: impl Into<Element<'a, M>>,
    tip: impl ToString,
) -> Element<'a, M> {
    tooltip(
        content,
        container(text(tip.to_string()).size(style::SMALL))
            .padding([5, 9])
            .style(style::tooltip),
        tooltip::Position::Bottom,
    )
    .gap(6)
    .delay(Duration::from_millis(500))
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

/// A section title: a small uppercase label above its group.
pub(super) fn heading<'a, M: 'a>(title: &'a str, p: &'static Palette) -> Element<'a, M> {
    container(
        text(title.to_uppercase())
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

pub(super) fn disclosure<'a>(
    title: String,
    open: bool,
    on: Message,
    p: &'static Palette,
) -> Element<'a, Message> {
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
            clipped(
                text(command.clone())
                    .size(style::SMALL)
                    .font(style::MONO)
                    .color(p.text)
            ),
            space::horizontal(),
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
    .padding([7, 16])
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
/// from 0 to 1; clicking outside sends `on_blur`.
pub(super) fn modal<'a>(
    base: Element<'a, Message>,
    content: Element<'a, Message>,
    on_blur: Message,
    shown: f32,
) -> Element<'a, Message> {
    let sheet = container(opaque(content)).padding(iced::Padding {
        top: 24.0 * (1.0 - shown),
        ..iced::Padding::ZERO
    });
    stack![
        base,
        opaque(
            mouse_area(center(sheet).style(move |theme| style::scrim(theme, shown)))
                .on_press(on_blur)
        ),
    ]
    .into()
}
