use super::*;
use crate::session::LogTimestamp;

fn activity_at(unix_ms: u64) -> SessionActivity {
    serde_json::from_value(serde_json::json!({
        "id":1,"origin":"client","actor":{"id":"test-client","name":"Test client","kind":"human"},"operation":"capture","camera":null,"feature":null,
        "label":"Capture","state":"succeeded","submitted_at_ms":unix_ms,
        "started_at_ms":null,"finished_at_ms":unix_ms,"detail":null,"file_count":0,"files":[]
    }))
    .unwrap()
}
#[test]
fn worker_coordinator_and_activity_show_the_same_instant_and_copy_a_dated_zone() {
    let time = DateTime::parse_from_rfc3339("2026-10-14T14:30:00.123Z")
        .unwrap()
        .timestamp_millis();
    let worker = LogEntry {
        timestamp: LogTimestamp::new(time, 0).unwrap(),
        level: "error".into(),
        message: "worker".into(),
    };
    let coordinator = LogEntry {
        timestamp: LogTimestamp::new(time, -4 * 3600).unwrap(),
        level: "error".into(),
        message: "coordinator".into(),
    };
    // Linux publishes native locale results asynchronously; compare one settled
    // presentation rather than the instant when its first cache miss completes.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let (a, b, c) = loop {
        let labels = (
            log(&worker),
            log(&coordinator),
            activity(&activity_at(time as u64)),
        );
        if labels.0.compact == labels.1.compact && labels.1.compact == labels.2.compact {
            break labels;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the same instant must settle on one clock format"
        );
        std::thread::yield_now();
    };
    assert_eq!(a.compact, b.compact);
    assert_eq!(a.compact, c.compact);
    assert_eq!(a.copy, b.copy);
    assert_eq!(a.copy, c.copy);
    assert_eq!(
        DateTime::parse_from_rfc3339(&a.copy)
            .unwrap()
            .timestamp_millis(),
        time
    );
    assert!(a.full.contains("2026-10-14T14:30:00.123+00:00"));
    assert!(b.full.contains("2026-10-14T10:30:00.123-04:00"));
}
#[test]
fn midnight_dst_and_invalid_activity_times_preserve_their_meaning() {
    let before = LogTimestamp::new(
        DateTime::parse_from_rfc3339("2026-11-01T01:30:00-04:00")
            .unwrap()
            .timestamp_millis(),
        -4 * 3600,
    )
    .unwrap();
    let after = LogTimestamp::new(
        DateTime::parse_from_rfc3339("2026-11-01T01:30:00-05:00")
            .unwrap()
            .timestamp_millis(),
        -5 * 3600,
    )
    .unwrap();
    assert_ne!(before.rfc3339(), after.rfc3339());
    assert_eq!(after.unix_ms() - before.unix_ms(), 3_600_000);
    let midnight = LogTimestamp::new(
        DateTime::parse_from_rfc3339("2026-10-14T00:30:00Z")
            .unwrap()
            .timestamp_millis(),
        -4 * 3600,
    )
    .unwrap();
    assert_eq!(midnight.rfc3339(), "2026-10-13T20:30:00.000-04:00");
    assert!(
        activity(&activity_at(u64::MAX))
            .full
            .contains("invalid timestamp")
    );
}

#[test]
fn day_groups_follow_the_displayed_local_date_independently_of_producer_offsets() {
    let instant = DateTime::parse_from_rfc3339("2026-10-14T00:30:00Z")
        .unwrap()
        .timestamp_millis();
    let worker = |offset| LogEntry {
        timestamp: LogTimestamp::new(instant, offset).unwrap(),
        level: "info".into(),
        message: "event".into(),
    };
    let expected = DateTime::parse_from_rfc3339(&log(&worker(0)).copy)
        .unwrap()
        .date_naive();
    for offset in [-12 * 3600, 0, 14 * 3600] {
        let day = log_day(&worker(offset));
        assert_eq!(day.key, Some(expected));
        assert!(!day.label.is_empty());
    }
    assert_eq!(
        activity_day(&activity_at(instant as u64)).key,
        Some(expected)
    );
    assert_eq!(activity_day(&activity_at(u64::MAX)).key, None);
}

