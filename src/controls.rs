//! Feature names as commands take them, and the canonical controls an agent
//! can use without knowing a camera's vendor names. Names are parsed on the
//! client; banks and controls are resolved, planned and written in the
//! camera worker, against the live node map.
//!
//! A name is a `GenICam` feature (`ExposureTime`), a feature at a bank
//! (`TriggerMode[TriggerSelector=FrameStart]`, or `TriggerMode[FrameStart]`
//! when one selector addresses it) or a lowercase canonical control
//! (`exposure`, `gain`, `fps`, `roi`, `format`, `trigger`, `auto_exposure`,
//! `auto_wb`), which cannot collide with SFNC names.
#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::panic, clippy::unreachable))]
use crate::camera::Camera;
use crate::genicam::Bounds;
use crate::types::{Categorized, ErrorKind, Transport, Usage};
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::time::Duration;

/// Exposure features, preferred first.
pub(crate) const EXPOSURE: [&str; 2] = ["ExposureTime", "ExposureTimeAbs"];
/// Gain features in dB or a multiplier, preferred first; `GainRaw` is the
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
pub(crate) const MAX_FEATURE_ROWS: usize = 200;

/// A canonical control, distinct from a camera's vendor feature names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Control {
    Exposure,
    Gain,
    Fps,
    Roi,
    Format,
    Trigger,
    AutoExposure,
    AutoWb,
}
impl Control {
    const ALL: [Self; 8] = [
        Self::Exposure,
        Self::Gain,
        Self::Fps,
        Self::Roi,
        Self::Format,
        Self::Trigger,
        Self::AutoExposure,
        Self::AutoWb,
    ];
    const fn name(self) -> &'static str {
        match self {
            Self::Exposure => "exposure",
            Self::Gain => "gain",
            Self::Fps => "fps",
            Self::Roi => "roi",
            Self::Format => "format",
            Self::Trigger => "trigger",
            Self::AutoExposure => "auto_exposure",
            Self::AutoWb => "auto_wb",
        }
    }
    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|control| control.name() == name)
    }
}

/// The canonical controls, in the order `info` lists them.
pub(crate) const CONTROLS: [(&str, &str); 8] = [
    (
        Control::Exposure.name(),
        "exposure time as a duration: 500us, 10ms",
    ),
    (
        Control::Gain.name(),
        "gain in dB when the camera's unit is dB, else its native unit",
    ),
    (
        Control::Fps.name(),
        "frame-rate limit in frames per second; turns the limiter on",
    ),
    (Control::Roi.name(), "region x,y,w,h in pixels, or full"),
    (
        Control::Format.name(),
        "pixel format, such as Mono8 or BayerRG8",
    ),
    (
        Control::Trigger.name(),
        "off, software, or a line: line0[:rising|falling]",
    ),
    (
        Control::AutoExposure.name(),
        "the camera's own auto exposure: off, once, continuous",
    ),
    (
        Control::AutoWb.name(),
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
    Control::parse(name).is_some()
}

/// A feature or selector name: a `GenICam` identifier.
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
            f.write_str("[")?;
            for (index, entry) in self.bank.iter().enumerate() {
                if index > 0 {
                    f.write_str(",")?;
                }
                if let Some(selector) = &entry.selector {
                    write!(f, "{selector}=")?;
                }
                f.write_str(&entry.value)?;
            }
            f.write_str("]")?;
        }
        Ok(())
    }
}
impl Assignment {
    /// `NAME=VALUE`, split at the first `=` outside a bank.
    pub fn parse(text: &str) -> Result<Self> {
        let mut depth = 0usize;
        let split = text.split_once(|c| {
            match c {
                '[' => depth += 1,
                ']' => depth = depth.saturating_sub(1),
                '=' if depth == 0 => return true,
                _ => {}
            }
            false
        });
        let Some((name, value)) = split else {
            return Err(usage(format!(
                "'{text}' needs a value: NAME=VALUE, such as ExposureTime=10ms or exposure=5ms"
            )));
        };
        let name = Name::parse(name)?;
        let value = value.trim();
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
        let mut key = self.feature.clone();
        key.push('[');
        for (index, (selector, value)) in self.bank.iter().enumerate() {
            if index > 0 {
                key.push(',');
            }
            if !short {
                key.push_str(selector);
                key.push('=');
            }
            key.push_str(value);
        }
        key.push(']');
        key
    }
    fn read(&self, camera: &mut Camera) -> Result<Value> {
        with_bank(camera, &self.bank, |camera| camera.get(&self.feature))
    }
}

/// The selectors that address a feature's bank and that a command can set.
fn real_selectors(camera: &Camera, feature: &str) -> Vec<String> {
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
    let mut close = Vec::with_capacity(5);
    for name in camera
        .feature_names()
        .into_iter()
        .chain(CONTROLS.iter().map(|(control, _)| *control))
    {
        let Some(distance) = crate::hint::close_distance(typed, name) else {
            continue;
        };
        let candidate = (distance, name);
        if let Err(index) = close.binary_search(&candidate)
            && index < 5
        {
            if close.len() == 5 {
                close.pop();
            }
            close.insert(index, candidate);
        }
    }
    close.into_iter().map(|(_, name)| name.to_owned()).collect()
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
        let selector = if let Some(selector) = &entry.selector {
            if !camera.has(selector) {
                return Err(not_found(camera, selector));
            }
            selector.clone()
        } else {
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

/// The choice of `feature` equal to `wanted` ignoring ASCII case, `_` and `-`.
fn choice(camera: &mut Camera, feature: &str, wanted: &str) -> Option<String> {
    camera
        .choices(feature)
        .ok()?
        .into_iter()
        .find(|choice| same_choice(choice, wanted))
}

fn same_choice(left: &str, right: &str) -> bool {
    fn normalized(text: &str) -> impl Iterator<Item = u8> + '_ {
        text.bytes()
            .filter(|byte| !matches!(byte, b'_' | b'-'))
            .map(|byte| byte.to_ascii_lowercase())
    }
    normalized(left).eq(normalized(right))
}

/// The bank `GainSelector=All` (or `AnalogAll`) when the camera has one.
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

/// The `FrameStart` bank of the trigger features, when they have one.
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
    let scaled = value * 1e9;
    let rounded = if scaled.is_finite() {
        scaled.round() / 1e9
    } else {
        value
    };
    if rounded == 0.0 {
        "0".to_owned()
    } else {
        rounded.to_string()
    }
}

