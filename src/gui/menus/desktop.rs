//! Window menus for Windows and Linux. They use the same command model as
//! AppKit, and preserve the editor's selection while an Edit menu is open.
use super::*;
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Operation, Tree, tree},
};
use iced::widget::column;
use iced::{
    Event, Rectangle, Vector,
    keyboard::{self, key},
    mouse,
};
mod semantics;
use crate::gui::accessibility;

const HEIGHT: f32 = 30.0;
const TAB_WIDTH: f32 = 68.0;
const POPUP_WIDTH: f32 = 360.0;

#[derive(Default)]
pub(in crate::gui) struct DesktopState {
    focused: Option<usize>,
    open: bool,
    selected: Option<usize>,
    editor: bool,
    alt_armed: bool,
    activation_held: bool,
}

impl DesktopState {
    pub(in crate::gui) fn active(&self) -> bool {
        self.focused.is_some()
    }

    pub(in crate::gui) fn cancel_alt(&mut self) {
        self.alt_armed = false;
    }

    fn close(&mut self) {
        *self = Self::default();
    }

    fn open(&mut self, index: usize, menus: &[Menu]) {
        if index >= menus.len() {
            return;
        }
        self.focused = Some(index);
        self.open = true;
        self.selected = enabled_indices(&menus[index]).first().copied();
        self.alt_armed = false;
    }

    fn navigate(&mut self, menus: &[Menu], forward: bool) {
        let index = self.focused.unwrap_or(0);
        if index >= menus.len() {
            return;
        }
        let enabled = enabled_indices(&menus[index]);
        if enabled.is_empty() {
            self.selected = None;
            return;
        }
        self.focused = Some(index);
        self.open = true;
        let current = self
            .selected
            .and_then(|selected| enabled.iter().position(|&i| i == selected));
        let next = match current {
            Some(at) if forward => (at + 1) % enabled.len(),
            Some(at) => (at + enabled.len() - 1) % enabled.len(),
            None if forward => 0,
            None => enabled.len() - 1,
        };
        self.selected = Some(enabled[next]);
    }

    fn pick(&mut self, index: usize, menus: &[Menu]) -> Option<Command> {
        let command = menus
            .get(self.focused?)?
            .items
            .get(index)?
            .as_ref()
            .filter(|item| item.enabled)?
            .command;
        self.close();
        Some(command)
    }

