//! Port of the native `src/inspect.rs` measurement path: request checks,
//! normalized regions, Bayer-safe crops, exposure summaries, the
//! auto-exposure meter, focus scores and the advisory notes. The preview
//! image is not ported (the browser draws its own), so `inspect` always
//! answers `image: null`, as the native code does for `max_dimension` 0.
use crate::Result;
use crate::auto;
use crate::frame;
use crate::json::Json;
use crate::types::{CameraKind, Frame};
use std::borrow::Cow;

/// Clipping above this share of samples is called out, as on the desktop
/// histogram.
const CLIP_WARN: f64 = 0.005;
/// Mean brightness outside this range is called out.
const DARK: f64 = 0.25;
const BRIGHT: f64 = 0.65;
/// Normalized region sums may exceed 1 by float rounding (0.7 + 0.3).
const REGION_SLACK: f64 = 1e-9;

/// The region part of `inspect::validate`.
pub fn validate_region(region: [f64; 4]) -> Result<()> {
    let [x, y, width, height] = region;
    ensure!(
        region.iter().all(|v| v.is_finite())
            && x >= 0.0
            && y >= 0.0
            && width > 0.0
            && height > 0.0
            && x + width <= 1.0 + REGION_SLACK
            && y + height <= 1.0 + REGION_SLACK,
        "region must be x, y, width, height inside 0..1 with positive width and height"
    );
    Ok(())
}

/// A normalized `[x, y, width, height]` region in pixels of a `width` ×
/// `height` frame, clamped to the frame and at least one pixel on each side.
pub fn region_pixels(region: [f64; 4], width: u32, height: u32) -> [u32; 4] {
    let (w, h) = (f64::from(width), f64::from(height));
    let x = ((region[0] * w).round().max(0.0) as u32).min(width.saturating_sub(1));
    let y = ((region[1] * h).round().max(0.0) as u32).min(height.saturating_sub(1));
    [
        x,
        y,
        ((region[2] * w).round().max(1.0) as u32).min(width - x),
        ((region[3] * h).round().max(1.0) as u32).min(height - y),
    ]
}

/// A PFNC name such as `BayerRG8`, or the code in hex when it has none here.
pub fn pixel_format_label(pixel_format: u32) -> String {
    match frame::pixel_format_name(pixel_format) {
        "Unknown" => format!("0x{pixel_format:08x}"),
        name => name.into(),
    }
}

/// The pixels `crop` takes for `px`: clamped to the frame and, for Bayer
/// frames, widened to whole 2×2 quads on even coordinates.
pub fn crop_rect(frame: &Frame, px: [u32; 4]) -> [u32; 4] {
    let (fw, fh) = (frame.width, frame.height);
    if fw == 0 || fh == 0 {
        return [0, 0, fw, fh];
    }
    let x = px[0].min(fw - 1);
    let y = px[1].min(fh - 1);
    let width = px[2].clamp(1, fw - x);
    let height = px[3].clamp(1, fh - y);
    if !frame::is_bayer(frame.pixel_format) || fw < 2 || fh < 2 {
        return [x, y, width, height];
    }
    let (even_width, even_height) = (fw & !1, fh & !1);
    let x0 = (x & !1).min(even_width - 2);
    let y0 = (y & !1).min(even_height - 2);
    let x1 = (x + width).next_multiple_of(2).clamp(x0 + 2, even_width);
    let y1 = (y + height).next_multiple_of(2).clamp(y0 + 2, even_height);
    [x0, y0, x1 - x0, y1 - y0]
}

/// The `crop_rect` of `px` as a frame of its own, with the original's
/// identity, pixel format and timestamp.
pub fn crop(frame: &Frame, px: [u32; 4]) -> Result<Frame> {
    let bytes = frame::validated_bytes_per_pixel(frame)?;
    let [x, y, width, height] = crop_rect(frame, px);
    let stride = frame.width as usize * bytes;
    let row = width as usize * bytes;
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
        data,
        ..*frame
    })
}

/// The frame itself when `rect` covers it all, else its crop.
pub fn view(frame: &Frame, rect: [u32; 4]) -> Result<Cow<'_, Frame>> {
    if rect == [0, 0, frame.width, frame.height] || !frame::convertible(frame.pixel_format) {
        Ok(Cow::Borrowed(frame))
    } else {
        crop(frame, rect).map(Cow::Owned)
    }
}

/// Four decimals, ties away from zero.
pub fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

