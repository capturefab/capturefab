//! The one place that prints results and errors. With `--json`, stdout gets
//! exactly one single-line document: the result when the exit code is 0,
//! `{"error":{code,message,hint?,details?}}` otherwise; no null values, and
//! at most ten distinct warnings. People get text on stdout and errors on
//! stderr.
use crate::types::{ErrorKind, classify};
use serde_json::{Value, json};
use std::io::{self, Write};

/// How this invocation reports.
#[derive(Debug, Clone, Copy, Default)]
pub struct Output {
    pub json: bool,
    pub quiet: bool,
    pub verbose: bool,
}

/// Most warnings a JSON result carries.
const MAX_WARNINGS: usize = 10;

/// Whether this process has printed, or is printing, its one document.
static CLAIMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Stdout carries something else (`capture -o -`'s image): the document,
/// result or error, goes to stderr.
static TO_STDERR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Send this invocation's document to stderr from now on.
pub fn document_to_stderr() {
    TO_STDERR.store(true, std::sync::atomic::Ordering::Release);
}
/// Take the right to print this invocation's document; false when a result
/// or error already took it, as when a signal arrives as a command ends.
pub fn claim() -> bool {
    !CLAIMED.swap(true, std::sync::atomic::Ordering::AcqRel)
}

/// A command's result: the JSON document, and the text people read.
pub struct Report {
    pub value: Value,
    pub text: Option<String>,
}
impl Report {
    pub fn json(value: Value) -> Self {
        Self { value, text: None }
    }
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }
}

impl Output {
    /// A line on stderr for people: progress, notes, next steps. Never in
    /// JSON mode or with `-q`.
    pub fn note(&self, line: impl std::fmt::Display) {
        if !self.json && !self.quiet {
            eprintln!("{line}");
        }
    }
    /// Print a result. Returns the exit code: 0, or 0 after a closed pipe.
    pub fn result(&self, report: Report) -> i32 {
        if !claim() {
            return super::signals::exit_code();
        }
        let written = if self.json {
            write_json(&document(report.value))
        } else {
            let text = report.text.unwrap_or_else(|| {
                serde_json::to_string_pretty(&report.value).unwrap_or_default() + "\n"
            });
            let mut out = io::stdout().lock();
            out.write_all(text.as_bytes()).and_then(|()| out.flush())
        };
        match written {
            Err(error) if error.kind() != io::ErrorKind::BrokenPipe => {
                eprintln!("capturefab: cannot write the result: {error}");
                1
            }
            _ => 0,
        }
    }
    /// Report an error; returns its exit code.
    pub fn error(&self, error: &anyhow::Error) -> i32 {
        match self.error_once(error) {
            Some(exit) => exit,
            None => exit_code(error),
        }
    }
    /// Report an error unless this invocation already printed its
    /// document; its exit code, or None when it did not print.
    pub fn error_once(&self, error: &anyhow::Error) -> Option<i32> {
        if error.chain().any(|cause| {
            cause
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
        }) {
            // The reader went away, as with `| head`: nothing to report.
            return Some(0);
        }
        let classified = classify(error);
        let code = classified.kind.code().0;
        let exit = exit_code(error);
        if !claim() {
            return None;
        }
        let message = crate::scrub::scrub(&format!("{error:#}")).into_owned();
        if self.json {
            let mut body = json!({"code": code, "message": message});
            if let Some(hint) = &classified.hint {
                body["hint"] = json!(crate::scrub::scrub(hint));
            }
            if let Some(details) = classified.details {
                body["details"] = details;
            }
            let _ = write_json(&without_nulls(json!({ "error": body })));
        } else {
            eprintln!("capturefab: {message}");
            if let Some(hint) = &classified.hint {
                eprintln!("hint: {}", crate::scrub::scrub(hint));
            }
            if self.verbose
                && let Some(details) = &classified.details
            {
                eprintln!("details: {details}");
            }
        }
        Some(exit)
    }
}

/// The exit code `error` ends the process with: its reason's category, and
/// for an interruption the signal's.
pub fn exit_code(error: &anyhow::Error) -> i32 {
    let kind = classify(error).kind;
    if kind == ErrorKind::Interrupted {
        super::signals::exit_code()
    } else {
        kind.code().1
    }
}

/// A result document as `--json` prints it: no nulls, warnings bounded and
/// deduplicated, and never a top-level `error` key.
pub fn document(value: Value) -> Value {
    let mut value = without_nulls(value);
    if let Some(object) = value.as_object_mut() {
        debug_assert!(!object.contains_key("error"), "a result has no error key");
        if let Some(Value::Array(warnings)) = object.get_mut("warnings") {
            let mut seen = Vec::new();
            warnings.retain(|warning| {
                let keep = !seen.contains(warning) && seen.len() < MAX_WARNINGS;
                if keep {
                    seen.push(warning.clone());
                }
                keep
            });
            if warnings.is_empty() {
                object.remove("warnings");
            }
        }
    }
    value
}

pub fn without_nulls(value: Value) -> Value {
    crate::types::without_nulls(value)
}

pub fn write_json(value: &Value) -> io::Result<()> {
    // One write, so the line is never split by other output.
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    if TO_STDERR.load(std::sync::atomic::Ordering::Acquire) {
        let mut out = io::stderr().lock();
        return out.write_all(&line).and_then(|()| out.flush());
    }
    let mut out = io::stdout().lock();
    out.write_all(&line)?;
    out.flush()
}

/// The reason and exit code `error` reports with, from its typed causes.
#[cfg(test)]
pub fn error_code(error: &anyhow::Error) -> (&'static str, i32) {
    classify(error).kind.code()
}

