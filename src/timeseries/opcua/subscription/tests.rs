use super::{
    Failure, QUEUE_BYTES, QUEUE_MESSAGES, Received, Shared, Subscription, message, republish_error,
};
use crate::timeseries::opcua::{Config, Mode, validate};
use crate::timeseries::{Reading, Source as _};
use crate::types::{Categorized, ErrorKind, classify};
use serde_json::json;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

fn config() -> Config {
    serde_json::from_value(
        json!({"endpoint":"opc.tcp://127.0.0.1:4840","nodes":[{"name":"P","node_id":"i=1"}]}),
    )
    .unwrap()
}
fn source(shared: Arc<Shared>) -> Subscription {
    Subscription {
        prepared: None,
        shared,
        owner: None,
        observed: None,
    }
}
fn prepared(number: u32, bytes: usize, terminal: Option<anyhow::Error>) -> message::Prepared {
    message::Prepared {
        reading: Reading {
            signal: "subscription".into(),
            value: json!({"number":number}),
            source_time: None,
        },
        bytes,
        terminal,
    }
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

#[test]
fn explicit_subscription_config_retains_exact_numbers_and_default_read() {
    let mut old = config();
    validate(&old).unwrap();
    assert_eq!(old.mode, Mode::Read);
    let encoded = serde_json::to_value(&old).unwrap();
    assert!(encoded.get("mode").is_none());
    old.mode = Mode::Subscription;
    assert_eq!(
        classify(&validate(&old).unwrap_err()).kind,
        ErrorKind::Usage
    );
    old.subscription = Some(serde_json::from_str("{}").unwrap());
    validate(&old).unwrap();
    let nested = old.subscription.as_ref().unwrap();
    assert_eq!(nested.signal, "subscription");
    assert_eq!(nested.publishing_ms, 100);
    assert_eq!(nested.queue_size, 128);
    assert_eq!(nested.max_notifications_per_publish, 1024);
    old.mode = Mode::Read;
    assert_eq!(
        classify(&validate(&old).unwrap_err()).kind,
        ErrorKind::Usage
    );
    for field in [
        "publishing_ms",
        "queue_size",
        "max_notifications_per_publish",
    ] {
        for number in [
            json!(0),
            json!(-1),
            json!(1.5),
            json!(18_446_744_073_709_551_616_u128),
        ] {
            let mut value = json!({});
            value[field] = number;
            if let Ok(subscription) = serde_json::from_value::<super::Config>(value) {
                assert_eq!(
                    classify(&subscription.validate().unwrap_err()).kind,
                    ErrorKind::Usage
                );
            }
        }
    }
    let exact: super::Config = serde_json::from_str(
        r#"{"queue_size":1.28e2,"publishing_ms":1e2,"max_notifications_per_publish":1.024e3}"#,
    )
    .unwrap();
    assert_eq!(exact.queue_size, 128);
    exact.validate().unwrap();
    serde_json::from_value::<super::Config>(json!({"queue_size":1.5})).unwrap_err();
}

#[test]
fn fifo_prefix_precedes_the_first_typed_failure_and_cannot_restart() {
    let shared = Arc::new(Shared::default());
    let first = Categorized::new(ErrorKind::AuthRejected, "first typed cause")
        .details(json!({"opcua_status":42}));
    {
        let mut mailbox = shared.mailbox.lock().unwrap();
        mailbox.queue.push_back(Received {
            reading: prepared(1, 5, None).reading,
            bytes: 5,
            observed: 10,
        });
        mailbox.bytes = 5;
        mailbox.initial_received = true;
        mailbox.error = Some(Failure::new(&first.into()));
        mailbox.done = true;
    }
    shared.fail(&Categorized::error(ErrorKind::Timeout, "later cause"));
    let mut source = source(shared);
    let rows = source.read(Duration::ZERO).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].value["number"], 1);
    assert_eq!(source.observed_ns(), Some(10));
    for _ in 0..2 {
        let error = classify(&source.read(Duration::from_millis(1)).unwrap_err());
        assert_eq!(error.kind, ErrorKind::AuthRejected);
        assert_eq!(error.details.unwrap()["opcua_status"], 42);
    }
    assert!(source.prepared.is_none());
    assert!(source.owner.is_none());
}

