//! Live signals: the session's data sources with their latest values and
//! sparklines (Slice C). The GUI owns the session, so it reads the hub
//! directly each tick instead of the snapshot's brief copy.
use super::*;
use crate::signals::{SignalSinkStatus, SourceState, SourceStatus, TagStatus};
use iced::widget::{canvas, column};
use iced::{Point, Rectangle, Renderer, Theme, mouse};

/// Recent data source URIs kept in preferences.
const RECENT: usize = 8;
/// Tag rows shown per source before "+N more".
const ROWS: usize = 24;
pub(super) const ENDPOINT: &str = "manual-data-source-endpoint";

/// What the signals section remembers between frames.
#[derive(Default)]
pub(super) struct SignalsUi {
    /// The URI typed into "Add data source".
    pub(super) input: String,
    /// Sources started here, without credentials, newest first.
    pub(super) recent: Vec<String>,
    /// The hub's sources as of the last tick, with recent points.
    pub(super) live: Vec<SourceStatus>,
    /// Persistence readiness and failures as of the last tick.
    pub(super) sink: SignalSinkStatus,
    /// Why the last start or stop failed.
    pub(super) error: Option<String>,
    pub(super) source_setup: super::source_setup::State,
    /// The URI of the start in flight.
    starting: Option<String>,
    starting_protected: bool,
    /// Screenshot scenes show these instead of the hub.
    pub(super) fixture: Option<Vec<SourceStatus>>,
    pub(super) fixture_sink: Option<SignalSinkStatus>,
}

#[derive(Clone)]
pub(super) enum Msg {
    Open,
    Input(String),
    Add,
    Start(String),
    Stop { id: String, capture_id: String },
    Profile(super::source_setup::Input),
}
impl fmt::Debug for Msg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open => f.write_str("OpenDataSources"),
            Self::Profile(input) => f.debug_tuple("SourceProfile").field(input).finish(),
            _ => f.write_str("SignalsInput([redacted])"),
        }
    }
}
impl fmt::Debug for SignalsUi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignalsUi")
            .field("live", &self.live.len())
            .field("recent", &self.recent.len())
            .field("starting", &self.starting.is_some())
            .finish_non_exhaustive()
    }
}

/// Only URIs without credentials or secrets are remembered.
fn storable(uri: &str) -> bool {
    crate::tools::context::DataSourceRef::approved(uri).is_ok()
}

