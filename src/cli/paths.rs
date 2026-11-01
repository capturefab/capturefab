//! Client-side resolution of local paths. A CLI path belongs to the caller,
//! even when the camera worker lives in a session started from another
//! directory, so relative outputs and file or replay sources are made
//! absolute before a command leaves this process.
use crate::session::{SessionCommand, validate_selector};
use anyhow::{Context, Result};
use std::path::Path;

/// Local sources follow the same caller/file base as outputs, and an
/// existing file or folder is named by its canonical path, so `a.png`,
/// `./a.png` and `../here/a.png` are one camera. Network/device locators and
/// discovered camera IDs are never rewritten.
pub(crate) fn resolve_selector(selector: &mut String, base: &Path) -> Result<()> {
    // Preserve malformed-input errors: an empty local path must never become
    // the caller's valid current directory during absolute path resolution.
    validate_selector(selector)?;
    if let Some(path) = selector.strip_prefix("sim:replay:") {
        let mut path = path.to_owned();
        resolve_source(&mut path, base)?;
        *selector = format!("sim:replay:{path}");
    } else if selector
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:"))
    {
        let path = &selector[5..];
        if !path.starts_with("//") {
            let mut path = path.to_owned();
            resolve_source(&mut path, base)?;
            *selector = format!("file:{path}");
        }
    } else if selector.parse::<std::net::Ipv4Addr>().is_err()
        && crate::media::is_recording(selector)
        && base.join(&*selector).is_file()
    {
        resolve_source(selector, base)?;
    }
    Ok(())
}

