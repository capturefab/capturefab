//! Rollback checkpoints for camera changes that write several values: what
//! a change would replace is read from the camera first, never inferred
//! from a cached snapshot, and a failure writes it all back.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

mod setup;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Address {
    feature: String,
    selectors: Vec<(String, String)>,
}

#[derive(Clone)]
struct Configuration {
    values: BTreeMap<Address, Value>,
    auto: Option<AutoController>,
}

fn text(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Number(number) if number.is_f64() => number.as_f64().unwrap().to_string(),
        value => value.to_string(),
    }
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
/// both success and failure. Selector switches made only to reach a bank are
/// never recorded as changes.
fn addressed<T>(
    camera: &mut Camera,
    address: &Address,
    action: impl FnOnce(&mut Camera) -> Result<T>,
) -> Result<T> {
    crate::controls::with_bank(camera, &address.selectors, action)
}

/// How results name a value: `Feature`, or `Feature[Selector=Value,...]`.
fn address_name(address: &Address) -> String {
    if address.selectors.is_empty() {
        return address.feature.clone();
    }
    let bank = address
        .selectors
        .iter()
        .map(|(selector, value)| format!("{selector}={value}"))
        .collect::<Vec<_>>();
    format!("{}[{}]", address.feature, bank.join(","))
}

/// Read the value at `address` for a checkpoint. A bank hidden by
/// `pIsAvailable` reads as `null`: it holds no value to restore, and a
/// restore writes nothing there, while the setting that hides it is
/// restored as usual. Other read failures stay errors.
fn read_at(camera: &mut Camera, address: &Address) -> Result<Value> {
    addressed(camera, address, |camera| {
        match camera.get(&address.feature) {
            Ok(value) => Ok(value),
            Err(_)
                if camera.availability(&address.feature)
                    == crate::genicam::Availability::Unavailable =>
            {
                Ok(Value::Null)
            }
            Err(error) => Err(error),
        }
    })
}

/// A feature addressed by more banks than a checkpoint reads.
#[derive(Debug)]
struct TooManyBanks(String);
impl std::fmt::Display for TooManyBanks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "too many selected banks to checkpoint {}", self.0)
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
            .with_context(|| format!("read {selector} for rollback"))?;
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

/// Features written together: changing one can change what the others
/// allow (an offset its width, binning the width, the pixel format the
/// size), so a checkpoint of one holds all of them.
const COUPLED: [&[&str]; 2] = [
    &[
        "OffsetX",
        "OffsetY",
        "Width",
        "Height",
        "BinningHorizontal",
        "BinningVertical",
        "DecimationHorizontal",
        "DecimationVertical",
        "PixelFormat",
    ],
    &[
        "AcquisitionFrameRate",
        "AcquisitionFrameRateEnable",
        "ExposureTime",
    ],
];

