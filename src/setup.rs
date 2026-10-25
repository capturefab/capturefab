//! Saved camera setups: a named, host-side snapshot of one camera's writable
//! configuration values, kept in the state directory and restored later as
//! one undoable camera change.
//!
//! A setup never touches the camera's own user sets, and holds no address,
//! credential or capture setting. Media sources keep only their transport
//! and model, since their identifiers fingerprint the stream's host and path.
//! The camera worker reads and restores setups (`engine::history::setup`);
//! this module owns the document, its limits and the store.
use crate::types::{CameraInfo, Categorized, ErrorKind, Transport};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

/// Largest setup file.
pub const MAX_SETUP_BYTES: usize = 256 * 1024;
/// Most values one setup holds.
pub const MAX_SETUP_VALUES: usize = 512;
/// Most setups the store keeps.
pub const MAX_SETUPS: usize = 128;
/// Longest note, in characters.
pub const MAX_NOTE: usize = 512;
/// Longest string value, selector value or camera field, in bytes.
pub const MAX_STRING: usize = 1024;
/// Most selectors that address one value.
pub const MAX_BANKS: usize = 4;
/// Most skipped entries a setup records.
pub const MAX_SKIPPED: usize = 1024;
/// Selectors whose every bank a setup reads, so per-line, per-trigger and
/// per-counter configuration round-trips. Other selectors stay at the bank
/// selected when the setup is saved.
pub const SETUP_ALL_BANKS: [&str; 8] = [
    "GainSelector",
    "BlackLevelSelector",
    "BalanceRatioSelector",
    "TriggerSelector",
    "LineSelector",
    "UserOutputSelector",
    "CounterSelector",
    "TimerSelector",
];

/// One saved value: a JSON boolean, number or string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Scalar {
    Bool(bool),
    Number(serde_json::Number),
    String(String),
}
impl Scalar {
    pub fn from_value(value: &serde_json::Value) -> Option<Self> {
        match value {
            serde_json::Value::Bool(value) => Some(Self::Bool(*value)),
            serde_json::Value::Number(value) => Some(Self::Number(value.clone())),
            serde_json::Value::String(value) => Some(Self::String(value.clone())),
            _ => None,
        }
    }
    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::Bool(value) => serde_json::Value::Bool(*value),
            Self::Number(value) => serde_json::Value::Number(value.clone()),
            Self::String(value) => serde_json::Value::String(value.clone()),
        }
    }
    /// The text `Camera::set` takes.
    pub fn text(&self) -> String {
        match self {
            Self::Bool(value) => value.to_string(),
            Self::Number(value) => value.to_string(),
            Self::String(value) => value.clone(),
        }
    }
}

/// A selector and the bank it selects.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bank {
    pub selector: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupValue {
    pub feature: String,
    pub value: Scalar,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub at: Vec<Bank>,
}

/// Capturefab's software auto exposure, which a restore turns on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedAuto {
    pub balance: f64,
}

/// A value the setup left out, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Skipped {
    pub feature: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub at: Vec<Bank>,
    pub reason: String,
}

/// The camera a setup was saved from. Never its address.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedCamera {
    pub transport: Transport,
    #[serde(default)]
    pub vendor: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    /// Hub cameras only: `hub:NAME/CAMERA`, resolved through the saved hub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default)]
    pub kind: crate::camera_profile::CameraKind,
}
impl SavedCamera {
    pub fn from_info(info: &CameraInfo) -> Self {
        let kind = info.profile.kind;
        if info.id.starts_with("hub:") {
            return Self {
                transport: info.transport,
                vendor: info.vendor.clone(),
                model: info.model.clone(),
                serial: None,
                id: Some(info.id.clone()),
                kind,
            };
        }
        if info.transport == Transport::Media {
            // The serial and id of a media source hash its URL, which still
            // identifies the host and path; every webcam index or RTSP URL
            // would also match another one wrongly.
            return Self {
                transport: info.transport,
                vendor: String::new(),
                model: info.model.clone(),
                serial: None,
                id: None,
                kind,
            };
        }
        Self {
            transport: info.transport,
            vendor: info.vendor.clone(),
            model: info.model.clone(),
            serial: (!info.serial.is_empty()).then(|| info.serial.clone()),
            id: None,
            kind,
        }
    }
    /// "Basler acA1920-40gm", or the model alone.
    pub fn label(&self) -> String {
        label(&self.vendor, &self.model)
    }
}
fn label(vendor: &str, model: &str) -> String {
    match (vendor.trim(), model.trim()) {
        ("", "") => "an unknown camera".into(),
        ("", model) => model.into(),
        (vendor, "") => vendor.into(),
        (vendor, model) if model.starts_with(vendor) => model.into(),
        (vendor, model) => format!("{vendor} {model}"),
    }
}

/// A saved setup, version 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setup {
    pub version: u32,
    pub name: String,
    pub saved_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub camera: SavedCamera,
    #[serde(default)]
    pub auto: Option<SavedAuto>,
    pub values: Vec<SetupValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<Skipped>,
}

/// What a camera worker read for a new setup (`read_setup`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reading {
    pub camera: SavedCamera,
    #[serde(default)]
    pub auto: Option<SavedAuto>,
    pub values: Vec<SetupValue>,
    #[serde(default)]
    pub skipped: Vec<Skipped>,
}

/// One row of `setup list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub name: String,
    pub transport: Transport,
    pub vendor: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    pub saved_at: String,
    pub values: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// How a saved setup's camera compares with a connected one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Match {
    SameCamera,
    SameModel,
    OtherModel,
    UnknownSource,
}
impl Match {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SameCamera => "same_camera",
            Self::SameModel => "same_model",
            Self::OtherModel => "other_model",
            Self::UnknownSource => "unknown_source",
        }
    }
}

