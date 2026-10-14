//! Cached, read-only view of the policy merged by the storage backend.
use crate::storage::StoragePolicy;
use iced::{Task, futures::channel::oneshot};
use std::{
    future::Future,
    time::{Duration, Instant},
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(5);
pub(super) type Reply = Result<Policy, String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OnFull {
    Stop,
    DeleteOldest,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Policy {
    pub max_bytes: u64,
    pub max_files: u32,
    pub max_age_seconds: Option<u64>,
    pub on_full: OnFull,
}

impl TryFrom<StoragePolicy> for Policy {
    type Error = String;
    fn try_from(policy: StoragePolicy) -> Reply {
        policy.validate().map_err(|error| format!("{error:#}"))?;
        Ok(Self {
            max_bytes: policy.max_bytes,
            max_files: policy.max_files,
            max_age_seconds: policy.max_age_seconds,
            on_full: if policy.on_full == "delete-oldest" {
                OnFull::DeleteOldest
            } else {
                OnFull::Stop
            },
        })
    }
}

impl Policy {
    /// Mirrors StoragePolicy::constrained_by without changing backend policy.
    /// A configured delete-oldest action takes precedence over a GUI Stop.
    fn effective(&self, requested: &StoragePolicy) -> Self {
        Self {
            max_bytes: self.max_bytes.min(requested.max_bytes),
            max_files: self.max_files.min(requested.max_files),
            max_age_seconds: match (requested.max_age_seconds, self.max_age_seconds) {
                (Some(requested), Some(global)) => Some(requested.min(global)),
                (requested, global) => requested.or(global),
            },
            on_full: if self.on_full == OnFull::DeleteOldest || requested.on_full == "delete-oldest"
            {
                OnFull::DeleteOldest
            } else {
                OnFull::Stop
            },
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) enum State {
    #[default]
    Loading,
    Available(Policy),
    Unavailable(String),
}

#[derive(Default)]
pub(super) struct Cache {
    state: State,
    pending: bool,
    checked_at: Option<Instant>,
}

impl Cache {
    pub fn state(&self) -> &State {
        &self.state
    }

    /// At most one filesystem/lock read is in flight; refreshes are bounded
    /// even when Tick runs once per frame. Work always runs on a worker thread.
    pub fn refresh(&mut self, now: Instant) -> Task<Reply> {
        if !self.begin(now) {
            return Task::none();
        }
        Task::perform(
            read_on_worker(|| {
                crate::storage::configured_policy()
                    .map_err(|error| format!("{error:#}"))
                    .and_then(Policy::try_from)
            }),
            std::convert::identity,
        )
    }

    fn begin(&mut self, now: Instant) -> bool {
        if self.pending
            || self
                .checked_at
                .is_some_and(|checked| now.saturating_duration_since(checked) < REFRESH_INTERVAL)
        {
            return false;
        }
        self.pending = true;
        true
    }

    pub fn update(&mut self, reply: Reply, now: Instant) {
        self.pending = false;
        self.checked_at = Some(now);
        // Failed reads must not present an old policy as the current one.
        self.state = match reply {
            Ok(policy) => State::Available(policy),
            Err(error) => State::Unavailable(error),
        };
    }
}

fn read_on_worker(
    work: impl FnOnce() -> Reply + Send + 'static,
) -> impl Future<Output = Reply> + Send + 'static {
    let (finished, result) = oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("capturefab-storage-policy".into())
        .spawn(move || {
            let _ = finished.send(work());
        })
        .map_err(|error| format!("Could not read app-wide storage policy: {error}"));
    async move {
        spawned?;
        result
            .await
            .map_err(|_| "The app-wide storage policy reader stopped.".to_owned())?
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tone {
    Normal,
    Warning,
    Error,
}

pub(super) struct Caption {
    pub text: String,
    pub tone: Tone,
}

fn caption(text: impl Into<String>, tone: Tone) -> Caption {
    Caption {
        text: text.into(),
        tone,
    }
}

impl State {
    pub fn summary(&self, requested: &StoragePolicy) -> String {
        match self {
            Self::Loading => "Checking app-wide policy…".into(),
            Self::Unavailable(_) => "Policy unavailable".into(),
            Self::Available(global) => {
                let effective = global.effective(requested);
                format!(
                    "{} · {} files",
                    bytes(effective.max_bytes),
                    super::format::grouped(effective.max_files.into())
                )
            }
        }
    }

    pub fn captions(&self, requested: &StoragePolicy) -> Vec<Caption> {
        let global = match self {
            Self::Loading => {
                return vec![caption("Reading app-wide storage policy…", Tone::Normal)];
            }
            Self::Unavailable(error) => {
                return vec![caption(
                    format!(
                        "App-wide storage policy unavailable: {error}. Effective limits and deletion behavior cannot be confirmed."
                    ),
                    Tone::Error,
                )];
            }
            Self::Available(policy) => policy,
        };
        let effective = global.effective(requested);
        let global_limits = limits(global);
        let effective_limits = limits(&effective);
        let mut captions = if global_limits == effective_limits {
            vec![caption(
                format!("Effective app-wide limits: {effective_limits}."),
                Tone::Normal,
            )]
        } else {
            vec![
                caption(format!("App-wide limits: {global_limits}."), Tone::Normal),
                caption(
                    format!("Effective limits: {effective_limits}."),
                    Tone::Normal,
                ),
            ]
        };
        if global.on_full == OnFull::DeleteOldest && requested.on_full != "delete-oldest" {
            captions.push(caption(
                "App-wide Delete oldest overrides this panel’s Stop setting.",
                Tone::Warning,
            ));
        }
        captions.push(caption(match effective.on_full {
            OnFull::DeleteOldest => "When saving at capacity, permanently deletes the oldest Capturefab managed files across output locations.",
            OnFull::Stop => "At capacity, saving stops and existing files are preserved.",
        }, if effective.on_full == OnFull::DeleteOldest { Tone::Warning } else { Tone::Normal }));
        if let Some(age) = effective.max_age_seconds {
            captions.push(caption(match effective.on_full {
                OnFull::DeleteOldest => format!("When saving, permanently deletes Capturefab managed files that are at least {} old. Active recordings and files awaiting upload are protected.", age_label(age)),
                OnFull::Stop => format!("Saving stops when a Capturefab managed file is at least {} old.", age_label(age)),
            }, if effective.on_full == OnFull::DeleteOldest { Tone::Warning } else { Tone::Normal }));
        }
        captions
    }
}

fn limits(policy: &Policy) -> String {
    format!(
        "{} · {} files · {}",
        bytes(policy.max_bytes),
        super::format::grouped(policy.max_files.into()),
        policy.max_age_seconds.map_or_else(
            || "no age limit".into(),
            |age| format!("{} maximum age", age_label(age))
        )
    )
}

fn bytes(bytes: u64) -> String {
    for (unit, label) in [
        (1024 * 1024 * 1024, "GiB"),
        (1024 * 1024, "MiB"),
        (1024, "KiB"),
    ] {
        if bytes >= unit && bytes.is_multiple_of(unit) {
            return format!("{} {label}", super::format::grouped(bytes / unit));
        }
    }
    format!("{} bytes", super::format::grouped(bytes))
}

fn age_label(seconds: u64) -> String {
    for (unit, singular, plural) in [
        (86400, "day", "days"),
        (3600, "hour", "hours"),
        (60, "minute", "minutes"),
    ] {
        if seconds >= unit && seconds.is_multiple_of(unit) {
            return super::format::plural(seconds / unit, singular, plural);
        }
    }
    super::format::plural(seconds, "second", "seconds")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(bytes: u64, files: u32, age: Option<u64>, action: &str) -> StoragePolicy {
        StoragePolicy {
            max_bytes: bytes,
            max_files: files,
            max_age_seconds: age,
            on_full: action.into(),
        }
    }

    #[test]
    fn configured_ceilings_and_both_age_limits_constrain_the_gui_request() {
        let global = Policy::try_from(policy(1024, 20, Some(60), "stop")).unwrap();
        let effective = global.effective(&policy(2048, 5, Some(120), "stop"));
        assert_eq!(
            (
                effective.max_bytes,
                effective.max_files,
                effective.max_age_seconds
            ),
            (1024, 5, Some(60))
        );
        for (requested, configured, expected) in [
            (None, None, None),
            (None, Some(60), Some(60)),
            (Some(120), None, Some(120)),
            (Some(30), Some(60), Some(30)),
        ] {
            let global = Policy::try_from(policy(1024, 20, configured, "stop")).unwrap();
            assert_eq!(
                global
                    .effective(&policy(2048, 5, requested, "stop"))
                    .max_age_seconds,
                expected
            );
        }
    }

    #[test]
    fn actual_global_deletion_overrides_stop_and_is_visible_even_without_gui_retention() {
        let requested = policy(2048, 50, None, "stop");
        let global = Policy::try_from(policy(1024, 20, Some(60), "delete-oldest")).unwrap();
        let state = State::Available(global);
        assert_eq!(state.summary(&requested), "1 KiB · 20 files");
        let captions = state.captions(&requested);
        assert!(captions.iter().any(
            |caption| caption.text.contains("overrides this panel’s Stop")
                && caption.tone == Tone::Warning
        ));
        assert!(
            captions
                .iter()
                .any(|caption| caption.text.contains("permanently deletes")
                    && caption.text.contains("1 minute"))
        );
        assert!(
            !captions
                .iter()
                .any(|caption| caption.text.contains("Saving stops")
                    || caption.text.contains("saving stops"))
        );
        let global = Policy::try_from(policy(1024, 20, None, "stop")).unwrap();
        assert_eq!(
            global
                .effective(&policy(1024, 20, None, "delete-oldest"))
                .on_full,
            OnFull::DeleteOldest
        );
    }

    #[test]
    fn unavailable_state_does_not_promise_preservation_or_display_stale_limits() {
        let mut cache = Cache::default();
        let now = Instant::now();
        cache.update(Ok(Policy::try_from(StoragePolicy::default()).unwrap()), now);
        cache.update(Err("storage manager is busy".into()), now);
        assert_eq!(
            cache.state().summary(&StoragePolicy::default()),
            "Policy unavailable"
        );
        let captions = cache.state().captions(&StoragePolicy::default());
        assert_eq!(captions.len(), 1);
        assert_eq!(captions[0].tone, Tone::Error);
        assert!(captions[0].text.contains("storage manager is busy"));
        assert!(!captions[0].text.contains("10 GiB"));
        assert!(!captions[0].text.contains("preserved"));
    }

    #[test]
    fn refreshes_are_single_flight_and_retry_after_errors_without_tick_flooding() {
        let mut cache = Cache::default();
        let now = Instant::now();
        assert!(cache.begin(now));
        assert!(!cache.begin(now + Duration::from_secs(60)));
        cache.update(Err("busy".into()), now + Duration::from_secs(60));
        assert!(!cache.begin(now + Duration::from_secs(64)));
        assert!(cache.begin(now + Duration::from_secs(65)));
    }

    #[test]
    fn slow_reads_run_on_a_worker_and_the_result_waits_for_completion() {
        let calling = std::thread::current().id();
        let (entered, started) = std::sync::mpsc::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let mut result = Box::pin(read_on_worker(move || {
            assert_ne!(std::thread::current().id(), calling);
            entered.send(()).unwrap();
            gate.recv_timeout(Duration::from_secs(2)).unwrap();
            Policy::try_from(StoragePolicy::default())
        }));
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        use std::task::{Context, Poll};
        let mut context = Context::from_waker(iced::futures::task::noop_waker_ref());
        assert!(matches!(result.as_mut().poll(&mut context), Poll::Pending));
        release.send(()).unwrap();
        assert!(iced::futures::executor::block_on(result).is_ok());
    }
}
