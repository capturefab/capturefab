//! Text formatting for values, statistics and addresses.
use super::*;

pub(super) fn feature_value(feature: &FeatureInfo) -> String {
    feature
        .value
        .as_ref()
        .map(value_text)
        .unwrap_or_else(|| "—".into())
}

/// A GenICam name as words: "AcquisitionFrameRate" → "Acquisition Frame Rate",
/// keeping acronyms together: "GevSCPSPacketSize" → "Gev SCPS Packet Size".
pub(super) fn words(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 4);
    for (i, &c) in chars.iter().enumerate() {
        if i > 0 && c != '_' {
            let previous = chars[i - 1];
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            let boundary = (c.is_uppercase()
                && (previous.is_lowercase() || (previous.is_uppercase() && next_lower)))
                || (c.is_ascii_digit() && previous.is_lowercase());
            if boundary && !out.ends_with(' ') {
                out.push(' ');
            }
        }
        out.push(if c == '_' { ' ' } else { c });
    }
    out
}

pub(super) fn capitalize(value: &str) -> String {
    let mut chars = value.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// 1204 → "1,204".
pub(super) fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub(super) fn feature_group(name: &str) -> &'static str {
    if [
        "Width",
        "Height",
        "Offset",
        "Pixel",
        "Binning",
        "Decimation",
        "Reverse",
        "Sensor",
        "TestPattern",
    ]
    .iter()
    .any(|p| name.starts_with(p))
    {
        "Image"
    } else if [
        "Acquisition",
        "Exposure",
        "Gain",
        "Trigger",
        "Balance",
        "Black",
        "Gamma",
        "Analog",
        "Digital",
        "Auto",
    ]
    .iter()
    .any(|p| name.starts_with(p))
    {
        "Acquisition"
    } else if ["Device", "UserSet", "Camera", "Temperature"]
        .iter()
        .any(|p| name.starts_with(p))
    {
        "Device"
    } else if ["Gev", "U3V", "TL", "Stream", "Payload", "Packet"]
        .iter()
        .any(|p| name.starts_with(p))
    {
        "Transport"
    } else {
        "Other"
    }
}

/// A feature value as text that parses back to the same value, so it can be
/// edited and sent as is. Whole floats read as whole numbers, 10000.0 as
/// "10000"; a fraction or an exponent stays.
pub(super) fn value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Number(value) => {
            let text = value.to_string();
            match text.strip_suffix(".0") {
                Some("-0") => "0".into(),
                Some(whole) => whole.into(),
                None => text,
            }
        }
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Null => "—".into(),
        _ => value.to_string(),
    }
}

/// A GenICam unit as people write it: "us" → "µs".
pub(super) fn unit_label(unit: &str) -> &str {
    match unit {
        "us" | "usec" => "µs",
        other => other,
    }
}

/// A bound in a range caption: whole numbers from 10,000 up are grouped,
/// "1,000,000"; the rest read as `number` has them. For captions only;
/// fields keep plain digits, so they parse.
pub(super) fn range_number(value: f64) -> String {
    if value.fract() == 0.0 && (10_000.0..1e15).contains(&value.abs()) {
        let digits = grouped(value.abs() as u64);
        if value < 0.0 {
            format!("-{digits}")
        } else {
            digits
        }
    } else {
        number(value)
    }
}

/// A feature's range and unit for the caption under its field, such as
/// "10 – 1,000,000 µs"; empty when it has neither.
pub(super) fn range_text(feature: &FeatureInfo) -> String {
    let range = match (feature.min, feature.max) {
        (Some(min), Some(max)) => format!("{} – {}", range_number(min), range_number(max)),
        (Some(min), None) => format!("min {}", range_number(min)),
        (None, Some(max)) => format!("max {}", range_number(max)),
        _ => String::new(),
    };
    match feature.unit.as_deref().map(unit_label) {
        Some(unit) if range.is_empty() => unit.to_owned(),
        Some(unit) => format!("{range} {unit}"),
        None => range,
    }
}

/// Features a camera cannot change while it streams: the image's geometry
/// and format.
pub(super) fn stream_locked(name: &str) -> bool {
    matches!(
        name,
        "Width"
            | "Height"
            | "OffsetX"
            | "OffsetY"
            | "PixelFormat"
            | "BinningHorizontal"
            | "BinningVertical"
            | "DecimationHorizontal"
            | "DecimationVertical"
            | "VideoMode"
    )
}

