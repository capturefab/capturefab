//! Editable fields retain iced's editor and clipboard while exposing names,
//! current drafts, invalid state, secure values and native text selection.
use super::accessibility::{Control, Node, Role, ValueHandler};
use accesskit::{Action, ActionData, Invalid};
#[cfg(test)]
use accesskit::{TextPosition, TextSelection};
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer, text,
    widget::{Id, Operation, Tree, tree},
};
use iced::widget::text_input::{self, Value, cursor};
pub(super) use iced::widget::text_input::{Icon, Status, Style};
use iced::{
    Alignment, Element, Event, Length, Padding, Pixels, Rectangle, Size, Theme, Vector, mouse,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

mod native_geometry;
mod selection;

pub(super) struct TextInput<'a, M, R: text::Renderer = iced::Renderer> {
    inner: text_input::TextInput<'a, M, Theme, R>,
    id: Id,
    name: String,
    description: Option<String>,
    invalid: Option<String>,
    value: String,
    secure: bool,
    on_input: Option<ValueHandler<M>>,
}

pub(super) fn text_input<'a, M: Clone + 'static>(
    placeholder: &str,
    value: &str,
) -> TextInput<'a, M> {
    TextInput::new(placeholder, value)
}

impl<'a, M: Clone + 'static, R: text::Renderer + 'a> TextInput<'a, M, R> {
    pub(super) fn new(placeholder: &str, value: &str) -> Self
    where
        R::Paragraph: selection::ParagraphGeometry,
    {
        let id = Id::unique();
        Self {
            inner: text_input::TextInput::new(placeholder, value)
                .id(id.clone())
                .show_unfocused_selection(true)
                .visual_geometry(selection::geometry::<R::Paragraph>),
            id,
            name: placeholder.to_owned(),
            description: None,
            invalid: None,
            value: value.to_owned(),
            secure: false,
            on_input: None,
        }
    }
    pub(super) fn id(mut self, id: impl Into<Id>) -> Self {
        self.id = id.into();
        self.inner = self.inner.id(self.id.clone());
        self
    }
    pub(super) fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }
    pub(super) fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
    pub(super) fn invalid(mut self, error: Option<String>) -> Self {
        self.invalid = error;
        self
    }
    pub(super) fn secure(mut self, secure: bool) -> Self {
        self.secure = secure;
        self.inner = self.inner.secure(secure);
        self
    }
    pub(super) fn on_input(self, handler: impl Fn(String) -> M + Send + Sync + 'static) -> Self {
        self.on_input_maybe(Some(handler))
    }
    pub(super) fn on_input_maybe(
        mut self,
        handler: Option<impl Fn(String) -> M + Send + Sync + 'static>,
    ) -> Self {
        self.on_input = handler.map(|handler| Arc::new(handler) as ValueHandler<M>);
        let input = self.on_input.clone();
        self.inner = self
            .inner
            .on_input_maybe(input.map(|input| move |value| input(value)));
        self
    }
    pub(super) fn on_submit(self, message: M) -> Self {
        self.on_submit_maybe(Some(message))
    }
    pub(super) fn on_submit_maybe(mut self, message: Option<M>) -> Self {
        self.inner = self.inner.on_submit_maybe(message);
        self
    }
    pub(super) fn icon(mut self, icon: Icon<R::Font>) -> Self {
        self.inner = self.inner.icon(icon);
        self
    }
    pub(super) fn width(mut self, width: impl Into<Length>) -> Self {
        self.inner = self.inner.width(width);
        self
    }
    pub(super) fn padding(mut self, padding: impl Into<Padding>) -> Self {
        self.inner = self.inner.padding(padding);
        self
    }
    pub(super) fn size(mut self, size: impl Into<Pixels>) -> Self {
        self.inner = self.inner.size(size);
        self
    }
    pub(super) fn align_x(mut self, alignment: Alignment) -> Self {
        self.inner = self.inner.align_x(alignment);
        self
    }
    pub(super) fn style(mut self, style: impl Fn(&Theme, Status) -> Style + 'a) -> Self {
        self.inner = self.inner.style(style);
        self
    }
}

