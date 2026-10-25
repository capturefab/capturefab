//! `dataset export --media`: copy the cataloged images (and recordings) into
//! the export so it is a self-contained folder that can be moved or zipped.
//! Files go to `images/` and `videos/`, optionally one folder per value of a
//! capture tag, and `media.csv` joins each copy to its row by `export_path`.
//! Only local `path` values are read; remote URLs are never followed or
//! written. Every copy goes through the storage ledger and is checked
//! against the row's SHA-256.
use super::{ExportRequest, ExportSource};
use crate::{dataset::MediaExport, storage::StoragePolicy};
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

/// Files one export may copy; narrow --since/--until for more.
pub(super) const MAX_FILES: usize = 100_000;
/// Skipped files listed by id in the manifest and result; the rest are counted.
const MAX_SKIPPED_LISTED: usize = 100;
/// The folder for files whose group tag is empty.
pub(super) const UNLABELED: &str = "_unlabeled";
/// Room per row for the index files, beyond the export path and group.
const INDEX_ROW_BYTES: u64 = 256;

/// One cataloged file, read inside the export's snapshot.
pub(super) struct Row {
    table: &'static str,
    id: u64,
    path: String,
    file_name: String,
    sha256: Option<String>,
    bytes: u64,
    context: Option<String>,
}

struct Planned {
    row: Row,
    export_path: String,
    group: Option<String>,
}

/// The files to copy and the ones left out, decided before anything is written.
pub(super) struct Plan {
    items: Vec<Planned>,
    skipped: Vec<(&'static str, u64, String)>,
    skipped_total: u64,
    /// The index files this export writes, by format extension.
    index_formats: Vec<&'static str>,
}

/// What was copied, for the manifest, the README and the result.
#[derive(Default)]
pub(super) struct Copied {
    pub rows: Vec<Value>,
    pub bytes: u64,
}

impl ExportSource<'_> {
    /// The cataloged files the request asks to copy, in id order.
    pub(super) fn media_rows(&self, request: &ExportRequest) -> Result<Vec<Row>> {
        let tables: &[&'static str] = match request.media {
            MediaExport::None => &[],
            MediaExport::Images => &["captures"],
            MediaExport::All => &["captures", "recordings"],
        };
        let mut rows = Vec::new();
        for &table in tables {
            let mut statement = self.conn.prepare(&format!(
                "SELECT id, path, file_name, sha256, bytes, context FROM {table}{} ORDER BY id LIMIT {}",
                self.filter(table, request),
                MAX_FILES + 1
            ))?;
            let found = statement
                .query_map([], |r| {
                    Ok(Row {
                        table,
                        id: r.get(0)?,
                        path: r.get(1)?,
                        file_name: r.get(2)?,
                        sha256: r.get(3)?,
                        bytes: r.get(4)?,
                        context: r.get(5)?,
                    })
                })?
                .collect::<duckdb::Result<Vec<_>>>()?;
            rows.extend(found);
            if rows.len() > MAX_FILES {
                return Err(crate::dataset::Usage(format!(
                    "dataset export copies at most {MAX_FILES} files; narrow --since/--until or use --media images"
                ))
                .into());
            }
        }
        Ok(rows)
    }
}

/// A file or folder name made safe and never hidden: `.x` becomes `_.x`.
fn component(value: &str) -> Option<String> {
    crate::naming::sanitize_component(value).ok()
}

/// `name` with `-suffix` before its extension.
fn with_suffix(name: &str, suffix: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem}-{suffix}.{ext}"),
        _ => format!("{name}-{suffix}"),
    }
}

/// The value of tag `key` in a row's context JSON, if it has one.
fn tag(context: Option<&str>, key: &str) -> Option<String> {
    let fields: serde_json::Map<String, Value> = serde_json::from_str(context?).ok()?;
    let value = fields.get(key)?;
    let text = value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned);
    (!text.trim().is_empty()).then_some(text)
}

