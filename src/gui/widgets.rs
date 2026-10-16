//! Small building blocks shared by the views.
use super::*;
use iced::widget::{Row, Stack, column};
mod keyboard_scroll;
#[cfg(test)]
mod keyboard_scroll_panel_tests;
#[cfg(test)]
mod keyboard_scroll_tests;
mod pinned_body;
mod radio_group;

pub(super) use super::oneline::one_line;

/// Inset of a sheet's content from its edges.
const SHEET_INSET: f32 = 24.0;
/// How far a sheet rises into place as it opens.
const SHEET_RISE: f32 = 12.0;
/// The scrolling body of the sheet that shows; only one shows at a time.
pub(super) const SHEET_BODY: &str = "sheet-body";
pub(super) const ACTIVITY_BODY: &str = "activity-body";

/// Activity keeps newer entries at the bottom. Its pinned action provides a
/// keyboard entry point without taking scrolling keys from the other panes.
pub(super) fn activity_scroll<'a, M: 'a>(content: Element<'a, M>) -> Element<'a, M> {
    keyboard_scroll::panel(content, ACTIVITY_BODY, scrollable::Anchor::End)
}

/// Windows places the affirmative action before cancellation in a row;
/// macOS and GNOME put cancellation first. Narrow native alert stacks put
/// the affirmative action above cancellation on all three platforms.
pub(super) fn dialog_actions<T>(cancel: T, affirmative: T, stacked: bool) -> (T, T) {
    if stacked || cfg!(target_os = "windows") {
        (affirmative, cancel)
    } else {
        (cancel, affirmative)
    }
}

pub(super) fn focus_address() -> Task<Message> {
    Task::batch([
        operation::focus("connect-address"),
        operation::select_all("connect-address"),
    ])
}

/// Tooltip in the workbench style, opening below `content`.
pub(super) fn tip<'a, M: Clone + 'static>(
    content: impl Into<Element<'a, M>>,
    tip: impl ToString,
) -> Element<'a, M> {
    tip_at(content, tip, tooltip::Position::Bottom)
}

/// Tooltip opening above `content`, for controls along the window's bottom
/// edge, where one below would land on the control itself.
pub(super) fn tip_above<'a, M: Clone + 'static>(
    content: impl Into<Element<'a, M>>,
    tip: impl ToString,
) -> Element<'a, M> {
    tip_at(content, tip, tooltip::Position::Top)
}

/// Tooltip at `position`. Long text wraps at 360 px.
pub(super) fn tip_at<'a, M: Clone + 'static>(
    content: impl Into<Element<'a, M>>,
    tip: impl ToString,
    position: tooltip::Position,
) -> Element<'a, M> {
    let tip = tip.to_string();
    tooltip(
        accessibility::named(content.into(), tip.clone()),
        container(text(tip).size(style::SMALL))
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

pub(super) fn checkbox<'a, M: Clone + 'static>(
    label: &'a str,
    checked: bool,
    on: impl Fn(bool) -> M + 'a,
) -> Element<'a, M> {
    checkbox_named(label, label, checked, on)
}

pub(super) fn checkbox_maybe<'a, M: Clone + 'static>(
    label: &'a str,
    checked: bool,
    on: Option<impl Fn(bool) -> M + 'a>,
) -> Element<'a, M> {
    checkbox_named_maybe(label, label, checked, on)
}

pub(super) fn checkbox_named<'a, M: Clone + 'static>(
    label: &'a str,
    accessible_name: &str,
    checked: bool,
    on: impl Fn(bool) -> M + 'a,
) -> Element<'a, M> {
    checkbox_named_maybe(label, accessible_name, checked, Some(on))
}

fn checkbox_named_maybe<'a, M: Clone + 'static>(
    label: &'a str,
    accessible_name: &str,
    checked: bool,
    on: Option<impl Fn(bool) -> M + 'a>,
) -> Element<'a, M> {
    let toggle = on.as_ref().map(|on| on(!checked));
    let mut check = iced::widget::checkbox(checked)
        .on_toggle_maybe(on)
        .size(16)
        .spacing(8)
        .text_size(style::BODY)
        .style(style::check);
    if !label.is_empty() {
        check = check.label(label);
    }
    button(check)
        .name(accessible_name)
        .role(accessibility::Role::CheckBox)
        .checked(checked)
        .padding(0)
        .style(style::bare)
        .on_press_maybe(toggle)
        .into()
}

