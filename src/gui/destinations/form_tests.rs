//! Frozen saves retain the complete readable form while refusing mutation.
use super::*;
use crate::gui::accessibility::{Node, NodeId, Role, mapped, snapshot_widget};
use iced::Size;

fn editor(source: Option<Source>) -> Editor {
    let mut editor = match source {
        None => Editor::new_folder("line-1".into(), std::env::temp_dir().join("captures")),
        Some(source) => {
            let mut editor = Editor::new_bucket();
            editor.source = source;
            editor.provider = 4;
            editor.endpoint = "https://storage.example.com".into();
            editor.bucket = "line-1".into();
            editor.access_key_id = "AKIA".into();
            editor.secret = "test-secret".into();
            editor.profile = "camera-profile".into();
            editor.prefix = "cameras/line-1/".into();
            editor.advanced = true;
            editor
        }
    };
    editor.name = "line-1".into();
    editor.name_touched = true;
    editor
}
use iced::advanced::{
    Layout, layout,
    renderer::Headless,
    widget::{Operation, Tree, operation},
};

fn controls(nodes: Vec<(NodeId, Node)>) -> BTreeMap<NodeId, Node> {
    nodes
        .into_iter()
        .filter(|(_, node)| {
            matches!(
                node.role(),
                Role::Button
                    | Role::CheckBox
                    | Role::ComboBox
                    | Role::TextInput
                    | Role::PasswordInput
            )
        })
        .collect()
}

fn inspect(
    picker: &Picker,
    tree: &mut Tree,
    renderer: &iced::Renderer,
) -> (BTreeMap<NodeId, Node>, operation::focusable::Count) {
    let mut view = mapped(
        picker.manager(Palette::of(false), 640.0, 0, Instant::now(), None, None),
        crate::gui::Message::CapturePicker,
    );
    tree.diff(view.as_widget());
    let node = view.as_widget_mut().layout(
        tree,
        renderer,
        &layout::Limits::new(Size::ZERO, Size::new(560.0, 640.0)),
    );
    let (snapshot, _, _) =
        snapshot_widget(view.as_widget_mut(), tree, Layout::new(&node), renderer);
    let mut count = operation::focusable::count();
    view.as_widget_mut().operate(
        tree,
        Layout::new(&node),
        renderer,
        &mut operation::black_box(&mut count),
    );
    let operation::Outcome::Some(count) = count.finish() else {
        panic!("the real form exposes its traversal count");
    };
    (controls(snapshot.nodes), count)
}

