//! One frame made readable: a digital crop that
//! keeps the colour filter phase, exposure, auto-meter and focus
//! measurements, and a bounded JPEG or PNG preview. The session's `frame`
//! command answers with these for agents, scripts and remote clients.
#![deny(clippy::missing_const_for_fn)]
#![deny(clippy::cast_precision_loss)]
use crate::{
    camera_profile::CameraKind,
    frame::{self, Clipping, FrameRef, RowReader},
    jpeg::Raster,
    types::Frame,
};
use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::ops::RangeInclusive;

/// Frames to wait for after the request by default: one, so the frame was
/// exposed after any change ordered before the request.
pub(crate) const fn default_fresh() -> u32 {
    1
}
/// How long an inspection waits for its frame by default, in milliseconds.
pub(crate) const fn default_wait_ms() -> u64 {
    3_000
}
/// The preview's longest edge by default, in pixels.
pub(crate) const fn default_edge() -> u32 {
    1_024
}

pub(crate) const MAX_FRESH: u32 = 10;
/// The longest an inspection may wait for its frame: ten minutes, for a
/// part on a conveyor that triggers the camera.
pub(crate) const MAX_WAIT_MS: u64 = 600_000;
/// The wait `--timeout none` asks for: for a trigger until stopped, as a
/// capture's.
pub(crate) const WAIT_FOREVER_MS: u64 = crate::scheduling::MAX_DURATION_MS;
/// Preview edges a request may ask for; 0 asks for measurements only.
pub(crate) const EDGES: RangeInclusive<u32> = 64..=2048;
/// The largest encoded preview. About 2 MiB as base64, well inside the
/// session's 16 MiB reply limit and what vision models accept per image.
pub(crate) const MAX_IMAGE_BYTES: usize = 3 * 512 * 1024;

/// JPEG qualities tried in turn while a preview exceeds `MAX_IMAGE_BYTES`.
const QUALITIES: [i32; 3] = [85, 75, 65];
/// Region crops this small are shown near their native resolution; they
/// stay small at a higher quality.
const SMALL_EDGE: u32 = 512;
const SMALL_QUALITY: i32 = 92;
/// After the lowest quality, the preview shrinks by this factor at most
/// `SCALE_STEPS` times.
const SCALE: f64 = 0.8;
const SCALE_STEPS: i32 = 3;
/// Clipping above this share of samples is called out.
pub(crate) const CLIP_WARN: f64 = 0.005;
/// Mean brightness outside this range is called out.
pub(crate) const DARK: f64 = 0.25;
pub(crate) const BRIGHT: f64 = 0.65;
/// Normalized region sums may exceed 1 by float rounding (0.7 + 0.3).
const REGION_SLACK: f64 = 1e-9;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewFormat {
    /// JPEG in builds with libjpeg-turbo (the `jpeg` feature), else PNG.
    #[default]
    Auto,
    Jpeg,
    Png,
}

/// Check an inspection request.
pub(crate) fn validate(
    fresh: u32,
    timeout_ms: u64,
    max_dimension: u32,
    region: Option<[f64; 4]>,
) -> Result<()> {
    ensure!(fresh <= MAX_FRESH, "fresh frames must be 0..10");
    ensure!(
        timeout_ms <= MAX_WAIT_MS || timeout_ms == WAIT_FOREVER_MS,
        "inspection wait must be at most 600000 milliseconds (0: the camera's default), or {WAIT_FOREVER_MS} for none"
    );
    ensure!(
        max_dimension == 0 || EDGES.contains(&max_dimension),
        "preview size must be 0 or 64..2048 pixels"
    );
    if let Some([x, y, width, height]) = region {
        ensure!(
            [x, y, width, height].iter().all(|v| v.is_finite())
                && x >= 0.0
                && y >= 0.0
                && width > 0.0
                && height > 0.0
                && x + width <= 1.0 + REGION_SLACK
                && y + height <= 1.0 + REGION_SLACK,
            "region must be x, y, width, height inside 0..1 with positive width and height"
        );
    }
    Ok(())
}

/// Round a pixel count within finite integer bounds, including empty sides.
fn rounded_pixels(value: f64, minimum: u32, maximum: u32) -> u32 {
    let value = value
        .round()
        .max(f64::from(minimum))
        .min(f64::from(maximum));
    u32::try_from(
        crate::types::integral_float(value).expect("bounded pixel count is a finite integer"),
    )
    .expect("bounded pixel count fits u32")
}

/// A normalized `[x, y, width, height]` region in pixels of a `width` ×
/// `height` frame, clamped to the frame and at least one pixel on each side.
pub(crate) fn region_pixels(region: [f64; 4], width: u32, height: u32) -> [u32; 4] {
    let (w, h) = (f64::from(width), f64::from(height));
    let x = rounded_pixels(region[0] * w, 0, width.saturating_sub(1));
    let y = rounded_pixels(region[1] * h, 0, height.saturating_sub(1));
    [
        x,
        y,
        rounded_pixels(region[2] * w, 1, width - x),
        rounded_pixels(region[3] * h, 1, height - y),
    ]
}

/// A PFNC name such as `BayerRG8`, or the code in hex when it has none here.
pub(crate) fn pixel_format_label(pixel_format: u32) -> String {
    match frame::pixel_format_name(pixel_format) {
        "Unknown" => format!("0x{pixel_format:08x}"),
        name => name.into(),
    }
}

pub(crate) fn pixel_format_label_with_encoding(
    code: u32,
    encoding: Option<crate::types::PixelEncoding>,
) -> String {
    encoding
        .filter(|encoding| encoding.check(code).is_ok())
        .map_or_else(
            || pixel_format_label(code),
            |encoding| encoding.name().into(),
        )
}
pub(crate) fn frame_pixel_format_label(frame: &Frame) -> String {
    pixel_format_label_with_encoding(frame.pixel_format, frame.pixel_encoding)
}

/// The pixels `crop` takes for `px`: clamped to the frame and, for Bayer
/// frames, widened to whole 2×2 quads on even coordinates, so every pixel
/// keeps its colour filter.
pub(crate) fn crop_rect(frame: &Frame, px: [u32; 4]) -> [u32; 4] {
    let (fw, fh) = (frame.width, frame.height);
    if fw == 0 || fh == 0 {
        return [0, 0, fw, fh];
    }
    let x = px[0].min(fw - 1);
    let y = px[1].min(fh - 1);
    let width = px[2].clamp(1, fw - x);
    let height = px[3].clamp(1, fh - y);
    if !frame::frame_bayer(frame) || fw < 2 || fh < 2 {
        return [x, y, width, height];
    }
    let (even_width, even_height) = (fw & !1, fh & !1);
    let x0 = (x & !1).min(even_width - 2);
    let y0 = (y & !1).min(even_height - 2);
    let x1 = ((x + width).saturating_add(1) & !1).clamp(x0 + 2, even_width);
    let y1 = ((y + height).saturating_add(1) & !1).clamp(y0 + 2, even_height);
    [x0, y0, x1 - x0, y1 - y0]
}

