//! Human-readable output for `signals` and `log` (the JSON envelope is the
//! contract; this is for people at a terminal).
use serde_json::Value as Json;
use std::fmt::Write;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Widest cell shown; longer text is cut with an ellipsis (JSON has it all).
const MAX_CELL: usize = 60;

fn age(ts_ms: u64) -> String {
    let ms = crate::scheduling::now_ms().saturating_sub(ts_ms);
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 120_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m", ms / 60_000)
    }
}

fn value_text(value: &Json) -> String {
    match value {
        Json::String(s) => s.clone(),
        Json::Null => String::new(),
        Json::Number(n) => match n.as_f64() {
            Some(x) if x.fract() != 0.0 => format!("{}", (x * 1e4).round() / 1e4),
            Some(x) if x.abs() < 1e15 => format!("{x:.0}"),
            _ => n.to_string(),
        },
        other => other.to_string(),
    }
}

/// `text` on one line, at most `MAX_CELL` terminal columns wide.
fn cell(text: &str) -> String {
    let text = text.replace(['\n', '\r', '\t'], " ");
    if text.width() <= MAX_CELL {
        return text;
    }
    let mut out = String::new();
    let mut width = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if width + w > MAX_CELL - 1 {
            break;
        }
        width += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// An aligned `tag  value unit  quality  age` table, padded by terminal
/// column width so wide (CJK) characters line up.
fn table(out: &mut String, rows: &[[String; 4]]) {
    let rows: Vec<[String; 4]> = rows
        .iter()
        .map(|row| [cell(&row[0]), cell(&row[1]), cell(&row[2]), cell(&row[3])])
        .collect();
    let widths: Vec<usize> = (0..3)
        .map(|i| rows.iter().map(|r| r[i].width()).max().unwrap_or(0))
        .collect();
    for row in &rows {
        let mut line = String::from(" ");
        for (i, width) in widths.iter().enumerate() {
            line.push(' ');
            line.push_str(&row[i]);
            line.push_str(&" ".repeat(width - row[i].width() + 1));
        }
        line.push_str(&row[3]);
        let _ = writeln!(out, "{}", line.trim_end());
    }
}

/// How old a sample is, or nothing for a bad sample without a device time
/// (a missing value, whose host read time says little).
fn sample_age(sample: &Json) -> String {
    if sample["quality"] == "bad" && sample["source_ts_ms"].is_null() {
        return String::new();
    }
    match sample["ts_ms"].as_u64() {
        Some(ts) if ts > 0 => age(ts),
        _ => String::new(),
    }
}

fn source(out: &mut String, s: &Json) {
    let _ = writeln!(
        out,
        "{}  {}  {}  {} · {} samples · {} written · {} dropped",
        s["id"].as_str().unwrap_or(""),
        s["protocol"].as_str().unwrap_or(""),
        s["uri"].as_str().unwrap_or(""),
        s["state"].as_str().unwrap_or(""),
        s["samples"],
        s["written"],
        s["dropped"].as_u64().unwrap_or(0) + s["helper_dropped"].as_u64().unwrap_or(0),
    );
    if let Some(error) = s["error"].as_str() {
        let _ = writeln!(out, "  error: {error}");
    }
    let rows: Vec<[String; 4]> = s["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| {
            let unit = t["unit"]
                .as_str()
                .map(|u| format!(" {u}"))
                .unwrap_or_default();
            [
                t["tag"].as_str().unwrap_or("").to_string(),
                format!("{}{unit}", value_text(&t["value"])),
                t["quality"].as_str().unwrap_or("").to_string(),
                sample_age(t),
            ]
        })
        .collect();
    table(out, &rows);
    if s["tags_truncated"] == true {
        let _ = writeln!(out, "  ({} tags in all)", s["tags_total"]);
    }
}

/// Text for a `signals ...` or `log` result.
pub fn human(value: &Json) -> String {
    let mut out = String::new();
    if let Some(stopped) = value["stopped"].as_array() {
        let _ = writeln!(out, "Stopped {} source(s)", stopped.len());
        for s in stopped {
            let _ = writeln!(
                out,
                "  {}  {}",
                s["id"].as_str().unwrap_or(""),
                s["uri"].as_str().unwrap_or("")
            );
        }
    } else if let Some(sources) = value["sources"].as_array() {
        if sources.is_empty() {
            let _ = writeln!(out, "No signal sources.");
        }
        for s in sources {
            source(&mut out, s);
        }
        if let Some(samples) = value["samples"].as_u64() {
            let mut targets: Vec<String> = Vec::new();
            for key in ["dataset", "csv"] {
                if let Some(path) = value[key].as_str() {
                    targets.push(path.to_string());
                }
            }
            let _ = writeln!(
                out,
                "{samples} samples, {} written to {}, {} dropped",
                value["written"],
                targets.join(" and "),
                value["dropped"]
            );
        } else if value["persisting"] == false && !sources.is_empty() {
            let _ = writeln!(out, "Not persisting: open a dataset to keep samples.");
        }
    } else if let Some(items) = value["items"].as_array() {
        let rows: Vec<[String; 4]> = items
            .iter()
            .map(|i| {
                [
                    i["id"].as_str().unwrap_or("").to_string(),
                    i["kind"].as_str().unwrap_or("").to_string(),
                    i["data_type"].as_str().unwrap_or("").to_string(),
                    value_text(&i["value"]),
                ]
            })
            .collect();
        table(&mut out, &rows);
        if value["truncated"] == true {
            let _ = writeln!(out, "(more items; raise --limit or --duration)");
        }
    } else if let Some(samples) = value["samples"].as_array() {
        let units: std::collections::BTreeMap<&str, &str> = value["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| Some((t["tag"].as_str()?, t["unit"].as_str()?)))
            .collect();
        let rows: Vec<[String; 4]> = samples
            .iter()
            .map(|s| {
                let tag = s["tag"].as_str().unwrap_or("");
                let unit = units.get(tag).map(|u| format!(" {u}")).unwrap_or_default();
                [
                    tag.to_string(),
                    format!("{}{unit}", value_text(&s["value"])),
                    s["quality"].as_str().unwrap_or("").to_string(),
                    sample_age(s),
                ]
            })
            .collect();
        table(&mut out, &rows);
    } else if value["id"].is_string() {
        source(&mut out, value);
    } else {
        let _ = writeln!(out, "{value:#}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tables_align_wide_text_and_cut_long_values() {
        let mut out = String::new();
        let long = "x".repeat(1000);
        table(
            &mut out,
            &[
                ["温度".into(), "1".into(), "good".into(), "1s".into()],
                ["temp".into(), long, "good".into(), "2s".into()],
            ],
        );
        let lines: Vec<&str> = out.lines().collect();
        let column = |line: &str, at: usize| line[..at].width();
        assert_eq!(
            column(lines[0], lines[0].find("1 ").unwrap()),
            column(lines[1], lines[1].find('x').unwrap())
        );
        assert!(lines[1].width() < 100 && lines[1].contains('…'), "{out}");
        assert!(lines[0].width() < 100, "{out}");
    }

    #[test]
    fn bad_samples_without_device_time_show_no_age() {
        let now = crate::scheduling::now_ms();
        let bad = json!({"quality":"bad","ts_ms":now,"source_ts_ms":null});
        assert_eq!(sample_age(&bad), "");
        let good = json!({"quality":"good","ts_ms":now,"source_ts_ms":null});
        assert!(sample_age(&good).ends_with("ms"));
    }
}
