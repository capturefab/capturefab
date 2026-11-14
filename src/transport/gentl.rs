//! Optional camera acquisition through installed `GenTL` producers. Native code
//! is loaded exclusively in `__gentl-helper`; this module is a bounded proxy.
use crate::types::{Backend, CameraInfo, Categorized, ErrorKind, Frame, RegisterIo};
use anyhow::{Context as _, Result, ensure};
use std::{
    ffi::OsString,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use super::{gendc, gentl_selector::Selector, multipart::word_bytes};

mod client;
mod native;
mod process;
mod wire;

pub(crate) const ARG: &str = "__gentl-helper";
const MAX_PRODUCERS: usize = 64;
const MAX_DEVICES: usize = 256;
const MAX_NAME: usize = 4096;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
/// A helper's orderly close (SDK unlock and process exit) gets this long
/// before it is killed; a loaded host can need more than a few hundred ms.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);
const NATIVE_EVENT_WAIT: Duration = Duration::from_millis(10);
const EVENT_REPLY_HEADROOM: Duration = Duration::from_millis(20);
/// How long after a frame slice's deadline the reply to its short native
/// wait may still arrive. A missed reply retires the helper, so a loaded host
/// that merely delays an ordinary SDK timeout must not look like a hung one.
const EVENT_REPLY_GRACE: Duration = Duration::from_secs(1);
pub(super) const MAX_XML: usize = 16 * 1024 * 1024;
pub(super) const MAX_FRAME: usize = 128 * 1024 * 1024;

/// Whether camera discovery has any explicitly configured producer locations.
pub(crate) fn configured() -> bool {
    ["CAPTUREFAB_GENTL_PRODUCERS", "GENICAM_GENTL64_PATH"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}
fn producers() -> Result<Vec<PathBuf>> {
    let mut paths = std::collections::BTreeSet::new();
    let mut insert = |path: PathBuf| -> Result<()> {
        paths.insert(path);
        ensure!(
            paths.len() <= MAX_PRODUCERS,
            "at most64 GenTL producers may be configured"
        );
        Ok(())
    };
    if let Some(configured) = std::env::var_os("CAPTUREFAB_GENTL_PRODUCERS") {
        for (index, path) in std::env::split_paths(&configured).enumerate() {
            ensure!(
                index < MAX_PRODUCERS,
                "at most64 explicit GenTL producer paths may be configured"
            );
            if path.as_os_str().is_empty() {
                continue;
            }
            // A missing or invalid individual producer must not hide cameras
            // from the other producers. Its isolated loader reports the error.
            insert(
                path.canonicalize()
                    .or_else(|_| std::path::absolute(&path))?,
            )?;
        }
    }
    if let Some(configured) = std::env::var_os("GENICAM_GENTL64_PATH") {
        for (index, directory) in std::env::split_paths(&configured).enumerate() {
            ensure!(
                index < 64,
                "at most64 GenTL producer directories may be configured"
            );
            if directory.as_os_str().is_empty() || !directory.is_dir() {
                continue;
            }
            let mut count = 0;
            for entry in directory.read_dir()? {
                count += 1;
                ensure!(
                    count <= 1024,
                    "GenTL producer directory has too many entries; use CAPTUREFAB_GENTL_PRODUCERS to select explicit files"
                );
                let path = entry?.path();
                if path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("cti"))
                    && path.is_file()
                {
                    insert(path.canonicalize()?)?;
                }
            }
        }
    }
    Ok(paths.into_iter().collect())
}

enum DiscoveryEvent {
    Report(PathBuf, Result<wire::Discovery>),
    Cleanup(PathBuf, anyhow::Error),
}
fn deliver_discovery(
    event: DiscoveryEvent,
    found: &mut impl FnMut(&CameraInfo) -> bool,
    errors: &mut Vec<String>,
    completed: &mut usize,
) -> bool {
    let push = |errors: &mut Vec<String>, path: &PathBuf, message: String| {
        if errors.len() < 8 {
            errors.push(format!("{}: {message}", path.display()));
        }
    };
    match event {
        DiscoveryEvent::Report(path, result) => {
            *completed += 1;
            match result {
                Ok(report) => {
                    for warning in report.warnings {
                        push(errors, &path, warning);
                    }
                    for camera in report.cameras {
                        if found(&camera) {
                            return true;
                        }
                    }
                }
                Err(error) => push(errors, &path, format!("{error:#}")),
            }
        }
        DiscoveryEvent::Cleanup(path, error) => {
            push(errors, &path, format!("GenTL discovery cleanup: {error:#}"));
        }
    }
    false
}

/// Report each completed provider promptly. A broken provider contributes an
/// error after healthy providers' cameras have already been delivered.
pub(crate) fn discover_until(
    timeout: Duration,
    mut found: impl FnMut(&CameraInfo) -> bool,
) -> Result<()> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("GenTL discovery timeout overflow")?;
    let paths = Arc::new(producers()?);
    if paths.is_empty() {
        return Ok(());
    }
    let next = Arc::new(AtomicUsize::new(0));
    let waves = u32::try_from(paths.len().div_ceil(paths.len().min(4)))?;
    let provider_slice = timeout / waves;
    let (sender, receiver) = mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();
    for _ in 0..paths.len().min(4) {
        let (paths, next, sender, worker_cancelled) = (
            paths.clone(),
            next.clone(),
            sender.clone(),
            cancelled.clone(),
        );
        let spawned = std::thread::Builder::new()
            .name("capturefab-gentl-discovery".into())
            .spawn(move || {
                loop {
                    if worker_cancelled.load(Ordering::Acquire) {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = paths.get(index) else {
                        break;
                    };
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    let spawned = (|| {
                        let provider_deadline = Instant::now()
                            .checked_add(left.min(provider_slice))
                            .context("GenTL provider deadline overflow")?
                            .min(deadline);
                        Ok::<_, anyhow::Error>((
                            client::Client::spawn(provider_deadline)?,
                            provider_deadline,
                        ))
                    })();
                    let (mut client, provider_deadline) = match spawned {
                        Ok(spawned) => spawned,
                        Err(error) => {
                            if sender
                                .send(DiscoveryEvent::Report(path.clone(), Err(error)))
                                .is_err()
                            {
                                break;
                            }
                            continue;
                        }
                    };
                    let result = client
                        .discover(path.clone(), provider_deadline, &worker_cancelled)
                        .and_then(|response| {
                            let report: wire::Discovery =
                                serde_json::from_value(response.reply.value)?;
                            ensure!(
                                report.cameras.len() <= MAX_DEVICES && report.warnings.len() <= 8,
                                "GenTL provider returned too many cameras or warnings"
                            );
                            Ok(report)
                        });
                    // Publish metadata before cleanup consumes any more budget.
                    // The caller still joins every owner before opening a match.
                    let delivered = sender
                        .send(DiscoveryEvent::Report(path.clone(), result))
                        .is_ok();
                    let cleanup_deadline =
                        (Instant::now() + CLEANUP_TIMEOUT).min(provider_deadline);
                    if let Err(error) = client.close(cleanup_deadline) {
                        let _ = sender.send(DiscoveryEvent::Cleanup(path.clone(), error));
                    }
                    if !delivered {
                        break;
                    }
                }
            });
        match spawned {
            Ok(thread) => threads.push(thread),
            Err(error) => {
                cancelled.store(true, Ordering::Release);
                for thread in threads {
                    let _ = thread.join();
                }
                return Err(error.into());
            }
        }
    }
    drop(sender);
    let mut errors = Vec::new();
    let mut completed = 0;
    let mut matched = false;
    while completed < paths.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match receiver.recv_timeout(left) {
            Ok(event) => {
                matched = deliver_discovery(event, &mut found, &mut errors, &mut completed);
            }
            Err(_) => break,
        }
        if matched {
            break;
        }
    }
    cancelled.store(true, Ordering::Release);
    for thread in threads {
        let _ = thread.join();
    }
    // A result may race the exact receive deadline. Owner threads are now
    // stopped, so drain their bounded queue, including cleanup diagnostics.
    for event in receiver.try_iter() {
        if !matched {
            matched = deliver_discovery(event, &mut found, &mut errors, &mut completed);
        }
    }
    if matched {
        return Ok(());
    }
    if completed < paths.len() {
        errors.push("GenTL discovery reached its total deadline".into());
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(Categorized::error(
            ErrorKind::Unavailable,
            errors.join("; "),
        ))
    }
}

