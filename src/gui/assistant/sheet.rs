//! The quick command surface renders cached, checked data. Decoding a sent
//! thumbnail happens on the assistant worker, from the exact encoded bytes.
use super::{quick, *};
use crate::assistant::conversation::ImagePart;
use iced::widget::{column, image};
use std::{fmt, io::Cursor};

#[derive(Clone)]
pub(in crate::gui) struct SentImage {
    pub part: ImagePart,
    pub handle: Option<image::Handle>,
}
impl fmt::Debug for SentImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SentImage")
            .field("image", &self.part)
            .field("decoded", &self.handle.is_some())
            .finish()
    }
}
impl SentImage {
    pub(in crate::gui) fn new(mut part: ImagePart) -> Self {
        part.label = crate::tools::redact::for_output_text(&part.label);
        let handle =
            decode(&part).map(|rgba| image::Handle::from_rgba(part.width, part.height, rgba));
        Self { part, handle }
    }
}

fn decode(part: &ImagePart) -> Option<Vec<u8>> {
    if part.width == 0
        || part.height == 0
        || part.width > 2048
        || part.height > 2048
        || part.data.len() > crate::inspect::MAX_IMAGE_BYTES
    {
        return None;
    }
    match part.mime {
        #[cfg(feature = "jpeg")]
        "image/jpeg" => {
            let header = turbojpeg::read_header(&part.data).ok()?;
            if (header.width, header.height) != (part.width as usize, part.height as usize) {
                return None;
            }
            let decoded = turbojpeg::decompress(&part.data, turbojpeg::PixelFormat::RGBA).ok()?;
            Some(decoded.pixels)
        }
        "image/png" => {
            let mut decoder = png::Decoder::new(Cursor::new(part.data.as_ref()));
            decoder.set_limits(png::Limits {
                bytes: 16 * 1024 * 1024,
            });
            decoder
                .set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
            let mut reader = decoder.read_info().ok()?;
            if (reader.info().width, reader.info().height) != (part.width, part.height) {
                return None;
            }
            let mut bytes = vec![0; reader.output_buffer_size()];
            let info = reader.next_frame(&mut bytes).ok()?;
            if info.bit_depth != png::BitDepth::Eight {
                return None;
            }
            let pixels = &bytes[..info.buffer_size()];
            let rgba = match info.color_type {
                png::ColorType::Rgba => pixels.to_vec(),
                png::ColorType::Rgb => pixels
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .flat_map(|rgb| [rgb[0], rgb[1], rgb[2], 255])
                    .collect(),
                png::ColorType::Grayscale => pixels
                    .iter()
                    .flat_map(|&gray| [gray, gray, gray, 255])
                    .collect(),
                png::ColorType::GrayscaleAlpha => pixels
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .flat_map(|gray| [gray[0], gray[0], gray[0], gray[1]])
                    .collect(),
                png::ColorType::Indexed => return None,
            };
            Some(rgba)
        }
        _ => None,
    }
}

#[cfg(test)]
pub(in crate::gui) fn content<'a>(
    state: &'a quick::State,
    p: &'static Palette,
    height: f32,
    spin: usize,
) -> Element<'a, quick::Input> {
    content_with_banner(state, p, height, spin, None)
}

