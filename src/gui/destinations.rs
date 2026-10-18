//! The GUI's "Save to" picker and destination manager: save captures and
//! recordings on this computer, in a folder on an external or network drive,
//! or in an S3-compatible bucket.
//!
//! Disk scans, bucket checks, keychain lookups and writes, and queue status
//! run on background threads, so a slow network share, service or credential
//! store never stalls the interface.
use super::button::button;
use super::format::{capitalize, grouped, plural};
use super::icon::{self, Icon, icon};
use super::motion::{self, Flashes, Kind, Motion};
use super::notice::Level;
use super::pick_list::pick_list;
use super::sparkline::History;
use super::style::{self, Palette};
use super::text_input::{self, text_input};
use super::widgets::{SheetFrame, disclosure, fade, field_error, one_line, sheet_frame};
use crate::destination::{self, Bucket, Credentials, Destination, NoOverwrite, Target};
use crate::volumes::{self, Volume};
use iced::widget::{column, container, row, space, text};
use iced::{Alignment, Element, Fill, Length, Theme};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    sync::mpsc::{Receiver, TryRecvError, channel},
    sync::{Arc, Mutex, PoisonError},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// How long the access key ID must rest before the keychain is asked
/// whether it holds the key's secret, so typing never waits on it.
const KEY_PAUSE: Duration = Duration::from_millis(400);
/// How long a just-saved destination's row glows in the list.
const SAVED_GLOW: Duration = Duration::from_millis(1000);
/// The key prefix new buckets start with.
const DEFAULT_PREFIX: &str = "capturefab/";
/// Ids of the editor fields that take focus as it opens.
const NAME_FIELD: &str = "destination-name";
const BUCKET_FIELD: &str = "destination-bucket";
const REMOVE_CANCEL: &str = "destination-remove-cancel";
const REMOVE_TRIGGER: &str = "destination-remove-trigger";
/// Width of the destination sheets.
const SHEET_WIDTH: f32 = 560.0;
#[cfg(test)]
mod confirmation_tests;
#[cfg(test)]
mod form_tests;
#[cfg(test)]
pub(super) mod layout_tests;
mod naming_presets;
pub use naming_presets::Naming;
static SECRET_WRITES: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());
static NEXT_SECRET_WRITE: AtomicU64 = AtomicU64::new(1);

/// Run `work` on a thread; poll the receiver for its result.
fn background<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Receiver<T> {
    let (sender, receiver) = channel();
    let _ = thread::Builder::new()
        .name("capturefab-destination".into())
        .spawn(move || {
            let _ = sender.send(work());
        });
    receiver
}

pub fn bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = value as f64;
    let mut unit = 0;
    while size >= 1000.0 && unit + 1 < UNITS.len() {
        size /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Destination names allow letters, digits, '.', '-' and '_'.
fn safe_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// S3-compatible services with their usual endpoint form and addressing.
const PROVIDERS: [(&str, &str, &str, bool); 5] = [
    ("Amazon S3", "", "us-east-1", false),
    (
        "Cloudflare R2",
        "https://ACCOUNT_ID.r2.cloudflarestorage.com",
        "auto",
        true,
    ),
    (
        "Backblaze B2",
        "https://s3.us-west-004.backblazeb2.com",
        "us-west-004",
        true,
    ),
    (
        "MinIO, RustFS or self-hosted",
        "http://nas.local:9000",
        "us-east-1",
        true,
    ),
    ("Other S3-compatible", "https://", "us-east-1", true),
];

/// The `PROVIDERS` entry a saved bucket's endpoint belongs to.
fn provider_of(endpoint: &str) -> usize {
    let url = endpoint.trim_end_matches('/');
    if url.is_empty() {
        0
    } else if url.ends_with(".r2.cloudflarestorage.com") {
        1
    } else if url.ends_with(".backblazeb2.com") {
        2
    } else {
        PROVIDERS
            .iter()
            .position(|(_, preset, _, _)| *preset == url)
            .unwrap_or(PROVIDERS.len() - 1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Keychain,
    Profile,
    Environment,
}

impl Source {
    const ALL: [Source; 3] = [Source::Keychain, Source::Profile, Source::Environment];
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Source::Keychain if cfg!(target_os = "macos") => "Access key (keychain)",
            Source::Keychain => "Access key (OS keychain)",
            Source::Profile => "AWS profile",
            Source::Environment => "Environment variables",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider(usize);

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(PROVIDERS[self.0].0)
    }
}

/// An entry of the "Save to" list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    name: Option<String>,
    label: String,
}

impl fmt::Display for Choice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

/// A field of the editor that a refused destination can point at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Name,
    Folder,
    Endpoint,
    Region,
    Bucket,
    Prefix,
    AccessKey,
    Secret,
    Profile,
}

/// Editable values only. Runtime tasks and credential-store observations are
/// refreshed when a draft is restored, rather than replayed from stale state.
#[derive(Clone, PartialEq, Eq)]
struct Draft {
    name: String,
    name_touched: bool,
    folder: String,
    provider: usize,
    endpoint: String,
    region: String,
    bucket: String,
    prefix: String,
    path_style: bool,
    source: Source,
    access_key_id: String,
    secret: String,
    profile: String,
    keep_local: bool,
}

struct DraftChange {
    before: Draft,
    after: Draft,
    label: &'static str,
}

#[derive(Default)]
struct DraftHistory {
    undo: Vec<DraftChange>,
    redo: Vec<DraftChange>,
    last: Option<(&'static str, Instant)>,
}

impl DraftHistory {
    fn record(&mut self, before: Draft, after: Draft, label: &'static str, coalesce: bool) {
        if before == after {
            return;
        }
        let now = Instant::now();
        let mut canceled = false;
        if coalesce
            && self.last.is_some_and(|(last, at)| {
                last == label && now.duration_since(at) < Duration::from_millis(600)
            })
            && let Some(previous) = self.undo.last_mut()
        {
            canceled = previous.before == after;
            previous.after = after;
            if canceled {
                self.undo.pop();
            }
        } else {
            if self.undo.len() == 200 {
                self.undo.remove(0);
            }
            self.undo.push(DraftChange {
                before,
                after,
                label,
            });
        }
        self.redo.clear();
        self.last = (coalesce && !canceled).then_some((label, now));
    }
}

#[derive(Clone, PartialEq, Eq)]
struct SecretChange {
    access_key_id: String,
    before: Option<String>,
    after: String,
}

impl fmt::Debug for SecretChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretChange")
            .field("access_key_id", &self.access_key_id)
            .field("previously_stored", &self.before.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
struct Selection {
    selected: Option<String>,
    output: String,
}

/// A successful saved-destination edit, including its selection side effects.
/// Secret values stay in memory and are deliberately omitted from Debug.
#[derive(Clone)]
pub struct Change {
    before: Option<Destination>,
    after: Option<Destination>,
    secret: Option<SecretChange>,
    before_selection: Selection,
    after_selection: Selection,
    label: String,
    created_at: Instant,
}

impl fmt::Debug for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DestinationChange")
            .field("before", &self.before)
            .field("after", &self.after)
            .field("credentials_changed", &self.secret.is_some())
            .field("label", &self.label)
            .finish()
    }
}

impl Change {
    pub fn label(&self) -> &str {
        &self.label
    }
    pub fn created_at(&self) -> Instant {
        self.created_at
    }
}

struct SaveContext {
    before: Option<Destination>,
    selection: Selection,
    created_at: Instant,
}

struct Replay {
    task: Receiver<anyhow::Result<()>>,
    selection: Selection,
}

impl Field {
    /// The field a message from `Destination::validate` is about, by its
    /// opening words, as `cli::error_code` classifies errors.
    fn of(message: &str) -> Option<Field> {
        const OPENINGS: [(&str, Field); 9] = [
            ("destination names", Field::Name),
            ("a folder destination", Field::Folder),
            ("S3 endpoint", Field::Endpoint),
            ("S3 region", Field::Region),
            ("S3 bucket", Field::Bucket),
            ("S3 key prefix", Field::Prefix),
            ("S3 access key", Field::AccessKey),
            ("S3 secret access key", Field::Secret),
            ("AWS profile", Field::Profile),
        ];
        OPENINGS
            .iter()
            .find(|(opening, _)| message.starts_with(opening))
            .map(|&(_, field)| field)
    }
}

/// The add/edit form.
struct Editor {
    /// The name being edited, when changing an existing destination.
    original: Option<String>,
    name: String,
    /// The name was typed, so it no longer follows the bucket.
    name_touched: bool,
    s3: bool,
    folder: String,
    provider: usize,
    endpoint: String,
    region: String,
    bucket: String,
    prefix: String,
    path_style: bool,
    source: Source,
    access_key_id: String,
    secret: String,
    /// Whether the keychain holds a secret for `access_key_id`.
    stored_secret: bool,
    /// When the access key ID last changed, until the keychain is asked.
    key_changed: Option<Instant>,
    /// The keychain answering whether it holds a secret for an access key ID.
    key_probe: Option<(String, Receiver<bool>)>,
    profile: String,
    keep_local: bool,
    /// Kept from the saved bucket: `destination test --probe-writes` may have
    /// switched it to check-first uploads.
    no_overwrite: NoOverwrite,
    /// The key prefix and addressing style show.
    advanced: bool,
    /// Fields the last save or test refused, and why, in form order.
    invalid: Vec<(Field, String)>,
    /// The last check's outcome or a refusal no field explains; true for
    /// a problem.
    message: Option<(String, bool)>,
    /// A running check. Typed credentials are used transiently and never
    /// change the saved key until Save succeeds.
    testing: Option<Receiver<Result<String, String>>>,
    /// A running save of this destination: its folder being made or its
    /// typed secret stored, answering whether a secret was.
    saving: Option<(Destination, Receiver<anyhow::Result<Option<SecretChange>>>)>,
    history: DraftHistory,
    /// An FTP or SFTP destination, which this form cannot represent: it is
    /// shown but never saved, so it cannot be replaced by a folder.
    read_only: bool,
}

/// Where FTP and SFTP destinations are changed until the form supports them.
const SERVER_EDIT_HINT: &str = "Edit with capturefab destination add-ftp/add-sftp";

/// The command that changes a server destination, shown on its row because
/// the form cannot edit one.
fn server_hint(target: &Target) -> Option<String> {
    let command = match target {
        Target::Ftp(_) => "add-ftp",
        Target::Sftp(_) => "add-sftp",
        _ => return None,
    };
    Some(format!("Edit with capturefab destination {command}"))
}

impl Editor {
    fn new_folder(name: String, folder: PathBuf) -> Self {
        Self {
            original: None,
            name,
            name_touched: true,
            s3: false,
            folder: folder.to_str().unwrap_or_default().to_owned(),
            provider: 0,
            endpoint: String::new(),
            region: "us-east-1".into(),
            bucket: String::new(),
            prefix: DEFAULT_PREFIX.into(),
            path_style: false,
            source: Source::Keychain,
            access_key_id: String::new(),
            secret: String::new(),
            stored_secret: false,
            key_changed: None,
            key_probe: None,
            profile: "default".into(),
            keep_local: false,
            no_overwrite: NoOverwrite::IfNoneMatch,
            advanced: false,
            invalid: if folder.to_str().is_none() {
                vec![(Field::Folder, folder_encoding_error().into())]
            } else {
                Vec::new()
            },
            message: None,
            testing: None,
            saving: None,
            history: DraftHistory::default(),
            read_only: false,
        }
    }
    /// A new bucket, named after the bucket until a name is typed.
    fn new_bucket() -> Self {
        Self {
            s3: true,
            name_touched: false,
            ..Self::new_folder(String::new(), PathBuf::new())
        }
    }
    fn edit(destination: &Destination) -> Self {
        let mut editor = Self::new_folder(destination.name.clone(), PathBuf::new());
        editor.original = Some(destination.name.clone());
        match &destination.target {
            Target::Folder { path } => {
                if let Some(path) = path.to_str() {
                    editor.folder = path.into();
                } else {
                    editor.invalid = vec![(Field::Folder, folder_encoding_error().into())];
                }
            }
            Target::Ftp(_) | Target::Sftp(_) => {
                editor.read_only = true;
                editor.message = Some((SERVER_EDIT_HINT.into(), true));
            }
            Target::S3(bucket) => {
                editor.s3 = true;
                editor.endpoint = bucket.endpoint.clone();
                editor.region = bucket.region.clone();
                editor.bucket = bucket.bucket.clone();
                editor.prefix = bucket.prefix.clone();
                editor.path_style = bucket.path_style;
                editor.keep_local = bucket.keep_local;
                editor.no_overwrite = bucket.no_overwrite;
                editor.provider = provider_of(&bucket.endpoint);
                match &bucket.credentials {
                    Credentials::Keychain { access_key_id } => {
                        editor.access_key_id = access_key_id.clone();
                        // Asked at once: nothing is being typed yet.
                        editor.key_edited(
                            Instant::now()
                                .checked_sub(KEY_PAUSE)
                                .unwrap_or_else(Instant::now),
                        );
                    }
                    Credentials::Profile { name } => {
                        editor.source = Source::Profile;
                        editor.profile = name.clone();
                    }
                    Credentials::Environment => editor.source = Source::Environment,
                }
                editor.advanced = editor.custom_advanced();
            }
        }
        editor
    }

    fn draft(&self) -> Draft {
        Draft {
            name: self.name.clone(),
            name_touched: self.name_touched,
            folder: self.folder.clone(),
            provider: self.provider,
            endpoint: self.endpoint.clone(),
            region: self.region.clone(),
            bucket: self.bucket.clone(),
            prefix: self.prefix.clone(),
            path_style: self.path_style,
            source: self.source,
            access_key_id: self.access_key_id.clone(),
            secret: self.secret.clone(),
            profile: self.profile.clone(),
            keep_local: self.keep_local,
        }
    }

    fn restore_draft(&mut self, draft: Draft) {
        self.name = draft.name;
        self.name_touched = draft.name_touched;
        self.folder = draft.folder;
        self.provider = draft.provider;
        self.endpoint = draft.endpoint;
        self.region = draft.region;
        self.bucket = draft.bucket;
        self.prefix = draft.prefix;
        self.path_style = draft.path_style;
        self.source = draft.source;
        self.access_key_id = draft.access_key_id;
        self.secret = draft.secret;
        self.profile = draft.profile;
        self.keep_local = draft.keep_local;
        self.invalid.clear();
        self.edited(None);
        self.key_probe = None;
        self.key_edited(Instant::now());
        self.advanced |= self.custom_advanced();
    }

    /// Whether the Advanced fields differ from what a new bucket of this
    /// service starts with, so they show without being asked for.
    fn custom_advanced(&self) -> bool {
        self.prefix != DEFAULT_PREFIX
            || self.path_style != PROVIDERS[self.provider].3
            || self.source != Source::Keychain
            || (self.region_advanced() && self.region != PROVIDERS[self.provider].2)
    }

    /// Whether the region is under Advanced: Cloudflare R2 takes "auto".
    fn region_advanced(&self) -> bool {
        self.provider == 1
    }

    /// The access key ID changed at `at`: whether the keychain holds its
    /// secret is unknown until typing rests for `KEY_PAUSE`.
    fn key_edited(&mut self, at: Instant) {
        self.stored_secret = false;
        if cfg!(feature = "s3") {
            self.key_changed = Some(at);
        }
    }

    /// The access key ID to ask the keychain about at `now`, once typing
    /// has rested and no other answer is awaited.
    fn key_due(&self, now: Instant) -> Option<String> {
        let rested = self
            .key_changed
            .is_some_and(|at| now.saturating_duration_since(at) >= KEY_PAUSE);
        (rested && self.key_probe.is_none()).then(|| self.access_key_id.trim().to_owned())
    }

