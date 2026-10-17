//! Live signals: the session's data sources with their latest values and
//! sparklines (Slice C). The GUI owns the session, so it reads the hub
//! directly each tick instead of the snapshot's brief copy.
use super::*;
use crate::signals::{SourceState, SourceStatus, TagStatus};
use iced::widget::{canvas, column};
use iced::{Point, Rectangle, Renderer, Theme, mouse};

/// Recent data source URIs kept in preferences.
const RECENT: usize = 8;
/// Tag rows shown per source before "+N more".
const ROWS: usize = 24;

/// What the signals section remembers between frames.
#[derive(Debug, Default)]
pub(super) struct SignalsUi {
    /// The URI typed into "Add data source".
    pub(super) input: String,
    /// Sources started here, without credentials, newest first.
    pub(super) recent: Vec<String>,
    /// The hub's sources as of the last tick, with recent points.
    pub(super) live: Vec<SourceStatus>,
    /// Whether samples go to a dataset or file.
    pub(super) persisting: bool,
    /// Why the last start or stop failed.
    pub(super) error: Option<String>,
    /// The URI of the start in flight.
    starting: Option<String>,
    /// Whether the inspector section was open at the last frame, so ticks
    /// fetch recent points only when sparklines are shown.
    shown: std::cell::Cell<bool>,
    /// Screenshot scenes show these instead of the hub.
    pub(super) fixture: Option<Vec<SourceStatus>>,
}

#[derive(Debug, Clone)]
pub(super) enum Msg {
    Input(String),
    Add,
    Start(String),
    Stop(String),
}

/// Only URIs without credentials or secrets are remembered.
fn storable(uri: &str) -> bool {
    crate::media::redact_url(uri) == uri
}

/// Whether `input` is worth an error style: typed and not a source URI.
fn invalid(input: &str) -> Option<String> {
    let typed = input.trim();
    if typed.is_empty() {
        return None;
    }
    crate::signals::parse(typed)
        .err()
        .map(|error| format!("{error:#}"))
}

