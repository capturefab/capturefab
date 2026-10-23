//! Actual compact-sheet geometry, using the same software text shaping as the GUI.
use super::*;
use crate::gui::{Message as GuiMessage, accessibility};
use iced::advanced::{
    Layout, layout,
    renderer::Headless,
    widget::{Id, Operation, Tree, operation::scrollable},
};
use iced::{Rectangle, Size};

pub(crate) fn renderer() -> iced::Renderer {
    let mut fonts = iced::advanced::graphics::text::font_system()
        .write()
        .unwrap();
    fonts.load_font(std::borrow::Cow::Borrowed(crate::gui::icon::REGULAR_BYTES));
    fonts.load_font(std::borrow::Cow::Borrowed(crate::gui::icon::FILL_BYTES));
    drop(fonts);
    iced::futures::executor::block_on(iced::Renderer::new(
        style::SANS,
        iced::Pixels(16.0),
        Some("tiny-skia"),
    ))
    .expect("the compiled software renderer is available")
}

pub(crate) fn add_external_drive(picker: &mut Picker) {
    picker.volumes = vec![Volume {
        name: "Laboratory archive".into(),
        path: "/Volumes/Laboratory Archive".into(),
        kind: "removable",
        total_bytes: 2_000_000_000_000,
        free_bytes: 1_000_000_000_000,
    }];
}

#[derive(Default)]
struct BodyBounds(Option<Rectangle>);
impl Operation for BodyBounds {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn scrollable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        _: Rectangle,
        _: iced::Vector,
        _: &mut dyn scrollable::Scrollable,
    ) {
        if id == Some(&Id::new(crate::gui::widgets::SHEET_BODY)) {
            self.0 = Some(bounds);
        }
    }
}

pub(crate) fn render_artifact(
    name: &str,
    view: &Element<'_, GuiMessage>,
    tree: &Tree,
    node: &layout::Node,
    renderer: &mut iced::Renderer,
    size: Size,
) {
    render_artifact_scaled(name, view, tree, node, renderer, size, 1.0);
}

pub(crate) fn render_artifact_scaled(
    name: &str,
    view: &Element<'_, GuiMessage>,
    tree: &Tree,
    node: &layout::Node,
    renderer: &mut iced::Renderer,
    size: Size,
    scale_factor: f32,
) {
    render_artifact_themed(
        name,
        view,
        tree,
        node,
        renderer,
        size,
        (false, scale_factor),
    );
}

