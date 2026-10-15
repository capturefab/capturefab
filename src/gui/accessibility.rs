//! The semantic widget tree and native accessibility actions share the same
//! messages, validation and focus operations as ordinary desktop interaction.
use super::Message;
use accesskit::{Action, ActionData, ActionRequest, Rect, Tree as AxTree, TreeUpdate};
pub(super) use accesskit::{Node, NodeId, Role, Toggled};
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Id, Operation, Tree, tree},
};
use iced::futures::{SinkExt, StreamExt, channel::mpsc};
use iced::{
    Element, Event, Length, Rectangle, Size, Subscription, Task, Theme, Vector, mouse, window,
};
use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

mod decorate;
mod group;
mod label;
mod native;
mod scroll;
pub(super) use decorate::{mapped, named};
pub(super) use group::{group, group_with_id, modal_group, scope};
pub(super) use label::{label, label_description};

const ROOT: NodeId = NodeId(0);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub(super) fn new_node_id() -> NodeId {
    NodeId(NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

pub(super) type ValueHandler<M> = Arc<dyn Fn(String) -> M + Send + Sync>;

/// Store this marker in widget state, so its ID survives ordinary view diffs.
/// Populate it during `operate`, then pass it to `Operation::custom` with the
/// real control bounds. Use `focus_id` as the fallback widget focus ID.
pub(super) struct Control<M> {
    pub id: NodeId,
    pub focus_id: Id,
    pub node: Node,
    pub focused: bool,
    pub focusable: bool,
    /// Popup rows are laid out in window coordinates and have their own
    /// viewport, outside the scrollable containing the closed control.
    pub window_coordinates: Option<Rectangle>,
    pub children: Vec<(NodeId, Node)>,
    pub activate: Option<M>,
    /// Controls whose logical focus has its own model (such as desktop menus)
    /// route native focus through that model while retaining editor selection.
    pub on_focus: Option<M>,
    pub increment: Option<M>,
    pub decrement: Option<M>,
    pub set_value: Option<ValueHandler<M>>,
    pub replace_selected_text: Option<ValueHandler<M>>,
    pub native_input: Option<super::text_input::NativeInput>,
    /// A native request that changes widget-local state (for example, opening
    /// a combo box). Consume it immediately after `Operation::custom`.
    pub requested: Option<ActionRequest>,
}

impl<M> Control<M> {
    pub fn new(role: Role) -> Self {
        Self {
            id: new_node_id(),
            focus_id: Id::unique(),
            node: Node::new(role),
            focused: false,
            focusable: true,
            window_coordinates: None,
            children: Vec::new(),
            activate: None,
            on_focus: None,
            increment: None,
            decrement: None,
            set_value: None,
            replace_selected_text: None,
            native_input: None,
            requested: None,
        }
    }
}

#[derive(Clone)]
struct Route {
    activate: Option<Message>,
    on_focus: Option<Message>,
    increment: Option<Message>,
    decrement: Option<Message>,
    set_value: Option<ValueHandler<Message>>,
    replace_selected_text: Option<ValueHandler<Message>>,
    native_input: Option<super::text_input::NativeInput>,
    widget_actions: Vec<Action>,
    widget_focus: bool,
    focusable: bool,
    reveal: bool,
}

#[derive(Clone, Debug)]
pub(super) struct Request {
    pub window: window::Id,
    pub action: ActionRequest,
    pub replay: Option<u64>,
}

struct Snapshot {
    tree: TreeUpdate,
    routes: HashMap<NodeId, Route>,
}

struct Shared {
    id: u64,
    snapshots: Mutex<HashMap<window::Id, Snapshot>>,
    sender: Mutex<mpsc::Sender<Request>>,
    receiver: Mutex<Option<mpsc::Receiver<Request>>>,
}

#[derive(Clone)]
pub(super) struct Bridge(Arc<Shared>);

impl Default for Bridge {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel(64);
        Self(Arc::new(Shared {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            snapshots: Mutex::new(HashMap::new()),
            sender: Mutex::new(sender),
            receiver: Mutex::new(Some(receiver)),
        }))
    }
}

impl Hash for Bridge {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.id.hash(state);
    }
}

impl Bridge {
    /// Must run while the native window is still hidden. The adapters live in
    /// UI-thread local storage; no NSView/HWND is moved to a worker thread.
    pub fn attach(&self, id: window::Id) -> Task<bool> {
        let bridge = self.clone();
        window::run(id, move |handle| native::attach(id, handle, bridge))
    }

