//! Capture task controls render cached scope, checked approvals and run events.
//! The view never opens credentials, contacts a provider or executes a tool.
use super::{settings::SecretDraft, tasks, *};
use crate::assistant::{
    approval::ApprovalScope,
    limits::Limits,
    run::{Outcome, RunState},
};
use crate::tools;
use iced::widget::{column, image};

pub(in crate::gui) fn mode_header<'a>(
    state: &'a tasks::State,
    _p: &'static Palette,
) -> Element<'a, tasks::Input> {
    let modes = [
        ("Plan first", tasks::Mode::Plan),
        ("Ask before each change", tasks::Mode::Supervised),
        ("Run on its own", tasks::Mode::Autonomous),
    ];
    let selected = modes
        .iter()
        .position(|(_, mode)| *mode == state.mode)
        .unwrap_or_default();
    segmented(
        "How the assistant works",
        modes
            .into_iter()
            .map(|(label, mode)| {
                let hint = match mode {
                    tasks::Mode::Plan => "Review and edit every step before running a plan.",
                    tasks::Mode::Supervised => "Approve each capture action before it runs.",
                    tasks::Mode::Autonomous => {
                        "Let the assistant work within your scope, approvals and limits."
                    }
                };
                (
                    label,
                    (state.editable() && state.outcome.is_none())
                        .then_some(tasks::Input::Mode(mode)),
                    hint.into(),
                )
            })
            .collect(),
        selected,
        selected as f32,
    )
}

pub(in crate::gui) fn content<'a>(
    state: &'a tasks::State,
    p: &'static Palette,
    height: f32,
    spin: usize,
) -> Element<'a, tasks::Input> {
    let started = state.active() || state.outcome.is_some() || !state.snapshot.run_id.is_empty();
    let mut body = column![].spacing(16).width(Fill);
    if let Some(crash) = crash_banner(state, p) {
        body = body.push(crash);
    }
    if started {
        body = body.push(
            text(&state.goal)
                .size(style::BODY)
                .wrapping(text::Wrapping::WordOrGlyph),
        );
        if let Some(outcome) = &state.outcome {
            let level = match outcome.outcome {
                Outcome::Finished => Level::Done,
                Outcome::Stopped | Outcome::NeedsUser | Outcome::AppQuit => Level::Warning,
                Outcome::Limit | Outcome::Failed => Level::Error,
            };
            let summary = status_line(outcome.summary.clone(), level, p);
            body = body.push(if level == Level::Error {
                accessibility::label(summary, outcome.summary.clone(), accessibility::Role::Alert)
            } else {
                summary
            });
        } else if !state.snapshot.summary.is_empty() && state.data_scope_issue().is_none() {
            body = body.push(caption(state.snapshot.summary.clone(), p));
        }
        if let Some(issue) = state.data_scope_issue() {
            body = body.push(status_line(issue, Level::Warning, p));
        }
        body = body.push(responsive(move |size| {
            overview(state, p, size.width < 552.0)
        }));
        body = body.push(timeline(state, p));
        if let Some(request) = &state.approval {
            body = body.push(approval(state, request, p));
        }
        if let Some(outcome) = &state.outcome
            && !outcome.leftovers.is_empty()
        {
            body = body.push(leftovers(state, p));
        }
    } else {
        body = body.push(setup(state, p));
    }
    if let Some(preview) = &state.restore_preview {
        body = body.push(restore_preview(preview, p));
    }
    body = body.push(retained_datasets(state, p));
    if let Some(error) = &state.error {
        body = body.push(field_error(error.clone(), p));
    }
    if let Some(excerpt) = &state.provider_excerpt {
        body = body.push(caption(format!("Provider said: {excerpt}"), p));
    }
    if let Some(log) = &state.log_text {
        body = body.push(log_view(log, p));
    }
    let mut controls = column![mode_header(state, p)].spacing(8);
    if started {
        controls = controls.push(status(state, p, spin));
    }
    controls = controls.push(
        button(
            row![
                icon(Icon::Sparkle, 13.0, p.accent),
                one_line(
                    format!("{} · {}", state.provider_label(), state.model_label()),
                    style::SMALL,
                    style::MEDIUM,
                    p.text,
                )
                .width(Fill),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .name("Assistant task provider and model")
        .description("Open AI assistant Settings")
        .width(Fill)
        .padding([5, 8])
        .style(style::secondary)
        .on_press_maybe((!state.active()).then_some(tasks::Input::Settings)),
    );
    sheet_frame("Quick Command", tasks::Input::Open(false), body, p)
        .mark(Icon::Sparkle)
        .size(720.0, height)
        // Approval controls are the newest activity and must be reachable
        // immediately. Other views start with the current image and budgets.
        .anchor(if state.approval.is_some() || state.log_text.is_some() {
            scrollable::Anchor::End
        } else {
            scrollable::Anchor::Start
        })
        .scrolled(state.scrolled)
        .on_scroll(tasks::Input::Scrolled)
        .controls(controls)
        .footer(footer(state, p))
        // Return belongs only to the focused text field. It never approves,
        // starts or stops a camera task.
        .default_action(None)
        .into()
}

/// Unfinished-task recovery remains visible in a fresh Plan-first
/// conversation. Showing a log never changes the selected assistant mode.
pub(in crate::gui) fn plan_notice<'a>(
    state: &'a tasks::State,
    p: &'static Palette,
) -> Option<Element<'a, tasks::Input>> {
    let mut body = column![].spacing(10).width(Fill);
    body = body.push(retained_datasets(state, p));
    if let Some(crash) = crash_banner(state, p) {
        body = body.push(crash);
    }
    if let Some(log) = &state.log_text {
        body = body.push(log_view(log, p));
    }
    if let Some(error) = &state.error {
        body = body.push(field_error(error.clone(), p));
    }
    Some(body.into())
}

fn retained_datasets<'a>(
    state: &'a tasks::State,
    p: &'static Palette,
) -> Element<'a, tasks::Input> {
    let idle = !state.active() && !state.retained_busy();
    let mut body = column![
        button(
            text(if state.retained_open {
                "Refresh retained datasets"
            } else {
                "Retained datasets…"
            })
            .size(style::SMALL)
        )
        .name("Review retained task datasets")
        .style(style::secondary)
        .padding([6, 10])
        .on_press_maybe(idle.then_some(tasks::Input::RetainedDatasets))
    ]
    .spacing(10);
    if state.retained_open {
        body = body.push(heading("Retained task datasets", p));
        if state.retained_busy() {
            body = body.push(caption("Working…".into(), p));
        } else if state.retained.is_empty() && state.error.is_none() {
            body = body.push(caption("No retained task datasets.".into(), p));
        }
        for (index, row) in state.retained.iter().enumerate() {
            let mut item = column![
                caption(row.detail.clone(), p),
                text(row.path.display().to_string())
                    .font(style::MONO)
                    .size(style::SMALL)
                    .wrapping(text::Wrapping::WordOrGlyph),
                caption(
                    format!(
                        "{} storage reservation",
                        crate::analytics::human_bytes(row.cap)
                    ),
                    p
                ),
                iced::widget::row![
                    button(text(crate::gui::reveal::label()).size(style::SMALL))
                        .name(format!("Reveal retained dataset {}", index + 1))
                        .style(style::secondary)
                        .padding([6, 10])
                        .on_press_maybe(
                            (idle && row.available())
                                .then_some(tasks::Input::RevealRetained(row.key))
                        ),
                    button(text("Save a copy…").size(style::SMALL))
                        .name(format!("Copy retained dataset {}", index + 1))
                        .style(style::secondary)
                        .padding([6, 10])
                        .on_press_maybe(
                            (idle && row.copyable()).then_some(tasks::Input::CopyRetained(row.key))
                        ),
                    button(text("Remove…").size(style::SMALL))
                        .id(format!("assistant-retained-remove-{}", row.key))
                        .name(format!("Remove retained dataset {}", index + 1))
                        .style(style::secondary)
                        .padding([6, 10])
                        .on_press_maybe(
                            (idle && row.removable())
                                .then_some(tasks::Input::RemoveRetained(row.key))
                        ),
                ]
                .spacing(8)
                .wrap()
                .vertical_spacing(8),
            ]
            .spacing(8);
            if let Some(error) = &row.error {
                item = item.push(status_line(error.clone(), Level::Warning, p));
            }
            if let Some((key, review)) = state.remove_retained.filter(|(key, _)| *key == row.key) {
                item = item.push(status_line("Permanently remove this workspace and all its data? This cannot be undone. Its full reservation is released only after confirmed removal.".into(), Level::Warning, p));
                item = item.push(
                    iced::widget::row![
                        button(text("Remove dataset permanently").size(style::SMALL))
                            .name("Confirm permanent dataset removal")
                            .style(style::secondary)
                            .padding([6, 10])
                            .on_press_maybe(
                                idle.then_some(tasks::Input::ConfirmRemoveRetained(key, review))
                            ),
                        button(text("Keep dataset").size(style::SMALL))
                            .id("assistant-retained-keep")
                            .name("Cancel dataset removal")
                            .style(style::secondary)
                            .padding([6, 10])
                            .on_press(tasks::Input::CancelRemoveRetained),
                    ]
                    .spacing(8)
                    .wrap()
                    .vertical_spacing(8),
                );
            }
            body = body.push(well(item.into()));
        }
    }
    body.into()
}

