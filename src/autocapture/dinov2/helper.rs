//! `capturefab __embed`: DINOv2 on the GPU in an isolated helper process.
//!
//! A driver fault or a hang inside the GPU driver must not freeze or crash the
//! camera worker, so the worker only talks to this process over pipes: a JSON
//! hello line (adapter or the reason the GPU is unusable), then for each frame
//! the `input x input` RGB bytes in and a status byte plus the unit-length
//! embedding out. Every request has a deadline; a helper that misses it is
//! abandoned and the session continues on the CPU. Both sides decode the same
//! bundled weights, and the hello carries their fingerprint to prove it.
use super::{Model, gpu::Gpu};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

pub const ARGUMENT: &str = "__embed";
/// `CAPTUREFAB_DINOV2_GPU=0` keeps DINOv2 on the CPU.
pub const DISABLE: &str = "CAPTUREFAB_DINOV2_GPU";
/// Device creation and shader compilation; healthy GPUs take well under this.
const STARTUP: Duration = Duration::from_secs(20);

#[derive(Serialize, Deserialize, Default)]
struct Hello {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    adapter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub fn wanted() -> bool {
    std::env::var_os(DISABLE).is_none_or(|value| value != "0")
}

fn executable() -> std::path::PathBuf {
    std::env::var_os("CAPTUREFAB_EXECUTABLE")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| "capturefab".into())
}

/// Hidden `__embed` mode: embed frames from stdin until it closes.
pub fn run_helper() -> Result<()> {
    let mut output = std::io::stdout().lock();
    let started = (|| -> Result<_> {
        let model = super::bundled()?.context(super::MISSING)?;
        let size = model.input();
        let gpu = Gpu::new(&model, size, size)?;
        Ok((model, gpu))
    })();
    let (model, mut gpu) = match started {
        Ok(ready) => ready,
        Err(error) => {
            return send_hello(
                &mut output,
                &Hello {
                    error: Some(format!("{error:#}")),
                    ..Hello::default()
                },
            );
        }
    };
    send_hello(
        &mut output,
        &Hello {
            ok: true,
            adapter: Some(gpu.adapter().into()),
            fingerprint: Some(model.fingerprint().into()),
            error: None,
        },
    )?;
    let size = model.input();
    let mut input = std::io::stdin().lock();
    let mut rgb = vec![0_u8; size * size * 3];
    loop {
        match input.read_exact(&mut rgb) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        match gpu.embed_rgb(&model, &rgb) {
            Ok(embedding) => {
                output.write_all(&[0])?;
                for value in embedding {
                    output.write_all(&value.to_le_bytes())?;
                }
            }
            Err(error) => {
                let message = format!("{error:#}");
                output.write_all(&[1])?;
                output.write_all(&(message.len() as u32).to_le_bytes())?;
                output.write_all(message.as_bytes())?;
            }
        }
        output.flush()?;
    }
}

