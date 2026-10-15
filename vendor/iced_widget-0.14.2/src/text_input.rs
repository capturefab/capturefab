//! Text inputs display fields that can be filled with text.
//!
//! # Example
//! ```no_run
//! # mod iced { pub mod widget { pub use iced_widget::*; } pub use iced_widget::Renderer; pub use iced_widget::core::*; }
//! # pub type Element<'a, Message> = iced_widget::core::Element<'a, Message, iced_widget::Theme, iced_widget::Renderer>;
//! #
//! use iced::widget::text_input;
//!
//! struct State {
//!    content: String,
//! }
//!
//! #[derive(Debug, Clone)]
//! enum Message {
//!     ContentChanged(String)
//! }
//!
//! fn view(state: &State) -> Element<'_, Message> {
//!     text_input("Type something here...", &state.content)
//!         .on_input(Message::ContentChanged)
//!         .into()
//! }
//!
//! fn update(state: &mut State, message: Message) {
//!     match message {
//!         Message::ContentChanged(content) => {
//!             state.content = content;
//!         }
//!     }
//! }
//! ```
mod editor;
mod value;

pub mod cursor;

pub use cursor::Cursor;
pub use value::Value;

use editor::Editor;

use crate::core::alignment;
use crate::core::clipboard::{self, Clipboard};
use crate::core::input_method;
use crate::core::keyboard;
use crate::core::keyboard::key;
use crate::core::layout;
use crate::core::mouse::{self, click};
use crate::core::renderer;
use crate::core::text::paragraph::{self, Paragraph as _};
use crate::core::text::{self, Text};
use crate::core::time::{Duration, Instant};
use crate::core::touch;
use crate::core::widget;
use crate::core::widget::operation::{self, Operation};
use crate::core::widget::tree::{self, Tree};
use crate::core::window;
use crate::core::{
    Alignment, Background, Border, Color, Element, Event, InputMethod, Layout,
    Length, Padding, Pixels, Point, Rectangle, Shell, Size, Theme, Vector,
    Widget,
};

/// A field that can be filled with text.
///
/// # Example
/// ```no_run
/// # mod iced { pub mod widget { pub use iced_widget::*; } pub use iced_widget::Renderer; pub use iced_widget::core::*; }
/// # pub type Element<'a, Message> = iced_widget::core::Element<'a, Message, iced_widget::Theme, iced_widget::Renderer>;
/// #
/// use iced::widget::text_input;
///
/// struct State {
///    content: String,
/// }
///
/// #[derive(Debug, Clone)]
/// enum Message {
///     ContentChanged(String)
/// }
///
/// fn view(state: &State) -> Element<'_, Message> {
///     text_input("Type something here...", &state.content)
///         .on_input(Message::ContentChanged)
///         .into()
/// }
///
/// fn update(state: &mut State, message: Message) {
///     match message {
///         Message::ContentChanged(content) => {
///             state.content = content;
///         }
///     }
/// }
/// ```
pub struct TextInput<
    'a,
    Message,
    Theme = crate::Theme,
    Renderer = crate::Renderer,
> where
    Theme: Catalog,
    Renderer: text::Renderer,
{
    id: Option<widget::Id>,
    placeholder: String,
    value: Value,
    is_secure: bool,
    show_unfocused_selection: bool,
    visual_geometry: Option<VisualGeometryFn<Renderer::Paragraph>>,
    font: Option<Renderer::Font>,
    width: Length,
    padding: Padding,
    size: Option<Pixels>,
    line_height: text::LineHeight,
    alignment: alignment::Horizontal,
    on_input: Option<Box<dyn Fn(String) -> Message + 'a>>,
    on_paste: Option<Box<dyn Fn(String) -> Message + 'a>>,
    on_submit: Option<Message>,
    icon: Option<Icon<Renderer::Font>>,
    class: Theme::Class<'a>,
    last_status: Option<Status>,
}

/// Paragraph-local selection advances and the logical focus endpoint.
///
/// Ranges are `(x, width)` pairs; callers must not join separated visual ranges
/// into a bounding rectangle. These coordinates precede field alignment/scroll.
#[derive(Debug, Clone)]
pub struct VisualGeometry {
    /// Selected visual advance intervals, from left to right.
    pub ranges: Vec<(f32, f32)>,
    /// Visual X coordinate of the logical cursor/selection focus.
    pub focus_x: f32,
    /// Logical grapheme advance cells used for the same pointer geometry.
    pub cells: Vec<VisualCell>,
    /// Resolved base direction of this single-line paragraph.
    pub base_rtl: bool,
}

/// Which visual position to retain at a split bidirectional insertion point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaretAffinity {
    /// Insertion in the paragraph's base direction.
    Strong,
    /// Insertion opposite to the paragraph's base direction.
    Weak,
}

/// A logical extended-grapheme boundary and its retained visual affinity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualCaret {
    /// Logical extended-grapheme boundary.
    pub index: usize,
    /// Retained strong/weak position at a bidirectional boundary.
    pub affinity: CaretAffinity,
}

/// Retained visual endpoints of the editor's logical selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualSelection {
    /// Fixed endpoint while extending a selection.
    pub anchor: VisualCaret,
    /// Moving endpoint, used for the caret, scrolling and input methods.
    pub focus: VisualCaret,
}

/// One logical extended grapheme's paragraph-local advance cell.
#[derive(Debug, Clone)]
pub struct VisualCell {
    /// Logical extended-grapheme index.
    pub index: usize,
    /// Left edge of the visual advance cell.
    pub x: f32,
    /// Width of the visual advance cell.
    pub width: f32,
    /// Whether its logical leading edge is on the right.
    pub rtl: bool,
    /// Resolved bidi embedding level of the cached shaping cluster.
    pub level: u8,
}

impl VisualGeometry {
    /// Hits the nearest grapheme cell using the same logical/visual mapping
    /// as selection paint. Bidi boundary affinity remains the caller's model.
    pub fn hit_test(&self, x: f32) -> Option<usize> {
        self.hit_test_caret(x).map(|caret| caret.index)
    }

    /// Hits a grapheme edge without losing its strong/weak visual position.
    pub fn hit_test_caret(&self, x: f32) -> Option<VisualCaret> {
        if !x.is_finite() {
            return None;
        }
        let distance = |cell: &VisualCell| {
            (cell.x - x).max(0.0) + (x - cell.x - cell.width).max(0.0)
        };
        let cell = self
            .cells
            .iter()
            .filter(|cell| {
                cell.x.is_finite() && cell.width.is_finite() && cell.width > 0.0
            })
            .min_by(|a, b| distance(a).total_cmp(&distance(b)))?;
        let after = (x >= cell.x + cell.width / 2.0) != cell.rtl;
        let index = cell.index + usize::from(after);
        let edge = if after != cell.rtl {
            cell.x + cell.width
        } else {
            cell.x
        };
        self.caret_near(index, edge)
    }

    /// Strong and weak X positions, derived from adjacent shaped advances and
    /// embedding levels. Paragraph edges act as base-direction boundaries.
    pub fn caret_positions(&self, index: usize) -> Option<(f32, f32)> {
        let left = self
            .cells
            .iter()
            .map(|cell| cell.x)
            .reduce(f32::min)
            .unwrap_or(0.0);
        let right = self
            .cells
            .iter()
            .map(|cell| cell.x + cell.width)
            .reduce(f32::max)
            .unwrap_or(0.0);
        self.caret_positions_with_edges(index, left, right)
    }

    fn caret_positions_with_edges(
        &self,
        index: usize,
        left: f32,
        right: f32,
    ) -> Option<(f32, f32)> {
        if index > self.cells.len() {
            return None;
        }
        let base_level = u8::from(self.base_rtl);
        let (before_x, before_rtl, before_level) = index
            .checked_sub(1)
            .and_then(|index| self.cells.get(index))
            .map_or(
                (
                    if self.base_rtl { right } else { left },
                    self.base_rtl,
                    base_level,
                ),
                |cell| {
                    (
                        if cell.rtl {
                            cell.x
                        } else {
                            cell.x + cell.width
                        },
                        cell.rtl,
                        cell.level,
                    )
                },
            );
        let (after_x, after_rtl, after_level) = self.cells.get(index).map_or(
            (
                if self.base_rtl { left } else { right },
                self.base_rtl,
                base_level,
            ),
            |cell| {
                (
                    if cell.rtl {
                        cell.x + cell.width
                    } else {
                        cell.x
                    },
                    cell.rtl,
                    cell.level,
                )
            },
        );
        let before_is_strong = before_rtl == self.base_rtl
            && (after_rtl != before_rtl || before_level < after_level);
        Some(if before_is_strong {
            (before_x, after_x)
        } else {
            (after_x, before_x)
        })
    }

