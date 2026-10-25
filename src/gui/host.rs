use super::*;
use std::collections::BTreeMap;

pub(super) struct Host {
    windows: BTreeMap<window::Id, Workbench>,
    focused: window::Id,
    shared: Prefs,
    released: Vec<String>,
}

fn inner(message: &Message) -> &Message {
    match message {
        Message::InWindow(_, message) => message,
        message => message,
    }
}

impl Workbench {
    fn vacant(&self) -> bool {
        self.local_camera.is_none() && self.closing.is_none() && !self.pending(Job::Connect)
    }
}

impl Host {
    pub(super) fn new(mut bench: Workbench) -> Self {
        if bench.assistant.commands.from_disk_image {
            bench.put_notice(
                agents::DISK_IMAGE_NOTICE.into(),
                Some(agents::DISK_IMAGE_DETAIL.into()),
                notice::Level::Warning,
            );
        }
        let focused = bench.main_window;
        Self {
            shared: bench.prefs().shared(),
            windows: BTreeMap::from([(focused, bench)]),
            focused,
            released: Vec::new(),
        }
    }

    fn find(&self, id: window::Id) -> Option<&Workbench> {
        self.windows.values().find(|bench| bench.known_window(id))
    }

    fn owner(&self, id: window::Id) -> window::Id {
        self.find(id)
            .map_or(self.focused, |bench| bench.main_window)
    }

