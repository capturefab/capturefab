//! Samples the complete exposed `DataMan` ID Reader result; sends no controls.
//!
//! Cognex Industrial Protocols 26.2: class 0x79, instance 1, attribute 0x10.
//! `ResultLength` may exceed stored data after reader-side truncation: reject it.
//! Buffering, acquisition and formatting are external settings, never inferred.
use super::{Connection, MAX_BATCH, cip_reply, cpf, remaining};
use crate::{
    timeseries::{Reading, Source},
    types::{Categorized, ErrorKind, Usage},
};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

// CPF16 + CIP4 + native UINT header8. Returned storage tail shares this bound.
const MAX_RESULT: u16 = 8_164;
const REQUEST: [u8; 8] = [0x0e, 3, 0x20, 0x79, 0x24, 1, 0x30, 0x10];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    address: SocketAddr,
    profile: Profile,
    signal: String,
    #[serde(
        default = "maximum",
        deserialize_with = "crate::config_number::unsigned"
    )]
    max_result_bytes: u16,
    #[serde(default)]
    encoding: Option<Encoding>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Profile {
    CipResult,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Encoding {
    Utf8,
}

const fn maximum() -> u16 {
    MAX_RESULT
}

impl Config {
    fn validate(&self) -> Result<()> {
        crate::timeseries::validate_name(&self.signal, "DataMan signal")?;
        if self.address.port() == 0 || !(1..=MAX_RESULT).contains(&self.max_result_bytes) {
            return Err(Usage(format!(
                "DataMan requires a nonzero port and max_result_bytes from 1 to {MAX_RESULT}"
            ))
            .into());
        }
        match self.profile {
            Profile::CipResult => Ok(()),
        }
    }
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    config.validate()?;
    Ok(Box::new(Reader {
        config: config.clone(),
        connection: None,
        context: 0,
        retired: false,
    }))
}

struct Reader {
    config: Config,
    connection: Option<Connection>,
    context: u64,
    retired: bool,
}

impl Source for Reader {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        if self.retired {
            return Err(Categorized::error(
                ErrorKind::Disconnected,
                "DataMan source retired after an earlier failed poll",
            ));
        }
        let result = (|| {
            let deadline = Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| Usage("DataMan timeout is too large".into()))?;
            self.poll(deadline)
        })();
        if result.is_err() {
            self.retired = true;
            self.connection = None;
        }
        result
    }
}

impl Reader {
    fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        remaining(deadline)?;
        let connection = Connection::get(&mut self.connection, self.config.address, deadline)?;
        let response = connection.exchange_ucmm(&REQUEST, None, &mut self.context, deadline)?;
        remaining(deadline)?;
        let reply = cip_reply(cpf(&response)?, 0x8e, false)?;
        if reply.partial {
            return Err(Categorized::new(
                ErrorKind::Incomplete,
                "DataMan result attribute is partial; no fragment or acknowledgement was sent",
            )
            .details(json!({"cip_status":6,"dataman_failure":"partial_result"}))
            .into());
        }
        let value = decode(&self.config, reply.data, deadline)?;
        let reading = Reading {
            signal: self.config.signal.clone(),
            value,
            source_time: None,
        };
        // Count the complete native Reading before it can enter the source batch.
        if serde_json::to_vec(&reading)?.len() + 2 > MAX_BATCH {
            return Err(incomplete(
                "json_limit",
                "DataMan result exceeds the batch bound",
            ));
        }
        let readings = vec![reading];
        remaining(deadline)?;
        Ok(readings)
    }
}

fn incomplete(reason: &str, message: &str) -> anyhow::Error {
    Categorized::new(ErrorKind::Incomplete, message)
        .details(json!({"dataman_failure":reason}))
        .into()
}

