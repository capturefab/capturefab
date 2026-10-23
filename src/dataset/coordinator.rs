//! Coordinator arms for the dataset commands. None of them touch a camera
//! worker, so none take a command turn.
use super::{ExportFormat, Slot, export::ExportRequest};
use crate::session::SessionCommand;
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

fn absolute(path: &str, what: &str) -> Result<PathBuf> {
    let path = PathBuf::from(path);
    ensure!(path.is_absolute(), "{what} must be absolute");
    Ok(path)
}

pub fn run(slot: &Arc<Slot>, command: SessionCommand) -> Result<Value> {
    match command {
        SessionCommand::DatasetStatus => Ok(serde_json::to_value(slot.status_cached())?),
        SessionCommand::DatasetOpen {
            path,
            mode,
            max_bytes,
        } => open(slot, &path, mode, max_bytes, None),
        SessionCommand::DatasetOpenIf {
            expected_slot_id,
            path,
            mode,
            max_bytes,
        } => open(
            slot,
            &path,
            mode,
            max_bytes,
            Some(expected_slot_id.as_deref()),
        ),
        SessionCommand::DatasetClose => close(slot, None),
        SessionCommand::DatasetCloseIf { slot_id } => close(slot, Some(&slot_id)),
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
    Ok(serde_json::to_value(result?)?)
}

fn close(slot: &Arc<Slot>, expected: Option<&str>) -> Result<Value> {
    let _lifecycle = slot.lifecycle();
    if let Some(id) = expected {
        slot.ensure_slot(Some(id))?;
    }
    slot.set_auto_path(None);
    let closed = slot.close()?;
    Ok(json!({"closed": closed}))
}