pub(in crate::gui) fn content_with_banner<'a>(
    state: &'a quick::State,
    p: &'static Palette,
    height: f32,
    spin: usize,
    banner: Option<Element<'a, quick::Input>>,
) -> Element<'a, quick::Input> {
    let provider = state.provider_label();
    let model = state.model_label();
    let identity = if model.is_empty() {
        provider.clone()
    } else {
        format!("{provider} · {model}")
    };
    let busy = !state.editable();
    let mut body = column![].spacing(16).width(Fill);
    if let Some(banner) = banner {
        body = body.push(banner);
    }
    if !state.has_provider() {
        body = body.push(setup(state, p));
    } else if state.transcript.is_empty() && state.plan.steps.is_empty() {
        body = body.push(empty(state, p));
    }
    for entry in &state.transcript {
        body = body.push(transcript(entry, p));
    }
    let announcement = state.announcement();
    body = body.push(accessibility::label(
        text(announcement.clone())
            .size(style::SMALL)
            .color(p.secondary)
            .wrapping(text::Wrapping::WordOrGlyph)
            .into(),
        announcement,
        accessibility::Role::Status,
    ));
    if let Some(error) = &state.error {
        body = body.push(
            column![
                field_error(error.clone(), p),
                button(text("Try again").size(style::BODY))
                    .name("Retry assistant request")
                    .style(style::secondary)
                    .padding([6, 12])
                    .on_press_maybe((!busy).then_some(quick::Input::Retry)),
            ]
            .spacing(8),
        );
    }
    if let Some(excerpt) = &state.provider_excerpt {
        body = body.push(
            text(format!("Provider said: {excerpt}"))
                .size(style::SMALL)
                .color(p.secondary)
                .wrapping(text::Wrapping::WordOrGlyph),
        );
    }
    if !state.plan.steps.is_empty() {
        body = body.push(super::cards::content(state, p));
    }
    if let Some(path) = &state.exported_plan {
        let command = format!(
            "capturefab tools apply {} --session NAME",
            shell_quote(&path.to_string_lossy())
        );
        body = body.push(
            column![
                text("Plan exported. Review it before applying it in a session.")
                    .size(style::SMALL)
                    .color(p.secondary),
                container(
                    row![
                        text(command.clone())
                            .size(style::SMALL)
                            .font(style::MONO)
                            .wrapping(text::Wrapping::WordOrGlyph)
                            .width(Fill),
                        button(icon(Icon::Copy, 13.0, p.secondary))
                            .name("Copy exported plan command")
                            .padding(4)
                            .style(style::plain)
                            .on_press(quick::Input::Copy(command)),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Start)
                )
                .padding(10)
                .width(Fill)
                .style(style::code),
            ]
            .spacing(6),
        );
    }
    if state.has_provider() {
        body = body.push(
            text(consent(&provider, state.allow_images))
                .size(style::SMALL)
                .color(p.secondary)
                .wrapping(text::Wrapping::WordOrGlyph),
        );
    }
    let provider_chip = tip(
        button(
            row![
                icon(Icon::Sparkle, 13.0, p.accent),
                one_line(identity.clone(), style::SMALL, style::MEDIUM, p.text).width(Fill),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .name("Assistant provider and model")
        .description("Open AI assistant Settings")
        .padding([5, 8])
        .width(Fill)
        .style(style::secondary)
        .on_press(quick::Input::Settings),
        format!("{identity} · AI assistant Settings"),
    );
    let controls = column![
        modes(p, !busy),
        row![
            provider_chip,
            button(text("New conversation").size(style::SMALL))
                .name("New assistant conversation")
                .description(if busy {
                    "Wait for the assistant or stop the current work first."
                } else {
                    "Clear this conversation and its proposed plan."
                })
                .padding([5, 8])
                .style(style::plain)
                .on_press_maybe((!busy).then_some(quick::Input::NewConversation)),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
        row![
            icon(Icon::Eye, 14.0, p.secondary),
            checkbox_maybe(
                "See camera images",
                state.allow_images,
                (!busy).then_some(quick::Input::SeeImages),
            ),
        ]
        .spacing(7)
        .align_y(Alignment::Center),
    ]
    .spacing(8);
    sheet_frame("Quick Command", quick::Input::Open(false), body, p)
        .mark(Icon::Sparkle)
        .size(720.0, height)
        .anchor(scrollable::Anchor::End)
        .scrolled(state.scrolled)
        .on_scroll(quick::Input::Scrolled)
        .controls(controls)
        .footer(footer(state, p, spin))
        .default_action(None)
        .into()
}

fn modes<'a>(_p: &'static Palette, enabled: bool) -> Element<'a, quick::Input> {
    let mut modes = row![].spacing(2).width(Fill);
    for (label, mode) in [
        ("Plan first", super::tasks::Mode::Plan),
        ("Ask before each change", super::tasks::Mode::Supervised),
        ("Run on its own", super::tasks::Mode::Autonomous),
    ] {
        let active = mode == super::tasks::Mode::Plan;
        modes = modes.push(
            button(
                text(label)
                    .size(style::SMALL)
                    .wrapping(text::Wrapping::WordOrGlyph)
                    .align_x(Alignment::Center),
            )
            .name(format!("Assistant mode: {label}"))
            .role(accessibility::Role::RadioButton)
            .checked(active)
            .description(if active {
                "Review and edit every step before running your plan."
            } else {
                "Task controls are available in the assistant task view."
            })
            .width(Fill)
            .padding([5, 6])
            .style(style::toggle(active))
            .on_press_maybe(enabled.then_some(quick::Input::TaskMode(mode))),
        );
    }
    accessibility::group(
        container(modes).style(style::well).padding(2).into(),
        "How the assistant works".into(),
        accessibility::Role::RadioGroup,
    )
}

