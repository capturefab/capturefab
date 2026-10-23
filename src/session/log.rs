//! Validated absolute event times retain the recording producer's UTC offset.
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogEntry {
    pub timestamp: LogTimestamp,
    pub level: String,
    pub message: String,
}

impl LogEntry {
    pub fn now_utc(level: impl Into<String>, message: impl Into<String>) -> Self {
        Self::at(Utc::now().fixed_offset(), level, message)
    }
    pub fn now_local(level: impl Into<String>, message: impl Into<String>) -> Self {
        Self::at(Local::now().fixed_offset(), level, message)
    }
    fn at(
        time: DateTime<FixedOffset>,
        level: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            level: level.into(),
            message: message.into(),
            timestamp: LogTimestamp::new(time.timestamp_millis(), time.offset().local_minus_utc())
                .expect("the current clock time and UTC offset are valid"),
        }
    }
    pub(super) fn same_event(&self, other: &Self) -> bool {
        self.timestamp.unix_ms == other.timestamp.unix_ms
            && self.level == other.level
            && self.message == other.message
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_or_invalid_timestamps_are_rejected() {
        let entry = serde_json::json!({"level":"warn","message":"diagnostic"});
        assert!(serde_json::from_value::<LogEntry>(entry.clone()).is_err());
        for timestamp in [
            serde_json::json!(null),
            serde_json::json!("invalid"),
            serde_json::json!({"unix_ms":i64::MAX,"utc_offset_seconds":0}),
            serde_json::json!({"unix_ms":0,"utc_offset_seconds":86400}),
            serde_json::json!({"unix_ms":"bad","utc_offset_seconds":0}),
            serde_json::json!({}),
        ] {
            let mut value = entry.clone();
            value["timestamp"] = timestamp;
            assert!(serde_json::from_value::<LogEntry>(value).is_err());
        }
    }
    #[test]
    fn captured_instant_retains_date_and_source_zone_without_a_second_read() {
        let utc = DateTime::parse_from_rfc3339("2026-10-14T14:30:00.123Z").unwrap();
        let local = utc.with_timezone(&FixedOffset::west_opt(4 * 3600).unwrap());
        let worker = LogEntry::at(utc, "error", "same event");
        let coordinator = LogEntry::at(local, "error", "same event");
        assert!(worker.same_event(&coordinator));
        assert_eq!(
            coordinator.timestamp.rfc3339(),
            "2026-10-14T10:30:00.123-04:00"
        );
        let roundtrip: LogEntry =
            serde_json::from_value(serde_json::to_value(&coordinator).unwrap()).unwrap();
        assert_eq!(roundtrip.timestamp, coordinator.timestamp);
        let tomorrow = LogEntry::at(local + chrono::Duration::days(1), "error", "same event");
        assert!(!tomorrow.same_event(&coordinator));
    }
}
