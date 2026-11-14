//! MELSEC 3E ASCII frames. Mitsubishi `SH-080956ENG-N`, §§4.1–4.2, 5.2.
use super::{Config, Device, DeviceFormat, Signal, protocol, read, target_error};
use anyhow::Result;
use serde_json::Value;
use std::{net::TcpStream, time::Instant};

const END_CODE: usize = 4;
const ERROR_DATA: usize = 18;

impl Device {
    pub(super) const fn decimal_address(self) -> bool {
        !matches!(
            self,
            Self::X | Self::Y | Self::B | Self::W | Self::Sb | Self::Sw | Self::Zr
        )
    }

    const fn name(self, wide: bool) -> &'static str {
        match self {
            Self::Sm => "SM",
            Self::Sd => "SD",
            Self::X => "X",
            Self::Y => "Y",
            Self::M => "M",
            Self::L => "L",
            Self::F => "F",
            Self::V => "V",
            Self::B => "B",
            Self::S => "S",
            Self::D => "D",
            Self::W => "W",
            Self::Ts => "TS",
            Self::Tc => "TC",
            Self::Tn => "TN",
            Self::Sts => {
                if wide {
                    "STS"
                } else {
                    "SS"
                }
            }
            Self::Stc => {
                if wide {
                    "STC"
                } else {
                    "SC"
                }
            }
            Self::Stn => {
                if wide {
                    "STN"
                } else {
                    "SN"
                }
            }
            Self::Ltn => "LTN",
            Self::Lstn => "LSTN",
            Self::Cs => "CS",
            Self::Cc => "CC",
            Self::Cn => "CN",
            Self::Sb => "SB",
            Self::Sw => "SW",
            Self::Z => "Z",
            Self::R => "R",
            Self::Zr => "ZR",
        }
    }
}

fn route(config: &Config) -> String {
    format!(
        "{:02X}{:02X}{:04X}{:02X}",
        config.network, config.station, config.module_io, config.multidrop
    )
}

pub(super) fn request(config: &Config, signal: &Signal) -> Result<Vec<u8>> {
    let wide = matches!(config.device_format, DeviceFormat::IqR);
    let code_width = if wide { 4 } else { 2 };
    let address_width = if wide { 8 } else { 6 };
    let code = signal.device.name(wide);
    let address = signal.address;
    let head = if signal.device.decimal_address() {
        format!("{address:0address_width$}")
    } else {
        format!("{address:0address_width$X}")
    };
    let data = format!(
        "{:04X}0401{:04X}{code:*<code_width$}{head}{:04X}",
        config.monitoring_timer,
        config.subcommand(signal),
        signal.points()?
    );
    Ok(format!("5000{}{:04X}{data}", route(config), data.len()).into_bytes())
}

pub(super) fn reply(
    config: &Config,
    signal: &Signal,
    stream: &mut TcpStream,
    deadline: Instant,
) -> Result<Value> {
    let mut header = [0; 18];
    read(stream, &mut header, deadline)?;
    if &header[..4] != b"D000" || &header[4..14] != route(config).as_bytes() {
        return Err(protocol(
            "3E response subheader or route differs from request",
        ));
    }
    let length = usize::from(hex(&header[14..])?);
    let points = usize::from(signal.points()?);
    let bits = signal.device.bit();
    let expected = if bits { points } else { points * 4 };
    if !(END_CODE..=(END_CODE + expected.max(ERROR_DATA))).contains(&length) {
        return Err(protocol("invalid or oversized 3E ASCII response length"));
    }
    let mut data = vec![0; length];
    read(stream, &mut data, deadline)?;
    let status = hex(&data[..END_CODE])?;
    let payload = &data[END_CODE..];
    if status != 0 {
        // The responding station can differ from the route in the header.
        // A status-only rejection is sufficient to stop acquisition.
        if !payload.is_empty() {
            let Ok(error) = <&[u8; ERROR_DATA]>::try_from(payload) else {
                return Err(protocol("invalid 3E ASCII error data length"));
            };
            for pair in error[..10].as_chunks::<2>().0 {
                hex(pair)?;
            }
            if &error[10..14] != b"0401" || hex(&error[14..])? != config.subcommand(signal) {
                return Err(protocol("3E ASCII error command differs from request"));
            }
        }
        return Err(target_error(status, payload));
    }
    if payload.len() != expected {
        return Err(protocol(
            "3E ASCII value length differs from requested device points",
        ));
    }

    let mut decoded = Vec::with_capacity(signal.bytes()?);
    if bits {
        // Normalize to the same high-nibble-first representation used
        // by the shared typed decoder. An odd final point has zero padding.
        for pair in payload.chunks(2) {
            let first = bit(pair[0])?;
            let second = pair.get(1).copied().map_or(Ok(0), bit)?;
            decoded.push((first << 4) | second);
        }
    } else {
        for word in payload.as_chunks::<4>().0 {
            decoded.extend_from_slice(&hex(word)?.to_le_bytes());
        }
    }
    signal.decode(&decoded, deadline)
}

fn hex(bytes: &[u8]) -> Result<u16> {
    bytes.iter().try_fold(0, |value, byte| {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'A'..=b'F' => byte - b'A' + 10,
            _ => {
                return Err(protocol(
                    "3E ASCII reply contains an invalid hexadecimal digit",
                ));
            }
        };
        Ok((value << 4) | u16::from(digit))
    })
}

fn bit(byte: u8) -> Result<u8> {
    match byte {
        b'0' => Ok(0),
        b'1' => Ok(1),
        _ => Err(protocol("3E ASCII bit response must contain only 0 or 1")),
    }
}