fn crash_banner<'a>(
    state: &'a tasks::State,
    p: &'static Palette,
) -> Option<Element<'a, tasks::Input>> {
    let crash = state.crash.as_ref()?;
    let at = local_time(state, crash.time_ms);
    Some(well(
        column![
            status_line(
                format!("The last assistant task did not finish (Capturefab stopped at {at})."),
                Level::Warning,
                p,
            ),
            button(text("View log").size(style::SMALL))
                .name("View unfinished assistant task log")
                .style(style::secondary)
                .padding([6, 10])
                .on_press(tasks::Input::ViewLog),
        ]
        .spacing(8)
        .into(),
    ))
}

fn log_view<'a>(log: &'a str, p: &'static Palette) -> Element<'a, tasks::Input> {
    well(
        column![
            row![
                heading("Task log", p),
                space().width(Fill),
                button(text("Close log").size(style::SMALL))
                    .name("Close assistant task log")
                    .id("assistant-task-close-log")
                    .style(style::plain)
                    .padding([5, 8])
                    .on_press(tasks::Input::CloseLog),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            scrollable(
                container(
                    text(log)
                        .font(style::MONO)
                        .size(style::SMALL)
                        .wrapping(text::Wrapping::WordOrGlyph),
                )
                .padding(10)
                .style(style::code)
                .width(Fill)
            )
            .height(200)
            .width(Fill)
            .style(style::sheet_scroll),
        ]
        .spacing(8)
        .into(),
    )
}

fn setup<'a>(state: &'a tasks::State, p: &'static Palette) -> Element<'a, tasks::Input> {
    let editable = state.editable();
    let goal = text_input::text_input("Describe what the assistant should achieve…", &state.goal)
        .id("assistant-task-goal")
        .name("Assistant task goal")
        .description("Describe the image or data capture task. Start task begins only after you choose its scope and limits.")
        .size(style::BODY)
        .padding([7, 10])
        .width(Fill)
        .style(style::input)
        .on_input_maybe(editable.then_some(tasks::Input::Goal));
    let mut body = column![
        heading("Set up this task", p),
        text("Task").size(style::SMALL).color(p.secondary),
        goal,
        heading("Cameras the assistant may control", p),
        caption(
            "Choose cameras for image capture. Data tasks can run with no camera selected.".into(),
            p
        ),
    ]
    .spacing(10)
    .width(Fill);
    if let Some(context) = &state.context {
        if context.snapshot.cameras.is_empty() {
            body = body.push(caption(
                "Connect a camera for images, or choose a data source below.".into(),
                p,
            ));
        }
        for camera in &context.snapshot.cameras {
            let id = camera.info.id.clone();
            let label = format!(
                "{} · {}",
                crate::tools::camera_name(context, &camera.info),
                camera.info.id
            );
            let checked = state.cameras.contains(&camera.info.id);
            body = body.push(scope_check(
                label,
                checked,
                editable.then_some(tasks::Input::Camera(id, !checked)),
            ));
        }
        for missing in state.cameras.iter().filter(|id| {
            !context
                .snapshot
                .cameras
                .iter()
                .any(|camera| camera.info.id == **id)
        }) {
            let id = missing.clone();
            let label = format!(
                "{} (not available)",
                crate::tools::redact::for_output_text(missing)
            );
            body = body.push(scope_check(
                label,
                true,
                editable.then_some(tasks::Input::Camera(id, false)),
            ));
        }
        body = body.push(data_setup(state, p));
        body = body
            .push(heading("Allowed destinations", p))
            .push(caption("Photos may also be saved in your capture folder. New remote destinations always need approval.".into(), p));
        if context.destinations.is_empty() {
            body = body.push(caption("No saved destinations.".into(), p));
        }
        for destination in &context.destinations {
            let name = destination.name.clone();
            let label = if destination.remote {
                format!("{} · leaves this computer", destination.name)
            } else {
                destination.name.clone()
            };
            let checked = state.destinations.contains(&destination.name);
            body = body.push(scope_check(
                label,
                checked,
                editable.then_some(tasks::Input::Destination(name, !checked)),
            ));
        }
        for missing in state.destinations.iter().filter(|name| {
            !context
                .destinations
                .iter()
                .any(|destination| destination.name == **name)
        }) {
            body = body.push(scope_check(
                format!(
                    "{} (not available)",
                    crate::tools::redact::for_output_text(missing)
                ),
                true,
                editable.then_some(tasks::Input::Destination(missing.clone(), false)),
            ));
        }
    } else {
        body = body.push(caption(
            "Gathering cameras and saved destinations…".into(),
            p,
        ));
    }
    body = body
        .push(checkbox_maybe(
            "Let the assistant see camera images",
            state.allow_images,
            editable.then_some(tasks::Input::SeeImages),
        ))
        .push(caption(
            format!(
                "Sends inspected images to {}, under that provider's data terms. Camera details and numeric image measurements are sent even when images are off. Never sends keys.",
                state.provider_label()
            ),
            p,
        ))
        .push(caption(state.vision_label(), p));
    if let Some(suggestion) = state.vision_suggestion() {
        body = body.push(caption(suggestion, p));
    }
    let presets = [
        tasks::Preset::Short,
        tasks::Preset::Standard,
        tasks::Preset::Long,
    ];
    body = body
        .push(heading("Hard limits", p))
        .push(
            pick_list::pick_list(
                presets.to_vec(),
                Some(state.preset),
                tasks::Input::Preset,
            )
            .name("Assistant task limits preset")
            .description("Short, Standard or Long. Every limit is enforced before requests and capture actions.")
            .width(Fill)
            .padding([7, 10])
            .text_size(style::BODY)
            .style(style::pick)
            .menu_style(style::menu)
            .enabled(editable),
        )
        .push(caption(
            format!(
                "{} rounds · {} · {} images · {} captures · {} data",
                state.limits.max_rounds,
                format::span(Duration::from_millis(state.limits.max_wall_ms)),
                state.limits.max_images,
                state.limits.max_captures,
                crate::analytics::human_bytes(state.limits.max_bytes),
            ),
            p,
        ))
        .push(caption(
            format!(
                "Up to {} recordings · {} per recording · {} setting changes",
                state.limits.max_recordings,
                format::span(Duration::from_millis(state.limits.max_recording_ms)),
                state.limits.max_setting_changes,
            ),
            p,
        ))
        .push(caption(format!("Up to {} source captures · {} samples · {} reserved data · {} total signal time",
            state.limits.max_sources,format::grouped(state.limits.max_samples),crate::analytics::human_bytes(state.limits.max_data_bytes),format::span(Duration::from_millis(state.limits.max_signal_ms))),p))
        .push(disclosure(
            "Edit limits",
            None,
            f32::from(state.limits_open),
            tasks::Input::Limits(!state.limits_open),
            p,
        ));
    if state.limits_open {
        for (key, label, help) in LIMIT_FIELDS {
            body = body.push(limit_field(state, key, label, help, p));
        }
    }
    if let Some((low, high)) = state.estimated_cost() {
        body = body.push(caption(
            format!("Estimated model cost: ${low:.2}–${high:.2}. Actual use varies with the task."),
            p,
        ));
    }
    body = body.push(caption(
        "These choices apply to this task. Every new conversation starts with Plan first.".into(),
        p,
    ));
    well(body.into())
}

fn data_setup<'a>(state: &'a tasks::State, p: &'static Palette) -> Element<'a, tasks::Input> {
    let editable = state.editable();
    let draft_error = state.source_draft_error();
    let source_input = text_input::text_input("sim-signal://line1",&state.data_source_draft)
        .id("assistant-task-data-source").name("Assistant task data source endpoint")
        .description("Add one credential-free source endpoint. The assistant receives an opaque source reference and a redacted location.")
        .size(style::BODY).padding([7,10]).width(Fill)
        .invalid(draft_error.clone())
        .style(if draft_error.is_some(){style::input_invalid}else{style::input})
        .on_input_maybe(editable.then_some(tasks::Input::DataSourceDraft))
        .on_submit(tasks::Input::AddDataSource);
    let add = button(text("Add source").size(style::SMALL))
        .name("Add data source to assistant task")
        .id("assistant-task-add-source")
        .style(style::secondary)
        .padding([8, 10])
        .on_press_maybe(
            (editable && !state.data_source_draft.trim().is_empty() && draft_error.is_none())
                .then_some(tasks::Input::AddDataSource),
        );
    let mut body = column![
        heading("Data sources",p),
        caption("Add an endpoint for new captures. Select a running source to inspect, wait or stop it. New network connections always need approval.".into(),p),
        row![source_input,add].spacing(8).align_y(Alignment::Center),
    ].spacing(8).width(Fill);
    if let Some(error) = draft_error {
        body = body.push(field_error(error, p));
    }
    for selected in &state.data_sources {
        body = body.push(scope_check(
            crate::tools::redact::for_output_text(selected),
            true,
            editable.then_some(tasks::Input::DataSource(selected.clone(), false)),
        ));
    }
    for source in &state.profile_sources {
        body = body.push(scope_check(
            format!(
                "{} · protected session binding",
                crate::tools::redact::for_output_text(&source.uri)
            ),
            true,
            editable.then_some(tasks::Input::DataSource(source.reference.clone(), false)),
        ));
    }
    body = body.push(accessibility::mapped(
        crate::gui::source_setup::view(
            &state.source_setup,
            p,
            editable,
            "task-source-profile",
            crate::gui::source_setup::Purpose::Approve,
        ),
        tasks::Input::SourceProfile,
    ));
    if let Some(context) = &state.context {
        for source in &context.snapshot.signals {
            if state.source_selected(&source.id) {
                continue;
            }
            body = body.push(scope_check(
                format!(
                    "{} · {} · {:?} · {} written",
                    tools::redact::for_output_text(&source.uri),
                    source.id,
                    source.state,
                    format::grouped(source.written)
                ),
                false,
                editable.then_some(tasks::Input::DataSource(source.id.clone(), true)),
            ));
        }
        body = body.push(heading("Dataset", p));
        body = body.push(
            if editable && (context.task_data_available || state.separate_dataset) {
                checkbox_named_id(
                    "Separate dataset for this task",
                    "Separate dataset for this task",
                    state.separate_dataset,
                    tasks::Input::SeparateDataset,
                    "assistant-task-separate-dataset".into(),
                )
            } else {
                checkbox_maybe(
                    "Separate dataset for this task",
                    state.separate_dataset,
                    None::<fn(bool) -> tasks::Input>,
                )
            },
        );
        if context.task_data_available || state.separate_dataset {
            let cap = state
                .task_dataset_cap()
                .map(crate::analytics::human_bytes)
                .map(|cap| format!("Sensor-only captures use a separate dataset with a {cap} logical file extent cap."))
                .unwrap_or_else(|| "Enter valid byte limits to review the separate dataset cap.".into());
            body = body.push(caption(format!("{cap} Its full storage reservation stays charged until the dataset is removed. Filesystem overhead and model memory are separate."), p));
            body = body.push(caption("The dataset reservation counts toward the overall byte limit. Signal payload and export allowances also apply.".into(),p));
            if !context.task_data_available {
                body = body.push(status_line("Separate task datasets are unavailable in this session. Turn off this choice to use the shared workflow.".into(),Level::Warning,p));
            }
            if state.separate_dataset && !state.cameras.is_empty() {
                body = body.push(status_line(
                    "Deselect cameras for a separate sensor dataset, or use the shared workflow."
                        .into(),
                    Level::Warning,
                    p,
                ));
            }
        } else {
            body = body.push(caption(
                "Separate task datasets are unavailable in this session.".into(),
                p,
            ));
        }
        if let Some(dataset) = context
            .snapshot
            .dataset
            .as_ref()
            .filter(|dataset| dataset.slot_id.is_some())
        {
            let selected = dataset.slot_id == state.dataset;
            body = body.push(scope_check(
                format!(
                    "Use {} · {} samples",
                    dataset.name,
                    format::grouped(dataset.samples)
                ),
                selected,
                editable.then_some(tasks::Input::UseDataset(!selected)),
            ));
            body = body.push(caption("Select this dataset to save or analyze its data. An unselected dataset is unavailable to the task.".into(),p));
        } else {
            body = body.push(caption("The assistant can create a dataset in your capture folder for the sources you choose.".into(),p));
        }
        if state.dataset.is_some()
            && context
                .snapshot
                .dataset
                .as_ref()
                .and_then(|dataset| dataset.slot_id.as_ref())
                != state.dataset.as_ref()
        {
            body = body.push(field_error(
                "The selected dataset was closed or replaced. Select the current dataset again.",
                p,
            ));
            if context.snapshot.dataset.is_none() {
                body = body.push(scope_check(
                    "Remove unavailable dataset selection".into(),
                    true,
                    editable.then_some(tasks::Input::UseDataset(false)),
                ));
            }
        }
    }
    body = body.push(checkbox_maybe(
        "Allow source values and dataset results to reach the AI provider",state.allow_data,
        editable.then_some(tasks::Input::AllowData),
    )).push(caption(
        format!("Only the sources and dataset you select are available. Their tag names, values, notes and query rows may be sent to {}. Source locations are redacted; credentials stay local.",state.provider_label()),p,
    ));
    body.into()
}

const LIMIT_FIELDS: [(&str, &str, &str); 19] = [
    ("max_rounds", "Rounds", "From 1 to 1,000."),
    (
        "max_wall_ms",
        "Time",
        "From 1m to 24h. Includes pauses and approval waits.",
    ),
    ("max_input_tokens", "Input tokens", "From 1 to 50,000,000."),
    ("max_output_tokens", "Output tokens", "From 1 to 5,000,000."),
    (
        "max_cost_usd",
        "Cost in US dollars",
        "From 0 to 500, or empty when model prices are unknown.",
    ),
    ("max_images", "Images", "From 0 to 2,000."),
    ("max_captures", "Captures", "From 0 to 100,000."),
    (
        "max_bytes",
        "Data",
        "A positive reservation, such as 2 GiB, for photos, recordings, signal payload and two copies of each export bundle. Failed attempts stay charged. Dataset overhead, query scratch and filesystem overhead are separate.",
    ),
    ("max_recordings", "Recordings", "From 0 to 32."),
    ("max_recording_ms", "Recording duration", "From 1s to 7d."),
    (
        "max_sources",
        "Source captures",
        "From 0 to 256. Restarting a capture uses another allowance.",
    ),
    (
        "max_samples",
        "Signal samples",
        "From 0 to 100,000,000 individual tag values.",
    ),
    (
        "max_data_bytes",
        "Reserved data",
        "Signal payload plus twice each export bundle limit for staged and published copies, such as 512 MiB. Failed attempts stay charged. Also counts toward Data; database, scratch and filesystem overhead are separate.",
    ),
    (
        "max_signal_ms",
        "Total signal duration",
        "From 0s to 7d, summed across all source captures.",
    ),
    (
        "max_setting_changes",
        "Setting changes",
        "From 0 to 10,000.",
    ),
    (
        "max_consecutive_errors",
        "Consecutive errors",
        "From 1 to 5.",
    ),
    ("max_identical_calls", "Identical calls", "From 1 to 3."),
    ("approval_timeout_ms", "Approval timeout", "From 1s to 24h."),
    (
        "min_round_interval_ms",
        "Time between rounds",
        "From 1s to 24h.",
    ),
];

fn limit_field<'a>(
    state: &'a tasks::State,
    key: &'static str,
    label: &'static str,
    help: &'static str,
    p: &'static Palette,
) -> Element<'a, tasks::Input> {
    let value = state
        .limit_drafts
        .get(key)
        .cloned()
        .unwrap_or_else(|| limit_text(&state.limits, key));
    let issue = state.limit_error(key);
    let invalid = issue.is_some();
    let field = text_input::text_input("", &value)
        .id(format!("assistant-limit-{key}"))
        .name(format!("Assistant task limit: {label}"))
        .description(issue.as_deref().unwrap_or(help))
        .invalid(issue.clone())
        .size(style::BODY)
        .padding([6, 9])
        .width(Fill)
        .style(if invalid {
            style::input_invalid
        } else {
            style::input
        })
        .on_input_maybe(
            state
                .editable()
                .then_some(move |value| tasks::Input::Limit {
                    field: key.into(),
                    value,
                }),
        )
        .on_submit(tasks::Input::CommitLimit(key.into()));
    let mut field = column![
        text(label).size(style::SMALL).color(p.secondary),
        field,
        caption(help.into(), p),
    ]
    .spacing(4);
    if let Some(issue) = issue {
        field = field.push(field_error(issue, p));
    }
    field.into()
}

