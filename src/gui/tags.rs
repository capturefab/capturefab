//! Capture tags: line, station, part, lot and note, sent with every capture
//! and recording as its context (Slice A).
use super::{Element, Message, Workbench, style::Palette};

/// The inspector's section, or `None` to leave it out.
pub(super) fn view<'a>(
    _bench: &'a Workbench,
    _open: bool,
    _p: &'static Palette,
) -> Option<Element<'a, Message>> {
    None
}
