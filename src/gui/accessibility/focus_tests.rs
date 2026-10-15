//! Explicit native Focus may restore its owner; ordinary semantic updates may
//! not. Exercise both through the actual two-window editor trees and tasks.
use super::*;
use iced::advanced::widget::operation::{self, Outcome};
use iced_runtime::{Action as RuntimeAction, window::Action as WindowAction};

struct Surface {
    id: window::Id,
    title: &'static str,
    view: Element<'static, Message>,
    tree: Tree,
    node: layout::Node,
}

fn surfaces(bridge: &Bridge, renderer: &iced::Renderer) -> Vec<Surface> {
    ["Main draft", "Settings draft"]
        .into_iter()
        .map(|title| {
            let id = window::Id::unique();
            let mut view = wrap(
                crate::gui::text_input::text_input(title, "unfinished 👩‍💻 draft")
                    .id("shared-native-editor")
                    .on_input(Message::Address)
                    .into(),
                bridge.clone(),
                id,
                title.into(),
                1.0,
                Message::AccessibilityChanged,
            );
            let mut tree = Tree::new(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                renderer,
                &layout::Limits::new(Size::ZERO, Size::new(400.0, 60.0)),
            );
            view.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                renderer,
                &mut operation::focusable::focus::<()>(Id::new("shared-native-editor")),
            );
            view.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                renderer,
                &mut operation::text_input::select_all::<()>(Id::new("shared-native-editor")),
            );
            Surface {
                id,
                title,
                view,
                tree,
                node,
            }
        })
        .collect()
}

fn collect(surface: &mut Surface, renderer: &iced::Renderer, bridge: &Bridge) -> TreeUpdate {
    let mut collector = Collector::new(1.0);
    surface.view.as_widget_mut().operate(
        &mut surface.tree,
        Layout::new(&surface.node),
        renderer,
        &mut collector,
    );
    let (native, routes) = collector.finish(surface.title, surface.node.bounds());
    bridge.store(surface.id, native.clone(), routes);
    native
}

fn editor(native: &TreeUpdate) -> (NodeId, &Node) {
    native
        .nodes
        .iter()
        .find(|(_, node)| node.role() == Role::TextInput)
        .map(|(id, node)| (*id, node))
        .expect("actual editor is present")
}

fn request(window: window::Id, target: NodeId) -> Request {
    Request {
        window,
        replay: None,
        action: ActionRequest {
            action: Action::Focus,
            target,
            data: None,
        },
    }
}

