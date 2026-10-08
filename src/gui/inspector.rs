//! The settings panel: features, auto exposure, capture, forwarding and storage.
use super::*;
use iced::widget::{Row, column};
use motion::Flashes;
use std::hash::{Hash, Hasher};

/// The width of every control in the panel's forms and feature rows, so
/// their edges line up.
const CONTROL: f32 = 136.0;
/// Room between the panel's sections.
const SECTION_GAP: f32 = 20.0;
/// Room between the rows of a section.
const ROW_GAP: f32 = 12.0;
/// How long a segmented control's thumb takes to slide one segment.
const THUMB: Duration = Duration::from_millis(180);
/// How long a field glows once the camera has taken its value.
const ACCEPTED: Duration = Duration::from_millis(450);
/// How long the camera context row glows as the selection moves to it.
const CONTEXT: Duration = Duration::from_millis(280);
/// The panel's scrollable, for jumping back to its top.
pub(super) const SCROLL: &str = "inspector";
/// The feature groups, in the order they show.
const GROUPS: [&str; 5] = ["Image", "Acquisition", "Device", "Transport", "Other"];

/// The inspector package's own state. Add fields here, register their motions
/// below and point them in `sync_inspector`.
pub(super) struct InspectorState {
    /// The last write or command that failed on each feature, as the whole
    /// error; show its first line at the row. Kept by `failed()` and cleared
    /// by the feature's next success, a new draft, a refresh or another
    /// camera.
    pub(super) write_errors: HashMap<String, String>,
    /// Feature writes the camera took, glowing on their fields for a moment.
    accepted: Flashes<String>,
    /// What each feature's field held when its write went out. The reply
    /// replaces that draft with the value the camera stored, unless it was
    /// edited in the meantime.
    sent: HashMap<String, Option<String>>,
    /// The tab thumb's place: a tab's index.
    tab_thumb: Motion,
    /// The Manual/Auto thumb's place: 1 for auto. It moves as soon as a
    /// switch is asked for, and back should the camera refuse.
    mode_thumb: Motion,
    /// Whether the thumbs have been placed; until then they jump.
    placed: bool,
    /// The camera the panel last showed.
    shown_camera: Option<String>,
    /// The camera context row lighting up as the selection moves to it.
    context: Motion,
    /// How open the storage and auto changes disclosures are.
    storage_turn: Motion,
    changes_turn: Motion,
    /// The features grouped and ordered for display.
    index: FeatureIndex,
    /// The output a forward was started with from here, and a summary of its
    /// settings.
    forwarded: Option<(String, String)>,
    /// The destination the last capture went to, when one was named.
    saved_to: Option<String>,
    /// Screenshot scenes' commands, which never finish.
    held: Vec<mpsc::Sender<anyhow::Result<serde_json::Value>>>,
}

impl Default for InspectorState {
    fn default() -> Self {
        Self {
            write_errors: HashMap::new(),
            accepted: Flashes::new(ACCEPTED),
            sent: HashMap::new(),
            tab_thumb: Motion::new(0.0, THUMB, THUMB, Kind::Move),
            mode_thumb: Motion::new(0.0, THUMB, THUMB, Kind::Move),
            placed: false,
            shown_camera: None,
            context: Motion::new(0.0, CONTEXT, CONTEXT, Kind::Fade),
            storage_turn: Motion::new(0.0, motion::RING, motion::RING_OUT, Kind::Move),
            changes_turn: Motion::new(0.0, motion::RING, motion::RING_OUT, Kind::Move),
            index: FeatureIndex::default(),
            forwarded: None,
            saved_to: None,
            held: Vec::new(),
        }
    }
}

impl InspectorState {
    super::motion::registry! {
        motions: [tab_thumb, mode_thumb, context, storage_turn, changes_turn],
        flashes: [accepted],
    }
}

/// The features as the panel lists them: grouped, in setup order, with
/// their labels and search text. Built again only when the names change, so
/// the view, which runs on every camera frame, does no per-row sorting,
/// grouping or lowercasing.
#[derive(Default)]
struct FeatureIndex {
    /// The names it was built from, hashed.
    key: u64,
    /// How many features it was built from.
    len: usize,
    /// Each group with its features' indices, in display order.
    groups: Vec<(&'static str, Vec<usize>)>,
    /// Each feature's label and tooltip.
    labels: Vec<String>,
    tips: Vec<String>,
    /// What a search matches for each feature: its name, label and
    /// description, lowercased.
    words: Vec<String>,
    /// How many features are listed; AcquisitionStop folds into the Stream row.
    listed: usize,
}

impl FeatureIndex {
    fn key(features: &[FeatureInfo]) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for feature in features {
            feature.name.hash(&mut hasher);
        }
        hasher.finish()
    }

    fn new(features: &[FeatureInfo]) -> Self {
        // One Stream row starts and stops, so AcquisitionStop goes.
        let folded = features.iter().any(|f| f.name == "AcquisitionStart");
        let mut groups = Vec::new();
        let mut listed = 0;
        for group in GROUPS {
            let mut members: Vec<usize> = (0..features.len())
                .filter(|&i| {
                    let name = features[i].name.as_str();
                    feature_group(name) == group && !(folded && name == "AcquisitionStop")
                })
                .collect();
            // Stable, so ties keep the camera's order.
            members.sort_by_key(|&i| feature_rank(&features[i].name));
            listed += members.len();
            if !members.is_empty() {
                groups.push((group, members));
            }
        }
        let labels: Vec<String> = features
            .iter()
            .map(|f| {
                if folded && f.name == "AcquisitionStart" {
                    "Stream".into()
                } else if f.display_name.is_empty() || f.display_name == f.name {
                    words(&f.name)
                } else {
                    f.display_name.clone()
                }
            })
            .collect();
        let tips = features
            .iter()
            .map(|f| {
                if f.description.is_empty() {
                    f.name.clone()
                } else {
                    format!("{}\n{}", f.name, f.description)
                }
            })
            .collect();
        let words = features
            .iter()
            .zip(&labels)
            .map(|(f, label)| format!("{}\n{label}\n{}", f.name, f.description).to_lowercase())
            .collect();
        Self {
            key: Self::key(features),
            len: features.len(),
            groups,
            labels,
            tips,
            words,
            listed,
        }
    }
}

/// How a text field shows where its edit stands.
#[derive(Clone, Copy)]
enum Edit {
    /// The camera refused it.
    Refused,
    /// Typed but not applied.
    Dirty,
    /// At rest, glowing by the amount while the camera's acceptance fades.
    Rest(f32),
}

/// The inspector package's hooks into the shared update cycle.
impl Workbench {
    /// Point the inspector package's motions at what they show; from
    /// `sync_animations`.
    pub(super) fn sync_inspector(&mut self) {
        let now = self.now;
        // The tick checks the names; a new length cannot wait for it.
        if self.inspect.index.len != self.snapshot.features.len() {
            self.inspect.index = FeatureIndex::new(&self.snapshot.features);
        }
        let tab = self.tab as usize as f32;
        let mode = self.mode_target();
        let moved = self.inspect.shown_camera.as_deref() != self.snapshot.active_camera.as_deref();
        let inspect = &mut self.inspect;
        if moved {
            // Light up the context row as another camera takes the panel,
            // but not for the first one.
            if inspect.shown_camera.is_some() && self.snapshot.cameras.len() > 1 {
                inspect.context.replay(1.0, 0.0, now);
            }
            inspect.shown_camera = self.snapshot.active_camera.clone();
        }
        // Another camera's mode is no change of mode, so the thumb jumps.
        if !inspect.placed || moved {
            inspect.mode_thumb.set(mode);
        } else {
            inspect.mode_thumb.go(mode, now);
        }
        if inspect.placed {
            inspect.tab_thumb.go(tab, now);
        } else {
            inspect.tab_thumb.set(tab);
            inspect.placed = true;
        }
        inspect.storage_turn.show(self.storage_open, now);
        inspect.changes_turn.show(self.auto_changes_open, now);
        // Note what each field held as its write went out.
        for pending in &self.pending {
            if let Some(feature) = &pending.feature
                && pending.label.starts_with("Setting ")
                && !inspect.sent.contains_key(feature)
            {
                inspect
                    .sent
                    .insert(feature.clone(), self.edits.get(feature).cloned());
            }
        }
    }

