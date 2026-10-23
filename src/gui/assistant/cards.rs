//! Reviewable, schema-driven plan cards. A field can only edit the plan;
//! Return commits its draft and never starts camera work.
use super::{quick, settings::SecretDraft, *};
use crate::{
    assistant::agent::PlanStep,
    tools::{self, FieldKind, FieldSpec, StepId},
};
use iced::widget::column;
use serde_json::{Value, json};

pub(in crate::gui) fn field_id(step: StepId, key: &str) -> String {
    format!("step-{}-{key}", step.0)
}

pub(in crate::gui) fn step_focus_id(step: StepId) -> String {
    format!("step-{}-first", step.0)
}

pub(in crate::gui) fn focus_id(state: &quick::State, id: StepId) -> String {
    let Some(step) = state.plan.steps.iter().find(|step| step.id == id) else {
        return "quick-command-prompt".into();
    };
    blocking_focus_id(step)
}

pub(in crate::gui) fn blocking_focus_id(step: &PlanStep) -> String {
    let Some(spec) = tools::spec(&step.call.tool) else {
        return step_focus_id(step.id);
    };
    let first = spec.fields.first();
    let key = step.checked.as_ref().and_then(|checked| {
        checked
            .issues
            .iter()
            .find(|issue| issue.is_error())
            .and_then(|issue| issue.field.as_deref())
            .map(form_key)
    });
    let Some(key) = key else {
        return step_focus_id(step.id);
    };
    let key = key.as_str();
    let root = key.split('.').next().unwrap_or(key);
    let Some(field) = spec.fields.iter().find(|field| field.key == root) else {
        return step_focus_id(step.id);
    };
    let actual = if step.call.tool == "schedule_captures"
        && matches!(root, "duration" | "count" | "until")
    {
        ["duration", "count", "until"]
            .into_iter()
            .find(|key| !step.call.args[*key].is_null())
            .unwrap_or("duration")
            .to_owned()
    } else if key == root {
        match field.kind {
            FieldKind::Features | FieldKind::Tags | FieldKind::StringList
                if step.call.args[root].as_array().is_none_or(Vec::is_empty) =>
            {
                format!("{root}-add")
            }
            FieldKind::Features => format!("{root}.0.feature"),
            FieldKind::Tags => format!("{root}.0.key"),
            FieldKind::StringList => format!("{root}.0"),
            FieldKind::Region => format!("{root}.x"),
            _ => key.into(),
        }
    } else {
        key.into()
    };
    if first.is_some_and(|first| first.key == root) {
        step_focus_id(step.id)
    } else {
        field_id(step.id, &actual)
    }
}

pub(in crate::gui) fn content<'a>(
    state: &'a quick::State,
    p: &'static Palette,
) -> Element<'a, quick::Input> {
    column(
        state
            .plan
            .steps
            .iter()
            .enumerate()
            .map(|(index, step)| card(state, step, index, p)),
    )
    .spacing(14)
    .width(Fill)
    .into()
}

