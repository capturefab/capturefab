//! The assistant's editors live in the workbench; settings, credentials and
//! model requests run on named background workers.
use super::*;

/// Camera actions require explicit activation. Space, pointer and native
/// accessibility clicks remain available when a button has focus.
pub(super) fn no_return<M>(key: &keyboard::Key, _: keyboard::Modifiers) -> Option<Vec<M>> {
    matches!(key, keyboard::Key::Named(keyboard::key::Named::Enter)).then(Vec::new)
}

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
#[cfg(test)]
mod task_integration_tests;
#[cfg(test)]
mod task_layout_tests;
mod task_quit;
mod task_scenes;
mod task_view;
pub(super) mod tasks;
mod tasks_worker;

#[derive(Clone, Debug)]
pub(super) enum Input {
    Settings(settings::Input),
    Quick(quick::Input),
    Task(tasks::Input),
}

#[derive(Default)]
pub(super) struct State {
    pub(super) commands: agents::CommandContext,
    pub settings: settings::State,
    pub quick: quick::State,
    pub tasks: tasks::State,
    local_actors: std::collections::BTreeSet<String>,
    handoff: Option<bool>,
    pub(super) quit: Option<CloseTarget>,
    announced_approval: Option<(String, u64)>,
    takeover: Option<tasks::TakeoverTarget>,
}

