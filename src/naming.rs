//! Output file naming: templates with {camera}, {date}, {time}, {datetime},
//! {seq} and {frame}, and what to do when a name is taken. Names are picked
//! only in the camera worker; the CLI and GUI validate and preview.
use crate::{storage::OutputExists, types::CameraInfo};
use anyhow::{Context as _, Result, bail, ensure};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
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
    pub fn is_fail(&self) -> bool {
        *self == Self::Fail
    }
}

pub const PLACEHOLDERS: [&str; 6] = ["camera", "date", "time", "datetime", "seq", "frame"];
/// Names tried for one file before giving up on races with other writers.
pub const MAX_ATTEMPTS: u32 = 64;
/// Directory entries read to find the highest {seq} already used.
pub const MAX_SCAN_ENTRIES: usize = 200_000;
pub const MAX_SEQ: u32 = 99_999_999;
const CACHE_KEYS: usize = 256;
const PLACEHOLDER_ERROR: &str = "capture output supports only the {camera}, {date}, {time}, {datetime}, {seq} and {frame} placeholders";

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Lit(String),
    Camera,
    Date,
    Time,
    DateTime,
    Seq,
    Frame,
}

fn tokens(template: &str) -> Result<Vec<Token>> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find(['{', '}']) {
        ensure!(rest.as_bytes()[open] == b'{', PLACEHOLDER_ERROR);
        if open > 0 {
            out.push(Token::Lit(rest[..open].into()));
        }
        let close = rest[open..].find('}').context(PLACEHOLDER_ERROR)? + open;
        out.push(match &rest[open + 1..close] {
            "camera" => Token::Camera,
            "date" => Token::Date,
            "time" => Token::Time,
            "datetime" => Token::DateTime,
            "seq" => Token::Seq,
            "frame" => Token::Frame,
            _ => bail!(PLACEHOLDER_ERROR),
        });
        rest = &rest[close + 1..];
    }
    if !rest.is_empty() {
        out.push(Token::Lit(rest.into()));
    }
    Ok(out)
}

/// Check placeholder names and where {seq} may appear.
pub fn validate_template(output: &str) -> Result<()> {
    let tokens = tokens(output)?;
    let Some(seq) = tokens.iter().position(|t| *t == Token::Seq) else {
        return Ok(());
    };
    ensure!(
        tokens.iter().filter(|t| **t == Token::Seq).count() == 1,
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
        Some(Token::Date | Token::Time | Token::DateTime | Token::Frame) => {
            bail!(
                "{{seq}} cannot be followed directly by {{date}}, {{time}}, {{datetime}} or {{frame}}"
            )
        }
        _ => Ok(()),
    }
}

pub fn has_placeholder(output: &str) -> bool {
    PLACEHOLDERS
        .iter()
        .any(|p| output.contains(&format!("{{{p}}}")))
}

/// A template written once per frame needs a name that changes per frame.
pub fn validate_for_count(output: &str, count: u32, on_exists: OnExists) -> Result<()> {
    ensure!(
        count <= 1
            || !has_placeholder(output)
            || output.contains("{frame}")
            || output.contains("{seq}")
            || on_exists == OnExists::Number,
        "a multi-frame template needs {{frame}} or {{seq}}, or on_exists number"
    );
    Ok(())
}

/// Check a capture, schedule or recording request before it is queued.
pub fn validate_request(
    output: &str,
    count: u32,
    on_exists: OnExists,
    context: &crate::context::Context,
) -> Result<()> {
    crate::context::validate(context)?;
    // Network URLs keep their own checks, which refuse braces.
    if output != "-" && crate::media::is_recording(output) {
        validate_template(output)?;
        validate_for_count(output, count, on_exists)?;
    }
    Ok(())
}

/// A file-name-safe camera label, `model-serial`. It never uses the camera's
/// ID or address, so URL credentials cannot reach a file name.
pub fn camera_label(info: &CameraInfo) -> String {
    let raw = format!("{}-{}", info.model, info.serial);
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
        label = label.trim_end_matches(['-', '.']).to_owned();
    }
    let stem = label.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0');
    if label.is_empty() || reserved {
        "camera".into()
    } else {
        label
    }
}

