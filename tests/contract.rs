//! The command-line contract agents rely on: one JSON document per call,
//! exit codes from typed reasons, the name and duration grammars, atomic
//! `set`, and a schema that describes what commands really print.
mod support;

use serde_json::{Value, json};
use std::collections::BTreeSet;
use support::cli::Home;

/// Every `--json` call prints one line, a result exactly when it exits 0.
#[test]
fn every_json_command_prints_one_document() {
    let home = Home::new("one-document");
    let cases: &[(&[&str], i32)] = &[
        (&["list", "--kind", "sim"], 0),
        (&["info", "-c", "sim:0"], 0),
        (&["features", "-c", "sim:0"], 0),
        (&["get", "-c", "sim:0", "Width", "exposure"], 0),
        (&["set", "-c", "sim:0", "exposure=5ms", "--dry-run"], 0),
        (&["exec", "-c", "sim:0", "TriggerSoftware[FrameStart]"], 0),
        (&["preset", "list"], 0),
        (&["login", "list"], 0),
        (&["doctor"], 0),
        (&["licenses"], 0),
        (&["schema"], 0),
        (&["schema", "set"], 0),
        (&["get", "-c", "sim:0", "NoSuchFeature"], 2),
        (&["set", "-c", "sim:0", "Gain=99"], 2),
        (&["get", "-c", "sim0", "Width"], 3),
        (&["list", "--timeout", "5000"], 2),
        (&["frobnicate"], 2),
        (&["get", "-c", "sim:0"], 2),
        (&["preset", "show", "missing"], 3),
    ];
    for (args, exit) in cases {
        let run = home.run(&[&["--json"][..], args].concat());
        if *exit == 0 {
            run.success();
        } else {
            run.failure(*exit);
        }
        // Diagnostics never reach stdout, and in JSON mode stderr is quiet.
        assert!(run.stderr.trim().is_empty(), "{args:?}: {}", run.stderr);
    }
    // CAPTUREFAB_JSON turns it on too.
    let run = home.run_env(&[("CAPTUREFAB_JSON", "1")], &["list", "--kind", "sim"]);
    assert_eq!(run.success()["cameras"][0]["id"], "sim:0");
}

#[test]
fn argument_errors_are_usage_documents_and_help_stays_text() {
    let home = Home::new("clap");
    let error = home.fails(2, &["lsit"]);
    assert_eq!(error["code"], "usage");
    assert_eq!(error["message"], "unrecognized subcommand 'lsit'");
    assert_eq!(error["details"]["suggestions"][0], "list");
    assert_eq!(error["details"]["invalid"], "lsit");
    let error = home.fails(2, &["get"]);
    assert_eq!(error["details"]["kind"], "missing_required_argument");
    assert!(
        error["details"]["usage"]
            .as_str()
            .unwrap()
            .starts_with("capturefab get"),
        "{error}"
    );
    let error = home.fails(2, &["list", "--kind", "usb"]);
    assert!(error["details"]["valid_values"].as_array().unwrap().len() >= 5);
    // Missing pieces are named in the message itself.
    let error = home.fails(2, &["get", "-c", "sim:0"]);
    assert_eq!(error["message"], "missing required argument: <NAME>...");
    let error = home.fails(2, &["--session", "bench", "preset"]);
    assert_eq!(error["details"]["kind"], "missing_subcommand");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .starts_with("capturefab preset needs a subcommand: save, list, show, apply, rm"),
        "{error}"
    );
    // Removed global flags and commands are argument errors now.
    for args in [
        &["--camera", "sim:0", "info"][..],
        &["--direct", "list"],
        &["--timeout-ms", "100", "list"],
        &["discover"],
        &["version"],
        &["setup", "list"],
        &["network-scan"],
        &["rpc"],
    ] {
        home.fails(2, args);
    }
    // --help and --version print text even with --json.
    let help = home.run(&["--json", "set", "--help"]);
    assert_eq!(help.code, Some(0));
    assert!(help.stdout.contains("Examples:"), "{}", help.stdout);
    let version = home.run(&["--json", "--version"]);
    assert_eq!(version.code, Some(0));
    assert!(version.stdout.starts_with("capturefab "));
    // Bare: an overview for people, a usage error for agents.
    let bare = home.run(&[]);
    assert_eq!(bare.code, Some(0));
    assert!(bare.stdout.contains("capturefab schema"), "{}", bare.stdout);
    assert_eq!(home.fails(2, &[])["code"], "usage");
    assert!(!home.sessions.exists(), "argument errors created state");
}

#[test]
fn durations_need_units() {
    let home = Home::new("durations");
    let error = home.fails(2, &["list", "--timeout", "5000"]);
    assert_eq!(error["code"], "invalid_duration");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("did you mean 5000ms or 5000s?"),
        "{error}"
    );
    for good in ["250ms", "0", "1.5s"] {
        home.ok(&["list", "--kind", "sim", "--timeout", good]);
    }
    for bad in ["5", "1.5", "5x", "1s5"] {
        assert_eq!(
            home.fails(2, &["list", "--timeout", bad])["code"],
            "invalid_duration",
            "{bad}"
        );
    }
    // Time-unit features take a duration as well.
    let set = home.ok(&["set", "-c", "sim:0", "ExposureTime=2.5ms", "--dry-run"]);
    assert_eq!(set["plan"][0]["to"], 2500);
    assert_eq!(
        home.fails(2, &["set", "-c", "sim:0", "exposure=5000"])["code"],
        "invalid_duration"
    );
}

