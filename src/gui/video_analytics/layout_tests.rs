//! Exercise the native Settings content at its real fixed window size. Its
//! scroll body must expose the complete credential actions above the footer.
use super::*;
use crate::gui::{
    accessibility,
    destinations::layout_tests::{prepare_artifact_frame, render_artifact_scaled, renderer},
    prefs::UiScale,
};
use iced::advanced::{
    Layout, layout,
    widget::{Id, Operation, Tree, operation::scrollable},
};
use iced::{Point, Rectangle, Vector};

const FOOTER: &str = "Preferences apply automatically. API keys require Save key.";
const ACTIONS: [&str; 3] = [
    "Save API key in the OS credential store",
    "Remove saved key",
    "Refresh API credential status",
];

#[derive(Default)]
struct SettingsGeometry {
    body: Option<Rectangle>,
    content: Option<Rectangle>,
    offset: Vector,
    actions: Vec<(String, Rectangle)>,
    scroll_to_end: bool,
}

impl Operation for SettingsGeometry {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn scrollable(
        &mut self,
        _: Option<&Id>,
        bounds: Rectangle,
        content: Rectangle,
        offset: Vector,
        state: &mut dyn scrollable::Scrollable,
    ) {
        assert!(self.body.is_none(), "Settings has one scrolling body");
        self.body = Some(bounds);
        self.content = Some(content);
        self.offset = offset;
        if self.scroll_to_end {
            state.snap_to(scrollable::RelativeOffset {
                x: None,
                y: Some(1.0),
            });
        }
    }

    fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(control) = state.downcast_ref::<accessibility::Control<Message>>() {
            // Remove uses its visible text as the accessible label; that
            // fallback is resolved by the Collector, rather than stored in
            // the marker. Identify raw widget geometry by its actual action.
            let label = match control.activate.as_ref() {
                Some(Message::VideoAnalytics(Input::SaveKey)) => Some(ACTIONS[0]),
                Some(Message::VideoAnalytics(Input::DeleteKey)) => Some(ACTIONS[1]),
                Some(Message::VideoAnalytics(Input::RefreshKey)) => Some(ACTIONS[2]),
                _ => None,
            };
            if let Some(label) = label {
                self.actions.push((label.to_owned(), bounds));
            }
        }
    }
}

fn footer(snapshot: &accesskit::TreeUpdate) -> accesskit::Rect {
    snapshot
        .nodes
        .iter()
        .find(|(_, node)| node.label() == Some(FOOTER))
        .expect("the real footer text is visible")
        .1
        .bounds()
        .expect("footer has its actual text bounds")
}

