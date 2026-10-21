//! Non-secret assistant settings. Reads are lenient and never prevent the
//! workbench opening; writes are locked, bounded, atomic and private.
use super::{
    limits_defaults::Limits,
    provider::{ProviderId, ProviderSettings},
    usage::Prices,
};
use anyhow::{Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

pub const VERSION: u32 = 1;
pub const MAX_BYTES: u64 = 64 * 1024;
const LOCK_WAIT: Duration = Duration::from_secs(if cfg!(test) { 60 } else { 5 });
static NEXT_WRITE: AtomicU64 = AtomicU64::new(1);

pub(crate) static STATE: crate::ipc::StateSet = crate::ipc::StateSet {
    what: "assistant settings",
    names: &["assistant.json"],
    lock: Some("assistant.lock"),
};

fn dir() -> PathBuf {
    static ADOPTED: std::sync::Once = std::sync::Once::new();
    ADOPTED.call_once(|| crate::ipc::adopt_state(&STATE));
    crate::ipc::state_dir()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultMode {
    #[default]
    Plan,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Autonomy {
    pub limits: Limits,
    pub see_images: bool,
    pub mode: DefaultMode,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub version: u32,
    pub provider: Option<ProviderId>,
    pub providers: BTreeMap<ProviderId, ProviderSettings>,
    pub autonomy: Autonomy,
    /// USD per million tokens. Empty until the user enters prices.
    pub prices: BTreeMap<String, Prices>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            version: VERSION,
            provider: None,
            providers: BTreeMap::new(),
            autonomy: Autonomy::default(),
            prices: BTreeMap::new(),
        }
    }
}
impl Settings {
    pub fn selected(&self) -> Option<(ProviderId, ProviderSettings)> {
        self.provider.map(|id| (id, self.provider_settings(id)))
    }
    pub fn provider_settings(&self, id: ProviderId) -> ProviderSettings {
        self.providers.get(&id).cloned().unwrap_or_default()
    }
    pub fn set_provider_settings(
        &mut self,
        id: ProviderId,
        settings: ProviderSettings,
    ) -> Result<()> {
        validate_provider(id, &settings)?;
        self.providers.insert(id, settings);
        Ok(())
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == VERSION, "Use assistant settings version 1.");
        for (id, settings) in &self.providers {
            validate_provider(*id, settings)?;
        }
        if let Err(issues) = self.autonomy.limits.validate() {
            anyhow::bail!("{}", issues.join(" "));
        }
        for (key, prices) in &self.prices {
            ensure!(
                valid_price_key(key) && prices.validate().is_ok(),
                "Use a provider/model price with finite, nonnegative USD amounts per million tokens."
            );
        }
        Ok(())
    }
}
fn validate_provider(id: ProviderId, settings: &ProviderSettings) -> Result<()> {
    if let Err(issues) = settings.validate(id) {
        anyhow::bail!(
            "{}",
            issues
                .iter()
                .map(|issue| issue.message.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    Ok(())
}
fn valid_price_key(key: &str) -> bool {
    let Some((id, model)) = key.split_once('/') else {
        return false;
    };
    id.parse::<ProviderId>().is_ok()
        && !model.is_empty()
        && model.len() <= 200
        && !model.starts_with('/')
        && !model.contains("..")
        && model.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'/' | b'@' | b'-')
        })
}

#[derive(Clone, Debug, Default)]
pub struct Loaded {
    pub settings: Settings,
    pub warnings: Vec<String>,
}

pub fn load() -> Loaded {
    load_in(&dir())
}

