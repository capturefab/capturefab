//! Hardware decoding of compressed sources: H.264, HEVC, AV1 or MJPEG from
//! network streams, files and webcams in a compressed mode
//! (`CAPTUREFAB_HWDEC=auto|none|METHOD`).
//!
//! A method is used only once a probe has proved it decodes on this host: a
//! 64x64 H.264 frame decoded into device memory and downloaded
//! (`hwdownload` accepts only hardware frames, so success is proof), cached
//! for the boot. FFmpeg then downloads every frame once, into the native
//! format the decoder reads (NV12), with no conversion. A source the method
//! cannot decode (a profile or size it lacks) is decoded in software by
//! FFmpeg itself, which shows as 8-bit frames that are not NV12; that codec
//! and size class is then left to software for the rest of the boot, and a
//! launch whose FFmpeg exits before its first frame is tried again without
//! it.
//!
//! Setting up a decoder session costs about 70-100 ms on Apple silicon,
//! added to the first frame's wait (it needs the stream's first packets),
//! while every later frame costs a seventh to a third of the CPU time. So
//! `auto` decodes live sources (network streams, webcams in a compressed
//! mode) in hardware only in a session's camera workers (the warm `auto`
//! session, a named one or `serve`), whose decoder runs on between commands
//! and repays the setup within a second of video; a command's own worker
//! (`--no-session`) and every file decode in software, which delivers one
//! frame sooner.
use super::{Executable, Found, probed, remember, timed_output, with_probes};
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use std::time::Duration;

pub(crate) const ENV: &str = "CAPTUREFAB_HWDEC";

/// Every method Capturefab knows how to ask FFmpeg for, with the hardware
/// pixel format its frames have in device memory, which the probe keeps
/// them in. `v4l2m2m` is a decoder per codec rather than a `-hwaccel`.
const METHODS: [(&str, &str); 8] = [
    ("videotoolbox", "videotoolbox_vld"),
    ("cuda", "cuda"),
    ("vaapi", "vaapi"),
    ("qsv", "qsv"),
    ("vulkan", "vulkan"),
    ("d3d11va", "d3d11"),
    ("dxva2", "dxva2_vld"),
    ("v4l2m2m", ""),
];

/// The methods `auto` tries on this platform, in order: those the bundled
/// FFmpeg builds for it (`scripts/build-ffmpeg.sh`). Others, such as VA-API
/// in an FFmpeg of one's own, can be named.
pub(crate) fn candidates() -> &'static [&'static str] {
    match std::env::consts::OS {
        "macos" => &["videotoolbox"],
        "windows" => &["d3d11va", "cuda", "qsv", "dxva2", "vulkan"],
        "linux" => &["cuda", "vulkan", "v4l2m2m"],
        _ => &[],
    }
}

/// What `CAPTUREFAB_HWDEC` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Setting {
    Auto,
    None,
    Named(&'static str),
}

impl Setting {
    pub(crate) fn parse(value: Option<&str>) -> Result<Self> {
        Ok(match value.map(str::trim).unwrap_or("") {
            "" | "auto" => Setting::Auto,
            "none" => Setting::None,
            name => match METHODS.iter().find(|(method, _)| *method == name) {
                Some((method, _)) => Setting::Named(method),
                None => {
                    let known: Vec<&str> = METHODS.iter().map(|(method, _)| *method).collect();
                    return Err(Categorized::error(
                        ErrorKind::Usage,
                        format!(
                            "{ENV}={name:?} is not a decoding method; use auto, none or one of {}",
                            known.join(", ")
                        ),
                    ));
                }
            },
        })
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            Setting::Auto => "auto",
            Setting::None => "none",
            Setting::Named(method) => method,
        }
    }
}

#[cfg(test)]
thread_local! {
    /// The setting for this test thread, instead of the environment's.
    pub(super) static SETTING: std::cell::Cell<Option<Setting>> =
        const { std::cell::Cell::new(None) };
}

pub(crate) fn setting() -> Result<Setting> {
    #[cfg(test)]
    if let Some(setting) = SETTING.get() {
        return Ok(setting);
    }
    Setting::parse(std::env::var(ENV).ok().as_deref())
}

/// What a source is, for choosing how to decode it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Source {
    /// A network stream or a webcam in a compressed mode.
    Live,
    /// A file, read as fast as a capture asks or at its own rate.
    File,
}

