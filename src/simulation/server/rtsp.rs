//! A bounded, video-only RTSP camera endpoint. A single FFmpeg encoder feeds
//! every client; RTP uses RFC 6184 single NAL units and FU-A fragmentation.
use super::LatestFrame;
use anyhow::{Context, Result, anyhow, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{
    collections::{HashMap, VecDeque},
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket},
    process::{Child, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_CLIENTS: usize = 16;
const MAX_QUEUE_BYTES: usize = 16 * 1024 * 1024;
const MAX_QUEUE_FRAMES: usize = 128;
const MAX_REQUEST: usize = 65_539;
const RTP_PAYLOAD: usize = 1_200;
const SESSION_TIMEOUT: Duration = Duration::from_secs(60);

struct AccessUnit {
    sequence: u64,
    timestamp: u32,
    nals: Vec<Vec<u8>>,
    idr: bool,
    bytes: usize,
}

#[derive(Default)]
struct EncodedState {
    queue: VecDeque<Arc<AccessUnit>>,
    bytes: usize,
    sps: Vec<u8>,
    pps: Vec<u8>,
    error: Option<String>,
}

#[derive(Default)]
struct EncodedStream {
    state: Mutex<EncodedState>,
    changed: Condvar,
}

impl EncodedStream {
    fn fail(&self, message: String) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.error.is_none() {
            state.error = Some(message);
        }
        self.changed.notify_all();
    }

    fn publish(&self, nals: Vec<Vec<u8>>, sequence: u64, fps: f64) {
        if !nals.iter().any(|n| matches!(n[0] & 31, 1 | 5)) {
            return;
        }
        let bytes = nals.iter().map(Vec::len).sum();
        if bytes > MAX_QUEUE_BYTES {
            self.fail("RTSP encoder produced an oversized H.264 frame".into());
            return;
        }
        let idr = nals.iter().any(|n| n[0] & 31 == 5);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for nal in &nals {
            match nal[0] & 31 {
                7 => state.sps.clone_from(nal),
                8 => state.pps.clone_from(nal),
                _ => {}
            }
        }
        state.queue.push_back(Arc::new(AccessUnit {
            sequence,
            timestamp: timestamp(sequence, fps),
            nals,
            idr,
            bytes,
        }));
        state.bytes += bytes;
        while state.queue.len() > MAX_QUEUE_FRAMES || state.bytes > MAX_QUEUE_BYTES {
            if let Some(unit) = state.queue.pop_front() {
                state.bytes -= unit.bytes;
            }
        }
        self.changed.notify_all();
    }

    fn sdp(&self, address: Ipv4Addr, width: u32, height: u32, fps: f64) -> Result<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.sps.len() < 4 || state.pps.is_empty() {
            if let Some(error) = &state.error {
                return Err(anyhow!(error.clone()));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "RTSP encoder has not received camera frames"
            );
            state = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        Ok(format!(
            "v=0\r\no=- 1 1 IN IP4 {address}\r\ns=Capturefab simulated camera\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\na=control:*\r\na=range:npt=now-\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=fmtp:96 packetization-mode=1;profile-level-id={:02x}{:02x}{:02x};sprop-parameter-sets={},{}\r\na=framerate:{fps}\r\na=framesize:96 {width}-{height}\r\na=control:trackID=0\r\n",
            state.sps[1],
            state.sps[2],
            state.sps[3],
            STANDARD.encode(&state.sps),
            STANDARD.encode(&state.pps)
        ))
    }

    /// A lagging client resumes only at a keyframe, preserving valid decoding.
    fn after(&self, sequence: u64, waiting_for_idr: bool) -> Option<Arc<AccessUnit>> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let behind = state
            .queue
            .front()
            .is_some_and(|f| f.sequence > sequence + 1);
        state
            .queue
            .iter()
            .find(|f| f.sequence > sequence && (!(waiting_for_idr || behind) || f.idr))
            .cloned()
    }

    fn sequence(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .queue
            .back()
            .map_or(0, |f| f.sequence)
    }
}

pub(crate) struct RtspServer {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Child>>,
    encoded: Arc<EncodedStream>,
    workers: Vec<JoinHandle<()>>,
    // Keep an extracted embedded executable alive until its child has exited.
    _executable: Arc<crate::media::Executable>,
}

