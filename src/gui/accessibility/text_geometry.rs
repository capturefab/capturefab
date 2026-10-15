//! Text-run offsets use the same ancestor scroll, clip and scale as controls.
//! Rebase clipped character cells so native range queries and hit testing
//! cannot expose portions hidden outside a scroll viewport.
use super::{Node, Rectangle, rect, scroll};
use accesskit::TextDirection;

pub(super) fn transform(
    node: &mut Node,
    viewport: &scroll::Viewport,
    window: Option<Rectangle>,
    scale: f64,
) {
    let Some(bounds) = node.bounds() else {
        return;
    };
    let raw = Rectangle {
        x: bounds.x0 as f32,
        y: bounds.y0 as f32,
        width: bounds.width() as f32,
        height: bounds.height() as f32,
    };
    let (visible, full) = viewport.bounds(raw, window);
    let left = visible.x;
    let right = visible.x + visible.width;
    let (Some(source_positions), Some(source_widths)) =
        (node.character_positions(), node.character_widths())
    else {
        node.set_bounds(rect(visible, scale));
        return;
    };
    let rtl = node.text_direction() == Some(TextDirection::RightToLeft);
    let mut positions = Vec::with_capacity(source_positions.len());
    let mut widths = Vec::with_capacity(source_widths.len());
    for (&position, &width) in source_positions.iter().zip(source_widths) {
        let (start, end) = if rtl {
            let end = full.x + full.width - position;
            (end - width, end)
        } else {
            let start = full.x + position;
            (start, start + width)
        };
        let start = start.clamp(left, right);
        let end = end.clamp(left, right);
        positions.push((if rtl { right - end } else { start - left }) * scale as f32);
        widths.push((end - start).max(0.0) * scale as f32);
    }
    node.set_character_positions(positions);
    node.set_character_widths(widths);
    node.set_bounds(rect(visible, scale));
}

#[cfg(test)]
#[path = "text_geometry_tests.rs"]
mod tests;