/// A size class, for remembering what a method cannot decode: decoders
/// have size limits, and a frame size between classes rarely matters.
pub(super) fn size_class(width: u32, height: u32) -> &'static str {
    match u64::from(width) * u64::from(height) {
        0..=921_600 => "hd",
        921_601..=2_073_600 => "fhd",
        2_073_601..=8_912_896 => "uhd",
        _ => "large",
    }
}

/// The key a method's answer for a codec and size class is kept under.
fn key(method: &str, codec: &str, class: &str) -> String {
    format!("{method} {codec} {class}")
}

/// `ffmpeg -hwaccels`, kept for the boot.
fn listing(exe: &Executable) -> Result<String> {
    if let Some(listing) = with_probes(exe, |probes| probes.hwaccels.clone()) {
        return Ok(listing);
    }
    let args = ["-hide_banner", "-hwaccels"].map(str::to_string);
    let (success, listing, _) = timed_output(exe, &args, &[], super::PROBE_TIMEOUT)?;
    anyhow::ensure!(success, "FFmpeg could not list its hardware decoders");
    remember(exe, |probes| probes.hwaccels = Some(listing.clone()));
    Ok(listing)
}

/// Whether this FFmpeg has `method`: in its `-hwaccels` list, or for
/// `v4l2m2m`, its V4L2 memory-to-memory H.264 decoder.
fn built(exe: &Executable, method: &str) -> Result<bool> {
    if method == "v4l2m2m" {
        return Ok(cfg!(target_os = "linux") && super::listed(&decoders(exe)?, "h264_v4l2m2m"));
    }
    Ok(listing(exe)?.lines().any(|line| line.trim() == method))
}

/// `ffmpeg -decoders`, for the per-codec V4L2 decoders, kept for the boot.
fn decoders(exe: &Executable) -> Result<String> {
    if let Some(listing) = with_probes(exe, |probes| probes.decoder_listing.clone()) {
        return Ok(listing);
    }
    let args = ["-hide_banner", "-decoders"].map(str::to_string);
    let (success, listing, _) = timed_output(exe, &args, &[], super::PROBE_TIMEOUT)?;
    anyhow::ensure!(success, "FFmpeg could not list its decoders");
    remember(exe, |probes| probes.decoder_listing = Some(listing.clone()));
    Ok(listing)
}

/// One black 64x64 baseline H.264 frame.
const CLIP: [u8; 65] = [
    0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xc0, 0x0a, 0xd9, 0x04, 0x26, 0xc0, 0x44, 0x00, 0x00, 0x03,
    0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0x08, 0x3c, 0x48, 0x99, 0x20, 0x00, 0x00, 0x00, 0x01, 0x68,
    0xcb, 0x83, 0xcb, 0x20, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x05, 0x7f, 0xff, 0xff, 0x0f, 0x45,
    0x00, 0x01, 0x42, 0xdf, 0x27, 0x27, 0x27, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7,
    0x80,
];

/// The arguments that decode the probe clip with `method`, failing unless
/// the hardware decodes it.
fn probe_args(method: &str) -> Vec<String> {
    let mut args = ["-nostdin", "-hide_banner", "-loglevel", "error"]
        .map(str::to_string)
        .to_vec();
    match METHODS.iter().find(|(name, _)| *name == method) {
        Some(("v4l2m2m", _)) => {
            args.extend(["-c:v", "h264_v4l2m2m", "-f", "h264", "-i", "pipe:0"].map(str::to_string));
            args.extend(["-f", "null", "-"].map(str::to_string));
        }
        Some((_, device_format)) => {
            args.extend(
                [
                    "-hwaccel",
                    method,
                    "-hwaccel_output_format",
                    device_format,
                    "-f",
                    "h264",
                    "-i",
                    "pipe:0",
                    // Software frames cannot be downloaded: only frames the
                    // hardware decoded get through.
                    "-vf",
                    "hwdownload,format=nv12",
                    "-f",
                    "null",
                    "-",
                ]
                .map(str::to_string),
            );
        }
        None => {}
    }
    args
}

