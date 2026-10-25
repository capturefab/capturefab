//! Camera checkpoints live beside the owner of the hardware. Values are read
//! before and after a successful command, never inferred from a UI snapshot.
use super::*;
use crate::session::{CameraHistoryEntry, CameraHistoryStatus};
use std::collections::{BTreeMap, BTreeSet};

mod setup;

const LIMIT: usize = 200;
static LIFETIME: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Address {
    feature: String,
    selectors: Vec<(String, String)>,
}

#[derive(Clone)]
struct Configuration {
    values: BTreeMap<Address, Value>,
    auto: Option<AutoController>,
}

#[derive(Clone)]
enum Checkpoint {
    Stream {
        before: bool,
        after: bool,
    },
    Configuration {
        before: Box<Configuration>,
        after: Box<Configuration>,
        fields: Vec<Address>,
    },
}

#[derive(Clone)]
struct Change {
    token: String,
    camera: String,
    label: String,
    applied: bool,
    created_at_ms: u64,
    last_changed_at_us: u64,
    feature: Option<String>,
    kind: String,
    checkpoint: Checkpoint,
}

pub(super) struct Journal {
    generation: String,
    next: u64,
    changes: Vec<Change>,
}

impl Default for Journal {
    fn default() -> Self {
        Self {
            generation: format!(
                "{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                LIFETIME.fetch_add(1, Ordering::Relaxed)
            ),
            next: 1,
            changes: Vec::new(),
        }
    }
}

impl Journal {
    fn status(&self) -> CameraHistoryStatus {
        CameraHistoryStatus {
            undo: self
                .changes
                .iter()
                .filter(|c| c.applied)
                .max_by_key(|c| c.last_changed_at_us)
                .map(|c| c.label.clone()),
            redo: self
                .changes
                .iter()
                .filter(|c| !c.applied)
                .max_by_key(|c| c.last_changed_at_us)
                .map(|c| c.label.clone()),
            entries: self
                .changes
                .iter()
                .map(|change| CameraHistoryEntry {
                    token: change.token.clone(),
                    label: change.label.clone(),
                    applied: change.applied,
                    created_at_ms: change.created_at_ms,
                    last_changed_at_us: change.last_changed_at_us,
                    feature: change.feature.clone(),
                    kind: change.kind.clone(),
                    setting_changes: match &change.checkpoint {
                        Checkpoint::Stream { .. } => 0,
                        Checkpoint::Configuration { fields, .. } => {
                            u32::try_from(fields.len()).unwrap_or(u32::MAX).max(1)
                        }
                    },
                })
                .collect(),
        }
    }

    fn record(
        &mut self,
        camera: String,
        label: String,
        kind: String,
        feature: Option<String>,
        checkpoint: Checkpoint,
    ) -> Value {
        // A new successful edit branches the timeline. Failed commands never
        // enter here and therefore leave redo available.
        self.changes.retain(|change| change.applied);
        let token = format!("{}-{}", self.generation, self.next);
        self.next += 1;
        let event_time = now_us();
        self.changes.push(Change {
            token: token.clone(),
            camera: camera.clone(),
            label: label.clone(),
            created_at_ms: event_time / 1000,
            last_changed_at_us: event_time,
            feature,
            kind,
            applied: true,
            checkpoint,
        });
        if self.changes.len() > LIMIT {
            self.changes.remove(0);
        }
        json!({"token":token,"camera":camera,"label":label,"last_changed_at_us":event_time})
    }
}

fn text(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Number(number) if number.is_f64() => number.as_f64().unwrap().to_string(),
        value => value.to_string(),
    }
}

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(u64::MAX as u128) as u64
}

fn auto_policy(auto: &Option<AutoController>) -> Option<f64> {
    auto.as_ref().map(|controller| controller.status().balance)
}

fn evolving(auto: &Option<AutoController>, feature: &str) -> bool {
    auto.as_ref()
        .is_some_and(|controller| controller.manages(feature))
        && matches!(
            feature,
            "ExposureTime" | "ExposureTimeAbs" | "Gain" | "GainAbs" | "BalanceRatio"
        )
}

/// Temporarily address a selected bank, then restore the user's selection on
/// both success and failure. Selector switches themselves are never replayed
/// as command features.
fn addressed<T>(
    camera: &mut Camera,
    address: &Address,
    action: impl FnOnce(&mut Camera) -> Result<T>,
) -> Result<T> {
    let mut originals = Vec::new();
    let result = (|| {
        for (selector, value) in &address.selectors {
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
    ensure!(
        restoration.is_empty(),
        "could not restore selectors: {}",
        restoration.join("; ")
    );
    result
}

/// A feature addressed by more banks than a checkpoint reads.
#[derive(Debug)]
struct TooManyBanks(String);
impl std::fmt::Display for TooManyBanks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "too many selected banks for {} undo", self.0)
    }
}
impl std::error::Error for TooManyBanks {}

