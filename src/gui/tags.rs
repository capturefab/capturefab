//! Capture tags: label, line, station, part, lot and note, sent with every
//! capture and recording as its context. File name patterns can use them as
//! {tag:KEY}, so the "One folder per label" naming puts each label in its own folder.
use super::*;
use iced::widget::column;
use prefs::CaptureTags;

/// One capture tag. Its key is the context key it is sent as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TagField {
    Label,
    Line,
    Station,
    Part,
    Lot,
    Note,
}

impl TagField {
    pub(super) const ALL: [TagField; 6] = [
        TagField::Label,
        TagField::Line,
        TagField::Station,
        TagField::Part,
        TagField::Lot,
        TagField::Note,
    ];
    pub(super) fn key(self) -> &'static str {
        match self {
            TagField::Label => "label",
            TagField::Line => "line",
            TagField::Station => "station",
            TagField::Part => "part",
            TagField::Lot => "lot",
            TagField::Note => "note",
        }
    }
    pub(super) fn label(self) -> &'static str {
        match self {
            TagField::Label => "Label",
            TagField::Line => "Line",
            TagField::Station => "Station",
            TagField::Part => "Part",
            TagField::Lot => "Lot",
            TagField::Note => "Note",
        }
    }
    fn placeholder(self) -> &'static str {
        match self {
            TagField::Label => "good / defect",
            TagField::Line => "L1",
            TagField::Station => "Inspection 2",
            TagField::Part => "A-100",
            TagField::Lot => "2026-10-17-A",
            TagField::Note => "Optional",
        }
    }
    pub(super) fn get(self, tags: &CaptureTags) -> &str {
        match self {
            TagField::Label => &tags.label,
            TagField::Line => &tags.line,
            TagField::Station => &tags.station,
            TagField::Part => &tags.part,
            TagField::Lot => &tags.lot,
            TagField::Note => &tags.note,
        }
    }
    pub(super) fn set(self, tags: &mut CaptureTags, value: String) {
        *match self {
            TagField::Label => &mut tags.label,
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

/// The first tag `output` names files after that is empty, if any.
fn missing_key(output: &str, tags: &CaptureTags) -> Option<String> {
    crate::naming::missing_tags(output, &context(tags))
        .into_iter()
        .next()
}

/// The capture tag field `output` needs filled in before it can save, for a
/// link that opens Capture tags at that field.
pub(super) fn missing_field(output: &str, tags: &CaptureTags) -> Option<TagField> {
    let key = missing_key(output, tags)?;
    TagField::ALL.into_iter().find(|field| field.key() == key)
}

/// What the save location still needs, in a few words for the toolbar,
/// such as "needs a Label".
pub(super) fn missing_short(output: &str, tags: &CaptureTags) -> Option<String> {
    let key = missing_key(output, tags)?;
    Some(
        match TagField::ALL.into_iter().find(|field| field.key() == key) {
            Some(field) => format!("needs a {}", field.label()),
            None => format!("needs the {key} tag"),
        },
    )
}

/// What to fill in when `output` names files after a tag that is empty,
/// such as "Fill in Label in Capture tags to choose the folder".
pub(super) fn missing_hint(output: &str, tags: &CaptureTags) -> Option<String> {
    let key = missing_key(output, tags)?;
    let what = match TagField::ALL.into_iter().find(|field| field.key() == key) {
        Some(field) => format!("Fill in {} in Capture tags", field.label()),
        None => format!("Add the {key} tag (--context {key}=VALUE)"),
    };
    let target = if output
        .rsplit(std::path::is_separator)
        .next()
        .is_some_and(|name| name.contains(&format!("{{tag:{key}}}")))
    {
        "the file name"
    } else {
        "the folder"
    };
    Some(format!("{what} to choose {target}"))
}

/// Where the tags go: with every capture and recording, into each saved
/// still when `format` embeds metadata (PNG and JPEG do; raw, PGM and PPM
/// carry none), and into the dataset `file` when captures are cataloged.
pub(super) fn caption(format: &str, file: Option<&str>) -> String {
    let embedded = if matches!(format, "png" | "jpeg" | "jpg") {
        "Saved with every capture and recording, and written into each PNG and JPEG"
    } else {
        "Saved with every capture and recording. Raw, PGM and PPM files carry no tags"
    };
    match file {
        Some(file) => format!("{embedded}; cataloged in {file}."),
        None => format!("{embedded}. Not cataloged in a dataset."),
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
    let file = dataset::catalog(bench);
    let mut note = column![
        text(caption(bench.format, file.as_deref()))
            .size(style::SMALL)
            .color(p.secondary)
    ]
    .spacing(2);
    if file.is_none() && !dataset_open {
        note = note.push(
            button(text("Show Dataset").size(style::SMALL).font(style::MEDIUM))
                .padding([3, 0])
                .style(style::link)
                .on_press(Message::InspectorDisclosure(inspector::Section::Dataset)),
        );
    }
    section = section.push(note);
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
            caption("png", Some("capturefab.duckdb")),
            "Saved with every capture and recording, and written into each PNG and JPEG; \
             cataloged in capturefab.duckdb."
        );
        assert_eq!(
            caption("raw", None),
            "Saved with every capture and recording. Raw, PGM and PPM files carry no tags. \
             Not cataloged in a dataset."
        );
        assert!(caption("jpeg", None).contains("written into each PNG and JPEG"));
    }

    #[test]
    fn missing_label_says_what_to_fill_in() {
        let output = "/tmp/shots/{tag:label}/{camera}-{datetime}-{seq}.png";
        let mut tags = CaptureTags::default();
        assert_eq!(
            missing_hint(output, &tags).as_deref(),
            Some("Fill in Label in Capture tags to choose the folder")
        );
        assert_eq!(missing_field(output, &tags), Some(TagField::Label));
        assert_eq!(
            missing_short(output, &tags).as_deref(),
            Some("needs a Label")
        );
        tags.label = "good".into();
        assert_eq!(missing_hint(output, &tags), None);
        assert_eq!(missing_field(output, &tags), None);
        assert_eq!(missing_hint("/tmp/{seq}.png", &tags), None);
        assert_eq!(
            missing_hint("/tmp/{tag:shift}-{seq}.png", &tags).as_deref(),
            Some("Add the shift tag (--context shift=VALUE) to choose the file name")
        );
    }

    #[test]
    fn context_drops_empty_tags_and_trims() {
        let tags = CaptureTags {
            label: " defect ".into(),
            line: " L1 ".into(),
            part: "A-100".into(),
            note: "   ".into(),
            ..Default::default()
        };
        assert_eq!(
            context(&tags),
            crate::context::Context::from([
                ("label".into(), "defect".into()),
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
