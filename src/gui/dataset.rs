//! The Dataset section: the session's catalog file, its counts, and New,
//! Open, Close, Export and Reveal actions (Slice B).
use super::*;
use crate::dataset::{DatasetStatus, OpenMode};
use iced::widget::column;
use std::path::{Path, PathBuf};

/// What the dataset section remembers between frames.
#[derive(Debug, Default)]
pub(super) struct DatasetUi {
    /// The dataset opened here, reopened at the next start.
    pub(super) path: Option<PathBuf>,
    /// Catalog captures in `capturefab.duckdb` under the capture folder
    /// when no dataset is open.
    pub(super) auto: bool,
    /// Why the last dataset action failed.
    pub(super) error: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) enum Msg {
    New,
    Open,
    Picked { path: Option<PathBuf>, create: bool },
    Close,
    Export,
    ExportPicked(Option<PathBuf>),
    Reveal,
    Auto(bool),
}

/// The automatic dataset, beside the captures.
pub(super) fn auto_path() -> PathBuf {
    prefs::capture_root().join("capturefab.duckdb")
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_owned())
}

/// 86400 as "86k", for the summary line.
fn short(count: u64) -> String {
    match count {
        0..1_000 => count.to_string(),
        1_000..1_000_000 => format!("{}k", count / 1_000),
        _ => format!("{:.1}M", count as f64 / 1_000_000.0),
    }
}

/// Whether the automatic dataset is waiting for the first save, or why it
/// could not be opened.
#[derive(Debug, Clone, PartialEq)]
enum Auto {
    Off,
    Armed,
    Failed(String),
}
impl Auto {
    fn of(handle: &SessionHandle) -> Self {
        let slot = handle.dataset();
        match (slot.auto_path(), slot.auto_error()) {
            (_, Some(error)) => Auto::Failed(error),
            (Some(_), None) => Auto::Armed,
            (None, None) => Auto::Off,
        }
    }
}

/// The dataset file captures are cataloged in, or will be on the first
/// save; `None` when nothing catalogs them.
pub(super) fn catalog(bench: &Workbench) -> Option<String> {
    match (bench.snapshot.dataset.as_ref(), Auto::of(&bench.handle)) {
        (Some(status), _) => Some(file_name(&status.path)),
        (None, Auto::Armed) => Some(file_name(&auto_path().to_string_lossy())),
        _ => None,
    }
}

fn summary(status: Option<&DatasetStatus>, auto: &Auto) -> String {
    match status {
        None => match auto {
            Auto::Off => "Not cataloging captures".into(),
            Auto::Armed => "Automatic · created on the first save".into(),
            Auto::Failed(_) => "Automatic dataset unavailable".into(),
        },
        Some(s) => {
            format!(
                "{} · {} captures · {} recordings",
                file_name(&s.path),
                short(s.captures),
                short(s.recordings)
            )
        }
    }
}

/// Start or stop cataloging camera captures into the automatic dataset.
fn auto_catalog(handle: &SessionHandle, on: bool) {
    handle.dataset().set_auto_path(on.then(auto_path));
}

impl DatasetUi {
    /// Restore the remembered dataset and the automatic one. Without saved
    /// preferences (tests, screenshots) nothing is opened or created.
    pub(super) fn start(handle: &SessionHandle, saved: Option<&Prefs>) -> Self {
        let Some(prefs) = saved else {
            return Self::default();
        };
        let mut ui = Self {
            path: prefs
                .dataset_path
                .clone()
                .filter(|p| p.is_absolute() && p.extension().is_some_and(|e| e == "duckdb")),
            auto: prefs.dataset_auto,
            error: None,
        };
        // `gui --dataset` already opened one; it wins over the preference.
        if handle.dataset().status_cached().is_none()
            && let Some(path) = ui.path.clone()
        {
            let open = crate::dataset::coordinator::run(
                handle.dataset(),
                SessionCommand::DatasetOpen {
                    path: path.to_string_lossy().into_owned(),
                    mode: OpenMode::Open,
                    max_bytes: None,
                },
            );
            if let Err(error) = open {
                let text = format!("{error:#}");
                // A moved or foreign file is forgotten; a locked one is kept.
                if text.contains("does not exist") || text.contains("not a Capturefab dataset") {
                    ui.path = None;
                }
                ui.error = Some(text);
            }
        }
        if handle.dataset().status_cached().is_none() && ui.auto {
            auto_catalog(handle, true);
        }
        ui
    }
}