    /// X position shared by drawing, horizontal scrolling and input methods.
    pub fn caret_x(&self, caret: VisualCaret) -> Option<f32> {
        self.caret_positions(caret.index).map(|(strong, weak)| {
            match caret.affinity {
                CaretAffinity::Strong => strong,
                CaretAffinity::Weak => weak,
            }
        })
    }

    fn caret_near(&self, index: usize, x: f32) -> Option<VisualCaret> {
        let (strong, weak) = self.caret_positions(index)?;
        Some(VisualCaret {
            index,
            affinity: if (weak - x).abs() + 0.001 < (strong - x).abs() {
                CaretAffinity::Weak
            } else {
                CaretAffinity::Strong
            },
        })
    }

    fn adjacent_caret(
        &self,
        caret: VisualCaret,
        right: bool,
    ) -> Option<VisualCaret> {
        let x = self.caret_x(caret)?;
        let left = self
            .cells
            .iter()
            .map(|cell| cell.x)
            .reduce(f32::min)
            .unwrap_or(0.0);
        let right_edge = self
            .cells
            .iter()
            .map(|cell| cell.x + cell.width)
            .reduce(f32::max)
            .unwrap_or(0.0);
        let next = (0..=self.cells.len())
            .filter_map(|index| {
                let candidate = VisualCaret { index, ..caret };
                let (strong, weak) =
                    self.caret_positions_with_edges(index, left, right_edge)?;
                let candidate_x = match candidate.affinity {
                    CaretAffinity::Strong => strong,
                    CaretAffinity::Weak => weak,
                };
                ((right && candidate_x > x + 0.001)
                    || (!right && candidate_x < x - 0.001))
                    .then_some((candidate, candidate_x))
            })
            .min_by(|(a, ax), (b, bx)| {
                if right {
                    ax.total_cmp(bx).then(a.index.cmp(&b.index))
                } else {
                    bx.total_cmp(ax).then(b.index.cmp(&a.index))
                }
            });
        Some(next.map_or(caret, |(caret, _)| caret))
    }

    fn selection_edge(
        &self,
        start: usize,
        end: usize,
        right: bool,
    ) -> Option<VisualCaret> {
        let cell = self
            .cells
            .iter()
            .filter(|cell| {
                (start.min(end)..start.max(end)).contains(&cell.index)
            })
            .min_by(|a, b| {
                if right {
                    (b.x + b.width).total_cmp(&(a.x + a.width))
                } else {
                    a.x.total_cmp(&b.x)
                }
            })?;
        let index = cell.index + usize::from(right != cell.rtl);
        self.caret_near(index, cell.x + if right { cell.width } else { 0.0 })
    }
}

/// An optional adapter to geometry from the paragraph's existing shaping.
pub type VisualGeometryFn<P> = fn(&P, cursor::State) -> Option<VisualGeometry>;

/// The default [`Padding`] of a [`TextInput`].
pub const DEFAULT_PADDING: Padding = Padding::new(5.0);