pub(crate) fn connect(selector: &str, timeout: Duration) -> Result<(CameraInfo, Box<dyn Backend>)> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("GenTL connection deadline overflow")?;
    let selector = Selector::parse(selector)?;
    let mut client = client::Client::spawn(deadline)?;
    let timeout = deadline.saturating_duration_since(Instant::now());
    let reply = client.call(
        wire::Request::Open {
            selector: selector.clone(),
            timeout_ms: millis(timeout),
        },
        Vec::new(),
        deadline,
        0,
    )?;
    let info = serde_json::from_value(reply.reply.value)?;
    Ok((
        info,
        Box::new(Proxy {
            client,
            selection_stop: None,
            selector,
            multipart_projection: false,
            pixel_encodings: Vec::new(),
        }),
    ))
}
struct Proxy {
    client: client::Client,
    selector: Selector,
    multipart_projection: bool,
    selection_stop: Option<Instant>,
    pixel_encodings: Vec<crate::types::PixelEncoding>,
}
impl Proxy {
    fn control_deadline(&self) -> Instant {
        let cap = Instant::now() + CONTROL_TIMEOUT;
        self.selection_stop
            .map_or(cap, |deadline| deadline.min(cap))
    }
}
impl RegisterIo for Proxy {
    fn read_memory_until(
        &mut self,
        address: u64,
        length: usize,
        deadline: Instant,
    ) -> Result<Vec<u8>> {
        let deadline = deadline.min(self.control_deadline());
        crate::types::control_remaining(deadline)?;
        ensure!(length <= MAX_XML, "GenTL register read exceeds 16 MiB");
        let response = self.client.call(
            wire::Request::Read { address, length },
            Vec::new(),
            deadline,
            length,
        )?;
        let payload = response.payload.into_bytes()?;
        ensure!(payload.len() == length, "short GenTL register read");
        crate::types::control_remaining(deadline)?;
        Ok(payload)
    }
    fn write_memory_until(&mut self, address: u64, data: &[u8], deadline: Instant) -> Result<()> {
        let deadline = deadline.min(self.control_deadline());
        crate::types::control_remaining(deadline)?;
        ensure!(
            data.len() <= 64 * 1024,
            "GenTL register write exceeds 64 KiB"
        );
        self.client
            .call(wire::Request::Write { address }, data.to_vec(), deadline, 0)?;
        crate::types::control_remaining(deadline)?;
        Ok(())
    }
    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        ensure!(length <= MAX_XML, "GenTL register read exceeds 16 MiB");
        let response = self.client.call(
            wire::Request::Read { address, length },
            Vec::new(),
            self.control_deadline(),
            length,
        )?;
        let payload = response.payload.into_bytes()?;
        ensure!(payload.len() == length, "short GenTL register read");
        Ok(payload)
    }
    fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
        ensure!(
            data.len() <= 64 * 1024,
            "GenTL register write exceeds 64 KiB"
        );
        self.client.call(
            wire::Request::Write { address },
            data.to_vec(),
            self.control_deadline(),
            0,
        )?;
        Ok(())
    }
}
impl Backend for Proxy {
    fn enable_multipart_projection(&mut self) -> Result<()> {
        self.client.call(
            wire::Request::EnableMultipartProjection,
            Vec::new(),
            self.control_deadline(),
            0,
        )?;
        self.multipart_projection = true;
        Ok(())
    }
    fn configure_pixel_encodings(
        &mut self,
        encodings: &[crate::types::PixelEncoding],
    ) -> Result<()> {
        ensure!(
            encodings.len()
                <= crate::types::IdsPixelFormat::ALL.len()
                    + crate::types::LucidPixelFormat::ALL.len(),
            "too many GenTL pixel encodings"
        );
        self.client.call(
            wire::Request::ConfigurePixelEncodings {
                encodings: encodings.to_vec(),
            },
            Vec::new(),
            self.control_deadline(),
            0,
        )?;
        self.pixel_encodings = encodings.to_vec();
        Ok(())
    }
    fn xml(&mut self) -> Result<String> {
        let response = self.client.call(
            wire::Request::Xml {
                timeout_ms: millis(CONTROL_TIMEOUT),
            },
            Vec::new(),
            Instant::now() + CONTROL_TIMEOUT,
            MAX_XML,
        )?;
        crate::genicam::decode_xml(&response.payload.into_bytes()?)
    }
    fn stream_payload_size(&mut self) -> Result<Option<usize>> {
        let response = self.client.call(
            wire::Request::StreamPayloadSize,
            Vec::new(),
            Instant::now() + CONTROL_TIMEOUT,
            0,
        )?;
        serde_json::from_value(response.reply.value).context("invalid GenTL stream payload size")
    }
    fn start(&mut self, payload_size: usize) -> Result<()> {
        self.client.call(
            wire::Request::Start { payload_size },
            Vec::new(),
            Instant::now() + CONTROL_TIMEOUT,
            0,
        )?;
        Ok(())
    }
    fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("GenTL frame deadline overflow")?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(wait) = event_wait(timeout, remaining) else {
                return Err(crate::types::FrameWait.into());
            };
            let began = Instant::now();
            // The helper answers within its native wait; allow the reply a
            // bounded grace past the slice so a busy host doesn't retire it.
            let reply_deadline = deadline.max(began + wait + EVENT_REPLY_GRACE);
            let response = self.client.call(
                wire::Request::Next {
                    timeout_ms: millis(wait),
                },
                Vec::new(),
                reply_deadline,
                MAX_FRAME,
            );
            match response {
                Ok(response) => {
                    let metadata: wire::Image = serde_json::from_value(response.reply.value)?;
                    validate_image_encoding(&metadata, &self.pixel_encodings)?;
                    validate_multipart_selection(
                        &metadata,
                        &self.selector,
                        self.multipart_projection,
                    )?;
                    return normalize(metadata, response.payload.into_frame()?);
                }
                Err(error) if crate::types::classify(&error).kind == ErrorKind::FrameTimeout => {
                    let rest = wait
                        .saturating_sub(began.elapsed())
                        .min(deadline.saturating_duration_since(Instant::now()));
                    if !rest.is_zero() {
                        std::thread::sleep(rest);
                    }
                }
                Err(error) => {
                    let classified = crate::types::classify(&error);
                    if classified.kind == ErrorKind::Unsupported
                        && classified
                            .details
                            .as_ref()
                            .and_then(|details| details["image_selectors"].as_array())
                            .is_some_and(|choices| !choices.is_empty())
                    {
                        // A part-specific selector uses a new worker. Keep the helper live for
                        // the camera's remote stop and unlock before releasing SDK ownership.
                        self.selection_stop = Some(deadline);
                    }
                    return Err(error);
                }
            }
        }
    }
    fn stop_after_frame_error(&self) -> bool {
        self.selection_stop.is_some()
    }
    fn stop(&mut self) -> Result<()> {
        self.client
            .call(wire::Request::Stop, Vec::new(), self.control_deadline(), 0)?;
        Ok(())
    }
    fn finish_stop(&mut self) {
        if let Some(deadline) = self.selection_stop.take() {
            let _ = self.client.close(deadline);
        }
    }
}
fn event_wait(timeout: Duration, remaining: Duration) -> Option<Duration> {
    // Keep time for the pipe reader to deliver an ordinary SDK timeout. On a
    // normal frame budget, skip the last short IPC call rather than retire an
    // idle trigger source because its reply arrived just after the deadline.
    // Short caller budgets can still attempt a native read.
    let minimum = (timeout / 2).clamp(NATIVE_EVENT_WAIT, NATIVE_EVENT_WAIT + EVENT_REPLY_HEADROOM);
    (remaining >= minimum).then(|| (remaining / 2).min(NATIVE_EVENT_WAIT))
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis())
        .unwrap_or(u64::MAX)
        .max(1)
}
fn validate_image_encoding(
    image: &wire::Image,
    encodings: &[crate::types::PixelEncoding],
) -> Result<()> {
    if let Some(format) = crate::frame::bicolor::Format::from_code(image.pixel_format) {
        format.validate_width(image.width)?;
    }
    if let Some(encoding) = image.pixel_encoding {
        encoding.check(image.pixel_format)?;
        ensure!(
            encoding.namespace().is_some()
                && encodings
                    .iter()
                    .any(|allowed| allowed.with_namespace(None) == encoding.with_namespace(None)),
            "GenTL helper returned an unqualified vendor encoding"
        );
    } else {
        ensure!(
            !crate::types::IdsPixelFormat::requires_qualification(image.pixel_format),
            "GenTL helper omitted the vendor pixel encoding"
        );
    }
    Ok(())
}

