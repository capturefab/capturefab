//! Scroll operations expose unshifted child layouts. Keep visual coordinates
//! scoped to their traversal, and reveal native focus using fresh widget state.
use super::*;
use iced::advanced::widget::operation::{self, Outcome, Scrollable};

#[derive(Clone, Copy)]
struct Frame {
    key: usize,
    bounds: Rectangle,
    content: Rectangle,
    translation: Vector,
}

fn key(state: &mut dyn Scrollable) -> usize {
    // Identity is used only across two chained operations on the same live
    // widget tree. The pointer is compared, never stored in native IDs or read.
    state as *mut dyn Scrollable as *mut () as usize
}

#[derive(Default)]
pub(super) struct Viewport {
    offset: Vector,
    clip: Option<Rectangle>,
    root: Option<Rectangle>,
    pending: Option<Frame>,
    frames: Vec<Frame>,
}

pub(super) struct Saved {
    offset: Vector,
    clip: Option<Rectangle>,
}

fn clipped(bounds: Rectangle, clip: Rectangle) -> Rectangle {
    let x = bounds.x.max(clip.x).min(clip.x + clip.width);
    let y = bounds.y.max(clip.y).min(clip.y + clip.height);
    Rectangle {
        x,
        y,
        width: ((bounds.x + bounds.width)
            .min(clip.x + clip.width)
            .max(clip.x)
            - x)
            .max(0.0),
        height: ((bounds.y + bounds.height)
            .min(clip.y + clip.height)
            .max(clip.y)
            - y)
            .max(0.0),
    }
}

impl Viewport {
    pub fn root(&mut self, bounds: Rectangle) {
        self.root = Some(bounds);
    }
    pub fn scrollable(
        &mut self,
        bounds: Rectangle,
        content: Rectangle,
        translation: Vector,
        state: &mut dyn Scrollable,
    ) {
        self.pending = Some(Frame {
            key: key(state),
            bounds,
            content,
            translation,
        });
    }
    pub fn enter(&mut self) -> Option<Saved> {
        let frame = self.pending.take()?;
        let saved = Saved {
            offset: self.offset,
            clip: self.clip,
        };
        let bounds = frame.bounds + self.offset;
        self.clip = Some(self.clip.map_or(bounds, |clip| clipped(bounds, clip)));
        self.offset -= frame.translation;
        self.frames.push(frame);
        Some(saved)
    }
    pub fn exit(&mut self, saved: Saved) {
        self.offset = saved.offset;
        self.clip = saved.clip;
        self.frames.pop();
    }
    pub fn scrolled(&self) -> bool {
        !self.frames.is_empty()
    }
    pub fn bounds(&self, raw: Rectangle, window: Option<Rectangle>) -> (Rectangle, Rectangle) {
        let (full, clip) = match window {
            Some(viewport) => (raw, Some(viewport)),
            None => (raw + self.offset, self.clip),
        };
        let visible = clip.map_or(full, |clip| clipped(full, clip));
        (
            self.root.map_or(visible, |root| clipped(visible, root)),
            full,
        )
    }
}

pub(super) struct WindowMarker {
    pub id: window::Id,
    pub begin: bool,
}

enum Target {
    Node(NodeId),
    Widget(Id),
    Focused,
}

pub(super) struct Locate {
    target: Target,
    focus: bool,
    window: window::Id,
    marked: bool,
    active: bool,
    viewport: Viewport,
    found: Option<(Id, Rectangle, Vec<Frame>)>,
}

impl Locate {
    pub fn node(id: NodeId, window: window::Id, focus: bool) -> Self {
        Self::new(Target::Node(id), window, focus)
    }
    pub fn focused(window: window::Id) -> Self {
        Self::new(Target::Focused, window, false)
    }
    pub fn widget(id: Id, window: window::Id) -> Self {
        Self::new(Target::Widget(id), window, true)
    }
    fn new(target: Target, window: window::Id, focus: bool) -> Self {
        Self {
            target,
            focus,
            window,
            marked: false,
            active: true,
            viewport: Viewport::default(),
            found: None,
        }
    }
}

