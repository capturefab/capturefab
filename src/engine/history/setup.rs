//! Saved setups in the camera worker: reading one, comparing it with the
//! camera, and restoring it inside one history checkpoint. The coordinator
//! loads named setups; the worker checks the camera's identity and node
//! kinds itself, so remote, workflow and RPC callers get the same checks.
use super::*;
use crate::genicam::Availability;
use crate::setup::{
    Bank, Confirmed, MAX_BANKS, MAX_SETUP_VALUES, MAX_STRING, Match, Reading, SETUP_ALL_BANKS,
    SavedAuto, SavedCamera, Scalar, Setup, SetupValue, Skipped,
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

/// What one bank of a feature holds for a setup.
enum BankRead {
    Value(Value),
    ReadOnly,
    Unavailable,
}

/// Read every configuration value a setup can restore. One unreadable bank
/// or feature never fails the whole read: it is recorded in `skipped`.
pub(super) fn read(
    h: &WorkerHandle,
    camera: &mut Camera,
    auto: &Option<AutoController>,
) -> Result<Value> {
    let mut walked = h
        .state
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .features
        .clone();
    if !walked.iter().any(|feature| feature.value.is_some()) {
        walked = camera.features();
    }
    // A value read at every bank is listed even when it has no value at the
    // selected bank: an output line's source is unavailable while an input
    // line is selected.
    let names = walked
        .into_iter()
        .filter(|feature| {
            feature.value.is_some()
                || camera
                    .selectors(&feature.name)
                    .iter()
                    .any(|selector| SETUP_ALL_BANKS.contains(&selector.as_str()))
        })
        .map(|feature| feature.name)
        .collect::<BTreeSet<_>>();
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
        // Banks where the value is read-only or not available (an input
        // line's source) are reported only when another bank of it was saved.
        let mut quiet = Vec::new();
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
                Ok(if camera.availability(&name) != Availability::Available {
                    BankRead::Unavailable
                } else if camera.is_configuration_value(&name) {
                    BankRead::Value(camera.get(&name)?)
                } else {
                    BankRead::ReadOnly
                })
            });
            // The bank is stored beside the reason, so the reason omits it.
            match read {
                Ok(BankRead::Value(value)) => match Scalar::from_value(&value) {
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
                Ok(BankRead::ReadOnly) => quiet.push(skip(&name, at, "read-only")),
                Ok(BankRead::Unavailable) => quiet.push(skip(&name, at, "not available")),
                Err(error) => {
                    let reason = unreadable(h, &name, &at, &error);
                    skipped.push(skip(&name, at, reason));
                }
            }
        }
        if saved {
            skipped.extend(quiet.into_iter().filter(|s| !s.at.is_empty()));
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
    /// A change to a value that is not available now: written after the
    /// others, which may make it available, and otherwise reported with
    /// this status and reason.
    gated: Option<(Status, String)>,
    /// `pIsLocked` holds now, while the camera streams.
    locked_now: bool,
    /// Written only with the stream stopped.
    stream_locked: bool,
}

/// How a restore changes Capturefab's software auto exposure.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum AutoChange {
    None,
    /// Auto starts with this balance.
    Enter(f64),
    /// Auto keeps running with another balance.
    Balance {
        from: f64,
        to: f64,
    },
    /// Auto stops; the camera keeps its values (manual mode).
    Release,
}
impl AutoChange {
    fn of(current: Option<f64>, saved: Option<f64>) -> Self {
        match (current, saved) {
            (None, None) => Self::None,
            (None, Some(balance)) => Self::Enter(balance),
            (Some(_), None) => Self::Release,
            (Some(from), Some(to)) if (from - to).abs() <= 1e-6 => Self::None,
            (Some(from), Some(to)) => Self::Balance { from, to },
        }
    }
    fn action(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Enter(_) => "enter",
            Self::Balance { .. } => "balance",
            Self::Release => "release",
        }
    }
    /// The words a log line or result uses, if anything changes.
    fn words(self) -> Option<String> {
        match self {
            Self::None => None,
            Self::Enter(balance) => Some(format!("auto exposure on (balance {balance:.2})")),
            Self::Balance { from, to } => Some(format!("auto balance {from:.2} → {to:.2}")),
            Self::Release => Some("auto exposure off (manual)".into()),
        }
    }
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
    /// Software auto exposure's balance now and in the setup.
    auto: (Option<f64>, Option<f64>),
}
impl Plan {
    pub(super) fn changes(&self) -> impl Iterator<Item = &Row> {
        self.rows.iter().filter(|row| row.status == Status::Change)
    }
    pub(super) fn auto_change(&self) -> AutoChange {
        AutoChange::of(self.auto.0, self.auto.1)
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
    /// camera may have changed since the reviewed diff. A confirmation
    /// names the feature and its banks; a bare name confirms only a value
    /// without banks. `None` checks nothing (RPC and workflow callers).
    pub(super) fn confirm(&self, confirmed: Option<&[Confirmed]>) -> Result<()> {
        let Some(confirmed) = confirmed else {
            return Ok(());
        };
        let mut unconfirmed = Vec::new();
        for row in self.changes() {
            let feature = row.address.feature.as_str();
            if risk_of(feature) != "disruptive"
                || confirmed
                    .iter()
                    .any(|confirmed| confirmed.confirms(feature, &row.at))
            {
                continue;
            }
            let label = if row.at.is_empty() {
                feature.to_owned()
            } else {
                format!("{feature} ({})", bank_text(&row.at))
            };
            if !unconfirmed.contains(&label) {
                unconfirmed.push(label);
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

/// What a value that is unavailable now says in the diff.
const LATER: &str = "applies only if earlier changes in this setup enable it";

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
        gated: None,
        locked_now: false,
        stream_locked: false,
    };
    let set = |row: &mut Row, status, reason: String| {
        row.status = status;
        row.reason = Some(reason);
    };
    // A value hidden now by another setting is written after the others;
    // the final check decides whether it took.
    let defer = |row: &mut Row, fallback: Status, reason: String, warning: String| {
        row.status = Status::Change;
        row.reason = None;
        row.warning = Some(warning);
        row.gated = Some((fallback, reason));
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
    let target = value.value.to_value();
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
            camera.is_streaming() && camera.is_locked(feature),
        ))
    });
    let (writable, current, bounds, choices, locked_now) = match read {
        Ok(read) => read,
        Err(error) => {
            let state = addressed(camera, &address, |camera| {
                Ok((
                    camera.availability(feature),
                    camera.is_configuration_value(feature),
                    match &target {
                        Value::String(symbol) if enumeration => {
                            camera.entry_availability(feature, symbol)
                        }
                        _ => Availability::Available,
                    },
                ))
            });
            match state {
                Ok((Availability::Unavailable, true, Availability::Absent)) => set(
                    &mut row,
                    Status::Invalid,
                    format!("{feature} has no choice {}", text(&target)),
                ),
                Ok((Availability::Unavailable, true, _)) => defer(
                    &mut row,
                    Status::Missing,
                    "not available on this camera now".into(),
                    format!("not available now; {LATER}"),
                ),
                _ => set(
                    &mut row,
                    Status::Missing,
                    format!("could not read: {error:#}"),
                ),
            }
            return row;
        }
    };
    row.current = Some(current.clone());
    row.locked_now = locked_now;
    if !writable {
        set(&mut row, Status::ReadOnly, "read-only now".into());
        return row;
    }
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
        let reason = format!("{feature} must be one of {}", choices.join(", "));
        let entry = addressed(camera, &address, |camera| {
            Ok(camera.entry_availability(feature, choice))
        });
        if matches!(entry, Ok(Availability::Unavailable)) {
            defer(
                &mut row,
                Status::Invalid,
                format!("{choice} is not available now; {reason}"),
                format!("{choice} is not available now; {LATER}"),
            );
        } else {
            set(&mut row, Status::Invalid, reason);
        }
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

/// Compare `setup` with the camera. A setup from another model is refused
/// unless `allow_other_model`.
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
    let mut rows = setup
        .values
        .iter()
        .map(|value| row(camera, auto, setup, value))
        .collect::<Vec<_>>();
    // Only another change can make a value available that is not now.
    if !rows
        .iter()
        .any(|row| row.status == Status::Change && row.gated.is_none())
    {
        for row in &mut rows {
            if let Some((status, reason)) = row.gated.take() {
                row.status = status;
                row.reason = Some(reason);
                row.warning = None;
            }
        }
    }
    let changed = rows
        .iter()
        .filter(|row| row.status == Status::Change)
        .map(|row| row.address.feature.clone())
        .collect::<BTreeSet<_>>();
    let changed = changed.iter().map(String::as_str).collect::<Vec<_>>();
    // Geometry and format, and anything the node map locks while the
    // camera acquires, unless another change here is what unlocks it (an
    // auto flag, for example).
    let mut locked = Vec::new();
    for row in &mut rows {
        if row.status != Status::Change {
            continue;
        }
        let feature = row.address.feature.as_str();
        row.stream_locked = crate::genicam::stream_locked(feature)
            || row.locked_now && !camera.lock_released_by(feature, &changed);
        if row.stream_locked && !locked.contains(&row.address.feature) {
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
        auto: (
            auto.as_ref().map(|controller| controller.status().balance),
            setup.auto.as_ref().map(|saved| saved.balance),
        ),
    })
}

