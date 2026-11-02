//! Client-side resolution of local paths. A CLI path belongs to the caller,
//! even when the camera worker lives in a session started from another
//! directory, so relative outputs and file or replay sources are made
//! absolute before a command leaves this process.
use crate::session::{SessionCommand, validate_selector};
use anyhow::{Context, Result};
use std::path::{Component, Path, PathBuf};

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
    if let Ok(canonical) = canonical(Path::new(&*path))
        && let Some(canonical) = canonical.to_str()
    {
        *path = canonical.to_owned();
    }
    Ok(())
}

/// `path` canonical, in the form a person would type. Windows names its
/// canonical paths verbatim, as `\\?\C:\x` and `\\?\UNC\server\share\x`;
/// the plain `C:\x` and `\\server\share\x` replace them where the system
/// reads the plain form as the same path, which it does unless a name
/// ends in a dot or a space or is a device's, such as `NUL`.
pub(crate) fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)?;
    #[cfg(windows)]
    if let Some(text) = canonical.to_str() {
        let plain = match text.strip_prefix(r"\\?\UNC\") {
            Some(share) => Some(format!(r"\\{share}")),
            None => text.strip_prefix(r"\\?\").map(str::to_owned),
        };
        if let Some(plain) = plain.map(PathBuf::from)
            && plain.is_absolute()
            && std::path::absolute(&plain).is_ok_and(|read| read == plain)
        {
            return Ok(plain);
        }
    }
    Ok(canonical)
}

/// Resolve a command's relative local outputs against `base`; stream URLs
/// are untouched.
pub(crate) fn resolve_outputs(command: &mut SessionCommand, base: &Path) -> Result<()> {
    match command {
        SessionCommand::Schedule { output, .. } => resolve_path(output, base),
        SessionCommand::Forward { output, .. } => resolve_recording(output, base),
        _ => Ok(()),
    }
}

/// A recording's local file made absolute against `base`, as a plain path
/// or the path of `file:PATH`; `file://` URLs are absolute already, and
/// stream URLs are left as they are.
pub(crate) fn resolve_recording(output: &mut String, base: &Path) -> Result<()> {
    if !crate::media::is_recording(output) {
        return Ok(());
    }
    match output.split_once(':') {
        Some((scheme, path)) if scheme.eq_ignore_ascii_case("file") => {
            if !path.starts_with("//") {
                let mut path = path.to_owned();
                resolve_path(&mut path, base)?;
                *output = format!("file:{path}");
            }
            Ok(())
        }
        _ => resolve_path(output, base),
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

/// `path` without `.` and `..` components, naming what the system would
/// open: the names the caller gave, except where a `..` follows one.
fn normalize(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => up(&mut normal),
            component => normal.push(component),
        }
    }
    normal
}

