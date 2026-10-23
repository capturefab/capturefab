//! Validate named local capture folders before execution.
use super::{
    Checked, FieldKind, FieldSpec, Op, PlanState, Risk, ToolKind, ToolSpec,
    context::{Context, DestinationInfo},
    parse_args, schema, sentence, values,
};
use crate::destination::{Destination, Target};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;

const fn field(
    key: &'static str,
    label: &'static str,
    kind: FieldKind,
    optional: bool,
    help: &'static str,
) -> FieldSpec {
    FieldSpec {
        key,
        label,
        kind,
        optional,
        help,
        ui_only: false,
    }
}
pub const CREATE_FOLDER_DESTINATION: ToolSpec = ToolSpec {
    name: "create_folder_destination",
    title: "Create folder destination",
    kind: ToolKind::Configure,
    risk: Risk::HumanOnly,
    needs_session: false,
    needs_vision: false,
    description: "Create a named capture folder without replacing an existing destination. path must be absolute (~/ expands to your home). Refuses filesystem roots, the home folder itself, hidden path components, the runtime and state folders, and ~/Library on macOS. The folder is created when the step runs. A person must approve destination creation.",
    schema: folder_schema,
    fields: &[
        field(
            "name",
            "Name",
            FieldKind::Text,
            false,
            "A new destination name: 1..64 letters, digits, dots, underscores or hyphens.",
        ),
        field(
            "path",
            "Capture folder",
            FieldKind::Path,
            false,
            "An absolute folder for captures, such as ~/Captures/line-1.",
        ),
    ],
};
fn folder_schema() -> Value {
    schema::object(vec![
        (
            "name",
            schema::string(
                "A new destination name, 1..64 letters, digits, dots, underscores or hyphens.",
            ),
        ),
        (
            "path",
            schema::string("An absolute capture folder; ~/ expands to the home folder."),
        ),
    ])
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FolderArgs {
    name: String,
    path: String,
}

pub(crate) fn check(ctx: &Context, plan: &mut PlanState, checked: &mut Checked) {
    if checked.call.tool == "create_folder_destination" {
        match parse_args::<FolderArgs>(&CREATE_FOLDER_DESTINATION, &checked.call.args) {
            Ok(args) => folder(ctx, plan, checked, args),
            Err(issues) => checked.issues.extend(issues),
        }
    }
}
fn name(plan: &PlanState, checked: &mut Checked, name: &str) {
    if let Err(error) = crate::destination::validate_name(name) {
        checked.error("name", sentence(&format!("{error:#}")));
    }
    if plan
        .destinations
        .iter()
        .any(|d| d.name.eq_ignore_ascii_case(name))
    {
        checked.error(
            "name",
            "Choose a new destination name; this name is already in use.",
        );
    }
}
fn home() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}
fn folder(_ctx: &Context, plan: &mut PlanState, checked: &mut Checked, args: FolderArgs) {
    name(plan, checked, &args.name);
    let path = match values::folder_path(
        &args.path,
        &home(),
        &crate::ipc::runtime_dir(),
        &crate::ipc::state_dir(),
    ) {
        Ok(path) => Some(path),
        Err(issue) => {
            checked.issue(issue);
            None
        }
    };
    let Some(path) = path.filter(|_| checked.ready()) else {
        return;
    };
    let destination = Destination {
        name: args.name.clone(),
        target: Target::Folder { path: path.clone() },
    };
    if let Err(error) = destination.validate() {
        checked.error("path", sentence(&format!("{error:#}")));
        return;
    }
    checked.call.args["path"] = json!(path);
    let info = DestinationInfo {
        approval_identity: DestinationInfo::identity(&destination),
        name: args.name.clone(),
        kind: "folder".into(),
        location: super::redact::home_path(&path),
        host: None,
        insecure: false,
        problem: None,
        free_bytes: None,
        remote: false,
    };
    checked.summary = format!("Create destination {} at {}.", args.name, info.location);
    checked.output_root = Some(path);
    checked.destination = Some(info.clone());
    plan.destinations.push(info);
    checked.ops.push(Op::CreateDestination { destination });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{
        StepId, ToolCall,
        tests::{fixture_context, fixture_snapshot},
    };
    fn checked(args: Value) -> Checked {
        let ctx = fixture_context(fixture_snapshot());
        let mut plan = PlanState::new(&ctx);
        let mut checked = Checked::new(
            StepId(42),
            &ToolCall {
                tool: "create_folder_destination".into(),
                args,
            },
        );
        check(&ctx, &mut plan, &mut checked);
        checked
    }
    #[test]
    fn folder_creation_is_human_only_and_refuses_sensitive_paths() {
        for path in ["/", "~", "~/.ssh", "relative", "~/Library/LaunchAgents"] {
            if path == "~/Library/LaunchAgents" && !cfg!(target_os = "macos") {
                continue;
            }
            let result = checked(json!({"name":"new-folder","path":path}));
            assert!(!result.ready(), "{path}");
            assert!(result.ops.is_empty());
        }
        let result = checked(json!({"name":"new-folder","path":"~/Captures/line-1"}));
        assert!(result.ready(), "{:?}", result.issues);
        assert!(matches!(&result.ops[..], [Op::CreateDestination { .. }]));
        assert!(!result.leaves_computer);
    }
    #[test]
    fn duplicate_destination_names_never_create_an_op() {
        let result = checked(json!({"name":"line-1","path":"~/Captures/new"}));
        assert!(!result.ready() && result.ops.is_empty());
    }
}
