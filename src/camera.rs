use crate::{
    genicam::{Bounds, FeatureInfo, NodeMap},
    types::{Backend, CameraInfo, Frame, Transport, TransportStats},
};
use anyhow::{Context, Result, ensure};
use std::{
    net::Ipv4Addr,
    time::{Duration, Instant},
};

/// The transports a discovery searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kinds {
    pub gige: bool,
    pub u3v: bool,
    /// Webcams and screen captures on this computer.
    pub host: bool,
    pub onvif: bool,
    pub sim: bool,
}
impl Kinds {
    pub fn all() -> Self {
        Self {
            gige: true,
            u3v: true,
            host: true,
            onvif: true,
            sim: true,
        }
    }
}

/// How long past its timeout a transport's search may run before it is
/// left behind.
const DISCOVERY_GRACE: Duration = Duration::from_secs(2);

/// Search the chosen transports in parallel; it returns when each has
/// answered, which for the GigE and ONVIF broadcasts is when `timeout` ends.
/// A transport that fails becomes a warning, and one still searching past
/// its deadline is left behind on its own thread, so a hung transport never
/// holds up the caller: what the others found is returned, with a warning.
/// What it found is kept a minute for `lookup`. Without a network, GigE and
/// ONVIF are not waited for.
pub fn discover_kinds(timeout: Duration, kinds: Kinds) -> (Vec<CameraInfo>, Vec<String>) {
    search(timeout, kinds, None)
}

/// Which cameras a search is for: it ends as soon as one answers.
type Want = std::sync::Arc<dyn Fn(&CameraInfo) -> bool + Send + Sync>;

/// Whether this computer has a network a GigE or ONVIF camera can answer
/// on: an IPv4 address other than loopback.
fn networked() -> bool {
    if_addrs::get_if_addrs().is_ok_and(|interfaces| {
        interfaces.iter().any(|interface| {
            matches!(&interface.addr, if_addrs::IfAddr::V4(v4)
                if !v4.ip.is_loopback() && !v4.ip.is_unspecified())
        })
    })
}

/// `discover_kinds`, ending as soon as a camera `want` names answers.
fn search(
    timeout: Duration,
    mut kinds: Kinds,
    want: Option<Want>,
) -> (Vec<CameraInfo>, Vec<String>) {
    enum Found {
        One(CameraInfo),
        Cameras(Vec<CameraInfo>),
        Native(serde_json::Value),
    }
    type Message = (&'static str, Result<Found>);
    type Search = Box<dyn FnOnce(&std::sync::mpsc::Sender<Message>) -> Result<Found> + Send>;
    // Whether it looks everywhere a camera can be.
    let everywhere = kinds.gige && kinds.u3v && kinds.host && kinds.onvif;
    // Without a network no broadcast can be answered: nothing to wait for.
    if (kinds.gige || kinds.onvif) && !networked() {
        kinds.gige = false;
        kinds.onvif = false;
    }
    let (sender, found) = std::sync::mpsc::channel::<Message>();
    // Each camera a broadcast finds is told as it answers, and ends that
    // search when it is the one wanted.
    let stream =
        |label: &'static str, sender: &std::sync::mpsc::Sender<Message>, want: &Option<Want>| {
            let sender = sender.clone();
            let want = want.clone();
            move |info: &CameraInfo| {
                let _ = sender.send((label, Ok(Found::One(info.clone()))));
                want.as_ref().is_some_and(|want| want(info))
            }
        };
    let mut asked = Vec::new();
    let mut searches: Vec<(&'static str, Search)> = Vec::new();
    if kinds.gige {
        let want = want.clone();
        searches.push((
            "GigE",
            Box::new(move |sender| {
                crate::transport::gige::discover_until(timeout, &mut stream("GigE", sender, &want))
                    .map(Found::Cameras)
            }),
        ));
    }
    if kinds.onvif {
        let want = want.clone();
        searches.push((
            "ONVIF",
            Box::new(move |sender| {
                crate::onvif::camera_infos_until(timeout, &mut stream("ONVIF", sender, &want))
                    .map(Found::Cameras)
            }),
        ));
    }
    if kinds.host {
        searches.push((
            "Native camera",
            Box::new(move |_| {
                crate::media::native_devices_with_timeout(timeout).map(Found::Native)
            }),
        ));
    }
    #[cfg(feature = "usb")]
    if kinds.u3v {
        searches.push((
            "USB",
            Box::new(|_| crate::transport::usb::discover().map(Found::Cameras)),
        ));
    }
    for (label, search) in searches {
        let sender = sender.clone();
        let spawned = std::thread::Builder::new()
            .name("capturefab-discovery".into())
            .spawn(move || {
                let result = search(&sender);
                let _ = sender.send((label, result));
            });
        if spawned.is_ok() {
            asked.push(label);
        }
    }
    drop(sender);
    let deadline = std::time::Instant::now() + timeout + DISCOVERY_GRACE;
    let mut heard = Heard::default();
    let mut warnings = Vec::new();
    let mut answered = Vec::new();
    let wanted = |heard: &Heard| want.as_ref().is_some_and(|want| heard.any(want.as_ref()));
    while answered.len() < asked.len() && !wanted(&heard) {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let Ok((label, result)) = found.recv_timeout(left) else {
            break;
        };
        match result {
            Ok(Found::One(info)) => {
                heard.told(label, info);
                continue;
            }
            Ok(Found::Cameras(found)) => heard.listed(label, found),
            Ok(Found::Native(native)) => heard.listed(label, native_cameras(&native)),
            Err(error) => warnings.push(format!("{label} discovery: {error:#}")),
        }
        answered.push(label);
    }
    let complete = !wanted(&heard);
    let mut devices = heard.cameras();
    if complete {
        for label in asked.iter().filter(|label| !answered.contains(label)) {
            warnings.push(format!(
                "{label} discovery did not answer within {}; listed without it",
                crate::scheduling::format_duration(
                    u64::try_from((timeout + DISCOVERY_GRACE).as_millis()).unwrap_or(u64::MAX)
                )
            ));
        }
    }
    if kinds.sim {
        devices.push(crate::transport::simulator::info());
    }
    devices.sort_by(|a, b| a.id.cmp(&b.id));
    devices.dedup_by(|a, b| a.id == b.id);
    for info in &mut devices {
        info.profile = crate::camera_profile::detect(info, &[]);
    }
    // A search to the end says which cameras of the transports that
    // answered are gone; one that ended early only adds what it found.
    let searched = |info: &CameraInfo| {
        let label = match info.transport {
            Transport::GigE => "GigE",
            Transport::Usb3 => "USB",
            Transport::Simulator => return false,
            Transport::Media if crate::onvif::is_source(&info.id) => "ONVIF",
            Transport::Media => "Native camera",
        };
        answered.contains(&label)
    };
    crate::discovery::store(
        &devices,
        complete.then_some(&searched as &dyn Fn(&CameraInfo) -> bool),
        everywhere && complete && answered.len() == asked.len(),
    );
    (devices, warnings)
}

/// The cameras a search heard of so far: each answer a broadcast told of
/// as it came, until the broadcast's own list replaces them, in which a
/// device that answered from two addresses is one camera.
#[derive(Default)]
struct Heard {
    listed: Vec<CameraInfo>,
    told: Vec<(&'static str, CameraInfo)>,
}
impl Heard {
    fn told(&mut self, from: &'static str, info: CameraInfo) {
        self.told.push((from, info));
    }
    fn listed(&mut self, from: &'static str, cameras: Vec<CameraInfo>) {
        self.told.retain(|(told, _)| *told != from);
        self.listed.extend(cameras);
    }
    fn any(&self, want: &(dyn Fn(&CameraInfo) -> bool + Send + Sync)) -> bool {
        self.listed
            .iter()
            .chain(self.told.iter().map(|(_, info)| info))
            .any(want)
    }
    fn cameras(self) -> Vec<CameraInfo> {
        let mut cameras = self.listed;
        cameras.extend(self.told.into_iter().map(|(_, info)| info));
        cameras
    }
}

/// The host cameras FFmpeg listed.
fn native_cameras(native: &serde_json::Value) -> Vec<CameraInfo> {
    native["devices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| {
            let id = d["id"].as_str()?;
            Some(CameraInfo {
                id: id.into(),
                transport: Transport::Media,
                vendor: std::env::consts::OS.into(),
                model: d["name"].as_str().unwrap_or("Host camera").into(),
                serial: id.into(),
                address: Some(id.into()),
                profile: Default::default(),
            })
        })
        .collect()
}

/// The cameras to resolve `selector` among, a serial, name, model or
/// discovered ID: those discovery found within the last minute when one of
/// them is still the camera it names, else those a search finds, which ends
/// as soon as the camera that has this ID, serial or address answers. A
/// name or model names one camera only among every camera there is, so it
/// is looked up there only after a search of every transport to its end.
pub fn lookup(selector: &str, timeout: Duration) -> (Vec<CameraInfo>, Vec<String>) {
    let (cached, everywhere) = crate::discovery::fresh();
    if let Some(info) = cached_match(selector, &cached, everywhere)
        && still_there(&info)
    {
        return (cached, Vec::new());
    }
    let wanted = selector.to_owned();
    let want: Want = std::sync::Arc::new(move |info: &CameraInfo| names_exactly(&wanted, info));
    search(timeout, kinds_for(selector), Some(want))
}

/// The camera `selector` names among those a discovery found lately: by
/// its ID, serial or address, or by a name or model only when they are
/// `everywhere` a search found, since a search that ended early may have
/// missed another camera of that model.
fn cached_match(selector: &str, cached: &[CameraInfo], everywhere: bool) -> Option<CameraInfo> {
    match resolve_selector(selector, cached) {
        Resolution::Exact(info) => Some(info),
        Resolution::Loose { info, .. } if everywhere => Some(info),
        _ => None,
    }
}

/// The transports a camera named `selector` may be on.
fn kinds_for(selector: &str) -> Kinds {
    let only = |gige, u3v| Kinds {
        gige,
        u3v,
        host: false,
        onvif: false,
        sim: false,
    };
    if prefixed(selector, "gige:") || mac_id(selector).is_some() {
        only(true, false)
    } else if prefixed(selector, "usb:") || prefixed(selector, "u3v:") {
        only(false, true)
    } else {
        Kinds {
            sim: false,
            ..Kinds::all()
        }
    }
}

/// Whether the camera a recent discovery found as `info` is still that
/// camera, where that is quick to tell: a GigE camera answers at its
/// address with the same MAC and serial; a Linux video node still has its
/// name; a webcam named by its name is that webcam. USB enumeration is
/// quick, and other cameras are looked for again.
pub(crate) fn still_there(info: &CameraInfo) -> bool {
    match info.transport {
        Transport::GigE => info
            .address
            .as_deref()
            .and_then(|address| address.parse::<Ipv4Addr>().ok())
            .and_then(|ip| crate::transport::gige::probe(ip, STILL_THERE).ok())
            .is_some_and(|probed| probed.id == info.id && probed.serial == info.serial),
        Transport::Media if crate::onvif::is_source(&info.id) => false,
        Transport::Media => crate::media::host_camera_unchanged(info),
        Transport::Usb3 | Transport::Simulator => false,
    }
}
/// How long a GigE camera a recent discovery found has to answer at its
/// address before it is looked for again.
const STILL_THERE: Duration = Duration::from_millis(250);

/// What a camera is, in the words `list` and `info` use: `gige`, `u3v`,
/// `webcam`, `screen` (a screen or Desk View capture), `onvif`, `stream`,
/// `file` or `sim`.
pub fn kind(info: &CameraInfo) -> &'static str {
    let source = media_source(info);
    match info.transport {
        Transport::GigE => "gige",
        Transport::Usb3 => "u3v",
        Transport::Simulator => "sim",
        Transport::Media if crate::onvif::is_source(source) => "onvif",
        Transport::Media if crate::media::is_host_camera(source) => {
            let name = info.model.to_ascii_lowercase();
            if name.starts_with("capture screen") || name.contains("desk view") {
                "screen"
            } else {
                "webcam"
            }
        }
        Transport::Media if source.contains("://") => "stream",
        Transport::Media => "file",
    }
}

/// What an FFmpeg source reads: a listed device's ID, or for an opened
/// source (ID `media:HASH`) the redacted selector it was opened with.
pub fn media_source(info: &CameraInfo) -> &str {
    match &info.address {
        Some(address) if info.id.starts_with("media:") => address,
        _ => &info.id,
    }
}

/// Commands that change the acquisition or transfer state, which only start
/// and stop may run, together with the transport: an AcquisitionAbort from
/// outside would leave the receiver running for a camera that has stopped.
const STREAM_COMMANDS: [&str; 7] = [
    "AcquisitionStart",
    "AcquisitionStop",
    "AcquisitionAbort",
    "AcquisitionArm",
    "TransferStart",
    "TransferStop",
    "TransferAbort",
];

