//! Saved setups in the camera worker: reading one, comparing it with the
//! camera, and restoring it inside one history checkpoint. The coordinator
//! loads named setups; the worker checks the camera's identity and node
//! kinds itself, so remote, workflow and RPC callers get the same checks.
use super::*;
use crate::setup::{
    Bank, MAX_BANKS, MAX_SETUP_VALUES, MAX_STRING, Match, Reading, SETUP_ALL_BANKS, SavedAuto,
    SavedCamera, Scalar, Setup, SetupValue, Skipped,
};
use crate::tools::risk;
use crate::types::{Categorized, ErrorKind};

/// How long a restore may spend writing before it gives up and rolls back.
pub(super) const DEADLINE: Duration = Duration::from_secs(90);

/// Firmware autos and the values they drive.
const FIRMWARE_AUTOS: [(&str, &[&str]); 3] = [
    ("ExposureAuto", &["ExposureTime", "ExposureTimeAbs"]),
    ("GainAuto", &["Gain", "GainAbs", "GainRaw"]),
    ("BalanceWhiteAuto", &["BalanceRatio"]),
];
/// What Capturefab's software auto exposure drives, when it is not running
/// yet to say.
const SOFTWARE_AUTO: [&str; 4] = ["ExposureTime", "ExposureTimeAbs", "Gain", "GainAbs"];

fn banks(address: &Address) -> Vec<Bank> {
    address
        .selectors
        .iter()
        .map(|(selector, value)| Bank {
            selector: selector.clone(),
            value: value.clone(),
        })
        .collect()
}
fn bank_text(at: &[Bank]) -> String {
    at.iter()
        .map(|bank| format!("{} = {}", bank.selector, bank.value))
        .collect::<Vec<_>>()
        .join(", ")
}
fn address_of(value: &SetupValue) -> Address {
    Address {
        feature: value.feature.clone(),
        selectors: value
            .at
            .iter()
            .map(|bank| (bank.selector.clone(), bank.value.clone()))
            .collect(),
    }
}
/// Longest skip reason a setup stores, in bytes; below the document's limit.
const MAX_REASON: usize = 240;

/// A skip reason fit to store in a setup: one line without control
/// characters, bounded on a char boundary.
fn reason_text(reason: &str) -> String {
    let line = reason
        .split(|c: char| c.is_control() || c.is_whitespace())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if line.len() <= MAX_REASON {
        return line;
    }
    let mut end = MAX_REASON - '…'.len_utf8();
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", line[..end].trim_end())
}

/// Why a value could not be read, without the camera's error text: that
/// can name the camera's address, which a setup never holds. The full error
/// goes to the session log.
fn unreadable(h: &WorkerHandle, feature: &str, at: &[Bank], error: &anyhow::Error) -> String {
    let place = if at.is_empty() {
        String::new()
    } else {
        format!(" at {}", bank_text(at))
    };
    h.log(
        "warn",
        format!("Setup: could not read {feature}{place}: {error:#}"),
    );
    let timed_out = crate::types::error_transport_code(error) == Some("timeout")
        || format!("{error:#}")
            .to_ascii_lowercase()
            .contains("timed out");
    if timed_out {
        "could not read: timed out".into()
    } else {
        "could not read".into()
    }
}

fn skip(feature: &str, at: Vec<Bank>, reason: impl Into<String>) -> Skipped {
    Skipped {
        feature: feature.into(),
        at,
        reason: reason_text(&reason.into()),
    }
}

/// Read every configuration value a setup can restore. One unreadable bank
/// or feature never fails the whole read: it is recorded in `skipped`.
pub(super) fn read(
    h: &WorkerHandle,
    camera: &mut Camera,
    auto: &Option<AutoController>,
) -> Result<Value> {
    let mut names = h
        .state
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .features
        .iter()
        .filter(|feature| feature.value.is_some())
        .map(|feature| feature.name.clone())
        .collect::<BTreeSet<_>>();
    if names.is_empty() {
        names = camera
            .features()
            .into_iter()
            .filter(|feature| feature.value.is_some())
            .map(|feature| feature.name)
            .collect();
    }
    let managed = auto
        .as_ref()
        .map(|controller| {
            controller
                .status()
                .managed
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let mut firmware = BTreeMap::new();
    for (flag, driven) in FIRMWARE_AUTOS {
        if camera.has(flag)
            && let Ok(value) = camera.get(flag)
            && text(&value) != "Off"
        {
            for name in driven {
                firmware.insert(*name, format!("set by {flag} = {}", text(&value)));
            }
        }
    }
    let mut values = Vec::new();
    let mut skipped = Vec::new();
    for name in names {
        if crate::session::validate_feature(&name).is_err() {
            continue;
        }
        let kind = camera.node_kind(&name).unwrap_or_default().to_owned();
        if risk::managed(&name, None, Some(&kind)) {
            continue;
        }
        let banked = camera
            .selectors(&name)
            .iter()
            .any(|selector| SETUP_ALL_BANKS.contains(&selector.as_str()));
        if !banked && !camera.is_configuration_value(&name) {
            continue;
        }
        if managed.contains(&name) {
            skipped.push(skip(
                &name,
                Vec::new(),
                "managed by Capturefab auto exposure",
            ));
            continue;
        }
        if let Some(reason) = firmware.get(name.as_str()) {
            skipped.push(skip(&name, Vec::new(), reason.clone()));
            continue;
        }
        if values.len() >= MAX_SETUP_VALUES {
            skipped.push(skip(&name, Vec::new(), "limit of 512 values reached"));
            continue;
        }
        let addresses = match addresses(camera, &name, &|selector| {
            SETUP_ALL_BANKS.contains(&selector)
        }) {
            Ok(addresses) => addresses,
            Err(error) if error.is::<TooManyBanks>() => {
                skipped.push(skip(
                    &name,
                    Vec::new(),
                    "more than 256 banks; saved at the selected bank only",
                ));
                match addresses(camera, &name, &|_| false) {
                    Ok(addresses) => addresses,
                    Err(error) => {
                        let reason = unreadable(h, &name, &[], &error);
                        skipped.push(skip(&name, Vec::new(), reason));
                        continue;
                    }
                }
            }
            Err(error) => {
                let reason = unreadable(h, &name, &[], &error);
                skipped.push(skip(&name, Vec::new(), reason));
                continue;
            }
        };
        // Banks where the value is read-only (an input line's source) are
        // reported only when another bank of it was saved.
        let mut read_only = Vec::new();
        let mut saved = false;
        for address in addresses {
            let at = banks(&address);
            if at.len() > MAX_BANKS
                || at.iter().any(|bank| {
                    risk::managed_name(&bank.selector) || !bank.selector.ends_with("Selector")
                })
            {
                skipped.push(skip(
                    &name,
                    Vec::new(),
                    "addressed by selectors a setup cannot use",
                ));
                break;
            }
            if values.len() >= MAX_SETUP_VALUES {
                skipped.push(skip(&name, at, "limit of 512 values reached"));
                break;
            }
            let read = addressed(camera, &address, |camera| {
                Ok(if camera.is_configuration_value(&name) {
                    Some(camera.get(&name)?)
                } else {
                    None
                })
            });
            // The bank is stored beside the reason, so the reason omits it.
            match read {
                Ok(Some(value)) => match Scalar::from_value(&value) {
                    Some(Scalar::String(text)) if text.len() > MAX_STRING => {
                        skipped.push(skip(&name, at, "longer than 1024 bytes"));
                    }
                    Some(value) => {
                        saved = true;
                        values.push(SetupValue {
                            feature: name.clone(),
                            value,
                            at,
                        });
                    }
                    None => skipped.push(skip(&name, at, "not a value a setup can hold")),
                },
                Ok(None) => read_only.push(skip(&name, at, "read-only")),
                Err(error) => {
                    let reason = unreadable(h, &name, &at, &error);
                    skipped.push(skip(&name, at, reason));
                }
            }
        }
        if saved {
            skipped.extend(read_only.into_iter().filter(|s| !s.at.is_empty()));
        }
    }
    let reading = Reading {
        camera: SavedCamera::from_info(&camera.info),
        auto: auto.as_ref().map(|controller| SavedAuto {
            balance: controller.status().balance,
        }),
        values,
        skipped,
    };
    Ok(serde_json::to_value(reading)?)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Status {
    Same,
    Change,
    Missing,
    ReadOnly,
    Invalid,
    Skipped,
    Refused,
}
impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Same => "same",
            Self::Change => "change",
            Self::Missing => "missing",
            Self::ReadOnly => "read_only",
            Self::Invalid => "invalid",
            Self::Skipped => "skipped",
            Self::Refused => "refused",
        }
    }
}