fn card<'a>(
    state: &'a quick::State,
    step: &'a PlanStep,
    index: usize,
    p: &'static Palette,
) -> Element<'a, quick::Input> {
    let checked = step.checked.as_ref();
    let spec = tools::spec(&step.call.tool);
    let title = checked.map_or_else(
        || spec.map_or("Camera action", |spec| spec.title),
        |checked| checked.title.as_str(),
    );
    let errors = checked.map_or(0, |checked| {
        checked
            .issues
            .iter()
            .filter(|issue| issue.is_error())
            .count()
    });
    let warnings =
        checked.is_some_and(|checked| checked.issues.iter().any(|issue| !issue.is_error()));
    let (status, level) = if let Some(status) = state.step_label(step.id) {
        (
            status.to_owned(),
            if state.step_error(step.id).is_some() {
                Level::Error
            } else if status == "Running" {
                Level::Warning
            } else {
                Level::Done
            },
        )
    } else if errors > 0 {
        ("Needs attention".into(), Level::Error)
    } else if warnings {
        ("Review warning".into(), Level::Warning)
    } else if checked.is_none() {
        ("Checking…".into(), Level::Warning)
    } else {
        ("Ready".into(), Level::Done)
    };
    let mark = match step.call.tool.as_str() {
        "start_recording" => Icon::Record,
        "schedule_captures" => Icon::CalendarBlank,
        "wait_for" => Icon::Timer,
        "stream_to_url" => Icon::Broadcast,
        "create_folder_destination" => Icon::FolderPlus,
        _ => Icon::Camera,
    };
    let heading = format!("Step {}: {title}", index + 1);
    let working = matches!(status.as_str(), "Running" | "Checking…");
    let status: Element<'_, quick::Input> = container(
        row![
            if working {
                icon(Icon::Timer, 11.0, p.secondary)
            } else {
                level.mark(11.0, p)
            },
            text(status).size(style::SMALL).color(if working {
                p.secondary
            } else {
                level.color(p)
            }),
        ]
        .spacing(5)
        .align_y(Alignment::Center),
    )
    .padding([3, 7])
    .style(style::pill(if working {
        p.secondary
    } else {
        level.color(p)
    }))
    .into();
    let remove = button(icon(Icon::Trash, style::BODY, p.secondary))
        .name(format!("Remove step {}", index + 1))
        .id(format!("step-{}-remove", step.id.0))
        .padding(6)
        .style(style::plain)
        .on_press_maybe(state.editable().then_some(quick::Input::Remove(step.id)));
    let mut body = column![
        row![
            icon(mark, 18.0, p.secondary),
            text(heading.clone())
                .size(style::BODY)
                .font(style::SEMIBOLD)
                .width(Fill),
            remove
        ]
        .spacing(8)
        .align_y(Alignment::Center),
        status,
    ]
    .spacing(10)
    .width(Fill);
    if checked.is_some_and(|checked| checked.leaves_computer) {
        body = body.push(
            container(
                row![
                    icon(Icon::CloudArrowUp, 12.0, p.ink(p.warn)),
                    text("Leaves this computer")
                        .size(style::SMALL)
                        .color(p.ink(p.warn))
                ]
                .spacing(5)
                .align_y(Alignment::Center),
            )
            .padding([3, 7])
            .style(style::pill(p.warn)),
        );
    }
    if step.changed_by_assistant {
        let fields = state.drafts.changed_fields(step.id);
        let text = if fields.is_empty() {
            "Changed by assistant. Review this step again.".into()
        } else {
            format!(
                "The assistant changed {}, which you edited.",
                fields.join(", ")
            )
        };
        body = body.push(caption(text, Level::Warning, p));
    }
    if let Some(error) = state.step_error(step.id) {
        body = body.push(caption(error.to_owned(), Level::Error, p));
    }
    if let Some(checked) = checked {
        for issue in &checked.issues {
            if card_issue(state, step, spec, issue) {
                body = body.push(caption(
                    issue.message.clone(),
                    if issue.is_error() {
                        Level::Error
                    } else {
                        Level::Warning
                    },
                    p,
                ));
            }
        }
    }
    if let Some(spec) = spec {
        let mut advanced = Vec::new();
        let mut first = true;
        for field in spec.fields {
            if step.call.tool == "schedule_captures"
                && matches!(field.key, "duration" | "count" | "until")
            {
                if field.key == "duration" {
                    body = body.push(schedule_ends(state, step, p));
                }
                continue;
            }
            if core_field(spec, field) {
                body = body.push(field_view(state, step, field, first, p));
                first = false;
            } else {
                advanced.push(field);
            }
        }
        if !advanced.is_empty() {
            let forced = advanced.iter().any(|field| {
                state.drafts.has_draft(step.id, field.key)
                    || field_issue(state, step, field.key).is_some()
            });
            let open = forced || state.options_open.contains(&step.id);
            body = body.push(disclosure(
                "Options",
                None,
                if open { 1.0 } else { 0.0 },
                quick::Input::Options(step.id, !open),
                p,
            ));
            if open {
                for field in advanced {
                    body = body.push(field_view(state, step, field, false, p));
                }
            }
        }
    }
    if let Some(checked) = checked {
        if !checked.facts.is_empty() {
            let facts = checked.facts.iter().map(|(name, value)| {
                text(format!("{name}: {value}"))
                    .size(style::SMALL)
                    .color(p.secondary)
                    .wrapping(text::Wrapping::WordOrGlyph)
                    .into()
            });
            body = body.push(column(facts).spacing(4));
        }
        for reason in &checked.reasons {
            body = body.push(caption(reason.clone(), Level::Warning, p));
        }
    }
    let details = state.details_open.contains(&step.id);
    body = body.push(disclosure(
        "Details",
        Some(step.call.tool.clone()),
        if details { 1.0 } else { 0.0 },
        quick::Input::Details(step.id, !details),
        p,
    ));
    if details {
        let value = tools::redact::for_output(json!({"tool":step.call.tool,"args":step.call.args}));
        let json = serde_json::to_string_pretty(&value).unwrap_or_default();
        body = body.push(
            container(
                column![
                    text(json.clone())
                        .size(style::SMALL)
                        .font(style::MONO)
                        .wrapping(text::Wrapping::WordOrGlyph),
                    button(text("Copy details").size(style::SMALL))
                        .name(format!("Copy tool details for step {}", index + 1))
                        .padding([4, 8])
                        .style(style::plain)
                        .on_press(quick::Input::Copy(json))
                ]
                .spacing(6),
            )
            .padding(10)
            .width(Fill)
            .style(style::code),
        );
    }
    if state.thinking() {
        body = body.push(
            text("Waiting for the assistant…")
                .size(style::SMALL)
                .color(p.secondary),
        );
    }
    accessibility::group(
        container(body)
            .padding(14)
            .width(Fill)
            .style(style::well)
            .into(),
        heading,
        accessibility::Role::Group,
    )
}

