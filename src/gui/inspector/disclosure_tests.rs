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

fn snapshot(
    app: &Workbench,
    tab: Tab,
    tree: &mut Tree,
    renderer: &iced::Renderer,
) -> accesskit::TreeUpdate {
    let mut view = match tab {
        Tab::Features => app.features(Palette::of(false)),
        Tab::Capture => app.capture_settings(Palette::of(false)),
        Tab::Forward => app.forward_settings(Palette::of(false)),
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
    let initial = snapshot(&app, Tab::Features, &mut tree, &renderer);
    assert_eq!(named(&initial, "Device").1.is_expanded(), Some(false));
    assert_eq!(named(&initial, "Device").1.description(), Some("1 feature"));
    assert!(
        !initial
            .nodes
            .iter()
            .any(|(_, node)| node.label() == Some("Device User ID"))
    );

    app.toggle_inspector_section(Section::Features("Transport"));
    let transport = snapshot(&app, Tab::Features, &mut tree, &renderer);
    let (field_id, _) = named(&transport, "Gev Heartbeat Timeout");
    app.edits.insert("GevHeartbeatTimeout".into(), "27".into());
    app.toggle_inspector_section(Section::Features("Device"));
    let expanded = snapshot(&app, Tab::Features, &mut tree, &renderer);
    let (current_id, field) = named(&expanded, "Gev Heartbeat Timeout");
    assert_eq!(
        current_id, field_id,
        "revealing an earlier group retains later editor identity"
    );
    assert_eq!(field.value(), Some("27"));
    app.toggle_inspector_section(Section::Features("Transport"));
    let collapsed = snapshot(&app, Tab::Features, &mut tree, &renderer);
    assert_eq!(
        named(&collapsed, "Transport").1.description(),
        Some("1 feature · 1 edited")
    );
    assert_eq!(
        app.edits.get("GevHeartbeatTimeout").map(String::as_str),
        Some("27")
    );

    app.search = "image size".into();
    let searched = snapshot(&app, Tab::Features, &mut tree, &renderer);
    named(&searched, "Width");
    app.search = "exposure duration".into();
    let searched = snapshot(&app, Tab::Features, &mut tree, &renderer);
    named(&searched, "Exposure Time");

    app.search = "VendorSpecial".into();
    let searched = snapshot(&app, Tab::Features, &mut tree, &renderer);
    named(&searched, "Vendor Special");
    assert!(!app.inspect.feature_groups_open.contains("Other"));
    app.search.clear();
    app.inspect
        .write_errors
        .insert("VendorSpecial".into(), "Camera refused this value".into());
    let failed = snapshot(&app, Tab::Features, &mut tree, &renderer);
    assert_eq!(named(&failed, "Other").1.is_expanded(), Some(true));
    named(&failed, "Vendor Special");
}

#[test]
fn capture_and_video_prioritize_actions_and_reveal_invalid_advanced_fields() {
    let renderer = destinations::layout_tests::renderer();
    let mut app = app();
    let mut tree = Tree::empty();
    let capture = snapshot(&app, Tab::Capture, &mut tree, &renderer);
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
    assert!(action.y1 < named(&capture, "Automatic capture").1.bounds().unwrap().y0);
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
    let invalid = snapshot(&app, Tab::Capture, &mut tree, &renderer);
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
    let video = snapshot(&app, Tab::Forward, &mut tree, &renderer);
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
    let invalid = snapshot(&app, Tab::Forward, &mut tree, &renderer);
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
fn active_automatic_sessions_prioritize_review_without_moving_editors_on_suggestions() {
    let renderer = destinations::layout_tests::renderer();
    let mut app = app();
    let mut tree = Tree::empty();
    let manual = snapshot(&app, Tab::Capture, &mut tree, &renderer);
    let output_id = named(&manual, "Output path").0;
    app.snapshot.capture_session = Some(crate::autocapture::SessionStatus {
        path: "/isolated/review.duckdb".into(),
        camera: "sim:0".into(),
        enabled: true,
        sensitivity: 0.5,
        cooldown_ms: 2000,
        manual_count: 1,
        kept_count: 0,
        discarded_count: 0,
        pending_count: 0,
        blocked_storage: false,
        state: "watching".into(),
        model: "Session exemplar kNN".into(),
    });
    let active = snapshot(&app, Tab::Capture, &mut tree, &renderer);
    assert_eq!(named(&active, "Output path").0, output_id);
    let (review_id, review) = named(&active, "Review suggestions (0)");
    let review_bounds = review.bounds().unwrap();
    assert!(review_bounds.y1 < named(&active, "Output path").1.bounds().unwrap().y0);
    assert!(
        review_bounds.y1 < 360.0,
        "review is among the first visible actions"
    );
    app.snapshot.capture_session.as_mut().unwrap().pending_count = 3;
    let suggestions = snapshot(&app, Tab::Capture, &mut tree, &renderer);
    let (current_id, review) = named(&suggestions, "Review suggestions (3)");
    assert_eq!(current_id, review_id);
    assert_eq!(review.bounds(), Some(review_bounds));
    assert_eq!(named(&suggestions, "Output path").0, output_id);
    app.snapshot.capture_session = None;
    let closed = snapshot(&app, Tab::Capture, &mut tree, &renderer);
    assert_eq!(named(&closed, "Output path").0, output_id);
}

#[test]
fn the_schedule_command_shows_its_form_in_folded_capture_options() {
    let renderer = destinations::layout_tests::renderer();
    let mut app = app();
    let mut tree = Tree::empty();
    let folded = snapshot(&app, Tab::Capture, &mut tree, &renderer);
    assert!(
        !folded
            .nodes
            .iter()
            .any(|(_, node)| node.label() == Some("Capture interval"))
    );
    let _ = app.menu_command(super::super::menus::Command::Schedule);
    let shown = snapshot(&app, Tab::Capture, &mut tree, &renderer);
    assert_eq!(named(&shown, "Capture options").1.is_expanded(), Some(true));
    assert_eq!(
        named(&shown, "Capture on a schedule").1.toggled(),
        Some(accesskit::Toggled::True)
    );
    named(&shown, "Capture delay");
    named(&shown, "Capture interval");

    // Folded again, the summary says a schedule is on; its form waits.
    let _ = app.update(Message::InspectorDisclosure(Section::CaptureOptions));
    let folded = snapshot(&app, Tab::Capture, &mut tree, &renderer);
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
