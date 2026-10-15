//! The optional raw-log timestamp is absolute; old opaque clock strings are
//! never interpreted as browser-local dates or silently assigned today's date.
use serde_json::Value;
pub(super) fn unix_ms(log: &Value) -> Option<i64> {
    let stamp = log.get("timestamp")?;
    let time = stamp.get("unix_ms")?.as_i64()?;
    let offset = stamp.get("utc_offset_seconds")?.as_i64()?;
    if !(-86_399..=86_399).contains(&offset) || time.unsigned_abs() > 8_640_000_000_000_000 {
        return None;
    }
    Some(time)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_is_optional_bounded_and_never_parsed_from_legacy_time() {
        let mut log = serde_json::json!({"time":"14:30:00","level":"warn","message":"diagnostic"});
        assert_eq!(unix_ms(&log), None);
        for invalid in [
            serde_json::json!("bad"),
            serde_json::json!({"unix_ms":i64::MAX,"utc_offset_seconds":0}),
            serde_json::json!({"unix_ms":0,"utc_offset_seconds":86400}),
        ] {
            log["timestamp"] = invalid;
            assert_eq!(unix_ms(&log), None);
        }
        log["timestamp"] =
            serde_json::json!({"unix_ms":1792017000123i64,"utc_offset_seconds":-14400});
        assert_eq!(unix_ms(&log), Some(1792017000123));
    }
}