fn limit_text(limits: &Limits, key: &str) -> String {
    let millis = match key {
        "max_wall_ms" => Some(limits.max_wall_ms),
        "max_recording_ms" => Some(limits.max_recording_ms),
        "max_signal_ms" => Some(limits.max_signal_ms),
        "approval_timeout_ms" => Some(limits.approval_timeout_ms),
        "min_round_interval_ms" => Some(limits.min_round_interval_ms),
        _ => None,
    };
    if let Some(ms) = millis {
        if ms == 0 {
            return "0s".into();
        }
        return if ms.is_multiple_of(3_600_000) {
            format!("{}h", ms / 3_600_000)
        } else if ms.is_multiple_of(60_000) {
            format!("{}m", ms / 60_000)
        } else {
            format!("{}s", ms as f64 / 1000.0)
        };
    }
    match key {
        "max_rounds" => limits.max_rounds.to_string(),
        "max_input_tokens" => limits.max_input_tokens.to_string(),
        "max_output_tokens" => limits.max_output_tokens.to_string(),
        "max_cost_usd" => limits
            .max_cost_usd
            .map(|cost| cost.to_string())
            .unwrap_or_default(),
        "max_images" => limits.max_images.to_string(),
        "max_captures" => limits.max_captures.to_string(),
        "max_bytes" => format!("{} B", limits.max_bytes),
        "max_sources" => limits.max_sources.to_string(),
        "max_samples" => limits.max_samples.to_string(),
        "max_data_bytes" => format!("{} B", limits.max_data_bytes),
        "max_recordings" => limits.max_recordings.to_string(),
        "max_setting_changes" => limits.max_setting_changes.to_string(),
        "max_consecutive_errors" => limits.max_consecutive_errors.to_string(),
        "max_identical_calls" => limits.max_identical_calls.to_string(),
        _ => String::new(),
    }
}

