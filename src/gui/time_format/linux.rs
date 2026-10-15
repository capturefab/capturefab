//! Desktop preferences and locale files are read only on a dedicated worker.
//! The UI uses a bounded, nonblocking cache and retains its fallback on misses.
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

const LIMIT: usize = 512;
const REFRESH: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Formatted {
    compact: String,
    full: String,
    date: String,
    utc_offset_seconds: i32,
}

#[derive(Default)]
struct Cache {
    values: HashMap<i64, Option<Formatted>>,
    pending: HashSet<i64>,
}

struct Service {
    cache: Arc<Mutex<Cache>>,
    requests: mpsc::SyncSender<i64>,
}

trait Reader {
    fn refresh(&mut self);
    fn format(&mut self, unix_ms: i64) -> Option<Formatted>;
}

impl Service {
    fn start<R: Reader + 'static>(create: impl FnOnce() -> R + Send + 'static) -> Self {
        Self::start_with_refresh(create, REFRESH)
    }

    fn start_with_refresh<R: Reader + 'static>(
        create: impl FnOnce() -> R + Send + 'static,
        refresh: Duration,
    ) -> Self {
        let cache = Arc::new(Mutex::new(Cache::default()));
        let (requests, receiver) = mpsc::sync_channel(LIMIT);
        let worker_cache = cache.clone();
        // Failure to start leaves a disconnected sender and the usual fallback.
        let _ = std::thread::Builder::new()
            .name("capturefab-clock-format".into())
            .spawn(move || worker(create(), receiver, worker_cache, refresh));
        Self { cache, requests }
    }

    fn lookup(&self, unix_ms: i64, utc_offset_seconds: i32) -> Option<Formatted> {
        let mut cache = self.cache.try_lock().ok()?;
        if let Some(value) = cache.values.get(&unix_ms) {
            return value
                .clone()
                .filter(|value| value.utc_offset_seconds == utc_offset_seconds);
        }
        if !cache.pending.contains(&unix_ms)
            && cache.pending.len() < LIMIT
            && self.requests.try_send(unix_ms).is_ok()
        {
            cache.pending.insert(unix_ms);
        }
        None
    }
}

