//! Choosing a recording's encoder: the codec families and the encoders
//! of each, tried in order, each proved by a test encode at the
//! recording's size before it is used, and the answers remembered for the
//! `FFmpeg` build and the boot. Jetson's `GStreamer` encoders are checked the
//! same way.
use super::*;

pub(super) fn codec_family(codec: &str) -> Result<&'static str> {
    match codec {
        "h264" | "avc" => Ok("h264"),
        "h265" | "hevc" => Ok("hevc"),
        "av1" => Ok("av1"),
        "ffv1" => Ok("ffv1"),
        _ => bail!("unsupported forwarding codec {codec}; use h264, hevc, av1 or ffv1"),
    }
}
/// The codec an `FFmpeg` encoder writes, `h264`, `hevc` or `av1`, or None for
/// encoders of other codecs.
pub(crate) fn encoder_family(encoder: &str) -> Option<&'static str> {
    match encoder {
        "h264" | "libx264" | "libx264rgb" | "libopenh264" | "nvv4l2h264enc" => Some("h264"),
        "hevc" | "libx265" | "nvv4l2h265enc" => Some("hevc"),
        "av1" | "libsvtav1" | "libaom-av1" | "librav1e" | "nvv4l2av1enc" => Some("av1"),
        "ffv1" => Some("ffv1"),
        name if name.starts_with("h264_") => Some("h264"),
        name if name.starts_with("hevc_") => Some("hevc"),
        name if name.starts_with("av1_") => Some("av1"),
        _ => None,
    }
}
pub(crate) fn validate_encoder(codec: &str, encoder: &str) -> Result<()> {
    let family = codec_family(codec)?;
    ensure!(
        !encoder.is_empty()
            && encoder.len() <= 64
            && encoder.as_bytes()[0].is_ascii_alphanumeric()
            && encoder
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
        "invalid forwarding encoder name; use auto or an FFmpeg video encoder such as libx264"
    );
    if let Some(encoder_family) = encoder_family(encoder) {
        ensure!(
            family == encoder_family,
            "forwarding encoder {encoder} encodes {encoder_family}, but the selected codec is {family}"
        );
    }
    Ok(())
}

pub(crate) fn validate_bitrate(bitrate: &str) -> Result<()> {
    bits_per_second(bitrate).map(|_| ())
}
/// What probing one `FFmpeg` found: remembered for the process and, beside a
/// shared extracted copy, for the rest of the boot. A missing, corrupt or
/// older file is ignored and the probes run again.
#[derive(Default, Serialize, Deserialize)]
pub(super) struct Probes {
    #[serde(default)]
    pub(super) boot: String,
    /// `ffmpeg -encoders` output.
    #[serde(default)]
    pub(super) listing: Option<String>,
    /// Test encodes by encoder, input and size, such as `h264_nvenc yuv 1920x1080`.
    #[serde(default)]
    pub(super) encoders: std::collections::BTreeMap<String, Found>,
    /// `ffmpeg -hwaccels` output.
    #[serde(default)]
    pub(super) hwaccels: Option<String>,
    /// `ffmpeg -decoders` output, asked for only where a method needs it.
    #[serde(default)]
    pub(super) decoder_listing: Option<String>,
    /// Hardware decoding by method, codec and size class, such as
    /// `videotoolbox hevc uhd`, and each method's probe (`hwdec`).
    #[serde(default)]
    pub(super) decoders: std::collections::BTreeMap<String, Found>,
}
/// A probe's answer and when it was found, in UTC Unix milliseconds.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(super) struct Found {
    pub(super) works: bool,
    pub(super) at_ms: u64,
}
/// How long a failed probe is believed: a busy device, for example one with
/// all its encode sessions in use, can refuse once and work soon after.
pub(super) const FAILURE_KEPT: Duration = Duration::from_secs(300);
impl Found {
    pub(super) fn now(works: bool) -> Self {
        Self {
            works,
            at_ms: crate::scheduling::now_ms(),
        }
    }
    /// The answer, unless it is a failure found too long ago.
    pub(super) fn current(self) -> Option<bool> {
        let age = crate::scheduling::now_ms().checked_sub(self.at_ms);
        (self.works || age.is_some_and(|age| u128::from(age) < FAILURE_KEPT.as_millis()))
            .then_some(self.works)
    }
    /// The later of two answers.
    pub(super) fn newer(self, other: Option<Self>) -> Self {
        other
            .filter(|other| other.at_ms > self.at_ms)
            .unwrap_or(self)
    }
}
pub(super) static PROBES: OnceLock<Mutex<std::collections::HashMap<PathBuf, Probes>>> =
    OnceLock::new();