    pub fn sync(&self, id: window::Id) -> Task<bool> {
        let bridge = self.clone();
        window::run(id, move |handle| native::sync(id, handle, &bridge))
    }

    pub fn focused(&self, id: window::Id, focused: bool) -> Task<bool> {
        window::run(id, move |_| native::focused(id, focused))
    }

    /// Chain this task before closing the native window. This restores the
    /// original native view/window class on the same UI thread that installed it.
    pub fn detach(&self, id: window::Id) -> Task<bool> {
        self.invalidate(id);
        self.0.snapshots.lock().unwrap().remove(&id);
        window::run(id, move |_| native::detach(id))
    }

    pub fn subscription(&self) -> Subscription<Request> {
        fn stream(bridge: &Bridge) -> impl iced::futures::Stream<Item = Request> + use<> {
            let receiver = bridge.0.receiver.lock().unwrap().take();
            iced::stream::channel(64, move |mut output: mpsc::Sender<Request>| async move {
                if let Some(mut receiver) = receiver {
                    while let Some(request) = receiver.next().await {
                        if output.send(request).await.is_err() {
                            break;
                        }
                    }
                }
            })
        }
        Subscription::run_with(self.clone(), stream)
    }

    pub fn dispatch(&self, request: Request) -> Task<Message> {
        let snapshots = self.0.snapshots.lock().unwrap();
        let Some(route) = snapshots
            .get(&request.window)
            .and_then(|snapshot| snapshot.routes.get(&request.action.target))
        else {
            // A node removed by a modal or a camera refresh cannot be activated.
            return Task::none();
        };
        let editable = match (&request.action.action, &request.action.data) {
            (Action::SetValue | Action::ReplaceSelectedText, Some(ActionData::Value(_))) => true,
            (Action::SetValue, Some(ActionData::NumericValue(value))) => value.is_finite(),
            (Action::SetTextSelection, Some(ActionData::SetTextSelection(_))) => true,
            _ => false,
        };
        if editable
            && let Some(input) = &route.native_input
            && input.defer(request.action.clone(), request.replay)
        {
            return Task::none();
        }
        let input_message = |message: Message| {
            if let Some(acknowledgement) = route
                .native_input
                .as_ref()
                .and_then(super::text_input::NativeInput::acknowledgement)
            {
                Message::NativeInput {
                    message: Box::new(message),
                    acknowledgement,
                }
            } else {
                message
            }
        };
        match request.action.action {
            Action::Focus => {
                if !route.focusable && !route.widget_focus && route.on_focus.is_none() {
                    return Task::none();
                }
                let focus = if route.focusable || route.widget_focus {
                    // Explicit widget IDs can be reused in separate native
                    // windows. Locate scopes both focus and scroll reveal to
                    // the requested window, including controls without scroll.
                    iced::advanced::widget::operate(scroll::Locate::node(
                        request.action.target,
                        request.window,
                        true,
                    ))
                } else {
                    Task::none()
                };
                let focus = if route.widget_focus {
                    focus.chain(iced::advanced::widget::operate(WidgetAction(
                        request.action,
                        request.window,
                    )))
                } else {
                    focus
                };
                let focus = if let Some(message) = &route.on_focus {
                    focus.chain(Task::done(message.clone()))
                } else {
                    focus
                };
                // AccessKit forwards native focus requests; it does not make
                // the owning NSWindow/HWND the active keyboard window.
                window::minimize(request.window, false)
                    .chain(window::gain_focus(request.window))
                    .chain(focus)
            }
            Action::Click if route.activate.is_some() => {
                Task::done(route.activate.clone().unwrap())
            }
            Action::Increment => route.increment.clone().map_or_else(Task::none, Task::done),
            Action::Decrement => route.decrement.clone().map_or_else(Task::none, Task::done),
            Action::SetValue => match (&route.set_value, request.action.data) {
                (Some(handler), Some(ActionData::Value(value))) => {
                    Task::done(input_message(handler(value.into())))
                }
                (Some(handler), Some(ActionData::NumericValue(value))) if value.is_finite() => {
                    Task::done(input_message(handler(value.to_string())))
                }
                _ => Task::none(),
            },
            Action::ReplaceSelectedText => {
                match (&route.replace_selected_text, request.action.data) {
                    (Some(handler), Some(ActionData::Value(value))) => {
                        Task::done(input_message(handler(value.into())))
                    }
                    _ => Task::none(),
                }
            }
            Action::SetTextSelection
                if route.widget_actions.contains(&Action::SetTextSelection) =>
            {
                if let Some(input) = &route.native_input {
                    input.begin_selection();
                }
                iced::advanced::widget::operate(WidgetAction(request.action, request.window))
            }
            Action::ScrollIntoView if route.reveal => iced::advanced::widget::operate(
                scroll::Locate::node(request.action.target, request.window, false),
            ),
            action if route.widget_actions.contains(&action) => {
                iced::advanced::widget::operate(WidgetAction(request.action, request.window))
            }
            _ => Task::none(),
        }
    }

