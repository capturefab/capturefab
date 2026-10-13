//! Camera-specific display transforms. These never change acquired or saved bytes.
use crate::{camera_profile::CameraKind, frame, genicam::FeatureInfo, types::Frame};
use anyhow::{Result, ensure};
use iced::Size;
use std::sync::Arc;

const HISTORY_ROWS: usize = 512;
const HISTORY_WIDTH: u32 = 2048;

pub(super) fn present(
    gpu: &super::gpu::GpuFrames,
    shown: &mut Option<super::preview::Shown>,
    state: &mut CameraView,
    context: (CameraKind, &[FeatureInfo]),
    key: &str,
    source: Arc<Frame>,
    limits: (u32, u32),
) -> Result<()> {
    let (kind, features) = context;
    if let Some(display) = state.transform(kind, &source, features)? {
        gpu.retire(key);
        *shown = Some(super::preview::Shown::Image {
            handle: iced::widget::image::Handle::from_rgba(
                display.width,
                display.height,
                display.rgba,
            ),
            size: display.native,
        });
        return Ok(());
    }
    super::preview::present(gpu, shown, key, source, |source| {
        frame::preview_rgba(source, limits.0, limits.1)
    })
}

#[derive(Default)]
pub(super) struct CameraView {
    pub sensor: bool,
    pub focus_open: Option<bool>,
    pub legend: Option<String>,
    lines: LineHistory,
}

impl CameraView {
    pub fn label(&self, kind: CameraKind) -> &'static str {
        if self.sensor || (kind != CameraKind::LineScan && self.legend.is_none()) {
            return "Sensor pixels";
        }
        match kind {
            CameraKind::LineScan => "Recent lines",
            CameraKind::Depth => "Scalar colors",
            CameraKind::Thermal => "Heatmap",
            _ => "Sensor pixels",
        }
    }

    pub fn toggle(&mut self) {
        self.sensor = !self.sensor;
        self.lines = LineHistory::default();
        self.legend = None;
    }

    /// Returns a CPU display image and its placement size, or None for the
    /// ordinary GPU/CPU sensor preview. Unsupported depth formats stay on
    /// the ordinary path, which explains the format and offers raw capture.
    pub fn transform(
        &mut self,
        kind: CameraKind,
        source: &Frame,
        features: &[FeatureInfo],
    ) -> Result<Option<DisplayImage>> {
        self.legend = None;
        if self.sensor {
            if source.pixel_format == 0x0120_00bf {
                let (image, low, high) =
                    scalar_image(source, true, false, invalid_sample(source, features))?;
                self.legend = Some(format!("Auto grayscale range: raw {low:.1}–{high:.1}"));
                return Ok(Some(image));
            }
            return Ok(None);
        }
        match kind {
            CameraKind::LineScan => {
                let recent = recent_lines(source)?;
                let (w, h, rgba) = frame::preview_rgba(
                    recent.as_ref().unwrap_or(source),
                    HISTORY_WIDTH,
                    HISTORY_ROWS as u32,
                )?;
                let image = self.lines.push(source, w, h, rgba)?;
                self.legend =
                    Some("Recent received lines · gaps mark skipped blocks · display only".into());
                Ok(Some(image))
            }
            CameraKind::Depth | CameraKind::Thermal
                if scalar_bytes(source.pixel_format).is_some() =>
            {
                let (image, low, high) = scalar_image(
                    source,
                    kind == CameraKind::Depth,
                    true,
                    invalid_sample(source, features),
                )?;
                self.legend = Some(format!(
                    "Auto color range: raw {low:.1}–{high:.1} · {} · save raw for measurements",
                    if kind == CameraKind::Depth {
                        "low → high"
                    } else {
                        "cool → warm"
                    }
                ));
                Ok(Some(image))
            }
            _ => Ok(None),
        }
    }
}

