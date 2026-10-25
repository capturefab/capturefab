//! Verify the real controls remain discoverable and retain their drafts.
use super::*;
use iced::advanced::{Layout, layout, widget::Tree};

fn app() -> Workbench {
    let mut app = Workbench::new(SessionHandle::new(), "disclosures".into(), true, None);
    let info = crate::transport::simulator::info();
    app.snapshot.active_camera = Some(info.id.clone());
    app.snapshot.connected = Some(info);
    app.snapshot.features = [
        "ExposureTime",
        "Gain",
        "Width",
        "DeviceUserID",
        "GevHeartbeatTimeout",
        "VendorSpecial",
    ]
    .into_iter()
    .map(|name| sample_feature(name, "Integer", serde_json::json!(10)))
    .collect();
    app.output = "capture.png".into();
    app.forward_output = "recording.mkv".into();
    app.capture_to = destinations::Picker::new(false, false);
    app.record_to = destinations::Picker::new(false, true);
    app.tick_inspector();
    app
}

enum Pane {
    Features,
    Capture,
    Forward,
}

fn snapshot(
    app: &Workbench,
    tab: Pane,
    tree: &mut Tree,
    renderer: &iced::Renderer,
) -> accesskit::TreeUpdate {
    let mut view = match tab {
        Pane::Features => app.features(Palette::of(false)),
        Pane::Capture => app.capture_settings(Palette::of(false)),
        Pane::Forward => app.forward_settings(Palette::of(false)),
    };
    tree.diff(view.as_widget());
    let node = view.as_widget_mut().layout(
        tree,
        renderer,
        &layout::Limits::new(Size::ZERO, Size::new(INSPECTOR - 38.0, 3000.0)),
    );
    accessibility::snapshot_widget(view.as_widget_mut(), tree, Layout::new(&node), renderer).0
}

fn named<'a>(
    tree: &'a accesskit::TreeUpdate,
    name: &str,
) -> (accesskit::NodeId, &'a accesskit::Node) {
    tree.nodes
        .iter()
        .filter(|(_, node)| node.label() == Some(name))
        .min_by_key(|(_, node)| node.role() != accessibility::Role::TextInput)
        .map(|(id, node)| (*id, node))
        .unwrap_or_else(|| panic!("missing {name}"))
}

#[test]
fn quick_controls_do_not_duplicate_or_remove_any_advertised_feature() {
    for kind in [
        CameraKind::AreaScan,
        CameraKind::LineScan,
        CameraKind::Depth,
        CameraKind::Microscope,
        CameraKind::Thermal,
        CameraKind::DocumentScanner,
        CameraKind::Unknown,
    ] {
        let features: Vec<_> = [
            "ExposureTime",
            "Gain",
            "Width",
            "ComponentSelector",
            "ComponentEnable",
            "ScanResolution",
            "IlluminationIntensity",
            "TemperatureRange",
            "VendorExtension",
            "AcquisitionStart",
            "AcquisitionStop",
        ]
        .into_iter()
        .map(|name| sample_feature(name, "Integer", serde_json::json!(10)))
        .collect();
        let index = FeatureIndex::new(&features, kind);
        let mut visible = index.quick.clone();
        visible.extend(
            index
                .groups
                .iter()
                .flat_map(|(_, members)| members.iter().copied())
                .filter(|i| !index.quick.contains(i)),
        );
        visible.sort_unstable();
        assert_eq!(
            visible,
            (0..features.len() - 1).collect::<Vec<_>>(),
            "{kind:?}"
        );
    }
}