#[test]
fn saving_freezes_all_destination_edits_but_keeps_their_values_readable() {
    let renderer = iced::futures::executor::block_on(iced::Renderer::new(
        iced::Font::default(),
        iced::Pixels(16.0),
        Some("tiny-skia"),
    ))
    .expect("the compiled software renderer is available");
    for source in [
        None,
        Some(Source::Keychain),
        Some(Source::Profile),
        Some(Source::Environment),
    ] {
        let mut picker = Picker::new(false, false);
        picker.manager_open = true;
        picker.editor = Some(editor(source));
        let mut tree = Tree::empty();
        let (before, before_focus) = inspect(&picker, &mut tree, &renderer);
        assert!(before.values().any(|node| node.role() == Role::TextInput));
        assert!(before_focus.total > 0);
        let save = before
            .values()
            .find(|node| node.label() == Some("Save"))
            .unwrap()
            .bounds()
            .unwrap();
        let cancel = before
            .values()
            .find(|node| node.label() == Some("Cancel"))
            .unwrap()
            .bounds()
            .unwrap();
        if cfg!(target_os = "windows") {
            assert!(save.x1 < cancel.x0, "Windows puts Save before Cancel");
        } else {
            assert!(
                cancel.x1 < save.x0,
                "macOS and GNOME put Cancel before Save"
            );
        }

        let (_sender, receiver) = channel();
        let editor = picker.editor.as_mut().unwrap();
        editor.saving = Some((editor.destination().unwrap(), receiver));
        let (pending, pending_focus) = inspect(&picker, &mut tree, &renderer);
        assert_eq!(pending.len(), before.len(), "all controls remain visible");
        for (id, node) in &pending {
            let previous = &before[id];
            assert_eq!(node.label(), previous.label());
            assert_eq!(
                node.value(),
                previous.value(),
                "the frozen draft remains readable"
            );
            assert_eq!(node.toggled(), previous.toggled());
            assert!(
                node.is_disabled(),
                "{} is unavailable during save",
                node.label().unwrap()
            );
            for action in [
                accesskit::Action::Click,
                accesskit::Action::Focus,
                accesskit::Action::SetValue,
                accesskit::Action::ReplaceSelectedText,
                accesskit::Action::SetTextSelection,
                accesskit::Action::Expand,
            ] {
                assert!(
                    !node.supports_action(action),
                    "a disabled control offers no {action:?}"
                );
            }
        }
        assert_eq!(
            pending_focus.total,
            usize::from(source.is_some()),
            "only S3's Advanced disclosure remains traversable for reading"
        );
        if source.is_some() {
            // This is a permitted local navigation action, even during save.
            let mut output = String::new();
            picker.update(Message::Advanced(false), &mut output);
            assert!(!picker.editor.as_ref().unwrap().advanced);
            picker.update(Message::Advanced(true), &mut output);
        }

        picker.editor.as_mut().unwrap().saving = None;
        let (after, after_focus) = inspect(&picker, &mut tree, &renderer);
        assert_eq!(after.len(), before.len());
        assert_eq!(after_focus.total, before_focus.total);
        for (id, node) in after {
            assert_eq!(node.is_disabled(), before[&id].is_disabled());
            assert_eq!(node.value(), before[&id].value());
        }
    }
}

#[test]
fn return_from_each_actual_single_line_field_requests_save_once() {
    use crate::gui::{Message as GuiMessage, accessibility::Control};
    use iced::advanced::{Shell, widget::Id};
    use iced::{Event, Rectangle, keyboard, mouse};
    use std::any::Any;
    #[derive(Default)]
    struct Fields(Vec<Id>);
    impl Operation for Fields {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }
        fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn Any) {
            if let Some(control) = state.downcast_ref::<Control<GuiMessage>>()
                && matches!(control.node.role(), Role::TextInput | Role::PasswordInput)
                && !control.node.is_disabled()
            {
                self.0.push(control.focus_id.clone());
            }
        }
    }
    let renderer = iced::futures::executor::block_on(iced::Renderer::new(
        iced::Font::default(),
        iced::Pixels(16.0),
        Some("tiny-skia"),
    ))
    .unwrap();
    for source in [
        None,
        Some(Source::Keychain),
        Some(Source::Profile),
        Some(Source::Environment),
    ] {
        let mut picker = Picker::new(false, false);
        picker.editor = Some(editor(source));
        picker.manager_open = true;
        let mut view = mapped(
            picker.manager(Palette::of(false), 640.0, 0, Instant::now(), None, None),
            GuiMessage::CapturePicker,
        );
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(560.0, 640.0)),
        );
        let mut fields = Fields::default();
        view.as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut fields);
        assert!(!fields.0.is_empty());
        for id in fields.0 {
            view.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut operation::focusable::focus::<()>(id),
            );
            for repeat in [false, true] {
                let enter = Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Enter),
                    modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
                    physical_key: keyboard::key::Physical::Unidentified(
                        keyboard::key::NativeCode::Unidentified,
                    ),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(),
                    text: None,
                    repeat,
                });
                let mut messages = Vec::new();
                view.as_widget_mut().update(
                    &mut tree,
                    &enter,
                    Layout::new(&node),
                    mouse::Cursor::Unavailable,
                    &renderer,
                    &mut iced::advanced::clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &Rectangle::with_size(node.size()),
                );
                if repeat {
                    assert!(messages.is_empty());
                } else {
                    assert!(matches!(
                        messages.as_slice(),
                        [GuiMessage::CapturePicker(Message::Save)]
                    ));
                }
            }
        }
    }
}

