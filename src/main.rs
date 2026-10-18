use capturefab::cli::{self, Cli};
use clap::Parser;
use serde_json::json;

fn main() {
    if std::env::args().nth(1).as_deref() == Some("__signal") {
        if let Err(e) = capturefab::signals::helper::run_helper() {
            eprintln!("capturefab signal helper: {e:#}");
            std::process::exit(1)
        }
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("__worker") {
        let result = std::env::args_os()
            .nth(2)
            .ok_or_else(|| anyhow::anyhow!("missing shared ring path"))
            .and_then(|p| capturefab::session::run_worker(std::path::Path::new(&p)));
        if let Err(e) = result {
            eprintln!("capturefab worker: {e:#}");
            std::process::exit(1)
        }
        return;
    }
    #[cfg(feature = "gui")]
    if std::env::args().nth(1).as_deref() == Some("__gpu-probe") {
        if let Err(e) = capturefab::doctor::run_gpu_probe() {
            eprintln!("capturefab GPU probe: {e:#}");
            std::process::exit(1)
        }
        return;
    }
    #[cfg(feature = "nvjpeg")]
    if std::env::args().nth(1).as_deref() == Some("__nvjpeg") {
        if let Err(e) = capturefab::nvjpeg::run_helper() {
            eprintln!("capturefab nvjpeg helper: {e:#}");
            std::process::exit(1)
        }
        return;
    }
    #[cfg(feature = "dinov2")]
    if std::env::args().nth(1).as_deref() == Some(capturefab::autocapture::dinov2::helper::ARGUMENT)
    {
        if let Err(e) = capturefab::autocapture::dinov2::helper::run_helper() {
            eprintln!("capturefab DINOv2 GPU helper: {e:#}");
            std::process::exit(1)
        }
        return;
    }
    #[cfg(feature = "vaapi")]
    if std::env::args().nth(1).as_deref() == Some("__vajpeg") {
        if let Err(e) = capturefab::vajpeg::run_helper() {
            eprintln!("capturefab VA-API JPEG helper: {e:#}");
            std::process::exit(1)
        }
        return;
    }
    let json_mode = std::env::args().any(|a| a == "--json");
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            if matches!(
                e.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                let _ = e.print();
                return;
            }
            if json_mode {
                let _ = cli::write_json(
                    &json!({"version":1,"ok":false,"error":{"code":"usage","message":e.to_string()}}),
                );
            } else {
                let _ = e.print();
            }
            std::process::exit(2)
        }
    };
    if let Err(error) = cli::run(cli) {
        if error.chain().any(|e| {
            e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe)
        }) {
            return;
        }
        let (code, exit) = cli::error_code(&error);
        let workflow_failure = error.downcast_ref::<capturefab::workflow::WorkflowFailure>();
        let message = workflow_failure
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("{error:#}"));
        if json_mode {
            let mut envelope =
                json!({"version":1,"ok":false,"error":{"code":code,"message":message}});
            if let Some(failure) = workflow_failure {
                envelope["error"]["details"] = failure.result();
            }
            let _ = cli::write_json(&envelope);
        } else {
            eprintln!("capturefab: {message}");
        }
        std::process::exit(exit)
    }
}