pub(crate) fn render_artifact_themed(
    name: &str,
    view: &Element<'_, GuiMessage>,
    tree: &Tree,
    node: &layout::Node,
    renderer: &mut iced::Renderer,
    size: Size,
    appearance: (bool, f32),
) {
    use iced::advanced::renderer::Renderer;
    let Ok(directory) = std::env::var("CAPTUREFAB_LAYOUT_ARTIFACTS") else {
        return;
    };
    let (dark, scale_factor) = appearance;
    let viewport = Rectangle::new(iced::Point::ORIGIN, size);
    renderer.reset(viewport);
    view.as_widget().draw(
        tree,
        renderer,
        if dark { &Theme::Dark } else { &Theme::Light },
        &iced::advanced::renderer::Style::default(),
        Layout::new(node),
        iced::mouse::Cursor::Unavailable,
        &viewport,
    );
    let physical = Size::new(
        (size.width * scale_factor).round() as u32,
        (size.height * scale_factor).round() as u32,
    );
    let pixels = renderer.screenshot(physical, scale_factor, Palette::of(dark).base);
    std::fs::create_dir_all(&directory).unwrap();
    let file = std::fs::File::create(PathBuf::from(directory).join(format!("{name}.png"))).unwrap();
    let mut encoder = png::Encoder::new(file, physical.width, physical.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(&pixels)
        .unwrap();
}

pub(crate) fn prepare_artifact_frame(
    view: &mut Element<'_, GuiMessage>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
    size: Size,
) {
    // iced caches Active/Disabled paint during its ordinary redraw event.
    // Layout alone would make enabled buttons look disabled in an artifact.
    let mut messages = Vec::new();
    view.as_widget_mut().update(
        tree,
        &iced::Event::Window(iced::window::Event::RedrawRequested(Instant::now())),
        Layout::new(node),
        iced::mouse::Cursor::Unavailable,
        renderer,
        &mut iced::advanced::clipboard::Null,
        &mut iced::advanced::Shell::new(&mut messages),
        &Rectangle::new(iced::Point::ORIGIN, size),
    );
}

pub(crate) fn apply(
    view: &mut Element<'_, GuiMessage>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
    operation: impl Operation<GuiMessage> + 'static,
) {
    let mut operation: Box<dyn Operation<GuiMessage>> = Box::new(operation);
    loop {
        view.as_widget_mut().operate(
            tree,
            Layout::new(node),
            renderer,
            &mut iced::advanced::widget::operation::black_box(operation.as_mut()),
        );
        match operation.finish() {
            iced::advanced::widget::operation::Outcome::Chain(next) => operation = next,
            _ => break,
        }
    }
}

pub(crate) fn assert_focus_traversal_reveals(
    view: &mut Element<'_, GuiMessage>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
) {
    use iced::advanced::widget::operation::focusable;
    #[derive(Default)]
    struct FocusedSize(Option<Size>);
    impl Operation for FocusedSize {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }
        fn focusable(
            &mut self,
            _: Option<&Id>,
            bounds: Rectangle,
            state: &mut dyn iced::advanced::widget::operation::Focusable,
        ) {
            if state.is_focused() {
                self.0 = Some(bounds.size());
            }
        }
    }
    let window = iced::window::Id::unique();
    let mut total = focusable::count();
    view.as_widget_mut().operate(
        tree,
        Layout::new(node),
        renderer,
        &mut iced::advanced::widget::operation::black_box(&mut total),
    );
    let iced::advanced::widget::operation::Outcome::Some(total) = total.finish() else {
        panic!("the real controls expose traversal state");
    };
    assert!(total.total > 0);
    for reverse in [false, true] {
        for _ in 0..=total.total {
            if reverse {
                apply(view, tree, node, renderer, focusable::focus_previous());
            } else {
                apply(view, tree, node, renderer, focusable::focus_next());
            }
            // Root wraps once when ordinary iced traversal clears at an edge.
            let mut count = focusable::count();
            view.as_widget_mut().operate(
                tree,
                Layout::new(node),
                renderer,
                &mut iced::advanced::widget::operation::black_box(&mut count),
            );
            let iced::advanced::widget::operation::Outcome::Some(count) = count.finish() else {
                panic!("the real controls expose traversal state");
            };
            if count.focused.is_none() {
                if reverse {
                    apply(view, tree, node, renderer, focusable::focus_previous());
                } else {
                    apply(view, tree, node, renderer, focusable::focus_next());
                }
            }
            apply(
                view,
                tree,
                node,
                renderer,
                accessibility::focus_reveal_operation(window),
            );
            let (snapshot, _, _) = accessibility::snapshot_widget(
                view.as_widget_mut(),
                tree,
                Layout::new(node),
                renderer,
            );
            let focused = snapshot
                .nodes
                .iter()
                .find(|(id, _)| *id == snapshot.focus)
                .expect("a real control retains keyboard focus");
            let bounds = focused.1.bounds().unwrap();
            let mut full = FocusedSize::default();
            view.as_widget_mut()
                .operate(tree, Layout::new(node), renderer, &mut full);
            let full = full.0.expect("the actual focused control's layout bounds");
            assert!(
                bounds.width() + 0.5 >= f64::from(full.width)
                    && bounds.height() + 0.5 >= f64::from(full.height)
                    && bounds.x0 >= -0.5
                    && bounds.y0 >= -0.5
                    && bounds.x1 <= f64::from(node.size().width) + 0.5
                    && bounds.y1 <= f64::from(node.size().height) + 0.5,
                "{} remains visible during {} traversal: {bounds:?}; full {full:?}, viewport {:?}",
                focused.1.label().unwrap_or("focused control"),
                if reverse { "Shift-Tab" } else { "Tab" },
                node.size(),
            );
        }
    }
}