/// Read-only; a missing settings file does not create the state directory.
pub fn load_in(directory: &Path) -> Loaded {
    let path = directory.join("assistant.json");
    match read_private(&path) {
        Ok(Some(value)) => decode(value),
        Ok(None) => Loaded::default(),
        Err(_) => Loaded {
            warnings: vec!["Assistant settings could not be read safely; using defaults.".into()],
            ..Loaded::default()
        },
    }
}
fn decode(value: Value) -> Loaded {
    let mut out = Loaded::default();
    if !value.is_object() || value["version"] != VERSION {
        out.warnings
            .push("Assistant settings have an unreadable version; using defaults.".into());
        return out;
    }
    if !value["provider"].is_null() {
        match serde_json::from_value::<ProviderId>(value["provider"].clone()) {
            Ok(provider) => out.settings.provider = Some(provider),
            Err(_) => out.warnings.push(
                "The selected assistant provider is unknown; choose a provider in Settings.".into(),
            ),
        }
    }
    if let Some(entries) = value["providers"].as_object() {
        for (name, entry) in entries {
            let Ok(id) = name.parse::<ProviderId>() else {
                out.warnings
                    .push("An unknown assistant provider entry was ignored.".into());
                continue;
            };
            match serde_json::from_value::<ProviderSettings>(entry.clone()) {
                Ok(settings) if settings.validate(id).is_ok() => {
                    out.settings.providers.insert(id, settings);
                }
                _ => out.warnings.push(format!(
                    "The {} settings entry is invalid; using its defaults.",
                    id.info().name
                )),
            }
        }
    } else if !value["providers"].is_null() {
        out.warnings
            .push("The assistant provider settings are unreadable; using defaults.".into());
    }
    if !value["autonomy"].is_null() {
        match serde_json::from_value::<Autonomy>(value["autonomy"].clone()) {
            Ok(autonomy) if autonomy.limits.validate().is_ok() => out.settings.autonomy = autonomy,
            _ => out
                .warnings
                .push("The assistant task defaults are invalid; using safe defaults.".into()),
        }
    }
    if let Some(entries) = value["prices"].as_object() {
        for (key, entry) in entries {
            match serde_json::from_value::<Prices>(entry.clone()) {
                Ok(prices) if valid_price_key(key) && prices.validate().is_ok() => {
                    out.settings.prices.insert(key.clone(), prices);
                }
                _ => out
                    .warnings
                    .push("An invalid assistant price entry was ignored.".into()),
            }
        }
    } else if !value["prices"].is_null() {
        out.warnings
            .push("The assistant prices are unreadable; no prices were loaded.".into());
    }
    out
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}
fn regular_private(path: &Path, max: u64) -> Result<fs::Metadata> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= max,
        "Use a regular private assistant settings file no larger than 64 KiB."
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            meta.permissions().mode() & 0o077 == 0,
            "Keep assistant settings private (chmod 600)."
        );
    }
    Ok(meta)
}
fn read_private(path: &Path) -> Result<Option<Value>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    regular_private(path, MAX_BYTES)?;
    let mut file = File::open(path)?;
    let path_meta = regular_private(path, MAX_BYTES)?;
    let opened = file.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            path_meta.dev() == opened.dev() && path_meta.ino() == opened.ino(),
            "The assistant settings file changed while opening it."
        );
    }
    #[cfg(not(unix))]
    {
        ensure!(
            path_meta.len() == opened.len(),
            "The assistant settings file changed while opening it."
        );
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "Keep assistant settings within 64 KiB."
    );
    Ok(Some(serde_json::from_slice(&bytes)?))
}
struct Locked {
    file: File,
    directory: PathBuf,
}
impl Drop for Locked {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}
fn lock(directory: &Path) -> Result<Locked> {
    crate::ipc::ensure_private_dir(directory)?;
    let directory = fs::canonicalize(directory)?;
    let path = directory.join("assistant.lock");
    let file = match private_options().create_new(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            regular_private(&path, 0)?;
            private_options().open(&path)?
        }
        Err(error) => return Err(error.into()),
    };
    let path_meta = regular_private(&path, 0)?;
    let opened = file.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            path_meta.dev() == opened.dev() && path_meta.ino() == opened.ino(),
            "The assistant settings lock changed while opening it."
        );
    }
    #[cfg(not(unix))]
    {
        ensure!(
            opened.is_file() && opened.len() == path_meta.len(),
            "Use a regular assistant settings lock file."
        );
    }
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(Locked { file, directory }),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
            {
                ensure!(
                    Instant::now() < deadline,
                    "Assistant settings are busy; try saving again."
                );
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
}
fn persist(locked: &Locked, settings: &Settings) -> Result<()> {
    settings.validate()?;
    let bytes = serde_json::to_vec_pretty(settings)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "Keep assistant settings within 64 KiB."
    );
    let path = locked.directory.join("assistant.json");
    if fs::symlink_metadata(&path).is_ok() {
        regular_private(&path, MAX_BYTES)?;
    }
    for _ in 0..100 {
        let sequence = NEXT_WRITE.fetch_add(1, Ordering::Relaxed);
        let temp = locked
            .directory
            .join(format!("assistant-{}-{sequence}.tmp", std::process::id()));
        let mut file = match private_options().create_new(true).open(&temp) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };
        let result = (|| {
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp, &path)?;
            #[cfg(unix)]
            File::open(&locked.directory)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        return result;
    }
    anyhow::bail!("Choose a new temporary assistant settings file.")
}
pub fn save_in(directory: &Path, settings: &Settings) -> Result<()> {
    let locked = lock(directory)?;
    persist(&locked, settings)
}
fn update_in(directory: &Path, update: impl FnOnce(&mut Settings) -> Result<()>) -> Result<()> {
    let locked = lock(directory)?;
    let mut settings = load_in(&locked.directory).settings;
    update(&mut settings)?;
    persist(&locked, &settings)
}
/// Update just one entry under the lock, retaining other providers and prices.
pub fn set_provider_settings(id: ProviderId, settings: ProviderSettings) -> Result<()> {
    update_provider_in(&dir(), id, settings, false)
}
pub fn select(id: ProviderId, settings: ProviderSettings) -> Result<()> {
    select_in(&dir(), id, settings)
}
fn select_in(directory: &Path, id: ProviderId, settings: ProviderSettings) -> Result<()> {
    update_provider_in(directory, id, settings, true)
}
fn update_provider_in(
    directory: &Path,
    id: ProviderId,
    mut settings: ProviderSettings,
    select: bool,
) -> Result<()> {
    validate_provider(id, &settings)?;
    update_in(directory, |store| {
        // Credential-store operations own this hint. A delayed form save
        // must not restore a stale hint after a key was saved or removed.
        settings.saved_key = store
            .providers
            .get(&id)
            .is_some_and(|entry| entry.saved_key);
        store.set_provider_settings(id, settings)?;
        if select {
            store.provider = Some(id);
        }
        Ok(())
    })
}
pub fn set_saved_key(id: ProviderId, present: bool) -> Result<()> {
    update_in(&dir(), |store| {
        store.providers.entry(id).or_default().saved_key = present;
        Ok(())
    })
}