fn status<'a>(
    state: &'a tasks::State,
    p: &'static Palette,
    spin: usize,
) -> Element<'a, tasks::Input> {
    let label = state.status_label();
    let (color, mark) = match state.snapshot.state {
        RunState::WaitingApproval | RunState::Paused | RunState::NeedsUser => (
            p.ink(p.warn),
            icon(Icon::WarningTriangle, 12.0, p.ink(p.warn)),
        ),
        RunState::Failed | RunState::LimitReached => {
            (p.ink(p.danger), icon(Icon::Warning, 12.0, p.ink(p.danger)))
        }
        RunState::Finished => (p.ink(p.live), icon(Icon::Check, 12.0, p.ink(p.live))),
        _ if state.busy() => (p.text, icon::spinner(12.0, p.secondary, spin)),
        _ => (p.secondary, icon(Icon::Timer, 12.0, p.secondary)),
    };
    let pill = container(
        row![mark, text(label.clone()).size(style::SMALL).color(color)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .padding([4, 8])
    .style(style::pill(color));
    accessibility::label(
        row![
            pill,
            space().width(Fill),
            text(elapsed(state.snapshot.elapsed_ms))
                .size(style::SMALL)
                .color(p.secondary),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .into(),
        format!("Assistant task: {label}"),
        accessibility::Role::Status,
    )
}

fn overview<'a>(
    state: &'a tasks::State,
    p: &'static Palette,
    stacked: bool,
) -> Element<'a, tasks::Input> {
    let live: Element<'a, tasks::Input> = if state.cameras.is_empty() {
        data_progress(state, p)
    } else if !state.data_sources.is_empty() || state.dataset.is_some() {
        column![latest(state, p), data_progress(state, p)]
            .spacing(14)
            .into()
    } else {
        latest(state, p)
    };
    if stacked {
        column![live, budgets(state, p)]
            .spacing(14)
            .width(Fill)
            .into()
    } else {
        row![container(live).width(240), budgets(state, p)]
            .spacing(18)
            .align_y(Alignment::Start)
            .width(Fill)
            .into()
    }
}

