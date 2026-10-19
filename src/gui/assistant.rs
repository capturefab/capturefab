//! The assistant's editors live in the workbench; settings, credentials and
//! model requests run on named background workers.
use super::*;

pub(super) mod settings;
#[cfg(test)]
mod settings_layout_tests;
mod settings_scenes;

#[derive(Clone, Debug)]
pub(super) enum Input {
    Settings(settings::Input),
}

#[derive(Default)]
pub(super) struct State {
    pub settings: settings::State,
}

impl Workbench {
    pub(super) fn update_assistant(&mut self, input: Input) -> Task<Message> {
        match input {
            Input::Settings(input) => self.assistant.settings.input(input, self.now),
        }
        Task::none()
    }

    pub(super) fn tick_assistant(&mut self) {
        self.assistant.settings.tick(self.now);
    }

    pub(super) fn assistant_settings_section(&self, p: &'static Palette) -> Element<'_, Message> {
        accessibility::mapped(self.assistant.settings.content(p, self.spin()), |input| {
            Message::Assistant(Input::Settings(input))
        })
    }
}
