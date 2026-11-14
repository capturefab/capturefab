//! Unit-test time scale, the same contract as `tests/support/time.rs`:
//! `CAPTUREFAB_TEST_TIME_SCALE` (integer 1–50, default 1) multiplies the
//! budgets, delays and bounds a timing-sensitive test routes through it.
use std::{sync::OnceLock, time::Duration};

pub(crate) fn scale() -> u64 {
    static SCALE: OnceLock<u64> = OnceLock::new();
    *SCALE.get_or_init(|| {
        std::env::var("CAPTUREFAB_TEST_TIME_SCALE")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|value| (1..=50).contains(value))
            .unwrap_or(1)
    })
}
pub(crate) fn ms(value: u64) -> u64 {
    value * scale()
}
pub(crate) fn millis(value: u64) -> Duration {
    Duration::from_millis(ms(value))
}
