//! Explicit Siemens fixed-layout temporal data, without a presumed clock basis.
//!
//! STEP7 DATE/TIME/TOD/DT/DTL definitions and the S7-1200 manual:
//! <https://docs.tia.siemens.cloud/r/en-us/v21/data-types/date-and-time/date>
//! <https://docs.tia.siemens.cloud/r/en-us/v20/data-types/date-and-time/date_and_time-date-and-time-of-day>
//! <https://docs.tia.siemens.cloud/r/simatic_s7_1200_manual_collection_eses_20/plc-concepts/data-types/time-and-date-data-types>
//! DTL's 2262 conversion boundary is recommended, not an absolute storage limit.
//! The older Siemens HMI structure also documents years through 2554:
//! <https://docs.tia.siemens.cloud/r/en-us/v20/communicating-with-plcs-basic-panels-panels-comfort-panels-rt-advanced-rt-professional/communicating-with-simatic-s7-1200-basic-panels-panels-comfort-panels-rt-advanced-rt-professional/data-exchange-basic-panels-panels-comfort-panels-rt-advanced-rt-professional/data-exchange-using-area-pointers-basic-panels-panels-comfort-panels-rt-advanced/date/time-plc-area-pointer-basic-panels-panels-comfort-panels-rt-advanced>
// Long layouts: STEP7 V21 LTIME/LTOD/LDT and Siemens ODK's signed/unsigned mapping:
// <https://docs.tia.siemens.cloud/r/en-us/v21/data-types/timers/ltime-iec-time-s7-1500-s7-1200-g2>
// <https://docs.tia.siemens.cloud/r/en-us/v20/data-types/date-and-time/ltod-ltime_of_day-s7-1500>
// <https://docs.tia.siemens.cloud/r/en-us/v21/data-types/date-and-time/ldt-date_and_ltime-s7-1500-s7-1200-g2?contentId=oCICtfsNsZkx8NWzouaYhg>
// <https://docs.tia.siemens.cloud/r/simatic_s7_1500_et_200mp_manual_collection_enus_21/comprehensive-information/cpu-function-manuals/open-development-kit-1500s-v2.5-sp4/syntax-interface-file-project-.odk-for-cpu-function-libraries/data-types/syntax-interface-data-types?contentId=z3HjN1vu_siFXi6EzYrD9g>
use super::{DataType, protocol};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{Datelike as _, Days, NaiveDate};
use serde_json::{Value, json};

// All supported objects have more than 64 serialized bytes even before their
// value fields. This conservative floor lets array admission stop before I/O.
pub(super) const MIN_JSON_BYTES: usize = 64;

fn object(kind: &str, data: &[u8]) -> Value {
    json!({"kind":"s7_temporal","data_type":kind,"decode_status":"decoded",
        "raw":{"encoding":"base64","bytes":data.len(),"data":STANDARD.encode(data)}})
}

