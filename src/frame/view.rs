//! A frame, or a rectangle of one, read where its bytes lie.
#![forbid(unsafe_code)]
#![deny(clippy::missing_const_for_fn)]
use crate::types::Frame;

/// A rectangle of a frame's pixels, read in place: pixel (x, y) of the view
/// is the frame's (x0 + x, y0 + y). A region is an image of its own, as a
/// crop of it would be: a demosaic at its edges reads only its own samples.
/// Bayer readers derive the color filter phase from this origin, including
/// odd regions; `inspect::crop_rect` rounds public crops to complete quads
/// when both source dimensions contain a whole quad.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FrameRef<'a> {
    pub frame: &'a Frame,
    pub x0: usize,
    pub y0: usize,
    pub width: usize,
    pub height: usize,
}

impl<'a> FrameRef<'a> {
    pub(crate) const fn whole(frame: &'a Frame) -> Self {
        Self {
            frame,
            x0: 0,
            y0: 0,
            width: frame.width as usize,
            height: frame.height as usize,
        }
    }
    /// The `[x, y, width, height]` rectangle of `frame`, clamped to it.
    pub(crate) fn region(frame: &'a Frame, [x, y, width, height]: [u32; 4]) -> Self {
        let (x, y) = (x.min(frame.width), y.min(frame.height));
        Self {
            frame,
            x0: x as usize,
            y0: y as usize,
            width: width.min(frame.width - x) as usize,
            height: height.min(frame.height - y) as usize,
        }
    }
    pub(crate) const fn pixel_format(&self) -> u32 {
        self.frame.pixel_format
    }
    /// Whether the view covers its whole frame.
    pub(crate) fn is_whole(&self) -> bool {
        (self.x0, self.y0, self.width, self.height)
            == (0, 0, self.frame.width as usize, self.frame.height as usize)
    }
    pub(crate) const fn pixels(&self) -> usize {
        self.width * self.height
    }
}

impl<'a> From<&'a Frame> for FrameRef<'a> {
    fn from(frame: &'a Frame) -> Self {
        Self::whole(frame)
    }
}