/// The quality/frame-rate balance slider participates in Tab traversal and
/// changes with arrow keys. A keyboard step is committed just like a completed
/// pointer drag, rather than leaving an unsubmitted value on screen.
pub(super) fn balance_slider<'a>(value: f64) -> Element<'a, Message> {
    button(
        iced::widget::slider(0.0..=1.0, value, Message::Balance)
            .step(0.01)
            .on_release(Message::BalanceReleased)
            .style(style::slide),
    )
    .name("Auto-exposure quality and frame-rate balance")
    .role(accessibility::Role::Slider)
    .value(format!("{:.0}%", value * 100.0))
    .numeric_value(value, 0.0, 1.0, 0.01)
    .increment(Message::BalanceCommit(
        ((value + 0.01) * 100.0).round().min(100.0) / 100.0,
    ))
    .decrement(Message::BalanceCommit(
        ((value - 0.01) * 100.0).round().max(0.0) / 100.0,
    ))
    .set_value(move |input| {
        let value = input
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .unwrap_or(value);
        Message::BalanceCommit(value.clamp(0.0, 1.0))
    })
    .width(Fill)
    .padding(0)
    .style(style::bare)
    .on_press(Message::BalanceReleased)
    .on_key_press(move |key, modifiers| {
        balance_key_value(value, key, modifiers.shift()).map(|next| {
            if next == value {
                Vec::new()
            } else {
                vec![Message::Balance(next), Message::BalanceReleased]
            }
        })
    })
    .into()
}

fn balance_key_value(value: f64, key: &keyboard::Key, large_step: bool) -> Option<f64> {
    use keyboard::{Key, key::Named};
    let step = if large_step { 0.1 } else { 0.01 };
    let value = match key {
        Key::Named(Named::ArrowRight | Named::ArrowUp) => value + step,
        Key::Named(Named::ArrowLeft | Named::ArrowDown) => value - step,
        Key::Named(Named::PageUp) => value + 0.1,
        Key::Named(Named::PageDown) => value - 0.1,
        Key::Named(Named::Home) => 0.0,
        Key::Named(Named::End) => 1.0,
        _ => return None,
    };
    Some((value.clamp(0.0, 1.0) * 100.0).round() / 100.0)
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

/// A camera's status dot in `p`: filled while it is connected, a ring
/// while it is not; see `CameraState`.
pub(super) fn status_dot<'a, M: 'a>(state: CameraState, size: f32, p: &Palette) -> Element<'a, M> {
    let color = state.color(p);
    if !state.hollow() {
        return dot(color, size);
    }
    container(space().width(size).height(size))
        .style(move |_| container::Style {
            border: iced::Border {
                color,
                width: 1.5,
                radius: (size / 2.0).into(),
            },
            ..container::Style::default()
        })
        .into()
}

/// A section title: a small label above its group.
pub(super) fn heading<'a, M: 'static>(title: &'a str, p: &'static Palette) -> Element<'a, M> {
    let content = container(
        text(title)
            .size(style::CAPTION)
            .font(style::SEMIBOLD)
            .color(p.secondary),
    )
    .padding(iced::Padding {
        top: 6.0,
        ..iced::Padding::ZERO
    })
    .into();
    accessibility::label(content, title.into(), accessibility::Role::Heading)
}