    pub(super) fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::From(id, message) => self.route(id, *message),
            Message::SystemTheme(_) => {
                let ids: Vec<_> = self.windows.keys().copied().collect();
                Task::batch(ids.into_iter().map(|id| self.route(id, message.clone())))
            }
            Message::InWindow(id, _) => {
                let owner = self.owner(id);
                if matches!(inner(&message), Message::WindowFocused(_)) {
                    self.focused = owner;
                }
                self.route(owner, message)
            }
            message => self.route(self.focused, message),
        }
    }

    fn route(&mut self, id: window::Id, message: Message) -> Task<Message> {
        if !self.windows.contains_key(&id) {
            return Task::none();
        }
        if let Some(task) = self.intercept(id, inner(&message)) {
            return task;
        }
        let sole = self.windows.len() == 1;
        let settings = self
            .windows
            .iter()
            .filter(|(main, _)| **main != id)
            .find_map(|(_, bench)| bench.settings_window);
        let bench = self.windows.get_mut(&id).expect("a live window");
        bench.sole = sole;
        bench.settings_elsewhere = settings;
        let frame_only = matches!(
            inner(&message),
            Message::Frame(_)
                | Message::FrameArrived
                | Message::Redrawn
                | Message::Pointer
                | Message::OverControls(_)
        );
        let tick = matches!(message, Message::Tick);
        let before = bench.local_camera.clone();
        let task = bench
            .update(message)
            .map(move |message| Message::From(id, Box::new(message)));
        if bench.retired {
            self.retire(id);
            return task;
        }
        let handed = self.hand_over(id, before);
        let shared = if frame_only {
            Task::none()
        } else {
            self.share(id)
        };
        let claimed = if tick { self.claim(id) } else { Task::none() };
        Task::batch([task, handed, shared, claimed])
    }

    fn intercept(&mut self, id: window::Id, message: &Message) -> Option<Task<Message>> {
        match message {
            Message::NewWindow => Some(self.new_window()),
            Message::QuitAll => Some(self.quit()),
            Message::Select(camera)
            | Message::CameraRow(camera)
            | Message::CameraActivity(camera) => {
                let owner = self.windows.values().find(|bench| {
                    bench.main_window != id && bench.local_camera.as_ref() == Some(camera)
                })?;
                Some(window::gain_focus(owner.main_window))
            }
            _ => None,
        }
    }

    fn hand_over(&mut self, id: window::Id, before: Option<String>) -> Task<Message> {
        let Some(camera) = self.windows[&id]
            .local_camera
            .clone()
            .filter(|camera| before.as_ref() != Some(camera))
        else {
            return Task::none();
        };
        let Some(owner) = self
            .windows
            .values()
            .find(|bench| bench.main_window != id && bench.local_camera.as_ref() == Some(&camera))
            .map(|bench| bench.main_window)
        else {
            return Task::none();
        };
        if let Some(bench) = self.windows.get_mut(&id) {
            bench.show_camera(None);
        }
        window::gain_focus(owner)
    }

    fn quit(&mut self) -> Task<Message> {
        if let Some((id, bench)) = self
            .windows
            .iter_mut()
            .find(|(_, bench)| bench.assistant.tasks.active())
        {
            let id = *id;
            return bench
                .guard_assistant_quit(CloseTarget::Quit)
                .map(move |message| Message::From(id, Box::new(message)));
        }
        Task::batch(
            self.windows
                .iter_mut()
                .filter(|(_, bench)| bench.closing.is_none())
                .map(|(id, bench)| {
                    let id = *id;
                    bench
                        .flush(CloseTarget::Quit)
                        .map(move |message| Message::From(id, Box::new(message)))
                })
                .collect::<Vec<_>>(),
        )
    }

    fn new_window(&mut self) -> Task<Message> {
        let Some(id) = self
            .windows
            .values()
            .find(|bench| bench.vacant())
            .map(|bench| bench.main_window)
        else {
            return self.open_window(None);
        };
        let enter = self.route(id, Message::EnterAddress);
        window::gain_focus(id).chain(enter)
    }

    fn open_window(&mut self, camera: Option<String>) -> Task<Message> {
        let source = &self.windows[&self.focused];
        let mut bench = Workbench::new(
            source.handle.clone(),
            source.session.clone(),
            false,
            source.saved.clone(),
        );
        drop(bench.handle_message(Message::Adopt(Box::new(source.prefs()))));
        let (id, opened) = window::open(camera_window_settings(
            Size::new(source.width, source.height),
            window::Position::Default,
        ));
        bench.main_window = id;
        bench.focused_window = id;
        bench.key_window = None;
        bench.quit_signal = source.quit_signal.clone();
        bench.system_dark = source.system_dark;
        bench.width = source.width;
        bench.height = source.height;
        match camera {
            Some(camera) => bench.show_camera(Some(camera)),
            None => bench.discover(),
        }
        self.windows.insert(id, bench);
        opened
            .map(Message::WindowOpened)
            .chain(screen_refresh(id))
            .map(move |message| Message::From(id, Box::new(message)))
    }

    fn retire(&mut self, id: window::Id) {
        if let Some(camera) = self
            .windows
            .remove(&id)
            .and_then(|bench| bench.local_camera)
        {
            self.released.push(camera);
        }
        if self.focused == id
            && let Some(main) = self.windows.keys().next()
        {
            self.focused = *main;
        }
    }

    fn share(&mut self, id: window::Id) -> Task<Message> {
        let shared = self.windows[&id].prefs().shared();
        if shared == self.shared {
            return Task::none();
        }
        self.shared = shared;
        let others: Vec<_> = self
            .windows
            .keys()
            .copied()
            .filter(|main| *main != id)
            .collect();
        Task::batch(
            others
                .into_iter()
                .map(|other| self.route(other, Message::Adopt(Box::new(self.shared.clone())))),
        )
    }

    fn claim(&mut self, id: window::Id) -> Task<Message> {
        if self
            .windows
            .values()
            .any(|bench| bench.pending(Job::Connect))
        {
            return Task::none();
        }
        let screenshot = self
            .windows
            .values()
            .any(|bench| bench.screenshot.is_some());
        let cameras: Vec<String> = self.windows[&id]
            .snapshot
            .cameras
            .iter()
            .map(|camera| camera.info.id.clone())
            .collect();
        self.released.retain(|camera| cameras.contains(camera));
        let mut opened = Vec::new();
        for camera in cameras {
            if self.released.contains(&camera)
                || self
                    .windows
                    .values()
                    .any(|bench| bench.local_camera.as_ref() == Some(&camera))
            {
                continue;
            }
            match self.windows.values_mut().find(|bench| bench.vacant()) {
                Some(bench) => bench.show_camera(Some(camera)),
                None if !screenshot => opened.push(self.open_window(Some(camera))),
                None => {}
            }
        }
        Task::batch(opened)
    }

    pub(super) fn view(&self, id: window::Id) -> Element<'_, Message> {
        self.find(id)
            .map_or_else(|| space::horizontal().into(), |bench| bench.window_view(id))
    }

    pub(super) fn title(&self, id: window::Id) -> String {
        match self.find(id) {
            Some(bench) if bench.settings_window == Some(id) => "Settings".into(),
            Some(bench) => bench.title(),
            None => "Capturefab".into(),
        }
    }

    pub(super) fn theme(&self, id: window::Id) -> Option<Theme> {
        self.find(id).map(|bench| bench.window_theme(id))
    }

    pub(super) fn scale_factor(&self, id: window::Id) -> f32 {
        self.find(id)
            .or_else(|| self.windows.get(&self.focused))
            .map_or(1.0, |bench| bench.ui_scale.factor())
    }

    pub(super) fn subscription(&self) -> Subscription<Message> {
        let menu_active = self
            .windows
            .get(&self.focused)
            .is_some_and(|bench| bench.desktop_menu.active());
        Subscription::batch(
            self.windows
                .iter()
                .map(|(id, bench)| {
                    bench
                        .subscription()
                        .with(*id)
                        .map(|(id, message)| Message::From(id, Box::new(message)))
                })
                .chain([events(menu_active)]),
        )
    }
}

