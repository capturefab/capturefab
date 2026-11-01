//! Feature names as commands take them, and the canonical controls an agent
//! can use without knowing a camera's vendor names. Names are parsed on the
//! client; banks and controls are resolved, planned and written in the
//! camera worker, against the live node map.
//!
//! A name is a GenICam feature (`ExposureTime`), a feature at a bank
//! (`TriggerMode[TriggerSelector=FrameStart]`, or `TriggerMode[FrameStart]`
//! when one selector addresses it) or a lowercase canonical control
//! (`exposure`, `gain`, `fps`, `roi`, `format`, `trigger`, `auto_exposure`,
//! `auto_wb`), which cannot collide with SFNC names.
use crate::camera::Camera;
use crate::genicam::Bounds;
use crate::types::{Categorized, ErrorKind, Transport, Usage};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

/// Exposure features, preferred first.
pub(crate) const EXPOSURE: [&str; 2] = ["ExposureTime", "ExposureTimeAbs"];
/// Gain features in dB or a multiplier, preferred first; GainRaw is the
/// last resort for `gain`.
pub(crate) const GAIN: [&str; 2] = ["Gain", "GainAbs"];
/// Frame-rate limiters, preferred first.
pub(crate) const FRAME_RATE: [&str; 2] = ["AcquisitionFrameRate", "AcquisitionFrameRateAbs"];
/// What turns a frame-rate limiter on.
pub(crate) const FRAME_RATE_ENABLE: [&str; 3] = [
    "AcquisitionFrameRateEnable",
    "AcquisitionFrameRateEnabled",
    "AcquisitionFrameRateMode",
];
/// The rate the camera actually runs at, when it says.
const RESULTING_RATE: [&str; 5] = [
    "AcquisitionResultingFrameRate",
    "ResultingFrameRate",
    "ResultingFrameRateAbs",
    "BslResultingAcquisitionFrameRate",
    "CurrentAcquisitionFrameRate",
];
/// Commands that change the acquisition or transfer state: capture and
/// sessions own them.
const STREAM_COMMAND_PREFIXES: [&str; 2] = ["Acquisition", "Transfer"];
/// Most selectors one name addresses.
const MAX_BANK: usize = 4;
/// Most rows `features` returns.
pub const MAX_FEATURE_ROWS: usize = 200;

/// The canonical controls, in the order `info` lists them.
pub const CONTROLS: [(&str, &str); 8] = [
    ("exposure", "exposure time as a duration: 500us, 10ms"),
    (
        "gain",
        "gain in dB when the camera's unit is dB, else its native unit",
    ),
    (
        "fps",
        "frame-rate limit in frames per second; turns the limiter on",
    ),
    ("roi", "region x,y,w,h in pixels, or full"),
    ("format", "pixel format, such as Mono8 or BayerRG8"),
    (
        "trigger",
        "off, software, or a line: line0[:rising|falling]",
    ),
    (
        "auto_exposure",
        "the camera's own auto exposure: off, once, continuous",
    ),
    (
        "auto_wb",
        "the camera's own white balance: off, once, continuous",
    ),
];

/// A feature or control as a command names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Name {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bank: Vec<BankEntry>,
}
/// One selector of a bank. Without `selector`, the one selector of the
/// feature that has `value` as a choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BankEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    pub value: String,
}
/// `NAME=VALUE` as a command gives it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assignment {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bank: Vec<BankEntry>,
    pub value: String,
}
impl Assignment {
    pub fn target(&self) -> Name {
        Name {
            name: self.name.clone(),
            bank: self.bank.clone(),
        }
    }
}

fn usage(message: impl Into<String>) -> anyhow::Error {
    Usage(message.into()).into()
}

fn control_name(name: &str) -> bool {
    CONTROLS.iter().any(|(control, _)| *control == name)
}

/// A feature or selector name: a GenICam identifier.
fn identifier(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
}

impl Name {
    /// `Feature`, `Feature[Selector=Value,...]`, `Feature[Value]` or a
    /// canonical control.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let (name, bank) = match text.split_once('[') {
            None => (text, Vec::new()),
            Some((name, rest)) => {
                let inner = rest.strip_suffix(']').ok_or_else(|| {
                    usage(format!(
                        "'{text}' needs a closing ]: Feature[Selector=Value]"
                    ))
                })?;
                let bank = inner
                    .split(',')
                    .map(|entry| {
                        let entry = entry.trim();
                        let (selector, value) = match entry.split_once('=') {
                            Some((selector, value)) => (Some(selector.trim()), value.trim()),
                            None => (None, entry),
                        };
                        if selector.is_some_and(|selector| !identifier(selector))
                            || value.is_empty()
                            || value.contains(['[', ']', '='])
                        {
                            return Err(usage(format!(
                                "'{text}' has an invalid bank '{entry}': use Feature[Selector=Value] or Feature[Value]"
                            )));
                        }
                        Ok(BankEntry {
                            selector: selector.map(str::to_owned),
                            value: value.to_owned(),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                (name, bank)
            }
        };
        let parsed = Self {
            name: name.to_owned(),
            bank,
        };
        parsed.validate()?;
        Ok(parsed)
    }
    pub fn validate(&self) -> Result<()> {
        if !identifier(&self.name) {
            return Err(usage(format!(
                "'{}' is not a feature name; names are like ExposureTime or TriggerMode[FrameStart]",
                self.name
            )));
        }
        if self.bank.len() > MAX_BANK {
            return Err(usage(format!(
                "{} addresses more than {MAX_BANK} selectors",
                self.name
            )));
        }
        for entry in &self.bank {
            if entry.selector.as_deref().is_some_and(|s| !identifier(s))
                || entry.value.is_empty()
                || entry.value.len() > 256
                || entry
                    .value
                    .chars()
                    .any(|c| c.is_control() || "[]=,".contains(c))
            {
                return Err(usage(format!("{} has an invalid bank", self.name)));
            }
        }
        if control_name(&self.name) && !self.bank.is_empty() {
            return Err(usage(format!(
                "{} is a canonical control; it takes no [bank]",
                self.name
            )));
        }
        Ok(())
    }
}
impl std::fmt::Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)?;
        if !self.bank.is_empty() {
            let entries = self
                .bank
                .iter()
                .map(|entry| match &entry.selector {
                    Some(selector) => format!("{selector}={}", entry.value),
                    None => entry.value.clone(),
                })
                .collect::<Vec<_>>();
            write!(f, "[{}]", entries.join(","))?;
        }
        Ok(())
    }
}
impl Assignment {
    /// `NAME=VALUE`, split at the first `=` outside a bank.
    pub fn parse(text: &str) -> Result<Self> {
        let mut depth = 0usize;
        let split = text.char_indices().find_map(|(index, c)| {
            match c {
                '[' => depth += 1,
                ']' => depth = depth.saturating_sub(1),
                '=' if depth == 0 => return Some(index),
                _ => {}
            }
            None
        });
        let Some(split) = split else {
            return Err(usage(format!(
                "'{text}' needs a value: NAME=VALUE, such as ExposureTime=10ms or exposure=5ms"
            )));
        };
        let name = Name::parse(&text[..split])?;
        let value = text[split + 1..].trim();
        if value.chars().any(char::is_control) || value.len() > 1024 {
            return Err(usage(format!(
                "the value for {name} must be at most 1024 bytes without control characters"
            )));
        }
        Ok(Self {
            name: name.name,
            bank: name.bank,
            value: value.to_owned(),
        })
    }
    pub fn validate(&self) -> Result<()> {
        self.target().validate()?;
        if self.value.chars().any(char::is_control) || self.value.len() > 1024 {
            return Err(usage(format!(
                "the value for {} must be at most 1024 bytes without control characters",
                self.name
            )));
        }
        Ok(())
    }
}

/// Select a bank, act, and select the bank that was selected before again,
/// also after a failure. Switching selectors only to reach a bank is never
/// a change of its own.
pub(crate) fn with_bank<T>(
    camera: &mut Camera,
    bank: &[(String, String)],
    action: impl FnOnce(&mut Camera) -> Result<T>,
) -> Result<T> {
    let mut originals = Vec::new();
    let result = (|| {
        for (selector, value) in bank {
            let old = camera.get(selector)?;
            if text(&old) != *value {
                originals.push((selector.clone(), old));
                camera.set(selector, value)?;
            }
        }
        action(camera)
    })();
    let mut restoration = Vec::new();
    for (selector, original) in originals.into_iter().rev() {
        if let Err(error) = camera.set(&selector, &text(&original)) {
            restoration.push(format!("{selector}: {error:#}"));
        }
    }
    anyhow::ensure!(
        restoration.is_empty(),
        "could not restore selectors: {}",
        restoration.join("; ")
    );
    result
}

/// A JSON value as the text a feature write takes.
pub(crate) fn text(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Number(number) if number.is_f64() => number.as_f64().unwrap().to_string(),
        value => value.to_string(),
    }
}

/// A feature at a resolved bank.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub feature: String,
    pub bank: Vec<(String, String)>,
}
impl Target {
    fn plain(feature: &str) -> Self {
        Self {
            feature: feature.into(),
            bank: Vec::new(),
        }
    }
    /// How results name it: `Feature[Value]` when that shorthand names the
    /// bank unambiguously, else `Feature[Selector=Value,...]`.
    fn key(&self, camera: &mut Camera) -> String {
        if self.bank.is_empty() {
            return self.feature.clone();
        }
        let selectors = real_selectors(camera, &self.feature);
        let short = self.bank.iter().all(|(selector, value)| {
            selectors
                .iter()
                .filter(|other| {
                    *other == selector
                        || camera
                            .choices(other)
                            .is_ok_and(|choices| choices.iter().any(|c| c == value))
                })
                .count()
                == 1
        });
        let entries = self
            .bank
            .iter()
            .map(|(selector, value)| {
                if short {
                    value.clone()
                } else {
                    format!("{selector}={value}")
                }
            })
            .collect::<Vec<_>>();
        format!("{}[{}]", self.feature, entries.join(","))
    }
    fn read(&self, camera: &mut Camera) -> Result<Value> {
        with_bank(camera, &self.bank, |camera| camera.get(&self.feature))
    }
}

