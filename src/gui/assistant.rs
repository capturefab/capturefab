//! The assistant's editors live in the workbench; settings, credentials and
//! model requests run on named background workers.
use super::*;

mod cards;
pub(super) mod drafts;
#[cfg(test)]
mod integration_tests;
#[cfg(test)]
mod layout_tests;
pub(super) mod quick;
mod runner;
mod scenes;
pub(super) mod settings;
#[cfg(test)]
mod settings_layout_tests;
mod settings_scenes;
mod sheet;

#[derive(Clone, Debug)]
pub(super) enum Input {
    Settings(settings::Input),
    Quick(quick::Input),
}

#[derive(Default)]
pub(super) struct State {
    pub settings: settings::State,
    pub quick: quick::State,
    local_actors: std::collections::BTreeSet<String>,
    handoff: Option<bool>,
}

impl Workbench {
    pub(super) fn update_assistant(&mut self, input: Input) -> Task<Message> {
        match input {
            Input::Settings(input) => self.assistant.settings.input(input, self.now),
            Input::Quick(input) => {
                if let quick::Input::Open(open) = &input {
                    if *open && !self.assistant.quick.open && self.sheet_open() {
                        return Task::none();
                    }
                    if *open && !self.assistant.quick.synthetic() {
                        self.assistant.settings.ensure_loaded();
                    }
                }
                self.configure_assistant_quick();
                let effect = self.assistant.quick.input(input, self.now);
                return self.assistant_effect(effect);
            }
        }
        Task::none()
    }

    pub(super) fn tick_assistant(&mut self) {
        self.assistant.settings.tick(self.now);
        if self.assistant.quick.open || self.assistant.quick.busy() {
            self.configure_assistant_quick();
        }
        self.assistant.quick.tick(self.now);
        if let Some((message, detail, level)) = self.assistant.quick.take_notice() {
            self.set_notice(message, detail, level);
        }
    }

    fn configure_assistant_quick(&mut self) {
        if self.assistant.quick.synthetic() {
            return;
        }
        let actor = SessionActor {
            id: format!("assistant:{}", self.actor.id),
            name: "Assistant".into(),
            kind: crate::session::ActorKind::Agent,
        };
        self.assistant.local_actors.insert(actor.id.clone());
        let services = Arc::new(runner::NativeServices::new(
            self.handle.clone(),
            actor,
            prefs::capture_root(),
            self.storage_policy(),
            crate::tools::context::RecordingDefaults {
                codec: self.forward_codec.into(),
                fps: self.forward_fps,
                bitrate: self.forward_bitrate.clone(),
            },
        ));
        let settings = if self.assistant.settings.loaded() {
            self.assistant.settings.settings().clone()
        } else {
            self.assistant.quick.settings.clone()
        };
        self.assistant.quick.configure(settings, services);
        if self.assistant.settings.loaded() {
            self.assistant
                .quick
                .set_detected_environment(self.assistant.settings.detected_environment());
        }
    }

    pub(super) fn assistant_actor_is_local(&self, id: &str) -> bool {
        self.assistant.local_actors.contains(id)
    }

    fn assistant_effect(&mut self, effect: quick::Effect) -> Task<Message> {
        match effect {
            quick::Effect::None => Task::none(),
            quick::Effect::FocusPrompt => operation::focus("quick-command-prompt"),
            quick::Effect::FocusStep(id) => {
                operation::focus(cards::focus_id(&self.assistant.quick, id))
            }
            quick::Effect::Copy(value) => {
                iced::clipboard::write(crate::tools::redact::for_output_text(&value))
            }
            quick::Effect::Settings | quick::Effect::NewDestination => {
                let settings = matches!(effect, quick::Effect::Settings);
                let _ = self
                    .assistant
                    .quick
                    .input(quick::Input::Open(false), self.now);
                self.assistant.handoff = Some(settings);
                Task::none()
            }
        }
    }

    pub(super) fn take_assistant_handoff(&mut self) -> Task<Message> {
        match self.assistant.handoff.take() {
            Some(true) => {
                self.assistant
                    .settings
                    .input(settings::Input::Open(true), self.now);
                Task::done(Message::Settings(true))
            }
            Some(false) => Task::done(Message::CapturePicker(destinations::Message::OpenManager)),
            None => Task::none(),
        }
    }

    pub(super) fn assistant_sheet(&self, p: &'static Palette, height: f32) -> Element<'_, Message> {
        accessibility::mapped(
            sheet::content(&self.assistant.quick, p, height, self.spin()),
            |input| Message::Assistant(Input::Quick(input)),
        )
    }

    pub(super) fn assistant_settings_section(&self, p: &'static Palette) -> Element<'_, Message> {
        accessibility::mapped(self.assistant.settings.content(p, self.spin()), |input| {
            Message::Assistant(Input::Settings(input))
        })
    }
}
