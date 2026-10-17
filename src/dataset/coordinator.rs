//! Coordinator arms for the dataset commands. None of them touch a camera
//! worker, so none take a command turn.
use super::{ExportFormat, Slot, export::ExportRequest};
use crate::{session::SessionCommand, signals::hub::SignalHub};
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};

const SINK_TIMEOUT: Duration = Duration::from_secs(10);

fn absolute(path: &str, what: &str) -> Result<PathBuf> {
    let path = PathBuf::from(path);
    ensure!(path.is_absolute(), "{what} must be absolute");
    Ok(path)
}

pub fn run(slot: &Arc<Slot>, signals: &SignalHub, command: SessionCommand) -> Result<Value> {
    match command {
        SessionCommand::DatasetStatus => Ok(serde_json::to_value(slot.status_cached())?),
        SessionCommand::DatasetOpen {
            path,
            mode,
            max_bytes,
        } => {
            let path = absolute(&path, "dataset path")?;
            // An explicit dataset replaces the GUI's automatic one.
            slot.set_auto_path(None);
            let status = slot.open(&path, mode, max_bytes)?;
            // Never hold a slot lock here: the hub's writer thread takes it.
            if !signals.has_sink() {
                let sink: Arc<dyn crate::signals::SampleSink> = slot.clone();
                if let Err(e) = signals.set_sink(Some(sink), SINK_TIMEOUT) {
                    eprintln!("capturefab: signal samples will not reach the dataset: {e:#}");
                }
            }
            Ok(serde_json::to_value(status)?)
        }
        SessionCommand::DatasetClose => {
            slot.set_auto_path(None);
            if signals.has_sink() {
                signals.set_sink(None, SINK_TIMEOUT)?;
            }
            Ok(json!({"closed": slot.close()?}))
        }
        SessionCommand::DatasetNote {
            text,
            context,
            camera,
        } => match slot.record_event("note", camera.as_deref(), None, &text, &context)? {
            Some(id) => Ok(json!({"id": id})),
            None => bail!("no dataset is open; run `capturefab dataset open FILE.duckdb`"),
        },
        SessionCommand::DatasetQuery {
            sql,
            limit,
            timeout_ms,
        } => slot.reader()?.query(&sql, limit, timeout_ms),
        SessionCommand::DatasetExport {
            output,
            formats,
            tables,
            since_ms,
            until_ms,
            storage,
        } => {
            let output = absolute(&output, "export folder")?;
            slot.reader()?.export(&ExportRequest {
                output,
                formats: ExportFormat::or_default(formats),
                tables,
                since_ms,
                until_ms,
                storage,
            })
        }
        _ => bail!("not a dataset command"),
    }
}