/// Whether `method` decodes on this host, proved once a boot; a failure is
/// believed for a few minutes.
pub(crate) fn works(exe: &Executable, method: &'static str) -> Result<bool> {
    if !built(exe, method)? {
        return Ok(false);
    }
    let key = key(method, "probe", "h264");
    let works = probed(
        exe,
        |probes| probes.decoders.get(&key).copied(),
        |probes, found| {
            probes.decoders.insert(key.clone(), found);
        },
        || {
            let (works, _, timed_out) =
                timed_output(exe, &probe_args(method), &CLIP, Duration::from_secs(5))?;
            Ok((!timed_out).then_some(works))
        },
    )?;
    Ok(works.unwrap_or(false))
}

/// The key a source's codec is remembered under: its name, and for samples
/// wider than 8 bits (10-bit HEVC) a key of its own, since a method may
/// decode one depth and not the other.
pub(super) fn codec_key(input: &super::native::Input) -> String {
    if input.deep {
        format!("{}-deep", input.codec)
    } else {
        input.codec.clone()
    }
}

/// What the method decoded `codec` at this size class as, if it has been
/// asked this boot.
pub(super) fn remembered(exe: &Executable, method: &str, codec: &str, class: &str) -> Option<bool> {
    let key = key(method, codec, class);
    with_probes(exe, |probes| probes.decoders.get(&key).copied()).and_then(Found::current)
}

/// Keep what `method` did with `codec` at this size class.
pub(super) fn learned(exe: &Executable, method: &str, codec: &str, class: &str, works: bool) {
    let key = key(method, codec, class);
    remember(exe, |probes| {
        probes.decoders.insert(key, Found::now(works));
    });
}

/// Whether this process's decoders run on between commands: it is a
/// session's camera worker.
fn lasting() -> bool {
    #[cfg(test)]
    if let Some(lasting) = LASTING.get() {
        return lasting;
    }
    crate::session::served_session().is_some()
}

#[cfg(test)]
thread_local! {
    /// Whether this test thread's decoders are a session's.
    pub(super) static LASTING: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// The method to decode a `source` with, if any: the one named, or under
/// `auto` the first candidate that works here, for live sources in a
/// session's worker. `known` is the codec key and size class of the
/// source's last launch, when one told; a method that decoded those in
/// software is not used for them again. A named method this FFmpeg lacks,
/// or that fails its probe, is the unavailable error.
pub(super) fn choose(
    exe: &Executable,
    source: Source,
    known: Option<(&str, &str)>,
) -> Result<Option<&'static str>> {
    let failed = |method: &str| {
        known.is_some_and(|(codec, class)| remembered(exe, method, codec, class) == Some(false))
    };
    match setting()? {
        Setting::None => Ok(None),
        Setting::Named(method) => {
            if !works(exe, method)? {
                let error = Categorized::new(
                    ErrorKind::Unavailable,
                    format!(
                        "{ENV}={method}: this FFmpeg cannot decode with {method} on this computer; set {ENV}=auto or none"
                    ),
                );
                return Err(match crate::session::environment_hint(ENV) {
                    Some(hint) => error.hint(hint),
                    None => error,
                }
                .into());
            }
            Ok((!failed(method)).then_some(method))
        }
        Setting::Auto if source == Source::File || !lasting() => Ok(None),
        Setting::Auto => {
            for &method in candidates() {
                if !failed(method) && works(exe, method).unwrap_or(false) {
                    return Ok(Some(method));
                }
            }
            Ok(None)
        }
    }
}

/// The input options that decode with `method`: `-hwaccel` (FFmpeg
/// downloads each frame into its software format), or for `v4l2m2m` the
/// codec's V4L2 decoder, when the codec is known. Once the method has
/// decoded the source's codec in hardware (`proven`), on one decoder
/// thread, since frame threads only add latency to a hardware decoder; until
/// then on FFmpeg's usual threads, which a codec it leaves to software
/// keeps for the life of the decoder.
pub(super) fn options(method: &str, codec: Option<&str>, proven: bool) -> Option<Vec<String>> {
    let mut options = match method {
        "v4l2m2m" => {
            let codec = codec
                .filter(|codec| matches!(*codec, "h264" | "hevc" | "mjpeg" | "vp8" | "vp9"))?;
            vec!["-c:v".into(), format!("{codec}_v4l2m2m")]
        }
        method => vec!["-hwaccel".into(), method.into()],
    };
    if proven {
        options.extend(["-threads".into(), "1".into()]);
    }
    Some(options)
}