/// A local source made absolute, and canonical when it exists.
fn resolve_source(path: &mut String, base: &Path) -> Result<()> {
    resolve_path(path, base)?;
    if let Ok(canonical) = std::fs::canonicalize(&*path)
        && let Some(canonical) = canonical.to_str()
    {
        // Windows canonical paths are verbatim (\\?\C:\...); keep the
        // plain form a person would type.
        *path = canonical
            .strip_prefix(r"\\?\")
            .filter(|plain| Path::new(plain).is_absolute())
            .unwrap_or(canonical)
            .to_owned();
    }
    Ok(())
}

/// Resolve a command's relative local outputs against `base`; stream URLs
/// are untouched.
pub(crate) fn resolve_outputs(command: &mut SessionCommand, base: &Path) -> Result<()> {
    match command {
        SessionCommand::Capture { output, .. } | SessionCommand::Schedule { output, .. } => {
            resolve_path(output, base)
        }
        SessionCommand::Forward { output, .. } if crate::media::is_recording(output) => {
            // `file:relative.mkv` is a local path, while `file:///...` and
            // `file://localhost/...` are already absolute file URLs.
            if let Some((scheme, path)) = output.split_once(':')
                && scheme.eq_ignore_ascii_case("file")
            {
                if !path.starts_with("//") && Path::new(path).is_relative() {
                    let mut path = path.to_owned();
                    resolve_path(&mut path, base)?;
                    *output = format!("file:{path}");
                }
                Ok(())
            } else {
                resolve_path(output, base)
            }
        }
        _ => Ok(()),
    }
}

pub(crate) fn resolve_path(output: &mut String, base: &Path) -> Result<()> {
    let path = Path::new(output);
    #[cfg(windows)]
    anyhow::ensure!(
        !(path
            .components()
            .next()
            .is_some_and(|component| matches!(component, std::path::Component::Prefix(_)))
            && !path.is_absolute()),
        "drive-relative paths are ambiguous; use an absolute path or a relative path without a drive prefix"
    );
    let joined = if path.is_relative() {
        base.join(path)
    } else {
        path.to_path_buf()
    };
    let mut normal = normalize(&joined)
        .to_str()
        .context("output path must be valid UTF-8")?
        .to_owned();
    // A trailing separator names a folder to fill.
    if output.ends_with(std::path::is_separator) && !normal.ends_with(std::path::is_separator) {
        normal.push(std::path::MAIN_SEPARATOR);
    }
    *output = normal;
    Ok(())
}

/// `path` without `.` components, each `..` taking away the name before it:
/// what the caller's words name, before any symbolic link is followed.
fn normalize(path: &Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut normal = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir
                if matches!(normal.components().next_back(), Some(Component::Normal(_))) =>
            {
                normal.pop();
            }
            // Above the root is the root.
            Component::ParentDir if normal.has_root() => {}
            component => normal.push(component),
        }
    }
    normal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_sources_without_rewriting_network_or_device_locators() {
        let base = std::env::temp_dir().join("capturefab-path-source-tests");
        let mut replay = "sim:replay:frames".to_owned();
        resolve_selector(&mut replay, &base).unwrap();
        assert_eq!(
            replay,
            format!("sim:replay:{}", base.join("frames").display())
        );
        let mut file = "file:clip.mp4".to_owned();
        resolve_selector(&mut file, &base).unwrap();
        assert_eq!(file, format!("file:{}", base.join("clip.mp4").display()));
        for source in [
            "192.168.1.10",
            "rtsp://camera/live",
            "sim:0",
            "avfoundation:0",
            "v4l2:/dev/video0",
            "dshow:video=Camera Name",
            "file:///tmp/clip.mp4",
        ] {
            let mut resolved = source.to_owned();
            resolve_selector(&mut resolved, &base).unwrap();
            assert_eq!(resolved, source);
        }
    }

    #[test]
    fn one_file_has_one_name_however_it_is_reached() {
        let base = std::env::temp_dir().join(format!("capturefab-paths-{}", std::process::id()));
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("a.png"), b"x").unwrap();
        let named = |selector: &str, base: &Path| {
            let mut selector = selector.to_owned();
            resolve_selector(&mut selector, base).unwrap();
            selector
        };
        let canonical = std::fs::canonicalize(base.join("a.png")).unwrap();
        for (selector, from) in [
            ("a.png", base.clone()),
            ("./a.png", base.clone()),
            ("../a.png", base.join("sub")),
        ] {
            assert_eq!(
                named(selector, &from),
                canonical.to_str().unwrap(),
                "{selector}"
            );
        }
        assert_eq!(
            named("sim:replay:./sub/../a.png", &base),
            format!("sim:replay:{}", canonical.display())
        );
        std::fs::remove_dir_all(base).unwrap();
        #[cfg(unix)]
        {
            let mut output = "./out/../shots/".to_owned();
            resolve_path(&mut output, Path::new("/data/run")).unwrap();
            assert_eq!(output, "/data/run/shots/");
            let mut output = "/data/./x/../../../y.png".to_owned();
            resolve_path(&mut output, Path::new("/elsewhere")).unwrap();
            assert_eq!(output, "/y.png");
        }
    }

    #[test]
    fn invalid_source_selectors_are_rejected_before_path_resolution() {
        let base = std::env::temp_dir();
        for source in ["sim:replay:", "file:", "FiLe:"] {
            let mut selector = source.to_owned();
            assert!(resolve_selector(&mut selector, &base).is_err(), "{source}");
            assert_eq!(
                selector, source,
                "invalid selectors retain their original meaning"
            );
        }
    }
    #[test]
    fn relative_outputs_resolve_against_the_caller_and_urls_do_not() {
        let base = std::env::temp_dir().join("capturefab-path-output-tests");
        let mut capture = SessionCommand::from_json_slice(
            br#"{"op":"capture","output":"frames/shot.png","count":1,"timeout_ms":2000,"format":"png"}"#,
        )
        .unwrap();
        resolve_outputs(&mut capture, &base).unwrap();
        let SessionCommand::Capture { output, .. } = &capture else {
            unreachable!()
        };
        assert_eq!(output, &base.join("frames/shot.png").display().to_string());
        let mut recording =
            SessionCommand::from_json_slice(br#"{"op":"forward","output":"file:clip.mkv","codec":"h264","encoder":"auto","fps":30.0,"bitrate":"4M"}"#)
                .unwrap();
        resolve_outputs(&mut recording, &base).unwrap();
        let SessionCommand::Forward { output, .. } = &recording else {
            unreachable!()
        };
        assert_eq!(output, &format!("file:{}", base.join("clip.mkv").display()));
        let mut stream = SessionCommand::from_json_slice(
            br#"{"op":"forward","output":"rtsp://localhost:8554/cam","codec":"h264","encoder":"auto","fps":30.0,"bitrate":"4M"}"#,
        )
        .unwrap();
        resolve_outputs(&mut stream, &base).unwrap();
        let SessionCommand::Forward { output, .. } = &stream else {
            unreachable!()
        };
        assert_eq!(output, "rtsp://localhost:8554/cam");
    }
}
