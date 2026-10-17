//! Live signals: the session's data sources with their latest values and
//! sparklines (Slice C).
use super::{Element, Message, Workbench, style::Palette};

/// The inspector's section, or `None` to leave it out.
pub(super) fn view<'a>(
    _bench: &'a Workbench,
    _open: bool,
    _p: &'static Palette,
) -> Option<Element<'a, Message>> {
    None
}
