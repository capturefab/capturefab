//! Camera control runs on the server; this crate is only the browser interface.
//! Access restrictions are provided by the network hosting the server.
use serde_json::{Value, json};
use std::{cell::RefCell, collections::BTreeMap};
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    Document, Element, Event, HtmlImageElement, HtmlInputElement, HtmlSelectElement, Request,
    RequestCache, RequestCredentials, RequestInit, RequestMode, Response, Url, Window,
};
mod activity;
mod log_time;
use activity::{apply_activity, render_activity};

#[derive(Default)]
struct App {
    connected: bool,
    connection_error: bool,
    generation: u32,
    session: Value,
    selected: String,
    actor_id: String,
    actor_name: String,
    activity: Vec<Value>,
    activity_revision: u64,
    activity_loaded: bool,
    activity_busy: bool,
    activity_live: bool,
    activity_subscribed: bool,
    activity_next_at: f64,
    activity_seen: BTreeMap<u64, String>,
    latest_activity: String,
    feature_schema: String,
    feature_camera: String,
    feature_names: Vec<String>,
    feature_drafts: BTreeMap<String, String>,
    camera_drafts: BTreeMap<String, BTreeMap<String, String>>,
    destinations: Value,
    command_busy: bool,
    snapshot_busy: bool,
    preview_busy: bool,
    preview_url: Option<String>,
    preview_frame: String,
    analytics_busy: bool,
    analytics: Value,
    analytics_url: Option<String>,
    revision: u32,
}

thread_local! {
    static APP: RefCell<App> = RefCell::new(App::default());
}

fn window() -> Window {
    web_sys::window().expect("Capturefab requires a browser window")
}
fn document() -> Document {
    window().document().expect("Capturefab requires a document")
}
fn element(id: &str) -> Element {
    document()
        .get_element_by_id(id)
        .expect("client markup missing")
}
fn set_text(id: &str, text: &str) {
    let element = element(id);
    // Polling should not repeatedly announce unchanged live-region messages.
    if element.text_content().as_deref() != Some(text) {
        element.set_text_content(Some(text));
    }
}
fn visible(id: &str, show: bool) {
    let e = element(id);
    if show {
        let _ = e.remove_attribute("hidden");
    } else {
        let _ = e.set_attribute("hidden", "");
    }
}
fn input(id: &str) -> String {
    let e = element(id);
    if let Some(input) = e.dyn_ref::<HtmlInputElement>() {
        input.value()
    } else if let Some(select) = e.dyn_ref::<HtmlSelectElement>() {
        select.value()
    } else {
        String::new()
    }
}
fn set_input(id: &str, value: &str) {
    let e = element(id);
    if let Some(input) = e.dyn_ref::<HtmlInputElement>() {
        input.set_value(value);
    } else if let Some(select) = e.dyn_ref::<HtmlSelectElement>() {
        select.set_value(value);
    }
}
fn checked(id: &str) -> bool {
    element(id).unchecked_into::<HtmlInputElement>().checked()
}
fn text<'a>(value: &'a Value, name: &str) -> &'a str {
    value.get(name).and_then(Value::as_str).unwrap_or("")
}
fn list<'a>(value: &'a Value, name: &str) -> &'a [Value] {
    value
        .get(name)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}
fn number(value: &Value, name: &str) -> u64 {
    value.get(name).and_then(Value::as_u64).unwrap_or(0)
}
fn flag(value: &Value, name: &str) -> bool {
    value.get(name).and_then(Value::as_bool).unwrap_or(false)
}
fn value_text(value: &Value) -> String {
    match value {
        Value::Null => "—".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn grouped_number(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}
fn data_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}
fn session_duration(started: u64, collected: u64) -> String {
    let seconds = collected.saturating_sub(started) / 1000;
    let hours = seconds / 3600;
    let minutes = seconds % 3600 / 60;
    if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {:02}s", seconds % 60)
    } else {
        format!("{seconds}s")
    }
}
fn drop_percent(frames: u64, dropped: u64) -> f64 {
    let total = frames as f64 + dropped as f64;
    if total == 0.0 {
        0.0
    } else {
        dropped as f64 / total * 100.0
    }
}
fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn js_error(value: JsValue) -> String {
    value.as_string().unwrap_or_else(|| {
        "The server could not be reached. Check the connection and try again.".into()
    })
}
fn notice(message: &str, error: bool) {
    set_text("notice", message);
    element("notice").set_class_name(if error { "message error" } else { "message" });
}
fn camera(app: &App) -> Option<&Value> {
    list(&app.session, "cameras")
        .iter()
        .find(|c| text(&c["info"], "id") == app.selected)
}
fn update_disabled(app: &App) {
    if let Ok(buttons) = document().query_selector_all("button") {
        for i in 0..buttons.length() {
            let Some(button) = buttons.item(i).and_then(|n| n.dyn_into::<Element>().ok()) else {
                continue;
            };
            let action = button.get_attribute("data-action").unwrap_or_default();
            let requires_camera = button
                .class_name()
                .split_whitespace()
                .any(|c| c == "camera-control")
                || action == "set-feature"
                || action == "execute-feature"
                || action == "cancel-job";
            if (app.command_busy && !matches!(action.as_str(), "select-camera" | "refresh"))
                || (!app.connected && action != "refresh")
                || (requires_camera && camera(app).is_none())
            {
                let _ = button.set_attribute("disabled", "");
            } else {
                let _ = button.remove_attribute("disabled");
            }
        }
    }
}

async fn fetch(path: &str, body: Option<&Value>) -> Result<Response, String> {
    let options = RequestInit::new();
    options.set_method(if body.is_some() { "POST" } else { "GET" });
    options.set_mode(RequestMode::SameOrigin);
    options.set_credentials(RequestCredentials::Omit);
    options.set_cache(RequestCache::NoStore);
    if let Some(body) = body {
        options.set_body(&JsValue::from_str(&body.to_string()));
    }
    let request = Request::new_with_str_and_init(path, &options).map_err(js_error)?;
    if body.is_some() {
        request
            .headers()
            .set("Content-Type", "application/json")
            .map_err(js_error)?;
    }
    JsFuture::from(window().fetch_with_request(&request))
        .await
        .map_err(js_error)?
        .dyn_into::<Response>()
        .map_err(js_error)
}
async fn request(path: &str, body: Option<&Value>) -> Result<Value, String> {
    let response = fetch(path, body).await?;
    let status = response.status();
    let body = JsFuture::from(response.text().map_err(js_error)?)
        .await
        .map_err(js_error)?;
    let data: Value = serde_json::from_str(&body.as_string().unwrap_or_default())
        .map_err(|_| format!("The server returned an unreadable response (HTTP {status})."))?;
    if !response.ok() || data.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(data
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("The server rejected the request.")
            .to_string());
    }
    if data.get("version").and_then(Value::as_u64) != Some(1) {
        return Err("This client and server use different API versions.".into());
    }
    Ok(data.get("result").cloned().unwrap_or(Value::Null))
}

