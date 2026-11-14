//! Every native provider remains in a killable process tree.
use anyhow::Result;
use std::process::{Child, Command};

pub(super) fn prepare(command: &mut Command) {
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(command, 0x0800_0200);
    #[cfg(not(any(unix, windows)))]
    let _ = command;
}
#[cfg(unix)]
pub(super) struct Tree(i32);
#[cfg(unix)]
impl Tree {
    pub(super) fn adopt(child: &Child) -> Result<Self> {
        Ok(Self(i32::try_from(child.id())?))
    }
    pub(super) fn terminate(&self) {
        // SAFETY: this is the positive group ID created for the owned helper.
        let _ = unsafe { libc::killpg(self.0, libc::SIGKILL) };
    }
}
#[cfg(windows)]
mod windows {
    use super::{Child, Result};
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject,
        },
    };
    pub(in super::super) struct Tree(HANDLE);
    // SAFETY: the owned job handle can be operated from any thread.
    unsafe impl Send for Tree {}
    impl Tree {
        pub(in super::super) fn adopt(child: &Child) -> Result<Self> {
            // SAFETY: unnamed job with default security.
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            anyhow::ensure!(!job.is_null(), "cannot create GenTL helper job object");
            let tree = Self(job);
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: both pointers name live correctly sized Windows objects.
            let configured = unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())?,
                )
            };
            anyhow::ensure!(configured != 0, "cannot configure GenTL helper job object");
            // SAFETY: child owns a live process handle and tree owns job.
            let assigned = unsafe { AssignProcessToJobObject(job, child.as_raw_handle().cast()) };
            anyhow::ensure!(
                assigned != 0,
                "cannot assign GenTL helper to its job object"
            );
            Ok(tree)
        }
        pub(in super::super) fn terminate(&self) {
            // SAFETY: this object owns the live job handle.
            let _ = unsafe { TerminateJobObject(self.0, 1) };
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            // SAFETY: close the handle owned once by this object.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}
#[cfg(windows)]
pub(super) use windows::Tree;
