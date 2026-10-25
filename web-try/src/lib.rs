//! Capturefab in the browser: the native app's frame measurements (exposure,
//! auto-exposure meter, focus), pixel conversions, still encoders and the
//! Pattern camera (`sim:0`), compiled to `wasm32-unknown-unknown` for
//! `web/try/`.
//!
//! Every function here is a port of the native crate's code, named after it
//! and kept bit-for-bit identical on identical input bytes; the module docs
//! cite the native source. `tests/golden.rs` checks the port against
//! `tests/golden.json`, which `tests/browser_golden.rs` in the main crate
//! computes from the real native code.
//!
//! The crate uses `std` (rounding, `sqrt`, `sin`, `hypot`, formatting) but
//! never clocks, threads, files or the network: none exist on this target.
//! Time arrives as arguments from JavaScript. The raw ABI is in [`abi`].

/// Return early with a formatted error, like anyhow's `ensure!`.
macro_rules! ensure {
    ($condition:expr, $($message:tt)+) => {{
        let holds: bool = $condition;
        if !holds {
            return Err(format!($($message)+));
        }
    }};
}

/// Return a formatted error, like anyhow's `bail!`.
macro_rules! bail {
    ($($message:tt)+) => {
        return Err(format!($($message)+))
    };
}

pub mod abi;
pub mod auto;
pub mod crc32;
pub mod frame;
pub mod genicam;
pub mod inspect;
pub mod jpeg;
pub mod json;
pub mod png;
pub mod simulator;
pub mod types;
pub mod yuv;

/// Errors carry the native app's message text, already formatted as
/// `{error:#}` would print it.
pub type Result<T> = std::result::Result<T, String>;