/// Every node of `view` laid out `width` px wide, with the renderer the
/// GUI uses.
fn nodes_of(mut view: Element<'_, crate::gui::Message>, width: f32) -> Vec<(NodeId, Node)> {
    let renderer = super::layout_tests::renderer();
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(width, 2000.0)),
    );
    let (snapshot, _, _) = snapshot_widget(
        view.as_widget_mut(),
        &mut tree,
        Layout::new(&node),
        &renderer,
    );
    snapshot.nodes
}

/// The capture panel's part of the picker, as the inspector shows it.
fn capture_nodes(picker: &Picker, output: &str) -> Vec<(NodeId, Node)> {
    let error = crate::destination::validate_output(picker.selected.as_deref(), output)
        .and_then(|_| crate::engine::validate_capture_output(output))
        .err()
        .map(|error| error.to_string());
    nodes_of(
        mapped(
            picker.view(output, "Output path", Palette::of(false), error),
            crate::gui::Message::CapturePicker,
        ),
        320.0,
    )
}

fn has(nodes: &[(NodeId, Node)], role: Role, label: &str) -> bool {
    nodes
        .iter()
        .any(|(_, node)| node.role() == role && node.label() == Some(label))
}

fn shows_text(nodes: &[(NodeId, Node)], text: &str) -> usize {
    nodes
        .iter()
        .filter(|(_, node)| node.label() == Some(text) || node.value() == Some(text))
        .count()
}

#[test]
fn the_default_capture_output_shows_a_folder_and_a_file_name_choice() {
    let picker = Picker::new(false, false);
    let output = crate::gui::prefs::default_capture_output();
    let nodes = capture_nodes(&picker, &output);
    assert!(has(&nodes, Role::ComboBox, "File names"));
    assert!(shows_text(&nodes, "Folder") > 0);
    assert!(
        !has(&nodes, Role::TextInput, "Output path"),
        "the template hides behind Custom pattern…"
    );
    assert!(
        nodes.iter().any(|(_, node)| {
            node.description() == Some("By date, camera and time (recommended)")
                || node.value() == Some("By date, camera and time (recommended)")
        }) || shows_text(&nodes, "By date, camera and time (recommended)") > 0
    );
    // No pattern line while no template shows.
    assert_eq!(shows_text(&nodes, "Pattern"), 0);
}

/// How tall the capture panel's part of the picker lays out.
fn capture_height(picker: &Picker, output: &str) -> f32 {
    let renderer = super::layout_tests::renderer();
    let mut view = mapped(
        picker.view(output, "Output path", Palette::of(false), None),
        crate::gui::Message::CapturePicker,
    );
    let mut tree = Tree::new(view.as_widget());
    view.as_widget_mut()
        .layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(320.0, 2000.0)),
        )
        .size()
        .height
}

#[test]
fn choosing_custom_pattern_moves_nothing_below_and_shows_what_a_template_holds() {
    let mut picker = Picker::new(false, false);
    let mut output = crate::gui::prefs::default_capture_output();
    let simple = capture_height(&picker, &output);
    picker.update(Message::Naming(Naming::Custom), &mut output);
    assert_eq!(capture_height(&picker, &output), simple);
    let nodes = capture_nodes(&picker, &output);
    assert!(has(&nodes, Role::TextInput, "Output path"));
    // The template has a visible label in place of the folder's.
    assert_eq!(shows_text(&nodes, "Pattern"), 1);
    assert_eq!(shows_text(&nodes, "Folder"), 0);
    // The field writes the home folder as `~`; its description and what
    // it sends are the full path.
    let field = nodes
        .iter()
        .find(|(_, node)| node.role() == Role::TextInput)
        .map(|(_, node)| node)
        .unwrap();
    assert_eq!(field.description(), Some(output.as_str()));
    assert_eq!(home_long(field.value().unwrap()), output);
}