struct State<M> {
    control: Control<M>,
    text_id: accesskit::NodeId,
    native_input: NativeInput,
    geometry: native_geometry::State,
    focus_return: super::keyboard_history::FocusReturn,
    ime: ImeMarker,
}

#[derive(Default)]
pub(in crate::gui) struct ImeMarker {
    pub composing: bool,
}

impl<M> Default for State<M> {
    fn default() -> Self {
        let control = Control::new(Role::TextInput);
        let focus_return = super::keyboard_history::FocusReturn::new(control.focus_id.clone());
        Self {
            control,
            text_id: Control::<()>::new(Role::TextRun).id,
            native_input: NativeInput::default(),
            geometry: native_geometry::State::default(),
            focus_return,
            ime: ImeMarker::default(),
        }
    }
}

/// Native edits wait for the ordinary application message to be handled and
/// for the widget to observe the resulting draft. Later native edits use that
/// authoritative draft and cursor, including when validation refuses a change.
#[derive(Clone, Default)]
pub(super) struct NativeInput(Arc<Mutex<NativeState>>);

#[derive(Default)]
struct NativeState {
    value: String,
    selection: Option<cursor::State>,
    next: u64,
    pending: Option<PendingEdit>,
    selection_pending: bool,
    deferred: VecDeque<accesskit::ActionRequest>,
    released: Option<u64>,
}

struct PendingEdit {
    token: u64,
    base: String,
    expected: String,
    caret: usize,
    acknowledged: bool,
    observed: bool,
}

#[derive(Clone)]
pub(super) struct Acknowledgement {
    input: NativeInput,
    token: u64,
}

impl std::fmt::Debug for Acknowledgement {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Drafts can contain passwords; a message's Debug output must not
        // expose this private widget state.
        formatter
            .debug_struct("NativeInputAcknowledgement")
            .field("token", &self.token)
            .finish_non_exhaustive()
    }
}

impl Acknowledgement {
    pub(super) fn is_current(&self) -> bool {
        self.input
            .0
            .lock()
            .unwrap()
            .pending
            .as_ref()
            .is_some_and(|pending| pending.token == self.token && !pending.acknowledged)
    }

    pub(super) fn acknowledge(&self) {
        let mut state = self.input.0.lock().unwrap();
        if let Some(pending) = &mut state.pending
            && pending.token == self.token
        {
            pending.acknowledged = true;
        }
    }
}

impl NativeInput {
    pub(super) fn defer(&self, request: accesskit::ActionRequest, replay: Option<u64>) -> bool {
        let mut state = self.0.lock().unwrap();
        if let Some(token) = replay {
            if state.released == Some(token) {
                state.released = None;
                return false;
            }
            // A modal, disabled field or removed node invalidated this replay.
            return true;
        }
        if state.pending.is_none()
            && !state.selection_pending
            && state.released.is_none()
            && state.deferred.is_empty()
        {
            return false;
        }
        if state.deferred.len() < 64 {
            state.deferred.push_back(request);
        }
        true
    }

    pub(super) fn acknowledgement(&self) -> Option<Acknowledgement> {
        self.0
            .lock()
            .unwrap()
            .pending
            .as_ref()
            .map(|pending| Acknowledgement {
                input: self.clone(),
                token: pending.token,
            })
    }

    pub(super) fn begin_selection(&self) {
        self.0.lock().unwrap().selection_pending = true;
    }

    fn finish_selection(&self) {
        self.0.lock().unwrap().selection_pending = false;
    }

    pub(super) fn release(&self) -> Option<(accesskit::ActionRequest, u64)> {
        let mut state = self.0.lock().unwrap();
        if state.selection_pending
            || state
                .pending
                .as_ref()
                .is_some_and(|pending| !pending.observed)
        {
            return None;
        }
        if state.pending.is_some() {
            state.pending = None;
        }
        if state.released.is_some() {
            return None;
        }
        let request = state.deferred.pop_front()?;
        state.next += 1;
        let token = state.next;
        state.released = Some(token);
        Some((request, token))
    }