impl<'a, Message, Theme, Renderer> TextInput<'a, Message, Theme, Renderer>
where
    Message: Clone,
    Theme: Catalog,
    Renderer: text::Renderer,
{
    /// Creates a new [`TextInput`] with the given placeholder and
    /// its current value.
    pub fn new(placeholder: &str, value: &str) -> Self {
        TextInput {
            id: None,
            placeholder: String::from(placeholder),
            value: Value::new(value),
            is_secure: false,
            show_unfocused_selection: false,
            visual_geometry: None,
            font: None,
            width: Length::Fill,
            padding: DEFAULT_PADDING,
            size: None,
            line_height: text::LineHeight::default(),
            alignment: alignment::Horizontal::Left,
            on_input: None,
            on_paste: None,
            on_submit: None,
            icon: None,
            class: Theme::default(),
            last_status: None,
        }
    }

    /// Sets the [`widget::Id`] of the [`TextInput`].
    pub fn id(mut self, id: impl Into<widget::Id>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Converts the [`TextInput`] into a secure password input.
    pub fn secure(mut self, is_secure: bool) -> Self {
        self.is_secure = is_secure;
        self
    }

    /// Keeps the retained selected range visible when its window loses focus.
    ///
    /// This does not draw an inactive caret or change editor focus, keyboard
    /// input, or input-method behavior. The caller's style supplies the
    /// unemphasized selection color. Defaults to `false`.
    pub fn show_unfocused_selection(mut self, show: bool) -> Self {
        self.show_unfocused_selection = show;
        self
    }

    /// Uses the cached paragraph's visual selection geometry. Defaults to none,
    /// preserving the renderer-independent upstream geometry for other callers.
    /// The adapter must use logical extended-grapheme indices and must not shape
    /// a second paragraph. Returning `None` keeps the existing fallback.
    pub fn visual_geometry(
        mut self,
        geometry: VisualGeometryFn<Renderer::Paragraph>,
    ) -> Self {
        self.visual_geometry = Some(geometry);
        self
    }

    /// Returns the unclipped logical origin used to draw the cached value.
    /// Alignment and horizontal scroll are applied once, as in drawing.
    pub fn text_origin(&self, tree: &Tree, layout: Layout<'_>) -> Point {
        let state = tree.state.downcast_ref::<State<Renderer::Paragraph>>();
        let bounds = layout.children().next().unwrap().bounds();
        let secure = self.is_secure.then(|| self.value.secure());
        let value = secure.as_ref().unwrap_or(&self.value);
        let paragraph = state.value.raw();
        let offset = if state.show_cursor_or_selection(
            value,
            self.on_input.is_none(),
            self.show_unfocused_selection,
        ) {
            offset(bounds, value, state, self.visual_geometry)
        } else {
            0.0
        };
        bounds.anchor(
            paragraph.min_bounds(),
            Alignment::Start,
            Alignment::Center,
        ) + Vector::new(
            alignment_offset(
                bounds.width,
                paragraph.min_width(),
                self.alignment,
            ) - offset,
            0.0,
        )
    }

    /// Sets the message that should be produced when some text is typed into
    /// the [`TextInput`].
    ///
    /// If this method is not called, the [`TextInput`] will be disabled.
    pub fn on_input(
        mut self,
        on_input: impl Fn(String) -> Message + 'a,
    ) -> Self {
        self.on_input = Some(Box::new(on_input));
        self
    }

    /// Sets the message that should be produced when some text is typed into
    /// the [`TextInput`], if `Some`.
    ///
    /// If `None`, the [`TextInput`] will be disabled.
    pub fn on_input_maybe(
        mut self,
        on_input: Option<impl Fn(String) -> Message + 'a>,
    ) -> Self {
        self.on_input = on_input.map(|f| Box::new(f) as _);
        self
    }

    /// Sets the message that should be produced when the [`TextInput`] is
    /// focused and the enter key is pressed.
    pub fn on_submit(mut self, message: Message) -> Self {
        self.on_submit = Some(message);
        self
    }

    /// Sets the message that should be produced when the [`TextInput`] is
    /// focused and the enter key is pressed, if `Some`.
    pub fn on_submit_maybe(mut self, on_submit: Option<Message>) -> Self {
        self.on_submit = on_submit;
        self
    }

    /// Sets the message that should be produced when some text is pasted into
    /// the [`TextInput`].
    pub fn on_paste(
        mut self,
        on_paste: impl Fn(String) -> Message + 'a,
    ) -> Self {
        self.on_paste = Some(Box::new(on_paste));
        self
    }

    /// Sets the message that should be produced when some text is pasted into
    /// the [`TextInput`], if `Some`.
    pub fn on_paste_maybe(
        mut self,
        on_paste: Option<impl Fn(String) -> Message + 'a>,
    ) -> Self {
        self.on_paste = on_paste.map(|f| Box::new(f) as _);
        self
    }

    /// Sets the [`Font`] of the [`TextInput`].
    ///
    /// [`Font`]: text::Renderer::Font
    pub fn font(mut self, font: Renderer::Font) -> Self {
        self.font = Some(font);
        self
    }

    /// Sets the [`Icon`] of the [`TextInput`].
    pub fn icon(mut self, icon: Icon<Renderer::Font>) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Sets the width of the [`TextInput`].
    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }

    /// Sets the [`Padding`] of the [`TextInput`].
    pub fn padding<P: Into<Padding>>(mut self, padding: P) -> Self {
        self.padding = padding.into();
        self
    }

    /// Sets the text size of the [`TextInput`].
    pub fn size(mut self, size: impl Into<Pixels>) -> Self {
        self.size = Some(size.into());
        self
    }

    /// Sets the [`text::LineHeight`] of the [`TextInput`].
    pub fn line_height(
        mut self,
        line_height: impl Into<text::LineHeight>,
    ) -> Self {
        self.line_height = line_height.into();
        self
    }

    /// Sets the horizontal alignment of the [`TextInput`].
    pub fn align_x(
        mut self,
        alignment: impl Into<alignment::Horizontal>,
    ) -> Self {
        self.alignment = alignment.into();
        self
    }

    /// Sets the style of the [`TextInput`].
    #[must_use]
    pub fn style(mut self, style: impl Fn(&Theme, Status) -> Style + 'a) -> Self
    where
        Theme::Class<'a>: From<StyleFn<'a, Theme>>,
    {
        self.class = (Box::new(style) as StyleFn<'a, Theme>).into();
        self
    }

    /// Sets the style class of the [`TextInput`].
    #[must_use]
    pub fn class(mut self, class: impl Into<Theme::Class<'a>>) -> Self {
        self.class = class.into();
        self
    }

    /// Lays out the [`TextInput`], overriding its [`Value`] if provided.
    ///
    /// [`Renderer`]: text::Renderer
    pub fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
        value: Option<&Value>,
    ) -> layout::Node {
        let state = tree.state.downcast_mut::<State<Renderer::Paragraph>>();
        let value = value.unwrap_or(&self.value);

        let font = self.font.unwrap_or_else(|| renderer.default_font());
        let text_size = self.size.unwrap_or_else(|| renderer.default_size());
        let padding = self.padding.fit(Size::ZERO, limits.max());
        let height = self.line_height.to_absolute(text_size);

        let limits = limits.width(self.width).shrink(padding);
        let text_bounds = limits.resolve(self.width, height, Size::ZERO);

        let placeholder_text = Text {
            font,
            line_height: self.line_height,
            content: self.placeholder.as_str(),
            bounds: Size::new(f32::INFINITY, text_bounds.height),
            size: text_size,
            align_x: text::Alignment::Default,
            align_y: alignment::Vertical::Center,
            shaping: text::Shaping::Advanced,
            wrapping: text::Wrapping::default(),
        };

        let _ = state.placeholder.update(placeholder_text);

        let secure_value = self.is_secure.then(|| value.secure());
        let value = secure_value.as_ref().unwrap_or(value);

        let _ = state.value.update(Text {
            content: &value.to_string(),
            ..placeholder_text
        });

        if let Some(icon) = &self.icon {
            let mut content = [0; 4];

            let icon_text = Text {
                line_height: self.line_height,
                content: icon.code_point.encode_utf8(&mut content) as &_,
                font: icon.font,
                size: icon.size.unwrap_or_else(|| renderer.default_size()),
                bounds: Size::new(f32::INFINITY, text_bounds.height),
                align_x: text::Alignment::Center,
                align_y: alignment::Vertical::Center,
                shaping: text::Shaping::Advanced,
                wrapping: text::Wrapping::default(),
            };

            let _ = state.icon.update(icon_text);

            let icon_width = state.icon.min_width();

            let (text_position, icon_position) = match icon.side {
                Side::Left => (
                    Point::new(
                        padding.left + icon_width + icon.spacing,
                        padding.top,
                    ),
                    Point::new(padding.left, padding.top),
                ),
                Side::Right => (
                    Point::new(padding.left, padding.top),
                    Point::new(
                        padding.left + text_bounds.width - icon_width,
                        padding.top,
                    ),
                ),
            };

            let text_node = layout::Node::new(
                text_bounds - Size::new(icon_width + icon.spacing, 0.0),
            )
            .move_to(text_position);

            let icon_node =
                layout::Node::new(Size::new(icon_width, text_bounds.height))
                    .move_to(icon_position);

            layout::Node::with_children(
                text_bounds.expand(padding),
                vec![text_node, icon_node],
            )
        } else {
            let text = layout::Node::new(text_bounds)
                .move_to(Point::new(padding.left, padding.top));

            layout::Node::with_children(text_bounds.expand(padding), vec![text])
        }
    }

    fn input_method<'b>(
        &self,
        state: &'b State<Renderer::Paragraph>,
        layout: Layout<'_>,
        value: &Value,
    ) -> InputMethod<&'b str> {
        let Some(Focus {
            is_window_focused: true,
            ..
        }) = &state.is_focused
        else {
            return InputMethod::Disabled;
        };

        let secure_value = self.is_secure.then(|| value.secure());
        let value = secure_value.as_ref().unwrap_or(value);

        let text_bounds = layout.children().next().unwrap().bounds();

        let caret_index = match state.cursor.state(value) {
            cursor::State::Index(position) => position,
            cursor::State::Selection { start, end } => start.min(end),
        };

        let text = state.value.raw();
        let (cursor_x, scroll_offset) = measure_cursor_and_scroll_offset(
            text,
            text_bounds,
            caret_index,
            state.cursor.state(value),
            self.visual_geometry,
            state.visual_caret(),
        );

        let alignment_offset = alignment_offset(
            text_bounds.width,
            text.min_width(),
            self.alignment,
        );

        let x = (text_bounds.x + cursor_x).floor() - scroll_offset
            + alignment_offset;

        InputMethod::Enabled {
            cursor: Rectangle::new(
                Point::new(x, text_bounds.y),
                Size::new(1.0, text_bounds.height),
            ),
            purpose: if self.is_secure {
                input_method::Purpose::Secure
            } else {
                input_method::Purpose::Normal
            },
            preedit: state.preedit.as_ref().map(input_method::Preedit::as_ref),
        }
    }

    /// Draws the [`TextInput`] with the given [`Renderer`], overriding its
    /// [`Value`] if provided.
    ///
    /// [`Renderer`]: text::Renderer
    pub fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        value: Option<&Value>,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<State<Renderer::Paragraph>>();
        let value = value.unwrap_or(&self.value);
        let is_disabled = self.on_input.is_none();

        let secure_value = self.is_secure.then(|| value.secure());
        let value = secure_value.as_ref().unwrap_or(value);

        let bounds = layout.bounds();

        let mut children_layout = layout.children();
        let text_bounds = children_layout.next().unwrap().bounds();

        let style = theme
            .style(&self.class, self.last_status.unwrap_or(Status::Disabled));

        renderer.fill_quad(
            renderer::Quad {
                bounds,
                border: style.border,
                ..renderer::Quad::default()
            },
            style.background,
        );

        if self.icon.is_some() {
            let icon_layout = children_layout.next().unwrap();

            let icon = state.icon.raw();

            renderer.fill_paragraph(
                icon,
                icon_layout.bounds().anchor(
                    icon.min_bounds(),
                    Alignment::Center,
                    Alignment::Center,
                ),
                style.icon,
                *viewport,
            );
        }

        let text = value.to_string();

        let (quads, offset, is_selecting) = if let Some(focus) =
            state.is_focused.as_ref().filter(|_| {
                state.show_cursor_or_selection(
                    value,
                    is_disabled,
                    self.show_unfocused_selection,
                )
            }) {
            match state.cursor.state(value) {
                cursor::State::Index(position) => {
                    let (x, offset) = measure_cursor_and_scroll_offset(
                        state.value.raw(),
                        text_bounds,
                        position,
                        cursor::State::Index(position),
                        self.visual_geometry,
                        state.visual_caret(),
                    );
                    let visible = !is_disabled
                        && ((focus.now - focus.updated_at).as_millis()
                            / CURSOR_BLINK_INTERVAL_MILLIS)
                            .is_multiple_of(2);
                    let quads = if visible {
                        vec![(
                            renderer::Quad {
                                bounds: Rectangle {
                                    x: (text_bounds.x + x).floor(),
                                    y: text_bounds.y,
                                    width: 1.0,
                                    height: text_bounds.height,
                                },
                                ..renderer::Quad::default()
                            },
                            style.value,
                        )]
                    } else {
                        Vec::new()
                    };
                    (quads, offset, false)
                }
                cursor::State::Selection { start, end } => {
                    let visual = resolved_visual_geometry(
                        state.value.raw(),
                        cursor::State::Selection { start, end },
                        self.visual_geometry,
                        state.visual_caret(),
                    );
                    let (ranges, offset) = if let Some(visual) = visual {
                        (
                            visual.ranges,
                            scroll_offset(visual.focus_x, text_bounds),
                        )
                    } else {
                        let (left, left_offset) =
                            measure_cursor_and_scroll_offset(
                                state.value.raw(),
                                text_bounds,
                                start.min(end),
                                cursor::State::Index(start.min(end)),
                                None,
                                None,
                            );
                        let (right, right_offset) =
                            measure_cursor_and_scroll_offset(
                                state.value.raw(),
                                text_bounds,
                                start.max(end),
                                cursor::State::Index(start.max(end)),
                                None,
                                None,
                            );
                        (
                            vec![(left, right - left)],
                            if end >= start {
                                right_offset
                            } else {
                                left_offset
                            },
                        )
                    };
                    let quads = ranges
                        .into_iter()
                        .map(|(x, width)| {
                            (
                                renderer::Quad {
                                    bounds: Rectangle {
                                        x: text_bounds.x + x,
                                        y: text_bounds.y,
                                        width,
                                        height: text_bounds.height,
                                    },
                                    ..renderer::Quad::default()
                                },
                                style.selection,
                            )
                        })
                        .collect();
                    (quads, offset, true)
                }
            }
        } else {
            (Vec::new(), 0.0, false)
        };

        let draw = |renderer: &mut Renderer, viewport| {
            let paragraph = if text.is_empty()
                && state
                    .preedit
                    .as_ref()
                    .map(|preedit| preedit.content.is_empty())
                    .unwrap_or(true)
            {
                state.placeholder.raw()
            } else {
                state.value.raw()
            };

            let alignment_offset = alignment_offset(
                text_bounds.width,
                paragraph.min_width(),
                self.alignment,
            );

            renderer.with_translation(
                Vector::new(alignment_offset - offset, 0.0),
                |renderer| {
                    for (quad, color) in &quads {
                        renderer.fill_quad(*quad, *color);
                    }
                },
            );

            renderer.fill_paragraph(
                paragraph,
                text_bounds.anchor(
                    paragraph.min_bounds(),
                    Alignment::Start,
                    Alignment::Center,
                ) + Vector::new(alignment_offset - offset, 0.0),
                if text.is_empty() {
                    style.placeholder
                } else {
                    style.value
                },
                viewport,
            );
        };

        if is_selecting {
            if let Some(visible_bounds) = text_bounds.intersection(viewport) {
                renderer.with_layer(visible_bounds, |renderer| {
                    draw(renderer, visible_bounds);
                });
            }
        } else {
            draw(renderer, text_bounds);
        }
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for TextInput<'_, Message, Theme, Renderer>
where
    Message: Clone,
    Theme: Catalog,
    Renderer: text::Renderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State<Renderer::Paragraph>>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::<Renderer::Paragraph>::new())
    }

    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State<Renderer::Paragraph>>();

        // Stop pasting if input becomes disabled
        if self.on_input.is_none() {
            state.is_pasting = None;
        }
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: self.width,
            height: Length::Shrink,
        }
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.layout(tree, renderer, limits, None)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        _renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        let state = tree.state.downcast_mut::<State<Renderer::Paragraph>>();

        operation.text_input(self.id.as_ref(), layout.bounds(), state);
        operation.focusable(self.id.as_ref(), layout.bounds(), state);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let update_cache = |state, value| {
            replace_paragraph(
                renderer,
                state,
                layout,
                value,
                self.font,
                self.size,
                self.line_height,
            );
        };

        match &event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
            | Event::Touch(touch::Event::FingerPressed { .. }) => {
                let state = state::<Renderer>(tree);
                let cursor_before = state.cursor;
                let visual_before = state.visual_selection();

                let click_position = cursor.position_over(layout.bounds());

                state.is_focused = if click_position.is_some() {
                    let now = Instant::now();

                    Some(Focus {
                        updated_at: now,
                        now,
                        is_window_focused: true,
                    })
                } else {
                    None
                };

                if let Some(cursor_position) = click_position {
                    let text_layout = layout.children().next().unwrap();

                    let target = {
                        let text_bounds = text_layout.bounds();

                        let alignment_offset = alignment_offset(
                            text_bounds.width,
                            state.value.raw().min_width(),
                            self.alignment,
                        );

                        cursor_position.x - text_bounds.x - alignment_offset
                    };

                    let click = mouse::Click::new(
                        cursor_position,
                        mouse::Button::Left,
                        state.last_click,
                    );

                    match click.kind() {
                        click::Kind::Single => {
                            let anchor = state
                                .visual_selection()
                                .map(|visual| visual.anchor)
                                .unwrap_or(VisualCaret {
                                    index: state.cursor.start(&self.value),
                                    affinity: CaretAffinity::Strong,
                                });
                            let (position, visual) = if target > 0.0
                                || self.visual_geometry.is_some_and(
                                    |geometry| {
                                        geometry(
                                            state.value.raw(),
                                            state.cursor.state(&self.value),
                                        )
                                        .is_some()
                                    },
                                ) {
                                let value = if self.is_secure {
                                    self.value.secure()
                                } else {
                                    self.value.clone()
                                };

                                find_cursor_position(
                                    text_layout.bounds(),
                                    &value,
                                    state,
                                    target,
                                    self.visual_geometry,
                                )
                            } else {
                                None
                            }
                            .unwrap_or((0, None));

                            if state.keyboard_modifiers.shift() {
                                state.cursor.select_range(
                                    state.cursor.start(&self.value),
                                    position,
                                );
                            } else {
                                state.cursor.move_to(position);
                            }
                            if let Some(visual) =
                                visual.filter(|_| !self.is_secure)
                            {
                                state.remember_visual_selection(
                                    if state.keyboard_modifiers.shift() {
                                        anchor
                                    } else {
                                        visual
                                    },
                                    visual,
                                );
                            } else {
                                state.visual_selection = None;
                            }

                            state.is_dragging = Some(Drag::Select);
                        }
                        click::Kind::Double => {
                            state.visual_selection = None;
                            if self.is_secure {
                                state.cursor.select_all(&self.value);

                                state.is_dragging = None;
                            } else {
                                let position = find_cursor_position(
                                    text_layout.bounds(),
                                    &self.value,
                                    state,
                                    target,
                                    self.visual_geometry,
                                )
                                .map(|(index, _)| index)
                                .unwrap_or(0);

                                state.cursor.select_range(
                                    self.value.previous_start_of_word(position),
                                    self.value.next_end_of_word(position),
                                );

                                state.is_dragging = Some(Drag::SelectWords {
                                    anchor: position,
                                });
                            }
                        }
                        click::Kind::Triple => {
                            state.visual_selection = None;
                            state.cursor.select_all(&self.value);
                            state.is_dragging = None;
                        }
                    }

                    state.last_click = Some(click);

                    if cursor_before != state.cursor
                        || visual_before != state.visual_selection()
                    {
                        shell.request_redraw();
                    }

                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
            | Event::Touch(touch::Event::FingerLifted { .. })
            | Event::Touch(touch::Event::FingerLost { .. }) => {
                state::<Renderer>(tree).is_dragging = None;
            }
            Event::Mouse(mouse::Event::CursorMoved { position })
            | Event::Touch(touch::Event::FingerMoved { position, .. }) => {
                let state = state::<Renderer>(tree);

                if let Some(is_dragging) = &state.is_dragging {
                    let text_layout = layout.children().next().unwrap();

                    let target = {
                        let text_bounds = text_layout.bounds();

                        let alignment_offset = alignment_offset(
                            text_bounds.width,
                            state.value.raw().min_width(),
                            self.alignment,
                        );

                        position.x - text_bounds.x - alignment_offset
                    };

                    let value = if self.is_secure {
                        self.value.secure()
                    } else {
                        self.value.clone()
                    };

                    let (position, visual) = find_cursor_position(
                        text_layout.bounds(),
                        &value,
                        state,
                        target,
                        self.visual_geometry,
                    )
                    .unwrap_or((0, None));

                    let selection_before = state.cursor.selection(&value);
                    let visual_before = state.visual_selection();

                    match is_dragging {
                        Drag::Select => {
                            state.cursor.select_range(
                                state.cursor.start(&value),
                                position,
                            );
                            if let Some(visual) =
                                visual.filter(|_| !self.is_secure)
                            {
                                let anchor = visual_before
                                    .map(|selection| selection.anchor)
                                    .unwrap_or(VisualCaret {
                                        index: state.cursor.start(&value),
                                        affinity: CaretAffinity::Strong,
                                    });
                                state.remember_visual_selection(anchor, visual);
                            } else {
                                state.visual_selection = None;
                            }
                        }
                        Drag::SelectWords { anchor } => {
                            state.visual_selection = None;
                            if position < *anchor {
                                state.cursor.select_range(
                                    self.value.previous_start_of_word(position),
                                    self.value.next_end_of_word(*anchor),
                                );
                            } else {
                                state.cursor.select_range(
                                    self.value.previous_start_of_word(*anchor),
                                    self.value.next_end_of_word(position),
                                );
                            }
                        }
                    }

                    if let Some(focus) = &mut state.is_focused {
                        focus.updated_at = Instant::now();
                    }

                    if selection_before != state.cursor.selection(&value)
                        || visual_before != state.visual_selection()
                    {
                        shell.request_redraw();
                    }

                    shell.capture_event();
                }
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                text,
                modified_key,
                physical_key,
                ..
            }) => {
                let state = state::<Renderer>(tree);

                if let Some(focus) = &mut state.is_focused {
                    let modifiers = state.keyboard_modifiers;

                    match key.to_latin(*physical_key) {
                        Some('c')
                            if state.keyboard_modifiers.command()
                                && !self.is_secure =>
                        {
                            if let Some((start, end)) =
                                state.cursor.selection(&self.value)
                            {
                                clipboard.write(
                                    clipboard::Kind::Standard,
                                    self.value.select(start, end).to_string(),
                                );
                            }

                            shell.capture_event();
                            return;
                        }
                        Some('x')
                            if state.keyboard_modifiers.command()
                                && !self.is_secure =>
                        {
                            let Some(on_input) = &self.on_input else {
                                return;
                            };

                            if let Some((start, end)) =
                                state.cursor.selection(&self.value)
                            {
                                clipboard.write(
                                    clipboard::Kind::Standard,
                                    self.value.select(start, end).to_string(),
                                );
                            }

                            let mut editor =
                                Editor::new(&mut self.value, &mut state.cursor);
                            editor.delete();

                            let message = (on_input)(editor.contents());
                            shell.publish(message);
                            shell.capture_event();

                            focus.updated_at = Instant::now();
                            update_cache(state, &self.value);
                            return;
                        }
                        Some('v')
                            if state.keyboard_modifiers.command()
                                && !state.keyboard_modifiers.alt() =>
                        {
                            let Some(on_input) = &self.on_input else {
                                return;
                            };

                            let content = match state.is_pasting.take() {
                                Some(content) => content,
                                None => {
                                    let content: String = clipboard
                                        .read(clipboard::Kind::Standard)
                                        .unwrap_or_default()
                                        .chars()
                                        .filter(|c| !c.is_control())
                                        .collect();

                                    Value::new(&content)
                                }
                            };

                            let mut editor =
                                Editor::new(&mut self.value, &mut state.cursor);
                            editor.paste(content.clone());

                            let message = if let Some(paste) = &self.on_paste {
                                (paste)(editor.contents())
                            } else {
                                (on_input)(editor.contents())
                            };
                            shell.publish(message);
                            shell.capture_event();

                            state.is_pasting = Some(content);
                            focus.updated_at = Instant::now();
                            update_cache(state, &self.value);
                            return;
                        }
                        Some('a') if state.keyboard_modifiers.command() => {
                            let cursor_before = state.cursor;
                            let had_visual_selection =
                                state.visual_selection.take().is_some();

                            state.cursor.select_all(&self.value);

                            if cursor_before != state.cursor
                                || had_visual_selection
                            {
                                focus.updated_at = Instant::now();

                                shell.request_redraw();
                            }

                            shell.capture_event();
                            return;
                        }
                        _ => {}
                    }

                    if let Some(text) = text {
                        let Some(on_input) = &self.on_input else {
                            return;
                        };

                        state.is_pasting = None;

                        if let Some(c) =
                            text.chars().next().filter(|c| !c.is_control())
                        {
                            let mut editor =
                                Editor::new(&mut self.value, &mut state.cursor);

                            editor.insert(c);

                            let message = (on_input)(editor.contents());
                            shell.publish(message);
                            shell.capture_event();

                            focus.updated_at = Instant::now();
                            update_cache(state, &self.value);
                            return;
                        }
                    }

                    #[cfg(target_os = "macos")]
                    let macos_shortcut =
                        crate::text_editor::convert_macos_shortcut(
                            key, modifiers,
                        );

                    #[cfg(target_os = "macos")]
                    let modified_key =
                        macos_shortcut.as_ref().unwrap_or(modified_key);

                    match modified_key.as_ref() {
                        keyboard::Key::Named(key::Named::Enter) => {
                            if let Some(on_submit) = self.on_submit.clone() {
                                shell.publish(on_submit);
                                shell.capture_event();
                            }
                        }
                        keyboard::Key::Named(key::Named::Backspace) => {
                            state.visual_selection = None;
                            let Some(on_input) = &self.on_input else {
                                return;
                            };

                            if state.cursor.selection(&self.value).is_none() {
                                if (self.is_secure && modifiers.jump())
                                    || modifiers.macos_command()
                                {
                                    state.cursor.select_range(
                                        state.cursor.start(&self.value),
                                        0,
                                    );
                                } else if modifiers.jump() {
                                    state
                                        .cursor
                                        .select_left_by_words(&self.value);
                                }
                            }

                            let mut editor =
                                Editor::new(&mut self.value, &mut state.cursor);
                            editor.backspace();

                            let message = (on_input)(editor.contents());
                            shell.publish(message);
                            shell.capture_event();

                            focus.updated_at = Instant::now();
                            update_cache(state, &self.value);
                        }
                        keyboard::Key::Named(key::Named::Delete) => {
                            state.visual_selection = None;
                            let Some(on_input) = &self.on_input else {
                                return;
                            };

                            if state.cursor.selection(&self.value).is_none() {
                                if (self.is_secure && modifiers.jump())
                                    || modifiers.macos_command()
                                {
                                    state.cursor.select_range(
                                        state.cursor.start(&self.value),
                                        self.value.len(),
                                    );
                                } else if modifiers.jump() {
                                    state
                                        .cursor
                                        .select_right_by_words(&self.value);
                                }
                            }

                            let mut editor =
                                Editor::new(&mut self.value, &mut state.cursor);
                            editor.delete();

                            let message = (on_input)(editor.contents());
                            shell.publish(message);
                            shell.capture_event();

                            focus.updated_at = Instant::now();
                            update_cache(state, &self.value);
                        }
                        keyboard::Key::Named(key::Named::Home) => {
                            let cursor_before = state.cursor;
                            let had_visual_selection =
                                state.visual_selection.take().is_some();

                            if modifiers.shift() {
                                state.cursor.select_range(
                                    state.cursor.start(&self.value),
                                    0,
                                );
                            } else {
                                state.cursor.move_to(0);
                            }

                            if cursor_before != state.cursor
                                || had_visual_selection
                            {
                                focus.updated_at = Instant::now();

                                shell.request_redraw();
                            }

                            shell.capture_event();
                        }
                        keyboard::Key::Named(key::Named::End) => {
                            let cursor_before = state.cursor;
                            let had_visual_selection =
                                state.visual_selection.take().is_some();

                            if modifiers.shift() {
                                state.cursor.select_range(
                                    state.cursor.start(&self.value),
                                    self.value.len(),
                                );
                            } else {
                                state.cursor.move_to(self.value.len());
                            }

                            if cursor_before != state.cursor
                                || had_visual_selection
                            {
                                focus.updated_at = Instant::now();

                                shell.request_redraw();
                            }

                            shell.capture_event();
                        }
                        keyboard::Key::Named(key::Named::ArrowLeft) => {
                            let cursor_before = state.cursor;
                            let visual_before = state
                                .visual_selection
                                .as_ref()
                                .map(|visual| visual.selection);
                            let visual = !self.is_secure
                                && state.preedit.as_ref().is_none_or(
                                    |preedit| preedit.content.is_empty(),
                                )
                                && !modifiers.control()
                                && !modifiers.alt()
                                && !modifiers.logo()
                                && move_visually(
                                    &state.value,
                                    &self.value,
                                    &mut state.cursor,
                                    &mut state.visual_selection,
                                    self.visual_geometry,
                                    false,
                                    modifiers.shift(),
                                );

                            if !visual {
                                state.visual_selection = None;
                                if (self.is_secure && modifiers.jump())
                                    || modifiers.macos_command()
                                {
                                    if modifiers.shift() {
                                        state.cursor.select_range(
                                            state.cursor.start(&self.value),
                                            0,
                                        );
                                    } else {
                                        state.cursor.move_to(0);
                                    }
                                } else if modifiers.jump() {
                                    if modifiers.shift() {
                                        state
                                            .cursor
                                            .select_left_by_words(&self.value);
                                    } else {
                                        state
                                            .cursor
                                            .move_left_by_words(&self.value);
                                    }
                                } else if modifiers.shift() {
                                    state.cursor.select_left(&self.value);
                                } else {
                                    state.cursor.move_left(&self.value);
                                }
                            }
                            if cursor_before != state.cursor
                                || visual_before
                                    != state
                                        .visual_selection
                                        .as_ref()
                                        .map(|visual| visual.selection)
                            {
                                focus.updated_at = Instant::now();

                                shell.request_redraw();
                            }

                            shell.capture_event();
                        }
                        keyboard::Key::Named(key::Named::ArrowRight) => {
                            let cursor_before = state.cursor;
                            let visual_before = state
                                .visual_selection
                                .as_ref()
                                .map(|visual| visual.selection);
                            let visual = !self.is_secure
                                && state.preedit.as_ref().is_none_or(
                                    |preedit| preedit.content.is_empty(),
                                )
                                && !modifiers.control()
                                && !modifiers.alt()
                                && !modifiers.logo()
                                && move_visually(
                                    &state.value,
                                    &self.value,
                                    &mut state.cursor,
                                    &mut state.visual_selection,
                                    self.visual_geometry,
                                    true,
                                    modifiers.shift(),
                                );

                            if !visual {
                                state.visual_selection = None;
                                if (self.is_secure && modifiers.jump())
                                    || modifiers.macos_command()
                                {
                                    if modifiers.shift() {
                                        state.cursor.select_range(
                                            state.cursor.start(&self.value),
                                            self.value.len(),
                                        );
                                    } else {
                                        state.cursor.move_to(self.value.len());
                                    }
                                } else if modifiers.jump() {
                                    if modifiers.shift() {
                                        state
                                            .cursor
                                            .select_right_by_words(&self.value);
                                    } else {
                                        state
                                            .cursor
                                            .move_right_by_words(&self.value);
                                    }
                                } else if modifiers.shift() {
                                    state.cursor.select_right(&self.value);
                                } else {
                                    state.cursor.move_right(&self.value);
                                }
                            }
                            if cursor_before != state.cursor
                                || visual_before
                                    != state
                                        .visual_selection
                                        .as_ref()
                                        .map(|visual| visual.selection)
                            {
                                focus.updated_at = Instant::now();

                                shell.request_redraw();
                            }

                            shell.capture_event();
                        }
                        keyboard::Key::Named(key::Named::Escape) => {
                            state.is_focused = None;
                            state.is_dragging = None;
                            state.is_pasting = None;

                            state.keyboard_modifiers =
                                keyboard::Modifiers::default();

                            shell.capture_event();
                        }
                        _ => {}
                    }
                }
            }
            Event::Keyboard(keyboard::Event::KeyReleased { key, .. }) => {
                let state = state::<Renderer>(tree);

                if state.is_focused.is_some()
                    && let keyboard::Key::Character("v") = key.as_ref()
                {
                    state.is_pasting = None;

                    shell.capture_event();
                }

                state.is_pasting = None;
            }
            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                let state = state::<Renderer>(tree);

                state.keyboard_modifiers = *modifiers;
            }
            Event::InputMethod(event) => match event {
                input_method::Event::Opened | input_method::Event::Closed => {
                    let state = state::<Renderer>(tree);

                    state.preedit =
                        matches!(event, input_method::Event::Opened)
                            .then(input_method::Preedit::new);

                    shell.request_redraw();
                }
                input_method::Event::Preedit(content, selection) => {
                    let state = state::<Renderer>(tree);
                    if !content.is_empty() {
                        state.visual_selection = None;
                    }

                    if state.is_focused.is_some() {
                        state.preedit = Some(input_method::Preedit {
                            content: content.to_owned(),
                            selection: selection.clone(),
                            text_size: self.size,
                        });

                        shell.request_redraw();
                    }
                }
                input_method::Event::Commit(text) => {
                    let state = state::<Renderer>(tree);

                    if let Some(focus) = &mut state.is_focused {
                        let Some(on_input) = &self.on_input else {
                            return;
                        };

                        let mut editor =
                            Editor::new(&mut self.value, &mut state.cursor);
                        editor.paste(Value::new(text));

                        focus.updated_at = Instant::now();
                        state.is_pasting = None;

                        let message = (on_input)(editor.contents());
                        shell.publish(message);
                        shell.capture_event();

                        update_cache(state, &self.value);
                    }
                }
            },
            Event::Window(window::Event::Unfocused) => {
                let state = state::<Renderer>(tree);

                if let Some(focus) = &mut state.is_focused {
                    focus.is_window_focused = false;
                }
            }
            Event::Window(window::Event::Focused) => {
                let state = state::<Renderer>(tree);

                if let Some(focus) = &mut state.is_focused {
                    focus.is_window_focused = true;
                    focus.updated_at = Instant::now();

                    shell.request_redraw();
                }
            }
            Event::Window(window::Event::RedrawRequested(now)) => {
                let state = state::<Renderer>(tree);

                if let Some(focus) = &mut state.is_focused
                    && focus.is_window_focused
                {
                    if matches!(
                        state.cursor.state(&self.value),
                        cursor::State::Index(_)
                    ) {
                        focus.now = *now;

                        let millis_until_redraw = CURSOR_BLINK_INTERVAL_MILLIS
                            - (*now - focus.updated_at).as_millis()
                                % CURSOR_BLINK_INTERVAL_MILLIS;

                        shell.request_redraw_at(
                            *now + Duration::from_millis(
                                millis_until_redraw as u64,
                            ),
                        );
                    }

                    shell.request_input_method(&self.input_method(
                        state,
                        layout,
                        &self.value,
                    ));
                }
            }
            _ => {}
        }

        let state = state::<Renderer>(tree);
        let is_disabled = self.on_input.is_none();

        let status = if is_disabled {
            Status::Disabled
        } else if state.is_focused() {
            Status::Focused {
                is_hovered: cursor.is_over(layout.bounds()),
            }
        } else if cursor.is_over(layout.bounds()) {
            Status::Hovered
        } else {
            Status::Active
        };

        if let Event::Window(window::Event::RedrawRequested(_now)) = event {
            self.last_status = Some(status);
        } else if self
            .last_status
            .is_some_and(|last_status| status != last_status)
        {
            shell.request_redraw();
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.draw(tree, renderer, theme, layout, cursor, None, viewport);
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if cursor.is_over(layout.bounds()) {
            if self.on_input.is_none() {
                mouse::Interaction::Idle
            } else {
                mouse::Interaction::Text
            }
        } else {
            mouse::Interaction::default()
        }
    }
}