pub(super) struct Row {
    pub(super) address: Address,
    at: Vec<Bank>,
    /// The saved value as the camera reports this feature's type.
    pub(super) target: Value,
    saved: Scalar,
    current: Option<Value>,
    pub(super) status: Status,
    reason: Option<String>,
    warning: Option<String>,
}

/// A setup compared with the connected camera, which a restore follows.
pub(super) struct Plan {
    pub(super) name: String,
    pub(super) rows: Vec<Row>,
    matched: Match,
    warnings: Vec<String>,
    pub(super) restarts_stream: bool,
    blocked: Option<String>,
    digest: String,
}
impl Plan {
    pub(super) fn changes(&self) -> impl Iterator<Item = &Row> {
        self.rows.iter().filter(|row| row.status == Status::Change)
    }
    /// Refuse before any write: a blocked restart, or a strict setup with
    /// values this camera cannot take.
    pub(super) fn check(&self, strict: bool) -> Result<()> {
        if let Some(blocked) = &self.blocked {
            return Err(Categorized::error(ErrorKind::Unavailable, blocked.clone()));
        }
        let failing = self
            .rows
            .iter()
            .filter(|row| {
                matches!(
                    row.status,
                    Status::Missing | Status::ReadOnly | Status::Invalid | Status::Refused
                )
            })
            .map(|row| {
                format!(
                    "{} ({})",
                    row.address.feature,
                    row.reason.as_deref().unwrap_or(row.status.as_str())
                )
            })
            .collect::<Vec<_>>();
        if strict && !failing.is_empty() {
            return Err(Categorized::error(
                ErrorKind::Unsupported,
                format!(
                    "Setup {} cannot be applied strictly; nothing changed. Not applicable: {}",
                    self.name,
                    failing.join(", ")
                ),
            ));
        }
        Ok(())
    }
    /// Refuse, before any write, disruptive changes nobody confirmed: the
    /// camera may have changed since the reviewed diff. `None` checks
    /// nothing (RPC and workflow callers).
    pub(super) fn confirm(&self, confirmed: Option<&[String]>) -> Result<()> {
        let Some(confirmed) = confirmed else {
            return Ok(());
        };
        let mut unconfirmed = Vec::new();
        for row in self.changes() {
            let feature = row.address.feature.as_str();
            if risk_of(feature) == "disruptive"
                && !confirmed.iter().any(|name| name == feature)
                && !unconfirmed.contains(&feature)
            {
                unconfirmed.push(feature);
            }
        }
        if unconfirmed.is_empty() {
            return Ok(());
        }
        Err(crate::dataset::Usage(format!(
            "Restoring {} now also changes {}, which was not confirmed; the camera changed after the preview. Nothing changed; preview it again.",
            self.name,
            unconfirmed.join(", ")
        ))
        .into())
    }
}

fn number_kind(value: &Value) -> &'static str {
    match value {
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "text",
        _ => "value",
    }
}

/// Whether the camera's value already matches. Floats compare within half
/// their increment, else a relative 1e-9, so a camera that rounds a stepped
/// value does not show a change forever.
pub(super) fn close(current: &Value, target: &Value, inc: Option<f64>) -> bool {
    match (current, target) {
        (Value::Number(a), Value::Number(b)) if a.is_f64() || b.is_f64() => {
            let (Some(a), Some(b)) = (a.as_f64(), b.as_f64()) else {
                return false;
            };
            let tolerance = inc
                .filter(|inc| inc.is_finite() && *inc > 0.0)
                .map_or(0.0, |inc| inc / 2.0)
                + 1e-9 * a.abs().max(b.abs());
            (a - b).abs() <= tolerance
        }
        (Value::Number(a), Value::Number(b)) => a == b || a.as_f64() == b.as_f64(),
        _ => current == target,
    }
}

