//! A chart uses the sheet's logical width; the daemon applies interface scale.
use super::super::style::Palette;
use crate::analytics::{SvgColors, SvgOptions};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Profile {
    pub width: u32,
    pub height: u32,
    pub options: SvgOptions,
}

pub(super) fn profile(logical_chart_width: f32, p: &'static Palette) -> Profile {
    let colors = SvgColors {
        background: rgb(p.base),
        text: rgb(p.text),
        secondary: rgb(p.secondary),
        axis: rgb(p.control_border),
        frame_rate: rgb(p.ink(p.live)),
        throughput: rgb(p.ink(p.accent_text)),
        missed_frames: rgb(p.ink(p.danger)),
        increased_contrast: p.increased(),
    };
    let options = SvgOptions::interface(logical_chart_width.floor() as u32, colors);
    let (width, height) = options.size();
    Profile {
        width,
        height,
        options,
    }
}

fn rgb(color: iced::Color) -> [u8; 3] {
    let [r, g, b, _] = color.into_rgba8();
    [r, g, b]
}

#[cfg(test)]
mod tests;
