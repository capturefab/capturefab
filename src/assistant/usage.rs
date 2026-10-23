//! Disjoint token counts and explicitly configured prices. Reasoning is
//! already included in output and is never charged a second time.
use super::provider::Protocol;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::ops::AddAssign;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    /// Uncached input, excluding cache reads and cache writes.
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Includes reasoning/thought tokens when the provider reports them.
    pub output: u64,
    pub reasoning: Option<u64>,
    pub provider_cost_usd: Option<f64>,
    pub estimated: bool,
}

/// User-entered USD per one million tokens. There are no built-in prices.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prices {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl Prices {
    pub fn validate(&self) -> Result<()> {
        for (name, price) in [
            ("input", self.input),
            ("output", self.output),
            ("cache_read", self.cache_read),
            ("cache_write", self.cache_write),
        ] {
            ensure!(
                price.is_finite() && price >= 0.0,
                "Price {name}: enter a finite, nonnegative USD amount per one million tokens."
            );
        }
        Ok(())
    }
}

impl Usage {
    pub(super) fn from_local(body: &Value) -> Option<Self> {
        let usage = body.get("local_usage")?;
        Some(Self {
            input: usage["input_tokens"].as_u64()?,
            output: usage["output_tokens"].as_u64()?,
            provider_cost_usd: Some(0.0),
            estimated: true,
            ..Self::default()
        })
    }

    pub fn from_protocol(
        protocol: Protocol,
        body: &Value,
        input_bytes: usize,
        output_bytes: usize,
    ) -> Self {
        match protocol {
            Protocol::Messages => messages(body, input_bytes, output_bytes),
            Protocol::Responses => responses(body, input_bytes, output_bytes),
            Protocol::Generate => generate(body, input_bytes, output_bytes),
            Protocol::Chat => chat(body, input_bytes, output_bytes),
            Protocol::Converse => converse(body, input_bytes, output_bytes),
        }
    }

    pub fn total_input(&self) -> u64 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }

    pub fn total_tokens(&self) -> u64 {
        self.total_input().saturating_add(self.output)
    }

    /// Display this amount as "Estimated cost". A provider-reported total
    /// takes precedence over a token-based estimate from the user's prices.
    pub fn estimated_cost(&self, prices: Option<&Prices>) -> Option<f64> {
        if let Some(cost) = self
            .provider_cost_usd
            .filter(|cost| cost.is_finite() && *cost >= 0.0)
        {
            return Some(cost);
        }
        let prices = prices?;
        prices.validate().ok()?;
        let cost = (self.input as f64 * prices.input
            + self.cache_read as f64 * prices.cache_read
            + self.cache_write as f64 * prices.cache_write
            + self.output as f64 * prices.output)
            / 1_000_000.0;
        cost.is_finite().then_some(cost)
    }
}

impl AddAssign for Usage {
    fn add_assign(&mut self, other: Self) {
        // A partial provider-reported total must never override the price
        // estimate for all requests combined.
        let empty = self.total_tokens() == 0 && self.provider_cost_usd.is_none();
        self.provider_cost_usd = if empty {
            other.provider_cost_usd
        } else {
            self.provider_cost_usd
                .zip(other.provider_cost_usd)
                .map(|(a, b)| a + b)
                .filter(|cost| cost.is_finite())
        };
        self.input = self.input.saturating_add(other.input);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.output = self.output.saturating_add(other.output);
        self.reasoning = match (self.reasoning, other.reasoning) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            (a, b) => a.or(b),
        };
        self.estimated |= other.estimated;
    }
}

fn estimate(bytes: usize) -> u64 {
    u64::try_from(bytes).unwrap_or(u64::MAX).div_ceil(3)
}
fn number(value: &Value, field: &str) -> Option<u64> {
    value[field].as_u64()
}
fn usage<'a>(body: &'a Value, key: &str) -> &'a Value {
    body.get(key).unwrap_or(body)
}
fn base(input: Option<u64>, output: Option<u64>, input_bytes: usize, output_bytes: usize) -> Usage {
    Usage {
        input: input.unwrap_or_else(|| estimate(input_bytes)),
        output: output.unwrap_or_else(|| estimate(output_bytes)),
        estimated: input.is_none() || output.is_none(),
        ..Usage::default()
    }
}

