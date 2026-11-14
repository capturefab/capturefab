//! Explicit LMS `CoLa A` scans; legacy/default ASCII keeps its original schema.
//! SICK 8012471/1U1F pp120–126, 8013796/1NJF pp155–162 and
//! 8014631/1PIK §§4.3.6–4.3.7 define the widths and channel namespaces.
use super::{Fields, binary::Limits, io_error, malformed, remaining, unsupported};
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use serde_json::{Value, json};
use std::{io, time::Instant};

const MAX_JSON: usize = 1024 * 1024;

#[derive(Clone, Copy)]
pub(super) enum Model {
    Lms1xx,
    Lms5xx,
}

impl Model {
    const fn name(self) -> &'static str {
        match self {
            Self::Lms1xx => "lms1xx",
            Self::Lms5xx => "lms5xx",
        }
    }
    const fn echoes(self) -> u8 {
        match self {
            Self::Lms1xx => 2,
            Self::Lms5xx => 5,
        }
    }
}

pub(super) fn read_reply(
    mut read: impl FnMut(&mut [u8]) -> io::Result<usize>,
    deadline: Instant,
    model: Model,
    limits: Limits,
) -> Result<Value> {
    let mut frame = Vec::with_capacity(4096.min(limits.max_frame_bytes));
    let mut buffer = [0; 4096];
    loop {
        remaining(deadline)?;
        let room = limits
            .max_frame_bytes
            .saturating_sub(frame.len())
            .min(buffer.len());
        if room == 0 {
            return Err(malformed(
                "ASCII model scan exceeds configured max_frame_bytes",
            ));
        }
        let count = match read(&mut buffer[..room]) {
            Ok(0) => {
                return Err(Categorized::error(
                    ErrorKind::Disconnected,
                    "SICK CoLa peer closed during ASCII model scan",
                ));
            }
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error(&error)),
        };
        let previous = frame.len();
        frame.extend_from_slice(&buffer[..count]);
        if frame.first() != Some(&2) {
            return Err(malformed("ASCII model scan must start with one STX"));
        }
        if let Some(end) = buffer[..count].iter().position(|&byte| byte == 3) {
            if previous + end + 1 != frame.len() {
                return Err(malformed("extra bytes after ASCII model scan ETX"));
            }
            remaining(deadline)?;
            return decode(&frame, model, limits, deadline);
        }
    }
}

fn decode(frame: &[u8], model: Model, limits: Limits, deadline: Instant) -> Result<Value> {
    remaining(deadline)?;
    if frame.len() > limits.max_frame_bytes {
        return Err(malformed(
            "ASCII model scan exceeds configured max_frame_bytes",
        ));
    }
    let payload = frame
        .strip_prefix(&[2])
        .and_then(|frame| frame.strip_suffix(&[3]))
        .ok_or_else(|| malformed("ASCII model scan requires STX/ETX framing"))?;
    if !payload.iter().all(|byte| (0x20..=0x7e).contains(byte)) {
        return Err(malformed("ASCII model payload must be printable CoLa A"));
    }
    let payload =
        std::str::from_utf8(payload).map_err(|_| malformed("ASCII model payload is not ASCII"))?;
    let mut fields = Fields { payload, offset: 0 };
    fields.expect("sRA")?;
    fields.expect("LMDscandata")?;
    let mut value = scan_body(&mut fields, model, limits, frame.len(), deadline)?;
    value["encoding"] = json!("cola_a");
    value["model_profile"] = json!(model.name());
    value["raw_frame_hex"] = json!(crate::types::hex(frame));
    if serde_json::to_vec(&value)?.len() > MAX_JSON {
        return Err(malformed(
            "ASCII model scan exceeds one MiB JSON value budget",
        ));
    }
    remaining(deadline)?;
    Ok(value)
}

