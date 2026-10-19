//! Bounded session-lifetime capture schedules. Clock times use UTC Unix milliseconds.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_DURATION_MS: u64 = 315_576_000_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureJob {
    pub id: u64,
    pub status: String,
    pub output: String,
    pub count: u32,
    pub captured: u32,
    /// Shots not taken because the saver already held two frames.
    #[serde(default)]
    pub skipped: u32,
    /// Shots that failed for a reason that may pass, such as a busy storage
    /// ledger, and were taken again at a later interval.
    #[serde(default)]
    pub retries: u32,
    pub first_at_ms: u64,
    pub interval_ms: u64,
    pub next_at_ms: u64,
    pub last_file: Option<String>,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub context: crate::context::Context,
}
impl CaptureJob {
    pub fn active(&self) -> bool {
        matches!(self.status.as_str(), "pending" | "running")
    }
}
/// A local recording in progress, kept by the worker until it stops so the
/// finished file can be reported with its context.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecordingMeta {
    pub path: String,
    /// UTC Unix milliseconds.
    pub since_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub context: crate::context::Context,
}
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
pub fn parse_duration(s: &str) -> Result<u64> {
    let s = s.trim();
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (value, unit) = s.split_at(split);
    let factor = match unit {
        "ms" => 1u128,
        "" | "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => anyhow::bail!("duration unit must be ms, s, m, h or d"),
    };
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    ensure!(
        !value.is_empty()
            && (!whole.is_empty() || !fraction.is_empty())
            && whole
                .bytes()
                .chain(fraction.bytes())
                .all(|b| b.is_ascii_digit()),
        "duration needs a decimal number and optional ms/s/m/h/d suffix (for example 30s or 5m)"
    );
    let whole = if whole.is_empty() {
        0
    } else {
        whole.parse::<u128>().context("duration is too large")?
    };
    // Trailing zeroes need no precision. Keeping this as integer arithmetic
    // prevents both floating-point overflow and silent millisecond rounding.
    let fraction = fraction.trim_end_matches('0');
    let scale = 10u128
        .checked_pow(
            fraction
                .len()
                .try_into()
                .context("duration has too many decimal places")?,
        )
        .context("duration has too many decimal places")?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse::<u128>()
            .context("duration has too many decimal places")?
    };
    let fractional_ms = fraction
        .checked_mul(factor)
        .context("duration is too large")?;
    ensure!(
        fractional_ms % scale == 0,
        "duration must resolve to a whole number of milliseconds"
    );
    let millis = whole
        .checked_mul(factor)
        .and_then(|n| n.checked_add(fractional_ms / scale))
        .context("duration is too large")?;
    ensure!(
        (1..=MAX_DURATION_MS as u128).contains(&millis),
        "duration must be 1ms..10 years"
    );
    Ok(millis as u64)
}
/// Validate the entire schedule, rather than only its first timestamp. Zero
/// interval captures consecutive frames; a past first timestamp starts now.
pub fn validate_schedule(first_at_ms: u64, interval_ms: u64, count: u32) -> Result<()> {
    ensure!((1..=100_000).contains(&count), "count must be 1..100000");
    ensure!(
        interval_ms <= MAX_DURATION_MS,
        "interval must be at most 10 years"
    );
    let now = now_ms();
    let first = first_at_ms.max(now);
    let final_at = interval_ms
        .checked_mul(u64::from(count - 1))
        .and_then(|span| first.checked_add(span))
        .context("capture schedule timestamp overflows")?;
    ensure!(
        final_at <= now.saturating_add(MAX_DURATION_MS),
        "all scheduled frames must fall within 10 years"
    );
    Ok(())
}
/// The frame count of a schedule that runs for `duration_ms` with frames
/// `interval_ms` apart, the first at the start: the duration divided by the
/// interval, rounded up. Every 5 minutes for 24 hours is 288 frames, the last
/// at 23:55.
pub fn count_for(duration_ms: u64, interval_ms: u64) -> Result<u32> {
    ensure!(
        (1..=MAX_DURATION_MS).contains(&duration_ms),
        "duration must be 1ms..10 years"
    );
    ensure!(
        (1..=MAX_DURATION_MS).contains(&interval_ms),
        "interval must be 1ms..10 years"
    );
    frame_count(duration_ms.div_ceil(interval_ms))
}
/// The frame count of a schedule whose first frame is at `first_at_ms`, with
/// frames `interval_ms` apart until `end_at_ms`: every frame at or before the
/// end, so an end exactly on a frame includes that frame.
pub fn count_until(first_at_ms: u64, interval_ms: u64, end_at_ms: u64) -> Result<u32> {
    ensure!(
        (1..=MAX_DURATION_MS).contains(&interval_ms),
        "interval must be 1ms..10 years"
    );
    let span = end_at_ms
        .checked_sub(first_at_ms)
        .context("schedule end must not be before its first frame")?;
    frame_count((span / interval_ms).saturating_add(1))
}
fn frame_count(count: u64) -> Result<u32> {
    ensure!(
        (1..=100_000).contains(&count),
        "the schedule needs {count} frames; count must be 1..100000"
    );
    Ok(count as u32)
}
/// A duration in words for messages and labels: "500 ms", "1 min 30 s",
/// "1 h". Every nonzero unit is kept, so the text is exact.
pub fn format_duration(duration_ms: u64) -> String {
    if duration_ms == 0 {
        return "0 ms".into();
    }
    let units = [
        (86_400_000, "d"),
        (3_600_000, "h"),
        (60_000, "min"),
        (1000, "s"),
        (1, "ms"),
    ];
    let mut rest = duration_ms;
    let mut parts = Vec::new();
    for (size, unit) in units {
        if rest >= size {
            parts.push(format!("{} {unit}", rest / size));
            rest %= size;
        }
    }
    parts.join(" ")
}
pub fn parse_at(s: &str) -> Result<u64> {
    let timestamp = chrono::DateTime::parse_from_rfc3339(s).context(
        "--at needs an RFC3339 time with timezone, for example 2026-10-05T09:00:00-04:00",
    )?;
    ensure!(
        timestamp.timestamp_subsec_nanos().is_multiple_of(1_000_000),
        "schedule time must resolve to a whole number of milliseconds"
    );
    let timestamp = timestamp.timestamp_millis();
    ensure!(timestamp >= 0, "schedule time precedes Unix epoch");
    Ok(timestamp as u64)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duration_and_timezone_contract() {
        assert_eq!(parse_duration("1.5m").unwrap(), 90_000);
        assert_eq!(parse_duration("500ms").unwrap(), 500);
        assert_eq!(parse_duration("1.001s").unwrap(), 1001);
        assert_eq!(parse_duration(".003s").unwrap(), 3);
        assert_eq!(parse_duration("1.0000000000000000000000ms").unwrap(), 1);
        assert_eq!(
            parse_at("2026-10-05T09:00:00-04:00").unwrap(),
            parse_at("2026-10-05T13:00:00Z").unwrap()
        );
        for s in [
            "0",
            "NaN",
            "-1s",
            "1year",
            "100000d",
            "1.1ms",
            ".0001s",
            "1..0s",
            "999999999999999999999999999999999999999999999s",
        ] {
            assert!(parse_duration(s).is_err());
        }
        assert!(parse_at("2026-10-05T09:00:00").is_err());
        assert!(parse_at("2026-10-05T09:00:00.1234Z").is_err());
        assert!(parse_at("2026-10-05T09:00:00.000000001Z").is_err());
        assert_eq!(
            parse_at("2026-10-05T09:00:00.123000Z").unwrap()
                - parse_at("2026-10-05T09:00:00Z").unwrap(),
            123
        );
    }
    #[test]
    fn schedules_validate_every_frame_and_overflow() {
        let now = now_ms();
        assert!(validate_schedule(now, 0, 100_000).is_ok());
        assert!(validate_schedule(now, MAX_DURATION_MS, 2).is_ok());
        assert!(validate_schedule(now, MAX_DURATION_MS, 3).is_err());
        assert!(validate_schedule(now, u64::MAX, 2).is_err());
        assert!(validate_schedule(u64::MAX, 1, 2).is_err());
        assert!(validate_schedule(now, 1, 0).is_err());
    }
    #[test]
    fn frame_counts_cover_a_duration_or_run_until_an_end() {
        let minute = 60_000;
        let hour = 60 * minute;
        // Every 5 minutes for a day: the last frame is at 23:55.
        assert_eq!(count_for(24 * hour, 5 * minute).unwrap(), 288);
        assert_eq!(count_for(minute, minute).unwrap(), 1);
        assert_eq!(count_for(minute + 1, minute).unwrap(), 2);
        assert_eq!(count_for(1, MAX_DURATION_MS).unwrap(), 1);
        assert_eq!(count_for(100_000, 1).unwrap(), 100_000);
        for (duration, interval) in [(0, 1), (1, 0), (100_001, 1), (MAX_DURATION_MS + 1, 1)] {
            assert!(
                count_for(duration, interval).is_err(),
                "{duration}/{interval}"
            );
        }
        assert!(
            count_for(1_000_000, 1)
                .unwrap_err()
                .to_string()
                .contains("needs 1000000 frames")
        );
        // Hourly from 09:00 until 17:00 includes the 17:00 frame.
        let first = 1_000_000;
        assert_eq!(count_until(first, hour, first + 8 * hour).unwrap(), 9);
        assert_eq!(count_until(first, hour, first + 9 * hour - 1).unwrap(), 9);
        assert_eq!(count_until(first, hour, first).unwrap(), 1);
        assert_eq!(count_until(first, 1, first + 99_999).unwrap(), 100_000);
        assert!(count_until(first, 1, first + 100_000).is_err());
        assert!(count_until(first, 0, first + hour).is_err());
        assert!(count_until(first, hour, first - 1).is_err());
        assert!(count_until(0, 1, u64::MAX).is_err());
    }
    #[test]
    fn durations_read_in_exact_units() {
        assert_eq!(format_duration(0), "0 ms");
        assert_eq!(format_duration(500), "500 ms");
        assert_eq!(format_duration(1000), "1 s");
        assert_eq!(format_duration(90_000), "1 min 30 s");
        assert_eq!(format_duration(3_600_000), "1 h");
        assert_eq!(format_duration(86_400_000 + 1001), "1 d 1 s 1 ms");
    }
}
