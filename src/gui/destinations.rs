//! Local capture destinations: folders on this computer and mounted drives.
//! Filesystem checks and saved-list writes run away from the UI thread.
use super::button::button;
use super::format::capitalize;
use super::icon::{self, Icon, icon};
use super::motion::Flashes;
use super::notice::Level;
use super::pick_list::pick_list;
use super::style::{self, Palette};
use super::text_input::{self, text_input};
use super::widgets::{SheetFrame, fade, field_error, one_line, sheet_frame};
use crate::destination::{self, Destination, Target};
use crate::volumes::{self, Volume};
use iced::widget::{column, container, row, space, text};
use iced::{Alignment, Element, Fill, Length, Theme};
use std::{
    fmt,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, TryRecvError, channel},
    sync::{Arc, Mutex, PoisonError},
    thread,
    time::{Duration, Instant, SystemTime},
};

const SAVED_GLOW: Duration = Duration::from_millis(1000);
const NAME_FIELD: &str = "destination-name";
const REMOVE_CANCEL: &str = "destination-remove-cancel";
const REMOVE_TRIGGER: &str = "destination-remove-trigger";
const SHEET_WIDTH: f32 = 560.0;
#[cfg(test)]
mod confirmation_tests;
#[cfg(test)]
mod form_tests;
#[cfg(test)]
pub(super) mod layout_tests;
mod naming_presets;
pub use naming_presets::Naming;
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Name,
    Folder,
}
#[derive(Clone, PartialEq, Eq)]
struct Draft {
    name: String,
    folder: String,
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

#[derive(Clone, Debug)]
struct Selection {
    selected: Option<String>,
    output: String,
}
#[derive(Clone, Debug)]
pub struct Change {
    before: Option<Destination>,
    after: Option<Destination>,
    before_selection: Selection,
    after_selection: Selection,
    label: String,
    created_at: Instant,
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
impl Field {
    fn of(message: &str) -> Option<Self> {
        if message.starts_with("destination names") {
            Some(Self::Name)
        } else if message.starts_with("a folder destination") {
            Some(Self::Folder)
        } else {
            None
        }
    }
}
struct Editor {
    original: Option<String>,
    name: String,
    folder: String,
    invalid: Vec<(Field, String)>,
    message: Option<(String, bool)>,
    testing: Option<Receiver<Result<String, String>>>,
    saving: Option<(Destination, Receiver<anyhow::Result<()>>)>,
    history: DraftHistory,
}
impl Editor {
    fn new_folder(name: String, folder: PathBuf) -> Self {
        let valid = folder.to_str().is_some();
        Self {
            original: None,
            name,
            folder: folder.to_str().unwrap_or_default().into(),
            invalid: if valid {
                Vec::new()
            } else {
                vec![(Field::Folder, folder_encoding_error().into())]
            },
            message: None,
            testing: None,
            saving: None,
            history: DraftHistory::default(),
        }
    }
    fn edit(destination: &Destination) -> Self {
        let Target::Folder { path } = &destination.target;
        let mut result = Self::new_folder(destination.name.clone(), path.clone());
        result.original = Some(destination.name.clone());
        result
    }
    fn draft(&self) -> Draft {
        Draft {
            name: self.name.clone(),
            folder: self.folder.clone(),
        }
    }
    fn restore_draft(&mut self, draft: Draft) {
        self.name = draft.name;
        self.folder = draft.folder;
        self.invalid.clear();
        self.edited(None);
    }
    fn build(&self, stand_ins: &[Field]) -> Destination {
        Destination {
            name: if stand_ins.contains(&Field::Name) {
                "destination".into()
            } else {
                self.name.trim().into()
            },
            target: Target::Folder {
                path: if stand_ins.contains(&Field::Folder) {
                    std::env::temp_dir()
                } else {
                    PathBuf::from(&self.folder)
                },
            },
        }
    }
    fn destination(&self) -> anyhow::Result<Destination> {
        let d = self.build(&[]);
        d.validate()?;
        Ok(d)
    }
    fn problems(&self) -> Vec<(Field, String)> {
        let mut found = Vec::new();
        loop {
            let fields: Vec<Field> = found.iter().map(|(f, _)| *f).collect();
            let Err(e) = self.build(&fields).validate() else {
                break;
            };
            let message = e.to_string();
            match Field::of(&message) {
                Some(field) if !fields.contains(&field) => found.push((field, message)),
                _ => break,
            }
        }
        found
    }
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
            self.invalid = problems;
            self.message = None;
        }
    }
    fn refuse_name(&mut self, why: String) {
        self.invalid = vec![(Field::Name, why)];
        self.message = None;
    }
    fn name_taken(&self, saved: &[Destination]) -> Option<String> {
        let name = self.name.trim();
        (self.original.as_deref() != Some(name) && saved.iter().any(|d| d.name == name))
            .then(|| format!("a destination named {name} already exists; choose another name"))
    }
    fn edited(&mut self, field: Option<Field>) {
        if let Some(field) = field {
            self.invalid.retain(|(f, _)| *f != field);
        }
        self.message = None;
        self.testing = None;
    }
    fn refusal(&self, field: Field) -> Option<&str> {
        self.invalid
            .iter()
            .find(|(f, _)| *f == field)
            .map(|(_, m)| m.as_str())
    }
    fn prepare(
        &self,
        destination: &Destination,
        before: Option<Destination>,
        remember: bool,
    ) -> impl FnOnce() -> anyhow::Result<()> + Send + 'static {
        let destination = destination.clone();
        move || {
            let Target::Folder { path } = &destination.target;
            destination::replace_with_selection(
                before.as_ref(),
                Some(&destination),
                Some((Some(&destination.name), remember)),
                || {
                    volumes::ensure_present(path)?;
                    std::fs::create_dir_all(path)?;
                    Ok(())
                },
                |_| Ok(()),
            )
        }
    }
    fn save(&mut self, remember: bool) -> anyhow::Result<String> {
        let d = self.destination()?;
        let before = self
            .original
            .as_ref()
            .map(|n| destination::get(n))
            .transpose()?;
        self.prepare(&d, before, remember)()?;
        self.original = Some(d.name.clone());
        Ok(d.name)
    }
    fn poll_test(&mut self) {
        let answer = match self.testing.as_ref().map(Receiver::try_recv) {
            None | Some(Err(TryRecvError::Empty)) => return,
            Some(Ok(a)) => a,
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
    fn poll_save(&mut self) -> Option<String> {
        let prepared = match self.saving.as_ref()?.1.try_recv() {
            Ok(v) => v,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                Err(anyhow::anyhow!("saving stopped without an answer"))
            }
        };
        let (d, _) = self.saving.take()?;
        match prepared {
            Ok(()) => {
                self.original = Some(d.name.clone());
                Some(d.name)
            }
            Err(e) => {
                self.refuse(e);
                None
            }
        }
    }
}
fn test(destination: Destination) -> Result<String, String> {
    (move || -> anyhow::Result<String> {
        let Target::Folder { path } = &destination.target;
        volumes::ensure_present(path)?;
        std::fs::create_dir_all(path)?;
        let mut random = [0; 16];
        getrandom::fill(&mut random)
            .map_err(|error| anyhow::anyhow!("create folder test name: {error}"))?;
        let token: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let probe = path.join(format!(".capturefab-write-test-{token}"));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)?;
        drop(file);
        std::fs::remove_file(probe)?;
        Ok(format!("Folder is writable: {}", path.display()))
    })()
    .map_err(|e| format!("{e:#}"))
}
#[derive(Clone, Debug)]
pub enum Message {
    Undo,
    Redo,
    Select(Choice),
    Output(String),
    Naming(Naming),
    BrowseOutput,
    UseVolume(usize),
    OpenManager,
    CloseManager,
    Edit(String),
    Remove(String),
    ConfirmRemove(Option<String>),
    AddFolder,
    Name(String),
    Folder(String),
    BrowseFolder,
    Save,
    Test,
    Cancel,
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

#[derive(Default)]
struct Watch {
    saved: Polled<(ListVersion, Vec<Destination>)>,
    volumes: Polled<Vec<Volume>>,
}
impl Watch {
    fn poll(&mut self) {
        let known = self.saved.value.as_ref().map(|(v, _)| *v);
        self.saved.poll(Duration::from_secs(3), move || {
            let v = list_version();
            (known != Some(v)).then(|| (v, destination::list().unwrap_or_default()))
        });
        self.volumes.poll(Duration::from_secs(5), || {
            Some(
                if std::env::var_os("CAPTUREFAB_SCREENSHOT_SCENE").is_some() {
                    Vec::new()
                } else {
                    volumes::list()
                },
            )
        });
    }
    fn saved_changed(&mut self) {
        self.saved.value = None;
        self.saved.at = None;
    }
}
struct FolderStatus {
    path: PathBuf,
    free: Option<u64>,
    mounted: bool,
}
pub struct Picker {
    saved: Vec<Destination>,
    loaded: Option<Instant>,
    pub selected: Option<String>,
    volumes: Vec<Volume>,
    folder: Option<FolderStatus>,
    folder_task: Option<Receiver<FolderStatus>>,
    folder_at: Option<Instant>,
    watch: Arc<Mutex<Watch>>,
    seen: [u64; 2],
    editor: Option<Editor>,
    removing: Option<String>,
    removal_focus: Option<String>,
    pub manager_open: bool,
    notice: Option<(String, bool)>,
    saved_glow: Flashes<String>,
    scrolled: bool,
    to_top: bool,
    focus: Option<&'static str>,
    remember: bool,
    recording: bool,
    custom_naming: bool,
    changes: Vec<Change>,
    saving_context: Option<SaveContext>,
}
impl Picker {
    super::motion::registry! {motions:[],flashes:[saved_glow],}
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
            watch: Arc::default(),
            seen: [0; 2],
            editor: None,
            removing: None,
            removal_focus: None,
            manager_open: false,
            notice: None,
            saved_glow: Flashes::new(SAVED_GLOW),
            scrolled: false,
            to_top: false,
            focus: None,
            remember,
            recording,
            custom_naming: false,
            changes: Vec::new(),
            saving_context: None,
        }
    }
    pub fn sharing(mut self, other: &Picker) -> Self {
        self.watch = other.watch.clone();
        self
    }
    pub fn tick(&mut self, output: &mut String) {
        if let Some(name) = self.editor.as_mut().and_then(Editor::poll_save) {
            let context = self.saving_context.take();
            let after = self.editor.as_ref().and_then(|e| e.destination().ok());
            self.saved_as(name, output);
            if let Some(c) = context {
                self.record_change(c.before, after, c.selection, output, c.created_at);
            }
        } else if self.editor.as_ref().is_none_or(|e| e.saving.is_none()) {
            self.saving_context = None;
        }
        let watch = self.watch.clone();
        let mut watch = watch.lock().unwrap_or_else(PoisonError::into_inner);
        if self.loaded.is_none() {
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
        drop(watch);
        if let Some(task) = &self.folder_task
            && let Ok(status) = task.try_recv()
        {
            self.folder = Some(status);
            self.folder_task = None;
        }
        let folder = self.selected_destination().map(|d| d.local_root());
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
        if let Some(e) = &mut self.editor {
            e.poll_test();
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
        self.editor.as_ref().is_some_and(|e| e.saving.is_some())
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
        before_selection: Selection,
        output: &str,
        created_at: Instant,
    ) {
        let label = match (&before, &after) {
            (None, Some(d)) => format!("Add destination {}", d.name),
            (Some(d), None) => format!("Remove destination {}", d.name),
            (_, Some(d)) => format!("Edit destination {}", d.name),
            _ => return,
        };
        self.changes.push(Change {
            before,
            after,
            before_selection,
            after_selection: self.selection(output),
            label,
            created_at,
        });
    }
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
        destination::replace_with_selection(
            before.as_ref(),
            after.as_ref(),
            Some((selection.selected.as_deref(), self.remember)),
            || Ok(()),
            |_| Ok(()),
        )
        .map_err(|e| format!("{e:#}"))?;
        self.saved = destination::list().unwrap_or_default();
        self.loaded = Some(Instant::now());
        self.selected = selection.selected.clone();
        *output = selection.output.clone();
        self.folder = None;
        self.notice = None;
        Ok(true)
    }
    pub fn take_replay_result(&mut self, _output: &mut String) -> Option<Result<(), String>> {
        None
    }
    pub fn busy(&self) -> bool {
        self.editor
            .as_ref()
            .is_some_and(|e| e.testing.is_some() || e.saving.is_some())
    }
    pub fn sync(&mut self, _now: Instant) {}
    pub fn take_focus(&mut self) -> Option<&'static str> {
        self.focus.take()
    }
    pub fn take_to_top(&mut self) -> bool {
        std::mem::take(&mut self.to_top)
    }
    fn open_editor(&mut self, editor: Editor) {
        self.focus = Some(NAME_FIELD);
        self.editor = Some(editor);
        self.removing = None;
        self.removal_focus = None;
        self.scrolled = false;
        self.to_top = true;
    }
    fn close_editor(&mut self) {
        self.editor = None;
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
            if self.remember {
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
        if self.mutation_busy() && !matches!(message, Message::Scrolled(_)) {
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
                    let chosen = !single_edit(output, &value)
                        && Naming::PRESETS
                            .iter()
                            .any(|&n| self.with_naming(output, n) == value);
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
            Message::Scrolled(value) => self.scrolled = value,
            Message::Edit(name) => {
                self.removing = None;
                if let Some(d) = self.saved.iter().find(|d| d.name == name) {
                    self.open_editor(Editor::edit(d));
                }
            }
            Message::Remove(name) => {
                let at = Instant::now();
                self.removing = None;
                let selection = self.selection(output);
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
                        self.record_change(before, None, selection, output, at);
                        self.notice =
                            Some((format!("Removed {name}; its files are untouched"), false));
                        self.focus = Some("sheet-close");
                    }
                    Err(e) => self.notice = Some((format!("{e:#}"), true)),
                }
            }
            Message::ConfirmRemove(Some(name)) => {
                self.removal_focus = Some(name.clone());
                self.removing = Some(name);
                self.focus = Some(REMOVE_CANCEL);
            }
            Message::ConfirmRemove(None) => self.cancel_removal(),
            Message::AddFolder => {
                self.open_editor(Editor::new_folder(String::new(), PathBuf::new()))
            }
            Message::Cancel if self.removing.is_some() => self.cancel_removal(),
            Message::Cancel => self.close_editor(),
            Message::Save => {
                let selection = self.selection(output);
                let Some(e) = &mut self.editor else { return };
                let d = match e.destination() {
                    Ok(d) => d,
                    Err(error) => return e.refuse(error),
                };
                if let Some(why) = e.name_taken(&self.saved) {
                    return e.refuse_name(why);
                }
                let before = e
                    .original
                    .as_ref()
                    .and_then(|original| self.saved.iter().find(|d| &d.name == original))
                    .cloned();
                if e.original.is_some() && before.is_none() {
                    return e.refuse(anyhow::anyhow!(
                        "This destination was removed; reopen the editor"
                    ));
                }
                e.message = None;
                let prepare = e.prepare(&d, before.clone(), self.remember);
                e.saving = Some((d, background(prepare)));
                self.saving_context = Some(SaveContext {
                    before,
                    selection,
                    created_at: Instant::now(),
                });
            }
            Message::Test => {
                let Some(e) = &mut self.editor else { return };
                let d = match e.destination() {
                    Ok(d) => d,
                    Err(error) => return e.refuse(error),
                };
                e.message = None;
                e.invalid.clear();
                e.testing = Some(background(move || test(d)));
            }
            message => {
                let Some(e) = &mut self.editor else { return };
                let before = e.draft();
                let (label, coalesce) = match message {
                    Message::Name(_) => ("Destination name", true),
                    Message::Folder(_) => ("Destination folder", true),
                    _ => ("Destination folder", false),
                };
                let field = match message {
                    Message::Name(v) => {
                        e.name = v;
                        Some(Field::Name)
                    }
                    Message::Folder(v) => {
                        e.folder = v;
                        Some(Field::Folder)
                    }
                    Message::BrowseFolder => {
                        let Some(folder) = rfd::FileDialog::new().pick_folder() else {
                            return;
                        };
                        let Some(folder) = folder.to_str() else {
                            e.invalid.retain(|(f, _)| *f != Field::Folder);
                            e.invalid
                                .push((Field::Folder, folder_encoding_error().into()));
                            return;
                        };
                        e.folder = folder.into();
                        if e.name.is_empty() {
                            e.name = Path::new(folder)
                                .file_name()
                                .map(|n| safe_name(&n.to_string_lossy()))
                                .unwrap_or_else(|| "folder".into());
                        }
                        Some(Field::Folder)
                    }
                    _ => None,
                };
                e.edited(field);
                let after = e.draft();
                e.history.record(before, after, label, coalesce);
            }
        }
    }
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
                    self.record_change(None, after, before_selection, output, created_at);
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
            label: format!("{} (folder)", d.name),
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
                    "Manage folders and mounted drives",
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
        let Target::Folder { path } = &destination.target;
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
            text("Save captures and recordings to a folder on this computer or a mounted drive.")
                .size(style::SMALL)
                .color(p.secondary)
        ]
        .spacing(14);
        if let Some((message, error)) = &self.notice {
            body = body.push(status(
                message.clone(),
                if *error { Status::Failed } else { Status::Done },
                0,
                p,
            ));
        }
        let frame = |body| sheet_frame("Destinations", Message::CloseManager, body, p);
        if self.saved.is_empty() {
            return frame(body.push(choice(
                Icon::Folder,
                "Folder or drive",
                "Choose a folder on this computer or a mounted drive",
                Message::AddFolder,
                p,
            )));
        }
        let rows = self.saved.iter().fold(column![].spacing(2), |rows, d| {
            rows.push(self.row(d, p, now))
        });
        frame(body.push(rows)).footer(
            button(
                row![
                    icon(Icon::FolderPlus, 14.0, p.text),
                    text("Add folder…").size(style::BODY)
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            )
            .name("Add folder…")
            .padding([7, 12])
            .style(style::secondary)
            .on_press(Message::AddFolder),
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
            let edit = button(text("Edit…").size(style::SMALL))
                .name(format!("Edit destination {name}"))
                .description(destination.describe())
                .style(style::link)
                .on_press(Message::Edit(name));
            (edit, remove)
        };
        let description = destination.describe();
        let info = column![
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
        let glow = self.saved_glow.level(destination.name.as_str(), now);
        container(
            row![
                icon(Icon::Folder, 15.0, p.accent,),
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
        _now: Instant,
    ) -> SheetFrame<'a, Message> {
        let editable = !self.mutation_busy();
        let saving = editor.saving.is_some();
        let can_save = editable;
        let field = |label: &'a str, control: Element<'a, Message>| -> Element<'a, Message> {
            row![
                text(label)
                    .size(style::BODY)
                    .color(p.secondary)
                    .width(Length::Fixed(LABEL)),
                control
            ]
            .spacing(12)
            .align_y(Alignment::Center)
            .into()
        };
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
                })
                .invalid(editor.refusal(kind).map(str::to_owned))
                .on_input_maybe(editable.then_some(on))
                .on_submit_maybe(can_save.then_some(Message::Save))
                .size(style::BODY)
                .padding([6, 10])
                .style(look)
        };
        let name = checked(
            Field::Name,
            field(
                "Name",
                super::tip(
                    input("archive", &editor.name, Message::Name, Field::Name).id(NAME_FIELD),
                    "Letters, digits, '.', '-' and '_'. Used with --destination on the command line.",
                ),
            ),
        );
        let folder = checked(
            Field::Folder,
            field(
                "Folder",
                row![
                    input(
                        "Choose a folder",
                        &editor.folder,
                        Message::Folder,
                        Field::Folder
                    ),
                    button(text("Choose…").size(style::BODY))
                        .style(style::secondary)
                        .padding([7, 10])
                        .on_press_maybe(editable.then_some(Message::BrowseFolder))
                ]
                .spacing(8)
                .into(),
            ),
        );
        let status_visible =
            !saving && editor.testing.is_none() && matches!(editor.message, Some((_, true)));
        let full_status: Element<'a, Message> =
            if status_visible && let Some((message, _)) = &editor.message {
                status(message.clone(), Status::Failed, spin, p)
            } else {
                space().into()
            };
        let form = column![name, folder].spacing(10);
        let testing = editor.testing.is_some();
        let outcome = if saving {
            compact_status("Saving…".into(), Status::Working, spin, p)
        } else if testing {
            compact_status("Checking the folder…".into(), Status::Working, spin, p)
        } else if let Some((message, error)) = &editor.message {
            super::tip(
                compact_status(
                    message.clone(),
                    if *error { Status::Failed } else { Status::Done },
                    spin,
                    p,
                ),
                message,
            )
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
        if !editor.history.undo.is_empty() || !editor.history.redo.is_empty() {
            footer = footer.extend(history_buttons(self.undo_label(), self.redo_label()));
        }
        let footer = footer
            .push(super::tip(
                button(text("Test").size(style::BODY))
                    .padding([7, 14])
                    .style(style::secondary)
                    .on_press_maybe((!testing && editable).then_some(Message::Test)),
                "Check that the folder can be written to",
            ))
            .push(container(outcome).width(Fill).padding([0, 4]))
            .push(first)
            .push(second)
            .spacing(8)
            .align_y(Alignment::Center);
        let title = editor
            .original
            .as_ref()
            .map_or_else(|| "New folder".into(), |n| format!("Edit {n}"));
        sheet_frame(
            title,
            Message::CloseManager,
            column![form, full_status].spacing(if status_visible { 12.0 } else { 0.0 }),
            p,
        )
        .back(Some(Message::Cancel))
        .back_enabled(editable)
        .close_enabled(editable)
        .default_action(can_save.then_some(Message::Save))
        .footer(footer)
    }
    pub(super) fn scene(&mut self, word: &str, output: &mut String) -> bool {
        match word {
            "capture-custom" => {
                self.custom_naming = true;
                return true;
            }
            "destinations-list" | "destinations-remove" => {
                for (name, path) in [
                    ("archive", "/Volumes/Archive/Capturefab"),
                    ("line-1", "/Volumes/Line 1/Capturefab"),
                ] {
                    let _ = destination::save(Destination {
                        name: name.into(),
                        target: Target::Folder { path: path.into() },
                    });
                }
                self.saved = destination::list().unwrap_or_default();
                self.notice = Some(("Saved line-1".into(), false));
                if word == "destinations-remove" {
                    self.removing = Some("archive".into());
                    self.notice = None;
                }
            }
            "folder-editor" | "folder-error" => {
                let mut editor =
                    Editor::new_folder("archive".into(), "/Volumes/Archive/Capturefab".into());
                if word == "folder-error" {
                    editor.message = Some(("This folder is not writable. Check its permissions or choose another folder.".into(), true));
                }
                self.open_editor(editor);
            }
            "testing" => {
                if let Some(e) = &mut self.editor {
                    let (sender, receiver) = channel();
                    std::mem::forget(sender);
                    e.message = None;
                    e.testing = Some(receiver);
                }
            }
            _ => return false,
        }
        let _ = output;
        self.manager_open = true;
        true
    }
}
const LABEL: f32 = 110.0;
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
pub(super) fn home_short(path: &Path) -> String {
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

fn row_summary(destination: &Destination) -> String {
    let Target::Folder { path } = &destination.target;
    home_short(path)
}
fn folder_encoding_error() -> &'static str {
    "A folder destination needs a UTF-8 path; choose a folder without invalid text bytes."
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn draft_undo_groups_typing_and_restores_folder_changes() {
        let mut picker = Picker::new(false, false);
        let mut output = String::new();
        picker.update(Message::AddFolder, &mut output);
        picker.update(Message::Name("cam".into()), &mut output);
        picker.update(Message::Name("camera".into()), &mut output);
        assert_eq!(picker.editor.as_ref().unwrap().history.undo.len(), 1);
        assert_eq!(picker.undo_label(), Some("Destination name"));
        assert!(picker.undo());
        assert!(picker.editor.as_ref().unwrap().name.is_empty());
        assert!(picker.redo());
        assert_eq!(picker.editor.as_ref().unwrap().name, "camera");
        picker.update(Message::Folder("/tmp/captures".into()), &mut output);
        assert!(picker.undo());
        assert!(picker.editor.as_ref().unwrap().folder.is_empty());
        assert!(picker.redo());
        assert_eq!(picker.editor.as_ref().unwrap().folder, "/tmp/captures");
    }
    #[test]
    fn pending_save_blocks_edits_and_only_success_creates_global_history() {
        let mut picker = Picker::new(false, false);
        let initial = std::env::temp_dir()
            .join("capture.png")
            .to_string_lossy()
            .into_owned();
        let mut output = initial.clone();
        let mut editor = Editor::new_folder("line-1".into(), std::env::temp_dir().join("captures"));
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
        sender.send(Ok(())).unwrap();
        picker.tick(&mut output);
        let changes = picker.take_changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].label(), "Add destination line-1");
        assert_eq!(changes[0].before_selection.output, initial);
        assert_eq!(changes[0].after_selection.output, "capture.png");
        assert!(!picker.mutation_busy());
        assert!(picker.take_changes().is_empty());
    }
    #[test]
    fn every_folder_refusal_lands_on_its_field() {
        let e = Editor::new_folder(String::new(), PathBuf::from("relative"));
        let fields = e.problems().iter().map(|(f, _)| *f).collect::<Vec<_>>();
        assert!(fields.contains(&Field::Name));
        assert!(fields.contains(&Field::Folder));
    }
    #[test]
    fn choosing_a_destination_keeps_template_folders_below_capture_root() {
        let mut picker = Picker::new(false, false);
        let mut output = super::super::prefs::default_capture_output();
        picker.select(Some("archive".into()), &mut output);
        assert_eq!(
            Path::new(&output),
            Path::new("{date}").join("{camera}-{datetime}-{seq}.png")
        );
    }
    #[test]
    fn a_saved_row_glows_once_and_settles() {
        let mut picker = Picker::new(false, false);
        let start = Instant::now();
        picker.saved_glow.hit("archive".into(), start);
        let animating = |picker: &Picker, at| picker.flashes().any(|f| f.animating(at));
        assert!(animating(&picker, start + Duration::from_millis(1)));
        assert!(picker.saved_glow.level("archive", start + SAVED_GLOW / 2) > 0.0);
        assert_eq!(picker.saved_glow.level("line-1", start), 0.0);
        assert!(!animating(&picker, start + SAVED_GLOW));
    }
}