fn same(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// Compare the camera a setup was saved from with a connected one. The
/// transport is ignored: the same camera may be reached another way.
pub fn matching(saved: &SavedCamera, live: &CameraInfo) -> (Match, Vec<String>) {
    let live_saved = SavedCamera::from_info(live);
    if saved.id.is_some() || live_saved.id.is_some() {
        return if saved.id.is_some() && saved.id == live_saved.id {
            (Match::SameCamera, Vec::new())
        } else {
            (Match::OtherModel, vec![other_model_warning(saved, live)])
        };
    }
    if saved.transport == Transport::Media || live.transport == Transport::Media {
        return if saved.transport == live.transport && same(&saved.model, &live.model) {
            (
                Match::UnknownSource,
                vec![format!(
                    "Saved from {}; Capturefab cannot tell whether this is the same stream.",
                    article(&saved.model)
                )],
            )
        } else {
            (Match::OtherModel, vec![other_model_warning(saved, live)])
        };
    }
    if !(same(&saved.vendor, &live.vendor) && same(&saved.model, &live.model)) {
        return (Match::OtherModel, vec![other_model_warning(saved, live)]);
    }
    match &saved.serial {
        Some(serial) if same(serial, &live.serial) => (Match::SameCamera, Vec::new()),
        serial => (
            Match::SameModel,
            vec![format!(
                "Saved from another {}{}.",
                saved.model.trim(),
                serial
                    .as_deref()
                    .map(|serial| format!(" (serial {})", serial.trim()))
                    .unwrap_or_default()
            )],
        ),
    }
}
/// The note a preview or restore of another model carries. It names no
/// flag: RPC callers read it too.
fn other_model_warning(saved: &SavedCamera, live: &CameraInfo) -> String {
    let saved = saved.id.clone().unwrap_or_else(|| saved.label());
    let live = if live.id.starts_with("hub:") {
        live.id.clone()
    } else {
        label(&live.vendor, &live.model)
    };
    format!("Saved from {saved}; this camera is {live}; only settings both share apply.")
}
fn article(model: &str) -> String {
    let model = model.trim();
    if model.is_empty() {
        return "a media source".into();
    }
    let vowel = model
        .chars()
        .next()
        .is_some_and(|c| "AEIOUaeiou".contains(c) || (c == 'R' && model.starts_with("RTSP")));
    format!("{} {model}", if vowel { "an" } else { "a" })
}

/// The refusal for a setup from another model, naming the flag that allows it.
pub fn other_model_message(setup: &Setup, live: &CameraInfo) -> String {
    format!(
        "{} was saved from {}; this camera is {}. Pass allow_other_model (CLI: --allow-other-model) to apply the settings both share.",
        setup.name,
        setup.camera.label(),
        label(&live.vendor, &live.model)
    )
}

/// Apply order: geometry and format first; then switches that disarm or
/// release (TriggerMode = Off, LineMode = Input, `*Enable` = false); other
/// modes; other values alphabetically; then switches that arm or drive
/// (TriggerMode = On, LineMode = Output, `*Enable` = true), so a trigger
/// or output line never runs from a stale source; auto flags; and
/// selectors last, so the inspector shows the saved bank afterwards.
/// `target` is the value about to be written, as text.
pub fn order_key(feature: &str, target: Option<&str>) -> (u8, u8, String) {
    const GEOMETRY: [&str; 6] = [
        "PixelFormat",
        "Binning",
        "Decimation",
        "Width",
        "Height",
        "Offset",
    ];
    if crate::genicam::stream_locked(feature) {
        let rank = if feature == "VideoMode" {
            GEOMETRY.len()
        } else {
            GEOMETRY
                .iter()
                .position(|prefix| feature.starts_with(prefix))
                .unwrap_or(GEOMETRY.len())
        };
        return (0, rank as u8, feature.into());
    }
    let switch = matches!(feature, "TriggerMode" | "LineMode") || feature.ends_with("Enable");
    let group = if feature.ends_with("Selector") {
        6
    } else if feature.ends_with("Auto") {
        5
    } else if let (true, Some(target)) = (switch, target) {
        if matches!(target, "Off" | "Input" | "false" | "0") {
            1
        } else {
            4
        }
    } else if feature.ends_with("Enable") || feature.ends_with("Mode") {
        2
    } else {
        3
    };
    (group, 0, feature.into())
}

/// Sort values into apply order.
pub fn sort_values(values: &mut [SetupValue]) {
    values.sort_by(|a, b| {
        order_key(&a.feature, Some(&a.value.text()))
            .cmp(&order_key(&b.feature, Some(&b.value.text())))
            .then_with(|| a.at.cmp(&b.at))
    });
}

/// A disruptive change a person confirmed from a reviewed diff: a feature
/// name, which confirms only a value without banks, or the feature and
/// the banks it is written at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Confirmed {
    Name(String),
    At(ConfirmedAt),
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmedAt {
    pub feature: String,
    #[serde(default)]
    pub at: Vec<Bank>,
}
impl Confirmed {
    /// The confirmation for one change of a diff (`{feature, at?}`).
    pub fn of_change(change: &serde_json::Value) -> Option<Self> {
        let feature = change["feature"].as_str()?.to_owned();
        let at = if change["at"].is_null() {
            Vec::new()
        } else {
            serde_json::from_value(change["at"].clone()).ok()?
        };
        Some(Self::At(ConfirmedAt { feature, at }))
    }
    pub fn feature(&self) -> &str {
        match self {
            Self::Name(feature) => feature,
            Self::At(confirmed) => &confirmed.feature,
        }
    }
    /// Whether this confirms writing `feature` at exactly `at`.
    pub fn confirms(&self, feature: &str, at: &[Bank]) -> bool {
        match self {
            Self::Name(name) => name == feature && at.is_empty(),
            Self::At(confirmed) => confirmed.feature == feature && confirmed.at == at,
        }
    }
    pub fn validate(&self) -> Result<()> {
        crate::session::validate_feature(self.feature())?;
        if let Self::At(confirmed) = self {
            ensure!(
                confirmed.at.len() <= MAX_BANKS,
                "{} is addressed by more than {MAX_BANKS} selectors",
                confirmed.feature
            );
            for bank in &confirmed.at {
                validate_bank(&confirmed.feature, bank)?;
            }
        }
        Ok(())
    }
}

/// Setup names make their file names, so they avoid names some file
/// systems reserve.
pub fn validate_name(name: &str) -> Result<()> {
    const RULE: &str = "Setup names are 1-64 letters, digits, '.', '-' and '_', cannot start or end with '.', and cannot be a reserved Windows name such as con or nul";
    // The character set first: the reserved-name check below slices bytes.
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            && !name.starts_with('.')
            && !name.ends_with('.'),
        RULE
    );
    let stem = name.split('.').next().unwrap_or_default();
    let reserved = ["con", "prn", "aux", "nul"]
        .iter()
        .any(|r| stem.eq_ignore_ascii_case(r))
        || (stem.len() == 4
            && ["com", "lpt"]
                .iter()
                .any(|p| stem.get(..3).is_some_and(|s| s.eq_ignore_ascii_case(p)))
            && stem.as_bytes().get(3).is_some_and(u8::is_ascii_digit));
    ensure!(!reserved, RULE);
    Ok(())
}

