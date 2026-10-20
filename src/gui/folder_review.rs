//! Review a folder of images and videos: the most interesting item first,
//! kept or discarded by swipe, button or arrow key, as auto capture reviews
//! suggestions. One worker thread owns the review session, so scanning,
//! decoding and DuckDB never run on the UI thread.
use super::autocapture::{key_decision, swipe_decision};
use super::*;
use crate::folder_review::{
    ExportSummary, Item, Options, Progress, Rank, ReviewSession, ScanSummary, Status,
};
use anyhow::Context as _;
use iced::widget::{column, image};
use iced::{ContentFit, Point};
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Debug)]
pub(super) enum Input {
    Open(bool),
    ChooseFolder,
    ChooseSession,
    Folder(Option<PathBuf>),
    Session(Option<PathBuf>),
    Scan,
    Stop,
    All(bool),
    Rank(Rank),
    Decide(bool),
    Export,
    ExportTo(Option<PathBuf>),
    Reveal,
    Retry,
    Pointer(Point),
    DragStart,
    DragEnd,
    DragCancel,
}

fn input(input: Input) -> Message {
    Message::FolderReview(input)
}

enum Request {
    Folder(PathBuf),
    Session(PathBuf),
    Scan,
    Next { all: bool },
    Decide { id: u64, keep: bool, all: bool },
    Rank { rank: Rank, all: bool },
    Export(PathBuf),
}

/// An item ready to show: decoded and scaled on the worker.
struct Shown {
    item: Item,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

enum Reply {
    Opened(Status),
    Scanned(ScanSummary, Status),
    Next(Option<Shown>, Status),
    Exported(ExportSummary, Status),
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Busy {
    Opening,
    Scanning,
    Loading,
    Saving,
    Exporting,
}

struct Worker {
    requests: mpsc::Sender<Request>,
    replies: mpsc::Receiver<Reply>,
    stop: Arc<AtomicBool>,
    progress: Arc<Mutex<Option<Progress>>>,
}

impl Worker {
    fn spawn() -> Result<Self> {
        let (requests, inbox) = mpsc::channel();
        let (outbox, replies) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(None));
        let (halt, shared) = (stop.clone(), progress.clone());
        std::thread::Builder::new()
            .name("capturefab-folder-review".into())
            .spawn(move || serve(inbox, outbox, halt, shared))?;
        Ok(Self {
            requests,
            replies,
            stop,
            progress,
        })
    }
}

impl Drop for Worker {
    /// A scan in progress stops at its next file; the thread then closes
    /// the session once it sees the channel close.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The worker thread: requests run in order on the one open session.
fn serve(
    inbox: mpsc::Receiver<Request>,
    outbox: mpsc::Sender<Reply>,
    stop: Arc<AtomicBool>,
    progress: Arc<Mutex<Option<Progress>>>,
) {
    let mut session: Option<ReviewSession> = None;
    for request in inbox {
        let reply = (|| -> Result<Reply> {
            match request {
                Request::Folder(folder) => {
                    let path = managed_session(&folder)?;
                    let opened = if path.exists() {
                        ReviewSession::open(&path)?
                    } else {
                        ReviewSession::create(&path, &Options::new(folder))?
                    };
                    let status = opened.status()?;
                    session = Some(opened);
                    Ok(Reply::Opened(status))
                }
                Request::Session(path) => {
                    let opened = ReviewSession::open(&path)?;
                    let status = opened.status()?;
                    session = Some(opened);
                    Ok(Reply::Opened(status))
                }
                request => {
                    let session = session.as_mut().context("open a folder review first")?;
                    match request {
                        Request::Scan => {
                            stop.store(false, Ordering::Relaxed);
                            let summary = session.scan(&stop, &mut |done| {
                                *progress.lock().unwrap_or_else(|e| e.into_inner()) =
                                    Some(done.clone());
                            });
                            progress.lock().unwrap_or_else(|e| e.into_inner()).take();
                            Ok(Reply::Scanned(summary?, session.status()?))
                        }
                        Request::Next { all } => next(session, all),
                        Request::Decide { id, keep, all } => {
                            session.review(id, keep)?;
                            next(session, all)
                        }
                        Request::Rank { rank, all } => {
                            session.configure(Some(rank), None)?;
                            next(session, all)
                        }
                        Request::Export(folder) => {
                            let output = folder
                                .to_str()
                                .context("export folder path must be valid UTF-8")?;
                            let target = crate::destination::resolve(None, output)?;
                            let summary = session.export(&target)?;
                            Ok(Reply::Exported(summary, session.status()?))
                        }
                        Request::Folder(_) | Request::Session(_) => unreachable!(),
                    }
                }
            }
        })()
        .unwrap_or_else(|error| Reply::Failed(format!("{error:#}")));
        if outbox.send(reply).is_err() {
            break;
        }
    }
    if let Some(session) = session {
        let _ = session.close();
    }
}

fn next(session: &mut ReviewSession, all: bool) -> Result<Reply> {
    let shown = match session.next(all)? {
        Some(item) => {
            let frame = session.frame(item.id)?;
            let (width, height, rgba) = frame::preview_rgba(&frame, 1280, 960)?;
            Some(Shown {
                item,
                width,
                height,
                rgba,
            })
        }
        None => None,
    };
    Ok(Reply::Next(shown, session.status()?))
}

/// Where the desktop keeps the session for `folder`: in the state
/// directory, named after the folder and its path, so choosing the same
/// folder again resumes its review and the folder itself is never written.
fn managed_session(folder: &Path) -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    let folder = folder.canonicalize()?;
    let directory = crate::ipc::state_dir().join("folder-reviews");
    crate::ipc::ensure_private_dir(&directory)?;
    let name: String = folder
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "folder".into())
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    let hash = Sha256::digest(folder.as_os_str().as_encoded_bytes());
    let hash: String = hash[..6].iter().map(|b| format!("{b:02x}")).collect();
    Ok(directory.join(format!("{name}-{hash}.duckdb")))
}

