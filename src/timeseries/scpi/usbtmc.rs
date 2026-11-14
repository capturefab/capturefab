//! Native USBTMC worker boundary; unsupported builds retain pure configuration.
mod config;
pub(crate) use config::Config;
#[cfg(feature = "usb")]
mod client;
#[cfg(feature = "usb")]
pub(crate) use client::Client;
#[cfg(feature = "usb")]
mod codec;
#[cfg(feature = "usb")]
mod native;
#[cfg(feature = "usb")]
pub(crate) mod wire;
#[cfg(feature = "usb")]
mod worker;
#[cfg(feature = "usb")]
pub(crate) fn run_worker() -> anyhow::Result<()> {
    worker::run_native()
}

#[cfg(all(feature = "usb", feature = "testing"))]
mod fixture;
#[cfg(all(feature = "usb", feature = "testing"))]
pub(crate) fn run_fixture() -> anyhow::Result<()> {
    fixture::run()
}
#[cfg(all(feature = "usb", feature = "testing"))]
pub(crate) fn run_probe(input: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    client::run_probe(input)
}
