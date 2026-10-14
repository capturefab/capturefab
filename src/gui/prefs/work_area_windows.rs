//! Windows work-area, client and frame geometry use physical screen pixels.
//! winit makes the process per-monitor DPI aware; interface scaling applies
//! only when converting the native content minimum back to iced UI units.
use super::UiScale;
use iced::Size;

#[cfg(target_os = "windows")]
pub(super) fn query(
    handle: &dyn iced::window::Window,
    wanted_native: Size,
    minimum_native: Option<Size>,
) -> Option<Fit> {
    use windows::Win32::{
        Foundation::RECT,
        Graphics::Gdi::{
            GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
        },
        UI::{
            HiDpi::GetDpiForWindow,
            WindowsAndMessaging::{GetClientRect, GetWindowRect},
        },
    };
    let hwnd = hwnd(handle)?;
    let mut outer = RECT::default();
    let mut client = RECT::default();
    let mut monitor = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        rcMonitor: RECT::default(),
        rcWork: RECT::default(),
        dwFlags: 0,
    };
    // SAFETY: window::run supplies a live HWND on its UI thread. winit's
    // per-monitor DPI awareness makes these rectangles physical pixels.
    let dpi = unsafe {
        let display = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if display.is_invalid()
            || !GetMonitorInfoW(display, &mut monitor).as_bool()
            || GetWindowRect(hwnd, &mut outer).is_err()
            || GetClientRect(hwnd, &mut client).is_err()
        {
            return None;
        }
        GetDpiForWindow(hwnd)
    };
    fit_geometry(
        outer.into(),
        monitor.rcWork.into(),
        (client.right - client.left, client.bottom - client.top),
        wanted_native,
        minimum_native,
        dpi,
    )
}

#[cfg(target_os = "windows")]
pub(super) fn apply(handle: &dyn iced::window::Window, fit: Fit) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos};
    let Some(hwnd) = hwnd(handle) else {
        return false;
    };
    // SAFETY: the callback holds the live native window on its UI thread.
    // Preserve activation, z-order and visibility while fitting its frame.
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            fit.frame.left,
            fit.frame.top,
            fit.frame.width(),
            fit.frame.height(),
            SWP_NOACTIVATE | SWP_NOZORDER,
        )
        .is_ok()
    }
}

