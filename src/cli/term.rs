//! The terminal a person runs Capturefab in: whether stdin, stdout and
//! stderr are one, its size, the status line that says what a slow command
//! is doing, the camera picker, and frames drawn inline. Escape sequences
//! go only to a terminal that takes them.
#![deny(clippy::missing_const_for_fn)]
#![deny(clippy::significant_drop_tightening)]
use anyhow::{Context as _, Result, bail};
use std::{
    fmt::Write as _,
    io::{self, BufRead, IsTerminal as _, Write},
    sync::{
        Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc::RecvTimeoutError,
    },
    time::{Duration, Instant},
};

pub(crate) fn stdin_is_tty() -> bool {
    io::stdin().is_terminal()
}
pub(crate) fn stdout_is_tty() -> bool {
    io::stdout().is_terminal()
}
pub(crate) fn stderr_is_tty() -> bool {
    io::stderr().is_terminal()
}

/// Whether the terminal takes escape sequences: not `TERM=dumb`, and on
/// Windows a console that turned virtual terminal processing on.
pub(crate) fn escapes() -> bool {
    std::env::var_os("TERM").is_none_or(|term| term != "dumb") && platform::escapes()
}

/// The terminal's columns and rows, from whichever of stdout, stderr and
/// stdin is one, else COLUMNS and LINES.
pub(crate) fn size() -> Option<(u16, u16)> {
    platform::size().or_else(|| {
        let var = |name| std::env::var(name).ok()?.trim().parse::<u16>().ok();
        Some((var("COLUMNS")?, var("LINES").unwrap_or(24))).filter(|(cols, _)| *cols > 0)
    })
}

