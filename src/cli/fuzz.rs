//! Deterministic mutation fuzzing. Never dispatch commands: parsing must be
//! safe even when the input would otherwise access hardware or the network.
//! Semantic control tests use only the in-process simulator.
use super::*;

const SEEDS: &[&str] = &[
    "",
    "0",
    "-1",
    "1",
    "255",
    "256",
    "4294967295",
    "4294967296",
    "1e309",
    "true",
    "false",
    "{}",
    "1.5ms",
    "1h30m",
    "NaN",
    "inf",
    "18446744073709551615",
    "999999999999999999999999999999999999999s",
    "µs",
    "μs",
    "é💡中",
    "\0\r\n\x1b",
    "ExposureTime=5ms",
    "TriggerMode[FrameStart]=On",
    "Gain[GainSelector=All]",
    "{seq:9}/{tag:label}.{ext}",
    "{seq:99999999999999999999}",
    "../../x",
    "rtsp://user:secret@localhost/live?password=secret",
    "srt://[::1]:9000?streamid=x",
    "sim:0?drop=NaN&line0=every:1ms",
    "2026-10-31T14:00:00Z",
    "23:59",
    "onvif://user:secret@localhost/service",
    "<RegisterDescription><Integer Name=\"Gain\"><Value>1</Value></Integer></RegisterDescription>",
    "'quoted text' \\\"x\\\"",
    "{\"op\":\"status\"}",
    "[]",
    "null",
];

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 as usize
    }
}

fn corpus(test: impl FnMut(&str)) {
    corpus_with(4096, test);
}

fn corpus_with(default_cases: usize, mut test: impl FnMut(&str)) {
    let mut rng = Rng(0x4341_5054_5552_4546);
    let iterations = std::env::var("CAPTUREFAB_FUZZ_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default_cases);
    for seed in SEEDS {
        test(seed);
    }
    for length in [
        31, 32, 33, 255, 256, 257, 1023, 1024, 1025, 16383, 16384, 16385,
    ] {
        test(&"a".repeat(length));
        test(&"é".repeat(length / 2));
    }
    for _ in 0..iterations {
        let mut bytes = SEEDS[rng.next() % SEEDS.len()].as_bytes().to_vec();
        for _ in 0..1 + rng.next() % 8 {
            let at = rng.next() % (bytes.len() + 1);
            match rng.next() % 4 {
                0 if at < bytes.len() => {
                    bytes.remove(at);
                }
                1 if at < bytes.len() => {
                    bytes[at] = rng.next() as u8;
                }
                2 => {
                    let seed = SEEDS[rng.next() % SEEDS.len()];
                    bytes.splice(at..at, seed.bytes());
                }
                _ => bytes.insert(at, rng.next() as u8),
            }
        }
        let text = String::from_utf8_lossy(&bytes);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| test(&text)));
        assert!(result.is_ok(), "fuzz input: {text:?}");
    }
}

