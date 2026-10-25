//! The three ways a hub camera delivers frames: an MJPEG stream and polled
//! snapshots, both fetched and decoded in this process, and an RTSP restream
//! decoded by FFmpeg.
use super::{
    Stream,
    http::{HttpError, Limits, Response},
    image,
    multipart::{Multipart, PartError},
    pump::{Cx, Failure, FrameSession, Next, SourceFactory},
};
use crate::types::{Backend, ErrorKind};
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    sync::Arc,
    time::{Duration, Instant},
};

/// The largest still image accepted.
const MAX_IMAGE: usize = super::multipart::MAX_PART;
/// Three damaged parts in a row mean the stream is out of step: reconnect.
const MALFORMED_LIMIT: u32 = 3;
/// An RTSP restream that delivers nothing this long is reopened.
const RTSP_SILENCE: Duration = Duration::from_secs(30);
/// RTSP restreams dial the camera on demand, so their first frame can take a
/// while.
pub(crate) const RTSP_OPEN: Duration = Duration::from_secs(15);

/// A media request for one camera, sent with the hub's credentials.
#[derive(Clone)]
pub(crate) struct MediaRequest {
    pub route: &'static str,
    pub target: String,
    pub accept: &'static str,
    pub limits: Limits,
}

/// Sends media requests the way a hub expects: its authentication, and its
/// status codes mapped to failures (401 refused, 404 missing, 503 offline).
pub(crate) trait Access: Send + Sync {
    fn media(&self, request: &MediaRequest, cx: &Cx) -> Result<Response, Failure>;
}

/// A failure while reading a response body.
pub(crate) fn body_failure(error: &std::io::Error, what: &str) -> Failure {
    match HttpError::from_io(error) {
        Some(http) => Failure::from(http.clone()),
        None => Failure::transient(ErrorKind::OperationFailed, format!("{what} failed")),
    }
}

pub(crate) struct MjpegSource {
    pub access: Arc<dyn Access>,
    pub request: MediaRequest,
    /// What the stream is, for messages: "camera camera.front_door on hub home".
    pub label: String,
    /// Reconnect without counting an attempt when the stream goes idle (Home
    /// Assistant sends nothing while the image is unchanged).
    pub quiet_idle: bool,
}

impl SourceFactory for MjpegSource {
    fn stream(&self) -> Stream {
        Stream::Mjpeg
    }
    fn open(&self, cx: &Cx) -> Result<Box<dyn FrameSession>, Failure> {
        let response = self.access.media(&self.request, cx)?;
        let content_type = response.header("content-type").unwrap_or("").to_owned();
        if !response
            .media_type()
            .is_some_and(|media| media.starts_with("multipart/"))
        {
            return Err(Failure::permanent(
                ErrorKind::OperationFailed,
                format!(
                    "{} answered stream=mjpeg with a single image; use stream=snapshot",
                    self.label
                ),
            ));
        }
        Ok(Box::new(MjpegSession {
            parts: Multipart::new(response.body, &content_type),
            label: self.label.clone(),
            previous: None,
            malformed: 0,
            quiet_idle: self.quiet_idle,
        }))
    }
}

struct MjpegSession {
    parts: Multipart<super::http::Body>,
    label: String,
    /// The length and hash of the last part, to drop repeats (Home Assistant
    /// sends its first frame twice).
    previous: Option<(usize, u64)>,
    malformed: u32,
    quiet_idle: bool,
}

impl FrameSession for MjpegSession {
    fn next(&mut self, _cx: &Cx) -> Result<Next, Failure> {
        match self.parts.next_part() {
            Ok(Some(part)) => {
                self.malformed = 0;
                if part
                    .content_type
                    .as_deref()
                    .is_some_and(|kind| !kind.starts_with("image/"))
                {
                    return Ok(Next::Skip("a part that is not an image"));
                }
                let mut hasher = DefaultHasher::new();
                part.bytes.hash(&mut hasher);
                let key = (part.bytes.len(), hasher.finish());
                if self.previous == Some(key) {
                    return Ok(Next::Skip("a repeated image"));
                }
                self.previous = Some(key);
                Ok(match image::decode(&part.bytes) {
                    Ok(frame) => Next::Frame(frame),
                    Err(reason) => Next::Skip(reason),
                })
            }
            Ok(None) => Err(Failure::transient(
                ErrorKind::OperationFailed,
                format!("{} ended its MJPEG stream", self.label),
            )),
            Err(PartError::Io(error)) => {
                let mut failure = body_failure(&error, &self.label);
                if self.quiet_idle
                    && HttpError::from_io(&error).is_some_and(|http| {
                        matches!(
                            http.problem,
                            super::http::Problem::Timeout {
                                phase: super::http::Phase::Idle,
                                ..
                            }
                        )
                    })
                {
                    failure.quiet = true;
                }
                Err(failure)
            }
            Err(PartError::TooLarge) => Err(Failure::transient(
                ErrorKind::OperationFailed,
                format!("{} sent an MJPEG part over 32 MiB", self.label),
            )),
            Err(PartError::Malformed(what)) => {
                self.malformed += 1;
                if self.malformed >= MALFORMED_LIMIT {
                    Err(Failure::transient(
                        ErrorKind::OperationFailed,
                        format!("{} sent a damaged MJPEG stream ({what})", self.label),
                    ))
                } else {
                    Ok(Next::Skip("a damaged MJPEG part"))
                }
            }
        }
    }
}