pub(super) fn decode(kind: DataType, data: &[u8]) -> Result<Value> {
    if data.len() != usize::from(kind.width()) {
        return Err(protocol(
            "temporal byte width differs from its configured layout",
        ));
    }
    match kind {
        DataType::Date => {
            let days = u16::from_be_bytes(data.try_into()?);
            let base = NaiveDate::from_ymd_opt(1990, 1, 1)
                .ok_or_else(|| protocol("DATE base cannot be represented"))?;
            let date = base
                .checked_add_days(Days::new(u64::from(days)))
                .ok_or_else(|| protocol("DATE exceeds the civil calendar"))?;
            let mut value = object("date", data);
            value["days_since_1990"] = json!(days);
            value["date"] = json!(date.to_string());
            Ok(value)
        }
        DataType::Time => {
            let mut value = object("time", data);
            value["milliseconds"] = json!(i32::from_be_bytes(data.try_into()?));
            value["unit"] = json!("ms");
            value["semantic"] = json!("duration");
            Ok(value)
        }
        DataType::Tod => {
            let milliseconds = u32::from_be_bytes(data.try_into()?);
            let mut value = object("tod", data);
            value["milliseconds"] = json!(milliseconds);
            value["unit"] = json!("ms");
            value["clock_basis"] = json!("unspecified");
            if milliseconds < 86_400_000 {
                value["time_of_day"] = json!(format!(
                    "{:02}:{:02}:{:02}.{:03}",
                    milliseconds / 3_600_000,
                    milliseconds / 60_000 % 60,
                    milliseconds / 1000 % 60,
                    milliseconds % 1000
                ));
            } else {
                value["decode_status"] = json!("invalid_value");
                value["invalid_fields"] = json!(["milliseconds"]);
            }
            Ok(value)
        }
        DataType::Ltime => {
            let mut value = object("ltime", data);
            value["nanoseconds"] = json!(i64::from_be_bytes(data.try_into()?));
            value["unit"] = json!("ns");
            value["semantic"] = json!("duration");
            Ok(value)
        }
        DataType::Ltod => {
            let nanoseconds = u64::from_be_bytes(data.try_into()?);
            let mut value = object("ltod", data);
            value["nanoseconds"] = json!(nanoseconds);
            value["unit"] = json!("ns");
            value["clock_basis"] = json!("unspecified");
            if nanoseconds < NS_PER_DAY {
                value["time_of_day"] = json!(time_of_day_ns(nanoseconds));
            } else {
                value["decode_status"] = json!("invalid_value");
                value["invalid_fields"] = json!(["nanoseconds"]);
            }
            Ok(value)
        }
        DataType::Ldt => long_date_and_time(data),
        DataType::DateAndTime => date_and_time(data),
        DataType::Dtl => dtl(data),
        _ => Err(protocol("data type is not a temporal layout")),
    }
}

const NS_PER_DAY: u64 = 86_400_000_000_000;
// This is LDT's scalar value range, unlike DTL's conversion recommendation.
const MAX_LDT_NS: u64 = 9_223_372_036_854_775_807;

fn time_of_day_ns(nanoseconds: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}.{:09}",
        nanoseconds / 3_600_000_000_000,
        nanoseconds / 60_000_000_000 % 60,
        nanoseconds / 1_000_000_000 % 60,
        nanoseconds % 1_000_000_000
    )
}

fn long_date_and_time(data: &[u8]) -> Result<Value> {
    // The unsigned count is retained even when it exceeds LDT's documented range.
    // A high bit is not interpreted as a negative instant or Unix/UTC timestamp.
    let nanoseconds = u64::from_be_bytes(data.try_into()?);
    let mut value = object("ldt", data);
    value["nanoseconds_since_1970"] = json!(nanoseconds);
    value["unit"] = json!("ns");
    value["clock_basis"] = json!("unspecified");
    if nanoseconds > MAX_LDT_NS {
        value["decode_status"] = json!("invalid_value");
        value["invalid_fields"] = json!(["nanoseconds_since_1970"]);
    } else {
        let base = NaiveDate::from_ymd_opt(1970, 1, 1)
            .ok_or_else(|| protocol("LDT base cannot be represented"))?;
        let date = base
            .checked_add_days(Days::new(nanoseconds / NS_PER_DAY))
            .ok_or_else(|| protocol("LDT exceeds the civil calendar"))?;
        value["calendar"] = json!(format!(
            "{date}T{}",
            time_of_day_ns(nanoseconds % NS_PER_DAY)
        ));
    }
    Ok(value)
}

fn bcd(byte: u8) -> Option<u8> {
    let high = byte >> 4;
    let low = byte & 15;
    (high <= 9 && low <= 9).then_some(high * 10 + low)
}