impl RtspServer {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        bind: Ipv4Addr,
        port: u16,
        width: u32,
        height: u32,
        fps: f64,
        bitrate: &str,
        frames: Arc<LatestFrame>,
        shutdown: Arc<AtomicBool>,
    ) -> Result<Self> {
        ensure!(
            width > 0 && height > 0 && fps.is_finite() && fps > 0.0,
            "invalid RTSP camera dimensions or frame rate"
        );
        ensure!(
            (width as u64)
                .checked_mul(height as u64)
                .and_then(|pixels| pixels.checked_mul(3))
                .is_some_and(|bytes| bytes <= 256 * 1024 * 1024),
            "RTSP camera frame exceeds 256 MiB"
        );
        crate::media::validate_bitrate(bitrate)?;
        let listener =
            TcpListener::bind((bind, port)).context("cannot bind simulated RTSP camera")?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let executable = crate::media::executable()?;
        let mut command = crate::media::command(&executable);
        // Baseline, no B-frames and repeated SPS/PPS let NVRs join a live stream
        // at every one-second keyframe. Padding also supports odd source sizes.
        command
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-f",
                "rawvideo",
                "-pixel_format",
                "rgb24",
                "-video_size",
                &format!("{width}x{height}"),
                "-framerate",
                &fps.to_string(),
                "-i",
                "pipe:0",
                "-an",
                "-vf",
                "pad=ceil(iw/2)*2:ceil(ih/2)*2",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
                "-profile:v",
                "baseline",
                "-pix_fmt",
                "yuv420p",
                "-b:v",
                bitrate,
                "-g",
                &fps.ceil().to_string(),
                "-keyint_min",
                &fps.ceil().to_string(),
                "-sc_threshold",
                "0",
                "-x264-params",
                "repeat-headers=1:aud=1",
                "-threads",
                "2",
                "-flush_packets",
                "1",
                "-f",
                "h264",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().context("cannot start RTSP H.264 encoder")?;
        let mut stdin = child
            .stdin
            .take()
            .context("RTSP encoder stdin unavailable")?;
        let mut stdout = child
            .stdout
            .take()
            .context("RTSP encoder stdout unavailable")?;
        let mut stderr = child
            .stderr
            .take()
            .context("RTSP encoder stderr unavailable")?;
        let child = Arc::new(Mutex::new(child));
        let stop = Arc::new(AtomicBool::new(false));
        let encoded = Arc::new(EncodedStream::default());
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut workers = Vec::new();
        {
            let log = log.clone();
            workers.push(thread::spawn(move || {
                let mut chunk = [0; 1024];
                while let Ok(count) = stderr.read(&mut chunk) {
                    if count == 0 {
                        break;
                    }
                    let mut log = log.lock().unwrap_or_else(|e| e.into_inner());
                    let room = (16_384usize).saturating_sub(log.len());
                    log.extend_from_slice(&chunk[..count.min(room)]);
                }
            }));
        }
        {
            let stop = stop.clone();
            let shutdown = shutdown.clone();
            let encoded = encoded.clone();
            workers.push(thread::spawn(move || {
                let mut sequence = 0;
                while !stopped(&stop, &shutdown) {
                    let Some((next, frame)) =
                        frames.wait_after(sequence, Duration::from_millis(100))
                    else {
                        continue;
                    };
                    sequence = next;
                    let result = (|| -> Result<()> {
                        ensure!(
                            frame.width == width && frame.height == height,
                            "RTSP camera dimensions changed during playback"
                        );
                        let rgb = crate::frame::rgb(&frame)?;
                        stdin
                            .write_all(&rgb)
                            .context("RTSP encoder stopped accepting frames")?;
                        Ok(())
                    })();
                    if let Err(error) = result {
                        if !stopped(&stop, &shutdown) {
                            encoded.fail(format!("{error:#}"));
                        }
                        break;
                    }
                }
            }));
        }
        {
            let encoded = encoded.clone();
            let stop = stop.clone();
            let shutdown = shutdown.clone();
            workers.push(thread::spawn(move || {
                let mut parser = AnnexB::default();
                let mut chunk = [0; 16_384];
                let mut nals = Vec::new();
                let mut nal_bytes = 0usize;
                let mut sequence = 1;
                loop {
                    let count = match stdout.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(count) => count,
                    };
                    if stopped(&stop, &shutdown) {
                        break;
                    }
                    let result = parser.push(&chunk[..count], |nal| {
                        if nal[0] & 31 == 9 && !nals.is_empty() {
                            encoded.publish(std::mem::take(&mut nals), sequence, fps);
                            sequence += 1;
                            nal_bytes = 0;
                        }
                        nal_bytes += nal.len();
                        nals.push(nal);
                    });
                    if nal_bytes > MAX_QUEUE_BYTES {
                        encoded.fail("RTSP encoder produced an oversized H.264 frame".into());
                        break;
                    }
                    if let Err(error) = result {
                        encoded.fail(error.to_string());
                        break;
                    }
                }
                if !stopped(&stop, &shutdown) {
                    let log = log.lock().unwrap_or_else(|e| e.into_inner());
                    encoded.fail(format!(
                        "RTSP H.264 encoder exited: {}",
                        String::from_utf8_lossy(&log).trim()
                    ));
                }
            }));
        }
        {
            let stop = stop.clone();
            let encoded = encoded.clone();
            workers.push(thread::spawn(move || {
                let mut clients: Vec<JoinHandle<()>> = Vec::new();
                while !stopped(&stop, &shutdown) {
                    let mut index = 0;
                    while index < clients.len() {
                        if clients[index].is_finished() {
                            let _ = clients.swap_remove(index).join();
                        } else {
                            index += 1;
                        }
                    }
                    match listener.accept() {
                        Ok((mut stream, _)) if clients.len() >= MAX_CLIENTS => {
                            let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
                            let _ = response(&mut stream, "0", 503, "Service Unavailable", &[], "");
                        }
                        Ok((stream, _)) => {
                            let stop = stop.clone();
                            let shutdown = shutdown.clone();
                            let encoded = encoded.clone();
                            clients.push(thread::spawn(move || {
                                let _ = serve_client(
                                    stream,
                                    bind,
                                    width + width % 2,
                                    height + height % 2,
                                    fps,
                                    encoded,
                                    stop,
                                    shutdown,
                                );
                            }));
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10))
                        }
                        Err(error) => {
                            encoded.fail(format!("RTSP listener failed: {error}"));
                            break;
                        }
                    }
                }
                for client in clients {
                    let _ = client.join();
                }
            }));
        }
        let server = Self {
            address,
            stop,
            child,
            encoded,
            workers,
            _executable: executable,
        };
        // The source producer is already running. Fail startup concretely if
        // FFmpeg lacks x264 or cannot encode this source, before publishing URLs.
        server
            .encoded
            .sdp(bind, width + width % 2, height + height % 2, fps)?;
        Ok(server)
    }

    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.address
    }

    pub(crate) fn error(&self) -> Option<String> {
        self.encoded
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .error
            .clone()
    }
}

