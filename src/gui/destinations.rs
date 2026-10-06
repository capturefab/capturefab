//! The GUI's "Save to" picker and destination manager: save captures and
//! recordings on this computer, in a folder on an external or network drive,
//! or in an S3-compatible bucket.
//!
//! Disk scans, bucket checks and queue status run on background threads, so a
//! slow network share or service never stalls the interface.
use super::icon::{Icon, icon};
use super::style::{self, Palette};
use crate::destination::{self, Bucket, Credentials, Destination, Target};
use crate::volumes::{self, Volume};
use iced::widget::{
    button, column, container, pick_list, row, scrollable, space, text, text_input,
};
use iced::{Alignment, Element, Fill, Length};
use serde_json::Value;
use std::{
    fmt,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, channel},
    thread,
    time::{Duration, Instant},
};

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
        "MinIO or self-hosted",
        "http://nas.local:9000",
        "us-east-1",
        true,
    ),
    ("Other S3-compatible", "https://", "us-east-1", true),
];

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

/// The add/edit form.
struct Editor {
    /// The name being edited, when changing an existing destination.
    original: Option<String>,
    name: String,
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
    profile: String,
    keep_local: bool,
    message: Option<(String, bool)>,
    testing: Option<Receiver<Result<String, String>>>,
}

impl Editor {
    fn new_folder(name: String, folder: PathBuf) -> Self {
        Self {
            original: None,
            name,
            s3: false,
            folder: folder.to_string_lossy().into_owned(),
            provider: 0,
            endpoint: String::new(),
            region: "us-east-1".into(),
            bucket: String::new(),
            prefix: "capturefab/".into(),
            path_style: false,
            source: Source::Keychain,
            access_key_id: String::new(),
            secret: String::new(),
            stored_secret: false,
            profile: "default".into(),
            keep_local: false,
            message: None,
            testing: None,
        }
    }
    fn new_bucket() -> Self {
        Self {
            s3: true,
            name: "bucket".into(),
            ..Self::new_folder(String::new(), PathBuf::new())
        }
    }
    fn edit(destination: &Destination) -> Self {
        let mut editor = Self::new_folder(destination.name.clone(), PathBuf::new());
        editor.original = Some(destination.name.clone());
        match &destination.target {
            Target::Folder { path } => editor.folder = path.to_string_lossy().into_owned(),
            Target::S3(bucket) => {
                editor.s3 = true;
                editor.endpoint = bucket.endpoint.clone();
                editor.region = bucket.region.clone();
                editor.bucket = bucket.bucket.clone();
                editor.prefix = bucket.prefix.clone();
                editor.path_style = bucket.path_style;
                editor.keep_local = bucket.keep_local;
                editor.provider = PROVIDERS
                    .iter()
                    .position(|(_, endpoint, _, _)| {
                        !endpoint.is_empty()
                            && bucket.endpoint.starts_with(
                                endpoint.trim_end_matches("ACCOUNT_ID.r2.cloudflarestorage.com"),
                            )
                    })
                    .unwrap_or(if bucket.endpoint.is_empty() { 0 } else { 4 });
                match &bucket.credentials {
                    Credentials::Keychain { access_key_id } => {
                        editor.access_key_id = access_key_id.clone();
                        editor.check_stored_secret();
                    }
                    Credentials::Profile { name } => {
                        editor.source = Source::Profile;
                        editor.profile = name.clone();
                    }
                    Credentials::Environment => editor.source = Source::Environment,
                }
            }
        }
        editor
    }
    fn check_stored_secret(&mut self) {
        #[cfg(feature = "s3")]
        {
            self.stored_secret = !self.access_key_id.trim().is_empty()
                && crate::s3::has_secret(self.access_key_id.trim());
        }
    }
    fn destination(&self) -> anyhow::Result<Destination> {
        let target = if self.s3 {
            Target::S3(Bucket {
                endpoint: self.endpoint.trim().into(),
                region: self.region.trim().into(),
                bucket: self.bucket.trim().into(),
                prefix: self.prefix.trim().trim_start_matches('/').into(),
                path_style: self.path_style,
                credentials: match self.source {
                    Source::Keychain => Credentials::Keychain {
                        access_key_id: self.access_key_id.trim().into(),
                    },
                    Source::Profile => Credentials::Profile {
                        name: self.profile.trim().into(),
                    },
                    Source::Environment => Credentials::Environment,
                },
                keep_local: self.keep_local,
            })
        } else {
            Target::Folder {
                path: PathBuf::from(self.folder.trim()),
            }
        };
        let destination = Destination {
            name: self.name.trim().into(),
            target,
        };
        destination.validate()?;
        Ok(destination)
    }
    /// Save the destination (and a typed secret, into the OS credential
    /// store); returns the saved name.
    fn save(&mut self) -> anyhow::Result<String> {
        let destination = self.destination()?;
        if let Target::Folder { path } = &destination.target {
            volumes::ensure_present(path)?;
            std::fs::create_dir_all(path)
                .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", path.display()))?;
        }
        #[cfg(feature = "s3")]
        if self.s3 && self.source == Source::Keychain && !self.secret.is_empty() {
            crate::s3::store_secret(self.access_key_id.trim(), &self.secret)?;
            self.secret.clear();
        }
        if let Some(original) = &self.original
            && original != &destination.name
        {
            destination::remove(original)?;
        }
        destination::save(destination.clone())?;
        self.original = Some(destination.name.clone());
        Ok(destination.name)
    }
}

