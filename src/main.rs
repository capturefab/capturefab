use capturefab::cli::{self, Cli};
// Diagnostics that a hung-up terminal cannot turn into a panic.
use capturefab::eprintln;
use clap::Parser;
use serde_json::json;

fn main() {
    if std::env::args().nth(1).as_deref() == Some("__worker") {
        let result = std::env::args_os()
            .nth(2)
            .ok_or_else(|| anyhow::anyhow!("missing shared ring"))
            .and_then(|ring| capturefab::session::run_worker(&ring));
        if let Err(e) = result {
            eprintln!(
                "capturefab worker: {}",
                capturefab::scrub::scrub(&format!("{e:#}"))
            );
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
                let _ = cli::write_json(&cli::clap_error_envelope(&e));
            } else {
                let _ = e.print();
            }
            std::process::exit(2)
        }
    };
    let typed_output = cli.typed_output();
    if let Err(error) = cli::run(cli) {
        if error.chain().any(|e| {
            e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe)
        }) {
            return;
        }
        let (code, exit) = cli::error_code(&error);
        let message = capturefab::scrub::scrub(&format!("{error:#}")).into_owned();
        let hint = typed_output
            .as_deref()
            .and_then(|output| cli::exists_hint(&error, output));
        if json_mode {
            let mut envelope =
                json!({"version":1,"ok":false,"error":{"code":code,"message":message}});
            if let Some(hint) = hint {
                envelope["error"]["details"] = json!({"hint":hint});
            }
            let _ = cli::write_json(&envelope);
        } else {
            eprintln!("capturefab: {message}");
            if let Some(hint) = hint {
                eprintln!("capturefab: to keep both, {hint}");
            }
        }
        std::process::exit(exit)
    }
}