    /// Take the keychain's answer, if it came, and ask again once due;
    /// `ask` starts a lookup off the UI thread. From `Picker::tick`.
    #[cfg_attr(not(feature = "s3"), allow(dead_code))]
    fn poll_key(&mut self, now: Instant, ask: impl FnOnce(String) -> Receiver<bool>) {
        if let Some((id, answer)) = &self.key_probe {
            match answer.try_recv() {
                Ok(stored) => {
                    // An answer about a key ID since edited is stale.
                    if id.as_str() == self.access_key_id.trim() {
                        self.stored_secret = stored;
                    }
                    self.key_probe = None;
                }
                Err(TryRecvError::Disconnected) => self.key_probe = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(id) = self.key_due(now) {
            self.key_changed = None;
            if !id.is_empty() {
                self.key_probe = Some((id.clone(), ask(id)));
            }
        }
    }

    /// The typed secret is now in the keychain under the current access key
    /// ID, so any lookup in flight or due for it is moot.
    fn secret_stored(&mut self) {
        self.secret.clear();
        self.stored_secret = true;
        self.key_probe = None;
        self.key_changed = None;
        self.history = DraftHistory::default();
    }

    /// The access key ID and typed secret to store in the keychain before
    /// the bucket is saved, if one was typed.
    fn secret_to_store(&self) -> Option<(String, String)> {
        (self.s3 && self.source == Source::Keychain && !self.secret.is_empty())
            .then(|| (self.access_key_id.trim().to_owned(), self.secret.clone()))
    }

    /// A save waits for the check using its typed key, so its result remains
    /// visible before the form closes. The check never changes the keychain.
    fn checking_secret(&self) -> bool {
        self.testing.is_some() && self.secret_to_store().is_some()
    }

    /// The destination as typed, with a valid stand-in for each of
    /// `stand_ins`, so validation reaches the fields after them.
    fn build(&self, stand_ins: &[Field]) -> Destination {
        let value = |field: Field, typed: &str, stand_in: &str| -> String {
            if stand_ins.contains(&field) {
                stand_in.into()
            } else {
                typed.trim().into()
            }
        };
        let target = if self.s3 {
            Target::S3(Bucket {
                endpoint: value(Field::Endpoint, &self.endpoint, ""),
                region: value(Field::Region, &self.region, "us-east-1"),
                bucket: value(Field::Bucket, &self.bucket, "bucket"),
                prefix: if stand_ins.contains(&Field::Prefix) {
                    String::new()
                } else {
                    self.prefix.clone()
                },
                path_style: self.path_style,
                credentials: match self.source {
                    Source::Keychain => Credentials::Keychain {
                        access_key_id: value(Field::AccessKey, &self.access_key_id, "AKIA"),
                    },
                    Source::Profile => Credentials::Profile {
                        name: value(Field::Profile, &self.profile, "default"),
                    },
                    Source::Environment => Credentials::Environment,
                },
                keep_local: self.keep_local,
                no_overwrite: self.no_overwrite,
            })
        } else {
            Target::Folder {
                path: if stand_ins.contains(&Field::Folder) {
                    std::env::temp_dir()
                } else {
                    PathBuf::from(&self.folder)
                },
            }
        };
        Destination {
            name: value(Field::Name, &self.name, "destination"),
            target,
        }
    }

    fn destination(&self) -> anyhow::Result<Destination> {
        anyhow::ensure!(!self.read_only, "{SERVER_EDIT_HINT}");
        let destination = self.build(&[]);
        destination.validate()?;
        #[cfg(feature = "s3")]
        if self.s3 && self.source == Source::Keychain && !self.secret.is_empty() {
            crate::s3::validate_secret(&self.secret)?;
        }
        Ok(destination)
    }

    /// Every field the destination can't be saved with, and why: validation
    /// stops at the first, so each found field gets a stand-in and it runs
    /// again.
    fn problems(&self) -> Vec<(Field, String)> {
        let mut found: Vec<(Field, String)> = Vec::new();
        loop {
            let fields: Vec<Field> = found.iter().map(|(field, _)| *field).collect();
            let Err(error) = self.build(&fields).validate() else {
                break;
            };
            let message = error.to_string();
            match Field::of(&message) {
                Some(field) if !fields.contains(&field) => found.push((field, message)),
                _ => break,
            }
        }
        // A name that follows an unusable bucket is the bucket's problem.
        if !self.name_touched && found.iter().any(|(field, _)| *field == Field::Bucket) {
            found.retain(|(field, _)| *field != Field::Name);
        }
        #[cfg(feature = "s3")]
        if self.s3
            && self.source == Source::Keychain
            && !self.secret.is_empty()
            && let Err(error) = crate::s3::validate_secret(&self.secret)
        {
            found.push((Field::Secret, error.to_string()));
        }
        found
    }

    /// Show why a save or test was refused: on the fields at fault when
    /// validation names them, else under the form.
    fn refuse(&mut self, error: anyhow::Error) {
        let message = format!("{error:#}");
        let problems = if Field::of(&message).is_some() {
            self.problems()
        } else {
            Vec::new()
        };
        if problems.is_empty() {
            self.message = Some((capitalize(&message), true));
        } else {
            // A refused field under Advanced opens it.
            self.advanced |= problems.iter().any(|(field, _)| match field {
                Field::Prefix | Field::Profile => true,
                Field::Region => self.region_advanced(),
                _ => false,
            });
            self.invalid = problems;
            self.message = None;
        }
    }

    /// Refuse the name: another saved destination has it, which saving
    /// would replace.
    fn refuse_name(&mut self, why: String) {
        // It no longer follows the bucket, so the fix sticks.
        self.name_touched = true;
        self.invalid = vec![(Field::Name, why)];
        self.message = None;
    }

    /// Why the name can't be saved, if another destination in `saved` has
    /// it. Keeping the name of the destination being edited is fine.
    fn name_taken(&self, saved: &[Destination]) -> Option<String> {
        let name = self.name.trim();
        (self.original.as_deref() != Some(name) && saved.iter().any(|d| d.name == name))
            .then(|| format!("a destination named {name} already exists; choose another name"))
    }

    /// `field` was edited: its refusal no longer applies, nor does the last
    /// check's outcome, or a check still running on the old values. A save
    /// retains its receiver until its worker's commit is acknowledged.
    fn edited(&mut self, field: Option<Field>) {
        if let Some(field) = field {
            self.invalid.retain(|(f, _)| *f != field);
        }
        self.message = None;
        self.testing = None;
    }

    /// Why `field` was refused, if it was.
    fn refusal(&self, field: Field) -> Option<&str> {
        self.invalid
            .iter()
            .find(|(f, _)| *f == field)
            .map(|(_, message)| message.as_str())
    }
    /// The slow part of saving `destination`, to run off the UI thread:
    /// making its folder and storing a typed secret in the OS credential
    /// store. Answers whether a secret was stored.
    fn prepare(
        &self,
        destination: &Destination,
        before: Option<Destination>,
        remember: bool,
    ) -> impl FnOnce() -> anyhow::Result<Option<SecretChange>> + Send + 'static {
        let folder = match &destination.target {
            Target::Folder { path } => Some(path.clone()),
            _ => None,
        };
        let destination = destination.clone();
        let secret = SecretWrite::queue(self.secret_to_store());
        move || {
            if let Some(path) = folder {
                volumes::ensure_present(&path)?;
                std::fs::create_dir_all(&path)
                    .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", path.display()))?;
            }
            destination::replace_with_selection(
                before.as_ref(),
                Some(&destination),
                Some((Some(&destination.name), remember)),
                || store_secret(secret),
                |change| match change {
                    Some(change) => restore_secret(change, true),
                    None => Ok(()),
                },
            )
        }
    }

    /// Save the destination at once; returns the saved name.
    fn save(&mut self, remember: bool) -> anyhow::Result<String> {
        let destination = self.destination()?;
        let before = self
            .original
            .as_ref()
            .map(|name| destination::get(name))
            .transpose()?;
        if self.prepare(&destination, before, remember)()?.is_some() {
            self.secret_stored();
        }
        self.original = Some(destination.name.clone());
        Ok(destination.name)
    }

    /// Take a running check's outcome, if it came.
    fn poll_test(&mut self) {
        let answer = match self.testing.as_ref().map(Receiver::try_recv) {
            None | Some(Err(TryRecvError::Empty)) => return,
            Some(Ok(answer)) => answer,
            Some(Err(TryRecvError::Disconnected)) => {
                Err("the check stopped without an answer".into())
            }
        };
        self.testing = None;
        self.message = Some(match answer {
            Ok(text) => (text, false),
            Err(text) => (capitalize(&text), true),
        });
    }

    /// Finish a save whose slow part answered: record the destination, or
    /// show why it was refused. The saved name, once saved.
    fn poll_save(&mut self) -> Option<(String, Option<SecretChange>)> {
        let prepared = match self.saving.as_ref()?.1.try_recv() {
            Ok(prepared) => prepared,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                Err(anyhow::anyhow!("saving stopped without an answer"))
            }
        };
        let (destination, _) = self.saving.take()?;
        match prepared {
            Ok(secret) => {
                if secret.is_some() {
                    self.secret_stored();
                }
                self.original = Some(destination.name.clone());
                Some((destination.name, secret))
            }
            Err(error) => {
                self.refuse(error);
                None
            }
        }
    }
}

/// A typed secret to store under its access key ID, numbered on the UI
/// thread in the order editors asked for stores.
struct SecretWrite {
    access_key_id: String,
    secret: String,
    ticket: u64,
}

impl SecretWrite {
    fn queue(secret: Option<(String, String)>) -> Option<Self> {
        secret.map(|(access_key_id, secret)| Self {
            access_key_id,
            secret,
            ticket: NEXT_SECRET_WRITE.fetch_add(1, Ordering::Relaxed),
        })
    }

    /// Store with `store`, one write at a time, unless a store asked for
    /// later already wrote this key: an editor drops a store it no longer
    /// wants, but its thread may still be waiting on a keychain prompt, and
    /// must not land over the newer secret.
    fn store_in_order<T>(
        self,
        written: &Mutex<BTreeMap<String, u64>>,
        store: impl FnOnce(&str, &str) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut written = written.lock().unwrap_or_else(PoisonError::into_inner);
        if written
            .get(&self.access_key_id)
            .is_some_and(|&ticket| ticket > self.ticket)
        {
            return Err(anyhow::anyhow!(
                "The credential write was superseded; save again"
            ));
        }
        let result = store(&self.access_key_id, &self.secret)?;
        written.insert(self.access_key_id, self.ticket);
        Ok(result)
    }
}

/// Store a typed secret under its access key ID in the OS credential store;
/// whether one was. It can wait on a keychain prompt, so off the UI thread.
fn store_secret(write: Option<SecretWrite>) -> anyhow::Result<Option<SecretChange>> {
    // Without S3 support there is no bucket to store a secret for.
    let Some(write) = write.filter(|_| cfg!(feature = "s3")) else {
        return Ok(None);
    };
    write
        .store_in_order(&SECRET_WRITES, |access_key_id, secret| {
            let before = keychain_read(access_key_id)?;
            keychain_store(access_key_id, secret)?;
            Ok(SecretChange {
                access_key_id: access_key_id.into(),
                before,
                after: secret.into(),
            })
        })
        .map(Some)
}

fn restore_secret(change: &SecretChange, undo: bool) -> anyhow::Result<()> {
    let mut written = SECRET_WRITES.lock().unwrap_or_else(PoisonError::into_inner);
    restore_secret_using(change, undo, keychain_read, |id, secret| match secret {
        Some(secret) => keychain_store(id, secret),
        None => keychain_delete(id),
    })?;
    written.insert(
        change.access_key_id.clone(),
        NEXT_SECRET_WRITE.fetch_add(1, Ordering::Relaxed),
    );
    Ok(())
}

fn restore_secret_using(
    change: &SecretChange,
    undo: bool,
    read: impl FnOnce(&str) -> anyhow::Result<Option<String>>,
    write: impl FnOnce(&str, Option<&str>) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let (expected, desired) = if undo {
        (Some(change.after.as_str()), change.before.as_deref())
    } else {
        (change.before.as_deref(), Some(change.after.as_str()))
    };
    anyhow::ensure!(
        read(&change.access_key_id)?.as_deref() == expected,
        "the stored credential changed outside this edit; undo cannot overwrite it"
    );
    write(&change.access_key_id, desired)
}

#[cfg(feature = "s3")]
use crate::s3::store_secret as keychain_store;
#[cfg(feature = "s3")]
use crate::s3::{delete_secret as keychain_delete, read_secret as keychain_read};

#[cfg(not(feature = "s3"))]
fn keychain_store(_access_key_id: &str, _secret: &str) -> anyhow::Result<()> {
    Ok(())
}
#[cfg(not(feature = "s3"))]
fn keychain_read(_access_key_id: &str) -> anyhow::Result<Option<String>> {
    Ok(None)
}
#[cfg(not(feature = "s3"))]
fn keychain_delete(_access_key_id: &str) -> anyhow::Result<()> {
    Ok(())
}