struct Card {
    item: Item,
    image: image::Handle,
}

#[derive(Default)]
pub(super) struct State {
    pub open: bool,
    worker: Option<Worker>,
    status: Option<Status>,
    card: Option<Card>,
    busy: Option<Busy>,
    dialog_busy: bool,
    error: Option<String>,
    /// The outcome of the last scan or export.
    notice: Option<String>,
    all: bool,
    reviewed: u64,
    progress: Option<Progress>,
    pointer: Option<Point>,
    drag_start: Option<Point>,
}

impl State {
    pub(super) fn busy(&self) -> bool {
        self.busy.is_some() || self.dialog_busy
    }
}

impl Workbench {
    pub(super) fn update_folder_review(&mut self, event: Input) -> Task<Message> {
        let state = &mut self.folder_review;
        match event {
            Input::Open(open) => {
                if open && !state.open && self.sheet_open() {
                    return Task::none();
                }
                let state = &mut self.folder_review;
                state.open = open;
                state.drag_start = None;
                if open {
                    state.error = None;
                    state.reviewed = 0;
                    if state.status.is_some() && state.busy.is_none() {
                        self.request(Request::Next {
                            all: self.folder_review.all,
                        });
                    }
                    return operation::focus("sheet-close");
                }
            }
            Input::ChooseFolder | Input::ChooseSession if !state.busy() => {
                state.dialog_busy = true;
                let folder = matches!(event, Input::ChooseFolder);
                return Task::perform(
                    async move {
                        let dialog = rfd::AsyncFileDialog::new();
                        if folder {
                            let chosen = dialog.set_title("Review a folder").pick_folder().await;
                            Input::Folder(chosen.map(|f| f.path().to_owned()))
                        } else {
                            let chosen = dialog
                                .add_filter("Folder review session", &["duckdb"])
                                .set_title("Open folder review session")
                                .pick_file()
                                .await;
                            Input::Session(chosen.map(|f| f.path().to_owned()))
                        }
                    },
                    input,
                );
            }
            Input::Folder(ref chosen) | Input::Session(ref chosen) => {
                state.dialog_busy = false;
                let folder = matches!(event, Input::Folder(_));
                if let Some(path) = chosen.clone() {
                    let request = if folder {
                        Request::Folder(path)
                    } else {
                        Request::Session(path)
                    };
                    // A new worker per session: closing the old one's
                    // channel checkpoints and closes its session.
                    state.worker = None;
                    state.status = None;
                    state.card = None;
                    state.notice = None;
                    state.error = None;
                    state.reviewed = 0;
                    self.request(request);
                }
            }
            Input::Scan if state.status.is_some() && state.busy.is_none() => {
                state.notice = None;
                self.request(Request::Scan);
            }
            Input::Stop => {
                if let Some(worker) = &state.worker {
                    worker.stop.store(true, Ordering::Relaxed);
                }
            }
            Input::All(all) if state.busy.is_none() => {
                state.all = all;
                if state.status.is_some() && state.card.is_none() {
                    self.request(Request::Next { all });
                }
            }
            Input::Rank(rank) if state.busy.is_none() && state.status.is_some() => {
                let all = state.all;
                self.request(Request::Rank { rank, all });
            }
            Input::Decide(keep) => self.decide_folder_item(keep),
            Input::Export if !state.busy() && state.status.is_some() => {
                state.dialog_busy = true;
                return Task::perform(
                    async {
                        let chosen = rfd::AsyncFileDialog::new()
                            .set_title("Export kept items to")
                            .pick_folder()
                            .await;
                        Input::ExportTo(chosen.map(|f| f.path().to_owned()))
                    },
                    input,
                );
            }
            Input::ExportTo(chosen) => {
                state.dialog_busy = false;
                if let Some(folder) = chosen {
                    state.notice = None;
                    self.request(Request::Export(folder));
                }
            }
            Input::Reveal => {
                if let (Some(status), Some(card)) = (&state.status, &state.card) {
                    let path = Path::new(&status.folder).join(&card.item.path);
                    if let Err(error) = reveal::reveal(&path) {
                        state.error = Some(format!("Could not show {}: {error}", card.item.path));
                    }
                }
            }
            Input::Retry if state.busy.is_none() => {
                state.error = None;
                if state.status.is_some() {
                    let all = state.all;
                    self.request(Request::Next { all });
                }
            }
            Input::Pointer(point) => state.pointer = Some(point),
            Input::DragStart => state.drag_start = state.pointer,
            Input::DragEnd => {
                let start = state.drag_start.take();
                if let Some((start, end)) = start.zip(state.pointer)
                    && let Some(keep) = swipe_decision(start, end)
                {
                    self.decide_folder_item(keep);
                }
            }
            Input::DragCancel => state.drag_start = None,
            _ => {}
        }
        Task::none()
    }