    fn input(&mut self, input: Input, menus: &[Menu], editor: bool) -> Option<Command> {
        if !self.active() {
            self.editor = editor;
        }
        match input {
            Input::Close => self.close(),
            Input::Focus => {
                if self.active() {
                    self.close();
                } else {
                    self.focused = Some(0);
                }
            }
            Input::FocusHeader(index) if index < menus.len() => {
                self.focused = Some(index);
                self.open = false;
                self.selected = None;
            }
            Input::FocusHeader(_) => {}
            Input::AltPressed => self.alt_armed = true,
            Input::ActivationReleased => self.activation_held = false,
            Input::CancelAlt => self.alt_armed = false,
            Input::AltReleased => {
                if self.alt_armed {
                    self.alt_armed = false;
                    return self.input(Input::Focus, menus, editor);
                }
            }
            Input::Open(index) => {
                if self.focused == Some(index) && self.open {
                    self.close();
                } else {
                    self.open(index, menus);
                }
            }
            Input::Hover(index) if self.open => self.open(index, menus),
            Input::Hover(_) => {}
            Input::HoverItem(index) | Input::FocusItem(index) => {
                self.selected = self
                    .focused
                    .and_then(|menu| menus.get(menu))
                    .and_then(|menu| menu.items.get(index))
                    .and_then(Option::as_ref)
                    .filter(|item| item.enabled)
                    .map(|_| index);
            }
            Input::Pick(index) => return self.pick(index, menus),
            Input::Activate if self.open => {
                if let Some(index) = self.selected {
                    let command = self.pick(index, menus);
                    self.activation_held = command.is_some();
                    return command;
                }
            }
            Input::Activate | Input::Down if !self.open => {
                self.open(self.focused.unwrap_or(0), menus);
            }
            Input::Down => self.navigate(menus, true),
            Input::Up if !self.open => {
                self.open(self.focused.unwrap_or(0), menus);
                self.selected = self
                    .focused
                    .and_then(|index| enabled_indices(&menus[index]).last().copied());
            }
            Input::Up => self.navigate(menus, false),
            Input::Left | Input::Right => {
                if menus.is_empty() {
                    return None;
                }
                let index = self.focused.unwrap_or(0);
                let next = if matches!(input, Input::Right) {
                    (index + 1) % menus.len()
                } else {
                    (index + menus.len() - 1) % menus.len()
                };
                if self.open {
                    self.open(next, menus);
                } else {
                    self.focused = Some(next);
                }
            }
            Input::Home | Input::End => {
                if let Some(menu) = self.focused.and_then(|index| menus.get(index)) {
                    let enabled = enabled_indices(menu);
                    self.selected = if matches!(input, Input::Home) {
                        enabled.first().copied()
                    } else {
                        enabled.last().copied()
                    };
                }
            }
            Input::Escape if self.open => {
                self.open = false;
                self.selected = None;
            }
            Input::Escape => self.close(),
            Input::Mnemonic(character) => {
                self.alt_armed = false;
                if let Some(index) = menus
                    .iter()
                    .position(|menu| first_letter(menu.title) == Some(character))
                {
                    self.open(index, menus);
                }
            }
            Input::Character(character) if self.open => {
                let menu = &menus[self.focused?];
                let matches: Vec<_> = enabled_indices(menu)
                    .into_iter()
                    .filter(|&index| {
                        first_letter(&menu.items[index].as_ref().unwrap().title) == Some(character)
                    })
                    .collect();
                if matches.len() == 1 {
                    return self.pick(matches[0], menus);
                }
                if !matches.is_empty() {
                    let next = self
                        .selected
                        .and_then(|selected| matches.iter().position(|&i| i == selected))
                        .map_or(0, |index| (index + 1) % matches.len());
                    self.selected = Some(matches[next]);
                }
            }
            Input::Character(_) => {}
            Input::Shortcut(chord) => {
                let command = menus
                    .iter()
                    .flat_map(|menu| menu.items.iter().flatten())
                    .find(|item| {
                        item.enabled && item.shortcut.is_some_and(|binding| binding.matches(&chord))
                    })
                    .map(|item| item.command);
                self.close();
                return command;
            }
            Input::Activate => {}
        }
        None
    }
}

#[derive(Clone, Copy, Debug)]
pub(in crate::gui) enum Input {
    Open(usize),
    Hover(usize),
    HoverItem(usize),
    FocusItem(usize),
    Pick(usize),
    Close,
    Focus,
    FocusHeader(usize),
    AltPressed,
    AltReleased,
    CancelAlt,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Escape,
    Activate,
    ActivationReleased,
    Mnemonic(char),
    Character(char),
    Shortcut(Chord),
}

fn first_letter(title: &str) -> Option<char> {
    title
        .chars()
        .find(char::is_ascii_alphabetic)
        .map(|c| c.to_ascii_lowercase())
}

fn enabled_indices(menu: &Menu) -> Vec<usize> {
    menu.items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.as_ref().is_some_and(|item| item.enabled))
        .map(|(index, _)| index)
        .collect()
}

fn item_role(command: Command) -> accessibility::Role {
    use accessibility::Role;
    match command {
        Command::Action(Action::FeaturesTab | Action::CaptureTab | Action::ForwardTab) => {
            Role::MenuItemRadio
        }
        Command::Action(
            Action::ToggleSidebar
            | Action::ToggleInspector
            | Action::ToggleActivity
            | Action::ToggleExposure
            | Action::ToggleFocusRegion
            | Action::ImageMode
            | Action::ZoomFit
            | Action::ZoomActual,
        ) => Role::MenuItemCheckBox,
        _ => Role::MenuItem,
    }
}