#[test]
fn fuzz_text_inputs() {
    if isolated("cli::fuzz::fuzz_text_inputs") {
        return;
    }
    corpus(|text| {
        let _ = args::duration(text);
        let _ = args::wait(text);
        let _ = args::time(text);
        let _ = inspect::parse_region(text);
        if let Ok(bytes) = debug::hex(text) {
            assert_eq!(crate::types::hex(bytes), text.to_ascii_lowercase());
        }
        let base = std::env::temp_dir();
        let mut path = text.to_owned();
        if paths::resolve_path(&mut path, &base).is_ok() {
            let once = path.clone();
            paths::resolve_path(&mut path, &base).unwrap();
            assert_eq!(path, once);
        }
        let _ = paths::resolve_selector(&mut text.to_owned(), &base);
        let _ = paths::resolve_recording(&mut text.to_owned(), &base);
        let _ = crate::types::parse_bytes(text);
        if let Ok(password) = crate::secret_input::read_stdin_password(text.as_bytes(), "fuzz") {
            let expected = text
                .strip_suffix('\n')
                .map(|s| s.strip_suffix('\r').unwrap_or(s))
                .unwrap_or(text);
            assert_eq!(password.as_str(), expected);
        }
        let _ = crate::setup::validate_name(text);
        let _ = crate::setup::validate_note(text);
        let _ = crate::setup::parse(text.as_bytes());
        let _ = crate::ipc::validate_name(text);
        let _ = crate::session::validate_selector(text);
        let _ = crate::camera::validate_selector_shape(text);
        let _ = crate::logins::key(text);
        let _ = crate::logins::percent_decode(text);
        let _ = crate::logins::split_userinfo(text);
        let _ = crate::genicam::NodeMap::parse(text);
        if let Ok(name) = crate::controls::Name::parse(text) {
            name.validate().unwrap();
            assert_eq!(
                crate::controls::Name::parse(&name.to_string()).unwrap(),
                name
            );
        }
        if let Ok(value) = crate::controls::Assignment::parse(text) {
            value.validate().unwrap();
        }
        if let Ok(pair) = crate::context::parse_pair(text) {
            crate::context::from_pairs(&[pair]).unwrap();
        }
        let _ = crate::naming::validate_output(text);
        let _ = crate::engine::validate_capture_output(text);
        let _ = crate::naming::validate_template(text);
        let _ = crate::naming::sanitize_component(text);
        let _ = crate::naming::tag_keys(text);
        let _ = crate::media::validate_source_shape(text);
        let _ = crate::media::validate_bitrate(text);
        for codec in ["h264", "hevc", "ffv1", "raw", "invalid"] {
            let _ = crate::media::validate_encoder(codec, text);
        }
        let _ = crate::onvif::validate_source_shape(text);
        let _ = crate::onvif::source_endpoint(text);
        let _ = crate::transport::simulator::validate_selector_shape(text);
        let redacted = crate::media::redact_url(text);
        assert_eq!(crate::media::redact_url(&redacted), redacted);
        let _ = crate::scrub::redact_urls(text);
        let _ = crate::types::printable_text(text);
        let _ = shell::split(text);
        let quoted = format!("'{}'", text.replace('\'', "'\\''"));
        assert_eq!(shell::split(&quoted).unwrap(), [text]);
    });
}

#[test]
fn fuzz_cli_arguments() {
    let tree = Cli::command();
    // Visit every command and option, including nested command families.
    fn visit(tree: &clap::Command, prefix: Vec<String>) {
        for arg in tree
            .get_arguments()
            .filter(|a| a.get_action().takes_values())
        {
            corpus_with(32, |text| {
                let mut argv = prefix.clone();
                if let Some(long) = arg.get_long() {
                    argv.push(format!("--{long}={text}"));
                } else if let Some(short) = arg.get_short() {
                    argv.push(format!("-{short}={text}"));
                } else {
                    for _ in 1..arg.get_index().unwrap_or(1) {
                        argv.push("0".into());
                    }
                    argv.push("--".into());
                    argv.push(text.to_owned());
                }
                let _ = Cli::try_parse_from(argv);
            });
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStringExt;
                let mut argv: Vec<std::ffi::OsString> = prefix.iter().map(Into::into).collect();
                if let Some(long) = arg.get_long() {
                    argv.push(format!("--{long}").into());
                } else if let Some(short) = arg.get_short() {
                    argv.push(format!("-{short}").into());
                }
                argv.push(std::ffi::OsString::from_vec(vec![0xff, 0xfe, b'=', 0x80]));
                let _ = Cli::try_parse_from(argv);
            }
        }
        for child in tree.get_subcommands() {
            let mut path = prefix.clone();
            path.push(child.get_name().to_owned());
            visit(child, path);
        }
    }
    visit(&tree, vec!["capturefab".into()]);
}

#[test]
fn fuzz_json_commands() {
    corpus(|text| {
        if let Ok(command) = SessionCommand::from_json_slice(text.as_bytes()) {
            let _ = command.validate();
            let encoded = serde_json::to_vec(&command).unwrap();
            let _: SessionCommand = serde_json::from_slice(&encoded).unwrap();
        }
    });
}

#[test]
fn fuzz_binary_stdin_and_documents() {
    let mut rng = Rng(0xfedc_ba98_7654_3210);
    for _ in 0..4096 {
        let mut bytes = SEEDS[rng.next() % SEEDS.len()].as_bytes().to_vec();
        for _ in 0..1 + rng.next() % 16 {
            let index = rng.next() % (bytes.len() + 1);
            bytes.insert(index, rng.next() as u8);
        }
        let _ = crate::setup::parse(&bytes);
        let _ = SessionCommand::from_json_slice(&bytes);
        if let Ok(secret) = crate::secret_input::read_stdin_password(bytes.as_slice(), "fuzz") {
            let original = std::str::from_utf8(&bytes).unwrap();
            let expected = original
                .strip_suffix('\n')
                .map(|s| s.strip_suffix('\r').unwrap_or(s))
                .unwrap_or(original);
            assert_eq!(secret.as_str(), expected);
        }
    }
}