impl Drop for RtspServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.encoded.fail("RTSP camera stopped".into());
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        let _ = child.kill();
        let _ = child.wait();
        drop(child);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

/// The 90 kHz RTP timestamp of an encoded frame.
fn timestamp(sequence: u64, fps: f64) -> u32 {
    (sequence as f64 * 90_000.0 / fps).round() as u64 as u32
}

/// The PLAY RTP-Info for the single track: the next packet's sequence number
/// and the RTP time of the live point the play range starts at (RFC 2326 12.33).
fn rtp_info(uri: &str, address: SocketAddr, sequence: u16, rtptime: u32) -> String {
    let base = if uri.starts_with("rtsp://") {
        uri.split('?').next().unwrap_or(uri).trim_end_matches('/')
    } else {
        return format!("url=rtsp://{address}/live/trackID=0;seq={sequence};rtptime={rtptime}");
    };
    let track = if base.ends_with("/trackID=0") {
        base.to_owned()
    } else {
        format!("{base}/trackID=0")
    };
    format!("url={track};seq={sequence};rtptime={rtptime}")
}

fn stopped(local: &AtomicBool, external: &AtomicBool) -> bool {
    local.load(Ordering::Acquire) || external.load(Ordering::Acquire)
}

#[derive(Default)]
struct AnnexB {
    bytes: Vec<u8>,
}

impl AnnexB {
    fn push(&mut self, bytes: &[u8], mut nal: impl FnMut(Vec<u8>)) -> Result<()> {
        self.bytes.extend_from_slice(bytes);
        ensure!(
            self.bytes.len() <= MAX_QUEUE_BYTES,
            "RTSP encoder produced an oversized H.264 NAL unit"
        );
        loop {
            let Some((first, length)) = start_code(&self.bytes, 0) else {
                return Ok(());
            };
            let Some((next, _)) = start_code(&self.bytes, first + length) else {
                if first > 0 {
                    self.bytes.drain(..first);
                }
                return Ok(());
            };
            let mut end = next;
            while end > first + length && self.bytes[end - 1] == 0 {
                end -= 1;
            }
            if end > first + length {
                nal(self.bytes[first + length..end].to_vec());
            }
            self.bytes.drain(..next);
        }
    }
}

fn start_code(bytes: &[u8], offset: usize) -> Option<(usize, usize)> {
    let mut i = offset;
    while i + 2 < bytes.len() {
        if bytes[i..i + 3] == [0, 0, 1] {
            return Some((i, 3));
        }
        if i + 3 < bytes.len() && bytes[i..i + 4] == [0, 0, 0, 1] {
            return Some((i, 4));
        }
        i += 1;
    }
    None
}

struct Request {
    method: String,
    uri: String,
    headers: HashMap<String, String>,
}

/// Incoming interleaved RTCP shares this stream with RTSP and sets
/// `interleaved`, since receiver reports keep a session alive. Partial
/// requests remain buffered across socket timeouts, and declared sizes are
/// bounded.
fn parse_request(buffer: &mut Vec<u8>, interleaved: &mut bool) -> Result<Option<Request>> {
    loop {
        if buffer.first() == Some(&b'$') {
            if buffer.len() < 4 {
                return Ok(None);
            }
            let length = u16::from_be_bytes([buffer[2], buffer[3]]) as usize + 4;
            if buffer.len() < length {
                return Ok(None);
            }
            buffer.drain(..length);
            *interleaved = true;
            continue;
        }
        let Some(end) = buffer.windows(4).position(|b| b == b"\r\n\r\n") else {
            return Ok(None);
        };
        let text = std::str::from_utf8(&buffer[..end]).context("invalid RTSP request encoding")?;
        let mut lines = text.split("\r\n");
        let mut request_line = lines
            .next()
            .context("missing RTSP request line")?
            .split_whitespace();
        let method = request_line
            .next()
            .context("missing RTSP method")?
            .to_string();
        let uri = request_line.next().context("missing RTSP URI")?.to_string();
        ensure!(
            request_line.next() == Some("RTSP/1.0") && request_line.next().is_none(),
            "unsupported RTSP request version"
        );
        let mut headers = HashMap::new();
        for line in lines {
            let (name, value) = line.split_once(':').context("invalid RTSP header")?;
            ensure!(
                !value.chars().any(char::is_control),
                "invalid RTSP header value"
            );
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
        let body = headers
            .get("content-length")
            .map(|v| v.parse::<usize>())
            .transpose()
            .context("invalid RTSP content length")?
            .unwrap_or(0);
        let length = (end + 4)
            .checked_add(body)
            .context("RTSP request is too large")?;
        ensure!(length <= MAX_REQUEST, "RTSP request is too large");
        if buffer.len() < length {
            return Ok(None);
        }
        buffer.drain(..length);
        return Ok(Some(Request {
            method,
            uri,
            headers,
        }));
    }
}

fn response(
    stream: &mut TcpStream,
    cseq: &str,
    code: u16,
    reason: &str,
    headers: &[(&str, String)],
    body: &str,
) -> io::Result<()> {
    let mut text = format!("RTSP/1.0 {code} {reason}\r\nCSeq: {cseq}\r\nServer: Capturefab\r\n");
    for (name, value) in headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    stream.write_all(text.as_bytes())
}

enum Delivery {
    Tcp { rtp: u8, rtcp: u8 },
    Udp { rtp: UdpSocket, rtcp: UdpSocket },
}

impl Delivery {
    fn send(&self, stream: &mut TcpStream, rtcp_packet: bool, bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Tcp { rtp, rtcp } => {
                let length = u16::try_from(bytes.len())
                    .map_err(|_| io::Error::other("RTP packet too large"))?;
                stream.write_all(&[
                    b'$',
                    if rtcp_packet { *rtcp } else { *rtp },
                    (length >> 8) as u8,
                    length as u8,
                ])?;
                stream.write_all(bytes)
            }
            Self::Udp { rtp, rtcp } if rtcp_packet => match rtcp.send(bytes) {
                // The RTCP socket is non-blocking; a dropped report is harmless.
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
                result => result.map(drop),
            },
            Self::Udp { rtp, .. } => rtp.send(bytes).map(drop),
        }
    }

    /// Drains RTCP the client sent to the server's UDP port, reporting
    /// whether any arrived. Interleaved RTCP arrives with RTSP requests.
    fn received_rtcp(&self) -> bool {
        let Self::Udp { rtcp, .. } = self else {
            return false;
        };
        let mut packet = [0; 1500];
        let mut received = false;
        // The socket is connected, so only the client's RTCP port reaches it.
        while let Ok(length) = rtcp.recv(&mut packet) {
            received |= length > 0;
        }
        received
    }
}