#[test]
fn compact_native_settings_keep_credential_actions_visible_above_the_pinned_footer() {
    let mut renderer = renderer();
    let native = crate::gui::settings::window_settings().size;
    for scale in UiScale::ALL {
        for error in [false, true] {
            let viewport = native / scale.factor();
            let mut app = Workbench::new(
                SessionHandle::new(),
                "native-settings-layout".into(),
                true,
                None,
            );
            app.ui_scale = scale;
            assert!(app.scene_video_analytics(
                if error {
                    "settings-credential-error"
                } else {
                    "settings-credentials"
                },
                true,
            ));
            let mut view = app.settings_window(Palette::of(false));
            let mut tree = Tree::new(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport),
            );
            let (before, _, _) = accessibility::snapshot_widget(
                view.as_widget_mut(),
                &mut tree,
                Layout::new(&node),
                &renderer,
            );
            let pinned_footer = footer(&before);
            let mut scroll = SettingsGeometry {
                scroll_to_end: true,
                ..SettingsGeometry::default()
            };
            view.as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut scroll);
            let mut geometry = SettingsGeometry::default();
            view.as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut geometry);
            let body = geometry.body.expect("actual Settings scroll viewport");
            let content = geometry.content.unwrap();
            assert!(content.height > body.height);
            assert!(
                (geometry.offset.y - (content.height - body.height).round()).abs() < 0.6,
                "the real Settings body reaches its end at {scale}"
            );
            let (after, _, _) = accessibility::snapshot_widget(
                view.as_widget_mut(),
                &mut tree,
                Layout::new(&node),
                &renderer,
            );
            assert_eq!(footer(&after), pinned_footer, "the footer stays pinned");
            assert!(
                body.y + body.height <= pinned_footer.y0 as f32 - 10.0,
                "Settings reserves a visible gap before its pinned footer at {scale}"
            );
            assert!(
                pinned_footer.x0 >= 0.0
                    && pinned_footer.y0 >= 0.0
                    && pinned_footer.x1 <= f64::from(viewport.width) + 0.5
                    && pinned_footer.y1 <= f64::from(viewport.height) + 0.5,
                "the entire footer fits at {scale}: {pinned_footer:?}"
            );
            assert_eq!(geometry.actions.len(), ACTIONS.len());
            let mut action_bounds = Vec::new();
            for label in ACTIONS {
                let control = &after
                    .nodes
                    .iter()
                    .find(|(_, control)| control.label() == Some(label))
                    .expect("the actual credential action survives clipping")
                    .1;
                assert!(control.supports_action(accesskit::Action::Click));
                let bounds = control.bounds().unwrap();
                let full = geometry
                    .actions
                    .iter()
                    .find(|(name, _)| name == label)
                    .unwrap()
                    .1;
                assert!(
                    (bounds.width() - f64::from(full.width)).abs() < 0.6
                        && (bounds.height() - f64::from(full.height)).abs() < 0.6
                        && bounds.x0 >= f64::from(body.x) - 0.5
                        && bounds.x1 <= f64::from(body.x + body.width) + 0.5
                        && bounds.y0 >= f64::from(body.y) - 0.5
                        && bounds.y1 <= f64::from(body.y + body.height) + 0.5
                        && bounds.y1 < pinned_footer.y0,
                    "{label} must be entirely visible at {scale}, error={error}: {bounds:?}, full {full:?}"
                );
                action_bounds.push(bounds);
                // Exercise hit testing without dispatching the collected
                // message: a regression must never touch the credential store.
                let cursor = iced::mouse::Cursor::Available(Point::new(
                    ((bounds.x0 + bounds.x1) / 2.0) as f32,
                    ((bounds.y0 + bounds.y1) / 2.0) as f32,
                ));
                let mut messages = Vec::new();
                for event in [
                    iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left),
                    iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left),
                ] {
                    view.as_widget_mut().update(
                        &mut tree,
                        &iced::Event::Mouse(event),
                        Layout::new(&node),
                        cursor,
                        &renderer,
                        &mut iced::advanced::clipboard::Null,
                        &mut iced::advanced::Shell::new(&mut messages),
                        &Rectangle::new(Point::ORIGIN, viewport),
                    );
                }
                assert!(
                    messages.iter().any(|message| matches!(
                        (label, message),
                        (
                            "Save API key in the OS credential store",
                            Message::VideoAnalytics(Input::SaveKey)
                        ) | (
                            "Remove saved key",
                            Message::VideoAnalytics(Input::DeleteKey)
                        ) | (
                            "Refresh API credential status",
                            Message::VideoAnalytics(Input::RefreshKey)
                        )
                    )),
                    "{label} is pointer reachable at {scale}"
                );
            }
            for pair in action_bounds.windows(2) {
                assert!(
                    pair[0].x1 + 4.0 <= pair[1].x0,
                    "credential actions do not overlap"
                );
            }
            prepare_artifact_frame(&mut view, &mut tree, &node, &renderer, viewport);
            render_artifact_scaled(
                &format!("native-settings-credentials-{scale}-error-{error}"),
                &view,
                &tree,
                &node,
                &mut renderer,
                viewport,
                scale.factor(),
            );
        }
    }
}