fn core_field(spec: &tools::ToolSpec, field: &FieldSpec) -> bool {
    !field.optional
        || field.ui_only
        || spec.kind == tools::ToolKind::Configure
        || matches!(
            field.key,
            "camera"
                | "destination"
                | "count"
                | "duration"
                | "every"
                | "start_at"
                | "until"
                | "file_name"
                | "url"
                | "username"
        )
}

fn card_issue(
    state: &quick::State,
    step: &PlanStep,
    spec: Option<&tools::ToolSpec>,
    issue: &tools::Issue,
) -> bool {
    if !issue.is_error() {
        return true;
    }
    let Some(key) = issue.field.as_deref() else {
        return true;
    };
    let key = form_key(key);
    let Some(field) = spec.and_then(|spec| {
        spec.fields
            .iter()
            .find(|field| field.key == key.split('.').next().unwrap_or(&key))
    }) else {
        return true;
    };
    !state.drafts.visible(step.id, &key, &step.call.args) && field.kind != FieldKind::Bool
}

fn form_key(key: &str) -> String {
    key.replace('[', ".").replace(']', "")
}

fn field_issue<'a>(state: &'a quick::State, step: &'a PlanStep, key: &str) -> Option<&'a str> {
    state.drafts.issue(step.id, key).or_else(|| {
        step.checked.as_ref().and_then(|checked| {
            checked
                .issues
                .iter()
                .find(|issue| {
                    issue.is_error()
                        && issue.field.as_deref().is_some_and(|field| {
                            let field = form_key(field);
                            field == key
                                || !key.contains('.') && field.starts_with(&format!("{key}."))
                        })
                })
                .map(|issue| issue.message.as_str())
        })
    })
}

fn field_view<'a>(
    state: &'a quick::State,
    step: &'a PlanStep,
    field: &'static FieldSpec,
    first: bool,
    p: &'static Palette,
) -> Element<'a, quick::Input> {
    match field.kind {
        FieldKind::Features | FieldKind::Tags => list_field(state, step, field, first, p),
        FieldKind::StringList => string_list_field(state, step, field, first, p),
        FieldKind::Region => {
            let mut out =
                column![text(field.label).size(style::SMALL).color(p.secondary)].spacing(6);
            for (index, (key, label)) in [
                ("x", "Left"),
                ("y", "Top"),
                ("width", "Width"),
                ("height", "Height"),
            ]
            .into_iter()
            .enumerate()
            {
                out = out.push(text_field(
                    state,
                    step,
                    field,
                    (
                        &format!("{}.{key}", field.key),
                        label,
                        first && index == 0,
                        false,
                    ),
                    p,
                ));
            }
            out = out.push(
                button(text("Use whole frame").size(style::SMALL))
                    .name(format!("Use the whole frame in step {}", step.id.0))
                    .padding([5, 10])
                    .style(style::secondary)
                    .on_press_maybe(state.editable().then_some(quick::Input::Field {
                        step: step.id,
                        key: field.key.into(),
                        value: String::new(),
                    })),
            );
            out.into()
        }
        FieldKind::Camera | FieldKind::Destination | FieldKind::Choice(_) => {
            choice_field(state, step, field, first, p)
        }
        FieldKind::Bool => {
            let checked = step.call.args[field.key].as_bool().unwrap_or(false);
            let message = quick::Input::Bool {
                step: step.id,
                key: field.key.into(),
                value: !checked,
            };
            let check = iced::widget::checkbox(checked)
                .label(field.label)
                .on_toggle_maybe(state.editable().then_some(move |value| quick::Input::Bool {
                    step: step.id,
                    key: field.key.into(),
                    value,
                }))
                .size(16)
                .spacing(8)
                .text_size(style::BODY)
                .style(style::check);
            column![
                button(check)
                    .name(format!("Step {} {}", step.id.0, field.label))
                    .description(field.help)
                    .id(widget_id(step.id, field.key, first))
                    .role(accessibility::Role::CheckBox)
                    .checked(checked)
                    .padding(0)
                    .style(style::bare)
                    .on_press_maybe(state.editable().then_some(message)),
                text(field.help).size(style::SMALL).color(p.secondary)
            ]
            .spacing(4)
            .into()
        }
        FieldKind::Secret => {
            text_field(state, step, field, (field.key, field.label, first, true), p)
        }
        _ => text_field(
            state,
            step,
            field,
            (field.key, field.label, first, false),
            p,
        ),
    }
}