#[test]
fn list_shows_the_simulator_only_when_asked() {
    let home = Home::new("list");
    let sim = home.ok(&["list", "--kind", "sim"]);
    assert_eq!(
        sim["cameras"],
        json!([{"id":"sim:0","kind":"sim","vendor":"Capturefab","model":"Pattern camera","serial":"SIM0"}])
    );
    let gige = home.ok(&["list", "--kind", "gige", "--timeout", "100ms"]);
    assert!(
        gige["cameras"]
            .as_array()
            .unwrap()
            .iter()
            .all(|camera| camera["kind"] == "gige"),
        "{gige}"
    );
    let text = home.run(&["list", "--kind", "sim"]);
    assert_eq!(
        text.stdout,
        "ID     KIND  MODEL           SERIAL\nsim:0  sim   Pattern camera  SIM0\n"
    );
    let empty = home.run(&["list", "--kind", "gige", "--timeout", "0"]);
    if empty.stdout.starts_with("No cameras") {
        assert_eq!(empty.stdout.lines().count(), 1, "{}", empty.stdout);
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
#[test]
fn list_reports_a_failed_transport_without_hiding_other_cameras() {
    let home = Home::new("list-failure");
    let missing = home.path("missing-ffmpeg");
    let run = home.run_env(
        &[("CAPTUREFAB_FFMPEG", missing.to_str().unwrap())],
        &["--json", "list", "--kind", "webcam,sim"],
    );
    let list = run.success();
    assert_eq!(list["cameras"][0]["id"], "sim:0", "{list}");
    assert!(
        list["warnings"].as_array().unwrap().iter().any(|warning| {
            warning
                .as_str()
                .unwrap()
                .contains("Native camera discovery")
        }),
        "{list}"
    );
}

#[test]
fn info_and_features_describe_the_camera() {
    let home = Home::new("info");
    let info = home.ok(&["info", "-c", "sim:0"]);
    assert_eq!(info["camera"], "sim:0");
    assert_eq!(info["kind"], "sim");
    assert_eq!(
        (info["width"].clone(), info["height"].clone()),
        (json!(640), json!(480))
    );
    assert_eq!(info["pixel_format"], "Mono8");
    assert_eq!(info["bits"], 8);
    assert_eq!(info["exposure_us"], 10000.0);
    assert_eq!(info["gain_db"], 0.0);
    assert_eq!(
        info["trigger"],
        json!({"selector":"FrameStart","mode":"Off","source":"Software","activation":"RisingEdge"})
    );
    assert_eq!(info["controls"]["exposure"]["feature"], "ExposureTime");
    assert_eq!(info["controls"]["exposure"]["value"], "10ms");
    let text = home.run(&["info", "-c", "sim:0"]);
    assert!(text.stdout.contains("640x480 Mono8"), "{}", text.stdout);
    let features = home.ok(&["features", "-c", "sim:0"]);
    let rows = features["features"].as_array().unwrap();
    let row = |name: &str| rows.iter().find(|row| row["name"] == name).unwrap().clone();
    assert_eq!(row("Width")["type"], "int");
    assert_eq!(row("Width")["access"], "rw");
    assert_eq!(row("DeviceScanType")["access"], "ro");
    assert_eq!(row("TriggerMode")["selectors"], json!(["TriggerSelector"]));
    assert_eq!(row("TriggerSoftware")["type"], "command");
    assert_eq!(row("PixelFormat")["category"], "ImageFormatControl");
    assert!(row("PixelFormat")["choices"].as_array().unwrap().len() > 1);
    let filtered = home.ok(&["features", "-c", "sim:0", "trigger*"]);
    assert!(
        filtered["features"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["name"].as_str().unwrap().starts_with("Trigger")),
        "{filtered}"
    );
    let category = home.ok(&[
        "features",
        "-c",
        "sim:0",
        "--category",
        "ImageFormatControl",
    ]);
    assert_eq!(category["features"].as_array().unwrap().len(), 3);
    assert_eq!(
        home.fails(2, &["features", "-c", "sim:0", "--category", "Nope"])["code"],
        "feature_not_found"
    );
}

#[test]
fn reasons_say_what_went_wrong_with_details() {
    let home = Home::new("reasons");
    let missing = home.fails(2, &["get", "-c", "sim:0", "ExposureTme"]);
    assert_eq!(missing["code"], "feature_not_found");
    assert_eq!(missing["details"]["did_you_mean"][0], "ExposureTime");
    let range = home.fails(2, &["set", "-c", "sim:0", "Gain=99"]);
    assert_eq!(range["code"], "out_of_range");
    assert_eq!(range["details"]["min"], 0);
    assert_eq!(range["details"]["max"], 24);
    assert_eq!(range["details"]["unit"], "dB");
    assert!(range["details"]["nearest"].is_number());
    let choice = home.fails(2, &["set", "-c", "sim:0", "PixelFormat=Mono99"]);
    assert_eq!(choice["code"], "invalid_choice");
    assert!(
        choice["details"]["choices"]
            .as_array()
            .unwrap()
            .contains(&json!("Mono8"))
    );
    let readonly = home.fails(2, &["set", "-c", "sim:0", "DeviceScanType=Linescan"]);
    assert_eq!(readonly["code"], "not_writable");
    let unknown = home.fails(3, &["get", "-c", "sim0", "Width"]);
    assert_eq!(unknown["code"], "camera_not_found");
    assert!(
        unknown["hint"].as_str().unwrap().contains("sim:0"),
        "{unknown}"
    );
    assert!(unknown["details"]["candidates"].is_array());
    let file = home.path("missing.mp4");
    let gone = home.fails(3, &["get", "-c", file.to_str().unwrap(), "Width"]);
    assert_eq!(gone["code"], "camera_not_found");
    let unsupported = home.fails(5, &["set", "-c", "sim:depth", "exposure=5ms"]);
    assert_eq!(unsupported["code"], "unsupported");
    assert_eq!(
        home.fails(
            5,
            &[
                "get",
                "-c",
                "onvif:https://192.0.2.1/onvif/device_service",
                "Width"
            ]
        )["code"],
        "unsupported"
    );
    for (camera, hint) in [
        ("hub:home/camera.front_door", "rtsp://HOST"),
        ("scanner:sane:test", "camera"),
    ] {
        let error = home.fails(2, &["get", "-c", camera, "Width"]);
        assert_eq!(error["code"], "usage");
        assert!(error["hint"].as_str().unwrap().contains(hint), "{error}");
    }
    let streaming = home.fails(2, &["exec", "-c", "sim:0", "AcquisitionStart"]);
    assert_eq!(streaming["code"], "usage");
    assert!(streaming["hint"].as_str().unwrap().contains("capture"));
    assert_eq!(
        home.fails(2, &["get", "-c", "sim:0", "Width[FrameStart]"])["code"],
        "usage"
    );
    // A command has no value to read.
    let command = home.fails(2, &["get", "-c", "sim:0", "TriggerSoftware"]);
    assert!(
        command["message"]
            .as_str()
            .unwrap()
            .contains("run it with exec"),
        "{command}"
    );
    // What was read before a camera failed goes with the error.
    let partial = home.fails(3, &["get", "-c", "sim:0", "-c", "sim0", "Width"]);
    assert_eq!(partial["code"], "camera_not_found");
    assert_eq!(partial["details"]["cameras"][0]["camera"], "sim:0");
    assert_eq!(partial["details"]["cameras"][0]["values"]["Width"], 640);
    assert!(partial["details"]["candidates"].is_array());
    let table = home.run(&["features", "-c", "sim:0", "--all"]).stdout;
    assert!(!table.contains("null"), "{table}");
    assert!(!home.sessions.join("cameras").exists());
}

#[test]
fn names_take_banks_and_canonical_controls() {
    let home = Home::new("names");
    let _session = home.serve("names", &["sim:0"]);
    let on = |args: &[&str]| home.ok(&[&["--session", "names"][..], args].concat());
    let set = on(&[
        "set",
        "TriggerMode[FrameStart]=On",
        "TriggerSource[TriggerSelector=FrameStart]=Line0",
    ]);
    let names = set["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|change| change["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    // The source is chosen before the trigger is armed.
    assert_eq!(
        names,
        ["TriggerSource[FrameStart]", "TriggerMode[FrameStart]"]
    );
    let values = on(&[
        "get",
        "TriggerMode[FrameStart]",
        "TriggerMode[AcquisitionStart]",
        "trigger",
        "TriggerSelector",
    ])["values"]
        .clone();
    assert_eq!(values["TriggerMode[FrameStart]"], "On");
    assert_eq!(values["TriggerMode[AcquisitionStart]"], "Off");
    assert_eq!(values["trigger"], "line0:rising");
    // The bank used is selected again afterwards.
    assert_eq!(values["TriggerSelector"], "FrameStart");
    on(&["set", "TriggerSelector=AcquisitionStart"]);
    on(&["set", "trigger=off"]);
    let values = on(&["get", "TriggerMode[FrameStart]", "TriggerSelector"])["values"].clone();
    assert_eq!(values["TriggerMode[FrameStart]"], "Off");
    assert_eq!(values["TriggerSelector"], "AcquisitionStart");
    let exposure = on(&["set", "exposure=5ms"]);
    assert_eq!(
        exposure["changed"],
        json!([{"name":"ExposureTime","from":10000.0,"to":5000.0}])
    );
    assert_eq!(
        on(&["get", "ExposureTime"])["values"]["ExposureTime"],
        5000.0
    );
    let roi = on(&["set", "roi=0,0,320,240"]);
    assert_eq!(roi["changed"].as_array().unwrap().len(), 2, "{roi}");
    assert_eq!(on(&["get", "roi"])["values"]["roi"], "0,0,320,240");
    on(&["set", "roi=full"]);
    assert_eq!(on(&["get", "roi"])["values"]["roi"], "0,0,1920,1080");
}

#[test]
fn set_is_one_atomic_change() {
    let home = Home::new("atomic");
    let _session = home.serve("atomic", &["sim:0"]);
    let on = |args: &[&str]| home.ok(&[&["--session", "atomic"][..], args].concat());
    let width = || on(&["get", "Width"])["values"]["Width"].clone();
    // A value no other write of the change can bring into range fails
    // before anything is written, and a dry run says so too.
    for dry_run in [&[][..], &["--dry-run"]] {
        let failed = home.fails(
            2,
            &[
                &["--session", "atomic", "set", "Width=128", "Height=5000"][..],
                dry_run,
            ]
            .concat(),
        );
        assert_eq!(failed["code"], "out_of_range", "{failed}");
        assert_eq!(failed["details"]["max"], 1080.0, "{failed}");
    }
    assert_eq!(width(), 640);
    // A dry run checks and plans, and writes nothing.
    let plan = on(&["set", "Width=256", "exposure=2ms", "--dry-run"]);
    assert_eq!(plan["dry_run"], true);
    assert_eq!(plan["plan"].as_array().unwrap().len(), 2);
    assert!(plan.get("changed").is_none());
    assert_eq!(width(), 640);
    // A value locked by its own auto gets that auto turned off first.
    on(&["set", "ExposureAuto=Once"]);
    let unlocked = on(&["set", "ExposureTime=3000"]);
    assert_eq!(
        unlocked["changed"],
        json!([
            {"name":"ExposureAuto","from":"Once","to":"Off"},
            {"name":"ExposureTime","from":10000.0,"to":3000.0}
        ])
    );
    // Keeping the auto on while setting the value it drives is locked.
    on(&["set", "ExposureAuto=Once"]);
    let locked = home.fails(
        1,
        &[
            "--session",
            "atomic",
            "set",
            "ExposureAuto=Once",
            "ExposureTime=4000",
        ],
    );
    assert_eq!(locked["code"], "feature_locked");
    assert_eq!(locked["details"]["locked_by"], "ExposureAuto");
    let planned = home.fails(
        1,
        &[
            "--session",
            "atomic",
            "set",
            "ExposureAuto=Once",
            "ExposureTime=4000",
            "--dry-run",
        ],
    );
    assert_eq!(planned["code"], "feature_locked");
    assert_eq!(
        on(&["get", "ExposureTime"])["values"]["ExposureTime"],
        3000.0
    );
    // Geometry cannot change while the camera streams.
    on(&["start"]);
    let busy = home.fails(3, &["--session", "atomic", "set", "Width=320"]);
    assert_eq!(busy["code"], "camera_busy");
    assert!(busy["hint"].is_string());
    on(&["stop"]);
    on(&["set", "Width=320"]);
}

#[test]
fn sessions_are_used_when_they_hold_the_camera() {
    let home = Home::new("routing");
    let _first = home.serve("first", &["sim:first"]);
    let _second = home.serve("second", &["sim:a", "sim:b"]);
    // The session that holds the camera answers; its state persists.
    home.ok(&["set", "-c", "sim:first", "Width=320"]);
    assert_eq!(
        home.ok(&["get", "-c", "sim:first", "Width"])["values"]["Width"],
        320
    );
    // --no-session opens a worker of its own, with the camera's defaults.
    assert_eq!(
        home.ok(&["--no-session", "get", "-c", "sim:first", "Width"])["values"]["Width"],
        640
    );
    // Without -c, the only session with exactly one camera.
    assert_eq!(home.ok(&["get", "Width"])["camera"], "sim:first");
    // A session with two cameras needs -c.
    let ambiguous = home.fails(2, &["--session", "second", "get", "Width"]);
    assert_eq!(ambiguous["code"], "camera_ambiguous");
    assert_eq!(
        ambiguous["details"]["candidates"].as_array().unwrap().len(),
        2
    );
    assert_eq!(
        home.ok(&["--session", "second", "get", "-c", "sim:b", "Width"])["camera"],
        "sim:b"
    );
    let missing = home.fails(3, &["--session", "nope", "get", "Width"]);
    assert_eq!(missing["code"], "session_not_found");
    // Session and direct results are the same document.
    let direct = home.ok(&["--no-session", "info", "-c", "sim:a"]);
    let session = home.ok(&["--session", "second", "info", "-c", "sim:a"]);
    assert_eq!(direct, session);
}

#[test]
fn session_commands_act_only_on_the_camera_named() {
    let home = Home::new("named-target");
    let _bench = home.serve("bench", &["sim:0"]);
    // A camera the session does not hold is refused, not swapped for its
    // only one.
    for command in [&["disconnect"][..], &["stop"], &["status"], &["jobs"]] {
        let error = home.fails(
            3,
            &[&["--session", "bench"][..], command, &["-c", "sim:9"]].concat(),
        );
        assert_eq!(error["code"], "camera_not_found", "{command:?}");
        assert_eq!(error["details"]["candidates"][0]["id"], "sim:0");
    }
    let status = home.ok(&["--session", "bench", "status"]);
    assert_eq!(status["cameras"][0]["info"]["id"], "sim:0");
    home.ok(&["--session", "bench", "disconnect", "-c", "sim:0"]);
    // With none left, a command for the only camera has none to act on.
    let none = home.fails(3, &["--session", "bench", "stop"]);
    assert_eq!(none["code"], "camera_not_found");
}

#[test]
fn an_environment_session_yields_to_no_session_and_binary_stdout() {
    let home = Home::new("env-session");
    let _first = home.serve("first", &[]);
    let env = [("CAPTUREFAB_SESSION", "first")];
    // --no-session overrides CAPTUREFAB_SESSION; the session stays empty.
    let run = home.run_env(
        &env,
        &["--json", "--no-session", "get", "-c", "sim:env", "Width"],
    );
    assert_eq!(run.success()["values"]["Width"], 640);
    let run = home.run_env(&env, &["--json", "status"]);
    assert!(run.success()["cameras"].as_array().unwrap().is_empty());
    // Both typed is a contradiction.
    let error = home.fails(2, &["--session", "first", "--no-session", "list"]);
    assert_eq!(error["code"], "usage");
    // Binary stdout comes from a worker of this command.
    let run = home.run_env(
        &env,
        &[
            "capture",
            "-c",
            "sim:stdout",
            "-o",
            "-",
            "--format",
            "raw",
            "--set",
            "Width=64",
            "--set",
            "Height=64",
        ],
    );
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.raw.len(), 4096);
    let run = home.run_env(&env, &["--json", "status"]);
    assert!(run.success()["cameras"].as_array().unwrap().is_empty());
}

#[test]
fn debug_rpc_takes_public_commands_only() {
    let home = Home::new("rpc");
    for request in [
        r#"{"op":"status","unknown":1}"#,
        r#"{"op":"status","op":"start"}"#,
        r#"{"op":"grab","timeout_ms":100}"#,
        r#"{"op":"optimize_network","bytes_per_second":1000}"#,
        r#"{"op":"set","feature":"Width","value":"320"}"#,
        r#"{"op":"features"}"#,
        r#"{"op":"discover","timeout_ms":100,"simulated":true}"#,
    ] {
        let run = home.run_stdin(request, &["--json", "--session", "missing", "debug", "rpc"]);
        assert_eq!(run.failure(2)["code"], "usage", "{request}");
    }
    assert!(!home.sessions.exists());
    let run = home.run_stdin(
        r#"{"op":"get_values","names":[{"name":"TriggerMode","bank":[{"value":"FrameStart"}]}]}"#,
        &["--json", "debug", "rpc", "-c", "sim:0"],
    );
    assert_eq!(run.success()["values"]["TriggerMode[FrameStart]"], "Off");
    let xml = home.ok(&["debug", "xml", "-c", "sim:0"]);
    assert!(xml["xml"].as_str().unwrap().contains("RegisterDescription"));
    let read = home.ok(&["debug", "read", "-c", "sim:0", "0x100", "4"]);
    assert_eq!(read["hex"], "80020000");
}

#[test]
fn schema_describes_every_command_and_its_results() {
    let home = Home::new("schema");
    let schema = home.ok(&["schema"]);
    assert_eq!(schema["contract_version"], 1);
    let commands = schema["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|command| command["name"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    for name in [
        "list",
        "info",
        "features",
        "get",
        "set",
        "exec",
        "preset apply",
        "doctor",
        "schema",
        "licenses",
    ] {
        assert!(commands.iter().any(|command| command == name), "{name}");
    }
    for command in &schema["commands"].as_array().unwrap()[..] {
        assert!(
            ["read", "camera_write", "file_write", "disruptive"]
                .contains(&command["effect"].as_str().unwrap()),
            "{command}"
        );
        let words = command["name"]
            .as_str()
            .unwrap()
            .split(' ')
            .collect::<Vec<_>>();
        let described = home.ok(&[&["schema"][..], &words].concat());
        assert_eq!(described["command"], command["name"]);
        // Every argument schema names is one --help shows.
        let help = home.run(&[&words[..], &["--help"]].concat()).stdout;
        for arg in described["args"].as_array().unwrap() {
            let shown = arg["long"]
                .as_str()
                .or(arg["value_name"].as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| arg["name"].as_str().unwrap().to_uppercase());
            assert!(
                help.contains(&shown) || help.contains(&shown.replace('_', "-")),
                "{words:?} --help lacks {shown}"
            );
        }
    }
    let codes = schema["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|error| error["code"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    for code in [
        "usage",
        "invalid_duration",
        "feature_not_found",
        "out_of_range",
        "camera_not_found",
        "storage_full",
        "interrupted",
    ] {
        assert!(codes.contains(code), "{code}");
    }
    assert!(schema.to_string().len() < 9000, "the overview stays small");
}

/// The top-level keys a command prints are the ones `schema COMMAND`
/// documents.
#[test]
fn results_carry_only_documented_keys() {
    let home = Home::new("documented");
    let preset = home.path("bench.json");
    let preset = preset.to_str().unwrap();
    let runs: &[(&str, &[&str])] = &[
        ("list", &["list", "--kind", "sim"]),
        ("info", &["info", "-c", "sim:0"]),
        ("features", &["features", "-c", "sim:0"]),
        ("get", &["get", "-c", "sim:0", "Width"]),
        ("get", &["get", "-c", "sim:0", "-c", "sim:1", "Width"]),
        ("set", &["set", "-c", "sim:0", "Width=320"]),
        ("set", &["set", "-c", "sim:0", "Width=320", "--dry-run"]),
        ("exec", &["exec", "-c", "sim:0", "TriggerSoftware"]),
        ("preset save", &["preset", "save", "bench", "-c", "sim:0"]),
        ("preset list", &["preset", "list"]),
        ("preset show", &["preset", "show", "bench"]),
        (
            "preset apply",
            &["preset", "apply", "bench", "-c", "sim:0", "--dry-run"],
        ),
        ("preset apply", &["preset", "apply", "bench", "-c", "sim:0"]),
        (
            "preset export",
            &["preset", "export", "bench", "-o", preset],
        ),
        ("preset export", &["preset", "export", "bench"]),
        ("preset rm", &["preset", "rm", "bench"]),
        ("preset import", &["preset", "import", preset]),
        ("login list", &["login", "list"]),
        ("doctor", &["doctor"]),
        ("licenses", &["licenses"]),
        ("schema", &["schema"]),
        ("network", &["network", "--timeout", "100ms"]),
        ("completions", &["completions", "zsh"]),
        ("debug xml", &["debug", "xml", "-c", "sim:0"]),
        ("debug read", &["debug", "read", "-c", "sim:0", "0x100"]),
    ];
    for (command, args) in runs {
        let result = home.ok(args);
        let words = command.split(' ').collect::<Vec<_>>();
        let documented = home.ok(&[&["schema"][..], &words].concat())["result"].clone();
        for (key, value) in result.as_object().unwrap() {
            let Some(meaning) = documented.get(key) else {
                panic!("{command}: {key} is not in schema {command}");
            };
            // Rows: every key of every row is named in `[{a,b?,c:…}]`.
            let meaning = meaning.as_str().unwrap();
            let (Some(row), Some(rows)) = (meaning.strip_prefix("[{"), value.as_array()) else {
                continue;
            };
            let names = row_keys(row);
            for row in rows.iter().filter_map(Value::as_object) {
                for field in row.keys() {
                    assert!(
                        names.contains(field.as_str()),
                        "{command}: {key}[].{field} is not in {meaning}"
                    );
                }
            }
        }
    }
}

/// The field names of a documented row, `a,b?,c:{x,y},d:x|y}]`, leaving
/// nested shapes out.
fn row_keys(row: &str) -> BTreeSet<&str> {
    let mut names = BTreeSet::new();
    let (mut depth, mut start, mut naming) = (0, 0, true);
    for (index, c) in row.char_indices() {
        match c {
            '{' | '[' => depth += 1,
            '}' | ']' if depth == 0 => {
                if naming {
                    names.insert(&row[start..index]);
                }
                break;
            }
            '}' | ']' => depth -= 1,
            ':' | '?' if depth == 0 && naming => {
                names.insert(&row[start..index]);
                naming = false;
            }
            ',' if depth == 0 => {
                if naming {
                    names.insert(&row[start..index]);
                }
                start = index + 1;
                naming = true;
            }
            _ => {}
        }
    }
    names.into_iter().map(str::trim).collect()
}

#[test]
fn every_new_command_shows_examples() {
    let home = Home::new("help");
    for words in [
        &["list"][..],
        &["info"],
        &["features"],
        &["get"],
        &["set"],
        &["exec"],
        &["preset"],
        &["login"],
        &["onvif"],
        &["debug"],
        &["preset", "save"],
        &["preset", "list"],
        &["preset", "show"],
        &["preset", "apply"],
        &["preset", "rm"],
        &["preset", "import"],
        &["preset", "export"],
        &["login", "add"],
        &["login", "list"],
        &["login", "rm"],
        &["onvif", "resolve"],
        &["network"],
        &["doctor"],
        &["schema"],
        &["completions"],
        &["debug", "xml"],
        &["debug", "read"],
        &["debug", "write"],
        &["debug", "rpc"],
        &["licenses"],
    ] {
        let help = home.run(&[words, &["--help"]].concat()).stdout;
        let examples = help
            .split_once("Examples:\n")
            .unwrap_or_else(|| panic!("{words:?} has no examples\n{help}"))
            .1
            .lines()
            .filter(|line| line.starts_with("  ") && line.contains("capturefab "))
            .count();
        assert!(
            (2..=4).contains(&examples),
            "{words:?}: {examples} examples"
        );
    }
    let top = home.run(&["--help"]).stdout;
    for removed in [
        "discover",
        "network-scan",
        "setup",
        "version",
        "memory",
        "xml",
        "rpc",
    ] {
        assert!(
            !top.lines()
                .any(|line| line.trim_start().starts_with(&format!("{removed} "))),
            "{removed} is still listed\n{top}"
        );
    }
}

#[test]
fn presets_save_preview_and_restore() {
    let home = Home::new("presets");
    let _session = home.serve("presets", &["sim:line:conveyor"]);
    let on = |args: &[&str]| home.ok(&[&["--session", "presets"][..], args].concat());
    let rate = || on(&["get", "AcquisitionLineRate"])["values"]["AcquisitionLineRate"].clone();
    on(&["preset", "save", "line"]);
    let saved = rate();
    on(&["set", "AcquisitionLineRate=500"]);
    // A taken name is refused before any camera is read.
    let taken = home.fails(1, &["preset", "save", "line", "-c", "gige:192.0.2.1"]);
    assert_eq!(taken["code"], "exists", "{taken}");
    assert!(
        taken["message"].as_str().unwrap().contains("--replace"),
        "{taken}"
    );
    // The dry run lists the disruptive change; nothing is written.
    let preview = on(&["preset", "apply", "line", "--dry-run"]);
    assert_eq!(preview["dry_run"], true);
    assert_eq!(preview["preset"], "line");
    assert_eq!(preview["changes"][0]["feature"], "AcquisitionLineRate");
    assert_eq!(preview["changes"][0]["risk"], "disruptive");
    assert_eq!(rate(), 500.0);
    let text = home
        .run(&[
            "--session",
            "presets",
            "preset",
            "apply",
            "line",
            "--dry-run",
        ])
        .stdout;
    assert!(text.contains("! AcquisitionLineRate: 500 → "), "{text}");
    // No confirmation step: applying applies.
    let applied = on(&["preset", "apply", "line"]);
    assert!(applied["applied"].as_u64().unwrap() > 0, "{applied}");
    assert_eq!(rate(), saved);
    // Export and import round-trip the document; export never overwrites.
    let file = home.path("line.json");
    let file = file.to_str().unwrap();
    on(&["preset", "export", "line", "-o", file]);
    assert_eq!(
        home.fails(1, &["preset", "export", "line", "-o", file])["code"],
        "exists"
    );
    on(&["preset", "rm", "line"]);
    assert_eq!(on(&["preset", "import", file])["preset"]["name"], "line");
    let taken = home.fails(1, &["preset", "import", file]);
    assert_eq!(taken["code"], "exists", "{taken}");
    assert!(taken["details"]["path"].is_string(), "{taken}");
    // Without -o, export prints the document itself.
    let exported = home.ok(&["preset", "export", "line"]);
    assert_eq!(exported["name"], "line");
    assert!(exported["values"].is_array());
    // A value this camera lacks: strict refuses, before and during apply.
    let mut document: Value =
        serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    document["values"]
        .as_array_mut()
        .unwrap()
        .push(json!({"feature":"Scan3dOutputMode","value":"Range"}));
    let extra = home.path("extra.json");
    std::fs::write(&extra, serde_json::to_vec(&document).unwrap()).unwrap();
    let extra = extra.to_str().unwrap();
    on(&["preset", "apply", "--file", extra, "--dry-run"]);
    for flags in [&["--dry-run", "--strict"][..], &["--strict"]] {
        let error = home.fails(
            5,
            &[
                &["--session", "presets", "preset", "apply", "--file", extra][..],
                flags,
            ]
            .concat(),
        );
        assert!(
            error["message"].as_str().unwrap().contains(
                "cannot be applied strictly; nothing changed. Not applicable: Scan3dOutputMode"
            ),
            "{error}"
        );
    }
    let list = on(&["preset", "list"]);
    assert_eq!(list["presets"][0]["name"], "line");
    assert!(home.fails(3, &["preset", "show", "nope"])["message"].is_string());
}

#[test]
fn preset_apply_refuses_before_changing_and_keeps_reasons() {
    let home = Home::new("preset-reasons");
    let _session = home.serve("reasons", &["sim:0"]);
    let on = |args: &[&str]| home.ok(&[&["--session", "reasons"][..], args].concat());
    on(&["preset", "save", "p1"]);
    on(&["set", "ExposureTime=2000", "Gain=5"]);
    // The simulator has no user sets: --persist is refused up front, in a
    // dry run too, and nothing changes.
    for flags in [&["--dry-run"][..], &[]] {
        let error = home.fails(
            5,
            &[
                &["--session", "reasons", "preset", "apply", "p1", "--persist"][..],
                flags,
            ]
            .concat(),
        );
        assert_eq!(error["code"], "unsupported", "{error}");
    }
    assert_eq!(
        on(&["get", "ExposureTime"])["values"]["ExposureTime"],
        2000.0
    );
    // A saved value that does not fit fails with its own reason, and what
    // was written is put back.
    let mut document = on(&["preset", "show", "p1"]);
    for value in document["values"].as_array_mut().unwrap() {
        if value["feature"] == "Height" {
            value["value"] = json!(5000);
        }
    }
    let file = home.path("bad.json");
    std::fs::write(&file, serde_json::to_vec(&document).unwrap()).unwrap();
    let error = home.fails(
        2,
        &[
            "--session",
            "reasons",
            "preset",
            "apply",
            "--file",
            file.to_str().unwrap(),
        ],
    );
    assert_eq!(error["code"], "out_of_range", "{error}");
    assert!(
        error["details"]["rollback"]["restored"]
            .as_array()
            .unwrap()
            .contains(&json!("Gain")),
        "{error}"
    );
    assert_eq!(on(&["get", "Gain"])["values"]["Gain"], 5.0);
}

#[test]
fn doctor_and_licenses() {
    let home = Home::new("doctor");
    let doctor = home.ok(&["doctor"]);
    assert_eq!(doctor["version"], env!("CARGO_PKG_VERSION"));
    let checks = doctor["checks"].as_array().unwrap();
    assert!(checks.iter().all(|check| {
        check["name"].is_string()
            && matches!(check["status"].as_str(), Some("ok" | "warn" | "fail"))
            && check["message"].is_string()
    }));
    assert!(checks.iter().any(|check| check["name"] == "ffmpeg"));
    let path = Value::from(home.sessions.to_string_lossy().into_owned());
    assert_eq!(doctor["dirs"]["runtime"], path);
    assert_eq!(doctor["dirs"]["state"], path);
    assert!(
        doctor["dirs"]["inspect"]
            .as_str()
            .unwrap()
            .ends_with("inspect")
    );
    assert_eq!(
        doctor["features"]["keychain"].as_bool(),
        Some(cfg!(feature = "keychain"))
    );
    assert!(doctor["ffmpeg"]["source"].is_string() || doctor["ffmpeg"]["error"].is_string());
    assert!(checks.iter().all(|check| check["name"] != "leftover_state"));
    // Files version 0.5 left behind, with the command that removes them.
    std::fs::create_dir_all(home.sessions.join("assistant")).unwrap();
    std::fs::write(home.sessions.join("gui.prefs.json"), "{}").unwrap();
    let doctor = home.ok(&["doctor"]);
    let leftovers = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "leftover_state")
        .cloned()
        .unwrap_or_else(|| panic!("{doctor}"));
    assert_eq!(leftovers["status"], "warn");
    let fix = leftovers["fix"].as_str().unwrap();
    assert!(
        fix.contains("gui.prefs.json") && fix.contains("assistant"),
        "{fix}"
    );
    let missing = home.path("missing-ffmpeg");
    let broken = home
        .run_env(
            &[("CAPTUREFAB_FFMPEG", missing.to_str().unwrap())],
            &["--json", "doctor"],
        )
        .success();
    let ffmpeg = broken["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "ffmpeg")
        .unwrap()
        .clone();
    assert_eq!(ffmpeg["status"], "warn", "{ffmpeg}");
    assert!(ffmpeg["fix"].is_string());
    let text = home.run(&["doctor"]).stdout;
    assert!(text.starts_with("Capturefab "), "{text}");
    let licenses = home.ok(&["licenses"]);
    assert!(
        licenses["capturefab"]
            .as_str()
            .unwrap()
            .starts_with("MIT License")
    );
    assert!(licenses["ffmpeg"].as_str().unwrap().contains("GPL"));
}

#[test]
fn login_commands_check_before_touching_the_credential_store() {
    let home = Home::new("login");
    let list = home.ok(&["login", "list"]);
    assert_eq!(list["logins"], json!([]));
    assert!(list["supported"].is_boolean());
    for (args, env) in [
        (
            &[
                "login",
                "add",
                "rtsp://u:p@h/x",
                "--user",
                "u",
                "--password-stdin",
            ][..],
            &[][..],
        ),
        (&["login", "add", "rtsp://h/x", "--password-stdin"], &[]),
        (&["login", "add", "rtsp://h/x", "--user", "u"], &[]),
        (
            &[
                "login",
                "add",
                "rtsp://h/x",
                "--user",
                "u",
                "--password-stdin",
            ],
            &[("CAPTUREFAB_CAMERA_PASSWORD", "x")],
        ),
        (
            &[
                "login",
                "add",
                "http://h/x",
                "--user",
                "u",
                "--password-stdin",
            ],
            &[],
        ),
    ] {
        let run = home.run_env(env, &[&["--json"][..], args].concat());
        assert_eq!(run.failure(2)["code"], "usage", "{args:?}");
        assert!(!run.stdout.contains("u:p@"));
    }
    let removed = home.run(&["--json", "login", "rm", "rtsp://h/x"]);
    assert!(matches!(removed.code, Some(3 | 5)), "{}", removed.stdout);
}

/// An ONVIF service that answers every request with a Digest challenge.
fn refusing_onvif() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                while let Ok(n) = stream.read(&mut buffer) {
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..n]);
                    let text = String::from_utf8_lossy(&request);
                    if let Some(head) = text.find("\r\n\r\n") {
                        let length = text[..head]
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if request.len() >= head + 4 + length {
                            break;
                        }
                    }
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"Camera\", nonce=\"n\", qop=\"auth\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            });
        }
    });
    address
}

