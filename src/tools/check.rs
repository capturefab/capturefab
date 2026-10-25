//! Pure checking shared by command-line calls, plan cards and agent runs.
use super::{
    Issue, IssueCode, StepId, ToolCall, ToolKind,
    context::Context,
    exec::{Checked, Op, PlanState},
};
use serde_json::json;

/// Check a step against the live context and preceding valid steps. Invalid
/// steps never affect the overlay used to check later steps.
pub fn check(
    ctx: &Context,
    plan: &mut PlanState,
    id: StepId,
    call: &ToolCall,
    secrets_present: &dyn Fn(StepId, &str) -> bool,
) -> Checked {
    let mut checked = Checked::new(id, call);
    checked.actor_id = plan.actor_id.clone();
    let Some(spec) = super::spec(&call.tool) else {
        checked.issue(Issue::usage(Some("tool"), "Choose a tool from tools list."));
        return checked;
    };
    checked.title = spec.title.into();
    checked.risk = spec.risk;
    let (_, issues) = super::schema::check(&call.tool, &call.args, &(spec.schema)());
    checked.issues.extend(issues);
    if !checked.ready() {
        return checked;
    }
    let mut overlay = plan.clone();
    if super::data::is_action(spec.name) {
        super::data::check(ctx, &mut overlay, &mut checked);
    } else {
        match spec.kind {
            ToolKind::Read => match super::read::preflight_with_origin(ctx, call, plan.origin) {
                Ok(()) => {
                    if call.tool == "query_dataset"
                        && let Some(slot) = &ctx.data_dataset
                    {
                        checked.data_resources.insert(format!("dataset:{slot}"));
                    }
                    checked.output_root = Some(ctx.capture_root.clone());
                    checked.ops.push(Op::Observe(call.clone()));
                }
                Err(issues) => checked.issues.extend(issues),
            },
            ToolKind::Control => {
                super::control::check(ctx, &mut overlay, &mut checked, secrets_present)
            }
            ToolKind::Capture => {
                super::capture::check(ctx, &mut overlay, &mut checked, secrets_present)
            }
            ToolKind::Configure if call.tool == super::setups::APPLY_CAMERA_SETUP.name => {
                super::setups::check(ctx, &mut overlay, &mut checked)
            }
            ToolKind::Configure => super::destinations::check(ctx, &mut overlay, &mut checked),
        }
    }
    if spec.kind != ToolKind::Read && spec.needs_session && !ctx.session {
        checked.issue(Issue::usage(
            None,
            super::sentence(super::context::NO_SESSION),
        ));
    }
    for op in &checked.ops {
        match op {
            Op::Command { command, .. }
            | Op::ApplySetup { command, .. }
            | Op::Stream { command, .. }
            | Op::Session { command, .. } => {
                if let Err(error) = command.validate() {
                    checked.issues.push(command_issue(&error.to_string()));
                }
            }
            Op::Restartable { commands, .. } => {
                for command in commands {
                    if let Err(error) = command.validate() {
                        checked.issues.push(command_issue(&error.to_string()));
                    }
                }
            }
            Op::Connect { selector, .. } => {
                if let Err(error) =
                    crate::session::SessionCommand::connect(selector.clone(), 10000).validate()
                {
                    checked.issues.push(command_issue(&error.to_string()));
                }
            }
            _ => {}
        }
    }
    super::exec::classify(ctx, plan.run.as_ref(), &mut checked);
    let known = json!({"call": checked.call});
    for issue in &mut checked.issues {
        issue.message = super::redact::for_output_error(&issue.message, &known);
    }
    checked.summary = super::redact::for_output_text(&checked.summary);
    for (label, fact) in &mut checked.facts {
        *label = super::redact::for_output_text(label);
        *fact = super::redact::for_output_text(fact);
    }
    for reason in &mut checked.reasons {
        *reason = super::redact::for_output_text(reason);
    }
    if checked.ready() {
        *plan = overlay;
    }
    checked
}

/// Check in plan order, keeping step IDs stable. Use each returned normalized
/// call for later checks so local times cannot roll over on refresh.
pub fn check_plan(
    ctx: &Context,
    steps: &[(StepId, ToolCall)],
    secrets_present: &dyn Fn(StepId, &str) -> bool,
) -> Vec<Checked> {
    let mut state = PlanState::new(ctx);
    check_plan_with(ctx, &mut state, steps, secrets_present)
}