/// The `crop_rect` of `px` as a frame of its own, with the original's
/// identity and timestamp. Wide color components retain their native depth
/// as interleaved RGB; other formats use `frame::unpacked`'s representation.
/// What a region measured in place must match.
#[cfg(test)]
pub(crate) fn crop(frame: &Frame, px: [u32; 4]) -> Result<Frame> {
    frame::validate_source(frame)?;
    if let Some(format) = frame::bicolor::Format::from_code(frame.pixel_format) {
        let [x, y, width, height] = crop_rect(frame, px);
        let output = format.unpacked().shifted(x as usize);
        let bytes = output.unpacked_pixel_bytes();
        let stride = frame::frame_row_bytes(frame);
        let row = width as usize * bytes;
        let mut data = Vec::with_capacity(row * height as usize);
        for yy in y..y + height {
            if format.unpacked() == format {
                let start = yy as usize * stride + x as usize * bytes;
                data.extend_from_slice(&frame.data[start..start + row]);
                continue;
            }
            let source = &frame.data[yy as usize * stride..][..stride];
            for xx in x..x + width {
                for value in format.pair(source, xx as usize) {
                    if format.bits() == 8 {
                        data.push(u8::try_from(value)?);
                    } else {
                        data.extend_from_slice(&value.to_le_bytes());
                    }
                }
            }
        }
        crate::copies::record(crate::copies::Site::Crop, data.len());
        return Ok(Frame {
            pixel_encoding: None,
            width,
            height,
            pixel_format: output.code(),
            data: data.into(),
            jpeg: None,
            chunks: frame.chunks.clone(),
            multipart: frame.multipart.clone(),
            ..*frame
        });
    }
    if let Some((_, bits)) = frame::native_color(frame, 0, 0) {
        let [x, y, width, height] = crop_rect(frame, px);
        let pixel_format = match bits {
            8 => crate::types::RGB8,
            10 => 0x0230_0018,
            12 => 0x0230_001a,
            14 => 0x0230_005e,
            16 => 0x0230_0033,
            _ => bail!("unsupported RGB component depth {bits}"),
        };
        let bytes = if bits == 8 { 3 } else { 6 };
        crate::copies::record(
            crate::copies::Site::Crop,
            width as usize * height as usize * bytes,
        );
        let mut data = Vec::with_capacity(width as usize * height as usize * bytes);
        for yy in y..y + height {
            for xx in x..x + width {
                let (rgb, _) =
                    frame::native_color(frame, xx as usize, yy as usize).ok_or_else(|| {
                        anyhow::anyhow!("validated native color layout changed across the frame")
                    })?;
                for sample in rgb {
                    if bits == 8 {
                        data.push(u8::try_from(sample).map_err(|_| {
                            anyhow::anyhow!("cropped eight-bit component exceeds u8")
                        })?);
                    } else {
                        data.extend_from_slice(&sample.to_le_bytes());
                    }
                }
            }
        }
        return Ok(Frame {
            pixel_encoding: None,
            width,
            height,
            pixel_format,
            data: data.into(),
            jpeg: None,
            chunks: frame.chunks.clone(),
            multipart: frame.multipart.clone(),
            ..*frame
        });
    }
    let frame = &*frame::unpacked(frame)?;
    let bytes = frame::validated_bytes_per_pixel(frame)?;
    let [x, y, width, height] = crop_rect(frame, px);
    let stride = frame.width as usize * bytes;
    let row = width as usize * bytes;
    crate::copies::record(crate::copies::Site::Crop, row * height as usize);
    let mut data = Vec::with_capacity(row * height as usize);
    for line in frame.data[..stride * frame.height as usize]
        .chunks_exact(stride)
        .skip(y as usize)
        .take(height as usize)
    {
        let start = x as usize * bytes;
        data.extend_from_slice(&line[start..start + row]);
    }
    Ok(Frame {
        width,
        height,
        data: data.into(),
        jpeg: None,
        chunks: frame.chunks.clone(),
        multipart: frame.multipart.clone(),
        ..*frame
    })
}

/// The `rect` of `frame`, read in place. A region of a frame that cannot
/// hold it is an error, as a crop of it would be.
fn view(frame: &Frame, rect: [u32; 4]) -> Result<FrameRef<'_>> {
    frame::validate_encoding(frame)?;
    let view = FrameRef::region(frame, rect);
    if !view.is_whole() && frame::frame_convertible(frame) {
        frame::validate_source(frame)?;
    }
    Ok(view)
}

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

/// How a region is exposed, from the evenly sampled pixels of
/// `frame::exposure`. Values are 0–1, rounded to 4 decimals; they are f64
/// so JSON carries them exactly as rounded.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct ExposureSummary {
    /// Mean luminance.
    pub mean: f64,
    /// Luminance percentiles, interpolated within the 64-bin histogram.
    pub p01: f64,
    pub p50: f64,
    pub p99: f64,
    /// Shares of samples crushed to black in every channel, and blown out
    /// in any channel.
    pub shadows: f64,
    pub highlights: f64,
    /// Luminance histogram in 16 equal bins from black to white, in samples.
    pub histogram16: [u32; 16],
    /// Mean of each colour channel; None for monochrome frames.
    pub channels: Option<Channels>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Channels {
    pub red: f64,
    pub green: f64,
    pub blue: f64,
}

/// `frame::Sharpness` rounded to 4 decimals, as f64 so JSON carries no
/// float noise.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Focus {
    pub laplacian: f64,
    pub tenengrad: f64,
    pub brenner: f64,
    pub variance: f64,
}

impl From<frame::Sharpness> for Focus {
    fn from(sharpness: frame::Sharpness) -> Self {
        let round = |value: f32| round4(f64::from(value));
        Self {
            laplacian: round(sharpness.laplacian),
            tenengrad: round(sharpness.tenengrad),
            brenner: round(sharpness.brenner),
            variance: round(sharpness.variance),
        }
    }
}

impl From<&frame::Exposure> for ExposureSummary {
    fn from(exposure: &frame::Exposure) -> Self {
        let samples: u32 = exposure.luma.iter().sum();
        let round = |value: f32| round4(f64::from(value));
        // Bin i holds 8-bit levels 4i..4i+3; its centre is 4i + 1.5.
        let mean_of = |bins: &[u32; 64]| {
            let total: f64 = (0u8..64)
                .zip(bins)
                .map(|(i, &n)| (4.0 * f64::from(i) + 1.5) * f64::from(n))
                .sum();
            round4(total / f64::from(samples.max(1)) / 255.0)
        };
        let percentile = |q: f64| {
            let target = q * f64::from(samples);
            let mut seen = 0.0;
            for (i, &n) in (0u8..64).zip(&exposure.luma) {
                let next = seen + f64::from(n);
                if n > 0 && next >= target {
                    let within = ((target - seen) / f64::from(n)).clamp(0.0, 1.0);
                    return round4((f64::from(i) + within) / 64.0);
                }
                seen = next;
            }
            1.0
        };
        Self {
            mean: round(exposure.mean),
            p01: percentile(0.01),
            p50: percentile(0.5),
            p99: percentile(0.99),
            shadows: round(exposure.shadows),
            highlights: round(exposure.highlights),
            histogram16: std::array::from_fn(|i| exposure.luma[4 * i..4 * i + 4].iter().sum()),
            channels: exposure
                .channels
                .as_ref()
                .map(|[red, green, blue]| Channels {
                    red: mean_of(red),
                    green: mean_of(green),
                    blue: mean_of(blue),
                }),
        }
    }
}

/// What a frame region measures, with advisory notes for a reader that
/// cannot see the image.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Measurements {
    pub exposure: Option<ExposureSummary>,
    /// Brightness as auto exposure meters it, at native bit depth.
    pub auto_meter: Option<crate::auto::Meter>,
    /// Clipped pixels over the whole region at native bit depth.
    pub clipping: Option<Clipping>,
    /// Focus scores of the region at full resolution. Absolute values depend
    /// on the scene; compare them for one region of one camera.
    pub focus: Option<Focus>,
    pub notes: Vec<String>,
}

/// Measure the `px` region of `frame` (see `crop_rect`) from a camera of
/// `kind`. Depth, thermal and line-scan images and undecodable formats have
/// no exposure or focus; a note says why.
#[cfg(any(test, feature = "testing"))]
#[must_use]
pub fn measure(frame: &Frame, kind: CameraKind, region_px: [u32; 4]) -> Measurements {
    let rect = crop_rect(frame, region_px);
    match view(frame, rect) {
        Ok(view) => measure_view(frame, view, kind, rect),
        Err(error) => Measurements {
            notes: vec![format!("not measured: {error:#}")],
            ..Measurements::default()
        },
    }
}

