//! Numeric plots share the export renderer. Camera identities use native text
//! shaping, including the system's fallback and color-emoji fonts.
use super::super::{
    Message, accessibility,
    format::grouped,
    style::{self, Palette},
};
use crate::analytics::{AnalyticsSnapshot, SvgColors, chart_cameras, human_bytes};
use iced::widget::{column, container, progress_bar, row, svg, text};
use iced::{Element, Length};
use std::{cell::RefCell, collections::HashMap};

const PANEL_HEIGHT: u32 = 260;
const GUTTER: u32 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Profile {
    pub width: u32,
    /// Height of the numeric grid. Native camera rows have their natural height.
    pub height: u32,
    pub panel_width: u32,
    colors: SvgColors,
}

#[derive(Debug, Clone)]
pub(super) struct Graphic {
    pub plots: [svg::Handle; 3],
}

/// Retain each camera group's identity when byte-ranked rows change order.
/// Only the displayed (at most five) cameras remain in this cache.
pub(super) struct ViewState {
    cameras: RefCell<HashMap<String, accessibility::NodeId>>,
    plots: [accessibility::NodeId; 3],
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            cameras: RefCell::default(),
            plots: std::array::from_fn(|_| accessibility::new_node_id()),
        }
    }
}

pub(super) fn profile(logical_chart_width: f32, p: &'static Palette) -> Profile {
    let width = (logical_chart_width.floor() as u32).clamp(180, 2048);
    let wide = width >= 840;
    Profile {
        width,
        height: PANEL_HEIGHT * if wide { 2 } else { 3 },
        panel_width: if wide { (width - GUTTER) / 2 } else { width },
        colors: colors(p),
    }
}

pub(super) fn colors(p: &'static Palette) -> SvgColors {
    SvgColors {
        background: rgb(p.base),
        text: rgb(p.text),
        secondary: rgb(p.secondary),
        axis: rgb(p.control_border),
        frame_rate: rgb(p.ink(p.live)),
        throughput: rgb(p.ink(p.accent_text)),
        missed_frames: rgb(p.ink(p.danger)),
        increased_contrast: p.increased(),
    }
}

fn rgb(color: iced::Color) -> [u8; 3] {
    let [r, g, b, _] = color.into_rgba8();
    [r, g, b]
}

/// Called only by the analytics worker, never during GUI layout or drawing.
pub(super) fn render(report: &AnalyticsSnapshot, profile: Profile) -> anyhow::Result<Graphic> {
    Ok(Graphic {
        plots: crate::analytics::render_interface_time_plots(
            report,
            profile.panel_width,
            profile.colors,
        )?
        .map(|source| svg::Handle::from_memory(source.into_bytes())),
    })
}

pub(super) fn view<'a>(
    report: &'a AnalyticsSnapshot,
    graphic: &Graphic,
    accepted: Profile,
    current: Profile,
    p: &'static Palette,
    state: &ViewState,
) -> Element<'a, Message> {
    let plot = |index: usize| -> Element<'a, Message> {
        let caption = ["Frame rate", "Throughput", "Missed frame rate"][index];
        let updating = accepted.panel_width != current.panel_width;
        let content: Element<'a, Message> = if updating {
            container(
                column![
                    text(caption).font(style::SANS).size(16).color(p.text),
                    text("Updating chart layout…")
                        .font(style::SANS)
                        .size(12)
                        .color(p.secondary),
                ]
                .spacing(8)
                .align_x(iced::Alignment::Center),
            )
            .center_x(current.panel_width as f32)
            .center_y(PANEL_HEIGHT as f32)
            .into()
        } else {
            svg(graphic.plots[index].clone())
                .width(current.panel_width as f32)
                .height(PANEL_HEIGHT as f32)
                .into()
        };
        accessibility::label_description(
            content,
            caption.to_owned(),
            if updating { "Updating chart layout. Retained measurements remain available in the chart history." }
            else { "Time plot of retained session measurements." }.to_owned(),
            accessibility::Role::Image,
            Some(state.plots[index]),
        )
    };
    let cameras = camera_panel(report, current.panel_width as f32, p, state);
    if current.width >= 840 {
        column![
            row![plot(0), plot(1)].spacing(GUTTER),
            row![plot(2), cameras].spacing(GUTTER),
        ]
        .width(current.width as f32)
        .into()
    } else {
        column![plot(0), plot(1), plot(2), cameras]
            .width(current.width as f32)
            .into()
    }
}