fn row(
    camera: &mut Camera,
    auto: &Option<AutoController>,
    setup: &Setup,
    value: &SetupValue,
) -> Row {
    let address = address_of(value);
    let feature = value.feature.as_str();
    let mut row = Row {
        address: address.clone(),
        at: value.at.clone(),
        target: value.value.to_value(),
        saved: value.value.clone(),
        current: None,
        status: Status::Missing,
        reason: Some("not on this camera".into()),
        warning: None,
    };
    let set = |row: &mut Row, status, reason: String| {
        row.status = status;
        row.reason = Some(reason);
    };
    if !camera.has(feature) {
        return row;
    }
    let kind = camera.node_kind(feature).unwrap_or_default().to_owned();
    if risk::managed(feature, None, Some(&kind)) {
        set(&mut row, Status::Refused, "not allowed in a setup".into());
        return row;
    }
    let selectors = camera.selectors(feature);
    for bank in &value.at {
        if risk::managed_name(&bank.selector) || !bank.selector.ends_with("Selector") {
            set(&mut row, Status::Refused, "not allowed in a setup".into());
            return row;
        }
        if !camera.has(&bank.selector) {
            set(
                &mut row,
                Status::Missing,
                format!("{} is not on this camera", bank.selector),
            );
            return row;
        }
        if !selectors.contains(&bank.selector) {
            set(
                &mut row,
                Status::Refused,
                format!("{} does not select {feature} on this camera", bank.selector),
            );
            return row;
        }
        if !camera
            .choices(&bank.selector)
            .is_ok_and(|choices| choices.contains(&bank.value))
        {
            set(
                &mut row,
                Status::Missing,
                format!("{} {} is not on this camera", bank.selector, bank.value),
            );
            return row;
        }
    }
    if value.at.is_empty() {
        for (flag, driven) in FIRMWARE_AUTOS {
            if driven.contains(&feature)
                && let Some(Scalar::String(mode)) = setup
                    .values
                    .iter()
                    .find(|v| v.feature == flag && v.at.is_empty())
                    .map(|v| &v.value)
                && mode != "Off"
            {
                set(
                    &mut row,
                    Status::Skipped,
                    format!("set by {flag} = {mode} in this setup"),
                );
                return row;
            }
        }
    }
    if setup.auto.is_some()
        && (auto
            .as_ref()
            .is_some_and(|controller| controller.manages(feature))
            || SOFTWARE_AUTO.contains(&feature))
    {
        set(
            &mut row,
            Status::Skipped,
            "managed by Capturefab auto exposure".into(),
        );
        return row;
    }
    let enumeration = kind == "Enumeration";
    let read = addressed(camera, &address, |camera| {
        Ok((
            camera.is_configuration_value(feature),
            camera.get(feature)?,
            camera.bounds(feature).ok(),
            if enumeration {
                camera.choices(feature).ok()
            } else {
                None
            },
        ))
    });
    let (writable, current, bounds, choices) = match read {
        Ok(read) => read,
        Err(error) => {
            set(
                &mut row,
                Status::Missing,
                format!("could not read: {error:#}"),
            );
            return row;
        }
    };
    row.current = Some(current.clone());
    if !writable {
        set(&mut row, Status::ReadOnly, "read-only now".into());
        return row;
    }
    let target = value.value.to_value();
    if number_kind(&current) != number_kind(&target) {
        set(
            &mut row,
            Status::Invalid,
            format!(
                "{feature} holds a {} on this camera, but the setup has a {}",
                number_kind(&current),
                number_kind(&target)
            ),
        );
        return row;
    }
    if let (Some(choices), Value::String(choice)) = (&choices, &target)
        && !choices.contains(choice)
    {
        set(
            &mut row,
            Status::Invalid,
            format!("{feature} must be one of {}", choices.join(", ")),
        );
        return row;
    }
    row.target = target.clone();
    let inc = bounds.as_ref().and_then(|bounds| bounds.inc);
    if close(&current, &target, inc) {
        row.status = Status::Same;
        row.reason = None;
        return row;
    }
    row.status = Status::Change;
    row.reason = None;
    if let (Some(bounds), Some(number)) = (&bounds, target.as_f64()) {
        let below = bounds.min.is_some_and(|min| number < min);
        let above = bounds.max.is_some_and(|max| number > max);
        if below || above {
            let limit = |limit: Option<f64>| limit.map_or("?".into(), |l| l.to_string());
            row.warning = Some(format!(
                "outside the current limits ({}..{}); applies only if earlier changes widen them",
                limit(bounds.min),
                limit(bounds.max)
            ));
        }
    }
    row
}

/// Compare `setup` with the camera. A setup from another model is refused
/// unless `allow_other_model`.
/// What holds a running stream, in words: "recording", "forwarding",
/// "scheduled captures". A geometry restore is refused while any does.
pub(super) fn stream_holders(
    forwarder: &Option<(Forwarder, f64)>,
    jobs: &[Scheduled],
) -> Vec<&'static str> {
    let mut holders = Vec::new();
    if let Some((forwarder, _)) = forwarder {
        holders.push(if forwarder.recording_path().is_some() {
            "recording"
        } else {
            "forwarding"
        });
    }
    if jobs.iter().any(|job| job.info.active()) {
        holders.push("scheduled captures");
    }
    holders
}

pub(super) fn plan(
    camera: &mut Camera,
    auto: &Option<AutoController>,
    holders: &[&str],
    setup: &Setup,
    allow_other_model: bool,
) -> Result<Plan> {
    let (matched, warnings) = crate::setup::matching(&setup.camera, &camera.info);
    if matched == Match::OtherModel && !allow_other_model {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            crate::setup::other_model_message(setup, &camera.info),
        ));
    }
    let rows = setup
        .values
        .iter()
        .map(|value| row(camera, auto, setup, value))
        .collect::<Vec<_>>();
    let mut locked = Vec::new();
    for row in &rows {
        if row.status == Status::Change
            && crate::genicam::stream_locked(&row.address.feature)
            && !locked.contains(&row.address.feature)
        {
            locked.push(row.address.feature.clone());
        }
    }
    let restarts_stream = camera.is_streaming() && !locked.is_empty();
    Ok(Plan {
        name: setup.name.clone(),
        rows,
        matched,
        warnings,
        restarts_stream,
        blocked: (restarts_stream && !holders.is_empty()).then(|| {
            format!(
                "Stop {} first: this setup changes {}.",
                holders.join(" and "),
                locked.join(", ")
            )
        }),
        digest: crate::setup::digest(setup),
    })
}

fn risk_of(feature: &str) -> &'static str {
    if risk::image_quality(feature) {
        "reversible"
    } else {
        "disruptive"
    }
}