const TL_PARAMS_LOCKED: &str = "TLParamsLocked";
const TL_PARAMS_LOCKED_MESSAGE: &str = "TLParamsLocked follows the stream; start and stop set it";

/// Triggers that start an acquisition, a burst or a frame, in the order a
/// software trigger fires them for one frame.
const START_TRIGGERS: [&str; 3] = ["AcquisitionStart", "FrameBurstStart", "FrameStart"];

pub struct Camera {
    pub info: CameraInfo,
    backend: Box<dyn Backend>,
    nodes: NodeMap,
    streaming: bool,
    xml: String,
    profile_metadata: Vec<FeatureInfo>,
    network_budget_bytes_per_second: Option<u64>,
    /// Whether start set TLParamsLocked, which stop clears again.
    tl_locked: bool,
    /// Whether start executed TransferStart, which stop follows with
    /// TransferStop.
    transferring: bool,
    /// Where TriggerSoftware starts a frame, read again after any write:
    /// each TriggerSelector value to fire under, or `None` for a camera
    /// without a selector.
    software_triggers: Option<Vec<Option<String>>>,
    /// What says which frames may have been exposed before the last
    /// settings change.
    epoch: Epoch,
}
/// Frames to come from before the last settings change. A change written
/// while the camera streams reaches the frame whose exposure starts after
/// it: on a free-running stream the frames already sent and waiting in the
/// host's buffers, the frame being read out and the one being exposed
/// still carry the old settings, as do the frames of software triggers
/// fired before the change, and a hardware-triggered frame exposed before
/// it. Where the camera's clock can be latched, a frame it stamped before
/// the change, or the first one after, which may have been exposed in part
/// before, predates it too, wherever it waited. A frame of an acquisition
/// started after the change is always new.
#[derive(Debug, Default)]
struct Epoch {
    /// Frames still to come that may predate the change.
    stale: u32,
    /// When no frame from before the change can come any more, so `stale`
    /// counts no longer: after a stream is switched to software triggers,
    /// no frame may come to count its frames in flight down.
    counted_until: Option<Instant>,
    /// Until when a hardware-triggered camera's frames may predate it.
    stale_until: Option<Instant>,
    /// The camera's clock at the change, where it can be latched.
    since_ns: Option<u64>,
    /// Software triggers fired whose frames have not come yet.
    pending: u32,
    /// Whether the frame `next_frame` returned last may predate it.
    last_stale: bool,
}
/// Frames a free-running stream may deliver after a change, besides those
/// already in the host's buffers, that were exposed at least in part
/// before it: the one being read out and the one being exposed.
const STALE_FRAMES: u32 = 2;
/// What a settings change means for the frames to come; see
/// `Camera::settings_changed`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Change {
    /// For a camera that waits for a hardware trigger for each frame: how
    /// long after the change a frame exposed before it may still come.
    pub(crate) hardware_hold: Option<Duration>,
    /// Whether it sent frames only when software triggered before the
    /// change.
    pub(crate) was_software: bool,
    /// How long a frame being exposed at the change may take to come.
    pub(crate) in_flight: Duration,
}
/// Reject malformed known source forms without checking availability or build capabilities.
/// Bare camera IDs and serials are resolved against discovery results on open.
pub fn validate_selector_shape(selector: &str) -> Result<()> {
    ensure!(
        !selector.trim().is_empty() && !selector.chars().any(char::is_control),
        "camera selector must be nonempty and contain no control characters"
    );
    if selector.starts_with("sim:") {
        crate::transport::simulator::validate_selector_shape(selector)?;
    } else if let Some(address) = selector.strip_prefix("gige:") {
        // Discovery names GigE cameras by their 48-bit MAC address.
        let discovered_id = address.len() == 12 && address.bytes().all(|b| b.is_ascii_hexdigit());
        ensure!(
            discovered_id || address.parse::<Ipv4Addr>().is_ok(),
            "GigE selector needs an IPv4 address or a discovered ID, for example gige:192.168.1.10"
        );
    } else if prefixed(selector, "hub:") {
        return Err(crate::types::Categorized::new(
            crate::types::ErrorKind::Usage,
            format!(
                "'{}' names a camera hub, which Capturefab no longer opens",
                crate::media::redact_url(selector)
            ),
        )
        .hint("use the camera's RTSP stream from the hub, for example -c rtsp://HOST:8554/CAMERA from Frigate or go2rtc")
        .into());
    } else if prefixed(selector, "scanner:") {
        return Err(crate::types::Categorized::new(
            crate::types::ErrorKind::Usage,
            "document scanners are not supported",
        )
        .hint("capture from a camera instead")
        .into());
    } else if crate::onvif::is_source(selector) {
        crate::onvif::validate_source_shape(selector)?;
    } else if crate::media::is_url_source(selector) || selector.contains("://") {
        crate::media::validate_source_shape(selector)?;
    } else if malformed_ipv4(selector) {
        return Err(crate::types::Usage(format!(
            "'{selector}' is not a valid IPv4 address; use A.B.C.D or gige:A.B.C.D"
        ))
        .into());
    }
    Ok(())
}

/// `selector` starts with `prefix`, ignoring ASCII case.
fn prefixed(selector: &str, prefix: &str) -> bool {
    selector
        .get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
}

/// How a bare selector (no `sim:`, `gige:`, URL or other explicit form)
/// matched the discovered cameras.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// One camera has this ID, serial or address.
    Exact(CameraInfo),
    /// One camera matched loosely: its model or the ID after the first ':',
    /// ignoring case, or whole words of its model.
    Loose {
        info: CameraInfo,
        matched_on: &'static str,
    },
    /// Several cameras matched; never guess among them.
    Ambiguous(Vec<CameraInfo>),
    NotFound,
}

/// Selectors whose syntax already says what they open; they are never
/// matched loosely against discovered cameras.
pub fn is_explicit_selector(selector: &str) -> bool {
    selector.starts_with("sim:")
        || selector.starts_with("gige:")
        || selector.parse::<Ipv4Addr>().is_ok()
        || selector.contains("://")
        || crate::onvif::is_source(selector)
        || crate::media::is_source(selector)
}

/// The discovery ID of the GigE camera a MAC address written with colons
/// or dashes (`00:c0:df:12:34:56`) names.
fn mac_id(selector: &str) -> Option<String> {
    let groups = selector.split([':', '-']).collect::<Vec<_>>();
    (groups.len() == 6
        && groups
            .iter()
            .all(|group| group.len() == 2 && group.bytes().all(|b| b.is_ascii_hexdigit())))
    .then(|| format!("gige:{}", groups.concat().to_ascii_lowercase()))
}

/// Whether `selector` is the ID, serial, address or MAC address of `info`.
fn names_exactly(selector: &str, info: &CameraInfo) -> bool {
    info.id == selector
        || info.serial == selector
        || info.address.as_deref() == Some(selector)
        || mac_id(selector).is_some_and(|id| id == info.id)
}

/// Resolve a bare selector against discovered cameras. Exact ID, serial,
/// address or MAC address wins; otherwise a unique case-insensitive model
/// or name match, then a unique run of whole model words.
pub fn resolve_selector(selector: &str, known: &[CameraInfo]) -> Resolution {
    let exact: Vec<_> = known
        .iter()
        .filter(|d| names_exactly(selector, d))
        .cloned()
        .collect();
    match exact.len() {
        1 => return Resolution::Exact(exact.into_iter().next().unwrap()),
        0 => {}
        _ => return Resolution::Ambiguous(exact),
    }
    if is_explicit_selector(selector) || selector.trim().is_empty() {
        return Resolution::NotFound;
    }
    let wanted = selector.to_lowercase();
    let suffix = |d: &CameraInfo| {
        d.id.split_once(':')
            .map_or(d.id.as_str(), |(_, rest)| rest)
            .to_lowercase()
    };
    let pick = |matches: Vec<(&CameraInfo, &'static str)>| match matches.as_slice() {
        [] => None,
        [(info, matched_on)] => Some(Resolution::Loose {
            info: (*info).clone(),
            matched_on,
        }),
        _ => Some(Resolution::Ambiguous(
            matches.iter().map(|(info, _)| (*info).clone()).collect(),
        )),
    };
    let equal = known
        .iter()
        .filter_map(|d| {
            if d.model.to_lowercase() == wanted {
                Some((d, "model"))
            } else if suffix(d) == wanted
                || d.id.to_lowercase() == wanted
                || d.serial.to_lowercase() == wanted
            {
                Some((d, "name"))
            } else {
                None
            }
        })
        .collect();
    if let Some(resolution) = pick(equal) {
        return resolution;
    }
    // Then whole words of a model (`C920` in `HD Pro Webcam C920`), never
    // part of a word, an ID or a serial: a partial or stale serial must not
    // open another camera.
    let words = wanted.split_whitespace().collect::<Vec<_>>();
    if wanted.chars().count() < 3 || words.is_empty() {
        return Resolution::NotFound;
    }
    let partial = known
        .iter()
        .filter(|d| {
            let model = d.model.to_lowercase();
            let model = model.split_whitespace().collect::<Vec<_>>();
            model.windows(words.len()).any(|window| window == words)
        })
        .map(|d| (d, "model"))
        .collect();
    pick(partial).unwrap_or(Resolution::NotFound)
}

/// A selector that names a local file rather than a camera.
fn path_like(selector: &str) -> bool {
    const MEDIA: [&str; 14] = [
        "mp4", "mov", "mkv", "avi", "webm", "ts", "m4v", "jpg", "jpeg", "png", "tif", "tiff",
        "bmp", "gif",
    ];
    selector.contains(['/', '\\'])
        || std::path::Path::new(selector)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| MEDIA.contains(&extension.to_ascii_lowercase().as_str()))
}

/// Four dot-separated digit groups that are not a valid IPv4 address.
fn malformed_ipv4(selector: &str) -> bool {
    let parts: Vec<_> = selector.split('.').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        && selector.parse::<Ipv4Addr>().is_err()
}

/// `sim0`, `SIM0` or `SIM:0` written for the simulator `sim:0`.
fn simulator_typo(selector: &str) -> Option<String> {
    if selector.starts_with("sim:") {
        return None;
    }
    let lower = selector.to_ascii_lowercase();
    let rest = lower.strip_prefix("sim")?;
    let digits = rest.strip_prefix(':').unwrap_or(rest);
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| format!("sim:{digits}"))
}

/// `Pattern camera`, the simulator's model, which `list` shows only with
/// `--kind sim`.
fn simulator_model(selector: &str) -> Option<String> {
    selector
        .trim()
        .eq_ignore_ascii_case("pattern camera")
        .then(|| "sim:0".into())
}

/// Bare selectors discovery cannot help: a missing file path or a malformed
/// IPv4 address fails at once instead of after a scan.
pub fn needs_no_discovery(selector: &str) -> bool {
    path_like(selector) || malformed_ipv4(selector)
}

/// A camera as `camera_not_found` and `camera_ambiguous` list it.
fn candidate(info: &CameraInfo) -> serde_json::Value {
    serde_json::json!({
        "id": crate::media::redact_url(&info.id),
        "model": info.model,
        "serial": (!info.serial.is_empty() && info.serial != info.id).then(|| info.serial.clone()),
    })
}

/// Why `selector` did not open, with the closest cameras that did answer.
fn selector_not_matched(
    selector: &str,
    known: &[CameraInfo],
    matches: &[CameraInfo],
) -> anyhow::Error {
    use crate::types::{Categorized, ErrorKind};
    let shown = crate::media::redact_url(selector);
    let list = |cameras: &[&CameraInfo]| serde_json::json!({ "candidates": cameras.iter().take(5).map(|d| candidate(d)).collect::<Vec<_>>() });
    if matches.len() > 1 {
        let listed = matches
            .iter()
            .take(5)
            .map(|d| format!("{} ({})", crate::media::redact_url(&d.id), d.model))
            .collect::<Vec<_>>()
            .join(", ");
        let more = if matches.len() > 5 { ", ..." } else { "" };
        return Categorized::new(
            ErrorKind::CameraAmbiguous,
            format!(
                "camera '{shown}' is ambiguous: it matches {} cameras: {listed}{more}; pass a camera ID or serial",
                matches.len()
            ),
        )
        .details(list(&matches.iter().collect::<Vec<_>>()))
        .into();
    }
    let not_found = |message: String, cameras: &[&CameraInfo], hint: &str| -> anyhow::Error {
        Categorized::new(ErrorKind::CameraNotFound, message)
            .hint(hint)
            .details(list(cameras))
            .into()
    };
    if let Some(simulator) = simulator_typo(selector).or_else(|| simulator_model(selector)) {
        return not_found(
            format!("camera '{shown}' not found; did you mean {simulator}?"),
            &[],
            &format!("use -c {simulator}"),
        );
    }
    if path_like(selector) {
        return not_found(
            format!("file not found: {shown}"),
            &[],
            "check the path; relative paths are from this terminal's folder",
        );
    }
    let mut candidates: Vec<(usize, &CameraInfo)> = known
        .iter()
        .filter_map(|d| {
            let suffix = d.id.split_once(':').map_or(d.id.as_str(), |(_, rest)| rest);
            [d.id.as_str(), suffix, d.serial.as_str(), d.model.as_str()]
                .into_iter()
                .filter(|name| crate::hint::is_close(selector, name))
                .map(|name| crate::hint::edit_distance(selector, name))
                .min()
                .map(|distance| (distance, d))
        })
        .collect();
    candidates.sort_by_key(|(distance, _)| *distance);
    if let Some((_, best)) = candidates.first() {
        let best_id = crate::hint::shell_quote(&crate::media::redact_url(&best.id));
        return not_found(
            format!("camera '{shown}' not found; did you mean {best_id}?"),
            &candidates.iter().map(|(_, d)| *d).collect::<Vec<_>>(),
            &format!("use -c {best_id}, or run capturefab list"),
        );
    }
    if known.is_empty() {
        return not_found(
            format!("camera '{shown}' not found"),
            &[],
            "run capturefab list to see cameras",
        );
    }
    let found = known
        .iter()
        .take(5)
        .map(|d| crate::media::redact_url(&d.id))
        .collect::<Vec<_>>()
        .join(", ");
    let more = if known.len() > 5 { ", ..." } else { "" };
    not_found(
        format!("camera '{shown}' not found. Found: {found}{more}"),
        &known.iter().collect::<Vec<_>>(),
        "pass one of details.candidates with -c",
    )
}