#[test]
fn collapsed_features_are_searchable_and_disclosures_retain_drafts_and_editor_identity() {
    let renderer = destinations::layout_tests::renderer();
    let mut app = app();
    let mut tree = Tree::empty();
    let initial = snapshot(&app, Pane::Features, &mut tree, &renderer);
    assert_eq!(named(&initial, "Device").1.is_expanded(), Some(false));
    assert_eq!(named(&initial, "Device").1.description(), Some("1 feature"));
    assert!(
        !initial
            .nodes
            .iter()
            .any(|(_, node)| node.label() == Some("Device User ID"))
    );

    app.toggle_inspector_section(Section::Features("Transport"));
    let transport = snapshot(&app, Pane::Features, &mut tree, &renderer);
    let (field_id, _) = named(&transport, "Gev Heartbeat Timeout");
    app.edits.insert("GevHeartbeatTimeout".into(), "27".into());
    app.toggle_inspector_section(Section::Features("Device"));
    let expanded = snapshot(&app, Pane::Features, &mut tree, &renderer);
    let (current_id, field) = named(&expanded, "Gev Heartbeat Timeout");
    assert_eq!(
        current_id, field_id,
        "revealing an earlier group retains later editor identity"
    );
    assert_eq!(field.value(), Some("27"));
    app.toggle_inspector_section(Section::Features("Transport"));
    let collapsed = snapshot(&app, Pane::Features, &mut tree, &renderer);
    assert_eq!(
        named(&collapsed, "Transport").1.description(),
        Some("1 feature · 1 edited")
    );
    assert_eq!(
        app.edits.get("GevHeartbeatTimeout").map(String::as_str),
        Some("27")
    );

    app.search = "image size".into();
    let searched = snapshot(&app, Pane::Features, &mut tree, &renderer);
    named(&searched, "Width");
    app.search = "exposure duration".into();
    let searched = snapshot(&app, Pane::Features, &mut tree, &renderer);
    named(&searched, "Exposure");
    app.search = "exposuretime".into();
    let searched = snapshot(&app, Pane::Features, &mut tree, &renderer);
    named(&searched, "Exposure");

    app.search = "VendorSpecial".into();
    let searched = snapshot(&app, Pane::Features, &mut tree, &renderer);
    named(&searched, "Vendor Special");
    assert!(!app.inspect.feature_groups_open.contains("Other"));
    app.search.clear();
    app.inspect
        .write_errors
        .insert("VendorSpecial".into(), "Camera refused this value".into());
    let failed = snapshot(&app, Pane::Features, &mut tree, &renderer);
    assert_eq!(named(&failed, "Other").1.is_expanded(), Some(true));
    named(&failed, "Vendor Special");
}

#[test]
fn capture_and_video_prioritize_actions_and_reveal_invalid_advanced_fields() {
    let renderer = destinations::layout_tests::renderer();
    let mut app = app();
    let mut tree = Tree::empty();
    let capture = snapshot(&app, Pane::Capture, &mut tree, &renderer);
    let action = capture
        .nodes
        .iter()
        .find(|(_, node)| {
            node.label()
                .is_some_and(|name| name.starts_with("Capture and save"))
        })
        .unwrap()
        .1
        .bounds()
        .unwrap();
    assert!(action.y1 < named(&capture, "Capture options").1.bounds().unwrap().y0);
    assert_eq!(
        named(&capture, "Capture options").1.is_expanded(),
        Some(false)
    );
    assert!(
        !capture
            .nodes
            .iter()
            .any(|(_, node)| node.label() == Some("Capture on a schedule")),
        "scheduling waits in Capture options"
    );
    assert!(
        !capture
            .nodes
            .iter()
            .any(|(_, node)| node.invalid().is_some())
    );
    app.drafts.insert(Num::Timeout, "invalid".into());
    let invalid = snapshot(&app, Pane::Capture, &mut tree, &renderer);
    assert_eq!(
        named(&invalid, "Capture options").1.is_expanded(),
        Some(true)
    );
    assert!(
        invalid
            .nodes
            .iter()
            .any(|(_, node)| node.invalid() == Some(accesskit::Invalid::True))
    );

    let mut tree = Tree::empty();
    let video = snapshot(&app, Pane::Forward, &mut tree, &renderer);
    let start = named(&video, "Start recording").1.bounds().unwrap();
    assert!(start.y1 < named(&video, "Video settings").1.bounds().unwrap().y0);
    for folded in ["Recording encoder", "Video codec", "Recording bitrate"] {
        assert!(
            !video
                .nodes
                .iter()
                .any(|(_, node)| node.label() == Some(folded)),
            "{folded} waits in Video settings"
        );
    }
    assert_eq!(
        named(&video, "Video settings").1.description(),
        Some("H.264 · 30 fps · 4M"),
        "the folded settings are summed up"
    );
    app.forward_encoder = "invalid/encoder".into();
    let invalid = snapshot(&app, Pane::Forward, &mut tree, &renderer);
    assert_eq!(
        named(&invalid, "Video settings").1.is_expanded(),
        Some(true)
    );
    assert_eq!(
        named(&invalid, "Recording encoder").1.invalid(),
        Some(accesskit::Invalid::True)
    );
}

#[test]
fn the_schedule_command_shows_its_form_in_folded_capture_options() {
    let renderer = destinations::layout_tests::renderer();
    let mut app = app();
    let mut tree = Tree::empty();
    let folded = snapshot(&app, Pane::Capture, &mut tree, &renderer);
    assert!(
        !folded
            .nodes
            .iter()
            .any(|(_, node)| node.label() == Some("Capture interval"))
    );
    let _ = app.menu_command(super::super::menus::Command::Schedule);
    let shown = snapshot(&app, Pane::Capture, &mut tree, &renderer);
    assert_eq!(named(&shown, "Capture options").1.is_expanded(), Some(true));
    assert_eq!(
        named(&shown, "Capture on a schedule").1.toggled(),
        Some(accesskit::Toggled::True)
    );
    named(&shown, "Capture delay");
    named(&shown, "Capture interval");

    // Folded again, the summary says a schedule is on; its form waits.
    let _ = app.update(Message::InspectorDisclosure(Section::CaptureOptions));
    let folded = snapshot(&app, Pane::Capture, &mut tree, &renderer);
    let options = named(&folded, "Capture options").1;
    assert_eq!(options.is_expanded(), Some(false));
    assert_eq!(options.description(), Some("Scheduled"));
    assert!(
        !folded
            .nodes
            .iter()
            .any(|(_, node)| node.label() == Some("Capture interval"))
    );
}

