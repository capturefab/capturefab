//! Prompt-defined numeric measurements share the session's in-memory database,
//! but have their own retention and row budget. Images and credentials never
//! enter this store.
use super::*;
use crate::ai::{MeasurementDefinition, MeasurementOutput, ModelConfig, Provider};
use serde::Deserialize;

pub const MAX_MEASUREMENT_SAMPLES: usize = 10_000;
const MAX_MEASUREMENT_SAMPLE_BYTES: usize = 8_192;
const MAX_MEASUREMENT_PANELS: usize = 24;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeasurementSample {
    pub timestamp_ms: u64,
    pub camera_id: String,
    pub frame_id: u64,
    pub definition: MeasurementDefinition,
    pub provider: Provider,
    pub model: String,
    pub result: MeasurementOutput,
}

pub(super) fn initialize_database(db: &Connection) -> Result<()> {
    // Additive initialization leaves capture counters and existing measurement
    // rows untouched, including when initialization is repeated.
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS measurement_samples (
            sequence UBIGINT NOT NULL, timestamp_ms UBIGINT NOT NULL,
            value DOUBLE, confidence DOUBLE,
            sample_json VARCHAR NOT NULL
        )",
    )
    .context("initialize prompt measurement analytics")?;
    Ok(())
}

pub(super) fn prune_measurements(db: &Connection, now: u64) -> Result<()> {
    db.execute(
        "DELETE FROM measurement_samples WHERE timestamp_ms < ?",
        params![now.saturating_sub(RETENTION_SECONDS * 1000)],
    )?;
    let count: u64 = db.query_row("SELECT count(*) FROM measurement_samples", [], |row| {
        row.get(0)
    })?;
    if count > MAX_MEASUREMENT_SAMPLES as u64 {
        db.execute(
            "DELETE FROM measurement_samples WHERE sequence IN (
                SELECT sequence FROM measurement_samples
                ORDER BY timestamp_ms, sequence LIMIT ?
            )",
            params![count - MAX_MEASUREMENT_SAMPLES as u64],
        )?;
    }
    Ok(())
}