impl Plan {
    /// Name every file and check its source, without writing anything.
    /// Missing files and files whose size changed are skipped, not fatal.
    pub(super) fn new(rows: Vec<Row>, group_by: Option<&str>, formats: &[&'static str]) -> Self {
        let mut plan = Plan {
            items: Vec::new(),
            skipped: Vec::new(),
            skipped_total: 0,
            index_formats: std::iter::once("csv")
                .chain(formats.iter().copied().filter(|f| *f != "csv"))
                .collect(),
        };
        // Case-insensitive, as on macOS and Windows volumes.
        let mut taken = HashSet::new();
        for row in rows {
            let path = Path::new(&row.path);
            let reason = if !path.is_absolute() {
                Some("not a local file path".to_owned())
            } else {
                match fs::metadata(path) {
                    Err(_) => Some("missing".to_owned()),
                    Ok(meta) if !meta.is_file() => Some("not a file".to_owned()),
                    Ok(meta) if meta.len() != row.bytes => {
                        Some("changed since capture (size differs)".to_owned())
                    }
                    Ok(_) => None,
                }
            };
            if let Some(reason) = reason {
                plan.skip(row.table, row.id, reason);
                continue;
            }
            let base = if row.table == "recordings" {
                "videos"
            } else {
                "images"
            };
            let group = group_by.map(|key| {
                tag(row.context.as_deref(), key)
                    .and_then(|value| component(&value))
                    .unwrap_or_else(|| UNLABELED.into())
            });
            let folder = match &group {
                Some(group) => format!("{base}/{group}"),
                None => base.to_owned(),
            };
            let name =
                component(&row.file_name).unwrap_or_else(|| format!("{}-{}", row.table, row.id));
            let mut export_path = format!("{folder}/{name}");
            let mut attempt = 0u32;
            while !taken.insert(export_path.to_lowercase()) {
                attempt += 1;
                let suffix = if attempt == 1 {
                    row.id.to_string()
                } else {
                    format!("{}-{attempt}", row.id)
                };
                export_path = format!("{folder}/{}", with_suffix(&name, &suffix));
            }
            plan.items.push(Planned {
                row,
                export_path,
                group,
            });
        }
        plan
    }

    fn skip(&mut self, table: &'static str, id: u64, reason: String) {
        self.skipped_total += 1;
        if self.skipped.len() < MAX_SKIPPED_LISTED {
            self.skipped.push((table, id, reason));
        }
    }

    /// Bytes and files the copies and index files may take, for preflight.
    pub(super) fn needs(&self) -> (u64, u64) {
        let copies: u64 = self.items.iter().map(|i| i.row.bytes).sum();
        let index_rows: u64 = self
            .items
            .iter()
            .map(|i| {
                i.export_path.len() as u64
                    + i.group.as_ref().map_or(0, |g| g.len() as u64)
                    + INDEX_ROW_BYTES
            })
            .sum();
        let formats = self.index_formats.len() as u64;
        (
            copies + (index_rows + INDEX_ROW_BYTES) * formats,
            self.items.len() as u64 + formats,
        )
    }

    pub(super) fn index_formats(&self) -> &[&'static str] {
        &self.index_formats
    }

    /// Copy every planned file under `output`. A source that went away or
    /// no longer matches its SHA-256 is skipped; a storage error stops the
    /// export with the files already written left in place.
    pub(super) fn copy(&mut self, output: &Path, policy: &StoragePolicy) -> Result<Copied> {
        let mut copied = Copied::default();
        for item in std::mem::take(&mut self.items) {
            let target = resolve(output, &item.export_path);
            match copy_verified(
                Path::new(&item.row.path),
                &target,
                policy,
                item.row.sha256.as_deref(),
            )? {
                Ok((bytes, sha256)) => {
                    copied.bytes += bytes;
                    copied.rows.push(json!({
                        "table": item.row.table,
                        "id": item.row.id,
                        "export_path": item.export_path,
                        "sha256": sha256,
                        "bytes": bytes,
                        "group": item.group,
                    }));
                }
                Err(reason) => self.skip(item.row.table, item.row.id, reason),
            }
        }
        Ok(copied)
    }

    /// The `media` object of the manifest and the result.
    pub(super) fn summary(&self, request: &ExportRequest, copied: &Copied) -> Value {
        json!({
            "mode": request.media,
            "group_by": request.group_by,
            "copied": copied.rows.len(),
            "bytes": copied.bytes,
            "skipped": self.skipped_total,
            "skipped_files": self.skipped.iter().map(|(table, id, reason)| {
                json!({"table": table, "id": id, "reason": reason})
            }).collect::<Vec<_>>(),
        })
    }
}

/// Copy `from` into a new ledger-tracked file at `to`. The inner `Err` is why
/// the file was skipped: it went away, or its SHA-256 is not the cataloged one.
fn copy_verified(
    from: &Path,
    to: &Path,
    policy: &StoragePolicy,
    expected: Option<&str>,
) -> Result<std::result::Result<(u64, String), String>> {
    let Ok(mut source) = fs::File::open(from) else {
        return Ok(Err("missing".into()));
    };
    let len = source.metadata()?.len();
    let mut writer = crate::storage::create_writer(to, policy, len.max(1))?;
    if let Err(e) = std::io::copy(&mut source, &mut writer) {
        let _ = writer.abort();
        return Err(anyhow!(e).context(format!("write {}", to.display())));
    }
    let (bytes, sha256) = (writer.bytes_written(), writer.sha256());
    if expected.is_some_and(|expected| !expected.eq_ignore_ascii_case(&sha256)) {
        writer.abort()?;
        return Ok(Err("changed since capture (SHA-256 differs)".into()));
    }
    writer.finish()?;
    Ok(Ok((bytes, sha256)))
}

const COLUMNS: [&str; 6] = ["table", "id", "export_path", "sha256", "bytes", "group"];

fn csv_field(value: &Value) -> String {
    let text = match value {
        Value::Null => return String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text
    }
}

/// `media.csv`: one header line, then one line per copied file.
pub(super) fn csv(rows: &[Value]) -> String {
    let mut out = COLUMNS.join(",");
    out.push('\n');
    for row in rows {
        let fields: Vec<String> = COLUMNS.iter().map(|c| csv_field(&row[c])).collect();
        out.push_str(&fields.join(","));
        out.push('\n');
    }
    out
}

/// `media.json` (an array) or `media.jsonl` (one object per line).
pub(super) fn json_text(rows: &[Value], lines: bool) -> Result<String> {
    if lines {
        let mut out = String::new();
        for row in rows {
            out.push_str(&serde_json::to_string(row)?);
            out.push('\n');
        }
        Ok(out)
    } else {
        Ok(serde_json::to_string(rows)?)
    }
}

/// The staged CSV read back by DuckDB into Parquet, with fixed column types.
pub(super) fn parquet_select(staged_csv: &str) -> String {
    format!(
        "SELECT * FROM read_csv('{staged_csv}', header = true, columns = {{'table': 'VARCHAR', 'id': 'UBIGINT', 'export_path': 'VARCHAR', 'sha256': 'VARCHAR', 'bytes': 'UBIGINT', 'group': 'VARCHAR'}})"
    )
}

/// A relative export path as a path under `output`.
pub(super) fn resolve(output: &Path, export_path: &str) -> PathBuf {
    export_path
        .split('/')
        .fold(output.to_path_buf(), |path, part| path.join(part))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_safe_unique_and_never_hidden() {
        assert_eq!(component("../x").as_deref(), Some("_.._x"));
        assert_eq!(component("a/b").as_deref(), Some("a_b"));
        assert_eq!(component(".."), None);
        assert_eq!(with_suffix("a.png", "7"), "a-7.png");
        assert_eq!(with_suffix("noext", "7"), "noext-7");
        assert_eq!(tag(Some(r#"{"part":"A"}"#), "part").as_deref(), Some("A"));
        assert_eq!(tag(Some(r#"{"part":" "}"#), "part"), None);
        assert_eq!(tag(None, "part"), None);
        let csv = csv(&[
            json!({"table":"captures","id":1,"export_path":"images/a,b.png","sha256":"x","bytes":3,"group":null}),
        ]);
        assert_eq!(
            csv,
            "table,id,export_path,sha256,bytes,group\ncaptures,1,\"images/a,b.png\",x,3,\n"
        );
    }
}
