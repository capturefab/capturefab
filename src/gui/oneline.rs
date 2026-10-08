//! Single-line text that ends in "…" when it does not fit, rather than
//! wrapping or being cut through a glyph at its container's edge.
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::text::{self, Paragraph};
use iced::advanced::widget::{self, Widget, tree};
use iced::{Color, Element, Font, Length, Pixels, Rectangle, Size, alignment, mouse};
use std::borrow::Cow;

/// `content` on one line, ending in "…" where it would overflow the room it
/// is given. Only the first line shows; later lines also earn an ellipsis.
/// Shrinks to its text by default; `.width(Fill)` takes the room instead.
pub(super) fn one_line<'a>(
    content: impl text::IntoFragment<'a>,
    size: f32,
    font: Font,
    color: Color,
) -> OneLine<'a> {
    OneLine {
        content: content.into_fragment(),
        size,
        font,
        color,
        width: Length::Shrink,
        align_x: text::Alignment::Default,
    }
}

pub(super) struct OneLine<'a> {
    content: text::Fragment<'a>,
    size: f32,
    font: Font,
    color: Color,
    width: Length,
    align_x: text::Alignment,
}

impl OneLine<'_> {
    pub(super) fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }

    /// Sets the text against the right edge of a wider room, for figures.
    #[allow(dead_code)] // adopted by the area packages
    pub(super) fn align_right(mut self) -> Self {
        self.align_x = text::Alignment::Right;
        self
    }
}

/// The text shown for `content` in `room` px: its first line when that fits
/// and nothing follows it, else the longest prefix, cut at a char boundary,
/// that fits with "…" appended. Empty when not even "…" fits. `width`
/// measures a candidate, and is called O(log n) times.
pub(super) fn fit(content: &str, room: f32, mut width: impl FnMut(&str) -> f32) -> Cow<'_, str> {
    let (line, more) = first_line(content);
    if !more && width(line) <= room {
        return Cow::Borrowed(line);
    }
    let cut = |end: usize| format!("{}…", line[..end].trim_end());
    // Where a cut may end. The whole line only when later lines were dropped.
    let mut ends: Vec<usize> = line.char_indices().map(|(at, _)| at).collect();
    if more || ends.is_empty() {
        ends.push(line.len());
    }
    // The longest fitting cut, assuming wider candidates for longer cuts.
    let (mut low, mut high) = (0, ends.len() - 1);
    while low < high {
        let mid = (low + high).div_ceil(2);
        if width(&cut(ends[mid])) <= room {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    let shown = cut(ends[low]);
    if low == 0 && width(&shown) > room {
        return Cow::Borrowed("");
    }
    Cow::Owned(shown)
}

/// The first line of `content`, and whether anything but blank lines follows.
fn first_line(content: &str) -> (&str, bool) {
    match content.split_once('\n') {
        Some((line, rest)) => (
            line.strip_suffix('\r').unwrap_or(line),
            !rest.trim().is_empty(),
        ),
        None => (content, false),
    }
}

struct State<P> {
    content: String,
    size: f32,
    font: Font,
    align_x: text::Alignment,
    /// Width of the whole first line.
    whole: f32,
    /// The room `shown` was fitted to.
    room: Option<f32>,
    shown: String,
    paragraph: P,
}

impl<P: Default> Default for State<P> {
    fn default() -> Self {
        Self {
            content: String::new(),
            size: 0.0,
            font: Font::DEFAULT,
            align_x: text::Alignment::Default,
            whole: 0.0,
            room: None,
            shown: String::new(),
            paragraph: P::default(),
        }
    }
}

impl<M, Theme, R> Widget<M, Theme, R> for OneLine<'_>
where
    R: text::Renderer<Font = Font>,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State<R::Paragraph>>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::<R::Paragraph>::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(self.width, Length::Shrink)
    }

    fn layout(
        &mut self,
        tree: &mut widget::Tree,
        _renderer: &R,
        limits: &layout::Limits,
    ) -> layout::Node {
        let state = tree.state.downcast_mut::<State<R::Paragraph>>();
        let (size, font, align_x) = (self.size, self.font, self.align_x);
        let shape = |content: &str| {
            R::Paragraph::with_text(text::Text {
                content,
                bounds: Size::INFINITE,
                size: Pixels(size),
                line_height: text::LineHeight::default(),
                font,
                align_x,
                align_y: alignment::Vertical::Top,
                shaping: text::Shaping::default(),
                wrapping: text::Wrapping::None,
            })
        };
        layout::sized(limits, self.width, Length::Shrink, |limits| {
            let room = limits.max().width;
            let content: &str = &self.content;
            let fresh = state.content != content
                || state.size != size
                || state.font != font
                || state.align_x != align_x;
            let (line, _) = first_line(content);
            let mut whole = None;
            if fresh {
                content.clone_into(&mut state.content);
                (state.size, state.font, state.align_x) = (size, font, align_x);
                let paragraph = shape(line);
                state.whole = paragraph.min_width();
                whole = Some(paragraph);
            }
            if fresh || state.room != Some(room) {
                state.room = Some(room);
                let full = state.whole;
                let shown = fit(content, room, |candidate| {
                    if candidate == line {
                        full
                    } else {
                        shape(candidate).min_width()
                    }
                });
                if fresh || *shown != state.shown {
                    state.paragraph = match whole {
                        Some(paragraph) if *shown == *line => paragraph,
                        _ => shape(&shown),
                    };
                    state.shown = shown.into_owned();
                }
            }
            // One line high even when nothing fits, so rows never jump.
            Size::new(
                state.paragraph.min_width(),
                text::LineHeight::default().to_absolute(Pixels(size)).0,
            )
        })
    }

    fn draw(
        &self,
        tree: &widget::Tree,
        renderer: &mut R,
        _theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<State<R::Paragraph>>();
        let bounds = layout.bounds();
        // Clip to the room as a backstop, should even "…" not fit.
        let room = Rectangle {
            x: bounds.x,
            width: bounds.width,
            ..*viewport
        };
        let Some(clip) = room.intersection(viewport) else {
            return;
        };
        widget::text::draw(
            renderer,
            style,
            bounds,
            &state.paragraph,
            widget::text::Style {
                color: Some(self.color),
            },
            &clip,
        );
    }

    fn operate(
        &mut self,
        _tree: &mut widget::Tree,
        layout: Layout<'_>,
        _renderer: &R,
        operation: &mut dyn widget::Operation,
    ) {
        operation.text(None, layout.bounds(), &self.content);
    }
}

