//! Native text runs share the editor's existing shaped grapheme cells. Runs
//! remain in logical order even when their visual order is bidirectional.
use super::{Node, Role, Value, cursor, selection::ParagraphGeometry};
use accesskit::{NodeId, TextDirection, TextPosition, TextSelection};
use iced::{Point, Rectangle};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Default)]
pub(super) struct State {
    ids: Vec<NodeId>,
    spans: Vec<Span>,
    length: usize,
}

struct Span {
    id: NodeId,
    start: usize,
    end: usize,
    // Unclipped logical boundary positions choose the same side of a bidi
    // boundary as the actual drawn caret; clipping must not change affinity.
    carets: Vec<f32>,
}

pub(super) struct Published {
    pub children: Vec<(NodeId, Node)>,
    pub selection: Option<TextSelection>,
}

impl State {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn update<P: ParagraphGeometry>(
        &mut self,
        paragraph: &P,
        paragraph_text: &str,
        value: &Value,
        selection: cursor::State,
        origin: Point,
        viewport: Rectangle,
        fallback_id: NodeId,
    ) -> Published {
        let source = value.to_string();
        let boundaries: Vec<_> = source
            .grapheme_indices(true)
            .map(|(byte, _)| byte)
            .chain(std::iter::once(source.len()))
            .collect();
        let lengths: Option<Vec<u8>> = boundaries
            .windows(2)
            .map(|pair| u8::try_from(pair[1] - pair[0]).ok())
            .collect();
        let cells = paragraph.visual_cells().filter(|cells| {
            paragraph_text == source
                && cells.len() == value.len()
                && cells.iter().enumerate().all(|(index, cell)| {
                    cell.grapheme == index
                        && cell.byte_start == boundaries[index]
                        && cell.byte_end == boundaries[index + 1]
                        && cell.x.is_finite()
                        && cell.width.is_finite()
                        && cell.width >= 0.0
                })
        });
        self.length = value.len();
        self.spans.clear();
        let Some((cells, lengths)) = cells.zip(lengths.clone()) else {
            // Unit renderers and a paragraph not yet synchronized to its draft
            // retain working text/selection semantics without invented bounds.
            self.ids.clear();
            self.ids.push(fallback_id);
            self.spans.push(Span {
                id: fallback_id,
                start: 0,
                end: value.len(),
                carets: Vec::new(),
            });
            let mut node = Node::new(Role::TextRun);
            node.set_value(source);
            if let Some(lengths) = lengths {
                node.set_character_lengths(lengths);
            }
            return Published {
                selection: (!node.character_lengths().is_empty() || value.is_empty())
                    .then(|| self.selection(selection, None, None)),
                children: vec![(fallback_id, node)],
            };
        };

        let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
        for (index, cell) in cells.iter().enumerate() {
            if let Some(range) = ranges.last_mut() {
                let previous = &cells[index - 1];
                let trailing = if previous.rtl {
                    previous.x
                } else {
                    previous.x + previous.width
                };
                let leading = if cell.rtl {
                    cell.x + cell.width
                } else {
                    cell.x
                };
                if previous.rtl == cell.rtl && (trailing - leading).abs() < 0.05 {
                    range.end += 1;
                    continue;
                }
            }
            ranges.push(index..index + 1);
        }
        // An empty editor still needs a TextRun for a native caret at index 0.
        if ranges.is_empty() {
            ranges.push(0..0);
        }
        if self.ids.is_empty() {
            self.ids.push(fallback_id);
        }
        self.ids
            .resize_with(ranges.len(), super::super::accessibility::new_node_id);
        self.ids.truncate(ranges.len());
        let height = if paragraph.min_height() > 0.0 {
            paragraph.min_height()
        } else {
            viewport.height
        };
        let mut children = Vec::with_capacity(ranges.len());
        for (id, range) in self.ids.iter().copied().zip(ranges) {
            let slice = &cells[range.clone()];
            let rtl = slice.first().is_some_and(|cell| cell.rtl);
            let left = slice
                .iter()
                .map(|cell| cell.x)
                .reduce(f32::min)
                .unwrap_or(0.0);
            let right = slice
                .iter()
                .map(|cell| cell.x + cell.width)
                .reduce(f32::max)
                .unwrap_or(0.0);
            let full = Rectangle {
                x: origin.x + left,
                y: origin.y,
                width: right - left,
                height,
            };
            let visible = clip(full, viewport);
            let mut positions = Vec::with_capacity(slice.len());
            let mut widths = Vec::with_capacity(slice.len());
            let mut carets = Vec::with_capacity(slice.len() + 1);
            for cell in slice {
                let left = origin.x + cell.x;
                let right = left + cell.width;
                let clipped_left = left.clamp(visible.x, visible.x + visible.width);
                let clipped_right = right.clamp(visible.x, visible.x + visible.width);
                positions.push(if rtl {
                    visible.x + visible.width - clipped_right
                } else {
                    clipped_left - visible.x
                });
                widths.push((clipped_right - clipped_left).max(0.0));
                carets.push(if rtl { right } else { left });
            }
            carets.push(slice.last().map_or(origin.x, |cell| {
                origin.x + cell.x + if rtl { 0.0 } else { cell.width }
            }));
            let mut node = Node::new(Role::TextRun);
            node.set_value(source[boundaries[range.start]..boundaries[range.end]].to_owned());
            node.set_character_lengths(lengths[range.clone()].to_vec());
            node.set_character_positions(positions);
            node.set_character_widths(widths);
            node.set_text_direction(if rtl {
                TextDirection::RightToLeft
            } else {
                TextDirection::LeftToRight
            });
            node.set_bounds(accesskit::Rect {
                x0: visible.x.into(),
                y0: visible.y.into(),
                x1: (visible.x + visible.width).into(),
                y1: (visible.y + visible.height).into(),
            });
            children.push((id, node));
            self.spans.push(Span {
                id,
                start: range.start,
                end: range.end,
                carets,
            });
        }
        for index in 0..children.len() {
            if index > 0 {
                children[index].1.set_previous_on_line(self.ids[index - 1]);
            }
            if index + 1 < children.len() {
                children[index].1.set_next_on_line(self.ids[index + 1]);
            }
        }
        let reverse = match selection {
            cursor::State::Index(index) => cursor::State::Index(index),
            cursor::State::Selection { start, end } => cursor::State::Selection {
                start: end,
                end: start,
            },
        };
        let focus_x = paragraph
            .visual_geometry(selection)
            .map(|geometry| origin.x + geometry.focus_x);
        let anchor_x = paragraph
            .visual_geometry(reverse)
            .map(|geometry| origin.x + geometry.focus_x);
        Published {
            children,
            selection: Some(self.selection(selection, anchor_x, focus_x)),
        }
    }

