//! Coordinator arms for the dataset commands. None of them touch a camera
//! worker, so none take a command turn.
use super::Slot;
use crate::{session::SessionCommand, signals::hub::SignalHub};
use anyhow::{Result, bail};
use serde_json::Value;
use std::sync::Arc;

pub fn run(slot: &Arc<Slot>, _signals: &SignalHub, command: SessionCommand) -> Result<Value> {
    match command {
        SessionCommand::DatasetStatus => Ok(serde_json::to_value(slot.status_cached())?),
        SessionCommand::DatasetOpen { .. }
        | SessionCommand::DatasetClose
        | SessionCommand::DatasetNote { .. }
        | SessionCommand::DatasetQuery { .. }
        | SessionCommand::DatasetExport { .. } => bail!("datasets are not implemented yet"),
        _ => bail!("not a dataset command"),
    }
}
