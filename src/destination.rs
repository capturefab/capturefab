//! Named capture folders on local, removable and mounted network disks.
//! Captures, schedules and recordings resolve names through this private list.
//! Every output uses the shared storage ledger for budgets and retention.
use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Destination {
    pub name: String,
    #[serde(flatten)]
    pub target: Target,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    Folder { path: PathBuf },
}

impl Target {
    pub fn kind(&self) -> &'static str {
        "folder"
    }
}

impl Destination {
    pub fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        let Target::Folder { path } = &self.target;
        ensure!(
            path.to_str().is_some(),
            "a folder destination needs a path with valid UTF-8; rename the folder or choose another destination"
        );
        ensure!(
            path.is_absolute()
                && !path
                    .as_os_str()
                    .as_encoded_bytes()
                    .iter()
                    .any(|b| b.is_ascii_control()),
            "a folder destination needs an absolute path without control characters"
        );
        Ok(())
    }
    pub fn local_root(&self) -> PathBuf {
        let Target::Folder { path } = &self.target;
        path.clone()
    }
    pub fn describe(&self) -> String {
        self.local_root().display().to_string()
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            && !name.starts_with('.'),
        "destination names are 1-64 letters, digits, '.', '-' and '_' and cannot start with '.'"
    );
    Ok(())
}

#[derive(Serialize, Deserialize, Default)]
struct File {
    version: u32,
    #[serde(deserialize_with = "read_folders")]
    destinations: Vec<Destination>,
    /// The destination the GUI last saved to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preferred: Option<String>,
}

/// Old saved remote destinations are ignored without touching their files.
/// Invalid folder records still fail closed rather than silently losing them.
fn read_folders<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<Destination>, D::Error> {
    let entries = Vec::<serde_json::Value>::deserialize(deserializer)?;
    if entries.len() > 256 {
        return Err(serde::de::Error::custom(
            "too many saved destinations (maximum 256)",
        ));
    }
    entries
        .into_iter()
        .filter_map(|entry| {
            if matches!(
                entry.get("kind").and_then(serde_json::Value::as_str),
                Some("s3" | "ftp" | "sftp")
            ) {
                None
            } else {
                Some(serde_json::from_value(entry).map_err(serde::de::Error::custom))
            }
        })
        .collect()
}

/// The directory holding the saved list and its lock.
fn dir() -> PathBuf {
    crate::ipc::state_dir()
}

/// The file holding the saved list.
pub(crate) fn path() -> PathBuf {
    dir().join("destinations.json")
}

/// All saved destinations.
pub fn list() -> Result<Vec<Destination>> {
    Ok(read()?.destinations)
}

fn read() -> Result<File> {
    let path = path();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(File {
                version: 1,
                ..File::default()
            });
        }
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    ensure!(bytes.len() <= 1 << 20, "destination list is too large");
    let file: File = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid destination list {}", path.display()))?;
    ensure!(file.version == 1, "unsupported destination list version");
    ensure!(
        file.destinations.len() <= 256,
        "too many saved destinations (maximum 256)"
    );
    let mut names = std::collections::HashSet::new();
    for destination in &file.destinations {
        destination
            .validate()
            .with_context(|| format!("invalid saved destination {}", destination.name))?;
        ensure!(
            names.insert(&destination.name),
            "duplicate saved destination name {}",
            destination.name
        );
    }
    Ok(file)
}

/// The destination the GUI last saved to, if it still exists.
pub fn preferred() -> Option<String> {
    let file = read().ok()?;
    file.preferred
        .filter(|name| file.destinations.iter().any(|d| &d.name == name))
}

pub fn set_preferred(name: Option<&str>) -> Result<()> {
    if let Some(name) = name {
        validate_name(name)?;
    }
    locked(|file| {
        ensure!(
            name.is_none_or(|name| file.destinations.iter().any(|d| d.name == name)),
            "preferred destination no longer exists"
        );
        file.preferred = name.map(str::to_string);
        Ok(())
    })
}

pub fn get(name: &str) -> Result<Destination> {
    validate_name(name)?;
    list()?.into_iter().find(|d| d.name == name).ok_or_else(|| {
        anyhow!("destination {name} not found; list them with `capturefab destination list`")
    })
}

