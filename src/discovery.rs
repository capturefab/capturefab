//! The cameras discovery found lately, kept in `<runtime>/.discovery.json`
//! (a name no session's `NAME.json` can have) for a minute: a camera named
//! by its serial, name or model is looked up there before anything is
//! broadcast. An address may since belong to another camera, so what the
//! cache names is checked before it is opened. Best effort: a cache that
//! cannot be read or written is no cache.
use crate::types::{CameraInfo, Transport};
use serde::{Deserialize, Serialize};
use std::{io::Read, path::PathBuf, time::Duration};

/// How long a camera discovery found is trusted without looking again.
pub(crate) const TTL: Duration = Duration::from_secs(60);
/// The largest cache read; a larger file is ignored.
const MAX_FILE: u64 = 1024 * 1024;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    cameras: Vec<Entry>,
    /// When a search of every transport last ran to its end, in
    /// milliseconds since the epoch: the cameras are then all there were.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    everywhere_ms: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    /// When a discovery last found it, in milliseconds since the epoch.
    seen_ms: u64,
    info: CameraInfo,
}

fn path() -> PathBuf {
    crate::ipc::runtime_dir().join(".discovery.json")
}

fn read() -> Cache {
    let Ok(file) = std::fs::File::open(path()) else {
        return Cache::default();
    };
    let mut text = Vec::new();
    if file.take(MAX_FILE + 1).read_to_end(&mut text).is_err() || text.len() as u64 > MAX_FILE {
        return Cache::default();
    }
    serde_json::from_slice(&text).unwrap_or_default()
}

/// The cameras a discovery found within `TTL`, and whether a search of
/// every transport ran to its end within it: then they are every camera
/// there was, and any found since.
pub(crate) fn fresh() -> (Vec<CameraInfo>, bool) {
    fresh_at(read(), crate::scheduling::now_ms())
}
fn fresh_at(cache: Cache, now_ms: u64) -> (Vec<CameraInfo>, bool) {
    let ttl = u64::try_from(TTL.as_millis()).unwrap_or(u64::MAX);
    let recent = |seen_ms: u64| seen_ms <= now_ms && now_ms - seen_ms < ttl;
    let cameras = cache
        .cameras
        .into_iter()
        .filter(|entry| recent(entry.seen_ms))
        .map(|entry| entry.info)
        .collect();
    (cameras, cache.everywhere_ms.is_some_and(recent))
}

/// Note the cameras a discovery `found`. When it searched to the end,
/// `searched` says which cameras it looked for: those it no longer found
/// are forgotten; `everywhere`, that it searched every transport. Simulators
/// are never kept: their selectors say what they open.
pub(crate) fn store(
    found: &[CameraInfo],
    searched: Option<&dyn Fn(&CameraInfo) -> bool>,
    everywhere: bool,
) {
    let cache = merge(
        read(),
        found,
        searched,
        everywhere,
        crate::scheduling::now_ms(),
    );
    let _ = write(&cache);
}
fn merge(
    mut cache: Cache,
    found: &[CameraInfo],
    searched: Option<&dyn Fn(&CameraInfo) -> bool>,
    everywhere: bool,
    now_ms: u64,
) -> Cache {
    if let Some(searched) = searched {
        cache.cameras.retain(|entry| !searched(&entry.info));
    }
    if everywhere {
        cache.everywhere_ms = Some(now_ms);
    }
    for info in found
        .iter()
        .filter(|info| info.transport != Transport::Simulator)
    {
        cache.cameras.retain(|entry| entry.info.id != info.id);
        cache.cameras.push(Entry {
            seen_ms: now_ms,
            info: info.clone(),
        });
    }
    // Long gone cameras go.
    let stale = u64::try_from(TTL.as_millis() * 10).unwrap_or(u64::MAX);
    cache
        .cameras
        .retain(|entry| now_ms.saturating_sub(entry.seen_ms) < stale);
    cache.cameras.sort_by(|a, b| a.info.id.cmp(&b.info.id));
    cache
}

