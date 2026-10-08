//! The settings panel: features, auto exposure, capture, forwarding and storage.
use super::*;
use iced::widget::column;

/// The inspector package's own state. Add fields here, register their motions
/// below and point them in `sync_inspector`.
#[derive(Default)]
pub(super) struct InspectorState {
    /// The last write or command that failed on each feature, as the whole
    /// error; show its first line at the row. Kept by `failed()` and cleared
    /// by the feature's next success, a new draft, a refresh or another
    /// camera.
    pub(super) write_errors: HashMap<String, String>,
}

impl InspectorState {
    super::motion::registry! {
        motions: [],
        flashes: [],
    }
}

/// The inspector package's hooks into the shared update cycle; empty until it
/// needs them.
impl Workbench {
    /// Point the inspector package's motions at what they show; from
    /// `sync_animations`.
    pub(super) fn sync_inspector(&mut self) {}

    /// The inspector package's bookkeeping on the slow tick, after the snapshot
    /// refresh; from `tick()`.
    pub(super) fn tick_inspector(&mut self) {}

    /// A command finished, after the shared bookkeeping (`finished`,
    /// `failed`) and before its notice; from `settle()`.
    pub(super) fn result_inspector(
        &mut self,
        _pending: &Pending,
        _result: &anyhow::Result<serde_json::Value>,
    ) {
    }

    /// Take a screenshot scene word the inspector package owns: `late` is false
    /// while the scene is set up and true once its cameras stream. Returns
    /// whether the word was taken; see `apply_scene`.
    pub(super) fn scene_inspector(&mut self, _word: &str, _late: bool) -> bool {
        false
    }
}