fn labels(tree: &accesskit::TreeUpdate) -> Vec<&str> {
    tree.nodes
        .iter()
        .filter_map(|(_, node)| node.label())
        .collect()
}

/// A workbench opened from what `app` would save, as on the next launch.
fn relaunched(app: &Workbench) -> Workbench {
    let mut next = Workbench::new(
        SessionHandle::new(),
        "disclosures".into(),
        true,
        Some(app.prefs()),
    );
    next.snapshot = app.snapshot.clone();
    next.capture_to = destinations::Picker::new(false, false);
    next.record_to = destinations::Picker::new(false, true);
    next.tick_inspector();
    next
}

#[test]
fn opened_feature_groups_are_remembered_for_the_next_launch() {
    let renderer = destinations::layout_tests::renderer();
    let mut app = app();
    assert!(app.prefs().open_sections.is_empty());
    let _ = app.update(Message::InspectorDisclosure(Section::Features("Image")));
    assert_eq!(app.prefs().open_sections, ["features:Image"]);
    let next = relaunched(&app);
    assert!(next.inspect.feature_groups_open.contains("Image"));
    let mut tree = Tree::empty();
    let features = snapshot(&next, Pane::Features, &mut tree, &renderer);
    assert_eq!(named(&features, "Image").1.is_expanded(), Some(true));
    named(&features, "Width");
    // Closed again, it is forgotten.
    let _ = app.update(Message::InspectorDisclosure(Section::Features("Image")));
    assert!(app.prefs().open_sections.is_empty());
}

#[test]
fn unknown_remembered_sections_are_ignored_and_dropped() {
    let prefs = prefs::Prefs {
        open_sections: vec![
            "features:No such group".into(),
            "telemetry".into(),
            "features:Device".into(),
        ],
        ..Default::default()
    };
    let app = Workbench::new(
        SessionHandle::new(),
        "disclosures".into(),
        true,
        Some(prefs),
    );
    assert_eq!(app.inspect.feature_groups_open.len(), 1);
    assert_eq!(app.prefs().open_sections, ["features:Device"]);
}

#[test]
fn capture_tags_and_storage_reopen_where_they_were_left_without_turning() {
    let renderer = destinations::layout_tests::renderer();
    let mut app = app();
    let _ = app.update(Message::InspectorDisclosure(Section::CaptureTags));
    let _ = app.update(Message::ToggleStorage);
    assert_eq!(app.prefs().open_sections, ["capture-tags", "storage"]);
    let next = relaunched(&app);
    assert!(next.storage_open);
    assert!(
        !next.inspect.storage_turn.animating(next.now),
        "remembered state shows at rest"
    );
    assert_eq!(next.inspect.storage_turn.get(next.now), 1.0);
    let mut tree = Tree::empty();
    let capture = snapshot(&next, Pane::Capture, &mut tree, &renderer);
    named(&capture, "Capture tag Line");
    named(&capture, "Capture tag Station");
    named(&capture, "When a storage limit is reached");
}

#[test]
fn a_section_shown_for_an_invalid_value_is_not_remembered_open() {
    let renderer = destinations::layout_tests::renderer();
    let mut app = app();
    app.forward_bitrate = "invalid".into();
    let mut tree = Tree::empty();
    let video = snapshot(&app, Pane::Forward, &mut tree, &renderer);
    assert_eq!(named(&video, "Video settings").1.is_expanded(), Some(true));
    assert!(!app.prefs().open_sections.contains(&"video-settings".into()));
    let _ = app.update(Message::InspectorDisclosure(Section::EncoderOptions));
    assert!(app.prefs().open_sections.contains(&"video-settings".into()));
}

