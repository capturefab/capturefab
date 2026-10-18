//! Showing camera frames: the GPU decodes formats it supports, the CPU converts
//! the rest into an RGBA image, and both are placed the same way.
use super::gpu::GpuFrames;
use crate::types::Frame;
use anyhow::Result;
use iced::widget::{canvas, image, shader};
use iced::{Element, Fill, Point, Rectangle, Renderer, Size, Theme, mouse};
use std::sync::Arc;

/// Space kept around a fitted image.
const FIT_MARGIN: f32 = 0.0;

/// The scale at which `native` fits inside `area`.
pub fn fit_scale(area: Size, native: Size) -> f32 {
    if native.width <= 0.0 || native.height <= 0.0 {
        return 1.0;
    }
    ((area.width - FIT_MARGIN) / native.width)
        .min((area.height - FIT_MARGIN) / native.height)
        .max(0.01)
}

/// Where an image of `native` size goes in `bounds`: centered, at `scale` or fitted.
pub fn placement(bounds: Rectangle, native: Size, scale: Option<f32>) -> Rectangle {
    let scale = scale.unwrap_or_else(|| fit_scale(bounds.size(), native));
    let size = Size::new(native.width * scale, native.height * scale);
    Rectangle::new(
        Point::new(
            bounds.center_x() - size.width / 2.0,
            bounds.center_y() - size.height / 2.0,
        ),
        size,
    )
}

/// A displayed frame: a CPU-converted image, or sensor bytes the GPU decodes
/// while drawing.
#[derive(Clone, Debug)]
pub enum Shown {
    Image { handle: image::Handle, size: Size },
    Gpu { key: String, size: Size },
}

impl Shown {
    /// The size a picture was converted at on the CPU.
    #[cfg(test)]
    pub fn converted(&self) -> Option<(u32, u32)> {
        match self {
            Self::Image {
                handle: image::Handle::Rgba { width, height, .. },
                ..
            } => Some((*width, *height)),
            _ => None,
        }
    }

    pub fn size(&self) -> Size {
        match self {
            Self::Image { size, .. } | Self::Gpu { size, .. } => *size,
        }
    }

    /// The frame scaled by `scale` (None fits it), centered and clipped by
    /// its container.
    pub fn view<'a, Message: 'a>(
        &self,
        gpu: &GpuFrames,
        scale: Option<f32>,
    ) -> Element<'a, Message> {
        match self {
            Self::Image { handle, size } => canvas::Canvas::new(Picture {
                handle: handle.clone(),
                native: *size,
                scale,
            })
            .width(Fill)
            .height(Fill)
            .into(),
            Self::Gpu { key, size } => shader::Shader::new(gpu.view(key, *size, scale))
                .width(Fill)
                .height(Fill)
                .into(),
        }
    }
}

/// How much of a frame a view can show, in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Frames larger than this each way are shrunk on the CPU; see
    /// `frame::preview_rgba`.
    pub size: (u32, u32),
    /// Shrink frames the GPU could decode whole too, as overview tiles do: a
    /// small texture filters cleanly where the shader would point-sample a
    /// large one, and uploads a fraction of the bytes. The main view keeps
    /// whole frames on the GPU, so zooming in needs no new upload.
    pub shrink_for_gpu: bool,
}

/// What a view showing `frame` at `scale` physical pixels per frame pixel
/// converts it within: every pixel from 1:1 up, else enough that the picture
/// is never enlarged, but no more than `most` on a side.
pub fn shown_bounds(frame: &Frame, scale: f32, most: u32) -> (u32, u32) {
    if scale >= 1.0 {
        return (frame.width, frame.height);
    }
    let shown = |side: u32| ((side as f32 * scale).ceil() as u32).max(1);
    let (width, height) = crate::frame::covering(frame, (shown(frame.width), shown(frame.height)));
    (width.min(most), height.min(most))
}

/// Show `frame` under `key`, decoded by the GPU when the renderer supports
/// its format, otherwise converted on the CPU into RGBA bytes. A frame too
/// large for a GPU texture is shrunk to `limits` and then decoded by the GPU
/// when that fits, so a view at 1:1 is the only one that converts it whole.
pub fn present(
    gpu: &GpuFrames,
    shown: &mut Option<Shown>,
    key: &str,
    frame: Arc<Frame>,
    limits: Limits,
) -> Result<()> {
    // Placed at the full frame size, so a shrunk frame still places like it.
    let size = Size::new(frame.width as f32, frame.height as f32);
    let whole = gpu.accepts(&frame);
    let small = (!whole || limits.shrink_for_gpu)
        .then(|| crate::frame::shrink_to(&frame, limits.size))
        .flatten();
    let decoded = match small {
        Some(small) if gpu.accepts(&small) => Ok(Arc::new(small)),
        _ if whole => Ok(frame.clone()),
        small => Err(small),
    };
    match decoded {
        Ok(frame) => {
            gpu.submit(key, frame);
            *shown = Some(Shown::Gpu {
                key: key.to_owned(),
                size,
            });
        }
        Err(small) => {
            if matches!(shown, Some(Shown::Gpu { .. })) {
                gpu.retire(key);
            }
            let source = small.as_ref().unwrap_or(&frame);
            let (width, height, rgba) =
                crate::frame::preview_rgba(source, limits.size.0, limits.size.1)?;
            *shown = Some(Shown::Image {
                handle: image::Handle::from_rgba(width, height, rgba),
                size,
            });
        }
    }
    Ok(())
}