/// The selectors that address a feature's bank and that a command can set.
fn real_selectors(camera: &mut Camera, feature: &str) -> Vec<String> {
    camera
        .selectors(feature)
        .into_iter()
        .filter(|selector| {
            !selector.starts_with("__")
                && matches!(
                    camera.node_kind(selector),
                    Some("Enumeration" | "Integer" | "Boolean")
                )
        })
        .collect()
}

/// Up to five feature or control names close to `typed`.
fn did_you_mean(camera: &Camera, typed: &str) -> Vec<String> {
    let mut close = camera
        .feature_names()
        .into_iter()
        .chain(CONTROLS.iter().map(|(control, _)| *control))
        .filter(|name| crate::hint::is_close(typed, name))
        .map(|name| (crate::hint::edit_distance(typed, name), name.to_owned()))
        .collect::<Vec<_>>();
    close.sort();
    close.dedup_by(|a, b| a.1 == b.1);
    close.into_iter().take(5).map(|(_, name)| name).collect()
}

fn not_found(camera: &Camera, name: &str) -> anyhow::Error {
    let close = did_you_mean(camera, name);
    let mut error = Categorized::new(
        ErrorKind::FeatureNotFound,
        format!("{} has no feature {name}", camera.info.model),
    )
    .details(json!({"feature": name, "did_you_mean": close}));
    if let Some(best) = close.first() {
        error = error.hint(format!(
            "did you mean {best}? `capturefab features` lists names"
        ));
    } else {
        error = error.hint("`capturefab features --all` lists every feature");
    }
    error.into()
}

/// Resolve a feature name and its bank on this camera.
fn resolve(camera: &mut Camera, name: &Name) -> Result<Target> {
    if !camera.has(&name.name)
        || camera.availability(&name.name) == crate::genicam::Availability::Absent
    {
        return Err(not_found(camera, &name.name));
    }
    let selectors = real_selectors(camera, &name.name);
    let mut bank: Vec<(String, String)> = Vec::new();
    for entry in &name.bank {
        let selector = match &entry.selector {
            Some(selector) => {
                if !camera.has(selector) {
                    return Err(not_found(camera, selector));
                }
                selector.clone()
            }
            None => {
                let matching = selectors
                    .iter()
                    .filter(|selector| {
                        camera
                            .choices(selector)
                            .is_ok_and(|choices| choices.contains(&entry.value))
                    })
                    .collect::<Vec<_>>();
                match (matching.as_slice(), selectors.as_slice()) {
                    ([one], _) => (*one).clone(),
                    ([], [one]) => one.clone(),
                    ([], []) => {
                        return Err(usage(format!(
                            "{} has no selector, so [{}] addresses nothing; name it without a bank",
                            name.name, entry.value
                        )));
                    }
                    _ => {
                        return Err(usage(format!(
                            "{}[{}] is ambiguous: name the selector, one of {}, as {}[SELECTOR={}]",
                            name.name,
                            entry.value,
                            selectors.join(", "),
                            name.name,
                            entry.value
                        )));
                    }
                }
            }
        };
        if bank.iter().any(|(other, _)| *other == selector) {
            return Err(usage(format!("{name} names {selector} twice")));
        }
        bank.push((selector, entry.value.clone()));
    }
    Ok(Target {
        feature: name.name.clone(),
        bank,
    })
}

/// Why a control is missing on this camera, for exit 5.
fn unsupported(camera: &Camera, control: &str, what: &str) -> anyhow::Error {
    let source = match (camera.info.transport, crate::camera::kind(&camera.info)) {
        (Transport::Media, "onvif") => "an ONVIF stream via FFmpeg",
        (Transport::Media, "webcam") => "a webcam via FFmpeg",
        (Transport::Media, "screen") => "a screen capture via FFmpeg",
        (Transport::Media, "stream") => "a stream via FFmpeg",
        (Transport::Media, _) => "a file via FFmpeg",
        _ => camera.info.model.as_str(),
    };
    Categorized::new(
        ErrorKind::Unsupported,
        format!("{source} has no {what} control ({control})"),
    )
    .details(json!({"control": control, "camera": camera.info.id}))
    .into()
}

fn first_present<'a>(camera: &Camera, names: &[&'a str]) -> Option<&'a str> {
    names.iter().copied().find(|name| camera.has(name))
}

pub(crate) fn has_choice(camera: &mut Camera, feature: &str, choice: &str) -> bool {
    camera
        .choices(feature)
        .is_ok_and(|c| c.iter().any(|v| v == choice))
}

/// The choice of `feature` equal to `wanted` ignoring case and `_`.
fn choice(camera: &mut Camera, feature: &str, wanted: &str) -> Option<String> {
    let squash = |s: &str| s.replace(['_', '-'], "").to_ascii_lowercase();
    camera
        .choices(feature)
        .ok()?
        .into_iter()
        .find(|choice| squash(choice) == squash(wanted))
}

/// The bank `GainSelector=All` (or AnalogAll) when the camera has one.
fn gain_bank(camera: &mut Camera) -> Vec<(String, String)> {
    if !camera.has("GainSelector") {
        return Vec::new();
    }
    ["All", "AnalogAll", "SensorAll"]
        .into_iter()
        .find(|c| has_choice(camera, "GainSelector", c))
        .map(|c| vec![("GainSelector".to_owned(), c.to_owned())])
        .unwrap_or_default()
}

/// The FrameStart bank of the trigger features, when they have one.
fn trigger_bank(camera: &mut Camera) -> Vec<(String, String)> {
    if camera.has("TriggerSelector") && has_choice(camera, "TriggerSelector", "FrameStart") {
        vec![("TriggerSelector".into(), "FrameStart".into())]
    } else {
        Vec::new()
    }
}

/// A duration's length in a time unit (`us`, `ms`, `s`, `ns`), or None for
/// any other unit.
fn in_time_unit(duration: Duration, unit: &str) -> Option<f64> {
    let seconds = duration.as_secs_f64();
    match unit.trim() {
        "us" | "µs" | "μs" | "microseconds" => Some(seconds * 1e6),
        "ms" | "milliseconds" => Some(seconds * 1e3),
        "s" | "seconds" => Some(seconds),
        "ns" | "nanoseconds" => Some(seconds * 1e9),
        _ => None,
    }
}

/// `value` for a feature, converting a duration (`10ms`) to its time unit.
fn feature_value(camera: &Camera, feature: &str, value: &str) -> Result<String> {
    let Some(unit) = camera.feature_unit(feature) else {
        return Ok(value.to_owned());
    };
    let looks_like_duration = value
        .trim()
        .chars()
        .last()
        .is_some_and(|c| c.is_ascii_alphabetic());
    if !looks_like_duration || in_time_unit(Duration::ZERO, unit).is_none() {
        return Ok(value.to_owned());
    }
    let duration = crate::types::parse_duration(value)?;
    Ok(number_text(in_time_unit(duration, unit).unwrap()))
}

/// A number without a trailing `.0`, rounded to nanosecond precision.
fn number_text(value: f64) -> String {
    let rounded = (value * 1e9).round() / 1e9;
    if rounded.fract() == 0.0 && rounded.abs() < 1e15 {
        format!("{}", rounded as i64)
    } else {
        rounded.to_string()
    }
}

/// An exposure time in µs as a duration `set` takes back.
fn duration_text(us: f64) -> String {
    if us >= 1000.0 && (us / 1000.0).fract() == 0.0 {
        format!("{}ms", number_text(us / 1000.0))
    } else {
        format!("{}us", number_text(us))
    }
}

/// One write of a planned change.
#[derive(Debug, Clone)]
struct Write {
    target: Target,
    value: String,
    /// The name it came from, for messages.
    origin: String,
    /// Added by the plan itself: an auto switched off, or an offset moved
    /// out of the way for a larger size.
    implied: bool,
}

