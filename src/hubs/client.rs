//! What every kind of hub client provides — a check, a camera list and the
//! sources for one camera — and the HTTP status handling they share.
use super::{
    Hub, Kind, Selector, Stream,
    http::{self, Auth, Endpoint, Limits, Request, Response},
    pump::{Cx, Failure, SourceFactory},
    secret::Secret,
    sources::MediaRequest,
};
use crate::types::ErrorKind;
use serde::Serialize;
use std::{sync::Arc, time::Duration};

/// The result of checking a hub.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Check {
    pub product: &'static str,
    pub version: Option<String>,
    /// The name the server gives itself (Home Assistant's location name).
    pub name: Option<String>,
    pub cameras: usize,
    pub warnings: Vec<String>,
}

/// One camera a hub lists.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Listed {
    pub camera: String,
    pub name: String,
    pub state: Option<String>,
    pub online: bool,
    pub streams: Vec<Stream>,
}

/// The sources to try for one camera, and its display name.
pub(crate) struct Plan {
    pub sources: Vec<Arc<dyn SourceFactory>>,
    pub name: String,
}

/// A request's limits, raised to at least `floor` (`--timeout-ms` for the
/// management commands).
pub(crate) fn limits(
    headers: u64,
    idle: u64,
    total: u64,
    max_body: u64,
    floor: Duration,
) -> Limits {
    Limits {
        headers: Duration::from_secs(headers),
        idle: Duration::from_secs(idle),
        total: Some(Duration::from_secs(total)),
        max_body: Some(max_body),
    }
    .at_least(floor)
}

/// Text a server returned that is safe to show: printable, short, and
/// stripped of anything that could hold a secret.
pub(crate) fn display(text: &str, max: usize) -> String {
    // A server can reflect a token in its name, version or camera labels.
    // Scrub before truncating, which could otherwise cut a secret into a
    // fragment the literal scrubber no longer recognizes.
    super::scrub::scrub(text)
        .chars()
        .filter(|c| !c.is_control())
        .take(max)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Identifiers are echoed in selectors, files and process boundaries. A
/// server must not turn a reflected credential into one; unlike a display
/// label, an identifier cannot be redacted and remain usable.
pub(crate) fn public_identifier(text: &str) -> bool {
    matches!(super::scrub::scrub(text), std::borrow::Cow::Borrowed(_))
}

/// `Retry-After` given in seconds.
pub(crate) fn retry_after(response: &Response) -> Option<Duration> {
    response
        .header("retry-after")?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// What a request was for, which decides how a status is reported.
#[derive(Clone, Copy)]
pub(crate) enum Route<'a> {
    /// A hub API route: the camera list, the version.
    Api(&'static str),
    /// A camera's media route.
    Camera(&'a str, &'static str),
}

impl Route<'_> {
    fn what(&self) -> &'static str {
        match self {
            Route::Api(what) | Route::Camera(_, what) => what,
        }
    }
}

/// Map a non-success status to a failure with fixed wording: no reason
/// phrase and no body. `refused` gives the kind's own 401 message.
pub(crate) fn status_failure(
    hub: &Hub,
    response: &Response,
    route: Route,
    refused: impl FnOnce(u16) -> Failure,
) -> Failure {
    let label = http::label(hub);
    let status = response.status;
    let name = &hub.name;
    match (status, route) {
        (401, _) => refused(401),
        (403, Route::Camera(camera, _)) if hub.kind == Kind::Frigate => Failure::permanent(
            ErrorKind::OperationFailed,
            format!("{label} refused access to camera {camera} (HTTP 403)"),
        ),
        (403, _) => refused(403),
        (404, Route::Camera(camera, _)) => Failure::permanent(
            ErrorKind::Unavailable,
            format!(
                "camera {camera} not found on hub {name}; list cameras with `capturefab hub cameras {name}`"
            ),
        ),
        (404, Route::Api(what)) => Failure::permanent(
            ErrorKind::OperationFailed,
            format!(
                "{label} at {} has no {what} (HTTP 404); check that the URL is the {} server",
                hub.parsed_url().map(|url| url.origin()).unwrap_or_default(),
                hub.kind.label()
            ),
        ),
        (503, Route::Camera(camera, _)) => Failure::transient(
            ErrorKind::Unavailable,
            format!("camera {camera} on hub {name} is offline (HTTP 503)"),
        ),
        (408 | 425 | 429 | 500..=599, route) => Failure {
            retry_after: retry_after(response),
            ..Failure::transient(
                if status == 503 {
                    ErrorKind::Unavailable
                } else {
                    ErrorKind::OperationFailed
                },
                format!(
                    "{label} failed (HTTP {status}) while reading {}",
                    route.what()
                ),
            )
        },
        (_, route) => Failure::permanent(
            ErrorKind::OperationFailed,
            format!(
                "{label} refused the request for {} (HTTP {status})",
                route.what()
            ),
        ),
    }
}

