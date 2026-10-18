//! Frigate NVR cameras. Port 5000 is Frigate's unauthenticated internal API;
//! port 8971 needs a login, whose JWT session cookie is sent back as a
//! Bearer token and renewed before it expires. Live video comes from
//! Frigate's RTSP restream (go2rtc) when the camera has one, otherwise from
//! `latest.jpg` snapshots. `/api/config` carries unredacted restream
//! credentials, so only the fields named here are ever read from it.
use super::{
    Hub, Kind, Selector, Stream,
    client::{self, Check, Listed, Plan, Route},
    http::{self, Auth, Endpoint, Method, Request, Response},
    pump::{Cx, Failure, SourceFactory},
    scrub,
    secret::Secret,
    sources::{Access, MediaRequest, MjpegSource, RtspSource, SnapshotSource},
};
use crate::types::ErrorKind;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{
    Deserialize, Deserializer,
    de::{IgnoredAny, MapAccess, Visitor},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

/// Log in again this long before the session expires.
const RENEW: u64 = 60;

fn rejected_login(hub: &Hub) -> Failure {
    Failure::rejected(
        401,
        format!(
            "{} rejected the user name or password (HTTP 401)",
            http::label(hub)
        ),
    )
}

fn refused(hub: &Hub) -> impl FnOnce(u16) -> Failure + '_ {
    move |status| {
        if hub.user.is_none() && status == 401 {
            Failure::rejected(
                401,
                format!(
                    "{} requires a sign-in (HTTP 401); add it again with --user USER --secret-stdin",
                    http::label(hub)
                ),
            )
        } else if status == 401 {
            rejected_login(hub)
        } else {
            Failure::rejected(
                status,
                format!("{} refused access (HTTP 403)", http::label(hub)),
            )
        }
    }
}

/// A signed-in session: the JWT and when it expires (seconds since 1970).
struct Session {
    token: Zeroizing<String>,
    expires: u64,
}

/// Talks to one Frigate server, signing in when it has a user.
pub(crate) struct Frigate {
    hub: Hub,
    endpoint: Endpoint,
    password: Option<Secret>,
    session: Mutex<Option<Session>>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_secs())
}

/// The JWT in a `Set-Cookie` value: any cookie whose value has three
/// base64url segments, since the cookie name is configurable.
fn session_cookie(response: &Response) -> Option<Session> {
    response.headers("set-cookie").find_map(|cookie| {
        let value = cookie.split(';').next()?.split_once('=')?.1.trim();
        let mut segments = value.split('.');
        let (_, payload, _) = (segments.next()?, segments.next()?, segments.next()?);
        if segments.next().is_some() {
            return None;
        }
        let claims = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
        #[derive(Deserialize)]
        struct Claims {
            exp: u64,
        }
        let claims: Claims = serde_json::from_slice(&claims).ok()?;
        Some(Session {
            token: Zeroizing::new(value.to_owned()),
            expires: claims.exp,
        })
    })
}

impl Frigate {
    pub(crate) fn new(hub: &Hub, password: Option<Secret>) -> Result<Self, Failure> {
        Ok(Self {
            endpoint: Endpoint::for_hub(hub).map_err(|error| {
                Failure::permanent(ErrorKind::OperationFailed, format!("{error:#}"))
            })?,
            hub: hub.clone(),
            password,
            session: Mutex::new(None),
        })
    }

    /// Sign in; `None` when this port does not use authentication.
    fn login(&self, cx: Option<&Cx>) -> Result<Option<Session>, Failure> {
        let (Some(user), Some(password)) = (&self.hub.user, &self.password) else {
            return Ok(None);
        };
        let body = Zeroizing::new(
            serde_json::to_vec(&serde_json::json!({"user": user, "password": password.expose()}))
                .expect("JSON"),
        );
        let request = Request {
            method: Method::Post,
            route: "the sign-in",
            target: "/api/login".into(),
            accept: "application/json",
            json: Some(&body),
            limits: client::limits(10, 10, 10, 64 << 10, Duration::ZERO),
        };
        let response = http::send(
            &self.endpoint,
            &request,
            &Auth::None,
            cx.map(|cx| &cx.cancel),
        )?;
        match response.status {
            200..=299 => {
                let session = session_cookie(&response).ok_or_else(|| {
                    Failure::permanent(
                        ErrorKind::OperationFailed,
                        format!(
                            "{} signed in without a session token",
                            http::label(&self.hub)
                        ),
                    )
                })?;
                scrub::register(&session.token);
                Ok(Some(session))
            }
            401 => Err(rejected_login(&self.hub)),
            // Authentication is turned off on this port.
            404 => Ok(None),
            _ => Err(client::status_failure(
                &self.hub,
                &response,
                Route::Api("the sign-in"),
                refused(&self.hub),
            )),
        }
    }

