use chrono::{DateTime, Datelike, FixedOffset, Timelike, Utc};
use windows::{
    Win32::{
        Foundation::SYSTEMTIME,
        Globalization::{
            ENUM_DATE_FORMATS_FLAGS, GetDateFormatEx, GetTimeFormatEx, TIME_FORMAT_FLAGS,
        },
    },
    core::PCWSTR,
};
pub(super) fn format(unix_ms: i64, utc_offset_seconds: i32) -> Option<(String, String)> {
    let time = system_time(unix_ms, utc_offset_seconds)?;
    // NULL locale/picture uses the current user's formats and overrides.
    // No forced hour cycle and no LOCALE_NOUSEROVERRIDE.
    unsafe {
        let mut clock = vec![0; 256];
        let len = GetTimeFormatEx(
            PCWSTR::null(),
            TIME_FORMAT_FLAGS(0),
            Some(&time),
            PCWSTR::null(),
            Some(&mut clock),
        );
        if len <= 1 {
            return None;
        }
        let compact = String::from_utf16(clock.get(..len as usize - 1)?).ok()?;
        let date = date(unix_ms, utc_offset_seconds)?;
        Some((compact.clone(), format!("{date} {compact}")))
    }
}

fn system_time(unix_ms: i64, utc_offset_seconds: i32) -> Option<SYSTEMTIME> {
    let zone = FixedOffset::east_opt(utc_offset_seconds)?;
    let utc = DateTime::<Utc>::from_timestamp_millis(unix_ms)?;
    utc.naive_utc()
        .checked_add_signed(chrono::Duration::seconds(i64::from(utc_offset_seconds)))?;
    let local = utc.with_timezone(&zone);
    Some(SYSTEMTIME {
        wYear: local.year().try_into().ok()?,
        wMonth: local.month() as u16,
        wDayOfWeek: local.weekday().num_days_from_sunday() as u16,
        wDay: local.day() as u16,
        wHour: local.hour() as u16,
        wMinute: local.minute() as u16,
        wSecond: local.second() as u16,
        wMilliseconds: local.timestamp_subsec_millis() as u16,
    })
}

pub(super) fn date(unix_ms: i64, utc_offset_seconds: i32) -> Option<String> {
    let time = system_time(unix_ms, utc_offset_seconds)?;
    unsafe {
        let mut date = vec![0; 256];
        let len = GetDateFormatEx(
            PCWSTR::null(),
            ENUM_DATE_FORMATS_FLAGS(0),
            Some(&time),
            PCWSTR::null(),
            Some(&mut date),
            PCWSTR::null(),
        );
        if len <= 1 {
            return None;
        }
        String::from_utf16(date.get(..len as usize - 1)?).ok()
    }
}