/// Check both selector syntax and local source availability before opening.
pub fn validate_selector(selector: &str) -> Result<()> {
    validate_selector_shape(selector)?;
    if let Some(source) = crate::transport::simulator::replay_source(selector) {
        crate::transport::simulator::replay::source_files(source)?;
    } else if crate::onvif::is_source(selector) {
        crate::onvif::validate_source(selector)?;
    } else if (crate::media::is_source(selector) || selector.contains("://"))
        && !crate::onvif::is_source(selector)
    {
        crate::media::validate_source(selector)?;
    }
    Ok(())
}
impl Camera {
    pub fn open(selector: &str, known: &[CameraInfo], timeout: Duration) -> Result<Self> {
        Self::open_with_login(selector, known, timeout, None)
    }
    /// Open a camera with a sign-in given beside its selector. Only ONVIF
    /// and media URL cameras use it; the session rejects a sign-in for any
    /// other kind before it gets here.
    pub fn open_with_login(
        selector: &str,
        known: &[CameraInfo],
        timeout: Duration,
        login: Option<&crate::logins::CameraLogin>,
    ) -> Result<Self> {
        validate_selector(selector)?;
        if crate::onvif::is_source(selector) {
            let (info, backend) = crate::onvif::connect_with(selector, login, timeout)?;
            return Self::from_backend(info, backend);
        }
        let info = Self::identify(selector, known, timeout)?;
        Self::open_identified(selector, info, timeout, login)
    }
    /// What `selector` names, read without opening the camera: a GigE camera
    /// answers its identity to anyone. Not for ONVIF sources, which open as
    /// they are identified.
    pub fn identify(selector: &str, known: &[CameraInfo], timeout: Duration) -> Result<CameraInfo> {
        Ok(if selector.starts_with("sim:") {
            crate::transport::simulator::info_named(selector)
        } else if crate::media::is_source(selector) {
            crate::media::info(selector)?
        } else if let Ok(ip) = selector
            .strip_prefix("gige:")
            .unwrap_or(selector)
            .parse::<Ipv4Addr>()
        {
            crate::transport::gige::probe(ip, timeout)?
        } else {
            match resolve_selector(selector, known) {
                Resolution::Exact(info) | Resolution::Loose { info, .. } => info,
                Resolution::Ambiguous(matches) => {
                    return Err(selector_not_matched(selector, known, &matches));
                }
                Resolution::NotFound => return Err(selector_not_matched(selector, known, &[])),
            }
        })
    }
    /// Open the camera `identify` found for `selector`.
    pub fn open_identified(
        selector: &str,
        info: CameraInfo,
        timeout: Duration,
        login: Option<&crate::logins::CameraLogin>,
    ) -> Result<Self> {
        let backend: Box<dyn Backend> = match info.transport {
            Transport::Simulator => crate::transport::simulator::open(&info.id, timeout)?,
            Transport::Media => {
                if crate::onvif::is_source(&info.id) {
                    let (info, backend) = crate::onvif::connect_with(&info.id, login, timeout)?;
                    return Self::from_backend(info, backend);
                }
                if crate::media::is_source(selector) {
                    crate::media::open_url_with(selector, login, timeout)?
                } else {
                    crate::media::open(&info, timeout)?
                }
            }
            Transport::GigE => crate::transport::gige::open(&info, timeout)?,
            Transport::Usb3 => {
                #[cfg(feature = "usb")]
                {
                    crate::transport::usb::open(&info, timeout)?
                }
                #[cfg(not(feature = "usb"))]
                {
                    anyhow::bail!("USB support disabled in this build")
                }
            }
        };
        Self::from_backend(info, backend)
    }
    pub fn from_backend(mut info: CameraInfo, mut backend: Box<dyn Backend>) -> Result<Self> {
        let mut xml = backend.xml().context("load camera GenICam XML")?;
        let nodes = match NodeMap::parse(&xml) {
            // A copy from the disk cache that does not parse, or links to a
            // node it does not have, is read again from the device.
            parsed
                if !parsed
                    .as_ref()
                    .is_ok_and(|nodes| nodes.unresolved().is_none())
                    && backend.forget_xml() =>
            {
                xml = backend.xml().context("load camera GenICam XML")?;
                NodeMap::parse(&xml)
            }
            parsed => parsed,
        }
        .context("parse camera GenICam XML")?;
        let mut profile_metadata = nodes.profile_features(backend.as_mut());
        info.profile = crate::camera_profile::detect(&info, &profile_metadata);
        // Feature evidence must be implemented and available now. Only the
        // few evidence nodes are checked, not every node at connection.
        for _ in 0..4 {
            if info.profile.source != crate::camera_profile::DetectionSource::Features {
                break;
            }
            let hidden: Vec<_> = info
                .profile
                .evidence
                .iter()
                .filter(|name| {
                    nodes.has(name)
                        && nodes.availability(backend.as_mut(), name)
                            != crate::genicam::Availability::Available
                })
                .cloned()
                .collect();
            if hidden.is_empty() {
                break;
            }
            profile_metadata.retain(|feature| !hidden.contains(&feature.name));
            info.profile = crate::camera_profile::detect(&info, &profile_metadata);
        }
        // Devices may keep their identifying nodes outside Root or Invisible.
        // Preserve those declarations when normal feature snapshots omit them.
        profile_metadata.retain(|feature| {
            matches!(
                feature.name.as_str(),
                "DeviceType"
                    | "CameraType"
                    | "SensorType"
                    | "DeviceScanType"
                    | "PixelFormat"
                    | "ComponentSelector"
                    | "Scan3dOutputMode"
            )
        });
        // A host that ended without stopping, such as a killed worker, can
        // leave a device-side TLParamsLocked set, which would keep features
        // locked on a camera that is not streaming.
        if nodes.has(TL_PARAMS_LOCKED)
            && nodes
                .get(backend.as_mut(), TL_PARAMS_LOCKED)
                .is_ok_and(|locked| locked != 0)
            && nodes.is_writable(backend.as_mut(), TL_PARAMS_LOCKED)
        {
            let _ = nodes.set(backend.as_mut(), TL_PARAMS_LOCKED, "0");
        }
        Ok(Self {
            info,
            backend,
            nodes,
            streaming: false,
            xml,
            profile_metadata,
            network_budget_bytes_per_second: None,
            tl_locked: false,
            transferring: false,
            software_triggers: None,
            epoch: Epoch::default(),
        })
    }
    pub fn features(&mut self) -> Vec<FeatureInfo> {
        let mut features = self.nodes.features(self.backend.as_mut());
        let visible_count = features.len();
        for declaration in &self.profile_metadata {
            if !features
                .iter()
                .any(|feature| feature.name == declaration.name)
            {
                features.push(declaration.clone());
            }
        }
        self.info.profile = crate::camera_profile::detect(&self.info, &features);
        features.truncate(visible_count);
        features
    }
    pub fn get(&mut self, name: &str) -> Result<serde_json::Value> {
        self.nodes.get(self.backend.as_mut(), name)
    }
    pub(crate) fn selectors(&self, name: &str) -> Vec<String> {
        self.nodes.selectors(name)
    }
    /// TLParamsLocked is acquisition state that start and stop own, so it
    /// is never a configuration value to record, restore or set.
    pub(crate) fn is_configuration_value(&mut self, name: &str) -> bool {
        name != TL_PARAMS_LOCKED
            && self
                .nodes
                .is_configuration_value(self.backend.as_mut(), name)
    }
    /// Whether the camera refuses changes to `name` while it streams: the
    /// image geometry and format, and any feature its XML locks through
    /// TLParamsLocked, which start sets.
    pub fn locked_while_streaming(&self, name: &str) -> bool {
        crate::genicam::stream_locked(name) || self.nodes.locked_by(name, TL_PARAMS_LOCKED)
    }
    pub(crate) fn availability(&mut self, name: &str) -> crate::genicam::Availability {
        self.nodes.availability(self.backend.as_mut(), name)
    }
    pub(crate) fn entry_availability(
        &mut self,
        name: &str,
        symbol: &str,
    ) -> crate::genicam::Availability {
        self.nodes
            .entry_availability(self.backend.as_mut(), name, symbol)
    }
    pub(crate) fn is_locked(&mut self, name: &str) -> bool {
        self.nodes.is_locked(self.backend.as_mut(), name)
    }
    pub(crate) fn lock_released_by(&self, name: &str, others: &[&str]) -> bool {
        self.nodes.lock_released_by(name, others)
    }
    pub(crate) fn feature_names(&self) -> Vec<&str> {
        self.nodes.feature_names()
    }
    pub(crate) fn feature(&mut self, name: &str) -> Option<FeatureInfo> {
        self.nodes.feature(self.backend.as_mut(), name)
    }
    pub(crate) fn lock_owner(&self, name: &str) -> Option<String> {
        self.nodes.lock_owner(name)
    }
    pub(crate) fn visibility(&self, name: &str) -> &str {
        self.nodes.visibility(name)
    }
    pub(crate) fn category(&self, name: &str) -> Option<&str> {
        self.nodes.category(name)
    }
    pub(crate) fn static_access(&self, name: &str) -> &'static str {
        self.nodes.static_access(name)
    }
    pub(crate) fn static_max(&self, name: &str) -> Option<f64> {
        self.nodes.static_max(name)
    }
    pub fn set(&mut self, name: &str, value: &str) -> Result<()> {
        ensure!(name != TL_PARAMS_LOCKED, "{TL_PARAMS_LOCKED_MESSAGE}");
        self.software_triggers = None;
        self.nodes.set(self.backend.as_mut(), name, value)
    }
    pub fn validate_set(&mut self, name: &str, value: &str) -> Result<()> {
        ensure!(name != TL_PARAMS_LOCKED, "{TL_PARAMS_LOCKED_MESSAGE}");
        self.nodes.validate_set(self.backend.as_mut(), name, value)
    }
    pub fn has(&self, name: &str) -> bool {
        self.nodes.has(name)
    }
    pub(crate) fn node_kind(&self, name: &str) -> Option<&str> {
        self.nodes.kind(name)
    }
    pub(crate) fn feature_unit(&self, name: &str) -> Option<&str> {
        self.nodes.unit(name)
    }
    pub fn is_writable(&mut self, name: &str) -> bool {
        self.nodes.is_writable(self.backend.as_mut(), name)
    }
    pub fn bounds(&mut self, name: &str) -> Result<Bounds> {
        self.nodes.bounds(self.backend.as_mut(), name)
    }
    pub fn choices(&mut self, name: &str) -> Result<Vec<String>> {
        self.nodes.choices(self.backend.as_mut(), name)
    }
    pub fn stats(&self) -> Option<TransportStats> {
        self.backend.stats()
    }
    pub fn network_budget_bytes_per_second(&self) -> Option<u64> {
        self.network_budget_bytes_per_second
    }
    pub(crate) fn set_network_budget_bytes_per_second(&mut self, budget: u64) {
        self.network_budget_bytes_per_second = Some(budget.max(1));
    }
    /// Apply measured GigE defaults without interfering with an acquisition.
    pub fn optimize_network(&mut self) -> Result<crate::network::NetworkTuning> {
        ensure!(
            !self.streaming,
            "stop acquisition before optimizing the camera network"
        );
        let size = self
            .get("PayloadSize")?
            .as_u64()
            .context("PayloadSize must be a positive integer")?;
        ensure!(
            size > 0 && size <= 256 * 1024 * 1024,
            "camera payload exceeds 256 MiB limit"
        );
        self.backend.optimize_network(size as usize)
    }
    pub fn execute(&mut self, name: &str) -> Result<()> {
        ensure!(
            !STREAM_COMMANDS.contains(&name),
            "use start/stop to manage transport and acquisition together"
        );
        self.software_triggers = None;
        self.nodes.execute(self.backend.as_mut(), name)?;
        // Let a command such as UserSetLoad finish before its effects are
        // read back; one whose target never clears just returns later.
        self.nodes
            .wait_done(self.backend.as_mut(), name, Duration::from_secs(2))?;
        Ok(())
    }
    /// Start acquisition as a GenTL producer would: lock the transport
    /// layer parameters (SFNC TLParamsLocked), which many cameras require
    /// before AcquisitionStart, then size buffers from the locked payload.
    pub fn start(&mut self) -> Result<()> {
        if self.streaming {
            return Ok(());
        }
        self.acquire_continuously()?;
        if self.has(TL_PARAMS_LOCKED) && self.is_writable(TL_PARAMS_LOCKED) {
            self.nodes
                .set(self.backend.as_mut(), TL_PARAMS_LOCKED, "1")?;
            self.tl_locked = true;
        }
        if let Err(error) = self.begin() {
            let _ = self.unlock();
            return Err(error);
        }
        self.streaming = true;
        // Every frame of a new acquisition is exposed after any change.
        self.epoch = Epoch::default();
        Ok(())
    }
    /// A camera left in SingleFrame or MultiFrame ends each acquisition by
    /// itself after a frame or a few, which would stall a stream and fail
    /// later captures: switch it to Continuous, before TLParamsLocked can
    /// lock the mode. The camera keeps the new mode after stop.
    fn acquire_continuously(&mut self) -> Result<()> {
        let limited = self.has("AcquisitionMode")
            && self
                .get("AcquisitionMode")
                .is_ok_and(|mode| mode == "SingleFrame" || mode == "MultiFrame");
        if limited
            && self.is_writable("AcquisitionMode")
            && self
                .choices("AcquisitionMode")
                .is_ok_and(|modes| modes.iter().any(|mode| mode == "Continuous"))
        {
            self.nodes
                .set(self.backend.as_mut(), "AcquisitionMode", "Continuous")?;
        }
        Ok(())
    }
    fn begin(&mut self) -> Result<()> {
        let size = self
            .get("PayloadSize")?
            .as_u64()
            .context("PayloadSize must be a positive integer")?;
        ensure!(
            size > 0 && size <= 256 * 1024 * 1024,
            "camera payload exceeds 256 MiB limit"
        );
        self.backend.start(size as usize)?;
        if let Err(e) = self
            .nodes
            .execute(self.backend.as_mut(), "AcquisitionStart")
        {
            let _ = self.backend.stop();
            return Err(e);
        }
        if self.user_controlled_transfer() {
            if let Err(e) = self.nodes.execute(self.backend.as_mut(), "TransferStart") {
                let _ = self.nodes.execute(self.backend.as_mut(), "AcquisitionStop");
                let _ = self.backend.stop();
                return Err(e);
            }
            self.transferring = true;
        }
        Ok(())
    }
    /// A camera in TransferControlMode UserControlled sends acquired frames
    /// only between TransferStart and TransferStop (SFNC Transfer Control).
    fn user_controlled_transfer(&mut self) -> bool {
        self.has("TransferControlMode")
            && self.has("TransferStart")
            && self
                .get("TransferControlMode")
                .is_ok_and(|mode| mode == "UserControlled")
    }
    fn unlock(&mut self) -> Result<()> {
        if !std::mem::take(&mut self.tl_locked) {
            return Ok(());
        }
        self.nodes.set(self.backend.as_mut(), TL_PARAMS_LOCKED, "0")
    }
    pub fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
        ensure!(self.streaming, "camera is not streaming");
        let mut frame = self.backend.next_frame(timeout)?;
        if frame.wall_ms == 0 {
            frame.wall_ms = crate::scheduling::now_ms();
        }
        let now = Instant::now();
        let epoch = &mut self.epoch;
        let counted = epoch.stale > 0 && epoch.counted_until.is_none_or(|until| now < until);
        let held = epoch.stale_until.is_some_and(|until| now < until);
        let clocked = match (epoch.since_ns, frame.device_ts_ns) {
            (Some(since), Some(stamped)) if stamped < since => true,
            // The first frame stamped after the change, at the end of its
            // exposure on some cameras, may have begun before it.
            (Some(_), Some(_)) => {
                epoch.since_ns = None;
                true
            }
            _ => false,
        };
        epoch.last_stale = counted || held || clocked;
        epoch.stale = epoch.stale.saturating_sub(1);
        epoch.pending = epoch.pending.saturating_sub(1);
        Ok(frame)
    }
    /// Ask for the next frame. A camera whose start trigger is On with a
    /// Software source sends one only after TriggerSoftware; one waiting for
    /// a hardware trigger still waits for its line.
    pub fn request_frame(&mut self) -> Result<()> {
        ensure!(self.streaming, "camera is not streaming");
        if self.software_triggered() {
            self.trigger_software()?;
            self.epoch.pending = self.epoch.pending.saturating_add(1);
        }
        self.backend.request_frame()
    }
    /// Give up on the frame of the oldest software trigger fired: the
    /// transport lost it, or it never came.
    pub(crate) fn trigger_unanswered(&mut self) {
        self.epoch.pending = self.epoch.pending.saturating_sub(1);
    }
    /// Note that settings were written: until frames exposed after the
    /// change come, `frame_predates_change` says so of each frame.
    pub(crate) fn settings_changed(&mut self, change: Change) {
        if !self.streaming {
            return;
        }
        // Frames already on their way left the camera before the change.
        let buffered = self.backend.buffered_frames();
        let now = Instant::now();
        if let Some(hold) = change.hardware_hold {
            self.epoch.stale = buffered;
            self.epoch.counted_until = None;
            self.epoch.stale_until = now.checked_add(hold);
        } else if self.software_triggered() {
            let in_flight = if change.was_software {
                self.epoch.pending.max(buffered)
            } else {
                // Switched from free-running: its frames being read out
                // and exposed may still come, or the camera may drop them.
                self.epoch
                    .pending
                    .saturating_add(buffered)
                    .saturating_add(STALE_FRAMES)
            };
            let counting = self.epoch.counted_until.is_none_or(|until| now < until);
            let earlier = if counting { self.epoch.stale } else { 0 };
            self.epoch.stale = earlier.max(in_flight);
            self.epoch.counted_until = now.checked_add(change.in_flight);
        } else {
            self.epoch.stale = buffered.saturating_add(STALE_FRAMES);
            self.epoch.counted_until = None;
            // A clock that reads 0 was never latched.
            self.epoch.since_ns = self.backend.device_time_ns().filter(|ns| *ns > 0);
        }
    }
    /// Settings this process emulates for the camera; see
    /// `Backend::emulated_settings`.
    pub(crate) fn emulated_settings(&mut self) -> Option<serde_json::Value> {
        self.backend.emulated_settings()
    }
    /// Take back settings another process emulated for the camera.
    pub(crate) fn restore_settings(&mut self, settings: &serde_json::Value) -> Result<()> {
        self.software_triggers = None;
        self.backend.restore_settings(settings)
    }
    /// Whether the frame `next_frame` returned last may have been exposed
    /// before the last settings change.
    pub(crate) fn frame_predates_change(&self) -> bool {
        self.epoch.last_stale
    }
    /// Whether the camera sends frames only when software triggered.
    pub fn software_triggered(&mut self) -> bool {
        !self.software_triggers().is_empty()
    }
    fn software_triggers(&mut self) -> Vec<Option<String>> {
        if let Some(triggers) = &self.software_triggers {
            return triggers.clone();
        }
        let triggers = self.find_software_triggers();
        self.software_triggers = Some(triggers.clone());
        triggers
    }
    /// The start triggers (SFNC TriggerSelector) set to On with a Software
    /// source. A camera offering none of them is read at its selected
    /// trigger, and one without a selector has a single trigger.
    fn find_software_triggers(&mut self) -> Vec<Option<String>> {
        if !["TriggerMode", "TriggerSource", "TriggerSoftware"]
            .iter()
            .all(|name| self.has(name))
        {
            return Vec::new();
        }
        let software = |camera: &mut Self| {
            camera.get("TriggerMode").is_ok_and(|mode| mode == "On")
                && camera
                    .get("TriggerSource")
                    .is_ok_and(|source| source == "Software")
        };
        if !self.has("TriggerSelector") {
            return if software(self) {
                vec![None]
            } else {
                Vec::new()
            };
        }
        let Ok(serde_json::Value::String(selected)) = self.get("TriggerSelector") else {
            return Vec::new();
        };
        let offered = self.choices("TriggerSelector").unwrap_or_default();
        let mut candidates = START_TRIGGERS
            .into_iter()
            .filter(|trigger| offered.iter().any(|choice| choice == trigger))
            .map(String::from)
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            candidates.push(selected.clone());
        }
        let mut found = Vec::new();
        let mut current = selected.clone();
        for candidate in candidates {
            if candidate != current {
                if self.select_trigger(&candidate).is_err() {
                    continue;
                }
                current = candidate.clone();
            }
            if software(self) {
                found.push(Some(candidate));
            }
        }
        if current != selected {
            let _ = self.select_trigger(&selected);
        }
        found
    }
    fn select_trigger(&mut self, trigger: &str) -> Result<()> {
        self.nodes
            .set(self.backend.as_mut(), "TriggerSelector", trigger)
    }
    /// Execute TriggerSoftware for each software start trigger, then select
    /// the trigger that was selected before.
    fn trigger_software(&mut self) -> Result<()> {
        let triggers = self.software_triggers();
        if triggers.is_empty() {
            return Ok(());
        }
        let before = if triggers.iter().any(Option::is_some) {
            self.get("TriggerSelector")?.as_str().map(String::from)
        } else {
            None
        };
        let mut selected = before.clone();
        let last = triggers.len() - 1;
        let fired = (|| -> Result<()> {
            for (index, trigger) in triggers.into_iter().enumerate() {
                if let Some(trigger) = trigger
                    && selected.as_deref() != Some(trigger.as_str())
                {
                    self.select_trigger(&trigger)?;
                    selected = Some(trigger);
                }
                let fired = self.nodes.execute(self.backend.as_mut(), "TriggerSoftware");
                // An acquisition or burst that an earlier request started may
                // refuse another start; the frame trigger after it still counts.
                if index == last {
                    fired?;
                }
            }
            Ok(())
        })();
        match before {
            Some(before) if selected.as_ref() != Some(&before) => {
                fired.and(self.select_trigger(&before))
            }
            _ => fired,
        }
    }
    pub fn stop(&mut self) -> Result<()> {
        if !self.streaming {
            return Ok(());
        }
        // Some cameras lock AcquisitionStop too until TLParamsLocked is set,
        // so the lock is released last.
        let acquisition = self.nodes.execute(self.backend.as_mut(), "AcquisitionStop");
        let transfer = if std::mem::take(&mut self.transferring) {
            self.nodes.execute(self.backend.as_mut(), "TransferStop")
        } else {
            Ok(())
        };
        let transport = self.backend.stop();
        self.streaming = false;
        let unlocked = self.unlock();
        acquisition?;
        transfer?;
        transport?;
        unlocked
    }
    pub fn xml(&self) -> &str {
        &self.xml
    }
    pub fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        self.backend.read_memory(address, length)
    }
    pub fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
        self.software_triggers = None;
        self.backend.write_memory(address, data)
    }
    /// See `Backend::lingers`.
    pub(crate) fn lingers(&self) -> bool {
        self.backend.lingers()
    }
    /// See `Backend::rest`; a streaming camera keeps what it streams with.
    pub(crate) fn rest(&mut self) {
        if !self.streaming {
            self.backend.rest();
        }
    }
    pub fn is_streaming(&self) -> bool {
        self.streaming
    }
}
impl Drop for Camera {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::camera_profile::{CameraCapability, CameraKind, DetectionSource};
    use std::sync::{Arc, Mutex};