/// Expand a canonical control into feature writes.
fn expand_control(camera: &mut Camera, control: &str, value: &str) -> Result<Vec<Write>> {
    let write = |target: Target, value: String| Write {
        target,
        value,
        origin: control.to_owned(),
        implied: false,
    };
    let lower = value.trim().to_ascii_lowercase();
    Ok(match control {
        "exposure" => {
            let feature = first_present(camera, &EXPOSURE)
                .ok_or_else(|| unsupported(camera, control, "exposure"))?;
            let duration = crate::types::parse_duration(value)?;
            let unit = camera.feature_unit(feature).unwrap_or("us").to_owned();
            let amount = in_time_unit(duration, &unit).unwrap_or(duration.as_secs_f64() * 1e6);
            vec![write(Target::plain(feature), number_text(amount))]
        }
        "gain" => {
            let feature = first_present(camera, &GAIN)
                .or_else(|| first_present(camera, &["GainRaw"]))
                .ok_or_else(|| unsupported(camera, control, "gain"))?;
            value
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .ok_or_else(|| usage(format!("gain takes a number, not '{value}'")))?;
            let bank = gain_bank(camera);
            vec![write(
                Target {
                    feature: feature.into(),
                    bank,
                },
                value.trim().into(),
            )]
        }
        "fps" => {
            let feature = first_present(camera, &FRAME_RATE)
                .ok_or_else(|| unsupported(camera, control, "frame-rate"))?;
            value
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v > 0.0)
                .ok_or_else(|| usage(format!("fps takes a positive number, not '{value}'")))?;
            let mut writes = Vec::new();
            if camera.has("AcquisitionFrameRateAuto")
                && has_choice(camera, "AcquisitionFrameRateAuto", "Off")
            {
                writes.push(write(
                    Target::plain("AcquisitionFrameRateAuto"),
                    "Off".into(),
                ));
            }
            if let Some(enable) = first_present(camera, &FRAME_RATE_ENABLE) {
                let on = if enable == "AcquisitionFrameRateMode" {
                    "On"
                } else {
                    "true"
                };
                writes.push(write(Target::plain(enable), on.into()));
            }
            writes.push(write(Target::plain(feature), value.trim().into()));
            writes
        }
        "format" => {
            if !camera.has("PixelFormat") {
                return Err(unsupported(camera, control, "pixel format"));
            }
            vec![write(Target::plain("PixelFormat"), value.trim().into())]
        }
        "roi" => roi(camera, value)?
            .into_iter()
            .map(|(feature, value)| write(Target::plain(feature), value))
            .collect(),
        "trigger" => {
            if !camera.has("TriggerMode") {
                return Err(unsupported(camera, control, "trigger"));
            }
            let bank = trigger_bank(camera);
            let at = |feature: &str| Target {
                feature: feature.into(),
                bank: bank.clone(),
            };
            if lower == "off" {
                return Ok(vec![write(at("TriggerMode"), "Off".into())]);
            }
            let (source, edge) = match lower.split_once(':') {
                Some((source, edge)) => (source, Some(edge)),
                None => (lower.as_str(), None),
            };
            if !camera.has("TriggerSource") {
                return Err(unsupported(camera, control, "trigger source"));
            }
            let chosen = with_bank(camera, &bank, |camera| {
                Ok(choice(camera, "TriggerSource", source))
            })?;
            let Some(chosen) = chosen else {
                let choices = with_bank(camera, &bank, |camera| camera.choices("TriggerSource"))
                    .unwrap_or_default();
                return Err(Categorized::new(
                    ErrorKind::InvalidChoice,
                    format!(
                        "trigger source '{source}' is not one of: {}",
                        choices.join(", ")
                    ),
                )
                .details(json!({"feature": "TriggerSource", "value": source, "choices": choices}))
                .into());
            };
            let mut writes = vec![write(at("TriggerSource"), chosen)];
            if let Some(edge) = edge {
                let activation = match edge {
                    "rising" => "RisingEdge",
                    "falling" => "FallingEdge",
                    "any" | "both" => "AnyEdge",
                    "high" => "LevelHigh",
                    "low" => "LevelLow",
                    other => {
                        return Err(usage(format!(
                            "trigger edge '{other}' is not rising, falling, any, high or low"
                        )));
                    }
                };
                if !camera.has("TriggerActivation") {
                    return Err(unsupported(camera, control, "trigger activation"));
                }
                writes.push(write(at("TriggerActivation"), activation.into()));
            }
            writes.push(write(at("TriggerMode"), "On".into()));
            writes
        }
        "auto_exposure" | "auto_wb" => {
            let feature = if control == "auto_exposure" {
                "ExposureAuto"
            } else {
                "BalanceWhiteAuto"
            };
            if !camera.has(feature) {
                let what = if control == "auto_exposure" {
                    "auto exposure"
                } else {
                    "white balance"
                };
                return Err(unsupported(camera, control, what));
            }
            if !matches!(lower.as_str(), "off" | "once" | "continuous") {
                return Err(usage(format!(
                    "{control} takes off, once or continuous, not '{value}'"
                )));
            }
            let Some(chosen) = choice(camera, feature, &lower) else {
                let choices = camera.choices(feature).unwrap_or_default();
                return Err(Categorized::new(
                    ErrorKind::Unsupported,
                    format!("{feature} offers {}, not {lower}", choices.join(", ")),
                )
                .details(json!({"feature": feature, "choices": choices}))
                .into());
            };
            vec![write(Target::plain(feature), chosen)]
        }
        _ => unreachable!("only canonical controls expand"),
    })
}

/// The writes `roi=x,y,w,h` or `roi=full` makes, in order.
fn roi(camera: &mut Camera, value: &str) -> Result<Vec<(&'static str, String)>> {
    if !camera.has("Width") || !camera.has("Height") {
        return Err(unsupported(camera, "roi", "region"));
    }
    let maximum = |camera: &mut Camera, size: &str, max: &str| -> Option<f64> {
        if camera.has(max) {
            camera.get(max).ok()?.as_f64()
        } else {
            camera.bounds(size).ok()?.max
        }
    };
    let [x, y, w, h] = if value.trim().eq_ignore_ascii_case("full") {
        let w = maximum(camera, "Width", "WidthMax")
            .ok_or_else(|| usage("this camera does not report its maximum width"))?;
        let h = maximum(camera, "Height", "HeightMax")
            .ok_or_else(|| usage("this camera does not report its maximum height"))?;
        [0.0, 0.0, w, h]
    } else {
        let parts = value
            .split(',')
            .map(|part| {
                part.trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|v| *v >= 0.0 && v.fract() == 0.0)
            })
            .collect::<Option<Vec<_>>>()
            .filter(|parts| parts.len() == 4)
            .ok_or_else(|| {
                usage(format!(
                    "roi takes x,y,w,h in whole pixels or full, not '{value}'"
                ))
            })?;
        [parts[0], parts[1], parts[2], parts[3]]
    };
    let offsets = camera.has("OffsetX") && camera.has("OffsetY");
    if !offsets && (x != 0.0 || y != 0.0) {
        return Err(unsupported(
            camera,
            "roi",
            "region offset (OffsetX/OffsetY)",
        ));
    }
    let mut writes = Vec::new();
    // A larger size may not fit beside the current offsets: move them to 0
    // first, then size, then the final offsets.
    if offsets {
        writes.push(("OffsetX", "0".to_owned()));
        writes.push(("OffsetY", "0".to_owned()));
    }
    writes.push(("Width", number_text(w)));
    writes.push(("Height", number_text(h)));
    if offsets {
        writes.push(("OffsetX", number_text(x)));
        writes.push(("OffsetY", number_text(y)));
    }
    Ok(writes)
}

/// Features whose bounds another write of the same change can move: the
/// image geometry.
const COUPLED: [&str; 9] = [
    "Width",
    "Height",
    "OffsetX",
    "OffsetY",
    "PixelFormat",
    "BinningHorizontal",
    "BinningVertical",
    "DecimationHorizontal",
    "DecimationVertical",
];

/// Turn assignments into ordered writes: controls expanded, bank shorthand
/// resolved, autos that lock a written value switched off first, geometry
/// in a safe order and a lower frame rate before a longer exposure.
fn plan(camera: &mut Camera, values: &[Assignment]) -> Result<Vec<Write>> {
    let mut writes = Vec::new();
    for assignment in values {
        assignment.validate()?;
        if control_name(&assignment.name) {
            writes.extend(expand_control(camera, &assignment.name, &assignment.value)?);
        } else {
            let target = resolve(camera, &assignment.target())?;
            let value = feature_value(camera, &target.feature, &assignment.value)?;
            writes.push(Write {
                origin: assignment.target().to_string(),
                target,
                value,
                implied: false,
            });
        }
    }
    // A feature named without a bank is written at the bank in effect where
    // the command names it: the selector value an earlier assignment sets,
    // else the current one. Its `from`, the unchanged check and the check
    // for a feature set twice then all read the bank it really writes.
    for index in 0..writes.len() {
        if !writes[index].target.bank.is_empty() {
            continue;
        }
        let feature = writes[index].target.feature.clone();
        let mut bank = Vec::new();
        for selector in real_selectors(camera, &feature) {
            let earlier = writes[..index]
                .iter()
                .rev()
                .find(|earlier| earlier.target.feature == selector)
                .map(|earlier| earlier.value.clone());
            match earlier.or_else(|| camera.get(&selector).ok().map(|value| text(&value))) {
                Some(value) => bank.push((selector, value)),
                None => {
                    bank.clear();
                    break;
                }
            }
        }
        writes[index].target.bank = bank;
    }
    // The same feature at the same bank twice is a mistake, except the
    // offsets an ROI moves out of the way and back.
    for (index, write) in writes.iter().enumerate() {
        if let Some(earlier) = writes[..index].iter().find(|earlier| {
            earlier.target == write.target && !(earlier.origin == "roi" && write.origin == "roi")
        }) {
            return Err(usage(format!(
                "{} is set twice (by {} and {})",
                write.target.feature, earlier.origin, write.origin
            )));
        }
    }
    let written = writes
        .iter()
        .map(|write| write.target.feature.clone())
        .collect::<Vec<_>>();
    // A value locked by its own auto gets that auto turned off first.
    let mut implied = Vec::new();
    for write in &writes {
        let feature = write.target.feature.clone();
        let locked = with_bank(camera, &write.target.bank, |camera| {
            Ok(camera.is_locked(&feature))
        })?;
        if !locked {
            continue;
        }
        let Some(owner) = camera.lock_owner(&feature) else {
            continue;
        };
        if owner.ends_with("Auto")
            && !written.contains(&owner)
            && !implied.iter().any(|w: &Write| w.target.feature == owner)
            && has_choice(camera, &owner, "Off")
        {
            implied.push(Write {
                target: Target::plain(&owner),
                value: "Off".into(),
                origin: format!("{owner} (locks {feature})"),
                implied: true,
            });
        }
    }
    writes.splice(0..0, implied);
    // Offsets moved to 0 only when they are not 0 already.
    writes.retain(|write| {
        !(write.origin == "roi"
            && write.value == "0"
            && write.target.feature.starts_with("Offset")
            && camera
                .get(&write.target.feature)
                .is_ok_and(|current| current.as_f64() == Some(0.0)))
    });
    order(camera, &mut writes);
    Ok(writes)
}