/// A header that opens and closes the group under it. `turn` is how open it
/// is, from 0 (closed, chevron pointing right) to 1 (open, pointing down),
/// for the caller to animate; `summary`, if any, sums up the closed group at
/// the right of the header.
pub(super) fn disclosure<'a, M: Clone + 'static>(
    title: impl text::IntoFragment<'a>,
    summary: Option<String>,
    turn: f32,
    on: M,
    p: &'static Palette,
) -> Element<'a, M> {
    let title = title.into_fragment();
    let name = title.to_string();
    let description = summary.clone().unwrap_or_default();
    // The font's chevrons at rest, like every other; drawn only mid-turn.
    let chevron = if turn <= 0.0 || turn >= 1.0 {
        icon(
            if turn >= 1.0 {
                Icon::ChevronDown
            } else {
                Icon::ChevronRight
            },
            11.0,
            p.secondary,
        )
    } else {
        iced::widget::canvas(Chevron {
            turn,
            color: p.secondary,
        })
        .width(11)
        .height(11)
        .into()
    };
    let mut header = row![
        chevron,
        text(title)
            .size(style::BODY)
            .font(style::MEDIUM)
            .color(p.text),
    ]
    .spacing(6)
    .align_y(Alignment::Center);
    if let Some(summary) = summary {
        header = header.push(space::horizontal()).push(one_line(
            summary,
            style::CAPTION,
            style::SANS,
            p.secondary,
        ));
    }
    // Flush with the content's edge; the hover fill still has room on the right.
    button(header)
        .name(name)
        .description(description)
        .role(accessibility::Role::DisclosureTriangle)
        .expanded(turn > 0.5)
        .width(Fill)
        .padding(iced::Padding {
            top: 3.0,
            right: 6.0,
            bottom: 3.0,
            left: 0.0,
        })
        .style(style::plain)
        .on_press(on)
        .into()
}

/// A disclosure's chevron, turned from pointing right at 0 to down at 1:
/// the font's caret, drawn so it can turn.
struct Chevron {
    turn: f32,
    color: Color,
}

/// The drawn chevron, kept until its turn or color changes.
#[derive(Default)]
struct Turned {
    cache: iced::widget::canvas::Cache,
    key: Cell<Option<(u32, Color)>>,
}

impl<M> iced::widget::canvas::Program<M> for Chevron {
    type State = Turned;

    fn draw(
        &self,
        state: &Turned,
        renderer: &iced::Renderer,
        _theme: &Theme,
        bounds: iced::Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Vec<iced::widget::canvas::Geometry> {
        use iced::widget::canvas::{LineCap, LineJoin, Path, Stroke};
        let key = Some((self.turn.to_bits(), self.color));
        if state.key.replace(key) != key {
            state.cache.clear();
        }
        let geometry = state.cache.draw(renderer, bounds.size(), |frame| {
            // Phosphor's caret: 5/16 of the size wide and 5/8 tall about the
            // center, stroked at 1/16.
            let side = bounds.width.min(bounds.height);
            let (half_w, half_h) = (side * 5.0 / 32.0, side * 5.0 / 16.0);
            let (sin, cos) = (self.turn * std::f32::consts::FRAC_PI_2).sin_cos();
            let center = frame.center();
            let at = |x: f32, y: f32| {
                iced::Point::new(center.x + x * cos - y * sin, center.y + x * sin + y * cos)
            };
            let caret = Path::new(|path| {
                path.move_to(at(-half_w, -half_h));
                path.line_to(at(half_w, 0.0));
                path.line_to(at(-half_w, half_h));
            });
            frame.stroke(
                &caret,
                Stroke::default()
                    .with_color(self.color)
                    .with_width(side / 16.0)
                    .with_line_cap(LineCap::Round)
                    .with_line_join(LineJoin::Round),
            );
        });
        vec![geometry]
    }
}

/// An error caption under the field it concerns. Pair it with
/// `style::input_invalid` on the field.
pub(super) fn field_error<'a, M: 'static>(
    message: impl text::IntoFragment<'a>,
    p: &'static Palette,
) -> Element<'a, M> {
    let message = message.into_fragment();
    let name = message.to_string();
    let content = row![
        // Centered on the first line, should the message wrap.
        container(Level::Error.mark(11.0, p))
            .height(Length::Fixed(style::line_height(style::CAPTION)))
            .align_y(Alignment::Center),
        text(message)
            .size(style::CAPTION)
            .color(Level::Error.color(p)),
    ]
    .spacing(5)
    .into();
    accessibility::label(content, name, accessibility::Role::Alert)
}

