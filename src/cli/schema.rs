//! `capturefab schema`: the command-line contract as JSON, generated from
//! the clap tree and the result types, so it cannot drift from either.
use super::{Cli, Global, output::Report};
use crate::types::{ErrorKind, Usage};
use anyhow::Result;
use serde_json::{Map, Value, json};

/// The contract's version; it changes when a result or argument changes
/// incompatibly.
pub(super) const CONTRACT_VERSION: u32 = 1;

/// What a command can change: read (nothing but ephemeral previews),
/// `camera_write`, `file_write` (outside the camera), or disruptive (may stop
/// streams, hold cameras or run arbitrary camera commands).
const EFFECTS: &[(&str, &str)] = &[
    ("list", "read"),
    ("info", "read"),
    ("features", "read"),
    ("get", "read"),
    ("set", "camera_write"),
    ("exec", "disruptive"),
    ("preset save", "file_write"),
    ("preset list", "read"),
    ("preset show", "read"),
    ("preset apply", "camera_write"),
    ("preset rm", "file_write"),
    ("preset import", "file_write"),
    ("preset export", "file_write"),
    ("capture", "file_write"),
    ("record", "file_write"),
    ("inspect", "read"),
    ("watch", "read"),
    ("auto", "camera_write"),
    ("jobs", "read"),
    ("jobs list", "read"),
    ("jobs stop", "camera_write"),
    ("jobs wait", "read"),
    ("session start", "disruptive"),
    ("session stop", "disruptive"),
    ("session status", "read"),
    ("session list", "read"),
    ("serve", "disruptive"),
    ("login add", "file_write"),
    ("login list", "read"),
    ("login rm", "file_write"),
    ("onvif resolve", "read"),
    ("network", "read"),
    ("doctor", "read"),
    ("schema", "read"),
    ("shell", "disruptive"),
    ("completions", "read"),
    ("debug xml", "read"),
    ("debug read", "read"),
    ("debug write", "disruptive"),
    ("debug rpc", "disruptive"),
    ("licenses", "read"),
];

/// Flags that widen a command's effect: inspect, capture and record with
/// `--set` or `--auto` change the camera first, and so does network with
/// `--tune`; inspect with `--preview PATH` and debug xml with `-o` write a
/// file where the caller says.
const EFFECT_WITH: &[(&str, &[(&str, &str)])] = &[
    (
        "capture",
        &[("--set", "camera_write"), ("--auto", "camera_write")],
    ),
    (
        "record",
        &[("--set", "camera_write"), ("--auto", "camera_write")],
    ),
    (
        "inspect",
        &[("--set", "camera_write"), ("--preview", "file_write")],
    ),
    ("network", &[("--tune", "camera_write")]),
    ("debug xml", &[("--output", "file_write")]),
];

/// A saved preset document, as `preset show` prints it.
const PRESET_DOCUMENT: &[(&str, &str)] = &[
    ("version", "int — document version"),
    ("name", "string"),
    ("saved_at", "string — RFC 3339"),
    ("note", "string"),
    ("camera", "{transport,vendor,model,serial?,kind}"),
    ("auto", "{} — Capturefab auto mode, when saved on"),
    ("values", "[{feature,value,at?:[{selector,value}]}]"),
    ("skipped", "[{feature,at?,reason}]"),
];