fn scan_body(
    fields: &mut Fields<'_>,
    model: Model,
    limits: Limits,
    raw_bytes: usize,
    deadline: Instant,
) -> Result<Value> {
    // Exact raw hex is charged before it is allocated. Sixteen KiB covers the
    // finite header/tail/channel metadata plus the containing Reading envelope.
    let mut budget = MAX_JSON
        .checked_sub(raw_bytes.saturating_mul(2).saturating_add(16 * 1024))
        .ok_or_else(|| malformed("ASCII model raw frame exceeds JSON budget"))?;
    let version: u16 = fields.number()?;
    // Current model manuals qualify version1. KA-08002 additionally publishes
    // version0 for LMS5xx PRO; it does not prove LMS1xx version0 compatibility.
    if version != 1 && !(version == 0 && matches!(model, Model::Lms5xx)) {
        return Err(unsupported(
            "ASCII scan version is not qualified for selected model",
        ));
    }
    let device_number: u16 = fields.number()?;
    let serial_number: u32 = fields.number()?;
    let device_status: [u8; 2] = [fields.number()?, fields.number()?];
    let telegram_counter: u16 = fields.number()?;
    let scan_counter: u16 = fields.number()?;
    let startup_us: u32 = fields.number()?;
    let transmit_us: u32 = fields.number()?;
    let digital_inputs_raw: [u8; 2] = [fields.number()?, fields.number()?];
    let digital_outputs_raw: [u8; 2] = [fields.number()?, fields.number()?];
    let layer_angle_raw: u16 = fields.number()?;
    if layer_angle_raw != 0 {
        return Err(unsupported(
            "ASCII model profile requires a single-layer scan",
        ));
    }
    let scan_frequency_raw: u32 = fields.number()?;
    let measurement_frequency_raw: u32 = fields.number()?;
    let encoder_count: u16 = fields.number()?;
    if encoder_count > 3 {
        return Err(unsupported("encoder count exceeds selected model profile"));
    }
    let mut encoders = Vec::with_capacity(usize::from(encoder_count));
    for _ in 0..encoder_count {
        let position_raw: u32 = fields.number()?;
        let speed_raw: u16 = fields.number()?;
        encoders.push(json!({"position_raw":position_raw,"speed_raw":speed_raw}));
    }
    let mut channels = Vec::new();
    for width in [16, 8] {
        let count: u16 = fields.number()?;
        let limit = u16::from(model.echoes()) * if width == 16 { 2 } else { 1 };
        // The shared primary namespace includes 16-bit DIST and RSSI. Model
        // tables' illustrative count sets do not cover every such combination.
        if count > limit {
            return Err(unsupported(
                "channel count exceeds selected model namespace",
            ));
        }
        for _ in 0..count {
            remaining(deadline)?;
            channels.push(channel(
                fields,
                width,
                model,
                limits,
                &mut budget,
                deadline,
            )?);
        }
    }
    fields.zero("position data")?;
    let device_name = match fields.number::<u16>()? {
        0 => None,
        1 => Some(fields.name()?.to_owned()),
        _ => return Err(malformed("ASCII optional-field flag must be zero or one")),
    };
    fields.zero("comment reserved in selected model profile")?;
    let calendar = match fields.number::<u16>()? {
        0 => None,
        1 => Some(calendar(fields)?),
        _ => return Err(malformed("ASCII optional-field flag must be zero or one")),
    };
    let event_count: u16 = fields.number()?;
    if event_count > 3 || (event_count > 0 && !matches!(model, Model::Lms5xx)) {
        return Err(unsupported("event count exceeds selected model profile"));
    }
    if event_count > 0 && encoder_count > 0 {
        return Err(malformed(
            "external encoder and event data are mutually exclusive",
        ));
    }
    let mut events = Vec::with_capacity(usize::from(event_count));
    for _ in 0..event_count {
        if fields.token()? != "FDIN" {
            return Err(unsupported("unsupported ASCII event type"));
        }
        let encoder_position_raw: u32 = fields.number()?;
        let startup_us: u32 = fields.number()?;
        let angle_raw = i32::from_be_bytes(fields.number::<u32>()?.to_be_bytes());
        events.push(
            json!({"type":"FDIN","encoder_position_raw":encoder_position_raw,
            "startup_us":startup_us,"angle_raw":angle_raw}),
        );
    }
    if fields.offset != fields.payload.len() {
        return Err(malformed("unexpected fields after ASCII model scan"));
    }
    remaining(deadline)?;
    let mut value = json!({"version":version,"device_number":device_number,
        "serial_number":serial_number,"device_status":device_status,
        "telegram_counter":telegram_counter,"scan_counter":scan_counter,
        "startup_us":startup_us,"transmit_us":transmit_us,
        "digital_inputs_raw":digital_inputs_raw,"digital_outputs_raw":digital_outputs_raw,
        "layer_angle_raw":layer_angle_raw,"scan_frequency_raw":scan_frequency_raw,
        "measurement_frequency_raw":measurement_frequency_raw,
        "encoders":encoders,"channels":channels,"events":events});
    if let Some(name) = device_name {
        value["device_name"] = json!(name);
    }
    if let Some(calendar) = calendar {
        value["calendar_raw"] = calendar;
    }
    Ok(value)
}

