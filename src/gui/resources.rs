//! Live resource headroom and the small set of limits the camera runtime can
//! enforce. Sampling and policy writes stay off the UI thread.
use super::*;
use crate::resources::{Policy, Pressure};
use crate::session::CameraSnapshot;
use iced::widget::column;
use std::collections::VecDeque;

const REFRESH: Duration = Duration::from_secs(1);
const STALE: Duration = Duration::from_secs(5);
const RECENT: Duration = Duration::from_secs(10);
const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Resource {
    Cpu,
    Memory,
    Disk,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Change {
    Cpu(Option<u32>),
    Memory(Option<u64>),
    Automatic,
}

#[derive(Clone, Debug)]
pub(super) enum Input {
    Open(bool),
    Scrolled(bool),
    Expand(Resource),
    Change(Change),
    Configured(Result<Policy, String>),
}

#[derive(Clone, Debug)]
struct Reading {
    policy: Result<Policy, String>,
    host: crate::resources::HostSample,
    buffers: Result<crate::resources::buffers::BufferUsage, String>,
    threads: usize,
    buffer_limit: u64,
}

impl Reading {
    #[cfg(not(test))]
    fn sample() -> Self {
        let host = crate::resources::host::sample();
        Self {
            policy: crate::resources::load().map_err(|error| format!("{error:#}")),
            threads: crate::resources::processing_threads(host.cpu_cores),
            buffer_limit: crate::resources::buffer_limit(),
            host,
            buffers: crate::resources::buffers::usage().map_err(|error| format!("{error:#}")),
        }
    }
}

#[derive(Clone, Debug)]
struct EncoderCounters {
    id: String,
    dropped: u64,
    late: u64,
}
#[derive(Clone, Debug)]
struct Counters {
    worker: u32,
    lost: u64,
    skipped: u64,
    encoder: Option<EncoderCounters>,
}

/// Ten-second deltas are independent from lifetime counters. Reconnects and
/// counter resets establish a new baseline; they are never counted as loss.
#[derive(Default)]
struct Delivery {
    previous: HashMap<String, Counters>,
    events: VecDeque<(Instant, u64, u64, u64, u64)>,
    checked: Option<Instant>,
    ready: bool,
    streaming: bool,
    encoder_stalled: bool,
}

impl Delivery {
    fn observe(&mut self, cameras: &[CameraSnapshot], now: Instant) {
        self.events
            .retain(|(at, ..)| now.saturating_duration_since(*at) <= RECENT);
        self.streaming = cameras.iter().any(|camera| camera.streaming);
        self.encoder_stalled = cameras.iter().any(|camera| {
            !camera.stale
                && camera.forwarding.is_some()
                && camera
                    .forwarding_progress
                    .as_ref()
                    .is_some_and(|progress| progress.stalled)
        });
        if self
            .checked
            .is_some_and(|at| now.saturating_duration_since(at) < REFRESH)
        {
            return;
        }
        self.checked = Some(now);
        self.previous
            .retain(|id, _| cameras.iter().any(|camera| camera.info.id == *id));
        let (mut lost, mut skipped, mut encoder_dropped, mut encoder_late) =
            (0u64, 0u64, 0u64, 0u64);
        for camera in cameras.iter().filter(|camera| !camera.stale) {
            let current = Counters {
                worker: camera.worker_pid,
                lost: camera.lost,
                skipped: camera.preview_skipped,
                encoder: camera
                    .forwarding_progress
                    .as_ref()
                    .map(|progress| EncoderCounters {
                        id: progress.id.clone(),
                        dropped: progress.dropped,
                        late: progress.late,
                    }),
            };
            if let Some(previous) = self
                .previous
                .insert(camera.info.id.clone(), current.clone())
                && previous.worker == current.worker
            {
                self.ready = true;
                lost = lost.saturating_add(current.lost.saturating_sub(previous.lost));
                skipped = skipped.saturating_add(current.skipped.saturating_sub(previous.skipped));
                if let Some(encoder) = &current.encoder {
                    let old = previous.encoder.as_ref().filter(|old| old.id == encoder.id);
                    encoder_dropped = encoder_dropped.saturating_add(
                        encoder
                            .dropped
                            .saturating_sub(old.map_or(0, |old| old.dropped)),
                    );
                    encoder_late = encoder_late
                        .saturating_add(encoder.late.saturating_sub(old.map_or(0, |old| old.late)));
                }
            }
        }
        if lost > 0 || skipped > 0 || encoder_dropped > 0 || encoder_late > 0 {
            self.events
                .push_back((now, lost, skipped, encoder_dropped, encoder_late));
        }
    }

    fn recent(&self) -> (u64, u64, u64, u64) {
        self.events.iter().fold(
            (0u64, 0u64, 0u64, 0u64),
            |(lost, skipped, dropped, late), (_, a, b, c, d)| {
                (
                    lost.saturating_add(*a),
                    skipped.saturating_add(*b),
                    dropped.saturating_add(*c),
                    late.saturating_add(*d),
                )
            },
        )
    }
}

#[derive(Default)]
pub(super) struct State {
    pub open: bool,
    scrolled: bool,
    expanded: Option<Resource>,
    pending: Option<Receiver<(u64, Reading)>>,
    reading: Option<Reading>,
    refreshed: Option<Instant>,
    requested: Option<Instant>,
    generation: u64,
    saving: bool,
    error: Option<String>,
    delivery: Delivery,
    pressure_noticed: bool,
    loss_noticed: bool,
    scene: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tone {
    Normal,
    Warning,
    Critical,
    Unknown,
}
struct ResourceMeter {
    summary: String,
    details: Vec<String>,
    fraction: Option<f32>,
    tone: Tone,
}

impl Tone {
    fn ink(self, p: &Palette) -> Color {
        match self {
            Self::Normal => p.secondary,
            Self::Warning => p.ink(p.warn),
            Self::Critical => p.ink(p.danger),
            Self::Unknown => p.secondary,
        }
    }
    fn bar(self, p: &Palette) -> Color {
        match self {
            Self::Normal => p.accent,
            Self::Warning => p.warn,
            Self::Critical => p.danger,
            Self::Unknown => p.secondary,
        }
    }
}

fn share(used: f64, cap: f64) -> Option<f32> {
    (used.is_finite() && used >= 0.0 && cap.is_finite() && cap > 0.0)
        .then(|| (used / cap).clamp(0.0, 1.0) as f32)
}
fn fraction_tone(value: Option<f32>) -> Tone {
    match value {
        Some(value) if value >= 0.95 => Tone::Critical,
        Some(value) if value >= 0.8 => Tone::Warning,
        Some(_) => Tone::Normal,
        None => Tone::Unknown,
    }
}
fn bytes(value: u64) -> String {
    if value >= GIB {
        format!("{:.1} GiB", value as f64 / GIB as f64)
    } else if value >= MIB {
        format!("{:.0} MiB", value as f64 / MIB as f64)
    } else {
        format!("{:.0} KiB", value as f64 / 1024.0)
    }
}
fn buffer_fraction(used: u64, limit: u64, available: Option<u64>) -> Option<f32> {
    let budget = share(used as f64, limit as f64)?;
    Some(match available {
        Some(0) => 1.0,
        Some(room) => {
            budget.max(share(used as f64, used.saturating_add(room) as f64).unwrap_or(0.0))
        }
        None => budget,
    })
}

fn remaining(used: u64, limit: u64) -> String {
    if used <= limit {
        format!("{} headroom", bytes(limit - used))
    } else {
        format!("{} over the limit", bytes(used - limit))
    }
}
fn mode(value: Option<u64>) -> &'static str {
    if value.is_none() {
        "Automatic"
    } else {
        "Your limit"
    }
}

impl State {
    fn live(&self, now: Instant) -> Option<&Reading> {
        self.reading.as_ref().filter(|_| {
            self.scene
                || self
                    .refreshed
                    .is_some_and(|at| now.saturating_duration_since(at) <= STALE)
        })
    }
    fn policy(&self) -> Option<&Policy> {
        self.reading.as_ref()?.policy.as_ref().ok()
    }
}

impl Workbench {
    pub(super) fn resources_input(&mut self, input: Input) -> Task<Message> {
        match input {
            Input::Open(open) => {
                // Settings can be a separate macOS window, not a modal.
                if open && !self.resources.open && self.settings_open {
                    return self
                        .handle_message(Message::Settings(false))
                        .chain(Task::done(Message::Resources(Input::Open(true))));
                }
                if open && !self.resources.open && self.sheet_open() {
                    return Task::none();
                }
                self.resources.open = open;
                if open {
                    self.resources.scrolled = false;
                    self.resources.requested = None;
                    self.tick_resources();
                    return Task::batch([
                        operation::focus("sheet-close"),
                        operation::snap_to(SHEET_BODY, operation::RelativeOffset::START),
                    ]);
                }
            }
            Input::Scrolled(scrolled) => self.resources.scrolled = scrolled,
            Input::Expand(resource) => {
                self.resources.expanded = if self.resources.expanded == Some(resource) {
                    None
                } else {
                    Some(resource)
                };
            }
            Input::Change(change) => {
                if self.resources.saving {
                    return Task::none();
                }
                self.resources.saving = true;
                self.resources.error = None;
                self.resources.generation = self.resources.generation.wrapping_add(1);
                return Task::perform(
                    async move {
                        let (send, receive) = iced::futures::channel::oneshot::channel();
                        let started = std::thread::Builder::new()
                            .name("capturefab-resource-settings".into())
                            .spawn(move || {
                                let result = match change {
                                    Change::Automatic => {
                                        crate::resources::configure(&Policy::default())
                                    }
                                    _ => crate::resources::update(|policy| {
                                        match change {
                                            Change::Cpu(value) => policy.cpu_threads = value,
                                            Change::Memory(value) => policy.buffer_bytes = value,
                                            Change::Automatic => unreachable!(),
                                        }
                                        Ok(())
                                    }),
                                }
                                .map_err(|error| format!("{error:#}"));
                                let _ = send.send(result);
                            });
                        match started {
                            Ok(_) => receive.await.unwrap_or_else(|_| {
                                Err("The resource settings worker stopped.".into())
                            }),
                            Err(error) => Err(format!("Could not save resource limits: {error}")),
                        }
                    },
                    |result| Message::Resources(Input::Configured(result)),
                );
            }
            Input::Configured(result) => {
                self.resources.saving = false;
                match result {
                    Ok(policy) => {
                        if let Some(reading) = &mut self.resources.reading {
                            reading.policy = Ok(policy);
                        }
                        self.resources.requested = None;
                        self.resources.refreshed = None;
                    }
                    Err(error) => self.resources.error = Some(error),
                }
                self.tick_resources();
            }
        }
        Task::none()
    }

    pub(super) fn tick_resources(&mut self) {
        if self.resources.scene {
            return;
        }
        self.resources
            .delivery
            .observe(&self.snapshot.cameras, self.now);
        if let Some(pending) = &self.resources.pending {
            match pending.try_recv() {
                Ok((generation, reading)) => {
                    self.resources.pending = None;
                    if generation == self.resources.generation {
                        let pressure = pressure_view(&reading.host);
                        performance::resource_pressure(if pressure.2 {
                            Some("device CPU or memory pressure")
                        } else if reading.host.pressure != Pressure::Normal {
                            Some("conservative resource safeguards")
                        } else {
                            None
                        });
                        if pressure.2 && !self.resources.pressure_noticed {
                            self.set_notice("Computer resources are busy", Some("Resource safeguards pace background work. Automatic preview mode also draws fewer frames. Open Resources for usage and headroom.".into()), Level::Warning);
                        }
                        if pressure.2 {
                            self.resources.pressure_noticed = true;
                        } else if reading.host.pressure == Pressure::Normal {
                            self.resources.pressure_noticed = false;
                        }
                        self.resources.reading = Some(reading);
                        self.resources.refreshed = Some(self.now);
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    self.resources.pending = None;
                    self.resources.error =
                        Some("Resource sampling stopped. Trying again shortly.".into());
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        let delivery = self.resources.delivery.recent();
        let lost = delivery.0 > 0 || delivery.2 > 0 || self.resources.delivery.encoder_stalled;
        if lost && !self.resources.loss_noticed {
            self.set_notice(
                if delivery.0 > 0 { "Camera frames were lost" } else if delivery.2 > 0 || delivery.3 > 0 { "Recording or stream frames fell behind" } else { "Recording or stream is waiting for frames" },
                Some("Open Resources for frame delivery and resource headroom. Camera loss, encoder drops and skipped previews are reported separately.".into()),
                Level::Warning,
            );
        }
        self.resources.loss_noticed = lost;
        // Tests inject readings and never sample the user's system or settings.
        #[cfg(not(test))]
        {
            if self.resources.pending.is_some()
                || self.resources.saving
                || self.screenshot.is_some()
                || self
                    .resources
                    .requested
                    .is_some_and(|at| self.now.saturating_duration_since(at) < REFRESH)
            {
                return;
            }
            let generation = self.resources.generation;
            let (sender, receiver) = mpsc::channel();
            self.resources.requested = Some(self.now);
            match std::thread::Builder::new()
                .name("capturefab-resources".into())
                .spawn(move || {
                    let _ = sender.send((generation, Reading::sample()));
                }) {
                Ok(_) => self.resources.pending = Some(receiver),
                Err(error) => {
                    self.resources.error = Some(format!("Could not check resources: {error}"))
                }
            }
        }
    }

    pub(super) fn resources_settings_link(&self, p: &'static Palette) -> Element<'_, Message> {
        column![
            button(
                row![
                    icon(Icon::Sliders, 16.0, p.accent_text),
                    text("Resource usage & limits").size(style::BODY)
                ]
                .spacing(8)
                .align_y(Alignment::Center)
            )
            .name("Open resource usage and limits")
            .style(style::secondary)
            .padding([7, 10])
            .on_press(Message::Resources(Input::Open(true))),
            text("Live CPU, camera-buffer and disk headroom.")
                .size(style::SMALL)
                .color(p.secondary),
        ]
        .spacing(7)
        .into()
    }

    fn resource_status(&self) -> (Tone, &'static str) {
        let delivery = self.resources.delivery.recent();
        if delivery.0 > 0 {
            return (Tone::Warning, "Camera frames lost");
        }
        if self.resources.delivery.encoder_stalled {
            return (Tone::Warning, "Encoder is waiting for frames");
        }
        if delivery.2 > 0 || delivery.3 > 0 {
            return (Tone::Warning, "Recording or stream frames dropped");
        }
        let Some(reading) = self.resources.live(self.now) else {
            return (Tone::Unknown, "Checking resource use");
        };
        let pressure = pressure_view(&reading.host);
        if matches!(pressure.0, Tone::Warning | Tone::Critical) {
            return (pressure.0, pressure.1);
        }
        if let Ok(buffers) = &reading.buffers {
            let room = crate::resources::buffers::headroom(
                buffers.used_bytes,
                reading.buffer_limit,
                reading.host,
            );
            let tone = fraction_tone(buffer_fraction(
                buffers.used_bytes,
                reading.buffer_limit,
                room.physical_bytes.map(|_| room.effective_bytes),
            ));
            if matches!(tone, Tone::Warning | Tone::Critical) {
                return (tone, "Camera buffers are nearly full");
            }
        }
        if let Some(meter) = self
            .storage_status
            .state()
            .meter(&self.storage_policy(), self.storage_status.usage())
        {
            match meter.tone {
                storage_policy::Tone::Error => {
                    return (Tone::Critical, "Capture storage is nearly full");
                }
                storage_policy::Tone::Warning => {
                    return (Tone::Warning, "Capture storage is filling up");
                }
                storage_policy::Tone::Normal => {}
            }
        }
        if pressure.0 == Tone::Unknown
            || reading
                .host
                .cpu_percent
                .is_none_or(|value| !value.is_finite())
            || reading.policy.is_err()
            || reading.buffers.is_err()
            || !matches!(
                self.storage_status.state(),
                storage_policy::State::Available(_)
            )
            || self.storage_status.usage().is_none()
        {
            (Tone::Unknown, "Some usage is unavailable")
        } else {
            (Tone::Normal, pressure.1)
        }
    }

    pub(super) fn resources_indicator(
        &self,
        show_label: bool,
        p: &'static Palette,
    ) -> Element<'_, Message> {
        let (tone, status) = self.resource_status();
        let mut label = row![icon(Icon::Sliders, 16.0, tone.ink(p))]
            .spacing(6)
            .align_y(Alignment::Center);
        if show_label {
            label = label.push(text("Resources").size(style::SMALL));
        }
        let marker = match tone {
            Tone::Normal => p.live,
            Tone::Warning => p.warn,
            Tone::Critical => p.danger,
            Tone::Unknown => p.secondary,
        };
        let content = row![label, dot(marker, 5.0)]
            .spacing(4)
            .align_y(Alignment::Center);
        tip_above(
            button(content)
                .name("Resource usage and limits")
                .description(status)
                .expanded(self.resources.open)
                .padding([4, 6])
                .style(style::plain)
                .on_press(Message::Resources(Input::Open(true))),
            format!("{status}. Open resource usage and limits."),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LimitChoice {
    value: Option<u64>,
    label: String,
}
impl fmt::Display for LimitChoice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.label)
    }
}
fn choices(resource: Resource, current: Option<u64>) -> (Vec<LimitChoice>, LimitChoice) {
    let values: &[u64] = match resource {
        Resource::Cpu => &[1, 2, 4, 8],
        Resource::Memory => &[512 * MIB, GIB, 2 * GIB, 4 * GIB],
        Resource::Disk => &[],
    };
    let choice = |value| LimitChoice {
        value,
        label: match value {
            None => "Automatic".into(),
            Some(value) => match resource {
                Resource::Cpu => {
                    format!("{value} {}", if value == 1 { "thread" } else { "threads" })
                }
                _ => bytes(value),
            },
        },
    };
    let selected = choice(current);
    let mut options = vec![choice(None)];
    options.extend(values.iter().map(|value| choice(Some(*value))));
    if !options.contains(&selected) {
        options.push(selected.clone());
    }
    (options, selected)
}

/// The actual observed cause, separate from the sampler's conservative state
/// during warmup and the short release hysteresis after a busy period.
fn pressure_view(host: &crate::resources::HostSample) -> (Tone, &'static str, bool) {
    match host.pressure_reason() {
        Some("Checking available resources") => (Tone::Unknown, "Checking device headroom", false),
        Some("Giving capture extra headroom") => {
            (Tone::Normal, "Giving capture extra headroom", false)
        }
        Some(reason) => (
            if host.pressure == Pressure::Critical {
                Tone::Critical
            } else {
                Tone::Warning
            },
            reason,
            true,
        ),
        None if host.cpu_percent.is_none()
            || host.memory_total.is_none()
            || host.memory_available.is_none() =>
        {
            (Tone::Unknown, "Some device usage is unavailable", false)
        }
        None => (Tone::Normal, "Within resource limits", false),
    }
}

impl Workbench {
    fn resource_limit_control(
        &self,
        resource: Resource,
        p: &'static Palette,
    ) -> Element<'_, Message> {
        let policy = self.resources.policy();
        let current = policy.and_then(|policy| match resource {
            Resource::Cpu => policy.cpu_threads.map(u64::from),
            Resource::Memory => policy.buffer_bytes,
            Resource::Disk => None,
        });
        let (options, selected) = choices(resource, current);
        let (label, name) = match resource {
            Resource::Cpu => ("Image processing", "Image processing thread limit"),
            Resource::Memory => ("Camera buffers", "Shared camera buffer limit"),
            Resource::Disk => ("Disk", "Disk limit"),
        };
        row![
            text(label).size(style::BODY).color(p.text).width(Fill),
            pick_list(options, policy.map(|_| selected), move |choice| {
                Message::Resources(Input::Change(match resource {
                    Resource::Cpu => Change::Cpu(choice.value.map(|value| value as u32)),
                    Resource::Memory => Change::Memory(choice.value),
                    Resource::Disk => Change::Automatic,
                }))
            })
            .name(name)
            .placeholder(if self.resources.reading.is_some() {
                "Unavailable"
            } else {
                "Checking…"
            })
            .enabled(policy.is_some() && !self.resources.saving)
            .width(190)
            .text_size(style::BODY)
            .padding([6, 8])
            .style(style::pick)
            .menu_style(style::menu),
        ]
        .spacing(12)
        .align_y(Alignment::Center)
        .into()
    }

    fn resource_row<'a>(
        &'a self,
        resource: Resource,
        title: &'static str,
        meter: ResourceMeter,
        controls: Element<'a, Message>,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let ResourceMeter {
            summary,
            details,
            fraction,
            tone,
        } = meter;
        let expanded = self.resources.expanded == Some(resource);
        let header = row![
            text(title)
                .size(style::BODY)
                .font(style::SEMIBOLD)
                .width(108),
            text(summary.clone())
                .size(style::BODY)
                .color(tone.ink(p))
                .width(Fill),
            button(
                row![
                    text(if expanded { "Done" } else { "Adjust" }).size(style::SMALL),
                    icon(
                        if expanded {
                            Icon::ChevronDown
                        } else {
                            Icon::ChevronRight
                        },
                        11.0,
                        p.secondary
                    )
                ]
                .spacing(6)
                .align_y(Alignment::Center)
            )
            .name(format!(
                "{} {title} limits",
                if expanded { "Close" } else { "Adjust" }
            ))
            .expanded(expanded)
            .style(style::plain)
            .padding([3, 5])
            .on_press(Message::Resources(Input::Expand(resource))),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let mut content = column![header].spacing(6);
        if let Some(fraction) = fraction {
            let bar = tone.bar(p);
            content = content.push(progress_bar(0.0..=1.0, fraction).girth(5).style(
                move |theme| iced::widget::progress_bar::Style {
                    bar: bar.into(),
                    ..style::progress(theme)
                },
            ));
        }
        if resource == Resource::Cpu
            && !expanded
            && let Some(reading) = self.resources.live(self.now)
        {
            let mode = reading.policy.as_ref().ok().map_or("Enforced", |policy| {
                if policy.cpu_threads.is_none() {
                    "Automatic"
                } else {
                    "Limit"
                }
            });
            content = content.push(
                text(format!(
                    "{mode} · up to {} image/codec {} per camera",
                    reading.threads,
                    if reading.threads == 1 {
                        "thread"
                    } else {
                        "threads"
                    }
                ))
                .size(style::SMALL)
                .color(p.secondary),
            );
        }
        for (index, detail) in details.into_iter().enumerate() {
            if expanded || resource == Resource::Memory && index == 0 {
                content = content.push(text(detail).size(style::SMALL).color(p.secondary));
            }
        }
        if expanded {
            content = content.push(container(controls).padding(iced::Padding {
                top: 5.0,
                ..iced::Padding::ZERO
            }));
        }
        accessibility::group(
            content.into(),
            format!("{title} resource: {summary}"),
            accessibility::Role::Group,
        )
    }

    fn cpu_resources(&self, p: &'static Palette) -> Element<'_, Message> {
        let reading = self.resources.live(self.now);
        let (summary, fraction, tone) = reading.map_or_else(
            || ("Checking CPU use…".into(), None, Tone::Unknown),
            |reading| {
                let fraction = reading
                    .host
                    .cpu_percent
                    .and_then(|percent| share(f64::from(percent), 100.0));
                let summary = reading
                    .host
                    .cpu_percent
                    .filter(|value| value.is_finite())
                    .map_or_else(
                        || "CPU usage unavailable".into(),
                        |percent| {
                            format!(
                                "{:.0}% device load · {:.0}% idle",
                                percent,
                                (100.0 - percent).max(0.0)
                            )
                        },
                    );
                (summary, fraction, fraction_tone(fraction))
            },
        );
        let mut details = Vec::new();
        if let Some(reading) = reading {
            let policy = reading.policy.as_ref().ok();
            let requested = policy.and_then(|policy| policy.cpu_threads);
            details.push(format!(
                "{}: up to {} image/codec {} per camera ({} CPU {} available).",
                if policy.is_some() {
                    mode(requested.map(u64::from))
                } else {
                    "Enforced limit"
                },
                reading.threads,
                if reading.threads == 1 {
                    "thread"
                } else {
                    "threads"
                },
                reading.host.cpu_cores,
                if reading.host.cpu_cores == 1 {
                    "core"
                } else {
                    "cores"
                }
            ));
            if requested.is_some_and(|cap| cap as usize > reading.threads) {
                details.push(if reading.host.pressure == Pressure::Normal {
                    "Available CPU cores constrain the effective thread limit.".into()
                } else {
                    "Resource safeguards temporarily reduce the effective thread limit.".into()
                });
            }
        }
        let controls = column![
            self.resource_limit_control(Resource::Cpu, p),
            text("Bounds image processing and new software encoders; it is not a hard limit on total app or device CPU use.")
                .size(style::SMALL).color(p.secondary),
            self.performance_section(p),
        ].spacing(12).into();
        self.resource_row(
            Resource::Cpu,
            "CPU",
            ResourceMeter {
                summary,
                details,
                fraction,
                tone,
            },
            controls,
            p,
        )
    }

    fn memory_resources(&self, p: &'static Palette) -> Element<'_, Message> {
        let reading = self.resources.live(self.now);
        let (summary, fraction, tone) = match reading {
            Some(reading) => match &reading.buffers {
                Ok(usage) => {
                    let room = crate::resources::buffers::headroom(
                        usage.used_bytes,
                        reading.buffer_limit,
                        reading.host,
                    );
                    let fraction = buffer_fraction(
                        usage.used_bytes,
                        reading.buffer_limit,
                        room.physical_bytes.map(|_| room.effective_bytes),
                    );
                    let remaining = if usage.used_bytes > reading.buffer_limit {
                        remaining(usage.used_bytes, reading.buffer_limit)
                    } else if room.physical_bytes.is_some() {
                        format!("{} headroom", bytes(room.effective_bytes))
                    } else {
                        format!(
                            "{} budget room; device headroom unknown",
                            bytes(room.budget_bytes)
                        )
                    };
                    (
                        format!(
                            "{} buffers / {} · {}",
                            bytes(usage.used_bytes),
                            bytes(reading.buffer_limit),
                            remaining
                        ),
                        fraction,
                        if room.physical_bytes.is_some() {
                            fraction_tone(fraction)
                        } else {
                            Tone::Unknown
                        },
                    )
                }
                Err(_) => (
                    "Camera-buffer usage unavailable".into(),
                    None,
                    Tone::Unknown,
                ),
            },
            None => ("Checking memory…".into(), None, Tone::Unknown),
        };
        let mut details = Vec::new();
        if let Some(reading) = reading {
            if let Some(available) = reading.host.memory_available {
                details.push(format!(
                    "Camera buffers · {} available on this computer{}.",
                    bytes(available),
                    reading
                        .host
                        .memory_total
                        .map_or_else(String::new, |total| format!(" out of {}", bytes(total)))
                ));
            } else {
                details.push("System memory headroom unavailable.".into());
            }
            if let Ok(usage) = &reading.buffers {
                let room = crate::resources::buffers::headroom(
                    usage.used_bytes,
                    reading.buffer_limit,
                    reading.host,
                );
                if room
                    .physical_bytes
                    .is_some_and(|physical| physical < room.budget_bytes)
                {
                    details.push(
                        "Available device memory is the current limit on new camera buffers."
                            .into(),
                    );
                } else if room.physical_bytes.is_none() {
                    details.push("Device memory headroom is unavailable; budget room alone does not guarantee a new buffer.".into());
                }
                details.push(format!(
                    "{} across {} active buffer {}.",
                    if reading.policy.is_ok() {
                        mode(
                            reading
                                .policy
                                .as_ref()
                                .ok()
                                .and_then(|policy| policy.buffer_bytes),
                        )
                    } else {
                        "Enforced buffer budget"
                    },
                    usage.allocations,
                    if usage.allocations == 1 {
                        "allocation"
                    } else {
                        "allocations"
                    }
                ));
            }
        }
        let controls = column![
            self.resource_limit_control(Resource::Memory, p),
            text("This budget covers shared camera frame buffers, not total app memory. A lower budget blocks new buffers until there is room; existing buffers are kept.")
                .size(style::SMALL).color(p.secondary),
        ].spacing(10).into();
        self.resource_row(
            Resource::Memory,
            "Memory",
            ResourceMeter {
                summary,
                details,
                fraction,
                tone,
            },
            controls,
            p,
        )
    }

    fn disk_resources(&self, p: &'static Palette) -> Element<'_, Message> {
        let meter = self
            .storage_status
            .state()
            .meter(&self.storage_policy(), self.storage_status.usage());
        let (summary, fraction, tone) = meter.map_or_else(
            || {
                (
                    if matches!(
                        self.storage_status.state(),
                        storage_policy::State::Unavailable(_)
                    ) {
                        "Capture storage unavailable".into()
                    } else {
                        "Checking capture storage…".into()
                    },
                    None,
                    Tone::Unknown,
                )
            },
            |meter| {
                let tone = match meter.tone {
                    storage_policy::Tone::Normal => Tone::Normal,
                    storage_policy::Tone::Warning => Tone::Warning,
                    storage_policy::Tone::Error => Tone::Critical,
                };
                let summary = match (self.storage_status.state(), self.storage_status.usage()) {
                    (storage_policy::State::Available(policy), Some(usage)) => {
                        let requested = self.storage_policy();
                        let cap = policy.max_bytes.min(requested.max_bytes);
                        let remaining_bytes = cap.saturating_sub(usage.bytes);
                        let room = usage.free.map(|free| {
                            remaining_bytes
                                .min(free.saturating_sub(crate::storage::FREE_SPACE_FLOOR))
                        });
                        if usage.files >= u64::from(policy.max_files.min(requested.max_files)) {
                            format!(
                                "File limit reached · {} of {}",
                                bytes(usage.bytes),
                                bytes(cap)
                            )
                        } else if let Some(room) = room {
                            format!(
                                "{} of {} · {} headroom",
                                bytes(usage.bytes),
                                bytes(cap),
                                bytes(room)
                            )
                        } else {
                            format!(
                                "{} of {} · disk headroom unverified",
                                bytes(usage.bytes),
                                bytes(cap)
                            )
                        }
                    }
                    _ => meter.text,
                };
                (summary, Some(meter.fraction), tone)
            },
        );
        let mut details = Vec::new();
        if let (storage_policy::State::Available(policy), Some(usage)) =
            (self.storage_status.state(), self.storage_status.usage())
        {
            let requested = self.storage_policy();
            let cap = policy.max_bytes.min(requested.max_bytes);
            let room = cap.saturating_sub(usage.bytes);
            let files =
                u64::from(policy.max_files.min(requested.max_files)).saturating_sub(usage.files);
            details.push(if let Some(free) = usage.free {
                format!(
                    "{} byte/disk headroom · {} file slots remaining.",
                    bytes(room.min(free.saturating_sub(crate::storage::FREE_SPACE_FLOOR))),
                    grouped(files)
                )
            } else {
                format!(
                    "{} byte-budget headroom · {} file slots remaining. Disk space is unverified.",
                    bytes(room),
                    grouped(files)
                )
            });
        }
        let mut controls = column![self.storage_controls(p)].spacing(10);
        for caption in self.storage_status.state().captions(&self.storage_policy()) {
            controls = controls.push(text(caption.text).size(style::SMALL).color(
                match caption.tone {
                    storage_policy::Tone::Normal => p.secondary,
                    storage_policy::Tone::Warning => p.ink(p.warn),
                    storage_policy::Tone::Error => p.ink(p.danger),
                },
            ));
        }
        self.resource_row(
            Resource::Disk,
            "Disk",
            ResourceMeter {
                summary,
                details,
                fraction,
                tone,
            },
            controls.into(),
            p,
        )
    }

    fn delivery_resources(&self, p: &'static Palette) -> Element<'_, Message> {
        let delivery = &self.resources.delivery;
        let (lost, skipped, dropped, late) = delivery.recent();
        let heading_text = if lost > 0 {
            "Camera frames lost"
        } else if dropped > 0 || late > 0 {
            "Recording or stream frames dropped"
        } else if delivery.encoder_stalled {
            "Encoder is waiting for frames"
        } else if !delivery.streaming {
            "Frame delivery idle"
        } else if !delivery.ready {
            "Checking frame delivery…"
        } else {
            "No recent frame loss"
        };
        let warning = lost > 0 || dropped > 0 || late > 0 || delivery.encoder_stalled;
        if !warning {
            let message = if skipped > 0 {
                let mut message = format!(
                    "No recent loss reported · {} preview frames skipped in 10 seconds.",
                    grouped(skipped)
                );
                if let Some(note) = self.preview_limit_note() {
                    message.push(' ');
                    message.push_str(&note);
                }
                message
            } else {
                heading_text.to_owned()
            };
            return accessibility::group(
                text(message).size(style::SMALL).color(p.secondary).into(),
                "Frame delivery".into(),
                accessibility::Role::Group,
            );
        }
        let mut body = column![
            text(heading_text)
                .size(style::BODY)
                .font(style::MEDIUM)
                .color(p.ink(p.warn))
        ]
        .spacing(6);
        if delivery.ready || warning || skipped > 0 {
            body = body.push(
                text(format!(
                    "Last 10 seconds: {} camera frames lost · {} preview frames skipped.",
                    grouped(lost),
                    grouped(skipped)
                ))
                .size(style::SMALL)
                .color(p.secondary),
            );
            body = body.push(
                text(format!(
                    "Recording/stream: {} frames dropped, including {} late at the encoder.",
                    grouped(dropped),
                    grouped(late)
                ))
                .size(style::SMALL)
                .color(if dropped > 0 {
                    p.ink(p.warn)
                } else {
                    p.secondary
                }),
            );
        }
        if delivery.encoder_stalled {
            body = body.push(
                text("The encoder has not received a new camera frame recently.")
                    .size(style::SMALL)
                    .color(p.ink(p.warn)),
            );
        }
        if late > 0
            && let Some(cap) = self
                .resources
                .policy()
                .and_then(|policy| policy.cpu_threads)
        {
            body = body.push(text(format!("The {}-thread image-work limit may reduce encoding headroom. Increase it or use a lighter recording setting if frames keep arriving late.", cap)).size(style::SMALL).color(p.secondary));
        }
        if let Some(note) = self.preview_limit_note() {
            body = body.push(
                text(format!(
                    "{note} Preview skips are separate from camera and encoder loss."
                ))
                .size(style::SMALL)
                .color(p.secondary),
            );
        } else if skipped > 0 {
            body = body.push(text("A newer frame replaced a preview frame. Preview skips are separate from camera and encoder loss.").size(style::SMALL).color(p.secondary));
        }
        accessibility::group(
            body.into(),
            "Frame delivery".into(),
            accessibility::Role::Group,
        )
    }

    pub(super) fn resources_sheet(&self, p: &'static Palette, height: f32) -> Element<'_, Message> {
        let (tone, status) = self.resource_status();
        let delivery = self.resources.delivery.recent();
        let delivery_warning = delivery.0 > 0
            || delivery.2 > 0
            || delivery.3 > 0
            || self.resources.delivery.encoder_stalled;
        let mut body = column![
            text(status)
                .size(style::SMALL)
                .font(style::MEDIUM)
                .color(tone.ink(p))
        ]
        .spacing(8);
        if self.resources.saving {
            body = body.push(
                text("Applying limits…")
                    .size(style::SMALL)
                    .color(p.accent_text),
            );
        }
        if let Some(error) = &self.resources.error {
            body = body.push(field_error(error.clone(), p));
        }
        if let Some(Err(error)) = self
            .resources
            .reading
            .as_ref()
            .map(|reading| &reading.policy)
        {
            body = body.push(field_error(format!("Saved limits unavailable: {error}"), p));
        }
        if delivery_warning {
            body = body
                .push(self.delivery_resources(p))
                .push(rule::horizontal(1).style(style::line));
        }
        body = body
            .push(self.cpu_resources(p))
            .push(rule::horizontal(1).style(style::line))
            .push(self.memory_resources(p))
            .push(rule::horizontal(1).style(style::line))
            .push(rule::horizontal(1).style(style::line))
            .push(self.disk_resources(p));
        if !delivery_warning {
            body = body.push(self.delivery_resources(p));
        }
        let footer = row![
            text("Limits apply automatically. Active recordings are not restarted.")
                .size(style::SMALL)
                .color(p.secondary)
                .width(Fill),
            button(text("Automatic work limits").size(style::SMALL))
                .name("Reset CPU and camera buffers to Automatic")
                .padding([6, 10])
                .style(style::secondary)
                .on_press_maybe(
                    (!self.resources.saving)
                        .then_some(Message::Resources(Input::Change(Change::Automatic)))
                ),
        ]
        .spacing(12)
        .align_y(Alignment::Center);
        sheet_frame("Resources", Message::Resources(Input::Open(false)), body, p)
            .mark(Icon::Sliders)
            .size(620.0f32.min((self.width - 48.0).max(0.0)), height)
            .footer(footer)
            .scrolled(self.resources.scrolled)
            .on_scroll(|value| Message::Resources(Input::Scrolled(value)))
            .into()
    }
}

fn demo_reading() -> Reading {
    Reading {
        policy: Ok(Policy::default()),
        host: crate::resources::HostSample {
            cpu_percent: Some(24.0),
            cpu_cores: 8,
            memory_total: Some(16 * GIB),
            memory_available: Some(9 * GIB),
            pressure: Pressure::Normal,
        },
        buffers: Ok(crate::resources::buffers::BufferUsage {
            used_bytes: 384 * MIB,
            allocations: 2,
        }),
        threads: 7,
        buffer_limit: 2 * GIB,
    }
}

impl Workbench {
    pub(super) fn scene_resources(&mut self, word: &str, late: bool) -> bool {
        let resource = match word {
            "resources" | "resources-busy" | "resources-unavailable" | "resources-loss" => None,
            "resources-cpu" => Some(Resource::Cpu),
            "resources-memory" => Some(Resource::Memory),
            "resources-disk" => Some(Resource::Disk),
            _ => return false,
        };
        if late {
            return true;
        }
        let mut reading = demo_reading();
        self.resources.open = true;
        self.resources.scene = true;
        self.resources.expanded = resource;
        self.resources.delivery.ready = true;
        self.resources.delivery.streaming = true;
        self.storage_status.update(
            storage_policy::Reply {
                policy: Ok(storage_policy::Policy {
                    max_bytes: 10 * GIB,
                    max_files: 10_000,
                    max_age_seconds: None,
                    on_full: storage_policy::OnFull::Stop,
                }),
                usage: Some(storage_policy::Usage {
                    bytes: 2 * GIB,
                    files: 1280,
                    free: Some(24 * GIB),
                }),
            },
            self.now,
        );
        if word == "resources-busy" {
            reading.host.cpu_percent = Some(97.0);
            reading.host.memory_available = Some(512 * MIB);
            reading.host.pressure = Pressure::Critical;
            reading.threads = 1;
            reading.buffers.as_mut().unwrap().used_bytes = 1900 * MIB;
        }
        if word == "resources-unavailable" {
            reading.host.cpu_percent = None;
            reading.host.memory_available = None;
            reading.host.pressure = Pressure::Busy;
            reading.threads = 2;
            reading.buffers = Err("Usage is temporarily unavailable.".into());
            self.storage_status
                .update(Err("The storage policy cannot be read.".into()), self.now);
        }
        if word == "resources-loss" {
            self.resources
                .delivery
                .events
                .push_back((self.now, 3, 24, 2, 2));
        }
        self.resources.reading = Some(reading);
        self.resources.refreshed = Some(self.now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::advanced::{Layout, layout, widget::Tree};

    fn camera() -> CameraSnapshot {
        crate::tools::tests::fixture_snapshot().cameras.remove(0)
    }

    #[test]
    fn delivery_distinguishes_preview_skips_and_real_loss_without_adopting_reconnect_counters() {
        let start = Instant::now();
        let mut camera = camera();
        camera.worker_pid = 4;
        camera.lost = 8;
        camera.preview_skipped = 20;
        let mut delivery = Delivery::default();
        delivery.observe(&[camera.clone()], start);
        assert_eq!(
            delivery.recent(),
            (0, 0, 0, 0),
            "first snapshot is only a baseline"
        );
        camera.preview_skipped += 5;
        delivery.observe(&[camera.clone()], start + Duration::from_secs(1));
        assert_eq!(delivery.recent(), (0, 5, 0, 0));
        camera.lost += 2;
        delivery.observe(&[camera.clone()], start + Duration::from_secs(2));
        assert_eq!(delivery.recent(), (2, 5, 0, 0));
        camera.worker_pid = 5;
        camera.lost = 100;
        camera.preview_skipped = 100;
        delivery.observe(&[camera.clone()], start + Duration::from_secs(3));
        assert_eq!(
            delivery.recent(),
            (2, 5, 0, 0),
            "new worker identity is a new baseline"
        );
        camera.lost = 1;
        camera.preview_skipped = 1;
        delivery.observe(&[camera], start + Duration::from_secs(4));
        assert_eq!(
            delivery.recent(),
            (2, 5, 0, 0),
            "reset counters never underflow"
        );
        delivery.observe(&[], start + Duration::from_secs(15));
        assert_eq!(delivery.recent(), (0, 0, 0, 0));
        assert!(!delivery.streaming);
    }

    #[test]
    fn stale_snapshots_cannot_report_new_loss() {
        let mut camera = camera();
        camera.worker_pid = 9;
        camera.lost = 0;
        let start = Instant::now();
        let mut delivery = Delivery::default();
        delivery.observe(&[camera.clone()], start);
        camera.stale = true;
        camera.lost = 800;
        delivery.observe(&[camera], start + Duration::from_secs(1));
        assert_eq!(delivery.recent(), (0, 0, 0, 0));
    }

    #[test]
    fn encoder_progress_is_separate_and_bound_to_each_recording() {
        let start = Instant::now();
        let mut camera = camera();
        camera.worker_pid = 4;
        camera.forwarding = Some("recording.mkv".into());
        camera.forwarding_progress = Some(crate::session::ForwardingProgress {
            id: "first".into(),
            accepted: 20,
            written: 20,
            dropped: 0,
            late: 0,
            stalled: false,
        });
        let mut delivery = Delivery::default();
        delivery.observe(&[camera.clone()], start);
        let progress = camera.forwarding_progress.as_mut().unwrap();
        progress.dropped = 3;
        progress.late = 2;
        delivery.observe(&[camera.clone()], start + Duration::from_secs(1));
        assert_eq!(delivery.recent().2, 3);
        assert_eq!(delivery.recent().3, 2);
        let progress = camera.forwarding_progress.as_mut().unwrap();
        progress.id = "second".into();
        progress.dropped = 1;
        progress.late = 1;
        progress.stalled = true;
        delivery.observe(&[camera.clone()], start + Duration::from_secs(2));
        assert_eq!(delivery.recent().2, 4);
        assert_eq!(delivery.recent().3, 3);
        assert!(delivery.encoder_stalled);
        camera.forwarding = None;
        delivery.observe(&[camera], start + Duration::from_secs(3));
        assert!(
            !delivery.encoder_stalled,
            "retained final counters cannot claim an active stall"
        );
        assert_eq!(
            delivery.recent().2,
            4,
            "unchanged retained counters are not counted twice"
        );
    }

    #[test]
    fn unknown_pressure_never_claims_a_confirmed_overload() {
        let mut host = demo_reading().host;
        host.cpu_percent = None;
        host.pressure = Pressure::Busy;
        assert_eq!(
            pressure_view(&host),
            (Tone::Unknown, "Checking device headroom", false)
        );
        host.cpu_percent = Some(92.0);
        assert!(pressure_view(&host).2);
        host.cpu_percent = Some(20.0);
        assert_eq!(
            pressure_view(&host),
            (Tone::Normal, "Giving capture extra headroom", false)
        );
    }

    #[test]
    fn camera_buffer_meter_uses_physical_headroom_without_hiding_the_budget() {
        let host = crate::resources::HostSample {
            cpu_percent: Some(20.0),
            cpu_cores: 8,
            memory_total: Some(16 * GIB),
            memory_available: Some(GIB),
            pressure: Pressure::Busy,
        };
        let room = crate::resources::buffers::headroom(384 * MIB, 2 * GIB, host);
        assert!(room.effective_bytes < room.budget_bytes);
        assert!(
            buffer_fraction(384 * MIB, 2 * GIB, Some(room.effective_bytes)).unwrap()
                > buffer_fraction(384 * MIB, 2 * GIB, None).unwrap()
        );
        assert_eq!(buffer_fraction(0, GIB, Some(0)), Some(1.0));
    }

    #[test]
    fn custom_saved_choices_stay_selected_and_over_budget_text_never_promises_headroom() {
        let (options, selected) = choices(Resource::Cpu, Some(3));
        assert!(options.contains(&selected));
        assert_eq!(selected.label, "3 threads");
        let (options, selected) = choices(Resource::Memory, Some(768 * MIB));
        assert!(options.contains(&selected));
        assert_eq!(selected.label, "768 MiB");
        assert_eq!(remaining(2 * GIB, GIB), "1.0 GiB over the limit");
        assert_eq!(share(f64::NAN, 1.0), None);
    }

    #[test]
    fn resources_are_reachable_and_keep_close_and_reset_visible_at_every_interface_size() {
        let mut renderer = super::super::destinations::layout_tests::renderer();
        for dark in [false, true] {
            for scale in [1.0, 1.15, 1.3] {
                let size = Size::new(900.0 / scale, 620.0 / scale);
                let height = size.height - 80.0;
                let mut bench =
                    Workbench::new(SessionHandle::new(), "resources-layout".into(), true, None);
                bench.width = size.width;
                assert!(bench.scene_resources("resources", false));
                let mut view = bench.resources_sheet(Palette::of(dark), height);
                let mut tree = Tree::new(view.as_widget());
                let node = view.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, size),
                );
                let snapshot = accessibility::snapshot_widget(
                    view.as_widget_mut(),
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                )
                .0;
                for label in [
                    "Close Resources",
                    "Reset CPU and camera buffers to Automatic",
                ] {
                    let button = snapshot
                        .nodes
                        .iter()
                        .find(|(_, node)| node.label() == Some(label))
                        .unwrap_or_else(|| panic!("missing {label}"));
                    let bounds = button.1.bounds().unwrap();
                    assert!(
                        bounds.x0 >= 0.0
                            && bounds.y0 >= 0.0
                            && bounds.x1 <= f64::from(size.width) + 0.5
                            && bounds.y1 <= f64::from(height) + 0.5,
                        "{label} clipped at {size:?}: {bounds:?}"
                    );
                    assert!(button.1.supports_action(accesskit::Action::Click));
                }
                for title in ["CPU", "Memory", "Disk"] {
                    assert!(
                        snapshot.nodes.iter().any(|(_, node)| node
                            .label()
                            .is_some_and(|label| label.starts_with(&format!("{title} resource:")))),
                        "missing {title}"
                    );
                }
                super::super::destinations::layout_tests::render_artifact_scaled(
                    &format!("resources-{dark}-{scale}"),
                    &view,
                    &tree,
                    &node,
                    &mut renderer,
                    size,
                    scale,
                );
            }
        }
    }

    #[test]
    fn resource_indicator_has_stable_name_and_open_action_when_pressure_changes() {
        let renderer = super::super::destinations::layout_tests::renderer();
        let mut bench = Workbench::new(
            SessionHandle::new(),
            "resources-indicator".into(),
            true,
            None,
        );
        let mut tree = Tree::empty();
        let mut previous = None;
        for scene in [
            "resources",
            "resources-busy",
            "resources-unavailable",
            "resources-loss",
        ] {
            bench.scene_resources(scene, false);
            let mut view = bench.resources_indicator(true, Palette::of(false));
            tree.diff(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(200.0, 40.0)),
            );
            let snapshot = accessibility::snapshot_widget(
                view.as_widget_mut(),
                &mut tree,
                Layout::new(&node),
                &renderer,
            )
            .0;
            let (id, control) = snapshot
                .nodes
                .iter()
                .find(|(_, node)| node.role() == accessibility::Role::Button)
                .unwrap();
            assert_eq!(control.label(), Some("Resource usage and limits"));
            assert!(control.supports_action(accesskit::Action::Click));
            if let Some(previous) = previous {
                assert_eq!(*id, previous);
            }
            previous = Some(*id);
        }
    }

    #[test]
    fn stale_readings_and_unavailable_buffer_usage_cannot_claim_headroom() {
        let mut bench = Workbench::new(SessionHandle::new(), "resources-stale".into(), true, None);
        bench.scene_resources("resources", false);
        bench.resources.reading.as_mut().unwrap().buffers = Err("Usage unavailable".into());
        assert_eq!(bench.resource_status().0, Tone::Unknown);
        bench.resources.scene = false;
        bench.resources.refreshed = Some(bench.now - Duration::from_secs(6));
        assert!(bench.resources.live(bench.now).is_none());
        assert_eq!(
            bench.resource_status(),
            (Tone::Unknown, "Checking resource use")
        );
    }

    #[test]
    fn preview_cap_explanations_distinguish_user_choice_from_automatic_pressure() {
        let mut bench = Workbench::new(
            SessionHandle::new(),
            "resources-preview-cause".into(),
            true,
            None,
        );
        bench.performance = crate::efficiency::Mode::Efficient;
        performance::start(bench.performance);
        assert!(
            bench
                .preview_limit_note()
                .unwrap()
                .starts_with("Your Efficient setting")
        );
        bench.performance = crate::efficiency::Mode::Auto;
        performance::start(bench.performance);
        performance::resource_pressure(Some("device CPU or memory pressure"));
        assert!(
            bench
                .preview_limit_note()
                .unwrap()
                .starts_with("Automatic protection for device pressure")
        );
        performance::resource_pressure(None);
        assert!(bench.preview_limit_note().is_none());
    }

    #[test]
    fn unavailable_policy_or_storage_cannot_claim_healthy_resource_limits() {
        let mut bench = Workbench::new(
            SessionHandle::new(),
            "resources-policy-status".into(),
            true,
            None,
        );
        bench.scene_resources("resources", false);
        bench.resources.reading.as_mut().unwrap().policy = Err("unreadable settings".into());
        assert_eq!(bench.resource_status().0, Tone::Unknown);
        bench.scene_resources("resources", false);
        bench
            .storage_status
            .update(Err("unavailable disk policy".into()), bench.now);
        assert_eq!(bench.resource_status().0, Tone::Unknown);
        bench.resources.reading.as_mut().unwrap().host.cpu_percent = Some(98.0);
        bench.resources.reading.as_mut().unwrap().host.pressure = Pressure::Critical;
        assert_eq!(
            bench.resource_status().0,
            Tone::Critical,
            "known pressure remains visible even if another reading fails"
        );
    }

    #[test]
    fn stale_policy_samples_are_ignored_and_pressure_only_adjusts_automatic_preview() {
        let mut bench = Workbench::new(
            SessionHandle::new(),
            "resources-generation".into(),
            true,
            None,
        );
        bench.resources.reading = Some(demo_reading());
        bench.resources.generation = 2;
        let mut busy = demo_reading();
        busy.host.cpu_percent = Some(98.0);
        busy.host.pressure = Pressure::Critical;
        let (sender, receiver) = mpsc::channel();
        sender.send((1, busy.clone())).unwrap();
        bench.resources.pending = Some(receiver);
        bench.tick_resources();
        assert_eq!(
            bench.resources.reading.as_ref().unwrap().host.cpu_percent,
            Some(24.0)
        );
        performance::start(crate::efficiency::Mode::Smooth);
        let (sender, receiver) = mpsc::channel();
        sender.send((2, busy.clone())).unwrap();
        bench.resources.pending = Some(receiver);
        bench.tick_resources();
        assert!(
            !performance::efficient(),
            "explicit Smooth stays a user choice"
        );
        performance::start(crate::efficiency::Mode::Auto);
        let (sender, receiver) = mpsc::channel();
        sender.send((2, busy)).unwrap();
        bench.resources.pending = Some(receiver);
        bench.tick_resources();
        assert!(
            performance::efficient(),
            "Automatic answers measured pressure"
        );
        assert!(
            bench.handle.snapshot().cameras.is_empty(),
            "sampling never opens or changes a camera"
        );
    }

    #[test]
    fn resource_sheet_closes_with_escape_and_never_pretends_missing_usage_is_zero() {
        let mut bench =
            Workbench::new(SessionHandle::new(), "resources-unknown".into(), true, None);
        bench.scene_resources("resources-unavailable", false);
        assert_eq!(bench.resource_status().0, Tone::Unknown);
        assert!(bench.sheet_open());
        let _ = bench.dispatch_action(Action::Overview);
        assert!(!bench.resources.open);
        assert!(!bench.sheet_open());
        assert!(
            bench.handle.snapshot().cameras.is_empty(),
            "scene never opens hardware"
        );
    }
}
