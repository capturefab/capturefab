//! Selection geometry from the editor's existing shaped paragraph. Logical
//! extended graphemes can occupy separate visual regions in bidirectional text.
//! This does not add cursor affinity or font GDEF ligature-caret information;
//! positions within a multi-grapheme shaping cluster use proportional advances,
//! using cosmic-text's proportional fallback while preserving bidi ordering.
use super::{cursor, text, text_input};
use iced::advanced::graphics::text::Paragraph;
use std::collections::BTreeMap;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Debug)]
pub(in crate::gui) struct Cell {
    pub grapheme: usize,
    pub byte_start: usize,
    pub byte_end: usize,
    pub x: f32,
    pub width: f32,
    pub rtl: bool,
    pub level: u8,
}

pub(in crate::gui) trait ParagraphGeometry: text::Paragraph {
    fn visual_geometry(&self, cursor: cursor::State) -> Option<text_input::VisualGeometry>;
    fn visual_cells(&self) -> Option<Vec<Cell>> {
        None
    }
}

impl ParagraphGeometry for () {
    fn visual_geometry(&self, _: cursor::State) -> Option<text_input::VisualGeometry> {
        None
    }
}

impl ParagraphGeometry for Paragraph {
    fn visual_geometry(&self, cursor: cursor::State) -> Option<text_input::VisualGeometry> {
        let (cells, count, rtl) = complete_cells(self)?;
        let (anchor, focus) = match cursor {
            cursor::State::Index(index) => (index, index),
            cursor::State::Selection { start, end } => (start, end),
        };
        if anchor > count || focus > count {
            return None;
        }
        let focus_x = boundary(
            &cells,
            focus,
            if anchor < focus {
                Some(true)
            } else if anchor > focus {
                Some(false)
            } else {
                None
            },
            rtl,
        );
        let mut ranges = cells
            .iter()
            .filter(|cell| {
                (anchor.min(focus)..anchor.max(focus)).contains(&cell.grapheme) && cell.width > 0.0
            })
            .map(|cell| (cell.x, cell.width))
            .collect::<Vec<_>>();
        ranges.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut merged: Vec<(f32, f32)> = Vec::new();
        for (x, width) in ranges {
            if let Some((left, old_width)) = merged.last_mut()
                && x <= *left + *old_width + 0.001
            {
                *old_width = (*left + *old_width).max(x + width) - *left;
            } else {
                merged.push((x, width));
            }
        }
        let mut geometry = text_input::VisualGeometry {
            ranges: merged,
            focus_x,
            base_rtl: rtl,
            cells: cells
                .into_iter()
                .map(|cell| text_input::VisualCell {
                    index: cell.grapheme,
                    x: cell.x,
                    width: cell.width,
                    rtl: cell.rtl,
                    level: cell.level,
                })
                .collect(),
        };
        if anchor == focus {
            geometry.focus_x = geometry.caret_x(text_input::VisualCaret {
                index: focus,
                affinity: text_input::CaretAffinity::Strong,
            })?;
        }
        Some(geometry)
    }
    fn visual_cells(&self) -> Option<Vec<Cell>> {
        complete_cells(self).map(|(cells, _, _)| cells)
    }
}

pub(super) fn geometry<P: ParagraphGeometry>(
    paragraph: &P,
    cursor: cursor::State,
) -> Option<text_input::VisualGeometry> {
    paragraph.visual_geometry(cursor)
}

