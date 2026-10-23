//! Shared, side-effect-free argument normalization. Live path containment is
//! checked separately, immediately before execution.
use super::{Issue, IssueCode, context::Context, context::Zone};
use chrono::{
    DateTime, FixedOffset, Local, LocalResult, NaiveDateTime, NaiveTime, Offset, SecondsFormat,
    TimeZone,
};
use std::path::{Component, Path, PathBuf};

/// Parse a duration and keep its normalized spelling in the checked call.
/// `minimum_ms` is 1 for intervals and 1,000 for durations.
pub fn duration(field: &str, text: &str, minimum_ms: u64) -> Result<(u64, String), Issue> {
    let millis =
        super::parse_duration_field(text).map_err(|message| Issue::usage(Some(field), message))?;
    if millis < minimum_ms {
        return Err(Issue::usage(
            Some(field),
            format!(
                "Choose a duration of at least {}.",
                super::format_duration_field(minimum_ms)
            ),
        ));
    }
    Ok((millis, super::format_duration_field(millis)))
}

/// A time pinned to an instant on the first check. Later checks consume the
/// canonical RFC3339 value rather than resolving a wall-clock time again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedTime {
    pub unix_ms: u64,
    pub canonical: String,
    pub issues: Vec<Issue>,
}

/// Parse `HH:MM` in the context's time zone or an RFC3339 instant with an
/// explicit offset. For `until`, `first_at_ms` is the first scheduled frame.
pub fn parse_time(
    ctx: &Context,
    field: &str,
    text: &str,
    first_at_ms: Option<u64>,
) -> Result<PinnedTime, Issue> {
    match ctx.zone {
        Zone::Local => time_in_zone(ctx.now_ms, Local, field, text, first_at_ms),
        Zone::Fixed(seconds) => {
            let offset = FixedOffset::east_opt(seconds).ok_or_else(|| {
                Issue::usage(Some(field), "Choose a valid local time zone offset.")
            })?;
            time_in_zone(ctx.now_ms, offset, field, text, first_at_ms)
        }
    }
}

fn time_in_zone<T: TimeZone>(
    now_ms: u64,
    zone: T,
    field: &str,
    text: &str,
    first_at_ms: Option<u64>,
) -> Result<PinnedTime, Issue> {
    let text = text.trim();
    let invalid = || {
        Issue::usage(
            Some(field),
            "Enter a time as HH:MM or an RFC3339 date with its offset, such as 2026-10-18T17:00:00+02:00.",
        )
    };
    let now = i64::try_from(now_ms)
        .ok()
        .and_then(|millis| zone.timestamp_millis_opt(millis).single())
        .ok_or_else(invalid)?;
    let mut issues = Vec::new();
    let time = if is_clock(text) {
        let clock = NaiveTime::parse_from_str(text, "%H:%M").map_err(|_| invalid())?;
        let mut date = now.date_naive();
        let (mut resolved, mut ambiguous) =
            resolve_local(&zone, date.and_time(clock), field, text)?;
        if resolved.timestamp_millis() <= now.timestamp_millis() {
            date = date.succ_opt().ok_or_else(invalid)?;
            (resolved, ambiguous) = resolve_local(&zone, date.and_time(clock), field, text)?;
            let span = u64::try_from(resolved.timestamp_millis() - now.timestamp_millis())
                .unwrap_or_default();
            let action = if field == "until" {
                "runs until tomorrow"
            } else {
                "starts tomorrow"
            };
            issues.push(Issue::warning(
                Some(field),
                format!(
                    "{text} has already passed today; this {action} at {text} ({}).",
                    human_span(span)
                ),
            ));
        }
        if ambiguous {
            issues.push(Issue::warning(
                Some(field),
                format!("{text} occurs twice on {date}; this uses its first occurrence."),
            ));
        }
        resolved
    } else {
        DateTime::parse_from_rfc3339(text).map_err(|_| invalid())?
    };
    let unix_ms = u64::try_from(time.timestamp_millis()).map_err(|_| invalid())?;
    if field == "start_at" && unix_ms < now_ms.saturating_sub(60_000) {
        return Err(Issue::usage(
            Some(field),
            "Choose a start time in the future.",
        ));
    }
    if field == "until" && unix_ms <= first_at_ms.unwrap_or(now_ms) {
        return Err(Issue::usage(
            Some(field),
            "Choose an end time after the start.",
        ));
    }
    if unix_ms > now_ms.saturating_add(crate::scheduling::MAX_DURATION_MS) {
        return Err(Issue::usage(
            Some(field),
            "Choose a time within the next 10 years.",
        ));
    }
    if unix_ms > now_ms.saturating_add(86_400_000) {
        let action = if field == "until" { "ends" } else { "starts" };
        issues.push(Issue::warning(
            Some(field),
            format!(
                "This {action} on {}; check the full date.",
                time.format("%Y-%m-%d at %H:%M %:z")
            ),
        ));
    }
    // Commands operate on milliseconds. Canonicalize any finer RFC3339
    // precision to the exact instant the command will use.
    let canonical = DateTime::from_timestamp_millis(time.timestamp_millis())
        .ok_or_else(invalid)?
        .with_timezone(time.offset())
        .to_rfc3339_opts(SecondsFormat::AutoSi, false);
    Ok(PinnedTime {
        unix_ms,
        canonical,
        issues,
    })
}

