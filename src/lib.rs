/// Diagnostics on stderr that never panic. std's `eprintln!` panics when
/// writing fails, as it does with EIO once the terminal hangs up, which is
/// SIGHUP's usual cause: shutdown would then stop halfway. This one drops the
/// line instead, and takes a status line off the terminal first. Control
/// characters other than line breaks and tabs do not reach the terminal: a
/// message can quote what a camera or the network sent. Defined before the
/// modules, it stands in for std's in all of them.
#[macro_export]
macro_rules! eprintln {
    () => {{
        use ::std::io::Write as _;
        $crate::cli::term::before_output();
        let _ = ::std::writeln!(::std::io::stderr());
    }};
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        $crate::cli::term::before_output();
        let line = ::std::format!($($arg)*);
        let _ = ::std::writeln!(
            ::std::io::stderr(),
            "{}",
            $crate::types::printable_text(&line)
        );
    }};
}

pub(crate) mod auto;
pub(crate) mod camera;
pub(crate) mod camera_profile;
pub(crate) mod cli;
pub(crate) mod context;
pub(crate) mod controls;
pub(crate) mod copies;
pub(crate) mod dirs;
pub(crate) mod discovery;
pub(crate) mod doctor;
pub(crate) mod engine;
pub(crate) mod frame;
pub(crate) mod framebuf;
pub(crate) mod genicam;
pub(crate) mod hint;
pub(crate) mod host;
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
pub(crate) mod xml_cache;

/// Every frame-sized allocation is counted in the executable, the tests
/// and the benchmarks of a `copy-audit` build.
#[cfg(feature = "copy-audit")]
#[global_allocator]
static ALLOCATOR: copies::Counting = copies::Counting;

/// The `capturefab` executable: the hidden camera worker and GPU JPEG
/// helper modes, or the CLI.
pub fn run() {
    let mode = std::env::args_os().nth(1);
    if mode.as_deref() == Some(std::ffi::OsStr::new("__worker")) {
        let result = session::run_worker(std::env::args_os().skip(2));
        if let Err(e) = result {
            eprintln!("capturefab worker: {}", scrub::scrub(&format!("{e:#}")));
            std::process::exit(1)
        }
        return;
    }
    #[cfg(jpeg_helper)]
    if mode.as_deref() == Some(std::ffi::OsStr::new(jpeg::helper::ARG)) {
        if let Err(e) = jpeg::helper::run(std::env::args_os().skip(2)) {
            eprintln!("capturefab JPEG helper: {e:#}");
            std::process::exit(1)
        }
        return;
    }
    std::process::exit(cli::main())
}

/// What the integration tests and benchmarks use, at the same paths as in
/// the crate. Not a public API: only the `testing` feature, which tests and
/// benchmarks turn on, builds it, with the reference conversions and
/// one-call wrappers that nothing else calls.
#[cfg(feature = "testing")]
#[doc(hidden)]
pub mod testing {
    pub mod auto {
        pub use crate::auto::meter;
    }
    pub mod camera {
        pub use crate::camera::Camera;
    }
    pub mod camera_profile {
        pub use crate::camera_profile::CameraKind;
    }
    pub mod frame {
        pub use crate::frame::{
            MAX_META, bit_depth, clipping, convert_into, convertible, encode, encode_parts,
            encode_with_meta, exposure, gray8, pixel_format_name, rgb, rgba, row_bytes,
            sample_bits, sharpness, unpacked,
        };
    }
    pub mod inspect {
        pub use crate::inspect::{PreviewFormat, inspect, measure};
    }
    pub mod jpeg {
        pub use crate::jpeg::{Raster, insert_com, software};
    }
    pub mod session {
        pub use crate::session::{SessionCommand, SessionHandle};
    }
    pub mod storage {
        pub use crate::storage::{SyncMode, save};
    }
    pub mod types {
        pub use crate::types::{Frame, MONO8, RGB8};
    }
    pub mod framebuf {
        pub use crate::framebuf::pool;
    }
}