struct Picture {
    handle: image::Handle,
    native: Size,
    scale: Option<f32>,
}

impl<Message> canvas::Program<Message> for Picture {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let local = Rectangle::new(Point::ORIGIN, bounds.size());
        frame.draw_image(
            placement(local, self.native, self.scale),
            canvas::Image::new(self.handle.clone()).filter_method(image::FilterMethod::Linear),
        );
        vec![frame.into_geometry()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RGB8;

    fn frame(pixel_format: u32, width: u32, height: u32) -> Arc<Frame> {
        Arc::new(Frame {
            width,
            height,
            pixel_format,
            data: vec![90; (width * height) as usize],
            ..Frame::default()
        })
    }

    #[test]
    fn the_gpu_takes_frames_whole_or_shrunk_and_the_cpu_converts_what_is_shown() {
        let bayer = frame(0x0108_0009, 2448, 2048);
        let native = Size::new(2448.0, 2048.0);
        let placed = |shown: &Option<Shown>| shown.as_ref().map(Shown::size);
        let half = Limits {
            size: (1224, 1024),
            shrink_for_gpu: false,
        };
        // The main view hands the GPU a frame it takes whole as it is.
        let gpu = GpuFrames::limited(8192);
        let mut shown = None;
        present(&gpu, &mut shown, "main", bayer.clone(), half).unwrap();
        assert!(Arc::ptr_eq(&gpu.pending("main").unwrap(), &bayer));
        assert!(matches!(shown, Some(Shown::Gpu { .. })));
        assert_eq!(placed(&shown), Some(native));
        // A tile shrinks it first, to colors the GPU filters cleanly.
        let tile = Limits {
            shrink_for_gpu: true,
            ..half
        };
        present(&gpu, &mut shown, "tile", bayer.clone(), tile).unwrap();
        let small = gpu.pending("tile").unwrap();
        assert_eq!(
            (small.width, small.height, small.pixel_format),
            (1224, 1024, RGB8)
        );
        assert_eq!(placed(&shown), Some(native));
        // Too large for a texture: shrunk for the GPU, but whole on the CPU at 1:1.
        let downlevel = GpuFrames::limited(2048);
        present(&downlevel, &mut shown, "main", bayer.clone(), half).unwrap();
        assert_eq!(downlevel.pending("main").unwrap().width, 1224);
        let every = Limits {
            size: (2448, 2048),
            shrink_for_gpu: false,
        };
        present(&downlevel, &mut shown, "main", bayer.clone(), every).unwrap();
        assert_eq!(
            shown.as_ref().and_then(Shown::converted),
            Some((2448, 2048))
        );
        assert!(downlevel.is_retired("main"));
        // Without a GPU the CPU converts only what the view shows.
        let cpu = GpuFrames::cpu_only();
        present(&cpu, &mut shown, "main", bayer, half).unwrap();
        assert_eq!(
            shown.as_ref().and_then(Shown::converted),
            Some((1224, 1024))
        );
        assert_eq!(placed(&shown), Some(native));
    }

    #[test]
    fn views_convert_enough_to_never_enlarge_and_everything_at_one_to_one() {
        let bayer = frame(0x0108_0009, 5472, 3648);
        assert_eq!(shown_bounds(&bayer, 1.0, u32::MAX), (5472, 3648));
        assert_eq!(
            shown_bounds(&bayer, 4.0, 1280),
            (5472, 3648),
            "1:1 stays exact"
        );
        // A quarter: whole quads, then two at a time.
        assert_eq!(shown_bounds(&bayer, 0.25, u32::MAX), (1368, 912));
        assert_eq!(shown_bounds(&bayer, 0.3, u32::MAX), (2736, 1824));
        assert_eq!(shown_bounds(&bayer, 0.3, 1280), (1280, 1280));
        let mono = frame(crate::types::MONO8, 2000, 1500);
        assert_eq!(shown_bounds(&mono, 0.4, u32::MAX), (1000, 750));
        assert_eq!(shown_bounds(&mono, 0.7, u32::MAX), (2000, 1500));
    }

    #[test]
    fn fitted_images_fill_and_center() {
        let bounds = Rectangle::new(Point::new(10.0, 20.0), Size::new(416.0, 316.0));
        let image = placement(bounds, Size::new(800.0, 600.0), None);
        assert_eq!(image.size(), Size::new(416.0, 312.0));
        assert_eq!(image.center(), bounds.center());
        let actual = placement(bounds, Size::new(800.0, 600.0), Some(1.0));
        assert_eq!(actual.size(), Size::new(800.0, 600.0));
        assert_eq!(actual.center(), bounds.center());
    }
}