fn footer<'a>(
    state: &'a quick::State,
    p: &'static Palette,
    spin: usize,
) -> Element<'a, quick::Input> {
    let busy = state.thinking() || state.running();
    let editable = state.editable();
    let send = editable && state.has_provider() && !state.prompt.trim().is_empty();
    let prompt = text_input::text_input("Describe a capture or camera change…", &state.prompt)
        .id("quick-command-prompt")
        .name("Quick command request")
        .description("Return sends your request. It never runs a proposed plan.")
        .size(style::BODY)
        .padding([7, 10])
        .width(Fill)
        .style(style::input)
        .on_input_maybe(editable.then_some(quick::Input::Prompt))
        .on_submit_maybe(send.then_some(quick::Input::Send));
    let action = if busy {
        button(icon(Icon::Stop, 15.0, p.text))
            .name("Stop assistant work")
            .description("Stop this turn or the remaining plan steps.")
            .padding(8)
            .style(style::stop)
            .on_key_press(super::no_return)
            .on_press(quick::Input::Stop)
    } else {
        button(icon(Icon::PaperPlaneRight, 15.0, p.accent))
            .name("Send quick command")
            .description(if state.has_provider() {
                "Send your request to the selected AI provider."
            } else {
                "Set up an AI provider before sending a request."
            })
            .padding(8)
            .style(style::secondary)
            .on_press_maybe(send.then_some(quick::Input::Send))
    };
    let mut footer = column![row![prompt, action].spacing(8).align_y(Alignment::Center)]
        .spacing(8)
        .width(Fill);
    if busy {
        let status = if state.status.is_empty() {
            if state.thinking() {
                format!("Asking {}…", state.provider_label())
            } else {
                "Running your plan…".into()
            }
        } else {
            state.status.clone()
        };
        footer = footer.push(accessibility::label(
            row![
                icon::spinner(14.0, p.secondary, spin),
                text(status.clone())
                    .size(style::SMALL)
                    .wrapping(text::Wrapping::WordOrGlyph),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into(),
            status,
            accessibility::Role::Status,
        ));
    }
    if let Some((step, reason)) = state.blocking_step() {
        footer = footer.push(
            button(
                text(reason.clone())
                    .size(style::SMALL)
                    .wrapping(text::Wrapping::WordOrGlyph),
            )
            .name(reason)
            .padding(0)
            .style(style::link)
            .on_press(quick::Input::FocusStep(step)),
        );
    }
    if !state.plan.steps.is_empty() {
        let export: Element<'a, quick::Input> = button(text("Export plan…").size(style::BODY))
            .name("Export assistant plan")
            .padding([6, 8])
            .style(style::plain)
            .on_press_maybe(editable.then_some(quick::Input::ExportPlan))
            .into();
        let close: Element<'a, quick::Input> = button(text("Close").size(style::BODY))
            .name("Close quick command")
            .padding([6, 10])
            .style(style::plain)
            .on_press(quick::Input::Open(false))
            .into();
        let run: Element<'a, quick::Input> = button(text(state.run_label()).size(style::BODY))
            .id("quick-command-run")
            .on_key_press(super::no_return)
            .name(state.run_label())
            .description(if busy {
                "Waiting for the assistant…".into()
            } else {
                state
                    .blocking_step()
                    .map(|(_, reason)| reason)
                    .unwrap_or_else(|| "Run the checked steps shown above.".into())
            })
            .padding([6, 12])
            .style(style::primary)
            .on_press_maybe(
                state
                    .ready_to_run()
                    .then_some(quick::Input::Run(state.plan.revision)),
            )
            .into();
        let (first, second) = dialog_actions(close, run, false);
        footer = footer.push(
            row![export, space().width(Fill), first, second]
                .spacing(6)
                .align_y(Alignment::Center),
        );
    }
    footer.into()
}

