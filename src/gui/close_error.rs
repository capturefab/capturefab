//! A failed close must be visible above Settings or an existing sheet, while
//! keeping their drafts and widget state available when the user stays open.
use super::*;
use iced::widget::column;

const TITLE: &str = "Could Not Save Settings";

pub(super) struct CloseError {
    pub(super) window: window::Id,
    pub(super) target: CloseTarget,
    details: String,
}

impl Workbench {
    pub(super) fn show_close_error(
        &mut self,
        target: CloseTarget,
        details: String,
    ) -> Task<Message> {
        let id = if self.settings_window == Some(self.focused_window) {
            self.focused_window
        } else {
            self.main_window
        };
        self.close_error = Some(CloseError {
            window: id,
            target,
            details,
        });
        self.desktop_menu = Default::default();
        self.popup_open = false;
        self.accessibility.invalidate(self.main_window);
        if let Some(settings) = self.settings_window {
            self.accessibility.invalidate(settings);
        }
        window::gain_focus(id).chain(operation::focus("close-retry"))
    }

    pub(super) fn dismiss_close_error(&mut self) {
        if self.close_error.take().is_some() {
            self.accessibility.invalidate(self.main_window);
            if let Some(settings) = self.settings_window {
                self.accessibility.invalidate(settings);
            }
        }
    }

    pub(super) fn close_error_view<'a>(
        &'a self,
        content: Element<'a, Message>,
        id: window::Id,
    ) -> Element<'a, Message> {
        // Keep this wrapper and first child even with no alert, preserving the
        // focused editor and selection across the failed close and dismissal.
        let base = keyboard_history::background(content, self.close_error.is_some());
        let mut layers = stack![base];
        if let Some(error) = &self.close_error
            && error.window == id
        {
            let p = Palette::of(self.dark());
            let details = error
                .details
                .strip_prefix("Could not save settings: ")
                .unwrap_or(&error.details);
            layers = layers.push(responsive(move |size| close_alert(details, p, size)));
        }
        layers.into()
    }
}

fn close_alert<'a>(details: &'a str, p: &'static Palette, viewport: Size) -> Element<'a, Message> {
    let width = (viewport.width - 32.0).clamp(1.0, 440.0);
    let compact = width < 320.0;
    let keep = button(text("Cancel").size(style::BODY))
        .id("close-keep-open")
        .padding([7, 16])
        .style(style::secondary)
        .on_press(Message::KeepOpen);
    let retry = button(text("Retry").size(style::BODY))
        .id("close-retry")
        .padding([7, 18])
        .style(style::primary)
        .on_press(Message::RetryClose);
    let footer: Element<'_, Message> = if compact {
        column![retry.width(Fill), keep.width(Fill)]
            .spacing(10)
            .into()
    } else {
        row![space::horizontal(), keep, retry]
            .spacing(10)
            .align_y(Alignment::Center)
            .into()
    };
    let body = column![
        text("Capturefab is still open. Resolve the error, then try again.").size(style::BODY),
        accessibility::label(
            text(details).size(style::SMALL).color(p.danger).into(),
            format!("Could not save settings: {details}"),
            accessibility::Role::Alert,
        ),
    ]
    .spacing(12);
    let alert = container(
        column![
            accessibility::label(
                text(TITLE)
                    .size(style::HEADING)
                    .font(style::SEMIBOLD)
                    .into(),
                TITLE.into(),
                accessibility::Role::Heading,
            ),
            container(scrollable(body).style(style::scroll))
                .max_height((viewport.height - if compact { 240.0 } else { 200.0 }).max(1.0)),
            footer,
        ]
        .spacing(16),
    )
    .id("close-error")
    .padding(24)
    .width(width)
    .style(style::sheet);
    let alert = keyboard_history::escape_guard(accessibility::group(
        alert.into(),
        TITLE.into(),
        accessibility::Role::AlertDialog,
    ));
    opaque(center(opaque(alert)).style(|theme| style::scrim(theme, 1.0)))
}
