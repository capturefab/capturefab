//! A finite None-security UA TCP proxy that creates one recoverable Publish gap.
use opcua::types::{
    BinaryDecodable as _, ByteString, ContextOwned, EndpointDescription, NodeId,
    NotificationMessage, PublishRequest, RepublishRequest, response_header::ResponseHeader,
};
use std::{
    io::{self, Read as _, Write as _},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const MAX_FRAME: usize = 1 << 20;
#[derive(Clone, Copy, Debug)]
pub enum Event {
    Suppressed,
    Repaired,
}
#[derive(Default, Debug)]
pub struct Trace {
    pub accepted: usize,
    pub suppressed: usize,
    pub republished: usize,
    pub repair_forwarded: bool,
    pub ordinary_after_repair: usize,
    pub acknowledgements: Vec<(u32, u32, bool)>,
    pub original_notification: Vec<u8>,
    pub repaired_notification: Vec<u8>,
    pub subscription_id: u32,
    pub endpoint_rewrites: usize,
    pub null_available: usize,
}
struct State {
    stop: AtomicBool,
    sockets: Mutex<Vec<TcpStream>>,
    trace: Mutex<Trace>,
    events: mpsc::SyncSender<Event>,
    uri: String,
    end: Instant,
}
pub struct Proxy {
    pub endpoint: String,
    pub events: mpsc::Receiver<Event>,
    state: Arc<State>,
    worker: Option<JoinHandle<io::Result<()>>>,
}
#[track_caller]
fn err() -> io::Error {
    io::Error::other(format!(
        "bounded plain-UA fixture contract failed at {}",
        std::panic::Location::caller()
    ))
}
fn le32(bytes: &[u8]) -> io::Result<u32> {
    Ok(u32::from_le_bytes(bytes.try_into().map_err(|_| err())?))
}
impl Proxy {
    pub fn start(upstream: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("opc.tcp://{}/fixture", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::sync_channel(4);
        let state = Arc::new(State {
            stop: AtomicBool::new(false),
            sockets: Mutex::new(Vec::new()),
            trace: Mutex::new(Trace::default()),
            events: tx,
            uri: endpoint.clone(),
            end: Instant::now() + Duration::from_secs(12),
        });
        let run = state.clone();
        let worker = thread::spawn(move || {
            let mut workers = Vec::new();
            while !run.stop.load(Ordering::Acquire) && Instant::now() < run.end {
                match listener.accept() {
                    Ok((client, _)) => {
                        client.set_nonblocking(false)?;
                        let mut trace = run.trace.lock().map_err(|_| err())?;
                        trace.accepted += 1;
                        if trace.accepted > 2 {
                            return Err(err());
                        }
                        drop(trace);
                        let server = TcpStream::connect_timeout(&upstream, Duration::from_secs(2))?;
                        for socket in [&client, &server] {
                            socket.set_read_timeout(Some(Duration::from_millis(100)))?;
                            socket.set_write_timeout(Some(Duration::from_millis(300)))?;
                            socket.set_nodelay(true)?;
                            run.sockets
                                .lock()
                                .map_err(|_| err())?
                                .push(socket.try_clone()?);
                        }
                        let client_out = client.try_clone()?;
                        let server_out = server.try_clone()?;
                        let a = run.clone();
                        let b = run.clone();
                        workers.push(thread::spawn(move || {
                            forward(client, server_out, false, &a)
                        }));
                        workers.push(thread::spawn(move || forward(server, client_out, true, &b)));
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => return Err(error),
                }
            }
            for worker in workers {
                worker.join().map_err(|_| err())??;
            }
            Ok(())
        });
        Self {
            endpoint,
            events: rx,
            state,
            worker: Some(worker),
        }
    }
    pub fn finish(mut self) -> Trace {
        self.stop();
        self.worker.take().unwrap().join().unwrap().unwrap();
        let mut trace = self.state.trace.lock().unwrap();
        std::mem::take(&mut *trace)
    }
    fn stop(&self) {
        self.state.stop.store(true, Ordering::Release);
        for socket in self.state.sockets.lock().unwrap().iter() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}
impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn read_exact(socket: &mut TcpStream, bytes: &mut [u8], state: &State) -> io::Result<bool> {
    let mut at = 0;
    while at < bytes.len() {
        if state.stop.load(Ordering::Acquire) {
            return Ok(false);
        }
        if Instant::now() >= state.end {
            return Err(err());
        }
        match socket.read(&mut bytes[at..]) {
            Ok(0) if at == 0 || state.stop.load(Ordering::Acquire) => return Ok(false),
            Ok(0) => return Err(err()),
            Ok(count) => at += count,
            Err(_error) if state.stop.load(Ordering::Acquire) => return Ok(false),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
                ) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}
fn forward(
    mut input: TcpStream,
    mut output: TcpStream,
    response: bool,
    state: &State,
) -> io::Result<()> {
    let mut frames = 0;
    let mut chunked = false;
    loop {
        let mut header = [0_u8; 8];
        if !read_exact(&mut input, &mut header, state)? {
            break;
        }
        let size = usize::try_from(le32(&header[4..8])?).map_err(|_| err())?;
        if !(8..=MAX_FRAME).contains(&size) {
            return Err(err());
        }
        let mut frame = vec![0; size];
        frame[..8].copy_from_slice(&header);
        if !read_exact(&mut input, &mut frame[8..], state)? {
            break;
        }
        frames += 1;
        if frames > 256 {
            return Err(err());
        }
        let mut event = None;
        if &frame[..3] == b"MSG" {
            if frame[3] != b'F' {
                chunked = true;
            } else if chunked {
                chunked = false;
            } else {
                event = inspect(&mut frame, response, state)?;
            }
        }
        // Serialize the ACK observer with the repair write: an ACK that races
        // the successful kernel write must still see that write's completion.
        let mut repair = if matches!(event, Some(Event::Repaired)) {
            Some(state.trace.lock().map_err(|_| err())?)
        } else {
            None
        };
        match output.write_all(&frame) {
            Ok(()) => {
                if let Some(trace) = &mut repair {
                    trace.repair_forwarded = true;
                }
            }
            Err(_error) if state.stop.load(Ordering::Acquire) => break,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                ) =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
        drop(repair);
        if let Some(event) = event {
            state.events.try_send(event).map_err(|_| err())?;
        }
    }
    let _ = output.shutdown(Shutdown::Write);
    Ok(())
}
fn replace(frame: &mut Vec<u8>, start: usize, end: usize, replacement: &[u8]) -> io::Result<()> {
    if start < 24 || start > end || end > frame.len() {
        return Err(err());
    }
    let prefix = frame[..start].to_vec();
    let suffix = frame[end..].to_vec();
    frame.truncate(start);
    frame.extend_from_slice(replacement);
    frame.extend_from_slice(&suffix);
    if frame.len() > MAX_FRAME {
        return Err(err());
    }
    let length = u32::try_from(frame.len()).map_err(|_| err())?;
    frame[4..8].copy_from_slice(&length.to_le_bytes());
    // All original channel/token/transport sequence/request IDs remain exact.
    if frame[..4] != prefix[..4] || frame[8..24] != prefix[8..24] || !frame.ends_with(&suffix) {
        return Err(err());
    }
    Ok(())
}
fn rewrite_endpoints(
    frame: &mut Vec<u8>,
    body_start: usize,
    session: bool,
    state: &State,
) -> io::Result<()> {
    let context = ContextOwned::default();
    let context = context.context();
    let mut body = &frame[body_start..];
    ResponseHeader::decode(&mut body, &context).map_err(|_| err())?;
    if session {
        NodeId::decode(&mut body, &context).map_err(|_| err())?;
        NodeId::decode(&mut body, &context).map_err(|_| err())?;
        f64::decode(&mut body, &context).map_err(|_| err())?;
        ByteString::decode(&mut body, &context).map_err(|_| err())?;
        ByteString::decode(&mut body, &context).map_err(|_| err())?;
    }
    let count = i32::decode(&mut body, &context).map_err(|_| err())?;
    if !(1..=16).contains(&count) {
        return Err(err());
    }
    let mut ranges = Vec::new();
    for _ in 0..count {
        let start = frame.len() - body.len();
        let len = i32::from_le_bytes(
            body.get(..4)
                .ok_or_else(err)?
                .try_into()
                .map_err(|_| err())?,
        );
        let len = usize::try_from(len).map_err(|_| err())?;
        if len > 8192 {
            return Err(err());
        }
        let endpoint = EndpointDescription::decode(&mut body, &context).map_err(|_| err())?;
        if frame.get(start + 4..start + 4 + len).ok_or_else(err)?
            != endpoint.endpoint_url.as_ref().as_bytes()
        {
            return Err(err());
        }
        ranges.push((start, start + 4 + len));
    }
    // Edit only the native endpoint URL fields; retain every response header,
    // token, identity, diagnostic, certificate and other original byte.
    let mut replacement = i32::try_from(state.uri.len())
        .map_err(|_| err())?
        .to_le_bytes()
        .to_vec();
    replacement.extend_from_slice(state.uri.as_bytes());
    for (start, end) in ranges.into_iter().rev() {
        replace(frame, start, end, &replacement)?;
    }
    state.trace.lock().map_err(|_| err())?.endpoint_rewrites += 1;
    Ok(())
}
fn inspect(frame: &mut Vec<u8>, response: bool, state: &State) -> io::Result<Option<Event>> {
    if frame.len() < 26 {
        return Err(err());
    }
    let context = ContextOwned::default();
    let context = context.context();
    let mut body = &frame[24..];
    let type_id = NodeId::decode(&mut body, &context).map_err(|_| err())?;
    let body_start = frame.len() - body.len();
    if response && (type_id == NodeId::new(0, 431_u32) || type_id == NodeId::new(0, 464_u32)) {
        rewrite_endpoints(frame, body_start, type_id == NodeId::new(0, 464_u32), state)?;
    } else if !response && type_id == NodeId::new(0, 826_u32) {
        let value = PublishRequest::decode(&mut body, &context).map_err(|_| err())?;
        if !body.is_empty() {
            return Err(err());
        }
        let mut trace = state.trace.lock().map_err(|_| err())?;
        let repaired = trace.repair_forwarded;
        for ack in value.subscription_acknowledgements.unwrap_or_default() {
            if trace.acknowledgements.len() >= 128 {
                return Err(err());
            }
            trace
                .acknowledgements
                .push((ack.subscription_id, ack.sequence_number, repaired));
        }
    } else if !response && type_id == NodeId::new(0, 832_u32) {
        let value = RepublishRequest::decode(&mut body, &context).map_err(|_| err())?;
        if !body.is_empty() || value.retransmit_sequence_number != 1 {
            return Err(err());
        }
        let trace = state.trace.lock().map_err(|_| err())?;
        if trace.suppressed != 1 || value.subscription_id != trace.subscription_id {
            return Err(err());
        }
    } else if response && (type_id == NodeId::new(0, 829_u32) || type_id == NodeId::new(0, 835_u32))
    {
        ResponseHeader::decode(&mut body, &context).map_err(|_| err())?;
        if type_id == NodeId::new(0, 829_u32) {
            let subscription = le32(body.get(..4).ok_or_else(err)?)?;
            let available = i32::from_le_bytes(
                body.get(4..8)
                    .ok_or_else(err)?
                    .try_into()
                    .map_err(|_| err())?,
            );
            if !(-1..=128).contains(&available) {
                return Err(err());
            }
            if available == -1 {
                state.trace.lock().map_err(|_| err())?.null_available += 1;
            }
            let skip = 9_usize + usize::try_from(available.max(0)).map_err(|_| err())? * 4;
            body = body.get(skip..).ok_or_else(err)?;
            let notification_start = frame.len() - body.len();
            let notification =
                NotificationMessage::decode(&mut body, &context).map_err(|_| err())?;
            let notification_end = frame.len() - body.len();
            if notification
                .notification_data
                .as_ref()
                .is_some_and(|data| !data.is_empty())
            {
                let mut trace = state.trace.lock().map_err(|_| err())?;
                if trace.suppressed == 0 {
                    if notification.sequence_number != 1 {
                        return Err(err());
                    }
                    trace.subscription_id = subscription;
                    trace.original_notification =
                        frame[notification_start..notification_end].to_vec();
                    trace.suppressed = 1;
                    // Keep the original sequence/time and all other service bytes;
                    // only the notification-data array becomes empty.
                    replace(
                        frame,
                        notification_start + 12,
                        notification_end,
                        &0_i32.to_le_bytes(),
                    )?;
                    return Ok(Some(Event::Suppressed));
                }
                if trace.republished > 0 && notification.sequence_number >= 3 {
                    trace.ordinary_after_repair += 1;
                }
            }
        } else {
            let start = frame.len() - body.len();
            let notification =
                NotificationMessage::decode(&mut body, &context).map_err(|_| err())?;
            if !body.is_empty() || notification.sequence_number != 1 {
                return Err(err());
            }
            let mut trace = state.trace.lock().map_err(|_| err())?;
            if trace.suppressed != 1 || trace.republished != 0 {
                return Err(err());
            }
            trace.repaired_notification = frame[start..].to_vec();
            if trace.original_notification != trace.repaired_notification {
                return Err(err());
            }
            trace.republished = 1;
            drop(trace);
            // Deliberately create an observable real receive frontier after the
            // already delivered later Publish, not a fabricated timestamp.
            thread::sleep(Duration::from_millis(40));
            return Ok(Some(Event::Repaired));
        }
    }
    Ok(None)
}