fn measure_view(
    frame: &Frame,
    view: FrameRef<'_>,
    kind: CameraKind,
    rect: [u32; 4],
) -> Measurements {
    let mut measured = Measurements::default();
    if frame::is_unclassified_mono16(frame) {
        measured.notes.push("The Mono16 component purpose is unspecified. Exposure and focus require a verified intensity image.".into());
        return measured;
    }
    if frame::is_measurement_frame(frame) {
        measured.notes.push(format!(
            "{} contains measurement samples. Exposure and focus require an intensity image.",
            frame_pixel_format_label(frame)
        ));
        return measured;
    }
    let scalar = match kind {
        CameraKind::Depth => Some("Depth images hold distances, not brightness"),
        CameraKind::Thermal => Some("Thermal images hold temperatures, not brightness"),
        CameraKind::LineScan => Some("Line-scan images are built line by line"),
        _ => None,
    };
    if let Some(what) = scalar {
        measured
            .notes
            .push(format!("{what}; exposure and focus are not measured"));
        return measured;
    }
    if !frame::frame_convertible(frame) {
        measured.notes.push(format!(
            "{} frames cannot be decoded; exposure, focus and the preview are unavailable",
            frame_pixel_format_label(frame)
        ));
        return measured;
    }
    match frame::exposure_in(view) {
        Ok(exposure) => measured.exposure = Some((&exposure).into()),
        Err(error) => measured
            .notes
            .push(format!("exposure not measured: {error:#}")),
    }
    match frame::clipping_in(view) {
        Ok(clipping) => {
            measured.clipping = Some(Clipping {
                max: round4(clipping.max),
                ..clipping
            });
        }
        Err(error) => measured
            .notes
            .push(format!("clipping not counted: {error:#}")),
    }
    measured.auto_meter = crate::auto::meter_in(view).map(|meter| crate::auto::Meter {
        mean: round4(meter.mean),
        p99: round4(meter.p99),
    });
    match frame::sharpness(frame, rect) {
        Ok(focus) => measured.focus = Some(focus.into()),
        Err(error) => measured
            .notes
            .push(format!("focus not measured: {error:#}")),
    }
    if let Some(exposure) = &measured.exposure {
        let percent = |share: f64| format!("{:.1} %", share * 100.0);
        if exposure.highlights > CLIP_WARN {
            measured.notes.push(format!(
                "highlights clipped: {} of the image at full scale",
                percent(exposure.highlights)
            ));
        }
        if exposure.shadows > CLIP_WARN {
            measured.notes.push(format!(
                "shadows crushed: {} of the image at black",
                percent(exposure.shadows)
            ));
        }
        if exposure.mean < DARK {
            measured.notes.push(format!(
                "dark: mean brightness {:.2} is below {DARK}",
                exposure.mean
            ));
        } else if exposure.mean > BRIGHT {
            measured.notes.push(format!(
                "bright: mean brightness {:.2} is above {BRIGHT}",
                exposure.mean
            ));
        }
    }
    measured
}

/// A compressed preview.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedImage {
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    pub bytes: frame::Body<'static>,
}

/// JSON carries the image as base64 beside its byte count.
impl Serialize for EncodedImage {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use base64::Engine as _;
        use serde::ser::SerializeStruct as _;
        let mut image = serializer.serialize_struct("EncodedImage", 5)?;
        image.serialize_field("mime", self.mime)?;
        image.serialize_field("width", &self.width)?;
        image.serialize_field("height", &self.height)?;
        image.serialize_field("bytes", &self.bytes.len())?;
        image.serialize_field(
            "base64",
            &base64::engine::general_purpose::STANDARD.encode(&self.bytes),
        )?;
        image.end()
    }
}

/// The first JPEG quality for a preview of a `width` × `height` source:
/// higher for a region crop of at most `SMALL_EDGE` pixels, whatever the
/// preview's size. A downscaled whole frame is not a crop.
fn first_quality(cropped: bool, width: u32, height: u32) -> i32 {
    if cropped && width.max(height) <= SMALL_EDGE {
        SMALL_QUALITY
    } else {
        QUALITIES[0]
    }
}

/// Whether a preview is a JPEG; PNG otherwise.
fn jpeg_preview(format: PreviewFormat, jpeg_available: bool) -> Result<bool> {
    Ok(match format {
        PreviewFormat::Auto => jpeg_available,
        PreviewFormat::Jpeg => {
            if !jpeg_available {
                return Err(crate::types::Categorized::error(
                    crate::types::ErrorKind::Unsupported,
                    "JPEG previews are unsupported in this build; request png or rebuild with --features jpeg",
                ));
            }
            true
        }
        PreviewFormat::Png => false,
    })
}

/// A preview of `view` whose longest edge is at most `max_dimension`, or
/// None for 0 and for formats that cannot be decoded. Pixels are averaged
/// over each output pixel's area, so detail blends instead of aliasing.
/// Monochrome stays gray. A preview larger than `limit` is encoded again at
/// lower JPEG quality, then smaller, before giving up.
fn render_within(
    view: FrameRef<'_>,
    max_dimension: u32,
    format: PreviewFormat,
    jpeg_available: bool,
    limit: usize,
    cropped: bool,
) -> Result<Option<EncodedImage>> {
    if max_dimension == 0
        || view.width == 0
        || view.height == 0
        || !frame::frame_convertible(view.frame)
    {
        return Ok(None);
    }
    let jpeg = jpeg_preview(format, jpeg_available)?;
    frame::validate_source(view.frame)?;
    let channels = if frame::frame_monochrome(view.frame) {
        frame::Channels::Gray
    } else {
        frame::Channels::Rgb
    };
    let source_width =
        u32::try_from(view.width).context("preview width exceeds native dimensions")?;
    let source_height =
        u32::try_from(view.height).context("preview height exceeds native dimensions")?;
    let (width, height) = fit(source_width, source_height, max_dimension);
    let first = first_quality(cropped, source_width, source_height);
    let lowest = QUALITIES[QUALITIES.len() - 1];
    for step in 0..=SCALE_STEPS {
        let shrink = |side: u32| rounded_pixels(f64::from(side) * SCALE.powi(step), 1, side.max(1));
        let (out_width, out_height) = (shrink(width), shrink(height));
        let pixels = shrunk(view, channels, out_width, out_height)?;
        let raster = Raster::of(out_width, out_height, channels.bytes(), &pixels);
        let qualities = std::iter::once(if step == 0 { first } else { lowest }).chain(
            QUALITIES
                .into_iter()
                .filter(|&quality| step == 0 && quality < first),
        );
        for quality in qualities {
            let bytes = if jpeg {
                crate::jpeg::software_quality(&raster, quality)?
            } else {
                png(&raster)?.into()
            };
            if bytes.len() <= limit {
                return Ok(Some(EncodedImage {
                    mime: if jpeg { "image/jpeg" } else { "image/png" },
                    width: raster.width,
                    height: raster.height,
                    bytes,
                }));
            }
            if !jpeg {
                break;
            }
        }
    }
    bail!("preview image exceeds the size limit; request a smaller max_dimension")
}

/// `width` × `height` scaled to fit `max_edge`, never enlarged.
fn fit(width: u32, height: u32, max_edge: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= max_edge {
        return (width, height);
    }
    let scale = |side: u32| {
        u32::try_from(
            ((u64::from(side) * u64::from(max_edge) + u64::from(longest) / 2) / u64::from(longest))
                .max(1),
        )
        .expect("a fitted edge cannot exceed its u32 source dimension")
    };
    (scale(width), scale(height))
}

/// `view` as `channels` samples, box-filtered down to `out_width` ×
/// `out_height` (see `area_average`) as its rows are decoded, so no
/// full-size converted image is built. Output rows are independent, so
/// large views are split into bands of them across cores. A view that is
/// not scaled is borrowed where the frame holds its samples.
fn shrunk(
    view: FrameRef<'_>,
    channels: frame::Channels,
    out_width: u32,
    out_height: u32,
) -> Result<frame::Samples<'_>> {
    let (w, h) = (view.width, view.height);
    let (ow, oh) = (out_width as usize, out_height as usize);
    debug_assert!((1..=w).contains(&ow) && (1..=h).contains(&oh));
    let bytes = channels.bytes();
    if !RowReader::new(view).borrows(channels) {
        crate::copies::record(crate::copies::Site::PreviewTransform, w * h * bytes);
    }
    if (ow, oh) == (w, h) {
        return frame::samples(view, channels);
    }
    let width = u32::try_from(w).context("preview width exceeds native dimensions")?;
    let height = u32::try_from(h).context("preview height exceeds native dimensions")?;
    // Every byte is an output pixel the box filter writes.
    let mut out = crate::framebuf::transforms().take_stale(ow * oh * bytes);
    let threads = frame::worker_threads(view.pixels()).min(oh);
    let band = oh.div_ceil(threads);
    let columns = AreaColumns::new(width, out_width);
    // The first source row of output row `row`.
    let first = |row: usize| {
        let row = u64::try_from(row).expect("output row indices fit u64");
        usize::try_from((row * u64::from(height)).div_ceil(u64::from(out_height)))
            .expect("a source row index fits the source height")
    };
    let shrink_band = |rows: std::ops::Range<usize>, out: &mut [u8]| {
        let mut shrink = AreaAverage::new(&columns, channels, out_height, height);
        let mut reader = RowReader::new(view);
        for y in first(rows.start)..first(rows.end) {
            shrink.push(y, reader.row(y, channels), out);
        }
    };
    if threads > 1 {
        std::thread::scope(|scope| {
            for (i, out) in out.chunks_mut(band * ow * bytes).enumerate() {
                let shrink_band = &shrink_band;
                let rows = i * band..((i + 1) * band).min(oh);
                scope.spawn(move || shrink_band(rows, out));
            }
        });
    } else {
        shrink_band(0..oh, &mut out);
    }
    Ok(frame::Samples::Converted(out))
}