fn port_pair(value: &str) -> Option<(u16, u16)> {
    let (a, b) = value.split_once('-')?;
    let (a, b) = (a.parse::<u16>().ok()?, b.parse::<u16>().ok()?);
    (a != b).then_some((a, b))
}

fn setup_transport(
    header: &str,
    bind: Ipv4Addr,
    peer: SocketAddr,
    ssrc: u32,
) -> Result<(Delivery, String)> {
    for offer in header.split(',') {
        let mut parts = offer.trim().split(';');
        let protocol = parts.next().unwrap_or("").to_ascii_uppercase();
        let values: HashMap<_, _> = parts
            .filter_map(|p| p.trim().split_once('='))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim()))
            .collect();
        if offer
            .split(';')
            .any(|p| p.trim().eq_ignore_ascii_case("multicast"))
        {
            continue;
        }
        if protocol == "RTP/AVP/TCP" {
            let (rtp, rtcp) = if let Some(channels) = values.get("interleaved") {
                let Some(pair) = port_pair(channels) else {
                    continue;
                };
                pair
            } else {
                (0, 1)
            };
            if rtp > 255 || rtcp > 255 {
                continue;
            }
            return Ok((
                Delivery::Tcp {
                    rtp: rtp as u8,
                    rtcp: rtcp as u8,
                },
                format!("RTP/AVP/TCP;unicast;interleaved={rtp}-{rtcp};ssrc={ssrc:08X}"),
            ));
        }
        if protocol == "RTP/AVP" || protocol == "RTP/AVP/UDP" {
            let Some((client_rtp, client_rtcp)) =
                values.get("client_port").and_then(|v| port_pair(v))
            else {
                continue;
            };
            if client_rtp == 0 || client_rtcp == 0 {
                continue;
            }
            // Never honor a third-party destination: UDP stays with the RTSP
            // peer, and server sockets remain bound to the selected interface.
            for _ in 0..32 {
                let rtp = UdpSocket::bind((bind, 0))?;
                let port = rtp.local_addr()?.port();
                if port % 2 != 0 || port == u16::MAX {
                    continue;
                }
                let Ok(rtcp) = UdpSocket::bind((bind, port + 1)) else {
                    continue;
                };
                rtp.connect(SocketAddr::new(peer.ip(), client_rtp))?;
                rtcp.connect(SocketAddr::new(peer.ip(), client_rtcp))?;
                rtp.set_write_timeout(Some(Duration::from_millis(500)))?;
                rtcp.set_nonblocking(true)?;
                return Ok((
                    Delivery::Udp { rtp, rtcp },
                    format!(
                        "RTP/AVP;unicast;client_port={client_rtp}-{client_rtcp};server_port={port}-{};ssrc={ssrc:08X}",
                        port + 1
                    ),
                ));
            }
        }
    }
    Err(anyhow!("unsupported RTSP transport"))
}

struct Packetizer {
    sequence: u16,
    ssrc: u32,
    packets: u32,
    octets: u32,
    timestamp: u32,
}

impl Packetizer {
    fn packet(&mut self, timestamp: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
        let mut packet = Vec::with_capacity(12 + payload.len());
        packet.extend_from_slice(&[0x80, 96 | if marker { 0x80 } else { 0 }]);
        packet.extend_from_slice(&self.sequence.to_be_bytes());
        packet.extend_from_slice(&timestamp.to_be_bytes());
        packet.extend_from_slice(&self.ssrc.to_be_bytes());
        packet.extend_from_slice(payload);
        self.sequence = self.sequence.wrapping_add(1);
        self.packets = self.packets.wrapping_add(1);
        self.octets = self.octets.wrapping_add(payload.len() as u32);
        self.timestamp = timestamp;
        packet
    }