#[test]
fn cameras_that_ask_for_a_sign_in_report_typed_errors() {
    let home = Home::new("auth");
    let address = refusing_onvif();
    let camera = format!("onvif:http://{address}/onvif/device_service");
    let required = home.fails(3, &["info", "-c", &camera]);
    assert_eq!(required["code"], "auth_required", "{required}");
    let camera = format!("onvif://admin:secret-pw@{address}");
    let run = home.run(&["--json", "info", "-c", &camera]);
    assert_eq!(run.failure(3)["code"], "auth_rejected");
    assert!(!run.stdout.contains("secret-pw") && !run.stderr.contains("secret-pw"));
    let resolve = home.fails(
        3,
        &[
            "onvif",
            "resolve",
            &format!("http://{address}/onvif/device_service"),
        ],
    );
    assert_eq!(resolve["code"], "auth_required", "{resolve}");
}

#[test]
fn network_is_read_only_unless_asked_to_tune() {
    let home = Home::new("network");
    let scan = home.ok(&["network", "--timeout", "100ms"]);
    assert!(scan["interfaces"].is_array());
    assert!(scan["time"].is_string());
    assert!(scan.get("scanned_at_ms").is_none());
    assert_eq!(home.fails(2, &["network", "-c", "sim:0"])["code"], "usage");
    // CAPTUREFAB_CAMERA is for camera commands; the checks stay read-only.
    let run = home.run_env(
        &[("CAPTUREFAB_CAMERA", "sim:0")],
        &["--json", "network", "--timeout", "100ms"],
    );
    assert!(run.success()["interfaces"].is_array());
    let text = home.run(&["network", "--timeout", "100ms"]);
    assert_eq!(text.code, Some(0));
}