fn is_clock(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 5
        && bytes[2] == b':'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 2 || byte.is_ascii_digit())
}

fn resolve_local<T: TimeZone>(
    zone: &T,
    local: NaiveDateTime,
    field: &str,
    clock: &str,
) -> Result<(DateTime<FixedOffset>, bool), Issue> {
    let (time, ambiguous) = match zone.from_local_datetime(&local) {
        LocalResult::Single(time) => (time, false),
        LocalResult::Ambiguous(first, second) => {
            let earliest = if first.timestamp_millis() <= second.timestamp_millis() {
                first
            } else {
                second
            };
            (earliest, true)
        }
        LocalResult::None => {
            return Err(Issue::usage(
                Some(field),
                format!(
                    "Choose another time; {clock} does not exist on {}.",
                    local.date()
                ),
            ));
        }
    };
    let offset = time.offset().fix();
    Ok((time.with_timezone(&offset), ambiguous))
}

fn human_span(millis: u64) -> String {
    let minutes = millis.div_ceil(60_000);
    match (minutes / 60, minutes % 60) {
        (0, minutes) => format!("{minutes} min"),
        (hours, 0) => format!("{hours} h"),
        (hours, minutes) => format!("{hours} h {minutes} min"),
    }
}

/// Long-running work needs the app and computer to remain awake.
pub fn awake_warning(ctx: &Context, end_ms: u64) -> Option<Issue> {
    (end_ms > ctx.now_ms.saturating_add(600_000)).then(|| {
        Issue::warning(
            None,
            format!(
                "Keep Capturefab open and this computer awake until {}. Photos missed during sleep are taken together on wake.",
                ctx.zone.at(end_ms).format("%H:%M")
            ),
        )
    })
}

const FILE_NAME_HELP: &str =
    "Enter a file name. Choose a folder destination to save somewhere else.";

/// Validate the single file-name template before it joins any output root.
pub fn validate_file_name(name: &str) -> Result<(), Issue> {
    if name.is_empty()
        || name.len() > 200
        || name.starts_with(['.', '~'])
        || name.contains("..")
        || name.contains(['/', '\\', ':'])
        || name.chars().any(char::is_control)
        || !Path::new(name).is_relative()
    {
        return Err(Issue::usage(Some("file_name"), FILE_NAME_HELP));
    }
    crate::naming::validate_template(name)
        .map_err(|error| Issue::usage(Some("file_name"), super::sentence(&error.to_string())))
}