impl<'a, Message, Theme, Renderer> From<TextInput<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: Clone + 'a,
    Theme: Catalog + 'a,
    Renderer: text::Renderer + 'a,
{
    fn from(
        text_input: TextInput<'a, Message, Theme, Renderer>,
    ) -> Element<'a, Message, Theme, Renderer> {
        Element::new(text_input)
    }
}

/// The content of the [`Icon`].
#[derive(Debug, Clone)]
pub struct Icon<Font> {
    /// The font that will be used to display the `code_point`.
    pub font: Font,
    /// The unicode code point that will be used as the icon.
    pub code_point: char,
    /// The font size of the content.
    pub size: Option<Pixels>,
    /// The spacing between the [`Icon`] and the text in a [`TextInput`].
    pub spacing: f32,
    /// The side of a [`TextInput`] where to display the [`Icon`].
    pub side: Side,
}

/// The side of a [`TextInput`].
#[derive(Debug, Clone)]
pub enum Side {
    /// The left side of a [`TextInput`].
    Left,
    /// The right side of a [`TextInput`].
    Right,
}

/// The state of a [`TextInput`].
#[derive(Debug, Default, Clone)]
pub struct State<P: text::Paragraph> {
    value: paragraph::Plain<P>,
    placeholder: paragraph::Plain<P>,
    icon: paragraph::Plain<P>,
    is_focused: Option<Focus>,
    is_dragging: Option<Drag>,
    is_pasting: Option<Value>,
    preedit: Option<input_method::Preedit>,
    last_click: Option<mouse::Click>,
    cursor: Cursor,
    visual_selection: Option<RetainedVisualSelection>,
    keyboard_modifiers: keyboard::Modifiers,
    // TODO: Add stateful horizontal scrolling offset
}

