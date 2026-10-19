//! Camera hubs: saved Home Assistant, Frigate, go2rtc and UniFi Protect
//! servers whose cameras open as `hub:NAME/CAMERA`. The saved settings
//! (`hubs.json` in the state directory) never hold secrets; tokens and
//! passwords come from the OS credential store or a `CAPTUREFAB_HUB_*`
//! environment variable in the process that opens or lists the camera.
//!
//! A selector names a saved hub and one of its cameras, never a host, path or
//! credential, so clients of a session cannot point a saved secret at another
//! server. Selector parsing and the saved settings are available in every
//! build; the network clients need the `hubs` feature.
#[cfg(feature = "hubs")]
pub(crate) mod client;
pub mod command;
#[cfg(feature = "hubs")]
pub(crate) mod discovery;
#[cfg(all(test, feature = "hubs"))]
#[path = "../../tests/support/fake_http.rs"]
pub(crate) mod fake_http;
#[cfg(all(test, feature = "hubs"))]
#[path = "../../tests/support/fake_hub.rs"]
pub(crate) mod fake_hub;
#[cfg(feature = "hubs")]
pub(crate) mod frigate;
#[cfg(feature = "hubs")]
pub(crate) mod go2rtc;
#[cfg(feature = "hubs")]
pub(crate) mod homeassistant;
#[cfg(feature = "hubs")]
pub(crate) mod http;
#[cfg(feature = "hubs")]
pub(crate) mod image;
#[cfg(feature = "hubs")]
pub(crate) mod multipart;
#[cfg(feature = "hubs")]
pub(crate) mod pump;
pub mod registry;
pub mod scrub;
pub mod secret;
#[cfg(feature = "hubs")]
pub(crate) mod sources;
#[cfg(feature = "hubs")]
pub(crate) mod tls;
#[cfg(feature = "hubs")]
pub(crate) mod unifi;

use anyhow::Result;
use serde::{Deserialize, Serialize};

pub use registry::{Hub, Kind, SecretSource, Store};

const MAX_SELECTOR: usize = 1024;
const MAX_CAMERA: usize = 256;
const SHAPE: &str = "hub selectors look like hub:NAME/CAMERA[?stream=MODE&fps=F]; encode other characters in CAMERA as %XX";

/// Whether this build can contact hubs.
pub fn compiled() -> bool {
    cfg!(feature = "hubs")
}

/// Whether a camera selector names a hub camera (`hub:` in any case).
pub fn is_source(selector: &str) -> bool {
    selector
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("hub:"))
}

/// How frames are read from a hub camera.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    /// The best stream this kind of hub offers, falling back in order.
    #[default]
    Auto,
    /// A `multipart/x-mixed-replace` JPEG stream.
    Mjpeg,
    /// Still images polled at `fps`.
    Snapshot,
    /// The hub's RTSP restream, decoded by FFmpeg.
    Rtsp,
}

impl Stream {
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Mjpeg => "mjpeg",
            Self::Snapshot => "snapshot",
            Self::Rtsp => "rtsp",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        [Self::Auto, Self::Mjpeg, Self::Snapshot, Self::Rtsp]
            .into_iter()
            .find(|stream| stream.name() == value)
    }
}

/// A parsed `hub:NAME/CAMERA[?stream=MODE&fps=F]` selector.
#[derive(Debug, Clone, PartialEq)]
pub struct Selector {
    pub hub: String,
    /// The camera identifier, percent-decoded.
    pub camera: String,
    pub stream: Stream,
    pub fps: Option<f64>,
}

fn shape(message: impl Into<String>) -> anyhow::Error {
    crate::dataset::Usage(message.into()).into()
}

/// Percent-encode everything except RFC 3986 unreserved characters, with
/// uppercase hexadecimal.
pub fn encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn decode(text: &str) -> Option<String> {
    let input = text.as_bytes();
    let mut bytes = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        let byte = input[index];
        if byte == b'%' {
            let hex = std::str::from_utf8(input.get(index + 1..index + 3)?).ok()?;
            if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            bytes.push(byte);
            index += 1;
        } else {
            return None;
        }
    }
    String::from_utf8(bytes).ok()
}

/// A decimal number of frames per second with at most three fraction digits.
fn parse_fps(value: &str) -> Option<f64> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty()
        || !digits(whole)
        || !digits(fraction)
        || fraction.len() > 3
        || (value.contains('.') && fraction.is_empty())
    {
        return None;
    }
    value.parse().ok()
}

