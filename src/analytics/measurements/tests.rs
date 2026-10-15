use super::*;

fn sample(timestamp_ms: u64, value: Option<f64>) -> MeasurementSample {
    MeasurementSample {
        timestamp_ms,
        camera_id: "sim:camera".into(),
        frame_id: 1,
        definition: MeasurementDefinition {
            id: "tubes".into(),
            label: "Tube count".into(),
            prompt: "Count the number of tubes".into(),
            unit: "tubes".into(),
        },
        provider: Provider::OpenAi,
        model: "test-vision-model".into(),
        result: MeasurementOutput {
            id: "tubes".into(),
            value,
            confidence: Some(0.8),
            note: "Two partially obscured tubes may be present.".into(),
        },
    }
}

#[test]
fn measurement_schema_is_additive_and_initialization_is_idempotent() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE samples (frames UBIGINT); INSERT INTO samples VALUES (17)")
        .unwrap();
    initialize_database(&db).unwrap();
    db.execute(
        "INSERT INTO measurement_samples VALUES (0, 100, NULL, NULL, ?)",
        params![serde_json::to_string(&sample(100, None)).unwrap()],
    )
    .unwrap();
    initialize_database(&db).unwrap();
    let frames: u64 = db
        .query_row("SELECT frames FROM samples", [], |row| row.get(0))
        .unwrap();
    let measurements: u64 = db
        .query_row("SELECT count(*) FROM measurement_samples", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(frames, 17);
    assert_eq!(measurements, 1);
}

#[test]
fn nullable_values_and_annotations_round_trip_in_chronological_order() {
    let analytics = Analytics::new();
    let now = now_ms();
    let mut unknown = sample(now + 1000, None);
    unknown.frame_id = 2;
    unknown.result.confidence = None;
    unknown.result.note = "Distance cannot be inferred without a known scale.".into();
    analytics
        .record_measurements(&[unknown.clone(), sample(now, Some(3.0))])
        .unwrap();
    let history = analytics.measurement_history().unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].result.value, Some(3.0));
    assert_eq!(history[1].result.value, None);
    assert_eq!(history[1].result.confidence, None);
    assert_eq!(history[1].result.note, unknown.result.note);
    assert_eq!(history[1].definition.prompt, unknown.definition.prompt);
    let store = analytics.store.lock().unwrap();
    let nulls: u64 = store
        .db
        .as_ref()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM measurement_samples WHERE value IS NULL AND confidence IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(nulls, 1);
}

#[test]
fn malformed_outputs_reject_the_entire_batch_without_inserting_earlier_rows() {
    let analytics = Analytics::new();
    let now = now_ms();
    analytics
        .record_measurements(&[sample(now, Some(1.0))])
        .unwrap();
    for invalid in [
        MeasurementOutput {
            id: "wrong-id".into(),
            ..sample(now, Some(2.0)).result
        },
        MeasurementOutput {
            value: Some(f64::NAN),
            ..sample(now, Some(2.0)).result
        },
        MeasurementOutput {
            value: Some(f64::INFINITY),
            ..sample(now, Some(2.0)).result
        },
        MeasurementOutput {
            confidence: Some(-0.1),
            ..sample(now, Some(2.0)).result
        },
        MeasurementOutput {
            confidence: Some(1.1),
            ..sample(now, Some(2.0)).result
        },
        MeasurementOutput {
            confidence: Some(f64::NAN),
            ..sample(now, Some(2.0)).result
        },
    ] {
        let bad = MeasurementSample {
            result: invalid,
            ..sample(now + 1, Some(2.0))
        };
        assert!(
            analytics
                .record_measurements(&[sample(now + 1, Some(5.0)), bad])
                .is_err()
        );
        assert_eq!(analytics.measurement_history().unwrap().len(), 1);
    }
    let oversized = MeasurementSample {
        result: MeasurementOutput {
            note: "x".repeat(2049),
            ..sample(now, None).result
        },
        ..sample(now, None)
    };
    assert!(analytics.record_measurements(&[oversized]).is_err());
    assert_eq!(analytics.measurement_history().unwrap().len(), 1);
}

#[test]
fn database_insert_failure_rolls_back_the_entire_batch() {
    let analytics = Analytics::new();
    {
        let store = analytics.store.lock().unwrap();
        store
            .db
            .as_ref()
            .unwrap()
            .execute_batch(
                "DROP TABLE measurement_samples;
             CREATE TABLE measurement_samples (
                sequence UBIGINT NOT NULL, timestamp_ms UBIGINT NOT NULL,
                value DOUBLE CHECK(value >= 0), confidence DOUBLE, sample_json VARCHAR NOT NULL
             )",
            )
            .unwrap();
    }
    let now = now_ms();
    assert!(
        analytics
            .record_measurements(&[sample(now, Some(1.0)), sample(now, Some(-1.0))])
            .is_err()
    );
    assert!(analytics.measurement_history().unwrap().is_empty());
}