pub(crate) struct SnapshotSource {
    pub access: Arc<dyn Access>,
    pub request: MediaRequest,
    pub label: String,
    pub fps: f64,
    /// A response header that marks a placeholder image (Frigate's
    /// `X-Frigate-Offline: true`).
    pub offline_header: Option<&'static str>,
}

impl SourceFactory for SnapshotSource {
    fn stream(&self) -> Stream {
        Stream::Snapshot
    }
    fn fps(&self) -> f64 {
        self.fps
    }
    fn open(&self, _cx: &Cx) -> Result<Box<dyn FrameSession>, Failure> {
        Ok(Box::new(SnapshotSession {
            access: self.access.clone(),
            request: self.request.clone(),
            label: self.label.clone(),
            period: Duration::from_secs_f64(1.0 / self.fps.max(0.01)),
            offline_header: self.offline_header,
            due: Instant::now(),
        }))
    }
}

struct SnapshotSession {
    access: Arc<dyn Access>,
    request: MediaRequest,
    label: String,
    period: Duration,
    offline_header: Option<&'static str>,
    due: Instant,
}

impl FrameSession for SnapshotSession {
    fn continuous(&self) -> bool {
        false
    }
    fn next(&mut self, cx: &Cx) -> Result<Next, Failure> {
        // Requests never overlap: the next starts a period after this one
        // started, or when it finished if it took longer. A capture's request
        // brings it forward.
        cx.wait_until(self.due);
        if cx.cancelled() {
            return Err(Failure::transient(ErrorKind::OperationFailed, "cancelled"));
        }
        cx.begin_snapshot();
        let started = Instant::now();
        let result = (|| {
            let response = self.access.media(&self.request, cx)?;
            let offline = self.offline_header.is_some_and(|name| {
                response
                    .header(name)
                    .is_some_and(|value| value.eq_ignore_ascii_case("true"))
            });
            let bytes = response.body.read_all(MAX_IMAGE).map_err(Failure::from)?;
            if offline {
                return Ok(Next::Skip("the hub reports the camera offline"));
            }
            if bytes.is_empty() {
                return Err(Failure::transient(
                    ErrorKind::OperationFailed,
                    format!("{} returned an empty image", self.label),
                ));
            }
            Ok(match image::decode(&bytes) {
                Ok(frame) => Next::Frame(frame),
                Err(reason) => Next::Skip(reason),
            })
        })();
        self.due = (started + self.period).max(Instant::now());
        result
    }
}

pub(crate) struct RtspSource {
    /// No credentials: hub restreams are opened without a password.
    pub url: String,
    pub host: String,
    pub port: u16,
    pub label: String,
}