    pub(super) fn retry(&self, request: accesskit::ActionRequest) {
        let mut state = self.0.lock().unwrap();
        state.released = None;
        state.deferred.push_front(request);
    }

    pub(super) fn cancel(&self) {
        let mut state = self.0.lock().unwrap();
        state.pending = None;
        state.selection_pending = false;
        state.deferred.clear();
        state.released = None;
    }

    fn prepare(&self, text: String, replace: bool) -> String {
        let mut state = self.0.lock().unwrap();
        let value = Value::new(&state.value);
        let selection = state.selection.unwrap_or(cursor::State::Index(value.len()));
        let (expected, caret) = if replace {
            replacement_edit(&value, selection, &text)
        } else {
            let caret = Value::new(&text).len();
            (text, caret)
        };
        state.next += 1;
        state.pending = Some(PendingEdit {
            token: state.next,
            base: state.value.clone(),
            expected: expected.clone(),
            caret,
            acknowledged: false,
            observed: false,
        });
        expected
    }

    fn observe<P: text::Paragraph>(&self, draft: &str, editor: &mut text_input::State<P>) {
        let value = Value::new(draft);
        let mut state = self.0.lock().unwrap();
        if let Some(pending) = &mut state.pending
            && pending.acknowledged
            && !pending.observed
        {
            // An unchanged base with a different requested draft means the
            // normal validation path refused the change. Preserve selection.
            if draft == pending.expected || draft != pending.base {
                editor.move_cursor_to(pending.caret.min(value.len()));
            }
            pending.observed = true;
        }
        state.value = draft.to_owned();
        state.selection = Some(editor.cursor().state(&value));
    }
}

#[cfg(test)]
fn replacement(value: &Value, selection: cursor::State, replacement: &str) -> String {
    replacement_edit(value, selection, replacement).0
}

fn replacement_edit(value: &Value, selection: cursor::State, replacement: &str) -> (String, usize) {
    let (start, end) = match selection {
        cursor::State::Index(at) => (at, at),
        cursor::State::Selection { start, end } => (start.min(end), start.max(end)),
    };
    let prefix = format!("{}{}", value.until(start), replacement);
    let expected = format!(
        "{}{}{}",
        value.until(start),
        replacement,
        value.select(end, value.len())
    );
    // Re-segmentation can merge inserted combining marks or ZWJ sequences with
    // surrounding text. The caret is the boundary after the inserted prefix.
    let caret = Value::new(&prefix).len().min(Value::new(&expected).len());
    (expected, caret)
}