/// The helper cannot omit deferred-selection policy or invent a projected job.
fn validate_multipart_selection(
    image: &wire::Image,
    binding: &Selector,
    projection: bool,
) -> Result<()> {
    let Some(layout) = &image.multipart else {
        ensure!(
            binding.part.is_none() && binding.gendc.is_none(),
            "GenTL helper omitted the archive required by its explicit component binding"
        );
        return Ok(());
    };
    ensure!(
        layout.projected_format.is_none(),
        "GenTL helper returned unrequested per-job projection metadata"
    );
    if let Some(container) = &layout.gendc {
        ensure!(
            binding.part.is_none(),
            "GenTL helper replaced an explicit multipart binding with GenDC"
        );
        ensure!(
            !layout.selection_required,
            "GenDC cannot carry deferred multipart selection"
        );
        if let Some(selection) = binding.gendc {
            ensure!(
                container.selection(layout.selected_part()?) == Some(selection),
                "GenTL helper contradicted its explicit GenDC component binding"
            );
        }
    } else if let Some(index) = binding.part {
        ensure!(
            layout.selected == index && !layout.selection_required,
            "GenTL helper contradicted its explicit part binding"
        );
        ensure!(
            binding.part_format == layout.selected_part()?.measurement_format(),
            "GenTL helper contradicted its explicit part format binding"
        );
    } else {
        ensure!(
            binding.gendc.is_none(),
            "GenTL helper replaced an explicit GenDC binding with multipart"
        );
        let mut ordinary = layout.parts.iter().filter(|part| {
            part.data_type == 1 && part.length > 0 && part.measurement_format().is_none()
        });
        let first = ordinary.next();
        let unique = first.filter(|_| ordinary.next().is_none());
        if let Some(part) = unique {
            ensure!(
                layout.selected == part.index && !layout.selection_required,
                "GenTL helper changed its unique ordinary image selection"
            );
        } else {
            ensure!(
                projection && layout.selection_required,
                "GenTL helper omitted the required per-job selection guard"
            );
        }
    }
    Ok(())
}