impl SourceFactory for RtspSource {
    fn stream(&self) -> Stream {
        Stream::Rtsp
    }
    fn first_frame(&self) -> Duration {
        RTSP_OPEN
    }
    fn open(&self, cx: &Cx) -> Result<Box<dyn FrameSession>, Failure> {
        if cx.cancelled() {
            return Err(Failure::transient(ErrorKind::OperationFailed, "cancelled"));
        }
        let unreachable = || {
            Failure::transient(
                ErrorKind::Unavailable,
                format!(
                    "the RTSP restream of {} at {}:{} is not reachable; use stream=snapshot",
                    self.label, self.host, self.port
                ),
            )
        };
        // A quick check first, so a missing restream does not wait out
        // FFmpeg's timeout.
        // DNS and all resolved addresses share one budget. A synchronous
        // getaddrinfo here could otherwise outlive the first-frame deadline.
        let deadline = Instant::now() + Duration::from_secs(1);
        let addrs =
            super::http::resolve(&self.host, self.port, deadline).map_err(|_| unreachable())?;
        let mut reachable = false;
        for addr in addrs {
            if cx.cancelled() {
                return Err(Failure::transient(ErrorKind::OperationFailed, "cancelled"));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            if std::net::TcpStream::connect_timeout(&addr, left).is_ok() {
                reachable = true;
                break;
            }
        }
        if !reachable {
            return Err(unreachable());
        }
        if cx.cancelled() {
            return Err(Failure::transient(ErrorKind::OperationFailed, "cancelled"));
        }
        let mut backend = crate::media::open_url(&self.url, RTSP_OPEN).map_err(|error| {
            let missing = error.downcast_ref::<crate::media::FfmpegUnavailable>().is_some();
            Failure::transient(
                ErrorKind::OperationFailed,
                format!(
                    "the RTSP restream of {} failed; use stream=snapshot (RTSP needs {} and a restream without a password)",
                    self.label,
                    if missing { "FFmpeg, which this build cannot find," } else { "FFmpeg" }
                ),
            )
        })?;
        backend.start(0).map_err(|_| {
            Failure::transient(
                ErrorKind::OperationFailed,
                format!("the RTSP restream of {} could not start", self.label),
            )
        })?;
        Ok(Box::new(RtspSession {
            backend,
            label: self.label.clone(),
            last: Instant::now(),
        }))
    }
}

struct RtspSession {
    backend: Box<dyn Backend>,
    label: String,
    last: Instant,
}

#[cfg(all(test, unix))]
pub(crate) fn rtsp_session_for_test(backend: Box<dyn Backend>, label: &str) -> impl FrameSession {
    RtspSession {
        backend,
        label: label.into(),
        last: Instant::now(),
    }
}

impl FrameSession for RtspSession {
    fn next(&mut self, cx: &Cx) -> Result<Next, Failure> {
        loop {
            if cx.cancelled() {
                return Err(Failure::transient(ErrorKind::OperationFailed, "cancelled"));
            }
            match self.backend.next_frame(Duration::from_millis(200)) {
                Ok(frame) => {
                    self.last = Instant::now();
                    return Ok(Next::Frame(frame));
                }
                Err(error)
                    if crate::media::stream_end(&error).is_some()
                        || error
                            .chain()
                            .any(|cause| cause.is::<crate::media::StreamEnded>()) =>
                {
                    // MediaBackend reports network decoder exits as a
                    // stream end. Hand that failure to the pump, which
                    // owns hub cancellation and reconnect backoff, before
                    // MediaBackend runs its own reconnect loop.
                    return Err(Failure::transient(
                        ErrorKind::OperationFailed,
                        format!("the RTSP restream of {} ended", self.label),
                    ));
                }
                Err(error) if format!("{error:#}").contains("timed out") => {
                    if self.last.elapsed() > RTSP_SILENCE {
                        return Err(Failure::transient(
                            ErrorKind::Timeout,
                            format!(
                                "the RTSP restream of {} sent no frame for 30 s (timed out)",
                                self.label
                            ),
                        ));
                    }
                }
                Err(_) => {
                    return Err(Failure::transient(
                        ErrorKind::OperationFailed,
                        format!("the RTSP restream of {} failed", self.label),
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn a_cancelled_rtsp_open_never_resolves_or_starts_ffmpeg() {
        let source = RtspSource {
            url: "rtsp://should-not-resolve.invalid:8554/live".into(),
            host: "should-not-resolve.invalid".into(),
            port: 8554,
            label: "camera live on hub test".into(),
        };
        let cx = Cx::new();
        cx.cancel.cancel();
        let started = Instant::now();
        let Err(failure) = source.open(&cx) else {
            panic!("cancelled before DNS");
        };
        assert_eq!(failure.message, "cancelled");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// Only an FFmpeg that could not start is called missing: an explained
    /// source failure quotes FFmpeg's log, which names FFmpeg too.
    #[test]
    #[cfg(unix)]
    fn a_refused_restream_does_not_say_ffmpeg_is_missing() {
        use std::os::unix::fs::PermissionsExt;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let dir =
            std::env::temp_dir().join(format!("capturefab-hub-rtsp-{}-{port}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ffmpeg = dir.join("ffmpeg");
        std::fs::write(
            &ffmpeg,
            format!(
                "#!/bin/sh\necho '[tcp @ 0x1] Connection to tcp://127.0.0.1:{port} failed: Connection refused' >&2\nexit 1\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&ffmpeg, std::fs::Permissions::from_mode(0o700)).unwrap();
        let source = RtspSource {
            url: format!("rtsp://127.0.0.1:{port}/live"),
            host: "127.0.0.1".into(),
            port,
            label: "camera live on hub test".into(),
        };
        let open = |path: PathBuf| {
            crate::media::set_fake_ffmpeg(Some(path));
            let result = source.open(&Cx::new());
            crate::media::set_fake_ffmpeg(None);
            match result {
                Ok(_) => panic!("the restream opened"),
                Err(failure) => failure.message,
            }
        };
        let refused = open(ffmpeg.clone());
        assert!(refused.contains("RTSP needs FFmpeg and"), "{refused}");
        assert!(!refused.contains("cannot find"), "{refused}");
        let missing = open(dir.join("absent"));
        assert!(missing.contains("cannot find"), "{missing}");
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
