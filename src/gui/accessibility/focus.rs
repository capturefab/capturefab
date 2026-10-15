//! Only an explicit, still-current native Focus request restores/activates its
//! owning window. Ordinary semantic updates never use this preparation.
use super::*;

pub(super) fn prepare(
    bridge: Bridge,
    id: window::Id,
    target: NodeId,
    focus: Task<Message>,
) -> Task<Message> {
    let mut focus = Some(focus);
    window::mode(id).then(move |mode| {
        // A modal or window retirement may have invalidated the route while
        // the UI thread answered the mode query. Do not revive that target.
        let current = bridge
            .0
            .snapshots
            .lock()
            .unwrap()
            .get(&id)
            .and_then(|snapshot| snapshot.routes.get(&target))
            .is_some_and(|route| route.focusable || route.widget_focus || route.on_focus.is_some());
        if !current {
            return Task::none();
        }
        // gain_focus has no effect on an invisible window. Change mode only
        // for Hidden; visible fullscreen must retain its existing mode.
        let show = if mode == window::Mode::Hidden {
            window::set_mode(id, window::Mode::Windowed)
        } else {
            Task::none()
        };
        show.chain(window::minimize(id, false))
            .chain(window::gain_focus(id))
            .chain(focus.take().unwrap_or_else(Task::none))
    })
}