fn validate_text(what: &str, text: &str) -> Result<()> {
    ensure!(
        text.len() <= MAX_STRING,
        "{what} is longer than {MAX_STRING} bytes"
    );
    ensure!(
        !text.chars().any(char::is_control),
        "{what} contains control characters"
    );
    Ok(())
}
fn validate_bank(feature: &str, bank: &Bank) -> Result<()> {
    crate::session::validate_feature(&bank.selector)?;
    ensure!(
        bank.selector.ends_with("Selector"),
        "{feature} is addressed by {}, which is not a selector",
        bank.selector
    );
    ensure!(
        !crate::tools::risk::managed_name(&bank.selector),
        "{} is managed by Capturefab and cannot address a setup value",
        bank.selector
    );
    validate_text(&format!("{} value", bank.selector), &bank.value)
}

/// Check a setup without a camera: limits, names and values. Node kinds are
/// checked by the camera worker.
pub fn validate(setup: &Setup) -> Result<()> {
    ensure!(
        setup.version == 1,
        "setup version {} is not supported; this Capturefab reads version 1",
        setup.version
    );
    validate_name(&setup.name)?;
    if let Some(note) = &setup.note {
        ensure!(
            note.chars().count() <= MAX_NOTE,
            "a setup note is at most {MAX_NOTE} characters"
        );
    }
    ensure!(
        !setup.values.is_empty(),
        "setup {} has no values to restore",
        setup.name
    );
    ensure!(
        setup.values.len() <= MAX_SETUP_VALUES,
        "a setup holds at most {MAX_SETUP_VALUES} values"
    );
    ensure!(
        setup.skipped.len() <= MAX_SKIPPED,
        "a setup records at most {MAX_SKIPPED} skipped values"
    );
    for (what, text) in [
        ("saved_at", &setup.saved_at),
        ("camera vendor", &setup.camera.vendor),
        ("camera model", &setup.camera.model),
    ] {
        validate_text(what, text)?;
    }
    for text in [&setup.camera.serial, &setup.camera.id]
        .into_iter()
        .flatten()
    {
        validate_text("camera identity", text)?;
    }
    if let Some(auto) = &setup.auto {
        ensure!(
            auto.balance.is_finite() && (0.0..=1.0).contains(&auto.balance),
            "auto balance must be a finite number from 0 to 1"
        );
    }
    let mut seen = BTreeSet::new();
    for value in &setup.values {
        crate::session::validate_feature(&value.feature)?;
        ensure!(
            !crate::tools::risk::managed_name(&value.feature),
            "Capturefab manages {}; a setup cannot restore it",
            value.feature
        );
        if let Scalar::String(text) = &value.value {
            validate_text(&value.feature, text)?;
        }
        if let Scalar::Number(number) = &value.value {
            ensure!(
                number.as_f64().is_some_and(f64::is_finite),
                "{} must be a finite number",
                value.feature
            );
        }
        ensure!(
            value.at.len() <= MAX_BANKS,
            "{} is addressed by more than {MAX_BANKS} selectors",
            value.feature
        );
        let mut selectors = BTreeSet::new();
        for bank in &value.at {
            validate_bank(&value.feature, bank)?;
            ensure!(
                selectors.insert(bank.selector.as_str()),
                "{} names {} twice",
                value.feature,
                bank.selector
            );
        }
        ensure!(
            seen.insert((value.feature.as_str(), &value.at)),
            "{} appears twice at the same bank",
            value.feature
        );
    }
    for skipped in &setup.skipped {
        validate_text("skipped feature", &skipped.feature)?;
        validate_text("skip reason", &skipped.reason)?;
        ensure!(skipped.at.len() <= MAX_BANKS, "too many selectors");
        for bank in &skipped.at {
            validate_text("skipped bank", &bank.selector)?;
            validate_text("skipped bank", &bank.value)?;
        }
    }
    ensure!(
        serde_json::to_vec(setup)?.len() <= MAX_SETUP_BYTES,
        "a setup is at most 256 KiB"
    );
    Ok(())
}

