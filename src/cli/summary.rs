//! One-line human summaries for capture, scheduled jobs, recordings and
//! storage. `--json` output never passes through here.
use crate::storage::human_bytes;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Names listed in full up to this many; longer lists show the first few,
/// a count and the last.
const LISTED: usize = 10;

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}

/// `640×480 Mono8` from a frame's metadata.
fn frame_shape(frame: &Value) -> Option<String> {
    let (width, height) = (frame["width"].as_u64()?, frame["height"].as_u64()?);
    let format = frame["pixel_format_name"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            frame["pixel_format"]
                .as_u64()
                .map(|format| crate::inspect::pixel_format_label(format as u32))
        })
        .unwrap_or_default();
    Some(format!("{width}×{height} {format}").trim_end().to_owned())
}

/// The format a file's extension names, as people write it: PNG, JPEG.
fn format_label(path: &str) -> String {
    match Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jpg" | "jpeg") => "JPEG".into(),
        Some(extension) => extension.to_ascii_uppercase(),
        None => "image".into(),
    }
}

/// The deepest folder holding every path.
fn common_folder(paths: &[&Path]) -> PathBuf {
    let mut folder = paths
        .first()
        .and_then(|path| path.parent())
        .map(Path::to_path_buf)
        .unwrap_or_default();
    while !paths.iter().all(|path| path.starts_with(&folder)) {
        if !folder.pop() {
            break;
        }
    }
    folder
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.into())
}

