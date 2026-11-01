// Removed in S8 after the dead-code sweep.
#![allow(dead_code)]

/// Diagnostics on stderr that never panic. std's `eprintln!` panics when
/// writing fails, as it does with EIO once the terminal hangs up, which is
/// SIGHUP's usual cause: shutdown would then stop halfway. This one drops the
/// line instead. Defined before the modules, it stands in for std's in all of
/// them.
#[macro_export]
macro_rules! eprintln {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let _ = ::std::writeln!(::std::io::stderr(), $($arg)*);
    }};
}

pub(crate) mod auto;
pub(crate) mod camera;
pub(crate) mod camera_profile;
pub(crate) mod cli;
pub(crate) mod context;
pub(crate) mod doctor;
pub(crate) mod engine;
pub(crate) mod frame;
pub(crate) mod genicam;
pub(crate) mod hint;
pub(crate) mod inspect;
pub(crate) mod ipc;
pub(crate) mod jpeg;
#[cfg(feature = "keychain")]
pub(crate) mod keychain;
pub(crate) mod logins;
pub(crate) mod media;
pub(crate) mod naming;
pub(crate) mod network;
pub(crate) mod onvif;
pub(crate) mod scheduling;
pub(crate) mod scrub;
pub(crate) mod secret_input;
pub(crate) mod session;
pub(crate) mod setup;
pub(crate) mod shared_memory;
pub(crate) mod storage;
#[cfg(test)]
mod test_cameras;
pub(crate) mod transport;
pub(crate) mod types;
pub(crate) mod volumes;

use serde_json::json;

/// The `capturefab` executable: the hidden camera worker mode, or the CLI.
pub fn run() {
    if std::env::args().nth(1).as_deref() == Some("__worker") {
        let result = std::env::args_os()
            .nth(2)
            .ok_or_else(|| anyhow::anyhow!("missing shared ring"))
            .and_then(|ring| session::run_worker(&ring));
        if let Err(e) = result {
            eprintln!("capturefab worker: {}", scrub::scrub(&format!("{e:#}")));
            std::process::exit(1)
        }
        return;
    }
    let json_mode = std::env::args().any(|a| a == "--json");
    let cli = match <cli::Cli as clap::Parser>::try_parse() {
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
        let message = scrub::scrub(&format!("{error:#}")).into_owned();
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

/// What the integration tests and benchmarks use, at the same paths as in
/// the crate. Not a public API.
#[doc(hidden)]
pub mod testing {
    pub mod camera {
        pub use crate::camera::Camera;
    }
    pub mod camera_profile {
        pub use crate::camera_profile::CameraKind;
    }
    pub mod frame {
        pub use crate::frame::{
            MAX_META, convert, encode, encode_with_meta, exposure, gray8, pixel_format_name, rgb,
            rgba, sharpness,
        };
    }
    pub mod inspect {
        pub use crate::inspect::{PreviewFormat, inspect, measure};
    }
    pub mod jpeg {
        pub use crate::jpeg::{Raster, insert_com, software, well_formed};
    }
    pub mod session {
        pub use crate::session::{SessionCommand, SessionHandle};
    }
    pub mod shared_memory {
        pub use crate::shared_memory::SharedRing;
    }
    pub mod storage {
        pub use crate::storage::{StoragePolicy, configure_policy, save_bytes};
    }
    pub mod types {
        pub use crate::types::{Frame, MONO8, RGB8};
    }
}