#[test]
fn compact_destination_editor_keeps_header_and_footer_actions_inside_the_sheet() {
    let mut renderer = renderer();
    for native in [
        Size::new(900.0, 620.0),
        Size::new(1061.0, 518.0),
        Size::new(740.0, 480.0),
        Size::new(640.0, 360.0),
    ] {
        for scale in crate::gui::prefs::UiScale::ALL {
            let viewport = native / scale.factor();
            let max_height = (viewport.height - 80.0).max(320.0);
            let mut editor = Editor::new_folder("archive".into(), PathBuf::from("/tmp/captures"));
            editor.name = "laboratory-camera-archive-with-a-long-destination-name".into();
            editor.original = Some(editor.name.clone());
            editor.message = Some((
                "The destination could not be reached because the configured network storage service refused the request. Check the folder permissions, then try the destination again. ".repeat(4),
                true,
            ));
            let mut picker = Picker::new(false, false);
            picker.manager_open = true;
            picker.editor = Some(editor);
            let mut view = accessibility::mapped(
                picker.manager(
                    Palette::of(false),
                    max_height,
                    0,
                    Instant::now(),
                    None,
                    None,
                ),
                GuiMessage::CapturePicker,
            );
            let mut tree = Tree::new(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport),
            );
            let (snapshot, _, _) = accessibility::snapshot_widget(
                view.as_widget_mut(),
                &mut tree,
                Layout::new(&node),
                &renderer,
            );
            assert!(
                snapshot
                    .nodes
                    .iter()
                    .any(|(_, node)| node.role() == accessibility::Role::Heading
                        && node.label()
                            == Some(
                                format!("Edit {}", picker.editor.as_ref().unwrap().name).as_str()
                            )),
                "the shortened visual title retains its complete accessible heading"
            );
            let mut body = BodyBounds::default();
            view.as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut body);
            assert!(
                body.0.unwrap().height >= 80.0,
                "a full form row remains usable at native {native:?}, {scale}"
            );
            prepare_artifact_frame(&mut view, &mut tree, &node, &renderer, viewport);
            render_artifact_scaled(
                &format!("destination-{}x{}-{scale}", native.width, native.height),
                &view,
                &tree,
                &node,
                &mut renderer,
                viewport,
                scale.factor(),
            );
            let actions: Vec<_> = snapshot
                .nodes
                .iter()
                .filter(|(_, node)| {
                    node.role() == accessibility::Role::Button
                        && node.label().is_some_and(|label| {
                            matches!(label, "Back" | "Test" | "Cancel" | "Save")
                                || label.starts_with("Close ")
                                || label.starts_with("Check that ")
                        })
                })
                .collect();
            assert_eq!(actions.len(), 5);
            for (_, action) in actions {
                let bounds = action.bounds().unwrap();
                assert!(
                    bounds.x0 >= 0.0
                        && bounds.y0 >= 0.0
                        && bounds.x1 <= f64::from(node.size().width) + 0.5
                        && bounds.y1 <= f64::from(max_height.min(viewport.height)) + 0.5
                        && bounds.width() >= 20.0
                        && bounds.height() >= 20.0,
                    "{} at {bounds:?} is clipped at native {native:?}, {scale}; sheet {:?}",
                    action.label().unwrap(),
                    node.size(),
                );
                let mut messages = Vec::new();
                let cursor = iced::mouse::Cursor::Available(iced::Point::new(
                    ((bounds.x0 + bounds.x1) / 2.0) as f32,
                    ((bounds.y0 + bounds.y1) / 2.0) as f32,
                ));
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
                        &Rectangle::new(iced::Point::ORIGIN, viewport),
                    );
                }
                let label = action.label().unwrap();
                assert!(
                    messages.iter().any(|message| match (label, message) {
                        ("Back" | "Cancel", GuiMessage::CapturePicker(Message::Cancel)) => true,
                        ("Save", GuiMessage::CapturePicker(Message::Save)) => true,
                        (label, GuiMessage::CapturePicker(Message::CloseManager))
                            if label.starts_with("Close ") =>
                            true,
                        (label, GuiMessage::CapturePicker(Message::Test))
                            if label.starts_with("Check that ") =>
                            true,
                        _ => false,
                    }),
                    "{label} is pointer-reachable at its reported bounds"
                );
            }
        }
    }
}