fn widget_id(step: StepId, key: &str, first: bool) -> String {
    if first {
        step_focus_id(step)
    } else {
        field_id(step, key)
    }
}

fn text_field<'a>(
    state: &'a quick::State,
    step: &'a PlanStep,
    field: &FieldSpec,
    control: (&str, &str, bool, bool),
    p: &'static Palette,
) -> Element<'a, quick::Input> {
    let (key, label, first, secret) = control;
    let raw = if secret {
        state.drafts.secret_text(step.id, key).to_owned()
    } else {
        state.drafts.text(step.id, key, &step.call.args)
    };
    let issue = field_issue(state, step, key)
        .filter(|_| state.drafts.visible(step.id, key, &step.call.args) || !raw.is_empty())
        .map(str::to_owned);
    let id = step.id;
    let editable = state.editable();
    let invalid = issue.is_some();
    let dirty = state.drafts.changed(step.id, key);
    let edit_key = key.to_owned();
    let commit_key = key.to_owned();
    let input = text_input(
        if field.optional {
            "Optional"
        } else {
            "Enter a value"
        },
        &raw,
    )
    .name(format!("Step {} {label}", id.0))
    .description(field.help)
    .invalid(issue.clone())
    .id(widget_id(id, key, first))
    .secure(secret)
    .on_input_maybe(state.editable().then_some(move |value: String| {
        if secret {
            quick::Input::Secret {
                step: id,
                key: edit_key.clone(),
                value: SecretDraft::from(value),
            }
        } else {
            quick::Input::Field {
                step: id,
                key: edit_key.clone(),
                value,
            }
        }
    }))
    .on_submit(quick::Input::Commit {
        step: id,
        key: commit_key,
    })
    .padding([6, 10])
    .size(style::BODY)
    .style(move |theme, status| field_style(theme, status, editable, invalid, dirty));
    let mut out = column![
        text(label.to_owned()).size(style::SMALL).color(p.secondary),
        input
    ]
    .spacing(6);
    if let Some(issue) = issue {
        out = out.push(if state.drafts.just_edited(id, key) {
            field_error(issue, p)
        } else {
            caption(issue, Level::Error, p)
        });
    } else if !field.help.is_empty() {
        out = out.push(
            text(field.help.to_owned())
                .size(style::SMALL)
                .color(p.secondary),
        );
    }
    if field.kind == FieldKind::Duration
        && let Ok(millis) = tools::parse_duration_field(&raw)
    {
        out = out.push(
            row![
                icon(Icon::Timer, 11.0, p.secondary),
                text(crate::scheduling::format_duration(millis))
                    .size(style::SMALL)
                    .color(p.secondary)
            ]
            .spacing(5)
            .align_y(Alignment::Center),
        );
    }
    if field.kind == FieldKind::Time
        && let Some(caption) = friendly_time(&raw, state.context.as_ref())
    {
        out = out.push(text(caption).size(style::SMALL).color(p.secondary));
    }
    out.into()
}

#[derive(Clone, PartialEq, Eq)]
struct Choice {
    value: String,
    label: String,
}
impl fmt::Display for Choice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

