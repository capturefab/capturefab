use super::{Command, Item, Workbench};
use crate::gui::{
    Message,
    keys::{Action, Key},
};
use objc2::rc::Retained;
use objc2::runtime::Sel;
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSMenu, NSMenuItem};
use objc2_foundation::{MainThreadMarker, NSObject, NSObjectProtocol, NSString};
use std::{cell::RefCell, sync::Mutex};

static EVENTS: Mutex<Option<iced::futures::channel::mpsc::Sender<Message>>> = Mutex::new(None);

define_class!(
    // SAFETY: NSObject imposes no subclassing requirements. All instances and
    // their menu callbacks stay on AppKit's main thread.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = Vec<Command>]
    #[name = "CapturefabMenuTarget"]
    struct Target;

    unsafe impl NSObjectProtocol for Target {}

    impl Target {
        #[unsafe(method(validateMenuItem:))]
        fn validate(&self, sender: &NSMenuItem) -> bool {
            sender.isEnabled()
        }

        #[unsafe(method(capturefabCommand:))]
        fn command(&self, sender: &NSMenuItem) {
            if let Ok(index) = usize::try_from(sender.tag())
                && let Some(command) = self.ivars().get(index)
                && let Some(output) = EVENTS.lock().unwrap_or_else(|error| error.into_inner()).as_mut()
            {
                let _ = output.try_send(Message::Menu(*command));
            }
        }
    }
);

struct Native {
    root: Retained<NSMenu>,
    // NSMenuItem's target is not retained by AppKit.
    _target: Retained<Target>,
    items: Vec<Retained<NSMenuItem>>,
}

thread_local! {
    static MENU: RefCell<Option<Native>> = const { RefCell::new(None) };
}

pub(in crate::gui) fn subscription() -> iced::Subscription<Message> {
    iced::Subscription::run(events)
}

fn events() -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(100, async move |output| {
        *EVENTS.lock().unwrap_or_else(|error| error.into_inner()) = Some(output);
        std::future::pending::<()>().await;
    })
}

pub(in crate::gui) fn sync(workbench: &Workbench) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let model = workbench.command_menus();
    let settings = Item::action(
        "Settings…",
        Action::Settings,
        workbench.close_error.is_none() && (!workbench.sheet_open() || workbench.settings_open),
    );
    let quit = Item {
        shortcut: Some(crate::gui::keys::Chord {
            key: Key::Char('q'),
            mods: crate::gui::keys::Mods {
                command: true,
                ..crate::gui::keys::Mods::NONE
            },
        }),
        ..Item::command("Quit Capturefab", Command::Quit, true)
    };
    let specs: Vec<_> = std::iter::once(&settings)
        .chain(std::iter::once(&quit))
        .chain(model.iter().flat_map(|menu| menu.items.iter().flatten()))
        .collect();
    MENU.with_borrow_mut(|slot| {
        let app = NSApplication::sharedApplication(mtm);
        let native = slot.get_or_insert_with(|| {
            let this =
                Target::alloc(mtm).set_ivars(specs.iter().map(|item| item.command).collect());
            // SAFETY: NSObject's init has this signature and initializes Target.
            let target: Retained<Target> = unsafe { msg_send![super(this), init] };
            let root = NSMenu::new(mtm);
            let app_menu = submenu(mtm, &root, "Capturefab");
            app_menu.addItem(&system_item(
                mtm,
                "About Capturefab",
                sel!(orderFrontStandardAboutPanel:),
                "",
                NSEventModifierFlags::empty(),
            ));
            separator(mtm, &app_menu);
            let mut items = Vec::new();
            add_command(mtm, &app_menu, &settings, &target, &mut items);
            separator(mtm, &app_menu);
            let services = submenu(mtm, &app_menu, "Services");
            app.setServicesMenu(Some(&services));
            separator(mtm, &app_menu);
            app_menu.addItem(&system_item(
                mtm,
                "Hide Capturefab",
                sel!(hide:),
                "h",
                NSEventModifierFlags::Command,
            ));
            app_menu.addItem(&system_item(
                mtm,
                "Hide Others",
                sel!(hideOtherApplications:),
                "h",
                NSEventModifierFlags::Command | NSEventModifierFlags::Option,
            ));
            app_menu.addItem(&system_item(
                mtm,
                "Show All",
                sel!(unhideAllApplications:),
                "",
                NSEventModifierFlags::empty(),
            ));
            separator(mtm, &app_menu);
            add_command(mtm, &app_menu, &quit, &target, &mut items);

            for menu in &model {
                // The Window menu belongs immediately before Help.
                if menu.title == "Help" {
                    let windows = submenu(mtm, &root, "Window");
                    windows.addItem(&system_item(
                        mtm,
                        "Minimize",
                        sel!(performMiniaturize:),
                        "m",
                        NSEventModifierFlags::Command,
                    ));
                    windows.addItem(&system_item(
                        mtm,
                        "Zoom",
                        sel!(performZoom:),
                        "",
                        NSEventModifierFlags::empty(),
                    ));
                    separator(mtm, &windows);
                    windows.addItem(&system_item(
                        mtm,
                        "Bring All to Front",
                        sel!(arrangeInFront:),
                        "",
                        NSEventModifierFlags::empty(),
                    ));
                    app.setWindowsMenu(Some(&windows));
                }
                let child = submenu(mtm, &root, menu.title);
                // Workbench availability is authoritative, including while a
                // command is pending or a sheet owns the keyboard.
                child.setAutoenablesItems(false);
                for item in &menu.items {
                    if let Some(item) = item {
                        add_command(mtm, &child, item, &target, &mut items);
                    } else {
                        separator(mtm, &child);
                    }
                }
                if menu.title == "Help" {
                    app.setHelpMenu(Some(&child));
                }
            }
            Native {
                root,
                _target: target,
                items,
            }
        });
        for (spec, item) in specs.iter().zip(&native.items) {
            let title = NSString::from_str(&spec.title);
            if !item.title().isEqualToString(&title) {
                item.setTitle(&title);
            }
            item.setEnabled(spec.enabled);
            item.setState(if spec.checked { 1 } else { 0 });
        }
        if app
            .mainMenu()
            .is_none_or(|menu| !std::ptr::eq(&*menu, &*native.root))
        {
            // winit creates its initial menu at launch. Install after the
            // window opens and restore ours if that default replaced it.
            app.setMainMenu(Some(&native.root));
        }
    });
}

