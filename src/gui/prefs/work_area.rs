//! Work-area geometry stays in AppKit's native logical screen coordinates.
//! Do not mix its bottom-left origins with iced/winit top-left positions.
#[cfg(target_os = "macos")]
pub(super) fn fit(
    handle: &dyn iced::window::Window,
    wanted: iced::Size,
    minimum: Option<iced::Size>,
) -> bool {
    use iced::window::raw_window_handle::RawWindowHandle;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSScreen, NSView};
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let Ok(handle) = handle.window_handle() else {
        return false;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return false;
    };
    // SAFETY: window::run provides the live content view on its UI thread.
    let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
    let Some(window) = view.window() else {
        return false;
    };
    let Some(screen) = window.screen().or_else(|| NSScreen::mainScreen(mtm)) else {
        return false;
    };
    let frame = window.frame();
    // This is also winit's definition of inner_size on macOS. It accounts for
    // both ordinary Settings title bars and FullSizeContentView main windows.
    let content = window.contentRectForFrameRect(frame);
    let visible = screen.visibleFrame();
    let Some(fit) = fit_geometry(
        Rect::from(frame),
        Rect::from(visible),
        (
            frame.size.width - content.size.width,
            frame.size.height - content.size.height,
        ),
        (f64::from(wanted.width), f64::from(wanted.height)),
        minimum.map(|size| (f64::from(size.width), f64::from(size.height))),
    ) else {
        return false;
    };
    // Native content constraints use screen points, independent of the app's
    // interface scale. Lower them before shrinking an oversized saved window.
    window.setContentMinSize(NSSize::new(fit.minimum.0, fit.minimum.1));
    window.setFrame_display(
        NSRect::new(
            NSPoint::new(fit.frame.x, fit.frame.y),
            NSSize::new(fit.frame.width, fit.frame.height),
        ),
        true,
    );
    true
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl Rect {
    fn valid(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && valid_size((self.width, self.height))
    }
}

#[cfg(target_os = "macos")]
impl From<objc2_foundation::NSRect> for Rect {
    fn from(rect: objc2_foundation::NSRect) -> Self {
        Self {
            x: rect.origin.x,
            y: rect.origin.y,
            width: rect.size.width,
            height: rect.size.height,
        }
    }
}

#[derive(Debug)]
struct Fit {
    frame: Rect,
    minimum: (f64, f64),
}

fn valid_size(size: (f64, f64)) -> bool {
    size.0.is_finite() && size.1.is_finite() && size.0 > 0.0 && size.1 > 0.0
}

