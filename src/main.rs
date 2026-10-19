use capturefab::cli::{self, Cli};
// Diagnostics that a hung-up terminal cannot turn into a panic.
use capturefab::eprintln;
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
            .ok_or_else(|| anyhow::anyhow!("missing shared ring"))
            .and_then(|ring| capturefab::session::run_worker(&ring));
        if let Err(e) = result {
            eprintln!(
                "capturefab worker: {}",
                capturefab::hubs::scrub::scrub(&format!("{e:#}"))
            );
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
        let check_failure = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<capturefab::tools::CheckFailure>());
        let plan_failure = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<capturefab::tools::PlanFailure>());
        let confirmation = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<cli::ToolsConfirmationRequired>());
        let assistant_failure = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<cli::AssistantTaskFailure>());
        #[cfg(feature = "ai")]
        let provider_failure = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<capturefab::assistant::http::ProviderError>());
        // Classify typed failures before removing any registered hub secret.
        let message = check_failure
            .map(ToString::to_string)
            .or_else(|| plan_failure.map(ToString::to_string))
            .or_else(|| confirmation.map(ToString::to_string))
            .or_else(|| assistant_failure.map(ToString::to_string))
            .or_else(|| workflow_failure.map(ToString::to_string))
            .unwrap_or_else(|| format!("{error:#}"));
        #[cfg(feature = "ai")]
        let message = provider_failure.map(ToString::to_string).unwrap_or(message);
        let message = capturefab::hubs::scrub::scrub(&message).into_owned();
        if json_mode {
            let mut envelope =
                json!({"version":1,"ok":false,"error":{"code":code,"message":message}});
            if let Some(failure) = assistant_failure {
                envelope["error"]["details"] = failure.details();
            } else if let Some(failure) = check_failure {
                envelope["error"]["details"] = failure.details();
            } else if let Some(failure) = plan_failure {
                envelope["error"]["details"] = failure.details();
            } else if let Some(confirmation) = confirmation {
                envelope["error"]["details"] = confirmation.details();
            } else if let Some(failure) = workflow_failure {
                envelope["error"]["details"] = failure.result();
            }
            #[cfg(feature = "ai")]
            if let Some(failure) = provider_failure {
                envelope["error"]["message"] = json!(failure.to_string());
                let mut details = assistant_failure
                    .map(cli::AssistantTaskFailure::details)
                    .unwrap_or_else(|| json!({}));
                if let (Some(report), Some(provider)) =
                    (details.as_object_mut(), failure.details().as_object())
                {
                    report.extend(
                        provider
                            .iter()
                            .map(|(key, value)| (key.clone(), value.clone())),
                    );
                }
                envelope["error"]["details"] = details;
            }
            let _ = cli::write_json(&envelope);
        } else if let Some(failure) = assistant_failure {
            let _ = failure.print_human();
            eprintln!("capturefab: {message}");
        } else if let Some(failure) = check_failure {
            for (step, issues) in &failure.issues_by_step {
                for issue in issues {
                    let field = issue
                        .field
                        .as_deref()
                        .map(|field| format!(" ({field})"))
                        .unwrap_or_default();
                    eprintln!("capturefab: step {step}{field}: {}", issue.message);
                }
            }
        } else {
            eprintln!("capturefab: {message}");
        }
        std::process::exit(exit)
    }
}