/// Whether the 8-bit frames of an 8-bit source, read in `pix_fmt`, came
/// from the hardware: methods download them as NV12, which no software
/// decoder of these codecs makes. A deeper source's frames are converted
/// to 8 bits whichever decoded them, so they do not tell.
pub(super) fn decoded_in_hardware(pix_fmt: &str) -> bool {
    pix_fmt == "nv12"
}

/// `doctor`'s `hwdec` object: what `CAPTUREFAB_HWDEC` selects and each
/// method of this platform, whether this FFmpeg has it and whether it
/// decodes here (probed, so a first `doctor` runs FFmpeg once for each).
pub(crate) fn report() -> serde_json::Value {
    let selected = match setting() {
        Ok(setting) => serde_json::json!(setting.name()),
        Err(error) => serde_json::json!(format!("{error:#}")),
    };
    let mut methods = serde_json::Map::new();
    let mut names: Vec<&'static str> = candidates().to_vec();
    if let Ok(Setting::Named(method)) = setting()
        && !names.contains(&method)
    {
        names.push(method);
    }
    match super::executable() {
        Ok(exe) => {
            for method in names {
                let entry = match (built(&exe, method), works(&exe, method)) {
                    (Ok(false), _) => serde_json::json!({"built": false, "works": false}),
                    (Ok(true), Ok(works)) => serde_json::json!({"built": true, "works": works}),
                    (Err(error), _) | (_, Err(error)) => {
                        serde_json::json!({"built": false, "works": false, "reason": format!("{error:#}")})
                    }
                };
                methods.insert(method.into(), entry);
            }
        }
        Err(_) => {
            for method in names {
                methods.insert(
                    method.into(),
                    serde_json::json!({"built": false, "works": false, "reason": "FFmpeg is not available"}),
                );
            }
        }
    }
    serde_json::json!({"selected": selected, "methods": methods})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_parsed_and_unknown_methods_are_usage_errors() {
        assert_eq!(Setting::parse(None).unwrap(), Setting::Auto);
        assert_eq!(Setting::parse(Some("none")).unwrap(), Setting::None);
        assert_eq!(
            Setting::parse(Some(" videotoolbox ")).unwrap(),
            Setting::Named("videotoolbox")
        );
        let error = Setting::parse(Some("nvdec")).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Usage);
        assert!(format!("{error:#}").contains("cuda"));
    }

    #[test]
    fn probes_prove_the_hardware_decoded() {
        let args = probe_args("videotoolbox").join(" ");
        assert!(args.contains("-hwaccel videotoolbox -hwaccel_output_format videotoolbox_vld"));
        assert!(args.contains("-vf hwdownload,format=nv12"));
        assert!(
            probe_args("d3d11va")
                .join(" ")
                .contains("-hwaccel_output_format d3d11")
        );
        let v4l2 = probe_args("v4l2m2m").join(" ");
        assert!(v4l2.contains("-c:v h264_v4l2m2m") && !v4l2.contains("hwaccel"));
    }

    #[test]
    fn options_decode_on_one_thread_once_proven_and_v4l2_needs_the_codec() {
        assert_eq!(
            options("cuda", None, true).unwrap(),
            ["-hwaccel", "cuda", "-threads", "1"]
        );
        assert_eq!(
            options("cuda", Some("h264"), false).unwrap(),
            ["-hwaccel", "cuda"]
        );
        assert_eq!(options("v4l2m2m", None, true), None);
        assert_eq!(options("v4l2m2m", Some("av1"), true), None);
        assert_eq!(
            options("v4l2m2m", Some("hevc"), true).unwrap(),
            ["-c:v", "hevc_v4l2m2m", "-threads", "1"]
        );
        let input = |deep| super::super::native::Input {
            codec: "hevc".into(),
            width: 1920,
            height: 1080,
            deep,
        };
        assert_eq!(codec_key(&input(false)), "hevc");
        assert_eq!(codec_key(&input(true)), "hevc-deep");
        assert!(decoded_in_hardware("nv12") && !decoded_in_hardware("yuv420p"));
        assert_eq!(size_class(1920, 1080), "fhd");
        assert_eq!(size_class(640, 480), "hd");
        assert_eq!(size_class(3840, 2160), "uhd");
    }
}
