//! EWMH work areas, frame extents and XRandR monitor bounds use physical root
//! pixels. Only the returned iced content/minimum sizes include interface scale;
//! winit's outer-position task uses native logical coordinates instead.
use super::UiScale;
use iced::{Point, Size};
use std::os::raw::c_ulong;

const PROPERTY_LIMIT: usize = 1024;

#[cfg(target_os = "linux")]
pub(super) enum Query {
    Fit(Fit),
    FramePending,
    Unavailable,
}

#[cfg(target_os = "linux")]
pub(super) fn query(
    handle: &dyn iced::window::Window,
    wanted_native: Size,
    minimum_native: Option<Size>,
    display_scale: f32,
    scale: UiScale,
) -> Query {
    native::query(handle, wanted_native, minimum_native, display_scale, scale)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl Rect {
    fn valid(self) -> bool {
        self.width > 0 && self.height > 0
    }

    fn right(self) -> i64 {
        i64::from(self.x) + i64::from(self.width)
    }

    fn bottom(self) -> i64 {
        i64::from(self.y) + i64::from(self.height)
    }

    fn intersect(self, other: Self) -> Option<Self> {
        let x = i64::from(self.x).max(i64::from(other.x));
        let y = i64::from(self.y).max(i64::from(other.y));
        let width = self.right().min(other.right()) - x;
        let height = self.bottom().min(other.bottom()) - y;
        (width > 0 && height > 0).then_some(Self {
            x: x as i32,
            y: y as i32,
            width: width as i32,
            height: height as i32,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Extents {
    left: i32,
    right: i32,
    top: i32,
    bottom: i32,
}

impl Extents {
    fn from_property(values: &[u32]) -> Option<Self> {
        let [left, right, top, bottom] = values else {
            return None;
        };
        Some(Self {
            left: (*left).try_into().ok()?,
            right: (*right).try_into().ok()?,
            top: (*top).try_into().ok()?,
            bottom: (*bottom).try_into().ok()?,
        })
    }

    fn frame(self, client: Rect) -> Option<Rect> {
        Some(Rect {
            x: client.x.checked_sub(self.left)?,
            y: client.y.checked_sub(self.top)?,
            width: client
                .width
                .checked_add(self.left)?
                .checked_add(self.right)?,
            height: client
                .height
                .checked_add(self.top)?
                .checked_add(self.bottom)?,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Fit {
    pub(super) content_ui: Size,
    pub(super) minimum_ui: Option<Size>,
    pub(super) origin_native: Point,
}

fn cardinal_layout(type_matches: bool, format: i32, remaining: c_ulong, count: usize) -> bool {
    type_matches && format == 32 && remaining == 0 && (1..=PROPERTY_LIMIT).contains(&count)
}

fn decode_cardinals(values: &[c_ulong]) -> Vec<u32> {
    // XGetWindowProperty format32 stores each wire value in a native C long,
    // which is eight bytes on LP64. Reading u32 slices would lose every other
    // value. Only the low protocol 32 bits carry the CARDINAL's value.
    values.iter().map(|value| *value as u32).collect()
}

fn desktop_work_area(desktop: &[u32], areas: &[u32]) -> Option<Rect> {
    let [desktop] = desktop else {
        return None;
    };
    if !areas.len().is_multiple_of(4) {
        return None;
    }
    let start = usize::try_from(*desktop).ok()?.checked_mul(4)?;
    let [x, y, width, height] = areas.get(start..start.checked_add(4)?)? else {
        return None;
    };
    let work = Rect {
        x: (*x).try_into().ok()?,
        y: (*y).try_into().ok()?,
        width: (*width).try_into().ok()?,
        height: (*height).try_into().ok()?,
    };
    work.valid().then_some(work)
}

fn selected_monitor(frame: Rect, monitors: &[Rect]) -> Option<Rect> {
    let mut selected = None;
    let mut largest_overlap = -1_i64;
    let mut shortest_distance = f64::INFINITY;
    let center = (
        f64::from(frame.x) + f64::from(frame.width) / 2.0,
        f64::from(frame.y) + f64::from(frame.height) / 2.0,
    );
    for &monitor in monitors.iter().filter(|monitor| monitor.valid()) {
        let overlap = frame.intersect(monitor).map_or(0, |overlap| {
            i64::from(overlap.width) * i64::from(overlap.height)
        });
        let near = (
            center.0.clamp(f64::from(monitor.x), monitor.right() as f64),
            center
                .1
                .clamp(f64::from(monitor.y), monitor.bottom() as f64),
        );
        let distance = (center.0 - near.0).powi(2) + (center.1 - near.1).powi(2);
        if overlap > largest_overlap
            || (overlap == 0 && largest_overlap == 0 && distance < shortest_distance)
        {
            selected = Some(monitor);
            largest_overlap = overlap;
            shortest_distance = distance;
        }
    }
    selected
}

fn fit_geometry(
    client: Rect,
    work: Rect,
    extents: Extents,
    wanted_native: Size,
    minimum_native: Option<Size>,
    display_scale: f32,
    scale: UiScale,
) -> Option<Fit> {
    if !client.valid() || !work.valid() || !display_scale.is_finite() || display_scale <= 0.0 {
        return None;
    }
    let current = extents.frame(client)?;
    let chrome = (
        extents.left.checked_add(extents.right)?,
        extents.top.checked_add(extents.bottom)?,
    );
    let available = (
        work.width.checked_sub(chrome.0)?,
        work.height.checked_sub(chrome.1)?,
    );
    if available.0 <= 0 || available.1 <= 0 {
        return None;
    }
    let physical_per_native = f64::from(display_scale);
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
    let minimum = minimum_native
        .filter(|size| super::valid_size(*size))
        .map(bounded_pixels);
    let wanted = if super::valid_size(wanted_native) {
        bounded_pixels(wanted_native)
    } else {
        minimum.unwrap_or((
            client.width.min(available.0),
            client.height.min(available.1),
        ))
    };
    let content = (
        wanted.0.max(minimum.map_or(1, |minimum| minimum.0)),
        wanted.1.max(minimum.map_or(1, |minimum| minimum.1)),
    );
    let width = content.0 + chrome.0;
    let height = content.1 + chrome.1;
    let x = (i64::from(current.x) + i64::from(current.width - width) / 2)
        .clamp(i64::from(work.x), work.right() - i64::from(width));
    let y = (i64::from(current.y) + i64::from(current.height - height) / 2)
        .clamp(i64::from(work.y), work.bottom() - i64::from(height));
    let physical_per_ui = physical_per_native * f64::from(scale.factor());
    let ui_size = |pixels: (i32, i32)| {
        Size::new(
            (f64::from(pixels.0) / physical_per_ui) as f32,
            (f64::from(pixels.1) / physical_per_ui) as f32,
        )
    };
    Some(Fit {
        content_ui: ui_size(content),
        minimum_ui: minimum.map(ui_size),
        origin_native: Point::new(
            (x as f64 / physical_per_native) as f32,
            (y as f64 / physical_per_native) as f32,
        ),
    })
}

#[cfg(target_os = "linux")]
mod native {
    use super::*;
    use iced::window::raw_window_handle::{RawDisplayHandle, RawWindowHandle};
    use std::{cell::OnceCell, ffi::CStr};
    use x11_dl::{xinerama, xlib, xrandr};

    thread_local! {
        static XLIB: OnceCell<Option<xlib::Xlib>> = const { OnceCell::new() };
        static RANDR: OnceCell<Option<xrandr::Xrandr>> = const { OnceCell::new() };
        static XINERAMA: OnceCell<Option<xinerama::Xlib>> = const { OnceCell::new() };
    }

    enum Property {
        Values(Vec<u32>),
        Missing,
        Invalid,
    }

    pub(super) fn query(
        handle: &dyn iced::window::Window,
        wanted_native: Size,
        minimum_native: Option<Size>,
        display_scale: f32,
        scale: UiScale,
    ) -> Query {
        let Ok(window) = handle.window_handle() else {
            return Query::Unavailable;
        };
        let RawWindowHandle::Xlib(window) = window.as_raw() else {
            return Query::Unavailable;
        };
        let Ok(display) = handle.display_handle() else {
            return Query::Unavailable;
        };
        let RawDisplayHandle::Xlib(display) = display.as_raw() else {
            return Query::Unavailable;
        };
        let Some(pointer) = display.display else {
            return Query::Unavailable;
        };
        XLIB.with(|library| {
            let Some(library) = library.get_or_init(|| xlib::Xlib::open().ok()).as_ref() else {
                return Query::Unavailable;
            };
            // SAFETY: window::run lends the live Xlib connection and window on
            // its UI thread. Property/monitor buffers are copied then freed;
            // the application's connection is never owned or closed here.
            unsafe {
                let pointer = pointer.as_ptr().cast::<xlib::Display>();
                let root = (library.XRootWindow)(pointer, display.screen);
                let Property::Values(desktop) =
                    property(library, pointer, root, c"_NET_CURRENT_DESKTOP")
                else {
                    return Query::Unavailable;
                };
                let Property::Values(areas) = property(library, pointer, root, c"_NET_WORKAREA")
                else {
                    return Query::Unavailable;
                };
                let Some(work) = desktop_work_area(&desktop, &areas) else {
                    return Query::Unavailable;
                };
                let extents = match property(library, pointer, window.window, c"_NET_FRAME_EXTENTS")
                {
                    Property::Values(values) => Extents::from_property(&values),
                    Property::Missing => return Query::FramePending,
                    Property::Invalid => return Query::Unavailable,
                };
                let Some(extents) = extents else {
                    return Query::Unavailable;
                };
                let mut attributes = std::mem::MaybeUninit::uninit();
                let (mut x, mut y, mut child) = (0, 0, 0);
                if (library.XGetWindowAttributes)(pointer, window.window, attributes.as_mut_ptr())
                    == 0
                    || (library.XTranslateCoordinates)(
                        pointer,
                        window.window,
                        root,
                        0,
                        0,
                        &mut x,
                        &mut y,
                        &mut child,
                    ) == 0
                {
                    return Query::Unavailable;
                }
                let attributes = attributes.assume_init();
                let client = Rect {
                    x,
                    y,
                    width: attributes.width,
                    height: attributes.height,
                };
                let Some(frame) = extents.frame(client).filter(|frame| frame.valid()) else {
                    return Query::Unavailable;
                };
                let Some(monitors) = monitors(library, pointer, root) else {
                    return Query::Unavailable;
                };
                let Some(work) =
                    selected_monitor(frame, &monitors).and_then(|monitor| work.intersect(monitor))
                else {
                    return Query::Unavailable;
                };
                fit_geometry(
                    client,
                    work,
                    extents,
                    wanted_native,
                    minimum_native,
                    display_scale,
                    scale,
                )
                .map_or(Query::Unavailable, Query::Fit)
            }
        })
    }

    unsafe fn property(
        library: &xlib::Xlib,
        display: *mut xlib::Display,
        window: xlib::Window,
        name: &CStr,
    ) -> Property {
        // SAFETY: caller holds a live UI-thread display/window, and this name
        // is a null-terminated static C string. No missing atom is created.
        unsafe {
            let atom = (library.XInternAtom)(display, name.as_ptr(), xlib::True);
            if atom == 0 {
                return Property::Missing;
            }
            let (mut actual_type, mut format, mut count, mut remaining) = (0, 0, 0, 0);
            let mut data = std::ptr::null_mut();
            let status = (library.XGetWindowProperty)(
                display,
                window,
                atom,
                0,
                PROPERTY_LIMIT as _,
                xlib::False,
                xlib::XA_CARDINAL,
                &mut actual_type,
                &mut format,
                &mut count,
                &mut remaining,
                &mut data,
            );
            let reply = if status != i32::from(xlib::Success) {
                Property::Invalid
            } else if actual_type == 0 {
                Property::Missing
            } else if !data.is_null()
                && cardinal_layout(
                    actual_type == xlib::XA_CARDINAL,
                    format,
                    remaining,
                    count as usize,
                )
            {
                Property::Values(decode_cardinals(std::slice::from_raw_parts(
                    data.cast::<c_ulong>(),
                    count as usize,
                )))
            } else {
                Property::Invalid
            };
            if !data.is_null() {
                (library.XFree)(data.cast());
            }
            reply
        }
    }

    unsafe fn monitors(
        library: &xlib::Xlib,
        display: *mut xlib::Display,
        root: xlib::Window,
    ) -> Option<Vec<Rect>> {
        let randr = RANDR.with(|library| {
            let library = library
                .get_or_init(|| xrandr::Xrandr::open().ok())
                .as_ref()?;
            // SAFETY: the caller borrows the live UI-thread display. Version
            // gating avoids using RandR 1.5 monitor requests on older servers.
            unsafe {
                let (mut major, mut minor) = (0, 0);
                if (library.XRRQueryVersion)(display, &mut major, &mut minor) == 0
                    || (major, minor) < (1, 5)
                {
                    return None;
                }
                let mut count = 0;
                let data = (library.XRRGetMonitors)(display, root, xlib::True, &mut count);
                if data.is_null() {
                    return None;
                }
                let rectangles = (1..=256).contains(&count).then(|| {
                    std::slice::from_raw_parts(data, count as usize)
                        .iter()
                        .map(|monitor| Rect {
                            x: monitor.x,
                            y: monitor.y,
                            width: monitor.width,
                            height: monitor.height,
                        })
                        .filter(|rect| rect.valid())
                        .collect::<Vec<_>>()
                });
                (library.XRRFreeMonitors)(data);
                rectangles.filter(|rectangles| !rectangles.is_empty())
            }
        });
        randr.or_else(|| {
            XINERAMA.with(|xinerama| {
                let xinerama = xinerama
                    .get_or_init(|| xinerama::Xlib::open().ok())
                    .as_ref()?;
                // SAFETY: Xinerama monitor data belongs to Xlib and is freed after
                // copying. No display or screen configuration is changed.
                unsafe {
                    if root != (library.XDefaultRootWindow)(display)
                        || (xinerama.XineramaIsActive)(display) == 0
                    {
                        return None;
                    }
                    let mut count = 0;
                    let data = (xinerama.XineramaQueryScreens)(display, &mut count);
                    if data.is_null() {
                        return None;
                    }
                    let rectangles = (1..=256).contains(&count).then(|| {
                        std::slice::from_raw_parts(data, count as usize)
                            .iter()
                            .map(|monitor| Rect {
                                x: i32::from(monitor.x_org),
                                y: i32::from(monitor.y_org),
                                width: i32::from(monitor.width),
                                height: i32::from(monitor.height),
                            })
                            .filter(|rect| rect.valid())
                            .collect::<Vec<_>>()
                    });
                    (library.XFree)(data.cast());
                    rectangles.filter(|rectangles| !rectangles.is_empty())
                }
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn extents() -> Extents {
        Extents {
            left: 4,
            right: 4,
            top: 28,
            bottom: 4,
        }
    }

    fn fitted_frame(fit: Fit, display_scale: f32, scale: UiScale) -> Rect {
        let factor = display_scale * scale.factor();
        let chrome = extents();
        rect(
            (fit.origin_native.x * display_scale).round() as i32,
            (fit.origin_native.y * display_scale).round() as i32,
            (fit.content_ui.width * factor).round() as i32 + chrome.left + chrome.right,
            (fit.content_ui.height * factor).round() as i32 + chrome.top + chrome.bottom,
        )
    }

    #[test]
    fn property_shape_and_native_long_stride_are_checked() {
        assert!(cardinal_layout(true, 32, 0, 8));
        for (matches, format, remaining, count) in [
            (false, 32, 0, 8),
            (true, 16, 0, 8),
            (true, 32, 4, 8),
            (true, 32, 0, 0),
            (true, 32, 0, PROPERTY_LIMIT + 1),
        ] {
            assert!(!cardinal_layout(matches, format, remaining, count));
        }
        let values: [c_ulong; 8] = [0, 24, 1920, 1056, 80, 32, 1840, 1048];
        let decoded = decode_cardinals(&values);
        assert_eq!(
            desktop_work_area(&[1], &decoded),
            Some(rect(80, 32, 1840, 1048))
        );
        for (desktop, areas) in [
            (&[2][..], &decoded[..]),
            (&[0, 1][..], &decoded[..]),
            (&[0][..], &decoded[..7]),
        ] {
            assert_eq!(desktop_work_area(desktop, areas), None);
        }
        assert_eq!(desktop_work_area(&[0], &[0, 0, u32::MAX, 500]), None);
        assert_eq!(Extents::from_property(&[4, 4, 28]), None);
        assert_eq!(Extents::from_property(&[4, 4, u32::MAX, 4]), None);
    }

    #[test]
    fn current_desktop_work_area_is_intersected_with_the_selected_monitor() {
        let areas = [0, 0, 3840, 1080, 80, 24, 3600, 1032];
        let work = desktop_work_area(&[1], &areas).unwrap();
        let frame = rect(2300, 100, 1400, 900);
        let monitors = [rect(0, 0, 1920, 1080), rect(1920, 0, 1920, 1080)];
        let monitor = selected_monitor(frame, &monitors).unwrap();
        assert_eq!(monitor, monitors[1]);
        assert_eq!(work.intersect(monitor), Some(rect(1920, 24, 1760, 1032)));
        assert_eq!(
            selected_monitor(rect(-1000, 0, 500, 400), &monitors),
            Some(monitors[0])
        );
        assert_eq!(selected_monitor(frame, &[]), None);
        assert_eq!(work.intersect(rect(-1000, 0, 500, 400)), None);
        // Prefer actual overlap when an offscreen window's center happens to
        // lie closer to a disconnected monitor rectangle.
        let monitors = [rect(0, 0, 100, 100), rect(-500, 0, 20, 20)];
        assert_eq!(
            selected_monitor(rect(-1000, 40, 1050, 20), &monitors),
            Some(monitors[0])
        );
    }

    #[test]
    fn side_and_top_panels_fit_the_whole_frame_including_native_title_bar() {
        for work in [rect(160, 0, 1760, 1080), rect(0, 120, 1920, 960)] {
            let fit = fit_geometry(
                rect(4, 28, 1920, 1080),
                work,
                extents(),
                Size::new(1920.0, 1080.0),
                Some(super::super::MIN_WINDOW),
                1.0,
                UiScale::Standard,
            )
            .unwrap();
            assert_eq!(fitted_frame(fit, 1.0, UiScale::Standard), work);
        }
    }

    #[test]
    fn high_dpi_small_work_area_lowers_minimum_in_iced_units_at_every_interface_size() {
        let work = rect(1920, 40, 900, 500);
        for scale in UiScale::ALL {
            let fit = fit_geometry(
                rect(1804, -100, 2560, 1680),
                work,
                extents(),
                Size::new(1280.0, 840.0),
                Some(super::super::MIN_WINDOW),
                2.0,
                scale,
            )
            .unwrap();
            assert_eq!(fitted_frame(fit, 2.0, scale), work);
            let minimum = fit.minimum_ui.unwrap() * (2.0 * scale.factor());
            assert_eq!(minimum.width.round(), 892.0);
            assert_eq!(minimum.height.round(), 468.0);
        }
    }

    #[test]
    fn auxiliary_size_applies_interface_and_display_scaling_once_and_preserves_center() {
        let work = rect(1920, 24, 1920, 1056);
        for scale in UiScale::ALL {
            let wanted = Size::new(520.0, 500.0) * scale.factor();
            let client = rect(2504, 300, 780, 750);
            let fit = fit_geometry(client, work, extents(), wanted, None, 1.5, scale).unwrap();
            let frame = fitted_frame(fit, 1.5, scale);
            assert_eq!(frame.width - 8, (wanted.width * 1.5).round() as i32);
            assert_eq!(frame.height - 32, (wanted.height * 1.5).round() as i32);
            assert_eq!(fit.minimum_ui, None);
            assert!(frame.x >= work.x && frame.y >= work.y);
            assert!(frame.right() <= work.right() && frame.bottom() <= work.bottom());
            let again = fit_geometry(
                rect(
                    frame.x + 4,
                    frame.y + 28,
                    frame.width - 8,
                    frame.height - 32,
                ),
                work,
                extents(),
                wanted,
                None,
                1.5,
                scale,
            )
            .unwrap();
            assert_eq!(fitted_frame(again, 1.5, scale), frame);
        }
    }
}