    /// The inspector package's bookkeeping on the slow tick, after the snapshot
    /// refresh; from `tick()`.
    pub(super) fn tick_inspector(&mut self) {
        let features = &self.snapshot.features;
        if self.inspect.index.len != features.len()
            || self.inspect.index.key != FeatureIndex::key(features)
        {
            self.inspect.index = FeatureIndex::new(features);
        }
        if self.snapshot.forwarding.is_none() {
            self.inspect.forwarded = None;
        }
    }

    /// A command finished, after the shared bookkeeping (`finished`,
    /// `failed`) and before its notice; from `settle()`.
    pub(super) fn result_inspector(
        &mut self,
        pending: &Pending,
        result: &anyhow::Result<serde_json::Value>,
    ) {
        match (pending.label.as_str(), result) {
            ("Saving capture", Ok(value)) => {
                self.inspect.saved_to = value["destination"].as_str().map(str::to_owned);
            }
            ("Starting forwarding", Ok(value)) => {
                self.inspect.forwarded = value["forwarding"]
                    .as_str()
                    .map(|output| (output.to_owned(), self.forward_summary()));
            }
            _ => {}
        }
        let Some(feature) = &pending.feature else {
            return;
        };
        if !pending.label.starts_with("Setting ") {
            return;
        }
        let sent = self.inspect.sent.remove(feature);
        let Ok(value) = result else {
            return;
        };
        // Show what the camera stored, which may be rounded or clamped,
        // unless the field was edited since.
        if let Some(stored) = value.get("value")
            && let Some(draft) = self.edits.get_mut(feature)
            && sent.is_none_or(|sent| sent.as_ref() == Some(draft))
        {
            *draft = value_text(stored);
        }
        self.inspect.accepted.hit(feature.clone(), self.now);
    }

    /// Take a screenshot scene word the inspector package owns: `late` is false
    /// while the scene is set up and true once its cameras stream. Returns
    /// whether the word was taken; see `apply_scene`. Words:
    /// `inspector-search` and `inspector-nomatch` (a search with and without
    /// matches), `inspector-storage` (storage settings open),
    /// `inspector-noforward` (no forward output yet), `inspector-widest` (the
    /// longest choices in every form, all of it open), `inspector-context`
    /// (the camera context row lighting up), `inspector-dirty` (an
    /// exposure time typed, not applied), `inspector-applying` (and being
    /// applied), `inspector-accepted` (just taken by the camera),
    /// `inspector-switching` (auto mode on its way) and `inspector-connecting`
    /// (with `welcome`: a camera connecting).
    pub(super) fn scene_inspector(&mut self, word: &str, late: bool) -> bool {
        let camera = self.snapshot.active_camera.clone();
        let feature = "ExposureTime";
        match (word, late) {
            ("inspector-search", _) => self.search = "gain".into(),
            ("inspector-nomatch", _) => self.search = "white balance".into(),
            ("inspector-storage", _) => self.storage_open = true,
            ("inspector-noforward", _) => self.forward_output.clear(),
            ("inspector-widest", _) => {
                self.format = "pgm";
                self.forward_codec = "h265";
                self.quota_action = "delete-oldest";
                self.storage_open = true;
                self.schedule_enabled = true;
            }
            // These need streaming cameras.
            (
                "inspector-dirty"
                | "inspector-applying"
                | "inspector-accepted"
                | "inspector-switching"
                | "inspector-connecting"
                | "inspector-context",
                false,
            ) => return false,
            ("inspector-dirty", true) => {
                self.edits.insert(feature.into(), "15000".into());
            }
            ("inspector-applying", true) => {
                self.edits.insert(feature.into(), "15000".into());
                self.scene_hold(&format!("Setting {feature}"), camera, Some(feature));
            }
            ("inspector-accepted", true) => {
                self.inspect.accepted.hit(feature.into(), self.now);
            }
            ("inspector-switching", true) => {
                self.scene_hold("Enabling auto mode", camera, None);
                // Where it slides to, for a still.
                self.inspect.mode_thumb.set(1.0);
            }
            ("inspector-connecting", true) => self.scene_hold("Connecting camera", None, None),
            ("inspector-context", true) => self.inspect.context.replay(1.0, 0.0, self.now),
            _ => return false,
        }
        true
    }

    /// A command under `label` that never finishes, for a scene.
    fn scene_hold(&mut self, label: &str, camera: Option<String>, feature: Option<&str>) {
        let (sender, receiver) = mpsc::channel();
        self.inspect.held.push(sender);
        self.pending.push(Pending {
            label: label.into(),
            receiver,
            target: None,
            camera,
            feature: feature.map(str::to_owned),
            at: self.now,
            batch: false,
        });
    }

    /// Where the Manual/Auto thumb heads: where a switch on its way to the
    /// selected camera goes, else where the camera is.
    fn mode_target(&self) -> f32 {
        let camera = self.snapshot.active_camera.as_deref().unwrap_or_default();
        if self.pending_for("Enabling auto mode", camera) {
            1.0
        } else if self.pending_for("Switching to manual", camera) {
            0.0
        } else if self.snapshot.auto.is_some() {
            1.0
        } else {
            0.0
        }
    }

    /// The forward settings as sent: "H.264 · 30 fps · 4M".
    fn forward_summary(&self) -> String {
        let mut parts = vec![
            find(&CODECS, self.forward_codec)
                .map_or(self.forward_codec, |codec| codec.label)
                .to_owned(),
            format!("{} fps", number(self.forward_fps)),
        ];
        let bitrate = self.forward_bitrate.trim();
        if !bitrate.is_empty() {
            parts.push(bitrate.to_owned());
        }
        let encoder = self.forward_encoder.trim();
        if !encoder.is_empty() && encoder != "auto" {
            parts.push(encoder.to_owned());
        }
        parts.join(" · ")
    }
}

/// A section's title, with an action such as Refresh at the right.
fn section<'a>(
    title: &'a str,
    action: Option<Element<'a, Message>>,
    p: &'static Palette,
) -> Element<'a, Message> {
    let mut head = row![
        text(title)
            .size(style::CAPTION)
            .font(style::SEMIBOLD)
            .color(p.secondary)
    ]
    .align_y(Alignment::Center);
    if let Some(action) = action {
        head = head.push(space::horizontal()).push(action);
    }
    head.into()
}

/// A form row: the label, the control in the shared column, and its unit
/// in a slot kept even when empty, so every box shares both edges.
fn form_row<'a>(
    label: &'a str,
    control: impl Into<Element<'a, Message>>,
    unit_label: &'a str,
    p: &'static Palette,
) -> Element<'a, Message> {
    field(
        label,
        unit(container(control).width(CONTROL).into(), unit_label, p),
        p,
    )
}

/// What a schedule will do: "10 frames, one every 2.5 seconds".
fn schedule_text(count: u32, interval: f64) -> String {
    format!(
        "{} {}, one every {} {}",
        grouped(count.into()),
        if count == 1 { "frame" } else { "frames" },
        number(interval),
        if interval == 1.0 { "second" } else { "seconds" }
    )
}

