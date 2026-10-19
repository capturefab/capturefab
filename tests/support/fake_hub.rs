//! Fake Home Assistant, Frigate, go2rtc and UniFi Protect servers for camera
//! hub tests, built on `fake_http`. They reproduce the protocol details
//! Capturefab depends on, including each server's quirks. Every
//! secret-bearing value they hand out contains `CANARY`, so tests can check
//! that none of them is ever printed or stored.
#![allow(dead_code)]

use super::fake_http::{FakeHttp, Identity, Reply, Request};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// How a fake camera answers its media routes.
#[derive(Clone)]
pub struct Camera {
    pub id: String,
    pub name: String,
    /// 200 to serve images; 404, 500, 503 ... to fail with that status.
    pub status: u16,
}

impl Camera {
    pub fn new(id: &str, name: &str) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            status: 200,
        }
    }
    pub fn status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }
}

/// How the MJPEG stream behaves.
#[derive(Clone)]
pub struct Mjpeg {
    /// Parts sent before the server closes the stream.
    pub parts: usize,
    pub delay: Duration,
    /// After the parts, keep the connection open without sending anything
    /// for this long instead of closing.
    pub stall: Option<Duration>,
}

impl Default for Mjpeg {
    fn default() -> Self {
        Self {
            parts: 50,
            delay: Duration::from_millis(40),
            stall: None,
        }
    }
}

fn take<'a>(frames: &'a [Vec<u8>], counter: &AtomicUsize) -> &'a [u8] {
    &frames[counter.fetch_add(1, Ordering::SeqCst) % frames.len()]
}

fn part(boundary: &str, jpeg: &[u8], length: bool) -> Vec<u8> {
    let mut part = if length {
        format!(
            "{boundary}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
            jpeg.len()
        )
        .into_bytes()
    } else {
        format!("{boundary}\r\nContent-Type: image/jpeg\r\n\r\n").into_bytes()
    };
    part.extend_from_slice(jpeg);
    part.extend_from_slice(if length { b"\r\n" } else { b"\r\n\r\n" });
    part
}

fn serve(
    identity: Option<Identity>,
    handler: impl Fn(&Request) -> Reply + Send + Sync + 'static,
) -> FakeHttp {
    match identity {
        Some(identity) => FakeHttp::start_tls(identity, handler),
        None => FakeHttp::start(handler),
    }
}

// ---------------------------------------------------------------------------
// Home Assistant

pub struct HomeAssistant {
    pub token: String,
    pub cameras: Vec<Camera>,
    pub frames: Vec<Vec<u8>>,
    pub mjpeg: Mjpeg,
    pub identity: Option<Identity>,
}

impl HomeAssistant {
    pub fn new(token: &str, frames: Vec<Vec<u8>>) -> Self {
        Self {
            token: token.into(),
            cameras: vec![
                Camera::new("camera.front_door", "Front door"),
                Camera::new("camera.garage", "Garage"),
            ],
            frames,
            mjpeg: Mjpeg::default(),
            identity: None,
        }
    }
    pub fn start(self) -> FakeHttp {
        let config = Arc::new(self);
        let counter = Arc::new(AtomicUsize::new(0));
        serve(config.identity.clone(), move |request| {
            home_assistant(&config, &counter, request)
        })
    }
}