/// A selector a setup reads bank by bank only chooses which bank later
/// reads and writes address; the per-bank values carry their own risk.
fn risk_of(feature: &str) -> &'static str {
    if risk::image_quality(feature) || SETUP_ALL_BANKS.contains(&feature) {
        "reversible"
    } else {
        "disruptive"
    }
}

/// The diff a person or script reviews before a restore.
pub(super) fn report(plan: &Plan, info: &crate::types::CameraInfo) -> Value {
    outcome(plan, info, &BTreeSet::new())
}

/// The diff, with the changes in `unavailable` (values that stayed
/// unavailable after the other writes) reported as not applied.
fn outcome(plan: &Plan, info: &crate::types::CameraInfo, unavailable: &BTreeSet<Address>) -> Value {
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
        let stayed = row
            .gated
            .as_ref()
            .filter(|_| row.status == Status::Change && unavailable.contains(&row.address));
        match row.status {
            _ if stayed.is_some() => {
                let (status, _) = stayed.unwrap();
                entry["status"] = json!(status.as_str());
                entry["saved"] = row.saved.to_value();
                entry["reason"] =
                    json!("still not available after the other changes in this setup");
                not_applied.push(entry);
            }
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
                entry["stream_locked"] = json!(row.stream_locked);
                if row.gated.is_some() {
                    entry["deferred"] = json!(true);
                }
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
    let auto = plan.auto_change();
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
        "auto_change": {
            "current": plan.auto.0,
            "saved": plan.auto.1,
            "action": auto.action(),
            "risk": "reversible",
        },
    })
}

/// Add the value at `address` to the checkpoint taken before a restore, so
/// a bank the camera did not have selected is covered too. A bank that is
/// not available now has no value to restore and is recorded as such.
pub(super) fn extend_at(
    camera: &mut Camera,
    before: &mut Configuration,
    address: &Address,
) -> Result<()> {
    if let std::collections::btree_map::Entry::Vacant(entry) = before.values.entry(address.clone())
    {
        let value = read_at(camera, address)
            .with_context(|| format!("read {} for undo", address.feature))?;
        entry.insert(value);
    }
    Ok(())
}

fn inc(camera: &mut Camera, feature: &str) -> Option<f64> {
    camera.bounds(feature).ok().and_then(|bounds| bounds.inc)
}

/// What a restore wrote.
#[derive(Debug)]
pub(super) struct Restored {
    /// The addresses written at least once.
    written: BTreeSet<Address>,
    /// Optional targets that stayed unavailable and were not written.
    unavailable: BTreeSet<Address>,
}

/// Whether the optional target at `address` still cannot be written: the
/// feature, or the enumeration entry it names, is not available.
fn still_unavailable(camera: &mut Camera, address: &Address, target: &Value) -> bool {
    addressed(camera, address, |camera| {
        let feature = &address.feature;
        Ok(camera.availability(feature) != Availability::Available
            || camera.node_kind(feature) == Some("Enumeration")
                && camera.entry_availability(feature, &text(target)) != Availability::Available)
    })
    .unwrap_or(false)
}

