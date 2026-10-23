//! Localized presentation is separate from a diagnostic's absolute event time.
use super::*;
use crate::session::{LogEntry, SessionActivity};
use chrono::{DateTime, Local, SecondsFormat, Utc};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Labels {
    pub compact: String,
    pub full: String,
    pub copy: String,
    pub day: Day,
}

/// Group by the viewer's local calendar day, independently of the localized
/// caption or the producer's UTC offset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Day {
    pub key: Option<chrono::NaiveDate>,
    pub label: String,
}
impl Day {
    fn unavailable() -> Self {
        Self {
            key: None,
            label: "Date unavailable".into(),
        }
    }
}
#[cfg(test)]
pub(super) fn log_day(entry: &LogEntry) -> Day {
    log(entry).day
}
#[cfg(test)]
pub(super) fn activity_day(entry: &SessionActivity) -> Day {
    activity(entry).day
}
fn day(unix_ms: i64, local: &DateTime<Local>) -> Day {
    Day {
        key: Some(local.date_naive()),
        label: native::date(unix_ms, local.offset().local_minus_utc())
            .unwrap_or_else(|| local.format("%Y-%m-%d").to_string()),
    }
}

/// Every row retains the same two child slots when a date boundary appears or
/// disappears. Only the passive caption changes; the actual row stays in place.
pub(super) fn dated_row<'a>(
    content: Element<'a, Message>,
    caption: Option<Day>,
    section: &str,
    heading_id: accessibility::NodeId,
    p: &'static Palette,
) -> Element<'a, Message> {
    let mut heading = iced::widget::column![].width(Fill);
    let spacing = if let Some(day) = caption {
        let label = format!("{section} · {}", day.label);
        heading = heading.push(accessibility::label_description(
            text(label.clone())
                .size(style::CAPTION)
                .font(style::SEMIBOLD)
                .color(p.secondary)
                .width(Fill)
                .wrapping(text::Wrapping::WordOrGlyph)
                .into(),
            label,
            "Dates use your current time zone. Entries are shown in their recorded order.".into(),
            accessibility::Role::Heading,
            Some(heading_id),
        ));
        3
    } else {
        0
    };
    iced::widget::column![heading, content]
        .spacing(spacing)
        .width(Fill)
        .into()
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct LogKey {
    instant: i64,
    level: String,
    message: String,
}
pub(super) type RawKey = (LogKey, usize);
pub(super) fn log_keys(logs: &[LogEntry]) -> Vec<RawKey> {
    let mut occurrences = std::collections::HashMap::new();
    logs.iter()
        .map(|entry| {
            let key = LogKey {
                instant: entry.timestamp.unix_ms(),
                level: entry.level.clone(),
                message: entry.message.clone(),
            };
            let count = occurrences.entry(key.clone()).or_insert(0);
            let result = (key, *count);
            *count += 1;
            result
        })
        .collect()
}

pub(super) fn log(entry: &LogEntry) -> Labels {
    let timestamp = entry.timestamp;
    let mut labels = instant(timestamp.unix_ms()).unwrap_or_else(unavailable);
    labels
        .full
        .push_str(&format!(" · Recorded {}", timestamp.rfc3339()));
    labels
}

fn unavailable() -> Labels {
    Labels {
        compact: "—".into(),
        full: "Time unavailable (invalid timestamp)".into(),
        copy: "— (invalid timestamp)".into(),
        day: Day::unavailable(),
    }
}

pub(super) fn activity(entry: &SessionActivity) -> Labels {
    i64::try_from(entry.finished_at_ms.unwrap_or(entry.submitted_at_ms))
        .ok()
        .and_then(instant)
        .unwrap_or_else(unavailable)
}

/// A moment, UTC Unix milliseconds, in the viewer's local time as the
/// activity log shows it, after its date when that is not today.
pub(super) fn local_time(unix_ms: u64) -> Option<String> {
    let labels = instant(i64::try_from(unix_ms).ok()?)?;
    Some(if labels.day.key == Some(Local::now().date_naive()) {
        labels.compact
    } else {
        format!("{} {}", labels.day.label, labels.compact)
    })
}

fn instant(unix_ms: i64) -> Option<Labels> {
    let local = local_instant(unix_ms)?;
    let copy = local.to_rfc3339_opts(SecondsFormat::Millis, false);
    let (compact, full) =
        native::format(unix_ms, local.offset().local_minus_utc()).unwrap_or_else(|| {
            (
                local.format("%H:%M:%S").to_string(),
                local.format("%Y-%m-%d %H:%M:%S").to_string(),
            )
        });
    Some(Labels {
        compact,
        full: format!("{full} · {copy}"),
        copy,
        day: day(unix_ms, &local),
    })
}

fn local_instant(unix_ms: i64) -> Option<DateTime<Local>> {
    let utc = DateTime::<Utc>::from_timestamp_millis(unix_ms)?;
    let local = utc.with_timezone(&Local);
    utc.naive_utc()
        .checked_add_signed(chrono::Duration::seconds(i64::from(
            local.offset().local_minus_utc(),
        )))?;
    Some(local)
}

/// Keep the existing timestamp column width. Its full dated, zoned meaning is
/// available to native reading and pointer hover without becoming a tab stop.
#[cfg(test)]
pub(super) fn view<'a>(labels: Labels, p: &'static Palette) -> Element<'a, Message> {
    view_with_id(labels, p, None)
}
pub(super) fn view_with_id<'a>(
    labels: Labels,
    p: &'static Palette,
    id: Option<accessibility::NodeId>,
) -> Element<'a, Message> {
    let compact = labels.compact.clone();
    tip(
        accessibility::label_description(
            text(labels.compact)
                .size(style::CAPTION)
                .font(style::MONO)
                .color(p.secondary)
                .width(86)
                .wrapping(text::Wrapping::WordOrGlyph)
                .into(),
            compact,
            labels.full.clone(),
            accessibility::Role::Label,
            id,
        ),
        labels.full,
    )
}

#[cfg(all(test, not(target_os = "linux")))]
#[path = "time_format/linux.rs"]
mod linux_checks;
#[cfg(target_os = "macos")]
#[path = "time_format/macos.rs"]
mod native;
#[cfg(target_os = "windows")]
#[path = "time_format/windows.rs"]
mod native;
#[cfg(target_os = "linux")]
#[path = "time_format/linux.rs"]
mod native;
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod native {
    pub(super) fn format(_: i64, _: i32) -> Option<(String, String)> {
        None
    }
    pub(super) fn date(_: i64, _: i32) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests;
