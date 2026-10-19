//! Desktop pop-up controls use iced's field and scrolling menu, adding keyboard
//! focus, highlighted navigation and type selection without committing on hover.
use super::{accessibility, motion, style};
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer, text,
    widget::{Id, Operation, Tree, operation::Focusable, tree},
};
use iced::widget::overlay::menu;
pub(super) use iced::widget::pick_list::{Status, Style};
use iced::{
    Border, Color, Element, Event, Length, Padding, Pixels, Point, Rectangle, Size, Theme, Vector,
    keyboard, mouse, touch, window,
};
use std::{
    borrow::Borrow,
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};
mod scroll_probe;

#[derive(Default)]
pub(super) struct PopupMarker {
    pub open: bool,
}

pub(super) struct PickList<'a, T, M, R = iced::Renderer>
where
    T: ToString + PartialEq + Clone,
    R: text::Renderer,
{
    inner: iced::widget::pick_list::PickList<'a, T, Rc<[T]>, T, M, Theme, R>,
    options: Rc<[T]>,
    selected: Option<T>,
    on_select: Rc<dyn Fn(T) -> M + 'a>,
    menu_style: menu::StyleFn<'a, Theme>,
    text_size: Option<Pixels>,
    line_height: text::LineHeight,
    shaping: text::Shaping,
    font: Option<R::Font>,
    padding: Padding,
    menu_height: Length,
    enabled: bool,
    id: Option<Id>,
    name: Option<String>,
    description: Option<String>,
    invalid: Option<String>,
}

pub(super) fn pick_list<'a, T, M, L, V>(
    options: L,
    selected: Option<V>,
    on_select: impl Fn(T) -> M + 'a,
) -> PickList<'a, T, M>
where
    T: ToString + PartialEq + Clone + 'a,
    M: Clone + 'static,
    L: Borrow<[T]>,
    V: Borrow<T>,
{
    PickList::new(options, selected, on_select)
}

impl<'a, T: ToString + PartialEq + Clone + 'a, M: Clone + 'static, R: text::Renderer + 'a>
    PickList<'a, T, M, R>
{
    pub(super) fn new<L: Borrow<[T]>, V: Borrow<T>>(
        options: L,
        selected: Option<V>,
        on_select: impl Fn(T) -> M + 'a,
    ) -> Self {
        let options: Rc<[T]> = options.borrow().into();
        let selected = selected.map(|value| value.borrow().clone());
        let on_select: Rc<dyn Fn(T) -> M + 'a> = Rc::new(on_select);
        let callback = on_select.clone();
        Self {
            inner: iced::widget::pick_list::PickList::new(
                options.clone(),
                selected.clone(),
                move |value| callback(value),
            ),
            options,
            selected,
            on_select,
            menu_style: Box::new(menu::default),
            text_size: None,
            line_height: text::LineHeight::default(),
            shaping: text::Shaping::default(),
            font: None,
            padding: Padding::from(5),
            menu_height: Length::Shrink,
            enabled: true,
            id: None,
            name: None,
            description: None,
            invalid: None,
        }
    }
    pub(super) fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
    pub(super) fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
    pub(super) fn invalid(mut self, invalid: Option<String>) -> Self {
        self.invalid = invalid;
        self
    }
    pub(super) fn placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.inner = self.inner.placeholder(placeholder);
        self
    }
    pub(super) fn width(mut self, width: impl Into<Length>) -> Self {
        self.inner = self.inner.width(width);
        self
    }
    pub(super) fn text_size(mut self, size: impl Into<Pixels>) -> Self {
        let size = size.into();
        self.text_size = Some(size);
        self.inner = self.inner.text_size(size);
        self
    }
    pub(super) fn padding(mut self, padding: impl Into<Padding>) -> Self {
        let padding = padding.into();
        self.padding = padding;
        self.inner = self.inner.padding(padding);
        self
    }
    pub(super) fn style(mut self, style: impl Fn(&Theme, Status) -> Style + 'a) -> Self {
        self.inner = self.inner.style(style);
        self
    }
    pub(super) fn menu_style(mut self, style: impl Fn(&Theme) -> menu::Style + 'a) -> Self {
        self.menu_style = Box::new(style);
        self
    }
    #[allow(dead_code)]
    pub(super) fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
    #[allow(dead_code)]
    pub(super) fn id(mut self, id: impl Into<Id>) -> Self {
        self.id = Some(id.into());
        self
    }
    fn selected_index(&self) -> Option<usize> {
        self.options
            .iter()
            .position(|value| Some(value) == self.selected.as_ref())
    }
    fn available(&self) -> bool {
        self.enabled && !self.options.is_empty()
    }
}

