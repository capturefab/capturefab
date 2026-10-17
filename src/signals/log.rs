//! `capturefab log`: log signal sources in a local session into a dataset
//! and/or a CSV file for a duration or until Ctrl-C.
use super::{SampleSink, TeeSink, csv::CsvSink, parse};
use crate::{session::SessionHandle, storage::StoragePolicy};
use anyhow::{Result, bail, ensure};
use serde_json::{Value as Json, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub struct LogOptions {
    pub uris: Vec<String>,
    pub interval_ms: u64,
    pub duration_ms: Option<u64>,
    pub csv: Option<PathBuf>,
    pub dataset: Option<PathBuf>,
    pub tags: Vec<String>,
    pub policy: StoragePolicy,
    /// Print progress to stderr every 10 s.
    pub progress: bool,
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(extension))
}

/// Check the arguments before anything starts (usage errors, exit 2). An
/// existing `--csv` file is refused by `run` instead, as an operation error.
pub fn validate(options: &LogOptions) -> Result<()> {
    ensure!(
        (1..=super::MAX_SOURCES).contains(&options.uris.len()),
        "log takes 1..{} source URIs",
        super::MAX_SOURCES
    );
    for uri in &options.uris {
        parse(uri)?;
    }
    super::validate_interval(options.interval_ms)?;
    super::validate_tags(&options.tags)?;
    if let Some(duration) = options.duration_ms {
        ensure!(duration > 0, "log --duration must be positive");
    }
    ensure!(
        options.csv.is_some() || options.dataset.is_some(),
        "log needs --dataset FILE.duckdb or --csv FILE.csv"
    );
    if let Some(csv) = &options.csv {
        ensure!(has_extension(csv, "csv"), "--csv file must end in .csv");
    }
    if let Some(dataset) = &options.dataset {
        ensure!(
            has_extension(dataset, "duckdb"),
            "--dataset file must end in .duckdb"
        );
    }
    Ok(())
}

/// Run until the duration passes or `stop` is set, then stop the sources,
/// flush and detach the sinks, and return the result.
pub fn run(handle: &SessionHandle, options: &LogOptions, stop: &AtomicBool) -> Result<Json> {
    // An existing output is refused like capture and export outputs are
    // (operation_failed, exit 1), before anything starts.
    if let Some(csv) = &options.csv {
        ensure!(
            std::fs::symlink_metadata(csv).is_err(),
            "{} already exists; choose a new file name",
            csv.display()
        );
    }
    let hub = handle.signals();
    let started = Instant::now();
    let mut sinks: Vec<Arc<dyn SampleSink>> = Vec::new();
    let dataset = match &options.dataset {
        Some(path) => {
            let path = std::path::absolute(path)?;
            handle
                .dataset()
                .open(&path, crate::dataset::OpenMode::Auto, None)?;
            sinks.push(handle.dataset().clone());
            Some(path)
        }
        None => None,
    };
    let mut csv_sink = None;
    let csv = match &options.csv {
        Some(path) => {
            let path = std::path::absolute(path)?;
            let sink = CsvSink::create(
                &path,
                &options.policy,
                crate::media::default_recording_cap(),
            );
            match sink {
                Ok(sink) => {
                    let sink = Arc::new(sink);
                    csv_sink = Some(sink.clone());
                    sinks.push(sink);
                }
                Err(error) => {
                    close_dataset(handle, dataset.is_some());
                    return Err(error);
                }
            }
            Some(path)
        }
        None => None,
    };
    let sink: Arc<dyn SampleSink> = if sinks.len() == 1 {
        sinks.remove(0)
    } else {
        Arc::new(TeeSink(sinks))
    };
    let finish = |result: Result<Json>| -> Result<Json> {
        let stopped = hub.stop("all", Duration::from_secs(5));
        let result = result.and_then(|value| {
            outcome(hub.sink_error(), stopped.as_deref().unwrap_or(&[]))?;
            Ok(value)
        });
        if result.is_err()
            && let Some(csv) = &csv_sink
        {
            csv.discard_if_empty();
        }
        let flushed = hub
            .flush(Duration::from_secs(10))
            .and_then(|()| hub.set_sink(None, Duration::from_secs(10)));
        close_dataset(handle, dataset.is_some());
        let sources = stopped?;
        flushed?;
        result.map(|mut value| {
            let samples: u64 = sources.iter().map(|s| s.samples).sum();
            let written: u64 = sources.iter().map(|s| s.written).sum();
            let dropped: u64 = sources.iter().map(|s| s.dropped + s.helper_dropped).sum();
            value["sources"] = json!(sources);
            value["samples"] = json!(samples);
            value["written"] = json!(written);
            value["dropped"] = json!(dropped);
            if samples == 0 {
                let warning = if options.tags.is_empty() {
                    "no samples were logged; check that the sources publish values"
                } else {
                    "no samples were logged; check the --tag names against signals browse"
                };
                eprintln!("capturefab: {warning}");
                value["warnings"] = json!([warning]);
            }
            value
        })
    };
    hub.set_sink(Some(sink), Duration::from_secs(10))?;
    for uri in &options.uris {
        if let Err(error) = hub.start(
            uri,
            options.interval_ms,
            options.tags.clone(),
            super::default_timeout_ms(),
        ) {
            return finish(Err(error));
        }
    }
    let deadline = options
        .duration_ms
        .map(|ms| started + Duration::from_millis(ms));
    let mut report = Instant::now();
    while !stop.load(Ordering::Relaxed) && deadline.is_none_or(|d| Instant::now() < d) {
        std::thread::sleep(Duration::from_millis(100));
        if hub.sink_error().is_some() {
            break;
        }
        if options.progress && report.elapsed() >= Duration::from_secs(10) {
            report = Instant::now();
            let status = hub.status(false);
            eprintln!(
                "Logging {} source(s): {} samples, {} written, {} dropped",
                status.len(),
                status.iter().map(|s| s.samples).sum::<u64>(),
                status.iter().map(|s| s.written).sum::<u64>(),
                status
                    .iter()
                    .map(|s| s.dropped + s.helper_dropped)
                    .sum::<u64>(),
            );
        }
    }
    let duration_ms = started.elapsed().as_millis() as u64;
    finish(Ok(json!({
        "dataset": dataset,
        "csv": csv,
        "duration_ms": duration_ms,
    })))
}

/// Fail a run whose sink failed or one of whose sources logged nothing.
fn outcome(sink_error: Option<String>, sources: &[super::SourceStatus]) -> Result<()> {
    if let Some(error) = sink_error {
        bail!("{error}");
    }
    if let Some(source) = sources.iter().find(|s| s.samples == 0 && s.error.is_some()) {
        bail!(
            "signal source {} logged no samples: {}",
            source.uri,
            source.error.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

fn close_dataset(handle: &SessionHandle, opened: bool) {
    if opened && let Err(error) = handle.dataset().close() {
        eprintln!("capturefab: dataset close: {error:#}");
    }
}