    /// The current session token, signing in when there is none or it is
    /// about to expire.
    fn token(&self, cx: Option<&Cx>, renew: bool) -> Result<Option<Zeroizing<String>>, Failure> {
        let mut session = self.session.lock().unwrap_or_else(PoisonError::into_inner);
        let fresh = session
            .as_ref()
            .is_some_and(|current| current.expires > now() + RENEW);
        if renew || !fresh {
            *session = self.login(cx)?;
        }
        Ok(session.as_ref().map(|current| current.token.clone()))
    }

    /// Send `request`, signing in once more if the session was refused.
    fn send(&self, request: &Request, cx: Option<&Cx>, route: Route) -> Result<Response, Failure> {
        let mut renewed = false;
        loop {
            let token = self.token(cx, renewed)?;
            let auth = token
                .as_deref()
                .map_or(Auth::None, |token| Auth::Bearer(token));
            let response = http::send(&self.endpoint, request, &auth, cx.map(|cx| &cx.cancel))?;
            if (200..300).contains(&response.status) {
                return Ok(response);
            }
            if response.status == 401 && self.hub.user.is_some() && !renewed {
                renewed = true;
                continue;
            }
            return Err(client::status_failure(
                &self.hub,
                &response,
                route,
                refused(&self.hub),
            ));
        }
    }

    fn get(
        &self,
        route: Route,
        target: String,
        accept: &'static str,
        limits: http::Limits,
    ) -> Result<Response, Failure> {
        let what = match route {
            Route::Api(what) | Route::Camera(_, what) => what,
        };
        self.send(&Request::get(what, target, accept, limits), None, route)
    }
}

/// `live.streams` in the order Frigate sends it (a map, whose order decides
/// the restream).
#[derive(Default)]
struct Ordered(Vec<(String, String)>);

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Pairs;
        impl<'de> Visitor<'de> for Pairs {
            type Value = Ordered;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a map of stream names")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Ordered, A::Error> {
                let mut pairs = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, serde_json::Value>()? {
                    if let Some(value) = value.as_str() {
                        pairs.push((key, value.to_owned()));
                    }
                }
                Ok(Ordered(pairs))
            }
            fn visit_unit<E>(self) -> Result<Ordered, E> {
                Ok(Ordered::default())
            }
        }
        deserializer.deserialize_any(Pairs)
    }
}

fn enabled() -> bool {
    true
}

#[derive(Deserialize)]
struct Camera {
    #[serde(default = "enabled")]
    enabled: bool,
    #[serde(default)]
    friendly_name: Option<String>,
    #[serde(default)]
    live: Option<Live>,
}

#[derive(Deserialize, Default)]
struct Live {
    #[serde(default)]
    streams: Ordered,
}

#[derive(Deserialize, Default)]
struct Restream {
    #[serde(default)]
    streams: Option<BTreeMap<String, IgnoredAny>>,
}

#[derive(Deserialize)]
struct Config {
    #[serde(default)]
    cameras: BTreeMap<String, Camera>,
    #[serde(default)]
    go2rtc: Option<Restream>,
}

#[derive(Deserialize)]
struct Profile {
    #[serde(default)]
    allowed_cameras: Option<Vec<String>>,
}

impl Config {
    fn restreams(&self) -> Vec<&str> {
        self.go2rtc
            .as_ref()
            .and_then(|go2rtc| go2rtc.streams.as_ref())
            .map(|streams| streams.keys().map(String::as_str).collect())
            .unwrap_or_default()
    }
}