impl Workbench {
    pub(super) fn dataset_update(&mut self, message: Msg) -> Task<Message> {
        match message {
            Msg::New | Msg::Open => {
                let create = matches!(message, Msg::New);
                let directory = self
                    .dataset
                    .path
                    .as_deref()
                    .and_then(Path::parent)
                    .map(Path::to_owned)
                    .unwrap_or_else(prefs::capture_root);
                return Task::perform(
                    async move {
                        let dialog = rfd::AsyncFileDialog::new()
                            .add_filter("Dataset", &["duckdb"])
                            .set_directory(directory)
                            .set_title(if create {
                                "New dataset"
                            } else {
                                "Open dataset"
                            });
                        let file = if create {
                            dialog
                                .set_file_name(format!(
                                    "dataset-{}.duckdb",
                                    chrono::Local::now().format("%Y%m%d")
                                ))
                                .save_file()
                                .await
                        } else {
                            dialog.pick_file().await
                        };
                        Msg::Picked {
                            path: file.map(|file| {
                                let mut path = file.path().to_owned();
                                if path.extension().is_none() {
                                    path.set_extension("duckdb");
                                }
                                path
                            }),
                            create,
                        }
                    },
                    Message::Dataset,
                );
            }
            Msg::Picked { path: None, .. } | Msg::ExportPicked(None) => {}
            Msg::Picked {
                path: Some(path),
                create,
            } => {
                self.dataset.error = None;
                self.send(
                    Job::DatasetOpen,
                    SessionCommand::DatasetOpen {
                        path: path.to_string_lossy().into_owned(),
                        // A save panel already confirmed replacing nothing:
                        // creating refuses an existing file.
                        mode: if create {
                            OpenMode::Create
                        } else {
                            OpenMode::Open
                        },
                        max_bytes: None,
                    },
                );
            }
            Msg::Close => {
                self.dataset.error = None;
                self.send(Job::DatasetClose, SessionCommand::DatasetClose);
            }
            Msg::Export => {
                let directory = self
                    .snapshot
                    .dataset
                    .as_ref()
                    .and_then(|s| Path::new(&s.path).parent().map(Path::to_owned))
                    .unwrap_or_else(prefs::capture_root);
                return Task::perform(
                    async move {
                        Msg::ExportPicked(
                            rfd::AsyncFileDialog::new()
                                .set_directory(directory)
                                .set_title("Export dataset into folder")
                                .pick_folder()
                                .await
                                .map(|folder| folder.path().to_owned()),
                        )
                    },
                    Message::Dataset,
                );
            }
            Msg::ExportPicked(Some(folder)) => {
                let name = self
                    .snapshot
                    .dataset
                    .as_ref()
                    .map(|s| s.name.clone())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| "dataset".into());
                let output = folder.join(format!(
                    "{name}-export-{}",
                    chrono::Local::now().format("%Y%m%d-%H%M%S")
                ));
                self.dataset.error = None;
                self.send(
                    Job::DatasetExport,
                    SessionCommand::DatasetExport {
                        output: output.to_string_lossy().into_owned(),
                        formats: Vec::new(),
                        tables: Vec::new(),
                        since_ms: None,
                        until_ms: None,
                        storage: crate::storage::configured_policy().unwrap_or_default(),
                    },
                );
            }
            Msg::Reveal => {
                if let Some(status) = &self.snapshot.dataset
                    && let Err(error) = reveal::reveal(Path::new(&status.path))
                {
                    self.set_notice(
                        "Could not show the dataset",
                        Some(error.to_string()),
                        Level::Error,
                    );
                }
            }
            Msg::Auto(on) => {
                self.dataset.auto = on;
                let open = self.handle.dataset().status_cached();
                if open.is_none() {
                    auto_catalog(&self.handle, on);
                } else if !on
                    && open.is_some_and(|s| Path::new(&s.path) == auto_path())
                    && self.dataset.path.is_none()
                {
                    // Unticking stops cataloging into the automatic dataset
                    // it opened, rather than leaving it open and recording.
                    self.dataset.error = None;
                    self.send(Job::DatasetClose, SessionCommand::DatasetClose);
                }
            }
        }
        Task::none()
    }

    /// Keep the remembered dataset in step with what the session did.
    pub(super) fn dataset_settled(
        &mut self,
        pending: &Pending,
        result: &Result<serde_json::Value>,
    ) {
        match (&pending.job, result) {
            (Job::DatasetOpen, Ok(status)) => {
                self.dataset.path = status["path"].as_str().map(PathBuf::from);
                self.dataset.error = None;
            }
            (Job::DatasetClose, Ok(_)) => {
                self.dataset.path = None;
                self.dataset.error = None;
                // Closing clears the automatic dataset; while the box stays
                // ticked, arm it again so the checkbox tells the truth.
                if self.dataset.auto {
                    auto_catalog(&self.handle, true);
                }
            }
            (Job::DatasetExport, Ok(result)) => {
                if let Some(output) = result["output"].as_str() {
                    let _ = reveal::reveal(&Path::new(output).join("README.md"));
                }
            }
            (Job::DatasetOpen | Job::DatasetClose | Job::DatasetExport, Err(error)) => {
                self.dataset.error = Some(format!("{error:#}"));
                // A failed open cleared the automatic dataset; re-arm it.
                if matches!(pending.job, Job::DatasetOpen)
                    && self.dataset.auto
                    && self.handle.dataset().status_cached().is_none()
                {
                    auto_catalog(&self.handle, true);
                }
            }
            _ => {}
        }
    }
}