fn setup<'a>(state: &quick::State, p: &'static Palette) -> Element<'a, quick::Input> {
    let mut body = column![
        heading("Choose an AI provider to get started", p),
        text("The assistant turns your request into an editable camera plan. You review the steps before anything runs.")
            .size(style::BODY)
            .wrapping(text::Wrapping::WordOrGlyph),
    ]
    .spacing(10);
    let detected = state.detected_environment();
    if !detected.is_empty() {
        body = body.push(
            text(format!(
                "Detected environment settings: {}.",
                detected.join(", ")
            ))
            .size(style::SMALL)
            .color(p.secondary)
            .wrapping(text::Wrapping::WordOrGlyph),
        );
    }
    body.push(
        button(text("Set up a provider…").size(style::BODY))
            .name("Set up an assistant provider")
            .padding([7, 12])
            .style(style::primary)
            .on_press(quick::Input::Settings),
    )
    .into()
}

fn empty<'a>(state: &quick::State, p: &'static Palette) -> Element<'a, quick::Input> {
    let suggestions = if state
        .context
        .as_ref()
        .is_some_and(|context| !context.snapshot.cameras.is_empty())
    {
        [
            "Take a photo",
            "Record for 30 seconds",
            "Check the camera exposure",
        ]
    } else {
        [
            "Find available cameras",
            "Help me connect a camera",
            "Show the storage budget",
        ]
    };
    let mut body = column![
        heading("What would you like to capture?", p),
        text("Describe a capture or a camera change. Review the proposed plan before it runs.")
            .size(style::BODY)
            .color(p.secondary)
            .wrapping(text::Wrapping::WordOrGlyph),
    ]
    .spacing(10);
    for suggestion in suggestions {
        body = body.push(
            button(text(suggestion).size(style::BODY))
                .name(format!("Suggested request: {suggestion}"))
                .padding([9, 12])
                .width(Fill)
                .style(style::card)
                .on_press(quick::Input::Suggestion(suggestion.into())),
        );
    }
    body.into()
}

fn transcript<'a>(
    entry: &'a quick::TranscriptEntry,
    p: &'static Palette,
) -> Element<'a, quick::Input> {
    use quick::TranscriptEntry;
    match entry {
        TranscriptEntry::User(message) => accessibility::group(
            container(
                text(message)
                    .size(style::BODY)
                    .wrapping(text::Wrapping::WordOrGlyph),
            )
            .padding([10, 12])
            .width(Fill)
            .style(style::well)
            .into(),
            "Your request".into(),
            accessibility::Role::Group,
        ),
        TranscriptEntry::Assistant(message) => accessibility::group(
            text(message)
                .size(style::BODY)
                .wrapping(text::Wrapping::WordOrGlyph)
                .into(),
            "Assistant reply".into(),
            accessibility::Role::Group,
        ),
        TranscriptEntry::Tool {
            title,
            summary,
            images,
        } => {
            let mut result = column![
                row![
                    Level::Done.mark(13.0, p),
                    text(title).size(style::SMALL).font(style::SEMIBOLD),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
                text(summary)
                    .size(style::SMALL)
                    .color(p.secondary)
                    .wrapping(text::Wrapping::WordOrGlyph),
            ]
            .spacing(6);
            for sent in images {
                result = result.push(thumbnail(sent, p));
            }
            accessibility::group(result.into(), title.clone(), accessibility::Role::Group)
        }
        TranscriptEntry::Outcome { summary, level } => row![
            level.mark(14.0, p),
            text(summary)
                .size(style::BODY)
                .color(level.color(p))
                .wrapping(text::Wrapping::WordOrGlyph),
        ]
        .spacing(8)
        .align_y(Alignment::Start)
        .into(),
        TranscriptEntry::Notice(message) => text(message)
            .size(style::SMALL)
            .color(p.secondary)
            .wrapping(text::Wrapping::WordOrGlyph)
            .into(),
    }
}