#[test]
fn the_output_field_writes_home_as_a_tilde_both_ways_exactly() {
    let home = Path::new("/Users/ada");
    let sep = std::path::MAIN_SEPARATOR;
    for value in [
        "/Users/ada".to_string(),
        format!("/Users/ada{sep}"),
        format!("/Users/ada{sep}Capturefab{sep}{{date}}{sep}"),
        format!("/Users/ada{sep}{sep}x.png"),
        "/Users/adam/x.png".into(),
        "relative/x.png".into(),
        "srt://host:9000".into(),
        String::new(),
    ] {
        let short = shorten_home(&value, Some(home));
        assert_eq!(expand_tilde(&short, Some(home)), value, "{value}");
    }
    assert_eq!(
        shorten_home(&format!("/Users/ada{sep}x.png"), Some(home)),
        format!("~{sep}x.png")
    );
    assert_eq!(shorten_home("/Users/adam/x.png", Some(home)), "/Users/adam/x.png");
    assert_eq!(expand_tilde("~x", Some(home)), "~x");
    assert_eq!(expand_tilde("~/x", None), "~/x");
}

#[test]
fn back_on_this_computer_a_destination_name_returns_below_the_capture_folder() {
    let mut picker = Picker::new(false, false);
    picker.saved = vec![Destination {
        name: "archive".into(),
        target: Target::Folder {
            path: std::env::temp_dir().join("archive"),
        },
    }];
    let mut output = crate::gui::prefs::default_capture_output();
    let choose = |name: Option<&str>| {
        Message::Select(Choice {
            name: name.map(Into::into),
            label: name.unwrap_or("This computer").into(),
        })
    };
    picker.update(choose(Some("archive")), &mut output);
    assert!(Path::new(&output).is_relative());
    picker.update(choose(None), &mut output);
    assert_eq!(output, crate::gui::prefs::default_capture_output());
    let nodes = capture_nodes(&picker, &output);
    assert!(shows_text(&nodes, "Folder") > 0);
    assert!(!has(&nodes, Role::TextInput, "Output path"));
    // A preset chosen for a relative output left behind (as before this
    // fix, or from a remembered session) also lands in the capture folder.
    let mut output = "{date}/{camera}-{datetime}-{seq}.png".to_string();
    picker.update(picker.naming_message(&output, Naming::CameraTime), &mut output);
    assert_eq!(
        output,
        naming_presets::join(
            Some(&crate::gui::prefs::capture_root()),
            Naming::CameraTime,
            "png"
        )
    );
    let nodes = capture_nodes(&picker, &output);
    assert!(shows_text(&nodes, "Folder") > 0);
    assert!(!has(&nodes, Role::TextInput, "Output path"));
    // A relative path typed on this computer stays as typed.
    picker.update(Message::Naming(Naming::Custom), &mut output);
    picker.update(Message::Output("shots/x.png".into()), &mut output);
    assert_eq!(output, "shots/x.png");
}

#[test]
fn a_subfolder_in_a_destination_shows_its_name_field() {
    let mut picker = Picker::new(false, false);
    picker.saved = vec![Destination {
        name: "archive".into(),
        target: Target::Folder {
            path: std::env::temp_dir().join("archive"),
        },
    }];
    let mut output = naming_presets::join(
        Some(&crate::gui::prefs::capture_root().join("line1")),
        Naming::DatedCameraTime,
        "png",
    );
    picker.select(Some("archive".into()), &mut output);
    assert_eq!(
        naming_presets::split(&output, "png"),
        (Some("line1".into()), Naming::DatedCameraTime)
    );
    let nodes = capture_nodes(&picker, &output);
    assert!(has(&nodes, Role::TextInput, "Capture name in destination"));
    // The choice still names the preset it is.
    assert!(
        nodes.iter().any(|(_, node)| node.role() == Role::ComboBox
            && node.label() == Some("File names")
            && (node.value() == Some("By date, camera and time (recommended)")
                || node.description() == Some("By date, camera and time (recommended)")))
            || shows_text(&nodes, "By date, camera and time (recommended)") > 0
    );
}

