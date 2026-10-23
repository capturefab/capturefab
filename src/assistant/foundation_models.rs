//! Host-provided, on-device generation. A bundled helper isolates Apple's
//! Swift framework from the application and never invokes a remote endpoint.
use super::{ProviderError, ProviderErrorKind};
use serde_json::Value;
use std::{sync::atomic::AtomicBool, time::Instant};

fn failure(kind: ProviderErrorKind, message: &str) -> ProviderError {
    ProviderError::local(kind, message)
}

/// Checks the OS model without generating text, reading credentials, or
/// downloading a model. Call only from a worker, never the UI thread.
pub fn availability() -> Result<(), ProviderError> {
    #[cfg(all(target_os = "macos", foundation_models_native))]
    {
        let response = native::invoke(None, &AtomicBool::new(false), None)?;
        if response["available"] == true {
            Ok(())
        } else {
            Err(failure(
                ProviderErrorKind::Malformed,
                "The local model helper returned an invalid availability response.",
            ))
        }
    }
    #[cfg(not(all(target_os = "macos", foundation_models_native)))]
    {
        Err(unsupported())
    }
}

/// Accept the existing bounded Chat request contract. The caller parses the
/// response through the ordinary Chat adapter and retains every tool approval.
pub fn generate(
    body: &[u8],
    cancel: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<Value, ProviderError> {
    #[cfg(all(target_os = "macos", foundation_models_native))]
    {
        native::invoke(Some(body), cancel, deadline)
    }
    #[cfg(not(all(target_os = "macos", foundation_models_native)))]
    {
        let _ = (body, cancel, deadline);
        Err(unsupported())
    }
}

#[cfg(not(all(target_os = "macos", foundation_models_native)))]
fn unsupported() -> ProviderError {
    failure(
        ProviderErrorKind::Unavailable,
        if cfg!(target_os = "macos") {
            "This build does not include Apple Foundation Models. Use a build made with the macOS 26 or newer SDK."
        } else {
            "Apple Foundation Models requires macOS 26 or later on a Mac that supports Apple Intelligence."
        },
    )
}

#[cfg(any(test, all(target_os = "macos", foundation_models_native)))]
fn decode(bytes: &[u8]) -> Result<Value, ProviderError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| {
        failure(
            ProviderErrorKind::Malformed,
            "The local model helper returned an invalid response.",
        )
    })?;
    if let Some(error) = value.get("error") {
        let kind = match error["kind"].as_str() {
            Some("unavailable") => ProviderErrorKind::Unavailable,
            Some("rejected") => ProviderErrorKind::Rejected,
            Some("rate_limited") => ProviderErrorKind::RateLimited,
            _ => ProviderErrorKind::Malformed,
        };
        return Err(failure(
            kind,
            &error["message"]
                .as_str()
                .unwrap_or("The local model could not complete this request.")
                .chars()
                .take(1_000)
                .collect::<String>(),
        ));
    }
    Ok(value)
}

#[cfg(all(target_os = "macos", foundation_models_native))]
mod native {
    use super::*;
    use std::{
        fs::{self, OpenOptions},
        io::{ErrorKind, Read, Write},
        os::{
            fd::AsRawFd,
            unix::fs::{DirBuilderExt, OpenOptionsExt},
        },
        path::PathBuf,
        process::{Child, Command, Stdio},
        sync::atomic::Ordering,
        thread,
        time::Duration,
    };

    const HELPER: &[u8] = include_bytes!(concat!(
        env!("OUT_DIR"),
        "/capturefab-foundation-models.bin"
    ));
    const MAX_OUTPUT: usize = 1024 * 1024;

