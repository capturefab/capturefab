use super::{Envelope, Node, Sequence, has_data, validate_publish};
use crate::timeseries::opcua::subscription::Config;
use crate::types::{ErrorKind, classify};
use opcua::types::{
    DataChangeNotification, DataValue, DateTime, ExtensionObject, MonitoredItemNotification,
    NotificationMessage, PublishResponse, StatusChangeNotification, StatusCode,
    SubscriptionAcknowledgement, Variant,
};
use std::time::{Duration, Instant};

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(2)
}
fn notification(sequence: u32, items: Vec<MonitoredItemNotification>) -> NotificationMessage {
    NotificationMessage {
        sequence_number: sequence,
        publish_time: DateTime::from(123_i64),
        notification_data: Some(vec![ExtensionObject::new(DataChangeNotification {
            monitored_items: Some(items),
            diagnostic_infos: None,
        })]),
    }
}
fn item(status: StatusCode, value: i32) -> MonitoredItemNotification {
    MonitoredItemNotification {
        client_handle: 1,
        value: DataValue {
            value: Some(Variant::Int32(value)),
            status: Some(status),
            source_timestamp: Some(DateTime::from(42_i64)),
            ..Default::default()
        },
    }
}
fn config() -> Config {
    serde_json::from_str("{}").unwrap()
}
fn node() -> Node {
    Node {
        name: "pressure".into(),
        node_id: "ns=2;s=P".into(),
    }
}

#[test]
fn keepalive_does_not_consume_or_skip_the_first_real_sequence() {
    let mut sequence = Sequence::default();
    let keepalive = NotificationMessage {
        sequence_number: 1,
        ..Default::default()
    };
    assert_eq!(sequence.gap(&keepalive).unwrap(), Vec::<u32>::new());
    sequence.admit(&keepalive).unwrap();
    sequence.admit(&keepalive).unwrap();
    let real = notification(1, vec![item(StatusCode::Good, 7)]);
    assert!(has_data(&real));
    sequence.admit(&real).unwrap();
    let next = notification(3, Vec::new());
    assert_eq!(sequence.gap(&next).unwrap(), [2]);
    assert_eq!(
        classify(&sequence.admit(&next).unwrap_err()).kind,
        ErrorKind::Incomplete
    );
    assert_eq!(
        classify(&sequence.gap(&real).unwrap_err()).kind,
        ErrorKind::Incomplete
    );
    assert_eq!(
        classify(&sequence.gap(&NotificationMessage::default()).unwrap_err()).kind,
        ErrorKind::Incomplete
    );
}

#[test]
fn wrap_is_maximum_to_one_and_repairs_are_bounded_at_sixty_four() {
    let mut sequence = Sequence { last: u32::MAX - 1 };
    sequence.admit(&notification(u32::MAX, Vec::new())).unwrap();
    assert_eq!(sequence.gap(&notification(2, Vec::new())).unwrap(), [1]);
    sequence.admit(&notification(1, Vec::new())).unwrap();
    let sequence = Sequence::default();
    assert_eq!(
        sequence.gap(&notification(65, Vec::new())).unwrap(),
        (1..=64).collect::<Vec<_>>()
    );
    assert_eq!(
        classify(&sequence.gap(&notification(66, Vec::new())).unwrap_err()).kind,
        ErrorKind::Incomplete
    );
}

#[test]
fn publish_receipt_acknowledgements_and_no_retransmission_are_explicit() {
    let mut response = PublishResponse {
        subscription_id: 9,
        notification_message: notification(1, Vec::new()),
        ..Default::default()
    };
    assert!(!validate_publish(&response, 9, &[]).unwrap());
    response.available_sequence_numbers = Some(vec![1]);
    assert!(validate_publish(&response, 9, &[]).unwrap());
    let ack = [SubscriptionAcknowledgement {
        subscription_id: 9,
        sequence_number: 3,
    }];
    assert_eq!(
        classify(&validate_publish(&response, 9, &ack).unwrap_err()).kind,
        ErrorKind::Incomplete
    );
    response.results = Some(vec![StatusCode::Good]);
    assert!(validate_publish(&response, 9, &ack).unwrap());
    response.results = Some(vec![StatusCode::BadSequenceNumberUnknown]);
    let error = classify(&validate_publish(&response, 9, &ack).unwrap_err());
    assert_eq!(
        error.details.unwrap()["opcua_status"],
        StatusCode::BadSequenceNumberUnknown.bits()
    );
    response.results = None;
    for available in [vec![0], vec![1, 1], vec![2]] {
        response.available_sequence_numbers = Some(available);
        assert_eq!(
            classify(&validate_publish(&response, 9, &[]).unwrap_err()).kind,
            ErrorKind::Incomplete
        );
    }
    response.available_sequence_numbers = Some(vec![1]);
    assert_eq!(
        classify(&validate_publish(&response, 10, &[]).unwrap_err()).kind,
        ErrorKind::Incomplete
    );
}