pub(in crate::gui) fn thumbnail<'a>(
    sent: &'a SentImage,
    p: &'static Palette,
) -> Element<'a, quick::Input> {
    let label = format!(
        "{} · {} × {} · {} bytes",
        sent.part.label,
        sent.part.width,
        sent.part.height,
        sent.part.data.len()
    );
    let preview: Element<'a, quick::Input> = match &sent.handle {
        Some(handle) => image(handle.clone())
            .width(240)
            .height(160)
            .content_fit(iced::ContentFit::Contain)
            .into(),
        None => text("Image preview unavailable")
            .size(style::SMALL)
            .color(p.secondary)
            .into(),
    };
    column![
        accessibility::label(preview, label.clone(), accessibility::Role::Image),
        text(label)
            .size(style::CAPTION)
            .color(p.secondary)
            .wrapping(text::Wrapping::WordOrGlyph),
    ]
    .spacing(4)
    .into()
}

fn consent(provider: &str, images: bool) -> String {
    let images = if images {
        ", and camera images the assistant inspects when See camera images is on"
    } else {
        ""
    };
    format!(
        "Sends your request and details of this session to {provider}, under {provider}'s data terms: camera names, models and IDs, discovered devices, destination names and locations, storage use, job file names and recent activity{images}. Never sends keys."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn thumbnails_keep_the_exact_encoded_image_and_decode_png_without_optional_codecs() {
        let part = super::super::scenes::image();
        let original = part.data.clone();
        let sent = SentImage::new(part);
        assert!(Arc::ptr_eq(&original, &sent.part.data));
        assert!(sent.handle.is_some());
        assert_eq!(decode(&sent.part).unwrap().len(), 96 * 64 * 4);
        assert!(!format!("{sent:?}").contains("base64"));
        let mut wrong = sent.part.clone();
        wrong.width = 97;
        assert!(SentImage::new(wrong).handle.is_none());
        let mut huge = sent.part.clone();
        huge.width = 2049;
        assert!(SentImage::new(huge).handle.is_none());
        let mut labeled = sent.part.clone();
        labeled.label = "Image 1: rtsp://operator:planted-password@camera/live".into();
        assert!(
            !SentImage::new(labeled)
                .part
                .label
                .contains("planted-password")
        );
    }

    #[cfg(feature = "jpeg")]
    #[test]
    fn thumbnails_decode_the_sent_jpeg_instead_of_showing_a_live_camera_frame() {
        let part = ImagePart {
            mime: "image/jpeg",
            data: Arc::from(include_bytes!("../../assistant/connection-test-red.jpg").as_slice()),
            width: 64,
            height: 64,
            label: "Image 1: inspected frame".into(),
        };
        let original = part.data.clone();
        let sent = SentImage::new(part);
        assert!(Arc::ptr_eq(&original, &sent.part.data));
        assert!(sent.handle.is_some());
        let pixels = decode(&sent.part).unwrap();
        assert_eq!(pixels.len(), 64 * 64 * 4);
        assert!(
            pixels
                .as_chunks::<4>()
                .0
                .iter()
                .all(|rgba| rgba[0] > 240 && rgba[1] < 16)
        );
    }

    #[test]
    fn consent_follows_explicit_plan_image_permission() {
        let plain = consent("Anthropic", false);
        assert!(!plain.contains("camera images"));
        assert!(plain.ends_with("Never sends keys."));
        assert!(consent("Anthropic", true).contains("camera images the assistant inspects"));
    }
}
