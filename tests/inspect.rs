//! `inspect` and `auto` on the simulator: a short document an agent can
//! act on, a private bounded preview, and exposure settled then held.
mod support;

use serde_json::json;
use std::path::Path;
use support::cli::Home;

#[test]
fn inspect_measures_a_fresh_frame_and_leaves_a_private_preview() {
    let home = Home::new("inspect");
    let measured = home.ok(&["inspect", "-c", "sim:0"]);
    assert_eq!(measured["camera"], "sim:0");
    assert_eq!(measured["frame"]["width"], 640);
    assert_eq!(measured["frame"]["pixel_format"], "Mono8");
    assert_eq!(measured["frame"]["bits"], 8);
    for key in ["mean", "p1", "p50", "p99", "clipped_low", "clipped_high"] {
        let share = measured["exposure"][key].as_f64().unwrap();
        assert!((0.0..=1.0).contains(&share), "{key}: {measured}");
    }
    assert_eq!(measured["verdict"], json!(["ok"]));
    assert!(measured["focus"]["sharpness"].as_f64().unwrap() > 0.0);
    assert_eq!(measured["settings"]["exposure_us"], 10000.0);
    assert!(measured.get("histogram").is_none(), "-v only: {measured}");
    let preview = Path::new(measured["preview"].as_str().unwrap());
    assert!(preview.starts_with(&home.sessions), "{preview:?}");
    assert!(preview.starts_with(home.sessions.join("inspect")));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(preview), 0o600);
        assert_eq!(mode(preview.parent().unwrap()), 0o700);
    }
    // doctor names the folder, so a person can allow reading it once.
    let doctor = home.ok(&["doctor"]);
    assert_eq!(doctor["dirs"]["inspect"], json!(preview.parent().unwrap()));
    // No captures are saved.
    assert_eq!(std::fs::read_dir(&home.work).unwrap().count(), 0);
    // A preview where it was asked for, never over a file.
    let named = home.ok(&[
        "inspect",
        "-c",
        "sim:0",
        "--preview",
        "look.png",
        "--size",
        "128",
    ]);
    assert_eq!(named["preview"], json!(home.real("look.png")));
    let decoded = png::Decoder::new(std::fs::File::open(home.path("look.png")).unwrap())
        .read_info()
        .unwrap();
    assert_eq!(decoded.info().width, 128);
    let taken = home.fails(1, &["inspect", "-c", "sim:0", "--preview", "look.png"]);
    assert_eq!(taken["code"], "exists");
    let none = home.ok(&["inspect", "-c", "sim:0", "--preview", "none"]);
    assert!(none.get("preview").is_none());
    // -v adds the histogram, channels and every focus metric.
    let verbose = home.ok(&["-v", "inspect", "-c", "sim:0", "--preview", "none"]);
    assert_eq!(verbose["histogram"].as_array().unwrap().len(), 16);
    assert!(verbose["focus"]["tenengrad"].is_number());
}

#[test]
fn inspect_applies_settings_first_and_suggests_an_exposure() {
    let home = Home::new("inspect-set");
    let dark = home.ok(&[
        "inspect",
        "-c",
        "sim:0",
        "--set",
        "exposure=1ms",
        "--preview",
        "none",
    ]);
    assert_eq!(dark["settings"]["exposure_us"], 1000.0);
    assert!(
        dark["verdict"].as_array().unwrap().contains(&json!("dark")),
        "{dark}"
    );
    let suggest = &dark["suggest"];
    assert!(suggest["ev"].as_f64().unwrap() > 1.0, "{dark}");
    let exposure = suggest["exposure_us"].as_f64().unwrap();
    assert!(exposure > 2000.0, "{dark}");
    // The suggestion works: the frame at it is fine.
    let fixed = home.ok(&[
        "inspect",
        "-c",
        "sim:0",
        "--set",
        &format!("ExposureTime={exposure}"),
        "--preview",
        "none",
    ]);
    assert_eq!(fixed["verdict"], json!(["ok"]), "{fixed}");
    let bright = home.ok(&[
        "inspect",
        "-c",
        "sim:0",
        "--set",
        "exposure=60ms",
        "--preview",
        "none",
    ]);
    let verdict = bright["verdict"].as_array().unwrap();
    assert!(verdict.contains(&json!("clipped_high")), "{bright}");
    assert!(bright["suggest"]["ev"].as_f64().unwrap() < 0.0);
    // A region is measured alone.
    let region = home.ok(&[
        "-v",
        "inspect",
        "-c",
        "sim:0",
        "--region",
        "0.25,0.25,0.5,0.5",
        "--preview",
        "none",
    ]);
    assert_eq!(region["region"]["width"], 320);
    home.fails(
        2,
        &["inspect", "-c", "sim:0", "--region", "0.5,0.5,0.6,0.1"],
    );
}

#[test]
fn auto_settles_then_holds_or_turns_off() {
    let home = Home::new("auto");
    let _session = home.serve("auto", &["sim:0"]);
    home.ok(&["set", "-c", "sim:0", "exposure=1ms"]);
    let settled = home.ok(&["auto", "-c", "sim:0", "--timeout", "10s"]);
    assert_eq!(settled["camera"], "sim:0");
    assert_eq!(settled["settled"], true, "{settled}");
    assert!(settled["iterations"].as_u64().unwrap() >= 1, "{settled}");
    assert!(
        settled["exposure_us"].as_f64().unwrap() > 1000.0,
        "{settled}"
    );
    let mean = settled["mean"].as_f64().unwrap();
    assert!((0.3..0.6).contains(&mean), "{settled}");
    // Held: the camera keeps that exposure and is idle again.
    let held = home.ok(&["get", "-c", "sim:0", "ExposureTime"])["values"]["ExposureTime"].clone();
    assert_eq!(held, settled["exposure_us"]);
    let status = home.ok(&["session", "status"]);
    assert_eq!(status["cameras"][0]["state"], "idle");
    let off = home.ok(&["auto", "-c", "sim:0", "--off"]);
    assert_eq!(off["mode"], "off");
    assert_eq!(
        home.ok(&["get", "-c", "sim:0", "ExposureAuto"])["values"]["ExposureAuto"],
        "Off"
    );
    // The simulator has no continuous auto exposure; Capturefab's runs in
    // the session.
    let continuous = home.ok(&["auto", "-c", "sim:0", "--continuous"]);
    assert_eq!(continuous["by"], "capturefab");
    let direct = home.fails(2, &["--no-session", "auto", "-c", "sim:9", "--continuous"]);
    assert_eq!(direct["code"], "usage");
}
