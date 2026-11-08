//! Output file naming: templates with {camera}, {serial}, {date}, {time},
//! {datetime}, {seq} (or {seq:N}), {frame}, {ext} and {tag:KEY}, and what to
//! do when a name is taken. Names are picked only in the camera worker; the
//! CLI validates and previews.
//! {tag:KEY} is the capture context value for KEY made into one safe path
//! component, so a label such as `defect` can choose a folder.
#![forbid(unsafe_code)]
#![deny(clippy::cast_precision_loss)]
use crate::{context::Context, storage::OutputExists, types::CameraInfo};
use anyhow::{Context as _, Result, bail, ensure};
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::Mutex,
};

/// What happens when an output name without {seq} already exists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum OnExists {
    /// Refuse, so nothing is ever overwritten.
    #[default]
    Fail,
    /// Write `stem-0001.ext`, `stem-0002.ext`, ... instead.
    Number,
}
impl OnExists {
    pub fn is_fail(self) -> bool {
        self == Self::Fail
    }
}

pub(crate) const PLACEHOLDERS: [&str; 8] = [
    "camera", "serial", "date", "time", "datetime", "seq", "frame", "ext",
];
/// Names tried for one file before giving up on races with other writers.
pub(crate) const MAX_ATTEMPTS: u32 = 64;
/// Directory entries read to find the highest {seq} already used.
pub(crate) const MAX_SCAN_ENTRIES: usize = 200_000;
pub(crate) const MAX_SEQ: u32 = 99_999_999;
const CACHE_KEYS: usize = 256;
const PLACEHOLDER_ERROR: &str = "capture output supports only the {camera}, {serial}, {date}, {time}, {datetime}, {seq}, {seq:N}, {frame}, {ext} and {tag:KEY} placeholders";
/// The digits {seq} is padded to unless {seq:N} says otherwise.
const SEQ_WIDTH: usize = 4;
/// The longest expanded {tag:KEY} value, in bytes.
pub(crate) const MAX_TAG_COMPONENT: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Lit(String),
    Camera,
    Serial,
    Date,
    Time,
    DateTime,
    /// The next free number, padded to this many digits.
    Seq(usize),
    Frame,
    /// The format's extension; [`Plan::new`] fills it in.
    Ext,
    /// A capture context value, by key.
    Tag(String),
}

fn tokens(template: &str) -> Result<Vec<Token>> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find(['{', '}']) {
        let (literal, placeholder) = rest.split_at(open);
        let placeholder = placeholder.strip_prefix('{').context(PLACEHOLDER_ERROR)?;
        if !literal.is_empty() {
            out.push(Token::Lit(literal.into()));
        }
        let (name, tail) = placeholder.split_once('}').context(PLACEHOLDER_ERROR)?;
        out.push(match name {
            "camera" => Token::Camera,
            "serial" => Token::Serial,
            "date" => Token::Date,
            "time" => Token::Time,
            "datetime" => Token::DateTime,
            "seq" => Token::Seq(SEQ_WIDTH),
            "frame" => Token::Frame,
            "ext" => Token::Ext,
            name => {
                if let Some(seq) = name.strip_prefix("seq:") {
                    let width = seq.parse::<usize>().ok().filter(|width| (1..=9).contains(width));
                    Token::Seq(width.context("{seq:N} pads to N digits, 1 to 9, such as {seq:6}")?)
                } else if let Some(key) = name.strip_prefix("tag:") {
                    ensure!(
                        crate::context::is_valid_key(key),
                        "{{tag:{}}} needs a tag key that starts with a lowercase letter and uses only a-z, 0-9 and _, such as {{tag:label}}",
                        key.escape_debug()
                    );
                    Token::Tag(key.into())
                } else {
                    bail!(PLACEHOLDER_ERROR);
                }
            }
        });
        rest = tail;
    }
    if !rest.is_empty() {
        out.push(Token::Lit(rest.into()));
    }
    Ok(out)
}

/// Check an output's shape before settings or storage can change: a path or
/// stream URL without control characters, at most 16 KiB.
pub(crate) fn validate_output(output: &str) -> Result<()> {
    ensure!(
        !output.trim().is_empty() && output.len() <= 16384 && !output.chars().any(char::is_control),
        "capture output needs a path without control characters (maximum 16384 bytes)"
    );
    Ok(())
}

/// Check placeholder names and where {seq} may appear.
pub(crate) fn validate_template(output: &str) -> Result<()> {
    let tokens = tokens(output)?;
    let Some(seq) = tokens.iter().position(|t| matches!(t, Token::Seq(_))) else {
        return Ok(());
    };
    ensure!(
        tokens.iter().filter(|t| matches!(t, Token::Seq(_))).count() == 1,
        "capture output may use {{seq}} only once"
    );
    ensure!(
        !tokens[seq + 1..]
            .iter()
            .any(|t| matches!(t, Token::Lit(s) if s.contains(std::path::is_separator))),
        "{{seq}} must be in the file name, not in a folder name"
    );
    match tokens.get(seq + 1) {
        Some(Token::Lit(s)) if s.starts_with(|c: char| c.is_ascii_digit()) => {
            bail!("{{seq}} cannot be followed directly by a digit")
        }
        Some(Token::Date | Token::Time | Token::DateTime | Token::Frame | Token::Tag(_)) => {
            bail!(
                "{{seq}} cannot be followed directly by {{date}}, {{time}}, {{datetime}}, {{frame}} or {{tag:KEY}}"
            )
        }
        _ => Ok(()),
    }
}

pub(crate) fn has_placeholder(output: &str) -> bool {
    output.contains("{tag:")
        || output.contains("{seq:")
        || PLACEHOLDERS
            .iter()
            .any(|p| output.contains(&format!("{{{p}}}")))
}

/// The tag keys a template uses, in order, without repeats.
pub(crate) fn tag_keys(template: &str) -> Vec<String> {
    let mut keys = Vec::new();
    for token in tokens(template).unwrap_or_default() {
        if let Token::Tag(key) = token
            && !keys.contains(&key)
        {
            keys.push(key);
        }
    }
    keys
}

