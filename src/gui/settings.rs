//! App-wide preferences, kept separate from the selected camera's inspector.
use super::prefs::UiScale;
use super::*;
use iced::widget::column;

const APPEARANCES: [Appearance; 3] = [Appearance::System, Appearance::Light, Appearance::Dark];

/// Preferences are a separate, ordinary window on macOS. Its native title bar
/// owns closing; fixed content needs neither zooming nor minimization.
pub(super) fn window_settings() -> window::Settings {
    window::Settings {
        size: Size::new(520.0, 500.0),
        position: window::Position::Centered,
        resizable: false,
        minimizable: false,
        exit_on_close_request: false,
        ..window::Settings::default()
    }
}

impl fmt::Display for Appearance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        })
    }
}

impl Workbench {
    /// Both presentations use the same preferences and apply changes as they
    /// are made. Camera-specific controls stay in the camera inspector.
    pub(super) fn settings_content(&self, p: &'static Palette) -> Element<'_, Message> {
        let appearance_index = match self.appearance {
            Appearance::System => 0,
            Appearance::Light => 1,
            Appearance::Dark => 2,
        };
        let size_index = match self.ui_scale {
            UiScale::Standard => 0,
            UiScale::Large => 1,
            UiScale::Larger => 2,
        };
        let appearance = column![
            heading("Appearance", p),
            segmented(
                "Appearance",
                APPEARANCES
                    .into_iter()
                    .map(|appearance| (
                        match appearance {
                            Appearance::System => "System",
                            Appearance::Light => "Light",
                            Appearance::Dark => "Dark",
                        },
                        Some(Message::Appearance(appearance)),
                        match appearance {
                            Appearance::System => "Match the computer’s appearance",
                            Appearance::Light => "Use a light appearance",
                            Appearance::Dark => "Use a dark appearance",
                        }
                        .into(),
                    ))
                    .collect(),
                appearance_index,
                appearance_index as f32,
            ),
            text("System follows your computer’s light or dark appearance.")
                .size(style::SMALL)
                .color(p.secondary),
        ]
        .spacing(9);
        let interface = column![
            heading("Interface size", p),
            segmented(
                "Interface size",
                UiScale::ALL
                    .into_iter()
                    .map(|scale| (
                        match scale {
                            UiScale::Standard => "Standard",
                            UiScale::Large => "Large",
                            UiScale::Larger => "Larger",
                        },
                        Some(Message::UiScale(scale)),
                        match scale {
                            UiScale::Standard => "Use the standard interface size",
                            UiScale::Large => "Make text and controls 15% larger",
                            UiScale::Larger => "Make text and controls 30% larger",
                        }
                        .into(),
                    ))
                    .collect(),
                size_index,
                size_index as f32,
            ),
            text("Larger sizes increase both text and control sizes.")
                .size(style::SMALL)
                .color(p.secondary),
        ]
        .spacing(9);
        let discovery = column![
            heading("Camera discovery", p),
            checkbox(
                "Include simulated cameras",
                self.include_simulator,
                Message::IncludeSimulator,
            ),
            text("Use simulated cameras to explore Capturefab without connecting a device.")
                .size(style::SMALL)
                .color(p.secondary),
            checkbox(
                "Start the live view when a camera connects",
                self.auto_start,
                Message::AutoStart,
            ),
            text("Document scanners never start on their own; each scan uses a page.")
                .size(style::SMALL)
                .color(p.secondary),
        ]
        .spacing(9);
        column![
            appearance,
            rule::horizontal(1).style(style::line),
            interface,
            rule::horizontal(1).style(style::line),
            discovery,
            rule::horizontal(1).style(style::line),
            self.api_keys_section(p),
        ]
        .spacing(20)
        .into()
    }

    /// The API keys, folded by default: most people never need them, and
    /// the fold says what is set. Shown open, without remembering it, while
    /// it has an error to show.
    fn api_keys_section(&self, p: &'static Palette) -> Element<'_, Message> {
        let state = &self.video_analytics;
        let open = state.keys_section_open();
        let mut section = column![disclosure(
            "AI provider API keys",
            (!open).then(|| state.keys_summary()),
            self.keys_chevron(),
            Message::VideoAnalytics(video_analytics::Input::Keys(!open)),
            p,
        )]
        .spacing(9);
        if open {
            section = section.push(self.api_credentials_content(p));
        }
        section.into()
    }

    /// The keys chevron's turn. It follows what shows: mid-turn only while
    /// heading where the section now is, else at rest there, so a section
    /// an error or scene opened never sits under a closed chevron.
    fn keys_chevron(&self) -> f32 {
        let open = self.video_analytics.keys_section_open();
        let turn = &self.sheets.keys_turn;
        if (turn.target() >= 0.5) == open {
            turn.get(self.now)
        } else if open {
            1.0
        } else {
            0.0
        }
    }

    /// The line under Settings: what applies at once, and while the keys
    /// show, what does not.
    fn settings_footer_text(&self) -> &'static str {
        if self.video_analytics.keys_section_open() {
            "Preferences apply automatically. API keys require Save key."
        } else {
            "Preferences apply automatically."
        }
    }

    pub(super) fn settings_window(&self, p: &'static Palette) -> Element<'_, Message> {
        container(
            column![
                scrollable(self.settings_content(p))
                    .height(Fill)
                    .style(style::scroll),
                text(self.settings_footer_text())
                    .size(style::SMALL)
                    .color(p.secondary),
            ]
            .spacing(20),
        )
        .padding(24)
        .width(Fill)
        .height(Fill)
        .into()
    }

    pub(super) fn settings_sheet(
        &self,
        p: &'static Palette,
        max_height: f32,
    ) -> Element<'_, Message> {
        let close = button(text("Close").size(style::BODY))
            .name("Close Settings")
            .padding([7, 18])
            .style(style::secondary)
            .on_press(Message::Settings(false));
        // There is one action: changes already apply. Keep it at the trailing
        // edge on each desktop platform, with no misleading Save/Cancel pair.
        let footer = row![
            text(self.settings_footer_text())
                .size(style::SMALL)
                .color(p.secondary)
                .width(Fill),
            close,
        ]
        .spacing(12)
        .align_y(Alignment::Center);
        sheet_frame(
            "Settings",
            Message::Settings(false),
            self.settings_content(p),
            p,
        )
        .size(520.0, max_height)
        .footer(footer)
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_footer_mentions_saving_keys_only_while_they_show() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        assert_eq!(
            bench.settings_footer_text(),
            "Preferences apply automatically."
        );
        bench.video_analytics_input(video_analytics::Input::Keys(true));
        assert_eq!(
            bench.settings_footer_text(),
            "Preferences apply automatically. API keys require Save key."
        );
        // Both presentations build either way.
        for open in [true, false] {
            bench.video_analytics_input(video_analytics::Input::Keys(open));
            let _ = bench.settings_window(Palette::of(false));
            let _ = bench.settings_sheet(Palette::of(true), 600.0);
        }
    }

    #[test]
    fn the_keys_chevron_rests_where_it_was_left_at_launch() {
        let _motion = motion::test_reduce_motion(false);
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        bench.video_analytics.preferences.keys_open = true;
        bench.sync_animations();
        assert_eq!(
            bench.sheets.keys_turn.get(bench.now),
            1.0,
            "no turn on launch"
        );
        bench.settings_open = true;
        bench.video_analytics_input(video_analytics::Input::Keys(false));
        bench.sync_animations();
        assert!(
            bench.sheets.keys_turn.animating(bench.now),
            "turns while shown"
        );
    }

    #[test]
    fn the_keys_chevron_points_down_whenever_the_section_shows() {
        let _motion = motion::test_reduce_motion(false);
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        bench.sync_animations();
        assert_eq!(bench.keys_chevron(), 0.0);
        // An error opens it before any sync has turned the chevron.
        bench.video_analytics.credential_error =
            Some("The OS credential store could not save this key.");
        assert!(bench.video_analytics.keys_section_open());
        assert_eq!(bench.keys_chevron(), 1.0, "open, so pointing down");
        // A screenshot scene shows it open too.
        bench.video_analytics.credential_error = None;
        assert!(bench.scene_video_analytics("settings-credentials", true));
        assert_eq!(bench.keys_chevron(), 1.0);
        // And folding it, even when it was held open, folds it.
        bench.video_analytics_input(video_analytics::Input::Keys(false));
        assert!(!bench.video_analytics.keys_section_open());
        assert_eq!(bench.keys_chevron(), 0.0);
    }

    #[test]
    fn preference_window_uses_native_chrome_and_app_owned_close() {
        let settings = window_settings();
        assert_eq!(settings.size, Size::new(520.0, 500.0));
        assert!(settings.decorations && settings.closeable && settings.visible);
        assert!(!settings.resizable && !settings.minimizable);
        assert!(
            !settings.exit_on_close_request,
            "root routes close by window ID"
        );
        assert!(!settings.transparent && !settings.fullscreen && !settings.maximized);
        #[cfg(target_os = "macos")]
        {
            assert!(!settings.platform_specific.title_hidden);
            assert!(!settings.platform_specific.titlebar_transparent);
            assert!(!settings.platform_specific.fullsize_content_view);
        }
    }
}