fn channel(
    fields: &mut Fields<'_>,
    width: u8,
    model: Model,
    limits: Limits,
    budget: &mut usize,
    deadline: Instant,
) -> Result<Value> {
    let name = fields.token()?;
    let [a, b, c, d, echo]: [u8; 5] = name
        .as_bytes()
        .try_into()
        .map_err(|_| unsupported("unknown ASCII model channel name"))?;
    let distance = match [a, b, c, d] {
        [b'D', b'I', b'S', b'T'] => true,
        [b'R', b'S', b'S', b'I'] => false,
        _ => return Err(unsupported("unknown ASCII model channel name")),
    };
    if width == 8 && distance {
        return Err(unsupported("eight-bit model channels require RSSI"));
    }
    if !(b'1'..=b'0' + model.echoes()).contains(&echo) {
        return Err(unsupported("channel echo exceeds selected model profile"));
    }
    let scale_bits: u32 = fields.number()?;
    let offset_bits: u32 = fields.number()?;
    let start_angle_raw = i32::from_be_bytes(fields.number::<u32>()?.to_be_bytes());
    let angle_step_raw: u16 = fields.number()?;
    let count = usize::from(fields.number::<u16>()?);
    if count > limits.max_points {
        return Err(malformed(
            "ASCII model channel exceeds configured max_points",
        ));
    }
    // Walk the bounded borrowed tokens once to validate and charge their exact
    // decimal JSON digits/separators before constructing any sample array.
    let mut probe = Fields {
        payload: fields.payload,
        offset: fields.offset,
    };
    let mut required = 2 + count.saturating_sub(1);
    for index in 0..count {
        if index.is_multiple_of(1024) {
            remaining(deadline)?;
        }
        let value: u16 = probe.number()?;
        if width == 8 && value > u16::from(u8::MAX) {
            return Err(malformed("ASCII sample exceeds its wire type"));
        }
        required += digits(value);
    }
    *budget = budget
        .checked_sub(required)
        .ok_or_else(|| malformed("ASCII sample arrays exceed JSON budget"))?;
    remaining(deadline)?;
    let mut values = Vec::with_capacity(count);
    for index in 0..count {
        if index.is_multiple_of(1024) {
            remaining(deadline)?;
        }
        values.push(fields.number::<u16>()?);
    }
    let mut value = json!({"name":name,"sample_bits":width,"scale_bits":scale_bits,
        "offset_bits":offset_bits,"start_angle_raw":start_angle_raw,
        "angle_step_raw":angle_step_raw,"values":values});
    if distance {
        value["scaled_unit"] = json!("mm");
    }
    Ok(value)
}

const fn digits(value: u16) -> usize {
    match value {
        0..=9 => 1,
        10..=99 => 2,
        100..=999 => 3,
        1000..=9999 => 4,
        _ => 5,
    }
}

