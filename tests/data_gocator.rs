//! Passive legacy Gocator GDP groups through the production CLI and storage.
mod support;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read as _, Write as _},
    net::TcpListener,
    time::Duration,
};
use support::cli::{Home, Run};

fn message(type_id: u16, last: bool, body: &[u8]) -> Vec<u8> {
    let size = u32::try_from(6 + body.len()).unwrap();
    let control = type_id | (u16::from(last) << 15);
    let mut message = size.to_le_bytes().to_vec();
    message.extend_from_slice(&control.to_le_bytes());
    message.extend_from_slice(body);
    message
}

fn group() -> Vec<u8> {
    let mut stamp = vec![0; 64];
    stamp[0..4].copy_from_slice(&1u32.to_le_bytes());
    stamp[4..6].copy_from_slice(&56u16.to_le_bytes());
    stamp[8..16].copy_from_slice(&9_007_199_254_740_993u64.to_le_bytes());
    stamp[16..24].copy_from_slice(&(u64::MAX - 5).to_le_bytes());
    stamp[24..32].copy_from_slice(&(-321i64).to_le_bytes());
    stamp[32..40].copy_from_slice(&(-7i64).to_le_bytes());
    stamp[40..48].copy_from_slice(&0x101u64.to_le_bytes());
    stamp[48..52].copy_from_slice(&102_030_u32.to_le_bytes());
    let mut profile = 40u16.to_le_bytes().to_vec();
    for value in [2u32, 3, 2500, 125, 0xffff_fffe, 15] {
        profile.extend_from_slice(&value.to_le_bytes());
    }
    profile.push(1);
    profile.extend_from_slice(&4000u32.to_le_bytes());
    profile.extend_from_slice(&[0; 3]);
    profile.extend_from_slice(&2i32.to_le_bytes());
    profile.extend_from_slice(&9i32.to_le_bytes());
    for sample in [i16::MIN, -1, 0, 1, 32_000, i16::MAX] {
        profile.extend_from_slice(&sample.to_le_bytes());
    }
    [
        message(1, false, &stamp),
        message(6, false, &profile),
        message(0x123, true, &[9, 8, 7]),
    ]
    .concat()
}

fn record(home: &Home, listener: &TcpListener) -> Run {
    record_options(home, listener, None, "500ms")
}

fn record_options(home: &Home, listener: &TcpListener, limit: Option<&str>, duration: &str) -> Run {
    let mut config = json!({"version":1,"sources":[{
        "id":"profiles","protocol":"gocator_gdp","address":listener.local_addr().unwrap(),
        "output_directory":"profiles","timeout_ms":60
    }]});
    if let Some(limit) = limit {
        config["sources"][0]["max_group_bytes"] = serde_json::from_str(limit).unwrap();
    }
    fs::write(home.path("sources.json"), config.to_string()).unwrap();
    home.run(&[
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        duration,
        "-o",
        "run.jsonl",
    ])
}

