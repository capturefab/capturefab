//! On-demand document scanners. Native drivers run in disposable processes;
//! opening a scanner and starting acquisition never move its scan head.
use crate::types::{Backend, CameraInfo, Transport};
use anyhow::{Result, bail, ensure};
use std::time::Duration;

#[cfg(feature = "scanner")]
mod platform;
#[cfg(feature = "scanner")]
use implementation::*;

pub fn is_source(selector: &str) -> bool {
    selector.starts_with("scanner:")
}

fn locator(selector: &str) -> Result<(&str, &str)> {
    let (backend, device) = selector
        .strip_prefix("scanner:")
        .and_then(|s| s.split_once(':'))
        .ok_or_else(|| anyhow::anyhow!("invalid scanner selector; use scanner:sane:DEVICE, scanner:wia:ID or scanner:ica:UUID"))?;
    ensure!(
        matches!(backend, "sane" | "wia" | "ica")
            && !device.is_empty()
            && device.len() <= 16384
            && !device.chars().any(char::is_control),
        "invalid scanner selector"
    );
    Ok((backend, device))
}

/// Validate a driver locator without requiring that driver or build feature.
pub fn validate_selector_shape(selector: &str) -> Result<()> {
    locator(selector).map(|_| ())
}

fn host_backend() -> &'static str {
    match std::env::consts::OS {
        "linux" => "sane",
        "windows" => "wia",
        "macos" => "ica",
        _ => "unsupported",
    }
}

pub fn info(selector: &str) -> Result<CameraInfo> {
    let (backend, device) = locator(selector)?;
    ensure!(
        backend == host_backend(),
        "scanner backend {backend} is unsupported on {}",
        std::env::consts::OS
    );
    ensure!(
        cfg!(feature = "scanner"),
        "scanner support disabled in this build; rebuild with --features scanner"
    );
    Ok(CameraInfo {
        id: selector.into(),
        transport: Transport::Scanner,
        vendor: backend.to_uppercase(),
        model: "Document scanner".into(),
        serial: device.into(),
        address: Some(selector.into()),
        profile: Default::default(),
    })
}

pub fn discover(timeout: Duration) -> Result<Vec<CameraInfo>> {
    #[cfg(feature = "scanner")]
    {
        let mut command = platform::command("discover", None, 150, "Color", "Default")?;
        command.env(
            "CAPTUREFAB_SCANNER_DISCOVERY_MS",
            timeout.as_millis().saturating_sub(100).max(50).to_string(),
        );
        // A native helper must count framework startup in the parent's budget,
        // and leave time for its output and process teardown.
        let budget = timeout.as_millis();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        command.env(
            "CAPTUREFAB_SCANNER_DISCOVERY_DEADLINE_MS",
            (now + budget.saturating_sub((budget / 3).min(200))).to_string(),
        );
        let bytes = Helper::spawn(command, 1024 * 1024)?.wait(timeout)?;
        platform::discovery(&bytes)?
            .into_iter()
            .map(|device| {
                let mut info = info(&format!("scanner:{}:{}", host_backend(), device.id))?;
                if !device.name.is_empty() {
                    info.model = device.name;
                }
                if !device.vendor.is_empty() {
                    info.vendor = device.vendor;
                }
                if !device.serial.is_empty() {
                    info.serial = device.serial;
                }
                Ok(info)
            })
            .collect()
    }
    #[cfg(not(feature = "scanner"))]
    {
        let _ = timeout;
        Ok(Vec::new())
    }
}

pub fn open(info: &CameraInfo) -> Result<Box<dyn Backend>> {
    #[cfg(feature = "scanner")]
    {
        let _ = self::info(&info.id)?;
        let (_, device) = locator(&info.id)?;
        Ok(Box::new(Scanner::new(device.into())))
    }
    #[cfg(not(feature = "scanner"))]
    {
        let _ = info;
        bail!("scanner support disabled in this build; rebuild with --features scanner")
    }
}

