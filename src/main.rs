use capturefab::cli::{self, Cli};
use clap::Parser;
use serde_json::json;

fn main() {
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
        if json_mode {
            let _ = cli::write_json(
                &json!({"version":1,"ok":false,"error":{"code":code,"message":format!("{error:#}")}}),
            );
        } else {
            eprintln!("capturefab: {error:#}");
        }
        std::process::exit(exit)
    }
}