impl Operation<Message> for Locate {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Message>)) {
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
        state: &mut dyn Scrollable,
    ) {
        self.viewport
            .scrollable(bounds, content, translation, state);
    }
    fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(marker) = state.downcast_ref::<WindowMarker>() {
            self.marked = true;
            self.active = marker.begin && marker.id == self.window;
        } else if self.active
            && self.found.is_none()
            && let Some(control) = state.downcast_ref::<Control<Message>>()
            && !control.node.is_disabled()
            && match &self.target {
                Target::Node(id) => control.id == *id,
                Target::Widget(id) => control.focusable && &control.focus_id == id,
                Target::Focused => control.focused,
            }
        {
            // Popup rows have their own reveal operation and window bounds.
            let path = if control.window_coordinates.is_some() {
                Vec::new()
            } else {
                self.viewport.frames.clone()
            };
            self.found = Some((control.focus_id.clone(), bounds, path));
        }
    }
    fn finish(&self) -> Outcome<Message> {
        let Some((id, bounds, path)) = &self.found else {
            return Outcome::None;
        };
        let mut target = *bounds;
        let mut offsets = Vec::new();
        // Reveal the innermost viewport first; its resulting control position
        // determines how much each outer viewport needs to move.
        for frame in path.iter().rev() {
            let axis =
                |start: f32, extent: f32, viewport: f32, size: f32, current: f32, maximum: f32| {
                    let visible = start - current;
                    // iced rounds the final translation to whole logical
                    // pixels. Round outward so the last half pixel of a
                    // focused control cannot remain behind the viewport edge.
                    let desired = if visible < viewport || extent > size {
                        (start - viewport).floor()
                    } else if visible + extent > viewport + size {
                        (start + extent - viewport - size).ceil()
                    } else {
                        current
                    };
                    desired.clamp(0.0, maximum.max(0.0))
                };
            let offset = Vector::new(
                axis(
                    target.x,
                    target.width,
                    frame.bounds.x,
                    frame.bounds.width,
                    frame.translation.x,
                    frame.content.width - frame.bounds.width,
                ),
                axis(
                    target.y,
                    target.height,
                    frame.bounds.y,
                    frame.bounds.height,
                    frame.translation.y,
                    frame.content.height - frame.bounds.height,
                ),
            );
            offsets.push((frame.key, offset));
            target = target - offset;
        }
        Outcome::Chain(Box::new(Reveal {
            window: self.window,
            active: !self.marked,
            offsets,
            focus: self.focus.then(|| id.clone()),
        }))
    }
}

struct Reveal {
    window: window::Id,
    active: bool,
    offsets: Vec<(usize, Vector)>,
    focus: Option<Id>,
}

impl Operation<Message> for Reveal {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Message>)) {
        operate(self);
    }
    fn scrollable(
        &mut self,
        _: Option<&Id>,
        _: Rectangle,
        _: Rectangle,
        _: Vector,
        state: &mut dyn Scrollable,
    ) {
        if self.active
            && let Some((_, offset)) = self
                .offsets
                .iter()
                .find(|(identity, _)| *identity == key(state))
        {
            state.scroll_to(operation::scrollable::AbsoluteOffset {
                x: Some(offset.x),
                y: Some(offset.y),
            });
        }
    }
    fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(marker) = state.downcast_ref::<WindowMarker>() {
            self.active = marker.begin && marker.id == self.window;
        } else if self.active
            && self.focus.is_some()
            && let Some(marker) =
                state.downcast_mut::<super::super::keyboard_history::FocusReturn>()
        {
            marker.pending = false;
        }
    }
    fn focusable(&mut self, id: Option<&Id>, _: Rectangle, state: &mut dyn operation::Focusable) {
        if self.active
            && let Some(target) = &self.focus
        {
            if id == Some(target) {
                state.focus();
            } else {
                state.unfocus();
            }
        }
    }
    fn finish(&self) -> Outcome<Message> {
        Outcome::Some(Message::AccessibilityChanged(self.window))
    }
}

#[cfg(test)]
mod tests;