fn render_cameras(app: &App) {
    let mut html = String::new();
    let mut devices = list(&app.session, "devices").to_vec();
    for connected in list(&app.session, "cameras") {
        let info = &connected["info"];
        if !devices
            .iter()
            .any(|device| text(device, "id") == text(info, "id"))
        {
            devices.push(info.clone());
        }
    }
    for info in &devices {
        let id = text(info, "id");
        let connected = list(&app.session, "cameras")
            .iter()
            .any(|c| text(&c["info"], "id") == id);
        let selected = app.selected == id;
        let action = if connected {
            "select-camera"
        } else {
            "connect-camera"
        };
        let label = if !connected {
            "Connect"
        } else if selected {
            "Selected"
        } else {
            "View camera"
        };
        let name = format!("{} {}", text(info, "vendor"), text(info, "model"));
        html.push_str(&format!("<div class=\"camera-item{}\"><strong>{}</strong><span class=\"muted\">{}</span><span class=\"muted\">{} · {}</span><button class=\"small\" data-action=\"{action}\" data-camera=\"{}\">{label}</button></div>",
            if selected { " selected" } else { "" }, escape(name.trim()), escape(id),
            escape(text(info, "transport")), if connected { "connected" } else { "available" }, escape(id)));
    }
    if html.is_empty() {
        html = "<p class=\"empty-state\">No cameras found.<br>Discover cameras or connect by address.</p>".into();
    }
    element("camera-list").set_inner_html(&html);
}

fn render_jobs(app: &App) {
    let jobs = camera(app).map(|c| list(c, "jobs")).unwrap_or(&[]);
    let mut html = String::new();
    for job in jobs.iter().rev() {
        let id = number(job, "id");
        let status = text(job, "status");
        let cancel = if matches!(status, "pending" | "running") {
            format!(
                "<button class=\"small danger\" data-action=\"cancel-job\" data-job=\"{id}\">Stop job</button>"
            )
        } else {
            String::new()
        };
        let next = number(job, "next_at_ms");
        let next_label = if next > 0 && matches!(status, "pending" | "running") {
            format!("<p>Next frame: {}</p>", escape(&date_label(next)))
        } else {
            String::new()
        };
        let error = text(job, "error");
        html.push_str(&format!("<article class=\"job\"><div class=\"job-header\"><strong>Job #{id} · {}</strong>{cancel}</div><p class=\"path\">{}</p><p>{} / {} frames · every {:.3} s</p>{next_label}<p>{}</p>{}</article>",
            escape(status), escape(text(job, "output")), number(job, "captured"), number(job, "count"),
            number(job, "interval_ms") as f64 / 1000.0, escape(text(job, "last_file")),
            if error.is_empty() { String::new() } else { format!("<p class=\"error\">{}</p>", escape(error)) }));
    }
    if html.is_empty() {
        html = "<p class=\"empty-state\">No capture jobs for this camera.<br>Set an interval and schedule a timelapse.</p>".into();
    }
    element("job-list").set_inner_html(&html);
}
fn date_label(timestamp: u64) -> String {
    js_sys::Date::new(&JsValue::from_f64(timestamp as f64))
        .to_locale_string("default", &JsValue::UNDEFINED)
        .into()
}
fn log_date_label(log: &Value) -> String {
    if let Some(unix_ms) = log_time::unix_ms(log) {
        let date = js_sys::Date::new(&JsValue::from_f64(unix_ms as f64));
        if date.get_time().is_finite() {
            let local: String = date.to_locale_string("default", &JsValue::UNDEFINED).into();
            let iso: String = date.to_iso_string().into();
            return format!("{local} [{iso}]");
        }
    }
    let time = text(log, "time");
    format!(
        "{} (date and time zone unavailable)",
        if time.is_empty() { "—" } else { time }
    )
}

fn sync_feature_camera(app: &mut App) {
    if app.feature_camera != app.selected {
        if !app.feature_camera.is_empty() {
            app.camera_drafts.insert(
                app.feature_camera.clone(),
                std::mem::take(&mut app.feature_drafts),
            );
        }
        app.feature_camera = app.selected.clone();
        app.feature_drafts = app.camera_drafts.remove(&app.selected).unwrap_or_default();
    }
}

