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
        features: vec![],
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
        ring_dropped: 0,
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
    assert!(
        store.db.is_some(),
        "DuckDB initialization: {:?}",
        store.error
    );
    store.observe_counters(&session(cameras), now);
    if sample {
        store.sample(now).unwrap();
    }
}
fn report(analytics: &Analytics, now: u64) -> AnalyticsSnapshot {
    analytics.store.lock().unwrap().report(now).unwrap()
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
            bytes: 1800
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
    {
        let store = analytics.store.lock().unwrap();
        let db = store.db.as_ref().unwrap();
        db.execute("INSERT INTO samples SELECT range, ? + range, 0, 1, 0, 100, 1.0, 100.0, 1, 0 FROM range(?)",
            params![now, MAX_SAMPLES as i64 + 5]).unwrap();
    }
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
        let store = analytics.store.lock().unwrap();
        let db = store.db.as_ref().unwrap();
        db.execute_batch("DELETE FROM samples").unwrap();
        db.execute("INSERT INTO samples SELECT range, ? + range * 60000, 0, 1, 0, 100, 1.0, 100.0, 1, 0 FROM range(1440)", params![now + 999]).unwrap();
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
        store.db.as_ref().unwrap().execute(
            "INSERT INTO samples SELECT CAST(? + range AS UBIGINT), ?, 0, 1, 0, 100, 1.0, 100.0, 1, 0 FROM range(?)",
            params![batch * batch_size as u64, now, batch_size]
        ).unwrap();
        store.prune(now).unwrap();
        let count: u64 = store
            .db
            .as_ref()
            .unwrap()
            .query_row("SELECT count(*) FROM samples", [], |r| r.get(0))
            .unwrap();
        assert!(count <= MAX_SAMPLES as u64);
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