fn write(file: &File) -> Result<()> {
    let dir = dir();
    crate::ipc::ensure_private_dir(&dir)?;
    // Write a sibling and rename it into place, so readers never see a
    // partial list.
    let temp = dir.join(format!("destinations.json.{}.tmp", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(&temp)?;
    output.write_all(&serde_json::to_vec_pretty(file)?)?;
    output.sync_all()?;
    fs::rename(&temp, path())?;
    Ok(())
}

/// Serialize read-modify-write updates across processes.
fn locked<T>(update: impl FnOnce(&mut File) -> Result<T>) -> Result<T> {
    locked_with_rollback(update, |_| Ok(()))
}

fn locked_with_rollback<T>(
    update: impl FnOnce(&mut File) -> Result<T>,
    rollback: impl FnOnce(&T) -> Result<()>,
) -> Result<T> {
    use fs2::FileExt;
    let dir = dir();
    crate::ipc::ensure_private_dir(&dir)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("destinations.lock"))?;
    lock.lock_exclusive()?;
    let mut file = read()?;
    file.version = 1;
    let result = update(&mut file)?;
    if let Err(error) = write(&file) {
        rollback(&result)
            .context("restore accompanying state after the destination could not be saved")?;
        return Err(error);
    }
    Ok(result)
}

/// Replace exactly the destination the caller observed. Renames are one
/// transaction, and a concurrent editor cannot silently lose its changes.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub(crate) fn replace(before: Option<&Destination>, after: Option<&Destination>) -> Result<()> {
    replace_with(before, after, || Ok(()), |_| Ok(()))
}

/// Apply accompanying state under the destination lock and restore it if
/// the settings cannot be written.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub(crate) fn replace_with<T>(
    before: Option<&Destination>,
    after: Option<&Destination>,
    prepare: impl FnOnce() -> Result<T>,
    rollback: impl FnOnce(&T) -> Result<()>,
) -> Result<T> {
    replace_with_selection(before, after, None, prepare, rollback)
}

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub(crate) fn replace_with_selection<T>(
    before: Option<&Destination>,
    after: Option<&Destination>,
    selection: Option<(Option<&str>, bool)>,
    prepare: impl FnOnce() -> Result<T>,
    rollback: impl FnOnce(&T) -> Result<()>,
) -> Result<T> {
    if let Some(after) = after {
        after.validate()?;
    }
    locked_with_rollback(
        |file| {
            replace_in_file(file, before, after)?;
            if let Some((selected, remember)) = selection {
                if let Some(name) = selected {
                    ensure!(
                        file.destinations.iter().any(|d| d.name == name),
                        "destination {name} no longer exists; cannot restore its selection"
                    );
                }
                if remember {
                    file.preferred = selected.map(str::to_owned);
                }
            }
            prepare()
        },
        rollback,
    )
}

fn replace_in_file(
    file: &mut File,
    before: Option<&Destination>,
    after: Option<&Destination>,
) -> Result<()> {
    for name in before.into_iter().chain(after).map(|d| &d.name) {
        let expected = before.filter(|d| &d.name == name);
        let current = file.destinations.iter().find(|d| &d.name == name);
        ensure!(
            current == expected,
            "destination {name} changed outside this edit; reload it before trying again"
        );
    }
    let old_name = before.map(|d| &d.name);
    if before.is_some() {
        file.destinations.retain(|d| Some(&d.name) != old_name);
    }
    if let Some(after) = after {
        ensure!(file.destinations.len() < 256, "too many destinations");
        file.destinations.push(after.clone());
        file.destinations.sort_by(|a, b| a.name.cmp(&b.name));
    }
    if file.preferred.as_ref() == old_name && old_name.is_some() {
        file.preferred = after.map(|d| d.name.clone());
    }
    Ok(())
}

/// Add a destination, or replace the one with the same name.
pub fn save(destination: Destination) -> Result<()> {
    destination.validate()?;
    locked(|file| {
        let all = &mut file.destinations;
        match all.iter_mut().find(|d| d.name == destination.name) {
            Some(existing) => *existing = destination,
            None => {
                ensure!(all.len() < 256, "too many destinations");
                all.push(destination);
            }
        }
        all.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(())
    })
}