/// A data series keeps its hue when the window loses key status. Derive it
/// from explicit appearance flags, never the saved graphic or global settings.
fn data_ink(p: &Palette) -> iced::Color {
    let active = match (p.dark, p.increased()) {
        (false, false) => &style::LIGHT,
        (false, true) => &style::LIGHT_CONTRAST,
        (true, false) => &style::DARK,
        (true, true) => &style::DARK_CONTRAST,
    };
    active.ink(active.accent_text)
}

fn camera_panel<'a>(
    report: &'a AnalyticsSnapshot,
    width: f32,
    p: &'static Palette,
    state: &ViewState,
) -> Element<'a, Message> {
    let data_color = data_ink(p);
    let cameras = chart_cameras(report);
    let ids = {
        let mut ids = state.cameras.borrow_mut();
        ids.retain(|key, _| cameras.iter().any(|camera| &camera.id == key));
        cameras
            .iter()
            .map(|camera| {
                *ids.entry(camera.id.clone())
                    .or_insert_with(accessibility::new_node_id)
            })
            .collect::<Vec<_>>()
    };
    let maximum = cameras
        .iter()
        .map(|camera| camera.bytes)
        .max()
        .unwrap_or(1)
        .max(1);
    let mut body = column![accessibility::label(
        text("Camera data volume")
            .font(style::SANS)
            .size(16)
            .color(p.text)
            .into(),
        "Camera data volume".to_owned(),
        accessibility::Role::Heading,
    )]
    .spacing(14)
    .width(Length::Fill);
    if cameras.is_empty() {
        body = body.push(
            text("No retained cameras")
                .font(style::SANS)
                .size(12)
                .color(p.secondary),
        );
    }
    for (camera, id) in cameras.into_iter().zip(ids) {
        let name = if camera.name.is_empty() {
            &camera.id
        } else {
            &camera.name
        };
        let value = format!(
            "{} · {} bytes",
            human_bytes(camera.bytes),
            grouped(camera.bytes)
        );
        let mut content = column![
            text(name)
                .font(style::SANS)
                .size(13)
                .color(p.text)
                .shaping(iced::widget::text::Shaping::Advanced)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        ]
        .spacing(4)
        .width(Length::Fill);
        if name != &camera.id {
            content = content.push(
                text(&camera.id)
                    .font(style::SANS)
                    .size(12)
                    .color(p.secondary)
                    .shaping(iced::widget::text::Shaping::Advanced)
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
            );
        }
        let content = content.extend([
            text(value)
                .font(style::SANS)
                .size(12)
                .color(p.secondary)
                .width(Length::Fill)
                .shaping(iced::widget::text::Shaping::Advanced)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                .into(),
            progress_bar(0.0..=1.0, (camera.bytes as f64 / maximum as f64) as f32)
                .girth(6)
                .style(move |_| iced::widget::progress_bar::Style {
                    background: p.field.into(),
                    bar: data_color.into(),
                    border: iced::Border::default(),
                })
                .into(),
        ]);
        body = body.push(accessibility::group_with_id(
            content.into(),
            if name == &camera.id {
                format!("Camera data volume: {name}")
            } else {
                format!("Camera data volume: {name} ({})", camera.id)
            },
            id,
        ));
    }
    container(body)
        .padding(14)
        .width(width)
        .style(move |_| iced::widget::container::Style {
            background: Some(p.base.into()),
            ..Default::default()
        })
        .into()
}

#[cfg(test)]
mod tests;
