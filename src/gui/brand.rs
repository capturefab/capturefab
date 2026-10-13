//! Application identity, exported from assets/icons/capturefab.svg.
use iced::{Size, widget::image, window};
use std::{io::Cursor, sync::OnceLock};

struct Pixels {
    rgba: Vec<u8>,
    size: Size<u32>,
}

fn pixels() -> &'static Pixels {
    static PIXELS: OnceLock<Pixels> = OnceLock::new();
    PIXELS.get_or_init(|| {
        let mut reader = png::Decoder::new(Cursor::new(include_bytes!(
            "../../assets/icons/capturefab.png"
        )))
        .read_info()
        .expect("embedded application icon is a valid PNG");
        let mut rgba = vec![0; reader.output_buffer_size()];
        let info = reader
            .next_frame(&mut rgba)
            .expect("embedded application icon decodes");
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(info.bit_depth, png::BitDepth::Eight);
        rgba.truncate(info.buffer_size());
        Pixels {
            rgba,
            size: Size::new(info.width, info.height),
        }
    })
}

pub fn mark() -> image::Handle {
    static MARK: OnceLock<image::Handle> = OnceLock::new();
    MARK.get_or_init(|| {
        let pixels = pixels();
        image::Handle::from_rgba(pixels.size.width, pixels.size.height, pixels.rgba.clone())
    })
    .clone()
}

pub fn window_icon() -> window::Icon {
    let pixels = pixels();
    window::icon::from_rgba(pixels.rgba.clone(), pixels.size.width, pixels.size.height)
        .expect("embedded application icon has valid RGBA dimensions")
}

/// macOS ignores window icons. Set the application icon as well so binaries
/// launched from a terminal have the same Dock icon as the packaged .app.
#[cfg(target_os = "macos")]
pub fn set_dock_icon() {
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let mtm = MainThreadMarker::new().expect("the GUI starts on the main thread");
    let data = NSData::with_bytes(include_bytes!("../../assets/icons/capturefab.icns"));
    let image = NSImage::initWithData(NSImage::alloc(), &data)
        .expect("embedded application icon is a valid ICNS");
    // SAFETY: AppKit is called on the main thread with a non-nil image.
    unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
}
