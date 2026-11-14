//! An explicit bridge to installed industrial protocol stacks. The helper
//! owns its connection and emits one `Reading` JSON object per line; this
//! module supplies bounded transport and process lifetime management only.

use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;

use super::Source;
use crate::types::{Categorized, ErrorKind};

/// A helper is executed directly, without shell expansion. Arguments and
/// helper output are never echoed in errors: they may contain credentials.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub executable: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
}

impl Config {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.executable.as_os_str().is_empty() {
            return Err(Categorized::error(
                ErrorKind::Usage,
                "industrial helper executable must not be empty",
            ));
        }
        if self.args.len() > 1024 {
            return Err(Categorized::error(
                ErrorKind::Usage,
                "industrial helper accepts at most 1024 arguments",
            ));
        }
        #[cfg(windows)]
        {
            quote::prepare(self)?;
        }
        Ok(())
    }
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    config.validate()?;
    #[cfg(unix)]
    {
        Ok(unix::open(config))
    }
    #[cfg(windows)]
    {
        windows::open(config)
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(Categorized::error(
            ErrorKind::Unsupported,
            "industrial protocol helpers require Unix or Windows",
        ))
    }
}

#[cfg(any(windows, test))]
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "Windows port primitives are tested on other hosts"
    )
)]
mod quote;
#[cfg(any(windows, test))]
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "Windows port primitives are tested on other hosts"
    )
)]
mod stream;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
mod unix {
    use std::{
        io::{ErrorKind as IoErrorKind, Read as _, Write},
        os::{fd::AsRawFd as _, unix::process::CommandExt as _},
        process::{Child, ChildStdout, Command, Stdio},
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use anyhow::Result;

    use super::Config;
    use crate::{
        timeseries::{Reading, Source},
        types::{Categorized, ErrorKind},
    };

    const MAX_LINE: usize = 1024 * 1024;
    const MAX_BATCH: usize = 256;

    /// Count serialization without allocating a second copy of a measurement.
    /// Stop at the batch budget even when JSON number normalization expands it.
    #[derive(Default)]
    struct SizeLimit {
        bytes: usize,
    }

    impl Write for SizeLimit {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes += bytes.len();
            if self.bytes > MAX_LINE - 2 {
                return Err(std::io::Error::other("helper reading exceeds batch budget"));
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct Bridge {
        child: Arc<Mutex<Option<Child>>>,
        stdout: ChildStdout,
        pending: Vec<u8>,
        line: u64,
        eof: bool,
        observed: Option<u64>,
    }

    struct Lazy {
        config: Config,
        bridge: Option<Bridge>,
    }

    pub(super) fn open(config: &Config) -> Box<dyn Source> {
        Box::new(Lazy {
            config: config.clone(),
            bridge: None,
        })
    }

    impl Source for Lazy {
        fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
            let began = Instant::now();
            if self.bridge.is_none() {
                self.bridge = Some(spawn(&self.config)?);
            }
            self.bridge.as_mut().map_or_else(
                || {
                    Err(Categorized::error(
                        ErrorKind::OperationFailed,
                        "industrial helper did not start",
                    ))
                },
                |bridge| bridge.read(timeout.saturating_sub(began.elapsed())),
            )
        }
        fn observed_ns(&self) -> Option<u64> {
            self.bridge.as_ref().and_then(Source::observed_ns)
        }
        fn buffered_timestamps(&self) -> bool {
            true
        }
        fn drain(&mut self) -> Result<Option<Reading>> {
            self.bridge.as_mut().map_or(Ok(None), Source::drain)
        }
    }

    fn spawn(config: &Config) -> Result<Bridge> {
        let mut child = Command::new(&config.executable)
            .args(&config.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // Helpers must put measurements on stdout. Keeping their stderr
            // private prevents an SDK diagnostic from bypassing redaction.
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|error| {
                Categorized::error(
                    ErrorKind::Unavailable,
                    format!("could not start industrial helper: {error}"),
                )
            })?;
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Categorized::error(
                ErrorKind::OperationFailed,
                "industrial helper stdout was not captured",
            ));
        };
        let bridge = Bridge {
            child: Arc::new(Mutex::new(Some(child))),
            stdout,
            pending: Vec::new(),
            line: 0,
            eof: false,
            observed: None,
        };
        crate::cli::on_second_signal(bridge.killer());
        let fd = bridge.stdout.as_raw_fd();
        // SAFETY: fd belongs to stdout and is open for the complete calls;
        // these fcntl operations only read and set descriptor status flags.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: the same valid descriptor, preserving its existing flags.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(bridge)
    }