/// Keep recent rows before downsampling, so a tall acquired block does not
/// destroy horizontal detail merely to fit the history's vertical budget.
fn recent_lines(source: &Frame) -> Result<Option<Frame>> {
    if source.height <= HISTORY_ROWS as u32 {
        return Ok(None);
    }
    ensure!(
        frame::convertible(source.pixel_format),
        "unsupported line preview pixel format; capture with --format raw to preserve bytes"
    );
    let pixels = (source.width as usize).checked_mul(source.height as usize);
    ensure!(
        pixels.is_some_and(|n| n > 0 && n <= 64 * 1024 * 1024),
        "image dimensions exceed conversion limit"
    );
    let bytes = ((source.pixel_format >> 16) & 0xff) as usize / 8;
    let stride = source.width as usize * bytes;
    ensure!(
        source.data.len() >= pixels.unwrap() * bytes,
        "truncated line frame"
    );
    let mut start = source.height as usize - HISTORY_ROWS;
    // The first retained Bayer row must keep its original CFA parity.
    if matches!(source.pixel_format, 0x0108_0008..=0x0108_000b) {
        start = start.next_multiple_of(2);
    }
    Ok(Some(Frame {
        id: source.id,
        width: source.width,
        height: source.height - start as u32,
        pixel_format: source.pixel_format,
        timestamp_ns: source.timestamp_ns,
        data: source.data[start * stride..source.height as usize * stride].to_vec(),
    }))
}

pub(super) struct DisplayImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub native: Size,
}

#[derive(Default)]
struct LineHistory {
    shape: Option<(u32, u32, u32)>,
    last: Option<(u64, u64)>,
    rows: Vec<u8>,
}

impl LineHistory {
    fn push(
        &mut self,
        source: &Frame,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    ) -> Result<DisplayImage> {
        let stride = width as usize * 4;
        ensure!(
            width > 0 && height > 0 && rgba.len() == stride * height as usize,
            "invalid line preview dimensions"
        );
        let shape = (source.width, source.height, source.pixel_format);
        if self.shape != Some(shape)
            || self
                .last
                .is_some_and(|(id, stamp)| source.id < id || source.timestamp_ns < stamp)
        {
            self.rows.clear();
            self.last = None;
        }
        self.shape = Some(shape);
        // Shared memory is newest-frame-wins: make skipped blocks visible,
        // never suggest this display is a complete encoder-aligned image.
        if self
            .last
            .is_none_or(|(id, stamp)| (id, stamp) != (source.id, source.timestamp_ns))
        {
            if self
                .last
                .is_some_and(|(id, _)| source.id != id.saturating_add(1))
            {
                self.rows.extend(std::iter::repeat_n(0, stride));
            }
            self.rows.extend_from_slice(&rgba);
        }
        self.last = Some((source.id, source.timestamp_ns));
        let keep = stride * HISTORY_ROWS;
        if self.rows.len() > keep {
            self.rows.drain(..self.rows.len() - keep);
        }
        let rows = (self.rows.len() / stride) as u32;
        // A stable canvas avoids zoom jumping as history fills.
        let mut display = vec![0; keep];
        let offset = stride * (HISTORY_ROWS - rows as usize);
        display[offset..].copy_from_slice(&self.rows);
        Ok(DisplayImage {
            width,
            height: HISTORY_ROWS as u32,
            rgba: display,
            native: Size::new(width as f32, HISTORY_ROWS as f32),
        })
    }
}

fn scalar_bytes(format: u32) -> Option<usize> {
    match format {
        crate::types::MONO8 | 0x0108_00b1 | 0x0108_00c6 => Some(1),
        0x0110_0007 | 0x0110_00b8 | 0x0110_00c7 => Some(2),
        0x0120_00bf => Some(4),
        _ => None,
    }
}

fn invalid_sample(source: &Frame, features: &[FeatureInfo]) -> Option<f32> {
    if !matches!(source.pixel_format, 0x0108_00b1 | 0x0110_00b8 | 0x0120_00bf) {
        return None;
    }
    let value = |name: &str| {
        features
            .iter()
            .find(|f| f.name == name)
            .and_then(|f| f.value.as_ref())
    };
    // Coordinate-dependent values describe C only when that coordinate is
    // selected. Do not change the camera's selectors just to draw a preview.
    if value("Scan3dCoordinateSelector")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|v| v != "CoordinateC")
    {
        return None;
    }
    if value("Scan3dInvalidDataFlag").and_then(serde_json::Value::as_bool) != Some(true) {
        return None;
    }
    value("Scan3dInvalidDataValue")
        .and_then(serde_json::Value::as_f64)
        .map(|v| v as f32)
        .filter(|v| v.is_finite())
}