#[test]
fn a_custom_or_refused_output_shows_its_field() {
    let mut picker = Picker::new(false, false);
    let mut output = crate::gui::prefs::default_capture_output();
    // Chosen: the same template shows, for editing.
    picker.update(Message::Naming(Naming::Custom), &mut output);
    assert_eq!(output, crate::gui::prefs::default_capture_output());
    let nodes = capture_nodes(&picker, &output);
    assert!(has(&nodes, Role::TextInput, "Output path"));
    assert!(has(&nodes, Role::ComboBox, "File names"));
    // A preset hides it again.
    picker.update(Message::Naming(Naming::DatedCameraTime), &mut output);
    assert!(!has(
        &capture_nodes(&picker, &output),
        Role::TextInput,
        "Output path"
    ));
    // Hand-edited, or refused, it shows without being asked for.
    let picker = Picker::new(false, false);
    let edited = std::env::temp_dir().join("shots").join("{frame}.png");
    assert!(has(
        &capture_nodes(&picker, &edited.to_string_lossy()),
        Role::TextInput,
        "Output path"
    ));
    let refused = std::env::temp_dir()
        .join("{nope}")
        .join("capture-{seq}.png");
    let nodes = capture_nodes(&picker, &refused.to_string_lossy());
    assert!(has(&nodes, Role::TextInput, "Output path"));
}

#[test]
fn a_named_destination_offers_file_names_in_place_of_its_name_field() {
    let mut picker = Picker::new(false, false);
    picker.saved = vec![Destination {
        name: "archive".into(),
        target: Target::Folder {
            path: std::env::temp_dir().join("archive"),
        },
    }];
    let mut output = crate::gui::prefs::default_capture_output();
    picker.select(Some("archive".into()), &mut output);
    let nodes = capture_nodes(&picker, &output);
    assert!(has(&nodes, Role::ComboBox, "File names"));
    assert!(!has(&nodes, Role::TextInput, "Capture name in destination"));
    picker.update(Message::Naming(Naming::Numbered), &mut output);
    assert_eq!(Path::new(&output), Path::new("capture-{seq}.png"));
    picker.update(Message::Naming(Naming::Custom), &mut output);
    assert!(has(
        &capture_nodes(&picker, &output),
        Role::TextInput,
        "Capture name in destination"
    ));
}

#[test]
fn choosing_a_preset_keeps_the_folder_and_extension_and_typing_keeps_the_field() {
    let mut picker = Picker::new(false, false);
    let folder = std::env::temp_dir().join("shots");
    let mut output = naming_presets::join(Some(&folder), Naming::DatedCameraTime, "jpeg");
    // The pick list sends the new output, so the app's undo records it.
    let message = picker.naming_message(&output, Naming::Numbered);
    assert!(matches!(&message, Message::Output(_)));
    picker.update(message, &mut output);
    assert_eq!(
        output,
        naming_presets::join(Some(&folder), Naming::Numbered, "jpeg")
    );
    assert!(!picker.custom_naming);
    // Typing in the field never hides it, even into a preset's shape.
    picker.update(Message::Naming(Naming::Custom), &mut output);
    let typed = format!("{output}x");
    picker.update(Message::Output(typed), &mut output);
    picker.update(
        Message::Output(naming_presets::join(
            Some(&folder),
            Naming::Numbered,
            "jpeg",
        )),
        &mut output,
    );
    assert!(picker.custom_naming, "one keystroke back is still typing");
    // A hand-edited template keeps its folder when a preset is chosen.
    let mut output = folder.join("frame.png").to_string_lossy().into_owned();
    picker.update(
        picker.naming_message(&output, Naming::CameraTime),
        &mut output,
    );
    assert_eq!(
        output,
        naming_presets::join(Some(&folder), Naming::CameraTime, "png")
    );
    assert!(!picker.custom_naming);
}