impl Workbench {
    fn desktop_model(&self) -> Vec<Menu> {
        let mut menus = self.command_menus();
        if self.desktop_menu.active() {
            // Opening a menu suppresses background widget operations. Remember
            // the editor's availability while keeping camera actions live.
            for item in menus
                .iter_mut()
                .flat_map(|menu| menu.items.iter_mut().flatten())
            {
                if matches!(item.command, Command::Edit(_)) {
                    item.enabled = self.desktop_menu.editor;
                }
            }
        }
        if let Some(file) = menus.first_mut() {
            let insert = file.items.len().saturating_sub(2);
            file.items.insert(
                insert,
                Some(Item::action(
                    "Settings…",
                    Action::Settings,
                    !self.sheet_open(),
                )),
            );
        }
        menus
    }

    pub(in crate::gui) fn desktop_menu_input(&mut self, input: Input) -> Task<Message> {
        if self.sheet_open() {
            self.desktop_menu.close();
            return Task::none();
        }
        let menus = self.desktop_model();
        let command = self.desktop_menu.input(input, &menus, self.menu_editor);
        if let Some(command) = command {
            // Go through the root update so undoable menu commands enter the
            // same history as their keyboard and toolbar equivalents.
            return Task::done(Message::Menu(command));
        }
        if let Some(selected) = self.desktop_menu.selected
            && matches!(
                input,
                Input::Up | Input::Down | Input::Home | Input::End | Input::FocusItem(_)
            )
            && let Some(menu) = self.desktop_menu.focused.and_then(|index| menus.get(index))
        {
            return operation::snap_to(
                "desktop-menu",
                operation::RelativeOffset {
                    x: 0.0,
                    y: selected as f32 / menu.items.len().saturating_sub(1).max(1) as f32,
                },
            );
        }
        Task::none()
    }

