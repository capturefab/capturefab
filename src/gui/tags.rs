//! Capture tags: line, station, part, lot and note, sent with every capture
//! and recording as its context.
use super::*;
use iced::widget::column;
use prefs::CaptureTags;

/// One capture tag. Its key is the context key it is sent as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TagField {
    Line,
    Station,
    Part,
    Lot,
    Note,
}

impl TagField {
    pub(super) const ALL: [TagField; 5] = [
        TagField::Line,
        TagField::Station,
        TagField::Part,
        TagField::Lot,
        TagField::Note,
    ];
    pub(super) fn key(self) -> &'static str {
        match self {
            TagField::Line => "line",
            TagField::Station => "station",
            TagField::Part => "part",
            TagField::Lot => "lot",
            TagField::Note => "note",
        }
    }
    pub(super) fn label(self) -> &'static str {
        match self {
            TagField::Line => "Line",
            TagField::Station => "Station",
            TagField::Part => "Part",
            TagField::Lot => "Lot",
            TagField::Note => "Note",
        }
    }
    fn placeholder(self) -> &'static str {
        match self {
            TagField::Line => "L1",
            TagField::Station => "Inspection 2",
            TagField::Part => "A-100",
            TagField::Lot => "2026-10-17-A",
            TagField::Note => "Optional",
        }
    }
    pub(super) fn get(self, tags: &CaptureTags) -> &str {
        match self {
            TagField::Line => &tags.line,
            TagField::Station => &tags.station,
            TagField::Part => &tags.part,
            TagField::Lot => &tags.lot,
            TagField::Note => &tags.note,
        }
    }
    pub(super) fn set(self, tags: &mut CaptureTags, value: String) {
        *match self {
            TagField::Line => &mut tags.line,
            TagField::Station => &mut tags.station,
            TagField::Part => &mut tags.part,
            TagField::Lot => &mut tags.lot,
            TagField::Note => &mut tags.note,
        } = value;
    }
}

/// The context a capture is sent with: the filled-in tags, trimmed.
pub(super) fn context(tags: &CaptureTags) -> crate::context::Context {
    TagField::ALL
        .into_iter()
        .filter_map(|field| {
            let value = field.get(tags).trim();
            (!value.is_empty()).then(|| (field.key().to_owned(), value.to_owned()))
        })
        .collect()
}

/// Where the tags go: into each saved still, and into the dataset `file`
/// when captures are being cataloged.
pub(super) fn caption(file: Option<&str>) -> String {
    match file {
        Some(file) => format!("Written into each saved PNG and JPEG, and cataloged in {file}."),
        None => "Written into each saved PNG and JPEG. Turn on Catalog captures \
                 automatically in Dataset to search them later."
            .into(),
    }
}

/// The inspector's section: a summary of the tags, and their fields when open.
/// `dataset_open` hides the link to a Dataset section already shown.
pub(super) fn view<'a>(
    bench: &'a Workbench,
    open: bool,
    dataset_open: bool,
    p: &'static Palette,
) -> Option<Element<'a, Message>> {
    let tags = &bench.tags;
    let context = context(tags);
    // In the fields' order, not the keys'.
    let filled: Vec<&str> = TagField::ALL
        .into_iter()
        .map(|field| field.get(tags).trim())
        .filter(|value| !value.is_empty())
        .collect();
    let summary = if filled.is_empty() {
        "None".to_owned()
    } else {
        filled.join(" · ")
    };
    let mut section = column![widgets::disclosure(
        "Capture tags",
        Some(summary),
        if open { 1.0 } else { 0.0 },
        Message::InspectorDisclosure(inspector::Section::CaptureTags),
        p,
    )]
    .spacing(inspector::ROW_GAP);
    if !open {
        return Some(section.into());
    }
    section = section.push(match dataset::catalog(bench) {
        Some(file) => Element::from(
            text(caption(Some(&file)))
                .size(style::SMALL)
                .color(p.secondary),
        ),
        None => {
            let mut note =
                column![text(caption(None)).size(style::SMALL).color(p.ink(p.warn))].spacing(2);
            if !dataset_open {
                note = note.push(
                    button(text("Show Dataset").size(style::SMALL).font(style::MEDIUM))
                        .padding([3, 0])
                        .style(style::link)
                        .on_press(Message::InspectorDisclosure(inspector::Section::Dataset)),
                );
            }
            note.into()
        }
    });
    let invalid = crate::context::validate(&context).err();
    for field in TagField::ALL {
        section = section.push(
            row![
                container(text(field.label()).size(style::BODY).color(p.secondary))
                    .padding(iced::Padding::ZERO.top(5.0))
                    .width(Fill),
                container(
                    text_input(field.placeholder(), field.get(tags))
                        .name(format!("Capture tag {}", field.label()))
                        .id(format!("capture-tag-{}", field.key()))
                        .on_input(move |value| Message::Tag(field, value))
                        .size(style::BODY)
                        .padding([5, 8])
                        .style(style::input),
                )
                .width(136),
            ]
            .spacing(6)
            .align_y(Alignment::Start),
        );
    }
    if let Some(error) = invalid {
        section = section.push(
            text(error.to_string())
                .size(style::CAPTION)
                .color(p.ink(p.danger)),
        );
    }
    Some(section.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caption_says_where_tags_go() {
        assert_eq!(
            caption(Some("capturefab.duckdb")),
            "Written into each saved PNG and JPEG, and cataloged in capturefab.duckdb."
        );
        assert!(caption(None).contains("Turn on Catalog captures automatically"));
    }

    #[test]
    fn context_drops_empty_tags_and_trims() {
        let tags = CaptureTags {
            line: " L1 ".into(),
            part: "A-100".into(),
            note: "   ".into(),
            ..Default::default()
        };
        assert_eq!(
            context(&tags),
            crate::context::Context::from([
                ("line".into(), "L1".into()),
                ("part".into(), "A-100".into()),
            ])
        );
        for field in TagField::ALL {
            let mut tags = CaptureTags::default();
            field.set(&mut tags, "x".into());
            assert_eq!(field.get(&tags), "x");
            crate::context::validate(&context(&tags)).unwrap();
        }
    }
}
