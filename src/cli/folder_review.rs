//! `folder-review`: rank a folder's images and video frames and review the
//! interesting ones. It runs in this process; no camera or session is used.
use super::{Cli, InvalidInput, on_quit_signal, parse_sensitivity};
use crate::folder_review::{self, Options, Rank, RankChoice, ReviewSession};
use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use serde_json::{Value, json};
use std::{
    io::{IsTerminal, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Debug, Subcommand)]
pub enum FolderReviewCommand {
    /// Create a DuckDB review session for a folder; the folder is only ever read
    New {
        /// Session file to create, ending in .duckdb; existing files are never overwritten
        path: PathBuf,
        /// Folder of images and videos, searched recursively
        #[arg(long)]
        folder: PathBuf,
        /// Image encoder: auto uses the bundled DINOv2 model, on the GPU when possible
        #[arg(long, value_enum)]
        encoder: Option<crate::autocapture::EncoderChoice>,
        /// Ranking: auto is similar with --examples, otherwise novelty
        #[arg(long, value_enum, default_value_t = RankChoice::Auto)]
        rank: RankChoice,
        /// Capture session whose kept and discarded captures teach the ranking
        #[arg(long)]
        examples: Option<PathBuf>,
        /// How many items count as interesting, from 0 (selective) to 1 (more)
        #[arg(long, default_value_t = 0.5, value_parser = parse_sensitivity)]
        sensitivity: f64,
        /// Sample videos every this many milliseconds
        #[arg(long, default_value_t = folder_review::DEFAULT_VIDEO_INTERVAL_MS, value_parser = clap::value_parser!(u64).range(100..=3_600_000))]
        video_interval_ms: u64,
    },
    /// Sample new and changed files and drop pending items of removed ones; Ctrl-C stops and keeps finished files
    Scan { path: PathBuf },
    /// Show counts, ranking settings and how many pending items are interesting
    Status { path: PathBuf },
    /// Offer the most interesting pending item
    Next {
        path: PathBuf,
        /// Offer the best pending item even when it is below the threshold
        #[arg(long)]
        all: bool,
        /// Include a bounded RGBA preview (at most 480x360) as base64
        #[arg(long)]
        inline_image: bool,
    },
    /// Keep a pending item and rank the rest with it
    Keep {
        path: PathBuf,
        #[arg(value_parser = clap::value_parser!(u64).range(1..=i64::MAX as u64))]
        id: u64,
    },
    /// Discard a pending item and suppress look-alikes
    Discard {
        path: PathBuf,
        #[arg(value_parser = clap::value_parser!(u64).range(1..=i64::MAX as u64))]
        id: u64,
    },
    /// Change how pending items are ranked
    Configure {
        path: PathBuf,
        #[arg(long, value_enum)]
        rank: Option<ConfigureRank>,
        #[arg(long, value_parser = parse_sensitivity)]
        sensitivity: Option<f64>,
    },
    /// Copy kept items not yet exported: stills unchanged, video frames as PNG; nothing is overwritten
    Export {
        path: PathBuf,
        /// Folder to copy into, or a path inside --destination
        #[arg(long, required_unless_present = "destination")]
        to: Option<String>,
        /// Saved destination; bucket destinations queue uploads
        #[arg(long)]
        destination: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ConfigureRank {
    Novelty,
    Similar,
}

pub(super) fn run(cli: &Cli, command: &FolderReviewCommand) -> Result<Value> {
    ensure!(
        cli.session.is_none(),
        InvalidInput("folder-review runs in this process; omit --session".into())
    );
    Ok(match command {
        FolderReviewCommand::New {
            path,
            folder,
            encoder,
            rank,
            examples,
            sensitivity,
            video_interval_ms,
        } => {
            let options = Options {
                folder: folder.clone(),
                encoder: *encoder,
                rank: *rank,
                examples: examples.clone(),
                sensitivity: *sensitivity,
                video_interval_ms: *video_interval_ms,
            };
            let session = ReviewSession::create(path, &options)?;
            let status = session.status()?;
            session.close()?;
            json!({"folder_review": status})
        }
        FolderReviewCommand::Scan { path } => {
            let mut session = ReviewSession::open(path)?;
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            on_quit_signal(move || flag.store(true, Ordering::Relaxed))?;
            let terminal = std::io::stderr().is_terminal();
            let summary = session.scan(&stop, &mut |progress| {
                if terminal {
                    let mut err = std::io::stderr().lock();
                    let _ = write!(
                        err,
                        "\r\x1b[2KScanned {}/{} files, {} items: {}",
                        progress.done, progress.total, progress.items, progress.path
                    );
                    let _ = err.flush();
                }
            })?;
            if terminal {
                eprintln!();
            }
            let status = session.status()?;
            session.close()?;
            json!({"scan": summary, "folder_review": status})
        }
        FolderReviewCommand::Status { path } => {
            let session = ReviewSession::open(path)?;
            json!({"folder_review": session.status()?})
        }
        FolderReviewCommand::Next {
            path,
            all,
            inline_image,
        } => {
            let mut session = ReviewSession::open(path)?;
            let item = session.next(*all)?;
            let mut value = serde_json::to_value(&item)?;
            if let (Some(item), true) = (&item, *inline_image) {
                use base64::Engine;
                let frame = session.frame(item.id)?;
                let (width, height, rgba) = crate::frame::preview_rgba(&frame, 480, 360)?;
                value["preview"] = json!({"width": width, "height": height,
                    "rgba": base64::engine::general_purpose::STANDARD.encode(rgba)});
            }
            let status = session.status()?;
            session.close()?;
            json!({"item": value, "folder_review": status})
        }
        FolderReviewCommand::Keep { path, id } | FolderReviewCommand::Discard { path, id } => {
            let keep = matches!(command, FolderReviewCommand::Keep { .. });
            let mut session = ReviewSession::open(path)?;
            session.review(*id, keep)?;
            let status = session.status()?;
            session.close()?;
            json!({"id": id, "keep": keep, "folder_review": status})
        }
        FolderReviewCommand::Configure {
            path,
            rank,
            sensitivity,
        } => {
            let mut session = ReviewSession::open(path)?;
            let rank = rank.map(|rank| match rank {
                ConfigureRank::Novelty => Rank::Novelty,
                ConfigureRank::Similar => Rank::Similar,
            });
            session.configure(rank, *sensitivity)?;
            let status = session.status()?;
            session.close()?;
            json!({"folder_review": status})
        }
        FolderReviewCommand::Export {
            path,
            to,
            destination,
        } => {
            let output = to.as_deref().unwrap_or("folder-review");
            let target = crate::destination::resolve(destination.as_deref(), output)
                .map_err(|error| InvalidInput(format!("{error:#}")))?;
            let mut session = ReviewSession::open(path)?;
            let summary = session
                .export(&target)
                .context("export kept folder review items")?;
            let status = session.status()?;
            session.close()?;
            json!({"export": summary, "folder_review": status})
        }
    })
}

pub(super) fn print_human(command: &FolderReviewCommand, value: &Value) -> Result<()> {
    let mut out = std::io::stdout().lock();
    let status = &value["folder_review"];
    match command {
        FolderReviewCommand::New { path, .. } => {
            writeln!(
                out,
                "Created {}. Next: capturefab folder-review scan {}",
                text(&status["path"]),
                path.display()
            )?;
        }
        FolderReviewCommand::Scan { .. } => {
            let scan = &value["scan"];
            writeln!(
                out,
                "{} files: {} new, {} changed, {} unchanged, {} removed, {} failed; {} items added ({} repeats){}{}",
                scan["files"],
                scan["added"],
                scan["changed"],
                scan["unchanged"],
                scan["missing"],
                scan["failed"],
                scan["items"],
                scan["duplicates"],
                if scan["limited"] == true {
                    "; item limit reached"
                } else {
                    ""
                },
                if scan["stopped"] == true {
                    "; stopped early"
                } else {
                    ""
                },
            )?;
            for error in scan["errors"].as_array().into_iter().flatten() {
                writeln!(out, "  {}: {}", text(&error["path"]), text(&error["error"]))?;
            }
            if let Some(accelerator) = scan["accelerator"].as_str() {
                writeln!(out, "Embeddings ran on {accelerator}")?;
            }
        }
        FolderReviewCommand::Next { path, .. } => match value["item"].as_object() {
            Some(item) => {
                write!(out, "Item {}: {}", item["id"], text(&item["path"]))?;
                if let Some(offset) = item.get("offset_ms").and_then(Value::as_u64) {
                    write!(out, " at {:.1} s", offset as f64 / 1000.0)?;
                }
                writeln!(
                    out,
                    "\n  score {:.2}: {}",
                    item["score"].as_f64().unwrap_or(0.0),
                    text(&item["reason"])
                )?;
                writeln!(
                    out,
                    "  capturefab folder-review keep {0} {1}  |  discard {0} {1}",
                    path.display(),
                    item["id"]
                )?;
            }
            None => writeln!(
                out,
                "Caught up: no pending item reaches the threshold. Use --all for the rest, or scan again."
            )?,
        },
        FolderReviewCommand::Export { .. } => {
            let export = &value["export"];
            for item in export["exported"].as_array().into_iter().flatten() {
                writeln!(out, "{} -> {}", text(&item["source"]), text(&item["path"]))?;
            }
            for item in export["skipped"].as_array().into_iter().flatten() {
                writeln!(
                    out,
                    "Skipped {}: {}",
                    text(&item["path"]),
                    text(&item["error"])
                )?;
            }
            writeln!(
                out,
                "Exported {} kept items to {}",
                export["exported"].as_array().map_or(0, Vec::len),
                text(&export["output"])
            )?;
        }
        _ => {}
    }
    if status.is_object() {
        writeln!(
            out,
            "{}: {} pending ({} interesting), {} kept, {} discarded, {} repeats; ranked by {} with the {} encoder",
            text(&status["folder"]),
            status["pending"],
            status["interesting"],
            status["kept"],
            status["discarded"],
            status["duplicates"],
            text(&status["rank"]),
            text(&status["encoder"]),
        )?;
        if let Some(note) = status["note"].as_str() {
            writeln!(out, "Note: {note}")?;
        }
    }
    Ok(())
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}