/// Check a destination: write a probe file to a folder, or reach a bucket.
fn test(destination: Destination) -> Result<String, String> {
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
                crate::s3::Client::new(bucket)
                    .and_then(|client| client.check())
                    .map(|()| format!("Connected to {}", bucket.bucket))
                    .map_err(|e| format!("{e:#}"))
            }
            #[cfg(not(feature = "s3"))]
            {
                let _ = bucket;
                Err("S3 support is not included in this build".into())
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum Message {
    Select(Choice),
    Output(String),
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
    Save,
    Test,
    Cancel,
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
    volumes_task: Option<Receiver<Vec<Volume>>>,
    volumes_at: Option<Instant>,
    folder: Option<FolderStatus>,
    folder_task: Option<Receiver<FolderStatus>>,
    folder_at: Option<Instant>,
    uploads: Option<Value>,
    #[cfg_attr(not(feature = "s3"), allow(dead_code))]
    uploads_task: Option<Receiver<Option<Value>>>,
    #[cfg_attr(not(feature = "s3"), allow(dead_code))]
    uploads_at: Option<Instant>,
    editor: Option<Editor>,
    removing: Option<String>,
    pub manager_open: bool,
    notice: Option<(String, bool)>,
    /// Remember the choice for the next launch (the capture panel does).
    remember: bool,
    /// Files are recordings, not stills; changes a few labels.
    recording: bool,
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
            volumes_task: None,
            volumes_at: None,
            folder: None,
            folder_task: None,
            folder_at: None,
            uploads: None,
            uploads_task: None,
            uploads_at: None,
            editor: None,
            removing: None,
            manager_open: false,
            notice: None,
            remember,
            recording,
        }
    }

    /// Reload the saved list now and then, and collect background results.
    pub fn tick(&mut self) {
        if self
            .loaded
            .is_none_or(|t| t.elapsed() > Duration::from_secs(3))
        {
            self.saved = destination::list().unwrap_or_default();
            self.loaded = Some(Instant::now());
            if self
                .selected
                .as_ref()
                .is_some_and(|name| !self.saved.iter().any(|d| &d.name == name))
            {
                self.selected = None;
            }
        }
        if let Some(task) = &self.volumes_task
            && let Ok(found) = task.try_recv()
        {
            self.volumes = found;
            self.volumes_task = None;
        }
        if self.volumes_task.is_none()
            && self
                .volumes_at
                .is_none_or(|t| t.elapsed() > Duration::from_secs(5))
        {
            self.volumes_at = Some(Instant::now());
            self.volumes_task = Some(background(volumes::list));
        }
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
        if let Some(task) = &self.uploads_task
            && let Ok(status) = task.try_recv()
        {
            self.uploads = status;
            self.uploads_task = None;
        }
        #[cfg(feature = "s3")]
        if self.uploads_task.is_none()
            && self
                .uploads_at
                .is_none_or(|t| t.elapsed() > Duration::from_secs(2))
        {
            self.uploads_at = Some(Instant::now());
            self.uploads_task = Some(background(|| crate::upload::status().ok()));
        }
        if let Some(editor) = &mut self.editor
            && let Some(task) = &editor.testing
            && let Ok(result) = task.try_recv()
        {
            editor.message = Some(match result {
                Ok(text) => (text, false),
                Err(text) => (text, true),
            });
            editor.testing = None;
        }
    }

    pub fn editing(&self) -> bool {
        self.editor.is_some() || self.removing.is_some()
    }

    /// Whether a background check is running, so the app keeps polling.
    pub fn busy(&self) -> bool {
        self.editor.as_ref().is_some_and(|e| e.testing.is_some())
    }

    /// Where a capture with this output goes, for compact displays.
    pub fn label(&self, output: &str) -> String {
        match &self.selected {
            Some(name) => format!("{name}: {output}"),
            None => output.to_string(),
        }
    }

    fn select(&mut self, name: Option<String>, output: &mut String) {
        if self.selected != name {
            if self.remember {
                let _ = destination::set_preferred(name.as_deref());
            }
            self.selected = name;
            self.folder = None;
        }
        // A destination holds relative names; drop a leftover absolute path.
        if self.selected.is_some() && Path::new(output.as_str()).is_absolute() {
            *output = Path::new(output.as_str())
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
        }
    }

    fn selected_destination(&self) -> Option<&Destination> {
        let name = self.selected.as_ref()?;
        self.saved.iter().find(|d| &d.name == name)
    }

    pub fn update(&mut self, message: Message, output: &mut String) {
        match message {
            Message::Select(choice) => self.select(choice.name, output),
            Message::Output(value) => {
                *output = value;
                self.select(self.selected.clone(), output);
            }
            Message::BrowseOutput => {
                if let Some(folder) = rfd::FileDialog::new()
                    .set_title("Save captures in")
                    .pick_folder()
                {
                    let name = Path::new(output.as_str())
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "capture.png".into());
                    *output = folder.join(name).to_string_lossy().into_owned();
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
            }
            Message::CloseManager => {
                self.manager_open = false;
                self.editor = None;
                self.removing = None;
            }
            Message::Retry => {
                #[cfg(feature = "s3")]
                {
                    let _ = crate::upload::retry(None);
                    self.uploads_at = None;
                }
            }
            Message::Edit(name) => {
                self.removing = None;
                if let Some(destination) = self.saved.iter().find(|d| d.name == name) {
                    self.editor = Some(Editor::edit(destination));
                }
            }
            Message::Remove(name) => {
                self.removing = None;
                self.notice = Some(match destination::remove(&name) {
                    Ok(_) => (format!("Removed {name}; its files are untouched"), false),
                    Err(e) => (format!("{e:#}"), true),
                });
                self.loaded = None;
                self.tick();
            }
            Message::ConfirmRemove(name) => self.removing = name,
            Message::AddFolder => {
                self.removing = None;
                if let Some(folder) = rfd::FileDialog::new()
                    .set_title("Save captures in")
                    .pick_folder()
                {
                    let name = folder
                        .file_name()
                        .map(|n| safe_name(&n.to_string_lossy()))
                        .unwrap_or_else(|| "folder".into());
                    self.editor = Some(Editor::new_folder(name, folder));
                }
            }
            Message::AddBucket => {
                self.removing = None;
                self.editor = Some(Editor::new_bucket());
            }
            Message::Cancel => {
                self.editor = None;
                self.removing = None;
            }
            Message::Save => {
                let Some(editor) = &mut self.editor else {
                    return;
                };
                match editor.save() {
                    Ok(name) => {
                        self.notice = Some((format!("Saved {name}"), false));
                        self.editor = None;
                        self.loaded = None;
                        self.tick();
                        self.select(Some(name), output);
                    }
                    Err(error) => editor.message = Some((format!("{error:#}"), true)),
                }
            }
            Message::Test => {
                let Some(editor) = &mut self.editor else {
                    return;
                };
                // Testing a bucket needs the key, so store a typed secret first.
                #[cfg(feature = "s3")]
                if editor.s3 && editor.source == Source::Keychain && !editor.secret.is_empty() {
                    match crate::s3::store_secret(editor.access_key_id.trim(), &editor.secret) {
                        Ok(()) => {
                            editor.secret.clear();
                            editor.stored_secret = true;
                        }
                        Err(e) => editor.message = Some((format!("{e:#}"), true)),
                    }
                }
                match editor.destination() {
                    Ok(destination) => {
                        editor.message = None;
                        editor.testing = Some(background(move || test(destination)));
                    }
                    Err(error) => editor.message = Some((format!("{error:#}"), true)),
                }
            }
            message => {
                let Some(editor) = &mut self.editor else {
                    return;
                };
                match message {
                    Message::Name(value) => editor.name = value,
                    Message::Folder(value) => editor.folder = value,
                    Message::BrowseFolder => {
                        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                            editor.folder = folder.to_string_lossy().into_owned();
                        }
                    }
                    Message::Provider(Provider(index)) => {
                        if index != editor.provider {
                            let (_, endpoint, region, path_style) = PROVIDERS[index];
                            editor.provider = index;
                            editor.endpoint = endpoint.into();
                            editor.region = region.into();
                            editor.path_style = path_style;
                        }
                    }
                    Message::Endpoint(value) => editor.endpoint = value,
                    Message::Region(value) => editor.region = value,
                    Message::Bucket(value) => editor.bucket = value,
                    Message::Prefix(value) => editor.prefix = value,
                    Message::Source(source) => editor.source = source,
                    Message::AccessKey(value) => {
                        editor.access_key_id = value;
                        editor.check_stored_secret();
                    }
                    Message::Secret(value) => editor.secret = value,
                    Message::Profile(value) => editor.profile = value,
                    Message::PathStyle(value) => editor.path_style = value,
                    Message::KeepLocal(value) => editor.keep_local = value,
                    _ => {}
                }
            }
        }
    }

    /// Save to `Capturefab/` on a drive, reusing a destination already there.
    fn use_volume(&mut self, volume: &Volume, output: &mut String) {
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
        match editor.save() {
            Ok(name) => {
                self.loaded = None;
                self.tick();
                self.select(Some(name.clone()), output);
                self.notice = Some((format!("Saving to {name}"), false));
            }
            Err(error) => {
                // Show the form so the name or folder can be adjusted.
                editor.message = Some((format!("{error:#}"), true));
                self.editor = Some(editor);
                self.manager_open = true;
            }
        }
    }

    /// The "Save to" row, the output field and the drive shortcuts. `output`
    /// is the path on this computer, or a name inside the destination.
    pub fn view<'a>(
        &'a self,
        output: &'a str,
        local_label: &'a str,
        dark: bool,
    ) -> Element<'a, Message> {
        let p = Palette::of(dark);
        let choices: Vec<Choice> = std::iter::once(Choice {
            name: None,
            label: "This computer".into(),
        })
        .chain(self.saved.iter().map(|d| Choice {
            name: Some(d.name.clone()),
            label: format!(
                "{} ({})",
                d.name,
                if matches!(d.target, Target::S3(_)) {
                    "S3"
                } else {
                    "folder"
                }
            ),
        }))
        .collect();
        let selected = choices.iter().find(|c| c.name == self.selected).cloned();
        let mut content = column![
            label("Save to", p),
            row![
                pick_list(choices, selected, Message::Select)
                    .width(Fill)
                    .text_size(style::BODY)
                    .padding([6, 10])
                    .style(style::pick)
                    .menu_style(style::menu),
                super::tip(
                    button(text("Manage…").size(style::SMALL))
                        .padding([6, 8])
                        .style(style::plain)
                        .on_press(Message::OpenManager),
                    "Add, edit or test folders, external drives and S3 buckets",
                ),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        ]
        .spacing(6);
        match self.selected_destination() {
            None => {
                content = content
                    .push(space().height(4))
                    .push(label(local_label, p))
                    .push(
                        row![
                            text_input("capture.png", output)
                                .on_input(Message::Output)
                                .size(style::BODY)
                                .padding([6, 10])
                                .style(style::input),
                            super::tip(
                                button(icon(Icon::Folder, 15.0, p.secondary))
                                    .padding(6)
                                    .style(style::plain)
                                    .on_press(Message::BrowseOutput),
                                "Choose a folder",
                            ),
                        ]
                        .spacing(6)
                        .align_y(Alignment::Center),
                    );
            }
            Some(destination) => {
                content = content
                    .push(space().height(4))
                    .push(label(
                        if self.recording {
                            "Recording file name in destination"
                        } else {
                            "Name in destination (file, folder or {frame} template)"
                        },
                        p,
                    ))
                    .push(
                        text_input("capture.png", output)
                            .on_input(Message::Output)
                            .size(style::BODY)
                            .padding([6, 10])
                            .style(style::input),
                    )
                    .push(self.summary(destination, p));
            }
        }
        if !self.volumes.is_empty() {
            let drives = self
                .volumes
                .iter()
                .enumerate()
                .fold(row![], |drives, (index, volume)| {
                    let kind = if volume.kind == "network" {
                        "network"
                    } else {
                        "drive"
                    };
                    drives.push(super::tip(
                        button(
                            text(format!(
                                "{} · {} free · {kind}",
                                volume.name,
                                bytes(volume.free_bytes)
                            ))
                            .size(style::CAPTION),
                        )
                        .padding([4, 8])
                        .style(style::secondary)
                        .on_press(Message::UseVolume(index)),
                        format!(
                            "Save to a Capturefab folder on {} ({} of {})",
                            volume.path.display(),
                            bytes(volume.free_bytes),
                            bytes(volume.total_bytes)
                        ),
                    ))
                });
            content = content
                .push(space().height(4))
                .push(label("External drives", p))
                .push(drives.spacing(6).wrap().vertical_spacing(6));
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
                            .color(p.warn),
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
                            .color(p.warn),
                    );
                }
                lines
                    .push(self.upload_status(Some(&destination.name), p))
                    .into()
            }
        }
    }

    /// Waiting and failed uploads, with a retry button.
    fn upload_status<'a>(
        &'a self,
        destination: Option<&str>,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let Some(status) = &self.uploads else {
            return column![].into();
        };
        let items = status["items"].as_array().cloned().unwrap_or_default();
        let mine = |state: &str| {
            items
                .iter()
                .filter(|i| {
                    i["state"] == state
                        && destination.is_none_or(|d| i["destination"].as_str() == Some(d))
                })
                .count()
        };
        let (waiting, failed) = (mine("pending") + mine("uploading"), mine("failed"));
        let summary = match (waiting, failed) {
            (0, 0) => format!("All uploaded · {} sent in total", status["uploaded"]),
            (w, 0) => format!("Uploading {w} file{}", if w == 1 { "" } else { "s" }),
            (w, f) => format!("{w} waiting · {f} failed"),
        };
        let mut head = row![text(summary).size(style::CAPTION).color(if failed > 0 {
            p.danger
        } else {
            p.secondary
        })]
        .spacing(8)
        .align_y(Alignment::Center);
        if cfg!(feature = "s3") && failed > 0 {
            head = head.push(
                button(text("Retry").size(style::CAPTION))
                    .padding([2, 6])
                    .style(style::link)
                    .on_press(Message::Retry),
            );
        }
        let mut lines = column![head].spacing(4);
        if failed > 0
            && let Some(error) = items
                .iter()
                .rev()
                .find(|i| i["state"] == "failed")
                .and_then(|i| i["error"].as_str())
        {
            lines = lines.push(text(error.to_owned()).size(style::CAPTION).color(p.danger));
        }
        if !status["uploader_running"].as_bool().unwrap_or(true) && waiting > 0 {
            lines = lines.push(
                text("No uploader is running; keep this window open to upload.")
                    .size(style::CAPTION)
                    .color(p.warn),
            );
        }
        lines.into()
    }

    /// The destination manager, shown as a sheet over the workbench.
    pub fn manager(&self, dark: bool) -> Element<'_, Message> {
        let p = Palette::of(dark);
        let mut content = column![
            row![
                text("Destinations").size(style::DISPLAY).font(style::BOLD),
                space::horizontal(),
                super::tip(
                    button(icon(Icon::Close, 14.0, p.secondary))
                        .padding(6)
                        .style(style::plain)
                        .on_press(Message::CloseManager),
                    super::Action::Overview.hint("Close", super::Os::CURRENT),
                ),
            ]
            .align_y(Alignment::Center),
            text(
                "Save captures and recordings in a folder (including external and network drives) or upload them to an S3-compatible bucket. Files are always written locally first, within your storage limits.",
            )
            .size(style::SMALL)
            .color(p.secondary),
        ]
        .spacing(10);
        if let Some((message, error)) = &self.notice {
            content = content.push(text(message.clone()).size(style::SMALL).color(if *error {
                p.danger
            } else {
                p.accent_text
            }));
        }
        content = content.push(space().height(4));
        content = match &self.editor {
            None => content.push(self.list(p)),
            Some(editor) => content.push(self.form(editor, p)),
        };
        container(scrollable(content.padding(24)).style(style::scroll))
            .width(520)
            .max_height(640)
            .style(style::sheet)
            .into()
    }

    fn list(&self, p: &'static Palette) -> Element<'_, Message> {
        let mut rows = column![].spacing(2);
        for destination in &self.saved {
            let name = destination.name.clone();
            let (first, second) = if self.removing.as_deref() == Some(name.as_str()) {
                (
                    button(text("Remove").size(style::SMALL))
                        .style(style::danger)
                        .on_press(Message::Remove(name)),
                    button(text("Cancel").size(style::SMALL))
                        .style(style::plain)
                        .on_press(Message::ConfirmRemove(None)),
                )
            } else {
                (
                    button(text("Edit…").size(style::SMALL))
                        .style(style::link)
                        .on_press(Message::Edit(name.clone())),
                    button(text("Remove").size(style::SMALL))
                        .style(style::plain)
                        .on_press(Message::ConfirmRemove(Some(name))),
                )
            };
            rows = rows.push(
                container(
                    row![
                        icon(
                            if matches!(destination.target, Target::S3(_)) {
                                Icon::Broadcast
                            } else {
                                Icon::Folder
                            },
                            15.0,
                            p.accent,
                        ),
                        column![
                            text(destination.name.clone())
                                .size(style::BODY)
                                .font(style::SEMIBOLD),
                            text(destination.describe())
                                .size(style::CAPTION)
                                .color(p.secondary),
                        ]
                        .spacing(2)
                        .width(Fill),
                        first.padding([3, 8]),
                        second.padding([3, 8]),
                    ]
                    .spacing(10)
                    .align_y(Alignment::Center),
                )
                .padding([8, 4]),
            );
        }
        if self.saved.is_empty() {
            rows = rows.push(
                text("No saved destinations yet.")
                    .size(style::SMALL)
                    .color(p.secondary),
            );
        }
        column![
            rows,
            row![
                button(
                    row![
                        icon(Icon::Folder, 14.0, p.text),
                        text("Add folder…").size(style::BODY)
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center)
                )
                .padding([7, 12])
                .style(style::secondary)
                .on_press(Message::AddFolder),
                button(
                    row![
                        icon(Icon::Plus, 14.0, p.text),
                        text("Add S3 bucket…").size(style::BODY)
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center)
                )
                .padding([7, 12])
                .style(style::secondary)
                .on_press(Message::AddBucket),
            ]
            .spacing(8),
        ]
        .spacing(14)
        .into()
    }

    fn form<'a>(&'a self, editor: &'a Editor, p: &'static Palette) -> Element<'a, Message> {
        let field = |label: &'a str, control: Element<'a, Message>| -> Element<'a, Message> {
            row![
                text(label)
                    .size(style::BODY)
                    .color(p.secondary)
                    .width(Length::Fixed(110.0)),
                control,
            ]
            .spacing(12)
            .align_y(Alignment::Center)
            .into()
        };
        let input = |placeholder: &'a str, value: &'a str, on: fn(String) -> Message| {
            text_input(placeholder, value)
                .on_input(on)
                .on_submit(Message::Save)
                .size(style::BODY)
                .padding([6, 10])
                .style(style::input)
        };
        let mut form = column![
            text(if editor.s3 { "S3 bucket" } else { "Folder" })
                .size(style::HEADING)
                .font(style::SEMIBOLD)
                .color(p.accent_text),
            field(
                "Name",
                super::tip(
                    input("archive", &editor.name, Message::Name),
                    "Letters, digits, '.', '-' and '_'. Used with --destination on the command line.",
                ),
            ),
        ]
        .spacing(10);
        if editor.s3 {
            form = form.push(field(
                "Service",
                pick_list(
                    (0..PROVIDERS.len()).map(Provider).collect::<Vec<_>>(),
                    Some(Provider(editor.provider)),
                    Message::Provider,
                )
                .width(Fill)
                .text_size(style::BODY)
                .padding([6, 10])
                .style(style::pick)
                .menu_style(style::menu)
                .into(),
            ));
            if editor.provider != 0 {
                form = form.push(field(
                    "Endpoint",
                    input("https://", &editor.endpoint, Message::Endpoint).into(),
                ));
            }
            form = form
                .push(field(
                    "Region",
                    input("us-east-1", &editor.region, Message::Region).into(),
                ))
                .push(field(
                    "Bucket",
                    input("bucket", &editor.bucket, Message::Bucket).into(),
                ))
                .push(field(
                    "Key prefix",
                    input("cameras/line-1/", &editor.prefix, Message::Prefix).into(),
                ))
                .push(field(
                    "Credentials",
                    pick_list(&Source::ALL[..], Some(editor.source), Message::Source)
                        .width(Fill)
                        .text_size(style::BODY)
                        .padding([6, 10])
                        .style(style::pick)
                        .menu_style(style::menu)
                        .into(),
                ));
            form = match editor.source {
                Source::Keychain => form
                    .push(field(
                        "Access key ID",
                        input("AKIA…", &editor.access_key_id, Message::AccessKey).into(),
                    ))
                    .push(field(
                        "Secret key",
                        text_input(
                            if editor.stored_secret {
                                if cfg!(target_os = "macos") {
                                    "Saved in your keychain; type to replace"
                                } else {
                                    "Saved in the OS keychain; type to replace"
                                }
                            } else if cfg!(target_os = "macos") {
                                "Saved to your keychain"
                            } else {
                                "Saved to the OS keychain"
                            },
                            &editor.secret,
                        )
                        .on_input(Message::Secret)
                        .on_submit(Message::Save)
                        .secure(true)
                        .size(style::BODY)
                        .padding([6, 10])
                        .style(style::input)
                        .into(),
                    )),
                Source::Profile => form.push(field(
                    "Profile",
                    input("default", &editor.profile, Message::Profile).into(),
                )),
                Source::Environment => form.push(field(
                    "",
                    text("AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY of the process that uploads")
                        .size(style::SMALL)
                        .color(p.secondary)
                        .into(),
                )),
            };
            form = form
                .push(field(
                    "",
                    super::tip(
                        super::checkbox(
                            "Path-style addressing",
                            editor.path_style,
                            Message::PathStyle,
                        ),
                        "Needed by MinIO and most non-AWS services",
                    ),
                ))
                .push(field(
                    "",
                    super::checkbox(
                        "Keep local copies after upload",
                        editor.keep_local,
                        Message::KeepLocal,
                    ),
                ));
        } else {
            form = form.push(field(
                "Folder",
                row![
                    input("/Volumes/Drive/Capturefab", &editor.folder, Message::Folder),
                    super::tip(
                        button(icon(Icon::Folder, 15.0, p.secondary))
                            .padding(6)
                            .style(style::plain)
                            .on_press(Message::BrowseFolder),
                        "Choose a folder",
                    ),
                ]
                .spacing(6)
                .align_y(Alignment::Center)
                .into(),
            ));
        }
        if let Some((message, error)) = &editor.message {
            form = form.push(text(message.clone()).size(style::SMALL).color(if *error {
                p.danger
            } else {
                p.live
            }));
        }
        let testing = editor.testing.is_some();
        let cancel = button(text("Cancel").size(style::BODY))
            .padding([7, 14])
            .style(style::plain)
            .on_press(Message::Cancel);
        let save = button(text("Save").size(style::BODY))
            .padding([7, 18])
            .style(style::primary)
            .on_press(Message::Save);
        let (first, second) = if cfg!(target_os = "windows") {
            (save, cancel)
        } else {
            (cancel, save)
        };
        form.push(space().height(6))
            .push(
                row![
                    super::tip(
                        button(text(if testing { "Testing…" } else { "Test" }).size(style::BODY))
                            .padding([7, 14])
                            .style(style::secondary)
                            .on_press_maybe((!testing).then_some(Message::Test)),
                        "Saves the secret key first, then checks the folder or bucket",
                    ),
                    space::horizontal(),
                    first,
                    second,
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            )
            .into()
    }
}

fn label<'a>(value: &'a str, p: &'static Palette) -> Element<'a, Message> {
    text(value).size(style::SMALL).color(p.secondary).into()
}
