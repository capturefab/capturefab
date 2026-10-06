//! Named capture destinations: a folder (on any disk, including external and
//! network volumes) or an S3-compatible bucket. Captures, schedules and
//! recordings refer to a destination by name, so commands never carry secrets;
//! the list lives in the private session directory and holds no credentials.
//!
//! Every file is still written locally through `storage` (budgets, retention,
//! never overwriting). A bucket destination writes into a local staging folder
//! and queues each finished file for upload (see `upload`).
use anyhow::{Context, Result, anyhow, bail, ensure};
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
    S3(Bucket),
}

/// An S3-compatible bucket: AWS S3, Cloudflare R2, Backblaze B2, MinIO, Wasabi
/// and others that implement the S3 API with Signature Version 4.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Bucket {
    /// Service URL, for example `https://ACCOUNT.r2.cloudflarestorage.com`;
    /// empty for AWS S3, whose endpoint follows from the region.
    #[serde(default)]
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    /// Key prefix, without leading slash, for example `cameras/line-1/`.
    #[serde(default)]
    pub prefix: String,
    /// Address the bucket in the URL path rather than the host name. Most
    /// non-AWS services and MinIO need it.
    #[serde(default)]
    pub path_style: bool,
    pub credentials: Credentials,
    /// Keep the local copy after a verified upload.
    #[serde(default)]
    pub keep_local: bool,
}

/// Where an S3 destination's keys come from. Only the access key ID is stored
/// here; secrets stay in the OS credential store or the standard AWS sources.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum Credentials {
    /// The OS credential store (macOS Keychain, Windows Credential Manager,
    /// Secret Service on Linux), under this access key ID.
    Keychain { access_key_id: String },
    /// A profile in the AWS shared credentials file.
    Profile { name: String },
    /// `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and optionally
    /// `AWS_SESSION_TOKEN` in the environment of the uploading process.
    Environment,
}

impl Bucket {
    /// The service URL, defaulting to AWS S3 for the region.
    pub fn service_url(&self) -> String {
        if self.endpoint.is_empty() {
            format!("https://s3.{}.amazonaws.com", self.region)
        } else {
            self.endpoint.trim_end_matches('/').to_string()
        }
    }
    /// The service is reached without TLS, so object data travels unencrypted
    /// (requests are still signed; the secret key is never sent).
    pub fn insecure(&self) -> bool {
        self.endpoint.starts_with("http://")
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.endpoint.is_empty()
                || self.endpoint.starts_with("https://")
                || self.endpoint.starts_with("http://"),
            "S3 endpoint must be an http:// or https:// URL"
        );
        ensure!(
            !self.endpoint.contains(['?', '#', '@', ' ']),
            "S3 endpoint must be a plain service URL without credentials, query or fragment"
        );
        ensure!(
            !self.region.is_empty()
                && self.region.len() <= 64
                && self
                    .region
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "S3 region must be a name such as us-east-1 or auto"
        );
        // S3 naming rules; dots are allowed but break virtual-hosted TLS.
        ensure!(
            (3..=63).contains(&self.bucket.len())
                && self.bucket.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'-'
                    || b == b'.')
                && self.bucket.as_bytes()[0].is_ascii_alphanumeric()
                && self.bucket.as_bytes()[self.bucket.len() - 1].is_ascii_alphanumeric(),
            "S3 bucket names are 3-63 lowercase letters, digits, dots and hyphens"
        );
        ensure!(
            self.prefix.len() <= 512
                && !self.prefix.starts_with('/')
                && !self
                    .prefix
                    .split('/')
                    .any(|part| part == ".." || part == ".")
                && !self.prefix.contains(['\\', '\0']),
            "S3 key prefix must be relative, like cameras/line-1/"
        );
        match &self.credentials {
            Credentials::Keychain { access_key_id } => ensure!(
                !access_key_id.is_empty()
                    && access_key_id.len() <= 128
                    && access_key_id.bytes().all(|b| b.is_ascii_graphic()),
                "S3 access key ID is empty or invalid"
            ),
            Credentials::Profile { name } => ensure!(
                !name.is_empty()
                    && name.len() <= 64
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
                "AWS profile name is invalid"
            ),
            Credentials::Environment => {}
        }
        Ok(())
    }
}

impl Destination {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.name.is_empty()
                && self.name.len() <= 64
                && self
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                && !self.name.starts_with('.'),
            "destination names are 1-64 letters, digits, '.', '-' and '_'"
        );
        match &self.target {
            Target::Folder { path } => ensure!(
                path.is_absolute(),
                "a folder destination needs an absolute path"
            ),
            Target::S3(bucket) => bucket.validate()?,
        }
        Ok(())
    }
    /// Local directory files for this destination are written under.
    pub fn local_root(&self) -> PathBuf {
        match &self.target {
            Target::Folder { path } => path.clone(),
            Target::S3(_) => staging_root().join(&self.name),
        }
    }
    /// Short description for lists and logs, without secrets.
    pub fn describe(&self) -> String {
        match &self.target {
            Target::Folder { path } => path.display().to_string(),
            Target::S3(bucket) => format!(
                "s3://{}/{} at {}",
                bucket.bucket,
                bucket.prefix,
                crate::media::redact_url(&bucket.service_url())
            ),
        }
    }
}