#[derive(Debug, Clone)]
struct RetainedVisualSelection {
    selection: VisualSelection,
    cursor: Cursor,
    text: String,
}

fn state<Renderer: text::Renderer>(
    tree: &mut Tree,
) -> &mut State<Renderer::Paragraph> {
    tree.state.downcast_mut::<State<Renderer::Paragraph>>()
}

#[derive(Debug, Clone)]
struct Focus {
    updated_at: Instant,
    now: Instant,
    is_window_focused: bool,
}

#[derive(Debug, Clone)]
enum Drag {
    Select,
    SelectWords { anchor: usize },
}

impl<P: text::Paragraph> State<P> {
    /// Creates a new [`State`], representing an unfocused [`TextInput`].
    pub fn new() -> Self {
        Self::default()
    }

    /// The actual cached shaped value, including secure masking when enabled.
    pub fn paragraph(&self) -> &P {
        self.value.raw()
    }

    /// Exact text corresponding to [`Self::paragraph`].
    pub fn paragraph_text(&self) -> &str {
        self.value.content()
    }

    /// The retained affinities, if they still describe this cursor and cached
    /// paragraph. Resolve their X positions with [`VisualGeometry::caret_x`].
    pub fn visual_selection(&self) -> Option<VisualSelection> {
        self.visual_selection
            .as_ref()
            .filter(|visual| {
                visual.cursor == self.cursor
                    && visual.text == self.value.content()
            })
            .map(|visual| visual.selection)
    }