impl Workbench {
    pub(super) fn inspector(&self, p: &'static Palette) -> Element<'_, Message> {
        let os = Os::CURRENT;
        let tabs = container(
            row![
                tip(
                    segment(
                        "Features",
                        self.tab == Tab::Features,
                        Message::Tab(Tab::Features)
                    ),
                    Action::FeaturesTab.hint("Features panel", os),
                ),
                tip(
                    segment(
                        "Capture",
                        self.tab == Tab::Capture,
                        Message::Tab(Tab::Capture)
                    ),
                    Action::CaptureTab.hint("Capture panel", os),
                ),
                tip(
                    segment(
                        "Forward",
                        self.tab == Tab::Forward,
                        Message::Tab(Tab::Forward)
                    ),
                    Action::ForwardTab.hint("Forward panel", os),
                ),
            ]
            .spacing(2),
        )
        .padding(2)
        .style(style::segment_track);
        let content = match self.tab {
            Tab::Features => self.features(p),
            Tab::Capture => self.capture_settings(p),
            Tab::Forward => self.forward_settings(p),
        };
        container(column![
            self.titlebar(container(tabs).padding([0, 16]).into()),
            space().height(8),
            scrollable(container(content).padding(iced::Padding {
                top: 4.0,
                right: 20.0,
                bottom: 24.0,
                left: 18.0,
            }))
            .height(Fill)
            .style(style::scroll),
        ])
        .width(INSPECTOR)
        .height(Fill)
        .style(style::base)
        .into()
    }

    pub(super) fn features(&self, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        let connected = snapshot.connected.is_some();
        let mut content = column![].spacing(12);
        if !connected {
            return content
                .push(text("Camera settings").size(style::HEADING).font(style::SEMIBOLD))
                .push(
                    text("Connect a camera to inspect its GenICam features, configure acquisition, and run commands.")
                        .size(style::BODY)
                        .color(p.secondary),
                )
                .into();
        }
        content = content.push(self.auto_card(p));
        content = content.push(
            text_input("Search features", &self.search)
                .id("feature-search")
                .icon(icon::input_icon(Icon::Search))
                .on_input(Message::Search)
                .size(style::BODY)
                .padding([7, 10])
                .style(style::input),
        );
        content = content.push(
            row![
                text(format!("{} features", snapshot.features.len()))
                    .size(style::SMALL)
                    .color(p.secondary),
                space::horizontal(),
                button(text("Refresh").size(style::SMALL))
                    .padding([3, 8])
                    .style(style::link)
                    .on_press(Message::RefreshFeatures),
            ]
            .align_y(Alignment::Center),
        );
        let query = self.search.to_lowercase();
        let managed = |name: &str| {
            snapshot
                .auto
                .as_ref()
                .is_some_and(|auto| auto.managed.iter().any(|m| m == name))
        };
        let mut visible = 0;
        for group in ["Image", "Acquisition", "Device", "Transport", "Other"] {
            let features: Vec<_> = snapshot
                .features
                .iter()
                .filter(|f| {
                    feature_group(&f.name) == group
                        && (query.is_empty()
                            || f.name.to_lowercase().contains(&query)
                            || f.display_name.to_lowercase().contains(&query))
                })
                .collect();
            if features.is_empty() {
                continue;
            }
            visible += features.len();
            let mut section = column![heading(group, p)].spacing(12);
            for feature in features {
                section = section.push(self.feature(
                    feature,
                    snapshot.streaming,
                    managed(&feature.name),
                    p,
                ));
            }
            content = content.push(space().height(6)).push(section);
        }
        if visible == 0 {
            content = content.push(
                text("No matching features")
                    .size(style::BODY)
                    .color(p.secondary),
            );
        }
        content.into()
    }

    pub(super) fn auto_card(&self, p: &'static Palette) -> Element<'_, Message> {
        let auto = self.snapshot.auto.as_ref();
        let busy = self.auto_busy();
        let os = Os::CURRENT;
        let mut head = row![
            text("Exposure").size(style::BODY).font(style::SEMIBOLD),
            space::horizontal(),
        ]
        .align_y(Alignment::Center);
        if let Some(status) = auto {
            head = head.push(
                text(capitalize(&status.state))
                    .size(style::CAPTION)
                    .font(style::MEDIUM)
                    .color(match status.state.as_str() {
                        "stable" => p.live,
                        "limited" => p.warn,
                        _ => p.secondary,
                    }),
            );
        }
        let modes = container(
            row![
                tip(
                    segment(
                        "Manual",
                        auto.is_none(),
                        if busy {
                            None
                        } else {
                            Some(Message::Auto(false))
                        },
                    ),
                    Action::ToggleAuto.hint("Set exposure, gain and frame rate yourself", os),
                ),
                tip(
                    segment(
                        "Auto",
                        auto.is_some(),
                        if busy {
                            None
                        } else {
                            Some(Message::Auto(true))
                        },
                    ),
                    Action::ToggleAuto.hint("Tune exposure, gain and frame rate automatically", os),
                ),
            ]
            .spacing(2),
        )
        .padding(2)
        .style(style::segment_track);
        let mut card = column![head, modes].spacing(10);
        match auto {
            None => {
                card = card.push(
                    text("Auto tunes exposure, gain and frame rate for this camera.")
                        .size(style::SMALL)
                        .color(p.secondary),
                );
            }
            Some(status) => {
                card = card
                    .push(
                        column![
                            slider(0.0..=1.0, self.balance, Message::Balance)
                                .step(0.01)
                                .on_release(Message::BalanceReleased)
                                .style(style::slide),
                            row![
                                text("Quality").size(style::CAPTION).color(p.secondary),
                                space::horizontal(),
                                text("Frame rate").size(style::CAPTION).color(p.secondary),
                            ],
                        ]
                        .spacing(4),
                    )
                    .push(
                        text(auto_summary(status))
                            .size(style::SMALL)
                            .color(p.secondary),
                    );
                for note in &status.notes {
                    card = card.push(text(note.clone()).size(style::SMALL).color(p.warn));
                }
                if !status.changes.is_empty() {
                    card = card.push(disclosure(
                        format!("Auto changes ({})", status.changes.len()),
                        None,
                        if self.auto_changes_open { 1.0 } else { 0.0 },
                        Message::ToggleAutoChanges,
                        p,
                    ));
                    if self.auto_changes_open {
                        let mut changes = column![].spacing(3);
                        for change in status.changes.iter().rev() {
                            let unit = self
                                .snapshot
                                .features
                                .iter()
                                .find(|f| f.name == change.feature)
                                .and_then(|f| f.unit.as_deref());
                            changes = changes.push(tip(
                                text(change_text(change, unit))
                                    .size(style::CAPTION)
                                    .font(style::MONO)
                                    .color(p.secondary),
                                change.reason.clone(),
                            ));
                        }
                        card = card.push(
                            scrollable(changes)
                                .height(Length::Shrink)
                                .style(style::scroll),
                        );
                    }
                }
            }
        }
        container(card)
            .padding(14)
            .width(Fill)
            .style(style::well)
            .into()
    }

    pub(super) fn feature<'a>(
        &'a self,
        feature: &'a FeatureInfo,
        streaming: bool,
        managed: bool,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let locked = streaming
            && matches!(
                feature.name.as_str(),
                "Width"
                    | "Height"
                    | "OffsetX"
                    | "OffsetY"
                    | "PixelFormat"
                    | "BinningHorizontal"
                    | "BinningVertical"
                    | "DecimationHorizontal"
                    | "DecimationVertical"
                    | "VideoMode"
            );
        let writable = (feature.writable || managed) && !locked;
        let name = if feature.display_name.is_empty() || feature.display_name == feature.name {
            words(&feature.name)
        } else {
            feature.display_name.clone()
        };
        let kind = feature.kind.to_lowercase();
        let label = tip(
            text(name).size(style::BODY),
            format!("{}\n{}", feature.name, feature.description),
        );
        let current = feature_value(feature);
        let mut notes: Vec<(String, Color)> = Vec::new();
        if managed && !locked {
            notes.push(("Managed by auto".into(), p.accent_text));
        } else if !writable && kind != "command" {
            notes.push((
                if locked {
                    "Stop stream to edit"
                } else {
                    "Read only"
                }
                .into(),
                p.tertiary,
            ));
        }
        let control: Element<'_, Message> = if let Some(error) = &feature.error
            && kind != "command"
        {
            notes.push((error.clone(), p.warn));
            space().into()
        } else if kind == "command" {
            let enabled = feature.writable
                && match feature.name.as_str() {
                    "AcquisitionStart" => !streaming,
                    "AcquisitionStop" => streaming,
                    _ => true,
                };
            let action = match feature.name.as_str() {
                "AcquisitionStart" => "Start stream",
                "AcquisitionStop" => "Stop stream",
                _ => "Execute",
            };
            button(text(action).size(style::SMALL))
                .padding([4, 10])
                .style(style::secondary)
                .on_press_maybe(enabled.then(|| Message::Execute(feature.name.clone())))
                .into()
        } else if !writable {
            text(format!(
                "{current}{}",
                feature
                    .unit
                    .as_ref()
                    .map(|u| format!(" {u}"))
                    .unwrap_or_default()
            ))
            .size(style::SMALL)
            .color(p.secondary)
            .into()
        } else if kind == "boolean" || kind == "bool" {
            let checked = feature
                .value
                .as_ref()
                .and_then(|v| v.as_bool())
                .unwrap_or(current == "true" || current == "1");
            let name = feature.name.clone();
            iced::widget::checkbox(checked)
                .on_toggle(move |value| Message::Set(name.clone(), value.to_string()))
                .size(16)
                .style(style::check)
                .into()
        } else if !feature.choices.is_empty() {
            let name = feature.name.clone();
            pick_list(
                &feature.choices[..],
                feature.choices.iter().find(|c| **c == current).cloned(),
                move |value| Message::Set(name.clone(), value),
            )
            .placeholder(current.clone())
            .width(150)
            .text_size(style::SMALL)
            .padding([5, 8])
            .style(style::pick)
            .menu_style(style::menu)
            .into()
        } else {
            let draft = self.edits.get(&feature.name).unwrap_or(&current);
            let draft_name = feature.name.clone();
            let mut editor = row![
                text_input(&current, draft)
                    .on_input(move |value| Message::Draft(draft_name.clone(), value))
                    .on_submit(Message::Commit(feature.name.clone()))
                    .size(style::SMALL)
                    .padding([5, 8])
                    .width(110)
                    .style(style::input),
            ]
            .spacing(4)
            .align_y(Alignment::Center);
            if *draft != current {
                editor = editor.push(tip(
                    button(text("Set").size(style::SMALL))
                        .padding([4, 6])
                        .style(style::link)
                        .on_press(Message::Commit(feature.name.clone())),
                    Action::FocusCamera.hint("Apply", Os::CURRENT),
                ));
            }
            editor.into()
        };
        if feature.min.is_some() || feature.max.is_some() || feature.unit.is_some() {
            let range = match (feature.min, feature.max) {
                (Some(min), Some(max)) => format!("{} – {}", number(min), number(max)),
                (Some(min), None) => format!("min {}", number(min)),
                (None, Some(max)) => format!("max {}", number(max)),
                _ => String::new(),
            };
            let text = format!("{range} {}", feature.unit.as_deref().unwrap_or_default());
            if !text.trim().is_empty() && writable {
                notes.push((text.trim().to_owned(), p.tertiary));
            }
        }
        let mut item = column![
            row![container(label).width(Fill), control]
                .spacing(10)
                .align_y(Alignment::Center),
        ]
        .spacing(2);
        if !notes.is_empty() {
            let mut line = row![].spacing(8);
            for (note, color) in notes {
                line = line.push(text(note).size(style::CAPTION).color(color));
            }
            item = item.push(line.wrap());
        }
        item.into()
    }

    pub(super) fn num(&self, key: Num, width: f32) -> Element<'_, Message> {
        let value = self
            .drafts
            .get(&key)
            .cloned()
            .unwrap_or_else(|| self.num_text(key));
        let valid = !self.drafts.contains_key(&key) || num_valid(key, &value);
        text_input("", &value)
            .on_input(move |value| Message::Num(key, value))
            .size(style::BODY)
            .padding([5, 8])
            .width(width)
            .style(if valid {
                style::input
            } else {
                style::input_invalid
            })
            .into()
    }

