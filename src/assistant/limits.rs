//! Runtime budgets are checked before requests and effects. Reservations are
//! conservative: a partially completed tool cannot reclaim its allowance.
pub use super::limits_defaults::Limits;
use super::{
    conversation::ImagePart,
    provider::{Protocol, ProviderId},
    usage::{Prices, Usage},
};
use crate::{
    session::SessionCommand,
    tools::{Checked, Op, ToolCall},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeSet, fmt};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    Short,
    Standard,
    Long,
}
impl Preset {
    pub fn limits(self) -> Limits {
        match self {
            Self::Short => Limits {
                max_wall_ms: 600_000,
                max_rounds: 15,
                max_images: 20,
                ..Limits::default()
            },
            Self::Standard => Limits::default(),
            Self::Long => Limits {
                max_wall_ms: 14_400_000,
                max_rounds: 300,
                max_images: 400,
                max_recording_ms: 7_200_000,
                ..Limits::default()
            },
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Totals {
    pub rounds: u32,
    pub usage: Usage,
    pub cost_usd: Option<f64>,
    pub images: u32,
    pub captures: u32,
    pub bytes: u64,
    pub recordings: u32,
    pub data_bytes: u64,
    pub setting_changes: u32,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remaining {
    pub rounds: u32,
    pub wall_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: Option<f64>,
    pub images: u32,
    pub captures: u32,
    pub bytes: u64,
    pub recordings: u32,
    pub data_bytes: u64,
    /// Maximum bundle limit after reserving both staged and published copies.
    pub export_bundle_bytes: u64,
    pub setting_changes: u32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RequestProjection {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// JSON text is estimated separately from pixels. Base64 is transport, not
/// language tokens; counting its characters would greatly overstate input.
pub fn projection_from_body(
    body: &Value,
    images: &[ImagePart],
    protocol: Protocol,
    output_tokens: u64,
) -> RequestProjection {
    let mut text = body.clone();
    fn remove_pixels(value: &mut Value) {
        match value {
            Value::Object(object) => {
                if object.get("type").and_then(Value::as_str) == Some("image")
                    && let Some(source) = object.get_mut("source").and_then(Value::as_object_mut)
                {
                    source.remove("data");
                }
                if let Some(inline) = object.get_mut("inlineData").and_then(Value::as_object_mut) {
                    inline.remove("data");
                }
                if let Some(image) = object.get_mut("image").and_then(Value::as_object_mut)
                    && let Some(source) = image.get_mut("source").and_then(Value::as_object_mut)
                {
                    source.remove("bytes");
                }
                if let Some(uri) = object.get_mut("image_url") {
                    match uri {
                        Value::String(text) if text.starts_with("data:image/") => {
                            *text = "[camera image]".into()
                        }
                        Value::Object(image)
                            if image
                                .get("url")
                                .and_then(Value::as_str)
                                .is_some_and(|url| url.starts_with("data:image/")) =>
                        {
                            image.insert("url".into(), Value::String("[camera image]".into()));
                        }
                        _ => {}
                    }
                }
                for value in object.values_mut() {
                    remove_pixels(value);
                }
            }
            Value::Array(values) => {
                for value in values {
                    remove_pixels(value);
                }
            }
            _ => {}
        }
    }
    remove_pixels(&mut text);
    let text_bytes = serde_json::to_vec(&text).expect("JSON request").len();
    RequestProjection {
        input_tokens: project_input(0, text_bytes, images, protocol),
        output_tokens,
    }
}

pub fn project_input(
    last_total_input: u64,
    new_text_bytes: usize,
    images: &[ImagePart],
    protocol: Protocol,
) -> u64 {
    let text = u64::try_from(new_text_bytes.div_ceil(3)).unwrap_or(u64::MAX);
    images
        .iter()
        .fold(last_total_input.saturating_add(text), |total, image| {
            let tokens = if protocol == Protocol::Messages {
                u64::from(image.width.div_ceil(28))
                    .saturating_mul(u64::from(image.height.div_ceil(28)))
            } else {
                1600
            };
            total.saturating_add(tokens)
        })
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct ToolImpact {
    pub images: u32,
    pub captures: u32,
    pub bytes: u64,
    pub recordings: u32,
    pub recording_ms: Option<u64>,
    pub data_bytes: u64,
    /// Worst-case synchronous work; it may not overrun the task deadline.
    pub blocking_ms: u64,
    pub setting_changes: u32,
    pub ends_at_ms: Option<u64>,
}
impl ToolImpact {
    /// `bytes` is the checked PlanState reservation delta. The runtime adds
    /// images only when attaching a new observation or checkpoint image.
    pub fn from_checked(checked: &Checked, bytes: u64, now_ms: u64) -> Self {
        let mut impact = Self {
            bytes,
            ..Self::default()
        };
        fn command(
            impact: &mut ToolImpact,
            command: &SessionCommand,
            now_ms: u64,
            recording: bool,
        ) {
            match command {
                SessionCommand::Capture { count, .. } => {
                    impact.captures = impact.captures.saturating_add(*count)
                }
                SessionCommand::Schedule {
                    count,
                    first_at_ms,
                    interval_ms,
                    ..
                } => {
                    impact.captures = impact.captures.saturating_add(*count);
                    let ends = first_at_ms.saturating_add(
                        u64::from(count.saturating_sub(1)).saturating_mul(*interval_ms),
                    );
                    impact.ends_at_ms = Some(impact.ends_at_ms.unwrap_or(0).max(ends));
                }
                SessionCommand::Forward { duration_ms, .. } => {
                    if recording {
                        impact.recordings = impact.recordings.saturating_add(1);
                    }
                    if let Some(duration) = duration_ms {
                        impact.recording_ms = Some(impact.recording_ms.unwrap_or(0).max(*duration));
                        impact.ends_at_ms = Some(
                            impact
                                .ends_at_ms
                                .unwrap_or(0)
                                .max(now_ms.saturating_add(*duration)),
                        );
                    }
                }
                SessionCommand::SetMany { values } => {
                    impact.setting_changes = impact
                        .setting_changes
                        .saturating_add(values.len().try_into().unwrap_or(u32::MAX))
                }
                // A checked restore (`Op::ApplySetup`) records its change
                // count; a raw restore counts as the most a setup can change.
                SessionCommand::ApplySetup { .. } => {
                    impact.setting_changes = impact.setting_changes.saturating_add(
                        crate::setup::MAX_SETUP_VALUES
                            .try_into()
                            .unwrap_or(u32::MAX),
                    );
                    impact.blocking_ms = impact.blocking_ms.saturating_add(240_000);
                }
                SessionCommand::Set { .. }
                | SessionCommand::Auto { .. }
                | SessionCommand::Manual { .. } => {
                    impact.setting_changes = impact.setting_changes.saturating_add(1)
                }
                SessionCommand::DatasetExportIf {
                    max_bytes,
                    timeout_ms,
                    ..
                } => {
                    impact.data_bytes = impact
                        .data_bytes
                        .saturating_add(max_bytes.saturating_mul(2));
                    impact.blocking_ms = impact.blocking_ms.saturating_add(*timeout_ms);
                }
                _ => {}
            }
        }
        let recording = checked.call.tool == "start_recording";
        for op in &checked.ops {
            match op {
                Op::Command { command: cmd, .. }
                | Op::Stream { command: cmd, .. }
                | Op::Session { command: cmd, .. } => command(&mut impact, cmd, now_ms, recording),
                Op::Restartable { commands, .. } => {
                    for cmd in commands {
                        command(&mut impact, cmd, now_ms, recording);
                    }
                }
                Op::ApplySetup { changes, .. } => {
                    impact.setting_changes = impact
                        .setting_changes
                        .saturating_add((*changes).try_into().unwrap_or(u32::MAX));
                    impact.blocking_ms = impact.blocking_ms.saturating_add(240_000);
                }
                _ => {}
            }
        }
        impact
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct LimitReached {
    pub name: &'static str,
    pub used: f64,
    pub max: f64,
}
impl fmt::Display for LimitReached {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "The assistant task reached its {} limit.",
            label(self.name)
        )
    }
}
impl std::error::Error for LimitReached {}

#[derive(Clone, Debug, Serialize)]
pub struct LimitNotice {
    pub name: &'static str,
    pub used: f64,
    pub max: f64,
    pub message: String,
}

#[derive(Debug)]
pub struct LimitConfigError {
    pub issues: Vec<String>,
}
impl fmt::Display for LimitConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.issues.join(" "))
    }
}
impl std::error::Error for LimitConfigError {}

fn label(name: &str) -> &str {
    match name {
        "wall" => "time",
        "input_tokens" => "input-token",
        "output_tokens" => "output-token",
        "cost" => "estimated cost",
        "bytes" => "data",
        "recording_ms" => "recording-duration",
        "data_bytes" => "catalog export bytes",
        "setting_changes" => "settings-change",
        "consecutive_errors" => "consecutive-error",
        "identical_calls" => "identical-action",
        other => other,
    }
}
fn bound(name: &'static str, used: u64, max: u64) -> Result<(), LimitReached> {
    if used > max {
        Err(LimitReached {
            name,
            used: used as f64,
            max: max as f64,
        })
    } else {
        Ok(())
    }
}
fn bound_sum(name: &'static str, used: u64, added: u64, max: u64) -> Result<(), LimitReached> {
    if used > max || added > max.saturating_sub(used) {
        Err(LimitReached {
            name,
            used: used as f64 + added as f64,
            max: max as f64,
        })
    } else {
        Ok(())
    }
}

pub struct Limiter {
    pub limits: Limits,
    pub totals: Totals,
    start_ms: u64,
    prices: Option<Prices>,
    noticed: BTreeSet<&'static str>,
    last_call: Option<ToolCall>,
    identical: u32,
    consecutive_errors: u32,
}
impl Limiter {
    pub fn new(
        limits: Limits,
        start_ms: u64,
        storage_ceiling: u64,
        prices: Option<Prices>,
    ) -> Result<Self, LimitConfigError> {
        let mut issues = limits.validate().err().unwrap_or_default();
        if limits.max_bytes > storage_ceiling {
            issues.push(
                "Choose a data limit within the remaining storage budget and free disk space."
                    .into(),
            );
        }
        if prices.is_some_and(|prices| prices.validate().is_err()) {
            issues
                .push("Enter finite, nonnegative prices in US dollars per million tokens.".into());
        }
        if limits.max_cost_usd.is_some() && prices.is_none() {
            issues
                .push("Set prices for this model before choosing an estimated cost limit.".into());
        }
        if !issues.is_empty() {
            return Err(LimitConfigError { issues });
        }
        Ok(Self {
            limits,
            totals: Totals::default(),
            start_ms,
            prices,
            noticed: BTreeSet::new(),
            last_call: None,
            identical: 0,
            consecutive_errors: 0,
        })
    }
    pub fn deadline_ms(&self) -> u64 {
        self.start_ms.saturating_add(self.limits.max_wall_ms)
    }
    pub fn remaining_wall_ms(&self, now_ms: u64) -> u64 {
        self.deadline_ms().saturating_sub(now_ms)
    }
    pub fn remaining(&self, now_ms: u64) -> Remaining {
        let bytes = self.limits.max_bytes.saturating_sub(self.totals.bytes);
        let data_bytes = self
            .limits
            .max_data_bytes
            .saturating_sub(self.totals.data_bytes);
        Remaining {
            rounds: self.limits.max_rounds.saturating_sub(self.totals.rounds),
            wall_ms: self.remaining_wall_ms(now_ms),
            input_tokens: self
                .limits
                .max_input_tokens
                .saturating_sub(self.totals.usage.total_input()),
            output_tokens: self
                .limits
                .max_output_tokens
                .saturating_sub(self.totals.usage.output),
            cost_usd: self
                .limits
                .max_cost_usd
                .map(|max| (max - self.totals.cost_usd.unwrap_or(0.0)).max(0.0)),
            images: self.limits.max_images.saturating_sub(self.totals.images),
            captures: self
                .limits
                .max_captures
                .saturating_sub(self.totals.captures),
            bytes,
            recordings: self
                .limits
                .max_recordings
                .saturating_sub(self.totals.recordings),
            data_bytes,
            export_bundle_bytes: bytes.min(data_bytes) / 2,
            setting_changes: self
                .limits
                .max_setting_changes
                .saturating_sub(self.totals.setting_changes),
        }
    }
    pub fn check_wall(&self, now_ms: u64) -> Result<(), LimitReached> {
        if now_ms >= self.deadline_ms() {
            Err(LimitReached {
                name: "wall",
                used: now_ms.saturating_sub(self.start_ms) as f64,
                max: self.limits.max_wall_ms as f64,
            })
        } else {
            Ok(())
        }
    }
    pub fn check_request(
        &self,
        now_ms: u64,
        projection: &RequestProjection,
    ) -> Result<(), LimitReached> {
        self.check_totals(now_ms)?;
        bound(
            "rounds",
            u64::from(self.totals.rounds).saturating_add(1),
            u64::from(self.limits.max_rounds),
        )?;
        bound(
            "input_tokens",
            self.totals
                .usage
                .total_input()
                .saturating_add(projection.input_tokens),
            self.limits.max_input_tokens,
        )?;
        bound(
            "output_tokens",
            self.totals
                .usage
                .output
                .saturating_add(projection.output_tokens),
            self.limits.max_output_tokens,
        )?;
        if let Some(max) = self.limits.max_cost_usd {
            let projected = Usage {
                input: projection.input_tokens,
                output: projection.output_tokens,
                ..Usage::default()
            }
            .estimated_cost(self.prices.as_ref())
            .unwrap_or(f64::MAX);
            let used = (self.totals.cost_usd.unwrap_or(0.0) + projected).min(f64::MAX);
            if used > max {
                return Err(LimitReached {
                    name: "cost",
                    used,
                    max,
                });
            }
        }
        Ok(())
    }
    pub fn begin_request(
        &mut self,
        now_ms: u64,
        projection: &RequestProjection,
    ) -> Result<Vec<LimitNotice>, LimitReached> {
        self.check_request(now_ms, projection)?;
        self.totals.rounds = self.totals.rounds.saturating_add(1);
        Ok(self.notices(now_ms))
    }
    pub fn record_usage(&mut self, usage: Usage) {
        let cost = usage
            .estimated_cost(self.prices.as_ref())
            .or_else(|| self.prices.map(|_| f64::MAX));
        self.totals.cost_usd = match (self.totals.cost_usd, cost) {
            (None, Some(cost)) if self.totals.usage.total_tokens() == 0 => Some(cost),
            (Some(total), Some(cost)) => Some(if (total + cost).is_finite() {
                total + cost
            } else {
                f64::MAX
            }),
            _ => None,
        };
        self.totals.usage += usage;
    }
    pub fn check_totals(&self, now_ms: u64) -> Result<(), LimitReached> {
        self.check_wall(now_ms)?;
        bound(
            "input_tokens",
            self.totals.usage.total_input(),
            self.limits.max_input_tokens,
        )?;
        bound(
            "output_tokens",
            self.totals.usage.output,
            self.limits.max_output_tokens,
        )?;
        if let (Some(used), Some(max)) = (self.totals.cost_usd, self.limits.max_cost_usd)
            && used > max
        {
            return Err(LimitReached {
                name: "cost",
                used,
                max,
            });
        }
        Ok(())
    }
    pub fn check_tool(&self, now_ms: u64, impact: &ToolImpact) -> Result<(), LimitReached> {
        self.check_totals(now_ms)?;
        bound_sum(
            "wall",
            now_ms.saturating_sub(self.start_ms),
            impact.blocking_ms,
            self.limits.max_wall_ms,
        )?;
        for (name, used, added, max) in [
            (
                "images",
                self.totals.images,
                impact.images,
                self.limits.max_images,
            ),
            (
                "captures",
                self.totals.captures,
                impact.captures,
                self.limits.max_captures,
            ),
            (
                "recordings",
                self.totals.recordings,
                impact.recordings,
                self.limits.max_recordings,
            ),
            (
                "setting_changes",
                self.totals.setting_changes,
                impact.setting_changes,
                self.limits.max_setting_changes,
            ),
        ] {
            bound(
                name,
                u64::from(used).saturating_add(u64::from(added)),
                u64::from(max),
            )?;
        }
        bound_sum(
            "bytes",
            self.totals.bytes,
            impact.bytes,
            self.limits.max_bytes,
        )?;
        bound_sum(
            "data_bytes",
            self.totals.data_bytes,
            impact.data_bytes,
            self.limits.max_data_bytes,
        )?;
        if let Some(duration) = impact.recording_ms {
            bound("recording_ms", duration, self.limits.max_recording_ms)?;
        }
        Ok(())
    }
    pub fn record_tool(&mut self, impact: ToolImpact) {
        self.totals.images = self.totals.images.saturating_add(impact.images);
        self.totals.captures = self.totals.captures.saturating_add(impact.captures);
        self.totals.bytes = self.totals.bytes.saturating_add(impact.bytes);
        self.totals.recordings = self.totals.recordings.saturating_add(impact.recordings);
        self.totals.data_bytes = self.totals.data_bytes.saturating_add(impact.data_bytes);
        self.totals.setting_changes = self
            .totals
            .setting_changes
            .saturating_add(impact.setting_changes);
    }
    pub fn record_call(&mut self, call: &ToolCall) -> Result<(), LimitReached> {
        if self.last_call.as_ref() == Some(call) {
            self.identical = self.identical.saturating_add(1);
        } else {
            self.last_call = Some(call.clone());
            self.identical = 1;
        }
        bound(
            "identical_calls",
            u64::from(self.identical),
            u64::from(self.limits.max_identical_calls),
        )
    }
    pub fn record_result(&mut self, success: bool) -> Result<(), LimitReached> {
        self.consecutive_errors = if success {
            0
        } else {
            self.consecutive_errors.saturating_add(1)
        };
        if self.consecutive_errors >= self.limits.max_consecutive_errors {
            Err(LimitReached {
                name: "consecutive_errors",
                used: f64::from(self.consecutive_errors),
                max: f64::from(self.limits.max_consecutive_errors),
            })
        } else {
            Ok(())
        }
    }
    pub fn notices(&mut self, now_ms: u64) -> Vec<LimitNotice> {
        let mut meters = vec![
            (
                "rounds",
                f64::from(self.totals.rounds),
                f64::from(self.limits.max_rounds),
            ),
            (
                "wall",
                now_ms.saturating_sub(self.start_ms) as f64,
                self.limits.max_wall_ms as f64,
            ),
            (
                "input_tokens",
                self.totals.usage.total_input() as f64,
                self.limits.max_input_tokens as f64,
            ),
            (
                "output_tokens",
                self.totals.usage.output as f64,
                self.limits.max_output_tokens as f64,
            ),
            (
                "images",
                f64::from(self.totals.images),
                f64::from(self.limits.max_images),
            ),
            (
                "captures",
                f64::from(self.totals.captures),
                f64::from(self.limits.max_captures),
            ),
            (
                "bytes",
                self.totals.bytes as f64,
                self.limits.max_bytes as f64,
            ),
            (
                "recordings",
                f64::from(self.totals.recordings),
                f64::from(self.limits.max_recordings),
            ),
            (
                "data_bytes",
                self.totals.data_bytes as f64,
                self.limits.max_data_bytes as f64,
            ),
            (
                "setting_changes",
                f64::from(self.totals.setting_changes),
                f64::from(self.limits.max_setting_changes),
            ),
        ];
        if let (Some(cost), Some(max)) = (self.totals.cost_usd, self.limits.max_cost_usd) {
            meters.push(("cost", cost, max));
        }
        meters
            .into_iter()
            .filter(|(name, used, max)| {
                *max > 0.0 && *used >= *max * 0.8 && self.noticed.insert(*name)
            })
            .map(|(name, used, max)| LimitNotice {
                name,
                used,
                max,
                message: format!("80% of the {} limit used; finish soon.", label(name)),
            })
            .collect()
    }
}

/// Converse uses the same patch formula for Claude even though it speaks a
/// different wire protocol. Other model families use the conservative 1,600.
pub fn projection_for_provider(
    body: &Value,
    images: &[ImagePart],
    provider: ProviderId,
    model: &str,
    output_tokens: u64,
) -> RequestProjection {
    let protocol = if provider == ProviderId::Bedrock && model.contains("anthropic.claude") {
        Protocol::Messages
    } else {
        super::protocol::protocol_for(provider, model)
    };
    let mut projection = projection_from_body(body, images, protocol, output_tokens);
    if provider.is_on_device() {
        // Tool selection and argument generation each replay the conversation.
        // Include both passes and their small guided-output schemas.
        projection.input_tokens = projection
            .input_tokens
            .saturating_mul(2)
            .saturating_add(2048);
    }
    projection
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        assistant::{
            protocol::{image_base64, test_image},
            usage::Prices,
        },
        session::FeatureValue,
        tools::{CameraRef, StepId},
    };
    use serde_json::json;

    fn limiter(limits: Limits) -> Limiter {
        Limiter::new(limits, 100, 100 * 1024 * 1024 * 1024, None).unwrap()
    }
    fn prices() -> Prices {
        Prices {
            input: 1.0,
            output: 2.0,
            cache_read: 0.1,
            cache_write: 1.2,
        }
    }

    #[test]
    fn presets_and_configuration_respect_storage_and_known_price_rules() {
        for preset in [Preset::Short, Preset::Standard, Preset::Long] {
            assert!(preset.limits().validate().is_ok());
        }
        assert_eq!(Preset::Short.limits().max_wall_ms, 600_000);
        assert_eq!(Preset::Long.limits().max_images, 400);
        assert!(Limiter::new(Limits::default(), 0, 1, None).is_err());
        let budget = Limits {
            max_cost_usd: Some(1.0),
            ..Limits::default()
        };
        assert!(Limiter::new(budget.clone(), 0, u64::MAX, None).is_err());
        assert!(Limiter::new(budget, 0, u64::MAX, Some(prices())).is_ok());
    }

    #[test]
    fn requests_reserve_rounds_and_project_input_output_and_cost_before_sending() {
        let mut rounds = limiter(Limits {
            max_rounds: 1,
            ..Limits::default()
        });
        let projection = RequestProjection {
            input_tokens: 10,
            output_tokens: 20,
        };
        rounds.begin_request(101, &projection).unwrap();
        assert_eq!(
            rounds.begin_request(102, &projection).unwrap_err().name,
            "rounds"
        );
        assert_eq!(rounds.totals.rounds, 1);
        let input = limiter(Limits {
            max_input_tokens: 9,
            ..Limits::default()
        });
        assert_eq!(
            input.check_request(101, &projection).unwrap_err().name,
            "input_tokens"
        );
        let output = limiter(Limits {
            max_output_tokens: 19,
            ..Limits::default()
        });
        assert_eq!(
            output.check_request(101, &projection).unwrap_err().name,
            "output_tokens"
        );
        let cost = Limiter::new(
            Limits {
                max_cost_usd: Some(0.000049),
                ..Limits::default()
            },
            100,
            u64::MAX,
            Some(prices()),
        )
        .unwrap();
        assert_eq!(
            cost.check_request(101, &projection).unwrap_err().name,
            "cost"
        );
        let mut cumulative = limiter(Limits {
            max_input_tokens: 100,
            ..Limits::default()
        });
        cumulative.record_usage(Usage {
            input: 30,
            cache_read: 40,
            cache_write: 20,
            ..Usage::default()
        });
        assert!(
            cumulative
                .check_request(
                    101,
                    &RequestProjection {
                        input_tokens: 10,
                        output_tokens: 0
                    }
                )
                .is_ok()
        );
        assert_eq!(
            cumulative
                .check_request(
                    101,
                    &RequestProjection {
                        input_tokens: 11,
                        output_tokens: 0
                    }
                )
                .unwrap_err()
                .name,
            "input_tokens"
        );
    }

    #[test]
    fn actual_normalized_usage_and_reported_cost_override_estimates() {
        let mut state = Limiter::new(
            Limits {
                max_cost_usd: Some(0.5),
                max_input_tokens: 100,
                max_output_tokens: 100,
                ..Limits::default()
            },
            100,
            u64::MAX,
            Some(prices()),
        )
        .unwrap();
        state.record_usage(Usage {
            input: 10,
            cache_read: 20,
            cache_write: 30,
            output: 40,
            reasoning: Some(20),
            provider_cost_usd: Some(0.1),
            ..Usage::default()
        });
        assert_eq!(state.totals.usage.total_input(), 60);
        assert_eq!(state.totals.usage.output, 40);
        assert_eq!(state.totals.cost_usd, Some(0.1));
        state.record_usage(Usage {
            input: 1,
            output: 1,
            provider_cost_usd: Some(0.41),
            ..Usage::default()
        });
        assert_eq!(state.check_totals(101).unwrap_err().name, "cost");
        let mut input = limiter(Limits {
            max_input_tokens: 100,
            ..Limits::default()
        });
        input.record_usage(Usage {
            input: 101,
            ..Usage::default()
        });
        assert_eq!(
            input
                .check_tool(101, &ToolImpact::default())
                .unwrap_err()
                .name,
            "input_tokens"
        );
        let mut output = limiter(Limits {
            max_output_tokens: 100,
            ..Limits::default()
        });
        output.record_usage(Usage {
            output: 101,
            ..Usage::default()
        });
        assert_eq!(output.check_totals(101).unwrap_err().name, "output_tokens");
        let mut huge = Limiter::new(
            Limits {
                max_cost_usd: Some(1.0),
                ..Limits::default()
            },
            100,
            u64::MAX,
            Some(Prices {
                input: f64::MAX,
                ..prices()
            }),
        )
        .unwrap();
        huge.record_usage(Usage {
            input: 2,
            ..Usage::default()
        });
        let error = huge.check_totals(101).unwrap_err();
        assert_eq!(error.name, "cost");
        assert!(error.used.is_finite());
    }

    #[test]
    fn every_tool_limit_is_checked_before_reservations_and_failure_cannot_refund() {
        let mut state = limiter(Limits {
            max_images: 2,
            max_captures: 3,
            max_bytes: 100,
            max_recordings: 1,
            max_recording_ms: 1000,
            max_setting_changes: 2,
            ..Limits::default()
        });
        for (impact, name) in [
            (
                ToolImpact {
                    images: 3,
                    ..Default::default()
                },
                "images",
            ),
            (
                ToolImpact {
                    captures: 4,
                    ..Default::default()
                },
                "captures",
            ),
            (
                ToolImpact {
                    bytes: 101,
                    ..Default::default()
                },
                "bytes",
            ),
            (
                ToolImpact {
                    recordings: 2,
                    ..Default::default()
                },
                "recordings",
            ),
            (
                ToolImpact {
                    recording_ms: Some(1001),
                    ..Default::default()
                },
                "recording_ms",
            ),
            (
                ToolImpact {
                    setting_changes: 3,
                    ..Default::default()
                },
                "setting_changes",
            ),
        ] {
            assert_eq!(state.check_tool(101, &impact).unwrap_err().name, name);
        }
        let impact = ToolImpact {
            images: 2,
            captures: 3,
            bytes: 100,
            recordings: 1,
            recording_ms: Some(1000),
            setting_changes: 2,
            ends_at_ms: None,
            ..Default::default()
        };
        state.check_tool(101, &impact).unwrap();
        state.record_tool(impact);
        state.record_result(false).unwrap();
        assert_eq!(
            state
                .check_tool(
                    102,
                    &ToolImpact {
                        captures: 1,
                        ..Default::default()
                    }
                )
                .unwrap_err()
                .name,
            "captures"
        );
        assert_eq!(state.totals.captures, 3);
        assert_eq!(state.totals.bytes, 100);
        let mut huge = Limiter::new(
            Limits {
                max_bytes: u64::MAX,
                ..Limits::default()
            },
            100,
            u64::MAX,
            None,
        )
        .unwrap();
        huge.record_tool(ToolImpact {
            bytes: u64::MAX,
            ..Default::default()
        });
        assert_eq!(
            huge.check_tool(
                101,
                &ToolImpact {
                    bytes: 1,
                    ..Default::default()
                }
            )
            .unwrap_err()
            .name,
            "bytes"
        );
    }

    #[test]
    fn export_reservations_count_both_copies_and_keep_failed_attempts_charged() {
        let mut checked = Checked::new(
            StepId(1),
            &ToolCall {
                tool: "export_dataset".into(),
                args: json!({"max_bytes":1}),
            },
        );
        checked.ops.push(Op::Session {
            command: SessionCommand::DatasetExportIf {
                slot_id: "1".repeat(32),
                output: "/data/export".into(),
                approved_root: Some("/data".into()),
                formats: vec![crate::dataset::ExportFormat::Csv],
                tables: Vec::new(),
                since_ms: None,
                until_ms: None,
                storage: Default::default(),
                max_bytes: 100,
                timeout_ms: 1000,
            },
            dataset_from_plan: false,
        });
        // PlanState already includes both copies. Runtime adds no third copy.
        let impact = ToolImpact::from_checked(&checked, 200, 100);
        assert_eq!(impact.bytes, 200);
        assert_eq!(impact.data_bytes, 200);
        for (max_bytes, max_data_bytes, rejected) in
            [(199, 1000, "bytes"), (1000, 199, "data_bytes")]
        {
            let state = limiter(Limits {
                max_bytes,
                max_data_bytes,
                ..Limits::default()
            });
            assert_eq!(state.check_tool(101, &impact).unwrap_err().name, rejected);
        }
        let mut state = limiter(Limits {
            max_bytes: 400,
            max_data_bytes: 400,
            ..Limits::default()
        });
        for expected in [200, 400] {
            state.check_tool(101, &impact).unwrap();
            // The runtime records reservations before execution. A failed
            // export or failed staging cleanup never refunds this charge.
            state.record_tool(impact);
            assert_eq!(state.totals.bytes, expected);
            assert_eq!(state.totals.data_bytes, expected);
        }
        assert_eq!(state.check_tool(101, &impact).unwrap_err().name, "bytes");
        assert_eq!(state.remaining(101).export_bundle_bytes, 0);
    }

    #[test]
    fn remaining_export_bundle_bytes_use_half_the_smaller_byte_allowance() {
        let mut state = limiter(Limits {
            max_bytes: 101,
            max_data_bytes: 80,
            ..Limits::default()
        });
        assert_eq!(state.remaining(101).export_bundle_bytes, 40);
        state.record_tool(ToolImpact {
            bytes: 50,
            data_bytes: 10,
            ..Default::default()
        });
        let remaining = state.remaining(101);
        assert_eq!(remaining.bytes, 51);
        assert_eq!(remaining.data_bytes, 70);
        assert_eq!(remaining.export_bundle_bytes, 25);
        assert_eq!(
            serde_json::to_value(remaining).unwrap()["export_bundle_bytes"],
            25
        );
    }

    #[test]
    fn wall_deadlines_include_sleep_and_notices_are_emitted_once_at_eighty_percent() {
        let mut state = limiter(Limits::default());
        assert_eq!(state.remaining_wall_ms(100), 1_800_000);
        assert!(state.check_wall(1_800_099).is_ok());
        assert_eq!(state.check_wall(1_800_100).unwrap_err().name, "wall");
        state.totals.rounds = 32;
        state.record_tool(ToolImpact {
            images: 48,
            captures: 80,
            // The 2 GiB cap is not divisible by five. Round up to the
            // first whole byte at or above 80%, rather than one below it.
            bytes: (state.limits.max_bytes * 4).div_ceil(5),
            recordings: 2,
            setting_changes: 240,
            ..Default::default()
        });
        state.record_usage(Usage {
            input: 2_400_000,
            output: 120_000,
            ..Usage::default()
        });
        let notices = state.notices(100 + 1_440_000);
        let names = notices
            .iter()
            .map(|notice| notice.name)
            .collect::<BTreeSet<_>>();
        for name in [
            "rounds",
            "wall",
            "input_tokens",
            "output_tokens",
            "images",
            "captures",
            "bytes",
            "recordings",
            "setting_changes",
        ] {
            assert!(names.contains(name), "{name}");
        }
        assert!(state.notices(100 + 1_500_000).is_empty());
    }

    #[test]
    fn repeated_calls_and_consecutive_errors_have_separate_exact_guards() {
        let mut state = limiter(Limits::default());
        let call = ToolCall {
            tool: "get_overview".into(),
            args: json!({}),
        };
        for _ in 0..3 {
            state.record_call(&call).unwrap();
        }
        assert_eq!(
            state.record_call(&call).unwrap_err().name,
            "identical_calls"
        );
        let different = ToolCall {
            tool: "list_jobs".into(),
            args: json!({"camera":"sim:0"}),
        };
        state.record_call(&different).unwrap();
        for _ in 0..4 {
            state.record_result(false).unwrap();
        }
        assert_eq!(
            state.record_result(false).unwrap_err().name,
            "consecutive_errors"
        );
        state.record_result(true).unwrap();
        state.record_result(false).unwrap();
    }

    #[test]
    fn projection_excludes_base64_and_uses_image_dimensions_for_claude() {
        let image = test_image();
        let b64 = image_base64(&image);
        let forms = [
            json!({"type":"input_image","image_url":format!("data:image/jpeg;base64,{b64}")}),
            json!({"type":"image_url","image_url":{"url":format!("data:image/jpeg;base64,{b64}")}}),
            json!({"type":"image","source":{"type":"base64","data":b64}}),
            json!({"inlineData":{"mimeType":"image/jpeg","data":b64}}),
            json!({"image":{"format":"jpeg","source":{"bytes":b64}}}),
        ];
        for mut body in forms {
            let small = projection_from_body(
                &body,
                std::slice::from_ref(&image),
                Protocol::Messages,
                8192,
            );
            fn enlarge(value: &mut Value) {
                match value {
                    Value::String(text) if text.len() > 100 => text.push_str(&"A".repeat(500_000)),
                    Value::Object(map) => {
                        for value in map.values_mut() {
                            enlarge(value)
                        }
                    }
                    _ => {}
                }
            }
            enlarge(&mut body);
            assert_eq!(
                projection_from_body(
                    &body,
                    std::slice::from_ref(&image),
                    Protocol::Messages,
                    8192
                )
                .input_tokens,
                small.input_tokens
            );
        }
        let mut large = image;
        large.width = 1280;
        large.height = 960;
        assert_eq!(
            project_input(100, 6, std::slice::from_ref(&large), Protocol::Messages),
            100 + 2 + 46 * 35
        );
        assert_eq!(project_input(100, 6, &[large], Protocol::Chat), 1702);
    }

    #[test]
    fn a_checked_restore_counts_its_recorded_changes() {
        let mut checked = Checked::new(
            StepId(1),
            &ToolCall {
                tool: "apply_camera_setup".into(),
                args: json!({}),
            },
        );
        let command = SessionCommand::ApplySetup {
            name: Some("bench".into()),
            setup: None,
            digest: None,
            allow_other_model: false,
            strict: false,
            confirmed_disruptive: Some(Vec::new()),
        };
        checked.ops.push(Op::ApplySetup {
            camera: CameraRef::Connected("sim:0".into()),
            command: command.clone(),
            changes: 3,
            features: vec!["ExposureTime".into(), "Gain".into(), "Width".into()],
            auto: false,
        });
        let impact = ToolImpact::from_checked(&checked, 0, 100);
        assert_eq!(impact.setting_changes, 3);
        assert_eq!(impact.blocking_ms, 240_000);
        // A raw restore has no reviewed count and fails closed.
        checked.ops = vec![Op::Command {
            camera: CameraRef::Connected("sim:0".into()),
            command,
        }];
        let raw = ToolImpact::from_checked(&checked, 0, 100);
        assert_eq!(raw.setting_changes, crate::setup::MAX_SETUP_VALUES as u32);
    }

    #[test]
    fn tool_impact_counts_checked_ops_and_pins_end_times() {
        let mut checked = Checked::new(
            StepId(1),
            &ToolCall {
                tool: "schedule_captures".into(),
                args: json!({}),
            },
        );
        checked.ops.push(Op::Command {
            camera: CameraRef::Connected("sim:0".into()),
            command: SessionCommand::Schedule {
                output: "photo-{seq}.png".into(),
                count: 3,
                timeout_ms: 1000,
                format: "png".into(),
                first_at_ms: 1000,
                interval_ms: 500,
                storage: Default::default(),
                destination: None,
                on_exists: crate::naming::OnExists::Number,
                context: Default::default(),
            },
        });
        checked.ops.push(Op::Restartable {
            camera: CameraRef::Connected("sim:0".into()),
            commands: vec![
                SessionCommand::Stop,
                SessionCommand::SetMany {
                    values: vec![
                        FeatureValue {
                            feature: "Width".into(),
                            value: "640".into(),
                        },
                        FeatureValue {
                            feature: "Height".into(),
                            value: "480".into(),
                        },
                    ],
                },
                SessionCommand::Start,
            ],
        });
        let impact = ToolImpact::from_checked(&checked, 999, 100);
        assert_eq!(impact.captures, 3);
        assert_eq!(impact.bytes, 999);
        assert_eq!(impact.setting_changes, 2);
        assert_eq!(impact.ends_at_ms, Some(2000));
        checked.call.tool = "start_recording".into();
        checked.ops = vec![Op::Command {
            camera: CameraRef::Connected("sim:0".into()),
            command: SessionCommand::Forward {
                output: "recording.mp4".into(),
                codec: "h264".into(),
                encoder: "auto".into(),
                fps: 30.0,
                bitrate: "8M".into(),
                storage: Default::default(),
                max_file_bytes: 1000,
                destination: None,
                on_exists: crate::naming::OnExists::Number,
                context: Default::default(),
                duration_ms: Some(1500),
            },
        }];
        let impact = ToolImpact::from_checked(&checked, 1000, 100);
        assert_eq!(impact.recordings, 1);
        assert_eq!(impact.recording_ms, Some(1500));
        assert_eq!(impact.ends_at_ms, Some(1600));
    }
}
