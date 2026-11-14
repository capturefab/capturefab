//! Owned native worker containment; no vendor code runs in the parent.
use anyhow::Result;
use std::time::Instant;

use crate::types::{Categorized, ErrorKind};

pub(super) fn deadline(until: Instant) -> Result<()> {
    if Instant::now() >= until {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "NI worker operation deadline expired",
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", windows))]
pub(super) fn unavailable(message: &'static str) -> anyhow::Error {
    Categorized::error(ErrorKind::Unavailable, message)
}

#[cfg(not(windows))]
fn unconfirmed() -> anyhow::Error {
    Categorized::new(ErrorKind::Unavailable, "NI worker cleanup is unconfirmed")
        .details(serde_json::json!({"cleanup":"unconfirmed"}))
        .into()
}

#[cfg(any(target_os = "linux", windows))]
pub(super) fn executable() -> Result<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("CAPTUREFAB_EXECUTABLE") {
        return Ok(path.into());
    }
    if cfg!(target_os = "linux") {
        return Ok("/proc/self/exe".into());
    }
    Ok(std::env::current_exe()?)
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{deadline, executable, unavailable, unconfirmed};
    use crate::types::{Categorized, ErrorKind};
    use anyhow::Result;
    use std::{
        io,
        os::unix::process::CommandExt as _,
        process::{Child, Command, Stdio},
        sync::{
            Arc, Mutex, PoisonError,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };

    struct Owned {
        child: Mutex<Option<Child>>,
        group: libc::pid_t,
        forced: AtomicBool,
    }

    impl Owned {
        fn terminate(&self) {
            let mut owner = self.child.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(child) = owner.as_mut() {
                self.forced.store(true, Ordering::Release);
                // SAFETY: child is still owned and unreaped under this lock;
                // its positive PID names the group created by process_group(0).
                let _ = unsafe { libc::killpg(self.group, libc::SIGKILL) };
                let _ = child.kill();
            }
        }

        fn exited(&self) -> Result<Option<bool>> {
            let mut owner = self.child.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(child) = owner.as_mut() else {
                return Ok(Some(true));
            };
            let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
            // SAFETY: this is our unreaped child and initialized output storage.
            // WNOWAIT retains PID ownership until the exact group is stopped.
            let status = unsafe {
                libc::waitid(
                    libc::P_PID,
                    u32::try_from(self.group)?,
                    information.as_mut_ptr(),
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if status != 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    return Ok(None);
                }
                return Err(error.into());
            }
            // SAFETY: successful waitid initialized the complete output value.
            let information = unsafe { information.assume_init() };
            // SAFETY: waitid supplies the child-status variant of siginfo_t.
            if unsafe { information.si_pid() } == 0 {
                return Ok(None);
            }
            // SAFETY: leader has exited but remains unreaped, so its group ID
            // cannot be reused. End any remaining descendants before reaping.
            let _ = unsafe { libc::killpg(self.group, libc::SIGKILL) };
            let status = child.wait()?;
            *owner = None;
            Ok(Some(status.success()))
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            let owner = self.child.get_mut().unwrap_or_else(PoisonError::into_inner);
            if let Some(mut child) = owner.take() {
                // SAFETY: this exact child has not been reaped or transferred.
                let _ = unsafe { libc::killpg(self.group, libc::SIGKILL) };
                let _ = child.kill();
                // A single final reaper can outlive an unconfirmed cleanup.
                // Neither OS termination latency nor thread completion is invented.
                let _ = std::thread::Builder::new()
                    .name("nidaqmx-reap".into())
                    .spawn(move || {
                        let _ = child.wait();
                    });
            }
        }
    }

    pub(in super::super) struct Process {
        owner: Arc<Owned>,
    }

    impl Process {
        pub(in super::super) fn spawn(
            until: Instant,
        ) -> Result<(Self, io::PipeReader, io::PipeWriter)> {
            deadline(until)?;
            let (child_input, input) = io::pipe()?;
            let (output, child_output) = io::pipe()?;
            let mut command = Command::new(executable()?);
            command
                .arg(super::super::WORKER_ARG)
                .stdin(child_input)
                .stdout(child_output)
                .stderr(Stdio::null())
                .process_group(0);
            let child = command
                .spawn()
                .map_err(|_| unavailable("cannot start NI worker"))?;
            drop(command);
            let group = libc::pid_t::try_from(child.id())?;
            let owner = Arc::new(Owned {
                child: Mutex::new(Some(child)),
                group,
                forced: AtomicBool::new(false),
            });
            let weak = Arc::downgrade(&owner);
            crate::cli::on_second_signal(move || {
                if let Some(owner) = weak.upgrade() {
                    owner.terminate();
                }
            });
            let process = Self { owner };
            if let Err(error) = deadline(until) {
                process.terminate();
                return Err(error);
            }
            Ok((process, output, input))
        }

        pub(in super::super) fn terminate(&self) {
            self.owner.terminate();
        }

        pub(in super::super) fn finish(&self, until: Instant) -> Result<()> {
            loop {
                if let Some(success) = self.owner.exited()? {
                    if self.owner.forced.load(Ordering::Acquire) {
                        return Err(unconfirmed());
                    }
                    if !success {
                        return Err(Categorized::error(
                            ErrorKind::WorkerCrashed,
                            "NI worker exited unsuccessfully",
                        ));
                    }
                    return Ok(());
                }
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    self.terminate();
                    return Err(unconfirmed());
                }
                std::thread::park_timeout(left.min(Duration::from_millis(2)));
            }
        }
    }

    impl Drop for Process {
        fn drop(&mut self) {
            self.terminate();
        }
    }
}

#[cfg(target_os = "linux")]
pub(super) use linux::Process;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(super) use windows::Process;

#[cfg(not(any(target_os = "linux", windows)))]
pub(super) struct Process {
    terminated: std::sync::atomic::AtomicBool,
}
#[cfg(not(any(target_os = "linux", windows)))]
impl Process {
    pub(super) fn spawn(
        _until: Instant,
    ) -> Result<(Self, std::io::PipeReader, std::io::PipeWriter)> {
        Err(Categorized::error(
            ErrorKind::Unsupported,
            "NI-DAQmx requires a supported Linux or Windows runtime",
        ))
    }
    pub(super) fn terminate(&self) {
        self.terminated
            .store(true, std::sync::atomic::Ordering::Release);
    }
    pub(super) fn finish(&self, _until: Instant) -> Result<()> {
        if self.terminated.load(std::sync::atomic::Ordering::Acquire) {
            Err(unconfirmed())
        } else {
            Ok(())
        }
    }
}