impl Workbench {
    pub(super) fn update_assistant(&mut self, input: Input) -> Task<Message> {
        match input {
            Input::Settings(settings::Input::ContinueToQuick) => {
                if !self.assistant.settings.ready_to_continue() {
                    return Task::none();
                }
                return self
                    .handle_message(Message::Settings(false))
                    .chain(Task::done(Message::Assistant(Input::Quick(
                        quick::Input::Open(true),
                    ))));
            }
            Input::Settings(input) => self.assistant.settings.input(input, self.now),
            Input::Task(input) => return self.update_assistant_task(input),
            Input::Quick(input) => {
                if let quick::Input::Task(input) = input {
                    return self.update_assistant_task(input);
                }
                if let quick::Input::TaskMode(mode) = input {
                    return self.update_assistant_task(tasks::Input::Mode(mode));
                }
                if matches!(input, quick::Input::NewConversation) {
                    if self.assistant.tasks.active() {
                        return Task::none();
                    }
                    let _ = self.assistant.tasks.input(tasks::Input::NewTask, self.now);
                }
                if let quick::Input::Open(open) = &input {
                    if *open && self.help_open && self.sheets.agents {
                        self.help_open = false;
                    }
                    if *open && !self.assistant.quick.open && self.sheet_open() {
                        return Task::none();
                    }
                    if *open && !self.assistant.quick.synthetic() {
                        self.assistant.settings.ensure_loaded();
                    }
                    let _ = self
                        .assistant
                        .tasks
                        .input(tasks::Input::Open(*open), self.now);
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
        if self.assistant.quick.open || self.assistant.quick.busy() || self.assistant.tasks.active()
        {
            self.configure_assistant_quick();
        }
        self.assistant.quick.tick(self.now);
        if let Some(context) = &self.assistant.quick.context {
            self.assistant.tasks.set_context(
                context.clone(),
                self.snapshot.active_camera.clone().or_else(|| {
                    self.snapshot
                        .connected
                        .as_ref()
                        .map(|camera| camera.id.clone())
                }),
            );
        }
        self.assistant.tasks.tick(self.now);
        if self.assistant.tasks.take_context_refresh() {
            self.assistant.quick.refresh_context(self.now);
        }
        if let Some(target) = self.assistant.tasks.take_takeover() {
            self.assistant.takeover = Some(target);
        }
        if let Some(actor) = self.assistant.tasks.actor_id() {
            self.assistant.local_actors.insert(actor);
        }
        if let Some((message, detail, level)) = self.assistant.quick.take_notice() {
            self.set_notice(message, detail, level);
        }
        if let Some((message, detail, level)) = self.assistant.tasks.take_notice() {
            self.set_notice(message, detail, level);
        }
        let approval = self
            .assistant
            .tasks
            .approval
            .as_ref()
            .map(|request| (request.run.clone(), request.id));
        if approval != self.assistant.announced_approval {
            self.assistant.announced_approval = approval.clone();
            if approval.is_some() && !self.assistant.quick.open {
                self.set_notice(
                    "AI agent needs approval",
                    Some("Open Quick Command to review this action.".into()),
                    Level::Warning,
                );
            }
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
        self.assistant.quick.configure(settings.clone(), services);
        self.assistant
            .quick
            .source_setup
            .configure(Arc::new(source_setup::Native(self.handle.clone())));
        let task_services = Arc::new(tasks_worker::NativeServices::new(
            self.handle.clone(),
            self.actor.clone(),
            prefs::capture_root(),
            self.storage_policy(),
            crate::tools::context::RecordingDefaults {
                codec: self.forward_codec.into(),
                fps: self.forward_fps,
                bitrate: self.forward_bitrate.clone(),
            },
        ));
        self.assistant.tasks.configure(settings, task_services);
        self.assistant
            .tasks
            .source_setup
            .configure(Arc::new(source_setup::Native(self.handle.clone())));
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
            quick::Effect::FocusField(id) => operation::focus(id),
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
        if let Some(target) = self.assistant.takeover.take() {
            return match target {
                tasks::TakeoverTarget::Camera(camera) => self.assistant_take_over(camera),
                tasks::TakeoverTarget::Data => {
                    let _ = self.update_assistant(Input::Quick(quick::Input::Open(false)));
                    self.set_notice(
                        "You have control of data sources. The assistant stopped.",
                        None,
                        Level::Done,
                    );
                    self.open_data_sources()
                }
            };
        }
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
        if self.assistant.tasks.mode != tasks::Mode::Plan
            || self.assistant.tasks.active()
            || self.assistant.tasks.outcome.is_some()
        {
            return accessibility::mapped(
                task_view::content(&self.assistant.tasks, p, height, self.spin()),
                |input| Message::Assistant(Input::Task(input)),
            );
        }
        accessibility::mapped(
            sheet::content_with_banner(
                &self.assistant.quick,
                p,
                height,
                self.spin(),
                task_view::plan_notice(&self.assistant.tasks, p)
                    .map(|notice| accessibility::mapped(notice, quick::Input::Task)),
                self.assistant
                    .quick
                    .exported_plan
                    .as_ref()
                    .and_then(|path| {
                        self.assistant.quick.context.as_ref().and_then(|context| {
                            self.assistant.commands.plan_preview(
                                &self.session,
                                path,
                                &context.capture_root,
                            )
                        })
                    }),
            ),
            |input| Message::Assistant(Input::Quick(input)),
        )
    }

    fn update_assistant_task(&mut self, input: tasks::Input) -> Task<Message> {
        let new_task = matches!(input, tasks::Input::NewTask) && !self.assistant.tasks.active();
        match input {
            tasks::Input::ConfirmQuit => return self.confirm_assistant_quit(),
            tasks::Input::KeepRunning => {
                self.assistant.quit = None;
                self.accessibility.invalidate(self.main_window);
                return Task::none();
            }
            tasks::Input::Open(open) => {
                return self.update_assistant(Input::Quick(quick::Input::Open(open)));
            }
            tasks::Input::Settings => return self.assistant_effect(quick::Effect::Settings),
            tasks::Input::Mode(_)
                if self.assistant.quick.thinking() || self.assistant.quick.running() =>
            {
                return Task::none();
            }
            _ => {}
        }
        self.configure_assistant_quick();
        if matches!(
            input,
            tasks::Input::Mode(tasks::Mode::Supervised | tasks::Mode::Autonomous)
        ) && self.assistant.tasks.goal.trim().is_empty()
        {
            self.assistant.tasks.goal = self.assistant.quick.prompt.clone();
        }
        if let Some(context) = &self.assistant.quick.context {
            self.assistant.tasks.set_context(
                context.clone(),
                self.snapshot.active_camera.clone().or_else(|| {
                    self.snapshot
                        .connected
                        .as_ref()
                        .map(|camera| camera.id.clone())
                }),
            );
        }
        let effect = self.assistant.tasks.input(input, self.now);
        if new_task && matches!(effect, tasks::Effect::ModeChanged(tasks::Mode::Plan)) {
            let _ = self
                .assistant
                .quick
                .input(quick::Input::NewConversation, self.now);
        }
        match effect {
            tasks::Effect::None => Task::none(),
            tasks::Effect::ModeChanged(mode) => {
                if mode == tasks::Mode::Plan {
                    operation::focus("quick-command-prompt")
                } else {
                    operation::focus("assistant-task-goal")
                }
            }
        }
    }

    fn assistant_take_over(&mut self, camera: String) -> Task<Message> {
        let name = self
            .snapshot
            .cameras
            .iter()
            .find(|item| item.info.id == camera)
            .map_or(camera.as_str(), |item| item.info.model.as_str())
            .to_owned();
        let name = crate::tools::redact::for_output_text(&name);
        let _ = self.update_assistant(Input::Quick(quick::Input::Open(false)));
        self.inspector_open = true;
        self.inspector_summoned = true;
        self.inspector_peek = !self.inspector_docked();
        self.inspector_slide.go(1.0, self.now);
        self.set_notice(
            format!("You have control of {name}. The assistant stopped."),
            None,
            Level::Done,
        );
        Task::done(Message::Select(camera))
    }

    pub(super) fn assistant_ui_busy(&self) -> bool {
        self.assistant.quick.open && (self.assistant.quick.busy() || self.assistant.tasks.busy())
    }

    pub(super) fn assistant_toolbar_status(
        &self,
        p: &'static Palette,
    ) -> Option<Element<'_, Message>> {
        let task = &self.assistant.tasks;
        if !task.active() || self.assistant.quick.open {
            return None;
        }
        let elapsed = task.snapshot.elapsed_ms / 1000;
        let label = if task.approval.is_some() {
            "AI agent needs approval".to_owned()
        } else {
            format!(
                "Assistant · round {}/{} · {}:{:02}",
                task.snapshot.round,
                task.limits.max_rounds,
                elapsed / 60,
                elapsed % 60
            )
        };
        let meters = format!(
            "{} rounds of {}; {} images of {}; {} captures of {}; {} bytes of {}. {}",
            task.snapshot.round,
            task.limits.max_rounds,
            task.snapshot.totals.images,
            task.limits.max_images,
            task.snapshot.totals.captures,
            task.limits.max_captures,
            task.snapshot.totals.bytes,
            task.limits.max_bytes,
            task.snapshot.summary
        );
        let ink = if task.approval.is_some() {
            Level::Warning.color(p)
        } else {
            p.secondary
        };
        let open = button(
            row![
                icon(Icon::Sparkle, 13.0, ink),
                one_line(label.clone(), style::SMALL, style::MEDIUM, ink).yield_room()
            ]
            .spacing(5)
            .align_y(Alignment::Center),
        )
        .name(label.clone())
        .description(meters.clone())
        .style(style::plain)
        .padding([4, 3])
        .width(Fill)
        .on_press(Message::Assistant(Input::Task(tasks::Input::Open(true))));
        let stop = button(text("Stop").size(style::SMALL))
            .name("Stop assistant task")
            .on_key_press(no_return)
            .padding([4, 6])
            .style(style::stop)
            .on_press(Message::Assistant(Input::Task(tasks::Input::Stop)));
        Some(accessibility::label(
            tip(
                row![open, stop].spacing(4).align_y(Alignment::Center),
                meters,
            ),
            label,
            accessibility::Role::Status,
        ))
    }

    pub(super) fn assistant_inspector_status(
        &self,
        p: &'static Palette,
    ) -> Option<Element<'_, Message>> {
        let camera = self.snapshot.active_camera.as_deref().or_else(|| {
            self.snapshot
                .connected
                .as_ref()
                .map(|info| info.id.as_str())
        })?;
        if !self.assistant.tasks.active() || !self.assistant.tasks.scope_contains(camera) {
            return None;
        }
        let label = "AI agent is working on this camera";
        let message = |input| Message::Assistant(Input::Task(input));
        Some(
            container(
                iced::widget::column![
                    accessibility::label(
                        row![
                            icon(Icon::Sparkle, 13.0, p.secondary),
                            text(label)
                                .size(style::SMALL)
                                .wrapping(text::Wrapping::WordOrGlyph)
                        ]
                        .spacing(6)
                        .into(),
                        label.into(),
                        accessibility::Role::Status
                    ),
                    row![
                        button(text("Stop").size(style::SMALL))
                            .name("Stop assistant task on this camera")
                            .on_key_press(no_return)
                            .style(style::stop)
                            .padding([4, 8])
                            .on_press(message(tasks::Input::Stop)),
                        button(text("Take over").size(style::SMALL))
                            .name("Take over this camera from the assistant")
                            .on_key_press(no_return)
                            .style(style::secondary)
                            .padding([4, 8])
                            .on_press(message(tasks::Input::TakeOver(
                                tasks::TakeoverTarget::Camera(camera.into())
                            ))),
                    ]
                    .spacing(8),
                ]
                .spacing(6),
            )
            .padding([8, 14])
            .width(Fill)
            .style(style::well)
            .into(),
        )
    }

    pub(super) fn assistant_settings_section(&self, p: &'static Palette) -> Element<'_, Message> {
        accessibility::mapped(self.assistant.settings.content(p, self.spin()), |input| {
            Message::Assistant(Input::Settings(input))
        })
    }
}
