//! Persisted hard-limit defaults, shared by settings and the run-time limiter.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_rounds: u32,
    pub max_wall_ms: u64,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
    pub max_cost_usd: Option<f64>,
    pub max_images: u32,
    pub max_captures: u32,
    pub max_bytes: u64,
    pub max_recordings: u32,
    pub max_recording_ms: u64,
    pub max_setting_changes: u32,
    pub max_consecutive_errors: u32,
    pub max_identical_calls: u32,
    pub approval_timeout_ms: u64,
    pub min_round_interval_ms: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_rounds: 40,
            max_wall_ms: 1_800_000,
            max_input_tokens: 3_000_000,
            max_output_tokens: 150_000,
            max_cost_usd: None,
            max_images: 60,
            max_captures: 100,
            max_bytes: 2 * 1024 * 1024 * 1024,
            max_recordings: 2,
            max_recording_ms: 1_800_000,
            max_setting_changes: 300,
            max_consecutive_errors: 5,
            max_identical_calls: 3,
            approval_timeout_ms: 600_000,
            min_round_interval_ms: 1000,
        }
    }
}
impl Limits {
    /// Validate user defaults. The live storage ceiling is applied separately
    /// before a run and before every capture or recording.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut issues = Vec::new();
        for (valid, message) in [
            (
                (1..=1000).contains(&self.max_rounds),
                "Enter a round limit from 1 to 1,000.",
            ),
            (
                (60_000..=86_400_000).contains(&self.max_wall_ms),
                "Enter a time limit from 1 minute to 24 hours.",
            ),
            (
                (1..=50_000_000).contains(&self.max_input_tokens),
                "Enter an input-token limit from 1 to 50,000,000.",
            ),
            (
                (1..=5_000_000).contains(&self.max_output_tokens),
                "Enter an output-token limit from 1 to 5,000,000.",
            ),
            (
                self.max_cost_usd
                    .is_none_or(|cost| cost.is_finite() && (0.0..=500.0).contains(&cost)),
                "Enter a cost limit from 0 to 500 US dollars, or leave it empty.",
            ),
            (
                self.max_images <= 2000,
                "Enter an image limit from 0 to 2,000.",
            ),
            (
                self.max_captures <= 100_000,
                "Enter a capture limit from 0 to 100,000.",
            ),
            (
                self.max_bytes > 0,
                "Enter a positive data limit within the storage budget.",
            ),
            (
                self.max_recordings <= 32,
                "Enter a recording limit from 0 to 32.",
            ),
            (
                (1000..=604_800_000).contains(&self.max_recording_ms),
                "Enter a recording-duration limit from 1 second to 7 days.",
            ),
            (
                self.max_setting_changes <= 10_000,
                "Enter a settings-change limit from 0 to 10,000.",
            ),
            (
                (1..=5).contains(&self.max_consecutive_errors),
                "Enter a consecutive-error limit from 1 to 5.",
            ),
            (
                (1..=3).contains(&self.max_identical_calls),
                "Enter an identical-call limit from 1 to 3.",
            ),
            (
                (1000..=86_400_000).contains(&self.approval_timeout_ms),
                "Enter an approval timeout from 1 second to 24 hours.",
            ),
            (
                (1000..=86_400_000).contains(&self.min_round_interval_ms),
                "Wait at least 1 second between model rounds.",
            ),
        ] {
            if !valid {
                issues.push(message.into());
            }
        }
        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_and_ceilings_are_explicit() {
        assert!(Limits::default().validate().is_ok());
        let excessive = Limits {
            max_rounds: 1001,
            max_wall_ms: 86_400_001,
            max_cost_usd: Some(f64::INFINITY),
            max_images: 2001,
            max_captures: 100_001,
            max_recordings: 33,
            max_recording_ms: 604_800_001,
            max_setting_changes: 10_001,
            ..Limits::default()
        };
        assert_eq!(excessive.validate().unwrap_err().len(), 8);
    }
}
