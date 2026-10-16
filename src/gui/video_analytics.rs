//! Prompt editors and credentials stay in the workbench; all provider,
//! credential-store, database and chart work runs on background workers.
use super::*;
use crate::ai::{self, MeasurementDefinition, ModelConfig, Provider};
use crate::analytics::MeasurementSample;
use crate::video_analytics::{VideoAnalytics, VideoAnalyticsConfig};
use iced::widget::{column, svg};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const MAX_MEASUREMENTS: usize = 16;
const REFRESH: Duration = Duration::from_secs(5);

/// This is the complete persistent analytics state. Credentials and the
/// running flag deliberately have no representation in preferences.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Preferences {
    pub model: ModelConfig,
    pub measurements: Vec<MeasurementDefinition>,
    pub interval_seconds: u64,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            model: ModelConfig::default(),
            measurements: vec![MeasurementDefinition {
                id: "tubes".into(),
                label: "Tube count".into(),
                prompt: "Count the number of tubes visible in the image.".into(),
                unit: "tubes".into(),
            }],
            interval_seconds: 30,
        }
    }
}

/// iced messages are Debug, including messages carrying password edits.
/// Keep this wrapper private and redact it at that boundary.
#[derive(Clone, Default)]
pub(super) struct SecretDraft(Zeroizing<String>);

impl fmt::Debug for SecretDraft {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretDraft([redacted])")
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Field {
    Label,
    Prompt,
    Unit,
}

#[derive(Clone, Debug)]
pub(super) enum Input {
    Provider(Provider),
    Model(String),
    Interval(String),
    Measurement(usize, Field, String),
    Add,
    Remove(usize),
    Start,
    Stop,
    CredentialProvider(Provider),
    Secret(SecretDraft),
    SaveKey,
    DeleteKey,
    RefreshKey,
}

enum CredentialAction {
    Status,
    Save(SecretDraft),
    Delete,
}

type CredentialReply = std::result::Result<(String, Option<&'static str>), &'static str>;
type Report =
    std::result::Result<(Vec<MeasurementSample>, String, crate::analytics::SvgOptions), String>;

pub(super) struct State {
    pub preferences: Preferences,
    pub runner: VideoAnalytics,
    interval: String,
    pub credential_provider: Provider,
    secret: SecretDraft,
    credential_status: Option<String>,
    credential_notice: Option<&'static str>,
    credential_error: Option<&'static str>,
    credential_pending: Option<Receiver<CredentialReply>>,
    pending: Option<Receiver<Report>>,
    refreshed: Option<Instant>,
    samples: Vec<MeasurementSample>,
    chart: Option<svg::Handle>,
    chart_options: Option<crate::analytics::SvgOptions>,
    requested_options: Option<crate::analytics::SvgOptions>,
    error: Option<String>,
    history_error: Option<String>,
    paused_for_edit: bool,
    pub running_camera: Option<String>,
}

impl State {
    pub(super) fn new(handle: SessionHandle, mut preferences: Preferences) -> Self {
        preferences.measurements.truncate(MAX_MEASUREMENTS);
        let credential_provider = preferences.model.provider;
        Self {
            interval: preferences.interval_seconds.to_string(),
            preferences,
            runner: VideoAnalytics::new(handle),
            credential_provider,
            secret: SecretDraft::default(),
            credential_status: None,
            credential_notice: None,
            credential_error: None,
            credential_pending: None,
            pending: None,
            refreshed: None,
            samples: Vec::new(),
            chart: None,
            chart_options: None,
            requested_options: None,
            error: None,
            history_error: None,
            paused_for_edit: false,
            running_camera: None,
        }
    }

    fn edited(&mut self) {
        if self.runner.status().running {
            self.runner.stop();
            self.paused_for_edit = true;
        }
        self.error = None;
    }

    fn interval_value(&self) -> Option<u64> {
        self.interval
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|value| (5..=3600).contains(value))
    }

    fn config(&self, camera_id: String) -> Result<VideoAnalyticsConfig> {
        let interval_seconds = self.interval_value().ok_or_else(|| {
            anyhow::anyhow!("Enter a whole sample interval from 5 to 3,600 seconds.")
        })?;
        self.preferences.model.validate()?;
        anyhow::ensure!(
            !self.preferences.measurements.is_empty(),
            "Add at least one measurement."
        );
        for measurement in &self.preferences.measurements {
            measurement.validate()?;
        }
        Ok(VideoAnalyticsConfig {
            camera_id,
            model: self.preferences.model.clone(),
            measurements: self.preferences.measurements.clone(),
            interval_seconds,
        })
    }

    pub(super) fn clear_secret(&mut self) {
        self.secret = SecretDraft::default();
    }

    pub(super) fn refresh_credentials(&mut self) {
        if self.credential_pending.is_none() {
            self.credentials(CredentialAction::Status);
        }
    }

    fn credentials(&mut self, action: CredentialAction) {
        if self.credential_pending.is_some() {
            return;
        }
        let provider = self.credential_provider;
        self.credential_error = None;
        self.credential_notice = None;
        // Neither resolving credentials nor entering a native keychain
        // authorization dialog can block iced's event loop.
        match spawn_credentials(move || {
            let notice = match action {
                CredentialAction::Status => None,
                CredentialAction::Save(secret) => {
                    ai::store_api_key(provider, &secret.0)
                        .map_err(|_| "The OS credential store could not save this key.")?;
                    Some("API key saved in the OS credential store.")
                }
                CredentialAction::Delete => {
                    ai::delete_api_key(provider)
                        .map_err(|_| "The OS credential store could not remove the saved key.")?;
                    Some("Saved API key removed. Environment variables still take precedence.")
                }
            };
            let status = ai::credential_status(provider)
                .map_err(|_| "The OS credential store could not be read.")?;
            Ok((status.to_string(), notice))
        }) {
            Ok(receiver) => self.credential_pending = Some(receiver),
            Err(_) => self.credential_error = Some("Could not start the credential worker."),
        }
    }
}

fn spawn_credentials(
    work: impl FnOnce() -> CredentialReply + Send + 'static,
) -> std::io::Result<Receiver<CredentialReply>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("capturefab-api-credentials".into())
        .spawn(move || {
            let _ = sender.send(work());
        })?;
    Ok(receiver)
}