struct State<M> {
    focused: bool,
    window_focused: bool,
    popup: PopupMarker,
    menu: menu::State,
    highlighted: Option<usize>,
    query: String,
    typed_at: Option<Instant>,
    ensure_visible: bool,
    control: accessibility::Control<M>,
    choices: Vec<accessibility::Control<M>>,
    option_labels: Vec<String>,
    selected_label: Option<String>,
    pending: Vec<M>,
    menu_bounds: Option<Rectangle>,
    row_height: f32,
    scroll_offset: f32,
}
impl<M> Default for State<M> {
    fn default() -> Self {
        Self {
            focused: false,
            window_focused: true,
            popup: PopupMarker::default(),
            menu: menu::State::default(),
            highlighted: None,
            query: String::new(),
            typed_at: None,
            ensure_visible: false,
            control: accessibility::Control::new(accessibility::Role::ComboBox),
            choices: Vec::new(),
            option_labels: Vec::new(),
            selected_label: None,
            pending: Vec::new(),
            menu_bounds: None,
            row_height: 0.0,
            scroll_offset: 0.0,
        }
    }
}
impl<M> Focusable for State<M> {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn focus(&mut self) {
        self.focused = true;
    }
    fn unfocus(&mut self) {
        self.focused = false;
        self.close();
    }
}
impl<M> State<M> {
    fn open(&mut self, selected: Option<usize>) {
        self.popup.open = true;
        self.highlighted = selected.or(Some(0));
        self.query.clear();
        self.typed_at = None;
        self.ensure_visible = true;
    }
    fn close(&mut self) {
        self.popup.open = false;
        self.query.clear();
        self.typed_at = None;
    }
    fn alt_key(
        &mut self,
        key: &keyboard::Key,
        modifiers: keyboard::Modifiers,
        selected: Option<usize>,
    ) -> bool {
        if !modifiers.alt() || modifiers.control() || modifiers.logo() {
            return false;
        }
        match key {
            keyboard::Key::Named(keyboard::key::Named::ArrowDown) => {
                if !self.popup.open {
                    self.open(selected);
                }
            }
            keyboard::Key::Named(keyboard::key::Named::ArrowUp) => self.close(),
            _ => return false,
        }
        true
    }
    /// A handled key and an optional index to commit. Navigation changes only
    /// the highlight; Enter or Space confirms, and Escape preserves the value.
    fn key<T: ToString>(
        &mut self,
        key: &keyboard::Key,
        options: &[T],
        selected: Option<usize>,
        now: Instant,
    ) -> Option<Option<usize>> {
        use keyboard::key::Named;
        if options.is_empty() {
            return None;
        }
        match key {
            keyboard::Key::Named(Named::Tab) => {
                self.close();
                return None;
            }
            keyboard::Key::Named(Named::Escape) => {
                if self.popup.open {
                    self.close();
                } else {
                    self.unfocus();
                }
                return Some(None);
            }
            keyboard::Key::Named(Named::Enter | Named::Space) => {
                if self.popup.open {
                    let selected = self.highlighted;
                    self.close();
                    return Some(selected);
                }
                self.open(selected);
                return Some(None);
            }
            _ => {}
        }
        let navigation = match key {
            keyboard::Key::Named(Named::ArrowDown | Named::ArrowRight) => Some(1isize),
            keyboard::Key::Named(Named::ArrowUp | Named::ArrowLeft) => Some(-1),
            keyboard::Key::Named(Named::PageDown) => Some(10),
            keyboard::Key::Named(Named::PageUp) => Some(-10),
            keyboard::Key::Named(Named::Home) => Some(isize::MIN),
            keyboard::Key::Named(Named::End) => Some(isize::MAX),
            _ => None,
        };
        if let Some(step) = navigation {
            let first_choice = !self.popup.open && selected.is_none();
            if !self.popup.open {
                self.open(selected);
            }
            let current = self.highlighted.unwrap_or(0);
            self.highlighted = Some(match step {
                isize::MIN => 0,
                isize::MAX => options.len() - 1,
                -1 => {
                    if current == 0 {
                        options.len() - 1
                    } else {
                        current - 1
                    }
                }
                1 if first_choice => 0,
                1 => (current + 1) % options.len(),
                step => (current as isize + step).clamp(0, options.len() as isize - 1) as usize,
            });
            self.query.clear();
            self.ensure_visible = true;
            return Some(None);
        }
        if let keyboard::Key::Character(typed) = key
            && !typed.is_empty()
            && typed.chars().all(|c| !c.is_control())
        {
            if !self.popup.open {
                self.open(selected);
            }
            let typed = typed.to_lowercase();
            let fresh = self
                .typed_at
                .is_none_or(|last| now.duration_since(last) > Duration::from_millis(750));
            if fresh {
                self.query.clear();
            }
            // Repeated letters cycle through the matching choices. Multiple
            // letters typed together search by prefix, including Unicode text.
            let cycling = self.query == typed;
            if !cycling {
                self.query.push_str(&typed);
            }
            let start = if cycling {
                self.highlighted.map_or(0, |index| index + 1)
            } else {
                0
            };
            let find = |prefix: &str| {
                (0..options.len())
                    .map(|offset| (start + offset) % options.len())
                    .find(|&index| {
                        options[index]
                            .to_string()
                            .to_lowercase()
                            .starts_with(prefix)
                    })
            };
            let found = find(&self.query).or_else(|| {
                self.query.clone_from(&typed);
                find(&self.query)
            });
            if let Some(index) = found {
                self.highlighted = Some(index);
                self.ensure_visible = true;
            }
            self.typed_at = Some(now);
            return Some(None);
        }
        None
    }
}