#[test]
fn unreachable_cameras_say_what_to_try() {
    let home = Home::new("unreachable");
    let error = home.fails(3, &["info", "-c", "127.0.0.1"]);
    assert_eq!(error["code"], "unavailable", "{error}");
    let message = error["message"].as_str().unwrap();
    assert!(
        message.contains("127.0.0.1") && message.contains("sim:0"),
        "{message}"
    );
    // ONVIF hosts that do not resolve or refuse are unavailable too.
    for host in ["nonexistent.invalid", "127.0.0.1:1"] {
        let error = home.fails(3, &["onvif", "resolve", host, "--timeout", "2s"]);
        assert_eq!(error["code"], "unavailable", "{host}: {error}");
    }
    // A capture to an external drive that is not mounted.
    if cfg!(target_os = "macos") {
        let error = home.fails(
            3,
            &[
                "capture",
                "-c",
                "sim:0",
                "-o",
                "/Volumes/capturefab-missing-4f1c/x.png",
            ],
        );
        assert_eq!(error["code"], "unavailable", "{error}");
    }
}

#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn a_file_describes_itself_as_a_file() {
    let home = Home::new("file-info");
    let clip = home.path("clip.mkv");
    let ffmpeg = std::env::var("CAPTUREFAB_FFMPEG").unwrap_or_else(|_| "ffmpeg".into());
    let made = std::process::Command::new(ffmpeg)
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x48:rate=5",
        ])
        .args(["-frames:v", "3"])
        .arg(&clip)
        .output()
        .unwrap();
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    let info = home.ok(&["--no-session", "info", "-c", "clip.mkv"]);
    assert_eq!(info["kind"], "file", "{info}");
    assert_eq!(info["pixel_format"], "RGB8", "{info}");
    assert_eq!(info["bits"], 8, "{info}");
    assert_eq!(info["controls"]["format"]["value"], "RGB8", "{info}");
    let values = home.ok(&["--no-session", "get", "-c", "clip.mkv", "format"]);
    assert_eq!(values["values"]["format"], "RGB8");
    let error = home.fails(
        5,
        &["--no-session", "set", "-c", "clip.mkv", "exposure=5ms"],
    );
    assert_eq!(error["code"], "unsupported");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .starts_with("a file via FFmpeg"),
        "{error}"
    );
}