#[test]
fn choosing_a_folder_keeps_the_preset() {
    let picker = Picker::new(false, false);
    let output = crate::gui::prefs::default_capture_output();
    let folder = std::env::temp_dir().join("elsewhere");
    assert_eq!(
        picker.browsed(&output, &folder),
        naming_presets::join(Some(&folder), Naming::DatedCameraTime, "png")
    );
    let custom = std::env::temp_dir().join("{frame}.png");
    assert_eq!(
        Path::new(&picker.browsed(&custom.to_string_lossy(), &folder)),
        folder.join("{frame}.png")
    );
}

#[test]
fn undo_and_redo_restore_a_file_name_choice() {
    use crate::gui::{Message as AppMessage, Workbench};
    let mut app = Workbench::new(
        crate::session::SessionHandle::new(),
        "naming-history-test".into(),
        true,
        None,
    );
    app.capture_to = Picker::new(false, false);
    let before = crate::gui::prefs::default_capture_output();
    app.output = before.clone();
    let message = app.capture_to.naming_message(&app.output, Naming::Numbered);
    let _ = app.update(AppMessage::CapturePicker(message));
    let numbered = naming_presets::join(
        Some(&crate::gui::prefs::capture_root()),
        Naming::Numbered,
        "png",
    );
    assert_eq!(app.output, numbered);
    let _ = app.update(AppMessage::Undo);
    assert_eq!(app.output, before);
    let _ = app.update(AppMessage::Redo);
    assert_eq!(app.output, numbered);
}

#[test]
fn drive_shortcuts_say_what_they_do_and_mark_the_drive_in_use() {
    let mut picker = Picker::new(false, false);
    super::layout_tests::add_external_drive(&mut picker);
    let output = crate::gui::prefs::default_capture_output();
    let nodes = capture_nodes(&picker, &output);
    assert_eq!(shows_text(&nodes, "Or save to a drive"), 1);
    let chip = |nodes: &[(NodeId, Node)]| {
        nodes
            .iter()
            .find(|(_, node)| {
                node.role() == Role::Button
                    && node
                        .label()
                        .is_some_and(|label| label.starts_with("Save to a Capturefab folder"))
            })
            .map(|(_, node)| node.is_selected())
            .expect("one chip per drive")
    };
    assert_ne!(chip(&nodes), Some(true));
    // Saved to the drive (as UseVolume(0) does), its chip shows selected.
    let drive = picker.volumes[0].path.join("Capturefab");
    picker.saved = vec![Destination {
        name: "Laboratory-archive".into(),
        target: Target::Folder { path: drive },
    }];
    picker.selected = Some("Laboratory-archive".into());
    let mut output = output;
    picker.update(Message::UseVolume(0), &mut output);
    assert_eq!(picker.selected.as_deref(), Some("Laboratory-archive"));
    assert_eq!(chip(&capture_nodes(&picker, &output)), Some(true));
}