    #[test]
    fn selectors_are_looked_for_only_where_they_can_be() {
        let gige = CameraInfo {
            id: "gige:00c0df123456".into(),
            transport: Transport::GigE,
            vendor: "Acme".into(),
            model: "Line Scan 4K".into(),
            serial: "SN42".into(),
            address: Some("192.168.1.20".into()),
            profile: Default::default(),
        };
        for selector in [
            "gige:00c0df123456",
            "SN42",
            "192.168.1.20",
            "00:C0:DF:12:34:56",
            "00-c0-df-12-34-56",
        ] {
            assert!(names_exactly(selector, &gige), "{selector}");
        }
        assert!(!names_exactly("SN4", &gige));
        assert!(!names_exactly("00c0df12345", &gige));
        assert_eq!(
            resolve_selector("00:c0:df:12:34:56", std::slice::from_ref(&gige)),
            Resolution::Exact(gige.clone())
        );
        let only = |gige, u3v| Kinds {
            gige,
            u3v,
            host: false,
            onvif: false,
            sim: false,
        };
        assert_eq!(kinds_for("gige:00c0df123456"), only(true, false));
        assert_eq!(kinds_for("00:c0:df:12:34:56"), only(true, false));
        assert_eq!(kinds_for("usb:2676:ba02:SN1"), only(false, true));
        // A serial or a model may be anywhere but the simulator.
        assert_eq!(
            kinds_for("SN42"),
            Kinds {
                sim: false,
                ..Kinds::all()
            }
        );
        // A 12-digit serial is not a MAC address.
        assert!(mac_id("123456789012").is_none());
        // A model names one camera only among every camera there is.
        let cached = std::slice::from_ref(&gige);
        assert_eq!(cached_match("SN42", cached, false), Some(gige.clone()));
        assert_eq!(cached_match("Line Scan 4K", cached, false), None);
        assert_eq!(
            cached_match("Line Scan 4K", cached, true),
            Some(gige.clone())
        );
    }