    /// Send `request` to the session's worker, starting one if needed.
    fn request(&mut self, request: Request) {
        let state = &mut self.folder_review;
        if state.worker.is_none() {
            match Worker::spawn() {
                Ok(worker) => state.worker = Some(worker),
                Err(error) => {
                    state.error = Some(format!("Could not start folder review: {error}"));
                    return;
                }
            }
        }
        state.busy = Some(match request {
            Request::Folder(_) | Request::Session(_) => Busy::Opening,
            Request::Scan => Busy::Scanning,
            Request::Next { .. } | Request::Rank { .. } => Busy::Loading,
            Request::Decide { .. } => Busy::Saving,
            Request::Export(_) => Busy::Exporting,
        });
        let sent = state
            .worker
            .as_ref()
            .is_some_and(|worker| worker.requests.send(request).is_ok());
        if !sent {
            state.worker = None;
            state.busy = None;
            state.error = Some("Folder review stopped; choose the folder again".into());
        }
    }

    fn decide_folder_item(&mut self, keep: bool) {
        let state = &self.folder_review;
        if !state.open || state.busy() || self.close_error.is_some() || self.closing.is_some() {
            return;
        }
        if let Some(card) = &state.card {
            let (id, all) = (card.item.id, state.all);
            self.request(Request::Decide { id, keep, all });
        }
    }

    /// Plain review keys never take cursor movement from an editor or popup.
    pub(super) fn folder_review_shortcut(&mut self, chord: Chord, captured: bool) -> bool {
        if !self.folder_review.open
            || self.close_error.is_some()
            || self.focused_window != self.main_window
            || self.popup_open
        {
            return false;
        }
        if let Some(keep) = key_decision(chord, captured, self.menu_editor) {
            self.decide_folder_item(keep);
            return true;
        }
        false
    }