impl<'a, T: ToString + PartialEq + Clone + 'a, M: Clone + 'static, R: text::Renderer + 'a>
    Widget<M, Theme, R> for PickList<'a, T, M, R>
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State<M>>()
    }
    fn state(&self) -> tree::State {
        let state = State::<M> {
            choices: self
                .options
                .iter()
                .map(|_| accessibility::Control::new(accessibility::Role::ListBoxOption))
                .collect(),
            option_labels: self.options.iter().map(ToString::to_string).collect(),
            selected_label: self.selected.as_ref().map(ToString::to_string),
            ..State::default()
        };
        tree::State::new(state)
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.inner as &dyn Widget<M, Theme, R>)]
    }
    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&[&self.inner as &dyn Widget<M, Theme, R>]);
        let state = tree.state.downcast_mut::<State<M>>();
        let labels: Vec<_> = self.options.iter().map(ToString::to_string).collect();
        let selected = self.selected.as_ref().map(ToString::to_string);
        if labels != state.option_labels || selected != state.selected_label {
            state.close();
            state.option_labels = labels;
            state.selected_label = selected;
        }
        state.choices.truncate(self.options.len());
        state.choices.resize_with(self.options.len(), || {
            accessibility::Control::new(accessibility::Role::ListBoxOption)
        });
        if !self.available() {
            state.unfocus();
        }
        if state
            .highlighted
            .is_some_and(|index| index >= self.options.len())
        {
            state.highlighted = self.selected_index();
        }
    }
    fn size(&self) -> Size<Length> {
        self.inner.size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.inner.size_hint()
    }
    fn layout(&mut self, tree: &mut Tree, renderer: &R, limits: &layout::Limits) -> layout::Node {
        self.inner.layout(&mut tree.children[0], renderer, limits)
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &R,
        operation: &mut dyn Operation,
    ) {
        let state = tree.state.downcast_mut::<State<M>>();
        let control = &mut state.control;
        if let Some(id) = &self.id {
            control.focus_id = id.clone();
        }
        let mut node = accessibility::Node::new(accessibility::Role::ComboBox);
        node.set_label(self.name.as_deref().unwrap_or("Options"));
        if let Some(error) = &self.invalid {
            node.set_invalid(accesskit::Invalid::True);
            node.set_description(error.as_str());
        } else if let Some(description) = &self.description {
            node.set_description(description.as_str());
        }
        node.set_value(
            self.selected
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
        );
        node.set_expanded(state.popup.open);
        node.set_has_popup(accesskit::HasPopup::Listbox);
        if state.popup.open {
            node.set_children(
                state
                    .choices
                    .iter()
                    .map(|choice| choice.id)
                    .collect::<Vec<_>>(),
            );
        }
        if self.available() {
            node.add_action(accesskit::Action::Click);
            node.add_action(if state.popup.open {
                accesskit::Action::Collapse
            } else {
                accesskit::Action::Expand
            });
        } else {
            node.set_disabled();
        }
        control.node = node;
        control.focused = state.focused;
        let selected = self.selected_index();
        let next = selected.map_or(0, |index| index + 1);
        control.increment = (self.available() && next < self.options.len())
            .then(|| (self.on_select)(self.options[next].clone()));
        control.decrement = selected
            .filter(|&index| self.available() && index > 0)
            .map(|index| (self.on_select)(self.options[index - 1].clone()));
        let focus_id = control.focus_id.clone();
        operation.custom(Some(&focus_id), layout.bounds(), control);
        if self.available()
            && let Some(request) = state.control.requested.take()
        {
            match request.action {
                accesskit::Action::Expand => {
                    state.focused = true;
                    state.open(self.selected_index());
                }
                accesskit::Action::Collapse => state.close(),
                accesskit::Action::Click => {
                    state.focused = true;
                    if state.popup.open {
                        state.close();
                    } else {
                        state.open(self.selected_index());
                    }
                }
                _ => {}
            }
        }
        operation.custom(Some(&focus_id), layout.bounds(), &mut state.popup);
        if self.available() {
            operation.focusable(Some(&focus_id), layout.bounds(), state);
        }
        if state.popup.open {
            let bounds = state.menu_bounds.unwrap_or(layout.bounds());
            for (index, option) in self.options.iter().enumerate() {
                let choice = &mut state.choices[index];
                choice.focus_id = focus_id.clone();
                choice.focused = state.highlighted == Some(index);
                choice.window_coordinates = state.menu_bounds;
                choice.node = accessibility::Node::new(accessibility::Role::ListBoxOption);
                choice.node.set_label(option.to_string());
                choice
                    .node
                    .set_selected(Some(option) == self.selected.as_ref());
                choice.node.add_action(accesskit::Action::Click);
                choice.node.add_action(accesskit::Action::ScrollIntoView);
                let choice_bounds = Rectangle {
                    x: bounds.x,
                    y: bounds.y + index as f32 * state.row_height - state.scroll_offset,
                    width: bounds.width,
                    height: state.row_height,
                };
                operation.custom(Some(&focus_id), choice_bounds, choice);
                if let Some(request) = choice.requested.take() {
                    match request.action {
                        accesskit::Action::Click => {
                            state.pending.push((self.on_select)(option.clone()));
                            state.close();
                        }
                        accesskit::Action::Focus | accesskit::Action::ScrollIntoView => {
                            state.focused = true;
                            state.highlighted = Some(index);
                            state.ensure_visible = true;
                        }
                        _ => {}
                    }
                }
            }
        }
        self.inner
            .operate(&mut tree.children[0], layout, renderer, operation);
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &R,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, M>,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State<M>>();
        if matches!(event, Event::Window(window::Event::RedrawRequested(_))) {
            for message in state.pending.drain(..) {
                shell.publish(message);
            }
        }
        if !self.available() {
            state.unfocus();
            // iced's field accepts command-wheel selection even without an
            // open menu. Keep modifier/window bookkeeping, but disabled and
            // empty controls must not publish a selection through that path.
            if !matches!(
                event,
                Event::Window(_) | Event::Keyboard(keyboard::Event::ModifiersChanged(_))
            ) {
                return;
            }
        }
        match event {
            // With a menu open, wheel gestures belong to its scrollable list.
            // The inner field has no private popup open, so keep its optional
            // command-wheel shortcut from committing an unrelated selection.
            Event::Mouse(mouse::Event::WheelScrolled { .. }) if state.popup.open => return,
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
            | Event::Touch(touch::Event::FingerPressed { .. }) => {
                if state.popup.open {
                    state.close();
                    shell.capture_event();
                    shell.request_redraw();
                } else if self.available()
                    && cursor.is_over(layout.bounds())
                    && !shell.is_event_captured()
                {
                    state.focused = true;
                    state.open(self.selected_index());
                    shell.capture_event();
                    shell.request_redraw();
                } else if state.focused {
                    state.unfocus();
                    shell.request_redraw();
                }
                // Our menu uses iced's native pointer list. Do not also open
                // the inner field's private menu state.
                return;
            }
            Event::Window(window::Event::Unfocused) => {
                state.window_focused = false;
                state.close();
            }
            Event::Window(window::Event::Focused) => state.window_focused = true,
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. })
                if state.focused
                    && state.window_focused
                    && self.available()
                    && !shell.is_event_captured()
                    && state.alt_key(key, *modifiers, self.selected_index()) =>
            {
                shell.capture_event();
                shell.request_redraw();
                return;
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                modifiers,
                repeat,
                ..
            }) if state.focused
                && state.window_focused
                && self.available()
                && !shell.is_event_captured()
                && !modifiers.control()
                && !modifiers.logo()
                && !modifiers.alt() =>
            {
                if *repeat
                    && matches!(
                        key,
                        keyboard::Key::Named(
                            keyboard::key::Named::Enter | keyboard::key::Named::Space
                        )
                    )
                {
                    shell.capture_event();
                    return;
                }
                if let Some(commit) =
                    state.key(key, &self.options, self.selected_index(), Instant::now())
                {
                    if let Some(index) = commit
                        && let Some(option) = self.options.get(index)
                    {
                        shell.publish((self.on_select)(option.clone()));
                    }
                    shell.capture_event();
                    shell.request_redraw();
                    return;
                }
            }
            _ => {}
        }
        self.inner.update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut R,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.inner.draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
        let state = tree.state.downcast_ref::<State<M>>();
        if state.focused && state.window_focused && self.available() {
            renderer.fill_quad(
                renderer::Quad {
                    bounds: layout.bounds(),
                    border: Border {
                        color: theme.extended_palette().primary.strong.color,
                        width: if motion::increase_contrast() {
                            3.0
                        } else {
                            2.0
                        },
                        radius: style::RADIUS_SMALL.into(),
                    },
                    ..renderer::Quad::default()
                },
                Color::TRANSPARENT,
            );
        }
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &R,
    ) -> mouse::Interaction {
        if self.available() {
            self.inner
                .mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
        } else {
            mouse::Interaction::None
        }
    }
    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        _renderer: &R,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, M, Theme, R>> {
        let state = tree.state.downcast_mut::<State<M>>();
        if !state.popup.open || !self.available() {
            return None;
        }
        Some(overlay::Element::new(Box::new(Popup {
            state: RefCell::new(state),
            options: &self.options,
            selected: self.selected_index(),
            on_select: &*self.on_select,
            class: &self.menu_style,
            position: layout.position() + translation,
            viewport: *viewport,
            width: layout.bounds().width,
            target_height: layout.bounds().height,
            padding: self.padding,
            text_size: self.text_size,
            line_height: self.line_height,
            shaping: self.shaping,
            font: self.font,
            menu_height: self.menu_height,
        })))
    }
}