/// Whether `--json` is on before clap has parsed: the flag anywhere in the
/// arguments, or CAPTUREFAB_JSON.
pub fn json_requested(args: &[std::ffi::OsString]) -> bool {
    args.iter().skip(1).any(|arg| arg == "--json")
        || std::env::var("CAPTUREFAB_JSON").is_ok_and(|value| {
            !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "" | "0" | "false" | "no" | "off"
            )
        })
}

/// The `--json` error for an argument-parsing failure: clap's first line as
/// the message, and what an agent needs to fix it as `details`. `group` is
/// the command and its subcommands when one is missing.
pub fn clap_error(error: &clap::Error, group: Option<(String, Vec<String>)>) -> Value {
    use clap::error::{ContextKind, ContextValue, ErrorKind as Clap};
    let rendered = error.render().to_string();
    let message = rendered
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("invalid usage");
    let mut message = message
        .strip_prefix("error: ")
        .unwrap_or(message)
        .to_owned();
    let kind = match error.kind() {
        Clap::InvalidValue => "invalid_value".to_owned(),
        Clap::UnknownArgument => "unknown_argument".to_owned(),
        Clap::InvalidSubcommand => "invalid_subcommand".to_owned(),
        Clap::ValueValidation => "value_validation".to_owned(),
        Clap::ArgumentConflict => "argument_conflict".to_owned(),
        Clap::MissingRequiredArgument => "missing_required_argument".to_owned(),
        Clap::MissingSubcommand | Clap::DisplayHelpOnMissingArgumentOrSubcommand => {
            "missing_subcommand".to_owned()
        }
        other => {
            let mut name = String::new();
            for (index, c) in format!("{other:?}").chars().enumerate() {
                if c.is_ascii_uppercase() && index > 0 {
                    name.push('_');
                }
                name.push(c.to_ascii_lowercase());
            }
            name
        }
    };
    let strings = |kind: ContextKind| -> Vec<String> {
        match error.get(kind) {
            Some(ContextValue::String(value)) => vec![value.clone()],
            Some(ContextValue::Strings(values)) => values.clone(),
            Some(ContextValue::StyledStr(value)) => vec![value.to_string()],
            Some(ContextValue::StyledStrs(values)) => {
                values.iter().map(ToString::to_string).collect()
            }
            _ => Vec::new(),
        }
    };
    let mut details = json!({ "kind": kind });
    // clap lists the closest match last; agents read the first.
    let mut suggestions = strings(ContextKind::SuggestedSubcommand);
    suggestions.extend(strings(ContextKind::SuggestedArg));
    suggestions.extend(strings(ContextKind::SuggestedValue));
    suggestions.reverse();
    if !suggestions.is_empty() {
        details["suggestions"] = json!(suggestions);
    }
    if let Some(invalid) = [
        ContextKind::InvalidSubcommand,
        ContextKind::InvalidValue,
        ContextKind::InvalidArg,
    ]
    .into_iter()
    .flat_map(strings)
    .find(|value| !value.is_empty())
    {
        details["invalid"] = json!(invalid);
    }
    let valid = strings(ContextKind::ValidValue);
    if !valid.is_empty() {
        details["valid_values"] = json!(valid);
    }
    // clap's own first line ends at a colon here, or is the group's help.
    match error.kind() {
        Clap::MissingRequiredArgument => {
            let missing = strings(ContextKind::InvalidArg);
            if !missing.is_empty() {
                message = format!("missing required argument: {}", missing.join(", "));
                details["missing"] = json!(missing);
            }
        }
        Clap::MissingSubcommand | Clap::DisplayHelpOnMissingArgumentOrSubcommand => {
            if let Some((command, subcommands)) = group {
                message = format!("{command} needs a subcommand: {}", subcommands.join(", "));
                details["valid_values"] = json!(subcommands);
            }
        }
        _ => {}
    }
    if let Some(usage) = strings(ContextKind::Usage).first() {
        let usage = usage.trim();
        details["usage"] = json!(usage.strip_prefix("Usage: ").unwrap_or(usage).trim());
    }
    // A value parser's own typed error keeps its reason, such as a
    // duration without a unit.
    let code = match std::error::Error::source(error).and_then(|source| {
        source
            .downcast_ref::<crate::types::Categorized>()
            .map(|categorized| categorized.kind)
    }) {
        Some(kind) if kind.code().1 == 2 => kind.code().0,
        _ => ErrorKind::Usage.code().0,
    };
    json!({"error": {"code": code, "message": message, "details": details}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_drop_nulls_and_bound_warnings() {
        let warnings = (0..15)
            .map(|i| format!("w{}", i % 12))
            .chain(["w0".to_owned()])
            .collect::<Vec<_>>();
        let doc = document(json!({"a": null, "b": {"c": null, "d": 1}, "warnings": warnings}));
        assert_eq!(doc["b"], json!({"d": 1}));
        assert!(doc.get("a").is_none());
        assert_eq!(doc["warnings"].as_array().unwrap().len(), 10);
        assert!(document(json!({"warnings": []})).get("warnings").is_none());
    }

    #[test]
    fn exit_codes_come_from_types_not_words() {
        use crate::types::Categorized;
        assert_eq!(
            error_code(&Categorized::error(
                ErrorKind::Unavailable,
                "camera camera.disabled_porch timed out"
            )),
            ("unavailable", 3)
        );
        assert_eq!(
            error_code(&Categorized::error(ErrorKind::Timeout, "camera not_found").context("x")),
            ("timeout", 4)
        );
        for words in [
            "connection refused",
            "timed out",
            "unsupported",
            "Storage full",
            "not found",
        ] {
            assert_eq!(
                error_code(&anyhow::anyhow!("{words}")),
                ("failed", 1),
                "{words}"
            );
        }
        assert_eq!(
            error_code(&crate::types::Usage("bad".into()).into()),
            ("usage", 2)
        );
    }
}