    /// Collect the worker's replies and scan progress.
    pub(super) fn tick_folder_review(&mut self) {
        let state = &mut self.folder_review;
        let Some(worker) = &state.worker else {
            return;
        };
        state.progress = worker
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let reply = match worker.replies.try_recv() {
            Ok(reply) => reply,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                state.worker = None;
                state.busy = None;
                state.error = Some("Folder review stopped; choose the folder again".into());
                return;
            }
        };
        let was = state.busy.take();
        match reply {
            Reply::Opened(status) => {
                state.status = Some(status);
                // Bring the inventory up to date before the first item.
                self.request(Request::Scan);
            }
            Reply::Scanned(summary, status) => {
                state.notice = Some(scan_notice(&summary));
                state.status = Some(status);
                let all = state.all;
                self.request(Request::Next { all });
            }
            Reply::Next(shown, status) => {
                if was == Some(Busy::Saving) {
                    state.reviewed += 1;
                }
                state.status = Some(status);
                state.error = None;
                state.drag_start = None;
                state.card = shown.map(|shown| Card {
                    image: image::Handle::from_rgba(shown.width, shown.height, shown.rgba),
                    item: shown.item,
                });
            }
            Reply::Exported(summary, status) => {
                state.status = Some(status);
                state.notice = Some(export_notice(&summary));
            }
            Reply::Failed(error) => state.error = Some(error),
        }
    }

    /// The menu item: open the sheet, and the folder picker when no folder
    /// is being reviewed yet.
    pub(super) fn show_folder_review(&mut self) -> Task<Message> {
        let opened = self.update_folder_review(Input::Open(true));
        if self.folder_review.open && self.folder_review.status.is_none() {
            return Task::batch([opened, self.update_folder_review(Input::ChooseFolder)]);
        }
        opened
    }

    /// `folder-review` shows the sheet with a synthetic item from the
    /// running camera; nothing is scanned or written.
    pub(super) fn scene_folder_review(&mut self, word: &str, late: bool) -> bool {
        if word != "folder-review" {
            return false;
        }
        if !late {
            return false;
        }
        let Some(frame) = self.selected_frame() else {
            return false;
        };
        let Ok((width, height, rgba)) = frame::preview_rgba(&frame, 1280, 960) else {
            return false;
        };
        let state = &mut self.folder_review;
        state.status = Some(Status {
            path: "/state/folder-reviews/line-1-photos.duckdb".into(),
            folder: "/Volumes/Field/line-1-photos".into(),
            encoder: "dinov2".into(),
            model_fingerprint: None,
            rank: Rank::Novelty,
            sensitivity: 0.5,
            threshold: 0.725,
            video_interval_ms: 2000,
            files: 1_284,
            failed_files: 2,
            missing_files: 0,
            items: 1_960,
            pending: 1_412,
            interesting: 38,
            kept: 17,
            discarded: 9,
            duplicates: 522,
            exported: 0,
            examples: 0,
            examples_from: None,
            scanned_ms: Some(0),
            note: None,
        });
        state.card = Some(Card {
            item: Item {
                id: 412,
                path: "2026-10-12/belt-cam/clip-0031.mp4".into(),
                kind: "video",
                offset_ms: Some(14_000),
                width,
                height,
                score: 0.91,
                reason: "91% novelty against everything reviewed".into(),
            },
            image: image::Handle::from_rgba(width, height, rgba),
        });
        state.notice = Some(
            "Scanned 1,284 files: 1,284 new, 2 could not be read; 522 repeats set aside.".into(),
        );
        state.reviewed = 26;
        state.open = true;
        self.sheets.shown.set(1.0);
        true
    }

    pub(super) fn folder_review_sheet(
        &self,
        p: &'static Palette,
        height: f32,
    ) -> Element<'_, Message> {
        let state = &self.folder_review;
        let ready = state.card.is_some() && !state.busy();
        let (outcome, role) = if let Some(error) = &state.error {
            (
                format!("Folder review failed: {error}"),
                accessibility::Role::Alert,
            )
        } else {
            let text = match state.busy {
                Some(Busy::Opening) => "Opening the folder review…".into(),
                Some(Busy::Scanning) => match &state.progress {
                    Some(progress) => format!(
                        "Scanning {} of {} files · {} items",
                        grouped(progress.done as u64),
                        grouped(progress.total as u64),
                        grouped(progress.items)
                    ),
                    None => "Looking for images and videos…".into(),
                },
                Some(Busy::Loading) => "Ranking the folder…".into(),
                Some(Busy::Saving) => "Saving your decision…".into(),
                Some(Busy::Exporting) => "Exporting kept items…".into(),
                None => match (&state.card, &state.status) {
                    (Some(card), _) => format!(
                        "{}Item {} ready. Swipe or use ← / →.",
                        if state.reviewed > 0 {
                            "Decision saved. "
                        } else {
                            ""
                        },
                        card.item.id
                    ),
                    (None, Some(_)) => "You're caught up.".into(),
                    (None, None) => "Choose a folder of images or videos to review.".into(),
                },
            };
            (text, accessibility::Role::Status)
        };
        let mut body = column![].spacing(14);
        let Some(status) = &state.status else {
            let choose = row![
                button(text("Choose Folder…").size(style::BODY))
                    .id("folder-review-choose")
                    .padding([8, 14])
                    .style(style::primary)
                    .on_press_maybe((!state.busy()).then_some(input(Input::ChooseFolder))),
                button(text("Open Review Session…").size(style::BODY))
                    .padding([8, 14])
                    .style(style::secondary)
                    .on_press_maybe((!state.busy()).then_some(input(Input::ChooseSession))),
            ]
            .spacing(10);
            body = body
                .push(text("Find what is worth a look in a folder of photos or recordings. The most unusual items come first; every keep or discard reranks the rest, and repeats are set aside.").size(style::BODY).color(p.secondary).wrapping(text::Wrapping::WordOrGlyph))
                .push(text("The folder is only read. Decisions are kept in a review session, and Export copies kept items elsewhere.").size(style::SMALL).color(p.secondary).wrapping(text::Wrapping::WordOrGlyph))
                .push(choose)
                .push(outcome_text(outcome, role, state.error.is_some(), p));
            return sheet_frame("Review a folder", input(Input::Open(false)), body, p)
                .mark(Icon::Folder)
                .size((self.width - 56.0).clamp(0.0, 920.0), height)
                .into();
        };
        let idle = state.busy.is_none();
        let rank = segmented(
            "Rank by",
            vec![
                (
                    "Most unusual",
                    idle.then_some(input(Input::Rank(Rank::Novelty))),
                    "Items least like everything reviewed first".into(),
                ),
                (
                    "Like kept items",
                    idle.then_some(input(Input::Rank(Rank::Similar))),
                    "Items most like what you kept first".into(),
                ),
            ],
            usize::from(status.rank == Rank::Similar),
            f32::from(u8::from(status.rank == Rank::Similar)),
        );
        let scan = if state.busy == Some(Busy::Scanning) {
            button(text("Stop").size(style::BODY))
                .padding([6, 12])
                .style(style::secondary)
                .on_press(input(Input::Stop))
        } else {
            button(text("Scan Again").size(style::BODY))
                .padding([6, 12])
                .style(style::secondary)
                .on_press_maybe(idle.then_some(input(Input::Scan)))
        };
        let controls = row![
            container(rank).width(Fill),
            scan,
            button(text("Export Kept…").size(style::BODY))
                .padding([6, 12])
                .style(style::secondary)
                .on_press_maybe(
                    (!state.busy() && status.kept > status.exported)
                        .then_some(input(Input::Export))
                ),
            button(text("Other Folder…").size(style::BODY))
                .padding([6, 12])
                .style(style::secondary)
                .on_press_maybe(idle.then_some(input(Input::ChooseFolder))),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        body = body
            .push(
                text(&status.folder)
                    .size(style::SMALL)
                    .font(style::MEDIUM)
                    .wrapping(text::Wrapping::WordOrGlyph),
            )
            .push(
                text(format!(
                    "{} waiting · {} worth a look · {} kept · {} discarded · {} repeats · {} reviewed now",
                    grouped(status.pending),
                    grouped(status.interesting),
                    grouped(status.kept),
                    grouped(status.discarded),
                    grouped(status.duplicates),
                    grouped(state.reviewed),
                ))
                .size(style::SMALL)
                .color(p.secondary),
            )
            .push(controls)
            .push(checkbox(
                "Also show items below the threshold",
                state.all,
                |all| input(Input::All(all)),
            ))
            .push(outcome_text(outcome, role, state.error.is_some(), p));
        if let Some(notice) = state.notice.as_ref().or(status.note.as_ref()) {
            body = body.push(
                text(notice)
                    .size(style::SMALL)
                    .color(p.secondary)
                    .wrapping(text::Wrapping::WordOrGlyph),
            );
        }
        if let Some(card) = &state.card {
            let swipe = state
                .drag_start
                .zip(state.pointer)
                .and_then(|(start, end)| swipe_decision(start, end));
            let caption = match swipe {
                Some(true) => "Release to keep →",
                Some(false) => "← Release to discard",
                None => "← Discard                         Keep →",
            };
            let mut place = card.item.path.clone();
            if let Some(offset) = card.item.offset_ms {
                place.push_str(&format!(" · at {:.1} s", offset as f64 / 1000.0));
            }
            body = body
                .push(
                    mouse_area(
                        container(accessibility::label_description(
                            image(card.image.clone())
                                .content_fit(ContentFit::Contain)
                                .width(Fill)
                                .height((height - 520.0).clamp(130.0, 480.0))
                                .into(),
                            format!("{} preview", card.item.path),
                            "Preview of the item being reviewed. Use Keep or Discard, or the right or left arrow keys, to record your decision.".into(),
                            accessibility::Role::Image,
                            None,
                        ))
                        .id("folder-review-image")
                        .padding(6)
                        .width(Fill)
                        .style(style::code),
                    )
                    .on_move(|point| input(Input::Pointer(point)))
                    .on_press(input(Input::DragStart))
                    .on_release(input(Input::DragEnd))
                    .on_exit(input(Input::DragCancel)),
                )
                .push(
                    text(caption)
                        .size(style::SMALL)
                        .font(style::MEDIUM)
                        .color(match swipe {
                            Some(true) => p.ink(p.live),
                            Some(false) => p.ink(p.danger),
                            None => p.secondary,
                        }),
                )
                .push(
                    row![
                        text(place)
                            .size(style::SMALL)
                            .font(style::MEDIUM)
                            .width(Fill)
                            .wrapping(text::Wrapping::WordOrGlyph),
                        button(text(reveal::label()).size(style::SMALL))
                            .padding([4, 10])
                            .style(style::secondary)
                            .on_press(input(Input::Reveal)),
                    ]
                    .spacing(10)
                    .align_y(Alignment::Center),
                )
                .push(
                    text(format!("Score {:.2} · {}", card.item.score, card.item.reason))
                        .size(style::SMALL)
                        .color(p.secondary)
                        .wrapping(text::Wrapping::WordOrGlyph),
                );
        } else if idle && state.error.is_none() {
            body = body.push(container(column![
                icon(Icon::Check, 42.0, p.accent_text),
                text("You're caught up").size(style::DISPLAY).font(style::BOLD),
                text(if status.pending > 0 && !state.all {
                    "Nothing else stands out. Show items below the threshold to keep going, or scan again after adding files."
                } else {
                    "Every item is reviewed or set aside as a repeat. Scan again after adding files."
                })
                .size(style::BODY)
                .color(p.secondary),
            ].spacing(14)).width(Fill).padding([32, 16]));
        }
        let mut retry = column![].width(Fill);
        if state.error.is_some() && idle {
            retry = retry.push(
                button(text("Try again").size(style::BODY))
                    .padding([6, 12])
                    .style(style::secondary)
                    .on_press(input(Input::Retry)),
            );
        }
        body = body.push(retry);
        let footer = column![
            row![
                button(
                    center(
                        row![
                            icon(Icon::Close, 20.0, p.ink(p.danger)),
                            text("Discard  ←").size(style::BODY).font(style::BOLD)
                        ]
                        .spacing(9)
                    )
                    .height(Length::Shrink)
                )
                .id("folder-review-discard")
                .name("Discard item, left arrow")
                .padding([13, 18])
                .height(48)
                .width(Fill)
                .style(style::danger)
                .on_press_maybe(ready.then_some(input(Input::Decide(false)))),
                button(
                    center(
                        row![
                            icon(Icon::Check, 20.0, Color::WHITE),
                            text("Keep  →").size(style::BODY).font(style::BOLD)
                        ]
                        .spacing(9)
                    )
                    .height(Length::Shrink)
                )
                .id("folder-review-keep")
                .name("Keep item, right arrow")
                .padding([13, 18])
                .height(48)
                .width(Fill)
                .style(style::primary)
                .on_press_maybe(ready.then_some(input(Input::Decide(true)))),
            ]
            .spacing(14),
            text("Swipe the image or use ← / →. Every decision reranks the folder.")
                .size(style::SMALL)
                .color(p.secondary),
        ]
        .spacing(10);
        sheet_frame("Review a folder", input(Input::Open(false)), body, p)
            .mark(Icon::Folder)
            .footer(footer)
            .size((self.width - 56.0).clamp(0.0, 920.0), height)
            .into()
    }
}