/// Normalize photo and schedule names. JPEG's default extension is `.jpeg`;
/// `.jpg` is accepted as the same format, matching the existing CLI.
pub fn photo_file_name(
    input: Option<&str>,
    format: &str,
    count: u32,
    schedule: bool,
) -> Result<String, Issue> {
    let extension = match format {
        "png" | "raw" | "pgm" | "ppm" => format,
        "jpeg" => "jpeg",
        _ => {
            return Err(Issue::usage(
                Some("format"),
                "Choose PNG, JPEG, raw, PGM or PPM photos.",
            ));
        }
    };
    let mut name = input.map_or_else(
        || format!("{{camera}}-{{datetime}}-{{seq}}.{extension}"),
        str::to_owned,
    );
    validate_file_name(&name)?;
    let actual = Path::new(&name)
        .extension()
        .and_then(|extension| extension.to_str());
    match actual {
        None => name.push_str(&format!(".{extension}")),
        Some(actual)
            if actual.eq_ignore_ascii_case(extension)
                || (format == "jpeg" && actual.eq_ignore_ascii_case("jpg")) => {}
        Some(_) => {
            return Err(Issue::usage(
                Some("file_name"),
                format!(
                    "Use the .{extension} extension for {} photos, or change the format.",
                    format.to_ascii_uppercase()
                ),
            ));
        }
    }
    if (count > 1 || schedule) && !crate::naming::has_placeholder(&name) {
        let extension_at = name.rfind('.').expect("normalized extension");
        name.insert_str(extension_at, "-{seq}");
    }
    validate_file_name(&name)?;
    Ok(name)
}

/// Normalize recording names and enforce the chosen container's codec rule.
pub fn recording_file_name(input: Option<&str>, codec: &str) -> Result<String, Issue> {
    let mut name = input.unwrap_or("{camera}-{datetime}.mp4").to_owned();
    validate_file_name(&name)?;
    let extension = Path::new(&name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        None => name.push_str(".mp4"),
        Some("mkv" | "mp4" | "mov" | "ts") => {}
        Some("webm") if codec == "av1" => {}
        Some("webm") => {
            return Err(Issue::usage(
                Some("file_name"),
                "Use AV1 for a .webm recording, or choose .mp4, .mkv, .mov or .ts.",
            ));
        }
        Some(_) => {
            return Err(Issue::usage(
                Some("file_name"),
                "Use .mp4, .mkv, .mov, .ts or .webm for recordings (.webm requires AV1).",
            ));
        }
    }
    validate_file_name(&name)?;
    Ok(name)
}

/// Join a validated template beneath an absolute capture root, without I/O.
/// Execution must also call `contain_path` immediately before writing.
pub fn capture_path(root: &Path, file_name: &str) -> Result<PathBuf, Issue> {
    validate_file_name(file_name)?;
    if !root.is_absolute()
        || root
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(Issue::error(
            Some("destination"),
            "Choose an absolute capture folder in Settings.",
            IssueCode::Unavailable,
        ));
    }
    Ok(root.join(file_name))
}

const FOLDER_HELP: &str = "Choose a folder for captures, such as ~/Captures/line-1.";

/// Expand `~/` and reject sensitive destination locations using lexical
/// components alone. Execution repeats these checks after resolving symlinks.
pub fn folder_path(
    text: &str,
    home: &Path,
    session: &Path,
    state: &Path,
) -> Result<PathBuf, Issue> {
    let text = text.trim();
    let path = match text.strip_prefix("~/") {
        Some(suffix) => home.join(suffix),
        None if text == "~" => home.to_owned(),
        None => PathBuf::from(text),
    };
    let hidden_component = path
        .as_os_str()
        .to_string_lossy()
        .split(std::path::is_separator)
        .any(|component| component.starts_with('.'));
    if text.is_empty()
        || text.chars().any(char::is_control)
        || !path.is_absolute()
        || path.file_name().is_none()
        || hidden_component
        || path == home
        || path.starts_with(session)
        || path.starts_with(state)
        || (cfg!(target_os = "macos") && path.starts_with(home.join("Library")))
    {
        return Err(Issue::usage(Some("path"), FOLDER_HELP));
    }
    Ok(path)
}

