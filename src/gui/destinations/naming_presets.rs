//! The capture panel's "File names" choice: a few output templates that
//! cover most uses, recognised in and rebuilt from an output path, so the
//! raw template only shows when it is chosen or hand-edited.
//!
//! Only exact suffixes are recognised. Anything else, including a folder
//! part with a placeholder in it, is `Custom` and never rewritten.
use std::fmt;
use std::path::{Path, PathBuf};

/// How captured files are named below their folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Naming {
    /// `{date}/{camera}-{datetime}-{seq}.ext`, the default.
    DatedCameraTime,
    /// `{camera}-{datetime}-{seq}.ext`.
    CameraTime,
    /// `capture-{seq}.ext`.
    Numbered,
    /// Any other template, typed by hand.
    Custom,
}

impl Naming {
    pub const ALL: [Naming; 4] = [
        Naming::DatedCameraTime,
        Naming::CameraTime,
        Naming::Numbered,
        Naming::Custom,
    ];
    /// The presets `join` can build, most specific suffix first.
    pub const PRESETS: [Naming; 3] = [
        Naming::DatedCameraTime,
        Naming::CameraTime,
        Naming::Numbered,
    ];

    /// The template's parts below the folder, as path components.
    fn parts(self, ext: &str) -> Option<Vec<String>> {
        match self {
            Naming::DatedCameraTime => Some(vec![
                "{date}".into(),
                format!("{{camera}}-{{datetime}}-{{seq}}.{ext}"),
            ]),
            Naming::CameraTime => Some(vec![format!("{{camera}}-{{datetime}}-{{seq}}.{ext}")]),
            Naming::Numbered => Some(vec![format!("capture-{{seq}}.{ext}")]),
            Naming::Custom => None,
        }
    }
}

impl fmt::Display for Naming {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // {seq} grows as needed, so promise no particular width.
        f.write_str(match self {
            Naming::DatedCameraTime => "By date, camera and time",
            Naming::CameraTime => "Camera and time",
            Naming::Numbered => "Numbered (capture-1, capture-2…)",
            Naming::Custom => "Custom pattern…",
        })
    }
}

/// `output`'s folder and naming, for files ending in `.ext`. Separators are
/// the platform's (both '/' and '\\' on Windows). A `Custom` output is
/// returned whole as its "folder", so `join` gives it back unchanged.
pub fn split(output: &str, ext: &str) -> (Option<PathBuf>, Naming) {
    split_with(output, ext, std::path::is_separator)
}

/// `split` with `is_separator` deciding which characters separate folders.
pub(super) fn split_with(
    output: &str,
    ext: &str,
    is_separator: fn(char) -> bool,
) -> (Option<PathBuf>, Naming) {
    let custom = || {
        (
            (!output.is_empty()).then(|| PathBuf::from(output)),
            Naming::Custom,
        )
    };
    if ext.is_empty() || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        return custom();
    }
    for naming in Naming::PRESETS {
        let Some(parts) = naming.parts(ext) else {
            continue;
        };
        let mut rest = output;
        let mut matched = true;
        // Match the components from the end, each after a separator.
        for (index, part) in parts.iter().enumerate().rev() {
            let Some(head) = rest.strip_suffix(part.as_str()) else {
                matched = false;
                break;
            };
            if index == 0 {
                rest = head;
                break;
            }
            match head.chars().next_back() {
                Some(c) if is_separator(c) => rest = &head[..head.len() - c.len_utf8()],
                _ => {
                    matched = false;
                    break;
                }
            }
        }
        if !matched {
            continue;
        }
        // What is left is empty (no folder) or ends with a separator.
        let folder = if rest.is_empty() {
            None
        } else {
            let Some(c) = rest.chars().next_back().filter(|c| is_separator(*c)) else {
                continue;
            };
            let trimmed = &rest[..rest.len() - c.len_utf8()];
            // A root folder keeps its separator.
            Some(if trimmed.is_empty() || trimmed.ends_with(':') {
                rest
            } else {
                trimmed
            })
        };
        if folder.is_some_and(|f| f.contains(['{', '}'])) {
            return custom();
        }
        return (folder.map(PathBuf::from), naming);
    }
    custom()
}