fn outcome_text<'a>(
    outcome: String,
    role: accessibility::Role,
    error: bool,
    p: &'static Palette,
) -> Element<'a, Message> {
    accessibility::label(
        text(outcome.clone())
            .size(style::SMALL)
            .color(if error { p.ink(p.danger) } else { p.secondary })
            .width(Fill)
            .wrapping(text::Wrapping::WordOrGlyph)
            .into(),
        outcome,
        role,
    )
}

fn scan_notice(summary: &ScanSummary) -> String {
    let mut notice = format!(
        "Scanned {} files: {} new, {} changed, {} removed",
        grouped(summary.files),
        grouped(summary.added),
        grouped(summary.changed),
        grouped(summary.missing)
    );
    if summary.failed > 0 {
        notice.push_str(&format!(", {} could not be read", grouped(summary.failed)));
        if let Some(first) = summary.errors.first() {
            notice.push_str(&format!(" ({}: {})", first.path, first.error));
        }
    }
    notice.push_str(&format!(
        "; {} repeats set aside.",
        grouped(summary.duplicates)
    ));
    if summary.limited {
        notice.push_str(" The review holds at most 20,000 items; review a subfolder for the rest.");
    }
    if summary.stopped {
        notice.push_str(" Stopped early; scan again to finish.");
    }
    notice
}