pub(super) fn data_progress<'a>(
    state: &'a tasks::State,
    p: &'static Palette,
) -> Element<'a, tasks::Input> {
    let mut body = column![heading("Data capture", p)].spacing(8).width(Fill);
    if let Some(receipt) = state
        .outcome
        .as_ref()
        .and_then(|outcome| outcome.task_data.as_ref())
    {
        let removed = receipt.phase == crate::session::task_data::TaskDataPhase::Removed;
        let complete = receipt.phase == crate::session::task_data::TaskDataPhase::Retained
            && receipt.catalog.closed
            && receipt.error.is_none()
            && !receipt.catalog.quarantined
            && receipt.catalog.dataset.last_error.is_none()
            && receipt.sink.error.is_none()
            && receipt.sink.dropped_total == 0;
        body = body.push(heading("Task dataset", p));
        body = body.push(status_line(
            if removed {
                "Dataset removed"
            } else if complete {
                "Dataset closed and retained"
            } else {
                "Dataset persistence or shutdown needs review"
            }
            .into(),
            if removed || complete {
                Level::Done
            } else {
                Level::Warning
            },
            p,
        ));
        body = body.push(
            text(&receipt.catalog.dataset.path)
                .font(style::MONO)
                .size(style::SMALL)
                .wrapping(text::Wrapping::WordOrGlyph),
        );
        body = body.push(caption(
            format!(
                "{} stored samples · {} signals · {} events",
                format::grouped(receipt.catalog.dataset.samples),
                format::grouped(receipt.catalog.dataset.signals),
                format::grouped(receipt.catalog.dataset.events)
            ),
            p,
        ));
        if !removed {
            body = body.push(caption(
                format!(
                    "{} logical file extents · {} reservation retained until removal",
                    crate::analytics::human_bytes(receipt.catalog.extents.used),
                    crate::analytics::human_bytes(receipt.catalog.extents.limit)
                ),
                p,
            ));
        }
        let mut errors = std::collections::BTreeSet::new();
        if receipt.sink.dropped_total > 0 {
            body = body.push(status_line(
                format!(
                    "Task writer reports {} dropped samples.",
                    format::grouped(receipt.sink.dropped_total)
                ),
                Level::Warning,
                p,
            ));
        }
        for error in [
            receipt.error.as_ref(),
            receipt.catalog.dataset.last_error.as_ref(),
            receipt.sink.error.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            if errors.insert(error) {
                body = body.push(status_line(
                    tools::redact::for_output_text(error),
                    Level::Error,
                    p,
                ));
            }
        }
    }
    if let Some(outcome) = state
        .outcome
        .as_ref()
        .filter(|outcome| !outcome.signal_reports.is_empty())
    {
        for report in &outcome.signal_reports {
            body = body.push(
                text(&report.source)
                    .size(style::SMALL)
                    .wrapping(text::Wrapping::WordOrGlyph),
            );
            let status = if report.completed_ms.is_none() {
                "Final persistence unverified"
            } else if report.state == crate::signals::SourceState::Failed {
                "Source failed"
            } else if report.persistence_complete {
                "All acquired samples saved"
            } else {
                "Persistence incomplete"
            };
            body = body.push(caption(status.into(), p));
            body = body.push(caption(
                format!(
                    "{} acquired · {} persisted",
                    format::grouped(report.samples),
                    format::grouped(report.written)
                ),
                p,
            ));
            body = body.push(caption(
                format!(
                    "{} payload · {} written",
                    crate::analytics::human_bytes(report.payload_bytes),
                    crate::analytics::human_bytes(report.written_bytes)
                ),
                p,
            ));
            if report.dropped > 0 || report.helper_dropped > 0 {
                body = body.push(status_line(
                    format!(
                        "{} dropped · {} helper drops",
                        format::grouped(report.dropped),
                        format::grouped(report.helper_dropped)
                    ),
                    Level::Warning,
                    p,
                ));
            }
            if let Some(error) = &report.error {
                body = body.push(status_line(error.clone(), Level::Error, p));
            }
        }
        return body.into();
    }
    if state.separate_dataset {
        if state
            .outcome
            .as_ref()
            .is_none_or(|outcome| outcome.task_data.is_none())
        {
            body = body.push(caption(if state.outcome.is_some() { "No final task dataset receipt was returned. Review the task error and storage status." } else { "Captures use this task's separate dataset. Its path and persistence receipt appear when the task ends." }.into(), p));
        }
        return body.into();
    }
    let Some(context) = &state.context else {
        return body
            .push(caption("Waiting for capture status…".into(), p))
            .into();
    };
    if let Some(sink) = &context.snapshot.signal_sink {
        if !sink.attached {
            body = body.push(status_line(
                "No signal writer is attached. Open an approved dataset to save samples.".into(),
                Level::Warning,
                p,
            ));
        } else if state.dataset.is_some() && sink.dataset_slot_id != state.dataset {
            body = body.push(status_line(
                "The signal writer belongs to a different dataset. Review this task's data scope."
                    .into(),
                Level::Warning,
                p,
            ));
        }
        if let Some(error) = &sink.error {
            body = body.push(status_line(
                format!("Session writer: {}", tools::redact::for_output_text(error)),
                Level::Error,
                p,
            ));
        }
        if sink.dropped_total > 0 {
            body = body.push(caption(
                format!(
                    "Session writer reports {} dropped samples.",
                    format::grouped(sink.dropped_total)
                ),
                p,
            ));
        }
    }
    if let Some(dataset) = &context.snapshot.dataset {
        body = body.push(caption(
            format!(
                "{} · {} stored samples",
                dataset.name,
                format::grouped(dataset.samples)
            ),
            p,
        ));
        if let Some(error) = &dataset.last_error {
            body = body.push(status_line(
                tools::redact::for_output_text(error),
                Level::Error,
                p,
            ));
        }
    }
    let mut found = false;
    for source in context
        .snapshot
        .signals
        .iter()
        .filter(|source| state.source_selected(&source.id))
        .take(8)
    {
        found = true;
        let status = if source.completed_ms.is_some() {
            if source.persistence_complete {
                "Saved"
            } else {
                "Persistence incomplete"
            }
        } else {
            match source.state {
                crate::signals::SourceState::Connecting => "Connecting",
                crate::signals::SourceState::Running => "Capturing",
                crate::signals::SourceState::Reconnecting => "Reconnecting",
                crate::signals::SourceState::Failed => "Failed",
                crate::signals::SourceState::Stopped => "Flushing",
            }
        };
        body = body.push(
            text(&source.id)
                .size(style::SMALL)
                .wrapping(text::Wrapping::WordOrGlyph),
        );
        body = body.push(caption(
            format!(
                "{status} · {} acquired · {} written",
                format::grouped(source.samples),
                format::grouped(source.written)
            ),
            p,
        ));
        if let Some(last) = source.last_sample_ms {
            body = body.push(caption(
                format!("Last sample {}", local_time(state, last)),
                p,
            ));
        }
        if source.dropped > 0 || source.helper_dropped > 0 || source.restarts > 0 {
            body = body.push(caption(
                format!(
                    "{} dropped · {} helper drops · {} restarts",
                    format::grouped(source.dropped),
                    format::grouped(source.helper_dropped),
                    source.restarts
                ),
                p,
            ));
        }
        if let Some(error) = source.persistence_error.as_ref().or(source.error.as_ref()) {
            body = body.push(status_line(
                tools::redact::for_output_text(error),
                Level::Error,
                p,
            ));
        }
        for tag in source.tags.iter().take(3) {
            let value = crate::signals::truncate(
                &tools::redact::for_output_text(&tag.value.to_string()),
                120,
            );
            body = body.push(caption(
                format!(
                    "{}: {}{} · {:?}",
                    tools::redact::for_output_text(&tag.tag),
                    value,
                    tag.unit
                        .as_ref()
                        .map(|unit| format!(" {unit}"))
                        .unwrap_or_default(),
                    tag.quality
                ),
                p,
            ));
        }
    }
    if !found && !state.data_sources.is_empty() {
        body = body.push(caption(if state.outcome.is_some() {"The source is no longer active. Review the task outcome and saved dataset for persistence."}else{"Waiting for the assistant to start an approved source. Connections and captures appear here."}.into(),p));
    }
    body.into()
}

fn latest<'a>(state: &'a tasks::State, p: &'static Palette) -> Element<'a, tasks::Input> {
    let mut view = column![heading("Latest view", p)].spacing(6).width(Fill);
    if let Some(sent) = &state.latest {
        let label = format!(
            "{} · exact image sent to the assistant · {} × {}",
            sent.part.label, sent.part.width, sent.part.height,
        );
        let preview: Element<'a, tasks::Input> = if let Some(handle) = &sent.handle {
            image(handle.clone())
                .width(240)
                .height(150)
                .content_fit(iced::ContentFit::Contain)
                .into()
        } else {
            caption("Sent image preview unavailable.".into(), p)
        };
        view = view.push(accessibility::label(
            preview,
            label,
            accessibility::Role::Image,
        ));
        let camera = state.latest_camera.as_deref().unwrap_or(&sent.part.label);
        let seen = state
            .latest_at_ms
            .map(|at| format!(" · seen {}", local_time(state, at)))
            .unwrap_or_default();
        view = view.push(caption(format!("{camera}{seen}"), p));
    } else {
        view = view.push(well(
            column![
                icon(Icon::Eye, 20.0, p.secondary),
                text("Measurements only")
                    .size(style::BODY)
                    .color(p.secondary),
            ]
            .spacing(8)
            .into(),
        ));
    }
    if !state.metrics.is_empty() {
        view = view.push(caption(state.metrics.clone(), p));
    }
    view.into()
}

