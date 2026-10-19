//! The `__signal` helper process: reads one `SignalSpec` line from stdin,
//! runs the protocol and writes bounded NDJSON lines to stdout. It ignores
//! Ctrl-C and exits on `{"op":"stop"}` or when stdin closes, so it never
//! outlives its parent.
use super::{BrowseItem, Line, Mode, Sample, SignalSpec, SignalUri, TagInfo, parse};
use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeSet,
    io::{BufRead, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

/// Longest spec line.
const MAX_SPEC: u64 = 64 * 1024;
/// Samples waiting for the emitter before new ones are dropped.
const QUEUE: usize = 8192;
/// A samples line is written at this many samples, bytes or milliseconds.
const FLUSH_SAMPLES: usize = 1000;
const FLUSH_BYTES: usize = 256 * 1024;
const FLUSH_MS: u64 = 200;
/// Browse items per line.
const BROWSE_CHUNK: usize = 500;

pub fn run_helper() -> Result<()> {
    // A terminal Ctrl-C reaches the whole process group; only the parent
    // decides when sources stop and flush.
    let _ = ctrlc::set_handler(|| {});
    let mut input = std::io::stdin().lock();
    let mut line = zeroize::Zeroizing::new(String::new());
    (&mut input).take(MAX_SPEC + 1).read_line(&mut line)?;
    if line.len() as u64 > MAX_SPEC {
        bail!("signal spec exceeds 64 KiB");
    }
    let spec: SignalSpec = serde_json::from_str(line.trim()).context("read signal spec")?;
    drop(line);
    drop(input);
    let stop = Arc::new(AtomicBool::new(false));
    // Watch stdin: `stop` ends the protocol loop; EOF means the parent is
    // gone, so exit at once.
    let watch = stop.clone();
    thread::Builder::new()
        .name("capturefab-signal-stdin".into())
        .spawn(move || {
            let mut input = std::io::stdin().lock();
            let mut line = String::new();
            loop {
                line.clear();
                match (&mut input).take(4096).read_line(&mut line) {
                    Ok(0) | Err(_) => std::process::exit(0),
                    Ok(_) if line.contains("\"stop\"") => watch.store(true, Ordering::SeqCst),
                    Ok(_) => {}
                }
            }
        })?;
    let (sender, emitter) = Emitter::start(std::io::stdout());
    let result = parse(&spec.uri).and_then(|mut uri| {
        if let Some(credentials) = &spec.credentials {
            credentials.apply(&mut uri)?;
        }
        let ctx = Ctx::new(spec.clone(), uri, stop, sender.clone());
        super::require_compiled(ctx.uri.protocol)?;
        let truncated = dispatch(&ctx)?;
        ctx.flush_dropped();
        Ok(truncated)
    });
    let password = parse(&spec.uri).ok().and_then(|u| u.password);
    let last = match result {
        Ok(truncated) => Line::Done { truncated },
        Err(error) => {
            let text = super::redact_text(&format!("{error:#}"), &spec.uri, password.as_deref());
            Line::Error {
                message: spec
                    .credentials
                    .as_ref()
                    .map_or_else(|| text.clone(), |credentials| credentials.redact(&text)),
            }
        }
    };
    let _ = sender.send(Out::Line(last));
    drop(sender);
    let _ = emitter.join();
    Ok(())
}

fn dispatch(ctx: &Ctx) -> Result<bool> {
    match &ctx.uri.target {
        super::Target::Sim { .. } => super::sim::run(ctx),
        #[cfg(feature = "modbus")]
        super::Target::Modbus(_) => super::modbus::run(ctx),
        #[cfg(feature = "mqtt")]
        super::Target::Mqtt { .. } => super::mqtt::run(ctx),
        #[cfg(feature = "opcua")]
        super::Target::Opcua { .. } => super::opcua::run(ctx),
        #[cfg(feature = "mtconnect")]
        super::Target::Mtconnect { .. } => super::mtconnect::run(ctx),
        #[allow(unreachable_patterns)]
        _ => super::require_compiled(ctx.uri.protocol).map(|()| false),
    }
}

pub(crate) enum Out {
    Sample(Sample),
    Line(Line),
}

/// What a protocol needs from the helper: the spec, a stop flag and the
/// bounded output queue.
pub struct Ctx {
    pub spec: SignalSpec,
    pub uri: SignalUri,
    stop: Arc<AtomicBool>,
    out: SyncSender<Out>,
    dropped: AtomicU64,
    allow: BTreeSet<String>,
}
impl Drop for Ctx {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        if let Some(user) = &mut self.uri.user {
            user.zeroize();
        }
        if let Some(password) = &mut self.uri.password {
            password.zeroize();
        }
    }
}
impl Ctx {
    pub(crate) fn new(
        spec: SignalSpec,
        uri: SignalUri,
        stop: Arc<AtomicBool>,
        out: SyncSender<Out>,
    ) -> Self {
        let allow = spec.tags.iter().cloned().collect();
        Self {
            spec,
            uri,
            stop,
            out,
            dropped: AtomicU64::new(0),
            allow,
        }
    }
    pub fn mode(&self) -> Mode {
        self.spec.mode
    }
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.spec.timeout_ms.max(1))
    }
    pub fn interval(&self) -> Duration {
        Duration::from_millis(self.spec.interval_ms.max(1))
    }
    /// Whether the `--tag` allow-list keeps this tag.
    pub fn allowed(&self, tag: &str) -> bool {
        self.allow.is_empty() || self.allow.contains(tag)
    }
    /// Queue a sample without blocking; a full queue counts a drop.
    pub fn sample(&self, mut sample: Sample) {
        if !self.allowed(&sample.tag) {
            return;
        }
        if let Some(credentials) = &self.spec.credentials {
            sample.tag = credentials.redact(&sample.tag);
            if let super::Value::Text(text) = &mut sample.value {
                *text = credentials.redact(text);
            }
        }
        match self.out.try_send(Out::Sample(sample)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
    /// Count a sample the protocol had to drop (oversized payload, tag cap).
    pub fn drop_sample(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }
    fn line(&self, mut line: Line) {
        if let Some(credentials) = &self.spec.credentials {
            credentials.scrub_line(&mut line);
        }
        let _ = self.out.send(Out::Line(line));
    }
    pub fn ready(&self, tags: Vec<TagInfo>) {
        let tags: Vec<TagInfo> = tags.into_iter().filter(|t| self.allowed(&t.tag)).collect();
        self.line(Line::Ready { tags });
    }
    pub fn connected(&self) {
        self.line(Line::State {
            state: "connected".into(),
            error: None,
        });
    }
    pub fn reconnecting(&self, error: &anyhow::Error) {
        let message = self.redact(&format!("{error:#}"));
        eprintln!("capturefab signal helper: {message}");
        self.line(Line::State {
            state: "reconnecting".into(),
            error: Some(message),
        });
    }
    pub fn browse(&self, items: Vec<BrowseItem>) {
        for chunk in items.chunks(BROWSE_CHUNK) {
            self.line(Line::Browse {
                items: chunk.to_vec(),
            });
        }
    }
    pub(crate) fn flush_dropped(&self) {
        let count = self.dropped.swap(0, Ordering::Relaxed);
        if count > 0 {
            self.line(Line::Dropped { count });
        }
    }
    pub fn redact(&self, text: &str) -> String {
        let text = super::redact_text(text, &self.spec.uri, self.uri.password.as_deref());
        self.spec
            .credentials
            .as_ref()
            .map_or_else(|| text.clone(), |credentials| credentials.redact(&text))
    }
    /// Sleep until `until`, waking early when stopped. False when stopped.
    pub fn sleep_until(&self, until: Instant) -> bool {
        loop {
            if self.stopped() {
                return false;
            }
            let now = Instant::now();
            if now >= until {
                return true;
            }
            thread::sleep((until - now).min(Duration::from_millis(50)));
        }
    }
    /// The next tick of a fixed schedule; missed ticks are skipped, never burst.
    pub fn next_tick(&self, tick: &mut Instant) -> Instant {
        let interval = self.interval();
        *tick += interval;
        let now = Instant::now();
        if *tick < now {
            let behind = (now - *tick).as_nanos() / interval.as_nanos().max(1);
            *tick += interval * (behind as u32 + 1);
        }
        *tick
    }
    /// Run `poll` on the log schedule, reconnecting with backoff (1 s to 30 s)
    /// after errors. The first connection error is returned, so a source that
    /// never worked fails at start instead of retrying forever.
    pub fn poll_loop<C>(
        &self,
        mut connect: impl FnMut() -> Result<C>,
        mut poll: impl FnMut(&mut C) -> Result<()>,
    ) -> Result<()> {
        let mut connection = Some(connect()?);
        self.connected();
        let mut backoff = Duration::from_secs(1);
        let mut tick = Instant::now();
        loop {
            if self.stopped() {
                return Ok(());
            }
            match connection.as_mut() {
                Some(c) => match poll(c) {
                    Ok(()) => backoff = Duration::from_secs(1),
                    Err(error) => {
                        self.reconnecting(&error);
                        connection = None;
                    }
                },
                None => match connect() {
                    Ok(c) => {
                        connection = Some(c);
                        self.connected();
                        backoff = Duration::from_secs(1);
                        continue;
                    }
                    Err(error) => self.reconnecting(&error),
                },
            }
            self.flush_dropped();
            let until = if connection.is_some() {
                self.next_tick(&mut tick)
            } else {
                let until = Instant::now() + backoff;
                backoff = (backoff * 2).min(Duration::from_secs(30));
                until
            };
            if !self.sleep_until(until) {
                return Ok(());
            }
        }
    }
}

/// Writes queued samples as lines of at most 1000 samples or 256 KiB, at
/// least every 200 ms, keeping control lines in order with samples.
pub(crate) struct Emitter;
impl Emitter {
    pub(crate) fn start<W: Write + Send + 'static>(
        output: W,
    ) -> (SyncSender<Out>, thread::JoinHandle<()>) {
        let (sender, receiver) = mpsc::sync_channel::<Out>(QUEUE);
        let handle = thread::Builder::new()
            .name("capturefab-signal-emit".into())
            .spawn(move || emit(receiver, output))
            .expect("start signal emitter");
        (sender, handle)
    }
}

