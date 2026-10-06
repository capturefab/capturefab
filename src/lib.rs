#[cfg(any(feature = "nvjpeg", feature = "vaapi"))]
pub mod accel;
pub mod auto;
pub mod camera;
pub mod cli;
pub mod compatibility;
pub mod destination;
mod engine;
pub mod frame;
pub mod genicam;
#[cfg(feature = "gui")]
pub mod gui;
pub mod ipc;
pub mod jpeg;
pub mod media;
#[cfg(feature = "nvjpeg")]
pub mod nvjpeg;
pub mod onvif;
#[cfg(feature = "s3")]
pub mod s3;
pub mod scheduling;
pub mod session;
pub mod shared_memory;
pub mod storage;
pub mod transport;
pub mod types;
#[cfg(feature = "s3")]
pub mod upload;
#[cfg(feature = "vaapi")]
pub mod vajpeg;
pub mod volumes;
#[cfg(all(feature = "videotoolbox", target_os = "macos"))]
pub mod vtjpeg;