    pub(in crate::gui) fn desktop_menus<'a>(
        &'a self,
        body: Element<'a, Message>,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        if cfg!(target_os = "macos") {
            return body;
        }
        let menus = self.desktop_model();
        let active = self.desktop_menu.active();
        let headers = semantics::HeaderIds::default();
        let bar = menus.iter().enumerate().fold(row![], |bar, (index, menu)| {
            let lit = self.desktop_menu.focused == Some(index);
            bar.push(
                mouse_area(
                    button(text(menu.title).size(style::BODY))
                        .name(menu.title)
                        .role(accessibility::Role::MenuItem)
                        .expanded(self.desktop_menu.open && lit)
                        .width(TAB_WIDTH)
                        .height(HEIGHT)
                        .padding([4, 8])
                        .style(move |theme, status| {
                            let mut visual = style::plain(theme, status);
                            if lit {
                                visual.background = Some(p.selected.into());
                                visual.border = iced::Border {
                                    color: p.accent_fill,
                                    width: if motion::increase_contrast() {
                                        3.0
                                    } else {
                                        2.0
                                    },
                                    radius: style::RADIUS_SMALL.into(),
                                };
                            }
                            visual
                        })
                        .on_press(Message::DesktopMenu(Input::Open(index))),
                )
                .on_enter(Message::DesktopMenu(Input::Hover(index))),
            )
        });
        let bar = Element::new(semantics::Region {
            content: bar.into(),
            role: accessibility::Role::MenuBar,
            name: "Application menu".into(),
            focus: (!self.desktop_menu.open || self.desktop_menu.selected.is_none())
                .then_some(self.desktop_menu.focused)
                .flatten(),
            override_focus: active,
            inputs: (0..menus.len()).map(Input::FocusHeader).collect(),
            shortcuts: Vec::new(),
            headers: headers.clone(),
            popup_for: None,
        });
        let content: Element<'a, Message> = column![
            container(bar).width(Fill).style(style::base),
            rule::horizontal(1).style(style::line),
            keyboard_history::background(body, active),
        ]
        .height(Fill)
        .into();
        let mut layers = stack![content];
        if active {
            // The menu row remains interactive; everything below it dismisses
            // the menu rather than changing the preserved editor selection.
            layers = layers.push(column![
                space().height(HEIGHT + 1.0),
                mouse_area(space().width(Fill).height(Fill))
                    .on_press(Message::DesktopMenu(Input::Close)),
            ]);
        }
        if self.desktop_menu.open
            && let Some(index) = self.desktop_menu.focused
        {
            let items =
                menus[index]
                    .items
                    .iter()
                    .enumerate()
                    .fold(column![], |list, (position, item)| {
                        let Some(item) = item else {
                            return list.push(
                                container(rule::horizontal(1).style(style::line)).padding([4, 10]),
                            );
                        };
                        let selected = self.desktop_menu.selected == Some(position) && item.enabled;
                        let ink = if selected {
                            Color::WHITE
                        } else if item.enabled {
                            p.text
                        } else {
                            p.secondary
                        };
                        let check: Element<'a, Message> = if item.checked {
                            icon(Icon::Check, 13.0, ink)
                        } else {
                            space().width(13).into()
                        };
                        let shortcut = item
                            .shortcut
                            .map(|chord| chord.label(Os::CURRENT))
                            .unwrap_or_default();
                        let role = item_role(item.command);
                        let command = button(
                            row![
                                container(check).width(20),
                                text(item.title.clone())
                                    .size(style::BODY)
                                    .color(ink)
                                    .width(Fill),
                                text(shortcut).size(style::SMALL).color(ink),
                            ]
                            .spacing(8)
                            .align_y(Alignment::Center),
                        )
                        .name(item.title.clone())
                        .role(role)
                        .width(Fill)
                        .padding([6, 10])
                        .style(move |theme, status| {
                            let mut visual = style::plain(theme, status);
                            if selected {
                                visual.background = Some(p.accent_fill.into());
                            }
                            visual
                        })
                        .on_press_maybe(
                            item.enabled
                                .then_some(Message::DesktopMenu(Input::Pick(position))),
                        );
                        let command = if role == accessibility::Role::MenuItem {
                            command
                        } else {
                            command.checked(item.checked)
                        };
                        list.push(
                            mouse_area(command)
                                .on_enter(Message::DesktopMenu(Input::HoverItem(position))),
                        )
                    });
            let popup = container(Element::new(semantics::Region {
                content: scrollable(items)
                    .id("desktop-menu")
                    .style(style::sheet_scroll)
                    .into(),
                role: accessibility::Role::Menu,
                name: menus[index].title.into(),
                // Separators are absent from the native item list.
                focus: self
                    .desktop_menu
                    .selected
                    .map(|selected| menus[index].items[..selected].iter().flatten().count()),
                override_focus: true,
                inputs: menus[index]
                    .items
                    .iter()
                    .enumerate()
                    .filter_map(|(position, item)| {
                        item.as_ref().map(|_| Input::FocusItem(position))
                    })
                    .collect(),
                shortcuts: menus[index]
                    .items
                    .iter()
                    .flatten()
                    .map(|item| item.shortcut.map(|chord| chord.label(Os::CURRENT)))
                    .collect(),
                headers: headers.clone(),
                popup_for: Some(index),
            }))
            .width(POPUP_WIDTH)
            .max_height((self.height - HEIGHT - 12.0).max(100.0))
            .padding(4)
            .style(move |_| container::Style {
                background: Some(p.base.into()),
                border: iced::Border {
                    color: p.control_border,
                    width: 1.0,
                    radius: style::RADIUS.into(),
                },
                shadow: iced::Shadow {
                    color: p.shadow,
                    offset: Vector::new(0.0, 5.0),
                    blur_radius: 16.0,
                },
                ..Default::default()
            });
            layers = layers.push(
                container(popup).padding(
                    iced::Padding::ZERO
                        .top(HEIGHT + 1.0)
                        .left(index as f32 * TAB_WIDTH),
                ),
            );
        }
        Element::new(MenuKeyboard {
            content: layers.into(),
            active,
            activation_held: self.desktop_menu.activation_held,
        })
    }
}