fn choice_field<'a>(
    state: &'a quick::State,
    step: &'a PlanStep,
    field: &FieldSpec,
    first: bool,
    p: &'static Palette,
) -> Element<'a, quick::Input> {
    let value = step.call.args[field.key].as_str().unwrap_or("");
    let selected_value = if field.kind == FieldKind::Camera {
        state
            .context
            .as_ref()
            .and_then(|context| tools::resolve_camera(context, field.key, value, true).ok())
            .map(|camera| camera.selector().to_owned())
            .unwrap_or_else(|| value.to_owned())
    } else {
        value.to_owned()
    };
    let mut choices = Vec::new();
    if field.optional {
        choices.push(Choice {
            value: String::new(),
            label: if field.kind == FieldKind::Destination {
                "Capture folder".into()
            } else {
                "Use default".into()
            },
        });
    }
    match field.kind {
        FieldKind::Camera => {
            if let Some(context) = &state.context {
                for camera in context
                    .snapshot
                    .cameras
                    .iter()
                    .map(|camera| &camera.info)
                    .chain(&context.snapshot.devices)
                {
                    if !choices.iter().any(|entry| entry.value == camera.id) {
                        choices.push(Choice {
                            value: camera.id.clone(),
                            label: format!(
                                "{} · {}",
                                tools::camera_name(context, camera),
                                camera.id
                            ),
                        });
                    }
                }
            }
        }
        FieldKind::Destination => {
            if let Some(context) = &state.context {
                for destination in &context.destinations {
                    choices.push(Choice {
                        value: destination.name.clone(),
                        label: destination.name.clone(),
                    });
                }
            }
            for preceding in state
                .plan
                .steps
                .iter()
                .take_while(|previous| previous.id != step.id)
            {
                if matches!(preceding.call.tool.as_str(), "create_folder_destination")
                    && let Some(name) = preceding.call.args["name"].as_str()
                    && !choices.iter().any(|choice| choice.value == name)
                {
                    choices.push(Choice {
                        value: name.into(),
                        label: format!("{name} (planned)"),
                    });
                }
            }
        }
        FieldKind::Choice(values) => choices.extend(values.iter().map(|(value, label)| Choice {
            value: (*value).into(),
            label: (*label).into(),
        })),
        _ => {}
    }
    let unavailable =
        !value.is_empty() && !choices.iter().any(|choice| choice.value == selected_value);
    if unavailable {
        choices.insert(
            0,
            Choice {
                value: value.into(),
                label: format!("{} (not available)", tools::redact::for_output_text(value)),
            },
        );
    }
    let selected = choices
        .iter()
        .find(|choice| choice.value == selected_value)
        .cloned();
    if field.kind == FieldKind::Destination {
        choices.push(Choice {
            value: "\0new".into(),
            label: "New destination…".into(),
        });
    }
    let issue = field_issue(state, step, field.key)
        .filter(|_| state.drafts.visible(step.id, field.key, &step.call.args))
        .map(str::to_owned)
        .or_else(|| {
            unavailable.then(|| format!("Choose an available {}.", field.label.to_lowercase()))
        });
    let id = step.id;
    let key = field.key.to_owned();
    let picker = pick_list(choices, selected, move |choice: Choice| {
        if choice.value == "\0new" {
            quick::Input::NewDestination
        } else {
            quick::Input::Choice {
                step: id,
                key: key.clone(),
                value: choice.value,
            }
        }
    })
    .name(format!("Step {} {}", id.0, field.label))
    .description(field.help)
    .invalid(issue.clone())
    .id(widget_id(id, field.key, first))
    .enabled(state.editable())
    .width(Fill)
    .padding([6, 10])
    .text_size(style::BODY)
    .style(if issue.is_some() {
        style::pick_invalid
    } else if state.drafts.changed(step.id, field.key) {
        style::pick_dirty
    } else {
        style::pick
    })
    .menu_style(style::menu);
    let mut out = column![
        text(field.label).size(style::SMALL).color(p.secondary),
        picker
    ]
    .spacing(6);
    if let Some(issue) = issue {
        out = out.push(if state.drafts.just_edited(id, field.key) {
            field_error(issue, p)
        } else {
            caption(issue, Level::Error, p)
        });
    } else {
        out = out.push(text(field.help).size(style::SMALL).color(p.secondary));
    }
    out.into()
}