pub(super) fn with_probes<T>(exe: &Executable, read: impl FnOnce(&mut Probes) -> T) -> T {
    let mut memo = PROBES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    read(
        memo.entry(exe.path.clone())
            .or_insert_with(|| load_probes(exe)),
    )
}
/// A remembered probe answer of `exe`, or the answer of running the probe.
/// A probe that cannot tell, for example because it timed out, returns
/// `None`, which is not remembered.
pub(super) fn probed(
    exe: &Executable,
    get: impl FnOnce(&Probes) -> Option<Found>,
    set: impl FnOnce(&mut Probes, Found),
    run: impl FnOnce() -> Result<Option<bool>>,
) -> Result<Option<bool>> {
    if let Some(works) = with_probes(exe, |probes| get(probes)).and_then(Found::current) {
        return Ok(Some(works));
    }
    // Probe unlocked: a test encode can take seconds.
    let works = run()?;
    if let Some(works) = works {
        remember(exe, |probes| set(probes, Found::now(works)));
    }
    Ok(works)
}
pub(super) fn remember(exe: &Executable, change: impl FnOnce(&mut Probes)) {
    with_probes(exe, |probes| {
        change(probes);
        save_probes(exe, probes);
    });
}
pub(super) fn load_probes(exe: &Executable) -> Probes {
    let stored = exe
        .probes
        .as_ref()
        .and_then(|path| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Probes>(&bytes).ok());
    stored
        .filter(|probes| boot_id().is_some_and(|boot| boot == probes.boot))
        .unwrap_or_default()
}
pub(super) fn save_probes(exe: &Executable, probes: &mut Probes) {
    let (Some(path), Some(boot)) = (&exe.probes, boot_id()) else {
        return;
    };
    // Keep what other processes found since this one read the file.
    let stored = load_probes(exe);
    boot.clone_into(&mut probes.boot);
    probes.listing = probes.listing.take().or(stored.listing);
    probes.hwaccels = probes.hwaccels.take().or(stored.hwaccels);
    probes.decoder_listing = probes.decoder_listing.take().or(stored.decoder_listing);
    for (key, found) in stored.encoders {
        let mine = probes.encoders.entry(key).or_insert(found);
        *mine = mine.newer(Some(found));
    }
    for (key, found) in stored.decoders {
        let mine = probes.decoders.entry(key).or_insert(found);
        *mine = mine.newer(Some(found));
    }
    let Ok(bytes) = serde_json::to_vec(probes) else {
        return;
    };
    // The folder of an installed FFmpeg's probes, in the cache, is made
    // on first use.
    if let Some(dir) = path.parent()
        && !dir.is_dir()
        && crate::dirs::ensure_private_dir(dir).is_err()
    {
        return;
    }
    let _ = crate::dirs::write_private(path, &bytes, false);
}
/// Identifies this boot, so remembered probes do not outlive a driver or
/// hardware change. Without one, probes are remembered by the process only.
pub(crate) fn boot_id() -> Option<&'static str> {
    static BOOT: OnceLock<Option<String>> = OnceLock::new();
    BOOT.get_or_init(|| {
        let id = if cfg!(target_os = "linux") {
            fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()
        } else if cfg!(target_os = "macos") {
            Command::new("/usr/sbin/sysctl")
                .args(["-n", "kern.boottime"])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            None
        };
        id.map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty())
    })
    .as_deref()
}
pub(super) fn encoder_listing(exe: &Executable) -> Result<String> {
    if let Some(listing) = with_probes(exe, |probes| probes.listing.clone()) {
        return Ok(listing);
    }
    let (ok, listing) = bounded_output(
        exe,
        &["-hide_banner".into(), "-encoders".into()],
        Duration::from_secs(5),
    )?;
    ensure!(ok, "FFmpeg could not list its encoders");
    remember(exe, |probes| probes.listing = Some(listing.clone()));
    Ok(listing)
}
pub(super) fn listed(listing: &str, encoder: &str) -> bool {
    listing
        .lines()
        .any(|line| line.split_whitespace().nth(1) == Some(encoder))
}
/// A probe's key: the encoder, how its input is converted, the pixel
/// format the frames reach `FFmpeg` in (`lavfi` for a generated test frame)
/// and the size, such as `h264_nvenc yuv bayer_rggb8 1920x1080`.
pub(super) fn probe_key(
    encoder: &str,
    input: EncoderInput,
    source: Option<&str>,
    width: u32,
    height: u32,
) -> String {
    let input = match input {
        EncoderInput::Yuv => "yuv",
        EncoderInput::Rgb => "rgb",
    };
    let source = source.unwrap_or("lavfi");
    format!("{encoder} {input} {source} {width}x{height}")
}
/// One test frame at the recording's size, in the pixel format the
/// recording's frames will reach `FFmpeg` in when that is known: hardware
/// encoders have size limits that a small probe would pass, and a format
/// conversion the encoder's input needs can fail where another does not.
/// A probe that cannot tell, because it timed out or was killed from
/// outside, as by a memory limit, is `None` and not remembered.
pub(super) fn encoder_works(
    exe: &Executable,
    encoder: &str,
    input: EncoderInput,
    source: Option<&RawInput>,
    width: u32,
    height: u32,
) -> Result<Option<bool>> {
    let pix_fmt = source.map(|raw| raw.input_pix_fmt(encoder));
    let key = probe_key(encoder, input, pix_fmt, width, height);
    probed(
        exe,
        |probes| probes.encoders.get(&key).copied(),
        |probes, found| {
            probes.encoders.insert(key.clone(), found);
        },
        || probe_encoder(exe, encoder, input, source, width, height),
    )
}
pub(super) fn jetson_works(exe: &Executable, encoder: &str) -> Result<Option<bool>> {
    let key = format!("{encoder} gstreamer");
    probed(
        exe,
        |probes| probes.encoders.get(&key).copied(),
        |probes, found| {
            probes.encoders.insert(key.clone(), found);
        },
        || Ok(probe_jetson(encoder)),
    )
}
/// The encoder is in this `FFmpeg` build, or is a Jetson `GStreamer` element,
/// and encodes a test frame of this size, in `source`'s format when it is
/// known; `None` when its test encode could not tell.
pub(super) fn usable(
    exe: &Executable,
    encoder: &str,
    width: u32,
    height: u32,
    source: Option<&RawInput>,
) -> Result<Option<bool>> {
    if jetson_encoder(encoder) {
        return jetson_works(exe, encoder);
    }
    // One that just failed the small check frame has no device here.
    let small = probe_key(encoder, EncoderInput::Yuv, None, PROBE_SIZE.0, PROBE_SIZE.1);
    let failed = with_probes(exe, |probes| probes.encoders.get(&small).copied())
        .and_then(Found::current)
        == Some(false);
    if !listed(&encoder_listing(exe)?, encoder) || failed {
        return Ok(Some(false));
    }
    encoder_works(exe, encoder, EncoderInput::Yuv, source, width, height)
}
/// The size probed before a recording's first frame says its real size.
pub(super) const PROBE_SIZE: (u32, u32) = (640, 480);
/// Encoders to try for a codec on this OS, all in the bundled `FFmpeg` or on
/// Jetson: vendor encoders first, then cross-vendor Vulkan Video, then
/// software. Another `FFmpeg`'s encoders are used when a forward names one.
pub(super) fn encoder_candidates(family: &str) -> &'static [&'static str] {
    match (family, std::env::consts::OS) {
        ("h264", "macos") => &["h264_videotoolbox", "libx264"],
        ("hevc", "macos") => &["hevc_videotoolbox"],
        ("h264", "linux") => &[
            "h264_nvenc",
            "nvv4l2h264enc",
            "h264_amf",
            "h264_vulkan",
            "h264_v4l2m2m",
            "libx264",
        ],
        ("hevc", "linux") => &[
            "hevc_nvenc",
            "nvv4l2h265enc",
            "hevc_amf",
            "hevc_vulkan",
            "hevc_v4l2m2m",
        ],
        ("av1", "linux") => &["av1_nvenc", "nvv4l2av1enc", "av1_amf", "av1_vulkan"],
        ("h264", "windows") => &[
            "h264_nvenc",
            "h264_qsv",
            "h264_amf",
            "h264_vulkan",
            "h264_mf",
            "libx264",
        ],
        ("hevc", "windows") => &[
            "hevc_nvenc",
            "hevc_qsv",
            "hevc_amf",
            "hevc_vulkan",
            "hevc_mf",
        ],
        ("av1", "windows") => &["av1_nvenc", "av1_qsv", "av1_amf", "av1_vulkan", "av1_mf"],
        // Lossless, in FFmpeg itself on every OS.
        ("ffv1", _) => &["ffv1"],
        ("h264", _) => &["libx264"],
        ("av1", "macos") | ("hevc", _) => &[],
        _ => &["av1_nvenc"],
    }
}
/// Every candidate must be in the `FFmpeg` build and pass a real test encode
/// at the recording's size before it is chosen. `skip` holds encoders that
/// already failed this recording.
pub(super) fn select_encoder(
    exe: &Executable,
    config: &ForwardConfig,
    (width, height): (u32, u32),
    raw: &RawInput,
    skip: &[String],
) -> Result<String> {
    let family = codec_family(&config.codec)?;
    if config.encoder != "auto" {
        validate_encoder(&config.codec, &config.encoder)?;
        if jetson_encoder(&config.encoder) {
            ensure!(
                jetson_works(exe, &config.encoder)? == Some(true),
                "forwarding encoder {} is unavailable or cannot encode on this device",
                config.encoder
            );
        } else {
            validate_encoder_listing(&encoder_listing(exe)?, &config.encoder, family)?;
        }
        return Ok(config.encoder.clone());
    }
    let mut undecided = Vec::new();
    for &candidate in encoder_candidates(family) {
        if skip.iter().any(|failed| failed == candidate) {
            continue;
        }
        let raw = raw.for_encoder(candidate);
        match usable(exe, candidate, width, height, Some(&raw))? {
            Some(true) => return Ok(candidate.into()),
            Some(false) => {}
            None => undecided.push(candidate),
        }
    }
    // A test encode cut short says nothing about the encoder: it may well
    // work once the host has the time or memory.
    if !undecided.is_empty() {
        return Err(Categorized::new(
            ErrorKind::Unavailable,
            format!(
                "could not tell whether {family} recording at {width}x{height} works here: the test encode with {} did not finish (it ran out of time, or was stopped, as by a memory limit)",
                undecided.join(", ")
            ),
        )
        .hint("try again; give the recording more memory or CPU if it happens again")
        .into());
    }
    Err(Categorized::error(
        ErrorKind::Unsupported,
        unsupported_codec(exe, family, Some((width, height))),
    ))
}
/// Before a recording starts, refuse a codec no encoder here can produce.
pub(crate) fn check_codec(config: &ForwardConfig) -> Result<()> {
    // Without FFmpeg, or when probing itself fails, the first frame reports why.
    let Ok(exe) = executable() else {
        return Ok(());
    };
    let family = codec_family(&config.codec)?;
    if config.encoder != "auto"
        || encoder_candidates(family).iter().any(|candidate| {
            // One that cannot be tested yet may still work.
            usable(&exe, candidate, PROBE_SIZE.0, PROBE_SIZE.1, None)
                .map_or(true, |works| works != Some(false))
        })
    {
        return Ok(());
    }
    Err(Categorized::error(
        ErrorKind::Unsupported,
        unsupported_codec(&exe, family, None),
    ))
}
/// Says which codecs do work, since the bundled `FFmpeg` has no software HEVC
/// or AV1 encoder and many hosts have no hardware one either.
pub(super) fn unsupported_codec(
    exe: &Executable,
    family: &str,
    size: Option<(u32, u32)>,
) -> String {
    let working = ["h264", "hevc", "av1"]
        .into_iter()
        .filter(|&other| {
            other != family
                && encoder_candidates(other).iter().any(|candidate| {
                    matches!(
                        usable(exe, candidate, PROBE_SIZE.0, PROBE_SIZE.1, None),
                        Ok(Some(true))
                    )
                })
        })
        .collect::<Vec<_>>();
    let at = size.map_or_else(String::new, |(width, height)| {
        format!(" at {width}x{height}")
    });
    format!(
        "{family} recording{at} is not supported on this host: no {family} encoder in FFmpeg works here; working codecs: {}. Choose another codec",
        if working.is_empty() {
            "none".to_owned()
        } else {
            working.join(", ")
        }
    )
}

