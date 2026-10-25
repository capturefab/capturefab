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

/// Why `setup list` cannot read a file in the setups folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    /// Damaged, edited by hand, or named differently from its file.
    Unreadable,
    /// Written by a newer Capturefab.
    NewerVersion,
}

/// One row of `setup list`. A file the store cannot read is listed under
/// its file name with `status` and `reason`, and no camera, date or values,
/// so it can be seen and deleted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
    pub vendor: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    pub saved_at: String,
    pub values: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<FileStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
impl Summary {
    /// The row for a file the store cannot read.
    pub fn unreadable(name: &str, status: FileStatus, reason: &str) -> Self {
        Self {
            name: name.into(),
            transport: None,
            vendor: String::new(),
            model: String::new(),
            serial: None,
            saved_at: String::new(),
            values: 0,
            note: None,
            status: Some(status),
            reason: Some(printable(reason)),
        }
    }
    /// True for a setup the store can read.
    pub fn readable(&self) -> bool {
        self.status.is_none()
    }
}

/// `text` without control characters, bounded, for a reason taken from a
/// file the store cannot read.
fn printable(text: &str) -> String {
    const MAX_REASON: usize = 240;
    let mut text = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>();
    if text.chars().count() > MAX_REASON {
        text = text.chars().take(MAX_REASON - 1).collect::<String>() + "…";
    }
    text
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
            (Match::OtherModel, Vec::new())
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
            (Match::OtherModel, Vec::new())
        };
    }
    if !(same(&saved.vendor, &live.vendor) && same(&saved.model, &live.model)) {
        return (Match::OtherModel, Vec::new());
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

/// Apply order: geometry and format first, then enabling switches and
/// modes, other values alphabetically, auto flags, and selectors last, so
/// the inspector shows the saved bank afterwards.
pub fn order_key(feature: &str) -> (u8, u8, String) {
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
    let group = if feature.ends_with("Selector") {
        4
    } else if feature.ends_with("Auto") {
        3
    } else if feature.ends_with("Enable") || feature.ends_with("Mode") {
        1
    } else {
        2
    };
    (group, 0, feature.into())
}

/// Sort values into apply order.
pub fn sort_values(values: &mut [SetupValue]) {
    values.sort_by(|a, b| {
        order_key(&a.feature)
            .cmp(&order_key(&b.feature))
            .then_with(|| a.at.cmp(&b.at))
    });
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

/// Check a setup note: bounded, on one line, with no terminal escapes.
pub fn validate_note(note: &str) -> Result<()> {
    ensure!(
        note.chars().count() <= MAX_NOTE,
        "a setup note is at most {MAX_NOTE} characters"
    );
    ensure!(
        !note.chars().any(char::is_control),
        "a setup note cannot contain control characters such as line breaks or escapes"
    );
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
    check_version(setup.version.into())?;
    validate_name(&setup.name)?;
    if let Some(note) = &setup.note {
        validate_note(note)?;
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
        stored_bytes(setup)?.len() <= MAX_SETUP_BYTES,
        "a setup is at most 256 KiB"
    );
    Ok(())
}

/// The bytes the store writes for `setup`. Sizes are checked on these, the
/// largest form, so every valid setup can be saved, sent and imported.
fn stored_bytes(setup: &Setup) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(setup)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// A document version this Capturefab cannot read.
#[derive(Debug)]
pub struct NewerVersion(pub u64);
impl std::fmt::Display for NewerVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "setup version {} was written by a newer Capturefab; this one reads version 1",
            self.0
        )
    }
}
impl std::error::Error for NewerVersion {}

fn check_version(version: u64) -> Result<()> {
    if version > 1 {
        return Err(NewerVersion(version).into());
    }
    ensure!(
        version == 1,
        "setup version {version} is not supported; this Capturefab reads version 1"
    );
    Ok(())
}

/// Parse a setup document strictly and validate it. The `--json` envelope
/// `setup show NAME --json` prints, `{"version":1,"ok":true,"result":DOCUMENT}`,
/// is accepted too, so its output can be imported or applied as is.
pub fn parse(bytes: &[u8]) -> Result<Setup> {
    ensure!(bytes.len() <= MAX_SETUP_BYTES, "a setup is at most 256 KiB");
    let value = serde_json::from_slice(bytes).context("not a valid setup document")?;
    from_value(value)
}