    /// Retained focus affinity for the actual cached paragraph.
    pub fn visual_caret(&self) -> Option<VisualCaret> {
        self.visual_selection().map(|visual| visual.focus)
    }

    fn remember_visual_selection(
        &mut self,
        anchor: VisualCaret,
        focus: VisualCaret,
    ) {
        self.visual_selection = Some(RetainedVisualSelection {
            selection: VisualSelection { anchor, focus },
            cursor: self.cursor,
            text: self.value.content().to_owned(),
        });
    }

    fn show_cursor_or_selection(
        &self,
        value: &Value,
        disabled: bool,
        retained: bool,
    ) -> bool {
        self.is_focused.as_ref().is_some_and(|focus| {
            focus.is_window_focused
                || (retained
                    && !disabled
                    && matches!(
                        self.cursor.state(value),
                        cursor::State::Selection { .. }
                    ))
        })
    }

    /// Returns whether the [`TextInput`] is currently focused or not.
    pub fn is_focused(&self) -> bool {
        self.is_focused.is_some()
    }

    /// Returns the [`Cursor`] of the [`TextInput`].
    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// Focuses the [`TextInput`].
    pub fn focus(&mut self) {
        let now = Instant::now();

        self.is_focused = Some(Focus {
            updated_at: now,
            now,
            is_window_focused: true,
        });

        self.move_cursor_to_end();
    }