#[test]
fn explicit_native_focus_prepares_hidden_minimized_or_fullscreen_owner_and_keeps_drafts() {
    let renderer = crate::gui::destinations::layout_tests::renderer();
    for mode in [
        window::Mode::Hidden,
        window::Mode::Windowed,
        window::Mode::Fullscreen,
    ] {
        for requested in 0..2 {
            let bridge = Bridge::default();
            let mut surfaces = surfaces(&bridge, &renderer);
            let before: Vec<_> = surfaces
                .iter_mut()
                .map(|surface| collect(surface, &renderer, &bridge))
                .collect();
            let window = surfaces[requested].id;
            let target = editor(&before[requested]).0;
            let mut actions =
                iced_runtime::task::into_stream(bridge.dispatch(request(window, target))).unwrap();
            let Some(RuntimeAction::Window(WindowAction::GetMode(id, reply))) =
                iced::futures::executor::block_on(actions.next())
            else {
                panic!("native Focus must inspect its owner's visibility before activation");
            };
            assert_eq!(id, window);
            reply.send(mode).unwrap();
            if mode == window::Mode::Hidden {
                assert!(
                    matches!(iced::futures::executor::block_on(actions.next()),
                        Some(RuntimeAction::Window(WindowAction::SetMode(id, window::Mode::Windowed))) if id == window
                    ),
                    "a hidden native owner must be shown before restoring/focusing it"
                );
            }
            assert!(
                matches!(iced::futures::executor::block_on(actions.next()),
                    Some(RuntimeAction::Window(WindowAction::Minimize(id, false))) if id == window
                ),
                "only the requested owner is restored; visible fullscreen is preserved"
            );
            assert!(matches!(iced::futures::executor::block_on(actions.next()),
                Some(RuntimeAction::Window(WindowAction::GainFocus(id))) if id == window
            ));
            let Some(RuntimeAction::Widget(mut operation)) =
                iced::futures::executor::block_on(actions.next())
            else {
                panic!("owning window must activate before its actual editor is focused");
            };
            loop {
                for surface in &mut surfaces {
                    surface.view.as_widget_mut().operate(
                        &mut surface.tree,
                        Layout::new(&surface.node),
                        &renderer,
                        operation.as_mut(),
                    );
                }
                match operation.finish() {
                    Outcome::Chain(next) => operation = next,
                    _ => break,
                }
            }
            drop(operation);
            let outputs = iced::futures::executor::block_on(actions.collect::<Vec<_>>());
            assert!(
                matches!(outputs.as_slice(), [RuntimeAction::Output(Message::AccessibilityChanged(id))] if *id == window)
            );
            for (surface, before) in surfaces.iter_mut().zip(&before) {
                let after = collect(surface, &renderer, &bridge);
                assert_eq!(after.focus, before.focus);
                assert_eq!(editor(&after).0, editor(before).0);
                assert_eq!(editor(&after).1.value(), editor(before).1.value());
                assert_eq!(
                    editor(&after).1.text_selection(),
                    editor(before).1.text_selection()
                );
            }
            // If the owning native window retires before answering its query,
            // no restore/activation or global widget focus may continue.
            let mut stale =
                iced_runtime::task::into_stream(bridge.dispatch(request(window, target))).unwrap();
            let Some(RuntimeAction::Window(WindowAction::GetMode(_, reply))) =
                iced::futures::executor::block_on(stale.next())
            else {
                panic!("native mode query expected");
            };
            drop(reply);
            assert!(iced::futures::executor::block_on(stale.next()).is_none());
        }
    }
}

#[test]
fn ordinary_native_sync_and_window_focus_bookkeeping_never_restore_or_activate_windows() {
    let bridge = Bridge::default();
    let window = window::Id::unique();
    for task in [
        bridge.sync(window),
        bridge.focused(window, false),
        bridge.focused(window, true),
    ] {
        let mut actions = iced_runtime::task::into_stream(task).unwrap();
        assert!(matches!(iced::futures::executor::block_on(actions.next()),
            Some(RuntimeAction::Window(WindowAction::Run(id, _))) if id == window
        ));
        assert!(
            iced::futures::executor::block_on(actions.next()).is_none(),
            "bookkeeping cannot show, restore, activate or focus a widget"
        );
    }
}

#[test]
fn pending_native_focus_cannot_restore_routes_retired_or_obscured_during_mode_query() {
    let renderer = crate::gui::destinations::layout_tests::renderer();
    for retired in [false, true] {
        let bridge = Bridge::default();
        let mut surfaces = surfaces(&bridge, &renderer);
        let native = collect(&mut surfaces[0], &renderer, &bridge);
        let window = surfaces[0].id;
        let target = editor(&native).0;
        let mut actions =
            iced_runtime::task::into_stream(bridge.dispatch(request(window, target))).unwrap();
        let Some(RuntimeAction::Window(WindowAction::GetMode(id, reply))) =
            iced::futures::executor::block_on(actions.next())
        else {
            panic!("native mode query expected");
        };
        assert_eq!(id, window);
        if retired {
            drop(bridge.detach(window));
        } else {
            bridge.invalidate(window);
        }
        reply.send(window::Mode::Hidden).unwrap();
        assert!(
            iced::futures::executor::block_on(actions.next()).is_none(),
            "removed or obscured controls cannot show, restore, activate or take widget focus"
        );
    }
}