/// A box filter fed one source row at a time: each source pixel belongs to
/// exactly one output pixel, which is the rounded mean of its source pixels.
struct AreaAverage<'a> {
    channels: usize,
    out_height: u32,
    height: u32,
    columns: &'a AreaColumns,
    sums: Vec<u64>,
    /// Source rows summed into the current output row.
    rows: u64,
    /// Output rows written so far into the caller's band.
    written: usize,
}

/// Immutable column geometry, shared by every output band of a preview.
struct AreaColumns {
    /// The output column of each source column.
    destinations: Vec<u32>,
    /// Source columns in each output column.
    widths: Vec<u32>,
}
impl AreaColumns {
    fn new(width: u32, out_width: u32) -> Self {
        debug_assert!(width > 0 && (1..=width).contains(&out_width));
        let mut columns =
            Vec::with_capacity(usize::try_from(width).expect("source width fits usize"));
        let mut widths = vec![0u32; usize::try_from(out_width).expect("output width fits usize")];
        let (mut column, mut remainder) = (0, 0);
        for _ in 0..width {
            columns.push(column);
            widths[usize::try_from(column).expect("output column fits usize")] += 1;
            // Advance floor(x * out_width / width) without forming that
            // product. Subtract first so even remainder + out_width cannot
            // overflow on a 32-bit host with a very wide source row.
            let room = width - remainder;
            if out_width >= room {
                remainder = out_width - room;
                column += 1;
            } else {
                remainder += out_width;
            }
        }
        Self {
            destinations: columns,
            widths,
        }
    }
}

impl<'a> AreaAverage<'a> {
    fn new(
        columns: &'a AreaColumns,
        channels: frame::Channels,
        out_height: u32,
        height: u32,
    ) -> Self {
        Self {
            channels: channels.bytes(),
            out_height,
            height,
            columns,
            sums: vec![0; columns.widths.len() * channels.bytes()],
            rows: 0,
            written: 0,
        }
    }
    /// Add source row `y`; when it ends an output row, write that row to
    /// the next row of `out`.
    fn push(&mut self, y: usize, line: &[u8], out: &mut [u8]) {
        let channels = self.channels;
        for (pixel, &column) in line.chunks_exact(channels).zip(&self.columns.destinations) {
            let column = usize::try_from(column).expect("output column fits usize");
            for (sum, &value) in self.sums[column * channels..][..channels]
                .iter_mut()
                .zip(pixel)
            {
                *sum += u64::from(value);
            }
        }
        self.rows += 1;
        let (h, oh) = (u64::from(self.height), u64::from(self.out_height));
        let y = u64::try_from(y).expect("source row indices fit u64");
        // The next source row starts another output row, or there is none.
        if y + 1 == h || (y + 1) * oh / h != y * oh / h {
            let row = &mut out[self.written * self.sums.len()..][..self.sums.len()];
            for (i, (sum, out)) in self.sums.iter_mut().zip(row).enumerate() {
                let count = self.rows * u64::from(self.columns.widths[i / channels]);
                *out = u8::try_from((*sum + count / 2) / count)
                    .expect("mean of eight-bit samples fits eight bits");
                *sum = 0;
            }
            self.rows = 0;
            self.written += 1;
        }
    }
}

/// Box-filter `source` (`width` × `height` pixels of `channels` bytes) down
/// to `out_width` × `out_height` at once: what `shrunk` streams.
#[cfg(test)]
fn area_average(
    source: &[u8],
    width: u32,
    height: u32,
    channels: usize,
    out_width: u32,
    out_height: u32,
) -> std::borrow::Cow<'_, [u8]> {
    let (w, h) = (width as usize, height as usize);
    let (ow, oh) = (out_width as usize, out_height as usize);
    if (ow, oh) == (w, h) {
        return std::borrow::Cow::Borrowed(&source[..w * h * channels]);
    }
    let gray = if channels == 1 {
        frame::Channels::Gray
    } else {
        frame::Channels::Rgb
    };
    let columns = AreaColumns::new(width, out_width);
    let mut shrink = AreaAverage::new(&columns, gray, out_height, height);
    let mut out = vec![0; ow * oh * channels];
    for (y, line) in source.chunks_exact(w * channels).take(h).enumerate() {
        shrink.push(y, line, &mut out);
    }
    std::borrow::Cow::Owned(out)
}

fn png(raster: &Raster<'_>) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, raster.width, raster.height);
    encoder.set_color(if raster.channels == 1 {
        png::ColorType::Grayscale
    } else {
        png::ColorType::Rgb
    });
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_filter(png::FilterType::Paeth);
    encoder.write_header()?.write_image_data(&raster.data)?;
    Ok(bytes)
}

/// The pixels an inspection covered.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// The same rectangle as fractions of the frame.
    pub normalized: [f64; 4],
}

/// A frame region's measurements and preview.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Inspection {
    pub region: Region,
    #[serde(flatten)]
    pub measurements: Measurements,
    pub image: Option<EncodedImage>,
}