/// The restream for `name`, chosen as Frigate's own Home Assistant
/// integration does: the camera's name if it is one of its live streams and
/// a restream, else its first live stream if that is a restream, else its
/// name if that is a restream.
fn restream<'a>(name: &'a str, camera: &'a Camera, restreams: &[&str]) -> Option<&'a str> {
    let values = camera
        .live
        .as_ref()
        .map(|live| {
            live.streams
                .0
                .iter()
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if values.contains(&name) && restreams.contains(&name) {
        return Some(name);
    }
    if let Some(first) = values.first()
        && restreams.contains(first)
    {
        return Some(first);
    }
    restreams.contains(&name).then_some(name)
}

fn config(frigate: &Frigate, floor: Duration) -> Result<Config, Failure> {
    let response = frigate.get(
        Route::Api("the camera configuration"),
        "/api/config".into(),
        "application/json",
        client::limits(10, 10, 30, 16 << 20, floor),
    )?;
    client::json(&frigate.hub, response, 16 << 20, "camera configuration")
}

/// Cameras the signed-in user may see, when Frigate says (0.17 and later).
fn allowed(frigate: &Frigate, floor: Duration) -> Option<Vec<String>> {
    let response = frigate
        .get(
            Route::Api("the user profile"),
            "/api/profile".into(),
            "application/json",
            client::limits(10, 10, 10, 64 << 10, floor),
        )
        .ok()?;
    client::json::<Profile>(&frigate.hub, response, 64 << 10, "user profile")
        .ok()?
        .allowed_cameras
}

fn list(frigate: &Frigate, floor: Duration) -> Result<(Config, Vec<Listed>), Failure> {
    let config = config(frigate, floor)?;
    let allowed = allowed(frigate, floor);
    let restreams = config.restreams();
    let cameras = config
        .cameras
        .iter()
        .filter(|(name, camera)| {
            camera.enabled
                && Kind::Frigate.validate_camera(name, "").is_ok()
                && allowed
                    .as_ref()
                    .is_none_or(|allowed| allowed.contains(name))
        })
        .map(|(name, camera)| Listed {
            camera: name.clone(),
            name: camera
                .friendly_name
                .as_deref()
                .map(|name| client::display(name, 128))
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| name.clone()),
            state: None,
            online: true,
            streams: restream(name, camera, &restreams)
                .map(|_| Stream::Rtsp)
                .into_iter()
                .chain([Stream::Mjpeg, Stream::Snapshot])
                .collect(),
        })
        .collect();
    Ok((config, cameras))
}

pub(crate) fn check(hub: &Hub, secret: Option<&Secret>, floor: Duration) -> Result<Check, Failure> {
    let frigate = Frigate::new(hub, secret.map(Secret::duplicate))?;
    let response = frigate.get(
        Route::Api("the version"),
        "/api/version".into(),
        "text/plain",
        client::limits(10, 10, 10, 64 << 10, floor),
    )?;
    let version = String::from_utf8_lossy(&response.body.read_all(64 << 10)?).into_owned();
    let (_, cameras) = list(&frigate, floor)?;
    Ok(Check {
        product: "Frigate",
        version: Some(client::display(&version, 64)).filter(|v| !v.is_empty()),
        name: None,
        cameras: cameras.len(),
        warnings: Vec::new(),
    })
}

pub(crate) fn cameras(
    hub: &Hub,
    secret: Option<&Secret>,
    floor: Duration,
) -> Result<Vec<Listed>, Failure> {
    Ok(list(&Frigate::new(hub, secret.map(Secret::duplicate))?, floor)?.1)
}

struct FrigateAccess {
    frigate: Arc<Frigate>,
    camera: String,
}

impl Access for FrigateAccess {
    fn media(&self, request: &MediaRequest, cx: &Cx) -> Result<Response, Failure> {
        self.frigate.send(
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
    let frigate = Arc::new(Frigate::new(hub, secret)?);
    let config = config(&frigate, Duration::ZERO)?;
    let camera = config
        .cameras
        .get(&selector.camera)
        .filter(|camera| camera.enabled)
        .ok_or_else(|| {
            Failure::permanent(
                ErrorKind::Unavailable,
                format!(
                    "camera {} not found on hub {}; list cameras with `capturefab hub cameras {}`",
                    selector.camera, hub.name, hub.name
                ),
            )
        })?;
    let name = camera
        .friendly_name
        .as_deref()
        .map(|name| client::display(name, 128))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| selector.camera.clone());
    let restreams = config.restreams();
    let restream = restream(&selector.camera, camera, &restreams).map(str::to_owned);
    let label = client::camera_label(hub, &selector.camera);
    let access = Arc::new(FrigateAccess {
        frigate: frigate.clone(),
        camera: selector.camera.clone(),
    });
    let url = hub
        .parsed_url()
        .map_err(|error| Failure::permanent(ErrorKind::OperationFailed, format!("{error:#}")))?;
    let encoded = super::encode(&selector.camera);
    let mut sources: Vec<Arc<dyn SourceFactory>> = Vec::new();
    for stream in client::streams(Kind::Frigate, selector.stream) {
        match stream {
            Stream::Rtsp => match &restream {
                Some(restream) => {
                    let port = hub.rtsp_port.unwrap_or(8554);
                    sources.push(Arc::new(RtspSource {
                        url: format!(
                            "rtsp://{}:{port}/{}",
                            url.authority_host(),
                            super::encode(restream)
                        ),
                        host: url.host.clone(),
                        port,
                        label: label.clone(),
                    }));
                }
                None if selector.stream == Stream::Rtsp => {
                    return Err(Failure::permanent(
                        ErrorKind::Unsupported,
                        format!(
                            "{label} has no RTSP restream; use stream=snapshot or stream=mjpeg"
                        ),
                    ));
                }
                None => {}
            },
            Stream::Mjpeg => sources.push(Arc::new(MjpegSource {
                access: access.clone(),
                request: client::media(
                    "the camera stream",
                    format!(
                        "/api/{encoded}?fps={}",
                        selector.fps.map_or(3, |fps| fps as u32)
                    ),
                    "multipart/x-mixed-replace",
                    http::Limits {
                        headers: Duration::from_secs(10),
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
                request: client::media(
                    "the camera image",
                    format!("/api/{encoded}/latest.jpg"),
                    "image/jpeg",
                    client::limits(10, 10, 10, 32 << 20, Duration::ZERO),
                ),
                label: label.clone(),
                fps: client::snapshot_fps(Kind::Frigate, selector),
                offline_header: Some("x-frigate-offline"),
            })),
        }
    }
    Ok(Plan { sources, name })
}

#[cfg(test)]
mod tests;
