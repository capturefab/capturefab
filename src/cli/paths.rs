//! Client-side resolution of local paths. A CLI path belongs to the caller,
//! even when the camera worker lives in a session started from another
//! directory, so relative outputs and file or replay sources are made
//! absolute before a command leaves this process.
use crate::session::{SessionCommand, validate_selector};
use anyhow::{Context, Result};
use std::path::Path;

/// Local sources follow the same caller/file base as outputs. Network/device
/// locators and discovered camera IDs are never rewritten.
pub(crate) fn resolve_selector(selector: &mut String, base: &Path) -> Result<()> {
    // Preserve malformed-input errors: an empty local path must never become
    // the caller's valid current directory during absolute path resolution.
    validate_selector(selector)?;
    if let Some(path) = selector.strip_prefix("sim:replay:") {
        let mut path = path.to_owned();
        resolve_path(&mut path, base)?;
        *selector = format!("sim:replay:{path}");
    } else if selector
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:"))
    {
        let path = &selector[5..];
        if !path.starts_with("//") && Path::new(path).is_relative() {
            let mut path = path.to_owned();
            resolve_path(&mut path, base)?;
            *selector = format!("file:{path}");
        }
    } else if selector.parse::<std::net::Ipv4Addr>().is_err()
        && crate::media::is_recording(selector)
        && base.join(&*selector).is_file()
    {
        resolve_path(selector, base)?;
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
    if path.is_relative() {
        *output = base
            .join(path)
            .to_str()
            .context("output path must be valid UTF-8")?
            .to_owned();
    }
    Ok(())
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