/// Every bank of `feature` to read: the selected one of each selector, and
/// every choice of the selectors `banks` names.
fn addresses(
    camera: &mut Camera,
    feature: &str,
    banks: &dyn Fn(&str) -> bool,
) -> Result<Vec<Address>> {
    let selectors = camera.selectors(feature);
    let mut addresses = vec![Address {
        feature: feature.into(),
        selectors: Vec::new(),
    }];
    for selector in selectors {
        let current = camera
            .get(&selector)
            .with_context(|| format!("read {selector} for undo"))?;
        let mut choices = vec![text(&current)];
        if banks(&selector) {
            for choice in camera.choices(&selector).unwrap_or_default() {
                if !choices.contains(&choice) {
                    choices.push(choice);
                }
            }
        }
        if addresses.len().saturating_mul(choices.len()) > 256 {
            return Err(TooManyBanks(feature.into()).into());
        }
        addresses = addresses
            .into_iter()
            .flat_map(|address| {
                choices
                    .iter()
                    .map(|choice| {
                        let mut address = address.clone();
                        address.selectors.push((selector.clone(), choice.clone()));
                        address
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
    }
    Ok(addresses)
}

/// Read the configuration values a command may change. The names come from
/// the last feature walk, which every edit ends with, so an edit does not
/// walk every feature again; the values are read now.
/// `all` includes every configuration value; `written` requires each named
/// feature, including controls absent from the feature tree.
fn capture(
    h: &WorkerHandle,
    camera: &mut Camera,
    auto: &Option<AutoController>,
    all: bool,
    written: &[&str],
) -> Result<Configuration> {
    let mut walked = h
        .state
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .features
        .iter()
        .filter(|feature| feature.value.is_some())
        .map(|feature| feature.name.clone())
        .collect::<Vec<_>>();
    if walked.is_empty() {
        walked = camera
            .features()
            .into_iter()
            .filter(|feature| feature.value.is_some())
            .map(|feature| feature.name)
            .collect();
    }
    let mut names = walked
        .into_iter()
        .filter(|name| camera.is_configuration_value(name))
        .collect::<BTreeSet<_>>();
    // Explicit writes must be restorable; stale cached names may be skipped
    // when they are no longer readable, just as a fresh walk would skip them.
    let mut required = written
        .iter()
        .map(|&name| name.to_owned())
        .collect::<BTreeSet<_>>();
    if all {
        if let Some(controller) = auto {
            required.extend(controller.status().managed.iter().cloned());
        }
        // Firmware may lock the numeric features while auto is running.
        // They still have readable originals and must be included when a
        // manual write stops the firmware controller.
        for name in [
            "ExposureTime",
            "ExposureTimeAbs",
            "Gain",
            "GainAbs",
            "ExposureAuto",
            "GainAuto",
            "BalanceWhiteAuto",
            "BalanceRatio",
            "BalanceRatioSelector",
            "GainSelector",
        ] {
            if camera.has(name) && camera.get(name).is_ok() {
                required.insert(name.to_owned());
            }
        }
    }
    names.extend(required.iter().cloned());
    let mut values = BTreeMap::new();
    for name in names {
        // Auto can change every gain/white-balance bank. Other selectors stay
        // at their current bank, avoiding intrusive enumeration of unrelated
        // device settings just to checkpoint one control.
        let all_banks = all && matches!(name.as_str(), "Gain" | "GainAbs" | "BalanceRatio");
        let read = (|| -> Result<Vec<(Address, Value)>> {
            let mut read = Vec::new();
            for address in addresses(camera, &name, &|_| all_banks)? {
                let value = addressed(camera, &address, |camera| camera.get(&name))
                    .with_context(|| format!("read {name} for undo"))?;
                read.push((address, value));
            }
            Ok(read)
        })();
        match read {
            Ok(read) => values.extend(read),
            // Reading the current bank switches no selector, so its failure
            // leaves the camera as it was.
            Err(_) if !all_banks && !required.contains(&name) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(Configuration {
        values,
        auto: auto.clone(),
    })
}

/// Add what a write of `feature` is about to replace, at the banks selected
/// now, to the checkpoint taken before a SetMany: an earlier value of the
/// same SetMany may have switched a selector to a bank it did not read.
fn extend(camera: &mut Camera, before: &mut Configuration, feature: &str) -> Result<()> {
    for address in addresses(camera, feature, &|_| false)? {
        if let std::collections::btree_map::Entry::Vacant(entry) = before.values.entry(address) {
            entry.insert(
                camera
                    .get(feature)
                    .with_context(|| format!("read {feature} for undo"))?,
            );
        }
    }
    Ok(())
}

fn capture_at(
    camera: &mut Camera,
    auto: &Option<AutoController>,
    before: &Configuration,
) -> Result<Configuration> {
    let mut values = BTreeMap::new();
    for address in before.values.keys() {
        values.insert(
            address.clone(),
            addressed(camera, address, |camera| camera.get(&address.feature))
                .with_context(|| format!("read {} after change for undo", address.feature))?,
        );
    }
    Ok(Configuration {
        values,
        auto: auto.clone(),
    })
}

fn affected(before: &Configuration, after: &Configuration) -> Vec<Address> {
    let mode_changed = auto_policy(&before.auto) != auto_policy(&after.auto);
    before
        .values
        .keys()
        .filter(|address| {
            before.values.get(*address) != after.values.get(*address)
                || mode_changed
                    && [&before.auto, &after.auto]
                        .into_iter()
                        .flatten()
                        .any(|auto| auto.manages(&address.feature))
        })
        .cloned()
        .collect()
}

fn set_exact(camera: &mut Camera, address: &Address, target: &Value) -> Result<()> {
    addressed(camera, address, |camera| {
        if camera.get(&address.feature)? != *target {
            if address.feature == "VideoMode" && camera.is_streaming() {
                camera.stop()?;
                let result = camera.set(&address.feature, &text(target));
                let restart = camera.start();
                result?;
                restart?;
            } else {
                camera.set(&address.feature, &text(target))?;
            }
        }
        ensure!(
            camera.get(&address.feature)? == *target,
            "{} did not accept the recorded value exactly",
            address.feature
        );
        Ok(())
    })
}

fn restore(
    camera: &mut Camera,
    auto: &mut Option<AutoController>,
    target: &Configuration,
    fields: &[Address],
    switch_auto: bool,
) -> Result<()> {
    if switch_auto {
        crate::auto::hold(camera)?;
    }
    let mut pending = fields
        .iter()
        .filter(|address| target.values.contains_key(*address))
        .cloned()
        .collect::<Vec<_>>();
    // Auto flags go last, selectors after the values in their banks. Retry
    // dependency-locked fields after limits and enabling controls settle.
    pending.sort_by_key(|address| {
        (
            matches!(
                address.feature.as_str(),
                "ExposureAuto" | "GainAuto" | "BalanceWhiteAuto"
            ),
            address.feature.ends_with("Selector"),
        )
    });
    let mut errors = Vec::new();
    for _ in 0..fields.len().saturating_add(1).clamp(3, 32) {
        errors.clear();
        let mut failures = BTreeMap::new();
        for address in &pending {
            if let Err(error) = set_exact(camera, address, &target.values[address]) {
                failures.insert(address.clone(), format!("{error:#}"));
            }
        }
        // A later write may clamp an earlier field. Verify the final state,
        // rather than treating each successful write as permanently settled.
        pending = fields
            .iter()
            .filter(|address| target.values.contains_key(*address))
            .filter(|address| {
                if evolving(&target.auto, &address.feature) {
                    return false;
                }
                match addressed(camera, address, |camera| camera.get(&address.feature)) {
                    Ok(current) if current == target.values[*address] => false,
                    Ok(_) => {
                        errors.push(format!(
                            "{}: {}",
                            address.feature,
                            failures
                                .get(*address)
                                .map(String::as_str)
                                .unwrap_or("did not retain its recorded value")
                        ));
                        true
                    }
                    Err(error) => {
                        errors.push(format!("{}: {error:#}", address.feature));
                        true
                    }
                }
            })
            .cloned()
            .collect();
        if pending.is_empty() {
            break;
        }
    }
    ensure!(
        pending.is_empty(),
        "could not restore camera values: {}",
        errors.join("; ")
    );
    *auto = target.auto.clone();
    if let Some(controller) = auto.as_mut() {
        controller.invalidate();
    }
    Ok(())
}

/// Publish the camera after a change. `features` walks every feature again;
/// a command that ended with its own walk, or only started or stopped the
/// stream, passes false.
fn publish(
    h: &WorkerHandle,
    camera: &mut Option<Camera>,
    auto: &Option<AutoController>,
    features: bool,
) {
    refresh(h, camera, features);
    if let Some(controller) = auto {
        publish_auto(h, controller);
    } else {
        h.state.write().unwrap_or_else(|e| e.into_inner()).auto = None;
    }
    let status = h.history.lock().unwrap_or_else(|e| e.into_inner()).status();
    h.state.write().unwrap_or_else(|e| e.into_inner()).history = status;
}

fn replay(
    h: &WorkerHandle,
    camera: &mut Option<Camera>,
    auto: &mut Option<AutoController>,
    token: Option<&str>,
    undo: bool,
    busy: bool,
) -> Result<Value> {
    let change = {
        let journal = h.history.lock().unwrap_or_else(|e| e.into_inner());
        match token {
            Some(token) => journal
                .changes
                .iter()
                .find(|change| change.token == token)
                .context("camera history checkpoint expired or belongs to a previous connection")?,
            None if undo => journal
                .changes
                .iter()
                .filter(|change| change.applied)
                .max_by_key(|change| change.last_changed_at_us)
                .context("nothing to undo on this camera")?,
            None => journal
                .changes
                .iter()
                .filter(|change| !change.applied)
                .max_by_key(|change| change.last_changed_at_us)
                .context("nothing to redo on this camera")?,
        }
        .clone()
    };
    ensure!(
        change.applied == undo,
        "this camera change was already {}",
        if undo { "undone" } else { "redone" }
    );
    let c = camera_mut(camera)?;
    ensure!(
        c.info.id == change.camera,
        "camera history belongs to another camera"
    );
    match &change.checkpoint {
        Checkpoint::Stream { before, after } => {
            ensure!(
                !busy,
                "cannot replay stream history while capture jobs or forwarding are active"
            );
            let (expected, target) = if undo {
                (after, before)
            } else {
                (before, after)
            };
            ensure!(
                c.is_streaming() == *expected,
                "stream state changed outside this history; undo/redo would overwrite it"
            );
            if *target {
                c.start()?;
            } else {
                c.stop()?;
            }
        }
        Checkpoint::Configuration {
            before,
            after,
            fields,
        } => {
            let (expected, target) = if undo {
                (after, before)
            } else {
                (before, after)
            };
            ensure!(
                auto_policy(auto) == auto_policy(&expected.auto),
                "auto mode changed outside this history; undo/redo would overwrite it"
            );
            if let Some(locked) = fields
                .iter()
                .find(|field| crate::genicam::stream_locked(&field.feature))
            {
                ensure!(
                    !busy,
                    "cannot restore {} while recording, forwarding or scheduled captures are active",
                    locked.feature
                );
            }
            for address in fields {
                // Software/firmware auto values evolve between commands. The
                // Exposure/gain/white balance evolve in firmware. For other
                // owned settings compare against the live controller's last
                // write, so a legitimate replan is allowed but an external
                // raw write to an auto flag or limit is a conflict.
                if expected
                    .auto
                    .as_ref()
                    .is_some_and(|auto| auto.manages(&address.feature))
                {
                    if !matches!(
                        address.feature.as_str(),
                        "ExposureTime" | "ExposureTimeAbs" | "Gain" | "GainAbs" | "BalanceRatio"
                    ) {
                        let current = addressed(c, address, |camera| camera.get(&address.feature))?;
                        let tracked = auto
                            .as_ref()
                            .and_then(|controller| controller.values().get(&address.feature));
                        ensure!(
                            tracked.or_else(|| expected.values.get(address)) == Some(&current),
                            "{} changed outside this history; undo/redo would overwrite it",
                            address.feature
                        );
                    }
                    continue;
                }
                let current = addressed(c, address, |camera| camera.get(&address.feature))?;
                ensure!(
                    expected.values.get(address) == Some(&current),
                    "{} changed outside this history; undo/redo would overwrite it",
                    address.feature
                );
            }
            let rollback = capture_at(c, auto, expected)?;
            let mut wanted = rollback.clone();
            wanted.auto = target.auto.clone();
            for address in fields {
                wanted
                    .values
                    .insert(address.clone(), target.values[address].clone());
            }
            let switch_auto = auto_policy(&expected.auto) != auto_policy(&target.auto);
            // Restore all readable configuration values to a merge of the
            // current unrelated settings and the recorded affected values.
            // This also repairs new coupled side effects caused by replay.
            let all_fields = wanted.values.keys().cloned().collect::<Vec<_>>();
            // A streaming camera locks its geometry: stop it once around the
            // writes, as restoring a setup did, and start it again after.
            let restart = c.is_streaming()
                && fields
                    .iter()
                    .any(|field| crate::genicam::stream_locked(&field.feature));
            if restart {
                c.stop()?;
            }
            let restored = restore(c, auto, &wanted, &all_fields, switch_auto);
            if let Err(error) = restored {
                let rollback_result = restore(c, auto, &rollback, &all_fields, switch_auto);
                if restart {
                    let _ = c.start();
                }
                publish(h, camera, auto, true);
                return match rollback_result {
                    Ok(()) => {
                        Err(error.context("undo/redo failed; previous camera state restored"))
                    }
                    Err(rollback_error) => Err(error.context(format!(
                        "undo/redo failed and rollback failed: {rollback_error:#}"
                    ))),
                };
            }
            if restart {
                c.start()?;
            }
        }
    }
    let status = {
        let mut journal = h.history.lock().unwrap_or_else(|e| e.into_inner());
        let entry = journal
            .changes
            .iter_mut()
            .find(|entry| entry.token == change.token)
            .unwrap();
        entry.applied = !undo;
        entry.last_changed_at_us = now_us();
        journal.status()
    };
    publish(h, camera, auto, true);
    h.log(
        "info",
        format!("{} {}", if undo { "Undid" } else { "Redid" }, change.label),
    );
    let snapshot = h.snapshot();
    Ok(
        json!({(if undo {"undone"} else {"redone"}):change.label,"history":status,
        "camera":change.camera,"features":snapshot.features,"auto":snapshot.auto,"streaming":snapshot.streaming}),
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn process(
    h: &WorkerHandle,
    camera: &mut Option<Camera>,
    forwarder: &mut Option<(Forwarder, f64)>,
    auto: &mut Option<AutoController>,
    jobs: &mut Vec<Scheduled>,
    next_job: &mut u64,
    scheduled_stream: &mut bool,
    command: SessionCommand,
) -> Result<Value> {
    command.validate()?;
    if let SessionCommand::Set { feature, value } = &command {
        camera_mut(camera)?.validate_set(feature, value)?;
    }
    let busy = forwarder.is_some() || jobs.iter().any(|job| job.info.active());
    let scanner = camera.as_ref().is_some_and(Camera::is_scanner);
    match &command {
        SessionCommand::Undo => return replay(h, camera, auto, None, true, busy),
        SessionCommand::Redo => return replay(h, camera, auto, None, false, busy),
        SessionCommand::ReplayCameraChange { token, undo } => {
            return replay(h, camera, auto, Some(token), *undo, busy);
        }
        // Reading and comparing switch selectors only to read, and restore them.
        SessionCommand::ReadSetup => return setup::read(h, camera_mut(camera)?, auto),
        SessionCommand::SetupDiff {
            setup: Some(document),
            allow_other_model,
            ..
        } => {
            let c = camera_mut(camera)?;
            let plan = setup::plan(c, auto, busy, document, *allow_other_model)?;
            return Ok(setup::report(&plan, &c.info));
        }
        SessionCommand::SetupDiff { .. } | SessionCommand::ApplySetup { setup: None, .. } => {
            anyhow::bail!("setup names are resolved by the session coordinator")
        }
        _ => {}
    }
    let eligible =
        matches!(
            command,
            SessionCommand::Set { .. }
                | SessionCommand::SetMany { .. }
                | SessionCommand::Auto { .. }
                | SessionCommand::Manual { .. }
                | SessionCommand::ApplySetup { .. }
        ) || !busy && !scanner && matches!(command, SessionCommand::Start | SessionCommand::Stop);
    if !eligible {
        let boundary = matches!(
            command,
            SessionCommand::Connect { .. } | SessionCommand::Disconnect
        );
        let unavailable = match &command {
            SessionCommand::Start | SessionCommand::Stop if scanner => Some(
                "Scanner acquisition may move or consume a document; it cannot be undone or replayed safely.",
            ),
            SessionCommand::Execute { .. } => {
                Some("Camera commands may have irreversible effects and cannot be replayed safely.")
            }
            SessionCommand::WriteMemory { .. } => Some(
                "Raw memory writes have device-specific side effects and cannot be replayed safely.",
            ),
            SessionCommand::Stop if busy => Some(
                "Stopping also closes forwarding or cancels capture jobs; those effects cannot be undone.",
            ),
            _ => None,
        };
        let mut result = process_plain(
            h,
            camera,
            forwarder,
            auto,
            jobs,
            next_job,
            scheduled_stream,
            command,
        );
        if boundary && result.is_ok() {
            *h.history.lock().unwrap_or_else(|e| e.into_inner()) = Journal::default();
            h.state.write().unwrap_or_else(|e| e.into_inner()).history =
                CameraHistoryStatus::default();
        }
        if let (Ok(value), Some(reason)) = (&mut result, unavailable) {
            value["history_unavailable"] = json!(reason);
        }
        return result;
    }
    let c = camera_mut(camera)?;
    let id = c.info.id.clone();
    let stream = matches!(command, SessionCommand::Start | SessionCommand::Stop);
    let before_stream = c.is_streaming();
    // Only a plain Set names the one value it changes. Otherwise every
    // configuration value is captured, so a failed SetMany restores all of
    // them, and the written features are added even when unlisted.
    // A setup is compared again here, by the camera's owner, right before
    // it is restored; only the values it changes are required readable.
    let setup_plan = match &command {
        SessionCommand::ApplySetup {
            setup: Some(document),
            allow_other_model,
            strict,
            ..
        } => {
            let plan = setup::plan(c, auto, busy, document, *allow_other_model)?;
            plan.check(*strict)?;
            Some(plan)
        }
        _ => None,
    };
    let (all, written) = match &command {
        SessionCommand::ApplySetup { .. } => (
            true,
            setup_plan
                .iter()
                .flat_map(|plan| plan.changes().map(|row| row.address.feature.as_str()))
                .collect(),
        ),
        SessionCommand::Set { feature, .. } => (auto.is_some(), vec![feature.as_str()]),
        SessionCommand::SetMany { values } => (
            true,
            values.iter().map(|value| value.feature.as_str()).collect(),
        ),
        _ => (true, Vec::new()),
    };
    let set_many = matches!(command, SessionCommand::SetMany { .. });
    let apply_setup = matches!(command, SessionCommand::ApplySetup { .. });
    let mut before = if stream {
        None
    } else {
        match capture(h, c, auto, all, &written) {
            Ok(before) => Some(before),
            // Its values must be restorable together, so it writes none.
            Err(error) if set_many || apply_setup => {
                return Err(error.context(format!(
                    "{} changed nothing: the values it would replace could not be read",
                    if apply_setup {
                        "apply_setup"
                    } else {
                        "set_many"
                    }
                )));
            }
            Err(error) => {
                // A write-only control is still useful. Its previous state
                // cannot be reconstructed, so report that accurately instead
                // of inventing an undo value or preventing the write.
                let mut result = process_plain(
                    h,
                    camera,
                    forwarder,
                    auto,
                    jobs,
                    next_job,
                    scheduled_stream,
                    command,
                )?;
                result["history_unavailable"] =
                    json!(format!("Undo is unavailable for this change: {error:#}"));
                return Ok(result);
            }
        }
    };
    let label = match &command {
        SessionCommand::Set { feature, .. } => format!("Set {feature}"),
        SessionCommand::SetMany { values } => {
            let mut names = Vec::new();
            for value in values {
                if !names.contains(&value.feature.as_str()) {
                    names.push(value.feature.as_str());
                }
            }
            if names.len() <= 3 {
                format!("Set {}", names.join(", "))
            } else {
                format!("Set {} features", names.len())
            }
        }
        SessionCommand::Auto { .. } if auto.is_some() => "Change auto balance".into(),
        SessionCommand::Auto { .. } => "Enable auto mode".into(),
        SessionCommand::Manual { .. } => "Switch to manual mode".into(),
        SessionCommand::Start => "Start stream".into(),
        SessionCommand::Stop => "Stop stream".into(),
        SessionCommand::ApplySetup {
            setup: Some(document),
            ..
        } => format!("Restore setup {}", document.name),
        _ => unreachable!(),
    };
    let mut restart = false;
    let result = match (&command, before.as_mut()) {
        // Each write first records the bank it is about to change.
        (SessionCommand::SetMany { values }, Some(before)) => {
            super::set_many(h, camera, auto, values, |camera, feature| {
                extend(camera, before, feature)
            })
        }
        // A streaming camera locks its geometry: stop it once, write, and
        // start it again after success or the rollback below.
        (
            SessionCommand::ApplySetup {
                setup: Some(document),
                ..
            },
            Some(before),
        ) => {
            let plan = setup_plan.as_ref().expect("planned above");
            restart = plan.restarts_stream;
            let stopped = if restart {
                camera_mut(camera).and_then(Camera::stop)
            } else {
                Ok(())
            };
            stopped
                .and_then(|()| {
                    setup::apply(
                        h,
                        camera,
                        forwarder,
                        auto,
                        jobs,
                        document,
                        plan,
                        |camera, address| setup::extend_at(camera, before, address),
                    )
                })
                .and_then(|mut result| {
                    if restart {
                        camera_mut(camera)?.start()?;
                        refresh(h, camera, false);
                        result["restarted_stream"] = json!(true);
                    }
                    Ok(result)
                })
        }
        _ => process_plain(
            h,
            camera,
            forwarder,
            auto,
            jobs,
            next_job,
            scheduled_stream,
            command.clone(),
        ),
    };
    let mut result = match result {
        Ok(result) => result,
        Err(error) => {
            if let Some(before) = &before {
                let fields = before.values.keys().cloned().collect::<Vec<_>>();
                let switch_auto = auto_policy(&before.auto) != auto_policy(auto);
                let rollback = restore(camera_mut(camera)?, auto, before, &fields, switch_auto);
                if restart {
                    let _ = camera_mut(camera).and_then(Camera::start);
                }
                publish(h, camera, auto, true);
                if let Err(rollback_error) = rollback {
                    return Err(
                        error.context(format!("failed command rollback: {rollback_error:#}"))
                    );
                }
            }
            return Err(error);
        }
    };
    let checkpoint = if stream {
        let after = camera_mut(camera)?.is_streaming();
        (before_stream != after).then_some(Checkpoint::Stream {
            before: before_stream,
            after,
        })
    } else {
        let before = before.unwrap();
        let after = match capture_at(camera_mut(camera)?, auto, &before) {
            Ok(after) => after,
            Err(error) => {
                let fields = before.values.keys().cloned().collect::<Vec<_>>();
                let switch_auto = auto_policy(&before.auto) != auto_policy(auto);
                let rollback = restore(camera_mut(camera)?, auto, &before, &fields, switch_auto);
                publish(h, camera, auto, true);
                return match rollback {
                    Ok(()) => Err(error.context("change could not be acknowledged accurately; previous camera state restored")),
                    Err(rollback_error) => Err(error.context(format!("checkpoint read failed and rollback failed: {rollback_error:#}"))),
                };
            }
        };
        let fields = affected(&before, &after);
        (!fields.is_empty() || auto_policy(&before.auto) != auto_policy(&after.auto)).then_some(
            Checkpoint::Configuration {
                before: Box::new(before),
                after: Box::new(after),
                fields,
            },
        )
    };
    if let Some(checkpoint) = checkpoint {
        let (kind, feature) = match &command {
            SessionCommand::Set { feature, .. } => ("set", Some(feature.clone())),
            SessionCommand::SetMany { .. } => ("set_many", None),
            SessionCommand::Auto { .. } if label == "Change auto balance" => ("balance", None),
            SessionCommand::Auto { .. } => ("auto", None),
            SessionCommand::Manual { .. } => ("manual", None),
            SessionCommand::Start => ("start", None),
            SessionCommand::Stop => ("stop", None),
            SessionCommand::ApplySetup { .. } => ("apply_setup", None),
            _ => unreachable!(),
        };
        result["camera_change"] = h.history.lock().unwrap_or_else(|e| e.into_inner()).record(
            id,
            label,
            kind.into(),
            feature,
            checkpoint,
        );
        // The command already walked the features, or only moved the stream,
        // and the checkpoint since only read values.
        publish(h, camera, auto, false);
        result["history"] =
            serde_json::to_value(&h.state.read().unwrap_or_else(|e| e.into_inner()).history)?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connected() -> WorkerHandle {
        let h = WorkerHandle::new();
        h.request(SessionCommand::Connect {
            camera: "sim:0".into(),
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
    fn set(h: &WorkerHandle, feature: &str, value: &str) -> Value {
        h.request(SessionCommand::Set {
            feature: feature.into(),
            value: value.into(),
        })
        .unwrap()
    }

    fn set_many(h: &WorkerHandle, values: &[(&str, &str)]) -> Result<Value> {
        h.request(SessionCommand::SetMany {
            values: values
                .iter()
                .map(|(feature, value)| crate::session::FeatureValue {
                    feature: (*feature).into(),
                    value: (*value).into(),
                })
                .collect(),
        })
    }

    #[test]
    fn set_many_fails_restores_and_undoes_as_one_change() {
        let h = connected();
        let (exposure, width) = (get(&h, "ExposureTime"), get(&h, "Width"));
        let entries = h.snapshot().history.entries.len();
        // The second value is out of range after the first was written.
        assert!(set_many(&h, &[("ExposureTime", "5000"), ("Width", "99999")]).is_err());
        assert_eq!(get(&h, "ExposureTime"), exposure);
        assert_eq!(get(&h, "Width"), width);
        assert_eq!(h.snapshot().history.entries.len(), entries);
        let changed = set_many(&h, &[("ExposureTime", "5000"), ("Width", "256")]).unwrap();
        assert_eq!(changed["values"][0]["name"], "ExposureTime");
        assert_eq!(changed["values"][0]["value"].as_f64(), Some(5000.0));
        assert_eq!(changed["values"][1], json!({"name":"Width","value":256}));
        assert_eq!(changed["camera_change"]["label"], "Set ExposureTime, Width");
        let token = changed["camera_change"]["token"]
            .as_str()
            .unwrap()
            .to_owned();
        let history = h.snapshot().history;
        assert_eq!(history.entries.len(), entries + 1);
        let entry = history.entries.last().unwrap();
        assert_eq!(entry.setting_changes, 2);
        assert_eq!(
            (entry.kind.as_str(), entry.feature.as_deref()),
            ("set_many", None)
        );
        let replay = |undo| {
            h.request(SessionCommand::ReplayCameraChange {
                token: token.clone(),
                undo,
            })
            .unwrap()
        };
        replay(true);
        assert_eq!(get(&h, "ExposureTime"), exposure);
        assert_eq!(get(&h, "Width"), width);
        replay(false);
        assert_eq!(get(&h, "ExposureTime").as_f64(), Some(5000.0));
        assert_eq!(get(&h, "Width"), 256);
        // Many features are counted rather than listed.
        let many = set_many(
            &h,
            &[
                ("Width", "128"),
                ("Height", "128"),
                ("Gain", "1"),
                ("ExposureTime", "6000"),
            ],
        )
        .unwrap();
        assert_eq!(many["camera_change"]["label"], "Set 4 features");
        h.shutdown();
    }

    #[test]
    fn acknowledged_values_no_ops_failed_writes_and_redo_branches() {
        let h = connected();
        let initial = get(&h, "Width");
        let changed = set(&h, "Width", "128");
        assert!(changed["camera_change"]["token"].is_string());
        assert_eq!(
            changed["camera_change"]["last_changed_at_us"],
            h.snapshot().history.entries[0].last_changed_at_us
        );
        assert_eq!(h.snapshot().history.undo.as_deref(), Some("Set Width"));
        assert!(set(&h, "Width", "128").get("camera_change").is_none());
        h.request(SessionCommand::Undo).unwrap();
        assert_eq!(get(&h, "Width"), initial);
        assert!(
            h.request(SessionCommand::Set {
                feature: "Width".into(),
                value: "0".into()
            })
            .is_err()
        );
        assert_eq!(h.snapshot().history.redo.as_deref(), Some("Set Width"));
        h.request(SessionCommand::Redo).unwrap();
        assert_eq!(get(&h, "Width"), 128);
        h.request(SessionCommand::Undo).unwrap();
        set(&h, "Height", "128");
        assert!(h.snapshot().history.redo.is_none());
        assert!(h.request(SessionCommand::Redo).is_err());
        h.shutdown();
    }

    #[test]
    fn external_raw_write_conflicts_without_overwriting_or_consuming_history() {
        let h = connected();
        set(&h, "Width", "128");
        h.request(SessionCommand::WriteMemory {
            address: 0x100,
            data: 256u32.to_le_bytes().to_vec(),
        })
        .unwrap();
        let error = h.request(SessionCommand::Undo).unwrap_err();
        assert!(format!("{error:#}").contains("changed outside this history"));
        assert_eq!(get(&h, "Width"), 256);
        assert_eq!(h.snapshot().history.undo.as_deref(), Some("Set Width"));
        h.request(SessionCommand::WriteMemory {
            address: 0x100,
            data: 128u32.to_le_bytes().to_vec(),
        })
        .unwrap();
        h.request(SessionCommand::Undo).unwrap();
        assert_eq!(get(&h, "Width"), 640);
        h.shutdown();
    }

    #[test]
    fn auto_enable_balance_and_manual_feature_writes_restore_the_whole_mode() {
        let h = connected();
        set(&h, "ExposureTime", "20000");
        let originals = [
            get(&h, "ExposureTime"),
            get(&h, "Gain"),
            get(&h, "AcquisitionFrameRate"),
        ];
        h.request(SessionCommand::Auto { balance: Some(1.0) })
            .unwrap();
        let first_auto = [
            get(&h, "ExposureTime"),
            get(&h, "Gain"),
            get(&h, "AcquisitionFrameRate"),
        ];
        h.request(SessionCommand::Auto { balance: Some(0.0) })
            .unwrap();
        h.request(SessionCommand::Undo).unwrap();
        assert_eq!(h.snapshot().auto.unwrap().balance, 1.0);
        assert_eq!(
            [
                get(&h, "ExposureTime"),
                get(&h, "Gain"),
                get(&h, "AcquisitionFrameRate")
            ],
            first_auto
        );
        h.request(SessionCommand::Undo).unwrap();
        assert!(h.snapshot().auto.is_none());
        assert_eq!(
            [
                get(&h, "ExposureTime"),
                get(&h, "Gain"),
                get(&h, "AcquisitionFrameRate")
            ],
            originals
        );
        h.request(SessionCommand::Redo).unwrap();
        set(&h, "ExposureTime", "5000");
        assert!(h.snapshot().auto.is_none());
        h.request(SessionCommand::Undo).unwrap();
        assert_eq!(h.snapshot().auto.unwrap().balance, 1.0);
        assert_eq!(
            [
                get(&h, "ExposureTime"),
                get(&h, "Gain"),
                get(&h, "AcquisitionFrameRate")
            ],
            first_auto
        );
        h.request(SessionCommand::Redo).unwrap();
        assert!(h.snapshot().auto.is_none());
        assert_eq!(get(&h, "ExposureTime"), 5000.0);
        h.shutdown();
    }

    #[test]
    fn failed_managed_write_does_not_leave_auto_disabled() {
        let h = connected();
        h.request(SessionCommand::Auto { balance: Some(0.3) })
            .unwrap();
        let exposure = get(&h, "ExposureTime");
        assert!(
            h.request(SessionCommand::Set {
                feature: "ExposureTime".into(),
                value: "0".into()
            })
            .is_err()
        );
        assert_eq!(h.snapshot().auto.unwrap().balance, 0.3);
        assert_eq!(get(&h, "ExposureTime"), exposure);
        assert_eq!(
            h.snapshot().history.undo.as_deref(),
            Some("Enable auto mode")
        );
        h.shutdown();
    }

    #[test]
    fn stream_changes_are_reversible_but_stopping_jobs_is_not() {
        let h = connected();
        h.request(SessionCommand::Start).unwrap();
        assert!(h.snapshot().streaming);
        h.request(SessionCommand::Undo).unwrap();
        assert!(!h.snapshot().streaming);
        h.request(SessionCommand::Redo).unwrap();
        assert!(h.snapshot().streaming);
        h.request(SessionCommand::Schedule {
            output: "history-never-written.png".into(),
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
        let stopped = h.request(SessionCommand::Stop).unwrap();
        assert!(stopped.get("camera_change").is_none());
        assert!(
            stopped["history_unavailable"]
                .as_str()
                .unwrap()
                .contains("capture jobs")
        );
        assert!(h.request(SessionCommand::Undo).is_err());
        h.shutdown();
    }

    #[test]
    fn checkpoints_cannot_cross_connection_lifetimes() {
        let h = connected();
        let token = set(&h, "Width", "128")["camera_change"]["token"]
            .as_str()
            .unwrap()
            .to_owned();
        h.request(SessionCommand::Disconnect).unwrap();
        h.request(SessionCommand::Connect {
            camera: "sim:0".into(),
            timeout_ms: 100,
        })
        .unwrap();
        assert!(
            h.request(SessionCommand::ReplayCameraChange { token, undo: true })
                .is_err()
        );
        assert_eq!(get(&h, "Width"), 640);
        assert_eq!(h.snapshot().history, CameraHistoryStatus::default());
        h.shutdown();
    }

    #[test]
    fn targeted_replays_preserve_unrelated_changes_and_publish_actual_transition_order() {
        let h = connected();
        let a = set(&h, "Width", "128")["camera_change"]["token"]
            .as_str()
            .unwrap()
            .to_owned();
        let b = set(&h, "Height", "128")["camera_change"]["token"]
            .as_str()
            .unwrap()
            .to_owned();
        let before = h.snapshot().history.entries;
        h.request(SessionCommand::ReplayCameraChange {
            token: a.clone(),
            undo: true,
        })
        .unwrap();
        assert_eq!(get(&h, "Height"), 128);
        h.request(SessionCommand::ReplayCameraChange {
            token: b,
            undo: true,
        })
        .unwrap();
        assert_eq!(h.snapshot().history.redo.as_deref(), Some("Set Height"));
        h.request(SessionCommand::Redo).unwrap();
        h.request(SessionCommand::Redo).unwrap();
        assert_eq!(h.snapshot().history.undo.as_deref(), Some("Set Width"));
        let after = h.snapshot().history.entries;
        assert!(
            after
                .iter()
                .zip(before)
                .all(|(after, before)| after.last_changed_at_us > before.last_changed_at_us)
        );
        h.request(SessionCommand::Undo).unwrap();
        assert_eq!(get(&h, "Width"), 640);
        assert_eq!(get(&h, "Height"), 128);
        h.shutdown();
    }

    struct Banks;
    impl crate::types::RegisterIo for Banks {
        fn read_memory(&mut self, _: u64, length: usize) -> Result<Vec<u8>> {
            Ok(vec![0; length])
        }
        fn write_memory(&mut self, _: u64, _: &[u8]) -> Result<()> {
            Ok(())
        }
    }
    impl crate::types::Backend for Banks {
        fn xml(&mut self) -> Result<String> {
            Ok(concat!("<RegisterDescription><Category Name='Root'><pFeature>BankSelector</pFeature><pFeature>Setting</pFeature></Category>",
                "<Enumeration Name='BankSelector'><EnumEntry Name='A'><Value>0</Value></EnumEntry><EnumEntry Name='B'><Value>1</Value></EnumEntry><Value>0</Value><pSelected>Setting</pSelected></Enumeration>",
                "<Integer Name='Setting'><pIndex>BankSelector</pIndex><pValueIndexed Index='0'>AValue</pValueIndexed><pValueIndexed Index='1'>BValue</pValueIndexed></Integer>",
                "<Integer Name='AValue'><Value>10</Value></Integer><Integer Name='BValue'><Value>11</Value></Integer></RegisterDescription>").into())
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

    struct Firmware;
    impl crate::types::RegisterIo for Firmware {
        fn read_memory(&mut self, _: u64, length: usize) -> Result<Vec<u8>> {
            Ok(vec![0; length])
        }
        fn write_memory(&mut self, _: u64, _: &[u8]) -> Result<()> {
            Ok(())
        }
    }
    impl crate::types::Backend for Firmware {
        fn xml(&mut self) -> Result<String> {
            Ok(concat!("<RegisterDescription><Category Name='Root'><pFeature>ExposureTime</pFeature><pFeature>ExposureAuto</pFeature><pFeature>AutoExposureTimeLowerLimit</pFeature><pFeature>AutoExposureTimeUpperLimit</pFeature></Category>",
                "<Float Name='ExposureTime'><Value>2000</Value><Min>10</Min><Max>1000000</Max></Float>",
                "<Enumeration Name='ExposureAuto'><EnumEntry Name='Off'><Value>0</Value></EnumEntry><EnumEntry Name='Continuous'><Value>1</Value></EnumEntry><Value>0</Value></Enumeration>",
                "<Float Name='AutoExposureTimeLowerLimit'><Value>10</Value><Min>10</Min><Max>1000000</Max></Float>",
                "<Float Name='AutoExposureTimeUpperLimit'><Value>1000000</Value><Min>10</Min><Max>1000000</Max></Float></RegisterDescription>").into())
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
    fn external_firmware_control_changes_conflict_with_auto_history() {
        let h = WorkerHandle::new();
        let mut camera = Some(
            Camera::from_backend(crate::transport::simulator::info(), Box::new(Firmware)).unwrap(),
        );
        let (mut forwarder, mut auto, mut jobs) = (None, None, Vec::new());
        let (mut next, mut scheduled) = (1, false);
        let mut run = |camera: &mut Option<Camera>, command| {
            process(
                &h,
                camera,
                &mut forwarder,
                &mut auto,
                &mut jobs,
                &mut next,
                &mut scheduled,
                command,
            )
        };
        run(&mut camera, SessionCommand::Auto { balance: Some(0.5) }).unwrap();
        assert_eq!(
            camera.as_mut().unwrap().get("ExposureAuto").unwrap(),
            "Continuous"
        );
        camera.as_mut().unwrap().set("ExposureAuto", "Off").unwrap();
        let error = run(&mut camera, SessionCommand::Undo).unwrap_err();
        assert!(format!("{error:#}").contains("ExposureAuto changed outside"));
        assert_eq!(camera.as_mut().unwrap().get("ExposureAuto").unwrap(), "Off");
        assert_eq!(
            h.snapshot().history.undo.as_deref(),
            Some("Enable auto mode")
        );
        h.shutdown();
    }

    #[test]
    fn replay_addresses_the_original_selector_bank_and_keeps_current_selection() {
        let h = WorkerHandle::new();
        let mut camera = Some(
            Camera::from_backend(crate::transport::simulator::info(), Box::new(Banks)).unwrap(),
        );
        let (mut forwarder, mut auto, mut jobs) = (None, None, Vec::new());
        let (mut next, mut scheduled) = (1, false);
        let mut run = |camera: &mut Option<Camera>, command| {
            process(
                &h,
                camera,
                &mut forwarder,
                &mut auto,
                &mut jobs,
                &mut next,
                &mut scheduled,
                command,
            )
        };
        let changed = run(
            &mut camera,
            SessionCommand::Set {
                feature: "Setting".into(),
                value: "20".into(),
            },
        )
        .unwrap();
        let token = changed["camera_change"]["token"]
            .as_str()
            .unwrap()
            .to_owned();
        run(
            &mut camera,
            SessionCommand::Set {
                feature: "BankSelector".into(),
                value: "B".into(),
            },
        )
        .unwrap();
        run(
            &mut camera,
            SessionCommand::ReplayCameraChange {
                token: token.clone(),
                undo: true,
            },
        )
        .unwrap();
        assert_eq!(camera.as_mut().unwrap().get("BankSelector").unwrap(), "B");
        assert_eq!(camera.as_mut().unwrap().get("Setting").unwrap(), 11);
        camera.as_mut().unwrap().set("BankSelector", "A").unwrap();
        assert_eq!(camera.as_mut().unwrap().get("Setting").unwrap(), 10);
        camera.as_mut().unwrap().set("BankSelector", "B").unwrap();
        run(
            &mut camera,
            SessionCommand::ReplayCameraChange { token, undo: false },
        )
        .unwrap();
        assert_eq!(camera.as_mut().unwrap().get("BankSelector").unwrap(), "B");
        camera.as_mut().unwrap().set("BankSelector", "A").unwrap();
        assert_eq!(camera.as_mut().unwrap().get("Setting").unwrap(), 20);
        h.shutdown();
    }

    #[test]
    fn set_many_restores_and_undoes_every_bank_it_selects() {
        let h = WorkerHandle::new();
        let mut camera = Some(
            Camera::from_backend(crate::transport::simulator::info(), Box::new(Banks)).unwrap(),
        );
        let (mut forwarder, mut auto, mut jobs) = (None, None, Vec::new());
        let (mut next, mut scheduled) = (1, false);
        let mut run = |camera: &mut Option<Camera>, command| {
            process(
                &h,
                camera,
                &mut forwarder,
                &mut auto,
                &mut jobs,
                &mut next,
                &mut scheduled,
                command,
            )
        };
        let many = |values: &[(&str, &str)]| SessionCommand::SetMany {
            values: values
                .iter()
                .map(|(feature, value)| crate::session::FeatureValue {
                    feature: (*feature).into(),
                    value: (*value).into(),
                })
                .collect(),
        };
        // Bank A holds 10 and bank B 11; A is selected.
        let banks = |camera: &mut Option<Camera>| {
            let camera = camera.as_mut().unwrap();
            let selected = camera.get("BankSelector").unwrap();
            let mut values = Vec::new();
            for bank in ["A", "B"] {
                camera.set("BankSelector", bank).unwrap();
                values.push(camera.get("Setting").unwrap());
            }
            camera
                .set("BankSelector", selected.as_str().unwrap())
                .unwrap();
            (selected, values)
        };
        let initial = (json!("A"), vec![json!(10), json!(11)]);
        assert_eq!(banks(&mut camera), initial);
        // A failure after switching banks restores the bank it wrote too.
        assert!(
            run(
                &mut camera,
                many(&[("BankSelector", "B"), ("Setting", "99"), ("Setting", "x")]),
            )
            .is_err()
        );
        assert_eq!(banks(&mut camera), initial);
        // So does one after writing an unlisted feature directly.
        assert!(
            run(
                &mut camera,
                many(&[("BValue", "42"), ("Setting", "20"), ("Setting", "x")])
            )
            .is_err()
        );
        assert_eq!(banks(&mut camera), initial);
        assert!(h.snapshot().history.entries.is_empty());
        // Success records every bank written, even with the selector put back.
        let changed = run(
            &mut camera,
            many(&[
                ("Setting", "20"),
                ("BankSelector", "B"),
                ("Setting", "99"),
                ("BankSelector", "A"),
            ]),
        )
        .unwrap();
        assert_eq!(banks(&mut camera), (json!("A"), vec![json!(20), json!(99)]));
        let token = changed["camera_change"]["token"]
            .as_str()
            .unwrap()
            .to_owned();
        let switched = run(
            &mut camera,
            many(&[("BankSelector", "B"), ("Setting", "7")]),
        )
        .unwrap()["camera_change"]["token"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(banks(&mut camera), (json!("B"), vec![json!(20), json!(7)]));
        run(
            &mut camera,
            SessionCommand::ReplayCameraChange {
                token: switched,
                undo: true,
            },
        )
        .unwrap();
        assert_eq!(banks(&mut camera), (json!("A"), vec![json!(20), json!(99)]));
        run(
            &mut camera,
            SessionCommand::ReplayCameraChange {
                token: token.clone(),
                undo: true,
            },
        )
        .unwrap();
        assert_eq!(banks(&mut camera), initial);
        run(
            &mut camera,
            SessionCommand::ReplayCameraChange { token, undo: false },
        )
        .unwrap();
        assert_eq!(banks(&mut camera), (json!("A"), vec![json!(20), json!(99)]));
        h.shutdown();
    }

    #[derive(Default)]
    struct CoupledState {
        width: u32,
        offset: u32,
        fail_offset_write: bool,
        fail_offset_read: u8,
        arm_read_failure: bool,
    }
    struct Coupled(Arc<Mutex<CoupledState>>);
    impl crate::types::RegisterIo for Coupled {
        fn read_memory(&mut self, address: u64, _: usize) -> Result<Vec<u8>> {
            let mut state = self.0.lock().unwrap();
            if address == 4 && state.fail_offset_read > 0 {
                state.fail_offset_read -= 1;
                anyhow::bail!("one-shot offset read failure");
            }
            Ok(if address == 0 {
                state.width
            } else {
                state.offset
            }
            .to_le_bytes()
            .to_vec())
        }
        fn write_memory(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            let value = u32::from_le_bytes(bytes.try_into().unwrap());
            if address == 0 {
                state.width = value;
                state.offset = state.offset.min(1024 - value);
                if state.arm_read_failure {
                    state.arm_read_failure = false;
                    state.fail_offset_read = 2;
                }
            } else {
                ensure!(!state.fail_offset_write, "offset write rejected");
                ensure!(value + state.width <= 1024, "offset outside image bounds");
                state.offset = value;
            }
            Ok(())
        }
    }
    impl crate::types::Backend for Coupled {
        fn xml(&mut self) -> Result<String> {
            Ok(concat!("<RegisterDescription><Category Name='Root'><pFeature>Width</pFeature><pFeature>OffsetX</pFeature></Category>",
                "<Integer Name='Width'><pValue>W</pValue><Min>1</Min><Max>1024</Max></Integer><IntReg Name='W'><Address>0</Address><Length>4</Length><AccessMode>RW</AccessMode></IntReg>",
                "<Integer Name='OffsetX'><pValue>O</pValue><Min>0</Min><Max>1024</Max></Integer><IntReg Name='O'><Address>4</Address><Length>4</Length><AccessMode>RW</AccessMode></IntReg></RegisterDescription>").into())
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
    fn coupled_values(state: &Arc<Mutex<CoupledState>>) -> (u32, u32) {
        let state = state.lock().unwrap();
        (state.width, state.offset)
    }

    #[test]
    fn coupled_feature_changes_and_partial_replay_failures_restore_exactly() {
        let state = Arc::new(Mutex::new(CoupledState {
            width: 640,
            offset: 100,
            ..Default::default()
        }));
        let h = WorkerHandle::new();
        let mut camera = Some(
            Camera::from_backend(
                crate::transport::simulator::info(),
                Box::new(Coupled(state.clone())),
            )
            .unwrap(),
        );
        let (mut forwarder, mut auto, mut jobs) = (None, None, Vec::new());
        let (mut next, mut scheduled) = (1, false);
        let mut run = |camera: &mut Option<Camera>, command| {
            process(
                &h,
                camera,
                &mut forwarder,
                &mut auto,
                &mut jobs,
                &mut next,
                &mut scheduled,
                command,
            )
        };
        run(
            &mut camera,
            SessionCommand::Set {
                feature: "Width".into(),
                value: "1000".into(),
            },
        )
        .unwrap();
        assert_eq!(state.lock().unwrap().offset, 24);
        state.lock().unwrap().fail_offset_write = true;
        assert!(run(&mut camera, SessionCommand::Undo).is_err());
        assert_eq!(coupled_values(&state), (1000, 24));
        assert_eq!(h.snapshot().history.undo.as_deref(), Some("Set Width"));
        state.lock().unwrap().fail_offset_write = false;
        run(&mut camera, SessionCommand::Undo).unwrap();
        assert_eq!(coupled_values(&state), (640, 100));
        run(&mut camera, SessionCommand::Redo).unwrap();
        assert_eq!(coupled_values(&state), (1000, 24));
        h.shutdown();
    }

    #[test]
    fn failed_checkpoint_read_rolls_back_the_successful_hardware_write() {
        let state = Arc::new(Mutex::new(CoupledState {
            width: 640,
            offset: 100,
            arm_read_failure: true,
            ..Default::default()
        }));
        let h = WorkerHandle::new();
        let mut camera = Some(
            Camera::from_backend(
                crate::transport::simulator::info(),
                Box::new(Coupled(state.clone())),
            )
            .unwrap(),
        );
        let (mut forwarder, mut auto, mut jobs) = (None, None, Vec::new());
        let (mut next, mut scheduled) = (1, false);
        let error = process(
            &h,
            &mut camera,
            &mut forwarder,
            &mut auto,
            &mut jobs,
            &mut next,
            &mut scheduled,
            SessionCommand::Set {
                feature: "Width".into(),
                value: "1000".into(),
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("previous camera state restored"));
        assert_eq!(coupled_values(&state), (640, 100));
        assert_eq!(h.snapshot().history, CameraHistoryStatus::default());
        h.shutdown();
    }

    /// The simulator with two registers only a full feature walk reads:
    /// Gain's maximum, which counts walks, and a lock on Width and the frame
    /// rate that holds while the camera streams. Height can be made
    /// unavailable.
    struct Walked {
        simulator: crate::transport::simulator::Simulator,
        walks: Arc<AtomicU64>,
        streaming: bool,
        hidden: Arc<AtomicBool>,
    }
    const GAIN_MAX: u64 = 0x1f0;
    const LOCKED: u64 = 0x1f8;
    const AVAILABLE: u64 = 0x1fc;
    impl crate::types::RegisterIo for Walked {
        fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
            match address {
                GAIN_MAX => {
                    self.walks.fetch_add(1, Ordering::Relaxed);
                    Ok(24f64.to_le_bytes().to_vec())
                }
                LOCKED => Ok(u32::from(self.streaming).to_le_bytes().to_vec()),
                AVAILABLE => {
                    let available = !self.hidden.load(Ordering::Relaxed);
                    Ok(u32::from(available).to_le_bytes().to_vec())
                }
                _ => self.simulator.read_memory(address, length),
            }
        }
        fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
            self.simulator.write_memory(address, data)
        }
    }
    impl crate::types::Backend for Walked {
        fn xml(&mut self) -> Result<String> {
            let registers = concat!(
                "<FloatReg Name='GainMax'><Address>0x1f0</Address><Length>8</Length><AccessMode>RO</AccessMode><Endianess>LittleEndian</Endianess></FloatReg>",
                "<IntReg Name='Locked'><Address>0x1f8</Address><Length>4</Length><AccessMode>RO</AccessMode><Endianess>LittleEndian</Endianess></IntReg>",
                "<IntReg Name='Available'><Address>0x1fc</Address><Length>4</Length><AccessMode>RO</AccessMode><Endianess>LittleEndian</Endianess></IntReg>",
                "</RegisterDescription>"
            );
            Ok(self
                .simulator
                .xml()?
                .replace("<Max>24</Max>", "<pMax>GainMax</pMax>")
                .replace(
                    "<pValue>WidthReg</pValue>",
                    "<pIsLocked>Locked</pIsLocked><pValue>WidthReg</pValue>",
                )
                .replace(
                    "<pValue>RateReg</pValue>",
                    "<pIsLocked>Locked</pIsLocked><pValue>RateReg</pValue>",
                )
                .replace(
                    "<pValue>HeightReg</pValue>",
                    "<pIsAvailable>Available</pIsAvailable><pValue>HeightReg</pValue>",
                )
                .replace("</RegisterDescription>", registers))
        }
        fn start(&mut self, payload: usize) -> Result<()> {
            self.streaming = true;
            self.simulator.start(payload)
        }
        fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
            self.simulator.next_frame(timeout)
        }
        fn stop(&mut self) -> Result<()> {
            self.streaming = false;
            self.simulator.stop()
        }
    }

    fn walked(walks: &Arc<AtomicU64>, hidden: &Arc<AtomicBool>) -> Option<Camera> {
        Some(
            Camera::from_backend(
                crate::transport::simulator::info(),
                Box::new(Walked {
                    simulator: Default::default(),
                    walks: walks.clone(),
                    streaming: false,
                    hidden: hidden.clone(),
                }),
            )
            .unwrap(),
        )
    }

    #[test]
    fn an_edit_walks_the_features_once_and_starting_walks_none() {
        let walks = Arc::new(AtomicU64::new(0));
        let h = WorkerHandle::new();
        let mut camera = walked(&walks, &Arc::default());
        // Connecting walks the features once.
        refresh(&h, &mut camera, true);
        let (mut forwarder, mut auto, mut jobs) = (None, None, Vec::new());
        let (mut next, mut scheduled) = (1, false);
        let mut walked = |camera: &mut Option<Camera>, command| {
            let before = walks.load(Ordering::Relaxed);
            let result = process(
                &h,
                camera,
                &mut forwarder,
                &mut auto,
                &mut jobs,
                &mut next,
                &mut scheduled,
                command,
            )
            .unwrap();
            (walks.load(Ordering::Relaxed) - before, result)
        };
        let set = |feature: &str, value: &str| SessionCommand::Set {
            feature: feature.into(),
            value: value.into(),
        };
        let (count, result) = walked(&mut camera, set("Width", "128"));
        assert_eq!(count, 1);
        assert!(result["camera_change"]["token"].is_string());
        assert_eq!(result["history"]["undo"], "Set Width");
        // A grouped assistant edit shares the cached checkpoint names too:
        // all writes together still end with only one full feature walk.
        let (count, result) = walked(
            &mut camera,
            SessionCommand::SetMany {
                values: [("Width", "256"), ("Height", "192")]
                    .into_iter()
                    .map(|(feature, value)| crate::session::FeatureValue {
                        feature: feature.into(),
                        value: value.into(),
                    })
                    .collect(),
            },
        );
        assert_eq!(count, 1);
        assert!(result["camera_change"]["token"].is_string());
        assert_eq!(result["history"]["undo"], "Set Width, Height");
        assert_eq!(walked(&mut camera, SessionCommand::Start).0, 0);
        let writable = |h: &WorkerHandle, name: &str| {
            h.snapshot()
                .features
                .iter()
                .find(|feature| feature.name == name)
                .unwrap()
                .writable
        };
        // A walk while streaming sees the geometry and the frame rate locked;
        // stopping unlocks both without another walk.
        assert_eq!(walked(&mut camera, set("ExposureTime", "5000")).0, 1);
        assert!(!writable(&h, "Width") && !writable(&h, "AcquisitionFrameRate"));
        assert_eq!(walked(&mut camera, SessionCommand::Stop).0, 0);
        assert!(writable(&h, "Width") && writable(&h, "AcquisitionFrameRate"));
        assert!(!writable(&h, "DeviceScanType"));
        // Undo still reads every value it restores, and walks once.
        assert_eq!(walked(&mut camera, SessionCommand::Undo).0, 1);
        assert!(camera.as_ref().unwrap().is_streaming());
        h.shutdown();
    }

    #[test]
    fn a_feature_that_became_unreadable_since_the_last_walk_is_left_out() {
        let hidden = Arc::new(AtomicBool::new(false));
        let h = WorkerHandle::new();
        let mut camera = walked(&Arc::default(), &hidden);
        refresh(&h, &mut camera, true);
        assert!(
            h.snapshot()
                .features
                .iter()
                .any(|f| f.name == "Height" && f.value.is_some())
        );
        hidden.store(true, Ordering::Relaxed);
        let result = process(
            &h,
            &mut camera,
            &mut None,
            &mut None,
            &mut Vec::new(),
            &mut 1,
            &mut false,
            SessionCommand::Set {
                feature: "Width".into(),
                value: "128".into(),
            },
        )
        .unwrap();
        // As a fresh walk would, the checkpoint skips Height instead of
        // giving up on undo.
        assert!(result.get("history_unavailable").is_none(), "{result}");
        assert!(result["camera_change"]["token"].is_string());
        h.shutdown();
    }
}
