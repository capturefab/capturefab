//! Bounded session-lifetime capture schedules. Clock times use UTC Unix milliseconds.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_DURATION_MS: u64 = 315_576_000_000;

/// Where a capture job is: waiting for its next shot, taking one, or how
/// it ended.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureStatus {
    /// Waiting until its next shot is due.
    #[default]
    Pending,
    /// Waiting for a shot's frame, or for its last shots to be saved.
    Running,
    /// Every frame asked for was saved.
    Done,
    Failed,
    /// Stopped by a command.
    Cancelled,
    /// Ended by the worker shutting down.
    Interrupted,
}
impl CaptureStatus {
    /// How the job ended, as the protocol reports it; None while it runs.
    pub(crate) fn ended(self) -> Option<crate::session::protocol::JobStatus> {
        use crate::session::protocol::JobStatus;
        Some(match self {
            Self::Pending | Self::Running => return None,
            Self::Done => JobStatus::Done,
            Self::Failed => JobStatus::Failed,
            Self::Cancelled => JobStatus::Cancelled,
            Self::Interrupted => JobStatus::Interrupted,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CaptureJob {
    pub id: u64,
    pub status: CaptureStatus,
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
        self.status.ended().is_none()
    }
    /// Stop the job as `status` (failed, cancelled or interrupted), with
    /// the failure.
    pub fn end(&mut self, status: CaptureStatus, error: Option<&anyhow::Error>) {
        self.status = status;
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
fn frame_count(count: u64) -> Result<u32> {
    ensure!(
        (1..=100_000).contains(&count),
        "the schedule needs {count} frames; count must be 1..100000"
    );
    Ok(count as u32)
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
    }
}