    /// Unfocuses the [`TextInput`].
    pub fn unfocus(&mut self) {
        self.is_focused = None;
    }

    /// Moves the [`Cursor`] of the [`TextInput`] to the front of the input text.
    pub fn move_cursor_to_front(&mut self) {
        self.visual_selection = None;
        self.cursor.move_to(0);
    }

    /// Moves the [`Cursor`] of the [`TextInput`] to the end of the input text.
    pub fn move_cursor_to_end(&mut self) {
        self.visual_selection = None;
        self.cursor.move_to(usize::MAX);
    }

    /// Moves the [`Cursor`] of the [`TextInput`] to an arbitrary location.
    pub fn move_cursor_to(&mut self, position: usize) {
        self.visual_selection = None;
        self.cursor.move_to(position);
    }

    /// Selects all the content of the [`TextInput`].
    pub fn select_all(&mut self) {
        self.visual_selection = None;
        self.cursor.select_range(0, usize::MAX);
    }

    /// Selects the given range of the content of the [`TextInput`].
    pub fn select_range(&mut self, start: usize, end: usize) {
        self.visual_selection = None;
        self.cursor.select_range(start, end);
    }
}

impl<P: text::Paragraph> operation::Focusable for State<P> {
    fn is_focused(&self) -> bool {
        State::is_focused(self)
    }

    fn focus(&mut self) {
        State::focus(self);
    }

    fn unfocus(&mut self) {
        State::unfocus(self);
    }
}

impl<P: text::Paragraph> operation::TextInput for State<P> {
    fn text(&self) -> &str {
        if self.value.content().is_empty() {
            self.placeholder.content()
        } else {
            self.value.content()
        }
    }

    fn move_cursor_to_front(&mut self) {
        State::move_cursor_to_front(self);
    }

    fn move_cursor_to_end(&mut self) {
        State::move_cursor_to_end(self);
    }

    fn move_cursor_to(&mut self, position: usize) {
        State::move_cursor_to(self, position);
    }

    fn select_all(&mut self) {
        State::select_all(self);
    }

    fn select_range(&mut self, start: usize, end: usize) {
        State::select_range(self, start, end);
    }
}

fn move_visually<P: text::Paragraph>(
    paragraph: &paragraph::Plain<P>,
    value: &Value,
    cursor: &mut Cursor,
    retained: &mut Option<RetainedVisualSelection>,
    geometry: Option<VisualGeometryFn<P>>,
    right: bool,
    extend: bool,
) -> bool {
    let Some(visual) = geometry
        .and_then(|geometry| geometry(paragraph.raw(), cursor.state(value)))
    else {
        return false;
    };
    let (anchor, focus) = match cursor.state(value) {
        cursor::State::Index(index) => (index, index),
        cursor::State::Selection { start, end } => (start, end),
    };
    let saved = retained
        .as_ref()
        .filter(|saved| {
            saved.cursor == *cursor && saved.text == paragraph.content()
        })
        .map(|saved| saved.selection);
    let anchor = saved.map(|saved| saved.anchor).unwrap_or(VisualCaret {
        index: anchor,
        affinity: CaretAffinity::Strong,
    });
    let focus = saved
        .map(|saved| saved.focus)
        .or_else(|| visual.caret_near(focus, visual.focus_x));
    let next = if !extend
        && anchor.index != focus.map_or(anchor.index, |focus| focus.index)
    {
        visual.selection_edge(anchor.index, cursor.end(value), right)
    } else {
        focus.and_then(|focus| visual.adjacent_caret(focus, right))
    };
    let Some(next) = next else {
        return false;
    };
    let anchor = if extend {
        cursor.select_range(anchor.index, next.index);
        anchor
    } else {
        cursor.move_to(next.index);
        next
    };
    *retained = Some(RetainedVisualSelection {
        selection: VisualSelection {
            anchor,
            focus: next,
        },
        cursor: *cursor,
        text: paragraph.content().to_owned(),
    });
    true
}

