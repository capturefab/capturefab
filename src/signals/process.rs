//! Parent side of a `__signal` helper: spawning it with the spec on stdin,
//! reading its bounded NDJSON lines, and one-shot `read` and `browse`.
use super::{BrowseItem, Line, Mode, Protocol, Sample, SignalSpec, TagInfo, parse};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value as Json, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

/// Longest line accepted from a helper.
const MAX_LINE: u64 = 1024 * 1024;

pub struct SignalProcess {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl SignalProcess {
    /// Start a helper and send it the spec; returns the stdout reader too.
    pub fn spawn(spec: &SignalSpec) -> Result<(Self, BufReader<ChildStdout>)> {
        let executable = std::env::var_os("CAPTUREFAB_EXECUTABLE")
            .map(PathBuf::from)
            .unwrap_or(std::env::current_exe()?);
        let mut child = crate::accel::spawn_helper(
            Command::new(executable)
                .arg("__signal")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit()),
        )
        .context("launch signal helper process")?;
        let stdout = child.stdout.take().context("signal helper stdout")?;
        let mut process = Self {
            stdin: child.stdin.take(),
            child,
        };
        let mut line = serde_json::to_vec(spec)?;
        line.push(b'\n');
        if let Some(stdin) = process.stdin.as_mut() {
            stdin
                .write_all(&line)
                .and_then(|()| stdin.flush())
                .context("send signal spec")?;
        }
        Ok((process, BufReader::new(stdout)))
    }
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
    /// Ask the helper to finish its current round and exit.
    pub fn request_stop(&mut self) {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = stdin
                .write_all(b"{\"op\":\"stop\"}\n")
                .and_then(|()| stdin.flush());
        }
    }
    /// Wait for the helper to exit until `deadline`, then kill it.
    pub fn wait_or_kill(&mut self, deadline: Instant) {
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return;
                }
                Ok(None) => thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

impl Drop for SignalProcess {
    fn drop(&mut self) {
        self.request_stop();
        self.wait_or_kill(Instant::now() + Duration::from_secs(2));
        // Closing stdin makes a helper that missed `stop` exit too.
        self.stdin.take();
    }
}

/// Read one helper line; `None` at EOF.
pub fn read_line(reader: &mut impl BufRead) -> Result<Option<Line>> {
    let mut data = Vec::new();
    reader.take(MAX_LINE + 1).read_until(b'\n', &mut data)?;
    if data.is_empty() {
        return Ok(None);
    }
    ensure!(
        data.len() as u64 <= MAX_LINE,
        "signal helper line exceeds 1 MiB"
    );
    Ok(Some(
        serde_json::from_slice(&data).context("signal helper wrote an invalid line")?,
    ))
}

/// Where OPC UA keeps certificates: a private directory in the session dir.
pub fn pki_dir() -> Option<PathBuf> {
    let base = crate::ipc::session_dir();
    let signals = base.join("signals");
    let pki = signals.join("opcua-pki");
    for dir in [&base, &signals, &pki] {
        crate::ipc::ensure_private_dir(dir).ok()?;
    }
    Some(pki)
}

/// Run a one-shot helper until `done` or `error`, or kill it at `deadline_ms`.
fn one_shot(spec: SignalSpec, deadline_ms: u64) -> Result<(Vec<Line>, u64)> {
    let (process, mut reader) = SignalProcess::spawn(&spec)?;
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("capturefab-signal-read".into())
        .spawn(move || {
            loop {
                let line = read_line(&mut reader);
                let end = !matches!(line, Ok(Some(_)));
                if sender.send(line).is_err() || end {
                    return;
                }
            }
        })?;
    let mut process = process;
    let deadline = Instant::now() + Duration::from_millis(deadline_ms);
    let mut lines = Vec::new();
    let mut dropped = 0;
    let password = parse(&spec.uri).ok().and_then(|u| u.password);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let line = match receiver.recv_timeout(remaining) {
            Ok(line) => line?,
            Err(_) => {
                process.wait_or_kill(Instant::now());
                bail!("signal source timed out after {deadline_ms} ms");
            }
        };
        match line {
            None => bail!("signal helper exited without a result"),
            Some(Line::Done { truncated }) => {
                lines.push(Line::Done { truncated });
                break;
            }
            Some(Line::Error { message }) => {
                // The helper redacts; redact again in case it did not.
                bail!(
                    "{}",
                    super::redact_text(&message, &spec.uri, password.as_deref())
                )
            }
            Some(Line::Dropped { count }) => dropped += count,
            Some(line) => lines.push(line),
        }
    }
    process.wait_or_kill(Instant::now() + Duration::from_secs(2));
    Ok((lines, dropped))
}

fn spec(uri: &str, mode: Mode, timeout_ms: u64, protocol: Protocol) -> SignalSpec {
    SignalSpec {
        uri: uri.to_string(),
        mode,
        interval_ms: super::default_interval_ms(),
        tags: Vec::new(),
        timeout_ms,
        pki_dir: (protocol == Protocol::Opcua).then(pki_dir).flatten(),
    }
}

/// `signal_read`: one round of current values.
pub fn read(uri: &str, timeout_ms: u64) -> Result<Json> {
    let parsed = parse(uri)?;
    super::require_compiled(parsed.protocol)?;
    let (lines, dropped) = one_shot(
        spec(uri, Mode::Read, timeout_ms, parsed.protocol),
        timeout_ms + 5000,
    )?;
    let mut tags: Vec<TagInfo> = Vec::new();
    let mut samples: Vec<Sample> = Vec::new();
    for line in lines {
        match line {
            Line::Ready { tags: t } => tags.extend(t),
            Line::Samples { samples: s } => samples.extend(s),
            _ => {}
        }
    }
    Ok(json!({
        "id": super::source_id(uri),
        "uri": crate::media::redact_url(uri),
        "protocol": parsed.protocol,
        "tags": tags,
        "samples": samples,
        "dropped": dropped,
    }))
}

/// `signal_browse`: nodes, topics or data items, at most `limit`.
pub fn browse(uri: &str, duration_ms: u64, limit: u32) -> Result<Json> {
    let parsed = parse(uri)?;
    super::require_compiled(parsed.protocol)?;
    if parsed.protocol == Protocol::ModbusTcp {
        bail!("browse is not supported for modbus-tcp sources");
    }
    let timeout_ms = super::default_timeout_ms();
    let (lines, _) = one_shot(
        spec(
            uri,
            Mode::Browse { duration_ms, limit },
            timeout_ms,
            parsed.protocol,
        ),
        duration_ms + timeout_ms + 5000,
    )?;
    let mut items: Vec<BrowseItem> = Vec::new();
    let mut truncated = false;
    for line in lines {
        match line {
            Line::Browse { items: i } => items.extend(i),
            Line::Done { truncated: t } => truncated |= t,
            _ => {}
        }
    }
    if items.len() > limit as usize {
        items.truncate(limit as usize);
        truncated = true;
    }
    Ok(json!({
        "id": super::source_id(uri),
        "uri": crate::media::redact_url(uri),
        "protocol": parsed.protocol,
        "items": items,
        "truncated": truncated,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_oversized_lines() {
        let mut long = vec![b'x'; MAX_LINE as usize + 10];
        long.push(b'\n');
        assert!(read_line(&mut &long[..]).is_err());
        let ok = b"{\"t\":\"done\",\"truncated\":true}\n";
        assert_eq!(
            read_line(&mut &ok[..]).unwrap(),
            Some(Line::Done { truncated: true })
        );
        assert_eq!(read_line(&mut &b""[..]).unwrap(), None);
    }
}