fn budgets<'a>(state: &'a tasks::State, p: &'static Palette) -> Element<'a, tasks::Input> {
    let totals = &state.snapshot.totals;
    let limits = &state.limits;
    let mut rows = column![heading("Task budget", p)].spacing(8).width(Fill);
    for (label, used, max, shown) in [
        (
            "Rounds",
            u64::from(state.snapshot.round.max(totals.rounds)),
            u64::from(limits.max_rounds),
            None,
        ),
        (
            "Time",
            state.snapshot.elapsed_ms,
            limits.max_wall_ms,
            Some((
                elapsed(state.snapshot.elapsed_ms),
                elapsed(limits.max_wall_ms),
            )),
        ),
        (
            "Images",
            u64::from(totals.images),
            u64::from(limits.max_images),
            None,
        ),
        (
            "Captures",
            u64::from(totals.captures),
            u64::from(limits.max_captures),
            None,
        ),
        (
            "Data",
            totals.bytes,
            limits.max_bytes,
            Some((
                crate::analytics::human_bytes(totals.bytes),
                crate::analytics::human_bytes(limits.max_bytes),
            )),
        ),
    ] {
        if state.cameras.is_empty() && matches!(label, "Images" | "Captures") {
            continue;
        }
        let (used_text, max_text) =
            shown.unwrap_or_else(|| (format::grouped(used), format::grouped(max)));
        rows = rows.push(meter(
            label,
            used as f64,
            max as f64,
            used_text,
            max_text,
            p,
        ));
    }
    if !state.data_sources.is_empty()
        || state.dataset.is_some()
        || totals.sources > 0
        || totals.data_bytes > 0
    {
        rows = rows.push(caption(
            "Data allowances reserve each action's maximum before it starts. Exports reserve two copies; failed attempts stay charged.".into(),
            p,
        ));
        for (label, used, max, shown) in [
            (
                "Source captures",
                u64::from(totals.sources),
                u64::from(limits.max_sources),
                None,
            ),
            ("Reserved samples", totals.samples, limits.max_samples, None),
            (
                "Reserved data",
                totals.data_bytes,
                limits.max_data_bytes,
                Some((
                    crate::analytics::human_bytes(totals.data_bytes),
                    crate::analytics::human_bytes(limits.max_data_bytes),
                )),
            ),
            (
                "Signal time",
                totals.signal_ms,
                limits.max_signal_ms,
                Some((elapsed(totals.signal_ms), elapsed(limits.max_signal_ms))),
            ),
        ] {
            let (used_text, max_text) =
                shown.unwrap_or_else(|| (format::grouped(used), format::grouped(max)));
            rows = rows.push(meter(
                label,
                used as f64,
                max as f64,
                used_text,
                max_text,
                p,
            ));
        }
    }
    if let (Some(cost), Some(max)) = (totals.cost_usd, limits.max_cost_usd) {
        rows = rows.push(meter(
            "Est. cost",
            cost,
            max,
            format!("${cost:.2}"),
            format!("${max:.2}"),
            p,
        ));
    } else {
        let used = totals
            .usage
            .input
            .saturating_add(totals.usage.cache_read)
            .saturating_add(totals.usage.cache_write)
            .saturating_add(totals.usage.output);
        let max = limits
            .max_input_tokens
            .saturating_add(limits.max_output_tokens);
        rows = rows.push(meter(
            "Tokens",
            used as f64,
            max as f64,
            format::grouped(used),
            format::grouped(max),
            p,
        ));
    }
    rows.into()
}

fn meter<'a>(
    label: &'static str,
    used: f64,
    max: f64,
    used_text: String,
    max_text: String,
    p: &'static Palette,
) -> Element<'a, tasks::Input> {
    let ratio = if max > 0.0 {
        used / max
    } else if used > 0.0 {
        1.0
    } else {
        0.0
    };
    let color = meter_color(ratio, p);
    let description = format!("{label} {used_text} of {max_text}");
    accessibility::meter(
        column![
            row![
                text(label).size(style::CAPTION).color(p.secondary),
                space().width(Fill),
                text(format!("{used_text} / {max_text}"))
                    .size(style::CAPTION)
                    .color(p.ink(color)),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
            progress_bar(0.0..=1.0, ratio.clamp(0.0, 1.0) as f32)
                .girth(4)
                .style(move |theme| iced::widget::progress_bar::Style {
                    bar: color.into(),
                    ..style::progress(theme)
                }),
        ]
        .spacing(3)
        .width(Fill)
        .into(),
        description,
        used,
        max,
    )
}

fn meter_color(ratio: f64, p: &Palette) -> Color {
    if ratio >= 1.0 {
        p.danger
    } else if ratio >= 0.8 {
        p.warn
    } else {
        p.accent
    }
}

fn timeline<'a>(state: &'a tasks::State, p: &'static Palette) -> Element<'a, tasks::Input> {
    let selected = usize::from(state.tab == tasks::Tab::Changes);
    let tabs = panel_tabs(
        "Assistant task activity",
        vec![
            (
                "Activity",
                Some(tasks::Input::Tab(tasks::Tab::Activity)),
                String::new(),
            ),
            (
                "Changes",
                Some(tasks::Input::Tab(tasks::Tab::Changes)),
                String::new(),
            ),
        ],
        selected,
        selected as f32,
    );
    let mut rows = column![].spacing(10).width(Fill);
    if selected == 0 {
        for entry in &state.activity {
            rows = rows.push(
                row![
                    text(local_time(state, entry.time_ms))
                        .size(style::CAPTION)
                        .color(p.secondary),
                    entry.level.mark(12.0, p),
                    text(&entry.summary)
                        .size(style::SMALL)
                        .wrapping(text::Wrapping::WordOrGlyph)
                        .width(Fill),
                ]
                .spacing(7)
                .align_y(Alignment::Start),
            );
        }
        if state.activity.is_empty() {
            rows = rows.push(caption(
                "The assistant's observations and actions appear here.".into(),
                p,
            ));
        }
    } else {
        for (index, change) in state.changes.iter().enumerate() {
            let mut row = column![
                text(format!(
                    "{} · {} · {}",
                    local_time(state, change.time_ms),
                    change.camera,
                    change.feature
                ))
                .size(style::SMALL)
                .font(style::SEMIBOLD)
                .wrapping(text::Wrapping::WordOrGlyph),
                text(format!("{} → {}", change.before, change.after))
                    .size(style::SMALL)
                    .wrapping(text::Wrapping::WordOrGlyph),
            ]
            .spacing(4);
            if change.can_restore {
                row = row.push(button(text("Restore").size(style::SMALL))
                    .name(format!("Restore {} on {}", change.feature, change.camera))
                    .description("Review the current value and original value before restoring this setting.")
                    .style(style::secondary).padding([5, 9])
                    .on_press_maybe((!state.active() && state.restore_preview.is_none()).then_some(tasks::Input::RestoreChange(index))));
            }
            rows = rows.push(well(row.into()));
        }
        if state.changes.is_empty() {
            rows = rows.push(caption(
                "The assistant has not changed any camera settings.".into(),
                p,
            ));
        }
    }
    let history = activity_scroll(
        scrollable(rows)
            .id(ACTIVITY_BODY)
            .anchor_y(scrollable::Anchor::End)
            .height(190)
            .width(Fill)
            .style(style::sheet_scroll)
            .into(),
    );
    column![tabs, history].spacing(10).width(Fill).into()
}