    /// Refuse requests from the previous active view until a changed modal is
    /// collected on its first redraw.
    pub fn invalidate(&self, id: window::Id) {
        if let Some(snapshot) = self.0.snapshots.lock().unwrap().get_mut(&id) {
            for input in snapshot
                .routes
                .values()
                .filter_map(|route| route.native_input.as_ref())
            {
                input.cancel();
            }
            snapshot.routes.clear();
        }
    }

    fn tree(&self, id: window::Id) -> Option<TreeUpdate> {
        self.0
            .snapshots
            .lock()
            .unwrap()
            .get(&id)
            .map(|snapshot| snapshot.tree.clone())
    }

    fn send(&self, window: window::Id, action: ActionRequest) -> bool {
        self.send_request(Request {
            window,
            action,
            replay: None,
        })
    }

    fn send_request(&self, request: Request) -> bool {
        // Never block an OS accessibility callback on camera work or the UI.
        self.0.sender.lock().unwrap().try_send(request).is_ok()
    }

    fn store(&self, id: window::Id, tree: TreeUpdate, routes: HashMap<NodeId, Route>) -> bool {
        let deferred: Vec<_> = routes
            .values()
            .filter_map(|route| {
                route
                    .native_input
                    .as_ref()
                    .and_then(|input| input.release().map(|request| (input.clone(), request)))
            })
            .collect();
        let mut snapshots = self.0.snapshots.lock().unwrap();
        if let Some(previous) = snapshots.get(&id) {
            for (node, route) in &previous.routes {
                if routes
                    .get(node)
                    .is_none_or(|current| current.native_input.is_none())
                    && let Some(input) = &route.native_input
                {
                    input.cancel();
                }
            }
        }
        let changed = snapshots
            .get(&id)
            .is_none_or(|snapshot| snapshot.tree != tree);
        snapshots.insert(id, Snapshot { tree, routes });
        drop(snapshots);
        for (input, (action, token)) in deferred {
            if !self.send_request(Request {
                window: id,
                action: action.clone(),
                replay: Some(token),
            }) {
                input.retry(action);
            }
        }
        changed
    }
}

struct Collector {
    nodes: Vec<(NodeId, Node)>,
    routes: HashMap<NodeId, Route>,
    focus: NodeId,
    scale: f64,
    text_counts: HashMap<(Option<NodeId>, String), u64>,
    groups: Vec<(NodeId, usize)>,
    viewport: scroll::Viewport,
    content_bounds: HashMap<NodeId, Rect>,
}

impl Collector {
    fn new(scale: f32) -> Self {
        Self {
            nodes: Vec::new(),
            routes: HashMap::new(),
            focus: ROOT,
            scale: scale.max(0.1) as f64,
            text_counts: HashMap::new(),
            groups: Vec::new(),
            viewport: scroll::Viewport::default(),
            content_bounds: HashMap::new(),
        }
    }

    fn finish(self, title: &str, bounds: Rectangle) -> (TreeUpdate, HashMap<NodeId, Route>) {
        let mut root = Node::new(Role::Window);
        root.set_label(title);
        root.set_bounds(rect(bounds, self.scale));
        let descendants: std::collections::HashSet<_> = self
            .nodes
            .iter()
            .flat_map(|(_, node)| node.children().iter().copied())
            .collect();
        root.set_children(
            self.nodes
                .iter()
                .map(|(id, _)| *id)
                .filter(|id| !descendants.contains(id))
                .collect::<Vec<_>>(),
        );
        let mut nodes = self.nodes;
        nodes.push((ROOT, root));
        (
            TreeUpdate {
                nodes,
                tree: Some(AxTree::new(ROOT)),
                focus: self.focus,
            },
            self.routes,
        )
    }
}