impl SignalsUi {
    pub(super) fn start(saved: Option<&Prefs>) -> Self {
        Self {
            recent: saved
                .map(|prefs| {
                    prefs
                        .recent_signals
                        .iter()
                        .filter(|uri| storable(uri) && crate::signals::parse(uri).is_ok())
                        .take(RECENT)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default(),
            ..Self::default()
        }
    }
    fn remember(&mut self, uri: &str) {
        if !storable(uri) {
            return;
        }
        self.recent.retain(|r| r != uri);
        self.recent.insert(0, uri.to_string());
        self.recent.truncate(RECENT);
    }
}

fn summary(ui: &SignalsUi) -> String {
    if ui.live.is_empty() {
        return "No data sources".into();
    }
    let tags: u64 = ui.live.iter().map(|s| s.tags_total).sum();
    let sources = match ui.live.len() {
        1 => "1 source".to_string(),
        n => format!("{n} sources"),
    };
    format!("{sources} · {tags} tags")
}

fn state_color(state: SourceState, p: &Palette) -> Color {
    match state {
        SourceState::Running => p.live,
        SourceState::Connecting | SourceState::Reconnecting => p.warn,
        SourceState::Failed => p.danger,
        SourceState::Stopped => p.tertiary,
    }
}

/// Error text ink: only a source that gave up is drawn as a failure.
fn error_ink(state: SourceState, p: &Palette) -> Color {
    match state {
        SourceState::Failed => p.ink(p.danger),
        SourceState::Connecting | SourceState::Reconnecting => p.ink(p.warn),
        _ => p.secondary,
    }
}

/// A short label that tells sources on one host apart: the host, then the
/// path and the name or node chosen, e.g. `192.168.1.30 · 1/holding/10
/// (pressure)`. The URI is already redacted.
fn source_label(uri: &str) -> String {
    let rest = uri.split_once("://").map_or(uri, |(_, rest)| rest);
    let (location, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, path) = location.split_once('/').unwrap_or((location, ""));
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let mut label = host.to_string();
    let path = path.trim_matches('/');
    if !path.is_empty() {
        label.push_str(" · ");
        label.push_str(path);
    }
    let names: Vec<&str> = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(key, _)| matches!(*key, "name" | "node"))
        .map(|(_, value)| value)
        .collect();
    if !names.is_empty() {
        label.push_str(&format!(" ({})", names.join(", ")));
    }
    if label.is_empty() {
        uri.to_string()
    } else {
        label
    }
}

fn state_label(source: &SourceStatus) -> String {
    let state = match source.state {
        SourceState::Connecting => "Connecting",
        SourceState::Running => "Running",
        SourceState::Reconnecting => "Reconnecting",
        SourceState::Failed => "Failed",
        SourceState::Stopped => "Stopped",
    };
    match source.samples {
        0 => state.to_string(),
        1 => format!("{state} · 1 sample"),
        n => format!("{state} · {n} samples"),
    }
}

fn value_text(tag: &TagStatus) -> String {
    let value = match &tag.value {
        crate::signals::Value::Number(x) if x.fract() != 0.0 => {
            format!("{}", (x * 1000.0).round() / 1000.0)
        }
        value => value.to_string(),
    };
    match &tag.unit {
        Some(unit) => format!("{value} {unit}"),
        None => value,
    }
}

impl Workbench {
    pub(super) fn signals_update(&mut self, message: Msg) -> Task<Message> {
        match message {
            Msg::Input(text) => self.signals.input = text,
            Msg::Add => {
                let uri = self.signals.input.trim().to_string();
                if uri.is_empty() || invalid(&uri).is_some() {
                    self.signals.error = invalid(&uri);
                    return Task::none();
                }
                return self.signals_update(Msg::Start(uri));
            }
            Msg::Start(uri) => {
                self.signals.error = None;
                self.signals.starting = Some(uri.clone());
                self.send(
                    Job::SignalStart,
                    SessionCommand::SignalStart {
                        uri,
                        interval_ms: crate::signals::default_interval_ms(),
                        tags: Vec::new(),
                    },
                );
            }
            Msg::Stop(id) => {
                self.signals.error = None;
                self.send(Job::SignalStop, SessionCommand::SignalStop { id });
            }
        }
        Task::none()
    }

    /// Remember started sources and show why a start or stop failed.
    pub(super) fn signals_settled(
        &mut self,
        pending: &Pending,
        result: &Result<serde_json::Value>,
    ) {
        match (&pending.job, result) {
            (Job::SignalStart, Ok(_)) => {
                if let Some(uri) = self.signals.starting.take() {
                    self.signals.remember(&uri);
                    if self.signals.input.trim() == uri {
                        self.signals.input.clear();
                    }
                }
                self.tick_signals();
            }
            (Job::SignalStop, Ok(_)) => self.tick_signals(),
            (Job::SignalStart | Job::SignalStop, Err(error)) => {
                self.signals.starting = None;
                self.signals.error = Some(format!("{error:#}"));
            }
            _ => {}
        }
    }

    /// Read the hub; recent points only while the section is open.
    pub(super) fn tick_signals(&mut self) {
        if let Some(fixture) = &self.signals.fixture {
            self.signals.live = fixture.clone();
            self.signals.persisting = true;
            return;
        }
        let hub = self.handle.signals();
        self.signals.live = hub.status(self.signals.shown.get());
        self.signals.persisting = hub.has_sink();
    }

