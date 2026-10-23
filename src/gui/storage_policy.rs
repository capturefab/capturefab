//! Cached, read-only view of the policy merged by the storage backend, and of
//! how full it and the capture disk are.
use crate::storage::{FREE_SPACE_FLOOR, StoragePolicy};
use iced::{Task, futures::channel::oneshot};
use std::{
    future::Future,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// One refresh: the policy, and usage when the ledger has reported it.
#[derive(Clone, Debug)]
pub(super) struct Reply {
    pub policy: Result<Policy, String>,
    pub usage: Option<Usage>,
}
impl From<Result<Policy, String>> for Reply {
    fn from(policy: Result<Policy, String>) -> Self {
        Self {
            policy,
            usage: None,
        }
    }
}

/// The app-wide ledger's totals, and the free space where captures are saved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Usage {
    pub bytes: u64,
    pub files: u64,
    pub free: Option<u64>,
}

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
    fn try_from(policy: StoragePolicy) -> Result<Self, String> {
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
    usage: Option<Usage>,
    pending: bool,
    checked_at: Option<Instant>,
}

impl Cache {
    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn usage(&self) -> Option<Usage> {
        self.usage
    }

    /// At most one filesystem read is in flight; refreshes are bounded even
    /// when Tick runs once per frame. Work always runs on a worker thread, and
    /// reads small files the backend replaces whole, never the ledger itself.
    /// Free space is measured in `folder`, when captures are saved locally.
    pub fn refresh(&mut self, now: Instant, folder: Option<PathBuf>) -> Task<Reply> {
        if !self.begin(now) {
            return Task::none();
        }
        Task::perform(
            read_on_worker(move || Reply {
                policy: crate::storage::configured_policy()
                    .map_err(|error| format!("{error:#}"))
                    .and_then(Policy::try_from),
                usage: crate::storage::usage().ok().flatten().map(|usage| Usage {
                    bytes: usage.allocated_bytes,
                    files: usage.files,
                    free: folder.as_deref().and_then(crate::storage::free_bytes),
                }),
            }),
            |reply| reply.unwrap_or_else(|error| Reply::from(Err(error))),
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

    pub fn update(&mut self, reply: impl Into<Reply>, now: Instant) {
        let reply = reply.into();
        self.pending = false;
        self.checked_at = Some(now);
        // Failed reads must not present an old policy as the current one.
        self.state = match reply.policy {
            Ok(policy) => State::Available(policy),
            Err(error) => State::Unavailable(error),
        };
        self.usage = reply.usage;
    }
}

/// The fixed folder of an output template, where its captures' free space is
/// measured: the part before anything that fills in.
pub(super) fn folder(template: &str) -> PathBuf {
    let fixed: PathBuf = Path::new(template.trim())
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .take_while(|part| !part.as_os_str().to_string_lossy().contains('{'))
        .collect();
    if fixed.as_os_str().is_empty() {
        crate::storage::capture_root()
    } else {
        fixed
    }
}

fn read_on_worker<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> impl Future<Output = Result<T, String>> + Send + 'static {
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
            .map_err(|_| "The app-wide storage policy reader stopped.".to_owned())
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

pub(super) struct Meter {
    pub text: String,
    /// The text, with how full storage is, for assistive technology.
    pub label: String,
    pub fraction: f32,
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
                // Every effective limit, as the folded panel shows only this.
                let mut summary = format!(
                    "{} · {} files",
                    bytes(effective.max_bytes),
                    super::format::grouped(effective.max_files.into())
                );
                if let Some(age) = effective.max_age_seconds {
                    summary = format!("{summary} · {}", age_label(age));
                }
                summary
            }
        }
    }

    /// How full the effective limits are, counting the capture disk as full
    /// once only the free space every save must leave is left.
    pub fn meter(&self, requested: &StoragePolicy, usage: Option<Usage>) -> Option<Meter> {
        let (Self::Available(global), Some(usage)) = (self, usage) else {
            return None;
        };
        let effective = global.effective(requested);
        let capacity = usage.free.map_or(effective.max_bytes, |free| {
            effective.max_bytes.min(
                usage
                    .bytes
                    .saturating_add(free.saturating_sub(FREE_SPACE_FLOOR)),
            )
        });
        let fraction =
            share(usage.bytes, capacity).max(share(usage.files, effective.max_files.into()));
        let tone = if fraction >= 0.95 {
            Tone::Error
        } else if fraction >= 0.8 {
            Tone::Warning
        } else {
            Tone::Normal
        };
        let mut text = format!(
            "{} of {} · {} of {} files",
            rounded(usage.bytes),
            bytes(effective.max_bytes),
            super::format::grouped(usage.files),
            super::format::grouped(effective.max_files.into())
        );
        if let Some(free) = usage.free {
            // Once the disk is what limits saves, say what is left for them
            // and why that is less than its free space.
            let room = free.saturating_sub(FREE_SPACE_FLOOR);
            text = if usage.bytes.saturating_add(room) < effective.max_bytes {
                format!(
                    "{text} · {} left for captures on this disk (saves keep {} free)",
                    rounded(room),
                    bytes(FREE_SPACE_FLOOR)
                )
            } else {
                format!("{text} · {} free on this disk", rounded(free))
            };
        }
        Some(Meter {
            label: format!("Storage {:.0}% full: {text}", fraction * 100.0),
            text,
            fraction,
            tone,
        })
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
                OnFull::DeleteOldest => format!("When saving, permanently deletes Capturefab managed files that are at least {} old. Active recordings are protected.", age_label(age)),
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

/// To one decimal in the largest binary unit it fills, as "4.2 GiB".
fn rounded(bytes: u64) -> String {
    for (unit, label) in [
        (1u64 << 40, "TiB"),
        (1 << 30, "GiB"),
        (1 << 20, "MiB"),
        (1 << 10, "KiB"),
    ] {
        if bytes >= unit {
            return format!("{:.1} {label}", bytes as f64 / unit as f64);
        }
    }
    format!("{} bytes", super::format::grouped(bytes))
}

fn share(used: u64, limit: u64) -> f32 {
    if limit == 0 {
        1.0
    } else {
        (used as f64 / limit as f64).min(1.0) as f32
    }
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
    fn the_folded_summary_names_every_effective_limit() {
        // A stop at an age limit set app-wide is not otherwise visible when
        // the storage panel is folded.
        let global = Policy::try_from(policy(1024, 20, Some(7 * 86400), "stop")).unwrap();
        let state = State::Available(global);
        assert_eq!(
            state.summary(&policy(2048, 50, None, "stop")),
            "1 KiB · 20 files · 7 days"
        );
        let global = Policy::try_from(policy(1024, 20, None, "stop")).unwrap();
        assert_eq!(
            State::Available(global).summary(&policy(2048, 50, None, "stop")),
            "1 KiB · 20 files"
        );
    }

    #[test]
    fn actual_global_deletion_overrides_stop_and_is_visible_even_without_gui_retention() {
        let requested = policy(2048, 50, None, "stop");
        let global = Policy::try_from(policy(1024, 20, Some(60), "delete-oldest")).unwrap();
        let state = State::Available(global);
        assert_eq!(state.summary(&requested), "1 KiB · 20 files · 1 minute");
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
    fn the_meter_turns_amber_at_80_and_red_at_95_percent_counting_the_disk() {
        let state =
            State::Available(Policy::try_from(policy(10 << 30, 10_000, None, "stop")).unwrap());
        let requested = policy(20 << 30, 20_000, None, "stop");
        let meter = |bytes, files, free| {
            state
                .meter(&requested, Some(Usage { bytes, files, free }))
                .unwrap()
        };
        let half = meter(5 << 30, 3_210, None);
        assert_eq!(half.text, "5.0 GiB of 10 GiB · 3,210 of 10,000 files");
        assert_eq!(half.label, format!("Storage 50% full: {}", half.text));
        assert_eq!(half.tone, Tone::Normal);
        assert_eq!(meter(8 << 30, 0, None).tone, Tone::Warning);
        assert_eq!(meter(0, 9_600, None).tone, Tone::Error);
        // Far below the budget, but the disk keeps only 100 MiB above the
        // free space every save must leave.
        let disk = meter(1 << 30, 10, Some(FREE_SPACE_FLOOR + (100 << 20)));
        assert_eq!(disk.tone, Tone::Warning);
        assert!(
            disk.text
                .ends_with(" · 100.0 MiB left for captures on this disk (saves keep 512 MiB free)"),
            "{}",
            disk.text
        );
        // With room for the whole budget, the disk's free space is enough.
        let roomy = meter(1 << 30, 10, Some(20 << 30));
        assert!(
            roomy.text.ends_with(" · 20.0 GiB free on this disk"),
            "{}",
            roomy.text
        );
        assert!(state.meter(&requested, None).is_none());
        assert!(
            State::Loading
                .meter(&requested, Some(Usage::default()))
                .is_none()
        );
    }

    #[test]
    fn free_space_is_measured_in_the_fixed_part_of_the_output() {
        assert_eq!(
            folder("/data/{date}/{camera}-{seq}.png"),
            PathBuf::from("/data")
        );
        assert_eq!(folder("/data/run 1/shot.png"), PathBuf::from("/data/run 1"));
        assert_eq!(folder("{camera}.png"), crate::storage::capture_root());
        assert_eq!(folder(""), crate::storage::capture_root());
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