fn scalar_image(
    source: &Frame,
    depth: bool,
    colorized: bool,
    invalid: Option<f32>,
) -> Result<(DisplayImage, f32, f32)> {
    let bytes = scalar_bytes(source.pixel_format).expect("checked scalar format");
    let pixels = (source.width as usize).checked_mul(source.height as usize);
    ensure!(
        pixels.is_some_and(|n| n > 0 && n <= 64 * 1024 * 1024),
        "image dimensions exceed conversion limit"
    );
    ensure!(
        source.data.len() >= pixels.unwrap() * bytes,
        "truncated scalar frame"
    );
    let sample = |i: usize| {
        let offset = i * bytes;
        match bytes {
            1 => source.data[offset] as f32,
            2 => u16::from_le_bytes(source.data[offset..offset + 2].try_into().unwrap()) as f32,
            _ => f32::from_le_bytes(source.data[offset..offset + 4].try_into().unwrap()),
        }
    };
    let step = source
        .width
        .div_ceil(2048)
        .max(source.height.div_ceil(1536))
        .max(1);
    let (width, height) = (source.width.div_ceil(step), source.height.div_ceil(step));
    let values: Vec<f32> = (0..source.height)
        .step_by(step as usize)
        .flat_map(|y| {
            (0..source.width)
                .step_by(step as usize)
                .map(move |x| sample((y * source.width + x) as usize))
        })
        .collect();
    // Zero may be a valid coordinate. Honor only advertised invalid data;
    // never guess a sentinel from pixel values or bit depth.
    let (mut low, mut high) = (f32::INFINITY, f32::NEG_INFINITY);
    for &value in &values {
        if value.is_finite() && Some(value) != invalid {
            low = low.min(value);
            high = high.max(value);
        }
    }
    if !low.is_finite() {
        low = 0.0;
        high = 0.0;
    }
    let span = (high - low).max(f32::EPSILON);
    let rgba = values
        .into_iter()
        .flat_map(|value| {
            if !value.is_finite() || Some(value) == invalid {
                return [0, 0, 0, 255];
            }
            let t = ((value - low) / span).clamp(0.0, 1.0);
            let [r, g, b] = if colorized {
                color(t, depth)
            } else {
                [(t * 255.0).round() as u8; 3]
            };
            [r, g, b, 255]
        })
        .collect();
    Ok((
        DisplayImage {
            width,
            height,
            rgba,
            native: Size::new(source.width as f32, source.height as f32),
        },
        low,
        high,
    ))
}

