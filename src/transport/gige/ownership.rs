//! Local controller continuity, separate from frame timeline clocks.
use crate::types::{Categorized, ErrorKind};
use anyhow::{Result, anyhow};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Clone, Debug)]
struct Loss {
    reason: &'static str,
    message: String,
}
impl Loss {
    fn error(&self) -> anyhow::Error {
        Retired(
            Categorized::new(ErrorKind::Disconnected, self.message.clone())
                .hint(
                    "disconnect this camera before explicitly opening a new controller connection",
                )
                .details(serde_json::json!({
                    "controller_ownership": "retired", "reason": self.reason
                })),
        )
        .into()
    }
}
#[derive(Debug)]
struct Retired(Categorized);
impl std::fmt::Display for Retired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}
impl std::error::Error for Retired {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}
pub(super) fn retired(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.is::<Retired>()
            || cause.downcast_ref::<Categorized>().is_some_and(|error| {
                error.kind == ErrorKind::Disconnected
                    && error
                        .details
                        .as_ref()
                        .is_some_and(|details| details["controller_ownership"] == "retired")
            })
    })
}

#[derive(Clone, Copy, Debug)]
struct Finite {
    timeout_ms: u32,
    reset: u64,
    deadline: u64,
}
impl Finite {
    fn new(reset: u64, timeout_ms: u32) -> Result<Self> {
        let timeout = u64::from(timeout_ms)
            .checked_mul(1_000_000)
            .filter(|_| timeout_ms != 0)
            .ok_or_else(|| anyhow!("invalid heartbeat timeout"))?;
        Ok(Self {
            timeout_ms,
            reset,
            deadline: reset
                .checked_add(timeout)
                .ok_or_else(|| anyhow!("heartbeat lease clock overflow"))?,
        })
    }
    fn narrow(self, timeout_ms: u32) -> Option<Self> {
        let next = Self::new(self.reset, timeout_ms).ok()?;
        Some(Self {
            deadline: self.deadline.min(next.deadline),
            ..next
        })
    }
}
#[derive(Debug)]
enum Phase {
    Claiming {
        finite: Finite,
        disabled: bool,
    },
    Held(Finite),
    Disabled {
        timeout_ms: Option<u32>,
        shadow: Option<Finite>,
    },
    Enabling(Finite),
    Released,
    Retired(Loss),
}
#[derive(Debug)]
struct State {
    last_clock: u64,
    last_probe: u64,
    phase: Phase,
}
#[derive(Default)]
pub(super) struct Wake {
    pub(super) stop: bool,
    pub(super) generation: u64,
}
pub(super) struct Lease {
    state: Mutex<State>,
    pub(super) wake: Arc<(Mutex<Wake>, Condvar)>,
    clock_source: fn() -> Option<u64>,
    #[cfg(test)]
    clock: Option<Arc<std::sync::atomic::AtomicU64>>,
}
impl Lease {
    pub(super) fn claiming(timeout_ms: u32, disabled: bool) -> Result<Arc<Self>> {
        let now =
            clock().ok_or_else(|| anyhow!("suspend-inclusive controller clock is unavailable"))?;
        Ok(Arc::new(Self {
            state: Mutex::new(State {
                last_clock: now,
                last_probe: now,
                phase: Phase::Claiming {
                    finite: Finite::new(now, timeout_ms)?,
                    disabled,
                },
            }),
            wake: Arc::new((Mutex::new(Wake::default()), Condvar::new())),
            clock_source: clock,
            #[cfg(test)]
            clock: None,
        }))
    }
    fn now(&self, state: &mut State) -> Result<u64> {
        #[cfg(test)]
        let value = self.clock.as_ref().map_or_else(self.clock_source, |clock| {
            let value = clock.load(std::sync::atomic::Ordering::SeqCst);
            (value != u64::MAX).then_some(value)
        });
        #[cfg(not(test))]
        let value = (self.clock_source)();
        if let Some(value) = value.filter(|value| *value >= state.last_clock) {
            state.last_clock = value;
            Ok(value)
        } else {
            let loss = Loss { reason: "lease_clock", message: "camera controller retired because its suspend-inclusive lease clock could not be verified".into() };
            let error = loss.error();
            state.phase = Phase::Retired(loss);
            Err(error)
        }
    }
    fn checked(&self, state: &mut State) -> Result<u64> {
        if let Phase::Retired(loss) = &state.phase {
            return Err(loss.error());
        }
        if matches!(state.phase, Phase::Released) {
            return Err(Loss { reason: "released", message: "camera controller privilege was released; this connection cannot acquire it again".into() }.error());
        }
        let now = self.now(state)?;
        let deadline = match state.phase {
            Phase::Claiming { finite, .. } | Phase::Held(finite) | Phase::Enabling(finite) => {
                Some(finite.deadline)
            }
            _ => None,
        };
        if deadline.is_some_and(|deadline| now >= deadline) {
            let loss = Loss {
                reason: "lease_expired",
                message:
                    "camera controller retired because its last verified heartbeat interval expired"
                        .into(),
            };
            let error = loss.error();
            state.phase = Phase::Retired(loss);
            return Err(error);
        }
        Ok(now)
    }
    pub(super) fn check(&self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.checked(&mut state).map(|_| ())
    }
    pub(super) fn remaining(&self) -> Result<Option<Duration>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = self.checked(&mut state)?;
        let finite = match state.phase {
            Phase::Claiming { finite, .. } | Phase::Held(finite) | Phase::Enabling(finite) => {
                Some(finite)
            }
            _ => None,
        };
        Ok(finite.map(|finite| Duration::from_nanos(finite.deadline - now)))
    }
    pub(super) fn needs_probe(&self, budget: Duration) -> Result<bool> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = self.checked(&mut state)?;
        match state.phase {
            Phase::Disabled { .. } => Ok(now.saturating_sub(state.last_probe) >= 1_000_000_000),
            Phase::Claiming { finite, .. } | Phase::Held(finite) | Phase::Enabling(finite) => {
                Ok(Duration::from_nanos(finite.deadline - now) <= budget)
            }
            _ => Ok(false),
        }
    }
    pub(super) fn finish_claim(&self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.checked(&mut state)?;
        if let Phase::Claiming { finite, disabled } = state.phase {
            state.phase = if disabled {
                Phase::Disabled {
                    timeout_ms: Some(finite.timeout_ms),
                    shadow: None,
                }
            } else {
                Phase::Held(finite)
            };
        }
        Ok(())
    }
    pub(super) fn renew(&self, first_send: u64) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = self.checked(&mut state)?;
        state.last_probe = now;
        if let Phase::Held(previous) = state.phase {
            let next = Finite::new(first_send, previous.timeout_ms)?;
            if first_send > now || next.deadline <= now {
                drop(state);
                return Err(self.retire(
                    "late_heartbeat",
                    "camera controller heartbeat completed outside its proved interval",
                ));
            }
            state.phase = Phase::Held(next);
        }
        Ok(())
    }
    pub(super) fn send_clock(&self) -> Result<u64> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.checked(&mut state)
    }
    pub(super) fn period(&self) -> Duration {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match state.phase {
            Phase::Held(finite) | Phase::Enabling(finite) | Phase::Claiming { finite, .. } => {
                (Duration::from_millis(u64::from(finite.timeout_ms)) / 3)
                    .min(Duration::from_secs(1))
            }
            _ => Duration::from_secs(1),
        }
    }
    pub(super) fn disabled(&self) -> bool {
        matches!(
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .phase,
            Phase::Disabled { .. } | Phase::Claiming { disabled: true, .. }
        )
    }
    pub(super) fn timeout_ms(&self) -> Option<u32> {
        match self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .phase
        {
            Phase::Held(finite) | Phase::Enabling(finite) | Phase::Claiming { finite, .. } => {
                Some(finite.timeout_ms)
            }
            Phase::Disabled { timeout_ms, .. } => timeout_ms,
            _ => None,
        }
    }
    pub(super) fn timer_changed(&self, timeout_ms: Option<u32>) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.checked(&mut state)?;
        if let Phase::Disabled { shadow, .. } = state.phase {
            state.phase = Phase::Disabled {
                timeout_ms,
                shadow: shadow.and_then(|shadow| timeout_ms.and_then(|ms| shadow.narrow(ms))),
            };
        }
        drop(state);
        self.notify();
        Ok(())
    }
    pub(super) fn enter_disabled(&self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.checked(&mut state)?;
        if let Phase::Held(finite) = state.phase {
            state.phase = Phase::Disabled {
                timeout_ms: Some(finite.timeout_ms),
                shadow: Some(finite),
            };
        }
        drop(state);
        self.notify();
        Ok(())
    }
    pub(super) fn begin_enabling(&self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = self.checked(&mut state)?;
        if let Phase::Disabled {
            timeout_ms: Some(ms),
            shadow: Some(shadow),
        } = state.phase
            && let Some(finite) = shadow.narrow(ms)
            && now < finite.deadline
        {
            state.phase = Phase::Enabling(finite);
            return Ok(());
        }
        Err(Categorized::error(
            ErrorKind::Unsupported,
            "re-enabling heartbeat requires an unexpired reset proved before disable; release this controller explicitly instead",
        ))
    }
    pub(super) fn finish_enabling(&self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.checked(&mut state)?;
        if let Phase::Enabling(finite) = state.phase {
            state.phase = Phase::Held(finite);
        }
        drop(state);
        self.notify();
        Ok(())
    }
    pub(super) fn release(&self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.checked(&mut state)?;
        state.phase = Phase::Released;
        drop(state);
        self.notify();
        Ok(())
    }
    pub(super) fn retire(&self, reason: &'static str, message: impl Into<String>) -> anyhow::Error {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Phase::Retired(loss) = &state.phase {
            return loss.error();
        }
        let loss = Loss {
            reason,
            message: message.into(),
        };
        let error = loss.error();
        state.phase = Phase::Retired(loss);
        drop(state);
        self.notify();
        error
    }
    pub(super) fn is_retired(&self) -> bool {
        self.check().is_err()
    }
    fn notify(&self) {
        let (lock, wake) = &*self.wake;
        let mut value = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        value.generation = value.generation.wrapping_add(1);
        drop(value);
        wake.notify_all();
    }
    #[cfg(test)]
    pub(super) fn claiming_for_test(
        timeout_ms: u32,
        disabled: bool,
        test_clock: Arc<std::sync::atomic::AtomicU64>,
    ) -> Arc<Self> {
        let now = test_clock.load(std::sync::atomic::Ordering::SeqCst);
        Arc::new(Self {
            state: Mutex::new(State {
                last_clock: now,
                last_probe: now,
                phase: Phase::Claiming {
                    finite: Finite::new(now, timeout_ms).unwrap(),
                    disabled,
                },
            }),
            wake: Arc::new((Mutex::new(Wake::default()), Condvar::new())),
            clock_source: clock,
            clock: Some(test_clock),
        })
    }
    #[cfg(test)]
    pub(super) fn testing(
        timeout_ms: u32,
        disabled: bool,
    ) -> (Arc<Self>, Arc<std::sync::atomic::AtomicU64>) {
        let clock = Arc::new(std::sync::atomic::AtomicU64::new(1_000_000_000));
        let lease = Self::claiming_for_test(timeout_ms, disabled, clock.clone());
        lease.finish_claim().unwrap();
        (lease, clock)
    }
}