fn decode(config: &Config, bytes: &[u8], deadline: Instant) -> Result<Value> {
    remaining(deadline)?;
    let Some((header, data)) = bytes.split_first_chunk::<8>() else {
        return Err(incomplete(
            "short_header",
            "DataMan result needs its eight-byte native header",
        ));
    };
    let id = u16::from_le_bytes([header[0], header[1]]);
    let code = u16::from_le_bytes([header[2], header[3]]);
    let extended = u16::from_le_bytes([header[4], header[5]]);
    let length = u16::from_le_bytes([header[6], header[7]]);
    if usize::from(length) > data.len() {
        return Err(Categorized::new(
            ErrorKind::Incomplete,
            "DataMan ResultLength exceeds returned storage; reader-side truncation is possible",
        )
        .details(json!({"dataman_failure":"length_exceeds_storage", "result_length_raw":length,"returned_storage_bytes":data.len()}))
        .into());
    }
    if length > config.max_result_bytes {
        return Err(incomplete(
            "result_limit",
            "DataMan ResultLength exceeds max_result_bytes",
        ));
    }
    let (active, tail) = data.split_at(usize::from(length));
    let mut value = json!({
        "kind":"dataman_result", "profile":"cip_result",
        "result_id_raw":id, "result_code_raw":code, "result_extended_raw":extended,
        "result_length_raw":length, "returned_storage_bytes":data.len(),
        "raw":{"encoding":"base64","scope":"decode_results_attribute","data":STANDARD.encode(bytes)},
        "active":{"encoding":"base64","scope":"result_data","data":STANDARD.encode(active)},
        "tail":{"encoding":"base64","scope":"returned_storage_tail","data":STANDARD.encode(tail)}
    });
    if config.encoding.is_some() {
        let text = std::str::from_utf8(active).map_err(|_| {
            incomplete(
                "invalid_utf8",
                "DataMan result is not valid requested UTF-8",
            )
        })?;
        value["encoding"] = json!("utf8");
        value["text"] = json!(text);
    }
    remaining(deadline)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        serde_json::from_value(
            json!({"address":"127.0.0.1:44818","profile":"cip_result","signal":"result"}),
        )
        .unwrap()
    }

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(2)
    }

    fn kind(error: &anyhow::Error) -> ErrorKind {
        crate::types::classify(error).kind
    }

    #[test]
    fn complete_native_literal_preserves_every_field_and_unknown_tail() {
        // Hand literal: ID1234, Code8007, Extendeda5c3, LEN8; delimiters/nontext.
        let native = [
            0x34, 0x12, 7, 0x80, 0xc3, 0xa5, 8, 0, 0x41, 0, 13, 10, 0x7c, 0xff, 0x80, 0x42, 0xde,
            0xad,
        ];
        let value = decode(&config(), &native, deadline()).unwrap();
        assert_eq!(
            value,
            json!({
                "kind":"dataman_result","profile":"cip_result",
                "result_id_raw":4_660,"result_code_raw":32_775,"result_extended_raw":42_435,
                "result_length_raw":8,"returned_storage_bytes":10,
                "raw":{"encoding":"base64","scope":"decode_results_attribute","data":STANDARD.encode(native)},
                "active":{"encoding":"base64","scope":"result_data","data":"QQANCnz/gEI="},
                "tail":{"encoding":"base64","scope":"returned_storage_tail","data":"3q0="}
            })
        );
        assert_eq!(REQUEST, [0x0e, 3, 0x20, 0x79, 0x24, 1, 0x30, 0x10]);
        assert!(value.get("text").is_none());
    }

    #[test]
    fn empty_and_repeated_or_wrapping_identifiers_are_native_observations() {
        for id in [u16::MAX, u16::MAX, 0] {
            let mut bytes = [0; 8];
            bytes[..2].copy_from_slice(&id.to_le_bytes());
            let value = decode(&config(), &bytes, deadline()).unwrap();
            assert_eq!(value["result_id_raw"], id);
            assert_eq!(value["result_code_raw"], 0);
            assert_eq!(value["active"]["data"], "");
            assert_eq!(value["tail"]["data"], "");
        }
    }

    #[test]
    fn utf8_is_explicit_strict_and_keeps_nul_and_original_bytes() {
        let mut config = config();
        config.encoding = Some(Encoding::Utf8);
        let bytes = [
            1, 0, 1, 0, 0, 0, 7, 0, b'A', 0, 0xc2, 0xa9, 0xe2, 0x82, 0xac,
        ];
        let value = decode(&config, &bytes, deadline()).unwrap();
        assert_eq!(value["text"], "A\0©€");
        assert_eq!(value["encoding"], "utf8");
        assert_eq!(value["raw"]["data"], STANDARD.encode(bytes));
        let mut bytes = bytes;
        bytes[10] = 0xff;
        assert_eq!(
            kind(&decode(&config, &bytes, deadline()).unwrap_err()),
            ErrorKind::Incomplete
        );
        config.encoding = None;
        decode(&config, &bytes, deadline()).unwrap();
    }

    #[test]
    fn native_length_is_exact_without_fixed_reader_capacity_assumption() {
        for length in [0, 470, 484, usize::from(MAX_RESULT)] {
            let mut bytes = vec![0; 8 + length];
            bytes[6..8].copy_from_slice(&u16::try_from(length).unwrap().to_le_bytes());
            assert_eq!(
                decode(&config(), &bytes, deadline()).unwrap()["result_length_raw"],
                length
            );
        }
        assert_eq!(
            kind(&decode(&config(), &[0; 7], deadline()).unwrap_err()),
            ErrorKind::Incomplete
        );
        let mut truncated = [0; 8 + 470];
        truncated[6..8].copy_from_slice(&471_u16.to_le_bytes());
        assert_eq!(
            kind(&decode(&config(), &truncated, deadline()).unwrap_err()),
            ErrorKind::Incomplete
        );
        let mut limited = config();
        limited.max_result_bytes = 1;
        assert_eq!(
            kind(&decode(&limited, &[0, 0, 0, 0, 0, 0, 2, 0, 1, 2], deadline()).unwrap_err()),
            ErrorKind::Incomplete
        );
    }

    #[test]
    fn preopen_validation_and_first_expired_read_never_reconnect() {
        for patch in [
            json!({"address":"127.0.0.1:0"}),
            json!({"max_result_bytes":0}),
            json!({"max_result_bytes":8165}),
            json!({"signal":""}),
        ] {
            let mut value =
                json!({"address":"127.0.0.1:44818","profile":"cip_result","signal":"result"});
            for (key, value_patch) in patch.as_object().unwrap() {
                value[key].clone_from(value_patch);
            }
            let config: Config = serde_json::from_value(value).unwrap();
            let Err(error) = open(&config) else {
                panic!("invalid DataMan configuration opened");
            };
            assert_eq!(kind(&error), ErrorKind::Usage);
        }
        let mut source = open(&config()).unwrap();
        assert_eq!(
            kind(&source.read(Duration::ZERO).unwrap_err()),
            ErrorKind::Timeout
        );
        assert_eq!(
            kind(&source.read(Duration::from_secs(1)).unwrap_err()),
            ErrorKind::Disconnected
        );
        assert_eq!(
            kind(&decode(&config(), &[0; 8], Instant::now()).unwrap_err()),
            ErrorKind::Timeout
        );
    }
}