/// Read the configuration values a command may change, now, and only
/// those: the features it writes with the rest of their coupled group, what
/// auto mode manages, and the exposure, gain and white-balance features an
/// auto mode it ends may have locked. Every gain and white-balance bank is
/// included, since auto can change them; any other feature stays at its
/// current bank. Never a walk of every feature, which on GigE is hundreds
/// of round trips. `written` requires each named feature, including
/// controls absent from the feature tree.
fn capture(
    camera: &mut Camera,
    auto: &Option<AutoController>,
    written: &[&str],
) -> Result<Configuration> {
    let mut required = written
        .iter()
        .map(|&name| name.to_owned())
        .collect::<BTreeSet<_>>();
    let mut names = BTreeSet::new();
    for group in COUPLED {
        if group.iter().any(|name| required.contains(*name)) {
            names.extend(
                group
                    .iter()
                    .filter(|name| camera.has(name) && camera.is_configuration_value(name))
                    .map(|name| (*name).to_owned()),
            );
        }
    }
    if let Some(controller) = auto {
        required.extend(controller.status().managed.iter().cloned());
    }
    // Firmware may lock the numeric features while auto is running. They
    // still have readable originals and must be included when a manual write
    // stops the firmware controller.
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
    names.extend(required.iter().cloned());
    let mut values = BTreeMap::new();
    for name in names {
        // Auto can change every gain/white-balance bank. Other selectors stay
        // at their current bank, avoiding intrusive enumeration of unrelated
        // device settings just to checkpoint one control.
        let all_banks = matches!(name.as_str(), "Gain" | "GainAbs" | "BalanceRatio");
        let read = (|| -> Result<Vec<(Address, Value)>> {
            let mut read = Vec::new();
            for address in addresses(camera, &name, &|_| all_banks)? {
                let value = read_at(camera, &address)
                    .with_context(|| format!("read {name} for rollback"))?;
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
#[cfg(test)]
fn extend(camera: &mut Camera, before: &mut Configuration, feature: &str) -> Result<()> {
    for address in addresses(camera, feature, &|_| false)? {
        if let std::collections::btree_map::Entry::Vacant(entry) = before.values.entry(address) {
            // The address names the banks selected now: nothing switches.
            let value = read_at(camera, entry.key())
                .with_context(|| format!("read {feature} for rollback"))?;
            entry.insert(value);
        }
    }
    Ok(())
}

fn set_exact(camera: &mut Camera, address: &Address, target: &Value) -> Result<()> {
    addressed(camera, address, |camera| {
        // A value an earlier write hid (an enumeration entry gated by
        // another setting) cannot be read, but can be overwritten.
        if camera.get(&address.feature).ok().as_ref() != Some(target) {
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
    // A bank that was unavailable has no value to write: restoring the
    // setting that hid it hides it again.
    let restorable = |address: &&Address| {
        target
            .values
            .get(*address)
            .is_some_and(|value| !value.is_null())
    };
    let mut pending = fields
        .iter()
        .filter(restorable)
        .cloned()
        .collect::<Vec<_>>();
    // The same order as a setup restore: a switch that arms (TriggerMode On,
    // LineMode Output) after its source, one that disarms before it, auto
    // flags last and selectors after the values in their banks. Retry
    // dependency-locked fields after limits and enabling controls settle.
    pending.sort_by_cached_key(|address| {
        (
            crate::setup::order_key(&address.feature, Some(&text(&target.values[address]))),
            address.selectors.clone(),
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
            .filter(restorable)
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

/// Publish the camera after a rollback: its state and auto mode. A
/// rollback to manual mode ends continuous auto exposure.
fn publish(h: &WorkerHandle, camera: &Option<Camera>, auto: &Option<AutoController>) {
    refresh(h, camera);
    if let Some(controller) = auto {
        publish_auto(h, controller);
    } else {
        h.state.write().unwrap_or_else(|e| e.into_inner()).auto = None;
        end_auto_job(h, JobStatus::Done);
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn process(
    h: &WorkerHandle,
    camera: &mut Option<Camera>,
    forwarder: &mut Option<(Forwarder, f64)>,
    auto: &mut Option<AutoController>,
    jobs: &mut Vec<Scheduled>,
    next_job: &mut u64,
    scheduled_stream: &mut JobStream,
    command: WorkerCommand,
) -> Result<Value> {
    command.validate()?;
    #[cfg(test)]
    if let WorkerCommand::Set { feature, value } = &command {
        camera_mut(camera)?.validate_set(feature, value)?;
    }
    match command.session() {
        // Reading and comparing switch selectors only to read, and restore them.
        Some(SessionCommand::ReadSetup) => return setup::read(h, camera_mut(camera)?, auto),
        Some(SessionCommand::SetupDiff {
            setup: Some(document),
            allow_other_model,
            ..
        }) => {
            let holders = setup::stream_holders(forwarder, jobs);
            let c = camera_mut(camera)?;
            let plan = setup::plan(c, auto, &holders, document, *allow_other_model)?;
            return Ok(setup::report(&plan, &c.info));
        }
        Some(SessionCommand::SetupDiff { .. } | SessionCommand::ApplySetup { setup: None, .. }) => {
            anyhow::bail!("preset names are resolved by the session coordinator")
        }
        _ => {}
    }
    // A change that may write several values is one change: what it would
    // replace is read first, and a failure puts every value back. A single
    // Set writes several only when it ends auto mode.
    let atomic = match &command {
        #[cfg(test)]
        WorkerCommand::Set { feature, .. } => auto.as_ref().is_some_and(|a| a.manages(feature)),
        #[cfg(test)]
        WorkerCommand::SetMany { .. } => true,
        WorkerCommand::Session(
            SessionCommand::Auto { .. }
            | SessionCommand::Manual { .. }
            | SessionCommand::ApplySetup { .. },
        ) => true,
        _ => false,
    };
    if !atomic {
        return process_plain(
            h,
            camera,
            forwarder,
            auto,
            jobs,
            next_job,
            scheduled_stream,
            command,
        );
    }
    let c = camera_mut(camera)?;
    // A setup is compared again here, by the camera's owner, right before
    // it is restored; only the values it changes are required readable.
    let setup_plan = match &command {
        WorkerCommand::Session(SessionCommand::ApplySetup {
            setup: Some(document),
            allow_other_model,
            strict,
            ..
        }) => {
            let holders = setup::stream_holders(forwarder, jobs);
            let plan = setup::plan(c, auto, &holders, document, *allow_other_model)?;
            plan.check(*strict)?;
            Some(plan)
        }
        _ => None,
    };
    // The features a change writes are checkpointed with their coupled
    // groups. A banked setup row is checkpointed at its own bank before it
    // is written, so its feature need not be readable at the bank selected
    // now (an input line hides an output's source).
    let written = match &command {
        WorkerCommand::Session(SessionCommand::ApplySetup { .. }) => setup_plan
            .iter()
            .flat_map(|plan| {
                plan.changes()
                    .filter(|row| row.address.selectors.is_empty())
                    .map(|row| row.address.feature.as_str())
            })
            .collect(),
        #[cfg(test)]
        WorkerCommand::Set { feature, .. } => vec![feature.as_str()],
        #[cfg(test)]
        WorkerCommand::SetMany { values } => {
            values.iter().map(|value| value.feature.as_str()).collect()
        }
        _ => Vec::new(),
    };
    let apply_setup = matches!(command.session(), Some(SessionCommand::ApplySetup { .. }));
    #[cfg(test)]
    let together = apply_setup || matches!(command, WorkerCommand::SetMany { .. });
    #[cfg(not(test))]
    let together = apply_setup;
    let mut before = match capture(c, auto, &written) {
        Ok(before) => before,
        // Its values must be restorable together, so it writes none.
        Err(error) if together => {
            return Err(error.context(format!(
                "{} changed nothing: the values it would replace could not be read",
                if apply_setup {
                    "apply_setup"
                } else {
                    "set_many"
                }
            )));
        }
        // A write-only control is still useful; such a change goes ahead
        // without a checkpoint to roll back to.
        Err(_) => {
            return process_plain(
                h,
                camera,
                forwarder,
                auto,
                jobs,
                next_job,
                scheduled_stream,
                command,
            );
        }
    };
    let mut restart = false;
    let result = match &command {
        // Each write first records the bank it is about to change.
        #[cfg(test)]
        WorkerCommand::SetMany { values } => {
            super::set_many(h, camera, auto, values, |camera, feature| {
                extend(camera, &mut before, feature)
            })
        }
        // A streaming camera locks its geometry: stop it once, write, and
        // start it again after success or the rollback below.
        WorkerCommand::Session(SessionCommand::ApplySetup {
            setup: Some(document),
            strict,
            ..
        }) => {
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
                        *strict,
                        |camera, address| setup::extend_at(camera, &mut before, address),
                    )
                })
                .and_then(|mut result| {
                    if restart {
                        camera_mut(camera)?.start()?;
                        refresh(h, camera);
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
    let error = match result {
        Ok(result) => return Ok(result),
        Err(error) => error,
    };
    let fields = before.values.keys().cloned().collect::<Vec<_>>();
    // What a failed preset wrote, for the rollback its error reports.
    let mut written = Vec::new();
    if apply_setup {
        let camera = camera_mut(camera)?;
        for address in &fields {
            let saved = &before.values[address];
            if !saved.is_null()
                && read_at(camera, address).is_ok_and(|now| !setup::close(&now, saved, None))
            {
                written.push(address_name(address));
            }
        }
    }
    let switch_auto = auto_policy(&before.auto) != auto_policy(auto);
    let rollback = restore(camera_mut(camera)?, auto, &before, &fields, switch_auto);
    let restarted = restart.then(|| camera_mut(camera).and_then(Camera::start));
    publish(h, camera, auto);
    let restart_note = match &restarted {
        Some(Err(start_error)) => {
            format!("; the stream could not be restarted: {start_error:#}")
        }
        Some(Ok(())) => "; the stream was restarted".into(),
        None => String::new(),
    };
    if let Err(rollback_error) = rollback {
        let error = error.context(format!(
            "failed command rollback: {rollback_error:#}{restart_note}"
        ));
        if apply_setup {
            return Err(crate::types::with_details(
                error,
                json!({"rollback": {"restored": [], "failed": [{"error": format!("{rollback_error:#}")}]}}),
            ));
        }
        return Err(error);
    }
    // The context keeps the cause's category for the exit code.
    if let WorkerCommand::Session(SessionCommand::ApplySetup {
        setup: Some(document),
        ..
    }) = &command
    {
        let error = error.context(format!(
            "Restoring preset {} failed; every value was put back{restart_note}",
            document.name
        ));
        return Err(crate::types::with_details(
            error,
            json!({"rollback": {"restored": written, "failed": []}}),
        ));
    }
    if let Some(Err(_)) = restarted {
        return Err(error.context(restart_note.trim_start_matches("; ").to_owned()));
    }
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connected() -> WorkerHandle {
        let h = WorkerHandle::new();
        h.request(SessionCommand::connect("sim:0".into(), 100))
            .unwrap();
        h
    }
    fn get(h: &WorkerHandle, feature: &str) -> Value {
        h.request(WorkerCommand::Get {
            feature: feature.into(),
        })
        .unwrap()["value"]
            .clone()
    }
    fn set_many(h: &WorkerHandle, values: &[(&str, &str)]) -> Result<Value> {
        h.request(WorkerCommand::SetMany {
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
    fn a_failed_set_many_restores_every_value_it_wrote() {
        let h = connected();
        let (exposure, width) = (get(&h, "ExposureTime"), get(&h, "Width"));
        // The second value is out of range after the first was written.
        assert!(set_many(&h, &[("ExposureTime", "5000"), ("Width", "99999")]).is_err());
        assert_eq!(get(&h, "ExposureTime"), exposure);
        assert_eq!(get(&h, "Width"), width);
        let changed = set_many(&h, &[("ExposureTime", "5000"), ("Width", "256")]).unwrap();
        assert_eq!(changed["values"][0]["name"], "ExposureTime");
        assert_eq!(changed["values"][0]["value"].as_f64(), Some(5000.0));
        assert_eq!(changed["values"][1], json!({"name":"Width","value":256}));
        assert_eq!(get(&h, "Width"), 256);
        h.shutdown();
    }

    #[test]
    fn failed_managed_write_does_not_leave_auto_disabled() {
        let h = connected();
        h.request(SessionCommand::Auto {
            balance: Some(0.3),
            continuous: false,
            job: None,
            max_exposure_us: None,
            region: None,
        })
        .unwrap();
        let exposure = get(&h, "ExposureTime");
        assert!(
            h.request(WorkerCommand::Set {
                feature: "ExposureTime".into(),
                value: "0".into()
            })
            .is_err()
        );
        assert_eq!(h.snapshot().auto.unwrap().balance, 0.3);
        assert_eq!(get(&h, "ExposureTime"), exposure);
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

    #[test]
    fn a_failed_set_many_restores_every_bank_it_selects() {
        let h = WorkerHandle::new();
        let mut camera = Some(
            Camera::from_backend(crate::transport::simulator::info(), Box::new(Banks)).unwrap(),
        );
        let (mut forwarder, mut auto, mut jobs) = (None, None, Vec::new());
        let (mut next, mut scheduled) = (1, JobStream::default());
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
        let many = |values: &[(&str, &str)]| WorkerCommand::SetMany {
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
        // Success writes every bank, even with the selector put back.
        run(
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
        h.shutdown();
    }

    /// A width that clamps the offset after it, as image geometry does.
    struct CoupledState {
        width: u32,
        offset: u32,
    }
    struct Coupled(Arc<Mutex<CoupledState>>);
    impl crate::types::RegisterIo for Coupled {
        fn read_memory(&mut self, address: u64, _: usize) -> Result<Vec<u8>> {
            let state = self.0.lock().unwrap();
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
            } else {
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
    fn a_failed_set_many_restores_values_an_earlier_write_moved() {
        let state = Arc::new(Mutex::new(CoupledState {
            width: 640,
            offset: 100,
        }));
        let h = WorkerHandle::new();
        let mut camera = Some(
            Camera::from_backend(
                crate::transport::simulator::info(),
                Box::new(Coupled(state.clone())),
            )
            .unwrap(),
        );
        let many = |values: &[(&str, &str)]| WorkerCommand::SetMany {
            values: values
                .iter()
                .map(|(feature, value)| crate::session::FeatureValue {
                    feature: (*feature).into(),
                    value: (*value).into(),
                })
                .collect(),
        };
        let mut run = |command| {
            process(
                &h,
                &mut camera,
                &mut None,
                &mut None,
                &mut Vec::new(),
                &mut 1,
                &mut JobStream::default(),
                command,
            )
        };
        // Width 1000 moves OffsetX to 24, and OffsetX 100 no longer fits.
        let error = run(many(&[("Width", "1000"), ("OffsetX", "100")])).unwrap_err();
        assert!(
            format!("{error:#}").contains("offset outside image bounds"),
            "{error:#}"
        );
        assert_eq!(coupled_values(&state), (640, 100));
        run(many(&[("Width", "1000"), ("OffsetX", "20")])).unwrap();
        assert_eq!(coupled_values(&state), (1000, 20));
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
    fn edits_starting_and_stopping_walk_no_features() {
        let walks = Arc::new(AtomicU64::new(0));
        let h = WorkerHandle::new();
        let mut camera = walked(&walks, &Arc::default());
        let (mut forwarder, mut auto, mut jobs) = (None, None, Vec::new());
        let (mut next, mut scheduled) = (1, JobStream::default());
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
        let set = |feature: &str, value: &str| WorkerCommand::Set {
            feature: feature.into(),
            value: value.into(),
        };
        let (count, result) = walked(&mut camera, set("Width", "128"));
        assert_eq!(count, 0);
        assert_eq!(result, json!({"name":"Width","value":128}));
        // A grouped edit checkpoints what it writes and the features coupled
        // with it, not every feature.
        let (count, result) = walked(
            &mut camera,
            WorkerCommand::SetMany {
                values: [("Width", "256"), ("Height", "192")]
                    .into_iter()
                    .map(|(feature, value)| crate::session::FeatureValue {
                        feature: feature.into(),
                        value: value.into(),
                    })
                    .collect(),
            },
        );
        assert_eq!(count, 0);
        assert_eq!(result["values"][1], json!({"name":"Height","value":192}));
        let values = vec![crate::controls::Assignment::parse("ExposureTime=4000").unwrap()];
        let (count, result) = walked(
            &mut camera,
            SessionCommand::SetValues {
                values,
                dry_run: false,
            }
            .into(),
        );
        assert_eq!(count, 0);
        assert_eq!(result["changed"][0]["name"], "ExposureTime", "{result}");
        assert_eq!(walked(&mut camera, SessionCommand::Start.into()).0, 0);
        assert_eq!(walked(&mut camera, set("ExposureTime", "5000")).0, 0);
        assert_eq!(walked(&mut camera, SessionCommand::Stop.into()).0, 0);
        h.shutdown();
    }

    #[test]
    fn a_coupled_feature_that_is_unavailable_is_left_out_of_the_checkpoint() {
        let hidden = Arc::new(AtomicBool::new(false));
        let h = WorkerHandle::new();
        let mut camera = walked(&Arc::default(), &hidden);
        assert!(camera.as_mut().unwrap().get("Height").is_ok());
        hidden.store(true, Ordering::Relaxed);
        let result = process(
            &h,
            &mut camera,
            &mut None,
            &mut None,
            &mut Vec::new(),
            &mut 1,
            &mut JobStream::default(),
            WorkerCommand::SetMany {
                values: vec![crate::session::FeatureValue {
                    feature: "Width".into(),
                    value: "128".into(),
                }],
            },
        )
        .unwrap();
        // Height, coupled with Width, cannot be read now: the checkpoint
        // skips it instead of refusing the change.
        assert_eq!(result["values"][0], json!({"name":"Width","value":128}));
        h.shutdown();
    }
}
