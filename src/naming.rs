//! Output file naming: templates with {camera}, {date}, {time}, {datetime},
//! {seq} and {frame}, and what to do when a name is taken. Names are picked
//! only in the camera worker; the CLI and GUI validate and preview.
use anyhow::Result;
use serde::{Deserialize, Serialize};

/// What happens when an output name without {seq} already exists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum OnExists {
    /// Refuse, so nothing is ever overwritten.
    #[default]
    Fail,
    /// Write `stem-0001.ext`, `stem-0002.ext`, ... instead.
    Number,
}
impl OnExists {
    pub fn is_fail(&self) -> bool {
        *self == Self::Fail
    }
}

/// Check a capture, schedule or recording request before it is queued.
pub fn validate_request(
    _output: &str,
    _count: u32,
    _on_exists: OnExists,
    context: &crate::context::Context,
) -> Result<()> {
    crate::context::validate(context)
}
