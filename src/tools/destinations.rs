//! Destination creation checks. Validation is pure; credentials are supplied
//! separately by stable step ID and are only read by the executor.
use super::{
    Checked, FieldKind, FieldSpec, Issue, IssueCode, Op, PlanState, Risk, StepId, ToolKind,
    ToolSpec,
    context::{Context, DestinationInfo},
    parse_args, schema, sentence, values,
};
use crate::destination::{Bucket, Credentials, Destination, NoOverwrite, Target};
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
const CREDENTIALS: &[(&str, &str)] = &[
    ("environment", "AWS environment"),
    ("profile", "AWS profile"),
    ("keychain", "OS credential store"),
];
pub const CREATE_FOLDER_DESTINATION: ToolSpec = ToolSpec {
    name: "create_folder_destination",
    title: "Create folder destination",
    kind: ToolKind::Configure,
    risk: Risk::HumanOnly,
    needs_session: false,
    needs_vision: false,
    description: "Create a named capture folder without replacing an existing destination. path must be absolute (~/ expands to your home). Refuses filesystem roots, the home folder itself, hidden path components, the session and staging folders, and ~/Library on macOS. The folder is created when the step runs. A person must approve destination creation.",
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
pub const CREATE_S3_DESTINATION: ToolSpec = ToolSpec {
    name: "create_s3_destination",
    title: "Create S3 destination",
    kind: ToolKind::Configure,
    risk: Risk::HumanOnly,
    needs_session: false,
    needs_vision: false,
    description: "Create a new S3-compatible destination after checking the bucket and custom endpoint's no-overwrite behavior. Requires S3 support and human approval. credentials is environment, profile, or keychain; only keychain needs access_key_id and a secret typed separately into the form or CLI. endpoint null uses AWS S3. prefix null is empty; path_style and keep_local null are false. Never invent bucket names or ask for secrets in a prompt. Uploads send captures to the displayed host.",
    schema: s3_schema,
    fields: &[
        field(
            "name",
            "Name",
            FieldKind::Text,
            false,
            "A new destination name.",
        ),
        field(
            "bucket",
            "Bucket",
            FieldKind::Text,
            false,
            "An existing bucket, 3..63 lowercase letters, digits, dots or hyphens.",
        ),
        field(
            "region",
            "Region",
            FieldKind::Text,
            false,
            "AWS region, or the service's region such as auto.",
        ),
        field(
            "endpoint",
            "Service URL",
            FieldKind::Url,
            true,
            "Empty uses AWS S3; a custom HTTP or HTTPS service URL has no path or credentials.",
        ),
        field(
            "prefix",
            "Object prefix",
            FieldKind::Text,
            true,
            "An optional relative folder prefix inside the bucket.",
        ),
        field(
            "path_style",
            "Path-style addressing",
            FieldKind::Bool,
            true,
            "Needed by many custom endpoints; false by default.",
        ),
        field(
            "credentials",
            "Credentials",
            FieldKind::Choice(CREDENTIALS),
            false,
            "Use AWS environment variables, a shared profile, or a secret in the OS credential store.",
        ),
        field(
            "profile",
            "AWS profile",
            FieldKind::Text,
            true,
            "Required only for profile credentials.",
        ),
        field(
            "access_key_id",
            "Access key ID",
            FieldKind::Text,
            true,
            "Required only for keychain credentials; this identifies the key, not its secret.",
        ),
        FieldSpec {
            key: "secret",
            label: "Secret access key",
            kind: FieldKind::Secret,
            optional: true,
            help: "Required for keychain credentials. Kept out of tool arguments, plans and model requests.",
            ui_only: true,
        },
        field(
            "keep_local",
            "Keep local copies",
            FieldKind::Bool,
            true,
            "Keep local files after verified uploads; false by default.",
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
fn s3_schema() -> Value {
    schema::object(vec![
        ("name", schema::string("A new destination name.")),
        (
            "bucket",
            schema::string(
                "An existing S3 bucket, 3..63 lowercase letters, digits, dots or hyphens.",
            ),
        ),
        (
            "region",
            schema::string("The service region, such as us-east-1 or auto."),
        ),
        (
            "endpoint",
            schema::nullable(schema::string(
                "A plain HTTP or HTTPS service URL, or null for AWS S3.",
            )),
        ),
        (
            "prefix",
            schema::nullable(schema::string(
                "A relative object-key prefix, or null for none.",
            )),
        ),
        (
            "path_style",
            schema::nullable(schema::boolean(
                "Use path-style bucket addressing; null is false.",
            )),
        ),
        (
            "credentials",
            schema::choice(
                &["environment", "profile", "keychain"],
                "Where AWS credentials come from; secrets are supplied separately.",
            ),
        ),
        (
            "profile",
            schema::nullable(schema::string(
                "AWS profile name for profile credentials; otherwise null.",
            )),
        ),
        (
            "access_key_id",
            schema::nullable(schema::string(
                "Access key ID for keychain credentials; otherwise null.",
            )),
        ),
        (
            "keep_local",
            schema::nullable(schema::boolean(
                "Keep local files after verified uploads; null is false.",
            )),
        ),
    ])
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FolderArgs {
    name: String,
    path: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct S3Args {
    name: String,
    bucket: String,
    region: String,
    endpoint: Option<String>,
    prefix: Option<String>,
    path_style: Option<bool>,
    credentials: String,
    profile: Option<String>,
    access_key_id: Option<String>,
    keep_local: Option<bool>,
}

pub(crate) fn check(
    ctx: &Context,
    plan: &mut PlanState,
    checked: &mut Checked,
    secrets_present: &dyn Fn(StepId, &str) -> bool,
) {
    match checked.call.tool.as_str() {
        "create_folder_destination" => {
            match parse_args::<FolderArgs>(&CREATE_FOLDER_DESTINATION, &checked.call.args) {
                Ok(args) => folder(ctx, plan, checked, args),
                Err(issues) => checked.issues.extend(issues),
            }
        }
        "create_s3_destination" => {
            match parse_args::<S3Args>(&CREATE_S3_DESTINATION, &checked.call.args) {
                Ok(args) => s3(ctx, plan, checked, args, secrets_present),
                Err(issues) => checked.issues.extend(issues),
            }
        }
        _ => {}
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
        &crate::ipc::session_dir(),
        &crate::destination::staging_root(),
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
    checked.ops.push(Op::CreateDestination {
        destination,
        needs_secret: false,
    });
}
fn s3(
    ctx: &Context,
    plan: &mut PlanState,
    checked: &mut Checked,
    args: S3Args,
    secrets_present: &dyn Fn(StepId, &str) -> bool,
) {
    name(plan, checked, &args.name);
    if !ctx.compiled.s3 {
        checked.issue(Issue::error(
            None,
            "Use a build with S3 support to create this destination.",
            IssueCode::Unsupported,
        ));
    }
    let credentials = match args.credentials.as_str() {
        "environment" => Credentials::Environment,
        "profile" => Credentials::Profile {
            name: args.profile.clone().unwrap_or_default(),
        },
        "keychain" => Credentials::Keychain {
            access_key_id: args.access_key_id.clone().unwrap_or_default(),
        },
        _ => {
            checked.error(
                "credentials",
                "Choose environment, profile or keychain credentials.",
            );
            return;
        }
    };
    if args.credentials != "profile" && args.profile.is_some() {
        checked.error(
            "profile",
            "Set profile only for profile credentials; use null.",
        );
    }
    if args.credentials != "keychain" && args.access_key_id.is_some() {
        checked.error(
            "access_key_id",
            "Set access_key_id only for keychain credentials; use null.",
        );
    }
    if args.credentials == "keychain" && !secrets_present(checked.id, "secret") {
        checked.error(
            "secret",
            "Enter the secret access key in Secret access key.",
        );
    }
    let bucket = Bucket {
        endpoint: args.endpoint.unwrap_or_default(),
        region: args.region,
        bucket: args.bucket,
        prefix: args.prefix.unwrap_or_default(),
        path_style: args.path_style.unwrap_or(false),
        credentials,
        keep_local: args.keep_local.unwrap_or(false),
        no_overwrite: NoOverwrite::IfNoneMatch,
    };
    // Validate fields independently so the form can show every problem
    // together. Each probe keeps the other fields valid.
    let valid = Bucket {
        endpoint: String::new(),
        region: "us-east-1".into(),
        bucket: "captures".into(),
        prefix: String::new(),
        path_style: true,
        credentials: Credentials::Environment,
        keep_local: false,
        no_overwrite: NoOverwrite::IfNoneMatch,
    };
    for (field, probe) in [
        (
            "endpoint",
            Bucket {
                endpoint: bucket.endpoint.clone(),
                path_style: bucket.path_style,
                ..valid.clone()
            },
        ),
        (
            "region",
            Bucket {
                region: bucket.region.clone(),
                ..valid.clone()
            },
        ),
        (
            "bucket",
            Bucket {
                bucket: bucket.bucket.clone(),
                ..valid.clone()
            },
        ),
        (
            "prefix",
            Bucket {
                prefix: bucket.prefix.clone(),
                ..valid.clone()
            },
        ),
    ] {
        if let Err(error) = probe.validate() {
            let message = format!("{error:#}");
            let field = if message.contains("path-style") {
                "path_style"
            } else {
                field
            };
            checked.error(field, sentence(&message));
        }
    }
    if let Err(error) = bucket.credentials.validate() {
        checked.error(
            if args.credentials == "profile" {
                "profile"
            } else {
                "access_key_id"
            },
            sentence(&format!("{error:#}")),
        );
    }
    if !checked.ready() {
        return;
    }
    let destination = Destination {
        name: args.name.clone(),
        target: Target::S3(bucket.clone()),
    };
    let url = bucket.service_url();
    let host = url
        .split_once("://")
        .map_or(url.as_str(), |(_, tail)| tail)
        .trim_end_matches('/')
        .to_owned();
    let info = DestinationInfo {
        name: args.name.clone(),
        kind: "s3".into(),
        location: format!("s3://{}/{} at {}", bucket.bucket, bucket.prefix, url),
        host: Some(host.clone()),
        insecure: bucket.insecure(),
        problem: None,
        free_bytes: None,
        remote: true,
    };
    checked.leaves_computer = true;
    checked.warning(
        "bucket",
        format!(
            "Captures saved here leave this computer and are sent to {host}.{}",
            if bucket.insecure() {
                " Unencrypted."
            } else {
                ""
            }
        ),
    );
    if !bucket.endpoint.is_empty()
        && matches!(
            bucket.credentials,
            Credentials::Environment | Credentials::Profile { .. }
        )
    {
        checked.warning(
            "endpoint",
            format!("Your AWS access key ID is sent to {host}."),
        );
    }
    checked.summary = format!("Create S3 destination {} on {host}.", args.name);
    checked
        .facts
        .push(("Location".into(), info.location.clone()));
    checked.destination = Some(info.clone());
    checked.output_root = Some(destination.local_root());
    plan.destinations.push(info);
    checked.ops.push(Op::CreateDestination {
        destination,
        needs_secret: args.credentials == "keychain",
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{
        ToolCall,
        tests::{fixture_context, fixture_snapshot},
    };
    fn checked(tool: &str, args: Value, secret: bool) -> Checked {
        let mut ctx = fixture_context(fixture_snapshot());
        ctx.compiled.s3 = true;
        let mut plan = PlanState::new(&ctx);
        let mut checked = Checked::new(
            StepId(42),
            &ToolCall {
                tool: tool.into(),
                args,
            },
        );
        check(&ctx, &mut plan, &mut checked, &|id, field| {
            assert_eq!(id, StepId(42));
            assert_eq!(field, "secret");
            secret
        });
        checked
    }
    fn bucket() -> Value {
        json!({"name":"new-archive","bucket":"camera-archive","region":"us-east-1","endpoint":null,"prefix":null,"path_style":null,"credentials":"environment","profile":null,"access_key_id":null,"keep_local":null})
    }
    #[test]
    fn folder_creation_is_human_only_and_refuses_sensitive_paths() {
        for path in ["/", "~", "~/.ssh", "relative", "~/Library/LaunchAgents"] {
            let result = checked(
                "create_folder_destination",
                json!({"name":"new-folder","path":path}),
                false,
            );
            if path == "~/Library/LaunchAgents" && !cfg!(target_os = "macos") {
                continue;
            }
            assert!(!result.ready(), "{path}");
            assert!(result.ops.is_empty());
        }
        let result = checked(
            "create_folder_destination",
            json!({"name":"new-folder","path":"~/Captures/line-1"}),
            false,
        );
        assert!(result.ready(), "{:?}", result.issues);
        assert!(matches!(
            &result.ops[..],
            [Op::CreateDestination {
                needs_secret: false,
                ..
            }]
        ));
        assert!(result.summary.contains("~/Captures/line-1"));
    }
    #[test]
    fn s3_defaults_egress_and_all_field_errors() {
        let result = checked("create_s3_destination", bucket(), false);
        assert!(result.ready(), "{:?}", result.issues);
        assert!(result.leaves_computer);
        let Op::CreateDestination {
            destination,
            needs_secret,
        } = &result.ops[0]
        else {
            panic!("create op")
        };
        assert!(!needs_secret);
        let Target::S3(bucket) = &destination.target else {
            panic!("bucket")
        };
        assert!(!bucket.path_style && !bucket.keep_local);
        let mut args = bucket_args();
        args["bucket"] = json!("BAD");
        args["region"] = json!("");
        args["prefix"] = json!("../outside");
        let result = checked("create_s3_destination", args, false);
        for field in ["bucket", "region", "prefix"] {
            assert!(
                result
                    .issues
                    .iter()
                    .any(|i| i.field.as_deref() == Some(field)),
                "{field}: {:?}",
                result.issues
            );
        }
        assert!(result.ops.is_empty());
    }
    fn bucket_args() -> Value {
        bucket()
    }
    #[test]
    fn keychain_secret_is_separate_and_bound_to_the_step() {
        let mut args = bucket();
        args["credentials"] = json!("keychain");
        args["access_key_id"] = json!("AKIAIOSFODNN7EXAMPLE");
        let missing = checked("create_s3_destination", args.clone(), false);
        assert!(
            missing
                .issues
                .iter()
                .any(|i| i.field.as_deref() == Some("secret"))
        );
        let ready = checked("create_s3_destination", args, true);
        assert!(ready.ready(), "{:?}", ready.issues);
        assert!(matches!(
            &ready.ops[..],
            [Op::CreateDestination {
                needs_secret: true,
                ..
            }]
        ));
        assert!(
            CREATE_S3_DESTINATION
                .fields
                .iter()
                .any(|f| f.key == "secret" && f.ui_only)
        );
        assert!(s3_schema()["properties"].get("secret").is_none());
    }
    #[test]
    fn custom_endpoints_warn_about_encryption_and_credential_egress() {
        let mut args = bucket();
        args["endpoint"] = json!("http://nas.local:9000");
        let result = checked("create_s3_destination", args, false);
        assert!(result.ready(), "{:?}", result.issues);
        assert!(
            result
                .issues
                .iter()
                .any(|i| i.message.contains("Unencrypted."))
        );
        assert!(
            result
                .issues
                .iter()
                .any(|i| i.message == "Your AWS access key ID is sent to nas.local:9000.")
        );
    }
    #[test]
    fn duplicate_destination_names_never_create_an_op() {
        let result = checked(
            "create_folder_destination",
            json!({"name":"line-1","path":"~/Captures/new"}),
            false,
        );
        assert!(!result.ready() && result.ops.is_empty());
    }
}
