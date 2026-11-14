//! Opto 22 groov EPIC/RIO input polling over `OptoMMP` `ReadQuadlet`; read-only and finite.
//! Each poll is published whole or not at all; no retry, reconnect or device write.
use super::{Reading, Source, net};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    io,
    net::TcpStream,
    time::{Duration, Instant},
};

mod codec;
mod config;
mod model;
use codec::failure;
pub(crate) use config::Config;
use model::Addresses;

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    config.validate()?;
    if crate::host::monotonic_ns().is_none() {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            "OptoMMP requires a host-wide monotonic receive clock",
        ));
    }
    let addresses = config
        .channels
        .iter()
        .map(|channel| {
            model::addresses(
                config.model,
                channel.module,
                channel.channel,
                channel.resource,
            )
            .ok_or_else(|| Usage("OptoMMP channel has no documented address".into()).into())
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Box::new(Client {
        config: config.clone(),
        addresses,
        owner: None,
        label: 1,
        retired: false,
        polls: 0,
        observed: None,
        cutoff: None,
    }))
}

/// One complete ACK response.
#[derive(Debug, Clone, Copy)]
struct Word {
    word: u32,
    source_id: u16,
    observed: u64,
}

struct Client {
    config: Config,
    addresses: Vec<Addresses>,
    owner: Option<Box<dyn net::Transport>>,
    /// Label of the next request: 1..63, then 0.
    label: u8,
    retired: bool,
    polls: u64,
    observed: Option<u64>,
    cutoff: Option<u64>,
}

impl Client {
    fn connect(&mut self, deadline: Instant) -> Result<()> {
        let socket = TcpStream::connect_timeout(&self.config.address, remaining(deadline)?)
            .map_err(|error| io_error(&error))?;
        socket.set_nodelay(true).map_err(|error| io_error(&error))?;
        remaining(deadline)?;
        self.owner = Some(net::plain(socket, deadline));
        Ok(())
    }

    /// One request, one outstanding response, under the poll's original deadline.
    fn exchange(&mut self, address: u64, deadline: Instant) -> Result<Word> {
        remaining(deadline)?;
        let label = self.label;
        let request = codec::request(label, address)?;
        let owner = self.owner.as_deref_mut().ok_or_else(closed)?;
        owner.set_deadline(deadline);
        // The label is spent once bytes may have reached the device.
        self.label = (label + 1) % codec::LABELS;
        owner
            .write_all(&request)
            .map_err(|error| io_error(&error))?;
        let mut header = [0; codec::PREFIX];
        fill(owner, &mut header, deadline)?;
        let prefix = codec::prefix(&header, label).map_err(|error| {
            crate::types::with_details(
                error,
                json!({"raw_header_hex":crate::types::hex(header),
                    "requested_address":codec::address_hex(address)}),
            )
        })?;
        if prefix.response_code != 0 {
            return Err(codec::native_error(&header, prefix, label, address));
        }
        let mut word = [0; codec::WORD];
        fill(owner, &mut word, deadline)?;
        // Complete-response receive time precedes interpretation and storage.
        let observed = crate::host::monotonic_ns().ok_or_else(|| {
            Categorized::error(ErrorKind::Unsupported, "OptoMMP receive clock unavailable")
        })?;
        if self.cutoff.is_some_and(|cutoff| observed >= cutoff) {
            return Err(failure(
                ErrorKind::Timeout,
                "acquisition_cutoff",
                "response completed at or after the acquisition cutoff",
            ));
        }
        remaining(deadline)?;
        Ok(Word {
            word: u32::from_be_bytes(word),
            source_id: prefix.source_id,
            observed,
        })
    }

    fn expect(
        &mut self,
        address: u64,
        expected: u32,
        reason: &str,
        deadline: Instant,
    ) -> Result<()> {
        let observed = self.exchange(address, deadline)?;
        if observed.word == expected {
            return Ok(());
        }
        Err(Categorized::new(
            ErrorKind::OperationFailed,
            format!(
                "OptoMMP: {reason} at {}: expected {expected:#010x}, read {:#010x}",
                codec::address_hex(address),
                observed.word
            ),
        )
        .details(json!({
            "reason":reason,
            "requested_address":codec::address_hex(address),
            "expected_word":expected,
            "observed_word":observed.word,
            "source_id":observed.source_id,
        }))
        .into())
    }

    fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        if self.owner.is_none() {
            self.connect(deadline)?;
        }
        let ordinal = self.polls.checked_add(1).ok_or_else(|| {
            Categorized::error(ErrorKind::OutOfRange, "OptoMMP poll ordinal overflow")
        })?;
        self.polls = ordinal;
        // Status identity and Powerup Clear precede every nonstatus access.
        self.expect(
            model::STATUS_UNIT_TYPE,
            self.config.model.unit_type(),
            "unit_type_mismatch",
            deadline,
        )?;
        self.expect(model::STATUS_PUC, 0, "puc_not_cleared", deadline)?;
        let selections = self
            .config
            .channels
            .iter()
            .zip(self.addresses.iter().copied())
            .map(|(channel, addresses)| {
                (
                    channel.expected_module_id,
                    channel.expected_channel_type,
                    addresses,
                )
            })
            .collect::<Vec<_>>();
        // The expectations already passed the closed table; equality rechecks them now.
        for (module_id, channel_type, addresses) in &selections {
            self.expect(
                addresses.module_id,
                *module_id,
                "module_id_mismatch",
                deadline,
            )?;
            self.expect(
                addresses.channel_type,
                *channel_type,
                "channel_type_mismatch",
                deadline,
            )?;
        }
        let mut words = Vec::with_capacity(selections.len());
        for (_, _, addresses) in &selections {
            let value = self.exchange(addresses.value, deadline)?;
            let quality = self.exchange(addresses.quality, deadline)?;
            words.push((value, quality));
        }
        let frontier = words
            .last()
            .map(|(_, quality)| quality.observed)
            .ok_or_else(|| Usage("OptoMMP requires at least one channel".into()))?;
        let readings = self
            .config
            .channels
            .iter()
            .zip(&self.addresses)
            .zip(words)
            .map(|((channel, addresses), (value, quality))| {
                let mut record = json!({
                    "kind":"optommp_input",
                    "model":self.config.model.name(),
                    "module":channel.module,
                    "channel":channel.channel,
                    "resource":channel.resource.name(),
                    "module_id":channel.expected_module_id,
                    "channel_type":channel.expected_channel_type,
                    "requested_value_address":codec::address_hex(addresses.value),
                    "requested_quality_address":codec::address_hex(addresses.quality),
                    "raw_word":value.word,
                    "raw_hex":format!("{:08x}", value.word),
                    "quality_code":quality.word,
                    "quality_raw_hex":format!("{:08x}", quality.word),
                    "host_value_observed_ns":value.observed,
                    "host_quality_observed_ns":quality.observed,
                    "poll_observed_ns":frontier,
                    "poll_ordinal":ordinal,
                    "value_source_id":value.source_id,
                    "quality_source_id":quality.source_id,
                    "units_provenance":"externally_configured_unqueried",
                });
                if let (Some(record), Value::Object(native)) = (
                    record.as_object_mut(),
                    codec::value(channel.resource, value.word),
                ) {
                    record.extend(native);
                }
                Reading {
                    signal: channel.name.clone(),
                    value: record,
                    source_time: None,
                }
            })
            .collect();
        self.observed = Some(frontier);
        Ok(readings)
    }

    fn retire(&mut self) {
        self.owner = None;
        self.retired = true;
    }
}

impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        if self.retired {
            return Err(closed());
        }
        self.observed = None;
        let result = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("OptoMMP timeout too large".into()).into())
            .and_then(|deadline| self.poll(deadline));
        if result.is_err() {
            // An ambiguous exchange is never retried on this or a new connection.
            self.observed = None;
            self.retire();
        }
        result
    }
    fn ready(&self) -> bool {
        self.polls > 0
    }
    fn observed_ns(&self) -> Option<u64> {
        self.observed
    }
    fn set_acquisition_end(&mut self, end_ns: Option<u64>) {
        self.cutoff = end_ns;
    }
    fn finish(&mut self, _timeout: Duration) -> Result<()> {
        // Closing TCP is the only shutdown action; no device command follows.
        self.retire();
        Ok(())
    }
}

/// Read exactly `bytes`, never past the expected frame.
fn fill(owner: &mut dyn net::Transport, bytes: &mut [u8], deadline: Instant) -> Result<()> {
    let mut count = 0;
    while count < bytes.len() {
        remaining(deadline)?;
        let target = bytes.get_mut(count..).ok_or_else(|| {
            failure(
                ErrorKind::Incomplete,
                "malformed_response",
                "receive extent",
            )
        })?;
        match owner.read(target) {
            Ok(0) => {
                return Err(failure(
                    ErrorKind::Disconnected,
                    "truncated_response",
                    "connection closed during a response",
                ));
            }
            Ok(received) => count += received,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(io_error(&error)),
        }
    }
    Ok(())
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(failure(
            ErrorKind::Timeout,
            "timeout",
            "original poll deadline expired",
        ))
    } else {
        Ok(left)
    }
}
fn closed() -> anyhow::Error {
    Categorized::error(ErrorKind::Disconnected, "OptoMMP source owner retired")
}
fn io_error(error: &io::Error) -> anyhow::Error {
    let (kind, reason) = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => (ErrorKind::Timeout, "timeout"),
        _ => (ErrorKind::Disconnected, "transport"),
    };
    failure(kind, reason, &format!("transport: {error}"))
}

#[cfg(test)]
mod tests;