/// Where bucket destinations stage files before upload: the platform's
/// per-user data directory, or `CAPTUREFAB_STAGING_DIR`.
pub fn staging_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("CAPTUREFAB_STAGING_DIR") {
        return dir.into();
    }
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    let base = if cfg!(target_os = "macos") {
        home().map(|h| h.join("Library/Application Support/Capturefab"))
    } else if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Capturefab"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(|d| PathBuf::from(d).join("capturefab"))
            .or_else(|| home().map(|h| h.join(".local/share/capturefab")))
    };
    base.unwrap_or_else(std::env::temp_dir).join("uploads")
}

#[derive(Serialize, Deserialize, Default)]
struct File {
    version: u32,
    destinations: Vec<Destination>,
    /// The destination the GUI last saved to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preferred: Option<String>,
}

fn path() -> PathBuf {
    crate::ipc::session_dir().join("destinations.json")
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
    Ok(file)
}

/// The destination the GUI last saved to, if it still exists.
pub fn preferred() -> Option<String> {
    let file = read().ok()?;
    file.preferred
        .filter(|name| file.destinations.iter().any(|d| &d.name == name))
}

pub fn set_preferred(name: Option<&str>) -> Result<()> {
    locked(|file| {
        file.preferred = name.map(str::to_string);
        Ok(())
    })
}

pub fn get(name: &str) -> Result<Destination> {
    list()?.into_iter().find(|d| d.name == name).ok_or_else(|| {
        anyhow!("destination {name} not found; list them with `capturefab destination list`")
    })
}

fn write(file: &File) -> Result<()> {
    let dir = crate::ipc::session_dir();
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
    use fs2::FileExt;
    let dir = crate::ipc::session_dir();
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
    write(&file)?;
    Ok(result)
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

pub fn remove(name: &str) -> Result<Destination> {
    locked(|file| {
        let all = &mut file.destinations;
        let index = all
            .iter()
            .position(|d| d.name == name)
            .ok_or_else(|| anyhow!("destination {name} not found"))?;
        Ok(all.remove(index))
    })
}

/// A capture output resolved against an optional destination: the local path
/// pattern to write, and where finished files are uploaded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Resolved {
    /// Output pattern (file, directory or `{frame}` template) on local disk.
    pub output: String,
    /// The destination's name, for messages and the upload queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    /// Upload finished files here; their keys are the prefix plus their path
    /// below `root`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload: Option<Bucket>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<PathBuf>,
}

/// Resolve `output` for capture. Without a destination it is used as is; with
/// one it must be relative (a file name, folder or `{frame}` template) and is
/// placed inside the destination's folder or staging area.
pub fn resolve(name: Option<&str>, output: &str) -> Result<Resolved> {
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        return Ok(Resolved {
            output: output.into(),
            destination: None,
            upload: None,
            root: None,
        });
    };
    let destination = get(name)?;
    resolve_with(&destination, output)
}

pub fn resolve_with(destination: &Destination, output: &str) -> Result<Resolved> {
    let relative = Path::new(output);
    ensure!(
        !output.is_empty()
            && relative.is_relative()
            && !relative.components().any(|c| matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )),
        "with a destination, the output is a name inside it (for example captures/run-1 or shot-{{frame}}.png), not {output}"
    );
    let root = destination.local_root();
    if let Target::Folder { path } = &destination.target {
        crate::volumes::ensure_present(path)?;
        ensure!(
            path.is_dir() || crate::volumes::mount_root(path).is_none(),
            "destination folder {} does not exist",
            path.display()
        );
    }
    let upload = match &destination.target {
        Target::Folder { .. } => None,
        Target::S3(bucket) => {
            if !cfg!(feature = "s3") {
                bail!("S3 destinations are unsupported in this build; rebuild with --features s3");
            }
            Some(bucket.clone())
        }
    };
    Ok(Resolved {
        output: root.join(relative).to_string_lossy().into_owned(),
        destination: Some(destination.name.clone()),
        upload,
        root: Some(root),
    })
}

impl Resolved {
    /// Queue a file this resolution produced for upload, if it has a bucket.
    /// Called after the file is complete and recorded in the storage ledger.
    pub fn finished(&self, path: &Path) -> Result<()> {
        let (Some(bucket), Some(root), Some(name)) = (&self.upload, &self.root, &self.destination)
        else {
            return Ok(());
        };
        let key = object_key(&bucket.prefix, root, path)?;
        #[cfg(feature = "s3")]
        {
            crate::upload::enqueue(name, bucket, path, &key)
        }
        #[cfg(not(feature = "s3"))]
        {
            let _ = (name, key);
            bail!("S3 destinations are unsupported in this build")
        }
    }
}

