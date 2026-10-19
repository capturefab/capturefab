//! Bounded private metadata with compare-and-save and immutable keychain accounts.
#[cfg(feature = "keychain")]
use super::KEYCHAIN_SERVICE;
use super::{ProfileDraft, ProfileInfo, ProfileRef, Secret, SecretSource, random_id};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_FILE: usize = 1 << 20;
const MAX_PROFILES: usize = 256;
const MAX_RETIRED: usize = 512;

/// Injectable local credential access. Listing/editing metadata never calls it.
pub trait SecretStore: Send + Sync {
    /// A static backend capability; this must not probe the OS store.
    fn keychain_supported(&self) -> bool {
        true
    }
    fn environment(&self, variable: &str) -> Result<Option<Secret>>;
    fn read_keychain(&self, account: &str) -> Result<Option<Secret>>;
    fn write_keychain(&self, account: &str, password: &Secret) -> Result<()>;
    fn delete_keychain(&self, account: &str) -> Result<()>;
}
struct Native;
impl SecretStore for Native {
    fn keychain_supported(&self) -> bool {
        cfg!(feature = "keychain")
    }
    fn environment(&self, variable: &str) -> Result<Option<Secret>> {
        match std::env::var(variable) {
            Ok(value) if !value.is_empty() => Secret::new(value.into()).map(Some),
            _ => Ok(None),
        }
    }
    fn read_keychain(&self, account: &str) -> Result<Option<Secret>> {
        #[cfg(feature = "keychain")]
        {
            crate::keychain::read_secret(KEYCHAIN_SERVICE, account)
                .map_err(|_| anyhow!("cannot read the signal password from the OS credential store; unlock it and bind again"))?
                .map(|value| Secret::new(value.into())).transpose()
        }
        #[cfg(not(feature = "keychain"))]
        {
            let _ = account;
            Err(keychain_unsupported())
        }
    }
    fn write_keychain(&self, account: &str, password: &Secret) -> Result<()> {
        #[cfg(feature = "keychain")]
        {
            crate::keychain::store_secret(KEYCHAIN_SERVICE, account, password.expose()).map_err(|_| anyhow!("cannot save the signal password in the OS credential store; unlock it and save again"))
        }
        #[cfg(not(feature = "keychain"))]
        {
            let _ = (account, password);
            Err(keychain_unsupported())
        }
    }
    fn delete_keychain(&self, account: &str) -> Result<()> {
        #[cfg(feature = "keychain")]
        {
            crate::keychain::delete_secret(KEYCHAIN_SERVICE, account).map_err(|_| {
                anyhow!("cannot remove an unused signal password from the OS credential store")
            })
        }
        #[cfg(not(feature = "keychain"))]
        {
            let _ = account;
            Err(keychain_unsupported())
        }
    }
}
fn keychain_unsupported() -> anyhow::Error {
    crate::types::Categorized::error(
        crate::types::ErrorKind::Unsupported,
        "the OS credential store is unavailable in this build; use a CAPTUREFAB_SIGNAL_ environment reference",
    )
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub profile: ProfileRef,
    pub name: String,
    pub endpoint: String,
    pub username: Option<String>,
    pub secret: SecretSource,
}
impl Record {
    pub fn draft(&self) -> ProfileDraft {
        ProfileDraft {
            name: self.name.clone(),
            endpoint: self.endpoint.clone(),
            username: self.username.clone(),
            secret: self.secret.clone(),
        }
    }
    pub fn info(&self) -> Result<ProfileInfo> {
        let parsed = super::super::parse(&self.endpoint)?;
        let insecure = match &parsed.target {
            super::super::Target::Mqtt { tls, .. } => !tls,
            super::super::Target::Opcua { .. } | super::super::Target::Modbus(_) => true,
            super::super::Target::Mtconnect { url, .. } => url.starts_with("http://"),
            super::super::Target::Sim { .. } => false,
        };
        Ok(ProfileInfo {
            profile: self.profile.clone(),
            name: self.name.clone(),
            endpoint: self.endpoint.clone(),
            reference: super::super::source_id(&self.endpoint),
            protocol: parsed.protocol,
            secret: self.secret.kind(),
            insecure,
        })
    }
    pub fn account(&self) -> String {
        format!("{}:{}", self.profile.id, self.profile.revision)
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    version: u32,
    profiles: Vec<Record>,
    #[serde(default)]
    retired_accounts: Vec<String>,
}
impl Default for File {
    fn default() -> Self {
        Self {
            version: 1,
            profiles: Vec::new(),
            retired_accounts: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct Store {
    dir: PathBuf,
    secrets: Arc<dyn SecretStore>,
}
impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignalProfileStore").finish_non_exhaustive()
    }
}
impl Store {
    /// No file or credential access occurs in either constructor.
    pub fn session() -> Self {
        Self::at(crate::ipc::state_dir())
    }
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            secrets: Arc::new(Native),
        }
    }
    pub fn with_secret_store(mut self, secrets: Arc<dyn SecretStore>) -> Self {
        self.secrets = secrets;
        self
    }
    fn path(&self) -> PathBuf {
        self.dir.join("signal-profiles.json")
    }
    pub fn list(&self) -> Result<Vec<ProfileInfo>> {
        self.read()?.profiles.iter().map(Record::info).collect()
    }
    pub fn get(&self, profile: &ProfileRef) -> Result<ProfileInfo> {
        self.with_current(profile, Record::info)
    }
    pub fn load_draft(&self, profile: &ProfileRef) -> Result<ProfileDraft> {
        self.with_current(profile, |record| Ok(record.draft()))
    }
    fn read(&self) -> Result<File> {
        let path = self.path();
        let input = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(File::default());
            }
            Err(error) => return Err(error).context("read local signal profiles"),
        };
        let metadata = input.metadata()?;
        ensure!(
            metadata.is_file() && !fs::symlink_metadata(&path)?.file_type().is_symlink(),
            "the signal profile file must be a regular private file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                metadata.mode() & 0o077 == 0 && metadata.uid() == unsafe { libc::geteuid() },
                "the signal profile file must belong to this user with mode 600"
            );
        }
        let mut bytes = Vec::new();
        input.take((MAX_FILE + 1) as u64).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_FILE,
            "the local signal profile file is too large"
        );
        let file: File = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow!("the local signal profile file is invalid"))?;
        ensure!(
            file.version == 1
                && file.profiles.len() <= MAX_PROFILES
                && file.retired_accounts.len() <= MAX_RETIRED,
            "the local signal profile file has an unsupported version or too many entries"
        );
        let mut ids = std::collections::BTreeSet::new();
        let mut names = std::collections::BTreeSet::new();
        for record in &file.profiles {
            record.profile.validate()?;
            record.draft().validate()?;
            ensure!(
                ids.insert(&record.profile.id) && names.insert(record.name.to_ascii_lowercase()),
                "the local signal profile file has duplicate profiles"
            );
        }
        for account in &file.retired_accounts {
            let (id, rev) = account
                .split_once(':')
                .ok_or_else(|| anyhow!("invalid retired signal credential identity"))?;
            ProfileRef {
                id: id.into(),
                revision: rev.into(),
            }
            .validate()?;
            ensure!(
                !file
                    .profiles
                    .iter()
                    .any(|record| record.secret == SecretSource::Keychain
                        && record.account() == *account),
                "an active signal credential cannot be retired"
            );
        }
        Ok(file)
    }
    fn lock(&self) -> Result<fs::File> {
        use fs2::FileExt;
        crate::ipc::ensure_private_dir(&self.dir)?;
        let mut options = fs::OpenOptions::new();
        options.create(true).truncate(false).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let lock = options.open(self.dir.join("signal-profiles.lock"))?;
        lock.lock_exclusive()?;
        Ok(lock)
    }
    fn write(&self, file: &File) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(file)?;
        ensure!(
            bytes.len() <= MAX_FILE,
            "the local signal profile file is too large"
        );
        let path = self
            .dir
            .join(format!("signal-profiles-{}.tmp", random_id()?));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let result = (|| {
            let mut out = options.open(&path)?;
            out.write_all(&bytes)?;
            out.sync_all()?;
            fs::rename(&path, self.path())?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(path);
        }
        result
    }
    pub(super) fn with_current<T>(
        &self,
        profile: &ProfileRef,
        run: impl FnOnce(&Record) -> Result<T>,
    ) -> Result<T> {
        profile.validate()?;
        let _lock = self.lock()?;
        let file = self.read()?;
        let record = file
            .profiles
            .iter()
            .find(|record| record.profile == *profile)
            .ok_or_else(|| {
                anyhow!(
                    "signal profile changed or was removed; select it locally and bind it again"
                )
            })?;
        run(record)
    }
    pub(super) fn resolve(&self, record: &Record) -> Result<Option<Secret>> {
        if record.secret == SecretSource::Keychain && !self.secrets.keychain_supported() {
            return Err(keychain_unsupported());
        }
        let secret=match &record.secret {
            SecretSource::None=>return Ok(None),
            SecretSource::Environment{variable}=>self.secrets.environment(variable),
            SecretSource::Keychain=>self.secrets.read_keychain(&record.account()),
        }.map_err(|_|anyhow!("cannot resolve the signal password locally; unlock the OS store or check the configured environment, then bind again"))?;
        secret.map(Some).ok_or_else(||anyhow!("the signal password is unavailable in this coordinator; save it locally or set its environment variable, then bind again"))
    }
    /// Compare-and-save. Typed passwords go to an immutable revision account.
    pub fn save(
        &self,
        expected: Option<&ProfileRef>,
        draft: ProfileDraft,
        password: Option<Secret>,
    ) -> Result<ProfileInfo> {
        draft.validate()?;
        if draft.secret == SecretSource::Keychain && !self.secrets.keychain_supported() {
            return Err(keychain_unsupported());
        }
        if let Some(expected) = expected {
            expected.validate()?;
        }
        let _lock = self.lock()?;
        let mut file = self.read()?;
        self.cleanup(&mut file);
        let index = match expected {
            Some(expected) => Some(
                file.profiles
                    .iter()
                    .position(|record| record.profile == *expected)
                    .ok_or_else(|| anyhow!("signal profile changed; reload it before saving"))?,
            ),
            None => None,
        };
        ensure!(
            index.is_some() || file.profiles.len() < MAX_PROFILES,
            "too many local signal profiles (maximum 256)"
        );
        ensure!(
            !file.profiles.iter().enumerate().any(
                |(i, record)| Some(i) != index && record.name.eq_ignore_ascii_case(&draft.name)
            ),
            "a signal profile with this name already exists"
        );
        ensure!(
            file.retired_accounts.len() < MAX_RETIRED,
            "unused signal credentials could not be removed; unlock the OS store before saving again"
        );
        let previous = index.map(|index| file.profiles[index].clone());
        let retained = if draft.secret == SecretSource::Keychain && password.is_none() {
            let old = previous
                .as_ref()
                .filter(|old| {
                    old.secret == SecretSource::Keychain
                        && old.endpoint == draft.endpoint
                        && old.username == draft.username
                })
                .ok_or_else(|| {
                    anyhow!("enter a password when creating or changing a keychain signal identity")
                })?;
            self.resolve(old)?
        } else {
            None
        };
        ensure!(
            password.is_none() || draft.secret == SecretSource::Keychain,
            "a typed password is accepted only for the OS credential store"
        );
        let record = Record {
            profile: ProfileRef {
                id: previous
                    .as_ref()
                    .map_or_else(random_id, |old| Ok(old.profile.id.clone()))?,
                revision: random_id()?,
            },
            name: draft.name,
            endpoint: draft.endpoint,
            username: draft.username,
            secret: draft.secret,
        };
        let password = password.as_ref().or(retained.as_ref());
        let mut rollback = file.clone();
        if record.secret == SecretSource::Keychain
            && self
                .secrets
                .write_keychain(
                    &record.account(),
                    password.context("enter a signal password")?,
                )
                .is_err()
        {
            // A failed OS-store operation can have an uncertain write result.
            // This new revision account cannot be used by another profile.
            self.discard_new_credential(&mut rollback, &record.account())?;
            bail!(
                "the signal password could not be saved; its new unused credential was removed; unlock the OS store and save again"
            );
        }
        if let Some(old) = &previous
            && old.secret == SecretSource::Keychain
        {
            file.retired_accounts.push(old.account());
        }
        match index {
            Some(index) => file.profiles[index] = record.clone(),
            None => file.profiles.push(record.clone()),
        };
        file.profiles.sort_by(|a, b| a.name.cmp(&b.name));
        if let Err(error) = self.write(&file) {
            if record.secret == SecretSource::Keychain {
                self.discard_new_credential(&mut rollback, &record.account())?;
            }
            return Err(error);
        }
        self.cleanup(&mut file);
        record.info()
    }
    fn discard_new_credential(&self, file: &mut File, account: &str) -> Result<()> {
        if self.secrets.delete_keychain(account).is_ok() {
            return Ok(());
        }
        if !file
            .retired_accounts
            .iter()
            .any(|retired| retired == account)
            && file.retired_accounts.len() < MAX_RETIRED
        {
            file.retired_accounts.push(account.into());
            if self.write(file).is_ok() {
                bail!(
                    "the profile was not saved and a new unused signal credential could not be removed; cleanup was recorded; unlock the OS store and save or remove a local profile to retry cleanup"
                );
            }
        }
        bail!(
            "the profile was not saved and removal of a new unused signal credential is unconfirmed; cleanup could not be recorded; remove OS-store account {account} in capturefab-signal before retrying"
        );
    }
    pub fn delete(&self, expected: &ProfileRef) -> Result<()> {
        expected.validate()?;
        let _lock = self.lock()?;
        let mut file = self.read()?;
        self.cleanup(&mut file);
        let index = file
            .profiles
            .iter()
            .position(|record| record.profile == *expected)
            .ok_or_else(|| anyhow!("signal profile changed; reload it before removing"))?;
        ensure!(
            file.retired_accounts.len() < MAX_RETIRED,
            "unused signal credentials could not be removed; unlock the OS store before removing another profile"
        );
        let removed = file.profiles.remove(index);
        if removed.secret == SecretSource::Keychain {
            file.retired_accounts.push(removed.account());
        }
        self.write(&file)?;
        self.cleanup(&mut file);
        if file.retired_accounts.is_empty() {
            Ok(())
        } else {
            bail!(
                "the profile was removed, but unused signal credentials remain in the OS store; unlock it and clean up local signal profiles"
            )
        }
    }
    fn cleanup(&self, file: &mut File) {
        let before = file.retired_accounts.len();
        file.retired_accounts
            .retain(|account| self.secrets.delete_keychain(account).is_err());
        if before != file.retired_accounts.len() {
            let _ = self.write(file);
        }
    }
    pub fn directory(&self) -> &Path {
        &self.dir
    }
}
