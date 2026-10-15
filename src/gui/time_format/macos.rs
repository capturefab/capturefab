use objc2::rc::Retained;
use objc2_foundation::{NSDate, NSDateFormatter, NSDateFormatterStyle, NSLocale, NSTimeZone};
use std::{
    cell::RefCell,
    collections::HashMap,
    time::{Duration, Instant},
};
struct Cache {
    refreshed: Instant,
    compact: Retained<NSDateFormatter>,
    full: Retained<NSDateFormatter>,
    date: Retained<NSDateFormatter>,
    labels: HashMap<(i64, i32), (String, String)>,
    dates: HashMap<(i64, i32), String>,
}
impl Cache {
    fn new() -> Self {
        let compact = NSDateFormatter::new();
        let full = NSDateFormatter::new();
        let date = NSDateFormatter::new();
        let locale = NSLocale::autoupdatingCurrentLocale();
        let zone = NSTimeZone::localTimeZone();
        for formatter in [&compact, &full, &date] {
            formatter.setLocale(Some(&locale));
            formatter.setTimeZone(Some(&zone));
        }
        compact.setDateStyle(NSDateFormatterStyle::NoStyle);
        compact.setTimeStyle(NSDateFormatterStyle::MediumStyle);
        full.setDateStyle(NSDateFormatterStyle::LongStyle);
        full.setTimeStyle(NSDateFormatterStyle::LongStyle);
        date.setDateStyle(NSDateFormatterStyle::LongStyle);
        date.setTimeStyle(NSDateFormatterStyle::NoStyle);
        Self {
            refreshed: Instant::now(),
            compact,
            full,
            date,
            labels: HashMap::new(),
            dates: HashMap::new(),
        }
    }
}

pub(super) fn date(unix_ms: i64, utc_offset_seconds: i32) -> Option<String> {
    let date = date_in_current_zone(unix_ms, utc_offset_seconds)?;
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.refreshed.elapsed() >= Duration::from_secs(1) {
            *cache = Cache::new();
        }
        if let Some(label) = cache.dates.get(&(unix_ms, utc_offset_seconds)) {
            return Some(label.clone());
        }
        let label = cache.date.stringFromDate(&date).to_string();
        if label.is_empty() {
            return None;
        }
        if cache.dates.len() >= 512 {
            cache.dates.clear();
        }
        cache
            .dates
            .insert((unix_ms, utc_offset_seconds), label.clone());
        Some(label)
    })
}
thread_local! { static CACHE: RefCell<Cache> = RefCell::new(Cache::new()); }
pub(super) fn format(unix_ms: i64, utc_offset_seconds: i32) -> Option<(String, String)> {
    let date = date_in_current_zone(unix_ms, utc_offset_seconds)?;
    // Read current user preferences; never alter defaults or OS settings.
    // Rebuild at most once a second so live hour-cycle/time-zone changes
    // are observed without allocating formatters on every camera frame.
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.refreshed.elapsed() >= Duration::from_secs(1) {
            *cache = Cache::new();
        }
        if let Some(labels) = cache.labels.get(&(unix_ms, utc_offset_seconds)) {
            return Some(labels.clone());
        }
        let labels = (
            cache.compact.stringFromDate(&date).to_string(),
            cache.full.stringFromDate(&date).to_string(),
        );
        if labels.0.is_empty() || labels.1.is_empty() {
            return None;
        }
        if cache.labels.len() >= 512 {
            cache.labels.clear();
        }
        cache
            .labels
            .insert((unix_ms, utc_offset_seconds), labels.clone());
        Some(labels)
    })
}

fn date_in_current_zone(unix_ms: i64, utc_offset_seconds: i32) -> Option<Retained<NSDate>> {
    let date = NSDate::dateWithTimeIntervalSince1970(unix_ms as f64 / 1000.0);
    // Native and chrono zone refreshes can arrive separately. Keep the clock,
    // day group and dated copy on the same offset during a system-zone change.
    (NSTimeZone::localTimeZone().secondsFromGMTForDate(&date) == utc_offset_seconds as isize)
        .then_some(date)
}