#[test]
fn expand_all_opens_every_group_and_collapse_all_closes_them() {
    let renderer = destinations::layout_tests::renderer();
    let mut app = app();
    let mut tree = Tree::empty();
    let folded = snapshot(&app, Pane::Features, &mut tree, &renderer);
    named(&folded, "Expand all");
    assert!(labels(&folded).contains(&"6 settings"));
    let _ = app.update(Message::ExpandFeatureGroups(true));
    let groups: Vec<_> = app.inspect.index.groups.iter().map(|(g, _)| *g).collect();
    assert!(
        groups
            .iter()
            .all(|group| app.inspect.feature_groups_open.contains(group))
    );
    let open = snapshot(&app, Pane::Features, &mut tree, &renderer);
    named(&open, "Collapse all");
    named(&open, "Vendor Special");
    let _ = app.update(Message::InspectorDisclosure(Section::Features(groups[0])));
    let one_closed = snapshot(&app, Pane::Features, &mut tree, &renderer);
    named(&one_closed, "Collapse all");
    let _ = app.update(Message::ExpandFeatureGroups(false));
    assert!(app.inspect.feature_groups_open.is_empty());
    app.search = "gain".into();
    let searching = snapshot(&app, Pane::Features, &mut tree, &renderer);
    assert!(
        !labels(&searching)
            .iter()
            .any(|label| *label == "Expand all" || *label == "Collapse all"),
        "searching reveals matches itself"
    );
}

#[test]
fn the_manual_exposure_card_explains_auto_and_toggles_the_histogram() {
    let renderer = destinations::layout_tests::renderer();
    let mut app = app();
    let profile = &mut app.snapshot.connected.as_mut().unwrap().profile;
    profile.kind = CameraKind::AreaScan;
    profile.capabilities = vec![crate::camera_profile::CameraCapability::Exposure];
    app.exposure_open = false;
    let mut tree = Tree::empty();
    let manual = snapshot(&app, Pane::Features, &mut tree, &renderer);
    assert!(
        labels(&manual)
            .iter()
            .any(|label| label.contains("choose Auto"))
    );
    let hint = |what: &str, action: Action| action.hint(what, Os::CURRENT);
    let show = hint("Show the histogram", Action::ToggleExposure);
    assert!(
        labels(&manual).contains(&show.as_str()),
        "{:?}",
        labels(&manual)
    );
    let focus = hint("Show the focus region", Action::ToggleFocusRegion);
    assert!(labels(&manual).contains(&focus.as_str()));
    let _ = app.update(Message::ToggleExposure);
    assert!(app.exposure_open);
    let shown = snapshot(&app, Pane::Features, &mut tree, &renderer);
    let hide = hint("Hide the histogram", Action::ToggleExposure);
    assert!(labels(&shown).contains(&hide.as_str()));
}

#[test]
fn quick_sliders_draft_while_dragging_and_apply_once_on_release() {
    let renderer = destinations::layout_tests::renderer();
    let mut app = app();
    let exposure = app
        .snapshot
        .features
        .iter_mut()
        .find(|f| f.name == "ExposureTime")
        .unwrap();
    exposure.kind = "Float".into();
    exposure.min = Some(10.0);
    exposure.max = Some(1_000_000.0);
    exposure.value = Some(serde_json::json!(10_000.0));
    exposure.unit = Some("us".into());
    let mut tree = Tree::empty();
    let features = snapshot(&app, Pane::Features, &mut tree, &renderer);
    let slider = features
        .nodes
        .iter()
        .find(|(_, node)| {
            node.role() == accessibility::Role::Slider && node.label() == Some("Exposure")
        })
        .expect("the quick exposure control slides");
    assert_eq!(slider.1.numeric_value(), Some(10_000.0));
    // The range in the field's unit, the value converted to ms, and the
    // field says its own unit.
    assert!(
        labels(&features).contains(&"= 10 ms · 10 – 1,000,000 µs"),
        "{:?}",
        labels(&features)
    );
    assert!(labels(&features).contains(&"µs"));
    let sets = |app: &Workbench| {
        app.pending
            .iter()
            .filter(|pending| pending.job == Job::Set("ExposureTime".into()))
            .count()
    };
    for value in ["12000", "15000", "20000"] {
        let _ = app.update(Message::Draft("ExposureTime".into(), value.into()));
    }
    assert_eq!(sets(&app), 0, "dragging only drafts");
    let _ = app.update(Message::Commit("ExposureTime".into()));
    assert_eq!(sets(&app), 1, "releasing applies once");
}

/// Drive the real quick slider with pointer and key events at `x`, the
/// fraction of its width, and return what it publishes.
fn slide(value: f64, events: &[iced::Event], x: f32) -> Vec<Message> {
    let renderer = destinations::layout_tests::renderer();
    let scale =
        widgets::FeatureScale::new("ExposureTime", "Float", Some(10.0), Some(1e6), None).unwrap();
    let on_press = Message::Draft("ExposureTime".into(), "10".into());
    let mut view = widgets::feature_slider("ExposureTime", "Exposure", value, scale, on_press);
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(300.0, 40.0)),
    );
    let bounds = node.bounds();
    let cursor = iced::mouse::Cursor::Available(iced::Point::new(
        bounds.x + bounds.width * x,
        bounds.center_y(),
    ));
    let mut messages = Vec::new();
    for event in events {
        view.as_widget_mut().update(
            &mut tree,
            event,
            Layout::new(&node),
            cursor,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut iced::advanced::Shell::new(&mut messages),
            &bounds,
        );
    }
    messages
}