pub(super) fn validate_encoder_listing(listing: &str, encoder: &str, family: &str) -> Result<()> {
    let line = listing.lines().find(|line| line.split_whitespace().nth(1) == Some(encoder))
        .with_context(|| format!("forwarding encoder {encoder} is unavailable in this FFmpeg build; use auto or choose an installed encoder"))?;
    ensure!(
        line.split_whitespace()
            .next()
            .is_some_and(|flags| flags.starts_with('V')),
        "forwarding encoder {encoder} must be a video encoder"
    );
    let declared = line
        .split_once("(codec ")
        .and_then(|(_, suffix)| suffix.split_once(')').map(|(codec, _)| codec));
    let declared = declared.or_else(|| encoder_family(encoder));
    ensure!(
        declared == Some(family),
        "forwarding encoder {encoder} does not encode the selected {family} codec; choose a matching encoder or change --codec"
    );
    Ok(())
}
/// NVIDIA Jetson hardware encoders, reached through NVIDIA's `GStreamer`
/// elements: on Jetson the video engine is a V4L2 device that `FFmpeg`'s NVENC
/// and generic V4L2 wrappers cannot drive. `GStreamer` runs as a separate
/// process that encodes to MPEG-TS for `FFmpeg` to remux and deliver.
pub(super) fn jetson_encoder(encoder: &str) -> bool {
    matches!(encoder, "nvv4l2h264enc" | "nvv4l2h265enc" | "nvv4l2av1enc")
}
pub(super) fn jetson_parser(encoder: &str) -> &'static str {
    match encoder {
        "nvv4l2h264enc" => "h264parse",
        "nvv4l2h265enc" => "h265parse",
        _ => "av1parse",
    }
}