/// The diff a person or script reviews before a restore.
pub(super) fn report(plan: &Plan, info: &crate::types::CameraInfo) -> Value {
    let mut changes = Vec::new();
    let mut not_applied = Vec::new();
    let mut matched = Vec::new();
    let mut unchanged = 0usize;
    let mut disruptive = false;
    for row in &plan.rows {
        let mut entry = json!({"feature":row.address.feature});
        if !row.at.is_empty() {
            entry["at"] = json!(row.at);
        }
        match row.status {
            Status::Same => {
                unchanged += 1;
                entry["value"] = row.target.clone();
                matched.push(entry);
            }
            Status::Change => {
                let risk = risk_of(&row.address.feature);
                disruptive |= risk == "disruptive";
                entry["saved"] = row.target.clone();
                entry["current"] = row.current.clone().unwrap_or(Value::Null);
                entry["risk"] = json!(risk);
                entry["stream_locked"] = json!(crate::genicam::stream_locked(&row.address.feature));
                if let Some(warning) = &row.warning {
                    entry["warning"] = json!(warning);
                }
                changes.push(entry);
            }
            status => {
                entry["status"] = json!(status.as_str());
                entry["saved"] = row.saved.to_value();
                entry["reason"] = json!(row.reason.as_deref().unwrap_or(status.as_str()));
                not_applied.push(entry);
            }
        }
    }
    json!({
        "setup": plan.name,
        "digest": plan.digest,
        "camera": {"id":info.id,"vendor":info.vendor,"model":info.model,"serial":info.serial},
        "match": plan.matched.as_str(),
        "warnings": plan.warnings,
        "changes": changes,
        "unchanged": unchanged,
        "matched": matched,
        "not_applied": not_applied,
        "risk": if disruptive {"disruptive"} else {"reversible"},
        "restarts_stream": plan.restarts_stream,
        "blocked": plan.blocked,
    })
}

/// Add the value at `address` to the checkpoint taken before a restore, so
/// a bank the camera did not have selected is covered too.
pub(super) fn extend_at(
    camera: &mut Camera,
    before: &mut Configuration,
    address: &Address,
) -> Result<()> {
    if let std::collections::btree_map::Entry::Vacant(entry) = before.values.entry(address.clone())
    {
        entry.insert(
            addressed(camera, address, |camera| camera.get(&address.feature))
                .with_context(|| format!("read {} for undo", address.feature))?,
        );
    }
    Ok(())
}

fn inc(camera: &mut Camera, feature: &str) -> Option<f64> {
    camera.bounds(feature).ok().and_then(|bounds| bounds.inc)
}

/// Write `targets` in apply order, retrying values a later write unlocked
/// or clamped, then verify every one. Gives up after `limit`.
pub(super) fn restore_ordered(
    h: &WorkerHandle,
    camera: &mut Camera,
    name: &str,
    targets: &[(Address, Value)],
    limit: Duration,
) -> Result<()> {
    let deadline = Instant::now() + limit;
    let total = targets.len();
    let mut pending = targets.iter().collect::<Vec<_>>();
    let mut errors = Vec::new();
    for pass in 1..=total.clamp(2, 4) {
        if pending.is_empty() {
            break;
        }
        pending.sort_by(|a, b| {
            crate::setup::order_key(&a.0.feature)
                .cmp(&crate::setup::order_key(&b.0.feature))
                .then_with(|| a.0.selectors.cmp(&b.0.selectors))
        });
        if pass > 1 {
            h.log(
                "info",
                format!(
                    "Restoring setup {name}: pass {pass}, {} values left",
                    pending.len()
                ),
            );
        }
        let settled = total - pending.len();
        let mut failures = BTreeMap::new();
        for (written, (address, target)) in pending.iter().map(|t| (&t.0, &t.1)).enumerate() {
            ensure!(
                Instant::now() < deadline,
                "setup restore exceeded {} s after {} of {total} values",
                limit.as_secs(),
                settled + written
            );
            let result = addressed(camera, address, |camera| {
                let current = camera.get(&address.feature)?;
                if !close(&current, target, inc(camera, &address.feature)) {
                    camera.set(&address.feature, &text(target))?;
                }
                Ok(())
            });
            if let Err(error) = result {
                failures.insert(address.clone(), format!("{error:#}"));
            }
        }
        errors.clear();
        pending = targets
            .iter()
            .filter(|(address, target)| {
                match addressed(camera, address, |camera| {
                    Ok((camera.get(&address.feature)?, inc(camera, &address.feature)))
                }) {
                    Ok((current, inc)) if close(&current, target, inc) => false,
                    Ok(_) => {
                        errors.push(format!(
                            "{}: {}",
                            address.feature,
                            failures
                                .get(address)
                                .map(String::as_str)
                                .unwrap_or("did not keep the saved value")
                        ));
                        true
                    }
                    Err(error) => {
                        errors.push(format!("{}: {error:#}", address.feature));
                        true
                    }
                }
            })
            .collect();
    }
    ensure!(
        pending.is_empty(),
        "could not restore setup values: {}",
        errors.join("; ")
    );
    Ok(())
}