/// Add a new folder destination without replacing an existing saved name.
/// The return value is retained for callers that previously reported side effects.
pub fn create(destination: &Destination) -> Result<bool> {
    destination.validate()?;
    let name = &destination.name;
    locked(|file| {
        ensure!(
            !file.destinations.iter().any(|saved| &saved.name == name),
            "a destination named {name} already exists; choose another name"
        );
        ensure!(file.destinations.len() < 256, "too many destinations");
        let Target::Folder { path } = &destination.target;
        crate::volumes::ensure_present(path)?;
        fs::create_dir_all(path)
            .with_context(|| format!("create destination folder {}", path.display()))?;
        file.destinations.push(destination.clone());
        file.destinations.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(false)
    })
}

/// Verify access without creating the destination folder or leaving a probe.
pub fn test(destination: &Destination) -> Result<()> {
    destination.validate()?;
    let root = destination.local_root();
    crate::volumes::ensure_present(&root)?;
    ensure!(
        root.is_dir(),
        "destination folder {} does not exist",
        root.display()
    );
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|error| anyhow!("generate destination test name: {error}"))?;
    let name: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
    let path = root.join(format!(".capturefab-write-test-{name}"));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut probe = options
        .open(&path)
        .context("create destination write test")?;
    let identity = probe.metadata()?;
    let written = probe
        .write_all(b"capturefab")
        .and_then(|()| probe.sync_all());
    drop(probe);
    let current = fs::symlink_metadata(&path)?;
    #[cfg(unix)]
    let same_file = {
        use std::os::unix::fs::MetadataExt;
        current.dev() == identity.dev() && current.ino() == identity.ino()
    };
    #[cfg(not(unix))]
    let same_file = identity
        .created()
        .ok()
        .is_some_and(|created| current.created().ok() == Some(created));
    ensure!(
        current.is_file() && !current.file_type().is_symlink() && same_file,
        "destination write-test file changed; refusing to remove it"
    );
    fs::remove_file(&path).context("remove destination write test")?;
    written.context("write destination test")?;
    Ok(())
}

pub fn remove(name: &str) -> Result<Destination> {
    validate_name(name)?;
    locked(|file| {
        let all = &mut file.destinations;
        let index = all
            .iter()
            .position(|d| d.name == name)
            .ok_or_else(|| anyhow!("destination {name} not found"))?;
        Ok(all.remove(index))
    })
}

/// A capture output resolved against an optional named folder.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Resolved {
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<PathBuf>,
}

pub fn resolve(name: Option<&str>, output: &str) -> Result<Resolved> {
    validate_output(name, output)?;
    let Some(name) = name else {
        return Ok(Resolved {
            output: output.into(),
            destination: None,
            root: None,
        });
    };
    resolve_with(&get(name)?, output)
}

pub fn resolve_with(destination: &Destination, output: &str) -> Result<Resolved> {
    destination.validate()?;
    validate_output(Some(&destination.name), output)?;
    let root = destination.local_root();
    let output_path = root.join(output);
    let output = output_path.to_str().context("destination output path must be valid UTF-8; rename the folder or choose another destination")?.to_owned();
    ensure!(
        resolve_existing_ancestor(&output_path)?.starts_with(resolve_existing_ancestor(&root)?),
        "destination output escapes its folder through a symbolic link; choose a path inside the destination"
    );
    crate::volumes::ensure_present(&root)?;
    ensure!(
        root.is_dir() || crate::volumes::mount_root(&root).is_none(),
        "destination folder {} does not exist",
        root.display()
    );
    Ok(Resolved {
        output,
        destination: Some(destination.name.clone()),
        root: Some(root),
    })
}