/// Parse a setup document strictly and validate it. The `--json` envelope
/// `setup show NAME --json` prints, `{"version":1,"ok":true,"result":DOCUMENT}`,
/// is accepted too, so its output can be imported or applied as is.
pub fn parse(bytes: &[u8]) -> Result<Setup> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Envelope {
        version: u32,
        ok: bool,
        result: serde_json::Value,
    }
    ensure!(bytes.len() <= MAX_SETUP_BYTES, "a setup is at most 256 KiB");
    let setup = match serde_json::from_slice::<Setup>(bytes) {
        Ok(setup) => setup,
        Err(error) => match serde_json::from_slice::<Envelope>(bytes) {
            Ok(Envelope {
                version: 1,
                ok: true,
                result,
            }) => serde_json::from_value(result).context("not a valid setup document")?,
            _ => return Err(anyhow::Error::new(error).context("not a valid setup document")),
        },
    };
    validate(&setup)?;
    Ok(setup)
}

/// Read and validate a setup file, reading at most `MAX_SETUP_BYTES`.
pub fn read_file(path: &Path) -> Result<Setup> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .with_context(|| format!("open {}", path.display()))?
        .take(MAX_SETUP_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    ensure!(
        bytes.len() <= MAX_SETUP_BYTES,
        "{} is larger than 256 KiB; it is not a setup",
        path.display()
    );
    parse(&bytes).with_context(|| format!("read setup {}", path.display()))
}

/// "sha256:" over the compact JSON of the document.
pub fn digest(setup: &Setup) -> String {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(setup).unwrap_or_default();
    let hash = Sha256::digest(&bytes);
    let mut text = String::with_capacity(71);
    text.push_str("sha256:");
    for byte in hash {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}
pub fn validate_digest(digest: &str) -> Result<()> {
    ensure!(
        digest.len() == 71
            && digest.starts_with("sha256:")
            && digest[7..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "setup digest must be sha256: followed by 64 lowercase hexadecimal characters"
    );
    Ok(())
}

/// Build a setup from what a camera worker read. Values go in apply order.
pub fn from_read(name: &str, note: Option<String>, mut reading: Reading) -> Result<Setup> {
    validate_name(name)?;
    if reading.values.is_empty() {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            "This camera has no settings a setup can save.",
        ));
    }
    sort_values(&mut reading.values);
    reading.values.truncate(MAX_SETUP_VALUES);
    reading.skipped.truncate(MAX_SKIPPED);
    let setup = Setup {
        version: 1,
        name: name.into(),
        saved_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        note: note.filter(|note| !note.trim().is_empty()),
        camera: reading.camera,
        auto: reading.auto,
        values: reading.values,
        skipped: reading.skipped,
    };
    validate(&setup)?;
    Ok(setup)
}

/// The name closest to `name` among plausible typos, for "did you mean".
fn closest<'a>(name: &str, names: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    names
        .into_iter()
        .filter(|candidate| crate::hint::is_close(name, candidate))
        .min_by_key(|candidate| crate::hint::edit_distance(name, candidate))
}

/// A setup name was taken and `replace` was not given.
#[derive(Debug)]
pub struct NameTaken(pub String);
impl std::fmt::Display for NameTaken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "A setup named {} exists; pass --replace to overwrite it",
            self.0
        )
    }
}
impl std::error::Error for NameTaken {}

