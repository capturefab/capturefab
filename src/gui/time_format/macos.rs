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
    dates: HashMap<i64, String>,
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

    fn matches_offset(&self, date: &NSDate, offset: i32) -> bool {
        self.compact.timeZone().secondsFromGMTForDate(date) == offset as isize
    }
}

pub(super) fn date(unix_ms: i64, utc_offset_seconds: i32) -> Option<String> {
    let date = date_in_current_zone(unix_ms, utc_offset_seconds)?;
    // Many entries share a date. Format each local calendar day once rather
    // than allocating the same long date for every retained event.
    let key = local_day_key(unix_ms, utc_offset_seconds)?;
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.refreshed.elapsed() >= Duration::from_secs(1)
            || !cache.matches_offset(&date, utc_offset_seconds)
        {
            *cache = Cache::new();
        }
        if let Some(label) = cache.dates.get(&key) {
            return Some(label.clone());
        }
        let label = cache.date.stringFromDate(&date).to_string();
        if label.is_empty() {
            return None;
        }
        if cache.dates.len() >= 512 {
            cache.dates.clear();
        }
        cache.dates.insert(key, label.clone());
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
        if cache.refreshed.elapsed() >= Duration::from_secs(1)
            || !cache.matches_offset(&date, utc_offset_seconds)
        {
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

fn local_day_key(unix_ms: i64, utc_offset_seconds: i32) -> Option<i64> {
    unix_ms
        .checked_add(i64::from(utc_offset_seconds) * 1000)
        .map(|time| time.div_euclid(86_400_000))
}

fn date_in_current_zone(unix_ms: i64, utc_offset_seconds: i32) -> Option<Retained<NSDate>> {
    let date = NSDate::dateWithTimeIntervalSince1970(unix_ms as f64 / 1000.0);
    // Native and chrono zone refreshes can arrive separately. Keep the clock,
    // day group and dated copy on the same offset during a system-zone change.
    (NSTimeZone::localTimeZone().secondsFromGMTForDate(&date) == utc_offset_seconds as isize)
        .then_some(date)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copied_formatter_zones_invalidate_old_clock_and_date_cache_entries() {
        let instant = 1_792_017_000_123;
        let stamp = NSDate::dateWithTimeIntervalSince1970(instant as f64 / 1000.0);
        let offset = NSTimeZone::localTimeZone().secondsFromGMTForDate(&stamp) as i32;
        let other = NSTimeZone::timeZoneForSecondsFromGMT(if offset == 0 { 3600 } else { 0 });
        CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            for formatter in [&cache.compact, &cache.full, &cache.date] {
                formatter.setTimeZone(Some(&other));
            }
            cache
                .labels
                .insert((instant, offset), ("old zone".into(), "old zone".into()));
            cache
                .dates
                .insert(local_day_key(instant, offset).unwrap(), "old zone".into());
        });
        assert_ne!(format(instant, offset).unwrap().0, "old zone");
        assert_ne!(date(instant, offset).unwrap(), "old zone");
        CACHE.with(|cache| assert!(cache.borrow().matches_offset(&stamp, offset)));
    }
}
