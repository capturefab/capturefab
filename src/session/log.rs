//! Absolute event times accompany the legacy producer clock string. Display
//! clients must not infer an instant from that date- and zone-free string.
use chrono::{DateTime, FixedOffset, Local, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Hash)]
pub struct LogTimestamp {
    unix_ms: i64,
    utc_offset_seconds: i32,
}

impl LogTimestamp {
    pub fn new(unix_ms: i64, utc_offset_seconds: i32) -> Option<Self> {
        let utc = DateTime::from_timestamp_millis(unix_ms)?;
        FixedOffset::east_opt(utc_offset_seconds)?;
        utc.naive_utc()
            .checked_add_signed(chrono::Duration::seconds(i64::from(utc_offset_seconds)))?;
        Some(Self {
            unix_ms,
            utc_offset_seconds,
        })
    }
    pub fn unix_ms(self) -> i64 {
        self.unix_ms
    }
    /// Offset of the producer's legacy `time` string, not the viewer's zone.
    pub fn utc_offset_seconds(self) -> i32 {
        self.utc_offset_seconds
    }
    pub fn datetime(self) -> DateTime<FixedOffset> {
        // Only constructors and the validating deserializer can create this.
        DateTime::from_timestamp_millis(self.unix_ms)
            .unwrap()
            .with_timezone(&FixedOffset::east_opt(self.utc_offset_seconds).unwrap())
    }
    pub fn rfc3339(self) -> String {
        self.datetime()
            .to_rfc3339_opts(SecondsFormat::Millis, false)
    }
}

impl<'de> Deserialize<'de> for LogTimestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Fields {
            unix_ms: i64,
            utc_offset_seconds: i32,
        }
        let fields = Fields::deserialize(deserializer)?;
        Self::new(fields.unix_ms, fields.utc_offset_seconds)
            .ok_or_else(|| serde::de::Error::custom("log timestamp or UTC offset is out of range"))
    }
}

fn optional_timestamp<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<LogTimestamp>, D::Error> {
    // Optional metadata from a newer/foreign producer must not make a useful
    // legacy diagnostic disappear. Invalid metadata remains explicitly unknown.
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| serde_json::from_value(value).ok()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub time: String,
    pub level: String,
    pub message: String,
    #[serde(
        default,
        deserialize_with = "optional_timestamp",
        skip_serializing_if = "Option::is_none"
    )]
    pub timestamp: Option<LogTimestamp>,
}

impl LogEntry {
    pub fn legacy(
        time: impl Into<String>,
        level: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            time: time.into(),
            level: level.into(),
            message: message.into(),
            timestamp: None,
        }
    }
    pub fn now_utc(level: impl Into<String>, message: impl Into<String>) -> Self {
        Self::at(Utc::now().fixed_offset(), false, level, message)
    }
    pub fn now_local(level: impl Into<String>, message: impl Into<String>) -> Self {
        Self::at(Local::now().fixed_offset(), false, level, message)
    }
    pub fn now_local_millis(level: impl Into<String>, message: impl Into<String>) -> Self {
        Self::at(Local::now().fixed_offset(), true, level, message)
    }
    fn at(
        time: DateTime<FixedOffset>,
        millis: bool,
        level: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            time: time
                .format(if millis { "%H:%M:%S%.3f" } else { "%H:%M:%S" })
                .to_string(),
            level: level.into(),
            message: message.into(),
            timestamp: LogTimestamp::new(time.timestamp_millis(), time.offset().local_minus_utc()),
        }
    }
    pub(super) fn same_event(&self, other: &Self) -> bool {
        let same_time = match (self.timestamp, other.timestamp) {
            (Some(a), Some(b)) => a.unix_ms == b.unix_ms,
            (None, None) => self.time == other.time,
            _ => false,
        };
        same_time && self.level == other.level && self.message == other.message
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_json_retains_its_exact_shape_and_invalid_optional_metadata_is_unknown() {
        let old = serde_json::json!({"time":"14:30:00","level":"warn","message":"diagnostic"});
        let entry: LogEntry = serde_json::from_value(old.clone()).unwrap();
        assert!(entry.timestamp.is_none());
        assert_eq!(serde_json::to_value(entry).unwrap(), old);
        for timestamp in [
            serde_json::json!("invalid"),
            serde_json::json!({"unix_ms":i64::MAX,"utc_offset_seconds":0}),
            serde_json::json!({"unix_ms":0,"utc_offset_seconds":86400}),
            serde_json::json!({"unix_ms":"bad","utc_offset_seconds":0}),
            serde_json::json!({}),
        ] {
            let mut value = old.clone();
            value["timestamp"] = timestamp;
            let entry: LogEntry = serde_json::from_value(value).unwrap();
            assert!(entry.timestamp.is_none());
            assert_eq!(entry.message, "diagnostic");
        }
    }
    #[test]
    fn captured_instant_supplies_legacy_clock_date_and_source_zone_without_a_second_read() {
        let utc = DateTime::parse_from_rfc3339("2026-10-14T14:30:00.123Z").unwrap();
        let local = utc.with_timezone(&FixedOffset::west_opt(4 * 3600).unwrap());
        let worker = LogEntry::at(utc, false, "error", "same event");
        let coordinator = LogEntry::at(local, true, "error", "same event");
        assert_eq!(worker.time, "14:30:00");
        assert_eq!(coordinator.time, "10:30:00.123");
        assert!(worker.same_event(&coordinator));
        assert_eq!(
            coordinator.timestamp.unwrap().rfc3339(),
            "2026-10-14T10:30:00.123-04:00"
        );
        let roundtrip: LogEntry =
            serde_json::from_value(serde_json::to_value(&coordinator).unwrap()).unwrap();
        assert_eq!(roundtrip.timestamp, coordinator.timestamp);
        let tomorrow = LogEntry::at(
            local + chrono::Duration::days(1),
            true,
            "error",
            "same event",
        );
        assert_eq!(tomorrow.time, coordinator.time);
        assert!(!tomorrow.same_event(&coordinator));
    }
}
