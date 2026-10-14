//! Native adapters are installed, updated and dropped on the window UI thread.
use super::Bridge;
use accesskit::{ActionHandler, ActionRequest, ActivationHandler, TreeUpdate};
use iced::window;
use std::{cell::RefCell, collections::HashMap};

struct Activate {
    bridge: Bridge,
    id: window::Id,
}
impl ActivationHandler for Activate {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        self.bridge.tree(self.id)
    }
}

struct Actions {
    bridge: Bridge,
    id: window::Id,
}
impl ActionHandler for Actions {
    fn do_action(&mut self, request: ActionRequest) {
        self.bridge.send(self.id, request);
    }
}

#[cfg(target_os = "macos")]
type Adapter = accesskit_macos::SubclassingAdapter;
#[cfg(target_os = "windows")]
type Adapter = accesskit_windows::SubclassingAdapter;
#[cfg(target_os = "linux")]
type Adapter = accesskit_unix::Adapter;

thread_local! { static ADAPTERS: RefCell<HashMap<window::Id, Adapter>> = RefCell::new(HashMap::new()); }

pub(super) fn attach(id: window::Id, handle: &dyn window::Window, bridge: Bridge) -> bool {
    if ADAPTERS.with(|adapters| adapters.borrow().contains_key(&id)) {
        return true;
    }
    let activation = Activate {
        bridge: bridge.clone(),
        id,
    };
    let actions = Actions { bridge, id };
    #[cfg(target_os = "macos")]
    let adapter = {
        let Ok(handle) = handle.window_handle() else {
            return false;
        };
        let raw_window_handle::RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return false;
        };
        // The pointer comes from the live hidden window, and this callback runs
        // on its UI thread. The adapter retains the NSView until detach.
        unsafe { Adapter::new(handle.ns_view.as_ptr(), activation, actions) }
    };
    #[cfg(target_os = "windows")]
    let adapter = {
        let Ok(handle) = handle.window_handle() else {
            return false;
        };
        let raw_window_handle::RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return false;
        };
        Adapter::new(
            windows::Win32::Foundation::HWND(handle.hwnd.get() as *mut _),
            activation,
            actions,
        )
    };
    #[cfg(target_os = "linux")]
    let adapter = {
        let _ = handle;
        struct Deactivate;
        impl accesskit::DeactivationHandler for Deactivate {
            fn deactivate_accessibility(&mut self) {}
        }
        Adapter::new(activation, actions, Deactivate)
    };
    ADAPTERS.with(|adapters| {
        adapters.borrow_mut().insert(id, adapter);
    });
    true
}

pub(super) fn sync(id: window::Id, handle: &dyn window::Window, bridge: &Bridge) -> bool {
    let Some(tree) = bridge.tree(id) else {
        return false;
    };
    let result = ADAPTERS.with(|adapters| {
        let mut adapters = adapters.borrow_mut();
        let adapter = adapters.get_mut(&id)?;
        #[cfg(target_os = "linux")]
        if let Some((outer, inner)) = x11_bounds(handle) {
            adapter.set_root_window_bounds(outer, inner);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = handle;
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let result = adapter.update_if_active(|| tree);
        #[cfg(target_os = "linux")]
        let result = {
            adapter.update_if_active(|| tree);
            true
        };
        Some(result)
    });
    let Some(result) = result else {
        return false;
    };
    // Native notification delivery can synchronously reenter accessibility.
    // Release the adapter map's runtime borrow before raising any events.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if let Some(events) = result {
        events.raise();
    }
    #[cfg(target_os = "linux")]
    let _ = result;
    true
}

/// AT-SPI screen coordinates need the actual X11 client origin, including the
/// WM's decorations. Wayland deliberately does not provide global positions.
#[cfg(target_os = "linux")]
fn x11_bounds(handle: &dyn window::Window) -> Option<(accesskit::Rect, accesskit::Rect)> {
    use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
    use std::cell::OnceCell;
    thread_local! { static XLIB: OnceCell<Option<x11_dl::xlib::Xlib>> = const { OnceCell::new() }; }
    let RawWindowHandle::Xlib(window) = handle.window_handle().ok()?.as_raw() else {
        return None;
    };
    let RawDisplayHandle::Xlib(display) = handle.display_handle().ok()?.as_raw() else {
        return None;
    };
    let display = display.display?.as_ptr().cast::<x11_dl::xlib::Display>();
    XLIB.with(|library| {
        let library = library
            .get_or_init(|| x11_dl::xlib::Xlib::open().ok())
            .as_ref()?;
        // The display and window handles are borrowed from the live window on
        // its UI thread. Xlib owns the connection and we do not close it.
        unsafe {
            let root = (library.XDefaultRootWindow)(display);
            let bounds = |window| {
                let mut attributes = std::mem::MaybeUninit::uninit();
                if (library.XGetWindowAttributes)(display, window, attributes.as_mut_ptr()) == 0 {
                    return None;
                }
                let attributes = attributes.assume_init();
                let (mut x, mut y, mut child) = (0, 0, 0);
                if (library.XTranslateCoordinates)(
                    display, window, root, 0, 0, &mut x, &mut y, &mut child,
                ) == 0
                {
                    return None;
                }
                Some(accesskit::Rect {
                    x0: x as f64,
                    y0: y as f64,
                    x1: (x + attributes.width) as f64,
                    y1: (y + attributes.height) as f64,
                })
            };
            let inner = bounds(window.window)?;
            let (mut parent, mut query_root, mut children, mut count) =
                (0, 0, std::ptr::null_mut(), 0);
            let queried = (library.XQueryTree)(
                display,
                window.window,
                &mut query_root,
                &mut parent,
                &mut children,
                &mut count,
            );
            if !children.is_null() {
                (library.XFree)(children.cast());
            }
            let outer = if queried != 0 && parent != root && parent != 0 {
                bounds(parent).unwrap_or(inner)
            } else {
                inner
            };
            Some((outer, inner))
        }
    })
}

pub(super) fn focused(id: window::Id, focused: bool) -> bool {
    let result = ADAPTERS.with(|adapters| {
        let mut adapters = adapters.borrow_mut();
        let adapter = adapters.get_mut(&id)?;
        #[cfg(target_os = "macos")]
        let result = adapter.update_view_focus_state(focused);
        #[cfg(target_os = "linux")]
        let result = {
            adapter.update_window_focus_state(focused);
            true
        };
        // The Windows subclass handles native WM_SETFOCUS/WM_KILLFOCUS.
        #[cfg(target_os = "windows")]
        let result = {
            let _ = (adapter, focused);
            true
        };
        Some(result)
    });
    let Some(result) = result else {
        return false;
    };
    #[cfg(target_os = "macos")]
    if let Some(events) = result {
        events.raise();
    }
    #[cfg(not(target_os = "macos"))]
    let _ = result;
    true
}

pub(super) fn detach(id: window::Id) -> bool {
    let adapter = ADAPTERS.with(|adapters| adapters.borrow_mut().remove(&id));
    let removed = adapter.is_some();
    // Dropping the native adapter can deliver element-destroyed notifications.
    // Keep that teardown outside the adapter map's runtime borrow as well.
    drop(adapter);
    removed
}