/// How a region is exposed, rounded to 4 decimals.
#[derive(Clone, Debug, PartialEq)]
pub struct ExposureSummary {
    pub mean: f64,
    pub p01: f64,
    pub p50: f64,
    pub p99: f64,
    pub shadows: f64,
    pub highlights: f64,
    pub histogram16: [u32; 16],
    pub channels: Option<Channels>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Channels {
    pub red: f64,
    pub green: f64,
    pub blue: f64,
}

/// `frame::Sharpness` rounded to 4 decimals.
#[derive(Clone, Copy, Debug, PartialEq)]
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
            let total: f64 = bins
                .iter()
                .enumerate()
                .map(|(i, &n)| (4.0 * i as f64 + 1.5) * f64::from(n))
                .sum();
            round4(total / f64::from(samples.max(1)) / 255.0)
        };
        let percentile = |q: f64| {
            let target = q * f64::from(samples);
            let mut seen = 0.0;
            for (i, &n) in exposure.luma.iter().enumerate() {
                let next = seen + f64::from(n);
                if n > 0 && next >= target {
                    let within = ((target - seen) / f64::from(n)).clamp(0.0, 1.0);
                    return round4((i as f64 + within) / 64.0);
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
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Measurements {
    pub exposure: Option<ExposureSummary>,
    pub auto_meter: Option<auto::Meter>,
    pub focus: Option<Focus>,
    pub notes: Vec<String>,
}

/// Measure the `px` region of `frame` (see `crop_rect`) from a camera of
/// `kind`.
pub fn measure(frame: &Frame, kind: CameraKind, region_px: [u32; 4]) -> Measurements {
    let rect = crop_rect(frame, region_px);
    match view(frame, rect) {
        Ok(view) => measure_view(frame, &view, kind, rect),
        Err(error) => Measurements {
            notes: vec![format!("not measured: {error}")],
            ..Measurements::default()
        },
    }
}

fn measure_view(frame: &Frame, view: &Frame, kind: CameraKind, rect: [u32; 4]) -> Measurements {
    let mut measured = Measurements::default();
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
    if !frame::convertible(frame.pixel_format) {
        measured.notes.push(format!(
            "{} frames cannot be decoded; exposure, focus and the preview are unavailable",
            pixel_format_label(frame.pixel_format)
        ));
        return measured;
    }
    match frame::exposure(view) {
        Ok(exposure) => measured.exposure = Some((&exposure).into()),
        Err(error) => measured
            .notes
            .push(format!("exposure not measured: {error}")),
    }
    measured.auto_meter = auto::meter(view).map(|meter| auto::Meter {
        mean: round4(meter.mean),
        p99: round4(meter.p99),
    });
    match frame::sharpness(frame, rect) {
        Ok(focus) => measured.focus = Some(focus.into()),
        Err(error) => measured.notes.push(format!("focus not measured: {error}")),
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

/// The pixels an inspection covered.
#[derive(Clone, Debug, PartialEq)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// The same rectangle as fractions of the frame.
    pub normalized: [f64; 4],
}

/// A frame region's measurements (`Inspection` without its preview).
#[derive(Clone, Debug, PartialEq)]
pub struct Inspection {
    pub region: Region,
    pub measurements: Measurements,
}

/// Measure the normalized `region` of `frame` (all of it when None). A crop
/// that cannot be taken fails the whole call, as natively.
pub fn inspect(frame: &Frame, kind: CameraKind, region: Option<[f64; 4]>) -> Result<Inspection> {
    let requested = region.map_or([0, 0, frame.width, frame.height], |region| {
        region_pixels(region, frame.width, frame.height)
    });
    let rect = crop_rect(frame, requested);
    let view = view(frame, rect)?;
    let measurements = measure_view(frame, &view, kind, rect);
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
    })
}

impl Measurements {
    /// The fields as native serde writes them, into an open object.
    pub fn write_fields(&self, json: &mut Json) {
        json.key("exposure");
        match &self.exposure {
            None => json.null(),
            Some(e) => {
                json.begin_object();
                json.field_f64("mean", e.mean);
                json.field_f64("p01", e.p01);
                json.field_f64("p50", e.p50);
                json.field_f64("p99", e.p99);
                json.field_f64("shadows", e.shadows);
                json.field_f64("highlights", e.highlights);
                json.key("histogram16");
                json.begin_array();
                for &n in &e.histogram16 {
                    json.u64(u64::from(n));
                }
                json.end_array();
                json.key("channels");
                match e.channels {
                    None => json.null(),
                    Some(c) => {
                        json.begin_object();
                        json.field_f64("red", c.red);
                        json.field_f64("green", c.green);
                        json.field_f64("blue", c.blue);
                        json.end_object();
                    }
                }
                json.end_object();
            }
        }
        json.key("auto_meter");
        match self.auto_meter {
            None => json.null(),
            Some(m) => {
                json.begin_object();
                json.field_f64("mean", m.mean);
                json.field_f64("p99", m.p99);
                json.end_object();
            }
        }
        json.key("focus");
        match self.focus {
            None => json.null(),
            Some(f) => {
                json.begin_object();
                json.field_f64("laplacian", f.laplacian);
                json.field_f64("tenengrad", f.tenengrad);
                json.field_f64("brenner", f.brenner);
                json.field_f64("variance", f.variance);
                json.end_object();
            }
        }
        json.key("notes");
        json.begin_array();
        for note in &self.notes {
            json.string(note);
        }
        json.end_array();
    }

    /// `serde_json::to_string(&measurements)`, up to float spelling.
    pub fn to_json(&self) -> String {
        let mut json = Json::default();
        json.begin_object();
        self.write_fields(&mut json);
        json.end_object();
        json.finish()
    }
}

impl Inspection {
    /// The native `Inspection` JSON with `image: null`.
    pub fn to_json(&self) -> String {
        let mut json = Json::default();
        json.begin_object();
        json.key("region");
        json.begin_object();
        json.field_u64("x", u64::from(self.region.x));
        json.field_u64("y", u64::from(self.region.y));
        json.field_u64("width", u64::from(self.region.width));
        json.field_u64("height", u64::from(self.region.height));
        json.key("normalized");
        json.begin_array();
        for &v in &self.region.normalized {
            json.f64(v);
        }
        json.end_array();
        json.end_object();
        self.measurements.write_fields(&mut json);
        json.key("image");
        json.null();
        json.end_object();
        json.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MONO8, RGB8};

    const BAYER_RG8: u32 = 0x0108_0009;
    const COORD3D_C16: u32 = 0x0110_00b8;

    fn frame(pixel_format: u32, width: u32, height: u32, data: Vec<u8>) -> Frame {
        Frame {
            id: 7,
            width,
            height,
            pixel_format,
            timestamp_ns: 99,
            data,
        }
    }

    fn ramp(width: u32, height: u32) -> Vec<u8> {
        (0..height)
            .flat_map(|_| {
                (0..width).map(move |x| {
                    let level = (x * 255 / (width - 1)) as u8;
                    if x % 8 < 2 { level / 2 } else { level }
                })
            })
            .collect()
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
        validate_region([0.7, 0.7, 0.3, 0.3]).unwrap();
        for region in [
            [0.5, 0.0, 0.6, 1.0],
            [0.0, 0.0, 0.0, 1.0],
            [-0.1, 0.0, 0.5, 0.5],
            [0.0, f64::NAN, 0.5, 0.5],
        ] {
            assert_eq!(
                validate_region(region).unwrap_err(),
                "region must be x, y, width, height inside 0..1 with positive width and height"
            );
        }
    }

    #[test]
    fn bayer_crops_keep_the_colour_filter_phase() {
        let source = frame(BAYER_RG8, 8, 6, (0..48).collect());
        assert_eq!(crop_rect(&source, [3, 1, 2, 2]), [2, 0, 4, 4]);
        assert_eq!(crop_rect(&source, [7, 5, 1, 1]), [6, 4, 2, 2]);
        let cropped = crop(&source, [3, 1, 2, 2]).unwrap();
        assert_eq!((cropped.width, cropped.height, cropped.id), (4, 4, 7));
        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(
                    cropped.data[(y * 4 + x) as usize],
                    source.data[(y * 8 + x + 2) as usize]
                );
            }
        }
        let mono = frame(MONO8, 8, 6, source.data.clone());
        assert_eq!(crop(&mono, [3, 1, 2, 2]).unwrap().data, [11, 12, 19, 20]);
    }

    /// measure.md §5 A: the native `rgb8_reports_channel_means_and_clipping`
    /// fixture, with every value derived by hand.
    #[test]
    fn rgb8_example_matches_the_hand_derivation() {
        let pixels: Vec<u8> = (0..64 * 32).flat_map(|_| [255, 128, 0]).collect();
        let measured = measure(
            &frame(RGB8, 64, 32, pixels),
            CameraKind::AreaScan,
            [0, 0, 64, 32],
        );
        let e = measured.exposure.as_ref().unwrap();
        assert_eq!(
            (e.mean, e.p01, e.p50, e.p99, e.shadows, e.highlights),
            (0.5686, 0.5627, 0.5703, 0.578, 0.0, 1.0)
        );
        assert_eq!(
            e.histogram16,
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 2048, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            e.channels,
            Some(Channels {
                red: 0.9941,
                green: 0.5078,
                blue: 0.0059
            })
        );
        assert_eq!(
            measured.auto_meter,
            Some(auto::Meter {
                mean: 0.501,
                p99: 1.0
            })
        );
        assert_eq!(
            measured.focus,
            Some(Focus {
                laplacian: 0.0,
                tenengrad: 0.0,
                brenner: 0.0,
                variance: 0.0
            })
        );
        assert_eq!(
            measured.notes,
            ["highlights clipped: 100.0 % of the image at full scale"]
        );
    }

    /// measure.md §5 B.
    #[test]
    fn four_pixel_example_matches_the_hand_derivation() {
        let measured = measure(
            &frame(
                RGB8,
                4,
                1,
                vec![0, 0, 0, 255, 255, 255, 128, 128, 128, 255, 0, 0],
            ),
            CameraKind::AreaScan,
            [0, 0, 4, 1],
        );
        let e = measured.exposure.as_ref().unwrap();
        assert_eq!(
            (e.mean, e.p01, e.p50, e.p99, e.shadows, e.highlights),
            (0.4275, 0.0006, 0.2188, 0.9994, 0.25, 0.5)
        );
        assert_eq!(
            e.histogram16,
            [1, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 1]
        );
        assert_eq!(
            e.channels,
            Some(Channels {
                red: 0.6255,
                green: 0.3784,
                blue: 0.3784
            })
        );
        assert_eq!(
            measured.auto_meter,
            Some(auto::Meter {
                mean: 0.438,
                p99: 1.0
            })
        );
        assert_eq!(measured.focus, None);
        assert_eq!(
            measured.notes,
            [
                "focus not measured: frame is too small to measure sharpness",
                "highlights clipped: 50.0 % of the image at full scale",
                "shadows crushed: 25.0 % of the image at black",
            ]
        );
    }

    #[test]
    fn mono8_measures_exposure_meter_and_focus_of_the_region() {
        let source = frame(MONO8, 128, 64, ramp(128, 64));
        let measured = measure(&source, CameraKind::AreaScan, [0, 0, 128, 64]);
        let exposure = measured.exposure.as_ref().unwrap();
        let expected = source.data.iter().map(|&v| f64::from(v)).sum::<f64>()
            / source.data.len() as f64
            / 255.0;
        assert!((exposure.mean - expected).abs() < 0.01, "{exposure:?}");
        assert!(exposure.channels.is_none());
        assert!(measured.focus.unwrap().tenengrad > 0.0);
        let left = measure(&source, CameraKind::AreaScan, [0, 0, 32, 64]);
        assert!(left.notes.iter().any(|note| note.starts_with("dark:")));
        assert!(
            left.notes
                .iter()
                .any(|note| note.starts_with("shadows crushed"))
        );
        let flat = measure(
            &frame(MONO8, 32, 32, vec![120; 1024]),
            CameraKind::Unknown,
            [0, 0, 32, 32],
        );
        assert_eq!(flat.focus.unwrap().tenengrad, 0.0);
        assert!(flat.notes.is_empty(), "{:?}", flat.notes);
    }

    #[test]
    fn scalar_images_and_unknown_formats_explain_missing_measurements() {
        let depth = frame(COORD3D_C16, 16, 16, vec![0x10; 512]);
        let measured = measure(&depth, CameraKind::Depth, [0, 0, 16, 16]);
        assert_eq!(
            measured.notes,
            ["Depth images hold distances, not brightness; exposure and focus are not measured"]
        );
        let unknown = frame(0x0110_0099, 4, 4, vec![0; 32]);
        let measured = measure(&unknown, CameraKind::AreaScan, [1, 1, 2, 2]);
        assert!(measured.notes[0].starts_with("0x01100099 frames cannot be decoded"));
        let short = measure(
            &frame(MONO8, 8, 8, vec![0; 10]),
            CameraKind::AreaScan,
            [0, 0, 4, 4],
        );
        assert_eq!(short.notes, ["not measured: truncated 8-bit scalar frame"]);
    }

    #[test]
    fn inspections_report_the_region_they_measured() {
        let source = frame(BAYER_RG8, 640, 480, ramp(640, 480));
        let inspection =
            inspect(&source, CameraKind::AreaScan, Some([0.1, 0.1, 0.25, 0.25])).unwrap();
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
        let whole = inspect(&source, CameraKind::AreaScan, None).unwrap();
        assert_eq!(whole.region.normalized, [0.0, 0.0, 1.0, 1.0]);
        let json = whole.to_json();
        assert!(json.starts_with("{\"region\":{\"x\":0,\"y\":0,\"width\":640,\"height\":480,\"normalized\":[0.0,0.0,1.0,1.0]},\"exposure\":{"), "{json}");
        assert!(json.ends_with(",\"image\":null}"), "{json}");
        let truncated = frame(MONO8, 8, 8, vec![0; 10]);
        assert_eq!(
            inspect(&truncated, CameraKind::AreaScan, Some([0.0, 0.0, 0.5, 0.5])).unwrap_err(),
            "truncated 8-bit scalar frame"
        );
    }

    #[test]
    fn notes_round_ties_to_even_like_rust_formatting() {
        // 0.125 is an exact binary tie: Rust prints 0.12 where JavaScript's
        // toFixed would print 0.13, which is why notes are built here.
        assert_eq!(format!("{:.2}", 0.125), "0.12");
        assert_eq!(format!("{:.1} %", 0.0625 * 100.0), "6.2 %");
    }
}
