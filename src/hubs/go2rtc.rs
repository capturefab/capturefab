//! go2rtc streams. Only stream names come from `/api/streams` (its producer
//! URLs can hold inline passwords and are never parsed), and only those
//! names are ever requested: `frame.jpeg` would otherwise create a new
//! stream for a URL-like name. `/api/config` is never read.
use super::{
    Hub, Kind, Selector, Stream,
    client::{self, Check, Listed, Plan, Route},
    http::{self, Auth, Endpoint, Request, Response},
    pump::{Cx, Failure, SourceFactory},
    secret::Secret,
    sources::{Access, MediaRequest, MjpegSource, RtspSource, SnapshotSource},
};
use crate::types::ErrorKind;
use serde::{Deserialize, de::IgnoredAny};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

fn refused(hub: &Hub) -> impl FnOnce(u16) -> Failure + '_ {
    move |status| {
        let label = http::label(hub);
        Failure::rejected(
            status,
            match (status, &hub.user) {
                (401, None) => format!(
                    "{label} requires a user name and password (HTTP 401); add it again with --user USER --secret-stdin"
                ),
                (401, Some(_)) => format!("{label} rejected the user name or password (HTTP 401)"),
                _ => format!("{label} refused access (HTTP {status})"),
            },
        )
    }
}

struct Go2rtc {
    hub: Hub,
    endpoint: Endpoint,
    password: Option<Secret>,
}

impl Go2rtc {
    fn new(hub: &Hub, password: Option<Secret>) -> Result<Self, Failure> {
        Ok(Self {
            endpoint: Endpoint::for_hub(hub).map_err(|error| {
                Failure::permanent(ErrorKind::OperationFailed, format!("{error:#}"))
            })?,
            hub: hub.clone(),
            password,
        })
    }
    fn auth(&self) -> Auth<'_> {
        match (&self.hub.user, &self.password) {
            (Some(user), Some(password)) => Auth::Basic { user, password },
            _ => Auth::None,
        }
    }
    fn send(&self, request: &Request, cx: Option<&Cx>, route: Route) -> Result<Response, Failure> {
        client::send_ok(
            &self.hub,
            &self.endpoint,
            request,
            &self.auth(),
            cx,
            route,
            refused(&self.hub),
        )
    }
    fn api<T: serde::de::DeserializeOwned>(
        &self,
        what: &'static str,
        target: &str,
        floor: Duration,
    ) -> Result<T, Failure> {
        let response = self.send(
            &Request::get(
                what,
                target.into(),
                "application/json",
                client::limits(10, 10, 30, 16 << 20, floor),
            ),
            None,
            Route::Api(what),
        )?;
        client::json(&self.hub, response, 16 << 20, what)
    }
    fn streams(&self, floor: Duration) -> Result<Vec<String>, Failure> {
        let streams: BTreeMap<String, IgnoredAny> =
            self.api("the stream list", "/api/streams", floor)?;
        Ok(streams.into_keys().collect())
    }
}

#[derive(Deserialize)]
struct Info {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    rtsp: Option<RtspInfo>,
}

#[derive(Deserialize)]
struct RtspInfo {
    #[serde(default)]
    listen: Option<String>,
}

/// The RTSP port, if the server listens where the hub host can reach it:
/// on all interfaces, or on loopback when the hub is on this computer.
fn rtsp_port(listen: Option<&str>, hub_host: &str) -> Option<u16> {
    let listen = listen?.trim();
    let (host, port) = listen.rsplit_once(':')?;
    let port = port.parse::<u16>().ok().filter(|port| *port > 0)?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let anywhere = host.is_empty() || host == "0.0.0.0" || host == "::";
    let loopback = |host: &str| {
        host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    };
    (anywhere || (loopback(host) && loopback(hub_host))).then_some(port)
}

pub(crate) fn check(hub: &Hub, secret: Option<&Secret>, floor: Duration) -> Result<Check, Failure> {
    let go2rtc = Go2rtc::new(hub, secret.map(Secret::duplicate))?;
    let info: Info = go2rtc.api("the server information", "/api", floor)?;
    let streams = go2rtc.streams(floor)?;
    let mut warnings = Vec::new();
    if rtsp_port(
        info.rtsp.as_ref().and_then(|rtsp| rtsp.listen.as_deref()),
        &go2rtc.endpoint.url.host,
    )
    .is_none()
    {
        warnings.push(
            "go2rtc's RTSP server is not reachable from this computer; cameras open as snapshots"
                .into(),
        );
    }
    Ok(Check {
        product: "go2rtc",
        version: info.version.map(|version| client::display(&version, 64)),
        name: None,
        cameras: streams.len(),
        warnings,
    })
}