/// A quiet line saying why the action above it cannot run.
fn reason<'a>(why: &'a str, p: &'static Palette) -> Element<'a, Message> {
    text(why).size(style::SMALL).color(p.secondary).into()
}

/// An accent text action.
fn link<'a>(label: &'a str, on: Option<Message>) -> Element<'a, Message> {
    button(text(label).size(style::SMALL))
        .padding([3, 8])
        .style(style::link)
        .on_press_maybe(on)
        .into()
}

/// A full-width action: `glyph` and `label` centered on a button styled
/// `base`. While `busy` it ignores presses but keeps its look.
fn wide_button<'a>(
    glyph: Element<'a, Message>,
    label: &'a str,
    base: fn(&Theme, button::Status) -> button::Style,
    on: Option<Message>,
    busy: bool,
) -> button::Button<'a, Message> {
    button(
        center(
            row![glyph, text(label).size(style::BODY).font(style::MEDIUM)]
                .spacing(8)
                .align_y(Alignment::Center),
        )
        .height(Length::Shrink),
    )
    .width(Fill)
    .padding([9, 14])
    .style(move |theme, status| base(theme, if busy { button::Status::Active } else { status }))
    .on_press_maybe(on.filter(|_| !busy))
}

impl Workbench {
    pub(super) fn inspector(&self, p: &'static Palette) -> Element<'_, Message> {
        let os = Os::CURRENT;
        let tabs = segmented(
            vec![
                (
                    "Features",
                    Some(Message::Tab(Tab::Features)),
                    Action::FeaturesTab.hint("Features panel", os),
                ),
                (
                    "Capture",
                    Some(Message::Tab(Tab::Capture)),
                    Action::CaptureTab.hint("Capture panel", os),
                ),
                (
                    "Forward",
                    Some(Message::Tab(Tab::Forward)),
                    Action::ForwardTab.hint("Forward panel", os),
                ),
            ],
            self.inspect.tab_thumb.get(self.now),
            Surface::Panel,
        );
        let mut panel = column![
            self.titlebar(
                container(tabs)
                    .padding(iced::Padding {
                        left: 18.0,
                        right: 20.0,
                        ..iced::Padding::ZERO
                    })
                    .into()
            )
        ];
        if let Some(context) = self.camera_context(p) {
            panel = panel.push(context);
        }
        let body: Element<'_, Message> =
            if self.tab == Tab::Features && self.snapshot.connected.is_none() {
                self.no_camera(p)
            } else {
                let content = match self.tab {
                    Tab::Features => self.features(p),
                    Tab::Capture => self.capture_settings(p),
                    Tab::Forward => self.forward_settings(p),
                };
                let mut scroll: Option<Element<'_, Message>> = Some(
                    scrollable(container(content).padding(iced::Padding {
                        top: 4.0,
                        right: 20.0,
                        bottom: 24.0,
                        left: 18.0,
                    }))
                    .id(SCROLL)
                    .height(Fill)
                    .style(style::scroll)
                    .into(),
                );
                // Each tab scrolls in a slot of its own, so changing tabs,
                // however it happens, starts the new one at its top.
                let mut slots = Row::new().height(Fill);
                for tab in [Tab::Features, Tab::Capture, Tab::Forward] {
                    slots = slots.push(match scroll.take_if(|_| tab == self.tab) {
                        Some(scroll) => scroll,
                        None => space().into(),
                    });
                }
                slots.into()
            };
        container(panel.push(space().height(8)).push(body))
            .width(INSPECTOR)
            .height(Fill)
            .style(style::base)
            .into()
    }