/// The setups directory: one `<lowercase name>.json` per setup.
pub struct Store {
    dir: PathBuf,
}
impl Store {
    /// The store in this user's state directory.
    pub fn session() -> Self {
        Self::at(crate::ipc::state_dir().join("setups"))
    }
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{}.json", name.to_ascii_lowercase()))
    }
    fn lock(&self) -> Result<fs::File> {
        use fs2::FileExt;
        crate::ipc::ensure_private_dir(&self.dir)?;
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("setups.lock"))?;
        lock.lock_exclusive()?;
        Ok(lock)
    }
    fn files(&self) -> Result<Vec<PathBuf>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut files = Vec::new();
        for entry in entries {
            let path = entry?.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.ends_with(".json") && !name.starts_with('.') {
                files.push(path);
            }
        }
        files.sort();
        Ok(files)
    }
    /// Saved setups by name; unreadable files are left out.
    pub fn list(&self) -> Result<Vec<Summary>> {
        #[derive(Deserialize)]
        struct Head {
            name: String,
            saved_at: String,
            #[serde(default)]
            note: Option<String>,
            camera: SavedCamera,
            values: Vec<serde::de::IgnoredAny>,
        }
        let mut summaries = Vec::new();
        for path in self.files()? {
            let mut bytes = Vec::new();
            let Ok(file) = fs::File::open(&path) else {
                continue;
            };
            if file
                .take(MAX_SETUP_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .is_err()
                || bytes.len() > MAX_SETUP_BYTES
            {
                continue;
            }
            let Ok(head) = serde_json::from_slice::<Head>(&bytes) else {
                continue;
            };
            summaries.push(Summary {
                name: head.name,
                transport: head.camera.transport,
                vendor: head.camera.vendor,
                model: head.camera.model,
                serial: head.camera.serial,
                saved_at: head.saved_at,
                values: head.values.len(),
                note: head.note,
            });
        }
        summaries.sort_by_key(|summary| summary.name.to_ascii_lowercase());
        Ok(summaries)
    }
    /// The saved setup, or an `unavailable` error suggesting a close name.
    pub fn get(&self, name: &str) -> Result<Setup> {
        validate_name(name)?;
        let path = self.path(name);
        if !path.exists() {
            let names = self.list().unwrap_or_default();
            let suggestion = closest(name, names.iter().map(|s| s.name.as_str()))
                .map(|close| format!(" Did you mean {close}?"))
                .unwrap_or_default();
            return Err(Categorized::error(
                ErrorKind::Unavailable,
                format!("No saved setup named {name}.{suggestion} See capturefab setup list"),
            ));
        }
        read_file(&path)
    }
    /// Whether `name` can be saved: refuses a taken name unless `replace`,
    /// and a new name when the store is full. `save` checks again under
    /// its lock; this lets a command refuse before reading a camera.
    pub fn check_free(&self, name: &str, replace: bool) -> Result<()> {
        let path = self.path(name);
        if path.exists() {
            if !replace {
                let existing = read_file(&path)
                    .map(|existing| existing.name)
                    .unwrap_or_else(|_| name.to_owned());
                return Err(NameTaken(existing).into());
            }
        } else {
            ensure!(
                self.files()?.len() < MAX_SETUPS,
                "{MAX_SETUPS} setups are saved; delete one first (capturefab setup delete NAME)"
            );
        }
        Ok(())
    }
    /// Save `setup`, refusing an existing name unless `replace`. Returns
    /// the file written.
    pub fn save(&self, setup: &Setup, replace: bool) -> Result<PathBuf> {
        validate(setup)?;
        let _lock = self.lock()?;
        self.check_free(&setup.name, replace)?;
        let path = self.path(&setup.name);
        self.write(&path, setup)?;
        Ok(path)
    }
    fn write(&self, path: &Path, setup: &Setup) -> Result<()> {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("setup file name")?;
        let temp = self
            .dir
            .join(format!(".{file_name}.{}.tmp", std::process::id()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let result = (|| -> Result<()> {
            let bytes = serde_json::to_vec_pretty(setup)?;
            ensure!(bytes.len() <= MAX_SETUP_BYTES, "a setup is at most 256 KiB");
            let _ = fs::remove_file(&temp);
            let mut output = options.open(&temp)?;
            output.write_all(&bytes)?;
            output.write_all(b"\n")?;
            output.sync_all()?;
            fs::rename(&temp, path)?;
            #[cfg(unix)]
            if let Ok(dir) = fs::File::open(&self.dir) {
                let _ = dir.sync_all();
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
    /// Delete a saved setup.
    pub fn remove(&self, name: &str) -> Result<Setup> {
        let _lock = self.lock()?;
        let setup = self.get(name)?;
        fs::remove_file(self.path(name))?;
        Ok(setup)
    }
    /// Save a setup document from a file, optionally under another name.
    pub fn import(&self, path: &Path, rename: Option<&str>, replace: bool) -> Result<Setup> {
        let mut setup = read_file(path)?;
        if let Some(name) = rename {
            validate_name(name)?;
            setup.name = name.into();
        }
        self.save(&setup, replace)?;
        Ok(setup)
    }
}

/// Replace a setup name in `apply_setup` or `setup_diff` with the saved
/// document, so the camera worker never reads the state directory. A given
/// digest must still match what was reviewed.
pub fn resolve(command: crate::session::SessionCommand) -> Result<crate::session::SessionCommand> {
    use crate::session::SessionCommand;
    let load = |name: &Option<String>, setup: Option<Setup>| -> Result<Option<Setup>> {
        match (name, setup) {
            (_, Some(setup)) => Ok(Some(setup)),
            (Some(name), None) => Store::session().get(name).map(Some),
            (None, None) => bail!("name a setup or pass one inline"),
        }
    };
    Ok(match command {
        SessionCommand::SetupDiff {
            name,
            setup,
            allow_other_model,
        } => SessionCommand::SetupDiff {
            setup: load(&name, setup)?,
            name: None,
            allow_other_model,
        },
        SessionCommand::ApplySetup {
            name,
            setup,
            digest: expected,
            allow_other_model,
            strict,
            confirmed_disruptive,
        } => {
            let setup = load(&name, setup)?;
            if let (Some(expected), Some(setup)) = (&expected, &setup)
                && *expected != digest(setup)
            {
                return Err(Categorized::error(
                    ErrorKind::OperationFailed,
                    format!(
                        "Setup {} changed after it was reviewed; preview it again",
                        setup.name
                    ),
                ));
            }
            SessionCommand::ApplySetup {
                name: None,
                setup,
                digest: expected,
                allow_other_model,
                strict,
                confirmed_disruptive,
            }
        }
        command => command,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionCommand;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "capturefab-setup-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }
    fn info(vendor: &str, model: &str, serial: &str) -> CameraInfo {
        CameraInfo {
            id: format!("gige:{serial}"),
            transport: Transport::GigE,
            vendor: vendor.into(),
            model: model.into(),
            serial: serial.into(),
            address: Some("192.168.1.20".into()),
            profile: Default::default(),
        }
    }
    fn value(feature: &str, value: serde_json::Value) -> SetupValue {
        SetupValue {
            feature: feature.into(),
            value: Scalar::from_value(&value).unwrap(),
            at: Vec::new(),
        }
    }
    fn setup(name: &str) -> Setup {
        Setup {
            version: 1,
            name: name.into(),
            saved_at: "2026-10-25T14:03:11Z".into(),
            note: None,
            camera: SavedCamera::from_info(&info("Basler", "acA1920-40gm", "23456789")),
            auto: None,
            values: vec![
                value("Width", serde_json::json!(1920)),
                value("ExposureTime", serde_json::json!(5000.5)),
            ],
            skipped: Vec::new(),
        }
    }

    #[test]
    fn names_reject_reserved_dots_and_length() {
        for good in [
            "bench",
            "line-2.mono",
            "a",
            "Con1",
            "comX",
            "com10",
            &"x".repeat(64),
        ] {
            validate_name(good).unwrap_or_else(|e| panic!("{good}: {e}"));
        }
        for bad in [
            "",
            "con",
            "NUL.json",
            "com1",
            "LPT9",
            "aux.x",
            ".bench",
            "bench.",
            "a b",
            "a/b",
            "1€",
            "é",
            "€€",
            "com€",
            &"x".repeat(65),
        ] {
            let error = validate_name(bad).unwrap_err().to_string();
            assert!(error.starts_with("Setup names are"), "{bad}: {error}");
        }
    }

    #[test]
    fn documents_round_trip_strictly() {
        let setup = setup("bench");
        let bytes = serde_json::to_vec(&setup).unwrap();
        assert_eq!(parse(&bytes).unwrap(), setup);
        let mut json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        json["extra"] = serde_json::json!(1);
        assert!(parse(&serde_json::to_vec(&json).unwrap()).is_err());
        let mut json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        json["version"] = serde_json::json!(2);
        let error = format!(
            "{:#}",
            parse(&serde_json::to_vec(&json).unwrap()).unwrap_err()
        );
        assert!(error.contains("version 2"), "{error}");
        // The --json envelope of setup show is accepted; its document is
        // still strict, and a failed envelope is not.
        let envelope = serde_json::json!({"version":1,"ok":true,"result":setup});
        assert_eq!(
            parse(&serde_json::to_vec(&envelope).unwrap()).unwrap(),
            setup
        );
        for bad in [
            serde_json::json!({"version":1,"ok":false,"result":setup}),
            serde_json::json!({"version":2,"ok":true,"result":setup}),
            serde_json::json!({"version":1,"ok":true,"result":setup,"extra":1}),
        ] {
            assert!(parse(&serde_json::to_vec(&bad).unwrap()).is_err(), "{bad}");
        }
        let mut inner = serde_json::to_value(&setup).unwrap();
        inner["extra"] = serde_json::json!(1);
        let error = format!(
            "{:#}",
            parse(
                &serde_json::to_vec(&serde_json::json!({"version":1,"ok":true,"result":inner}))
                    .unwrap()
            )
            .unwrap_err()
        );
        assert!(error.contains("not a valid setup document"), "{error}");
        for bad in [
            serde_json::json!([1]),
            serde_json::json!({"a":1}),
            serde_json::Value::Null,
        ] {
            let mut json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            json["values"][0]["value"] = bad;
            assert!(parse(&serde_json::to_vec(&json).unwrap()).is_err());
        }
    }

    #[test]
    fn floats_survive_a_write_and_parse_exactly() {
        for float in [
            0.1f64 + 0.2,
            f64::from_bits(4999.8f64.to_bits() + 1),
            1.0 / 3.0,
            1e-300,
            123456.789,
        ] {
            let mut setup = setup("bench");
            setup.values[1] = value("ExposureTime", serde_json::json!(float));
            let envelope = serde_json::json!({"version":1,"ok":true,"result":setup});
            for bytes in [
                serde_json::to_vec_pretty(&setup).unwrap(),
                serde_json::to_vec(&envelope).unwrap(),
            ] {
                let parsed = parse(&bytes).unwrap();
                let Scalar::Number(number) = &parsed.values[1].value else {
                    panic!()
                };
                assert_eq!(number.as_f64().unwrap().to_bits(), float.to_bits());
            }
        }
    }

    #[test]
    fn limits_hold_and_a_maximal_setup_fits_one_request() {
        let mut big = setup("big");
        big.values = (0..MAX_SETUP_VALUES)
            .map(|i| value(&format!("Feature{i}"), serde_json::json!("x".repeat(400))))
            .collect();
        validate(&big).unwrap();
        let line = serde_json::to_vec(&SessionCommand::ApplySetup {
            name: None,
            setup: Some(big.clone()),
            digest: Some(digest(&big)),
            allow_other_model: true,
            strict: true,
            confirmed_disruptive: Some(vec![Confirmed::Name("TriggerMode".into()); 8]),
        })
        .unwrap();
        assert!(line.len() < 1024 * 1024);
        big.values.push(value("One", serde_json::json!(1)));
        assert!(validate(&big).is_err());
        let mut huge = setup("huge");
        huge.values = (0..MAX_SETUP_VALUES)
            .map(|i| value(&format!("Feature{i}"), serde_json::json!("x".repeat(1000))))
            .collect();
        assert!(format!("{:#}", validate(&huge).unwrap_err()).contains("256 KiB"));
        let mut note = setup("note");
        note.note = Some("n".repeat(MAX_NOTE + 1));
        assert!(validate(&note).is_err());
    }

    #[test]
    fn managed_features_and_bad_banks_are_refused() {
        for feature in [
            "GevSCDA",
            "UserSetSelector",
            "DeviceUserID",
            "FileAccessLength",
        ] {
            let mut s = setup("bench");
            s.values.push(value(feature, serde_json::json!(1)));
            assert!(validate(&s).is_err(), "{feature}");
        }
        for selector in ["UserSetSelector", "Width", "Gain Selector"] {
            let mut s = setup("bench");
            s.values[0].at = vec![Bank {
                selector: selector.into(),
                value: "A".into(),
            }];
            assert!(validate(&s).is_err(), "{selector}");
        }
        let mut s = setup("bench");
        s.values.push(s.values[0].clone());
        assert!(validate(&s).is_err());
    }

    #[test]
    fn apply_order_puts_geometry_first_and_selectors_last() {
        let mut values = [
            "GainSelector",
            "ExposureAuto",
            "Gain",
            "TriggerMode",
            "OffsetX",
            "Width",
            "PixelFormat",
            "BinningHorizontal",
            "AcquisitionFrameRateEnable",
            "BlackLevel",
        ]
        .map(|f| value(f, serde_json::json!(1)));
        sort_values(&mut values);
        assert_eq!(
            values
                .iter()
                .map(|v| v.feature.as_str())
                .collect::<Vec<_>>(),
            [
                "PixelFormat",
                "BinningHorizontal",
                "Width",
                "OffsetX",
                "BlackLevel",
                "Gain",
                "AcquisitionFrameRateEnable",
                "TriggerMode",
                "ExposureAuto",
                "GainSelector"
            ]
        );
    }

    #[test]
    fn switches_disarm_first_and_arm_after_their_sources() {
        let mut values = [
            ("TriggerMode", "On"),
            ("TriggerSource", "Software"),
            ("TriggerActivation", "RisingEdge"),
            ("LineMode", "Output"),
            ("LineSource", "ExposureActive"),
            ("UserOutputValue", "true"),
            ("ExposureMode", "Timed"),
            ("CounterEnable", "false"),
            ("LineSelector", "Line1"),
        ]
        .map(|(f, v)| value(f, serde_json::json!(v)));
        sort_values(&mut values);
        assert_eq!(
            values
                .iter()
                .map(|v| v.feature.as_str())
                .collect::<Vec<_>>(),
            [
                "CounterEnable",
                "ExposureMode",
                "LineSource",
                "TriggerActivation",
                "TriggerSource",
                "UserOutputValue",
                "LineMode",
                "TriggerMode",
                "LineSelector"
            ]
        );
        // Disarming switches go before every value they gate.
        assert!(order_key("TriggerMode", Some("Off")) < order_key("TriggerSource", Some("Line1")));
        assert!(order_key("LineMode", Some("Input")) < order_key("LineSource", Some("Off")));
        assert!(order_key("LineMode", Some("Output")) > order_key("LineSource", Some("Off")));
        assert!(
            order_key("AcquisitionFrameRateEnable", Some("true"))
                > order_key("AcquisitionFrameRate", Some("30"))
        );
    }

    #[test]
    fn matching_compares_identity_not_transport() {
        let saved = SavedCamera::from_info(&info("Basler", "acA1920-40gm", "23456789"));
        let mut same = info(" basler", "ACA1920-40GM ", "23456789 ");
        same.transport = Transport::Usb3;
        assert_eq!(matching(&saved, &same).0, Match::SameCamera);
        let (other, warnings) = matching(&saved, &info("Basler", "acA1920-40gm", "1"));
        assert_eq!(other, Match::SameModel);
        assert_eq!(
            warnings,
            ["Saved from another acA1920-40gm (serial 23456789)."]
        );
        // Another model carries one note naming both, and no CLI flag.
        assert_eq!(
            matching(&saved, &info("FLIR", "BFS-U3-51S5M", "1")),
            (
                Match::OtherModel,
                vec![
                    "Saved from Basler acA1920-40gm; this camera is FLIR BFS-U3-51S5M; only settings both share apply."
                        .to_owned()
                ]
            )
        );
        let rtsp = CameraInfo {
            id: "media:0123abcd".into(),
            transport: Transport::Media,
            vendor: "FFmpeg".into(),
            model: "RTSP input".into(),
            serial: "0123abcd".into(),
            address: Some("rtsp://user:pass@cam.example:554/live".into()),
            profile: Default::default(),
        };
        let saved_rtsp = SavedCamera::from_info(&rtsp);
        let mut other_rtsp = rtsp.clone();
        other_rtsp.serial = "ffff".into();
        other_rtsp.id = "media:ffff".into();
        let (unknown, warnings) = matching(&saved_rtsp, &other_rtsp);
        assert_eq!(unknown, Match::UnknownSource);
        assert_eq!(
            warnings,
            ["Saved from an RTSP input; Capturefab cannot tell whether this is the same stream."]
        );
        assert_eq!(matching(&saved_rtsp, &same).0, Match::OtherModel);
        let (other, warnings) = matching(&saved, &rtsp);
        assert_eq!(other, Match::OtherModel);
        assert_eq!(
            warnings,
            [
                "Saved from Basler acA1920-40gm; this camera is FFmpeg RTSP input; only settings both share apply."
            ]
        );
        let hub = CameraInfo {
            id: "hub:home/camera.front".into(),
            ..rtsp.clone()
        };
        let saved_hub = SavedCamera::from_info(&hub);
        assert_eq!(saved_hub.id.as_deref(), Some("hub:home/camera.front"));
        assert_eq!(matching(&saved_hub, &hub).0, Match::SameCamera);
        let other_hub = CameraInfo {
            id: "hub:home/camera.back".into(),
            ..rtsp
        };
        let (other, warnings) = matching(&saved_hub, &other_hub);
        assert_eq!(other, Match::OtherModel);
        assert_eq!(
            warnings,
            [
                "Saved from hub:home/camera.front; this camera is hub:home/camera.back; only settings both share apply."
            ]
        );
    }

    #[test]
    fn media_setups_store_no_address_host_or_hash() {
        let rtsp = CameraInfo {
            id: "media:9f8e7d6c".into(),
            transport: Transport::Media,
            vendor: "FFmpeg".into(),
            model: "RTSP input".into(),
            serial: "9f8e7d6c".into(),
            address: Some("rtsp://user:pass@cam.example:554/live".into()),
            profile: Default::default(),
        };
        let mut setup = setup("rtsp");
        setup.camera = SavedCamera::from_info(&rtsp);
        let text = serde_json::to_string(&setup).unwrap();
        for secret in [
            "pass",
            "cam.example",
            "live",
            "media:",
            "9f8e7d6c",
            "192.168",
        ] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        let gige = serde_json::to_string(&setup_with_info(info("A", "B", "C"))).unwrap();
        assert!(!gige.contains("192.168"));
    }
    fn setup_with_info(info: CameraInfo) -> Setup {
        Setup {
            camera: SavedCamera::from_info(&info),
            ..setup("x")
        }
    }

    #[test]
    fn the_store_saves_lists_replaces_across_case_and_deletes() {
        let store = Store::at(temp("store"));
        assert!(store.list().unwrap().is_empty());
        let path = store.save(&setup("bench"), false).unwrap();
        assert!(path.ends_with("bench.json"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(store.dir()).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let taken = store.save(&setup("BENCH"), false).unwrap_err();
        assert!(taken.is::<NameTaken>());
        assert_eq!(
            taken.to_string(),
            "A setup named bench exists; pass --replace to overwrite it"
        );
        // The same refusal, before any camera is read; replace passes.
        let early = store.check_free("Bench", false).unwrap_err();
        assert!(early.is::<NameTaken>());
        assert_eq!(early.to_string(), taken.to_string());
        store.check_free("bench", true).unwrap();
        store.check_free("other", false).unwrap();
        store.save(&setup("BENCH"), true).unwrap();
        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "BENCH");
        assert_eq!(list[0].values, 2);
        assert_eq!(store.get("bench").unwrap().name, "BENCH");
        let missing = store.get("bnch").unwrap_err();
        assert_eq!(
            crate::types::error_transport_code(&missing),
            Some("unavailable")
        );
        assert_eq!(
            missing.to_string(),
            "No saved setup named bnch. Did you mean BENCH? See capturefab setup list"
        );
        store.remove("Bench").unwrap();
        assert!(store.list().unwrap().is_empty());
        let names = fs::read_dir(store.dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["setups.lock"]);
        let _ = fs::remove_dir_all(store.dir());
    }

    #[test]
    fn a_full_store_refuses_a_new_name_before_saving() {
        let store = Store::at(temp("full"));
        crate::ipc::ensure_private_dir(store.dir()).unwrap();
        for index in 0..MAX_SETUPS {
            fs::write(store.dir().join(format!("s{index}.json")), b"{}").unwrap();
        }
        let full = store.check_free("new", false).unwrap_err();
        assert!(full.to_string().contains("setups are saved; delete one"));
        // An existing name can still be replaced.
        store.check_free("s1", true).unwrap();
        let _ = fs::remove_dir_all(store.dir());
    }

    #[test]
    fn concurrent_saves_both_land_and_failures_leave_no_temp_file() {
        let store = std::sync::Arc::new(Store::at(temp("concurrent")));
        let threads = ["one", "two"].map(|name| {
            let store = store.clone();
            std::thread::spawn(move || store.save(&setup(name), false).unwrap())
        });
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(store.list().unwrap().len(), 2);
        // A directory where the file would go makes the rename fail.
        fs::create_dir(store.dir().join("blocked.json")).unwrap();
        assert!(store.save(&setup("blocked"), true).is_err());
        let temps = fs::read_dir(store.dir())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            })
            .count();
        assert_eq!(temps, 0);
        let _ = fs::remove_dir_all(store.dir());
    }

    #[test]
    fn imports_rename_and_digests_are_stable() {
        let dir = temp("import");
        let store = Store::at(dir.join("setups"));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("bench.json");
        let original = setup("bench");
        fs::write(&file, serde_json::to_vec_pretty(&original).unwrap()).unwrap();
        let copy = store.import(&file, Some("copy"), false).unwrap();
        assert_eq!(copy.name, "copy");
        assert!(
            store
                .import(&file, Some("copy"), false)
                .unwrap_err()
                .is::<NameTaken>()
        );
        assert!(store.import(&file, Some("con"), false).is_err());
        assert_eq!(digest(&original), digest(&original.clone()));
        assert_ne!(digest(&original), digest(&copy));
        validate_digest(&digest(&original)).unwrap();
        assert!(validate_digest("sha256:abc").is_err());
        fs::write(&file, vec![b' '; MAX_SETUP_BYTES + 1]).unwrap();
        assert!(format!("{:#}", read_file(&file).unwrap_err()).contains("256 KiB"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_reading_with_nothing_to_save_is_unsupported() {
        let reading = Reading {
            camera: setup("x").camera,
            auto: None,
            values: Vec::new(),
            skipped: Vec::new(),
        };
        let error = from_read("bench", None, reading).unwrap_err();
        assert_eq!(
            crate::types::error_transport_code(&error),
            Some("unsupported")
        );
        assert_eq!(
            error.to_string(),
            "This camera has no settings a setup can save."
        );
    }
}