/// The features `values` would write, in order, autos switched off to
/// unlock a value included.
pub(crate) fn written(camera: &mut Camera, values: &[Assignment]) -> Result<Vec<String>> {
    Ok(plan(camera, values)?
        .into_iter()
        .map(|write| write.target.feature)
        .collect())
}

fn order(camera: &mut Camera, writes: &mut Vec<Write>) {
    // ROI writes keep their own order; everything else follows setups.
    let mut keyed = writes
        .drain(..)
        .enumerate()
        .map(|(index, write)| {
            let key = crate::setup::order_key(&write.target.feature, Some(&write.value));
            (key, index, write)
        })
        .collect::<Vec<_>>();
    keyed.sort_by(|a, b| {
        let roi = a.2.origin == "roi" && b.2.origin == "roi";
        if roi {
            a.1.cmp(&b.1)
        } else {
            (&a.0, a.1).cmp(&(&b.0, b.1))
        }
    });
    *writes = keyed.into_iter().map(|(_, _, write)| write).collect();
    // A value locked now goes after the write of the change that can
    // unlock it.
    let locked = writes
        .iter()
        .map(|write| {
            let feature = write.target.feature.clone();
            let locked = with_bank(camera, &write.target.bank, |camera| {
                Ok(camera.is_locked(&feature))
            });
            (feature, locked.unwrap_or(false))
        })
        .filter(|(_, locked)| *locked)
        .map(|(feature, _)| feature)
        .collect::<Vec<_>>();
    for _ in 0..writes.len() {
        let mut moved = false;
        for index in 0..writes.len() {
            let feature = writes[index].target.feature.clone();
            if !locked.contains(&feature) {
                continue;
            }
            if let Some(later) = (index + 1..writes.len()).find(|&later| {
                let other = writes[later].target.feature.as_str();
                other != feature && camera.lock_released_by(&feature, &[other])
            }) {
                let unlocking = writes.remove(later);
                writes.insert(index, unlocking);
                moved = true;
                break;
            }
        }
        if !moved {
            break;
        }
    }
    // A longer exposure may not fit the current frame period: lower the
    // rate first. A higher rate may not fit the current exposure: shorten
    // the exposure first.
    let rate = writes
        .iter()
        .position(|w| FRAME_RATE.contains(&w.target.feature.as_str()));
    let exposure = writes
        .iter()
        .position(|w| EXPOSURE.contains(&w.target.feature.as_str()));
    if let (Some(rate), Some(exposure)) = (rate, exposure) {
        let current = camera
            .get(&writes[rate].target.feature)
            .ok()
            .and_then(|v| v.as_f64());
        let wanted = writes[rate].value.parse::<f64>().ok();
        let raising =
            matches!((current, wanted), (Some(current), Some(wanted)) if wanted > current);
        // Removing the rate shifts the exposure, so inserting at its old
        // index puts the rate after it when it came first, else before it.
        if raising == (rate < exposure) {
            let write = writes.remove(rate);
            writes.insert(exposure, write);
        }
    }
}

/// Whether two feature values are the same setting.
fn same(current: &Value, wanted: &str) -> bool {
    match current {
        Value::String(current) => current == wanted,
        Value::Bool(current) => {
            matches!(
                (current, wanted.to_ascii_lowercase().as_str()),
                (true, "true" | "1" | "on") | (false, "false" | "0" | "off")
            )
        }
        Value::Number(number) => wanted
            .trim()
            .parse::<f64>()
            .ok()
            .zip(number.as_f64())
            .is_some_and(|(wanted, current)| wanted == current),
        _ => false,
    }
}

/// A value as JSON for results: the number or boolean it reads as.
fn wanted_json(current: Option<&Value>, wanted: &str) -> Value {
    match current {
        Some(Value::Number(_)) => wanted
            .trim()
            .parse::<i64>()
            .map(Value::from)
            .ok()
            .or_else(|| wanted.trim().parse::<f64>().ok().map(|v| json!(v)))
            .unwrap_or_else(|| json!(wanted)),
        Some(Value::Bool(_)) => json!(matches!(
            wanted.to_ascii_lowercase().as_str(),
            "true" | "1" | "on"
        )),
        _ => json!(wanted),
    }
}

/// Whether earlier writes of a change unlock a feature that is locked now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Unlock {
    No,
    /// A write that can unlock it, to a value whose effect only writing
    /// shows.
    Maybe,
    /// Its auto switched off.
    Yes,
}

fn unlock(camera: &Camera, feature: &str, earlier: &[Write]) -> Unlock {
    earlier
        .iter()
        .filter(|other| {
            other.target.feature != feature
                && camera.lock_released_by(feature, &[&other.target.feature])
        })
        .map(|other| {
            if !other.target.feature.ends_with("Auto") {
                Unlock::Maybe
            } else if matches!(
                other.value.trim().to_ascii_lowercase().as_str(),
                "off" | "false" | "0"
            ) {
                Unlock::Yes
            } else {
                Unlock::No
            }
        })
        .max()
        .unwrap_or(Unlock::No)
}

/// Whether an earlier write of the change can move this feature's range:
/// the image geometry, or exposure and frame rate, which bound each other.
fn range_moved(feature: &str, earlier: &[Write]) -> bool {
    let any = |names: &[&str]| {
        earlier
            .iter()
            .any(|other| names.contains(&other.target.feature.as_str()))
    };
    (COUPLED.contains(&feature) && any(&COUPLED))
        || (EXPOSURE.contains(&feature) && any(&FRAME_RATE))
        || (FRAME_RATE.contains(&feature) && any(&EXPOSURE))
}

/// The most a geometry value can be whatever else the change writes: the
/// sensor size, else WidthMax/HeightMax unless the change bins or decimates
/// that axis, else a maximum the XML fixes.
fn outer_max(camera: &mut Camera, feature: &str, earlier: &[Write]) -> Option<f64> {
    let mut read = |name: &str| {
        camera
            .has(name)
            .then(|| camera.get(name).ok()?.as_f64())
            .flatten()
    };
    let axis = match feature {
        "Width" | "OffsetX" => Some(("SensorWidth", "WidthMax", "Horizontal")),
        "Height" | "OffsetY" => Some(("SensorHeight", "HeightMax", "Vertical")),
        _ => None,
    };
    if let Some((sensor, max, axis)) = axis {
        if let Some(sensor) = read(sensor) {
            return Some(sensor);
        }
        let scaled = earlier.iter().any(|other| {
            let other = other.target.feature.as_str();
            other.ends_with(axis)
                && (other.starts_with("Binning") || other.starts_with("Decimation"))
        });
        if !scaled && let Some(max) = read(max) {
            return Some(max);
        }
    }
    camera.static_max(feature)
}

fn out_of_range(named: &str, value: &str, min: Option<f64>, max: Option<f64>) -> anyhow::Error {
    let limit = match (min, max) {
        (Some(min), Some(max)) => format!("{}..{}", number_text(min), number_text(max)),
        (Some(min), None) => format!("at least {}", number_text(min)),
        (None, Some(max)) => format!("at most {}", number_text(max)),
        (None, None) => String::new(),
    };
    Categorized::new(
        ErrorKind::OutOfRange,
        format!("{named} cannot be {value}: it takes {limit}"),
    )
    .details(json!({"feature": named, "value": value, "min": min, "max": max}))
    .into()
}

/// A value inside the feature's current range, checked without the lock
/// that an earlier write of the change releases.
fn within_range(camera: &mut Camera, named: &str, feature: &str, value: &str) -> Result<()> {
    let Ok(number) = value.trim().parse::<f64>() else {
        return Ok(());
    };
    let bounds = camera.bounds(feature).unwrap_or_default();
    if bounds.min.is_some_and(|min| number < min) || bounds.max.is_some_and(|max| number > max) {
        return Err(out_of_range(named, value, bounds.min, bounds.max));
    }
    Ok(())
}

/// Check a write against the live node map without writing: the feature is
/// writable now (or the change unlocks it first), and the value is in range
/// or one of the choices. `earlier` are the writes before it. A check that
/// only the writes before it can decide is a warning: `set` checks again as
/// it writes.
fn check(
    camera: &mut Camera,
    write: &Write,
    earlier: &[Write],
    streaming: bool,
) -> Result<Option<String>> {
    let feature = write.target.feature.as_str();
    let named = write.target.key(camera);
    if camera.static_access(feature) == "ro" {
        return Err(
            Categorized::new(ErrorKind::NotWritable, format!("{named} is read-only"))
                .details(json!({"feature": named}))
                .into(),
        );
    }
    if camera.node_kind(feature) == Some("Command") {
        return Err(usage(format!("{named} is a command; run it with exec")));
    }
    if streaming && camera.locked_while_streaming(feature) {
        return Err(Categorized::new(
            ErrorKind::CameraBusy,
            format!("{named} cannot change while the camera streams"),
        )
        .hint("stop acquisition in the session first, then set it")
        .details(json!({"feature": named}))
        .into());
    }
    let unlock = unlock(camera, feature, earlier);
    let value = write.value.clone();
    let checked = with_bank(camera, &write.target.bank, |camera| {
        if camera.is_locked(feature) {
            match unlock {
                Unlock::No => {
                    let owner = camera.lock_owner(feature);
                    let mut locked =
                        Categorized::new(ErrorKind::FeatureLocked, format!("{named} is locked"))
                            .details(json!({"feature": named, "locked_by": owner}));
                    if let Some(owner) = &owner {
                        locked = locked.hint(format!(
                            "set {owner} to Off in the same command so it unlocks {feature} first"
                        ));
                    }
                    return Err(locked.into());
                }
                Unlock::Maybe => {
                    return Ok(Some(format!(
                        "{named} is locked now; set checks {value} once the writes before it run"
                    )));
                }
                Unlock::Yes => return within_range(camera, &named, feature, &value).map(|()| None),
            }
        }
        camera.validate_set(feature, &value).map(|()| None)
    });
    match checked {
        Err(error)
            if range_moved(feature, earlier)
                && crate::types::classify(&error).kind == ErrorKind::OutOfRange =>
        {
            // Another write of this change may move these bounds; the
            // write itself checks again. A value past what no write can
            // move fails now.
            if let Ok(number) = value.trim().parse::<f64>()
                && let Some(max) = outer_max(camera, feature, earlier)
                && number > max
            {
                return Err(out_of_range(&named, &value, None, Some(max)));
            }
            Ok(Some(format!(
                "{named}={value} is outside its current range; set checks it again after the writes before it"
            )))
        }
        Err(error) => Err(with_unit(camera, feature, error)),
        checked => checked,
    }
}