#[test]
fn a_quick_slider_applies_only_what_a_drag_changed() {
    use iced::mouse::{Button, Event as Mouse};
    let press = iced::Event::Mouse(Mouse::ButtonPressed(Button::Left));
    let release = iced::Event::Mouse(Mouse::ButtonReleased(Button::Left));
    let sets = |messages: Vec<Message>| {
        let mut app = app();
        for message in messages {
            let _ = app.update(message);
        }
        app.pending
            .iter()
            .filter(|pending| pending.job == Job::Set("ExposureTime".into()))
            .count()
    };
    // Pressing and letting go at the bound it already sits on changes nothing.
    let still = slide(10.0, &[press.clone(), release.clone()], 0.0);
    assert!(
        !still.iter().any(|m| matches!(m, Message::Draft(..))),
        "{still:?}"
    );
    assert_eq!(sets(still), 0, "an unchanged release sends nothing");
    // A real drag drafts, and its release applies once.
    let dragged = slide(10.0, &[press, release], 0.5);
    assert!(dragged.iter().any(|m| matches!(m, Message::Draft(..))));
    assert_eq!(sets(dragged), 1);
    // Hover keys and Ctrl-scroll leave the slider to its focused button.
    let keys = [
        iced::keyboard::Event::ModifiersChanged(iced::keyboard::Modifiers::CTRL),
        iced::keyboard::Event::KeyPressed {
            key: iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowUp),
            modified_key: iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowUp),
            physical_key: iced::keyboard::key::Physical::Unidentified(
                iced::keyboard::key::NativeCode::Unidentified,
            ),
            location: iced::keyboard::Location::Standard,
            modifiers: iced::keyboard::Modifiers::CTRL,
            text: None,
            repeat: false,
        },
    ]
    .map(iced::Event::Keyboard);
    let wheel = iced::Event::Mouse(Mouse::WheelScrolled {
        delta: iced::mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
    });
    let hovered = slide(10_000.0, &[keys[0].clone(), keys[1].clone(), wheel], 0.5);
    assert!(hovered.is_empty(), "{hovered:?}");
}

#[test]
fn an_out_of_range_draft_reads_plainly_and_keeps_the_raw_description() {
    let renderer = destinations::layout_tests::renderer();
    let mut app = app();
    let exposure = app
        .snapshot
        .features
        .iter_mut()
        .find(|f| f.name == "ExposureTime")
        .unwrap();
    exposure.min = Some(10.0);
    exposure.max = Some(1_000_000.0);
    exposure.unit = Some("us".into());
    let raw = format!("{:#}", exposure.validate_input("5").unwrap_err());
    app.edits.insert("ExposureTime".into(), "5".into());
    let mut tree = Tree::empty();
    let features = snapshot(&app, Pane::Features, &mut tree, &renderer);
    let field = features
        .nodes
        .iter()
        .find(|(_, node)| {
            node.role() == accessibility::Role::TextInput && node.label() == Some("Exposure")
        })
        .map(|(_, node)| node)
        .unwrap();
    assert_eq!(field.invalid(), Some(accesskit::Invalid::True));
    assert_eq!(field.description(), Some(raw.as_str()));
    assert!(labels(&features).contains(&"Too low: use at least 10 µs"));
}

#[test]
fn the_next_capture_names_its_file_and_folder_apart() {
    let renderer = destinations::layout_tests::renderer();
    let mut app = app();
    app.output = prefs::default_capture_output();
    let preview = app.output_preview();
    let (file, folder) = split_preview(&preview);
    let camera = crate::naming::camera_label(app.snapshot.connected.as_ref().unwrap());
    assert!(file.starts_with(&camera), "{file}");
    assert!(file.ends_with(".png"), "{file}");
    if std::env::var_os("HOME").is_some_and(|home| !home.is_empty()) && !cfg!(windows) {
        assert!(folder.starts_with('~'), "{folder}");
    }
    assert_eq!(
        split_preview("clip.png"),
        ("clip.png".into(), String::new())
    );
    assert_eq!(split_preview("/clip.png"), ("clip.png".into(), "/".into()));
    assert_eq!(
        split_preview("C:\\captures\\clip.png"),
        ("clip.png".into(), "C:\\captures".into())
    );
    let mut tree = Tree::empty();
    let capture = snapshot(&app, Pane::Capture, &mut tree, &renderer);
    assert!(
        labels(&capture)
            .iter()
            .any(|label| label.starts_with("Next file"))
    );
    // The name whole, and the dated subfolder under the chosen folder on
    // a line of its own, so neither cuts into the other.
    let root = app.fixed_folder(&app.output).unwrap();
    let shown = &preview[root.len() + 1..];
    let (_, dated) = split_preview(shown);
    assert!(!dated.is_empty(), "{shown}");
    assert!(
        labels(&capture).contains(&file.as_str()),
        "{:?}",
        labels(&capture)
    );
    let caption = format!("in {dated}/");
    assert!(
        labels(&capture).contains(&caption.as_str()),
        "{:?}",
        labels(&capture)
    );
}