fn worker<R: Reader>(
    mut reader: R,
    requests: mpsc::Receiver<i64>,
    cache: Arc<Mutex<Cache>>,
    refresh: Duration,
) {
    let mut refreshed = Instant::now();
    loop {
        if refreshed.elapsed() >= refresh {
            reader.refresh();
            let keys = match cache.lock() {
                Ok(cache) => cache.values.keys().copied().collect::<Vec<_>>(),
                Err(_) => return,
            };
            let values = keys
                .into_iter()
                .map(|key| (key, reader.format(key)))
                .collect();
            match cache.lock() {
                Ok(mut cache) => cache.values = values,
                Err(_) => return,
            }
            refreshed = Instant::now();
        }
        match requests.recv_timeout(refresh.saturating_sub(refreshed.elapsed())) {
            Ok(key) => {
                let value = reader.format(key);
                let Ok(mut cache) = cache.lock() else { return };
                if cache.values.len() >= LIMIT {
                    cache.values.clear();
                }
                cache.values.insert(key, value);
                cache.pending.remove(&key);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Desktop {
    Gnome,
    Kde,
    Other,
}
fn desktop(value: &str) -> Desktop {
    value
        .split([':', ';'])
        .find_map(|name| match name.trim().to_ascii_lowercase().as_str() {
            "gnome" => Some(Desktop::Gnome),
            "kde" | "plasma" => Some(Desktop::Kde),
            _ => None,
        })
        .unwrap_or(Desktop::Other)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cycle {
    Locale,
    Twelve,
    TwentyFour,
}
fn clock_cycle(value: &str) -> Cycle {
    match value {
        "12h" => Cycle::Twelve,
        "24h" => Cycle::TwentyFour,
        _ => Cycle::Locale,
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
struct Regional {
    time: Option<String>,
    language: Option<String>,
    inherit_time: bool,
}

fn valid_locale(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'@')))
    .then_some(value)
}

fn regional(config: &str) -> Regional {
    let mut formats = false;
    let mut result = Regional::default();
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            formats = line == "[Formats]";
        } else if formats && let Some((key, value)) = line.split_once('=') {
            let key = key.trim().trim_end_matches("[$i]");
            if key == "LC_TIME" && value.trim() == "Inherit Language" {
                result.inherit_time = true;
                result.time = None;
                continue;
            }
            let Some(value) = valid_locale(value) else {
                continue;
            };
            match key {
                "LC_TIME" => {
                    result.time = Some(value.into());
                    result.inherit_time = false;
                }
                "LANG" => result.language = Some(value.into()),
                _ => {}
            }
        }
    }
    result
}

fn locale_name<'a>(all: &'a str, time: &'a str, language: &'a str, kde: &'a Regional) -> &'a str {
    // LC_ALL remains an explicit process override. Plasma settings may have
    // changed since launch, so its live regional choice precedes stale LC_TIME.
    [
        Some(all),
        kde.time.as_deref(),
        (!kde.inherit_time).then_some(time),
        kde.language.as_deref(),
        Some(language),
    ]
    .into_iter()
    .flatten()
    .find_map(valid_locale)
    .unwrap_or("C")
}

#[cfg(target_os = "linux")]
mod native {
    use super::*;
    use chrono::{DateTime, Datelike, Local, Timelike, Utc};
    use std::{
        ffi::{CStr, CString, c_char, c_void},
        io::Read,
        path::PathBuf,
        ptr,
        sync::OnceLock,
    };

    static SERVICE: OnceLock<Service> = OnceLock::new();
    pub(super) fn lookup(unix_ms: i64, utc_offset_seconds: i32) -> Option<Formatted> {
        SERVICE
            .get_or_init(|| Service::start(Preferences::new))
            .lookup(unix_ms, utc_offset_seconds)
    }

    struct Preferences {
        desktop: Desktop,
        gnome: Option<Gnome>,
        locale: Option<Locale>,
        cycle: Cycle,
    }
    impl Preferences {
        fn new() -> Self {
            let desktop = desktop(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default());
            let mut result = Self {
                desktop,
                gnome: (desktop == Desktop::Gnome).then(Gnome::load).flatten(),
                locale: None,
                cycle: Cycle::Locale,
            };
            result.refresh();
            result
        }
    }
    impl Reader for Preferences {
        fn refresh(&mut self) {
            self.cycle = self.gnome.as_ref().map_or(Cycle::Locale, Gnome::cycle);
            let kde = if self.desktop == Desktop::Kde {
                kde_regional()
            } else {
                Regional::default()
            };
            self.locale = Locale::new(locale_name(
                &std::env::var("LC_ALL").unwrap_or_default(),
                &std::env::var("LC_TIME").unwrap_or_default(),
                &std::env::var("LANG").unwrap_or_default(),
                &kde,
            ));
        }
        fn format(&mut self, unix_ms: i64) -> Option<Formatted> {
            self.locale.as_ref()?.format(unix_ms, self.cycle)
        }
    }

    fn kde_regional() -> Regional {
        let user = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
        let mut roots = user.into_iter().collect::<Vec<_>>();
        roots.extend(
            std::env::split_paths(
                &std::env::var_os("XDG_CONFIG_DIRS").unwrap_or_else(|| "/etc/xdg".into()),
            )
            .filter(|path| path.is_absolute()),
        );
        let mut result = Regional::default();
        for root in roots {
            let Ok(file) = std::fs::File::open(root.join("plasma-localerc")) else {
                continue;
            };
            let mut text = String::new();
            if file.take(65_537).read_to_string(&mut text).is_err() || text.len() > 65_536 {
                continue;
            }
            let config = regional(&text);
            if result.time.is_none() && !result.inherit_time {
                result.time = config.time;
                result.inherit_time = config.inherit_time;
            }
            result.language = result.language.or(config.language);
            if (result.time.is_some() || result.inherit_time) && result.language.is_some() {
                break;
            }
        }
        result
    }

    struct Locale(libc::locale_t);
    impl Locale {
        fn new(name: &str) -> Option<Self> {
            let name = CString::new(name).ok()?;
            // A separate locale object reads the user's installed locale without
            // mutating process-global locale or another thread's formatting.
            let locale =
                unsafe { libc::newlocale(libc::LC_TIME_MASK, name.as_ptr(), ptr::null_mut()) };
            if locale.is_null() {
                None
            } else {
                Some(Self(locale))
            }
        }
        fn text(&self, format: &CStr, time: &libc::tm) -> Option<String> {
            let mut output = [0u8; 2048];
            // SAFETY: locale and tm remain valid; output is a writable buffer.
            let size = unsafe {
                libc::strftime_l(
                    output.as_mut_ptr().cast(),
                    output.len(),
                    format.as_ptr(),
                    time,
                    self.0,
                )
            };
            if size == 0 {
                return None;
            }
            std::str::from_utf8(&output[..size]).ok().map(str::to_owned)
        }
        fn format(&self, unix_ms: i64, cycle: Cycle) -> Option<Formatted> {
            let utc = DateTime::<Utc>::from_timestamp_millis(unix_ms)?;
            let local = utc.with_timezone(&Local);
            utc.naive_utc()
                .checked_add_signed(chrono::Duration::seconds(i64::from(
                    local.offset().local_minus_utc(),
                )))?;
            let time = libc::tm {
                tm_sec: local.second() as i32,
                tm_min: local.minute() as i32,
                tm_hour: local.hour() as i32,
                tm_mday: local.day() as i32,
                tm_mon: local.month0() as i32,
                tm_year: local.year() - 1900,
                tm_wday: local.weekday().num_days_from_sunday() as i32,
                tm_yday: local.ordinal0() as i32,
                tm_isdst: 0,
                tm_gmtoff: local.offset().local_minus_utc().into(),
                tm_zone: ptr::null(),
            };
            let compact = match cycle {
                Cycle::Locale => self.text(c"%X", &time)?,
                Cycle::TwentyFour => self.text(c"%H:%M:%S", &time)?,
                Cycle::Twelve => {
                    let period = self
                        .text(c"%p", &time)
                        .unwrap_or_else(|| if local.hour() < 12 { "AM" } else { "PM" }.into());
                    format!("{} {period}", self.text(c"%I:%M:%S", &time)?)
                }
            };
            let date = self.text(c"%x", &time)?;
            Some(Formatted {
                full: format!("{date} {compact}"),
                compact,
                date,
                utc_offset_seconds: local.offset().local_minus_utc(),
            })
        }
    }
    impl Drop for Locale {
        fn drop(&mut self) {
            // SAFETY: this object owns the locale and remains worker-local.
            unsafe { libc::freelocale(self.0) };
        }
    }

    type Pointer = *mut c_void;
    struct Gnome {
        _library: libloading::Library,
        settings: Pointer,
        context: Pointer,
        get_string: unsafe extern "C" fn(Pointer, *const c_char) -> *mut c_char,
        free: unsafe extern "C" fn(Pointer),
        unref: unsafe extern "C" fn(Pointer),
        iterate: unsafe extern "C" fn(Pointer, i32) -> i32,
        pop: unsafe extern "C" fn(Pointer),
        context_unref: unsafe extern "C" fn(Pointer),
    }
    impl Gnome {
        fn load() -> Option<Self> {
            // The installed SONAME is optional; do not search arbitrary paths.
            // Resolve every symbol before allocating native resources.
            unsafe {
                // GObject registers type callbacks process-wide. Keep their
                // library mapped even after this worker releases its objects.
                let library: libloading::Library = libloading::os::unix::Library::open(
                    Some("libgio-2.0.so.0"),
                    libc::RTLD_LAZY | libc::RTLD_LOCAL | libc::RTLD_NODELETE,
                )
                .ok()?
                .into();
                let source_default = *library
                    .get::<unsafe extern "C" fn() -> Pointer>(
                        b"g_settings_schema_source_get_default\0",
                    )
                    .ok()?;
                let lookup = *library
                    .get::<unsafe extern "C" fn(Pointer, *const c_char, i32) -> Pointer>(
                        b"g_settings_schema_source_lookup\0",
                    )
                    .ok()?;
                let has_key = *library
                    .get::<unsafe extern "C" fn(Pointer, *const c_char) -> i32>(
                        b"g_settings_schema_has_key\0",
                    )
                    .ok()?;
                let schema_unref = *library
                    .get::<unsafe extern "C" fn(Pointer)>(b"g_settings_schema_unref\0")
                    .ok()?;
                let new = *library
                    .get::<unsafe extern "C" fn(Pointer, Pointer, *const c_char) -> Pointer>(
                        b"g_settings_new_full\0",
                    )
                    .ok()?;
                let context_new = *library
                    .get::<unsafe extern "C" fn() -> Pointer>(b"g_main_context_new\0")
                    .ok()?;
                let push = *library
                    .get::<unsafe extern "C" fn(Pointer)>(b"g_main_context_push_thread_default\0")
                    .ok()?;
                let mut result = Self {
                    get_string: *library.get(b"g_settings_get_string\0").ok()?,
                    free: *library.get(b"g_free\0").ok()?,
                    unref: *library.get(b"g_object_unref\0").ok()?,
                    iterate: *library.get(b"g_main_context_iteration\0").ok()?,
                    pop: *library.get(b"g_main_context_pop_thread_default\0").ok()?,
                    context_unref: *library.get(b"g_main_context_unref\0").ok()?,
                    _library: library,
                    settings: ptr::null_mut(),
                    context: ptr::null_mut(),
                };
                let source = source_default();
                if source.is_null() {
                    return None;
                }
                let schema = lookup(source, c"org.gnome.desktop.interface".as_ptr(), 1);
                if schema.is_null() {
                    return None;
                }
                if has_key(schema, c"clock-format".as_ptr()) == 0 {
                    schema_unref(schema);
                    return None;
                }
                result.context = context_new();
                if result.context.is_null() {
                    schema_unref(schema);
                    return None;
                }
                push(result.context);
                result.settings = new(schema, ptr::null_mut(), ptr::null());
                schema_unref(schema);
                (!result.settings.is_null()).then_some(result)
            }
        }
        fn cycle(&self) -> Cycle {
            // Dispatch only this worker's context; never service UI callbacks.
            unsafe {
                for _ in 0..32 {
                    if (self.iterate)(self.context, 0) == 0 {
                        break;
                    }
                }
                let value = (self.get_string)(self.settings, c"clock-format".as_ptr());
                if value.is_null() {
                    return Cycle::Locale;
                }
                let cycle = clock_cycle(CStr::from_ptr(value).to_str().unwrap_or_default());
                (self.free)(value.cast());
                cycle
            }
        }
    }
    impl Drop for Gnome {
        fn drop(&mut self) {
            // All GIO values are released while their library is still loaded.
            unsafe {
                if !self.settings.is_null() {
                    (self.unref)(self.settings);
                }
                if !self.context.is_null() {
                    (self.pop)(self.context);
                    (self.context_unref)(self.context);
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn real_posix_locale_formats_both_hour_cycles_and_date_without_global_changes() {
            let locale = Locale::new("C").expect("the POSIX locale is always available");
            let time = 1_792_017_000_123;
            let local = DateTime::<Utc>::from_timestamp_millis(time)
                .unwrap()
                .with_timezone(&Local);
            let twelve = locale.format(time, Cycle::Twelve).unwrap();
            let twenty_four = locale.format(time, Cycle::TwentyFour).unwrap();
            assert_eq!(twelve.compact, local.format("%I:%M:%S %p").to_string());
            assert_eq!(twenty_four.compact, local.format("%H:%M:%S").to_string());
            assert_eq!(twelve.date, twenty_four.date);
            assert_eq!(twenty_four.date, local.format("%m/%d/%y").to_string());
            assert!(twelve.full.contains(&twelve.compact));
            assert!(twelve.full.contains(&twelve.date));
            assert!(Locale::new("capturefab_nonexistent_locale").is_none());
            assert!(locale.format(i64::MAX, Cycle::Locale).is_none());
            // Viewer-zone conversion near chrono's valid boundaries must not
            // panic, regardless of the test machine's current offset.
            for bound in [DateTime::<Utc>::MIN_UTC, DateTime::<Utc>::MAX_UTC] {
                let _ = locale.format(bound.timestamp_millis(), Cycle::Locale);
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub(super) fn format(unix_ms: i64, utc_offset_seconds: i32) -> Option<(String, String)> {
    native::lookup(unix_ms, utc_offset_seconds).map(|value| (value.compact, value.full))
}
#[cfg(target_os = "linux")]
pub(super) fn date(unix_ms: i64, utc_offset_seconds: i32) -> Option<String> {
    native::lookup(unix_ms, utc_offset_seconds).map(|value| value.date)
}

#[cfg(test)]
#[path = "linux/tests.rs"]
mod tests;