/// `Saved 3 PNG frames · 640×480 Mono8 · 832 KiB → out/` and the names, or
/// `Saved out/x.png · 640×480 Mono8 · 277 KiB` for one file.
pub(super) fn capture_summary(value: &Value) -> String {
    let files = value["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    let frames = value["frames"].as_array().cloned().unwrap_or_default();
    let mut details = Vec::new();
    if let Some(shape) = frames.first().and_then(frame_shape) {
        details.push(shape);
    }
    // The saved size, read from the disk for a worker that does not report it.
    let bytes = files
        .iter()
        .enumerate()
        .map(|(index, file)| {
            frames
                .get(index)
                .and_then(|frame| frame["file_bytes"].as_u64())
                .or_else(|| std::fs::metadata(file).ok().map(|meta| meta.len()))
        })
        .sum::<Option<u64>>();
    if let Some(bytes) = bytes {
        details.push(human_bytes(bytes));
    }
    let details = details
        .iter()
        .map(|detail| format!(" · {detail}"))
        .collect::<String>();
    let mut out = match files.as_slice() {
        [] => "Saved no files.\n".to_owned(),
        [file] => format!("Saved {file}{details}\n"),
        files => {
            let paths = files.iter().map(Path::new).collect::<Vec<_>>();
            let folder = common_folder(&paths);
            let kinds = files
                .iter()
                .map(|file| format_label(file))
                .collect::<Vec<_>>();
            let kind = if kinds.iter().all(|kind| *kind == kinds[0]) {
                format!("{} ", kinds[0])
            } else {
                String::new()
            };
            let mut out = format!(
                "Saved {} {kind}frames{details} → {}{}\n",
                files.len(),
                folder.display(),
                std::path::MAIN_SEPARATOR
            );
            let name = |path: &Path| {
                path.strip_prefix(&folder)
                    .unwrap_or(path)
                    .display()
                    .to_string()
            };
            if paths.len() <= LISTED {
                for path in &paths {
                    out.push_str(&format!("  {}\n", name(path)));
                }
            } else {
                for path in &paths[..3] {
                    out.push_str(&format!("  {}\n", name(path)));
                }
                out.push_str(&format!("  … {} more\n", paths.len() - 4));
                out.push_str(&format!("  {}\n", name(paths[paths.len() - 1])));
            }
            out
        }
    };
    if let Some(destination) = value["destination"].as_str() {
        out.push_str(&format!("Destination: {destination}\n"));
    }
    if let Some(context) = value["context"].as_object().filter(|tags| !tags.is_empty()) {
        let tags = context
            .iter()
            .map(|(key, value)| format!("{key}={}", value.as_str().unwrap_or_default()))
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str(&format!("Tags: {tags}\n"));
    }
    out
}

/// `1s`, `500ms`, `2m`, `1h30m`: a job's interval as it was typed.
pub(super) fn interval(ms: u64) -> String {
    if ms == 0 || !ms.is_multiple_of(1000) {
        return format!("{ms}ms");
    }
    let seconds = ms / 1000;
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    let mut out = String::new();
    for (value, unit) in [(hours, "h"), (minutes, "m"), (seconds, "s")] {
        if value > 0 {
            out.push_str(&format!("{value}{unit}"));
        }
    }
    out
}

/// `in 4s` for a time `ms` from now, `now` when it is due.
pub(super) fn relative(ms: u64) -> String {
    match ms {
        0..1000 => "now".into(),
        _ => format!("in {}", interval(ms / 1000 * 1000)),
    }
}

/// How many frames a job takes: `3 frames`, or `until cancelled`.
fn job_count(job: &Value) -> String {
    match job["count"].as_u64() {
        None | Some(0) => "until cancelled".into(),
        Some(1) => "1 frame".into(),
        Some(count) => format!("{count} frames"),
    }
}

/// What a session prints when it accepts a scheduled capture.
pub(super) fn job_scheduled(job: &Value, camera: Option<&str>, session: &str) -> String {
    let camera_flag = camera
        .map(|camera| format!(" --camera {}", crate::hint::shell_quote(camera)))
        .unwrap_or_default();
    let on = camera
        .map(|camera| format!(" on {camera}"))
        .unwrap_or_default();
    let session = crate::hint::shell_quote(session);
    format!(
        "Scheduled job {}{on}: {} every {} → {}\nWatch: capturefab --session {session} jobs   Cancel: capturefab --session {session}{camera_flag} cancel {}\n",
        job["id"],
        job_count(job),
        interval(job["interval_ms"].as_u64().unwrap_or(0)),
        text(job, "output"),
        job["id"],
    )
}

/// The stderr line a local schedule prints as each frame is saved.
pub(super) fn job_progress(job: &Value, now_ms: u64) -> String {
    let captured = job["captured"].as_u64().unwrap_or(0);
    let mut line = format!(
        "{captured}/{} saved {}",
        job["count"].as_u64().unwrap_or(0),
        job["last_file"].as_str().map(file_name).unwrap_or_default()
    );
    if job["status"] != "complete" && captured < job["count"].as_u64().unwrap_or(u64::MAX) {
        let next = job["next_at_ms"]
            .as_u64()
            .unwrap_or(0)
            .saturating_sub(now_ms);
        if next >= 1000 {
            line.push_str(&format!(" · next {}", relative(next)));
        }
    }
    line
}

/// The one-line result of a local schedule that ran to the end.
pub(super) fn job_finished(job: &Value) -> String {
    let captured = job["captured"].as_u64().unwrap_or(0);
    if captured == 0 {
        return "Saved 0 frames.\n".into();
    }
    let mut out = format!("Captured {captured} of {} frames", job["count"]);
    if let Some(last) = job["last_file"].as_str() {
        let folder = Path::new(last)
            .parent()
            .map(|parent| parent.display().to_string())
            .unwrap_or_default();
        out.push_str(&format!(
            " → {folder}{} (last: {})",
            std::path::MAIN_SEPARATOR,
            file_name(last)
        ));
    }
    for (key, label) in [("skipped", "skipped"), ("retries", "retried")] {
        if let Some(count) = job[key].as_u64().filter(|count| *count > 0) {
            out.push_str(&format!(" · {count} {label}"));
        }
    }
    out.push('\n');
    if let Some(error) = job["error"].as_str() {
        out.push_str(&format!("Error: {error}\n"));
    }
    out
}

/// The `jobs` table, or how to schedule one when there are none.
pub(super) fn jobs_table(jobs: &[Value], session: Option<&str>, now_ms: u64) -> String {
    let session = session.map(crate::hint::shell_quote);
    let flag = session
        .as_deref()
        .map(|session| format!("--session {session} "))
        .unwrap_or_default();
    if jobs.is_empty() {
        let place = session
            .as_deref()
            .map(|session| format!(" in session {session}"))
            .unwrap_or_default();
        return format!(
            "No capture jobs{place}. Schedule one with: capturefab {flag}--camera ID capture -n 10 --interval 1s -o 'frame-{{seq}}.png'\n"
        );
    }
    let mut out = format!(
        "{:<5} {:<10} {:<10} {:<9} OUTPUT\n",
        "ID", "STATUS", "PROGRESS", "NEXT"
    );
    for job in jobs {
        let next = if matches!(text(job, "status"), "pending" | "running") {
            relative(
                job["next_at_ms"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_sub(now_ms),
            )
        } else {
            "-".into()
        };
        let count = match job["count"].as_u64() {
            None | Some(0) => "∞".into(),
            Some(count) => count.to_string(),
        };
        out.push_str(&format!(
            "{:<5} {:<10} {:<10} {:<9} {}\n",
            job["id"].to_string(),
            text(job, "status"),
            format!("{}/{count}", job["captured"].as_u64().unwrap_or(0)),
            next,
            text(job, "output")
        ));
        if let Some(error) = job["error"].as_str() {
            out.push_str(&format!("      Error: {error}\n"));
        }
    }
    out
}

/// `storage`: how much of the budget is used, and what needs attention.
pub(super) fn storage_summary(value: &Value) -> String {
    let quota = if value["quota"].is_object() {
        &value["quota"]
    } else {
        value
    };
    let policy = &quota["policy"];
    let number = |value: &Value| value.as_u64().unwrap_or(0);
    let mut out = String::new();
    if value["quota"].is_object() {
        out.push_str("Saved the storage policy.\n");
    }
    let free = quota["free_bytes"]
        .as_u64()
        .map(|free| format!(" · {} free on disk", human_bytes(free)))
        .unwrap_or_default();
    out.push_str(&format!(
        "Using {} of {} ({} of {} files) · when full: {}{free}\n",
        human_bytes(number(&quota["allocated_bytes"])),
        human_bytes(number(&policy["max_bytes"])),
        number(&quota["files"]),
        number(&policy["max_files"]),
        policy["on_full"].as_str().unwrap_or("stop"),
    ));
    if let Some(age) = policy["max_age_seconds"].as_u64() {
        out.push_str(&format!(
            "Files older than {} are removed.\n",
            interval(age * 1000)
        ));
    }
    if quota["over_limit"] == true {
        out.push_str("Over limit: new saves stop until files are removed or the limits are raised (storage configure).\n");
    }
    let reservations = number(&quota["active_reservations"]);
    if reservations > 0 {
        out.push_str(&format!(
            "{reservations} file(s) being written now ({} reserved).\n",
            human_bytes(number(&quota["reserved_bytes"]))
        ));
    }
    let changed = number(&quota["externally_changed_files"]);
    if changed > 0 {
        out.push_str(&format!(
            "{changed} file(s) changed outside Capturefab; they are kept and never removed automatically.\n"
        ));
    }
    let retained = quota["retained_namespaces"].as_array().map_or(0, Vec::len);
    if retained > 0 {
        out.push_str(&format!(
            "{retained} retained task workspace(s); see capturefab storage task-data list.\n"
        ));
    }
    if let Some(ledger) = quota["ledger"].as_str() {
        out.push_str(&format!("Ledger: {ledger}\n"));
    }
    out.push_str("Use storage --json for full details.\n");
    out
}

/// `sim:0 · Capturefab Pattern camera · serial SIM0 · simulator · area_scan`
/// and the camera's capabilities.
pub(super) fn info_header(camera: &Value) -> String {
    let mut parts = vec![crate::media::redact_url(text(camera, "id"))];
    let name = format!("{} {}", text(camera, "vendor"), text(camera, "model"));
    if !name.trim().is_empty() {
        parts.push(name.trim().to_owned());
    }
    if !text(camera, "serial").is_empty() {
        parts.push(format!("serial {}", text(camera, "serial")));
    }
    if !text(camera, "transport").is_empty() {
        parts.push(text(camera, "transport").to_owned());
    }
    if let Some(address) = camera["address"]
        .as_str()
        .filter(|address| !address.is_empty())
    {
        parts.push(crate::media::redact_url(address));
    }
    if let Some(kind) = camera["profile"]["kind"].as_str() {
        parts.push(kind.to_owned());
    }
    let mut out = parts.join(" · ");
    out.push('\n');
    let capabilities = camera["profile"]["capabilities"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    if !capabilities.is_empty() {
        out.push_str(&format!("Capabilities: {}\n", capabilities.join(", ")));
    }
    out
}

/// Whether a recording output is a file rather than a stream.
pub(super) fn is_file(output: &str) -> bool {
    crate::media::is_recording(output)
}

/// What the CLI says as a recording or stream starts.
pub(super) fn forward_banner(output: &str, shown: &str, duration: Option<&str>) -> String {
    let verb = if is_file(output) {
        "Recording"
    } else {
        "Streaming"
    };
    match duration {
        Some(duration) => format!("{verb} {duration} to {shown} (Ctrl-C stops early)…"),
        None => format!("{verb} to {shown}. Press Ctrl-C to stop."),
    }
}

/// `Recorded 2.0 s · 60 frames written (0 dropped, 0 late) · 457 KiB →
/// clip.mp4`, and warnings about lost frames, for a finished recording.
pub(super) fn forward_summary(output: &str, value: &Value, now_ms: u64) -> (String, Vec<String>) {
    let stopped = &value["stopped"];
    let stats = &stopped["stats"];
    let recording = &stats["recording"];
    let since = value["since_ms"].as_u64().unwrap_or(now_ms);
    let length_ms = match (
        recording["started_ms"].as_u64(),
        recording["ended_ms"].as_u64(),
    ) {
        (Some(start), Some(end)) if end >= start => end - start,
        _ if stopped["by"] == "duration" => value["until_ms"]
            .as_u64()
            .unwrap_or(now_ms)
            .saturating_sub(since),
        _ => now_ms.saturating_sub(since),
    };
    let file = is_file(output);
    let mut out = format!(
        "{} {:.1} s",
        if file { "Recorded" } else { "Streamed" },
        length_ms as f64 / 1000.0
    );
    let mut warnings = Vec::new();
    if let Some(written) = stats["written"].as_u64() {
        let dropped = stats["dropped"].as_u64().unwrap_or(0);
        let late = stats["late"].as_u64().unwrap_or(0);
        out.push_str(&format!(
            " · {written} frames {} ({dropped} dropped, {late} late)",
            if file { "written" } else { "sent" }
        ));
        if dropped > 0 || late > 0 {
            warnings.push(format!(
                "{dropped} frame(s) were dropped ({late} because the encoder was still busy); lower --fps or the resolution, or try another --encoder"
            ));
        }
        if stats["stalled"] == true {
            warnings.push("the camera sent no new frame for 2 s or more".into());
        }
    }
    if let Some(error) = stats["error"].as_str() {
        warnings.push(error.to_owned());
    }
    if let Some(bytes) = recording["bytes"].as_u64() {
        out.push_str(&format!(" · {}", human_bytes(bytes)));
    }
    let target = recording["path"]
        .as_str()
        .or_else(|| value["path"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            crate::media::redact_url(value["forwarding"].as_str().unwrap_or(output))
        });
    out.push_str(&format!(" → {target}\n"));
    (out, warnings)
}

/// A persistent session's reply to an untimed recording, which runs on.
pub(super) fn forward_started(
    output: &str,
    value: &Value,
    session: &str,
    camera: Option<&str>,
) -> String {
    let shown = value["path"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| {
            crate::media::redact_url(value["forwarding"].as_str().unwrap_or(output))
        });
    let verb = if is_file(output) {
        "Recording"
    } else {
        "Streaming"
    };
    let camera = camera
        .map(|camera| format!(" --camera {}", crate::hint::shell_quote(camera)))
        .unwrap_or_default();
    format!(
        "{verb} to {shown} in session {session}. Stop it with: capturefab --session {}{camera} stop-forward\n",
        crate::hint::shell_quote(session)
    )
}

/// The `-o` that fixes a taken output name: `x-{seq}.png` for `x.png`.
pub(super) fn numbered_name(output: &str) -> String {
    let split = output
        .rfind(std::path::is_separator)
        .map_or(0, |separator| separator + 1);
    match output[split..].rfind('.').filter(|dot| *dot > 0) {
        Some(dot) => format!(
            "{}-{{seq}}{}",
            &output[..split + dot],
            &output[split + dot..]
        ),
        None => format!("{output}-{{seq}}"),
    }
}

/// How to save when the output name is taken.
pub(super) fn exists_hint(output: &str) -> String {
    format!(
        "add {{seq}} to the name (-o '{}') or pass --on-exists number",
        numbered_name(output)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame(file: &str) -> Value {
        json!({"width":640,"height":480,"pixel_format":0x0108_0001u32,"pixel_format_name":"Mono8","file":file,"file_bytes":2048})
    }

    #[test]
    fn captures_summarize_shape_size_and_folder() {
        let one = json!({"files":["out/x.png"],"frames":[frame("out/x.png")],"count":1});
        assert_eq!(
            capture_summary(&one),
            "Saved out/x.png · 640×480 Mono8 · 2 KiB\n"
        );
        let files = (1..=3)
            .map(|i| format!("out/frame-00000{i}.png"))
            .collect::<Vec<_>>();
        let three = json!({"files":files,"frames":files.iter().map(|f| frame(f)).collect::<Vec<_>>(),"destination":"bench","context":{"line":"L1"}});
        let text = capture_summary(&three);
        let sep = std::path::MAIN_SEPARATOR;
        assert!(
            text.starts_with(&format!(
                "Saved 3 PNG frames · 640×480 Mono8 · 6 KiB → out{sep}\n  frame-000001.png\n"
            )),
            "{text}"
        );
        assert!(
            text.contains("Destination: bench\nTags: line=L1\n"),
            "{text}"
        );
        let many = (1..=25)
            .map(|i| format!("out/f-{i:02}.jpg"))
            .collect::<Vec<_>>();
        let text = capture_summary(&json!({"files":many,"frames":[]}));
        assert!(text.starts_with("Saved 25 JPEG frames → out"), "{text}");
        assert!(
            text.contains("  f-03.jpg\n  … 21 more\n  f-25.jpg\n"),
            "{text}"
        );
        assert_eq!(text.lines().count(), 6);
    }

    #[test]
    fn intervals_and_relative_times_read_as_typed() {
        assert_eq!(interval(1000), "1s");
        assert_eq!(interval(500), "500ms");
        assert_eq!(interval(120_000), "2m");
        assert_eq!(interval(5_400_000), "1h30m");
        assert_eq!(relative(400), "now");
        assert_eq!(relative(4_200), "in 4s");
    }

    #[test]
    fn jobs_read_as_progress_and_a_table() {
        let job = json!({"id":1,"status":"running","output":"tl-{seq}.png","count":3,"captured":2,"skipped":0,"retries":0,"interval_ms":1000,"next_at_ms":11_000,"last_file":"/tmp/tl/tl-0002.png","error":null});
        assert_eq!(
            job_progress(&job, 10_000),
            "2/3 saved tl-0002.png · next in 1s"
        );
        let scheduled = job_scheduled(&job, Some("sim:0"), "bench");
        assert!(
            scheduled.starts_with("Scheduled job 1 on sim:0: 3 frames every 1s → tl-{seq}.png\n"),
            "{scheduled}"
        );
        assert!(
            scheduled.contains("capturefab --session bench jobs"),
            "{scheduled}"
        );
        assert!(scheduled.contains("--camera sim:0 cancel 1"), "{scheduled}");
        let table = jobs_table(std::slice::from_ref(&job), Some("bench"), 10_000);
        assert!(table.starts_with("ID    STATUS"), "{table}");
        assert!(table.contains("2/3") && table.contains("in 1s"), "{table}");
        assert!(jobs_table(&[], Some("bench"), 0).contains("No capture jobs in session bench"));
        let mut done = job.clone();
        done["status"] = json!("complete");
        done["captured"] = json!(3);
        done["skipped"] = json!(1);
        let finished = job_finished(&done);
        assert!(
            finished.starts_with("Captured 3 of 3 frames → /tmp/tl"),
            "{finished}"
        );
        assert!(
            finished.contains("(last: tl-0002.png) · 1 skipped"),
            "{finished}"
        );
    }

    #[test]
    fn storage_reads_as_a_budget() {
        let status = json!({"ledger":"/s/storage.json","policy":{"max_bytes":10u64<<30,"max_files":10000,"max_age_seconds":null,"on_full":"stop"},"over_limit":false,"allocated_bytes":1_677_722,"reserved_bytes":0,"files":6,"active_reservations":0,"externally_changed_files":0,"free_bytes":2u64<<30,"namespace_accounting":"jargon","retained_namespaces":[]});
        let text = storage_summary(&status);
        assert!(
            text.starts_with("Using 1.6 MiB of 10 GiB (6 of 10000 files) · when full: stop · 2 GiB free on disk\n"),
            "{text}"
        );
        assert!(!text.contains("jargon") && !text.contains("Over limit") && !text.contains('{'));
        let mut over = status.clone();
        over["over_limit"] = json!(true);
        over["externally_changed_files"] = json!(2);
        let text = storage_summary(&over);
        assert!(
            text.contains("Over limit") && text.contains("2 file(s) changed outside"),
            "{text}"
        );
    }

    #[test]
    fn recordings_summarize_their_final_stats() {
        let result = json!({"forwarding":"clip.mp4","forwarding_id":"a","since_ms":1000,"until_ms":3000,"path":"/abs/clip.mp4",
            "stopped":{"forwarding":null,"by":"duration","stats":{"accepted":60,"written":60,"dropped":0,"late":0,"stalled":false,
            "recording":{"path":"/abs/clip.mp4","bytes":467_968,"started_ms":1000,"ended_ms":3000,"encoder":"libx264"}}}});
        let (text, warnings) = forward_summary("clip.mp4", &result, 5000);
        assert_eq!(
            text,
            "Recorded 2.0 s · 60 frames written (0 dropped, 0 late) · 457 KiB → /abs/clip.mp4\n"
        );
        assert!(warnings.is_empty());
        let mut lossy = result.clone();
        lossy["stopped"]["stats"]["dropped"] = json!(3);
        assert_eq!(forward_summary("clip.mp4", &lossy, 5000).1.len(), 1);
        assert_eq!(
            forward_banner("clip.mp4", "clip.mp4", Some("2s")),
            "Recording 2s to clip.mp4 (Ctrl-C stops early)…"
        );
        assert!(forward_banner("rtsp://h/x", "rtsp://h/x", None).starts_with("Streaming to"));
    }

    #[test]
    fn taken_names_suggest_a_numbered_template() {
        assert_eq!(numbered_name("x.png"), "x-{seq}.png");
        assert_eq!(numbered_name("out/clip.mp4"), "out/clip-{seq}.mp4");
        assert_eq!(numbered_name("out.d/noext"), "out.d/noext-{seq}");
        assert!(exists_hint("x.png").contains("--on-exists number"));
    }

    #[test]
    fn info_header_names_the_camera_on_one_line() {
        let camera = json!({"id":"sim:0","vendor":"Capturefab","model":"Pattern camera","serial":"SIM0","transport":"simulator","address":null,"profile":{"kind":"area_scan","capabilities":["exposure","gain"]}});
        assert_eq!(
            info_header(&camera),
            "sim:0 · Capturefab Pattern camera · serial SIM0 · simulator · area_scan\nCapabilities: exposure, gain\n"
        );
    }
}