/// Result fields of the commands that have a typed contract.
fn fields(command: &str) -> Option<&'static [(&'static str, &'static str)]> {
    Some(match command {
        "list" => super::camera::ListResult::FIELDS,
        "info" => crate::controls::Info::FIELDS,
        "features" => crate::controls::FeatureList::FIELDS,
        "get" => super::camera::GET_FIELDS,
        "set" => crate::controls::SetResult::FIELDS,
        "exec" => super::camera::EXEC_FIELDS,
        "preset apply" => super::preset::APPLY_FIELDS,
        "capture" => super::capture::FIELDS,
        "record" => super::record::FIELDS,
        "inspect" => super::inspect::FIELDS,
        "watch" => super::watch::FIELDS,
        "shell" => &[
            (
                "lines",
                "input: one command a line on stdin (blank and # lines skipped), or each -e; output: one line per command run, in order, its result or {error} as that command prints it (NDJSON with --json); the exit code is the first failure's",
            ),
            ("use", "{camera?} — the default camera"),
            (
                "cameras",
                "{cameras:[{id,state:connecting|connected|failed,default?,session?,error?}]}",
            ),
            (
                "help",
                "{help} — the text of help COMMAND, or of a line's --help",
            ),
        ],
        "auto" => super::auto::FIELDS,
        "jobs" | "jobs list" => super::jobs::LIST_FIELDS,
        "jobs stop" => super::jobs::STOP_FIELDS,
        "jobs wait" => super::jobs::WAIT_FIELDS,
        "session start" => super::session::START_FIELDS,
        "session stop" => super::session::STOP_FIELDS,
        "session status" => super::session::STATUS_FIELDS,
        "session list" => super::session::LIST_FIELDS,
        "serve" => super::session::SERVE_FIELDS,
        "preset save" | "preset import" => &[
            (
                "preset",
                "{name,camera,values,skipped,saved_at,path} — values is a count",
            ),
            ("camera", "string — the camera read (save)"),
        ],
        "preset list" => &[
            ("dir", "string — the folder that holds presets"),
            (
                "presets",
                "[{name,transport?,vendor,model,serial?,saved_at,values,note?,status?:unreadable|newer_version,reason?}] — values is a count",
            ),
        ],
        "preset show" => PRESET_DOCUMENT,
        "preset rm" => &[("removed", "string — the preset's name")],
        "preset export" => &[
            ("preset", "string — with -o: the preset's name"),
            ("path", "string — with -o: the file written"),
            (
                "version",
                "int — without -o: the document itself, as preset show prints it",
            ),
            ("name", "string — without -o"),
            ("saved_at", "string — without -o"),
            ("note", "string — without -o"),
            (
                "camera",
                "{transport,vendor,model,serial?,kind} — without -o",
            ),
            (
                "auto",
                "{} — without -o: Capturefab auto mode, when saved on",
            ),
            (
                "values",
                "[{feature,value,at?:[{selector,value}]}] — without -o",
            ),
            ("skipped", "[{feature,at?,reason}] — without -o"),
        ],
        "login list" => &[
            ("logins", "[{address,user,saved_at}] — never passwords"),
            ("supported", "bool — this build can save sign-ins"),
        ],
        "login add" => &[
            ("login", "{address,user,saved_at}"),
            ("replaced", "bool"),
            ("check", "{ok,camera} — unless --no-check"),
        ],
        "login rm" => &[
            ("removed", "string — the address"),
            ("secret_forgotten", "bool"),
        ],
        "onvif resolve" => super::onvif::FIELDS,
        "network" => super::network::FIELDS,
        "doctor" => super::doctor::DOCTOR_FIELDS,
        "licenses" => super::doctor::LICENSES_FIELDS,
        "debug xml" => &[
            ("xml", "string — without -o"),
            ("path", "string — with -o"),
            ("bytes", "int — with -o"),
        ],
        "debug read" => &[("address", "int"), ("hex", "string — the bytes read")],
        "debug write" => &[("address", "int"), ("written", "int — bytes")],
        "completions" => &[
            ("shell", "string — with --json"),
            (
                "script",
                "string — with --json: the completion script; without it, the script alone",
            ),
        ],
        "schema" => &[
            ("contract_version", "int"),
            (
                "commands",
                "{name: [effect, summary]} — effect: read|file_write|camera_write|disruptive",
            ),
            (
                "effect_with",
                "{name: {flag: effect}} — flags that widen a command's effect",
            ),
            ("conventions", "{...}"),
            ("grammars", "{selector,name,duration,template}"),
            ("exit_codes", "{exit: meaning}"),
            ("errors", "{code: [exit, meaning]} — every error.code"),
        ],
        _ => return None,
    })
}