fn approval<'a>(
    state: &'a tasks::State,
    request: &'a crate::assistant::approval::ApprovalRequest,
    p: &'static Palette,
) -> Element<'a, tasks::Input> {
    let remaining = state.approval_remaining(state.now_ms);
    let secret = state.approval_secret.as_str();
    let secret_issue = if secret.len() > 4096 {
        Some("Enter a secret key of at most 4,096 bytes.".to_owned())
    } else if secret.chars().any(|c| matches!(c, '\r' | '\n' | '\0')) {
        Some("Enter a secret key without line breaks or null characters.".to_owned())
    } else {
        None
    };
    let secret_valid = !request.needs_secret || (!secret.is_empty() && secret_issue.is_none());
    let valid = !remaining.is_zero() && secret_valid;
    let mut card = column![
        row![
            icon(Icon::WarningTriangle, 16.0, p.ink(p.warn)),
            text(&request.title)
                .size(style::BODY)
                .font(style::SEMIBOLD)
                .wrapping(text::Wrapping::WordOrGlyph)
                .width(Fill),
        ]
        .spacing(8)
        .align_y(Alignment::Start),
        text(&request.summary)
            .size(style::BODY)
            .wrapping(text::Wrapping::WordOrGlyph),
    ]
    .spacing(9)
    .width(Fill);
    for (key, value) in &request.facts {
        card = card.push(caption(format!("{key}: {value}"), p));
    }
    for reason in &request.reasons {
        card = card.push(status_line(reason.clone(), Level::Warning, p));
    }
    if request.needs_secret {
        card = card
            .push(text("Secret key").size(style::SMALL).color(p.secondary))
            .push(text_input::text_input("Enter securely…", secret)
                .id("assistant-approval-secret")
                .name("Assistant approval secret key")
                .description("Used only for this checked destination. Never sent to the model or written to its log.")
                .invalid(secret_issue.clone())
                .secure(true).size(style::BODY).padding([7, 10]).width(Fill)
                .style(if secret_issue.is_some() { style::input_invalid } else { style::input })
                .on_input(|value| tasks::Input::ApprovalSecret(SecretDraft::from(value))));
        if let Some(issue) = secret_issue {
            card = card.push(field_error(issue, p));
        } else if secret.is_empty() {
            card = card.push(caption(
                "Enter the secret key before approving this destination.".into(),
                p,
            ));
        }
    }
    card = card.push(
        text_input::text_input("Tell the assistant why…", &state.approval_note)
            .id("assistant-approval-note")
            .name("Tell the assistant why")
            .description("Optional note sent to the assistant when you deny this action.")
            .size(style::BODY)
            .padding([7, 10])
            .width(Fill)
            .style(style::input)
            .on_input(tasks::Input::ApprovalNote),
    );
    card = card.push(caption(
        if remaining.is_zero() {
            "This approval expired. The assistant will continue without this action.".into()
        } else {
            format!(
                "Expires in {}",
                elapsed(remaining.as_millis().min(u128::from(u64::MAX)) as u64)
            )
        },
        p,
    ));
    let id = request.id;
    let mut actions = column![
        button(text("Approve").size(style::BODY))
            .name("Approve assistant action")
            .id("assistant-approval-approve")
            .style(style::primary)
            .padding([7, 12])
            .width(Fill)
            .on_key_press(no_return)
            .on_press_maybe(valid.then_some(tasks::Input::Approve {
                id,
                scope: ApprovalScope::Once
            })),
    ]
    .spacing(7)
    .width(Fill);
    if request.allow_run_scope {
        actions = actions.push(
            button(text("Approve for this task").size(style::BODY))
                .name("Approve this action for this task")
                .description(
                    "Applies only to this tool, camera, risk and destination during this task.",
                )
                .style(style::secondary)
                .padding([7, 12])
                .width(Fill)
                .on_key_press(no_return)
                .on_press_maybe(valid.then_some(tasks::Input::Approve {
                    id,
                    scope: ApprovalScope::ForRun,
                })),
        );
    }
    actions = actions.push(
        button(text("Deny").size(style::BODY))
            .name("Deny assistant action")
            .style(style::secondary)
            .padding([7, 12])
            .width(Fill)
            .on_press(tasks::Input::Deny(id)),
    );
    card = card.push(actions);
    let mut announcement = format!("Approval needed. {}. {}", request.title, request.summary);
    for (key, value) in &request.facts {
        announcement.push_str(&format!(" {key}: {value}."));
    }
    for reason in &request.reasons {
        announcement.push_str(&format!(" {reason}"));
    }
    accessibility::label(
        container(card)
            .padding(14)
            .style(style::well)
            .width(Fill)
            .into(),
        announcement,
        accessibility::Role::Alert,
    )
}

fn leftovers<'a>(state: &'a tasks::State, p: &'static Palette) -> Element<'a, tasks::Input> {
    let Some(outcome) = &state.outcome else {
        return space().into();
    };
    let mut card = column![heading("Needs review", p)].spacing(10).width(Fill);
    for (index, leftover) in outcome.leftovers.iter().enumerate() {
        if leftover.kind == "signal_persistence" {
            card = card.push(status_line(leftover.summary.clone(), Level::Warning, p));
            continue;
        }
        let label = match leftover.kind.as_str() {
            "recording" => "Stop recording",
            "schedule" => "Cancel schedule",
            "auto" | "auto_mode" => "Turn off auto mode",
            "signal_capture" => "Stop signal capture",
            _ => "Review cleanup",
        };
        card = card.push(
            column![
                text(&leftover.summary)
                    .size(style::SMALL)
                    .wrapping(text::Wrapping::WordOrGlyph),
                button(text(label).size(style::SMALL))
                    .name(format!("{label} on {}", leftover.camera))
                    .description("Review this action before it runs.")
                    .style(style::secondary)
                    .padding([6, 10])
                    .on_press_maybe(
                        state
                            .restore_preview
                            .is_none()
                            .then_some(tasks::Input::Cleanup(index))
                    ),
            ]
            .spacing(5),
        );
    }
    well(card.into())
}

fn restore_preview<'a>(
    preview: &'a tasks::RestorePreview,
    p: &'static Palette,
) -> Element<'a, tasks::Input> {
    let data = preview
        .calls
        .iter()
        .any(|call| call.tool == "stop_signal_capture");
    let mut card = column![heading(
        if data {
            "Review signal capture cleanup"
        } else {
            "Review camera changes"
        },
        p
    )]
    .spacing(9)
    .width(Fill);
    let mut ready = !preview.checked.is_empty();
    for checked in &preview.checked {
        ready &= checked.ready();
        card = card.push(
            text(&checked.title)
                .size(style::BODY)
                .font(style::SEMIBOLD)
                .wrapping(text::Wrapping::WordOrGlyph),
        );
        card = card.push(caption(checked.summary.clone(), p));
        for (key, value) in &checked.facts {
            card = card.push(caption(format!("{key}: {value}"), p));
        }
        for issue in &checked.issues {
            card = card.push(status_line(
                issue.message.clone(),
                if issue.is_error() {
                    Level::Error
                } else {
                    Level::Warning
                },
                p,
            ));
        }
        for reason in &checked.reasons {
            card = card.push(status_line(reason.clone(), Level::Warning, p));
        }
    }
    for skipped in &preview.skipped {
        card = card.push(status_line(
            format!(
                "{} · {}: {}",
                skipped.camera, skipped.feature, skipped.reason
            ),
            Level::Warning,
            p,
        ));
    }
    card = card.push(
        row![
            button(text("Cancel").size(style::BODY))
                .name(if data {
                    "Cancel reviewed signal cleanup"
                } else {
                    "Cancel reviewed camera changes"
                })
                .style(style::secondary)
                .padding([7, 12])
                .on_press(tasks::Input::CancelRestore),
            space().width(Fill),
            button(text("Apply changes").size(style::BODY))
                .name(if data {
                    "Apply reviewed signal cleanup"
                } else {
                    "Apply reviewed camera changes"
                })
                .style(style::primary)
                .padding([7, 12])
                .on_key_press(no_return)
                .on_press_maybe(ready.then_some(tasks::Input::ConfirmRestore)),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    );
    well(card.into())
}