/// Show what a template expands to now; {seq} is shown as `####`.
pub fn preview(template: &str, camera: &str, now: DateTime<Local>) -> String {
    preview_numbered(template, camera, now, None)
}

/// `preview` with {seq} as `seq` when it is known, else `####`.
pub fn preview_numbered(
    template: &str,
    camera: &str,
    now: DateTime<Local>,
    seq: Option<u32>,
) -> String {
    let Ok(tokens) = tokens(template) else {
        return template.into();
    };
    tokens
        .iter()
        .map(|t| match (t, seq) {
            (Token::Seq, Some(seq)) => format!("{seq:04}"),
            (Token::Seq, None) => "####".into(),
            (t, _) => expand(t, camera, &now, 0),
        })
        .collect()
}

/// The number {seq} would take in `template`'s next file now: one past the
/// highest already in its folder, past any taken name. Read-only: it
/// reserves nothing, creates nothing, and shares no state with the namer
/// that saves, so the file saved may still take a later number. `None`
/// when the template has no {seq} or names no file.
pub fn peek_next(template: &str, camera: &str, now: DateTime<Local>) -> Option<u32> {
    if !tokens(template).ok()?.contains(&Token::Seq) {
        return None;
    }
    let input = NameInput {
        camera,
        now,
        index: 0,
    };
    let mut namer = Namer::default();
    namer
        .candidate(
            &Plan::Template(template.to_owned()),
            &input,
            OnExists::Number,
        )
        .ok()?;
    namer.next.values().next().copied()
}

fn expand(token: &Token, camera: &str, now: &DateTime<Local>, index: u32) -> String {
    match token {
        Token::Lit(s) => s.clone(),
        Token::Camera => camera.into(),
        Token::Date => now.format("%Y-%m-%d").to_string(),
        Token::Time => now.format("%H%M%S").to_string(),
        Token::DateTime => now.format("%Y%m%d-%H%M%S").to_string(),
        Token::Frame => format!("{:06}", index.saturating_add(1)),
        Token::Seq => unreachable!("{{seq}} is chosen by the namer"),
    }
}

/// `Ok` when nothing is at `path`; `OutputExists` when anything is, even a
/// dangling symbolic link.
pub fn check_free(path: &Path) -> Result<()> {
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
pub enum Plan {
    Stdout,
    File(PathBuf),
    /// `frame-NNNNNN.ext`, continuing after the highest existing number.
    Directory {
        dir: PathBuf,
        ext: String,
    },
    Template(String),
}
impl Plan {
    /// The same file and directory rules capture has always used.
    pub fn new(output: &str, count: u32, format: &str) -> Plan {
        if output == "-" {
            Plan::Stdout
        } else if has_placeholder(output) {
            Plan::Template(output.into())
        } else if count > 1 || Path::new(output).is_dir() {
            Plan::Directory {
                dir: output.into(),
                ext: format.into(),
            }
        } else {
            Plan::File(output.into())
        }
    }
    /// Whether every frame's name is known in advance.
    pub fn is_static(&self) -> bool {
        match self {
            Plan::File(_) => true,
            Plan::Template(t) => tokens(t).is_ok_and(|tokens| {
                tokens
                    .iter()
                    .all(|t| matches!(t, Token::Lit(_) | Token::Frame))
            }),
            Plan::Stdout | Plan::Directory { .. } => false,
        }
    }
    fn describe(&self) -> String {
        match self {
            Plan::Stdout => "-".into(),
            Plan::File(path) | Plan::Directory { dir: path, .. } => path.display().to_string(),
            Plan::Template(t) => t.clone(),
        }
    }
}

/// Whether a taken name is replaced by the next free one rather than failing.
pub fn renumbers(plan: &Plan, on_exists: OnExists) -> bool {
    match plan {
        Plan::Stdout => false,
        Plan::Directory { .. } => true,
        Plan::Template(t) if t.contains("{seq}") => true,
        _ => on_exists == OnExists::Number,
    }
}

/// What a name is built from, taken once per frame before it is requested.
pub struct NameInput<'a> {
    pub camera: &'a str,
    pub now: DateTime<Local>,
    pub index: u32,
}

/// One part of a numbered file name. A stamp matches any text of its length
/// made of digits and dashes, so numbering continues across seconds and days.
enum Part {
    Lit(String),
    Stamp(String),
    Seq,
}