/// Error reasons a command can report besides `usage` and `failed`.
fn errors(command: &str) -> &'static [&'static str] {
    const CAMERA: &[&str] = &[
        "camera_not_found",
        "camera_ambiguous",
        "camera_busy",
        "permission_denied",
        "disconnected",
        "auth_required",
        "auth_rejected",
        "session_not_found",
        "ffmpeg_missing",
        "unavailable",
        "timeout",
    ];
    match command {
        "list" => &["invalid_duration", "timeout"],
        "get" | "features" | "info" => &[
            "feature_not_found",
            "unsupported",
            "camera_not_found",
            "camera_ambiguous",
            "camera_busy",
            "permission_denied",
            "disconnected",
            "session_not_found",
            "unavailable",
            "timeout",
        ],
        "set" => &[
            "feature_not_found",
            "out_of_range",
            "invalid_choice",
            "not_writable",
            "feature_locked",
            "invalid_duration",
            "unsupported",
            "camera_not_found",
            "camera_ambiguous",
            "camera_busy",
            "disconnected",
            "session_not_found",
            "unavailable",
            "timeout",
        ],
        "exec" => &[
            "feature_not_found",
            "not_writable",
            "feature_locked",
            "camera_not_found",
            "camera_ambiguous",
            "camera_busy",
            "timeout",
        ],
        "preset apply" => &[
            "unsupported",
            "out_of_range",
            "invalid_choice",
            "not_writable",
            "feature_locked",
            "camera_not_found",
            "camera_busy",
            "unavailable",
            "timeout",
        ],
        "preset save" | "preset import" | "preset export" | "debug xml" => {
            &["exists", "storage_full", "camera_not_found", "unsupported"]
        }
        "capture" => &[
            "exists",
            "storage_full",
            "frame_timeout",
            "trigger_timeout",
            "interrupted",
            "incomplete",
            "invalid_duration",
            "feature_not_found",
            "out_of_range",
            "unsupported",
            "camera_not_found",
            "camera_ambiguous",
            "camera_busy",
            "disconnected",
            "session_not_found",
            "unavailable",
            "timeout",
        ],
        "record" => &[
            "exists",
            "storage_full",
            "frame_timeout",
            "interrupted",
            "invalid_duration",
            "feature_not_found",
            "out_of_range",
            "unsupported",
            "camera_not_found",
            "camera_ambiguous",
            "camera_busy",
            "ffmpeg_missing",
            "session_not_found",
            "unavailable",
            "timeout",
        ],
        "inspect" => &[
            "frame_timeout",
            "trigger_timeout",
            "exists",
            "storage_full",
            "feature_not_found",
            "out_of_range",
            "camera_not_found",
            "camera_ambiguous",
            "camera_busy",
            "disconnected",
            "session_not_found",
            "unavailable",
            "timeout",
        ],
        "watch" => &[
            "frame_timeout",
            "trigger_timeout",
            "storage_full",
            "camera_not_found",
            "camera_ambiguous",
            "camera_busy",
            "disconnected",
            "session_not_found",
            "unavailable",
            "timeout",
        ],
        "auto" => &[
            "unsupported",
            "interrupted",
            "invalid_duration",
            "camera_not_found",
            "camera_ambiguous",
            "camera_busy",
            "session_not_found",
            "unavailable",
            "timeout",
        ],
        "jobs" | "jobs list" | "jobs stop" => &["session_not_found", "unavailable", "timeout"],
        "jobs wait" => &[
            "timeout",
            "interrupted",
            "incomplete",
            "session_not_found",
            "storage_full",
            "frame_timeout",
            "trigger_timeout",
            "exists",
            "unavailable",
        ],
        "session start" | "serve" => &[
            "camera_not_found",
            "camera_busy",
            "auth_required",
            "auth_rejected",
            "unavailable",
            "invalid_duration",
            "timeout",
        ],
        "session stop" | "session status" | "session list" => {
            &["session_not_found", "unavailable", "timeout"]
        }
        "login add" | "onvif resolve" => &[
            "auth_required",
            "auth_rejected",
            "unavailable",
            "unsupported",
            "timeout",
        ],
        "network" => &["invalid_duration", "camera_not_found", "unavailable"],
        _ => CAMERA,
    }
}