/// Add the feature's unit to an out-of-range error's details.
fn with_unit(camera: &Camera, feature: &str, error: anyhow::Error) -> anyhow::Error {
    let classified = crate::types::classify(&error);
    if classified.kind != ErrorKind::OutOfRange {
        return error;
    }
    let mut details = classified.details.unwrap_or_else(|| json!({}));
    if let Some(unit) = camera.feature_unit(feature) {
        details["unit"] = json!(unit);
    }
    let nearest = details["nearest"].clone();
    let mut categorized =
        Categorized::new(ErrorKind::OutOfRange, format!("{error:#}")).details(details);
    if !nearest.is_null() {
        categorized = categorized.hint(format!("the nearest accepted value is {nearest}"));
    }
    categorized.into()
}

/// One row of a change: what it was and what it is now.
#[derive(Debug, Clone, Serialize)]
pub struct Change {
    pub name: String,
    pub from: Value,
    pub to: Value,
}

/// The result of `set`.
#[derive(Debug, Clone, Serialize)]
pub struct SetResult {
    pub camera: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed: Option<Vec<Change>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<Vec<Change>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}
impl SetResult {
    /// Top-level keys and what they mean, for `schema set`.
    pub const FIELDS: &[(&str, &str)] = &[
        ("camera", "string — camera ID"),
        (
            "changed",
            "[{name,from,to}] — every value written, in order, `to` read back (may be rounded); autos turned off to unlock a value are listed too",
        ),
        (
            "plan",
            "[{name,from,to}] — with --dry-run: the writes in order, none made",
        ),
        ("dry_run", "true — with --dry-run"),
        (
            "warnings",
            "[string] — with --dry-run: checks only the writes themselves can decide (a value another write of the change may unlock or bring into range)",
        ),
    ];
}

/// Plan and, unless `dry_run`, apply `values` as one change: on any
/// failure every value written is restored in reverse order at its bank,
/// and the error's details say what the rollback did.
pub(crate) fn set(camera: &mut Camera, values: &[Assignment], dry_run: bool) -> Result<SetResult> {
    let writes = plan(camera, values)?;
    let streaming = camera.is_streaming();
    let mut rows = Vec::with_capacity(writes.len());
    let mut pending = Vec::with_capacity(writes.len());
    let mut warnings = Vec::new();
    for (index, write) in writes.iter().enumerate() {
        warnings.extend(check(camera, write, &writes[..index], streaming)?);
        let from = write.target.read(camera).ok();
        // A write that changes nothing is left out, unless an earlier write
        // of the change may move it (an offset that returns to its value).
        if !write.implied
            && from.as_ref().is_some_and(|from| same(from, &write.value))
            && !writes[..index]
                .iter()
                .any(|earlier| earlier.target == write.target)
        {
            continue;
        }
        let name = write.target.key(camera);
        rows.push(Change {
            name,
            to: wanted_json(from.as_ref(), &write.value),
            from: from.unwrap_or(Value::Null),
        });
        pending.push(write.clone());
    }
    let id = crate::media::redact_url(&camera.info.id);
    if dry_run {
        return Ok(SetResult {
            camera: id,
            changed: None,
            plan: Some(rows),
            dry_run: true,
            warnings,
        });
    }
    let mut written: Vec<(Target, Value)> = Vec::new();
    for write in &pending {
        let from = write.target.read(camera);
        let result = from.and_then(|from| {
            with_bank(camera, &write.target.bank, |camera| {
                camera.set(&write.target.feature, &write.value)
            })
            .map(|()| from)
        });
        match result {
            Ok(from) => written.push((write.target.clone(), from)),
            Err(error) => {
                let error = with_unit(camera, &write.target.feature, error);
                return Err(rollback(camera, written, error));
            }
        }
    }
    // A feature written twice (an offset moved out of the way and back) is
    // one change, from its first value to its last.
    let mut changed: Vec<Change> = Vec::with_capacity(written.len());
    for (target, from) in &written {
        let name = target.key(camera);
        if changed.iter().any(|change| change.name == name) {
            continue;
        }
        changed.push(Change {
            to: target.read(camera).unwrap_or(Value::Null),
            name,
            from: from.clone(),
        });
    }
    Ok(SetResult {
        camera: id,
        changed: Some(changed),
        plan: None,
        dry_run: false,
        warnings: Vec::new(),
    })
}

/// Restore `written` in reverse order and report it in `error`'s details.
fn rollback(
    camera: &mut Camera,
    written: Vec<(Target, Value)>,
    error: anyhow::Error,
) -> anyhow::Error {
    let mut restored = Vec::new();
    let mut failed = Vec::new();
    for (target, from) in written.into_iter().rev() {
        let name = target.key(camera);
        let result = with_bank(camera, &target.bank, |camera| {
            camera.set(&target.feature, &text(&from))
        });
        match result {
            Ok(()) => restored.push(name),
            Err(error) => failed.push(json!({"name": name, "error": format!("{error:#}")})),
        }
    }
    let classified = crate::types::classify(&error);
    let mut details = classified.details.unwrap_or_else(|| json!({}));
    if !details.is_object() {
        details = json!({ "cause": details });
    }
    let summary = if failed.is_empty() {
        if restored.is_empty() {
            "; nothing was changed".to_owned()
        } else {
            "; every value written was restored".to_owned()
        }
    } else {
        format!("; {} could not be restored", failed.len())
    };
    details["rollback"] = json!({"restored": restored, "failed": failed});
    let mut categorized =
        Categorized::new(classified.kind, format!("{error:#}{summary}")).details(details);
    categorized.hint = classified.hint;
    categorized.into()
}

/// The values `names` read now, keyed as the command named them, with the
/// bank shorthand where it is unambiguous.
pub(crate) fn get(camera: &mut Camera, names: &[Name]) -> Result<serde_json::Map<String, Value>> {
    let mut values = serde_json::Map::new();
    for name in names {
        name.validate()?;
        if control_name(&name.name) {
            values.insert(name.name.clone(), control_value(camera, &name.name)?);
            continue;
        }
        let target = resolve(camera, name)?;
        if camera.node_kind(&target.feature) == Some("Command") {
            return Err(usage(format!("{name} is a command; run it with exec")));
        }
        let value = target
            .read(camera)
            .with_context(|| format!("read {name}"))?;
        values.insert(target.key(camera), value);
    }
    Ok(values)
}

/// A canonical control's current value, in the form `set` takes.
fn control_value(camera: &mut Camera, control: &str) -> Result<Value> {
    let read_f64 = |camera: &mut Camera, feature: &str| -> Result<f64> {
        camera
            .get(feature)?
            .as_f64()
            .with_context(|| format!("{feature} is not a number"))
    };
    Ok(match control {
        "exposure" => {
            let feature = first_present(camera, &EXPOSURE)
                .ok_or_else(|| unsupported(camera, control, "exposure"))?;
            let unit = camera.feature_unit(feature).unwrap_or("us").to_owned();
            let value = read_f64(camera, feature)?;
            let us = in_time_unit(Duration::from_secs(1), &unit)
                .map_or(value, |per_second| value * 1e6 / per_second);
            json!(duration_text(us))
        }
        "gain" => {
            let feature = first_present(camera, &GAIN)
                .or_else(|| first_present(camera, &["GainRaw"]))
                .ok_or_else(|| unsupported(camera, control, "gain"))?;
            let bank = gain_bank(camera);
            with_bank(camera, &bank, |camera| camera.get(feature))?
        }
        "fps" => {
            let feature = first_present(camera, &FRAME_RATE)
                .ok_or_else(|| unsupported(camera, control, "frame-rate"))?;
            camera.get(feature)?
        }
        "format" => {
            if !camera.has("PixelFormat") {
                return Err(unsupported(camera, control, "pixel format"));
            }
            json!(pixel_format(camera)?)
        }
        "roi" => {
            let mut part = |feature: &str| -> Result<i64> {
                if !camera.has(feature) {
                    return Ok(0);
                }
                Ok(read_f64(camera, feature)? as i64)
            };
            json!(format!(
                "{},{},{},{}",
                part("OffsetX")?,
                part("OffsetY")?,
                part("Width")?,
                part("Height")?
            ))
        }
        "trigger" => {
            if !camera.has("TriggerMode") {
                return Err(unsupported(camera, control, "trigger"));
            }
            let trigger = trigger_state(camera)?;
            json!(trigger_text(&trigger))
        }
        "auto_exposure" | "auto_wb" => {
            let feature = if control == "auto_exposure" {
                "ExposureAuto"
            } else {
                "BalanceWhiteAuto"
            };
            if !camera.has(feature) {
                return Err(unsupported(camera, control, feature));
            }
            json!(text(&camera.get(feature)?).to_ascii_lowercase())
        }
        _ => unreachable!("only canonical controls have values"),
    })
}