fn send_hello(output: &mut impl Write, hello: &Hello) -> Result<()> {
    serde_json::to_writer(&mut *output, hello)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

/// A ready GPU helper, owned by one session's encoder thread.
pub struct Client {
    child: Option<Child>,
    jobs: mpsc::Sender<Vec<u8>>,
    results: mpsc::Receiver<Result<Vec<f32>, String>>,
    adapter: String,
}

impl Client {
    /// Start a helper and wait until its GPU is ready, for `model`'s weights.
    pub fn start(model: &Model) -> Result<Self> {
        let mut child = crate::session::spawn_helper(
            Command::new(executable())
                .arg(ARGUMENT)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit()),
        )
        .context("launch DINOv2 GPU helper")?;
        let input = child.stdin.take().context("helper stdin")?;
        let output = BufReader::new(child.stdout.take().context("helper stdout")?);
        let (hello_sender, hello) = mpsc::channel();
        let (jobs, job_queue) = mpsc::channel::<Vec<u8>>();
        let (result_sender, results) = mpsc::channel();
        let dims = model.dims();
        // The I/O thread owns both pipes, so a stalled helper blocks only it.
        thread::Builder::new()
            .name("capturefab-embed-helper".into())
            .spawn(move || io(input, output, dims, hello_sender, job_queue, result_sender))
            .context("start DINOv2 helper I/O")?;
        let ready = match hello.recv_timeout(STARTUP) {
            Ok(Ok(hello)) => Ok(hello),
            Ok(Err(error)) => Err(anyhow!(error)),
            Err(_) => Err(anyhow!("GPU helper did not start within {STARTUP:?}")),
        }
        .and_then(|hello: Hello| {
            ensure!(
                hello.ok,
                "{}",
                hello.error.unwrap_or_else(|| "GPU unavailable".into())
            );
            ensure!(
                hello.fingerprint.as_deref() == Some(model.fingerprint()),
                "GPU helper bundles different DINOv2 weights"
            );
            Ok(hello.adapter.unwrap_or_default())
        });
        match ready {
            Ok(adapter) => Ok(Self {
                child: Some(child),
                jobs,
                results,
                adapter,
            }),
            Err(error) => {
                abandon(child);
                Err(error)
            }
        }
    }

    pub fn adapter(&self) -> &str {
        &self.adapter
    }

    /// One embedding, or an error after which the helper must not be reused.
    pub fn embed(&mut self, rgb: Vec<u8>, deadline: Duration) -> Result<Vec<f32>> {
        self.jobs
            .send(rgb)
            .map_err(|_| anyhow!("GPU helper I/O stopped"))?;
        match self.results.recv_timeout(deadline) {
            Ok(Ok(embedding)) => Ok(embedding),
            Ok(Err(error)) => bail!("{error}"),
            Err(_) => bail!("GPU embedding exceeded its {deadline:?} deadline"),
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            abandon(child);
        }
    }
}

fn io(
    mut input: ChildStdin,
    mut output: BufReader<std::process::ChildStdout>,
    dims: usize,
    hello: mpsc::Sender<Result<Hello, String>>,
    jobs: mpsc::Receiver<Vec<u8>>,
    results: mpsc::Sender<Result<Vec<f32>, String>>,
) {
    let mut line = Vec::new();
    let first = output
        .by_ref()
        .take(64 * 1024)
        .read_until(b'\n', &mut line)
        .map_err(|e| e.to_string())
        .and_then(|_| {
            serde_json::from_slice::<Hello>(&line)
                .map_err(|_| "GPU helper did not report readiness".to_string())
        });
    let ready = first.as_ref().is_ok_and(|hello| hello.ok);
    if hello.send(first).is_err() || !ready {
        return;
    }
    for rgb in jobs {
        let result = (|| -> Result<Vec<f32>> {
            input.write_all(&rgb)?;
            input.flush()?;
            let mut status = [0_u8];
            output.read_exact(&mut status)?;
            if status[0] != 0 {
                let mut length = [0_u8; 4];
                output.read_exact(&mut length)?;
                let length = u32::from_le_bytes(length).min(64 * 1024) as usize;
                let mut message = vec![0; length];
                output.read_exact(&mut message)?;
                bail!("{}", String::from_utf8_lossy(&message));
            }
            let mut bytes = vec![0_u8; dims * 4];
            output.read_exact(&mut bytes)?;
            Ok(bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect())
        })();
        let failed = result.is_err();
        if results.send(result.map_err(|e| format!("{e:#}"))).is_err() || failed {
            return;
        }
    }
}

/// Stop a helper without waiting on the caller's thread: a process stuck in
/// a GPU driver may not exit even after SIGKILL.
fn abandon(mut child: Child) {
    let _ = child.kill();
    let _ = thread::Builder::new()
        .name("capturefab-embed-reap".into())
        .spawn(move || child.wait());
}

/// Start a helper and report its adapter, for `doctor`.
pub fn probe(model: &Model) -> Result<String> {
    if !wanted() {
        bail!("disabled by {DISABLE}=0");
    }
    Client::start(model).map(|client| client.adapter().to_owned())
}
