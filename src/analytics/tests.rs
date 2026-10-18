use super::*;
use crate::{
    session::{CameraHistoryStatus, CameraSnapshot},
    types::{CameraInfo, Transport},
};

fn camera(id: &str, pid: u32, frames: u64, dropped: u64, bytes: u64) -> CameraSnapshot {
    CameraSnapshot {
        info: CameraInfo {
            id: id.to_owned(),
            vendor: "Capturefab".into(),
            model: "Test camera".into(),
            serial: id.into(),
            address: None,
            transport: Transport::Simulator,
            profile: Default::default(),
        },
        features: vec![].into(),
        streaming: true,
        frames,
        dropped,
        received_bytes: bytes,
        fps: 10.0,
        worker_pid: pid,
        last_error: None,
        forwarding: None,
        jobs: vec![],
        auto: None,
        capture_session: None,
        transport: None,
        history: CameraHistoryStatus::default(),
        lost: 0,
        preview_skipped: 0,
        stale: false,
        error_logged: None,
        forwarding_since_ms: None,
    }
}
fn session(cameras: Vec<CameraSnapshot>) -> SessionSnapshot {
    SessionSnapshot {
        cameras,
        ..Default::default()
    }
}
fn observe(analytics: &Analytics, cameras: Vec<CameraSnapshot>, now: u64, sample: bool) {
    let mut store = analytics.store.lock().unwrap();
    store.observe_counters(&session(cameras), now);
    if sample {
        store.sample(now).unwrap();
    }
}
fn report(analytics: &Analytics, now: u64) -> AnalyticsSnapshot {
    analytics.store.lock().unwrap().report(now).unwrap()
}
/// Insert rows directly, counting them as samples do.
fn insert(store: &mut Store, sql: &str, params: impl duckdb::Params) {
    let rows = store.connection().unwrap().execute(sql, params).unwrap();
    store.rows += rows as u64;
}
fn count(store: &Store) -> u64 {
    let db = store.db.as_ref().unwrap();
    db.query_row("SELECT count(*) FROM samples", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn short_captures_reconnects_resets_and_stale_status_keep_true_totals() {
    let analytics = Analytics::new();
    let now = now_ms();
    observe(&analytics, vec![camera("a", 1, 0, 0, 0)], now, true);
    // Multiple updates inside a sample interval must all reach lifetime totals.
    observe(
        &analytics,
        vec![camera("a", 1, 10, 2, 1000)],
        now + 200,
        false,
    );
    let mut stale = camera("a", 1, 999, 999, 99999);
    stale.stale = true;
    observe(&analytics, vec![stale], now + 300, false);
    observe(
        &analytics,
        vec![camera("a", 1, 15, 3, 1500)],
        now + 500,
        false,
    );
    // Reconnected workers can restart every counter from zero.
    observe(
        &analytics,
        vec![camera("a", 2, 2, 1, 200)],
        now + 700,
        false,
    );
    // Same-generation counter reset also must not underflow or erase history.
    observe(
        &analytics,
        vec![camera("a", 2, 1, 0, 100)],
        now + 800,
        false,
    );
    observe(&analytics, vec![], now + 1000, true);
    let report = report(&analytics, now + 1000);
    assert_eq!(
        report.totals,
        AnalyticsTotals {
            frames: 18,
            dropped: 4,
            bytes: 1800,
            ..Default::default()
        }
    );
    assert_eq!(report.cameras[0].frames, 18);
    assert!(!report.cameras[0].streaming);
    assert_eq!(report.cameras[0].fps, 0.0);
    assert_eq!(report.cameras[0].bytes_per_second, 0.0);
    assert_eq!(report.points.last().unwrap().frames, 18);
    // Initial/reset counts belong in totals but cannot create a rate spike.
    assert_eq!(report.points.last().unwrap().fps, 15.0);
    assert_eq!(report.points.last().unwrap().bytes_per_second, 1500.0);
}

#[test]
fn idle_reads_expire_history_but_keep_constant_size_session_totals() {
    let analytics = Analytics::new();
    let now = now_ms();
    observe(&analytics, vec![camera("a", 1, 5, 1, 500)], now, true);
    observe(&analytics, vec![], now + 1, false);
    let report = report(&analytics, now + RETENTION_SECONDS * 1000 + 2);
    assert_eq!(report.samples, 0);
    assert!(report.points.is_empty());
    assert!(report.cameras.is_empty());
    assert_eq!(report.totals.frames, 5);
}

#[test]
fn hard_row_limit_and_plot_downsampling_bound_long_and_busy_sessions() {
    let analytics = Analytics::new();
    let now = now_ms();
    insert(
        &mut analytics.store.lock().unwrap(),
        "INSERT INTO samples SELECT range, ? + range, 0, 1, 0, 100, 1.0, 100.0, 1, 0, 0, 0 FROM range(?)",
        params![now, MAX_SAMPLES as i64 + 5],
    );
    let summary = report(&analytics, now + MAX_SAMPLES as u64 + 5);
    assert_eq!(summary.samples, MAX_SAMPLES);
    assert!(summary.truncated);
    assert!(summary.points.len() as u64 <= MAX_POINTS);
    let oldest: u64 = analytics
        .store
        .lock()
        .unwrap()
        .db
        .as_ref()
        .unwrap()
        .query_row("SELECT min(sequence) FROM samples", [], |r| r.get(0))
        .unwrap();
    assert_eq!(oldest, 5);
    // Sparse 24-hour data exercises bucket alignment as well as count limits.
    {
        let mut store = analytics.store.lock().unwrap();
        store
            .db
            .as_ref()
            .unwrap()
            .execute_batch("DELETE FROM samples")
            .unwrap();
        store.rows = 0;
        insert(
            &mut store,
            "INSERT INTO samples SELECT range, ? + range * 60000, 0, 1, 0, 100, 1.0, 100.0, 1, 0, 0, 0 FROM range(1440)",
            params![now + 999],
        );
    }
    assert!(
        report(&analytics, now + RETENTION_SECONDS * 1000)
            .points
            .len() as u64
            <= MAX_POINTS
    );
}

#[test]
fn camera_identity_cache_is_bounded_without_evicting_current_workers() {
    let analytics = Analytics::new();
    let now = now_ms();
    observe(
        &analytics,
        vec![camera("active", 1, 10, 0, 1000)],
        now,
        false,
    );
    for index in 0..(MAX_CAMERAS * 2) {
        // Existing active worker comes after the new ID in the incoming list.
        observe(
            &analytics,
            vec![
                camera(&format!("historic-{index}"), 10 + index as u32, 1, 0, 100),
                camera("active", 1, 10, 0, 1000),
            ],
            now + index as u64 + 1,
            false,
        );
    }
    let store = analytics.store.lock().unwrap();
    assert_eq!(store.cameras.len(), MAX_CAMERAS);
    assert_eq!(store.cameras["active"].report.frames, 10);
    assert_eq!(store.totals.frames, 10 + (MAX_CAMERAS * 2) as u64);
}

#[test]
fn metrics_labels_are_escaped_and_nonfinite_rates_are_sanitized() {
    let analytics = Analytics::new();
    let mut camera = camera("a\"\\\nline", 1, 4, 1, 400);
    camera.info.vendor = "bad\"\\\nname".into();
    camera.fps = f64::NAN;
    let session = session(vec![camera]);
    analytics.observe_final(&session);
    let metrics = analytics.metrics(&session).unwrap();
    assert!(metrics.contains("camera_id=\"a\\\"\\\\\\nline\""));
    assert!(metrics.contains("name=\"bad\\\"\\\\\\nname Test camera\""));
    assert!(metrics.contains("capturefab_session_frames_total 4\n"));
    assert!(!metrics.contains("NaN"));
    assert!(!metrics.contains("Infinity"));
    assert!(metrics.ends_with('\n'));
}

#[test]
fn long_camera_ids_with_the_same_prefix_do_not_alias() {
    let analytics = Analytics::new();
    let now = now_ms();
    let prefix = "long-camera-prefix".repeat(100);
    let cameras = vec![
        camera(&format!("{prefix}a"), 1, 10, 0, 1000),
        camera(&format!("{prefix}b"), 2, 20, 0, 2000),
    ];
    observe(&analytics, cameras.clone(), now, true);
    observe(&analytics, cameras, now + 1000, true);
    let summary = report(&analytics, now + 1000);
    assert_eq!(summary.totals.frames, 30);
    assert_eq!(summary.cameras.len(), 2);
    assert_ne!(summary.cameras[0].id, summary.cameras[1].id);
    assert!(
        summary
            .cameras
            .iter()
            .all(|c| c.id.len() <= MAX_LABEL_BYTES)
    );
}

#[test]
fn database_configuration_prohibits_disk_spill_and_bounds_engine_resources() {
    let db = open_database().unwrap();
    let (memory, temp, external, threads): (String, String, bool, u64) = db.query_row(
        "SELECT current_setting('memory_limit'), current_setting('temp_directory'), current_setting('enable_external_access'), current_setting('threads')", [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    ).unwrap();
    // DuckDB displays the SI128MB budget in binary units.
    assert_eq!(memory, "122.0 MiB");
    assert_eq!(temp, "");
    assert!(!external);
    assert_eq!(threads, 1);
    assert!(
        db.execute_batch("COPY (SELECT 1) TO '/tmp/capturefab-must-not-write.csv'")
            .is_err()
    );
}

#[test]
fn database_reuses_memory_through_millions_of_rows_and_expired_history() {
    let analytics = Analytics::new();
    let mut store = analytics.store.lock().unwrap();
    let initial = now_ms();
    let batch_size = 25_000_i64;
    for batch in 0..80_u64 {
        // Repeatedly exceed the live row cap, then advance a full day halfway
        // through to retire every old row. Collection must remain usable.
        let now = initial
            + batch * 1000
            + if batch >= 40 {
                RETENTION_SECONDS * 1000 + 1
            } else {
                0
            };
        insert(
            &mut store,
            "INSERT INTO samples SELECT CAST(? + range AS UBIGINT), ?, 0, 1, 0, 100, 1.0, 100.0, 1, 0, 0, 0 FROM range(?)",
            params![batch * batch_size as u64, now, batch_size],
        );
        store.prune(now).unwrap();
        assert!(count(&store) <= MAX_SAMPLES as u64);
        assert_eq!(count(&store), store.rows);
    }
    let summary = store
        .report(initial + RETENTION_SECONDS * 1000 + 80_000)
        .unwrap();
    assert_eq!(summary.samples, MAX_SAMPLES);
    assert!(
        summary
            .points
            .iter()
            .all(|p| p.timestamp_ms > initial + RETENTION_SECONDS * 1000)
    );
    assert!(store.error.is_none());
}

#[test]
fn empty_and_active_svg_are_valid_accessible_escaped_plotters_charts() {
    let analytics = Analytics::new();
    let empty = analytics.svg().unwrap();
    assert!(empty.contains("Ready for your first frame"));
    let doc = roxmltree::Document::parse(&empty).unwrap();
    assert_eq!(doc.root_element().tag_name().name(), "svg");
    assert_eq!(
        doc.root_element().attribute("viewBox"),
        Some("0 0 1200 700")
    );
    let now = now_ms();
    let mut first = camera("<&cam>", 1, 0, 0, 0);
    first.info.model = "<script>alert('x')</script>".into();
    observe(&analytics, vec![first.clone()], now, true);
    first.frames = 10;
    first.received_bytes = 10240;
    observe(&analytics, vec![first], now + 1000, true);
    let svg = render_svg(&report(&analytics, now + 1000)).unwrap();
    let doc = roxmltree::Document::parse(&svg).unwrap();
    assert!(!doc.descendants().any(|node| node.has_tag_name("script")));
    assert!(svg.contains("Frame rate"));
    assert!(svg.contains("Throughput"));
    assert!(svg.contains("Missed frame rate"));
    assert!(
        doc.descendants()
            .filter(|n| n.has_tag_name("polyline"))
            .any(|n| n
                .attribute("points")
                .unwrap_or_default()
                .split_whitespace()
                .count()
                >= 2)
    );
    // Optional artifact for visual review, portable and absent in normal CI.
    if let Some(path) = std::env::var_os("CAPTUREFAB_ANALYTICS_PREVIEW_SVG") {
        let preview = Analytics::new();
        let mut overview = camera("sim:overview", 2, 0, 0, 0);
        overview.info.model = "Overview".into();
        let mut inspection = camera("sim:inspection", 3, 0, 0, 0);
        inspection.info.model = "Inspection".into();
        observe(
            &preview,
            vec![overview.clone(), inspection.clone()],
            now,
            true,
        );
        for second in 1..=120_u64 {
            let acquired = 25 + (second as f64 / 6.0).sin().mul_add(6.0, 0.0) as i64;
            overview.frames += acquired as u64;
            overview.received_bytes = overview.frames * 65536;
            inspection.frames += 48;
            inspection.received_bytes = inspection.frames * 32768;
            if second % 25 == 0 {
                inspection.dropped += 2;
            }
            observe(
                &preview,
                vec![overview.clone(), inspection.clone()],
                now + second * 1000,
                true,
            );
        }
        std::fs::write(path, render_svg(&report(&preview, now + 120_000)).unwrap()).unwrap();
    }
}

#[test]
fn database_initialization_failure_is_visible_in_health_and_queries() {
    let analytics = Analytics::new();
    {
        let mut store = analytics.store.lock().unwrap();
        store.db = None;
        store.error = Some("database startup failed".into());
    }
    assert!(!analytics.health().ready);
    assert!(analytics.health().error.is_some());
    assert!(analytics.snapshot().is_err());
    assert!(analytics.svg().is_err());
    assert!(analytics.metrics(&SessionSnapshot::default()).is_err());
}

#[test]
fn eight_cameras_keep_a_full_day_of_history() {
    // One row per sample whatever the camera count, so a day of one-second
    // samples fits the row limit. With a row per camera as well, eight
    // cameras kept under eight hours.
    const { assert!(RETENTION_SECONDS / SAMPLE_INTERVAL_SECONDS <= MAX_SAMPLES as u64) };
    let analytics = Analytics::new();
    let start = now_ms();
    let mut cameras: Vec<_> = (0..8)
        .map(|index| camera(&format!("cam{index}"), index + 1, 0, 0, 0))
        .collect();
    let samples = RETENTION_SECONDS / 60;
    for minute in 0..samples {
        for camera in &mut cameras {
            camera.frames += 1800;
            camera.received_bytes += 180_000;
        }
        observe(&analytics, cameras.clone(), start + minute * 60_000, true);
    }
    let end = start + (samples - 1) * 60_000;
    let summary = report(&analytics, end);
    assert!(!summary.truncated);
    assert_eq!(summary.samples as u64, samples, "one row per sample");
    assert!(summary.points.len() as u64 <= MAX_POINTS + 1);
    assert_eq!(summary.points.first().unwrap().timestamp_ms, start);
    assert_eq!(summary.points.last().unwrap().timestamp_ms, end);
    assert_eq!(summary.totals.frames, samples * 8 * 1800);
}

#[test]
fn sessions_without_cameras_never_open_the_database_and_stay_ready() {
    let analytics = Analytics::new();
    let now = now_ms();
    for second in 0..5 {
        observe(&analytics, vec![], now + second * 1000, true);
    }
    analytics.observe(&SessionSnapshot::default());
    assert!(analytics.store.lock().unwrap().db.is_none());
    let health = analytics.health();
    assert!(health.ready && health.error.is_none());
    let empty = analytics.snapshot().unwrap();
    assert_eq!((empty.samples, empty.points.len()), (0, 0));
    assert!(
        analytics
            .svg()
            .unwrap()
            .contains("Ready for your first frame")
    );
    assert!(
        analytics.store.lock().unwrap().db.is_none(),
        "reports open nothing"
    );
    observe(&analytics, vec![camera("a", 1, 0, 0, 0)], now + 5000, true);
    assert!(analytics.store.lock().unwrap().db.is_some());
}

#[test]
fn missed_frames_count_losses_and_preview_skips_apart() {
    let analytics = Analytics::new();
    let now = now_ms();
    let counted = |frames, lost, skipped| CameraSnapshot {
        lost,
        preview_skipped: skipped,
        dropped: lost + skipped,
        ..camera("a", 1, frames, 0, frames * 100)
    };
    observe(&analytics, vec![counted(0, 0, 0)], now, true);
    observe(&analytics, vec![counted(95, 5, 400)], now + 1000, true);
    let summary = report(&analytics, now + 1000);
    assert_eq!(
        summary.totals,
        AnalyticsTotals {
            frames: 95,
            dropped: 405,
            bytes: 9500,
            lost: 5,
            preview_skipped: 400,
        }
    );
    assert_eq!(summary.cameras[0].lost_percent, 5.0);
    let point = summary.points.last().unwrap();
    assert_eq!((point.lost, point.lost_percent), (5, 5.0));
    let metrics = analytics
        .metrics(&session(vec![counted(95, 5, 400)]))
        .unwrap();
    for line in [
        "capturefab_session_lost_frames_total 5\n",
        "capturefab_session_preview_skipped_frames_total 400\n",
        "capturefab_session_dropped_frames_total 405\n",
        "capturefab_camera_lost_frames_total{camera_id=\"a\",name=\"Capturefab Test camera\"} 5\n",
        "capturefab_camera_preview_skipped_frames_total{camera_id=\"a\",name=\"Capturefab Test camera\"} 400\n",
    ] {
        assert!(metrics.contains(line), "{line} in {metrics}");
    }
}

#[test]
fn time_without_cameras_charts_as_idle() {
    let analytics = Analytics::new();
    let now = now_ms();
    let mut busy = camera("a", 1, 0, 0, 0);
    for second in 0..3 {
        busy.frames += 30;
        observe(&analytics, vec![busy.clone()], now + second * 1000, true);
    }
    // Disconnected for ten minutes: nothing is stored.
    for second in 3..603 {
        observe(&analytics, vec![], now + second * 1000, true);
    }
    let mut back = camera("a", 2, 0, 0, 0);
    for second in 603..606 {
        back.frames += 30;
        observe(&analytics, vec![back.clone()], now + second * 1000, true);
    }
    let summary = report(&analytics, now + 605_000);
    assert_eq!(summary.samples, 6, "only seconds with a camera are stored");
    let idle: Vec<_> = summary
        .points
        .iter()
        .filter(|point| point.frames == 0 && point.fps == 0.0)
        .map(|point| point.timestamp_ms - now)
        .collect();
    assert_eq!(idle, [3000, 602_000], "the gap drops to idle and back");
}

#[test]
fn expired_samples_are_deleted_once_a_minute() {
    let analytics = Analytics::new();
    let now = now_ms();
    let day = RETENTION_SECONDS * 1000;
    observe(&analytics, vec![camera("a", 1, 1, 0, 1)], now, true);
    let mut store = analytics.store.lock().unwrap();
    store.prune(now + day).unwrap();
    assert_eq!(store.pruned_at, Some(now + day));
    assert_eq!(count(&store), 1, "not expired yet");
    // Expired now, but the last deletion was under a minute ago.
    store.prune(now + day + 30_000).unwrap();
    assert_eq!((count(&store), store.rows), (1, 1));
    let summary = store.report(now + day + 30_000).unwrap();
    assert!(summary.points.is_empty(), "reports skip it");
    store.prune(now + day + 60_000).unwrap();
    assert_eq!((count(&store), store.rows), (0, 0));
}

#[test]
fn the_last_cameras_final_counts_are_charted_and_the_clock_runs_on_without_cameras() {
    let analytics = Analytics::new();
    let mut busy = camera("a", 1, 0, 0, 0);
    analytics.observe(&session(vec![busy.clone()]));
    // Disconnected well within a second of that sample.
    busy.frames = 18;
    analytics.observe_final(&session(vec![busy]));
    analytics.observe(&session(vec![]));
    let summary = analytics.snapshot().unwrap();
    assert_eq!(summary.totals.frames, 18);
    assert_eq!(
        summary.points.iter().map(|point| point.frames).sum::<u64>(),
        18,
        "{summary:?}"
    );
    // Nothing observes the session now, but its clock keeps running.
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert!(analytics.snapshot().unwrap().collected_at_ms >= summary.collected_at_ms + 20);
}