/// Make a tag value one safe file or folder name: separators and characters
/// Windows refuses become `_`, trailing dots and spaces go, `.`, `..` and
/// empty values are refused, a leading dot (a hidden name) and device names
/// such as `CON` get a `_` prefix, and the result is at most
/// [`MAX_TAG_COMPONENT`] bytes.
pub(crate) fn sanitize_component(value: &str) -> Result<String> {
    let mut out: String = value
        .trim()
        .chars()
        .map(|c| {
            if std::path::is_separator(c)
                || matches!(c, '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*')
                || c.is_control()
            {
                '_'
            } else {
                c
            }
        })
        .collect();
    if out.len() > MAX_TAG_COMPONENT {
        let mut end = MAX_TAG_COMPONENT;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    let out = out.trim_end_matches(['.', ' ']).trim_start();
    ensure!(
        !out.is_empty() && out != "." && out != "..",
        "tag value '{}' cannot be used as a file or folder name",
        value.escape_debug()
    );
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0');
    Ok(if reserved || out.starts_with('.') {
        format!("_{out}")
    } else {
        out.to_owned()
    })
}

/// The safe path component for {tag:KEY}, or a usage error naming the key
/// when the tag is empty or missing.
fn tag_value(key: &str, context: &Context) -> Result<String> {
    let value = context.get(key).map(|v| v.trim()).unwrap_or_default();
    if value.is_empty() {
        return Err(crate::types::Usage(format!(
            "capture output uses {{tag:{key}}} but the capture tag '{key}' is empty; fill it in (--tag {key}=VALUE) or change the file name pattern"
        ))
        .into());
    }
    sanitize_component(value).map_err(|e| crate::types::Usage(format!("{e:#}")).into())
}

/// Whether `output` ends in a path separator, naming a folder to fill.
pub(crate) fn names_directory(output: &str) -> bool {
    output != "-" && output.ends_with(std::path::is_separator)
}

/// The extension a file of `format` is given when its name has none.
pub(crate) fn format_extension(format: &str) -> &str {
    if format == "jpeg" { "jpg" } else { format }
}

/// `output` with `.ext` appended when its last component has no '.' at all.
fn with_extension(output: &str, format: &str) -> String {
    let last = output
        .rsplit(std::path::is_separator)
        .next()
        .unwrap_or(output);
    if last.contains('.') || last.is_empty() {
        output.into()
    } else {
        format!("{output}.{}", format_extension(format))
    }
}

/// The image extension (as written) and the format it names, when the last
/// component of `output` ends in a literal .png, .jpg, .jpeg or .raw. Text
/// after a placeholder's closing brace counts; a placeholder inside the
/// extension does not.
pub(crate) fn image_extension(output: &str) -> Option<(&str, &'static str)> {
    let last = output.rsplit(std::path::is_separator).next()?;
    let (_, extension) = last.rsplit_once('.')?;
    let format = [
        ("png", "png"),
        ("jpg", "jpeg"),
        ("jpeg", "jpeg"),
        ("raw", "raw"),
    ]
    .into_iter()
    .find_map(|(suffix, format)| extension.eq_ignore_ascii_case(suffix).then_some(format))?;
    Some((extension, format))
}

/// Why `output` cannot hold `count` frames: a sequence written to one image
/// name (not an existing folder), with a template and a folder that would.
pub(crate) fn sequence_needs_template(output: &str, count: u32) -> Option<String> {
    if count <= 1 || output == "-" || has_placeholder(output) || Path::new(output).is_dir() {
        return None;
    }
    let (extension, _) = image_extension(output)?;
    let (stem, _) = output.rsplit_once('.')?;
    Some(format!(
        "-n {count} saves several files, but {output} names one file; use a template such as -o '{stem}-{{seq}}.{extension}' or a folder such as -o {stem}/"
    ))
}

/// Why `format` cannot be written to `output`: its image extension names
/// another format. jpg and jpeg are the same.
pub(crate) fn format_mismatch(output: &str, format: &str) -> Option<String> {
    let (_, named) = image_extension(output)?;
    if named == format {
        return None;
    }
    let last = output
        .rsplit(std::path::is_separator)
        .next()
        .unwrap_or(output);
    let renamed = format!("{}.{}", last.rsplit_once('.')?.0, format_extension(format));
    Some(format!(
        "{last} names a {} file but the format is {format}; rename it to {renamed} or drop --format",
        named.to_ascii_uppercase()
    ))
}

/// A template written once per frame needs a name that changes per frame:
/// {seq}, {frame} or {time}, or a folder, whose frames are numbered.
pub(crate) fn validate_for_count(output: &str, count: u32, on_exists: OnExists) -> Result<()> {
    ensure!(
        count <= 1
            || !has_placeholder(output)
            || names_directory(output)
            || ["{frame}", "{seq}", "{seq:", "{time}"]
                .iter()
                .any(|placeholder| output.contains(placeholder))
            || on_exists == OnExists::Number,
        "a multi-frame template needs {{seq}}, {{frame}} or {{time}}, or --on-exists number"
    );
    Ok(())
}

/// Check a capture, schedule or recording request before it is queued.
pub(crate) fn validate_request(
    output: &str,
    count: u32,
    on_exists: OnExists,
    context: &Context,
) -> Result<()> {
    crate::context::validate(context)?;
    // Network URLs keep their own checks, which refuse braces.
    if output != "-" && crate::media::is_recording(output) {
        validate_template(output)?;
        validate_for_count(output, count, on_exists)?;
        for key in tag_keys(output) {
            tag_value(&key, context)?;
        }
    }
    Ok(())
}

/// What {camera} and {serial} name a camera's files with.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CameraName {
    /// `model-serial`, from [`camera_label`].
    pub label: String,
    /// The serial number made file-name-safe the same way, or `unknown`.
    pub serial: String,
}
/// The names {camera} and {serial} give `info`'s files.
pub(crate) fn camera_name(info: &CameraInfo) -> CameraName {
    let serial = safe_label(&info.serial);
    CameraName {
        label: camera_label(info),
        serial: if serial.is_empty() {
            "unknown".into()
        } else {
            serial
        },
    }
}

/// A file-name-safe camera label, `model-serial`. It never uses the camera's
/// ID or address, so URL credentials cannot reach a file name.
pub(crate) fn camera_label(info: &CameraInfo) -> String {
    let label = safe_label(&format!("{}-{}", info.model, info.serial));
    if label.is_empty() {
        "camera".into()
    } else {
        label
    }
}
/// `raw` limited to `A-Z a-z 0-9 . _ -` (other characters become one `-`)
/// and 64 bytes, without leading or trailing `-` and `.`; empty for a
/// Windows device name.
fn safe_label(raw: &str) -> String {
    let mut label = String::new();
    for c in raw.chars() {
        let c = if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            c
        } else {
            '-'
        };
        if !(c == '-' && label.ends_with('-')) {
            label.push(c);
        }
    }
    let mut label = label.trim_matches(['-', '.']).to_owned();
    if label.len() > 64 {
        label.truncate(64);
        label.truncate(label.trim_end_matches(['-', '.']).len());
    }
    let stem = label.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0');
    if reserved { String::new() } else { label }
}

