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

fn summary(status: Option<&DatasetStatus>) -> String {
    match status {
        None => "Not cataloging captures".into(),
        Some(s) => {
            let mut parts = vec![
                file_name(&s.path),
                format!("{} captures", short(s.captures)),
            ];
            if s.samples > 0 {
                parts.push(format!("{} samples", short(s.samples)));
            }
            parts.join(" · ")
        }
    }
}

/// Start or stop cataloging into the automatic dataset, for captures and
/// signal samples alike, while no dataset is open.
fn auto_catalog(handle: &SessionHandle, on: bool) {
    let slot = handle.dataset();
    slot.set_auto_path(on.then(auto_path));
    let sink: Option<Arc<dyn crate::signals::SampleSink>> = on.then(|| slot.clone() as _);
    if on != handle.signals().has_sink()
        && let Err(error) = handle.signals().set_sink(sink, Duration::from_secs(10))
    {
        eprintln!("capturefab: signal samples will not reach the automatic dataset: {error:#}");
    }
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
                handle.signals(),
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
                if self.handle.dataset().status_cached().is_none() {
                    auto_catalog(&self.handle, on);
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
            }
            (Job::DatasetExport, Ok(result)) => {
                if let Some(output) = result["output"].as_str() {
                    let _ = reveal::reveal(&Path::new(output).join("README.md"));
                }
            }
            (Job::DatasetOpen | Job::DatasetClose | Job::DatasetExport, Err(error)) => {
                self.dataset.error = Some(format!("{error:#}"));
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
    let mut section = column![widgets::disclosure(
        "Dataset",
        Some(summary(status)),
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
            container(text(label).size(style::BODY).color(p.secondary)).width(Fill),
            text(value).size(style::BODY),
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
                    "Signals",
                    format!("{} tags · {} samples", s.signals, short(s.samples)),
                ))
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
                .spacing(6),
            );
        }
        None => {
            section = section
                .push(
                    text(
                        "A dataset records every capture, recording and signal sample in one \
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
    if status.is_none() && bench.handle.dataset().auto_path().is_some() {
        section = section.push(
            text(format!(
                "Creates {} on the first save.",
                auto_path().display()
            ))
            .size(style::CAPTION)
            .color(p.secondary),
        );
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
    fn summary_reads_short() {
        assert_eq!(summary(None), "Not cataloging captures");
        let status = DatasetStatus {
            path: "/d/plant.duckdb".into(),
            captures: 120,
            samples: 86_400,
            ..Default::default()
        };
        assert_eq!(
            summary(Some(&status)),
            "plant.duckdb · 120 captures · 86k samples"
        );
        assert_eq!(short(1_500_000), "1.5M");
    }
}