    #[test]
    fn a_broadcasts_list_replaces_the_answers_it_told_of() {
        let onvif = |endpoint: &str| CameraInfo {
            id: format!("onvif://{endpoint}"),
            transport: Transport::Media,
            vendor: "Acme".into(),
            model: "Dome".into(),
            serial: "D1".into(),
            address: Some(endpoint.into()),
            profile: Default::default(),
        };
        let gige = CameraInfo {
            id: "gige:00c0df123456".into(),
            transport: Transport::GigE,
            vendor: "Acme".into(),
            model: "Line Scan 4K".into(),
            serial: "SN42".into(),
            address: Some("192.168.1.20".into()),
            profile: Default::default(),
        };
        // One device answered from two addresses; ONVIF's own list merges
        // them into one camera.
        let mut heard = Heard::default();
        heard.told("ONVIF", onvif("10.0.0.5"));
        heard.told("ONVIF", onvif("192.168.1.5"));
        heard.told("GigE", gige.clone());
        assert!(heard.any(&|info: &CameraInfo| info.serial == "D1"));
        heard.listed("ONVIF", vec![onvif("10.0.0.5")]);
        let ids = |heard: Heard| {
            heard
                .cameras()
                .into_iter()
                .map(|info| info.id)
                .collect::<Vec<_>>()
        };
        // A broadcast that ended before its list keeps what it told of.
        assert_eq!(ids(heard), ["onvif://10.0.0.5", "gige:00c0df123456"]);
    }

    #[test]
    fn opened_media_sources_keep_their_kind() {
        let clip = std::env::temp_dir().join(format!("capturefab-kind-{}.mkv", std::process::id()));
        std::fs::write(&clip, b"").unwrap();
        for (selector, kind) in [
            ("avfoundation:0", "webcam"),
            ("v4l2:/dev/video0", "webcam"),
            ("rtsp://user:secret@camera.local/live", "stream"),
            ("http://camera.local/snapshot.jpg", "stream"),
            (clip.to_str().unwrap(), "file"),
        ] {
            let info = crate::media::info(selector).unwrap();
            assert!(info.id.starts_with("media:"), "{selector}");
            assert_eq!(super::kind(&info), kind, "{selector}");
        }
        let _ = std::fs::remove_file(&clip);
    }

    const PAYLOAD: u64 = 0x00;
    const ACQUISITION_START: u64 = 0x04;
    const ACQUISITION_STOP: u64 = 0x08;
    const ACQUISITION_MODE: u64 = 0x0c;
    const FRAME_COUNT: u64 = 0x10;
    const TRIGGER_SELECTOR: u64 = 0x14;
    const TRIGGER_MODE: u64 = 0x20;
    const TRIGGER_SOURCE: u64 = 0x30;
    /// The FrameStart trigger's mode (1 is On) and source (0 is Software,
    /// 1 is Line1).
    pub(crate) const FRAME_TRIGGER_MODE: u64 = TRIGGER_MODE + 4;
    pub(crate) const FRAME_TRIGGER_SOURCE: u64 = TRIGGER_SOURCE + 4;
    const TRIGGER_SOFTWARE: u64 = 0x40;
    const TRANSFER_MODE: u64 = 0x50;
    const TRANSFER_START: u64 = 0x54;
    const TRANSFER_STOP: u64 = 0x58;
    const ACQUISITION_ABORT: u64 = 0x5c;
    const LOCKS: u64 = 0x60;
    const SELECTORS: [&str; 3] = ["AcquisitionStart", "FrameStart", "FrameBurstStart"];