/// A setup document, or the `setup show NAME --json` envelope around one,
/// checked strictly and validated. A newer version is reported as such
/// before any field it may have added.
pub fn from_value(value: serde_json::Value) -> Result<Setup> {
    let setup = document(value)?;
    validate(&setup)?;
    Ok(setup)
}

/// `from_value` without `validate`, which session commands run later.
fn document(value: serde_json::Value) -> Result<Setup> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Envelope {
        version: u32,
        ok: bool,
        result: serde_json::Value,
    }
    let document = if value.get("ok").is_some() {
        match serde_json::from_value::<Envelope>(value) {
            Ok(Envelope {
                version: 1,
                ok: true,
                result,
            }) => result,
            _ => bail!(
                "not a valid setup document: pass the setup itself or the whole output of a successful capturefab setup show NAME --json (version 1, ok true)"
            ),
        }
    } else {
        value
    };
    if let Some(version) = document.get("version").and_then(serde_json::Value::as_u64) {
        check_version(version)?;
    }
    serde_json::from_value(document).context("not a valid setup document")
}

/// Deserialize an inline `setup` field, as `from_value` reads it: either
/// the document or the `setup show NAME --json` envelope around it. The
/// command's `validate` checks its values.
pub fn deserialize_inline<'de, D>(deserializer: D) -> Result<Option<Setup>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    match Option::<serde_json::Value>::deserialize(deserializer)? {
        None => Ok(None),
        Some(value) => document(value)
            .map(Some)
            .map_err(|error| D::Error::custom(format!("{error:#}"))),
    }
}

/// Read and validate a setup file, reading at most `MAX_SETUP_BYTES`.
pub fn read_file(path: &Path) -> Result<Setup> {
    parse(&read_bytes(path)?).with_context(|| format!("read setup {}", path.display()))
}

/// Read a file in the store. A note with control characters, which older
/// versions stored, is put on one line instead of making the setup
/// unreadable; new notes are refused by `validate_note`.
fn read_stored(path: &Path) -> Result<Setup> {
    let bytes = read_bytes(path)?;
    let mut setup =
        document(serde_json::from_slice(&bytes).context("not a valid setup document")?)?;
    if let Some(note) = &mut setup.note {
        *note = note
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
    }
    validate(&setup)?;
    Ok(setup)
}

/// A file's bytes, refusing one larger than any setup.
fn read_bytes(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .with_context(|| format!("open {}", path.display()))?
        .take(MAX_SETUP_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    ensure!(
        bytes.len() <= MAX_SETUP_BYTES,
        "{} is larger than 256 KiB; it is not a setup",
        file_name(path)
    );
    Ok(bytes)
}

/// The file name alone, for messages: the folder is shown elsewhere.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
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
    // Past the limit, keep room for the entry that counts the rest.
    let over = if reading.skipped.len() > MAX_SKIPPED {
        let keep = MAX_SKIPPED - 1;
        let over = reading.skipped.len() - keep;
        reading.skipped.truncate(keep);
        over
    } else {
        0
    };
    let mut setup = Setup {
        version: 1,
        name: name.into(),
        saved_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        note: note.filter(|note| !note.trim().is_empty()),
        camera: reading.camera,
        auto: reading.auto,
        values: reading.values,
        skipped: reading.skipped,
    };
    fit_skipped(&mut setup, over)?;
    validate(&setup)?;
    Ok(setup)
}

/// Drop skipped entries from the end until the setup fits its size limit,
/// noting how many were left out, with the `dropped` already left out, so a
/// large camera still saves.
fn fit_skipped(setup: &mut Setup, mut dropped: usize) -> Result<()> {
    let fits =
        |setup: &Setup| -> Result<bool> { Ok(stored_bytes(setup)?.len() <= MAX_SETUP_BYTES) };
    if dropped == 0 && fits(setup)? {
        return Ok(());
    }
    while !setup.skipped.is_empty() && !fits(setup)? {
        setup.skipped.pop();
        dropped += 1;
    }
    if !fits(setup)? {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            format!(
                "This camera's {} settings do not fit in one setup (at most {} KiB).",
                setup.values.len(),
                MAX_SETUP_BYTES / 1024
            ),
        ));
    }
    loop {
        setup.skipped.push(Skipped {
            feature: "(more)".into(),
            at: Vec::new(),
            reason: format!("{dropped} more skipped values not listed"),
        });
        if fits(setup)? {
            break;
        }
        // The note does not fit either: drop one more entry for it, or go
        // without it when nothing is left to drop.
        setup.skipped.pop();
        if setup.skipped.pop().is_none() {
            break;
        }
        dropped += 1;
    }
    Ok(())
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
pub struct NameTaken {
    /// The saved setup's name, or the requested one when its file cannot
    /// be read or holds another setup.
    pub name: String,
    /// Why the existing file is not a usable setup, if it is not.
    pub problem: Option<&'static str>,
}
impl std::fmt::Display for NameTaken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "A setup named {} exists{}; pass --replace to overwrite it",
            self.name,
            self.problem
                .map(|problem| format!(" but {problem}"))
                .unwrap_or_default()
        )
    }
}
impl std::error::Error for NameTaken {}

