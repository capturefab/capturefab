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
        ]
        .spacing(9);
        column![
            appearance,
            rule::horizontal(1).style(style::line),
            interface,
            rule::horizontal(1).style(style::line),
            discovery
        ]
        .spacing(20)
        .into()
    }

    pub(super) fn settings_window(&self, p: &'static Palette) -> Element<'_, Message> {
        container(
            column![
                scrollable(self.settings_content(p))
                    .height(Fill)
                    .style(style::scroll),
                text("Changes apply automatically.")
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
            text("Changes apply automatically.")
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
