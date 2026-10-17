//! The Dataset section: the session's catalog file, its counts, and New,
//! Open, Close, Export and Reveal actions (Slice B).
use super::{Element, Message, Workbench, style::Palette};

/// The inspector's section, or `None` to leave it out.
pub(super) fn view<'a>(
    _bench: &'a Workbench,
    _open: bool,
    _p: &'static Palette,
) -> Option<Element<'a, Message>> {
    None
}