/// A command in monospace with a button that copies it. `copied` (from
/// `Workbench::just_copied`) confirms a copy in place for a moment.
pub(super) fn code_block<'a>(
    command: String,
    copied: bool,
    p: &'static Palette,
) -> Element<'a, Message> {
    let (glyph, color, hint) = if copied {
        (Icon::Check, p.ink(p.live), "Copied")
    } else {
        (Icon::Copy, p.secondary, "Copy")
    };
    container(
        row![
            // Filling the room the button leaves and wrapping in it, so a
            // long command stays readable without pushing the button out.
            text(command.clone())
                .size(style::SMALL)
                .font(style::MONO)
                .color(p.text)
                .width(Fill)
                .wrapping(text::Wrapping::WordOrGlyph),
            tip(
                button(icon(glyph, 13.0, color))
                    .padding(4)
                    .style(style::plain)
                    .on_press(Message::Copy(command)),
                hint,
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

/// A segmented control of equal segments, each a label, its message (`None`
/// disables it) and a hint (empty for none), on a panel. One thumb marks
/// the selection at `thumb`, a segment index the caller animates: whole
/// numbers at rest, fractions while sliding. Labels keep one weight, so
/// nothing reflows, and brighten as the thumb passes under them.
pub(super) fn segmented<'a, M: Clone + 'static>(
    name: &'a str,
    items: Vec<(&'a str, Option<M>, String)>,
    selected: usize,
    thumb: f32,
) -> Element<'a, M> {
    segments(name, items, selected, thumb, false)
}

/// Inspector panes are tabs. Selection describes the current pane immediately;
/// the thumb remains a visual animation, independent of native semantics.
pub(super) fn inspector_tabs<'a, M: Clone + 'static>(
    items: Vec<(&'a str, Option<M>, String)>,
    selected: usize,
    thumb: f32,
) -> Element<'a, M> {
    panel_tabs("Inspector panels", items, selected, thumb)
}

pub(super) fn panel_tabs<'a, M: Clone + 'static>(
    name: &'a str,
    items: Vec<(&'a str, Option<M>, String)>,
    selected: usize,
    thumb: f32,
) -> Element<'a, M> {
    segments(name, items, selected, thumb, true)
}

fn segments<'a, M: Clone + 'static>(
    name: &'a str,
    items: Vec<(&'a str, Option<M>, String)>,
    selected: usize,
    thumb: f32,
    tabs: bool,
) -> Element<'a, M> {
    let choices = items
        .iter()
        .filter_map(|(_, message, _)| message.clone())
        .collect();
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
        .name(label)
        .width(Fill)
        .padding([4, 12])
        .style(style::segment_label(lit))
        .on_press_maybe(on);
        let segment = if tabs {
            let prefix = if name == "Inspector panels" {
                "inspector"
            } else {
                name
            };
            segment
                .id(format!("{prefix}-tab-{i}"))
                .role(accessibility::Role::Tab)
                .selected(i == selected)
        } else {
            segment
                .role(accessibility::Role::RadioButton)
                .checked(i == selected)
        };
        if hint.is_empty() {
            segment.into()
        } else {
            tip(segment, hint)
        }
    }));
    let group = container(Stack::new().push(labels).push_under(slide))
        .padding(2)
        .style(style::segment_track)
        .into();
    radio_group::wrap(group, choices, tabs, name)
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

/// Start or Stop for a stream, sending `on`. Start is the primary action;
/// Stop is routine, so its glyph and label use the neutral text color.
/// While `busy`, a spinner at step `spin` (from `Workbench::spin`) takes the
/// glyph's place and presses are ignored, with the button keeping
/// its look.
pub(super) fn stream_button<'a>(
    streaming: bool,
    label: &'a str,
    on: Option<Message>,
    busy: bool,
    spin: usize,
    p: &'static Palette,
) -> button::Button<'a, Message> {
    let glyph = match (busy, streaming) {
        (true, true) => icon::spinner(12.0, p.secondary, spin),
        (true, false) => icon::spinner(12.0, Color::WHITE, spin),
        (false, true) => icon(Icon::Stop, 12.0, p.text),
        (false, false) => icon(Icon::Play, 12.0, Color::WHITE),
    };
    let base = if streaming {
        style::stop
    } else {
        style::primary
    };
    button(
        row![glyph, text(label).size(style::BODY).font(style::MEDIUM)]
            .spacing(7)
            .align_y(Alignment::Center),
    )
    .name(label)
    .padding([5, 14])
    // Ignoring presses would otherwise read as disabled.
    .style(move |theme, status| base(theme, if busy { button::Status::Active } else { status }))
    .on_press_maybe(on.filter(|_| !busy))
}

