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
    ("features", "{usb,jpeg,keychain} — what this build includes"),
    (
        "dirs",
        "{runtime,state,cache,inspect} — runtime: sessions; state: presets, sign-ins; cache: FFmpeg; inspect: previews",
    ),
    (
        "ffmpeg",
        "{source:bundled|CAPTUREFAB_FFMPEG,path,version} or {error}",
    ),
];
pub const LICENSES_FIELDS: &[(&str, &str)] = &[
    ("capturefab", "string — Capturefab's MIT license"),
    (
        "ffmpeg",
        "string — the bundled FFmpeg's GPL notice and license texts",
    ),
];

pub fn doctor(_global: &Global) -> Report {
    let report = crate::doctor::report();
    let mut text = Vec::new();
    let _ = crate::doctor::print_checklist(&mut text, &report);
    Report::json(report).text(String::from_utf8_lossy(&text))
}

const LICENSE: &str = include_str!("../../LICENSE");

pub fn licenses() -> Report {
    let ffmpeg = crate::media::license_notice();
    let text = format!("Capturefab\n\n{LICENSE}\n\nBundled FFmpeg\n\n{ffmpeg}\n");
    Report::json(json!({"capturefab": LICENSE, "ffmpeg": ffmpeg})).text(text)
}