    pub(super) fn capture_settings(&self, p: &'static Palette) -> Element<'_, Message> {
        let connected = self.snapshot.connected.is_some();
        let capturing = self.pending("Saving capture");
        let os = Os::CURRENT;
        let mut content = column![
            heading("Save frames", p),
            text("Capture to a file or numbered sequence on this computer, an external or network drive, or an S3 bucket.")
                .size(style::SMALL)
                .color(p.secondary),
            self.capture_to
                .view(&self.output, "Output path", self.dark())
                .map(Message::CapturePicker),
            field("Frames", unit(self.num(Num::Count, 80.0), "", p), p),
            field(
                "Format",
                pick_list(&FORMATS[..], find(&FORMATS, self.format), Message::Format)
                    .width(150)
                    .text_size(style::BODY)
                    .padding([5, 8])
                    .style(style::pick)
                    .menu_style(style::menu)
                    .into(),
                p,
            ),
            field("Timeout", unit(self.num(Num::Timeout, 80.0), "ms", p), p),
            space().height(2),
            button(
                center(
                    row![
                        icon(Icon::Camera, 14.0, Color::WHITE),
                        text(if capturing { "Saving…" } else { "Capture & save" })
                            .size(style::BODY)
                            .font(style::MEDIUM),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .height(Length::Shrink),
            )
            .width(Fill)
            .padding([9, 14])
            .style(style::primary)
            .on_press_maybe(
                (connected && !self.output.trim().is_empty() && !capturing)
                    .then_some(Message::Capture),
            ),
            text(format!("{} · Capture from this session", Action::Capture.shortcut(os)))
                .size(style::CAPTION)
                .color(p.tertiary),
            space().height(6),
            checkbox(
                "Capture on a schedule",
                self.schedule_enabled,
                Message::ScheduleEnabled,
            ),
        ]
        .spacing(12);
        if self.schedule_enabled {
            content = content
                .push(field(
                    "Start after",
                    unit(self.num(Num::Delay, 80.0), "s", p),
                    p,
                ))
                .push(field(
                    "Frame interval",
                    unit(self.num(Num::Interval, 80.0), "s", p),
                    p,
                ))
                .push(
                    text(format!(
                        "{} frames, one every {} seconds",
                        self.count,
                        number(self.schedule_interval_seconds)
                    ))
                    .size(style::SMALL)
                    .color(p.secondary),
                )
                .push(
                    button(
                        center(text("Schedule capture").size(style::BODY)).height(Length::Shrink),
                    )
                    .width(Fill)
                    .padding([8, 14])
                    .style(style::secondary)
                    .on_press_maybe(
                        (connected && !self.output.trim().is_empty()).then_some(Message::Schedule),
                    ),
                );
        }
        content = content
            .push(space().height(4))
            .push(self.storage_settings(p))
            .push(space().height(4))
            .push(self.jobs(p))
            .push(space().height(8))
            .push(heading("Automation", p))
            .push(
                text("Control this same camera from your shell or coding agent. Camera ownership stays in this session.")
                    .size(style::SMALL)
                    .color(p.secondary),
            )
            .push(self.copyable(self.session_command(), p));
        content.into()
    }

    pub(super) fn forward_settings(&self, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        let pending = self.pending("Starting forwarding") || self.pending("Stopping forwarding");
        let mut content = column![
            heading("Forward to a recorder", p),
            text("Publish the selected camera to MediaMTX, an NVR, or another compatible destination.")
                .size(style::SMALL)
                .color(p.secondary),
            self.record_to
                .view(
                    &self.forward_output,
                    "Stream URL or recording file (rtsp://, srt://, capture.mkv)",
                    self.dark(),
                )
                .map(Message::RecordPicker),
            field(
                "Codec",
                pick_list(&CODECS[..], find(&CODECS, self.forward_codec), Message::Codec)
                    .width(150)
                    .text_size(style::BODY)
                    .padding([5, 8])
                    .style(style::pick)
                    .menu_style(style::menu)
                    .into(),
                p,
            ),
            field(
                "Encoder",
                tip(
                    text_input("auto", &self.forward_encoder)
                        .on_input(Message::Encoder)
                        .size(style::BODY)
                        .padding([5, 8])
                        .width(150)
                        .style(style::input),
                    "auto selects an available encoder. Enter an FFmpeg encoder name to override.",
                ),
                p,
            ),
            field("Frame rate", unit(self.num(Num::Fps, 80.0), "fps", p), p),
            field(
                "Bitrate",
                text_input("4M", &self.forward_bitrate)
                    .on_input(Message::Bitrate)
                    .size(style::BODY)
                    .padding([5, 8])
                    .width(150)
                    .style(style::input)
                    .into(),
                p,
            ),
            field(
                "Maximum file",
                tip(
                    unit(self.num(Num::FileMib, 80.0), "MiB", p),
                    "For file destinations, recording stops when this file size limit is reached.",
                ),
                p,
            ),
            space().height(2),
        ]
        .spacing(12);
        if let Some(destination) = &snapshot.forwarding {
            content = content
                .push(
                    row![
                        dot(p.live, 7.0),
                        text("Forwarding")
                            .size(style::SMALL)
                            .font(style::SEMIBOLD)
                            .color(p.ink(p.live)),
                    ]
                    .spacing(7)
                    .align_y(Alignment::Center),
                )
                .push(
                    text(redact_address(destination))
                        .size(style::SMALL)
                        .font(style::MONO)
                        .color(p.secondary),
                )
                .push(
                    button(
                        center(text("Stop forwarding").size(style::BODY)).height(Length::Shrink),
                    )
                    .width(Fill)
                    .padding([9, 14])
                    .style(style::danger)
                    .on_press_maybe((!pending).then_some(Message::StopForward)),
                );
        } else {
            content = content.push(
                button(
                    center(
                        row![
                            icon(Icon::Broadcast, 15.0, Color::WHITE),
                            text(if pending {
                                "Starting…"
                            } else {
                                "Start forwarding"
                            })
                            .size(style::BODY)
                            .font(style::MEDIUM),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center),
                    )
                    .height(Length::Shrink),
                )
                .width(Fill)
                .padding([9, 14])
                .style(style::primary)
                .on_press_maybe(
                    (snapshot.connected.is_some()
                        && !pending
                        && !self.forward_output.trim().is_empty())
                    .then_some(Message::StartForward),
                ),
            );
        }
        content
            .push(space().height(4))
            .push(self.storage_settings(p))
            .push(space().height(8))
            .push(heading("Encoder availability", p))
            .push(
                text("Automatic selection uses the available hardware or software encoder. Run capturefab doctor to inspect media support and available encoders.")
                    .size(style::SMALL)
                    .color(p.secondary),
            )
            .push(self.copyable("capturefab doctor".into(), p))
            .into()
    }

    pub(super) fn storage_settings(&self, p: &'static Palette) -> Element<'_, Message> {
        let summary = format!(
            "{} GiB · {} files · {}",
            number(self.quota_gib),
            grouped(self.quota_files.into()),
            if self.quota_action == "stop" {
                "stop at capacity"
            } else {
                "delete oldest at capacity"
            }
        );
        let mut content = column![disclosure(
            "Storage & retention",
            None,
            if self.storage_open { 1.0 } else { 0.0 },
            Message::ToggleStorage,
            p
        ),]
        .spacing(10);
        if self.storage_open {
            content = content
                .push(field(
                    "Disk budget",
                    unit(self.num(Num::QuotaGib, 80.0), "GiB", p),
                    p,
                ))
                .push(field(
                    "File limit",
                    unit(self.num(Num::QuotaFiles, 80.0), "", p),
                    p,
                ))
                .push(field(
                    "At capacity",
                    pick_list(
                        &ON_FULL[..],
                        find(&ON_FULL, self.quota_action),
                        Message::OnFull,
                    )
                    .width(150)
                    .text_size(style::BODY)
                    .padding([5, 8])
                    .style(style::pick)
                    .menu_style(style::menu)
                    .into(),
                    p,
                ))
                .push(checkbox(
                    "Limit capture age",
                    self.retention_enabled,
                    Message::RetentionEnabled,
                ));
            if self.retention_enabled {
                content = content.push(field(
                    "Keep for",
                    unit(self.num(Num::RetentionDays, 80.0), "days", p),
                    p,
                ));
            }
            if self.quota_action == "delete-oldest" {
                content = content.push(
                    text("At capacity, removes the oldest Capturefab managed files in the output location.")
                        .size(style::SMALL)
                        .color(p.warn),
                );
            }
        }
        content
            .push(text(summary).size(style::CAPTION).color(p.tertiary))
            .into()
    }

    pub(super) fn jobs(&self, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        let mut content = column![
            row![
                text("Scheduled jobs")
                    .size(style::BODY)
                    .font(style::SEMIBOLD),
                space::horizontal(),
                button(text("Refresh").size(style::SMALL))
                    .padding([3, 8])
                    .style(style::link)
                    .on_press_maybe(snapshot.connected.is_some().then_some(Message::RefreshJobs)),
            ]
            .align_y(Alignment::Center),
        ]
        .spacing(10);
        if snapshot.jobs.is_empty() {
            return content
                .push(
                    text("No scheduled captures for this camera.")
                        .size(style::SMALL)
                        .color(p.secondary),
                )
                .into();
        }
        let jobs = serde_json::to_value(&snapshot.jobs).unwrap_or_default();
        for job in jobs.as_array().into_iter().flatten().rev() {
            let id = job["id"].as_u64().unwrap_or_default();
            let status = job["status"].as_str().unwrap_or("unknown");
            let captured = job["captured"].as_u64().unwrap_or_default();
            let count = job["count"].as_u64().unwrap_or(1);
            let mut card = column![
                row![
                    text(format!("Job {id}"))
                        .size(style::BODY)
                        .font(style::MEDIUM),
                    space::horizontal(),
                    text(capitalize(status))
                        .size(style::CAPTION)
                        .color(match status {
                            "failed" => p.danger,
                            "running" | "pending" => p.accent_text,
                            _ => p.secondary,
                        }),
                ]
                .align_y(Alignment::Center),
                progress_bar(0.0..=1.0, captured as f32 / count.max(1) as f32)
                    .girth(4)
                    .style(style::progress),
                text(format!("{captured} / {count} frames"))
                    .size(style::CAPTION)
                    .color(p.secondary),
                text(job["output"].as_str().unwrap_or("").to_owned())
                    .size(style::CAPTION)
                    .font(style::MONO)
                    .color(p.secondary),
            ]
            .spacing(6);
            if matches!(status, "pending" | "running") {
                let wait = job["next_at_ms"]
                    .as_u64()
                    .unwrap_or_default()
                    .saturating_sub(epoch_ms());
                card = card.push(
                    row![
                        text(format!("Next frame in {:.1} s", wait as f64 / 1000.0))
                            .size(style::CAPTION)
                            .color(p.secondary),
                        space::horizontal(),
                        button(text("Stop").size(style::SMALL))
                            .padding([3, 8])
                            .style(style::danger)
                            .on_press(Message::CancelJob(id)),
                    ]
                    .align_y(Alignment::Center),
                );
            }
            if let Some(error) = job["error"].as_str() {
                card = card.push(
                    text(error.to_owned())
                        .size(style::CAPTION)
                        .color(p.ink(p.danger)),
                );
            }
            content = content.push(container(card).padding(12).width(Fill).style(style::well));
        }
        content.into()
    }
}