impl Selector {
    pub fn parse(selector: &str) -> Result<Self> {
        if !is_source(selector)
            || selector.len() > MAX_SELECTOR
            || selector.chars().any(char::is_control)
        {
            return Err(shape(SHAPE));
        }
        let rest = &selector[4..];
        let (path, query) = match rest.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (rest, None),
        };
        let (hub, camera) = path.split_once('/').ok_or_else(|| shape(SHAPE))?;
        registry::validate_name(hub)?;
        let camera = decode(camera)
            .filter(|camera| {
                (1..=MAX_CAMERA).contains(&camera.len())
                    && !camera.chars().any(char::is_control)
                    && !camera.contains("://")
            })
            .ok_or_else(|| shape(SHAPE))?;
        let mut stream = None;
        let mut fps = None;
        for option in query.into_iter().flat_map(|query| query.split('&')) {
            let (key, value) = option
                .split_once('=')
                .filter(|(_, value)| !value.is_empty())
                .ok_or_else(|| shape(SHAPE))?;
            match key {
                "stream" => {
                    if stream.is_some() {
                        return Err(shape("duplicate hub selector option stream"));
                    }
                    stream = Some(Stream::parse(value).ok_or_else(|| {
                        shape("hub stream must be auto, mjpeg, snapshot or rtsp")
                    })?);
                }
                "fps" => {
                    if fps.is_some() {
                        return Err(shape("duplicate hub selector option fps"));
                    }
                    fps = Some(
                        parse_fps(value)
                            .filter(|fps| (0.01..=30.0).contains(fps))
                            .ok_or_else(|| shape("fps must be 0.01..30"))?,
                    );
                }
                _ => {
                    return Err(shape(format!(
                        "unknown hub selector option {}; use stream= or fps=",
                        key.chars().take(32).collect::<String>()
                    )));
                }
            }
        }
        Ok(Self {
            hub: hub.into(),
            camera,
            stream: stream.unwrap_or_default(),
            fps,
        })
    }
    /// The camera's ID: stable across connections, without options.
    pub fn id(&self) -> String {
        format!("hub:{}/{}", self.hub, encode(&self.camera))
    }
    /// The selector with every non-default option, in a fixed order.
    pub fn address(&self) -> String {
        let mut options = Vec::new();
        if self.stream != Stream::Auto {
            options.push(format!("stream={}", self.stream.name()));
        }
        if let Some(fps) = self.fps {
            options.push(format!("fps={}", registry::format_number(fps)));
        }
        if options.is_empty() {
            self.id()
        } else {
            format!("{}?{}", self.id(), options.join("&"))
        }
    }
    /// A unique, readable serial for file names: `NAME/CAMERA`.
    pub fn serial(&self) -> String {
        format!("{}/{}", self.hub, encode(&self.camera))
    }
}

/// Check selector syntax without reading saved hubs or the network.
pub fn validate_shape(selector: &str) -> Result<()> {
    Selector::parse(selector).map(|_| ())
}

/// Check a selector against the saved hubs: the hub exists, this build can
/// reach it, and its camera and options suit its kind.
pub fn validate(selector: &str) -> Result<()> {
    resolve(selector).map(|_| ())
}

/// The saved hub and canonical selector for `selector`.
pub fn resolve(selector: &str) -> Result<(Hub, Selector)> {
    resolve_in(&Store::session(), selector)
}

pub(crate) fn resolve_in(store: &Store, selector: &str) -> Result<(Hub, Selector)> {
    let mut parsed = Selector::parse(selector)?;
    if !compiled() {
        return Err(registry::unsupported());
    }
    let hub = store.get(&parsed.hub)?;
    parsed.camera = hub.kind.validate_camera(&parsed.camera, &hub.name)?;
    hub.kind.validate_options(parsed.stream, parsed.fps)?;
    Ok((hub, parsed))
}

