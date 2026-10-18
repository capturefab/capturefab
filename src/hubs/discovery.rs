//! Cameras on saved hubs for `discover`, which runs only in the session
//! coordinator (GUI, `serve`, or the CLI's own session), never in a camera
//! worker. Each hub is listed on a detached thread; results are cached, so a
//! hub slower than the caller's discovery deadline still shows the cameras it
//! listed recently, and a hub is asked at most every 10 seconds. A hub whose
//! secret was refused is skipped, without sending it again.
use super::{
    Hub, Selector, Store, Stream,
    client::{self, Listed},
    scrub, secret,
};
use crate::types::{CameraInfo, Transport};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Condvar, LazyLock, Mutex, PoisonError},
    time::{Duration, Instant},
};

/// Listings are refreshed at most this often.
const REFRESH: Duration = Duration::from_secs(10);
/// A listing older than this is not shown.
const KEEP: Duration = Duration::from_secs(600);

#[derive(Default)]
struct Entry {
    digest: [u8; 32],
    in_flight: bool,
    started: Option<Instant>,
    /// When the last listing finished, and what it found.
    result: Option<(Instant, Result<Vec<Listed>, String>)>,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<(PathBuf, String), Entry>,
}

static CACHE: LazyLock<(Mutex<Cache>, Condvar)> = LazyLock::new(Default::default);

fn info(hub: &Hub, camera: &Listed) -> CameraInfo {
    let selector = Selector {
        hub: hub.name.clone(),
        camera: camera.camera.clone(),
        stream: Stream::Auto,
        fps: None,
    };
    CameraInfo {
        id: selector.id(),
        transport: Transport::Media,
        vendor: hub.kind.label().into(),
        model: client::display(&camera.name, 128),
        serial: selector.serial(),
        address: Some(selector.id()),
        profile: Default::default(),
    }
}

fn list(store: &Store, hub: &Hub) -> Result<Vec<Listed>, String> {
    let secret = secret::resolve(hub).map_err(|error| format!("{error:#}"))?;
    client::cameras(hub, secret.as_ref(), Duration::ZERO).map_err(|failure| {
        if let Some(status) = failure.auth_status {
            let _ = store.mark_rejected(&hub.name, &hub.digest(), status);
        }
        failure.message
    })
}

/// Cameras on saved hubs and warnings, within `timeout`.
pub(crate) fn discovered(timeout: Duration) -> (Vec<CameraInfo>, Vec<String>) {
    discovered_in(&Store::session(), timeout)
}

