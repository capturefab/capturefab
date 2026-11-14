//! Typed input requests. Parsing and validation never load a driver.
use anyhow::Result;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use std::{collections::BTreeSet, path::PathBuf};

use crate::types::{Categorized, ErrorKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TerminalConfig {
    Default,
    Rse,
    Nrse,
    Differential,
    Pseudodifferential,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum ThermocoupleType {
    J,
    K,
    N,
    R,
    S,
    T,
    B,
    E,
    A,
    C,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TemperatureUnits {
    DegC,
    DegF,
    Kelvin,
    DegR,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum ColdJunction {
    BuiltIn,
    Constant { value: f64 },
}
impl<'de> Deserialize<'de> for ColdJunction {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Source {
            BuiltIn,
            Constant,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input {
            source: Source,
            #[serde(default)]
            value: Field<f64>,
        }
        let input = Input::deserialize(deserializer)?;
        match (input.source, input.value) {
            (Source::BuiltIn, Field::Missing) => Ok(Self::BuiltIn),
            (Source::Constant, Field::Present(value)) => Ok(Self::Constant { value }),
            _ => Err(D::Error::custom(
                "NI-DAQmx built_in CJC has no value; constant CJC requires a value in the channel's units",
            )),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ThermocoupleChannel {
    pub(super) physical: String,
    pub(super) thermocouple_type: ThermocoupleType,
    pub(super) units: TemperatureUnits,
    pub(super) min_temperature: f64,
    pub(super) max_temperature: f64,
    pub(super) cjc: ColdJunction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) terminal_config: Option<TerminalConfig>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum CurrentShunt {
    BuiltIn,
    External { ohms: f64 },
}
impl<'de> Deserialize<'de> for CurrentShunt {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Source {
            BuiltIn,
            External,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input {
            source: Source,
            #[serde(default)]
            ohms: Field<f64>,
        }
        let input = Input::deserialize(deserializer)?;
        match (input.source, input.ohms) {
            (Source::BuiltIn, Field::Missing) => Ok(Self::BuiltIn),
            (Source::External, Field::Present(ohms)) => Ok(Self::External { ohms }),
            _ => Err(D::Error::custom(
                "NI-DAQmx built_in shunt has no resistor value; external shunt requires explicit ohms",
            )),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CurrentChannel {
    pub(super) physical: String,
    pub(super) terminal_config: TerminalConfig,
    pub(super) min_amperes: f64,
    pub(super) max_amperes: f64,
    pub(super) shunt: CurrentShunt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum RtdType {
    Pt3750,
    Pt3851,
    Pt3911,
    Pt3916,
    Pt3920,
    Pt3928,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub(super) enum ResistanceConfig {
    #[serde(rename = "two_wire")]
    Two,
    #[serde(rename = "three_wire")]
    Three,
    #[serde(rename = "four_wire")]
    Four,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub(super) enum CurrentExcitation {
    Internal { amperes: f64 },
    External { amperes: f64 },
}
impl<'de> Deserialize<'de> for CurrentExcitation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Source {
            Internal,
            External,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input {
            source: Source,
            amperes: f64,
        }
        let input = Input::deserialize(deserializer)?;
        Ok(match input.source {
            Source::Internal => Self::Internal {
                amperes: input.amperes,
            },
            Source::External => Self::External {
                amperes: input.amperes,
            },
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RtdChannel {
    pub(super) physical: String,
    pub(super) rtd_type: RtdType,
    pub(super) units: TemperatureUnits,
    pub(super) min_temperature: f64,
    pub(super) max_temperature: f64,
    pub(super) resistance_config: ResistanceConfig,
    pub(super) r0_ohms: f64,
    pub(super) excitation: CurrentExcitation,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct VoltageChannel {
    pub(super) physical: String,
    pub(super) terminal_config: TerminalConfig,
    pub(super) min_volts: f64,
    pub(super) max_volts: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Edge {
    Rising,
    Falling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CountDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum FrequencyMethod {
    LowFrequencyOneCounter,
    HighFrequencyTwoCounter,
    LargeRangeTwoCounter,
    DynamicAveraging,
}

fn optional_input_terminal<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CounterCountChannel {
    pub(super) physical: String,
    pub(super) edge: Edge,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(super) initial_count: u32,
    pub(super) direction: CountDirection,
    #[serde(
        default,
        deserialize_with = "optional_input_terminal",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) input_terminal: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CounterFrequencyChannel {
    pub(super) physical: String,
    pub(super) min_hz: f64,
    pub(super) max_hz: f64,
    pub(super) edge: Edge,
    pub(super) method: FrequencyMethod,
    pub(super) measurement_time_seconds: f64,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(super) divisor: u32,
    #[serde(
        default,
        deserialize_with = "optional_input_terminal",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) input_terminal: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ExcitationSource {
    Internal,
    External,
    None,
}
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct VoltageExcitation {
    pub(super) source: ExcitationSource,
    pub(super) volts: f64,
}
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AccelerationExcitation {
    pub(super) source: ExcitationSource,
    pub(super) amperes: f64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Coupling {
    Ac,
    Dc,
    Ground,
}
fn optional_coupling<'de, D: Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<Coupling>, D::Error> {
    Coupling::deserialize(d).map(Some)
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum StrainBridgeType {
    FullBridgeI,
    FullBridgeIi,
    FullBridgeIii,
    HalfBridgeI,
    HalfBridgeIi,
    QuarterBridgeI,
    QuarterBridgeIi,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum BridgeConfig {
    #[serde(rename = "full_bridge")]
    Full,
    #[serde(rename = "half_bridge")]
    Half,
    #[serde(rename = "quarter_bridge")]
    Quarter,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum BridgeUnits {
    VoltsPerVolt,
    MillivoltsPerVolt,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub(super) enum AccelerationUnits {
    #[serde(rename = "g")]
    G,
    #[serde(rename = "m_s2")]
    MetersPerSecondSquared,
    #[serde(rename = "in_s2")]
    InchesPerSecondSquared,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub(super) enum SensitivityUnits {
    #[serde(rename = "mv_per_g")]
    MillivoltsPerG,
    #[serde(rename = "v_per_g")]
    VoltsPerG,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StrainChannel {
    pub(super) physical: String,
    pub(super) min_strain: f64,
    pub(super) max_strain: f64,
    pub(super) bridge_type: StrainBridgeType,
    pub(super) excitation: VoltageExcitation,
    pub(super) gage_factor: f64,
    pub(super) initial_bridge_volts: f64,
    pub(super) nominal_gage_ohms: f64,
    pub(super) poisson_ratio: f64,
    pub(super) lead_wire_ohms: f64,
    #[serde(
        default,
        deserialize_with = "optional_coupling",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) coupling: Option<Coupling>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BridgeRatioChannel {
    pub(super) physical: String,
    pub(super) min_ratio: f64,
    pub(super) max_ratio: f64,
    pub(super) units: BridgeUnits,
    pub(super) bridge_config: BridgeConfig,
    pub(super) excitation: VoltageExcitation,
    pub(super) nominal_bridge_ohms: f64,
    #[serde(
        default,
        deserialize_with = "optional_coupling",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) coupling: Option<Coupling>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AccelerationChannel {
    pub(super) physical: String,
    pub(super) terminal_config: TerminalConfig,
    pub(super) min_acceleration: f64,
    pub(super) max_acceleration: f64,
    pub(super) units: AccelerationUnits,
    pub(super) sensitivity: f64,
    pub(super) sensitivity_units: SensitivityUnits,
    pub(super) excitation: AccelerationExcitation,
    #[serde(
        default,
        deserialize_with = "optional_coupling",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) coupling: Option<Coupling>,
}
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CurrentInputExcitation {
    pub(super) source: ExcitationSource,
    pub(super) amperes: f64,
}
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ThermistorCoefficients {
    pub(super) a: f64,
    pub(super) b: f64,
    pub(super) c: f64,
}
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CalibrationPoint {
    pub(super) electrical: f64,
    pub(super) physical: f64,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BridgeCalibration<Units> {
    pub(super) electrical_units: BridgeUnits,
    pub(super) physical_units: Units,
    pub(super) first: CalibrationPoint,
    pub(super) second: CalibrationPoint,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ForceUnits {
    Newtons,
    Pounds,
    KilogramForce,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PressureUnits {
    Pascals,
    Psi,
    Bar,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TorqueUnits {
    NewtonMeters,
    InchOunces,
    InchPounds,
    FootPounds,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ResistanceChannel {
    pub(super) physical: String,
    pub(super) min_ohms: f64,
    pub(super) max_ohms: f64,
    pub(super) resistance_config: ResistanceConfig,
    pub(super) excitation: CurrentInputExcitation,
    #[serde(
        default,
        deserialize_with = "optional_coupling",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) coupling: Option<Coupling>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ThermistorCurrentChannel {
    pub(super) physical: String,
    pub(super) min_temperature: f64,
    pub(super) max_temperature: f64,
    pub(super) units: TemperatureUnits,
    pub(super) resistance_config: ResistanceConfig,
    pub(super) excitation: CurrentInputExcitation,
    pub(super) coefficients: ThermistorCoefficients,
    #[serde(
        default,
        deserialize_with = "optional_coupling",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) coupling: Option<Coupling>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ThermistorVoltageChannel {
    pub(super) physical: String,
    pub(super) min_temperature: f64,
    pub(super) max_temperature: f64,
    pub(super) units: TemperatureUnits,
    pub(super) resistance_config: ResistanceConfig,
    pub(super) excitation: VoltageExcitation,
    pub(super) coefficients: ThermistorCoefficients,
    pub(super) r1_ohms: f64,
    #[serde(
        default,
        deserialize_with = "optional_coupling",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) coupling: Option<Coupling>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ForceBridgeChannel {
    pub(super) physical: String,
    pub(super) min_force: f64,
    pub(super) max_force: f64,
    pub(super) units: ForceUnits,
    pub(super) bridge_config: BridgeConfig,
    pub(super) excitation: VoltageExcitation,
    pub(super) nominal_bridge_ohms: f64,
    pub(super) calibration: BridgeCalibration<ForceUnits>,
    #[serde(
        default,
        deserialize_with = "optional_coupling",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) coupling: Option<Coupling>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PressureBridgeChannel {
    pub(super) physical: String,
    pub(super) min_pressure: f64,
    pub(super) max_pressure: f64,
    pub(super) units: PressureUnits,
    pub(super) bridge_config: BridgeConfig,
    pub(super) excitation: VoltageExcitation,
    pub(super) nominal_bridge_ohms: f64,
    pub(super) calibration: BridgeCalibration<PressureUnits>,
    #[serde(
        default,
        deserialize_with = "optional_coupling",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) coupling: Option<Coupling>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TorqueBridgeChannel {
    pub(super) physical: String,
    pub(super) min_torque: f64,
    pub(super) max_torque: f64,
    pub(super) units: TorqueUnits,
    pub(super) bridge_config: BridgeConfig,
    pub(super) excitation: VoltageExcitation,
    pub(super) nominal_bridge_ohms: f64,
    pub(super) calibration: BridgeCalibration<TorqueUnits>,
    #[serde(
        default,
        deserialize_with = "optional_coupling",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) coupling: Option<Coupling>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PeriodMethod {
    #[serde(rename = "low_frequency_one_counter")]
    LowOne,
    #[serde(rename = "high_frequency_two_counter")]
    HighTwo,
    #[serde(rename = "large_range_two_counter")]
    LargeTwo,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CounterPeriodChannel {
    pub(super) physical: String,
    pub(super) min_seconds: f64,
    pub(super) max_seconds: f64,
    pub(super) edge: Edge,
    pub(super) method: PeriodMethod,
    pub(super) measurement_time_seconds: f64,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(super) divisor: u32,
    #[serde(
        default,
        deserialize_with = "optional_input_terminal",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) input_terminal: Option<String>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CounterPulseWidthChannel {
    pub(super) physical: String,
    pub(super) min_seconds: f64,
    pub(super) max_seconds: f64,
    pub(super) starting_edge: Edge,
    #[serde(
        default,
        deserialize_with = "optional_input_terminal",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) input_terminal: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum EncoderDecoding {
    X1,
    X2,
    X4,
    TwoPulseCounting,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ZIndexPhase {
    #[serde(rename = "a_high_b_high")]
    HighHigh,
    #[serde(rename = "a_high_b_low")]
    HighLow,
    #[serde(rename = "a_low_b_high")]
    LowHigh,
    #[serde(rename = "a_low_b_low")]
    LowLow,
}
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ZIndex {
    pub(super) enabled: bool,
    pub(super) value: f64,
    pub(super) phase: ZIndexPhase,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum AngularUnits {
    Degrees,
    Radians,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum LinearUnits {
    Meters,
    Inches,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CounterAngularPositionChannel {
    pub(super) physical: String,
    pub(super) decoding: EncoderDecoding,
    pub(super) units: AngularUnits,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(super) pulses_per_rev: u32,
    pub(super) initial_angle: f64,
    pub(super) z_index: ZIndex,
    #[serde(
        default,
        deserialize_with = "optional_input_terminal",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) a_input_terminal: Option<String>,
    #[serde(
        default,
        deserialize_with = "optional_input_terminal",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) b_input_terminal: Option<String>,
    #[serde(
        default,
        deserialize_with = "optional_input_terminal",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) z_input_terminal: Option<String>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CounterLinearPositionChannel {
    pub(super) physical: String,
    pub(super) decoding: EncoderDecoding,
    pub(super) units: LinearUnits,
    pub(super) distance_per_pulse: f64,
    pub(super) initial_position: f64,
    pub(super) z_index: ZIndex,
    #[serde(
        default,
        deserialize_with = "optional_input_terminal",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) a_input_terminal: Option<String>,
    #[serde(
        default,
        deserialize_with = "optional_input_terminal",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) b_input_terminal: Option<String>,
    #[serde(
        default,
        deserialize_with = "optional_input_terminal",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) z_input_terminal: Option<String>,
}

// These new records capture strict JSON before selected typed parsing. In particular,
// neither a channel nor its excitation can be admitted as a positional array.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub(super) enum VelocityUnits {
    #[serde(rename = "m_s")]
    Meters,
    #[serde(rename = "in_s")]
    Inches,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub(super) enum VelocitySensitivityUnits {
    #[serde(rename = "mv_per_mm_s")]
    Millimeters,
    #[serde(rename = "mv_per_in_s")]
    Inches,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub(super) enum ForceSensitivityUnits {
    #[serde(rename = "mv_per_newton")]
    Newtons,
    #[serde(rename = "mv_per_pound")]
    Pounds,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub(super) enum ChargeSensitivityUnits {
    #[serde(rename = "pc_per_g")]
    G,
    #[serde(rename = "pc_per_m_s2")]
    MetersPerSecondSquared,
    #[serde(rename = "pc_per_in_s2")]
    InchesPerSecondSquared,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub(super) enum LvdtSensitivityUnits {
    #[serde(rename = "mv_per_v_per_mm")]
    Millimeters,
    #[serde(rename = "mv_per_v_per_milli_inch")]
    ThousandthsOfInch,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub(super) enum RvdtSensitivityUnits {
    #[serde(rename = "mv_per_v_per_degree")]
    Degrees,
    #[serde(rename = "mv_per_v_per_radian")]
    Radians,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub(super) enum AcWireMode {
    #[serde(rename = "four_wire")]
    Four,
    #[serde(rename = "five_wire")]
    Five,
    #[serde(rename = "six_wire")]
    Six,
}
macro_rules! closed_channel_record {
    ($(#[$attr:meta])* $name:ident { $($(#[$field_attr:meta])* $field:ident: $ty:ty),* $(,)? }; nested[$($nested:literal),*]) => {
        $(#[$attr])*
        pub(super) struct $name {
            $($(#[$field_attr])* pub(super) $field: $ty),*
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Raw { $($(#[$field_attr])* $field: $ty),* }
                let value = crate::session::strict_json_value(deserializer)?;
                let object = value.as_object().ok_or_else(|| D::Error::custom("NI-DAQmx channel/excitation requires an object"))?;
                $(if object.get($nested).is_some_and(|field| !field.is_object()) {
                    return Err(D::Error::custom(concat!("NI-DAQmx ", $nested, " requires an object")));
                })*
                let _ = object;
                let raw: Raw = serde_json::from_value(value).map_err(D::Error::custom)?;
                Ok(Self { $($field: raw.$field),* })
            }
        }
    };
}
closed_channel_record! {
    #[derive(Debug, Clone, Copy, Serialize)]
    AcVoltageExcitation { source: ExcitationSource, volts: f64, frequency_hz: f64 }; nested[]
}
closed_channel_record! {
    #[derive(Debug, Clone, Serialize)]
    VelocityIepeChannel {
        physical: String,
        terminal_config: TerminalConfig,
        min_velocity: f64,
        max_velocity: f64,
        units: VelocityUnits,
        sensitivity: f64,
        sensitivity_units: VelocitySensitivityUnits,
        excitation: CurrentInputExcitation,
        #[serde(default, deserialize_with = "optional_coupling", skip_serializing_if = "Option::is_none")]
        coupling: Option<Coupling>,
    }; nested["excitation"]
}
closed_channel_record! {
    #[derive(Debug, Clone, Serialize)]
    ForceIepeChannel {
        physical: String,
        terminal_config: TerminalConfig,
        min_force: f64,
        max_force: f64,
        units: ForceUnits,
        sensitivity: f64,
        sensitivity_units: ForceSensitivityUnits,
        excitation: CurrentInputExcitation,
        #[serde(default, deserialize_with = "optional_coupling", skip_serializing_if = "Option::is_none")]
        coupling: Option<Coupling>,
    }; nested["excitation"]
}
closed_channel_record! {
    #[derive(Debug, Clone, Serialize)]
    ChargeAccelerationChannel {
        physical: String,
        terminal_config: TerminalConfig,
        min_acceleration: f64,
        max_acceleration: f64,
        units: AccelerationUnits,
        sensitivity: f64,
        sensitivity_units: ChargeSensitivityUnits,
        #[serde(default, deserialize_with = "optional_coupling", skip_serializing_if = "Option::is_none")]
        coupling: Option<Coupling>,
    }; nested[]
}
closed_channel_record! {
    #[derive(Debug, Clone, Serialize)]
    LvdtChannel {
        physical: String,
        min_position: f64,
        max_position: f64,
        units: LinearUnits,
        sensitivity: f64,
        sensitivity_units: LvdtSensitivityUnits,
        excitation: AcVoltageExcitation,
        ac_wire_mode: AcWireMode,
        #[serde(default, deserialize_with = "optional_coupling", skip_serializing_if = "Option::is_none")]
        coupling: Option<Coupling>,
    }; nested["excitation"]
}
closed_channel_record! {
    #[derive(Debug, Clone, Serialize)]
    RvdtChannel {
        physical: String,
        min_angle: f64,
        max_angle: f64,
        units: AngularUnits,
        sensitivity: f64,
        sensitivity_units: RvdtSensitivityUnits,
        excitation: AcVoltageExcitation,
        ac_wire_mode: AcWireMode,
        #[serde(default, deserialize_with = "optional_coupling", skip_serializing_if = "Option::is_none")]
        coupling: Option<Coupling>,
    }; nested["excitation"]
}

fn finite_range(min: f64, max: f64) -> bool {
    min.is_finite() && max.is_finite() && min < max
}
fn positive(value: f64) -> bool {
    value.is_finite() && value > 0.0
}
fn nonnegative(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}
fn valid_excitation(source: ExcitationSource, value: f64) -> bool {
    if source == ExcitationSource::None {
        nonnegative(value)
    } else {
        positive(value)
    }
}
impl StrainChannel {
    fn valid(&self) -> bool {
        finite_range(self.min_strain, self.max_strain)
            && valid_excitation(self.excitation.source, self.excitation.volts)
            && positive(self.gage_factor)
            && self.initial_bridge_volts.is_finite()
            && positive(self.nominal_gage_ohms)
            && self.poisson_ratio.is_finite()
            && nonnegative(self.lead_wire_ohms)
    }
}
impl BridgeRatioChannel {
    fn valid(&self) -> bool {
        finite_range(self.min_ratio, self.max_ratio)
            && valid_excitation(self.excitation.source, self.excitation.volts)
            && positive(self.nominal_bridge_ohms)
    }
}
impl AccelerationChannel {
    fn valid(&self) -> bool {
        finite_range(self.min_acceleration, self.max_acceleration)
            && positive(self.sensitivity)
            && valid_excitation(self.excitation.source, self.excitation.amperes)
    }
}
impl VelocityIepeChannel {
    fn valid(&self) -> bool {
        finite_range(self.min_velocity, self.max_velocity)
            && self.sensitivity.is_finite()
            && valid_excitation(self.excitation.source, self.excitation.amperes)
    }
}
impl ForceIepeChannel {
    fn valid(&self) -> bool {
        finite_range(self.min_force, self.max_force)
            && self.sensitivity.is_finite()
            && valid_excitation(self.excitation.source, self.excitation.amperes)
    }
}
impl ChargeAccelerationChannel {
    fn valid(&self) -> bool {
        finite_range(self.min_acceleration, self.max_acceleration) && self.sensitivity.is_finite()
    }
}
impl LvdtChannel {
    fn valid(&self) -> bool {
        finite_range(self.min_position, self.max_position)
            && self.sensitivity.is_finite()
            && valid_excitation(self.excitation.source, self.excitation.volts)
            && positive(self.excitation.frequency_hz)
    }
}
impl RvdtChannel {
    fn valid(&self) -> bool {
        finite_range(self.min_angle, self.max_angle)
            && self.sensitivity.is_finite()
            && valid_excitation(self.excitation.source, self.excitation.volts)
            && positive(self.excitation.frequency_hz)
    }
}
impl ThermistorCoefficients {
    fn valid(&self) -> bool {
        self.a.is_finite() && self.b.is_finite() && self.c.is_finite()
    }
    #[cfg(any(all(feature = "ni-daqmx", any(target_os = "linux", windows)), test))]
    fn same_request(&self, other: &Self) -> bool {
        self.a.to_bits() == other.a.to_bits()
            && self.b.to_bits() == other.b.to_bits()
            && self.c.to_bits() == other.c.to_bits()
    }
}
impl CalibrationPoint {
    fn valid(&self) -> bool {
        self.electrical.is_finite() && self.physical.is_finite()
    }
    #[cfg(any(all(feature = "ni-daqmx", any(target_os = "linux", windows)), test))]
    fn same_request(&self, other: &Self) -> bool {
        self.electrical.to_bits() == other.electrical.to_bits()
            && self.physical.to_bits() == other.physical.to_bits()
    }
}
impl<Units> BridgeCalibration<Units> {
    fn valid(&self) -> bool {
        self.first.valid() && self.second.valid() && self.first.electrical != self.second.electrical
    }
}
impl<Units: PartialEq> BridgeCalibration<Units> {
    #[cfg(any(all(feature = "ni-daqmx", any(target_os = "linux", windows)), test))]
    fn same_request(&self, other: &Self) -> bool {
        self.electrical_units == other.electrical_units
            && self.physical_units == other.physical_units
            && self.first.same_request(&other.first)
            && self.second.same_request(&other.second)
    }
}
impl ResistanceChannel {
    fn valid(&self) -> bool {
        finite_range(self.min_ohms, self.max_ohms)
            && valid_excitation(self.excitation.source, self.excitation.amperes)
    }
}
impl ThermistorCurrentChannel {
    fn valid(&self) -> bool {
        finite_range(self.min_temperature, self.max_temperature)
            && self.coefficients.valid()
            && valid_excitation(self.excitation.source, self.excitation.amperes)
    }
}
impl ThermistorVoltageChannel {
    fn valid(&self) -> bool {
        finite_range(self.min_temperature, self.max_temperature)
            && self.coefficients.valid()
            && positive(self.r1_ohms)
            && valid_excitation(self.excitation.source, self.excitation.volts)
    }
}
impl ForceBridgeChannel {
    fn valid(&self) -> bool {
        finite_range(self.min_force, self.max_force)
            && positive(self.nominal_bridge_ohms)
            && self.calibration.valid()
            && valid_excitation(self.excitation.source, self.excitation.volts)
    }
}
impl PressureBridgeChannel {
    fn valid(&self) -> bool {
        finite_range(self.min_pressure, self.max_pressure)
            && positive(self.nominal_bridge_ohms)
            && self.calibration.valid()
            && valid_excitation(self.excitation.source, self.excitation.volts)
    }
}
impl TorqueBridgeChannel {
    fn valid(&self) -> bool {
        finite_range(self.min_torque, self.max_torque)
            && positive(self.nominal_bridge_ohms)
            && self.calibration.valid()
            && valid_excitation(self.excitation.source, self.excitation.volts)
    }
}
impl CounterPeriodChannel {
    fn valid(&self) -> bool {
        finite_range(self.min_seconds, self.max_seconds)
            && self.min_seconds >= 0.0
            && nonnegative(self.measurement_time_seconds)
            && valid_input_terminal(self.input_terminal.as_deref())
    }
}
impl CounterPulseWidthChannel {
    fn valid(&self) -> bool {
        finite_range(self.min_seconds, self.max_seconds)
            && self.min_seconds >= 0.0
            && valid_input_terminal(self.input_terminal.as_deref())
    }
}
fn valid_encoder(z: ZIndex, a: Option<&str>, b: Option<&str>, input_z: Option<&str>) -> bool {
    z.value.is_finite()
        && (z.enabled || input_z.is_none())
        && [a, b, input_z].into_iter().all(valid_input_terminal)
}
impl CounterAngularPositionChannel {
    fn valid(&self) -> bool {
        self.pulses_per_rev > 0
            && self.initial_angle.is_finite()
            && valid_encoder(
                self.z_index,
                self.a_input_terminal.as_deref(),
                self.b_input_terminal.as_deref(),
                self.z_input_terminal.as_deref(),
            )
    }
}
impl CounterLinearPositionChannel {
    fn valid(&self) -> bool {
        positive(self.distance_per_pulse)
            && self.initial_position.is_finite()
            && valid_encoder(
                self.z_index,
                self.a_input_terminal.as_deref(),
                self.b_input_terminal.as_deref(),
                self.z_input_terminal.as_deref(),
            )
    }
}

macro_rules! request_identity {
 ($type:ty; equal[$first:ident $(,$eq:ident)*]; bits[$($bits:ident),*]; nested[$($nested:ident),*]) => {
  impl $type {
   #[cfg(any(all(feature="ni-daqmx",any(target_os="linux",windows)),test))]
   fn same_request(&self,other:&Self)->bool {
    self.$first==other.$first $(&& self.$eq==other.$eq)* $(&& self.$bits.to_bits()==other.$bits.to_bits())* $(&& self.$nested.same_request(&other.$nested))*
   }
  }
 };
}
request_identity!(AcVoltageExcitation; equal[source]; bits[volts,frequency_hz]; nested[]);
request_identity!(VelocityIepeChannel; equal[physical,terminal_config,units,sensitivity_units,coupling]; bits[min_velocity,max_velocity,sensitivity]; nested[excitation]);
request_identity!(ForceIepeChannel; equal[physical,terminal_config,units,sensitivity_units,coupling]; bits[min_force,max_force,sensitivity]; nested[excitation]);
request_identity!(ChargeAccelerationChannel; equal[physical,terminal_config,units,sensitivity_units,coupling]; bits[min_acceleration,max_acceleration,sensitivity]; nested[]);
request_identity!(LvdtChannel; equal[physical,units,sensitivity_units,coupling,ac_wire_mode]; bits[min_position,max_position,sensitivity]; nested[excitation]);
request_identity!(RvdtChannel; equal[physical,units,sensitivity_units,coupling,ac_wire_mode]; bits[min_angle,max_angle,sensitivity]; nested[excitation]);
request_identity!(VoltageExcitation; equal[source]; bits[volts]; nested[]);
request_identity!(AccelerationExcitation; equal[source]; bits[amperes]; nested[]);
request_identity!(CurrentInputExcitation; equal[source]; bits[amperes]; nested[]);
request_identity!(ResistanceChannel; equal[physical,resistance_config,coupling]; bits[min_ohms,max_ohms]; nested[excitation]);
request_identity!(ThermistorCurrentChannel; equal[physical,units,resistance_config,coupling]; bits[min_temperature,max_temperature]; nested[excitation,coefficients]);
request_identity!(ThermistorVoltageChannel; equal[physical,units,resistance_config,coupling]; bits[min_temperature,max_temperature,r1_ohms]; nested[excitation,coefficients]);
request_identity!(ForceBridgeChannel; equal[physical,units,bridge_config,coupling]; bits[min_force,max_force,nominal_bridge_ohms]; nested[excitation,calibration]);
request_identity!(PressureBridgeChannel; equal[physical,units,bridge_config,coupling]; bits[min_pressure,max_pressure,nominal_bridge_ohms]; nested[excitation,calibration]);
request_identity!(TorqueBridgeChannel; equal[physical,units,bridge_config,coupling]; bits[min_torque,max_torque,nominal_bridge_ohms]; nested[excitation,calibration]);
request_identity!(ZIndex; equal[enabled,phase]; bits[value]; nested[]);
request_identity!(StrainChannel; equal[physical,bridge_type,coupling]; bits[min_strain,max_strain,gage_factor,initial_bridge_volts,nominal_gage_ohms,poisson_ratio,lead_wire_ohms]; nested[excitation]);
request_identity!(BridgeRatioChannel; equal[physical,units,bridge_config,coupling]; bits[min_ratio,max_ratio,nominal_bridge_ohms]; nested[excitation]);
request_identity!(AccelerationChannel; equal[physical,terminal_config,units,sensitivity_units,coupling]; bits[min_acceleration,max_acceleration,sensitivity]; nested[excitation]);
request_identity!(CounterPeriodChannel; equal[physical,edge,method,divisor,input_terminal]; bits[min_seconds,max_seconds,measurement_time_seconds]; nested[]);
request_identity!(CounterPulseWidthChannel; equal[physical,starting_edge,input_terminal]; bits[min_seconds,max_seconds]; nested[]);
request_identity!(CounterAngularPositionChannel; equal[physical,decoding,units,pulses_per_rev,a_input_terminal,b_input_terminal,z_input_terminal]; bits[initial_angle]; nested[z_index]);
request_identity!(CounterLinearPositionChannel; equal[physical,decoding,units,a_input_terminal,b_input_terminal,z_input_terminal]; bits[distance_per_pulse,initial_position]; nested[z_index]);

fn valid_input_terminal(terminal: Option<&str>) -> bool {
    terminal.is_none_or(|name| {
        !name.is_empty()
            && name.len() <= 256
            && name.is_ascii()
            && name
                .strip_prefix('/')
                .unwrap_or(name)
                .split('/')
                .all(|part| {
                    !part.is_empty()
                        && part
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
                })
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "measurement", rename_all = "snake_case")]
pub(super) enum MixedChannel {
    Voltage(VoltageChannel),
    Thermocouple(ThermocoupleChannel),
    Current(CurrentChannel),
    Rtd(RtdChannel),
    Strain(StrainChannel),
    BridgeRatio(BridgeRatioChannel),
    Acceleration(AccelerationChannel),
    Resistance(ResistanceChannel),
    ThermistorCurrent(ThermistorCurrentChannel),
    ThermistorVoltage(ThermistorVoltageChannel),
    ForceBridge(ForceBridgeChannel),
    PressureBridge(PressureBridgeChannel),
    VelocityIepe(VelocityIepeChannel),
    ForceIepe(ForceIepeChannel),
    ChargeAcceleration(ChargeAccelerationChannel),
    Lvdt(LvdtChannel),
    Rvdt(RvdtChannel),
    TorqueBridge(TorqueBridgeChannel),
}
impl<'de> Deserialize<'de> for MixedChannel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let mut value = crate::session::strict_json_value(deserializer)?;
        let tag = value
            .as_object_mut()
            .and_then(|object| object.remove("measurement"))
            .ok_or_else(|| {
                D::Error::custom("NI-DAQmx mixed channels require an explicit measurement tag")
            })?;
        let kind = serde_json::from_value::<Kind>(tag).map_err(D::Error::custom)?;
        match kind {
            Kind::Voltage => serde_json::from_value(value).map(Self::Voltage),
            Kind::Thermocouple => serde_json::from_value(value).map(Self::Thermocouple),
            Kind::Current => serde_json::from_value(value).map(Self::Current),
            Kind::Rtd => serde_json::from_value(value).map(Self::Rtd),
            Kind::Strain => serde_json::from_value(value).map(Self::Strain),
            Kind::BridgeRatio => serde_json::from_value(value).map(Self::BridgeRatio),
            Kind::Acceleration => serde_json::from_value(value).map(Self::Acceleration),
            Kind::Resistance => serde_json::from_value(value).map(Self::Resistance),
            Kind::ThermistorCurrent => serde_json::from_value(value).map(Self::ThermistorCurrent),
            Kind::ThermistorVoltage => serde_json::from_value(value).map(Self::ThermistorVoltage),
            Kind::ForceBridge => serde_json::from_value(value).map(Self::ForceBridge),
            Kind::PressureBridge => serde_json::from_value(value).map(Self::PressureBridge),
            Kind::VelocityIepe => serde_json::from_value(value).map(Self::VelocityIepe),
            Kind::ForceIepe => serde_json::from_value(value).map(Self::ForceIepe),
            Kind::ChargeAcceleration => serde_json::from_value(value).map(Self::ChargeAcceleration),
            Kind::Lvdt => serde_json::from_value(value).map(Self::Lvdt),
            Kind::Rvdt => serde_json::from_value(value).map(Self::Rvdt),
            Kind::TorqueBridge => serde_json::from_value(value).map(Self::TorqueBridge),

            Kind::Mixed => {
                return Err(D::Error::custom(
                    "NI-DAQmx mixed channels cannot contain another mixed profile",
                ));
            }
            Kind::DigitalPort
            | Kind::CounterCount
            | Kind::CounterFrequency
            | Kind::CounterPeriod
            | Kind::CounterPulseWidth
            | Kind::CounterAngularPosition
            | Kind::CounterLinearPosition => {
                return Err(D::Error::custom(
                    "NI-DAQmx mixed channels require an analog input family",
                ));
            }
        }
        .map_err(D::Error::custom)
    }
}
impl MixedChannel {
    pub(super) fn physical(&self) -> &str {
        match self {
            Self::Voltage(c) => &c.physical,
            Self::Thermocouple(c) => &c.physical,
            Self::Current(c) => &c.physical,
            Self::Rtd(c) => &c.physical,
            Self::Strain(c) => &c.physical,
            Self::BridgeRatio(c) => &c.physical,
            Self::Acceleration(c) => &c.physical,
            Self::Resistance(c) => &c.physical,
            Self::ThermistorCurrent(c) => &c.physical,
            Self::ThermistorVoltage(c) => &c.physical,
            Self::ForceBridge(c) => &c.physical,
            Self::PressureBridge(c) => &c.physical,
            Self::VelocityIepe(c) => &c.physical,
            Self::ForceIepe(c) => &c.physical,
            Self::ChargeAcceleration(c) => &c.physical,
            Self::Lvdt(c) => &c.physical,
            Self::Rvdt(c) => &c.physical,
            Self::TorqueBridge(c) => &c.physical,
        }
    }
    fn valid(&self) -> bool {
        let range = |min: f64, max: f64| min.is_finite() && max.is_finite() && min < max;
        match self {
            Self::Strain(c) => c.valid(),
            Self::BridgeRatio(c) => c.valid(),
            Self::Acceleration(c) => c.valid(),
            Self::Resistance(c) => c.valid(),
            Self::ThermistorCurrent(c) => c.valid(),
            Self::ThermistorVoltage(c) => c.valid(),
            Self::ForceBridge(c) => c.valid(),
            Self::PressureBridge(c) => c.valid(),
            Self::VelocityIepe(c) => c.valid(),
            Self::ForceIepe(c) => c.valid(),
            Self::ChargeAcceleration(c) => c.valid(),
            Self::Lvdt(c) => c.valid(),
            Self::Rvdt(c) => c.valid(),
            Self::TorqueBridge(c) => c.valid(),
            Self::Voltage(c) => range(c.min_volts, c.max_volts),
            Self::Thermocouple(c) => {
                range(c.min_temperature, c.max_temperature)
                    && !matches!(c.cjc, ColdJunction::Constant { value } if !value.is_finite())
            }
            Self::Current(c) => {
                range(c.min_amperes, c.max_amperes)
                    && !matches!(c.shunt, CurrentShunt::External { ohms } if !ohms.is_finite() || ohms <= 0.0)
            }
            Self::Rtd(c) => {
                let amperes = match c.excitation {
                    CurrentExcitation::Internal { amperes }
                    | CurrentExcitation::External { amperes } => amperes,
                };
                range(c.min_temperature, c.max_temperature)
                    && c.r0_ohms.is_finite()
                    && c.r0_ohms > 0.0
                    && amperes.is_finite()
                    && amperes > 0.0
            }
        }
    }
    #[cfg(any(all(feature = "ni-daqmx", any(target_os = "linux", windows)), test))]
    pub(super) fn finite_key(&self) -> &'static str {
        match self {
            Self::Voltage(_) => "volts",
            Self::Thermocouple(_)
            | Self::Rtd(_)
            | Self::ThermistorCurrent(_)
            | Self::ThermistorVoltage(_) => "temperature",
            Self::Current(_) => "amperes",
            Self::Strain(_) => "strain",
            Self::BridgeRatio(_) => "ratio",
            Self::Acceleration(_) | Self::ChargeAcceleration(_) => "acceleration",
            Self::Resistance(_) => "ohms",
            Self::ForceBridge(_) | Self::ForceIepe(_) => "force",
            Self::PressureBridge(_) => "pressure",
            Self::VelocityIepe(_) => "velocity",
            Self::Lvdt(_) => "position",
            Self::Rvdt(_) => "angle",
            Self::TorqueBridge(_) => "torque",
        }
    }
    #[cfg(any(all(feature = "ni-daqmx", any(target_os = "linux", windows)), test))]
    fn same_request(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Voltage(a), Self::Voltage(b)) => {
                a.physical == b.physical
                    && a.terminal_config == b.terminal_config
                    && a.min_volts.to_bits() == b.min_volts.to_bits()
                    && a.max_volts.to_bits() == b.max_volts.to_bits()
            }
            (Self::Thermocouple(a), Self::Thermocouple(b)) => {
                let cjc = match (a.cjc, b.cjc) {
                    (ColdJunction::BuiltIn, ColdJunction::BuiltIn) => true,
                    (ColdJunction::Constant { value: a }, ColdJunction::Constant { value: b }) => {
                        a.to_bits() == b.to_bits()
                    }
                    _ => false,
                };
                a.physical == b.physical
                    && a.thermocouple_type == b.thermocouple_type
                    && a.units == b.units
                    && a.min_temperature.to_bits() == b.min_temperature.to_bits()
                    && a.max_temperature.to_bits() == b.max_temperature.to_bits()
                    && a.terminal_config == b.terminal_config
                    && cjc
            }
            (Self::Current(a), Self::Current(b)) => {
                let shunt = match (a.shunt, b.shunt) {
                    (CurrentShunt::BuiltIn, CurrentShunt::BuiltIn) => true,
                    (CurrentShunt::External { ohms: a }, CurrentShunt::External { ohms: b }) => {
                        a.to_bits() == b.to_bits()
                    }
                    _ => false,
                };
                a.physical == b.physical
                    && a.terminal_config == b.terminal_config
                    && a.min_amperes.to_bits() == b.min_amperes.to_bits()
                    && a.max_amperes.to_bits() == b.max_amperes.to_bits()
                    && shunt
            }
            (Self::Rtd(a), Self::Rtd(b)) => {
                let excitation = match (a.excitation, b.excitation) {
                    (
                        CurrentExcitation::Internal { amperes: a },
                        CurrentExcitation::Internal { amperes: b },
                    )
                    | (
                        CurrentExcitation::External { amperes: a },
                        CurrentExcitation::External { amperes: b },
                    ) => a.to_bits() == b.to_bits(),
                    _ => false,
                };
                a.physical == b.physical
                    && a.rtd_type == b.rtd_type
                    && a.units == b.units
                    && a.min_temperature.to_bits() == b.min_temperature.to_bits()
                    && a.max_temperature.to_bits() == b.max_temperature.to_bits()
                    && a.resistance_config == b.resistance_config
                    && a.r0_ohms.to_bits() == b.r0_ohms.to_bits()
                    && excitation
            }
            (Self::Strain(a), Self::Strain(b)) => a.same_request(b),
            (Self::BridgeRatio(a), Self::BridgeRatio(b)) => a.same_request(b),
            (Self::Acceleration(a), Self::Acceleration(b)) => a.same_request(b),
            (Self::Resistance(a), Self::Resistance(b)) => a.same_request(b),
            (Self::ThermistorCurrent(a), Self::ThermistorCurrent(b)) => a.same_request(b),
            (Self::ThermistorVoltage(a), Self::ThermistorVoltage(b)) => a.same_request(b),
            (Self::ForceBridge(a), Self::ForceBridge(b)) => a.same_request(b),
            (Self::PressureBridge(a), Self::PressureBridge(b)) => a.same_request(b),
            (Self::VelocityIepe(a), Self::VelocityIepe(b)) => a.same_request(b),
            (Self::ForceIepe(a), Self::ForceIepe(b)) => a.same_request(b),
            (Self::ChargeAcceleration(a), Self::ChargeAcceleration(b)) => a.same_request(b),
            (Self::Lvdt(a), Self::Lvdt(b)) => a.same_request(b),
            (Self::Rvdt(a), Self::Rvdt(b)) => a.same_request(b),
            (Self::TorqueBridge(a), Self::TorqueBridge(b)) => a.same_request(b),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "measurement", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Measurement {
    Voltage {
        channels: Vec<String>,
        terminal_config: TerminalConfig,
        min_volts: f64,
        max_volts: f64,
    },
    Thermocouple {
        channels: Vec<ThermocoupleChannel>,
    },
    Current {
        channels: Vec<CurrentChannel>,
    },
    Rtd {
        channels: Vec<RtdChannel>,
    },
    Mixed {
        channels: Vec<MixedChannel>,
    },
    DigitalPort {
        channels: Vec<String>,
    },
    CounterCount {
        channels: Vec<CounterCountChannel>,
    },
    CounterFrequency {
        channels: Vec<CounterFrequencyChannel>,
    },
    Strain {
        channels: Vec<StrainChannel>,
    },
    BridgeRatio {
        channels: Vec<BridgeRatioChannel>,
    },
    Acceleration {
        channels: Vec<AccelerationChannel>,
    },
    Resistance {
        channels: Vec<ResistanceChannel>,
    },
    ThermistorCurrent {
        channels: Vec<ThermistorCurrentChannel>,
    },
    ThermistorVoltage {
        channels: Vec<ThermistorVoltageChannel>,
    },
    ForceBridge {
        channels: Vec<ForceBridgeChannel>,
    },
    PressureBridge {
        channels: Vec<PressureBridgeChannel>,
    },
    VelocityIepe {
        channels: Vec<VelocityIepeChannel>,
    },
    ForceIepe {
        channels: Vec<ForceIepeChannel>,
    },
    ChargeAcceleration {
        channels: Vec<ChargeAccelerationChannel>,
    },
    Lvdt {
        channels: Vec<LvdtChannel>,
    },
    Rvdt {
        channels: Vec<RvdtChannel>,
    },
    TorqueBridge {
        channels: Vec<TorqueBridgeChannel>,
    },
    CounterPeriod {
        channels: Vec<CounterPeriodChannel>,
    },
    CounterPulseWidth {
        channels: Vec<CounterPulseWidthChannel>,
    },
    CounterAngularPosition {
        channels: Vec<CounterAngularPositionChannel>,
    },
    CounterLinearPosition {
        channels: Vec<CounterLinearPositionChannel>,
    },
}
impl<'de> Deserialize<'de> for Measurement {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input {
            measurement: Kind,
            #[serde(deserialize_with = "crate::session::strict_json_value")]
            channels: serde_json::Value,
            #[serde(default)]
            terminal_config: Field<TerminalConfig>,
            #[serde(default)]
            min_volts: Field<f64>,
            #[serde(default)]
            max_volts: Field<f64>,
        }
        let input = Input::deserialize(deserializer)?;
        measurement(
            input.measurement,
            input.channels,
            input.terminal_config,
            input.min_volts,
            input.max_volts,
        )
        .map_err(D::Error::custom)
    }
}

impl Measurement {
    pub(super) fn channel_count(&self) -> usize {
        match self {
            Self::Voltage { channels, .. } | Self::DigitalPort { channels } => channels.len(),
            Self::Thermocouple { channels } => channels.len(),
            Self::Current { channels } => channels.len(),
            Self::Rtd { channels } => channels.len(),
            Self::Mixed { channels } => channels.len(),
            Self::CounterCount { channels } => channels.len(),
            Self::CounterFrequency { channels } => channels.len(),
            Self::Strain { channels } => channels.len(),
            Self::BridgeRatio { channels } => channels.len(),
            Self::Acceleration { channels } => channels.len(),
            Self::Resistance { channels } => channels.len(),
            Self::ThermistorCurrent { channels } => channels.len(),
            Self::ThermistorVoltage { channels } => channels.len(),
            Self::ForceBridge { channels } => channels.len(),
            Self::PressureBridge { channels } => channels.len(),
            Self::VelocityIepe { channels } => channels.len(),
            Self::ForceIepe { channels } => channels.len(),
            Self::ChargeAcceleration { channels } => channels.len(),
            Self::Lvdt { channels } => channels.len(),
            Self::Rvdt { channels } => channels.len(),
            Self::TorqueBridge { channels } => channels.len(),
            Self::CounterPeriod { channels } => channels.len(),
            Self::CounterPulseWidth { channels } => channels.len(),
            Self::CounterAngularPosition { channels } => channels.len(),
            Self::CounterLinearPosition { channels } => channels.len(),
        }
    }

    pub(super) fn on_demand_only(&self) -> bool {
        matches!(
            self,
            Self::DigitalPort { .. }
                | Self::CounterCount { .. }
                | Self::CounterFrequency { .. }
                | Self::CounterPeriod { .. }
                | Self::CounterPulseWidth { .. }
                | Self::CounterAngularPosition { .. }
                | Self::CounterLinearPosition { .. }
        )
    }

    #[cfg(any(all(feature = "ni-daqmx", any(target_os = "linux", windows)), test))]
    pub(super) fn word_bytes(&self) -> usize {
        if matches!(self, Self::DigitalPort { .. } | Self::CounterCount { .. }) {
            4
        } else {
            8
        }
    }

    #[cfg(any(all(feature = "ni-daqmx", any(target_os = "linux", windows)), test))]
    pub(super) fn read_operation(&self) -> &'static str {
        match self {
            Self::DigitalPort { .. } => "ReadDigitalU32",
            Self::CounterCount { .. } => "ReadCounterU32",
            Self::CounterFrequency { .. }
            | Self::CounterPeriod { .. }
            | Self::CounterPulseWidth { .. }
            | Self::CounterAngularPosition { .. }
            | Self::CounterLinearPosition { .. } => "ReadCounterF64",
            _ => "ReadAnalogF64",
        }
    }

    #[cfg(any(all(feature = "ni-daqmx", any(target_os = "linux", windows)), test))]
    pub(super) fn same_request(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Voltage {
                    channels: a,
                    terminal_config: at,
                    min_volts: amin,
                    max_volts: amax,
                },
                Self::Voltage {
                    channels: b,
                    terminal_config: bt,
                    min_volts: bmin,
                    max_volts: bmax,
                },
            ) => {
                a == b
                    && at == bt
                    && amin.to_bits() == bmin.to_bits()
                    && amax.to_bits() == bmax.to_bits()
            }
            (Self::Thermocouple { channels: a }, Self::Thermocouple { channels: b }) => {
                a.len() == b.len()
                    && a.iter().zip(b).all(|(a, b)| {
                        let same_cjc = match (a.cjc, b.cjc) {
                            (ColdJunction::BuiltIn, ColdJunction::BuiltIn) => true,
                            (
                                ColdJunction::Constant { value: a },
                                ColdJunction::Constant { value: b },
                            ) => a.to_bits() == b.to_bits(),
                            _ => false,
                        };
                        a.physical == b.physical
                            && a.thermocouple_type == b.thermocouple_type
                            && a.units == b.units
                            && a.min_temperature.to_bits() == b.min_temperature.to_bits()
                            && a.max_temperature.to_bits() == b.max_temperature.to_bits()
                            && a.terminal_config == b.terminal_config
                            && same_cjc
                    })
            }
            (Self::Current { channels: a }, Self::Current { channels: b }) => {
                a.len() == b.len()
                    && a.iter().zip(b).all(|(a, b)| {
                        let same_shunt = match (a.shunt, b.shunt) {
                            (CurrentShunt::BuiltIn, CurrentShunt::BuiltIn) => true,
                            (
                                CurrentShunt::External { ohms: a },
                                CurrentShunt::External { ohms: b },
                            ) => a.to_bits() == b.to_bits(),
                            _ => false,
                        };
                        a.physical == b.physical
                            && a.terminal_config == b.terminal_config
                            && a.min_amperes.to_bits() == b.min_amperes.to_bits()
                            && a.max_amperes.to_bits() == b.max_amperes.to_bits()
                            && same_shunt
                    })
            }
            (Self::Rtd { channels: a }, Self::Rtd { channels: b }) => {
                a.len() == b.len()
                    && a.iter().zip(b).all(|(a, b)| {
                        let same_excitation = match (a.excitation, b.excitation) {
                            (
                                CurrentExcitation::Internal { amperes: a },
                                CurrentExcitation::Internal { amperes: b },
                            )
                            | (
                                CurrentExcitation::External { amperes: a },
                                CurrentExcitation::External { amperes: b },
                            ) => a.to_bits() == b.to_bits(),
                            _ => false,
                        };
                        a.physical == b.physical
                            && a.rtd_type == b.rtd_type
                            && a.units == b.units
                            && a.min_temperature.to_bits() == b.min_temperature.to_bits()
                            && a.max_temperature.to_bits() == b.max_temperature.to_bits()
                            && a.resistance_config == b.resistance_config
                            && a.r0_ohms.to_bits() == b.r0_ohms.to_bits()
                            && same_excitation
                    })
            }
            (Self::Mixed { channels: a }, Self::Mixed { channels: b }) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same_request(b))
            }
            (Self::DigitalPort { channels: a }, Self::DigitalPort { channels: b }) => a == b,
            (Self::CounterCount { channels: a }, Self::CounterCount { channels: b }) => a == b,
            (Self::CounterFrequency { channels: a }, Self::CounterFrequency { channels: b }) => {
                a.len() == b.len()
                    && a.iter().zip(b).all(|(a, b)| {
                        a.physical == b.physical
                            && a.edge == b.edge
                            && a.method == b.method
                            && a.min_hz.to_bits() == b.min_hz.to_bits()
                            && a.max_hz.to_bits() == b.max_hz.to_bits()
                            && a.measurement_time_seconds.to_bits()
                                == b.measurement_time_seconds.to_bits()
                            && a.divisor == b.divisor
                            && a.input_terminal == b.input_terminal
                    })
            }
            (Self::Strain { channels: a }, Self::Strain { channels: b }) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same_request(b))
            }
            (Self::BridgeRatio { channels: a }, Self::BridgeRatio { channels: b }) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same_request(b))
            }
            (Self::Acceleration { channels: a }, Self::Acceleration { channels: b }) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same_request(b))
            }
            (Self::Resistance { channels: a }, Self::Resistance { channels: b }) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same_request(b))
            }
            (Self::ThermistorCurrent { channels: a }, Self::ThermistorCurrent { channels: b }) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same_request(b))
            }
            (Self::ThermistorVoltage { channels: a }, Self::ThermistorVoltage { channels: b }) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same_request(b))
            }
            (Self::ForceBridge { channels: a }, Self::ForceBridge { channels: b }) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same_request(b))
            }
            (Self::PressureBridge { channels: a }, Self::PressureBridge { channels: b }) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same_request(b))
            }
            (Self::VelocityIepe { channels: a }, Self::VelocityIepe { channels: b }) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same_request(b))
            }
            (Self::ForceIepe { channels: a }, Self::ForceIepe { channels: b }) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same_request(b))
            }
            (
                Self::ChargeAcceleration { channels: a },
                Self::ChargeAcceleration { channels: b },
            ) => a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same_request(b)),
            (Self::Lvdt { channels: a }, Self::Lvdt { channels: b }) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same_request(b))
            }
            (Self::Rvdt { channels: a }, Self::Rvdt { channels: b }) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same_request(b))
            }
            (Self::TorqueBridge { channels: a }, Self::TorqueBridge { channels: b }) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same_request(b))
            }
            (Self::CounterPeriod { channels: a }, Self::CounterPeriod { channels: b }) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same_request(b))
            }
            (Self::CounterPulseWidth { channels: a }, Self::CounterPulseWidth { channels: b }) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same_request(b))
            }
            (
                Self::CounterAngularPosition { channels: a },
                Self::CounterAngularPosition { channels: b },
            ) => a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same_request(b)),
            (
                Self::CounterLinearPosition { channels: a },
                Self::CounterLinearPosition { channels: b },
            ) => a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same_request(b)),
            _ => false,
        }
    }

    fn valid_engineering(&self) -> bool {
        match self {
            Self::Strain { channels } => channels.iter().all(StrainChannel::valid),
            Self::BridgeRatio { channels } => channels.iter().all(BridgeRatioChannel::valid),
            Self::Acceleration { channels } => channels.iter().all(AccelerationChannel::valid),
            Self::Resistance { channels } => channels.iter().all(ResistanceChannel::valid),
            Self::ThermistorCurrent { channels } => {
                channels.iter().all(ThermistorCurrentChannel::valid)
            }
            Self::ThermistorVoltage { channels } => {
                channels.iter().all(ThermistorVoltageChannel::valid)
            }
            Self::ForceBridge { channels } => channels.iter().all(ForceBridgeChannel::valid),
            Self::PressureBridge { channels } => channels.iter().all(PressureBridgeChannel::valid),
            Self::VelocityIepe { channels } => channels.iter().all(VelocityIepeChannel::valid),
            Self::ForceIepe { channels } => channels.iter().all(ForceIepeChannel::valid),
            Self::ChargeAcceleration { channels } => {
                channels.iter().all(ChargeAccelerationChannel::valid)
            }
            Self::Lvdt { channels } => channels.iter().all(LvdtChannel::valid),
            Self::Rvdt { channels } => channels.iter().all(RvdtChannel::valid),
            Self::TorqueBridge { channels } => channels.iter().all(TorqueBridgeChannel::valid),
            Self::CounterPeriod { channels } => channels.iter().all(CounterPeriodChannel::valid),
            Self::CounterPulseWidth { channels } => {
                channels.iter().all(CounterPulseWidthChannel::valid)
            }
            Self::CounterAngularPosition { channels } => {
                channels.iter().all(CounterAngularPositionChannel::valid)
            }
            Self::CounterLinearPosition { channels } => {
                channels.iter().all(CounterLinearPositionChannel::valid)
            }
            _ => true,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Config {
    pub(super) signal: String,
    #[serde(flatten)]
    pub(super) measurement: Measurement,
    #[serde(flatten)]
    pub(super) timing: Timing,
    pub(super) cleanup_timeout_ms: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) library: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "timing", rename_all = "snake_case")]
pub(super) enum Timing {
    Continuous {
        rate_hz: f64,
        max_samples_per_channel: u32,
        host_buffer_samples_per_channel: u32,
    },
    OnDemand {
        poll_interval_ms: u64,
    },
}
impl Timing {
    #[cfg(any(all(feature = "ni-daqmx", any(target_os = "linux", windows)), test))]
    pub(super) fn poll_interval_ns(self) -> Option<u64> {
        match self {
            Self::Continuous { .. } => None,
            Self::OnDemand { poll_interval_ms } => Some(poll_interval_ms * 1_000_000),
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TimingKind {
    #[default]
    Continuous,
    OnDemand,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    #[default]
    Voltage,
    Thermocouple,
    Current,
    Rtd,
    Mixed,
    DigitalPort,
    CounterCount,
    CounterFrequency,
    Strain,
    BridgeRatio,
    Acceleration,
    Resistance,
    ThermistorCurrent,
    ThermistorVoltage,
    ForceBridge,
    PressureBridge,
    VelocityIepe,
    ForceIepe,
    ChargeAcceleration,
    Lvdt,
    Rvdt,
    TorqueBridge,
    CounterPeriod,
    CounterPulseWidth,
    CounterAngularPosition,
    CounterLinearPosition,
}

// An explicitly supplied null is not a missing voltage field, and voltage-only
// fields are rejected on the temperature branch even when supplied as null.
#[derive(Default)]
pub(super) enum Field<T> {
    #[default]
    Missing,
    Present(T),
}

pub(super) fn unsigned_field<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Field<u32>, D::Error> {
    crate::config_number::unsigned(deserializer).map(Field::Present)
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for Field<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self::Present)
    }
}

// Select a typed channel vector only after the explicit measurement tag is
// known. The captured JSON preserves decimal/exponent numbers and rejects
// duplicate keys before conversion, without Serde's untagged Content buffer.
fn measurement(
    kind: Kind,
    channels: serde_json::Value,
    terminal: Field<TerminalConfig>,
    min: Field<f64>,
    max: Field<f64>,
) -> std::result::Result<Measurement, String> {
    let parsed = match (kind, terminal, min, max) {
        (Kind::Voltage, Field::Present(terminal_config), Field::Present(min_volts), Field::Present(max_volts)) => {
            serde_json::from_value(channels).map(|channels| Measurement::Voltage { channels, terminal_config, min_volts, max_volts })
        }
        (Kind::Thermocouple, Field::Missing, Field::Missing, Field::Missing) => {
            serde_json::from_value(channels).map(|channels| Measurement::Thermocouple { channels })
        }
        (Kind::Current, Field::Missing, Field::Missing, Field::Missing) => {
            serde_json::from_value(channels).map(|channels| Measurement::Current { channels })
        }
        (Kind::Rtd, Field::Missing, Field::Missing, Field::Missing) => {
            serde_json::from_value(channels).map(|channels| Measurement::Rtd { channels })
        }
        (Kind::Mixed, Field::Missing, Field::Missing, Field::Missing) => {
            serde_json::from_value(channels).map(|channels| Measurement::Mixed { channels })
        }
        (Kind::DigitalPort, Field::Missing, Field::Missing, Field::Missing) => {
            serde_json::from_value(channels).map(|channels| Measurement::DigitalPort { channels })
        }
        (Kind::CounterCount, Field::Missing, Field::Missing, Field::Missing) => {
            serde_json::from_value(channels).map(|channels| Measurement::CounterCount { channels })
        }
        (Kind::CounterFrequency, Field::Missing, Field::Missing, Field::Missing) => {
            serde_json::from_value(channels).map(|channels| Measurement::CounterFrequency { channels })
        }
        (Kind::Strain, Field::Missing, Field::Missing, Field::Missing) => serde_json::from_value(channels).map(|channels| Measurement::Strain { channels }),
        (Kind::BridgeRatio, Field::Missing, Field::Missing, Field::Missing) => serde_json::from_value(channels).map(|channels| Measurement::BridgeRatio { channels }),
        (Kind::Acceleration, Field::Missing, Field::Missing, Field::Missing) => serde_json::from_value(channels).map(|channels| Measurement::Acceleration { channels }),
        (Kind::Resistance, Field::Missing, Field::Missing, Field::Missing) => serde_json::from_value(channels).map(|channels| Measurement::Resistance { channels }),
        (Kind::ThermistorCurrent, Field::Missing, Field::Missing, Field::Missing) => serde_json::from_value(channels).map(|channels| Measurement::ThermistorCurrent { channels }),
        (Kind::ThermistorVoltage, Field::Missing, Field::Missing, Field::Missing) => serde_json::from_value(channels).map(|channels| Measurement::ThermistorVoltage { channels }),
        (Kind::ForceBridge, Field::Missing, Field::Missing, Field::Missing) => serde_json::from_value(channels).map(|channels| Measurement::ForceBridge { channels }),
        (Kind::PressureBridge, Field::Missing, Field::Missing, Field::Missing) => serde_json::from_value(channels).map(|channels| Measurement::PressureBridge { channels }),
        (Kind::VelocityIepe, Field::Missing, Field::Missing, Field::Missing) => serde_json::from_value(channels).map(|channels| Measurement::VelocityIepe { channels }),
        (Kind::ForceIepe, Field::Missing, Field::Missing, Field::Missing) => serde_json::from_value(channels).map(|channels| Measurement::ForceIepe { channels }),
        (Kind::ChargeAcceleration, Field::Missing, Field::Missing, Field::Missing) => serde_json::from_value(channels).map(|channels| Measurement::ChargeAcceleration { channels }),
        (Kind::Lvdt, Field::Missing, Field::Missing, Field::Missing) => serde_json::from_value(channels).map(|channels| Measurement::Lvdt { channels }),
        (Kind::Rvdt, Field::Missing, Field::Missing, Field::Missing) => serde_json::from_value(channels).map(|channels| Measurement::Rvdt { channels }),
        (Kind::TorqueBridge, Field::Missing, Field::Missing, Field::Missing) => serde_json::from_value(channels).map(|channels| Measurement::TorqueBridge { channels }),
        (Kind::CounterPeriod, Field::Missing, Field::Missing, Field::Missing) => serde_json::from_value(channels).map(|channels| Measurement::CounterPeriod { channels }),
        (Kind::CounterPulseWidth, Field::Missing, Field::Missing, Field::Missing) => serde_json::from_value(channels).map(|channels| Measurement::CounterPulseWidth { channels }),
        (Kind::CounterAngularPosition, Field::Missing, Field::Missing, Field::Missing) => serde_json::from_value(channels).map(|channels| Measurement::CounterAngularPosition { channels }),
        (Kind::CounterLinearPosition, Field::Missing, Field::Missing, Field::Missing) => serde_json::from_value(channels).map(|channels| Measurement::CounterLinearPosition { channels }),
        _ => return Err("NI-DAQmx voltage requires string channels and terminal_config/min_volts/max_volts; thermocouple/current/rtd/mixed require their own typed channel objects and no voltage-only fields".into()),
    };
    parsed.map_err(|error| format!("invalid NI-DAQmx channel configuration: {error}"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    signal: String,
    #[serde(default)]
    measurement: Kind,
    #[serde(deserialize_with = "crate::session::strict_json_value")]
    channels: serde_json::Value,
    #[serde(default)]
    terminal_config: Field<TerminalConfig>,
    #[serde(default)]
    min_volts: Field<f64>,
    #[serde(default)]
    max_volts: Field<f64>,
    #[serde(default)]
    timing: TimingKind,
    #[serde(default)]
    rate_hz: Field<f64>,
    #[serde(default, deserialize_with = "unsigned_field")]
    max_samples_per_channel: Field<u32>,
    #[serde(default, deserialize_with = "unsigned_field")]
    host_buffer_samples_per_channel: Field<u32>,
    #[serde(default, deserialize_with = "unsigned_field")]
    poll_interval_ms: Field<u32>,
    #[serde(
        default = "default_cleanup_timeout",
        deserialize_with = "crate::config_number::unsigned"
    )]
    cleanup_timeout_ms: u32,
    #[serde(default)]
    library: Option<PathBuf>,
}

const fn default_cleanup_timeout() -> u32 {
    30_000
}

impl<'de> Deserialize<'de> for Config {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let input = Input::deserialize(deserializer)?;
        let measurement = measurement(
            input.measurement,
            input.channels,
            input.terminal_config,
            input.min_volts,
            input.max_volts,
        )
        .map_err(D::Error::custom)?;
        let timing = match (
            input.timing,
            input.rate_hz,
            input.max_samples_per_channel,
            input.host_buffer_samples_per_channel,
            input.poll_interval_ms,
        ) {
            (
                TimingKind::Continuous,
                Field::Present(rate_hz),
                Field::Present(max_samples_per_channel),
                Field::Present(host_buffer_samples_per_channel),
                Field::Missing,
            ) => Timing::Continuous {
                rate_hz,
                max_samples_per_channel,
                host_buffer_samples_per_channel,
            },
            (
                TimingKind::OnDemand,
                Field::Missing,
                Field::Missing,
                Field::Missing,
                Field::Present(poll_interval_ms),
            ) => Timing::OnDemand {
                poll_interval_ms: u64::from(poll_interval_ms),
            },
            _ => {
                return Err(D::Error::custom(
                    "NI-DAQmx continuous timing requires rate_hz/max_samples_per_channel/host_buffer_samples_per_channel and no poll_interval_ms; on_demand requires poll_interval_ms and omits all three continuous fields",
                ));
            }
        };
        Ok(Self {
            signal: input.signal,
            measurement,
            timing,
            cleanup_timeout_ms: input.cleanup_timeout_ms,
            library: input.library,
        })
    }
}

impl Config {
    #[cfg(any(all(feature = "ni-daqmx", any(target_os = "linux", windows)), test))]
    pub(super) fn max_samples_per_channel(&self) -> u32 {
        match self.timing {
            Timing::Continuous {
                max_samples_per_channel,
                ..
            } => max_samples_per_channel,
            Timing::OnDemand { .. } => 1,
        }
    }
    pub(super) fn physical_channels(&self) -> Vec<String> {
        match &self.measurement {
            Measurement::Voltage { channels, .. } | Measurement::DigitalPort { channels } => {
                channels.clone()
            }
            Measurement::Thermocouple { channels } => {
                channels.iter().map(|c| c.physical.clone()).collect()
            }
            Measurement::Current { channels } => {
                channels.iter().map(|c| c.physical.clone()).collect()
            }
            Measurement::Rtd { channels } => channels.iter().map(|c| c.physical.clone()).collect(),
            Measurement::Mixed { channels } => {
                channels.iter().map(|c| c.physical().to_owned()).collect()
            }
            Measurement::CounterCount { channels } => {
                channels.iter().map(|c| c.physical.clone()).collect()
            }
            Measurement::CounterFrequency { channels } => {
                channels.iter().map(|c| c.physical.clone()).collect()
            }
            Measurement::Strain { channels } => {
                channels.iter().map(|c| c.physical.clone()).collect()
            }
            Measurement::BridgeRatio { channels } => {
                channels.iter().map(|c| c.physical.clone()).collect()
            }
            Measurement::Acceleration { channels } => {
                channels.iter().map(|c| c.physical.clone()).collect()
            }
            Measurement::Resistance { channels } => {
                channels.iter().map(|c| c.physical.clone()).collect()
            }
            Measurement::ThermistorCurrent { channels } => {
                channels.iter().map(|c| c.physical.clone()).collect()
            }
            Measurement::ThermistorVoltage { channels } => {
                channels.iter().map(|c| c.physical.clone()).collect()
            }
            Measurement::ForceBridge { channels } => {
                channels.iter().map(|c| c.physical.clone()).collect()
            }
            Measurement::PressureBridge { channels } => {
                channels.iter().map(|c| c.physical.clone()).collect()
            }
            Measurement::VelocityIepe { channels } => {
                channels.iter().map(|c| c.physical.clone()).collect()
            }
            Measurement::ForceIepe { channels } => {
                channels.iter().map(|c| c.physical.clone()).collect()
            }
            Measurement::ChargeAcceleration { channels } => {
                channels.iter().map(|c| c.physical.clone()).collect()
            }
            Measurement::Lvdt { channels } => channels.iter().map(|c| c.physical.clone()).collect(),
            Measurement::Rvdt { channels } => channels.iter().map(|c| c.physical.clone()).collect(),
            Measurement::TorqueBridge { channels } => {
                channels.iter().map(|c| c.physical.clone()).collect()
            }
            Measurement::CounterPeriod { channels } => {
                channels.iter().map(|c| c.physical.clone()).collect()
            }
            Measurement::CounterPulseWidth { channels } => {
                channels.iter().map(|c| c.physical.clone()).collect()
            }
            Measurement::CounterAngularPosition { channels } => {
                channels.iter().map(|c| c.physical.clone()).collect()
            }
            Measurement::CounterLinearPosition { channels } => {
                channels.iter().map(|c| c.physical.clone()).collect()
            }
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        crate::timeseries::validate_name(&self.signal, "NI-DAQmx signal")?;
        let count = self.measurement.channel_count();
        if !self.measurement.valid_engineering() {
            return Err(Categorized::error(
                ErrorKind::Usage,
                "NI-DAQmx engineering input requires finite scientific ranges/requests, positive sensitivity/resistance/excitation where selected, and valid explicit encoder/source settings",
            ));
        }
        if matches!(
            self.measurement,
            Measurement::CounterCount { .. }
                | Measurement::CounterFrequency { .. }
                | Measurement::CounterPeriod { .. }
                | Measurement::CounterPulseWidth { .. }
                | Measurement::CounterAngularPosition { .. }
                | Measurement::CounterLinearPosition { .. }
        ) && count != 1
        {
            return Err(Categorized::error(
                ErrorKind::Usage,
                "NI-DAQmx counter input requires exactly one individual counter per task",
            ));
        }
        if self.measurement.on_demand_only() && !matches!(self.timing, Timing::OnDemand { .. }) {
            return Err(Categorized::error(
                ErrorKind::Usage,
                "NI-DAQmx digital-port and counter input require explicit on_demand timing",
            ));
        }
        if !(1..=super::MAX_CHANNELS).contains(&count) {
            return Err(Categorized::error(
                ErrorKind::Usage,
                if self.measurement.on_demand_only() {
                    "NI-DAQmx requires 1–64 individual physical input channels"
                } else {
                    "NI-DAQmx requires 1–64 individual physical AI channels"
                },
            ));
        }
        let mut distinct = BTreeSet::new();
        if !self.physical_channels().iter().all(|name| {
            self.physical_key(name)
                .is_some_and(|key| distinct.insert(key))
        }) {
            return Err(Categorized::error(
                ErrorKind::Usage,
                if self.measurement.on_demand_only() {
                    "NI-DAQmx requires distinct individual ASCII physical names for the selected input family; ranges and aliases are not accepted"
                } else {
                    "NI-DAQmx requires distinct individual ASCII physical AI names, e.g. Dev1/ai0; ranges and aliases are not accepted"
                },
            ));
        }
        match &self.measurement {
            Measurement::Voltage {
                min_volts,
                max_volts,
                ..
            } if !min_volts.is_finite() || !max_volts.is_finite() || min_volts >= max_volts => {
                return Err(Categorized::error(
                    ErrorKind::Usage,
                    "NI-DAQmx requires finite min_volts < max_volts",
                ));
            }
            Measurement::Thermocouple { channels }
                if channels.iter().any(|c| {
                    !c.min_temperature.is_finite()
                        || !c.max_temperature.is_finite()
                        || c.min_temperature >= c.max_temperature
                        || matches!(c.cjc, ColdJunction::Constant { value } if !value.is_finite())
                }) =>
            {
                return Err(Categorized::error(
                    ErrorKind::Usage,
                    "NI-DAQmx thermocouple ranges require finite min_temperature < max_temperature and finite constant CJC values in each channel's units",
                ));
            }
            Measurement::Current { channels } if channels.iter().any(|c| !c.min_amperes.is_finite()
                || !c.max_amperes.is_finite() || c.min_amperes >= c.max_amperes
                || matches!(c.shunt, CurrentShunt::External { ohms } if !ohms.is_finite() || ohms <= 0.0)) => {
                return Err(Categorized::error(ErrorKind::Usage, "NI-DAQmx current ranges require finite min_amperes < max_amperes and positive finite external shunt ohms"));
            }
            Measurement::Rtd { channels } if channels.iter().any(|c| {
                let amperes = match c.excitation {
                    CurrentExcitation::Internal { amperes } | CurrentExcitation::External { amperes } => amperes,
                };
                !c.min_temperature.is_finite() || !c.max_temperature.is_finite()
                    || c.min_temperature >= c.max_temperature
                    || !c.r0_ohms.is_finite() || c.r0_ohms <= 0.0
                    || !amperes.is_finite() || amperes <= 0.0
            }) => {
                return Err(Categorized::error(ErrorKind::Usage, "NI-DAQmx RTD requires finite min_temperature < max_temperature, positive finite r0_ohms at 0 Celsius and explicit positive finite excitation amperes"));
            }
            Measurement::Mixed { channels } if channels.iter().any(|c| !c.valid()) => {
                return Err(Categorized::error(ErrorKind::Usage, "NI-DAQmx mixed channels require valid finite family ranges, CJC/shunt values and positive finite RTD reference resistance/excitation"));
            }
            Measurement::CounterCount { channels } if channels.iter().any(|c| !valid_input_terminal(c.input_terminal.as_deref())) => {
                return Err(Categorized::error(ErrorKind::Usage, "NI-DAQmx input_terminal must be an individual bounded ASCII terminal path"));
            }
            Measurement::CounterFrequency { channels } if channels.iter().any(|c| {
                !c.min_hz.is_finite() || c.min_hz < 0.0 || !c.max_hz.is_finite() || c.min_hz >= c.max_hz
                    || !c.measurement_time_seconds.is_finite() || c.measurement_time_seconds < 0.0
                    || !valid_input_terminal(c.input_terminal.as_deref())
            }) => {
                return Err(Categorized::error(ErrorKind::Usage, "NI-DAQmx counter frequency requires finite 0 <= min_hz < max_hz, finite nonnegative measurement_time_seconds and an optional individual ASCII input_terminal"));
            }
            _ => {}
        }
        match self.timing {
            Timing::Continuous {
                rate_hz,
                max_samples_per_channel,
                host_buffer_samples_per_channel,
            } if !rate_hz.is_finite()
                || rate_hz <= 0.0
                || !(1..=4096).contains(&max_samples_per_channel)
                || !(1..=1_048_576).contains(&host_buffer_samples_per_channel)
                || u64::from(host_buffer_samples_per_channel)
                    * u64::try_from(count).unwrap_or(u64::MAX)
                    > 4_194_304 =>
            {
                return Err(Categorized::error(
                    ErrorKind::Usage,
                    "NI-DAQmx continuous timing requires positive finite rate_hz, max_samples_per_channel 1–4096, host_buffer_samples_per_channel 1–1048576 and at most 4194304 channel-samples in total",
                ));
            }
            Timing::OnDemand { poll_interval_ms }
                if !(1..=3_600_000).contains(&poll_interval_ms) =>
            {
                return Err(Categorized::error(
                    ErrorKind::Usage,
                    "NI-DAQmx on_demand poll_interval_ms must be 1–3600000",
                ));
            }
            _ => {}
        }
        if !(1..=30_000).contains(&self.cleanup_timeout_ms) {
            return Err(Categorized::error(
                ErrorKind::Usage,
                "NI-DAQmx cleanup_timeout_ms must be 1–30000",
            ));
        }
        if let Some(path) = &self.library {
            if !path.is_absolute()
                || path.to_str().is_none_or(|s| s.contains('\0'))
                || path.as_os_str().len() > 16_384
            {
                return Err(Categorized::error(
                    ErrorKind::Usage,
                    "NI-DAQmx library must be a bounded absolute NUL-free installed-library path",
                ));
            }
            #[cfg(windows)]
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_none_or(|name| !name.eq_ignore_ascii_case("nicai_utf8.dll"))
            {
                return Err(Categorized::error(
                    ErrorKind::Usage,
                    "NI-DAQmx on Windows requires nicai_utf8.dll; locale-encoded nicaiu.dll is not supported",
                ));
            }
        }
        Ok(())
    }

    #[cfg(any(all(feature = "ni-daqmx", any(target_os = "linux", windows)), test))]
    pub(super) fn validate_driver_channels(&self, channels: &[String]) -> Result<()> {
        if let Measurement::DigitalPort {
            channels: requested,
        } = &self.measurement
        {
            if channels.len() != requested.len()
                || channels
                    .iter()
                    .enumerate()
                    .any(|(index, name)| *name != format!("cfdp{index:04}"))
            {
                return Err(Categorized::error(
                    ErrorKind::Incomplete,
                    "NI-DAQmx returned a different assigned digital-port channel count or order",
                ));
            }
            return Ok(());
        }
        let expected = self.physical_channels();
        if channels.len() != expected.len()
            || channels.iter().zip(&expected).any(|(actual, wanted)| {
                self.physical_key(actual).is_none()
                    || self.physical_key(actual) != self.physical_key(wanted)
            })
        {
            return Err(Categorized::error(
                ErrorKind::Incomplete,
                "NI-DAQmx returned a different physical channel count or read order",
            ));
        }
        Ok(())
    }

    fn physical_key(&self, name: &str) -> Option<(String, u32)> {
        match self.measurement {
            Measurement::DigitalPort { .. } => super::input_channel_key(name, "port"),
            Measurement::CounterCount { .. }
            | Measurement::CounterFrequency { .. }
            | Measurement::CounterPeriod { .. }
            | Measurement::CounterPulseWidth { .. }
            | Measurement::CounterAngularPosition { .. }
            | Measurement::CounterLinearPosition { .. } => super::input_channel_key(name, "ctr"),
            _ => super::channel_key(name),
        }
    }
}