/// Check a destination: write a probe file to a folder, or reach a bucket.
fn test(destination: Destination, secret: Option<String>) -> Result<String, String> {
    match &destination.target {
        Target::Folder { path } => {
            volumes::ensure_present(path).map_err(|e| e.to_string())?;
            std::fs::create_dir_all(path).map_err(|e| format!("Can't create the folder: {e}"))?;
            let probe = path.join(format!(".capturefab-write-test-{}", std::process::id()));
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&probe)
                .map_err(|e| {
                    format!("Can't write to this folder: {e}. Choose a folder you can write to.")
                })?;
            let _ = std::fs::remove_file(&probe);
            Ok(match fs2::available_space(path) {
                Ok(free) => format!("Writable, {} free", bytes(free)),
                Err(_) => "Writable".into(),
            })
        }
        Target::S3(bucket) => {
            #[cfg(feature = "s3")]
            {
                let client = match secret {
                    Some(secret) => crate::s3::Client::with_secret(bucket, secret),
                    None => crate::s3::Client::new(bucket),
                };
                client
                    .and_then(|client| client.check())
                    .map(|()| format!("Connected to {}", bucket.bucket))
                    .map_err(|e| format!("{e:#}"))
            }
            #[cfg(not(feature = "s3"))]
            {
                let _ = (bucket, secret);
                Err("S3 support is not included in this build".into())
            }
        }
        Target::Ftp(_) | Target::Sftp(_) => {
            let remote = destination.target.remote().expect("network destination");
            #[cfg(feature = "uploads")]
            {
                let location = remote.location();
                match secret {
                    Some(secret) => crate::transfer::connect_with_secret(&remote, secret),
                    None => crate::transfer::connect(&remote),
                }
                .and_then(|mut uploader| uploader.probe())
                .map(|_| format!("Connected to {location} and wrote a test file"))
                .map_err(|e| format!("{e:#}"))
            }
            #[cfg(not(feature = "uploads"))]
            {
                let _ = secret;
                Err(format!(
                    "{} support is not included in this build",
                    remote.kind().to_uppercase()
                ))
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum Message {
    Undo,
    Redo,
    Select(Choice),
    Output(String),
    /// Name captured files with a preset, or show the template to edit.
    Naming(Naming),
    BrowseOutput,
    UseVolume(usize),
    OpenManager,
    CloseManager,
    #[cfg_attr(not(feature = "s3"), allow(dead_code))]
    Retry,
    Edit(String),
    Remove(String),
    ConfirmRemove(Option<String>),
    AddFolder,
    AddBucket,
    Name(String),
    Folder(String),
    BrowseFolder,
    Provider(Provider),
    Endpoint(String),
    Region(String),
    Bucket(String),
    Prefix(String),
    Source(Source),
    AccessKey(String),
    Secret(String),
    Profile(String),
    PathStyle(bool),
    KeepLocal(bool),
    /// Show or hide the editor's key prefix and addressing style.
    Advanced(bool),
    Save,
    Test,
    Cancel,
    /// The sheet's body scrolled away from its top, or back.
    Scrolled(bool),
}

/// A value read on a background thread now and then. `changes` counts
/// readings that found something new, so each picker copies only those.
struct Polled<T> {
    value: Option<T>,
    changes: u64,
    task: Option<Receiver<Option<T>>>,
    at: Option<Instant>,
}

impl<T> Default for Polled<T> {
    fn default() -> Self {
        Self {
            value: None,
            changes: 0,
            task: None,
            at: None,
        }
    }
}

impl<T: Send + 'static> Polled<T> {
    /// Collect a finished reading, and start the next once `every` has
    /// passed; `read` returns `None` when nothing changed.
    fn poll(&mut self, every: Duration, read: impl FnOnce() -> Option<T> + Send + 'static) {
        self.collect();
        if self.task.is_none() && self.at.is_none_or(|t| t.elapsed() >= every) {
            self.start(read);
        }
    }

    /// Read now, replacing any reading under way.
    fn start(&mut self, read: impl FnOnce() -> Option<T> + Send + 'static) {
        self.at = Some(Instant::now());
        self.task = Some(background(read));
    }

    fn collect(&mut self) {
        let Some(task) = &self.task else { return };
        match task.try_recv() {
            Ok(found) => {
                self.task = None;
                if let Some(found) = found {
                    self.value = Some(found);
                    self.changes += 1;
                }
            }
            Err(TryRecvError::Disconnected) => self.task = None,
            Err(TryRecvError::Empty) => {}
        }
    }

    /// The value, if it changed since `seen`.
    fn newer(&self, seen: &mut u64) -> Option<&T> {
        if *seen == self.changes {
            return None;
        }
        *seen = self.changes;
        self.value.as_ref()
    }
}

/// The saved list's file as it stands: modification time and length.
type ListVersion = Option<(Option<SystemTime>, u64)>;

fn list_version() -> ListVersion {
    let meta = std::fs::metadata(destination::path()).ok()?;
    Some((meta.modified().ok(), meta.len()))
}

/// What both pickers read in the background and share, so they never poll
/// twice: the saved list when its file changes, the drives, and upload
/// status while there is a network destination or anything queued.
#[derive(Default)]
struct Watch {
    saved: Polled<(ListVersion, Vec<Destination>)>,
    volumes: Polled<Vec<Volume>>,
    /// The status and how long reading it took.
    uploads: Polled<(Arc<Value>, Duration)>,
    /// A screenshot scene holds the upload status it set.
    pinned: bool,
}

impl Watch {
    fn poll(&mut self) {
        let known = self.saved.value.as_ref().map(|(version, _)| *version);
        self.saved.poll(Duration::from_secs(3), move || {
            let version = list_version();
            (known != Some(version)).then(|| (version, destination::list().unwrap_or_default()))
        });
        self.volumes
            .poll(Duration::from_secs(5), || Some(volumes::list()));
        #[cfg(feature = "uploads")]
        if !self.pinned && self.uploads_matter() {
            let cost = self
                .uploads
                .value
                .as_ref()
                .map_or(Duration::ZERO, |(_, cost)| *cost);
            self.uploads.poll(upload_poll(cost), read_uploads);
        }
    }

    /// Whether a network destination is saved or uploads are queued; both
    /// count until first read.
    #[cfg(feature = "uploads")]
    fn uploads_matter(&self) -> bool {
        let remote = self
            .saved
            .value
            .as_ref()
            .is_none_or(|(_, saved)| saved.iter().any(|d| d.target.is_remote()));
        let queued = self.uploads.value.as_ref().is_none_or(|(status, _)| {
            ["pending", "uploading", "failed"]
                .iter()
                .any(|key| status[key].as_u64().unwrap_or(0) > 0)
        });
        remote || queued
    }

    /// The saved list changed here; read it again for the other picker.
    fn saved_changed(&mut self) {
        self.saved.value = None;
        self.saved.at = None;
    }

    /// Retry failed and waiting uploads now, then read what that leaves.
    #[cfg(feature = "uploads")]
    fn retry(&mut self) {
        self.uploads.start(|| {
            let _ = crate::upload::retry(None, true);
            read_uploads()
        });
    }
}

/// How long to wait after an upload status read that took `cost`. A long
/// queue takes longer to read, so look less often then and status never
/// keeps a core busy; but a read can also have waited for another process
/// to finish with the queue, so never wait long enough to look stale.
#[cfg(feature = "uploads")]
fn upload_poll(cost: Duration) -> Duration {
    (cost * 20).clamp(Duration::from_secs(2), Duration::from_secs(10))
}

#[cfg(feature = "uploads")]
fn read_uploads() -> Option<(Arc<Value>, Duration)> {
    let started = Instant::now();
    let status = crate::upload::status().ok()?;
    Some((Arc::new(status), started.elapsed()))
}

/// How a destination's upload line reads.
#[derive(Debug, PartialEq)]
enum Mood {
    Calm,
    /// Waiting for the destination to come back.
    Waiting,
    Failed,
}

/// What a destination's upload line says, from the queue's status.
#[derive(Debug, PartialEq)]
struct UploadLine<'a> {
    summary: String,
    mood: Mood,
    /// The retry button's label, when it has one.
    action: Option<&'static str>,
    /// The newest error, when it matters.
    error: Option<&'a str>,
    /// Files waiting or uploading.
    waiting: u64,
}

impl<'a> UploadLine<'a> {
    /// `now` is in Unix seconds, like the status's `next_attempt`.
    fn of(status: &'a Value, destination: &str, now: u64) -> Self {
        let backlog = &status["destinations"][destination];
        let count = |key: &str| backlog[key].as_u64().unwrap_or(0);
        let waiting = count("pending") + count("uploading");
        let (failed, size) = (count("failed"), bytes(count("waiting_bytes")));
        let files = plural(waiting, "file", "files");
        // Nothing starts before the next attempt, so a later one means the
        // destination could not be reached and is waiting to retry.
        let retry_in = backlog["next_attempt"]
            .as_u64()
            .filter(|&next| count("uploading") == 0 && next > now)
            .map(|next| super::format::span(Duration::from_secs(next - now)));
        let error = backlog["last_error"].as_str();
        let (summary, mood, action, error) = match (waiting, failed, retry_in) {
            (0, 0, _) => (
                format!("All uploaded · {} sent in total", status["uploaded"]),
                Mood::Calm,
                None,
                None,
            ),
            (_, 0, None) => (
                format!("Uploading {files} ({size})"),
                Mood::Calm,
                None,
                None,
            ),
            (_, 0, Some(retry_in)) => (
                format!("Offline · {files} ({size}) waiting · retrying in {retry_in}"),
                Mood::Waiting,
                Some("Retry now"),
                error,
            ),
            (waiting, failed, _) => (
                format!("{} waiting · {} failed", grouped(waiting), grouped(failed)),
                Mood::Failed,
                Some("Retry"),
                error,
            ),
        };
        Self {
            summary,
            mood,
            action,
            error,
            waiting,
        }
    }
}

/// Free space and presence of the selected folder, refreshed in the background.
struct FolderStatus {
    path: PathBuf,
    free: Option<u64>,
    mounted: bool,
}

/// The capture panel's destination choice and everything behind it.
pub struct Picker {
    saved: Vec<Destination>,
    loaded: Option<Instant>,
    /// None saves to the output path on this computer.
    pub selected: Option<String>,
    volumes: Vec<Volume>,
    folder: Option<FolderStatus>,
    folder_task: Option<Receiver<FolderStatus>>,
    folder_at: Option<Instant>,
    uploads: Option<Arc<Value>>,
    /// Shared with the other picker.
    watch: Arc<Mutex<Watch>>,
    /// The watch's changes already copied: saved list, drives, uploads.
    seen: [u64; 3],
    /// Bytes waiting to upload, while any are.
    backlog: History,
    editor: Option<Editor>,
    removing: Option<String>,
    /// The one row whose removal trigger regains focus after cancellation.
    removal_focus: Option<String>,
    pub manager_open: bool,
    /// The outcome of the manager's last change; true for a problem.
    notice: Option<(String, bool)>,
    /// Destinations just saved, whose rows glow for a moment.
    saved_glow: Flashes<String>,
    /// The editor's Advanced chevron: 0 closed, 1 open.
    advanced_turn: Motion,
    /// The sheet's body has scrolled under its header.
    scrolled: bool,
    /// The page changed, so its body is due to snap to its top.
    to_top: bool,
    /// The control to focus after an editor or confirmation changes.
    focus: Option<&'static str>,
    /// Remember the choice for the next launch (the capture panel does).
    remember: bool,
    /// Files are recordings, not stills; changes a few labels.
    recording: bool,
    /// "Custom pattern…" was chosen, or the template typed, so the output
    /// field shows even for a preset's template. Never remembered.
    custom_naming: bool,
    changes: Vec<Change>,
    saving_context: Option<SaveContext>,
    replay: Option<Replay>,
}

impl Picker {
    // The picker's own motions and flashes, which the workbench's registry
    // includes.
    super::motion::registry! {
        motions: [advanced_turn],
        flashes: [saved_glow],
    }
}

impl Picker {
    pub fn new(remember: bool, recording: bool) -> Self {
        Self {
            saved: destination::list().unwrap_or_default(),
            loaded: Some(Instant::now()),
            selected: if remember {
                destination::preferred()
            } else {
                None
            },
            volumes: Vec::new(),
            folder: None,
            folder_task: None,
            folder_at: None,
            uploads: None,
            watch: Arc::default(),
            seen: [0; 3],
            // Status is read every two seconds or so; take every reading.
            backlog: History::every(Duration::from_secs(1), Duration::from_secs(120)),
            editor: None,
            removing: None,
            removal_focus: None,
            manager_open: false,
            notice: None,
            saved_glow: Flashes::new(SAVED_GLOW),
            advanced_turn: Motion::new(0.0, motion::RING, motion::RING_OUT, Kind::Move),
            scrolled: false,
            to_top: false,
            focus: None,
            remember,
            recording,
            custom_naming: false,
            changes: Vec::new(),
            saving_context: None,
            replay: None,
        }
    }

    /// Share `other`'s background reads, so the two never poll twice.
    pub fn sharing(mut self, other: &Picker) -> Self {
        self.watch = other.watch.clone();
        self
    }

    /// Reload the saved list now and then, and collect background results;
    /// a finished save chooses its destination, as for `update`.
    pub fn tick(&mut self, output: &mut String) {
        if let Some((name, secret)) = self.editor.as_mut().and_then(Editor::poll_save) {
            let context = self.saving_context.take();
            let after = self.editor.as_ref().and_then(|e| e.destination().ok());
            self.saved_as(name, output);
            if let Some(context) = context {
                self.record_change(
                    context.before,
                    after,
                    secret,
                    context.selection,
                    output,
                    context.created_at,
                );
            }
        } else if self.editor.as_ref().is_none_or(|e| e.saving.is_none()) {
            self.saving_context = None;
        }
        let watch = self.watch.clone();
        let mut watch = watch.lock().unwrap_or_else(PoisonError::into_inner);
        if self.loaded.is_none() {
            // Changed here: read it now, so the change shows at once.
            self.adopt(destination::list().unwrap_or_default());
            watch.saved_changed();
        }
        watch.poll();
        if let Some((_, saved)) = watch.saved.newer(&mut self.seen[0]) {
            self.adopt(saved.clone());
        }
        if let Some(volumes) = watch.volumes.newer(&mut self.seen[1]) {
            self.volumes = volumes.clone();
        }
        if let Some((status, _)) = watch.uploads.newer(&mut self.seen[2]) {
            self.sample_uploads(status);
            self.uploads = Some(status.clone());
        }
        drop(watch);
        if let Some(task) = &self.folder_task
            && let Ok(status) = task.try_recv()
        {
            self.folder = Some(status);
            self.folder_task = None;
        }
        let folder = match self.selected_destination().map(|d| &d.target) {
            Some(Target::Folder { path }) => Some(path.clone()),
            _ => None,
        };
        match folder {
            Some(path)
                if self.folder_task.is_none()
                    && (self.folder.as_ref().is_none_or(|f| f.path != path)
                        || self
                            .folder_at
                            .is_none_or(|t| t.elapsed() > Duration::from_secs(2))) =>
            {
                self.folder_at = Some(Instant::now());
                self.folder_task = Some(background(move || FolderStatus {
                    free: fs2::available_space(&path).ok(),
                    mounted: volumes::ensure_present(&path).is_ok(),
                    path,
                }));
            }
            Some(_) => {}
            None => self.folder = None,
        }
        if let Some(editor) = &mut self.editor {
            editor.poll_test();
        }
        #[cfg(feature = "s3")]
        if let Some(editor) = &mut self.editor {
            editor.poll_key(Instant::now(), |id| {
                background(move || crate::s3::has_secret(&id))
            });
        }
    }

    /// Take a freshly read saved list; a chosen destination that is gone
    /// goes back to this computer.
    fn adopt(&mut self, saved: Vec<Destination>) {
        self.saved = saved;
        self.loaded = Some(Instant::now());
        if self
            .selected
            .as_ref()
            .is_some_and(|name| !self.saved.iter().any(|d| &d.name == name))
        {
            self.selected = None;
        }
    }

    pub fn editing(&self) -> bool {
        self.editor.is_some() || self.removing.is_some()
    }

    pub fn editor_open(&self) -> bool {
        self.editor.is_some()
    }

    pub fn can_undo(&self) -> bool {
        !self.mutation_busy()
            && self
                .editor
                .as_ref()
                .is_some_and(|e| !e.history.undo.is_empty())
    }

    pub fn can_redo(&self) -> bool {
        !self.mutation_busy()
            && self
                .editor
                .as_ref()
                .is_some_and(|e| !e.history.redo.is_empty())
    }

    pub fn undo_label(&self) -> Option<&str> {
        self.can_undo()
            .then(|| {
                self.editor
                    .as_ref()?
                    .history
                    .undo
                    .last()
                    .map(|change| change.label)
            })
            .flatten()
    }

    pub fn redo_label(&self) -> Option<&str> {
        self.can_redo()
            .then(|| {
                self.editor
                    .as_ref()?
                    .history
                    .redo
                    .last()
                    .map(|change| change.label)
            })
            .flatten()
    }

    pub fn undo(&mut self) -> bool {
        if !self.can_undo() {
            return false;
        }
        let editor = self.editor.as_mut().unwrap();
        let change = editor.history.undo.pop().unwrap();
        editor.restore_draft(change.before.clone());
        editor.history.redo.push(change);
        editor.history.last = None;
        true
    }

    pub fn redo(&mut self) -> bool {
        if !self.can_redo() {
            return false;
        }
        let editor = self.editor.as_mut().unwrap();
        let change = editor.history.redo.pop().unwrap();
        editor.restore_draft(change.after.clone());
        editor.history.undo.push(change);
        editor.history.last = None;
        true
    }

    pub fn mutation_busy(&self) -> bool {
        self.replay.is_some() || self.editor.as_ref().is_some_and(|e| e.saving.is_some())
    }

    pub fn take_changes(&mut self) -> Vec<Change> {
        std::mem::take(&mut self.changes)
    }

    fn selection(&self, output: &str) -> Selection {
        Selection {
            selected: self.selected.clone(),
            output: output.into(),
        }
    }

    /// Restore the exact output supplied by the app's history. Selection
    /// restoration must not strip directories as a fresh user selection does.
    pub fn restore_selection(
        &mut self,
        selected: Option<String>,
        _output: &mut String,
    ) -> Result<(), String> {
        if let Some(name) = &selected {
            let current = destination::list().map_err(|e| format!("{e:#}"))?;
            if !current.iter().any(|d| &d.name == name) {
                return Err(format!("Destination {name} no longer exists"));
            }
            self.saved = current;
        }
        if self.remember {
            destination::set_preferred(selected.as_deref()).map_err(|e| format!("{e:#}"))?;
        }
        self.selected = selected;
        self.folder = None;
        Ok(())
    }

    fn record_change(
        &mut self,
        before: Option<Destination>,
        after: Option<Destination>,
        secret: Option<SecretChange>,
        before_selection: Selection,
        output: &str,
        created_at: Instant,
    ) {
        let label = match (&before, &after) {
            (None, Some(destination)) => format!("Add destination {}", destination.name),
            (Some(destination), None) => format!("Remove destination {}", destination.name),
            (_, Some(destination)) => format!("Edit destination {}", destination.name),
            _ => return,
        };
        self.changes.push(Change {
            before,
            after,
            secret,
            before_selection,
            after_selection: self.selection(output),
            label,
            created_at,
        });
    }

    /// Credential access runs in a worker. Return true for immediate success,
    /// false when the app should await `take_replay_result` before moving its
    /// history cursor.
    pub fn replay(
        &mut self,
        change: &Change,
        undo: bool,
        output: &mut String,
    ) -> Result<bool, String> {
        if self.mutation_busy() {
            return Err("A destination change is still running".into());
        }
        let (before, after, selection) = if undo {
            (&change.after, &change.before, &change.before_selection)
        } else {
            (&change.before, &change.after, &change.after_selection)
        };
        if let Some(secret) = &change.secret {
            let (before, after, secret, selection) = (
                before.clone(),
                after.clone(),
                secret.clone(),
                selection.clone(),
            );
            self.replay = Some(Replay {
                selection: selection.clone(),
                task: background({
                    let remember = self.remember;
                    move || {
                        destination::replace_with_selection(
                            before.as_ref(),
                            after.as_ref(),
                            Some((selection.selected.as_deref(), remember)),
                            || restore_secret(&secret, undo),
                            |_| restore_secret(&secret, !undo),
                        )
                    }
                }),
            });
            return Ok(false);
        }
        destination::replace_with_selection(
            before.as_ref(),
            after.as_ref(),
            Some((selection.selected.as_deref(), self.remember)),
            || Ok(()),
            |_| Ok(()),
        )
        .map_err(|e| format!("{e:#}"))?;
        self.finish_replay(selection.clone(), output);
        Ok(true)
    }

    fn finish_replay(&mut self, selection: Selection, output: &mut String) {
        self.saved = destination::list().unwrap_or_default();
        self.loaded = Some(Instant::now());
        self.selected = selection.selected;
        *output = selection.output;
        self.folder = None;
        self.notice = None;
    }

    pub fn take_replay_result(&mut self, output: &mut String) -> Option<Result<(), String>> {
        let result = match self.replay.as_ref()?.task.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err(anyhow::anyhow!(
                "Destination restore stopped without an answer"
            )),
        };
        let replay = self.replay.take().unwrap();
        Some(
            result
                .map(|()| self.finish_replay(replay.selection, output))
                .map_err(|e| format!("{e:#}")),
        )
    }

    /// Whether a background check, save or keychain lookup is running or
    /// due, so the app keeps polling.
    pub fn busy(&self) -> bool {
        self.replay.is_some()
            || self.editor.as_ref().is_some_and(|e| {
                e.testing.is_some()
                    || e.saving.is_some()
                    || e.key_probe.is_some()
                    || e.key_changed.is_some()
            })
    }

    /// Point the editor's Advanced chevron at the group's state; from
    /// `Workbench::sync_sheets`.
    pub fn sync(&mut self, now: Instant) {
        let open = self.editor.as_ref().is_some_and(|e| e.advanced);
        self.advanced_turn.show(open, now);
    }

    /// The id of the control to focus, once, after a page or confirmation changes.
    pub fn take_focus(&mut self) -> Option<&'static str> {
        self.focus.take()
    }

    /// Whether the page changed since last asked, so the sheet's body
    /// should snap to its top.
    pub fn take_to_top(&mut self) -> bool {
        std::mem::take(&mut self.to_top)
    }

    /// Show `editor` in the manager, scrolled to its top, its first field
    /// to be focused.
    fn open_editor(&mut self, editor: Editor) {
        self.focus = Some(if editor.s3 { BUCKET_FIELD } else { NAME_FIELD });
        // A bucket with custom settings opens with Advanced open, unturned.
        self.advanced_turn
            .set(if editor.advanced { 1.0 } else { 0.0 });
        self.editor = Some(editor);
        self.removing = None;
        self.removal_focus = None;
        self.scrolled = false;
        self.to_top = true;
    }

    /// Back to the list, scrolled to its top.
    fn close_editor(&mut self) {
        self.editor = None;
        self.advanced_turn.set(0.0);
        self.removing = None;
        self.removal_focus = None;
        self.scrolled = false;
        self.to_top = true;
    }

    fn cancel_removal(&mut self) {
        if self.removing.take().is_some() {
            self.focus = Some(REMOVE_TRIGGER);
        }
    }

    /// Where a capture with this output goes, for compact displays.
    pub fn label(&self, output: &str) -> String {
        match &self.selected {
            Some(name) => format!("{name}: {output}"),
            None => output.to_string(),
        }
    }

    fn select(&mut self, name: Option<String>, output: &mut String) {
        let changed = self.selected != name;
        if changed {
            if self.remember && self.replay.is_none() {
                let _ = destination::set_preferred(name.as_deref());
            }
            self.selected = name;
            self.folder = None;
        }
        // Back on this computer, a destination's relative name goes below
        // the default capture folder again, undoing what choosing it did.
        // Only on the change itself: a relative path typed here stays.
        if changed
            && self.selected.is_none()
            && !self.recording
            && !output.is_empty()
            && Path::new(output.as_str()).is_relative()
        {
            *output = super::prefs::capture_root()
                .join(output.as_str())
                .to_string_lossy()
                .into_owned();
        }
        // A destination holds relative names; drop a leftover absolute path,
        // keeping a template's folders below the default capture folder.
        if self.selected.is_some() && Path::new(output.as_str()).is_absolute() {
            let path = Path::new(output.as_str());
            *output = match path.strip_prefix(super::prefs::capture_root()) {
                Ok(rest) if !rest.as_os_str().is_empty() => rest.to_string_lossy().into_owned(),
                _ => path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            };
        }
    }

    fn selected_destination(&self) -> Option<&Destination> {
        let name = self.selected.as_ref()?;
        self.saved.iter().find(|d| &d.name == name)
    }

    /// The folder and preset `output` names its files with; `Custom` for
    /// recordings and hand-edited templates.
    fn naming_of(&self, output: &str) -> (Option<PathBuf>, Naming) {
        match naming_presets::extension(output).filter(|_| !self.recording) {
            Some(ext) => naming_presets::split(output, ext),
            None => (None, Naming::Custom),
        }
    }

    /// `output` with its files named by `naming`, keeping its folder and
    /// extension. A hand-edited template keeps the folder before its file
    /// name when that holds no placeholder, else the default folder.
    fn with_naming(&self, output: &str, naming: Naming) -> String {
        if naming == Naming::Custom {
            return output.into();
        }
        let ext = naming_presets::extension(output).unwrap_or("png");
        let folder = match self.naming_of(output) {
            // On this computer a preset's folder is absolute: a relative
            // one, left by a destination, goes below the capture folder.
            (folder, preset) if preset != Naming::Custom => match self.selected {
                None if !folder.as_deref().is_some_and(Path::is_absolute) => {
                    let root = super::prefs::capture_root();
                    Some(folder.map_or_else(|| root.clone(), |f| root.join(f)))
                }
                _ => folder,
            },
            _ => {
                let path = Path::new(output);
                // Without an extension, the output is a folder of frames.
                let folder = if naming_presets::extension(output).is_some() {
                    path.parent()
                } else {
                    Some(path)
                }
                .filter(|f| !f.as_os_str().is_empty() && !f.to_string_lossy().contains(['{', '}']))
                .map(Path::to_path_buf);
                match self.selected {
                    None => folder
                        .filter(|f| f.is_absolute())
                        .or_else(|| Some(super::prefs::capture_root())),
                    Some(_) => folder.filter(|f| !f.is_absolute()),
                }
            }
        };
        naming_presets::join(folder.as_deref(), naming, ext)
    }

    /// What choosing `naming` under "File names" sends. A preset changes
    /// the output as typing does, so the app's undo history records it.
    fn naming_message(&self, output: &str, naming: Naming) -> Message {
        match self.with_naming(output, naming) {
            changed if naming != Naming::Custom && changed != output => Message::Output(changed),
            _ => Message::Naming(naming),
        }
    }

    /// The output after choosing `folder` with "Choose…": a preset keeps its
    /// dated folders and file names, anything else keeps its file name.
    fn browsed(&self, output: &str, folder: &Path) -> String {
        match self.naming_of(output) {
            (_, Naming::Custom) => {
                let name = Path::new(output)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "capture.png".into());
                folder.join(name).to_string_lossy().into_owned()
            }
            (_, naming) => naming_presets::join(
                Some(folder),
                naming,
                naming_presets::extension(output).unwrap_or("png"),
            ),
        }
    }

    /// The editor saved `name`: back to the list, due to reload, with its
    /// row glowing, and chosen.
    fn saved_as(&mut self, name: String, output: &mut String) {
        self.notice = Some((format!("Saved {name}"), false));
        self.saved_glow.hit(name.clone(), Instant::now());
        self.close_editor();
        self.loaded = None;
        self.select(Some(name), output);
    }

    pub fn update(&mut self, message: Message, output: &mut String) {
        // Saving now commits in its worker. Keep its form and receiver alive
        // until the result arrives, so a close or edit cannot hide a commit.
        let local_during_replay = self.replay.is_some()
            && matches!(
                message,
                Message::Output(_)
                    | Message::Naming(_)
                    | Message::Select(_)
                    | Message::BrowseOutput
            );
        if self.mutation_busy()
            && !local_during_replay
            && !matches!(message, Message::Scrolled(_) | Message::Advanced(_))
        {
            return;
        }
        match message {
            Message::Undo => {
                self.undo();
            }
            Message::Redo => {
                self.redo();
            }
            Message::Select(choice) => {
                if choice.name != self.selected {
                    self.custom_naming = false;
                }
                self.select(choice.name, output);
            }
            Message::Output(value) => {
                if value != *output {
                    // A preset chosen under "File names" hides the field;
                    // typing never does, so the field never vanishes under
                    // the caret.
                    let chosen = !single_edit(output, &value)
                        && Naming::PRESETS
                            .iter()
                            .any(|&naming| self.with_naming(output, naming) == value);
                    self.custom_naming = !chosen;
                }
                *output = value;
                self.select(self.selected.clone(), output);
            }
            Message::Naming(naming) => {
                self.custom_naming = naming == Naming::Custom;
                *output = self.with_naming(output, naming);
                self.select(self.selected.clone(), output);
            }
            Message::BrowseOutput => {
                if let Some(folder) = rfd::FileDialog::new()
                    .set_title("Save captures in")
                    .pick_folder()
                {
                    if folder.to_str().is_none() {
                        self.notice = Some((folder_encoding_error().into(), true));
                        return;
                    }
                    *output = self.browsed(output, &folder);
                }
            }
            Message::UseVolume(index) => {
                if let Some(volume) = self.volumes.get(index).cloned() {
                    self.use_volume(&volume, output);
                }
            }
            Message::OpenManager => {
                self.manager_open = true;
                self.notice = None;
                self.scrolled = false;
            }
            Message::CloseManager => {
                self.manager_open = false;
                self.close_editor();
            }
            Message::Scrolled(scrolled) => self.scrolled = scrolled,
            Message::Retry => {
                #[cfg(feature = "uploads")]
                self.watch
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .retry();
            }
            Message::Edit(name) => {
                self.removing = None;
                // FTP and SFTP rows offer no Edit action; refuse it here too.
                if let Some(destination) = self.saved.iter().find(|d| {
                    d.name == name && !matches!(d.target, Target::Ftp(_) | Target::Sftp(_))
                }) {
                    self.open_editor(Editor::edit(destination));
                }
            }
            Message::Remove(name) => {
                let created_at = Instant::now();
                self.removing = None;
                let before_selection = self.selection(output);
                let before = self.saved.iter().find(|d| d.name == name).cloned();
                let result = match &before {
                    Some(before) => destination::replace(Some(before), None),
                    None => Err(anyhow::anyhow!(
                        "Destination {name} is no longer in the list"
                    )),
                };
                match result {
                    Ok(()) => {
                        self.loaded = None;
                        self.tick(output);
                        self.record_change(
                            before,
                            None,
                            None,
                            before_selection,
                            output,
                            created_at,
                        );
                        self.notice =
                            Some((format!("Removed {name}; its files are untouched"), false));
                        self.focus = Some("sheet-close");
                    }
                    Err(error) => self.notice = Some((format!("{error:#}"), true)),
                }
            }
            Message::ConfirmRemove(Some(name)) => {
                self.removal_focus = Some(name.clone());
                self.removing = Some(name);
                self.focus = Some(REMOVE_CANCEL);
            }
            Message::ConfirmRemove(None) => self.cancel_removal(),
            Message::AddFolder => {
                self.removing = None;
                if let Some(folder) = rfd::FileDialog::new()
                    .set_title("Save captures in")
                    .pick_folder()
                {
                    if folder.to_str().is_none() {
                        self.notice = Some((folder_encoding_error().into(), true));
                        return;
                    }
                    let name = folder
                        .file_name()
                        .map(|n| safe_name(&n.to_string_lossy()))
                        .unwrap_or_else(|| "folder".into());
                    self.open_editor(Editor::new_folder(name, folder));
                }
            }
            Message::AddBucket => self.open_editor(Editor::new_bucket()),
            Message::Cancel if self.removing.is_some() => self.cancel_removal(),
            Message::Cancel => self.close_editor(),
            // The folder and keychain may be slow to answer, so saving
            // finishes in `tick`.
            Message::Save => {
                let selection = self.selection(output);
                let Some(editor) = &mut self.editor else {
                    return;
                };
                if editor.saving.is_some() || editor.checking_secret() {
                    return;
                }
                let destination = match editor.destination() {
                    Ok(destination) => destination,
                    Err(error) => return editor.refuse(error),
                };
                if let Some(why) = editor.name_taken(&self.saved) {
                    return editor.refuse_name(why);
                }
                let before = editor
                    .original
                    .as_ref()
                    .and_then(|original| self.saved.iter().find(|d| &d.name == original))
                    .cloned();
                if editor.original.is_some() && before.is_none() {
                    return editor.refuse(anyhow::anyhow!(
                        "This destination was removed; reopen the editor"
                    ));
                }
                if editor.secret_to_store().is_some() {
                    // Its answer would be about the keychain before the store.
                    editor.key_probe = None;
                }
                editor.message = None;
                let prepare = editor.prepare(&destination, before.clone(), self.remember);
                editor.saving = Some((destination, background(prepare)));
                self.saving_context = Some(SaveContext {
                    before,
                    selection,
                    created_at: Instant::now(),
                });
            }
            Message::Test => {
                let Some(editor) = &mut self.editor else {
                    return;
                };
                if editor.saving.is_some() {
                    return;
                }
                let destination = match editor.destination() {
                    Ok(destination) => destination,
                    Err(error) => return editor.refuse(error),
                };
                // A check uses the typed key transiently. Testing or canceling
                // a draft must never overwrite a saved destination's key.
                let secret = editor.secret_to_store().map(|(_, secret)| secret);
                editor.message = None;
                editor.invalid.clear();
                editor.testing = Some(background(move || test(destination, secret)));
            }
            message => {
                let Some(editor) = &mut self.editor else {
                    return;
                };
                let before = editor.draft();
                let (label, coalesce) = match &message {
                    Message::Name(_) => ("Destination name", true),
                    Message::Folder(_) => ("Destination folder", true),
                    Message::BrowseFolder => ("Destination folder", false),
                    Message::Provider(_) => ("Bucket provider", false),
                    Message::Endpoint(_) => ("Bucket endpoint", true),
                    Message::Region(_) => ("Bucket region", true),
                    Message::Bucket(_) => ("Bucket name", true),
                    Message::Prefix(_) => ("Bucket key prefix", true),
                    Message::Source(_) => ("Credential source", false),
                    Message::AccessKey(_) => ("Access key ID", true),
                    Message::Secret(_) => ("Secret key", true),
                    Message::Profile(_) => ("AWS profile", true),
                    Message::PathStyle(_) => ("Bucket addressing", false),
                    Message::KeepLocal(_) => ("Keep local copy", false),
                    _ => ("Destination", false),
                };
                let field = match message {
                    Message::Name(value) => {
                        editor.name = value;
                        editor.name_touched = true;
                        Some(Field::Name)
                    }
                    Message::Folder(value) => {
                        editor.folder = value;
                        Some(Field::Folder)
                    }
                    Message::BrowseFolder => {
                        let Some(folder) = rfd::FileDialog::new().pick_folder() else {
                            return;
                        };
                        let Some(folder) = folder.to_str() else {
                            editor.invalid.retain(|(field, _)| *field != Field::Folder);
                            editor
                                .invalid
                                .push((Field::Folder, folder_encoding_error().into()));
                            return;
                        };
                        editor.folder = folder.into();
                        Some(Field::Folder)
                    }
                    Message::Provider(Provider(index)) => {
                        if index == editor.provider {
                            return;
                        }
                        let (_, endpoint, region, path_style) = PROVIDERS[index];
                        editor.provider = index;
                        editor.endpoint = endpoint.into();
                        editor.region = region.into();
                        editor.path_style = path_style;
                        editor.edited(Some(Field::Region));
                        Some(Field::Endpoint)
                    }
                    Message::Endpoint(value) => {
                        editor.endpoint = value;
                        Some(Field::Endpoint)
                    }
                    Message::Region(value) => {
                        editor.region = value;
                        Some(Field::Region)
                    }
                    Message::Bucket(value) => {
                        if !editor.name_touched {
                            editor.name = safe_name(value.trim());
                            editor.edited(Some(Field::Name));
                        }
                        editor.bucket = value;
                        Some(Field::Bucket)
                    }
                    Message::Prefix(value) => {
                        editor.prefix = value;
                        Some(Field::Prefix)
                    }
                    Message::Source(source) => {
                        editor.source = source;
                        None
                    }
                    Message::AccessKey(value) => {
                        editor.access_key_id = value;
                        editor.key_edited(Instant::now());
                        Some(Field::AccessKey)
                    }
                    Message::Secret(value) => {
                        editor.secret = value;
                        Some(Field::Secret)
                    }
                    Message::Profile(value) => {
                        editor.profile = value;
                        Some(Field::Profile)
                    }
                    Message::PathStyle(value) => {
                        editor.path_style = value;
                        None
                    }
                    Message::KeepLocal(value) => {
                        editor.keep_local = value;
                        None
                    }
                    // Opening the group changes nothing a check looked at.
                    Message::Advanced(open) => {
                        editor.advanced = open;
                        return;
                    }
                    _ => return,
                };
                editor.edited(field);
                let after = editor.draft();
                editor.history.record(before, after, label, coalesce);
            }
        }
    }

    /// Save to `Capturefab/` on a drive, reusing a destination already there.
    fn use_volume(&mut self, volume: &Volume, output: &mut String) {
        let created_at = Instant::now();
        let folder = volume.path.join("Capturefab");
        if let Some(existing) = self
            .saved
            .iter()
            .find(|d| matches!(&d.target, Target::Folder { path } if path == &folder))
        {
            let name = existing.name.clone();
            self.select(Some(name), output);
            return;
        }
        let name: String = safe_name(&volume.name)
            .trim_matches(['-', '.'])
            .chars()
            .take(48)
            .collect();
        let mut editor = Editor::new_folder(
            if name.is_empty() {
                "drive".into()
            } else {
                name
            },
            folder,
        );
        let before_selection = self.selection(output);
        match editor.name_taken(&self.saved) {
            Some(why) => editor.refuse_name(why),
            None => match editor.save(self.remember) {
                Ok(name) => {
                    let after = editor.destination().ok();
                    self.loaded = None;
                    self.tick(output);
                    self.select(Some(name.clone()), output);
                    self.record_change(None, after, None, before_selection, output, created_at);
                    self.notice = Some((format!("Saving to {name}"), false));
                    return;
                }
                Err(error) => editor.refuse(error),
            },
        }
        // Show the form so the name or folder can be adjusted.
        self.open_editor(editor);
        self.manager_open = true;
    }

    /// The "Save to" row and output field. Drive shortcuts are a separate
    /// section so the parent can keep its primary action ahead of them. `output`
    /// is the path on this computer, or a name inside the destination.
    pub fn view<'a>(
        &'a self,
        output: &'a str,
        local_label: &'a str,
        p: &'static Palette,
        output_error: Option<String>,
    ) -> Element<'a, Message> {
        let choices: Vec<Choice> = std::iter::once(Choice {
            name: None,
            label: "This computer".into(),
        })
        .chain(self.saved.iter().map(|d| Choice {
            name: Some(d.name.clone()),
            label: format!(
                "{} ({})",
                d.name,
                match d.target {
                    Target::Folder { .. } => "folder",
                    Target::S3(_) => "S3",
                    Target::Ftp(_) => "FTP",
                    Target::Sftp(_) => "SFTP",
                }
            ),
        }))
        .collect();
        let selected = choices.iter().find(|c| c.name == self.selected).cloned();
        // Each field's action sits on its label's line, so the fields take
        // the panel's full width like the buttons under them.
        let mut content = column![
            labeled(
                "Save to",
                super::tip(
                    label_action("Manage…", Message::OpenManager),
                    "Manage folders, external drives, S3 buckets and FTP/SFTP servers",
                ),
                p,
            ),
            pick_list(choices, selected, Message::Select)
                .name("Save destination")
                .width(Fill)
                .text_size(style::BODY)
                .padding([6, 10])
                .style(style::pick)
                .menu_style(style::menu),
        ]
        .spacing(6);
        if !self.volumes.is_empty() {
            content = content.push(self.drive_shortcuts(p));
        }
        let (local_example, named_example) = if self.recording {
            ("recording.mp4", "recording.mkv")
        } else {
            ("capture.png", "capture.png")
        };
        let (folder, naming) = self.naming_of(output);
        let shown = if self.custom_naming {
            Naming::Custom
        } else {
            naming
        };
        let destination = self.selected_destination();
        // Stills name their files with a preset; the template shows when it
        // is chosen, hand-edited or refused. Recordings keep their field.
        let preset = !self.recording && shown != Naming::Custom && output_error.is_none();
        let simple = preset
            && match destination {
                // A subfolder in a destination is customised, so it shows.
                Some(_) => folder.is_none(),
                None => folder.as_deref().is_some_and(Path::is_absolute),
            };
        // The field shows the home folder as `~`, so the part that varies
        // stays in view; what it sends, and its description, are the path.
        let input = |placeholder: &'a str, name: &'a str| {
            let shown = home_text(output);
            let mut field = text_input(placeholder, &shown);
            if shown != output {
                field = field.description(output);
            }
            field
                .name(name)
                .invalid(output_error.clone())
                .id(if self.recording {
                    "record-output"
                } else {
                    "capture-output"
                })
                .on_input(|value| Message::Output(home_long(&value)))
                .size(style::BODY)
                .padding(if self.recording { [6, 10] } else { [3, 10] })
                .style(if output_error.is_some() {
                    style::input_invalid
                } else {
                    style::input
                })
        };
        if self.recording {
            // Recordings keep their field: a stream URL or a file name.
            match destination {
                None => {
                    content = content
                        .push(labeled(
                            local_label,
                            super::tip(
                                label_action("Choose…", Message::BrowseOutput),
                                "Choose a folder",
                            ),
                            p,
                        ))
                        .push(input(local_example, local_label));
                }
                Some(destination) => {
                    content = content
                        .push(label("Recording file name in destination", p))
                        .push(input(named_example, "Recording file name in destination"))
                        .push(self.summary(destination, p));
                }
            }
        } else {
            // "File names" and the line under it share a label column, so
            // their controls start together and take the rest of the width.
            let names = row![
                side_label("File names", p),
                super::tip(
                    pick_list(
                        &Naming::ALL[..],
                        Some(if preset && (simple || destination.is_some()) {
                            shown
                        } else {
                            Naming::Custom
                        }),
                        move |naming| self.naming_message(output, naming),
                    )
                    .name("File names")
                    .width(Fill)
                    .text_size(style::BODY)
                    .padding([5, 8])
                    .style(style::pick)
                    .menu_style(style::menu),
                    "How captured files are named. By date, camera and time is recommended.",
                ),
            ]
            .spacing(6)
            .align_y(Alignment::Center);
            content = content.push(names);
            // The template, with what it may hold in its tooltip.
            let template = |placeholder: &'a str, name: &'a str| {
                super::tip(input(placeholder, name), PLACEHOLDERS)
            };
            match destination {
                // The folder line and the template share one slot as tall
                // as the field, so choosing either moves nothing below.
                None => {
                    let line: Element<'a, Message> = if simple {
                        let folder = folder.unwrap_or_default();
                        row![
                            side_label("Folder", p),
                            super::tip(
                                one_line(home_short(&folder), style::BODY, style::SANS, p.text)
                                    .width(Fill),
                                folder.display(),
                            ),
                            super::tip(
                                label_action("Choose…", Message::BrowseOutput),
                                "Choose a folder",
                            ),
                        ]
                        .spacing(6)
                        .align_y(Alignment::Center)
                        .into()
                    } else {
                        row![
                            super::tip(side_label("Pattern", p), PLACEHOLDERS),
                            template(local_example, local_label),
                            super::tip(
                                button(icon(Icon::Folder, 15.0, p.secondary))
                                    .padding(4)
                                    .style(style::plain)
                                    .on_press(Message::BrowseOutput),
                                "Choose a folder",
                            ),
                        ]
                        .spacing(6)
                        .align_y(Alignment::Center)
                        .into()
                    };
                    content = content.push(
                        container(line)
                            .height(FIELD_HEIGHT)
                            .align_y(Alignment::Center),
                    );
                }
                Some(destination) => {
                    if !simple {
                        content =
                            content.push(template(named_example, "Capture name in destination"));
                    }
                    content = content.push(self.summary(destination, p));
                }
            }
        }
        if let Some(error) = output_error {
            content = content.push(field_error(error, p));
        }
        content.into()
    }

    /// One-click destinations on the external drives, under "Save to".
    fn drive_shortcuts(&self, p: &'static Palette) -> Element<'_, Message> {
        let mut content = column![].spacing(6);
        if !self.volumes.is_empty() {
            let current = match self.selected_destination().map(|d| &d.target) {
                Some(Target::Folder { path }) => Some(path),
                _ => None,
            };
            let mut chips = self
                .volumes
                .iter()
                .enumerate()
                .map(|(index, volume)| {
                    let (glyph, kind) = if volume.kind == "network" {
                        (Icon::Network, "network")
                    } else {
                        (Icon::HardDrives, "external")
                    };
                    // The drive saved to now; pressing it again reselects it.
                    let chosen =
                        current.is_some_and(|path| *path == volume.path.join("Capturefab"));
                    super::tip(
                        button(
                            row![
                                // The drive's glyph, a check once chosen; one
                                // icon either way, so the chip keeps its width.
                                icon(
                                    if chosen { Icon::Check } else { glyph },
                                    12.0,
                                    if chosen { p.ink(p.accent) } else { p.secondary },
                                ),
                                text(format!(
                                    "{} · {} free",
                                    volume.name,
                                    bytes(volume.free_bytes)
                                ))
                                .size(style::CAPTION),
                            ]
                            .spacing(5)
                            .align_y(Alignment::Center),
                        )
                        .padding([4, 8])
                        .style(style::toggle(chosen))
                        .selected(chosen)
                        .on_press(Message::UseVolume(index)),
                        format!(
                            "Save to a Capturefab folder on this {kind} drive: {} ({} of {} free)",
                            volume.path.display(),
                            bytes(volume.free_bytes),
                            bytes(volume.total_bytes)
                        ),
                    )
                })
                .collect::<Vec<_>>()
                .into_iter();
            // The lead shares the first chip's line, cut short before the
            // chip is, so it never adds a line of its own.
            let first = chips.next().expect("a drive");
            content = content.push(
                row![
                    one_line("Or save to a drive", style::SMALL, style::SANS, p.secondary)
                        .yield_room(),
                    first,
                ]
                .spacing(6)
                .width(Fill)
                .align_y(Alignment::Center),
            );
            let rest = chips.fold(row![], |drives, chip| drives.push(chip));
            if self.volumes.len() > 1 {
                content = content.push(rest.spacing(6).wrap().vertical_spacing(6));
            }
        }
        content.into()
    }

    fn summary<'a>(
        &'a self,
        destination: &'a Destination,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        match &destination.target {
            Target::Folder { path } => {
                let status = self.folder.as_ref().filter(|f| &f.path == path);
                let mut lines = column![
                    text(format!(
                        "{}{}",
                        path.display(),
                        status
                            .and_then(|s| s.free)
                            .map(|f| format!(" · {} free", bytes(f)))
                            .unwrap_or_default()
                    ))
                    .size(style::CAPTION)
                    .color(p.secondary)
                ]
                .spacing(4);
                if status.is_some_and(|s| !s.mounted) {
                    lines = lines.push(
                        text("Drive not connected. Connect it or choose another destination.")
                            .size(style::CAPTION)
                            .color(p.ink(p.warn)),
                    );
                }
                lines.into()
            }
            Target::S3(bucket) => {
                let mut lines = column![
                    text(format!(
                        "s3://{}/{} · staged locally, then uploaded{}",
                        bucket.bucket,
                        bucket.prefix,
                        if bucket.keep_local {
                            " (local copy kept)"
                        } else {
                            ""
                        }
                    ))
                    .size(style::CAPTION)
                    .color(p.secondary)
                ]
                .spacing(4);
                if bucket.insecure() {
                    lines = lines.push(
                        text("Plain http: requests are signed but data is not encrypted.")
                            .size(style::CAPTION)
                            .color(p.ink(p.warn)),
                    );
                }
                lines.push(self.upload_status(&destination.name, p)).into()
            }
            Target::Ftp(_) | Target::Sftp(_) => {
                let remote = destination.target.remote().expect("network destination");
                let (user, keep_local) = match &destination.target {
                    Target::Ftp(server) => (&server.user, server.keep_local),
                    Target::Sftp(server) => (&server.user, server.keep_local),
                    _ => unreachable!(),
                };
                let mut lines = column![
                    text(format!(
                        "{} as {user} · staged locally, then uploaded{}",
                        remote.location(),
                        if keep_local { " (local copy kept)" } else { "" }
                    ))
                    .size(style::CAPTION)
                    .color(p.secondary)
                ]
                .spacing(4);
                if remote.insecure() {
                    lines = lines.push(
                        text("Files and password travel unencrypted.")
                            .size(style::CAPTION)
                            .color(p.ink(p.warn)),
                    );
                }
                lines.push(self.upload_status(&destination.name, p)).into()
            }
        }
    }

    /// Add the bytes waiting to upload to their history, flagging readings
    /// where uploads failed; an empty queue clears it. The queue counts a
    /// file only once it is fully sent, so this shows whether uploads keep
    /// up rather than a transfer speed.
    fn sample_uploads(&mut self, status: &Value) {
        let count = |key: &str| status[key].as_u64().unwrap_or(0);
        if count("pending") + count("uploading") == 0 {
            self.backlog.clear();
        } else {
            self.backlog.offer(
                Instant::now(),
                count("waiting_bytes") as f64,
                count("failures"),
            );
        }
    }

    /// A destination's waiting and failed uploads, with a retry button.
    fn upload_status<'a>(&'a self, destination: &str, p: &'static Palette) -> Element<'a, Message> {
        let Some(status) = &self.uploads else {
            return column![].into();
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let line = UploadLine::of(status, destination, now);
        let ink = match line.mood {
            Mood::Calm => p.secondary,
            Mood::Waiting => p.ink(p.warn),
            Mood::Failed => p.ink(p.danger),
        };
        // The summary wraps; the action keeps its line.
        let mut head = row![
            text(line.summary)
                .size(style::CAPTION)
                .color(ink)
                .width(Fill)
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if cfg!(feature = "uploads")
            && let Some(action) = line.action
        {
            head = head.push(
                button(
                    text(action)
                        .size(style::CAPTION)
                        .wrapping(text::Wrapping::None),
                )
                .padding([2, 6])
                .style(style::link)
                .on_press(Message::Retry),
            );
        }
        let mut lines = column![head].spacing(4);
        if line.waiting > 0
            && let Some(left) = self.backlog.last()
        {
            lines = lines.push(
                row![
                    super::spark(
                        Some(&self.backlog),
                        p.accent,
                        p.danger,
                        (140.0, 18.0),
                        |history| {
                            super::format::trend(
                                history,
                                "Waiting to upload, all buckets",
                                |left| bytes(left as u64),
                                "failed uploads",
                            )
                        },
                    ),
                    text(format!("{} to go", bytes(left as u64)))
                        .size(style::CAPTION)
                        .color(p.secondary),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
        }
        if let Some(error) = line.error {
            let ink = if line.mood == Mood::Failed {
                ink
            } else {
                p.secondary
            };
            lines = lines.push(text(error.to_owned()).size(style::CAPTION).color(ink));
        }
        if !status["uploader_running"].as_bool().unwrap_or(true) && line.waiting > 0 {
            lines = lines.push(
                text("No uploader is running; keep this window open to upload.")
                    .size(style::CAPTION)
                    .color(p.ink(p.warn)),
            );
        }
        lines.into()
    }

    /// The destination manager, shown as a sheet over the workbench: the
    /// list, or an editor in its place. `max_height` follows the window,
    /// `spin` steps a running check's spinner and `now` fades the glow on a
    /// row just saved and turns the Advanced chevron.
    pub fn manager(
        &self,
        p: &'static Palette,
        max_height: f32,
        spin: usize,
        now: Instant,
        undo_label: Option<&str>,
        redo_label: Option<&str>,
    ) -> Element<'_, Message> {
        let page = match &self.editor {
            None => self.list(p, now, undo_label, redo_label),
            Some(editor) => self.form(editor, p, spin, now),
        }
        .size(SHEET_WIDTH, max_height)
        .scrolled(self.scrolled)
        .on_scroll(Message::Scrolled);
        // To iced both pages are one sheet, whose body would keep its
        // scroll; the workbench snaps it to the top as the page changes
        // (see `take_to_top`).
        page.into()
    }

    fn list(
        &self,
        p: &'static Palette,
        now: Instant,
        undo_label: Option<&str>,
        redo_label: Option<&str>,
    ) -> SheetFrame<'_, Message> {
        let mut body = column![
            history_controls(undo_label, redo_label, p),
            text(
                "Save captures and recordings to a folder or drive, or upload them to an S3 bucket."
            )
            .size(style::SMALL)
            .color(p.secondary),
        ]
        .spacing(14);
        if let Some((message, error)) = &self.notice {
            let outcome = if *error { Status::Failed } else { Status::Done };
            body = body.push(status(message.clone(), outcome, 0, p));
        }
        let frame = |body| sheet_frame("Destinations", Message::CloseManager, body, p);
        if self.saved.is_empty() {
            return frame(
                body.push(
                    column![
                        choice(
                            Icon::Folder,
                            "Folder or drive",
                            "This computer, an external or a network drive",
                            Message::AddFolder,
                            p,
                        ),
                        choice(
                            Icon::CloudArrowUp,
                            "S3 bucket",
                            "Amazon S3, Cloudflare R2, Backblaze B2, MinIO and others",
                            Message::AddBucket,
                            p,
                        ),
                        server_note(p),
                    ]
                    .spacing(8),
                ),
            );
        }
        let rows = self.saved.iter().fold(column![].spacing(2), |rows, d| {
            rows.push(self.row(d, p, now))
        });
        let add = |glyph, label: &'static str, on| {
            button(
                row![icon(glyph, 14.0, p.text), text(label).size(style::BODY)]
                    .spacing(6)
                    .align_y(Alignment::Center),
            )
            .name(label)
            .padding([7, 12])
            .style(style::secondary)
            .on_press(on)
        };
        frame(body.push(rows)).footer(
            column![
                row![
                    add(Icon::FolderPlus, "Add folder…", Message::AddFolder),
                    super::tip(
                        add(Icon::CloudArrowUp, "Add S3 bucket…", Message::AddBucket)
                            .description(S3_SERVICES),
                        S3_SERVICES,
                    ),
                ]
                .spacing(8),
                text("S3 buckets include Amazon S3, Cloudflare R2, Backblaze B2 and MinIO.")
                    .size(style::CAPTION)
                    .color(p.secondary),
                server_note(p),
            ]
            .spacing(6),
        )
    }

    /// A saved destination, glowing for a moment after it was saved.
    fn row<'a>(
        &'a self,
        destination: &'a Destination,
        p: &'static Palette,
        now: Instant,
    ) -> Element<'a, Message> {
        let name = destination.name.clone();
        let (first, second) = if self.removing.as_deref() == Some(name.as_str()) {
            let cancel = button(text("Cancel").size(style::SMALL))
                .name(format!("Cancel removal of {name}"))
                .id(REMOVE_CANCEL)
                .style(style::plain)
                .on_press(Message::ConfirmRemove(None));
            let remove = button(text("Remove").size(style::SMALL))
                .name(format!("Remove destination {name}"))
                .style(style::danger)
                .on_press(Message::Remove(name));
            super::widgets::dialog_actions(cancel, remove, false)
        } else {
            let remove = button(text("Remove").size(style::SMALL))
                .name(format!("Remove destination {name}"))
                .style(style::plain)
                .on_press(Message::ConfirmRemove(Some(name.clone())));
            let remove = if self.removal_focus.as_deref() == Some(name.as_str()) {
                remove.id(REMOVE_TRIGGER)
            } else {
                remove
            };
            let edit = if matches!(destination.target, Target::Ftp(_) | Target::Sftp(_)) {
                // The form cannot represent a server yet; without a press
                // action the button reads as a caption.
                button(text("Edit with CLI").size(style::SMALL))
                    .name(format!("{SERVER_EDIT_HINT} {name}"))
                    .description(destination.describe())
                    .style(style::link)
            } else {
                button(text("Edit…").size(style::SMALL))
                    .name(format!("Edit destination {name}"))
                    .description(destination.describe())
                    .style(style::link)
                    .on_press(Message::Edit(name))
            };
            (edit, remove)
        };
        let description = destination.describe();
        let mut info = column![
            text(destination.name.as_str())
                .size(style::BODY)
                .font(style::SEMIBOLD),
            super::tip(
                one_line(
                    row_summary(destination),
                    style::CAPTION,
                    style::SANS,
                    p.secondary
                )
                .width(Fill),
                description.clone(),
            ),
        ]
        .spacing(2)
        .width(Fill);
        if let Some(hint) = server_hint(&destination.target) {
            info = info.push(one_line(hint, style::CAPTION, style::SANS, p.tertiary).width(Fill));
        }
        let glow = self.saved_glow.level(destination.name.as_str(), now);
        container(
            row![
                icon(
                    match destination.target {
                        Target::Folder { .. } => Icon::Folder,
                        Target::S3(_) => Icon::CloudArrowUp,
                        Target::Ftp(_) | Target::Sftp(_) => Icon::Network,
                    },
                    15.0,
                    p.accent,
                ),
                info,
                first.padding([3, 8]),
                second.padding([3, 8]),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        )
        .padding([8, 10])
        .style(move |_| container::Style {
            background: (glow > 0.0).then(|| fade(p.accent_soft, glow).into()),
            border: iced::border::rounded(style::RADIUS),
            ..container::Style::default()
        })
        .into()
    }

    fn form<'a>(
        &'a self,
        editor: &'a Editor,
        p: &'static Palette,
        spin: usize,
        now: Instant,
    ) -> SheetFrame<'a, Message> {
        let editable = !self.mutation_busy();
        let saving = editor.saving.is_some();
        let can_save = editable && !editor.checking_secret() && !editor.read_only;
        let field = |label: &'a str, control: Element<'a, Message>| -> Element<'a, Message> {
            row![
                text(label)
                    .size(style::BODY)
                    .color(p.secondary)
                    .width(Length::Fixed(LABEL)),
                control,
            ]
            .spacing(12)
            .align_y(Alignment::Center)
            .into()
        };
        // A field with why it was refused under its control, if it was.
        let checked = |kind: Field, row: Element<'a, Message>| -> Element<'a, Message> {
            match editor.refusal(kind) {
                Some(message) => column![row, field("", field_error(capitalize(message), p))]
                    .spacing(4)
                    .into(),
                None => row,
            }
        };
        let input = |placeholder: &'a str, value: &'a str, on: fn(String) -> Message, kind| {
            let look: fn(&Theme, text_input::Status) -> text_input::Style =
                if editor.refusal(kind).is_some() {
                    style::input_invalid
                } else {
                    style::input
                };
            text_input(placeholder, value)
                .name(match kind {
                    Field::Name => "Destination name",
                    Field::Folder => "Destination folder",
                    Field::Endpoint => "S3 endpoint",
                    Field::Region => "S3 region",
                    Field::Bucket => "S3 bucket",
                    Field::Prefix => "S3 key prefix",
                    Field::AccessKey => "S3 access key ID",
                    Field::Secret => "S3 secret access key",
                    Field::Profile => "AWS profile",
                })
                .invalid(editor.refusal(kind).map(str::to_owned))
                .on_input_maybe(editable.then_some(on))
                .on_submit_maybe(can_save.then_some(Message::Save))
                .size(style::BODY)
                .padding([6, 10])
                .style(look)
        };
        let name_example = if editor.name_touched {
            "archive"
        } else {
            "Same as the bucket"
        };
        let name = checked(
            Field::Name,
            field(
                "Name",
                super::tip(
                    input(name_example, &editor.name, Message::Name, Field::Name).id(NAME_FIELD),
                    "Letters, digits, '.', '-' and '_'. Used with --destination on the command line.",
                ),
            ),
        );
        // A success fits the footer; only a problem repeats in full.
        let status_visible =
            !saving && editor.testing.is_none() && matches!(editor.message, Some((_, true)));
        let full_status: Element<'a, Message> =
            if status_visible && let Some((message, _)) = &editor.message {
                // Full service/path errors remain readable in the scroll body;
                // they must not consume the pinned actions' entire viewport.
                status(message.clone(), Status::Failed, spin, p)
            } else {
                space().into()
            };
        let mut form = column![].spacing(10);
        if editor.s3 {
            form = form.push(field(
                "Service",
                pick_list(
                    (0..PROVIDERS.len()).map(Provider).collect::<Vec<_>>(),
                    Some(Provider(editor.provider)),
                    Message::Provider,
                )
                .name("S3-compatible service")
                .enabled(editable)
                .width(Fill)
                .text_size(style::BODY)
                .padding([6, 10])
                .style(style::pick)
                .menu_style(style::menu)
                .into(),
            ));
            if editor.provider != 0 {
                form = form.push(checked(
                    Field::Endpoint,
                    field(
                        "Endpoint",
                        input(
                            "https://",
                            &editor.endpoint,
                            Message::Endpoint,
                            Field::Endpoint,
                        )
                        .into(),
                    ),
                ));
            }
            form = form.push(checked(
                Field::Bucket,
                field(
                    "Bucket",
                    input(
                        "bucket-name",
                        &editor.bucket,
                        Message::Bucket,
                        Field::Bucket,
                    )
                    .id(BUCKET_FIELD)
                    .into(),
                ),
            ));
            // Cloudflare R2 takes "auto", so its region waits under Advanced.
            let region_advanced = editor.region_advanced();
            let region = || {
                checked(
                    Field::Region,
                    field(
                        "Region",
                        input("us-east-1", &editor.region, Message::Region, Field::Region).into(),
                    ),
                )
            };
            if !region_advanced {
                form = form.push(region());
            }
            // An access key is the usual way in; a profile or the environment
            // are chosen under Advanced.
            if editor.source == Source::Keychain {
                let mut secret = row![
                    text_input(
                        if editor.stored_secret {
                            "Type to replace"
                        } else {
                            "Secret access key"
                        },
                        &editor.secret,
                    )
                    .name("S3 secret access key")
                    .invalid(editor.refusal(Field::Secret).map(str::to_owned))
                    .on_input_maybe(editable.then_some(Message::Secret))
                    .on_submit_maybe(can_save.then_some(Message::Save))
                    .secure(true)
                    .size(style::BODY)
                    .padding([6, 10])
                    .style(if editor.refusal(Field::Secret).is_some() {
                        style::input_invalid
                    } else {
                        style::input
                    })
                ]
                .spacing(8)
                .align_y(Alignment::Center);
                if editor.stored_secret {
                    secret = secret.push(stored_badge(p));
                }
                form = form
                    .push(checked(
                        Field::AccessKey,
                        field(
                            "Access key ID",
                            input(
                                "AKIA…",
                                &editor.access_key_id,
                                Message::AccessKey,
                                Field::AccessKey,
                            )
                            .into(),
                        ),
                    ))
                    .push(checked(Field::Secret, field("Secret key", secret.into())))
                    .push(field("", caption(KEYCHAIN_NOTE, p)));
            }
            let open = editor.advanced;
            form = form
                .push(name)
                .push(field(
                    "",
                    super::widgets::checkbox_maybe(
                        "Keep local copies after upload",
                        editor.keep_local,
                        editable.then_some(Message::KeepLocal),
                    ),
                ))
                .push(disclosure(
                    "Advanced",
                    (!open).then(|| advanced_summary(editor)),
                    self.advanced_turn.get(now),
                    Message::Advanced(!open),
                    p,
                ));
            if open {
                form = form
                    .push(checked(
                        Field::Prefix,
                        field(
                            "Folder in bucket",
                            super::tip(
                                input(
                                    "cameras/line-1/",
                                    &editor.prefix,
                                    Message::Prefix,
                                    Field::Prefix,
                                ),
                                "S3 key prefix; files are uploaded under it",
                            ),
                        ),
                    ))
                    .push(field(
                        "",
                        super::tip(
                            super::widgets::checkbox_maybe(
                                "Path-style addressing",
                                editor.path_style,
                                editable.then_some(Message::PathStyle),
                            ),
                            "Needed by MinIO and most non-AWS services",
                        ),
                    ));
                if region_advanced {
                    form = form.push(region());
                }
                form = form.push(field(
                    "Credentials",
                    pick_list(&Source::ALL[..], Some(editor.source), Message::Source)
                        .name("Credentials source")
                        .enabled(editable)
                        .width(Fill)
                        .text_size(style::BODY)
                        .padding([6, 10])
                        .style(style::pick)
                        .menu_style(style::menu)
                        .into(),
                ));
                form = match editor.source {
                    Source::Keychain => form,
                    Source::Profile => form.push(checked(
                        Field::Profile,
                        field(
                            "Profile",
                            input("default", &editor.profile, Message::Profile, Field::Profile)
                                .into(),
                        ),
                    )),
                    Source::Environment => form.push(field(
                        "",
                        caption(
                            "AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY of the process that uploads",
                            p,
                        ),
                    )),
                };
            }
        } else {
            form = form.push(name).push(checked(
                Field::Folder,
                field(
                    "Folder",
                    row![
                        input(
                            "/Volumes/Drive/Capturefab",
                            &editor.folder,
                            Message::Folder,
                            Field::Folder
                        ),
                        super::tip(
                            button(icon(Icon::Folder, 15.0, p.secondary))
                                .padding(6)
                                .style(style::plain)
                                .on_press_maybe(editable.then_some(Message::BrowseFolder)),
                            "Choose a folder",
                        ),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center)
                    .into(),
                ),
            ));
        }
        let testing = editor.testing.is_some();
        let outcome = if saving {
            compact_status("Saving…".into(), Status::Working, spin, p)
        } else if testing {
            let doing = if editor.s3 {
                "Checking the bucket…"
            } else {
                "Checking the folder…"
            };
            compact_status(doing.into(), Status::Working, spin, p)
        } else if let Some((message, error)) = &editor.message {
            let outcome = if *error { Status::Failed } else { Status::Done };
            super::tip(compact_status(message.clone(), outcome, spin, p), message)
        } else {
            space().into()
        };
        let cancel = button(text("Cancel").size(style::BODY))
            .padding([7, 14])
            .style(style::plain)
            .on_press_maybe(editable.then_some(Message::Cancel));
        let save = button(text("Save").size(style::BODY))
            .padding([7, 18])
            .style(style::primary)
            .on_press_maybe(can_save.then_some(Message::Save));
        let (first, second) = super::widgets::dialog_actions(cancel, save, false);
        let mut footer = row![];
        // Shown once there is something to undo, and kept while a save
        // briefly disables them, so Test never shifts under the pointer.
        if !editor.history.undo.is_empty() || !editor.history.redo.is_empty() {
            footer = footer.extend(history_buttons(self.undo_label(), self.redo_label()));
        }
        let footer = footer
            .push(super::tip(
                button(text("Test").size(style::BODY))
                    .padding([7, 14])
                    .style(style::secondary)
                    .on_press_maybe((!testing && editable).then_some(Message::Test)),
                if editor.s3 {
                    "Check that the bucket can be reached with these credentials"
                } else {
                    "Check that the folder can be written to"
                },
            ))
            .push(container(outcome).width(Fill).padding([0, 4]))
            .push(first)
            .push(second)
            .spacing(8)
            .align_y(Alignment::Center);
        let title = match &editor.original {
            Some(name) => format!("Edit {name}"),
            None if editor.s3 => "New S3 bucket".into(),
            None => "New folder".into(),
        };
        // Keep the outcome after the fields in a permanent slot. Background
        // checks must not move an active editor or rebuild its widget state.
        let body = column![form, full_status].spacing(if status_visible { 12.0 } else { 0.0 });
        sheet_frame(title, Message::CloseManager, body, p)
            .back(Some(Message::Cancel))
            .back_enabled(editable)
            .close_enabled(editable)
            .default_action(can_save.then_some(Message::Save))
            .footer(footer)
    }

    /// Put the manager in a named state for a screenshot; see
    /// `Workbench::scene_sheets`. Whether `word` was one of its own:
    /// `destinations-list` (two saved destinations, one just saved; it
    /// saves them in the session folder, so give the screenshot its own),
    /// `folder-editor`, `bucket-editor` (filled in, its secret stored and
    /// checked) and `testing` (after an editor word: its check never ends).
    /// `capture-custom` shows the capture panel's template field instead,
    /// and `uploads-offline` the capture panel saving to a bucket that has
    /// been out of reach for a while.
    pub(super) fn scene(&mut self, word: &str, output: &mut String) -> bool {
        match word {
            "capture-custom" => {
                self.custom_naming = true;
                return true;
            }
            "destinations-list" | "destinations-remove" | "uploads-offline" => {
                for (name, target) in [
                    (
                        "archive",
                        Target::Folder {
                            path: "/Volumes/Archive/Capturefab".into(),
                        },
                    ),
                    (
                        "line-1",
                        Target::S3(Bucket {
                            endpoint: "https://ACCOUNT_ID.r2.cloudflarestorage.com".into(),
                            region: "auto".into(),
                            bucket: "line-1-captures".into(),
                            prefix: DEFAULT_PREFIX.into(),
                            path_style: true,
                            credentials: Credentials::Keychain {
                                access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
                            },
                            keep_local: false,
                            no_overwrite: NoOverwrite::IfNoneMatch,
                        }),
                    ),
                ] {
                    let _ = destination::save(Destination {
                        name: name.into(),
                        target,
                    });
                }
                self.saved = destination::list().unwrap_or_default();
                if word == "uploads-offline" {
                    self.select(Some("line-1".into()), output);
                    self.scene_offline();
                    return true;
                }
                self.notice = Some(("Saved line-1".into(), false));
                if word == "destinations-remove" {
                    self.removing = Some("archive".into());
                    self.notice = None;
                }
            }
            "folder-editor" => self.open_editor(Editor::new_folder(
                "archive".into(),
                "/Volumes/Archive/Capturefab".into(),
            )),
            "bucket-editor" | "bucket-editor-advanced" => {
                let mut editor = Editor::new_bucket();
                editor.bucket = "line-1-captures".into();
                editor.name = safe_name(&editor.bucket);
                editor.access_key_id = "AKIAIOSFODNN7EXAMPLE".into();
                editor.stored_secret = true;
                editor.message = Some(("Connected to line-1-captures".into(), false));
                editor.advanced = word == "bucket-editor-advanced";
                self.open_editor(editor);
            }
            "testing" => {
                if let Some(editor) = &mut self.editor {
                    let (sender, receiver) = channel();
                    // Never answered: the check runs for the whole scene.
                    std::mem::forget(sender);
                    editor.message = None;
                    editor.testing = Some(receiver);
                }
            }
            _ => return false,
        }
        self.manager_open = true;
        true
    }
}