impl<'a, T: ToString + PartialEq + Clone + 'a, M: Clone + 'static, R: text::Renderer + 'a>
    From<PickList<'a, T, M, R>> for Element<'a, M, Theme, R>
{
    fn from(pick: PickList<'a, T, M, R>) -> Self {
        Element::new(pick)
    }
}

struct Popup<'a, 'b, T, M, R: text::Renderer> {
    state: RefCell<&'a mut State<M>>,
    options: &'a [T],
    selected: Option<usize>,
    on_select: &'a dyn Fn(T) -> M,
    class: &'a menu::StyleFn<'b, Theme>,
    position: Point,
    viewport: Rectangle,
    width: f32,
    target_height: f32,
    padding: Padding,
    text_size: Option<Pixels>,
    line_height: text::LineHeight,
    shaping: text::Shaping,
    font: Option<R::Font>,
    menu_height: Length,
}
impl<T: ToString + Clone, M, R: text::Renderer> Popup<'_, '_, T, M, R> {
    fn with_menu<O, MenuRenderer: text::Renderer<Font = R::Font>>(
        &self,
        default_font: R::Font,
        f: impl FnOnce(&mut overlay::Element<'_, M, Theme, MenuRenderer>) -> O,
    ) -> O {
        let mut state = self.state.borrow_mut();
        let State {
            menu,
            highlighted,
            popup,
            ..
        } = &mut **state;
        let on_select = self.on_select;
        let mut menu = menu::Menu::new(
            menu,
            self.options,
            highlighted,
            |value| {
                popup.open = false;
                on_select(value)
            },
            None,
            self.class,
        )
        .width(self.width)
        .padding(self.padding)
        .font(self.font.unwrap_or(default_font))
        .text_line_height(self.line_height)
        .text_shaping(self.shaping);
        if let Some(size) = self.text_size {
            menu = menu.text_size(size);
        }
        f(&mut menu.overlay(
            self.position,
            self.viewport,
            self.target_height,
            self.menu_height,
        ))
    }
    fn row_height(&self, renderer: &R) -> f32 {
        f32::from(
            self.line_height
                .to_absolute(self.text_size.unwrap_or_else(|| renderer.default_size())),
        ) + self.padding.y()
    }
    fn read_scroll(&self, renderer: &R, layout: Layout<'_>) {
        let mut probe = scroll_probe::Probe::new(renderer);
        self.with_menu(renderer.default_font(), |menu| {
            menu.as_overlay().draw(
                &mut probe,
                &Theme::Light,
                &renderer::Style::default(),
                layout,
                mouse::Cursor::Unavailable,
            );
        });
        self.state.borrow_mut().scroll_offset = probe.offset;
    }
}
impl<T: ToString + Clone, M, R: text::Renderer> overlay::Overlay<M, Theme, R>
    for Popup<'_, '_, T, M, R>
{
    fn layout(&mut self, renderer: &R, bounds: Size) -> layout::Node {
        let node = self.with_menu(renderer.default_font(), |menu| {
            menu.as_overlay_mut().layout(renderer, bounds)
        });
        let row = self.row_height(renderer);
        {
            let mut state = self.state.borrow_mut();
            state.menu_bounds = Some(node.bounds());
            state.row_height = row;
        }
        let highlight = {
            let mut state = self.state.borrow_mut();
            if std::mem::take(&mut state.ensure_visible) {
                state.highlighted
            } else {
                None
            }
        };
        if let Some(index) = highlight {
            let offset = (index as f32 * row - (node.size().height - row) / 2.0).max(0.0);
            self.state.borrow_mut().scroll_offset =
                offset.min((self.options.len() as f32 * row - node.size().height).max(0.0));
            // Reset first, so keyboard focus also recovers after a scrollbar
            // drag or touch scroll. The menu keeps iced's scrolling behavior.
            for y in [self.options.len() as f32 * row, -offset] {
                self.with_menu(renderer.default_font(), |menu| {
                    let mut messages = Vec::new();
                    menu.as_overlay_mut().update(
                        &Event::Mouse(mouse::Event::WheelScrolled {
                            delta: mouse::ScrollDelta::Pixels { x: 0.0, y },
                        }),
                        Layout::new(&node),
                        mouse::Cursor::Available(node.bounds().center()),
                        renderer,
                        &mut iced::advanced::clipboard::Null,
                        &mut Shell::new(&mut messages),
                    );
                });
            }
        }
        self.read_scroll(renderer, Layout::new(&node));
        node
    }
    fn update(
        &mut self,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &R,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, M>,
    ) {
        if !self.state.borrow().popup.open {
            return;
        }
        if let Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) = event
            && self
                .state
                .borrow_mut()
                .alt_key(key, *modifiers, self.selected)
        {
            shell.capture_event();
            shell.request_redraw();
            return;
        }
        if let Event::Keyboard(keyboard::Event::KeyPressed {
            key,
            modifiers,
            repeat,
            ..
        }) = event
            && !modifiers.control()
            && !modifiers.logo()
            && !modifiers.alt()
        {
            if *repeat
                && matches!(
                    key,
                    keyboard::Key::Named(keyboard::key::Named::Enter | keyboard::key::Named::Space)
                )
            {
                shell.capture_event();
                return;
            }
            let result =
                self.state
                    .borrow_mut()
                    .key(key, self.options, self.selected, Instant::now());
            if let Some(commit) = result {
                if let Some(index) = commit
                    && let Some(option) = self.options.get(index)
                {
                    shell.publish((self.on_select)(option.clone()));
                }
                shell.capture_event();
                shell.request_redraw();
                return;
            }
        }
        self.with_menu(renderer.default_font(), |menu| {
            menu.as_overlay_mut()
                .update(event, layout, cursor, renderer, clipboard, shell)
        });
        self.read_scroll(renderer, layout);
    }
    fn draw(
        &self,
        renderer: &mut R,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        if !self.state.borrow().popup.open {
            return;
        }
        self.with_menu(renderer.default_font(), |menu| {
            menu.as_overlay()
                .draw(renderer, theme, style, layout, cursor)
        });
    }
    fn mouse_interaction(
        &self,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &R,
    ) -> mouse::Interaction {
        if !self.state.borrow().popup.open {
            return mouse::Interaction::None;
        }
        self.with_menu(renderer.default_font(), |menu| {
            menu.as_overlay()
                .mouse_interaction(layout, cursor, renderer)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::event;
    use keyboard::{Key, key::Named};

    #[test]
    fn invalid_choices_expose_their_error_and_valid_choices_keep_help_text() {
        for error in [
            None,
            Some("Choose a camera that is still connected.".to_owned()),
        ] {
            let mut pick = PickList::<_, _, ()>::new(["sim:0"], Some("sim:0"), |value| value)
                .name("Step 1 camera")
                .description("The camera this step controls.")
                .invalid(error.clone());
            let mut tree = Tree::new(&pick as &dyn Widget<&str, Theme, ()>);
            let node = pick.layout(
                &mut tree,
                &(),
                &layout::Limits::new(Size::ZERO, Size::new(200.0, 30.0)),
            );
            let mut operation = iced::advanced::widget::operation::focusable::count();
            pick.operate(
                &mut tree,
                Layout::new(&node),
                &(),
                &mut iced::advanced::widget::operation::black_box(&mut operation),
            );
            let control = &tree.state.downcast_ref::<State<&str>>().control;
            assert_eq!(control.node.label(), Some("Step 1 camera"));
            assert_eq!(
                control.node.invalid(),
                error.as_ref().map(|_| accesskit::Invalid::True)
            );
            assert_eq!(
                control.node.description(),
                Some(error.as_deref().unwrap_or("The camera this step controls."))
            );
            assert_eq!(control.node.role(), accessibility::Role::ComboBox);
        }
    }

    fn press(key: Key) -> Event {
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        })
    }

    #[test]
    fn popup_navigation_highlights_until_confirmed_and_escape_cancels() {
        let options = ["Red", "Green", "Blue"];
        let mut state = State::<()>::default();
        let now = Instant::now();
        assert_eq!(
            state.key(&Key::Named(Named::Space), &options, Some(0), now),
            Some(None)
        );
        assert!(state.popup.open);
        assert_eq!(
            state.key(&Key::Named(Named::ArrowDown), &options, Some(0), now),
            Some(None)
        );
        assert_eq!(state.highlighted, Some(1));
        assert_eq!(
            state.key(&Key::Named(Named::Escape), &options, Some(0), now),
            Some(None)
        );
        assert!(
            !state.popup.open,
            "Escape must preserve the existing selection"
        );
        state.key(&Key::Named(Named::Space), &options, Some(0), now);
        state.key(&Key::Named(Named::End), &options, Some(0), now);
        assert_eq!(
            state.key(&Key::Named(Named::Enter), &options, Some(0), now),
            Some(Some(2))
        );
        assert!(!state.popup.open);
        state.key(&Key::Named(Named::ArrowDown), &options, None, now);
        assert_eq!(
            state.highlighted,
            Some(0),
            "a control with no value begins at the first choice"
        );
        state.key(&Key::Named(Named::ArrowUp), &options, None, now);
        assert_eq!(state.highlighted, Some(2), "popup arrows cycle");
    }

    #[test]
    fn typing_searches_prefixes_cycles_letters_and_resets_after_pause() {
        let options = ["Red", "Green", "Blue", "Black", "Écru"];
        let mut state = State::<()>::default();
        let now = Instant::now();
        state.key(&Key::Character("b".into()), &options, None, now);
        assert_eq!(state.highlighted, Some(2));
        state.key(&Key::Character("b".into()), &options, None, now);
        assert_eq!(state.highlighted, Some(3));
        state.key(&Key::Character("l".into()), &options, None, now);
        assert_eq!(state.query, "bl");
        assert_eq!(state.highlighted, Some(2));
        state.key(
            &Key::Character("É".into()),
            &options,
            None,
            now + Duration::from_secs(1),
        );
        assert_eq!(
            state.highlighted,
            Some(4),
            "non-ASCII labels support type selection"
        );
    }

    #[test]
    fn alt_arrows_open_and_dismiss_without_advancing_the_current_choice() {
        let mut state = State::<()>::default();
        assert!(state.alt_key(
            &Key::Named(Named::ArrowDown),
            keyboard::Modifiers::ALT,
            Some(1)
        ));
        assert!(state.popup.open);
        assert_eq!(state.highlighted, Some(1));
        assert!(state.alt_key(
            &Key::Named(Named::ArrowDown),
            keyboard::Modifiers::ALT,
            Some(1)
        ));
        assert_eq!(
            state.highlighted,
            Some(1),
            "holding Alt+Down does not advance"
        );
        assert!(!state.alt_key(
            &Key::Named(Named::ArrowDown),
            keyboard::Modifiers::ALT | keyboard::Modifiers::CTRL,
            Some(1)
        ));
        assert!(state.alt_key(
            &Key::Named(Named::ArrowUp),
            keyboard::Modifiers::ALT,
            Some(1)
        ));
        assert!(!state.popup.open);
    }

    #[test]
    fn keyboard_selection_and_pointer_selection_share_the_native_menu() {
        let mut pick =
            PickList::<_, _, ()>::new(["Red", "Green", "Blue"], Some("Red"), |value| value)
                .text_size(14)
                .padding([5, 8]);
        let mut tree = Tree::new(&pick as &dyn Widget<&str, Theme, ()>);
        tree.state.downcast_mut::<State<&str>>().focus();
        let node = pick.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(200.0, 30.0)),
        );
        let viewport = Rectangle::with_size(Size::new(300.0, 300.0));
        let mut messages = Vec::new();
        let mut clipboard = iced::advanced::clipboard::Null;
        let mut shell = Shell::new(&mut messages);
        pick.update(
            &mut tree,
            &press(Key::Named(Named::Space)),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &(),
            &mut clipboard,
            &mut shell,
            &viewport,
        );
        assert_eq!(
            shell.event_status(),
            event::Status::Captured,
            "Space cannot toggle a camera stream"
        );
        for event in [
            Event::Keyboard(keyboard::Event::ModifiersChanged(
                keyboard::Modifiers::COMMAND,
            )),
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 },
            }),
        ] {
            pick.update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Available(node.bounds().center()),
                &(),
                &mut clipboard,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        }
        assert!(messages.is_empty(), "wheel cannot commit an open menu");
        let mut overlay = pick
            .overlay(&mut tree, Layout::new(&node), &(), &viewport, Vector::ZERO)
            .unwrap();
        let popup = overlay.as_overlay_mut().layout(&(), viewport.size());
        for event in [
            press(Key::Named(Named::ArrowDown)),
            press(Key::Named(Named::Enter)),
        ] {
            overlay.as_overlay_mut().update(
                &event,
                Layout::new(&popup),
                mouse::Cursor::Unavailable,
                &(),
                &mut clipboard,
                &mut Shell::new(&mut messages),
            );
        }
        drop(overlay);
        assert_eq!(messages, ["Green"]);
        assert!(!tree.state.downcast_ref::<State<&str>>().popup.open);
        // Reopen by pointer; the native menu still owns hit-testing and hover.
        pick.update(
            &mut tree,
            &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Layout::new(&node),
            mouse::Cursor::Available(node.bounds().center()),
            &(),
            &mut clipboard,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        let mut overlay = pick
            .overlay(&mut tree, Layout::new(&node), &(), &viewport, Vector::ZERO)
            .unwrap();
        let popup = overlay.as_overlay_mut().layout(&(), viewport.size());
        let cursor = Point::new(popup.bounds().center_x(), popup.bounds().y + 8.0);
        for event in [
            Event::Mouse(mouse::Event::CursorMoved { position: cursor }),
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
        ] {
            overlay.as_overlay_mut().update(
                &event,
                Layout::new(&popup),
                mouse::Cursor::Available(cursor),
                &(),
                &mut clipboard,
                &mut Shell::new(&mut messages),
            );
        }
        drop(overlay);
        assert_eq!(messages, ["Green", "Red"], "pointer behavior is retained");
    }

    #[test]
    fn disabled_and_empty_pick_lists_are_not_tab_stops() {
        for (options, enabled) in [(vec!["Red"], false), (vec![], true)] {
            let mut pick =
                PickList::<_, _, ()>::new(options, Some("Red"), |value| value).enabled(enabled);
            let mut tree = Tree::new(&pick as &dyn Widget<&str, Theme, ()>);
            let node = pick.layout(
                &mut tree,
                &(),
                &layout::Limits::new(Size::ZERO, Size::new(200.0, 30.0)),
            );
            let mut count = iced::advanced::widget::operation::focusable::count();
            pick.operate(
                &mut tree,
                Layout::new(&node),
                &(),
                &mut iced::advanced::widget::operation::black_box(&mut count),
            );
            let iced::advanced::widget::operation::Outcome::Some(count) = count.finish() else {
                panic!("missing focus count")
            };
            assert_eq!(count.total, 0);
            let viewport = Rectangle::with_size(Size::new(200.0, 100.0));
            let mut messages = Vec::new();
            for event in [
                Event::Keyboard(keyboard::Event::ModifiersChanged(
                    keyboard::Modifiers::COMMAND,
                )),
                Event::Mouse(mouse::Event::WheelScrolled {
                    delta: mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 },
                }),
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                press(Key::Named(Named::Space)),
            ] {
                pick.update(
                    &mut tree,
                    &event,
                    Layout::new(&node),
                    mouse::Cursor::Available(node.bounds().center()),
                    &(),
                    &mut iced::advanced::clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &viewport,
                );
            }
            assert!(messages.is_empty(), "disabled controls cannot select");
            assert!(
                pick.overlay(&mut tree, Layout::new(&node), &(), &viewport, Vector::ZERO)
                    .is_none(),
                "disabled controls cannot open a menu"
            );
        }
    }

    #[test]
    fn long_menu_reveals_end_and_recovers_home_after_pointer_scrolling() {
        let options: Vec<_> = (0..40).map(|index| format!("Choice {index}")).collect();
        let mut pick =
            PickList::<_, _, ()>::new(&options[..], Some(options[0].clone()), |value| value)
                .text_size(14)
                .padding(5);
        let mut tree = Tree::new(&pick as &dyn Widget<String, Theme, ()>);
        tree.state.downcast_mut::<State<String>>().focus();
        let node = pick.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(200.0, 30.0)),
        );
        let viewport = Rectangle::with_size(Size::new(220.0, 160.0));
        let mut messages = Vec::new();
        for (key, at_bottom) in [(Named::End, true), (Named::Home, false)] {
            pick.update(
                &mut tree,
                &press(Key::Named(Named::Space)),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &(),
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            let mut overlay = pick
                .overlay(&mut tree, Layout::new(&node), &(), &viewport, Vector::ZERO)
                .unwrap();
            let initial = overlay.as_overlay_mut().layout(&(), viewport.size());
            // A previous pointer scroll must not prevent Home from revealing
            // the first choice in iced's retained scrolling menu.
            overlay.as_overlay_mut().update(
                &Event::Mouse(mouse::Event::WheelScrolled {
                    delta: mouse::ScrollDelta::Pixels { x: 0.0, y: -500.0 },
                }),
                Layout::new(&initial),
                mouse::Cursor::Available(initial.bounds().center()),
                &(),
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
            );
            overlay.as_overlay_mut().update(
                &press(Key::Named(key)),
                Layout::new(&initial),
                mouse::Cursor::Unavailable,
                &(),
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
            );
            let popup = overlay.as_overlay_mut().layout(&(), viewport.size());
            let cursor = Point::new(
                popup.bounds().center_x(),
                if at_bottom {
                    popup.bounds().y + popup.bounds().height - 5.0
                } else {
                    popup.bounds().y + 5.0
                },
            );
            for event in [
                Event::Mouse(mouse::Event::CursorMoved { position: cursor }),
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            ] {
                overlay.as_overlay_mut().update(
                    &event,
                    Layout::new(&popup),
                    mouse::Cursor::Available(cursor),
                    &(),
                    &mut iced::advanced::clipboard::Null,
                    &mut Shell::new(&mut messages),
                );
            }
        }
        assert_eq!(messages, ["Choice 39", "Choice 0"]);
    }

    #[test]
    fn native_popup_actions_open_reveal_and_select_through_normal_messages() {
        struct NativeAction {
            target: accessibility::NodeId,
            action: accesskit::Action,
        }
        impl Operation for NativeAction {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
                if let Some(control) = state.downcast_mut::<accessibility::Control<&str>>()
                    && control.id == self.target
                {
                    control.requested = Some(accesskit::ActionRequest {
                        action: self.action,
                        target: self.target,
                        data: None,
                    });
                }
            }
        }
        let mut pick =
            PickList::<_, _, ()>::new(["Red", "Green", "Blue"], Some("Green"), |value| value)
                .name("Color");
        let mut tree = Tree::new(&pick as &dyn Widget<&str, Theme, ()>);
        let node = pick.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(200.0, 30.0)),
        );
        let (control, choice) = {
            let state = tree.state.downcast_ref::<State<&str>>();
            (state.control.id, state.choices[2].id)
        };
        for (target, action) in [
            (control, accesskit::Action::Expand),
            (choice, accesskit::Action::ScrollIntoView),
        ] {
            pick.operate(
                &mut tree,
                Layout::new(&node),
                &(),
                &mut NativeAction { target, action },
            );
        }
        let state = tree.state.downcast_ref::<State<&str>>();
        assert!(state.focused && state.popup.open);
        assert_eq!(state.highlighted, Some(2));
        assert_eq!(state.control.node.label(), Some("Color"));
        assert_eq!(state.control.node.value(), Some("Green"));
        assert_eq!(state.control.node.children().len(), 3);
        assert_eq!(state.choices[1].node.is_selected(), Some(true));
        pick.operate(
            &mut tree,
            Layout::new(&node),
            &(),
            &mut NativeAction {
                target: choice,
                action: accesskit::Action::Click,
            },
        );
        assert!(!tree.state.downcast_ref::<State<&str>>().popup.open);
        let mut messages = Vec::new();
        for _ in 0..2 {
            pick.update(
                &mut tree,
                &Event::Window(window::Event::RedrawRequested(Instant::now())),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &(),
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &Rectangle::with_size(Size::new(300.0, 300.0)),
            );
        }
        assert_eq!(messages, ["Blue"], "a native choice commits exactly once");
    }

    #[test]
    fn semantic_option_bounds_follow_the_native_scrollbar_drag() {
        struct OptionBounds {
            id: accessibility::NodeId,
            bounds: Option<Rectangle>,
        }
        impl Operation for OptionBounds {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
                if let Some(control) = state.downcast_ref::<accessibility::Control<String>>()
                    && control.id == self.id
                {
                    self.bounds = Some(bounds);
                }
            }
        }
        let options: Vec<_> = (0..40).map(|index| format!("Choice {index}")).collect();
        let mut pick =
            PickList::<_, _, ()>::new(&options[..], Some(options[0].clone()), |value| value)
                .text_size(14)
                .padding(5);
        let mut tree = Tree::new(&pick as &dyn Widget<String, Theme, ()>);
        tree.state.downcast_mut::<State<String>>().focus();
        let node = pick.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(200.0, 30.0)),
        );
        let viewport = Rectangle::with_size(Size::new(220.0, 160.0));
        let mut messages = Vec::new();
        pick.update(
            &mut tree,
            &press(Key::Named(Named::Space)),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &(),
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        let mut overlay = pick
            .overlay(&mut tree, Layout::new(&node), &(), &viewport, Vector::ZERO)
            .unwrap();
        let popup = overlay.as_overlay_mut().layout(&(), viewport.size());
        let bounds = popup.bounds();
        let start = Point::new(bounds.x + bounds.width - 4.0, bounds.y + 5.0);
        let end = Point::new(start.x, bounds.y + bounds.height - 5.0);
        for (event, cursor) in [
            (
                Event::Mouse(mouse::Event::CursorMoved { position: start }),
                start,
            ),
            (
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                start,
            ),
            (
                Event::Mouse(mouse::Event::CursorMoved { position: end }),
                end,
            ),
            (
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                end,
            ),
        ] {
            overlay.as_overlay_mut().update(
                &event,
                Layout::new(&popup),
                mouse::Cursor::Available(cursor),
                &(),
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
            );
        }
        drop(overlay);
        let state = tree.state.downcast_ref::<State<String>>();
        assert!(
            state.scroll_offset > 0.0,
            "the native scrollbar really moved"
        );
        let mut option = OptionBounds {
            id: state.choices[39].id,
            bounds: None,
        };
        pick.operate(&mut tree, Layout::new(&node), &(), &mut option);
        let point = option.bounds.unwrap().center();
        assert!(
            bounds.contains(point),
            "the last option's native bounds are visible"
        );
        let mut overlay = pick
            .overlay(&mut tree, Layout::new(&node), &(), &viewport, Vector::ZERO)
            .unwrap();
        let popup = overlay.as_overlay_mut().layout(&(), viewport.size());
        for event in [
            Event::Mouse(mouse::Event::CursorMoved { position: point }),
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
        ] {
            overlay.as_overlay_mut().update(
                &event,
                Layout::new(&popup),
                mouse::Cursor::Available(point),
                &(),
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
            );
        }
        assert_eq!(
            messages,
            ["Choice 39"],
            "native hit-testing agrees with accessibility bounds"
        );
    }
}