fn date_and_time(data: &[u8]) -> Result<Value> {
    let bytes = data
        .first_chunk::<8>()
        .ok_or_else(|| protocol("truncated DATE_AND_TIME layout"))?;
    let mut value = object("date_and_time", data);
    value["clock_basis"] = json!("unspecified");
    let mut decoded = [0; 7];
    let mut invalid = Vec::new();
    for ((target, &byte), name) in decoded.iter_mut().zip(bytes).zip([
        "year",
        "month",
        "day",
        "hour",
        "minute",
        "second",
        "millisecond",
    ]) {
        if let Some(number) = bcd(byte) {
            *target = number;
        } else {
            invalid.push(name);
        }
    }
    if bytes[7] >> 4 > 9 && !invalid.contains(&"millisecond") {
        invalid.push("millisecond");
    }
    if !invalid.is_empty() {
        value["decode_status"] = json!("invalid_value");
        value["invalid_fields"] = json!(invalid);
        return Ok(value);
    }
    let [year, month, day, hour, minute, second, fraction] = decoded;
    let year = u16::from(year) + if year >= 90 { 1900 } else { 2000 };
    let calendar = Calendar {
        year,
        month,
        day,
        hour,
        minute,
        second,
        weekday: bytes[7] & 15,
        nanosecond: (u32::from(fraction) * 10 + u32::from(bytes[7] >> 4)) * 1_000_000,
    };
    value["fields"] = calendar.fields(false);
    calendar.finish(&mut value, false);
    Ok(value)
}

fn dtl(data: &[u8]) -> Result<Value> {
    let bytes = data
        .first_chunk::<12>()
        .ok_or_else(|| protocol("truncated DTL layout"))?;
    let calendar = Calendar {
        year: u16::from_be_bytes([bytes[0], bytes[1]]),
        month: bytes[2],
        day: bytes[3],
        weekday: bytes[4],
        hour: bytes[5],
        minute: bytes[6],
        second: bytes[7],
        nanosecond: u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
    };
    let mut value = object("dtl", data);
    value["clock_basis"] = json!("unspecified");
    value["fields"] = calendar.fields(true);
    calendar.finish(&mut value, true);
    Ok(value)
}

