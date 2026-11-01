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
pub(crate) mod controls;
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
pub(crate) mod storage;
#[cfg(test)]
mod test_cameras;
pub(crate) mod transport;
pub(crate) mod types;
pub(crate) mod volumes;

/// The `capturefab` executable: the hidden camera worker mode, or the CLI.
pub fn run() {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("__worker")) {
        let result = session::run_worker(std::env::args_os().skip(2));
        if let Err(e) = result {
            eprintln!("capturefab worker: {}", scrub::scrub(&format!("{e:#}")));
            std::process::exit(1)
        }
        return;
    }
    std::process::exit(cli::main())
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
            MAX_META, encode, encode_with_meta, exposure, gray8, pixel_format_name, rgb, rgba,
            sharpness,
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
    pub mod storage {
        pub use crate::storage::{StoragePolicy, configure_policy, save_bytes};
    }
    pub mod types {
        pub use crate::types::{Frame, MONO8, RGB8};
    }
}