/// Check an output's shape before settings or storage can change. Network
/// forwarding can use URLs without a destination; named destinations use paths.
pub fn validate_output(name: Option<&str>, output: &str) -> Result<()> {
    ensure!(
        !output.trim().is_empty() && output.len() <= 16384 && !output.chars().any(char::is_control),
        "capture output needs a path without control characters (maximum 16384 bytes)"
    );
    if let Some(name) = name {
        validate_name(name)?;
        let relative = Path::new(output);
        ensure!(
            relative.is_relative()
                && !output.contains("://")
                && !output.starts_with("file:")
                && !relative.components().any(|c| matches!(
                    c,
                    std::path::Component::ParentDir
                        | std::path::Component::Prefix(_)
                        | std::path::Component::RootDir
                )),
            "with a destination, the output must be a relative name inside it, such as captures/run-1 or shot-{{frame}}.png"
        );
    }
    Ok(())
}

/// Resolve existing symlinks without creating directories for an invalid output.
fn resolve_existing_ancestor(path: &Path) -> Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(_) => fs::canonicalize(path)
            .with_context(|| format!("cannot resolve destination path {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .context("destination path has no existing ancestor")?;
            let name = path
                .file_name()
                .context("invalid destination output path")?;
            Ok(resolve_existing_ancestor(parent)?.join(name))
        }
        Err(error) => Err(error)
            .with_context(|| format!("cannot inspect destination path {}", path.display())),
    }
}