impl Picker {
    /// Hold an offline backlog for line-1 that has grown for a minute.
    fn scene_offline(&mut self) {
        let next = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            + 180;
        let error = "connect to line-1-captures.ACCOUNT_ID.r2.cloudflarestorage.com: network is unreachable";
        self.uploads = Some(Arc::new(serde_json::json!({
            "pending": 1234, "uploading": 0, "failed": 0, "waiting_bytes": 2_104_000_000u64,
            "uploaded": 8812, "failures": 0, "last_error": error, "uploader_running": true,
            "destinations": {"line-1": {
                "pending": 1234, "uploading": 0, "failed": 0, "waiting_bytes": 2_104_000_000u64,
                "next_attempt": next, "last_error": error,
            }},
        })));
        self.watch
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pinned = true;
        let now = Instant::now();
        for second in (0..=60).rev().step_by(2) {
            let at = now.checked_sub(Duration::from_secs(second)).unwrap_or(now);
            let bytes = 2_104_000_000.0 - second as f64 * 1_700_000.0;
            self.backlog.offer(at, bytes, 0);
        }
    }
}

/// Width of the editor's label column.
const LABEL: f32 = 110.0;

/// Where Save keeps a typed secret key; Test never changes a saved key.
const KEYCHAIN_NOTE: &str = if cfg!(target_os = "macos") {
    "Save keeps this key in your macOS keychain. Test uses it for this check only."
} else {
    "Save keeps this key in the OS credential store. Test uses it for this check only."
};