pub(crate) fn cameras(
    hub: &Hub,
    secret: Option<&Secret>,
    floor: Duration,
) -> Result<Vec<Listed>, Failure> {
    let go2rtc = Go2rtc::new(hub, secret.map(Secret::duplicate))?;
    let info: Info = go2rtc.api("the server information", "/api", floor)?;
    let rtsp = rtsp_port(
        info.rtsp.as_ref().and_then(|rtsp| rtsp.listen.as_deref()),
        &go2rtc.endpoint.url.host,
    )
    .is_some();
    Ok(go2rtc
        .streams(floor)?
        .into_iter()
        .filter(|name| {
            !name.is_empty()
                && name.len() <= 256
                && !name.chars().any(char::is_control)
                && !name.contains("://")
        })
        .map(|name| Listed {
            name: client::display(&name, 128),
            camera: name,
            state: None,
            online: true,
            streams: rtsp
                .then_some(Stream::Rtsp)
                .into_iter()
                .chain([Stream::Mjpeg, Stream::Snapshot])
                .collect(),
        })
        .collect())
}

struct Go2rtcAccess {
    go2rtc: Arc<Go2rtc>,
    camera: String,
}

impl Access for Go2rtcAccess {
    fn media(&self, request: &MediaRequest, cx: &Cx) -> Result<Response, Failure> {
        self.go2rtc.send(
            &Request::get(
                request.route,
                request.target.clone(),
                request.accept,
                request.limits,
            ),
            Some(cx),
            Route::Camera(&self.camera, request.route),
        )
    }
}

pub(crate) fn plan(
    hub: &Hub,
    secret: Option<Secret>,
    selector: &Selector,
) -> Result<Plan, Failure> {
    let go2rtc = Arc::new(Go2rtc::new(hub, secret)?);
    // Only a name the server lists is ever sent back to it.
    if !go2rtc.streams(Duration::ZERO)?.contains(&selector.camera) {
        return Err(Failure::permanent(
            ErrorKind::Unavailable,
            format!(
                "camera {} not found on hub {}; list cameras with `capturefab hub cameras {}`",
                selector.camera, hub.name, hub.name
            ),
        ));
    }
    let info: Info = go2rtc.api("the server information", "/api", Duration::ZERO)?;
    let url = go2rtc.endpoint.url.clone();
    let port = rtsp_port(
        info.rtsp.as_ref().and_then(|rtsp| rtsp.listen.as_deref()),
        &url.host,
    );
    let label = client::camera_label(hub, &selector.camera);
    let source = super::encode(&selector.camera);
    let access = Arc::new(Go2rtcAccess {
        go2rtc: go2rtc.clone(),
        camera: selector.camera.clone(),
    });
    let mut sources: Vec<Arc<dyn SourceFactory>> = Vec::new();
    for stream in client::streams(Kind::Go2rtc, selector.stream) {
        match stream {
            Stream::Rtsp => match port {
                Some(port) => sources.push(Arc::new(RtspSource {
                    url: format!("rtsp://{}:{port}/{source}", url.authority_host()),
                    host: url.host.clone(),
                    port,
                    label: label.clone(),
                })),
                None if selector.stream == Stream::Rtsp => {
                    return Err(Failure::permanent(
                        ErrorKind::Unsupported,
                        format!(
                            "go2rtc's RTSP server is not reachable for {label}; use stream=snapshot"
                        ),
                    ));
                }
                None => {}
            },
            Stream::Mjpeg => sources.push(Arc::new(MjpegSource {
                access: access.clone(),
                request: client::media(
                    "the camera stream",
                    format!("/api/stream.mjpeg?src={source}"),
                    "multipart/x-mixed-replace",
                    http::Limits {
                        headers: Duration::from_secs(15),
                        idle: Duration::from_secs(30),
                        total: None,
                        max_body: None,
                    },
                ),
                label: label.clone(),
                quiet_idle: false,
            })),
            _ => sources.push(Arc::new(SnapshotSource {
                access: access.clone(),
                // go2rtc waits for a keyframe before it answers.
                request: client::media(
                    "the camera image",
                    format!("/api/frame.jpeg?src={source}"),
                    "image/jpeg",
                    client::limits(15, 15, 15, 32 << 20, Duration::ZERO),
                ),
                label: label.clone(),
                fps: client::snapshot_fps(Kind::Go2rtc, selector),
                offline_header: None,
            })),
        }
    }
    Ok(Plan {
        sources,
        name: client::display(&selector.camera, 128),
    })
}

#[cfg(test)]
mod tests;
