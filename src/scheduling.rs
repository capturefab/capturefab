//! Bounded session-lifetime capture schedules. Clock times use UTC Unix milliseconds.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureJob {
    pub id: u64,
    pub status: String,
    pub output: String,
    pub count: u32,
    pub captured: u32,
    pub first_at_ms: u64,
    pub interval_ms: u64,
    pub next_at_ms: u64,
    pub last_file: Option<String>,
    pub error: Option<String>,
}
impl CaptureJob {
    pub fn active(&self) -> bool {
        matches!(self.status.as_str(), "pending" | "running")
    }
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
    let number = value.parse::<f64>().context(
        "duration needs a number and optional ms/s/m/h/d suffix (for example 30s or 5m)",
    )?;
    let factor = match unit {
        "ms" => 1.0,
        "" | "s" => 1000.0,
        "m" => 60_000.0,
        "h" => 3_600_000.0,
        "d" => 86_400_000.0,
        _ => anyhow::bail!("duration unit must be ms, s, m, h or d"),
    };
    let millis = number * factor;
    ensure!(
        millis.is_finite() && (1.0..=315_576_000_000.0).contains(&millis),
        "duration must be 1ms..10 years"
    );
    Ok(millis.round() as u64)
}
pub fn parse_at(s: &str) -> Result<u64> {
    let timestamp = chrono::DateTime::parse_from_rfc3339(s)
        .context("--at needs an RFC3339 time with timezone, for example 2026-10-05T09:00:00-04:00")?
        .timestamp_millis();
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
        assert_eq!(
            parse_at("2026-10-05T09:00:00-04:00").unwrap(),
            parse_at("2026-10-05T13:00:00Z").unwrap()
        );
        for s in ["0", "NaN", "-1s", "1year", "100000d"] {
            assert!(parse_duration(s).is_err());
        }
        assert!(parse_at("2026-10-05T09:00:00").is_err());
    }
}