impl<'a, M, Theme, R> From<OneLine<'a>> for Element<'a, M, Theme, R>
where
    Theme: 'a,
    R: text::Renderer<Font = Font> + 'a,
{
    fn from(one_line: OneLine<'a>) -> Self {
        Element::new(one_line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// Every char 10 px wide, the ellipsis included.
    fn tens(text: &str) -> f32 {
        text.chars().count() as f32 * 10.0
    }

    #[test]
    fn text_that_fits_is_shown_whole() {
        assert!(matches!(
            fit("Pattern camera", 140.0, tens),
            Cow::Borrowed("Pattern camera")
        ));
        assert_eq!(fit("", 0.0, tens), "");
    }

    #[test]
    fn long_text_ends_in_an_ellipsis_that_fits() {
        assert_eq!(fit("Pattern camera", 139.0, tens), "Pattern came…");
        assert_eq!(fit("Pattern camera", 80.0, tens), "Pattern…");
        for room in [0.0, 5.0, 10.0, 35.0, 99.0, 139.0] {
            assert!(tens(&fit("Pattern camera", room, tens)) <= room, "{room}");
        }
    }

    #[test]
    fn cuts_drop_trailing_spaces_before_the_ellipsis() {
        assert_eq!(fit("Pattern camera", 90.0, tens), "Pattern…");
    }

    #[test]
    fn nothing_shows_when_not_even_the_ellipsis_fits() {
        assert_eq!(fit("Pattern camera", 9.0, tens), "");
        assert_eq!(fit("Pattern camera", 10.0, tens), "…");
    }

    #[test]
    fn cuts_land_on_char_boundaries() {
        let serial = "Kamera Ölçüm 日本語 📷 camera";
        for room in 0..=280 {
            let shown = fit(serial, room as f32, tens);
            assert!(tens(&shown) <= room as f32);
            let kept = shown.trim_end_matches('…');
            assert!(serial.starts_with(kept.trim_end()), "{shown}");
        }
        assert_eq!(fit(serial, 60.0, tens), "Kamer…");
        assert_eq!(fit(serial, 160.0, tens), "Kamera Ölçüm 日本…");
    }

    #[test]
    fn only_the_first_line_shows() {
        assert_eq!(
            fit("connect failed\ncaused by: timeout", 400.0, tens),
            "connect failed…"
        );
        assert_eq!(fit("connect failed\r\ncaused by", 90.0, tens), "connect…");
        assert_eq!(fit("connect failed\n\n  \n", 400.0, tens), "connect failed");
    }

    #[test]
    fn measuring_is_logarithmic() {
        let calls = Cell::new(0);
        let long = "x".repeat(1000);
        let shown = fit(&long, 505.0, |text| {
            calls.set(calls.get() + 1);
            tens(text)
        });
        assert_eq!(shown.chars().count(), 50);
        assert!(calls.get() <= 12, "{} measurements", calls.get());
    }
}