/// The assistant supplies its origin and actor through the overlay; selectors
/// typed by a person can be checked separately with a CLI-origin overlay.
pub fn check_plan_with(
    ctx: &Context,
    state: &mut PlanState,
    steps: &[(StepId, ToolCall)],
    secrets_present: &dyn Fn(StepId, &str) -> bool,
) -> Vec<Checked> {
    let mut seen = std::collections::BTreeSet::new();
    steps
        .iter()
        .enumerate()
        .map(|(index, (id, call))| {
            let mut overlay = state.clone();
            let mut checked = check(ctx, &mut overlay, *id, call, secrets_present);
            if index >= super::MAX_PLAN_CALLS {
                checked.issue(Issue::usage(None, "Keep a plan to at most 64 steps."));
            }
            if !seen.insert(*id) {
                checked.issue(Issue::usage(None, "Give each step its own stable ID."));
            }
            if checked.ready() {
                *state = overlay;
            }
            checked
        })
        .collect()
}

fn command_issue(message: &str) -> Issue {
    let lower = message.to_ascii_lowercase();
    let field = if lower.contains("fps") {
        "fps"
    } else if lower.contains("bitrate") {
        "bitrate"
    } else if lower.contains("codec") || lower.contains("encoder") {
        "codec"
    } else if lower.contains("duration") || lower.contains("reservation") {
        "duration"
    } else if lower.contains("interval") {
        "every"
    } else if lower.contains("first_at") {
        "start_at"
    } else if lower.contains("count") {
        "count"
    } else if lower.contains("feature") {
        "values"
    } else if lower.contains("destination") {
        "destination"
    } else if lower.contains("output") || lower.contains("template") {
        "file_name"
    } else {
        "camera"
    };
    Issue::error(Some(field), super::sentence(message), IssueCode::Usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{
        Origin, Risk,
        tests::{fixture_context, fixture_snapshot},
    };

    #[test]
    fn failed_steps_do_not_change_the_plan_overlay() {
        let ctx = fixture_context(fixture_snapshot());
        let mut state = PlanState::new(&ctx);
        let before = state.connected.clone();
        let call = ToolCall {
            tool: "capture_photos".into(),
            args: json!({"camera":"sim:2","count":1,"format":"png","file_name":"../escape.png","destination":null,"tags":null}),
        };
        let checked = check(&ctx, &mut state, StepId(1), &call, &|_, _| false);
        assert!(!checked.ready());
        assert_eq!(state.connected, before);
        assert_eq!(state.planned_bytes, 0);
        assert_eq!(state.planned_files, 0);
    }

    #[test]
    fn assistant_origin_cannot_introduce_a_camera_selector() {
        let ctx = fixture_context(fixture_snapshot());
        let mut state = PlanState::assistant(&ctx);
        assert_eq!(state.origin, Origin::Assistant);
        let call = ToolCall {
            tool: "connect_camera".into(),
            args: json!({"camera":"gige:10.100.99.88","size":null,"fps":null}),
        };
        let checked = check(&ctx, &mut state, StepId(1), &call, &|_, _| false);
        assert!(!checked.ready());
        assert!(
            checked.issues.iter().any(|i| i.message
                == "Choose a camera from the list, or type its address in this field.")
        );
        assert!(checked.ops.is_empty());
    }

    #[test]
    fn strict_arguments_return_all_structural_field_issues() {
        let ctx = fixture_context(fixture_snapshot());
        let checked = check(
            &ctx,
            &mut PlanState::new(&ctx),
            StepId(1),
            &ToolCall {
                tool: "capture_photos".into(),
                args: json!({"camera":3,"count":"two","format":null,"file_name":null,"destination":null,"tags":null,"extra":true}),
            },
            &|_, _| false,
        );
        assert_eq!(checked.issues.len(), 3);
        assert!(checked.ops.is_empty());
        assert!(checked.issues.iter().all(|i| i.field.is_some()));
    }

    #[test]
    fn check_failure_fields_remain_typed_and_redacted() {
        let checked = check(
            &fixture_context(fixture_snapshot()),
            &mut PlanState::new(&fixture_context(fixture_snapshot())),
            StepId(1),
            &ToolCall {
                tool: "stream_to_url".into(),
                args: json!({"camera":"sim:0","url":"rtmp://h/live2/KEY","duration":null,"codec":null,"fps":null,"bitrate":null}),
            },
            &|_, _| false,
        );
        assert!(!checked.ready());
        assert_eq!(checked.risk, Risk::Disruptive);
        assert!(!checked.value().to_string().contains("live2/KEY"));
        assert!(!format!("{checked:?}").contains("live2/KEY"));
    }
}