#[test]
fn destination_rows_summarize_in_plain_words() {
    let s3 = |endpoint: &str, keep_local: bool| Destination {
        name: "b".into(),
        target: Target::S3(Bucket {
            endpoint: endpoint.into(),
            region: "auto".into(),
            bucket: "line-1".into(),
            prefix: "capturefab/".into(),
            path_style: false,
            credentials: Credentials::Environment,
            keep_local,
            no_overwrite: NoOverwrite::IfNoneMatch,
        }),
    };
    assert_eq!(
        row_summary(&s3("", false)),
        "Amazon S3 · line-1/capturefab/"
    );
    assert_eq!(
        row_summary(&s3("https://abc.r2.cloudflarestorage.com", true)),
        "Cloudflare R2 · line-1/capturefab/ · keeps local copies"
    );
    assert_eq!(
        row_summary(&s3("https://s3.us-west-004.backblazeb2.com", false)),
        "Backblaze B2 · line-1/capturefab/"
    );
    assert_eq!(
        row_summary(&s3("https://storage.example.com", false)),
        "Other S3-compatible · line-1/capturefab/"
    );
    let home = Path::new("/Users/ada");
    assert_eq!(tilde(&home.join("Captures"), Some(home)), "~/Captures");
    assert_eq!(tilde(home, Some(home)), "~");
    assert_eq!(tilde(Path::new("/Volumes/A"), Some(home)), "/Volumes/A");
    let ftp = Destination {
        name: "line1".into(),
        target: Target::Ftp(destination::FtpServer {
            host: "10.0.0.5".into(),
            port: 21,
            user: "cam".into(),
            dir: "incoming".into(),
            tls: destination::FtpTls::Explicit,
            password: destination::SecretSource::Environment,
            keep_local: false,
        }),
    };
    assert_eq!(row_summary(&ftp), ftp.describe());
}

#[test]
fn credentials_and_r2_regions_wait_under_advanced_unless_customised() {
    let bucket = |change: fn(&mut Editor)| {
        let mut editor = editor(Some(Source::Keychain));
        editor.prefix = DEFAULT_PREFIX.into();
        editor.provider = 0;
        editor.endpoint = String::new();
        editor.path_style = false;
        editor.advanced = false;
        change(&mut editor);
        editor
    };
    assert!(!bucket(|_| {}).custom_advanced());
    assert!(bucket(|e| e.source = Source::Profile).custom_advanced());
    assert!(bucket(|e| e.source = Source::Environment).custom_advanced());
    let r2 = |e: &mut Editor| {
        e.provider = 1;
        e.region = "auto".into();
        e.path_style = true;
    };
    assert!(!bucket(r2).custom_advanced());
    assert!(
        bucket(|e| {
            e.provider = 1;
            e.path_style = true;
            e.region = "eu".into();
        })
        .custom_advanced()
    );
    // Amazon's region is in the main column, so it opens nothing.
    assert!(!bucket(|e| e.region = "eu-west-1".into()).custom_advanced());
    // A refused field under Advanced opens it.
    let mut profile = bucket(|e| {
        e.source = Source::Profile;
        e.profile = String::new();
    });
    profile.advanced = false;
    profile.refuse(profile.destination().unwrap_err());
    assert!(profile.advanced);
    let mut region = bucket(|e| {
        e.provider = 1;
        e.region = String::new();
    });
    region.refuse(region.destination().unwrap_err());
    assert!(region.advanced);
    let mut amazon = bucket(|e| e.region = String::new());
    amazon.refuse(amazon.destination().unwrap_err());
    assert!(!amazon.advanced);
    let summary = |change| advanced_summary(&bucket(change));
    assert_eq!(summary(|_| {}), "Folder capturefab/");
    assert_eq!(summary(|e| e.prefix = String::new()), "No folder");
    assert_eq!(
        summary(|e| e.source = Source::Profile),
        "Folder capturefab/ · AWS profile camera-profile"
    );
    assert_eq!(
        summary(|e| e.source = Source::Environment),
        "Folder capturefab/ · environment credentials"
    );
    assert_eq!(
        summary(|e| {
            e.provider = 1;
            e.path_style = true;
            e.region = "eu".into();
        }),
        "Folder capturefab/ · path-style · region eu"
    );
}