    fn access_unit(
        &mut self,
        unit: &AccessUnit,
        mut send: impl FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        for (index, nal) in unit.nals.iter().enumerate() {
            let last = index + 1 == unit.nals.len();
            if nal.len() <= RTP_PAYLOAD {
                send(&self.packet(unit.timestamp, last, nal))?;
            } else {
                let chunks = nal[1..].chunks(RTP_PAYLOAD - 2);
                let count = chunks.len();
                for (part, chunk) in chunks.enumerate() {
                    let mut payload = Vec::with_capacity(chunk.len() + 2);
                    payload.push((nal[0] & 0xe0) | 28);
                    payload.push(
                        (nal[0] & 31)
                            | if part == 0 { 0x80 } else { 0 }
                            | if part + 1 == count { 0x40 } else { 0 },
                    );
                    payload.extend_from_slice(chunk);
                    send(&self.packet(unit.timestamp, last && part + 1 == count, &payload))?;
                }
            }
        }
        Ok(())
    }

    fn sender_report(&self) -> Vec<u8> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let mut packet = vec![0x80, 200, 0, 6];
        packet.extend_from_slice(&self.ssrc.to_be_bytes());
        packet.extend_from_slice(
            &(now.as_secs() as u32)
                .wrapping_add(2_208_988_800)
                .to_be_bytes(),
        );
        packet.extend_from_slice(
            &((now.subsec_nanos() as u64 * (1u64 << 32) / 1_000_000_000) as u32).to_be_bytes(),
        );
        packet.extend_from_slice(&self.timestamp.to_be_bytes());
        packet.extend_from_slice(&self.packets.to_be_bytes());
        packet.extend_from_slice(&self.octets.to_be_bytes());
        // A compound RTCP packet also includes SDES/CNAME (RFC 3550 6.1).
        let cname = b"capturefab";
        let mut sdes = vec![0x81, 202, 0, 0];
        sdes.extend_from_slice(&self.ssrc.to_be_bytes());
        sdes.extend_from_slice(&[1, cname.len() as u8]);
        sdes.extend_from_slice(cname);
        sdes.push(0);
        while sdes.len() % 4 != 0 {
            sdes.push(0);
        }
        let words = (sdes.len() / 4 - 1) as u16;
        sdes[2..4].copy_from_slice(&words.to_be_bytes());
        packet.extend(sdes);
        packet
    }
}

