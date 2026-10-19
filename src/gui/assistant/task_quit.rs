//! Shutdown is a human decision; the task journal closes before the desktop
//! starts ignoring worker messages.
use super::*;
use iced::widget::column;

impl Workbench {
    pub(in crate::gui) fn guard_assistant_quit(&mut self, target: CloseTarget) -> Task<Message> {
        self.assistant.quit = Some(target);
        self.accessibility.invalidate(self.main_window);
        self.desktop_menu = Default::default();
        self.popup_open = false;
        window::gain_focus(self.main_window).chain(operation::focus("assistant-keep-running"))
    }

    pub(in crate::gui) fn confirm_assistant_quit(&mut self) -> Task<Message> {
        let Some(target) = self.assistant.quit.take() else {
            return Task::none();
        };
        // Do this synchronously. close() sets the closing message filter.
        self.assistant.tasks.finish_app_quit();
        self.close(target)
    }

    pub(in crate::gui) fn assistant_quit_view<'a>(
        &'a self,
        content: Element<'a, Message>,
        id: window::Id,
    ) -> Element<'a, Message> {
        let waiting = self.assistant.quit.is_some();
        let base = keyboard_history::background(content, waiting);
        let mut layers = stack![base];
        if waiting && id == self.main_window {
            let p = self.window_palette(id);
            layers = layers.push(responsive(move |size| alert(p, size)));
        }
        layers.into()
    }
}

fn alert<'a>(_p: &'static Palette, viewport: Size) -> Element<'a, Message> {
    let width = (viewport.width - 32.0).clamp(1.0, 460.0);
    let compact = width < 350.0;
    let keep = button(text("Keep running").size(style::BODY))
        .id("assistant-keep-running")
        .padding([7, 14])
        .style(style::secondary)
        .on_press(Message::Assistant(Input::Task(tasks::Input::KeepRunning)));
    let stop = button(text("Stop and quit").size(style::BODY))
        .id("assistant-stop-quit")
        .padding([7, 14])
        .style(style::stop)
        .on_key_press(super::no_return)
        .on_press(Message::Assistant(Input::Task(tasks::Input::ConfirmQuit)));
    let (first, second) = dialog_actions(keep, stop, compact);
    let footer: Element<'_, Message> = if compact {
        column![first.width(Fill), second.width(Fill)]
            .spacing(10)
            .into()
    } else {
        row![space::horizontal(), first, second].spacing(10).into()
    };
    let title = "Stop the assistant and quit?";
    let dialog = container(column![
        text(title).size(style::HEADING).font(style::SEMIBOLD),
        text("The assistant is still working. Quitting stops the task and closes this session. Recordings and schedules in this session also stop.").size(style::BODY).wrapping(text::Wrapping::WordOrGlyph),
        footer,
    ].spacing(16)).width(width).padding(24).style(style::sheet);
    let dialog = keyboard_history::escape_guard(accessibility::group(
        keyboard_history::default_action(dialog.into(), None),
        title.into(),
        accessibility::Role::AlertDialog,
    ));
    opaque(center(opaque(dialog)).style(|theme| style::scrim(theme, 1.0)))
}