fn render_features(app: &mut App) {
    sync_feature_camera(app);
    let features = camera(app)
        .map(|c| list(c, "features").to_vec())
        .unwrap_or_default();
    // Snapshot polling updates the reported value independently of the draft.
    // Never replace an operator's partially edited feature value every second.
    let schema = features
        .iter()
        .map(|f| json!([f["name"], f["kind"], f["writable"], f["choices"]]))
        .collect::<Vec<_>>();
    let schema = format!("{}:{}", app.selected, json!(schema));
    if app.feature_schema != schema {
        app.feature_schema = schema;
        app.feature_names = features
            .iter()
            .map(|f| text(f, "name").to_string())
            .collect();
        let mut html = String::new();
        for (index, feature) in features.iter().enumerate() {
            let name = text(feature, "name");
            let display = text(feature, "display_name");
            let value = value_text(&feature["value"]);
            let draft = app
                .feature_drafts
                .get(name)
                .cloned()
                .unwrap_or_else(|| value.clone());
            let mut bounds = Vec::new();
            if !feature["min"].is_null() {
                bounds.push(format!("min {}", value_text(&feature["min"])));
            }
            if !feature["max"].is_null() {
                bounds.push(format!("max {}", value_text(&feature["max"])));
            }
            if !feature["unit"].is_null() {
                bounds.push(value_text(&feature["unit"]));
            }
            let editor = if !flag(feature, "writable") {
                "<span class=\"muted\">Read only</span>".into()
            } else if text(feature, "kind") == "Command" {
                format!(
                    "<button data-action=\"execute-feature\" data-feature=\"{index}\">Execute</button>"
                )
            } else {
                let choices = list(feature, "choices");
                let control = if !choices.is_empty() || text(feature, "kind") == "Boolean" {
                    let choices = if choices.is_empty() {
                        vec![json!("false"), json!("true")]
                    } else {
                        choices.to_vec()
                    };
                    let options = choices
                        .iter()
                        .map(|choice| {
                            let choice = value_text(choice);
                            format!(
                                "<option value=\"{}\"{}>{}</option>",
                                escape(&choice),
                                if choice == draft { " selected" } else { "" },
                                escape(&choice)
                            )
                        })
                        .collect::<String>();
                    format!(
                        "<select id=\"feature-{index}\" aria-label=\"{}\">{options}</select>",
                        escape(name)
                    )
                } else {
                    format!(
                        "<input id=\"feature-{index}\" value=\"{}\" aria-label=\"{}\" spellcheck=\"false\">",
                        escape(if draft == "—" { "" } else { &draft }),
                        escape(name)
                    )
                };
                format!(
                    "{control}<button data-action=\"set-feature\" data-feature=\"{index}\">Apply</button>"
                )
            };
            let description = format!("{} {}", text(feature, "description"), bounds.join(" · "));
            let search = format!("{name} {display} {description}").to_lowercase();
            html.push_str(&format!("<div class=\"feature\" data-search=\"{}\"><div class=\"feature-label\"><strong>{}</strong><span class=\"muted\" title=\"{}\">{} · {}</span></div><span id=\"feature-value-{index}\" class=\"feature-value\"></span><div class=\"feature-editor\">{editor}</div></div>",
                escape(&search), escape(if display.is_empty() { name } else { display }), escape(&description), escape(name), escape(&bounds.join(" · "))));
        }
        if html.is_empty() {
            html = "<p class=\"empty-state\">Connect a camera to inspect its features.</p>".into();
        }
        element("feature-list").set_inner_html(&html);
        filter_features();
    }
    for (index, feature) in features.iter().enumerate() {
        let e = element(&format!("feature-value-{index}"));
        let error = text(feature, "error");
        let value = value_text(&feature["value"]);
        e.set_text_content(Some(if error.is_empty() { &value } else { error }));
        if flag(feature, "writable")
            && text(feature, "kind") != "Command"
            && !app.feature_drafts.contains_key(text(feature, "name"))
        {
            set_input(
                &format!("feature-{index}"),
                if value == "—" { "" } else { &value },
            );
        }
        e.set_class_name(if error.is_empty() {
            "feature-value"
        } else {
            "feature-value error"
        });
    }
}
fn filter_features() {
    let filter = input("feature-filter").to_lowercase();
    if let Ok(rows) = element("feature-list").query_selector_all("[data-search]") {
        for i in 0..rows.length() {
            let Some(row) = rows.item(i).and_then(|n| n.dyn_into::<Element>().ok()) else {
                continue;
            };
            if row
                .get_attribute("data-search")
                .unwrap_or_default()
                .contains(&filter)
            {
                let _ = row.remove_attribute("hidden");
            } else {
                let _ = row.set_attribute("hidden", "");
            }
        }
    }
}
fn reconcile_camera_selection(app: &mut App) {
    if app.selected.is_empty() {
        app.selected = list(&app.session, "cameras")
            .first()
            .map(|c| text(&c["info"], "id").to_string())
            .unwrap_or_default();
    }
}

