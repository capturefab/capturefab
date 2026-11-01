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

pub mod auto;
pub mod camera;
pub mod camera_profile;
pub mod cli;
pub mod context;
pub mod doctor;
mod engine;
pub mod frame;
pub mod genicam;
pub mod hint;
pub mod inspect;
pub mod ipc;
pub mod jpeg;
#[cfg(feature = "keychain")]
pub mod keychain;
pub mod logins;
pub mod media;
pub mod naming;
pub mod network;
pub mod onvif;
pub mod scheduling;
pub mod scrub;
pub mod secret_input;
pub mod session;
pub mod setup;
pub mod shared_memory;
pub mod storage;
#[cfg(test)]
mod test_cameras;
pub mod transport;
pub mod types;
pub mod volumes;