fn emit<W: Write>(receiver: mpsc::Receiver<Out>, mut output: W) {
    let mut pending: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    let mut since = Instant::now();
    let flush = |pending: &mut Vec<String>, bytes: &mut usize, output: &mut W| -> bool {
        if pending.is_empty() {
            return true;
        }
        let line = format!(
            "{{\"t\":\"samples\",\"samples\":[{}]}}\n",
            pending.join(",")
        );
        pending.clear();
        *bytes = 0;
        output.write_all(line.as_bytes()).is_ok() && output.flush().is_ok()
    };
    loop {
        let wait = Duration::from_millis(FLUSH_MS).saturating_sub(since.elapsed());
        match receiver.recv_timeout(wait) {
            Ok(Out::Sample(sample)) => {
                let Ok(text) = serde_json::to_string(&sample) else {
                    continue;
                };
                if pending.is_empty() {
                    since = Instant::now();
                }
                if bytes + text.len() > FLUSH_BYTES && !flush(&mut pending, &mut bytes, &mut output)
                {
                    return;
                }
                bytes += text.len() + 1;
                pending.push(text);
                if pending.len() >= FLUSH_SAMPLES && !flush(&mut pending, &mut bytes, &mut output) {
                    return;
                }
            }
            Ok(Out::Line(line)) => {
                if !flush(&mut pending, &mut bytes, &mut output) {
                    return;
                }
                let Ok(mut text) = serde_json::to_string(&line) else {
                    continue;
                };
                text.push('\n');
                if output.write_all(text.as_bytes()).is_err() || output.flush().is_err() {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if !flush(&mut pending, &mut bytes, &mut output) {
                    return;
                }
                since = Instant::now();
            }
            Err(RecvTimeoutError::Disconnected) => {
                flush(&mut pending, &mut bytes, &mut output);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signals::{Quality, Value};
    use std::sync::Mutex;

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);
    impl Write for Shared {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn emitter_bounds_lines_and_keeps_order() {
        let out = Shared::default();
        let (sender, handle) = Emitter::start(out.clone());
        sender
            .send(Out::Line(Line::Ready { tags: vec![] }))
            .unwrap();
        let big = "x".repeat(1024);
        for i in 0..3000 {
            sender
                .send(Out::Sample(Sample::now(
                    &format!("t{i}"),
                    Value::text(&big),
                    Quality::Good,
                )))
                .unwrap();
        }
        sender
            .send(Out::Line(Line::Done { truncated: false }))
            .unwrap();
        drop(sender);
        handle.join().unwrap();
        let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.first().unwrap().contains("\"ready\""));
        assert!(lines.last().unwrap().contains("\"done\""));
        let mut count = 0;
        for line in &lines {
            assert!(line.len() < 300 * 1024, "line of {} bytes", line.len());
            if let Line::Samples { samples } = serde_json::from_str(line).unwrap() {
                count += samples.len();
            }
        }
        assert_eq!(count, 3000);
        assert!(lines.len() > 10, "256 KiB flushes split the samples");
    }
}