pub(super) fn last_frame<'a, M: 'a>() -> Element<'a, M> {
    container(text("Last frame").size(style::SMALL))
        .padding([2, 8])
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
    // Centering splits the padding above and below, so the sheet travels
    // half of it.
    let sheet = container(opaque(content))
        .id("sheet")
        .padding(iced::Padding {
            top: motion::rise(2.0 * SHEET_RISE, shown),
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

/// The frame every sheet shares, built by `sheet_frame`: a pinned header,
/// the body scrolling with a visible scroller once it overflows, and an
/// optional pinned footer for actions under a hairline. The whole sheet
/// stays within its height, so derive that from the window's. Goes inside
/// `modal`, which keeps the focus scope.
pub(super) struct SheetFrame<'a, M> {
    title: text::Fragment<'a>,
    mark: Option<Icon>,
    back: Option<M>,
    back_enabled: bool,
    close: M,
    close_enabled: bool,
    body: Element<'a, M>,
    controls: Option<Element<'a, M>>,
    footer: Option<Element<'a, M>>,
    default_action: Option<M>,
    width: f32,
    max_height: f32,
    scrolled: bool,
    on_scroll: Option<Box<dyn Fn(bool) -> M + 'a>>,
    p: &'static Palette,
}

/// A sheet titled `title` around `body`, whose close button sends `close`;
/// 560 × 640 unless sized.
pub(super) fn sheet_frame<'a, M: Clone + 'static>(
    title: impl text::IntoFragment<'a>,
    close: M,
    body: impl Into<Element<'a, M>>,
    p: &'static Palette,
) -> SheetFrame<'a, M> {
    SheetFrame {
        title: title.into_fragment(),
        mark: None,
        back: None,
        back_enabled: true,
        close,
        close_enabled: true,
        body: body.into(),
        controls: None,
        footer: None,
        default_action: None,
        width: 560.0,
        max_height: 640.0,
        scrolled: false,
        on_scroll: None,
        p,
    }
}

impl<'a, M: Clone + 'a> SheetFrame<'a, M> {
    /// An accent glyph before the title.
    pub(super) fn mark(mut self, mark: Icon) -> Self {
        self.mark = Some(mark);
        self
    }

    /// A back button before the title sending `back`, for a page within
    /// the sheet, such as an editor; `None` leaves it out.
    pub(super) fn back(mut self, back: Option<M>) -> Self {
        self.back = back;
        self
    }

    /// Keep the back control visible while an operation temporarily prevents
    /// leaving the current page.
    pub(super) fn back_enabled(mut self, enabled: bool) -> Self {
        self.back_enabled = enabled;
        self
    }

    /// Keep the close control visible while an operation temporarily prevents
    /// dismissing the sheet.
    pub(super) fn close_enabled(mut self, enabled: bool) -> Self {
        self.close_enabled = enabled;
        self
    }

    /// Actions pinned under the body.
    pub(super) fn footer(mut self, footer: impl Into<Element<'a, M>>) -> Self {
        self.footer = Some(footer.into());
        self
    }

    /// Navigation pinned below the title while the body scrolls.
    pub(super) fn controls(mut self, controls: impl Into<Element<'a, M>>) -> Self {
        self.controls = Some(controls.into());
        self
    }

    /// Return activates this enabled primary action, with the platform's
    /// distinction between the default button and keyboard focus.
    pub(super) fn default_action(mut self, action: Option<M>) -> Self {
        self.default_action = action;
        self
    }

    pub(super) fn size(mut self, width: f32, max_height: f32) -> Self {
        self.width = width;
        self.max_height = max_height;
        self
    }

    /// Whether the body has scrolled under the header, which then shows a
    /// hairline; pair it with `on_scroll`.
    pub(super) fn scrolled(mut self, scrolled: bool) -> Self {
        self.scrolled = scrolled;
        self
    }

    /// Report whether the body is scrolled away from its top: once as the
    /// sheet shows, then only as its top edge leaves or returns.
    pub(super) fn on_scroll(mut self, on_scroll: impl Fn(bool) -> M + 'a) -> Self {
        self.on_scroll = Some(Box::new(on_scroll));
        self
    }
}