fn label<'a>(value: &'a str, p: &'static Palette) -> Element<'a, Message> {
    text(value).size(style::SMALL).color(p.secondary).into()
}

/// A label in the column beside "File names" and its folder or pattern.
fn side_label<'a>(value: &'a str, p: &'static Palette) -> Element<'a, Message> {
    container(text(value).size(style::BODY).color(p.secondary))
        .width(SIDE_LABEL)
        .into()
}

/// A row on that label column, its control taking the rest of the width,
/// so other rows of the same form start their controls together.
pub(super) fn side_row<'a, M: 'a>(
    value: &'a str,
    control: impl Into<Element<'a, M>>,
    p: &'static Palette,
) -> Element<'a, M> {
    let label = container(text(value).size(style::BODY).color(p.secondary)).width(SIDE_LABEL);
    row![label, container(control).width(Fill)]
        .spacing(6)
        .align_y(Alignment::Center)
        .into()
}

/// The width of that column: "File names" at body size, with room to spare.
const SIDE_LABEL: f32 = 74.0;

/// A field's label with an action at the end of its line.
fn labeled<'a>(
    value: &'a str,
    action: impl Into<Element<'a, Message>>,
    p: &'static Palette,
) -> Element<'a, Message> {
    row![label(value, p), space::horizontal(), action.into()]
        .align_y(Alignment::Center)
        .into()
}