fn write(cache: &Cache) -> anyhow::Result<()> {
    let path = path();
    let dir = path.parent().ok_or_else(|| anyhow::anyhow!("no folder"))?;
    crate::ipc::ensure_private_dir(dir)?;
    let mut suffix = [0u8; 6];
    getrandom::fill(&mut suffix).map_err(|e| anyhow::anyhow!("random file name: {e}"))?;
    let suffix = suffix
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect::<String>();
    let temp = dir.join(format!(".discovery.{suffix}.tmp"));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options
        .open(&temp)
        .map_err(anyhow::Error::from)
        .and_then(|file| Ok(serde_json::to_writer(file, cache)?))
        .and_then(|()| Ok(std::fs::rename(&temp, &path)?));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera(id: &str, transport: Transport) -> CameraInfo {
        CameraInfo {
            id: id.into(),
            transport,
            vendor: String::new(),
            model: String::new(),
            serial: format!("SN-{id}"),
            address: None,
            profile: Default::default(),
        }
    }

    #[test]
    fn a_complete_search_forgets_what_it_no_longer_finds_and_keeps_the_rest() {
        let gige = |id: &str| camera(id, Transport::GigE);
        let webcam = camera("avfoundation:FaceTime HD Camera", Transport::Media);
        let cache = merge(
            Cache::default(),
            &[gige("gige:a"), gige("gige:b"), webcam.clone()],
            None,
            false,
            1_000,
        );
        assert_eq!(
            fresh_at(merge(Cache::default(), &[], None, false, 0), 0)
                .0
                .len(),
            0
        );
        // A GigE search found only one of them; the webcam was not looked for.
        let is_gige = |info: &CameraInfo| info.transport == Transport::GigE;
        let cache = merge(cache, &[gige("gige:b")], Some(&is_gige), false, 2_000);
        let (cameras, everywhere) = fresh_at(cache, 2_000);
        let ids = cameras.into_iter().map(|info| info.id).collect::<Vec<_>>();
        assert_eq!(ids, ["avfoundation:FaceTime HD Camera", "gige:b"]);
        assert!(!everywhere);
    }

    #[test]
    fn only_a_search_of_every_transport_lists_every_camera() {
        let found = [camera("gige:a", Transport::GigE)];
        let ttl = u64::try_from(TTL.as_millis()).unwrap();
        // A search that ended once its camera answered adds what it found.
        let early = merge(Cache::default(), &found, None, false, 1_000);
        assert!(!fresh_at(early, 1_000).1);
        let whole = merge(Cache::default(), &found, Some(&|_| true), true, 1_000);
        let text = serde_json::to_string(&whole).unwrap();
        let parsed = || serde_json::from_str::<Cache>(&text).unwrap();
        assert!(fresh_at(parsed(), 1_000 + ttl - 1).1);
        assert!(!fresh_at(parsed(), 1_000 + ttl).1);
        // A later search that ended early keeps the list whole.
        let added = merge(
            parsed(),
            &[camera("gige:b", Transport::GigE)],
            None,
            false,
            2_000,
        );
        let (cameras, everywhere) = fresh_at(added, 2_000);
        assert_eq!(cameras.len(), 2);
        assert!(everywhere);
    }

    #[test]
    fn entries_expire_and_simulators_are_never_kept() {
        let cache = merge(
            Cache::default(),
            &[
                camera("gige:a", Transport::GigE),
                camera("sim:0", Transport::Simulator),
            ],
            None,
            false,
            1_000,
        );
        let ttl = u64::try_from(TTL.as_millis()).unwrap();
        let text = serde_json::to_string(&cache).unwrap();
        let parsed = || serde_json::from_str::<Cache>(&text).unwrap();
        assert_eq!(fresh_at(parsed(), 1_000 + ttl - 1).0.len(), 1);
        assert!(fresh_at(parsed(), 1_000 + ttl).0.is_empty());
        // A clock that went back trusts nothing.
        assert!(fresh_at(parsed(), 500).0.is_empty());
    }

    #[test]
    fn no_session_name_is_the_cache() {
        let name = path().file_stem().unwrap().to_string_lossy().into_owned();
        assert!(crate::ipc::validate_name(&name).is_err(), "{name}");
    }
}