    fn selection(
        &self,
        selection: cursor::State,
        anchor_x: Option<f32>,
        focus_x: Option<f32>,
    ) -> TextSelection {
        let (anchor, focus) = match selection {
            cursor::State::Index(index) => (index, index),
            cursor::State::Selection { start, end } => (start, end),
        };
        TextSelection {
            anchor: self.position(anchor, anchor_x),
            focus: self.position(focus, focus_x),
        }
    }

    fn position(&self, index: usize, wanted_x: Option<f32>) -> TextPosition {
        let index = index.min(self.length);
        let span = self
            .spans
            .iter()
            .filter(|span| (span.start..=span.end).contains(&index))
            .min_by(|a, b| {
                let distance = |span: &Span| {
                    wanted_x
                        .zip(span.carets.get(index - span.start))
                        .map_or(0.0, |(wanted, actual)| (wanted - actual).abs())
                };
                distance(a).total_cmp(&distance(b))
            })
            .expect("each logical text boundary belongs to a run");
        TextPosition {
            node: span.id,
            character_index: index - span.start,
        }
    }

    pub(super) fn selection_indices(&self, selection: &TextSelection) -> Option<(usize, usize)> {
        let index = |position: TextPosition| {
            self.spans
                .iter()
                .find(|span| span.id == position.node)
                .map(|span| span.start + position.character_index.min(span.end - span.start))
        };
        Some((index(selection.anchor)?, index(selection.focus)?))
    }
}

fn clip(bounds: Rectangle, viewport: Rectangle) -> Rectangle {
    let x = bounds.x.clamp(viewport.x, viewport.x + viewport.width);
    let y = bounds.y.clamp(viewport.y, viewport.y + viewport.height);
    Rectangle {
        x,
        y,
        width: ((bounds.x + bounds.width).clamp(viewport.x, viewport.x + viewport.width) - x)
            .max(0.0),
        height: ((bounds.y + bounds.height).clamp(viewport.y, viewport.y + viewport.height) - y)
            .max(0.0),
    }
}

#[cfg(test)]
mod tests;