fn calendar(fields: &mut Fields<'_>) -> Result<Value> {
    let year: u16 = fields.number()?;
    let month: u8 = fields.number()?;
    let day: u8 = fields.number()?;
    let hour: u8 = fields.number()?;
    let minute: u8 = fields.number()?;
    let second: u8 = fields.number()?;
    let microsecond: u32 = fields.number()?;
    // Match the existing binary raw calendar component contract, including
    // documented zero month/day values; no inferred UTC/epoch/synchronization.
    if !(1970..=2037).contains(&year)
        || month > 12
        || day > 31
        || hour > 23
        || minute > 59
        || second > 59
        || microsecond > 999_999
    {
        return Err(malformed(
            "ASCII calendar component exceeds documented width/range",
        ));
    }
    Ok(json!({"year":year,"month":month,"day":day,"hour":hour,
        "minute":minute,"second":second,"microsecond":microsecond}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest as _, Sha256};
    use std::{fmt::Write as _, io::Read as _, time::Duration};

    const PUBLISHED: &[u8] = include_bytes!("../../../tests/fixtures/sick/lms-published-sra.bin");
    const HEADER: &str =
        "sRA LMDscandata 1 FFFF FFFFFFFF FF 80 FFFF 0 FFFFFFFF 0 12 34 AB CD 0 9C4 21C0";
    const DIST: &str = "DIST1 40000000 BF800000 FFFF3CB0 683 4 0 1 F FFFF";

    fn frame(payload: &str) -> Vec<u8> {
        format!("\x02{payload}\x03").into_bytes()
    }
    fn reply(raw: &[u8], model: Model) -> Result<Value> {
        let mut input = io::Cursor::new(raw);
        read_reply(
            |bytes| input.read(&mut bytes[..1]),
            Instant::now() + Duration::from_secs(1),
            model,
            Limits::default(),
        )
    }

    #[test]
    fn exact_published_lms_ascii_reply_is_a_positive_for_both_model_profiles() {
        assert_eq!(PUBLISHED.len(), 215);
        assert_eq!(
            crate::types::hex(Sha256::digest(PUBLISHED)),
            "ca81f93e9f85e0164076d8459e9c50324b07ea22bbcef1d3ca7838bcb4acf95d"
        );
        for model in [Model::Lms1xx, Model::Lms5xx] {
            let value = reply(PUBLISHED, model).unwrap();
            assert_eq!(value["encoding"], "cola_a");
            assert_eq!(value["model_profile"], model.name());
            assert_eq!(value["raw_frame_hex"], crate::types::hex(PUBLISHED));
            assert_eq!(
                value["channels"][0]["values"],
                json!([
                    2209, 2213, 2219, 2220, 2214, 2220, 2230, 2248, 2242, 2249, 2251, 2244, 2276,
                    2273, 2283, 2272, 2293, 2312, 2300, 2311, 2310
                ])
            );
            assert_eq!(value["channels"][0]["start_angle_raw"], 100_000);
            assert_eq!(value["channels"][0]["angle_step_raw"], 5000);
            assert_eq!(value["encoders"], json!([]));
            assert_eq!(value["events"], json!([]));
            assert!(value.get("calendar_raw").is_none());
        }
    }

    #[test]
    fn independent_channels_raw_status_name_calendar_and_three_encoders_are_preserved() {
        let payload = format!(
            "{HEADER} 3 FFFFFFFF FFFF FFFFFFFE FFFE FFFFFFFD FFFD 3 {DIST} DIST2 3F800000 0 FFFF3CB1 684 1 FFFF RSSI2 7FC12345 80000000 80000000 FFFF 2 0 FFFF 1 RSSI1 3F800000 0 FFFF3CBA 68D 3 0 1 FF 0 1 5 A B   0 1 7F5 0 0 17 3B 3B F423F 0"
        );
        let value = reply(&frame(&payload), Model::Lms1xx).unwrap();
        assert_eq!(value["serial_number"], u32::MAX);
        assert_eq!(value["device_status"], json!([255, 128]));
        assert_eq!(value["digital_outputs_raw"], json!([171, 205]));
        assert_eq!(
            value["encoders"][2],
            json!({"position_raw":4_294_967_293_u32,"speed_raw":65_533})
        );
        assert_eq!(value["channels"].as_array().unwrap().len(), 4);
        assert_eq!(value["channels"][0]["values"], json!([0, 1, 15, 65_535]));
        assert_eq!(value["channels"][0]["scaled_unit"], "mm");
        assert_eq!(value["channels"][2]["scale_bits"], 0x7fc1_2345_u32);
        assert_eq!(value["channels"][2]["offset_bits"], 0x8000_0000_u32);
        assert_eq!(value["channels"][2]["start_angle_raw"], i32::MIN);
        assert!(value["channels"][2].get("scaled_unit").is_none());
        assert_eq!(value["channels"][3]["sample_bits"], 8);
        assert_eq!(value["channels"][3]["values"], json!([0, 1, 255]));
        assert_eq!(value["device_name"], "A B  ");
        assert_eq!(
            value["calendar_raw"],
            json!({"year":2037,"month":0,"day":0,"hour":23,
            "minute":59,"second":59,"microsecond":999_999})
        );
    }

    #[test]
    fn documented_lms5_version_zero_and_pro_event_body_are_qualified_without_relabeling_sns() {
        // Symbolic body from SICK KA-08002's two-event example. Printed visual
        // line wraps are joined, not presented as captured/framed wire bytes.
        let body = "0 1 D5EF4F 0 0 D3A3 D4C1 42C154D5 42C19EB7 8 0 3F 0 0 1388 168 0 1 DIST1 40000000 00000000 D6D80 1388 5 348 343 344 348 347 0 0 1 B SN 14020431 0 1 7E4 6 8 C 21 1D F618 2 FDIN 0 42C15A44 CE77 FDIN 0 42C15A1A B264";
        let mut fields = Fields {
            payload: body,
            offset: 0,
        };
        let value = scan_body(
            &mut fields,
            Model::Lms5xx,
            Limits::default(),
            body.len() + 19,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(value["version"], 0);
        assert_eq!(value["device_name"], "SN 14020431");
        assert_eq!(value["channels"][0]["scale_bits"], 0x4000_0000_u32);
        assert_eq!(
            value["events"],
            json!([
            {"type":"FDIN","encoder_position_raw":0,"startup_us":1_119_967_812_u32,"angle_raw":52_855},
            {"type":"FDIN","encoder_position_raw":0,"startup_us":1_119_967_770_u32,"angle_raw":45_668}])
        );
        let raw = frame(&format!("sSN LMDscandata {body}"));
        assert_eq!(
            crate::types::classify(&reply(&raw, Model::Lms5xx).unwrap_err()).kind,
            ErrorKind::OperationFailed
        );
        let raw = frame(&format!("sRA LMDscandata {body}"));
        assert_eq!(
            crate::types::classify(&reply(&raw, Model::Lms1xx).unwrap_err()).kind,
            ErrorKind::Unsupported
        );
    }

    #[test]
    fn optional_empty_fields_and_each_pro_event_count_preserve_wire_order() {
        let raw = frame(&format!("{HEADER} 0 0 0 0 1 0  0 0 0"));
        let value = reply(&raw, Model::Lms1xx).unwrap();
        assert_eq!(value["channels"], json!([]));
        assert_eq!(value["device_name"], "");
        for count in 0..=3 {
            let mut events = String::new();
            for index in 0..count {
                write!(events, " FDIN FFFFFFFF {:X} FFFFFFFF", u32::MAX - index).unwrap();
            }
            let payload = format!("{HEADER} 0 1 {DIST} 0 0 0 0 0 {count}{events}");
            let value = reply(&frame(&payload), Model::Lms5xx).unwrap();
            assert_eq!(
                value["events"].as_array().unwrap().len(),
                usize::try_from(count).unwrap()
            );
            for (index, event) in value["events"].as_array().unwrap().iter().enumerate() {
                assert_eq!(event["angle_raw"], -1);
                assert_eq!(
                    event["startup_us"],
                    u32::MAX - u32::try_from(index).unwrap()
                );
            }
        }
        let payload = format!("{HEADER} 1 FFFFFFFF FFFF 1 {DIST} 0 0 0 0 0 1 FDIN 0 0 0");
        assert!(
            reply(&frame(&payload), Model::Lms5xx)
                .unwrap_err()
                .to_string()
                .contains("mutually exclusive")
        );
    }

    #[test]
    fn malformed_fields_and_unqualified_namespaces_tails_are_whole_scan_failures() {
        let base = format!("{HEADER} 0 1 {DIST} 0 0 0 0 0 0");
        for bad in [
            base.replace(" 1 FFFF ", " 2 FFFF "),
            base.replace("DIST1", "DIST3"),
            base.replace(" 4 0 1 F FFFF", " 4 0 1 F 10000"),
            base.replace(" 0 0 0 0 0 0", " 1 DIST1 3F800000 0 0 1 1 0 0 0 0 0 0"),
            base.replace(" 0 0 0 0 0 0", " 0 1 0 0 0 0"),
            base.replace(" 0 0 0 0 0 0", " 0 0 2 0 0 0"),
            base.replace(" 0 0 0 0 0 0", " 0 0 1 10 TOO-LONG 0 0 0"),
            base.replace(" 0 0 0 0 0 0", " 0 0 1 3 AB 0 0 0"),
            base.replace(" 0 0 0 0 0 0", " 0 0 0 1 0 0"),
            base.replace(" 0 0 0 0 0 0", " 0 0 0 0 1 7E8 1 1 0 0 0 F4240 0"),
            base.replace(" 0 0 0 0 0 0", " 0 0 0 0 0 1 FDIN 0 0 0"),
            format!("{base} 0"),
            base.replace("40000000", "4000000G"),
        ] {
            assert!(
                reply(&frame(&bad), Model::Lms1xx)
                    .unwrap_err()
                    .to_string()
                    .contains("SICK CoLa")
            );
        }
    }

    #[test]
    fn exact_array_charge_occurs_before_materialization_and_before_next_channel() {
        let body = "RSSI1 3F800000 0 FFFFFFFF 1 4 0 9 A FF NEXT";
        let mut fields = Fields {
            payload: body,
            offset: 0,
        };
        let values_at = body.find("0 9 A FF").unwrap();
        let mut budget = 12; // [0,9,10,255] is exactly12bytes, including commas.
        let value = channel(
            &mut fields,
            8,
            Model::Lms1xx,
            Limits::default(),
            &mut budget,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(budget, 0);
        assert_eq!(value["values"], json!([0, 9, 10, 255]));
        assert_eq!(fields.token().unwrap(), "NEXT");
        let mut fields = Fields {
            payload: body,
            offset: 0,
        };
        let mut budget = 11;
        let error = channel(
            &mut fields,
            8,
            Model::Lms1xx,
            Limits::default(),
            &mut budget,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("JSON budget"));
        assert_eq!(fields.offset, values_at); // Probe did not consume/publish an array.
    }

    #[test]
    fn last_field_cannot_extend_the_original_deadline_and_limits_are_exact() {
        let end = Instant::now() + Duration::from_millis(10);
        let mut offered = false;
        let error = read_reply(
            |out| {
                assert!(!offered);
                offered = true;
                std::thread::sleep(Duration::from_millis(25));
                out[..PUBLISHED.len()].copy_from_slice(PUBLISHED);
                Ok(PUBLISHED.len())
            },
            end,
            Model::Lms1xx,
            Limits::default(),
        )
        .unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
        let exact = Limits::new(Some(PUBLISHED.len()), Some(21)).unwrap();
        decode(
            PUBLISHED,
            Model::Lms1xx,
            exact,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        let small = Limits::new(Some(PUBLISHED.len() - 1), Some(21)).unwrap();
        assert!(
            decode(
                PUBLISHED,
                Model::Lms1xx,
                small,
                Instant::now() + Duration::from_secs(1)
            )
            .unwrap_err()
            .to_string()
            .contains("max_frame_bytes")
        );
        let small = Limits::new(Some(PUBLISHED.len()), Some(20)).unwrap();
        assert!(
            decode(
                PUBLISHED,
                Model::Lms1xx,
                small,
                Instant::now() + Duration::from_secs(1)
            )
            .unwrap_err()
            .to_string()
            .contains("max_points")
        );
    }
}