#[test]
fn queue_capacity_retains_one_unacknowledged_pre_cutoff_pending_message() {
    let shared = Arc::new(Shared::default());
    let runtime = runtime();
    let observed = crate::host::monotonic_ns().unwrap();
    for number in 0..QUEUE_MESSAGES {
        assert!(
            runtime
                .block_on(shared.admit(
                    prepared(
                        u32::try_from(number).unwrap(),
                        QUEUE_BYTES / QUEUE_MESSAGES,
                        None
                    ),
                    observed,
                    Instant::now() + Duration::from_secs(1)
                ))
                .unwrap()
        );
    }
    assert_eq!(shared.mailbox.lock().unwrap().bytes, QUEUE_BYTES);
    shared.stop();
    assert!(
        !runtime
            .block_on(shared.admit(
                prepared(8, 32, None),
                observed,
                Instant::now() + Duration::from_secs(1)
            ))
            .unwrap()
    );
    let mut source = source(Arc::clone(&shared));
    for number in 0..=QUEUE_MESSAGES {
        let reading = source.drain().unwrap().unwrap();
        assert_eq!(reading.value["number"], number);
        assert_eq!(source.observed_ns(), Some(observed));
    }
    assert!(source.drain().unwrap().is_none());
    let mailbox = shared.mailbox.lock().unwrap();
    assert_eq!(mailbox.bytes, 0);
    assert_eq!(mailbox.queue.len(), 0);
}

#[test]
fn full_fifo_wait_keeps_its_original_deadline_and_cutoff_never_retimes_a_reply() {
    let shared = Shared::default();
    let runtime = runtime();
    let observed = crate::host::monotonic_ns().unwrap();
    for number in 0..QUEUE_MESSAGES {
        runtime
            .block_on(shared.admit(
                prepared(u32::try_from(number).unwrap(), 1, None),
                observed,
                Instant::now() + Duration::from_secs(1),
            ))
            .unwrap();
    }
    let error = runtime
        .block_on(shared.admit(
            prepared(99, 1, None),
            observed,
            Instant::now() + Duration::from_millis(10),
        ))
        .unwrap_err();
    assert_eq!(classify(&error).kind, ErrorKind::Timeout);
    assert_eq!(shared.mailbox.lock().unwrap().queue.len(), QUEUE_MESSAGES);
    shared.mailbox.lock().unwrap().end_ns = Some(observed - 1);
    assert!(
        !runtime
            .block_on(shared.admit(
                prepared(100, 1, None),
                observed,
                Instant::now() + Duration::from_secs(1)
            ))
            .unwrap()
    );
    assert!(shared.mailbox.lock().unwrap().pending.is_none());
}

#[test]
fn lossy_terminal_reading_is_retained_before_failure_and_never_hidden_by_stop() {
    let shared = Arc::new(Shared::default());
    let runtime = runtime();
    let observed = crate::host::monotonic_ns().unwrap();
    let error = Categorized::error(ErrorKind::Incomplete, "native queue overflow");
    assert!(
        runtime
            .block_on(shared.admit(
                prepared(7, 32, Some(error)),
                observed,
                Instant::now() + Duration::from_secs(1)
            ))
            .unwrap()
    );
    let mut source = source(shared);
    source.finish(Duration::ZERO).unwrap();
    assert_eq!(source.drain().unwrap().unwrap().value["number"], 7);
    assert_eq!(
        classify(&source.drain().unwrap_err()).kind,
        ErrorKind::Incomplete
    );
}

#[test]
fn startup_expiry_and_unavailable_republish_preserve_their_typed_cause() {
    let shared = Arc::new(Shared::default());
    let mut source = source(Arc::clone(&shared));
    assert_eq!(
        classify(&source.read(Duration::from_millis(1)).unwrap_err()).kind,
        ErrorKind::Timeout
    );
    assert!(shared.stopping());
    let missing =
        super::super::status_error(opcua::types::StatusCode::BadMessageNotAvailable, true);
    let error = classify(&republish_error(missing, 4));
    assert_eq!(error.kind, ErrorKind::Incomplete);
    assert_eq!(error.details.unwrap()["sequence_number"], 4);
    let timeout = republish_error(super::super::timeout_error(), 4);
    assert_eq!(classify(&timeout).kind, ErrorKind::Timeout);
}

#[test]
fn finish_requires_actual_thread_completion_not_only_a_done_mailbox() {
    let shared = Arc::new(Shared::default());
    shared.mailbox.lock().unwrap().done = true;
    let (release, receive) = std::sync::mpsc::sync_channel::<()>(1);
    let owner = std::thread::spawn(move || {
        receive.recv_timeout(Duration::from_secs(2)).unwrap();
    });
    let mut source = source(shared);
    source.owner = Some(owner);
    let result = source.finish(Duration::from_millis(10));
    release.send(()).unwrap();
    source.owner.take().unwrap().join().unwrap();
    assert_eq!(classify(&result.unwrap_err()).kind, ErrorKind::Timeout);
}