/// {time}: the date and the time to the millisecond, so names made in the
/// same second differ.
const TIME_FORMAT: &str = "%Y%m%d-%H%M%S%.3f";

fn expand(token: &Token, input: &NameInput<'_>) -> Result<String> {
    Ok(match token {
        Token::Lit(s) => s.clone(),
        Token::Camera => input.camera.label.clone(),
        Token::Serial => input.camera.serial.clone(),
        Token::Date => input.now.format("%Y-%m-%d").to_string(),
        Token::Time => input.now.format(TIME_FORMAT).to_string(),
        Token::DateTime => input.now.format("%Y%m%d-%H%M%S").to_string(),
        Token::Frame => format!("{:06}", input.index.saturating_add(1)),
        Token::Tag(key) => tag_value(key, input.context)?,
        Token::Ext => bail!("{{ext}} names an image format; a recording's name gives its own"),
        Token::Seq(_) => unreachable!("{{seq}} is chosen by the namer"),
    })
}

/// `Ok` when nothing is at `path`; `OutputExists` when anything is, even a
/// dangling symbolic link. A `.raw` name is taken too when its sidecar's
/// is, so a raw frame and its sidecar always go together.
pub(crate) fn check_free(path: &Path) -> Result<()> {
    if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("raw"))
    {
        check_one(&sidecar(path))?;
    }
    check_one(path)
}

/// The `NAME.json` sidecar that describes the raw frame saved as `NAME`.
pub(crate) fn sidecar(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".json");
    path.with_file_name(name)
}

fn check_one(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Err(OutputExists(path.to_path_buf()).into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "capture output cannot be inspected: {}; check directory permissions",
                path.display()
            )
        }),
    }
}

/// How a capture output names its files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    Stdout,
    File(PathBuf),
    /// `{camera}-{time}-{seq}.ext`, continuing after the highest number
    /// the camera's files there have.
    Directory {
        dir: PathBuf,
        ext: String,
    },
    Template(String),
}
/// The names a folder's frames get.
const FOLDER_NAMES: &str = "{camera}-{time}-{seq}.{ext}";
impl Plan {
    /// A name ending in a separator, a sequence without a placeholder and
    /// an existing folder name `{camera}-{time}-{seq}.ext` files in that
    /// folder. A file name or template whose last component has no
    /// extension gets the format's, so the name says what the file holds;
    /// {ext} is the format's.
    pub(crate) fn new(output: &str, count: u32, format: &str) -> Self {
        if output == "-" {
            Self::Stdout
        } else if has_placeholder(output) {
            // A templated folder still names a folder: its frames are
            // named inside it, like an untemplated one's.
            let template = if names_directory(output) {
                format!("{output}{FOLDER_NAMES}")
            } else {
                output.to_owned()
            };
            let template = template.replace("{ext}", format_extension(format));
            Self::Template(with_extension(&template, format))
        } else if names_directory(output) || count > 1 || Path::new(output).is_dir() {
            Self::Directory {
                dir: output.into(),
                ext: format.into(),
            }
        } else {
            Self::File(with_extension(output, format).into())
        }
    }
    /// Whether every frame's name is known in advance. A {tag:KEY} name
    /// depends on the request's context, so it is not.
    pub(crate) fn is_static(&self) -> bool {
        match self {
            Self::File(_) => true,
            Self::Template(t) => tokens(t).is_ok_and(|tokens| {
                tokens
                    .iter()
                    .all(|t| matches!(t, Token::Lit(_) | Token::Frame))
            }),
            Self::Stdout | Self::Directory { .. } => false,
        }
    }
    /// Whether a frame's name says when it was taken: a folder's names, or
    /// a template with {date}, {time} or {datetime}.
    pub(crate) fn names_time(&self) -> bool {
        match self {
            Self::Directory { .. } => true,
            Self::Template(t) => tokens(t).is_ok_and(|tokens| {
                tokens
                    .iter()
                    .any(|t| matches!(t, Token::Date | Token::Time | Token::DateTime))
            }),
            Self::Stdout | Self::File(_) => false,
        }
    }
    /// Whether each camera's frames get names of their own: a folder's
    /// names, or a template with {camera} or {serial}.
    pub(crate) fn names_camera(&self) -> bool {
        match self {
            Self::Directory { .. } => true,
            Self::Template(t) => tokens(t).is_ok_and(|tokens| {
                tokens
                    .iter()
                    .any(|t| matches!(t, Token::Camera | Token::Serial))
            }),
            Self::Stdout | Self::File(_) => false,
        }
    }
    fn describe(&self) -> String {
        match self {
            Self::Stdout => "-".into(),
            Self::File(path) | Self::Directory { dir: path, .. } => path.display().to_string(),
            Self::Template(t) => t.clone(),
        }
    }
}

/// Whether a taken name is replaced by the next free one rather than
/// failing: names with {seq}, or with {time}, which a name taken in the
/// same millisecond must not fail on.
pub(crate) fn renumbers(plan: &Plan, on_exists: OnExists) -> bool {
    match plan {
        Plan::Stdout => false,
        Plan::Directory { .. } => true,
        Plan::Template(t) if t.contains("{seq") || t.contains("{time}") => true,
        _ => on_exists == OnExists::Number,
    }
}

/// The clock {date} and {time} read: now, at the caller's UTC offset in
/// seconds, or this computer's when none was sent. A session started in
/// another time zone then names files as the person who asked would.
pub(crate) fn clock(utc_offset_s: Option<i32>) -> DateTime<FixedOffset> {
    in_zone(chrono::Utc::now(), utc_offset_s)
}
/// A frame's time, `wall_ms` UTC Unix milliseconds, as `clock` reads now.
pub(crate) fn frame_time(wall_ms: u64, utc_offset_s: Option<i32>) -> DateTime<FixedOffset> {
    let time = i64::try_from(wall_ms)
        .ok()
        .and_then(DateTime::from_timestamp_millis)
        .unwrap_or_else(chrono::Utc::now);
    in_zone(time, utc_offset_s)
}
fn in_zone(time: DateTime<chrono::Utc>, utc_offset_s: Option<i32>) -> DateTime<FixedOffset> {
    let offset = utc_offset_s
        .and_then(FixedOffset::east_opt)
        .unwrap_or_else(|| *time.with_timezone(&chrono::Local).offset());
    time.with_timezone(&offset)
}
/// This computer's UTC offset in seconds now, which requests carry.
pub(crate) fn local_offset() -> i32 {
    chrono::Local::now().offset().local_minus_utc()
}
/// An offset within a day either way.
pub(crate) fn validate_offset(utc_offset_s: Option<i32>) -> Result<()> {
    ensure!(
        utc_offset_s.is_none_or(|offset| FixedOffset::east_opt(offset).is_some()),
        "UTC offset must be within a day"
    );
    Ok(())
}

