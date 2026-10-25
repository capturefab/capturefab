//! Show a saved file in the system file manager: Finder, Explorer, or the
//! Linux file manager's folder view.
use std::{io, path::Path, process::Command};

/// What the action is called on this system.
pub(super) fn label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Show in Finder"
    } else if cfg!(windows) {
        "Show in Explorer"
    } else {
        "Show in folder"
    }
}

/// Open the file manager on `path`, selecting it where the system can. The
/// child runs without a shell and is reaped on a detached thread.
pub(super) fn reveal(path: &Path) -> io::Result<()> {
    let path = std::path::absolute(path)?;
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg("-R").arg(&path);
        command
    };
    #[cfg(windows)]
    let mut command = {
        use std::os::windows::process::CommandExt;
        // Explorer parses its own command line; quoting keeps spaces.
        let mut command = Command::new("explorer.exe");
        command.raw_arg(format!("/select,\"{}\"", path.display()));
        command
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(path.parent().unwrap_or(&path));
        command
    };
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    // Explorer exits 1 even when it worked; the status is not checked.
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// What the button that opens the system's camera privacy settings says;
/// `None` where there is no such page to open (Linux grants access through
/// device groups, which the connect error explains).
pub(super) fn camera_privacy_label() -> Option<&'static str> {
    if cfg!(any(target_os = "macos", windows)) {
        Some("Open Camera Settings")
    } else {
        None
    }
}

/// Open the system's camera privacy settings: Privacy & Security > Camera
/// on macOS, Privacy & security > Camera on Windows. Like `reveal`, the
/// child runs without a shell and is reaped on a detached thread.
pub(super) fn open_camera_privacy() -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Camera");
        command
    };
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("explorer.exe");
        command.arg("ms-settings:privacy-webcam");
        command
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this system has no camera privacy settings page",
    ));
    #[cfg(any(target_os = "macos", windows))]
    {
        let mut child = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        std::thread::spawn(move || child.wait());
        Ok(())
    }
}

/// Open a release link in the default browser: only links the release list
/// may carry (see `updates::link_allowed`) and the downloads page.
pub(super) fn open_link(url: &str) -> io::Result<()> {
    if url != crate::updates::DOWNLOADS_URL
        && !crate::updates::link_allowed(url, crate::updates::override_origin().as_deref())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "this link is not on capturefab.com or the Capturefab GitHub releases",
        ));
    }
    browse(url)
}

/// Open `url`, an `https://` address, in the default browser.
pub(super) fn open_url(url: &str) -> io::Result<()> {
    if !url.starts_with("https://") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only https:// addresses open in the browser",
        ));
    }
    browse(url)
}

/// Hand a checked `url` to the default browser. Like `reveal`, the child
/// runs without a shell and is reaped on a detached thread.
fn browse(url: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler");
        command
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let mut command = Command::new("xdg-open");
    let mut child = command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}