#[test]
fn the_main_column_holds_the_bucket_essentials_in_order() {
    let fields = |change: fn(&mut Editor)| {
        let mut editor = Editor::new_bucket();
        change(&mut editor);
        let mut picker = Picker::new(false, false);
        picker.manager_open = true;
        picker.editor = Some(editor);
        nodes_of(
            mapped(
                picker.manager(Palette::of(false), 2000.0, 0, Instant::now(), None, None),
                crate::gui::Message::CapturePicker,
            ),
            560.0,
        )
        .into_iter()
        .filter(|(_, node)| {
            matches!(
                node.role(),
                Role::ComboBox | Role::TextInput | Role::PasswordInput
            )
        })
        .filter_map(|(_, node)| node.label().map(str::to_owned))
        .collect::<Vec<_>>()
    };
    assert_eq!(
        fields(|_| {}),
        [
            "S3-compatible service",
            "S3 bucket",
            "S3 region",
            "S3 access key ID",
            "S3 secret access key",
            "Destination name"
        ]
    );
    assert_eq!(
        fields(|e| {
            e.provider = 1;
            e.endpoint = PROVIDERS[1].1.into();
            e.region = "auto".into();
            e.path_style = true;
        }),
        [
            "S3-compatible service",
            "S3 endpoint",
            "S3 bucket",
            "S3 access key ID",
            "S3 secret access key",
            "Destination name"
        ]
    );
    let advanced = fields(|e| {
        e.provider = 1;
        e.source = Source::Profile;
        e.advanced = true;
    });
    assert_eq!(
        advanced[advanced.len() - 4..],
        [
            "S3 key prefix",
            "S3 region",
            "Credentials source",
            "AWS profile"
        ]
    );
    assert!(!advanced.contains(&"S3 access key ID".to_string()));
}

#[test]
fn an_editor_without_history_offers_no_undo_stops() {
    let nodes = nodes_of(
        mapped(
            history_controls(None, None, Palette::of(false)),
            crate::gui::Message::CapturePicker,
        ),
        560.0,
    );
    assert!(!nodes.iter().any(|(_, node)| node.role() == Role::Button));
    // Once edited, the footer offers Undo before Test.
    let mut picker = Picker::new(false, false);
    let mut output = String::new();
    picker.update(Message::AddBucket, &mut output);
    picker.update(Message::Bucket("line-1".into()), &mut output);
    let nodes = nodes_of(
        mapped(
            picker.manager(Palette::of(false), 2000.0, 0, Instant::now(), None, None),
            crate::gui::Message::CapturePicker,
        ),
        560.0,
    );
    let bounds = |prefix: &str| {
        nodes
            .iter()
            .find(|(_, node)| {
                node.role() == Role::Button && node.label().is_some_and(|l| l.starts_with(prefix))
            })
            .map(|(_, node)| (node.bounds().unwrap(), node.is_disabled()))
            .unwrap_or_else(|| panic!("{prefix}"))
    };
    let (undo, undo_disabled) = bounds("Undo Bucket name");
    let (redo, redo_disabled) = bounds("Nothing to redo");
    let (test, _) = bounds("Check that ");
    assert!(!undo_disabled && redo_disabled);
    assert!(undo.x1 <= redo.x0 && redo.x1 <= test.x0 && (undo.y0 - test.y0).abs() < 8.0);
}

#[test]
fn a_check_outcome_shows_once_unless_it_is_a_problem() {
    let count = |message: (&str, bool)| {
        let mut editor = editor(None);
        editor.message = Some((message.0.into(), message.1));
        let mut picker = Picker::new(false, false);
        picker.manager_open = true;
        picker.editor = Some(editor);
        shows_text(
            &nodes_of(
                mapped(
                    picker.manager(Palette::of(false), 2000.0, 0, Instant::now(), None, None),
                    crate::gui::Message::CapturePicker,
                ),
                560.0,
            ),
            message.0,
        )
    };
    let once = count(("Writable, 1.0 TB free", false));
    let twice = count(("Can't write to this folder", true));
    assert!(once >= 1);
    assert_eq!(twice, once + 1, "a problem also shows in full");
}