fn rect(bounds: Rectangle, scale: f64) -> Rect {
    Rect {
        x0: bounds.x as f64 * scale,
        y0: bounds.y as f64 * scale,
        x1: (bounds.x + bounds.width) as f64 * scale,
        y1: (bounds.y + bounds.height) as f64 * scale,
    }
}

impl Operation for Collector {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        let saved = self.viewport.enter();
        operate(self);
        if let Some(saved) = saved {
            self.viewport.exit(saved);
        }
    }

    fn scrollable(
        &mut self,
        _: Option<&Id>,
        bounds: Rectangle,
        content: Rectangle,
        translation: Vector,
        state: &mut dyn iced::advanced::widget::operation::Scrollable,
    ) {
        self.viewport
            .scrollable(bounds, content, translation, state);
    }

    fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(marker) = state.downcast_ref::<group::Marker>() {
            if marker.begin {
                self.groups.push((marker.id, self.nodes.len()));
            } else if let Some((id, start)) = self.groups.pop()
                && id == marker.id
            {
                let descendants: std::collections::HashSet<_> = self.nodes[start..]
                    .iter()
                    .flat_map(|(_, node)| node.children().iter().copied())
                    .collect();
                let children: Vec<_> = self.nodes[start..]
                    .iter()
                    .map(|(id, _)| *id)
                    .filter(|id| !descendants.contains(id))
                    .collect();
                if let Some((_, node)) = self.nodes.iter_mut().find(|(node_id, _)| *node_id == id) {
                    node.set_children(children);
                }
            }
            return;
        }
        if let Some(control) = state.downcast_ref::<Control<Message>>() {
            let mut node = control.node.clone();
            let (visible, full) = self.viewport.bounds(bounds, control.window_coordinates);
            node.set_bounds(rect(visible, self.scale));
            self.content_bounds
                .insert(control.id, rect(full, self.scale));
            let reveal = self.viewport.scrolled() && control.window_coordinates.is_none();
            if !node.is_disabled() {
                if control.focusable {
                    node.add_action(Action::Focus);
                }
                if control.activate.is_some() {
                    node.add_action(Action::Click);
                }
                if control.increment.is_some() {
                    node.add_action(Action::Increment);
                }
                if control.decrement.is_some() {
                    node.add_action(Action::Decrement);
                }
                if control.set_value.is_some() {
                    node.add_action(Action::SetValue);
                }
                if control.replace_selected_text.is_some() {
                    node.add_action(Action::ReplaceSelectedText);
                }
                if reveal {
                    node.add_action(Action::ScrollIntoView);
                }
                self.routes.insert(
                    control.id,
                    Route {
                        activate: control.activate.clone(),
                        on_focus: control.on_focus.clone(),
                        increment: control.increment.clone(),
                        decrement: control.decrement.clone(),
                        set_value: control.set_value.clone(),
                        replace_selected_text: control.replace_selected_text.clone(),
                        native_input: control.native_input.clone(),
                        widget_actions: [
                            Action::Click,
                            Action::Expand,
                            Action::Collapse,
                            Action::SetTextSelection,
                            Action::ScrollIntoView,
                        ]
                        .into_iter()
                        .filter(|action| node.supports_action(*action))
                        .collect(),
                        widget_focus: node.role() == Role::ListBoxOption,
                        focusable: control.focusable,
                        reveal,
                    },
                );
            }
            for (id, child) in &control.children {
                let mut child = child.clone();
                if child.bounds().is_none() {
                    child.set_bounds(rect(visible, self.scale));
                }
                self.nodes.push((*id, child));
            }
            if control.focused && !node.is_disabled() {
                self.focus = control.id;
            }
            self.nodes.push((control.id, node));
        }
    }

    fn text(&mut self, _: Option<&Id>, bounds: Rectangle, text: &str) {
        let text = text.trim();
        if text.is_empty()
            || text
                .chars()
                .all(|ch| ('\u{e000}'..='\u{f8ff}').contains(&ch))
        {
            return;
        }
        let (visible, full) = self.viewport.bounds(bounds, None);
        let bounds = rect(visible, self.scale);
        let full = rect(full, self.scale);
        // Text within a control supplies its fallback name. It is not exposed
        // a second time as unrelated static text.
        if let Some((_, node)) = self.nodes.iter_mut().rev().find(|(id, node)| {
            matches!(
                node.role(),
                Role::Button
                    | Role::DisclosureTriangle
                    | Role::DefaultButton
                    | Role::CheckBox
                    | Role::RadioButton
                    | Role::Switch
                    | Role::Slider
                    | Role::MenuItem
                    | Role::MenuItemCheckBox
                    | Role::MenuItemRadio
                    | Role::Heading
                    | Role::Label
                    | Role::Alert
                    | Role::Status
                    | Role::Image
            ) && self.content_bounds.get(id).is_some_and(|parent| {
                parent.x0 <= full.x0
                    && parent.y0 <= full.y0
                    && parent.x1 >= full.x1
                    && parent.y1 >= full.y1
            })
        }) {
            if node
                .label()
                .is_none_or(|label| matches!(label, "Button" | ""))
            {
                node.set_label(text);
            }
            return;
        }
        // A journal row's identical path or timestamp belongs to that row.
        // Counts shared across the whole window would move its native text ID
        // when another row containing the same caption is inserted first.
        let parent = self.groups.last().map(|(id, _)| *id);
        let count = self
            .text_counts
            .entry((parent, text.to_owned()))
            .or_default();
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        if let Some(parent) = parent {
            parent.hash(&mut hash);
        }
        text.hash(&mut hash);
        count.hash(&mut hash);
        *count += 1;
        let id = NodeId(hash.finish() | (1 << 63));
        let mut node = Node::new(Role::Label);
        node.set_label(text);
        node.set_bounds(bounds);
        self.nodes.push((id, node));
    }
}