/// Write `targets` in apply order, retrying values a later write unlocked,
/// made available or moved, then verify every one. A target in `optional`
/// that is still unavailable at the end is left out instead of failing.
/// Gives up after `limit`.
pub(super) fn restore_ordered(
    h: &WorkerHandle,
    camera: &mut Camera,
    name: &str,
    targets: &[(Address, Value)],
    optional: &BTreeSet<Address>,
    limit: Duration,
) -> Result<Restored> {
    let deadline = Instant::now() + limit;
    let total = targets.len();
    let mut pending = targets.iter().collect::<Vec<_>>();
    let mut errors = Vec::new();
    let mut written = BTreeSet::new();
    for pass in 1..=total.clamp(2, 4) {
        if pending.is_empty() {
            break;
        }
        pending.sort_by(|a, b| {
            crate::setup::order_key(&a.0.feature, Some(&text(&a.1)))
                .cmp(&crate::setup::order_key(&b.0.feature, Some(&text(&b.1))))
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
        for (count, (address, target)) in pending.iter().map(|t| (&t.0, &t.1)).enumerate() {
            ensure!(
                Instant::now() < deadline,
                "setup restore exceeded {} s after {} of {total} values",
                limit.as_secs(),
                settled + count
            );
            let result = addressed(camera, address, |camera| {
                // A current value an earlier write hid can still be replaced.
                if let Ok(current) = camera.get(&address.feature)
                    && close(&current, target, inc(camera, &address.feature))
                {
                    return Ok(false);
                }
                camera.set(&address.feature, &text(target))?;
                Ok(true)
            });
            match result {
                Ok(true) => {
                    written.insert(address.clone());
                }
                Ok(false) => {}
                Err(error) => {
                    failures.insert(address.clone(), format!("{error:#}"));
                }
            }
        }
        errors.clear();
        // Every target is checked again, so one that a later write moved
        // (binning rescales Width) is written on the next pass.
        pending = targets
            .iter()
            .filter(|(address, target)| {
                match addressed(camera, address, |camera| {
                    Ok((camera.get(&address.feature)?, inc(camera, &address.feature)))
                }) {
                    Ok((current, inc)) if close(&current, target, inc) => false,
                    Ok(_) => {
                        errors.push((
                            address.clone(),
                            format!(
                                "{}: {}",
                                address.feature,
                                failures
                                    .get(address)
                                    .map(String::as_str)
                                    .unwrap_or("did not keep the saved value")
                            ),
                        ));
                        true
                    }
                    Err(error) => {
                        errors.push((address.clone(), format!("{}: {error:#}", address.feature)));
                        true
                    }
                }
            })
            .collect();
    }
    let mut unavailable = BTreeSet::new();
    pending.retain(|(address, target)| {
        if optional.contains(address) && still_unavailable(camera, address, target) {
            unavailable.insert(address.clone());
            return false;
        }
        true
    });
    errors.retain(|(address, _)| !unavailable.contains(address));
    ensure!(
        pending.is_empty(),
        "could not restore setup values: {}",
        errors
            .iter()
            .map(|(_, error)| error.as_str())
            .collect::<Vec<_>>()
            .join("; ")
    );
    Ok(Restored {
        written,
        unavailable,
    })
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
    strict: bool,
    mut checkpoint: impl FnMut(&mut Camera, &Address) -> Result<()>,
) -> Result<Value> {
    // Every value that already matches is verified too, and written again
    // if an earlier write moved it.
    let targets = plan
        .rows
        .iter()
        .filter(|row| matches!(row.status, Status::Change | Status::Same))
        .map(|row| (row.address.clone(), row.target.clone()))
        .collect::<Vec<_>>();
    let optional = plan
        .changes()
        .filter(|row| row.gated.is_some())
        .map(|row| row.address.clone())
        .collect::<BTreeSet<_>>();
    let c = camera_mut(camera)?;
    // The stream is stopped now if the plan restarts it: a value locked
    // only while acquiring must have become writable.
    if plan.restarts_stream {
        for row in plan
            .changes()
            .filter(|row| row.stream_locked && row.locked_now)
        {
            if addressed(c, &row.address, |camera| {
                Ok(camera.is_locked(&row.address.feature))
            })? {
                return Err(Categorized::error(
                    ErrorKind::OperationFailed,
                    format!(
                        "{} is still locked with the stream stopped",
                        row.address.feature
                    ),
                ));
            }
        }
    }
    for (address, _) in &targets {
        checkpoint(c, address).context(
            "apply_setup changed nothing: the values it would replace could not be read",
        )?;
    }
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
    let restored = restore_ordered(h, c, &setup.name, &targets, &optional, DEADLINE)?;
    if strict && !restored.unavailable.is_empty() {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            format!(
                "Setup {} cannot be applied strictly: {} stayed unavailable after the other changes",
                setup.name,
                restored
                    .unavailable
                    .iter()
                    .map(|address| address.feature.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    let mut result = outcome(plan, &c.info, &restored.unavailable);
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
    let applied = plan
        .rows
        .iter()
        .filter(|row| match row.status {
            Status::Change => !restored.unavailable.contains(&row.address),
            Status::Same => restored.written.contains(&row.address),
            _ => false,
        })
        .count();
    let auto_change = plan.auto_change();
    result["applied"] = json!(applied);
    result["auto_changed"] = json!(auto_change != AutoChange::None);
    result["restarted_stream"] = json!(false);
    h.log(
        "info",
        format!(
            "Restored setup {}: {applied} changed{}",
            setup.name,
            auto_change
                .words()
                .map(|words| format!("; {words}"))
                .unwrap_or_default()
        ),
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::Setup;

    fn connected(selector: &str) -> WorkerHandle {
        let h = WorkerHandle::new();
        h.request(SessionCommand::connect(selector.into(), 100))
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
        assert_eq!(
            error.to_string(),
            "Restoring setup bench failed; every value was put back"
        );
        assert_eq!(crate::cli::error_code(&error).0, "operation_failed");
        assert_eq!(get(&h, "Gain").as_f64(), Some(5.0));
        assert_eq!(get(&h, "ExposureTime").as_f64(), Some(7000.0));
        assert_eq!(get(&h, "Width"), 640);
        assert_eq!(h.snapshot().history.entries.len(), entries);
        // A restore that stopped the stream says it started it again.
        set(&h, "Width", "320");
        h.request(SessionCommand::Start).unwrap();
        let mut gain = saved(&h, "bench");
        for value in &mut gain.values {
            match value.feature.as_str() {
                "Width" => value.value = crate::setup::Scalar::Number(640.into()),
                "Gain" => value.value = crate::setup::Scalar::Number(99_999.into()),
                _ => {}
            }
        }
        let error = apply(&h, &gain).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Restoring setup bench failed; every value was put back; the stream was restarted"
        );
        assert!(h.snapshot().streaming);
        assert_eq!(get(&h, "Width"), 320);
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
        // Leaving auto is shown in the diff, even with no value to change.
        let release = diff(&h, &manual).unwrap();
        assert_eq!(
            release["auto_change"],
            json!({"current":0.25,"saved":null,"action":"release","risk":"reversible"})
        );
        let released = apply(&h, &manual).unwrap();
        assert_eq!(released["auto_changed"], true, "{released}");
        assert!(h.snapshot().auto.is_none());
        let enter = diff(&h, &automatic).unwrap();
        assert_eq!(enter["changes"], json!([]), "{enter}");
        assert_eq!(enter["auto_change"]["action"], "enter");
        assert_eq!(enter["auto_change"]["saved"], 0.25);
        let restored = apply(&h, &automatic).unwrap();
        assert_eq!(restored["auto_changed"], true);
        assert_eq!(restored["applied"], 0);
        assert!(
            h.snapshot()
                .logs
                .iter()
                .any(|l| l.message
                    == "Restored setup auto: 0 changed; auto exposure on (balance 0.25)"),
            "{:?}",
            h.snapshot().logs
        );
        assert_eq!(
            diff(&h, &automatic).unwrap()["auto_change"]["action"],
            "none"
        );
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
        let apply = |confirmed: Option<Vec<Confirmed>>| SessionCommand::ApplySetup {
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
        rig.run(apply(Some(vec![Confirmed::Name("TriggerMode".into())])))
            .unwrap();
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
            Self::with(Box::new(Banked))
        }
        fn with(backend: Box<dyn crate::types::Backend>) -> Self {
            let h = WorkerHandle::new();
            let mut camera =
                Some(Camera::from_backend(crate::transport::simulator::info(), backend).unwrap());
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
            &BTreeSet::new(),
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

    /// Shared state of a `Rich` camera: register memory, a log of register
    /// writes, the acquisition flag and injected read failures.
    #[derive(Clone, Default)]
    struct Wires {
        memory: std::sync::Arc<std::sync::Mutex<BTreeMap<u64, Vec<u8>>>>,
        writes: std::sync::Arc<std::sync::Mutex<Vec<u64>>>,
        acquiring: std::sync::Arc<std::sync::atomic::AtomicBool>,
        /// Reads of Line2's source register fail.
        fail_line2: std::sync::Arc<std::sync::atomic::AtomicBool>,
        /// Reads of BlackLevel's register fail after this many more.
        fail_black_after: std::sync::Arc<std::sync::Mutex<Option<usize>>>,
        black_reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    const LINE_SOURCE: [u64; 3] = [0x20, 0x24, 0x30];
    const LINE_MODE: [u64; 3] = [0x50, 0x54, 0x58];
    const TRIGGER_MODE: u64 = 0x60;
    const TRIGGER_SOURCE: u64 = 0x68;
    const BLACK_LEVEL: u64 = 0x40;
    /// Lines and their pIsAvailable-gated sources, triggers per selector, an
    /// ADC depth that unlocks Mono12, binning that rescales Width, firmware
    /// exposure auto that locks ExposureTime, a value locked while acquiring,
    /// a register-backed BlackLevel, a 300-bank counter and a stream-channel
    /// packet size.
    struct Rich(Wires);
    impl crate::types::RegisterIo for Rich {
        fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
            use std::sync::atomic::Ordering::SeqCst;
            if address == LINE_SOURCE[2] && self.0.fail_line2.load(SeqCst) {
                anyhow::bail!("injected read failure at 0x{address:x}");
            }
            if address == BLACK_LEVEL {
                let reads = self.0.black_reads.fetch_add(1, SeqCst);
                if self
                    .0
                    .fail_black_after
                    .lock()
                    .unwrap()
                    .is_some_and(|n| reads >= n)
                {
                    anyhow::bail!("injected read failure at 0x{address:x}");
                }
            }
            let mut bytes = vec![0; length];
            if address == 0x10 {
                bytes[0] = u8::from(self.0.acquiring.load(SeqCst));
            } else if let Some(stored) = self.0.memory.lock().unwrap().get(&address) {
                bytes[..stored.len().min(length)]
                    .copy_from_slice(&stored[..stored.len().min(length)]);
            }
            Ok(bytes)
        }
        fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
            self.0.writes.lock().unwrap().push(address);
            self.0.memory.lock().unwrap().insert(address, data.to_vec());
            Ok(())
        }
    }
    impl crate::types::Backend for Rich {
        fn xml(&mut self) -> Result<String> {
            let reg = |name: &str, address: u64, access: &str| {
                format!(
                    "<IntReg Name='{name}'><Address>0x{address:x}</Address><Length>4</Length><AccessMode>{access}</AccessMode><Endianess>LittleEndian</Endianess><Sign>Unsigned</Sign></IntReg>"
                )
            };
            let entries = |names: &[&str]| {
                names
                    .iter()
                    .enumerate()
                    .map(|(i, name)| {
                        format!("<EnumEntry Name='{name}'><Value>{i}</Value></EnumEntry>")
                    })
                    .collect::<String>()
            };
            let indexed = |name: &str, selector: &str, banks: &[(&str, u64)]| {
                let mut xml = format!("<Integer Name='{name}'><pIndex>{selector}</pIndex>");
                for (i, (bank, _)) in banks.iter().enumerate() {
                    xml += &format!("<pValueIndexed Index='{i}'>{bank}</pValueIndexed>");
                }
                xml += "</Integer>";
                for (bank, address) in banks {
                    xml += &reg(bank, *address, "RW");
                }
                xml
            };
            let counters = (0..300).map(|i| format!("Counter{i}")).collect::<Vec<_>>();
            let counters = counters.iter().map(String::as_str).collect::<Vec<_>>();
            let features = [
                "LineSelector",
                "LineMode",
                "LineSource",
                "TriggerSelector",
                "TriggerMode",
                "TriggerSource",
                "ADCBitDepth",
                "PixelFormat",
                "BinningHorizontal",
                "Width",
                "ExposureAuto",
                "ExposureTime",
                "DigitalShift",
                "BlackLevel",
                "CounterSelector",
                "CounterDuration",
                "DeviceStreamChannelPacketSize",
            ];
            Ok([
                "<RegisterDescription><Category Name='Root'>".to_owned(),
                features.map(|f| format!("<pFeature>{f}</pFeature>")).concat(),
                "</Category>".into(),
                format!("<Enumeration Name='LineSelector'>{}<Value>0</Value><pSelected>LineMode</pSelected><pSelected>LineSource</pSelected></Enumeration>", entries(&["Line0", "Line1", "Line2"])),
                format!("<Enumeration Name='LineMode'>{}<pValue>LineModeValue</pValue></Enumeration>", entries(&["Input", "Output"])),
                indexed("LineModeValue", "LineSelector", &[("LM0", LINE_MODE[0]), ("LM1", LINE_MODE[1]), ("LM2", LINE_MODE[2])]),
                format!("<Enumeration Name='LineSource'>{}<pValue>LineSourceValue</pValue><pIsAvailable>IsOutput</pIsAvailable></Enumeration>", entries(&["Off", "ExposureActive"])),
                indexed("LineSourceValue", "LineSelector", &[("LS0", LINE_SOURCE[0]), ("LS1", LINE_SOURCE[1]), ("LS2", LINE_SOURCE[2])]),
                "<IntSwissKnife Name='IsOutput'><pVariable Name='M'>LineModeValue</pVariable><Formula>M = 1</Formula></IntSwissKnife>".into(),
                format!("<Enumeration Name='TriggerSelector'>{}<Value>0</Value><pSelected>TriggerMode</pSelected><pSelected>TriggerSource</pSelected></Enumeration>", entries(&["FrameStart", "LineStart"])),
                format!("<Enumeration Name='TriggerMode'>{}<pValue>TriggerModeValue</pValue></Enumeration>", entries(&["Off", "On"])),
                indexed("TriggerModeValue", "TriggerSelector", &[("TM0", TRIGGER_MODE), ("TM1", TRIGGER_MODE + 4)]),
                format!("<Enumeration Name='TriggerSource'>{}<pValue>TriggerSourceValue</pValue></Enumeration>", entries(&["Software", "Line0"])),
                indexed("TriggerSourceValue", "TriggerSelector", &[("TS0", TRIGGER_SOURCE), ("TS1", TRIGGER_SOURCE + 4)]),
                format!("<Enumeration Name='ADCBitDepth'>{}<pValue>ADCValue</pValue></Enumeration>", entries(&["Bit8", "Bit12"])),
                "<Integer Name='ADCValue'><Value>0</Value></Integer>".into(),
                "<IntSwissKnife Name='Is12'><pVariable Name='B'>ADCValue</pVariable><Formula>B = 1</Formula></IntSwissKnife>".into(),
                "<Enumeration Name='PixelFormat'><EnumEntry Name='Mono8'><Value>17301505</Value></EnumEntry><EnumEntry Name='Mono12'><Value>17825797</Value><pIsAvailable>Is12</pIsAvailable></EnumEntry><Value>17301505</Value></Enumeration>".into(),
                "<Integer Name='BinningHorizontal'><Value>1</Value><Min>1</Min><Max>4</Max></Integer>".into(),
                "<Integer Name='Width'><pValue>WidthConverter</pValue></Integer>".into(),
                "<IntConverter Name='WidthConverter'><pVariable Name='B'>BinningHorizontal</pVariable><pValue>SensorWidth</pValue><FormulaFrom>TO / B</FormulaFrom><FormulaTo>FROM * B</FormulaTo></IntConverter>".into(),
                "<Integer Name='SensorWidth'><Value>640</Value></Integer>".into(),
                format!("<Enumeration Name='ExposureAuto'>{}<pValue>ExposureAutoValue</pValue></Enumeration>", entries(&["Off", "Continuous"])),
                "<Integer Name='ExposureAutoValue'><Value>0</Value></Integer>".into(),
                "<IntSwissKnife Name='AutoLocked'><pVariable Name='A'>ExposureAutoValue</pVariable><Formula>A = 1</Formula></IntSwissKnife>".into(),
                "<Float Name='ExposureTime'><Value>10000</Value><Min>10</Min><Max>1000000</Max><pIsLocked>AutoLocked</pIsLocked></Float>".into(),
                "<Integer Name='DigitalShift'><Value>0</Value><Min>0</Min><Max>4</Max><pIsLocked>Acquiring</pIsLocked></Integer>".into(),
                "<Integer Name='Acquiring'><pValue>AcquiringReg</pValue></Integer>".into(),
                reg("AcquiringReg", 0x10, "RO"),
                "<Integer Name='BlackLevel'><pValue>BlackLevelReg</pValue><Min>0</Min><Max>255</Max></Integer>".into(),
                reg("BlackLevelReg", BLACK_LEVEL, "RW"),
                format!("<Enumeration Name='CounterSelector'>{}<Value>0</Value><pSelected>CounterDuration</pSelected></Enumeration>", entries(&counters)),
                "<Integer Name='CounterDuration'><pIndex>CounterSelector</pIndex><pValueDefault>CounterDurationValue</pValueDefault></Integer>".into(),
                "<Integer Name='CounterDurationValue'><Value>5</Value></Integer>".into(),
                "<Integer Name='DeviceStreamChannelPacketSize'><Value>1500</Value></Integer>".into(),
                "<Integer Name='PayloadSize'><Value>1000</Value><ImposedAccessMode>RO</ImposedAccessMode></Integer>".into(),
                "<Command Name='AcquisitionStart'><pValue>AcquisitionReg</pValue><CommandValue>1</CommandValue></Command>".into(),
                "<Command Name='AcquisitionStop'><pValue>AcquisitionReg</pValue><CommandValue>0</CommandValue></Command>".into(),
                reg("AcquisitionReg", 0x80, "WO"),
                "</RegisterDescription>".into(),
            ]
            .concat())
        }
        fn start(&mut self, _: usize) -> Result<()> {
            self.0
                .acquiring
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn next_frame(&mut self, _: Duration) -> Result<Frame> {
            anyhow::bail!("no frames")
        }
        fn stop(&mut self) -> Result<()> {
            self.0
                .acquiring
                .store(false, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    impl Rig {
        fn rich() -> (Self, Wires) {
            let wires = Wires::default();
            (Self::with(Box::new(Rich(wires.clone()))), wires)
        }
        fn save(&mut self, name: &str) -> Setup {
            refresh(&self.h, &mut self.camera, true);
            let reading =
                serde_json::from_value(self.run(SessionCommand::ReadSetup).unwrap()).unwrap();
            crate::setup::from_read(name, None, reading).unwrap()
        }
        fn diff(&mut self, setup: &Setup) -> Value {
            self.run(SessionCommand::SetupDiff {
                name: None,
                setup: Some(setup.clone()),
                allow_other_model: false,
            })
            .unwrap()
        }
        fn apply_with(
            &mut self,
            setup: &Setup,
            strict: bool,
            confirmed: Option<Vec<Confirmed>>,
        ) -> Result<Value> {
            let result = self.run(SessionCommand::ApplySetup {
                name: None,
                setup: Some(setup.clone()),
                digest: None,
                allow_other_model: false,
                strict,
                confirmed_disruptive: confirmed,
            });
            refresh(&self.h, &mut self.camera, true);
            result
        }
        /// Read `feature` at `selector` = `bank`, keeping the selection.
        fn at(&mut self, selector: &str, bank: &str, feature: &str) -> String {
            let c = self.c();
            let selected = c.get(selector).unwrap();
            c.set(selector, bank).unwrap();
            let value = c
                .get(feature)
                .map(|v| text(&v))
                .unwrap_or_else(|_| "unavailable".into());
            c.set(selector, selected.as_str().unwrap()).unwrap();
            value
        }
        fn set_at(&mut self, selector: &str, bank: &str, feature: &str, value: &str) {
            let c = self.c();
            let selected = c.get(selector).unwrap();
            c.set(selector, bank).unwrap();
            c.set(feature, value).unwrap();
            c.set(selector, selected.as_str().unwrap()).unwrap();
        }
        fn line(&mut self, line: &str) -> (String, String) {
            (
                self.at("LineSelector", line, "LineMode"),
                self.at("LineSelector", line, "LineSource"),
            )
        }
        fn entries(&self) -> usize {
            self.h.snapshot().history.entries.len()
        }
    }
    fn saved_at<'a>(setup: &'a Setup, feature: &str, bank: &str) -> Option<&'a Scalar> {
        setup
            .values
            .iter()
            .find(|v| v.feature == feature && v.at.iter().any(|b| b.value == bank))
            .map(|v| &v.value)
    }
    fn change<'a>(diff: &'a Value, feature: &str) -> Option<&'a Value> {
        diff["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["feature"] == feature)
    }

    #[test]
    fn an_output_line_saved_from_an_input_line_round_trips() {
        let (mut rig, _) = Rig::rich();
        rig.set_at("LineSelector", "Line1", "LineMode", "Output");
        rig.set_at("LineSelector", "Line1", "LineSource", "ExposureActive");
        assert_eq!(rig.c().get("LineSelector").unwrap(), "Line0");
        let setup = rig.save("lines");
        assert_eq!(
            saved_at(&setup, "LineSource", "Line1"),
            Some(&Scalar::String("ExposureActive".into())),
            "{:?}",
            setup.values
        );
        assert!(saved_at(&setup, "LineSource", "Line0").is_none());
        for line in ["Line0", "Line2"] {
            assert!(
                setup.skipped.iter().any(|s| s.feature == "LineSource"
                    && s.at[0].value == line
                    && s.reason == "not available"),
                "{:?}",
                setup.skipped
            );
        }
        assert!(
            !setup
                .skipped
                .iter()
                .any(|s| s.reason.starts_with("could not read"))
        );
        assert!(
            !rig.h.snapshot().logs.iter().any(|l| l.level == "warn"),
            "{:?}",
            rig.h.snapshot().logs
        );
        // The same camera with an output line selected saves the same values.
        rig.c().set("LineSelector", "Line1").unwrap();
        let other = rig.save("lines");
        let without_selection = |setup: &Setup| {
            setup
                .values
                .iter()
                .filter(|v| v.feature != "LineSelector")
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(without_selection(&other), without_selection(&setup));
        // Restored with the input line selected, in one apply.
        rig.c().set("LineSelector", "Line0").unwrap();
        rig.set_at("LineSelector", "Line1", "LineSource", "Off");
        let applied = rig.apply_with(&setup, false, None).unwrap();
        assert_eq!(applied["applied"], 1, "{applied}");
        assert_eq!(
            rig.line("Line1"),
            ("Output".into(), "ExposureActive".into())
        );
        assert_eq!(rig.c().get("LineSelector").unwrap(), "Line0");
        rig.run(SessionCommand::Undo).unwrap();
        assert_eq!(rig.line("Line1"), ("Output".into(), "Off".into()));
        rig.h.shutdown();
    }

    #[test]
    fn values_an_earlier_write_makes_available_restore_in_one_apply() {
        let (mut rig, _) = Rig::rich();
        rig.set_at("LineSelector", "Line1", "LineMode", "Output");
        rig.set_at("LineSelector", "Line1", "LineSource", "ExposureActive");
        rig.c().set("ADCBitDepth", "Bit12").unwrap();
        rig.c().set("PixelFormat", "Mono12").unwrap();
        let setup = rig.save("gated");
        rig.set_at("LineSelector", "Line1", "LineSource", "Off");
        rig.set_at("LineSelector", "Line1", "LineMode", "Input");
        rig.c().set("PixelFormat", "Mono8").unwrap();
        rig.c().set("ADCBitDepth", "Bit8").unwrap();
        let preview = rig.diff(&setup);
        assert_eq!(preview["not_applied"], json!([]), "{preview}");
        let source = change(&preview, "LineSource").unwrap();
        assert_eq!(source["deferred"], true);
        assert_eq!(source["current"], Value::Null);
        assert!(
            source["warning"]
                .as_str()
                .unwrap()
                .contains("applies only if earlier changes in this setup enable it")
        );
        let format = change(&preview, "PixelFormat").unwrap();
        assert_eq!(
            (&format["current"], &format["deferred"]),
            (&json!("Mono8"), &json!(true))
        );
        let entries = rig.entries();
        // Strict too: nothing is missing once the earlier writes land.
        let applied = rig.apply_with(&setup, true, None).unwrap();
        assert_eq!(applied["not_applied"], json!([]), "{applied}");
        assert_eq!(
            rig.line("Line1"),
            ("Output".into(), "ExposureActive".into())
        );
        assert_eq!(rig.c().get("ADCBitDepth").unwrap(), "Bit12");
        assert_eq!(rig.c().get("PixelFormat").unwrap(), "Mono12");
        assert_eq!(rig.entries(), entries + 1);
        rig.run(SessionCommand::Undo).unwrap();
        assert_eq!(rig.line("Line1"), ("Input".into(), "unavailable".into()));
        assert_eq!(rig.c().get("ADCBitDepth").unwrap(), "Bit8");
        assert_eq!(rig.c().get("PixelFormat").unwrap(), "Mono8");
        rig.run(SessionCommand::Redo).unwrap();
        assert_eq!(
            rig.line("Line1"),
            ("Output".into(), "ExposureActive".into())
        );
        assert_eq!(rig.c().get("PixelFormat").unwrap(), "Mono12");
        rig.h.shutdown();
    }

    #[test]
    fn values_that_stay_unavailable_are_reported_or_refused() {
        let (mut rig, _) = Rig::rich();
        let mut setup = rig.save("stuck");
        let line2 = vec![Bank {
            selector: "LineSelector".into(),
            value: "Line2".into(),
        }];
        // Line2 stays an input, so its source never becomes available.
        setup.values.push(SetupValue {
            feature: "LineSource".into(),
            value: Scalar::String("ExposureActive".into()),
            at: line2,
        });
        // Alone, an unavailable value is not applicable.
        let alone = rig.diff(&setup);
        assert_eq!(alone["not_applied"][0]["status"], "missing", "{alone}");
        assert_eq!(alone["changes"], json!([]));
        // Beside another change it is tried after it, then reported.
        rig.c().set("BlackLevel", "9").unwrap();
        let preview = rig.diff(&setup);
        assert_eq!(change(&preview, "LineSource").unwrap()["deferred"], true);
        let strict = rig.apply_with(&setup, true, None).unwrap_err();
        assert_eq!(
            crate::types::error_transport_code(&strict),
            Some("unsupported")
        );
        assert!(
            format!("{strict:#}").contains("every value was put back"),
            "{strict:#}"
        );
        assert_eq!(rig.c().get("BlackLevel").unwrap(), 9);
        let applied = rig.apply_with(&setup, false, None).unwrap();
        assert_eq!(rig.c().get("BlackLevel").unwrap(), 0);
        assert_eq!(applied["applied"], 1, "{applied}");
        assert_eq!(applied["not_applied"][0]["feature"], "LineSource");
        assert_eq!(
            applied["not_applied"][0]["reason"],
            "still not available after the other changes in this setup"
        );
        // A symbol or feature the camera does not have stays not applicable.
        let mut unknown = setup.clone();
        unknown.values.retain(|v| v.feature != "LineSource");
        for value in &mut unknown.values {
            if value.feature == "PixelFormat" {
                value.value = Scalar::String("Mono16".into());
            }
        }
        unknown.values.push(SetupValue {
            feature: "Scan3dMode".into(),
            value: Scalar::String("x".into()),
            at: Vec::new(),
        });
        rig.c().set("BlackLevel", "9").unwrap();
        let preview = rig.diff(&unknown);
        let statuses = preview["not_applied"]
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
            statuses,
            [("PixelFormat", "invalid"), ("Scan3dMode", "missing")]
        );
        rig.h.shutdown();
    }

    #[test]
    fn a_value_an_earlier_write_moves_is_written_again() {
        let (mut rig, _) = Rig::rich();
        // Binning 1, Width 640.
        let setup = rig.save("full");
        rig.c().set("BinningHorizontal", "2").unwrap();
        rig.c().set("Width", "640").unwrap();
        let preview = rig.diff(&setup);
        assert_eq!(preview["changes"].as_array().unwrap().len(), 1, "{preview}");
        assert!(change(&preview, "BinningHorizontal").is_some());
        // Binning 1 doubles Width to 1280; the restore writes it again.
        let applied = rig.apply_with(&setup, false, None).unwrap();
        assert_eq!(rig.c().get("BinningHorizontal").unwrap(), 1);
        assert_eq!(rig.c().get("Width").unwrap(), 640);
        assert_eq!(applied["applied"], 2, "{applied}");
        rig.run(SessionCommand::Undo).unwrap();
        assert_eq!(rig.c().get("BinningHorizontal").unwrap(), 2);
        assert_eq!(rig.c().get("Width").unwrap(), 640);
        rig.h.shutdown();
    }

    #[test]
    fn values_locked_while_acquiring_restart_the_stream() {
        let (mut rig, _) = Rig::rich();
        let setup = rig.save("shift");
        rig.c().set("DigitalShift", "2").unwrap();
        rig.run(SessionCommand::Start).unwrap();
        assert!(!rig.c().is_writable("DigitalShift"));
        let preview = rig.diff(&setup);
        assert_eq!(preview["restarts_stream"], true, "{preview}");
        assert_eq!(
            change(&preview, "DigitalShift").unwrap()["stream_locked"],
            true
        );
        let applied = rig.apply_with(&setup, false, None).unwrap();
        assert_eq!(applied["restarted_stream"], true, "{applied}");
        assert!(rig.c().is_streaming());
        assert_eq!(rig.c().get("DigitalShift").unwrap(), 0);
        rig.run(SessionCommand::Undo).unwrap();
        assert!(rig.c().is_streaming());
        assert_eq!(rig.c().get("DigitalShift").unwrap(), 2);
        rig.run(SessionCommand::Redo).unwrap();
        assert_eq!(rig.c().get("DigitalShift").unwrap(), 0);
        // A lock the setup itself releases needs no restart.
        rig.run(SessionCommand::Stop).unwrap();
        let manual = rig.save("manual");
        rig.c().set("ExposureTime", "5000").unwrap();
        rig.c().set("ExposureAuto", "Continuous").unwrap();
        rig.run(SessionCommand::Start).unwrap();
        let preview = rig.diff(&manual);
        assert_eq!(preview["restarts_stream"], false, "{preview}");
        assert_eq!(
            change(&preview, "ExposureTime").unwrap()["stream_locked"],
            false
        );
        let applied = rig.apply_with(&manual, false, None).unwrap();
        assert_eq!(applied["restarted_stream"], false);
        assert_eq!(rig.c().get("ExposureAuto").unwrap(), "Off");
        assert_eq!(rig.c().get("ExposureTime").unwrap().as_f64(), Some(10000.0));
        rig.h.shutdown();
    }

    #[test]
    fn firmware_auto_values_are_skipped_on_save_and_diff() {
        let (mut rig, _) = Rig::rich();
        rig.c().set("ExposureAuto", "Continuous").unwrap();
        let setup = rig.save("firmware");
        assert!(!setup.values.iter().any(|v| v.feature == "ExposureTime"));
        assert!(
            setup
                .skipped
                .iter()
                .any(|s| s.feature == "ExposureTime"
                    && s.reason == "set by ExposureAuto = Continuous"),
            "{:?}",
            setup.skipped
        );
        let mut by_hand = setup.clone();
        by_hand.values.push(SetupValue {
            feature: "ExposureTime".into(),
            value: Scalar::Number(serde_json::Number::from_f64(5000.0).unwrap()),
            at: Vec::new(),
        });
        let preview = rig.diff(&by_hand);
        let row = &preview["not_applied"][0];
        assert_eq!(row["feature"], "ExposureTime", "{preview}");
        assert_eq!(row["status"], "skipped");
        assert_eq!(
            row["reason"],
            "set by ExposureAuto = Continuous in this setup"
        );
        rig.h.shutdown();
    }

    #[test]
    fn unreadable_and_many_banks_are_skipped_and_selectors_choose_what_is_enumerated() {
        let (mut rig, wires) = Rig::rich();
        rig.set_at("LineSelector", "Line2", "LineMode", "Output");
        let good = rig.save("good");
        wires
            .fail_line2
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let setup = rig.save("partial");
        assert!(saved_at(&setup, "LineSource", "Line2").is_none());
        assert!(
            setup.skipped.iter().any(|s| s.feature == "LineSource"
                && s.at[0].value == "Line2"
                && s.reason == "could not read"),
            "{:?}",
            setup.skipped
        );
        // Every trigger bank is saved; a 300-bank counter only at its bank.
        for bank in ["FrameStart", "LineStart"] {
            assert!(saved_at(&setup, "TriggerSource", bank).is_some(), "{bank}");
        }
        let durations = setup
            .values
            .iter()
            .filter(|v| v.feature == "CounterDuration")
            .collect::<Vec<_>>();
        assert_eq!(durations.len(), 1);
        assert_eq!(durations[0].at[0].value, "Counter0");
        assert!(setup.skipped.iter().any(|s| s.feature == "CounterDuration"
            && s.reason == "more than 256 banks; saved at the selected bank only"));
        // Transport settings Capturefab negotiates are never saved.
        assert!(
            !setup
                .values
                .iter()
                .any(|v| v.feature.starts_with("DeviceStreamChannel"))
        );
        let mut packet = good.clone();
        packet.values.push(SetupValue {
            feature: "DeviceStreamChannelPacketSize".into(),
            value: Scalar::Number(9000.into()),
            at: Vec::new(),
        });
        assert!(
            rig.run(SessionCommand::SetupDiff {
                name: None,
                setup: Some(packet),
                allow_other_model: false,
            })
            .is_err()
        );
        // A bank that cannot be read is not applied; the rest is.
        rig.c().set("BlackLevel", "7").unwrap();
        let applied = rig.apply_with(&good, false, None).unwrap();
        assert_eq!(applied["applied"], 1, "{applied}");
        let row = &applied["not_applied"][0];
        assert_eq!(
            (&row["feature"], &row["status"]),
            (&json!("LineSource"), &json!("missing"))
        );
        assert_eq!(rig.c().get("BlackLevel").unwrap(), 0);
        rig.h.shutdown();
    }

    #[test]
    fn apply_changes_nothing_when_the_checkpoint_cannot_be_read() {
        let (mut rig, wires) = Rig::rich();
        let setup = rig.save("black");
        rig.c().set("BlackLevel", "7").unwrap();
        rig.c().set("BinningHorizontal", "2").unwrap();
        // The comparison reads BlackLevel this many times; the next read,
        // for the undo checkpoint, fails.
        let before = wires.black_reads.load(std::sync::atomic::Ordering::SeqCst);
        rig.diff(&setup);
        let plan_reads = wires.black_reads.load(std::sync::atomic::Ordering::SeqCst) - before;
        let entries = rig.entries();
        *wires.fail_black_after.lock().unwrap() =
            Some(wires.black_reads.load(std::sync::atomic::Ordering::SeqCst) + plan_reads);
        let error = rig.apply_with(&setup, false, None).unwrap_err();
        assert!(
            format!("{error:#}").contains("apply_setup changed nothing"),
            "{error:#}"
        );
        *wires.fail_black_after.lock().unwrap() = None;
        assert_eq!(rig.c().get("BlackLevel").unwrap(), 7);
        assert_eq!(rig.c().get("BinningHorizontal").unwrap(), 2);
        assert_eq!(rig.entries(), entries);
        rig.h.shutdown();
    }

    #[test]
    fn confirmations_name_the_bank_they_confirm() {
        let (mut rig, _) = Rig::rich();
        let setup = rig.save("triggers");
        let frame_start = vec![Bank {
            selector: "TriggerSelector".into(),
            value: "FrameStart".into(),
        }];
        let confirmed = vec![Confirmed::At(crate::setup::ConfirmedAt {
            feature: "TriggerMode".into(),
            at: frame_start.clone(),
        })];
        rig.set_at("TriggerSelector", "FrameStart", "TriggerMode", "On");
        // A bare name confirms only a value without banks.
        let bare = rig
            .apply_with(
                &setup,
                false,
                Some(vec![Confirmed::Name("TriggerMode".into())]),
            )
            .unwrap_err();
        assert_eq!(crate::types::error_transport_code(&bare), Some("usage"));
        assert!(
            bare.to_string()
                .contains("TriggerMode (TriggerSelector = FrameStart)"),
            "{bare}"
        );
        // Another bank changed after the preview: refused, nothing changes.
        rig.set_at("TriggerSelector", "LineStart", "TriggerMode", "On");
        let refused = rig
            .apply_with(&setup, false, Some(confirmed.clone()))
            .unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("TriggerMode (TriggerSelector = LineStart)"),
            "{refused}"
        );
        assert_eq!(rig.at("TriggerSelector", "FrameStart", "TriggerMode"), "On");
        assert_eq!(rig.at("TriggerSelector", "LineStart", "TriggerMode"), "On");
        rig.set_at("TriggerSelector", "LineStart", "TriggerMode", "Off");
        rig.apply_with(&setup, false, Some(confirmed)).unwrap();
        assert_eq!(
            rig.at("TriggerSelector", "FrameStart", "TriggerMode"),
            "Off"
        );
        rig.h.shutdown();
    }

    #[test]
    fn selecting_another_bank_is_reversible() {
        let (mut rig, _) = Rig::rich();
        let setup = rig.save("selection");
        rig.c().set("LineSelector", "Line1").unwrap();
        rig.c().set("TriggerSelector", "LineStart").unwrap();
        let preview = rig.diff(&setup);
        assert_eq!(preview["changes"].as_array().unwrap().len(), 2, "{preview}");
        assert_eq!(preview["risk"], "reversible");
        rig.apply_with(&setup, false, Some(Vec::new())).unwrap();
        assert_eq!(rig.c().get("LineSelector").unwrap(), "Line0");
        // A per-bank value still needs confirmation.
        rig.set_at("LineSelector", "Line0", "LineMode", "Output");
        let preview = rig.diff(&setup);
        assert_eq!(change(&preview, "LineMode").unwrap()["risk"], "disruptive");
        assert!(rig.apply_with(&setup, false, Some(Vec::new())).is_err());
        rig.h.shutdown();
    }

    #[test]
    fn switches_arm_after_their_sources_and_disarm_before() {
        let (mut rig, wires) = Rig::rich();
        let order = |wires: &Wires| {
            let writes = wires.writes.lock().unwrap();
            let position = |address| writes.iter().position(|w| *w == address).unwrap();
            (position(TRIGGER_SOURCE), position(TRIGGER_MODE))
        };
        let off = rig.save("off");
        rig.c().set("TriggerSource", "Line0").unwrap();
        rig.c().set("TriggerMode", "On").unwrap();
        let on = rig.save("on");
        // Arming: the source is written before the mode turns on.
        rig.apply_with(&off, false, None).unwrap();
        wires.writes.lock().unwrap().clear();
        rig.apply_with(&on, false, None).unwrap();
        let (source, mode) = order(&wires);
        assert!(source < mode, "{:?}", wires.writes.lock().unwrap());
        // Disarming: the mode turns off before the source changes.
        wires.writes.lock().unwrap().clear();
        rig.apply_with(&off, false, None).unwrap();
        let (source, mode) = order(&wires);
        assert!(mode < source, "{:?}", wires.writes.lock().unwrap());
        rig.h.shutdown();
    }

    /// More configuration values than a setup holds.
    struct Many;
    impl crate::types::RegisterIo for Many {
        fn read_memory(&mut self, _: u64, length: usize) -> Result<Vec<u8>> {
            Ok(vec![0; length])
        }
        fn write_memory(&mut self, _: u64, _: &[u8]) -> Result<()> {
            Ok(())
        }
    }
    impl crate::types::Backend for Many {
        fn xml(&mut self) -> Result<String> {
            let names = (0..MAX_SETUP_VALUES + 8).map(|i| format!("Value{i:03}"));
            Ok(format!(
                "<RegisterDescription><Category Name='Root'>{}</Category>{}</RegisterDescription>",
                names
                    .clone()
                    .map(|n| format!("<pFeature>{n}</pFeature>"))
                    .collect::<String>(),
                names
                    .map(|n| format!("<Integer Name='{n}'><Value>1</Value></Integer>"))
                    .collect::<String>()
            ))
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

    #[test]
    fn a_reading_stops_at_the_value_limit() {
        let mut rig = Rig::with(Box::new(Many));
        let reading: Reading =
            serde_json::from_value(rig.run(SessionCommand::ReadSetup).unwrap()).unwrap();
        assert_eq!(reading.values.len(), MAX_SETUP_VALUES);
        assert_eq!(
            reading
                .skipped
                .iter()
                .filter(|s| s.reason == "limit of 512 values reached")
                .count(),
            8
        );
        rig.h.shutdown();
    }
}