impl Workbench {
    /// Synthetic UI-review data. These fixtures never resolve/store a key,
    /// contact a provider, start collection, or write a measurement database.
    pub(super) fn scene_video_analytics(&mut self, word: &str, late: bool) -> bool {
        let options = self.analytics_svg_options();
        match word {
            "settings-credentials" | "settings-credential-error" => {
                self.settings_open = true;
                if !late {
                    return false;
                }
                let state = &mut self.video_analytics;
                state.credential_provider = Provider::Gemini;
                state.credential_pending = None;
                state.credential_status =
                    Some("No saved key. Enter an API key to enable prompt measurements.".into());
                state.secret = SecretDraft(Zeroizing::new("example-ui-review-key".into()));
                if word == "settings-credential-error" {
                    state.credential_error =
                        Some("The OS credential store could not save this key.");
                }
            }
            "analytics-measurements" => {
                self.analytics.open = true;
                self.analytics.tab = analytics::Tab::Measurements;
            }
            "analytics-measurements-populated" | "analytics-invalid" => {
                self.analytics.open = true;
                self.analytics.tab = analytics::Tab::Measurements;
                if !late {
                    return false;
                }
                let state = &mut self.video_analytics;
                state.preferences.measurements.push(MeasurementDefinition {
                    id: "ribbon-length".into(),
                    label: "Ribbon length at the inspection station".into(),
                    prompt: "Estimate the visible ribbon length using the 10 cm calibration marker beside the conveyor.".into(),
                    unit: "meters".into(),
                });
                state.samples = (0..6)
                    .map(|index| MeasurementSample {
                        timestamp_ms: 1_792_012_000_000 + index * 30_000,
                        camera_id: "sim:0 / laboratory acquisition camera".into(),
                        frame_id: index,
                        definition: state.preferences.measurements[1].clone(),
                        provider: state.preferences.model.provider,
                        model: state.preferences.model.model.clone(),
                        result: ai::MeasurementOutput {
                            id: "ribbon-length".into(),
                            value: (index != 2).then_some(0.45 + index as f64 * 0.03),
                            confidence: (index != 2).then_some(0.92),
                            note: if index == 2 {
                                "The calibration marker is obscured; the image does not support a measurement."
                            } else {
                                "Estimated from the visible calibration marker. The ribbon curves slightly near the edge of the frame."
                            }.into(),
                        },
                    })
                    .collect();
                if let Ok(chart) =
                    crate::analytics::render_measurement_svg_with_options(&state.samples, options)
                {
                    state.chart = Some(svg::Handle::from_memory(chart.into_bytes()));
                    state.chart_options = Some(options);
                    state.requested_options = Some(options);
                }
                state.pending = None;
                state.history_error = None;
                if word == "analytics-invalid" {
                    state.interval = "2".into();
                    state.error =
                        Some("Enter a whole sample interval from 5 to 3,600 seconds.".into());
                }
            }
            _ => return false,
        }
        true
    }