    /// A device that follows SFNC acquisition control over a small register
    /// map: it streams only between AcquisitionStart and AcquisitionStop,
    /// ends a SingleFrame or MultiFrame acquisition by itself, sends nothing
    /// in UserControlled transfer mode before TransferStart, and with the
    /// FrameStart trigger on delivers one frame per software trigger, or
    /// none while it waits for a hardware line. Tests give it the vendor XML
    /// nodes they exercise.
    pub(crate) struct Device {
        xml: String,
        memory: [u32; 32],
        /// Transport and device commands, in the order they happened.
        pub(crate) events: Arc<Mutex<Vec<String>>>,
        receiving: bool,
        acquiring: bool,
        transferring: bool,
        left: Option<u32>,
        triggers: u32,
        frames: u64,
    }
    impl Device {
        pub(crate) fn new(nodes: &str) -> Self {
            let mut memory = [0; 32];
            memory[(PAYLOAD / 4) as usize] = 16;
            memory[(ACQUISITION_MODE / 4) as usize] = 2;
            memory[(FRAME_COUNT / 4) as usize] = 1;
            Self {
                xml: format!(
                    r#"<RegisterDescription ModelName="Fixture" VendorName="Capturefab"><Category Name="Root"><pFeature>PayloadSize</pFeature><pFeature>Fixture</pFeature></Category><Integer Name="PayloadSize"><pValue>PayloadReg</pValue></Integer><IntReg Name="PayloadReg"><Address>0x00</Address><Length>4</Length><AccessMode>RO</AccessMode><Endianess>LittleEndian</Endianess></IntReg>{nodes}</RegisterDescription>"#
                ),
                memory,
                events: Arc::default(),
                receiving: false,
                acquiring: false,
                transferring: false,
                left: None,
                triggers: 0,
                frames: 0,
            }
        }
        pub(crate) fn with(mut self, address: u64, value: u32) -> Self {
            self.memory[(address / 4) as usize] = value;
            self
        }
        fn word(&self, address: u64) -> u32 {
            self.memory[(address / 4) as usize]
        }
        fn event(&self, event: impl Into<String>) {
            self.events.lock().unwrap().push(event.into());
        }
    }
    impl crate::types::RegisterIo for Device {
        fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
            ensure!(length == 4 && address.is_multiple_of(4), "unaligned read");
            Ok(self.word(address).to_le_bytes().to_vec())
        }
        fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
            ensure!(
                data.len() == 4 && address.is_multiple_of(4),
                "unaligned write"
            );
            ensure!(address != PAYLOAD && address != LOCKS, "read-only register");
            let value = u32::from_le_bytes(data.try_into()?);
            self.memory[(address / 4) as usize] = value;
            match address {
                ACQUISITION_START => {
                    self.acquiring = true;
                    self.triggers = 0;
                    self.left = match self.word(ACQUISITION_MODE) {
                        0 => Some(1),
                        1 => Some(self.word(FRAME_COUNT)),
                        _ => None,
                    };
                    self.event("AcquisitionStart");
                }
                ACQUISITION_STOP | ACQUISITION_ABORT => {
                    self.acquiring = false;
                    self.event(if address == ACQUISITION_STOP {
                        "AcquisitionStop"
                    } else {
                        "AcquisitionAbort"
                    });
                }
                TRANSFER_START => {
                    self.transferring = true;
                    self.event("TransferStart");
                }
                TRANSFER_STOP => {
                    self.transferring = false;
                    self.event("TransferStop");
                }
                _ if (TRIGGER_SOFTWARE..TRIGGER_SOFTWARE + 12).contains(&address) => {
                    let selector = ((address - TRIGGER_SOFTWARE) / 4) as usize;
                    if selector == 1
                        && self.acquiring
                        && self.word(TRIGGER_MODE + 4) == 1
                        && self.word(TRIGGER_SOURCE + 4) == 0
                    {
                        self.triggers += 1;
                    }
                    self.event(format!("TriggerSoftware {}", SELECTORS[selector]));
                }
                _ => {}
            }
            Ok(())
        }
    }
    impl Backend for Device {
        fn xml(&mut self) -> Result<String> {
            Ok(self.xml.clone())
        }
        fn start(&mut self, payload: usize) -> Result<()> {
            ensure!(payload == 16, "unexpected payload size {payload}");
            self.receiving = true;
            self.event("receive");
            Ok(())
        }
        fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
            let triggered = self.word(TRIGGER_MODE + 4) == 1;
            let ready = self.receiving
                && self.acquiring
                && (self.word(TRANSFER_MODE) != 2 || self.transferring)
                && self.left != Some(0)
                && (!triggered || self.word(TRIGGER_SOURCE + 4) == 0 && self.triggers > 0);
            if !ready {
                std::thread::sleep(timeout);
                anyhow::bail!("frame timeout");
            }
            if triggered {
                self.triggers -= 1;
            }
            if let Some(left) = self.left.as_mut() {
                *left -= 1;
                self.acquiring = *left > 0;
            }
            self.frames += 1;
            Ok(Frame {
                id: self.frames,
                width: 4,
                height: 4,
                pixel_format: crate::types::MONO8,
                device_ts_ns: Some(self.frames),
                wall_ms: 0,
                data: vec![0; 16],
            })
        }
        fn stop(&mut self) -> Result<()> {
            self.receiving = false;
            self.event("release");
            Ok(())
        }
    }

    pub(crate) const TL_PARAMS_LOCKED: &str = r#"<Integer Name="TLParamsLocked"><ToolTip> Indicates whether a live grab is under way</ToolTip><Visibility>Invisible</Visibility><Value>0</Value><Min>0</Min><Max>1</Max></Integer>"#;
    /// Acquisition commands with no lock, as on cameras that never gate them.
    pub(crate) const OPEN_ACQUISITION: &str = r#"<Command Name="AcquisitionStart"><pValue>AcquisitionStartReg</pValue><CommandValue>1</CommandValue></Command><IntReg Name="AcquisitionStartReg"><Address>0x04</Address><Length>4</Length><AccessMode>WO</AccessMode><Endianess>LittleEndian</Endianess></IntReg><Command Name="AcquisitionStop"><pValue>AcquisitionStopReg</pValue><CommandValue>1</CommandValue></Command><IntReg Name="AcquisitionStopReg"><Address>0x08</Address><Length>4</Length><AccessMode>WO</AccessMode><Endianess>LittleEndian</Endianess></IntReg>"#;
    /// Basler ace GigE (acA1300-60gm): both commands locked by N411 until
    /// the transport layer sets TLParamsLocked.
    pub(crate) const BASLER_ACQUISITION: &str = r#"<Command Name="AcquisitionStart" NameSpace="Standard"><ToolTip>Starts the acquisition of images</ToolTip><pIsLocked>N411</pIsLocked><ImposedAccessMode>WO</ImposedAccessMode><pValue>N412</pValue><CommandValue>1</CommandValue></Command><Command Name="AcquisitionStop" NameSpace="Standard"><ToolTip>Stops the acquisition of images</ToolTip><pIsLocked>N411</pIsLocked><ImposedAccessMode>WO</ImposedAccessMode><pValue>N413</pValue><CommandValue>0</CommandValue></Command><IntSwissKnife Name="N411"><pVariable Name="P1">TLParamsLocked</pVariable><Formula>P1=0</Formula></IntSwissKnife><IntReg Name="N412"><Address>0x04</Address><Length>4</Length><AccessMode>WO</AccessMode><Endianess>LittleEndian</Endianess></IntReg><IntReg Name="N413"><Address>0x08</Address><Length>4</Length><AccessMode>WO</AccessMode><Endianess>LittleEndian</Endianess></IntReg>"#;
    /// FLIR Blackfly S (BFS-PGE-23S6C, BFS-U3-161S7M): AcquisitionStart
    /// locked by the camera's own lock bit or an unlocked transport layer.
    const FLIR_ACQUISITION: &str = r#"<Command Name="AcquisitionStart" NameSpace="Standard"><ToolTip>Starts the acquisition of images</ToolTip><Visibility>Beginner</Visibility><pIsLocked>Acquisition_Locked</pIsLocked><pValue>AcquisitionStart_Val</pValue><CommandValue>1</CommandValue></Command><IntSwissKnife Name="Acquisition_Locked"><Visibility>Invisible</Visibility><pVariable Name="TLPL">TLParamsLocked</pVariable><pVariable Name="LCK">AcquisitionStart_Lck</pVariable><Formula>LCK || ((TLPL = 0) ? 1 : 0)</Formula></IntSwissKnife><MaskedIntReg Name="AcquisitionStart_Lck"><Address>0x60</Address><Length>4</Length><AccessMode>RO</AccessMode><Bit>3</Bit><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></MaskedIntReg><IntReg Name="AcquisitionStart_Val"><Address>0x04</Address><Length>4</Length><AccessMode>WO</AccessMode><Endianess>LittleEndian</Endianess></IntReg><Command Name="AcquisitionStop" NameSpace="Standard"><Visibility>Beginner</Visibility><pValue>AcquisitionStop_Val</pValue><CommandValue>1</CommandValue></Command><IntReg Name="AcquisitionStop_Val"><Address>0x08</Address><Length>4</Length><AccessMode>WO</AccessMode><Endianess>LittleEndian</Endianess></IntReg>"#;

    /// AcquisitionMode and AcquisitionFrameCount, locked by the transport
    /// layer as on FLIR Blackfly S, with Basler ace entry values.
    const ACQUISITION_MODE_NODES: &str = r#"<Enumeration Name="AcquisitionMode" NameSpace="Standard"><pIsLocked>TLParamsLocked</pIsLocked><EnumEntry Name="SingleFrame" NameSpace="Standard"><Value>0</Value></EnumEntry><EnumEntry Name="MultiFrame" NameSpace="Standard"><Value>1</Value></EnumEntry><EnumEntry Name="Continuous" NameSpace="Standard"><Value>2</Value></EnumEntry><pValue>AcquisitionModeReg</pValue></Enumeration><IntReg Name="AcquisitionModeReg"><Address>0x0c</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></IntReg><Integer Name="AcquisitionFrameCount" NameSpace="Standard"><pIsLocked>TLParamsLocked</pIsLocked><pValue>FrameCountReg</pValue><Min>1</Min><Max>255</Max><Inc>1</Inc></Integer><IntReg Name="FrameCountReg"><Address>0x10</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></IntReg>"#;

    /// FLIR Blackfly S transfer control: TransferStart is locked until the
    /// transport layer locks its parameters.
    const TRANSFER_NODES: &str = r#"<Enumeration Name="TransferControlMode" NameSpace="Standard"><ImposedAccessMode>RW</ImposedAccessMode><EnumEntry Name="Basic" NameSpace="Standard"><Value>0</Value></EnumEntry><EnumEntry Name="Automatic" NameSpace="Standard"><Value>1</Value></EnumEntry><EnumEntry Name="UserControlled" NameSpace="Standard"><Value>2</Value></EnumEntry><pValue>TransferControlMode_Val</pValue></Enumeration><IntReg Name="TransferControlMode_Val"><Address>0x50</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></IntReg><Command Name="TransferStart" NameSpace="Standard"><pIsLocked>TransferStart_Locked</pIsLocked><ImposedAccessMode>WO</ImposedAccessMode><pValue>XmitStart_Val</pValue><CommandValue>1</CommandValue></Command><IntSwissKnife Name="TransferStart_Locked"><pVariable Name="TLP">TLParamsLocked</pVariable><pVariable Name="LCK">XmitStart_Lck</pVariable><Formula>(TLP = 0) || LCK</Formula></IntSwissKnife><MaskedIntReg Name="XmitStart_Lck"><Address>0x60</Address><Length>4</Length><AccessMode>RO</AccessMode><Bit>4</Bit><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></MaskedIntReg><IntReg Name="XmitStart_Val"><Address>0x54</Address><Length>4</Length><AccessMode>WO</AccessMode><Endianess>LittleEndian</Endianess></IntReg><Command Name="TransferStop" NameSpace="Standard"><ImposedAccessMode>WO</ImposedAccessMode><pValue>XmitStop_Val</pValue><CommandValue>1</CommandValue></Command><IntReg Name="XmitStop_Val"><Address>0x58</Address><Length>4</Length><AccessMode>WO</AccessMode><Endianess>LittleEndian</Endianess></IntReg>"#;

    /// SFNC trigger control: TriggerMode, TriggerSource and TriggerSoftware
    /// banked by TriggerSelector, with the start triggers FLIR and Basler
    /// cameras offer.
    pub(crate) const TRIGGER_NODES: &str = r#"<Enumeration Name="TriggerSelector" NameSpace="Standard"><EnumEntry Name="AcquisitionStart" NameSpace="Standard"><Value>0</Value></EnumEntry><EnumEntry Name="FrameStart" NameSpace="Standard"><Value>1</Value></EnumEntry><EnumEntry Name="FrameBurstStart" NameSpace="Standard"><Value>2</Value></EnumEntry><pValue>TriggerSelectorReg</pValue><pSelected>TriggerMode</pSelected><pSelected>TriggerSoftware</pSelected><pSelected>TriggerSource</pSelected></Enumeration><IntReg Name="TriggerSelectorReg"><Address>0x14</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></IntReg><Enumeration Name="TriggerMode" NameSpace="Standard"><EnumEntry Name="Off" NameSpace="Standard"><Value>0</Value></EnumEntry><EnumEntry Name="On" NameSpace="Standard"><Value>1</Value></EnumEntry><pValue>TriggerModeReg</pValue></Enumeration><IntReg Name="TriggerModeReg"><Address>0x20</Address><pIndex Offset="4">TriggerSelectorReg</pIndex><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></IntReg><Enumeration Name="TriggerSource" NameSpace="Standard"><EnumEntry Name="Software" NameSpace="Standard"><Value>0</Value></EnumEntry><EnumEntry Name="Line1" NameSpace="Standard"><Value>1</Value></EnumEntry><pValue>TriggerSourceReg</pValue></Enumeration><IntReg Name="TriggerSourceReg"><Address>0x30</Address><pIndex Offset="4">TriggerSelectorReg</pIndex><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></IntReg><Command Name="TriggerSoftware" NameSpace="Standard"><ImposedAccessMode>WO</ImposedAccessMode><pValue>TriggerSoftwareReg</pValue><CommandValue>1</CommandValue></Command><IntReg Name="TriggerSoftwareReg"><Address>0x40</Address><pIndex Offset="4">TriggerSelectorReg</pIndex><Length>4</Length><AccessMode>WO</AccessMode><Endianess>LittleEndian</Endianess></IntReg>"#;

    pub(crate) fn device_camera(device: Device) -> Camera {
        Camera::from_backend(crate::transport::simulator::info(), Box::new(device)).unwrap()
    }
    pub(crate) fn events(device: &Device) -> Arc<Mutex<Vec<String>>> {
        device.events.clone()
    }
    pub(crate) fn taken(events: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        std::mem::take(&mut *events.lock().unwrap())
    }

    #[test]
    fn acquisition_locked_until_tl_params_locked_starts_and_unlocks() {
        for vendor in [BASLER_ACQUISITION, FLIR_ACQUISITION] {
            let device = Device::new(&format!("{TL_PARAMS_LOCKED}{vendor}"));
            let log = events(&device);
            let mut camera = device_camera(device);
            assert_eq!(camera.get("TLParamsLocked").unwrap(), 0);
            camera.start().unwrap();
            assert_eq!(camera.get("TLParamsLocked").unwrap(), 1);
            camera.next_frame(Duration::from_millis(10)).unwrap();
            camera.stop().unwrap();
            assert_eq!(camera.get("TLParamsLocked").unwrap(), 0);
            assert_eq!(
                taken(&log),
                ["receive", "AcquisitionStart", "AcquisitionStop", "release"]
            );
            // Restarting locks again.
            camera.start().unwrap();
            assert_eq!(camera.get("TLParamsLocked").unwrap(), 1);
        }
    }

    #[test]
    fn a_failed_start_unlocks_the_transport_layer_parameters() {
        // The FLIR lock bit holds AcquisitionStart however TLParamsLocked is set.
        let device = Device::new(&format!("{TL_PARAMS_LOCKED}{FLIR_ACQUISITION}")).with(LOCKS, 8);
        let log = events(&device);
        let mut camera = device_camera(device);
        let error = camera.start().unwrap_err();
        assert!(
            error.to_string().contains("AcquisitionStart is locked"),
            "{error:#}"
        );
        assert!(!camera.is_streaming());
        assert_eq!(camera.get("TLParamsLocked").unwrap(), 0);
        assert_eq!(taken(&log), ["receive", "release"]);
    }

    #[test]
    fn features_locked_by_the_transport_layer_are_read_only_while_streaming() {
        // As GevSCPSPacketSize, PixelFormat and Width are on Basler and FLIR cameras.
        let device = Device::new(&format!(
            r#"{TL_PARAMS_LOCKED}{BASLER_ACQUISITION}<Integer Name="GevSCPSPacketSize"><pIsLocked>TLParamsLocked</pIsLocked><Value>1500</Value><Min>220</Min><Max>9000</Max><Inc>4</Inc></Integer>"#
        ));
        let mut camera = device_camera(device);
        assert!(camera.is_writable("GevSCPSPacketSize"));
        camera.start().unwrap();
        assert!(!camera.is_writable("GevSCPSPacketSize"));
        let error = camera.set("GevSCPSPacketSize", "9000").unwrap_err();
        assert!(error.to_string().contains("is locked"), "{error:#}");
        assert_eq!(crate::cli::error_code(&error), ("feature_locked", 1));
        camera.stop().unwrap();
        camera.set("GevSCPSPacketSize", "9000").unwrap();
    }

    #[test]
    fn single_and_multi_frame_cameras_stream_continuously() {
        for (mode, frames) in [(0, 1), (1, 2)] {
            let device = Device::new(&format!(
                "{TL_PARAMS_LOCKED}{FLIR_ACQUISITION}{ACQUISITION_MODE_NODES}"
            ))
            .with(ACQUISITION_MODE, mode)
            .with(FRAME_COUNT, frames);
            let mut camera = device_camera(device);
            camera.start().unwrap();
            assert_eq!(camera.get("AcquisitionMode").unwrap(), "Continuous");
            for _ in 0..4 {
                camera.next_frame(Duration::from_millis(10)).unwrap();
            }
            camera.stop().unwrap();
            assert_eq!(camera.get("AcquisitionMode").unwrap(), "Continuous");
        }
        // Without a Continuous mode the camera keeps its own.
        let single = ACQUISITION_MODE_NODES.replace(
            r#"<EnumEntry Name="Continuous" NameSpace="Standard"><Value>2</Value></EnumEntry>"#,
            "",
        );
        let device = Device::new(&format!("{OPEN_ACQUISITION}{single}")).with(ACQUISITION_MODE, 0);
        let mut camera = device_camera(device);
        camera.start().unwrap();
        assert_eq!(camera.get("AcquisitionMode").unwrap(), "SingleFrame");
        camera.next_frame(Duration::from_millis(10)).unwrap();
        assert!(camera.next_frame(Duration::from_millis(10)).is_err());
    }

    #[test]
    fn user_controlled_transfer_starts_and_stops_with_acquisition() {
        let device = Device::new(&format!(
            "{TL_PARAMS_LOCKED}{FLIR_ACQUISITION}{TRANSFER_NODES}"
        ))
        .with(TRANSFER_MODE, 2);
        let log = events(&device);
        let mut camera = device_camera(device);
        camera.start().unwrap();
        camera.next_frame(Duration::from_millis(10)).unwrap();
        camera.stop().unwrap();
        assert_eq!(
            taken(&log),
            [
                "receive",
                "AcquisitionStart",
                "TransferStart",
                "AcquisitionStop",
                "TransferStop",
                "release"
            ]
        );
        // A refused TransferStart stops acquisition and the receiver again.
        let device = Device::new(&format!(
            "{TL_PARAMS_LOCKED}{FLIR_ACQUISITION}{TRANSFER_NODES}"
        ))
        .with(TRANSFER_MODE, 2)
        .with(LOCKS, 16);
        let log = events(&device);
        let mut camera = device_camera(device);
        let error = camera.start().unwrap_err();
        assert!(
            error.to_string().contains("TransferStart is locked"),
            "{error:#}"
        );
        assert_eq!(camera.get("TLParamsLocked").unwrap(), 0);
        assert_eq!(
            taken(&log),
            ["receive", "AcquisitionStart", "AcquisitionStop", "release"]
        );
        // Basic and Automatic transfers follow acquisition by themselves.
        for mode in [0, 1] {
            let device = Device::new(&format!(
                "{TL_PARAMS_LOCKED}{FLIR_ACQUISITION}{TRANSFER_NODES}"
            ))
            .with(TRANSFER_MODE, mode);
            let log = events(&device);
            let mut camera = device_camera(device);
            camera.start().unwrap();
            camera.next_frame(Duration::from_millis(10)).unwrap();
            camera.stop().unwrap();
            assert_eq!(
                taken(&log),
                ["receive", "AcquisitionStart", "AcquisitionStop", "release"]
            );
        }
    }

    #[test]
    fn acquisition_and_transfer_commands_are_left_to_start_and_stop() {
        let device = Device::new(&format!(
            r#"{OPEN_ACQUISITION}{TRANSFER_NODES}<Command Name="AcquisitionAbort"><pValue>AcquisitionAbortReg</pValue><CommandValue>1</CommandValue></Command><IntReg Name="AcquisitionAbortReg"><Address>0x5c</Address><Length>4</Length><AccessMode>WO</AccessMode><Endianess>LittleEndian</Endianess></IntReg>"#
        ));
        let log = events(&device);
        let mut camera = device_camera(device);
        camera.start().unwrap();
        taken(&log);
        for command in ["AcquisitionAbort", "TransferStop", "AcquisitionStart"] {
            let error = camera.execute(command).unwrap_err();
            assert!(error.to_string().contains("use start/stop"), "{error:#}");
        }
        assert!(taken(&log).is_empty());
        assert!(camera.is_streaming());
        camera.next_frame(Duration::from_millis(10)).unwrap();
    }

    /// A stream whose frames wait in the host's buffers, stamped by a clock
    /// the camera can latch: the test queues the frames it sends.
    struct Queued {
        device: Device,
        frames: std::collections::VecDeque<u64>,
        buffered: u32,
        clock: Option<u64>,
    }
    impl crate::types::RegisterIo for Queued {
        fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
            self.device.read_memory(address, length)
        }
        fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
            self.device.write_memory(address, data)
        }
    }
    impl Backend for Queued {
        fn xml(&mut self) -> Result<String> {
            self.device.xml()
        }
        fn start(&mut self, payload: usize) -> Result<()> {
            self.device.start(payload)
        }
        fn next_frame(&mut self, _: Duration) -> Result<Frame> {
            let stamped = self.frames.pop_front().context("frame timeout")?;
            Ok(Frame {
                id: stamped,
                width: 4,
                height: 4,
                pixel_format: crate::types::MONO8,
                device_ts_ns: Some(stamped),
                wall_ms: 0,
                data: vec![0; 16],
            })
        }
        fn stop(&mut self) -> Result<()> {
            self.device.stop()
        }
        fn buffered_frames(&mut self) -> u32 {
            self.buffered
        }
        fn device_time_ns(&mut self) -> Option<u64> {
            self.clock
        }
    }
    /// A streaming camera that sends frames stamped `stamps`, and which of
    /// them predate a change made before the first.
    fn after_change(
        device: Device,
        buffered: u32,
        clock: Option<u64>,
        change: impl FnOnce(&mut Camera) -> Change,
        stamps: &[u64],
    ) -> Vec<bool> {
        let queued = Queued {
            device,
            frames: stamps.iter().copied().collect(),
            buffered,
            clock,
        };
        let mut camera =
            Camera::from_backend(crate::transport::simulator::info(), Box::new(queued)).unwrap();
        camera.start().unwrap();
        let change = change(&mut camera);
        camera.settings_changed(change);
        stamps
            .iter()
            .map(|_| {
                camera.next_frame(Duration::ZERO).unwrap();
                camera.frame_predates_change()
            })
            .collect()
    }

    #[test]
    fn frames_sent_before_a_change_predate_it_wherever_they_waited() {
        let plain = || Device::new(OPEN_ACQUISITION);
        // Three frames wait in the host's buffers, then come the frame being
        // read out and the one being exposed.
        assert_eq!(
            after_change(plain(), 3, None, |_| Change::default(), &[1, 2, 3, 4, 5, 6]),
            [true, true, true, true, true, false]
        );
        // The camera's clock says more: frames it stamped before the change
        // waited in the camera itself, and the first after it may have
        // begun before it.
        assert_eq!(
            after_change(
                plain(),
                0,
                Some(1000),
                |_| Change::default(),
                &[10, 20, 900, 1010, 1040]
            ),
            [true, true, true, true, false]
        );
        // A clock that reads 0 was never latched.
        assert_eq!(
            after_change(plain(), 0, Some(0), |_| Change::default(), &[1, 2, 3]),
            [true, true, false]
        );
        // A hardware-triggered camera's frames for a while, besides those
        // already sent.
        let hold = Change {
            hardware_hold: Some(Duration::from_secs(60)),
            ..Change::default()
        };
        assert_eq!(
            after_change(plain(), 1, None, |_| hold, &[1, 2]),
            [true, true]
        );
    }

    #[test]
    fn a_stream_switched_to_software_triggers_counts_its_frames_in_flight_for_a_while() {
        let triggers = || Device::new(&format!("{OPEN_ACQUISITION}{TRIGGER_NODES}"));
        let switch = |in_flight: Duration| {
            move |camera: &mut Camera| {
                camera.set("TriggerSelector", "FrameStart").unwrap();
                camera.set("TriggerSource", "Software").unwrap();
                camera.set("TriggerMode", "On").unwrap();
                Change {
                    in_flight,
                    ..Change::default()
                }
            }
        };
        // The free-running frames being read out and exposed when the
        // trigger went on, besides the one already sent.
        assert_eq!(
            after_change(
                triggers(),
                1,
                None,
                switch(Duration::from_secs(60)),
                &[1, 2, 3, 4]
            ),
            [true, true, true, false]
        );
        // The camera dropped them: once they could have come, the frames of
        // the triggers fired since are new.
        let dropped = after_change(triggers(), 0, None, switch(Duration::ZERO), &[1]);
        assert_eq!(dropped, [false]);
        // Already software triggered: only the frames of triggers fired
        // before the change predate it.
        let mut camera = device_camera(
            triggers()
                .with(TRIGGER_SELECTOR, 1)
                .with(FRAME_TRIGGER_MODE, 1),
        );
        camera.start().unwrap();
        camera.request_frame().unwrap();
        camera.settings_changed(Change {
            was_software: true,
            in_flight: Duration::from_secs(60),
            ..Change::default()
        });
        camera.next_frame(Duration::from_millis(10)).unwrap();
        assert!(camera.frame_predates_change());
        camera.request_frame().unwrap();
        camera.next_frame(Duration::from_millis(10)).unwrap();
        assert!(!camera.frame_predates_change());
    }

    #[test]
    fn software_frame_triggers_fire_once_per_requested_frame() {
        // Selected: AcquisitionStart, as a camera often is after setup.
        let device = Device::new(&format!("{OPEN_ACQUISITION}{TRIGGER_NODES}"))
            .with(TRIGGER_SELECTOR, 0)
            .with(FRAME_TRIGGER_MODE, 1);
        let log = events(&device);
        let mut camera = device_camera(device);
        camera.start().unwrap();
        assert!(camera.software_triggered());
        assert!(camera.next_frame(Duration::from_millis(10)).is_err());
        for _ in 0..3 {
            camera.request_frame().unwrap();
            camera.next_frame(Duration::from_millis(10)).unwrap();
        }
        assert!(camera.next_frame(Duration::from_millis(10)).is_err());
        assert_eq!(camera.get("TriggerSelector").unwrap(), "AcquisitionStart");
        assert_eq!(
            taken(&log),
            [
                "receive",
                "AcquisitionStart",
                "TriggerSoftware FrameStart",
                "TriggerSoftware FrameStart",
                "TriggerSoftware FrameStart"
            ]
        );
        // Changing the trigger is seen on the next request.
        camera.set("TriggerSelector", "FrameStart").unwrap();
        camera.set("TriggerMode", "Off").unwrap();
        assert!(!camera.software_triggered());
        camera.request_frame().unwrap();
        camera.next_frame(Duration::from_millis(10)).unwrap();
        assert!(taken(&log).is_empty());
    }

    #[test]
    fn hardware_and_free_running_cameras_get_no_software_trigger() {
        for (mode, source) in [(1, 1), (0, 0)] {
            let device = Device::new(&format!("{OPEN_ACQUISITION}{TRIGGER_NODES}"))
                .with(TRIGGER_SELECTOR, 1)
                .with(FRAME_TRIGGER_MODE, mode)
                .with(FRAME_TRIGGER_SOURCE, source);
            let log = events(&device);
            let mut camera = device_camera(device);
            camera.start().unwrap();
            camera.request_frame().unwrap();
            assert!(!camera.software_triggered());
            // A camera waiting for Line1 still waits; a free-running one streams.
            assert_eq!(
                camera.next_frame(Duration::from_millis(10)).is_ok(),
                mode == 0
            );
            assert_eq!(taken(&log), ["receive", "AcquisitionStart"]);
        }
    }

    #[test]
    fn a_camera_without_trigger_selector_fires_its_only_trigger() {
        let device = Device::new(&format!(
            r#"{OPEN_ACQUISITION}<Enumeration Name="TriggerMode"><EnumEntry Name="Off"><Value>0</Value></EnumEntry><EnumEntry Name="On"><Value>1</Value></EnumEntry><pValue>TriggerModeReg</pValue></Enumeration><IntReg Name="TriggerModeReg"><Address>0x24</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></IntReg><Enumeration Name="TriggerSource"><EnumEntry Name="Software"><Value>0</Value></EnumEntry><EnumEntry Name="Line1"><Value>1</Value></EnumEntry><pValue>TriggerSourceReg</pValue></Enumeration><IntReg Name="TriggerSourceReg"><Address>0x34</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></IntReg><Command Name="TriggerSoftware"><pValue>TriggerSoftwareReg</pValue><CommandValue>1</CommandValue></Command><IntReg Name="TriggerSoftwareReg"><Address>0x44</Address><Length>4</Length><AccessMode>WO</AccessMode><Endianess>LittleEndian</Endianess></IntReg>"#
        ))
        .with(FRAME_TRIGGER_MODE, 1);
        let mut camera = device_camera(device);
        camera.start().unwrap();
        camera.request_frame().unwrap();
        camera.next_frame(Duration::from_millis(10)).unwrap();
    }

    #[test]
    fn a_lock_left_on_the_device_is_cleared_on_open() {
        let device = Device::new(
            r#"<Integer Name="TLParamsLocked"><Visibility>Invisible</Visibility><pValue>TLParamsLockedReg</pValue><Min>0</Min><Max>1</Max></Integer><IntReg Name="TLParamsLockedReg"><Address>0x68</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></IntReg>"#,
        )
        .with(0x68, 1);
        let mut camera = device_camera(device);
        assert_eq!(camera.get("TLParamsLocked").unwrap(), 0);
        let error = camera.set("TLParamsLocked", "1").unwrap_err();
        assert!(error.to_string().contains("start and stop"), "{error:#}");
        assert!(!camera.is_configuration_value("TLParamsLocked"));
    }

    #[test]
    fn cameras_without_tl_params_locked_start_unchanged() {
        let device = Device::new(OPEN_ACQUISITION);
        let log = events(&device);
        let mut camera = device_camera(device);
        camera.start().unwrap();
        camera.next_frame(Duration::from_millis(10)).unwrap();
        camera.stop().unwrap();
        assert_eq!(
            taken(&log),
            ["receive", "AcquisitionStart", "AcquisitionStop", "release"]
        );
        // A read-only status node is left alone.
        let device = Device::new(&format!(
            r#"{OPEN_ACQUISITION}<Integer Name="TLParamsLocked"><Value>0</Value><ImposedAccessMode>RO</ImposedAccessMode></Integer>"#
        ));
        let mut camera = device_camera(device);
        camera.start().unwrap();
        assert_eq!(camera.get("TLParamsLocked").unwrap(), 0);
    }

    #[test]
    fn typed_simulator_serial_preserves_its_backend() {
        let info = crate::transport::simulator::info_named("sim:depth:alias");
        let mut camera = Camera::open(
            &info.serial,
            std::slice::from_ref(&info),
            Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(camera.info.id, "sim:depth:alias");
        assert_eq!(camera.info.profile.kind, CameraKind::Depth);
        assert_eq!(camera.get("PixelFormat").unwrap(), "Coord3D_C16");
    }

    #[test]
    fn hidden_type_metadata_survives_refresh_without_extra_feature_polling() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct MetadataBackend(Arc<AtomicUsize>);
        impl crate::types::RegisterIo for MetadataBackend {
            fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
                self.0.fetch_add(1, Ordering::Relaxed);
                if address == 0 {
                    let mut value = b"Linescan".to_vec();
                    value.resize(length, 0);
                    Ok(value)
                } else {
                    Ok(vec![0; length])
                }
            }
            fn write_memory(&mut self, _: u64, _: &[u8]) -> Result<()> {
                Ok(())
            }
        }
        impl Backend for MetadataBackend {
            fn xml(&mut self) -> Result<String> {
                Ok(r#"<RegisterDescription>
                    <Category Name="Root"><pFeature>ExposureTime</pFeature><pFeature>Width</pFeature><pFeature>Height</pFeature><pFeature>AcquisitionFrameRate</pFeature></Category>
                    <StringReg Name="DeviceScanType"><Address>0</Address><Length>32</Length><AccessMode>RO</AccessMode><Visibility>Invisible</Visibility></StringReg>
                    <FloatReg Name="ExposureTime"><Address>64</Address><Length>8</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></FloatReg>
                    <Integer Name="Width"><Value>640</Value></Integer>
                    <Integer Name="Height"><Value>1</Value></Integer>
                    <Float Name="AcquisitionFrameRate"><Value>30</Value></Float>
                    </RegisterDescription>"#.into())
            }
            fn start(&mut self, _: usize) -> Result<()> {
                Ok(())
            }
            fn next_frame(&mut self, _: Duration) -> Result<Frame> {
                anyhow::bail!("no frames")
            }
            fn stop(&mut self) -> Result<()> {
                Ok(())
            }
        }
        let reads = Arc::new(AtomicUsize::new(0));
        let info = crate::transport::simulator::info();
        let mut camera =
            Camera::from_backend(info, Box::new(MetadataBackend(reads.clone()))).unwrap();
        assert_eq!(
            reads.load(Ordering::Relaxed),
            1,
            "opening must not poll unrelated exposure registers"
        );
        assert_eq!(camera.info.profile.kind, CameraKind::LineScan);
        assert!(
            camera
                .info
                .profile
                .capabilities
                .contains(&CameraCapability::Exposure)
        );
        assert!(!camera.features().iter().any(|f| f.name == "DeviceScanType"));
        assert_eq!(camera.info.profile.kind, CameraKind::LineScan);
        assert_eq!(camera.info.profile.source, DetectionSource::DeviceMetadata);
    }

    #[test]
    fn unavailable_line_rate_node_does_not_classify_an_area_camera() {
        struct Fixture(&'static str);
        impl crate::types::RegisterIo for Fixture {
            fn read_memory(&mut self, _: u64, length: usize) -> Result<Vec<u8>> {
                Ok(vec![0; length])
            }
            fn write_memory(&mut self, _: u64, _: &[u8]) -> Result<()> {
                Ok(())
            }
        }
        impl Backend for Fixture {
            fn xml(&mut self) -> Result<String> {
                Ok(self.0.into())
            }
            fn start(&mut self, _: usize) -> Result<()> {
                Ok(())
            }
            fn next_frame(&mut self, _: Duration) -> Result<Frame> {
                anyhow::bail!("no frames")
            }
            fn stop(&mut self) -> Result<()> {
                Ok(())
            }
        }
        // An area sensor whose line scan mode is off hides its line rate.
        let xml = r#"<RegisterDescription>
            <Integer Name="Width"><Value>640</Value></Integer>
            <Integer Name="Height"><Value>480</Value></Integer>
            <Float Name="AcquisitionFrameRate"><Value>30</Value></Float>
            <Float Name="AcquisitionLineRate"><Value>5000</Value><pIsAvailable>LinescanMode</pIsAvailable></Float>
            <IntReg Name="LinescanMode"><Address>0</Address><Length>4</Length><AccessMode>RO</AccessMode></IntReg>
            </RegisterDescription>"#;
        let info = crate::transport::simulator::info();
        let mut camera = Camera::from_backend(info, Box::new(Fixture(xml))).unwrap();
        assert_eq!(camera.info.profile.kind, CameraKind::AreaScan);
        assert!(
            !camera
                .info
                .profile
                .capabilities
                .contains(&CameraCapability::LineRate)
        );
        camera.features();
        assert_eq!(camera.info.profile.kind, CameraKind::AreaScan);
    }

    fn device(id: &str, model: &str, serial: &str) -> CameraInfo {
        let mut info = crate::transport::simulator::info();
        info.id = id.into();
        info.model = model.into();
        info.serial = serial.into();
        info.address = Some(id.into());
        info
    }
    fn known() -> Vec<CameraInfo> {
        vec![
            device(
                "avfoundation:HD Pro Webcam C920",
                "HD Pro Webcam C920",
                "avfoundation:HD Pro Webcam C920",
            ),
            device(
                "avfoundation:Capture screen 0",
                "Capture screen 0",
                "avfoundation:Capture screen 0",
            ),
            device("gige:02007f000001", "Virtual GigE camera", "SIM7F000001"),
        ]
    }
    fn open_error(selector: &str, known: &[CameraInfo]) -> anyhow::Error {
        match Camera::open(selector, known, Duration::from_millis(1)) {
            Ok(_) => panic!("{selector} opened"),
            Err(error) => error,
        }
    }

    #[test]
    fn exact_selectors_win_over_loose_matches() {
        let mut known = known();
        known.push(device("usb:C920", "Other", "C920"));
        let Resolution::Exact(info) = resolve_selector("C920", &known) else {
            panic!("serial must match exactly");
        };
        assert_eq!(info.id, "usb:C920");
    }

    #[test]
    fn unique_loose_names_resolve_and_several_are_ambiguous() {
        let known = known();
        for (selector, id, on) in [
            (
                "hd pro webcam c920",
                "avfoundation:HD Pro Webcam C920",
                "model",
            ),
            ("C920", "avfoundation:HD Pro Webcam C920", "model"),
            ("virtual gige", "gige:02007f000001", "model"),
            ("02007F000001", "gige:02007f000001", "name"),
        ] {
            let Resolution::Loose { info, matched_on } = resolve_selector(selector, &known) else {
                panic!("{selector} did not match loosely");
            };
            assert_eq!((info.id.as_str(), matched_on), (id, on), "{selector}");
        }
        let mut known = known;
        known.push(device(
            "avfoundation:HD Pro Webcam C922",
            "HD Pro Webcam C922",
            "avfoundation:HD Pro Webcam C922",
        ));
        let Resolution::Ambiguous(matches) = resolve_selector("pro webcam", &known) else {
            panic!("two cameras share the name");
        };
        assert_eq!(matches.len(), 2);
        let error = open_error("Pro Webcam", &known);
        let message = error.to_string();
        assert!(message.contains("ambiguous"), "{message}");
        assert!(message.contains("C922"), "{message}");
        assert!(message.contains("HD Pro Webcam C920"), "{message}");
        assert_eq!(crate::cli::error_code(&error), ("camera_ambiguous", 2));
        let details = crate::types::classify(&error).details.unwrap();
        assert_eq!(
            details["candidates"].as_array().unwrap().len(),
            2,
            "{details}"
        );
    }

    #[test]
    fn partial_serials_ids_and_words_never_match() {
        let mut known = known();
        known.push(device("usb:2676:ba02:23015", "Industrial camera", "23015"));
        known.push(device(
            "onvif:http://192.0.2.7:80/onvif/device_service",
            "Lobby PTZ camera",
            "E3248000-80CE-11DB-8000-3C2AF4FA0DE9",
        ));
        for selector in ["2301", "ba02:2301", "E3248", "e3248000", "cam", "webca"] {
            assert_eq!(
                resolve_selector(selector, &known),
                Resolution::NotFound,
                "{selector}"
            );
        }
        assert!(matches!(
            resolve_selector("lobby ptz camera", &known),
            Resolution::Loose { info, .. } if info.id.starts_with("onvif:")
        ));
    }

    #[test]
    fn hub_and_scanner_selectors_are_usage_errors_with_a_way_forward() {
        for selector in ["hub:home/camera.front_door", "HUB:x/y"] {
            let error = validate_selector_shape(selector).unwrap_err();
            assert_eq!(crate::cli::error_code(&error), ("usage", 2));
            let hint = crate::types::classify(&error).hint.unwrap();
            assert!(hint.contains("rtsp://HOST"), "{hint}");
        }
        let error = validate_selector("scanner:sane:device").unwrap_err();
        assert_eq!(crate::cli::error_code(&error), ("usage", 2));
        assert!(error.to_string().contains("not supported"), "{error:#}");
    }

    #[test]
    fn unmatched_selectors_say_what_was_meant() {
        let known = known();
        let cases = [
            ("sim0", "did you mean sim:0?"),
            ("SIM:2", "did you mean sim:2?"),
            ("./nope.mp4", "file not found: ./nope.mp4"),
            ("nope.mp4", "file not found: nope.mp4"),
            ("SIM7F00001", "did you mean gige:02007f000001?"),
            ("Pattern camera", "did you mean sim:0?"),
            ("SIM0", "did you mean sim:0?"),
            ("zzzz", "Found: avfoundation:HD Pro Webcam C920"),
        ];
        for (selector, expected) in cases {
            let error = open_error(selector, &known);
            assert!(
                error.to_string().contains(expected),
                "{selector}: {error:#}"
            );
            assert_eq!(
                crate::cli::error_code(&error),
                ("camera_not_found", 3),
                "{selector}"
            );
        }
        let error = open_error("zzzz", &[]);
        let hint = crate::types::classify(&error).hint.unwrap();
        assert!(hint.contains("run capturefab list"), "{hint}");
        let error = open_error("zzzz", &known);
        let details = crate::types::classify(&error).details.unwrap();
        assert!(
            !details["candidates"].as_array().unwrap().is_empty(),
            "{details}"
        );
        assert!(needs_no_discovery("./nope.mp4"));
        assert!(needs_no_discovery("192.168.1.300"));
        assert!(!needs_no_discovery("C920"));
    }

    #[test]
    fn suggestions_never_show_credentials() {
        let known = vec![device(
            "rtsp://admin:secret@10.0.0.5/stream",
            "Door",
            "rtsp://admin:secret@10.0.0.5/stream",
        )];
        let message = selector_not_matched("dor", &known, &[]).to_string();
        assert!(!message.contains("secret"), "{message}");
        let message =
            selector_not_matched("x", &known, &[known[0].clone(), known[0].clone()]).to_string();
        assert!(!message.contains("secret"), "{message}");
    }

    #[test]
    fn malformed_ipv4_is_a_usage_error() {
        let error = validate_selector_shape("192.168.1.300").unwrap_err();
        assert!(
            error.to_string().contains("not a valid IPv4 address"),
            "{error:#}"
        );
        assert_eq!(crate::cli::error_code(&error), ("usage", 2));
        validate_selector_shape("192.168.1.30").unwrap();
    }

    #[test]
    fn malformed_selectors_fail_before_opening_a_backend() {
        for selector in [
            "",
            "  ",
            "sim:",
            "sim:   ",
            "sim:a\0b",
            "sim:a\nb",
            "gige:",
            "gige:not-an-ip",
            "gige:gige:127.0.0.1",
            "scanner:unknown:device",
        ] {
            assert!(validate_selector(selector).is_err(), "{selector:?}");
            assert!(Camera::open(selector, &[], Duration::from_millis(1)).is_err());
        }
        assert!(Camera::open("sim:named camera", &[], Duration::from_millis(1)).is_ok());
        for selector in ["gige:00305311aabb", "gige:00305311AABB", "gige:192.0.2.1"] {
            validate_selector_shape(selector).unwrap();
        }
        for selector in [
            "gige:00305311aab",
            "gige:00305311aabbc",
            "gige:00305311aabg",
        ] {
            assert!(validate_selector_shape(selector).is_err(), "{selector:?}");
        }
    }

    #[test]
    fn selector_shape_does_not_depend_on_local_availability() {
        let source = format!(
            "file:/private/tmp/capturefab-missing-{}-{}.mp4",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        validate_selector_shape(&source).unwrap();
        assert!(validate_selector(&source).is_err());
    }

    #[test]
    #[ignore = "requires FFmpeg"]
    fn discovered_media_opens_by_id_and_serial() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "capturefab-camera-{}-{nonce}.ppm",
            std::process::id()
        ));
        let mut ppm = b"P6\n16 16\n255\n".to_vec();
        ppm.extend(vec![127; 16 * 16 * 3]);
        std::fs::write(&path, ppm).unwrap();
        let info = crate::media::info(path.to_str().unwrap()).unwrap();
        for selector in [&info.id, &info.serial] {
            let mut camera = Camera::open(
                selector,
                std::slice::from_ref(&info),
                Duration::from_secs(3),
            )
            .unwrap();
            camera.start().unwrap();
            let frame = camera.next_frame(Duration::from_secs(3)).unwrap();
            assert_eq!((frame.width, frame.height), (16, 16));
            assert_eq!(frame.data, vec![127; 16 * 16 * 3]);
            camera.stop().unwrap();
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn discovered_onvif_serial_uses_onvif_transport() {
        let info = CameraInfo {
            id: "onvif:https://camera/service".into(),
            transport: Transport::Media,
            vendor: "ONVIF".into(),
            model: "Network camera".into(),
            serial: "network-serial".into(),
            address: Some("https://camera/service".into()),
            profile: Default::default(),
        };
        let error = Camera::open(
            &info.serial,
            std::slice::from_ref(&info),
            Duration::from_millis(100),
        )
        .err()
        .expect("HTTPS SOAP should be rejected before any network request");
        assert!(error.to_string().contains("HTTPS ONVIF SOAP"), "{error:#}");
    }
}