/// The inspector's section, or `None` to leave it out.
pub(super) fn view<'a>(
    bench: &'a Workbench,
    open: bool,
    p: &'static Palette,
) -> Option<Element<'a, Message>> {
    let status = bench.snapshot.dataset.as_ref();
    let auto = Auto::of(&bench.handle);
    let mut section = column![widgets::disclosure(
        "Dataset",
        Some(summary(status, &auto)),
        if open { 1.0 } else { 0.0 },
        Message::InspectorDisclosure(inspector::Section::Dataset),
        p,
    )]
    .spacing(inspector::ROW_GAP);
    if !open {
        return Some(section.into());
    }
    let busy = bench.pending(Job::DatasetOpen)
        || bench.pending(Job::DatasetClose)
        || bench.pending(Job::DatasetExport);
    let row_of = |label: &'a str, value: String| -> Element<'a, Message> {
        row![
            container(text(label).size(style::BODY).color(p.secondary))
                .width(Length::FillPortion(1)),
            text(value)
                .size(style::BODY)
                .width(Length::FillPortion(2))
                .align_x(Alignment::End)
                .wrapping(text::Wrapping::WordOrGlyph),
        ]
        .spacing(6)
        .into()
    };
    let action = |glyph: Icon, label: &'a str, on: Msg| -> Element<'a, Message> {
        button(
            row![
                icon(glyph, 13.0, p.text),
                text(label).size(style::BODY).font(style::MEDIUM)
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .padding([6, 10])
        .style(style::secondary)
        .on_press_maybe((!busy).then_some(Message::Dataset(on)))
        .into()
    };
    match status {
        Some(s) => {
            section = section
                .push(row_of("File", file_name(&s.path)))
                .push(row_of("Captures", s.captures.to_string()))
                .push(row_of("Recordings", s.recordings.to_string()))
                .push(row_of(
                    "Last saved",
                    s.last_saved
                        .as_deref()
                        .map(file_name)
                        .unwrap_or_else(|| "Nothing yet".into()),
                ));
            if s.missed_files > 0 {
                section = section.push(
                    text(format!(
                        "{} saved files were not cataloged; see the events table.",
                        s.missed_files
                    ))
                    .size(style::CAPTION)
                    .color(p.ink(p.warn)),
                );
            }
            if let Some(error) = &s.last_error {
                section = section.push(
                    text(error.clone())
                        .size(style::CAPTION)
                        .color(p.ink(p.danger)),
                );
            }
            section = section.push(
                row![
                    action(Icon::FolderPlus, "Export…", Msg::Export),
                    action(Icon::Folder, reveal::label(), Msg::Reveal),
                    action(Icon::Close, "Close", Msg::Close),
                ]
                .spacing(6)
                .wrap()
                .vertical_spacing(6),
            );
        }
        None => {
            section = section
                .push(
                    text(
                        "A dataset records every capture and recording in one \
                         .duckdb file, ready to export for analysis.",
                    )
                    .size(style::SMALL)
                    .color(p.secondary),
                )
                .push(
                    row![
                        action(Icon::Plus, "New…", Msg::New),
                        action(Icon::Folder, "Open…", Msg::Open),
                    ]
                    .spacing(6),
                );
        }
    }
    section = section.push(checkbox(
        "Catalog captures automatically",
        bench.dataset.auto,
        |on| Message::Dataset(Msg::Auto(on)),
    ));
    match (status, &auto) {
        (None, Auto::Armed) => {
            section = section.push(
                text(format!(
                    "Creates {} on the first save.",
                    auto_path().display()
                ))
                .size(style::CAPTION)
                .color(p.secondary),
            );
        }
        (None, Auto::Failed(error)) if bench.dataset.auto => {
            section = section.push(
                text(format!(
                    "Captures are not being cataloged: {} could not be opened ({error}). \
                     Untick and tick the box to retry, or choose New… or Open….",
                    auto_path().display()
                ))
                .size(style::CAPTION)
                .color(p.ink(p.danger)),
            );
        }
        _ => {}
    }
    if let Some(error) = &bench.dataset.error {
        section = section.push(
            text(error.clone())
                .size(style::CAPTION)
                .color(p.ink(p.danger)),
        );
    }
    Some(section.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_values_and_actions_fit_the_compact_inspector() {
        use iced::advanced::{Layout, layout, widget::Tree};
        let mut renderer = destinations::layout_tests::renderer();
        for dark in [false, true] {
            let mut bench =
                Workbench::new(SessionHandle::new(), "dataset-layout".into(), true, None);
            let name = format!("{}.duckdb", "production-line-capture-history-".repeat(5));
            bench.snapshot.dataset = Some(DatasetStatus {
                path: format!("/captures/{name}"),
                captures: u64::MAX,
                recordings: u64::MAX,
                last_saved: Some(format!("/captures/{}-00001.png", "camera".repeat(20))),
                ..Default::default()
            });
            let size = Size::new(INSPECTOR - 32.0, 1200.0);
            // The real sidebar's panel_body supplies the inherited text color.
            // Keep that ancestor when rendering this section in isolation.
            let mut view: Element<'_, Message> =
                container(super::view(&bench, true, Palette::of(dark)).unwrap())
                    .style(style::base)
                    .into();
            let mut tree = Tree::new(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, size),
            );
            let (snapshot, _, _) = accessibility::snapshot_widget(
                view.as_widget_mut(),
                &mut tree,
                Layout::new(&node),
                &renderer,
            );
            for label in [
                "File",
                "Captures",
                "Recordings",
                "Last saved",
                "Export…",
                reveal::label(),
                "Close",
            ] {
                let control = snapshot
                    .nodes
                    .iter()
                    .find(|(_, node)| node.label() == Some(label))
                    .unwrap_or_else(|| panic!("missing {label}"));
                let bounds = control.1.bounds().unwrap();
                assert!(
                    bounds.width() > 0.0
                        && bounds.height() > 0.0
                        && bounds.x0 >= 0.0
                        && bounds.x1 <= f64::from(size.width) + 0.5,
                    "{label} must remain readable inside the inspector: {bounds:?}"
                );
            }
            destinations::layout_tests::render_artifact_themed(
                if dark {
                    "dataset-compact-dark"
                } else {
                    "dataset-compact-light"
                },
                &view,
                &tree,
                &node,
                &mut renderer,
                size,
                (dark, 1.3),
            );
        }
    }

    #[test]
    fn summary_reads_short() {
        assert_eq!(summary(None, &Auto::Off), "Not cataloging captures");
        assert_eq!(
            summary(None, &Auto::Armed),
            "Automatic · created on the first save"
        );
        assert_eq!(
            summary(None, &Auto::Failed("locked".into())),
            "Automatic dataset unavailable"
        );
        let status = DatasetStatus {
            path: "/d/plant.duckdb".into(),
            captures: 120,
            recordings: 12,
            ..Default::default()
        };
        assert_eq!(
            summary(Some(&status), &Auto::Armed),
            "plant.duckdb · 120 captures · 12 recordings"
        );
        assert_eq!(short(1_500_000), "1.5M");
    }
}