impl Analytics {
    /// Validate the entire response before opening a transaction. A rejected
    /// output cannot leave a partial batch in the numeric history.
    pub fn record_measurements(&self, samples: &[MeasurementSample]) -> Result<()> {
        anyhow::ensure!(
            samples.len() <= MAX_MEASUREMENT_SAMPLES,
            "measurement batch exceeds the session row limit"
        );
        let encoded = samples
            .iter()
            .map(|sample| {
                validate_sample(sample)?;
                let encoded = serde_json::to_string(sample)?;
                anyhow::ensure!(
                    encoded.len() <= MAX_MEASUREMENT_SAMPLE_BYTES,
                    "measurement exceeds the maximum stored size"
                );
                Ok(encoded)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut store = self.store.lock().unwrap_or_else(|error| error.into_inner());
        let now = store.now();
        let db = store
            .db
            .as_mut()
            .context("analytics database is unavailable")?;
        let transaction = db.transaction()?;
        let last: Option<u64> =
            transaction.query_row("SELECT max(sequence) FROM measurement_samples", [], |row| {
                row.get(0)
            })?;
        let first = match last {
            Some(last) => last
                .checked_add(1)
                .context("measurement sequence exhausted")?,
            None => 0,
        };
        {
            let mut statement = transaction.prepare(
                "INSERT INTO measurement_samples (sequence, timestamp_ms, value, confidence, sample_json)
                 VALUES (?, ?, ?, ?, ?)",
            )?;
            for (offset, (sample, encoded)) in samples.iter().zip(encoded.iter()).enumerate() {
                let sequence = first
                    .checked_add(offset as u64)
                    .context("measurement sequence exhausted")?;
                statement.execute(params![
                    sequence,
                    sample.timestamp_ms,
                    sample.result.value,
                    sample.result.confidence,
                    encoded
                ])?;
            }
        }
        prune_measurements(&transaction, now)?;
        transaction.commit()?;
        Ok(())
    }

    /// Return the retained observations in capture-time order. A missing value
    /// stays missing: models cannot turn an unknown quantity into a zero.
    pub fn measurement_history(&self) -> Result<Vec<MeasurementSample>> {
        let encoded = {
            let store = self.store.lock().unwrap_or_else(|error| error.into_inner());
            let db = store
                .db
                .as_ref()
                .context("analytics database is unavailable")?;
            prune_measurements(db, store.now())?;
            let mut statement = db.prepare(
                "SELECT sample_json FROM measurement_samples
                 ORDER BY timestamp_ms, sequence LIMIT ?",
            )?;
            statement
                .query_map(params![MAX_MEASUREMENT_SAMPLES as u64], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<duckdb::Result<Vec<_>>>()?
        };
        encoded
            .iter()
            .map(|encoded| serde_json::from_str(encoded).context("read stored prompt measurement"))
            .collect()
    }

    pub fn measurement_svg(&self) -> Result<String> {
        render_measurement_svg(&self.measurement_history()?)
    }
}

fn validate_sample(sample: &MeasurementSample) -> Result<()> {
    anyhow::ensure!(
        !sample.camera_id.trim().is_empty()
            && sample.camera_id.len() <= MAX_LABEL_BYTES
            && !sample.camera_id.contains('\0'),
        "measurement camera identity is empty or too long"
    );
    ModelConfig {
        provider: sample.provider,
        model: sample.model.clone(),
    }
    .validate()?;
    // Field-specific checks follow the provider's structured output contract.
    validate_measurement_result(&sample.definition, &sample.result)
}

fn validate_measurement_result(
    definition: &MeasurementDefinition,
    result: &MeasurementOutput,
) -> Result<()> {
    crate::ai::validate_outputs(
        std::slice::from_ref(definition),
        std::slice::from_ref(result),
    )
}

fn series_identity(sample: &MeasurementSample) -> Result<String> {
    // Include the complete definition, so edited prompts or units never join
    // incompatible observations even if an application reuses an ID.
    serde_json::to_string(&(
        &sample.camera_id,
        &sample.definition,
        &sample.provider,
        &sample.model,
    ))
    .context("identify prompt measurement series")
}

/// Draw each numeric series in its own panel. Distinct units, cameras, models
/// and changed definitions are never combined on an axis. Null results break
/// line segments rather than appearing as zero or interpolated estimates.
pub fn render_measurement_svg(samples: &[MeasurementSample]) -> Result<String> {
    anyhow::ensure!(
        samples.len() <= MAX_MEASUREMENT_SAMPLES,
        "measurement chart exceeds the session row limit"
    );
    let mut grouped: BTreeMap<String, Vec<&MeasurementSample>> = BTreeMap::new();
    for sample in samples {
        validate_sample(sample)?;
        grouped
            .entry(series_identity(sample)?)
            .or_default()
            .push(sample);
    }
    for series in grouped.values_mut() {
        series.sort_by_key(|sample| (sample.timestamp_ms, sample.frame_id));
    }
    let mut series: Vec<_> = grouped.into_values().collect();
    series.sort_by_key(|series| std::cmp::Reverse(series.last().unwrap().timestamp_ms));
    let omitted = series.len().saturating_sub(MAX_MEASUREMENT_PANELS);
    series.truncate(MAX_MEASUREMENT_PANELS);
    let height = 96 + 280 * series.len().max(1) as u32;
    let mut svg = String::with_capacity(30_000);
    {
        let root = SVGBackend::with_string(&mut svg, (1200, height)).into_drawing_area();
        root.fill(&BACKGROUND).map_err(plot_error)?;
        root.draw(&Text::new(
            "Prompt measurements",
            (28, 33),
            ("sans-serif", 24).into_font().color(&TEXT),
        ))
        .map_err(plot_error)?;
        let subtitle = if omitted > 0 {
            format!(
                "Showing the {MAX_MEASUREMENT_PANELS} most recently observed series; {omitted} more remain in the session history."
            )
        } else {
            "Model estimates for this session. Unknown values remain gaps; units and camera identities stay separate.".to_owned()
        };
        root.draw(&Text::new(
            subtitle,
            (28, 61),
            ("sans-serif", 14).into_font().color(&MUTED),
        ))
        .map_err(plot_error)?;
        if series.is_empty() {
            root.draw(&Text::new(
                "Add a measurement prompt and analyze a camera frame to start collecting values.",
                (140, 210),
                ("sans-serif", 20).into_font().color(&TEXT),
            ))
            .map_err(plot_error)?;
        } else {
            let panels = root.margin(78, 18, 10, 10).split_evenly((series.len(), 1));
            for (area, series) in panels.iter().zip(&series) {
                draw_measurement_series(area, series)?;
            }
        }
        root.present().map_err(plot_error)?;
    }
    svg = svg.replacen(
        "<svg ",
        "<svg role=\"img\" aria-labelledby=\"measurements-title measurements-description\" ",
        1,
    );
    if let Some(position) = svg.find('>') {
        svg.insert_str(position + 1, "<title id=\"measurements-title\">Prompt-defined video measurements</title><desc id=\"measurements-description\">Each panel contains one camera, measurement definition, provider and model. Unknown values are gaps. Data is retained in memory for at most twenty-four hours.</desc>");
    }
    Ok(svg)
}

fn draw_measurement_series(
    area: &DrawingArea<SVGBackend<'_>, Shift>,
    samples: &[&MeasurementSample],
) -> Result<()> {
    let first = samples[0];
    let label = chart_label(&first.definition.label, 54);
    let camera = chart_label(&first.camera_id, 26);
    let model = chart_label(&first.model, 28);
    let provider = match &first.provider {
        Provider::OpenAi => "OpenAI",
        Provider::Gemini => "Gemini",
        Provider::Claude => "Claude",
    };
    let caption = format!("{label} · {camera} · {provider} / {model}");
    let start = first.timestamp_ms;
    let end = samples
        .last()
        .unwrap()
        .timestamp_ms
        .max(start.saturating_add(1000));
    let span = (end.saturating_sub(start) as f64 / 1000.0).max(1.0);
    let magnitude = samples
        .iter()
        .filter_map(|sample| sample.result.value)
        .map(f64::abs)
        .fold(0.0_f64, f64::max);
    let exponent = if magnitude > 1e12 || (magnitude > 0.0 && magnitude < 1e-3) {
        magnitude.log10().floor().clamp(-300.0, 300.0) as i32
    } else {
        0
    };
    let scale = 10_f64.powi(exponent);
    let unit = chart_label(&first.definition.unit, 24);
    let unit = if exponent == 0 {
        if unit.is_empty() {
            "value".to_owned()
        } else {
            unit
        }
    } else {
        format!("{unit} × 10^{exponent}")
    };
    let (low, high) = samples
        .iter()
        .filter_map(|sample| sample.result.value)
        .map(|value| value / scale)
        .fold((0.0_f64, 0.0_f64), |(low, high), value| {
            (low.min(value), high.max(value))
        });
    let spread = high - low;
    let padding = if spread > 0.0 { spread * 0.12 } else { 0.12 };
    let mut chart = ChartBuilder::on(area)
        .margin(16)
        .caption(caption, ("sans-serif", 18).into_font().color(&TEXT))
        .x_label_area_size(42)
        .y_label_area_size(86)
        .build_cartesian_2d(0.0..span, low - padding..high + padding)
        .map_err(plot_error)?;
    chart
        .configure_mesh()
        .disable_x_mesh()
        .light_line_style(MUTED.mix(0.12))
        .bold_line_style(MUTED.mix(0.2))
        .axis_style(MUTED.mix(0.6))
        .label_style(("sans-serif", 12).into_font().color(&MUTED))
        .x_labels(5)
        .y_labels(4)
        .x_label_formatter(&|seconds| duration_label(*seconds))
        .x_desc("Time in this series")
        .y_desc(&unit)
        .axis_desc_style(("sans-serif", 12).into_font().color(&MUTED))
        .draw()
        .map_err(plot_error)?;
    for segment in numeric_segments(samples, start, scale) {
        if segment.len() > 1 {
            chart
                .draw_series(LineSeries::new(segment.clone(), ACCENT.stroke_width(2)))
                .map_err(plot_error)?;
        }
        chart
            .draw_series(
                segment
                    .into_iter()
                    .map(|point| Circle::new(point, 3, ACCENT.filled())),
            )
            .map_err(plot_error)?;
    }
    if samples.iter().all(|sample| sample.result.value.is_none()) {
        chart
            .draw_series(std::iter::once(Text::new(
                "No numeric value available",
                (span * 0.35, (low + high) * 0.5),
                ("sans-serif", 15).into_font().color(&MUTED),
            )))
            .map_err(plot_error)?;
    }
    Ok(())
}

fn numeric_segments(
    samples: &[&MeasurementSample],
    start: u64,
    scale: f64,
) -> Vec<Vec<(f64, f64)>> {
    let mut segments = Vec::new();
    let mut current = Vec::new();
    for sample in samples {
        match sample.result.value {
            Some(value) => current.push((
                sample.timestamp_ms.saturating_sub(start) as f64 / 1000.0,
                value / scale,
            )),
            None if !current.is_empty() => segments.push(std::mem::take(&mut current)),
            None => {}
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

fn chart_label(value: &str, budget: u32) -> String {
    // Keep XML 1.0-invalid controls out of SVG text. Plotters escapes the
    // remaining markup characters before writing text elements.
    let printable: String = value
        .chars()
        .filter(|character| {
            matches!(*character, '\t' | '\n' | '\r')
                || (*character >= '\u{20}' && !matches!(*character, '\u{fffe}' | '\u{ffff}'))
        })
        .collect();
    interface_camera_name(&printable, budget)
}

#[cfg(test)]
mod tests;