/// A small link-styled action on a label's line. Its padding is the hover
/// fill's, so the text ends where disclosure summaries end.
fn label_action(value: &str, on: Message) -> Element<'_, Message> {
    button(text(value).size(style::SMALL))
        .padding([1, 6])
        .style(style::link)
        .on_press(on)
        .into()
}

fn caption<'a>(value: &'a str, p: &'static Palette) -> Element<'a, Message> {
    text(value).size(style::CAPTION).color(p.secondary).into()
}

/// Whether `after` is `before` with at most one character typed, deleted
/// or replaced, as one keystroke in a field changes it.
fn single_edit(before: &str, after: &str) -> bool {
    let (before, after): (Vec<char>, Vec<char>) =
        (before.chars().collect(), after.chars().collect());
    let prefix = before
        .iter()
        .zip(&after)
        .take_while(|(a, b)| a == b)
        .count();
    let (before, after) = (&before[prefix..], &after[prefix..]);
    let suffix = before
        .iter()
        .rev()
        .zip(after.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    before.len() - suffix <= 1 && after.len() - suffix <= 1
}

/// What an output template may hold, in full, for tooltips.
const PLACEHOLDERS: &str = "Placeholders: {camera} {date} {time} {datetime} {frame} {seq}";
/// The template field's height (13 px text at 1.3 lines, 3 px padding each
/// side), which the line under "File names" keeps in every mode.
const FIELD_HEIGHT: f32 = 23.0;

/// The output field's text: `value` with a leading home folder written
/// `~`, character for character, so `home_long` gives `value` back and
/// typing never loses a separator.
fn home_text(value: &str) -> String {
    shorten_home(value, home().as_deref())
}

fn shorten_home(value: &str, home: Option<&Path>) -> String {
    let Some(home) = home.and_then(Path::to_str) else {
        return value.into();
    };
    match value.strip_prefix(home) {
        Some("") => "~".into(),
        Some(rest) if rest.starts_with(std::path::MAIN_SEPARATOR) => format!("~{rest}"),
        _ => value.into(),
    }
}

/// `value` with a leading `~` written out as the home folder, undoing
/// `home_short` for what the output field sends.
fn home_long(value: &str) -> String {
    expand_tilde(value, home().as_deref())
}

fn expand_tilde(value: &str, home: Option<&Path>) -> String {
    let Some(home) = home else {
        return value.into();
    };
    match value.strip_prefix('~') {
        Some("") => home.to_string_lossy().into_owned(),
        Some(rest) if rest.starts_with(std::path::MAIN_SEPARATOR) => {
            format!("{}{rest}", home.display())
        }
        _ => value.into(),
    }
}

/// The user's home folder, where it is written `~`.
fn home() -> Option<PathBuf> {
    if cfg!(windows) {
        return None;
    }
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .filter(|home| home.parent().is_some())
}

/// `path` with the home folder shown as `~`.
fn home_short(path: &Path) -> String {
    tilde(path, home().as_deref())
}

fn tilde(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        None => path.display().to_string(),
    }
}

/// What a status line reports.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Working,
    Done,
    Failed,
}

fn history_controls<'a>(
    undo_label: Option<&str>,
    redo_label: Option<&str>,
    _p: &'static Palette,
) -> Element<'a, Message> {
    use super::keys::{Action, Os};
    // Nothing to undo or redo: no disabled stops for Tab to land on.
    if undo_label.is_none() && redo_label.is_none() {
        return space().into();
    }
    let control = |action: Action, label: Option<&str>, on: Message, word: &str| {
        let description = label
            .map(|label| format!("{word} {label}"))
            .unwrap_or_else(|| format!("Nothing to {}", word.to_lowercase()));
        super::tip(
            button(
                text(format!("{word} · {}", action.key_label(Os::CURRENT))).size(style::CAPTION),
            )
            .padding([5, 10])
            .style(style::secondary)
            .on_press_maybe(label.map(|_| on)),
            action.hint(&description, Os::CURRENT),
        )
    };
    row![
        control(Action::Undo, undo_label, Message::Undo, "Undo"),
        control(Action::Redo, redo_label, Message::Redo, "Redo")
    ]
    .spacing(8)
    .into()
}

/// The editor footer's compact Undo and Redo, each enabled while it has a
/// change to replay; their tooltips name it and the shortcut.
fn history_buttons<'a>(
    undo_label: Option<&str>,
    redo_label: Option<&str>,
) -> [Element<'a, Message>; 2] {
    use super::keys::{Action, Os};
    let control = |action: Action, label: Option<&str>, on: Message, word: &'static str| {
        let description = label
            .map(|label| format!("{word} {label}"))
            .unwrap_or_else(|| format!("Nothing to {}", word.to_lowercase()));
        super::tip(
            button(text(word).size(style::BODY))
                .padding([7, 10])
                .style(style::plain)
                .on_press_maybe(label.map(|_| on)),
            action.hint(&description, Os::CURRENT),
        )
    };
    [
        control(Action::Undo, undo_label, Message::Undo, "Undo"),
        control(Action::Redo, redo_label, Message::Redo, "Redo"),
    ]
}

/// An outcome in the toolbar's manner: a glyph, then the text. `spin` steps
/// the spinner while working.
fn status<'a>(
    message: String,
    status: Status,
    spin: usize,
    p: &'static Palette,
) -> Element<'a, Message> {
    status_line(message, status, spin, p, false)
}

fn compact_status<'a>(
    message: String,
    status: Status,
    spin: usize,
    p: &'static Palette,
) -> Element<'a, Message> {
    status_line(message, status, spin, p, true)
}

fn status_line<'a>(
    message: String,
    status: Status,
    spin: usize,
    p: &'static Palette,
    compact: bool,
) -> Element<'a, Message> {
    let ink = if status == Status::Failed {
        Level::Error.color(p)
    } else {
        p.secondary
    };
    let glyph = match status {
        Status::Working => icon::spinner(13.0, p.secondary, spin),
        Status::Done => Level::Done.mark(13.0, p),
        Status::Failed => Level::Error.mark(13.0, p),
    };
    let line: Element<'a, Message> = if compact {
        one_line(message, style::SMALL, style::SANS, ink)
            .width(Fill)
            .into()
    } else {
        text(message)
            .size(style::SMALL)
            .color(ink)
            .width(Fill)
            .into()
    };
    row![
        // On the first line, should the text wrap.
        container(glyph)
            .height(Length::Fixed(style::line_height(style::SMALL)))
            .align_y(Alignment::Center),
        line,
    ]
    .spacing(6)
    .width(Fill)
    .into()
}

/// One way to start a destination, in the welcome screen's card style.
fn choice<'a>(
    glyph: Icon,
    title: &'a str,
    detail: &'a str,
    on: Message,
    p: &'static Palette,
) -> Element<'a, Message> {
    button(
        row![
            icon(glyph, 20.0, p.accent),
            column![
                text(title).size(style::BODY).font(style::MEDIUM),
                text(detail).size(style::CAPTION).color(p.secondary),
            ]
            .spacing(2)
            .width(Fill),
            icon(Icon::ChevronRight, 13.0, p.tertiary),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .width(Fill)
    .padding([10, 14])
    .style(style::card)
    .on_press(on)
    .into()
}

/// Where FTP and SFTP servers are added, under the manager's add actions.
fn server_note<'a>(p: &'static Palette) -> Element<'a, Message> {
    text(SERVER_NOTE)
        .size(style::CAPTION)
        .color(p.secondary)
        .into()
}

/// The note under the add actions, naming the commands to run, in view
/// rather than in a tooltip.
const SERVER_NOTE: &str = "Add FTP and SFTP servers from the command line with capturefab destination add-ftp or add-sftp.";
/// What "S3 bucket" covers, for the add action once the list has entries.
const S3_SERVICES: &str =
    "Amazon S3, Cloudflare R2, Backblaze B2, MinIO and other S3-compatible services";

/// A saved destination's line under its name in the manager, in plain
/// words; `Destination::describe` stays in its tooltip.
fn row_summary(destination: &Destination) -> String {
    match &destination.target {
        Target::S3(bucket) => format!(
            "{} · {}/{}{}",
            PROVIDERS[provider_of(&bucket.endpoint)].0,
            bucket.bucket,
            bucket.prefix,
            if bucket.keep_local {
                " · keeps local copies"
            } else {
                ""
            }
        ),
        Target::Folder { path } => home_short(path),
        Target::Ftp(_) | Target::Sftp(_) => destination.describe(),
    }
}

/// Says that the credential store holds the access key's secret.
fn stored_badge<'a>(p: &'static Palette) -> Element<'a, Message> {
    super::tip(
        container(
            row![
                icon(Icon::Check, 11.0, p.ink(p.live)),
                text(if cfg!(target_os = "macos") {
                    "In keychain"
                } else {
                    "Stored"
                })
                .size(style::CAPTION),
            ]
            .spacing(4)
            .align_y(Alignment::Center),
        )
        .padding([2, 8])
        .style(style::pill(p.live)),
        "A secret is stored for this access key ID. Leave the field empty to keep it.",
    )
}