struct MenuKeyboard<'a, R: renderer::Renderer = iced::Renderer> {
    content: Element<'a, Message, Theme, R>,
    active: bool,
    activation_held: bool,
}

impl<R: renderer::Renderer> Widget<Message, Theme, R> for MenuKeyboard<'_, R> {
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
    fn layout(&mut self, tree: &mut Tree, renderer: &R, limits: &layout::Limits) -> layout::Node {
        self.content.as_widget_mut().layout(tree, renderer, limits)
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
        self.content
            .as_widget()
            .draw(tree, renderer, theme, style, layout, cursor, viewport);
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &R,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(tree, layout, renderer, operation);
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &R,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if self.activation_held
            && matches!(
                event,
                Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(key::Named::Enter | key::Named::Space),
                    ..
                })
            )
        {
            shell.capture_event();
            return;
        }
        if (self.activation_held || self.active)
            && matches!(
                event,
                Event::Keyboard(keyboard::Event::KeyReleased {
                    key: keyboard::Key::Named(key::Named::Enter | key::Named::Space),
                    ..
                })
            )
        {
            shell.publish(Message::DesktopMenu(Input::ActivationReleased));
            shell.capture_event();
            return;
        }
        if matches!(
            event,
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
        ) && let Some(position) = cursor.position()
            && position.y >= layout.bounds().y
            && position.y < layout.bounds().y + HEIGHT
            && position.x >= layout.bounds().x
            && position.x < layout.bounds().x + 5.0 * TAB_WIDTH
        {
            // Native menu bars do not send their mouse-down to the document.
            // Intercept before a text field treats this as an outside click.
            let index = ((position.x - layout.bounds().x) / TAB_WIDTH) as usize;
            shell.publish(Message::DesktopMenu(Input::Open(index)));
            shell.capture_event();
            return;
        }
        let input = match event {
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                physical_key,
                modifiers,
                repeat,
                ..
            }) => {
                if *repeat
                    && self.active
                    && matches!(
                        key,
                        keyboard::Key::Named(key::Named::Enter | key::Named::Space)
                    )
                {
                    shell.capture_event();
                    return;
                }
                if *key == keyboard::Key::Named(key::Named::Alt) {
                    Some(Input::AltPressed)
                } else if *key == keyboard::Key::Named(key::Named::F10) && modifiers.is_empty() {
                    Some(Input::Focus)
                } else if modifiers.alt() && !modifiers.control() && !modifiers.logo() {
                    match key {
                        keyboard::Key::Character(text) => text
                            .chars()
                            .next()
                            .map(|c| Input::Mnemonic(c.to_ascii_lowercase())),
                        _ => Some(Input::CancelAlt),
                    }
                } else if self.active {
                    if modifiers.control() || modifiers.logo() {
                        keys::chord(key, *physical_key, *modifiers, Os::CURRENT)
                            .map(Input::Shortcut)
                    } else {
                        match key {
                            keyboard::Key::Named(key::Named::ArrowDown) => Some(Input::Down),
                            keyboard::Key::Named(key::Named::ArrowUp) => Some(Input::Up),
                            keyboard::Key::Named(key::Named::ArrowLeft) => Some(Input::Left),
                            keyboard::Key::Named(key::Named::ArrowRight) => Some(Input::Right),
                            keyboard::Key::Named(key::Named::Home) => Some(Input::Home),
                            keyboard::Key::Named(key::Named::End) => Some(Input::End),
                            keyboard::Key::Named(key::Named::Escape) => Some(Input::Escape),
                            keyboard::Key::Named(key::Named::Enter | key::Named::Space) => {
                                Some(Input::Activate)
                            }
                            keyboard::Key::Named(key::Named::Tab) => Some(Input::Close),
                            keyboard::Key::Character(text) => text
                                .chars()
                                .next()
                                .map(|c| Input::Character(c.to_ascii_lowercase())),
                            _ => None,
                        }
                    }
                } else {
                    None
                }
            }
            Event::Keyboard(keyboard::Event::KeyReleased {
                key: keyboard::Key::Named(key::Named::Alt),
                ..
            }) => Some(Input::AltReleased),
            Event::Window(window::Event::Unfocused) if self.active => Some(Input::Close),
            _ => None,
        };
        if let Some(input) = input {
            shell.publish(Message::DesktopMenu(input));
            if matches!(input, Input::CancelAlt) {
                self.content.as_widget_mut().update(
                    tree, event, layout, cursor, renderer, clipboard, shell, viewport,
                );
                return;
            }
            shell.capture_event();
            return;
        }
        if self.active
            && matches!(
                event,
                Event::Keyboard(
                    keyboard::Event::KeyPressed { .. } | keyboard::Event::KeyReleased { .. }
                )
            )
        {
            shell.capture_event();
            return;
        }
        self.content.as_widget_mut().update(
            tree, event, layout, cursor, renderer, clipboard, shell, viewport,
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
        self.content
            .as_widget()
            .mouse_interaction(tree, layout, cursor, viewport, renderer)
    }
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &R,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, R>> {
        self.content
            .as_widget_mut()
            .overlay(tree, layout, renderer, viewport, translation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::widget::column;

    fn menus() -> Vec<Menu> {
        vec![
            Menu {
                title: "File",
                items: vec![
                    Some(Item::action("Disabled", Action::Capture, false)),
                    None,
                    Some(Item::action("Discover", Action::Discover, true)),
                    Some(Item::action("Connect", Action::ConnectAddress, true)),
                ],
            },
            Menu {
                title: "Edit",
                items: vec![Some(Item::command(
                    "Copy",
                    Command::Edit(keyboard_history::EditCommand::Copy),
                    true,
                ))],
            },
        ]
    }

    #[test]
    fn navigation_skips_disabled_commands_and_separators_and_wraps() {
        let menus = menus();
        let mut state = DesktopState::default();
        state.input(Input::Mnemonic('f'), &menus, false);
        assert_eq!(state.selected, Some(2));
        state.input(Input::Up, &menus, false);
        assert_eq!(state.selected, Some(3));
        state.input(Input::Down, &menus, false);
        assert_eq!(state.selected, Some(2));
        assert!(state.input(Input::Pick(0), &menus, false).is_none());
        assert!(state.active());
        assert_eq!(
            state.input(Input::Activate, &menus, false),
            Some(Command::Action(Action::Discover))
        );
        assert!(!state.active());
        assert!(
            state.activation_held,
            "the activating key must not repeat into the document"
        );
        state.input(Input::ActivationReleased, &menus, false);
        assert!(!state.activation_held);
    }

    #[test]
    fn f10_arrows_and_escape_restore_the_underlying_editor_context() {
        let menus = menus();
        let mut state = DesktopState::default();
        state.input(Input::Focus, &menus, true);
        assert!(state.active());
        assert!(!state.open);
        state.input(Input::Right, &menus, false);
        state.input(Input::Down, &menus, false);
        assert_eq!(state.focused, Some(1));
        assert!(state.editor, "menu opening retains editor availability");
        state.input(Input::Escape, &menus, false);
        assert!(state.active(), "first Escape returns to the menu bar");
        state.input(Input::Escape, &menus, false);
        assert!(!state.active());
    }

    #[test]
    fn edit_menu_retains_editor_availability_and_settings_are_discoverable() {
        let mut app = Workbench::new(
            SessionHandle::new(),
            "desktop-menu-test".into(),
            false,
            None,
        );
        app.menu_editor = true;
        let menus = app.desktop_model();
        app.desktop_menu.input(Input::Open(1), &menus, true);
        app.menu_editor = false; // background widgets are excluded while open
        let shown = app.desktop_model();
        let edits: Vec<_> = shown
            .iter()
            .flat_map(|menu| menu.items.iter().flatten())
            .filter(|item| matches!(item.command, Command::Edit(_)))
            .collect();
        assert_eq!(edits.len(), 4);
        assert!(edits.iter().all(|item| item.enabled));
        assert!(
            shown[0]
                .items
                .iter()
                .flatten()
                .any(|item| item.command == Command::Action(Action::Settings) && item.enabled)
        );
        assert!(
            shown
                .iter()
                .flat_map(|menu| menu.items.iter().flatten())
                .filter(|item| item.command == Command::Action(Action::Capture))
                .all(|item| !item.enabled)
        );
    }

    #[test]
    fn menu_mouse_down_and_shortcuts_preserve_the_document_editor() {
        let field = iced::widget::TextInput::<Message, Theme, ()>::new("", "128")
            .on_input(Message::Address);
        let mut menu = MenuKeyboard {
            content: column![space().height(HEIGHT + 1.0), field].into(),
            active: false,
            activation_held: false,
        };
        let mut tree = Tree::new(&menu as &dyn Widget<Message, Theme, ()>);
        tree.children[1]
            .state
            .downcast_mut::<iced::widget::text_input::State<()>>()
            .focus();
        let node = menu.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(300.0, 100.0)),
        );
        let viewport = Rectangle::with_size(node.size());
        let mut clipboard = iced::advanced::clipboard::Null;
        let mut messages = Vec::new();
        menu.update(
            &mut tree,
            &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Layout::new(&node),
            mouse::Cursor::Available(iced::Point::new(10.0, 10.0)),
            &(),
            &mut clipboard,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        assert!(matches!(
            messages.as_slice(),
            [Message::DesktopMenu(Input::Open(0))]
        ));
        assert!(
            tree.children[1]
                .state
                .downcast_ref::<iced::widget::text_input::State<()>>()
                .is_focused()
        );

        menu.active = true;
        messages.clear();
        let modifiers = if Os::CURRENT == Os::Mac {
            keyboard::Modifiers::LOGO
        } else {
            keyboard::Modifiers::CTRL
        };
        let press = Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Character("r".into()),
            modified_key: keyboard::Key::Character("r".into()),
            physical_key: key::Physical::Code(key::Code::KeyR),
            location: keyboard::Location::Standard,
            modifiers,
            text: Some("r".into()),
            repeat: false,
        });
        let mut shell = Shell::new(&mut messages);
        menu.update(
            &mut tree,
            &press,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &(),
            &mut clipboard,
            &mut shell,
            &viewport,
        );
        assert_eq!(shell.event_status(), event::Status::Captured);
        assert!(matches!(
            messages.as_slice(),
            [Message::DesktopMenu(Input::Shortcut(_))]
        ));
        assert!(
            tree.children[1]
                .state
                .downcast_ref::<iced::widget::text_input::State<()>>()
                .is_focused()
        );

        menu.active = false;
        menu.activation_held = true;
        messages.clear();
        let repeat = Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key::Named::Enter),
            modified_key: keyboard::Key::Named(key::Named::Enter),
            physical_key: key::Physical::Code(key::Code::Enter),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: true,
        });
        let mut shell = Shell::new(&mut messages);
        menu.update(
            &mut tree,
            &repeat,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &(),
            &mut clipboard,
            &mut shell,
            &viewport,
        );
        assert_eq!(shell.event_status(), event::Status::Captured);
        assert!(
            messages.is_empty(),
            "closing a menu does not send its held Enter to the document"
        );
    }
}