/// The object key for a staged file: the prefix plus its path below the
/// staging root, with forward slashes.
pub fn object_key(prefix: &str, root: &Path, path: &Path) -> Result<String> {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let relative = path
        .strip_prefix(&root)
        .map_err(|_| anyhow!("{} is outside the staging folder", path.display()))?;
    let parts: Vec<String> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    ensure!(!parts.is_empty(), "empty object key");
    let mut key = prefix.trim_start_matches('/').to_string();
    if !key.is_empty() && !key.ends_with('/') {
        key.push('/');
    }
    key.push_str(&parts.join("/"));
    ensure!(key.len() <= 1024, "object key is longer than 1024 bytes");
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket() -> Bucket {
        Bucket {
            endpoint: "https://minio.example:9000".into(),
            region: "us-east-1".into(),
            bucket: "camera-archive".into(),
            prefix: "line-1".into(),
            path_style: true,
            credentials: Credentials::Keychain {
                access_key_id: "AKIDEXAMPLE".into(),
            },
            keep_local: false,
        }
    }

    #[test]
    fn destinations_validate_names_paths_and_buckets() {
        let folder = |name: &str, path: &str| Destination {
            name: name.into(),
            target: Target::Folder { path: path.into() },
        };
        let root = if cfg!(windows) {
            "C:\\captures"
        } else {
            "/captures"
        };
        assert!(folder("usb-drive", root).validate().is_ok());
        assert!(folder("../x", root).validate().is_err());
        assert!(folder("rel", "relative/dir").validate().is_err());
        let s3 = |change: fn(&mut Bucket)| {
            let mut b = bucket();
            change(&mut b);
            Destination {
                name: "archive".into(),
                target: Target::S3(b),
            }
            .validate()
        };
        assert!(s3(|_| {}).is_ok());
        assert!(s3(|b| b.bucket = "Upper".into()).is_err());
        assert!(s3(|b| b.bucket = "ab".into()).is_err());
        assert!(s3(|b| b.prefix = "/abs".into()).is_err());
        assert!(s3(|b| b.prefix = "a/../b".into()).is_err());
        assert!(s3(|b| b.endpoint = "ftp://x".into()).is_err());
        assert!(s3(|b| b.endpoint = "https://user:pw@host".into()).is_err());
        assert!(s3(|b| b.region = "us east".into()).is_err());
        assert!(
            s3(|b| b.credentials = Credentials::Keychain {
                access_key_id: String::new()
            })
            .is_err()
        );
        let aws = Bucket {
            endpoint: String::new(),
            ..bucket()
        };
        assert_eq!(aws.service_url(), "https://s3.us-east-1.amazonaws.com");
        assert!(!aws.insecure());
    }

    #[test]
    fn resolution_places_relative_outputs_inside_the_destination() {
        let dir = std::env::temp_dir().join(format!("capturefab-dest-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let folder = Destination {
            name: "lab".into(),
            target: Target::Folder { path: dir.clone() },
        };
        let resolved = resolve_with(&folder, "run-1/shot-{frame}.png").unwrap();
        assert_eq!(
            PathBuf::from(&resolved.output),
            dir.join("run-1/shot-{frame}.png")
        );
        assert!(resolved.upload.is_none());
        for bad in ["/etc/x.png", "../x.png", "a/../../x.png", ""] {
            assert!(resolve_with(&folder, bad).is_err(), "{bad}");
        }
        let staged = Destination {
            name: "archive".into(),
            target: Target::S3(bucket()),
        };
        if cfg!(feature = "s3") {
            let resolved = resolve_with(&staged, "frames").unwrap();
            assert!(resolved.output.ends_with(&format!(
                "uploads{}archive{}frames",
                std::path::MAIN_SEPARATOR,
                std::path::MAIN_SEPARATOR
            )));
            assert_eq!(resolved.upload.as_ref().unwrap().bucket, "camera-archive");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn object_keys_join_the_prefix_and_relative_path() {
        let root = Path::new("/staging/archive");
        let key = |prefix: &str, path: &str| object_key(prefix, root, Path::new(path));
        assert_eq!(
            key("line-1", "/staging/archive/run/a.png").unwrap(),
            "line-1/run/a.png"
        );
        assert_eq!(
            key("line-1/", "/staging/archive/a.png").unwrap(),
            "line-1/a.png"
        );
        assert_eq!(key("", "/staging/archive/a.png").unwrap(), "a.png");
        assert!(key("x", "/elsewhere/a.png").is_err());
    }
}