/// The `hubs` section of `capturefab schema`.
pub fn schema_contract() -> serde_json::Value {
    let kinds = Kind::ALL
        .iter()
        .map(|kind| {
            let (min, max, default) = kind.snapshot_fps();
            (
                kind.id().to_owned(),
                serde_json::json!({
                    "label": kind.label(),
                    "add": format!("capturefab hub {} NAME --url URL ...", kind.command()),
                    "camera": kind.camera_example(),
                    "secret": kind.secret_noun(),
                    "streams": {
                        "auto": kind.auto_order().iter().map(|s| s.name()).collect::<Vec<_>>(),
                        "explicit": kind.streams().iter().map(|s| s.name()).collect::<Vec<_>>(),
                    },
                    "snapshot_fps": {"min": min, "max": max, "default": default},
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    serde_json::json!({
        "compiled": compiled(),
        "keychain": cfg!(feature = "keychain"),
        "selector": "hub:NAME/CAMERA[?stream=auto|mjpeg|snapshot|rtsp&fps=F]",
        "id": "hub:NAME/CAMERA; options are not part of the ID (the camera's address keeps them)",
        "kinds": kinds,
        "secrets": {
            "keychain": format!("OS credential store, service {}", registry::KEYCHAIN_SERVICE),
            "environment": "a CAPTUREFAB_HUB_* variable in the process that opens or lists the camera (a GUI or serve session, or this CLI)",
            "argv": "never: --secret-stdin or --secret-env VAR",
        },
        "remote": "serve --listen refuses hub cameras unless started with --allow-hubs",
        "results": {
            "list": {"hubs": "[record]"},
            "add": {"hub": "record", "replaced": "bool", "check": "{ok, product, version, name, cameras, warnings} unless --no-check", "certificate": "{fingerprint, trust} for pinned hubs", "warnings": "[string]"},
            "test": {"ok": true, "hub": "NAME", "kind": "KIND", "product": "string", "version": "string|null", "name": "string|null", "cameras": "count", "warnings": "[string]"},
            "cameras": {"cameras": "[{id, hub, kind, camera, name, state, online, streams}]", "warnings": "[string]"},
            "remove": {"removed": "NAME", "secret_forgotten": "bool"},
            "record": "{name, kind, url, secret: keychain|environment|none, insecure, user?, secret_env?, certificate?, tls_server_name?, rtsp_port?, rejected?: {at, status}, secret_saved?}",
        },
        "errors": {
            "1": "secret rejected, certificate refused, or the hub answered unexpectedly",
            "2": "invalid name, URL, flags or selector",
            "3": "hub, camera or server unavailable or offline",
            "4": "timed out",
            "5": "hubs or the OS credential store are not compiled in",
        },
    })
}

/// Cameras on saved hubs, for the session coordinator's discovery.
pub(crate) fn discovered(
    timeout: std::time::Duration,
) -> (Vec<crate::types::CameraInfo>, Vec<String>) {
    #[cfg(feature = "hubs")]
    {
        discovery::discovered(timeout)
    }
    #[cfg(not(feature = "hubs"))]
    {
        let _ = timeout;
        (Vec::new(), Vec::new())
    }
}

/// Open a hub camera. The hub's record and secret are read here, in the
/// camera worker that owns the camera.
#[cfg(feature = "hubs")]
pub fn connect(
    selector: &str,
    timeout: std::time::Duration,
) -> Result<(crate::types::CameraInfo, Box<dyn crate::types::Backend>)> {
    let (hub, selector) = resolve(selector)?;
    let store = Store::session();
    let digest = hub.digest();
    let latch = |failure: &pump::Failure| {
        if let Some(status) = failure.auth_status {
            let _ = store.mark_rejected(&hub.name, &digest, status);
        }
    };
    let secret = secret::resolve(&hub)?;
    if !client::public_identifier(&selector.camera) {
        return Err(crate::types::Categorized::error(
            crate::types::ErrorKind::OperationFailed,
            "the hub camera identifier contains a secret",
        ));
    }
    let plan = client::plan(&hub, secret, &selector).map_err(|failure| {
        latch(&failure);
        failure.into_error()
    })?;
    let meta = pump::Meta {
        id: selector.id(),
        product: hub.kind.label(),
        latch: Some((store.clone(), hub.name.clone(), digest)),
    };
    let backend = pump::HubBackend::connect(plan.sources, timeout, meta)?;
    let info = crate::types::CameraInfo {
        id: selector.id(),
        transport: crate::types::Transport::Media,
        vendor: hub.kind.label().into(),
        model: plan.name,
        serial: selector.serial(),
        address: Some(selector.address()),
        profile: Default::default(),
    };
    Ok((info, Box::new(backend)))
}

/// Without the `hubs` feature, hub cameras cannot be opened.
#[cfg(not(feature = "hubs"))]
pub fn connect(
    selector: &str,
    _timeout: std::time::Duration,
) -> Result<(crate::types::CameraInfo, Box<dyn crate::types::Backend>)> {
    resolve(selector)?;
    Err(registry::unsupported())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors_round_trip_through_their_canonical_forms() {
        let s = Selector::parse("HUB:home/camera.front_door").unwrap();
        assert_eq!(s.hub, "home");
        assert_eq!(s.camera, "camera.front_door");
        assert_eq!((s.stream, s.fps), (Stream::Auto, None));
        assert_eq!(s.id(), "hub:home/camera.front_door");
        assert_eq!(s.address(), s.id());
        assert_eq!(s.serial(), "home/camera.front_door");
        let s = Selector::parse("hub:g/front%20door%2fleft?fps=0.5&stream=snapshot").unwrap();
        assert_eq!(s.camera, "front door/left");
        assert_eq!(s.id(), "hub:g/front%20door%2Fleft");
        assert_eq!(
            s.address(),
            "hub:g/front%20door%2Fleft?stream=snapshot&fps=0.5"
        );
        assert_eq!(Selector::parse(&s.address()).unwrap(), s);
        let s = Selector::parse("hub:nvr/back?stream=mjpeg&fps=2.000").unwrap();
        assert_eq!(s.address(), "hub:nvr/back?stream=mjpeg&fps=2");
        assert!(is_source("Hub:x") && !is_source("hu") && !is_source("rtsp://hub:x"));
    }

    #[test]
    fn malformed_selectors_are_usage_errors() {
        let long = format!("hub:h/{}", "a".repeat(1100));
        for bad in [
            "hub:home",
            "hub:home/",
            "hub:/camera.x",
            "hub:.home/camera.x",
            "hub:ho me/camera.x",
            "hub:home/camera x",
            "hub:home/a/b",
            "hub:home/a#b",
            "hub:home/%zz",
            "hub:home/%4",
            "hub:home/%0D%0AHost:%20evil",
            "hub:home/%00",
            "hub:home/rtsp%3A%2F%2Fevil",
            "hub:home/%FF",
            "hub:home/x?stream=hls",
            "hub:home/x?stream=",
            "hub:home/x?stream",
            "hub:home/x?stream=mjpeg&stream=snapshot",
            "hub:home/x?fps=0",
            "hub:home/x?fps=31",
            "hub:home/x?fps=1e3",
            "hub:home/x?fps=.5",
            "hub:home/x?fps=1.",
            "hub:home/x?fps=0.0005",
            "hub:home/x?fps=1&fps=2",
            "hub:home/x?host=evil",
            "hub:home/x?",
            long.as_str(),
        ] {
            let error = Selector::parse(bad).unwrap_err();
            assert!(
                error.chain().any(|c| c.is::<crate::dataset::Usage>()),
                "{bad}: {error:#}"
            );
            assert_eq!(crate::cli::error_code(&error), ("usage", 2), "{bad}");
        }
        assert!(Selector::parse(&format!("hub:h/{}", "a".repeat(256))).is_ok());
        assert!(Selector::parse(&format!("hub:h/{}", "a".repeat(257))).is_err());
    }

    #[test]
    fn validation_reads_saved_hubs_and_applies_kind_rules() {
        let dir = std::env::temp_dir().join(format!(
            "capturefab-hub-selectors-{}-{}",
            std::process::id(),
            crate::scheduling::now_ms()
        ));
        let store = Store::at(&dir);
        let missing = resolve_in(&store, "hub:home/camera.x").unwrap_err();
        if !compiled() {
            assert_eq!(crate::cli::error_code(&missing), ("unsupported", 5));
            return;
        }
        assert_eq!(crate::cli::error_code(&missing), ("unavailable", 3));
        store
            .save_with(
                &Hub {
                    name: "home".into(),
                    kind: Kind::HomeAssistant,
                    url: "http://ha:8123".into(),
                    user: None,
                    secret: SecretSource::Environment,
                    secret_env: Some("CAPTUREFAB_HUB_T".into()),
                    certificate: None,
                    tls_server_name: None,
                    rtsp_port: None,
                    rejected: None,
                    revision: None,
                },
                |_| Ok(()),
                |_| Ok(()),
            )
            .unwrap();
        let (hub, selector) =
            resolve_in(&store, "hub:home/camera.x?stream=snapshot&fps=2").unwrap();
        assert_eq!((hub.name.as_str(), selector.fps), ("home", Some(2.0)));
        for (bad, code) in [
            ("hub:home/light.x", "usage"),
            ("hub:home/camera.x?stream=rtsp", "usage"),
            ("hub:home/camera.x?fps=1", "usage"),
            ("hub:nvr/camera.x", "unavailable"),
        ] {
            let error = resolve_in(&store, bad).unwrap_err();
            assert_eq!(crate::cli::error_code(&error).0, code, "{bad}: {error:#}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
