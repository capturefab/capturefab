//! Containment before initial-thread execution, with no breakaway fallback.
use super::{super::quote, deadline, unavailable};
use anyhow::Result;
use std::{
    fs::OpenOptions,
    io,
    os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        DUPLICATE_SAME_ACCESS, DuplicateHandle, GetLastError, HANDLE, WAIT_FAILED, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    },
    System::{
        JobObjects::{
            CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
            QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
        },
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
            DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
            InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION,
            ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute,
            WaitForSingleObject,
        },
    },
};

fn os_failure(operation: &'static str) -> anyhow::Error {
    // Numeric OS status only; executable and argv are private.
    // SAFETY: reads the calling thread's last-error value, without borrowed data.
    let code = unsafe { GetLastError() };
    crate::types::Categorized::new(crate::types::ErrorKind::Unavailable, operation)
        .details(serde_json::json!({"windows_error":code}))
        .into()
}
fn cleanup_failure(operation: &'static str) -> anyhow::Error {
    // SAFETY: reads this thread's numeric status, without borrowing OS memory.
    let code = unsafe { GetLastError() };
    crate::types::Categorized::new(crate::types::ErrorKind::Unavailable, operation)
        .details(serde_json::json!({"windows_error":code,"cleanup":"unconfirmed"}))
        .into()
}
fn raw(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle().cast()
}
unsafe fn own(handle: HANDLE) -> OwnedHandle {
    // SAFETY: caller transfers one non-null valid owned Windows handle.
    unsafe { OwnedHandle::from_raw_handle(handle.cast()) }
}
struct Attributes {
    storage: Vec<usize>,
    initialized: bool,
    jobs: Box<[HANDLE; 1]>,
    handles: Box<[HANDLE; 3]>,
}
impl Attributes {
    fn pointer(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_mut_ptr().cast()
    }
    fn new(job: HANDLE, handles: [HANDLE; 3]) -> Result<Self> {
        let mut bytes = 0;
        // SAFETY: the first call queries storage size into a valid slot.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 2, 0, &raw mut bytes) };
        if bytes == 0 || bytes > 64 * 1024 {
            return Err(unavailable(
                "industrial helper process attributes are unavailable",
            ));
        }
        let mut attributes = Self {
            storage: vec![0; bytes.div_ceil(size_of::<usize>())],
            initialized: false,
            jobs: Box::new([job]),
            handles: Box::new(handles),
        };
        // SAFETY: aligned owned backing has at least the queried size.
        if unsafe { InitializeProcThreadAttributeList(attributes.pointer(), 2, 0, &raw mut bytes) }
            == 0
        {
            return Err(os_failure(
                "cannot initialize industrial helper process attributes",
            ));
        }
        attributes.initialized = true;
        // Attribute values live in stable heap allocations through Delete, even
        // if this Rust owner moves. Handle owners outlive the attribute owner.
        let jobs = attributes.jobs.as_ptr();
        let inherited = attributes.handles.as_ptr();
        // SAFETY: initialized list and stable arrays, exact byte extents.
        if unsafe {
            UpdateProcThreadAttribute(
                attributes.pointer(),
                0,
                usize::try_from(PROC_THREAD_ATTRIBUTE_JOB_LIST)?,
                jobs.cast(),
                size_of::<[HANDLE; 1]>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
        {
            // SAFETY: reads the current thread's failure status from this call.
            let code = unsafe { GetLastError() };
            return Err(crate::types::Categorized::new(
                crate::types::ErrorKind::Unsupported,
                "Windows industrial helpers require supported creation-time job assignment",
            )
            .details(serde_json::json!({"windows_error":code}))
            .into());
        }
        // SAFETY: the three real inheritable handles stay open through spawn.
        if unsafe {
            UpdateProcThreadAttribute(
                attributes.pointer(),
                0,
                usize::try_from(PROC_THREAD_ATTRIBUTE_HANDLE_LIST)?,
                inherited.cast(),
                size_of::<[HANDLE; 3]>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
        {
            return Err(os_failure(
                "cannot restrict industrial helper handle inheritance",
            ));
        }
        Ok(attributes)
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: initialized attribute list and every backing value remain alive.
            unsafe { DeleteProcThreadAttributeList(self.pointer()) };
        }
    }
}
fn inherited(handle: HANDLE) -> Result<OwnedHandle> {
    let mut duplicate = std::ptr::null_mut();
    // SAFETY: current-process pseudo-handle is borrowed, never closed.
    let current = unsafe { GetCurrentProcess() };
    // SAFETY: duplicate a valid borrowed handle into one owned inheritable handle.
    if unsafe {
        DuplicateHandle(
            current,
            handle,
            current,
            &raw mut duplicate,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(os_failure(
            "cannot prepare industrial helper standard handles",
        ));
    }
    // SAFETY: DuplicateHandle succeeded and transferred one valid handle.
    Ok(unsafe { own(duplicate) })
}
pub(super) struct Job {
    handle: OwnedHandle,
    stopping: AtomicBool,
}
impl Job {
    fn new() -> Result<Arc<Self>> {
        // SAFETY: unnamed, non-inheritable job with default security.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(os_failure("cannot create industrial helper job"));
        }
        let job = Arc::new(Self {
            // SAFETY: CreateJobObjectW succeeded and transferred ownership.
            handle: unsafe { own(handle) },
            stopping: AtomicBool::new(false),
        });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: valid owned handle and exact initialized limits layout.
        if unsafe {
            SetInformationJobObject(
                raw(&job.handle),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())?,
            )
        } == 0
        {
            return Err(os_failure("cannot configure industrial helper job"));
        }
        Ok(job)
    }
    pub(super) fn stop(&self) -> Result<()> {
        self.stopping.store(true, Ordering::Release);
        // SAFETY: this exact non-inherited job handle remains owned through call.
        if unsafe { TerminateJobObject(raw(&self.handle), 1) } == 0 {
            return Err(cleanup_failure(
                "industrial helper job termination is unconfirmed",
            ));
        }
        Ok(())
    }
}
pub(super) struct Process {
    pub(super) job: Arc<Job>,
    process: Option<OwnedHandle>,
    thread: Option<OwnedHandle>,
}
impl Process {
    pub(super) fn spawn(
        mut prepared: quote::Prepared,
        until: Instant,
    ) -> Result<(Self, io::PipeReader)> {
        deadline(until)?;
        let job = Job::new()?;
        let weak = Arc::downgrade(&job);
        crate::cli::on_second_signal(move || {
            if let Some(job) = weak.upgrade() {
                let _ = job.stop();
            }
        });
        let (reader, writer) =
            io::pipe().map_err(|_| unavailable("cannot create industrial helper stdout pipe"))?;
        let input = OpenOptions::new()
            .read(true)
            .open(r"\\.\NUL")
            .map_err(|_| unavailable("cannot prepare industrial helper stdin"))?;
        let errors = OpenOptions::new()
            .write(true)
            .open(r"\\.\NUL")
            .map_err(|_| unavailable("cannot prepare industrial helper stderr"))?;
        let standard = [
            inherited(input.as_raw_handle().cast())?,
            inherited(writer.as_raw_handle().cast())?,
            inherited(errors.as_raw_handle().cast())?,
        ];
        let mut attributes = Attributes::new(
            raw(&job.handle),
            [raw(&standard[0]), raw(&standard[1]), raw(&standard[2])],
        )?;
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = u32::try_from(size_of::<STARTUPINFOEXW>())?;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = raw(&standard[0]);
        startup.StartupInfo.hStdOutput = raw(&standard[1]);
        startup.StartupInfo.hStdError = raw(&standard[2]);
        startup.lpAttributeList = attributes.pointer();
        let mut information = PROCESS_INFORMATION::default();
        deadline(until)?;
        if job.stopping.load(Ordering::Acquire) {
            return Err(unavailable("industrial helper startup was stopped"));
        }
        // SAFETY: writable bounded UTF16 buffers, initialized startup metadata,
        // stable arrays and real inherited handles; child is suspended and
        // assigned to the exact job before it can execute its initial thread.
        let created = unsafe {
            CreateProcessW(
                prepared.application.as_ptr(),
                prepared.command.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                EXTENDED_STARTUPINFO_PRESENT
                    | CREATE_SUSPENDED
                    | CREATE_NO_WINDOW
                    | CREATE_UNICODE_ENVIRONMENT,
                std::ptr::null(),
                std::ptr::null(),
                &raw const startup.StartupInfo,
                &raw mut information,
            )
        };
        if created == 0 {
            return Err(os_failure("could not start industrial helper"));
        }
        let mut process = Self {
            job,
            // SAFETY: successful CreateProcessW transfers these two handles.
            process: Some(unsafe { own(information.hProcess) }),
            // SAFETY: successful CreateProcessW returned the primary thread.
            thread: Some(unsafe { own(information.hThread) }),
        };
        drop(attributes);
        drop(standard);
        drop(writer);
        let mut member = 0;
        let handle = process
            .process
            .as_ref()
            .ok_or_else(|| unavailable("industrial helper process handle is missing"))?;
        // SAFETY: owned process/job handles and valid output BOOL slot.
        if unsafe { IsProcessInJob(raw(handle), raw(&process.job.handle), &raw mut member) } == 0
            || member == 0
        {
            let error = unavailable("industrial helper containment was not established");
            let _ = process.stop_and_wait(until);
            return Err(error);
        }
        if let Err(error) = deadline(until) {
            let _ = process.stop_and_wait(until);
            return Err(error);
        }
        Ok((process, reader))
    }
    pub(super) fn resume(&mut self, cutoff: Option<u64>, until: Instant) -> Result<()> {
        deadline(until)?;
        if self.job.stopping.load(Ordering::Acquire) {
            return Err(unavailable("industrial helper startup was stopped"));
        }
        let thread = self
            .thread
            .take()
            .ok_or_else(|| unavailable("industrial helper primary thread is missing"))?;
        if !super::super::stream::startup_admitted(cutoff, crate::host::monotonic_ns())? {
            return Err(crate::types::Categorized::error(
                crate::types::ErrorKind::Timeout,
                "industrial helper acquisition cutoff expired before resume",
            ));
        }
        // SAFETY: this is the owned primary thread of our suspended process.
        let suspended = unsafe { ResumeThread(raw(&thread)) };
        if suspended == u32::MAX {
            return Err(os_failure("cannot resume contained industrial helper"));
        }
        if suspended != 1 {
            return Err(unavailable(
                "industrial helper suspension state was not the expected one",
            ));
        }
        drop(thread);
        deadline(until)?;
        Ok(())
    }
    pub(super) fn stop_and_wait(&mut self, until: Instant) -> Result<()> {
        let termination = self.job.stop();
        self.thread = None;
        if let Some(process) = self.process.as_ref() {
            let left = until.saturating_duration_since(Instant::now());
            let millis = u32::try_from(left.as_millis())
                .unwrap_or(u32::MAX - 1)
                .min(u32::MAX - 1);
            // SAFETY: live owned process handle, never closed during this wait.
            match unsafe { WaitForSingleObject(raw(process), millis) } {
                WAIT_OBJECT_0 => self.process = None,
                WAIT_TIMEOUT => return Err(cleanup_timeout()),
                WAIT_FAILED => {
                    return Err(cleanup_failure(
                        "industrial helper process exit is unconfirmed",
                    ));
                }
                _ => return Err(unavailable("invalid industrial helper process wait result")),
            }
        }
        loop {
            let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            // SAFETY: exact owned job, initialized output with matching extent.
            if unsafe {
                QueryInformationJobObject(
                    raw(&self.job.handle),
                    JobObjectBasicAccountingInformation,
                    (&raw mut accounting).cast(),
                    u32::try_from(size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>())?,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(cleanup_failure(
                    "industrial helper tree completion is unconfirmed",
                ));
            }
            if accounting.ActiveProcesses == 0 {
                return termination;
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(cleanup_timeout());
            }
            std::thread::park_timeout(left.min(Duration::from_millis(5)));
        }
    }
}
fn cleanup_timeout() -> anyhow::Error {
    crate::types::Categorized::new(
        crate::types::ErrorKind::Timeout,
        "industrial helper cleanup did not confirm process-tree completion",
    )
    .details(serde_json::json!({"cleanup":"unconfirmed"}))
    .into()
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.job.stop();
    }
}