fn submenu(mtm: MainThreadMarker, parent: &NSMenu, title: &str) -> Retained<NSMenu> {
    let item = NSMenuItem::new(mtm);
    item.setTitle(&NSString::from_str(title));
    let menu = NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str(title));
    item.setSubmenu(Some(&menu));
    parent.addItem(&item);
    menu
}

fn separator(mtm: MainThreadMarker, menu: &NSMenu) {
    menu.addItem(&NSMenuItem::separatorItem(mtm));
}

fn system_item(
    mtm: MainThreadMarker,
    title: &str,
    action: Sel,
    key: &str,
    flags: NSEventModifierFlags,
) -> Retained<NSMenuItem> {
    // SAFETY: These selectors are AppKit's documented responder-chain actions;
    // no custom target or pointer crosses this boundary.
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc(),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(key),
        )
    };
    item.setKeyEquivalentModifierMask(flags);
    item
}

fn add_command(
    mtm: MainThreadMarker,
    menu: &NSMenu,
    spec: &Item,
    target: &Target,
    items: &mut Vec<Retained<NSMenuItem>>,
) {
    let (key, flags) = equivalent(spec);
    // SAFETY: Target implements capturefabCommand: with an NSMenuItem sender
    // and is retained for the full lifetime of the installed menu.
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc(),
            &NSString::from_str(&spec.title),
            Some(sel!(capturefabCommand:)),
            &NSString::from_str(&key),
        )
    };
    item.setKeyEquivalentModifierMask(flags);
    item.setTag(items.len() as isize);
    unsafe { item.setTarget(Some(target)) };
    menu.addItem(&item);
    items.push(item);
}

fn equivalent(item: &Item) -> (String, NSEventModifierFlags) {
    let Some(chord) = item.shortcut else {
        return (String::new(), NSEventModifierFlags::empty());
    };
    let mut flags = NSEventModifierFlags::empty();
    if chord.mods.command {
        flags |= NSEventModifierFlags::Command;
    }
    if chord.mods.ctrl {
        flags |= NSEventModifierFlags::Control;
    }
    if chord.mods.alt {
        flags |= NSEventModifierFlags::Option;
    }
    if chord.mods.shift {
        flags |= NSEventModifierFlags::Shift;
    }
    let key = match chord.key {
        Key::Char('=') if item.command == Command::Action(Action::ZoomIn) => "+".into(),
        Key::Char(c) => c.to_string(),
        Key::F1 => "\u{f704}".into(),
        Key::F5 => "\u{f708}".into(),
        Key::F11 => "\u{f70e}".into(),
        Key::Left => "\u{f702}".into(),
        Key::Right => "\u{f703}".into(),
        Key::PageUp => "\u{f72c}".into(),
        Key::PageDown => "\u{f72d}".into(),
        _ => String::new(),
    };
    (key, flags)
}