/// `amount` of time `unit` (`us`, `ms`, `s`, `ns`) as a duration `set`
/// takes back, to the nanosecond; None for any other unit.
fn time_text(amount: f64, unit: &str) -> Option<String> {
    let per_second = in_time_unit(Duration::from_secs(1), unit)?;
    let nanos = (amount * 1e9 / per_second).round();
    let nanos = u64::try_from(crate::types::integral_float(nanos)?).ok()?;
    Some(crate::types::duration_text(Duration::from_nanos(nanos)))
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
fn expand_control(camera: &mut Camera, kind: Control, value: &str) -> Result<Vec<Write>> {
    let control = kind.name();
    let write = |target: Target, value: String| Write {
        target,
        value,
        origin: control.to_owned(),
        implied: false,
    };
    Ok(match kind {
        Control::Exposure => {
            let feature = first_present(camera, &EXPOSURE)
                .ok_or_else(|| unsupported(camera, control, "exposure"))?;
            let duration = crate::types::parse_duration(value)?;
            let unit = camera.feature_unit(feature).unwrap_or("us").to_owned();
            let amount = in_time_unit(duration, &unit).unwrap_or(duration.as_secs_f64() * 1e6);
            vec![write(Target::plain(feature), number_text(amount))]
        }
        Control::Gain => {
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
        Control::Fps => {
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
        Control::Format => {
            if !camera.has("PixelFormat") {
                return Err(unsupported(camera, control, "pixel format"));
            }
            vec![write(Target::plain("PixelFormat"), value.trim().into())]
        }
        Control::Roi => roi(camera, value)?
            .into_iter()
            .map(|(feature, value)| write(Target::plain(feature), value))
            .collect(),
        Control::Trigger => {
            let lower = value.trim().to_ascii_lowercase();
            if !camera.has("TriggerMode") {
                return Err(unsupported(camera, control, "trigger"));
            }
            let bank = trigger_bank(camera);
            let at = |feature: &str| Target {
                feature: feature.into(),
                bank: bank.clone(),
            };
            if lower == "off" {
                // Every start trigger that holds frames back goes off.
                let mut writes = vec![write(at("TriggerMode"), "Off".into())];
                for other in other_start_banks(camera) {
                    let on = with_bank(camera, &other, |camera| {
                        Ok(camera.get("TriggerMode").is_ok_and(|mode| mode == "On"))
                    })?;
                    if on {
                        let target = Target {
                            feature: "TriggerMode".into(),
                            bank: other,
                        };
                        writes.push(write(target, "Off".into()));
                    }
                }
                return Ok(writes);
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
        Control::AutoExposure | Control::AutoWb => {
            let feature = if kind == Control::AutoExposure {
                "ExposureAuto"
            } else {
                "BalanceWhiteAuto"
            };
            if !camera.has(feature) {
                let what = if kind == Control::AutoExposure {
                    "auto exposure"
                } else {
                    "white balance"
                };
                return Err(unsupported(camera, control, what));
            }
            let Some(lower) = ["off", "once", "continuous"]
                .into_iter()
                .find(|candidate| value.trim().eq_ignore_ascii_case(candidate))
            else {
                return Err(usage(format!(
                    "{control} takes off, once or continuous, not '{value}'"
                )));
            };
            let Some(chosen) = choice(camera, feature, lower) else {
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
        let mut parts = value.split(',').map(|part| {
            part.trim()
                .parse::<f64>()
                .ok()
                .filter(|v| *v >= 0.0 && v.fract() == 0.0)
        });
        match (
            std::array::from_fn::<_, 4, _>(|_| parts.next().flatten()),
            parts.next(),
        ) {
            ([Some(x), Some(y), Some(w), Some(h)], None) => [x, y, w, h],
            _ => {
                return Err(usage(format!(
                    "roi takes x,y,w,h in whole pixels or full, not '{value}'"
                )));
            }
        }
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

/// The image geometry: features whose bounds another write of the same
/// change can move, and whose values it can clamp or rescale (a binning
/// scales the size and offsets, a width bounds its offset), so a change
/// checkpoints them as a group.
pub(crate) const GEOMETRY: [&str; 9] = [
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
        if let Some(control) = Control::parse(&assignment.name) {
            writes.extend(expand_control(camera, control, &assignment.value)?);
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
            if let Some(value) =
                earlier.or_else(|| camera.get(&selector).ok().map(|value| text(&value)))
            {
                bank.push((selector, value));
            } else {
                bank.clear();
                break;
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
            let (group, rank, _) =
                crate::setup::order_key(&write.target.feature, Some(&write.value));
            ((group, rank), index, write)
        })
        .collect::<Vec<_>>();
    keyed.sort_by(|a, b| {
        let roi = a.2.origin == "roi" && b.2.origin == "roi";
        if roi {
            a.1.cmp(&b.1)
        } else {
            (a.0, &a.2.target.feature, a.1).cmp(&(b.0, &b.2.target.feature, b.1))
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
    // A larger size may not fit beside the current offset: lower the
    // offset first. A larger offset may not fit beside the current size:
    // size first, as the geometry order has it.
    for (size, offset) in [("Width", "OffsetX"), ("Height", "OffsetY")] {
        let at = |feature: &str| writes.iter().position(|w| w.target.feature == feature);
        let (Some(size_at), Some(offset_at)) = (at(size), at(offset)) else {
            continue;
        };
        let current = camera.get(offset).ok().and_then(|v| v.as_f64());
        let wanted = writes[offset_at].value.trim().parse::<f64>().ok();
        if offset_at > size_at
            && matches!((current, wanted), (Some(current), Some(wanted)) if wanted < current)
        {
            let write = writes.remove(offset_at);
            writes.insert(size_at, write);
        }
    }
}

/// Boolean spellings, ignoring ASCII case without trimming whitespace.
fn boolean_setting(text: &str) -> Option<bool> {
    if ["true", "1", "on"]
        .iter()
        .any(|value| text.eq_ignore_ascii_case(value))
    {
        Some(true)
    } else if ["false", "0", "off"]
        .iter()
        .any(|value| text.eq_ignore_ascii_case(value))
    {
        Some(false)
    } else {
        None
    }
}

/// Whether two feature values are the same setting.
fn same(current: &Value, wanted: &str) -> bool {
    match current {
        Value::String(current) => current == wanted,
        Value::Bool(current) => boolean_setting(wanted) == Some(*current),
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
        Some(Value::Bool(_)) => json!(boolean_setting(wanted) == Some(true)),
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
            } else if boolean_setting(other.value.trim()) == Some(false) {
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
    (GEOMETRY.contains(&feature) && any(&GEOMETRY))
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
    let nearest = value.trim().parse::<f64>().ok().map(|number| {
        let number = min.map_or(number, |min| number.max(min));
        max.map_or(number, |max| number.min(max))
    });
    Categorized::new(
        ErrorKind::OutOfRange,
        format!("{named} cannot be {value}: it takes {limit}"),
    )
    .details(json!({"feature": named, "value": value, "min": min, "max": max, "nearest": nearest}))
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
                return Err(with_unit(
                    camera,
                    feature,
                    out_of_range(&named, &value, None, Some(max)),
                ));
            }
            Ok(Some(format!(
                "{named}={value} is outside its current range; set checks it again after the writes before it"
            )))
        }
        Err(error) => Err(with_unit(camera, feature, error)),
        checked => checked,
    }
}

/// Add the feature's unit to an out-of-range error's details, and a hint
/// with the nearest value it takes, written so `set` takes it back: a time
/// as a duration (`1s`, `10us`), which the exposure control and the feature
/// both read, and a number in another unit with that unit beside it.
fn with_unit(camera: &Camera, feature: &str, error: anyhow::Error) -> anyhow::Error {
    let classified = crate::types::classify(&error);
    if classified.kind != ErrorKind::OutOfRange {
        return error;
    }
    let mut details = classified.details.unwrap_or_else(|| json!({}));
    let unit = camera.feature_unit(feature).map(str::to_owned);
    if let Some(unit) = &unit {
        details["unit"] = json!(unit);
    }
    let hint = out_of_range_hint(&details, unit.as_deref());
    let mut categorized =
        Categorized::new(ErrorKind::OutOfRange, format!("{error:#}")).details(details);
    if let Some(hint) = hint {
        categorized = categorized.hint(hint);
    }
    categorized.into()
}

/// What an out-of-range error with `details` (feature, min, max, nearest)
/// tells a caller to send instead, for a feature in `unit`.
fn out_of_range_hint(details: &Value, unit: Option<&str>) -> Option<String> {
    let nearest = details["nearest"].as_f64()?;
    let time = |value: f64| unit.and_then(|unit| time_text(value, unit));
    let Some(nearest_time) = time(nearest) else {
        return Some(unit.map_or_else(
            || format!("the nearest accepted value is {}", number_text(nearest)),
            |unit| {
                format!(
                    "the nearest accepted value is {} ({unit})",
                    number_text(nearest)
                )
            },
        ));
    };
    let mut hint = format!("the nearest accepted value is {nearest_time}");
    let feature = details["feature"].as_str().unwrap_or("it");
    match (
        details["min"].as_f64().and_then(time),
        details["max"].as_f64().and_then(time),
    ) {
        (Some(min), Some(max)) => {
            let _ = write!(hint, "; {feature} takes {min} to {max}");
        }
        (Some(min), None) => {
            let _ = write!(hint, "; {feature} takes {min} or more");
        }
        (None, Some(max)) => {
            let _ = write!(hint, "; {feature} takes up to {max}");
        }
        (None, None) => {}
    }
    Some(hint)
}

/// One row of a change: what it was and what it is now.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Change {
    pub name: String,
    pub from: Value,
    pub to: Value,
}

/// The result of `set`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SetResult {
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
    pub(crate) const FIELDS: &[(&str, &str)] = &[
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
/// then the rest of the image geometry a write touched, and the error's
/// details say what the rollback did.
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
    let geometry = geometry_checkpoint(camera, &pending);
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
                return Err(rollback(camera, written, &geometry, &error));
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

/// The image geometry a change's writes can clamp or rescale, read before
/// any is made: every geometry feature the camera has, when the change
/// writes one. Values that cannot be read or are locked (an auto drives
/// them) are left out.
fn geometry_checkpoint(camera: &mut Camera, writes: &[Write]) -> Vec<(String, Value)> {
    if !writes
        .iter()
        .any(|write| GEOMETRY.contains(&write.target.feature.as_str()))
    {
        return Vec::new();
    }
    let mut values = Vec::new();
    for feature in GEOMETRY {
        if !camera.has(feature) || !camera.is_configuration_value(feature) {
            continue;
        }
        if camera.is_locked(feature) {
            continue;
        }
        if let Ok(value) = camera.get(feature) {
            values.push((feature.to_owned(), value));
        }
    }
    values
}

/// Restore `written` in reverse order, then any value of the `geometry`
/// checkpoint that a write moved (a binning rescales the size and offsets,
/// and scaling back can round them), and report it in `error`'s details.
fn rollback(
    camera: &mut Camera,
    written: Vec<(Target, Value)>,
    geometry: &[(String, Value)],
    error: &anyhow::Error,
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
    // One geometry value can bound or rescale another (an offset the
    // width, a binning the size), so the whole checkpoint is read again
    // after each pass of writes, and what differs is written again, for
    // as many passes as the group has values.
    let differing = |camera: &mut Camera| {
        geometry
            .iter()
            .filter(|(feature, value)| camera.get(feature).ok().as_ref() != Some(value))
            .collect::<Vec<_>>()
    };
    let mut left = differing(camera);
    let mut rewritten: Vec<&String> = Vec::new();
    let mut errors = std::collections::BTreeMap::new();
    for _ in 0..geometry.len() {
        if left.is_empty() {
            break;
        }
        for (feature, value) in &left {
            match camera.set(feature, &text(value)) {
                Ok(()) => errors.remove(feature),
                Err(error) => errors.insert(feature.clone(), format!("{error:#}")),
            };
            if !rewritten.contains(&feature) {
                rewritten.push(feature);
            }
        }
        left = differing(camera);
    }
    for feature in rewritten {
        if left.iter().any(|(other, _)| other == feature) {
            continue;
        }
        failed.retain(|failure| failure["name"] != feature.as_str());
        if !restored.contains(feature) {
            restored.push(feature.clone());
        }
    }
    for (feature, value) in left {
        failed.retain(|failure| failure["name"] != feature.as_str());
        let error = errors
            .remove(feature)
            .unwrap_or_else(|| format!("did not return to {}", text(value)));
        failed.push(json!({"name": feature, "error": error}));
    }
    let classified = crate::types::classify(error);
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
        if let Some(control) = Control::parse(&name.name) {
            values.insert(name.name.clone(), control_value(camera, control)?);
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
fn control_value(camera: &mut Camera, kind: Control) -> Result<Value> {
    let control = kind.name();
    let read_f64 = |camera: &mut Camera, feature: &str| -> Result<f64> {
        camera
            .get(feature)?
            .as_f64()
            .with_context(|| format!("{feature} is not a number"))
    };
    Ok(match kind {
        Control::Exposure => {
            let feature = first_present(camera, &EXPOSURE)
                .ok_or_else(|| unsupported(camera, control, "exposure"))?;
            let unit = camera.feature_unit(feature).unwrap_or("us").to_owned();
            let value = read_f64(camera, feature)?;
            let unit = if in_time_unit(Duration::ZERO, &unit).is_some() {
                unit.as_str()
            } else {
                "us"
            };
            json!(time_text(value, unit).with_context(|| format!(
                "{feature} is not a representable nonnegative duration"
            ))?)
        }
        Control::Gain => {
            let feature = first_present(camera, &GAIN)
                .or_else(|| first_present(camera, &["GainRaw"]))
                .ok_or_else(|| unsupported(camera, control, "gain"))?;
            let bank = gain_bank(camera);
            with_bank(camera, &bank, |camera| camera.get(feature))?
        }
        Control::Fps => {
            let feature = first_present(camera, &FRAME_RATE)
                .ok_or_else(|| unsupported(camera, control, "frame-rate"))?;
            camera.get(feature)?
        }
        Control::Format => {
            if !camera.has("PixelFormat") {
                return Err(unsupported(camera, control, "pixel format"));
            }
            json!(pixel_format(camera)?)
        }
        Control::Roi => {
            let mut part = |feature: &str| -> Result<u64> {
                if !camera.has(feature) {
                    return Ok(0);
                }
                camera
                    .get(feature)?
                    .as_u64()
                    .with_context(|| format!("{feature} is not a nonnegative integer pixel count"))
            };
            json!(format!(
                "{},{},{},{}",
                part("OffsetX")?,
                part("OffsetY")?,
                part("Width")?,
                part("Height")?
            ))
        }
        Control::Trigger => {
            if !camera.has("TriggerMode") {
                return Err(unsupported(camera, control, "trigger"));
            }
            let trigger = trigger_state(camera)?;
            json!(trigger_text(&trigger))
        }
        Control::AutoExposure | Control::AutoWb => {
            let feature = if kind == Control::AutoExposure {
                "ExposureAuto"
            } else {
                "BalanceWhiteAuto"
            };
            if !camera.has(feature) {
                return Err(unsupported(camera, control, feature));
            }
            json!(text(&camera.get(feature)?).to_ascii_lowercase())
        }
    })
}

/// The start trigger as `info` reports it.
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct Trigger {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activation: Option<String>,
}
impl Trigger {
    /// Whether frames wait for this trigger: it is on, with a source other
    /// than the camera's own timing.
    pub(crate) fn holds_frames(&self) -> bool {
        self.mode == "On" && !self.source.as_deref().is_some_and(camera_timed)
    }
}
/// Whether trigger source `source` is the camera's own timing, which sends
/// frames as they come with the trigger on: Allied Vision cameras ship with
/// `TriggerMode` On and `TriggerSource` Freerun, or `FixedRate` to run at
/// `AcquisitionFrameRateAbs`.
pub(crate) fn camera_timed(source: &str) -> bool {
    matches!(source, "Freerun" | "FixedRate")
}
/// The start trigger in effect: the `FrameStart` trigger, or when it holds
/// no frames back an acquisition or burst start trigger that does, else
/// the first that is on, as with a source that is the camera's own timing.
pub(crate) fn trigger_state(camera: &mut Camera) -> Result<Trigger> {
    let bank = trigger_bank(camera);
    let frame = bank_trigger(camera, &bank)?;
    if frame.holds_frames() {
        return Ok(frame);
    }
    let mut on = None;
    for bank in other_start_banks(camera) {
        let trigger = bank_trigger(camera, &bank)?;
        if trigger.holds_frames() {
            return Ok(trigger);
        }
        if trigger.mode == "On" && on.is_none() {
            on = Some(trigger);
        }
    }
    Ok(match on {
        Some(on) if frame.mode != "On" => on,
        _ => frame,
    })
}
/// The banks of the start triggers other than `FrameStart` that hold frames
/// back: `AcquisitionStart` and `FrameBurstStart`.
fn other_start_banks(camera: &mut Camera) -> Vec<Vec<(String, String)>> {
    if !camera.has("TriggerSelector") || !has_choice(camera, "TriggerSelector", "FrameStart") {
        return Vec::new();
    }
    ["AcquisitionStart", "FrameBurstStart"]
        .into_iter()
        .filter(|selector| has_choice(camera, "TriggerSelector", selector))
        .map(|selector| vec![("TriggerSelector".to_owned(), selector.to_owned())])
        .collect()
}
fn bank_trigger(camera: &mut Camera, bank: &[(String, String)]) -> Result<Trigger> {
    with_bank(camera, bank, |camera| {
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
pub(crate) struct ControlInfo {
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
pub(crate) struct Info {
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
    pub(crate) const FIELDS: &[(&str, &str)] = &[
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

/// `PixelFormat` by its PFNC name, also where the node is an integer PFNC
/// code (`FFmpeg` sources); a code without a name reads as hex.
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
    crate::frame::name_bits(pixel_format)
}

/// The camera's exposure time and frame rate, when it has them: what its
/// frame waits, and the time after a change before its frames show it,
/// follow.
pub(crate) fn frame_timing(camera: &mut Camera) -> (Option<Duration>, Option<f64>) {
    let exposure = first_present(camera, &EXPOSURE).and_then(|feature| {
        let unit = camera.feature_unit(feature).unwrap_or("us").to_owned();
        let value = number(camera, &[feature])?;
        let us = in_time_unit(Duration::from_secs(1), &unit).map_or(value, |per| value * 1e6 / per);
        Duration::try_from_secs_f64(us / 1e6).ok()
    });
    let fps = number(camera, &RESULTING_RATE)
        .or_else(|| number(camera, &FRAME_RATE))
        .filter(|fps| fps.is_finite() && *fps > 0.0);
    (exposure, fps)
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
    let limit_on = first_present(camera, &FRAME_RATE_ENABLE).is_none_or(|enable| {
        camera
            .get(enable)
            .is_ok_and(|v| v == json!(true) || v == json!("On"))
    });
    // A stream that does not say its rate has none (0), not a limit.
    let fps_limit = limit_on
        .then(|| number(camera, &FRAME_RATE))
        .flatten()
        .filter(|fps| *fps > 0.0);
    let trigger = camera
        .has("TriggerMode")
        .then(|| trigger_state(camera).ok())
        .flatten();
    let mut controls = serde_json::Map::new();
    for kind in Control::ALL {
        let control = kind.name();
        let feature = match kind {
            Control::Exposure => exposure,
            Control::Gain => gain_feature.or_else(|| first_present(camera, &["GainRaw"])),
            Control::Fps => first_present(camera, &FRAME_RATE),
            Control::Roi => first_present(camera, &["Width"]).filter(|_| camera.has("Height")),
            Control::Format => first_present(camera, &["PixelFormat"]),
            Control::Trigger => first_present(camera, &["TriggerMode"]),
            Control::AutoExposure => first_present(camera, &["ExposureAuto"]),
            Control::AutoWb => first_present(camera, &["BalanceWhiteAuto"]),
        };
        let Some(feature) = feature else { continue };
        let bounds = if matches!(kind, Control::Exposure | Control::Gain | Control::Fps) {
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
                value: control_value(camera, kind).ok(),
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
        width: camera.get("Width").ok().and_then(|value| value.as_u64()),
        height: camera.get("Height").ok().and_then(|value| value.as_u64()),
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
    let (pattern, name) = (pattern.as_bytes(), name.as_bytes());
    if !pattern.iter().any(|byte| matches!(byte, b'*' | b'?')) {
        return pattern.is_empty()
            || name
                .windows(pattern.len())
                .any(|part| part.eq_ignore_ascii_case(pattern));
    }
    // Remember the last star and extend its match on a mismatch. Recursive
    // alternatives take exponential time for patterns such as *a*a*...b.
    let (mut p, mut n) = (0, 0);
    let mut star = None;
    let mut retry = 0;
    while n < name.len() {
        if p < pattern.len()
            && pattern[p] != b'*'
            && (pattern[p] == b'?' || pattern[p].eq_ignore_ascii_case(&name[n]))
        {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            retry = n;
        } else if let Some(last) = star {
            retry += 1;
            n = retry;
            p = last + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

/// One `features` row.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct FeatureRow {
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
pub(crate) struct FeatureList {
    pub camera: String,
    pub features: Vec<FeatureRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<usize>,
}
impl FeatureList {
    pub(crate) const FIELDS: &[(&str, &str)] = &[
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
        "Command" => "command",
        _ => "string",
    }
}

pub(crate) fn features(camera: &mut Camera, filter: &FeatureFilter) -> Result<FeatureList> {
    filter.validate()?;
    let selected = camera
        .feature_names()
        .into_iter()
        // A pattern or category is a search: it looks at every level.
        .filter(|name| {
            filter.all
                || filter.pattern.is_some()
                || filter.category.is_some()
                || camera.visibility(name) == "Beginner"
                || COMMON.contains(name)
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
        .map(str::to_owned)
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
    #[test]
    fn canonical_control_names_and_order_remain_stable() {
        let names = [
            "exposure",
            "gain",
            "fps",
            "roi",
            "format",
            "trigger",
            "auto_exposure",
            "auto_wb",
        ];
        assert_eq!(Control::ALL.map(Control::name), names);
        assert_eq!(CONTROLS.map(|(name, _)| name), names);
        for name in names {
            assert_eq!(Control::parse(name).unwrap().name(), name);
        }
        for feature in ["ExposureTime", "Gain", "ROI", "AutoExposure", "unknown"] {
            assert_eq!(Control::parse(feature), None);
        }
    }

    use super::*;

    #[test]
    fn roi_requires_exactly_four_nonnegative_integer_components() {
        let mut camera = sim();
        assert_eq!(
            roi(&mut camera, " 0 , 0 , 64 , 48 ").unwrap(),
            roi(&mut camera, "0,0,64,48").unwrap()
        );
        for value in [
            "",
            "0,0,64",
            "0,0,64,48,1",
            "0,0,64,48,",
            "0,,64,48",
            "-1,0,64,48",
            "0,0,64.5,48",
            "0,0,NaN,48",
            "0,0,inf,48",
            "0,0,６４,48",
        ] {
            let error = roi(&mut camera, value).unwrap_err();
            assert!(
                error.to_string().starts_with("roi takes x,y,w,h"),
                "{error}"
            );
        }
    }

    #[test]
    fn boolean_settings_keep_invalid_values_distinct_from_false() {
        for (text, expected) in [
            ("true", Some(true)),
            ("TrUe", Some(true)),
            ("1", Some(true)),
            ("ON", Some(true)),
            ("false", Some(false)),
            ("FaLsE", Some(false)),
            ("0", Some(false)),
            ("OFF", Some(false)),
            ("", None),
            (" true ", None),
            ("yes", None),
            ("ｏｎ", None),
            ("é", None),
        ] {
            assert_eq!(boolean_setting(text), expected, "{text:?}");
            for current in [false, true] {
                assert_eq!(
                    same(&json!(current), text),
                    expected == Some(current),
                    "{text:?} compared with {current}"
                );
                assert_eq!(
                    wanted_json(Some(&json!(current)), text),
                    json!(expected == Some(true)),
                    "{text:?} with current {current}"
                );
            }
        }
    }

    #[test]
    fn choices_ignore_only_ascii_case_underscores_and_hyphens() {
        for (left, right, equal) in [
            ("AnalogAll", "analog_all", true),
            ("Frame-Start", "FRAME_START", true),
            ("_a--B_", "ab", true),
            ("", "_-", true),
            ("É_A", "É-a", true),
            ("É", "é", false),
            ("a b", "ab", false),
            ("a", "aa", false),
            ("a/b", "ab", false),
        ] {
            assert_eq!(same_choice(left, right), equal, "{left:?}, {right:?}");
            assert_eq!(same_choice(right, left), equal, "{right:?}, {left:?}");
        }
    }

    fn sim() -> Camera {
        Camera::open("sim:0", &[], Duration::from_secs(2)).unwrap()
    }
    fn assign(text: &str) -> Assignment {
        Assignment::parse(text).unwrap()
    }

    #[test]
    fn bounded_suggestions_match_the_complete_ranking() {
        let camera = sim();
        let names: Vec<_> = camera
            .feature_names()
            .into_iter()
            .chain(CONTROLS.iter().map(|(name, _)| *name))
            .collect();
        let queries = [
            "", "e", "Exposure", "Trigger", "gain", "exposur", "__", "三",
        ]
        .into_iter()
        .map(str::to_owned)
        .chain(names.iter().map(|name| name.chars().take(3).collect()));
        for typed in queries {
            let mut ranked: Vec<_> = names
                .iter()
                .filter_map(|name| {
                    crate::hint::close_distance(&typed, name).map(|distance| (distance, *name))
                })
                .collect();
            ranked.sort_unstable();
            ranked.dedup_by(|a, b| a.1 == b.1);
            let expected: Vec<_> = ranked.into_iter().take(5).map(|(_, name)| name).collect();
            assert_eq!(did_you_mean(&camera, &typed), expected, "{typed:?}");
        }
    }

    #[test]
    fn fuzz_feature_globs() {
        // Small exhaustive reference comparisons, followed by inputs whose
        // recursive search used to grow exponentially.
        fn reference(p: &[u8], n: &[u8]) -> bool {
            match (p.first(), n.first()) {
                (None, None) => true,
                (Some(b'*'), _) => {
                    reference(&p[1..], n) || (!n.is_empty() && reference(p, &n[1..]))
                }
                (Some(b'?'), Some(_)) => reference(&p[1..], &n[1..]),
                (Some(a), Some(b)) if a == b => reference(&p[1..], &n[1..]),
                _ => false,
            }
        }
        for code in 0..4096usize {
            let mut code = code;
            let pattern: String = std::iter::repeat_with(|| {
                let c = b"ab*?"[code % 4];
                code /= 4;
                c as char
            })
            .take(6)
            .collect();
            for bits in 0..32usize {
                let name: String = (0..5)
                    .map(|i| if bits & (1 << i) == 0 { 'a' } else { 'b' })
                    .collect();
                let expected = if pattern.contains(['*', '?']) {
                    reference(pattern.as_bytes(), name.as_bytes())
                } else {
                    name.contains(&pattern)
                };
                assert_eq!(
                    matches_pattern(&pattern, &name),
                    expected,
                    "{pattern} {name}"
                );
                assert_eq!(
                    matches_pattern(&pattern.to_ascii_uppercase(), &name),
                    expected
                );
                assert_eq!(
                    matches_pattern(&pattern, &name.to_ascii_uppercase()),
                    expected
                );
            }
        }
        let pattern = format!("{}b", "*a".repeat(100));
        for (pattern, name) in [("*a", "*ba"), ("*?", "*"), ("?*?", "a?b")] {
            assert_eq!(
                matches_pattern(pattern, name),
                reference(pattern.as_bytes(), name.as_bytes())
            );
        }
        assert!(!matches_pattern(&pattern, &"a".repeat(200)));
        assert!(matches_pattern(
            &format!("{}*", "*a".repeat(100)),
            &"a".repeat(200)
        ));
    }

    #[test]
    fn feature_patterns_preserve_ascii_case_and_utf8_byte_matching() {
        for (pattern, name, expected) in [
            ("", "", true),
            ("", "ExposureTime", true),
            ("exposure", "ExposureTime", true),
            ("EXPO*TIME", "ExposureTime", true),
            ("éx", "éX", true),
            ("Éx", "éX", false),
            ("*é*", "AéB", true),
            ("?", "é", false),
            ("??", "é", true),
            ("longer", "short", false),
        ] {
            assert_eq!(
                matches_pattern(pattern, name),
                expected,
                "{pattern:?} {name:?}"
            );
        }
    }

    #[test]
    fn names_parse_banks_and_controls() {
        for text in [
            "ExposureTime",
            "Gain[AnalogAll]",
            "Setting[BankSelector=A,OtherSelector=B]",
            "Setting[BankSelector=é,二]",
        ] {
            assert_eq!(Name::parse(text).unwrap().to_string(), text);
        }
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
        let assignment = assign("Setting[BankSelector=é,二]= 三=四 ");
        assert_eq!(assignment.value, "三=四");
        assert_eq!(
            assignment.bank,
            Name::parse("Setting[BankSelector=é,二]").unwrap().bank
        );
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
    fn number_formatting_preserves_finite_values_and_normalizes_zero() {
        assert!(time_text(18_446_744_073_709_551_616.0, "ns").is_none());
        assert!(time_text(18_446_744_073_709_549_568.0, "ns").is_some());
        for (value, expected) in [
            (0.0, "0"),
            (-0.0, "0"),
            (-0.000_000_000_1, "0"),
            (1.0, "1"),
            (1.25, "1.25"),
            (1.234_567_890_4, "1.23456789"),
        ] {
            assert_eq!(number_text(value), expected);
        }
        for value in [f64::MAX, -f64::MAX, 1e300, -1e300] {
            assert_eq!(number_text(value).parse::<f64>().unwrap(), value);
        }
    }

    #[test]
    fn camera_readbacks_keep_exact_pixels_and_reject_invalid_values() {
        struct Geometry(String);
        impl crate::types::RegisterIo for Geometry {
            fn read_memory(&mut self, _: u64, _: usize) -> Result<Vec<u8>> {
                anyhow::bail!("literal geometry needs no register reads")
            }
            fn write_memory(&mut self, _: u64, _: &[u8]) -> Result<()> {
                anyhow::bail!("read-only geometry")
            }
        }
        impl crate::types::Backend for Geometry {
            fn xml(&mut self) -> Result<String> {
                Ok(self.0.clone())
            }
            fn start(&mut self, _: usize) -> Result<()> {
                anyhow::bail!("no acquisition")
            }
            fn next_frame(&mut self, _: Duration) -> Result<crate::types::Frame> {
                anyhow::bail!("no frames")
            }
            fn stop(&mut self) -> Result<()> {
                Ok(())
            }
        }
        for (kind, width, expected) in [
            (
                "Integer",
                "9007199254740993",
                Some(9_007_199_254_740_993u64),
            ),
            ("Integer", "-1", None),
            ("Float", "640.5", None),
        ] {
            let xml = format!(
                "<RegisterDescription><{kind} Name='Width'><Value>{width}</Value></{kind}>\
                 <Integer Name='Height'><Value>480</Value></Integer></RegisterDescription>"
            );
            let mut camera =
                Camera::from_backend(crate::transport::simulator::info(), Box::new(Geometry(xml)))
                    .unwrap();
            let report = info(&mut camera);
            assert_eq!(report.width, expected, "{width}");
            assert_eq!(report.height, Some(480));
            match expected {
                Some(width) => assert_eq!(
                    control_value(&mut camera, Control::Roi).unwrap(),
                    format!("0,0,{width},480")
                ),
                None => assert!(
                    control_value(&mut camera, Control::Roi)
                        .unwrap_err()
                        .to_string()
                        .contains("nonnegative integer pixel count")
                ),
            }
        }
        for (amount, unit, expected) in [
            ("10000", "us", Some("10ms")),
            ("10", "vendor-unit", Some("10us")),
            ("-1", "us", None),
            ("1e300", "us", None),
            ("1e12", "s", None),
        ] {
            let xml = format!(
                "<RegisterDescription><Float Name='ExposureTime'><Value>{amount}</Value>\
                 <Unit>{unit}</Unit></Float></RegisterDescription>"
            );
            let mut camera =
                Camera::from_backend(crate::transport::simulator::info(), Box::new(Geometry(xml)))
                    .unwrap();
            let result = control_value(&mut camera, Control::Exposure);
            match expected {
                Some(expected) => assert_eq!(result.unwrap(), expected),
                None => assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("representable nonnegative duration")
                ),
            }
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
    fn an_exposure_out_of_range_says_the_nearest_as_a_duration_set_takes() {
        let mut camera = sim();
        for (asked, nearest) in [("exposure=5s", "1s"), ("exposure=1us", "10us")] {
            let error = set(&mut camera, &[assign(asked)], false).unwrap_err();
            let classified = crate::types::classify(&error);
            assert_eq!(classified.kind, ErrorKind::OutOfRange, "{error:#}");
            let details = classified.details.unwrap();
            assert_eq!(details["unit"], "us");
            assert_eq!(
                classified.hint.unwrap(),
                format!("the nearest accepted value is {nearest}; ExposureTime takes 10us to 1s")
            );
            // What the hint says is what set takes, for the control and
            // for the feature, and it lands on the bound in microseconds.
            for name in ["exposure", "ExposureTime"] {
                set(&mut camera, &[assign(&format!("{name}={nearest}"))], false).unwrap();
                assert_eq!(
                    camera.get("ExposureTime").unwrap(),
                    details["nearest"],
                    "{name}={nearest}"
                );
            }
        }
        // A number in another unit keeps it beside the number.
        let error = set(&mut camera, &[assign("Gain=99")], false).unwrap_err();
        assert_eq!(
            crate::types::classify(&error).hint.unwrap(),
            "the nearest accepted value is 24 (dB)"
        );
    }

    #[test]
    fn automatic_modes_preserve_case_insensitive_choices() {
        let mut camera = sim();
        for (input, expected) in [
            (" oFf ", "Off"),
            ("ONcE", "Once"),
            ("continuous", "Continuous"),
        ] {
            let writes = expand_control(&mut camera, Control::AutoExposure, input).unwrap();
            assert_eq!(writes.len(), 1);
            assert_eq!(writes[0].value, expected);
        }
        for input in ["", "once more", "ＯＦＦ"] {
            let error = expand_control(&mut camera, Control::AutoExposure, input).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("auto_exposure takes off, once or continuous, not '{input}'")
            );
        }
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
    const GEOMETRY_XML: &str = r#"<Integer Name="Width"><pValue>WidthReg</pValue><Min>16</Min><Max>1920</Max></Integer><IntReg Name="WidthReg"><Address>0x64</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></IntReg><Integer Name="Height"><pValue>HeightReg</pValue><Min>16</Min><pMax>HeightLimit</pMax></Integer><IntReg Name="HeightReg"><Address>0x68</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></IntReg><IntReg Name="HeightLimit"><Address>0x6c</Address><Length>4</Length><AccessMode>RO</AccessMode><Endianess>LittleEndian</Endianess></IntReg>"#;

    #[test]
    fn a_value_only_the_writes_can_check_is_a_warning_then_rolled_back() {
        use crate::camera::tests::{Device, device_camera};
        let device = Device::new(GEOMETRY_XML)
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

    /// Bank A of Setting holds 10 and bank B 11, with A selected. Strobe
    /// looks writable, but the device refuses every write of it.
    const BANKS: &str = concat!(
        "<Enumeration Name='BankSelector'><EnumEntry Name='A'><Value>0</Value></EnumEntry><EnumEntry Name='B'><Value>1</Value></EnumEntry><Value>0</Value><pSelected>Setting</pSelected></Enumeration>",
        "<Integer Name='Setting'><pIndex>BankSelector</pIndex><pValueIndexed Index='0'>AValue</pValueIndexed><pValueIndexed Index='1'>BValue</pValueIndexed></Integer>",
        "<Integer Name='AValue'><Value>10</Value></Integer><Integer Name='BValue'><Value>11</Value></Integer>",
        "<Integer Name='Strobe'><pValue>StrobeReg</pValue></Integer><IntReg Name='StrobeReg'><Address>0x00</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></IntReg>",
    );

    #[test]
    fn a_failed_change_restores_every_bank_it_wrote() {
        use crate::camera::tests::{Device, device_camera};
        let mut camera = device_camera(Device::new(BANKS));
        let banks = |camera: &mut Camera| {
            let names =
                ["BankSelector", "Setting[A]", "Setting[B]"].map(|name| Name::parse(name).unwrap());
            Value::Object(get(camera, &names).unwrap())
        };
        let initial = banks(&mut camera);
        assert_eq!(
            initial,
            json!({"BankSelector":"A","Setting[A]":10,"Setting[B]":11})
        );
        let restored = |camera: &mut Camera, values: &[&str]| {
            let values = values.iter().map(|value| assign(value)).collect::<Vec<_>>();
            let error = set(camera, &values, false).unwrap_err();
            assert!(
                format!("{error:#}").contains("every value written was restored"),
                "{error:#}"
            );
            crate::types::classify(&error).details.unwrap()["rollback"].clone()
        };
        // A value written at the bank an earlier assignment selects is put
        // back at that bank.
        assert_eq!(
            restored(&mut camera, &["BankSelector=B", "Setting=99", "Strobe=1"]),
            json!({"restored":["Setting[B]"],"failed":[]})
        );
        assert_eq!(banks(&mut camera), initial);
        // So is one written directly to the feature a bank reads.
        assert_eq!(
            restored(&mut camera, &["BValue=42", "Setting=20", "Strobe=1"]),
            json!({"restored":["Setting[A]","BValue"],"failed":[]})
        );
        assert_eq!(banks(&mut camera), initial);
        // Success writes every bank and keeps the selection.
        set(
            &mut camera,
            &[assign("Setting[A]=20"), assign("Setting[B]=99")],
            false,
        )
        .unwrap();
        assert_eq!(
            banks(&mut camera),
            json!({"BankSelector":"A","Setting[A]":20,"Setting[B]":99})
        );
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

    /// A binning written and rolled back scales the width and offset
    /// twice, rounding them: the rollback puts the geometry back as a
    /// group, within the simulator's dynamic bounds (`OffsetX` at most
    /// `WidthMax` - Width).
    #[test]
    fn a_failed_change_restores_the_geometry_a_write_rescaled() {
        let mut camera = sim();
        set(
            &mut camera,
            &[assign("Width=639"), assign("OffsetX=1281")],
            false,
        )
        .unwrap();
        let values = [assign("BinningHorizontal=4"), assign("Width=1000")];
        // The binning moves Width's bounds, so only the write can check it.
        let plan = set(&mut camera, &values, true).unwrap();
        assert_eq!(plan.warnings.len(), 1, "{:?}", plan.warnings);
        let error = set(&mut camera, &values, false).unwrap_err();
        let classified = crate::types::classify(&error);
        assert_eq!(classified.kind, ErrorKind::OutOfRange, "{error:#}");
        assert!(
            format!("{error:#}").contains("every value written was restored"),
            "{error:#}"
        );
        // Restoring BinningHorizontal alone leaves Width 636 and OffsetX
        // 1280, each scaled down by 4 and back up.
        assert_eq!(
            classified.details.unwrap()["rollback"],
            json!({"restored":["BinningHorizontal","Width","OffsetX"],"failed":[]})
        );
        for (feature, value) in [
            ("BinningHorizontal", 1),
            ("Width", 639),
            ("OffsetX", 1281),
            ("WidthMax", 1920),
        ] {
            assert_eq!(camera.get(feature).unwrap(), value, "{feature}");
        }
        // A change that writes no geometry checkpoints none.
        let error = set(
            &mut camera,
            &[assign("ExposureTime=4000"), assign("Gain=99")],
            false,
        )
        .unwrap_err();
        assert!(crate::types::classify(&error).details.unwrap()["rollback"].is_null());
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
            &[],
            &Categorized::error(ErrorKind::OutOfRange, "Width is too large"),
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
    fn a_trigger_on_the_cameras_own_timing_holds_no_frames_back() {
        let mut camera = Camera::open("sim:camera-timed", &[], Duration::from_secs(2)).unwrap();
        camera.set("TriggerMode", "On").unwrap();
        for source in ["Freerun", "FixedRate"] {
            camera.set("TriggerSource", source).unwrap();
            let trigger = trigger_state(&mut camera).unwrap();
            assert_eq!(
                (trigger.mode.as_str(), trigger.source.as_deref()),
                ("On", Some(source))
            );
            assert!(!trigger.holds_frames(), "{source}");
            assert!(!crate::auto::frame_triggered(&mut camera), "{source}");
        }
        // An acquisition trigger on a line still holds frames back, and is
        // the trigger in effect.
        camera.set("TriggerSelector", "AcquisitionStart").unwrap();
        camera.set("TriggerMode", "On").unwrap();
        camera.set("TriggerSource", "Line0").unwrap();
        camera.set("TriggerSelector", "FrameStart").unwrap();
        let trigger = trigger_state(&mut camera).unwrap();
        assert_eq!(trigger.selector.as_deref(), Some("AcquisitionStart"));
        assert!(trigger.holds_frames());
        // Once it runs on its own timing too, the frame trigger is shown.
        camera.set("TriggerSelector", "AcquisitionStart").unwrap();
        camera.set("TriggerSource", "Freerun").unwrap();
        camera.set("TriggerSelector", "FrameStart").unwrap();
        let trigger = trigger_state(&mut camera).unwrap();
        assert_eq!(
            (trigger.selector.as_deref(), trigger.source.as_deref()),
            (Some("FrameStart"), Some("FixedRate"))
        );
        camera.set("TriggerSource", "Line1").unwrap();
        assert!(trigger_state(&mut camera).unwrap().holds_frames());
        for selector in ["AcquisitionStart", "FrameStart"] {
            camera.set("TriggerSelector", selector).unwrap();
            camera.set("TriggerMode", "Off").unwrap();
        }
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