#[cfg(target_os = "macos")]
#[test]
fn native_cached_clock_and_day_reject_a_different_zone_without_changing_os_settings() {
    let instant = 1_792_017_000_123;
    let offset = local_instant(instant).unwrap().offset().local_minus_utc();
    assert!(native::format(instant, offset).is_some());
    assert!(native::date(instant, offset).is_some());
    let different = if offset == 0 { 3600 } else { 0 };
    assert_eq!(native::format(instant, different), None);
    assert_eq!(native::date(instant, different), None);
    assert!(native::format(instant, offset).is_some());
}
#[test]
fn timestamp_native_reading_contains_full_date_and_zone_without_a_focus_action() {
    use iced::advanced::{Layout, layout, widget::Tree};
    let renderer = crate::gui::destinations::layout_tests::renderer();
    let labels = activity(&activity_at(1_792_017_000_123));
    let full = labels.full.clone();
    let compact = labels.compact.clone();
    let mut stamp = view(labels, Palette::of(false));
    let mut tree = Tree::new(stamp.as_widget());
    let node = stamp.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(86.0, 200.0)),
    );
    let (snapshot, _, _) = accessibility::snapshot_widget(
        stamp.as_widget_mut(),
        &mut tree,
        Layout::new(&node),
        &renderer,
    );
    let stamps: Vec<_> = snapshot
        .nodes
        .iter()
        .filter(|(_, node)| node.role() == accessibility::Role::Label)
        .collect();
    assert_eq!(
        stamps.len(),
        1,
        "the compact visual caption is not a second native voice item"
    );
    assert_eq!(stamps[0].1.label(), Some(compact.as_str()));
    assert_eq!(stamps[0].1.description(), Some(full.as_str()));
    assert!(!stamps[0].1.supports_action(accesskit::Action::Focus));
}

#[test]
fn raw_timestamp_identity_survives_front_eviction_and_prunes_when_closed_or_empty() {
    use iced::advanced::{Layout, layout, widget::Tree};
    let renderer = crate::gui::destinations::layout_tests::renderer();
    let mut app = Workbench::new(
        crate::session::SessionHandle::new(),
        "raw-time-identity".into(),
        true,
        None,
    );
    let entry = |index: i64| LogEntry {
        timestamp: LogTimestamp::new(1_792_017_000_123 + index * 86_400_000, 0).unwrap(),
        level: "info".into(),
        message: format!("Event {index}"),
    };
    app.snapshot.logs = vec![entry(0), entry(1), entry(2)];
    let mut tree = Tree::empty();
    let collect = |app: &Workbench, tree: &mut Tree| {
        let mut view = app.activity(Palette::of(false));
        tree.diff(view.as_widget());
        let node = view.as_widget_mut().layout(
            tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(640.0, 171.0)),
        );
        let (snapshot, _, _) = accessibility::snapshot_widget(
            view.as_widget_mut(),
            tree,
            Layout::new(&node),
            &renderer,
        );
        snapshot
            .nodes
            .into_iter()
            .filter_map(|(id, node)| {
                node.description().and_then(|description| {
                    description
                        .rsplit_once(" · Recorded ")
                        .map(|(_, recorded)| (recorded.to_owned(), id))
                })
            })
            .collect::<std::collections::HashMap<_, _>>()
    };
    let original = collect(&app, &mut tree);
    let retained: Vec<_> = app.snapshot.logs[1..]
        .iter()
        .map(|entry| entry.timestamp.rfc3339())
        .collect();
    let removed = original[&app.snapshot.logs[0].timestamp.rfc3339()];
    app.snapshot.logs.remove(0);
    app.snapshot.logs.push(entry(3));
    app.sync_client_activity();
    let current = collect(&app, &mut tree);
    for full in retained {
        assert_eq!(
            current[&full], original[&full],
            "retained events keep their actual native timestamp ID after front eviction"
        );
    }
    assert!(
        !current.values().any(|id| *id == removed),
        "the new event cannot inherit an evicted event's timestamp ID"
    );
    assert_eq!(app.client_activity.raw_time_ids.borrow().len(), 3);
    app.snapshot.logs.clear();
    app.sync_client_activity();
    assert!(
        app.client_activity.raw_time_ids.borrow().is_empty(),
        "closed panels also prune their bounded native timestamp cache"
    );
    assert!(collect(&app, &mut tree).is_empty());
}
#[test]
fn local_times_name_their_date_unless_it_is_today() {
    use chrono::Datelike;
    let noon = |days| {
        let date = Local::now().date_naive() + chrono::Days::new(days);
        let at = date
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .earliest()
            .unwrap();
        (at.timestamp_millis() as u64, date.year().to_string())
    };
    let (today, year) = noon(0);
    let time = local_time(today).unwrap();
    assert!(!time.is_empty() && !time.contains(&year), "{time}");
    let (later, year) = noon(3);
    let time = local_time(later).unwrap();
    // The C locale writes two digits of the year.
    assert!(time.contains(&year) || time.contains(&year[2..]), "{time}");
    assert!(local_time(u64::MAX).is_none());
}
