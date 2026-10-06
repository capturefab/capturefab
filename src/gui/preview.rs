//! Showing camera frames: the GPU decodes formats it supports, the CPU converts
//! the rest into an RGBA image, and both are placed the same way.
use super::gpu::GpuFrames;
use crate::types::Frame;
use anyhow::Result;
use iced::widget::{canvas, image, shader};
use iced::{Color, Element, Fill, Point, Rectangle, Renderer, Size, Theme, mouse};
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

/// Show `frame` under `key`, decoded by the GPU when the renderer supports its
/// format, otherwise converted on the CPU by `convert` into RGBA bytes.
pub fn present(
    gpu: &GpuFrames,
    shown: &mut Option<Shown>,
    key: &str,
    frame: Arc<Frame>,
    convert: impl FnOnce(&Frame) -> Result<(u32, u32, Vec<u8>)>,
) -> Result<()> {
    if gpu.accepts(&frame) {
        let size = Size::new(frame.width as f32, frame.height as f32);
        gpu.submit(key, frame);
        *shown = Some(Shown::Gpu {
            key: key.to_owned(),
            size,
        });
        return Ok(());
    }
    if matches!(shown, Some(Shown::Gpu { .. })) {
        gpu.retire(key);
    }
    let (width, height, rgba) = convert(&frame)?;
    *shown = Some(Shown::Image {
        handle: image::Handle::from_rgba(width, height, rgba),
        // The full frame size, so a downsampled preview still places like the frame.
        size: Size::new(frame.width as f32, frame.height as f32),
    });
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

/// Luminance histogram bars.
pub struct Histogram {
    pub bins: [u32; 64],
    pub color: Color,
}

impl<Message> canvas::Program<Message> for Histogram {
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
        let max = self.bins.iter().copied().max().unwrap_or(1).max(1) as f32;
        let width = bounds.width / self.bins.len() as f32;
        for (i, value) in self.bins.iter().enumerate() {
            let height = *value as f32 / max * bounds.height;
            frame.fill_rectangle(
                Point::new(i as f32 * width, bounds.height - height),
                Size::new((width - 1.0).max(0.5), height),
                self.color,
            );
        }
        vec![frame.into_geometry()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