fn schedule_ends<'a>(
    state: &'a quick::State,
    step: &'a PlanStep,
    p: &'static Palette,
) -> Element<'a, quick::Input> {
    let kinds = [
        ("duration", "After a duration"),
        ("count", "After photos"),
        ("until", "At a time"),
    ];
    let selected = kinds
        .iter()
        .position(|(key, _)| !step.call.args[*key].is_null())
        .unwrap_or(0);
    let count = kinds
        .iter()
        .filter(|(key, _)| !step.call.args[*key].is_null())
        .count();
    let items = kinds
        .iter()
        .map(|(key, label)| {
            (
                *label,
                state.editable().then_some(quick::Input::ScheduleEnd {
                    step: step.id,
                    kind: (*key).into(),
                }),
                format!("End the schedule {}", label.to_lowercase()),
            )
        })
        .collect();
    let mut out = column![
        text("Ends").size(style::SMALL).color(p.secondary),
        segmented("Schedule ends", items, selected, selected as f32)
    ]
    .spacing(6);
    if count > 1 {
        out = out.push(caption(
            "Choose only one way to end this schedule.".into(),
            Level::Error,
            p,
        ));
    }
    if let Some(field) = tools::spec(&step.call.tool).and_then(|spec| {
        spec.fields
            .iter()
            .find(|field| field.key == kinds[selected].0)
    }) {
        out = out.push(field_view(state, step, field, false, p));
    }
    out.into()
}

fn list_field<'a>(
    state: &'a quick::State,
    step: &'a PlanStep,
    field: &'static FieldSpec,
    first: bool,
    p: &'static Palette,
) -> Element<'a, quick::Input> {
    let entries = step.call.args[field.key].as_array();
    let mut out = column![text(field.label).size(style::SMALL).color(p.secondary)].spacing(8);
    let columns = if field.kind == FieldKind::Features {
        vec![
            ("feature", "Feature"),
            ("value", "Value"),
            ("expect", "Expected value"),
        ]
    } else {
        vec![("key", "Tag key"), ("value", "Tag value")]
    };
    for (index, _) in entries.into_iter().flatten().enumerate() {
        let mut entry = column![
            text(format!(
                "{} {}",
                if field.kind == FieldKind::Features {
                    "Setting"
                } else {
                    "Tag"
                },
                index + 1
            ))
            .size(style::SMALL)
            .font(style::SEMIBOLD)
        ]
        .spacing(6);
        for (column, (key, label)) in columns.iter().enumerate() {
            entry = entry.push(text_field(
                state,
                step,
                field,
                (
                    &format!("{}.{index}.{key}", field.key),
                    &format!("{label} {}", index + 1),
                    first && index == 0 && column == 0,
                    false,
                ),
                p,
            ));
        }
        let mut values = entries.cloned().unwrap_or_default();
        values.remove(index);
        entry = entry.push(
            button(text("Remove row").size(style::SMALL))
                .name(format!(
                    "Remove {} row {} from step {}",
                    field.label,
                    index + 1,
                    step.id.0
                ))
                .padding([4, 8])
                .style(style::plain)
                .on_press_maybe(state.editable().then_some(quick::Input::Field {
                    step: step.id,
                    key: field.key.into(),
                    value: Value::Array(values).to_string(),
                })),
        );
        out = out.push(container(entry).padding(10).width(Fill).style(style::code));
    }
    let mut values = entries.cloned().unwrap_or_default();
    let can_add = values.len() < 16;
    values.push(if field.kind == FieldKind::Features {
        json!({"feature":"","value":"","expect":null})
    } else {
        json!({"key":"","value":""})
    });
    out = out.push(
        button(
            text(if field.kind == FieldKind::Features {
                "Add feature"
            } else {
                "Add tag"
            })
            .size(style::SMALL),
        )
        .name(format!("Add {} row to step {}", field.label, step.id.0))
        .id(if first && entries.is_none_or(Vec::is_empty) {
            step_focus_id(step.id)
        } else {
            field_id(step.id, &format!("{}-add", field.key))
        })
        .padding([5, 10])
        .style(style::secondary)
        .on_press_maybe(
            (state.editable() && can_add).then_some(quick::Input::Field {
                step: step.id,
                key: field.key.into(),
                value: Value::Array(values).to_string(),
            }),
        ),
    );
    if let Some(issue) = field_issue(state, step, field.key) {
        out = out.push(caption(issue.into(), Level::Error, p));
    }
    out.into()
}

