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
    credentials: Option<super::auth::SignalCredentials>,
}

impl SignalProcess {
    /// Start a helper and send it the spec; returns the stdout reader too.
    pub fn spawn(spec: &SignalSpec) -> Result<(Self, BufReader<ChildStdout>)> {
        Self::spawn_wire(spec, false)
    }
    pub(crate) fn spawn_bound(
        spec: &SignalSpec,
        session: &super::auth::Session,
        binding: Option<&super::auth::AuthBinding>,
        deadline: Option<Instant>,
    ) -> Result<(Self, BufReader<ChildStdout>)> {
        ensure!(
            deadline.is_none_or(|deadline| Instant::now() < deadline),
            "signal operation deadline expired; no connection was started"
        );
        match binding {
            Some(binding) => session.with_credentials(binding, &spec.uri, |credentials| {
                ensure!(deadline.is_none_or(|deadline|Instant::now()<deadline),"signal operation timed out while resolving credentials; no connection was started");
                let mut wire = spec.clone();
                wire.credentials = credentials;
                Self::spawn_wire(&wire, true)
            }),
            None => Self::spawn(spec),
        }
    }
    fn spawn_wire(spec: &SignalSpec, protected: bool) -> Result<(Self, BufReader<ChildStdout>)> {
        let executable = std::env::var_os("CAPTUREFAB_EXECUTABLE")
            .map(PathBuf::from)
            .unwrap_or(std::env::current_exe()?);
        let mut command = Command::new(executable);
        command
            .arg("__signal")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if protected || spec.credentials.is_some() {
                Stdio::null()
            } else {
                Stdio::inherit()
            });
        for (name, _) in std::env::vars_os() {
            if name
                .to_str()
                .and_then(|name| name.get(..super::auth::ENV_PREFIX.len()))
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(super::auth::ENV_PREFIX))
            {
                command.env_remove(name);
            }
        }
        let mut child =
            crate::session::spawn_helper(&mut command).context("launch signal helper process")?;
        let stdout = child.stdout.take().context("signal helper stdout")?;
        let mut process = Self {
            stdin: child.stdin.take(),
            child,
            credentials: spec.credentials.clone(),
        };
        let mut line = zeroize::Zeroizing::new(serde_json::to_vec(spec)?);
        line.push(b'\n');
        if let Some(stdin) = process.stdin.as_mut() {
            stdin
                .write_all(&line)
                .and_then(|()| stdin.flush())
                .context("send signal spec")?;
        }
        Ok((process, BufReader::new(stdout)))
    }
    pub(crate) fn redact(&self, text: &str) -> String {
        self.credentials
            .as_ref()
            .map_or_else(|| text.to_owned(), |credentials| credentials.redact(text))
    }
    pub(crate) fn scrub_line(&self, line: &mut Line) {
        if let Some(credentials) = &self.credentials {
            credentials.scrub_line(line);
        }
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
    let mut data = zeroize::Zeroizing::new(Vec::new());
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

/// Where OPC UA keeps certificates: a private directory in the runtime dir.
pub fn pki_dir() -> Option<PathBuf> {
    let base = crate::ipc::runtime_dir();
    let signals = base.join("signals");
    let pki = signals.join("opcua-pki");
    for dir in [&base, &signals, &pki] {
        crate::ipc::ensure_private_dir(dir).ok()?;
    }
    Some(pki)
}

/// Run a one-shot helper until `done` or `error`, or kill it at `deadline_ms`.
fn one_shot(
    spec: SignalSpec,
    deadline_ms: u64,
    binding: Option<(&super::auth::Session, &super::auth::AuthBinding)>,
) -> Result<(Vec<Line>, u64)> {
    let deadline = Instant::now() + Duration::from_millis(deadline_ms);
    let (process, mut reader) = match binding {
        Some((session, binding)) => {
            SignalProcess::spawn_bound(&spec, session, Some(binding), Some(deadline))?
        }
        None => SignalProcess::spawn(&spec)?,
    };
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
    let mut lines = Vec::new();
    let mut dropped = 0;
    let password = parse(&spec.uri).ok().and_then(|u| u.password);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let line = match receiver.recv_timeout(remaining) {
            Ok(line) => {
                line.map_err(|error| anyhow::anyhow!("{}", process.redact(&format!("{error:#}"))))?
            }
            Err(_) => {
                process.wait_or_kill(Instant::now());
                bail!("signal source timed out after {deadline_ms} ms");
            }
        };
        let mut line = line;
        if let Some(line) = &mut line {
            process.scrub_line(line);
        }
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
                    process.redact(&super::redact_text(
                        &message,
                        &spec.uri,
                        password.as_deref()
                    ))
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
        credentials: None,
        mode,
        interval_ms: super::default_interval_ms(),
        tags: Vec::new(),
        timeout_ms,
        pki_dir: (protocol == Protocol::Opcua).then(pki_dir).flatten(),
    }
}

pub fn read_bound(
    uri: &str,
    timeout_ms: u64,
    session: &super::auth::Session,
    binding: Option<&super::auth::AuthBinding>,
) -> Result<Json> {
    read_inner(uri, timeout_ms, binding.map(|binding| (session, binding)))
}
fn read_inner(
    uri: &str,
    timeout_ms: u64,
    binding: Option<(&super::auth::Session, &super::auth::AuthBinding)>,
) -> Result<Json> {
    let parsed = parse(uri)?;
    super::require_compiled(parsed.protocol)?;
    let (lines, dropped) = one_shot(
        spec(uri, Mode::Read, timeout_ms, parsed.protocol),
        timeout_ms + 5000,
        binding,
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

pub fn browse_bound(
    uri: &str,
    duration_ms: u64,
    limit: u32,
    session: &super::auth::Session,
    binding: Option<&super::auth::AuthBinding>,
) -> Result<Json> {
    browse_inner(
        uri,
        duration_ms,
        limit,
        binding.map(|binding| (session, binding)),
    )
}
fn browse_inner(
    uri: &str,
    duration_ms: u64,
    limit: u32,
    binding: Option<(&super::auth::Session, &super::auth::AuthBinding)>,
) -> Result<Json> {
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
        binding,
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
