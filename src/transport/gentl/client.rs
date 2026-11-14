//! One bounded I/O operation at a time. A hung DLL cannot block the camera
//! owner: timeout kills the isolated helper and permanently retires this proxy.
use super::{ARG, CLEANUP_TIMEOUT, process, wire};
use crate::types::{Categorized, ErrorKind};
use anyhow::{Context as _, Result};
use std::{
    io::Read as _,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

struct Call {
    request: wire::Request,
    payload: Vec<u8>,
    maximum: usize,
    response: mpsc::Sender<(Instant, Result<wire::Response>)>,
}

#[derive(Debug, PartialEq, Eq)]
enum WaitError {
    Cancelled,
    Timeout,
    PipeClosed,
}

fn wait_for_reply<T>(
    receiver: &mpsc::Receiver<(Instant, T)>,
    deadline: Instant,
    cancelled: Option<&AtomicBool>,
) -> Result<T, WaitError> {
    let (completed, reply) = loop {
        let stopped = cancelled.is_some_and(|flag| flag.load(Ordering::Acquire));
        let left = deadline.saturating_duration_since(Instant::now());
        if stopped || left.is_zero() {
            // The pipe reader may have already completed this reply while the
            // owner was descheduled. Use it without waiting beyond the budget.
            break receiver.try_recv().map_err(|_| {
                if stopped {
                    WaitError::Cancelled
                } else {
                    WaitError::Timeout
                }
            })?;
        }
        let wait = if cancelled.is_some() {
            left.min(Duration::from_millis(10))
        } else {
            left
        };
        match receiver.recv_timeout(wait) {
            Ok(completed) => break completed,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(WaitError::PipeClosed),
        }
    };
    if completed > deadline {
        return Err(WaitError::Timeout);
    }
    Ok(reply)
}

pub(super) struct Client {
    child: Option<Child>,
    tree: process::Tree,
    calls: mpsc::SyncSender<Call>,
    tail: Arc<Mutex<Vec<u8>>>,
}
impl Client {
    pub(super) fn spawn(deadline: Instant) -> Result<Self> {
        let executable = std::env::var_os("CAPTUREFAB_EXECUTABLE").map_or_else(
            || {
                if cfg!(target_os = "linux") {
                    Ok(std::path::PathBuf::from("/proc/self/exe"))
                } else {
                    std::env::current_exe()
                }
            },
            |path| Ok(path.into()),
        )?;
        Self::spawn_command(Command::new(executable), deadline)
    }
    fn spawn_command(mut command: Command, deadline: Instant) -> Result<Self> {
        command
            .arg(ARG)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        process::prepare(&mut command);
        let mut child = command.spawn().context("start the GenTL helper")?;
        let tree = match process::Tree::adopt(&child) {
            Ok(tree) => tree,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let mut input = child.stdin.take().context("GenTL helper stdin missing")?;
        let mut output = child.stdout.take().context("GenTL helper stdout missing")?;
        let mut errors = child.stderr.take().context("GenTL helper stderr missing")?;
        let tail = Arc::new(Mutex::new(Vec::new()));
        let (calls, receiver) = mpsc::sync_channel::<Call>(1);
        let mut client = Self {
            child: Some(child),
            tree,
            calls,
            tail: tail.clone(),
        };
        if let Err(error) = std::thread::Builder::new()
            .name("capturefab-gentl-log".into())
            .spawn(move || {
                let mut bytes = [0; 4096];
                loop {
                    match errors.read(&mut bytes) {
                        Ok(0) => break,
                        Ok(count) => {
                            let mut tail = tail
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            tail.extend_from_slice(&bytes[..count]);
                            if tail.len() > 16 * 1024 {
                                let extra = tail.len() - 16 * 1024;
                                tail.drain(..extra);
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
            })
        {
            client.terminate();
            return Err(error.into());
        }
        if let Err(error) = std::thread::Builder::new()
            .name("capturefab-gentl-pipe".into())
            .spawn(move || {
                while let Ok(call) = receiver.recv() {
                    let reply = wire::write_request(&mut input, &call.request, &call.payload)
                        .and_then(|()| wire::read_reply(&mut output, call.maximum));
                    let completed = Instant::now();
                    let failed = reply.is_err();
                    let _ = call.response.send((completed, reply));
                    if failed {
                        break;
                    }
                }
            })
        {
            client.terminate();
            return Err(error.into());
        }
        if Instant::now() >= deadline {
            client.terminate();
            return Err(Categorized::error(
                ErrorKind::Timeout,
                "GenTL helper startup reached the operation deadline",
            ));
        }
        let greeting = (|| {
            let reply = client.call(
                wire::Request::Hello {
                    protocol: wire::PROTOCOL,
                },
                Vec::new(),
                deadline,
                0,
            )?;
            let hello: wire::Hello = serde_json::from_value(reply.reply.value)
                .context("invalid GenTL helper protocol greeting")?;
            anyhow::ensure!(
                hello.protocol == wire::PROTOCOL,
                Categorized::error(
                    ErrorKind::Unsupported,
                    format!(
                        "GenTL helper speaks protocol {}, not {}",
                        hello.protocol,
                        wire::PROTOCOL
                    )
                )
            );
            Ok(())
        })();
        if let Err(error) = greeting {
            // An incompatible helper must not receive even a cleanup command.
            client.terminate();
            return Err(error);
        }
        Ok(client)
    }
    pub(super) fn call(
        &mut self,
        request: wire::Request,
        payload: Vec<u8>,
        deadline: Instant,
        maximum: usize,
    ) -> Result<wire::Response> {
        self.call_inner(request, payload, deadline, maximum, None)
    }
    pub(super) fn discover(
        &mut self,
        producer: std::path::PathBuf,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<wire::Response> {
        self.call_inner(
            wire::Request::Discover {
                producer,
                timeout_ms: super::millis(deadline.saturating_duration_since(Instant::now())),
            },
            Vec::new(),
            deadline,
            0,
            Some(cancelled),
        )
    }
    fn call_inner(
        &mut self,
        request: wire::Request,
        payload: Vec<u8>,
        deadline: Instant,
        maximum: usize,
        cancelled: Option<&AtomicBool>,
    ) -> Result<wire::Response> {
        if Instant::now() >= deadline {
            self.terminate();
            return Err(Categorized::error(
                ErrorKind::Timeout,
                "GenTL operation deadline expired before queuing",
            ));
        }
        if self.child.is_none() {
            return Err(Categorized::error(
                ErrorKind::Disconnected,
                "GenTL helper is no longer running",
            ));
        }
        let (sender, receiver) = mpsc::channel();
        if self
            .calls
            .try_send(Call {
                request,
                payload,
                maximum,
                response: sender,
            })
            .is_err()
        {
            self.terminate();
            return Err(Categorized::error(
                ErrorKind::WorkerCrashed,
                "GenTL helper control channel closed",
            ));
        }
        let response = match wait_for_reply(&receiver, deadline, cancelled) {
            Ok(Ok(response)) => response,
            Err(WaitError::Cancelled) => {
                self.terminate();
                return Err(Categorized::error(
                    ErrorKind::Interrupted,
                    "GenTL discovery cancelled",
                ));
            }
            failure => {
                let kind = if matches!(failure, Err(WaitError::Timeout)) {
                    ErrorKind::Timeout
                } else {
                    ErrorKind::WorkerCrashed
                };
                self.terminate();
                let tail = self
                    .tail
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let text = String::from_utf8_lossy(&tail);
                let detail = crate::scrub::scrub(&text);
                return Err(Categorized::error(
                    kind,
                    format!(
                        "GenTL helper did not complete its bounded operation: {}",
                        crate::types::printable_text(&detail)
                    ),
                ));
            }
        };
        if !response.reply.ok {
            return Err(crate::types::error_from_tag(
                response.reply.tag,
                response
                    .reply
                    .error
                    .unwrap_or_else(|| "GenTL helper rejected the request".into()),
            ));
        }
        Ok(response)
    }
    fn terminate(&mut self) {
        if let Some(mut child) = self.child.take() {
            self.tree.terminate();
            let _ = child.kill();
            // Reaping a process stuck in a vendor kernel driver must not hang
            // its camera worker. One retired helper can own only one I/O job.
            let _ = std::thread::Builder::new()
                .name("capturefab-gentl-reaper".into())
                .spawn(move || {
                    let _ = child.wait();
                });
        }
    }
    pub(super) fn close(&mut self, deadline: Instant) -> Result<()> {
        if self.child.is_some() {
            let result = self.call(wire::Request::Close, Vec::new(), deadline, 0);
            self.terminate();
            result?;
        }
        Ok(())
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.close(Instant::now() + CLEANUP_TIMEOUT);
    }
}

#[cfg(test)]
mod tests {
    use super::{AtomicBool, Duration, Instant, WaitError, mpsc, wait_for_reply};

    #[test]
    fn queued_on_time_replies_survive_deadline_and_cancellation() {
        let deadline = Instant::now();
        let cancelled = AtomicBool::new(true);
        for flag in [None, Some(&cancelled)] {
            let (sender, receiver) = mpsc::channel();
            sender.send((deadline, 42)).unwrap();
            assert_eq!(wait_for_reply(&receiver, deadline, flag).unwrap(), 42);
        }
    }

    #[test]
    fn queued_late_replies_do_not_extend_the_deadline() {
        let deadline = Instant::now();
        let (sender, receiver) = mpsc::channel();
        sender
            .send((deadline + Duration::from_nanos(1), 42))
            .unwrap();
        assert_eq!(
            wait_for_reply(&receiver, deadline, None).unwrap_err(),
            WaitError::Timeout
        );
    }

    #[test]
    fn empty_reply_queue_cannot_delay_timeout_or_cancellation() {
        let (_sender, receiver) = mpsc::channel::<(Instant, u8)>();
        let now = Instant::now();
        assert_eq!(
            wait_for_reply(&receiver, now, None).unwrap_err(),
            WaitError::Timeout
        );
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            wait_for_reply(&receiver, now + Duration::from_secs(1), Some(&cancelled)).unwrap_err(),
            WaitError::Cancelled
        );
    }
}

#[cfg(all(test, unix))]
mod protocol_tests {
    use super::*;

    #[test]
    fn incompatible_helper_protocol_is_rejected_before_any_device_request() {
        use std::fmt::Write as _;
        for version in [3, 5] {
            let mut packet = Vec::new();
            wire::write_reply(
                &mut packet,
                &wire::Reply::ok(serde_json::json!({"protocol":version})),
                &[],
            )
            .unwrap();
            let mut octal = String::with_capacity(packet.len() * 4);
            for byte in packet {
                write!(&mut octal, "\\{byte:03o}").unwrap();
            }
            let mut command = Command::new("/bin/sh");
            command.args([
                "-c",
                "printf '%b' \"$1\"; exec sleep 30",
                "other-gentl",
                &octal,
            ]);
            let began = Instant::now();
            let Err(error) = Client::spawn_command(command, began + Duration::from_secs(2)) else {
                panic!("accepted GenTL protocol {version}");
            };
            assert_eq!(crate::types::classify(&error).kind, ErrorKind::Unsupported);
            assert!(
                format!("{error:#}").contains(&format!("protocol {version}, not 4")),
                "{error:#}"
            );
            assert!(began.elapsed() < Duration::from_secs(3));
        }
    }
}