pub(in crate::gui) fn string_list_focus_id(
    step: &PlanStep,
    key: &str,
    index: Option<usize>,
) -> String {
    let first = tools::spec(&step.call.tool)
        .and_then(|spec| spec.fields.first())
        .is_some_and(|field| field.key == key);
    let first_control = index == Some(0)
        || index.is_none() && step.call.args[key].as_array().is_none_or(Vec::is_empty);
    let key = index.map_or_else(|| format!("{key}-add"), |index| format!("{key}.{index}"));
    widget_id(step.id, &key, first && first_control)
}

fn string_list_field<'a>(
    state: &'a quick::State,
    step: &'a PlanStep,
    field: &'static FieldSpec,
    first: bool,
    p: &'static Palette,
) -> Element<'a, quick::Input> {
    let entries = step.call.args[field.key].as_array();
    let mut out = column![text(field.label).size(style::SMALL).color(p.secondary)].spacing(8);
    for (index, _) in entries.into_iter().flatten().enumerate() {
        let entry = column![
            text_field(
                state,
                step,
                field,
                (
                    &format!("{}.{index}", field.key),
                    &format!("{} {}", field.label, index + 1),
                    first && index == 0,
                    false,
                ),
                p,
            ),
            button(text("Remove name").size(style::SMALL))
                .name(format!(
                    "Remove {} row {} from step {}",
                    field.label,
                    index + 1,
                    step.id.0
                ))
                .id(field_id(step.id, &format!("{}.{index}-remove", field.key)))
                .padding([4, 8])
                .style(style::plain)
                .on_press_maybe(state.editable().then_some(quick::Input::StringList {
                    step: step.id,
                    key: field.key.into(),
                    remove: Some(index),
                })),
        ]
        .spacing(6);
        out = out.push(container(entry).padding(10).width(Fill).style(style::code));
    }
    out = out.push(
        button(text("Add name").size(style::SMALL))
            .name(format!("Add {} row to step {}", field.label, step.id.0))
            .id(string_list_focus_id(step, field.key, None))
            .padding([5, 10])
            .style(style::secondary)
            .on_press_maybe(state.editable().then_some(quick::Input::StringList {
                step: step.id,
                key: field.key.into(),
                remove: None,
            })),
    );
    if let Some(issue) = field_issue(state, step, field.key) {
        out = out.push(caption(issue.into(), Level::Error, p));
    }
    out.into()
}

fn friendly_time(value: &str, context: Option<&tools::context::Context>) -> Option<String> {
    let context = context?;
    let time = chrono::DateTime::parse_from_rfc3339(value).ok()?;
    let time = context
        .zone
        .at(u64::try_from(time.timestamp_millis()).ok()?);
    let today = context.zone.at(context.now_ms).date_naive();
    let day = if time.date_naive() == today {
        "Today".into()
    } else if today.succ_opt() == Some(time.date_naive()) {
        "Tomorrow".into()
    } else {
        time.format("%Y-%m-%d").to_string()
    };
    Some(format!("{day} {}", time.format("%H:%M")))
}

fn caption<'a>(message: String, level: Level, p: &'static Palette) -> Element<'a, quick::Input> {
    row![
        level.mark(11.0, p),
        text(message)
            .size(style::CAPTION)
            .color(level.color(p))
            .wrapping(text::Wrapping::WordOrGlyph)
    ]
    .spacing(5)
    .align_y(Alignment::Start)
    .into()
}

fn field_style(
    theme: &Theme,
    status: text_input::Status,
    editable: bool,
    invalid: bool,
    dirty: bool,
) -> text_input::Style {
    let status = if !editable && status == text_input::Status::Disabled {
        text_input::Status::Active
    } else {
        status
    };
    if invalid {
        style::input_invalid(theme, status)
    } else if dirty {
        style::input_dirty(theme, status)
    } else {
        style::input(theme, status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_fields_keep_the_normal_value_and_outline_paint() {
        for theme in [Theme::Light, Theme::Dark] {
            let read_only = field_style(&theme, text_input::Status::Disabled, false, false, false);
            let active = style::input(&theme, text_input::Status::Active);
            assert_eq!(read_only.value, active.value);
            assert_eq!(read_only.border, active.border);
            let invalid = field_style(&theme, text_input::Status::Disabled, false, true, false);
            assert_eq!(invalid.value, active.value);
            assert_eq!(
                invalid.border,
                style::input_invalid(&theme, text_input::Status::Active).border
            );
        }
    }
}