#[test]
fn measurement_retention_and_row_cap_are_independent_of_capture_counters() {
    let analytics = Analytics::new();
    let now = now_ms();
    analytics
        .record_measurements(&[sample(now, Some(7.0))])
        .unwrap();
    {
        let store = analytics.store.lock().unwrap();
        let db = store.db.as_ref().unwrap();
        db.execute(
            "INSERT INTO samples VALUES (0, ?, 0, 1, 0, 100, 1.0, 100.0, 1, 0)",
            params![now],
        )
        .unwrap();
        let json = serde_json::to_string(&sample(now, Some(7.0))).unwrap();
        db.execute(
            "INSERT INTO measurement_samples SELECT range + 1, ? + range, 7.0, 0.8, ? FROM range(?)",
            params![now, json, MAX_MEASUREMENT_SAMPLES as i64 + 4],
        ).unwrap();
        prune_measurements(db, now).unwrap();
        let (count, oldest): (u64, u64) = db
            .query_row(
                "SELECT count(*), min(sequence) FROM measurement_samples",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(count, MAX_MEASUREMENT_SAMPLES as u64);
        assert_eq!(oldest, 5);
        assert_eq!(
            db.query_row::<u64, _, _>("SELECT count(*) FROM samples", [], |row| row.get(0))
                .unwrap(),
            1
        );
        prune_measurements(
            db,
            now + RETENTION_SECONDS * 1000 + MAX_MEASUREMENT_SAMPLES as u64 + 5,
        )
        .unwrap();
        assert_eq!(
            db.query_row::<u64, _, _>("SELECT count(*) FROM measurement_samples", [], |row| row
                .get(0))
                .unwrap(),
            0
        );
        assert_eq!(
            db.query_row::<u64, _, _>("SELECT count(*) FROM samples", [], |row| row.get(0))
                .unwrap(),
            1
        );
    }
}

#[test]
fn idle_reads_prune_expired_measurements_and_oversized_batches_are_rejected() {
    let analytics = Analytics::new();
    let now = now_ms();
    analytics.record_measurements(&[sample(now, None)]).unwrap();
    {
        let mut store = analytics.store.lock().unwrap();
        store.started_at_ms = now + RETENTION_SECONDS * 1000 + 1;
    }
    assert!(analytics.measurement_history().unwrap().is_empty());
    assert!(
        analytics
            .record_measurements(&vec![sample(now, None); MAX_MEASUREMENT_SAMPLES + 1])
            .is_err()
    );
}

#[test]
fn changes_to_camera_definition_provider_and_model_isolate_chart_series() {
    let original = sample(1000, Some(1.0));
    let original_key = series_identity(&original).unwrap();
    let mut variants = vec![original.clone(); 5];
    variants[0].camera_id = "other-camera".into();
    variants[1].definition.prompt = "Count only the red tubes".into();
    variants[2].definition.unit = "meters".into();
    variants[3].provider = Provider::Claude;
    variants[4].model = "other-model".into();
    for variant in &variants {
        assert_ne!(series_identity(variant).unwrap(), original_key);
    }
    let svg = render_measurement_svg(&variants).unwrap();
    assert_eq!(svg.matches("Tube count").count(), variants.len());
    assert!(svg.contains("meters"));
}

#[test]
fn null_values_break_chart_segments_and_unknown_only_series_render() {
    let observations = [
        sample(1000, Some(1.0)),
        sample(2000, None),
        sample(3000, Some(3.0)),
        sample(4000, Some(4.0)),
    ];
    let references: Vec<_> = observations.iter().collect();
    assert_eq!(
        numeric_segments(&references, 1000, 1.0),
        vec![vec![(0.0, 1.0)], vec![(2.0, 3.0), (3.0, 4.0)]]
    );
    let svg = render_measurement_svg(&observations).unwrap();
    let document = roxmltree::Document::parse(&svg).unwrap();
    let numeric_lines: Vec<_> = document
        .descendants()
        .filter(|node| {
            node.has_tag_name("polyline")
                && node
                    .attribute("stroke")
                    .is_some_and(|stroke| stroke.eq_ignore_ascii_case("#b6ef70"))
        })
        .collect();
    assert_eq!(numeric_lines.len(), 1);
    assert_eq!(
        numeric_lines[0]
            .attribute("points")
            .unwrap()
            .split_whitespace()
            .count(),
        2
    );
    assert!(
        render_measurement_svg(&[sample(1000, None)])
            .unwrap()
            .contains("No numeric value available")
    );
}

#[test]
fn measurement_svg_is_accessible_escapes_labels_and_bounds_panel_count() {
    let mut hostile = sample(1000, Some(2.0));
    hostile.camera_id = "<&camera>\u{1}".into();
    hostile.definition.label = "<script>bad()</script> & tubes".into();
    hostile.definition.unit = "m<&>".into();
    let svg = render_measurement_svg(&[hostile]).unwrap();
    let document = roxmltree::Document::parse(&svg).unwrap();
    assert_eq!(document.root_element().attribute("role"), Some("img"));
    assert!(
        !document
            .descendants()
            .any(|node| node.has_tag_name("script"))
    );
    assert!(
        document
            .descendants()
            .filter_map(|node| node.text())
            .any(|text| text.contains("<script>"))
    );
    let series: Vec<_> = (0..MAX_MEASUREMENT_PANELS + 3)
        .map(|index| MeasurementSample {
            timestamp_ms: index as u64 + 1000,
            camera_id: format!("camera-{index}"),
            ..sample(1000, Some(index as f64))
        })
        .collect();
    let svg = render_measurement_svg(&series).unwrap();
    assert!(svg.contains("3 more remain"));
    assert_eq!(svg.matches("Tube count").count(), MAX_MEASUREMENT_PANELS);
    assert!(roxmltree::Document::parse(&render_measurement_svg(&[]).unwrap()).is_ok());
}

#[test]
fn extreme_finite_and_negative_values_have_renderable_numeric_axes() {
    for values in [
        [f64::MAX, -f64::MAX],
        [f64::MIN_POSITIVE, 5e-324],
        [-15.0, -1.0],
    ] {
        let svg =
            render_measurement_svg(&[sample(1000, Some(values[0])), sample(2000, Some(values[1]))])
                .unwrap();
        assert!(roxmltree::Document::parse(&svg).is_ok());
        assert!(!svg.contains("NaN"));
        assert!(!svg.contains("inf"));
    }
}