/// The adapter only describes a complete single-line paragraph. In particular,
/// layout_runs alone is insufficient: it may hide later lines below the field's
/// height. Unexpected multiline/wrapped values keep the upstream fallback;
/// never reinterpret their full cursor range as a first-line selection.
fn complete_cells(paragraph: &Paragraph) -> Option<(Vec<Cell>, usize, bool)> {
    let buffer = paragraph.buffer();
    let [line] = buffer.lines.as_slice() else {
        return None;
    };
    if line.layout_opt()?.len() != 1 {
        return None;
    }
    let mut runs = buffer.layout_runs();
    let run = runs.next()?;
    if runs.next().is_some() || run.line_i != 0 || run.text != line.text() {
        return None;
    }
    let boundaries = run
        .text
        .grapheme_indices(true)
        .map(|(byte, _)| byte)
        .chain(std::iter::once(run.text.len()))
        .collect::<Vec<_>>();
    let count = boundaries.len().saturating_sub(1);
    let rtl = run.rtl;
    let cells = cells(run.glyphs, &boundaries);
    if cells.len() != count
        || !cells.iter().enumerate().all(|(index, cell)| {
            cell.grapheme == index
                && cell.byte_start == boundaries[index]
                && cell.byte_end == boundaries[index + 1]
                && cell.x.is_finite()
                && cell.width.is_finite()
                && cell.width >= 0.0
        })
    {
        return None;
    }
    Some((cells, count, rtl))
}

/// Paragraph-local advance cells in logical grapheme order, before field
/// alignment and horizontal scroll. Ink offsets do not move selection advances.
fn cells(
    glyphs: &[iced::advanced::graphics::text::cosmic_text::LayoutGlyph],
    boundaries: &[usize],
) -> Vec<Cell> {
    let count = boundaries.len().saturating_sub(1);
    let mut clusters: BTreeMap<(usize, usize, u8), (f32, f32)> = BTreeMap::new();
    for glyph in glyphs {
        if !glyph.x.is_finite() || !glyph.w.is_finite() || glyph.start >= glyph.end {
            continue;
        }
        let left = glyph.x.min(glyph.x + glyph.w);
        let right = glyph.x.max(glyph.x + glyph.w);
        clusters
            .entry((glyph.start, glyph.end, glyph.level.number()))
            .and_modify(|(x0, x1)| {
                *x0 = x0.min(left);
                *x1 = x1.max(right);
            })
            .or_insert((left, right));
    }
    let mut result: BTreeMap<usize, Cell> = BTreeMap::new();
    for ((start, end, level), (left, right)) in clusters {
        let first = boundaries
            .partition_point(|byte| *byte <= start)
            .saturating_sub(1)
            .min(count);
        let last = boundaries.partition_point(|byte| *byte < end).min(count);
        let n = last.saturating_sub(first);
        if n == 0 {
            continue;
        }
        let width = (right - left) / n as f32;
        for (part, index) in (first..last).enumerate() {
            let x = left
                + width
                    * if level.is_multiple_of(2) {
                        part
                    } else {
                        n - part - 1
                    } as f32;
            result
                .entry(index)
                .and_modify(|cell| {
                    let end = (cell.x + cell.width).max(x + width);
                    cell.x = cell.x.min(x);
                    cell.width = end - cell.x;
                })
                .or_insert(Cell {
                    grapheme: index,
                    byte_start: boundaries[index],
                    byte_end: boundaries[index + 1],
                    x,
                    width,
                    rtl: !level.is_multiple_of(2),
                    level,
                });
        }
    }
    result.into_values().collect()
}

fn leading(cell: &Cell) -> f32 {
    if cell.rtl {
        cell.x + cell.width
    } else {
        cell.x
    }
}
fn trailing(cell: &Cell) -> f32 {
    if cell.rtl {
        cell.x
    } else {
        cell.x + cell.width
    }
}

fn boundary(cells: &[Cell], index: usize, prefer_before: Option<bool>, base_rtl: bool) -> f32 {
    let before = cells.iter().rev().find(|cell| cell.grapheme < index);
    let after = cells.iter().find(|cell| cell.grapheme >= index);
    match (before, after) {
        (Some(before), Some(after)) => {
            let before_side =
                prefer_before.unwrap_or(before.rtl == base_rtl && after.rtl != base_rtl);
            if before_side {
                trailing(before)
            } else {
                leading(after)
            }
        }
        (Some(before), None) => trailing(before),
        (None, Some(after)) => leading(after),
        (None, None) => 0.0,
    }
}