fn clock() -> Option<u64> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        #[cfg(target_os = "linux")]
        const CLOCK: libc::clockid_t = libc::CLOCK_BOOTTIME;
        #[cfg(target_os = "macos")]
        const CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC_RAW;
        let mut value = std::mem::MaybeUninit::<libc::timespec>::uninit();
        // SAFETY: a valid writable timespec is provided for the complete call.
        if unsafe { libc::clock_gettime(CLOCK, value.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: the successful clock_gettime call initialized the structure.
        let value = unsafe { value.assume_init() };
        u64::try_from(value.tv_sec)
            .ok()?
            .checked_mul(1_000_000_000)?
            .checked_add(u64::try_from(value.tv_nsec).ok()?)
    }
    #[cfg(windows)]
    {
        crate::host::monotonic_ns()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    #[test]
    fn earliest_send_and_previous_deadline_are_both_required() {
        let (lease, now) = Lease::testing(1000, false);
        now.store(1_800_000_000, Ordering::SeqCst);
        lease.renew(1_100_000_000).unwrap();
        now.store(2_100_000_000, Ordering::SeqCst);
        let error = lease.check().unwrap_err();
        assert!(retired(&error));
        now.store(2_200_000_000, Ordering::SeqCst);
        assert_eq!(
            lease.renew(2_200_000_000).unwrap_err().to_string(),
            error.to_string()
        );
    }
    #[test]
    fn disabled_checks_cannot_extend_the_pre_disable_shadow() {
        let (lease, now) = Lease::testing(1000, false);
        lease.enter_disabled().unwrap();
        now.store(1_800_000_000, Ordering::SeqCst);
        lease.renew(1_800_000_000).unwrap();
        now.store(2_000_000_000, Ordering::SeqCst);
        lease.check().unwrap();
        assert_eq!(
            crate::types::classify(&lease.begin_enabling().unwrap_err()).kind,
            ErrorKind::Unsupported
        );
        lease.release().unwrap();
        assert!(retired(&lease.check().unwrap_err()));
    }
    #[test]
    fn immediate_reenable_keeps_the_old_shadow_until_normal_ccp_renewal() {
        let (lease, now) = Lease::testing(1000, false);
        lease.enter_disabled().unwrap();
        now.store(1_100_000_000, Ordering::SeqCst);
        lease.begin_enabling().unwrap();
        lease.finish_enabling().unwrap();
        lease.renew(1_100_000_000).unwrap();
        now.store(2_050_000_000, Ordering::SeqCst);
        lease.check().unwrap();
    }
    #[test]
    fn unknown_disabled_timeout_preserves_release_but_not_reenable() {
        let (lease, _) = Lease::testing(1000, false);
        lease.enter_disabled().unwrap();
        lease.timer_changed(None).unwrap();
        assert_eq!(
            crate::types::classify(&lease.begin_enabling().unwrap_err()).kind,
            ErrorKind::Unsupported
        );
        lease.release().unwrap();
    }
    #[test]
    fn clock_failure_backwards_or_sleep_gap_is_sticky() {
        for value in [u64::MAX, 1, 2_000_000_000] {
            let (lease, now) = Lease::testing(1000, false);
            now.store(value, Ordering::SeqCst);
            let error = lease.check().unwrap_err();
            now.store(1_001_000_000, Ordering::SeqCst);
            assert_eq!(lease.check().unwrap_err().to_string(), error.to_string());
        }
    }
    #[test]
    fn initially_disabled_and_expired_or_shortened_shadow_cannot_reenable() {
        let (initial, _) = Lease::testing(1000, true);
        assert_eq!(
            crate::types::classify(&initial.begin_enabling().unwrap_err()).kind,
            ErrorKind::Unsupported
        );
        initial.release().unwrap();
        let (lease, now) = Lease::testing(1000, false);
        lease.enter_disabled().unwrap();
        lease.timer_changed(Some(500)).unwrap();
        now.store(1_500_000_000, Ordering::SeqCst);
        assert_eq!(
            crate::types::classify(&lease.begin_enabling().unwrap_err()).kind,
            ErrorKind::Unsupported
        );
        lease.check().unwrap();
        lease.release().unwrap();
    }
    #[test]
    fn retired_tag_survives_the_worker_wire_without_matching_text() {
        let (lease, now) = Lease::testing(1, false);
        now.store(1_001_000_000, Ordering::SeqCst);
        let error = lease.check().unwrap_err();
        let wire = crate::session::protocol::wire_error(&error);
        assert!(retired(&crate::session::protocol::from_wire(&wire)));
        assert!(!retired(&anyhow!("camera controller retired")));
        assert!(!retired(&Categorized::error(
            ErrorKind::Disconnected,
            "a different disconnected source"
        )));
    }
}