fn normalize(mut image: wire::Image, mut raw: crate::framebuf::PoolVec) -> Result<Frame> {
    if let Some(format) = crate::frame::bicolor::Format::from_code(image.pixel_format) {
        format.validate_width(image.width)?;
    }
    if let Some(encoding) = image.pixel_encoding {
        encoding.check(image.pixel_format)?;
        ensure!(
            image.multipart.is_none(),
            "vendor encoding is not a multipart selection"
        );
    }
    crate::copies::record(crate::copies::Site::PipeRead, raw.len());
    let multipart = if let Some(layout) = image.multipart.take() {
        ensure!(
            image.chunks.is_none(),
            "multipart chunks require a separate indexed layout"
        );
        ensure!(
            image.byte_order == layout.byte_order,
            "multipart pixel byte order differs from its archive metadata"
        );
        if let Some(container) = &layout.gendc {
            let mut parsed = gendc::parse(
                &raw,
                gendc::Fill {
                    announced: layout
                        .original_buffer_bytes
                        .context("missing GenDC announced buffer size")?,
                    written: raw.len(),
                    intended: container.intended_buffer_bytes,
                },
            )?;
            parsed.selected = layout.selected;
            ensure!(
                parsed == layout,
                "GenDC descriptor differs from its helper metadata"
            );
        }
        let shape = layout.image_shape(layout.selected_part()?)?;
        ensure!(
            shape == (image.width, image.height, image.pixel_format)
                && image.offset == 0
                && image.x_padding == 0
                && image.y_padding == 0,
            "multipart selected image metadata is inconsistent"
        );
        let (parts, pixels) = crate::transport::multipart::MultipartData::retain(layout, raw)?;
        raw = pixels;
        Some(Arc::new(parts))
    } else {
        None
    };
    let row = crate::frame::image_row_bytes(image.width, image.pixel_format, image.pixel_encoding)
        .context("invalid GenTL pixel row layout")?;
    let rows = usize::try_from(image.height)?;
    let planar = crate::frame::planes(image.pixel_format, image.width, image.height).is_some();
    ensure!(
        image.x_padding == 0 || !planar,
        Categorized::error(
            ErrorKind::Unsupported,
            "padded GenTL planar images require an explicit component layout"
        )
    );
    let bytes = crate::frame::image_len(
        image.pixel_format,
        image.pixel_encoding,
        image.width,
        image.height,
    )
    .filter(|bytes| *bytes <= MAX_FRAME)
    .context("GenTL image size overflow")?;
    let stride = row
        .checked_add(image.x_padding)
        .context("GenTL row stride overflow")?;
    let end = image
        .offset
        .checked_add(bytes)
        .and_then(|end| end.checked_add(image.x_padding.checked_mul(rows)?))
        .and_then(|end| end.checked_add(image.y_padding))
        .context("GenTL image offset overflow")?;
    ensure!(
        image.width > 0 && image.height > 0 && row > 0 && end <= raw.len(),
        "GenTL IMAGE buffer contains unexpected or incomplete data"
    );

    let chunks = if let Some(layout) = image.chunks.take() {
        ensure!(
            layout.entries.len() <= 4096
                && layout
                    .descriptor_error
                    .as_ref()
                    .is_none_or(|error| error.len() <= 512),
            "GenTL chunk descriptor metadata exceeds its bounds"
        );
        let image_bytes = end
            .checked_sub(image.offset)
            .context("GenTL image span underflow")?;
        let original = crate::transport::chunks::ImageLayout {
            offset: image.offset,
            pfnc_code: image.pixel_encoding.is_none().then_some(image.pixel_format),
            pixel_encoding: image.pixel_encoding,
            width: image.width,
            height: image.height,
            row_bytes: row,
            row_stride: stride,
            padding_y: image.y_padding,
            byte_order: image.byte_order,
            continuous_packing: false,
        };
        let mut chunks = crate::transport::chunks::ChunkData::indexed(
            &raw,
            image_bytes,
            original,
            layout.layout_id.filter(|id| *id != 0),
            layout.entries,
        )?;
        chunks.descriptor_error = layout.descriptor_error;
        raw = chunks.retain(raw);
        image.offset = 0;
        Some(Arc::new(chunks))
    } else {
        ensure!(
            end == raw.len(),
            "GenTL IMAGE buffer has unexplained trailing bytes"
        );
        None
    };
    if image.offset > 0 || image.x_padding > 0 {
        if image.x_padding == 0 {
            raw.copy_within(image.offset..image.offset + bytes, 0);
        } else {
            for index in 0..rows {
                let first = image.offset + stride * index;
                raw.copy_within(first..first + row, index * row);
            }
        }
        crate::copies::record(crate::copies::Site::GentlNormalize, bytes);
    }
    raw.truncate(bytes);
    if image.byte_order == Some(crate::transport::chunks::ByteOrder::Big)
        && let Some(sample) = word_bytes(image.pixel_format)
    {
        raw.chunks_exact_mut(sample).for_each(<[u8]>::reverse);
        crate::copies::record(crate::copies::Site::GentlNormalize, bytes);
    }
    Ok(Frame {
        pixel_encoding: image.pixel_encoding,
        id: image.id,
        width: image.width,
        height: image.height,
        pixel_format: image.pixel_format,
        device_ts_ns: image.device_ts_ns,
        device_timestamp: image
            .device_timestamp_ticks
            .map(|ticks| crate::types::DeviceTimestamp {
                ticks,
                frequency_hz: image.device_timestamp_frequency_hz.filter(|hz| *hz > 0),
            }),
        data: raw.freeze(),
        chunks,
        multipart,
        ..Frame::default()
    })
}