/// Where a feature sorts within its group: in the order a camera is set up,
/// then everything else.
pub(super) fn feature_rank(name: &str) -> usize {
    const ORDER: [&str; 14] = [
        "Width",
        "Height",
        "OffsetX",
        "OffsetY",
        "PixelFormat",
        "Binning",
        "Decimation",
        "AcquisitionStart",
        "AcquisitionMode",
        "AcquisitionFrameRate",
        "ExposureAuto",
        "ExposureTime",
        "GainAuto",
        "Gain",
    ];
    ORDER
        .iter()
        .position(|prefix| name.starts_with(prefix))
        .unwrap_or(ORDER.len())
}

pub(super) fn auto_summary(status: &AutoStatus) -> String {
    let strategy = match status.strategy.as_str() {
        "firmware" => "Camera auto exposure",
        "software" => "Capturefab exposure",
        "video-mode" => "Video mode",
        "none" => "Frame rate only",
        other => other,
    };
    std::iter::once(strategy.to_owned())
        .chain(status.exposure_us.map(|us| {
            if us < 1000.0 {
                format!("{us:.0} µs")
            } else {
                format!("{:.1} ms", us / 1000.0)
            }
        }))
        .chain(status.gain_db.map(|db| format!("{db:.1} dB")))
        .chain(status.target_fps.map(|fps| format!("{fps:.0} fps")))
        .collect::<Vec<_>>()
        .join(" · ")
}

pub(super) fn change_text(change: &AutoChange, unit: Option<&str>) -> String {
    format!(
        "{} {} {} → {}{}",
        change.time,
        change.feature,
        value_text(change.from.as_ref().unwrap_or(&serde_json::Value::Null)),
        value_text(&change.to),
        unit.map(|u| format!(" {}", unit_label(u)))
            .unwrap_or_default()
    )
}

pub(super) fn transport_loss(stats: &TransportStats) -> u64 {
    stats.incomplete_frames + stats.lost_frames
}

pub(super) fn transport_text(stats: &TransportStats) -> String {
    format!(
        "{}resend {}/{} · {} lost",
        stats
            .packet_size
            .map(|size| format!("{size} B packets · "))
            .unwrap_or_default(),
        stats.resend_recovered,
        stats.resend_requested,
        transport_loss(stats)
    )
}

/// What a sparkline shows: `what` over its span, its range as `value`
/// formats it, and the marked samples where `flagged` happened.
pub(super) fn trend(
    history: &super::sparkline::History,
    what: &str,
    value: impl Fn(f32) -> String,
    flagged: &str,
) -> String {
    let mut about = format!("{what}, last {} s", history.seconds().max(1));
    if let Some((lo, hi)) = history.range() {
        about += &format!(": {} to {}", value(lo), value(hi));
    }
    match history.flagged() {
        0 => about,
        n => format!("{about}\nMarked: {flagged} ({n})"),
    }
}

pub(super) fn number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.3}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned()
    }
}

pub(super) fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

pub(super) fn pixel_name(pixel_format: u32) -> String {
    match pixel_format {
        MONO8 => "Mono8".into(),
        RGB8 => "RGB8".into(),
        0x0218_0015 => "BGR8".into(),
        0x0110_0003 => "Mono10".into(),
        0x0110_0005 => "Mono12".into(),
        0x0110_0007 => "Mono16".into(),
        0x0108_0008 => "BayerGR8".into(),
        0x0108_0009 => "BayerRG8".into(),
        0x0108_000a => "BayerGB8".into(),
        0x0108_000b => "BayerBG8".into(),
        _ => format!("0x{pixel_format:08x}"),
    }
}

pub(super) fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
        && !value.is_empty()
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