    /// With several cameras, which one the panel edits: pinned under the
    /// tabs, lighting up as the selection moves.
    fn camera_context(&self, p: &'static Palette) -> Option<Element<'_, Message>> {
        if self.snapshot.cameras.len() < 2 {
            return None;
        }
        let camera = self.snapshot.connected.as_ref()?;
        let glow = self.inspect.context.get(self.now);
        let line = row![
            icon(Icon::transport(camera.transport), 13.0, p.secondary),
            one_line(camera.model.as_str(), style::SMALL, style::MEDIUM, p.text),
            one_line(
                format!("S/N {}", camera.serial),
                style::SMALL,
                style::SANS,
                p.secondary
            )
            .width(Fill),
            dot(
                if self.snapshot.streaming {
                    p.live
                } else {
                    p.tertiary
                },
                6.0
            ),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        Some(
            container(
                container(line)
                    .padding([6, 10])
                    .width(Fill)
                    .style(move |_| container::Style {
                        background: Some(fade(p.accent_soft, glow).into()),
                        border: iced::border::rounded(style::RADIUS),
                        ..container::Style::default()
                    }),
            )
            .padding(iced::Padding {
                top: 6.0,
                right: 10.0,
                bottom: 0.0,
                left: 8.0,
            })
            .into(),
        )
    }

    /// The Features tab with no camera: what will show here, and what can
    /// be set up meanwhile.
    fn no_camera(&self, p: &'static Palette) -> Element<'_, Message> {
        let connecting = self.pending("Connecting camera");
        let content = if connecting {
            column![
                icon::spinner(22.0, p.secondary, self.spin()),
                text("Reading camera features…")
                    .size(style::BODY)
                    .color(p.secondary),
            ]
        } else {
            column![
                icon(Icon::Sliders, 28.0, p.tertiary),
                text("No camera connected")
                    .size(style::HEADING)
                    .font(style::SEMIBOLD),
                text("Features appear here once a camera is connected.")
                    .size(style::SMALL)
                    .color(p.secondary)
                    .align_x(Alignment::Center),
                row![
                    link("Set up capture", Some(Message::Tab(Tab::Capture))),
                    link("Set up forwarding", Some(Message::Tab(Tab::Forward))),
                ]
                .spacing(4),
            ]
        };
        center(
            content
                .spacing(10)
                .align_x(Alignment::Center)
                .max_width(240),
        )
        .padding(iced::Padding {
            bottom: 80.0,
            ..iced::Padding::ZERO
        })
        .into()
    }

    pub(super) fn features(&self, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        let index = &self.inspect.index;
        let query = self.search.trim().to_lowercase();
        let mut groups = column![].spacing(SECTION_GAP);
        let mut matched = 0;
        for (group, members) in &index.groups {
            let mut rows = column![].spacing(ROW_GAP);
            let mut shown = false;
            let mut locked = false;
            for &i in members {
                let Some(feature) = snapshot.features.get(i) else {
                    continue;
                };
                if !query.is_empty() && !index.words[i].contains(&query) {
                    continue;
                }
                shown = true;
                matched += 1;
                locked |= snapshot.streaming && stream_locked(&feature.name);
                rows = rows.push(self.feature(feature, &index.labels[i], &index.tips[i], p));
            }
            if shown {
                let banner = locked.then(|| self.locked_banner(p));
                groups = groups.push(column![section(group, banner, p), rows].spacing(10));
            }
        }
        let refreshing = self.pending("Refreshing features");
        let total = index.listed;
        let count = match (query.is_empty(), total) {
            (true, 1) => "1 feature".to_owned(),
            (true, _) => format!("{total} features"),
            (false, _) => format!("{matched} of {total} features"),
        };
        let count = row![
            text(count).size(style::SMALL).color(p.secondary),
            space::horizontal(),
            if refreshing {
                Element::from(
                    container(text("Refreshing…").size(style::SMALL).color(p.secondary))
                        .padding([3, 8]),
                )
            } else {
                link("Refresh", Some(Message::RefreshFeatures))
            },
        ]
        .align_y(Alignment::Center);
        let list: Element<'_, Message> = if matched == 0 && !query.is_empty() {
            column![
                text(format!("No features match “{}”", self.search.trim()))
                    .size(style::BODY)
                    .color(p.secondary)
                    .align_x(Alignment::Center),
                link("Clear search", Some(Message::Search(String::new()))),
            ]
            .spacing(4)
            .width(Fill)
            .align_x(Alignment::Center)
            .into()
        } else {
            groups.into()
        };
        column![
            self.auto_card(p),
            column![self.search_field(p), count].spacing(6),
            list,
        ]
        .spacing(SECTION_GAP)
        .into()
    }

    /// The feature search, with a button that clears it once it holds text.
    fn search_field(&self, p: &'static Palette) -> Element<'_, Message> {
        let input = text_input("Search features", &self.search)
            .id("feature-search")
            .icon(icon::input_icon(Icon::Search))
            .on_input(Message::Search)
            .size(style::BODY)
            // Room for the clear button, held even without it so the text
            // never reflows.
            .padding(iced::Padding {
                top: 7.0,
                right: 30.0,
                bottom: 7.0,
                left: 10.0,
            })
            .style(style::input);
        if self.search.is_empty() {
            return input.into();
        }
        stack![
            input,
            container(tip(
                button(icon(Icon::Close, 11.0, p.secondary))
                    .padding(5)
                    .style(style::plain)
                    .on_press(Message::Search(String::new())),
                "Clear search",
            ))
            .align_right(Fill)
            .center_y(Fill)
            .padding(iced::Padding {
                right: 5.0,
                ..iced::Padding::ZERO
            }),
        ]
        .into()
    }

    /// Said once per group that holds features the stream locks.
    fn locked_banner(&self, p: &'static Palette) -> Element<'_, Message> {
        let camera = self.snapshot.active_camera.as_deref().unwrap_or_default();
        let stopping = self.pending_for("Stopping stream", camera);
        row![
            icon(Icon::Lock, 11.0, p.secondary),
            text("Locked while streaming")
                .size(style::CAPTION)
                .color(p.secondary),
            tip(
                button(text("Stop").size(style::CAPTION))
                    .padding([1, 6])
                    .style(style::link)
                    .on_press_maybe((!stopping).then_some(Message::ToggleStream)),
                Action::ToggleStream.hint("Stop the stream", Os::CURRENT),
            ),
        ]
        .spacing(4)
        .align_y(Alignment::Center)
        .into()
    }

    pub(super) fn auto_card(&self, p: &'static Palette) -> Element<'_, Message> {
        let auto = self.snapshot.auto.as_ref();
        let busy = self.auto_busy();
        let os = Os::CURRENT;
        let camera = self.snapshot.active_camera.as_deref().unwrap_or_default();
        let switching = self.pending_for("Enabling auto mode", camera)
            || self.pending_for("Switching to manual", camera);
        let mut head = row![
            text("Exposure").size(style::BODY).font(style::SEMIBOLD),
            space::horizontal(),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        if switching {
            head = head
                .push(icon::spinner(10.0, p.secondary, self.spin()))
                .push(
                    text("Switching…")
                        .size(style::CAPTION)
                        .font(style::MEDIUM)
                        .color(p.secondary),
                );
        } else if let Some(status) = auto {
            head = head.push(
                text(capitalize(&status.state))
                    .size(style::CAPTION)
                    .font(style::MEDIUM)
                    .color(match status.state.as_str() {
                        "stable" => p.ink(p.live),
                        "limited" => p.ink(p.warn),
                        _ => p.secondary,
                    }),
            );
        }
        let modes = segmented(
            vec![
                (
                    "Manual",
                    (!busy).then_some(Message::Auto(false)),
                    Action::ToggleAuto.hint("Set exposure, gain and frame rate yourself", os),
                ),
                (
                    "Auto",
                    (!busy).then_some(Message::Auto(true)),
                    Action::ToggleAuto.hint("Tune exposure, gain and frame rate automatically", os),
                ),
            ],
            self.inspect.mode_thumb.get(self.now),
            Surface::Panel,
        );
        let mut card = column![head, modes].spacing(10);
        match auto {
            None => {
                card = card.push(
                    text("Auto tunes exposure, gain and frame rate for this camera.")
                        .size(style::SMALL)
                        .color(p.secondary),
                );
            }
            Some(status) => {
                card = card
                    .push(
                        column![
                            slider(0.0..=1.0, self.balance, Message::Balance)
                                .step(0.01)
                                .on_release(Message::BalanceReleased)
                                .style(style::slide),
                            row![
                                text("Quality").size(style::CAPTION).color(p.secondary),
                                space::horizontal(),
                                text("Frame rate").size(style::CAPTION).color(p.secondary),
                            ],
                        ]
                        .spacing(4),
                    )
                    .push(
                        text(auto_summary(status))
                            .size(style::SMALL)
                            .color(p.secondary),
                    );
                for note in &status.notes {
                    card = card.push(text(note.clone()).size(style::SMALL).color(p.ink(p.warn)));
                }
                if !status.changes.is_empty() {
                    card = card.push(disclosure(
                        format!("Auto changes ({})", status.changes.len()),
                        None,
                        self.inspect.changes_turn.get(self.now),
                        Message::ToggleAutoChanges,
                        p,
                    ));
                    if self.auto_changes_open {
                        let mut changes = column![].spacing(3);
                        for change in status.changes.iter().rev() {
                            let unit = self
                                .snapshot
                                .features
                                .iter()
                                .find(|f| f.name == change.feature)
                                .and_then(|f| f.unit.as_deref());
                            changes = changes.push(tip(
                                text(change_text(change, unit))
                                    .size(style::CAPTION)
                                    .font(style::MONO)
                                    .color(p.secondary),
                                change.reason.clone(),
                            ));
                        }
                        card = card.push(
                            scrollable(changes)
                                .height(Length::Shrink)
                                .style(style::scroll),
                        );
                    }
                }
            }
        }
        container(card)
            .padding(14)
            .width(Fill)
            .style(style::well)
            .into()
    }

    /// One feature: its label and control, and a caption under them with its
    /// range, where an edit stands, or why the camera refused it. Runs per
    /// row on every camera frame, so each check here is constant time and
    /// skips at once while nothing is pending, refused or glowing.
    pub(super) fn feature<'a>(
        &'a self,
        feature: &'a FeatureInfo,
        label: &'a str,
        about: &'a str,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let name = feature.name.as_str();
        let streaming = self.snapshot.streaming;
        let managed = self
            .snapshot
            .auto
            .as_ref()
            .is_some_and(|auto| auto.managed.iter().any(|m| m == name));
        let locked = streaming && stream_locked(name);
        let writable = (feature.writable || managed) && !locked;
        let kind = feature.kind.as_str();
        let command = kind.eq_ignore_ascii_case("command");
        let refused = if self.inspect.write_errors.is_empty() {
            None
        } else {
            self.inspect.write_errors.get(name)
        };
        let current = feature_value(feature);
        // The caption's left side, and whether this row holds a text field,
        // which keeps its caption line so nothing moves as an edit begins.
        let mut notes: Vec<Element<'a, Message>> = Vec::new();
        let note = |note: String, color: Color| -> Element<'a, Message> {
            text(note).size(style::CAPTION).color(color).into()
        };
        let mut trailing = None;
        let read_only = !writable && !command && !locked;
        let control: Element<'_, Message> = if command && name == "AcquisitionStart" {
            self.stream_toggle(p)
        } else if let Some(error) = feature.error.as_ref().filter(|_| !command) {
            notes.push(note(error.clone(), p.ink(p.warn)));
            space().into()
        } else if command {
            let (action, enabled) = match name {
                "AcquisitionStart" => ("Start stream", !streaming),
                "AcquisitionStop" => ("Stop stream", streaming),
                _ => ("Execute", true),
            };
            button(text(action).size(style::SMALL))
                .padding([4, 10])
                .style(style::secondary)
                .on_press_maybe(
                    (feature.writable && enabled).then(|| Message::Execute(feature.name.clone())),
                )
                .into()
        } else if !writable {
            let value = match feature.unit.as_deref() {
                Some(unit) => format!("{current} {}", unit_label(unit)),
                None => current.clone(),
            };
            let mut shown = row![].spacing(6).align_y(Alignment::Center);
            if locked {
                shown = shown.push(tip(
                    icon(Icon::Lock, 11.0, p.tertiary),
                    format!("Stop the stream to change {label}"),
                ));
            }
            // In line with the digits of the fields above and below.
            container(shown.push(text(value).size(style::SMALL).color(p.secondary)))
                .padding(iced::Padding {
                    right: 8.0,
                    ..iced::Padding::ZERO
                })
                .into()
        } else if kind.eq_ignore_ascii_case("boolean") || kind.eq_ignore_ascii_case("bool") {
            let checked = feature
                .value
                .as_ref()
                .and_then(|v| v.as_bool())
                .unwrap_or(current == "true" || current == "1");
            iced::widget::checkbox(checked)
                .on_toggle(move |value| Message::Set(feature.name.clone(), value.to_string()))
                .size(16)
                .style(style::check)
                .into()
        } else if !feature.choices.is_empty() {
            let glow = self.inspect.accepted.level(name, self.now);
            pick_list(
                &feature.choices[..],
                feature.choices.iter().find(|c| **c == current).cloned(),
                move |value| Message::Set(feature.name.clone(), value),
            )
            .placeholder(current.clone())
            .width(CONTROL)
            .text_size(style::SMALL)
            .padding([5, 8])
            .style(style::pick_flash(glow))
            .menu_style(style::menu)
            .into()
        } else {
            let draft = self.edits.get(name).unwrap_or(&current);
            let dirty = *draft != current;
            let applying = !self.pending.is_empty()
                && self.pending.iter().any(|pending| {
                    pending.feature.as_deref() == Some(name)
                        && pending.label.starts_with("Setting ")
                });
            let edit = if refused.is_some() {
                Edit::Refused
            } else if dirty {
                Edit::Dirty
            } else {
                Edit::Rest(self.inspect.accepted.level(name, self.now))
            };
            trailing = Some(if applying {
                text("Applying…")
                    .size(style::CAPTION)
                    .color(p.secondary)
                    .into()
            } else if dirty {
                tip(
                    button(text("Apply").size(style::CAPTION))
                        .padding([0, 4])
                        .style(style::link)
                        .on_press(Message::Commit(feature.name.clone())),
                    Action::FocusCamera.hint("Apply", Os::CURRENT),
                )
            } else {
                Element::from(space())
            });
            text_input(&current, draft)
                .on_input(move |value| Message::Draft(feature.name.clone(), value))
                .on_submit(Message::Commit(feature.name.clone()))
                .size(style::SMALL)
                .padding([5, 8])
                .width(CONTROL)
                .align_x(Alignment::End)
                .style(move |theme, status| match edit {
                    Edit::Refused => style::input_invalid(theme, status),
                    Edit::Dirty => style::input_dirty(theme, status),
                    Edit::Rest(glow) => style::input_flash(glow)(theme, status),
                })
                .into()
        };
        if managed && !locked {
            notes.push(tip(
                note("Set by auto".into(), p.accent_text),
                "Editing it switches the camera to manual",
            ));
        }
        if writable && !command {
            let range = range_text(feature);
            if !range.is_empty() {
                notes.push(note(range, p.secondary));
            }
        }
        let label = text(label).size(style::BODY);
        let label = if read_only {
            tip(label, format!("{about}\nRead only"))
        } else {
            tip(label, about)
        };
        let head = row![container(label).width(Fill), control,]
            .spacing(10)
            .align_y(Alignment::Center);
        let caption: Option<Element<'_, Message>> = if let Some(error) = refused {
            Some(field_error(
                error.lines().next().unwrap_or_default().to_owned(),
                p,
            ))
        } else if !notes.is_empty() || trailing.is_some() {
            let line = Row::with_children(notes).spacing(8).wrap();
            let mut caption = row![container(line).width(Fill)].align_y(Alignment::Start);
            if let Some(trailing) = trailing {
                caption = caption.push(trailing);
            }
            Some(caption.into())
        } else {
            None
        };
        match caption {
            Some(caption) => column![head, caption].spacing(2).into(),
            None => head.into(),
        }
    }

    /// The Stream row: starts or stops the selected camera, with a spinner
    /// while that is on its way.
    fn stream_toggle(&self, p: &'static Palette) -> Element<'_, Message> {
        let camera = self.snapshot.active_camera.as_deref().unwrap_or_default();
        let starting = self.pending_for("Starting stream", camera);
        let stopping = self.pending_for("Stopping stream", camera);
        let busy = starting || stopping;
        let streaming = self.snapshot.streaming;
        let glyph = if busy {
            icon::spinner(11.0, p.secondary, self.spin())
        } else if streaming {
            icon(Icon::Stop, 11.0, p.ink(p.danger))
        } else {
            icon(Icon::Play, 11.0, p.accent_text)
        };
        let label = match (starting, stopping, streaming) {
            (true, _, _) => "Starting…",
            (_, true, _) => "Stopping…",
            (_, _, true) => "Stop",
            _ => "Start",
        };
        tip(
            button(
                row![glyph, text(label).size(style::SMALL)]
                    .spacing(6)
                    .align_y(Alignment::Center),
            )
            .padding([4, 10])
            .style(move |theme, status| {
                style::secondary(theme, if busy { button::Status::Active } else { status })
            })
            .on_press_maybe((!busy).then_some(Message::ToggleStream)),
            Action::ToggleStream.hint(
                if streaming {
                    "Stop the stream"
                } else {
                    "Start the stream"
                },
                Os::CURRENT,
            ),
        )
    }

    pub(super) fn num(&self, key: Num) -> Element<'_, Message> {
        let value = self
            .drafts
            .get(&key)
            .cloned()
            .unwrap_or_else(|| self.num_text(key));
        let valid = !self.drafts.contains_key(&key) || num_valid(key, &value);
        text_input("", &value)
            .on_input(move |value| Message::Num(key, value))
            .size(style::BODY)
            .padding([5, 8])
            .width(Fill)
            .align_x(Alignment::End)
            .style(if valid {
                style::input
            } else {
                style::input_invalid
            })
            .into()
    }

    pub(super) fn capture_settings(&self, p: &'static Palette) -> Element<'_, Message> {
        let connected = self.snapshot.connected.is_some();
        let capturing = self.pending("Saving capture");
        let saved = self.just_saved(self.snapshot.active_camera.as_deref());
        let os = Os::CURRENT;
        let (glyph, label) = if capturing {
            (icon::spinner(14.0, Color::WHITE, self.spin()), "Saving…")
        } else if saved {
            (icon(Icon::Check, 14.0, Color::WHITE), "Saved")
        } else {
            (icon(Icon::Camera, 14.0, Color::WHITE), "Capture & save")
        };
        let ready = connected && !self.output.trim().is_empty();
        let mut action = column![tip(
            wide_button(
                glyph,
                label,
                style::primary,
                ready.then_some(Message::Capture),
                capturing
            ),
            Action::Capture.hint("Capture and save", os),
        )]
        .spacing(8);
        if !capturing && let Some(status) = self.capture_status(connected, p) {
            action = action.push(status);
        }
        let mut save = column![
            section("Save frames", None, p),
            self.capture_to
                .view(&self.output, "Output path", self.dark())
                .map(Message::CapturePicker),
            form_row("Frames", self.num(Num::Count), "", p),
            form_row(
                "Format",
                pick_list(&FORMATS[..], find(&FORMATS, self.format), Message::Format)
                    .width(Fill)
                    .text_size(style::BODY)
                    .padding([5, 8])
                    .style(style::pick)
                    .menu_style(style::menu),
                "",
                p,
            ),
            form_row("Timeout", self.num(Num::Timeout), "ms", p),
            action,
            checkbox(
                "Capture on a schedule",
                self.schedule_enabled,
                Message::ScheduleEnabled,
            ),
        ]
        .spacing(ROW_GAP);
        if self.schedule_enabled {
            save = save
                .push(form_row("Start after", self.num(Num::Delay), "s", p))
                .push(form_row("Frame interval", self.num(Num::Interval), "s", p))
                .push(
                    text(schedule_text(self.count, self.schedule_interval_seconds))
                        .size(style::SMALL)
                        .color(p.secondary),
                )
                .push(
                    button(
                        center(text("Schedule capture").size(style::BODY)).height(Length::Shrink),
                    )
                    .width(Fill)
                    .padding([8, 14])
                    .style(style::secondary)
                    .on_press_maybe(ready.then_some(Message::Schedule)),
                );
        }
        column![
            save,
            self.storage_settings(p),
            self.jobs(p),
            column![
                section("Automation", None, p),
                text(
                    "Control this camera from a shell or coding agent while this window keeps it."
                )
                .size(style::SMALL)
                .color(p.secondary),
                self.copyable(self.session_command(), p),
            ]
            .spacing(ROW_GAP),
        ]
        .spacing(SECTION_GAP)
        .into()
    }

    /// Under the capture button: why it cannot capture, or else the last
    /// file this camera saved, with a button that copies its path.
    fn capture_status(&self, connected: bool, p: &'static Palette) -> Option<Element<'_, Message>> {
        if !connected {
            return Some(reason("Connect a camera to capture.", p));
        }
        if self.output.trim().is_empty() {
            return Some(reason("Enter a file name to save to.", p));
        }
        let saved = self.last_saved.as_ref().filter(|saved| {
            saved.camera.is_none() || saved.camera == self.snapshot.active_camera
        })?;
        let path = std::path::Path::new(&saved.path);
        let name = |path: &std::path::Path| {
            path.file_name()
                .map_or_else(|| path.to_string_lossy(), |name| name.to_string_lossy())
                .into_owned()
        };
        let folder = path
            .parent()
            .filter(|folder| !folder.as_os_str().is_empty());
        let (mut summary, copy) = match folder {
            Some(folder) if saved.count > 1 => (
                format!("{} files in {}", grouped(saved.count as u64), name(folder)),
                folder.to_string_lossy().into_owned(),
            ),
            _ if saved.count > 1 => (
                format!("{} files", grouped(saved.count as u64)),
                saved.path.clone(),
            ),
            _ => (name(path), saved.path.clone()),
        };
        if let Some(destination) = &self.inspect.saved_to {
            summary = format!("{summary} · {destination}");
        }
        let copied = self.just_copied(&copy);
        Some(
            row![
                icon(Icon::Check, 12.0, p.ink(p.live)),
                tip(
                    one_line(summary, style::SMALL, style::SANS, p.secondary).width(Fill),
                    copy.clone(),
                ),
                tip(
                    button(icon(
                        if copied { Icon::Check } else { Icon::Copy },
                        12.0,
                        if copied { p.ink(p.live) } else { p.secondary },
                    ))
                    .padding(4)
                    .style(style::plain)
                    .on_press(Message::Copy(copy)),
                    if copied { "Copied" } else { "Copy path" },
                ),
            ]
            .spacing(6)
            .align_y(Alignment::Center)
            .into(),
        )
    }

    pub(super) fn forward_settings(&self, p: &'static Palette) -> Element<'_, Message> {
        let first = match &self.snapshot.forwarding {
            Some(destination) => self.forward_card(destination, p),
            None => self.forward_form(p),
        };
        column![
            first,
            self.storage_settings(p),
            column![
                section("Encoder availability", None, p),
                text("Auto picks an available hardware or software encoder. To list what this computer has, run:")
                    .size(style::SMALL)
                    .color(p.secondary),
                self.copyable("capturefab doctor".into(), p),
            ]
            .spacing(ROW_GAP),
        ]
        .spacing(SECTION_GAP)
        .into()
    }

    fn forward_form(&self, p: &'static Palette) -> Element<'_, Message> {
        let connected = self.snapshot.connected.is_some();
        let starting = self.pending("Starting forwarding");
        let glyph = if starting {
            icon::spinner(15.0, Color::WHITE, self.spin())
        } else {
            icon(Icon::Broadcast, 15.0, Color::WHITE)
        };
        let ready = connected && !self.forward_output.trim().is_empty();
        let mut action = column![wide_button(
            glyph,
            if starting {
                "Starting…"
            } else {
                "Start forwarding"
            },
            style::primary,
            ready.then_some(Message::StartForward),
            starting,
        )]
        .spacing(8);
        if !connected {
            action = action.push(reason("Connect a camera to forward it.", p));
        } else if self.forward_output.trim().is_empty() {
            action = action.push(reason("Enter a stream URL or a file name.", p));
        }
        column![
            section("Forward to a recorder", None, p),
            text("Publish this camera to MediaMTX, an NVR or a recording file.")
                .size(style::SMALL)
                .color(p.secondary),
            self.record_to
                .view(&self.forward_output, "Stream URL or file", self.dark())
                .map(Message::RecordPicker),
            form_row(
                "Codec",
                pick_list(
                    &CODECS[..],
                    find(&CODECS, self.forward_codec),
                    Message::Codec
                )
                .width(Fill)
                .text_size(style::BODY)
                .padding([5, 8])
                .style(style::pick)
                .menu_style(style::menu),
                "",
                p,
            ),
            form_row(
                "Encoder",
                tip(
                    text_input("auto", &self.forward_encoder)
                        .on_input(Message::Encoder)
                        .size(style::BODY)
                        .padding([5, 8])
                        .width(Fill)
                        .style(style::input),
                    "auto picks an available encoder. Enter an FFmpeg encoder name to choose one.",
                ),
                "",
                p,
            ),
            form_row("Frame rate", self.num(Num::Fps), "fps", p),
            form_row(
                "Bitrate",
                text_input("4M", &self.forward_bitrate)
                    .on_input(Message::Bitrate)
                    .size(style::BODY)
                    .padding([5, 8])
                    .width(Fill)
                    .style(style::input),
                "",
                p,
            ),
            form_row(
                "Maximum file",
                tip(
                    self.num(Num::FileMib),
                    "For file destinations, recording stops when this file size limit is reached.",
                ),
                "MiB",
                p,
            ),
            action,
        ]
        .spacing(ROW_GAP)
        .into()
    }

    /// While forwarding: where to, with what, and how to stop. Its settings
    /// cannot change mid-stream, so the form gives way to this.
    fn forward_card<'a>(
        &'a self,
        destination: &'a str,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let stopping = self.pending("Stopping forwarding");
        // A path rather than a URL is a recording on this computer.
        let recording = !destination.contains("://");
        let (mark, title, stop) = if recording {
            (dot(p.danger, 8.0), "Recording", "Stop recording")
        } else {
            (
                icon(Icon::Broadcast, 14.0, p.accent_text),
                "Forwarding",
                "Stop forwarding",
            )
        };
        let mut card = column![
            row![mark, text(title).size(style::BODY).font(style::SEMIBOLD)]
                .spacing(8)
                .align_y(Alignment::Center),
            tip(
                one_line(
                    redact_address(destination),
                    style::SMALL,
                    style::MONO,
                    p.secondary
                )
                .width(Fill),
                redact_address(destination),
            ),
        ]
        .spacing(6);
        // Only settings sent from here are known; the CLI may have started it.
        if let Some((_, summary)) = self
            .inspect
            .forwarded
            .as_ref()
            .filter(|(output, _)| output == destination)
        {
            card = card.push(text(summary.as_str()).size(style::SMALL).color(p.secondary));
        }
        let glyph = if stopping {
            icon::spinner(12.0, p.secondary, self.spin())
        } else {
            icon(Icon::Stop, 12.0, p.ink(p.danger))
        };
        card = card.push(space().height(2)).push(wide_button(
            glyph,
            if stopping { "Stopping…" } else { stop },
            style::stop,
            Some(Message::StopForward),
            stopping,
        ));
        column![
            container(card).padding(14).width(Fill).style(style::well),
            reason(
                if recording {
                    "Stop recording to change its settings."
                } else {
                    "Stop forwarding to change its settings."
                },
                p
            ),
        ]
        .spacing(10)
        .into()
    }

    pub(super) fn storage_settings(&self, p: &'static Palette) -> Element<'_, Message> {
        let open = self.storage_open;
        let summary = format!(
            "{} GiB · {} files",
            number(self.quota_gib),
            grouped(self.quota_files.into()),
        );
        let mut content = column![disclosure(
            "Storage & retention",
            (!open).then_some(summary),
            self.inspect.storage_turn.get(self.now),
            Message::ToggleStorage,
            p
        )]
        .spacing(ROW_GAP);
        if open {
            content = content
                .push(form_row("Disk budget", self.num(Num::QuotaGib), "GiB", p))
                .push(form_row("File limit", self.num(Num::QuotaFiles), "", p))
                .push(form_row(
                    "At capacity",
                    pick_list(
                        &ON_FULL[..],
                        find(&ON_FULL, self.quota_action),
                        Message::OnFull,
                    )
                    .width(Fill)
                    .text_size(style::BODY)
                    .padding([5, 8])
                    .style(style::pick)
                    .menu_style(style::menu),
                    "",
                    p,
                ))
                .push(checkbox(
                    "Limit capture age",
                    self.retention_enabled,
                    Message::RetentionEnabled,
                ));
            if self.retention_enabled {
                content = content.push(form_row(
                    "Keep for",
                    self.num(Num::RetentionDays),
                    "days",
                    p,
                ));
            }
            if self.quota_action == "delete-oldest" {
                content = content.push(
                    text("At capacity, removes the oldest Capturefab managed files in the output location.")
                        .size(style::SMALL)
                        .color(p.ink(p.warn)),
                );
            }
        }
        content.into()
    }

    pub(super) fn jobs(&self, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        let refresh = if self.pending("Refreshing capture jobs") {
            container(text("Refreshing…").size(style::SMALL).color(p.secondary))
                .padding([3, 8])
                .into()
        } else {
            link(
                "Refresh",
                snapshot.connected.is_some().then_some(Message::RefreshJobs),
            )
        };
        let mut content = column![section("Scheduled jobs", Some(refresh), p)].spacing(10);
        if snapshot.jobs.is_empty() {
            return content
                .push(
                    text("No scheduled captures for this camera.")
                        .size(style::SMALL)
                        .color(p.secondary),
                )
                .into();
        }
        let jobs = serde_json::to_value(&snapshot.jobs).unwrap_or_default();
        for job in jobs.as_array().into_iter().flatten().rev() {
            let id = job["id"].as_u64().unwrap_or_default();
            let status = job["status"].as_str().unwrap_or("unknown");
            let captured = job["captured"].as_u64().unwrap_or_default();
            let count = job["count"].as_u64().unwrap_or(1);
            let mut card = column![
                row![
                    text(format!("Job {id}"))
                        .size(style::BODY)
                        .font(style::MEDIUM),
                    space::horizontal(),
                    text(capitalize(status))
                        .size(style::CAPTION)
                        .color(match status {
                            "failed" => p.ink(p.danger),
                            "running" | "pending" => p.accent_text,
                            _ => p.secondary,
                        }),
                ]
                .align_y(Alignment::Center),
                progress_bar(0.0..=1.0, captured as f32 / count.max(1) as f32)
                    .girth(4)
                    .style(style::progress),
                text(format!("{captured} / {count} frames"))
                    .size(style::CAPTION)
                    .color(p.secondary),
                text(job["output"].as_str().unwrap_or("").to_owned())
                    .size(style::CAPTION)
                    .font(style::MONO)
                    .color(p.secondary),
            ]
            .spacing(6);
            if matches!(status, "pending" | "running") {
                let wait = job["next_at_ms"]
                    .as_u64()
                    .unwrap_or_default()
                    .saturating_sub(epoch_ms());
                card = card.push(
                    row![
                        text(format!("Next frame in {:.1} s", wait as f64 / 1000.0))
                            .size(style::CAPTION)
                            .color(p.secondary),
                        space::horizontal(),
                        button(text("Stop").size(style::SMALL))
                            .padding([3, 8])
                            .style(style::stop)
                            .on_press(Message::CancelJob(id)),
                    ]
                    .align_y(Alignment::Center),
                );
            }
            if let Some(error) = job["error"].as_str() {
                card = card.push(
                    text(error.to_owned())
                        .size(style::CAPTION)
                        .color(p.ink(p.danger)),
                );
            }
            content = content.push(container(card).padding(12).width(Fill).style(style::well));
        }
        content.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bench() -> Workbench {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        bench.snapshot.features = vec![
            sample_feature("Gain", "Float", json!(0.0)),
            sample_feature("Width", "Integer", json!(640)),
            sample_feature("AcquisitionStop", "Command", json!(null)),
            sample_feature("ExposureTime", "Float", json!(10000.0)),
            sample_feature("Height", "Integer", json!(480)),
            sample_feature("AcquisitionStart", "Command", json!(null)),
            sample_feature("DeviceModelName", "String", json!("Pattern")),
        ];
        bench.snapshot.active_camera = Some("sim:0".into());
        bench.tick_inspector();
        bench
    }

    /// Whether anything the inspector registered still moves.
    fn moving(bench: &Workbench) -> bool {
        let now = bench.now;
        bench.inspect.motions().any(|motion| motion.animating(now))
            || bench
                .inspect
                .flashes()
                .any(|flashes| flashes.animating(now))
    }

    fn sent(
        bench: &mut Workbench,
        feature: &str,
    ) -> mpsc::Sender<anyhow::Result<serde_json::Value>> {
        let (sender, receiver) = mpsc::channel();
        bench.pending.push(Pending {
            label: format!("Setting {feature}"),
            receiver,
            target: None,
            camera: Some("sim:0".into()),
            feature: Some(feature.into()),
            at: bench.now,
            batch: false,
        });
        bench.sync_animations();
        sender
    }

    #[test]
    fn features_list_in_setup_order_with_one_stream_row() {
        let bench = bench();
        let index = &bench.inspect.index;
        let names = |group: &str| -> Vec<&str> {
            index
                .groups
                .iter()
                .find(|(name, _)| *name == group)
                .map(|(_, members)| {
                    members
                        .iter()
                        .map(|&i| bench.snapshot.features[i].name.as_str())
                        .collect()
                })
                .unwrap_or_default()
        };
        assert_eq!(names("Image"), ["Width", "Height"]);
        assert_eq!(
            names("Acquisition"),
            ["AcquisitionStart", "ExposureTime", "Gain"]
        );
        assert_eq!(names("Device"), ["DeviceModelName"]);
        assert_eq!(index.listed, 6, "AcquisitionStop folds into the Stream row");
        assert_eq!(index.labels[5], "Stream");
        assert_eq!(index.labels[3], "Exposure Time");
        assert!(index.words[3].contains("exposure time"));
    }

    #[test]
    fn the_index_follows_the_names_not_the_values() {
        let mut bench = bench();
        let key = bench.inspect.index.key;
        bench.snapshot.features[0].value = Some(json!(3.5));
        bench.tick_inspector();
        assert_eq!(bench.inspect.index.key, key);
        bench.snapshot.features[0].name = "BlackLevel".into();
        bench.tick_inspector();
        assert_ne!(bench.inspect.index.key, key);
        bench.snapshot.features.pop();
        bench.sync_animations();
        assert_eq!(bench.inspect.index.len, bench.snapshot.features.len());
    }

    #[test]
    fn an_accepted_write_shows_the_stored_value_and_glows_once() {
        let mut bench = bench();
        bench.edits.insert("ExposureTime".into(), "15003".into());
        let reply = sent(&mut bench, "ExposureTime");
        reply
            .send(Ok(json!({"name": "ExposureTime", "value": 15000.0})))
            .unwrap();
        bench.poll();
        assert_eq!(
            bench.edits.get("ExposureTime").map(String::as_str),
            Some("15000"),
            "what the camera stored"
        );
        let start = bench.now;
        assert_eq!(bench.inspect.accepted.level("ExposureTime", start), 1.0);
        assert_eq!(bench.inspect.accepted.level("Gain", start), 0.0);
        assert!(moving(&bench));
        bench.now = start + ACCEPTED;
        assert!(!moving(&bench), "the glow settles");
        assert_eq!(bench.inspect.accepted.level("ExposureTime", bench.now), 0.0);
    }

    #[test]
    fn a_draft_edited_while_its_write_is_on_its_way_stays() {
        let mut bench = bench();
        bench.edits.insert("ExposureTime".into(), "15000".into());
        let reply = sent(&mut bench, "ExposureTime");
        bench.edits.insert("ExposureTime".into(), "150001".into());
        reply
            .send(Ok(json!({"name": "ExposureTime", "value": 15000.0})))
            .unwrap();
        bench.poll();
        assert_eq!(
            bench.edits.get("ExposureTime").map(String::as_str),
            Some("150001")
        );
        assert!(bench.inspect.sent.is_empty());
    }

    #[test]
    fn a_refused_write_keeps_the_draft_and_its_error() {
        let mut bench = bench();
        bench.edits.insert("ExposureTime".into(), "5".into());
        let reply = sent(&mut bench, "ExposureTime");
        reply
            .send(Err(anyhow::anyhow!("5 is below the minimum of 10")))
            .unwrap();
        bench.poll();
        assert_eq!(
            bench.edits.get("ExposureTime").map(String::as_str),
            Some("5")
        );
        assert!(bench.inspect.write_errors.contains_key("ExposureTime"));
        assert!(bench.inspect.accepted.is_empty(), "no glow");
    }

    #[test]
    fn thumbs_jump_into_place_then_slide() {
        let mut bench = bench();
        bench.tab = Tab::Forward;
        bench.sync_animations();
        let start = bench.now;
        assert_eq!(bench.inspect.tab_thumb.get(start), 2.0, "placed at once");
        assert!(!moving(&bench));
        bench.tab = Tab::Features;
        bench.sync_animations();
        let mid = bench.inspect.tab_thumb.get(start + THUMB / 2);
        assert!(mid > 0.0 && mid < 2.0, "slides: {mid}");
        bench.now = start + THUMB;
        assert!(!moving(&bench), "settles");
        assert_eq!(bench.inspect.tab_thumb.get(bench.now), 0.0);
    }

    #[test]
    fn the_mode_thumb_moves_on_the_request_and_back_on_refusal() {
        let mut bench = bench();
        bench.sync_animations();
        let (sender, receiver) = mpsc::channel();
        bench.pending.push(Pending {
            label: "Enabling auto mode".into(),
            receiver,
            target: None,
            camera: Some("sim:0".into()),
            feature: None,
            at: bench.now,
            batch: false,
        });
        bench.sync_animations();
        assert_eq!(bench.inspect.mode_thumb.target(), 1.0, "optimistic");
        sender.send(Err(anyhow::anyhow!("refused"))).unwrap();
        bench.poll();
        bench.sync_animations();
        assert_eq!(bench.inspect.mode_thumb.target(), 0.0, "slides back");
        bench.now += THUMB;
        assert!(!moving(&bench));
        // Another camera's mode is no change of mode: the thumb jumps.
        bench.snapshot.active_camera = Some("sim:1".into());
        bench.snapshot.auto = Some(Default::default());
        bench.sync_animations();
        assert_eq!(bench.inspect.mode_thumb.get(bench.now), 1.0);
        assert!(!bench.inspect.mode_thumb.animating(bench.now));
    }

    #[test]
    fn the_context_row_lights_up_when_another_camera_is_selected() {
        let mut bench = bench();
        bench.snapshot.cameras = vec![
            liveness::streaming_camera(10, 0, 30.0),
            liveness::streaming_camera(10, 0, 30.0),
        ];
        bench.sync_animations();
        let start = bench.now;
        assert_eq!(bench.inspect.context.get(start), 0.0);
        bench.snapshot.active_camera = Some("sim:1".into());
        bench.sync_animations();
        assert_eq!(bench.inspect.context.get(start), 1.0);
        bench.now = start + CONTEXT;
        assert!(!moving(&bench), "fades out and settles");
    }

    #[test]
    fn disclosures_turn_open_and_settle() {
        let mut bench = bench();
        bench.sync_animations();
        let start = bench.now;
        bench.storage_open = true;
        bench.sync_animations();
        let mid = bench.inspect.storage_turn.get(start + motion::RING / 2);
        assert!(mid > 0.0 && mid < 1.0, "turns: {mid}");
        bench.now = start + motion::RING;
        assert!(!moving(&bench));
        assert_eq!(bench.inspect.storage_turn.get(bench.now), 1.0);
        bench.storage_open = false;
        bench.sync_animations();
        bench.now += motion::RING_OUT;
        assert!(!moving(&bench));
        assert_eq!(bench.inspect.storage_turn.get(bench.now), 0.0);
    }

    #[test]
    fn schedules_read_in_the_right_number() {
        assert_eq!(schedule_text(1, 60.0), "1 frame, one every 60 seconds");
        assert_eq!(schedule_text(1200, 1.0), "1,200 frames, one every 1 second");
        assert_eq!(schedule_text(10, 2.5), "10 frames, one every 2.5 seconds");
    }

    #[test]
    fn forward_settings_sum_up_what_was_sent() {
        let mut bench = bench();
        assert_eq!(bench.forward_summary(), "H.264 · 30 fps · 4M");
        bench.forward_encoder = "h264_videotoolbox".into();
        bench.forward_bitrate = " ".into();
        assert_eq!(
            bench.forward_summary(),
            "H.264 · 30 fps · h264_videotoolbox"
        );
    }
}