#[derive(Clone, Copy)]
struct Calendar {
    year: u16,
    month: u8,
    day: u8,
    weekday: u8,
    hour: u8,
    minute: u8,
    second: u8,
    nanosecond: u32,
}
impl Calendar {
    fn fields(&self, wide: bool) -> Value {
        let mut fields = json!({"year":self.year,"month":self.month,"day":self.day,
            "weekday":self.weekday,"hour":self.hour,"minute":self.minute,"second":self.second});
        if wide {
            fields["nanosecond"] = json!(self.nanosecond);
        } else {
            fields["millisecond"] = json!(self.nanosecond / 1_000_000);
        }
        fields
    }
    fn finish(&self, value: &mut Value, wide: bool) {
        let years = if wide { 1970..=2554 } else { 1990..=2089 };
        let mut invalid = Vec::new();
        for (valid, field) in [
            (years.contains(&self.year), "year"),
            ((1..=12).contains(&self.month), "month"),
            ((1..=31).contains(&self.day), "day"),
            ((1..=7).contains(&self.weekday), "weekday"),
            (self.hour <= 23, "hour"),
            (self.minute <= 59, "minute"),
            (self.second <= 59, "second"),
            (self.nanosecond < 1_000_000_000, "nanosecond"),
        ] {
            if !valid {
                invalid.push(field);
            }
        }
        let date = NaiveDate::from_ymd_opt(
            i32::from(self.year),
            u32::from(self.month),
            u32::from(self.day),
        );
        if date.is_none() && !invalid.contains(&"month") && !invalid.contains(&"day") {
            invalid.push("date");
        }
        if wide {
            value["within_documented_year_range"] = json!(years.contains(&self.year));
            // Civil fields only: no Unix/UTC conversion or epoch arithmetic.
            let recommended = (1970..=2262).contains(&self.year)
                && (
                    self.year,
                    self.month,
                    self.day,
                    self.hour,
                    self.minute,
                    self.second,
                    self.nanosecond,
                ) <= (2262, 4, 11, 23, 47, 16, 854_775_807)
                && invalid.is_empty();
            value["within_recommended_conversion_range"] = json!(recommended);
        }
        if !invalid.is_empty() {
            value["decode_status"] = json!("invalid_value");
            value["invalid_fields"] = json!(invalid);
        } else if let Some(date) = date {
            value["weekday_matches_date"] =
                json!(date.weekday().number_from_sunday() == u32::from(self.weekday));
            let fraction = if wide {
                format!("{:09}", self.nanosecond)
            } else {
                format!("{:03}", self.nanosecond / 1_000_000)
            };
            value["calendar"] = json!(format!(
                "{date}T{:02}:{:02}:{:02}.{fraction}",
                self.hour, self.minute, self.second
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: [u8; 8] = [0x08, 0x10, 0x25, 0x08, 0x12, 0x34, 0x56, 0x77];
    const DTL: [u8; 12] = [0x07, 0xd8, 12, 16, 3, 20, 30, 20, 0x0e, 0xe6, 0xb2, 0x80];

    #[test]
    fn documented_calendar_examples_keep_exact_bytes_and_unzoned_civil_fields() {
        for (kind, bytes, text) in [
            (
                DataType::DateAndTime,
                DT.as_slice(),
                "2008-10-25T08:12:34.567",
            ),
            (
                DataType::Dtl,
                DTL.as_slice(),
                "2008-12-16T20:30:20.250000000",
            ),
        ] {
            let value = decode(kind, bytes).unwrap();
            assert_eq!(value["calendar"], text);
            assert_eq!(value["decode_status"], "decoded");
            assert_eq!(value["clock_basis"], "unspecified");
            assert_eq!(value["weekday_matches_date"], true);
            assert_eq!(
                STANDARD
                    .decode(value["raw"]["data"].as_str().unwrap())
                    .unwrap(),
                bytes
            );
            assert!(serde_json::to_vec(&value).unwrap().len() >= MIN_JSON_BYTES);
        }
        assert_eq!(
            decode(DataType::DateAndTime, &DT).unwrap()["fields"]["millisecond"],
            567
        );
        assert_eq!(
            decode(DataType::Dtl, &DTL).unwrap()["fields"]["nanosecond"],
            250_000_000
        );
    }

    #[test]
    fn exact_counter_extremes_and_documented_vs_recommended_calendar_ranges_are_distinct() {
        for milliseconds in [i32::MIN, 0, i32::MAX] {
            let value = decode(DataType::Time, &milliseconds.to_be_bytes()).unwrap();
            assert_eq!(value["milliseconds"], milliseconds);
            assert_eq!(value["unit"], "ms");
            assert_eq!(value["semantic"], "duration");
        }
        for (days, date) in [(0_u16, "1990-01-01"), (u16::MAX, "2169-06-06")] {
            let value = decode(DataType::Date, &days.to_be_bytes()).unwrap();
            assert_eq!(value["days_since_1990"], days);
            assert_eq!(value["date"], date);
        }
        for (milliseconds, text) in [(0_u32, "00:00:00.000"), (86_399_999, "23:59:59.999")] {
            assert_eq!(
                decode(DataType::Tod, &milliseconds.to_be_bytes()).unwrap()["time_of_day"],
                text
            );
        }
        for raw in [86_400_000_u32, u32::MAX] {
            let value = decode(DataType::Tod, &raw.to_be_bytes()).unwrap();
            assert_eq!(value["milliseconds"], raw);
            assert_eq!(value["decode_status"], "invalid_value");
            assert!(value.get("time_of_day").is_none());
        }
        for (year, expected) in [(0x90, 1990), (0x99, 1999), (0x00, 2000), (0x89, 2089)] {
            let mut bytes = DT;
            bytes[0] = year;
            assert_eq!(
                decode(DataType::DateAndTime, &bytes).unwrap()["fields"]["year"],
                expected
            );
        }
        for (year, month, day, ns, valid, recommended) in [
            (1969_u16, 12, 31, 0_u32, false, false),
            (1970, 1, 1, 0, true, true),
            (2000, 2, 29, 0, true, true),
            (2100, 2, 29, 0, false, false),
            (2263, 1, 1, 0, true, false),
            (2554, 12, 31, 999_999_999, true, false),
            (2555, 1, 1, 0, false, false),
            (u16::MAX, 1, 1, 0, false, false),
        ] {
            let mut bytes = DTL;
            bytes[..2].copy_from_slice(&year.to_be_bytes());
            bytes[2] = month;
            bytes[3] = day;
            bytes[8..].copy_from_slice(&ns.to_be_bytes());
            let value = decode(DataType::Dtl, &bytes).unwrap();
            assert_eq!(
                value["decode_status"],
                if valid { "decoded" } else { "invalid_value" }
            );
            assert_eq!(
                value["within_documented_year_range"],
                (1970..=2554).contains(&year)
            );
            assert_eq!(value["within_recommended_conversion_range"], recommended);
        }
        for (ns, recommended) in [(854_775_807_u32, true), (854_775_808, false)] {
            let bytes = [
                2262_u16.to_be_bytes().as_slice(),
                &[4, 11, 6, 23, 47, 16],
                &ns.to_be_bytes(),
            ]
            .concat();
            let value = decode(DataType::Dtl, &bytes).unwrap();
            assert_eq!(value["decode_status"], "decoded");
            assert_eq!(value["within_recommended_conversion_range"], recommended);
        }
    }

    #[test]
    fn invalid_application_bytes_remain_raw_while_wrong_wire_extent_is_an_error() {
        for (index, byte) in [
            (0, 0xa0),
            (1, 0x1a),
            (2, 0x32),
            (3, 0x24),
            (4, 0x60),
            (5, 0x60),
            (6, 0xa0),
            (7, 0xa3),
            (7, 0x78),
            (7, 0x70),
        ] {
            let mut bytes = DT;
            bytes[index] = byte;
            let value = decode(DataType::DateAndTime, &bytes).unwrap();
            assert_eq!(value["decode_status"], "invalid_value");
            assert_eq!(
                STANDARD
                    .decode(value["raw"]["data"].as_str().unwrap())
                    .unwrap(),
                bytes
            );
            assert!(value.get("calendar").is_none());
        }
        let mut mismatch = DTL;
        mismatch[4] = 1;
        let value = decode(DataType::Dtl, &mismatch).unwrap();
        assert_eq!(value["decode_status"], "decoded");
        assert_eq!(value["weekday_matches_date"], false);
        for (index, byte) in [
            (2, 0),
            (2, 13),
            (3, 0),
            (4, 0),
            (4, 8),
            (5, 24),
            (6, 60),
            (7, 60),
        ] {
            let mut bytes = DTL;
            bytes[index] = byte;
            let value = decode(DataType::Dtl, &bytes).unwrap();
            assert_eq!(value["decode_status"], "invalid_value");
            let field = match index {
                2 => "month",
                3 => "day",
                4 => "weekday",
                5 => "hour",
                6 => "minute",
                _ => "second",
            };
            assert_eq!(value["fields"][field], byte);
        }
        let mut bytes = DTL;
        bytes[8..].copy_from_slice(&u32::MAX.to_be_bytes());
        let value = decode(DataType::Dtl, &bytes).unwrap();
        assert_eq!(value["fields"]["nanosecond"], u32::MAX);
        assert_eq!(value["decode_status"], "invalid_value");
        decode(DataType::Dtl, &DTL[..11]).unwrap_err();
        decode(DataType::DateAndTime, &DT[..7]).unwrap_err();
        decode(DataType::U16, &[0, 1]).unwrap_err();
    }

    #[test]
    fn long_duration_retains_full_signed_nanoseconds_without_magnitude_overflow() {
        for nanoseconds in [i64::MIN, -1, 0, 1, i64::MAX] {
            let bytes = nanoseconds.to_be_bytes();
            let value = decode(DataType::Ltime, &bytes).unwrap();
            assert_eq!(value["nanoseconds"].as_i64().unwrap(), nanoseconds);
            assert_eq!(value["unit"], "ns");
            assert_eq!(value["semantic"], "duration");
            assert_eq!(value["decode_status"], "decoded");
            assert_eq!(
                STANDARD
                    .decode(value["raw"]["data"].as_str().unwrap())
                    .unwrap(),
                bytes
            );
            assert!(serde_json::to_vec(&value).unwrap().len() >= MIN_JSON_BYTES);
            assert!(value.get("milliseconds").is_none());
            assert!(value.get("calendar").is_none());
        }
    }

    #[test]
    fn long_day_clock_preserves_unsigned_invalid_counts_without_wrapping() {
        for (nanoseconds, text) in [
            (0_u64, "00:00:00.000000000"),
            (1, "00:00:00.000000001"),
            (37_230_400_365_215, "10:20:30.400365215"),
            (86_399_999_999_999, "23:59:59.999999999"),
        ] {
            let value = decode(DataType::Ltod, &nanoseconds.to_be_bytes()).unwrap();
            assert_eq!(value["nanoseconds"].as_u64().unwrap(), nanoseconds);
            assert_eq!(value["time_of_day"], text);
            assert_eq!(value["unit"], "ns");
            assert_eq!(value["clock_basis"], "unspecified");
            assert_eq!(value["decode_status"], "decoded");
            assert!(serde_json::to_vec(&value).unwrap().len() >= MIN_JSON_BYTES);
        }
        for nanoseconds in [86_400_000_000_000_u64, 1_u64 << 63, u64::MAX] {
            let bytes = nanoseconds.to_be_bytes();
            let value = decode(DataType::Ltod, &bytes).unwrap();
            assert_eq!(value["nanoseconds"].as_u64().unwrap(), nanoseconds);
            assert_eq!(value["decode_status"], "invalid_value");
            assert_eq!(value["invalid_fields"], json!(["nanoseconds"]));
            assert_eq!(
                STANDARD
                    .decode(value["raw"]["data"].as_str().unwrap())
                    .unwrap(),
                bytes
            );
            assert!(value.get("time_of_day").is_none());
        }
    }

    #[test]
    fn long_civil_count_keeps_exact_epoch_origin_without_assigning_utc_or_negative_instants() {
        for (nanoseconds, text) in [
            (0_u64, "1970-01-01T00:00:00.000000000"),
            (1, "1970-01-01T00:00:00.000000001"),
            (951_782_400_123_456_789, "2000-02-29T00:00:00.123456789"),
            (9_223_372_036_854_775_807, "2262-04-11T23:47:16.854775807"),
        ] {
            let bytes = nanoseconds.to_be_bytes();
            let value = decode(DataType::Ldt, &bytes).unwrap();
            assert_eq!(
                value["nanoseconds_since_1970"].as_u64().unwrap(),
                nanoseconds
            );
            assert_eq!(value["calendar"], text);
            assert_eq!(value["unit"], "ns");
            assert_eq!(value["clock_basis"], "unspecified");
            assert_eq!(value["decode_status"], "decoded");
            assert_eq!(
                STANDARD
                    .decode(value["raw"]["data"].as_str().unwrap())
                    .unwrap(),
                bytes
            );
            assert!(serde_json::to_vec(&value).unwrap().len() >= MIN_JSON_BYTES);
        }
        for nanoseconds in [1_u64 << 63, u64::MAX] {
            let value = decode(DataType::Ldt, &nanoseconds.to_be_bytes()).unwrap();
            assert_eq!(
                value["nanoseconds_since_1970"].as_u64().unwrap(),
                nanoseconds
            );
            assert_eq!(value["decode_status"], "invalid_value");
            assert_eq!(value["invalid_fields"], json!(["nanoseconds_since_1970"]));
            assert!(value.get("calendar").is_none());
        }
        for kind in [DataType::Ltime, DataType::Ltod, DataType::Ldt] {
            decode(kind, &[0; 7]).unwrap_err();
            decode(kind, &[0; 9]).unwrap_err();
        }
    }
}