#[test]
fn storage_limits_read_in_plain_words() {
    let renderer = destinations::layout_tests::renderer();
    let mut app = app();
    let _ = app.update(Message::ToggleStorage);
    let _ = app.update(Message::RetentionEnabled(true));
    let mut tree = Tree::empty();
    let capture = snapshot(&app, Pane::Capture, &mut tree, &renderer);
    for label in [
        "Use up to",
        "Keep at most",
        "When full",
        "When a storage limit is reached",
        "Delete captures older than a set age",
        "Older than",
        "Your limits. An app-wide policy can be stricter.",
    ] {
        assert!(labels(&capture).contains(&label), "missing {label}");
    }
    assert!(
        !labels(&capture)
            .iter()
            .any(|label| label.starts_with("Requested"))
    );
}

#[test]
fn switching_to_a_stream_and_back_restores_the_recording_exactly() {
    let mut app = app();
    let template = "clips/{camera}-{seq}.mkv".to_owned();
    app.forward_output = template.clone();
    let command = format!("{:?}", app.forward_command());
    let _ = app.update(Message::ForwardMode(true));
    assert!(app.forward_streams());
    assert_eq!(app.forward_output, "");
    assert_eq!(app.record_to.selected, None);
    let _ = app.update(Message::RecordPicker(destinations::Message::Output(
        "rtsp://user:pass@host:8554/live".into(),
    )));
    let _ = app.update(Message::ForwardMode(false));
    assert!(!app.forward_streams());
    assert_eq!(app.forward_output, template);
    assert_eq!(
        format!("{:?}", app.forward_command()),
        command,
        "the same output sends the same command"
    );
    let saved = serde_json::to_string(&app.prefs()).unwrap();
    assert!(
        !saved.contains("user:pass"),
        "a stashed URL never reaches the prefs"
    );
    // Back to the stream: its URL returns.
    let _ = app.update(Message::ForwardMode(true));
    assert_eq!(app.forward_output, "rtsp://user:pass@host:8554/live");
    // Undo takes the switch back.
    let _ = app.update(Message::Undo);
    assert_eq!(app.forward_output, template);
}

#[test]
fn undoing_a_switch_back_to_recording_keeps_the_recording_for_later() {
    let mut app = app();
    let template = "clips/{camera}-{seq}.mkv".to_owned();
    app.forward_output = template.clone();
    let _ = app.update(Message::ForwardMode(true));
    let _ = app.update(Message::ForwardMode(false));
    assert_eq!(app.forward_output, template);
    // Undo returns to the stream, and its stash with it.
    let _ = app.update(Message::Undo);
    assert!(app.forward_streams());
    let _ = app.update(Message::ForwardMode(false));
    assert_eq!(app.forward_output, template, "not the default template");
}

#[test]
fn recording_switched_to_first_starts_from_the_default_template() {
    let mut app = app();
    app.forward_output = "rtsp://127.0.0.1:8554/line-1".into();
    let _ = app.update(Message::ForwardMode(false));
    assert_eq!(app.forward_output, prefs::default_record_output());
    assert_eq!(app.record_to.selected, None);
    let _ = app.update(Message::ForwardMode(true));
    assert_eq!(app.forward_output, "rtsp://127.0.0.1:8554/line-1");
}

#[test]
fn a_stream_url_being_retyped_stays_in_stream_mode() {
    let renderer = destinations::layout_tests::renderer();
    // A returning user whose saved output is a stream URL.
    let saved = prefs::Prefs {
        forward_output: "rtsp://127.0.0.1:8554/line-1".into(),
        ..prefs::Prefs::default()
    };
    let mut relaunched = Workbench::new(
        SessionHandle::new(),
        "disclosures".into(),
        true,
        Some(saved),
    );
    relaunched.snapshot = app().snapshot.clone();
    // And one who typed a URL into the File field.
    let mut typed = app();
    let _ = typed.update(Message::RecordPicker(destinations::Message::Output(
        "rtsp://host/live".into(),
    )));
    for mut app in [relaunched, typed] {
        assert!(app.forward_streams());
        for value in ["", "192.", "192.168.1.10:8554/live", "mediamtx.local"] {
            let _ = app.update(Message::RecordPicker(destinations::Message::Output(
                value.into(),
            )));
            assert!(app.forward_streams(), "{value:?} switched to Record");
            let video = snapshot(&app, Pane::Forward, &mut Tree::empty(), &renderer);
            assert_eq!(named(&video, "Stream URL").1.value(), Some(value));
        }
        // Only the switch leaves Stream mode, and Undo takes it back.
        let _ = app.update(Message::RecordPicker(destinations::Message::Output(
            String::new(),
        )));
        let _ = app.update(Message::ForwardMode(false));
        assert!(!app.forward_streams());
        let _ = app.update(Message::Undo);
        assert!(app.forward_streams(), "Undo restores the empty stream");
        assert_eq!(app.forward_output, "");
    }
}

