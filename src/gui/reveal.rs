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

/// Open a release link in the default browser: only links the release list
/// may carry (see `updates::link_allowed`) and the downloads page. The child
/// runs without a shell and is reaped on a detached thread.
pub(super) fn open_link(url: &str) -> io::Result<()> {
    if url != crate::updates::DOWNLOADS_URL
        && !crate::updates::link_allowed(url, crate::updates::override_origin().as_deref())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "this link is not on capturefab.com or the Capturefab GitHub releases",
        ));
    }
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
