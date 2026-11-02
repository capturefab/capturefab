//! `capturefab doctor` and `capturefab licenses`.
use super::{Global, output::Report};
use serde_json::json;

pub const DOCTOR_FIELDS: &[(&str, &str)] = &[
    (
        "checks",
        "[{name,status:ok|warn|fail,message,fix?}] — fix is a command or step that resolves a warning",
    ),
    ("version", "string — Capturefab version"),
    ("os", "string"),
    ("arch", "string"),
    (
        "features",
        "{usb,jpeg,videotoolbox,nvjpeg,vaapi,keychain,shell} — what this build includes",
    ),
    (
        "jpeg",
        "{selected:auto|cpu|videotoolbox|nvjpeg|vaapi,backends:{libjpeg_turbo:{compiled,simd},videotoolbox|nvjpeg|vaapi:{compiled,available,verified?,reason?,version?,driver?,device?,integrated?,upload?:in-place|dma|copy}}} — the JPEG encoder CAPTUREFAB_JPEG selects; verified: the engine encoded a test frame as accurately as libjpeg-turbo, checked once a boot (a failure again after five minutes) and kept in the cache; nvjpeg and vaapi (Linux) run in a helper process; upload: how frames reach the GPU (read where the camera wrote them, one DMA, or written into its memory)",
    ),
    (
        "folders",
        "{runtime,state,cache: {path,from,writable,fs_type,passed:[{path,from,why}],error}} — the folders in use (runtime: sessions; state: presets, sign-ins; cache: FFmpeg), path absent when there is none; which candidate each uses (from: a variable, XDG_*, login, home, run, temp, runtime) and the fallbacks passed over",
    ),
    (
        "inspect_dir",
        "string, absent when there is none — the private folder inspect writes previews to without --preview PATH",
    ),
    (
        "warm_sessions",
        "bool — camera commands can keep their camera open in the auto session",
    ),
    (
        "read_only_root",
        "bool, absent off Linux or when it cannot tell — the root file system is mounted read-only",
    ),
    (
        "host",
        "{cpus,threads,cpu_quota,memory:{total_bytes,available_bytes,cgroup_limit_bytes,cgroup_used_bytes,budget_bytes}} — what cgroups leave this process",
    ),
    (
        "ffmpeg",
        "{source:CAPTUREFAB_FFMPEG|bundled|next_to_executable|path,path,version,bundled_error?} or {error} — FFmpeg is looked for in that order; bundled_error says why a bundled copy could not be extracted",
    ),
    (
        "hwdec",
        "{selected:auto|none|METHOD,methods:{METHOD:{built,works,reason?}}} — hardware decoding CAPTUREFAB_HWDEC selects, and each method of this platform: in this FFmpeg (built) and proved to decode here (works); only with FFmpeg",
    ),
];
pub const LICENSES_FIELDS: &[(&str, &str)] = &[
    ("capturefab", "string — Capturefab's MIT license"),
    (
        "ffmpeg",
        "string — the bundled FFmpeg's GPL notice and license texts, or a note that this build has none",
    ),
];

pub fn doctor(_global: &Global) -> Report {
    let report = crate::doctor::report(super::session::can_start());
    let mut text = Vec::new();
    let _ = crate::doctor::print_checklist(&mut text, &report);
    Report::json(report).text(String::from_utf8_lossy(&text))
}

const LICENSE: &str = include_str!("../../LICENSE");

pub fn licenses() -> Report {
    let ffmpeg = crate::media::license_notice();
    let text = format!("Capturefab\n\n{LICENSE}\n\nFFmpeg\n\n{ffmpeg}\n");
    Report::json(json!({"capturefab": LICENSE, "ffmpeg": ffmpeg})).text(text)
}
