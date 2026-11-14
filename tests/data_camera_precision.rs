//! Industrial image references retain enough layout and precision metadata to decode them.
mod support;

use serde_json::{Value, json};
use std::{fs, path::Path};
use support::cli::Home;

#[test]
fn camera_observations_describe_stored_precision_and_raw_layout() {
    let home = Home::new("data-camera-precision");
    let mut sources = vec![
        json!({"id":"packed","protocol":"camera","camera":"sim:packed","output":"packed.raw",
            "settings":["Width=64","Height=64","PixelFormat=Mono12p"]}),
        json!({"id":"mono","protocol":"camera","camera":"sim:mono","output":"mono.png",
            "settings":["Width=64","Height=64","PixelFormat=Mono12"]}),
        json!({"id":"depth","protocol":"camera","camera":"sim:depth","output":"depth.png",
            "settings":["Width=64","Height=64"]}),
        json!({"id":"thermal","protocol":"camera","camera":"sim:thermal","output":"thermal.png",
            "settings":["Width=64","Height=64"]}),
    ];
    if cfg!(feature = "jpeg") {
        sources.push(
            json!({"id":"jpeg","protocol":"camera","camera":"sim:jpeg","output":"mono.jpg",
            "settings":["Width=64","Height=64","PixelFormat=Mono12"]}),
        );
    }
    for source in &mut sources {
        source["every_ms"] = json!(3_600_000);
        source["timeout_ms"] = json!(2000);
    }
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":sources}).to_string(),
    )
    .unwrap();
    let result = home.ok(&[
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "3s",
        "-o",
        "run.jsonl",
    ]);
    assert_eq!(result["status"], "complete");
    let recording = fs::read_to_string(home.path("run.jsonl")).unwrap();
    let rows = recording
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let sample = |source: &str| {
        let matching = rows
            .iter()
            .filter(|row| row["source"] == source)
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 1, "{recording}");
        let value = &matching[0]["value"];
        assert_eq!(value["width"], 64);
        assert_eq!(value["height"], 64);
        assert_eq!(
            value["bytes"],
            fs::metadata(value["path"].as_str().unwrap()).unwrap().len()
        );
        value
    };

    let packed = sample("packed");
    assert_eq!(packed["pixel_format"], "Mono12p");
    assert_eq!(packed["pfnc_code"], 0x010c_0047);
    assert_eq!(packed["bit_depth"], 12);
    assert!(packed.get("source_bits").is_none());
    let sidecar: Value =
        serde_json::from_slice(&fs::read(packed["sidecar_path"].as_str().unwrap()).unwrap())
            .unwrap();
    assert_eq!(sidecar["pfnc_code"], packed["pfnc_code"]);
    assert_eq!(sidecar["frame_id"], packed["frame_id"]);
    assert_eq!(sidecar["row_bytes"], 96);
    assert_eq!(sidecar["bytes"], 64 * 96);

    for (id, format, code) in [
        ("mono", "Mono12", 0x0110_0005),
        ("depth", "Coord3D_C16", 0x0110_00b8),
        ("thermal", "Mono16", 0x0110_0007),
    ] {
        let value = sample(id);
        assert_eq!(value["pixel_format"], format);
        assert_eq!(value["pfnc_code"], code);
        assert_eq!(value["bit_depth"], 16);
        assert!(value.get("source_bits").is_none());
        assert!(value.get("sidecar_path").is_none());
        let path = Path::new(value["path"].as_str().unwrap());
        let mut reader = png::Decoder::new(std::io::BufReader::new(fs::File::open(path).unwrap()))
            .read_info()
            .unwrap();
        assert_eq!(reader.info().bit_depth, png::BitDepth::Sixteen);
        assert_eq!(reader.info().color_type, png::ColorType::Grayscale);
        let mut bytes = vec![0; reader.output_buffer_size()];
        let frame = reader.next_frame(&mut bytes).unwrap();
        assert_eq!(frame.buffer_size(), 64 * 64 * 2);
        assert!(bytes.as_chunks::<2>().0.iter().any(|pair| pair[0] != 0));
        if id == "depth" {
            assert_eq!(&bytes[..2], &[0, 0], "invalid depth sample survives");
        }
    }
    if cfg!(feature = "jpeg") {
        let value = sample("jpeg");
        assert_eq!(value["pixel_format"], "Mono12");
        assert_eq!(value["bit_depth"], 8);
        assert_eq!(value["source_bits"], 12);
        assert!(value.get("sidecar_path").is_none());
    }
}