struct WidgetAction(ActionRequest, window::Id);

impl Operation<Message> for WidgetAction {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Message>)) {
        operate(self);
    }

    fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(control) = state.downcast_mut::<Control<Message>>()
            && control.id == self.0.target
        {
            control.requested = Some(self.0.clone());
        }
    }

    fn finish(&self) -> iced::advanced::widget::operation::Outcome<Message> {
        // Widget-local mutations need another application update/redraw even
        // when no camera is producing frames. The next collection publishes
        // the changed popup or text-selection state.
        iced::advanced::widget::operation::Outcome::Some(Message::AccessibilityChanged(self.1))
    }
}

/// Chain after keyboard traversal. This reads the newly focused real widget,
/// scrolls its ancestors, and preserves its caret and selection.
pub(super) fn reveal_focus(id: window::Id) -> Task<Message> {
    iced::advanced::widget::operate(focus_reveal_operation(id))
}

pub(super) fn focus_in_window(id: impl Into<Id>, window: window::Id) -> Task<Message> {
    iced::advanced::widget::operate(scroll::Locate::widget(id.into(), window))
}

pub(super) fn focus_reveal_operation(id: window::Id) -> impl Operation<Message> {
    scroll::Locate::focused(id)
}

pub(super) fn wrap<'a>(
    content: Element<'a, Message>,
    bridge: Bridge,
    id: window::Id,
    title: String,
    scale: f32,
    on_changed: fn(window::Id) -> Message,
) -> Element<'a, Message> {
    Element::new(Semantics {
        content,
        bridge,
        id,
        title,
        scale,
        on_changed,
    })
}

struct Semantics<'a> {
    content: Element<'a, Message>,
    bridge: Bridge,
    id: window::Id,
    title: String,
    scale: f32,
    on_changed: fn(window::Id) -> Message,
}

impl Widget<Message, Theme, iced::Renderer> for Semantics<'_> {
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }
    fn tag(&self) -> tree::Tag {
        self.content.as_widget().tag()
    }
    fn state(&self) -> tree::State {
        self.content.as_widget().state()
    }
    fn children(&self) -> Vec<Tree> {
        self.content.as_widget().children()
    }
    fn diff(&self, tree: &mut Tree) {
        self.content.as_widget().diff(tree);
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content.as_widget_mut().layout(tree, renderer, limits)
    }
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content
            .as_widget()
            .draw(tree, renderer, theme, style, layout, cursor, viewport);
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.custom(
            None,
            layout.bounds(),
            &mut scroll::WindowMarker {
                id: self.id,
                begin: true,
            },
        );
        self.content
            .as_widget_mut()
            .operate(tree, layout, renderer, operation);
        operation.custom(
            None,
            layout.bounds(),
            &mut scroll::WindowMarker {
                id: self.id,
                begin: false,
            },
        );
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            tree, event, layout, cursor, renderer, clipboard, shell, viewport,
        );
        if matches!(event, Event::Window(window::Event::RedrawRequested(_))) {
            let mut collector = Collector::new(self.scale);
            collector.viewport.root(layout.bounds());
            self.content
                .as_widget_mut()
                .operate(tree, layout, renderer, &mut collector);
            let (tree, routes) = collector.finish(&self.title, layout.bounds());
            if self.bridge.store(self.id, tree, routes) {
                shell.publish((self.on_changed)(self.id));
            }
        } else if matches!(event, Event::Window(window::Event::Moved(_))) {
            // The semantic client-relative tree may be identical after a move,
            // but the AT-SPI screen origin must follow the native window.
            shell.publish((self.on_changed)(self.id));
        }
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_widget()
            .mouse_interaction(tree, layout, cursor, viewport, renderer)
    }
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, iced::Renderer>> {
        self.content
            .as_widget_mut()
            .overlay(tree, layout, renderer, viewport, translation)
    }
}