/// A nonsecret schema of the persisted file. No auth value is a settings field.
pub fn schema_contract() -> Value {
    json!({"path":"<state_dir>/assistant.json","version":VERSION,"max_bytes":MAX_BYTES,"mode":"0600","atomic":true,"prices_unit":"USD per million tokens","shape":{"version":1,"provider":"provider id or null","providers":{"<id>":{"model":null,"base_url":null,"account_id":null,"gateway_id":null,"region":null,"profile":null,"project":null,"location":null,"endpoint":null,"effort":null,"vision":null,"saved_key":false}},"autonomy":{"limits":Limits::default(),"see_images":false,"mode":"plan"},"prices":{}}})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn directory() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "cfa-assistant-settings-{}-{}",
            std::process::id(),
            NEXT_WRITE.fetch_add(1, Ordering::Relaxed)
        ));
        crate::ipc::ensure_private_dir(&p).unwrap();
        p
    }
    fn put(path: &Path, value: &Value) {
        let mut file = private_options().create_new(true).open(path).unwrap();
        file.write_all(&serde_json::to_vec(value).unwrap()).unwrap();
    }
    #[test]
    fn selecting_preserves_the_latest_key_hint_and_other_providers() {
        let root = directory();
        let mut store = Settings::default();
        store.providers.insert(
            ProviderId::Anthropic,
            ProviderSettings {
                saved_key: true,
                ..ProviderSettings::default()
            },
        );
        store.providers.insert(
            ProviderId::Gemini,
            ProviderSettings {
                model: Some("gemini-custom".into()),
                ..ProviderSettings::default()
            },
        );
        save_in(&root, &store).unwrap();
        select_in(
            &root,
            ProviderId::Anthropic,
            ProviderSettings {
                model: Some("claude-custom".into()),
                ..ProviderSettings::default()
            },
        )
        .unwrap();
        let loaded = load_in(&root).settings;
        assert!(loaded.provider_settings(ProviderId::Anthropic).saved_key);
        assert_eq!(
            loaded
                .provider_settings(ProviderId::Gemini)
                .model
                .as_deref(),
            Some("gemini-custom")
        );
        update_in(&root, |store| {
            store
                .providers
                .get_mut(&ProviderId::Anthropic)
                .unwrap()
                .saved_key = false;
            Ok(())
        })
        .unwrap();
        select_in(
            &root,
            ProviderId::Anthropic,
            ProviderSettings {
                saved_key: true,
                ..ProviderSettings::default()
            },
        )
        .unwrap();
        assert!(
            !load_in(&root)
                .settings
                .provider_settings(ProviderId::Anthropic)
                .saved_key
        );
        update_provider_in(
            &root,
            ProviderId::Gemini,
            ProviderSettings {
                saved_key: true,
                ..ProviderSettings::default()
            },
            false,
        )
        .unwrap();
        let loaded = load_in(&root).settings;
        assert_eq!(loaded.provider, Some(ProviderId::Anthropic));
        assert!(!loaded.provider_settings(ProviderId::Gemini).saved_key);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn missing_or_invalid_settings_never_block_loading() {
        let root = directory();
        let missing = root.join("missing");
        assert!(load_in(&missing).warnings.is_empty());
        assert!(!missing.exists());
        for value in [
            json!(null),
            json!({"version":99}),
            json!({"version":1,"provider":"future","providers":42,"autonomy":{"mode":"autonomous"},"prices":42}),
        ] {
            let p = root.join("assistant.json");
            put(&p, &value);
            let loaded = load_in(&root);
            assert!(!loaded.warnings.is_empty());
            fs::remove_file(p).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn provider_entries_and_prices_load_independently() {
        let loaded = decode(
            json!({"version":1,"provider":"openai","providers":{"openai":{"model":"gpt-6.1-sol","saved_key":true},"anthropic":{"model":"../bad"},"future":{"api_key":"not-a-real-secret"}},"prices":{"openai/gpt-6.1-sol":{"input":1.0,"output":2.0,"cache_read":0.5,"cache_write":1.2},"bad":{"input":-1}}}),
        );
        assert_eq!(loaded.settings.provider, Some(ProviderId::OpenAi));
        assert_eq!(loaded.settings.providers.len(), 1);
        assert_eq!(loaded.settings.prices.len(), 1);
        assert_eq!(loaded.warnings.len(), 3);
        assert!(!format!("{loaded:?}").contains("not-a-real-secret"));
    }
    #[test]
    fn saves_are_private_bounded_and_atomic_and_keep_other_entries() {
        let root = directory();
        let mut settings = Settings::default();
        settings
            .set_provider_settings(ProviderId::OpenAi, ProviderSettings::default())
            .unwrap();
        save_in(&root, &settings).unwrap();
        update_in(&root, |store| {
            store
                .providers
                .entry(ProviderId::Anthropic)
                .or_default()
                .saved_key = true;
            store.provider = Some(ProviderId::Anthropic);
            Ok(())
        })
        .unwrap();
        let loaded = load_in(&root);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.settings.providers.len(), 2);
        assert_eq!(loaded.settings.provider, Some(ProviderId::Anthropic));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(root.join("assistant.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(
            fs::read_dir(&root).unwrap().all(|p| !p
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp"))
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn insecure_and_symlink_settings_are_not_read_or_overwritten() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = directory();
        let target = root.join("target");
        put(&target, &json!({"version":1,"provider":"openai"}));
        let path = root.join("assistant.json");
        symlink(&target, &path).unwrap();
        assert!(!load_in(&root).warnings.is_empty());
        assert!(save_in(&root, &Settings::default()).is_err());
        fs::remove_file(&path).unwrap();
        put(&path, &json!({"version":1}));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!load_in(&root).warnings.is_empty());
        assert!(save_in(&root, &Settings::default()).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn oversized_files_and_bad_limits_are_ignored_without_secret_diagnostics() {
        let root = directory();
        let mut file = private_options()
            .create_new(true)
            .open(root.join("assistant.json"))
            .unwrap();
        file.write_all(&vec![b'X'; MAX_BYTES as usize + 1]).unwrap();
        assert!(!load_in(&root).warnings.is_empty());
        let loaded = decode(json!({"version":1,"autonomy":{"limits":{"max_rounds":1001}}}));
        assert!(!loaded.warnings.is_empty());
        assert_eq!(loaded.settings.autonomy.limits.max_rounds, 40);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn concurrent_entry_updates_keep_every_provider() {
        let root = directory();
        let threads = ProviderId::ALL
            .into_iter()
            .map(|id| {
                let root = root.clone();
                thread::spawn(move || {
                    update_in(&root, |store| {
                        store.providers.entry(id).or_default().saved_key = true;
                        Ok(())
                    })
                    .unwrap()
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap();
        }
        let loaded = load_in(&root);
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.settings.providers.len(), 22);
        assert!(
            loaded
                .settings
                .providers
                .values()
                .all(|entry| entry.saved_key)
        );
        fs::remove_dir_all(root).unwrap();
    }
}
