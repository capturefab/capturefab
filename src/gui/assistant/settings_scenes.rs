//! Provider setup fixtures are display-only, including their credential status.
//! They replace the settings model before the section can start a worker.
use super::*;

impl Workbench {
    pub(in crate::gui) fn scene_assistant_settings(&mut self, word: &str, _late: bool) -> bool {
        match word {
            "settings-assistant" | "settings-assistant-error" => {
                self.settings_open = true;
                self.assistant.settings = settings::State::scene(word.ends_with("-error"));
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_settings_scenes_stay_display_only_at_both_scene_passes() {
        for word in ["settings-assistant", "settings-assistant-error"] {
            let mut app = Workbench::new(
                SessionHandle::new(),
                "assistant-settings-scene".into(),
                true,
                None,
            );
            for late in [false, true] {
                assert!(app.scene_assistant_settings(word, late));
                assert!(app.settings_open);
                assert!(app.assistant.settings.open());
                assert!(!app.assistant.settings.busy());
                assert!(app.handle.snapshot().cameras.is_empty());
            }
        }
    }
}