impl<'a, M: Clone + 'static> From<SheetFrame<'a, M>> for Element<'a, M> {
    fn from(frame: SheetFrame<'a, M>) -> Self {
        let SheetFrame {
            title,
            mark,
            back,
            back_enabled,
            close,
            close_enabled,
            body,
            controls,
            footer,
            default_action,
            width,
            max_height,
            scrolled,
            on_scroll,
            p,
        } = frame;
        let close_name = format!("Close {title}");
        let title_name = title.to_string();
        let mut head = row![].spacing(10).align_y(Alignment::Center);
        if let Some(back) = back {
            head = head.push(tip(
                button(icon(Icon::ChevronLeft, 14.0, p.secondary))
                    .name("Back")
                    .padding(6)
                    .style(style::plain)
                    .on_press_maybe(back_enabled.then_some(back)),
                "Back",
            ));
        }
        if let Some(mark) = mark {
            head = head.push(icon(mark, 24.0, p.accent));
        }
        let head = head
            .push(accessibility::label(
                one_line(title, style::DISPLAY, style::BOLD, p.text)
                    .width(Fill)
                    .into(),
                title_name.clone(),
                accessibility::Role::Heading,
            ))
            .push(tip(
                button(icon(Icon::Close, 14.0, p.secondary))
                    .name(close_name)
                    .id("sheet-close")
                    .padding(6)
                    .style(style::plain)
                    .on_press_maybe(close_enabled.then_some(close)),
                Action::Overview.hint("Close", Os::CURRENT),
            ));
        // A marker of no size at the body's top edge tells when it scrolls
        // under the header: only on the change, not on every scroll step.
        // Half a pixel of slack absorbs rounding at the top.
        let mut content = column![];
        if let Some(on_scroll) = on_scroll {
            let under = on_scroll(true);
            content = content.push(
                iced::widget::sensor(space())
                    .anticipate(0.5)
                    .on_hide(under)
                    .on_show(move |_| on_scroll(false)),
            );
        }
        let body = scrollable(
            container(content.push(body))
                .width(Fill)
                .padding(iced::Padding {
                    top: 0.0,
                    right: SHEET_INSET,
                    bottom: SHEET_INSET,
                    left: SHEET_INSET,
                }),
        )
        .id(SHEET_BODY)
        .width(Fill)
        .style(style::sheet_scroll);
        // A hairline under the header once the body scrolls beneath it; the
        // same height either way, so nothing moves.
        let edge: Element<'a, M> = if scrolled {
            rule::horizontal(1).style(style::line).into()
        } else {
            space().height(1).into()
        };
        let mut header = column![container(head).padding(iced::Padding {
            top: SHEET_INSET,
            right: SHEET_INSET,
            bottom: 13.0,
            left: SHEET_INSET,
        }),];
        if let Some(controls) = controls {
            header = header.push(container(controls).width(Fill).padding(iced::Padding {
                top: 0.0,
                right: SHEET_INSET,
                bottom: 14.0,
                left: SHEET_INSET,
            }));
        }
        let mut sheet = column![
            header,
            edge,
            // Measure the actual header/footer first. The body uses only
            // what remains, while still shrinking for a short form.
            pinned_body::wrap(body.into()),
        ];
        if let Some(footer) = footer {
            sheet = sheet
                .push(rule::horizontal(1).style(style::line))
                .push(container(footer).width(Fill).padding([12.0, SHEET_INSET]));
        }
        accessibility::modal_group(
            keyboard_history::default_action(
                keyboard_scroll::wrap(
                    container(pinned_body::column(sheet.into()))
                        .width(width)
                        .max_height(max_height.max(0.0))
                        .style(style::sheet)
                        .into(),
                    SHEET_BODY,
                ),
                default_action,
            ),
            title_name,
            accessibility::Role::Dialog,
        )
    }
}