/// The Advanced group's values, shown while it is closed.
fn advanced_summary(editor: &Editor) -> String {
    let mut summary = match editor.prefix.as_str() {
        "" => "No folder".to_string(),
        prefix => format!("Folder {prefix}"),
    };
    if editor.path_style {
        summary.push_str(" · path-style");
    }
    match editor.source {
        Source::Keychain => {}
        Source::Profile => summary.push_str(&format!(" · AWS profile {}", editor.profile.trim())),
        Source::Environment => summary.push_str(" · environment credentials"),
    }
    if editor.region_advanced() && editor.region != PROVIDERS[editor.provider].2 {
        summary.push_str(&format!(" · region {}", editor.region.trim()));
    }
    summary
}

fn folder_encoding_error() -> &'static str {
    "A folder destination needs a UTF-8 path; choose a folder without invalid text bytes."
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn choosing_a_destination_keeps_template_folders_below_the_capture_root() {
        let mut picker = Picker::new(false, false);
        let mut output = super::super::prefs::default_capture_output();
        picker.select(Some("archive".into()), &mut output);
        assert_eq!(
            Path::new(&output),
            Path::new("{date}").join("{camera}-{datetime}-{seq}.png")
        );
        let mut output = std::env::temp_dir()
            .join("elsewhere")
            .join("shot.png")
            .to_string_lossy()
            .into_owned();
        picker.select(Some("archive".into()), &mut output);
        assert_eq!(output, "shot.png");
    }

    #[test]
    fn draft_undo_groups_typing_and_restores_fields_changed_by_presets() {
        let mut picker = Picker::new(false, false);
        let mut output = "capture.png".to_string();
        picker.update(Message::AddBucket, &mut output);
        picker.update(Message::Endpoint("https://example.com".into()), &mut output);
        picker.update(Message::Endpoint(String::new()), &mut output);
        assert!(
            !picker.can_undo(),
            "typing back to the starting value has no effect to undo"
        );
        picker.update(Message::Bucket("cam".into()), &mut output);
        picker.update(Message::Bucket("camera".into()), &mut output);
        assert_eq!(picker.editor.as_ref().unwrap().history.undo.len(), 1);
        assert_eq!(picker.undo_label(), Some("Bucket name"));
        assert!(picker.undo());
        let editor = picker.editor.as_ref().unwrap();
        assert!(
            editor.bucket.is_empty() && editor.name.is_empty(),
            "automatic name changes restore with the bucket"
        );
        assert!(picker.redo());
        assert_eq!(picker.editor.as_ref().unwrap().name, "camera");
        picker.update(Message::Provider(Provider(1)), &mut output);
        assert!(picker.editor.as_ref().unwrap().path_style);
        assert!(picker.undo());
        let editor = picker.editor.as_ref().unwrap();
        assert_eq!(
            (
                editor.provider,
                editor.region.as_str(),
                editor.endpoint.as_str(),
                editor.path_style
            ),
            (0, "us-east-1", "", false)
        );
        assert!(picker.redo());
        assert_eq!(picker.editor.as_ref().unwrap().region, "auto");
        assert_eq!(output, "capture.png");
    }

    #[test]
    fn draft_history_branches_and_keeps_secret_values_private() {
        let mut picker = Picker::new(false, false);
        let mut output = String::new();
        picker.update(Message::AddBucket, &mut output);
        picker.update(Message::Secret("private-first".into()), &mut output);
        picker.update(Message::Source(Source::Environment), &mut output);
        assert!(picker.undo());
        assert_eq!(picker.editor.as_ref().unwrap().source, Source::Keychain);
        assert!(picker.undo());
        assert!(picker.editor.as_ref().unwrap().secret.is_empty());
        assert!(picker.redo());
        assert_eq!(picker.editor.as_ref().unwrap().secret, "private-first");
        picker.update(Message::Secret("private-second".into()), &mut output);
        assert!(!picker.can_redo(), "editing branches the history");
        picker.editor.as_mut().unwrap().secret_stored();
        assert!(!picker.can_undo(), "storing clears sensitive draft history");
        let before_selection = picker.selection(&output);
        picker.record_change(
            None,
            Some(bucket(|_| {}).destination().unwrap()),
            Some(SecretChange {
                access_key_id: "AKIA".into(),
                before: Some("private-first".into()),
                after: "private-second".into(),
            }),
            before_selection,
            &output,
            Instant::now(),
        );
        let text = format!("{:?}", picker.take_changes());
        assert!(!text.contains("private-first") && !text.contains("private-second"));
        picker.update(Message::Cancel, &mut output);
        assert!(!picker.can_undo() && !picker.can_redo());
    }

    #[test]
    fn pending_save_blocks_edits_and_only_success_creates_global_history() {
        let mut picker = Picker::new(false, false);
        let initial = std::env::temp_dir()
            .join("capture.png")
            .to_string_lossy()
            .into_owned();
        let mut output = initial.clone();
        let mut editor = bucket(|e| e.source = Source::Environment);
        let (sender, receiver) = channel();
        editor.saving = Some((editor.destination().unwrap(), receiver));
        picker.editor = Some(editor);
        picker.saving_context = Some(SaveContext {
            before: None,
            selection: picker.selection(&output),
            created_at: Instant::now(),
        });
        picker.manager_open = true;
        picker.update(Message::Name("different".into()), &mut output);
        picker.update(Message::CloseManager, &mut output);
        assert!(picker.manager_open && picker.mutation_busy());
        assert_eq!(picker.editor.as_ref().unwrap().name, "line-1");
        picker.tick(&mut output);
        assert!(picker.take_changes().is_empty());
        sender.send(Ok(None)).unwrap();
        picker.tick(&mut output);
        let changes = picker.take_changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].label(), "Add destination line-1");
        assert_eq!(changes[0].before_selection.output, initial);
        assert_eq!(changes[0].after_selection.output, "capture.png");
        assert!(!picker.mutation_busy());
        assert!(picker.take_changes().is_empty(), "delivered once");
    }

    #[test]
    fn pending_save_disables_header_actions_and_excludes_them_from_tab_traversal() {
        use crate::gui::accessibility::Control;
        use iced::advanced::{
            Layout, layout,
            renderer::Headless,
            widget::{Id, Operation, Tree, operation::Focusable},
        };
        use iced::{Rectangle, Size};
        use std::any::Any;

        #[derive(Default)]
        struct Header {
            controls: BTreeMap<String, (Id, bool, Option<Message>)>,
            tab_stops: Vec<Id>,
        }
        impl Operation for Header {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn Any) {
                if let Some(control) = state.downcast_ref::<Control<Message>>()
                    && let Some(name) = control.node.label()
                    && (name == "Back" || name.starts_with("Close "))
                {
                    self.controls.insert(
                        name.into(),
                        (
                            control.focus_id.clone(),
                            control.node.is_disabled(),
                            control.activate.clone(),
                        ),
                    );
                }
            }
            fn focusable(&mut self, id: Option<&Id>, _: Rectangle, _: &mut dyn Focusable) {
                if let Some(id) = id {
                    self.tab_stops.push(id.clone());
                }
            }
        }

        // Use the app's actual sheet, including the tooltip decorators and
        // pinned header, with the software renderer rather than a GPU/window.
        let renderer = iced::futures::executor::block_on(iced::Renderer::new(
            iced::Font::default(),
            iced::Pixels(16.0),
            Some("tiny-skia"),
        ))
        .expect("the compiled software renderer is available");
        let mut picker = Picker::new(false, false);
        picker.manager_open = true;
        picker.editor = Some(bucket(|editor| editor.source = Source::Environment));
        let mut tree = Tree::empty();
        let mut inspect = |picker: &Picker| {
            let mut view = picker.manager(Palette::of(false), 640.0, 0, Instant::now(), None, None);
            tree.diff(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(560.0, 640.0)),
            );
            let mut header = Header::default();
            view.as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut header);
            header
        };
        let before = inspect(&picker);
        assert_eq!(before.controls.len(), 2);
        let (_sender, receiver) = channel();
        let editor = picker.editor.as_mut().unwrap();
        editor.saving = Some((editor.destination().unwrap(), receiver));
        let pending = inspect(&picker);
        for (name, (id, disabled, activate)) in &pending.controls {
            assert_eq!(
                *id, before.controls[name].0,
                "keep the header's tree stable"
            );
            assert!(*disabled, "{name} announces its temporary unavailability");
            assert!(activate.is_none(), "{name} offers no ignored native action");
            assert!(
                !pending.tab_stops.contains(id),
                "{name} is not a disabled Tab stop"
            );
        }
        picker.editor.as_mut().unwrap().saving = None;
        let after = inspect(&picker);
        for (name, (id, disabled, activate)) in &after.controls {
            assert_eq!(*id, before.controls[name].0);
            assert!(!disabled);
            assert!(after.tab_stops.contains(id));
            assert!(matches!(
                activate,
                Some(Message::Cancel | Message::CloseManager)
            ));
        }
    }

    #[test]
    fn failed_replay_retains_selection_and_success_restores_the_exact_output() {
        let mut picker = Picker::new(false, false);
        let mut output = "current.png".to_string();
        let desired = Selection {
            selected: None,
            output: "/tmp/previous.png".into(),
        };
        let (sender, receiver) = channel();
        picker.replay = Some(Replay {
            task: receiver,
            selection: desired.clone(),
        });
        assert!(picker.mutation_busy());
        assert!(picker.take_replay_result(&mut output).is_none());
        picker.update(Message::Output("newer.png".into()), &mut output);
        assert_eq!(
            output, "newer.png",
            "new local typing stays available for the app's deferred history"
        );
        sender
            .send(Err(anyhow::anyhow!("credential changed")))
            .unwrap();
        assert!(picker.take_replay_result(&mut output).unwrap().is_err());
        assert_eq!(output, "newer.png");
        assert!(!picker.mutation_busy());
        let (sender, receiver) = channel();
        picker.replay = Some(Replay {
            task: receiver,
            selection: desired,
        });
        sender.send(Ok(())).unwrap();
        assert!(picker.take_replay_result(&mut output).unwrap().is_ok());
        assert_eq!(output, "/tmp/previous.png");
        assert!(
            picker.take_changes().is_empty(),
            "replay creates no new edits"
        );
    }

    #[test]
    fn asynchronous_destination_restore_reapplies_newer_output_through_the_global_timeline() {
        assert_newer_output_survives_destination_replay(false);
    }

    #[test]
    fn destination_replay_ready_before_input_preserves_the_current_output_gesture() {
        assert_newer_output_survives_destination_replay(true);
    }

    fn assert_newer_output_survives_destination_replay(ready_before_input: bool) {
        use super::super::{Message as AppMessage, Workbench};
        let mut app = Workbench::new(
            crate::session::SessionHandle::new(),
            "destination-history-test".into(),
            true,
            None,
        );
        // Use the worker acknowledgment boundary instead of accessing the
        // user's OS credential store or destination settings in this test.
        app.capture_to = Picker::new(false, false);
        app.output = "saved.png".into();
        let before = Selection {
            selected: None,
            output: "/tmp/before.png".into(),
        };
        app.capture_to.record_change(
            None,
            Some(bucket(|_| {}).destination().unwrap()),
            Some(SecretChange {
                access_key_id: "test-key".into(),
                before: None,
                after: "test-secret".into(),
            }),
            before.clone(),
            &app.output,
            Instant::now(),
        );
        app.poll_destination_history();
        let (sender, receiver) = channel();
        app.capture_to.replay = Some(Replay {
            task: receiver,
            selection: before,
        });
        app.history_replay = Some(true);
        if ready_before_input {
            sender.send(Ok(())).unwrap();
        }
        let _ = app.update(AppMessage::CapturePicker(Message::Output(
            "newer.png".into(),
        )));
        assert_eq!(app.output, "newer.png");
        if !ready_before_input {
            sender.send(Ok(())).unwrap();
        }
        app.poll_destination_history();
        assert!(app.history_replay.is_none());
        assert_eq!(
            app.output, "newer.png",
            "the completed destination restore cannot erase newer typing"
        );
        assert_eq!(app.history_label(true), Some("Edit capture output"));
        assert!(
            app.history_label(false).is_none(),
            "new typing branches the redo timeline"
        );
        let _ = app.update(AppMessage::Undo);
        assert_eq!(
            app.output,
            if ready_before_input {
                "/tmp/before.png"
            } else {
                "saved.png"
            },
            "undo restores the value shown when the newer edit began"
        );
        let _ = app.update(AppMessage::Redo);
        assert_eq!(app.output, "newer.png");
    }

    #[test]
    fn a_superseded_secret_store_never_lands_over_a_newer_one() {
        let queue =
            |id: &str, secret: &str| SecretWrite::queue(Some((id.into(), secret.into()))).unwrap();
        let older = queue("AKIA", "old");
        let newer = queue("AKIA", "new");
        let other = queue("AKIB", "b");
        let written = Mutex::new(BTreeMap::new());
        let stored = std::cell::RefCell::new(Vec::new());
        let store = |id: &str, secret: &str| {
            stored.borrow_mut().push(format!("{id}={secret}"));
            Ok(())
        };
        // The newer store's thread runs first; the older one then finds it
        // superseded, while an unrelated key still stores.
        newer.store_in_order(&written, store).unwrap();
        assert!(older.store_in_order(&written, store).is_err());
        other.store_in_order(&written, store).unwrap();
        assert_eq!(*stored.borrow(), ["AKIA=new", "AKIB=b"]);
        // In order, both store and the newer one is last.
        let (first, second) = (queue("AKIC", "1"), queue("AKIC", "2"));
        first.store_in_order(&written, store).unwrap();
        second.store_in_order(&written, store).unwrap();
        assert_eq!(stored.borrow()[2..], ["AKIC=1", "AKIC=2"]);
    }

    #[test]
    fn credential_undo_redo_restore_old_values_without_overwriting_external_changes() {
        let change = SecretChange {
            access_key_id: "AKIA".into(),
            before: Some("old".into()),
            after: "new".into(),
        };
        let current = std::cell::RefCell::new(Some("new".to_string()));
        let read = |_: &str| Ok(current.borrow().clone());
        let write = |_: &str, value: Option<&str>| {
            *current.borrow_mut() = value.map(str::to_owned);
            Ok(())
        };
        restore_secret_using(&change, true, read, write).unwrap();
        assert_eq!(current.borrow().as_deref(), Some("old"));
        restore_secret_using(&change, false, read, write).unwrap();
        assert_eq!(current.borrow().as_deref(), Some("new"));
        *current.borrow_mut() = Some("external".into());
        assert!(restore_secret_using(&change, true, read, write).is_err());
        assert_eq!(current.borrow().as_deref(), Some("external"));
        assert!(
            restore_secret_using(&change, true, |_| Err(anyhow::anyhow!("locked")), write).is_err()
        );
        assert_eq!(current.borrow().as_deref(), Some("external"));
        let new_key = SecretChange {
            before: None,
            ..change
        };
        *current.borrow_mut() = Some("new".into());
        restore_secret_using(&new_key, true, read, write).unwrap();
        assert!(
            current.borrow().is_none(),
            "undoing a new key removes only that key"
        );
        restore_secret_using(&new_key, false, read, write).unwrap();
        assert_eq!(current.borrow().as_deref(), Some("new"));
    }

    /// A bucket that validates, changed by `change`.
    fn bucket(change: impl FnOnce(&mut Editor)) -> Editor {
        let mut editor = Editor::new_bucket();
        editor.bucket = "line-1".into();
        editor.access_key_id = "AKIA".into();
        editor.name = "line-1".into();
        editor.name_touched = true;
        change(&mut editor);
        editor
    }

    fn fields(editor: &Editor) -> Vec<Field> {
        editor
            .problems()
            .into_iter()
            .map(|(field, _)| field)
            .collect()
    }

    /// A change to a valid editor.
    type Change = fn(&mut Editor);

    #[test]
    fn every_refusal_lands_on_its_field() {
        assert!(bucket(|_| {}).destination().is_ok());
        let cases: [(Change, Field); 8] = [
            (|e| e.name = String::new(), Field::Name),
            (
                |e| {
                    e.provider = 4;
                    e.endpoint = "ftp://nas".into();
                },
                Field::Endpoint,
            ),
            (|e| e.endpoint = "https://user@nas".into(), Field::Endpoint),
            (|e| e.region = String::new(), Field::Region),
            (|e| e.bucket = "Line 1".into(), Field::Bucket),
            (|e| e.prefix = "a/../b".into(), Field::Prefix),
            (|e| e.access_key_id = String::new(), Field::AccessKey),
            (
                |e| {
                    e.source = Source::Profile;
                    e.profile = String::new();
                },
                Field::Profile,
            ),
        ];
        for (change, field) in cases {
            assert_eq!(fields(&bucket(change)), [field]);
        }
        let folder = Editor::new_folder("archive".into(), "relative".into());
        assert_eq!(fields(&folder), [Field::Folder]);
    }

    #[cfg(feature = "s3")]
    #[test]
    fn invalid_typed_secrets_fail_before_save_and_mark_the_secret_field() {
        let mut editor = bucket(|e| e.secret = "x".repeat(4097));
        editor.refuse(editor.destination().unwrap_err());
        assert!(editor.refusal(Field::Secret).is_some());
        editor.secret = " Unicode secret é \t ".into();
        editor.edited(Some(Field::Secret));
        assert!(editor.destination().is_ok());
        assert_eq!(editor.secret_to_store().unwrap().1, " Unicode secret é \t ");
    }

    #[test]
    fn destination_paths_preserve_meaningful_whitespace() {
        let folder = std::env::temp_dir().join("archive ");
        let editor = Editor::new_folder("archive".into(), folder.clone());
        let destination = editor.destination().unwrap();
        let Target::Folder { path } = destination.target else {
            panic!("expected folder destination");
        };
        assert_eq!(path, folder);

        let editor = bucket(|e| e.prefix = " images ".into());
        let destination = editor.destination().unwrap();
        let Target::S3(bucket) = destination.target else {
            panic!("expected bucket destination");
        };
        assert_eq!(bucket.prefix, " images ");
    }

    #[cfg(unix)]
    #[test]
    fn folder_editor_refuses_paths_that_cannot_be_preserved() {
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/archive-\xff".to_vec()));
        let editor = Editor::new_folder("archive".into(), path);
        assert!(editor.destination().is_err());
        assert!(editor.refusal(Field::Folder).unwrap().contains("UTF-8"));
        assert!(!editor.folder.contains('\u{fffd}'));
    }

    #[test]
    fn a_refused_save_marks_every_field_at_fault() {
        let mut editor = bucket(|e| {
            e.name = String::new();
            e.region = String::new();
            e.access_key_id = String::new();
        });
        editor.refuse(editor.destination().unwrap_err());
        assert_eq!(
            editor.invalid.iter().map(|(f, _)| *f).collect::<Vec<_>>(),
            [Field::Name, Field::Region, Field::AccessKey]
        );
        assert!(editor.message.is_none(), "nothing left for the footer");
        assert_eq!(
            editor.refusal(Field::Region),
            Some("S3 region must be a name such as us-east-1 or auto")
        );
        editor.edited(Some(Field::Region));
        assert!(editor.refusal(Field::Region).is_none());
        assert!(editor.refusal(Field::Name).is_some(), "only the edited one");
        // Refusals no field explains go under the form, as a sentence.
        editor.refuse(anyhow::anyhow!("cannot create /x: denied"));
        assert_eq!(
            editor.message,
            Some(("Cannot create /x: denied".into(), true))
        );
        // A refused prefix opens the group it hides in.
        let mut editor = bucket(|e| e.prefix = "a/../b".into());
        editor.refuse(editor.destination().unwrap_err());
        assert!(editor.advanced);
    }

    #[test]
    fn a_new_bucket_is_named_after_it_until_a_name_is_typed() {
        let mut picker = Picker::new(false, false);
        let mut output = String::new();
        picker.update(Message::AddBucket, &mut output);
        assert_eq!(picker.take_focus(), Some(BUCKET_FIELD));
        assert_eq!(picker.take_focus(), None, "once");
        // The editor opens at its top, wherever the list was scrolled.
        assert!(picker.take_to_top());
        assert!(!picker.take_to_top(), "once");
        // Saving with nothing typed blames the bucket, not the name that
        // follows it.
        picker.update(Message::Save, &mut output);
        let editor = picker.editor.as_ref().unwrap();
        assert!(editor.refusal(Field::Bucket).is_some());
        assert!(editor.refusal(Field::Name).is_none());
        picker.update(Message::Bucket("Line 1".into()), &mut output);
        let editor = picker.editor.as_ref().unwrap();
        assert_eq!(editor.name, "Line-1");
        assert!(editor.refusal(Field::Bucket).is_none(), "edited");
        picker.update(Message::Name("archive".into()), &mut output);
        picker.update(Message::Bucket("line-2".into()), &mut output);
        assert_eq!(picker.editor.as_ref().unwrap().name, "archive");
        // Back to the list, at its top, and a folder's editor focuses its
        // name.
        picker.update(Message::Scrolled(true), &mut output);
        picker.update(Message::Cancel, &mut output);
        assert!(picker.editor.is_none());
        assert!(!picker.scrolled && picker.take_to_top());
        picker.scene("folder-editor", &mut output);
        assert_eq!(picker.take_focus(), Some(NAME_FIELD));
        assert!(picker.manager_open);
    }

    #[cfg(feature = "s3")]
    #[test]
    fn the_keychain_is_asked_once_typing_rests() {
        let start = Instant::now();
        let mut editor = bucket(|e| e.access_key_id = "AKIA1".into());
        editor.stored_secret = true;
        editor.key_edited(start);
        assert!(!editor.stored_secret, "unknown while typing");
        let answer = |stored: bool| {
            let (sender, receiver) = channel();
            let _ = sender.send(stored);
            receiver
        };
        let mut asked = Vec::new();
        editor.poll_key(start + KEY_PAUSE - ms(1), |id| {
            asked.push(id);
            answer(true)
        });
        assert!(asked.is_empty(), "still typing");
        editor.poll_key(start + KEY_PAUSE, |id| {
            asked.push(id);
            answer(true)
        });
        assert_eq!(asked, ["AKIA1"]);
        assert!(editor.key_probe.is_some() && editor.key_changed.is_none());
        editor.poll_key(start + KEY_PAUSE + ms(60), |_| unreachable!());
        assert!(editor.stored_secret);
        assert!(editor.key_probe.is_none());
        // An answer about a key ID edited since is dropped.
        let later = start + ms(1000);
        editor.access_key_id = "AKIA2".into();
        editor.key_edited(later);
        editor.poll_key(later + KEY_PAUSE, |_| answer(true));
        editor.access_key_id = "AKIA3".into();
        editor.key_edited(later + KEY_PAUSE);
        editor.poll_key(later + KEY_PAUSE + ms(10), |_| unreachable!());
        assert!(!editor.stored_secret);
        // An empty key ID has no secret and asks nothing.
        editor.access_key_id = " ".into();
        editor.key_edited(later);
        editor.poll_key(later + KEY_PAUSE * 2, |_| unreachable!());
        assert!(editor.key_probe.is_none() && editor.key_changed.is_none());
    }

    #[test]
    fn a_new_destination_never_replaces_a_saved_one() {
        let mut picker = Picker::new(false, false);
        let mut output = String::new();
        picker.saved = vec![
            bucket(|_| {}).destination().unwrap(),
            bucket(|e| e.name = "archive".into()).destination().unwrap(),
        ];
        picker.update(Message::AddBucket, &mut output);
        picker.update(Message::Bucket("line-1".into()), &mut output);
        picker.update(Message::AccessKey("AKIA".into()), &mut output);
        picker.update(Message::Save, &mut output);
        let editor = picker.editor.as_ref().unwrap();
        assert_eq!(
            editor.refusal(Field::Name),
            Some("a destination named line-1 already exists; choose another name")
        );
        assert!(editor.saving.is_none(), "nothing saved");
        // The name stops following the bucket, so it can be fixed.
        picker.update(Message::Bucket("line-2".into()), &mut output);
        assert_eq!(picker.editor.as_ref().unwrap().name, "line-1");
        // An edited destination keeps its own name, but can't take another's.
        let mut editor = Editor::edit(&picker.saved[0]);
        assert_eq!(editor.name_taken(&picker.saved), None);
        editor.name = "archive".into();
        assert!(editor.name_taken(&picker.saved).is_some());
    }

    #[test]
    fn editing_drops_a_stale_check_but_keeps_the_save_acknowledgment() {
        let pending = || {
            let (sender, receiver) = channel::<Result<String, String>>();
            std::mem::forget(sender);
            receiver
        };
        let mut editor = bucket(|_| {});
        editor.testing = Some(pending());
        editor.edited(Some(Field::Bucket));
        assert!(editor.testing.is_none());
        let (_sender, receiver) = channel();
        editor.saving = Some((editor.destination().unwrap(), receiver));
        editor.edited(None);
        assert!(
            editor.saving.is_some(),
            "a committing worker must retain its acknowledgment receiver"
        );
    }

    #[test]
    fn saving_runs_off_the_ui_thread_and_keeps_a_refused_secret() {
        let mut picker = Picker::new(false, false);
        let mut output = String::new();
        let mut editor = bucket(|e| e.secret = "secret".into());
        let (sender, receiver) = channel();
        editor.saving = Some((editor.destination().unwrap(), receiver));
        picker.editor = Some(editor);
        assert!(picker.busy(), "polls for the answer");
        // Neither saves nor checks twice while it runs.
        picker.update(Message::Save, &mut output);
        picker.update(Message::Test, &mut output);
        let editor = picker.editor.as_mut().unwrap();
        assert!(editor.saving.is_some() && editor.testing.is_none());
        assert_eq!(editor.poll_save(), None, "no answer yet");
        sender
            .send(Err(anyhow::anyhow!("the keychain is locked")))
            .unwrap();
        assert_eq!(editor.poll_save(), None);
        assert!(editor.saving.is_none());
        assert_eq!(
            editor.message,
            Some(("The keychain is locked".into(), true))
        );
        assert_eq!(editor.secret, "secret", "typed once");
    }

    #[test]
    fn a_save_waits_for_the_check_using_its_typed_credentials() {
        let mut picker = Picker::new(false, false);
        let mut output = String::new();
        let mut editor = bucket(|e| e.secret = "secret".into());
        let (sender, receiver) = channel();
        editor.testing = Some(receiver);
        picker.editor = Some(editor);
        picker.update(Message::Save, &mut output);
        let editor = picker.editor.as_mut().unwrap();
        assert!(
            editor.saving.is_none(),
            "finish the check before closing its form"
        );
        sender.send(Ok("Connected".into())).unwrap();
        editor.poll_test();
        assert!(!editor.checking_secret() && editor.secret_to_store().is_some());
        assert_eq!(
            editor.secret, "secret",
            "checking never stores or clears the key"
        );
        // Without a typed secret, a check holds nothing up.
        editor.secret.clear();
        let (_sender, receiver) = channel();
        editor.testing = Some(receiver);
        assert!(!editor.checking_secret());
    }

    #[test]
    fn a_check_keeps_the_typed_secret_without_claiming_it_is_stored() {
        let mut editor = bucket(|e| e.secret = "secret".into());
        let (sender, receiver) = channel();
        editor.testing = Some(receiver);
        editor.poll_test();
        assert!(editor.testing.is_some(), "still checking");
        sender.send(Err("access denied".into())).unwrap();
        editor.poll_test();
        assert!(editor.testing.is_none());
        assert!(!editor.stored_secret && editor.secret == "secret");
        assert_eq!(editor.message, Some(("Access denied".into(), true)));
    }

    #[cfg(feature = "s3")]
    #[test]
    fn a_stored_secret_outlasts_an_older_lookup() {
        let start = Instant::now();
        let mut editor = bucket(|e| e.secret = "secret".into());
        editor.key_edited(start);
        editor.poll_key(start + KEY_PAUSE, |_| {
            let (sender, receiver) = channel();
            let _ = sender.send(false);
            receiver
        });
        editor.secret_stored();
        editor.poll_key(start + KEY_PAUSE + ms(60), |_| unreachable!());
        assert!(editor.stored_secret);
        assert!(editor.key_probe.is_none() && editor.key_changed.is_none());
    }

    #[test]
    fn the_advanced_chevron_turns_and_settles() {
        let _motion = motion::test_reduce_motion(false);
        let mut picker = Picker::new(false, false);
        let mut output = String::new();
        let start = Instant::now();
        picker.update(Message::AddBucket, &mut output);
        picker.sync(start);
        assert_eq!(picker.advanced_turn.get(start), 0.0);
        picker.update(Message::Advanced(true), &mut output);
        picker.sync(start);
        let mid = picker.advanced_turn.get(start + motion::RING / 2);
        assert!(mid > 0.0 && mid < 1.0, "turns: {mid}");
        assert!(!picker.advanced_turn.animating(start + motion::RING));
        // Closing the editor rests it at once; nothing shows it.
        picker.update(Message::Cancel, &mut output);
        picker.sync(start + motion::RING);
        assert!(!picker.advanced_turn.animating(start + motion::RING));
        // A bucket with custom settings opens with the group open, unturned.
        let custom = bucket(|e| e.prefix = "line-1/".into())
            .destination()
            .unwrap();
        picker.saved = vec![custom.clone()];
        picker.update(Message::Edit(custom.name), &mut output);
        picker.sync(start);
        assert_eq!(picker.advanced_turn.get(start), 1.0);
        assert!(!picker.advanced_turn.animating(start));
    }

    #[test]
    fn the_picker_polls_while_a_lookup_is_due() {
        let mut picker = Picker::new(false, false);
        assert!(!picker.busy());
        let mut editor = bucket(|_| {});
        editor.key_changed = Some(Instant::now());
        picker.editor = Some(editor);
        assert!(picker.busy());
    }

    #[test]
    fn a_saved_row_glows_once_and_settles() {
        let mut picker = Picker::new(false, false);
        let start = Instant::now();
        picker.saved_glow.hit("archive".into(), start);
        let animating = |picker: &Picker, at| picker.flashes().any(|f| f.animating(at));
        assert!(animating(&picker, start + ms(1)));
        assert!(picker.saved_glow.level("archive", start + SAVED_GLOW / 2) > 0.0);
        assert_eq!(picker.saved_glow.level("line-1", start), 0.0);
        assert!(!animating(&picker, start + SAVED_GLOW));
    }

    #[test]
    fn saved_buckets_find_their_service() {
        assert_eq!(provider_of(""), 0);
        assert_eq!(provider_of("https://abc.r2.cloudflarestorage.com/"), 1);
        assert_eq!(provider_of("https://s3.eu-central-003.backblazeb2.com"), 2);
        assert_eq!(provider_of("http://nas.local:9000"), 3);
        assert_eq!(provider_of("https://minio.example.com"), 4);
        let editor = bucket(|e| e.prefix = "line-1/".into());
        assert!(editor.custom_advanced());
        assert!(!bucket(|_| {}).custom_advanced());
        assert_eq!(
            advanced_summary(&bucket(|e| e.path_style = true)),
            "Folder capturefab/ · path-style"
        );
    }

    #[test]
    fn ftp_destination_offers_no_edit_and_never_saves_as_folder() {
        let server = Destination {
            name: "line1".into(),
            target: Target::Ftp(destination::FtpServer {
                host: "10.0.0.5".into(),
                port: 21,
                user: "cam".into(),
                dir: "incoming".into(),
                tls: destination::FtpTls::Explicit,
                password: destination::SecretSource::Environment,
                keep_local: false,
            }),
        };
        let mut picker = Picker::new(false, false);
        picker.saved = vec![server.clone()];
        let mut output = String::new();
        picker.update(Message::Edit("line1".into()), &mut output);
        assert!(picker.editor.is_none(), "rows of servers cannot be edited");
        // Even if an editor were opened, it refuses to save.
        let editor = Editor::edit(&server);
        assert!(editor.read_only);
        assert!(
            editor
                .destination()
                .unwrap_err()
                .to_string()
                .contains("add-ftp/add-sftp")
        );
        assert_eq!(
            editor.message.as_ref().map(|(text, _)| text.as_str()),
            Some(SERVER_EDIT_HINT)
        );
        assert!(!editor.s3);
    }

    #[test]
    fn upload_lines_count_the_whole_destination_and_say_when_it_retries() {
        let now = 1_000_000;
        let status = |backlog: Value| serde_json::json!({"uploaded": 7, "destinations": {"line-1": backlog}});
        let offline = status(serde_json::json!({
            "pending": 1234, "uploading": 0, "failed": 0, "waiting_bytes": 2_100_000_000u64,
            "next_attempt": now + 180, "last_error": "network is unreachable"
        }));
        let line = UploadLine::of(&offline, "line-1", now);
        assert_eq!(
            line.summary,
            "Offline · 1,234 files (2.1 GB) waiting · retrying in 3 min"
        );
        assert_eq!(
            (line.mood, line.action, line.error),
            (
                Mood::Waiting,
                Some("Retry now"),
                Some("network is unreachable")
            )
        );
        // One under way is uploading, whenever the others are due.
        let due = status(serde_json::json!({
            "pending": 2, "uploading": 1, "failed": 0, "waiting_bytes": 3000,
            "next_attempt": now + 60, "last_error": null
        }));
        let line = UploadLine::of(&due, "line-1", now);
        assert_eq!(line.summary, "Uploading 3 files (3.0 KB)");
        assert_eq!(
            (line.mood, line.action, line.waiting),
            (Mood::Calm, None, 3)
        );
        let failed = status(serde_json::json!({
            "pending": 1, "uploading": 0, "failed": 2, "waiting_bytes": 10,
            "next_attempt": now + 60, "last_error": "credentials refused"
        }));
        let line = UploadLine::of(&failed, "line-1", now);
        assert_eq!(line.summary, "1 waiting · 2 failed");
        assert_eq!(
            (line.mood, line.action, line.error),
            (Mood::Failed, Some("Retry"), Some("credentials refused"))
        );
        let other = UploadLine::of(&offline, "archive", now);
        assert_eq!(other.summary, "All uploaded · 7 sent in total");
        assert_eq!((other.action, other.error), (None, None));
    }

    #[cfg(feature = "uploads")]
    #[test]
    fn upload_status_is_read_less_often_for_a_long_queue_but_stays_fresh() {
        let ms = Duration::from_millis;
        assert_eq!(upload_poll(ms(3)), Duration::from_secs(2));
        assert_eq!(upload_poll(ms(250)), Duration::from_secs(5));
        // Five seconds waiting on another process's fold is not a slow read.
        assert_eq!(upload_poll(ms(5000)), Duration::from_secs(10));
    }

    #[test]
    fn both_pickers_take_one_reading_and_a_scene_holds_its_own() {
        let mut capture = Picker::new(false, false);
        let mut record = Picker::new(false, true).sharing(&capture);
        assert!(Arc::ptr_eq(&capture.watch, &record.watch));
        capture.scene_offline();
        let mut output = String::new();
        for _ in 0..2 {
            capture.tick(&mut output);
            record.tick(&mut output);
        }
        let held = capture.uploads.clone().expect("the scene's status");
        assert_eq!(
            UploadLine::of(&held, "line-1", 0).mood,
            Mood::Waiting,
            "ticks keep the scene's status"
        );
        assert!(capture.backlog.last().is_some());
        let mut watch = capture.watch.lock().unwrap();
        watch.uploads.value = Some((Arc::new(serde_json::json!({"pending": 1})), Duration::ZERO));
        watch.uploads.changes += 1;
        drop(watch);
        capture.tick(&mut output);
        record.tick(&mut output);
        let (Some(a), Some(b)) = (&capture.uploads, &record.uploads) else {
            panic!("both pickers have the reading");
        };
        assert!(Arc::ptr_eq(a, b));
    }

    #[cfg(feature = "uploads")]
    #[test]
    fn upload_status_is_read_only_while_it_can_show_something() {
        let mut watch = Watch::default();
        assert!(watch.uploads_matter(), "until first read");
        let folder = Destination {
            name: "archive".into(),
            target: Target::Folder {
                path: "/Volumes/Archive".into(),
            },
        };
        let empty = serde_json::json!({"pending": 0, "uploading": 0, "failed": 0});
        watch.saved.value = Some((None, vec![folder.clone()]));
        watch.uploads.value = Some((Arc::new(empty), Duration::ZERO));
        assert!(!watch.uploads_matter());
        watch.uploads.value = Some((Arc::new(serde_json::json!({"failed": 1})), Duration::ZERO));
        assert!(watch.uploads_matter(), "failed uploads still show");
        watch.uploads.value = None;
        let bucket = Destination {
            name: "line-1".into(),
            target: Target::S3(Bucket {
                endpoint: "https://storage.example".into(),
                region: "auto".into(),
                bucket: "line-1".into(),
                prefix: String::new(),
                path_style: true,
                credentials: Credentials::Environment,
                keep_local: false,
                no_overwrite: NoOverwrite::IfNoneMatch,
            }),
        };
        watch.saved.value = Some((None, vec![folder, bucket]));
        assert!(watch.uploads_matter());
    }
}
