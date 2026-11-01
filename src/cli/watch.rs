//! `capturefab watch`: live measurements while a person aims, focuses or
//! sets exposure. The camera streams; its worker measures a frame each
//! `--every` off its camera thread, as for inspect, and only a small
//! preview ever leaves it. A terminal gets a block redrawn in place, with
//! the frame drawn under it for `--show`; anything else one JSON object a
//! line.
use super::{
    Client, Global,
    args::{CameraArg, KeepArg},
    output::{self, Report},
    signals, term,
};
use crate::{inspect::PreviewFormat, session::SessionCommand, types::Usage};
use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};
use std::{
    fmt::Write as _,
    io::Write as _,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

#[derive(Debug, Args)]
pub struct WatchArgs {
    #[command(flatten)]
    pub camera: CameraArg,
    /// Time between measurements, 50ms or more
    #[arg(long, default_value = "250ms", value_name = "DUR", value_parser = super::args::duration)]
    pub every: Duration,
    /// Stop after this long (default: at Ctrl-C)
    #[arg(long = "for", value_name = "DUR", value_parser = super::args::duration)]
    pub duration: Option<Duration>,
    /// Measure only this part of the frame: X,Y,W,H as fractions 0..1
    #[arg(long, value_name = "X,Y,W,H", value_parser = super::inspect::parse_region)]
    pub region: Option<[f64; 4]>,
    /// Draw the frame in the terminal under the measurements (kitty, iTerm2, or colour blocks)
    #[arg(long)]
    pub show: bool,
    /// How long each measurement waits for its frame, up to 10m (default: 5s, or 3 exposures and frame periods when longer)
    #[arg(long, value_name = "DUR", value_parser = super::args::duration)]
    pub timeout: Option<Duration>,
    #[command(flatten)]
    pub keep: KeepArg,
}

/// The shortest time between measurements: a frame's measurements and a
/// status, each a short request.
const MIN_EVERY: Duration = Duration::from_millis(50);
/// How often the settings shown are read again.
const SETTINGS_EVERY: Duration = Duration::from_secs(1);
/// Rows the measurements take above a picture, and below it.
const TEXT_ROWS: u16 = 5;

pub fn run(global: &Global, args: WatchArgs) -> Result<Option<Report>> {
    if args.every < MIN_EVERY {
        return Err(Usage("--every is at least 50ms".into()).into());
    }
    if args
        .timeout
        .is_some_and(|wait| wait.is_zero() || super::millis(wait) > crate::inspect::MAX_WAIT_MS)
    {
        return Err(Usage("--timeout must be longer than 0 and at most 10m".into()).into());
    }
    signals::take_over();
    // Ctrl-C wakes the wait for a frame or the next measurement.
    let (sender, wakes) = mpsc::channel();
    {
        let sender = sender.clone();
        signals::on_signal(move || {
            let _ = sender.send(Wake::Signal);
        });
    }
    // The measurements' requests, which Ctrl-C closes: a session then
    // cancels the one waiting for a frame. In a shell they are the
    // command's.
    if crate::ipc::requests_owner() == 0 {
        crate::ipc::own_requests(crate::ipc::new_owner());
    }
    let client = Arc::new(Client::open_with(
        global,
        args.camera.camera.as_deref(),
        args.keep.route(),
    )?);
    let live = !global.json && term::stdout_is_tty() && term::escapes();
    let graphics = if args.show && live {
        term::graphics()
    } else {
        None
    };
    let streaming = client.status()?["streaming"] == true;
    let started = !streaming && client.call(SessionCommand::Start).is_ok();
    let mut paused = None;
    let watched = Watch {
        client: &client,
        args: &args,
        live,
        graphics,
        sender,
        wakes,
        paused: &mut paused,
    }
    .run();
    let interrupted = signals::interrupted();
    if interrupted {
        // A measurement may wait for a frame no one will see: closing its
        // request lets the session cancel it, and Stop need not wait.
        crate::ipc::close_requests(crate::ipc::requests_owner());
    }
    // What this started streaming stops, Ctrl-C being how watch usually
    // ends: before this exits, as a session would stream on. A worker of
    // this command alone ends with it.
    let stopped = if !started {
        Ok(())
    } else if interrupted {
        if client.outlives() {
            let _ = super::inspect::stop_unless_busy(&client);
        }
        Ok(())
    } else {
        super::inspect::stop_unless_busy(&client)
    };
    drop(paused);
    watched?;
    stopped?;
    Ok(None)
}

enum Wake {
    Sample(Result<Sample>),
    Signal,
}

/// One measurement: the frame's, the camera's counters, and its settings
/// when they were read again.
struct Sample {
    frame: Value,
    status: Value,
    info: Option<Value>,
}

struct Watch<'a> {
    client: &'a Arc<Client>,
    args: &'a WatchArgs,
    live: bool,
    graphics: Option<term::Graphics>,
    sender: mpsc::Sender<Wake>,
    wakes: mpsc::Receiver<Wake>,
    /// The status line kept off a terminal the measurements show on.
    paused: &'a mut Option<term::Paused>,
}