fn offset<P: text::Paragraph>(
    text_bounds: Rectangle,
    value: &Value,
    state: &State<P>,
    geometry: Option<VisualGeometryFn<P>>,
) -> f32 {
    if state.is_focused() {
        let cursor = state.cursor.state(value);
        let focus = match cursor {
            cursor::State::Index(i) => i,
            cursor::State::Selection { end, .. } => end,
        };
        measure_cursor_and_scroll_offset(
            state.value.raw(),
            text_bounds,
            focus,
            cursor,
            geometry,
            state.visual_caret(),
        )
        .1
    } else {
        0.0
    }
}

fn scroll_offset(x: f32, text_bounds: Rectangle) -> f32 {
    ((x + 5.0) - text_bounds.width).max(0.0)
}

fn measure_cursor_and_scroll_offset<P: text::Paragraph>(
    paragraph: &P,
    text_bounds: Rectangle,
    legacy_index: usize,
    cursor: cursor::State,
    geometry: Option<VisualGeometryFn<P>>,
    caret: Option<VisualCaret>,
) -> (f32, f32) {
    let x = resolved_visual_geometry(paragraph, cursor, geometry, caret)
        .map(|geometry| geometry.focus_x)
        .unwrap_or_else(|| {
            paragraph
                .grapheme_position(0, legacy_index)
                .unwrap_or(Point::ORIGIN)
                .x
        });
    (x, scroll_offset(x, text_bounds))
}

fn resolved_visual_geometry<P: text::Paragraph>(
    paragraph: &P,
    cursor: cursor::State,
    geometry: Option<VisualGeometryFn<P>>,
    caret: Option<VisualCaret>,
) -> Option<VisualGeometry> {
    let mut visual = geometry?(paragraph, cursor)?;
    if let Some(x) = caret.and_then(|caret| visual.caret_x(caret)) {
        visual.focus_x = x;
    }
    Some(visual)
}

/// Computes the position of the text cursor at the given X coordinate of
/// a [`TextInput`].
fn find_cursor_position<P: text::Paragraph>(
    text_bounds: Rectangle,
    value: &Value,
    state: &State<P>,
    x: f32,
    geometry: Option<VisualGeometryFn<P>>,
) -> Option<(usize, Option<VisualCaret>)> {
    let visual = resolved_visual_geometry(
        state.value.raw(),
        state.cursor.state(value),
        geometry,
        state.visual_caret(),
    );
    let offset =
        if let Some(visual) = visual.as_ref().filter(|_| state.is_focused()) {
            scroll_offset(visual.focus_x, text_bounds)
        } else {
            offset(text_bounds, value, state, None)
        };
    if let Some(caret) =
        visual.and_then(|visual| visual.hit_test_caret(x + offset))
    {
        return Some((caret.index.min(value.len()), Some(caret)));
    }
    let value = value.to_string();

    let char_offset = state
        .value
        .raw()
        .hit_test(Point::new(x + offset, text_bounds.height / 2.0))
        .map(text::Hit::cursor)?;

    Some((
        unicode_segmentation::UnicodeSegmentation::graphemes(
            &value[..char_offset.min(value.len())],
            true,
        )
        .count(),
        None,
    ))
}

fn replace_paragraph<Renderer>(
    renderer: &Renderer,
    state: &mut State<Renderer::Paragraph>,
    layout: Layout<'_>,
    value: &Value,
    font: Option<Renderer::Font>,
    text_size: Option<Pixels>,
    line_height: text::LineHeight,
) where
    Renderer: text::Renderer,
{
    state.visual_selection = None;
    let font = font.unwrap_or_else(|| renderer.default_font());
    let text_size = text_size.unwrap_or_else(|| renderer.default_size());

    let mut children_layout = layout.children();
    let text_bounds = children_layout.next().unwrap().bounds();

    state.value = paragraph::Plain::new(Text {
        font,
        line_height,
        content: value.to_string(),
        bounds: Size::new(f32::INFINITY, text_bounds.height),
        size: text_size,
        align_x: text::Alignment::Default,
        align_y: alignment::Vertical::Center,
        shaping: text::Shaping::Advanced,
        wrapping: text::Wrapping::default(),
    });
}

const CURSOR_BLINK_INTERVAL_MILLIS: u128 = 500;

/// The possible status of a [`TextInput`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The [`TextInput`] can be interacted with.
    Active,
    /// The [`TextInput`] is being hovered.
    Hovered,
    /// The [`TextInput`] is focused.
    Focused {
        /// Whether the [`TextInput`] is hovered, while focused.
        is_hovered: bool,
    },
    /// The [`TextInput`] cannot be interacted with.
    Disabled,
}

/// The appearance of a text input.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Style {
    /// The [`Background`] of the text input.
    pub background: Background,
    /// The [`Border`] of the text input.
    pub border: Border,
    /// The [`Color`] of the icon of the text input.
    pub icon: Color,
    /// The [`Color`] of the placeholder of the text input.
    pub placeholder: Color,
    /// The [`Color`] of the value of the text input.
    pub value: Color,
    /// The [`Color`] of the selection of the text input.
    pub selection: Color,
}

/// The theme catalog of a [`TextInput`].
pub trait Catalog: Sized {
    /// The item class of the [`Catalog`].
    type Class<'a>;

    /// The default class produced by the [`Catalog`].
    fn default<'a>() -> Self::Class<'a>;

    /// The [`Style`] of a class with the given status.
    fn style(&self, class: &Self::Class<'_>, status: Status) -> Style;
}

/// A styling function for a [`TextInput`].
///
/// This is just a boxed closure: `Fn(&Theme, Status) -> Style`.
pub type StyleFn<'a, Theme> = Box<dyn Fn(&Theme, Status) -> Style + 'a>;

impl Catalog for Theme {
    type Class<'a> = StyleFn<'a, Self>;

    fn default<'a>() -> Self::Class<'a> {
        Box::new(default)
    }

    fn style(&self, class: &Self::Class<'_>, status: Status) -> Style {
        class(self, status)
    }
}

/// The default style of a [`TextInput`].
pub fn default(theme: &Theme, status: Status) -> Style {
    let palette = theme.extended_palette();

    let active = Style {
        background: Background::Color(palette.background.base.color),
        border: Border {
            radius: 2.0.into(),
            width: 1.0,
            color: palette.background.strong.color,
        },
        icon: palette.background.weak.text,
        placeholder: palette.secondary.base.color,
        value: palette.background.base.text,
        selection: palette.primary.weak.color,
    };

    match status {
        Status::Active => active,
        Status::Hovered => Style {
            border: Border {
                color: palette.background.base.text,
                ..active.border
            },
            ..active
        },
        Status::Focused { .. } => Style {
            border: Border {
                color: palette.primary.strong.color,
                ..active.border
            },
            ..active
        },
        Status::Disabled => Style {
            background: Background::Color(palette.background.weak.color),
            value: active.placeholder,
            placeholder: palette.background.strongest.color,
            ..active
        },
    }
}

fn alignment_offset(
    text_bounds_width: f32,
    text_min_width: f32,
    alignment: alignment::Horizontal,
) -> f32 {
    if text_min_width > text_bounds_width {
        0.0
    } else {
        match alignment {
            alignment::Horizontal::Left => 0.0,
            alignment::Horizontal::Center => {
                (text_bounds_width - text_min_width) / 2.0
            }
            alignment::Horizontal::Right => text_bounds_width - text_min_width,
        }
    }
}