#[cfg(target_os = "windows")]
fn hwnd(handle: &dyn iced::window::Window) -> Option<windows::Win32::Foundation::HWND> {
    use iced::window::raw_window_handle::RawWindowHandle;
    let RawWindowHandle::Win32(handle) = handle.window_handle().ok()?.as_raw() else {
        return None;
    };
    Some(windows::Win32::Foundation::HWND(handle.hwnd.get() as *mut _))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl Rect {
    fn valid(self) -> bool {
        let width = i64::from(self.right) - i64::from(self.left);
        let height = i64::from(self.bottom) - i64::from(self.top);
        (1..=i64::from(i32::MAX)).contains(&width) && (1..=i64::from(i32::MAX)).contains(&height)
    }

    fn width(self) -> i32 {
        self.right - self.left
    }

    fn height(self) -> i32 {
        self.bottom - self.top
    }
}

#[cfg(target_os = "windows")]
impl From<windows::Win32::Foundation::RECT> for Rect {
    fn from(rect: windows::Win32::Foundation::RECT) -> Self {
        Self {
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Fit {
    frame: Rect,
    minimum_pixels: Option<(i32, i32)>,
    dpi: u32,
}

impl Fit {
    pub(super) fn minimum_ui(self, scale: UiScale) -> Option<Size> {
        let physical_per_ui = f64::from(self.dpi) / 96.0 * f64::from(scale.factor());
        self.minimum_pixels.map(|(width, height)| {
            Size::new(
                (f64::from(width) / physical_per_ui) as f32,
                (f64::from(height) / physical_per_ui) as f32,
            )
        })
    }
}

fn fit_geometry(
    current: Rect,
    work: Rect,
    client_pixels: (i32, i32),
    wanted_native: Size,
    minimum_native: Option<Size>,
    dpi: u32,
) -> Option<Fit> {
    if dpi == 0 || !current.valid() || !work.valid() {
        return None;
    }
    let chrome = (
        current.width().checked_sub(client_pixels.0)?,
        current.height().checked_sub(client_pixels.1)?,
    );
    if client_pixels.0 <= 0 || client_pixels.1 <= 0 || chrome.0 < 0 || chrome.1 < 0 {
        return None;
    }
    let available = (
        work.width().checked_sub(chrome.0)?,
        work.height().checked_sub(chrome.1)?,
    );
    if available.0 <= 0 || available.1 <= 0 {
        return None;
    }
    let physical_per_native = f64::from(dpi) / 96.0;
    let bounded_pixels = |size: Size| {
        (
            (f64::from(size.width) * physical_per_native)
                .round()
                .clamp(1.0, f64::from(available.0)) as i32,
            (f64::from(size.height) * physical_per_native)
                .round()
                .clamp(1.0, f64::from(available.1)) as i32,
        )
    };
    let minimum_pixels = minimum_native
        .filter(|size| super::valid_size(*size))
        .map(bounded_pixels);
    let wanted = if super::valid_size(wanted_native) {
        bounded_pixels(wanted_native)
    } else {
        minimum_pixels.unwrap_or((
            client_pixels.0.min(available.0),
            client_pixels.1.min(available.1),
        ))
    };
    let minimum = minimum_pixels.unwrap_or((1, 1));
    let width = wanted.0.max(minimum.0) + chrome.0;
    let height = wanted.1.max(minimum.1) + chrome.1;
    // Keep half-pixel center calculations wide until the final screen origin.
    // Work-area coordinates can be negative on secondary displays.
    let left = (i64::from(current.left) + i64::from(current.width() - width) / 2).clamp(
        i64::from(work.left),
        i64::from(work.right) - i64::from(width),
    ) as i32;
    let top = (i64::from(current.top) + i64::from(current.height() - height) / 2).clamp(
        i64::from(work.top),
        i64::from(work.bottom) - i64::from(height),
    ) as i32;
    Some(Fit {
        frame: Rect {
            left,
            top,
            right: left + width,
            bottom: top + height,
        },
        minimum_pixels,
        dpi,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, width: i32, height: i32) -> Rect {
        Rect {
            left,
            top,
            right: left + width,
            bottom: top + height,
        }
    }

    fn contained(frame: Rect, work: Rect) {
        assert!(frame.left >= work.left && frame.top >= work.top);
        assert!(frame.right <= work.right && frame.bottom <= work.bottom);
    }

    #[test]
    fn side_taskbar_and_top_panel_clamp_the_entire_native_frame() {
        for work in [rect(160, 0, 1760, 1080), rect(0, 120, 1920, 960)] {
            let fit = fit_geometry(
                rect(8, 8, 1936, 1119),
                work,
                (1920, 1080),
                Size::new(1920.0, 1080.0),
                Some(super::super::MIN_WINDOW),
                96,
            )
            .unwrap();
            assert_eq!(fit.frame, work);
            contained(fit.frame, work);
        }
    }

    #[test]
    fn high_dpi_small_display_lowers_iced_minimum_before_native_fitting() {
        let work = rect(0, 40, 1600, 800);
        let fit = fit_geometry(
            rect(-488, -440, 2576, 1720),
            work,
            (2560, 1680),
            Size::new(1280.0, 840.0),
            Some(super::super::MIN_WINDOW),
            192,
        )
        .unwrap();
        assert_eq!(fit.frame, work);
        assert_eq!(fit.minimum_pixels, Some((1584, 760)));
        for scale in UiScale::ALL {
            let minimum = fit.minimum_ui(scale).unwrap();
            let physical_per_ui = 2.0 * scale.factor();
            assert_eq!((minimum.width * physical_per_ui).round() as i32, 1584);
            assert_eq!((minimum.height * physical_per_ui).round() as i32, 760);
        }
    }

    #[test]
    fn settings_apply_interface_scale_once_and_include_title_bar_chrome() {
        let work = rect(-1920, -40, 1800, 1040);
        for scale in UiScale::ALL {
            let wanted_native = Size::new(520.0, 500.0) * scale.factor();
            let fit = fit_geometry(
                rect(-2100, -200, 1056, 1059),
                work,
                (1040, 1000),
                wanted_native,
                None,
                192,
            )
            .unwrap();
            contained(fit.frame, work);
            assert_eq!(fit.minimum_ui(scale), None);
            assert_eq!(
                fit.frame.width() - 16,
                (wanted_native.width * 2.0).round() as i32
            );
            assert_eq!(fit.frame.height(), 1040);
        }
    }

    #[test]
    fn center_is_preserved_and_repeated_fitting_is_stable_at_fractional_dpi() {
        let work = rect(-1600, 30, 1600, 870);
        let current = rect(-1350, 140, 1000, 600);
        let wanted = Size::new(520.0, 400.0);
        let fit = fit_geometry(current, work, (980, 550), wanted, None, 120).unwrap();
        assert_eq!(fit.frame, rect(-1185, 165, 670, 550));
        let again = fit_geometry(fit.frame, work, (650, 500), wanted, None, 120).unwrap();
        assert_eq!(again.frame, fit.frame);
    }

    #[test]
    fn invalid_native_geometry_falls_back_and_invalid_saved_size_uses_bounded_minimum() {
        let current = rect(0, 0, 1280, 840);
        let work = rect(0, 100, 1000, 600);
        for (bad_work, client, dpi) in [
            (rect(0, 0, 0, 600), (1264, 801), 96),
            (work, (1290, 801), 96),
            (work, (1264, 801), 0),
        ] {
            assert!(
                fit_geometry(
                    current,
                    bad_work,
                    client,
                    Size::new(900.0, 620.0),
                    None,
                    dpi
                )
                .is_none()
            );
        }
        let fit = fit_geometry(
            current,
            work,
            (1264, 801),
            Size::new(f32::NAN, f32::INFINITY),
            Some(super::super::MIN_WINDOW),
            96,
        )
        .unwrap();
        assert_eq!(fit.minimum_pixels, Some((900, 561)));
        assert_eq!((fit.frame.width(), fit.frame.height()), (916, 600));
        contained(fit.frame, work);
    }
}