impl Watch<'_> {
    fn run(self) -> Result<()> {
        let began = Instant::now();
        let mut view = self.live.then(|| View::new(self.graphics));
        let mut info = self.client.call(SessionCommand::Info)?;
        let mut info_at = Instant::now();
        let mut best: Option<(f64, f64)> = None;
        let box_size = term::picture_box(TEXT_ROWS + 2);
        let edge = self.graphics.map_or(0, |graphics| {
            term::preview_edge(graphics, box_size.0, box_size.1)
        });
        // Until the first measurement shows, the status line says what
        // this waits for; from then on the measurements say it, and a
        // status line would only get in their way on a terminal.
        let camera = crate::media::redact_url(info["camera"].as_str().unwrap_or("the camera"));
        let mut waiting = Some(term::activity(format!(
            "waiting for a frame from {camera}…"
        )));
        for index in 0u32.. {
            if signals::interrupted() {
                break;
            }
            self.request(edge, info_at.elapsed() >= SETTINGS_EVERY);
            let sample = match self.wakes.recv() {
                Ok(Wake::Sample(sample)) => sample?,
                Ok(Wake::Signal) | Err(_) => break,
            };
            if let Some(read) = &sample.info {
                info = read.clone();
                info_at = Instant::now();
            }
            let t = began.elapsed().as_secs_f64();
            let sharpness = sample.frame["focus"]["laplacian"].as_f64().unwrap_or(0.0);
            if best.is_none_or(|(sharpest, _)| sharpness > sharpest) {
                best = Some((sharpness, t));
            }
            let line = measurement(&info, &sample, t, best);
            let preview = sample.frame["image"]["path"]
                .as_str()
                .map(std::path::PathBuf::from);
            if waiting.take().is_some() && term::stdout_is_tty() {
                *self.paused = Some(term::pause());
            }
            match &mut view {
                Some(view) => view.draw(&line, preview.as_deref())?,
                None => output::write_json(&output::document(line))?,
            }
            // The preview was for this view alone.
            if let Some(preview) = preview {
                let _ = std::fs::remove_file(preview);
            }
            let next = self.args.every.saturating_mul(index + 1);
            if self.args.duration.is_some_and(|duration| next > duration) {
                break;
            }
            match self
                .wakes
                .recv_timeout((began + next).saturating_duration_since(Instant::now()))
            {
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        if let Some(view) = view {
            view.finish();
        }
        Ok(())
    }

    /// Ask for the next measurement on a thread of its own, which a signal
    /// does not wait for: its requests are this command's, which the
    /// signal closes.
    fn request(&self, edge: u32, settings: bool) {
        let (client, sender) = (self.client.clone(), self.sender.clone());
        let lineage = signals::Lineage::here();
        let frame = SessionCommand::Frame {
            fresh: 1,
            timeout_ms: self.args.timeout.map_or(0, super::millis),
            max_dimension: edge,
            format: PreviewFormat::Png,
            region: self.args.region,
            preview: None,
            warmup_ms: Some(0),
        };
        let spawned = std::thread::Builder::new()
            .name("capturefab-watch".into())
            .spawn({
                let sender = sender.clone();
                move || {
                    lineage.enter();
                    // A signal just now has no one waiting for this.
                    if signals::interrupted() {
                        return;
                    }
                    let sample = (|| {
                        let frame = client.call(frame)?;
                        let status = client.status()?;
                        let info = settings
                            .then(|| client.call(SessionCommand::Info))
                            .transpose()?;
                        Ok(Sample {
                            frame,
                            status,
                            info,
                        })
                    })();
                    let _ = sender.send(Wake::Sample(sample));
                }
            });
        if let Err(error) = spawned {
            let _ = sender.send(Wake::Sample(Err(error.into())));
        }
    }
}

/// One measurement as a JSON line: what inspect says of the frame, the
/// camera's rate and counters, and the sharpest frame so far.
fn measurement(info: &Value, sample: &Sample, t: f64, best: Option<(f64, f64)>) -> Value {
    let inspected = super::inspect::document(info, &sample.frame, crate::inspect::CLIP_WARN, false);
    let status = &sample.status;
    let mut value = json!({
        "t": (t * 1000.0).round() / 1000.0,
        "camera": inspected["camera"],
        "frame": {
            "width": inspected["frame"]["width"],
            "height": inspected["frame"]["height"],
            "pixel_format": inspected["frame"]["pixel_format"],
            "id": inspected["frame"]["id"],
            "age_ms": inspected["frame"]["age_ms"],
        },
        // The rate is known once a second of frames came.
        "fps": status["fps"]
            .as_f64()
            .filter(|fps| *fps > 0.0)
            .map(|fps| (fps * 100.0).round() / 100.0),
        "frames": status["frames"],
        "dropped": status["dropped"],
        "lost": status["lost"],
        "exposure": inspected["exposure"],
        "verdict": inspected["verdict"],
        "suggest": inspected["suggest"],
        "settings": inspected["settings"],
    });
    if let Some(sharpness) = inspected["focus"]["sharpness"].as_f64() {
        value["focus"] = json!({"sharpness": sharpness});
        if let Some((sharpest, at)) = best {
            value["focus"]["best"] = json!(sharpest);
            value["focus"]["best_t"] = json!((at * 1000.0).round() / 1000.0);
        }
    }
    crate::types::without_nulls(value)
}

/// The measurements a terminal shows, redrawn in place: the block's rows
/// are known, so each redraw starts where the last one did.
struct View {
    graphics: Option<term::Graphics>,
    /// Rows the last block took.
    rows: u16,
    /// A kitty image each frame replaces.
    id: u32,
}

impl View {
    fn new(graphics: Option<term::Graphics>) -> Self {
        Self {
            graphics,
            rows: 0,
            id: 0x6366_0000 | (std::process::id() & 0xffff),
        }
    }
    /// Draw `line` over the last block. Each row is cut to the terminal's
    /// width, as it is now: a row that wrapped would take two lines, and
    /// the next block would start one too low.
    fn draw(&mut self, line: &Value, preview: Option<&std::path::Path>) -> Result<()> {
        let width = term::size().map_or(80, |(cols, _)| cols);
        let block = self.block(line, preview, width);
        let mut out = std::io::stdout().lock();
        out.write_all(block.as_bytes())?;
        out.flush()?;
        Ok(())
    }
    /// What draws `line` over the last block on a terminal `width`
    /// columns wide.
    fn block(&mut self, line: &Value, preview: Option<&std::path::Path>, width: u16) -> String {
        let (cols, rows) = term::picture_box(TEXT_ROWS + 2);
        let mut block = String::new();
        if self.rows > 0 {
            let _ = write!(block, "\x1b[{}A\r", self.rows);
        }
        let lines = text(line);
        for row in &lines {
            let _ = writeln!(block, "\x1b[2K{}", term::fit(row, width));
        }
        let mut drawn = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        if let (Some(graphics), Some(preview)) = (self.graphics, preview)
            && let Ok(bytes) = std::fs::read(preview)
            && let Ok(picture) = term::picture(&bytes, graphics, cols, rows, Some(self.id))
        {
            block.push_str(&picture.text);
            drawn += picture.rows;
        }
        let stops = format!(
            "Ctrl-C stops · {}",
            term::clock(Duration::from_secs_f64(line["t"].as_f64().unwrap_or(0.0)))
        );
        let _ = writeln!(block, "\x1b[2K{}", term::fit(&stops, width));
        drawn += 1;
        // A shorter block than the last leaves nothing of it below.
        block.push_str("\x1b[J");
        self.rows = drawn;
        block
    }
    fn finish(self) {
        let _ = std::io::stdout().flush();
    }
}

/// The measurements as a person reads them.
fn text(line: &Value) -> Vec<String> {
    let number = |value: &Value| value.as_f64().unwrap_or(0.0);
    let exposure = &line["exposure"];
    let mut rows = vec![format!(
        "{}  {}x{} {}  {} fps  {} frames  {} dropped{}",
        line["camera"].as_str().unwrap_or_default(),
        line["frame"]["width"],
        line["frame"]["height"],
        line["frame"]["pixel_format"].as_str().unwrap_or_default(),
        line["fps"]
            .as_f64()
            .map_or_else(|| "…".to_owned(), |fps| format!("{fps:.1}")),
        line["frames"].as_u64().unwrap_or(0),
        line["dropped"].as_u64().unwrap_or(0) + line["lost"].as_u64().unwrap_or(0),
        line["frame"]["age_ms"]
            .as_u64()
            .map(|age| format!("  {age} ms old"))
            .unwrap_or_default(),
    )];
    if exposure.is_object() {
        rows.push(format!(
            "exposure  mean {:.2}  p99 {:.2}  clipped {:.2}% dark {:.2}% bright  {}",
            number(&exposure["mean"]),
            number(&exposure["p99"]),
            number(&exposure["clipped_low"]) * 100.0,
            number(&exposure["clipped_high"]) * 100.0,
            line["verdict"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }
    if let Some(sharpness) = line["focus"]["sharpness"].as_f64() {
        rows.push(format!(
            "focus     sharpness {sharpness:.1}  best {:.1} at {:.1}s",
            number(&line["focus"]["best"]),
            number(&line["focus"]["best_t"])
        ));
    }
    let settings = &line["settings"];
    let mut shown = Vec::new();
    if let Some(us) = settings["exposure_us"].as_f64() {
        shown.push(format!("exposure {}", exposure_text(us)));
    }
    if let Some(db) = settings["gain_db"].as_f64() {
        shown.push(format!("gain {db} dB"));
    } else if let Some(gain) = settings["gain"].as_f64() {
        shown.push(format!("gain {gain}"));
    }
    if let Some(fps) = settings["fps"].as_f64() {
        shown.push(format!("frame rate {fps}"));
    }
    if let Some(suggest) = line["suggest"]["ev"].as_f64() {
        shown.push(format!("suggest {suggest:+} EV"));
    }
    if !shown.is_empty() {
        rows.push(format!("settings  {}", shown.join("  ")));
    }
    rows
}

/// An exposure time as people write it: `10ms`, `250us`, `1.5s`.
fn exposure_text(us: f64) -> String {
    if us >= 1e6 {
        format!("{}s", (us / 1e4).round() / 100.0)
    } else if us >= 1000.0 {
        format!("{}ms", (us / 10.0).round() / 100.0)
    } else {
        format!("{}us", us.round())
    }
}

/// Fields of each line, for `schema`.
pub const FIELDS: &[(&str, &str)] = &[
    (
        "t",
        "float — seconds since watching began; one JSON line each --every (a terminal shows them redrawn instead)",
    ),
    ("camera", "string — camera ID"),
    ("frame", "{width,height,pixel_format,id?,age_ms}"),
    (
        "fps",
        "float — the camera's frame rate as received, once a second of frames came",
    ),
    ("frames", "int — frames received since it began streaming"),
    ("dropped", "int — frames dropped on the way"),
    ("lost", "int — frames lost or incomplete in transfer"),
    (
        "exposure",
        "{mean,p1,p50,p99,clipped_low,clipped_high,…} — as inspect",
    ),
    (
        "verdict",
        "[ok|dark|bright|clipped_high|clipped_low|…] — as inspect",
    ),
    ("suggest", "{ev,exposure_us?,limited_by?} — as inspect"),
    (
        "focus",
        "{sharpness,best,best_t} — this frame's sharpness, and the sharpest so far with its t",
    ),
    (
        "settings",
        "{exposure_us?,gain_db?,gain?,fps?,pixel_format?}",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_measurement_reads_as_lines_and_one_json_object() {
        let info = json!({"exposure_us": 10000.0, "gain_db": 0.0, "bits": 8});
        let sample = Sample {
            frame: json!({
                "camera": "sim:0",
                "frame": {"width": 640, "height": 480, "pixel_format": "Mono8", "id": 7, "age_ms": 3},
                "exposure": {"mean": 0.45, "p01": 0.1, "p50": 0.45, "p99": 0.8},
                "clipping": {"pixels": 100, "high_px": 0, "low_px": 0},
                "focus": {"laplacian": 12.5},
            }),
            status: json!({"fps": 29.97, "frames": 120, "dropped": 1, "lost": 0}),
            info: None,
        };
        let line = measurement(&info, &sample, 0.2504, Some((14.0, 0.0)));
        assert_eq!(line["t"], 0.25);
        assert_eq!(line["fps"], 29.97);
        assert_eq!(line["frame"]["id"], 7);
        assert_eq!(line["exposure"]["clipped_high"], 0.0);
        assert_eq!(line["verdict"], json!(["ok"]));
        assert_eq!(
            line["focus"],
            json!({"sharpness": 12.5, "best": 14.0, "best_t": 0.0})
        );
        assert_eq!(line["settings"]["exposure_us"], 10000.0);
        for key in line.as_object().unwrap().keys() {
            assert!(FIELDS.iter().any(|(field, _)| field == key), "{key}");
        }
        let rows = text(&line);
        assert_eq!(
            rows[0],
            "sim:0  640x480 Mono8  30.0 fps  120 frames  1 dropped  3 ms old"
        );
        assert_eq!(
            rows[1],
            "exposure  mean 0.45  p99 0.80  clipped 0.00% dark 0.00% bright  ok"
        );
        assert_eq!(rows[2], "focus     sharpness 12.5  best 14.0 at 0.0s");
        assert_eq!(rows[3], "settings  exposure 10ms  gain 0 dB");
        // At a narrow terminal no row wraps, so each redraw goes up by as
        // many rows as the last one took.
        let mut view = View::new(None);
        let first = view.block(&line, None, 40);
        let drawn = first.matches('\n').count();
        assert_eq!(usize::from(view.rows), drawn);
        for row in first.split('\n') {
            let shown = row.trim_start_matches("\x1b[2K").trim_end_matches("\x1b[J");
            assert!(shown.chars().count() < 40, "{shown:?}");
        }
        let second = view.block(&line, None, 40);
        assert!(
            second.starts_with(&format!("\x1b[{drawn}A\r")),
            "{second:?}"
        );
        assert_eq!(exposure_text(1500.0), "1.5ms");
        assert_eq!(exposure_text(250.0), "250us");
        assert_eq!(exposure_text(2_500_000.0), "2.5s");
    }
}