/// Send `request` and return the response only for a 2xx status.
pub(crate) fn send_ok(
    hub: &Hub,
    endpoint: &Endpoint,
    request: &Request,
    auth: &Auth,
    cx: Option<&Cx>,
    route: Route,
    refused: impl FnOnce(u16) -> Failure,
) -> Result<Response, Failure> {
    let response = http::send(endpoint, request, auth, cx.map(|cx| &cx.cancel))?;
    if (200..300).contains(&response.status) {
        Ok(response)
    } else {
        Err(status_failure(hub, &response, route, refused))
    }
}

/// Read a JSON body of at most `max` bytes into `T`, ignoring every field
/// `T` does not name.
pub(crate) fn json<T: serde::de::DeserializeOwned>(
    hub: &Hub,
    response: Response,
    max: usize,
    what: &str,
) -> Result<T, Failure> {
    let bytes = response.body.read_all(max)?;
    serde_json::from_slice(&bytes).map_err(|_| {
        Failure::permanent(
            ErrorKind::OperationFailed,
            format!("{} sent an unreadable {what}", http::label(hub)),
        )
    })
}

pub(crate) fn media(
    route: &'static str,
    target: String,
    accept: &'static str,
    limits: Limits,
) -> MediaRequest {
    MediaRequest {
        route,
        target,
        accept,
        limits,
    }
}

/// Check that a hub answers and accepts its secret.
pub(crate) fn check(hub: &Hub, secret: Option<&Secret>, floor: Duration) -> Result<Check, Failure> {
    match hub.kind {
        Kind::HomeAssistant => super::homeassistant::check(hub, secret, floor),
        Kind::Frigate => super::frigate::check(hub, secret, floor),
        Kind::Go2rtc => super::go2rtc::check(hub, secret, floor),
        Kind::UnifiProtect => super::unifi::check(hub, secret, floor),
    }
}

/// The cameras a hub offers.
pub(crate) fn cameras(
    hub: &Hub,
    secret: Option<&Secret>,
    floor: Duration,
) -> Result<Vec<Listed>, Failure> {
    match hub.kind {
        Kind::HomeAssistant => super::homeassistant::cameras(hub, secret, floor),
        Kind::Frigate => super::frigate::cameras(hub, secret, floor),
        Kind::Go2rtc => super::go2rtc::cameras(hub, secret, floor),
        Kind::UnifiProtect => super::unifi::cameras(hub, secret, floor),
    }
}

/// The sources to try for `selector`, in order.
pub(crate) fn plan(
    hub: &Hub,
    secret: Option<Secret>,
    selector: &Selector,
) -> Result<Plan, Failure> {
    match hub.kind {
        Kind::HomeAssistant => super::homeassistant::plan(hub, secret, selector),
        Kind::Frigate => super::frigate::plan(hub, secret, selector),
        Kind::Go2rtc => super::go2rtc::plan(hub, secret, selector),
        Kind::UnifiProtect => super::unifi::plan(hub, secret, selector),
    }
}

/// The streams to try: the kind's `auto` order, or the one asked for.
pub(crate) fn streams(kind: Kind, stream: Stream) -> Vec<Stream> {
    if stream == Stream::Auto {
        kind.auto_order().to_vec()
    } else {
        vec![stream]
    }
}

/// The snapshot rate for `selector`.
pub(crate) fn snapshot_fps(kind: Kind, selector: &Selector) -> f64 {
    selector.fps.unwrap_or(kind.snapshot_fps().2)
}

/// "camera camera.front_door on hub home", for stream messages.
pub(crate) fn camera_label(hub: &Hub, camera: &str) -> String {
    format!("camera {camera} on hub {}", hub.name)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::hubs::{SecretSource, registry::format_fingerprint};

    /// A hub record pointing at a fake server.
    pub(crate) fn hub(kind: Kind, url: &str, secret: bool) -> Hub {
        Hub {
            name: "test".into(),
            kind,
            url: url.into(),
            user: (secret && matches!(kind, Kind::Frigate | Kind::Go2rtc)).then(|| "viewer".into()),
            secret: if secret {
                SecretSource::Environment
            } else {
                SecretSource::None
            },
            secret_env: secret.then(|| "CAPTUREFAB_HUB_UNIT_TEST".into()),
            certificate: url.starts_with("https://").then(|| {
                format_fingerprint(
                    &crate::hubs::registry::parse_fingerprint(
                        crate::hubs::fake_http::LOCALHOST_SHA256,
                    )
                    .unwrap(),
                )
            }),
            tls_server_name: None,
            rtsp_port: (kind == Kind::Frigate).then_some(8554),
            rejected: None,
            revision: None,
        }
    }
    pub(crate) fn secret(kind: Kind, value: &str) -> Secret {
        Secret::new(kind, value.into()).unwrap()
    }

    #[test]
    fn server_display_text_scrubs_secrets_before_truncating() {
        let secret = secret(Kind::HomeAssistant, "display-test-CANARY-long-token");
        let echoed = format!("Porch {} camera\n", secret.expose());
        assert_eq!(display(&echoed, 128), "Porch <redacted> camera");
        assert_eq!(display(&echoed, 16), "Porch <redacted>");
        assert!(!public_identifier(secret.expose()));
        assert!(public_identifier("front_door"));
    }
}