/// Picks free names, remembering the last number used per folder and pattern
/// so that names it handed out are not handed out again before they exist.
#[derive(Default)]
pub struct Namer {
    next: HashMap<(PathBuf, String), u32>,
}
impl Namer {
    /// Never creates files. Returns a name that is free right now, or
    /// `OutputExists` when a name without {seq} is taken in fail mode.
    pub fn candidate(
        &mut self,
        plan: &Plan,
        input: &NameInput,
        on_exists: OnExists,
    ) -> Result<PathBuf> {
        match plan {
            Plan::Stdout => bail!("stdout output has no file name"),
            Plan::Directory { dir, ext } => self.numbered(
                dir,
                vec![
                    Part::Lit("frame-".into()),
                    Part::Seq,
                    Part::Lit(format!(".{ext}")),
                ],
                6,
                plan,
            ),
            Plan::File(path) => self.fixed(path.clone(), on_exists, plan),
            Plan::Template(template) => {
                let tokens = tokens(template)?;
                let Some(seq) = tokens.iter().position(|t| *t == Token::Seq) else {
                    let name: String = tokens
                        .iter()
                        .map(|t| expand(t, input.camera, &input.now, input.index))
                        .collect();
                    return self.fixed(name.into(), on_exists, plan);
                };
                // {seq} is in the final component: split the text before it
                // at the last separator into the folder and the name prefix.
                let before: String = tokens[..seq]
                    .iter()
                    .map(|t| expand(t, input.camera, &input.now, input.index))
                    .collect();
                let split = before.rfind(std::path::is_separator).map_or(0, |i| i + 1);
                let dir = PathBuf::from(&before[..split]);
                let mut parts = Vec::new();
                let mut lit = String::new();
                for t in tokens[..seq].iter().rev() {
                    let text = expand(t, input.camera, &input.now, input.index);
                    match t {
                        Token::Date | Token::Time | Token::DateTime => {
                            if let Some(i) = text.rfind(std::path::is_separator) {
                                lit.insert_str(0, &text[i + 1..]);
                                break;
                            }
                            if !lit.is_empty() {
                                parts.push(Part::Lit(std::mem::take(&mut lit)));
                            }
                            parts.push(Part::Stamp(text));
                        }
                        _ => match text.rfind(std::path::is_separator) {
                            Some(i) => {
                                lit.insert_str(0, &text[i + 1..]);
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
                    let text = expand(t, input.camera, &input.now, input.index);
                    match t {
                        Token::Date | Token::Time | Token::DateTime => {
                            parts.push(Part::Stamp(text))
                        }
                        _ => parts.push(Part::Lit(text)),
                    }
                }
                self.numbered(&dir, parts, 4, plan)
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
                self.numbered(&dir, parts, 4, plan)
            }
            Err(e) => Err(e),
        }
    }

    fn numbered(
        &mut self,
        dir: &Path,
        parts: Vec<Part>,
        width: usize,
        plan: &Plan,
    ) -> Result<PathBuf> {
        let key: String = parts
            .iter()
            .map(|p| match p {
                Part::Lit(s) => s.clone(),
                Part::Stamp(s) => format!("{{{}}}", s.len()),
                Part::Seq => "{seq}".into(),
            })
            .collect();
        let key = (dir.to_path_buf(), key);
        let start = match self.next.get(&key) {
            Some(&last) => last.saturating_add(1),
            None => highest(dir, &parts).saturating_add(1),
        };
        self.free(dir, &parts, width, plan, key, start)
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
            let name: String = parts
                .iter()
                .map(|p| match p {
                    Part::Lit(s) | Part::Stamp(s) => s.clone(),
                    Part::Seq => format!("{n:0width$}"),
                })
                .collect();
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

/// The highest number already used in `dir` for this pattern, or 0.
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
            matches(name.to_str()?, parts)
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
                let head = name.get(..s.len())?;
                if !head.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
                    return None;
                }
                name = &name[s.len()..];
            }
            Part::Seq => {
                let digits = name.bytes().take_while(u8::is_ascii_digit).count();
                if digits == 0 {
                    return None;
                }
                seq = name[..digits].parse().ok();
                name = &name[digits..];
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
pub fn save_named<T>(
    namer: &Mutex<Namer>,
    plan: &Plan,
    input: &NameInput,
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
                    .unwrap_or_else(|e| e.into_inner())
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
    use chrono::TimeZone;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "capturefab-naming-{name}-{}-{}",
            std::process::id(),
            crate::scheduling::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn input(now: DateTime<Local>) -> NameInput<'static> {
        NameInput {
            camera: "Model-SN1",
            now,
            index: 0,
        }
    }
    fn fixed_time() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, 17, 10, 15, 0).unwrap()
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
                .contains("supports only the {camera}")
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
            profile: Default::default(),
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
    fn expansion_and_preview() {
        let now = fixed_time();
        assert_eq!(
            preview("{date}/{camera}-{datetime}-{seq}.png", "A-1", now),
            "2026-10-17/A-1-20261017-101500-####.png"
        );
        assert_eq!(preview("t{time}-{frame}", "c", now), "t101500-000001");
        let dir = temp("expand");
        let template = format!("{}/{{date}}/{{camera}}-{{frame}}.png", dir.display());
        let path = Namer::default()
            .candidate(&Plan::Template(template), &input(now), OnExists::Fail)
            .unwrap();
        assert_eq!(path, dir.join("2026-10-17/Model-SN1-000001.png"));
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
        input.camera = "A";
        let first = namer.candidate(&plan, &input, OnExists::Fail).unwrap();
        assert_eq!(first, dir.join("A-20261017-101500-0013.png"));
        // A name handed out is not handed out again before it exists.
        input.now = fixed_time() + chrono::Duration::seconds(1);
        let second = namer.candidate(&plan, &input, OnExists::Fail).unwrap();
        assert_eq!(second, dir.join("A-20261017-101501-0014.png"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn peeking_the_next_number_reads_without_reserving() {
        let dir = temp("peek");
        let template = format!("{}/{{camera}}-{{datetime}}-{{seq}}.png", dir.display());
        assert_eq!(peek_next(&template, "A", fixed_time()), Some(1));
        let missing = format!("{}/missing/{{camera}}-{{seq}}.png", dir.display());
        assert_eq!(peek_next(&missing, "A", fixed_time()), Some(1));
        for name in ["A-20261017-101500-0007.png", "A-20261017-101501-0012.png"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        assert_eq!(peek_next(&template, "A", fixed_time()), Some(13));
        // Nothing was reserved, and nothing was created.
        assert_eq!(peek_next(&template, "A", fixed_time()), Some(13));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        let fixed = format!("{}/{{camera}}.png", dir.display());
        assert_eq!(peek_next(&fixed, "A", fixed_time()), None);
        assert_eq!(
            preview_numbered("{camera}-{seq}.png", "A", fixed_time(), Some(13)),
            "A-0013.png"
        );
        assert_eq!(
            preview("{camera}-{seq}.png", "A", fixed_time()),
            "A-####.png"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn existing_names_fail_or_number() {
        let dir = temp("exists");
        let file = dir.join("capture.png");
        std::fs::write(&file, b"x").unwrap();
        let plan = Plan::File(file.clone());
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
            std::fs::write(dir.join(format!("frame-{i:06}.raw")), b"x").unwrap();
        }
        let plan = Plan::new(dir.to_str().unwrap(), 1, "raw");
        assert!(matches!(plan, Plan::Directory { .. }) && !plan.is_static());
        assert_eq!(
            namer
                .candidate(&plan, &input(fixed_time()), OnExists::Fail)
                .unwrap(),
            dir.join("frame-000004.raw")
        );
        std::fs::remove_dir_all(dir).unwrap();
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
    fn requests_validate_context_and_templates() {
        let mut context = crate::context::Context::new();
        validate_request("out-{seq}.png", 3, OnExists::Fail, &context).unwrap();
        validate_request("rtsp://host/live", 1, OnExists::Fail, &context).unwrap();
        validate_request("-", 1, OnExists::Fail, &context).unwrap();
        assert!(validate_request("out-{nope}.png", 1, OnExists::Fail, &context).is_err());
        context.insert("Line".into(), "x".into());
        assert!(validate_request("out.png", 1, OnExists::Fail, &context).is_err());
    }
}