#[cfg(test)]
pub(super) fn snapshot_widget<R: renderer::Renderer>(
    widget: &mut dyn Widget<Message, Theme, R>,
    tree: &mut Tree,
    layout: Layout<'_>,
    renderer: &R,
) -> (TreeUpdate, Bridge, window::Id) {
    let mut collector = Collector::new(1.0);
    widget.operate(tree, layout, renderer, &mut collector);
    let (tree, routes) = collector.finish("Capturefab", layout.bounds());
    let bridge = Bridge::default();
    let window = window::Id::unique();
    bridge.store(window, tree.clone(), routes);
    (tree, bridge, window)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_editor(
        input: &mut super::super::text_input::TextInput<'_, Message, ()>,
        widget_tree: &mut Tree,
        bridge: &Bridge,
        window: window::Id,
    ) -> NodeId {
        let layout = input.layout(
            widget_tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(240.0, 30.0)),
        );
        let mut collector = Collector::new(1.0);
        input.operate(widget_tree, Layout::new(&layout), &(), &mut collector);
        let (tree, routes) = collector.finish("Editor", layout.bounds());
        let id = *routes.keys().next().unwrap_or(&ROOT);
        bridge.store(window, tree, routes);
        id
    }

    #[test]
    fn native_editor_bridge_serializes_back_to_back_edits_until_real_draft_collection() {
        use super::super::text_input::TextInput;
        let bridge = Bridge::default();
        let window = window::Id::unique();
        let mut input = TextInput::<Message, ()>::new("Value", "abcd").on_input(Message::Address);
        let mut tree = Tree::new(&input as &dyn Widget<Message, Theme, ()>);
        let id = collect_editor(&mut input, &mut tree, &bridge, window);
        let native = bridge.0.snapshots.lock().unwrap()[&window].routes[&id]
            .native_input
            .clone()
            .unwrap();
        let request = |action, value: &str| Request {
            window,
            replay: None,
            action: ActionRequest {
                action,
                target: id,
                data: Some(ActionData::Value(value.into())),
            },
        };
        assert!(bridge.dispatch(request(Action::SetValue, "axd")).units() > 0);
        let acknowledgement = native.acknowledgement().unwrap();
        // Both requests arrive before the app processes the first callback.
        assert_eq!(
            bridge
                .dispatch(request(Action::ReplaceSelectedText, "Y"))
                .units(),
            0
        );
        assert_eq!(
            bridge
                .dispatch(request(Action::ReplaceSelectedText, "Y"))
                .units(),
            0
        );
        input.diff(&mut tree);
        collect_editor(&mut input, &mut tree, &bridge, window);
        assert!(
            bridge
                .0
                .receiver
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .try_recv()
                .is_err()
        );
        acknowledgement.acknowledge();
        input = TextInput::new("Value", "axd").on_input(Message::Address);
        input.diff(&mut tree);
        collect_editor(&mut input, &mut tree, &bridge, window);
        let first = bridge
            .0
            .receiver
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .try_recv()
            .unwrap();
        assert!(first.replay.is_some());
        assert!(bridge.dispatch(first).units() > 0);
        // Recollection before the replay's callback cannot release the final
        // identical edit or use an unaccepted speculative value.
        collect_editor(&mut input, &mut tree, &bridge, window);
        assert!(
            bridge
                .0
                .receiver
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .try_recv()
                .is_err()
        );
        native.acknowledgement().unwrap().acknowledge();
        input = TextInput::new("Value", "axdY").on_input(Message::Address);
        input.diff(&mut tree);
        collect_editor(&mut input, &mut tree, &bridge, window);
        let second = bridge
            .0
            .receiver
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .try_recv()
            .unwrap();
        assert!(bridge.dispatch(second.clone()).units() > 0);
        bridge.invalidate(window);
        assert!(!native.acknowledgement().is_some_and(|ack| ack.is_current()));
        collect_editor(&mut input, &mut tree, &bridge, window);
        assert_eq!(
            bridge.dispatch(second).units(),
            0,
            "a stale replay cannot activate the returned background editor"
        );
    }

    #[test]
    fn native_selection_task_finishes_before_a_back_to_back_replacement_uses_the_editor() {
        use super::super::text_input::TextInput;
        let bridge = Bridge::default();
        let window = window::Id::unique();
        let drafts = Arc::new(Mutex::new(Vec::new()));
        let received = drafts.clone();
        let mut input = TextInput::<Message, ()>::new("Value", "abcd").on_input(move |value| {
            received.lock().unwrap().push(value.clone());
            Message::Address(value)
        });
        let mut tree = Tree::new(&input as &dyn Widget<Message, Theme, ()>);
        let id = collect_editor(&mut input, &mut tree, &bridge, window);
        let text_id = bridge.0.snapshots.lock().unwrap()[&window]
            .tree
            .nodes
            .iter()
            .find(|(node_id, _)| *node_id == id)
            .unwrap()
            .1
            .children()[0];
        let selection = ActionRequest {
            action: Action::SetTextSelection,
            target: id,
            data: Some(ActionData::SetTextSelection(accesskit::TextSelection {
                anchor: accesskit::TextPosition {
                    node: text_id,
                    character_index: 3,
                },
                focus: accesskit::TextPosition {
                    node: text_id,
                    character_index: 1,
                },
            })),
        };
        assert!(
            bridge
                .dispatch(Request {
                    window,
                    action: selection.clone(),
                    replay: None
                })
                .units()
                > 0
        );
        let replacement = ActionRequest {
            action: Action::ReplaceSelectedText,
            target: id,
            data: Some(ActionData::Value("x".into())),
        };
        assert_eq!(
            bridge
                .dispatch(Request {
                    window,
                    action: replacement,
                    replay: None
                })
                .units(),
            0
        );
        collect_editor(&mut input, &mut tree, &bridge, window);
        assert!(
            drafts.lock().unwrap().is_empty(),
            "replacement waits for the queued widget Task"
        );
        assert!(
            bridge
                .0
                .receiver
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .try_recv()
                .is_err()
        );
        // Execute the real widget operation after both model messages, as
        // iced's runtime does, then collect the actual editor selection.
        let node = input.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(240.0, 30.0)),
        );
        let mut operation = WidgetAction(selection, window);
        let mut erased = iced::advanced::widget::operation::black_box(&mut operation);
        input.operate(&mut tree, Layout::new(&node), &(), &mut erased);
        collect_editor(&mut input, &mut tree, &bridge, window);
        let released = bridge
            .0
            .receiver
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .try_recv()
            .unwrap();
        assert!(bridge.dispatch(released).units() > 0);
        assert_eq!(*drafts.lock().unwrap(), ["axd"]);
    }

    #[test]
    fn disabling_or_removing_an_editor_cancels_pending_native_callbacks_and_queued_edits() {
        use super::super::text_input::TextInput;
        for removed in [false, true] {
            let bridge = Bridge::default();
            let window = window::Id::unique();
            let mut input =
                TextInput::<Message, ()>::new("Value", "base").on_input(Message::Address);
            let mut tree = Tree::new(&input as &dyn Widget<Message, Theme, ()>);
            let id = collect_editor(&mut input, &mut tree, &bridge, window);
            let native = bridge.0.snapshots.lock().unwrap()[&window].routes[&id]
                .native_input
                .clone()
                .unwrap();
            let request = || Request {
                window,
                replay: None,
                action: ActionRequest {
                    action: Action::SetValue,
                    target: id,
                    data: Some(ActionData::Value("draft".into())),
                },
            };
            assert!(bridge.dispatch(request()).units() > 0);
            let acknowledgement = native.acknowledgement().unwrap();
            assert_eq!(bridge.dispatch(request()).units(), 0);
            if removed {
                let (tree, routes) = Collector::new(1.0).finish("Empty", Rectangle::default());
                bridge.store(window, tree, routes);
            } else {
                input = TextInput::new("Value", "base");
                input.diff(&mut tree);
                collect_editor(&mut input, &mut tree, &bridge, window);
            }
            assert!(!acknowledgement.is_current());
            input = TextInput::new("Value", "base").on_input(Message::Address);
            input.diff(&mut tree);
            collect_editor(&mut input, &mut tree, &bridge, window);
            assert!(
                bridge
                    .0
                    .receiver
                    .lock()
                    .unwrap()
                    .as_mut()
                    .unwrap()
                    .try_recv()
                    .is_err()
            );
            assert!(native.release().is_none());
        }
    }

    #[test]
    fn collected_controls_have_physical_bounds_names_and_a_single_parent() {
        let mut collector = Collector::new(2.0);
        let mut button = Control::new(Role::Button);
        button.node.set_label("Button");
        button.activate = Some(Message::Discover);
        button.focused = true;
        let bounds = Rectangle {
            x: 10.0,
            y: 20.0,
            width: 100.0,
            height: 30.0,
        };
        collector.custom(None, bounds, &mut button);
        collector.text(None, bounds, "Discover cameras");
        collector.text(None, bounds, "\u{e001}");
        let mut input = Control::<Message>::new(Role::TextInput);
        input.node.set_label("Camera address");
        let child = Control::<()>::new(Role::TextRun).id;
        let mut text = Node::new(Role::TextRun);
        text.set_value("sim:0");
        input.node.set_children([child]);
        input.children = vec![(child, text)];
        collector.custom(
            None,
            Rectangle {
                x: 10.0,
                y: 60.0,
                width: 100.0,
                height: 30.0,
            },
            &mut input,
        );
        let mut disabled = Control::<Message>::new(Role::Button);
        disabled.node.set_disabled();
        disabled.node.set_label("Unavailable capture");
        disabled.activate = Some(Message::Capture);
        collector.custom(
            None,
            Rectangle {
                x: 10.0,
                y: 100.0,
                width: 100.0,
                height: 30.0,
            },
            &mut disabled,
        );
        collector.text(
            None,
            Rectangle {
                x: 10.0,
                y: 150.0,
                width: 150.0,
                height: 20.0,
            },
            "Ready",
        );
        let (tree, routes) =
            collector.finish("Capturefab", Rectangle::with_size(Size::new(400.0, 300.0)));
        let nodes: HashMap<_, _> = tree.nodes.into_iter().collect();
        assert_eq!(tree.focus, button.id);
        assert_eq!(nodes[&button.id].label(), Some("Discover cameras"));
        assert_eq!(
            nodes[&button.id].bounds(),
            Some(Rect {
                x0: 20.0,
                y0: 40.0,
                x1: 220.0,
                y1: 100.0
            })
        );
        assert_eq!(nodes[&input.id].children(), &[child]);
        assert!(!nodes[&ROOT].children().contains(&child));
        assert!(!routes.contains_key(&disabled.id));
        assert_eq!(
            nodes
                .values()
                .filter(|node| node.role() == Role::Label)
                .count(),
            1
        );
        assert!(matches!(
            routes[&button.id].activate,
            Some(Message::Discover)
        ));
    }

    #[test]
    fn removed_and_disabled_nodes_cannot_dispatch_stale_native_actions() {
        let bridge = Bridge::default();
        let window = window::Id::unique();
        let mut control = Control::<Message>::new(Role::Button);
        control.activate = Some(Message::Discover);
        let mut collector = Collector::new(1.0);
        collector.custom(None, Rectangle::default(), &mut control);
        let (tree, routes) = collector.finish("Capturefab", Rectangle::default());
        assert!(bridge.store(window, tree, routes));
        let request = Request {
            window,
            replay: None,
            action: ActionRequest {
                action: Action::Click,
                target: control.id,
                data: None,
            },
        };
        assert!(bridge.dispatch(request.clone()).units() > 0);
        let (tree, routes) = Collector::new(1.0).finish("Capturefab", Rectangle::default());
        assert!(bridge.store(window, tree, routes));
        assert_eq!(bridge.dispatch(request).units(), 0);
        control.node.set_disabled();
        let mut collector = Collector::new(1.0);
        collector.custom(None, Rectangle::default(), &mut control);
        let (tree, routes) = collector.finish("Capturefab", Rectangle::default());
        bridge.store(window, tree, routes);
        assert_eq!(
            bridge
                .dispatch(Request {
                    window,
                    replay: None,
                    action: ActionRequest {
                        action: Action::Click,
                        target: control.id,
                        data: None
                    }
                })
                .units(),
            0
        );
    }
}