#[test]
fn a_stream_shows_one_url_field_and_nothing_about_files() {
    let renderer = destinations::layout_tests::renderer();
    let mut app = app();
    app.forward_output = "rtsp://127.0.0.1:8554/line-1".into();
    let _ = app.update(Message::InspectorDisclosure(Section::EncoderOptions));
    let mut tree = Tree::empty();
    let video = snapshot(&app, Pane::Forward, &mut tree, &renderer);
    let field = named(&video, "Stream URL").1;
    assert_eq!(field.value(), Some("rtsp://127.0.0.1:8554/line-1"));
    named(&video, "Start forwarding");
    for absent in [
        "Save destination",
        "Storage & retention",
        "Maximum file size",
    ] {
        assert!(!labels(&video).contains(&absent), "{absent} shows");
    }
    assert_eq!(
        named(&video, "Forward to URL").1.toggled(),
        Some(accesskit::Toggled::True)
    );
}

#[test]
fn an_empty_output_asks_once_without_a_red_field() {
    let renderer = destinations::layout_tests::renderer();
    for stream in [false, true] {
        let mut app = app();
        if stream {
            let _ = app.update(Message::ForwardMode(true));
        } else {
            app.forward_output.clear();
        }
        let mut tree = Tree::empty();
        let video = snapshot(&app, Pane::Forward, &mut tree, &renderer);
        let field = named(&video, if stream { "Stream URL" } else { "File" }).1;
        assert_eq!(field.invalid(), None);
        let asks: Vec<_> = labels(&video)
            .into_iter()
            .filter(|label| label.starts_with("Enter a"))
            .collect();
        assert_eq!(asks.len(), 1, "{asks:?}");
        assert!(
            !labels(&video)
                .iter()
                .any(|label| label.contains("output needs") || label.contains("must")),
            "{:?}",
            labels(&video)
        );
        assert!(
            named(
                &video,
                if stream {
                    "Start forwarding"
                } else {
                    "Start recording"
                }
            )
            .1
            .is_disabled()
        );
    }
    // A wrong value says so once, at its field.
    let mut app = app();
    app.forward_output = "recording.unsupported".into();
    let mut tree = Tree::empty();
    let video = snapshot(&app, Pane::Forward, &mut tree, &renderer);
    let error = crate::media::validate_forward_output("recording.unsupported")
        .unwrap_err()
        .to_string();
    assert_eq!(
        labels(&video)
            .iter()
            .filter(|label| **label == error)
            .count(),
        1,
        "{:?}",
        labels(&video)
    );
}

#[test]
fn a_capture_output_error_is_said_once() {
    let renderer = destinations::layout_tests::renderer();
    let mut app = app();
    app.output = "rtsp://host/live".into();
    let mut tree = Tree::empty();
    let capture = snapshot(&app, Pane::Capture, &mut tree, &renderer);
    let error = crate::engine::validate_capture_output("rtsp://host/live")
        .unwrap_err()
        .to_string();
    assert_eq!(
        labels(&capture)
            .iter()
            .filter(|label| **label == error)
            .count(),
        1
    );
}

#[test]
fn the_default_recording_shows_where_it_saves_and_folds_its_field() {
    let renderer = destinations::layout_tests::renderer();
    let mut app = app();
    app.forward_output = prefs::default_record_output();
    let mut tree = Tree::empty();
    let video = snapshot(&app, Pane::Forward, &mut tree, &renderer);
    assert!(!labels(&video).contains(&"File"), "the field waits folded");
    let location = named(&video, "Location & name").1;
    assert_eq!(location.is_expanded(), Some(false));
    let summary = location.description().unwrap();
    assert!(summary.starts_with("This computer · "), "{summary}");
    assert!(
        !summary.contains('{') && !summary.contains("####"),
        "{summary}"
    );
    let now = chrono::Local::now();
    let (file, _) = split_preview(&app.preview_at(&app.forward_output, now));
    assert!(file.contains("####"), "{file}");
    assert!(file.ends_with(".mp4"));
    assert!(
        labels(&video)
            .iter()
            .any(|label| label.ends_with("-####.mp4"))
    );
    let _ = app.update(Message::InspectorDisclosure(Section::RecordLocation));
    assert_eq!(app.prefs().open_sections, ["record-location"]);
    for output in ["clips/run.mkv", "recording.unsupported"] {
        let mut app = super::disclosure_tests::app();
        app.forward_output = output.into();
        let video = snapshot(&app, Pane::Forward, &mut Tree::empty(), &renderer);
        assert_eq!(named(&video, "File").1.value(), Some(output));
        assert!(
            !app.prefs()
                .open_sections
                .contains(&"record-location".into())
        );
    }
    // A chosen destination shows the field, view-only.
    let mut app = super::disclosure_tests::app();
    app.forward_output = prefs::default_record_output();
    app.record_to.selected = Some("named destination".into());
    let video = snapshot(&app, Pane::Forward, &mut Tree::empty(), &renderer);
    assert_eq!(named(&video, "Location & name").1.is_expanded(), Some(true));
    assert!(!labels(&video).iter().any(|l| l.starts_with("Saves as")));
    assert!(
        !app.prefs()
            .open_sections
            .contains(&"record-location".into())
    );
    app.forward_output = "clip.mkv".into();
    assert_eq!(app.record_summary(), "This computer");
}

