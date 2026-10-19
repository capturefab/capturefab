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
        } => open(slot, signals, &path, mode, max_bytes, None),
        SessionCommand::DatasetOpenIf {
            expected_slot_id,
            path,
            mode,
            max_bytes,
        } => open(
            slot,
            signals,
            &path,
            mode,
            max_bytes,
            Some(expected_slot_id.as_deref()),
        ),
        SessionCommand::DatasetClose => close(slot, signals, None, None),
        SessionCommand::DatasetCloseIf {
            slot_id,
            sink_generation,
        } => close(slot, signals, Some(&slot_id), sink_generation),
        SessionCommand::DatasetNote {
            text,
            context,
            camera,
        } => match slot.record_event("note", camera.as_deref(), None, &text, &context)? {
            Some(id) => Ok(json!({"id": id})),
            None => bail!("no dataset is open; run `capturefab dataset open FILE.duckdb`"),
        },
        SessionCommand::DatasetNoteIf {
            slot_id,
            text,
            context,
            camera,
        } => Ok(json!({"id": slot.note_if(&slot_id, camera.as_deref(), &text, &context)?})),
        SessionCommand::DatasetQuery {
            sql,
            limit,
            timeout_ms,
        } => slot.reader()?.query(&sql, limit, timeout_ms),
        SessionCommand::DatasetQueryIf {
            slot_id,
            sql,
            limit,
            timeout_ms,
            max_result_bytes,
        } => slot.reader_if(&slot_id)?.query_bounded(
            &sql,
            limit,
            timeout_ms,
            max_result_bytes as usize,
        ),
        SessionCommand::DatasetModelQueryIf {
            slot_id,
            sql,
            limit,
            timeout_ms,
            max_result_bytes,
        } => slot.reader_if(&slot_id)?.query_model_bounded(
            &sql,
            limit,
            timeout_ms,
            max_result_bytes as usize,
        ),
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
        SessionCommand::DatasetExportIf {
            slot_id,
            output,
            approved_root,
            formats,
            tables,
            since_ms,
            until_ms,
            storage,
            max_bytes,
            timeout_ms,
        } => {
            let output = absolute(&output, "export folder")?;
            let approved_root = approved_root
                .as_deref()
                .map(|root| absolute(root, "approved export root"))
                .transpose()?;
            slot.reader_if(&slot_id)?.export_bounded_in(
                &ExportRequest {
                    output,
                    formats,
                    tables,
                    since_ms,
                    until_ms,
                    storage,
                },
                max_bytes,
                timeout_ms,
                approved_root.as_deref(),
            )
        }
        _ => bail!("not a dataset command"),
    }
}

fn open(
    slot: &Arc<Slot>,
    signals: &SignalHub,
    path: &str,
    mode: super::OpenMode,
    max_bytes: Option<u64>,
    expected: Option<Option<&str>>,
) -> Result<Value> {
    let path = absolute(path, "dataset path")?;
    let _lifecycle = slot.lifecycle();
    let result = match expected {
        Some(id) => slot.open_if(id, &path, mode, max_bytes),
        None => {
            slot.set_auto_path(None);
            slot.open(&path, mode, max_bytes)
        }
    };
    let status = match result {
        Ok(status) => status,
        Err(error) => {
            // A stale conditional command cannot change even the sink of an
            // empty slot; only a valid open that failed may detach it.
            if expected.is_none() && slot.status_cached().is_none() && signals.has_sink() {
                let _ = signals.set_sink(None, SINK_TIMEOUT);
            }
            return Err(error);
        }
    };
    // Never hold a writer lock here: the hub's writer thread takes it.
    if !signals.has_sink() {
        let sink: Arc<dyn crate::signals::SampleSink> = slot.clone();
        if let Err(e) = signals.set_sink(Some(sink), SINK_TIMEOUT) {
            eprintln!("capturefab: signal samples will not reach the dataset: {e:#}");
        }
    }
    // This receipt is captured while the dataset lifecycle is still held, so
    // later plan steps bind the sink actually acknowledged by this open.
    let mut result = serde_json::to_value(status)?;
    result["sink_generation"] = json!(signals.sink_status().generation);
    Ok(result)
}

fn close(
    slot: &Arc<Slot>,
    signals: &SignalHub,
    expected: Option<&str>,
    sink_generation: Option<u64>,
) -> Result<Value> {
    let _lifecycle = slot.lifecycle();
    if let Some(id) = expected {
        slot.ensure_slot(Some(id))?;
    }
    if signals.has_sink() || sink_generation.is_some() {
        signals.set_sink_if_generation(None, sink_generation, SINK_TIMEOUT)?;
    }
    slot.set_auto_path(None);
    let closed = slot.close()?;
    Ok(json!({"closed": closed, "sink_generation": signals.sink_status().generation}))
}