    /// The sidebar's "Data sources" list, when any are running.
    pub(super) fn sidebar_signals(&self, p: &'static Palette) -> Option<Element<'_, Message>> {
        if self.signals.live.is_empty() {
            return None;
        }
        let mut list = column![
            container(
                row![
                    text("Data sources")
                        .size(style::CAPTION)
                        .font(style::SEMIBOLD)
                        .color(p.secondary),
                    text(self.signals.live.len().to_string())
                        .size(style::CAPTION)
                        .color(p.secondary),
                ]
                .spacing(6),
            )
            .padding(iced::Padding {
                top: 14.0,
                right: sidebar::INSET,
                bottom: 4.0,
                left: sidebar::INSET,
            })
        ]
        .spacing(2);
        let busy = self.pending(Job::SignalStop);
        for source in &self.signals.live {
            let detail = source
                .error
                .as_deref()
                .filter(|_| source.state != SourceState::Running)
                .map(|e| format::first_line(e).to_string())
                .unwrap_or_else(|| state_label(source));
            let ink = error_ink(source.state, p);
            let full = match source.error.as_deref() {
                Some(error) if source.state != SourceState::Running => {
                    format!("{}\n{error}", source.uri)
                }
                _ => source.uri.clone(),
            };
            let line = row![
                container(widgets::dot(state_color(source.state, p), 8.0))
                    .width(16)
                    .center_x(16),
                tip(
                    column![
                        one_line(
                            source_label(&source.uri),
                            style::BODY,
                            style::MEDIUM,
                            p.text
                        )
                        .width(Fill),
                        one_line(detail, style::CAPTION, style::SANS, ink).width(Fill),
                    ]
                    .spacing(1)
                    .width(Fill),
                    full,
                ),
                tip(
                    button(icon(Icon::Stop, 11.0, p.secondary))
                        .name(format!("Stop logging {}", source.uri))
                        .padding(5)
                        .style(style::plain)
                        .on_press_maybe(
                            (!busy).then(|| Message::Signals(Msg::Stop(source.id.clone())))
                        ),
                    "Stop logging",
                ),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            list = list.push(container(line).padding([7.0, sidebar::INSET]));
        }
        Some(list.into())
    }
}

/// Two running sources for screenshot scenes: a simulated line with a
/// minute of history and a Modbus PLC that is reconnecting.
pub(super) fn fixture() -> Vec<SourceStatus> {
    use crate::signals::{Protocol, sim};
    let now = crate::scheduling::now_ms();
    let ticks = 60u64;
    let history: Vec<Vec<crate::signals::Sample>> = (0..ticks)
        .map(|tick| sim::values("line1", 6, tick * 3))
        .collect();
    let infos = sim::tags("line1", 6);
    let tags = infos
        .iter()
        .enumerate()
        .map(|(i, info)| {
            let last = &history[ticks as usize - 1][i];
            TagStatus {
                tag: info.tag.clone(),
                unit: info.unit.clone(),
                value: last.value.clone(),
                quality: last.quality,
                ts_ms: now - 400,
                source_ts_ms: None,
                recent: Some(
                    history
                        .iter()
                        .enumerate()
                        .filter_map(|(t, samples)| {
                            Some((now - (ticks - t as u64) * 1000, samples[i].value.as_f64()?))
                        })
                        .collect(),
                ),
            }
        })
        .collect();
    let source =
        |uri: &str, protocol, state, samples, error: Option<&str>, tags: Vec<TagStatus>| {
            SourceStatus {
                id: crate::signals::source_id(uri),
                protocol,
                uri: uri.into(),
                interval_ms: 1000,
                state,
                started_ms: now - 61_000,
                last_sample_ms: Some(now - 400),
                samples,
                written: samples,
                dropped: 0,
                helper_dropped: 0,
                restarts: 0,
                error: error.map(str::to_string),
                helper_pid: None,
                tags_total: tags.len() as u64,
                tags,
                tags_truncated: false,
            }
        };
    vec![
        source(
            "sim-signal://line1",
            Protocol::SimSignal,
            SourceState::Running,
            360,
            None,
            tags,
        ),
        source(
            "modbus-tcp://192.168.1.30/1/holding/0?count=2&type=f32",
            Protocol::ModbusTcp,
            SourceState::Reconnecting,
            0,
            Some("connect to Modbus server 192.168.1.30:502: Connection refused (os error 61)"),
            Vec::new(),
        ),
    ]
}

/// The inspector's section, or `None` to leave it out.
pub(super) fn view<'a>(
    bench: &'a Workbench,
    open: bool,
    p: &'static Palette,
) -> Option<Element<'a, Message>> {
    let ui = &bench.signals;
    ui.shown.set(open);
    let mut section = column![widgets::disclosure(
        "Live signals",
        Some(summary(ui)),
        if open { 1.0 } else { 0.0 },
        Message::InspectorDisclosure(inspector::Section::Signals),
        p,
    )]
    .spacing(inspector::ROW_GAP);
    if !open {
        return Some(section.into());
    }
    let starting = bench.pending(Job::SignalStart);
    let invalid = invalid(&ui.input);
    section = section.push(
        text_input("modbus-tcp://, mqtt://, opc.tcp://…", &ui.input)
            .name("Add data source")
            .description(
                "Enter a Modbus TCP, MQTT, OPC UA, MTConnect or sim-signal URI, then press Return to start logging.",
            )
            .invalid(invalid.clone())
            .icon(icon::input_icon(Icon::Plus))
            .on_input(|text| Message::Signals(Msg::Input(text)))
            .on_submit_maybe((!starting).then_some(Message::Signals(Msg::Add)))
            .size(style::BODY)
            .padding([7, 10])
            .style(if invalid.is_some() {
                style::input_invalid
            } else {
                style::input
            }),
    );
    let running: Vec<&str> = ui.live.iter().map(|s| s.uri.as_str()).collect();
    let recent: Vec<&String> = ui
        .recent
        .iter()
        .filter(|uri| !running.contains(&uri.as_str()))
        .collect();
    if ui.input.trim().is_empty() && !recent.is_empty() {
        let mut list = column![].spacing(2);
        for uri in recent {
            list = list.push(
                button(
                    row![
                        icon(Icon::Recent, 12.0, p.tertiary),
                        one_line(uri.as_str(), style::SMALL, style::SANS, p.text).width(Fill),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                )
                .name(format!("Start logging {uri}"))
                .width(Fill)
                .padding([4, 6])
                .style(style::plain)
                .on_press_maybe((!starting).then(|| Message::Signals(Msg::Start(uri.clone())))),
            );
        }
        section = section.push(list);
    }
    if let Some(error) = invalid.or_else(|| ui.error.clone()) {
        section = section.push(text(error).size(style::CAPTION).color(p.ink(p.danger)));
    }
    if !ui.live.is_empty() && !ui.persisting {
        section = section.push(
            text("Values are shown, not saved: open a dataset to keep them.")
                .size(style::CAPTION)
                .color(p.secondary),
        );
    }
    for source in &ui.live {
        section = section.push(source_view(source, p));
    }
    Some(section.into())
}

fn source_view<'a>(source: &'a SourceStatus, p: &'static Palette) -> Element<'a, Message> {
    let mut rows = column![
        row![
            widgets::dot(state_color(source.state, p), 8.0),
            tip(
                one_line(
                    source_label(&source.uri),
                    style::SMALL,
                    style::MEDIUM,
                    p.text
                )
                .width(Fill),
                source.uri.clone(),
            ),
            text(state_label(source))
                .size(style::CAPTION)
                .color(p.secondary),
        ]
        .spacing(6)
        .align_y(Alignment::Center)
    ]
    .spacing(3);
    if let Some(error) = source
        .error
        .as_deref()
        .filter(|_| source.state != SourceState::Running)
    {
        rows = rows.push(
            text(error.to_string())
                .size(style::CAPTION)
                .color(error_ink(source.state, p)),
        );
    }
    for tag in source.tags.iter().take(ROWS) {
        let ink = match tag.quality {
            crate::signals::Quality::Good => p.text,
            crate::signals::Quality::Uncertain => p.ink(p.warn),
            crate::signals::Quality::Bad => p.ink(p.danger),
        };
        let name = tag.tag.rsplit('/').next().unwrap_or(&tag.tag);
        let points: Vec<(u64, f64)> = tag.recent.clone().unwrap_or_default();
        rows = rows.push(
            row![
                tip(
                    one_line(name, style::SMALL, style::SANS, p.secondary).width(Fill),
                    tag.tag.clone(),
                ),
                container(one_line(value_text(tag), style::SMALL, style::MONO, ink))
                    .width(Length::FillPortion(1)),
                canvas(SignalSparkline {
                    points,
                    color: p.accent,
                })
                .width(64)
                .height(16),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
    }
    let hidden = (source.tags_total as usize).saturating_sub(ROWS.min(source.tags.len()));
    if hidden > 0 {
        rows = rows.push(
            text(format!("+{hidden} more"))
                .size(style::CAPTION)
                .color(p.secondary),
        );
    }
    container(rows).padding([4, 0]).into()
}

/// A tag's recent values, scaled between their own minimum and maximum so
/// negative readings (temperatures, offsets) draw correctly.
pub(super) struct SignalSparkline {
    pub(super) points: Vec<(u64, f64)>,
    pub(super) color: Color,
}

/// Points in a `size` box: time across, value up, flat when constant.
fn scale(points: &[(u64, f64)], size: iced::Size) -> Vec<Point> {
    let (Some(first), Some(last)) = (points.first(), points.last()) else {
        return Vec::new();
    };
    let (lo, hi) = points
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), (_, v)| {
            (lo.min(*v), hi.max(*v))
        });
    let span_t = last.0.saturating_sub(first.0).max(1) as f64;
    let span_v = hi - lo;
    let inset = 1.5;
    let height = (size.height - 2.0 * inset).max(0.0) as f64;
    points
        .iter()
        .map(|(t, v)| {
            let x = (t.saturating_sub(first.0) as f64 / span_t) * size.width as f64;
            let y = if span_v > 0.0 {
                inset as f64 + (1.0 - (v - lo) / span_v) * height
            } else {
                size.height as f64 / 2.0
            };
            Point::new(x as f32, y as f32)
        })
        .collect()
}

impl<M> canvas::Program<M> for SignalSparkline {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let points = scale(&self.points, bounds.size());
        if points.len() >= 2 {
            let line = canvas::Path::new(|b| {
                b.move_to(points[0]);
                for point in &points[1..] {
                    b.line_to(*point);
                }
            });
            frame.stroke(
                &line,
                canvas::Stroke::default()
                    .with_color(self.color)
                    .with_width(1.25),
            );
        }
        vec![frame.into_geometry()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparkline_scales_negative_values() {
        let size = iced::Size::new(100.0, 20.0);
        let points = scale(&[(0, -10.0), (50, 0.0), (100, -5.0)], size);
        assert_eq!(points[0], Point::new(0.0, 18.5));
        assert_eq!(points[1], Point::new(50.0, 1.5));
        assert_eq!(points[2].x, 100.0);
        assert!((points[2].y - 10.0).abs() < 1e-4);
        let flat = scale(&[(0, 3.0), (10, 3.0)], size);
        assert!(flat.iter().all(|p| p.y == 10.0));
        assert!(scale(&[], size).is_empty());
    }

    #[test]
    fn source_labels_tell_registers_on_one_plc_apart() {
        for (uri, label) in [
            (
                "modbus-tcp://192.168.1.30/1/holding/0?type=f32&name=temp",
                "192.168.1.30 · 1/holding/0 (temp)",
            ),
            (
                "modbus-tcp://192.168.1.30/1/holding/10?name=pressure",
                "192.168.1.30 · 1/holding/10 (pressure)",
            ),
            ("sim-signal://line1", "line1"),
            (
                "mqtt://<redacted>@broker:1883/plant/#",
                "broker:1883 · plant/#",
            ),
            (
                "opc.tcp://plc:4840/?node=ns=2;s=Temp",
                "plc:4840 (ns=2;s=Temp)",
            ),
        ] {
            assert_eq!(source_label(uri), label, "{uri}");
        }
    }

    #[test]
    fn remembers_only_plain_uris() {
        let mut ui = SignalsUi::default();
        ui.remember("mqtt://user:pw@broker/t");
        assert!(ui.recent.is_empty());
        for i in 0..10 {
            ui.remember(&format!("sim-signal://l{i}"));
        }
        ui.remember("sim-signal://l5");
        assert_eq!(ui.recent.len(), RECENT);
        assert_eq!(ui.recent[0], "sim-signal://l5");
        assert!(invalid("ftp://x").is_some());
        assert!(invalid(" ").is_none());
    }
}