#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn a_refused_rtsp_stream_explains_itself_in_one_line() {
    let home = Home::new("rtsp-refused");
    let run = home.run(&["--json", "info", "-c", "rtsp://user:secret@127.0.0.1:1/x"]);
    let error = run.failure(3);
    assert_eq!(error["code"], "unavailable", "{error}");
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("127.0.0.1:1"), "{message}");
    assert!(
        !message.contains('\n') && !message.contains("secret") && !message.contains("@ 0x"),
        "{message}"
    );
}

#[cfg(unix)]
#[test]
fn doctor_says_how_to_make_shared_folders_private() {
    use std::os::unix::fs::PermissionsExt;
    let home = Home::new("doctor-private");
    std::fs::create_dir_all(&home.sessions).unwrap();
    std::fs::set_permissions(&home.sessions, std::fs::Permissions::from_mode(0o755)).unwrap();
    let doctor = home.ok(&["doctor"]);
    let private = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "private_dirs")
        .cloned()
        .unwrap_or_else(|| panic!("{doctor}"));
    assert_eq!(private["status"], "warn");
    assert!(
        private["fix"]
            .as_str()
            .unwrap()
            .starts_with(&format!("chmod 700 {}", home.sessions.display())),
        "{private}"
    );
}

#[test]
#[ignore = "saves to and removes from the real OS credential store"]
fn login_add_saves_in_the_real_credential_store() {
    let home = Home::new("login-real");
    let address = "rtsp://capturefab-test.invalid/live";
    home.run_env(
        &[("CAPTUREFAB_CAMERA_PASSWORD", "capturefab-test-password")],
        &[
            "--json",
            "login",
            "add",
            address,
            "--user",
            "test",
            "--no-check",
        ],
    )
    .success();
    home.ok(&["login", "rm", address]);
}

#[test]
fn unreadable_presets_are_listed_and_can_be_removed() {
    let home = Home::new("unreadable");
    let dir = home.ok(&["preset", "list"])["dir"]
        .as_str()
        .unwrap()
        .to_owned();
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for private in [&home.sessions, &std::path::PathBuf::from(&dir)] {
            std::fs::set_permissions(private, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    std::fs::write(std::path::Path::new(&dir).join("broken.json"), b"{not json").unwrap();
    let list = home.ok(&["preset", "list"]);
    let broken = &list["presets"][0];
    assert_eq!(broken["name"], "broken", "{list}");
    assert_eq!(broken["status"], "unreadable", "{list}");
    assert!(broken["reason"].is_string());
    let text = home.run(&["preset", "list"]).stdout;
    assert!(text.contains("capturefab preset rm broken"), "{text}");
    assert_eq!(
        home.fails(3, &["preset", "show", "broken"])["code"],
        "unavailable"
    );
    home.ok(&["preset", "rm", "broken"]);
    assert_eq!(home.ok(&["preset", "list"])["presets"], json!([]));
}