    impl Bridge {
        fn killer(&self) -> impl FnMut() + Send + 'static {
            let child = Arc::downgrade(&self.child);
            move || {
                if let Some(child) = child.upgrade() {
                    terminate(&child);
                }
            }
        }
    }

    impl Bridge {
        fn reading(&self, bytes: &[u8]) -> Result<Reading> {
            serde_json::from_slice::<Reading>(bytes).map_err(|error| {
                Categorized::error(
                    ErrorKind::OperationFailed,
                    format!(
                        "industrial helper line {} is not a reading: {:?} at column {}",
                        self.line + 1,
                        error.classify(),
                        error.column()
                    ),
                )
            })
        }
        fn encoded_size(reading: &Reading) -> Result<usize> {
            let mut size = SizeLimit::default();
            serde_json::to_writer(&mut size, reading).map_err(|_| {
                Categorized::error(
                    ErrorKind::OperationFailed,
                    "industrial helper serialized reading exceeds 1 MiB batch limit",
                )
            })?;
            Ok(size.bytes)
        }
    }
    impl Source for Bridge {
        fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
            let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
                Categorized::error(ErrorKind::Usage, "industrial helper timeout is too large")
            })?;
            let mut readings = Vec::new();
            let mut batch_bytes = 2;
            loop {
                while let Some(newline) = self.pending.iter().position(|&byte| byte == b'\n') {
                    if newline > MAX_LINE {
                        return Err(line_too_long());
                    }
                    let separator = usize::from(!readings.is_empty());
                    if !readings.is_empty() && batch_bytes + separator + newline > MAX_LINE {
                        return Ok(readings);
                    }
                    let reading = match self.reading(&self.pending[..newline]) {
                        Ok(reading) => reading,
                        Err(_) if !readings.is_empty() => return Ok(readings),
                        Err(error) => return Err(error),
                    };
                    let size = match Self::encoded_size(&reading) {
                        Ok(size) => size,
                        Err(_) if !readings.is_empty() => return Ok(readings),
                        Err(error) => return Err(error),
                    };
                    if batch_bytes + separator + size > MAX_LINE {
                        return Ok(readings);
                    }
                    batch_bytes += separator + size;
                    self.line += 1;
                    self.pending.drain(..=newline);
                    readings.push(reading);
                    if readings.len() == MAX_BATCH {
                        return Ok(readings);
                    }
                }
                if self.pending.len() > MAX_LINE {
                    return Err(line_too_long());
                }
                // Do not add another receive time to a batch already containing
                // complete measurements. Buffered rows keep this chunk's clock.
                if !readings.is_empty() {
                    return Ok(readings);
                }
                if self.eof {
                    return Err(Categorized::error(
                        ErrorKind::Disconnected,
                        if self.pending.is_empty() {
                            "industrial helper closed stdout"
                        } else {
                            "industrial helper ended with an incomplete JSON line"
                        },
                    ));
                }
                if Instant::now() >= deadline {
                    return Ok(readings);
                }
                let mut chunk = [0u8; 8192];
                match self.stdout.read(&mut chunk) {
                    Ok(0) => self.eof = true,
                    Ok(count) => {
                        self.observed = crate::host::monotonic_ns();
                        self.pending.extend_from_slice(&chunk[..count]);
                    }
                    Err(error) if error.kind() == IoErrorKind::Interrupted => {}
                    Err(error) if error.kind() == IoErrorKind::WouldBlock => {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            return Ok(readings);
                        }
                        let millis = i32::try_from(remaining.as_millis().saturating_add(1))
                            .unwrap_or(i32::MAX);
                        let mut descriptor = libc::pollfd {
                            fd: self.stdout.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        };
                        // SAFETY: descriptor is initialized and stdout remains
                        // open throughout this bounded wait.
                        if unsafe { libc::poll(&raw mut descriptor, 1, millis) } < 0 {
                            let error = std::io::Error::last_os_error();
                            if error.kind() != IoErrorKind::Interrupted {
                                return Err(error.into());
                            }
                        }
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
        fn observed_ns(&self) -> Option<u64> {
            self.observed
        }
        fn buffered_timestamps(&self) -> bool {
            true
        }
        fn drain(&mut self) -> Result<Option<Reading>> {
            if self.pending.is_empty() {
                return Ok(None);
            }
            let Some(newline) = self.pending.iter().position(|&byte| byte == b'\n') else {
                return Err(Categorized::error(
                    ErrorKind::Incomplete,
                    "industrial helper acquisition ended with an incomplete JSON line",
                ));
            };
            if newline > MAX_LINE {
                return Err(line_too_long());
            }
            let reading = self.reading(&self.pending[..newline])?;
            Self::encoded_size(&reading)?;
            self.pending.drain(..=newline);
            self.line += 1;
            Ok(Some(reading))
        }
    }

    fn line_too_long() -> anyhow::Error {
        Categorized::error(
            ErrorKind::OperationFailed,
            format!("industrial helper JSON line exceeds {MAX_LINE} bytes"),
        )
    }

    fn terminate(owner: &Mutex<Option<Child>>) {
        let mut owner = owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(mut child) = owner.take() {
            if let Ok(group) = libc::pid_t::try_from(child.id()) {
                // SAFETY: the unreaped child leads its own process group,
                // so its ID has not been reused and cannot name ours. The
                // owner lock excludes concurrent kill/reap operations.
                unsafe {
                    libc::killpg(group, libc::SIGKILL);
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    impl Drop for Bridge {
        fn drop(&mut self) {
            terminate(&self.child);
        }
    }

    #[cfg(test)]
    mod batch_tests {
        use super::{Config, MAX_LINE, Source as _, spawn};
        use std::{sync::Arc, time::Duration};

        fn idle_bridge() -> super::Bridge {
            spawn(&Config {
                executable: "/bin/sh".into(),
                args: vec!["-c".into(), "sleep 30".into()],
            })
            .unwrap()
        }

        #[test]
        fn buffered_large_readings_are_split_without_consuming_the_next_line() {
            let mut bridge = idle_bridge();
            let payload = "x".repeat(600_000);
            for signal in ["a", "b"] {
                let reading = serde_json::json!({"signal": signal, "value": payload});
                bridge.pending.extend(serde_json::to_vec(&reading).unwrap());
                bridge.pending.push(b'\n');
            }
            let first = bridge.read(Duration::from_secs(2)).unwrap();
            assert_eq!(first.len(), 1);
            assert_eq!(first[0].signal, "a");
            assert!(serde_json::to_vec(&first).unwrap().len() <= MAX_LINE);
            assert_eq!(bridge.line, 1);
            assert_ne!(bridge.pending, [] as [u8; 0]);
            let second = bridge.read(Duration::from_secs(2)).unwrap();
            assert_eq!(second.len(), 1);
            assert_eq!(second[0].signal, "b");
            assert!(serde_json::to_vec(&second).unwrap().len() <= MAX_LINE);
            assert_eq!(bridge.line, 2);
            assert_eq!(bridge.pending, [] as [u8; 0]);
        }

        #[test]
        fn numeric_normalization_cannot_exceed_the_batch_limit() {
            let mut bridge = idle_bridge();
            let numbers = std::iter::repeat_n("1e9", 220_000)
                .collect::<Vec<_>>()
                .join(",");
            bridge.pending =
                format!("{{\"signal\":\"expanded\",\"value\":[{numbers}]}}\n").into_bytes();
            assert!(bridge.pending.len() < MAX_LINE);
            let error = bridge.read(Duration::from_secs(2)).unwrap_err();
            assert!(error.to_string().contains("serialized reading exceeds"));
        }

        #[test]
        fn helper_numbers_beyond_machine_precision_round_trip_exactly() {
            let mut bridge = idle_bridge();
            let literals = [
                "18446744073709551617",
                "-18446744073709551617",
                "0.123456789012345678901234567890123456789",
                "1.234567890123456789e+100",
            ];
            for (index, value) in literals.iter().enumerate() {
                bridge.pending.extend(
                    format!("{{\"signal\":\"value{index}\",\"value\":{value}}}\n").as_bytes(),
                );
            }
            let readings = bridge.read(Duration::from_secs(2)).unwrap();
            assert_eq!(readings.len(), literals.len());
            for (reading, literal) in readings.iter().zip(literals) {
                assert!(reading.value.is_number());
                assert_eq!(reading.value.to_string(), literal);
            }
        }

        #[test]
        fn cutoff_drains_buffered_readings_before_failing_a_partial_tail() {
            let mut bridge = idle_bridge();
            for index in 0..263 {
                bridge.pending.extend_from_slice(
                    format!("{{\"signal\":\"counter\",\"value\":{index}}}\n").as_bytes(),
                );
            }
            bridge.pending.extend_from_slice(b"{\"signal\":");
            let observed = crate::host::monotonic_ns();
            bridge.observed = observed;
            assert_eq!(bridge.read(Duration::ZERO).unwrap().len(), 256);
            bridge.finish(Duration::ZERO).unwrap();
            for index in 256..263 {
                assert_eq!(bridge.drain().unwrap().unwrap().value, index);
                assert_eq!(bridge.observed_ns(), observed);
            }
            let error = bridge.drain().unwrap_err();
            assert_eq!(
                crate::types::classify(&error).kind,
                crate::types::ErrorKind::Incomplete
            );
        }

        #[test]
        fn a_later_malformed_reading_does_not_discard_an_accepted_reading() {
            let mut bridge = idle_bridge();
            bridge
                .pending
                .extend_from_slice(b"{\"signal\":\"counter\",\"value\":1}\nprivate-secret\n");
            assert_eq!(bridge.read(Duration::ZERO).unwrap()[0].value, 1);
            let error = bridge.read(Duration::ZERO).unwrap_err();
            assert!(!error.to_string().contains("private-secret"));
            assert_eq!(bridge.line, 1);
        }

        #[test]
        fn registered_killer_releases_child_once_and_does_not_keep_it_alive() {
            let bridge = idle_bridge();
            let weak = Arc::downgrade(&bridge.child);
            let mut killer = bridge.killer();
            killer();
            assert_eq!(
                bridge
                    .child
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(std::process::Child::id),
                None
            );
            drop(bridge);
            assert_eq!(weak.strong_count(), 0);
            // The retained signal callback carries only a weak owner, not a
            // PID that could have been reused by an unrelated process.
            killer();
            assert_eq!(weak.strong_count(), 0);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::{Duration, Instant};

    use super::{Config, open};
    use crate::types::{ErrorKind, classify};

    fn config(script: &str) -> Config {
        Config {
            executable: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
        }
    }

    #[test]
    fn reads_multiple_bounded_json_lines_before_eof() {
        let mut source = open(&config(
            "printf '%s\\n' '{\"signal\":\"pressure\",\"value\":12.5}' '{\"signal\":\"ready\",\"value\":true}'",
        ))
        .unwrap();
        let mut readings = Vec::new();
        while readings.len() < 2 {
            readings.extend(source.read(Duration::from_secs(2)).unwrap());
        }
        assert_eq!(readings.len(), 2);
        assert_eq!(readings[0].signal, "pressure");
        assert_eq!(readings[0].value, serde_json::json!(12.5));
        assert_eq!(readings[1].signal, "ready");
        assert_eq!(readings[1].value, serde_json::json!(true));
        let error = source.read(Duration::from_secs(2)).unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::Disconnected);
    }

    #[test]
    fn acquisition_starts_at_first_read() {
        let mut source = open(&Config {
            executable: "/capturefab-fixture/nonexistent-industrial-helper".into(),
            args: Vec::new(),
        })
        .unwrap();
        let error = source.read(Duration::from_secs(1)).unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::Unavailable);
    }

    #[test]
    fn idle_helper_read_and_drop_do_not_wait_for_its_sleep() {
        let started = Instant::now();
        let mut source = open(&config("sleep 30")).unwrap();
        assert!(source.read(Duration::from_millis(20)).unwrap().is_empty());
        drop(source);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn malformed_output_is_an_error_without_echoing_secrets() {
        let mut source = open(&config("printf '%s\\n' secret-token")).unwrap();
        let error = source.read(Duration::from_secs(2)).unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::OperationFailed);
        assert!(!error.to_string().contains("secret-token"));
    }

    #[test]
    fn incomplete_final_line_is_not_silently_accepted() {
        let mut source = open(&config("printf '%s' '{\"signal\":\"x\",\"value\":1}'")).unwrap();
        let error = source.read(Duration::from_secs(2)).unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::Disconnected);
        assert!(error.to_string().contains("incomplete JSON line"));
    }

    #[test]
    fn oversized_line_is_rejected_without_unbounded_buffering() {
        let mut source = open(&config("head -c 1048577 /dev/zero")).unwrap();
        let error = source.read(Duration::from_secs(2)).unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::OperationFailed);
        assert!(error.to_string().contains("exceeds 1048576 bytes"));
    }
}