#[test]
fn fuzz_structured_commands() {
    if isolated("cli::fuzz::fuzz_structured_commands") {
        return;
    }
    let mut seeds = json!([
        {"op":"network_scan","timeout_ms":1000,"tune":false},
        {"op":"connect","camera":"rtsp://localhost/live","timeout_ms":1000,"saved_login":true,"login":{"user":"fuzz-user","password":"fuzz-password"}},
        {"op":"disconnect"}, {"op":"get_values","names":[{"name":"Gain","bank":[{"selector":"GainSelector","value":"All"}]}]},
        {"op":"set_values","values":[{"name":"Gain","bank":[],"value":"1"}],"dry_run":true},
        {"op":"exec","name":{"name":"TriggerSoftware","bank":[]}},
        {"op":"info"}, {"op":"feature_list","filter":{"pattern":"*Gain*","all":true,"category":"AnalogControl"}}, {"op":"start"}, {"op":"stop"},
        {"op":"arm","warmup_ms":0,"bytes_per_second":100000}, {"op":"read_setup"},
        {"op":"setup_diff","name":"sample","allow_other_model":false},
        {"op":"apply_setup","name":"sample","strict":true,"allow_other_model":false},
        {"op":"auto","continuous":false,"region":[0.0,0.0,1.0,1.0],"max_exposure_us":1000.0,"job":1},
        {"op":"manual"},
        {"op":"capture","timeout_ms":1000,"format":"png","context":{"label":"sample"},"quality":90,"fresh":0,"warmup_ms":0},
        {"op":"schedule","output":"/tmp/{seq}.png","count":1,"timeout_ms":1000,"format":"png","first_at_ms":0,"interval_ms":1000,
         "quality":90,"fresh":0,"warmup_ms":0,"utc_offset_s":0,"context":{},"min_free":0,"inspect":false,"lease":false,"job":1,"on_exists":"fail","manifest":"/tmp/fuzz.jsonl","sync":"file"},
        {"op":"jobs"}, {"op":"wait","job":"sample-1","after_seq":0,"timeout_ms":1000},
        {"op":"stop_job","job":"sample-1"}, {"op":"status"},
        {"op":"frame","fresh":0,"timeout_ms":1000,"max_dimension":100,"format":"png","preview":"/tmp/fuzz-preview.png","min_free":0,"region":[0.0,0.0,1.0,1.0],"warmup_ms":0},
        {"op":"xml"}, {"op":"read_memory","address":0,"length":4},
        {"op":"write_memory","address":0,"data":[0,1,2,3]},
        {"op":"forward","output":"/tmp/sample.mp4","codec":"h264","encoder":"auto","fps":30.0,"bitrate":"4M",
         "max_file_bytes":1000000,"min_free":0,"on_exists":"fail","context":{},"duration_ms":1000,"utc_offset_s":0,"warmup_ms":0,"job":1,"lease":false,"copy":"never"}
    ]);
    for op in ["setup_diff", "apply_setup"] {
        seeds
            .as_array_mut()
            .unwrap()
            .push(json!({"op":op,"setup":preset_seed()}));
    }
    // Fixtures must reach validation on Windows as well as Unix. A /tmp
    // path has no drive prefix on Windows and fails absolute-path checks.
    for seed in seeds.as_array_mut().unwrap() {
        for key in ["output", "manifest", "preview"] {
            if let Some(name) = seed[key].as_str().and_then(|s| s.strip_prefix("/tmp/")) {
                seed[key] = json!(std::env::temp_dir().join(name));
            }
        }
    }
    fn check(value: &Value) {
        let bytes = serde_json::to_vec(value).unwrap();
        if let Ok(command) = SessionCommand::from_json_slice(&bytes) {
            let valid = command.validate().is_ok();
            let encoded = serde_json::to_vec(&command).unwrap();
            let decoded = SessionCommand::from_json_slice(&encoded).unwrap();
            assert_eq!(valid, decoded.validate().is_ok());
            if valid {
                let _ = crate::session::default_request_timeout(&command);
            }
        }
    }
    for seed in seeds.as_array().unwrap() {
        let command = SessionCommand::from_json_slice(&serde_json::to_vec(seed).unwrap())
            .unwrap_or_else(|e| panic!("invalid fuzz seed {seed}: {e}"));
        command
            .validate()
            .unwrap_or_else(|e| panic!("invalid fuzz seed {seed}: {e}"));
        check(seed);
        mutate_json(seed, &mut check);
        if seed["op"] == "capture" {
            let mut with_stdout = seed.clone();
            with_stdout["stdout"] = json!("/tmp/fuzz-stdout");
            // An internal stdout path is constrained to the private runtime
            // output directory; arbitrary RPC paths must also be rejected safely.
            mutate_json(&with_stdout, &mut check);
        }
    }
}