fn events(menu_active: bool) -> Subscription<Message> {
    Subscription::batch([
        event::listen_with(|event, status, window| {
            let message = match event {
            iced::Event::Window(window::Event::Focused) => Some(Message::WindowFocused(window)),
            iced::Event::Window(window::Event::Unfocused) => Some(Message::WindowUnfocused(window)),
            iced::Event::Window(window::Event::CloseRequested) => {
                Some(Message::CloseRequested(window))
            }
            iced::Event::Window(
                window::Event::Opened { .. }
                | window::Event::Moved(_)
                | window::Event::Rescaled(_),
            ) => Some(Message::Displaced),
            // The widget guard routes Escape after inspecting the active
            // popup. A captured popup Escape closes only that popup.
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                modifiers,
                ..
            }) if modifiers.is_empty() => None,
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Tab),
                modifiers,
                ..
            }) if status == event::Status::Ignored
                && !modifiers.control()
                && !modifiers.logo()
                && !modifiers.alt() =>
            {
                Some(Message::FocusNext(modifiers.shift()))
            }
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                physical_key,
                modifiers,
                ..
            }) => keys::chord(&key, physical_key, modifiers, Os::CURRENT).map(|chord| {
                Message::Key {
                    chord,
                    captured: status == event::Status::Captured,
                }
            }),
            _ => None,
            };
            message.map(|message| Message::InWindow(window, Box::new(message)))
        }).with(menu_active).filter_map(|(active, message)| {
            if active && matches!(&message, Message::InWindow(_, inner) if matches!(**inner, Message::Key { .. } | Message::FocusNext(_))) {
                None
            } else {
                Some(message)
            }
        }),
        system::theme_changes().map(Message::SystemTheme),
        window::resize_events().map(|(id, size)| Message::InWindow(id, Box::new(Message::Resized(size)))),
        motion::accessibility_subscription(),
        menus::subscription(),
    ])
}

#[cfg(test)]
mod tests {
    use super::super::window_lifecycle_tests::{Effect, effects};
    use super::*;
    use iced::futures::StreamExt;

    fn host() -> (Host, window::Id) {
        let bench = Workbench::new(SessionHandle::new(), "host".into(), false, None);
        let main = bench.main_window;
        (Host::new(bench), main)
    }

    fn camera(host: &mut Host, id: &str) -> window::Id {
        drop(host.open_window(Some(id.into())));
        host.windows
            .values()
            .find(|bench| bench.local_camera.as_deref() == Some(id))
            .unwrap()
            .main_window
    }

    fn close(host: &mut Host, id: window::Id) -> Message {
        let task = host.update(Message::From(id, Box::new(Message::CloseRequested(id))));
        let mut stream = iced_runtime::task::into_stream(task).expect("close flushes preferences");
        match iced::futures::executor::block_on(stream.next()) {
            Some(iced_runtime::Action::Output(message)) => message,
            _ => panic!("close must deliver its flush through the host"),
        }
    }

    #[test]
    fn closing_one_of_several_windows_keeps_the_app_and_the_last_quits() {
        let (mut host, first) = host();
        let second = camera(&mut host, "sim:1");
        let flushed = close(&mut host, second);
        assert!(matches!(&flushed, Message::From(id, inner)
            if *id == second && matches!(**inner, Message::PrefsFlushed(Ok(())))));
        let Message::From(_, inner) = &flushed else {
            unreachable!()
        };
        assert!(
            effects(host.update(Message::InWindow(second, inner.clone()))).is_empty(),
            "a closing window hears only its own flush"
        );
        let closed = effects(host.update(flushed));
        assert!(closed.contains(&Effect::Close(second)), "{closed:?}");
        assert!(!closed.contains(&Effect::Exit));
        assert_eq!(host.windows.keys().copied().collect::<Vec<_>>(), [first]);
        assert_eq!(host.released, ["sim:1"]);
        let flushed = close(&mut host, first);
        assert_eq!(effects(host.update(flushed)).last(), Some(&Effect::Exit));
    }