/// The start trigger as `info` reports it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Trigger {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activation: Option<String>,
}
pub(crate) fn trigger_state(camera: &mut Camera) -> Result<Trigger> {
    let bank = trigger_bank(camera);
    with_bank(camera, &bank, |camera| {
        let read = |camera: &mut Camera, feature: &str| {
            camera
                .has(feature)
                .then(|| camera.get(feature).ok().map(|v| text(&v)))
                .flatten()
        };
        Ok(Trigger {
            selector: bank.first().map(|(_, value)| value.clone()),
            mode: read(camera, "TriggerMode").unwrap_or_else(|| "Off".into()),
            source: read(camera, "TriggerSource"),
            activation: read(camera, "TriggerActivation"),
        })
    })
}
fn trigger_text(trigger: &Trigger) -> String {
    if trigger.mode != "On" {
        return "off".into();
    }
    let source = trigger
        .source
        .as_deref()
        .unwrap_or("software")
        .to_ascii_lowercase();
    match trigger.activation.as_deref() {
        Some("RisingEdge") if source != "software" => format!("{source}:rising"),
        Some("FallingEdge") if source != "software" => format!("{source}:falling"),
        _ => source,
    }
}

/// Run a command feature, at its bank.
pub(crate) fn exec(camera: &mut Camera, name: &Name) -> Result<String> {
    name.validate()?;
    if control_name(&name.name) {
        return Err(usage(format!(
            "{} is a control; change it with set",
            name.name
        )));
    }
    if STREAM_COMMAND_PREFIXES
        .iter()
        .any(|prefix| name.name.starts_with(prefix))
    {
        return Err(Categorized::new(
            ErrorKind::Usage,
            format!(
                "{} changes acquisition, which capture and sessions own",
                name.name
            ),
        )
        .hint("use capture, or keep a camera streaming with a session")
        .into());
    }
    let target = resolve(camera, name)?;
    if camera.node_kind(&target.feature) != Some("Command") {
        return Err(usage(format!(
            "{} is not a command; change it with set",
            target.feature
        )));
    }
    let key = target.key(camera);
    with_bank(camera, &target.bank, |camera| {
        camera.execute(&target.feature)
    })?;
    Ok(key)
}

/// A control as `info` describes it.
#[derive(Debug, Clone, Serialize)]
pub struct ControlInfo {
    pub feature: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

/// What an agent needs before capturing, without a feature dump.
#[derive(Debug, Clone, Serialize)]
pub struct Info {
    pub camera: String,
    pub kind: &'static str,
    pub vendor: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pixel_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bits: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fps_limit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposure_us: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gain_db: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gain: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gain_unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<Trigger>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp_hz: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ptp: Option<String>,
    pub controls: serde_json::Map<String, Value>,
}
impl Info {
    pub const FIELDS: &[(&str, &str)] = &[
        ("camera", "string — camera ID"),
        ("kind", "gige|u3v|webcam|screen|onvif|stream|file|sim"),
        ("vendor", "string"),
        ("model", "string"),
        ("serial", "string — when the camera reports one"),
        (
            "firmware",
            "string — DeviceFirmwareVersion or DeviceVersion",
        ),
        ("address", "string — IP address or device path"),
        ("width", "int — image width in pixels"),
        ("height", "int — image height in pixels"),
        ("pixel_format", "string — PFNC name such as Mono8"),
        ("bits", "int — bits per sample"),
        ("fps", "float — the rate the camera reports it runs at"),
        (
            "fps_limit",
            "float — AcquisitionFrameRate when its limiter is on",
        ),
        ("exposure_us", "float — ExposureTime in microseconds"),
        ("gain_db", "float — Gain when its unit is dB"),
        ("gain", "float — Gain in gain_unit when not dB"),
        ("gain_unit", "string — the unit of gain"),
        (
            "trigger",
            "{selector,mode,source,activation} — the start trigger",
        ),
        ("timestamp_hz", "float — device timestamp ticks per second"),
        ("ptp", "string — PTP state when the camera reports one"),
        (
            "controls",
            "{name:{feature,unit,min,max,value}} — canonical controls this camera has",
        ),
    ];
}

fn number(camera: &mut Camera, names: &[&str]) -> Option<f64> {
    names.iter().find_map(|name| {
        camera
            .has(name)
            .then(|| camera.get(name).ok()?.as_f64())
            .flatten()
            .filter(|v| v.is_finite())
    })
}
fn string(camera: &mut Camera, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        camera
            .has(name)
            .then(|| camera.get(name).ok().map(|v| text(&v)))
            .flatten()
            .filter(|v| !v.trim().is_empty())
    })
}

/// PixelFormat by its PFNC name, also where the node is an integer PFNC
/// code (FFmpeg sources); a code without a name reads as hex.
fn pixel_format(camera: &mut Camera) -> Result<String> {
    Ok(match camera.get("PixelFormat")? {
        Value::Number(code) => {
            let code = code
                .as_u64()
                .and_then(|code| u32::try_from(code).ok())
                .context("PixelFormat is not a PFNC code")?;
            match crate::frame::pixel_format_name(code) {
                "Unknown" => format!("0x{code:08x}"),
                name => name.to_owned(),
            }
        }
        value => text(&value),
    })
}

/// Bits per sample of a PFNC pixel format name: Mono12p is 12, RGB8 is 8.
/// None for a hex code.
pub(crate) fn bits(pixel_format: &str) -> Option<u32> {
    if pixel_format.starts_with("0x") {
        return None;
    }
    let digits = pixel_format
        .trim_end_matches(|c: char| c.is_ascii_alphabetic())
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    digits.chars().rev().collect::<String>().parse().ok()
}

pub(crate) fn info(camera: &mut Camera) -> Info {
    let pixel_format = camera
        .has("PixelFormat")
        .then(|| pixel_format(camera).ok())
        .flatten()
        .filter(|format| !format.trim().is_empty());
    let exposure = first_present(camera, &EXPOSURE);
    let exposure_us = exposure.and_then(|feature| {
        let unit = camera.feature_unit(feature).unwrap_or("us").to_owned();
        let value = number(camera, &[feature])?;
        Some(in_time_unit(Duration::from_secs(1), &unit).map_or(value, |per| value * 1e6 / per))
    });
    let gain_feature = first_present(camera, &GAIN);
    let gain_bank = gain_bank(camera);
    let gain = gain_feature.and_then(|feature| {
        with_bank(camera, &gain_bank, |camera| {
            Ok(camera.get(feature)?.as_f64())
        })
        .ok()
        .flatten()
    });
    let gain_unit =
        gain_feature.and_then(|feature| camera.feature_unit(feature).map(str::to_owned));
    let db = gain_unit
        .as_deref()
        .is_some_and(|unit| unit.trim().eq_ignore_ascii_case("db"));
    let limit_on = first_present(camera, &FRAME_RATE_ENABLE)
        .map(|enable| {
            camera
                .get(enable)
                .is_ok_and(|v| v == json!(true) || v == json!("On"))
        })
        .unwrap_or(true);
    let fps_limit = limit_on.then(|| number(camera, &FRAME_RATE)).flatten();
    let trigger = camera
        .has("TriggerMode")
        .then(|| trigger_state(camera).ok())
        .flatten();
    let mut controls = serde_json::Map::new();
    for (control, _) in CONTROLS {
        let feature = match control {
            "exposure" => exposure,
            "gain" => gain_feature.or_else(|| first_present(camera, &["GainRaw"])),
            "fps" => first_present(camera, &FRAME_RATE),
            "roi" => first_present(camera, &["Width"]).filter(|_| camera.has("Height")),
            "format" => first_present(camera, &["PixelFormat"]),
            "trigger" => first_present(camera, &["TriggerMode"]),
            "auto_exposure" => first_present(camera, &["ExposureAuto"]),
            _ => first_present(camera, &["BalanceWhiteAuto"]),
        };
        let Some(feature) = feature else { continue };
        let bounds = if matches!(control, "exposure" | "gain" | "fps") {
            camera.bounds(feature).unwrap_or_default()
        } else {
            Bounds::default()
        };
        controls.insert(
            control.to_owned(),
            json!(ControlInfo {
                feature: feature.to_owned(),
                unit: camera.feature_unit(feature).map(str::to_owned),
                min: bounds.min,
                max: bounds.max,
                value: control_value(camera, control).ok(),
            }),
        );
    }
    let info = camera.info.clone();
    Info {
        camera: crate::media::redact_url(&info.id),
        kind: crate::camera::kind(&info),
        vendor: info.vendor,
        model: info.model,
        serial: (!info.serial.is_empty() && info.transport != Transport::Media)
            .then_some(info.serial),
        firmware: string(camera, &["DeviceFirmwareVersion", "DeviceVersion"]),
        address: info
            .address
            .filter(|_| info.transport != Transport::Media)
            .map(|a| crate::media::redact_url(&a)),
        width: number(camera, &["Width"]).map(|v| v as u64),
        height: number(camera, &["Height"]).map(|v| v as u64),
        bits: pixel_format.as_deref().and_then(bits),
        pixel_format,
        fps: number(camera, &RESULTING_RATE),
        fps_limit,
        exposure_us,
        gain_db: gain.filter(|_| db),
        gain: gain.filter(|_| !db),
        gain_unit: gain_unit.filter(|_| !db && gain.is_some()),
        trigger,
        timestamp_hz: number(
            camera,
            &["GevTimestampTickFrequency", "DeviceTimestampFrequency"],
        ),
        ptp: string(camera, &["PtpStatus", "GevIEEE1588Status"]),
        controls,
    }
}

/// Which features `features` lists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureFilter {
    /// A substring, or a glob with `*` and `?`, ignoring case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// Every visibility level, not only Beginner; a pattern or category
    /// searches every level too.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}
impl FeatureFilter {
    pub fn validate(&self) -> Result<()> {
        for text in [&self.pattern, &self.category].into_iter().flatten() {
            if text.is_empty() || text.len() > 256 || text.chars().any(char::is_control) {
                return Err(usage(
                    "a pattern or category must be 1..256 bytes without control characters",
                ));
            }
        }
        Ok(())
    }
}