fn summary(command: &clap::Command) -> String {
    let about = command
        .get_about()
        .map(ToString::to_string)
        .unwrap_or_default();
    about.split(". ").next().unwrap_or_default().to_owned()
}

/// Every command with no subcommands of its own, by its full name.
fn leaves(command: &clap::Command, prefix: &str, out: &mut Vec<(String, clap::Command)>) {
    for sub in command.get_subcommands().filter(|sub| !sub.is_hide_set()) {
        let name = if prefix.is_empty() {
            sub.get_name().to_owned()
        } else {
            format!("{prefix} {}", sub.get_name())
        };
        if sub.has_subcommands() {
            // A group that runs on its own (jobs) is a command too.
            if !sub.is_subcommand_required_set() {
                out.push((name.clone(), sub.clone()));
            }
            leaves(sub, &name, out);
        } else {
            out.push((name, sub.clone()));
        }
    }
}

pub(crate) fn commands() -> Vec<(String, clap::Command)> {
    let mut out = Vec::new();
    leaves(&Cli::tree(), "", &mut out);
    out
}

fn effect(name: &str) -> &'static str {
    EFFECTS
        .iter()
        .find(|(command, _)| *command == name)
        .map_or("disruptive", |(_, effect)| effect)
}

/// The effect each widening flag of `name` gives it, if it has any.
fn effect_with(name: &str) -> Option<Value> {
    EFFECT_WITH
        .iter()
        .find(|(command, _)| *command == name)
        .map(|(_, flags)| {
            Value::Object(
                flags
                    .iter()
                    .map(|(flag, effect)| ((*flag).to_owned(), json!(effect)))
                    .collect(),
            )
        })
}

fn arg_type(arg: &clap::Arg) -> &'static str {
    use clap::ArgAction;
    if matches!(arg.get_action(), ArgAction::SetTrue | ArgAction::SetFalse) {
        return "flag";
    }
    if arg.get_value_parser().possible_values().is_some() {
        return "choice";
    }
    match arg
        .get_value_names()
        .and_then(|names| names.first())
        .map(clap::builder::Str::as_str)
    {
        Some("DUR") => "duration",
        Some("CAM") => "camera",
        Some("NAME=VALUE") => "assignment",
        Some("NAME") => "name",
        _ => "string",
    }
}