pub fn doctor() -> serde_json::Value {
    #[cfg(feature = "scanner")]
    {
        platform::available()
    }
    #[cfg(not(feature = "scanner"))]
    {
        serde_json::json!({"available":false,"reason":"scanner support not compiled in"})
    }
}

#[cfg(feature = "scanner")]
#[derive(serde::Deserialize)]
struct Device {
    id: String,
    name: String,
    #[serde(default)]
    vendor: String,
    #[serde(default)]
    serial: String,
}

#[cfg(all(feature = "scanner", target_os = "macos"))]
fn macos_executable() -> Result<std::path::PathBuf> {
    use std::{fs, io::Write, os::unix::fs::OpenOptionsExt};
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|e| anyhow::anyhow!("scanner helper entropy: {e}"))?;
    let dir = crate::ipc::runtime_dir().join(format!(
        "scanner-{}",
        nonce.iter().map(|n| format!("{n:02x}")).collect::<String>()
    ));
    crate::ipc::ensure_private_dir(&dir)?;
    let path = dir.join("capturefab-scanner-ica");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&path)?;
    file.write_all(include_bytes!(concat!(
        env!("OUT_DIR"),
        "/capturefab-scanner-ica.bin"
    )))?;
    drop(file);
    Ok(path)
}

#[cfg(feature = "scanner")]
mod implementation {
    use super::*;
    use crate::types::{Frame, MONO8, RGB8, RegisterIo, TransportStats};
    use anyhow::Context;
    use std::{
        io::Read,
        process::{Child, Command, Stdio},
        sync::mpsc::{self, Receiver},
        time::{Instant, SystemTime, UNIX_EPOCH},
    };

    // A4 at 600 dpi RGB fits the scanner's 128 MiB shared-memory slot.
    const MAX_FRAME: usize = 128 * 1024 * 1024;
    const MAX_OUTPUT: usize = MAX_FRAME * 2 + 65536;
    const MODES: [&str; 3] = ["Color", "Gray", "Lineart"];
    const SOURCES: [&str; 3] = ["Default", "Flatbed", "Feeder"];

