//! Arguments several commands share: camera selection, and the duration and
//! time grammars.
use crate::types::{Categorized, ErrorKind};
use clap::Args;
use std::time::Duration;

/// One camera.
#[derive(Debug, Clone, Default, Args)]
pub struct CameraArg {
    /// Camera: list ID, serial, IP, rtsp://…, file path, sim:0 (default: the only one)
    #[arg(
        short = 'c',
        long = "camera",
        value_name = "CAM",
        env = "CAPTUREFAB_CAMERA",
        hide_env_values = true
    )]
    pub camera: Option<String>,
}

/// One or more cameras, `-c` repeated.
#[derive(Debug, Clone, Default, Args)]
pub struct CamerasArg {
    /// Camera, repeatable: list ID, serial, IP, rtsp://…, file path, sim:0 (default: the only one)
    #[arg(
        short = 'c',
        long = "camera",
        value_name = "CAM",
        env = "CAPTUREFAB_CAMERA",
        hide_env_values = true,
        value_delimiter = None
    )]
    pub cameras: Vec<String>,
}

/// `--keep DUR`: run through a session that stays this long after its
/// last use, started if none holds the camera.
#[derive(Debug, Clone, Default, Args)]
pub struct KeepArg {
    /// Keep the camera open in a session for this long after the command, so the next one starts at once (5m for interactive loops)
    #[arg(
        long,
        value_name = "DUR",
        env = "CAPTUREFAB_KEEP",
        value_parser = duration
    )]
    pub keep: Option<Duration>,
}
impl KeepArg {
    pub fn route(&self) -> super::Route {
        super::Route::keep(self.keep.filter(|keep| !keep.is_zero()))
    }
}

fn typed(error: anyhow::Error) -> Categorized {
    error
        .downcast::<Categorized>()
        .unwrap_or_else(|error| Categorized::new(ErrorKind::Usage, format!("{error:#}")))
}

/// A duration with a unit (`250ms`, `2s`, `1h30m`) or `0`.
pub fn duration(text: &str) -> Result<Duration, Categorized> {
    crate::types::parse_duration(text).map_err(typed)
}

/// A wait that may also be `none`: no limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    For(Duration),
    Forever,
}
pub fn wait(text: &str) -> Result<Wait, Categorized> {
    if text.trim().eq_ignore_ascii_case("none") {
        return Ok(Wait::Forever);
    }
    duration(text).map(Wait::For)
}

/// A time: `HH:MM[:SS]` in this computer's local time (today, or tomorrow
/// once that time has passed), or RFC 3339. The result is a UTC instant,
/// so a session in another time zone runs it at the same moment.
pub fn time(text: &str) -> Result<chrono::DateTime<chrono::Utc>, Categorized> {
    use chrono::TimeZone;
    let text = text.trim();
    if let Ok(time) = chrono::DateTime::parse_from_rfc3339(text) {
        return Ok(time.with_timezone(&chrono::Utc));
    }
    let clock = chrono::NaiveTime::parse_from_str(text, "%H:%M:%S")
        .or_else(|_| chrono::NaiveTime::parse_from_str(text, "%H:%M"))
        .map_err(|_| {
            Categorized::new(
                ErrorKind::Usage,
                format!("'{text}' is not a time; use HH:MM, HH:MM:SS or RFC 3339 such as 2026-10-31T14:00:00Z"),
            )
        })?;
    let now = chrono::Local::now();
    let mut date = now.date_naive();
    if clock <= now.time() {
        date = date.succ_opt().unwrap_or(date);
    }
    chrono::Local
        .from_local_datetime(&date.and_time(clock))
        .earliest()
        .map(|time| time.with_timezone(&chrono::Utc))
        .ok_or_else(|| {
            Categorized::new(
                ErrorKind::Usage,
                format!("{text} does not exist today in this time zone"),
            )
        })
}

/// A duration as the grammar writes it: `1h30m`, `2s`, `250ms`.
pub fn duration_text(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis == 0 {
        return if duration.is_zero() {
            "0".into()
        } else {
            format!("{}us", duration.as_micros())
        };
    }
    if !millis.is_multiple_of(1000) {
        return format!("{millis}ms");
    }
    let mut seconds = millis / 1000;
    let mut text = String::new();
    for (unit, size) in [("h", 3600), ("m", 60), ("s", 1)] {
        if seconds >= size {
            text.push_str(&format!("{}{unit}", seconds / size));
            seconds %= size;
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_take_none_and_durations_need_units() {
        assert_eq!(wait("none").unwrap(), Wait::Forever);
        assert_eq!(wait("2s").unwrap(), Wait::For(Duration::from_secs(2)));
        assert_eq!(wait("5000").unwrap_err().kind, ErrorKind::InvalidDuration);
        assert_eq!(duration_text(Duration::from_secs(5400)), "1h30m");
        assert_eq!(duration_text(Duration::from_millis(250)), "250ms");
        assert_eq!(duration_text(Duration::ZERO), "0");
    }

    #[test]
    fn times_are_utc_instants() {
        let at = time("2026-10-31T14:00:00+01:00").unwrap();
        assert_eq!(at.to_rfc3339(), "2026-10-31T13:00:00+00:00");
        let soon = time("23:59").unwrap();
        assert!(soon > chrono::Utc::now() - chrono::Duration::seconds(1));
        assert!(time("25:00").is_err());
        assert!(time("tomorrow").is_err());
    }
}
