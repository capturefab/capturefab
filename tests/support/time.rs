//! One time scale for every timing-sensitive test, so slow CI runners keep
//! the same ordering margins. `CAPTUREFAB_TEST_TIME_SCALE` (integer 1–50,
//! default 1) multiplies every budget, delay and elapsed-time bound a test
//! routes through these helpers; relations between them are unchanged.
#![allow(
    dead_code,
    reason = "each integration suite uses a different subset of these shared helpers"
)]
use std::{sync::OnceLock, time::Duration};

/// The configured multiplier.
pub fn scale() -> u64 {
    static SCALE: OnceLock<u64> = OnceLock::new();
    *SCALE.get_or_init(|| {
        std::env::var("CAPTUREFAB_TEST_TIME_SCALE")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|value| (1..=50).contains(value))
            .unwrap_or(1)
    })
}
/// `value` milliseconds scaled, as a number (for JSON `*_ms` fields).
pub fn ms(value: u64) -> u64 {
    value * scale()
}
/// `value` milliseconds scaled, as a duration (sleeps, deadlines, bounds).
pub fn millis(value: u64) -> Duration {
    Duration::from_millis(ms(value))
}
/// `value` seconds scaled, as a duration.
pub fn secs(value: u64) -> Duration {
    Duration::from_secs(value * scale())
}
/// A scaled CLI duration argument such as `--for`: `"<n>ms"`.
pub fn arg(value: u64) -> String {
    format!("{}ms", ms(value))
}
