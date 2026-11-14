//! Bounded binary decode regressions for the locally patched SDK depth gauge.
use opcua::types::{
    BinaryDecodable, Context, ContextOwned, DataChangeNotification, DataValue, DecodingOptions,
    DepthGauge, DiagnosticInfo, NamespaceMap, ResponseHeader, StatusCode, Variant,
};
use std::{fmt::Debug, io::Cursor};

fn owned_context(depth: u64) -> ContextOwned {
    ContextOwned::new_default(
        NamespaceMap::new(),
        DecodingOptions {
            decoding_depth_gauge: DepthGauge::new(depth),
            ..Default::default()
        },
    )
}

fn decoded<T: BinaryDecodable>(raw: &[u8], context: &Context<'_>) -> T {
    let mut input = Cursor::new(raw);
    let value = T::decode(&mut input, context).unwrap();
    assert_eq!(usize::try_from(input.position()).unwrap(), raw.len());
    value
}

fn refused<T: BinaryDecodable + Debug>(raw: &[u8], context: &Context<'_>) {
    let error = T::decode(&mut Cursor::new(raw), context).unwrap_err();
    assert_eq!(error.status(), StatusCode::BadDecodingError);
}

// Hand-built binary grammar: each non-leaf has Value present and a DataValue
// Variant tag (23); the last DataValue has no fields. No SDK encoder oracle.
fn data_values(levels: usize) -> Vec<u8> {
    assert!((1..=16).contains(&levels));
    let mut raw = Vec::new();
    for _ in 1..levels {
        raw.extend_from_slice(&[1, 23]);
    }
    raw.push(0);
    raw
}

fn diagnostics(levels: usize) -> Vec<u8> {
    assert!((1..=16).contains(&levels));
    let mut raw = vec![0x40; levels - 1];
    raw.push(0);
    raw
}

fn header(levels: usize) -> Vec<u8> {
    let mut raw = Vec::new();
    raw.extend_from_slice(&0_i64.to_le_bytes());
    raw.extend_from_slice(&42_u32.to_le_bytes());
    raw.extend_from_slice(&0_u32.to_le_bytes());
    raw.extend_from_slice(&diagnostics(levels));
    raw.extend_from_slice(&(-1_i32).to_le_bytes());
    raw.extend_from_slice(&[0, 0, 0]); // null additional ExtensionObject
    raw
}

// Variant -> Variant -> ExtensionObject(DataChangeNotification, ns=0;i=811)
// -> DataValue -> DiagnosticInfo -> inner DiagnosticInfo: five active guards.
// The generated/default type loader must keep the same borrowed Context.
fn mixed() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&1_i32.to_le_bytes()); // one monitored item
    body.extend_from_slice(&7_u32.to_le_bytes());
    body.extend_from_slice(&[1, 25, 0x40, 0]);
    body.extend_from_slice(&(-1_i32).to_le_bytes()); // absent diagnostics array
    let mut raw = vec![24, 22, 1, 0, 0x2b, 3, 1];
    raw.extend_from_slice(&i32::try_from(body.len()).unwrap().to_le_bytes());
    raw.extend_from_slice(&body);
    raw
}

#[test]
fn direct_and_variant_data_values_obey_the_same_entry_limit() {
    let owned = owned_context(2);
    let context = owned.context();
    let direct: DataValue = decoded(&data_values(2), &context);
    assert_eq!(direct.value, Some(Variant::DataValue(Box::default())));
    refused::<DataValue>(&data_values(3), &context);

    let mut variant = vec![23];
    variant.extend_from_slice(&data_values(2));
    let value: Variant = decoded(&variant, &context);
    assert_eq!(value, Variant::DataValue(Box::new(direct)));
    let mut too_deep = vec![23];
    too_deep.extend_from_slice(&data_values(3));
    refused::<Variant>(&too_deep, &context);
    // Reuse this same borrowed Context after both depth refusals.
    let shallow: DataValue = decoded(&[0], &context);
    assert_eq!(shallow, DataValue::default());
}

#[test]
fn diagnostic_entry_limit_also_protects_direct_response_headers() {
    let owned = owned_context(2);
    let context = owned.context();
    let value: DiagnosticInfo = decoded(&diagnostics(2), &context);
    assert_eq!(
        value.inner_diagnostic_info,
        Some(Box::new(DiagnosticInfo::default()))
    );
    refused::<DiagnosticInfo>(&diagnostics(3), &context);
    let value: ResponseHeader = decoded(&header(2), &context);
    assert_eq!(value.request_handle, 42);
    assert_eq!(value.service_result, StatusCode::Good);
    assert_eq!(
        value.service_diagnostics.inner_diagnostic_info,
        Some(Box::default())
    );
    refused::<ResponseHeader>(&header(3), &context);
    let shallow: ResponseHeader = decoded(&header(1), &context);
    assert_eq!(shallow.service_diagnostics, DiagnosticInfo::default());
}