/// Execution-time containment through existing symlinks, including a dangling
/// output whose nearest existing parent is a symlink. Never creates anything.
pub fn contain_path(root: &Path, candidate: &Path) -> Result<(), Issue> {
    let root = existing_ancestor(root).map_err(path_problem)?;
    let candidate = existing_ancestor(candidate).map_err(path_problem)?;
    if !candidate.starts_with(root) {
        return Err(Issue::usage(
            Some("file_name"),
            "The output leaves its capture folder through a symbolic link. Choose another file name.",
        ));
    }
    Ok(())
}

/// Pin the physical capture root while retaining any not-yet-created tail.
pub fn canonical_root(root: &Path) -> Result<PathBuf, Issue> {
    existing_ancestor(root).map_err(path_problem)
}

/// Repeat destination refusals on the physical location just before creating
/// the folder. This closes aliases into protected session and state roots.
pub fn folder_path_live(
    path: &Path,
    home: &Path,
    session: &Path,
    state: &Path,
) -> Result<(), Issue> {
    let path = existing_ancestor(path).map_err(path_problem)?;
    let home = existing_ancestor(home).map_err(path_problem)?;
    let session = existing_ancestor(session).map_err(path_problem)?;
    let state = existing_ancestor(state).map_err(path_problem)?;
    let text = path
        .to_str()
        .ok_or_else(|| Issue::usage(Some("path"), FOLDER_HELP))?;
    folder_path(text, &home, &session, &state).map(|_| ())
}

fn path_problem(error: std::io::Error) -> Issue {
    Issue::error(
        Some("path"),
        format!("The capture folder cannot be inspected: {error}."),
        IssueCode::Unavailable,
    )
}