/// Whether `input` is worth an error style: typed and not a source URI.
fn invalid(input: &str) -> Option<String> {
    let typed = input.trim();
    if typed.is_empty() {
        return None;
    }
    crate::tools::context::DataSourceRef::approved(typed)
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
    let health = if ui.sink.error.is_some()
        || ui
            .live
            .iter()
            .any(|source| source.persistence_error.is_some())
    {
        " · Storage error"
    } else if ui.sink.dropped_total > 0
        || ui
            .live
            .iter()
            .any(|source| source.dropped > 0 || source.helper_dropped > 0)
    {
        " · Samples dropped"
    } else {
        ""
    };
    let tag_label = if tags == 1 { "tag" } else { "tags" };
    format!("{sources} · {tags} {tag_label}{health}")
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
    /// Expose manual controls without binding credentials or starting a source.
    pub(super) fn open_data_sources(&mut self) -> Task<Message> {
        self.image_mode = false;
        self.tab = Tab::Capture;
        self.inspector_open = true;
        self.inspector_summoned = true;
        self.inspector_peek = !self.inspector_docked();
        self.inspector_slide.go(1.0, self.now);
        self.restore_open_sections(&["signals".into()]);
        self.sync_inspector();
        operation::snap_to_end(inspector::SCROLL)
            .chain(accessibility::focus_in_window(ENDPOINT, self.main_window))
    }
    pub(super) fn signals_update(&mut self, message: Msg) -> Task<Message> {
        match message {
            Msg::Open => return self.open_data_sources(),
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
                if let Some(error) = invalid(&uri) {
                    self.signals.error = Some(error);
                    return Task::none();
                }
                self.signals.error = None;
                self.signals.starting = Some(uri.clone());
                self.signals.starting_protected = false;
                self.send(
                    Job::SignalStart,
                    SessionCommand::SignalStart {
                        uri,
                        interval_ms: crate::signals::default_interval_ms(),
                        tags: Vec::new(),
                        auth: None,
                    },
                );
            }
            Msg::Stop { id, capture_id } => {
                self.signals.error = None;
                self.send(
                    Job::SignalStop,
                    SessionCommand::SignalStopIf { id, capture_id },
                );
            }
            Msg::Profile(input) => {
                self.signals
                    .source_setup
                    .configure(Arc::new(super::source_setup::Native(self.handle.clone())));
                self.signals
                    .source_setup
                    .set_synthetic(self.signals.fixture.is_some());
                let enabled = !self.pending(Job::SignalStart);
                let effects = self.signals.source_setup.input(input, enabled);
                self.signal_profile_effects(effects);
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
                    if !self.signals.starting_protected {
                        self.signals.remember(&uri);
                    } else {
                        self.signals.recent.retain(|recent| recent != &uri);
                    }
                    if self.signals.input.trim() == uri {
                        self.signals.input.clear();
                    }
                }
                self.signals.starting_protected = false;
                self.tick_signals();
            }
            (Job::SignalStop, Ok(_)) => self.tick_signals(),
            (Job::SignalStart | Job::SignalStop, Err(error)) => {
                self.signals.starting = None;
                self.signals.starting_protected = false;
                self.signals.error = Some(format!("{error:#}"));
            }
            _ => {}
        }
    }

    /// Read the hub; recent points only while the section is open.
    pub(super) fn tick_signals(&mut self) {
        let effects = self.signals.source_setup.tick();
        self.signal_profile_effects(effects);
        if let Some(fixture) = &self.signals.fixture {
            self.signals.live = fixture.clone();
            self.signals.sink =
                self.signals
                    .fixture_sink
                    .clone()
                    .unwrap_or_else(|| SignalSinkStatus {
                        attached: true,
                        ..Default::default()
                    });
            return;
        }
        let hub = self.handle.signals();
        self.signals.live = hub.status(self.signals_history_visible());
        self.signals.sink = hub.sink_status();
    }
    fn signal_profile_effects(&mut self, effects: Vec<super::source_setup::Effect>) {
        for effect in effects {
            if let super::source_setup::Effect::Bound(binding) = effect {
                self.signals.error = None;
                self.signals.starting = Some(binding.uri.clone());
                self.signals.starting_protected = true;
                self.send(
                    Job::SignalStart,
                    SessionCommand::SignalStart {
                        uri: binding.uri,
                        interval_ms: crate::signals::default_interval_ms(),
                        tags: Vec::new(),
                        auth: binding.auth,
                    },
                );
            }
        }
    }

    /// Manual source setup stays reachable before the first source is started.
    pub(super) fn sidebar_signals(&self, p: &'static Palette) -> Element<'_, Message> {
        let mut list = column![
            container(
                row![
                    button(
                        row![
                            text("Data sources")
                                .size(style::CAPTION)
                                .font(style::SEMIBOLD)
                                .color(p.secondary),
                            text(self.signals.live.len().to_string())
                                .size(style::CAPTION)
                                .color(p.secondary)
                        ]
                        .spacing(6)
                    )
                    .id("sidebar-data-sources")
                    .name("Open data sources")
                    .description("Open manual source controls in the Capture inspector.")
                    .style(style::plain)
                    .padding([4, 0])
                    .width(Fill)
                    .on_press(Message::Signals(Msg::Open)),
                    button(icon(Icon::Plus, 12.0, p.secondary))
                        .id("sidebar-add-data-source")
                        .name("Add data source")
                        .description(
                            "Open local source setup. No connection starts until you choose one."
                        )
                        .style(style::plain)
                        .padding(5)
                        .on_press(Message::Signals(Msg::Open)),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
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
            let storage_error = source.persistence_error.as_deref().or(self
                .signals
                .sink
                .error
                .as_deref());
            let detail = storage_error
                .map(|_| "Storage error".to_string())
                .or_else(|| {
                    source
                        .error
                        .as_deref()
                        .filter(|_| source.state != SourceState::Running)
                        .map(|e| format::first_line(e).to_string())
                })
                .or_else(|| {
                    (source.dropped > 0 || source.helper_dropped > 0).then(|| {
                        format!(
                            "{} samples dropped",
                            format::grouped(source.dropped.saturating_add(source.helper_dropped))
                        )
                    })
                })
                .unwrap_or_else(|| state_label(source));
            let ink = if storage_error.is_some() {
                p.ink(p.danger)
            } else {
                error_ink(source.state, p)
            };
            let full = match storage_error.or(source.error.as_deref()) {
                Some(error) if storage_error.is_some() => {
                    format!(
                        "{}\n{}",
                        source.uri,
                        crate::tools::redact::for_output_text(error)
                    )
                }
                Some(error) if source.state != SourceState::Running => {
                    format!("{}\n{error}", source.uri)
                }
                _ => source.uri.clone(),
            };
            let line = row![
                container(widgets::dot(
                    if storage_error.is_some() {
                        p.danger
                    } else {
                        state_color(source.state, p)
                    },
                    8.0
                ))
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
                        .on_press_maybe((!busy && !source.capture_id.is_empty()).then(|| {
                            Message::Signals(Msg::Stop {
                                id: source.id.clone(),
                                capture_id: source.capture_id.clone(),
                            })
                        })),
                    "Stop logging",
                ),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            list = list.push(container(line).padding([7.0, sidebar::INSET]));
        }
        list.into()
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
                auth: None,
                capture_id: "0123456789abcdef0123456789abcdef".into(),
                dataset_slot_id: None,
                protocol,
                uri: uri.into(),
                interval_ms: 1000,
                state,
                started_ms: now - 61_000,
                capture_limits: None,
                deadline_ms: None,
                completed_ms: None,
                completion: None,
                payload_bytes: 0,
                written_bytes: 0,
                persistence_complete: false,
                persistence_error: None,
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

pub(super) fn persistence_failure_fixture() -> (Vec<SourceStatus>, SignalSinkStatus) {
    let mut source = fixture().remove(0);
    source.tags.truncate(1);
    source.tags_total = 1;
    source.written = 0;
    source.written_bytes = 0;
    source.dropped = 37;
    source.helper_dropped = 5;
    source.restarts = 2;
    source.persistence_error = Some("Dataset write failed: storage quota reached.".into());
    (
        vec![source],
        SignalSinkStatus {
            attached: true,
            error: Some("Dataset write failed: storage quota reached.".into()),
            dropped_total: 37,
            ..Default::default()
        },
    )
}

/// The inspector's section, or `None` to leave it out.
pub(super) fn view<'a>(
    bench: &'a Workbench,
    open: bool,
    p: &'static Palette,
) -> Option<Element<'a, Message>> {
    let ui = &bench.signals;
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
            .id(ENDPOINT)
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
    section = section.push(accessibility::mapped(
        super::source_setup::view(
            &ui.source_setup,
            p,
            !starting,
            "manual-source-profile",
            super::source_setup::Purpose::Log,
        ),
        |input| Message::Signals(Msg::Profile(input)),
    ));
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
    if let Some(error) = &ui.sink.error {
        section = section.push(
            text(format!(
                "Storage error: {}",
                crate::tools::redact::for_output_text(error)
            ))
            .size(style::CAPTION)
            .color(p.ink(p.danger)),
        );
    }
    if ui.sink.dropped_total > 0 {
        section = section.push(
            text(format!(
                "Queue / storage drops: {} total",
                format::grouped(ui.sink.dropped_total)
            ))
            .size(style::CAPTION)
            .color(p.ink(p.warn)),
        );
    }
    if !ui.live.is_empty() && !ui.sink.attached {
        section = section.push(
            text("Values are shown, not saved: open a dataset to keep them.")
                .size(style::CAPTION)
                .color(p.secondary),
        );
    }
    for source in &ui.live {
        section = section.push(source_view(source, ui.sink.error.is_some(), p));
    }
    Some(section.into())
}

fn source_view<'a>(
    source: &'a SourceStatus,
    sink_failed: bool,
    p: &'static Palette,
) -> Element<'a, Message> {
    let failed_writes = sink_failed || source.persistence_error.is_some();
    let mut rows = column![
        row![
            widgets::dot(
                if failed_writes {
                    p.danger
                } else {
                    state_color(source.state, p)
                },
                8.0
            ),
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
    rows = rows.push(
        text(format!(
            "{} written · {} dropped",
            format::grouped(source.written),
            format::grouped(source.dropped)
        ))
        .size(style::CAPTION)
        .color(if failed_writes {
            p.ink(p.danger)
        } else {
            p.secondary
        }),
    );
    rows = rows.push(
        text(format!(
            "{} helper drops · {} restarts",
            format::grouped(source.helper_dropped),
            format::grouped(source.restarts)
        ))
        .size(style::CAPTION)
        .color(if source.helper_dropped > 0 {
            p.ink(p.warn)
        } else {
            p.secondary
        }),
    );
    let last = source
        .last_sample_ms
        .and_then(|ms| chrono::DateTime::from_timestamp_millis(ms.min(i64::MAX as u64) as i64))
        .map(|time| format!("Last sample {}", time.format("%H:%M:%S UTC")))
        .unwrap_or_else(|| "No samples received yet".into());
    rows = rows.push(text(last).size(style::CAPTION).color(p.secondary));
    if let Some(error) = &source.persistence_error {
        rows = rows.push(
            text(format!(
                "Save error: {}",
                crate::tools::redact::for_output_text(error)
            ))
            .size(style::CAPTION)
            .color(p.ink(p.danger)),
        );
    }
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
    fn hidden_signal_history_stops_immediately_after_ui_events_without_rerendering() {
        for width in [1280.0, 900.0] {
            let mut app = Workbench::new(
                SessionHandle::new(),
                "source-history-test".into(),
                true,
                None,
            );
            app.width = width;
            assert!(!app.signals_history_visible());
            let _ = app.handle_message(Message::Signals(Msg::Open));
            assert!(app.signals_history_visible());
            // Reproduce the old stale flag: render the expanded Capture pane,
            // then change state without ever rendering Live signals again.
            drop(app.view());
            for tab in [Tab::Features, Tab::Forward] {
                let _ = app.handle_message(Message::Tab(tab));
                assert!(!app.signals_history_visible(), "{width}/{tab:?}");
            }
            let _ = app.handle_message(Message::Tab(Tab::Capture));
            assert!(app.signals_history_visible());
            let _ = app.handle_message(Message::ToggleInspector);
            assert!(
                !app.signals_history_visible(),
                "hidden inspector at {width}"
            );
            let _ = app.handle_message(Message::ToggleInspector);
            assert!(app.signals_history_visible());
            let _ = app.handle_message(Message::ToggleImage);
            assert!(!app.signals_history_visible(), "image mode at {width}");
            let _ = app.handle_message(Message::ToggleImage);
            assert!(app.signals_history_visible());
            for open in [false, true] {
                let _ =
                    app.handle_message(Message::InspectorDisclosure(inspector::Section::Signals));
                assert_eq!(app.signals_history_visible(), open, "disclosure at {width}");
            }
            assert!(app.pending.is_empty());
            assert!(app.handle.snapshot().signals.is_empty());
        }
    }

    fn stop_messages(app: &Workbench, renderer: &iced::Renderer) -> Vec<Message> {
        use iced::advanced::{
            Layout, layout,
            widget::{Id, Operation, Tree},
        };
        #[derive(Default)]
        struct Stops(Vec<Message>);
        impl Operation for Stops {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
                if let Some(control) = state.downcast_ref::<accessibility::Control<Message>>()
                    && control.node.role() == accessibility::Role::Button
                    && control
                        .node
                        .label()
                        .is_some_and(|label| label.starts_with("Stop logging "))
                    && let Some(message) = &control.activate
                {
                    self.0.push(message.clone());
                }
            }
        }
        let mut view = app.sidebar_signals(Palette::of(false));
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            renderer,
            &layout::Limits::new(Size::ZERO, Size::new(SIDEBAR, 620.0)),
        );
        let mut stops = Stops::default();
        view.as_widget_mut()
            .operate(&mut tree, Layout::new(&node), renderer, &mut stops);
        stops.0
    }

    #[test]
    fn stop_controls_pin_the_rendered_capture_and_dispatch_a_conditional_stop() {
        let renderer = destinations::layout_tests::renderer();
        let mut app = Workbench::new(SessionHandle::new(), "source-stop-test".into(), true, None);
        let mut source = fixture().remove(0);
        let original = source.capture_id.clone();
        app.signals.live = vec![source.clone()];
        let stale_click = stop_messages(&app, &renderer).remove(0);
        source.capture_id = "fedcba9876543210fedcba9876543210".into();
        app.signals.live = vec![source.clone()];
        assert!(matches!(&stale_click,
            Message::Signals(Msg::Stop { id, capture_id })
            if *id == source.id && *capture_id == original));
        // Replacement cannot retarget an event already queued by the widget.
        let _ = app.handle_message(stale_click);
        let pending = app
            .pending
            .pop()
            .expect("the exact rendered stop was submitted");
        assert!(matches!(pending.job, Job::SignalStop));
        let error = pending
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("not found"));
        assert!(
            app.handle
                .activity()
                .activity
                .iter()
                .any(|entry| entry.operation == "signal_stop_if"
                    && entry.origin == ActivityOrigin::Gui)
        );
        // A helper restart changes telemetry, not the capture identity used by Stop.
        source.restarts += 1;
        source.helper_pid = Some(12345);
        app.signals.live = vec![source.clone()];
        assert!(matches!(&stop_messages(&app, &renderer)[0],
            Message::Signals(Msg::Stop { id, capture_id })
            if *id == source.id && *capture_id == source.capture_id));
        // An old status without an instance token cannot offer an unsafe legacy Stop.
        app.signals.live[0].capture_id.clear();
        assert!(stop_messages(&app, &renderer).is_empty());
        app.handle.shutdown();
    }

    #[test]
    fn attached_sink_failure_is_rendered_with_running_connection_and_zero_written() {
        use destinations::layout_tests::{
            apply, prepare_artifact_frame, render_artifact, renderer,
        };
        use iced::advanced::{Layout, layout, widget::Tree};
        let mut renderer = renderer();
        for dark in [false, true] {
            // Legacy unbounded captures report write failures on the sink;
            // bounded captures additionally retain their own persistence_error.
            for bounded_error in [false, true] {
                let mut app = Workbench::new(
                    SessionHandle::new(),
                    "source-storage-test".into(),
                    true,
                    None,
                );
                app.apply_scene(&["signals-persistence-error"]);
                if !bounded_error {
                    app.signals.live[0].persistence_error = None;
                }
                assert!(app.signals.sink.attached);
                assert_eq!(app.signals.live[0].state, SourceState::Running);
                assert!(app.snapshot.cameras.is_empty());
                assert!(app.pending.is_empty());
                assert!(app.handle.snapshot().signals.is_empty());
                assert!(summary(&app.signals).contains("Storage error"));
                let size = Size::new(INSPECTOR, 500.0);
                let mut view = app.inspector(Palette::of(dark));
                let mut tree = Tree::new(view.as_widget());
                let node = view.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, size),
                );
                prepare_artifact_frame(&mut view, &mut tree, &node, &renderer, size);
                apply(
                    &mut view,
                    &mut tree,
                    &node,
                    &renderer,
                    iced::advanced::widget::operation::scrollable::snap_to::<Message>(
                        iced::advanced::widget::Id::new(inspector::SCROLL),
                        iced::widget::scrollable::RelativeOffset::END.into(),
                    ),
                );
                let (snapshot, _, _) = accessibility::snapshot_widget(
                    view.as_widget_mut(),
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                );
                let labels = snapshot
                    .nodes
                    .iter()
                    .filter_map(|(_, node)| node.label())
                    .collect::<Vec<_>>();
                for expected in [
                    "Storage error: Dataset write failed: storage quota reached.",
                    "Queue / storage drops: 37 total",
                    "0 written · 37 dropped",
                    "5 helper drops · 2 restarts",
                ] {
                    assert!(labels.contains(&expected), "Missing {expected}: {labels:?}");
                    let bounds = snapshot
                        .nodes
                        .iter()
                        .find(|(_, node)| node.label() == Some(expected))
                        .unwrap()
                        .1
                        .bounds()
                        .unwrap();
                    assert!(
                        bounds.width() > 0.0
                            && bounds.height() > 0.0
                            && bounds.y0 >= 0.0
                            && bounds.y1 <= f64::from(size.height) + 0.5,
                        "Failure accounting is visible in the actual inspector: {expected}, {bounds:?}"
                    );
                }
                assert!(
                    labels
                        .iter()
                        .any(|label| label.starts_with("Last sample ") && label.ends_with(" UTC"))
                );
                assert_eq!(
                    labels.iter().any(|label| label.starts_with("Save error: ")),
                    bounded_error
                );
                if bounded_error && !dark {
                    render_artifact(
                        &format!("signals-persistence-error-{dark}"),
                        &view,
                        &tree,
                        &node,
                        &mut renderer,
                        size,
                    );
                }
                // A view consumes cached state only; the fixture never touches the session.
                assert!(app.handle.snapshot().signals.is_empty());
                assert!(app.handle.snapshot().signal_bindings.is_empty());
            }
        }
    }

    #[test]
    fn zero_source_sidebar_opens_and_focuses_manual_controls_without_a_camera_or_connection() {
        use iced::advanced::{
            Layout, layout,
            widget::{Id, Operation, Tree, operation::Outcome},
        };
        use iced::futures::StreamExt;
        #[derive(Default)]
        struct Entries(Vec<Message>);
        impl Operation for Entries {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
                if let Some(control) = state.downcast_ref::<accessibility::Control<Message>>()
                    && control.node.role() == accessibility::Role::Button
                    && matches!(
                        control.node.label(),
                        Some("Open data sources" | "Add data source")
                    )
                    && let Some(message) = &control.activate
                {
                    self.0.push(message.clone());
                }
            }
        }
        let renderer = destinations::layout_tests::renderer();
        let mut app = Workbench::new(
            SessionHandle::new(),
            "empty-sources-entry".into(),
            true,
            None,
        );
        assert!(app.snapshot.connected.is_none() && app.snapshot.cameras.is_empty());
        assert!(app.signals.live.is_empty());
        app.image_mode = true;
        app.inspector_open = false;
        app.inspector_peek = false;
        app.tab = Tab::Features;
        let mut sidebar = app.sidebar(Palette::of(false));
        let mut tree = Tree::new(sidebar.as_widget());
        let node = sidebar.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(SIDEBAR, 620.0)),
        );
        let (snapshot, _, _) = accessibility::snapshot_widget(
            sidebar.as_widget_mut(),
            &mut tree,
            Layout::new(&node),
            &renderer,
        );
        for label in ["Open data sources", "Add data source"] {
            assert!(
                snapshot
                    .nodes
                    .iter()
                    .any(|(_, node)| node.role() == accessibility::Role::Button
                        && node.label() == Some(label)
                        && !node.is_disabled())
            );
        }
        let mut entries = Entries::default();
        sidebar
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut entries);
        assert_eq!(entries.0.len(), 2);
        assert!(
            entries
                .0
                .iter()
                .all(|message| matches!(message, Message::Signals(Msg::Open)))
        );
        drop(sidebar);
        let task = app.handle_message(entries.0.remove(0));
        assert_eq!(app.tab, Tab::Capture);
        assert!(app.inspector_shown());
        assert!(!app.image_mode);
        assert!(app.pending.is_empty());
        assert!(app.handle.snapshot().signals.is_empty());
        assert!(app.handle.snapshot().signal_bindings.is_empty());
        assert!(
            !app.signals.source_setup.pending,
            "opening source controls never resolves profile credentials"
        );
        let mut inspector = app.inspector(Palette::of(false));
        let mut tree = Tree::new(inspector.as_widget());
        let size = Size::new(INSPECTOR, 620.0);
        let node = inspector.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, size),
        );
        let mut actions = iced_runtime::task::into_stream(task).unwrap();
        let mut focus_operations = 0;
        while let Some(action) = iced::futures::executor::block_on(actions.next()) {
            match action {
                iced_runtime::Action::Widget(mut operation) => {
                    focus_operations += 1;
                    loop {
                        inspector.as_widget_mut().operate(
                            &mut tree,
                            Layout::new(&node),
                            &renderer,
                            operation.as_mut(),
                        );
                        match operation.finish() {
                            Outcome::Chain(next) => operation = next,
                            _ => break,
                        }
                    }
                }
                iced_runtime::Action::Output(Message::AccessibilityChanged(_)) => {}
                _ => panic!("opening Sources must only reveal and focus local controls"),
            }
        }
        assert_eq!(focus_operations, 2);
        let (snapshot, _, _) = accessibility::snapshot_widget(
            inspector.as_widget_mut(),
            &mut tree,
            Layout::new(&node),
            &renderer,
        );
        let (id, endpoint) = snapshot
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == accessibility::Role::TextInput
                    && node.label() == Some("Add data source")
            })
            .expect("the Capture inspector exposes source entry with no camera connected");
        assert!(!endpoint.is_disabled());
        assert_eq!(snapshot.focus, *id);
        let bounds = endpoint.bounds().unwrap();
        assert!(
            bounds.width() > 0.0
                && bounds.height() > 0.0
                && bounds.x0 >= 0.0
                && bounds.y0 >= 0.0
                && bounds.x1 <= f64::from(size.width) + 0.5
                && bounds.y1 <= f64::from(size.height) + 0.5
        );
        assert!(
            snapshot
                .nodes
                .iter()
                .any(|(_, node)| node.label() == Some("Protected source profiles"))
        );
        assert!(
            app.signals.input.is_empty(),
            "entry does not adopt a cached URI or start a source"
        );
    }

    #[test]
    fn opening_sources_scrolls_before_focus_in_the_actual_camera_free_window() {
        use destinations::layout_tests::{
            prepare_artifact_frame, render_artifact_scaled, renderer,
        };
        use iced::advanced::{
            Layout, layout,
            widget::{Tree, operation::Outcome},
        };
        use iced::futures::StreamExt;
        let mut renderer = renderer();
        for (physical, scale) in [
            (Size::new(1100.0, 700.0), UiScale::Standard),
            (Size::new(900.0, 620.0), UiScale::Larger),
        ] {
            for populated in [false, true] {
                let mut app = Workbench::new(
                    SessionHandle::new(),
                    "open-sources-window-test".into(),
                    true,
                    None,
                );
                let size = Size::new(
                    physical.width / scale.factor(),
                    physical.height / scale.factor(),
                );
                app.width = size.width;
                app.height = size.height;
                app.ui_scale = scale;
                app.image_mode = true;
                app.inspector_open = false;
                app.inspector_peek = false;
                app.tab = Tab::Features;
                if populated {
                    let base = fixture().remove(0);
                    app.signals.live = (0..8)
                        .map(|index| {
                            let mut source = base.clone();
                            source.id =
                                crate::signals::source_id(&format!("sim-signal://line{index}"));
                            source.uri = format!("sim-signal://line{index}");
                            source
                        })
                        .collect();
                }
                let task = app.handle_message(Message::Signals(Msg::Open));
                app.now += Duration::from_millis(500);
                assert!(app.snapshot.cameras.is_empty() && app.snapshot.connected.is_none());
                assert!(app.inspector_shown());
                let mut view = app.view();
                let mut tree = Tree::new(view.as_widget());
                let node = view.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, size),
                );
                prepare_artifact_frame(&mut view, &mut tree, &node, &renderer, size);
                let mut actions = iced_runtime::task::into_stream(task).unwrap();
                let mut operations = 0;
                let mut endpoint_before_focus = None;
                while let Some(action) = iced::futures::executor::block_on(actions.next()) {
                    match action {
                        iced_runtime::Action::Widget(mut operation) => {
                            operations += 1;
                            loop {
                                view.as_widget_mut().operate(
                                    &mut tree,
                                    Layout::new(&node),
                                    &renderer,
                                    operation.as_mut(),
                                );
                                match operation.finish() {
                                    Outcome::Chain(next) => operation = next,
                                    _ => break,
                                }
                            }
                            if operations == 1 {
                                let (snapshot, _, _) = accessibility::snapshot_widget(
                                    view.as_widget_mut(),
                                    &mut tree,
                                    Layout::new(&node),
                                    &renderer,
                                );
                                endpoint_before_focus = snapshot
                                    .nodes
                                    .iter()
                                    .find(|(_, node)| {
                                        node.role() == accessibility::Role::TextInput
                                            && node.label() == Some("Add data source")
                                    })
                                    .map(|(_, node)| node.bounds().unwrap());
                            }
                        }
                        iced_runtime::Action::Output(Message::AccessibilityChanged(_)) => {}
                        _ => panic!("Opening Sources only scrolls and focuses local controls"),
                    }
                }
                assert_eq!(operations, 2, "scroll must finish before focus begins");
                let (snapshot, _, _) = accessibility::snapshot_widget(
                    view.as_widget_mut(),
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                );
                let (id, endpoint) = snapshot
                    .nodes
                    .iter()
                    .find(|(_, node)| {
                        node.role() == accessibility::Role::TextInput
                            && node.label() == Some("Add data source")
                    })
                    .unwrap();
                assert_eq!(snapshot.focus, *id);
                let endpoint = endpoint.bounds().unwrap();
                let profile = snapshot
                    .nodes
                    .iter()
                    .find(|(_, node)| node.label() == Some("Protected source profiles"))
                    .unwrap()
                    .1
                    .bounds()
                    .unwrap();
                for (name, bounds) in [("endpoint", endpoint), ("profile entry", profile)] {
                    assert!(
                        bounds.width() > 0.0
                            && bounds.height() > 0.0
                            && bounds.y0 >= 0.0
                            && bounds.y1 <= f64::from(size.height - TOOLBAR) + 0.5,
                        "{name} is visible after Open: {physical:?}/{scale}/{populated}, {bounds:?}"
                    );
                }
                if populated {
                    assert!(
                        endpoint_before_focus.unwrap().height() < 1.0,
                        "the end of many sources is reached before returning to setup"
                    );
                    assert!(
                        endpoint.y0 < f64::from(size.height / 2.0),
                        "focus returns setup near the inspector top even with many sources"
                    );
                }
                if !populated {
                    render_artifact_scaled(
                        &format!("signals-empty-open-window-{scale}"),
                        &view,
                        &tree,
                        &node,
                        &mut renderer,
                        size,
                        scale.factor(),
                    );
                }
                assert!(app.pending.is_empty());
                assert!(app.handle.snapshot().signals.is_empty());
                assert!(app.handle.snapshot().signal_bindings.is_empty());
            }
        }
    }

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
        for uri in [
            "mqtt://reader@broker/topic",
            "mqtt://broker/topic?password=private-example",
            "mqtt://reader:private-example@broker/topic",
        ] {
            ui.remember(uri);
            assert!(!ui.recent.iter().any(|recent| recent == uri));
            assert!(!invalid(uri).unwrap().contains("private-example"));
            assert!(!format!("{:?}", Msg::Start(uri.into())).contains(uri));
        }
        ui.input = "mqtt://private-user:private-example@broker/topic".into();
        assert!(!format!("{ui:?}").contains("private"));
    }
}
