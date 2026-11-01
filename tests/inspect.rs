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

/// --fresh lets frames pass on a stopped camera too: each direct
/// inspection starts the simulator from its first frame.
#[test]
fn fresh_frames_pass_before_the_one_measured() {
    let home = Home::new("inspect-fresh");
    let measure = |fresh: &str| {
        let measured = home.ok(&[
            "inspect",
            "-c",
            "sim:0",
            "--fresh",
            fresh,
            "--preview",
            "none",
        ]);
        (measured["exposure"].clone(), measured["focus"].clone())
    };
    assert_eq!(measure("1"), measure("1"));
    assert_eq!(measure("3"), measure("3"));
    assert_ne!(measure("1"), measure("3"));
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
    // The camera's own continuous auto exposure runs, direct too, until
    // turned off.
    let continuous = home.ok(&["auto", "-c", "sim:0", "--continuous"]);
    assert_eq!(continuous["by"], "camera");
    assert_eq!(
        home.ok(&["get", "-c", "sim:0", "ExposureAuto"])["values"]["ExposureAuto"],
        "Continuous"
    );
    home.ok(&["auto", "-c", "sim:0", "--off"]);
    let direct = home.ok(&["--no-session", "auto", "-c", "sim:9", "--continuous"]);
    assert_eq!(direct["by"], "camera");
}

#[test]
fn clipping_is_counted_over_every_pixel_at_the_cameras_bit_depth() {
    let home = Home::new("clipping");
    // Mid-grey but for the marker lines, which clip at Mono12's 4095.
    home.ok(&[
        "set",
        "-c",
        "sim:0",
        "PixelFormat=Mono12",
        "ExposureTime=10000",
        "Gain=1",
    ]);
    let whole = home.ok(&["inspect", "-c", "sim:0", "--preview", "none"]);
    let exposure = &whole["exposure"];
    let clipped = exposure["clipped_high_px"].as_u64().unwrap();
    // Every pixel of the 640-pixel marker row and the moving column, and a
    // few of the brightest of the scene.
    assert!((640..5000).contains(&clipped), "{whole}");
    assert_eq!(exposure["max"], 1.0, "{whole}");
    // ...but any clipped pixel inside a region counts, unless a threshold
    // allows it.
    let marked = home.ok(&[
        "inspect",
        "-c",
        "sim:0",
        "--preview",
        "none",
        "--region",
        "0.4,0.4,0.2,0.2",
    ]);
    assert!(
        marked["verdict"]
            .as_array()
            .unwrap()
            .contains(&json!("clipped_high")),
        "{marked}"
    );
    let allowed = home.ok(&[
        "inspect",
        "-c",
        "sim:0",
        "--preview",
        "none",
        "--region",
        "0.4,0.4,0.2,0.2",
        "--clip-threshold",
        "0.5",
    ]);
    assert!(
        !allowed["verdict"]
            .as_array()
            .unwrap()
            .contains(&json!("clipped_high")),
        "{allowed}"
    );
}

#[test]
fn a_series_samples_focus_and_keeps_the_sharpest_frame() {
    let home = Home::new("series");
    let series = home.ok(&[
        "inspect",
        "-c",
        "sim:microscope",
        "--for",
        "600ms",
        "--every",
        "200ms",
        "--preview",
        "best.jpg",
    ]);
    let samples = series["series"].as_array().unwrap();
    assert_eq!(samples.len(), 4, "{series}");
    let sharpest = samples
        .iter()
        .map(|sample| sample["sharpness"].as_f64().unwrap())
        .fold(0.0, f64::max);
    assert_eq!(series["best"]["sharpness"].as_f64(), Some(sharpest));
    assert_eq!(series["focus"]["sharpness"].as_f64(), Some(sharpest));
    assert!(home.path("best.jpg").is_file());
    assert_eq!(
        home.fails(
            2,
            &["inspect", "-c", "sim:0", "--for", "1m", "--every", "100ms"]
        )["code"],
        "usage"
    );
    // A capture measures its last frame the same way.
    let shot = home.ok(&[
        "capture",
        "-c",
        "sim:0",
        "-n",
        "2",
        "--inspect",
        "-o",
        "shots/",
    ]);
    assert_eq!(shot["inspect"]["verdict"], json!(["ok"]), "{shot}");
    assert!(Path::new(shot["inspect"]["preview"].as_str().unwrap()).is_file());
}

#[test]
fn auto_meters_a_region_and_keeps_exposure_short_when_asked() {
    let home = Home::new("auto-limits");
    let _session = home.serve("limits", &["sim:0"]);
    home.ok(&["set", "-c", "sim:0", "exposure=1ms"]);
    // Capturefab's auto mode keeps to the limits; gain makes up the rest.
    let capped = home.ok(&[
        "auto",
        "-c",
        "sim:0",
        "--max-exposure",
        "3ms",
        "--region",
        "0.25,0.25,0.5,0.5",
        "--timeout",
        "10s",
    ]);
    assert_eq!(capped["by"], "capturefab", "{capped}");
    assert!(
        capped["exposure_us"].as_f64().unwrap() <= 3000.0,
        "{capped}"
    );
    assert!(capped["gain_db"].as_f64().unwrap() > 0.0, "{capped}");
    assert_eq!(capped["settled"], true, "{capped}");
    // Without limits the camera's own auto exposure runs once.
    home.ok(&["set", "-c", "sim:0", "exposure=1ms", "Gain=0"]);
    let once = home.ok(&["auto", "-c", "sim:0", "--timeout", "10s"]);
    assert_eq!(once["by"], "camera", "{once}");
    assert_eq!(once["settled"], true, "{once}");
    assert!(once["exposure_us"].as_f64().unwrap() > 3000.0, "{once}");
    assert_eq!(
        home.ok(&["get", "-c", "sim:0", "ExposureAuto"])["values"]["ExposureAuto"],
        "Off"
    );
}