/// Measure and preview the normalized `region` of `frame` (all of it when
/// None) from a camera of `kind`; see `measure` and `render`.
///
/// # Errors
/// Returns an error if the source cannot provide the requested view, the
/// preview dimensions or pixel format cannot be rendered, or image encoding
/// fails. The requested region is clamped to the source frame.
pub fn inspect(
    frame: &Frame,
    kind: CameraKind,
    region: Option<[f64; 4]>,
    max_dimension: u32,
    format: PreviewFormat,
) -> Result<Inspection> {
    let requested = region.map_or([0, 0, frame.width, frame.height], |region| {
        region_pixels(region, frame.width, frame.height)
    });
    let rect = crop_rect(frame, requested);
    let view = view(frame, rect)?;
    let measurements = measure_view(frame, view, kind, rect);
    let image = render_within(
        view,
        max_dimension,
        format,
        cfg!(feature = "jpeg"),
        MAX_IMAGE_BYTES,
        !view.is_whole() && frame::frame_convertible(frame),
    )?;
    let [x, y, width, height] = rect;
    let share = |part: u32, whole: u32| round4(f64::from(part) / f64::from(whole.max(1)));
    Ok(Inspection {
        region: Region {
            x,
            y,
            width,
            height,
            normalized: [
                share(x, frame.width),
                share(y, frame.height),
                share(width, frame.width),
                share(height, frame.height),
            ],
        },
        measurements,
        image,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn very_tall_previews_keep_exact_row_boundaries() {
        let source: Vec<u8> = (0..100_000u32).map(|row| row.to_le_bytes()[0]).collect();
        let input = frame(MONO8, 1, 100_000, source.clone());
        let actual = shrunk(FrameRef::whole(&input), frame::Channels::Gray, 1, 80_000).unwrap();
        let expected: Vec<u8> = (0..80_000u64)
            .map(|row| {
                let start = usize::try_from((row * 100_000).div_ceil(80_000)).unwrap();
                let end = usize::try_from(((row + 1) * 100_000).div_ceil(80_000)).unwrap();
                let count = u64::try_from(end - start).unwrap();
                let sum: u64 = source[start..end].iter().copied().map(u64::from).sum();
                u8::try_from((sum + count / 2) / count).unwrap()
            })
            .collect();
        assert_eq!(&*actual, expected.as_slice());
    }

    #[test]
    fn preview_columns_match_exact_ratios_without_native_width_products() {
        fn check(width: u32, out_width: u32) {
            let columns = AreaColumns::new(width, out_width);
            let mut widths = vec![0u32; usize::try_from(out_width).unwrap()];
            for (x, &column) in columns.destinations.iter().enumerate() {
                let expected = u64::try_from(x).unwrap() * u64::from(out_width) / u64::from(width);
                assert_eq!(
                    column,
                    u32::try_from(expected).unwrap(),
                    "{width} → {out_width}, x={x}"
                );
                widths[usize::try_from(column).expect("output column fits usize")] += 1;
            }
            assert_eq!(columns.widths, widths);
        }
        for width in 1..=64 {
            for out_width in 1..=width {
                check(width, out_width);
            }
        }
        // The direct product exceeds u32 even though the row is only 100 KB.
        check(100_000, 80_000);
    }
    use crate::types::{MONO8, RGB8};
    use std::borrow::Cow;

    const MONO12: u32 = 0x0110_0005;
    const BAYER_RG8: u32 = 0x0108_0009;
    const COORD3D_C16: u32 = 0x0110_00b8;

    /// A preview as this build makes it, within the inspection limit.
    fn render(
        frame: &Frame,
        max_dimension: u32,
        format: PreviewFormat,
    ) -> Result<Option<EncodedImage>> {
        render_within(
            FrameRef::whole(frame),
            max_dimension,
            format,
            cfg!(feature = "jpeg"),
            MAX_IMAGE_BYTES,
            false,
        )
    }

    fn frame(pixel_format: u32, width: u32, height: u32, data: Vec<u8>) -> Frame {
        Frame {
            pixel_encoding: None,
            device_timestamp: None,
            received_monotonic_ns: None,
            chunks: None,
            multipart: None,
            id: 7,
            width,
            height,
            pixel_format,
            device_ts_ns: Some(99),
            wall_ms: 0,
            data: data.into(),
            jpeg: None,
        }
    }
    /// A horizontal ramp from black to white with vertical bars, so it has
    /// both a spread histogram and edges to focus on.
    fn ramp(width: u32, height: u32) -> Vec<u8> {
        (0..height)
            .flat_map(|_| {
                (0..width).map(move |x| {
                    let level = u8::try_from(x * 255 / (width - 1)).unwrap();
                    if x % 8 < 2 { level / 2 } else { level }
                })
            })
            .collect()
    }
    fn noise(len: usize) -> Vec<u8> {
        let mut state = 0x2545_f491_u32;
        std::iter::repeat_with(|| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state.to_le_bytes()[0]
        })
        .take(len)
        .collect()
    }

    #[test]
    fn requests_are_bounded_with_messages_that_never_read_as_timeouts() {
        validate(default_fresh(), default_wait_ms(), default_edge(), None).unwrap();
        validate(0, 1, 0, Some([0.0, 0.0, 1.0, 1.0])).unwrap();
        validate(10, 600_000, 2048, Some([0.7, 0.7, 0.3, 0.3])).unwrap();
        validate(1, 3000, 64, Some([0.25, 0.5, 0.5, 0.5])).unwrap();
        // 0: the camera's default wait.
        validate(1, 0, 64, None).unwrap();
        for (fresh, wait, edge, region, message) in [
            (11, 3000, 1024, None, "fresh frames must be 0..10"),
            (
                1,
                600_001,
                1024,
                None,
                "inspection wait must be at most 600000 milliseconds",
            ),
            (
                1,
                3000,
                63,
                None,
                "preview size must be 0 or 64..2048 pixels",
            ),
            (1, 3000, 2049, None, "preview size"),
            (1, 3000, 1024, Some([0.5, 0.0, 0.6, 1.0]), "region must be"),
            (1, 3000, 1024, Some([0.0, 0.0, 0.0, 1.0]), "region must be"),
            (1, 3000, 1024, Some([-0.1, 0.0, 0.5, 0.5]), "region must be"),
            (
                1,
                3000,
                1024,
                Some([0.0, f64::NAN, 0.5, 0.5]),
                "region must be",
            ),
        ] {
            let error = validate(fresh, wait, edge, region).unwrap_err();
            assert!(error.to_string().contains(message), "{error}");
            assert_eq!(crate::cli::error_code(&error).1, 1, "{error}");
        }
    }

    #[test]
    fn regions_map_to_pixels_clamped_inside_the_frame() {
        assert_eq!(
            region_pixels([0.0, 0.0, 1.0, 1.0], 640, 480),
            [0, 0, 640, 480]
        );
        assert_eq!(
            region_pixels([0.25, 0.25, 0.5, 0.5], 640, 480),
            [160, 120, 320, 240]
        );
        // A sliver is still one pixel; a region past the edge ends there.
        assert_eq!(
            region_pixels([1.0, 1.0, 1e-9, 1e-9], 640, 480),
            [639, 479, 1, 1]
        );
        assert_eq!(
            region_pixels([0.9, 0.9, 0.2, 0.2], 100, 100),
            [90, 90, 10, 10]
        );
        let mono = frame(MONO8, 10, 8, vec![0; 80]);
        assert_eq!(crop_rect(&mono, [3, 5, 99, 99]), [3, 5, 7, 3]);
        assert_eq!(crop_rect(&mono, [99, 99, 0, 0]), [9, 7, 1, 1]);
        assert_eq!(
            region_pixels([0.5, 0.5, 0.5, 0.5], u32::MAX, u32::MAX),
            [2_147_483_648, 2_147_483_648, 2_147_483_647, 2_147_483_647]
        );
        assert_eq!(region_pixels([0.0, 0.0, 1.0, 1.0], 0, 0), [0; 4]);
        assert_eq!(
            fit(u32::MAX, u32::MAX - 1, u32::MAX - 1),
            (u32::MAX - 1, u32::MAX - 2)
        );
        assert_eq!(fit(1, u32::MAX, 17), (1, 17));
    }

    #[test]
    fn bayer_crops_keep_the_colour_filter_phase() {
        let (width, height) = (8, 6);
        let source = frame(
            BAYER_RG8,
            width,
            height,
            (0..width * height).map(|i| i.to_le_bytes()[0]).collect(),
        );
        // Odd corners widen to whole quads on even coordinates.
        assert_eq!(crop_rect(&source, [3, 1, 2, 2]), [2, 0, 4, 4]);
        assert_eq!(crop_rect(&source, [7, 5, 1, 1]), [6, 4, 2, 2]);
        let cropped = crop(&source, [3, 1, 2, 2]).unwrap();
        assert_eq!(
            (
                cropped.width,
                cropped.height,
                cropped.pixel_format,
                cropped.id
            ),
            (4, 4, BAYER_RG8, 7)
        );
        // The crop's first pixel is the source's red site at (2, 0).
        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(
                    cropped.data[(y * 4 + x) as usize],
                    source.data[(y * width + x + 2) as usize]
                );
            }
        }
        assert_eq!(
            frame::bayer_pattern(cropped.pixel_format),
            frame::bayer_pattern(source.pixel_format)
        );
        // Mono crops are exact.
        let mono = frame(MONO8, 8, 6, source.data.to_vec());
        let cropped = crop(&mono, [3, 1, 2, 2]).unwrap();
        assert_eq!(cropped.data, [11, 12, 19, 20]);
    }

    #[test]
    fn bayer_crop_rect_clamps_maximum_dimensions_without_overflow() {
        let source = frame(BAYER_RG8, u32::MAX, u32::MAX, Vec::new());
        assert_eq!(
            crop_rect(&source, [u32::MAX - 2, u32::MAX - 2, 2, 2]),
            [u32::MAX - 3, u32::MAX - 3, 2, 2]
        );
    }

    #[test]
    fn packed_frames_crop_unpacked_and_measure_at_native_depth() {
        // BayerRG12p: each pair of 12-bit samples lsb packed into three bytes.
        let (width, height) = (8u32, 6u32);
        let sample = |i: u32| u16::try_from(i * 50).unwrap();
        let packed = (0..width * height / 2)
            .flat_map(|pair| {
                let (a, b) = (sample(2 * pair), sample(2 * pair + 1));
                [
                    a.to_le_bytes()[0],
                    a.to_le_bytes()[1] | (b << 4).to_le_bytes()[0],
                    u8::try_from(b >> 4).unwrap(),
                ]
            })
            .collect();
        let source = frame(0x010c_0059, width, height, packed);
        assert_eq!(crop_rect(&source, [3, 1, 2, 2]), [2, 0, 4, 4]);
        let cropped = crop(&source, [3, 1, 2, 2]).unwrap();
        assert_eq!(
            (cropped.width, cropped.height, cropped.pixel_format),
            (4, 4, 0x0110_0011)
        );
        let expected: Vec<u8> = (0..4)
            .flat_map(|y| (2..6).flat_map(move |x| sample(y * width + x).to_le_bytes()))
            .collect();
        assert_eq!(cropped.data, expected);
        assert_eq!(
            frame::bayer_pattern(cropped.pixel_format),
            frame::bayer_pattern(BAYER_RG8)
        );
        // Mono12p rows of 4095 and 0 measure half bright, metered at 12 bits.
        let mono12p = frame(0x010c_0047, 4, 4, [0xff, 0x0f, 0x00].repeat(8));
        for region in [[0, 0, 4, 4], [1, 1, 2, 2]] {
            let measured = measure(&mono12p, CameraKind::AreaScan, region);
            assert!(measured.exposure.unwrap().channels.is_none());
            assert!((measured.auto_meter.unwrap().mean - 0.5).abs() < 1e-9);
        }
        let image = render(&mono12p, 64, PreviewFormat::Png).unwrap().unwrap();
        assert_eq!((image.width, image.height), (4, 4));
    }

    #[test]
    fn color_crops_keep_native_samples_for_metering() {
        let (width, height) = (4u32, 3u32);
        for (format, bits, bgr, planar, packed, alpha, canonical) in [
            (0x0230_0018, 10, false, false, false, false, 0x0230_0018),
            (0x0224_0049, 12, true, false, true, false, 0x0230_001a),
            (0x0240_0063, 14, false, false, false, true, 0x0230_005e),
            (0x0230_0024, 16, false, true, false, false, 0x0230_0033),
            (0x0220_012e, 8, true, false, false, true, RGB8),
        ] {
            let max = (1u32 << bits) - 1;
            let pixels: Vec<[u16; 3]> = (0..width * height)
                .map(|i| {
                    [
                        max / 4 + 3 * i + 1,
                        max / 2 + 2 * i + 2,
                        3 * max / 4 + i + 3,
                    ]
                    .map(|v| u16::try_from(v).unwrap())
                })
                .collect();
            let components: Vec<u16> = if planar {
                (0..3)
                    .flat_map(|channel| pixels.iter().map(move |rgb| rgb[channel]))
                    .collect()
            } else {
                pixels
                    .iter()
                    .flat_map(|rgb| {
                        let mut channels = if bgr {
                            vec![rgb[2], rgb[1], rgb[0]]
                        } else {
                            rgb.to_vec()
                        };
                        if alpha {
                            channels.push(u16::try_from(max).unwrap());
                        }
                        channels
                    })
                    .collect()
            };
            let data = if packed {
                components
                    .chunks_exact(width as usize * 3)
                    .flat_map(|row| {
                        let mut packed = vec![0u8; (row.len() * bits).div_ceil(8)];
                        for (i, &value) in row.iter().enumerate() {
                            for bit in 0..bits {
                                packed[(i * bits + bit) / 8] |=
                                    u8::from((value >> bit) & 1 != 0) << ((i * bits + bit) % 8);
                            }
                        }
                        packed
                    })
                    .collect()
            } else if bits == 8 {
                components
                    .iter()
                    .map(|&v| u8::try_from(v).unwrap())
                    .collect()
            } else {
                components.iter().flat_map(|v| v.to_le_bytes()).collect()
            };
            let mut source = frame(format, width, height, data);
            let cropped = crop(&source, [1, 1, 2, 2]).unwrap();
            let selected = [pixels[5], pixels[6], pixels[9], pixels[10]];
            let expected = frame(
                canonical,
                2,
                2,
                if bits == 8 {
                    selected
                        .into_iter()
                        .flatten()
                        .map(|v| u8::try_from(v).unwrap())
                        .collect()
                } else {
                    selected
                        .into_iter()
                        .flatten()
                        .flat_map(u16::to_le_bytes)
                        .collect()
                },
            );
            assert_eq!(cropped.pixel_format, canonical, "{format:08x}");
            assert_eq!(cropped.data, expected.data, "{format:08x}");
            assert_eq!(
                (cropped.id, cropped.device_ts_ns, cropped.wall_ms),
                (source.id, source.device_ts_ns, source.wall_ms)
            );
            let meter = crate::auto::meter(&expected).unwrap();
            assert_eq!(crate::auto::meter(&cropped), Some(meter), "{format:08x}");
            let measured = measure(&source, CameraKind::AreaScan, [1, 1, 2, 2]);
            assert_eq!(
                measured.auto_meter.unwrap().mean,
                round4(meter.mean),
                "{format:08x}"
            );
            source.data.make_mut().pop();
            assert!(crop(&source, [1, 1, 2, 2]).is_err(), "{format:08x}");
        }
    }

    #[test]
    fn mono8_measures_exposure_meter_and_focus_of_the_region() {
        let source = frame(MONO8, 128, 64, ramp(128, 64));
        let measured = measure(&source, CameraKind::AreaScan, [0, 0, 128, 64]);
        let exposure = measured.exposure.as_ref().unwrap();
        let expected = source.data.iter().map(|&v| f64::from(v)).sum::<f64>()
            / f64::from(u32::try_from(source.data.len()).unwrap())
            / 255.0;
        assert!((exposure.mean - expected).abs() < 0.01, "{exposure:?}");
        assert!(exposure.p01 < exposure.p50 && exposure.p50 < exposure.p99);
        assert_eq!(exposure.histogram16.iter().sum::<u32>(), 128 * 64);
        assert!(exposure.channels.is_none());
        let meter = measured.auto_meter.unwrap();
        assert!((meter.mean - expected).abs() < 0.02, "{meter:?}");
        assert!(measured.focus.unwrap().tenengrad > 0.0);
        // The dark left half measures darker and gets an advisory note.
        let left = measure(&source, CameraKind::AreaScan, [0, 0, 32, 64]);
        let dark = left.exposure.as_ref().unwrap();
        assert!(dark.mean < exposure.mean);
        assert!(
            left.notes.iter().any(|note| note.starts_with("dark:")),
            "{:?}",
            left.notes
        );
        assert!(
            left.notes
                .iter()
                .any(|note| note.starts_with("shadows crushed"))
        );
        // A uniform frame has no edges to focus on.
        let flat = frame(MONO8, 32, 32, vec![120; 1024]);
        let flat = measure(&flat, CameraKind::Unknown, [0, 0, 32, 32]);
        assert_eq!(flat.focus.unwrap().tenengrad, 0.0);
        assert!(flat.notes.is_empty(), "{:?}", flat.notes);
    }

    #[test]
    fn rgb8_reports_channel_means_and_clipping() {
        let pixels: Vec<u8> = (0..64 * 32).flat_map(|_| [255, 128, 0]).collect();
        let measured = measure(
            &frame(RGB8, 64, 32, pixels),
            CameraKind::AreaScan,
            [0, 0, 64, 32],
        );
        let exposure = measured.exposure.unwrap();
        let channels = exposure.channels.unwrap();
        assert!((channels.red - 1.0).abs() < 0.01, "{channels:?}");
        assert!((channels.green - 0.5).abs() < 0.01, "{channels:?}");
        assert!(channels.blue < 0.01, "{channels:?}");
        assert_eq!(exposure.highlights, 1.0);
        assert!(
            measured
                .notes
                .contains(&"highlights clipped: 100.0 % of the image at full scale".into()),
            "{:?}",
            measured.notes
        );
    }

    #[test]
    fn mono12_measures_after_shifting_and_meters_at_native_depth() {
        let level: u16 = 2048;
        let pixels = (0..64 * 64).flat_map(|_| level.to_le_bytes()).collect();
        let measured = measure(
            &frame(MONO12, 64, 64, pixels),
            CameraKind::AreaScan,
            [0, 0, 64, 64],
        );
        let exposure = measured.exposure.unwrap();
        assert!((exposure.mean - 128.0 / 255.0).abs() < 0.01, "{exposure:?}");
        let meter = measured.auto_meter.unwrap();
        assert!((meter.mean - 2048.0 / 4095.0).abs() < 0.001, "{meter:?}");
        assert!(measured.notes.is_empty(), "{:?}", measured.notes);
    }

    #[test]
    fn scalar_images_and_unknown_formats_explain_missing_measurements() {
        let depth = frame(COORD3D_C16, 16, 16, vec![0x10; 16 * 16 * 2]);
        let measured = measure(&depth, CameraKind::Depth, [0, 0, 16, 16]);
        assert!(measured.exposure.is_none() && measured.focus.is_none());
        assert!(measured.auto_meter.is_none());
        assert_eq!(
            measured.notes,
            [
                "Coord3D_C16 contains measurement samples. Exposure and focus require an intensity image."
            ]
        );
        for kind in [CameraKind::Thermal, CameraKind::LineScan] {
            let measured = measure(&frame(MONO8, 16, 16, vec![9; 256]), kind, [0, 0, 16, 16]);
            assert!(
                measured.exposure.is_none() && measured.focus.is_none(),
                "{kind:?}"
            );
            assert_eq!(measured.notes.len(), 1);
        }
        // Depth previews still render, as gray.
        let image = render(&depth, 64, PreviewFormat::Png).unwrap().unwrap();
        assert_eq!(
            (image.mime, image.width, image.height),
            ("image/png", 16, 16)
        );
        let unknown = frame(0x0110_0099, 4, 4, vec![0; 32]);
        let measured = measure(&unknown, CameraKind::AreaScan, [1, 1, 2, 2]);
        assert!(measured.exposure.is_none());
        assert!(measured.notes[0].starts_with("0x01100099 frames cannot be decoded"));
        assert!(render(&unknown, 64, PreviewFormat::Auto).unwrap().is_none());
        // A truncated frame is a note, not a failed inspection.
        let short = measure(
            &frame(MONO8, 8, 8, vec![0; 10]),
            CameraKind::AreaScan,
            [0, 0, 4, 4],
        );
        assert!(
            short.notes[0].starts_with("not measured"),
            "{:?}",
            short.notes
        );
    }

    #[test]
    fn previews_average_areas_instead_of_dropping_pixels() {
        // Decimating a one-pixel checkerboard by two keeps only black or
        // white; averaging gives the mid gray the eye sees.
        let checker: Vec<u8> = (0..8 * 8)
            .map(|i| if (i % 8 + i / 8) % 2 == 0 { 0 } else { 255 })
            .collect();
        assert!(
            area_average(&checker, 8, 8, 1, 4, 4)
                .iter()
                .all(|&v| v == 128)
        );
        // Uneven ratios assign every source pixel to one output pixel.
        let ramp: Vec<u8> = (0..10).flat_map(|x| [x * 10, 0, 255]).collect();
        assert_eq!(
            area_average(&ramp, 10, 1, 3, 3, 1),
            &[15, 0, 255, 50, 0, 255, 80, 0, 255][..]
        );
        // An image that keeps its size is not copied.
        let same = area_average(&checker, 8, 8, 1, 8, 8);
        assert!(matches!(same, Cow::Borrowed(_)));
        assert_eq!(same, checker);
        assert_eq!(fit(1920, 1080, 1024), (1024, 576));
        assert_eq!(fit(480, 640, 64), (48, 64));
        assert_eq!(fit(100, 50, 1024), (100, 50));
        assert_eq!(fit(4000, 1, 64), (64, 1));
    }

    #[test]
    fn previews_fit_the_requested_edge_in_gray_or_colour() {
        let mono = frame(MONO8, 300, 200, ramp(300, 200));
        let pixels = (0..300 * 200)
            .flat_map(|i| [u8::try_from(i % 256).unwrap(), 64, 200])
            .collect();
        let color = frame(RGB8, 300, 200, pixels);
        let bayer = frame(BAYER_RG8, 300, 200, ramp(300, 200));
        for (source, channels) in [(&mono, 1), (&color, 3), (&bayer, 3)] {
            let image = render(source, 64, PreviewFormat::Png).unwrap().unwrap();
            assert_eq!((image.width, image.height), (64, 43));
            let decoder = png::Decoder::new(&*image.bytes);
            let reader = decoder.read_info().unwrap();
            assert_eq!(reader.info().color_type.samples(), channels);
            assert_eq!((reader.info().width, reader.info().height), (64, 43));
        }
        // Smaller frames are never enlarged; 0 asks for no preview.
        let image = render(&mono, 2048, PreviewFormat::Png).unwrap().unwrap();
        assert_eq!((image.width, image.height), (300, 200));
        assert!(render(&mono, 0, PreviewFormat::Auto).unwrap().is_none());
        let json = serde_json::to_value(&image).unwrap();
        assert_eq!(json["bytes"], image.bytes.len());
        assert_eq!(json["mime"], "image/png");
        use base64::Engine as _;
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(json["base64"].as_str().unwrap())
                .unwrap(),
            image.bytes
        );
    }

    #[test]
    fn eight_bit_gray_and_rgb8_previews_read_the_frame_in_place() {
        let gray = ramp(64, 48);
        let mono = render(&frame(MONO8, 64, 48, gray.clone()), 32, PreviewFormat::Png)
            .unwrap()
            .unwrap();
        // Coord3D_C8 and Confidence8 are their own gray samples, as Mono8 is.
        for format in [MONO8, 0x0108_00b1, 0x0108_00c6] {
            let source = frame(format, 64, 48, gray.clone());
            let view = FrameRef::whole(&source);
            assert!(frame::monochrome(format), "{format:x}");
            let pixels = shrunk(view, frame::Channels::Gray, 64, 48).unwrap();
            assert!(matches!(pixels, frame::Samples::Borrowed(_)), "{format:x}");
            let image = render(&source, 32, PreviewFormat::Png).unwrap().unwrap();
            assert_eq!(image.bytes, mono.bytes, "{format:x}");
        }
        let rgb = frame(RGB8, 4, 3, (0..36).collect());
        assert!(matches!(
            shrunk(FrameRef::whole(&rgb), frame::Channels::Rgb, 4, 3).unwrap(),
            frame::Samples::Borrowed(_)
        ));
        let wide = frame(MONO12, 4, 3, vec![0; 24]);
        assert!(matches!(
            shrunk(FrameRef::whole(&wide), frame::Channels::Gray, 4, 3).unwrap(),
            frame::Samples::Converted(_)
        ));
    }

    #[test]
    fn auto_previews_are_png_without_libjpeg_turbo() {
        assert!(!jpeg_preview(PreviewFormat::Auto, false).unwrap());
        assert!(!jpeg_preview(PreviewFormat::Png, true).unwrap());
        assert!(jpeg_preview(PreviewFormat::Auto, true).unwrap());
        let refused = jpeg_preview(PreviewFormat::Jpeg, false).unwrap_err();
        assert_eq!(crate::cli::error_code(&refused).1, 5, "{refused}");
        let mono = frame(MONO8, 64, 64, ramp(64, 64));
        let image = render_within(
            FrameRef::whole(&mono),
            64,
            PreviewFormat::Auto,
            false,
            MAX_IMAGE_BYTES,
            false,
        )
        .unwrap()
        .unwrap();
        assert!(image.bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(image.mime, "image/png");
    }

    #[cfg(feature = "jpeg")]
    #[test]
    fn jpeg_previews_are_gray_for_mono_and_step_down_to_fit_the_size_limit() {
        let mono = frame(MONO8, 600, 400, ramp(600, 400));
        let image = render(&mono, 256, PreviewFormat::Auto).unwrap().unwrap();
        assert_eq!(
            (image.mime, image.width, image.height),
            ("image/jpeg", 256, 171)
        );
        assert!(crate::jpeg::well_formed(&image.bytes, 256, 171));
        let sof = image
            .bytes
            .windows(2)
            .position(|w| w == [0xFF, 0xC0])
            .unwrap();
        assert_eq!(
            image.bytes[sof + 9],
            1,
            "monochrome previews are gray JPEGs"
        );
        // Noise defeats compression, so a tight limit steps quality, then size.
        let noisy = frame(RGB8, 256, 256, noise(256 * 256 * 3));
        let full = render_within(
            FrameRef::whole(&noisy),
            256,
            PreviewFormat::Jpeg,
            true,
            usize::MAX,
            false,
        )
        .unwrap()
        .unwrap();
        let lower = render_within(
            FrameRef::whole(&noisy),
            256,
            PreviewFormat::Jpeg,
            true,
            full.bytes.len() - 1,
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (lower.width, lower.height),
            (256, 256),
            "quality drops first"
        );
        assert!(lower.bytes.len() < full.bytes.len());
        let smaller = render_within(
            FrameRef::whole(&noisy),
            256,
            PreviewFormat::Jpeg,
            true,
            full.bytes.len() / 3,
            false,
        )
        .unwrap()
        .unwrap();
        assert!(
            smaller.width < 256 && smaller.width >= 131,
            "{}",
            smaller.width
        );
        assert!(smaller.bytes.len() <= full.bytes.len() / 3);
        assert!(crate::jpeg::well_formed(
            &smaller.bytes,
            smaller.width,
            smaller.height
        ));
        let error = render_within(
            FrameRef::whole(&noisy),
            256,
            PreviewFormat::Jpeg,
            true,
            100,
            false,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "preview image exceeds the size limit; request a smaller max_dimension"
        );
    }

    #[test]
    fn png_previews_shrink_to_fit_the_size_limit() {
        let noisy = frame(MONO8, 200, 200, noise(200 * 200));
        let full = render_within(
            FrameRef::whole(&noisy),
            200,
            PreviewFormat::Png,
            true,
            usize::MAX,
            false,
        )
        .unwrap()
        .unwrap();
        let smaller = render_within(
            FrameRef::whole(&noisy),
            200,
            PreviewFormat::Png,
            true,
            full.bytes.len() - 1,
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!((smaller.width, smaller.height), (160, 160));
        render_within(
            FrameRef::whole(&noisy),
            200,
            PreviewFormat::Png,
            true,
            64,
            false,
        )
        .unwrap_err();
    }

    #[test]
    fn inspections_report_the_region_they_measured() {
        let source = frame(BAYER_RG8, 640, 480, ramp(640, 480));
        let inspection = inspect(
            &source,
            CameraKind::AreaScan,
            Some([0.1, 0.1, 0.25, 0.25]),
            64,
            PreviewFormat::Png,
        )
        .unwrap();
        assert_eq!(
            inspection.region,
            Region {
                x: 64,
                y: 48,
                width: 160,
                height: 120,
                normalized: [0.1, 0.1, 0.25, 0.25]
            }
        );
        let image = inspection.image.as_ref().unwrap();
        assert_eq!((image.width, image.height), (64, 48));
        let json = serde_json::to_value(&inspection).unwrap();
        for key in [
            "region",
            "exposure",
            "auto_meter",
            "focus",
            "notes",
            "image",
        ] {
            assert!(json.get(key).is_some(), "{key} in {json}");
        }
        assert!(json["focus"]["tenengrad"].as_f64().unwrap() > 0.0);
        assert!(json["exposure"]["channels"]["red"].is_number());
        assert_eq!(
            json["exposure"]["histogram16"].as_array().unwrap().len(),
            16
        );
        let whole = inspect(&source, CameraKind::AreaScan, None, 0, PreviewFormat::Auto).unwrap();
        assert_eq!(whole.region.normalized, [0.0, 0.0, 1.0, 1.0]);
        assert!(whole.image.is_none());
        assert_eq!(
            serde_json::to_value(&whole).unwrap()["image"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn measurements_serialize_with_at_most_four_decimals() {
        fn numbers(value: &serde_json::Value, found: &mut Vec<String>) {
            match value {
                serde_json::Value::Number(number) => found.push(number.to_string()),
                serde_json::Value::Array(items) => {
                    for item in items {
                        numbers(item, found);
                    }
                }
                serde_json::Value::Object(fields) => {
                    for field in fields.values() {
                        numbers(field, found);
                    }
                }
                _ => {}
            }
        }
        let source = frame(BAYER_RG8, 640, 480, ramp(640, 480));
        let json =
            serde_json::to_value(measure(&source, CameraKind::AreaScan, [0, 0, 640, 480])).unwrap();
        let mut found = Vec::new();
        for key in ["exposure", "auto_meter", "focus"] {
            numbers(&json[key], &mut found);
        }
        assert!(found.len() > 30, "{json}");
        for number in found {
            let decimals = number.split_once('.').map_or(0, |(_, digits)| digits.len());
            assert!(decimals <= 4 && !number.contains('e'), "{number} in {json}");
        }
    }

    #[test]
    fn only_small_region_crops_get_the_higher_jpeg_quality() {
        assert_eq!(first_quality(false, 1920, 1080), 85);
        assert_eq!(first_quality(false, 256, 256), 85);
        assert_eq!(first_quality(true, 512, 300), 92);
        assert_eq!(first_quality(true, 513, 300), 85);
    }

    #[cfg(feature = "jpeg")]
    #[test]
    fn downscaled_whole_frames_are_encoded_at_the_default_quality() {
        let noisy = frame(RGB8, 1024, 512, noise(1024 * 512 * 3));
        let encoded = |source: &Frame, quality| {
            let (width, height) = (256, 128);
            let pixels = area_average(
                &frame::rgb(source).unwrap(),
                source.width,
                source.height,
                3,
                width,
                height,
            )
            .into_owned();
            crate::jpeg::software_quality(&Raster::of(width, height, 3, &pixels), quality).unwrap()
        };
        // A whole frame previewed at 256 px is downscaled, not cropped.
        let whole = inspect(&noisy, CameraKind::AreaScan, None, 256, PreviewFormat::Jpeg)
            .unwrap()
            .image
            .unwrap();
        assert_eq!(whole.bytes, encoded(&noisy, 85));
        // A region of at most 512 px is a crop.
        let region = [0.0, 0.0, 0.25, 0.25];
        let crop = inspect(
            &noisy,
            CameraKind::AreaScan,
            Some(region),
            256,
            PreviewFormat::Jpeg,
        )
        .unwrap()
        .image
        .unwrap();
        let view = super::crop(&noisy, region_pixels(region, 1024, 512)).unwrap();
        assert_eq!(crop.bytes, encoded(&view, 92));
    }

    /// Regions are measured and previewed in place exactly as their crops
    /// were, also at sizes where previews are shrunk in parallel bands.
    #[test]
    fn regions_read_in_place_measure_and_preview_as_their_crops() {
        for format in [
            0x010c_0059,
            0x0108_0009,
            0x0110_0011,
            0x010c_0047,
            RGB8,
            MONO12,
        ] {
            let (width, height) = (1600, 1100);
            let source = frame(
                format,
                width,
                height,
                noise(frame::row_bytes(width, format) * height as usize),
            );
            for region in [
                [0, 0, width, height],
                [3, 5, 1400, 900],
                [801, 1, 799, 1099],
            ] {
                let rect = crop_rect(&source, region);
                let view = FrameRef::region(&source, rect);
                let crop = crop(&source, rect).unwrap();
                let whole = FrameRef::whole(&crop);
                let label = format!("{format:08x} {rect:?}");
                assert_eq!(
                    frame::exposure_in(view).unwrap(),
                    frame::exposure(&crop).unwrap(),
                    "{label}"
                );
                assert_eq!(
                    frame::clipping_in(view).unwrap(),
                    frame::clipping(&crop).unwrap(),
                    "{label}"
                );
                assert_eq!(
                    crate::auto::meter_in(view),
                    crate::auto::meter(&crop),
                    "{label}"
                );
                for edge in [1024, 256] {
                    let preview = |view| {
                        render_within(view, edge, PreviewFormat::Png, true, usize::MAX, true)
                            .unwrap()
                            .unwrap()
                    };
                    assert_eq!(preview(view).bytes, preview(whole).bytes, "{label} {edge}");
                }
            }
        }
    }
}
