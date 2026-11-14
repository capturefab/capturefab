//! Actual helper/`GenApi`/source/Saver integration; no physical Action/readiness claim.
use super::{Home, Run, producer, selector};
use crate::support::time;
use serde_json::{Value, json};
use std::{
    fs,
    io::Read as _,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

const PIXELS: [u8; 6] = [1, 2, 3, 4, 5, 6];
const KEY: u32 = 0x1122_3344;
const FILE_CAP: u64 = 1024 * 1024;

fn bytes(path: &Path) -> Vec<u8> {
    let mut data = Vec::new();
    fs::File::open(path)
        .unwrap()
        .take(FILE_CAP + 1)
        .read_to_end(&mut data)
        .unwrap();
    assert!(
        u64::try_from(data.len()).unwrap() <= FILE_CAP,
        "{} exceeds fixture bound",
        path.display()
    );
    data
}
fn rows(path: &Path) -> Vec<Value> {
    String::from_utf8(bytes(path))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn no_key(value: &Value) {
    match value {
        Value::Object(fields) => {
            for (name, value) in fields {
                assert!(
                    !matches!(name.as_str(), "ActionDeviceKey" | "action_device_key"),
                    "exported WO key: {value}"
                );
                no_key(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                no_key(value);
            }
        }
        _ => {}
    }
}
fn config(home: &Home, sources: &[Value]) {
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":sources}).to_string(),
    )
    .unwrap();
}
fn still(id: &str, camera: &str, extension: &str, key: Option<u32>, every_ms: u64) -> Value {
    let mut source = json!({"id":id,"protocol":"camera","camera":camera,
        "output":format!("{id}-{{seq}}.{extension}"),"warmup_ms":0,
        "timeout_ms":time::ms(1500),"every_ms":every_ms});
    if let Some(key) = key {
        source["action_device_key"] = json!(key);
        // Existing local RW node: exercises the real pinned SetValues path,
        // without claiming physical-register mutation or changing image layout.
        source["settings"] = json!(["Width=3"]);
    }
    source
}
struct Recording {
    process: crate::support::cli::Session,
    stdout: PathBuf,
    stderr: PathBuf,
    args: Vec<String>,
    deadline: Instant,
}
impl Recording {
    fn start(home: &Home, producer: &Path, duration: &str, loss: Option<&Path>) -> Self {
        let args = [
            "--json",
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            duration,
            "-o",
            "run.jsonl",
        ];
        let stdout = home.path("command.stdout");
        let stderr = home.path("command.stderr");
        let mut command = home.command();
        command
            .env_remove("GENICAM_GENTL64_PATH")
            .env("CAPTUREFAB_GENTL_PRODUCERS", producer)
            .env("CAPTUREFAB_GENTL_MOCK_LOG", home.root.join("sdk.log"))
            .env("CAPTUREFAB_GENTL_MOCK_LOCK", home.root.join("sdk.lock"))
            .env_remove("CAPTUREFAB_GENTL_MOCK_EPOCH_LOSS")
            .args(args)
            .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
            .stderr(Stdio::from(fs::File::create(&stderr).unwrap()));
        if let Some(path) = loss {
            command.env("CAPTUREFAB_GENTL_MOCK_EPOCH_LOSS", path);
        }
        Self {
            process: crate::support::cli::Session {
                child: command.spawn().unwrap(),
            },
            stdout,
            stderr,
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            deadline: Instant::now() + time::secs(20),
        }
    }
    fn finish(mut self) -> Run {
        let status = wait(&mut self.process, self.deadline);
        let args: Vec<_> = self.args.iter().map(String::as_str).collect();
        Run::of_output(
            Output {
                status,
                stdout: bytes(&self.stdout),
                stderr: bytes(&self.stderr),
            },
            &args,
        )
    }
}
fn wait(process: &mut crate::support::cli::Session, deadline: Instant) -> std::process::ExitStatus {
    loop {
        if let Some(status) = process.child.try_wait().unwrap() {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "child exceeded bounded fixture wait"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn operations(home: &Home) -> Vec<String> {
    String::from_utf8(bytes(&home.root.join("sdk.log")))
        .unwrap()
        .lines()
        .map(|line| line.split_once(' ').unwrap().1.to_owned())
        .collect()
}
fn once_before_arm(home: &Home) -> Vec<String> {
    let operations = operations(home);
    for name in [
        "init",
        "producer_locked",
        "dev_open",
        "action_key_write",
        "action_key_bytes_one",
    ] {
        assert_eq!(
            operations
                .iter()
                .filter(|operation| operation.as_str() == name)
                .count(),
            1,
            "{operations:?}"
        );
    }
    for name in [
        "forbidden_action_key_read",
        "industrial_stream_before_key",
        "industrial_acquire_before_key",
        "producer_lock_busy",
    ] {
        assert!(
            !operations.iter().any(|operation| operation == name),
            "{operations:?}"
        );
    }
    let key = operations
        .iter()
        .position(|operation| operation == "action_key_bytes_one")
        .unwrap();
    for name in ["ds_start", "acq_start", "frame"] {
        let armed = operations
            .iter()
            .position(|operation| operation == name)
            .unwrap();
        assert!(key < armed, "{operations:?}");
    }
    operations
}
fn image(sample: &Value, png: bool) {
    no_key(sample);
    assert!(sample["elapsed_ns"].is_u64());
    assert!(sample.get("source_time").is_none());
    let value = &sample["value"];
    assert_eq!(value["width"], 3);
    assert_eq!(value["height"], 2);
    assert_eq!(value["pixel_format"], "Mono8");
    let data = bytes(Path::new(value["path"].as_str().unwrap()));
    if png {
        let mut reader = png::Decoder::new(std::io::Cursor::new(data))
            .read_info()
            .unwrap();
        let mut pixels = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut pixels).unwrap();
        assert_eq!((info.width, info.height), (3, 2));
        assert_eq!(&pixels[..info.buffer_size()], &PIXELS);
    } else {
        assert_eq!(data, PIXELS);
        let sidecar = bytes(Path::new(value["sidecar_path"].as_str().unwrap()));
        let metadata: Value = serde_json::from_slice(&sidecar).unwrap();
        assert_eq!(metadata["width"], 3);
        assert_eq!(metadata["height"], 2);
        assert_eq!(metadata["bytes"], 6);
        no_key(&metadata);
    }
}

#[test]
fn industrial_shared_stills_write_real_wo_key_before_real_helper_images_and_export() {
    let home = Home::new("gentl-industrial-key-images");
    let producer = producer(&home.root, 200);
    let camera = selector(&producer);
    config(
        &home,
        &[
            still("raw", &camera, "raw", Some(KEY), 3_600_000),
            still("png", &camera, "png", None, 3_600_000),
        ],
    );
    let result = Recording::start(&home, &producer, &time::arg(2000), None)
        .finish()
        .success();
    assert_eq!(result["samples"], 2);
    let samples = rows(&home.path("run.jsonl"));
    for (source, png) in [("raw", false), ("png", true)] {
        image(
            samples.iter().find(|row| row["source"] == source).unwrap(),
            png,
        );
    }
    no_key(&json!(samples));
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "export.jsonl",
        "--format",
        "jsonl",
    ]);
    let exported = rows(&home.path("export.jsonl"));
    assert_eq!(exported.len(), 2);
    for (source, png) in [("raw", false), ("png", true)] {
        image(
            exported.iter().find(|row| row["source"] == source).unwrap(),
            png,
        );
    }
    once_before_arm(&home);
}

#[test]
fn industrial_helper_epoch_loss_after_saved_pixels_never_reopens_or_reapplies_key() {
    let home = Home::new("gentl-industrial-key-epoch");
    let producer = producer(&home.root, 200);
    let camera = selector(&producer);
    config(
        &home,
        &[
            still("a", &camera, "raw", Some(KEY), time::ms(100)),
            still("b", &camera, "raw", None, time::ms(100)),
        ],
    );
    let marker = home.path("lose-owned-helper");
    let mut recording = Recording::start(&home, &producer, &time::arg(5000), Some(&marker));
    let until = Instant::now() + time::secs(4);
    loop {
        let published = fs::read_to_string(home.path("run.jsonl")).unwrap_or_default();
        if published
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|row| row["source"] == "a" || row["source"] == "b")
        {
            break;
        }
        assert!(
            recording.process.child.try_wait().unwrap().is_none(),
            "data ended before a saved image"
        );
        assert!(Instant::now() < until, "no actual saved-image publication");
        std::thread::sleep(Duration::from_millis(5));
    }
    // Test-only barrier after a published real image; this is no public arm signal.
    fs::write(&marker, b"retire this fixture helper").unwrap();
    let failed = recording.finish().failure(3);
    assert!(
        matches!(
            failed["code"].as_str(),
            Some("disconnected" | "worker_crashed")
        ),
        "{failed}"
    );
    let samples = rows(&home.path("run.jsonl"));
    let images = samples
        .iter()
        .filter(|row| row["source"] == "a" || row["source"] == "b")
        .collect::<Vec<_>>();
    assert_ne!(images, [] as [&Value; 0]);
    for sample in images {
        image(sample, false);
    }
    no_key(&json!(samples));
    let operations = once_before_arm(&home);
    assert_eq!(
        operations
            .iter()
            .filter(|name| *name == "industrial_epoch_lost")
            .count(),
        1
    );
}

#[test]
#[ignore = "requires existing FFmpeg; real GenTL indexed FFV1 and still shared-key workflow"]
fn industrial_video_and_still_share_one_real_key_owner_and_exact_ffv1_pixels() {
    let home = Home::new("gentl-industrial-key-video");
    let producer = producer(&home.root, 200);
    let camera = selector(&producer);
    config(
        &home,
        &[
            json!({"id":"video","protocol":"video","camera":camera,"output":"clip.mkv",
            "codec":"ffv1","fps":5,"warmup_ms":0,"timeout_ms":time::ms(1500),"action_device_key":KEY,"settings":["Width=3"]}),
            still("still", &camera, "raw", None, time::ms(200)),
        ],
    );
    let result = Recording::start(&home, &producer, &time::arg(2000), None)
        .finish()
        .success();
    let samples = rows(&home.path("run.jsonl"));
    let frames = samples
        .iter()
        .filter(|row| row["source"] == "video" && row["value"]["kind"] == "video_frame")
        .collect::<Vec<_>>();
    assert!(frames.len() >= 2, "{result}");
    let stills = samples
        .iter()
        .filter(|row| row["source"] == "still")
        .collect::<Vec<_>>();
    assert!(stills.len() >= 2);
    for sample in stills {
        image(sample, false);
    }
    let index = rows(&home.path("clip.mkv.frames.jsonl"));
    assert_eq!(index.len(), frames.len());
    let origin = index[0]["received_monotonic_ns"].as_u64().unwrap()
        - frames[0]["elapsed_ns"].as_u64().unwrap();
    for (number, (row, sample)) in index.iter().zip(&frames).enumerate() {
        assert_eq!(row["frame_index"], number);
        for key in ["frame_index", "frame_id", "video_time_ns"] {
            assert_eq!(sample["value"][key], row[key]);
        }
        assert_eq!(
            sample["elapsed_ns"].as_u64().unwrap(),
            row["received_monotonic_ns"].as_u64().unwrap() - origin
        );
    }
    let summary = samples
        .iter()
        .find(|row| row["source"] == "video" && row["signal"] == "recording")
        .unwrap();
    assert_eq!(summary["value"]["frames"], frames.len());
    assert_eq!(summary["value"]["finalized"], true);
    let ffmpeg = std::env::var_os("CAPTUREFAB_FFMPEG").unwrap_or_else(|| "ffmpeg".into());
    let decoded_path = home.path("decoded.gray");
    let errors_path = home.path("decoder.stderr");
    let mut decoder = crate::support::cli::Session {
        child: Command::new(ffmpeg)
            .args(["-v", "error", "-i"])
            .arg(home.path("clip.mkv"))
            .args([
                "-fps_mode",
                "passthrough",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "gray",
                "pipe:1",
            ])
            .stdout(Stdio::from(fs::File::create(&decoded_path).unwrap()))
            .stderr(Stdio::from(fs::File::create(&errors_path).unwrap()))
            .spawn()
            .unwrap(),
    };
    let status = wait(&mut decoder, Instant::now() + time::secs(10));
    assert!(
        status.success(),
        "{}",
        String::from_utf8_lossy(&bytes(&errors_path))
    );
    let decoded = bytes(&decoded_path);
    assert_eq!(decoded.len(), frames.len() * PIXELS.len());
    for frame in decoded.as_chunks::<6>().0 {
        assert_eq!(frame, &PIXELS);
    }
    no_key(&json!(samples));
    no_key(&json!(index));
    once_before_arm(&home);
}