/// A file in the store holding a setup named differently: a copy or a
/// renamed file.
#[derive(Debug)]
struct Misnamed {
    file: String,
    holds: String,
    name: String,
}
impl std::fmt::Display for Misnamed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} holds a setup named {}, not {}",
            self.file, self.holds, self.name
        )
    }
}
impl std::error::Error for Misnamed {}

fn newer(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<NewerVersion>())
}

/// What `Store::remove` deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    /// The setup's name, or the file's when it could not be read.
    pub name: String,
    pub readable: bool,
}

/// A file's name without `.json`, when it could be a setup's.
fn stem(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?.strip_suffix(".json")?;
    validate_name(name).is_ok().then_some(name)
}

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
            // Only files a setup could be named: `list` shows these, and
            // the limit counts them.
            if stem(&path).is_some() {
                files.push(path);
            }
        }
        files.sort();
        Ok(files)
    }
    /// Saved setups by name. A file the store cannot read, or one holding
    /// a setup named differently from its file, is listed with a `status`
    /// and `reason` under its file name, so it can be deleted. Files whose
    /// names no setup could have are not setups and are left out.
    pub fn list(&self) -> Result<Vec<Summary>> {
        let mut summaries = Vec::new();
        for path in self.files()? {
            let Some(name) = stem(&path) else {
                continue;
            };
            summaries.push(match self.read_named(&path, name) {
                Ok(setup) => Summary {
                    name: setup.name,
                    transport: Some(setup.camera.transport),
                    vendor: setup.camera.vendor,
                    model: setup.camera.model,
                    serial: setup.camera.serial,
                    saved_at: setup.saved_at,
                    values: setup.values.len(),
                    note: setup.note,
                    status: None,
                    reason: None,
                },
                Err(error) => {
                    let status = if newer(&error) {
                        FileStatus::NewerVersion
                    } else {
                        FileStatus::Unreadable
                    };
                    Summary::unreadable(name, status, &format!("{:#}", error.root_cause()))
                }
            });
        }
        summaries.sort_by_key(|summary| summary.name.to_ascii_lowercase());
        Ok(summaries)
    }
    /// The setup in `path`, which `get(name)` opens, refusing one that is
    /// named differently: a copied or renamed file.
    /// Its messages name the file alone, so a listed reason stays short.
    fn read_named(&self, path: &Path, name: &str) -> Result<Setup> {
        if self.path(name).file_name() != path.file_name() {
            bail!(
                "{} has capital letters; Capturefab names setup files in lower case. Rename it to {}.json",
                file_name(path),
                name.to_ascii_lowercase()
            );
        }
        let setup = read_stored(path)?;
        if !setup.name.eq_ignore_ascii_case(name) {
            return Err(Misnamed {
                file: file_name(path),
                holds: printable(&setup.name),
                name: name.into(),
            }
            .into());
        }
        Ok(setup)
    }
    /// The saved setup, or an `unavailable` error suggesting a close name.
    pub fn get(&self, name: &str) -> Result<Setup> {
        validate_name(name)?;
        let path = self.path(name);
        if !path.exists() {
            return Err(self.missing(name));
        }
        self.read_named(&path, name)
            .map_err(|error| self.unusable(name, &path, error))
    }
    /// Why a saved setup's file cannot be used, and what to do about it.
    fn unusable(&self, name: &str, path: &Path, error: anyhow::Error) -> anyhow::Error {
        if newer(&error) {
            return Categorized::error(
                ErrorKind::Unsupported,
                format!(
                    "Setup {name} was saved by a newer Capturefab ({:#}); update Capturefab to use it",
                    error.root_cause()
                ),
            );
        }
        let quoted = crate::hint::shell_quote(name);
        let hint = if let Some(misnamed) = error.downcast_ref::<Misnamed>() {
            format!(
                "{misnamed}. Save it under this name with: capturefab setup import {} --name {quoted} --replace, or remove the copy with: capturefab setup delete {quoted}",
                crate::hint::shell_quote(&path.display().to_string())
            )
        } else {
            format!(
                "{error:#}. Save the camera again with: capturefab --camera ID setup save {quoted} --replace, or remove it with: capturefab setup delete {quoted}"
            )
        };
        Categorized::error(
            ErrorKind::Unavailable,
            format!("Setup {name} cannot be read: {hint}"),
        )
    }
    fn missing(&self, name: &str) -> anyhow::Error {
        let names = self.list().unwrap_or_default();
        let suggestion = closest(name, names.iter().map(|s| s.name.as_str()))
            .map(|close| format!(" Did you mean {close}?"))
            .unwrap_or_default();
        Categorized::error(
            ErrorKind::Unavailable,
            format!("No saved setup named {name}.{suggestion} See capturefab setup list"),
        )
    }
    /// Save `setup`, refusing an existing name unless `replace`. Returns
    /// the file written.
    pub fn save(&self, setup: &Setup, replace: bool) -> Result<PathBuf> {
        validate(setup)?;
        let _lock = self.lock()?;
        let path = self.path(&setup.name);
        if path.exists() {
            if !replace {
                // Name the setup the user asked for, unless the file holds
                // it under other capitals.
                let taken = match self.read_named(&path, &setup.name) {
                    Ok(existing) => NameTaken {
                        name: existing.name,
                        problem: None,
                    },
                    Err(error) => NameTaken {
                        name: setup.name.clone(),
                        problem: Some(if newer(&error) {
                            "was saved by a newer Capturefab"
                        } else {
                            "cannot be read"
                        }),
                    },
                };
                return Err(taken.into());
            }
        } else {
            ensure!(
                self.files()?.len() < MAX_SETUPS,
                "{MAX_SETUPS} setups are saved; delete one first (capturefab setup delete NAME)"
            );
        }
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
            let bytes = stored_bytes(setup)?;
            ensure!(bytes.len() <= MAX_SETUP_BYTES, "a setup is at most 256 KiB");
            let _ = fs::remove_file(&temp);
            let mut output = options.open(&temp)?;
            output.write_all(&bytes)?;
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
    /// Delete a saved setup's file without parsing it, so a damaged or
    /// newer file can be removed too. A file name with capital letters,
    /// which `list` shows as unreadable, is deleted when named exactly.
    pub fn remove(&self, name: &str) -> Result<Removed> {
        validate_name(name)?;
        let _lock = self.lock()?;
        let mut path = self.path(name);
        if !path.exists() {
            let exact = self.dir.join(format!("{name}.json"));
            if !exact.is_file() {
                return Err(self.missing(name));
            }
            path = exact;
        }
        let shown = self.read_named(&path, name).ok().map(|setup| setup.name);
        fs::remove_file(&path).with_context(|| format!("delete {}", path.display()))?;
        Ok(Removed {
            readable: shown.is_some(),
            name: shown.unwrap_or_else(|| name.into()),
        })
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
            confirmed_disruptive: Some(vec!["TriggerMode".into(); 8]),
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
                "AcquisitionFrameRateEnable",
                "TriggerMode",
                "BlackLevel",
                "Gain",
                "ExposureAuto",
                "GainSelector"
            ]
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
        assert_eq!(
            matching(&saved, &info("FLIR", "BFS-U3-51S5M", "1")).0,
            Match::OtherModel
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
        assert_eq!(matching(&saved, &rtsp).0, Match::OtherModel);
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
        assert_eq!(matching(&saved_hub, &other_hub).0, Match::OtherModel);
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
    fn damaged_and_newer_files_are_listed_and_can_be_deleted() {
        let store = Store::at(temp("unreadable"));
        store.save(&setup("good"), false).unwrap();
        fs::write(store.dir().join("broken.json"), br#"{"name":"broken","#).unwrap();
        let mut future = serde_json::to_value(setup("future")).unwrap();
        future["version"] = serde_json::json!(2);
        future["lens"] = serde_json::json!("25mm");
        let future = serde_json::to_vec_pretty(&future).unwrap();
        fs::write(store.dir().join("future.json"), &future).unwrap();
        // A newer document says so, not that a field is unknown.
        let error = format!("{:#}", parse(&future).unwrap_err());
        assert!(
            error.contains("version 2 was written by a newer Capturefab"),
            "{error}"
        );
        // It is a good setup for a newer Capturefab: never steer it to
        // deletion, and say it is unsupported here.
        let error = store.get("future").unwrap_err();
        assert_eq!(
            crate::types::error_transport_code(&error),
            Some("unsupported")
        );
        let error = format!("{error:#}");
        assert!(error.contains("update Capturefab to use it"), "{error}");
        assert!(
            !error.contains("delete") && !error.contains("--replace"),
            "{error}"
        );
        let error = store.get("broken").unwrap_err();
        assert_eq!(
            crate::types::error_transport_code(&error),
            Some("unavailable")
        );
        assert!(format!("{error:#}").contains("setup delete broken"));
        let list = store.list().unwrap();
        let rows = list
            .iter()
            .map(|row| (row.name.as_str(), row.status))
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                ("broken", Some(FileStatus::Unreadable)),
                ("future", Some(FileStatus::NewerVersion)),
                ("good", None),
            ]
        );
        assert!(
            list[0].reason.as_deref().unwrap().contains("EOF"),
            "{list:?}"
        );
        assert!(list[0].transport.is_none() && list[0].values == 0);
        let json = serde_json::to_value(&list).unwrap();
        assert_eq!(json[0]["status"], "unreadable");
        assert_eq!(json[1]["status"], "newer_version");
        assert!(json[2].get("status").is_none());
        assert_eq!(json[2]["transport"], serde_json::json!(Transport::GigE));
        // Neither blocks its name silently, and both delete.
        let taken = store.save(&setup("broken"), false).unwrap_err();
        assert!(taken.is::<NameTaken>());
        assert_eq!(
            taken.to_string(),
            "A setup named broken exists but cannot be read; pass --replace to overwrite it"
        );
        assert_eq!(
            store.save(&setup("future"), false).unwrap_err().to_string(),
            "A setup named future exists but was saved by a newer Capturefab; pass --replace to overwrite it"
        );
        assert_eq!(
            store.remove("broken").unwrap(),
            Removed {
                name: "broken".into(),
                readable: false
            }
        );
        assert_eq!(store.remove("FUTURE").unwrap().name, "FUTURE");
        assert_eq!(store.remove("good").unwrap().name, "good");
        assert!(store.list().unwrap().is_empty());
        let missing = store.remove("good").unwrap_err();
        assert_eq!(
            crate::types::error_transport_code(&missing),
            Some("unavailable")
        );
        let _ = fs::remove_dir_all(store.dir());
    }

    #[test]
    fn a_copied_or_renamed_file_is_not_another_setup() {
        let store = Store::at(temp("renamed"));
        let bench = store.save(&setup("bench"), false).unwrap();
        fs::copy(&bench, store.dir().join("other.json")).unwrap();
        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!((list[0].name.as_str(), list[0].status), ("bench", None));
        assert_eq!(list[1].name, "other");
        assert_eq!(list[1].status, Some(FileStatus::Unreadable));
        assert!(
            list[1]
                .reason
                .as_deref()
                .unwrap()
                .contains("named bench, not other"),
            "{list:?}"
        );
        let error = store.get("other").unwrap_err();
        assert_eq!(
            crate::types::error_transport_code(&error),
            Some("unavailable")
        );
        // Saving over the copy names the setup asked for, not the one
        // inside the file.
        let taken = store.save(&setup("other"), false).unwrap_err();
        assert_eq!(
            taken.to_string(),
            "A setup named other exists but cannot be read; pass --replace to overwrite it"
        );
        // The suggested import, quoted, makes the copy a setup of its own.
        let message = error.to_string();
        let import = format!(
            "capturefab setup import {} --name other --replace",
            crate::hint::shell_quote(&store.dir().join("other.json").display().to_string())
        );
        assert!(message.contains(&import), "{message}");
        store
            .import(&store.dir().join("other.json"), Some("other"), true)
            .unwrap();
        assert_eq!(store.get("other").unwrap().name, "other");
        assert!(store.list().unwrap().iter().all(Summary::readable));
        fs::copy(&bench, store.dir().join("other.json")).unwrap();
        // Deleting other removes other.json and says so; bench stays.
        assert_eq!(store.remove("other").unwrap().name, "other");
        assert!(bench.exists());
        assert_eq!(store.list().unwrap().len(), 1);
        // A file name with capitals is flagged, and deletes by that name.
        let mut shelf = setup("shelf");
        shelf.name = "shelf".into();
        fs::write(
            store.dir().join("Shelf.json"),
            serde_json::to_vec_pretty(&shelf).unwrap(),
        )
        .unwrap();
        let list = store.list().unwrap();
        let row = list.iter().find(|row| row.name == "Shelf").unwrap();
        assert_eq!(row.status, Some(FileStatus::Unreadable));
        assert!(row.reason.as_deref().unwrap().contains("capital letters"));
        store.remove("Shelf").unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        // Files no setup could be named are not setups, and do not count
        // toward the limit.
        fs::write(store.dir().join("my notes.json"), b"{}").unwrap();
        fs::write(store.dir().join("a b.json"), b"{}").unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        for i in 1..MAX_SETUPS {
            store.save(&setup(&format!("s{i}")), false).unwrap();
        }
        assert_eq!(store.list().unwrap().len(), MAX_SETUPS);
        let full = format!("{:#}", store.save(&setup("one-more"), false).unwrap_err());
        assert!(full.contains("setups are saved"), "{full}");
        let _ = fs::remove_dir_all(store.dir());
    }

    #[test]
    fn reasons_name_files_not_long_paths() {
        let deep = temp("long").join("a".repeat(120)).join("b".repeat(120));
        let store = Store::at(deep.join("setups"));
        let bench = store.save(&setup("bench"), false).unwrap();
        fs::copy(&bench, store.dir().join("other.json")).unwrap();
        let mut shelf = setup("shelf");
        shelf.name = "shelf".into();
        fs::write(
            store.dir().join("Shelf.json"),
            serde_json::to_vec_pretty(&shelf).unwrap(),
        )
        .unwrap();
        let list = store.list().unwrap();
        let reason = |name: &str| {
            list.iter()
                .find(|row| row.name == name)
                .unwrap()
                .reason
                .clone()
                .unwrap()
        };
        assert_eq!(
            reason("other"),
            "other.json holds a setup named bench, not other"
        );
        assert_eq!(
            reason("Shelf"),
            "Shelf.json has capital letters; Capturefab names setup files in lower case. Rename it to shelf.json"
        );
        let _ = fs::remove_dir_all(temp("long"));
        let _ = fs::remove_dir_all(&deep);
    }

    #[test]
    fn notes_reject_control_characters() {
        for bad in ["a\x1b[2Jb", "a\nb", "\x1b]0;pwned\x07", "tab\there"] {
            let mut note = setup("note");
            note.note = Some(bad.into());
            let error = format!("{:#}", validate(&note).unwrap_err());
            assert!(error.contains("control characters"), "{bad:?}: {error}");
            assert!(validate_note(bad).is_err());
            assert!(parse(&serde_json::to_vec(&note).unwrap()).is_err());
        }
        validate_note("Ring light at 60% · überall").unwrap();
    }

    #[test]
    fn a_stored_note_with_a_tab_still_reads() {
        // Older versions stored such notes; they stay usable.
        let store = Store::at(temp("tabbed"));
        let path = store.save(&setup("tabbed"), false).unwrap();
        let mut json: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        json["note"] = serde_json::json!("a\tb\nc");
        fs::write(&path, serde_json::to_vec_pretty(&json).unwrap()).unwrap();
        let list = store.list().unwrap();
        assert!(list[0].readable(), "{list:?}");
        assert_eq!(list[0].note.as_deref(), Some("a b c"));
        assert_eq!(store.get("tabbed").unwrap().note.as_deref(), Some("a b c"));
        // A new file with one is still refused.
        assert!(read_file(&path).is_err());
        let _ = fs::remove_dir_all(store.dir());
    }

    #[test]
    fn readings_past_the_skip_limit_count_what_they_leave_out() {
        let reading = Reading {
            camera: setup("x").camera,
            auto: None,
            values: vec![value("Gain", serde_json::json!(1))],
            skipped: (0..1500)
                .map(|i| Skipped {
                    feature: format!("S{i}"),
                    at: Vec::new(),
                    reason: "r".into(),
                })
                .collect(),
        };
        let setup = from_read("many", None, reading).unwrap();
        assert_eq!(setup.skipped.len(), MAX_SKIPPED);
        let last = setup.skipped.last().unwrap();
        assert_eq!(last.feature, "(more)");
        assert_eq!(last.reason, "477 more skipped values not listed");
    }

    #[test]
    fn sizes_are_measured_as_stored_and_large_readings_still_save() {
        let banks = || {
            [
                "GainSelector",
                "LineSelector",
                "TriggerSelector",
                "CounterSelector",
            ]
            .map(|selector| Bank {
                selector: selector.into(),
                value: "Line0".into(),
            })
            .to_vec()
        };
        let reading = Reading {
            camera: setup("x").camera,
            auto: None,
            values: (0..MAX_SETUP_VALUES)
                .map(|i| SetupValue {
                    at: banks(),
                    ..value(&format!("Feature{i}"), serde_json::json!(i))
                })
                .collect(),
            skipped: (0..MAX_SKIPPED)
                .map(|i| Skipped {
                    feature: format!("Skipped{i}"),
                    at: banks(),
                    reason: "r".repeat(240),
                })
                .collect(),
        };
        let big = from_read("big", None, reading.clone()).unwrap();
        assert!(stored_bytes(&big).unwrap().len() <= MAX_SETUP_BYTES);
        assert!(big.skipped.len() < MAX_SKIPPED);
        let last = big.skipped.last().unwrap();
        assert_eq!(last.feature, "(more)");
        let listed = big.skipped.len() - 1;
        assert_eq!(
            last.reason,
            format!("{} more skipped values not listed", MAX_SKIPPED - listed)
        );
        // It saves and imports back as written.
        let dir = temp("size");
        let store = Store::at(dir.join("setups"));
        let path = store.save(&big, false).unwrap();
        assert_eq!(
            store.import(&path, Some("again"), false).unwrap().values,
            big.values
        );
        assert_eq!(store.get("again").unwrap().skipped, big.skipped);
        // Values that cannot fit at all are unsupported, not a bare limit.
        let mut huge = reading;
        for value in &mut huge.values {
            value.value = Scalar::String("x".repeat(600));
        }
        let error = from_read("huge", None, huge).unwrap_err();
        assert_eq!(
            crate::types::error_transport_code(&error),
            Some("unsupported")
        );
        // A document small when compact but over the limit as stored is
        // refused when read, not only when saved.
        let mut document = setup("pretty");
        let mut i = 0;
        while stored_bytes(&document).unwrap().len() <= MAX_SETUP_BYTES {
            document.skipped.push(Skipped {
                feature: format!("Skipped{i}"),
                at: banks(),
                reason: "r".into(),
            });
            i += 1;
        }
        let compact = serde_json::to_vec(&document).unwrap();
        assert!(compact.len() <= MAX_SETUP_BYTES);
        let error = format!("{:#}", parse(&compact).unwrap_err());
        assert!(error.contains("256 KiB"), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn inline_setups_accept_the_show_envelope() {
        let bench = setup("bench");
        let bare = serde_json::json!({"op":"apply_setup","setup":bench});
        let wrapped = serde_json::json!({
            "op":"apply_setup",
            "setup":{"version":1,"ok":true,"result":bench}
        });
        let parse_command =
            |json: &serde_json::Value| SessionCommand::from_json_slice(json.to_string().as_bytes());
        let expected = parse_command(&bare).unwrap();
        assert_eq!(
            serde_json::to_value(parse_command(&wrapped).unwrap()).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        let diff = serde_json::json!({
            "op":"setup_diff",
            "setup":{"version":1,"ok":true,"result":bench}
        });
        assert!(parse_command(&diff).is_ok());
        for bad in [
            serde_json::json!({"version":1,"ok":false,"result":bench}),
            serde_json::json!({"version":2,"ok":true,"result":bench}),
            serde_json::json!({"version":1,"ok":true,"result":bench,"extra":1}),
        ] {
            let error =
                parse_command(&serde_json::json!({"op":"apply_setup","setup":bad})).unwrap_err();
            assert!(
                format!("{error:#}").contains("setup show NAME --json"),
                "{error:#}"
            );
        }
        let mut newer = serde_json::to_value(&bench).unwrap();
        newer["version"] = serde_json::json!(2);
        newer["lens"] = serde_json::json!(1);
        let error =
            parse_command(&serde_json::json!({"op":"setup_diff","setup":newer})).unwrap_err();
        assert!(
            format!("{error:#}").contains("newer Capturefab"),
            "{error:#}"
        );
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
