use crate::{
    camera::Camera,
    genicam::Bounds,
    types::{Frame, Transport, TransportStats},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutoChange {
    pub time: String,
    pub feature: String,
    pub from: Option<Value>,
    pub to: Value,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AutoStatus {
    pub balance: f64,
    pub strategy: String,
    pub state: String,
    pub target_fps: Option<f64>,
    pub exposure_us: Option<f64>,
    pub gain_db: Option<f64>,
    pub exposure_limits_us: Option<[f64; 2]>,
    pub gain_limits_db: Option<[f64; 2]>,
    pub brightness: Option<f64>,
    #[serde(default)]
    pub managed: Vec<String>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub changes: Vec<AutoChange>,
}

pub const DEFAULT_BALANCE: f64 = 0.5;
const TARGET: f64 = 0.45;
const AUTOS: [&str; 3] = ["ExposureAuto", "GainAuto", "BalanceWhiteAuto"];
const SECOND: Duration = Duration::from_secs(1);
const DARK: &str = "too dark for this balance: slide toward Quality or add light";
const BRIGHT: &str = "too bright at minimum exposure";

pub fn validate_balance(balance: f64) -> Result<f64> {
    ensure!(
        balance.is_finite() && (0.0..=1.0).contains(&balance),
        "balance must be between 0 (quality) and 1 (frame rate)"
    );
    Ok(balance)
}

pub(crate) fn clock() -> String {
    let s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        % 86400;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

fn finite(v: f64) -> Option<f64> {
    v.is_finite().then_some(v)
}

fn range(low: f64, high: f64) -> Option<[f64; 2]> {
    (low.is_finite() && high.is_finite()).then_some([low, high])
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Meter {
    pub mean: f64,
    pub p99: f64,
}

pub fn meter(frame: &Frame) -> Option<Meter> {
    let (w, h) = (frame.width as usize, frame.height as usize);
    let (bytes, bits, quad) = match frame.pixel_format {
        crate::types::MONO8 => (1, 8, false),
        crate::types::RGB8 | 0x0218_0015 => (3, 8, false),
        0x0110_0003 => (2, 10, false),
        0x0110_0005 => (2, 12, false),
        0x0110_0007 => (2, 16, false),
        0x0108_0008..=0x0108_000b => (1, 8, true),
        _ => return None,
    };
    let (gw, gh) = if quad { (w / 2, h / 2) } else { (w, h) };
    if gw == 0 || gh == 0 || frame.data.len() < w.checked_mul(h)?.checked_mul(bytes)? {
        return None;
    }
    let mut stride = 1;
    while gw.div_ceil(stride) * gh.div_ceil(stride) > 16_384 {
        stride += 1;
    }
    let scale = ((1u32 << bits) - 1) as f64;
    let d = &frame.data;
    let px = |i: usize| match bytes {
        2 => u16::from_le_bytes([d[2 * i], d[2 * i + 1]]) as f64,
        _ => d[i] as f64,
    };
    let (mut sum, mut count, mut histogram) = (0.0, 0u32, [0u32; 256]);
    for y in (0..gh).step_by(stride) {
        for x in (0..gw).step_by(stride) {
            let (luma, peak) = if quad {
                let i = 2 * (y * w + x);
                let q = [px(i), px(i + 1), px(i + w), px(i + w + 1)];
                (
                    q.iter().sum::<f64>() / 4.0,
                    q.into_iter().fold(0.0, f64::max),
                )
            } else if bytes == 3 {
                let i = 3 * (y * w + x);
                let (r, g, b) = (px(i), px(i + 1), px(i + 2));
                ((r + 2.0 * g + b) / 4.0, r.max(g).max(b))
            } else {
                let v = px(y * w + x);
                (v, v)
            };
            sum += (luma / scale).min(1.0);
            count += 1;
            histogram[((peak / scale).min(1.0) * 255.0).round() as usize] += 1;
        }
    }
    let mut seen = 0;
    let p99 = histogram
        .iter()
        .position(|n| {
            seen += n;
            seen as f64 >= 0.99 * count as f64
        })
        .unwrap_or(255);
    Some(Meter {
        mean: (sum / count as f64).max(1.0 / 1024.0),
        p99: p99 as f64 / 255.0,
    })
}

fn error(m: Meter) -> f64 {
    let e = (TARGET / m.mean).log2();
    e.min((0.96 / m.p99).log2()).max(e - 1.0)
}

#[derive(Debug, Clone, Default)]
pub struct Caps {
    pub exposure: Option<&'static str>,
    pub exposure_bounds: Bounds,
    pub exposure_floor: Option<f64>,
    pub gain: Option<&'static str>,
    pub gain_bounds: Bounds,
    pub sensor_fps: Option<f64>,
    pub link: Option<f64>,
    pub payload: Option<f64>,
    pub forward_fps: Option<f64>,
    pub triggered: bool,
    pub trigger_period: Option<f64>,
    pub exposure_auto: Option<&'static str>,
    pub gain_auto: Option<&'static str>,
    pub white_auto: Option<&'static str>,
    pub exposure_limits: Option<[&'static str; 2]>,
    pub gain_limits: Option<[&'static str; 2]>,
    pub profile: Option<[&'static str; 2]>,
    pub target: Option<f64>,
    pub frame_rate: Option<&'static str>,
    pub frame_rate_enable: Option<&'static str>,
    pub resulting_fps: Option<&'static str>,
    pub throughput: Option<&'static str>,
    pub throughput_mode: Option<&'static str>,
    pub current_throughput: Option<&'static str>,
    pub gain_selector: Option<&'static str>,
    pub video_mode: bool,
    pub color: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Policy {
    pub fps_max: f64,
    pub fps_floor: f64,
    pub fps_target: Option<f64>,
    pub exposure_lower: f64,
    pub exposure_cap: f64,
    pub gain_min: f64,
    pub gain_cap: f64,
}

fn margin(period: f64) -> f64 {
    (0.02 * period).max(100.0)
}

pub fn policy(caps: &Caps, balance: f64) -> Policy {
    let fps_link = match (caps.link, caps.payload) {
        (Some(c), Some(p)) if c.is_finite() && c > 0.0 && p.is_finite() && p > 0.0 => {
            0.92 * c / (1.05 * p)
        }
        _ => f64::INFINITY,
    };
    let fps_max = [caps.sensor_fps, caps.forward_fps]
        .into_iter()
        .flatten()
        .filter(|fps| fps.is_finite() && *fps > 0.0)
        .fold(fps_link.min(10_000.0), f64::min);
    let fps_floor = (fps_max / 4.0).max(fps_max.min(10.0));
    let fps_target = fps_floor * (fps_max / fps_floor).powf(balance);
    let e = caps.exposure_bounds;
    let (e_min, e_max) = (e.min.unwrap_or(0.0), e.max.unwrap_or(f64::INFINITY));
    let period = if caps.triggered {
        caps.trigger_period
    } else {
        Some(1e6 / fps_target)
    };
    let (g_min, g_max) = match caps.gain {
        Some(_) => (
            caps.gain_bounds.min.unwrap_or(0.0),
            caps.gain_bounds.max.unwrap_or(f64::INFINITY),
        ),
        None => (0.0, 0.0),
    };
    Policy {
        fps_max,
        fps_floor,
        fps_target: (!caps.triggered).then_some(fps_target),
        exposure_lower: e_min.max(caps.exposure_floor.unwrap_or(0.0)),
        exposure_cap: period
            .map_or(e_max, |p| e_max.min(p - margin(p)))
            .max(e_min),
        gain_min: g_min,
        gain_cap: g_min + (g_max - g_min).min(6.0 + 18.0 * balance),
    }
}

pub fn mode_score(pixels: f64, fps: f64, pixels_max: f64, fps_max: f64, balance: f64) -> f64 {
    (1.0 - balance) * (pixels / pixels_max).log2() + balance * (fps / fps_max).log2()
}

#[derive(Debug, Clone)]
pub struct Aimd {
    pub limit: f64,
    pub floor: f64,
    pub cap: f64,
    pub step: f64,
    jitter: f64,
    slow_start: bool,
    hold: Duration,
    hold_until: Option<Instant>,
    last_loss: Option<Instant>,
}

impl Aimd {
    pub fn new(limit: f64, floor: f64, cap: f64, step: f64, jitter: f64) -> Self {
        Self {
            limit,
            floor,
            cap,
            step,
            jitter,
            slow_start: true,
            hold: 10 * SECOND,
            hold_until: None,
            last_loss: None,
        }
    }
    pub fn holding(&self, now: Instant) -> bool {
        self.hold_until.is_some_and(|t| now < t)
    }
    pub fn step(&mut self, now: Instant, usage: f64, loss: bool, probe: bool) -> Option<f64> {
        let since_loss = self.last_loss.map(|t| now.saturating_duration_since(t));
        if since_loss.is_some_and(|d| d >= 300 * SECOND) {
            self.hold = 10 * SECOND;
        }
        let next = if loss {
            if since_loss.is_some_and(|d| d < 60 * SECOND) {
                self.hold = (self.hold * 2).min(160 * SECOND);
            }
            self.slow_start = false;
            self.last_loss = Some(now);
            self.hold_until = Some(now + self.hold.mul_f64(1.0 + self.jitter));
            (0.8 * self.limit.min(usage))
                .max(self.floor)
                .min(self.limit)
        } else if probe && !self.holding(now) {
            let raised = if self.slow_start {
                self.limit * 1.25
            } else {
                self.limit + self.step
            };
            raised.min(self.cap).max(self.limit)
        } else {
            self.limit
        };
        (next != self.limit).then(|| {
            self.limit = next;
            next
        })
    }
}

struct Bandwidth {
    aimd: Aimd,
    feature: &'static str,
    fps: bool,
    since: Option<Instant>,
    base: TransportStats,
    frames: u64,
}

#[derive(Default)]
struct Supervisor {
    read: Option<(Instant, f64, f64)>,
    decided: Option<Instant>,
    high: u32,
    low: u32,
    dark: u32,
    stall: Option<Instant>,
    calibrated: bool,
    lowered: bool,
}

struct WhiteBalance {
    selector: Option<Value>,
    ratios: Vec<(Option<String>, Value)>,
}

#[derive(Default)]
pub struct AutoController {
    status: AutoStatus,
    published: AutoStatus,
    published_at: Option<Instant>,
    caps: Caps,
    policy: Policy,
    values: BTreeMap<String, Value>,
    originals: Vec<(String, Value)>,
    notes: BTreeMap<String, String>,
    blocked: Vec<String>,
    failed: Vec<(String, Value, &'static str)>,
    stale: bool,
    reprobe: bool,
    planned: bool,
    jitter: f64,
    retry_at: Option<Instant>,
    backoff: Duration,
    streaming: bool,
    last_tick: Option<Instant>,
    last_frame: Option<Instant>,
    period: Option<f64>,
    frames: u64,
    last_meter: Option<Instant>,
    samples: VecDeque<f64>,
    p99: f64,
    in_band: u32,
    unmetered: bool,
    correcting: bool,
    settle: Option<(Instant, u64)>,
    writes: u64,
    counted: u64,
    written: Option<(Instant, u64)>,
    quiet_until: Option<Instant>,
    fw: Supervisor,
    bound: bool,
    link_fps: Option<f64>,
    bandwidth: Option<Bandwidth>,
    white_balance: Option<WhiteBalance>,
}

impl AutoController {
    pub fn enter(camera: &mut Camera, balance: f64) -> Result<Self> {
        let balance = validate_balance(balance)?;
        let hash = camera
            .info
            .serial
            .bytes()
            .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b.into()));
        let mut controller = Self {
            status: AutoStatus {
                balance,
                state: "waiting".into(),
                ..Default::default()
            },
            jitter: (hash % 1000) as f64 / 4000.0,
            ..Default::default()
        };
        controller
            .probe(camera)
            .context("auto mode capability probe")?;
        controller.plan(camera);
        let strategy = json!(controller.status.strategy);
        controller.change(
            "auto",
            None,
            strategy,
            &format!("auto mode entered with balance {balance:.2}"),
        );
        Ok(controller)
    }
    pub fn set_balance(&mut self, camera: &mut Camera, balance: f64) -> Result<()> {
        let balance = validate_balance(balance)?;
        if balance != self.status.balance {
            self.change(
                "balance",
                Some(json!(self.status.balance)),
                json!(balance),
                "balance changed",
            );
            self.status.balance = balance;
            self.plan(camera);
        }
        Ok(())
    }
    pub fn set_forward_fps(&mut self, fps: Option<f64>) {
        if self.caps.forward_fps != fps {
            self.caps.forward_fps = fps;
            self.stale = true;
        }
    }
    pub fn observe(&mut self, frame: &Frame, now: Instant) {
        if let Some(last) = self.last_frame {
            let dt = now.saturating_duration_since(last).as_secs_f64();
            self.period = Some(self.period.map_or(dt, |p| 0.9 * p + 0.1 * dt));
        }
        self.last_frame = Some(now);
        self.frames += 1;
        if self
            .last_meter
            .is_some_and(|t| now.saturating_duration_since(t) < Duration::from_millis(100))
            || self
                .settle
                .is_some_and(|(until, frames)| now < until || self.frames < frames)
        {
            return;
        }
        self.last_meter = Some(now);
        let Some(m) = meter(frame) else {
            self.unmetered = true;
            return;
        };
        let e = match self.status.brightness {
            _ if self.status.strategy == "software" => error(m),
            Some(b) => (m.mean / b).log2(),
            None => 1.0,
        };
        self.unmetered = false;
        self.in_band = if e.abs() < self.band().0 {
            self.in_band + 1
        } else {
            0
        };
        self.samples.push_back(e);
        if self.samples.len() > 3 {
            self.samples.pop_front();
        }
        self.p99 = m.p99;
        self.status.brightness = finite(m.mean);
    }
    pub fn tick(&mut self, camera: &mut Camera, now: Instant) -> bool {
        let stall = Duration::from_secs_f64(3.0 * self.period.unwrap_or(0.0))
            .max(Duration::from_millis(250));
        if self
            .last_tick
            .is_some_and(|t| now.saturating_duration_since(t) > stall)
        {
            self.taint(now);
        }
        self.last_tick = Some(now);
        if camera.is_streaming() != self.streaming {
            self.streaming = !self.streaming;
            self.restart(now);
        }
        self.account(now);
        let started = Instant::now();
        if self.retry_at.is_none_or(|t| now >= t) {
            match self.run(camera, now) {
                Ok(()) => {
                    self.backoff = Duration::ZERO;
                    self.clear_note("camera");
                }
                Err(e) => {
                    self.backoff = (self.backoff * 2).clamp(SECOND, 30 * SECOND);
                    self.retry_at = Some(now + self.backoff);
                    self.note(
                        "camera",
                        format!("auto mode paused by a camera error: {e:#}"),
                    );
                }
            }
        }
        if started.elapsed() > stall {
            self.taint(now);
        }
        self.account(now);
        let due = self.status.state != self.published.state
            || self
                .published_at
                .is_none_or(|t| now.saturating_duration_since(t) >= Duration::from_millis(250));
        if due && self.status != self.published {
            self.published = self.status.clone();
            self.published_at = Some(now);
            return true;
        }
        false
    }
    pub fn invalidate(&mut self) {
        self.stale = true;
        self.reprobe = true;
    }
    pub fn manages(&self, feature: &str) -> bool {
        self.status.managed.iter().any(|f| f == feature)
    }
    pub fn converged(&self) -> bool {
        self.unmetered || matches!(self.status.state.as_str(), "stable" | "limited")
    }
    pub fn status(&self) -> &AutoStatus {
        &self.status
    }
    pub fn values(&self) -> &BTreeMap<String, Value> {
        &self.values
    }
    pub fn choose_video_mode(&mut self, camera: &mut Camera) {
        if !self.caps.video_mode {
            return;
        }
        let balance = self.status.balance;
        let result = camera.choices("VideoMode").and_then(|choices| {
            let modes: Vec<_> = choices
                .iter()
                .filter_map(|m| {
                    let (size, fps) = m.split_once('@')?;
                    let (w, h) = size.split_once('x')?;
                    let pixels = w.parse::<f64>().ok()? * h.parse::<f64>().ok()?;
                    let fps = fps.parse::<f64>().ok()?;
                    (pixels.is_finite() && pixels > 0.0 && fps.is_finite() && fps > 0.0)
                        .then_some((m.as_str(), pixels, fps))
                })
                .collect();
            let pixels = modes.iter().map(|m| m.1).fold(0.0, f64::max);
            let fps = modes.iter().map(|m| m.2).fold(0.0, f64::max);
            let score = |m: &(&str, f64, f64)| mode_score(m.1, m.2, pixels, fps, balance);
            let best = modes
                .iter()
                .max_by(|a, b| {
                    score(a)
                        .total_cmp(&score(b))
                        .then(a.2.total_cmp(&b.2))
                        .then(a.1.total_cmp(&b.1))
                })
                .context("camera lists no video modes")?;
            self.set_mode(camera, json!(best.0), Some("video mode for the balance"))
        });
        match result {
            Ok(_) => self.clear_note("video-mode"),
            Err(e) => self.note("video-mode", format!("video mode unchanged: {e:#}")),
        }
    }
    pub fn release(mut self, camera: &mut Camera, revert: bool) -> Result<()> {
        if !revert {
            return hold(camera);
        }
        self.blocked.clear();
        let mut errors = Vec::new();
        if let Err(e) = hold(camera) {
            errors.push(format!("stop firmware auto: {e:#}"));
        }
        if let Some(white) = self.white_balance.take() {
            for (selector, ratio) in white.ratios {
                let restored = (|| -> Result<()> {
                    if let Some(selector) = selector {
                        camera.set("BalanceRatioSelector", &selector)?;
                    }
                    camera.set("BalanceRatio", &text(&ratio))
                })();
                if let Err(e) = restored {
                    errors.push(format!("white balance: {e:#}"));
                }
            }
            if let Some(selector) = white.selector
                && let Err(e) = camera.set("BalanceRatioSelector", &text(&selector))
            {
                errors.push(format!("white balance selector: {e:#}"));
            }
        }
        let (autos, mut rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.originals)
            .into_iter()
            .rev()
            .partition(|(f, _)| AUTOS.contains(&f.as_str()));
        for [lower, upper] in [self.caps.exposure_limits, self.caps.gain_limits]
            .into_iter()
            .flatten()
        {
            let originals = rest
                .iter()
                .find(|(f, _)| f == lower)
                .zip(rest.iter().find(|(f, _)| f == upper));
            if let Some((low, high)) = originals {
                let low = low.clone();
                let high = high.clone();
                let upper_first = low
                    .1
                    .as_f64()
                    .zip(camera.get(upper).ok().and_then(|v| v.as_f64()))
                    .is_some_and(|(target, current)| target > current);
                rest.retain(|(f, _)| f != lower && f != upper);
                let ordered = if upper_first {
                    [high, low]
                } else {
                    [low, high]
                };
                rest.splice(0..0, ordered);
            }
        }
        let failed: Vec<_> = rest
            .into_iter()
            .chain(autos)
            .filter(|(f, v)| self.restore(camera, f, v).is_err())
            .collect();
        errors.extend(
            failed
                .iter()
                .filter_map(|(f, v)| {
                    let e = self.restore(camera, f, v).err()?;
                    Some(format!("{f}: {e:#}"))
                })
                .collect::<Vec<_>>(),
        );
        ensure!(errors.is_empty(), "could not restore {}", errors.join("; "));
        Ok(())
    }
}

impl AutoController {
    fn probe(&mut self, camera: &mut Camera) -> Result<()> {
        self.caps = Caps {
            forward_fps: self.caps.forward_fps,
            trigger_period: self.caps.trigger_period,
            ..Default::default()
        };
        let media = camera.info.transport == Transport::Media;
        if camera.has("PayloadSize") {
            self.caps.payload = camera.get("PayloadSize")?.as_f64();
        }
        self.caps.exposure = self
            .find(camera, &[["ExposureTime"], ["ExposureTimeAbs"]])
            .map(|[f]| f);
        self.caps.gain = self.find(camera, &[["Gain"], ["GainAbs"]]).map(|[f]| f);
        if camera.get("GainSelector").is_ok_and(|v| v != "All") {
            let choices = camera.choices("GainSelector").unwrap_or_default();
            self.caps.gain_selector = ["All", "AnalogAll"]
                .into_iter()
                .find(|c| choices.iter().any(|v| v == c));
        }
        for f in [self.caps.exposure, self.caps.gain].into_iter().flatten() {
            self.read(camera, f)?;
        }
        self.caps.exposure_bounds = bounds(camera, self.caps.exposure);
        self.caps.gain_bounds = bounds(camera, self.caps.gain);
        for f in AUTOS {
            if let Ok(v) = camera.get(f) {
                self.remember(camera, f);
                self.values.insert(f.into(), v);
            }
        }
        [
            self.caps.exposure_auto,
            self.caps.gain_auto,
            self.caps.white_auto,
        ] = AUTOS.map(|f| {
            self.find(camera, &[[f]])
                .map(|[f]| f)
                .filter(|f| has_choice(camera, f, "Continuous"))
        });
        self.caps.exposure_limits = self.find(
            camera,
            &[
                ["AutoExposureTimeLowerLimit", "AutoExposureTimeUpperLimit"],
                [
                    "AutoExposureTimeAbsLowerLimit",
                    "AutoExposureTimeAbsUpperLimit",
                ],
                [
                    "AutoExposureExposureTimeLowerLimit",
                    "AutoExposureExposureTimeUpperLimit",
                ],
                ["ExposureAutoMin", "ExposureAutoMax"],
                ["AutoExposureTimeMin", "AutoExposureTimeMax"],
            ],
        );
        self.caps.exposure_floor = self
            .caps
            .exposure_limits
            .and_then(|[f, _]| camera.bounds(f).ok()?.min);
        self.caps.gain_limits = self.find(
            camera,
            &[
                ["AutoGainLowerLimit", "AutoGainUpperLimit"],
                ["AutoExposureGainLowerLimit", "AutoExposureGainUpperLimit"],
                ["GainAutoMin", "GainAutoMax"],
                ["AutoGainMin", "AutoGainMax"],
            ],
        );
        self.caps.profile = [
            ["AutoFunctionProfile", "MinimizeGain"],
            ["AutoFunctionProfile", "GainMinimum"],
            ["AutoExposureControlPriority", "Gain"],
        ]
        .into_iter()
        .find(|[f, v]| camera.is_writable(f) && has_choice(camera, f, v));
        self.caps.target = read_any(
            camera,
            &[
                "AutoTargetBrightness",
                "AutoExposureTargetGreyValue",
                "ExposureAutoTarget",
                "ExpectedGrayValue",
            ],
        )
        .and_then(|(f, v)| {
            let b = camera.bounds(f).ok()?;
            let (low, high) = (b.min?, b.max?);
            (high > low).then(|| (v - low) / (high - low))
        });
        if !media {
            self.caps.frame_rate = self
                .find(
                    camera,
                    &[["AcquisitionFrameRate"], ["AcquisitionFrameRateAbs"]],
                )
                .map(|[f]| f);
            self.caps.frame_rate_enable = self
                .find(
                    camera,
                    &[["AcquisitionFrameRateEnable"], ["AcquisitionFrameRateMode"]],
                )
                .map(|[f]| f);
        }
        let rate_max = ["AcquisitionFrameRate", "AcquisitionFrameRateAbs"]
            .into_iter()
            .find(|f| camera.has(f))
            .and_then(|f| camera.bounds(f).ok()?.max);
        self.caps.sensor_fps = read_any(camera, &["SensorReadoutTime", "ReadoutTimeAbs"])
            .and_then(|(_, v)| (v > 0.0).then(|| 1e6 / v))
            .or(rate_max.filter(|m| *m <= 10_000.0));
        self.caps.resulting_fps = [
            "BslResultingAcquisitionFrameRate",
            "ResultingFrameRate",
            "ResultingFrameRateAbs",
            "AcquisitionResultingFrameRate",
            "CurrentAcquisitionFrameRate",
        ]
        .into_iter()
        .find(|f| camera.has(f));
        self.caps.throughput = self
            .find(
                camera,
                &[["DeviceLinkThroughputLimit"], ["StreamBytesPerSecond"]],
            )
            .map(|[f]| f);
        self.caps.throughput_mode = Some("DeviceLinkThroughputLimitMode").filter(|f| {
            self.caps.throughput == Some("DeviceLinkThroughputLimit")
                && camera.is_writable(f)
                && has_choice(camera, f, "On")
        });
        self.caps.current_throughput = [
            "BslDeviceLinkCurrentThroughput",
            "DeviceLinkCurrentThroughput",
        ]
        .into_iter()
        .find(|f| camera.has(f));
        if camera.info.transport == Transport::GigE {
            let speed = read_any(camera, &["DeviceLinkSpeed"])
                .map(|(_, v)| v / 8.0)
                .or_else(|| read_any(camera, &["GevLinkSpeed"]).map(|(_, v)| v * 125_000.0));
            self.caps.link = Some(speed.filter(|v| *v > 0.0).unwrap_or(125e6));
        }
        self.caps.triggered = camera.get("TriggerMode").is_ok_and(|v| v == "On")
            && camera
                .get("TriggerSelector")
                .map_or(true, |v| v == "FrameStart");
        self.caps.color = camera
            .get("PixelFormat")
            .is_ok_and(|v| v.as_str().is_some_and(|s| !s.starts_with("Mono")));
        self.caps.video_mode = media && camera.has("VideoMode");
        let c = &self.caps;
        let strategy =
            if c.exposure_auto.is_some() && c.exposure.is_some() && c.exposure_limits.is_some() {
                "firmware"
            } else if c.exposure.is_some() || c.gain.is_some() {
                "software"
            } else if c.video_mode {
                "video-mode"
            } else {
                self.note("none", "no adjustable exposure, gain or frame rate");
                "none"
            };
        self.status.strategy = strategy.into();
        Ok(())
    }
    fn find<const N: usize>(
        &mut self,
        camera: &mut Camera,
        options: &[[&'static str; N]],
    ) -> Option<[&'static str; N]> {
        let found = options
            .iter()
            .copied()
            .find(|o| o.iter().all(|f| camera.has(f) && camera.is_writable(f)));
        if found.is_none()
            && let Some(o) = options.iter().find(|o| o.iter().all(|f| camera.has(f)))
        {
            self.note(
                o[0],
                format!("{} is not writable, so auto mode leaves it unchanged", o[0]),
            );
        }
        found
    }
    fn plan(&mut self, camera: &mut Camera) {
        self.stale = false;
        self.blocked.clear();
        // Keep ownership of earlier writes until manual/revert releases them.
        // A bandwidth or ROI replan must not silently stop managing VideoMode
        // or a selector whose current value still came from this controller.
        self.fw = Supervisor::default();
        if let Some(choice) = self.caps.gain_selector {
            self.apply(
                camera,
                "GainSelector",
                json!(choice),
                "gain in dB for all channels",
            );
            self.caps.gain_bounds = bounds(camera, self.caps.gain);
            if let Some(gain) = self.caps.gain
                && let Err(e) = self.read(camera, gain)
            {
                self.note("gain", format!("gain selector readback: {e:#}"));
            }
        }
        if self.caps.triggered {
            self.caps.trigger_period = self.period.map(|p| p * 1e6);
        }
        let c = self.caps.clone();
        let mut p = policy(&c, self.status.balance);
        if let Some(fps) = self.link_fps {
            let free = 1e6 / fps - margin(1e6 / fps);
            p.exposure_cap = p
                .exposure_cap
                .max(free.min(c.exposure_bounds.max.unwrap_or(f64::INFINITY)));
        }
        self.policy = p;
        match self.status.strategy.as_str() {
            "firmware" => self.plan_firmware(camera, &c, p),
            "software" => {
                self.limiter_off(camera, &c);
                for f in [c.exposure, c.gain].into_iter().flatten() {
                    self.own(f);
                }
                if let Err(e) = self.step(camera, 0.0, Instant::now()) {
                    self.note("exposure", format!("{e:#}"));
                }
            }
            _ => {}
        }
        self.plan_bandwidth(camera, &c, p);
        for (feature, value, reason) in std::mem::take(&mut self.failed) {
            if let Err(e) = self.write(camera, &feature, value, Some(reason)) {
                self.reject(&feature, &e);
            }
        }
        self.status.target_fps = match self.status.strategy.as_str() {
            "video-mode" => video_mode_fps(camera),
            _ => p.fps_target.and_then(finite),
        };
        self.status.exposure_limits_us = c.exposure.and(range(p.exposure_lower, p.exposure_cap));
        self.status.gain_limits_db = c.gain.and(range(p.gain_min, p.gain_cap));
        match c.forward_fps {
            Some(f) if p.fps_max < f => self.note(
                "forward",
                format!(
                    "camera delivers at most {:.1} fps; the encoder repeats frames",
                    p.fps_max
                ),
            ),
            _ => self.clear_note("forward"),
        }
        self.planned = true;
    }
    fn plan_firmware(&mut self, camera: &mut Camera, c: &Caps, p: Policy) {
        self.limiter_off(camera, c);
        if let Some(pair) = c.exposure_limits {
            let high = p.exposure_cap.max(p.exposure_lower);
            self.write_pair(
                camera,
                pair,
                p.exposure_lower,
                high,
                "exposure range for the balance",
            );
        }
        if let Some(pair) = c.gain_limits {
            self.write_pair(
                camera,
                pair,
                p.gain_min,
                p.gain_cap,
                "gain range for the balance",
            );
        }
        if let Some([f, v]) = c.profile {
            self.apply(camera, f, json!(v), "prefer exposure over gain");
        }
        if let Some(g) = c.gain {
            self.own(g);
            match (c.gain_auto, c.profile) {
                (Some(auto), Some(_)) => {
                    self.remember(camera, g);
                    self.apply(camera, auto, json!("Continuous"), "camera auto gain");
                }
                (auto, _) => {
                    if let Some(auto) = auto {
                        self.apply(camera, auto, json!("Off"), "Capturefab steps gain");
                    }
                    let start = match self.planned {
                        true => self.status.gain_db.unwrap_or(p.gain_min),
                        false => p.gain_min,
                    };
                    let start = start.min(p.gain_cap).max(p.gain_min);
                    self.apply(
                        camera,
                        g,
                        json!(start),
                        "gain within the range for the balance",
                    );
                }
            }
        }
        if let (Some(e), Some(auto)) = (c.exposure, c.exposure_auto) {
            self.own(e);
            self.remember(camera, e);
            self.apply(camera, auto, json!("Continuous"), "camera auto exposure");
        }
        if let (true, Some(auto)) = (c.color, c.white_auto) {
            if self.white_balance.is_none()
                && camera.has("BalanceRatio")
                && let Err(e) = self.write(
                    camera,
                    auto,
                    json!("Off"),
                    Some("hold white balance while reading its original values"),
                )
            {
                self.note(
                    "white-balance",
                    format!("auto white balance unchanged: {e:#}"),
                );
                return;
            }
            if let Err(e) = self.remember_white_balance(camera) {
                self.note(
                    "white-balance",
                    format!("auto white balance unchanged: {e:#}"),
                );
                return;
            }
            self.remember(camera, auto);
            self.apply(
                camera,
                auto,
                json!("Continuous"),
                "camera auto white balance",
            );
            if camera.has("BalanceRatio") {
                self.own("BalanceRatio");
            }
        }
    }
    fn remember_white_balance(&mut self, camera: &mut Camera) -> Result<()> {
        if self.white_balance.is_some() || !camera.has("BalanceRatio") {
            return Ok(());
        }
        if !camera.has("BalanceRatioSelector") {
            self.white_balance = Some(WhiteBalance {
                selector: None,
                ratios: vec![(None, camera.get("BalanceRatio")?)],
            });
            return Ok(());
        }
        let selector = camera.get("BalanceRatioSelector")?;
        let result = (|| -> Result<Vec<(Option<String>, Value)>> {
            let choices = camera.choices("BalanceRatioSelector")?;
            ensure!(
                !choices.is_empty(),
                "camera lists no white balance channels"
            );
            let mut ratios = Vec::new();
            for choice in choices {
                camera.set("BalanceRatioSelector", &choice)?;
                ratios.push((Some(choice), camera.get("BalanceRatio")?));
            }
            Ok(ratios)
        })();
        camera.set("BalanceRatioSelector", &text(&selector))?;
        self.white_balance = Some(WhiteBalance {
            selector: Some(selector),
            ratios: result?,
        });
        self.own("BalanceRatioSelector");
        Ok(())
    }
    fn write_pair(
        &mut self,
        camera: &mut Camera,
        [lower, upper]: [&'static str; 2],
        low: f64,
        high: f64,
        reason: &'static str,
    ) {
        let order = match camera.get(upper).ok().and_then(|v| v.as_f64()) {
            Some(u) if low > u => [(upper, high), (lower, low)],
            _ => [(lower, low), (upper, high)],
        };
        for (f, v) in order {
            self.apply(camera, f, json!(v), reason);
        }
    }
    fn limiter_off(&mut self, camera: &mut Camera, c: &Caps) {
        if let Some(f) = c.frame_rate_enable {
            let off = match f {
                "AcquisitionFrameRateMode" => json!("Off"),
                _ => json!(false),
            };
            self.apply(camera, f, off, "frame-rate limiter off");
        } else if let Some(f) = c.frame_rate
            && let Some(max) = bounds(camera, Some(f)).max
        {
            self.apply(camera, f, json!(max), "frame rate at the camera maximum");
        }
    }
    fn plan_bandwidth(&mut self, camera: &mut Camera, c: &Caps, p: Policy) {
        let previous = self.bandwidth.take();
        if c.triggered || camera.stats().is_none() {
            return;
        }
        let link = c.link.unwrap_or(125e6);
        let (feature, fps, limit, floor, cap, step) = if let Some(f) = c.throughput {
            if let Some(mode) = c.throughput_mode {
                self.apply(camera, mode, json!("On"), "throughput limit active");
            }
            let Some(limit) = camera.get(f).ok().and_then(|v| v.as_f64()) else {
                return;
            };
            let b = bounds(camera, Some(f));
            let floor = p.fps_floor * 1.05 * c.payload.unwrap_or(0.0);
            let cap = b.max.unwrap_or(f64::INFINITY).min(0.92 * link);
            (
                f,
                false,
                limit,
                b.min.unwrap_or(0.0).max(floor),
                cap,
                0.05 * link,
            )
        } else if let (Some(f), Some(enable)) = (c.frame_rate, c.frame_rate_enable) {
            self.own(enable);
            let limit = previous.as_ref().map_or(p.fps_max, |b| b.aimd.limit);
            let step = 0.05 * p.fps_max;
            (f, true, limit.min(p.fps_max), p.fps_floor, p.fps_max, step)
        } else {
            return;
        };
        let floor = floor.min(cap);
        let limit = limit.clamp(floor, cap);
        if !fps {
            self.apply(
                camera,
                feature,
                json!(limit),
                "throughput within the link budget",
            );
        } else if limit < cap {
            self.apply(camera, feature, json!(limit), "preserve bandwidth backoff");
            if let Some(enable) = c.frame_rate_enable {
                let on = match enable {
                    "AcquisitionFrameRateMode" => json!("On"),
                    _ => json!(true),
                };
                self.apply(camera, enable, on, "preserve bandwidth backoff");
            }
        }
        self.own(feature);
        let aimd = match previous {
            Some(b) if b.feature == feature => Aimd {
                limit,
                floor,
                cap,
                step,
                ..b.aimd
            },
            _ => Aimd::new(limit, floor, cap, step, self.jitter),
        };
        self.bandwidth = Some(Bandwidth {
            aimd,
            feature,
            fps,
            since: None,
            base: TransportStats::default(),
            frames: 0,
        });
    }
    fn apply(
        &mut self,
        camera: &mut Camera,
        feature: &'static str,
        value: Value,
        reason: &'static str,
    ) {
        self.own(feature);
        if self
            .write(camera, feature, value.clone(), Some(reason))
            .is_err()
        {
            self.failed.push((feature.into(), value, reason));
        }
    }
    fn own(&mut self, feature: &str) {
        if !self.manages(feature) {
            self.status.managed.push(feature.into());
        }
    }
    fn remember(&mut self, camera: &mut Camera, feature: &str) {
        if !self.originals.iter().any(|(f, _)| f == feature)
            && let Ok(v) = camera.get(feature)
        {
            self.originals.push((feature.into(), v));
        }
    }
    fn write(
        &mut self,
        camera: &mut Camera,
        feature: &str,
        target: Value,
        reason: Option<&str>,
    ) -> Result<Value> {
        self.own(feature);
        ensure!(
            !self.blocked.iter().any(|f| f == feature),
            "{feature} was rejected; auto mode retries it at the next plan"
        );
        let auto = match feature {
            "BalanceRatio" => Some("BalanceWhiteAuto"),
            f if self.caps.exposure == Some(f) => Some("ExposureAuto"),
            f if self.caps.gain == Some(f) => Some("GainAuto"),
            _ => None,
        };
        if let Some(auto) = auto.filter(|a| camera.has(a))
            && self.values.get(auto).is_none_or(|v| v != "Off")
        {
            let why = reason.unwrap_or("Capturefab sets the value itself");
            self.write(camera, auto, json!("Off"), Some(why))?;
        }
        let current = camera.get(feature)?;
        let integer = current.is_i64() || current.is_u64();
        for attempt in 0..2 {
            let value = match target.as_f64() {
                Some(v) => {
                    let mut b = camera.bounds(feature).unwrap_or_default();
                    b.inc = b.inc.or(integer.then_some(1.0));
                    let q = quantize(v, b, f64::round);
                    let same = |c: f64| (c - q).abs() <= 1e-9 * q.abs().max(1.0);
                    if current.as_f64().is_some_and(same) {
                        self.record(feature, current.clone());
                        return Ok(current);
                    }
                    json!(q)
                }
                None if current == target => {
                    self.record(feature, current.clone());
                    return Ok(current);
                }
                None => target.clone(),
            };
            if !self.originals.iter().any(|(f, _)| f == feature) {
                self.originals.push((feature.into(), current.clone()));
            }
            match camera.set(feature, &text(&value)) {
                Ok(()) => break,
                Err(e) if attempt == 0 && retryable(&e) => continue,
                Err(e) => return Err(e),
            }
        }
        self.writes += 1;
        let read = camera.get(feature)?;
        if let Some(reason) = reason {
            self.change(feature, Some(current), read.clone(), reason);
        }
        self.record(feature, read.clone());
        Ok(read)
    }
    fn record(&mut self, feature: &str, value: Value) {
        let number = value.as_f64().and_then(finite);
        if self.caps.exposure == Some(feature) {
            self.status.exposure_us = number;
        }
        if self.caps.gain == Some(feature) {
            self.status.gain_db = number;
        }
        self.values.insert(feature.into(), value);
    }
    fn read(&mut self, camera: &mut Camera, feature: &str) -> Result<f64> {
        let value = camera.get(feature)?;
        let number = value
            .as_f64()
            .and_then(finite)
            .with_context(|| format!("{feature} is not a finite number"))?;
        self.record(feature, value);
        Ok(number)
    }
    fn reject(&mut self, feature: &str, e: &anyhow::Error) {
        if retryable(e) {
            self.blocked.push(feature.into());
        }
        self.note(feature, format!("{feature}: {e:#}"));
    }
    fn restore(&mut self, camera: &mut Camera, feature: &str, value: &Value) -> Result<Value> {
        match feature {
            "VideoMode" => self.set_mode(camera, value.clone(), None),
            _ => self.write(camera, feature, value.clone(), None),
        }
    }
    fn set_mode(
        &mut self,
        camera: &mut Camera,
        mode: Value,
        reason: Option<&str>,
    ) -> Result<Value> {
        self.own("VideoMode");
        let old = camera.get("VideoMode")?;
        if old == mode {
            return Ok(old);
        }
        let streaming = camera.is_streaming();
        if streaming {
            camera.stop()?;
        }
        let result = self.write(camera, "VideoMode", mode, reason).and_then(|v| {
            if streaming {
                camera.start()?;
            }
            Ok(v)
        });
        if result.is_err() {
            camera.set("VideoMode", &text(&old))?;
            self.record("VideoMode", old);
            if streaming {
                camera
                    .start()
                    .context("restart with the previous video mode")?;
            }
        }
        if result.is_ok() {
            // The new source starts a fresh frame sequence at a different rate.
            // Discard the old mode's cadence and brightness convergence samples.
            self.period = None;
            self.restart(Instant::now());
            if self.status.strategy == "video-mode" {
                self.status.target_fps = video_mode_fps(camera);
            }
        }
        result
    }
    fn run(&mut self, camera: &mut Camera, now: Instant) -> Result<()> {
        if self.reprobe {
            self.probe(camera)?;
            self.reprobe = false;
            self.restart(now);
        }
        if self.caps.triggered
            && let Some(p) = self.period
            && self
                .caps
                .trigger_period
                .is_none_or(|t| (t / (p * 1e6) - 1.0).abs() > 0.1)
        {
            self.stale = true;
        }
        if self.stale {
            self.plan(camera);
        }
        if !self.streaming {
            return Ok(());
        }
        match self.status.strategy.as_str() {
            "software" => self.expose(camera, now)?,
            "firmware" => self.supervise(camera, now)?,
            "none" => self.set_state("limited"),
            _ if self.in_band >= 2 || self.unmetered => self.set_state("stable"),
            _ => {}
        }
        self.supervise_bandwidth(camera, now)
    }
    fn restart(&mut self, now: Instant) {
        self.last_frame = None;
        self.last_meter = None;
        self.samples.clear();
        self.in_band = 0;
        self.settle = None;
        self.correcting = false;
        self.status.brightness = None;
        self.fw.read = None;
        if let Some(b) = self.bandwidth.as_mut() {
            b.since = None;
        }
        self.quiet(now + SECOND);
        self.set_state(if self.streaming {
            "converging"
        } else {
            "waiting"
        });
    }
    fn band(&self) -> (f64, f64) {
        let e = self.status.exposure_us.unwrap_or(0.0);
        let q = match self.caps.exposure_bounds.inc {
            Some(inc) if e > 0.0 && e < self.policy.exposure_cap => (1.0 + inc / e).log2(),
            _ => self.caps.gain_bounds.inc.map_or(0.0, |inc| inc / 6.02),
        };
        ((0.6 * q).max(0.1), (0.6 * q).max(0.2))
    }
    fn expose(&mut self, camera: &mut Camera, now: Instant) -> Result<()> {
        if self.unmetered {
            self.limit(
                "metering",
                "brightness cannot be metered in this pixel format",
            );
            return Ok(());
        }
        self.clear_note("metering");
        let (settle, correct) = self.band();
        if self.in_band >= 2 {
            self.correcting = false;
            self.clear_note("dark");
            self.clear_note("bright");
            self.set_state("stable");
        }
        if self.samples.len() < 3 {
            return Ok(());
        }
        let mut sorted: Vec<f64> = self.samples.iter().copied().collect();
        sorted.sort_by(f64::total_cmp);
        let e = sorted[1];
        if e.abs() < settle || !(self.correcting || e.abs() > correct) {
            return Ok(());
        }
        self.correcting = true;
        self.step(camera, e, now)
    }
    fn step(&mut self, camera: &mut Camera, e: f64, now: Instant) -> Result<()> {
        let usable = |f: Option<&'static str>| f.filter(|f| !self.blocked.iter().any(|b| b == f));
        let (ef, gf) = (usable(self.caps.exposure), usable(self.caps.gain));
        let p = self.policy;
        let e_now = self.status.exposure_us.filter(|v| *v > 0.0).unwrap_or(1.0);
        let g_now = self.status.gain_db.unwrap_or(0.0);
        let (e_low, e_high) = match ef {
            Some(_) => (p.exposure_lower, p.exposure_cap.max(p.exposure_lower)),
            None => (e_now, e_now),
        };
        let (g_low, g_high) = match gf {
            Some(_) => (p.gain_min, p.gain_cap.max(p.gain_min)),
            None => (g_now, g_now),
        };
        let linear = |g: f64| 10f64.powf((g - g_low) / 20.0);
        let total = e_now * linear(g_now) * (0.6 * e).clamp(-2.0, 2.0).exp2();
        let round: fn(f64) -> f64 = match e {
            e if e > 0.0 => f64::ceil,
            e if e < 0.0 => f64::floor,
            _ => f64::round,
        };
        let e_new = match ef {
            Some(_) => quantize_range(total, self.caps.exposure_bounds, e_low, e_high, round),
            None => e_now,
        };
        let g_new = match gf {
            Some(_) => {
                let g = (g_low + 20.0 * (total / e_new).log10()).clamp(g_low, g_high);
                quantize_range(g, self.caps.gain_bounds, g_low, g_high, round)
            }
            None => g_now,
        };
        let realized = (e_new * linear(g_new) / (e_now * linear(g_now))).log2();
        let changed = (e_new - e_now).abs() > 1e-6 * e_now || (g_new - g_now).abs() > 1e-6;
        if e != 0.0 && (!changed || realized * e < 0.0 || realized.abs() > 2.0 * e.abs()) {
            match (changed, e > 0.0) {
                (false, true) => self.limit("dark", DARK),
                (false, false) => self.limit("bright", BRIGHT),
                _ => self.set_state("limited"),
            }
            return Ok(());
        }
        if !changed {
            return Ok(());
        }
        self.clear_note("dark");
        self.clear_note("bright");
        if e < 0.0 {
            self.set_gain(camera, gf, g_new)?;
            self.set_exposure(camera, ef, e_new)?;
        } else {
            self.set_exposure(camera, ef, e_new)?;
            self.set_gain(camera, gf, g_new)?;
        }
        let wait = e_now.max(e_new) / 1e6 + 2.0 * self.period.unwrap_or(0.0);
        self.settle = Some((now + Duration::from_secs_f64(wait), self.frames + 3));
        self.samples.clear();
        self.in_band = 0;
        self.set_state("converging");
        Ok(())
    }
    fn set_gain(&mut self, camera: &mut Camera, feature: Option<&str>, value: f64) -> Result<()> {
        if let Some(f) = feature {
            self.write(
                camera,
                f,
                json!(value),
                Some("gain adjusted for brightness"),
            )?;
        }
        Ok(())
    }
    fn set_exposure(
        &mut self,
        camera: &mut Camera,
        feature: Option<&str>,
        value: f64,
    ) -> Result<()> {
        let Some(f) = feature else {
            return Ok(());
        };
        let old = self.status.exposure_us.unwrap_or(value);
        let rate = self
            .caps
            .frame_rate
            .filter(|_| self.caps.frame_rate_enable.is_none());
        if let Some(r) = rate
            && value > old
            && self
                .values
                .get(r)
                .and_then(Value::as_f64)
                .is_some_and(|fps| fps > 1e6 / value)
        {
            self.write(camera, r, json!(1e6 / value), None)?;
        }
        self.write(
            camera,
            f,
            json!(value),
            Some("exposure adjusted for brightness"),
        )?;
        if let Some(r) = rate
            && value < old
            && let Some(max) = bounds(camera, Some(r)).max
        {
            self.write(camera, r, json!(max), None)?;
        }
        Ok(())
    }
    fn supervise(&mut self, camera: &mut Camera, now: Instant) -> Result<()> {
        let Some(ef) = self.caps.exposure else {
            return Ok(());
        };
        if self
            .fw
            .read
            .is_some_and(|(t, ..)| now.saturating_duration_since(t) < Duration::from_millis(500))
        {
            return Ok(());
        }
        let e = self.read(camera, ef)?;
        let g = match self.caps.gain {
            Some(f) => self.read(camera, f)?,
            None => 0.0,
        };
        let settled = self
            .fw
            .read
            .is_some_and(|(_, pe, pg)| (e - pe).abs() < 0.02 * pe && (g - pg).abs() < 0.2);
        self.fw.read = Some((now, e, g));
        if self.status.state != "limited" {
            self.set_state(if settled { "stable" } else { "converging" });
        }
        let quiet = self.written.is_none_or(|(t, f)| {
            now.saturating_duration_since(t) >= SECOND && self.frames >= f + 20
        });
        if !quiet
            || self
                .fw
                .decided
                .is_some_and(|t| now.saturating_duration_since(t) < SECOND)
        {
            return Ok(());
        }
        self.fw.decided = Some(now);
        let p = self.policy;
        let high = e >= 0.97 * p.exposure_cap;
        let target = self.status.brightness.zip(self.caps.target);
        let capped = self.caps.gain.is_none() || g >= p.gain_cap - 0.1;
        let low = e <= 1.03 * p.exposure_lower && g <= p.gain_min + 0.1 && self.p99 >= 0.98;
        let dark = high && target.is_none_or(|(b, t)| b < 0.7 * t);
        let off = target.is_some_and(|(b, t)| (b / t).log2().abs() > 1.0);
        self.fw.high = if high && capped { self.fw.high + 1 } else { 0 };
        self.fw.low = if low { self.fw.low + 1 } else { 0 };
        self.fw.dark = if dark { self.fw.dark + 1 } else { 0 };
        self.fw.stall = (off && settled && !high && !low).then(|| self.fw.stall.unwrap_or(now));
        if self
            .fw
            .stall
            .is_some_and(|t| now.saturating_duration_since(t) >= 3 * SECOND)
        {
            return self.fall_back(camera);
        }
        if !self.fw.calibrated
            && high
            && settled
            && e <= 1.03 * p.exposure_cap
            && !self.bound
            && let (Some(fps), Some(f)) = (p.fps_target, self.caps.resulting_fps)
        {
            let resulting = self.read(camera, f)?;
            let link = self
                .bandwidth
                .as_ref()
                .filter(|b| !b.fps)
                .zip(self.caps.payload.filter(|p| *p > 0.0))
                .map(|(b, payload)| b.aimd.limit / (1.05 * payload));
            if link.is_some_and(|l| resulting >= 0.8 * l) {
                return Ok(());
            }
            self.fw.calibrated = true;
            if resulting > 0.0
                && resulting < 0.95 * fps
                && let Some([_, upper]) = self.caps.exposure_limits
            {
                self.policy.exposure_cap = (self.policy.exposure_cap
                    - (1e6 / resulting - 1e6 / fps))
                    .max(p.exposure_lower);
                self.status.exposure_limits_us = range(p.exposure_lower, self.policy.exposure_cap);
                let cap = json!(self.policy.exposure_cap);
                let why = "exposure limit fitted to the measured frame time";
                return self.write(camera, upper, cap, Some(why)).map(drop);
            }
        }
        let stepped = self.caps.gain.filter(|_| self.caps.profile.is_none());
        if let Some(f) = stepped
            && self.fw.dark >= 2
            && g < p.gain_cap - 0.1
        {
            let raised = json!((g + 3.0).min(p.gain_cap));
            return self
                .write(
                    camera,
                    f,
                    raised,
                    Some("gain raised: exposure is at its limit"),
                )
                .map(drop);
        }
        if let Some(f) = stepped
            && e < 0.5 * p.exposure_cap
            && g > p.gain_min + 0.1
        {
            let lowered = json!((g - 3.0).max(p.gain_min));
            return self
                .write(
                    camera,
                    f,
                    lowered,
                    Some("gain lowered: exposure has headroom"),
                )
                .map(drop);
        }
        if self.fw.low >= 2 {
            let min = self.caps.exposure_bounds.min.unwrap_or(0.0);
            if let Some([lower, _]) = self.caps.exposure_limits
                && !self.fw.lowered
                && self
                    .values
                    .get(lower)
                    .and_then(Value::as_f64)
                    .is_some_and(|v| v > min)
            {
                self.fw.lowered = true;
                let why = "exposure lower limit at the camera minimum";
                return self.write(camera, lower, json!(min), Some(why)).map(drop);
            }
            self.limit("bright", BRIGHT);
            return Ok(());
        }
        if self.fw.high >= 2 {
            self.limit("dark", DARK);
            return Ok(());
        }
        self.clear_note("dark");
        self.clear_note("bright");
        self.set_state(if settled { "stable" } else { "converging" });
        Ok(())
    }
    fn fall_back(&mut self, camera: &mut Camera) -> Result<()> {
        self.status.strategy = "software".into();
        self.note(
            "firmware",
            "camera auto exposure missed its target; Capturefab adjusts exposure",
        );
        self.correcting = true;
        self.samples.clear();
        for f in [self.caps.exposure_auto, self.caps.gain_auto]
            .into_iter()
            .flatten()
        {
            self.write(
                camera,
                f,
                json!("Off"),
                Some("camera auto exposure stalled"),
            )?;
        }
        Ok(())
    }
    fn supervise_bandwidth(&mut self, camera: &mut Camera, now: Instant) -> Result<()> {
        let (Some(stats), Some(bw)) = (camera.stats(), self.bandwidth.as_mut()) else {
            return Ok(());
        };
        let Some(since) = bw.since else {
            (bw.since, bw.base, bw.frames) = (Some(now), stats, self.frames);
            return Ok(());
        };
        if now.saturating_duration_since(since) < 2 * SECOND {
            return Ok(());
        }
        let lost = stats.lost_frames.saturating_sub(bw.base.lost_frames);
        let frames = self.frames.saturating_sub(bw.frames);
        let resent = stats
            .resend_requested
            .saturating_sub(bw.base.resend_requested);
        let packets = stats
            .packets_received
            .saturating_sub(bw.base.packets_received);
        (bw.since, bw.base, bw.frames) = (Some(now), stats, self.frames);
        let quiet = self.quiet_until.is_some_and(|t| now < t);
        let starved = self
            .last_frame
            .is_none_or(|t| now.saturating_duration_since(t) >= SECOND);
        let loss = !quiet
            && (lost >= 2
                || (lost > 0 && lost * 100 >= frames + lost)
                || resent * 200 > packets
                || starved);
        let fps = self.period.filter(|p| *p > 0.0).map_or(0.0, |p| 1.0 / p);
        let bytes = 1.05 * self.caps.payload.unwrap_or(0.0);
        let usage = match bw.fps {
            true => fps,
            false => self
                .caps
                .current_throughput
                .and_then(|f| camera.get(f).ok()?.as_f64())
                .unwrap_or(fps * bytes),
        };
        let bound = usage >= 0.85 * bw.aimd.limit;
        let settled = matches!(self.status.state.as_str(), "stable" | "limited");
        let change = bw
            .aimd
            .step(now, usage, loss, !quiet && !loss && bound && settled);
        let (feature, limiter, limit, cap) = (bw.feature, bw.fps, bw.aimd.limit, bw.aimd.cap);
        let at_floor = limit <= bw.aimd.floor;
        let free = !limiter && bound && bytes > 0.0 && (limit >= cap || bw.aimd.holding(now));
        self.bound = bound && !limiter;
        let link_fps = free.then(|| limit / bytes);
        if match (link_fps, self.link_fps) {
            (Some(a), Some(b)) => (a / b - 1.0).abs() > 0.1,
            (a, b) => a.is_some() != b.is_some(),
        } {
            self.link_fps = link_fps;
            self.stale = true;
        }
        if loss && at_floor {
            self.note(
                "bandwidth",
                "loss persists at minimum bandwidth (host CPU, NIC or receive buffer)",
            );
        } else if !loss {
            self.clear_note("bandwidth");
        }
        let Some(limit) = change else {
            return Ok(());
        };
        let why = match loss {
            true => "bandwidth reduced after frame loss",
            false => "bandwidth raised: no frame loss",
        };
        if !limiter {
            return self
                .write(camera, feature, json!(limit), Some(why))
                .map(drop);
        }
        let enable = self
            .caps
            .frame_rate_enable
            .context("frame-rate limiter has no enable feature")?;
        let on = limit < cap;
        if on {
            self.write(camera, feature, json!(limit), Some(why))?;
        }
        let value = match enable {
            "AcquisitionFrameRateMode" => json!(if on { "On" } else { "Off" }),
            _ => json!(on),
        };
        self.write(camera, enable, value, Some(why)).map(drop)
    }
    fn limit(&mut self, key: &str, text: &str) {
        self.note(key, text);
        self.set_state("limited");
    }
    fn note(&mut self, key: &str, text: impl Into<String>) {
        let text = text.into();
        if self.notes.get(key) != Some(&text)
            && (self.notes.len() < 8 || self.notes.contains_key(key))
        {
            self.notes.insert(key.into(), text);
            self.status.notes = self.notes.values().cloned().collect();
        }
    }
    fn clear_note(&mut self, key: &str) {
        if self.notes.remove(key).is_some() {
            self.status.notes = self.notes.values().cloned().collect();
        }
    }
    fn change(&mut self, feature: &str, from: Option<Value>, to: Value, reason: &str) {
        if from.as_ref() != Some(&to) {
            self.status.changes.push(AutoChange {
                time: clock(),
                feature: feature.into(),
                from,
                to,
                reason: reason.into(),
            });
            if self.status.changes.len() > 32 {
                self.status.changes.remove(0);
            }
        }
    }
    fn set_state(&mut self, state: &str) {
        if self.status.state != state {
            self.status.state = state.into();
        }
    }
    fn taint(&mut self, now: Instant) {
        if let Some(b) = self.bandwidth.as_mut() {
            b.since = None;
        }
        self.quiet(now + Duration::from_millis(2500));
    }
    fn account(&mut self, now: Instant) {
        if self.writes != self.counted {
            self.counted = self.writes;
            self.written = Some((now, self.frames));
            self.quiet(now + SECOND);
        }
    }
    fn quiet(&mut self, until: Instant) {
        self.quiet_until = Some(self.quiet_until.map_or(until, |t| t.max(until)));
    }
}

pub fn hold(camera: &mut Camera) -> Result<()> {
    let mut result = Ok(());
    for f in AUTOS {
        if camera.has(f) && camera.is_writable(f) {
            let off = camera.get(f).and_then(|v| match v == "Off" {
                true => Ok(()),
                false => camera.set(f, "Off"),
            });
            result = result.and(off);
        }
    }
    result
}

fn bounds(camera: &mut Camera, feature: Option<&str>) -> Bounds {
    feature
        .and_then(|f| camera.bounds(f).ok())
        .unwrap_or_default()
}

fn has_choice(camera: &mut Camera, feature: &str, choice: &str) -> bool {
    camera
        .choices(feature)
        .is_ok_and(|c| c.iter().any(|v| v == choice))
}

fn read_any(camera: &mut Camera, names: &[&'static str]) -> Option<(&'static str, f64)> {
    names.iter().copied().find_map(|n| {
        camera.has(n).then_some(())?;
        Some((n, finite(camera.get(n).ok()?.as_f64()?)?))
    })
}

fn video_mode_fps(camera: &mut Camera) -> Option<f64> {
    let mode = camera.get("VideoMode").ok()?;
    let (_, fps) = mode.as_str()?.split_once('@')?;
    let fps = fps.parse::<f64>().ok()?;
    (fps.is_finite() && fps > 0.0).then_some(fps)
}

fn quantize(v: f64, b: Bounds, round: fn(f64) -> f64) -> f64 {
    let (low, high) = (b.min.unwrap_or(f64::MIN), b.max.unwrap_or(f64::MAX));
    let v = v.max(low).min(high);
    let Some(inc) = b.inc.filter(|i| *i > 0.0) else {
        return v;
    };
    let base = b.min.unwrap_or(0.0);
    let q = base + round((v - base) / inc) * inc;
    if q > high {
        q - inc
    } else if q < low {
        q + inc
    } else {
        q
    }
}

fn quantize_range(v: f64, bounds: Bounds, low: f64, high: f64, round: fn(f64) -> f64) -> f64 {
    let low = quantize(low, bounds, f64::ceil);
    let high = quantize(high, bounds, f64::floor).max(low);
    quantize(v.clamp(low, high), bounds, round).clamp(low, high)
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) if n.is_f64() => n.as_f64().unwrap_or_default().to_string(),
        v => v.to_string(),
    }
}

fn retryable(e: &anyhow::Error) -> bool {
    let message = format!("{e:#}");
    ["violates", "requires increment", "is locked", "GVCP status"]
        .iter()
        .any(|k| message.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Backend, CameraInfo, MONO8, RGB8, RegisterIo};
    use std::sync::{Arc, Mutex};

    fn frame(pixel_format: u32, width: u32, height: u32, data: Vec<u8>) -> Frame {
        Frame {
            id: 1,
            width,
            height,
            pixel_format,
            timestamp_ns: 0,
            data,
        }
    }
    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-6 * b.abs().max(1.0)
    }

    #[test]
    fn meter_handles_formats_without_bayer_aliasing() {
        let mono = meter(&frame(MONO8, 4, 4, vec![128; 16])).unwrap();
        assert!(near(mono.mean, 128.0 / 255.0) && near(mono.p99, 128.0 / 255.0));
        let rgb = meter(&frame(RGB8, 2, 1, vec![255, 0, 0, 255, 0, 0])).unwrap();
        assert!(near(rgb.mean, 0.25) && near(rgb.p99, 1.0));
        let red_sites = (0..512 * 512)
            .map(|i| {
                if i / 512 % 2 == 0 && i % 2 == 0 {
                    255
                } else {
                    0
                }
            })
            .collect();
        let bayer = meter(&frame(0x0108_0009, 512, 512, red_sites)).unwrap();
        assert!(near(bayer.mean, 0.25) && near(bayer.p99, 1.0));
        let mono12 = [4095u16, 0]
            .repeat(4)
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let deep = meter(&frame(0x0110_0005, 4, 2, mono12)).unwrap();
        assert!(near(deep.mean, 0.5) && near(deep.p99, 1.0));
        assert_eq!(
            meter(&frame(MONO8, 2, 2, vec![0; 4])).unwrap().mean,
            1.0 / 1024.0
        );
        assert!(meter(&frame(MONO8, 4, 4, vec![0; 15])).is_none());
        assert!(meter(&frame(0x010c_0047, 4, 4, vec![0; 24])).is_none());
    }

    #[test]
    fn policy_trades_frame_rate_for_exposure_and_gain() {
        let caps = Caps {
            exposure: Some("ExposureTime"),
            exposure_bounds: Bounds {
                min: Some(19.0),
                max: Some(1e7),
                inc: Some(1.0),
            },
            gain: Some("Gain"),
            gain_bounds: Bounds {
                min: Some(0.0),
                max: Some(48.0),
                inc: None,
            },
            sensor_fps: Some(1e6 / 5925.0),
            link: Some(125e6),
            payload: Some(2_304_000.0),
            ..Default::default()
        };
        let fps_max = 0.92 * 125e6 / (1.05 * 2_304_000.0);
        let [quality, middle, speed] = [0.0, 0.5, 1.0].map(|b| policy(&caps, b));
        assert!(near(quality.fps_max, fps_max) && near(quality.fps_floor, fps_max / 4.0));
        assert!(near(quality.fps_target.unwrap(), fps_max / 4.0));
        assert!(near(middle.fps_target.unwrap(), fps_max / 2.0));
        assert!(near(speed.fps_target.unwrap(), fps_max));
        let period = 1e6 / fps_max;
        assert!(near(speed.exposure_cap, period - 0.02 * period));
        assert!(near(quality.exposure_cap, 4.0 * period * 0.98));
        assert_eq!(
            [quality.gain_cap, middle.gain_cap, speed.gain_cap],
            [6.0, 15.0, 24.0]
        );
        assert_eq!(quality.exposure_lower, 19.0);
        let forwarding = policy(
            &Caps {
                forward_fps: Some(5.0),
                ..caps.clone()
            },
            1.0,
        );
        assert_eq!(
            (forwarding.fps_max, forwarding.fps_target),
            (5.0, Some(5.0))
        );
        let triggered = Caps {
            triggered: true,
            trigger_period: Some(50_000.0),
            ..caps
        };
        let triggered = policy(&triggered, 0.5);
        assert_eq!(
            (triggered.fps_target, triggered.exposure_cap),
            (None, 49_000.0)
        );
        assert_eq!(policy(&Caps::default(), 1.0).fps_max, 10_000.0);
    }

    #[test]
    fn mode_score_prefers_pixels_for_quality_and_rate_for_speed() {
        let modes = [
            (1920.0 * 1080.0, 30.0),
            (1280.0 * 720.0, 60.0),
            (640.0 * 480.0, 120.0),
        ];
        let best = |b: f64| {
            let score = |m: &(f64, f64)| mode_score(m.0, m.1, 1920.0 * 1080.0, 120.0, b);
            *modes
                .iter()
                .max_by(|x, y| score(x).total_cmp(&score(y)))
                .unwrap()
        };
        assert_eq!((best(0.0), best(1.0)), (modes[0], modes[2]));
        assert_eq!(mode_score(640.0, 30.0, 640.0, 30.0, 0.5), 0.0);
        assert_eq!(mode_score(320.0, 15.0, 640.0, 30.0, 0.5), -1.0);
    }

    #[test]
    fn aimd_slow_starts_cuts_holds_and_raises_additively() {
        let t = Instant::now();
        let at = |s: u64| t + Duration::from_secs(s);
        let mut a = Aimd::new(40e6, 10e6, 115e6, 6.25e6, 0.0);
        assert_eq!(a.step(at(0), 40e6, false, true), Some(50e6));
        assert_eq!(a.step(at(2), 50e6, false, false), None);
        assert!(
            a.step(at(4), 45e6, true, false)
                .is_some_and(|v| near(v, 36e6))
        );
        assert!(a.holding(at(13)));
        assert_eq!(a.step(at(13), 36e6, false, true), None);
        assert!(
            a.step(at(14), 36e6, false, true)
                .is_some_and(|v| near(v, 42.25e6))
        );
        assert!(
            a.step(at(30), 40e6, true, false)
                .is_some_and(|v| near(v, 32e6))
        );
        assert_eq!(a.step(at(49), 32e6, false, true), None);
        assert!(
            a.step(at(50), 32e6, false, true)
                .is_some_and(|v| near(v, 38.25e6))
        );
        assert_eq!(a.step(at(60), 1e6, true, false), Some(10e6));
        assert_eq!(a.step(at(400), 10e6, true, false), None);
        assert!(!a.holding(at(410)));
        let mut capped = Aimd::new(110e6, 10e6, 115e6, 6.25e6, 0.0);
        assert_eq!(capped.step(at(0), 110e6, false, true), Some(115e6));
        assert_eq!(capped.step(at(2), 115e6, false, true), None);
    }

    const EXPOSURE: u64 = 0x00;
    const GAIN: u64 = 0x08;
    const EXPOSURE_AUTO: u64 = 0x10;
    const GAIN_AUTO: u64 = 0x14;
    const WHITE_AUTO: u64 = 0x18;
    const LOWER: u64 = 0x20;
    const UPPER: u64 = 0x28;
    const GAIN_LOWER: u64 = 0x30;
    const GAIN_UPPER: u64 = 0x38;
    const PROFILE: u64 = 0x40;
    const BRIGHTNESS: u64 = 0x48;
    const ENABLE: u64 = 0x50;
    const RATE: u64 = 0x58;
    const RESULTING: u64 = 0x60;
    const THROUGHPUT: u64 = 0x68;
    const MODE: u64 = 0x6c;
    const CURRENT: u64 = 0x70;
    const PAYLOAD: u64 = 0x74;
    const PIXEL: u64 = 0x78;
    const READOUT: u64 = 0x80;
    const ACQUISITION: u64 = 0x88;

    fn xml(profile: bool) -> String {
        let float = |n: &str, a: u64, bounds: &str, access: &str| {
            format!(
                "<Float Name='{n}'><pValue>{n}Reg</pValue>{bounds}</Float><FloatReg Name='{n}Reg'><Address>{a}</Address><Length>8</Length><AccessMode>{access}</AccessMode></FloatReg>"
            )
        };
        let int = |n: &str, a: u64, bounds: &str, access: &str| {
            format!(
                "<Integer Name='{n}'><pValue>{n}Reg</pValue>{bounds}</Integer><IntReg Name='{n}Reg'><Address>{a}</Address><Length>4</Length><AccessMode>{access}</AccessMode></IntReg>"
            )
        };
        let enumeration = |n: &str, a: u64, entries: &[(&str, u32)]| {
            let entries: String = entries
                .iter()
                .map(|(e, v)| format!("<EnumEntry Name='{e}'><Value>{v}</Value></EnumEntry>"))
                .collect();
            format!(
                "<Enumeration Name='{n}'>{entries}<pValue>{n}Reg</pValue></Enumeration><IntReg Name='{n}Reg'><Address>{a}</Address><Length>4</Length><AccessMode>RW</AccessMode></IntReg>"
            )
        };
        let autos = [("Off", 0), ("Once", 1), ("Continuous", 2)];
        let mut body = [
            float("ExposureTime", EXPOSURE, "<Min>19</Min><Max>10000000</Max>", "RW"),
            float("Gain", GAIN, "<Min>0</Min><Max>48</Max>", "RW"),
            enumeration("ExposureAuto", EXPOSURE_AUTO, &autos),
            enumeration("GainAuto", GAIN_AUTO, &autos),
            enumeration("BalanceWhiteAuto", WHITE_AUTO, &autos),
            float("AutoExposureTimeLowerLimit", LOWER, "<Min>1</Min><pMax>AutoExposureTimeUpperLimit</pMax>", "RW"),
            float("AutoExposureTimeUpperLimit", UPPER, "<pMin>AutoExposureTimeLowerLimit</pMin><Max>10000000</Max>", "RW"),
            float("AutoGainLowerLimit", GAIN_LOWER, "<Min>0</Min><Max>48</Max>", "RW"),
            float("AutoGainUpperLimit", GAIN_UPPER, "<Min>0</Min><Max>48</Max>", "RW"),
            float("AutoTargetBrightness", BRIGHTNESS, "<Min>0</Min><Max>1</Max>", "RW"),
            format!("<Boolean Name='AcquisitionFrameRateEnable'><pValue>EnableReg</pValue></Boolean><IntReg Name='EnableReg'><Address>{ENABLE}</Address><Length>4</Length><AccessMode>RW</AccessMode></IntReg>"),
            float("AcquisitionFrameRate", RATE, "<Min>0.1</Min><Max>1000000</Max>", "RW"),
            float("BslResultingAcquisitionFrameRate", RESULTING, "", "RO"),
            int("DeviceLinkThroughputLimit", THROUGHPUT, "<Min>1923077</Min><Max>125000000</Max><Inc>1</Inc>", "RW"),
            enumeration("DeviceLinkThroughputLimitMode", MODE, &[("On", 0)]),
            int("BslDeviceLinkCurrentThroughput", CURRENT, "", "RO"),
            int("PayloadSize", PAYLOAD, "", "RO"),
            enumeration("PixelFormat", PIXEL, &[("Mono8", MONO8), ("BayerRG8", 0x0108_0009)]),
            float("SensorReadoutTime", READOUT, "", "RO"),
            format!("<Command Name='AcquisitionStart'><pValue>AcqReg</pValue><CommandValue>1</CommandValue></Command><Command Name='AcquisitionStop'><pValue>AcqReg</pValue><CommandValue>0</CommandValue></Command><IntReg Name='AcqReg'><Address>{ACQUISITION}</Address><Length>4</Length><AccessMode>RW</AccessMode></IntReg>"),
        ]
        .concat();
        if profile {
            body += &enumeration(
                "AutoFunctionProfile",
                PROFILE,
                &[("MinimizeGain", 0), ("MinimizeExposureTime", 1)],
            );
        }
        format!("<RegisterDescription>{body}</RegisterDescription>")
    }

    #[derive(Clone)]
    struct Fake {
        memory: Arc<Mutex<Vec<u8>>>,
        writes: Arc<Mutex<Vec<u64>>>,
        stats: Arc<Mutex<Option<TransportStats>>>,
        xml: String,
    }
    impl Fake {
        fn new(profile: bool) -> Self {
            let fake = Self {
                memory: Arc::new(Mutex::new(vec![0; 0x100])),
                writes: Arc::default(),
                stats: Arc::default(),
                xml: xml(profile),
            };
            for (a, v) in [
                (EXPOSURE, 3982.0),
                (GAIN, 13.3),
                (LOWER, 500.0),
                (UPPER, 10_000.0),
                (GAIN_UPPER, 24.0),
                (BRIGHTNESS, 0.5),
                (RATE, 1.0),
                (RESULTING, 28.1468),
                (READOUT, 5925.0),
            ] {
                fake.set_f64(a, v);
            }
            for (a, v) in [
                (PROFILE, 1),
                (ENABLE, 1),
                (THROUGHPUT, 75_748_621),
                (CURRENT, 68_134_091),
                (PAYLOAD, 2_304_000),
                (PIXEL, 0x0108_0009),
            ] {
                fake.set_u32(a, v);
            }
            fake
        }
        fn camera(&self) -> Camera {
            let info = CameraInfo {
                id: "fake".into(),
                transport: Transport::GigE,
                vendor: "Basler".into(),
                model: "ace 2".into(),
                serial: "12345678".into(),
                address: None,
            };
            Camera::from_backend(info, Box::new(self.clone())).unwrap()
        }
        fn f64(&self, a: u64) -> f64 {
            let m = self.memory.lock().unwrap();
            f64::from_le_bytes(m[a as usize..a as usize + 8].try_into().unwrap())
        }
        fn u32(&self, a: u64) -> u32 {
            let m = self.memory.lock().unwrap();
            u32::from_le_bytes(m[a as usize..a as usize + 4].try_into().unwrap())
        }
        fn set_f64(&self, a: u64, v: f64) {
            self.memory.lock().unwrap()[a as usize..a as usize + 8]
                .copy_from_slice(&v.to_le_bytes());
        }
        fn set_u32(&self, a: u64, v: u32) {
            self.memory.lock().unwrap()[a as usize..a as usize + 4]
                .copy_from_slice(&v.to_le_bytes());
        }
        fn set_lost(&self, lost_frames: u64) {
            *self.stats.lock().unwrap() = Some(TransportStats {
                lost_frames,
                ..Default::default()
            });
        }
    }
    impl RegisterIo for Fake {
        fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
            Ok(self.memory.lock().unwrap()[address as usize..address as usize + length].to_vec())
        }
        fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
            self.writes.lock().unwrap().push(address);
            self.memory.lock().unwrap()[address as usize..address as usize + data.len()]
                .copy_from_slice(data);
            Ok(())
        }
    }
    impl Backend for Fake {
        fn xml(&mut self) -> Result<String> {
            Ok(self.xml.clone())
        }
        fn start(&mut self, _: usize) -> Result<()> {
            Ok(())
        }
        fn next_frame(&mut self, _: Duration) -> Result<Frame> {
            anyhow::bail!("frames are supplied by the test")
        }
        fn stop(&mut self) -> Result<()> {
            Ok(())
        }
        fn stats(&self) -> Option<TransportStats> {
            self.stats.lock().unwrap().clone()
        }
    }

    fn drive(
        auto: &mut AutoController,
        camera: &mut Camera,
        now: &mut Instant,
        seconds: f64,
        level: u8,
    ) {
        for i in 0..(seconds * 50.0) as u64 {
            *now += Duration::from_millis(20);
            let mut f = frame(MONO8, 16, 16, vec![level; 256]);
            f.id = (65_530 + i) % 65_535 + 1;
            auto.observe(&f, *now);
            auto.tick(camera, *now);
        }
    }

    #[test]
    fn firmware_plan_writes_in_order_and_applies_the_balance() {
        let fake = Fake::new(true);
        fake.set_lost(0);
        let mut camera = fake.camera();
        let auto = AutoController::enter(&mut camera, 0.5).unwrap();
        assert_eq!(
            *fake.writes.lock().unwrap(),
            [
                ENABLE,
                LOWER,
                UPPER,
                GAIN_UPPER,
                PROFILE,
                GAIN_AUTO,
                EXPOSURE_AUTO,
                WHITE_AUTO
            ]
        );
        let p = policy(&auto.caps, 0.5);
        assert!(near(
            p.fps_target.unwrap(),
            0.92 * 125e6 / (1.05 * 2_304_000.0) / 2.0
        ));
        assert_eq!(fake.u32(ENABLE), 0);
        assert_eq!(fake.f64(LOWER), 19.0);
        assert!(near(fake.f64(UPPER), p.exposure_cap));
        assert_eq!((fake.f64(GAIN_LOWER), fake.f64(GAIN_UPPER)), (0.0, 15.0));
        assert_eq!(
            [PROFILE, GAIN_AUTO, EXPOSURE_AUTO, WHITE_AUTO].map(|a| fake.u32(a)),
            [0, 2, 2, 2]
        );
        let status = auto.status();
        assert_eq!(
            (status.strategy.as_str(), status.state.as_str()),
            ("firmware", "waiting")
        );
        assert_eq!(status.gain_limits_db, Some([0.0, 15.0]));
        for f in [
            "ExposureTime",
            "Gain",
            "ExposureAuto",
            "AutoExposureTimeUpperLimit",
            "DeviceLinkThroughputLimit",
        ] {
            assert!(auto.manages(f), "{f}");
        }
        assert_eq!(status.changes[0].feature, "AcquisitionFrameRateEnable");
        assert_eq!(status.changes.last().unwrap().feature, "auto");
        assert!(
            status
                .changes
                .iter()
                .all(|c| c.from.as_ref() != Some(&c.to))
        );
    }

    #[test]
    fn limit_pairs_are_written_in_an_order_that_stays_valid() {
        let fake = Fake::new(true);
        let mut camera = fake.camera();
        let mut auto = AutoController::enter(&mut camera, 0.5).unwrap();
        let pair = ["AutoExposureTimeLowerLimit", "AutoExposureTimeUpperLimit"];
        for (low, high) in [(50_000.0, 60_000.0), (100.0, 200.0)] {
            fake.writes.lock().unwrap().clear();
            auto.write_pair(&mut camera, pair, low, high, "test");
            assert!(auto.failed.is_empty());
            assert_eq!((fake.f64(LOWER), fake.f64(UPPER)), (low, high));
            let first = if low > 1000.0 { UPPER } else { LOWER };
            assert_eq!(fake.writes.lock().unwrap()[0], first);
        }
    }

    #[test]
    fn supervisor_reports_pinned_exposure_and_recovers() {
        let fake = Fake::new(true);
        let mut camera = fake.camera();
        let mut auto = AutoController::enter(&mut camera, 0.5).unwrap();
        camera.start().unwrap();
        let mut now = Instant::now();
        fake.set_f64(EXPOSURE, auto.policy.exposure_cap);
        fake.set_f64(GAIN, 15.0);
        drive(&mut auto, &mut camera, &mut now, 3.5, 20);
        assert_eq!(auto.status().state, "limited");
        assert!(auto.status().notes.contains(&DARK.to_string()));
        fake.set_f64(EXPOSURE, 19.0);
        fake.set_f64(GAIN, 0.0);
        drive(&mut auto, &mut camera, &mut now, 3.0, 255);
        assert_eq!(auto.status().state, "limited");
        assert!(auto.status().notes.contains(&BRIGHT.to_string()));
        fake.set_f64(EXPOSURE, 5000.0);
        drive(&mut auto, &mut camera, &mut now, 2.5, 120);
        assert_eq!(auto.status().state, "stable");
        assert!(auto.status().notes.is_empty());
        assert!(auto.converged());
    }

    #[test]
    fn exposure_fit_waits_for_firmware_to_settle_at_the_cap() {
        let fake = Fake::new(true);
        fake.set_lost(0);
        fake.set_u32(CURRENT, 20_000_000);
        let mut camera = fake.camera();
        let mut auto = AutoController::enter(&mut camera, 1.0).unwrap();
        camera.start().unwrap();
        let mut now = Instant::now();
        let fitted = |auto: &AutoController| {
            auto.status()
                .changes
                .iter()
                .any(|c| c.reason == "exposure limit fitted to the measured frame time")
        };
        fake.set_f64(RESULTING, 10.0);
        fake.set_f64(EXPOSURE, 2.0 * auto.policy.exposure_cap);
        drive(&mut auto, &mut camera, &mut now, 3.0, 20);
        assert!(!fitted(&auto));
        fake.set_f64(EXPOSURE, auto.policy.exposure_cap);
        fake.set_f64(RESULTING, 28.0);
        drive(&mut auto, &mut camera, &mut now, 3.0, 20);
        assert!(!fitted(&auto));
        fake.set_f64(RESULTING, 10.0);
        drive(&mut auto, &mut camera, &mut now, 3.0, 20);
        assert!(fitted(&auto));
    }
    #[test]
    fn supervisor_steps_gain_without_a_profile_and_falls_back_when_stalled() {
        let fake = Fake::new(false);
        let mut camera = fake.camera();
        let mut auto = AutoController::enter(&mut camera, 0.5).unwrap();
        assert_eq!((fake.u32(GAIN_AUTO), fake.f64(GAIN)), (0, 0.0));
        camera.start().unwrap();
        let mut now = Instant::now();
        fake.set_f64(EXPOSURE, auto.policy.exposure_cap);
        drive(&mut auto, &mut camera, &mut now, 2.5, 20);
        assert_eq!(fake.f64(GAIN), 3.0);
        assert_eq!(auto.status().changes.last().unwrap().feature, "Gain");
        fake.set_f64(EXPOSURE, 5000.0);
        drive(&mut auto, &mut camera, &mut now, 2.5, 20);
        assert_eq!(fake.f64(GAIN), 0.0);
        drive(&mut auto, &mut camera, &mut now, 5.0, 20);
        assert_eq!(auto.status().strategy, "software");
        assert_eq!(fake.u32(EXPOSURE_AUTO), 0);
    }

    #[test]
    fn bandwidth_follows_transport_stats_and_ignores_tainted_windows() {
        let fake = Fake::new(true);
        fake.set_lost(0);
        let mut camera = fake.camera();
        let mut auto = AutoController::enter(&mut camera, 1.0).unwrap();
        camera.start().unwrap();
        let mut now = Instant::now();
        drive(&mut auto, &mut camera, &mut now, 3.0, 120);
        assert_eq!(fake.u32(THROUGHPUT), 94_685_776);
        assert_eq!(
            auto.status().changes.last().unwrap().reason,
            "bandwidth raised: no frame loss"
        );
        fake.set_lost(5);
        drive(&mut auto, &mut camera, &mut now, 2.5, 120);
        assert_eq!(fake.u32(THROUGHPUT), 54_507_273);
        fake.set_lost(10);
        now += Duration::from_millis(400);
        auto.tick(&mut camera, now);
        drive(&mut auto, &mut camera, &mut now, 4.0, 120);
        assert_eq!(fake.u32(THROUGHPUT), 54_507_273);
        fake.set_lost(12);
        drive(&mut auto, &mut camera, &mut now, 2.5, 120);
        assert!(fake.u32(THROUGHPUT) < 54_507_273);
    }

    #[test]
    fn release_reverts_everything_or_holds_current_values() {
        let fake = Fake::new(true);
        let mut camera = fake.camera();
        let before = fake.memory.lock().unwrap().clone();
        let auto = AutoController::enter(&mut camera, 0.5).unwrap();
        fake.set_f64(EXPOSURE, 20_000.0);
        fake.set_f64(GAIN, 6.0);
        auto.release(&mut camera, true).unwrap();
        assert_eq!(
            &fake.memory.lock().unwrap()[..ACQUISITION as usize],
            &before[..ACQUISITION as usize]
        );

        let fake = Fake::new(true);
        let mut camera = fake.camera();
        let auto = AutoController::enter(&mut camera, 0.5).unwrap();
        auto.release(&mut camera, false).unwrap();
        assert_eq!(
            [EXPOSURE_AUTO, GAIN_AUTO, WHITE_AUTO].map(|a| fake.u32(a)),
            [0; 3]
        );
        assert_eq!(fake.f64(LOWER), 19.0);

        let fake = Fake::new(true);
        let mut camera = fake.camera();
        fake.set_u32(EXPOSURE_AUTO, 2);
        fake.set_u32(WHITE_AUTO, 1);
        hold(&mut camera).unwrap();
        hold(&mut camera).unwrap();
        assert_eq!(
            [EXPOSURE_AUTO, GAIN_AUTO, WHITE_AUTO].map(|a| fake.u32(a)),
            [0; 3]
        );
    }

    #[test]
    fn external_image_and_trigger_changes_refresh_the_policy() {
        let mut fake = Fake::new(true);
        fake.xml = fake.xml.replace(
            "</RegisterDescription>",
            "<Enumeration Name='TriggerMode'><EnumEntry Name='Off'><Value>0</Value></EnumEntry><EnumEntry Name='On'><Value>1</Value></EnumEntry><pValue>TriggerReg</pValue></Enumeration><IntReg Name='TriggerReg'><Address>176</Address><Length>4</Length><AccessMode>RW</AccessMode></IntReg></RegisterDescription>",
        );
        let mut camera = fake.camera();
        let mut auto = AutoController::enter(&mut camera, 1.0).unwrap();
        let fps = auto.status().target_fps.unwrap();
        fake.set_u32(PAYLOAD, 4_608_000);
        fake.set_u32(PIXEL, MONO8);
        auto.invalidate();
        auto.tick(&mut camera, Instant::now());
        assert!(near(auto.status().target_fps.unwrap(), fps / 2.0));
        assert!(!auto.caps.color);
        fake.set_u32(176, 1);
        auto.invalidate();
        auto.tick(&mut camera, Instant::now());
        assert!(auto.caps.triggered);
        assert_eq!(auto.status().target_fps, None);
    }

    #[test]
    fn quantization_stays_within_the_balance_caps() {
        let bounds = Bounds {
            min: Some(19.0),
            max: Some(1000.0),
            inc: Some(10.0),
        };
        assert_eq!(quantize_range(92.0, bounds, 19.0, 92.0, f64::ceil), 89.0);
        assert_eq!(quantize_range(25.0, bounds, 25.0, 92.0, f64::floor), 29.0);
        let gain = Bounds {
            min: Some(0.0),
            max: Some(48.0),
            inc: Some(1.0),
        };
        assert_eq!(quantize_range(1.5, gain, 0.0, 1.5, f64::ceil), 1.0);
    }

    #[test]
    fn firmware_without_exposure_limits_uses_software_control() {
        let mut fake = Fake::new(true);
        fake.xml = fake
            .xml
            .replace("AutoExposureTimeLowerLimit", "UnusedLowerLimit")
            .replace("AutoExposureTimeUpperLimit", "UnusedUpperLimit");
        fake.set_u32(EXPOSURE_AUTO, 2);
        let mut camera = fake.camera();
        let auto = AutoController::enter(&mut camera, 0.5).unwrap();
        assert_eq!(auto.status().strategy, "software");
        assert_eq!(fake.u32(EXPOSURE_AUTO), 0);
        assert!(auto.manages("ExposureAuto"));
        auto.release(&mut camera, true).unwrap();
        assert_eq!(fake.u32(EXPOSURE_AUTO), 2);
    }

    #[test]
    fn revert_restores_exact_values_and_selected_white_balance_channels() {
        let mut fake = Fake::new(true);
        fake.xml = fake.xml.replace(
            "</RegisterDescription>",
            "<Enumeration Name='BalanceRatioSelector'><EnumEntry Name='Red'><Value>0</Value></EnumEntry><EnumEntry Name='Blue'><Value>1</Value></EnumEntry><pValue>BalanceSelectorReg</pValue></Enumeration><IntReg Name='BalanceSelectorReg'><Address>144</Address><Length>4</Length><AccessMode>RW</AccessMode></IntReg><Float Name='BalanceRatio'><pIndex>BalanceRatioSelector</pIndex><pValueIndexed Index='0'>RedRatio</pValueIndexed><pValueIndexed Index='1'>BlueRatio</pValueIndexed></Float><FloatReg Name='RedRatio'><Address>152</Address><Length>8</Length><AccessMode>RW</AccessMode></FloatReg><FloatReg Name='BlueRatio'><Address>160</Address><Length>8</Length><AccessMode>RW</AccessMode></FloatReg></RegisterDescription>",
        );
        fake.set_f64(152, 1.1);
        fake.set_f64(160, 1.8);
        fake.set_u32(EXPOSURE_AUTO, 2);
        fake.set_u32(WHITE_AUTO, 2);
        let mut camera = fake.camera();
        let auto = AutoController::enter(&mut camera, 0.5).unwrap();
        assert!(auto.manages("BalanceRatioSelector"));
        fake.set_f64(EXPOSURE, 3983.0);
        fake.set_f64(152, 1.6);
        fake.set_f64(160, 2.2);
        fake.set_u32(144, 1);
        auto.release(&mut camera, true).unwrap();
        assert_eq!(fake.f64(EXPOSURE), 3982.0);
        assert_eq!((fake.f64(152), fake.f64(160), fake.u32(144)), (1.1, 1.8, 0));
        assert_eq!((fake.u32(EXPOSURE_AUTO), fake.u32(WHITE_AUTO)), (2, 2));
    }

    #[test]
    fn throughput_starts_within_the_link_budget() {
        let fake = Fake::new(true);
        fake.set_lost(0);
        fake.set_u32(THROUGHPUT, 125_000_000);
        let mut camera = fake.camera();
        let auto = AutoController::enter(&mut camera, 1.0).unwrap();
        assert_eq!(fake.u32(THROUGHPUT), 115_000_000);
        assert_eq!(auto.bandwidth.as_ref().unwrap().aimd.limit, 115e6);
        auto.release(&mut camera, true).unwrap();
        assert_eq!(fake.u32(THROUGHPUT), 125_000_000);
    }

    #[test]
    fn video_mode_reports_selected_rate_resets_convergence_and_reverts() {
        let mut fake = Fake::new(true);
        fake.xml = fake.xml
            .replace("ExposureTime", "UnusedExposure")
            .replace("Gain", "UnusedGain")
            .replace(
                "</RegisterDescription>",
                "<Enumeration Name='VideoMode'><EnumEntry Name='1920x1080@30'><Value>0</Value></EnumEntry><EnumEntry Name='640x480@120'><Value>1</Value></EnumEntry><pValue>VideoModeReg</pValue></Enumeration><IntReg Name='VideoModeReg'><Address>176</Address><Length>4</Length><AccessMode>RW</AccessMode></IntReg></RegisterDescription>",
            );
        let mut camera = fake.camera();
        camera.info.transport = Transport::Media;
        let mut auto = AutoController::enter(&mut camera, 0.5).unwrap();
        assert_eq!(auto.status().strategy, "video-mode");
        assert_eq!(auto.status().target_fps, Some(30.0));
        camera.start().unwrap();
        let mut now = Instant::now();
        drive(&mut auto, &mut camera, &mut now, 1.0, 120);
        assert_eq!(auto.status().state, "stable");
        auto.set_balance(&mut camera, 1.0).unwrap();
        auto.choose_video_mode(&mut camera);
        assert_eq!(camera.get("VideoMode").unwrap(), "640x480@120");
        assert_eq!(auto.status().target_fps, Some(120.0));
        assert_eq!(auto.status().state, "converging");
        assert_eq!(auto.status().brightness, None);
        assert_eq!(auto.period, None);
        assert!(camera.is_streaming());
        assert!(!auto.converged());
        auto.set_forward_fps(Some(20.0));
        auto.tick(&mut camera, now);
        assert_eq!(auto.status().target_fps, Some(120.0));
        assert!(auto.manages("VideoMode"));
        auto.release(&mut camera, true).unwrap();
        assert_eq!(camera.get("VideoMode").unwrap(), "1920x1080@30");
        assert!(camera.is_streaming());
    }

    #[test]
    fn revert_orders_limits_after_a_later_plan_raises_the_lower_bound() {
        let fake = Fake::new(true);
        let mut camera = fake.camera();
        let mut auto = AutoController::enter(&mut camera, 0.5).unwrap();
        auto.write_pair(
            &mut camera,
            ["AutoExposureTimeLowerLimit", "AutoExposureTimeUpperLimit"],
            50_000.0,
            60_000.0,
            "new exposure range",
        );
        assert_eq!((fake.f64(LOWER), fake.f64(UPPER)), (50_000.0, 60_000.0));
        auto.release(&mut camera, true).unwrap();
        assert_eq!((fake.f64(LOWER), fake.f64(UPPER)), (500.0, 10_000.0));
    }
}