impl<'a, M: Clone + 'static, R: text::Renderer + 'a> Widget<M, Theme, R> for TextInput<'a, M, R>
where
    R::Paragraph: selection::ParagraphGeometry,
{
    fn size(&self) -> Size<Length> {
        Widget::size(&self.inner)
    }
    fn size_hint(&self) -> Size<Length> {
        self.inner.size_hint()
    }
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State<M>>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(State::<M>::default())
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.inner as &dyn Widget<M, Theme, R>)]
    }
    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&[&self.inner as &dyn Widget<M, Theme, R>]);
        let state = tree.state.downcast_mut::<State<M>>();
        let editor = tree.children[0]
            .state
            .downcast_mut::<text_input::State<R::Paragraph>>();
        if self.on_input.is_none() {
            state.ime.composing = false;
            state.focus_return.pending |= editor.is_focused();
            editor.unfocus();
            state.native_input.cancel();
        }
        state.native_input.observe(&self.value, editor);
    }
    fn layout(&mut self, tree: &mut Tree, renderer: &R, limits: &layout::Limits) -> layout::Node {
        Widget::layout(&mut self.inner, &mut tree.children[0], renderer, limits)
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
        Widget::draw(
            &self.inner,
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &R,
        operation: &mut dyn Operation,
    ) {
        let origin = self.inner.text_origin(&tree.children[0], layout);
        let viewport = layout.children().next().unwrap().bounds();
        let state = tree.state.downcast_mut::<State<M>>();
        let editor = tree.children[0]
            .state
            .downcast_mut::<text_input::State<R::Paragraph>>();
        if self.on_input.is_none() {
            state.focus_return.pending |= editor.is_focused();
            editor.unfocus();
            state.native_input.cancel();
        }
        state.native_input.observe(&self.value, editor);
        let value = Value::new(&self.value);
        let selection = editor.cursor().state(&value);
        let control = &mut state.control;
        control.node = Node::new(if self.secure {
            Role::PasswordInput
        } else {
            Role::TextInput
        });
        control.node.set_label(if self.name.is_empty() {
            "Text field"
        } else {
            &self.name
        });
        let visible = if self.secure {
            value.secure()
        } else {
            value.clone()
        };
        control.node.set_value(visible.to_string());
        if let Some(error) = &self.invalid {
            control.node.set_invalid(Invalid::True);
            control.node.set_description(error.as_str());
        } else if let Some(description) = &self.description {
            control.node.set_description(description.as_str());
        }
        if self.on_input.is_none() {
            control.node.set_disabled();
        }
        control.focus_id = self.id.clone();
        control.focusable = self.on_input.is_some();
        control.focused = editor.is_focused();
        // Native text-field focus/selection must not become AXPress, which
        // submits a form. Keyboard Return remains the inner editor's action.
        control.activate = None;
        control.native_input = self.on_input.as_ref().map(|_| state.native_input.clone());
        control.set_value = self.on_input.as_ref().map(|handler| {
            let handler = handler.clone();
            let input = state.native_input.clone();
            Arc::new(move |text: String| handler(input.prepare(text, false))) as ValueHandler<M>
        });
        control.replace_selected_text = self.on_input.as_ref().map(|handler| {
            let handler = handler.clone();
            let input = state.native_input.clone();
            Arc::new(move |text: String| handler(input.prepare(text, true))) as ValueHandler<M>
        });
        let geometry = state.geometry.update(
            editor.paragraph(),
            editor.paragraph_text(),
            &visible,
            selection,
            origin,
            viewport,
            state.text_id,
        );
        if let Some(selection) = geometry.selection {
            control.node.set_text_selection(selection);
            if self.on_input.is_some() {
                control.node.add_action(Action::SetTextSelection);
            }
        }
        control.node.set_children(
            geometry
                .children
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
        );
        control.children = geometry.children;
        state.focus_return.id = self.id.clone();
        state.focus_return.available = self.on_input.is_some();
        operation.custom(Some(&self.id), layout.bounds(), &mut state.focus_return);
        if !editor.is_focused() {
            state.ime.composing = false;
        }
        operation.custom(Some(&self.id), layout.bounds(), &mut state.ime);
        operation.custom(Some(&self.id), layout.bounds(), control);
        if let Some(request) = control.requested.take()
            && request.action == Action::SetTextSelection
        {
            if self.on_input.is_some()
                && let Some(ActionData::SetTextSelection(selection)) = request.data
                && let Some((anchor, focus)) = state.geometry.selection_indices(&selection)
            {
                editor.focus();
                editor.select_range(anchor.min(value.len()), focus.min(value.len()));
            }
            // Selection Tasks are applied after model messages drain. A
            // queued replacement must wait for this actual editor mutation,
            // including when an invalid selection request changes nothing.
            state.native_input.finish_selection();
            state.native_input.observe(&self.value, editor);
        }
        if self.on_input.is_some() {
            self.inner
                .operate(&mut tree.children[0], layout, renderer, operation);
        }
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
        let editor_focused = tree.children[0]
            .state
            .downcast_ref::<text_input::State<R::Paragraph>>()
            .is_focused();
        let state = tree.state.downcast_mut::<State<M>>();
        match event {
            Event::InputMethod(iced::advanced::input_method::Event::Preedit(text, _)) => {
                state.ime.composing = editor_focused && self.on_input.is_some() && !text.is_empty();
            }
            Event::InputMethod(
                iced::advanced::input_method::Event::Commit(_)
                | iced::advanced::input_method::Event::Closed,
            )
            | Event::Window(iced::window::Event::Unfocused) => state.ime.composing = false,
            Event::Keyboard(iced::keyboard::Event::KeyPressed {
                key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter),
                modifiers,
                ..
            }) if editor_focused && state.ime.composing && modifiers.is_empty() => {
                shell.capture_event();
                return;
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
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &R,
    ) -> mouse::Interaction {
        self.inner
            .mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }
    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &R,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, M, Theme, R>> {
        self.inner.overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, M: Clone + 'static, R: text::Renderer + 'a> From<TextInput<'a, M, R>>
    for Element<'a, M, Theme, R>
where
    R::Paragraph: selection::ParagraphGeometry,
{
    fn from(input: TextInput<'a, M, R>) -> Self {
        Self::new(input)
    }
}

#[cfg(test)]
mod render_tests;

#[cfg(test)]
mod tests {
    use super::*;

    struct Inspect;
    impl Operation for Inspect {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }
    }

    fn inspect(input: &mut TextInput<'_, String, ()>, tree: &mut Tree) {
        let node = input.layout(
            tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(240.0, 30.0)),
        );
        input.operate(tree, Layout::new(&node), &(), &mut Inspect);
    }

    fn editor(tree: &mut Tree) -> &mut text_input::State<()> {
        tree.children[0].state.downcast_mut()
    }

    fn request(target: accesskit::NodeId, text: &str) -> accesskit::ActionRequest {
        accesskit::ActionRequest {
            action: Action::ReplaceSelectedText,
            target,
            data: Some(ActionData::Value(text.into())),
        }
    }

    #[test]
    fn accepted_native_replacements_collapse_the_actual_editor_after_acknowledgement() {
        for (base, start, end, inserted, accepted, caret, next) in [
            ("abcd", 1, 3, "x", "axd", 2, "axyd"),
            ("abcd", 3, 1, "", "ad", 1, "ayd"),
            ("a🇨🇦e\u{301}z", 3, 1, "👩‍🚀🇨🇦", "a👩‍🚀🇨🇦z", 3, "a👩‍🚀🇨🇦yz"),
            ("az", 1, 1, "\u{301}", "a\u{301}z", 1, "a\u{301}yz"),
        ] {
            let mut input = TextInput::<String, ()>::new("Value", base).on_input(|value| value);
            let mut tree = Tree::new(&input as &dyn Widget<String, Theme, ()>);
            editor(&mut tree).select_range(start, end);
            let original_selection = editor(&mut tree).cursor().state(&Value::new(base));
            inspect(&mut input, &mut tree);
            let state = tree.state.downcast_ref::<State<String>>();
            let native = state.native_input.clone();
            let draft = state.control.replace_selected_text.as_ref().unwrap()(inserted.into());
            assert_eq!(draft, accepted);
            let acknowledgement = native.acknowledgement().unwrap();
            // Widget operations and a view diff can occur before the normal
            // model callback. Neither is an acknowledgement of acceptance.
            input.diff(&mut tree);
            inspect(&mut input, &mut tree);
            assert_eq!(
                editor(&mut tree).cursor().state(&Value::new(base)),
                original_selection
            );
            assert!(native.release().is_none());
            acknowledgement.acknowledge();
            input = TextInput::new("Value", accepted).on_input(|value| value);
            input.diff(&mut tree);
            inspect(&mut input, &mut tree);
            assert_eq!(
                editor(&mut tree).cursor().state(&Value::new(accepted)),
                cursor::State::Index(caret)
            );
            native.release();
            let control = &tree.state.downcast_ref::<State<String>>().control;
            assert_eq!(
                control.replace_selected_text.as_ref().unwrap()("y".into()),
                next
            );
        }
    }

    #[test]
    fn queued_edits_use_normalized_or_rejected_drafts_and_have_distinct_replay_tokens() {
        for (actual, selection, expected_next) in [
            ("abcd", cursor::State::Selection { start: 1, end: 3 }, "aXd"),
            ("Q", cursor::State::Index(1), "QX"),
        ] {
            let mut input = TextInput::<String, ()>::new("Value", "abcd").on_input(|value| value);
            let mut tree = Tree::new(&input as &dyn Widget<String, Theme, ()>);
            editor(&mut tree).select_range(1, 3);
            inspect(&mut input, &mut tree);
            let state = tree.state.downcast_ref::<State<String>>();
            let native = state.native_input.clone();
            let second = request(state.control.id, "X");
            assert_eq!(
                state.control.replace_selected_text.as_ref().unwrap()("refused".into()),
                "arefusedd"
            );
            let acknowledgement = native.acknowledgement().unwrap();
            assert!(native.defer(second.clone(), None));
            input.diff(&mut tree);
            inspect(&mut input, &mut tree);
            assert!(native.release().is_none());
            acknowledgement.acknowledge();
            input = TextInput::new("Value", actual).on_input(|value| value);
            input.diff(&mut tree);
            inspect(&mut input, &mut tree);
            assert_eq!(
                editor(&mut tree).cursor().state(&Value::new(actual)),
                selection
            );
            let (released, token) = native.release().unwrap();
            assert_eq!(released, second);
            // An identical fresh OS request must remain queued rather than
            // consume the in-flight replay's identity.
            assert!(native.defer(second.clone(), None));
            assert!(!native.defer(released, Some(token)));
            let control = &tree.state.downcast_ref::<State<String>>().control;
            assert_eq!(
                control.replace_selected_text.as_ref().unwrap()("X".into()),
                expected_next
            );
            native.cancel();
            assert!(!native.acknowledgement().is_some_and(|ack| ack.is_current()));
            assert!(native.defer(second, Some(token)));
            assert!(native.release().is_none());
        }
    }

    #[test]
    fn native_set_value_places_the_real_caret_at_the_valid_grapheme_end() {
        let mut input = TextInput::<String, ()>::new("Value", "old").on_input(|value| value);
        let mut tree = Tree::new(&input as &dyn Widget<String, Theme, ()>);
        editor(&mut tree).select_range(0, 2);
        inspect(&mut input, &mut tree);
        let state = tree.state.downcast_ref::<State<String>>();
        assert_eq!(
            state.control.set_value.as_ref().unwrap()("👩‍🚀e\u{301}".into()),
            "👩‍🚀e\u{301}"
        );
        state.native_input.acknowledgement().unwrap().acknowledge();
        input = TextInput::new("Value", "👩‍🚀e\u{301}").on_input(|value| value);
        input.diff(&mut tree);
        assert_eq!(
            editor(&mut tree).cursor().state(&Value::new("👩‍🚀e\u{301}")),
            cursor::State::Index(2)
        );
    }

    #[test]
    fn disabled_editors_leave_focus_traversal_and_refuse_native_selection_without_hiding_values() {
        struct Count(usize);
        impl Operation for Count {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn focusable(
                &mut self,
                _: Option<&Id>,
                _: Rectangle,
                _: &mut dyn iced::advanced::widget::operation::Focusable,
            ) {
                self.0 += 1;
            }
        }
        let mut input = TextInput::<String, ()>::new("Value", "readable").on_input(|value| value);
        let mut tree = Tree::new(&input as &dyn Widget<String, Theme, ()>);
        editor(&mut tree).focus();
        inspect(&mut input, &mut tree);
        let node = input.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(240.0, 30.0)),
        );
        let mut count = Count(0);
        input.operate(&mut tree, Layout::new(&node), &(), &mut count);
        assert_eq!(count.0, 1);
        input = TextInput::new("Value", "readable")
            .on_input_maybe(None::<fn(String) -> String>)
            .on_submit_maybe(None);
        input.diff(&mut tree);
        input.operate(&mut tree, Layout::new(&node), &(), &mut count);
        assert_eq!(
            count.0, 1,
            "disabled editors never participate in Tab traversal"
        );
        assert!(!editor(&mut tree).is_focused());
        let control = &tree.state.downcast_ref::<State<String>>().control;
        assert!(control.node.is_disabled());
        assert!(!control.focusable);
        assert!(!control.node.supports_action(Action::SetTextSelection));
        assert!(control.set_value.is_none() && control.replace_selected_text.is_none());
        assert_eq!(control.node.value(), Some("readable"));
        assert_eq!(control.children[0].1.value(), Some("readable"));
    }

    #[test]
    fn mapped_destination_native_edits_acknowledge_the_normal_message_and_normalized_model() {
        use crate::gui::{Message, Workbench, accessibility, destinations};
        use iced::advanced::renderer::Headless;
        #[derive(Default)]
        struct Native {
            replace: Option<ValueHandler<Message>>,
            input: Option<NativeInput>,
        }
        impl Operation for Native {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
                if let Some(control) = state.downcast_ref::<Control<Message>>() {
                    self.replace = control.replace_selected_text.clone();
                    self.input = control.native_input.clone();
                }
            }
        }
        fn view(value: &str) -> Element<'_, Message> {
            accessibility::mapped(
                text_input("Output", value)
                    .on_input(destinations::Message::Output)
                    .into(),
                Message::CapturePicker,
            )
        }
        let renderer = iced::futures::executor::block_on(iced::Renderer::new(
            iced::Font::default(),
            iced::Pixels(16.0),
            Some("tiny-skia"),
        ))
        .expect("software renderer");
        let mut app = Workbench::new(
            crate::session::SessionHandle::new(),
            "text-edit-test".into(),
            true,
            None,
        );
        app.capture_to = destinations::Picker::new(false, false);
        app.capture_to.selected = Some("named destination".into());
        app.output = "abcd".into();
        let mut widget = view(&app.output);
        let mut tree = Tree::new(widget.as_widget());
        tree.children[0]
            .state
            .downcast_mut::<text_input::State<<iced::Renderer as text::Renderer>::Paragraph>>()
            .select_range(4, 0);
        let limits = layout::Limits::new(Size::ZERO, Size::new(240.0, 30.0));
        let node = widget.as_widget_mut().layout(&mut tree, &renderer, &limits);
        let mut native = Native::default();
        widget
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut native);
        let absolute = std::env::temp_dir()
            .join("🇨🇦.png")
            .to_string_lossy()
            .into_owned();
        let requested = native.replace.as_ref().unwrap()(absolute);
        let acknowledgement = native.input.as_ref().unwrap().acknowledgement().unwrap();
        // An ordinary diff before the callback must not accept a speculative
        // draft; the mapped message uses the application's real update hook.
        widget.as_widget().diff(&mut tree);
        drop(widget);
        let _ = app.update(Message::NativeInput {
            message: Box::new(requested),
            acknowledgement: acknowledgement.clone(),
        });
        assert!(!acknowledgement.is_current());
        assert_eq!(
            app.output, "🇨🇦.png",
            "the destination's normal path removes an absolute prefix"
        );
        let mut widget = view(&app.output);
        widget.as_widget().diff(&mut tree);
        let node = widget.as_widget_mut().layout(&mut tree, &renderer, &limits);
        widget
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut native);
        let actual = tree.children[0]
            .state
            .downcast_ref::<text_input::State<<iced::Renderer as text::Renderer>::Paragraph>>();
        assert_eq!(
            actual.cursor().state(&Value::new(&app.output)),
            cursor::State::Index(5)
        );
        native.input.as_ref().unwrap().release();
        let requested = native.replace.as_ref().unwrap()("x".into());
        let acknowledgement = native.input.as_ref().unwrap().acknowledgement().unwrap();
        drop(widget);
        let _ = app.update(Message::InWindow(
            app.main_window,
            Box::new(Message::NativeInput {
                message: Box::new(requested),
                acknowledgement,
            }),
        ));
        assert_eq!(
            app.output, "🇨🇦.pngx",
            "the next replacement uses the accepted destination draft and collapsed editor cursor"
        );
    }
    #[test]
    fn replacement_uses_the_real_grapheme_selection_in_both_directions() {
        let value = Value::new("a🇨🇦e\u{301}z");
        for (start, end) in [(1, 3), (3, 1)] {
            assert_eq!(
                replacement(&value, cursor::State::Selection { start, end }, "x"),
                "axz"
            );
        }
        assert_eq!(
            replacement(&value, cursor::State::Index(2), "x"),
            "a🇨🇦xe\u{301}z"
        );
    }

    #[test]
    fn native_selection_updates_the_real_editor_and_preserves_the_semantic_id() {
        let mut input = TextInput::<String, ()>::new("", "a🇨🇦e\u{301}z")
            .name("Feature value")
            .invalid(Some("Outside the camera's range".into()))
            .on_input(|value| value);
        let mut tree = Tree::new(&input as &dyn Widget<String, Theme, ()>);
        let node = input.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(240.0, 30.0)),
        );
        struct Select;
        impl Operation for Select {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
                if let Some(control) = state.downcast_mut::<Control<String>>() {
                    let child = control.node.children()[0];
                    control.requested = Some(accesskit::ActionRequest {
                        action: Action::SetTextSelection,
                        target: control.id,
                        data: Some(ActionData::SetTextSelection(TextSelection {
                            anchor: TextPosition {
                                node: child,
                                character_index: 1,
                            },
                            focus: TextPosition {
                                node: child,
                                character_index: 3,
                            },
                        })),
                    });
                }
            }
        }
        input.operate(&mut tree, Layout::new(&node), &(), &mut Select);
        let id = tree.state.downcast_ref::<State<String>>().control.id;
        let editor = tree.children[0]
            .state
            .downcast_ref::<text_input::State<()>>();
        assert!(editor.is_focused());
        assert_eq!(
            editor.cursor().selection(&Value::new("a🇨🇦e\u{301}z")),
            Some((1, 3))
        );
        struct Inspect;
        impl Operation for Inspect {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
        }
        input.operate(&mut tree, Layout::new(&node), &(), &mut Inspect);
        let control = &tree.state.downcast_ref::<State<String>>().control;
        assert_eq!(control.node.label(), Some("Feature value"));
        assert_eq!(control.node.invalid(), Some(Invalid::True));
        assert_eq!(
            control.node.description(),
            Some("Outside the camera's range")
        );
        assert_eq!(control.children[0].1.character_lengths(), &[1, 8, 3, 1]);
        assert_eq!(
            control.replace_selected_text.as_ref().unwrap()("x".into()),
            "axz"
        );
        input = TextInput::new("", "updated")
            .name("Feature value")
            .on_input(|value| value);
        input.diff(&mut tree);
        input.operate(&mut tree, Layout::new(&node), &(), &mut Inspect);
        assert_eq!(tree.state.downcast_ref::<State<String>>().control.id, id);
    }

    #[test]
    fn password_semantics_never_publish_the_underlying_secret() {
        let mut input = TextInput::<String, ()>::new("Secret access key", "private🇨🇦key")
            .secure(true)
            .on_input(|value| value);
        let mut tree = Tree::new(&input as &dyn Widget<String, Theme, ()>);
        let node = input.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(240.0, 30.0)),
        );
        struct Inspect;
        impl Operation for Inspect {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
        }
        input.operate(&mut tree, Layout::new(&node), &(), &mut Inspect);
        let control = &tree.state.downcast_ref::<State<String>>().control;
        assert_eq!(control.node.role(), Role::PasswordInput);
        let masked = "•".repeat(Value::new("private🇨🇦key").len());
        assert_eq!(control.node.value(), Some(masked.as_str()));
        assert_eq!(control.children[0].1.value(), Some(masked.as_str()));
        assert_eq!(
            control.set_value.as_ref().unwrap()("replacement".into()),
            "replacement"
        );
    }
}