pub(crate) fn discovered_in(store: &Store, timeout: Duration) -> (Vec<CameraInfo>, Vec<String>) {
    let hubs = match store.list() {
        Ok(hubs) => hubs,
        Err(_) => return (Vec::new(), vec!["saved hubs: cannot read hubs.json".into()]),
    };
    let started = Instant::now();
    let deadline = started + timeout;
    let (lock, changed) = &*CACHE;
    let mut warnings = Vec::new();
    let mut waiting = Vec::new();
    {
        let mut cache = lock.lock().unwrap_or_else(PoisonError::into_inner);
        for hub in &hubs {
            if let Some(rejected) = &hub.rejected {
                warnings.push(format!(
                    "{} hub {}: the secret was refused (HTTP {}); skipped by discovery until `capturefab hub test {}` succeeds",
                    hub.kind.label(),
                    hub.name,
                    rejected.status,
                    hub.name
                ));
                continue;
            }
            let key = (store.dir().to_path_buf(), hub.name.clone());
            let entry = cache.entries.entry(key.clone()).or_default();
            if entry.digest != hub.digest() {
                *entry = Entry {
                    digest: hub.digest(),
                    ..Entry::default()
                };
            }
            if !entry.in_flight && entry.started.is_none_or(|at| at.elapsed() >= REFRESH) {
                entry.in_flight = true;
                entry.started = Some(Instant::now());
                let (hub, store) = (hub.clone(), store.clone());
                let spawned = std::thread::Builder::new()
                    .name("capturefab-hub-listing".into())
                    .spawn(move || {
                        let result = list(&store, &hub);
                        let (lock, changed) = &*CACHE;
                        let mut cache = lock.lock().unwrap_or_else(PoisonError::into_inner);
                        if let Some(entry) = cache.entries.get_mut(&key)
                            && entry.digest == hub.digest()
                        {
                            entry.in_flight = false;
                            entry.result = Some((Instant::now(), result));
                        }
                        changed.notify_all();
                    });
                if spawned.is_err() {
                    entry.in_flight = false;
                }
            }
            if entry.in_flight {
                waiting.push(hub.name.clone());
            }
        }
        // Wait for listings in flight, never past the caller's deadline.
        loop {
            let busy = hubs.iter().any(|hub| {
                cache
                    .entries
                    .get(&(store.dir().to_path_buf(), hub.name.clone()))
                    .is_some_and(|entry| entry.in_flight)
            });
            let left = deadline.saturating_duration_since(Instant::now());
            if !busy || left.is_zero() {
                break;
            }
            cache = changed
                .wait_timeout(cache, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        let mut devices = Vec::new();
        for hub in hubs.iter().filter(|hub| hub.rejected.is_none()) {
            let Some(entry) = cache
                .entries
                .get(&(store.dir().to_path_buf(), hub.name.clone()))
            else {
                continue;
            };
            let label = format!("{} hub {}", hub.kind.label(), hub.name);
            match &entry.result {
                Some((at, result)) if at.elapsed() <= KEEP => {
                    let fresh = *at >= started;
                    if !fresh && entry.in_flight {
                        warnings.push(format!(
                            "hub {} did not answer within {} ms; showing cameras listed {} s ago",
                            hub.name,
                            timeout.as_millis(),
                            at.elapsed().as_secs()
                        ));
                    }
                    match result {
                        Ok(cameras) => {
                            devices.extend(cameras.iter().map(|camera| info(hub, camera)))
                        }
                        Err(message) => warnings.push(format!("{label}: {message}")),
                    }
                }
                _ if entry.in_flight => warnings.push(format!(
                    "hub {} did not answer within {} ms",
                    hub.name,
                    timeout.as_millis()
                )),
                _ => {}
            }
        }
        devices.sort_by(|a, b| a.id.cmp(&b.id));
        let warnings = warnings
            .into_iter()
            .map(|warning| scrub::scrub(&warning).into_owned())
            .collect();
        (devices, warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hubs::{Kind, SecretSource, fake_hub::HomeAssistant, image::tests::jpeg_of};

    fn store(name: &str) -> Store {
        Store::at(std::env::temp_dir().join(format!(
            "capturefab-hub-discovery-{name}-{}-{}",
            std::process::id(),
            crate::scheduling::now_ms()
        )))
    }
    fn save(store: &Store, url: &str, variable: &str) -> Hub {
        let hub = Hub {
            name: "home".into(),
            kind: Kind::HomeAssistant,
            url: url.into(),
            user: None,
            secret: SecretSource::Environment,
            secret_env: Some(variable.into()),
            certificate: None,
            tls_server_name: None,
            rtsp_port: None,
            rejected: None,
        };
        store.save_with(&hub, |_| Ok(()), |_| Ok(())).unwrap();
        hub
    }

    #[test]
    fn hub_cameras_are_listed_cached_and_rate_limited() {
        // SAFETY: the variable is unique to this test.
        unsafe { std::env::set_var("CAPTUREFAB_HUB_DISCOVERY_TOKEN", "CANARY-discovery-token") };
        let fake = HomeAssistant::new("CANARY-discovery-token", vec![jpeg_of(8, 8, 1)]).start();
        let store = store("cache");
        save(&store, &fake.url(), "CAPTUREFAB_HUB_DISCOVERY_TOKEN");
        let (devices, warnings) = discovered_in(&store, Duration::from_secs(5));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            devices.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            ["hub:home/camera.front_door", "hub:home/camera.garage"]
        );
        assert_eq!(devices[0].vendor, "Home Assistant");
        assert_eq!(devices[0].serial, "home/camera.front_door");
        let requests = fake.requests().len();
        let (again, _) = discovered_in(&store, Duration::from_millis(100));
        assert_eq!(again.len(), 2, "cached cameras are shown again");
        assert_eq!(
            fake.requests().len(),
            requests,
            "a second discovery within 10 s asks nothing"
        );
    }

    #[test]
    fn slow_and_refused_hubs_become_warnings() {
        // A hub that never answers: discovery keeps the caller's deadline.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        unsafe { std::env::set_var("CAPTUREFAB_HUB_DISCOVERY_SLOW", "CANARY-slow-token") };
        let store = store("slow");
        save(&store, &url, "CAPTUREFAB_HUB_DISCOVERY_SLOW");
        let start = Instant::now();
        let (devices, warnings) = discovered_in(&store, Duration::from_millis(300));
        assert!(start.elapsed() < Duration::from_millis(1000));
        assert!(devices.is_empty());
        assert_eq!(warnings, ["hub home did not answer within 300 ms"]);
        drop(listener);
        // A refused token is recorded once, then the hub is skipped.
        unsafe { std::env::set_var("CAPTUREFAB_HUB_DISCOVERY_BAD", "CANARY-wrong-token") };
        let fake = HomeAssistant::new("CANARY-right-token", vec![jpeg_of(8, 8, 1)]).start();
        let store = super::tests::store("refused");
        save(&store, &fake.url(), "CAPTUREFAB_HUB_DISCOVERY_BAD");
        let (_, warnings) = discovered_in(&store, Duration::from_secs(5));
        assert!(
            warnings[0].contains("rejected the access token (HTTP 401)"),
            "{warnings:?}"
        );
        assert_eq!(store.get("home").unwrap().rejected.unwrap().status, 401);
        let requests = fake.requests().len();
        let (_, warnings) = discovered_in(&store, Duration::from_secs(5));
        assert!(warnings[0].contains("skipped by discovery"), "{warnings:?}");
        assert_eq!(
            fake.requests().len(),
            requests,
            "no request after the refusal"
        );
    }
}
