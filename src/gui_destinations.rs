//! The GUI's "Save to" picker and destination manager: save captures and
//! recordings on this computer, in a folder on an external or network drive,
//! or in an S3-compatible bucket.
//!
//! Disk scans, bucket checks and queue status run on background threads, so a
//! slow network share or service never stalls the interface.
use crate::destination::{self, Bucket, Credentials, Destination, Target};
use crate::volumes::{self, Volume};
use eframe::egui::{self, RichText, Vec2};
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::mpsc::{Receiver, channel},
    thread,
    time::{Duration, Instant},
};

/// Run `work` on a thread and repaint when it is done.
fn background<T: Send + 'static>(
    ctx: &egui::Context,
    work: impl FnOnce() -> T + Send + 'static,
) -> Receiver<T> {
    let (sender, receiver) = channel();
    let ctx = ctx.clone();
    let _ = thread::Builder::new()
        .name("capturefab-destination".into())
        .spawn(move || {
            let _ = sender.send(work());
            ctx.request_repaint();
        });
    receiver
}

fn bytes(value: u64) -> String {
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

/// One control-high row laid out right to left, so trailing buttons take
/// their natural size and the field before them fills what is left.
fn row(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let size = Vec2::new(ui.available_width(), ui.spacing().interact_size.y.max(26.0));
    ui.allocate_ui_with_layout(size, egui::Layout::right_to_left(egui::Align::Center), add);
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

#[derive(Clone, Copy, PartialEq)]
enum Source {
    Keychain,
    Profile,
    Environment,
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
                        editor.access_key_id = access_key_id.clone()
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
            std::fs::create_dir_all(path).map_err(|e| format!("cannot create the folder: {e}"))?;
            let probe = path.join(format!(".capturefab-write-test-{}", std::process::id()));
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&probe)
                .map_err(|e| format!("cannot write here: {e}"))?;
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

/// The capture panel's destination choice and everything behind it.
pub struct Picker {
    saved: Vec<Destination>,
    loaded: Option<Instant>,
    /// None saves to the output path on this computer.
    pub selected: Option<String>,
    volumes: Vec<Volume>,
    volumes_task: Option<Receiver<Vec<Volume>>>,
    volumes_at: Option<Instant>,
    uploads: Option<Value>,
    uploads_task: Option<Receiver<Option<Value>>>,
    uploads_at: Option<Instant>,
    editor: Option<Editor>,
    manager_open: bool,
    notice: Option<(String, bool)>,
    /// Remember the choice for the next launch (the capture panel does).
    remember: bool,
}

impl Picker {
    pub fn new(remember: bool) -> Self {
        Self {
            saved: Vec::new(),
            loaded: None,
            selected: if remember {
                destination::preferred()
            } else {
                None
            },
            volumes: Vec::new(),
            volumes_task: None,
            volumes_at: None,
            uploads: None,
            uploads_task: None,
            uploads_at: None,
            editor: None,
            manager_open: false,
            notice: None,
            remember,
        }
    }
    fn refresh(&mut self, ctx: &egui::Context) {
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
            self.volumes_task = Some(background(ctx, volumes::list));
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
            self.uploads_task = Some(background(ctx, || crate::upload::status().ok()));
        }
    }

    /// Where a capture with this output goes, for compact displays.
    pub fn label(&self, output: &str) -> String {
        match &self.selected {
            Some(name) => format!("{name}: {output}"),
            None => output.to_string(),
        }
    }

    fn select(&mut self, name: Option<String>) {
        if self.selected != name {
            if self.remember {
                let _ = destination::set_preferred(name.as_deref());
            }
            self.selected = name;
        }
    }

    fn selected_destination(&self) -> Option<&Destination> {
        let name = self.selected.as_ref()?;
        self.saved.iter().find(|d| &d.name == name)
    }

    /// The "Save to" row, the output field and the drive shortcuts. `output`
    /// is the path on this computer, or a name inside the destination.
    pub fn ui(&mut self, ui: &mut egui::Ui, output: &mut String, id: &str, local_label: &str) {
        self.refresh(ui.ctx());
        ui.label("Save to");
        let label = match self.selected_destination() {
            None => "This computer".to_string(),
            Some(d) => match d.target {
                Target::Folder { .. } => format!("{} (folder)", d.name),
                Target::S3(_) => format!("{} (S3)", d.name),
            },
        };
        let mut choice = self.selected.clone();
        // Right to left: the button takes its size, the list the rest.
        row(ui, |ui| {
            if ui
                .button("Manage…")
                .on_hover_text("Add, edit or test folders, external drives and S3 buckets")
                .clicked()
            {
                self.manager_open = true;
            }
            egui::ComboBox::from_id_salt(format!("{id}-save-to"))
                .selected_text(label)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut choice, None, "This computer (path below)");
                    for destination in &self.saved {
                        let kind = if matches!(destination.target, Target::S3(_)) {
                            "S3"
                        } else {
                            "folder"
                        };
                        ui.selectable_value(
                            &mut choice,
                            Some(destination.name.clone()),
                            format!("{} ({kind})", destination.name),
                        )
                        .on_hover_text(destination.describe());
                    }
                });
        });
        if choice != self.selected {
            self.select(choice);
        }
        let selected = self.selected_destination().cloned();
        match &selected {
            None => {
                ui.add_space(4.0);
                ui.label(local_label);
                row(ui, |ui| {
                    if ui
                        .button("Folder…")
                        .on_hover_text("Pick a folder with the system file dialog")
                        .clicked()
                        && let Some(folder) = rfd::FileDialog::new()
                            .set_title("Save captures in")
                            .pick_folder()
                    {
                        let name = PathBuf::from(output.as_str())
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "capture.png".into());
                        *output = folder.join(name).to_string_lossy().into_owned();
                    }
                    ui.add(egui::TextEdit::singleline(output).desired_width(ui.available_width()));
                });
            }
            Some(destination) => {
                ui.add_space(4.0);
                ui.label(if id == "record" {
                    "Recording file name in destination"
                } else {
                    "Name in destination (file, folder or {frame} template)"
                });
                ui.add(egui::TextEdit::singleline(output).desired_width(f32::INFINITY));
                // A destination holds relative names; drop a leftover absolute path.
                if PathBuf::from(output.as_str()).is_absolute() {
                    *output = PathBuf::from(output.as_str())
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                }
                self.summary(ui, destination);
            }
        }
        self.drives(ui);
        self.manager(ui.ctx());
    }

    fn summary(&mut self, ui: &mut egui::Ui, destination: &Destination) {
        let weak = ui.visuals().weak_text_color();
        match &destination.target {
            Target::Folder { path } => {
                let free = fs2::available_space(path).ok();
                let mounted = volumes::ensure_present(path).is_ok();
                ui.label(
                    RichText::new(format!(
                        "{}{}",
                        path.display(),
                        free.map(|f| format!(" · {} free", bytes(f)))
                            .unwrap_or_default()
                    ))
                    .small()
                    .color(weak),
                );
                if !mounted {
                    ui.label(
                        RichText::new("Drive not connected: captures will fail until it is.")
                            .small()
                            .color(ui.visuals().warn_fg_color),
                    );
                }
            }
            Target::S3(bucket) => {
                ui.label(
                    RichText::new(format!(
                        "s3://{}/{} · staged locally, then uploaded{}",
                        bucket.bucket,
                        bucket.prefix,
                        if bucket.keep_local {
                            " (local copy kept)"
                        } else {
                            ""
                        }
                    ))
                    .small()
                    .color(weak),
                );
                if bucket.insecure() {
                    ui.label(
                        RichText::new("Plain http: requests are signed but data is not encrypted.")
                            .small()
                            .color(ui.visuals().warn_fg_color),
                    );
                }
                self.upload_status(ui, Some(&destination.name));
            }
        }
    }

    /// Waiting and failed uploads, with a retry button.
    fn upload_status(&mut self, ui: &mut egui::Ui, destination: Option<&str>) {
        let Some(status) = &self.uploads else {
            return;
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
        ui.horizontal(|ui| {
            let text = match (waiting, failed) {
                (0, 0) => format!("All uploaded · {} sent in total", status["uploaded"]),
                (w, 0) => format!("Uploading {w} file{}", if w == 1 { "" } else { "s" }),
                (w, f) => format!("{w} waiting · {f} failed"),
            };
            ui.label(RichText::new(text).small().color(if failed > 0 {
                ui.visuals().error_fg_color
            } else {
                ui.visuals().weak_text_color()
            }));
            #[cfg(feature = "s3")]
            if failed > 0 && ui.small_button("Retry").clicked() {
                let _ = crate::upload::retry(None);
                self.uploads_at = None;
            }
        });
        if failed > 0
            && let Some(error) = items
                .iter()
                .rev()
                .find(|i| i["state"] == "failed")
                .and_then(|i| i["error"].as_str())
        {
            ui.label(
                RichText::new(error)
                    .small()
                    .color(ui.visuals().error_fg_color),
            );
        }
        if !status["uploader_running"].as_bool().unwrap_or(true) && waiting > 0 {
            ui.label(
                RichText::new("No uploader is running; keep this window open to upload.")
                    .small()
                    .color(ui.visuals().warn_fg_color),
            );
        }
    }

    /// One-click buttons for connected external and network drives.
    fn drives(&mut self, ui: &mut egui::Ui) {
        if self.volumes.is_empty() {
            return;
        }
        ui.add_space(4.0);
        ui.label(
            RichText::new("External drives")
                .small()
                .color(ui.visuals().weak_text_color()),
        );
        let mut chosen = None;
        ui.horizontal_wrapped(|ui| {
            for volume in &self.volumes {
                let kind = if volume.kind == "network" {
                    "network"
                } else {
                    "drive"
                };
                let text = format!(
                    "{} · {} free · {kind}",
                    volume.name,
                    bytes(volume.free_bytes)
                );
                if ui
                    .button(RichText::new(text).small())
                    .on_hover_text(format!(
                        "Save to a Capturefab folder on {} ({} of {})",
                        volume.path.display(),
                        bytes(volume.free_bytes),
                        bytes(volume.total_bytes)
                    ))
                    .clicked()
                {
                    chosen = Some(volume.clone());
                }
            }
        });
        if let Some(volume) = chosen {
            self.use_volume(&volume);
        }
    }

    /// Save to `Capturefab/` on a drive, reusing a destination already there.
    fn use_volume(&mut self, volume: &Volume) {
        let folder = volume.path.join("Capturefab");
        if let Some(existing) = self
            .saved
            .iter()
            .find(|d| matches!(&d.target, Target::Folder { path } if path == &folder))
        {
            let name = existing.name.clone();
            self.select(Some(name));
            return;
        }
        let name: String = volume
            .name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                    c
                } else {
                    '-'
                }
            })
            .collect::<String>()
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
                self.select(Some(name.clone()));
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

    fn manager(&mut self, ctx: &egui::Context) {
        if !self.manager_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Destinations")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(460.0)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "Save captures and recordings in a folder (including external and network drives) or upload them to an S3-compatible bucket. Files are always written locally first, within your storage limits.",
                    )
                    .small()
                    .color(ui.visuals().weak_text_color()),
                );
                ui.add_space(6.0);
                if let Some((text, error)) = &self.notice {
                    ui.label(RichText::new(text).small().color(if *error {
                        ui.visuals().error_fg_color
                    } else {
                        ui.visuals().weak_text_color()
                    }));
                }
                if self.editor.is_none() {
                    self.list(ui);
                }
                if let Some(saved) = self.form(ui) {
                    self.loaded = None;
                    self.select(Some(saved));
                }
            });
        if !open {
            self.manager_open = false;
            self.editor = None;
        }
    }

    fn list(&mut self, ui: &mut egui::Ui) {
        let mut edit = None;
        let mut remove = None;
        egui::Grid::new("destination-list")
            .num_columns(3)
            .striped(true)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                for destination in &self.saved {
                    ui.label(RichText::new(&destination.name).strong());
                    ui.label(RichText::new(destination.describe()).small());
                    ui.horizontal(|ui| {
                        if ui.small_button("Edit").clicked() {
                            edit = Some(destination.clone());
                        }
                        if ui.small_button("Remove").clicked() {
                            remove = Some(destination.name.clone());
                        }
                    });
                    ui.end_row();
                }
            });
        if self.saved.is_empty() {
            ui.label(RichText::new("No saved destinations yet.").small());
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Add folder…").clicked()
                && let Some(folder) = rfd::FileDialog::new()
                    .set_title("Save captures in")
                    .pick_folder()
            {
                let name = folder
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "folder".into())
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                            c
                        } else {
                            '-'
                        }
                    })
                    .collect();
                self.editor = Some(Editor::new_folder(name, folder));
            }
            if ui.button("Add S3 bucket…").clicked() {
                self.editor = Some(Editor::new_bucket());
            }
        });
        if let Some(destination) = edit {
            self.editor = Some(Editor::edit(&destination));
        }
        if let Some(name) = remove {
            self.notice = Some(match destination::remove(&name) {
                Ok(_) => (format!("Removed {name}; its files are untouched"), false),
                Err(e) => (format!("{e:#}"), true),
            });
            self.loaded = None;
        }
    }

    /// The add/edit form; returns the name of a destination it saved.
    fn form(&mut self, ui: &mut egui::Ui) -> Option<String> {
        let editor = self.editor.as_mut()?;
        let mut saved = None;
        let mut close = false;
        ui.separator();
        ui.label(
            RichText::new(if editor.s3 { "S3 bucket" } else { "Folder" })
                .size(15.0)
                .strong(),
        );
        egui::Grid::new("destination-form")
            .num_columns(2)
            .spacing([10.0, 8.0])
            .show(ui, |ui| {
                ui.label("Name");
                ui.add(egui::TextEdit::singleline(&mut editor.name).desired_width(240.0))
                    .on_hover_text("Letters, digits, '.', '-' and '_'. Used with --destination on the command line.");
                ui.end_row();
                if !editor.s3 {
                    ui.label("Folder");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut editor.folder).desired_width(240.0));
                        if ui.button("Browse…").clicked()
                            && let Some(folder) = rfd::FileDialog::new().pick_folder()
                        {
                            editor.folder = folder.to_string_lossy().into_owned();
                        }
                    });
                    ui.end_row();
                    return;
                }
                ui.label("Service");
                let previous = editor.provider;
                egui::ComboBox::from_id_salt("destination-provider")
                    .selected_text(PROVIDERS[editor.provider].0)
                    .width(240.0)
                    .show_ui(ui, |ui| {
                        for (index, (name, ..)) in PROVIDERS.iter().enumerate() {
                            ui.selectable_value(&mut editor.provider, index, *name);
                        }
                    });
                if editor.provider != previous {
                    let (_, endpoint, region, path_style) = PROVIDERS[editor.provider];
                    editor.endpoint = endpoint.into();
                    editor.region = region.into();
                    editor.path_style = path_style;
                }
                ui.end_row();
                if editor.provider != 0 {
                    ui.label("Endpoint");
                    ui.add(egui::TextEdit::singleline(&mut editor.endpoint).desired_width(240.0));
                    ui.end_row();
                }
                ui.label("Region");
                ui.add(egui::TextEdit::singleline(&mut editor.region).desired_width(240.0));
                ui.end_row();
                ui.label("Bucket");
                ui.add(egui::TextEdit::singleline(&mut editor.bucket).desired_width(240.0));
                ui.end_row();
                ui.label("Key prefix");
                ui.add(
                    egui::TextEdit::singleline(&mut editor.prefix)
                        .hint_text("cameras/line-1/")
                        .desired_width(240.0),
                );
                ui.end_row();
                ui.label("Credentials");
                egui::ComboBox::from_id_salt("destination-credentials")
                    .selected_text(match editor.source {
                        Source::Keychain => "Access key (OS keychain)",
                        Source::Profile => "AWS profile",
                        Source::Environment => "Environment variables",
                    })
                    .width(240.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut editor.source, Source::Keychain, "Access key (OS keychain)");
                        ui.selectable_value(&mut editor.source, Source::Profile, "AWS profile (~/.aws/credentials)");
                        ui.selectable_value(&mut editor.source, Source::Environment, "Environment variables");
                    });
                ui.end_row();
                match editor.source {
                    Source::Keychain => {
                        ui.label("Access key ID");
                        ui.add(egui::TextEdit::singleline(&mut editor.access_key_id).desired_width(240.0));
                        ui.end_row();
                        ui.label("Secret key");
                        #[cfg(feature = "s3")]
                        let stored = !editor.access_key_id.trim().is_empty()
                            && crate::s3::has_secret(editor.access_key_id.trim());
                        #[cfg(not(feature = "s3"))]
                        let stored = false;
                        ui.add(
                            egui::TextEdit::singleline(&mut editor.secret)
                                .password(true)
                                .hint_text(if stored { "saved in keychain; type to replace" } else { "saved to the OS keychain" })
                                .desired_width(240.0),
                        );
                        ui.end_row();
                    }
                    Source::Profile => {
                        ui.label("Profile");
                        ui.add(egui::TextEdit::singleline(&mut editor.profile).desired_width(240.0));
                        ui.end_row();
                    }
                    Source::Environment => {
                        ui.label("");
                        ui.label(RichText::new("AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY of the process that uploads").small());
                        ui.end_row();
                    }
                }
                ui.label("");
                ui.checkbox(&mut editor.path_style, "Path-style addressing")
                    .on_hover_text("Needed by MinIO and most non-AWS services");
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut editor.keep_local, "Keep local copies after upload");
                ui.end_row();
            });
        if let Some(task) = &editor.testing
            && let Ok(result) = task.try_recv()
        {
            editor.message = Some(match result {
                Ok(text) => (text, false),
                Err(text) => (text, true),
            });
            editor.testing = None;
        }
        if let Some((text, error)) = &editor.message {
            ui.label(RichText::new(text).small().color(if *error {
                ui.visuals().error_fg_color
            } else {
                ui.visuals().weak_text_color()
            }));
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui
                .add(egui::Button::new("Save").min_size(Vec2::new(90.0, 28.0)))
                .clicked()
            {
                match editor.save() {
                    Ok(name) => {
                        saved = Some(name.clone());
                        close = true;
                        self.notice = Some((format!("Saved {name}"), false));
                    }
                    Err(error) => editor.message = Some((format!("{error:#}"), true)),
                }
            }
            let testing = editor.testing.is_some();
            if ui
                .add_enabled(
                    !testing,
                    egui::Button::new(if testing { "Testing…" } else { "Test" }),
                )
                .on_hover_text("Saves the secret key first, then checks the folder or bucket")
                .clicked()
            {
                // Testing a bucket needs the key, so store a typed secret first.
                #[cfg(feature = "s3")]
                if editor.s3 && editor.source == Source::Keychain && !editor.secret.is_empty() {
                    match crate::s3::store_secret(editor.access_key_id.trim(), &editor.secret) {
                        Ok(()) => editor.secret.clear(),
                        Err(e) => editor.message = Some((format!("{e:#}"), true)),
                    }
                }
                match editor.destination() {
                    Ok(destination) => {
                        editor.message = None;
                        editor.testing = Some(background(ui.ctx(), move || test(destination)));
                    }
                    Err(error) => editor.message = Some((format!("{error:#}"), true)),
                }
            }
            if ui.button("Cancel").clicked() {
                close = true;
            }
        });
        if close {
            self.editor = None;
        }
        saved
    }
}
