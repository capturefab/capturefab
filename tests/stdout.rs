//! `capture -o -`: the image streams from the camera's worker, through a
//! pipe for a worker of the command or a private file for a session, at any
//! size, and a reader that goes away ends the command quietly.
mod support;

use serde_json::Value;
use std::{io::Read, process::Stdio};
use support::cli::Home;

/// A frame far larger than a pipe holds streams whole.
#[test]
fn a_large_frame_streams_whole_from_a_worker_of_the_command() {
    let home = Home::new("stdout-large");
    let run = home.run(&[
        "--json",
        "capture",
        "-c",
        "sim:0",
        "-o",
        "-",
        "-f",
        "raw",
        "--set",
        "Width=1920",
        "--set",
        "Height=1080",
        "--set",
        "PixelFormat=RGB8",
    ]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.raw.len(), 1920 * 1080 * 3);
    let result: Value = serde_json::from_str(run.stderr.trim()).unwrap();
    assert_eq!(result["bytes"], 1920 * 1080 * 3);
    assert_eq!(result["width"], 1920);
    assert_eq!(result["pixel_format"], "RGB8");
}

/// A session that holds the camera writes the image to a private file the
/// command streams and deletes.
#[test]
fn a_session_holding_the_camera_streams_through_a_private_file() {
    let home = Home::new("stdout-session");
    let _session = home.serve("held", &["sim:0"]);
    home.ok(&["set", "-c", "sim:0", "Width=320", "Height=240"]);
    let run = home.run(&["capture", "-c", "sim:0", "-o", "-", "-f", "raw"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    // The session's camera, with its settings.
    assert_eq!(run.raw.len(), 320 * 240);
    let out = home.sessions.join("out");
    let left = std::fs::read_dir(&out)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(left, 0, "the private file is deleted");
    // A typed --session works too, and a PNG decodes.
    let run = home.run(&["--session", "held", "capture", "-o", "-"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let decoded = png::Decoder::new(std::io::Cursor::new(run.raw))
        .read_info()
        .unwrap();
    assert_eq!(decoded.info().width, 320);
}

/// A reader that stops reading, as `| head -c` does, ends the capture
/// quietly with exit 0.
#[test]
fn a_closed_stdout_ends_the_capture_quietly() {
    let home = Home::new("stdout-epipe");
    let mut child = home
        .command()
        .args([
            "capture",
            "-c",
            "sim:0",
            "-o",
            "-",
            "-f",
            "raw",
            "--set",
            "Width=1920",
            "--set",
            "Height=1080",
            "--set",
            "PixelFormat=RGB8",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut head = [0u8; 100];
    child.stdout.take().unwrap().read_exact(&mut head).unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