/// What a name is built from, taken once per frame before it is requested.
pub(crate) struct NameInput<'a> {
    pub camera: &'a CameraName,
    pub now: DateTime<FixedOffset>,
    pub index: u32,
    /// The capture context that fills {tag:KEY}.
    pub context: &'a Context,
}

/// One part of a numbered file name. A stamp matches any text of its length
/// made of digits, dashes and dots, so numbering continues across seconds
/// and days.
enum Part {
    Lit(String),
    Stamp(String),
    Seq,
}

/// Picks free names, remembering the last number used per folder and pattern
/// so that names it handed out are not handed out again before they exist.
#[derive(Default)]
pub(crate) struct Namer {
    next: HashMap<(PathBuf, String), u32>,
}
impl Namer {
    /// Never creates files. Returns a name that is free right now, or
    /// `OutputExists` when a name without {seq} is taken in fail mode.
    pub(crate) fn candidate(
        &mut self,
        plan: &Plan,
        input: &NameInput<'_>,
        on_exists: OnExists,
    ) -> Result<PathBuf> {
        match plan {
            Plan::Stdout => bail!("stdout output has no file name"),
            Plan::Directory { dir, ext } => self.numbered(
                dir,
                &[
                    Part::Lit(format!("{}-", input.camera.label)),
                    Part::Stamp(input.now.format(TIME_FORMAT).to_string()),
                    Part::Lit("-".into()),
                    Part::Seq,
                    Part::Lit(format!(".{}", format_extension(ext))),
                ],
                SEQ_WIDTH,
                plan,
            ),
            Plan::File(path) => self.fixed(path.clone(), on_exists, plan),
            Plan::Template(template) => {
                let tokens = tokens(template)?;
                let Some(seq) = tokens.iter().position(|t| matches!(t, Token::Seq(_))) else {
                    let name = tokens
                        .iter()
                        .map(|t| expand(t, input))
                        .collect::<Result<String>>()?;
                    // A {time} name taken in the same millisecond is
                    // numbered rather than failing.
                    let on_exists = if tokens.contains(&Token::Time) {
                        OnExists::Number
                    } else {
                        on_exists
                    };
                    return self.fixed(name.into(), on_exists, plan);
                };
                let Token::Seq(width) = tokens[seq] else {
                    unreachable!("found as {{seq}}")
                };
                // {seq} is in the final component: split the text before it
                // at the last separator into the folder and the name prefix.
                let before = tokens[..seq]
                    .iter()
                    .map(|t| expand(t, input))
                    .collect::<Result<String>>()?;
                let split = before.rfind(std::path::is_separator).map_or(0, |i| i + 1);
                let dir = PathBuf::from(before.split_at(split).0);
                let mut parts = Vec::new();
                let mut lit = String::new();
                for t in tokens[..seq].iter().rev() {
                    let text = expand(t, input)?;
                    match t {
                        Token::Date | Token::Time | Token::DateTime => {
                            if let Some((_, tail)) = text.rsplit_once(std::path::is_separator) {
                                lit.insert_str(0, tail);
                                break;
                            }
                            if !lit.is_empty() {
                                parts.push(Part::Lit(std::mem::take(&mut lit)));
                            }
                            parts.push(Part::Stamp(text));
                        }
                        _ => match text.rsplit_once(std::path::is_separator) {
                            Some((_, tail)) => {
                                lit.insert_str(0, tail);
                                break;
                            }
                            None => lit.insert_str(0, &text),
                        },
                    }
                }
                if !lit.is_empty() {
                    parts.push(Part::Lit(lit));
                }
                parts.reverse();
                parts.push(Part::Seq);
                for t in &tokens[seq + 1..] {
                    let text = expand(t, input)?;
                    match t {
                        Token::Date | Token::Time | Token::DateTime => {
                            parts.push(Part::Stamp(text));
                        }
                        _ => parts.push(Part::Lit(text)),
                    }
                }
                self.numbered(&dir, &parts, width, plan)
            }
        }
    }

    fn fixed(&mut self, path: PathBuf, on_exists: OnExists, plan: &Plan) -> Result<PathBuf> {
        match check_free(&path) {
            Ok(()) => Ok(path),
            Err(e) if on_exists == OnExists::Number && is_exists(&e) => {
                let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
                let name = path
                    .file_name()
                    .context("capture output has no file name")?;
                let name = Path::new(name);
                let stem = name
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let mut parts = vec![Part::Lit(format!("{stem}-")), Part::Seq];
                if let Some(ext) = name.extension() {
                    parts.push(Part::Lit(format!(".{}", ext.to_string_lossy())));
                }
                self.numbered(&dir, &parts, 4, plan)
            }
            Err(e) => Err(e),
        }
    }

    fn numbered(
        &mut self,
        dir: &Path,
        parts: &[Part],
        width: usize,
        plan: &Plan,
    ) -> Result<PathBuf> {
        let mut key = String::new();
        for part in parts {
            match part {
                Part::Lit(text) => key.push_str(text),
                Part::Stamp(text) => write!(key, "{{{}}}", text.len())?,
                Part::Seq => key.push_str("{seq}"),
            }
        }
        let key = (dir.to_path_buf(), key);
        let start = match self.next.get(&key) {
            Some(&last) => last.saturating_add(1),
            None => highest(dir, parts).saturating_add(1),
        };
        self.free(dir, parts, width, plan, key, start)
            .map(|(_, path)| path)
    }

    /// The first free name numbered `start` or higher, and its number.
    fn free(
        &mut self,
        dir: &Path,
        parts: &[Part],
        width: usize,
        plan: &Plan,
        key: (PathBuf, String),
        start: u32,
    ) -> Result<(u32, PathBuf)> {
        let build = |n: u32| -> PathBuf {
            let mut name = String::new();
            for part in parts {
                match part {
                    Part::Lit(text) | Part::Stamp(text) => name.push_str(text),
                    Part::Seq => write!(name, "{n:0width$}").unwrap(),
                }
            }
            dir.join(name)
        };
        let mut n = start.max(1);
        loop {
            ensure!(
                n <= MAX_SEQ,
                "no free file name for {}; choose another output",
                plan.describe()
            );
            let path = build(n);
            match check_free(&path) {
                Ok(()) => {
                    if self.next.len() >= CACHE_KEYS && !self.next.contains_key(&key) {
                        self.next.clear();
                    }
                    self.next.insert(key, n);
                    return Ok((n, path));
                }
                Err(e) if is_exists(&e) => n += 1,
                Err(e) => return Err(e),
            }
        }
    }
}