fn export_notice(summary: &ExportSummary) -> String {
    let mut notice = format!(
        "Exported {} kept items to {}.",
        grouped(summary.exported.len() as u64),
        summary.output
    );
    if !summary.skipped.is_empty() {
        notice.push_str(&format!(
            " {} skipped because their files changed.",
            grouped(summary.skipped.len() as u64)
        ));
    }
    notice
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_sessions_are_named_after_their_folder_and_path() {
        let root =
            std::env::temp_dir().join(format!("capturefab-gui-review-{}", std::process::id()));
        let (a, b) = (root.join("a/Photos 1"), root.join("b/Photos 1"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let (first, second) = (managed_session(&a).unwrap(), managed_session(&b).unwrap());
        assert_ne!(first, second);
        assert_eq!(first, managed_session(&a).unwrap());
        let name = first.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("Photos_1-") && name.ends_with(".duckdb"),
            "{name}"
        );
        assert!(first.starts_with(crate::ipc::state_dir()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn notices_say_what_a_scan_and_an_export_did() {
        let summary = ScanSummary {
            files: 1200,
            added: 1200,
            failed: 1,
            duplicates: 40,
            stopped: true,
            errors: vec![crate::folder_review::FileError {
                path: "a.png".into(),
                error: "damaged".into(),
            }],
            ..ScanSummary::default()
        };
        assert_eq!(
            scan_notice(&summary),
            "Scanned 1,200 files: 1,200 new, 0 changed, 0 removed, 1 could not be read (a.png: damaged); 40 repeats set aside. Stopped early; scan again to finish."
        );
        let export = ExportSummary {
            output: "/out".into(),
            ..ExportSummary::default()
        };
        assert_eq!(export_notice(&export), "Exported 0 kept items to /out.");
    }
}