    struct Executable {
        directory: PathBuf,
        path: PathBuf,
    }
    impl Executable {
        fn create(bytes: &[u8]) -> Result<Self, ProviderError> {
            let unavailable = || {
                failure(
                    ProviderErrorKind::Unavailable,
                    "Could not prepare the bundled local model helper.",
                )
            };
            let mut nonce = [0u8; 16];
            getrandom::fill(&mut nonce).map_err(|_| unavailable())?;
            let nonce: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
            let directory =
                std::env::temp_dir().join(format!("capturefab-foundation-models-{nonce}"));
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&directory)
                .map_err(|_| unavailable())?;
            let executable = Self {
                path: directory.join("model"),
                directory,
            };
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o700)
                .open(&executable.path)
                .and_then(|mut file| file.write_all(bytes))
                .map_err(|_| unavailable())?;
            Ok(executable)
        }
    }
    impl Drop for Executable {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
            let _ = fs::remove_dir(&self.directory);
        }
    }

    struct Process(Child);
    impl Drop for Process {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn supported_os() -> bool {
        let mut bytes = [0u8; 64];
        let mut length = bytes.len();
        // A read-only OS property avoids launching a second program or relying
        // on mutable PATH/environment settings to identify the host version.
        let result = unsafe {
            libc::sysctlbyname(
                c"kern.osproductversion".as_ptr(),
                bytes.as_mut_ptr().cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        result == 0
            && std::str::from_utf8(&bytes[..length.min(bytes.len())])
                .ok()
                .and_then(|version| version.split('.').next()?.parse::<u32>().ok())
                .is_some_and(|major| major >= 26)
    }

    fn nonblocking(pipe: &impl AsRawFd) -> Result<(), ProviderError> {
        let descriptor = pipe.as_raw_fd();
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
        if flags == -1
            || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return Err(failure(
                ProviderErrorKind::Unavailable,
                "Could not open local model communication pipes.",
            ));
        }
        Ok(())
    }

    pub(super) fn invoke(
        body: Option<&[u8]>,
        cancel: &AtomicBool,
        deadline: Option<Instant>,
    ) -> Result<Value, ProviderError> {
        if !supported_os() {
            return Err(failure(
                ProviderErrorKind::Unavailable,
                "Apple Foundation Models requires macOS 26 or later.",
            ));
        }
        let executable = Executable::create(HELPER)?;
        execute(&executable, body, cancel, deadline)
    }

    fn execute(
        executable: &Executable,
        body: Option<&[u8]>,
        cancel: &AtomicBool,
        deadline: Option<Instant>,
    ) -> Result<Value, ProviderError> {
        let deadline = deadline.unwrap_or_else(|| {
            Instant::now() + Duration::from_secs(if body.is_some() { 120 } else { 10 })
        });
        let interrupted = || cancel.load(Ordering::Relaxed) || Instant::now() >= deadline;
        let timeout = || {
            failure(
                ProviderErrorKind::Timeout,
                "The local model request was cancelled or timed out.",
            )
        };
        if interrupted() {
            return Err(timeout());
        }
        if body.is_some_and(|body| body.len() > super::super::protocol::MAX_REQUEST_BYTES) {
            return Err(failure(
                ProviderErrorKind::Rejected,
                "The local model request is too large.",
            ));
        }
        let mut command = Command::new(&executable.path);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if body.is_none() {
            command.arg("--availability");
        }
        let mut process = Process(command.spawn().map_err(|_| {
            failure(
                ProviderErrorKind::Unavailable,
                "Could not start the bundled local model helper.",
            )
        })?);
        let mut stdin = process.0.stdin.take();
        let mut stdout = process.0.stdout.take().expect("piped local model output");
        nonblocking(stdin.as_ref().expect("piped local model input"))?;
        nonblocking(&stdout)?;
        let input = body.unwrap_or_default();
        let mut written = 0;
        let mut output = Vec::new();
        let mut exited = None;
        let mut eof = false;
        loop {
            if interrupted() {
                return Err(timeout());
            }
            if let Some(pipe) = &mut stdin {
                if written == input.len() {
                    stdin.take();
                } else {
                    match pipe.write(&input[written..]) {
                        Ok(0) => {
                            stdin.take();
                        }
                        Ok(count) => written += count,
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                        Err(error) if error.kind() == ErrorKind::Interrupted => {}
                        Err(_) => {
                            stdin.take();
                        }
                    }
                }
            }
            let mut buffer = [0u8; 8192];
            loop {
                match stdout.read(&mut buffer) {
                    Ok(0) => {
                        eof = true;
                        break;
                    }
                    Ok(count) => {
                        if output.len() + count > MAX_OUTPUT {
                            return Err(failure(
                                ProviderErrorKind::CutOff,
                                "The local model response exceeded its size limit.",
                            ));
                        }
                        output.extend_from_slice(&buffer[..count]);
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => {
                        return Err(failure(
                            ProviderErrorKind::Malformed,
                            "Could not read the local model response.",
                        ));
                    }
                }
            }
            if exited.is_none() {
                exited = process.0.try_wait().map_err(|_| {
                    failure(
                        ProviderErrorKind::Unavailable,
                        "Could not wait for the local model helper.",
                    )
                })?;
            }
            if let Some(status) = exited.filter(|_| eof) {
                if !status.success() {
                    return Err(failure(
                        ProviderErrorKind::Unavailable,
                        "The local model helper exited unexpectedly. Check Apple Intelligence in System Settings.",
                    ));
                }
                return decode(&output);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn cancelled_native_request_does_not_start_helper() {
            let helper = Executable::create(b"#!/bin/sh\nexit 2\n").unwrap();
            let error = execute(&helper, Some(b"{}"), &AtomicBool::new(true), None).unwrap_err();
            assert_eq!(error.kind, ProviderErrorKind::Timeout);
        }

        #[test]
        fn native_helper_is_bounded_and_cancellable_while_reading_input() {
            let helper = Executable::create(b"#!/bin/sh\nexec sleep 10\n").unwrap();
            let now = Instant::now();
            let body = vec![b' '; 128 * 1024];
            let error = execute(
                &helper,
                Some(&body),
                &AtomicBool::new(false),
                Some(now + Duration::from_millis(30)),
            )
            .unwrap_err();
            assert_eq!(error.kind, ProviderErrorKind::Timeout);
            assert!(now.elapsed() < Duration::from_secs(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_errors_keep_actionable_native_reason() {
        let error =
            decode(br#"{"error":{"kind":"unavailable","message":"Turn on Apple Intelligence."}}"#)
                .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::Unavailable);
        assert_eq!(
            error.excerpt.as_deref(),
            Some("Turn on Apple Intelligence.")
        );
    }

    #[test]
    fn invalid_native_output_never_becomes_a_chat_reply() {
        assert_eq!(
            decode(b"not JSON").unwrap_err().kind,
            ProviderErrorKind::Malformed
        );
        assert_eq!(
            decode(br#"{"error":{"kind":"invented"}}"#)
                .unwrap_err()
                .kind,
            ProviderErrorKind::Malformed
        );
    }
}
