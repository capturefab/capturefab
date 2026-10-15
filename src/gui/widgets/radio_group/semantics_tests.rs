//! Real segmented controls retain named ownership without adding a Tab stop.
use super::*;
use crate::gui::{Appearance, Message, Palette, Workbench};
use accesskit::{Action, ActionRequest, NodeId, Toggled};
use iced::futures::StreamExt;

fn choices(selected: usize, dark_enabled: bool) -> Element<'static, Message> {
    crate::gui::widgets::segmented(
        "Appearance",
        vec![
            (
                "System",
                Some(Message::Appearance(Appearance::System)),
                String::new(),
            ),
            (
                "Light",
                Some(Message::Appearance(Appearance::Light)),
                String::new(),
            ),
            (
                "Dark",
                dark_enabled.then_some(Message::Appearance(Appearance::Dark)),
                String::new(),
            ),
        ],
        selected,
        // Native selection follows the model, independently of animation.
        0.25,
    )
}

fn collect(
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    renderer: &iced::Renderer,
) -> (
    accesskit::TreeUpdate,
    accessibility::Bridge,
    window::Id,
    layout::Node,
) {
    tree.diff(view.as_widget());
    let node = view.as_widget_mut().layout(
        tree,
        renderer,
        &layout::Limits::new(Size::ZERO, Size::new(520.0, 1500.0)),
    );
    let (snapshot, bridge, window) =
        accessibility::snapshot_widget(view.as_widget_mut(), tree, Layout::new(&node), renderer);
    (snapshot, bridge, window, node)
}

fn group<'a>(snapshot: &'a accesskit::TreeUpdate, name: &str) -> (NodeId, &'a accesskit::Node) {
    snapshot
        .nodes
        .iter()
        .find(|(_, node)| {
            node.role() == accessibility::Role::RadioGroup && node.label() == Some(name)
        })
        .map(|(id, node)| (*id, node))
        .unwrap_or_else(|| panic!("missing native radio group {name}"))
}

fn click(
    bridge: &accessibility::Bridge,
    window: window::Id,
    target: NodeId,
) -> iced::Task<Message> {
    bridge.dispatch(accessibility::Request {
        window,
        replay: None,
        action: ActionRequest {
            action: Action::Click,
            target,
            data: None,
        },
    })
}

#[test]
fn actual_segments_have_stable_radio_ownership_selection_focus_and_native_actions() {
    let renderer = crate::gui::destinations::layout_tests::renderer();
    let mut tree = Tree::empty();
    let mut previous_ids = None;
    let mut focus_id = None;
    for (selected, dark_enabled) in [(0, true), (1, false), (1, true), (2, true)] {
        let mut view = choices(selected, dark_enabled);
        let (_, _, _, node) = collect(&mut view, &mut tree, &renderer);
        if focus_id.is_none() {
            let mut focusables = Focused::default();
            view.as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut focusables);
            focus_id = Some(focusables.ids[1].clone());
            view.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut iced::advanced::widget::operation::focusable::focus::<()>(
                    focus_id.clone().unwrap(),
                ),
            );
        }
        let (snapshot, bridge, window, node) = collect(&mut view, &mut tree, &renderer);
        let (group_id, group) = group(&snapshot, "Appearance");
        assert!(!group.supports_action(Action::Focus));
        assert!(!group.supports_action(Action::Click));
        let ids = group.children().to_vec();
        assert_eq!(ids.len(), 3, "disabled choices keep their group ownership");
        let identity = (group_id, ids.clone());
        if let Some(previous) = &previous_ids {
            assert_eq!(
                &identity, previous,
                "ordinary value diffs retain native IDs"
            );
        }
        previous_ids = Some(identity);
        assert_eq!(
            snapshot.focus, ids[1],
            "the existing choice keeps real focus"
        );

        let consumer = accesskit_consumer::Tree::new(snapshot, true);
        let state = consumer.state();
        let parent = state.node_by_id(group_id).unwrap();
        assert!(parent.is_container_with_selectable_children());
        assert_eq!(
            parent
                .items(|_| accesskit_consumer::FilterResult::Include)
                .map(|node| node.id())
                .collect::<Vec<_>>(),
            ids
        );
        for (index, (id, name)) in ids.iter().zip(["System", "Light", "Dark"]).enumerate() {
            let child = state.node_by_id(*id).unwrap();
            assert_eq!(child.role(), accessibility::Role::RadioButton);
            assert_eq!(child.label().as_deref(), Some(name));
            assert_eq!(
                child
                    .selection_container(&|_| accesskit_consumer::FilterResult::Include)
                    .unwrap()
                    .id(),
                group_id
            );
            assert_eq!(
                child.toggled(),
                Some(if index == selected {
                    Toggled::True
                } else {
                    Toggled::False
                })
            );
            assert_eq!(child.is_disabled(), index == 2 && !dark_enabled);
        }

        assert_eq!(click(&bridge, window, group_id).units(), 0);
        if dark_enabled {
            let mut actions = iced_runtime::task::into_stream(click(&bridge, window, ids[2]))
                .expect("enabled choice offers the normal native Click action");
            assert!(matches!(
                iced::futures::executor::block_on(actions.next()),
                Some(iced_runtime::Action::Output(Message::Appearance(
                    Appearance::Dark
                )))
            ));
            assert!(iced::futures::executor::block_on(actions.next()).is_none());
        } else {
            let dark = state.node_by_id(ids[2]).unwrap();
            assert!(!dark.is_clickable(&|_| accesskit_consumer::FilterResult::Include));
            assert!(!dark.supports_action(Action::Focus, &|_| {
                accesskit_consumer::FilterResult::Include
            }));
            assert_eq!(click(&bridge, window, ids[2]).units(), 0);
        }
        let mut focusables = Focused::default();
        view.as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut focusables);
        assert_eq!(focusables.ids.len(), if dark_enabled { 3 } else { 2 });
        assert_eq!(focusables.current, Some(1));
        assert_eq!(focusables.ids[1], *focus_id.as_ref().unwrap());
    }
}

#[test]
fn actual_preferences_and_exposure_use_distinct_meaningful_radio_groups() {
    let renderer = crate::gui::destinations::layout_tests::renderer();
    let app = Workbench::new(
        crate::session::SessionHandle::new(),
        "native-radio-groups".into(),
        true,
        None,
    );
    let mut settings = app.settings_content(Palette::of(false));
    let (snapshot, _, _, _) = collect(&mut settings, &mut Tree::empty(), &renderer);
    for (name, count) in [("Appearance", 3), ("Interface size", 3)] {
        let (_, parent) = group(&snapshot, name);
        assert_eq!(parent.children().len(), count);
    }
    assert_eq!(
        snapshot
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == accessibility::Role::RadioGroup)
            .count(),
        2
    );
    let mut exposure = app.auto_card(Palette::of(false));
    let (snapshot, _, _, _) = collect(&mut exposure, &mut Tree::empty(), &renderer);
    let (_, parent) = group(&snapshot, "Exposure mode");
    assert_eq!(parent.children().len(), 2);
}