    pub(super) fn video_analytics_input(&mut self, input: Input) {
        let state = &mut self.video_analytics;
        match input {
            Input::Provider(provider) => {
                state.edited();
                state.preferences.model = ModelConfig {
                    provider,
                    model: provider.default_model().into(),
                };
            }
            Input::Model(model) => {
                state.edited();
                state.preferences.model.model = model;
            }
            Input::Interval(value) => {
                state.edited();
                state.interval = value;
                if let Some(value) = state.interval_value() {
                    state.preferences.interval_seconds = value;
                }
            }
            Input::Measurement(index, field, value) => {
                state.edited();
                if let Some(measurement) = state.preferences.measurements.get_mut(index) {
                    match field {
                        Field::Label => measurement.label = value,
                        Field::Prompt => measurement.prompt = value,
                        Field::Unit => measurement.unit = value,
                    }
                }
            }
            Input::Add => {
                if state.preferences.measurements.len() < MAX_MEASUREMENTS {
                    state.edited();
                    static NEXT: AtomicU64 = AtomicU64::new(0);
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos();
                    state.preferences.measurements.push(MeasurementDefinition {
                        id: format!("measurement-{now}-{}", NEXT.fetch_add(1, Ordering::Relaxed)),
                        label: String::new(),
                        prompt: String::new(),
                        unit: String::new(),
                    });
                }
            }
            Input::Remove(index) => {
                if index < state.preferences.measurements.len() {
                    state.edited();
                    state.preferences.measurements.remove(index);
                }
            }
            Input::Start => {
                let result = self
                    .snapshot
                    .active_camera
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("Select a connected camera first."))
                    .and_then(|camera_id| state.config(camera_id))
                    .and_then(|config| {
                        let camera_id = config.camera_id.clone();
                        state.runner.start(config)?;
                        state.running_camera = Some(camera_id);
                        state.paused_for_edit = false;
                        Ok(())
                    });
                state.error = result.err().map(|error| format!("{error:#}"));
            }
            Input::Stop => {
                state.runner.stop();
                state.paused_for_edit = false;
                state.error = None;
            }
            Input::CredentialProvider(provider) => {
                if state.credential_pending.is_none() {
                    state.credential_provider = provider;
                    state.clear_secret();
                    state.credential_status = None;
                    state.refresh_credentials();
                }
            }
            Input::Secret(secret) => {
                if state.credential_pending.is_none() {
                    state.secret = secret;
                }
            }
            Input::SaveKey => {
                if state.credential_pending.is_none() && !state.secret.0.trim().is_empty() {
                    let secret = std::mem::take(&mut state.secret);
                    state.credentials(CredentialAction::Save(secret));
                }
            }
            Input::DeleteKey => {
                state.clear_secret();
                state.credentials(CredentialAction::Delete);
            }
            Input::RefreshKey => state.refresh_credentials(),
        }
    }

    pub(super) fn tick_video_analytics(&mut self) {
        let options = self.analytics_svg_options();
        let state = &mut self.video_analytics;
        if let Some(pending) = &state.credential_pending {
            match pending.try_recv() {
                Ok(result) => {
                    state.credential_pending = None;
                    match result {
                        Ok((status, notice)) => {
                            state.credential_status = Some(status);
                            state.credential_notice = notice;
                            state.credential_error = None;
                        }
                        Err(error) => state.credential_error = Some(error),
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    state.credential_pending = None;
                    state.credential_error = Some("The credential worker stopped.");
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(pending) = &state.pending {
            match pending.try_recv() {
                Ok(result) => {
                    state.pending = None;
                    match result {
                        Ok((samples, chart, rendered_options)) => {
                            state.samples = samples;
                            if rendered_options == options {
                                state.chart = Some(svg::Handle::from_memory(chart.into_bytes()));
                                state.chart_options = Some(rendered_options);
                            }
                            state.history_error = None;
                        }
                        Err(error) => {
                            state.history_error =
                                Some(format!("Could not load measurement history: {error}"))
                        }
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    state.pending = None;
                    state.history_error = Some("The measurement history worker stopped.".into());
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if !self.analytics.open || state.pending.is_some() {
            return;
        }
        let query_due = !state
            .refreshed
            .is_some_and(|at| self.now.saturating_duration_since(at) < REFRESH);
        let render_due = state.requested_options != Some(options) && !state.samples.is_empty();
        if !query_due && !render_due {
            return;
        }
        let cached = if query_due {
            state.refreshed = Some(self.now);
            None
        } else {
            Some(state.samples.clone())
        };
        state.requested_options = Some(options);
        let handle = self.handle.clone();
        let (sender, receiver) = mpsc::channel();
        match std::thread::Builder::new()
            .name("capturefab-measurements-view".into())
            .spawn(move || {
                let report = (|| -> Result<_> {
                    let samples = match cached {
                        Some(samples) => samples,
                        None => handle.analytics().measurement_history()?,
                    };
                    let chart =
                        crate::analytics::render_measurement_svg_with_options(&samples, options)?;
                    Ok((samples, chart, options))
                })()
                .map_err(|error| format!("{error:#}"));
                let _ = sender.send(report);
            }) {
            Ok(_) => state.pending = Some(receiver),
            Err(error) => {
                state.history_error = Some(format!("Could not load measurement history: {error}"))
            }
        }
    }

    pub(super) fn api_credentials_content(&self, p: &'static Palette) -> Element<'_, Message> {
        let state = &self.video_analytics;
        let ready = state.credential_pending.is_none();
        let provider = pick_list(Provider::ALL, Some(state.credential_provider), |value| {
            Message::VideoAnalytics(Input::CredentialProvider(value))
        })
        .name("API key provider")
        .enabled(ready)
        .width(Fill)
        .padding([6, 10])
        .text_size(style::BODY)
        .style(style::pick);
        let input = text_input("API key", &state.secret.0)
            .name(format!("{} API key", state.credential_provider))
            .description(
                "Stored securely in the OS credential store. Never saved in app preferences.",
            )
            .secure(true)
            .on_input_maybe(ready.then_some(|value| {
                Message::VideoAnalytics(Input::Secret(SecretDraft(Zeroizing::new(value))))
            }))
            .on_submit_maybe(ready.then_some(Message::VideoAnalytics(Input::SaveKey)))
            .padding([6, 10])
            .size(style::BODY)
            .style(style::input);
        let save = button(text("Save key").size(style::BODY))
            .name("Save API key in the OS credential store")
            .padding([6, 12])
            .style(style::secondary)
            .on_press_maybe(
                (ready && !state.secret.0.trim().is_empty())
                    .then_some(Message::VideoAnalytics(Input::SaveKey)),
            );
        let remove = button(text("Remove saved key").size(style::BODY))
            .padding([6, 12])
            .style(style::secondary)
            .on_press_maybe(ready.then_some(Message::VideoAnalytics(Input::DeleteKey)));
        let refresh = button(text("Refresh").size(style::BODY))
            .name("Refresh API credential status")
            .padding([6, 12])
            .style(style::secondary)
            .on_press_maybe(ready.then_some(Message::VideoAnalytics(Input::RefreshKey)));
        let status = if ready {
            state
                .credential_status
                .as_deref()
                .unwrap_or("Credential status has not been checked.")
        } else {
            "Accessing OS credential store…"
        };
        let mut content = column![
            heading("AI provider API keys", p),
            provider,
            text(format!("Environment: {}. Environment values take precedence over a saved key.", state.credential_provider.env_names().join(" or "))).size(style::SMALL).color(p.secondary),
            text(status).size(style::SMALL).color(p.secondary),
            input,
            row![save, remove, refresh].spacing(8),
            text("Keys entered here are stored in your OS native credential store. Configure the model and measurements in Session analytics.")
                .size(style::SMALL).color(p.secondary),
        ].spacing(9);
        if let Some(notice) = state.credential_notice {
            content = content.push(text(notice).size(style::SMALL).color(p.secondary));
        }
        if let Some(error) = state.credential_error {
            content = content.push(field_error(error, p));
        }
        content.into()
    }

    pub(super) fn video_analytics_content(&self, p: &'static Palette) -> Element<'_, Message> {
        let state = &self.video_analytics;
        let status = state.runner.status();
        let provider = pick_list(
            Provider::ALL,
            Some(state.preferences.model.provider),
            |value| Message::VideoAnalytics(Input::Provider(value)),
        )
        .name("Video analytics provider")
        .width(Length::FillPortion(1))
        .padding([6, 10])
        .text_size(style::BODY)
        .style(style::pick);
        let model = text_input("Model ID", &state.preferences.model.model)
            .name("Video analytics model ID")
            .on_input(|value| Message::VideoAnalytics(Input::Model(value)))
            .width(Length::FillPortion(2))
            .padding([6, 10])
            .size(style::BODY)
            .style(style::input);
        let interval_valid = state.interval_value().is_some();
        let interval = text_input("30", &state.interval)
            .name("Video analytics sample interval in seconds")
            .description("One sampled image every 5 to 3,600 seconds.")
            .invalid(
                (!interval_valid).then(|| "Enter a whole interval from 5 to 3,600 seconds.".into()),
            )
            .on_input(|value| Message::VideoAnalytics(Input::Interval(value)))
            .width(96)
            .padding([6, 10])
            .size(style::BODY)
            .style(if interval_valid {
                style::input
            } else {
                style::input_invalid
            });
        let mut content = column![
            heading("Prompt measurements", p),
            text("Describe the numbers to track in sampled images. Results, confidence and notes are saved in this session.")
                .size(style::SMALL).color(p.secondary),
            row![
                column![text("Provider").size(style::SMALL).color(p.secondary), provider].spacing(6).width(Length::FillPortion(1)),
                column![text("Model").size(style::SMALL).color(p.secondary), model].spacing(6).width(Length::FillPortion(2)),
            ].spacing(10),
            row![text("Sample every").size(style::BODY), interval, text("seconds").size(style::BODY)].spacing(10).align_y(Alignment::Center),
        ].spacing(12);
        for (index, measurement) in state.preferences.measurements.iter().enumerate() {
            let label = text_input("Measurement name", &measurement.label)
                .name(format!("Measurement {} name", index + 1))
                .on_input(move |value| {
                    Message::VideoAnalytics(Input::Measurement(index, Field::Label, value))
                })
                .width(Length::FillPortion(2))
                .padding([6, 10])
                .size(style::BODY)
                .style(style::input);
            let unit = text_input("Unit, e.g. meters", &measurement.unit)
                .name(format!("Measurement {} unit", index + 1))
                .on_input(move |value| {
                    Message::VideoAnalytics(Input::Measurement(index, Field::Unit, value))
                })
                .width(Length::FillPortion(1))
                .padding([6, 10])
                .size(style::BODY)
                .style(style::input);
            let prompt = text_input("Count the number of tubes", &measurement.prompt)
                .name(format!("Measurement {} prompt", index + 1))
                .on_input(move |value| {
                    Message::VideoAnalytics(Input::Measurement(index, Field::Prompt, value))
                })
                .padding([6, 10])
                .size(style::BODY)
                .style(style::input);
            let remove = button(text("Remove").size(style::BODY))
                .name(format!("Remove measurement {}", index + 1))
                .on_press(Message::VideoAnalytics(Input::Remove(index)))
                .padding([6, 10])
                .style(style::secondary);
            content = content.push(
                container(
                    column![
                        row![
                            column![text("Name").size(style::SMALL).color(p.secondary), label]
                                .spacing(6)
                                .width(Length::FillPortion(2)),
                            column![text("Unit").size(style::SMALL).color(p.secondary), unit]
                                .spacing(6)
                                .width(Length::FillPortion(1)),
                            remove,
                        ]
                        .spacing(8)
                        .align_y(Alignment::End),
                        column![text("Prompt").size(style::SMALL).color(p.secondary), prompt]
                            .spacing(6),
                    ]
                    .spacing(8),
                )
                .padding(12)
                .width(Fill)
                .style(style::code),
            );
        }
        content = content.push(
            button(text("Add measurement").size(style::BODY))
                .on_press_maybe(
                    (state.preferences.measurements.len() < MAX_MEASUREMENTS)
                        .then_some(Message::VideoAnalytics(Input::Add)),
                )
                .padding([6, 12])
                .style(style::secondary),
        );
        content = content.push(text("For distances or lengths, include scale or calibration details in the prompt. Values are estimates; unsupported measurements are recorded as unknown.")
            .size(style::SMALL).color(p.secondary));
        let camera = self.snapshot.active_camera.as_deref();
        let target = if status.running {
            state.running_camera.as_deref().unwrap_or("selected camera")
        } else {
            camera.unwrap_or("Select a connected camera")
        };
        content = content.push(
            text(format!("Camera: {target}"))
                .size(style::BODY)
                .font(style::MEDIUM)
                .wrapping(text::Wrapping::WordOrGlyph),
        );
        content = content.push(text(format!("Starting collection sends sampled camera images and your prompts to {}. API usage may incur charges. Collection pauses when you edit the configuration and never starts automatically.", state.preferences.model.provider))
            .size(style::SMALL).color(p.secondary));
        let action = if status.running {
            button(text("Stop collection").size(style::BODY))
                .on_press(Message::VideoAnalytics(Input::Stop))
                .padding([7, 14])
                .style(style::secondary)
        } else {
            button(text("Start collection").size(style::BODY))
                .on_press_maybe(
                    (camera.is_some() && !status.busy && interval_valid)
                        .then_some(Message::VideoAnalytics(Input::Start)),
                )
                .padding([7, 14])
                .style(style::primary)
        };
        let progress = if status.running && status.busy {
            "Measuring sampled image…"
        } else if status.running {
            "Collecting"
        } else if status.busy {
            "Finishing cancelled request…"
        } else {
            "Stopped"
        };
        content = content.push(
            row![
                action,
                text(format!(
                    "{progress} · {} measurements",
                    status.completed_samples
                ))
                .size(style::SMALL)
                .color(p.secondary)
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        );
        if let Some(error) = state.error.as_ref().or(status.error.as_ref()) {
            content = content.push(field_error(error.clone(), p));
        }
        if state.paused_for_edit {
            content = content.push(text("Collection paused because its configuration changed. Start again to apply your edits.").size(style::SMALL).color(p.secondary));
        }
        if !interval_valid {
            content = content.push(field_error(
                "Enter a whole sample interval from 5 to 3,600 seconds.",
                p,
            ));
        }
        content = content.push(
            text("Configure API keys in Settings.")
                .size(style::SMALL)
                .color(p.secondary),
        );
        if let Some(error) = &state.history_error {
            content = content.push(field_error(error.clone(), p));
        }
        if !state.samples.is_empty() {
            content = content.push(heading("Measurement history", p));
            if let Some(chart) = &state.chart
                && state.chart_options == Some(self.analytics_svg_options())
            {
                content = content.push(svg(chart.clone()).width(Fill));
            } else {
                content = content.push(
                    text("Updating measurement chart…")
                        .size(style::SMALL)
                        .color(p.secondary),
                );
            }
            content = content.push(
                text("Latest measurements across cameras · current session")
                    .size(style::SMALL)
                    .color(p.secondary),
            );
            for sample in state.samples.iter().rev().take(12) {
                let value = sample.result.value.map_or_else(
                    || "Unknown".into(),
                    |value| {
                        format!("{value:.3} {}", sample.definition.unit)
                            .trim_end()
                            .to_owned()
                    },
                );
                let confidence = sample
                    .result
                    .confidence
                    .map_or_else(String::new, |confidence| {
                        format!(" · confidence {:.0}%", confidence * 100.0)
                    });
                content = content.push(
                    column![
                        row![
                            text(sample.definition.label.clone())
                                .size(style::BODY)
                                .font(style::MEDIUM)
                                .width(Fill)
                                .wrapping(text::Wrapping::WordOrGlyph),
                            text(value)
                                .size(style::BODY)
                                .font(style::MEDIUM)
                                .wrapping(text::Wrapping::WordOrGlyph),
                        ]
                        .spacing(16)
                        .align_y(Alignment::Start),
                        text(format!(
                            "{} · {} · {} / {}{}",
                            super::analytics::measurement_timestamp(sample.timestamp_ms),
                            sample.camera_id,
                            sample.provider,
                            sample.model,
                            confidence
                        ))
                        .size(style::SMALL)
                        .color(p.secondary)
                        .wrapping(text::Wrapping::WordOrGlyph),
                        text(sample.result.note.clone())
                            .size(style::SMALL)
                            .color(p.secondary)
                            .wrapping(text::Wrapping::WordOrGlyph),
                    ]
                    .spacing(4),
                );
            }
        } else {
            content = content.push(
                text("Start collection to build a chart and measurement history.")
                    .size(style::SMALL)
                    .color(p.secondary),
            );
        }
        content.into()
    }
}

#[cfg(test)]
mod layout_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measurement_chart_reflows_cached_history_when_width_or_appearance_changes() {
        let mut bench = Workbench::new(
            SessionHandle::new(),
            "measurement-reflow".into(),
            true,
            None,
        );
        bench.width = 1100.0;
        bench.scene_video_analytics("analytics-measurements-populated", true);
        bench.video_analytics.refreshed = Some(bench.now);
        let retained = bench.video_analytics.samples.len();
        let previous = bench.video_analytics.chart_options.unwrap();
        bench.width = 700.0;
        bench.appearance = Appearance::Dark;
        let current = bench.analytics_svg_options();
        assert_ne!(previous, current);
        // A reply rendered for the previous view must not overwrite this one.
        let (sender, receiver) = mpsc::channel();
        sender
            .send(Ok((
                bench.video_analytics.samples.clone(),
                "old chart".into(),
                previous,
            )))
            .unwrap();
        bench.video_analytics.pending = Some(receiver);
        bench.tick_video_analytics();
        assert_ne!(bench.video_analytics.chart_options, Some(current));
        assert!(
            bench.video_analytics.pending.is_some(),
            "the cached history is rerendered immediately"
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while bench.video_analytics.chart_options != Some(current) {
            assert!(
                Instant::now() < deadline,
                "measurement chart worker finished"
            );
            std::thread::sleep(Duration::from_millis(2));
            bench.tick_video_analytics();
        }
        assert_eq!(bench.video_analytics.samples.len(), retained);
        assert!(
            bench
                .handle
                .analytics()
                .measurement_history()
                .unwrap()
                .is_empty(),
            "rerendering retains view history without writing the session database"
        );
        assert!(!bench.video_analytics.runner.status().running);
    }

    #[test]
    fn ui_review_measurements_select_their_tab_without_starting_collection_or_storing_data() {
        let mut bench = Workbench::new(SessionHandle::new(), "ui-review-test".into(), true, None);
        assert!(bench.scene_video_analytics("analytics-measurements", false));
        assert!(bench.analytics.open);
        assert_eq!(bench.analytics.tab, analytics::Tab::Measurements);
        assert_eq!(bench.video_analytics.preferences.measurements.len(), 1);
        assert!(bench.video_analytics.samples.is_empty());
        assert!(!bench.scene_video_analytics("analytics-measurements-populated", false));
        assert!(bench.scene_video_analytics("analytics-measurements-populated", true));
        assert_eq!(bench.video_analytics.preferences.measurements.len(), 2);
        assert_eq!(bench.video_analytics.samples.len(), 6);
        assert!(bench.video_analytics.chart.is_some());
        assert!(!bench.video_analytics.runner.status().running);
        assert!(!bench.video_analytics.runner.status().busy);
        assert!(bench.video_analytics.pending.is_none());
        assert!(
            bench
                .handle
                .analytics()
                .measurement_history()
                .unwrap()
                .is_empty()
        );
        assert!(bench.handle.snapshot().cameras.is_empty());
    }

    #[test]
    fn ui_review_credentials_remain_display_only_without_a_credential_worker() {
        let mut bench = Workbench::new(SessionHandle::new(), "ui-review-test".into(), true, None);
        assert!(!bench.scene_video_analytics("settings-credential-error", false));
        assert!(bench.scene_video_analytics("settings-credential-error", true));
        assert!(bench.settings_open);
        assert_eq!(bench.video_analytics.credential_provider, Provider::Gemini);
        assert!(bench.video_analytics.credential_pending.is_none());
        assert_eq!(
            bench.video_analytics.credential_error,
            Some("The OS credential store could not save this key.")
        );
        assert_eq!(
            bench.video_analytics.secret.0.as_str(),
            "example-ui-review-key"
        );
        assert!(!bench.video_analytics.runner.status().running);
    }

    #[test]
    fn secrets_are_redacted_from_messages_and_excluded_from_preferences() {
        let mut state = State::new(SessionHandle::new(), Preferences::default());
        state.secret = SecretDraft(Zeroizing::new("private-key-must-not-appear".into()));
        let message = Message::VideoAnalytics(Input::Secret(state.secret.clone()));
        let serialized = serde_json::to_string(&state.preferences).unwrap();
        assert!(!serialized.contains("private-key"));
        assert!(!format!("{message:?}").contains("private-key"));
        assert!(format!("{message:?}").contains("[redacted]"));
        assert!(local_history::gesture(&message).is_none());
        state.clear_secret();
        assert!(state.secret.0.is_empty());
    }

    #[test]
    fn restoring_measurements_does_not_start_collection() {
        let preferences = Preferences {
            interval_seconds: 45,
            ..Preferences::default()
        };
        let state = State::new(SessionHandle::new(), preferences.clone());
        assert_eq!(state.preferences, preferences);
        assert!(!state.runner.status().running);
        assert!(state.running_camera.is_none());
        assert!(state.credential_pending.is_none());
    }

    #[test]
    fn invalid_interval_draft_does_not_change_saved_interval() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), false, None);
        bench.video_analytics_input(Input::Interval("0".into()));
        assert!(bench.video_analytics.interval_value().is_none());
        assert_eq!(bench.prefs().video_analytics.interval_seconds, 30);
        bench.video_analytics_input(Input::Interval("60".into()));
        assert_eq!(bench.prefs().video_analytics.interval_seconds, 60);
    }

    #[test]
    fn credential_operations_run_on_a_worker_thread() {
        let ui_thread = std::thread::current().id();
        let receiver = spawn_credentials(move || {
            assert_ne!(std::thread::current().id(), ui_thread);
            assert_eq!(
                std::thread::current().name(),
                Some("capturefab-api-credentials")
            );
            Ok(("Missing".into(), None))
        })
        .unwrap();
        assert_eq!(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap()
                .0,
            "Missing"
        );
    }

    #[test]
    fn measurement_configuration_roundtrips_without_runtime_state() {
        let preferences = Preferences::default();
        let json = serde_json::to_string(&preferences).unwrap();
        let restored: Preferences = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, preferences);
        assert!(!json.contains("running"));
        assert!(!json.contains("secret"));
        assert!(!json.contains("api_key"));
    }

    #[test]
    fn closing_settings_erases_unsaved_key_and_keys_never_enter_app_preferences() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), false, None);
        bench.video_analytics_input(Input::Secret(SecretDraft(Zeroizing::new(
            "unsaved-secret-key".into(),
        ))));
        assert!(
            !serde_json::to_string(&bench.prefs())
                .unwrap()
                .contains("unsaved-secret-key")
        );
        let _ = bench.handle_message(Message::Settings(false));
        assert!(bench.video_analytics.secret.0.is_empty());
        assert!(bench.history.undo().is_none());
    }
}