#[test]
fn compact_sheet_keeps_a_short_form_natural_and_long_manager_rows_scrollable() {
    let renderer = renderer();
    let mut picker = Picker::new(false, false);
    picker.manager_open = true;
    picker.editor = Some(Editor::new_folder(
        "archive".into(),
        PathBuf::from("/tmp/captures"),
    ));
    let mut view = picker.manager(Palette::of(false), 640.0, 0, Instant::now(), None, None);
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(560.0, 640.0)),
    );
    assert!(
        node.size().height < 400.0,
        "a short folder form does not fill the entire sheet height"
    );
    drop(view);

    picker.editor = None;
    picker.saved = (0..12)
        .map(|index| Destination {
            name: format!("laboratory-line-{index}-archive-with-a-very-long-name"),
            target: Target::Folder {
                path: PathBuf::from(
                    "/Volumes/Laboratory Network Archive/a-long-directory-name/camera-captures",
                ),
            },
        })
        .collect();
    for scale in crate::gui::prefs::UiScale::ALL {
        let viewport = Size::new(740.0, 480.0) / scale.factor();
        let mut view = accessibility::mapped(
            picker.manager(
                Palette::of(false),
                (viewport.height - 80.0).max(320.0),
                0,
                Instant::now(),
                None,
                None,
            ),
            GuiMessage::CapturePicker,
        );
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport),
        );
        let (snapshot, _, _) = accessibility::snapshot_widget(
            view.as_widget_mut(),
            &mut tree,
            Layout::new(&node),
            &renderer,
        );
        {
            let action = "Add folder…";
            let button = snapshot
                .nodes
                .iter()
                .find(|(_, node)| {
                    node.role() == accessibility::Role::Button && node.label() == Some(action)
                })
                .unwrap();
            let bounds = button.1.bounds().unwrap();
            assert!(
                bounds.x0 >= 0.0
                    && bounds.x1 <= f64::from(node.size().width) + 0.5
                    && bounds.y1 <= f64::from(viewport.height),
                "{action} remains reachable below long destination rows at {scale}"
            );
        }
        let mut body = BodyBounds::default();
        view.as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut body);
        assert!(body.0.unwrap().height >= 80.0);
    }
}