/// One command's arguments, as clap parses them; globals are in
/// `conventions`.
pub(crate) fn arguments(command: &clap::Command) -> Vec<Value> {
    command
        .get_arguments()
        .filter(|arg| !arg.is_hide_set() && !arg.is_global_set())
        .filter(|arg| !matches!(arg.get_id().as_str(), "help" | "version"))
        .map(|arg| {
            let mut value = json!({
                "name": arg.get_id().as_str(),
                "type": arg_type(arg),
                "required": arg.is_required_set(),
                "repeatable": matches!(arg.get_action(), clap::ArgAction::Append),
            });
            if let Some(long) = arg.get_long() {
                value["long"] = json!(format!("--{long}"));
            }
            if let Some(names) = arg.get_value_names() {
                value["value_name"] = json!(
                    names
                        .iter()
                        .map(clap::builder::Str::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
            if let Some(short) = arg.get_short() {
                value["short"] = json!(format!("-{short}"));
            }
            if let Some(help) = arg.get_help() {
                value["help"] = json!(help.to_string());
            }
            let defaults = arg
                .get_default_values()
                .iter()
                .map(|v| v.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            if let [default] = defaults.as_slice() {
                value["default"] = json!(default);
            }
            if let Some(choices) = arg.get_value_parser().possible_values() {
                let choices = choices
                    .filter(|v| !v.is_hide_set())
                    .map(|v| v.get_name().to_owned())
                    .collect::<Vec<_>>();
                if !choices.is_empty() && arg_type(arg) == "choice" {
                    value["choices"] = json!(choices);
                }
            }
            if let Some(env) = arg.get_env() {
                value["env"] = json!(env.to_string_lossy());
            }
            value
        })
        .collect()
}

/// The whole contract in brief: maps keyed by command and by error code,
/// with positional values, which keeps it small enough to read once.
fn overview() -> Value {
    let found = commands();
    let commands = found
        .iter()
        .map(|(name, command)| (name.clone(), json!([effect(name), summary(command)])))
        .collect::<Map<_, _>>();
    let widened = found
        .iter()
        .filter_map(|(name, _)| Some((name.clone(), effect_with(name)?)))
        .collect::<Map<_, _>>();
    let exit_codes = json!({
        "0": "ok", "1": "failed", "2": "usage or input", "3": "unavailable",
        "4": "timeout", "5": "unsupported", "6": "storage full",
        "130": "interrupted (143 SIGTERM, 129 SIGHUP)"
    });
    let errors = ErrorKind::ALL
        .iter()
        .map(|kind| {
            let (code, exit) = kind.code();
            (code.to_owned(), json!([exit, kind.meaning()]))
        })
        .collect::<Map<_, _>>();
    json!({
        "contract_version": CONTRACT_VERSION,
        "commands": commands,
        "effect_with": widened,
        "conventions": {
            "json": "--json (or CAPTUREFAB_JSON=1): one line on stdout, the result on exit 0, else {\"error\":{code,message,hint?,details?}}: code is the reason (see errors), the exit code its category. No nulls; warnings ≤10",
            "globals": "--json, -q/--quiet, -v/--verbose, --session NAME (CAPTUREFAB_SESSION), --no-session (overrides it); before or after the command",
            "camera": "-c/--camera CAM (CAPTUREFAB_CAMERA). Without it: the session's only camera, else the only camera list finds (not sim: or screens)",
            "routing": "--session NAME, else a session holding the camera, else the auto session, open 60s after last use (--keep DUR); --keep 0 or --no-session: a worker of the command",
            "jobs": "capture/record block until done; --background prints a job instead (jobs wait ID). Ctrl-C: capture exits 130 with details.files; open-ended record ends normally",
            "streams": "NDJSON: watch a line per measurement, shell per command; capture, record, jobs wait --progress: stderr lines {progress:{job,state,saved,planned?,elapsed_s,left_s?,…}}",
            "results": "snake_case keys; units in names (exposure_us); times RFC 3339 UTC; lists bounded with truncated",
            "credentials": "never printed; URLs are redacted",
            "describe": "capturefab schema COMMAND: arguments, result fields and errors"
        },
        "grammars": {
            "selector": "list id (gige:…, u3v:…, avfoundation:…, v4l2:…, dshow:…), serial, IPv4, rtsp(s)://, srt://, http(s)://, onvif://host[/path], file, sim:N[?drop=0..0.5&line0=every:DUR], sim:replay:PATH",
            "name": "Feature | Feature[Selector=Value,…] | Feature[Value] (when one selector addresses it) | control: exposure, gain, fps, roi, format, trigger, auto_exposure, auto_wb",
            "duration": "number+unit, repeatable: 500us, 250ms, 2s, 5m, 1h30m; 0; none where a wait may be unlimited. A bare number is invalid_duration",
            "template": "-o: {camera} {serial} {seq} {seq:N} {time} {date} {tag:KEY} {ext}; a folder holds {camera}-{time}-{seq}.{ext}; a taken name fails without {seq}, {time} or --on-exists number"
        },
        "exit_codes": exit_codes,
        "errors": errors,
    })
}

fn command(name: &str) -> Result<Value> {
    let found = commands();
    let Some((_, command)) = found.iter().find(|(candidate, _)| candidate == name) else {
        let names = found
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        return Err(crate::types::Categorized::new(
            ErrorKind::Usage,
            format!("no command '{name}'"),
        )
        .details(json!({"commands": names}))
        .into());
    };
    let mut value = json!({
        "contract_version": CONTRACT_VERSION,
        "command": name,
        "summary": summary(command),
        "effect": effect(name),
        "args": arguments(command),
    });
    if let Some(with) = effect_with(name) {
        value["effect_with"] = with;
    }
    let mut errors = vec!["usage", "failed"];
    match name {
        // Each line reports the reasons of the command it runs, and a
        // signal interrupts the run.
        "shell" => errors.extend(
            ErrorKind::ALL
                .iter()
                .map(|kind| kind.code().0)
                .filter(|code| !matches!(*code, "usage" | "failed")),
        ),
        _ => errors.extend(self::errors(name)),
    }
    value["errors"] = json!(errors);
    if let Some(fields) = fields(name) {
        value["result"] = Value::Object(
            fields
                .iter()
                .map(|(field, meaning)| ((*field).to_owned(), json!(meaning)))
                .collect::<Map<_, _>>(),
        );
    }
    Ok(value)
}

pub(super) fn run(_global: &Global, words: &[String]) -> Result<Report> {
    if words.is_empty() {
        let value = overview();
        let mut text = brief(&value, 0);
        text.push('\n');
        return Ok(Report::json(value).text(text));
    }
    let name = words.join(" ");
    if name.trim().is_empty() {
        return Err(Usage("name a command, such as schema set".into()).into());
    }
    let value = command(&name)?;
    let mut text = serde_json::to_string_pretty(&value)?;
    text.push('\n');
    Ok(Report::json(value).text(text))
}

/// JSON with each entry of the outer two maps on a line of its own: the
/// overview as people read it, little larger than the line `--json` prints.
fn brief(value: &Value, depth: usize) -> String {
    fn append(text: &mut String, value: &Value, depth: usize) {
        use std::fmt::Write as _;
        let Value::Object(map) = value else {
            write!(text, "{value}").expect("writing to a String cannot fail");
            return;
        };
        if depth == 2 || map.is_empty() {
            write!(text, "{value}").expect("writing to a String cannot fail");
            return;
        }
        text.push_str("{\n");
        for (index, (key, value)) in map.iter().enumerate() {
            if index > 0 {
                text.push_str(",\n");
            }
            text.extend(std::iter::repeat_n(' ', 2 * (depth + 1)));
            write!(text, "{}: ", Value::from(key.as_str()))
                .expect("writing to a String cannot fail");
            append(text, value, depth + 1);
        }
        text.push('\n');
        text.extend(std::iter::repeat_n(' ', 2 * depth));
        text.push('}');
    }
    let mut text = String::new();
    append(&mut text, value, depth);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overview_format_keeps_json_escaping_and_two_expanded_levels() {
        let value = json!({"a": {"b": {"c": [1, 2]}}, "k\n": "quoted\n\"é"});
        assert_eq!(
            brief(&value, 0),
            "{\n  \"a\": {\n    \"b\": {\"c\":[1,2]}\n  },\n  \"k\\n\": \"quoted\\n\\\"é\"\n}"
        );
        for value in [json!({}), json!([]), json!(null), json!("é\n")] {
            assert_eq!(brief(&value, 0), value.to_string());
        }
    }

    #[test]
    fn every_command_has_an_effect_and_every_effect_a_command() {
        let names = commands()
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        for name in &names {
            assert!(
                EFFECTS.iter().any(|(command, _)| command == name),
                "{name} has no effect"
            );
        }
        for (command, _) in EFFECTS {
            assert!(
                names.iter().any(|name| name == command),
                "{command} is gone"
            );
        }
        // A widening flag is one of its command's own, and widens it.
        let rank = |effect: &str| {
            ["read", "file_write", "camera_write", "disruptive"]
                .iter()
                .position(|known| *known == effect)
                .unwrap()
        };
        for (name, flags) in EFFECT_WITH {
            let (_, command) = commands()
                .into_iter()
                .find(|(candidate, _)| candidate == name)
                .unwrap();
            for (flag, effect) in *flags {
                assert!(
                    command
                        .get_arguments()
                        .any(|arg| arg.get_long() == flag.strip_prefix("--")),
                    "{name} has no {flag}"
                );
                assert!(rank(effect) > rank(super::effect(name)), "{name} {flag}");
            }
        }
        assert_eq!(
            command("capture").unwrap()["effect_with"]["--set"],
            "camera_write"
        );
        // A read command's flag that names a file or path writes there: it
        // widens the command to a file write, which a harness that allows
        // reads must be told.
        for (name, command) in commands() {
            if effect(&name) != "read" {
                continue;
            }
            for arg in command.get_arguments() {
                let names_file = arg
                    .get_value_names()
                    .is_some_and(|names| names.iter().any(|n| n == "FILE" || n == "PATH"));
                if let (true, Some(long)) = (names_file, arg.get_long()) {
                    let widened = effect_with(&name).unwrap_or_default();
                    assert_eq!(
                        widened[format!("--{long}")],
                        "file_write",
                        "{name} --{long}"
                    );
                }
            }
        }
        assert_eq!(
            command("debug xml").unwrap()["effect_with"]["--output"],
            "file_write"
        );
    }

    #[test]
    fn schema_arguments_are_clap_arguments() {
        for (name, command) in commands() {
            let documented = super::command(&name).unwrap()["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|arg| arg["name"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>();
            let parsed = command
                .get_arguments()
                .filter(|arg| !arg.is_hide_set() && !arg.is_global_set())
                .map(|arg| arg.get_id().as_str().to_owned())
                .filter(|id| !matches!(id.as_str(), "help" | "version"))
                .collect::<Vec<_>>();
            assert_eq!(documented, parsed, "{name}");
        }
        let set = command("set").unwrap();
        let dry_run = set["args"]
            .as_array()
            .unwrap()
            .iter()
            .find(|arg| arg["name"] == "dry_run")
            .unwrap();
        assert_eq!(dry_run["type"], "flag");
        assert!(set["result"]["changed"].is_string());
    }

    #[test]
    fn results_describe_left_out_keys_as_absent_never_null() {
        // Output drops null values (`output::document`), so a key a result
        // leaves out is absent, and the schema must not promise a null.
        for (name, _) in commands() {
            let documented = command(&name).unwrap();
            for (field, meaning) in documented["result"].as_object().into_iter().flatten() {
                let meaning = meaning.as_str().unwrap();
                assert!(
                    !meaning
                        .split(|c: char| !c.is_ascii_alphanumeric())
                        .any(|word| word == "null"),
                    "{name} {field}: {meaning}"
                );
            }
        }
        assert!(
            command("doctor").unwrap()["result"]["read_only_root"]
                .as_str()
                .unwrap()
                .contains("absent")
        );
    }

    #[test]
    fn the_overview_stays_small_and_lists_every_reason() {
        let overview = overview();
        let text = overview.to_string();
        // Under 8 KB, some 2k tokens at four characters a token.
        assert!(text.len() < 8000, "{} bytes", text.len());
        assert_eq!(
            overview["errors"].as_object().unwrap().len(),
            ErrorKind::ALL.len()
        );
        assert_eq!(overview["errors"]["camera_not_found"][0], 3);
        assert_eq!(overview["commands"]["set"][0], "camera_write");
        assert_eq!(overview["effect_with"]["capture"]["--set"], "camera_write");
        assert_eq!(
            overview["commands"].as_object().unwrap().len(),
            commands().len()
        );
        command("no such").unwrap_err();
        // What people read is the same JSON, an entry a line.
        let shown = brief(&overview, 0);
        assert_eq!(serde_json::from_str::<Value>(&shown).unwrap(), overview);
        assert!(shown.len() < text.len() * 11 / 10, "{} bytes", shown.len());
    }
}
