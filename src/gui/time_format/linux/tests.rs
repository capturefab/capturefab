use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn desktop_regional_settings_do_not_override_an_explicit_process_locale() {
    assert_eq!(desktop("ubuntu:GNOME"), Desktop::Gnome);
    assert_eq!(desktop("KDE"), Desktop::Kde);
    assert_eq!(desktop("sway"), Desktop::Other);
    assert_eq!(clock_cycle("12h"), Cycle::Twelve);
    assert_eq!(clock_cycle("24h"), Cycle::TwentyFour);
    assert_eq!(clock_cycle("unexpected"), Cycle::Locale);
    let config = regional(
        "[Formats]\nLC_TIME[$i]=de_DE.UTF-8\nLANG=fr_FR.UTF-8\n[Other]\nLC_TIME=invalid\n",
    );
    assert_eq!(
        locale_name("ja_JP.UTF-8", "en_US.UTF-8", "C", &config),
        "ja_JP.UTF-8"
    );
    assert_eq!(locale_name("", "en_US.UTF-8", "C", &config), "de_DE.UTF-8");
    let config = regional("[Formats]\nLC_TIME=Inherit Language\nLANG=fr_FR.UTF-8\n");
    assert_eq!(locale_name("", "en_US.UTF-8", "C", &config), "fr_FR.UTF-8");
    assert_eq!(
        locale_name("ja_JP.UTF-8", "en_US.UTF-8", "C", &config),
        "ja_JP.UTF-8"
    );
    assert_eq!(
        locale_name("", "en_US.UTF-8", "C", &Regional::default()),
        "en_US.UTF-8"
    );
    assert_eq!(locale_name("", "", "", &Regional::default()), "C");
    assert!(regional("[Formats]\nLC_TIME=bad\0locale\n").time.is_none());
    assert!(
        regional("[Applet]\nuse24hFormat=1\n").time.is_none(),
        "a per-widget Plasma clock preference is not a global application setting"
    );
}

struct Blocked {
    entered: Option<mpsc::SyncSender<()>>,
    release: mpsc::Receiver<()>,
    calls: Arc<AtomicUsize>,
}
impl Reader for Blocked {
    fn refresh(&mut self) {}
    fn format(&mut self, key: i64) -> Option<Formatted> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if let Some(entered) = self.entered.take() {
            entered.send(()).unwrap();
            self.release.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        Some(Formatted {
            compact: key.to_string(),
            full: format!("Full {key}"),
            date: "Local date".into(),
            utc_offset_seconds: 0,
        })
    }
}

#[test]
fn slow_native_reads_leave_ui_nonblocking_deduplicate_and_bound_work() {
    let (entered, ready) = mpsc::sync_channel(1);
    let (release, unblock) = mpsc::sync_channel(1);
    let calls = Arc::new(AtomicUsize::new(0));
    let reader_calls = calls.clone();
    let service = Service::start(move || Blocked {
        entered: Some(entered),
        release: unblock,
        calls: reader_calls,
    });
    assert_eq!(service.lookup(0, 0), None);
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(service.lookup(0, 0), None);
    for key in 1..(LIMIT * 2) as i64 {
        assert_eq!(service.lookup(key, 0), None);
    }
    // The background reader is still held. Every UI lookup above returned,
    // and repeated requests did not schedule duplicate or unbounded work.
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(service.cache.lock().unwrap().pending.len(), LIMIT);
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let value = loop {
        if let Some(value) = service.lookup((LIMIT - 1) as i64, 0) {
            break value;
        }
        assert!(
            Instant::now() < deadline,
            "native worker did not publish its result"
        );
        std::thread::yield_now();
    };
    assert_eq!(value.date, "Local date");
    assert_eq!(value.full, format!("Full {}", LIMIT - 1));
    assert!(service.cache.lock().unwrap().values.len() <= LIMIT);
    // A busy worker publication cannot block the renderer either.
    let cache = service.cache.lock().unwrap();
    assert_eq!(service.lookup(0, 0), None);
    drop(cache);
}

struct Refreshing(usize);
impl Reader for Refreshing {
    fn refresh(&mut self) {
        self.0 += 1
    }
    fn format(&mut self, _: i64) -> Option<Formatted> {
        Some(Formatted {
            compact: self.0.to_string(),
            full: "Date and zone".into(),
            date: self.0.to_string(),
            utc_offset_seconds: 0,
        })
    }
}

#[test]
fn cached_time_and_date_refresh_together_without_another_row_request() {
    let service = Service::start_with_refresh(|| Refreshing(0), Duration::from_millis(20));
    let wait = |previous: Option<&str>| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(value) = service.lookup(42, 0)
                && previous != Some(value.compact.as_str())
            {
                assert_eq!(value.date, value.compact);
                return value;
            }
            assert!(
                Instant::now() < deadline,
                "clock preferences did not refresh"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let first = wait(None);
    wait(Some(&first.compact));
}

#[test]
fn a_cached_clock_and_date_cannot_cross_the_callers_current_local_offset() {
    let (requests, _receiver) = mpsc::sync_channel(1);
    let value = |offset: i32| {
        let utc = chrono::DateTime::parse_from_rfc3339("2026-10-15T00:30:00Z").unwrap();
        let local = utc.with_timezone(&chrono::FixedOffset::east_opt(offset).unwrap());
        Formatted {
            compact: local.format("%H:%M:%S").to_string(),
            full: local.to_rfc3339(),
            date: local.format("%Y-%m-%d").to_string(),
            utc_offset_seconds: offset,
        }
    };
    let cache = Arc::new(Mutex::new(Cache::default()));
    cache
        .lock()
        .unwrap()
        .values
        .insert(42, Some(value(-4 * 3600)));
    let service = Service { cache, requests };
    assert_eq!(service.lookup(42, -4 * 3600).unwrap().date, "2026-10-14");
    assert_eq!(
        service.lookup(42, 3600),
        None,
        "a date from yesterday must yield to the caller's coherent numeric fallback after a zone change"
    );
    service
        .cache
        .lock()
        .unwrap()
        .values
        .insert(42, Some(value(3600)));
    let refreshed = service.lookup(42, 3600).unwrap();
    assert_eq!(refreshed.date, "2026-10-15");
    assert_eq!(refreshed.compact, "01:30:00");
    assert_eq!(service.lookup(42, -4 * 3600), None);
}