fn color(t: f32, depth: bool) -> [u8; 3] {
    let stops = if depth {
        [
            [32.0, 80.0, 220.0],
            [25.0, 205.0, 205.0],
            [250.0, 220.0, 65.0],
        ]
    } else {
        [
            [35.0, 20.0, 100.0],
            [210.0, 55.0, 65.0],
            [255.0, 235.0, 125.0],
        ]
    };
    let (index, mix) = if t < 0.5 {
        (0, t * 2.0)
    } else {
        (1, (t - 0.5) * 2.0)
    };
    std::array::from_fn(|c| {
        (stops[index][c] * (1.0 - mix) + stops[index + 1][c] * mix).round() as u8
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn line(id: u64, width: u32, value: u8) -> Frame {
        Frame {
            id,
            width,
            height: 1,
            pixel_format: crate::types::MONO8,
            timestamp_ns: id,
            data: vec![value; width as usize],
        }
    }

    #[test]
    fn line_history_keeps_rows_marks_loss_and_resets_on_geometry_changes() {
        let mut view = CameraView::default();
        view.transform(CameraKind::LineScan, &line(1, 2, 40), &[])
            .unwrap();
        let image = view
            .transform(CameraKind::LineScan, &line(3, 2, 80), &[])
            .unwrap()
            .unwrap();
        assert_eq!(
            &image.rgba[image.rgba.len() - 24..image.rgba.len() - 16],
            &[40, 40, 40, 255, 40, 40, 40, 255]
        );
        assert_eq!(
            &image.rgba[image.rgba.len() - 16..image.rgba.len() - 8],
            &[0; 8]
        );
        assert_eq!(view.lines.rows.len(), 24);
        view.transform(CameraKind::LineScan, &line(3, 2, 80), &[])
            .unwrap();
        assert_eq!(
            view.lines.rows.len(),
            24,
            "re-rendering a stopped block cannot invent loss"
        );
        view.transform(CameraKind::LineScan, &line(4, 3, 20), &[])
            .unwrap();
        assert_eq!(view.lines.rows.len(), 12);
        for id in 5..600 {
            view.transform(CameraKind::LineScan, &line(id, 3, 20), &[])
                .unwrap();
        }
        assert_eq!(view.lines.rows.len(), 3 * 4 * HISTORY_ROWS);
    }

    #[test]
    fn scalar_colors_use_full_precision_and_leave_source_untouched() {
        let source = Frame {
            width: 3,
            height: 1,
            pixel_format: 0x0110_00b8,
            data: [1000u16, 1001, 1002]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect(),
            ..Frame::default()
        };
        let original = source.data.clone();
        let (image, low, high) = scalar_image(&source, true, true, None).unwrap();
        assert_eq!((low, high), (1000.0, 1002.0));
        assert_ne!(image.rgba[..4], image.rgba[4..8]);
        assert_eq!(source.data, original);
        let mut short = source;
        short.data.pop();
        assert!(scalar_image(&short, true, true, None).is_err());
    }

    #[test]
    fn sensor_toggle_and_nonscalar_depth_components_use_normal_display() {
        let mut view = CameraView::default();
        let source = line(1, 2, 20);
        view.toggle();
        assert!(
            view.transform(CameraKind::LineScan, &source, &[])
                .unwrap()
                .is_none()
        );
        let color = Frame {
            pixel_format: crate::types::RGB8,
            ..source
        };
        assert!(
            CameraView::default()
                .transform(CameraKind::Depth, &color, &[])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn float_range_handles_nonfinite_and_constant_samples() {
        let source = Frame {
            width: 3,
            height: 1,
            pixel_format: 0x0120_00bf,
            data: [f32::NAN, 4.0, 4.0]
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .collect(),
            ..Frame::default()
        };
        let (image, low, high) = scalar_image(&source, true, true, None).unwrap();
        assert_eq!((low, high), (4.0, 4.0));
        assert_eq!(&image.rgba[..4], &[0, 0, 0, 255]);
        assert_eq!(image.rgba[4..8], image.rgba[8..12]);
        let mut sensor = CameraView {
            sensor: true,
            ..Default::default()
        };
        assert!(
            sensor
                .transform(CameraKind::Depth, &source, &[])
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn tall_line_blocks_keep_horizontal_detail_and_only_recent_rows() {
        let source = Frame {
            width: 640,
            height: 10_000,
            pixel_format: crate::types::MONO8,
            data: vec![128; 640 * 10_000],
            ..Frame::default()
        };
        let image = CameraView::default()
            .transform(CameraKind::LineScan, &source, &[])
            .unwrap()
            .unwrap();
        assert_eq!((image.width, image.height), (640, HISTORY_ROWS as u32));
        assert_eq!(&image.rgba[..4], &[128, 128, 128, 255]);
    }

    #[test]
    fn invalid_depth_samples_follow_advertised_metadata_without_guessing_zero() {
        let mut camera =
            crate::camera::Camera::open("sim:depth", &[], std::time::Duration::from_millis(10))
                .unwrap();
        let mut features = camera.features();
        let source = Frame {
            width: 3,
            height: 1,
            pixel_format: 0x0110_00b8,
            data: [0u16, 1000, 1100]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect(),
            ..Frame::default()
        };
        let invalid = invalid_sample(&source, &features);
        assert_eq!(invalid, Some(0.0));
        let (image, low, high) = scalar_image(&source, true, true, invalid).unwrap();
        assert_eq!((low, high), (1000.0, 1100.0));
        assert_eq!(&image.rgba[..4], &[0, 0, 0, 255]);
        features
            .iter_mut()
            .find(|f| f.name == "Scan3dInvalidDataFlag")
            .unwrap()
            .value = Some(serde_json::json!(false));
        assert_eq!(invalid_sample(&source, &features), None);
        assert_eq!(scalar_image(&source, true, true, None).unwrap().1, 0.0);
    }
}