#[test]
fn revised_idle_cadence_cannot_silently_widen_the_service_budget() {
    let mut revision = opcua::types::CreateSubscriptionResponse {
        subscription_id: 7,
        revised_publishing_interval: 100.5,
        revised_max_keep_alive_count: 2,
        revised_lifetime_count: 6,
        ..Default::default()
    };
    super::validate_subscription(&revision, Duration::from_millis(250)).unwrap();
    super::validate_subscription(&revision, Duration::from_millis(212)).unwrap();
    assert_eq!(
        classify(&super::validate_subscription(&revision, Duration::from_millis(211)).unwrap_err())
            .kind,
        ErrorKind::Unsupported
    );
    let error = super::validate_subscription(&revision, Duration::from_millis(201)).unwrap_err();
    assert_eq!(classify(&error).kind, ErrorKind::Unsupported);
    revision.revised_publishing_interval = f64::NAN;
    assert_eq!(
        classify(&super::validate_subscription(&revision, Duration::from_secs(1)).unwrap_err())
            .kind,
        ErrorKind::Incomplete
    );
}

#[test]
fn monitored_item_revisions_refuse_latest_only_and_filter_downgrade() {
    let mut result = opcua::types::MonitoredItemCreateResult {
        status_code: opcua::types::StatusCode::Good,
        monitored_item_id: 9,
        revised_sampling_interval: 0.5,
        revised_queue_size: 2,
        ..Default::default()
    };
    super::validate_monitored_item(&result, 0).unwrap();
    result.revised_queue_size = 8192;
    super::validate_monitored_item(&result, 0).unwrap();
    result.revised_queue_size = 1;
    assert_eq!(
        classify(&super::validate_monitored_item(&result, 0).unwrap_err()).kind,
        ErrorKind::Incomplete
    );
    result.revised_queue_size = 2;
    result.status_code = opcua::types::StatusCode::BadMonitoredItemFilterUnsupported;
    let error = classify(&super::validate_monitored_item(&result, 3).unwrap_err());
    assert_eq!(error.kind, ErrorKind::Unsupported);
    assert_eq!(error.details.unwrap()["node_index"], 3);
    result.status_code = opcua::types::StatusCode::Good;
    result.filter_result = opcua::types::ExtensionObject::new(opcua::types::DataChangeFilter {
        trigger: opcua::types::DataChangeTrigger::Status,
        deadband_type: 0,
        deadband_value: 0.0,
    });
    assert_eq!(
        classify(&super::validate_monitored_item(&result, 0).unwrap_err()).kind,
        ErrorKind::Incomplete
    );
}

#[test]
fn recovered_queue_releases_at_the_real_frontier_without_changing_original_receipts() {
    let runtime = runtime();
    let shared = Arc::new(Shared::default());
    let make = |number: u32, delivery: &str| {
        let reading = Reading {
            signal: "subscription".into(),
            value: json!({"kind":"opcua_publish",
            "delivery":delivery,"sequence_number":number,"notifications":[]}),
            source_time: None,
        };
        let bytes = message::count(&[&reading], Instant::now() + Duration::from_secs(1)).unwrap();
        message::Prepared {
            reading,
            bytes,
            terminal: None,
        }
    };
    let release = message::recovery_release(
        make(3, "publish"),
        100,
        110,
        Instant::now() + Duration::from_secs(1),
    )
    .unwrap();
    for (reading, observed) in [
        (make(2, "republish"), 110),
        (release, 110),
        (make(4, "publish"), 120),
    ] {
        assert!(
            runtime
                .block_on(shared.admit(reading, observed, Instant::now() + Duration::from_secs(1)))
                .unwrap()
        );
    }
    let mut source = source(shared);
    let (send, receive) = std::sync::mpsc::sync_channel(4);
    let began = Instant::now();
    let stop = std::sync::atomic::AtomicBool::new(false);
    let output = crate::timeseries::SourceOutput {
        source: "native",
        stop: &stop,
        sender: &send,
        clock: crate::timeseries::AcquisitionClock {
            start: began,
            end: began + Duration::from_secs(1),
            host_origin: Some(0),
        },
    };
    let mut sequences = Vec::new();
    for expected in [110, 110, 120] {
        let reading = source.drain().unwrap().unwrap();
        output
            .send(&mut source, vec![reading], began, Some(0))
            .unwrap();
        let batch = receive.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(batch.elapsed, Duration::from_nanos(expected));
        assert_eq!(batch.readings.len(), 1);
        if expected == 110 && batch.readings[0].value["kind"] == "opcua_recovery_release" {
            assert_eq!(
                batch.readings[0].value["original_received_monotonic_ns"],
                100
            );
            assert_eq!(batch.readings[0].value["pending"]["sequence_number"], 3);
            sequences.push(batch.readings[0].value["pending"]["sequence_number"].clone());
        } else {
            sequences.push(batch.readings[0].value["sequence_number"].clone());
        }
    }
    assert_eq!(sequences, json!([2, 3, 4]).as_array().unwrap().clone());
}
