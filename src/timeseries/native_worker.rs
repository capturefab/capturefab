//! Owned USB worker containment before native device access.
use anyhow::Result;
use std::time::Instant;

use crate::types::{Categorized, ErrorKind};
#[derive(Debug, Clone, Copy)]
pub(crate) enum WorkerMode {
    UsbTmc,
}
impl WorkerMode {
    pub(crate) const fn argument(self) -> &'static str {
        match self {
            Self::UsbTmc => "__usbtmc-worker",
        }
    }
}

pub(crate) fn deadline(until: Instant) -> Result<()> {
    if Instant::now() >= until {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "USB worker operation deadline expired",
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub(crate) fn unavailable(message: &'static str) -> anyhow::Error {
    Categorized::error(ErrorKind::Unavailable, message)
}

#[cfg(not(windows))]
fn unconfirmed() -> anyhow::Error {
    Categorized::new(ErrorKind::Unavailable, "USB worker cleanup is unconfirmed")
        .details(serde_json::json!({"cleanup":"unconfirmed"}))
        .into()
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub(crate) fn executable() -> Result<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("CAPTUREFAB_EXECUTABLE") {
        return Ok(path.into());
    }
    if cfg!(target_os = "linux") {
        return Ok("/proc/self/exe".into());
    }
    Ok(std::env::current_exe()?)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix {
    use super::{WorkerMode, deadline, executable, unavailable, unconfirmed};
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
            #[cfg(target_os = "linux")]
            let reported_pid = unsafe { information.si_pid() };
            #[cfg(target_os = "macos")]
            let reported_pid = information.si_pid;
            if reported_pid == 0 {
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
                    .name("usbtmc-reap".into())
                    .spawn(move || {
                        let _ = child.wait();
                    });
            }
        }
    }

    pub(crate) struct Process {
        owner: Arc<Owned>,
    }

    impl Process {
        pub(crate) fn spawn(
            mode: WorkerMode,
            until: Instant,
        ) -> Result<(Self, io::PipeReader, io::PipeWriter)> {
            deadline(until)?;
            let (child_input, input) = io::pipe()?;
            let (output, child_output) = io::pipe()?;
            let mut command = Command::new(executable()?);
            command
                .arg(mode.argument())
                .stdin(child_input)
                .stdout(child_output)
                .stderr(Stdio::null())
                .process_group(0);
            let child = command
                .spawn()
                .map_err(|_| unavailable("cannot start USB worker"))?;
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

        pub(crate) fn terminate(&self) {
            self.owner.terminate();
        }

        pub(crate) fn finish(&self, until: Instant) -> Result<()> {
            loop {
                if let Some(success) = self.owner.exited()? {
                    if self.owner.forced.load(Ordering::Acquire) {
                        return Err(unconfirmed());
                    }
                    if !success {
                        return Err(Categorized::error(
                            ErrorKind::WorkerCrashed,
                            "USB worker exited unsuccessfully",
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use unix::Process;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::Process;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub(crate) struct Process {
    terminated: std::sync::atomic::AtomicBool,
}
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
impl Process {
    pub(crate) fn spawn(
        _mode: WorkerMode,
        _until: Instant,
    ) -> Result<(Self, std::io::PipeReader, std::io::PipeWriter)> {
        Err(Categorized::error(
            ErrorKind::Unsupported,
            "USB workers require Linux, macOS or Windows",
        ))
    }
    pub(crate) fn terminate(&self) {
        self.terminated
            .store(true, std::sync::atomic::Ordering::Release);
    }
    pub(crate) fn finish(&self, _until: Instant) -> Result<()> {
        if self.terminated.load(std::sync::atomic::Ordering::Acquire) {
            Err(unconfirmed())
        } else {
            Ok(())
        }
    }
}