/// Run before loading any native producer. The private output descriptor is
/// duplicated, and vendor stdout is redirected to the bounded stderr drainer.
pub(crate) fn run(_args: impl IntoIterator<Item = OsString>) -> Result<()> {
    let mut output = crate::session::protocol_output()?;
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut device: Option<native::Device> = None;
    let mut greeted = false;
    while let Some(packet) = wire::read_request(&mut input)? {
        if !greeted {
            let result = match packet.request {
                wire::Request::Hello { protocol } if protocol == wire::PROTOCOL => Ok(()),
                _ => Err(Categorized::error(
                    ErrorKind::Unsupported,
                    "GenTL helper requires a matching protocol greeting before any device operation",
                )),
            };
            match result {
                Ok(()) => {
                    wire::write_reply(
                        &mut output,
                        &wire::Reply::ok(serde_json::to_value(wire::Hello {
                            protocol: wire::PROTOCOL,
                        })?),
                        &[],
                    )?;
                    greeted = true;
                    continue;
                }
                Err(error) => {
                    wire::write_reply(&mut output, &wire::Reply::error(&error), &[])?;
                    break;
                }
            }
        }
        if matches!(packet.request, wire::Request::Hello { .. }) {
            let error =
                Categorized::error(ErrorKind::Unsupported, "GenTL helper was already greeted");
            wire::write_reply(&mut output, &wire::Reply::error(&error), &[])?;
            break;
        }
        if let wire::Request::Discover {
            producer,
            timeout_ms,
        } = &packet.request
        {
            match native::discover(producer, *timeout_ms) {
                Ok(discovered) => {
                    let reply = wire::Reply::ok(serde_json::to_value(&discovered.report)?);
                    wire::write_reply(&mut output, &reply, &[])?;
                    // Completed metadata reaches the parent before a vendor's
                    // teardown can hang or crash. Close is separately bounded.
                    drop(discovered);
                }
                Err(error) => wire::write_reply(&mut output, &wire::Reply::error(&error), &[])?,
            }
            continue;
        }
        if let wire::Request::Next { timeout_ms } = packet.request {
            let active = device.as_mut().context("GenTL device is not open")?;
            match native::Device::next(active, timeout_ms) {
                Ok(frame) => {
                    let handle = frame.handle;
                    let reply = wire::Reply::ok(serde_json::to_value(frame.image)?);
                    wire::write_frame(&mut output, &reply, &frame.bytes)?;
                    active.release(handle)?;
                }
                Err(error) => wire::write_reply(&mut output, &wire::Reply::error(&error), &[])?,
            }
            continue;
        }
        let closing = matches!(packet.request, wire::Request::Close);
        let result: Result<(serde_json::Value, Vec<u8>)> = (|| match packet.request {
            wire::Request::Open {
                selector,
                timeout_ms,
            } => {
                ensure!(device.is_none(), "a GenTL device is already open");
                let opened = native::Device::open(&selector, timeout_ms)?;
                let info = serde_json::to_value(&opened.info)?;
                device = Some(opened);
                Ok((info, Vec::new()))
            }
            wire::Request::Close => {
                drop(device.take());
                Ok((serde_json::Value::Null, Vec::new()))
            }
            request => {
                let device = device.as_mut().context("GenTL device is not open")?;
                match request {
                    wire::Request::Read { address, length } => {
                        Ok((serde_json::Value::Null, device.read(address, length)?))
                    }
                    wire::Request::Write { address } => {
                        device.write(address, &packet.payload)?;
                        Ok((serde_json::Value::Null, Vec::new()))
                    }
                    wire::Request::Xml { timeout_ms } => Ok((
                        serde_json::Value::Null,
                        device.xml(Duration::from_millis(timeout_ms).min(CONTROL_TIMEOUT))?,
                    )),
                    wire::Request::ConfigurePixelEncodings { encodings } => {
                        device.configure_pixel_encodings(&encodings)?;
                        Ok((serde_json::Value::Null, Vec::new()))
                    }
                    wire::Request::EnableMultipartProjection => {
                        device.enable_multipart_projection()?;
                        Ok((serde_json::Value::Null, Vec::new()))
                    }
                    wire::Request::StreamPayloadSize => Ok((
                        serde_json::to_value(device.stream_payload_size()?)?,
                        Vec::new(),
                    )),
                    wire::Request::Start { payload_size } => {
                        device.start(payload_size)?;
                        Ok((serde_json::Value::Null, Vec::new()))
                    }
                    wire::Request::Stop => {
                        device.stop()?;
                        Ok((serde_json::Value::Null, Vec::new()))
                    }
                    _ => unreachable!(),
                }
            }
        })();
        match result {
            Ok((value, payload)) => {
                wire::write_reply(&mut output, &wire::Reply::ok(value), &payload)?;
            }
            Err(error) => wire::write_reply(&mut output, &wire::Reply::error(&error), &[])?,
        }
        if closing {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_wait_preserves_reply_headroom_and_short_calls() {
        let normal = Duration::from_millis(100);
        assert_eq!(event_wait(normal, normal), Some(Duration::from_millis(10)));
        assert_eq!(
            event_wait(normal, Duration::from_millis(30)),
            Some(Duration::from_millis(10))
        );
        assert_eq!(event_wait(normal, Duration::from_millis(29)), None);
        let short = Duration::from_millis(15);
        assert_eq!(
            event_wait(short, Duration::from_millis(14)),
            Some(Duration::from_millis(7))
        );
        assert_eq!(event_wait(short, Duration::from_millis(9)), None);
    }

    fn image() -> wire::Image {
        wire::Image {
            pixel_encoding: None,
            id: 1,
            width: 3,
            height: 2,
            pixel_format: crate::types::MONO8,
            device_ts_ns: None,
            device_timestamp_ticks: None,
            device_timestamp_frequency_hz: None,
            offset: 0,
            x_padding: 0,
            y_padding: 0,
            byte_order: Some(crate::transport::chunks::ByteOrder::Little),
            chunks: None,
            multipart: None,
        }
    }

    #[test]
    fn helper_selection_guards_and_binding_kinds_are_revalidated_by_the_parent() {
        use crate::transport::multipart::{GvspPartFormat, job_selection_tests};
        let frame = job_selection_tests::ambiguous(false);
        let mut metadata = image();
        metadata.multipart = Some(frame.multipart.as_ref().unwrap().layout.clone());
        let mut binding = Selector {
            producer: std::env::temp_dir().join("mock.cti"),
            interface: "if0".into(),
            device: "dev0".into(),
            stream: None,
            part: None,
            part_format: None,
            gendc: None,
        };
        validate_multipart_selection(&metadata, &binding, false).unwrap_err();
        validate_multipart_selection(&metadata, &binding, true).unwrap();
        metadata.multipart.as_mut().unwrap().selection_required = false;
        validate_multipart_selection(&metadata, &binding, true).unwrap_err();
        metadata.multipart.as_mut().unwrap().selection_required = true;
        metadata.multipart.as_mut().unwrap().projected_format = Some(GvspPartFormat::Mono8);
        validate_multipart_selection(&metadata, &binding, true).unwrap_err();
        metadata.multipart.as_mut().unwrap().projected_format = None;
        binding.part = Some(0);
        validate_multipart_selection(&metadata, &binding, true).unwrap_err();
        metadata.multipart.as_mut().unwrap().selection_required = false;
        validate_multipart_selection(&metadata, &binding, true).unwrap();
        metadata.multipart.as_mut().unwrap().selected = 1;
        validate_multipart_selection(&metadata, &binding, true).unwrap_err();
        metadata.multipart = None;
        validate_multipart_selection(&metadata, &binding, true).unwrap_err();
        binding.part = None;
        validate_multipart_selection(&metadata, &binding, false).unwrap();
        binding.gendc = Some(gendc::Selection {
            component: 0,
            part: 0,
            type_id: 1,
            header_type: 0x4200,
            pfnc_code: crate::types::MONO8,
            group_id: 0,
            source_id: 0,
            region_id: 0,
        });
        validate_multipart_selection(&metadata, &binding, true).unwrap_err();
        metadata.multipart = Some(frame.multipart.as_ref().unwrap().layout.clone());
        validate_multipart_selection(&metadata, &binding, true).unwrap_err();
    }

    #[test]
    fn normalization_keeps_the_original_pool_allocation() {
        let bytes = [
            0xaa, 0x55, 1, 2, 3, 4, 5, 6, 0xff, 0xff, 7, 8, 9, 10, 11, 12, 0xff, 0xff,
        ];
        let mut raw = crate::framebuf::pool().take_stale(bytes.len());
        raw.copy_from_slice(&bytes);
        let pointer = raw.as_ptr();
        let frame = normalize(
            wire::Image {
                pixel_encoding: None,
                pixel_format: 0x0110_0007,
                offset: 2,
                x_padding: 2,
                byte_order: Some(crate::transport::chunks::ByteOrder::Big),
                ..image()
            },
            raw,
        )
        .unwrap();
        assert_eq!(frame.data.as_ptr(), pointer);
        assert_eq!(&*frame.data, &[2, 1, 4, 3, 6, 5, 8, 7, 10, 9, 12, 11]);
    }

    #[test]
    fn unpadded_odd_semiplanar_images_keep_all_planes() {
        let bytes = [
            0xaa, 0xbb, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17,
        ];
        let mut raw = crate::framebuf::pool().take_stale(bytes.len());
        raw.copy_from_slice(&bytes);
        let frame = normalize(
            wire::Image {
                pixel_encoding: None,
                height: 3,
                pixel_format: 0x020c_0112,
                offset: 2,
                ..image()
            },
            raw,
        )
        .unwrap();
        assert_eq!(&*frame.data, &bytes[2..]);
    }

    #[test]
    fn packed_word_formats_use_their_defined_packing_unit() {
        for (pixel_format, expected) in [
            (0x0210_0035, vec![2, 1, 4, 3, 6, 5, 8, 7, 10, 9, 12, 11]),
            (0x0220_001d, vec![4, 3, 2, 1, 8, 7, 6, 5, 12, 11, 10, 9]),
        ] {
            let mut raw = crate::framebuf::pool().take_stale(12);
            raw.copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
            let frame = normalize(
                wire::Image {
                    pixel_encoding: None,
                    height: if pixel_format == 0x0220_001d { 1 } else { 2 },
                    pixel_format,
                    byte_order: Some(crate::transport::chunks::ByteOrder::Big),
                    ..image()
                },
                raw,
            )
            .unwrap();
            assert_eq!(&*frame.data, expected.as_slice());
        }
    }

    #[test]
    fn indexed_chunk_payload_stays_in_its_original_allocation() {
        let original = [
            0x91, 0x92, 0x93, 0x94, 1, 2, 3, 4, 5, 6, 0xa1, 0xa2, 7, 8, 9, 10, 11, 12, 0xa3, 0xa4,
            0xa5, 0xa6, 255, 255, 255, 255, 255, 255, 255, 255, 0xc1, 0xc2,
        ];
        let mut raw = crate::framebuf::pool().take_stale(original.len());
        raw.copy_from_slice(&original);
        let pointer = raw.as_ptr();
        let frame = normalize(
            wire::Image {
                pixel_encoding: None,
                pixel_format: 0x0110_0007,
                offset: 4,
                x_padding: 2,
                y_padding: 2,
                byte_order: Some(crate::transport::chunks::ByteOrder::Big),
                chunks: Some(wire::ChunkLayout {
                    layout_id: Some(u64::MAX),
                    entries: vec![crate::transport::chunks::Chunk {
                        id: u64::MAX,
                        offset: 22,
                        length: 8,
                    }],
                    descriptor_error: None,
                }),
                ..image()
            },
            raw,
        )
        .unwrap();
        assert_eq!(&*frame.data, &[2, 1, 4, 3, 6, 5, 8, 7, 10, 9, 12, 11]);
        let chunks = frame.chunks.unwrap();
        assert_eq!(chunks.payload.as_ptr(), pointer);
        assert_ne!(frame.data.as_ptr(), pointer);
        assert_eq!(&*chunks.payload, &original);
        assert_eq!(chunks.image_layout.unwrap().offset, 4);
        assert_eq!(chunks.layout_id, Some(u64::MAX));
        assert_eq!(chunks.byte_order, None);
    }
    #[test]
    fn qualified_lucid_abcy16_normalizes_without_swapping_and_keeps_its_descriptor() {
        use crate::types::{IdsPixelFormat, LucidPixelFormat};
        let encoding = LucidPixelFormat::Coord3dAbcy16.encoding(Some(1));
        let mut metadata = image();
        metadata.width = 1;
        metadata.height = 1;
        metadata.pixel_format = 0x8240_0403;
        metadata.pixel_encoding = Some(encoding);
        validate_image_encoding(&metadata, &[LucidPixelFormat::Coord3dAbcy16.encoding(None)])
            .unwrap();
        validate_image_encoding(&metadata, &[IdsPixelFormat::Mono10g40.encoding(None)])
            .unwrap_err();
        let words = [0x34, 0x12, 0x00, 0x80, 0xfe, 0xff, 0xcd, 0xab];
        let mut raw = crate::framebuf::pool().take_stale(words.len());
        raw.copy_from_slice(&words);
        let frame = normalize(metadata, raw).unwrap();
        assert_eq!(&*frame.data, &words);
        assert_eq!(frame.pixel_encoding, Some(encoding));
        assert_eq!(crate::frame::frame_format_name(&frame), "Coord3D_ABCY16");
        assert_eq!(
            crate::inspect::frame_pixel_format_label(&frame),
            "Coord3D_ABCY16"
        );
        assert_eq!(crate::frame::frame_sample_bits(&frame), Some(16));
    }
    #[test]
    fn proxy_reply_encoding_must_match_its_configured_vendor_layout() {
        use crate::types::IdsPixelFormat;
        let mut image = image();
        image.pixel_format = IdsPixelFormat::Mono10g40.code();
        image.pixel_encoding = Some(IdsPixelFormat::Mono10g40.encoding(Some(4)));
        let allowed = [IdsPixelFormat::Mono10g40.encoding(None)];
        validate_image_encoding(&image, &allowed).unwrap();
        assert!(
            validate_image_encoding(&image, &[])
                .unwrap_err()
                .to_string()
                .contains("unqualified")
        );
        assert!(
            validate_image_encoding(&image, &[IdsPixelFormat::Mono12g24.encoding(None)])
                .unwrap_err()
                .to_string()
                .contains("unqualified")
        );
        image.pixel_encoding = Some(IdsPixelFormat::Mono10g40.encoding(Some(1000)));
        validate_image_encoding(&image, &allowed).unwrap();
        image.pixel_encoding = Some(IdsPixelFormat::Mono10g40.encoding(None));
        assert!(
            validate_image_encoding(&image, &allowed)
                .unwrap_err()
                .to_string()
                .contains("unqualified")
        );
        image.pixel_encoding = None;
        assert!(
            validate_image_encoding(&image, &allowed)
                .unwrap_err()
                .to_string()
                .contains("omitted")
        );
        image.pixel_encoding = Some(IdsPixelFormat::Mono12g24.encoding(Some(4)));
        assert!(
            validate_image_encoding(&image, &allowed)
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );
        image.pixel_format = crate::types::MONO8;
        image.pixel_encoding = None;
        validate_image_encoding(&image, &allowed).unwrap();
    }
    #[test]
    fn ids_bayer_helper_requires_matching_parent_qualification_and_explicit_descriptor() {
        use crate::types::IdsPixelFormat;
        for format in IdsPixelFormat::ALL
            .into_iter()
            .filter(|format| !format.is_monochrome())
        {
            let mut image = image();
            image.pixel_format = format.code();
            image.pixel_encoding = None;
            assert!(
                validate_image_encoding(&image, &[format.encoding(None)])
                    .unwrap_err()
                    .to_string()
                    .contains("omitted")
            );
            image.pixel_encoding = Some(format.encoding(Some(4)));
            validate_image_encoding(&image, &[format.encoding(None)]).unwrap();
            assert!(
                validate_image_encoding(&image, &[])
                    .unwrap_err()
                    .to_string()
                    .contains("unqualified")
            );
            image.pixel_encoding = Some(format.encoding(Some(1000)));
            validate_image_encoding(&image, &[format.encoding(None)]).unwrap();
            image.pixel_encoding = Some(format.encoding(Some(1001)));
            assert!(
                validate_image_encoding(&image, &[format.encoding(None)])
                    .unwrap_err()
                    .to_string()
                    .contains("namespace")
            );
        }
    }

    #[test]
    fn odd10p_helper_metadata_never_reaches_row_normalization() {
        for code in [0x0214_00a8, 0x0214_00aa] {
            for height in [1, 2] {
                let metadata = wire::Image {
                    pixel_format: code,
                    width: 3,
                    height,
                    ..image()
                };
                let error = validate_image_encoding(&metadata, &[]).unwrap_err();
                assert_eq!(crate::types::classify(&error).kind, ErrorKind::Unsupported);
                let raw = crate::framebuf::pool().take_stale(64);
                let error = normalize(metadata, raw).unwrap_err();
                assert_eq!(crate::types::classify(&error).kind, ErrorKind::Unsupported);
            }
        }
    }
}