/// Restore a setup: the dispatch of `apply_setup` inside `process`, which
/// checkpoints before and rolls back after a failure. `checkpoint` records
/// each bank before it is written.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply(
    h: &WorkerHandle,
    camera: &mut Option<Camera>,
    forwarder: &Option<(Forwarder, f64)>,
    auto: &mut Option<AutoController>,
    jobs: &[Scheduled],
    setup: &Setup,
    plan: &Plan,
    mut checkpoint: impl FnMut(&mut Camera, &Address) -> Result<()>,
) -> Result<Value> {
    let targets = plan
        .changes()
        .map(|row| (row.address.clone(), row.target.clone()))
        .collect::<Vec<_>>();
    if setup.auto.is_none() && auto.is_some() {
        release_auto(h, auto, camera, false)?;
        h.log(
            "info",
            format!("Switched to manual mode to restore setup {}", setup.name),
        );
    } else if let Some(controller) = auto.as_mut() {
        controller.invalidate();
    }
    let c = camera_mut(camera)?;
    for (address, _) in &targets {
        checkpoint(c, address)?;
    }
    restore_ordered(h, c, &setup.name, &targets, DEADLINE)?;
    let mut result = report(plan, &c.info);
    match &setup.auto {
        Some(saved) => {
            let entered = enter_auto(h, camera, auto, forwarder, jobs, Some(saved.balance))?;
            result["auto"] = entered["auto"].clone();
        }
        None => {
            refresh(h, camera, true);
            result["auto"] = Value::Null;
        }
    }
    result["applied"] = json!(targets.len());
    result["restarted_stream"] = json!(false);
    h.log(
        "info",
        format!("Restored setup {}: {} changed", setup.name, targets.len()),
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::Setup;

    fn connected(selector: &str) -> WorkerHandle {
        let h = WorkerHandle::new();
        h.request(SessionCommand::Connect {
            camera: selector.into(),
            timeout_ms: 100,
        })
        .unwrap();
        h
    }
    fn get(h: &WorkerHandle, feature: &str) -> Value {
        h.request(SessionCommand::Get {
            feature: feature.into(),
        })
        .unwrap()["value"]
            .clone()
    }
    fn set(h: &WorkerHandle, feature: &str, value: &str) {
        h.request(SessionCommand::Set {
            feature: feature.into(),
            value: value.into(),
        })
        .unwrap();
    }
    fn saved(h: &WorkerHandle, name: &str) -> Setup {
        let reading =
            serde_json::from_value(h.request(SessionCommand::ReadSetup).unwrap()).unwrap();
        crate::setup::from_read(name, None, reading).unwrap()
    }
    fn diff(h: &WorkerHandle, setup: &Setup) -> Result<Value> {
        h.request(SessionCommand::SetupDiff {
            name: None,
            setup: Some(setup.clone()),
            allow_other_model: false,
        })
    }
    fn apply(h: &WorkerHandle, setup: &Setup) -> Result<Value> {
        h.request(SessionCommand::ApplySetup {
            name: None,
            setup: Some(setup.clone()),
            digest: None,
            allow_other_model: false,
            strict: false,
            confirmed_disruptive: None,
        })
    }

    #[test]
    fn a_simulator_setup_restores_as_one_undoable_change() {
        let h = connected("sim:0");
        let setup = saved(&h, "bench");
        let names = setup
            .values
            .iter()
            .map(|v| v.feature.as_str())
            .collect::<Vec<_>>();
        for name in ["ExposureTime", "Gain", "Width", "PixelFormat"] {
            assert!(names.contains(&name), "{names:?}");
        }
        assert!(
            !names
                .iter()
                .any(|n| n.ends_with("Reg") || n.starts_with("Acquisition") && n.ends_with("Start"))
        );
        // Geometry goes first.
        assert_eq!(names[0], "PixelFormat");
        let original = [get(&h, "ExposureTime"), get(&h, "Gain"), get(&h, "Width")];
        set(&h, "ExposureTime", "5000");
        set(&h, "Gain", "3");
        set(&h, "Width", "320");
        let changed = [get(&h, "ExposureTime"), get(&h, "Gain"), get(&h, "Width")];
        let entries = h.snapshot().history.entries.len();
        let preview = diff(&h, &setup).unwrap();
        assert_eq!(preview["changes"].as_array().unwrap().len(), 3, "{preview}");
        assert_eq!(preview["match"], "same_camera");
        assert_eq!(preview["risk"], "reversible");
        assert_eq!(preview["restarts_stream"], false);
        // Previewing changes nothing and records nothing.
        assert_eq!(get(&h, "Width"), 320);
        assert_eq!(h.snapshot().history.entries.len(), entries);
        let applied = apply(&h, &setup).unwrap();
        assert_eq!(applied["applied"], 3, "{applied}");
        assert_eq!(applied["camera_change"]["label"], "Restore setup bench");
        assert_eq!(
            [get(&h, "ExposureTime"), get(&h, "Gain"), get(&h, "Width")],
            original
        );
        let history = h.snapshot().history;
        assert_eq!(history.entries.len(), entries + 1);
        let entry = history.entries.last().unwrap();
        assert_eq!(entry.kind, "apply_setup");
        assert_eq!(entry.setting_changes, 3);
        // Already matching: a second restore changes nothing and records nothing.
        let again = apply(&h, &setup).unwrap();
        assert_eq!(again["applied"], 0);
        assert!(again.get("camera_change").is_none());
        h.request(SessionCommand::Undo).unwrap();
        assert_eq!(
            [get(&h, "ExposureTime"), get(&h, "Gain"), get(&h, "Width")],
            changed
        );
        h.request(SessionCommand::Redo).unwrap();
        assert_eq!(get(&h, "Width"), original[2]);
        h.shutdown();
    }

    #[test]
    fn a_streaming_geometry_change_restarts_once_and_replays() {
        let h = connected("sim:0");
        let setup = saved(&h, "bench");
        set(&h, "Width", "320");
        h.request(SessionCommand::Start).unwrap();
        let preview = diff(&h, &setup).unwrap();
        assert_eq!(preview["restarts_stream"], true);
        assert_eq!(preview["changes"][0]["stream_locked"], true);
        let applied = apply(&h, &setup).unwrap();
        assert_eq!(applied["restarted_stream"], true, "{applied}");
        assert!(h.snapshot().streaming);
        assert_eq!(get(&h, "Width"), 640);
        h.request(SessionCommand::Undo).unwrap();
        assert!(h.snapshot().streaming);
        assert_eq!(get(&h, "Width"), 320);
        h.request(SessionCommand::Redo).unwrap();
        assert_eq!(get(&h, "Width"), 640);
        // A scheduled capture keeps the stream: no restart, nothing changes,
        // and undoing the geometry is refused.
        h.request(SessionCommand::Schedule {
            output: "setup-never-written.png".into(),
            count: 1,
            timeout_ms: 100,
            format: "png".into(),
            first_at_ms: crate::scheduling::now_ms() + 60_000,
            interval_ms: 0,
            storage: Default::default(),
            destination: None,
            on_exists: Default::default(),
            context: Default::default(),
        })
        .unwrap();
        let undo = h.request(SessionCommand::Undo).unwrap_err();
        assert!(
            format!("{undo:#}").contains("cannot restore Width while recording"),
            "{undo:#}"
        );
        let mut narrow = setup.clone();
        for value in &mut narrow.values {
            if value.feature == "Width" {
                value.value = crate::setup::Scalar::Number(256.into());
            }
        }
        let blocked = diff(&h, &narrow).unwrap();
        assert_eq!(
            blocked["blocked"],
            "Stop scheduled captures first: this setup changes Width."
        );
        let error = apply(&h, &narrow).unwrap_err();
        assert_eq!(
            crate::types::error_transport_code(&error),
            Some("unavailable")
        );
        assert_eq!(get(&h, "Width"), 640);
        h.shutdown();
    }

    #[test]
    fn other_models_are_refused_by_the_worker_unless_allowed() {
        let h = connected("sim:0");
        let mut setup = saved(&h, "bench");
        setup.camera.model = "Another camera".into();
        let error = diff(&h, &setup).unwrap_err();
        assert_eq!(
            crate::types::error_transport_code(&error),
            Some("unsupported")
        );
        assert!(error.to_string().contains("allow_other_model"), "{error}");
        assert!(apply(&h, &setup).is_err());
        let allowed = h
            .request(SessionCommand::SetupDiff {
                name: None,
                setup: Some(setup.clone()),
                allow_other_model: true,
            })
            .unwrap();
        assert_eq!(allowed["match"], "other_model");
        setup.camera.model = "Pattern camera".into();
        setup.camera.serial = Some("SIM9".into());
        let same_model = diff(&h, &setup).unwrap();
        assert_eq!(same_model["match"], "same_model");
        assert_eq!(
            same_model["warnings"][0],
            "Saved from another Pattern camera (serial SIM9)."
        );
        h.shutdown();
    }

    #[test]
    fn missing_values_apply_the_rest_unless_strict() {
        let h = connected("sim:0");
        let mut setup = saved(&h, "bench");
        setup.values.push(crate::setup::SetupValue {
            feature: "Scan3dOutputMode".into(),
            value: crate::setup::Scalar::String("x".into()),
            at: Vec::new(),
        });
        setup.values.push(crate::setup::SetupValue {
            feature: "PixelFormat".into(),
            value: crate::setup::Scalar::String("NotAFormat".into()),
            at: vec![],
        });
        setup.values.retain(|v| {
            !(v.feature == "PixelFormat" && v.value == crate::setup::Scalar::String("Mono8".into()))
        });
        set(&h, "Gain", "4");
        let strict = h
            .request(SessionCommand::ApplySetup {
                name: None,
                setup: Some(setup.clone()),
                digest: None,
                allow_other_model: false,
                strict: true,
                confirmed_disruptive: None,
            })
            .unwrap_err();
        assert_eq!(
            crate::types::error_transport_code(&strict),
            Some("unsupported")
        );
        assert!(strict.to_string().contains("Scan3dOutputMode"), "{strict}");
        assert_eq!(get(&h, "Gain").as_f64(), Some(4.0));
        let applied = apply(&h, &setup).unwrap();
        let not_applied = applied["not_applied"].as_array().unwrap();
        assert_eq!(not_applied.len(), 2, "{applied}");
        let row = |feature: &str| {
            not_applied
                .iter()
                .find(|row| row["feature"] == feature)
                .unwrap()
                .clone()
        };
        assert_eq!(row("PixelFormat")["status"], "invalid");
        assert!(
            row("PixelFormat")["reason"]
                .as_str()
                .unwrap()
                .starts_with("PixelFormat must be one of")
        );
        assert_eq!(row("Scan3dOutputMode")["status"], "missing");
        assert_eq!(row("Scan3dOutputMode")["reason"], "not on this camera");
        assert_eq!(get(&h, "Gain").as_f64(), Some(0.0));
        h.shutdown();
    }

    #[test]
    fn a_failed_restore_rolls_every_value_back() {
        let h = connected("sim:0");
        let mut setup = saved(&h, "bench");
        set(&h, "Gain", "5");
        set(&h, "ExposureTime", "7000");
        // Width is outside its limits: the plan warns, the write fails.
        for value in &mut setup.values {
            if value.feature == "Width" {
                value.value = crate::setup::Scalar::Number(99_999.into());
            }
        }
        let preview = diff(&h, &setup).unwrap();
        let width = preview["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["feature"] == "Width")
            .unwrap()
            .clone();
        assert!(
            width["warning"]
                .as_str()
                .unwrap()
                .starts_with("outside the current limits")
        );
        let entries = h.snapshot().history.entries.len();
        let error = apply(&h, &setup).unwrap_err();
        assert!(
            format!("{error:#}").contains("could not restore setup values"),
            "{error:#}"
        );
        assert_eq!(get(&h, "Gain").as_f64(), Some(5.0));
        assert_eq!(get(&h, "ExposureTime").as_f64(), Some(7000.0));
        assert_eq!(get(&h, "Width"), 640);
        assert_eq!(h.snapshot().history.entries.len(), entries);
        h.shutdown();
    }

    #[test]
    fn auto_policies_are_restored_and_released() {
        let h = connected("sim:0");
        let manual = saved(&h, "manual");
        h.request(SessionCommand::Auto {
            balance: Some(0.25),
        })
        .unwrap();
        let automatic = saved(&h, "auto");
        assert_eq!(automatic.auto.as_ref().unwrap().balance, 0.25);
        assert!(
            automatic.skipped.iter().any(|s| s.feature == "ExposureTime"
                && s.reason == "managed by Capturefab auto exposure"),
            "{:?}",
            automatic.skipped
        );
        assert!(!automatic.values.iter().any(|v| v.feature == "ExposureTime"));
        apply(&h, &manual).unwrap();
        assert!(h.snapshot().auto.is_none());
        let restored = apply(&h, &automatic).unwrap();
        assert_eq!(restored["auto"]["balance"], 0.25, "{restored}");
        assert_eq!(h.snapshot().auto.unwrap().balance, 0.25);
        h.shutdown();
    }

    #[test]
    fn skip_reasons_are_one_bounded_line() {
        let long = format!("timed out\nat {}\tend\u{7}", "€".repeat(400));
        let reason = reason_text(&long);
        assert!(reason.len() <= MAX_REASON, "{}", reason.len());
        assert!(!reason.chars().any(char::is_control), "{reason}");
        assert!(reason.starts_with("timed out at €"), "{reason}");
        assert!(reason.ends_with('…'));
        assert_eq!(reason_text("read-only"), "read-only");
        let h = WorkerHandle::new();
        let error =
            anyhow::anyhow!("GVCP read to 192.168.1.20:3956\nfailed").context("read LineSource");
        assert_eq!(unreadable(&h, "LineSource", &[], &error), "could not read");
        let timeout = Categorized::error(ErrorKind::Timeout, "camera 10.0.0.7 did not answer");
        assert_eq!(
            unreadable(&h, "LineSource", &[], &timeout),
            "could not read: timed out"
        );
        // A setup holding the reason still validates.
        let setup = crate::setup::from_read(
            "reasons",
            None,
            Reading {
                camera: SavedCamera::from_info(&crate::transport::simulator::info()),
                auto: None,
                values: vec![SetupValue {
                    feature: "Gain".into(),
                    value: Scalar::Number(1.into()),
                    at: Vec::new(),
                }],
                skipped: vec![skip("LineSource", Vec::new(), long)],
            },
        )
        .unwrap();
        assert!(setup.skipped[0].reason.len() <= MAX_REASON);
        h.shutdown();
    }

    #[test]
    fn unconfirmed_disruptive_changes_are_refused_before_any_write() {
        let mut rig = Rig::new();
        let reading: crate::setup::Reading =
            serde_json::from_value(rig.run(SessionCommand::ReadSetup).unwrap()).unwrap();
        let setup = crate::setup::from_read("confirm", None, reading).unwrap();
        // Reviewed with only Gain differing; TriggerMode changes afterwards.
        rig.c().set("Gain", "40").unwrap();
        rig.c().set("TriggerMode", "On").unwrap();
        let apply = |confirmed: Option<Vec<String>>| SessionCommand::ApplySetup {
            name: None,
            setup: Some(setup.clone()),
            digest: None,
            allow_other_model: false,
            strict: false,
            confirmed_disruptive: confirmed,
        };
        let refused = rig.run(apply(Some(Vec::new()))).unwrap_err();
        assert_eq!(crate::types::error_transport_code(&refused), Some("usage"));
        assert!(
            refused.to_string().contains("also changes TriggerMode"),
            "{refused}"
        );
        assert_eq!(rig.c().get("Gain").unwrap(), 40);
        assert_eq!(rig.c().get("TriggerMode").unwrap(), "On");
        rig.run(apply(Some(vec!["TriggerMode".into()]))).unwrap();
        assert_eq!(rig.c().get("TriggerMode").unwrap(), "Off");
        rig.c().set("TriggerMode", "On").unwrap();
        // Without the field (RPC, workflows), nothing is checked.
        rig.run(apply(None)).unwrap();
        assert_eq!(rig.c().get("TriggerMode").unwrap(), "Off");
        rig.h.shutdown();
    }

    #[test]
    fn floats_within_half_a_step_already_match() {
        assert!(close(&json!(5000.0), &json!(5000.1), Some(0.2)));
        assert!(!close(&json!(5000.0), &json!(5000.3), Some(0.2)));
        assert!(close(&json!(0.30000000000000004), &json!(0.3), None));
        assert!(!close(&json!(1.0), &json!(1.001), None));
        assert!(close(&json!(5), &json!(5), Some(1.0)));
        assert!(close(&json!(5), &json!(5.0), None));
        assert!(!close(&json!("On"), &json!("Off"), None));
    }

    /// Gain at two GainSelector banks, a setting under a selector a setup
    /// does not enumerate, a trigger mode, a register and a command.
    struct Banked;
    impl crate::types::RegisterIo for Banked {
        fn read_memory(&mut self, _: u64, length: usize) -> Result<Vec<u8>> {
            Ok(vec![0; length])
        }
        fn write_memory(&mut self, _: u64, _: &[u8]) -> Result<()> {
            Ok(())
        }
    }
    impl crate::types::Backend for Banked {
        fn xml(&mut self) -> Result<String> {
            Ok(concat!(
                "<RegisterDescription><Category Name='Root'><pFeature>GainSelector</pFeature><pFeature>Gain</pFeature>",
                "<pFeature>BankSelector</pFeature><pFeature>Setting</pFeature><pFeature>TriggerMode</pFeature>",
                "<pFeature>Scratch</pFeature><pFeature>DoIt</pFeature><pFeature>UserSetSelector</pFeature></Category>",
                "<Enumeration Name='GainSelector'><EnumEntry Name='All'><Value>0</Value></EnumEntry><EnumEntry Name='Red'><Value>1</Value></EnumEntry><Value>0</Value><pSelected>Gain</pSelected></Enumeration>",
                "<Integer Name='Gain'><pIndex>GainSelector</pIndex><pValueIndexed Index='0'>GainAll</pValueIndexed><pValueIndexed Index='1'>GainRed</pValueIndexed></Integer>",
                "<Integer Name='GainAll'><Value>10</Value></Integer><Integer Name='GainRed'><Value>11</Value></Integer>",
                "<Enumeration Name='BankSelector'><EnumEntry Name='A'><Value>0</Value></EnumEntry><EnumEntry Name='B'><Value>1</Value></EnumEntry><Value>0</Value><pSelected>Setting</pSelected></Enumeration>",
                "<Integer Name='Setting'><pIndex>BankSelector</pIndex><pValueIndexed Index='0'>AValue</pValueIndexed><pValueIndexed Index='1'>BValue</pValueIndexed></Integer>",
                "<Integer Name='AValue'><Value>1</Value></Integer><Integer Name='BValue'><Value>2</Value></Integer>",
                "<Enumeration Name='TriggerMode'><EnumEntry Name='Off'><Value>0</Value></EnumEntry><EnumEntry Name='On'><Value>1</Value></EnumEntry><Value>0</Value></Enumeration>",
                "<IntReg Name='Scratch'><Address>0x100</Address><Length>4</Length><AccessMode>RW</AccessMode></IntReg>",
                "<Command Name='DoIt'><pValue>Scratch</pValue><CommandValue>1</CommandValue></Command>",
                "<Enumeration Name='UserSetSelector'><EnumEntry Name='Default'><Value>0</Value></EnumEntry><Value>0</Value></Enumeration>",
                "</RegisterDescription>"
            )
            .into())
        }
        fn start(&mut self, _: usize) -> Result<()> {
            Ok(())
        }
        fn next_frame(&mut self, _: Duration) -> Result<Frame> {
            anyhow::bail!("no frames")
        }
        fn stop(&mut self) -> Result<()> {
            Ok(())
        }
    }

    struct Rig {
        h: WorkerHandle,
        camera: Option<Camera>,
        forwarder: Option<(Forwarder, f64)>,
        auto: Option<AutoController>,
        jobs: Vec<Scheduled>,
    }
    impl Rig {
        fn new() -> Self {
            let h = WorkerHandle::new();
            let mut camera = Some(
                Camera::from_backend(crate::transport::simulator::info(), Box::new(Banked))
                    .unwrap(),
            );
            refresh(&h, &mut camera, true);
            Self {
                h,
                camera,
                forwarder: None,
                auto: None,
                jobs: Vec::new(),
            }
        }
        fn run(&mut self, command: SessionCommand) -> Result<Value> {
            process(
                &self.h,
                &mut self.camera,
                &mut self.forwarder,
                &mut self.auto,
                &mut self.jobs,
                &mut 1,
                &mut false,
                command,
            )
        }
        fn c(&mut self) -> &mut Camera {
            self.camera.as_mut().unwrap()
        }
        fn gains(&mut self) -> (Value, Value, Value) {
            let c = self.c();
            let selected = c.get("GainSelector").unwrap();
            c.set("GainSelector", "All").unwrap();
            let all = c.get("Gain").unwrap();
            c.set("GainSelector", "Red").unwrap();
            let red = c.get("Gain").unwrap();
            c.set("GainSelector", selected.as_str().unwrap()).unwrap();
            (selected, all, red)
        }
    }

    #[test]
    fn every_gain_bank_round_trips_and_the_selection_is_kept() {
        let mut rig = Rig::new();
        let reading: crate::setup::Reading =
            serde_json::from_value(rig.run(SessionCommand::ReadSetup).unwrap()).unwrap();
        let gains = reading
            .values
            .iter()
            .filter(|v| v.feature == "Gain")
            .map(|v| (v.at[0].value.clone(), v.value.clone()))
            .collect::<Vec<_>>();
        assert_eq!(gains.len(), 2, "{:?}", reading.values);
        // A selector a setup does not enumerate is read at its bank only.
        let settings = reading
            .values
            .iter()
            .filter(|v| v.feature == "Setting")
            .collect::<Vec<_>>();
        assert_eq!(settings.len(), 1);
        assert_eq!(settings[0].at[0].value, "A");
        // Registers, commands and user sets are never saved.
        for name in ["Scratch", "DoIt", "UserSetSelector"] {
            assert!(!reading.values.iter().any(|v| v.feature == name), "{name}");
        }
        assert_eq!(rig.c().get("GainSelector").unwrap(), "All");
        let setup = crate::setup::from_read("banks", None, reading).unwrap();
        rig.c().set("GainSelector", "Red").unwrap();
        rig.c().set("Gain", "50").unwrap();
        rig.c().set("GainSelector", "All").unwrap();
        rig.c().set("Gain", "40").unwrap();
        rig.c().set("TriggerMode", "On").unwrap();
        rig.c().set("GainSelector", "Red").unwrap();
        let mut setup_on = setup.clone();
        for value in &mut setup_on.values {
            if value.feature == "GainSelector" {
                value.value = crate::setup::Scalar::String("Red".into());
            }
        }
        let preview = rig
            .run(SessionCommand::SetupDiff {
                name: None,
                setup: Some(setup_on.clone()),
                allow_other_model: false,
            })
            .unwrap();
        assert_eq!(preview["risk"], "disruptive", "{preview}");
        let trigger = preview["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["feature"] == "TriggerMode")
            .unwrap();
        assert_eq!(trigger["risk"], "disruptive");
        assert_eq!(
            (&trigger["current"], &trigger["saved"]),
            (&json!("On"), &json!("Off"))
        );
        let applied = rig
            .run(SessionCommand::ApplySetup {
                name: None,
                setup: Some(setup_on),
                digest: None,
                allow_other_model: false,
                strict: false,
                confirmed_disruptive: None,
            })
            .unwrap();
        assert_eq!(applied["applied"], 3, "{applied}");
        assert_eq!(rig.gains(), (json!("Red"), json!(10), json!(11)));
        assert_eq!(rig.c().get("TriggerMode").unwrap(), "Off");
        rig.run(SessionCommand::Undo).unwrap();
        assert_eq!(rig.gains(), (json!("Red"), json!(40), json!(50)));
        assert_eq!(rig.c().get("TriggerMode").unwrap(), "On");
        rig.h.shutdown();
    }

    #[test]
    fn the_worker_refuses_registers_commands_and_managed_selectors() {
        let mut rig = Rig::new();
        let reading: crate::setup::Reading =
            serde_json::from_value(rig.run(SessionCommand::ReadSetup).unwrap()).unwrap();
        let mut setup = crate::setup::from_read("banks", None, reading).unwrap();
        let value = |feature: &str, at: Vec<crate::setup::Bank>| crate::setup::SetupValue {
            feature: feature.into(),
            value: crate::setup::Scalar::Number(1.into()),
            at,
        };
        let bank = |selector: &str| {
            vec![crate::setup::Bank {
                selector: selector.into(),
                value: "A".into(),
            }]
        };
        setup.values.extend([
            value("Scratch", Vec::new()),
            value("DoIt", Vec::new()),
            value("Setting", bank("TriggerModeSelector")),
            value("TriggerMode", bank("BankSelector")),
        ]);
        let preview = rig
            .run(SessionCommand::SetupDiff {
                name: None,
                setup: Some(setup.clone()),
                allow_other_model: false,
            })
            .unwrap();
        let refused = preview["not_applied"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["feature"].as_str().unwrap(),
                    row["status"].as_str().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            refused,
            [
                ("Scratch", "refused"),
                ("DoIt", "refused"),
                ("Setting", "missing"),
                ("TriggerMode", "refused")
            ]
        );
        // Strict refuses all of it before writing.
        rig.c().set("Gain", "33").unwrap();
        assert!(
            rig.run(SessionCommand::ApplySetup {
                name: None,
                setup: Some(setup),
                digest: None,
                allow_other_model: false,
                strict: true,
                confirmed_disruptive: None,
            })
            .is_err()
        );
        assert_eq!(rig.c().get("Gain").unwrap(), 33);
        rig.h.shutdown();
    }

    #[test]
    fn the_write_deadline_is_enforced() {
        let mut rig = Rig::new();
        let address = Address {
            feature: "TriggerMode".into(),
            selectors: Vec::new(),
        };
        let h = rig.h.clone();
        let error = restore_ordered(
            &h,
            rig.c(),
            "slow",
            &[(address, json!("On"))],
            Duration::ZERO,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "setup restore exceeded 0 s after 0 of 1 values"
        );
        assert_eq!(rig.c().get("TriggerMode").unwrap(), "Off");
        rig.h.shutdown();
    }
}