/// The highest number already used in `dir` for this pattern, or 0. Stills
/// still being written, under their hidden partial names, are not counted.
fn highest(dir: &Path, parts: &[Part]) -> u32 {
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .take(MAX_SCAN_ENTRIES)
        .filter_map(|entry| {
            let name = entry.ok()?.file_name();
            let name = name.to_str()?;
            (!crate::storage::is_partial(name))
                .then(|| matches(name, parts))
                .flatten()
        })
        .max()
        .unwrap_or(0)
}

fn matches(mut name: &str, parts: &[Part]) -> Option<u32> {
    let mut seq = None;
    for part in parts {
        match part {
            Part::Lit(s) => name = name.strip_prefix(s.as_str())?,
            Part::Stamp(s) => {
                let (head, tail) = name.split_at_checked(s.len())?;
                if !head
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b == b'-' || b == b'.')
                {
                    return None;
                }
                name = tail;
            }
            Part::Seq => {
                let digits = name.bytes().take_while(u8::is_ascii_digit).count();
                if digits == 0 {
                    return None;
                }
                let (digits, tail) = name.split_at(digits);
                seq = digits.parse().ok();
                name = tail;
            }
        }
    }
    if name.is_empty() { seq } else { None }
}

fn is_exists(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|e| e.downcast_ref::<OutputExists>().is_some())
}

