/// Diagnostics on stderr that never panic. std's `eprintln!` panics when
/// writing fails, as it does with EIO once the terminal hangs up, which is
/// SIGHUP's usual cause: shutdown would then stop halfway. This one drops the
/// line instead. Defined before the modules, it stands in for std's in all of
/// them; `main.rs` imports it.
#[macro_export]
macro_rules! eprintln {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let _ = ::std::writeln!(::std::io::stderr(), $($arg)*);
    }};
}

#[cfg(any(feature = "nvjpeg", feature = "vaapi"))]
pub mod accel;
#[cfg(feature = "ai")]
pub mod ai;
pub mod analytics;
#[cfg(feature = "ai")]
pub mod assistant;
pub mod auto;
#[cfg(feature = "ai")]
pub mod aws;
pub mod camera;
pub mod camera_profile;
pub mod cli;
pub mod compatibility;
pub mod context;
pub mod dataset;
pub mod destination;
pub mod doctor;
pub mod efficiency;
mod engine;
pub mod frame;
pub mod genicam;
#[cfg(feature = "gui")]
pub mod gui;
pub mod hint;
pub mod hubs;
pub mod inspect;
pub mod ipc;
pub mod jpeg;
#[cfg(feature = "keychain")]
pub mod keychain;
pub mod media;
pub mod naming;
pub mod network;
#[cfg(feature = "nvjpeg")]
pub mod nvjpeg;
pub mod onvif;
pub mod remote;
pub mod resources;
pub mod scanner;
pub mod scheduling;
pub mod secret_input;
pub mod session;
pub mod shared_memory;
pub mod simulation;
pub mod storage;
pub mod tools;
pub mod transport;
pub mod types;
#[cfg(feature = "vaapi")]
pub mod vajpeg;
#[cfg(feature = "ai")]
pub mod video_analytics;
pub mod volumes;
#[cfg(all(feature = "videotoolbox", target_os = "macos"))]
pub mod vtjpeg;
pub mod workflow;