impl Resolved {
    /// Check containment again after a filename template expands.
    pub fn contain(&self, path: &Path) -> Result<()> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        ensure!(
            resolve_existing_ancestor(path)?.starts_with(resolve_existing_ancestor(root)?),
            "destination output escapes its folder through a symbolic link; choose a path inside the destination"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> Destination {
        Destination {
            name: name.into(),
            target: Target::Folder {
                path: std::env::temp_dir().join(name),
            },
        }
    }

    #[test]
    fn undo_destination_edits_preserve_unrelated_data_and_check_the_expected_version() {
        let (original, renamed, other) = (folder("original"), folder("renamed"), folder("other"));
        let mut file = File {
            version: 1,
            destinations: vec![original.clone(), other.clone()],
            preferred: Some(original.name.clone()),
        };
        replace_in_file(&mut file, Some(&original), Some(&renamed)).unwrap();
        assert_eq!(file.destinations, vec![other.clone(), renamed.clone()]);
        assert_eq!(file.preferred.as_deref(), Some("renamed"));
        // Undo is the reverse compare-and-swap, with the other destination left intact.
        replace_in_file(&mut file, Some(&renamed), Some(&original)).unwrap();
        assert_eq!(file.destinations, vec![original.clone(), other.clone()]);
        assert_eq!(file.preferred.as_deref(), Some("original"));
        let mut modified = original.clone();
        modified.target = Target::Folder {
            path: std::env::temp_dir().join("changed-elsewhere"),
        };
        file.destinations[0] = modified.clone();
        assert!(replace_in_file(&mut file, Some(&original), Some(&renamed)).is_err());
        assert_eq!(file.destinations, vec![modified, other]);
    }

    #[test]
    fn add_remove_and_rename_undo_refuse_to_overwrite_concurrent_destinations() {
        let (original, occupied) = (folder("original"), folder("occupied"));
        let mut file = File {
            version: 1,
            destinations: vec![original.clone(), occupied.clone()],
            preferred: Some(original.name.clone()),
        };
        let mut renamed = original.clone();
        renamed.name = occupied.name.clone();
        assert!(replace_in_file(&mut file, Some(&original), Some(&renamed)).is_err());
        assert_eq!(
            file.destinations,
            vec![original.clone(), occupied.clone()],
            "rename checks both names before touching either"
        );
        assert!(replace_in_file(&mut file, None, Some(&original)).is_err());
        replace_in_file(&mut file, Some(&original), None).unwrap();
        assert!(file.preferred.is_none());
        replace_in_file(&mut file, None, Some(&original)).unwrap();
        let mut concurrent = original.clone();
        concurrent.target = occupied.target.clone();
        replace_in_file(&mut file, Some(&original), Some(&concurrent)).unwrap();
        assert!(
            replace_in_file(&mut file, Some(&original), None).is_err(),
            "undo of an add cannot remove another editor's update"
        );
        assert!(file.destinations.contains(&concurrent));
    }

    #[test]
    fn persisted_undo_is_atomic_and_rolls_back_accompanying_state_when_writing_fails() {
        // Run the disk checks in a child with its own session directory. Tests
        // must never alter the launching user's destinations or process-wide
        // environment while other tests run.
        if std::env::var_os("CAPTUREFAB_DESTINATION_UNDO_TEST").is_none() {
            let dir = std::env::temp_dir().join(format!(
                "capturefab-destination-undo-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            crate::ipc::ensure_private_dir(&dir).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "destination::tests::persisted_undo_is_atomic_and_rolls_back_accompanying_state_when_writing_fails", "--nocapture"])
                .env("CAPTUREFAB_SESSION_DIR", &dir).env("CAPTUREFAB_DESTINATION_UNDO_TEST", "1")
                .output().unwrap();
            let _ = fs::remove_dir_all(dir);
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let (before, after, unrelated) = (folder("before"), folder("after"), folder("unrelated"));
        save(before.clone()).unwrap();
        save(unrelated.clone()).unwrap();
        set_preferred(Some(&before.name)).unwrap();
        replace(Some(&before), Some(&after)).unwrap();
        assert_eq!(get("after").unwrap(), after);
        assert!(get("before").is_err());
        assert_eq!(preferred().as_deref(), Some("after"));
        replace(Some(&after), Some(&before)).unwrap();
        assert_eq!(get("unrelated").unwrap(), unrelated);
        assert_eq!(preferred().as_deref(), Some("before"));
        let accompanying = std::cell::Cell::new("old");
        let temp =
            crate::ipc::state_dir().join(format!("destinations.json.{}.tmp", std::process::id()));
        let result = replace_with(
            Some(&before),
            Some(&after),
            || {
                accompanying.set("new");
                fs::create_dir(&temp)?; // Make opening the atomic-write sibling fail.
                Ok(())
            },
            |_| {
                accompanying.set("old");
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(
            accompanying.get(),
            "old",
            "restore accompanying state on persistence failure"
        );
        assert_eq!(
            get("before").unwrap(),
            before,
            "original settings stay readable"
        );
        assert!(get("after").is_err());
        fs::remove_dir(&temp).unwrap();
        let touched = std::cell::Cell::new(false);
        assert!(
            replace_with(
                Some(&after),
                None,
                || {
                    touched.set(true);
                    Ok(())
                },
                |_| Ok(())
            )
            .is_err()
        );
        assert!(
            !touched.get(),
            "conflict validation happens before side effects"
        );
    }

    #[test]
    #[cfg(unix)]
    fn named_outputs_cannot_escape_through_existing_symlinks() {
        let root = std::env::temp_dir().join(format!(
            "capturefab-destination-symlink-{}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("folder")).unwrap();
        fs::create_dir_all(root.join("outside")).unwrap();
        std::os::unix::fs::symlink(root.join("outside"), root.join("folder/link")).unwrap();
        let destination = Destination {
            name: "lab".into(),
            target: Target::Folder {
                path: root.join("folder"),
            },
        };
        let error = resolve_with(&destination, "link/new/run/shot.png").unwrap_err();
        assert!(error.to_string().contains("symbolic link"));
        assert!(!root.join("outside/new").exists());
        assert!(resolve_with(&destination, "inside/shot.png").is_ok());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn unsupported_folder_encoding_is_rejected_without_lossy_path_redirection() {
        use std::os::unix::ffi::OsStringExt;
        let root = std::env::temp_dir().join(std::ffi::OsString::from_vec(
            b"capturefab-invalid-utf8-\xff".to_vec(),
        ));
        let destination = Destination {
            name: "lab".into(),
            target: Target::Folder { path: root.clone() },
        };
        assert!(
            destination
                .validate()
                .unwrap_err()
                .to_string()
                .contains("valid UTF-8")
        );
        assert!(
            resolve_with(&destination, "new/shot.png")
                .unwrap_err()
                .to_string()
                .contains("valid UTF-8")
        );
        assert!(!root.join("new").exists());
        let replacement_path = PathBuf::from(root.to_string_lossy().as_ref());
        assert!(!replacement_path.join("new").exists());
    }

    fn isolated(name: &str) -> bool {
        if std::env::var("CAPTUREFAB_FOLDER_TEST").as_deref() == Ok(name) {
            return false;
        }
        let root = std::env::temp_dir().join(format!(
            "capturefab-folders-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        crate::ipc::ensure_private_dir(&root).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("CAPTUREFAB_SESSION_DIR", &root)
            .env("CAPTUREFAB_FOLDER_TEST", name)
            .output()
            .unwrap();
        let _ = fs::remove_dir_all(root);
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    #[test]
    fn create_makes_folders_and_never_replaces_a_saved_name() {
        if isolated("destination::tests::create_makes_folders_and_never_replaces_a_saved_name") {
            return;
        }
        let root = crate::ipc::state_dir().join("folders");
        let destination = Destination {
            name: "line-1".into(),
            target: Target::Folder {
                path: root.join("line-1/photos"),
            },
        };
        assert!(!create(&destination).unwrap());
        assert!(destination.local_root().is_dir());
        assert_eq!(get("line-1").unwrap(), destination);
        let duplicate = Destination {
            name: destination.name.clone(),
            target: Target::Folder {
                path: root.join("other"),
            },
        };
        assert!(
            create(&duplicate)
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
        assert!(!duplicate.local_root().exists());
        let invalid = Destination {
            name: "../bad".into(),
            target: Target::Folder {
                path: root.join("bad"),
            },
        };
        assert!(create(&invalid).is_err());
        assert!(!invalid.local_root().exists());
        assert_eq!(list().unwrap().len(), 1);
    }

    #[test]
    fn folder_paths_and_named_outputs_are_validated() {
        let saved = folder("camera-1");
        assert_eq!(
            serde_json::from_value::<Destination>(serde_json::to_value(&saved).unwrap()).unwrap(),
            saved
        );
        saved.validate().unwrap();
        let invalid = Destination {
            name: "relative".into(),
            target: Target::Folder {
                path: "relative/dir".into(),
            },
        };
        assert!(invalid.validate().is_err());
        for name in ["", "../bad", ".hidden", "has space"] {
            assert!(validate_name(name).is_err());
        }
        let resolved = resolve_with(&saved, "run/shot-{frame}.png").unwrap();
        assert_eq!(
            PathBuf::from(resolved.output),
            saved.local_root().join("run/shot-{frame}.png")
        );
        for output in [
            "/etc/capture.png",
            "../bad.png",
            "a/../../bad.png",
            "",
            "file:out.mp4",
            "https://host/live",
        ] {
            assert!(resolve_with(&saved, output).is_err(), "{output}");
        }
    }

    #[test]
    fn test_requires_an_existing_folder_and_leaves_no_probe() {
        if isolated("destination::tests::test_requires_an_existing_folder_and_leaves_no_probe") {
            return;
        }
        let destination = Destination {
            name: "test-folder".into(),
            target: Target::Folder {
                path: crate::ipc::state_dir().join("missing"),
            },
        };
        assert!(test(&destination).is_err());
        assert!(!destination.local_root().exists());
        create(&destination).unwrap();
        test(&destination).unwrap();
        assert_eq!(fs::read_dir(destination.local_root()).unwrap().count(), 0);
    }

    #[test]
    fn old_remote_records_are_ignored_without_modifying_the_saved_file() {
        if isolated(
            "destination::tests::old_remote_records_are_ignored_without_modifying_the_saved_file",
        ) {
            return;
        }
        let local = folder("usb");
        let raw=serde_json::json!({"version":1,"preferred":"archive","destinations":[
            {"name":"archive","kind":"s3","endpoint":"https://storage.example","credentials":{"source":"environment"}},
            {"name":"old-ftp","kind":"ftp","host":"example"},
            {"name":"old-sftp","kind":"sftp","host":"example"},
            serde_json::to_value(&local).unwrap()
        ]}).to_string();
        fs::write(path(), &raw).unwrap();
        assert_eq!(list().unwrap(), vec![local.clone()]);
        assert_eq!(preferred(), None);
        assert_eq!(fs::read_to_string(path()).unwrap(), raw);
        assert!(get("archive").is_err());
        set_preferred(Some("usb")).unwrap();
        assert_eq!(preferred().as_deref(), Some("usb"));
        assert_eq!(get("usb").unwrap(), local);
    }
}