    pub(super) struct Helper {
        child: Child,
        output: Receiver<(bool, Result<Vec<u8>>)>,
        stdout: Option<Vec<u8>>,
        stderr: Option<Vec<u8>>,
        finished: bool,
        #[cfg(target_os = "macos")]
        directory: Option<std::path::PathBuf>,
        #[cfg(windows)]
        _job: std::os::windows::io::OwnedHandle,
    }
    impl Helper {
        pub(super) fn spawn(mut command: Command, limit: usize) -> Result<Self> {
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            #[cfg(target_os = "linux")]
            {
                use std::os::unix::process::CommandExt;
                unsafe extern "C" {
                    fn prctl(option: std::ffi::c_int, ...) -> std::ffi::c_int;
                    fn getppid() -> std::ffi::c_int;
                }
                let parent = std::process::id() as std::ffi::c_int;
                // SAFETY: this child-only pre_exec callback uses only Linux's
                // async-signal-safe system calls, with no allocation or locks.
                unsafe {
                    command.pre_exec(move || {
                        if prctl(1, 9usize, 0usize, 0usize, 0usize) != 0 {
                            // PR_SET_PDEATHSIG, SIGKILL
                            return Err(std::io::Error::last_os_error());
                        }
                        if getppid() != parent {
                            return Err(std::io::Error::from_raw_os_error(10));
                        }
                        Ok(())
                    });
                }
            }
            #[cfg(target_os = "macos")]
            let directory = {
                let executable = std::path::Path::new(command.get_program());
                if executable
                    .file_name()
                    .is_some_and(|name| name == "capturefab-scanner-ica")
                {
                    let dir = executable
                        .parent()
                        .context("scanner helper directory")?
                        .to_path_buf();
                    let scratch = dir.join("transfer");
                    crate::ipc::ensure_private_dir(&scratch)?;
                    command.env("CAPTUREFAB_SCANNER_TRANSFER_DIR", scratch);
                    Some(dir)
                } else {
                    None
                }
            };
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x08000000); // CREATE_NO_WINDOW
            }
            let child = command.spawn();
            #[cfg(target_os = "macos")]
            if child.is_err()
                && let Some(dir) = &directory
            {
                let _ = std::fs::remove_dir_all(dir);
            }
            let mut child = child.context("scanner helper not found or could not be started; install the platform scanner driver and runtime")?;
            #[cfg(windows)]
            let job = match scanner_job(&child) {
                Ok(job) => job,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            };
            let stdout = child.stdout.take().context("scanner helper stdout")?;
            let stderr = child.stderr.take().context("scanner helper stderr")?;
            let (tx, output) = mpsc::channel();
            let out = tx.clone();
            std::thread::spawn(move || {
                let bytes = (|| -> Result<Vec<u8>> {
                    let mut bytes = Vec::new();
                    stdout.take(limit as u64 + 1).read_to_end(&mut bytes)?;
                    ensure!(bytes.len() <= limit, "scanner output exceeds memory limit");
                    Ok(bytes)
                })();
                let _ = out.send((true, bytes));
            });
            std::thread::spawn(move || {
                let bytes = (|| -> Result<Vec<u8>> {
                    let mut bytes = Vec::new();
                    let mut stderr = stderr;
                    (&mut stderr).take(16384).read_to_end(&mut bytes)?;
                    std::io::copy(&mut stderr, &mut std::io::sink())?;
                    Ok(bytes)
                })();
                let _ = tx.send((false, bytes));
            });
            Ok(Self {
                child,
                output,
                stdout: None,
                stderr: None,
                finished: false,
                #[cfg(target_os = "macos")]
                directory,
                #[cfg(windows)]
                _job: job,
            })
        }
        fn poll(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
            let deadline = Instant::now() + timeout;
            loop {
                while let Ok((stdout, bytes)) = self.output.try_recv() {
                    if stdout {
                        self.stdout = Some(bytes?);
                    } else {
                        self.stderr = Some(bytes?);
                    }
                }
                if self.stdout.is_some()
                    && self.stderr.is_some()
                    && let Some(status) = self.child.try_wait()?
                {
                    self.finished = true;
                    let diagnostics = String::from_utf8_lossy(self.stderr.as_ref().unwrap())
                        .trim()
                        .to_owned();
                    ensure!(
                        status.success(),
                        "scanner acquisition failed ({status}): {diagnostics}"
                    );
                    return Ok(self.stdout.take());
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(None);
                }
                match self
                    .output
                    .recv_timeout(remaining.min(Duration::from_millis(10)))
                {
                    Ok((stdout, bytes)) => {
                        if stdout {
                            self.stdout = Some(bytes?);
                        } else {
                            self.stderr = Some(bytes?);
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        std::thread::sleep(remaining.min(Duration::from_millis(2)))
                    }
                }
            }
        }
        pub(super) fn wait(mut self, timeout: Duration) -> Result<Vec<u8>> {
            self.poll(timeout)?
                .context("timeout waiting for scanner helper")
        }
    }
    impl Drop for Helper {
        fn drop(&mut self) {
            #[cfg(unix)]
            if !self.finished {
                unsafe extern "C" {
                    fn kill(pid: std::ffi::c_int, signal: std::ffi::c_int) -> std::ffi::c_int;
                }
                // SAFETY: the helper creates its own process group. Only that
                // group and its descendants are terminated, never the worker.
                unsafe {
                    kill(-(self.child.id() as std::ffi::c_int), 9);
                }
            }
            if !self.finished {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
            #[cfg(target_os = "macos")]
            if let Some(dir) = &self.directory {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    #[cfg(windows)]
    fn scanner_job(child: &Child) -> Result<std::os::windows::io::OwnedHandle> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        // SAFETY: null attributes/name create a private, non-inheritable job;
        // OwnedHandle closes it on every exit, including a worker crash.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        ensure!(
            !handle.is_null(),
            "create scanner process job: {}",
            std::io::Error::last_os_error()
        );
        let job = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the pointer/size name a fully initialized limits structure,
        // and the job and child handles remain live throughout these calls.
        ensure!(
            unsafe {
                SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&limits) as u32,
                )
            } != 0,
            "configure scanner process job: {}",
            std::io::Error::last_os_error()
        );
        ensure!(
            unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } != 0,
            "isolate scanner helper in process job: {}",
            std::io::Error::last_os_error()
        );
        Ok(job)
    }

    pub(super) struct Scanner {
        device: String,
        dpi: u32,
        mode: u32,
        source: u32,
        running: bool,
        pending: Option<Helper>,
        width: u32,
        height: u32,
        pixel_format: u32,
        bytes: u32,
        sequence: u64,
    }
    impl Scanner {
        pub(super) fn new(device: String) -> Self {
            Self {
                device,
                dpi: 150,
                mode: 0,
                source: 0,
                running: false,
                pending: None,
                width: 0,
                height: 0,
                pixel_format: RGB8,
                bytes: 1,
                sequence: 0,
            }
        }
    }
    impl RegisterIo for Scanner {
        fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
            let values = [
                self.width,
                self.height,
                self.pixel_format,
                self.bytes,
                u32::from(self.running),
                self.dpi,
                self.mode,
                self.source,
            ];
            let registers: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
            let start = usize::try_from(address)?;
            let end = start
                .checked_add(length)
                .context("scanner register range overflow")?;
            Ok(registers
                .get(start..end)
                .context("unsupported scanner register")?
                .to_vec())
        }
        fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
            ensure!(data.len() == 4, "unsupported scanner register size");
            let value = u32::from_le_bytes(data.try_into()?);
            if address == 16 {
                ensure!(
                    value <= 1,
                    "scanner acquisition control must be 0 (stop) or 1 (start)"
                );
                return Ok(());
            }
            ensure!(
                !self.running && self.pending.is_none(),
                "stop scanner acquisition before changing scan settings"
            );
            match address {
                20 => {
                    ensure!(
                        (75..=600).contains(&value),
                        "scan resolution must be 75..600 dpi"
                    );
                    self.dpi = value;
                }
                24 => {
                    ensure!((value as usize) < MODES.len(), "unsupported scan mode");
                    self.mode = value;
                }
                28 => {
                    ensure!((value as usize) < SOURCES.len(), "unsupported scan source");
                    self.source = value;
                }
                _ => bail!("unsupported scanner register"),
            }
            Ok(())
        }
    }
    impl Backend for Scanner {
        fn xml(&mut self) -> Result<String> {
            let mut xml = String::from(
                "<RegisterDescription><Category Name='Root'><pFeature>ScanResolution</pFeature><pFeature>ScanMode</pFeature><pFeature>ScanSource</pFeature><pFeature>Width</pFeature><pFeature>Height</pFeature><pFeature>PixelFormat</pFeature><pFeature>PayloadSize</pFeature><pFeature>AcquisitionStart</pFeature><pFeature>AcquisitionStop</pFeature></Category>",
            );
            for (name, address) in [
                ("Width", 0),
                ("Height", 4),
                ("PixelFormat", 8),
                ("PayloadSize", 12),
                ("IsAcquiring", 16),
                ("ScanResolution", 20),
            ] {
                let writable = address == 20;
                let access = if writable { "RW" } else { "RO" };
                let bounds = if writable {
                    "<Min>75</Min><Max>600</Max><Inc>1</Inc><Unit>dpi</Unit><pIsLocked>IsAcquiring</pIsLocked><Description>Resolution requested from the scanner driver. The device may support only a subset; unsupported settings fail without a fallback.</Description>"
                } else {
                    ""
                };
                xml.push_str(&format!("<Integer Name='{name}'><pValue>{name}Reg</pValue>{bounds}</Integer><IntReg Name='{name}Reg'><Address>{address}</Address><Length>4</Length><AccessMode>{access}</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>"));
            }
            for (name, address, choices) in [("ScanMode", 24, &MODES), ("ScanSource", 28, &SOURCES)]
            {
                xml.push_str(&format!("<Enumeration Name='{name}'><pValue>{name}Reg</pValue><pIsLocked>IsAcquiring</pIsLocked>"));
                for (index, choice) in choices.iter().enumerate() {
                    xml.push_str(&format!(
                        "<EnumEntry Name='{choice}'><Value>{index}</Value></EnumEntry>"
                    ));
                }
                xml.push_str(&format!("</Enumeration><IntReg Name='{name}Reg'><Address>{address}</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>"));
            }
            xml.push_str("<Command Name='AcquisitionStart'><pValue>Control</pValue><CommandValue>1</CommandValue></Command><Command Name='AcquisitionStop'><pValue>Control</pValue><CommandValue>0</CommandValue></Command><IntReg Name='Control'><Address>16</Address><Length>4</Length><AccessMode>WO</AccessMode></IntReg></RegisterDescription>");
            Ok(xml)
        }
        fn start(&mut self, _: usize) -> Result<()> {
            self.running = true;
            Ok(())
        }
        fn request_frame(&mut self) -> Result<()> {
            ensure!(self.running, "scanner acquisition has not started");
            if self.pending.is_none() {
                let command = super::platform::command(
                    "scan",
                    Some(&self.device),
                    self.dpi,
                    MODES[self.mode as usize],
                    SOURCES[self.source as usize],
                )?;
                self.pending = Some(Helper::spawn(command, MAX_OUTPUT)?);
            }
            Ok(())
        }
        fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
            ensure!(self.running, "scanner acquisition has not started");
            let Some(pending) = self.pending.as_mut() else {
                std::thread::sleep(timeout.min(Duration::from_millis(100)));
                bail!("frame timeout; scanner awaits a capture request");
            };
            let bytes = match pending.poll(timeout) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => bail!("frame timeout waiting for scanner"),
                Err(e) => {
                    self.pending.take();
                    return Err(e);
                }
            };
            self.pending.take();
            let mut frame = decode_pnm(&bytes)?;
            self.sequence = self
                .sequence
                .checked_add(1)
                .context("scan sequence exhausted")?;
            frame.id = self.sequence;
            frame.timestamp_ns = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .min(u64::MAX as u128) as u64;
            self.width = frame.width;
            self.height = frame.height;
            self.pixel_format = frame.pixel_format;
            self.bytes = frame.data.len() as u32;
            Ok(frame)
        }
        fn stop(&mut self) -> Result<()> {
            self.pending.take();
            self.running = false;
            Ok(())
        }
        fn stats(&self) -> Option<TransportStats> {
            Some(TransportStats {
                notes: vec![format!(
                    "Document scanner: {} dpi, {}, {}. Each capture requests one page.",
                    self.dpi, MODES[self.mode as usize], SOURCES[self.source as usize]
                )],
                ..TransportStats::default()
            })
        }
    }

    /// Binary Netpbm from SANE and the native helpers. Preserve the first raster
    /// byte even when it happens to be whitespace or '#'. Normalize 1/16-bit
    /// scans to the app's existing Mono8/RGB8 formats.
    fn decode_pnm(bytes: &[u8]) -> Result<Frame> {
        let mut offset = 0;
        fn token<'a>(bytes: &'a [u8], offset: &mut usize) -> Result<&'a [u8]> {
            loop {
                while bytes.get(*offset).is_some_and(u8::is_ascii_whitespace) {
                    *offset += 1;
                }
                if bytes.get(*offset) != Some(&b'#') {
                    break;
                }
                while bytes.get(*offset).is_some_and(|b| *b != b'\n') {
                    *offset += 1;
                }
            }
            let begin = *offset;
            while bytes
                .get(*offset)
                .is_some_and(|b| !b.is_ascii_whitespace() && *b != b'#')
            {
                *offset += 1;
            }
            ensure!(
                *offset > begin && *offset <= 65536,
                "invalid scanner PNM header"
            );
            Ok(&bytes[begin..*offset])
        }
        fn number(bytes: &[u8], offset: &mut usize) -> Result<u32> {
            std::str::from_utf8(token(bytes, offset)?)?
                .parse()
                .context("invalid scanner PNM dimension or sample range")
        }
        let magic = token(bytes, &mut offset)?;
        ensure!(
            matches!(magic, b"P4" | b"P5" | b"P6"),
            "unsupported scanner image; expected binary PNM"
        );
        let width = number(bytes, &mut offset)?;
        let height = number(bytes, &mut offset)?;
        let max = if magic == b"P4" {
            1
        } else {
            number(bytes, &mut offset)?
        };
        ensure!(
            width > 0 && height > 0 && max > 0 && max <= 65535,
            "invalid scanner image dimensions or sample range"
        );
        ensure!(
            bytes.get(offset).is_some_and(u8::is_ascii_whitespace),
            "invalid scanner PNM raster separator"
        );
        if bytes.get(offset..offset + 2) == Some(b"\r\n") {
            offset += 2;
        } else {
            offset += 1;
        }
        let channels = if magic == b"P6" { 3usize } else { 1 };
        let size = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(channels))
            .context("scanner image dimensions overflow")?;
        ensure!(
            size <= MAX_FRAME,
            "scanner image exceeds 128 MiB frame limit; lower ScanResolution"
        );
        let raster = bytes.get(offset..).context("truncated scanner image")?;
        let data = if magic == b"P4" {
            let row = (width as usize).div_ceil(8);
            ensure!(
                raster.len() == row * height as usize,
                "truncated or trailing scanner image data"
            );
            (0..height as usize)
                .flat_map(|y| {
                    (0..width as usize).map(move |x| {
                        if raster[y * row + x / 8] & (0x80 >> (x % 8)) == 0 {
                            255
                        } else {
                            0
                        }
                    })
                })
                .collect()
        } else {
            let sample_bytes = if max > 255 { 2 } else { 1 };
            ensure!(
                raster.len() == size * sample_bytes,
                "truncated or trailing scanner image data"
            );
            let mut data = Vec::with_capacity(size);
            for sample in raster.chunks_exact(sample_bytes) {
                let value = if sample_bytes == 2 {
                    u16::from_be_bytes([sample[0], sample[1]]) as u32
                } else {
                    sample[0] as u32
                };
                ensure!(value <= max, "invalid scanner sample exceeds PNM range");
                data.push(((value * 255 + max / 2) / max) as u8);
            }
            data
        };
        Ok(Frame {
            width,
            height,
            pixel_format: if channels == 3 { RGB8 } else { MONO8 },
            data,
            ..Frame::default()
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[cfg(unix)]
        #[test]
        fn helper_bounds_output_and_propagates_driver_errors() {
            let mut success = Command::new("/bin/sh");
            success.args(["-c", "printf scan"]);
            assert_eq!(
                Helper::spawn(success, 4)
                    .unwrap()
                    .wait(Duration::from_secs(2))
                    .unwrap(),
                b"scan"
            );
            let mut oversized = Command::new("/bin/sh");
            oversized.args(["-c", "printf toolarge"]);
            assert!(
                Helper::spawn(oversized, 4)
                    .unwrap()
                    .wait(Duration::from_secs(2))
                    .unwrap_err()
                    .to_string()
                    .contains("memory limit")
            );
            let mut failure = Command::new("/bin/sh");
            failure.args(["-c", "printf 'unsupported paper source' >&2; exit 1"]);
            let error = Helper::spawn(failure, 4)
                .unwrap()
                .wait(Duration::from_secs(2))
                .unwrap_err();
            assert_eq!(crate::cli::error_code(&error), ("unsupported", 5));
        }

        #[cfg(unix)]
        #[test]
        fn helper_timeout_terminates_acquisition_process() {
            let mut command = Command::new("/bin/sleep");
            command.arg("10");
            let mut helper = Helper::spawn(command, 4).unwrap();
            let pid = helper.child.id();
            assert!(helper.poll(Duration::from_millis(20)).unwrap().is_none());
            drop(helper);
            unsafe extern "C" {
                fn kill(pid: std::ffi::c_int, signal: std::ffi::c_int) -> std::ffi::c_int;
            }
            // SAFETY: signal 0 only checks existence; it sends no signal.
            assert_eq!(unsafe { kill(pid as std::ffi::c_int, 0) }, -1);
        }
        #[test]
        fn binary_pnm_preserves_pixels_and_scales_depth() {
            let frame = decode_pnm(b"P6\n# SANE\n2 1\n255\n\x20\x0a\x23\x00\xff\x80").unwrap();
            assert_eq!(
                (frame.width, frame.height, frame.pixel_format),
                (2, 1, RGB8)
            );
            assert_eq!(frame.data, [32, 10, 35, 0, 255, 128]);
            assert_eq!(
                decode_pnm(b"P5\n2 1\n65535\n\x00\x00\xff\xff")
                    .unwrap()
                    .data,
                [0, 255]
            );
            assert_eq!(
                decode_pnm(b"P5\r\n2 1\r\n15\r\n\x00\x0f").unwrap().data,
                [0, 255]
            );
        }
        #[test]
        fn lineart_unpacks_padded_rows() {
            let frame = decode_pnm(b"P4\n9 2\n\xaa\x80\x55\x00").unwrap();
            assert_eq!(frame.pixel_format, MONO8);
            assert_eq!(
                frame.data,
                [
                    0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255
                ]
            );
        }
        #[test]
        fn malformed_or_oversized_scans_fail() {
            for bytes in [
                b"P5\n0 1\n255\n".as_slice(),
                b"P6\n4294967295 4294967295\n255\n",
                b"P5\n2 1\n255\n\xff",
                b"P5\n1 1\n1\n\x02",
                b"P3\n1 1\n255\n0 0 0",
            ] {
                assert!(decode_pnm(bytes).is_err());
            }
        }
        #[test]
        fn opening_and_arming_does_not_scan() {
            let mut scanner = Scanner::new("not a real device".into());
            let map = crate::genicam::NodeMap::parse(&scanner.xml().unwrap()).unwrap();
            assert!(map.has("ScanResolution"));
            scanner.start(1).unwrap();
            assert!(scanner.pending.is_none());
            assert!(
                scanner
                    .next_frame(Duration::ZERO)
                    .unwrap_err()
                    .to_string()
                    .contains("timeout")
            );
            assert!(scanner.write_memory(20, &300u32.to_le_bytes()).is_err());
            scanner.stop().unwrap();
            scanner.write_memory(20, &300u32.to_le_bytes()).unwrap();
            assert_eq!(scanner.dpi, 300);
        }

        #[test]
        fn invalid_scan_settings_do_not_change_the_requested_scan() {
            let mut scanner = Scanner::new("not a real device".into());
            for (address, values) in [
                (20, [0, 74, 601, u32::MAX]),
                (24, [3, 4, 5, u32::MAX]),
                (28, [3, 4, 5, u32::MAX]),
                (16, [2, 3, 4, u32::MAX]),
            ] {
                for value in values {
                    assert!(scanner.write_memory(address, &value.to_le_bytes()).is_err());
                    assert_eq!((scanner.dpi, scanner.mode, scanner.source), (150, 0, 0));
                    assert!(!scanner.running);
                    assert!(scanner.pending.is_none());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selectors_preserve_driver_ids_and_reject_invalid_locators() {
        assert_eq!(
            locator("scanner:sane:net:192.168.1.2:epson").unwrap(),
            ("sane", "net:192.168.1.2:epson")
        );
        for id in [
            "scanner:",
            "scanner:twain:1",
            "scanner:sane:",
            "scanner:wia:a\nb",
        ] {
            assert!(locator(id).is_err());
        }
        assert!(is_source("scanner:ica:uuid"));
        assert!(!is_source("sim:0"));
    }
}