fn rows(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

// LMI GoProfilePointCloudMsg_WriteV5 at ff648a46, including the two context words.
// The array contains original signed (X, Z) sample words, including invalid X.
fn point_cloud(attributes: u16) -> Vec<u8> {
    let mut wire = vec![
        0x50, 0, 0, 0, 5, 0, 0x30, 0, 2, 0, 0, 0, 3, 0, 0, 0, 0xc4, 9, 0, 0, 0x7d, 0, 0, 0, 0xfe,
        0xff, 0xff, 0xff, 0x0f, 0, 0, 0, 1, 0xa0, 0x0f, 0, 0, 2, 0, 0, 2, 0, 0, 0, 9, 0, 0, 0,
        0xff, 0xff, 0xff, 0xff, 7, 0, 0, 0, 0, 0x80, 0xff, 0xff, 0, 0, 1, 0, 2, 0, 0xfe, 0xff, 3,
        0, 4, 0, 0, 0x7d, 0, 0x83, 0xff, 0x7f, 0, 0,
    ];
    match attributes {
        32 => drop(wire.drain(40..56)),
        40 => drop(wire.drain(48..56)),
        48 => {}
        56 => {
            drop(wire.splice(56..56, [0xa5; 8]));
        }
        _ => panic!("unsupported fixture attribute size"),
    }
    let size = u32::try_from(wire.len()).unwrap();
    wire[..4].copy_from_slice(&size.to_le_bytes());
    wire[6..8].copy_from_slice(&attributes.to_le_bytes());
    wire
}

// LMI GoUniformSurfaceMsg_WriteV8, GoSurfaceIntensityMsg_WriteV9 and
// GoSurfacePointCloudMsg_WriteV8 at ff648a46. Compact wire IDs are 8, 9 and 28.
fn surface_record(type_id: u16) -> Vec<u8> {
    match type_id {
        8 => vec![
            0x50, 0x00, 0x00, 0x00, 0x08, 0x00, 0x3c, 0x00, 0x02, 0x00, 0x00, 0x00, 0x03, 0x00,
            0x00, 0x00, 0xc4, 0x09, 0x00, 0x00, 0x88, 0x13, 0x00, 0x00, 0x7d, 0x00, 0x00, 0x00,
            0xfe, 0xff, 0xff, 0xff, 0x03, 0x00, 0x00, 0x00, 0x0f, 0x00, 0x00, 0x00, 0x01, 0xa0,
            0x0f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00,
            0x09, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0x07, 0x00, 0x00, 0x00, 0x00, 0x80,
            0xff, 0xff, 0x00, 0x00, 0x01, 0x00, 0x00, 0x7d, 0xff, 0x7f,
        ],
        9 => vec![
            0x3e, 0x00, 0x00, 0x00, 0x09, 0x00, 0x30, 0x00, 0x02, 0x00, 0x00, 0x00, 0x03, 0x00,
            0x00, 0x00, 0xc4, 0x09, 0x00, 0x00, 0x88, 0x13, 0x00, 0x00, 0xfe, 0xff, 0xff, 0xff,
            0x03, 0x00, 0x00, 0x00, 0x02, 0x88, 0x13, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00,
            0x00, 0x00, 0x09, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0x07, 0x00, 0x00, 0x00,
            0x00, 0x01, 0x7f, 0x80, 0xfe, 0xff,
        ],
        28 => vec![
            0x62, 0x00, 0x00, 0x00, 0x1c, 0x00, 0x36, 0x00, 0x02, 0x00, 0x00, 0x00, 0x03, 0x00,
            0x00, 0x00, 0xc4, 0x09, 0x00, 0x00, 0x88, 0x13, 0x00, 0x00, 0x7d, 0x00, 0x00, 0x00,
            0xfe, 0xff, 0xff, 0xff, 0x03, 0x00, 0x00, 0x00, 0x0f, 0x00, 0x00, 0x00, 0x03, 0x70,
            0x17, 0x00, 0x00, 0x01, 0x03, 0x00, 0x00, 0x00, 0x09, 0x00, 0x00, 0x00, 0xff, 0xff,
            0xff, 0xff, 0x07, 0x00, 0x00, 0x00, 0x00, 0x80, 0xff, 0xff, 0x00, 0x00, 0x01, 0x00,
            0x02, 0x00, 0x00, 0x80, 0x2c, 0x01, 0xd4, 0xfe, 0x88, 0x13, 0xff, 0xff, 0xfe, 0xff,
            0xfd, 0xff, 0xff, 0x7f, 0x00, 0x80, 0x7b, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0x7f,
        ],
        _ => panic!("unsupported fixture surface type"),
    }
}

#[test]
fn unknown_stamp_and_uniform_versions_preserve_mixed_archives_and_export() {
    let home = Home::new("gocator-future-versions");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = listener.try_clone().unwrap();
    let known = group();
    let mut stamp = known[..70].to_vec();
    stamp[0..4].copy_from_slice(&158u32.to_le_bytes());
    stamp[6..10].copy_from_slice(&2u32.to_le_bytes());
    stamp[10..12].copy_from_slice(&72u16.to_le_bytes());
    stamp.extend_from_slice(&(u64::MAX - 7).to_le_bytes());
    stamp.extend_from_slice(&[0xa5; 8]);
    stamp.extend_from_slice(&known[14..70]);
    stamp.extend_from_slice(&(u64::MAX - 8).to_le_bytes());
    stamp.extend_from_slice(&[0x5a; 8]);
    assert_eq!(stamp.len(), 158);
    let mut uniform = known[70..130].to_vec();
    uniform[0..4].copy_from_slice(&76u32.to_le_bytes());
    uniform[6..8].copy_from_slice(&56u16.to_le_bytes());
    drop(uniform.splice(48..48, [0xa5; 16]));
    let wire = [stamp, uniform, known].concat();
    let original = wire.clone();
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = server.accept().unwrap();
        for bytes in wire.chunks(19) {
            stream.write_all(bytes).unwrap();
        }
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = [0];
        assert_eq!(stream.read(&mut request).unwrap(), 0);
    });
    assert_eq!(record(&home, &listener).success()["samples"], 1);
    worker.join().unwrap();
    let rows = rows(&home);
    let value = &rows[1]["value"];
    assert_eq!(fs::read(value["path"].as_str().unwrap()).unwrap(), original);
    let messages = value["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 5);
    assert_eq!(
        messages[0],
        json!({"type_id":1,"offset":0,"bytes":158,"last":false})
    );
    assert_eq!(
        messages[1],
        json!({"type_id":6,"offset":158,"bytes":76,"last":false})
    );
    assert_eq!(messages[2]["stamp"]["data_offset"], 248);
    assert_eq!(
        messages[2]["stamp"]["first"]["frame_index"],
        9_007_199_254_740_993u64
    );
    assert_eq!(messages[3]["uniform_profile"]["data_offset"], 352);
    assert_eq!(messages[3]["uniform_profile"]["width"], 3);
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "groups.jsonl",
        "--format",
        "jsonl",
    ]);
    let exported: Value = serde_json::from_str(
        fs::read_to_string(home.path("groups.jsonl"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(exported["value"], *value);
}

#[test]
fn explicit_group_limit_records_a_known_xyz_surface_larger_than_four_mib() {
    for limit in [None, Some("7e6")] {
        let home = Home::new("gocator-large-surface");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server = listener.try_clone().unwrap();
        let mut wire = surface_record(28);
        wire.resize(62 + 1024 * 1024 * 6, 0xa5);
        let length = u32::try_from(wire.len()).unwrap();
        wire[..4].copy_from_slice(&length.to_le_bytes());
        wire[4..6].copy_from_slice(&0x801cu16.to_le_bytes());
        wire[8..12].copy_from_slice(&1024u32.to_le_bytes());
        wire[12..16].copy_from_slice(&1024u32.to_le_bytes());
        let original = wire.clone();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            if limit.is_some() {
                stream.write_all(&wire).unwrap();
            } else {
                // A rejected declared size must close without reading its absent body.
                stream.write_all(&wire[..6]).unwrap();
            }
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = [0];
            assert_eq!(stream.read(&mut request).unwrap(), 0);
        });
        let result = record_options(&home, &listener, limit, "1500ms");
        if limit.is_some() {
            assert_eq!(result.success()["samples"], 1);
            let rows = rows(&home);
            let value = &rows[1]["value"];
            assert_eq!(value["bytes"], length);
            assert_eq!(fs::read(value["path"].as_str().unwrap()).unwrap(), original);
            let cloud = &value["messages"][0]["point_cloud_surface"];
            assert_eq!(cloud["data_offset"], 62);
            assert_eq!(cloud["rows"], 1024);
            assert_eq!(cloud["width"], 1024);
            assert_eq!(cloud["row_stride_bytes"], 6144);
            assert_eq!(cloud["components"], json!(["x", "y", "z"]));
        } else {
            let error = result.failure(1);
            assert_eq!(error["code"], "incomplete");
            assert_eq!(error["details"]["max_group_bytes"], 4 * 1024 * 1024);
            assert_eq!(error["details"]["required_bytes"], length);
            assert!(!home.path("profiles").exists());
        }
        worker.join().unwrap();
    }
}