/// The output for `naming` in `folder`, for files ending in `.ext`. For
/// `Custom`, `folder` is the whole output, given back unchanged.
pub fn join(folder: Option<&Path>, naming: Naming, ext: &str) -> String {
    let Some(parts) = naming.parts(ext) else {
        return folder
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
    };
    let mut path = folder.map(Path::to_path_buf).unwrap_or_default();
    for part in parts {
        path = path.join(part);
    }
    path.to_string_lossy().into_owned()
}

/// The extension of `output`'s file name, if it is a plain one such as
/// `png`, which is what the presets keep.
pub fn extension(output: &str) -> Option<&str> {
    let name = output.rsplit(std::path::is_separator).next()?;
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphanumeric()))
        .then_some(ext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::naming::{OnExists, validate_for_count, validate_template};

    fn windows(c: char) -> bool {
        c == '/' || c == '\\'
    }

    #[test]
    fn presets_round_trip_for_every_format_and_separator() {
        for ext in ["png", "jpeg", "raw"] {
            for naming in Naming::PRESETS {
                for folder in [
                    None,
                    Some(PathBuf::from("/Users/ada/Capturefab")),
                    Some(PathBuf::from("captures")),
                    Some(PathBuf::from("/")),
                ] {
                    let output = join(folder.as_deref(), naming, ext);
                    assert_eq!(split(&output, ext), (folder.clone(), naming), "{output}");
                    assert_eq!(join(folder.as_deref(), naming, ext), output);
                }
                // Windows paths, with either separator.
                let suffix = naming.parts(ext).unwrap();
                for sep in ["\\", "/"] {
                    let output = format!(r"C:\Users\ada\Capturefab{sep}{}", suffix.join(sep));
                    assert_eq!(
                        split_with(&output, ext, windows),
                        (Some(PathBuf::from(r"C:\Users\ada\Capturefab")), naming),
                        "{output}"
                    );
                    let root = format!(r"C:\{}", suffix.join(sep));
                    assert_eq!(
                        split_with(&root, ext, windows),
                        (Some(PathBuf::from(r"C:\")), naming),
                        "{root}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_default_output_is_the_recommended_preset_in_the_capture_folder() {
        let output = crate::gui::prefs::default_capture_output();
        assert_eq!(
            split(&output, "png"),
            (
                Some(crate::gui::prefs::capture_root()),
                Naming::DatedCameraTime
            )
        );
        assert_eq!(extension(&output), Some("png"));
    }

    #[test]
    fn anything_else_is_custom_and_never_rewritten() {
        for output in [
            "capture.png",
            "/tmp/shots/{frame}.png",
            "/tmp/{camera}/capture-{seq}.png",
            "/tmp/{date}/{date}/{camera}-{datetime}-{seq}.png",
            "/tmp/shots/capture-{seq}-{frame}.png",
            "/tmp/shots/{frame}-capture-{seq}.png",
            "/tmp/shots/mycapture-{seq}.png",
            "/tmp/shots/capture-{seq}.jpeg",
            "/tmp/shots/capture-{seq}.png/",
            "",
        ] {
            let (folder, naming) = split(output, "png");
            assert_eq!(naming, Naming::Custom, "{output}");
            assert_eq!(join(folder.as_deref(), naming, "png"), output);
        }
        // A file named with a backslash is a name on Unix, not a folder.
        if cfg!(unix) {
            assert_eq!(split(r"shots\capture-{seq}.png", "png").1, Naming::Custom);
        }
        assert_eq!(split("capture-{seq}.png", "").1, Naming::Custom);
    }

    #[test]
    fn every_preset_is_a_valid_multi_frame_template() {
        for ext in ["png", "jpeg", "raw"] {
            for naming in Naming::PRESETS {
                let output = join(Some(Path::new("/tmp/captures")), naming, ext);
                validate_template(&output).unwrap();
                validate_for_count(&output, 10, OnExists::Fail).unwrap();
                crate::engine::validate_capture_output(&output).unwrap();
            }
        }
    }

    #[test]
    fn extensions_are_plain_file_name_suffixes() {
        assert_eq!(extension("/tmp/a/capture-{seq}.jpeg"), Some("jpeg"));
        assert_eq!(extension("/tmp/a.b/capture"), None);
        assert_eq!(extension("/tmp/.png"), None);
        assert_eq!(extension("shot.p-g"), None);
    }

    #[test]
    fn labels_promise_no_sequence_width() {
        assert!(Naming::ALL.iter().all(|n| !n.to_string().contains("0001")));
    }
}