pub fn messages(body: &Value, input_bytes: usize, output_bytes: usize) -> Usage {
    let value = usage(body, "usage");
    if let Some(iterations) = value["iterations"].as_array() {
        let mut total = Usage::default();
        for iteration in iterations {
            total += messages(iteration, input_bytes, output_bytes);
        }
        return total;
    }
    let mut result = base(
        number(value, "input_tokens"),
        number(value, "output_tokens"),
        input_bytes,
        output_bytes,
    );
    result.cache_read = number(value, "cache_read_input_tokens").unwrap_or(0);
    result.cache_write = number(value, "cache_creation_input_tokens").unwrap_or(0);
    result
}

pub fn responses(body: &Value, input_bytes: usize, output_bytes: usize) -> Usage {
    let value = usage(body, "usage");
    let cached = value["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);
    let mut result = base(
        number(value, "input_tokens").map(|input| input.saturating_sub(cached)),
        number(value, "output_tokens"),
        input_bytes,
        output_bytes,
    );
    result.cache_read = cached;
    result.reasoning = value["output_tokens_details"]["reasoning_tokens"].as_u64();
    result
}

pub fn generate(body: &Value, input_bytes: usize, output_bytes: usize) -> Usage {
    let value = usage(body, "usageMetadata");
    let cached = number(value, "cachedContentTokenCount").unwrap_or(0);
    let tools = number(value, "toolUsePromptTokenCount").unwrap_or(0);
    let thoughts = number(value, "thoughtsTokenCount");
    let mut result = base(
        number(value, "promptTokenCount")
            .map(|input| input.saturating_sub(cached).saturating_add(tools)),
        number(value, "candidatesTokenCount")
            .map(|output| output.saturating_add(thoughts.unwrap_or(0))),
        input_bytes,
        output_bytes,
    );
    result.cache_read = cached;
    result.reasoning = thoughts;
    result
}

pub fn chat(body: &Value, input_bytes: usize, output_bytes: usize) -> Usage {
    let value = usage(body, "usage");
    let cached = number(value, "prompt_cache_hit_tokens")
        .or_else(|| value["prompt_tokens_details"]["cached_tokens"].as_u64())
        .unwrap_or(0);
    let input = number(value, "prompt_cache_miss_tokens")
        .or_else(|| number(value, "prompt_tokens").map(|input| input.saturating_sub(cached)));
    let mut result = base(
        input,
        number(value, "completion_tokens"),
        input_bytes,
        output_bytes,
    );
    result.cache_read = cached;
    result.reasoning = value["completion_tokens_details"]["reasoning_tokens"].as_u64();
    result.provider_cost_usd = value["cost"]
        .as_f64()
        .filter(|cost| cost.is_finite() && *cost >= 0.0);
    result
}

pub fn converse(body: &Value, input_bytes: usize, output_bytes: usize) -> Usage {
    let value = usage(body, "usage");
    let mut result = base(
        number(value, "inputTokens"),
        number(value, "outputTokens"),
        input_bytes,
        output_bytes,
    );
    result.cache_read = number(value, "cacheReadInputTokens").unwrap_or(0);
    result.cache_write = number(value, "cacheWriteInputTokens").unwrap_or(0);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_usage_keeps_both_pass_estimates_and_has_no_api_cost() {
        let usage =
            Usage::from_local(&json!({"local_usage":{"input_tokens":2100,"output_tokens":180}}))
                .unwrap();
        assert_eq!(usage.input, 2100);
        assert_eq!(usage.output, 180);
        assert!(usage.estimated);
        assert_eq!(usage.estimated_cost(None), Some(0.0));
        for value in [
            json!({}),
            json!({"local_usage":{"input_tokens":-1,"output_tokens":0}}),
        ] {
            assert!(Usage::from_local(&value).is_none());
        }
    }

    #[test]
    fn each_protocol_maps_disjoint_input_and_counts_reasoning_once() {
        let fixtures = [
            (
                Protocol::Messages,
                json!({"usage":{"input_tokens":100,"cache_read_input_tokens":200,"cache_creation_input_tokens":30,"output_tokens":40}}),
                100,
                200,
                30,
                40,
                None,
            ),
            (
                Protocol::Responses,
                json!({"usage":{"input_tokens":300,"input_tokens_details":{"cached_tokens":200},"output_tokens":40,"output_tokens_details":{"reasoning_tokens":10}}}),
                100,
                200,
                0,
                40,
                Some(10),
            ),
            (
                Protocol::Generate,
                json!({"usageMetadata":{"promptTokenCount":300,"cachedContentTokenCount":200,"toolUsePromptTokenCount":5,"candidatesTokenCount":30,"thoughtsTokenCount":10}}),
                105,
                200,
                0,
                40,
                Some(10),
            ),
            (
                Protocol::Chat,
                json!({"usage":{"prompt_tokens":300,"prompt_tokens_details":{"cached_tokens":200},"completion_tokens":40,"completion_tokens_details":{"reasoning_tokens":10}}}),
                100,
                200,
                0,
                40,
                Some(10),
            ),
            (
                Protocol::Converse,
                json!({"usage":{"inputTokens":100,"cacheReadInputTokens":200,"cacheWriteInputTokens":30,"outputTokens":40}}),
                100,
                200,
                30,
                40,
                None,
            ),
        ];
        for (protocol, body, input, read, write, output, reasoning) in fixtures {
            let usage = Usage::from_protocol(protocol, &body, 900, 900);
            assert_eq!(
                (
                    usage.input,
                    usage.cache_read,
                    usage.cache_write,
                    usage.output,
                    usage.reasoning
                ),
                (input, read, write, output, reasoning)
            );
            assert_eq!(usage.total_tokens(), input + read + write + output);
            assert!(!usage.estimated);
        }
    }

    #[test]
    fn messages_iterations_replace_top_level_totals_and_deepseek_uses_miss_hit() {
        let usage = messages(
            &json!({"usage":{"input_tokens":9999,"output_tokens":9999,"iterations":[{"input_tokens":10,"cache_read_input_tokens":20,"output_tokens":3},{"input_tokens":4,"cache_creation_input_tokens":5,"output_tokens":6}]}}),
            0,
            0,
        );
        assert_eq!(
            (
                usage.input,
                usage.cache_read,
                usage.cache_write,
                usage.output
            ),
            (14, 20, 5, 9)
        );
        let usage = chat(
            &json!({"usage":{"prompt_tokens":9999,"prompt_cache_miss_tokens":100,"prompt_cache_hit_tokens":200,"completion_tokens":40,"cost":0.012}}),
            0,
            0,
        );
        assert_eq!(
            (usage.input, usage.cache_read, usage.output),
            (100, 200, 40)
        );
        assert_eq!(usage.provider_cost_usd, Some(0.012));
    }

    #[test]
    fn missing_usage_estimates_from_bytes_and_zero_is_a_real_count() {
        let missing = responses(&json!({}), 10, 7);
        assert_eq!((missing.input, missing.output), (4, 3));
        assert!(missing.estimated);
        let zero = responses(
            &json!({"usage":{"input_tokens":0,"output_tokens":0}}),
            100,
            100,
        );
        assert_eq!(zero.total_tokens(), 0);
        assert!(!zero.estimated);
        let invalid = responses(
            &json!({"usage":{"input_tokens":3,"input_tokens_details":{"cached_tokens":10},"output_tokens":0}}),
            0,
            0,
        );
        assert_eq!(invalid.input, 0);
        assert_eq!(invalid.cache_read, 10);
    }

    #[test]
    fn prices_are_usd_per_million_and_reported_totals_take_precedence() {
        let prices = Prices {
            input: 2.0,
            output: 10.0,
            cache_read: 0.2,
            cache_write: 2.5,
        };
        let mut usage = Usage {
            input: 1_000_000,
            output: 100_000,
            cache_read: 500_000,
            cache_write: 200_000,
            ..Usage::default()
        };
        assert_eq!(usage.estimated_cost(Some(&prices)), Some(3.6));
        assert_eq!(usage.estimated_cost(None), None);
        usage.provider_cost_usd = Some(0.5);
        assert_eq!(usage.estimated_cost(Some(&prices)), Some(0.5));
        assert_eq!(usage.estimated_cost(None), Some(0.5));
        for invalid in [-1.0, f64::NAN, f64::INFINITY] {
            assert!(
                Prices {
                    input: invalid,
                    ..prices
                }
                .validate()
                .is_err()
            );
        }
    }

    #[test]
    fn usage_totals_saturate_and_do_not_treat_partial_provider_cost_as_a_total() {
        let mut total = Usage::default();
        total += Usage {
            input: 5,
            provider_cost_usd: Some(1.0),
            ..Usage::default()
        };
        total += Usage {
            input: 10,
            provider_cost_usd: Some(2.0),
            reasoning: Some(2),
            ..Usage::default()
        };
        assert_eq!(total.provider_cost_usd, Some(3.0));
        total += Usage {
            input: u64::MAX,
            output: 10,
            estimated: true,
            ..Usage::default()
        };
        assert_eq!(total.input, u64::MAX);
        assert_eq!(total.total_tokens(), u64::MAX);
        assert_eq!(total.provider_cost_usd, None);
        assert!(total.estimated);
    }
}