/// Features an agent commonly needs, listed even when the XML marks them
/// Expert or Guru.
const COMMON: [&str; 20] = [
    "ExposureTime",
    "ExposureTimeAbs",
    "ExposureAuto",
    "Gain",
    "GainAbs",
    "GainRaw",
    "GainAuto",
    "BalanceWhiteAuto",
    "AcquisitionFrameRate",
    "AcquisitionFrameRateAbs",
    "AcquisitionFrameRateEnable",
    "PixelFormat",
    "Width",
    "Height",
    "OffsetX",
    "OffsetY",
    "TriggerSelector",
    "TriggerMode",
    "TriggerSource",
    "TriggerActivation",
];

/// A pattern match ignoring case: a glob when it has `*` or `?`, else a
/// substring.
fn matches_pattern(pattern: &str, name: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    if !pattern.contains(['*', '?']) {
        return name.contains(&pattern);
    }
    fn glob(pattern: &[u8], name: &[u8]) -> bool {
        match (pattern.first(), name.first()) {
            (None, None) => true,
            (Some(b'*'), _) => {
                glob(&pattern[1..], name) || (!name.is_empty() && glob(pattern, &name[1..]))
            }
            (Some(b'?'), Some(_)) => glob(&pattern[1..], &name[1..]),
            (Some(a), Some(b)) if a == b => glob(&pattern[1..], &name[1..]),
            _ => false,
        }
    }
    glob(pattern.as_bytes(), name.as_bytes())
}

/// One `features` row.
#[derive(Debug, Clone, Serialize)]
pub struct FeatureRow {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub access: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inc: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub selectors: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locked_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

/// The `features` result.
#[derive(Debug, Clone, Serialize)]
pub struct FeatureList {
    pub camera: String,
    pub features: Vec<FeatureRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<usize>,
}
impl FeatureList {
    pub const FIELDS: &[(&str, &str)] = &[
        ("camera", "string — camera ID"),
        (
            "features",
            "[{name,type:int|float|bool|enum|string|command,access:rw|ro|wo,value,unit,min,max,inc,choices,selectors,locked_by,category}] — Beginner features and common controls; every level with --all, a PATTERN or --category; locked_by when another setting locks it now",
        ),
        (
            "truncated",
            "int — rows left out beyond the first 200; narrow with PATTERN or --category",
        ),
    ];
}

fn normalized_type(kind: &str) -> &'static str {
    match kind {
        "Integer" | "IntReg" | "MaskedIntReg" | "IntSwissKnife" | "IntConverter" => "int",
        "Float" | "FloatReg" | "SwissKnife" | "Converter" => "float",
        "Boolean" => "bool",
        "Enumeration" => "enum",
        "String" | "StringReg" => "string",
        "Command" => "command",
        _ => "string",
    }
}