/// `history` as a sparkline of `width` × `height`, explained by `about` on
/// hover; blank space of the same size until there is a line to draw.
pub(super) fn spark<'a, M: Clone + 'static>(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_sheet_is_a_named_modal_dialog_with_retained_native_close() {
        use iced::advanced::{Layout, layout, widget::Tree};
        use iced::futures::StreamExt;
        let renderer = super::super::destinations::layout_tests::renderer();
        let mut tree = Tree::empty();
        let mut previous = None;
        for content in ["Loading report…", "Report available."] {
            let mut view: Element<'_, Message> = sheet_frame(
                "Session analytics",
                Message::Analytics(false),
                text(content),
                Palette::of(false),
            )
            .mark(Icon::Activity)
            .size(500.0, 420.0)
            .into();
            tree.diff(view.as_widget());
            let layout = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(500.0, 420.0)),
            );
            let (snapshot, bridge, window) = accessibility::snapshot_widget(
                view.as_widget_mut(),
                &mut tree,
                Layout::new(&layout),
                &renderer,
            );
            let named = |name, role| {
                snapshot
                    .nodes
                    .iter()
                    .find(|(_, node)| node.label() == Some(name) && node.role() == role)
                    .unwrap()
            };
            let (dialog_id, dialog) = named("Session analytics", accessibility::Role::Dialog);
            let (heading_id, _) = named("Session analytics", accessibility::Role::Heading);
            let (close_id, close) = named("Close Session analytics", accessibility::Role::Button);
            let (content_id, _) = named(content, accessibility::Role::Label);
            assert!(dialog.is_modal());
            assert!(!dialog.supports_action(accesskit::Action::Focus));
            assert!(!dialog.supports_action(accesskit::Action::Click));
            for child in [heading_id, close_id, content_id] {
                assert!(dialog.children().contains(child));
                assert_eq!(
                    snapshot
                        .nodes
                        .iter()
                        .filter(|(_, node)| node.children().contains(child))
                        .count(),
                    1,
                    "each visible control belongs to the dialog once"
                );
            }
            let root = &snapshot.nodes.iter().find(|(id, _)| id.0 == 0).unwrap().1;
            assert_eq!(root.children(), [*dialog_id]);
            if let Some(previous) = previous {
                assert_eq!((*dialog_id, *close_id), previous);
            }
            previous = Some((*dialog_id, *close_id));
            assert!(close.supports_action(accesskit::Action::Click));
            let task = bridge.dispatch(accessibility::Request {
                window,
                replay: None,
                action: accesskit::ActionRequest {
                    action: accesskit::Action::Click,
                    target: *close_id,
                    data: None,
                },
            });
            let mut actions = iced_runtime::task::into_stream(task).unwrap();
            assert!(matches!(
                iced::futures::executor::block_on(actions.next()),
                Some(iced_runtime::Action::Output(Message::Analytics(false)))
            ));
            assert!(iced::futures::executor::block_on(actions.next()).is_none());
        }
    }

    #[test]
    fn slider_keys_step_snap_and_reach_the_bounds() {
        use keyboard::{Key, key::Named};
        assert_eq!(
            balance_key_value(0.5, &Key::Named(Named::ArrowRight), false),
            Some(0.51)
        );
        assert_eq!(
            balance_key_value(0.5, &Key::Named(Named::ArrowLeft), true),
            Some(0.4)
        );
        assert_eq!(
            balance_key_value(0.95, &Key::Named(Named::PageUp), false),
            Some(1.0)
        );
        assert_eq!(
            balance_key_value(0.05, &Key::Named(Named::PageDown), false),
            Some(0.0)
        );
        assert_eq!(
            balance_key_value(0.5, &Key::Named(Named::Home), false),
            Some(0.0)
        );
        assert_eq!(
            balance_key_value(0.5, &Key::Named(Named::End), false),
            Some(1.0)
        );
        assert_eq!(
            balance_key_value(1.0, &Key::Named(Named::ArrowUp), false),
            Some(1.0)
        );
        assert_eq!(
            balance_key_value(0.0, &Key::Named(Named::ArrowDown), false),
            Some(0.0)
        );
        assert_eq!(
            balance_key_value(0.5, &Key::Named(Named::Space), false),
            None
        );
    }
}