fn existing_ancestor(path: &Path) -> std::io::Result<PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => std::fs::canonicalize(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or(error)?;
            let name = path.file_name().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid capture folder")
            })?;
            Ok(existing_ancestor(parent)?.join(name))
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, NaiveDate, Timelike};

    fn context(now: &str) -> Context {
        let mut ctx = crate::tools::tests::fixture_context(Default::default());
        ctx.now_ms = DateTime::parse_from_rfc3339(now)
            .unwrap()
            .timestamp_millis() as u64;
        ctx.zone = Zone::Fixed(0);
        ctx
    }

    #[test]
    fn durations_have_units_bounds_and_canonical_output() {
        assert_eq!(
            duration("duration", "1h30m", 1000).unwrap(),
            (5_400_000, "1h30m".into())
        );
        assert_eq!(duration("duration", "1 hour", 1000).unwrap().0, 3_600_000);
        assert_eq!(duration("duration", "90s", 1000).unwrap().1, "1m30s");
        assert_eq!(duration("every", "1ms", 1).unwrap().0, 1);
        assert!(
            duration("duration", "999ms", 1000)
                .unwrap_err()
                .message
                .contains("at least 1s")
        );
        assert_eq!(
            duration("duration", "2", 1000).unwrap_err().message,
            "Add a unit, for example 90s, 15m or 1h30m."
        );
        assert!(duration("duration", "30m1h", 1000).is_err());
        assert!(duration("duration", "999999999d", 1000).is_err());
    }

    #[test]
    fn times_pin_once_and_preserve_offsets() {
        let ctx = context("2026-10-18T16:58:00Z");
        let first = parse_time(&ctx, "until", "17:00", None).unwrap();
        assert_eq!(first.canonical, "2026-10-18T17:00:00+00:00");
        let later = context("2026-10-18T17:01:00Z");
        let pinned = parse_time(&later, "until", &first.canonical, Some(ctx.now_ms)).unwrap();
        assert_eq!(pinned.unix_ms, first.unix_ms);
        assert!(parse_time(&later, "until", &first.canonical, None).is_err());
        let offset = parse_time(&ctx, "until", "2026-10-18T19:00:00+02:00", None).unwrap();
        assert_eq!(offset.unix_ms, first.unix_ms);
        assert!(offset.canonical.ends_with("+02:00"));
    }

    #[test]
    fn times_warn_about_tomorrow_and_full_dates_and_reject_past_starts() {
        let ctx = context("2026-10-18T18:00:00Z");
        let until = parse_time(&ctx, "until", "17:00", None).unwrap();
        assert_eq!(until.canonical, "2026-10-19T17:00:00+00:00");
        assert_eq!(
            until.issues[0].message,
            "17:00 has already passed today; this runs until tomorrow at 17:00 (23 h)."
        );
        let future = parse_time(&ctx, "start_at", "2026-10-21T17:00:00Z", None).unwrap();
        assert!(future.issues[0].message.contains("2026-10-21"));
        assert_eq!(
            parse_time(&ctx, "start_at", "2026-10-18T17:58:59Z", None)
                .unwrap_err()
                .message,
            "Choose a start time in the future."
        );
        assert!(parse_time(&ctx, "start_at", "2026-10-18T17:59:00Z", None).is_ok());
        for invalid in [
            "25:00",
            "9:30",
            "2026-10-18T19:00:00",
            "",
            "2050-01-01T00:00:00Z",
        ] {
            assert!(
                parse_time(&ctx, "until", invalid, None).is_err(),
                "{invalid}"
            );
        }
        assert!(awake_warning(&ctx, ctx.now_ms + 600_000).is_none());
        assert!(awake_warning(&ctx, ctx.now_ms + 600_001).is_some());
    }

    /// Deterministic daylight-saving fixture; never changes the process zone.
    #[derive(Clone, Copy)]
    struct Dst;

    impl TimeZone for Dst {
        type Offset = FixedOffset;

        fn from_offset(_: &Self::Offset) -> Self {
            Self
        }
        fn offset_from_local_date(&self, date: &NaiveDate) -> LocalResult<Self::Offset> {
            self.offset_from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
        }
        fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<Self::Offset> {
            let winter = FixedOffset::west_opt(5 * 3600).unwrap();
            let summer = FixedOffset::west_opt(4 * 3600).unwrap();
            match (local.month(), local.day(), local.hour()) {
                (3, 8, 2) => LocalResult::None,
                (11, 1, 1) => LocalResult::Ambiguous(winter, summer),
                (4..=10, _, _) | (3, 8..=31, _) => LocalResult::Single(summer),
                _ => LocalResult::Single(winter),
            }
        }
        fn offset_from_utc_date(&self, date: &NaiveDate) -> Self::Offset {
            self.offset_from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
        }
        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> Self::Offset {
            let summer = (4..=10).contains(&utc.month())
                || (utc.month() == 3 && (utc.day(), utc.hour()) >= (8, 7))
                || (utc.month() == 11 && (utc.day(), utc.hour()) < (1, 6));
            FixedOffset::west_opt(if summer { 4 * 3600 } else { 5 * 3600 }).unwrap()
        }
    }

    #[test]
    fn nonexistent_local_times_fail_and_ambiguous_times_choose_earliest() {
        let march = context("2026-03-08T05:00:00Z");
        let error = time_in_zone(march.now_ms, Dst, "until", "02:30", None).unwrap_err();
        assert_eq!(
            error.message,
            "Choose another time; 02:30 does not exist on 2026-03-08."
        );
        let november = context("2026-11-01T04:00:00Z");
        let time = time_in_zone(november.now_ms, Dst, "until", "01:30", None).unwrap();
        assert_eq!(time.canonical, "2026-11-01T01:30:00-04:00");
        assert!(time.issues[0].message.contains("occurs twice"));
    }

    #[test]
    fn photo_names_get_extensions_and_sequences_without_directories() {
        assert_eq!(
            photo_file_name(None, "png", 1, false).unwrap(),
            "{camera}-{datetime}-{seq}.png"
        );
        assert_eq!(
            photo_file_name(Some("shot"), "png", 1, false).unwrap(),
            "shot.png"
        );
        assert_eq!(
            photo_file_name(Some("shot.png"), "png", 2, false).unwrap(),
            "shot-{seq}.png"
        );
        assert_eq!(
            photo_file_name(Some("shot.jpeg"), "jpeg", 1, true).unwrap(),
            "shot-{seq}.jpeg"
        );
        assert_eq!(
            photo_file_name(Some("shot.JPG"), "jpeg", 1, false).unwrap(),
            "shot.JPG"
        );
        assert_eq!(
            photo_file_name(Some("{camera}.png"), "png", 2, true).unwrap(),
            "{camera}.png"
        );
        assert_eq!(
            photo_file_name(Some("shot.jpg"), "png", 1, false)
                .unwrap_err()
                .message,
            "Use the .png extension for PNG photos, or change the format."
        );
    }

    #[test]
    fn unsafe_or_unknown_file_templates_are_refused() {
        for invalid in [
            "",
            ".capture",
            "../shot",
            "shot..png",
            "~/shot",
            "/shot",
            "a/b",
            "a\\b",
            "C:shot",
            "a\nb",
            "{unknown}.png",
            "{seq}{frame}.png",
        ] {
            assert!(
                photo_file_name(Some(invalid), "png", 1, false).is_err(),
                "{invalid}"
            );
        }
        assert!(photo_file_name(Some(&"x".repeat(200)), "png", 1, false).is_err());
        assert!(
            photo_file_name(Some(&format!("{}.png", "x".repeat(195))), "png", 2, false).is_err()
        );
        assert!(photo_file_name(Some("é"), "png", 1, false).is_ok());
    }

    #[test]
    fn recording_names_enforce_containers_and_codec() {
        assert_eq!(
            recording_file_name(None, "h264").unwrap(),
            "{camera}-{datetime}.mp4"
        );
        assert_eq!(recording_file_name(Some("run"), "hevc").unwrap(), "run.mp4");
        for extension in ["mkv", "mp4", "mov", "ts", "webm"] {
            assert!(recording_file_name(Some(&format!("run.{extension}")), "av1").is_ok());
        }
        assert!(recording_file_name(Some("run.webm"), "h264").is_err());
        assert!(recording_file_name(Some("run.png"), "av1").is_err());
    }

    #[test]
    fn folder_checks_are_lexical_and_reject_sensitive_roots() {
        let base = std::env::temp_dir().join("capturefab-values-shape");
        let home = base.join("home");
        let session = base.join("session");
        let state = base.join("state");
        assert_eq!(
            folder_path("~/Captures/line-1", &home, &session, &state).unwrap(),
            home.join("Captures/line-1")
        );
        for invalid in [
            "/".to_owned(),
            "~".to_owned(),
            home.to_string_lossy().into_owned(),
            home.join(".ssh/captures").to_string_lossy().into_owned(),
            base.join("safe/../etc").to_string_lossy().into_owned(),
            base.join("safe/./shots").to_string_lossy().into_owned(),
            session.join("shots").to_string_lossy().into_owned(),
            state.join("shots").to_string_lossy().into_owned(),
            "relative/shots".to_owned(),
        ] {
            assert_eq!(
                folder_path(&invalid, &home, &session, &state)
                    .unwrap_err()
                    .message,
                FOLDER_HELP,
                "{invalid}"
            );
        }
        #[cfg(target_os = "macos")]
        assert!(folder_path("~/Library/LaunchAgents", &home, &session, &state).is_err());
        let captures = base.join("captures");
        assert_eq!(
            capture_path(&captures, "shot.png").unwrap(),
            captures.join("shot.png")
        );
        assert!(capture_path(&captures, "../shot.png").is_err());
        assert!(capture_path(Path::new("relative"), "shot.png").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn live_paths_cannot_escape_or_alias_protected_roots() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "capturefab-tool-values-{}-{nonce}",
            std::process::id()
        ));
        let captures = root.join("captures");
        let outside = root.join("outside");
        let session = root.join("session");
        let home = root.join("home");
        for path in [&captures, &outside, &session, &home] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::os::unix::fs::symlink(&outside, captures.join("escaped.png")).unwrap();
        std::os::unix::fs::symlink(&session, captures.join("alias")).unwrap();
        assert!(contain_path(&captures, &captures.join("new.png")).is_ok());
        assert!(contain_path(&captures, &captures.join("escaped.png")).is_err());
        assert!(
            folder_path_live(
                &captures.join("alias/new"),
                &home,
                &session,
                &root.join("state")
            )
            .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