#[test]
fn compact_editor_outcome_keeps_the_existing_field_focus_and_native_identity() {
    let renderer = renderer();
    let mut picker = Picker::new(false, false);
    picker.manager_open = true;
    picker.editor = Some(Editor::new_folder(
        "archive".into(),
        PathBuf::from("/tmp/captures"),
    ));
    let mut tree = Tree::empty();
    let inspect = |picker: &Picker, tree: &mut Tree, focus: bool| {
        let mut view = accessibility::mapped(
            picker.manager(Palette::of(false), 400.0, 0, Instant::now(), None, None),
            GuiMessage::CapturePicker,
        );
        tree.diff(view.as_widget());
        let node = view.as_widget_mut().layout(
            tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(560.0, 400.0)),
        );
        if focus {
            let mut focus =
                iced::advanced::widget::operation::focusable::focus::<()>(Id::new(NAME_FIELD));
            view.as_widget_mut()
                .operate(tree, Layout::new(&node), &renderer, &mut focus);
        }
        let (snapshot, _, _) = accessibility::snapshot_widget(
            view.as_widget_mut(),
            tree,
            Layout::new(&node),
            &renderer,
        );
        let (id, field) = snapshot
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == accessibility::Role::TextInput
                    && node.label() == Some("Destination name")
            })
            .unwrap();
        (*id, snapshot.focus, field.bounds().unwrap().y0)
    };
    let before = inspect(&picker, &mut tree, true);
    assert_eq!(before.0, before.1);
    picker.editor.as_mut().unwrap().message = Some((
        "The destination could not be reached. Check the network connection, then try again."
            .into(),
        true,
    ));
    assert_eq!(
        inspect(&picker, &mut tree, false),
        before,
        "an asynchronous outcome preserves the existing editor state"
    );
    picker.editor.as_mut().unwrap().message = None;
    assert_eq!(
        inspect(&picker, &mut tree, false),
        before,
        "clearing the outcome preserves the same editor state"
    );
}

#[test]
fn compact_editor_tab_and_shift_tab_reveal_the_actual_fields_and_pinned_actions() {
    let renderer = renderer();
    for scale in crate::gui::prefs::UiScale::ALL {
        let viewport = Size::new(640.0, 360.0) / scale.factor();
        let mut picker = Picker::new(false, false);
        picker.manager_open = true;
        let editor = Editor::new_folder("archive".into(), PathBuf::from("/tmp/captures"));
        picker.editor = Some(editor);
        let mut view = accessibility::mapped(
            picker.manager(
                Palette::of(false),
                (viewport.height - 80.0).max(320.0),
                0,
                Instant::now(),
                None,
                None,
            ),
            GuiMessage::CapturePicker,
        );
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport),
        );
        assert_focus_traversal_reveals(&mut view, &mut tree, &node, &renderer);
    }
}

#[test]
fn footer_undo_and_redo_fit_beside_the_actions_at_every_interface_size() {
    let renderer = renderer();
    for scale in crate::gui::prefs::UiScale::ALL {
        let viewport = Size::new(640.0, 480.0) / scale.factor();
        let mut picker = Picker::new(false, false);
        let mut output = String::new();
        picker.update(Message::AddFolder, &mut output);
        picker.update(Message::Name("line-1".into()), &mut output);
        picker.update(Message::Folder("/tmp/second".into()), &mut output);
        picker.undo();
        let mut view = accessibility::mapped(
            picker.manager(
                Palette::of(false),
                (viewport.height - 80.0).max(320.0),
                0,
                Instant::now(),
                None,
                None,
            ),
            GuiMessage::CapturePicker,
        );
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport),
        );
        let (snapshot, _, _) = accessibility::snapshot_widget(
            view.as_widget_mut(),
            &mut tree,
            Layout::new(&node),
            &renderer,
        );
        let mut found = 0;
        for (_, action) in &snapshot.nodes {
            let Some(label) = action.label() else {
                continue;
            };
            if action.role() != accessibility::Role::Button
                || !(label.starts_with("Undo ")
                    || label.starts_with("Redo ")
                    || label.starts_with("Check that ")
                    || matches!(label, "Cancel" | "Save"))
            {
                continue;
            }
            found += 1;
            let bounds = action.bounds().unwrap();
            assert!(
                bounds.x0 >= 0.0
                    && bounds.x1 <= f64::from(node.size().width) + 0.5
                    && bounds.width() >= 20.0,
                "{label} at {bounds:?} fits the {:?} sheet at {scale}",
                node.size()
            );
        }
        assert_eq!(found, 5, "Undo, Redo, Test, Cancel and Save at {scale}");
    }
}
