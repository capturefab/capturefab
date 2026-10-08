//! Every keyboard command. Bindings, dispatch, tooltips and the help sheet all
//! derive from the one `Action` table so they cannot drift apart.
use iced::keyboard::{self, key};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Mac,
    Windows,
    Nix,
}

impl Os {
    pub const CURRENT: Os = if cfg!(target_os = "macos") {
        Os::Mac
    } else if cfg!(target_os = "windows") {
        Os::Windows
    } else {
        Os::Nix
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// A lowercase letter, digit or symbol on the main keyboard.
    Char(char),
    F1,
    F5,
    F11,
    Enter,
    Escape,
    Space,
    Left,
    Right,
    PageUp,
    PageDown,
}

/// A key and its modifiers. `command` is ⌘ on macOS and Ctrl elsewhere;
/// `ctrl` is the macOS Control key (Ctrl elsewhere is `command`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Mods {
    pub command: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Mods {
    pub const NONE: Mods = Mods {
        command: false,
        ctrl: false,
        alt: false,
        shift: false,
    };
    const COMMAND: Mods = Mods {
        command: true,
        ..Mods::NONE
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    pub mods: Mods,
    pub key: Key,
}

impl Chord {
    /// Whether `pressed` triggers this chord. Modifiers match exactly, except
    /// that Shift is ignored for digits and symbols, which need it on some
    /// layouts (⌘+ is ⌘⇧= on US keyboards).
    pub fn matches(&self, pressed: &Chord) -> bool {
        let loose_shift = matches!(self.key, Key::Char(c) if !c.is_ascii_alphabetic());
        self.key == pressed.key
            && self.mods.command == pressed.mods.command
            && self.mods.ctrl == pressed.mods.ctrl
            && self.mods.alt == pressed.mods.alt
            && (loose_shift || self.mods.shift == pressed.mods.shift)
    }

    /// Plain keys (Space, Enter, Escape) belong to a focused field; only
    /// chords and function keys work while one has focus.
    pub fn needs_free_keyboard(&self) -> bool {
        self.mods == Mods::NONE && !matches!(self.key, Key::F1 | Key::F5 | Key::F11)
    }

    /// The chord's label in the platform's own notation, e.g. "⌥⌘0" or "Ctrl+Shift+A".
    pub fn label(&self, os: Os) -> String {
        let key = match self.key {
            Key::Char(c) => c.to_ascii_uppercase().to_string(),
            Key::F1 => "F1".into(),
            Key::F5 => "F5".into(),
            Key::F11 => "F11".into(),
            Key::Enter if os == Os::Mac => "↩".into(),
            Key::Enter => "Enter".into(),
            Key::Escape => "Esc".into(),
            Key::Space => "Space".into(),
            Key::Left if os == Os::Mac => "←".into(),
            Key::Left => "Left".into(),
            Key::Right if os == Os::Mac => "→".into(),
            Key::Right => "Right".into(),
            Key::PageUp => "PgUp".into(),
            Key::PageDown => "PgDn".into(),
        };
        let m = self.mods;
        if os == Os::Mac {
            // Apple's order: Control, Option, Shift, Command.
            let mut text = String::new();
            for (held, glyph) in [
                (m.ctrl, '⌃'),
                (m.alt, '⌥'),
                (m.shift, '⇧'),
                (m.command, '⌘'),
            ] {
                if held {
                    text.push(glyph);
                }
            }
            text + &key
        } else {
            let mut parts = Vec::new();
            for (held, name) in [(m.command, "Ctrl"), (m.alt, "Alt"), (m.shift, "Shift")] {
                if held {
                    parts.push(name);
                }
            }
            parts.push(&key);
            parts.join("+")
        }
    }
}

/// Translate an iced key press; None for keys no binding uses.
pub fn chord(
    key: &keyboard::Key,
    physical: key::Physical,
    modifiers: keyboard::Modifiers,
    os: Os,
) -> Option<Chord> {
    use key::Named;
    let key = match key {
        keyboard::Key::Named(named) => match named {
            Named::F1 => Key::F1,
            Named::F5 => Key::F5,
            Named::F11 => Key::F11,
            Named::Enter => Key::Enter,
            Named::Escape => Key::Escape,
            Named::Space => Key::Space,
            Named::ArrowLeft => Key::Left,
            Named::ArrowRight => Key::Right,
            Named::PageUp => Key::PageUp,
            Named::PageDown => Key::PageDown,
            _ => return None,
        },
        keyboard::Key::Character(text) => {
            let mut chars = text.chars();
            let logical = chars.next().filter(|_| chars.next().is_none());
            match logical {
                Some(' ') => Key::Space,
                // Option turns ⌥⌘0 into "º" on macOS: use the key's position then.
                Some(c) if c.is_ascii_graphic() => Key::Char(c.to_ascii_lowercase()),
                _ => Key::Char(physical_char(physical)?),
            }
        }
        keyboard::Key::Unidentified => return None,
    };
    let mods = if os == Os::Mac {
        Mods {
            command: modifiers.logo(),
            ctrl: modifiers.control(),
            alt: modifiers.alt(),
            shift: modifiers.shift(),
        }
    } else {
        Mods {
            command: modifiers.control(),
            ctrl: false,
            alt: modifiers.alt(),
            shift: modifiers.shift(),
        }
    };
    Some(Chord { mods, key })
}

fn physical_char(physical: key::Physical) -> Option<char> {
    use key::Code;
    let key::Physical::Code(code) = physical else {
        return None;
    };
    Some(match code {
        Code::Digit0 => '0',
        Code::Digit1 => '1',
        Code::Digit2 => '2',
        Code::Digit3 => '3',
        Code::Digit4 => '4',
        Code::Digit5 => '5',
        Code::Digit6 => '6',
        Code::Digit7 => '7',
        Code::Digit8 => '8',
        Code::Digit9 => '9',
        Code::Equal => '=',
        Code::Minus => '-',
        Code::Slash => '/',
        _ => {
            let name = format!("{code:?}");
            let letter = name.strip_prefix("Key")?;
            let mut chars = letter.chars();
            let c = chars.next().filter(|_| chars.next().is_none())?;
            c.to_ascii_lowercase()
        }
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Discover,
    ConnectAddress,
    NextCamera,
    PreviousCamera,
    FocusCamera,
    Overview,
    ToggleStream,
    Capture,
    ToggleAuto,
    SearchFeatures,
    FeaturesTab,
    CaptureTab,
    ForwardTab,
    ZoomIn,
    ZoomOut,
    ZoomFit,
    ZoomActual,
    ToggleActivity,
    ToggleSidebar,
    ToggleInspector,
    ImageMode,
    ToggleExposure,
    ToggleFocusRegion,
    CopySessionCommand,
    Fullscreen,
    Help,
    CloseWindow,
}

impl Action {
    pub const ALL: [Action; 27] = [
        Action::Discover,
        Action::ConnectAddress,
        Action::NextCamera,
        Action::PreviousCamera,
        Action::FocusCamera,
        Action::Overview,
        Action::ToggleStream,
        Action::Capture,
        Action::ToggleAuto,
        Action::SearchFeatures,
        Action::FeaturesTab,
        Action::CaptureTab,
        Action::ForwardTab,
        Action::ZoomIn,
        Action::ZoomOut,
        Action::ZoomFit,
        Action::ZoomActual,
        Action::ToggleActivity,
        Action::ToggleSidebar,
        Action::ToggleInspector,
        Action::ImageMode,
        Action::ToggleExposure,
        Action::ToggleFocusRegion,
        Action::CopySessionCommand,
        Action::Fullscreen,
        Action::Help,
        Action::CloseWindow,
    ];

    /// Help sheet layout: section title and its actions in reading order.
    pub const SECTIONS: [(&'static str, &'static [Action]); 5] = [
        (
            "Cameras",
            &[
                Action::Discover,
                Action::ConnectAddress,
                Action::NextCamera,
                Action::PreviousCamera,
                Action::FocusCamera,
                Action::Overview,
            ],
        ),
        (
            "Acquisition",
            &[
                Action::ToggleStream,
                Action::Capture,
                Action::ToggleAuto,
                Action::SearchFeatures,
            ],
        ),
        (
            "View",
            &[
                Action::ZoomIn,
                Action::ZoomOut,
                Action::ZoomFit,
                Action::ZoomActual,
                Action::ImageMode,
                Action::Fullscreen,
                Action::ToggleExposure,
                Action::ToggleFocusRegion,
            ],
        ),
        (
            "Panels",
            &[
                Action::FeaturesTab,
                Action::CaptureTab,
                Action::ForwardTab,
                Action::ToggleSidebar,
                Action::ToggleInspector,
                Action::ToggleActivity,
            ],
        ),
        (
            "Window",
            &[
                Action::CopySessionCommand,
                Action::Help,
                Action::CloseWindow,
            ],
        ),
    ];

    /// The action's name in the help sheet, under its section's title.
    /// Toggles are named by what they show.
    pub fn label(self) -> &'static str {
        match self {
            Action::Discover => "Discover cameras",
            Action::ConnectAddress => "Connect to an address or stream",
            Action::NextCamera => "Select next camera",
            Action::PreviousCamera => "Select previous camera",
            Action::FocusCamera => "Open selected camera",
            Action::Overview => "Back to all cameras",
            Action::ToggleStream => "Start or stop streaming",
            Action::Capture => "Capture and save",
            Action::ToggleAuto => "Switch between auto and manual",
            Action::SearchFeatures => "Search camera features",
            Action::FeaturesTab => "Features panel",
            Action::CaptureTab => "Capture panel",
            Action::ForwardTab => "Forward panel",
            Action::ZoomIn => "Zoom in",
            Action::ZoomOut => "Zoom out",
            Action::ZoomFit => "Zoom to fit",
            Action::ZoomActual => "Actual pixels (1:1)",
            Action::ToggleActivity => "Activity log",
            Action::ToggleSidebar => "Camera list",
            Action::ToggleInspector => "Camera settings",
            Action::ImageMode => "Image only",
            Action::ToggleExposure => "Exposure histogram",
            Action::ToggleFocusRegion => "Focus region and scores",
            Action::CopySessionCommand => "Copy session command",
            Action::Fullscreen => "Full screen",
            Action::Help => "This guide",
            Action::CloseWindow => "Close window",
        }
    }

    /// Key bindings for `os`, primary first. Command chords use ⌘ on macOS and Ctrl elsewhere,
    /// avoiding keys that the OS menu or text fields already own (⌘H, ⌘Q, Ctrl+W, Ctrl+Tab).
    pub fn bindings(self, os: Os) -> Vec<Chord> {
        let mac = os == Os::Mac;
        let with = |mods, key| Chord { mods, key };
        let cmd = |c| with(Mods::COMMAND, Key::Char(c));
        let bare = |key| with(Mods::NONE, key);
        let cmd_alt = Mods {
            alt: true,
            ..Mods::COMMAND
        };
        let cmd_shift = Mods {
            shift: true,
            ..Mods::COMMAND
        };
        match self {
            Action::Discover if mac => vec![cmd('r')],
            Action::Discover => vec![cmd('r'), bare(Key::F5)],
            Action::ConnectAddress => vec![cmd('l')],
            Action::NextCamera if mac => vec![with(cmd_alt, Key::Right)],
            Action::NextCamera => vec![with(Mods::COMMAND, Key::PageDown)],
            Action::PreviousCamera if mac => vec![with(cmd_alt, Key::Left)],
            Action::PreviousCamera => vec![with(Mods::COMMAND, Key::PageUp)],
            Action::FocusCamera => vec![bare(Key::Enter)],
            Action::Overview => vec![bare(Key::Escape)],
            Action::ToggleStream => vec![bare(Key::Space)],
            Action::Capture => vec![cmd('s')],
            Action::ToggleAuto => vec![with(cmd_shift, Key::Char('a'))],
            Action::SearchFeatures => vec![cmd('f')],
            Action::FeaturesTab => vec![cmd('1')],
            Action::CaptureTab => vec![cmd('2')],
            Action::ForwardTab => vec![cmd('3')],
            Action::ZoomIn => vec![cmd('='), cmd('+')],
            Action::ZoomOut => vec![cmd('-')],
            Action::ZoomFit => vec![cmd('0')],
            Action::ZoomActual => vec![with(cmd_alt, Key::Char('0'))],
            Action::ToggleActivity => vec![cmd('j')],
            Action::ToggleSidebar => vec![cmd('b')],
            Action::ToggleInspector => vec![cmd('i')],
            Action::ImageMode => vec![bare(Key::Char('f'))],
            Action::ToggleExposure => vec![bare(Key::Char('h'))],
            Action::ToggleFocusRegion => vec![bare(Key::Char('r'))],
            Action::CopySessionCommand => vec![with(cmd_shift, Key::Char('c'))],
            Action::Fullscreen if mac => vec![with(
                Mods {
                    ctrl: true,
                    ..Mods::COMMAND
                },
                Key::Char('f'),
            )],
            Action::Fullscreen => vec![bare(Key::F11)],
            Action::Help if mac => vec![cmd('?'), bare(Key::F1)],
            Action::Help => vec![bare(Key::F1), cmd('/')],
            Action::CloseWindow if mac => vec![cmd('w')],
            Action::CloseWindow if os == Os::Nix => vec![cmd('q')],
            // Windows closes with Alt+F4, which the OS handles.
            Action::CloseWindow => vec![],
        }
    }

    /// The action `pressed` triggers. `keyboard_free` is false while a field
    /// has focus or used the key itself.
    pub fn find(pressed: &Chord, keyboard_free: bool, os: Os) -> Option<Action> {
        Action::ALL.into_iter().find(|action| {
            action.bindings(os).iter().any(|binding| {
                (keyboard_free || !binding.needs_free_keyboard()) && binding.matches(pressed)
            })
        })
    }

    /// The one binding a list of shortcuts shows: the primary, except that
    /// zooming in reads ⌘+ on macOS, as Apple's menus have it.
    pub fn display_binding(self, os: Os) -> Option<Chord> {
        let bindings = self.bindings(os);
        let plus = (self == Action::ZoomIn && os == Os::Mac)
            .then(|| bindings.iter().find(|chord| chord.key == Key::Char('+')))
            .flatten();
        plus.or(bindings.first()).copied()
    }

    /// `display_binding`'s label, e.g. "⌘?"; empty with no binding.
    pub fn key_label(self, os: Os) -> String {
        self.display_binding(os)
            .map(|chord| chord.label(os))
            .unwrap_or_default()
    }

    /// All bindings, e.g. "⌘? / F1".
    pub fn shortcut(self, os: Os) -> String {
        self.bindings(os)
            .iter()
            .map(|chord| chord.label(os))
            .collect::<Vec<_>>()
            .join(" / ")
    }

    /// Tooltip text in the workbench's "description · shortcut" style.
    pub fn hint(self, text: &str, os: Os) -> String {
        match self.bindings(os).first() {
            Some(chord) => format!("{text} · {}", chord.label(os)),
            None => text.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLATFORMS: [Os; 3] = [Os::Mac, Os::Windows, Os::Nix];

    #[test]
    fn every_binding_dispatches_to_its_own_action() {
        for os in PLATFORMS {
            for action in Action::ALL {
                for chord in action.bindings(os) {
                    assert_eq!(
                        Action::find(&chord, true, os),
                        Some(action),
                        "{chord:?} {os:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn bindings_avoid_keys_owned_by_the_os_and_text_fields() {
        for os in PLATFORMS {
            // Text editing, the macOS app menu, and emacs-style deletions in fields elsewhere.
            let mut reserved: Vec<char> = vec!['a', 'c', 'v', 'x', 'z', 'y'];
            if os == Os::Mac {
                reserved.extend(['h', 'q']);
            } else {
                reserved.extend(['h', 'k', 'u', 'w']);
            }
            for action in Action::ALL {
                for chord in action.bindings(os) {
                    for c in &reserved {
                        let plain = Chord {
                            mods: Mods::COMMAND,
                            key: Key::Char(*c),
                        };
                        assert!(
                            chord != plain,
                            "{action:?} uses reserved {chord:?} on {os:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn help_sheet_lists_every_action_once() {
        let listed: Vec<Action> = Action::SECTIONS
            .iter()
            .flat_map(|(_, actions)| actions.iter().copied())
            .collect();
        assert_eq!(listed.len(), Action::ALL.len());
        for action in Action::ALL {
            assert_eq!(
                listed.iter().filter(|&&a| a == action).count(),
                1,
                "{action:?}"
            );
        }
        for os in PLATFORMS {
            for action in Action::ALL {
                // Only Windows leaves window closing to the OS (Alt+F4).
                assert_eq!(
                    action.bindings(os).is_empty(),
                    action == Action::CloseWindow && os == Os::Windows,
                    "{action:?} on {os:?}"
                );
            }
        }
    }

    #[test]
    fn shortcut_labels_follow_the_platform() {
        assert_eq!(Action::Capture.shortcut(Os::Windows), "Ctrl+S");
        assert_eq!(Action::Discover.shortcut(Os::Windows), "Ctrl+R / F5");
        assert_eq!(Action::Fullscreen.shortcut(Os::Windows), "F11");
        assert_eq!(Action::ToggleAuto.shortcut(Os::Nix), "Ctrl+Shift+A");
        assert_eq!(
            Action::ToggleStream.hint("Start or stop acquisition", Os::Windows),
            "Start or stop acquisition · Space"
        );
        assert_eq!(Action::Discover.shortcut(Os::Mac), "⌘R");
        assert_eq!(Action::ZoomActual.shortcut(Os::Mac), "⌥⌘0");
        assert_eq!(Action::Fullscreen.shortcut(Os::Mac), "⌃⌘F");
        assert_eq!(Action::ToggleAuto.shortcut(Os::Mac), "⇧⌘A");
        assert_eq!(Action::Help.shortcut(Os::Mac), "⌘? / F1");
    }

    #[test]
    fn the_help_sheet_shows_one_working_key_per_action() {
        assert_eq!(Action::ZoomIn.key_label(Os::Mac), "⌘+");
        assert_eq!(Action::ZoomIn.key_label(Os::Windows), "Ctrl+=");
        assert_eq!(Action::Help.key_label(Os::Mac), "⌘?");
        assert_eq!(Action::Help.key_label(Os::Windows), "F1");
        assert_eq!(Action::Discover.key_label(Os::Windows), "Ctrl+R");
        assert_eq!(Action::CloseWindow.key_label(Os::Windows), "");
        for os in PLATFORMS {
            for action in Action::ALL {
                if let Some(chord) = action.display_binding(os) {
                    assert_eq!(Action::find(&chord, true, os), Some(action), "{action:?}");
                }
                assert!(!action.key_label(os).contains(" / "), "{action:?}");
            }
        }
    }

    #[test]
    fn plain_keys_wait_for_free_keyboard() {
        let os = Os::Windows;
        let press = |mods, key| Chord { mods, key };
        assert_eq!(
            Action::find(&press(Mods::NONE, Key::Space), true, os),
            Some(Action::ToggleStream)
        );
        assert_eq!(
            Action::find(&press(Mods::NONE, Key::Space), false, os),
            None
        );
        assert_eq!(
            Action::find(&press(Mods::NONE, Key::F1), false, os),
            Some(Action::Help)
        );
        assert_eq!(
            Action::find(&press(Mods::COMMAND, Key::Char('s')), false, os),
            Some(Action::Capture)
        );
    }

    #[test]
    fn iced_key_presses_become_chords() {
        use keyboard::{Key as K, Modifiers as M, key::Code, key::Physical};
        let code = Physical::Code;
        // ⌥⌘0 on macOS arrives as "º"; the key position recovers the digit.
        let actual = chord(
            &K::Character("º".into()),
            code(Code::Digit0),
            M::LOGO | M::ALT,
            Os::Mac,
        )
        .unwrap();
        assert_eq!(
            Action::find(&actual, true, Os::Mac),
            Some(Action::ZoomActual)
        );
        // Ctrl+Shift+= reads "+" and still zooms in.
        let zoom = chord(
            &K::Character("+".into()),
            code(Code::Equal),
            M::CTRL | M::SHIFT,
            Os::Windows,
        )
        .unwrap();
        assert_eq!(Action::find(&zoom, true, Os::Windows), Some(Action::ZoomIn));
        // Capital letters from Shift still compare as letters, with Shift kept.
        let copy = chord(
            &K::Character("C".into()),
            code(Code::KeyC),
            M::CTRL | M::SHIFT,
            Os::Nix,
        )
        .unwrap();
        assert_eq!(
            Action::find(&copy, true, Os::Nix),
            Some(Action::CopySessionCommand)
        );
        // Plain Ctrl+C belongs to text fields.
        let plain = chord(
            &K::Character("c".into()),
            code(Code::KeyC),
            M::CTRL,
            Os::Nix,
        )
        .unwrap();
        assert_eq!(Action::find(&plain, true, Os::Nix), None);
        // Dvorak: the logical letter wins over the key position.
        let dvorak = chord(
            &K::Character("s".into()),
            code(Code::Semicolon),
            M::CTRL,
            Os::Nix,
        );
        assert_eq!(
            Action::find(&dvorak.unwrap(), true, Os::Nix),
            Some(Action::Capture)
        );
    }
}