/// A duration as a clock: `0:07`, `12:30`, `1:02:03`.
pub(crate) fn clock(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

// ---------------------------------------------------------------------------
// The status line

/// How long an operation runs before the status line names it: anything a
/// person would notice waiting for.
const DELAY: Duration = Duration::from_millis(100);
/// How long the status line stays away after other output, so a line
/// printed meanwhile is not drawn over.
const SETTLE: Duration = Duration::from_millis(50);

/// What the status line shows: the innermost operation in progress.
struct Line {
    /// Operations in progress, innermost last.
    stack: Vec<Entry>,
    next: u64,
    /// A prompt or a live view owns the terminal while this is above 0.
    paused: usize,
    /// What is on screen now, if anything.
    drawn: Option<String>,
    /// Other output was written just now: draw again from then on.
    hold_until: Option<Instant>,
    ticker: bool,
}
struct Entry {
    id: u64,
    text: String,
    since: Instant,
    /// A progress line says its own elapsed time.
    progress: bool,
}

static ENABLED: AtomicBool = AtomicBool::new(false);
/// The command running, as a shell runs it, was given `-q`.
static HUSHED: AtomicBool = AtomicBool::new(false);
static LINE: Mutex<Line> = Mutex::new(Line {
    stack: Vec::new(),
    next: 0,
    paused: 0,
    drawn: None,
    hold_until: None,
    ticker: false,
});
static CHANGED: Condvar = Condvar::new();

fn line() -> MutexGuard<'static, Line> {
    LINE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Show the status line on stderr from now on when it is a terminal that
/// takes escape sequences, unless `quiet`.
pub(crate) fn enable(quiet: bool) {
    if !quiet && stderr_is_tty() && escapes() {
        ENABLED.store(true, Ordering::Release);
    }
}

/// Whether the status line is shown: a command that reports progress
/// asks for it often enough to keep its clock moving.
pub(crate) fn live() -> bool {
    ENABLED.load(Ordering::Acquire) && !HUSHED.load(Ordering::Acquire)
}

/// Whether the command a shell runs now was given `-q`, which silences
/// its status line as it would the process's.
pub(crate) fn hush(quiet: bool) {
    HUSHED.store(quiet, Ordering::Release);
}

impl Line {
    fn erase(&mut self) {
        if self.drawn.take().is_some() {
            let mut err = io::stderr().lock();
            let _ = err.write_all(b"\r\x1b[2K").and_then(|()| err.flush());
        }
    }
    fn draw(&mut self, text: String) {
        if self.drawn.as_deref() == Some(text.as_str()) {
            return;
        }
        let mut err = io::stderr().lock();
        let _ = write!(err, "\r\x1b[2K{text}").and_then(|()| err.flush());
        self.drawn = Some(text);
    }
    /// Draw what is due now; how long until something changes.
    fn refresh(&mut self, now: Instant) -> Option<Duration> {
        if let Some(until) = self.hold_until.filter(|until| *until > now) {
            return Some(until - now);
        }
        self.hold_until = None;
        let Some(entry) = self.stack.last().filter(|_| self.paused == 0) else {
            self.erase();
            return None;
        };
        let age = now.saturating_duration_since(entry.since);
        // What is shown stays until the operation begun since is due.
        if let Some(left) = DELAY.checked_sub(age).filter(|left| !left.is_zero()) {
            return Some(left);
        }
        let cols = size().map_or(80, |(cols, _)| cols);
        let text = status_text(&entry.text, age, entry.progress, cols);
        let progress = entry.progress;
        self.draw(text);
        (!progress).then(|| {
            Duration::from_secs(1).saturating_sub(Duration::from_nanos(age.subsec_nanos().into()))
        })
    }
}

/// The status line for an operation `age` old: its text, then the seconds
/// it has taken once that is a second or more (a progress line says its
/// own), cut to fit `cols` columns without wrapping.
fn status_text(text: &str, age: Duration, progress: bool, cols: u16) -> String {
    let mut line = text.to_owned();
    if !progress && age >= Duration::from_secs(1) {
        let _ = write!(line, " {}s", age.as_secs());
    }
    fit(&line, cols)
}

/// `text` as one line without escapes, cut to fit `cols` columns without
/// wrapping: what is cut ends in `…`. Control characters, as in a name a
/// camera gave, do not reach the terminal.
pub(crate) fn fit(text: &str, cols: u16) -> String {
    let text = crate::types::printable_line(text);
    let room = usize::from(cols.max(2)) - 1;
    if text.chars().count() <= room {
        return text.into_owned();
    }
    text.chars()
        .take(room.saturating_sub(1))
        .chain(std::iter::once('…'))
        .collect()
}

fn ticker() -> ! {
    let mut line = line();
    loop {
        let wait = line.refresh(Instant::now());
        line = match wait {
            Some(wait) => {
                CHANGED
                    .wait_timeout(line, wait)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            }
            None => CHANGED.wait(line).unwrap_or_else(PoisonError::into_inner),
        };
    }
}

/// An operation in progress, which the status line names once it has run
/// for `DELAY`; it leaves the line when dropped.
#[must_use]
pub(crate) struct Activity {
    id: u64,
}

/// Name what this command is doing now, such as "connecting to sim:0…".
pub(crate) fn activity(text: impl Into<String>) -> Activity {
    if !live() {
        return Activity { id: 0 };
    }
    let mut line = line();
    line.next += 1;
    let id = line.next;
    line.stack.push(Entry {
        id,
        text: text.into(),
        since: Instant::now(),
        progress: false,
    });
    if !line.ticker {
        line.ticker = std::thread::Builder::new()
            .name("capturefab-status".into())
            .spawn(ticker)
            .is_ok();
    }
    drop(line);
    CHANGED.notify_all();
    Activity { id }
}

impl Activity {
    fn edit(&self, text: String, progress: bool) {
        if self.id == 0 {
            return;
        }
        let mut line = line();
        if let Some(entry) = line.stack.iter_mut().find(|entry| entry.id == self.id) {
            entry.text = text;
            entry.progress = progress;
        }
        drop(line);
        CHANGED.notify_all();
    }
    /// Show how far it is: a line that carries its own elapsed time, such
    /// as "12/100 frames · 0:03 · 0:21 left".
    pub(crate) fn progress(&self, text: impl Into<String>) {
        self.edit(text.into(), true);
    }
}
impl Drop for Activity {
    fn drop(&mut self) {
        if self.id == 0 {
            return;
        }
        let mut line = line();
        line.stack.retain(|entry| entry.id != self.id);
        if line.stack.is_empty() {
            line.erase();
        }
        drop(line);
        CHANGED.notify_all();
    }
}

/// Take the status line off the screen before other output: what the
/// crate's `eprintln!` and every result do first.
pub(crate) fn before_output() {
    if !ENABLED.load(Ordering::Acquire) {
        return;
    }
    let mut line = line();
    line.erase();
    line.hold_until = Some(Instant::now() + SETTLE);
}

/// Forget operations of commands no one waits for any more, such as one a
/// shell left running after Ctrl-C.
pub(crate) fn forget() {
    if !ENABLED.load(Ordering::Acquire) {
        return;
    }
    let mut line = line();
    line.stack.clear();
    line.erase();
}

/// The terminal belongs to something else while this lives: a prompt, or
/// a live view drawing in place.
#[must_use]
pub(crate) struct Paused(());
pub(crate) fn pause() -> Paused {
    let mut line = line();
    line.paused += 1;
    line.erase();
    drop(line);
    Paused(())
}
impl Drop for Paused {
    fn drop(&mut self) {
        let mut line = line();
        line.paused = line.paused.saturating_sub(1);
        drop(line);
        CHANGED.notify_all();
    }
}

// ---------------------------------------------------------------------------
// The camera picker

/// Whether a command without `-c` may ask a person which camera to use:
/// someone reads stderr and types on stdin, and nothing reads JSON.
pub(crate) fn can_pick(json: bool) -> bool {
    may_pick(json, stdin_is_tty(), stderr_is_tty())
}
const fn may_pick(json: bool, stdin: bool, stderr: bool) -> bool {
    !json && stdin && stderr
}

/// Ask which of `cameras` to use: a numbered list and a prompt on
/// `output`, the answer from `input`, Enter for the first. None when the
/// input ends or three answers were not numbers of the list.
pub(crate) fn pick(
    cameras: &[String],
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> io::Result<Option<usize>> {
    writeln!(output, "{} cameras found:", cameras.len())?;
    for (index, camera) in cameras.iter().enumerate() {
        let camera = crate::types::printable_line(camera);
        writeln!(output, "  {}) {camera}", index + 1)?;
    }
    for _ in 0..3 {
        write!(output, "Camera [1]: ")?;
        output.flush()?;
        let mut answer = String::new();
        if input.read_line(&mut answer)? == 0 {
            writeln!(output)?;
            return Ok(None);
        }
        let answer = answer.trim();
        if answer.is_empty() {
            return Ok(Some(0));
        }
        match answer.parse::<usize>() {
            Ok(number) if (1..=cameras.len()).contains(&number) => return Ok(Some(number - 1)),
            _ => writeln!(
                output,
                "Type a number from 1 to {}, or Enter for 1.",
                cameras.len()
            )?,
        }
    }
    Ok(None)
}

/// `pick`, the answer read on a thread of its own so that `stopped`, a
/// signal, ends the wait at once with an `Interrupted` error; the thread
/// is left waiting for a line no one reads.
pub(crate) fn pick_until<I: BufRead>(
    cameras: Vec<String>,
    input: impl FnOnce() -> I + Send + 'static,
    mut output: impl Write + Send + 'static,
    stopped: &dyn Fn() -> bool,
) -> io::Result<Option<usize>> {
    let (sender, answer) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("capturefab-pick".into())
        .spawn(move || {
            let _ = sender.send(pick(&cameras, &mut input(), &mut output));
        })?;
    loop {
        match answer.recv_timeout(Duration::from_millis(50)) {
            Ok(picked) => return picked,
            Err(RecvTimeoutError::Timeout) if stopped() => {
                return Err(io::ErrorKind::Interrupted.into());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Frames drawn inline

/// How a terminal draws an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Graphics {
    /// The kitty graphics protocol: kitty, Ghostty.
    Kitty,
    /// iTerm2 inline images: iTerm2, `WezTerm`.
    Iterm,
    /// Half blocks in 24-bit colour.
    TrueColor,
    /// Half blocks in the 256-colour palette.
    Colors256,
}

/// The graphics this terminal takes, from its environment; None for one
/// that takes no escape sequences. `CAPTUREFAB_IMAGES` (kitty, iterm,
/// truecolor, 256 or none) overrides what the variables suggest.
pub(crate) fn graphics() -> Option<Graphics> {
    if !escapes() {
        return None;
    }
    graphics_from(|name| std::env::var(name).ok())
}

pub(crate) fn graphics_from(var: impl Fn(&str) -> Option<String>) -> Option<Graphics> {
    let is = |name: &str, value: &str| var(name).is_some_and(|set| set == value);
    if let Some(chosen) = var("CAPTUREFAB_IMAGES") {
        let chosen = chosen.trim();
        for (name, graphics) in [
            ("kitty", Some(Graphics::Kitty)),
            ("iterm", Some(Graphics::Iterm)),
            ("iterm2", Some(Graphics::Iterm)),
            ("truecolor", Some(Graphics::TrueColor)),
            ("24bit", Some(Graphics::TrueColor)),
            ("256", Some(Graphics::Colors256)),
            ("none", None),
            ("off", None),
            ("0", None),
        ] {
            if chosen.eq_ignore_ascii_case(name) {
                return graphics;
            }
        }
    }
    if is("TERM", "dumb") {
        return None;
    }
    let truecolor = var("COLORTERM").is_some_and(|value| {
        value.eq_ignore_ascii_case("truecolor") || value.eq_ignore_ascii_case("24bit")
    }) || var("WT_SESSION").is_some();
    let half = if truecolor {
        Graphics::TrueColor
    } else {
        Graphics::Colors256
    };
    // Inside tmux or screen an image would need passing through; half
    // blocks are plain text.
    let term = var("TERM").unwrap_or_default();
    if var("TMUX").is_some() || term.starts_with("screen") || term.starts_with("tmux") {
        return Some(half);
    }
    if var("KITTY_WINDOW_ID").is_some()
        || term == "xterm-kitty"
        || term == "xterm-ghostty"
        || is("TERM_PROGRAM", "ghostty")
    {
        return Some(Graphics::Kitty);
    }
    // WezTerm draws kitty images only when configured to; its iTerm2
    // support is on by default.
    if is("TERM_PROGRAM", "iTerm.app")
        || is("LC_TERMINAL", "iTerm2")
        || is("TERM_PROGRAM", "WezTerm")
    {
        return Some(Graphics::Iterm);
    }
    Some(half)
}

/// An image as 8-bit RGB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rgb {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// The largest image drawn: previews are at most 2048 pixels on a side.
const MAX_PIXELS: u64 = 2048 * 2048;

/// Decode a PNG preview, or a JPEG one in builds with libjpeg-turbo.
pub(crate) fn decode(bytes: &[u8]) -> Result<Rgb> {
    if bytes.starts_with(b"\x89PNG") {
        return decode_png(bytes);
    }
    if bytes.starts_with(&[0xff, 0xd8]) {
        return decode_jpeg(bytes);
    }
    bail!("the preview is neither PNG nor JPEG")
}

fn decode_png(bytes: &[u8]) -> Result<Rgb> {
    let mut decoder = png::Decoder::new(bytes);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let header = decoder.read_header_info().context("reading the preview")?;
    let (width, height) = (header.width, header.height);
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        bail!("the preview is too large to draw ({width}x{height})");
    }
    let mut reader = decoder.read_info().context("reading the preview")?;
    let mut buffer = vec![0; reader.output_buffer_size()];
    let frame = reader
        .next_frame(&mut buffer)
        .context("reading the preview")?;
    let channels = frame.color_type.samples();
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 3);
    for row in buffer.chunks(frame.line_size).take(height as usize) {
        for pixel in row.chunks(channels).take(width as usize) {
            match pixel {
                [gray] | [gray, _] => pixels.extend([*gray; 3]),
                [r, g, b, ..] => pixels.extend([*r, *g, *b]),
                _ => bail!("unexpected preview pixels"),
            }
        }
    }
    Ok(Rgb {
        width,
        height,
        pixels,
    })
}

#[cfg(feature = "jpeg")]
fn decode_jpeg(bytes: &[u8]) -> Result<Rgb> {
    let mut decoder = turbojpeg::Decompressor::new().context("reading the preview")?;
    let header = decoder.read_header(bytes).context("reading the preview")?;
    let (width, height) = (header.width, header.height);
    if width.saturating_mul(height) > usize::try_from(MAX_PIXELS)? {
        bail!("the preview is too large to draw ({width}x{height})");
    }
    let mut pixels = vec![0; width * height * 3];
    decoder
        .decompress(
            bytes,
            turbojpeg::Image {
                pixels: pixels.as_mut_slice(),
                width,
                pitch: width * 3,
                height,
                format: turbojpeg::PixelFormat::RGB,
            },
        )
        .context("reading the preview")?;
    Ok(Rgb {
        width: u32::try_from(width)?,
        height: u32::try_from(height)?,
        pixels,
    })
}
#[cfg(not(feature = "jpeg"))]
fn decode_jpeg(_: &[u8]) -> Result<Rgb> {
    bail!("this build cannot read JPEG previews; ask for a PNG one")
}

/// `image` scaled to `width` × `height`, each pixel the mean of the ones it
/// covers.
fn resize(image: &Rgb, width: u32, height: u32) -> Result<Rgb> {
    let (width, height) = (width.max(1), height.max(1));
    let (sw, sh) = (u64::from(image.width), u64::from(image.height));
    let (tw, th) = (u64::from(width), u64::from(height));
    if tw * th > MAX_PIXELS {
        bail!("terminal preview of {width}x{height} exceeds the pixel limit");
    }
    let mut pixels = Vec::with_capacity(usize::try_from(tw * th * 3)?);
    for y in 0..th {
        let (y0, y1) = (
            y * sh / th,
            ((y + 1) * sh / th).max(y * sh / th + 1).min(sh),
        );
        for x in 0..tw {
            let (x0, x1) = (
                x * sw / tw,
                ((x + 1) * sw / tw).max(x * sw / tw + 1).min(sw),
            );
            let mut sum = [0u64; 3];
            for sy in y0..y1 {
                let row = usize::try_from(sy * sw)? * 3;
                for sx in x0..x1 {
                    let at = row + usize::try_from(sx)? * 3;
                    for (total, value) in sum.iter_mut().zip(&image.pixels[at..at + 3]) {
                        *total += u64::from(*value);
                    }
                }
            }
            let count = ((y1 - y0) * (x1 - x0)).max(1);
            pixels.extend(sum.map(|total| {
                u8::try_from(total / count).expect("a mean RGB channel is within 0..255")
            }));
        }
    }
    Ok(Rgb {
        width,
        height,
        pixels,
    })
}

/// The cells an image of `width` × `height` pixels fills within `cols` ×
/// `rows`, keeping its shape: a cell is about twice as tall as wide.
pub(crate) fn cells(width: u32, height: u32, cols: u16, rows: u16) -> (u16, u16) {
    let (width, height) = (u64::from(width.max(1)), u64::from(height.max(1)));
    let (cols, rows) = (cols.max(1), rows.max(1));
    let across = u64::from(cols) * height;
    let down = 2 * u64::from(rows) * width;
    if across <= down {
        let fitted_rows = across.div_ceil(2 * width).max(1);
        (
            cols,
            u16::try_from(fitted_rows).expect("fitted rows are within the terminal"),
        )
    } else {
        let fitted_cols = ((down + height / 2) / height).max(1);
        (
            u16::try_from(fitted_cols).expect("fitted columns are within the terminal"),
            rows,
        )
    }
}

/// What draws an image in a terminal: `rows` lines from the cursor's line,
/// after which the cursor is at the start of the next line.
pub(crate) struct Picture {
    pub rows: u16,
    pub text: String,
}

/// Draw the PNG or JPEG `encoded` in at most `cols` × `rows` cells. `id`
/// names a kitty image and its one placement, which each new picture with
/// it replaces, as a live view's frames do.
pub(crate) fn picture(
    encoded: &[u8],
    graphics: Graphics,
    cols: u16,
    rows: u16,
    id: Option<u32>,
) -> Result<Picture> {
    let image = decode(encoded)?;
    let (cols, rows) = cells(image.width, image.height, cols, rows);
    let text = match graphics {
        Graphics::TrueColor | Graphics::Colors256 => {
            half_blocks(&image, cols, rows, graphics == Graphics::TrueColor)?
        }
        Graphics::Kitty => {
            let png = if encoded.starts_with(b"\x89PNG") {
                encoded.to_vec()
            } else {
                encode_png(&image)?
            };
            placed(&kitty(&png, cols, rows, id), rows)
        }
        Graphics::Iterm => placed(&iterm(encoded, cols, rows), rows),
    };
    Ok(Picture { rows, text })
}

/// An image escape drawn in `rows` cleared lines, the cursor ending below
/// them whatever the terminal does with it after an image.
fn placed(image: &str, rows: u16) -> String {
    let mut text = "\x1b[2K\n".repeat(usize::from(rows));
    let _ = write!(text, "\x1b[{rows}A\x1b7{image}\x1b8\x1b[{rows}B\r");
    text
}

fn encode_png(image: &Rgb) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, image.width, image.height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    encoder.write_header()?.write_image_data(&image.pixels)?;
    Ok(bytes)
}

fn base64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The kitty escapes that place PNG `png` over `cols` × `rows` cells
/// without moving the cursor, asking for no reply.
fn kitty(png: &[u8], cols: u16, rows: u16, id: Option<u32>) -> String {
    let data = base64(png);
    let chunks = data.as_bytes().chunks(4096).collect::<Vec<_>>();
    let mut text = String::new();
    for (index, chunk) in chunks.iter().enumerate() {
        let more = u8::from(index + 1 < chunks.len());
        if index == 0 {
            // One placement of the image: each new picture replaces it.
            let id = id.map(|id| format!(",i={id},p=1")).unwrap_or_default();
            let _ = write!(
                text,
                "\x1b_Ga=T,f=100,q=2,C=1,c={cols},r={rows}{id},m={more};"
            );
        } else {
            let _ = write!(text, "\x1b_Gm={more};");
        }
        text.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        text.push_str("\x1b\\");
    }
    text
}

/// The iTerm2 escape that draws `encoded` over `cols` × `rows` cells.
fn iterm(encoded: &[u8], cols: u16, rows: u16) -> String {
    format!(
        "\x1b]1337;File=inline=1;size={};width={cols};height={rows};preserveAspectRatio=1:{}\x07",
        encoded.len(),
        base64(encoded)
    )
}

/// The nearest colour of the xterm 256-colour palette: its 6×6×6 cube, or
/// its grey ramp for greys.
fn color256([r, g, b]: [u8; 3]) -> u8 {
    let (high, low) = (r.max(g).max(b), r.min(g).min(b));
    if high - low < 12 {
        let grey = (u16::from(r) + u16::from(g) + u16::from(b)) / 3;
        return match grey {
            0..8 => 16,
            239.. => 231,
            grey => 232 + ((grey - 8) / 10).min(23) as u8,
        };
    }
    let level = |value: u8| match value {
        0..48 => 0,
        48..115 => 1,
        value => (value - 35) / 40,
    };
    16 + 36 * level(r) + 6 * level(g) + level(b)
}

/// `image` as rows of upper half blocks, a cell showing two pixels: the
/// upper one in its foreground colour, the lower in its background.
fn half_blocks(image: &Rgb, cols: u16, rows: u16, truecolor: bool) -> Result<String> {
    let small = resize(image, u32::from(cols), u32::from(rows) * 2)?;
    let width = small.width as usize;
    let pixel = |x: usize, y: usize| {
        let at = (y * width + x) * 3;
        [small.pixels[at], small.pixels[at + 1], small.pixels[at + 2]]
    };
    let color = |layer: u8, [r, g, b]: [u8; 3]| {
        if truecolor {
            format!("\x1b[{layer}8;2;{r};{g};{b}m")
        } else {
            format!("\x1b[{layer}8;5;{}m", color256([r, g, b]))
        }
    };
    let mut text = String::new();
    for row in 0..usize::from(rows) {
        text.push_str("\x1b[2K");
        let (mut fg, mut bg) = (None, None);
        for x in 0..width {
            let (top, bottom) = (pixel(x, row * 2), pixel(x, row * 2 + 1));
            if fg != Some(top) {
                text.push_str(&color(3, top));
                fg = Some(top);
            }
            if bg != Some(bottom) {
                text.push_str(&color(4, bottom));
                bg = Some(bottom);
            }
            text.push('▀');
        }
        text.push_str("\x1b[0m\n");
    }
    Ok(text)
}

/// The most cells a picture takes: the terminal's width, and its height
/// less `keep` rows for the text around it.
pub(crate) fn picture_box(keep: u16) -> (u16, u16) {
    let (cols, rows) = size().unwrap_or((80, 24));
    (
        cols.saturating_sub(1).clamp(16, 160),
        rows.saturating_sub(keep).clamp(4, 60),
    )
}

/// The longest edge of a preview worth asking for to fill `cols` × `rows`
/// cells: a pixel a half cell for half blocks, more for terminals that draw
/// images themselves.
pub(crate) fn preview_edge(graphics: Graphics, cols: u16, rows: u16) -> u32 {
    let cells = u32::from(cols).max(2 * u32::from(rows));
    let edge = match graphics {
        Graphics::TrueColor | Graphics::Colors256 => cells,
        Graphics::Kitty | Graphics::Iterm => cells * 8,
    };
    edge.clamp(64, 1024)
}

#[cfg(unix)]
mod platform {
    pub(super) const fn escapes() -> bool {
        true
    }
    pub(super) fn size() -> Option<(u16, u16)> {
        [1, 2, 0].into_iter().find_map(|fd| {
            // SAFETY: winsize contains integer fields, for which zero is valid.
            let mut size: libc::winsize = unsafe { std::mem::zeroed() };
            // SAFETY: TIOCGWINSZ writes one winsize into the local and
            // reports an error for an invalid or non-terminal descriptor.
            let read = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0;
            (read && size.ws_col > 0 && size.ws_row > 0).then_some((size.ws_col, size.ws_row))
        })
    }
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::System::Console::{
        CONSOLE_SCREEN_BUFFER_INFO, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
        GetConsoleScreenBufferInfo, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
        SetConsoleMode,
    };

    /// Turn on escape sequences for the console's stdout and stderr; false
    /// when a console does not take them.
    pub(super) fn escapes() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            let mut on = true;
            for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                // SAFETY: which names this process's stdout or stderr.
                let handle = unsafe { GetStdHandle(which) };
                let mut mode = 0;
                // SAFETY: the console API validates the handle and writes to a local.
                if unsafe { GetConsoleMode(handle, &raw mut mode) } != 0
                    && mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING == 0
                    // SAFETY: this handle is a console; only its processing mode changes.
                    && unsafe { SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) } == 0
                {
                    on = false;
                }
            }
            on
        })
    }
    pub(super) fn size() -> Option<(u16, u16)> {
        [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
            .into_iter()
            .find_map(|which| {
                // SAFETY: zero initializes all fields before the console API fills them.
                let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
                // SAFETY: which names this process's stdout or stderr.
                let handle = unsafe { GetStdHandle(which) };
                // SAFETY: info is writable; the API validates the console handle.
                if unsafe { GetConsoleScreenBufferInfo(handle, &raw mut info) } == 0 {
                    return None;
                }
                let window = info.srWindow;
                let cols =
                    u16::try_from(i32::from(window.Right) - i32::from(window.Left) + 1).ok()?;
                let rows =
                    u16::try_from(i32::from(window.Bottom) - i32::from(window.Top) + 1).ok()?;
                (cols > 0 && rows > 0).then_some((cols, rows))
            })
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    pub(super) fn escapes() -> bool {
        true
    }
    pub(super) fn size() -> Option<(u16, u16)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn detect(vars: &[(&str, &str)]) -> Option<Graphics> {
        let vars = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect::<HashMap<_, _>>();
        graphics_from(|name| vars.get(name).cloned())
    }

    #[test]
    fn terminals_are_told_apart_by_their_variables() {
        assert_eq!(detect(&[("TERM", "xterm-kitty")]), Some(Graphics::Kitty));
        assert_eq!(
            detect(&[("TERM", "xterm-256color"), ("KITTY_WINDOW_ID", "1")]),
            Some(Graphics::Kitty)
        );
        assert_eq!(
            detect(&[("TERM_PROGRAM", "ghostty")]),
            Some(Graphics::Kitty)
        );
        assert_eq!(
            detect(&[("TERM_PROGRAM", "iTerm.app"), ("COLORTERM", "truecolor")]),
            Some(Graphics::Iterm)
        );
        assert_eq!(detect(&[("LC_TERMINAL", "iTerm2")]), Some(Graphics::Iterm));
        assert_eq!(
            detect(&[("TERM_PROGRAM", "WezTerm")]),
            Some(Graphics::Iterm)
        );
        assert_eq!(
            detect(&[("TERM", "xterm-256color"), ("COLORTERM", "truecolor")]),
            Some(Graphics::TrueColor)
        );
        assert_eq!(detect(&[("WT_SESSION", "x")]), Some(Graphics::TrueColor));
        assert_eq!(
            detect(&[("TERM", "xterm-256color")]),
            Some(Graphics::Colors256)
        );
        assert_eq!(detect(&[]), Some(Graphics::Colors256));
        // tmux would swallow an image: half blocks, even in kitty.
        assert_eq!(
            detect(&[
                ("TERM", "tmux-256color"),
                ("KITTY_WINDOW_ID", "1"),
                ("COLORTERM", "truecolor")
            ]),
            Some(Graphics::TrueColor)
        );
        assert_eq!(
            detect(&[("TMUX", "/tmp/tmux"), ("TERM_PROGRAM", "iTerm.app")]),
            Some(Graphics::Colors256)
        );
        assert_eq!(detect(&[("TERM", "dumb")]), None);
        // A person can say what their terminal draws.
        assert_eq!(
            detect(&[("TERM", "dumb"), ("CAPTUREFAB_IMAGES", "truecolor")]),
            Some(Graphics::TrueColor)
        );
        assert_eq!(
            detect(&[("TERM", "xterm-kitty"), ("CAPTUREFAB_IMAGES", "none")]),
            None
        );
        assert_eq!(
            detect(&[("CAPTUREFAB_IMAGES", "Kitty")]),
            Some(Graphics::Kitty)
        );
    }

    #[test]
    fn graphics_overrides_keep_aliases_case_and_precedence() {
        for (name, expected) in [
            ("kitty", Some(Graphics::Kitty)),
            ("iterm", Some(Graphics::Iterm)),
            ("iterm2", Some(Graphics::Iterm)),
            ("truecolor", Some(Graphics::TrueColor)),
            ("24bit", Some(Graphics::TrueColor)),
            ("256", Some(Graphics::Colors256)),
            ("none", None),
            ("off", None),
            ("0", None),
        ] {
            let chosen = format!(" \t{}\n", name.to_ascii_uppercase());
            assert_eq!(
                detect(&[("TERM", "dumb"), ("CAPTUREFAB_IMAGES", &chosen)]),
                expected
            );
        }
        assert_eq!(
            detect(&[("COLORTERM", "TRUECOLOR")]),
            Some(Graphics::TrueColor)
        );
        assert_eq!(
            detect(&[("COLORTERM", " truecolor ")]),
            Some(Graphics::Colors256)
        );
        assert_eq!(
            detect(&[("TERM", "xterm-kitty"), ("CAPTUREFAB_IMAGES", "unknown")]),
            Some(Graphics::Kitty)
        );
    }

    fn rgb(width: u32, height: u32, pixels: &[[u8; 3]]) -> Rgb {
        Rgb {
            width,
            height,
            pixels: pixels.iter().flatten().copied().collect(),
        }
    }

    #[test]
    fn half_blocks_put_two_pixels_in_a_cell() {
        let red = [255, 0, 0];
        let blue = [0, 0, 255];
        let image = rgb(2, 2, &[red, red, blue, red]);
        assert_eq!(
            half_blocks(&image, 2, 1, true).unwrap(),
            "\x1b[2K\x1b[38;2;255;0;0m\x1b[48;2;0;0;255m▀\x1b[48;2;255;0;0m▀\x1b[0m\n"
        );
        assert_eq!(
            half_blocks(&image, 2, 1, false).unwrap(),
            "\x1b[2K\x1b[38;5;196m\x1b[48;5;21m▀\x1b[48;5;196m▀\x1b[0m\n"
        );
        // Two cells' rows from four pixel rows, each its own line.
        let tall = rgb(1, 4, &[red, blue, blue, red]);
        let text = half_blocks(&tall, 1, 2, true).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.starts_with("\x1b[2K\x1b[38;2;255;0;0m\x1b[48;2;0;0;255m▀"));
    }

    #[test]
    fn terminal_geometry_handles_ratios_and_extreme_dimensions() {
        for (width, height, cols, rows, expected) in [
            (3, 4, 5, 7, (5, 4)),
            (7, 3, 10, 2, (9, 2)),
            (5, 4, 9, 1, (3, 1)),
            (u32::MAX, u32::MAX, u16::MAX, u16::MAX, (u16::MAX, 32_768)),
            (u32::MAX, 1, 80, 24, (80, 1)),
            (1, u32::MAX, 80, 24, (1, 24)),
            (0, 0, 0, 0, (1, 1)),
        ] {
            assert_eq!(cells(width, height, cols, rows), expected);
        }
        let image = rgb(1, 1, &[[0, 0, 0]]);
        resize(&image, u32::MAX, u32::MAX).unwrap_err();
        half_blocks(&image, u16::MAX, u16::MAX, true).unwrap_err();
    }

    #[test]
    fn images_shrink_to_mean_colours_and_keep_their_shape() {
        let image = rgb(
            2,
            2,
            &[[0, 0, 0], [200, 100, 50], [100, 50, 0], [100, 50, 50]],
        );
        assert_eq!(resize(&image, 1, 1).unwrap().pixels, vec![100, 50, 25]);
        // A 4:3 frame in 80x24 cells: as wide as fits, half as many rows
        // as pixel rows.
        assert_eq!(cells(640, 480, 80, 24), (64, 24));
        assert_eq!(cells(640, 480, 40, 24), (40, 15));
        assert_eq!(cells(100, 2000, 80, 10), (1, 10));
        assert_eq!(color256([0, 0, 0]), 16);
        assert_eq!(color256([255, 255, 255]), 231);
        assert_eq!(color256([128, 128, 128]), 244);
        assert_eq!(color256([0, 255, 0]), 46);
    }

    #[test]
    fn previews_decode_from_png_whatever_their_colour_type() {
        let gray = {
            let mut bytes = Vec::new();
            let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Sixteen);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&[0xff, 0xff, 0x80, 0x00])
                .unwrap();
            bytes
        };
        assert_eq!(decode(&gray).unwrap(), rgb(2, 1, &[[255; 3], [128; 3]]));
        let color = encode_png(&rgb(1, 1, &[[1, 2, 3]])).unwrap();
        assert_eq!(decode(&color).unwrap(), rgb(1, 1, &[[1, 2, 3]]));
        decode(b"GIF89a").unwrap_err();
        // Each graphics kind draws in the rows it says.
        for graphics in [
            Graphics::Kitty,
            Graphics::Iterm,
            Graphics::TrueColor,
            Graphics::Colors256,
        ] {
            let picture = picture(&gray, graphics, 10, 5, Some(7)).unwrap();
            assert_eq!(picture.rows, 3, "{graphics:?}");
            assert!(picture.text.ends_with('\n') || picture.text.ends_with('\r'));
        }
        let kitty = picture(&gray, Graphics::Kitty, 10, 5, Some(7))
            .unwrap()
            .text;
        assert!(
            kitty.contains("\x1b_Ga=T,f=100,q=2,C=1,c=10,r=3,i=7,p=1,m=0;"),
            "{kitty:?}"
        );
        let iterm = picture(&gray, Graphics::Iterm, 10, 5, None).unwrap().text;
        assert!(iterm.contains("\x1b]1337;File=inline=1;size="), "{iterm:?}");
    }

    #[test]
    fn png_preview_limits_are_checked_before_reading_image_metadata() {
        let mut bytes = Vec::new();
        png::Encoder::new(&mut bytes, 2049, 2048)
            .write_header()
            .unwrap();
        assert_eq!(
            decode(&bytes).unwrap_err().to_string(),
            "the preview is too large to draw (2049x2048)"
        );
    }

    #[cfg(feature = "jpeg")]
    #[test]
    fn jpeg_preview_limits_are_checked_before_decoding_pixels() {
        let jpeg = turbojpeg::compress(
            turbojpeg::Image {
                pixels: &[96; 3],
                width: 1,
                pitch: 3,
                height: 1,
                format: turbojpeg::PixelFormat::RGB,
            },
            90,
            turbojpeg::Subsamp::None,
        )
        .unwrap()
        .to_vec();
        let image = decode(&jpeg).unwrap();
        assert_eq!((image.width, image.height), (1, 1));
        assert_eq!(image.pixels, [96; 3]);

        let mut oversized = jpeg;
        let sof = oversized
            .windows(2)
            .position(|bytes| bytes == [0xff, 0xc0])
            .unwrap();
        oversized[sof + 5..sof + 7].copy_from_slice(&2048u16.to_be_bytes());
        oversized[sof + 7..sof + 9].copy_from_slice(&2049u16.to_be_bytes());
        let scan = oversized
            .windows(2)
            .position(|bytes| bytes == [0xff, 0xda])
            .unwrap();
        let length = usize::from(u16::from_be_bytes([
            oversized[scan + 2],
            oversized[scan + 3],
        ]));
        oversized.truncate(scan + 2 + length);
        assert_eq!(
            decode(&oversized).unwrap_err().to_string(),
            "the preview is too large to draw (2049x2048)"
        );
    }

    #[test]
    fn kitty_images_go_in_chunks_of_4096() {
        let text = kitty(&[7u8; 6000], 4, 2, None);
        let chunks = text.matches("\x1b_G").count();
        assert_eq!(chunks, 2);
        assert!(text.contains(",m=1;"));
        assert!(text.contains("\x1b_Gm=0;"));
    }

    #[test]
    fn the_status_line_names_slow_operations_and_fits_the_terminal() {
        let second = Duration::from_secs(1);
        assert_eq!(
            status_text("connecting to sim:0…", second / 2, false, 80),
            "connecting to sim:0…"
        );
        assert_eq!(
            status_text("connecting to sim:0…", second * 3, false, 80),
            "connecting to sim:0… 3s"
        );
        assert_eq!(
            status_text("3/10 frames", second * 3, true, 80),
            "3/10 frames"
        );
        assert_eq!(status_text("abcdefghij", second, true, 6), "abcd…");
        // A camera's name in the line cannot steer the terminal.
        assert_eq!(
            status_text("connecting to Cam\x1b[2J\n…", second / 2, false, 80),
            "connecting to Cam\u{fffd}[2J …"
        );
        assert_eq!(clock(Duration::from_secs(7)), "0:07");
        assert_eq!(clock(Duration::from_secs(750)), "12:30");
        assert_eq!(clock(Duration::from_secs(3723)), "1:02:03");
    }

    #[test]
    fn the_picker_takes_a_number_or_enter() {
        let cameras = ["gige:a".to_owned(), "u3v:b  Cam\x1b[2J".to_owned()];
        let mut shown = Vec::new();
        let picked = pick(&cameras, &mut b"\n".as_slice(), &mut shown).unwrap();
        assert_eq!(picked, Some(0));
        let shown = String::from_utf8(shown).unwrap();
        assert!(
            shown.contains("  1) gige:a\n  2) u3v:b  Cam\u{fffd}[2J\n"),
            "{shown}"
        );
        let picked = pick(&cameras, &mut b"x\n9\n2\n".as_slice(), &mut Vec::new()).unwrap();
        assert_eq!(picked, Some(1));
        assert_eq!(
            pick(&cameras, &mut b"".as_slice(), &mut Vec::new()).unwrap(),
            None
        );
        assert_eq!(
            pick(&cameras, &mut b"a\nb\nc\n1\n".as_slice(), &mut Vec::new()).unwrap(),
            None
        );
        // Nothing asks where no one can answer, or where JSON is read:
        // there a command without -c fails with camera_ambiguous as before.
        assert!(may_pick(false, true, true));
        assert!(!may_pick(true, true, true));
        assert!(!may_pick(false, false, true));
        assert!(!may_pick(false, true, false));
        assert!(!can_pick(true));
    }

    /// Input that has no line yet, until its sender goes.
    struct Silent(std::sync::mpsc::Receiver<()>);
    impl io::Read for Silent {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            let _ = self.0.recv();
            Ok(0)
        }
    }

    #[test]
    fn a_signal_stops_the_picker_without_a_camera() {
        let cameras = || vec!["gige:a".to_owned(), "u3v:b".to_owned()];
        let picked = pick_until(cameras(), || b"2\n".as_slice(), io::sink(), &|| false);
        assert_eq!(picked.unwrap(), Some(1));
        // Ctrl-C while the prompt waits: no camera, at once, though no line
        // came, nor would Enter pick the first one afterwards.
        let (_typing, silent) = std::sync::mpsc::channel();
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let began = Instant::now();
        let stopped = pick_until(
            cameras(),
            move || io::BufReader::new(Silent(silent)),
            io::sink(),
            &|| asked.fetch_add(1, Ordering::Relaxed) >= 2,
        );
        assert_eq!(stopped.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert!(began.elapsed() < Duration::from_secs(5));
    }
}