fn fit_geometry(
    current: Rect,
    visible: Rect,
    chrome: (f64, f64),
    wanted: (f64, f64),
    minimum: Option<(f64, f64)>,
) -> Option<Fit> {
    if !current.valid()
        || !visible.valid()
        || !chrome.0.is_finite()
        || !chrome.1.is_finite()
        || chrome.0 < 0.0
        || chrome.1 < 0.0
    {
        return None;
    }
    let available = (visible.width - chrome.0, visible.height - chrome.1);
    if !valid_size(available) {
        return None;
    }
    let minimum = minimum
        .filter(|size| valid_size(*size))
        .map_or((0.0, 0.0), |minimum| {
            (minimum.0.min(available.0), minimum.1.min(available.1))
        });
    let wanted = if valid_size(wanted) {
        wanted
    } else if valid_size(minimum) {
        minimum
    } else {
        (current.width - chrome.0, current.height - chrome.1)
    };
    if !valid_size(wanted) {
        return None;
    }
    let content = (
        wanted
            .0
            .clamp(minimum.0.max(1.0).min(available.0), available.0),
        wanted
            .1
            .clamp(minimum.1.max(1.0).min(available.1), available.1),
    );
    let (width, height) = (
        (content.0 + chrome.0).min(visible.width),
        (content.1 + chrome.1).min(visible.height),
    );
    Some(Fit {
        frame: Rect {
            x: (current.x + (current.width - width) / 2.0)
                .clamp(visible.x, visible.x + visible.width - width),
            y: (current.y + (current.height - height) / 2.0)
                .clamp(visible.y, visible.y + visible.height - height),
            width,
            height,
        },
        minimum,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }
    fn contained(frame: Rect, visible: Rect) {
        assert!(frame.x >= visible.x && frame.y >= visible.y);
        assert!(frame.x + frame.width <= visible.x + visible.width);
        assert!(frame.y + frame.height <= visible.y + visible.height);
    }

    #[test]
    fn restored_main_window_fits_a_large_bottom_dock_and_menu_bar() {
        let visible = rect(0.0, 160.0, 1093.0, 430.0);
        let fitted = fit_geometry(
            rect(-93.5, -113.0, 1280.0, 840.0),
            visible,
            (0.0, 0.0),
            (1280.0, 840.0),
            Some((900.0, 620.0)),
        )
        .unwrap();
        assert_eq!(fitted.frame, visible);
        assert_eq!(fitted.minimum, (900.0, 430.0));
        contained(fitted.frame, visible);
    }

    #[test]
    fn side_docks_and_negative_secondary_screen_origins_clamp_the_entire_frame() {
        for visible in [
            rect(160.0, 24.0, 933.0, 590.0),
            rect(-1920.0, -900.0, 1720.0, 876.0),
        ] {
            let fitted = fit_geometry(
                rect(-2000.0, -1000.0, 2000.0, 1200.0),
                visible,
                (0.0, 28.0),
                (2000.0, 1200.0),
                Some((900.0, 620.0)),
            )
            .unwrap();
            contained(fitted.frame, visible);
            assert!(fitted.minimum.1 + 28.0 <= visible.height);
        }
    }

    #[test]
    fn scaled_settings_include_native_title_bar_height_without_a_main_minimum() {
        let visible = rect(0.0, 160.0, 1093.0, 430.0);
        for factor in [1.0, 1.15, 1.3] {
            let wanted = (520.0 * factor, 500.0 * factor);
            let fitted = fit_geometry(
                rect(100.0, 0.0, wanted.0, wanted.1 + 28.0),
                visible,
                (0.0, 28.0),
                wanted,
                None,
            )
            .unwrap();
            contained(fitted.frame, visible);
            assert_eq!(fitted.minimum, (0.0, 0.0));
            assert!((fitted.frame.width / factor - 520.0).abs() < 0.001);
            assert_eq!(fitted.frame.height, 430.0);
            assert!((fitted.frame.height - 28.0) / factor <= 500.0);
        }
    }

    #[test]
    fn fitting_preserves_center_when_reachable_and_moves_only_when_needed() {
        let visible = rect(0.0, 80.0, 1440.0, 796.0);
        let current = rect(200.0, 160.0, 1040.0, 600.0);
        let fitted = fit_geometry(current, visible, (0.0, 28.0), (520.0, 400.0), None).unwrap();
        assert_eq!(fitted.frame, rect(460.0, 246.0, 520.0, 428.0));
        let fitted_again =
            fit_geometry(fitted.frame, visible, (0.0, 28.0), (520.0, 400.0), None).unwrap();
        assert_eq!(fitted_again.frame, fitted.frame);
    }

    #[test]
    fn invalid_screen_geometry_is_rejected_and_invalid_saved_size_uses_safe_minimum() {
        let current = rect(200.0, 160.0, 1040.0, 600.0);
        assert!(
            fit_geometry(
                current,
                rect(0.0, 0.0, f64::NAN, 900.0),
                (0.0, 28.0),
                (520.0, 400.0),
                None
            )
            .is_none()
        );
        let fitted = fit_geometry(
            current,
            rect(0.0, 80.0, 1440.0, 796.0),
            (0.0, 28.0),
            (f64::NAN, f64::INFINITY),
            Some((900.0, 620.0)),
        )
        .unwrap();
        assert_eq!((fitted.frame.width, fitted.frame.height), (900.0, 648.0));
    }
}