#[test]
fn invalid_group_limits_fail_before_connecting() {
    for limit in ["5", "1.34217729e8", "6.0000000000000001", "-1", "\"7e6\""] {
        let home = Home::new("gocator-invalid-limit");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        record_options(&home, &listener, Some(limit), "500ms").failure(2);
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert!(!home.path("profiles").exists());
    }
}

#[test]
fn sdk_surfaces_preserve_height_intensity_xyz_and_opaque_records() {
    let home = Home::new("gocator-surfaces");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = listener.try_clone().unwrap();
    let mut opaque = surface_record(8);
    drop(opaque.splice(68..68, [0xa5; 8]));
    opaque[..4].copy_from_slice(&88u32.to_le_bytes());
    opaque[4..6].copy_from_slice(&0x8008u16.to_le_bytes());
    opaque[6..8].copy_from_slice(&68u16.to_le_bytes());
    let wire = [
        surface_record(8),
        surface_record(9),
        surface_record(28),
        opaque,
    ]
    .concat();
    let original = wire.clone();
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = server.accept().unwrap();
        for bytes in wire.chunks(13) {
            stream.write_all(bytes).unwrap();
        }
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = [0];
        assert_eq!(stream.read(&mut request).unwrap(), 0);
    });
    assert_eq!(record(&home, &listener).success()["samples"], 1);
    worker.join().unwrap();
    let rows = rows(&home);
    let value = &rows[1]["value"];
    assert_eq!(fs::read(value["path"].as_str().unwrap()).unwrap(), original);
    let messages = value["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    for (index, key, offset, stride) in [
        (0, "uniform_surface", 68, 6),
        (1, "surface_intensity", 136, 3),
        (2, "point_cloud_surface", 204, 18),
    ] {
        let surface = &messages[index][key];
        assert_eq!(surface["data_offset"], offset);
        assert_eq!(surface["rows"], 2);
        assert_eq!(surface["width"], 3);
        assert_eq!(surface["row_stride_bytes"], stride);
        assert_eq!(surface["byte_order"], "little");
        assert_eq!(surface["x_scale_nm"], 2500);
        assert_eq!(surface["y_scale_nm"], 5000);
        assert_eq!(surface["x_offset_um"], -2);
        assert_eq!(surface["y_offset_um"], 3);
        assert_eq!(surface["stream_step"], 3);
        assert_eq!(surface["stream_step_id"], 9);
        assert_eq!(surface["arrayed_count"], -1);
        assert_eq!(surface["arrayed_index"], 7);
        assert!(surface.get("exposure_ns").is_none());
    }
    assert_eq!(messages[0]["uniform_surface"]["sample_type"], "int16");
    assert_eq!(messages[0]["uniform_surface"]["invalid_code"], i16::MIN);
    assert_eq!(messages[0]["uniform_surface"]["z_scale_nm"], 125);
    assert_eq!(messages[0]["uniform_surface"]["z_offset_um"], 15);
    assert_eq!(messages[0]["uniform_surface"]["source_id"], 1);
    assert_eq!(messages[0]["uniform_surface"]["exposure_raw"], 4000);
    assert_eq!(messages[1]["surface_intensity"]["sample_type"], "uint8");
    assert_eq!(messages[1]["surface_intensity"]["source_id"], 2);
    assert_eq!(messages[1]["surface_intensity"]["exposure_raw"], 5000);
    assert_eq!(&original[136..142], &[0, 1, 127, 128, 254, 255]);
    let cloud = &messages[2]["point_cloud_surface"];
    assert_eq!(cloud["sample_type"], "int16");
    assert_eq!(cloud["components"], json!(["x", "y", "z"]));
    assert_eq!(cloud["organization"], "interleaved");
    assert_eq!(cloud["point_stride_bytes"], 6);
    assert_eq!(cloud["invalid_z_code"], i16::MIN);
    assert_eq!(cloud["adjacent_raw"], 1);
    assert_eq!(cloud["source_id"], 3);
    assert_eq!(cloud["exposure_raw"], 6000);
    assert_eq!(
        messages[3],
        json!({"type_id":8,"offset":240,"bytes":88,"last":true})
    );
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "surfaces.jsonl",
        "--format",
        "jsonl",
    ]);
    let exported: Value = serde_json::from_str(
        fs::read_to_string(home.path("surfaces.jsonl"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(exported["value"], *value);
}

#[test]
fn sdk_empty_records_survive_recording_and_export_without_invented_first_stamp() {
    let home = Home::new("gocator-empty");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = listener.try_clone().unwrap();
    let mut stamp = [0; 8];
    stamp[4..6].copy_from_slice(&64u16.to_le_bytes());
    let mut profile = point_cloud(48);
    profile.truncate(56);
    profile[..4].copy_from_slice(&56u32.to_le_bytes());
    profile[8..12].copy_from_slice(&0u32.to_le_bytes());
    profile[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut uniform = profile.clone();
    uniform[4..6].copy_from_slice(&0x8006u16.to_le_bytes());
    let wire = [message(1, false, &stamp), profile, uniform].concat();
    let original = wire.clone();
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = server.accept().unwrap();
        stream.write_all(&wire).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = [0];
        assert_eq!(stream.read(&mut request).unwrap(), 0);
    });
    assert_eq!(record(&home, &listener).success()["samples"], 1);
    worker.join().unwrap();
    let rows = rows(&home);
    let value = &rows[1]["value"];
    assert_eq!(fs::read(value["path"].as_str().unwrap()).unwrap(), original);
    let messages = &value["messages"];
    assert_eq!(
        messages[0]["stamp"],
        json!({"data_offset":14,"count":0,"stride_bytes":64,"source_id":0})
    );
    assert_eq!(messages[1]["point_cloud_profile"]["profiles"], 0);
    assert_eq!(messages[1]["point_cloud_profile"]["width"], u32::MAX);
    assert_eq!(messages[1]["point_cloud_profile"]["data_offset"], 70);
    assert_eq!(messages[2]["uniform_profile"]["profiles"], 0);
    assert_eq!(messages[2]["uniform_profile"]["width"], u32::MAX);
    assert_eq!(messages[2]["uniform_profile"]["data_offset"], 126);
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "groups.jsonl",
        "--format",
        "jsonl",
    ]);
    let exported: Value = serde_json::from_str(
        fs::read_to_string(home.path("groups.jsonl"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(exported["value"], *value);
}

#[test]
fn sdk_point_cloud_context_and_64_byte_stamps_keep_the_original_archive() {
    let home = Home::new("gocator-sdk-profile");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = listener.try_clone().unwrap();
    let legacy = group();
    let mut first = legacy[14..70].to_vec();
    first.extend_from_slice(&(u64::MAX - 7).to_le_bytes());
    let mut stamps = 2u32.to_le_bytes().to_vec();
    stamps.extend_from_slice(&64u16.to_le_bytes());
    stamps.extend_from_slice(&[1, 0]);
    stamps.extend_from_slice(&first);
    stamps.extend_from_slice(&first);
    let mut uniform = legacy[70..130].to_vec();
    uniform[..4].copy_from_slice(&68u32.to_le_bytes());
    uniform[6..8].copy_from_slice(&48u16.to_le_bytes());
    drop(uniform.splice(48..48, [(-1i32).to_le_bytes(), 7i32.to_le_bytes()].concat()));
    let mut opaque = point_cloud(56);
    opaque[5] = 0x80;
    let wire = [
        message(1, false, &stamps),
        uniform,
        point_cloud(32),
        point_cloud(40),
        point_cloud(48),
        opaque,
    ]
    .concat();
    let original = wire.clone();
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = server.accept().unwrap();
        for bytes in wire.chunks(17) {
            stream.write_all(bytes).unwrap();
        }
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = [0];
        assert_eq!(stream.read(&mut request).unwrap(), 0);
    });
    assert_eq!(record(&home, &listener).success()["samples"], 1);
    worker.join().unwrap();
    let rows = rows(&home);
    let value = &rows[1]["value"];
    assert_eq!(fs::read(value["path"].as_str().unwrap()).unwrap(), original);
    let messages = value["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 6);
    let stamp = &messages[0]["stamp"];
    assert_eq!(stamp["data_offset"], 14);
    assert_eq!(stamp["count"], 2);
    assert_eq!(stamp["stride_bytes"], 64);
    assert_eq!(stamp["first"]["ptp_timestamp_us"], u64::MAX - 7);
    assert_eq!(stamp["first"]["timestamp_us"], u64::MAX - 5);
    assert_eq!(messages[1]["uniform_profile"]["data_offset"], 198);
    assert_eq!(messages[1]["uniform_profile"]["exposure_raw"], 4000);
    assert!(messages[1]["uniform_profile"].get("exposure_ns").is_none());
    assert_eq!(messages[1]["uniform_profile"]["arrayed_count"], -1);
    assert_eq!(messages[1]["uniform_profile"]["arrayed_index"], 7);
    for (index, offset) in [(2, 250), (3, 322), (4, 402)] {
        let profile = &messages[index]["point_cloud_profile"];
        assert_eq!(profile["data_offset"], offset);
        assert_eq!(profile["profiles"], 2);
        assert_eq!(profile["width"], 3);
        assert_eq!(profile["sample_type"], "int16");
        assert_eq!(profile["byte_order"], "little");
        assert_eq!(profile["organization"], "interleaved");
        assert_eq!(profile["components"], json!(["x", "z"]));
        assert_eq!(profile["point_stride_bytes"], 4);
        assert_eq!(profile["invalid_x_code"], i16::MIN);
        assert_eq!(profile["x_scale_nm"], 2500);
        assert_eq!(profile["z_scale_nm"], 125);
        assert_eq!(profile["x_offset_um"], -2);
        assert_eq!(profile["z_offset_um"], 15);
        assert_eq!(profile["camera_index"], 2);
        assert_eq!(profile["exposure_raw"], 4000);
        assert!(profile.get("exposure_ns").is_none());
        assert_eq!(&original[offset..offset + 24], &point_cloud(48)[56..80]);
    }
    assert!(
        messages[2]["point_cloud_profile"]
            .get("stream_step")
            .is_none()
    );
    assert_eq!(messages[3]["point_cloud_profile"]["stream_step"], 2);
    assert_eq!(messages[4]["point_cloud_profile"]["arrayed_count"], -1);
    assert_eq!(messages[4]["point_cloud_profile"]["arrayed_index"], 7);
    assert_eq!(
        messages[5],
        json!({"type_id":5,"offset":426,"bytes":88,"last":true})
    );
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "groups.jsonl",
        "--format",
        "jsonl",
    ]);
    let exported: Value = serde_json::from_str(
        fs::read_to_string(home.path("groups.jsonl"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(exported["value"], *value);
}

#[test]
fn fragmented_gdp_resumes_after_idle_and_preserves_known_and_unknown_records() {
    let home = Home::new("gocator-gdp");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = listener.try_clone().unwrap();
    let wire = group();
    let original = wire.clone();
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = server.accept().unwrap();
        stream.write_all(&wire[..3]).unwrap();
        std::thread::sleep(Duration::from_millis(120));
        for bytes in wire[3..].chunks(11) {
            stream.write_all(bytes).unwrap();
        }
        stream.write_all(&wire).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = [0];
        assert_eq!(stream.read(&mut request).unwrap(), 0);
    });
    let response = record(&home, &listener).success();
    worker.join().unwrap();
    assert_eq!(response["samples"], 2);
    let rows = rows(&home);
    let sample = &rows[1];
    assert_eq!(sample["signal"], "profile_group");
    let value = &sample["value"];
    assert_eq!(value["kind"], "gocator_gdp_group");
    assert_eq!(value["group_start_confirmed"], false);
    assert_eq!(value["bytes"], original.len());
    assert_eq!(fs::read(value["path"].as_str().unwrap()).unwrap(), original);
    assert_eq!(value["sha256"].as_str().unwrap().len(), 64);
    let messages = value["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    let stamp = &messages[0]["stamp"];
    assert_eq!(stamp["first"]["frame_index"], 9_007_199_254_740_993u64);
    assert_eq!(stamp["first"]["timestamp_us"], u64::MAX - 5);
    assert_eq!(stamp["first"]["encoder_ticks"], -321);
    assert_eq!(stamp["data_offset"], 14);
    let profile = &messages[1]["uniform_profile"];
    assert_eq!(
        *profile,
        json!({
            "data_offset":118,"profiles":2,"width":3,"sample_type":"int16","byte_order":"little",
            "invalid_code":-32768,"x_scale_nm":2500,"z_scale_nm":125,"x_offset_um":-2,"z_offset_um":15,
            "source_id":1,"exposure_raw":4000,"stream_step":2,"stream_step_id":9
        })
    );
    assert_eq!(
        messages[2],
        json!({"type_id":0x123,"offset":130,"bytes":9,"last":true})
    );
    let second = &rows[2]["value"];
    assert_eq!(second["group_start_confirmed"], true);
    assert_ne!(second["path"], value["path"]);
    assert_eq!(second["messages"], value["messages"]);
    assert_eq!(
        fs::read(second["path"].as_str().unwrap()).unwrap(),
        original
    );
    let result = home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "groups.jsonl",
        "--format",
        "jsonl",
        "--source",
        "profiles",
    ]);
    assert_eq!(result["samples"], 2);
    assert_eq!(result["complete"], true);
    let exported = fs::read_to_string(home.path("groups.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    for (sample, value) in exported.iter().zip([value, second]) {
        assert_eq!(sample["source"], "profiles");
        assert_eq!(sample["signal"], "profile_group");
        assert_eq!(sample["value"], *value);
    }
    let csv = home.ok(&["data", "export", "run.jsonl", "-o", "groups.csv"]);
    assert_eq!(csv["samples"], 2);
}

