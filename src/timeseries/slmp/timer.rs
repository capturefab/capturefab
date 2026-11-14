//! Complete SLMP long-timer records from Mitsubishi `SH-080956ENG-N` §5.2.
//! Four native words retain the current pair, status and system word. No timebase
//! or ordering/signedness interpretation of the current pair is inferred.
use super::{Device, MAX_BATCH, Reading, Signal, SignalType, SizeLimit, protocol, remaining};
use crate::types::Usage;
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::VecDeque, time::Instant};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    LongTimer,
}

// All required fields, shortest integer spellings and shortest true flags.
// [0,0,3,0] makes both true flags reachable while every word has one digit.
const MIN_LTN: usize = br#"{"kind":"slmp_long_timer","device":"LTN","raw_words":[0,0,3,0],"current_word_pair_raw":[0,0],"coil":true,"contact":true}"#.len();

fn name(device: Device) -> Result<&'static str> {
    match device {
        Device::Ltn => Ok("LTN"),
        Device::Lstn => Ok("LSTN"),
        _ => Err(Usage("SLMP long_timer requires LTN or LSTN".into()).into()),
    }
}

fn minimum_value(signal: &Signal) -> Result<usize> {
    let scalar = match signal.data_type {
        SignalType::Timer(_) => MIN_LTN + usize::from(name(signal.device)? == "LSTN"),
        SignalType::Numeric(kind) => {
            if matches!(kind, super::DataType::Bool) {
                4
            } else {
                1
            }
        }
    };
    let elements = usize::from(signal.elements);
    if elements == 1 {
        return Ok(scalar);
    }
    scalar
        .checked_mul(elements)
        .and_then(|bytes| bytes.checked_add(elements + 1))
        .ok_or_else(|| protocol("serialized poll minimum overflow"))
}

fn minimum_reading(signal: &Signal) -> Result<usize> {
    let envelope = Reading {
        signal: signal.name.clone(),
        value: json!(0),
        source_time: None,
    };
    let mut count = SizeLimit { bytes: 0 };
    serde_json::to_writer(&mut count, &envelope)
        .map_err(|_| protocol("serialized poll exceeds 1 MiB batch limit"))?;
    // Remove the one-byte value placeholder, then charge the inherited one-byte
    // per-reading separator. Their equal widths leave count.bytes unchanged.
    count
        .bytes
        .checked_add(minimum_value(signal)?)
        .ok_or_else(|| protocol("serialized poll minimum overflow"))
}

#[derive(Default)]
pub(super) struct Budget {
    remaining: usize,
    readings: VecDeque<usize>,
}
impl Budget {
    pub(super) fn new(signals: &[Signal]) -> Result<Self> {
        if !signals.iter().any(|signal| signal.data_type.is_timer()) {
            return Ok(Self::default());
        }
        let mut budget = Self::default();
        for signal in signals {
            let bytes = minimum_reading(signal)?;
            budget.remaining = budget
                .remaining
                .checked_add(bytes)
                .ok_or_else(|| protocol("serialized poll minimum overflow"))?;
            budget.readings.push_back(bytes);
        }
        Ok(budget)
    }
    pub(super) fn check(&self, charged: usize) -> Result<()> {
        if charged
            .checked_add(self.remaining)
            .is_none_or(|bytes| bytes > MAX_BATCH)
        {
            return Err(protocol("serialized poll exceeds 1 MiB batch limit"));
        }
        Ok(())
    }
    pub(super) fn advance(&mut self) {
        if let Some(bytes) = self.readings.pop_front() {
            self.remaining -= bytes;
        }
    }
}

pub(super) fn decode(signal: &Signal, data: &[u8], deadline: Instant) -> Result<Value> {
    let device = name(signal.device)?;
    if !(1..=240).contains(&signal.elements) || data.len() != usize::from(signal.elements) * 8 {
        return Err(protocol(
            "long_timer response differs from requested whole four-word elements",
        ));
    }
    let mut values = Vec::with_capacity(usize::from(signal.elements));
    for raw in data.as_chunks::<8>().0 {
        remaining(deadline)?;
        let [a, b, c, d, e, f, g, h] = *raw;
        let words = [
            u16::from_le_bytes([a, b]),
            u16::from_le_bytes([c, d]),
            u16::from_le_bytes([e, f]),
            u16::from_le_bytes([g, h]),
        ];
        let [first, second, status, _system] = words;
        let value = json!({
            "kind":"slmp_long_timer",
            "device":device,
            "raw_words":words,
            "current_word_pair_raw":[first,second],
            "coil":status & 1 != 0,
            "contact":status & 2 != 0,
        });
        remaining(deadline)?;
        values.push(value);
    }
    remaining(deadline)?;
    if signal.elements == 1 {
        values
            .pop()
            .ok_or_else(|| protocol("long_timer scalar is missing"))
    } else {
        Ok(Value::Array(values))
    }
}

#[cfg(test)]
mod tests;