#[test]
fn mixed_default_loader_uses_one_shared_guard_budget() {
    let owned = owned_context(5);
    let context = owned.context();
    let raw = mixed();
    let value: Variant = decoded(&raw, &context);
    let Variant::Variant(inner) = value else {
        panic!("missing explicit nested Variant");
    };
    let Variant::ExtensionObject(extension) = inner.as_ref() else {
        panic!("missing generated ExtensionObject");
    };
    let change = extension.inner_as::<DataChangeNotification>().unwrap();
    let items = change.monitored_items.as_ref().unwrap();
    assert_eq!(items.len(), 1);
    let item = items.first().unwrap();
    assert_eq!(item.client_handle, 7);
    let Some(Variant::DiagnosticInfo(diagnostic)) = &item.value.value else {
        panic!("missing native diagnostic value");
    };
    assert_eq!(diagnostic.inner_diagnostic_info, Some(Box::default()));

    let mut sixth = vec![24];
    sixth.extend_from_slice(&raw);
    refused::<Variant>(&sixth, &context);
    let _: Variant = decoded(&raw, &context);
    let fewer = owned_context(4);
    let fewer_context = fewer.context();
    refused::<Variant>(&raw, &fewer_context);
}

#[test]
fn shallow_array_siblings_release_each_guard_instead_of_accumulating_depth() {
    let owned = owned_context(1);
    let context = owned.context();
    let mut raw = vec![0x97]; // array of DataValue, not nested Variant elements
    raw.extend_from_slice(&128_i32.to_le_bytes());
    raw.extend_from_slice(&[0; 128]);
    let value: Variant = decoded(&raw, &context);
    let Variant::Array(array) = value else {
        panic!("missing homogeneous DataValue array");
    };
    assert_eq!(array.values.len(), 128);
    assert!(
        array
            .values
            .iter()
            .all(|value| *value == Variant::DataValue(Box::default()))
    );

    let mut nested = vec![0x97];
    nested.extend_from_slice(&1_i32.to_le_bytes());
    nested.extend_from_slice(&data_values(2));
    refused::<Variant>(&nested, &context);
    let _: Variant = decoded(&raw, &context);
}

#[test]
fn depth_and_truncation_errors_release_the_same_context_for_later_decodes() {
    let owned = owned_context(2);
    let context = owned.context();
    for _ in 0..4 {
        refused::<DataValue>(&data_values(3), &context);
        refused::<DataValue>(&[1], &context);
        refused::<DataValue>(&[1, 23], &context);
        let value: DataValue = decoded(&data_values(2), &context);
        assert_eq!(value.value, Some(Variant::DataValue(Box::default())));
        refused::<DiagnosticInfo>(&diagnostics(3), &context);
        refused::<DiagnosticInfo>(&[0x40], &context);
        refused::<ResponseHeader>(&header(3), &context);
        let value: DiagnosticInfo = decoded(&diagnostics(2), &context);
        assert_eq!(value.inner_diagnostic_info, Some(Box::default()));
    }
}

#[test]
fn source_timestamp_zero_offset_leaf_does_not_reset_recursive_value_admission() {
    let owned = ContextOwned::new_default(
        NamespaceMap::new(),
        DecodingOptions {
            client_offset: chrono::TimeDelta::seconds(7),
            decoding_depth_gauge: DepthGauge::new(1),
            ..Default::default()
        },
    );
    let context = owned.context();
    let raw = [5, 6, 7, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0];
    let value: DataValue = decoded(&raw, &context);
    assert_eq!(value.value, Some(Variant::Int32(7)));
    assert_eq!(value.source_timestamp.unwrap().ticks(), 1);
    refused::<DataValue>(&data_values(2), &context);
    let again: DataValue = decoded(&raw, &context);
    assert_eq!(again.source_timestamp.unwrap().ticks(), 1);
}

#[test]
fn default_sdk_limit_remains_ten_active_guards_not_json_depth_thirty_two() {
    let owned = ContextOwned::default();
    let context = owned.context();
    assert_eq!(context.options().decoding_depth_gauge.max_depth(), 10);
    let _: DataValue = decoded(&data_values(10), &context);
    refused::<DataValue>(&data_values(11), &context);
    let _: DiagnosticInfo = decoded(&diagnostics(10), &context);
    refused::<DiagnosticInfo>(&diagnostics(11), &context);
    let _: DataValue = decoded(&[0], &context);
}