pub(super) fn redact_address(value: &str) -> String {
    let Some((scheme, rest)) = value.split_once("://") else {
        return value.into();
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let safe_tail = tail.split(['?', '#']).next().unwrap_or(tail);
    let suffix = if safe_tail.len() != tail.len() {
        "?[redacted]"
    } else {
        ""
    };
    if let Some((_, host)) = authority.rsplit_once('@') {
        format!("{scheme}://[redacted]@{host}{safe_tail}{suffix}")
    } else {
        format!("{scheme}://{authority}{safe_tail}{suffix}")
    }
}

/// A short span of time: "4 s", "3 min", "2 h".
pub(super) fn span(duration: Duration) -> String {
    match duration.as_secs() {
        seconds @ 0..60 => format!("{seconds} s"),
        seconds @ 60..3600 => format!("{} min", seconds / 60),
        seconds => format!("{} h", seconds / 3600),
    }
}

/// What a stalled camera's badge, row and pill say: "No new frame for 6 s".
pub(super) fn silence(silent: Duration) -> String {
    format!(
        "No new frame for {}",
        span(silent.max(Duration::from_secs(1)))
    )
}

/// A writable feature named `name` of GenICam `kind` holding `value`, for tests.
#[cfg(test)]
pub(super) fn sample_feature(name: &str, kind: &str, value: serde_json::Value) -> FeatureInfo {
    FeatureInfo {
        name: name.into(),
        display_name: String::new(),
        kind: kind.into(),
        value: Some(value),
        writable: true,
        description: String::new(),
        unit: None,
        min: None,
        max: None,
        inc: None,
        choices: Vec::new(),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spans_read_in_the_largest_whole_unit() {
        let ms = Duration::from_millis;
        assert_eq!(span(ms(6400)), "6 s");
        assert_eq!(span(Duration::from_secs(130)), "2 min");
        assert_eq!(span(Duration::from_secs(7300)), "2 h");
        assert_eq!(silence(ms(6400)), "No new frame for 6 s");
        assert_eq!(silence(ms(300)), "No new frame for 1 s");
    }

    #[test]
    fn trends_name_their_span_range_and_marks() {
        let start = Instant::now();
        let mut history = super::super::sparkline::History::default();
        history.offer(start, 30.0, 0);
        history.offer(start + Duration::from_secs(2), 24.5, 3);
        assert_eq!(
            trend(
                &history,
                "Frame rate",
                |v| format!("{v:.1} fps"),
                "lost frames"
            ),
            "Frame rate, last 2 s: 24.5 fps to 30.0 fps\nMarked: lost frames (1)"
        );
    }

    #[test]
    fn auto_feature_grouping_and_text() {
        assert_eq!(feature_group("AutoExposureTimeUpperLimit"), "Acquisition");
        assert_eq!(feature_group("AutoFunctionProfile"), "Acquisition");
        let status = AutoStatus {
            strategy: "firmware".into(),
            exposure_us: Some(8200.0),
            gain_db: Some(3.06),
            target_fps: Some(45.2),
            ..Default::default()
        };
        assert_eq!(
            auto_summary(&status),
            "Camera auto exposure · 8.2 ms · 3.1 dB · 45 fps"
        );
        let status = AutoStatus {
            strategy: "software".into(),
            exposure_us: Some(250.0),
            ..Default::default()
        };
        assert_eq!(auto_summary(&status), "Capturefab exposure · 250 µs");
        let change = AutoChange {
            time: "12:00:01".into(),
            feature: "AutoExposureTimeUpperLimit".into(),
            from: Some(json!(5000)),
            to: json!(19800.5),
            reason: "limits".into(),
        };
        assert_eq!(
            change_text(&change, Some("us")),
            "12:00:01 AutoExposureTimeUpperLimit 5000 → 19800.5 µs"
        );
        let change = AutoChange {
            from: None,
            feature: "ExposureAuto".into(),
            to: json!("Continuous"),
            ..change
        };
        assert_eq!(
            change_text(&change, None),
            "12:00:01 ExposureAuto — → Continuous"
        );
    }

    #[test]
    fn transport_line_and_loss() {
        let stats = TransportStats {
            packet_size: Some(1500),
            resend_requested: 14,
            resend_recovered: 12,
            incomplete_frames: 2,
            lost_frames: 1,
            ..Default::default()
        };
        assert_eq!(transport_loss(&stats), 3);
        assert_eq!(
            transport_text(&stats),
            "1500 B packets · resend 12/14 · 3 lost"
        );
        assert_eq!(
            transport_text(&TransportStats::default()),
            "resend 0/0 · 0 lost"
        );
    }

    #[test]
    fn feature_names_read_as_words() {
        assert_eq!(words("AcquisitionFrameRate"), "Acquisition Frame Rate");
        assert_eq!(words("GevSCPSPacketSize"), "Gev SCPS Packet Size");
        assert_eq!(words("OffsetX"), "Offset X");
        assert_eq!(words("PixelFormat"), "Pixel Format");
        assert_eq!(words("U3VMaxPacket"), "U3V Max Packet");
        assert_eq!(words("Gain"), "Gain");
        assert_eq!(words("Device_Temperature"), "Device Temperature");
    }

    #[test]
    fn numbers_group_and_capitalize() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1204), "1,204");
        assert_eq!(grouped(10_000_000), "10,000,000");
        assert_eq!(capitalize("stable"), "Stable");
        assert_eq!(capitalize(""), "");
    }

    #[test]
    fn whole_floats_read_as_whole_numbers_and_parse_back() {
        assert_eq!(value_text(&json!(10000.0)), "10000");
        assert_eq!(value_text(&json!(30.0)), "30");
        assert_eq!(value_text(&json!(0.0)), "0");
        assert_eq!(value_text(&json!(-0.0)), "0");
        assert_eq!(value_text(&json!(-4.0)), "-4");
        assert_eq!(value_text(&json!(2.5)), "2.5");
        assert_eq!(value_text(&json!(15000)), "15000");
        assert_eq!(value_text(&json!(1e21)), "1e+21");
        assert_eq!(value_text(&json!("Mono8")), "Mono8");
        assert_eq!(value_text(&json!(true)), "true");
        for value in [
            10000.0,
            0.1,
            -0.5,
            30.0,
            1e15,
            1e16,
            123456.789,
            f64::MAX,
            f64::MIN_POSITIVE,
            -1e-7,
        ] {
            let text = value_text(&json!(value));
            assert_eq!(text.parse::<f64>().ok(), Some(value), "{text}");
        }
        // The draft a write leaves matches the camera's value once read back.
        let feature = sample_feature("ExposureTime", "Float", json!(15000.0));
        assert_eq!(feature_value(&feature), "15000");
    }

    #[test]
    fn ranges_read_with_grouped_digits_and_real_units() {
        let feature = |min, max, unit: Option<&str>| FeatureInfo {
            min,
            max,
            unit: unit.map(str::to_owned),
            ..sample_feature("ExposureTime", "Float", json!(1))
        };
        assert_eq!(
            range_text(&feature(Some(10.0), Some(1_000_000.0), Some("us"))),
            "10 – 1,000,000 µs"
        );
        assert_eq!(
            range_text(&feature(Some(0.0), Some(24.0), Some("dB"))),
            "0 – 24 dB"
        );
        assert_eq!(
            range_text(&feature(Some(-20_000.0), Some(0.5), None)),
            "-20,000 – 0.5"
        );
        assert_eq!(range_text(&feature(None, Some(9999.0), None)), "max 9999");
        assert_eq!(range_text(&feature(None, None, Some("Hz"))), "Hz");
        assert_eq!(range_text(&feature(None, None, None)), "");
        assert_eq!(range_number(1e20), number(1e20), "too large to group");
        assert_eq!(unit_label("Hz"), "Hz");
    }

    #[test]
    fn features_sort_the_way_a_camera_is_set_up() {
        let mut names = vec![
            "Height",
            "PixelFormat",
            "Width",
            "TestPattern",
            "OffsetX",
            "WidthMax",
        ];
        names.sort_by_key(|name| feature_rank(name));
        assert_eq!(
            names,
            [
                "Width",
                "WidthMax",
                "Height",
                "OffsetX",
                "PixelFormat",
                "TestPattern"
            ]
        );
        let mut names = vec![
            "Gain",
            "AcquisitionStart",
            "ExposureTime",
            "AcquisitionFrameRate",
            "GainAuto",
            "ExposureAuto",
            "BlackLevel",
        ];
        names.sort_by_key(|name| feature_rank(name));
        assert_eq!(
            names,
            [
                "AcquisitionStart",
                "AcquisitionFrameRate",
                "ExposureAuto",
                "ExposureTime",
                "GainAuto",
                "Gain",
                "BlackLevel"
            ]
        );
        assert!(stream_locked("PixelFormat") && !stream_locked("Gain"));
    }

    #[test]
    fn addresses_hide_credentials_and_queries() {
        assert_eq!(
            redact_address("rtsp://user:pass@cam.local:554/stream?token=1"),
            "rtsp://[redacted]@cam.local:554/stream?[redacted]"
        );
        assert_eq!(redact_address("192.168.1.20"), "192.168.1.20");
    }
}