    fn outputs(task: Task<Message>) -> Vec<Message> {
        let Some(stream) = iced_runtime::task::into_stream(task) else {
            return Vec::new();
        };
        iced::futures::executor::block_on(
            stream
                .filter_map(|action| async move {
                    match action {
                        iced_runtime::Action::Output(message) => Some(message),
                        _ => None,
                    }
                })
                .collect(),
        )
    }

    #[test]
    fn quit_flushes_every_window_and_exits_after_the_last() {
        let (mut host, _) = host();
        let second = camera(&mut host, "sim:1");
        let mut requested = outputs(host.update(Message::From(second, Box::new(Message::Quit))));
        assert!(matches!(&requested[..], [Message::From(id, inner)]
            if *id == second && matches!(**inner, Message::QuitAll)));
        let flushed = outputs(host.update(requested.remove(0)));
        assert!(
            host.windows
                .values()
                .all(|bench| matches!(bench.closing, Some(CloseTarget::Quit)))
        );
        let [early, late] = <[Message; 2]>::try_from(flushed).expect("one flush per window");
        assert!(!effects(host.update(early)).contains(&Effect::Exit));
        assert_eq!(effects(host.update(late)).last(), Some(&Effect::Exit));
        assert!(host.released.is_empty(), "quitting disconnects nothing");
    }

    #[test]
    fn quit_from_any_window_waits_for_an_assistant_task_in_another() {
        use super::super::assistant::task_integration_tests::{LiveRun, app_with_run};
        let run = LiveRun::new();
        let mut host = Host::new(app_with_run(&run, false));
        let busy = host.focused;
        let other = camera(&mut host, "sim:1");
        let mut requested = outputs(host.update(Message::From(other, Box::new(Message::Quit))));
        let guarded = effects(host.update(requested.remove(0)));
        assert_eq!(guarded.first(), Some(&Effect::Focus(busy)));
        assert!(host.windows[&busy].assistant.quit.is_some());
        assert!(host.windows.values().all(|bench| bench.closing.is_none()));
    }

    #[test]
    fn connecting_a_camera_another_window_shows_focuses_that_window() {
        let (mut host, empty) = host();
        let owner = camera(&mut host, "sim:1");
        let (sender, receiver) = mpsc::channel();
        sender
            .send(Ok(serde_json::json!({
                "connected": crate::transport::simulator::info_named("sim:1")
            })))
            .unwrap();
        host.windows.get_mut(&empty).unwrap().pending.push(Pending {
            receiver,
            target: Some("sim:1".into()),
            ..Pending::unanswered(Job::Connect)
        });
        let effects = effects(host.update(Message::From(empty, Box::new(Message::Tick))));
        assert!(effects.contains(&Effect::Focus(owner)), "{effects:?}");
        assert!(host.windows[&empty].local_camera.is_none());
        assert_eq!(host.windows[&owner].local_camera.as_deref(), Some("sim:1"));
    }

    #[test]
    fn closing_a_recording_window_asks_before_disconnecting() {
        let (mut host, _) = host();
        let second = camera(&mut host, "sim:1");
        let bench = host.windows.get_mut(&second).unwrap();
        let mut recording = liveness::streaming_camera(0, 0, 0.0);
        recording.info.id = "sim:1".into();
        recording.forwarding = Some("/tmp/clip.mp4".into());
        bench.snapshot.cameras = vec![recording];
        bench.project_local_camera();
        drop(host.update(Message::From(
            second,
            Box::new(Message::CloseRequested(second)),
        )));
        assert!(host.windows[&second].recording_close_confirmed());
        assert!(host.windows[&second].closing.is_none());
        drop(host.update(Message::From(second, Box::new(Message::KeepOpen))));
        assert!(host.windows[&second].close_error.is_none());
        drop(host.update(Message::From(
            second,
            Box::new(Message::CloseRequested(second)),
        )));
        let flushed = outputs(host.update(Message::From(second, Box::new(Message::RetryClose))));
        assert!(matches!(
            host.windows[&second].closing,
            Some(CloseTarget::Window)
        ));
        let closed = effects(host.update(flushed.into_iter().next().unwrap()));
        assert!(closed.contains(&Effect::Close(second)), "{closed:?}");
        assert_eq!(host.released, ["sim:1"]);
    }

    #[test]
    fn a_flush_that_outlives_its_sibling_quits_as_the_last_window() {
        let (mut host, first) = host();
        let second = camera(&mut host, "sim:1");
        let late = close(&mut host, first);
        let early = close(&mut host, second);
        assert!(!effects(host.update(early)).contains(&Effect::Exit));
        assert_eq!(effects(host.update(late)).last(), Some(&Effect::Exit));
    }