/// Write a file under `first`, moving to the next free name when another
/// writer took it first. Lock `namer` only to pick names, never across `write`.
pub(crate) fn save_named<T>(
    namer: &Mutex<Namer>,
    plan: &Plan,
    input: &NameInput<'_>,
    on_exists: OnExists,
    first: PathBuf,
    mut write: impl FnMut(&Path) -> Result<T>,
) -> Result<(PathBuf, T)> {
    let mut path = first;
    for attempt in 1..=MAX_ATTEMPTS {
        match write(&path) {
            Ok(value) => return Ok((path, value)),
            Err(e) if is_exists(&e) && renumbers(plan, on_exists) => {
                if attempt == MAX_ATTEMPTS {
                    break;
                }
                path = namer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .candidate(plan, input, on_exists)?;
            }
            Err(e) => return Err(e),
        }
    }
    bail!(
        "no free file name for {} after {MAX_ATTEMPTS} attempts; choose another output",
        plan.describe()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_templated_folder_names_frames_inside_it() {
        assert_eq!(
            Plan::new("dataset/{tag:label}/", 3, "png"),
            Plan::Template("dataset/{tag:label}/{camera}-{time}-{seq}.png".into())
        );
        assert_eq!(
            Plan::new("dataset/{tag:label}/", 1, "jpeg"),
            Plan::Template("dataset/{tag:label}/{camera}-{time}-{seq}.jpg".into())
        );
        validate_for_count("out/{camera}/", 2, OnExists::Fail).unwrap();
    }

    #[test]
    fn serials_seq_widths_and_extensions() {
        let dir = temp("placeholders");
        let mut namer = Namer::default();
        let plan = Plan::new(
            &format!("{}/{{serial}}-{{seq:6}}.{{ext}}", dir.display()),
            1,
            "jpeg",
        );
        assert_eq!(
            plan,
            Plan::Template(format!("{}/{{serial}}-{{seq:6}}.jpg", dir.display()))
        );
        assert_eq!(
            namer
                .candidate(&plan, &input(fixed_time()), OnExists::Fail)
                .unwrap(),
            dir.join("SN1-000001.jpg")
        );
        for bad in [
            "{seq:0}.png",
            "{seq:10}.png",
            "{seq:x}.png",
            "{seq:4}-{seq}.png",
        ] {
            assert!(validate_template(bad).is_err(), "{bad}");
        }
        // {time} reads to the millisecond, and a name taken in that
        // millisecond is numbered rather than failing.
        let plan = Plan::new(&format!("{}/{{time}}.png", dir.display()), 1, "png");
        let first = namer
            .candidate(&plan, &input(fixed_time()), OnExists::Fail)
            .unwrap();
        assert_eq!(first, dir.join("20261017-101500.000.png"));
        std::fs::write(&first, b"x").unwrap();
        assert_eq!(
            namer
                .candidate(&plan, &input(fixed_time()), OnExists::Fail)
                .unwrap(),
            dir.join("20261017-101500.000-0001.png")
        );
        assert!(renumbers(&plan, OnExists::Fail));
        validate_for_count("{time}.png", 5, OnExists::Fail).unwrap();
        let mut unnamed = CameraInfo {
            id: "sim:0".into(),
            transport: crate::types::Transport::Simulator,
            vendor: String::new(),
            model: "M".into(),
            serial: "A/B 7".into(),
            address: None,
            profile: crate::camera_profile::CameraProfile::default(),
        };
        assert_eq!(camera_name(&unnamed).serial, "A-B-7");
        unnamed.serial.clear();
        assert_eq!(camera_name(&unnamed).serial, "unknown");
        std::fs::remove_dir_all(dir).unwrap();
    }
    use chrono::TimeZone as _;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "capturefab-naming-{name}-{}-{}",
            std::process::id(),
            crate::scheduling::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
    static NO_TAGS: Context = Context::new();
    fn camera(label: &str) -> &'static CameraName {
        Box::leak(Box::new(CameraName {
            label: label.into(),
            serial: "SN1".into(),
        }))
    }
    fn input(now: DateTime<FixedOffset>) -> NameInput<'static> {
        NameInput {
            camera: camera("Model-SN1"),
            now,
            index: 0,
            context: &NO_TAGS,
        }
    }
    fn fixed_time() -> DateTime<FixedOffset> {
        FixedOffset::east_opt(3600)
            .unwrap()
            .with_ymd_and_hms(2026, 10, 17, 10, 15, 0)
            .unwrap()
    }

    #[test]
    fn a_raw_name_is_taken_while_its_sidecar_is() {
        let dir = temp("sidecar");
        std::fs::write(dir.join("x.raw.json"), b"{}").unwrap();
        std::fs::write(dir.join("y-0001.raw.json"), b"{}").unwrap();
        // Any other file's name stays free beside a .json of its name.
        std::fs::write(dir.join("z.png.json"), b"{}").unwrap();
        assert!(is_exists(&check_free(&dir.join("x.raw")).unwrap_err()));
        check_free(&dir.join("z.png")).unwrap();
        let mut namer = Namer::default();
        let raw = Plan::new(dir.join("x.raw").to_str().unwrap(), 1, "raw");
        let refused = namer
            .candidate(&raw, &input(fixed_time()), OnExists::Fail)
            .unwrap_err();
        assert!(is_exists(&refused));
        assert_eq!(
            namer
                .candidate(&raw, &input(fixed_time()), OnExists::Number)
                .unwrap(),
            dir.join("x-0001.raw")
        );
        let numbered = Plan::new(dir.join("y-{seq}.raw").to_str().unwrap(), 2, "raw");
        assert_eq!(
            namer
                .candidate(&numbered, &input(fixed_time()), OnExists::Fail)
                .unwrap(),
            dir.join("y-0002.raw")
        );
        assert_eq!(sidecar(&dir.join("f.dat")), dir.join("f.dat.json"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_clock_reads_the_callers_offset() {
        let east = clock(Some(5 * 3600 + 1800));
        assert_eq!(east.offset().local_minus_utc(), 5 * 3600 + 1800);
        let here = clock(None);
        assert_eq!(here.offset().local_minus_utc(), local_offset());
        assert!(validate_offset(Some(86_400)).is_err());
        validate_offset(Some(-3600)).unwrap();
        // A frame's time, read the same way.
        let taken = frame_time(1_700_000_000_123, Some(3600));
        assert_eq!(taken.format(TIME_FORMAT).to_string(), "20231114-231320.123");
    }

    #[test]
    fn names_say_whether_they_carry_the_time() {
        for (output, count, timed) in [
            ("shots/", 1, true),
            ("a/{date}/b.png", 1, true),
            ("{time}.png", 1, true),
            ("{datetime}-{seq}.png", 2, true),
            ("{camera}-{seq}.png", 2, false),
            ("shot.png", 1, false),
            ("-", 1, false),
        ] {
            assert_eq!(
                Plan::new(output, count, "png").names_time(),
                timed,
                "{output}"
            );
        }
    }

    #[test]
    fn templates_are_validated() {
        for ok in [
            "a/{camera}/{date}/{time}-{datetime}-{frame}-{seq}.png",
            "plain.png",
            "{seq}",
            "x-{seq}-{camera}.png",
        ] {
            validate_template(ok).unwrap();
        }
        for bad in [
            "{unknown}.png",
            "{seq}/x.png",
            "{seq}-{seq}.png",
            "{seq}{time}.png",
            "{seq}1.png",
            "a{b.png",
            "a}b.png",
            "{Camera}.png",
        ] {
            assert!(validate_template(bad).is_err(), "{bad}");
        }
        assert!(
            validate_template("{x}")
                .unwrap_err()
                .to_string()
                .contains("supports only the {camera}, {serial}")
        );
        validate_for_count("d/{camera}.png", 1, OnExists::Fail).unwrap();
        validate_for_count("d/{camera}.png", 2, OnExists::Number).unwrap();
        validate_for_count("d/{camera}-{seq}.png", 2, OnExists::Fail).unwrap();
        validate_for_count("d/{frame}.png", 2, OnExists::Fail).unwrap();
        validate_for_count("dir", 2, OnExists::Fail).unwrap();
        assert!(validate_for_count("d/{camera}.png", 2, OnExists::Fail).is_err());
    }

    #[test]
    fn camera_labels_are_safe() {
        let mut info = CameraInfo {
            id: "rtsp://user:secret@host/x".into(),
            transport: crate::types::Transport::Simulator,
            vendor: "Capturefab".into(),
            model: "Sim Cam/2".into(),
            serial: "SIM0".into(),
            address: Some("rtsp://user:secret@host/x".into()),
            profile: crate::camera_profile::CameraProfile::default(),
        };
        assert_eq!(camera_label(&info), "Sim-Cam-2-SIM0");
        info.model = "..".into();
        info.serial = String::new();
        assert_eq!(camera_label(&info), "camera");
        info.model = "con".into();
        assert_eq!(camera_label(&info), "camera");
        info.model = "COM1".into();
        assert_eq!(camera_label(&info), "camera");
        info.model = "é@:/".repeat(40);
        info.serial = "x".repeat(100);
        let label = camera_label(&info);
        assert!(
            label.len() <= 64 && !label.contains(['@', ':', '/']),
            "{label}"
        );
    }

    #[test]
    fn expansion() {
        let now = fixed_time();
        let dir = temp("expand");
        let template = format!("{}/{{date}}/{{camera}}-{{frame}}.png", dir.display());
        let path = Namer::default()
            .candidate(&Plan::Template(template), &input(now), OnExists::Fail)
            .unwrap();
        assert_eq!(path, dir.join("2026-10-17/Model-SN1-000001.png"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unicode_templates_keep_literals_and_placeholder_boundaries() {
        let dir = temp("unicode-template");
        let template = format!("{}/雪/{{date}}/🙂-{{serial}}-{{seq:6}}.png", dir.display());
        let path = Namer::default()
            .candidate(
                &Plan::Template(template),
                &input(fixed_time()),
                OnExists::Fail,
            )
            .unwrap();
        assert_eq!(path, dir.join("雪/2026-10-17/🙂-SN1-000001.png"));
        for bad in ["雪}.png", "雪{seq:é}.png", "雪{tag:É}.png"] {
            assert!(validate_template(bad).is_err(), "{bad}");
        }
        assert!(
            sequence_needs_template("雪.画像.jpeg", 2)
                .unwrap()
                .contains("雪.画像-{seq}.jpeg")
        );
        assert!(
            format_mismatch("雪.画像.jpeg", "png")
                .unwrap()
                .contains("雪.画像.png")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn seq_continues_per_folder_and_pattern() {
        let dir = temp("seq");
        for name in [
            "A-20261017-101500-0007.png",
            "A-20261017-101501-0012.png",
            "B-20261017-101500-0099.png",
            "A-notastamp-0500.png",
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let plan = Plan::Template(format!(
            "{}/{{camera}}-{{datetime}}-{{seq}}.png",
            dir.display()
        ));
        let mut namer = Namer::default();
        let mut input = input(fixed_time());
        input.camera = camera("A");
        let first = namer.candidate(&plan, &input, OnExists::Fail).unwrap();
        assert_eq!(first, dir.join("A-20261017-101500-0013.png"));
        // A name handed out is not handed out again before it exists.
        input.now = fixed_time() + chrono::Duration::seconds(1);
        let second = namer.candidate(&plan, &input, OnExists::Fail).unwrap();
        assert_eq!(second, dir.join("A-20261017-101501-0014.png"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn existing_names_fail_or_number() {
        let dir = temp("exists");
        let file = dir.join("capture.png");
        std::fs::write(&file, b"x").unwrap();
        let plan = Plan::File(file);
        let mut namer = Namer::default();
        let error = namer
            .candidate(&plan, &input(fixed_time()), OnExists::Fail)
            .unwrap_err();
        assert!(error.downcast_ref::<OutputExists>().is_some());
        let numbered = namer
            .candidate(&plan, &input(fixed_time()), OnExists::Number)
            .unwrap();
        assert_eq!(numbered, dir.join("capture-0001.png"));
        std::fs::write(&numbered, b"x").unwrap();
        assert_eq!(
            Namer::default()
                .candidate(&plan, &input(fixed_time()), OnExists::Number)
                .unwrap(),
            dir.join("capture-0002.png")
        );
        #[cfg(unix)]
        {
            let link = dir.join("dangling.png");
            std::os::unix::fs::symlink(dir.join("missing.png"), &link).unwrap();
            assert!(
                namer
                    .candidate(&Plan::File(link), &input(fixed_time()), OnExists::Fail)
                    .unwrap_err()
                    .downcast_ref::<OutputExists>()
                    .is_some()
            );
        }
        for i in 1..=3 {
            std::fs::write(
                dir.join(format!("Model-SN1-20261016-090000.000-{i:04}.raw")),
                b"x",
            )
            .unwrap();
        }
        let plan = Plan::new(dir.to_str().unwrap(), 1, "raw");
        assert!(matches!(plan, Plan::Directory { .. }) && !plan.is_static());
        assert_eq!(
            namer
                .candidate(&plan, &input(fixed_time()), OnExists::Fail)
                .unwrap(),
            dir.join("Model-SN1-20261017-101500.000-0004.raw")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stills_being_written_are_not_numbers_in_use() {
        // Another process's burst: two published, two still being written.
        let dir = temp("partials");
        for name in [
            "x-0001.png",
            "x-0002.png",
            ".x-0003.png.4242-a1b2c3d4e5f6.partial",
            ".x-0004.png.4242-0a0b0c0d0e0f.partial",
        ] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let parts = [Part::Lit("x-".into()), Part::Seq, Part::Lit(".png".into())];
        assert_eq!(highest(&dir, &parts), 2);
        // Even a pattern their hidden names fit.
        let tail = Part::Lit(".png.4242-a1b2c3d4e5f6.partial".into());
        let hidden = [Part::Lit(".x-".into()), Part::Seq, tail];
        assert_eq!(highest(&dir, &hidden), 0);
        let plan = Plan::Template(format!("{}/x-{{seq}}.png", dir.display()));
        let next = Namer::default()
            .candidate(&plan, &input(fixed_time()), OnExists::Fail)
            .unwrap();
        assert_eq!(next, dir.join("x-0003.png"));
    }

    #[test]
    fn save_named_retries_only_taken_names() {
        let dir = temp("save");
        let plan = Plan::Template(format!("{}/x-{{seq}}.png", dir.display()));
        let namer = Mutex::new(Namer::default());
        let input = input(fixed_time());
        let first = namer
            .lock()
            .unwrap()
            .candidate(&plan, &input, OnExists::Fail)
            .unwrap();
        let mut calls = 0;
        let (path, ()) = save_named(&namer, &plan, &input, OnExists::Fail, first, |p| {
            calls += 1;
            if calls == 1 {
                std::fs::write(p, b"raced").unwrap();
                return Err(OutputExists(p.into()).into());
            }
            Ok(())
        })
        .unwrap();
        assert_eq!((calls, path), (2, dir.join("x-0002.png")));
        let mut calls = 0;
        let error = save_named(
            &namer,
            &plan,
            &input,
            OnExists::Fail,
            dir.join("y"),
            |_| -> Result<()> {
                calls += 1;
                anyhow::bail!("disk on fire")
            },
        )
        .unwrap_err();
        assert_eq!((calls, error.to_string()), (1, "disk on fire".into()));
        let error = save_named(&namer, &plan, &input, OnExists::Fail, dir.join("z"), |p| {
            Err::<(), _>(OutputExists(p.into()).into())
        })
        .unwrap_err();
        assert!(error.to_string().contains("after 64 attempts"), "{error}");
        // A static name in fail mode is never renumbered.
        let file = Plan::File(dir.join("f.png"));
        let error = save_named(
            &namer,
            &file,
            &input,
            OnExists::Fail,
            dir.join("f.png"),
            |p| Err::<(), _>(OutputExists(p.into()).into()),
        )
        .unwrap_err();
        assert!(error.downcast_ref::<OutputExists>().is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tag_placeholders_parse() {
        for ok in [
            "{tag:label}/{seq}.png",
            "{tag:part}-{camera}-{seq}.png",
            "{tag:a_1}/{tag:b}/x-{frame}.png",
        ] {
            validate_template(ok).unwrap();
            assert!(has_placeholder(ok), "{ok}");
        }
        for bad in [
            "{tag:}/x.png",
            "{tag:Bad}/x.png",
            "{tag:1a}/x.png",
            "{tag:a-b}/x.png",
            "{seq}{tag:label}.png",
            "{tag}/x.png",
        ] {
            assert!(validate_template(bad).is_err(), "{bad}");
        }
        assert_eq!(
            tag_keys("{tag:label}/{tag:part}-{tag:label}-{seq}.png"),
            ["label", "part"]
        );
        assert_eq!(
            Plan::new("{tag:label}/x.png", 1, "png"),
            Plan::Template("{tag:label}/x.png".into())
        );
        assert!(!Plan::new("{tag:label}/x.png", 1, "png").is_static());
        // Several cameras each need names of their own.
        for (output, own) in [
            ("{camera}-{seq}.png", true),
            ("by/{serial}/{frame}.png", true),
            ("frames/", true),
            ("{seq}.png", false),
            ("{frame}.png", false),
            ("{tag:label}-{date}.png", false),
            ("x.png", false),
        ] {
            assert_eq!(Plan::new(output, 1, "png").names_camera(), own, "{output}");
        }
    }

    #[test]
    fn tag_values_become_safe_components() {
        for (value, safe) in [
            ("good", "good"),
            ("  defect  ", "defect"),
            ("a/b", "a_b"),
            ("a\\b", "a_b"),
            ("../x", "_.._x"),
            (".hidden", "_.hidden"),
            ("x:y*?", "x_y__"),
            ("trailing. .", "trailing"),
            ("CON", "_CON"),
            ("com1.txt", "_com1.txt"),
            ("COM0", "COM0"),
        ] {
            assert_eq!(sanitize_component(value).unwrap(), safe, "{value}");
        }
        for bad in ["", " ", ".", "..", "...", " . "] {
            assert!(sanitize_component(bad).is_err(), "{bad:?}");
        }
        let long = sanitize_component(&"é".repeat(100)).unwrap();
        assert!(long.len() <= MAX_TAG_COMPONENT && long.chars().all(|c| c == 'é'));
    }

    #[test]
    fn missing_tags_are_usage_errors() {
        let template = "shots/{tag:label}/{camera}-{seq}.png";
        let error = validate_request(template, 1, OnExists::Fail, &NO_TAGS).unwrap_err();
        assert!(error.is::<crate::types::Usage>(), "{error}");
        assert!(error.to_string().contains("'label' is empty"), "{error}");
        let blank = Context::from([("label".into(), "  ".into())]);
        assert!(validate_request(template, 1, OnExists::Fail, &blank).is_err());
        let good = Context::from([("label".into(), "a/b".into())]);
        validate_request(template, 1, OnExists::Fail, &good).unwrap();
        // Network outputs keep refusing braces elsewhere and are not expanded.
        validate_request("rtsp://host/live", 1, OnExists::Fail, &NO_TAGS).unwrap();
    }

    #[test]
    fn seq_counts_per_label_folder() {
        let dir = temp("labels");
        std::fs::create_dir_all(dir.join("defect")).unwrap();
        std::fs::write(dir.join("defect/x-0041.png"), b"x").unwrap();
        let plan = Plan::new(
            &format!("{}/{{tag:label}}/x-{{seq}}.png", dir.display()),
            1,
            "png",
        );
        let mut namer = Namer::default();
        let good = Context::from([("label".into(), "good".into())]);
        let defect = Context::from([("label".into(), "defect".into())]);
        let mut name = |context: &Context| {
            let mut input = input(fixed_time());
            input.context = context;
            namer.candidate(&plan, &input, OnExists::Fail).unwrap()
        };
        assert_eq!(name(&good), dir.join("good/x-0001.png"));
        assert_eq!(name(&defect), dir.join("defect/x-0042.png"));
        assert_eq!(name(&good), dir.join("good/x-0002.png"));
        let traversal = Context::from([("label".into(), "../x".into())]);
        assert_eq!(name(&traversal), dir.join("_.._x/x-0001.png"));
        let error = namer
            .candidate(&plan, &input(fixed_time()), OnExists::Fail)
            .unwrap_err();
        assert!(error.is::<crate::types::Usage>());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn requests_validate_context_and_templates() {
        let mut context = Context::new();
        validate_request("out-{seq}.png", 3, OnExists::Fail, &context).unwrap();
        validate_request("rtsp://host/live", 1, OnExists::Fail, &context).unwrap();
        validate_request("-", 1, OnExists::Fail, &context).unwrap();
        assert!(validate_request("out-{nope}.png", 1, OnExists::Fail, &context).is_err());
        context.insert("Line".into(), "x".into());
        assert!(validate_request("out.png", 1, OnExists::Fail, &context).is_err());
    }
    #[test]
    fn output_names_say_what_they_hold() {
        assert!(matches!(
            Plan::new("out/", 1, "png"),
            Plan::Directory { .. }
        ));
        assert_eq!(Plan::new("noext", 1, "png"), Plan::File("noext.png".into()));
        assert_eq!(
            Plan::new("noext", 1, "jpeg"),
            Plan::File("noext.jpg".into())
        );
        assert_eq!(Plan::new("y.jpg", 1, "png"), Plan::File("y.jpg".into()));
        assert_eq!(
            Plan::new("z/{seq}", 2, "png"),
            Plan::Template("z/{seq}.png".into())
        );
        assert_eq!(Plan::new("-", 1, "png"), Plan::Stdout);
        assert_eq!(image_extension("a/b.JPG"), Some(("JPG", "jpeg")));
        assert_eq!(image_extension("x-{seq}.png"), Some(("png", "png")));
        assert_eq!(image_extension("x.{date}"), None);
        for (suffix, format) in [
            ("PnG", "png"),
            ("JpG", "jpeg"),
            ("JpEg", "jpeg"),
            ("RaW", "raw"),
        ] {
            let path = format!("é-{{seq}}.{suffix}");
            assert_eq!(image_extension(&path), Some((suffix, format)));
        }
        for path in [
            "shot.p{n}g",
            "shot.png}",
            "shot.ｐｎｇ",
            "shot.jpeg/",
            "shot.",
        ] {
            assert_eq!(image_extension(path), None);
        }
        assert_eq!(image_extension("clip.mp4"), None);
        // Stills are PNG, JPEG or raw; other image names say nothing.
        assert_eq!(image_extension("shot.pgm"), None);
        assert_eq!(image_extension("shot.ppm"), None);
        assert!(format_mismatch("y.jpeg", "jpeg").is_none());
        assert!(format_mismatch("y.jpg", "jpeg").is_none());
        assert_eq!(
            format_mismatch("out/y.jpg", "png").unwrap(),
            "y.jpg names a JPEG file but the format is png; rename it to y.png or drop --format"
        );
    }

    #[test]
    fn sequences_need_a_template_or_folder() {
        let hint = sequence_needs_template("shot.jpg", 3).unwrap();
        assert!(
            hint.contains("-o 'shot-{seq}.jpg'") && hint.contains("-o shot/"),
            "{hint}"
        );
        assert!(
            sequence_needs_template("SHOT.PNG", 2)
                .unwrap()
                .contains("SHOT-{seq}.PNG")
        );
        for allowed in ["shots", "shot-{seq}.jpg", "-", "clip.mp4"] {
            assert!(sequence_needs_template(allowed, 3).is_none(), "{allowed}");
        }
        assert!(sequence_needs_template("shot.jpg", 1).is_none());
        let dir = temp("seqdir").join("d.png");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(sequence_needs_template(dir.to_str().unwrap(), 3).is_none());
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }
}
