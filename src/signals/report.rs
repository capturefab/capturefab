//! Human-readable output for `signals` and `log` (the JSON envelope is the
//! contract; this is for people at a terminal).
use serde_json::Value as Json;
use std::fmt::Write;

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

/// An aligned `tag  value unit  quality  age` table.
fn table(out: &mut String, rows: &[[String; 4]]) {
    let widths: Vec<usize> = (0..3)
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    for row in rows {
        let _ = writeln!(
            out,
            "  {:w0$}  {:w1$}  {:w2$}  {}",
            row[0],
            row[1],
            row[2],
            row[3],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2]
        );
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
                age(t["ts_ms"].as_u64().unwrap_or(0)),
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
                    age(s["ts_ms"].as_u64().unwrap_or(0)),
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
