//! Private bounded messages. No SDK code is loaded in the parent.
use super::config::{
    AccelerationChannel, BridgeRatioChannel, CounterAngularPositionChannel,
    CounterLinearPositionChannel, CounterPeriodChannel, CounterPulseWidthChannel, CurrentChannel,
    EncoderDecoding, Field, MixedChannel, ResistanceChannel, RtdChannel, StrainChannel,
    ThermocoupleChannel, Timing, VoltageChannel, ZIndex,
};
use super::{Config, MAX_ROW, MAX_WORDS, Measurement, TerminalConfig};
use crate::{
    timeseries::Reading,
    types::{Categorized, ErrorKind, ErrorTag},
};
use anyhow::Result;
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{DeserializeOwned, Error as _},
};
use serde_json::{Value, json};
use std::io::{Read, Write};

pub(super) const PROTOCOL: u32 = 10;
pub(super) const MAX_NOTICES: usize = 96;
pub(super) const MAX_DIAGNOSTIC_BYTES: usize = 256;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Packet<T> {
    protocol: u32,
    body: T,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Request {
    Open {
        config: Config,
        deadline_ns: u64,
        cutoff_ns: u64,
    },
    Read {
        deadline_ns: u64,
        cutoff_ns: u64,
    },
    Finish {
        deadline_ns: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Descriptor {
    pub(super) channels: Vec<String>,
    pub(super) measurement: Measurement,
    pub(super) timing: TimingDescriptor,
    #[serde(
        default,
        deserialize_with = "optional_line_counts",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) digital_port_line_counts: Option<Vec<u32>>,
}

fn optional_line_counts<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Vec<u32>>, D::Error> {
    Vec::<u32>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub(super) enum TimingDescriptor {
    Continuous {
        requested_rate_hz: f64,
        nominal_rate_hz: f64,
        host_buffer_samples_per_channel: u32,
    },
    OnDemand {
        poll_interval_ms: u64,
    },
}
impl<'de> Deserialize<'de> for TimingDescriptor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Mode {
            Continuous,
            OnDemand,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input {
            mode: Mode,
            #[serde(default)]
            requested_rate_hz: Field<f64>,
            #[serde(default)]
            nominal_rate_hz: Field<f64>,
            #[serde(default, deserialize_with = "super::config::unsigned_field")]
            host_buffer_samples_per_channel: Field<u32>,
            #[serde(default, deserialize_with = "super::config::unsigned_field")]
            poll_interval_ms: Field<u32>,
        }
        let input = Input::deserialize(deserializer)?;
        match (
            input.mode,
            input.requested_rate_hz,
            input.nominal_rate_hz,
            input.host_buffer_samples_per_channel,
            input.poll_interval_ms,
        ) {
            (
                Mode::Continuous,
                Field::Present(requested_rate_hz),
                Field::Present(nominal_rate_hz),
                Field::Present(host_buffer_samples_per_channel),
                Field::Missing,
            ) => Ok(Self::Continuous {
                requested_rate_hz,
                nominal_rate_hz,
                host_buffer_samples_per_channel,
            }),
            (
                Mode::OnDemand,
                Field::Missing,
                Field::Missing,
                Field::Missing,
                Field::Present(poll_interval_ms),
            ) => Ok(Self::OnDemand {
                poll_interval_ms: u64::from(poll_interval_ms),
            }),
            _ => Err(D::Error::custom("inconsistent NI-DAQmx timing descriptor")),
        }
    }
}
impl Descriptor {
    pub(super) fn for_config(
        config: &Config,
        channels: Vec<String>,
        nominal_rate_hz: Option<f64>,
    ) -> Result<Self> {
        let timing = match (config.timing, nominal_rate_hz) {
            (
                Timing::Continuous {
                    rate_hz,
                    host_buffer_samples_per_channel,
                    ..
                },
                Some(nominal_rate_hz),
            ) => TimingDescriptor::Continuous {
                requested_rate_hz: rate_hz,
                nominal_rate_hz,
                host_buffer_samples_per_channel,
            },
            (Timing::OnDemand { poll_interval_ms }, None) => {
                TimingDescriptor::OnDemand { poll_interval_ms }
            }
            _ => return Err(invalid("NI-DAQmx returned an inconsistent task timing")),
        };
        let descriptor = Self {
            channels,
            measurement: config.measurement.clone(),
            timing,
            digital_port_line_counts: None,
        };
        descriptor.validate(config)?;
        Ok(descriptor)
    }

    pub(super) fn for_digital_ports(
        config: &Config,
        channels: Vec<String>,
        line_counts: Vec<u32>,
    ) -> Result<Self> {
        let Timing::OnDemand { poll_interval_ms } = config.timing else {
            return Err(invalid("NI-DAQmx digital ports require software timing"));
        };
        let descriptor = Self {
            channels,
            measurement: config.measurement.clone(),
            timing: TimingDescriptor::OnDemand { poll_interval_ms },
            digital_port_line_counts: Some(line_counts),
        };
        descriptor.validate(config)?;
        Ok(descriptor)
    }
    pub(super) fn validate(&self, config: &Config) -> Result<()> {
        config.validate()?;
        config.validate_driver_channels(&self.channels)?;
        match (&self.measurement, &self.digital_port_line_counts) {
            (Measurement::DigitalPort { .. }, Some(widths))
                if widths.len() == self.channels.len()
                    && widths.iter().all(|n| (1..=32).contains(n)) => {}
            (Measurement::DigitalPort { .. }, _) | (_, Some(_)) => {
                return Err(invalid(
                    "NI-DAQmx returned inconsistent digital-port line counts",
                ));
            }
            _ => {}
        }
        let timing_matches = match (self.timing, config.timing) {
            (
                TimingDescriptor::Continuous {
                    requested_rate_hz,
                    nominal_rate_hz,
                    host_buffer_samples_per_channel: actual,
                },
                Timing::Continuous {
                    rate_hz,
                    host_buffer_samples_per_channel: requested,
                    ..
                },
            ) => {
                requested_rate_hz.to_bits() == rate_hz.to_bits()
                    && nominal_rate_hz.is_finite()
                    && nominal_rate_hz > 0.0
                    && actual == requested
            }
            (
                TimingDescriptor::OnDemand {
                    poll_interval_ms: actual,
                },
                Timing::OnDemand {
                    poll_interval_ms: requested,
                },
            ) => actual == requested,
            _ => false,
        };
        if !timing_matches || !self.measurement.same_request(&config.measurement) {
            return Err(invalid("NI-DAQmx returned an inconsistent task descriptor"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Notice {
    pub(super) operation: String,
    pub(super) status_code: i32,
    pub(super) diagnostic_hex: String,
    pub(super) diagnostic_truncated: bool,
    pub(super) diagnostic_source: String,
    pub(super) diagnostic_encoding: String,
    pub(super) diagnostic_lookup_status: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Failure {
    pub(super) message: String,
    pub(super) tag: Option<ErrorTag>,
}
impl Failure {
    pub(super) fn from_error(error: &anyhow::Error) -> Self {
        Self {
            message: crate::scrub::scrub(&format!("{error:#}")).into_owned(),
            tag: crate::types::error_tag(error),
        }
    }
    pub(super) fn error(&self) -> anyhow::Error {
        crate::types::error_from_tag(self.tag.clone(), self.message.clone())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Block {
    pub(super) sample_index: u64,
    pub(super) samples_per_channel: u32,
    pub(super) bits_le_hex: String,
    pub(super) status_code: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pacing {
    pub(super) attempt_start_ns: Option<u64>,
    pub(super) next_not_before_ns: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Response {
    Ready {
        descriptor: Descriptor,
        observed_ns: u64,
        notices: Vec<Notice>,
    },
    Read {
        observed_ns: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pacing: Option<Pacing>,
        block: Option<Block>,
        notices: Vec<Notice>,
        terminal: Option<Failure>,
    },
    Finished {
        observed_ns: u64,
        notices: Vec<Notice>,
        terminal: Option<Failure>,
    },
    Failed {
        error: Failure,
    },
}

pub(super) fn invalid(message: &'static str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, message)
}
pub(super) fn now() -> Result<u64> {
    crate::host::monotonic_ns().ok_or_else(|| {
        Categorized::error(
            ErrorKind::Unavailable,
            "NI-DAQmx requires the host monotonic clock",
        )
    })
}
pub(super) fn check_deadline(deadline_ns: u64) -> Result<()> {
    let now = now()?;
    if now >= deadline_ns {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "NI-DAQmx original operation deadline expired",
        ));
    }
    if deadline_ns - now > 31_000_000_000 {
        return Err(Categorized::error(
            ErrorKind::Usage,
            "NI-DAQmx operation budget exceeds 31 seconds",
        ));
    }
    Ok(())
}

pub(super) fn write<T: Serialize>(output: &mut impl Write, body: &T) -> Result<()> {
    let bytes = serde_json::to_vec(&Packet {
        protocol: PROTOCOL,
        body,
    })?;
    if bytes.is_empty() || bytes.len() > MAX_ROW {
        return Err(invalid("NI-DAQmx worker message exceeds 512 KiB"));
    }
    output.write_all(&u32::try_from(bytes.len())?.to_le_bytes())?;
    output.write_all(&bytes)?;
    output.flush()?;
    Ok(())
}
pub(super) fn read<T: DeserializeOwned>(input: &mut impl Read) -> Result<Option<T>> {
    let mut size = [0u8; 4];
    loop {
        match input.read(&mut size[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    input.read_exact(&mut size[1..])?;
    let size = usize::try_from(u32::from_le_bytes(size))?;
    if size == 0 || size > MAX_ROW {
        return Err(invalid("NI-DAQmx worker message exceeds 512 KiB"));
    }
    let mut bytes = vec![0; size];
    input.read_exact(&mut bytes)?;
    let packet: Packet<T> = serde_json::from_slice(&bytes)
        .map_err(|_| invalid("NI-DAQmx worker sent invalid framed JSON"))?;
    if packet.protocol != PROTOCOL {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            "NI-DAQmx worker protocol version differs",
        ));
    }
    Ok(Some(packet.body))
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}
pub(super) fn hex(bytes: &[u8]) -> String {
    let digit = |n| char::from(if n < 10 { b'0' + n } else { b'a' + n - 10 });
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(digit(b >> 4));
        out.push(digit(b & 15));
    }
    out
}
pub(super) fn validate_notices(notices: &[Notice]) -> Result<()> {
    if notices.len() > MAX_NOTICES
        || notices.iter().any(|n| {
            n.operation.is_empty()
                || n.operation.len() > 64
                || !n.operation.is_ascii()
                || n.status_code == 0
                || n.diagnostic_hex.len() > MAX_DIAGNOSTIC_BYTES * 2
                || !n.diagnostic_hex.len().is_multiple_of(2)
                || !n.diagnostic_hex.bytes().all(|b| hex_nibble(b).is_some())
                || !matches!(
                    n.diagnostic_source.as_str(),
                    "extended_error_info" | "error_string" | "unavailable" | "deferred"
                )
                || !matches!(n.diagnostic_encoding.as_str(), "utf8" | "locale_bytes")
        })
    {
        return Err(invalid(
            "NI-DAQmx worker returned invalid or excessive notices",
        ));
    }
    Ok(())
}
fn reading(config: &Config, value: Value) -> Result<Reading> {
    let reading = Reading {
        signal: config.signal.clone(),
        value,
        source_time: None,
    };
    if serde_json::to_vec(&reading)?.len() > MAX_ROW {
        return Err(invalid("NI-DAQmx data row exceeds 512 KiB"));
    }
    Ok(reading)
}
pub(super) fn event_reading(
    config: &Config,
    descriptor: &Descriptor,
    event: &str,
    notices: &[Notice],
) -> Result<Reading> {
    descriptor.validate(config)?;
    validate_notices(notices)?;
    let mut value = json!({"event":event,"channels":descriptor.channels,"notices":notices});
    timing_metadata(&mut value, descriptor, true);
    match &descriptor.measurement {
        Measurement::Voltage {
            terminal_config,
            min_volts,
            max_volts,
            ..
        } => {
            value["terminal_config"] = json!(terminal_config);
            value["min_volts"] = json!(min_volts);
            value["max_volts"] = json!(max_volts);
        }
        Measurement::Thermocouple { .. } => thermocouple_metadata(&mut value, descriptor),
        Measurement::Current { .. } => current_metadata(&mut value, descriptor),
        Measurement::Rtd { .. } => rtd_metadata(&mut value, descriptor),
        Measurement::Mixed { .. } => mixed_metadata(&mut value, descriptor)?,
        Measurement::Strain { .. }
        | Measurement::BridgeRatio { .. }
        | Measurement::Acceleration { .. }
        | Measurement::VelocityIepe { .. }
        | Measurement::ForceIepe { .. }
        | Measurement::ChargeAcceleration { .. }
        | Measurement::Lvdt { .. }
        | Measurement::Rvdt { .. }
        | Measurement::Resistance { .. }
        | Measurement::ThermistorCurrent { .. }
        | Measurement::ThermistorVoltage { .. }
        | Measurement::ForceBridge { .. }
        | Measurement::PressureBridge { .. }
        | Measurement::TorqueBridge { .. }
        | Measurement::CounterPeriod { .. }
        | Measurement::CounterPulseWidth { .. }
        | Measurement::CounterAngularPosition { .. }
        | Measurement::CounterLinearPosition { .. } => {}
        Measurement::DigitalPort { .. }
        | Measurement::CounterCount { .. }
        | Measurement::CounterFrequency { .. } => input_metadata(&mut value, descriptor),
    }
    engineering_metadata(&mut value, descriptor)?;
    reading(config, value)
}

fn timing_metadata(value: &mut Value, descriptor: &Descriptor, event: bool) {
    match descriptor.timing {
        TimingDescriptor::Continuous {
            requested_rate_hz,
            nominal_rate_hz,
            host_buffer_samples_per_channel,
        } => {
            value["requested_rate_hz"] = json!(requested_rate_hz);
            value["nominal_rate_hz"] = json!(nominal_rate_hz);
            value["clock"] = json!("internal_sample_clock_nominal_setting");
            if event {
                value["host_buffer_samples_per_channel"] = json!(host_buffer_samples_per_channel);
            }
        }
        TimingDescriptor::OnDemand { poll_interval_ms } => {
            value["timing"] = json!({"mode":"on_demand","poll_interval_ms":poll_interval_ms,"basis":"host_software_request_spacing"});
            value["clock"] = json!("software_on_demand_host_completion");
        }
    }
}

fn terminal_provenance(terminal: TerminalConfig) -> &'static str {
    if terminal == TerminalConfig::Default {
        "driver_default_unqueried"
    } else {
        "explicit_request_not_read_back"
    }
}
fn thermocouple_channel_metadata(channel: &ThermocoupleChannel, physical: &str) -> Value {
    let mut item = json!({"physical":physical,"thermocouple_type":channel.thermocouple_type,
        "units":channel.units,"min_temperature":channel.min_temperature,"max_temperature":channel.max_temperature,
        "cjc":channel.cjc,"terminal_config_provenance":"driver_default_unqueried"});
    if let Some(request) = channel.terminal_config {
        item["terminal_config_request"] = json!(request);
        item["terminal_config_provenance"] = json!(terminal_provenance(request));
    }
    item
}
fn current_channel_metadata(channel: &CurrentChannel, physical: &str) -> Value {
    json!({"physical":physical,"units":"amps","min_amperes":channel.min_amperes,
        "max_amperes":channel.max_amperes,"shunt":channel.shunt,
        "terminal_config_request":channel.terminal_config,
        "terminal_config_provenance":terminal_provenance(channel.terminal_config)})
}
fn rtd_channel_metadata(channel: &RtdChannel, physical: &str) -> Value {
    json!({"physical":physical,"rtd_type":channel.rtd_type,"units":channel.units,
        "min_temperature":channel.min_temperature,"max_temperature":channel.max_temperature,
        "resistance_config":channel.resistance_config,"r0_ohms":channel.r0_ohms,"excitation":channel.excitation})
}
fn voltage_channel_metadata(channel: &VoltageChannel, physical: &str) -> Value {
    json!({"physical":physical,"units":"volts","min_volts":channel.min_volts,"max_volts":channel.max_volts,
        "terminal_config_request":channel.terminal_config,"terminal_config_provenance":terminal_provenance(channel.terminal_config)})
}
fn thermocouple_metadata(value: &mut Value, descriptor: &Descriptor) {
    if let Measurement::Thermocouple { channels } = &descriptor.measurement {
        value["measurement"] = json!("thermocouple");
        value["settings_provenance"] = json!("requested_not_read_back");
        value["channel_measurements"] = Value::Array(
            channels
                .iter()
                .zip(&descriptor.channels)
                .map(|(channel, physical)| thermocouple_channel_metadata(channel, physical))
                .collect(),
        );
    }
}
fn current_metadata(value: &mut Value, descriptor: &Descriptor) {
    if let Measurement::Current { channels } = &descriptor.measurement {
        value["measurement"] = json!("current");
        value["settings_provenance"] = json!("requested_not_read_back");
        value["channel_measurements"] = Value::Array(
            channels
                .iter()
                .zip(&descriptor.channels)
                .map(|(channel, physical)| current_channel_metadata(channel, physical))
                .collect(),
        );
    }
}
fn rtd_metadata(value: &mut Value, descriptor: &Descriptor) {
    if let Measurement::Rtd { channels } = &descriptor.measurement {
        value["measurement"] = json!("rtd");
        value["settings_provenance"] = json!("requested_not_read_back");
        value["channel_measurements"] = Value::Array(
            channels
                .iter()
                .zip(&descriptor.channels)
                .map(|(channel, physical)| rtd_channel_metadata(channel, physical))
                .collect(),
        );
    }
}
fn mixed_metadata(value: &mut Value, descriptor: &Descriptor) -> Result<()> {
    if let Measurement::Mixed { channels } = &descriptor.measurement {
        value["measurement"] = json!("mixed");
        value["settings_provenance"] = json!("requested_not_read_back");
        value["channel_measurements"] = Value::Array(
            channels
                .iter()
                .zip(&descriptor.channels)
                .enumerate()
                .map(|(index, (channel, physical))| -> Result<Value> {
                    let (mut item, measurement, quantity) = match channel {
                        MixedChannel::Voltage(c) => {
                            (voltage_channel_metadata(c, physical), "voltage", "voltage")
                        }
                        MixedChannel::Thermocouple(c) => (
                            thermocouple_channel_metadata(c, physical),
                            "thermocouple",
                            "temperature",
                        ),
                        MixedChannel::Current(c) => {
                            (current_channel_metadata(c, physical), "current", "current")
                        }
                        MixedChannel::Rtd(c) => {
                            (rtd_channel_metadata(c, physical), "rtd", "temperature")
                        }
                        MixedChannel::Strain(c) => {
                            (strain_channel_metadata(c, physical)?, "strain", "strain")
                        }
                        MixedChannel::BridgeRatio(c) => (
                            bridge_channel_metadata(c, physical)?,
                            "bridge_ratio",
                            "bridge_ratio",
                        ),
                        MixedChannel::Acceleration(c) => (
                            acceleration_channel_metadata(c, physical)?,
                            "acceleration",
                            "acceleration",
                        ),
                        MixedChannel::Resistance(c) => (
                            resistance_channel_metadata(c, physical)?,
                            "resistance",
                            "resistance",
                        ),
                        MixedChannel::ThermistorCurrent(c) => (
                            engineering_ai_request(c, physical)?,
                            "thermistor_current",
                            "temperature",
                        ),
                        MixedChannel::ThermistorVoltage(c) => (
                            engineering_ai_request(c, physical)?,
                            "thermistor_voltage",
                            "temperature",
                        ),
                        MixedChannel::ForceBridge(c) => (
                            physical_bridge_channel_metadata(c, physical)?,
                            "force_bridge",
                            "force",
                        ),
                        MixedChannel::PressureBridge(c) => (
                            physical_bridge_channel_metadata(c, physical)?,
                            "pressure_bridge",
                            "pressure",
                        ),
                        MixedChannel::TorqueBridge(c) => (
                            physical_bridge_channel_metadata(c, physical)?,
                            "torque_bridge",
                            "torque",
                        ),
                        MixedChannel::VelocityIepe(c) => (
                            terminal_sensor_channel_metadata(c, physical, c.terminal_config)?,
                            "velocity_iepe",
                            "velocity",
                        ),
                        MixedChannel::ForceIepe(c) => (
                            terminal_sensor_channel_metadata(c, physical, c.terminal_config)?,
                            "force_iepe",
                            "force",
                        ),
                        MixedChannel::ChargeAcceleration(c) => (
                            terminal_sensor_channel_metadata(c, physical, c.terminal_config)?,
                            "charge_acceleration",
                            "acceleration",
                        ),
                        MixedChannel::Lvdt(c) => (
                            engineering_ai_request(c, physical)?,
                            "lvdt",
                            "linear_position",
                        ),
                        MixedChannel::Rvdt(c) => (
                            engineering_ai_request(c, physical)?,
                            "rvdt",
                            "angular_position",
                        ),
                    };
                    item["channel_index"] = json!(index);
                    item["measurement"] = json!(measurement);
                    item["quantity"] = json!(quantity);
                    Ok(item)
                })
                .collect::<Result<Vec<_>>>()?,
        );
    }
    Ok(())
}

fn request_object<T: Serialize>(channel: &T) -> Result<serde_json::Map<String, Value>> {
    match serde_json::to_value(channel)? {
        Value::Object(fields) => Ok(fields),
        _ => Err(invalid(
            "NI-DAQmx engineering channel metadata is not an object",
        )),
    }
}
fn engineering_ai_request<T: Serialize>(channel: &T, physical: &str) -> Result<Value> {
    let mut fields = request_object(channel)?;
    fields.insert("physical".into(), json!(physical));
    let coupling = fields.remove("coupling");
    fields.insert(
        "coupling_provenance".into(),
        json!(if coupling.is_some() {
            "explicit_request_not_read_back"
        } else {
            "driver_default_unqueried"
        }),
    );
    if let Some(coupling) = coupling {
        fields.insert("coupling_request".into(), coupling);
    }
    Ok(Value::Object(fields))
}
fn strain_channel_metadata(channel: &StrainChannel, physical: &str) -> Result<Value> {
    let mut item = engineering_ai_request(channel, physical)?;
    item["units"] = json!("strain");
    Ok(item)
}
fn bridge_channel_metadata(channel: &BridgeRatioChannel, physical: &str) -> Result<Value> {
    engineering_ai_request(channel, physical)
}
fn acceleration_channel_metadata(channel: &AccelerationChannel, physical: &str) -> Result<Value> {
    let mut item = engineering_ai_request(channel, physical)?;
    item.as_object_mut()
        .ok_or_else(|| invalid("NI-DAQmx acceleration metadata is not an object"))?
        .remove("terminal_config");
    item["terminal_config_request"] = json!(channel.terminal_config);
    item["terminal_config_provenance"] = json!(terminal_provenance(channel.terminal_config));
    Ok(item)
}
fn terminal_sensor_channel_metadata<T: Serialize>(
    channel: &T,
    physical: &str,
    terminal: TerminalConfig,
) -> Result<Value> {
    let mut item = engineering_ai_request(channel, physical)?;
    item.as_object_mut()
        .ok_or_else(|| invalid("NI-DAQmx dynamic channel metadata is not an object"))?
        .remove("terminal_config");
    item["terminal_config_request"] = json!(terminal);
    item["terminal_config_provenance"] = json!(terminal_provenance(terminal));
    Ok(item)
}
fn resistance_channel_metadata(channel: &ResistanceChannel, physical: &str) -> Result<Value> {
    let mut item = engineering_ai_request(channel, physical)?;
    item["units"] = json!("ohms");
    Ok(item)
}
fn physical_bridge_channel_metadata<T: Serialize>(channel: &T, physical: &str) -> Result<Value> {
    let mut item = engineering_ai_request(channel, physical)?;
    item["calibration_provenance"] = json!("declared_two_point_linear_request");
    Ok(item)
}
fn counter_request<T: Serialize>(channel: &T, physical: &str, terminals: &[&str]) -> Result<Value> {
    let mut fields = request_object(channel)?;
    let requested = fields
        .insert("physical".into(), json!(physical))
        .ok_or_else(|| invalid("NI-DAQmx engineering channel lacks a physical request"))?;
    fields.insert("requested_physical".into(), requested);
    for field in terminals {
        let terminal = fields.remove(*field);
        fields.insert(
            format!("{field}_provenance"),
            json!(if terminal.is_some() {
                "explicit_request_not_read_back"
            } else {
                "driver_default_unqueried"
            }),
        );
        if let Some(terminal) = terminal {
            fields.insert(format!("{field}_request"), terminal);
        }
    }
    Ok(Value::Object(fields))
}
fn period_channel_metadata(channel: &CounterPeriodChannel, physical: &str) -> Result<Value> {
    let mut item = counter_request(channel, physical, &["input_terminal"])?;
    item["units"] = json!("seconds");
    Ok(item)
}
fn pulse_width_channel_metadata(
    channel: &CounterPulseWidthChannel,
    physical: &str,
) -> Result<Value> {
    let mut item = counter_request(channel, physical, &["input_terminal"])?;
    item["units"] = json!("seconds");
    Ok(item)
}
fn encoder_request<T: Serialize>(
    channel: &T,
    physical: &str,
    decoding: EncoderDecoding,
    z: ZIndex,
) -> Result<Value> {
    let mut item = counter_request(
        channel,
        physical,
        &["a_input_terminal", "b_input_terminal", "z_input_terminal"],
    )?;
    item["z_index_phase_applicability"] = json!(if !z.enabled {
        "z_index_disabled"
    } else if decoding == EncoderDecoding::TwoPulseCounting {
        "ignored_for_two_pulse_counting"
    } else {
        "quadrature_phase_request"
    });
    if !z.enabled {
        item["z_input_terminal_provenance"] = json!("not_requested_z_disabled");
    }
    Ok(item)
}
fn angular_channel_metadata(
    channel: &CounterAngularPositionChannel,
    physical: &str,
) -> Result<Value> {
    encoder_request(channel, physical, channel.decoding, channel.z_index)
}
fn linear_channel_metadata(
    channel: &CounterLinearPositionChannel,
    physical: &str,
) -> Result<Value> {
    encoder_request(channel, physical, channel.decoding, channel.z_index)
}
fn engineering_channels<T>(
    channels: &[T],
    descriptor: &Descriptor,
    convert: impl Fn(&T, &str) -> Result<Value>,
) -> Result<Vec<Value>> {
    channels
        .iter()
        .zip(&descriptor.channels)
        .enumerate()
        .map(|(index, (channel, physical))| {
            let mut item = convert(channel, physical)?;
            item["channel_index"] = json!(index);
            Ok(item)
        })
        .collect()
}
fn engineering_metadata(value: &mut Value, descriptor: &Descriptor) -> Result<()> {
    let (measurement, channels) = match &descriptor.measurement {
        Measurement::Strain { channels } => (
            "strain",
            engineering_channels(channels, descriptor, strain_channel_metadata)?,
        ),
        Measurement::BridgeRatio { channels } => (
            "bridge_ratio",
            engineering_channels(channels, descriptor, bridge_channel_metadata)?,
        ),
        Measurement::Acceleration { channels } => (
            "acceleration",
            engineering_channels(channels, descriptor, acceleration_channel_metadata)?,
        ),
        Measurement::VelocityIepe { channels } => (
            "velocity_iepe",
            engineering_channels(channels, descriptor, |c, physical| {
                terminal_sensor_channel_metadata(c, physical, c.terminal_config)
            })?,
        ),
        Measurement::ForceIepe { channels } => (
            "force_iepe",
            engineering_channels(channels, descriptor, |c, physical| {
                terminal_sensor_channel_metadata(c, physical, c.terminal_config)
            })?,
        ),
        Measurement::ChargeAcceleration { channels } => (
            "charge_acceleration",
            engineering_channels(channels, descriptor, |c, physical| {
                terminal_sensor_channel_metadata(c, physical, c.terminal_config)
            })?,
        ),
        Measurement::Lvdt { channels } => (
            "lvdt",
            engineering_channels(channels, descriptor, engineering_ai_request)?,
        ),
        Measurement::Rvdt { channels } => (
            "rvdt",
            engineering_channels(channels, descriptor, engineering_ai_request)?,
        ),
        Measurement::Resistance { channels } => (
            "resistance",
            engineering_channels(channels, descriptor, resistance_channel_metadata)?,
        ),
        Measurement::ThermistorCurrent { channels } => (
            "thermistor_current",
            engineering_channels(channels, descriptor, engineering_ai_request)?,
        ),
        Measurement::ThermistorVoltage { channels } => (
            "thermistor_voltage",
            engineering_channels(channels, descriptor, engineering_ai_request)?,
        ),
        Measurement::ForceBridge { channels } => (
            "force_bridge",
            engineering_channels(channels, descriptor, physical_bridge_channel_metadata)?,
        ),
        Measurement::PressureBridge { channels } => (
            "pressure_bridge",
            engineering_channels(channels, descriptor, physical_bridge_channel_metadata)?,
        ),
        Measurement::TorqueBridge { channels } => (
            "torque_bridge",
            engineering_channels(channels, descriptor, physical_bridge_channel_metadata)?,
        ),
        Measurement::CounterPeriod { channels } => (
            "counter_period",
            engineering_channels(channels, descriptor, period_channel_metadata)?,
        ),
        Measurement::CounterPulseWidth { channels } => (
            "counter_pulse_width",
            engineering_channels(channels, descriptor, pulse_width_channel_metadata)?,
        ),
        Measurement::CounterAngularPosition { channels } => (
            "counter_angular_position",
            engineering_channels(channels, descriptor, angular_channel_metadata)?,
        ),
        Measurement::CounterLinearPosition { channels } => (
            "counter_linear_position",
            engineering_channels(channels, descriptor, linear_channel_metadata)?,
        ),
        _ => return Ok(()),
    };
    value["measurement"] = json!(measurement);
    value["settings_provenance"] = json!("requested_not_read_back");
    value["channel_measurements"] = Value::Array(channels);
    Ok(())
}

fn input_terminal_metadata(item: &mut Value, terminal: Option<&str>) {
    item["input_terminal_provenance"] = json!(if terminal.is_some() {
        "explicit_request_not_read_back"
    } else {
        "driver_default_unqueried"
    });
    if let Some(terminal) = terminal {
        item["input_terminal_request"] = json!(terminal);
    }
}

fn input_metadata(value: &mut Value, descriptor: &Descriptor) {
    let (measurement, items) = match &descriptor.measurement {
        Measurement::DigitalPort { channels } => {
            let widths = descriptor
                .digital_port_line_counts
                .as_deref()
                .unwrap_or_default();
            ("digital_port", channels.iter().zip(&descriptor.channels).zip(widths).enumerate().map(|(index,((physical,actual),width))| {
                json!({"channel_index":index,"physical":physical,"physical_provenance":"constructor_request_not_read_back",
                    "read_channel":actual,"line_count":width,"line_count_provenance":"driver_channel_property",
                    "line_grouping":"chan_for_all_lines"})
            }).collect())
        }
        Measurement::CounterCount { channels } => {
            value["settings_provenance"] = json!("requested_not_read_back");
            ("counter_count", channels.iter().zip(&descriptor.channels).map(|(channel,actual)| {
                let mut item = json!({"channel_index":0,"physical":actual,"requested_physical":channel.physical,
                    "edge":channel.edge,"initial_count":channel.initial_count,"direction":channel.direction,"api_word_bits":32});
                input_terminal_metadata(&mut item,channel.input_terminal.as_deref()); item
            }).collect())
        }
        Measurement::CounterFrequency { channels } => {
            value["settings_provenance"] = json!("requested_not_read_back");
            ("counter_frequency", channels.iter().zip(&descriptor.channels).map(|(channel,actual)| {
                let mut item = json!({"channel_index":0,"physical":actual,"requested_physical":channel.physical,"units":"hz",
                    "min_hz":channel.min_hz,"max_hz":channel.max_hz,"edge":channel.edge,"method":channel.method,
                    "measurement_time_seconds":channel.measurement_time_seconds,"divisor":channel.divisor});
                input_terminal_metadata(&mut item,channel.input_terminal.as_deref()); item
            }).collect())
        }
        _ => return,
    };
    value["measurement"] = json!(measurement);
    value["channel_measurements"] = Value::Array(items);
}

pub(super) fn block_reading(
    config: &Config,
    descriptor: &Descriptor,
    block: &Block,
    notices: &[Notice],
) -> Result<Reading> {
    descriptor.validate(config)?;
    validate_notices(notices)?;
    let count = usize::try_from(block.samples_per_channel)?;
    let words = count
        .checked_mul(descriptor.channels.len())
        .ok_or_else(|| invalid("NI-DAQmx block shape overflow"))?;
    let word_bytes = descriptor.measurement.word_bytes();
    let word_hex = word_bytes * 2;
    if count == 0
        || block.samples_per_channel > config.max_samples_per_channel()
        || words > MAX_WORDS
        || block.bits_le_hex.len() != words * word_hex
        || block
            .sample_index
            .checked_add(u64::from(block.samples_per_channel))
            .is_none()
    {
        return Err(invalid(
            "NI-DAQmx worker returned an invalid sample block shape",
        ));
    }
    let (event, representation, finite_key) = match &descriptor.measurement {
        Measurement::Voltage { .. } => {
            ("analog_voltage_block", "driver_scaled_f64_voltage", "volts")
        }
        Measurement::Thermocouple { .. } => (
            "thermocouple_temperature_block",
            "driver_scaled_f64_temperature",
            "temperature",
        ),
        Measurement::Current { .. } => (
            "analog_current_block",
            "driver_scaled_f64_current",
            "amperes",
        ),
        Measurement::Rtd { .. } => (
            "rtd_temperature_block",
            "driver_scaled_f64_rtd_temperature",
            "temperature",
        ),
        Measurement::Mixed { .. } => ("mixed_analog_block", "driver_scaled_f64_mixed_analog", ""),
        Measurement::Strain { .. } => ("strain_block", "driver_scaled_f64_strain", "strain"),
        Measurement::BridgeRatio { .. } => (
            "bridge_ratio_block",
            "driver_scaled_f64_bridge_ratio",
            "ratio",
        ),
        Measurement::Acceleration { .. } => (
            "acceleration_block",
            "driver_scaled_f64_acceleration",
            "acceleration",
        ),
        Measurement::VelocityIepe { .. } => (
            "velocity_iepe_block",
            "driver_scaled_f64_velocity_iepe",
            "velocity",
        ),
        Measurement::ForceIepe { .. } => {
            ("force_iepe_block", "driver_scaled_f64_force_iepe", "force")
        }
        Measurement::ChargeAcceleration { .. } => (
            "charge_acceleration_block",
            "driver_scaled_f64_charge_acceleration",
            "acceleration",
        ),
        Measurement::Lvdt { .. } => (
            "lvdt_position_block",
            "driver_scaled_f64_lvdt_position",
            "position",
        ),
        Measurement::Rvdt { .. } => ("rvdt_angle_block", "driver_scaled_f64_rvdt_angle", "angle"),
        Measurement::Resistance { .. } => {
            ("resistance_block", "driver_scaled_f64_resistance", "ohms")
        }
        Measurement::ThermistorCurrent { .. } => (
            "thermistor_current_block",
            "driver_scaled_f64_thermistor_current",
            "temperature",
        ),
        Measurement::ThermistorVoltage { .. } => (
            "thermistor_voltage_block",
            "driver_scaled_f64_thermistor_voltage",
            "temperature",
        ),
        Measurement::ForceBridge { .. } => (
            "force_bridge_block",
            "driver_scaled_f64_force_bridge",
            "force",
        ),
        Measurement::PressureBridge { .. } => (
            "pressure_bridge_block",
            "driver_scaled_f64_pressure_bridge",
            "pressure",
        ),
        Measurement::TorqueBridge { .. } => (
            "torque_bridge_block",
            "driver_scaled_f64_torque_bridge",
            "torque",
        ),
        Measurement::CounterPeriod { .. } => (
            "counter_period_block",
            "driver_scaled_f64_counter_period",
            "period_seconds",
        ),
        Measurement::CounterPulseWidth { .. } => (
            "counter_pulse_width_block",
            "driver_scaled_f64_counter_pulse_width",
            "pulse_width_seconds",
        ),
        Measurement::CounterAngularPosition { .. } => (
            "counter_angular_position_block",
            "driver_scaled_f64_counter_angular_position",
            "angle",
        ),
        Measurement::CounterLinearPosition { .. } => (
            "counter_linear_position_block",
            "driver_scaled_f64_counter_linear_position",
            "position",
        ),

        Measurement::DigitalPort { .. } => ("digital_port_block", "sdk_u32_digital_port", "value"),
        Measurement::CounterCount { .. } => (
            "counter_count_block",
            "driver_unscaled_u32_counter_count",
            "count",
        ),
        Measurement::CounterFrequency { .. } => (
            "counter_frequency_block",
            "driver_scaled_f64_counter_frequency",
            "frequency_hz",
        ),
    };
    let mut samples = Vec::with_capacity(count);
    for scan in block
        .bits_le_hex
        .as_bytes()
        .chunks_exact(descriptor.channels.len() * word_hex)
    {
        let mut row = Vec::with_capacity(descriptor.channels.len());
        for (column, word) in scan.chunks_exact(word_hex).enumerate() {
            let mut bytes = [0u8; 8];
            for (output, pair) in bytes.iter_mut().zip(word.as_chunks::<2>().0) {
                *output = hex_nibble(pair[0])
                    .zip(hex_nibble(pair[1]))
                    .map(|(hi, lo)| (hi << 4) | lo)
                    .ok_or_else(|| {
                        invalid("NI-DAQmx sample words are not canonical little-endian hex")
                    })?;
            }
            let sample = if word_bytes == 4 {
                let raw = [bytes[0], bytes[1], bytes[2], bytes[3]];
                let mut sample = json!({"u32_le_hex":hex(&raw)});
                sample[finite_key] = json!(u32::from_le_bytes(raw));
                sample
            } else {
                let mut sample = json!({"ieee754_le_hex":hex(&bytes)});
                let scaled = f64::from_bits(u64::from_le_bytes(bytes));
                if let Some(number) = serde_json::Number::from_f64(scaled) {
                    let key = match &descriptor.measurement {
                        Measurement::Mixed { channels } => channels[column].finite_key(),
                        _ => finite_key,
                    };
                    sample[key] = Value::Number(number);
                }
                sample
            };
            row.push(sample);
        }
        samples.push(row);
    }
    let mut value = json!({"event":event,"channels":descriptor.channels,"layout":"scan_major",
        "sample_index":block.sample_index,"samples_per_channel":block.samples_per_channel,"samples":samples,
        "representation":representation,
        "status_code":block.status_code,"notices":notices});
    timing_metadata(&mut value, descriptor, false);
    thermocouple_metadata(&mut value, descriptor);
    current_metadata(&mut value, descriptor);
    rtd_metadata(&mut value, descriptor);
    mixed_metadata(&mut value, descriptor)?;
    input_metadata(&mut value, descriptor);
    engineering_metadata(&mut value, descriptor)?;
    reading(config, value)
}