/// Take `normal` to its parent, as the system resolves `..`: on Unix the
/// folder holding where a symbolic link leads, not the one holding the
/// link; Windows takes away the name before `..` as it is written.
fn up(normal: &mut PathBuf) {
    #[cfg(unix)]
    if std::fs::symlink_metadata(&*normal).is_ok_and(|meta| meta.file_type().is_symlink()) {
        match std::fs::canonicalize(&*normal) {
            Ok(target) => *normal = target,
            // A link to nothing: the system resolves the `..`, or fails,
            // once the path is used.
            Err(_) => {
                normal.push(Component::ParentDir);
                return;
            }
        }
    }
    match normal.components().next_back() {
        Some(Component::Normal(_)) => {
            normal.pop();
        }
        // Above the root is the root.
        Some(Component::RootDir | Component::Prefix(_)) => {}
        // After a `..` kept for the system, or at the start of a relative
        // path, one more.
        _ => normal.push(Component::ParentDir),
    }
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
        let canonical = canonical(&base.join("a.png")).unwrap();
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

    /// A canonical path is the system's, without Windows's verbatim prefix
    /// where the plain form names the same file.
    #[test]
    fn canonical_paths_are_plain_where_they_can_be() {
        let base =
            std::env::temp_dir().join(format!("capturefab-canonical-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let named = canonical(&base).unwrap();
        assert!(named.is_absolute(), "{named:?}");
        assert_eq!(
            std::fs::canonicalize(&named).unwrap(),
            std::fs::canonicalize(&base).unwrap()
        );
        #[cfg(windows)]
        {
            assert!(!named.to_str().unwrap().starts_with(r"\\?\"), "{named:?}");
            // The plain form of a name ending in a dot names the name
            // without it, so that path stays verbatim.
            let dotted = std::fs::canonicalize(&base).unwrap().join("dotted.");
            std::fs::create_dir(&dotted).unwrap();
            let named = canonical(&dotted).unwrap();
            assert!(named.to_str().unwrap().starts_with(r"\\?\"), "{named:?}");
            assert!(named.to_str().unwrap().ends_with("dotted."), "{named:?}");
        }
        std::fs::remove_dir_all(std::fs::canonicalize(&base).unwrap()).unwrap();
    }

    /// A `..` after a symbolic link leads to the parent of where the link
    /// leads, as the system resolves it, for outputs and sources alike;
    /// names before no `..` stay as the caller wrote them.
    #[cfg(unix)]
    #[test]
    fn a_parent_after_a_symbolic_link_is_the_parent_of_its_target() {
        let root = std::env::temp_dir().join(format!("capturefab-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b/sub/inner")).unwrap();
        std::os::unix::fs::symlink(root.join("b/sub"), root.join("a/link")).unwrap();
        std::os::unix::fs::symlink(root.join("a"), root.join("alias")).unwrap();
        std::os::unix::fs::symlink(root.join("gone"), root.join("a/dangling")).unwrap();
        let real = std::fs::canonicalize(&root).unwrap();
        let resolved = |path: &str, base: &Path| {
            let mut path = path.to_owned();
            resolve_path(&mut path, base).unwrap();
            path
        };
        let shown = |path: PathBuf| path.to_str().unwrap().to_owned();
        let base = root.join("a");
        assert_eq!(
            resolved("link/../shot.png", &base),
            shown(real.join("b/shot.png"))
        );
        assert_eq!(
            resolved("./link/./../../shot.png", &base),
            shown(real.join("shot.png"))
        );
        assert_eq!(
            resolved(&format!("{}/a/link/../frames/", root.display()), &base),
            format!("{}/", real.join("b/frames").display())
        );
        // A link to nothing keeps its `..` for the system to resolve.
        assert_eq!(
            resolved("dangling/../shot.png", &base),
            shown(base.join("dangling/../shot.png"))
        );
        // Without a link before it, `..` takes away the name before it, and
        // the caller's names stay, links among them.
        let alias = root.join("alias");
        assert_eq!(
            resolved("out/../shot.png", &alias),
            shown(alias.join("shot.png"))
        );
        assert_eq!(
            resolved("link/inner/../shot.png", &alias),
            shown(alias.join("link/shot.png"))
        );
        // A source through the same path is the file the system would open.
        std::fs::write(root.join("b/clip.mp4"), b"x").unwrap();
        std::fs::write(root.join("a/clip.mp4"), b"y").unwrap();
        let mut source = "file:link/../clip.mp4".to_owned();
        resolve_selector(&mut source, &base).unwrap();
        assert_eq!(
            source,
            format!("file:{}", real.join("b/clip.mp4").display())
        );
        let mut replay = "sim:replay:link/..".to_owned();
        resolve_selector(&mut replay, &base).unwrap();
        assert_eq!(replay, format!("sim:replay:{}", real.join("b").display()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recording_files_resolve_against_the_caller_as_paths_or_file_paths() {
        let base = std::env::temp_dir().join("capturefab-path-recording-tests");
        let resolved = |output: &str| {
            let mut output = output.to_owned();
            resolve_recording(&mut output, &base).unwrap();
            output
        };
        let clip = base.join("clip.mkv").display().to_string();
        assert_eq!(resolved("clip.mkv"), clip);
        assert_eq!(resolved("file:clip.mkv"), format!("file:{clip}"));
        assert_eq!(resolved("FILE:./out/../clip.mkv"), format!("file:{clip}"));
        for kept in [
            "file:///tmp/clip.mkv",
            "file://localhost/tmp/clip.mkv",
            "rtsp://localhost:8554/cam",
            "srt://localhost:8890?streamid=publish:cam",
        ] {
            assert_eq!(resolved(kept), kept);
        }
        #[cfg(unix)]
        assert_eq!(
            resolved("file:/data/./x/../clip.mkv"),
            "file:/data/clip.mkv"
        );
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
            br#"{"op":"schedule","output":"frames/shot.png","count":1,"timeout_ms":2000,"format":"png","first_at_ms":0,"interval_ms":0}"#,
        )
        .unwrap();
        resolve_outputs(&mut capture, &base).unwrap();
        let SessionCommand::Schedule { output, .. } = &capture else {
            unreachable!()
        };
        assert_eq!(
            output,
            &base.join("frames").join("shot.png").display().to_string()
        );
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