#[allow(clippy::too_many_arguments)]
fn serve_client(
    mut stream: TcpStream,
    bind: Ipv4Addr,
    width: u32,
    height: u32,
    fps: f64,
    encoded: Arc<EncodedStream>,
    stop: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(2)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    stream.set_nodelay(true)?;
    let peer = stream.peer_addr()?;
    let address = stream.local_addr()?;
    let mut random = [0; 12];
    getrandom::fill(&mut random).map_err(|e| anyhow!("cannot create RTSP session: {e}"))?;
    let session = random[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let ssrc = u32::from_be_bytes(random[8..].try_into().unwrap());
    let mut packets = Packetizer {
        sequence: 0,
        ssrc,
        packets: 0,
        octets: 0,
        timestamp: 0,
    };
    let mut delivery: Option<Delivery> = None;
    let mut buffer = Vec::new();
    let mut playing = false;
    let mut waiting_for_idr = true;
    let mut sequence = 0;
    let mut last_request = Instant::now();
    let mut last_report = Instant::now() - Duration::from_secs(5);
    while !stopped(&stop, &shutdown) && last_request.elapsed() < SESSION_TIMEOUT {
        if encoded
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .error
            .is_some()
        {
            break;
        }
        let mut keepalive = delivery.as_ref().is_some_and(Delivery::received_rtcp);
        let request = parse_request(&mut buffer, &mut keepalive)?;
        if keepalive {
            last_request = Instant::now();
        }
        if let Some(request) = request {
            last_request = Instant::now();
            let cseq = request
                .headers
                .get("cseq")
                .map(String::as_str)
                .unwrap_or("0");
            ensure!(
                cseq.bytes().all(|b| b.is_ascii_digit()),
                "invalid RTSP CSeq"
            );
            let path = request
                .uri
                .split_once("://")
                .map_or(request.uri.as_str(), |(_, rest)| {
                    rest.find('/').map_or("/", |i| &rest[i..])
                })
                .split('?')
                .next()
                .unwrap_or("");
            if request.method != "OPTIONS"
                && !matches!(path, "/live" | "/live/" | "/live/trackID=0")
            {
                response(&mut stream, cseq, 404, "Not Found", &[], "")?;
                continue;
            }
            if let Some(supplied) = request.headers.get("session")
                && supplied.split(';').next() != Some(session.as_str())
            {
                response(&mut stream, cseq, 454, "Session Not Found", &[], "")?;
                continue;
            }
            let session_header = (
                "Session",
                format!("{session};timeout={}", SESSION_TIMEOUT.as_secs()),
            );
            match request.method.as_str() {
                "OPTIONS" => response(&mut stream, cseq, 200, "OK", &[("Public", "OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN, GET_PARAMETER, SET_PARAMETER".into())], "")?,
                "DESCRIBE" => {
                    match encoded.sdp(match address.ip() { std::net::IpAddr::V4(ip) => ip, _ => bind }, width, height, fps) {
                        Ok(sdp) => {
                            let base = if request.uri.starts_with("rtsp://") { format!("{}/", request.uri.trim_end_matches('/')) } else { format!("rtsp://{address}/live/") };
                            response(&mut stream, cseq, 200, "OK", &[("Content-Type", "application/sdp".into()), ("Content-Base", base)], &sdp)?;
                        }
                        Err(_) => response(&mut stream, cseq, 503, "Service Unavailable", &[], "")?,
                    }
                }
                "SETUP" => {
                    if playing { response(&mut stream, cseq, 455, "Method Not Valid in This State", &[], "")?; continue; }
                    match setup_transport(request.headers.get("transport").map(String::as_str).unwrap_or(""), bind, peer, ssrc) {
                        Ok((transport, header)) => {
                            delivery = Some(transport);
                            response(&mut stream, cseq, 200, "OK", &[session_header, ("Transport", header)], "")?;
                        }
                        Err(_) => response(&mut stream, cseq, 461, "Unsupported Transport", &[], "")?,
                    }
                }
                "PLAY" if delivery.is_some() => {
                    sequence = encoded.sequence();
                    let info = rtp_info(&request.uri, address, packets.sequence, timestamp(sequence + 1, fps));
                    response(&mut stream, cseq, 200, "OK", &[session_header, ("Range", "npt=now-".into()), ("RTP-Info", info)], "")?;
                    playing = true;
                    waiting_for_idr = true;
                }
                "PAUSE" if delivery.is_some() => { playing = false; response(&mut stream, cseq, 200, "OK", &[session_header], "")?; }
                "TEARDOWN" if delivery.is_some() => { response(&mut stream, cseq, 200, "OK", &[session_header], "")?; break; }
                "GET_PARAMETER" | "SET_PARAMETER" => response(&mut stream, cseq, 200, "OK", &[session_header], "")?,
                "PLAY" | "PAUSE" | "TEARDOWN" => response(&mut stream, cseq, 455, "Method Not Valid in This State", &[], "")?,
                _ => response(&mut stream, cseq, 405, "Method Not Allowed", &[], "")?,
            }
            continue;
        }
        if playing && let Some(unit) = encoded.after(sequence, waiting_for_idr) {
            let delivery = delivery.as_ref().unwrap();
            packets.access_unit(&unit, |packet| delivery.send(&mut stream, false, packet))?;
            sequence = unit.sequence;
            waiting_for_idr = false;
            if last_report.elapsed() >= Duration::from_secs(5) {
                delivery.send(&mut stream, true, &packets.sender_report())?;
                last_report = Instant::now();
            }
        }
        let mut chunk = [0; 4096];
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                buffer.extend_from_slice(&chunk[..count]);
                ensure!(buffer.len() <= MAX_REQUEST, "RTSP request is too large");
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_b_handles_split_start_codes() {
        let mut parser = AnnexB::default();
        let mut nals = Vec::new();
        for byte in [0, 0, 0, 1, 0x67, 4, 0, 0, 1, 0x68, 5, 0, 0, 0, 1, 9] {
            parser.push(&[byte], |nal| nals.push(nal)).unwrap();
        }
        assert_eq!(nals, vec![vec![0x67, 4], vec![0x68, 5]]);
    }

    #[test]
    fn request_parser_preserves_partial_requests_and_ignores_rtcp() {
        let mut buffer = b"$\x01\0\x03abcOPTIONS rtsp://localhost/live RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 3\r\n\r\na".to_vec();
        let mut interleaved = false;
        assert!(
            parse_request(&mut buffer, &mut interleaved)
                .unwrap()
                .is_none()
        );
        assert!(interleaved, "receiver reports keep the session alive");
        buffer.extend_from_slice(b"bc");
        let request = parse_request(&mut buffer, &mut interleaved)
            .unwrap()
            .unwrap();
        assert_eq!(
            (request.method.as_str(), request.headers["cseq"].as_str()),
            ("OPTIONS", "1")
        );
        assert!(buffer.is_empty());
        let mut invalid = b"OPTIONS * RTSP/1.0\r\nContent-Length: 999999\r\n\r\n".to_vec();
        assert!(parse_request(&mut invalid, &mut interleaved).is_err());
    }

    #[test]
    fn fragmented_h264_reconstructs_nal_and_marks_frame_end() {
        let nal: Vec<_> = std::iter::once(0x65)
            .chain((0..4000).map(|n| n as u8))
            .collect();
        let unit = AccessUnit {
            sequence: 1,
            timestamp: 90_000,
            nals: vec![vec![0x67, 1, 2], nal.clone()],
            idr: true,
            bytes: nal.len() + 3,
        };
        let mut packetizer = Packetizer {
            sequence: u16::MAX,
            ssrc: 123,
            packets: 0,
            octets: 0,
            timestamp: 0,
        };
        let mut packets = Vec::new();
        packetizer
            .access_unit(&unit, |packet| {
                packets.push(packet.to_vec());
                Ok(())
            })
            .unwrap();
        assert!(packets.iter().all(|p| p.len() <= RTP_PAYLOAD + 12));
        assert_eq!(packets[0][2..4], u16::MAX.to_be_bytes());
        assert_eq!(packets[1][2..4], 0u16.to_be_bytes());
        assert!(
            packets[..packets.len() - 1]
                .iter()
                .all(|p| p[1] & 0x80 == 0)
        );
        assert_ne!(packets.last().unwrap()[1] & 0x80, 0);
        let mut reconstructed = vec![(packets[1][12] & 0xe0) | (packets[1][13] & 31)];
        for packet in &packets[1..] {
            reconstructed.extend_from_slice(&packet[14..]);
        }
        assert_eq!(reconstructed, nal);
        assert_ne!(packets[1][13] & 0x80, 0);
        assert_ne!(packets.last().unwrap()[13] & 0x40, 0);
    }

    #[test]
    fn udp_transport_uses_client_ports_and_rejects_multicast() {
        let a = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let b = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let header = format!(
            "RTP/AVP;unicast;client_port={}-{}",
            a.local_addr().unwrap().port(),
            b.local_addr().unwrap().port()
        );
        let (delivery, transport) = setup_transport(
            &header,
            Ipv4Addr::LOCALHOST,
            "127.0.0.1:12345".parse().unwrap(),
            7,
        )
        .unwrap();
        assert!(matches!(delivery, Delivery::Udp { .. }));
        assert!(transport.contains("server_port="));
        assert!(
            setup_transport(
                "RTP/AVP;multicast;client_port=5000-5001",
                Ipv4Addr::LOCALHOST,
                "127.0.0.1:12345".parse().unwrap(),
                7
            )
            .is_err()
        );
    }

    #[test]
    fn udp_receiver_reports_are_seen_on_the_server_rtcp_port() {
        let client_rtp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let client_rtcp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let header = format!(
            "RTP/AVP;unicast;client_port={}-{}",
            client_rtp.local_addr().unwrap().port(),
            client_rtcp.local_addr().unwrap().port()
        );
        let (delivery, transport) = setup_transport(
            &header,
            Ipv4Addr::LOCALHOST,
            client_rtp.local_addr().unwrap(),
            7,
        )
        .unwrap();
        assert!(!delivery.received_rtcp());
        let server_rtcp = transport
            .split(';')
            .find_map(|part| part.strip_prefix("server_port="))
            .and_then(port_pair)
            .unwrap()
            .1;
        // An empty receiver report (RFC 3550 6.4.2) from the client's RTCP port.
        let report = [0x80, 201, 0, 1, 0x12, 0x34, 0x56, 0x78];
        client_rtcp
            .send_to(&report, (Ipv4Addr::LOCALHOST, server_rtcp))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !delivery.received_rtcp() {
            assert!(Instant::now() < deadline, "receiver report not seen");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!delivery.received_rtcp());
    }

    /// Reads one RTSP response, skipping interleaved frames.
    fn read_response(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut byte = [0];
        while !bytes.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
        }
        let text = String::from_utf8(bytes).unwrap();
        let length = text
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .map_or(0, |value| value.parse().unwrap());
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        text + std::str::from_utf8(&body).unwrap()
    }

    #[test]
    fn play_reports_the_first_packet_sequence_and_live_rtp_time() {
        let encoded = Arc::new(EncodedStream::default());
        encoded.publish(
            vec![
                vec![0x67, 0x42, 0xc0, 0x1e],
                vec![0x68, 0xce],
                vec![0x65, 1],
            ],
            1,
            30.0,
        );
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (server_encoded, server_stop) = (encoded.clone(), stop.clone());
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve_client(
                stream,
                Ipv4Addr::LOCALHOST,
                64,
                48,
                30.0,
                server_encoded,
                server_stop,
                Arc::new(AtomicBool::new(false)),
            )
        });
        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let url = format!("rtsp://{address}/live");
        write!(
            client,
            "SETUP {url}/trackID=0 RTSP/1.0\r\nCSeq: 1\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        )
        .unwrap();
        let setup = read_response(&mut client);
        let session = setup
            .lines()
            .find_map(|line| line.strip_prefix("Session: "))
            .and_then(|value| value.split(';').next())
            .unwrap()
            .to_owned();
        write!(
            client,
            "PLAY {url}/ RTSP/1.0\r\nCSeq: 2\r\nSession: {session}\r\nRange: npt=0.000-\r\n\r\n"
        )
        .unwrap();
        let play = read_response(&mut client);
        let rtptime = timestamp(2, 30.0);
        assert!(play.starts_with("RTSP/1.0 200 OK\r\n"), "{play}");
        assert!(
            play.contains(&format!(
                "\r\nRTP-Info: url={url}/trackID=0;seq=0;rtptime={rtptime}\r\n"
            )),
            "{play}"
        );
        encoded.publish(
            vec![
                vec![0x67, 0x42, 0xc0, 0x1e],
                vec![0x68, 0xce],
                vec![0x65, 2],
            ],
            2,
            30.0,
        );
        let mut header = [0; 4];
        client.read_exact(&mut header).unwrap();
        assert_eq!(header[..2], [b'$', 0]);
        let mut rtp = vec![0; u16::from_be_bytes([header[2], header[3]]) as usize];
        client.read_exact(&mut rtp).unwrap();
        assert_eq!(rtp[2..4], [0, 0]);
        assert_eq!(rtp[4..8], rtptime.to_be_bytes());
        write!(
            client,
            "TEARDOWN {url}/ RTSP/1.0\r\nCSeq: 3\r\nSession: {session}\r\n\r\n"
        )
        .unwrap();
        stop.store(true, Ordering::Release);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn lagging_clients_resume_at_keyframe_and_queue_is_bounded() {
        let stream = EncodedStream::default();
        for sequence in 1..=200 {
            stream.publish(
                vec![vec![if sequence % 10 == 0 { 0x65 } else { 0x61 }, 1]],
                sequence,
                30.0,
            );
        }
        let first = stream.after(0, false).unwrap();
        assert!(first.idr);
        assert_eq!(first.sequence, 80);
        assert_eq!(stream.after(80, false).unwrap().sequence, 81);
        assert_eq!(stream.after(81, true).unwrap().sequence, 90);
        let state = stream.state.lock().unwrap();
        assert_eq!(state.queue.len(), MAX_QUEUE_FRAMES);
        assert_eq!(state.bytes, MAX_QUEUE_FRAMES * 2);
    }

    #[test]
    #[ignore = "requires FFmpeg with libx264; restarts the camera under a live media reader"]
    fn media_reader_reconnects_after_the_camera_restarts() {
        let frames = Arc::new(LatestFrame::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        let (source_frames, source_shutdown) = (frames.clone(), shutdown.clone());
        let producer = thread::spawn(move || {
            let mut id = 0;
            while !source_shutdown.load(Ordering::Acquire) {
                id += 1;
                source_frames.publish(crate::types::Frame {
                    id,
                    width: 64,
                    height: 48,
                    pixel_format: crate::types::RGB8,
                    timestamp_ns: id * 50_000_000,
                    data: vec![(id % 255) as u8; 64 * 48 * 3],
                });
                thread::sleep(Duration::from_millis(50));
            }
        });
        let start = |port| {
            RtspServer::start(
                Ipv4Addr::LOCALHOST,
                port,
                64,
                48,
                20.0,
                "100k",
                frames.clone(),
                shutdown.clone(),
            )
            .unwrap()
        };
        let server = start(0);
        let port = server.local_addr().port();
        let url = format!("rtsp://127.0.0.1:{port}/live");
        let mut camera = crate::media::open_url(&url, Duration::from_secs(10)).unwrap();
        camera.start(64 * 48 * 3).unwrap();
        camera.next_frame(Duration::from_secs(2)).unwrap();
        drop(server);
        // While the camera is down every poll waits out its budget, and the
        // loss is reported once.
        let outage = Instant::now();
        let (mut polls, mut lost) = (0, 0);
        while outage.elapsed() < Duration::from_secs(3) {
            if let Err(error) = camera.next_frame(Duration::from_millis(100)) {
                assert!(format!("{error:#}").contains("timed out"), "{error:#}");
                polls += 1;
                if crate::media::stream_end(&error) == Some(crate::media::StreamEnd::Lost) {
                    lost += 1;
                }
            }
        }
        assert_eq!(lost, 1);
        assert!(polls <= 40, "{polls} polls in 3 s");
        let server = start(port);
        let resumed = Instant::now();
        while camera.next_frame(Duration::from_millis(100)).is_err() {
            assert!(resumed.elapsed() < Duration::from_secs(20));
        }
        camera.stop().unwrap();
        drop(server);
        shutdown.store(true, Ordering::Release);
        producer.join().unwrap();
    }

    #[test]
    #[ignore = "requires FFmpeg with libx264; exercises real TCP and UDP RTSP playback"]
    fn ffmpeg_decodes_camera_over_tcp_and_udp() {
        let executable = crate::media::executable().unwrap();
        let frames = Arc::new(LatestFrame::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        let source_frames = frames.clone();
        let source_shutdown = shutdown.clone();
        let producer = thread::spawn(move || {
            let mut id = 0;
            while !source_shutdown.load(Ordering::Acquire) {
                id += 1;
                source_frames.publish(crate::types::Frame {
                    id,
                    width: 64,
                    height: 48,
                    pixel_format: crate::types::RGB8,
                    timestamp_ns: id * 50_000_000,
                    data: vec![(id % 255) as u8; 64 * 48 * 3],
                });
                thread::sleep(Duration::from_millis(50));
            }
        });
        let result = (|| -> Result<()> {
            let server = RtspServer::start(
                Ipv4Addr::LOCALHOST,
                0,
                64,
                48,
                20.0,
                "100k",
                frames,
                shutdown.clone(),
            )?;
            for transport in ["tcp", "udp"] {
                let url = format!("rtsp://{}/live", server.local_addr());
                let mut decoder = crate::media::command(&executable)
                    .args([
                        "-hide_banner",
                        "-loglevel",
                        "error",
                        "-rtsp_transport",
                        transport,
                        "-i",
                        &url,
                        "-frames:v",
                        "3",
                        "-pix_fmt",
                        "rgb24",
                        "-f",
                        "rawvideo",
                        "pipe:1",
                    ])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()?;
                let mut stdout = decoder.stdout.take().unwrap();
                let mut stderr = decoder.stderr.take().unwrap();
                let output = thread::spawn(move || {
                    let mut bytes = Vec::new();
                    let _ = stdout.read_to_end(&mut bytes);
                    bytes
                });
                let diagnostic = thread::spawn(move || {
                    let mut bytes = Vec::new();
                    let _ = stderr.read_to_end(&mut bytes);
                    bytes
                });
                let deadline = Instant::now() + Duration::from_secs(10);
                let status = loop {
                    if let Some(status) = decoder.try_wait()? {
                        break status;
                    }
                    if Instant::now() >= deadline {
                        let _ = decoder.kill();
                        break decoder.wait()?;
                    }
                    thread::sleep(Duration::from_millis(10));
                };
                let bytes = output.join().unwrap();
                let diagnostic = diagnostic.join().unwrap();
                ensure!(
                    status.success(),
                    "FFmpeg {transport} playback failed: {}",
                    String::from_utf8_lossy(&diagnostic)
                );
                ensure!(
                    bytes.len() == 3 * 64 * 48 * 3,
                    "FFmpeg {transport} playback returned {} bytes",
                    bytes.len()
                );
                ensure!(
                    server.error().is_none(),
                    "RTSP encoder failed during playback"
                );
            }
            Ok(())
        })();
        shutdown.store(true, Ordering::Release);
        producer.join().unwrap();
        result.unwrap();
    }
}