#[test]
fn same_node_changes_keep_order_and_native_non_good_quality() {
    let config = config();
    let nodes = [node()];
    let response = PublishResponse {
        subscription_id: 9,
        notification_message: notification(
            1,
            vec![
                item(StatusCode::Good, 7),
                item(StatusCode::Uncertain, 8),
                item(StatusCode::BadNotConnected, 9),
            ],
        ),
        ..Default::default()
    };
    let envelope = Envelope {
        config: &config,
        subscription_id: 9,
        nodes: &nodes,
        setup: None,
        response_header: &response.response_header,
        publish: Some((&response, &[])),
    };
    let prepared = envelope
        .prepare(&response.notification_message, deadline())
        .unwrap();
    assert_eq!(prepared.reading.signal, "subscription");
    assert_eq!(prepared.reading.source_time, None);
    let values = prepared.reading.value["notifications"][0]["items"]
        .as_array()
        .unwrap();
    assert_eq!(values.len(), 3);
    assert_eq!(
        values
            .iter()
            .map(|item| item["data_value"]["status_raw"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [
            u64::from(StatusCode::Good.bits()),
            u64::from(StatusCode::Uncertain.bits()),
            u64::from(StatusCode::BadNotConnected.bits())
        ]
    );
    assert_eq!(
        values
            .iter()
            .map(|item| item["client_handle"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [1, 1, 1]
    );
    assert_eq!(prepared.reading.value["sequence_number"], 1);
    assert_eq!(prepared.reading.value["publish_time_ticks"], 123);
    assert_eq!(prepared.reading.value["lossy"], false);
    assert!(prepared.terminal.is_none());
}

#[test]
fn overflow_retains_the_full_lossy_envelope_before_terminal_failure() {
    let config = config();
    let nodes = [node()];
    let response = PublishResponse {
        subscription_id: 9,
        notification_message: notification(
            1,
            vec![
                item(StatusCode::from(0x0000_0480), 3),
                item(StatusCode::Good, 4),
            ],
        ),
        ..Default::default()
    };
    let envelope = Envelope {
        config: &config,
        subscription_id: 9,
        nodes: &nodes,
        setup: None,
        response_header: &response.response_header,
        publish: Some((&response, &[])),
    };
    let prepared = envelope
        .prepare(&response.notification_message, deadline())
        .unwrap();
    assert_eq!(prepared.reading.value["lossy"], true);
    assert_eq!(
        prepared.reading.value["notifications"][0]["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        classify(&prepared.terminal.unwrap()).kind,
        ErrorKind::Incomplete
    );
    let status = NotificationMessage {
        sequence_number: 1,
        publish_time: DateTime::from(123_i64),
        notification_data: Some(vec![ExtensionObject::new(StatusChangeNotification {
            status: StatusCode::BadTimeout,
            diagnostic_info: opcua::types::DiagnosticInfo::default(),
        })]),
    };
    let prepared = envelope.prepare(&status, deadline()).unwrap();
    assert_eq!(
        prepared.reading.value["notifications"][0]["status_raw"],
        StatusCode::BadTimeout.bits()
    );
    assert_eq!(
        classify(&prepared.terminal.unwrap()).kind,
        ErrorKind::Unavailable
    );
}

#[test]
fn malformed_handle_diagnostics_extent_and_expiry_do_not_publish_a_partial_message() {
    let config = config();
    let nodes = [node()];
    let mut response = PublishResponse {
        subscription_id: 9,
        ..Default::default()
    };
    let mut invalid = item(StatusCode::Good, 7);
    invalid.client_handle = 2;
    response.notification_message = notification(1, vec![item(StatusCode::Good, 6), invalid]);
    let envelope = Envelope {
        config: &config,
        subscription_id: 9,
        nodes: &nodes,
        setup: None,
        response_header: &response.response_header,
        publish: None,
    };
    assert_eq!(
        classify(
            &envelope
                .prepare(&response.notification_message, deadline())
                .unwrap_err()
        )
        .kind,
        ErrorKind::Incomplete
    );
    let valid = notification(1, vec![item(StatusCode::Good, 7)]);
    assert_eq!(
        classify(&envelope.prepare(&valid, Instant::now()).unwrap_err()).kind,
        ErrorKind::Timeout
    );
    let too_many = notification(
        1,
        std::iter::repeat_with(|| item(StatusCode::Good, 7))
            .take(1025)
            .collect(),
    );
    assert_eq!(
        classify(&envelope.prepare(&too_many, deadline()).unwrap_err()).kind,
        ErrorKind::Incomplete
    );
}

#[test]
fn recovery_release_moves_the_original_envelope_and_reserves_wrapper_space() {
    let original = serde_json::json!({"kind":"opcua_publish","delivery":"publish","sequence_number":3,
        "raw":{"clock":100,"opaque":"AP8R"},"notifications":[{"native":18_446_744_073_709_551_615_u64}]});
    let reading = crate::timeseries::Reading {
        signal: "subscription".into(),
        value: original.clone(),
        source_time: None,
    };
    let bytes = super::count(&[&reading], deadline()).unwrap();
    let prepared = super::Prepared {
        reading,
        bytes,
        terminal: None,
    };
    let released = super::recovery_release(prepared, 100, 110, deadline()).unwrap();
    assert_eq!(released.reading.value["kind"], "opcua_recovery_release");
    assert_eq!(
        released.reading.value["clock_basis"],
        "recovery_completed_receive"
    );
    assert_eq!(
        released.reading.value["original_received_monotonic_ns"],
        100
    );
    assert_eq!(
        released.reading.value["recovery_completed_received_monotonic_ns"],
        110
    );
    assert_eq!(released.reading.value["pending"], original);
    assert!(released.bytes <= bytes + super::RECOVERY_RESERVE);
    assert_eq!(
        released.bytes,
        super::count(&[&released.reading], deadline()).unwrap()
    );
    let config = config();
    let nodes = [node()];
    let response = PublishResponse::default();
    let envelope = Envelope {
        config: &config,
        subscription_id: 9,
        nodes: &nodes,
        setup: None,
        response_header: &response.response_header,
        publish: None,
    };
    assert_eq!(
        classify(
            &envelope
                .prepare_reserved(
                    &notification(1, vec![item(StatusCode::Good, 7)]),
                    deadline(),
                    super::MAX_JSON
                )
                .unwrap_err()
        )
        .kind,
        ErrorKind::Incomplete
    );
}

#[test]
fn additional_header_opaque_body_and_diagnostic_string_table_are_not_discarded() {
    let config = config();
    let nodes = [node()];
    let mut response = PublishResponse {
        subscription_id: 9,
        notification_message: notification(1, Vec::new()),
        ..Default::default()
    };
    response.response_header.additional_header =
        ExtensionObject::new(opcua::types::type_loader::ByteStringBody::new(
            vec![0, 255, 17].into(),
            opcua::types::NodeId::new(2, 1234_u32),
        ));
    response.response_header.string_table = Some(vec!["symbol".into()]);
    response.response_header.service_diagnostics.symbolic_id = Some(0);
    let envelope = Envelope {
        config: &config,
        subscription_id: 9,
        nodes: &nodes,
        setup: None,
        response_header: &response.response_header,
        publish: Some((&response, &[])),
    };
    let prepared = envelope
        .prepare(&response.notification_message, deadline())
        .unwrap();
    assert_eq!(
        prepared.reading.value["response_header"]["additional_header"]["body"]["base64"],
        "AP8R"
    );
    assert_eq!(
        prepared.reading.value["response_header"]["string_table"],
        serde_json::json!(["symbol"])
    );
}