fn footer<'a>(state: &'a tasks::State, p: &'static Palette) -> Element<'a, tasks::Input> {
    if state.active() {
        let pause = if matches!(state.snapshot.state, RunState::Paused | RunState::NeedsUser) {
            button(text("Resume").size(style::BODY))
                .name("Resume assistant task")
                .description(state.data_scope_issue().unwrap_or_else(|| {
                    "Continue within the task's original scope and limits.".into()
                }))
                .style(style::secondary)
                .padding([7, 10])
                .on_press_maybe(state.can_resume().then_some(tasks::Input::Resume))
        } else {
            button(text("Pause").size(style::BODY))
                .name("Pause assistant task")
                .style(style::secondary)
                .padding([7, 10])
                .on_press_maybe(state.can_pause().then_some(tasks::Input::Pause))
        };
        let send = !state.message.trim().is_empty() && state.snapshot.state != RunState::Stopping;
        return column![
            row![
                button(row![icon(Icon::Stop, 12.0, p.text), text("Stop").size(style::BODY)].spacing(6).align_y(Alignment::Center))
                    .id("assistant-task-stop").name("Stop assistant task")
                    .description("Stop the assistant and flush its owned signal captures. Recordings and image schedules may continue; review them after the task.")
                    .style(style::stop).padding([7, 10]).on_key_press(no_return).on_press(tasks::Input::Stop),
                pause,
                space().width(Fill),
                button(text("Take over").size(style::BODY)).name(if state.cameras.is_empty(){"Take over data capture control"}else{"Take over camera control"})
                    .description(if state.cameras.is_empty(){"Stop the assistant and flush its captures so you can review the data manually."}else{"Stop the assistant and open this camera in the inspector."})
                    .style(style::secondary).padding([7, 10]).on_key_press(no_return).on_press(tasks::Input::TakeOver(state.takeover_target())),
            ].spacing(7).align_y(Alignment::Center),
            row![
                text_input::text_input("Message the assistant…", &state.message)
                    .id("assistant-task-message").name("Message the assistant")
                    .description("Delivered at the next round. Return sends this message; it never stops or approves an action.")
                    .size(style::BODY).padding([7, 10]).width(Fill).style(style::input)
                    .on_input(tasks::Input::Message).on_submit_maybe(send.then_some(tasks::Input::SendMessage)),
                button(icon(Icon::PaperPlaneRight, 14.0, p.accent)).name("Send message to assistant task")
                    .style(style::secondary).padding(9).on_press_maybe(send.then_some(tasks::Input::SendMessage)),
            ].spacing(7).align_y(Alignment::Center),
        ].spacing(8).width(Fill).into();
    }
    if state.outcome.is_some() || !state.snapshot.run_id.is_empty() {
        let restore = state.changes.iter().any(|change| change.can_restore);
        let mut actions = column![].spacing(7).width(Fill);
        if restore {
            actions = actions.push(
                button(text("Restore settings…").size(style::BODY))
                    .name("Review assistant setting restoration")
                    .style(style::secondary)
                    .padding([7, 10])
                    .width(Fill)
                    .on_press_maybe(
                        state
                            .restore_preview
                            .is_none()
                            .then_some(tasks::Input::Restore),
                    ),
            );
        }
        return actions
            .push(
                row![
                    button(text("Export log…").size(style::BODY))
                        .name("Export assistant task log")
                        .style(style::plain)
                        .padding([7, 10])
                        .on_press(tasks::Input::ExportLog),
                    space().width(Fill),
                    button(text("New task").size(style::BODY))
                        .name("New assistant task")
                        .style(style::primary)
                        .padding([7, 12])
                        .on_press_maybe(
                            state
                                .restore_preview
                                .is_none()
                                .then_some(tasks::Input::NewTask)
                        ),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            )
            .into();
    }
    let mut footer = column![].spacing(7).width(Fill);
    if let Some(reason) = state.start_reason() {
        footer = footer.push(caption(reason, p));
    }
    let ready = state.valid_to_start();
    footer
        .push(
            button(
                row![
                    icon(
                        Icon::Sparkle,
                        14.0,
                        if ready { Color::WHITE } else { p.secondary }
                    ),
                    text("Start task").size(style::BODY)
                ]
                .spacing(7)
                .align_y(Alignment::Center),
            )
            .id("assistant-task-start")
            .name("Start assistant task")
            .description(
                "Begin this task with the cameras, data sources, dataset, destinations and hard limits shown above.",
            )
            .style(style::primary)
            .padding([8, 12])
            .width(Fill)
            .on_key_press(no_return)
            .on_press_maybe(ready.then_some(tasks::Input::Start)),
        )
        .into()
}

fn elapsed(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

fn local_time(state: &tasks::State, ms: u64) -> String {
    state
        .context
        .as_ref()
        .map(|context| context.zone.at(ms).format("%H:%M:%S").to_string())
        .unwrap_or_else(|| {
            chrono::DateTime::from_timestamp_millis(ms.min(i64::MAX as u64) as i64)
                .map(|time| time.format("%H:%M:%S").to_string())
                .unwrap_or_else(|| "—".into())
        })
}

fn well<'a>(body: Element<'a, tasks::Input>) -> Element<'a, tasks::Input> {
    container(body)
        .padding(14)
        .width(Fill)
        .style(style::well)
        .into()
}

fn scope_check<'a>(
    label: String,
    checked: bool,
    on: Option<tasks::Input>,
) -> Element<'a, tasks::Input> {
    let toggle = on.clone();
    let check = iced::widget::checkbox(checked)
        .label(label.clone())
        .on_toggle_maybe(on.map(|message| move |_| message.clone()))
        .size(16)
        .spacing(8)
        .text_size(style::BODY)
        .style(style::check);
    button(check)
        .name(label)
        .role(accessibility::Role::CheckBox)
        .checked(checked)
        .padding(0)
        .style(style::bare)
        .on_press_maybe(toggle)
        .into()
}

fn caption<'a>(message: String, p: &'static Palette) -> Element<'a, tasks::Input> {
    text(message)
        .size(style::SMALL)
        .color(p.secondary)
        .wrapping(text::Wrapping::WordOrGlyph)
        .into()
}

fn status_line<'a>(
    message: String,
    level: Level,
    p: &'static Palette,
) -> Element<'a, tasks::Input> {
    row![
        level.mark(12.0, p),
        text(message)
            .size(style::SMALL)
            .color(level.color(p))
            .wrapping(text::Wrapping::WordOrGlyph)
            .width(Fill),
    ]
    .spacing(7)
    .align_y(Alignment::Start)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_thresholds_have_warning_and_error_inks() {
        for p in [&style::LIGHT, &style::DARK] {
            assert_eq!(meter_color(0.799, p), p.accent);
            assert_eq!(meter_color(0.8, p), p.warn);
            assert_eq!(meter_color(1.0, p), p.danger);
            assert_eq!(meter_color(1.5, p), p.danger);
        }
    }

    #[test]
    fn limits_show_round_trippable_values_and_elapsed_time_is_precise() {
        let limits = Limits::default();
        assert_eq!(limit_text(&limits, "max_wall_ms"), "30m");
        assert_eq!(limit_text(&limits, "approval_timeout_ms"), "10m");
        assert_eq!(limit_text(&limits, "min_round_interval_ms"), "1s");
        assert_eq!(limit_text(&limits, "max_cost_usd"), "");
        assert_eq!(elapsed(372_000), "6:12");
        assert_eq!(elapsed(3_723_000), "1:02:03");
    }
}
