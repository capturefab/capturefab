//! Bounded session-lifetime capture schedules. Clock times use UTC Unix milliseconds.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_DURATION_MS: u64 = 315_576_000_000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CaptureJob {
    pub id: u64,
    pub status: String,
    pub output: String,
    pub count: u32,
    pub captured: u32,
    /// Frames not taken because the encoders' queue was full.
    #[serde(default)]
    pub skipped: u32,
    /// Shots that failed for a reason that may pass, such as a network
    /// share that timed out, and were taken again at a later interval.
    #[serde(default)]
    pub retries: u32,
    pub first_at_ms: u64,
    pub interval_ms: u64,
    pub next_at_ms: u64,
    pub last_file: Option<String>,
    pub error: Option<String>,
    /// The error's reason, hint and details, so a client reports a failed
    /// job with the exit code a direct capture would have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_tag: Option<crate::types::ErrorTag>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub context: crate::context::Context,
    /// When the job was scheduled, UTC Unix milliseconds.
    #[serde(default)]
    pub created_ms: u64,
    /// When it completed, failed or was cancelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<u64>,
    /// The first [`MAX_JOB_FILES`] files saved, in order; `captured` counts
    /// them all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// The newest saved frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<JobFrame>,
    /// When the first saved frame arrived, UTC Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_ms: Option<u64>,
    /// The camera's acquisition settings when the first frame was saved.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub settings: serde_json::Map<String, serde_json::Value>,
    /// Frames were let pass because they were exposed before the camera's
    /// last settings change.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub after_change: bool,
    /// How long the first warm-up of a camera that sets its own exposure
    /// took, when the job started the camera.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warmup_ms: Option<u64>,
    /// Whether every warm-up settled before its limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled: Option<bool>,
    /// Frames the camera delivered while the job ran.
    #[serde(default)]
    pub frames: u64,
    /// Frames lost while it ran: those the encoders' queue had no room for,
    /// and those the transport lost on the way.
    #[serde(default)]
    pub dropped: u64,
    /// Frames that arrived incomplete while it ran, and packets the camera
    /// sent again, where the transport counts them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incomplete: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resent: Option<u64>,
    /// The first and the newest saved frame: block IDs, the camera's clock
    /// and when they arrived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_ts_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ts_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ms: Option<u64>,
    /// The last frame's measurements and preview, when asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inspect: Option<serde_json::Value>,
}
/// Files a job lists; a longer job keeps counting in `captured`, and its
/// newest file is `last_file`.
pub const MAX_JOB_FILES: usize = 1000;
/// The shape of a saved frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobFrame {
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
}
impl CaptureJob {
    pub fn active(&self) -> bool {
        matches!(self.status.as_str(), "pending" | "running")
    }
    /// Stop the job as `status` (failed or cancelled), with the failure.
    pub fn end(&mut self, status: &str, error: Option<&anyhow::Error>) {
        self.status = status.into();
        self.ended_ms = Some(now_ms());
        if let Some(error) = error {
            self.error = Some(format!("{error:#}"));
            self.error_tag = crate::types::error_tag(error);
        }
    }
}
/// A local recording in progress, kept by the worker until it stops so the
/// finished file can be reported with its context.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecordingMeta {
    pub path: String,
    /// UTC Unix milliseconds.
    pub since_ms: u64,
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
/// UTC Unix milliseconds as an RFC 3339 time, as results and job documents
/// show times.
pub fn rfc3339(ms: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(i64::try_from(ms).unwrap_or(i64::MAX))
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
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
#[cfg(test)]
mod tests {
    use super::*;
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