/// `GStreamer` fraction fields use signed 32-bit numerator and denominator.
/// Continued fractions recover rates such as 30000/1001 without rounding to
/// decimal milliseconds. Accept a rate only if the fraction reproduces the
/// requested floating-point value exactly.
#[deny(clippy::cast_precision_loss)]
pub(super) fn jetson_framerate(fps: f64) -> Result<(u32, u32)> {
    ensure!(
        fps.is_finite() && (0.1..=240.0).contains(&fps),
        "forwarding FPS must be a finite number within 0.1..240"
    );
    const LIMIT: u32 = i32::MAX as u32;
    let (mut previous_numerator, mut numerator) = (0u32, 1u32);
    let (mut previous_denominator, mut denominator) = (1u32, 0u32);
    let mut remainder = fps;
    for _ in 0..64 {
        let coefficient = remainder.floor();
        let numerator_limit = (LIMIT - previous_numerator)
            .checked_div(numerator)
            .unwrap_or(LIMIT);
        let denominator_limit = (LIMIT - previous_denominator)
            .checked_div(denominator)
            .unwrap_or(LIMIT);
        // Bound both next values by LIMIT, so their products and sums fit u32.
        let maximum = numerator_limit.min(denominator_limit);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the floored coefficient is nonnegative and bounded by the signed 32-bit fraction limit"
        )]
        let multiplier = if coefficient <= f64::from(maximum) {
            coefficient as u32
        } else {
            maximum
        };
        let next_numerator = multiplier * numerator + previous_numerator;
        let next_denominator = multiplier * denominator + previous_denominator;
        if next_denominator > 0 && f64::from(next_numerator) / f64::from(next_denominator) == fps {
            return Ok((next_numerator, next_denominator));
        }
        if f64::from(multiplier) != coefficient {
            break;
        }
        let fraction = remainder - coefficient;
        if fraction <= 0.0 {
            break;
        }
        remainder = 1.0 / fraction;
        (previous_numerator, numerator) = (numerator, next_numerator);
        (previous_denominator, denominator) = (denominator, next_denominator);
    }
    bail!(
        "Jetson forwarding FPS {fps} cannot be represented exactly with 32-bit fraction fields; choose a simpler frame rate or a different encoder"
    )
}
/// Run a helper tool briefly: whether it succeeded, or `None` when it had to
/// be stopped at the deadline.
pub(super) fn tool_succeeds(program: &str, args: &[&str], timeout: Duration) -> Option<bool> {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Some(false);
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status.success()),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}
/// The element exists and encodes a short test clip on this device; Orin,
/// for example, lists the AV1 element but has no AV1 encoder.
pub(super) fn probe_jetson(encoder: &str) -> Option<bool> {
    if !cfg!(target_os = "linux")
        || !tool_succeeds("gst-inspect-1.0", &[encoder], Duration::from_secs(5))?
    {
        return Some(false);
    }
    tool_succeeds(
        "gst-launch-1.0",
        &[
            "-q",
            "videotestsrc",
            "num-buffers=3",
            "!",
            "video/x-raw,width=640,height=480,format=I420",
            "!",
            "nvvidconv",
            "!",
            "video/x-raw(memory:NVMM),format=NV12",
            "!",
            encoder,
            "!",
            jetson_parser(encoder),
            "!",
            "fakesink",
        ],
        Duration::from_secs(10),
    )
}
/// Bits per second from a bitrate like `4M`, `2500k` or `800000`.
pub(super) fn bits_per_second(bitrate: &str) -> Result<u64> {
    ensure!(
        !bitrate.is_empty() && bitrate.len() <= 32,
        "forwarding bitrate needs a positive bit rate, for example 4M or 2500k"
    );
    let (number, power) = [('k', 3u32), ('K', 3), ('M', 6), ('G', 9)]
        .into_iter()
        .find_map(|(suffix, power)| bitrate.strip_suffix(suffix).map(|number| (number, power)))
        .unwrap_or((bitrate, 0));
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    ensure!(
        !number.is_empty()
            && number.bytes().any(|b| b.is_ascii_digit())
            && whole
                .bytes()
                .chain(fraction.bytes())
                .all(|b| b.is_ascii_digit()),
        "invalid forwarding bitrate; use a number with an optional k, M or G suffix (for example 4M)"
    );
    let fraction = fraction.trim_end_matches('0');
    let digits = u32::try_from(fraction.len())
        .context("forwarding bitrate has too many fractional digits")?;
    ensure!(
        digits <= power,
        "forwarding bitrate must resolve to a whole number of bits per second"
    );
    let whole = if whole.is_empty() {
        0
    } else {
        whole
            .parse::<u64>()
            .context("forwarding bitrate is too large")?
    };
    let fractional = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u64>()? * 10u64.pow(power - digits)
    };
    whole
        .checked_mul(10u64.pow(power))
        .and_then(|whole| whole.checked_add(fractional))
        .filter(|value| (1..=u64::from(u32::MAX)).contains(value))
        .context("forwarding bitrate must be within 1..4294967295 bits per second")
}
/// Pixel layout handed to the encoder after `FFmpeg`'s input conversion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum EncoderInput {
    /// Planar or semi-planar 4:2:0 converted by `FFmpeg`'s SIMD swscale.
    Yuv,
    /// Packed RGB; the encoder's own color-conversion hardware makes YUV.
    Rgb,
}
/// NVENC and AMF accept packed RGB and convert it to YUV on the GPU, which
/// keeps that matrix multiply off the CPU for RGB sources.
pub(super) fn hardware_rgb_input(encoder: &str) -> bool {
    encoder.ends_with("_nvenc") || encoder.ends_with("_amf")
}
/// Choose the encoder input for a source. Only RGB sources go to an encoder
/// as RGB: a Bayer mosaic or YUV would be converted on the CPU either way
/// (a mosaic demosaicked to RGB24 first, see `encoder_output_options`), and
/// as 4:2:0 it sends under half the bytes of `bgr0` over the bus. RGB input
/// is used only when the encoder accepts it on this machine, so a driver
/// without it keeps the YUV path instead of falling back to software
/// encoding.
pub(super) fn encoder_input(
    exe: &Executable,
    encoder: &str,
    raw: &RawInput,
    (width, height): (u32, u32),
) -> Result<EncoderInput> {
    if raw.rgb()
        && hardware_rgb_input(encoder)
        && encoder_works(exe, encoder, EncoderInput::Rgb, Some(raw), width, height)? == Some(true)
    {
        Ok(EncoderInput::Rgb)
    } else {
        Ok(EncoderInput::Yuv)
    }
}
/// Encode one frame with `candidate`: a black frame in `source`'s format,
/// read as the recording will read its frames, or a generated one.
pub(super) fn probe_encoder(
    exe: &Executable,
    candidate: &str,
    input: EncoderInput,
    source: Option<&RawInput>,
    width: u32,
    height: u32,
) -> Result<Option<bool>> {
    let mut args = ["-hide_banner", "-loglevel", "error"]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut frame = Vec::new();
    let pix_fmt = if let Some(raw) = source {
        let pix_fmt = raw.input_pix_fmt(candidate);
        args.extend(
            [
                "-f",
                "rawvideo",
                "-pix_fmt",
                pix_fmt,
                "-video_size",
                &format!("{width}x{height}"),
            ]
            .map(str::to_owned),
        );
        args.extend(raw.color_options().iter().map(ToString::to_string));
        args.extend(["-i".into(), "pipe:0".into()]);
        // Neutral YUV chroma, so a black frame really is one.
        frame = vec![if raw.pairs() { 128 } else { 0 }; raw.pipe_len(width, height)?];
        pix_fmt
    } else {
        args.extend(
            [
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                &format!("color=size={width}x{height}:rate=1"),
            ]
            .map(str::to_owned),
        );
        "yuv420p"
    };
    args.extend(["-frames:v", "1", "-c:v", candidate].map(str::to_owned));
    args.extend(encoder_device_options(candidate));
    args.extend(encoder_output_options(candidate, input, pix_fmt));
    // As the recording runs it: x264's defaults would size its threads by
    // the host's cores and add a lookahead, needing more memory than the
    // recording it vouches for.
    args.extend(software_encoder_options(
        candidate, width, height, PROBE_FPS,
    ));
    args.extend(["-f".into(), "null".into(), "-".into()]);
    // Large sensors take longer to encode even once.
    let pixels = u64::from(width) * u64::from(height);
    let timeout = Duration::from_secs(3) + Duration::from_millis(pixels / 10_000);
    let (works, _, timed_out) = timed_output(exe, &args, &frame, timeout)?;
    Ok((!timed_out).then_some(works))
}
/// The frame rate a test encode assumes for the options that depend on it.
pub(super) const PROBE_FPS: f64 = 30.0;
/// x264's and x265's speed, threads and latency for a recording of
/// `width`x`height` at `fps`; nothing for other encoders.
pub(super) fn software_encoder_options(
    encoder: &str,
    width: u32,
    height: u32,
    fps: f64,
) -> Vec<String> {
    if !matches!(encoder, "libx264" | "libx265") {
        return Vec::new();
    }
    let cores = cores();
    let preset = x264_preset(width, height, fps, cores);
    let mut options = vec![
        "-preset".into(),
        preset.into(),
        "-threads".into(),
        cores.to_string(),
    ];
    if encoder == "libx264" {
        // Rate control by the frames' times rather than `fps`, so a camera
        // slower than `fps` still gets the bitrate asked.
        options.extend(["-tune", "zerolatency", "-x264-params", "force-cfr=0"].map(str::to_owned));
    }
    options
}
pub(super) fn encoder_device_options(encoder: &str) -> Vec<String> {
    if encoder.ends_with("_vaapi") {
        vec![
            "-vaapi_device".into(),
            std::env::var("CAPTUREFAB_VAAPI_DEVICE")
                .unwrap_or_else(|_| "/dev/dri/renderD128".into()),
        ]
    } else if encoder.ends_with("_vulkan") {
        // The first Vulkan device with video encode; FFmpeg loads the system
        // Vulkan loader at run time.
        ["-init_hw_device", "vulkan=vk", "-filter_hw_device", "vk"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    }
}
/// Preserve each raw recording frame's wall-clock time at microsecond
/// precision, including frames the pipe delivered within one nominal tick.
pub(super) fn recording_clock_options(mut options: Vec<String>) -> Vec<String> {
    const CLOCK: &str = "settb=AVTB,setpts=(RTCTIME-RTCSTART)/(TB*1000000)";
    let mut filtered = false;
    for [flag, value] in options.as_chunks_mut::<2>().0 {
        if flag == "-vf" {
            value.insert(0, ',');
            value.insert_str(0, CLOCK);
            filtered = true;
            break;
        }
    }
    if !filtered {
        options.extend(["-vf".to_owned(), CLOCK.to_owned()]);
    }
    options.extend(["-enc_time_base", "1:1000000"].map(str::to_owned));
    options
}

/// Output options for `encoder` reading `input` from frames in `FFmpeg`'s
/// pixel format `source`.
pub(super) fn encoder_output_options(
    encoder: &str,
    input: EncoderInput,
    source: &str,
) -> Vec<String> {
    if encoder == "ffv1" {
        // Every frame intra-coded at the camera's own pixel layout, so the
        // file is lossless frame by frame; no padding or 4:2:0.
        return ["-level", "3", "-g", "1"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
    }
    // A mosaic is demosaicked to RGB24 first: the bundled FFmpeg's swscale
    // makes 4:2:0 from RGB24 2.6 times faster than its direct Bayer to
    // yuv420p path (about 21 against 57 ms a 12 MP frame on one Apple core,
    // the two steps together), and as fast as straight to NV12.
    let bayer = if source.starts_with("bayer_") && input == EncoderInput::Yuv {
        "format=rgb24,"
    } else {
        ""
    };
    let mut options = if encoder.ends_with("_vaapi") || encoder.ends_with("_vulkan") {
        // Convert and pad on the CPU, then upload into the device's frames.
        vec![
            "-vf".into(),
            format!("{bayer}pad=ceil(iw/2)*2:ceil(ih/2)*2,format=nv12,hwupload"),
            "-pix_fmt".into(),
            if encoder.ends_with("_vaapi") {
                "vaapi"
            } else {
                "vulkan"
            }
            .into(),
        ]
    } else {
        let nv12 = encoder.ends_with("_qsv")
            || encoder.ends_with("_mf")
            || encoder.ends_with("_videotoolbox");
        vec![
            "-vf".into(),
            format!("{bayer}pad=ceil(iw/2)*2:ceil(ih/2)*2"),
            "-pix_fmt".into(),
            if input == EncoderInput::Rgb {
                "bgr0"
            } else if nv12 {
                // The native surface layout of these encoders: no repacking copy.
                "nv12"
            } else {
                "yuv420p"
            }
            .into(),
        ]
    };
    if encoder.ends_with("_videotoolbox") {
        options.extend(["-allow_sw", "0"].map(str::to_owned));
    }
    if encoder.ends_with("_mf") {
        options.extend(["-hw_encoding", "1"].map(str::to_owned));
    }
    options
}