    #[test]
    fn picking_a_camera_another_window_shows_focuses_that_window() {
        let (mut host, first) = host();
        let owner = camera(&mut host, "sim:1");
        for message in [
            Message::From(first, Box::new(Message::Select("sim:1".into()))),
            Message::InWindow(first, Box::new(Message::CameraRow("sim:1".into()))),
        ] {
            assert_eq!(effects(host.update(message)), [Effect::Focus(owner)]);
        }
        assert!(host.windows[&first].local_camera.is_none());
    }

    #[test]
    fn new_window_reuses_an_empty_window_before_opening_one() {
        let (mut host, empty) = host();
        let shown = camera(&mut host, "sim:1");
        let reused = effects(host.update(Message::From(shown, Box::new(Message::NewWindow))));
        assert_eq!(reused.first(), Some(&Effect::Focus(empty)));
        assert_eq!(host.windows.len(), 2);
        host.windows
            .get_mut(&empty)
            .unwrap()
            .show_camera(Some("sim:0".into()));
        drop(host.update(Message::From(shown, Box::new(Message::NewWindow))));
        assert_eq!(host.windows.len(), 3);
    }

    #[test]
    fn window_events_reach_their_window_and_the_theme_reaches_all() {
        let (mut host, first) = host();
        let second = camera(&mut host, "sim:1");
        drop(host.update(Message::InWindow(
            second,
            Box::new(Message::WindowFocused(second)),
        )));
        assert_eq!(host.focused, second);
        assert_eq!(host.windows[&second].key_window, Some(second));
        drop(host.update(Message::InWindow(
            second,
            Box::new(Message::Address("typed".into())),
        )));
        assert_eq!(host.windows[&second].address, "typed");
        assert!(host.windows[&first].address.is_empty());
        drop(host.update(Message::SystemTheme(theme::Mode::Dark)));
        assert!(host.windows.values().all(|bench| bench.system_dark));
    }

    #[test]
    fn shared_preferences_follow_every_window_but_layout_stays() {
        let bench = Workbench::new(
            SessionHandle::new(),
            "host".into(),
            false,
            Some(Prefs::default()),
        );
        let first = bench.main_window;
        let mut host = Host::new(bench);
        let second = camera(&mut host, "sim:1");
        drop(host.update(Message::From(first, Box::new(Message::ToggleActivity))));
        drop(host.update(Message::From(
            first,
            Box::new(Message::Appearance(Appearance::Dark)),
        )));
        let other = &host.windows[&second];
        assert_eq!(other.appearance, Appearance::Dark);
        assert_eq!(other.saved.as_ref().unwrap().appearance, Appearance::Dark);
        assert_ne!(other.logs_open, host.windows[&first].logs_open);
        assert!(other.saved.as_ref() == Some(&other.prefs()));
    }

    #[test]
    fn only_one_settings_window_opens_across_windows() {
        let (mut host, first) = host();
        let second = camera(&mut host, "sim:1");
        let settings = window::Id::unique();
        host.windows.get_mut(&first).unwrap().settings_window = Some(settings);
        assert_eq!(
            effects(host.update(Message::From(second, Box::new(Message::Settings(true))))),
            [Effect::Focus(settings)]
        );
        assert!(host.windows[&second].settings_window.is_none());
    }

    #[test]
    fn a_camera_no_window_shows_gets_a_window() {
        let (mut host, first) = host();
        let mut snapshot = liveness::streaming_camera(0, 0, 0.0);
        let cameras: Vec<_> = ["sim:0", "sim:1"]
            .map(|id| {
                snapshot.info.id = id.into();
                snapshot.clone()
            })
            .into();
        host.windows.get_mut(&first).unwrap().snapshot.cameras = cameras;
        drop(host.claim(first));
        let shown: Vec<_> = host
            .windows
            .values()
            .map(|bench| bench.local_camera.clone())
            .collect();
        assert_eq!(shown.len(), 2);
        assert_eq!(host.windows[&first].local_camera.as_deref(), Some("sim:0"));
        assert!(shown.contains(&Some("sim:1".into())));
        let released = host
            .windows
            .keys()
            .copied()
            .find(|id| *id != first)
            .unwrap();
        host.retire(released);
        drop(host.claim(first));
        assert_eq!(
            host.windows.len(),
            1,
            "a closed window's camera is released"
        );
    }
}