fn render(app: &mut App) {
    reconcile_camera_selection(app);
    render_cameras(app);
    if let Some(camera) = camera(app) {
        let info = &camera["info"];
        set_text(
            "camera-title",
            &format!("{} {}", text(info, "vendor"), text(info, "model")),
        );
        set_text(
            "camera-subtitle",
            &format!(
                "{} · {} · {}",
                text(info, "id"),
                text(info, "transport"),
                text(info, "serial")
            ),
        );
        let fps = camera.get("fps").and_then(Value::as_f64).unwrap_or(0.0);
        set_text(
            "stream-status",
            &format!(
                "{} · {fps:.1} fps{}",
                if flag(camera, "streaming") {
                    "Streaming"
                } else {
                    "Stream stopped"
                },
                if camera.get("auto").is_some_and(|a| !a.is_null()) {
                    " · auto exposure"
                } else {
                    ""
                }
            ),
        );
        set_text(
            "frame-stats",
            &format!(
                "{} frames · {} dropped",
                number(camera, "frames"),
                number(camera, "dropped")
            ),
        );
        if let Some(error) = camera.get("last_error").and_then(Value::as_str) {
            notice(error, true);
        }
    } else {
        set_text(
            "camera-title",
            if app.selected.is_empty() {
                "Select a camera"
            } else {
                "Camera disconnected"
            },
        );
        set_text(
            "camera-subtitle",
            if app.selected.is_empty() {
                "Discover or connect a camera to get started."
            } else {
                &app.selected
            },
        );
        set_text("stream-status", "No camera connected");
        set_text("frame-stats", "0 frames · 0 dropped");
        clear_preview(app);
    }
    render_jobs(app);
    render_features(app);
    render_activity(app);
    let logs = list(&app.session, "logs");
    set_text("log-count", &format!("({})", logs.len()));
    let lines = logs
        .iter()
        .rev()
        .take(100)
        .map(|log| {
            format!(
                "{} [{}] {}",
                log_date_label(log),
                text(log, "level"),
                text(log, "message")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    set_text("logs", &lines);
    update_disabled(app);
}

fn clear_preview(app: &mut App) {
    if let Some(url) = app.preview_url.take() {
        let _ = Url::revoke_object_url(&url);
    }
    app.preview_frame.clear();
    let _ = element("preview").remove_attribute("src");
    visible("preview", false);
    visible("preview-empty", true);
}
fn connection_succeeded(app: &mut App) -> bool {
    let reconnected = !app.connected;
    if reconnected {
        app.connected = true;
        app.generation = app.generation.wrapping_add(1);
        app.snapshot_busy = false;
        app.preview_busy = false;
        app.activity_busy = false;
        app.activity_live = false;
        app.activity_subscribed = false;
        app.activity_next_at = 0.0;
    }
    set_text("server-status", "Connected");
    element("connection-dot").set_class_name("dot");
    if app.connection_error {
        notice("", false);
        app.connection_error = false;
    }
    reconnected
}

fn connection_failed(app: &mut App, error: &str) {
    if app.connected {
        app.connected = false;
        // Invalidate pending previews and server details when the connection
        // changes, so old responses cannot overwrite a reconnected session.
        app.generation = app.generation.wrapping_add(1);
        app.snapshot_busy = false;
        app.preview_busy = false;
        app.activity_busy = false;
        app.activity_live = false;
        app.activity_subscribed = false;
        app.activity_next_at = 0.0;
        app.command_busy = false;
        clear_preview(app);
    }
    app.connection_error = true;
    set_text("server-status", "Reconnecting…");
    element("connection-dot").set_class_name("dot offline");
    notice(error, true);
    analytics_unavailable(
        app,
        "The server is reconnecting. Analytics will resume automatically.",
    );
    update_disabled(app);
    render_activity(app);
}

fn render_analytics(app: &App) {
    let snapshot = &app.analytics;
    let totals = &snapshot["totals"];
    let frames = number(totals, "frames");
    let dropped = number(totals, "dropped");
    set_text("analytics-frames", &grouped_number(frames));
    set_text(
        "analytics-dropped",
        &format!("{:.2}%", drop_percent(frames, dropped)),
    );
    set_text(
        "analytics-drop-detail",
        &format!("{} missed frames", grouped_number(dropped)),
    );
    set_text("analytics-bytes", &data_size(number(totals, "bytes")));
    set_text(
        "analytics-duration",
        &session_duration(
            number(snapshot, "started_at_ms"),
            number(snapshot, "collected_at_ms"),
        ),
    );
    set_text(
        "analytics-started",
        &format!("Started {}", date_label(number(snapshot, "started_at_ms"))),
    );
    set_text("analytics-status", "Live · refreshes every 5 s");
    element("analytics-status").set_class_name("analytics-status live");

    let has_activity = frames > 0 || dropped > 0;
    visible(
        "analytics-empty",
        !has_activity || app.analytics_url.is_none(),
    );
    visible(
        "analytics-chart",
        has_activity && app.analytics_url.is_some(),
    );
    set_text(
        "analytics-empty-description",
        if has_activity {
            "The timeline is temporarily unavailable. Session totals and camera statistics are shown above and below."
        } else {
            "Start a stream or capture a frame to see frame rate, throughput and missed frames."
        },
    );
    let hours = number(snapshot, "retention_seconds") / 3600;
    set_text(
        "analytics-chart-caption",
        &format!(
            "Totals cover all cameras since this server started. Timeline history stays in memory for at most {hours} hours and clears on restart. Missed frames include transport losses and overwritten preview frames.{}",
            if flag(snapshot, "truncated") {
                " The oldest samples have been removed to keep history bounded."
            } else {
                ""
            },
        ),
    );
    let _ = element("analytics-chart").set_attribute("alt", &format!(
        "Capture timeline for all cameras: frame rate, acquired data rate, missed frame rate and acquired data volume by camera. This session has {} acquired frames, {} missed frames and {} of camera data.",
        grouped_number(frames), grouped_number(dropped), data_size(number(totals, "bytes")),
    ));

    let cameras = list(snapshot, "cameras");
    let mut rows = String::new();
    for camera in cameras {
        let streaming = flag(camera, "streaming");
        let stale = flag(camera, "stale");
        let state = if stale {
            "No recent data"
        } else if streaming {
            "Streaming"
        } else {
            "Idle"
        };
        let name = text(camera, "name");
        let id = text(camera, "id");
        let fps = camera.get("fps").and_then(Value::as_f64).unwrap_or(0.0);
        let rate = camera
            .get("bytes_per_second")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let frames = number(camera, "frames");
        let dropped = number(camera, "dropped");
        rows.push_str(&format!(
            "<tr><th scope=\"row\" title=\"{}\">{}<span class=\"muted\">{state}</span></th><td>{}</td><td>{}</td><td>{}</td><td>{:.2}%</td></tr>",
            escape(id), escape(if name.is_empty() { id } else { name }), grouped_number(frames),
            if streaming && !stale { format!("{fps:.1} fps") } else { "—".into() },
            if streaming && !stale { format!("{}/s", data_size(rate.max(0.0) as u64)) } else { "—".into() },
            drop_percent(frames, dropped),
        ));
    }
    element("analytics-cameras").set_inner_html(&format!(
        "<table><caption class=\"sr-only\">Session totals and current acquisition rates by camera. Missed frame percentages include transport losses and overwritten preview frames.</caption><thead><tr><th scope=\"col\">Camera</th><th scope=\"col\">Frames</th><th scope=\"col\">Now</th><th scope=\"col\">Data rate</th><th scope=\"col\">Missed</th></tr></thead><tbody>{rows}</tbody></table>"
    ));
    visible("analytics-cameras", !cameras.is_empty());
}
fn analytics_unavailable(app: &App, reason: &str) {
    set_text(
        "analytics-status",
        if app.analytics.is_null() {
            "Unavailable · retrying"
        } else {
            "Last available snapshot"
        },
    );
    element("analytics-status").set_class_name("analytics-status");
    set_text("analytics-warning", reason);
    visible("analytics-warning", true);
    if app.analytics.is_null() {
        set_text(
            "analytics-empty-description",
            "Analytics are temporarily unavailable. This panel will retry automatically.",
        );
    }
}
fn clear_analytics_chart(app: &mut App) {
    if let Some(url) = app.analytics_url.take() {
        let _ = Url::revoke_object_url(&url);
    }
    let _ = element("analytics-chart").remove_attribute("src");
    visible("analytics-chart", false);
}
async fn analytics_chart_url() -> Result<String, String> {
    let response = fetch("/api/analytics.svg", None).await?;
    if !response.ok() {
        return Err(
            "The capture timeline could not be loaded. It will retry automatically.".into(),
        );
    }
    let blob = JsFuture::from(response.blob().map_err(js_error)?)
        .await
        .map_err(js_error)?
        .dyn_into::<web_sys::Blob>()
        .map_err(js_error)?;
    let image = HtmlImageElement::new().map_err(js_error)?;
    let url = Url::create_object_url_with_blob(&blob).map_err(js_error)?;
    // Decode before replacing the last chart, so partial or invalid responses
    // cannot turn a useful timeline into a broken image.
    image.set_src(&url);
    if JsFuture::from(image.decode()).await.is_err() {
        let _ = Url::revoke_object_url(&url);
        return Err(
            "The capture timeline could not be displayed. It will retry automatically.".into(),
        );
    }
    Ok(url)
}
fn refresh_analytics() {
    let generation = APP.with(|state| {
        let mut app = state.borrow_mut();
        if !app.connected || app.analytics_busy || document().hidden() {
            return None;
        }
        app.analytics_busy = true;
        Some(app.generation)
    });
    let Some(generation) = generation else { return };
    spawn_local(async move {
        let snapshot = request("/api/analytics", None).await;
        let chart = if snapshot.as_ref().is_ok_and(|snapshot| {
            number(&snapshot["totals"], "frames") > 0 || number(&snapshot["totals"], "dropped") > 0
        }) {
            Some(analytics_chart_url().await)
        } else {
            None
        };
        APP.with(|state| {
            let mut app = state.borrow_mut();
            app.analytics_busy = false;
            if app.generation != generation {
                if let Some(Ok(url)) = chart { let _ = Url::revoke_object_url(&url); }
                return;
            }
            let Ok(snapshot) = snapshot else {
                analytics_unavailable(&app, "Analytics could not refresh. Any values shown are from the last available snapshot; retrying automatically.");
                return;
            };
            // Session totals may reset when the server restarts. Do not show a
            // chart from that previous process beside the new session totals.
            if number(&app.analytics, "started_at_ms") != number(&snapshot, "started_at_ms") || chart.is_none() {
                clear_analytics_chart(&mut app);
            }
            app.analytics = snapshot;
            let chart_error = match chart {
                Some(Ok(url)) => {
                    if let Some(old) = app.analytics_url.replace(url.clone()) { let _ = Url::revoke_object_url(&old); }
                    element("analytics-chart").unchecked_into::<HtmlImageElement>().set_src(&url);
                    None
                }
                Some(Err(error)) => Some(error),
                None => None,
            };
            render_analytics(&app);
            visible("analytics-warning", chart_error.is_some());
            if let Some(error) = chart_error {
                set_text("analytics-warning", &format!("{error} Any timeline shown is from the last successful refresh."));
            }
        });
    });
}

fn load_server_details(generation: u32) {
    spawn_local(async move {
        let result = request("/api/destinations", None).await;
        APP.with(|state| {
            let mut app = state.borrow_mut();
            if app.generation != generation {
                return;
            }
            if let Ok(destinations) = result {
                app.destinations = destinations;
                let mut options = "<option value=\"\">Server filesystem</option>".to_string();
                for destination in list(&app.destinations, "destinations") {
                    options.push_str(&format!(
                        "<option value=\"{}\">{}</option>",
                        escape(text(destination, "name")),
                        escape(text(destination, "name"))
                    ));
                }
                let selected = input("destination");
                element("destination").set_inner_html(&options);
                set_input("destination", &selected);
                destination_description(&app);
            }
        });
        let result = request("/api/storage", None).await;
        if APP.with(|state| state.borrow().generation != generation) {
            return;
        }
        match result {
            Ok(storage) => {
                let used = storage
                    .get("allocated_bytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let max = storage
                    .get("policy")
                    .and_then(|p| p.get("max_bytes"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                set_text(
                    "storage-summary",
                    &if max > 0 {
                        format!(
                            "{:.1} / {:.1} GiB",
                            used as f64 / 1_073_741_824.0,
                            max as f64 / 1_073_741_824.0
                        )
                    } else {
                        "Server storage policy applies.".into()
                    },
                );
            }
            Err(_) => set_text("storage-summary", "Storage status unavailable."),
        }
    });
}
fn destination_description(app: &App) {
    let selected = input("destination");
    let label = list(&app.destinations, "destinations")
        .iter()
        .find(|d| text(d, "name") == selected)
        .map(|d| {
            format!(
                "Server destination: {}. Enter a filename or relative path inside it.",
                text(d, "location")
            )
        })
        .unwrap_or_else(|| {
            "Use a path on the server. The server must be able to write to it.".into()
        });
    set_text("destination-description", &label);
}

fn refresh() {
    let ticket = APP.with(|state| {
        let mut app = state.borrow_mut();
        if app.snapshot_busy {
            return None;
        }
        app.snapshot_busy = true;
        Some((app.generation, app.revision))
    });
    let Some((generation, revision)) = ticket else {
        return;
    };
    spawn_local(async move {
        let result = request("/api/session", None).await;
        let details = APP.with(|state| {
            let mut app = state.borrow_mut();
            if app.generation != generation {
                return None;
            }
            app.snapshot_busy = false;
            if app.revision != revision {
                return None;
            }
            match result {
                Ok(snapshot) => {
                    sync_snapshot_activity(&mut app, &snapshot);
                    app.session = snapshot;
                    let reconnected = connection_succeeded(&mut app);
                    render(&mut app);
                    reconnected.then_some(app.generation)
                }
                Err(error) => {
                    connection_failed(&mut app, &error);
                    None
                }
            }
        });
        if let Some(generation) = details {
            load_server_details(generation);
            refresh_analytics();
            refresh_activity();
        }
    });
}

fn sync_snapshot_activity(app: &mut App, snapshot: &Value) {
    let revision = number(snapshot, "activity_revision");
    // The activity stream may already be ahead of an in-flight session read.
    if revision >= app.activity_revision || !app.activity_loaded {
        apply_activity(app, revision, list(snapshot, "activity"));
    }
}

/// Listen for activity even while a capture command waits. Short idle waits
/// release HTTP/1 connections regularly: several tabs share the browser's
/// small connection pool, including commands, snapshots and previews.
fn refresh_activity() {
    let ticket = APP.with(|state| {
        let mut app = state.borrow_mut();
        if !app.connected || app.activity_busy || js_sys::Date::now() < app.activity_next_at {
            return None;
        }
        app.activity_busy = true;
        Some((
            app.generation,
            app.activity_subscribed,
            app.activity_revision,
        ))
    });
    let Some((generation, loaded, revision)) = ticket else {
        return;
    };
    spawn_local(async move {
        let path = if loaded {
            format!("/api/activity?after={revision}&timeout_ms=500")
        } else {
            "/api/activity?timeout_ms=500".into()
        };
        let result = request(&path, None).await;
        let outcome = APP.with(|state| {
            let mut app = state.borrow_mut();
            if app.generation != generation {
                return None;
            }
            app.activity_busy = false;
            match result {
                Ok(update) => {
                    let changed = !app.activity_subscribed
                        || number(&update, "revision") != app.activity_revision;
                    app.activity_live = true;
                    app.activity_subscribed = true;
                    // Leave room for this tab and other tabs to use the
                    // released connection before opening another wait.
                    app.activity_next_at = js_sys::Date::now() + 100.0;
                    apply_activity(
                        &mut app,
                        number(&update, "revision"),
                        list(&update, "activity"),
                    );
                    render_activity(&app);
                    Some(changed)
                }
                Err(_) => {
                    // Session polling still supplies activity during a stream
                    // outage; the interval will reopen this request shortly.
                    app.activity_live = false;
                    render_activity(&app);
                    None
                }
            }
        });
        if let Some(changed) = outcome {
            if changed {
                refresh();
            }
            let next = Closure::once_into_js(refresh_activity);
            let _ = window()
                .set_timeout_with_callback_and_timeout_and_arguments_0(next.unchecked_ref(), 100);
        }
    });
}
fn preview() {
    let ticket = APP.with(|state| {
        let mut app = state.borrow_mut();
        if !app.connected || app.preview_busy || camera(&app).is_none() || document().hidden() {
            return None;
        }
        app.preview_busy = true;
        let worker = camera(&app).map(|c| number(c, "worker_pid")).unwrap_or(0);
        Some((app.generation, app.selected.clone(), worker))
    });
    let Some((generation, selected, worker)) = ticket else {
        return;
    };
    spawn_local(async move {
        let encoded = js_sys::encode_uri_component(&selected)
            .as_string()
            .unwrap_or_default();
        let response = fetch(&format!("/api/preview?camera={encoded}"), None).await;
        let response = match response {
            Ok(response) if response.status() == 204 => {
                APP.with(|state| {
                    let mut app = state.borrow_mut();
                    if app.generation == generation {
                        app.preview_busy = false;
                        if app.selected == selected
                            && camera(&app).is_some_and(|c| number(c, "worker_pid") == worker)
                        {
                            clear_preview(&mut app);
                        }
                    }
                });
                return;
            }
            Ok(response) if response.ok() => response,
            _ => {
                APP.with(|state| {
                    let mut app = state.borrow_mut();
                    if app.generation == generation {
                        app.preview_busy = false;
                        if app.selected == selected
                            && camera(&app).is_some_and(|c| number(c, "worker_pid") == worker)
                        {
                            set_text("preview-status", "Preview unavailable; waiting for a frame");
                        }
                    }
                });
                return;
            }
        };
        let frame_id = response
            .headers()
            .get("X-Frame-Id")
            .ok()
            .flatten()
            .unwrap_or_default();
        let timestamp = response
            .headers()
            .get("X-Frame-Timestamp-Ns")
            .ok()
            .flatten()
            .unwrap_or_default();
        let identity = format!("{worker}:{frame_id}:{timestamp}");
        let result = match response.blob() {
            Ok(promise) => JsFuture::from(promise)
                .await
                .map_err(js_error)
                .and_then(|blob| blob.dyn_into::<web_sys::Blob>().map_err(js_error)),
            Err(error) => Err(js_error(error)),
        };
        APP.with(|state| {
            let mut app = state.borrow_mut();
            if app.generation != generation {
                return;
            }
            app.preview_busy = false;
            if app.selected != selected
                || !camera(&app).is_some_and(|c| number(c, "worker_pid") == worker)
            {
                return;
            }
            if !frame_id.is_empty() && app.preview_frame == identity {
                return;
            }
            let Ok(blob) = result else {
                return;
            };
            let Ok(url) = Url::create_object_url_with_blob(&blob) else {
                return;
            };
            if let Some(old) = app.preview_url.replace(url.clone()) {
                let _ = Url::revoke_object_url(&old);
            }
            app.preview_frame = identity;
            element("preview")
                .unchecked_into::<HtmlImageElement>()
                .set_src(&url);
            visible("preview", true);
            visible("preview-empty", false);
            set_text(
                "preview-status",
                "Latest server frame · preview up to 4 fps",
            );
        });
    });
}

fn command(command: Value, camera_id: Option<String>, success: &str, reload_features: bool) {
    let connecting = text(&command, "op") == "connect";
    let set_feature = (text(&command, "op") == "set").then(|| {
        (
            text(&command, "feature").to_string(),
            text(&command, "value").to_string(),
        )
    });
    let target_camera = camera_id.clone();
    let generation = APP.with(|state| {
        let mut app = state.borrow_mut();
        if !app.connected || app.command_busy {
            return None;
        }
        app.command_busy = true;
        app.revision = app.revision.wrapping_add(1);
        update_disabled(&app);
        Some((app.generation, app.selected.clone()))
    });
    let Some((generation, selection_at_submit)) = generation else {
        return;
    };
    let success = success.to_string();
    notice("Working…", false);
    let mut body = json!({"version": 1, "command": command});
    APP.with(|state| {
        let app = state.borrow();
        body["actor"] = json!({"id": app.actor_id, "name": app.actor_name, "kind": "human"});
    });
    if let Some(camera) = camera_id {
        body["camera"] = json!(camera);
    }
    spawn_local(async move {
        let result = request("/api/command", Some(&body)).await;
        let snapshot = request("/api/session", None).await;
        APP.with(|state| {
            let mut app = state.borrow_mut();
            if app.generation != generation {
                return;
            }
            app.command_busy = false;
            let snapshot_fetched = snapshot.is_ok();
            let snapshot_error = match snapshot {
                Ok(snapshot) => {
                    sync_snapshot_activity(&mut app, &snapshot);
                    app.session = snapshot;
                    connection_succeeded(&mut app);
                    None
                }
                Err(error) => Some(error),
            };
            if connecting
                && app.selected == selection_at_submit
                && let Ok(result) = &result
                && let Some(id) = result
                    .get("connected")
                    .and_then(|c| c.get("id"))
                    .and_then(Value::as_str)
            {
                clear_preview(&mut app);
                app.selected = id.to_string();
            }
            if result.is_ok()
                && let Some((feature, submitted_value)) = set_feature
            {
                if target_camera.as_deref() == Some(app.feature_camera.as_str()) {
                    if app.feature_drafts.get(&feature) == Some(&submitted_value) {
                        app.feature_drafts.remove(&feature);
                    }
                } else if let Some(drafts) = target_camera
                    .as_ref()
                    .and_then(|camera| app.camera_drafts.get_mut(camera))
                    && drafts.get(&feature) == Some(&submitted_value)
                {
                    drafts.remove(&feature);
                }
                if !snapshot_fetched
                    && let Ok(result) = &result
                    && let Some(value) = result.get("value")
                    && let Some(cameras) =
                        app.session.get_mut("cameras").and_then(Value::as_array_mut)
                    && let Some(camera) = cameras.iter_mut().find(|c| {
                        text(&c["info"], "id") == target_camera.as_deref().unwrap_or_default()
                    })
                    && let Some(features) = camera.get_mut("features").and_then(Value::as_array_mut)
                    && let Some(feature) = features.iter_mut().find(|f| text(f, "name") == feature)
                {
                    feature["value"] = value.clone();
                }
            }
            if reload_features
                && result.is_ok()
                && target_camera.as_deref() == Some(app.feature_camera.as_str())
            {
                app.feature_schema.clear();
            }
            render(&mut app);
            if let Some(error) = snapshot_error {
                connection_failed(&mut app, &error);
                return;
            }
            match result {
                Ok(result) => {
                    let message = if let Some(files) = result.get("files").and_then(Value::as_array)
                    {
                        format!("{success} {} file(s) saved on the server.", files.len())
                    } else {
                        success
                    };
                    notice(&message, false);
                }
                Err(error) => notice(&error, true),
            }
        });
    });
}

fn integer_input(id: &str, min: u64, max: u64, label: &str) -> Result<u64, String> {
    input(id)
        .parse::<u64>()
        .ok()
        .filter(|n| (min..=max).contains(n))
        .ok_or_else(|| format!("{label} must be a whole number from {min} to {max}."))
}
fn capture_command(schedule: bool) -> Result<Value, String> {
    let output = input("output").trim().to_string();
    if output.is_empty() || output == "-" {
        return Err(
            "Enter an output path on the server or a filename in a saved destination.".into(),
        );
    }
    let count = integer_input("count", 1, 100_000, "Frame count")?;
    let background_capture = !schedule && count > 1;
    let max_mib = integer_input("max-mib", 1, 16_777_216, "Storage limit in MiB")?;
    let max_files = integer_input("max-files", 1, 100_000, "Maximum files")?;
    let mut command = json!({"op": if schedule || background_capture { "schedule" } else { "capture" }, "output": output, "count": count,
        "timeout_ms": 10000, "format": input("format"), "storage": {"max_bytes": max_mib * 1_048_576, "max_files": max_files, "on_full": "stop"}});
    let destination = input("destination");
    if !destination.is_empty() {
        command["destination"] = json!(destination);
    }
    if schedule {
        let seconds = input("interval")
            .parse::<f64>()
            .map_err(|_| "Enter a valid interval in seconds.")?;
        let millis = seconds * 1000.0;
        if !millis.is_finite()
            || !(1.0..=315_576_000_000.0).contains(&millis)
            || (millis - millis.round()).abs() > 0.00001
        {
            return Err(
                "The interval must be at least 0.001 seconds and resolve to whole milliseconds."
                    .into(),
            );
        }
        let at = input("first-at");
        let first = if at.is_empty() {
            js_sys::Date::now()
        } else {
            js_sys::Date::new(&JsValue::from_str(&at)).get_time()
        };
        if !first.is_finite() || first < 0.0 {
            return Err("Enter a valid local start time, or leave it empty to start now.".into());
        }
        command["first_at_ms"] = json!(first as u64);
        command["interval_ms"] = json!(millis.round() as u64);
    } else if background_capture {
        command["first_at_ms"] = json!(js_sys::Date::now() as u64);
        command["interval_ms"] = json!(0);
    }
    Ok(command)
}
fn selected_camera() -> Option<String> {
    APP.with(|state| {
        let app = state.borrow();
        camera(&app).map(|_| app.selected.clone())
    })
}
fn dispatch(action: &str, source: &Element) {
    match action {
        "refresh" => {
            refresh();
            refresh_analytics();
            let details = APP.with(|s| {
                let a = s.borrow();
                a.connected.then_some(a.generation)
            });
            if let Some(generation) = details {
                load_server_details(generation);
            }
        }
        "discover" => command(
            json!({"op": "discover", "timeout_ms": 1000, "simulated": checked("simulated")}),
            None,
            "Camera discovery complete.",
            false,
        ),
        "connect-manual" | "connect-camera" => {
            let id = if action == "connect-manual" {
                input("camera-selector").trim().to_string()
            } else {
                source.get_attribute("data-camera").unwrap_or_default()
            };
            if id.is_empty() {
                notice("Enter a camera IP, serial, or ID.", true);
                return;
            }
            command(
                json!({"op": "connect", "camera": id, "timeout_ms": 3000}),
                None,
                "Camera connected.",
                false,
            );
        }
        "select-camera" => APP.with(|state| {
            let mut app = state.borrow_mut();
            app.selected = source.get_attribute("data-camera").unwrap_or_default();
            app.feature_schema.clear();
            clear_preview(&mut app);
            render(&mut app);
        }),
        "capture" | "schedule" => match capture_command(action == "schedule") {
            Ok(body) => {
                if let Some(camera) = selected_camera() {
                    let success = if text(&body, "op") == "schedule" {
                        "Capture job scheduled on the server."
                    } else {
                        "Capture complete."
                    };
                    command(body, Some(camera), success, false);
                }
            }
            Err(error) => notice(&error, true),
        },
        "set-feature" | "execute-feature" => {
            let Some(index) = source
                .get_attribute("data-feature")
                .and_then(|v| v.parse::<usize>().ok())
            else {
                return;
            };
            let name = APP.with(|state| {
                let app = state.borrow();
                camera(&app)
                    .and_then(|c| list(c, "features").get(index))
                    .map(|f| text(f, "name").to_string())
            });
            let Some(name) = name else { return };
            let body = if action == "execute-feature" {
                json!({"op": "execute", "feature": name})
            } else {
                json!({"op": "set", "feature": name, "value": input(&format!("feature-{index}"))})
            };
            if let Some(camera) = selected_camera() {
                command(body, Some(camera), "Camera feature updated.", false);
            }
        }
        "cancel-job" => {
            let Some(id) = source
                .get_attribute("data-job")
                .and_then(|v| v.parse::<u64>().ok())
            else {
                return;
            };
            if let Some(camera) = selected_camera() {
                command(
                    json!({"op": "cancel_job", "id": id}),
                    Some(camera),
                    "Capture job stopped.",
                    false,
                );
            }
        }
        "disconnect" | "start" | "stop" | "features" | "jobs" | "auto" | "manual" => {
            let Some(camera) = selected_camera() else {
                return;
            };
            let success = match action {
                "disconnect" => "Camera disconnected.",
                "start" => "Camera streaming.",
                "stop" => "Camera stream stopped.",
                "features" => "Camera features reloaded.",
                "jobs" => "Capture jobs refreshed.",
                "auto" => "Automatic exposure enabled.",
                _ => "Manual control enabled.",
            };
            command(
                json!({"op": action}),
                Some(camera),
                success,
                action == "features",
            );
        }
        _ => {}
    }
}

#[wasm_bindgen(start)]
pub fn start() -> Result<(), JsValue> {
    let clicks = Closure::<dyn FnMut(Event)>::new(|event: Event| {
        let Some(target) = event.target().and_then(|t| t.dyn_into::<Element>().ok()) else {
            return;
        };
        if let Ok(Some(button)) = target.closest("button[data-action]") {
            if button.has_attribute("disabled") {
                return;
            }
            let action = button.get_attribute("data-action").unwrap_or_default();
            event.prevent_default();
            dispatch(&action, &button);
        }
    });
    document().add_event_listener_with_callback("click", clicks.as_ref().unchecked_ref())?;
    clicks.forget();
    let submits = Closure::<dyn FnMut(Event)>::new(|event: Event| {
        let Some(form) = event.target().and_then(|t| t.dyn_into::<Element>().ok()) else {
            return;
        };
        let Some(action) = form.get_attribute("data-action") else {
            return;
        };
        event.prevent_default();
        dispatch(&action, &form);
    });
    document().add_event_listener_with_callback("submit", submits.as_ref().unchecked_ref())?;
    submits.forget();
    let inputs = Closure::<dyn FnMut(Event)>::new(|event: Event| {
        let Some(target) = event.target().and_then(|t| t.dyn_into::<Element>().ok()) else {
            return;
        };
        if target.id() == "feature-filter" {
            filter_features();
        }
        if target.id() == "destination" {
            APP.with(|state| destination_description(&state.borrow()));
        }
        if target.id() == "actor-name" {
            APP.with(|state| {
                let mut app = state.borrow_mut();
                let name = input("actor-name").trim().to_string();
                if !name.is_empty() && name.len() <= 128 && !name.chars().any(char::is_control) {
                    app.actor_name = name;
                }
                set_text(
                    "your-identity",
                    &format!("You appear as {}", app.actor_name),
                );
            });
        }
        if let Some(index) = target
            .id()
            .strip_prefix("feature-")
            .and_then(|s| s.parse::<usize>().ok())
        {
            APP.with(|state| {
                let mut app = state.borrow_mut();
                if let Some(name) = app.feature_names.get(index).cloned() {
                    app.feature_drafts.insert(name, input(&target.id()));
                }
            });
        }
    });
    document().add_event_listener_with_callback("input", inputs.as_ref().unchecked_ref())?;
    document().add_event_listener_with_callback("change", inputs.as_ref().unchecked_ref())?;
    inputs.forget();
    let snapshots = Closure::<dyn FnMut()>::new(refresh);
    window().set_interval_with_callback_and_timeout_and_arguments_0(
        snapshots.as_ref().unchecked_ref(),
        1000,
    )?;
    snapshots.forget();
    let previews = Closure::<dyn FnMut()>::new(preview);
    window().set_interval_with_callback_and_timeout_and_arguments_0(
        previews.as_ref().unchecked_ref(),
        250,
    )?;
    previews.forget();
    let analytics = Closure::<dyn FnMut()>::new(refresh_analytics);
    window().set_interval_with_callback_and_timeout_and_arguments_0(
        analytics.as_ref().unchecked_ref(),
        5000,
    )?;
    analytics.forget();

    let activity = Closure::<dyn FnMut()>::new(refresh_activity);
    window().set_interval_with_callback_and_timeout_and_arguments_0(
        activity.as_ref().unchecked_ref(),
        500,
    )?;
    activity.forget();

    let resumed = Closure::<dyn FnMut(Event)>::new(|_: Event| {
        if !document().hidden() {
            refresh();
            refresh_activity();
        }
    });
    document()
        .add_event_listener_with_callback("visibilitychange", resumed.as_ref().unchecked_ref())?;
    resumed.forget();

    notice("", false);
    APP.with(|state| {
        let mut app = state.borrow_mut();
        // Each loaded tab is a distinct participant, including duplicate tabs.
        app.actor_id = format!(
            "browser-{:x}-{:x}",
            js_sys::Date::now() as u64,
            (js_sys::Math::random() * (1u64 << 52) as f64) as u64
        );
        app.actor_name = format!("Browser {}", &app.actor_id[app.actor_id.len() - 6..]);
        set_input("actor-name", &app.actor_name);
        set_text(
            "your-identity",
            &format!("You appear as {}", app.actor_name),
        );
        update_disabled(&app);
    });
    refresh();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn server_strings_are_escaped_before_markup() {
        assert_eq!(
            escape("<script x=\"'\">&"),
            "&lt;script x=&quot;&#39;&quot;&gt;&amp;"
        );
    }
    #[test]
    fn analytics_formats_session_totals_without_losing_units() {
        assert_eq!(grouped_number(0), "0");
        assert_eq!(grouped_number(u64::MAX), "18,446,744,073,709,551,615");
        assert_eq!(data_size(1023), "1023 B");
        assert_eq!(data_size(1024), "1.0 KiB");
        assert_eq!(data_size(1_073_741_824), "1.0 GiB");
        assert_eq!(session_duration(5000, 4999), "0s");
        assert_eq!(session_duration(5000, 3666000), "1h 01m");
    }
    #[test]
    fn missed_fraction_uses_acquired_and_missed_without_overflow() {
        assert_eq!(drop_percent(0, 0), 0.0);
        assert_eq!(drop_percent(90, 10), 10.0);
        assert_eq!(drop_percent(u64::MAX, u64::MAX), 50.0);
    }

    #[test]
    fn another_participants_selection_or_disconnect_cannot_redirect_local_commands() {
        let mut app = App {
            selected: "sim:1".into(),
            session: json!({"active_camera": "sim:0", "cameras": [
                {"info": {"id": "sim:0"}}, {"info": {"id": "sim:1"}}
            ]}),
            ..Default::default()
        };
        reconcile_camera_selection(&mut app);
        assert_eq!(text(&camera(&app).unwrap()["info"], "id"), "sim:1");
        app.session["cameras"] = json!([{"info": {"id": "sim:0"}}]);
        reconcile_camera_selection(&mut app);
        assert_eq!(app.selected, "sim:1");
        assert!(camera(&app).is_none());
    }

    #[test]
    fn camera_drafts_survive_inspecting_other_cameras_and_remote_feature_changes() {
        let mut app = App {
            selected: "sim:0".into(),
            ..Default::default()
        };
        sync_feature_camera(&mut app);
        app.feature_drafts
            .insert("ExposureTime".into(), "1500".into());
        app.selected = "sim:1".into();
        sync_feature_camera(&mut app);
        app.feature_drafts.insert("Gain".into(), "2".into());
        app.session = json!({"cameras": [{"info": {"id": "sim:0"}, "features": [
            {"name": "ExposureTime", "value": "2000"}
        ]}]});
        app.selected = "sim:0".into();
        sync_feature_camera(&mut app);
        assert_eq!(app.feature_drafts["ExposureTime"], "1500");
        assert_eq!(app.camera_drafts["sim:1"]["Gain"], "2");
    }
}