#[test]
fn fuzz_duration_roundtrip() {
    let mut rng = Rng(0x1234_5678_9abc_def0);
    for _ in 0..10000 {
        // Every supported duration, including nanoseconds at the ten-year limit.
        let nanos = rng.next() as u64 % 315_576_000_000_000_001;
        let duration = Duration::from_nanos(nanos);
        let text = crate::types::duration_text(duration);
        assert_eq!(
            crate::types::parse_duration(&text).unwrap(),
            duration,
            "{text}"
        );
    }
}

#[test]
fn fuzz_control_values_preserve_dry_run_state() {
    use crate::controls::{Assignment, Name};
    let mut camera = crate::camera::Camera::open("sim:0", &[], Duration::from_secs(1)).unwrap();
    let names: Vec<_> = crate::controls::CONTROLS
        .iter()
        .map(|(name, _)| Name::parse(name).unwrap())
        .filter(|name| crate::controls::get(&mut camera, std::slice::from_ref(name)).is_ok())
        .collect();
    let before = crate::controls::get(&mut camera, &names).unwrap();
    corpus_with(128, |text| {
        for (name, _) in crate::controls::CONTROLS {
            let assignment = Assignment {
                name: name.into(),
                bank: vec![],
                value: text.into(),
            };
            let _ = crate::controls::set(&mut camera, &[assignment], true);
            assert_eq!(
                crate::controls::get(&mut camera, &names).unwrap(),
                before,
                "dry run changed camera: {name}={text:?}"
            );
        }
    });
}

/// Change complete containers as well as scalar leaves, including array
/// elements and removal of object fields. Preserve the rest of the valid
/// document so deeper validators remain reachable.
fn mutate_json(root: &Value, check: &mut impl FnMut(&Value)) {
    fn visit(value: &Value, path: &str, root: &Value, check: &mut impl FnMut(&Value)) {
        corpus_with(32, |text| {
            let replacement = serde_json::from_str(text).unwrap_or_else(|_| json!(text));
            let mut changed = root.clone();
            *changed.pointer_mut(path).unwrap() = replacement;
            check(&changed);
        });
        match value {
            Value::Object(fields) => {
                for (key, child) in fields {
                    let mut removed = root.clone();
                    removed
                        .pointer_mut(path)
                        .unwrap()
                        .as_object_mut()
                        .unwrap()
                        .remove(key);
                    check(&removed);
                    let escaped = key.replace('~', "~0").replace('/', "~1");
                    visit(child, &format!("{path}/{escaped}"), root, check);
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    visit(child, &format!("{path}/{index}"), root, check);
                }
            }
            _ => {}
        }
    }
    visit(root, "", root, check);
}

#[test]
fn fuzz_preset_documents() {
    let seed = preset_seed();
    crate::setup::from_value(seed.clone()).unwrap();
    mutate_json(&seed, &mut |value| {
        let bytes = serde_json::to_vec(value).unwrap();
        if let Ok(setup) = crate::setup::parse(&bytes) {
            crate::setup::validate(&setup).unwrap();
            let encoded = serde_json::to_vec(&setup).unwrap();
            assert_eq!(crate::setup::parse(&encoded).unwrap(), setup);
        }
    });
}

fn preset_seed() -> Value {
    json!({
        "version":1, "name":"sample", "saved_at":"2026-10-31T14:00:00Z", "note":"sample",
        "camera":{"transport":"simulator","vendor":"Capturefab","model":"Pattern","serial":"0","kind":"area_scan"},
        "values":[{"feature":"Gain","value":1,"at":[{"selector":"GainSelector","value":"All"}]}],
        "skipped":[{"feature":"Missing","reason":"not available","at":[]}]
    })
}

// Credential parsing registers arbitrary secrets; isolate it from parallel
// tests whose output must not be redacted with generated passwords.
fn isolated(name: &str) -> bool {
    const CHILD: &str = "CAPTUREFAB_FUZZ_CHILD";
    if std::env::var(CHILD).ok().as_deref() == Some(name) {
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}