pub(crate) fn features(camera: &mut Camera, filter: &FeatureFilter) -> Result<FeatureList> {
    filter.validate()?;
    let names = camera
        .feature_names()
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let selected = names
        .into_iter()
        // A pattern or category is a search: it looks at every level.
        .filter(|name| {
            filter.all
                || filter.pattern.is_some()
                || filter.category.is_some()
                || camera.visibility(name) == "Beginner"
                || COMMON.contains(&name.as_str())
        })
        .filter(|name| {
            filter
                .pattern
                .as_deref()
                .is_none_or(|pattern| matches_pattern(pattern, name))
        })
        .filter(|name| {
            filter.category.as_deref().is_none_or(|wanted| {
                camera
                    .category(name)
                    .is_some_and(|category| category.eq_ignore_ascii_case(wanted))
            })
        })
        .collect::<Vec<_>>();
    if selected.is_empty()
        && let Some(category) = &filter.category
        && !camera.feature_names().iter().any(|name| {
            camera
                .category(name)
                .is_some_and(|c| c.eq_ignore_ascii_case(category))
        })
    {
        return Err(Categorized::new(
            ErrorKind::FeatureNotFound,
            format!("{} has no category {category}", camera.info.model),
        )
        .into());
    }
    let truncated = selected.len().saturating_sub(MAX_FEATURE_ROWS);
    let mut rows = Vec::new();
    for name in selected.into_iter().take(MAX_FEATURE_ROWS) {
        let Some(info) = camera.feature(&name) else {
            continue;
        };
        let access = camera.static_access(&name);
        let kind = normalized_type(&info.kind);
        let locked_by = (access == "rw" && !info.writable && kind != "command")
            .then(|| camera.lock_owner(&name))
            .flatten();
        rows.push(FeatureRow {
            selectors: real_selectors(camera, &name),
            category: camera.category(&name).map(str::to_owned),
            name,
            kind,
            access,
            value: info.value,
            unit: info.unit,
            min: info.min,
            max: info.max,
            inc: info.inc,
            choices: info.choices,
            locked_by,
        });
    }
    Ok(FeatureList {
        camera: crate::media::redact_url(&camera.info.id),
        features: rows,
        truncated: (truncated > 0).then_some(truncated),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sim() -> Camera {
        Camera::open("sim:0", &[], Duration::from_secs(2)).unwrap()
    }
    fn assign(text: &str) -> Assignment {
        Assignment::parse(text).unwrap()
    }

    #[test]
    fn names_parse_banks_and_controls() {
        let name = Name::parse("TriggerMode[TriggerSelector=FrameStart]").unwrap();
        assert_eq!(name.name, "TriggerMode");
        assert_eq!(
            name.bank,
            [BankEntry {
                selector: Some("TriggerSelector".into()),
                value: "FrameStart".into()
            }]
        );
        assert_eq!(name.to_string(), "TriggerMode[TriggerSelector=FrameStart]");
        let short = Name::parse("BalanceRatio[Red]").unwrap();
        assert_eq!(short.bank[0].selector, None);
        let assignment = assign("TriggerMode[FrameStart]=On");
        assert_eq!(
            (assignment.name.as_str(), assignment.value.as_str()),
            ("TriggerMode", "On")
        );
        assert_eq!(assign("exposure=5ms").value, "5ms");
        assert_eq!(assign("A=B=C").value, "B=C");
        for bad in [
            "",
            "Exposure Time",
            "Mode[",
            "Mode[]",
            "Mode[A=]",
            "exposure[x]",
            "a[b][c]",
        ] {
            assert!(Name::parse(bad).is_err(), "{bad}");
        }
        for bad in ["oops", "=5", "Width"] {
            let error = Assignment::parse(bad).unwrap_err();
            assert_eq!(
                crate::types::classify(&error).kind,
                ErrorKind::Usage,
                "{bad}"
            );
        }
    }

    #[test]
    fn get_resolves_controls_and_bank_shorthand() {
        let mut camera = sim();
        let names = [
            "Width",
            "exposure",
            "TriggerMode[FrameStart]",
            "trigger",
            "roi",
        ]
        .map(|n| Name::parse(n).unwrap());
        let values = get(&mut camera, &names).unwrap();
        assert_eq!(values["Width"], 640);
        assert_eq!(values["exposure"], "10ms");
        assert_eq!(values["TriggerMode[FrameStart]"], "Off");
        assert_eq!(values["trigger"], "off");
        assert_eq!(values["roi"], "0,0,640,480");
        let explicit = get(
            &mut camera,
            &[Name::parse("TriggerMode[TriggerSelector=AcquisitionStart]").unwrap()],
        )
        .unwrap();
        assert!(explicit.contains_key("TriggerMode[AcquisitionStart]"));
        let missing = get(&mut camera, &[Name::parse("ExposureTme").unwrap()]).unwrap_err();
        let classified = crate::types::classify(&missing);
        assert_eq!(classified.kind, ErrorKind::FeatureNotFound);
        assert_eq!(
            classified.details.unwrap()["did_you_mean"][0],
            "ExposureTime"
        );
    }

    #[test]
    fn set_converts_controls_and_durations_and_reports_changes() {
        let mut camera = sim();
        let result = set(
            &mut camera,
            &[assign("exposure=5ms"), assign("Gain=3")],
            false,
        )
        .unwrap();
        let changed = result.changed.unwrap();
        assert_eq!(changed[0].name, "ExposureTime");
        assert_eq!(changed[0].to, json!(5000.0));
        assert_eq!(camera.get("Gain").unwrap(), json!(3.0));
        set(&mut camera, &[assign("ExposureTime=2.5ms")], false).unwrap();
        assert_eq!(camera.get("ExposureTime").unwrap(), json!(2500.0));
        // Setting a value it has already writes nothing.
        let again = set(&mut camera, &[assign("ExposureTime=2500")], false).unwrap();
        assert!(again.changed.unwrap().is_empty());
        let trigger = set(&mut camera, &[assign("trigger=line0:falling")], false).unwrap();
        let names = trigger
            .changed
            .unwrap()
            .into_iter()
            .map(|change| change.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "TriggerActivation[FrameStart]",
                "TriggerSource[FrameStart]",
                "TriggerMode[FrameStart]"
            ]
        );
        // The bank written is restored as selected afterwards.
        assert_eq!(camera.get("TriggerSelector").unwrap(), "FrameStart");
        let values = get(&mut camera, &[Name::parse("trigger").unwrap()]).unwrap();
        assert_eq!(values["trigger"], "line0:falling");
    }

    #[test]
    fn an_auto_that_locks_a_value_is_turned_off_in_the_same_change() {
        let mut camera = sim();
        camera.set("ExposureAuto", "Once").unwrap();
        assert!(camera.is_locked("ExposureTime"));
        let plan = set(&mut camera, &[assign("ExposureTime=4000")], true).unwrap();
        let plan = plan.plan.unwrap();
        assert_eq!(plan[0].name, "ExposureAuto");
        assert_eq!(plan[0].to, "Off");
        assert_eq!(
            camera.get("ExposureAuto").unwrap(),
            "Once",
            "a dry run writes nothing"
        );
        let changed = set(&mut camera, &[assign("ExposureTime=4000")], false)
            .unwrap()
            .changed
            .unwrap();
        assert_eq!(changed.len(), 2);
        assert_eq!(camera.get("ExposureTime").unwrap(), json!(4000.0));
    }

    #[test]
    fn a_failed_change_restores_every_value_it_wrote() {
        let mut camera = sim();
        let before = camera.get("ExposureTime").unwrap();
        let error = set(
            &mut camera,
            &[assign("ExposureTime=4000"), assign("Gain=99")],
            false,
        )
        .unwrap_err();
        let classified = crate::types::classify(&error);
        assert_eq!(classified.kind, ErrorKind::OutOfRange);
        let details = classified.details.unwrap();
        assert_eq!(details["max"], 24.0);
        assert_eq!(details["unit"], "dB");
        assert_eq!(camera.get("ExposureTime").unwrap(), before);
        // The checks before writing catch it: nothing was written.
        assert!(details.get("rollback").is_none(), "{details}");
        let error = set(&mut camera, &[assign("PixelFormat=Mono99")], false).unwrap_err();
        let classified = crate::types::classify(&error);
        assert_eq!(classified.kind, ErrorKind::InvalidChoice);
        assert!(
            classified.details.unwrap()["choices"]
                .as_array()
                .unwrap()
                .len()
                > 1
        );
        let error = set(&mut camera, &[assign("PayloadSize=5")], false).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::NotWritable);
    }

    /// Width with a fixed maximum, and Height whose maximum a register
    /// the XML does not name sets, as on cameras where it follows other
    /// settings.
    const GEOMETRY: &str = r#"<Integer Name="Width"><pValue>WidthReg</pValue><Min>16</Min><Max>1920</Max></Integer><IntReg Name="WidthReg"><Address>0x64</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></IntReg><Integer Name="Height"><pValue>HeightReg</pValue><Min>16</Min><pMax>HeightLimit</pMax></Integer><IntReg Name="HeightReg"><Address>0x68</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></IntReg><IntReg Name="HeightLimit"><Address>0x6c</Address><Length>4</Length><AccessMode>RO</AccessMode><Endianess>LittleEndian</Endianess></IntReg>"#;

    #[test]
    fn a_value_only_the_writes_can_check_is_a_warning_then_rolled_back() {
        use crate::camera::tests::{Device, device_camera};
        let device = Device::new(GEOMETRY)
            .with(0x64, 640)
            .with(0x68, 480)
            .with(0x6c, 480);
        let mut camera = device_camera(device);
        let values = [assign("Width=128"), assign("Height=1000")];
        // Width may move Height's maximum, so a dry run cannot decide it.
        let plan = set(&mut camera, &values, true).unwrap();
        assert_eq!(plan.plan.unwrap().len(), 2);
        assert_eq!(plan.warnings.len(), 1, "{:?}", plan.warnings);
        assert!(
            plan.warnings[0].starts_with("Height=1000"),
            "{:?}",
            plan.warnings
        );
        // Writing shows it does not fit: Width is put back.
        let error = set(&mut camera, &values, false).unwrap_err();
        let classified = crate::types::classify(&error);
        assert_eq!(classified.kind, ErrorKind::OutOfRange);
        assert_eq!(
            classified.details.unwrap()["rollback"],
            json!({"restored":["Width"],"failed":[]})
        );
        assert_eq!(camera.get("Width").unwrap(), 640);
        // Past a maximum the XML fixes, it fails before writing.
        let error = set(
            &mut camera,
            &[assign("Height=100"), assign("Width=5000")],
            true,
        )
        .unwrap_err();
        let details = crate::types::classify(&error).details.unwrap();
        assert_eq!(details["max"], 1920.0);
        assert!(details.get("rollback").is_none());
    }

    #[test]
    fn a_feature_without_a_bank_is_written_at_the_bank_selected_before_it() {
        let mut camera = sim();
        set(&mut camera, &[assign("TriggerMode[FrameStart]=On")], false).unwrap();
        set(
            &mut camera,
            &[assign("TriggerSelector=AcquisitionStart")],
            false,
        )
        .unwrap();
        let changed = set(
            &mut camera,
            &[
                assign("TriggerSelector=FrameStart"),
                assign("TriggerMode=Off"),
            ],
            false,
        )
        .unwrap()
        .changed
        .unwrap();
        let names = changed.iter().map(|c| c.name.as_str()).collect::<Vec<_>>();
        assert_eq!(names, ["TriggerMode[FrameStart]", "TriggerSelector"]);
        let read = |camera: &mut Camera, name: &str| {
            get(camera, &[Name::parse(name).unwrap()]).unwrap()[name].clone()
        };
        assert_eq!(read(&mut camera, "TriggerMode[FrameStart]"), "Off");
        assert_eq!(read(&mut camera, "TriggerSelector"), "FrameStart");
        // The same register named twice, with and without its bank.
        let error = set(
            &mut camera,
            &[
                assign("TriggerMode=On"),
                assign("TriggerMode[FrameStart]=Off"),
            ],
            true,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("set twice"), "{error:#}");
    }

    #[test]
    fn a_dry_run_refuses_what_the_change_would_refuse() {
        let mut camera = sim();
        // An auto left on keeps the value it drives locked.
        camera.set("ExposureAuto", "Once").unwrap();
        let error = set(
            &mut camera,
            &[assign("ExposureAuto=Once"), assign("ExposureTime=3000")],
            true,
        )
        .unwrap_err();
        assert_eq!(
            crate::types::classify(&error).kind,
            ErrorKind::FeatureLocked
        );
        // Switched off in the same change, the value is checked in full.
        let error = set(
            &mut camera,
            &[assign("ExposureAuto=Off"), assign("ExposureTime=5000000")],
            true,
        )
        .unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::OutOfRange);
        let plan = set(
            &mut camera,
            &[assign("ExposureAuto=Off"), assign("ExposureTime=3000")],
            true,
        )
        .unwrap();
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        // Geometry beyond what any write of the change can allow.
        camera.set("ExposureAuto", "Off").unwrap();
        for values in [
            [assign("Width=128"), assign("Height=5000")],
            [assign("PixelFormat=RGB8"), assign("Width=99999")],
        ] {
            let error = set(&mut camera, &values, true).unwrap_err();
            assert_eq!(crate::types::classify(&error).kind, ErrorKind::OutOfRange);
        }
    }

    #[test]
    fn rollback_reports_what_it_restored() {
        let mut camera = sim();
        let before = camera.get("Gain").unwrap();
        camera.set("Gain", "2").unwrap();
        let written = vec![(Target::plain("Gain"), before.clone())];
        let error = rollback(
            &mut camera,
            written,
            Categorized::error(ErrorKind::OutOfRange, "Width is too large"),
        );
        let classified = crate::types::classify(&error);
        assert_eq!(classified.kind, ErrorKind::OutOfRange);
        assert_eq!(
            classified.details.unwrap()["rollback"],
            json!({"restored":["Gain"],"failed":[]})
        );
        assert_eq!(camera.get("Gain").unwrap(), before);
    }

    #[test]
    fn missing_controls_are_unsupported_and_banks_must_resolve() {
        let mut camera = Camera::open("sim:depth", &[], Duration::from_secs(2)).unwrap();
        let error = set(&mut camera, &[assign("exposure=5ms")], false).unwrap_err();
        assert_eq!(
            crate::types::classify(&error).kind.code(),
            ("unsupported", 5)
        );
        let mut camera = sim();
        let error = get(&mut camera, &[Name::parse("Width[FrameStart]").unwrap()]).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Usage);
        let error = exec(&mut camera, &Name::parse("AcquisitionStart").unwrap()).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Usage);
        assert_eq!(
            exec(
                &mut camera,
                &Name::parse("TriggerSoftware[FrameStart]").unwrap()
            )
            .unwrap(),
            "TriggerSoftware[FrameStart]"
        );
    }

    #[test]
    fn features_rows_are_normalized_and_filtered() {
        let mut camera = sim();
        camera.set("ExposureAuto", "Once").unwrap();
        let list = features(&mut camera, &FeatureFilter::default()).unwrap();
        let row = |name: &str| list.features.iter().find(|row| row.name == name).unwrap();
        assert_eq!(row("Width").kind, "int");
        assert_eq!(row("DeviceScanType").access, "ro");
        assert_eq!(row("TriggerMode").selectors, ["TriggerSelector"]);
        assert_eq!(
            row("ExposureTime").locked_by.as_deref(),
            Some("ExposureAuto")
        );
        assert_eq!(row("Width").category.as_deref(), Some("ImageFormatControl"));
        let filtered = features(
            &mut camera,
            &FeatureFilter {
                pattern: Some("trig*mode".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(filtered.features.len(), 1);
        assert_eq!(bits("Mono12p"), Some(12));
        assert_eq!(bits("RGB8"), Some(8));
        assert_eq!(bits("Coord3D_C16"), Some(16));
    }

    #[test]
    fn info_describes_the_controls() {
        let mut camera = sim();
        let info = info(&mut camera);
        assert_eq!(info.kind, "sim");
        assert_eq!(info.exposure_us, Some(10_000.0));
        assert_eq!(info.gain_db, Some(0.0));
        assert_eq!(info.bits, Some(8));
        assert_eq!(info.trigger.as_ref().unwrap().mode, "Off");
        assert!(info.controls.contains_key("exposure"));
        let fields = serde_json::to_value(&info).unwrap();
        for key in fields.as_object().unwrap().keys() {
            assert!(
                Info::FIELDS.iter().any(|(name, _)| name == key),
                "{key} is undocumented"
            );
        }
    }
}
