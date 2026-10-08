//! The title bar over the main area: pane toggles, what is shown, its live
//! status and its actions.
use super::*;

/// A rounded status label: a dot, a word and an optional live figure.
pub(super) fn status_pill<'a>(
    label: &'a str,
    figure: Option<String>,
    chart: Option<Element<'a, Message>>,
    color: Color,
    pulse: f32,
) -> Element<'a, Message> {
    let mut content = row![
        dot(fade(color, pulse), 6.0),
        text(label).size(style::SMALL).font(style::MEDIUM),
    ]
    .spacing(6)
    .align_y(Alignment::Center);
    if let Some(chart) = chart {
        content = content.push(chart);
    }
    if let Some(figure) = figure {
        // A fixed slot, so a changing figure never shifts what follows.
        content = content.push(text(figure).size(style::SMALL).width(58));
    }
    container(content)
        .padding([3, 10])
        .style(style::pill(color))
        .into()
}

/// A camera's recent frame rate, with lost frames marked.
pub(super) fn fps_spark<'a>(
    history: Option<&'a sparkline::History>,
    color: Color,
    flag: Color,
    size: (f32, f32),
) -> Element<'a, Message> {
    spark(history, color, flag, size, |history| {
        trend(
            history,
            "Frame rate",
            |fps| format!("{fps:.1} fps"),
            "lost frames",
        )
    })
}

impl Workbench {
    /// The row above the main area: pane toggles, what is shown, its live
    /// status and its actions. It folds away in image mode.
    pub(super) fn title_bar<'a>(
        &self,
        title: String,
        status: Option<Element<'a, Message>>,
        actions: Element<'a, Message>,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let os = Os::CURRENT;
        let sidebar = self.sidebar_slide.lerp(0.0, SIDEBAR, self.now);
        let mut left = row![tip(
            icon_button(
                Icon::Sidebar,
                16.0,
                if self.sidebar_open {
                    p.text
                } else {
                    p.secondary
                },
                Message::ToggleSidebar,
            ),
            Action::ToggleSidebar.hint("Camera list", os),
        )]
        .spacing(10)
        .align_y(Alignment::Center);
        if !title.is_empty() {
            left = left.push(clipped(
                text(title)
                    .size(style::TITLE)
                    .font(style::BOLD)
                    .wrapping(text::Wrapping::None),
            ));
        }
        if let Some(status) = status {
            left = left.push(status);
        }
        let bar = row![
            container(left).width(Fill).clip(true),
            actions,
            tip(
                icon_button(Icon::CornersOut, 16.0, p.secondary, Message::ToggleImage),
                Action::ImageMode.hint("Image only", os),
            ),
            tip(
                icon_button(
                    Icon::Sliders,
                    16.0,
                    if self.inspector_shown() {
                        p.text
                    } else {
                        p.secondary
                    },
                    Message::ToggleInspector,
                ),
                Action::ToggleInspector.hint("Camera settings", os),
            ),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let bar = container(bar)
            .height(BAR)
            .width(Fill)
            .align_y(Alignment::Center)
            .padding(iced::Padding {
                top: 0.0,
                right: 10.0,
                bottom: 0.0,
                left: 10.0 + (self.lights() - sidebar).max(0.0),
            });
        let bar: Element<'a, Message> = if cfg!(target_os = "macos") {
            mouse_area(bar)
                .on_press(Message::DragWindow)
                .on_double_click(Message::ZoomWindow)
                .into()
        } else {
            bar.into()
        };
        container(bar)
            .height(self.chrome.lerp(0.0, BAR, self.now))
            .clip(true)
            .into()
    }
}