fn home_assistant(
    config: &Arc<HomeAssistant>,
    counter: &Arc<AtomicUsize>,
    request: &Request,
) -> Reply {
    if request.bearer() != Some(config.token.as_str()) {
        return Reply::text(401, "401: Unauthorized").header("WWW-Authenticate", "Bearer");
    }
    let camera = |entity: &str| config.cameras.iter().find(|camera| camera.id == entity);
    let state = |camera: &Camera| {
        format!(
            r#"{{"entity_id":"{}","state":"{}","attributes":{{"access_token":"CANARY-camera-token-{}","entity_picture":"/api/camera_proxy/{}?token=CANARY-camera-token","friendly_name":"{}","supported_features":2,"brand":"Fake"}},"last_changed":"2026-10-18T00:00:00+00:00"}}"#,
            camera.id,
            if camera.status == 200 {
                "idle"
            } else {
                "unavailable"
            },
            camera.id,
            camera.id,
            camera.name
        )
    };
    match request.path() {
        "/api/" => Reply::json(r#"{"message":"API running."}"#),
        "/api/config" => Reply::json(
            r#"{"version":"2026.10.2","location_name":"Test Home","latitude":52.37,"longitude":4.89,"internal_url":"http://CANARY-internal:8123","components":["camera","stream"]}"#,
        ),
        "/api/states" => {
            let mut states = config.cameras.iter().map(state).collect::<Vec<_>>();
            states.push(r#"{"entity_id":"light.kitchen","state":"on","attributes":{"friendly_name":"Kitchen"}}"#.into());
            states.push(r#"{"entity_id":"camera.Bad Name","state":"idle","attributes":{}}"#.into());
            Reply::json(&format!("[{}]", states.join(",")))
        }
        path => {
            if let Some(entity) = path.strip_prefix("/api/states/") {
                return match camera(entity) {
                    Some(camera) => Reply::json(&state(camera)),
                    None => Reply::new(404)
                        .header("Content-Type", "application/json")
                        .bytes(br#"{"message":"Entity not found."}"#.to_vec()),
                };
            }
            if let Some(entity) = path.strip_prefix("/api/camera_proxy_stream/") {
                let Some(camera) = camera(entity) else {
                    return Reply::new(404);
                };
                if camera.status != 200 {
                    return Reply::new(camera.status);
                }
                let frames = config.frames.clone();
                let mjpeg = config.mjpeg.clone();
                let counter = counter.clone();
                return Reply::new(200)
                    .header(
                        "Content-Type",
                        "multipart/x-mixed-replace; boundary=--frameboundary",
                    )
                    .chunked_stream(Box::new(move |out| {
                        // Home Assistant sends its first frame twice.
                        let first = take(&frames, &counter).to_vec();
                        out.write_all(&part("--frameboundary", &first, true))?;
                        out.write_all(&part("--frameboundary", &first, true))?;
                        for _ in 1..mjpeg.parts {
                            thread::sleep(mjpeg.delay);
                            out.write_all(&part("--frameboundary", take(&frames, &counter), true))?;
                        }
                        if let Some(stall) = mjpeg.stall {
                            thread::sleep(stall);
                        }
                        Ok(())
                    }));
            }
            if let Some(entity) = path.strip_prefix("/api/camera_proxy/") {
                let Some(camera) = camera(entity) else {
                    return Reply::new(404);
                };
                if camera.status != 200 {
                    return Reply::new(camera.status);
                }
                return Reply::image("image/jpeg", take(&config.frames, counter).to_vec());
            }
            Reply::new(404)
        }
    }
}

// ---------------------------------------------------------------------------
// Frigate

pub struct Frigate {
    /// User and password; `None` serves the unauthenticated port.
    pub login: Option<(String, String)>,
    pub cameras: Vec<Camera>,
    /// Restream names of go2rtc streams.
    pub restreams: Vec<String>,
    /// The profile's camera restrictions; `None` allows every camera.
    pub allowed_cameras: Option<Vec<String>>,
    /// Older versions return 404; refusals and server failures must not be ignored.
    pub profile_status: u16,
    pub frames: Vec<Vec<u8>>,
    pub offline: bool,
    /// Seconds a session lasts.
    pub session: u64,
    pub identity: Option<Identity>,
    pub logins: Arc<AtomicUsize>,
}

impl Frigate {
    pub fn new(frames: Vec<Vec<u8>>) -> Self {
        Self {
            login: None,
            cameras: vec![
                Camera::new("driveway", "Driveway"),
                Camera::new("back_yard", "Back yard"),
            ],
            restreams: vec!["driveway".into()],
            allowed_cameras: None,
            profile_status: 200,
            frames,
            offline: false,
            session: 86400,
            identity: None,
            logins: Arc::default(),
        }
    }
    pub fn start(self) -> FakeHttp {
        let config = Arc::new(self);
        let counter = Arc::new(AtomicUsize::new(0));
        serve(config.identity.clone(), move |request| {
            frigate(&config, &counter, request)
        })
    }
}

fn base64url(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// A JWT-shaped session token whose payload holds `exp`.
fn session_token(user: &str, seconds: u64, serial: usize) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!(
        "{}.{}.CANARY-signature-{serial}",
        base64url(br#"{"alg":"HS256","typ":"JWT"}"#),
        base64url(
            format!(
                r#"{{"sub":"{user}","role":"viewer","exp":{}}}"#,
                now + seconds
            )
            .as_bytes()
        )
    )
}

fn frigate(config: &Arc<Frigate>, counter: &Arc<AtomicUsize>, request: &Request) -> Reply {
    if request.path() == "/api/login" && request.method == "POST" {
        let Some((user, password)) = &config.login else {
            return Reply::new(404)
                .header("Content-Type", "application/json")
                .bytes(br#"{"message":"Authentication is disabled"}"#.to_vec());
        };
        if request.header("origin").is_some() {
            return Reply::new(401)
                .bytes(br#"{"success":false,"message":"Missing CSRF header"}"#.to_vec());
        }
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
        if body["user"] != user.as_str() || body["password"] != password.as_str() {
            return Reply::new(401)
                .header("Content-Type", "application/json")
                .bytes(br#"{"message":"Login failed"}"#.to_vec());
        }
        let serial = config.logins.fetch_add(1, Ordering::SeqCst);
        return Reply::new(200).header(
            "Set-Cookie",
            &format!(
                "frigate_token={}; HttpOnly; Path=/; Max-Age={}",
                session_token(user, config.session, serial),
                config.session
            ),
        );
    }
    if let Some((user, _)) = &config.login {
        let current = config.logins.load(Ordering::SeqCst);
        let valid = request.bearer().is_some_and(|token| {
            current > 0
                && token.ends_with(&format!("CANARY-signature-{}", current - 1))
                && token.contains(&base64url(format!(r#"{{"sub":"{user}""#).as_bytes())[..8])
        });
        if !valid {
            return Reply::new(401).bytes(br#"{"message":"Unauthorized"}"#.to_vec());
        }
    }
    let path = request.path();
    match path {
        "/api/version" => Reply::text(200, "0.18.2-test"),
        "/api/profile" if config.profile_status != 200 => Reply::new(config.profile_status),
        "/api/profile" => Reply::json(
            &serde_json::json!({
                "username": "viewer", "role": "viewer",
                "allowed_cameras": config.allowed_cameras.clone().unwrap_or_else(|| {
                    config.cameras.iter().map(|camera| camera.id.clone()).collect()
                }),
            })
            .to_string(),
        ),
        "/api/config" => {
            let cameras = config
                .cameras
                .iter()
                .map(|camera| {
                    format!(
                        r#""{}":{{"name":"{}","friendly_name":"{}","enabled":true,"ffmpeg":{{"inputs":[{{"path":"rtsp://*:*@10.0.0.9/stream","roles":["detect"]}}]}},"detect":{{"height":360,"width":640,"fps":5}},"live":{{"streams":{{"Main":"{}","Sub":"{}_sub"}}}}}}"#,
                        camera.id, camera.id, camera.name, camera.id, camera.id
                    )
                })
                .chain([r#""disabled_cam":{"enabled":false,"live":{"streams":{}}}"#.to_owned()])
                .collect::<Vec<_>>()
                .join(",");
            let restreams = config
                .restreams
                .iter()
                .map(|name| format!(r#""{name}":["rtsp://admin:CANARY-producer@10.0.0.9/{name}"]"#))
                .collect::<Vec<_>>()
                .join(",");
            Reply::json(&format!(
                r#"{{"cameras":{{{cameras}}},"go2rtc":{{"streams":{{{restreams}}},"rtsp":{{"username":"CANARY-rtsp-user","password":"CANARY-rtsp-password"}}}},"environment_vars":{{"FRIGATE_RTSP_PASSWORD":"CANARY-env"}}}}"#
            ))
        }
        _ => {
            let rest = path.strip_prefix("/api/").unwrap_or("");
            let (name, image) = match rest.split_once('/') {
                Some((name, "latest.jpg")) => (name, true),
                Some(_) => return Reply::new(404),
                None => (rest, false),
            };
            let Some(camera) = config.cameras.iter().find(|camera| camera.id == name) else {
                return Reply::new(404)
                    .header("Content-Type", "application/json")
                    .bytes(br#"{"success":false,"message":"Camera not found"}"#.to_vec());
            };
            if camera.status != 200 {
                return Reply::new(camera.status);
            }
            if image {
                let mut reply = Reply::image("image/jpeg", take(&config.frames, counter).to_vec());
                if config.offline {
                    reply = reply.header("X-Frigate-Offline", "true");
                }
                return reply;
            }
            let frames = config.frames.clone();
            let counter = counter.clone();
            Reply::new(200)
                .header("Content-Type", "multipart/x-mixed-replace;boundary=frame")
                .stream(Box::new(move |out| {
                    for _ in 0..50 {
                        out.write_all(&part("--frame", take(&frames, &counter), false))?;
                        thread::sleep(Duration::from_millis(40));
                    }
                    Ok(())
                }))
        }
    }
}

// ---------------------------------------------------------------------------
// go2rtc

pub struct Go2rtc {
    pub basic: Option<(String, String)>,
    pub streams: Vec<String>,
    pub rtsp_listen: String,
    pub frames: Vec<Vec<u8>>,
    /// Answer frame.jpeg with an empty 200, as go2rtc does when it cannot
    /// add a consumer.
    pub empty_frames: bool,
}

impl Go2rtc {
    pub fn new(frames: Vec<Vec<u8>>) -> Self {
        Self {
            basic: None,
            streams: vec!["porch".into(), "garage cam".into()],
            rtsp_listen: ":8554".into(),
            frames,
            empty_frames: false,
        }
    }
    pub fn start(self) -> FakeHttp {
        let config = Arc::new(self);
        let counter = Arc::new(AtomicUsize::new(0));
        FakeHttp::start(move |request| go2rtc(&config, &counter, request))
    }
}

fn basic(user: &str, password: &str) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let data = format!("{user}:{password}").into_bytes();
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    format!("Basic {out}")
}

fn decode_src(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(byte) = value
                .get(index + 1..index + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn go2rtc(config: &Arc<Go2rtc>, counter: &Arc<AtomicUsize>, request: &Request) -> Reply {
    if let Some((user, password)) = &config.basic
        && request.header("authorization") != Some(basic(user, password).as_str())
    {
        return Reply::text(401, "Unauthorized")
            .header("Www-Authenticate", "Basic realm=\"go2rtc\"");
    }
    let stream = || {
        request
            .query("src")
            .map(decode_src)
            .filter(|name| config.streams.contains(name))
    };
    match request.path() {
        "/api" => Reply::json(&format!(
            r#"{{"version":"1.9.14","revision":"test","config_path":"/config/go2rtc.yaml","host":"x","rtsp":{{"listen":"{}","default_query":"video&audio"}}}}"#,
            config.rtsp_listen
        )),
        "/api/streams" => Reply::json(&format!(
            "{{{}}}",
            config
                .streams
                .iter()
                .map(|name| format!(r#""{name}":{{"producers":[{{"url":"rtsp://admin:CANARY-producer-password@10.0.0.7/{name}"}}],"consumers":null}}"#))
                .collect::<Vec<_>>()
                .join(",")
        )),
        "/api/config" => Reply::text(200, "streams:\n  porch: rtsp://admin:CANARY-config@10.0.0.7/\n"),
        "/api/frame.jpeg" => match stream() {
            None => Reply::text(404, "stream not found"),
            Some(_) if config.empty_frames => Reply::new(200).header("Content-Type", "image/jpeg"),
            Some(_) => Reply::image("image/jpeg", take(&config.frames, counter).to_vec()),
        },
        "/api/stream.mjpeg" => match stream() {
            None => Reply::text(404, "stream not found"),
            Some(_) => {
                let frames = config.frames.clone();
                let counter = counter.clone();
                Reply::new(200)
                    .header("Content-Type", "multipart/x-mixed-replace; boundary=frame")
                    .stream(Box::new(move |out| {
                        for _ in 0..50 {
                            out.write_all(&part("--frame", take(&frames, &counter), true))?;
                            thread::sleep(Duration::from_millis(40));
                        }
                        Ok(())
                    }))
            }
        },
        _ => Reply::new(404),
    }
}

// ---------------------------------------------------------------------------
// UniFi Protect

pub struct Protect {
    pub key: String,
    /// id, name, state, full-HD snapshots.
    pub cameras: Vec<(String, String, String, bool)>,
    pub frames: Vec<Vec<u8>>,
    pub identity: Identity,
}

impl Protect {
    pub fn new(key: &str, frames: Vec<Vec<u8>>, identity: Identity) -> Self {
        Self {
            key: key.into(),
            cameras: vec![
                (
                    "66d025b301ebc903e80003ea".into(),
                    "Doorbell".into(),
                    "CONNECTED".into(),
                    true,
                ),
                (
                    "66d025b301ebc903e80003eb".into(),
                    "Shed".into(),
                    "DISCONNECTED".into(),
                    false,
                ),
            ],
            frames,
            identity,
        }
    }
    pub fn start(self) -> FakeHttp {
        let config = Arc::new(self);
        let counter = Arc::new(AtomicUsize::new(0));
        let identity = config.identity.clone();
        FakeHttp::start_tls(identity, move |request| protect(&config, &counter, request))
    }
}

fn protect(config: &Arc<Protect>, counter: &Arc<AtomicUsize>, request: &Request) -> Reply {
    if request.header("x-api-key") != Some(config.key.as_str()) {
        return Reply::new(401)
            .header("Content-Type", "application/json")
            .bytes(br#"{"error":"Unauthorized","name":"UNAUTHORIZED"}"#.to_vec());
    }
    let Some(path) = request.path().strip_prefix("/proxy/protect/integration/v1") else {
        return Reply::new(404);
    };
    let camera = |id: &str| config.cameras.iter().find(|camera| camera.0 == id);
    let json = |camera: &(String, String, String, bool)| {
        format!(
            r#"{{"id":"{}","modelKey":"camera","state":"{}","name":"{}","type":"UVC G4 Doorbell","mac":"24A43C3DFEB9","featureFlags":{{"supportFullHdSnapshot":{},"hasHdr":true}}}}"#,
            camera.0, camera.2, camera.1, camera.3
        )
    };
    match path {
        "/meta/info" => Reply::json(r#"{"applicationVersion":"7.3.70"}"#),
        "/cameras" => Reply::json(&format!(
            "[{}]",
            config
                .cameras
                .iter()
                .map(json)
                .collect::<Vec<_>>()
                .join(",")
        )),
        _ => {
            let rest = path.strip_prefix("/cameras/").unwrap_or("");
            match rest.split_once('/') {
                None => match camera(rest) {
                    Some(camera) => Reply::json(&json(camera)),
                    None => Reply::new(404),
                },
                Some((id, "snapshot")) => match camera(id) {
                    Some(camera) if camera.2 != "CONNECTED" => Reply::new(503),
                    Some(_) => Reply::image("image/jpeg", take(&config.frames, counter).to_vec()),
                    None => Reply::new(404),
                },
                Some(_) => Reply::new(404),
            }
        }
    }
}