#[test]
fn incomplete_gdp_groups_leave_only_previously_completed_artifacts() {
    let mut bad_known_stamp = group()[..70].to_vec();
    bad_known_stamp[4..6].copy_from_slice(&0x8001u16.to_le_bytes());
    bad_known_stamp[6..10].copy_from_slice(&2u32.to_le_bytes());
    let mut short_surface = surface_record(8);
    short_surface.pop();
    short_surface[..4].copy_from_slice(&79u32.to_le_bytes());
    short_surface[5] = 0x80;
    let mut overflowing_surface = surface_record(28);
    overflowing_surface[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
    overflowing_surface[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
    overflowing_surface[5] = 0x80;
    let mut short_point_cloud = point_cloud(48);
    short_point_cloud.pop();
    short_point_cloud[..4].copy_from_slice(&79u32.to_le_bytes());
    short_point_cloud[5] = 0x80;
    let mut overflowing_point_cloud = point_cloud(48);
    overflowing_point_cloud[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
    overflowing_point_cloud[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
    overflowing_point_cloud[5] = 0x80;
    let overflowing_profile = [
        40u16.to_le_bytes().as_slice(),
        u32::MAX.to_le_bytes().as_slice(),
        u32::MAX.to_le_bytes().as_slice(),
    ]
    .concat();
    let unterminated = message(0x123, false, &[]).repeat(128);
    for (suffix, expected) in [
        (
            bad_known_stamp,
            "Stamp length differs from its declared array",
        ),
        (short_surface, "Surface length differs from its geometry"),
        (
            overflowing_surface,
            "Surface length differs from its geometry",
        ),
        (vec![8, 0, 0], "unpublished group"),
        (short_point_cloud, "Point Cloud Profile length differs"),
        (
            overflowing_point_cloud,
            "Point Cloud Profile length differs",
        ),
        (
            message(6, true, &overflowing_profile),
            "length differs from its geometry",
        ),
        (unterminated, "exceeds 128 messages"),
        (
            message(6, true, &[32, 0]),
            "truncated Gocator GDP record metadata",
        ),
        (
            [4_194_305u32.to_le_bytes().as_slice(), &[6, 0x80]].concat(),
            "exceeds max_group_bytes",
        ),
    ] {
        let home = Home::new("gocator-gdp-incomplete");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server = listener.try_clone().unwrap();
        let wire = group();
        let original = wire.clone();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            stream.write_all(&wire).unwrap();
            stream.write_all(&suffix).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0];
            assert_eq!(stream.read(&mut request).unwrap(), 0);
        });
        let error = record(&home, &listener).failure(1);
        worker.join().unwrap();
        assert_eq!(error["code"], "incomplete");
        assert!(
            error["message"].as_str().unwrap().contains(expected),
            "{error}"
        );
        assert_eq!(
            fs::read(home.path("profiles/group-00000001.gdp")).unwrap(),
            original
        );
        assert_eq!(fs::read_dir(home.path("profiles")).unwrap().count(), 1);
        let rows = rows(&home);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2]["status"], "failed");
    }
}