#[test]
fn quick_recording_ignores_an_unfinished_stream_url() {
    let mut app = app();
    let _ = app.update(Message::ForwardMode(true));
    assert_eq!(app.forward_output, "");
    match app.record_command(false) {
        SessionCommand::Forward { output, .. } => {
            assert_eq!(output, prefs::default_record_output())
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_saved_line_and_recording_card_read_as_they_progress() {
    let renderer = destinations::layout_tests::renderer();
    let mut app = app();
    let camera = app.snapshot.active_camera.clone();
    app.last_saved = Saved::from_result(
        camera.clone(),
        &serde_json::json!({
            "files": ["capture-0024.png"],
            "frames": [{"width": 640, "height": 480}],
        }),
        app.now,
    );
    if let Some(camera) = &camera {
        app.add_saved(camera, 24);
    }
    let mut tree = Tree::empty();
    let capture = snapshot(&app, Pane::Capture, &mut tree, &renderer);
    assert!(
        labels(&capture).contains(&"24 saved this session \u{b7} 640\u{d7}480 PNG"),
        "{:?}",
        labels(&capture)
    );

    // A recording the CLI started: the session names its file.
    let home = prefs::capture_root();
    let template = home.join("{date}/{camera}-{seq}.mp4");
    let file = home.join("2026-10-25/Sim-SIM0-0001.mp4");
    app.snapshot.forwarding = Some(template.to_string_lossy().into_owned());
    app.snapshot.recording_path = Some(file.to_string_lossy().into_owned());
    app.snapshot.forwarding_id = Some("a".into());
    app.snapshot.forwarding_since_ms = Some(super::epoch_ms().saturating_sub(84_500));
    app.snapshot.forwarding_progress = Some(crate::session::ForwardingProgress {
        id: "a".into(),
        accepted: 2_520,
        written: 2_520,
        dropped: 3,
        late: 0,
        stalled: false,
        recording: None,
    });
    let video = snapshot(&app, Pane::Forward, &mut tree, &renderer);
    let shown = labels(&video);
    assert!(shown.contains(&"Sim-SIM0-0001.mp4"), "{shown:?}");
    assert!(
        !shown.iter().any(|label| label.contains("{date}")),
        "{shown:?}"
    );
    assert!(
        shown
            .iter()
            .any(|label| label.ends_with("2,520 written \u{b7} 3 dropped")),
        "{shown:?}"
    );
}

#[test]
fn capture_tags_say_whether_the_dataset_catalogs_them() {
    let renderer = destinations::layout_tests::renderer();
    let mut app = app();
    let _ = app.update(Message::InspectorDisclosure(Section::CaptureTags));
    let mut tree = Tree::empty();
    let off = snapshot(&app, Pane::Capture, &mut tree, &renderer);
    let caption = super::super::tags::caption(app.format, None);
    assert!(
        labels(&off).contains(&caption.as_str()),
        "{:?}",
        labels(&off)
    );
    named(&off, "Show Dataset");
    let _ = app.update(Message::Dataset(super::super::dataset::Msg::Auto(true)));
    let on = snapshot(&app, Pane::Capture, &mut tree, &renderer);
    let caption = super::super::tags::caption(app.format, Some("capturefab.duckdb"));
    assert!(labels(&on).contains(&caption.as_str()), "{:?}", labels(&on));
    assert!(!labels(&on).contains(&"Show Dataset"));
}

#[test]
fn fill_in_label_opens_capture_tags_and_stays_open() {
    let mut app = app();
    app.output = "/tmp/shots/{tag:label}/{camera}-{seq}.png".into();
    assert_eq!(
        super::super::tags::missing_field(&app.output, &app.tags),
        Some(super::super::tags::TagField::Label)
    );
    for _ in 0..2 {
        let _ = app.update(Message::FillTag(super::super::tags::TagField::Label));
        assert!(app.inspect.tags_open);
    }
}
