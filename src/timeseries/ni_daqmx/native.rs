//! Fixed `NI-DAQmx` ABI, loaded only in the isolated SDK worker.
//! The independent ABI oracle was frozen before this module was authored:
//! `tests/fixtures/nidaqmx/abi.json`, SHA256 1bbc016312c3aab6093483cfa3aeee442e6bc3aaeb74eaa8113641a090329b6b.
//! The TC extension follows independently frozen thermocouple.json, SHA256
//! 2a8109877c63aa7448965c0d0ac9ec060355d8dde41e98496b4ff8050ac65cb1.
//! Current-input arguments follow independently frozen current.json, SHA256
//! 14f428f82a6cf94704ac47ff2b42d18bf71553435437ae4f0345bf4260779bff.
//! On-demand policy follows independently frozen on-demand.json, SHA256
//! b87122de19d2bdb77f16c0511d1759925b84697ad59e97375197d1b54e81fe89.
//! RTD arguments follow the independently frozen rtd.json, SHA256
//! 5f83a52d2ce10c71d6e1f7498bb6bd020828365a734fbe235cb7bbcadd2cb6a5.
//! Mixed selection/columns follow independently frozen mixed-ai.json, SHA256
//! d4a74d1e2310b8f16648e296ab95ebffe175559fbe326655ac98dae6d31a9d78.
//! Digital ports and counter inputs follow independently frozen inputs:
//! digital-port.json, SHA256 78475d5d3a4dc0e02405b57887dfd9f3bfd3e7659b31613edde6aea674af9434;
//! counter-input.json, SHA256 a056640e872e599b54413e18e7824ac114a6fe99b248693838b09b7a51a44d93.
//! Seven engineering extensions follow frozen engineering-ai.json, SHA256
//! 889cba7a8fef9408df7dba6af195dda402df094387d39ee14a54f4eaa44797fb,
//! and counter-time-position.json, SHA256
//! 1a3e1be6f66ff03ea3f41c776ca057339d5cc1ce3c985e38d935b13a8c21e63d.
//! Resistive/physical bridge extensions follow frozen resistive-ai.json, SHA256
//! 1b53d4de698afc6cadeb64f1b801adf24284b4eb32bb039958b31c509bb4c240,
//! and physical-bridge.json, SHA256
//! ddea433df60cc8a0b13a1dfa252a47a059f18fb47995ec4829ab97116abd9f55.
//! Dynamic/analog-position additions follow frozen dynamic-ai.json, SHA256
//! 879fdd4184384522e4aa759140173435dfcfc795de830f7c2039d24c5893d639,
//! and analog-position.json, SHA256
//! 003282505c0ca4a0532ea7e10534bddd26845e8b19720867d0d108d46c7c09bc.
use super::{
    Config, MAX_CHANNELS, MAX_WORDS, Measurement, TerminalConfig,
    config::{
        AcWireMode, AccelerationChannel, AccelerationUnits, AngularUnits, BridgeCalibration,
        BridgeConfig, BridgeRatioChannel, BridgeUnits, ChargeAccelerationChannel,
        ChargeSensitivityUnits, ColdJunction, CountDirection, CounterAngularPositionChannel,
        CounterCountChannel, CounterFrequencyChannel, CounterLinearPositionChannel,
        CounterPeriodChannel, CounterPulseWidthChannel, Coupling, CurrentChannel,
        CurrentExcitation, CurrentShunt, Edge, EncoderDecoding, ExcitationSource,
        ForceBridgeChannel, ForceIepeChannel, ForceSensitivityUnits, ForceUnits, FrequencyMethod,
        LinearUnits, LvdtChannel, LvdtSensitivityUnits, MixedChannel, PeriodMethod,
        PressureBridgeChannel, PressureUnits, ResistanceChannel, ResistanceConfig, RtdChannel,
        RtdType, RvdtChannel, RvdtSensitivityUnits, SensitivityUnits, StrainBridgeType,
        StrainChannel, TemperatureUnits, ThermistorCurrentChannel, ThermistorVoltageChannel,
        ThermocoupleChannel, ThermocoupleType, Timing, TorqueBridgeChannel, TorqueUnits,
        VelocityIepeChannel, VelocitySensitivityUnits, VelocityUnits, VoltageChannel, ZIndex,
        ZIndexPhase,
    },
    wire,
};
use crate::types::{Categorized, ErrorKind, with_details};
use anyhow::{Context as _, Result};
use serde_json::json;
use std::{
    ffi::{CStr, CString, c_char, c_void},
    mem::ManuallyDrop,
    path::Path,
    ptr::NonNull,
    time::Duration,
};

type Handle = *mut c_void;
type VoltageConstructor = unsafe extern "system" fn(
    Handle,
    *const c_char,
    *const c_char,
    i32,
    f64,
    f64,
    i32,
    *const c_char,
) -> i32;
type ThermocoupleConstructor = unsafe extern "system" fn(
    Handle,
    *const c_char,
    *const c_char,
    f64,
    f64,
    i32,
    i32,
    i32,
    f64,
    *const c_char,
) -> i32;
type CurrentConstructor = unsafe extern "system" fn(
    Handle,
    *const c_char,
    *const c_char,
    i32,
    f64,
    f64,
    i32,
    i32,
    f64,
    *const c_char,
) -> i32;
type RtdConstructor = unsafe extern "system" fn(
    Handle,
    *const c_char,
    *const c_char,
    f64,
    f64,
    i32,
    i32,
    i32,
    i32,
    f64,
    f64,
) -> i32;
type TerminalSetter = unsafe extern "system" fn(Handle, *const c_char, i32) -> i32;
type DigitalConstructor =
    unsafe extern "system" fn(Handle, *const c_char, *const c_char, i32) -> i32;
type CountConstructor =
    unsafe extern "system" fn(Handle, *const c_char, *const c_char, i32, u32, i32) -> i32;
type FrequencyConstructor = unsafe extern "system" fn(
    Handle,
    *const c_char,
    *const c_char,
    f64,
    f64,
    i32,
    i32,
    i32,
    f64,
    u32,
    *const c_char,
) -> i32;
type InputTerminalSetter = unsafe extern "system" fn(Handle, *const c_char, *const c_char) -> i32;
type AnalogRead =
    unsafe extern "system" fn(Handle, i32, f64, u32, *mut f64, u32, *mut i32, *mut u32) -> i32;
type DigitalRead =
    unsafe extern "system" fn(Handle, i32, f64, u32, *mut u32, u32, *mut i32, *mut u32) -> i32;
// Counter array reads have seven slots and NO fill mode. Their F64/U32
// output types do not make them interchangeable with analog/digital reads.
type CountRead =
    unsafe extern "system" fn(Handle, i32, f64, *mut u32, u32, *mut i32, *mut u32) -> i32;
type FrequencyRead =
    unsafe extern "system" fn(Handle, i32, f64, *mut f64, u32, *mut i32, *mut u32) -> i32;
type DigitalLines = unsafe extern "system" fn(Handle, *const c_char, *mut u32) -> i32;
type StrainConstructor = unsafe extern "system" fn(
    Handle,
    *const c_char,
    *const c_char,
    f64,
    f64,
    i32,
    i32,
    i32,
    f64,
    f64,
    f64,
    f64,
    f64,
    f64,
    *const c_char,
) -> i32;
type BridgeConstructor = unsafe extern "system" fn(
    Handle,
    *const c_char,
    *const c_char,
    f64,
    f64,
    i32,
    i32,
    i32,
    f64,
    f64,
    *const c_char,
) -> i32;
type AccelerationConstructor = unsafe extern "system" fn(
    Handle,
    *const c_char,
    *const c_char,
    i32,
    f64,
    f64,
    i32,
    f64,
    i32,
    i32,
    f64,
    *const c_char,
) -> i32;
type PeriodConstructor = unsafe extern "system" fn(
    Handle,
    *const c_char,
    *const c_char,
    f64,
    f64,
    i32,
    i32,
    i32,
    f64,
    u32,
    *const c_char,
) -> i32;
type PulseWidthConstructor = unsafe extern "system" fn(
    Handle,
    *const c_char,
    *const c_char,
    f64,
    f64,
    i32,
    i32,
    *const c_char,
) -> i32;
type AngularConstructor = unsafe extern "system" fn(
    Handle,
    *const c_char,
    *const c_char,
    i32,
    u32,
    f64,
    i32,
    i32,
    u32,
    f64,
    *const c_char,
) -> i32;
type LinearConstructor = unsafe extern "system" fn(
    Handle,
    *const c_char,
    *const c_char,
    i32,
    u32,
    f64,
    i32,
    i32,
    f64,
    f64,
    *const c_char,
) -> i32;
type ResistanceConstructor = unsafe extern "system" fn(
    Handle,
    *const c_char,
    *const c_char,
    f64,
    f64,
    i32,
    i32,
    i32,
    f64,
    *const c_char,
) -> i32;
type ThermistorCurrentConstructor = unsafe extern "system" fn(
    Handle,
    *const c_char,
    *const c_char,
    f64,
    f64,
    i32,
    i32,
    i32,
    f64,
    f64,
    f64,
    f64,
) -> i32;
type ThermistorVoltageConstructor = unsafe extern "system" fn(
    Handle,
    *const c_char,
    *const c_char,
    f64,
    f64,
    i32,
    i32,
    i32,
    f64,
    f64,
    f64,
    f64,
    f64,
) -> i32;
// The three ordinary two-point bridge symbols have this same fixed17-slot ABI.
// Each is still selected/resolved independently; their unit domains differ.
type PhysicalBridgeConstructor = unsafe extern "system" fn(
    Handle,
    *const c_char,
    *const c_char,
    f64,
    f64,
    i32,
    i32,
    i32,
    f64,
    f64,
    f64,
    f64,
    i32,
    f64,
    f64,
    i32,
    *const c_char,
) -> i32;

// The two distinct IEPE symbols share this independently frozen fixed12-slot ABI.
// Output and sensitivity unit domains remain selected separately for each symbol.
type IepeConstructor = unsafe extern "system" fn(
    Handle,
    *const c_char,
    *const c_char,
    i32,
    f64,
    f64,
    i32,
    f64,
    i32,
    i32,
    f64,
    *const c_char,
) -> i32;
type ChargeConstructor = unsafe extern "system" fn(
    Handle,
    *const c_char,
    *const c_char,
    i32,
    f64,
    f64,
    i32,
    f64,
    i32,
    *const c_char,
) -> i32;
// LVDT and RVDT have this same fixed13-slot ABI, with distinct unit domains.
// There is no terminal slot, and AC leads are not resistance wiring.
type AcPositionConstructor = unsafe extern "system" fn(
    Handle,
    *const c_char,
    *const c_char,
    f64,
    f64,
    i32,
    f64,
    i32,
    i32,
    f64,
    f64,
    i32,
    *const c_char,
) -> i32;

#[derive(Clone, Copy)]
enum ConstructorKind {
    Voltage,
    Thermocouple,
    Current,
    Rtd,
    Terminal,
    Digital,
    Count,
    Frequency,
    CountTerminal,
    FrequencyTerminal,
    Strain,
    Bridge,
    Acceleration,
    Coupling,
    Period,
    PulseWidth,
    AngularPosition,
    LinearPosition,
    PeriodTerminal,
    PulseWidthTerminal,
    EncoderATerminal,
    EncoderBTerminal,
    EncoderZTerminal,
    Resistance,
    ThermistorCurrent,
    ThermistorVoltage,
    ForceBridge,
    PressureBridge,
    TorqueBridge,
    VelocityIepe,
    ForceIepe,
    ChargeAcceleration,
    Lvdt,
    Rvdt,
}
#[derive(Default)]
struct ConstructorSelection(u64);
impl ConstructorSelection {
    fn insert(&mut self, kind: ConstructorKind) {
        self.0 |= 1u64 << kind as u8;
    }
    fn contains(&self, kind: ConstructorKind) -> bool {
        self.0 & (1u64 << kind as u8) != 0
    }
    fn thermocouple(&mut self, channel: &ThermocoupleChannel) {
        self.insert(ConstructorKind::Thermocouple);
        if terminal_request(channel.terminal_config).is_some() {
            self.insert(ConstructorKind::Terminal);
        }
    }
    fn coupling(&mut self, coupling: Option<Coupling>) {
        if coupling.is_some() {
            self.insert(ConstructorKind::Coupling);
        }
    }
    fn encoder_sources(&mut self, sources: [Option<&str>; 3]) {
        for (source, kind) in sources.into_iter().zip([
            ConstructorKind::EncoderATerminal,
            ConstructorKind::EncoderBTerminal,
            ConstructorKind::EncoderZTerminal,
        ]) {
            if source.is_some() {
                self.insert(kind);
            }
        }
    }
    fn for_measurement(measurement: &Measurement) -> Self {
        let mut selected = Self::default();
        match measurement {
            Measurement::VelocityIepe { channels } => {
                selected.insert(ConstructorKind::VelocityIepe);
                for channel in channels {
                    selected.coupling(channel.coupling);
                }
            }
            Measurement::ForceIepe { channels } => {
                selected.insert(ConstructorKind::ForceIepe);
                for channel in channels {
                    selected.coupling(channel.coupling);
                }
            }
            Measurement::ChargeAcceleration { channels } => {
                selected.insert(ConstructorKind::ChargeAcceleration);
                for channel in channels {
                    selected.coupling(channel.coupling);
                }
            }
            Measurement::Lvdt { channels } => {
                selected.insert(ConstructorKind::Lvdt);
                for channel in channels {
                    selected.coupling(channel.coupling);
                }
            }
            Measurement::Rvdt { channels } => {
                selected.insert(ConstructorKind::Rvdt);
                for channel in channels {
                    selected.coupling(channel.coupling);
                }
            }
            Measurement::Voltage { .. } => selected.insert(ConstructorKind::Voltage),
            Measurement::Current { .. } => selected.insert(ConstructorKind::Current),
            Measurement::Rtd { .. } => selected.insert(ConstructorKind::Rtd),
            Measurement::Resistance { channels } => {
                selected.insert(ConstructorKind::Resistance);
                for channel in channels {
                    selected.coupling(channel.coupling);
                }
            }
            Measurement::ThermistorCurrent { channels } => {
                selected.insert(ConstructorKind::ThermistorCurrent);
                for channel in channels {
                    selected.coupling(channel.coupling);
                }
            }
            Measurement::ThermistorVoltage { channels } => {
                selected.insert(ConstructorKind::ThermistorVoltage);
                for channel in channels {
                    selected.coupling(channel.coupling);
                }
            }
            Measurement::ForceBridge { channels } => {
                selected.insert(ConstructorKind::ForceBridge);
                for channel in channels {
                    selected.coupling(channel.coupling);
                }
            }
            Measurement::PressureBridge { channels } => {
                selected.insert(ConstructorKind::PressureBridge);
                for channel in channels {
                    selected.coupling(channel.coupling);
                }
            }
            Measurement::TorqueBridge { channels } => {
                selected.insert(ConstructorKind::TorqueBridge);
                for channel in channels {
                    selected.coupling(channel.coupling);
                }
            }
            Measurement::Strain { channels } => {
                selected.insert(ConstructorKind::Strain);
                for channel in channels {
                    selected.coupling(channel.coupling);
                }
            }
            Measurement::BridgeRatio { channels } => {
                selected.insert(ConstructorKind::Bridge);
                for channel in channels {
                    selected.coupling(channel.coupling);
                }
            }
            Measurement::Acceleration { channels } => {
                selected.insert(ConstructorKind::Acceleration);
                for channel in channels {
                    selected.coupling(channel.coupling);
                }
            }
            Measurement::CounterPeriod { channels } => {
                selected.insert(ConstructorKind::Period);
                if channels.iter().any(|c| c.input_terminal.is_some()) {
                    selected.insert(ConstructorKind::PeriodTerminal);
                }
            }
            Measurement::CounterPulseWidth { channels } => {
                selected.insert(ConstructorKind::PulseWidth);
                if channels.iter().any(|c| c.input_terminal.is_some()) {
                    selected.insert(ConstructorKind::PulseWidthTerminal);
                }
            }
            Measurement::CounterAngularPosition { channels } => {
                selected.insert(ConstructorKind::AngularPosition);
                for c in channels {
                    selected.encoder_sources([
                        c.a_input_terminal.as_deref(),
                        c.b_input_terminal.as_deref(),
                        c.z_input_terminal.as_deref(),
                    ]);
                }
            }
            Measurement::CounterLinearPosition { channels } => {
                selected.insert(ConstructorKind::LinearPosition);
                for c in channels {
                    selected.encoder_sources([
                        c.a_input_terminal.as_deref(),
                        c.b_input_terminal.as_deref(),
                        c.z_input_terminal.as_deref(),
                    ]);
                }
            }
            Measurement::DigitalPort { .. } => selected.insert(ConstructorKind::Digital),
            Measurement::CounterCount { channels } => {
                selected.insert(ConstructorKind::Count);
                if channels.iter().any(|c| c.input_terminal.is_some()) {
                    selected.insert(ConstructorKind::CountTerminal);
                }
            }
            Measurement::CounterFrequency { channels } => {
                selected.insert(ConstructorKind::Frequency);
                if channels.iter().any(|c| c.input_terminal.is_some()) {
                    selected.insert(ConstructorKind::FrequencyTerminal);
                }
            }
            Measurement::Thermocouple { channels } => {
                for channel in channels {
                    selected.thermocouple(channel);
                }
            }
            Measurement::Mixed { channels } => {
                for channel in channels {
                    match channel {
                        MixedChannel::VelocityIepe(channel) => {
                            selected.insert(ConstructorKind::VelocityIepe);
                            selected.coupling(channel.coupling);
                        }
                        MixedChannel::ForceIepe(channel) => {
                            selected.insert(ConstructorKind::ForceIepe);
                            selected.coupling(channel.coupling);
                        }
                        MixedChannel::ChargeAcceleration(channel) => {
                            selected.insert(ConstructorKind::ChargeAcceleration);
                            selected.coupling(channel.coupling);
                        }
                        MixedChannel::Lvdt(channel) => {
                            selected.insert(ConstructorKind::Lvdt);
                            selected.coupling(channel.coupling);
                        }
                        MixedChannel::Rvdt(channel) => {
                            selected.insert(ConstructorKind::Rvdt);
                            selected.coupling(channel.coupling);
                        }
                        MixedChannel::Voltage(_) => selected.insert(ConstructorKind::Voltage),
                        MixedChannel::Thermocouple(channel) => selected.thermocouple(channel),
                        MixedChannel::Current(_) => selected.insert(ConstructorKind::Current),
                        MixedChannel::Rtd(_) => selected.insert(ConstructorKind::Rtd),
                        MixedChannel::Resistance(c) => {
                            selected.insert(ConstructorKind::Resistance);
                            selected.coupling(c.coupling);
                        }
                        MixedChannel::ThermistorCurrent(c) => {
                            selected.insert(ConstructorKind::ThermistorCurrent);
                            selected.coupling(c.coupling);
                        }
                        MixedChannel::ThermistorVoltage(c) => {
                            selected.insert(ConstructorKind::ThermistorVoltage);
                            selected.coupling(c.coupling);
                        }
                        MixedChannel::ForceBridge(c) => {
                            selected.insert(ConstructorKind::ForceBridge);
                            selected.coupling(c.coupling);
                        }
                        MixedChannel::PressureBridge(c) => {
                            selected.insert(ConstructorKind::PressureBridge);
                            selected.coupling(c.coupling);
                        }
                        MixedChannel::TorqueBridge(c) => {
                            selected.insert(ConstructorKind::TorqueBridge);
                            selected.coupling(c.coupling);
                        }
                        MixedChannel::Strain(c) => {
                            selected.insert(ConstructorKind::Strain);
                            selected.coupling(c.coupling);
                        }
                        MixedChannel::BridgeRatio(c) => {
                            selected.insert(ConstructorKind::Bridge);
                            selected.coupling(c.coupling);
                        }
                        MixedChannel::Acceleration(c) => {
                            selected.insert(ConstructorKind::Acceleration);
                            selected.coupling(c.coupling);
                        }
                    }
                }
            }
        }
        selected
    }
}

struct Constructor {
    voltage: Option<VoltageConstructor>,
    thermocouple: Option<ThermocoupleConstructor>,
    current: Option<CurrentConstructor>,
    rtd: Option<RtdConstructor>,
    set_terminal: Option<TerminalSetter>,
    digital: Option<DigitalConstructor>,
    count: Option<CountConstructor>,
    frequency: Option<FrequencyConstructor>,
    set_count_terminal: Option<InputTerminalSetter>,
    set_frequency_terminal: Option<InputTerminalSetter>,
    strain: Option<StrainConstructor>,
    bridge: Option<BridgeConstructor>,
    acceleration: Option<AccelerationConstructor>,
    set_coupling: Option<TerminalSetter>,
    period: Option<PeriodConstructor>,
    pulse_width: Option<PulseWidthConstructor>,
    angular: Option<AngularConstructor>,
    linear: Option<LinearConstructor>,
    set_period_terminal: Option<InputTerminalSetter>,
    set_pulse_width_terminal: Option<InputTerminalSetter>,
    set_encoder_a: Option<InputTerminalSetter>,
    set_encoder_b: Option<InputTerminalSetter>,
    set_encoder_z: Option<InputTerminalSetter>,
    resistance: Option<ResistanceConstructor>,
    thermistor_current: Option<ThermistorCurrentConstructor>,
    thermistor_voltage: Option<ThermistorVoltageConstructor>,
    force_bridge: Option<PhysicalBridgeConstructor>,
    pressure_bridge: Option<PhysicalBridgeConstructor>,
    torque_bridge: Option<PhysicalBridgeConstructor>,
    velocity_iepe: Option<IepeConstructor>,
    force_iepe: Option<IepeConstructor>,
    charge: Option<ChargeConstructor>,
    lvdt: Option<AcPositionConstructor>,
    rvdt: Option<AcPositionConstructor>,
}
impl Constructor {
    fn load(library: &Library, measurement: &Measurement, deadline: u64) -> Result<Self> {
        wire::check_deadline(deadline)?;
        let selected = ConstructorSelection::for_measurement(measurement);
        // Each expression is inferred as the exact fixed function-pointer type
        // of its field. All selected exports resolve before CreateTask.
        macro_rules! load {
            ($kind:ident, $symbol:literal) => {
                if selected.contains(ConstructorKind::$kind) {
                    wire::check_deadline(deadline)?;
                    // SAFETY: the fixed signature of this literal symbol is
                    // frozen by its independent oracle; Api retains the module.
                    Some(unsafe { library.symbol($symbol)? })
                } else {
                    None
                }
            };
        }
        Ok(Self {
            voltage: load!(Voltage, b"DAQmxCreateAIVoltageChan\0"),
            thermocouple: load!(Thermocouple, b"DAQmxCreateAIThrmcplChan\0"),
            current: load!(Current, b"DAQmxCreateAICurrentChan\0"),
            rtd: load!(Rtd, b"DAQmxCreateAIRTDChan\0"),
            set_terminal: load!(Terminal, b"DAQmxSetAITermCfg\0"),
            digital: load!(Digital, b"DAQmxCreateDIChan\0"),
            count: load!(Count, b"DAQmxCreateCICountEdgesChan\0"),
            frequency: load!(Frequency, b"DAQmxCreateCIFreqChan\0"),
            set_count_terminal: load!(CountTerminal, b"DAQmxSetCICountEdgesTerm\0"),
            set_frequency_terminal: load!(FrequencyTerminal, b"DAQmxSetCIFreqTerm\0"),
            strain: load!(Strain, b"DAQmxCreateAIStrainGageChan\0"),
            bridge: load!(Bridge, b"DAQmxCreateAIBridgeChan\0"),
            acceleration: load!(Acceleration, b"DAQmxCreateAIAccelChan\0"),
            set_coupling: load!(Coupling, b"DAQmxSetAICoupling\0"),
            period: load!(Period, b"DAQmxCreateCIPeriodChan\0"),
            pulse_width: load!(PulseWidth, b"DAQmxCreateCIPulseWidthChan\0"),
            angular: load!(AngularPosition, b"DAQmxCreateCIAngEncoderChan\0"),
            linear: load!(LinearPosition, b"DAQmxCreateCILinEncoderChan\0"),
            set_period_terminal: load!(PeriodTerminal, b"DAQmxSetCIPeriodTerm\0"),
            set_pulse_width_terminal: load!(PulseWidthTerminal, b"DAQmxSetCIPulseWidthTerm\0"),
            set_encoder_a: load!(EncoderATerminal, b"DAQmxSetCIEncoderAInputTerm\0"),
            set_encoder_b: load!(EncoderBTerminal, b"DAQmxSetCIEncoderBInputTerm\0"),
            set_encoder_z: load!(EncoderZTerminal, b"DAQmxSetCIEncoderZInputTerm\0"),
            resistance: load!(Resistance, b"DAQmxCreateAIResistanceChan\0"),
            thermistor_current: load!(ThermistorCurrent, b"DAQmxCreateAIThrmstrChanIex\0"),
            thermistor_voltage: load!(ThermistorVoltage, b"DAQmxCreateAIThrmstrChanVex\0"),
            force_bridge: load!(ForceBridge, b"DAQmxCreateAIForceBridgeTwoPointLinChan\0"),
            pressure_bridge: load!(
                PressureBridge,
                b"DAQmxCreateAIPressureBridgeTwoPointLinChan\0"
            ),
            torque_bridge: load!(TorqueBridge, b"DAQmxCreateAITorqueBridgeTwoPointLinChan\0"),
            velocity_iepe: load!(VelocityIepe, b"DAQmxCreateAIVelocityIEPEChan\0"),
            force_iepe: load!(ForceIepe, b"DAQmxCreateAIForceIEPEChan\0"),
            charge: load!(ChargeAcceleration, b"DAQmxCreateAIAccelChargeChan\0"),
            lvdt: load!(Lvdt, b"DAQmxCreateAIPosLVDTChan\0"),
            rvdt: load!(Rvdt, b"DAQmxCreateAIPosRVDTChan\0"),
        })
    }
}

macro_rules! invoke {
    ($function:expr $(, $argument:expr)* $(,)?) => {{
        // SAFETY: exact fixed ABI from the independent oracle; all pointed-to
        // buffers and the owning library/task live throughout this call.
        unsafe { ($function)($($argument),*) }
    }};
}
#[derive(Default)]
struct ReadApi {
    analog: Option<AnalogRead>,
    digital: Option<DigitalRead>,
    count: Option<CountRead>,
    frequency: Option<FrequencyRead>,
    digital_lines: Option<DigitalLines>,
}
impl ReadApi {
    fn load(library: &Library, measurement: &Measurement, deadline: u64) -> Result<Self> {
        macro_rules! load {
            ($symbol:literal) => {{
                wire::check_deadline(deadline)?;
                // SAFETY: the assigned field fixes the independent ABI type;
                // the owning Api retains this library until task retirement.
                Some(unsafe { library.symbol($symbol)? })
            }};
        }
        Ok(match measurement {
            Measurement::DigitalPort { .. } => Self {
                digital: load!(b"DAQmxReadDigitalU32\0"),
                digital_lines: load!(b"DAQmxGetDINumLines\0"),
                ..Self::default()
            },
            Measurement::CounterCount { .. } => Self {
                count: load!(b"DAQmxReadCounterU32\0"),
                ..Self::default()
            },
            Measurement::CounterFrequency { .. }
            | Measurement::CounterPeriod { .. }
            | Measurement::CounterPulseWidth { .. }
            | Measurement::CounterAngularPosition { .. }
            | Measurement::CounterLinearPosition { .. } => Self {
                frequency: load!(b"DAQmxReadCounterF64\0"),
                ..Self::default()
            },
            _ => Self {
                analog: load!(b"DAQmxReadAnalogF64\0"),
                ..Self::default()
            },
        })
    }
    fn read(
        &self,
        handle: Handle,
        request: i32,
        timeout: f64,
        capacity: u32,
        buffer: &mut ReadBuffer,
        actual: &mut i32,
    ) -> Result<i32> {
        let code = match buffer {
            ReadBuffer::F64(values) => {
                if let Some(read) = self.analog {
                    invoke!(
                        read,
                        handle,
                        request,
                        timeout,
                        GROUP_BY_SCAN,
                        values.as_mut_ptr(),
                        capacity,
                        actual,
                        std::ptr::null_mut()
                    )
                } else if let Some(read) = self.frequency {
                    invoke!(
                        read,
                        handle,
                        request,
                        timeout,
                        values.as_mut_ptr(),
                        capacity,
                        actual,
                        std::ptr::null_mut()
                    )
                } else {
                    return Err(wire::invalid("NI-DAQmx selected F64 read is missing"));
                }
            }
            ReadBuffer::U32(values) => {
                if let Some(read) = self.digital {
                    invoke!(
                        read,
                        handle,
                        request,
                        timeout,
                        GROUP_BY_SCAN,
                        values.as_mut_ptr(),
                        capacity,
                        actual,
                        std::ptr::null_mut()
                    )
                } else if let Some(read) = self.count {
                    invoke!(
                        read,
                        handle,
                        request,
                        timeout,
                        values.as_mut_ptr(),
                        capacity,
                        actual,
                        std::ptr::null_mut()
                    )
                } else {
                    return Err(wire::invalid("NI-DAQmx selected U32 read is missing"));
                }
            }
        };
        Ok(code)
    }
}

enum ReadBuffer {
    F64(Vec<f64>),
    U32(Vec<u32>),
}
impl ReadBuffer {
    fn new(measurement: &Measurement, capacity: usize) -> Self {
        if measurement.word_bytes() == 4 {
            Self::U32(vec![0; capacity])
        } else {
            Self::F64(vec![0.0; capacity])
        }
    }
    fn len(&self) -> usize {
        match self {
            Self::F64(values) => values.len(),
            Self::U32(values) => values.len(),
        }
    }
    fn prefix_hex(&self, words: usize) -> Result<String> {
        let capacity = match self {
            Self::F64(values) => values.len().min(words) * 8,
            Self::U32(values) => values.len().min(words) * 4,
        };
        let mut bytes = Vec::with_capacity(capacity);
        match self {
            Self::F64(values) => {
                let prefix = values
                    .get(..words)
                    .ok_or_else(|| wire::invalid("NI-DAQmx F64 prefix exceeds its buffer"))?;
                for value in prefix {
                    bytes.extend_from_slice(&value.to_bits().to_le_bytes());
                }
            }
            Self::U32(values) => {
                let prefix = values
                    .get(..words)
                    .ok_or_else(|| wire::invalid("NI-DAQmx U32 prefix exceeds its buffer"))?;
                for value in prefix {
                    bytes.extend_from_slice(&value.to_le_bytes());
                }
            }
        }
        Ok(wire::hex(&bytes))
    }
}

#[derive(Clone, Copy)]
struct VoltageRequest<'a> {
    physical: &'a str,
    terminal_config: TerminalConfig,
    min_volts: f64,
    max_volts: f64,
}
fn voltage_request(channel: &VoltageChannel) -> VoltageRequest<'_> {
    VoltageRequest {
        physical: &channel.physical,
        terminal_config: channel.terminal_config,
        min_volts: channel.min_volts,
        max_volts: channel.max_volts,
    }
}
const VOLTS: i32 = 10_348;
const AMPERES: i32 = 10_342;
const RISING: i32 = 10_280;
const CONTINUOUS: i32 = 10_123;
const DO_NOT_OVERWRITE: i32 = 10_159;
const GROUP_BY_SCAN: u32 = 1;
const ALL_LINES: i32 = 1;
const HERTZ: i32 = 10_373;
const SECONDS: i32 = 10_364;
const OHMS: i32 = 10_384;
const STRAIN: i32 = 10_299;
const MAX_NAME_BYTES: usize = 257;
const DIAGNOSTIC_SCRATCH: usize = 2048;
// Leave room for the one terminal negative and both Stop/Clear notices.
const MAX_DEFERRED_WARNINGS: usize = wire::MAX_NOTICES - 3;

fn edge_code(edge: Edge) -> i32 {
    match edge {
        Edge::Rising => RISING,
        Edge::Falling => 10_171,
    }
}
fn count_direction(direction: CountDirection) -> i32 {
    match direction {
        CountDirection::Up => 10_128,
        CountDirection::Down => 10_124,
    }
}
fn frequency_method(method: FrequencyMethod) -> i32 {
    match method {
        FrequencyMethod::LowFrequencyOneCounter => 10_105,
        FrequencyMethod::HighFrequencyTwoCounter => 10_157,
        FrequencyMethod::LargeRangeTwoCounter => 10_205,
        FrequencyMethod::DynamicAveraging => 16_065,
    }
}

fn excitation_source(source: ExcitationSource) -> i32 {
    match source {
        ExcitationSource::Internal => 10_200,
        ExcitationSource::External => 10_167,
        ExcitationSource::None => 10_230,
    }
}
fn coupling_code(coupling: Coupling) -> i32 {
    match coupling {
        Coupling::Ac => 10_045,
        Coupling::Dc => 10_050,
        Coupling::Ground => 10_066,
    }
}
fn strain_bridge_type(kind: StrainBridgeType) -> i32 {
    match kind {
        StrainBridgeType::FullBridgeI => 10_183,
        StrainBridgeType::FullBridgeIi => 10_184,
        StrainBridgeType::FullBridgeIii => 10_185,
        StrainBridgeType::HalfBridgeI => 10_188,
        StrainBridgeType::HalfBridgeIi => 10_189,
        StrainBridgeType::QuarterBridgeI => 10_271,
        StrainBridgeType::QuarterBridgeIi => 10_272,
    }
}
fn bridge_config(kind: BridgeConfig) -> i32 {
    match kind {
        BridgeConfig::Full => 10_182,
        BridgeConfig::Half => 10_187,
        BridgeConfig::Quarter => 10_270,
    }
}
fn bridge_units(units: BridgeUnits) -> i32 {
    match units {
        BridgeUnits::VoltsPerVolt => 15_896,
        BridgeUnits::MillivoltsPerVolt => 15_897,
    }
}
fn acceleration_units(units: AccelerationUnits) -> i32 {
    match units {
        AccelerationUnits::G => 10_186,
        AccelerationUnits::MetersPerSecondSquared => 12_470,
        AccelerationUnits::InchesPerSecondSquared => 12_471,
    }
}
fn sensitivity_units(units: SensitivityUnits) -> i32 {
    match units {
        SensitivityUnits::MillivoltsPerG => 12_509,
        SensitivityUnits::VoltsPerG => 12_510,
    }
}
fn period_method(method: PeriodMethod) -> i32 {
    match method {
        PeriodMethod::LowOne => 10_105,
        PeriodMethod::HighTwo => 10_157,
        PeriodMethod::LargeTwo => 10_205,
    }
}
fn encoder_decoding(decoding: EncoderDecoding) -> i32 {
    match decoding {
        EncoderDecoding::X1 => 10_090,
        EncoderDecoding::X2 => 10_091,
        EncoderDecoding::X4 => 10_092,
        EncoderDecoding::TwoPulseCounting => 10_313,
    }
}
fn z_phase(phase: ZIndexPhase) -> i32 {
    match phase {
        ZIndexPhase::HighHigh => 10_040,
        ZIndexPhase::HighLow => 10_041,
        ZIndexPhase::LowHigh => 10_042,
        ZIndexPhase::LowLow => 10_043,
    }
}
struct ZArguments {
    enabled: u32,
    value: f64,
    phase: i32,
}
fn z_arguments(request: ZIndex) -> ZArguments {
    ZArguments {
        enabled: u32::from(request.enabled),
        value: request.value,
        phase: z_phase(request.phase),
    }
}
fn angular_units(units: AngularUnits) -> i32 {
    match units {
        AngularUnits::Degrees => 10_146,
        AngularUnits::Radians => 10_273,
    }
}
fn linear_units(units: LinearUnits) -> i32 {
    match units {
        LinearUnits::Meters => 10_219,
        LinearUnits::Inches => 10_379,
    }
}
fn force_units(units: ForceUnits) -> i32 {
    match units {
        ForceUnits::Newtons => 15_875,
        ForceUnits::Pounds => 15_876,
        ForceUnits::KilogramForce => 15_877,
    }
}
fn pressure_units(units: PressureUnits) -> i32 {
    match units {
        PressureUnits::Pascals => 10_081,
        PressureUnits::Psi => 15_879,
        PressureUnits::Bar => 15_880,
    }
}
fn torque_units(units: TorqueUnits) -> i32 {
    match units {
        TorqueUnits::NewtonMeters => 15_881,
        TorqueUnits::InchOunces => 15_882,
        TorqueUnits::InchPounds => 15_883,
        TorqueUnits::FootPounds => 15_884,
    }
}

#[derive(Clone, Copy)]
struct CalibrationArguments {
    first_electrical: f64,
    second_electrical: f64,
    electrical_units: i32,
    first_physical: f64,
    second_physical: f64,
    physical_units: i32,
}
fn calibration_arguments<Units>(
    request: &BridgeCalibration<Units>,
    physical_units: i32,
) -> CalibrationArguments {
    CalibrationArguments {
        first_electrical: request.first.electrical,
        second_electrical: request.second.electrical,
        electrical_units: bridge_units(request.electrical_units),
        first_physical: request.first.physical,
        second_physical: request.second.physical,
        physical_units,
    }
}
#[derive(Clone, Copy)]
struct PhysicalBridgeRequest<'a> {
    physical: &'a str,
    minimum: f64,
    maximum: f64,
    output_units: i32,
    bridge: i32,
    source: i32,
    volts: f64,
    nominal_ohms: f64,
    calibration: CalibrationArguments,
    coupling: Option<Coupling>,
}
fn force_request(channel: &ForceBridgeChannel) -> PhysicalBridgeRequest<'_> {
    PhysicalBridgeRequest {
        physical: &channel.physical,
        minimum: channel.min_force,
        maximum: channel.max_force,
        output_units: force_units(channel.units),
        bridge: bridge_config(channel.bridge_config),
        source: excitation_source(channel.excitation.source),
        volts: channel.excitation.volts,
        nominal_ohms: channel.nominal_bridge_ohms,
        calibration: calibration_arguments(
            &channel.calibration,
            force_units(channel.calibration.physical_units),
        ),
        coupling: channel.coupling,
    }
}
fn pressure_request(channel: &PressureBridgeChannel) -> PhysicalBridgeRequest<'_> {
    PhysicalBridgeRequest {
        physical: &channel.physical,
        minimum: channel.min_pressure,
        maximum: channel.max_pressure,
        output_units: pressure_units(channel.units),
        bridge: bridge_config(channel.bridge_config),
        source: excitation_source(channel.excitation.source),
        volts: channel.excitation.volts,
        nominal_ohms: channel.nominal_bridge_ohms,
        calibration: calibration_arguments(
            &channel.calibration,
            pressure_units(channel.calibration.physical_units),
        ),
        coupling: channel.coupling,
    }
}
fn torque_request(channel: &TorqueBridgeChannel) -> PhysicalBridgeRequest<'_> {
    PhysicalBridgeRequest {
        physical: &channel.physical,
        minimum: channel.min_torque,
        maximum: channel.max_torque,
        output_units: torque_units(channel.units),
        bridge: bridge_config(channel.bridge_config),
        source: excitation_source(channel.excitation.source),
        volts: channel.excitation.volts,
        nominal_ohms: channel.nominal_bridge_ohms,
        calibration: calibration_arguments(
            &channel.calibration,
            torque_units(channel.calibration.physical_units),
        ),
        coupling: channel.coupling,
    }
}

fn velocity_units(units: VelocityUnits) -> i32 {
    match units {
        VelocityUnits::Meters => 15_959,
        VelocityUnits::Inches => 15_960,
    }
}
fn velocity_sensitivity_units(units: VelocitySensitivityUnits) -> i32 {
    match units {
        VelocitySensitivityUnits::Millimeters => 15_963,
        VelocitySensitivityUnits::Inches => 15_964,
    }
}
fn force_sensitivity_units(units: ForceSensitivityUnits) -> i32 {
    match units {
        ForceSensitivityUnits::Newtons => 15_891,
        ForceSensitivityUnits::Pounds => 15_892,
    }
}
fn charge_sensitivity_units(units: ChargeSensitivityUnits) -> i32 {
    match units {
        ChargeSensitivityUnits::G => 16_099,
        ChargeSensitivityUnits::MetersPerSecondSquared => 16_100,
        ChargeSensitivityUnits::InchesPerSecondSquared => 16_101,
    }
}
fn lvdt_sensitivity_units(units: LvdtSensitivityUnits) -> i32 {
    match units {
        LvdtSensitivityUnits::Millimeters => 12_506,
        LvdtSensitivityUnits::ThousandthsOfInch => 12_505,
    }
}
fn rvdt_sensitivity_units(units: RvdtSensitivityUnits) -> i32 {
    match units {
        RvdtSensitivityUnits::Degrees => 12_507,
        RvdtSensitivityUnits::Radians => 12_508,
    }
}
fn ac_wire_mode(mode: AcWireMode) -> i32 {
    match mode {
        AcWireMode::Four => 4,
        AcWireMode::Five => 5,
        AcWireMode::Six => 6,
    }
}

#[derive(Clone, Copy)]
struct IepeRequest<'a> {
    physical: &'a str,
    terminal: i32,
    minimum: f64,
    maximum: f64,
    units: i32,
    sensitivity: f64,
    sensitivity_units: i32,
    source: i32,
    amperes: f64,
    coupling: Option<Coupling>,
}
fn velocity_iepe_request(channel: &VelocityIepeChannel) -> IepeRequest<'_> {
    IepeRequest {
        physical: &channel.physical,
        terminal: terminal(channel.terminal_config),
        minimum: channel.min_velocity,
        maximum: channel.max_velocity,
        units: velocity_units(channel.units),
        sensitivity: channel.sensitivity,
        sensitivity_units: velocity_sensitivity_units(channel.sensitivity_units),
        source: excitation_source(channel.excitation.source),
        amperes: channel.excitation.amperes,
        coupling: channel.coupling,
    }
}
fn force_iepe_request(channel: &ForceIepeChannel) -> IepeRequest<'_> {
    IepeRequest {
        physical: &channel.physical,
        terminal: terminal(channel.terminal_config),
        minimum: channel.min_force,
        maximum: channel.max_force,
        units: force_units(channel.units),
        sensitivity: channel.sensitivity,
        sensitivity_units: force_sensitivity_units(channel.sensitivity_units),
        source: excitation_source(channel.excitation.source),
        amperes: channel.excitation.amperes,
        coupling: channel.coupling,
    }
}
#[derive(Clone, Copy)]
struct ChargeRequest<'a> {
    physical: &'a str,
    terminal: i32,
    minimum: f64,
    maximum: f64,
    units: i32,
    sensitivity: f64,
    sensitivity_units: i32,
    coupling: Option<Coupling>,
}
fn charge_request(channel: &ChargeAccelerationChannel) -> ChargeRequest<'_> {
    ChargeRequest {
        physical: &channel.physical,
        terminal: terminal(channel.terminal_config),
        minimum: channel.min_acceleration,
        maximum: channel.max_acceleration,
        units: acceleration_units(channel.units),
        sensitivity: channel.sensitivity,
        sensitivity_units: charge_sensitivity_units(channel.sensitivity_units),
        coupling: channel.coupling,
    }
}
#[derive(Clone, Copy)]
struct AcPositionRequest<'a> {
    physical: &'a str,
    minimum: f64,
    maximum: f64,
    units: i32,
    sensitivity: f64,
    sensitivity_units: i32,
    source: i32,
    volts: f64,
    frequency_hz: f64,
    wire_mode: i32,
    coupling: Option<Coupling>,
}
fn lvdt_request(channel: &LvdtChannel) -> AcPositionRequest<'_> {
    AcPositionRequest {
        physical: &channel.physical,
        minimum: channel.min_position,
        maximum: channel.max_position,
        units: linear_units(channel.units),
        sensitivity: channel.sensitivity,
        sensitivity_units: lvdt_sensitivity_units(channel.sensitivity_units),
        source: excitation_source(channel.excitation.source),
        volts: channel.excitation.volts,
        frequency_hz: channel.excitation.frequency_hz,
        wire_mode: ac_wire_mode(channel.ac_wire_mode),
        coupling: channel.coupling,
    }
}
fn rvdt_request(channel: &RvdtChannel) -> AcPositionRequest<'_> {
    AcPositionRequest {
        physical: &channel.physical,
        minimum: channel.min_angle,
        maximum: channel.max_angle,
        units: angular_units(channel.units),
        sensitivity: channel.sensitivity,
        sensitivity_units: rvdt_sensitivity_units(channel.sensitivity_units),
        source: excitation_source(channel.excitation.source),
        volts: channel.excitation.volts,
        frequency_hz: channel.excitation.frequency_hz,
        wire_mode: ac_wire_mode(channel.ac_wire_mode),
        coupling: channel.coupling,
    }
}

#[derive(Clone, Copy)]
struct InputSourceRequest<'a> {
    source: Option<&'a str>,
    setter: Option<InputTerminalSetter>,
    operation: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingDiagnostic {
    operation: &'static str,
    code: i32,
}
impl PendingDiagnostic {
    fn placeholder(self) -> wire::Notice {
        wire::Notice {
            operation: self.operation.to_owned(),
            status_code: self.code,
            diagnostic_hex: String::new(),
            diagnostic_truncated: false,
            diagnostic_source: "deferred".to_owned(),
            diagnostic_encoding: if cfg!(windows) {
                "utf8"
            } else {
                "locale_bytes"
            }
            .to_owned(),
            diagnostic_lookup_status: 0,
        }
    }
}

#[derive(Default)]
struct DeferredDiagnostics {
    negative: Option<PendingDiagnostic>,
    warnings: Vec<PendingDiagnostic>,
}
impl DeferredDiagnostics {
    fn record(&mut self, operation: &'static str, code: i32) -> Option<wire::Notice> {
        if code == 0 {
            return None;
        }
        let pending = PendingDiagnostic { operation, code };
        if code < 0 {
            // A negative read retires this task before another acquisition SDK
            // call, preserving its thread-local error until Finish.
            self.negative.get_or_insert(pending);
        } else if !self.warnings.contains(&pending) {
            if self.warnings.len() >= MAX_DEFERRED_WARNINGS {
                let mut notice = pending.placeholder();
                "unavailable".clone_into(&mut notice.diagnostic_source);
                notice.diagnostic_truncated = true;
                return Some(notice);
            }
            self.warnings.push(pending);
        }
        // Optional text capacity never removes a raw status from its Read row.
        Some(pending.placeholder())
    }
}

macro_rules! api {
    (clocked { $($clocked:ident: $clocked_symbol:literal ($($clocked_argument:ty),*)),+ $(,)? }
     common { $($field:ident: $symbol:literal ($($argument:ty),*)),+ $(,)? }) => {
        #[derive(Clone, Copy)]
        struct ClockedApi {
            $($clocked: unsafe extern "system" fn($($clocked_argument),*) -> i32),+
        }
        impl ClockedApi {
            fn load(library: &Library, deadline: u64) -> Result<Self> {
                Ok(Self {
                    $($clocked: {
                        wire::check_deadline(deadline)?;
                        // SAFETY: each fixed signature is frozen in abi.json;
                        // the enclosing Api retains this module and its task.
                        unsafe { library.symbol(concat!($clocked_symbol, "\0").as_bytes())? }
                    }),+
                })
            }
        }
        struct Api {
            _library: Library,
            constructor: Constructor,
            read_api: ReadApi,
            clocked: Option<ClockedApi>,
            $($field: unsafe extern "system" fn($($argument),*) -> i32),+
        }
        impl Api {
            fn load(path: Option<&Path>, config: &Config, deadline: u64) -> Result<Self> {
                wire::check_deadline(deadline)?;
                let library = Library::open(path)?;
                wire::check_deadline(deadline)?;
                let constructor = Constructor::load(&library, &config.measurement, deadline)?;
                let read_api = ReadApi::load(&library, &config.measurement, deadline)?;
                let clocked = match &config.timing {
                    Timing::Continuous { .. } => Some(ClockedApi::load(&library, deadline)?),
                    Timing::OnDemand { .. } => None,
                };
                Ok(Self {
                    $($field: {
                        wire::check_deadline(deadline)?;
                        // SAFETY: this symbol has the exact fixed NI signature
                        // frozen in abi.json; Api retains its loaded library.
                        unsafe { library.symbol(concat!($symbol, "\0").as_bytes())? }
                    }),+,
                    constructor,
                    read_api,
                    clocked,
                    _library: library,
                })
            }
        }
    };
}
api! {
    clocked {
        timing: "DAQmxCfgSampClkTiming"(Handle, *const c_char, f64, i32, i32, u64),
        input_buffer: "DAQmxCfgInputBuffer"(Handle, u32),
        overwrite: "DAQmxSetReadOverWrite"(Handle, i32),
        rate: "DAQmxGetSampClkRate"(Handle, *mut f64),
        available: "DAQmxGetReadAvailSampPerChan"(Handle, *mut u32),
    }
    common {
        create: "DAQmxCreateTask"(*const c_char, *mut Handle),
        count: "DAQmxGetTaskNumChans"(Handle, *mut u32),
        nth: "DAQmxGetNthTaskReadChannel"(Handle, u32, *mut c_char, i32),
        start: "DAQmxStartTask"(Handle),
        stop: "DAQmxStopTask"(Handle),
        clear: "DAQmxClearTask"(Handle),
        error_string: "DAQmxGetErrorString"(i32, *mut c_char, u32),
        extended_error: "DAQmxGetExtendedErrorInfo"(*mut c_char, u32),
    }
}

struct Library(NonNull<c_void>);
impl Library {
    #[cfg(target_os = "linux")]
    fn open(path: Option<&Path>) -> Result<Self> {
        use std::os::unix::ffi::OsStrExt as _;
        let name = match path {
            Some(path) => CString::new(path.as_os_str().as_bytes())?,
            None => CString::new("libnidaqmx.so")?,
        };
        // SAFETY: user-installed SDK is loaded only inside the isolated worker;
        // name is NUL terminated and remains live throughout dlopen.
        NonNull::new(unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) })
            .map(Self)
            .ok_or_else(|| unavailable("cannot load the installed NI-DAQmx library"))
    }
    #[cfg(windows)]
    fn open(path: Option<&Path>) -> Result<Self> {
        use std::os::windows::ffi::OsStrExt as _;
        use windows_sys::Win32::System::LibraryLoader::{
            LOAD_WITH_ALTERED_SEARCH_PATH, LoadLibraryExW,
        };
        let path = path.unwrap_or_else(|| Path::new("nicai_utf8.dll"));
        let flags = if path.is_absolute() {
            LOAD_WITH_ALTERED_SEARCH_PATH
        } else {
            0
        };
        let name = path
            .as_os_str()
            .encode_wide()
            .chain([0])
            .collect::<Vec<_>>();
        // SAFETY: name is live UTF-16 with a terminal NUL; an explicit absolute
        // path may resolve adjacent NI dependencies. No locale DLL fallback.
        NonNull::new(unsafe { LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), flags) }.cast())
            .map(Self)
            .ok_or_else(|| unavailable("cannot load the installed nicai_utf8.dll"))
    }
    unsafe fn symbol<T: Copy>(&self, name: &[u8]) -> Result<T> {
        if size_of::<T>() != size_of::<*mut c_void>() {
            return Err(unavailable("incompatible NI-DAQmx function pointer size"));
        }
        let name = CStr::from_bytes_with_nul(name)?;
        #[cfg(target_os = "linux")]
        // SAFETY: the module is live, and name is a live NUL-terminated symbol.
        let address = unsafe { libc::dlsym(self.0.as_ptr(), name.as_ptr()) };
        #[cfg(windows)]
        // SAFETY: the module is live, and name is a live ASCII symbol.
        let address = unsafe {
            windows_sys::Win32::System::LibraryLoader::GetProcAddress(
                self.0.as_ptr().cast(),
                name.as_ptr().cast(),
            )
        }
        .map_or(std::ptr::null_mut(), |function| function as *mut c_void);
        if address.is_null() {
            return Err(Categorized::new(
                ErrorKind::Unavailable,
                "NI-DAQmx library is missing a required fixed export",
            )
            .details(json!({"symbol": name.to_string_lossy()}))
            .into());
        }
        // SAFETY: the caller supplies the fixed signature of this named symbol,
        // and the function pointer is used only while its Api owns this module.
        Ok(unsafe { std::mem::transmute_copy(&address) })
    }
}
impl Drop for Library {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        // SAFETY: this object owns this one successful dlopen reference.
        let _ = unsafe { libc::dlclose(self.0.as_ptr()) };
        #[cfg(windows)]
        // SAFETY: this object owns this one successful LoadLibrary reference.
        let _ = unsafe { windows_sys::Win32::Foundation::FreeLibrary(self.0.as_ptr().cast()) };
    }
}

fn unavailable(message: &'static str) -> anyhow::Error {
    Categorized::error(ErrorKind::Unavailable, message)
}
fn expired() -> anyhow::Error {
    Categorized::error(
        ErrorKind::Timeout,
        "NI-DAQmx original operation or acquisition cutoff expired",
    )
}
fn check(deadline: u64, cutoff: u64) -> Result<()> {
    wire::check_deadline(deadline)?;
    if wire::now()? >= cutoff {
        return Err(expired());
    }
    Ok(())
}
fn software_attempt(
    start: u64,
    next_not_before: u64,
    interval: u64,
    deadline: u64,
    cutoff: u64,
) -> Result<Option<(wire::Pacing, f64)>> {
    let limit = deadline.min(cutoff);
    if start >= limit {
        return Err(expired());
    }
    if start < next_not_before {
        return Ok(None);
    }
    let next_not_before_ns = start
        .checked_add(interval)
        .ok_or_else(|| wire::invalid("NI-DAQmx software pacing frontier overflow"))?;
    Ok(Some((
        wire::Pacing {
            attempt_start_ns: Some(start),
            next_not_before_ns,
        },
        Duration::from_nanos(limit - start).as_secs_f64(),
    )))
}
fn invalid_read_geometry(
    message: &'static str,
    status: i32,
    actual: i32,
    requested: u32,
) -> anyhow::Error {
    with_details(
        wire::invalid(message),
        json!({
            "ni_daqmx_status":status,
            "actual_samples_per_channel":actual,
            "requested_samples_per_channel":requested
        }),
    )
}
fn terminal(config: TerminalConfig) -> i32 {
    match config {
        TerminalConfig::Default => -1,
        TerminalConfig::Rse => 10_083,
        TerminalConfig::Nrse => 10_078,
        TerminalConfig::Differential => 10_106,
        TerminalConfig::Pseudodifferential => 12_529,
    }
}
fn terminal_request(request: Option<TerminalConfig>) -> Option<i32> {
    // Default=-1 belongs to the voltage constructor. The fixed setter does not
    // qualify that sentinel; omitted/default TC requests leave the driver alone.
    request
        .filter(|request| *request != TerminalConfig::Default)
        .map(terminal)
}
fn thermocouple_type(kind: ThermocoupleType) -> i32 {
    match kind {
        ThermocoupleType::J => 10_072,
        ThermocoupleType::K => 10_073,
        ThermocoupleType::N => 10_077,
        ThermocoupleType::R => 10_082,
        ThermocoupleType::S => 10_085,
        ThermocoupleType::T => 10_086,
        ThermocoupleType::B => 10_047,
        ThermocoupleType::E => 10_055,
        ThermocoupleType::A => 16_208,
        ThermocoupleType::C => 16_209,
    }
}
fn temperature_units(units: TemperatureUnits) -> i32 {
    match units {
        TemperatureUnits::DegC => 10_143,
        TemperatureUnits::DegF => 10_144,
        TemperatureUnits::Kelvin => 10_325,
        TemperatureUnits::DegR => 10_145,
    }
}
fn cold_junction(cjc: ColdJunction) -> (i32, f64) {
    match cjc {
        // This ABI dummy is ignored by NI in builtin mode, not a temperature
        // measurement or a configured constant exposed to the user.
        ColdJunction::BuiltIn => (10_200, 0.0),
        // The value is already in the configured channel's measurement units.
        ColdJunction::Constant { value } => (10_116, value),
    }
}
fn current_shunt(shunt: CurrentShunt) -> (i32, f64) {
    match shunt {
        // The numeric external-resistor ABI slot uses NI's reference-wrapper
        // convention. It is not a configured/read-back built-in resistance or
        // a promise that every driver accepts or ignores this argument.
        CurrentShunt::BuiltIn => (10_200, 249.0),
        CurrentShunt::External { ohms } => (10_167, ohms),
    }
}
fn rtd_type(kind: RtdType) -> i32 {
    match kind {
        RtdType::Pt3750 => 12_481,
        RtdType::Pt3851 => 10_071,
        RtdType::Pt3911 => 12_482,
        RtdType::Pt3916 => 10_069,
        RtdType::Pt3920 => 10_053,
        RtdType::Pt3928 => 12_483,
    }
}
fn resistance_config(wiring: ResistanceConfig) -> i32 {
    match wiring {
        ResistanceConfig::Two => 2,
        ResistanceConfig::Three => 3,
        ResistanceConfig::Four => 4,
    }
}
fn current_excitation(excitation: CurrentExcitation) -> (i32, f64) {
    match excitation {
        // Internal is an explicit physical input-excitation request, not a
        // promise of passive wiring or a measured/coerced excitation current.
        CurrentExcitation::Internal { amperes } => (10_200, amperes),
        CurrentExcitation::External { amperes } => (10_167, amperes),
    }
}
struct RtdArguments {
    minimum: f64,
    maximum: f64,
    units: i32,
    kind: i32,
    wiring: i32,
    excitation_source: i32,
    excitation_amperes: f64,
    reference_ohms: f64,
}
fn rtd_arguments(channel: &RtdChannel) -> RtdArguments {
    let (excitation_source, excitation_amperes) = current_excitation(channel.excitation);
    RtdArguments {
        minimum: channel.min_temperature,
        maximum: channel.max_temperature,
        units: temperature_units(channel.units),
        kind: rtd_type(channel.rtd_type),
        wiring: resistance_config(channel.resistance_config),
        excitation_source,
        excitation_amperes,
        // R0 is the requested sensor resistance at0C for every output unit.
        // No local temperature, resistance or excitation conversion is applied.
        reference_ohms: channel.r0_ohms,
    }
}
fn diagnostic(api: &Api, operation: &str, code: i32, deadline: u64) -> wire::Notice {
    let mut bytes = [0u8; DIAGNOSTIC_SCRATCH];
    let mut source = "unavailable";
    let skipped = wire::check_deadline(deadline).is_err();
    let lookup = if skipped {
        0
    } else if code < 0 {
        source = "extended_error_info";
        invoke!(api.extended_error, bytes.as_mut_ptr().cast(), 2048)
    } else {
        source = "error_string";
        invoke!(api.error_string, code, bytes.as_mut_ptr().cast(), 2048)
    };
    let end = bytes.iter().position(|byte| *byte == 0);
    let available = end.unwrap_or(bytes.len());
    let retained = available.min(wire::MAX_DIAGNOSTIC_BYTES);
    wire::Notice {
        operation: operation.to_owned(),
        status_code: code,
        diagnostic_hex: wire::hex(&bytes[..retained]),
        diagnostic_truncated: skipped || available > retained || end.is_none() || lookup != 0,
        diagnostic_source: source.to_owned(),
        diagnostic_encoding: if cfg!(windows) {
            "utf8"
        } else {
            "locale_bytes"
        }
        .to_owned(),
        diagnostic_lookup_status: lookup,
    }
}
fn native_error(notice: &wire::Notice) -> anyhow::Error {
    let kind = match notice.status_code {
        -200_284 => ErrorKind::Timeout,
        -200_279 | -200_361 => ErrorKind::Incomplete,
        _ => ErrorKind::OperationFailed,
    };
    Categorized::new(kind, format!("NI-DAQmx {} failed", notice.operation))
        .details(json!({"ni_daqmx":notice}))
        .into()
}
fn record_status(
    api: &Api,
    code: i32,
    operation: &str,
    deadline: u64,
    notices: &mut Vec<wire::Notice>,
) -> Result<()> {
    if code != 0 {
        let notice = diagnostic(api, operation, code, deadline);
        let error = (code < 0).then(|| native_error(&notice));
        if notices.len() >= wire::MAX_NOTICES {
            return Err(Categorized::new(
                ErrorKind::Incomplete,
                "NI-DAQmx exceeded the bounded native notice count",
            )
            .details(json!({"ni_daqmx":notice}))
            .into());
        }
        notices.push(notice);
        if let Some(error) = error {
            return Err(error);
        }
    }
    wire::check_deadline(deadline)
        .map_err(|error| with_details(error, json!({"native_notices":notices})))
}

/// One task owned exclusively by this worker. Drop never calls the SDK: a hung
/// or expired cleanup is supervised by the parent's exact process-tree owner.
pub(super) struct Task {
    api: ManuallyDrop<Api>,
    handle: Handle,
    unconfirmed_create: bool,
    start_attempted: bool,
    config: Config,
    descriptor: Option<wire::Descriptor>,
    cutoff_ns: u64,
    next_not_before_ns: u64,
    sample_index: u64,
    first_failure: Option<wire::Failure>,
    deferred: DeferredDiagnostics,
}
impl Drop for Task {
    fn drop(&mut self) {
        if self.handle.is_null() && !self.unconfirmed_create {
            // SAFETY: Task uniquely owns Api, and a null task has no outstanding
            // SDK work that could continue executing inside the loaded module.
            unsafe { ManuallyDrop::drop(&mut self.api) };
        }
        // An uncleared native task deliberately retains its module until the
        // worker exits. Never unload code still reachable by vendor threads.
    }
}
impl Task {
    pub(super) fn open(
        config: Config,
        deadline: u64,
        cutoff: u64,
    ) -> Result<(Self, wire::Response)> {
        config.validate()?;
        check(deadline, cutoff)?;
        let api = Api::load(config.library.as_deref(), &config, deadline)?;
        let mut task = Self {
            api: ManuallyDrop::new(api),
            handle: std::ptr::null_mut(),
            unconfirmed_create: false,
            start_attempted: false,
            config,
            descriptor: None,
            cutoff_ns: cutoff,
            next_not_before_ns: 0,
            sample_index: 0,
            first_failure: None,
            deferred: DeferredDiagnostics::default(),
        };
        let mut notices = Vec::new();
        let initialized = task
            .initialize(deadline, cutoff, &mut notices)
            .and_then(|descriptor| {
                let observed_ns = wire::now()?;
                check(deadline, cutoff)?;
                Ok((descriptor, observed_ns))
            });
        match initialized {
            Ok((descriptor, observed_ns)) => {
                task.descriptor = Some(descriptor.clone());
                Ok((
                    task,
                    wire::Response::Ready {
                        descriptor,
                        observed_ns,
                        notices,
                    },
                ))
            }
            Err(error) => {
                let mut error = with_details(error, json!({"setup_notices":notices}));
                let (cleanup, failure) = task.cleanup(deadline);
                error = with_details(error, cleanup_details(&cleanup, failure.as_ref()));
                Err(error)
            }
        }
    }
    fn initialize(
        &mut self,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<wire::Descriptor> {
        check(deadline, cutoff)?;
        let mut returned = std::ptr::null_mut();
        let code = invoke!(self.api.create, c"".as_ptr(), &raw mut returned);
        if code >= 0 {
            // A successful or warning return qualifies ownership immediately,
            // even if diagnostic collection or the post-call budget later fails.
            self.handle = returned;
        } else {
            // A failing function's output is unspecified. Never operate on an
            // apparent handle that the driver did not qualify as created.
            self.unconfirmed_create = !returned.is_null();
        }
        record_status(&self.api, code, "CreateTask", deadline, notices).map_err(|error| {
            if self.unconfirmed_create {
                with_details(error, json!({"task_ownership":"unconfirmed_after_failed_create","cleanup_confirmed":false}))
            } else { error }
        })?;
        if self.handle.is_null() {
            return Err(wire::invalid("NI-DAQmx CreateTask returned a null handle"));
        }
        self.create_channels(deadline, cutoff, notices)?;
        if let Timing::Continuous {
            rate_hz,
            host_buffer_samples_per_channel,
            ..
        } = &self.config.timing
        {
            let clocked = self
                .api
                .clocked
                .context("NI-DAQmx clocked API is missing")?;
            check(deadline, cutoff)?;
            let code = invoke!(
                clocked.timing,
                self.handle,
                c"".as_ptr(),
                *rate_hz,
                RISING,
                CONTINUOUS,
                u64::from(*host_buffer_samples_per_channel)
            );
            record_status(&self.api, code, "CfgSampClkTiming", deadline, notices)?;
            check(deadline, cutoff)?;
            let code = invoke!(
                clocked.input_buffer,
                self.handle,
                *host_buffer_samples_per_channel
            );
            record_status(&self.api, code, "CfgInputBuffer", deadline, notices)?;
            check(deadline, cutoff)?;
            let code = invoke!(clocked.overwrite, self.handle, DO_NOT_OVERWRITE);
            record_status(&self.api, code, "SetReadOverWrite", deadline, notices)?;
        }
        check(deadline, cutoff)?;
        let mut count = 0;
        let code = invoke!(self.api.count, self.handle, &raw mut count);
        record_status(&self.api, code, "GetTaskNumChans", deadline, notices)?;
        let count = usize::try_from(count)?;
        if !(1..=MAX_CHANNELS).contains(&count) || count != self.config.measurement.channel_count()
        {
            return Err(wire::invalid(
                "NI-DAQmx returned an inconsistent physical channel count",
            ));
        }
        let mut channels = Vec::with_capacity(count);
        for index in 1..=count {
            channels.push(self.channel_name(u32::try_from(index)?, deadline, cutoff, notices)?);
        }
        self.config.validate_driver_channels(&channels)?;
        let digital_lines = if matches!(self.config.measurement, Measurement::DigitalPort { .. }) {
            let get_lines = self.api.read_api.digital_lines.ok_or_else(|| {
                wire::invalid("NI-DAQmx selected digital line-count getter is missing")
            })?;
            let mut widths = Vec::with_capacity(count);
            for channel in &channels {
                check(deadline, cutoff)?;
                let name = CString::new(channel.as_bytes())?;
                let mut width = 0;
                let code = invoke!(get_lines, self.handle, name.as_ptr(), &raw mut width);
                record_status(&self.api, code, "GetDINumLines", deadline, notices)?;
                if !(1..=32).contains(&width) {
                    return Err(wire::invalid(
                        "NI-DAQmx returned a digital-port width outside 1–32",
                    ));
                }
                widths.push(width);
            }
            Some(widths)
        } else {
            None
        };
        check(deadline, cutoff)?;
        self.start_attempted = true;
        let code = invoke!(self.api.start, self.handle);
        record_status(&self.api, code, "StartTask", deadline, notices)?;
        check(deadline, cutoff)?;
        let rate = self
            .api
            .clocked
            .map(|clocked| -> Result<f64> {
                let mut rate = 0.0;
                let code = invoke!(clocked.rate, self.handle, &raw mut rate);
                record_status(&self.api, code, "GetSampClkRate", deadline, notices)?;
                Ok(rate)
            })
            .transpose()?;
        let descriptor = match digital_lines {
            Some(widths) => wire::Descriptor::for_digital_ports(&self.config, channels, widths)?,
            None => wire::Descriptor::for_config(&self.config, channels, rate)?,
        };
        descriptor.validate(&self.config)?;
        check(deadline, cutoff)?;
        Ok(descriptor)
    }
    fn create_channels(
        &self,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        match &self.config.measurement {
            Measurement::VelocityIepe { channels } => {
                for channel in channels {
                    self.create_velocity_iepe(channel, deadline, cutoff, notices)?;
                }
            }
            Measurement::ForceIepe { channels } => {
                for channel in channels {
                    self.create_force_iepe(channel, deadline, cutoff, notices)?;
                }
            }
            Measurement::ChargeAcceleration { channels } => {
                for channel in channels {
                    self.create_charge(channel, deadline, cutoff, notices)?;
                }
            }
            Measurement::Lvdt { channels } => {
                for channel in channels {
                    self.create_lvdt(channel, deadline, cutoff, notices)?;
                }
            }
            Measurement::Rvdt { channels } => {
                for channel in channels {
                    self.create_rvdt(channel, deadline, cutoff, notices)?;
                }
            }
            Measurement::Voltage {
                channels,
                terminal_config,
                min_volts,
                max_volts,
            } => {
                for name in channels {
                    self.create_voltage(
                        VoltageRequest {
                            physical: name,
                            terminal_config: *terminal_config,
                            min_volts: *min_volts,
                            max_volts: *max_volts,
                        },
                        deadline,
                        cutoff,
                        notices,
                    )?;
                }
            }
            Measurement::Current { channels } => {
                for channel in channels {
                    self.create_current(channel, deadline, cutoff, notices)?;
                }
            }
            Measurement::Rtd { channels } => {
                for channel in channels {
                    self.create_rtd(channel, deadline, cutoff, notices)?;
                }
            }
            Measurement::Resistance { channels } => {
                for channel in channels {
                    self.create_resistance(channel, deadline, cutoff, notices)?;
                }
            }
            Measurement::ThermistorCurrent { channels } => {
                for channel in channels {
                    self.create_thermistor_current(channel, deadline, cutoff, notices)?;
                }
            }
            Measurement::ThermistorVoltage { channels } => {
                for channel in channels {
                    self.create_thermistor_voltage(channel, deadline, cutoff, notices)?;
                }
            }
            Measurement::ForceBridge { channels } => {
                for channel in channels {
                    self.create_physical_bridge(
                        force_request(channel),
                        self.api.constructor.force_bridge,
                        "CreateAIForceBridgeTwoPointLinChan",
                        deadline,
                        cutoff,
                        notices,
                    )?;
                }
            }
            Measurement::PressureBridge { channels } => {
                for channel in channels {
                    self.create_physical_bridge(
                        pressure_request(channel),
                        self.api.constructor.pressure_bridge,
                        "CreateAIPressureBridgeTwoPointLinChan",
                        deadline,
                        cutoff,
                        notices,
                    )?;
                }
            }
            Measurement::TorqueBridge { channels } => {
                for channel in channels {
                    self.create_physical_bridge(
                        torque_request(channel),
                        self.api.constructor.torque_bridge,
                        "CreateAITorqueBridgeTwoPointLinChan",
                        deadline,
                        cutoff,
                        notices,
                    )?;
                }
            }
            Measurement::DigitalPort { channels } => {
                for (index, physical) in channels.iter().enumerate() {
                    self.create_digital(physical, index, deadline, cutoff, notices)?;
                }
            }
            Measurement::CounterCount { channels } => {
                for channel in channels {
                    self.create_count(channel, deadline, cutoff, notices)?;
                }
            }
            Measurement::CounterFrequency { channels } => {
                for channel in channels {
                    self.create_frequency(channel, deadline, cutoff, notices)?;
                }
            }
            Measurement::Strain { channels } => {
                for channel in channels {
                    self.create_strain(channel, deadline, cutoff, notices)?;
                }
            }
            Measurement::BridgeRatio { channels } => {
                for channel in channels {
                    self.create_bridge(channel, deadline, cutoff, notices)?;
                }
            }
            Measurement::Acceleration { channels } => {
                for channel in channels {
                    self.create_acceleration(channel, deadline, cutoff, notices)?;
                }
            }
            Measurement::CounterPeriod { channels } => {
                for channel in channels {
                    self.create_period(channel, deadline, cutoff, notices)?;
                }
            }
            Measurement::CounterPulseWidth { channels } => {
                for channel in channels {
                    self.create_pulse_width(channel, deadline, cutoff, notices)?;
                }
            }
            Measurement::CounterAngularPosition { channels } => {
                for channel in channels {
                    self.create_angular(channel, deadline, cutoff, notices)?;
                }
            }
            Measurement::CounterLinearPosition { channels } => {
                for channel in channels {
                    self.create_linear(channel, deadline, cutoff, notices)?;
                }
            }
            Measurement::Thermocouple { channels } => {
                for channel in channels {
                    self.create_thermocouple(channel, deadline, cutoff, notices)?;
                }
            }
            Measurement::Mixed { channels } => {
                for channel in channels {
                    match channel {
                        MixedChannel::VelocityIepe(channel) => {
                            self.create_velocity_iepe(channel, deadline, cutoff, notices)?;
                        }
                        MixedChannel::ForceIepe(channel) => {
                            self.create_force_iepe(channel, deadline, cutoff, notices)?;
                        }
                        MixedChannel::ChargeAcceleration(channel) => {
                            self.create_charge(channel, deadline, cutoff, notices)?;
                        }
                        MixedChannel::Lvdt(channel) => {
                            self.create_lvdt(channel, deadline, cutoff, notices)?;
                        }
                        MixedChannel::Rvdt(channel) => {
                            self.create_rvdt(channel, deadline, cutoff, notices)?;
                        }
                        MixedChannel::Voltage(channel) => {
                            self.create_voltage(
                                voltage_request(channel),
                                deadline,
                                cutoff,
                                notices,
                            )?;
                        }
                        MixedChannel::Thermocouple(channel) => {
                            self.create_thermocouple(channel, deadline, cutoff, notices)?;
                        }
                        MixedChannel::Current(channel) => {
                            self.create_current(channel, deadline, cutoff, notices)?;
                        }
                        MixedChannel::Rtd(channel) => {
                            self.create_rtd(channel, deadline, cutoff, notices)?;
                        }
                        MixedChannel::Resistance(channel) => {
                            self.create_resistance(channel, deadline, cutoff, notices)?;
                        }
                        MixedChannel::ThermistorCurrent(channel) => {
                            self.create_thermistor_current(channel, deadline, cutoff, notices)?;
                        }
                        MixedChannel::ThermistorVoltage(channel) => {
                            self.create_thermistor_voltage(channel, deadline, cutoff, notices)?;
                        }
                        MixedChannel::ForceBridge(channel) => {
                            self.create_physical_bridge(
                                force_request(channel),
                                self.api.constructor.force_bridge,
                                "CreateAIForceBridgeTwoPointLinChan",
                                deadline,
                                cutoff,
                                notices,
                            )?;
                        }
                        MixedChannel::PressureBridge(channel) => {
                            self.create_physical_bridge(
                                pressure_request(channel),
                                self.api.constructor.pressure_bridge,
                                "CreateAIPressureBridgeTwoPointLinChan",
                                deadline,
                                cutoff,
                                notices,
                            )?;
                        }
                        MixedChannel::TorqueBridge(channel) => {
                            self.create_physical_bridge(
                                torque_request(channel),
                                self.api.constructor.torque_bridge,
                                "CreateAITorqueBridgeTwoPointLinChan",
                                deadline,
                                cutoff,
                                notices,
                            )?;
                        }
                        MixedChannel::Strain(channel) => {
                            self.create_strain(channel, deadline, cutoff, notices)?;
                        }
                        MixedChannel::BridgeRatio(channel) => {
                            self.create_bridge(channel, deadline, cutoff, notices)?;
                        }
                        MixedChannel::Acceleration(channel) => {
                            self.create_acceleration(channel, deadline, cutoff, notices)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
    fn create_velocity_iepe(
        &self,
        channel: &VelocityIepeChannel,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        self.create_iepe(
            velocity_iepe_request(channel),
            self.api.constructor.velocity_iepe,
            "CreateAIVelocityIEPEChan",
            deadline,
            cutoff,
            notices,
        )
    }
    fn create_force_iepe(
        &self,
        channel: &ForceIepeChannel,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        self.create_iepe(
            force_iepe_request(channel),
            self.api.constructor.force_iepe,
            "CreateAIForceIEPEChan",
            deadline,
            cutoff,
            notices,
        )
    }
    fn create_iepe(
        &self,
        request: IepeRequest<'_>,
        constructor: Option<IepeConstructor>,
        operation: &'static str,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        let create = constructor
            .ok_or_else(|| wire::invalid("NI-DAQmx selected IEPE constructor is missing"))?;
        check(deadline, cutoff)?;
        let name = CString::new(request.physical.as_bytes())?;
        let code = invoke!(
            create,
            self.handle,
            name.as_ptr(),
            c"".as_ptr(),
            request.terminal,
            request.minimum,
            request.maximum,
            request.units,
            request.sensitivity,
            request.sensitivity_units,
            request.source,
            request.amperes,
            std::ptr::null()
        );
        record_status(&self.api, code, operation, deadline, notices)?;
        self.set_coupling(&name, request.coupling, deadline, cutoff, notices)
    }
    fn create_charge(
        &self,
        channel: &ChargeAccelerationChannel,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        let create = self.api.constructor.charge.ok_or_else(|| {
            wire::invalid("NI-DAQmx selected charge acceleration constructor is missing")
        })?;
        let request = charge_request(channel);
        check(deadline, cutoff)?;
        let name = CString::new(request.physical.as_bytes())?;
        let code = invoke!(
            create,
            self.handle,
            name.as_ptr(),
            c"".as_ptr(),
            request.terminal,
            request.minimum,
            request.maximum,
            request.units,
            request.sensitivity,
            request.sensitivity_units,
            std::ptr::null()
        );
        record_status(
            &self.api,
            code,
            "CreateAIAccelChargeChan",
            deadline,
            notices,
        )?;
        self.set_coupling(&name, request.coupling, deadline, cutoff, notices)
    }
    fn create_lvdt(
        &self,
        channel: &LvdtChannel,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        self.create_ac_position(
            lvdt_request(channel),
            self.api.constructor.lvdt,
            "CreateAIPosLVDTChan",
            deadline,
            cutoff,
            notices,
        )
    }
    fn create_rvdt(
        &self,
        channel: &RvdtChannel,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        self.create_ac_position(
            rvdt_request(channel),
            self.api.constructor.rvdt,
            "CreateAIPosRVDTChan",
            deadline,
            cutoff,
            notices,
        )
    }
    fn create_ac_position(
        &self,
        request: AcPositionRequest<'_>,
        constructor: Option<AcPositionConstructor>,
        operation: &'static str,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        let create = constructor
            .ok_or_else(|| wire::invalid("NI-DAQmx selected AC position constructor is missing"))?;
        check(deadline, cutoff)?;
        let name = CString::new(request.physical.as_bytes())?;
        let code = invoke!(
            create,
            self.handle,
            name.as_ptr(),
            c"".as_ptr(),
            request.minimum,
            request.maximum,
            request.units,
            request.sensitivity,
            request.sensitivity_units,
            request.source,
            request.volts,
            request.frequency_hz,
            request.wire_mode,
            std::ptr::null()
        );
        record_status(&self.api, code, operation, deadline, notices)?;
        self.set_coupling(&name, request.coupling, deadline, cutoff, notices)
    }
    fn create_resistance(
        &self,
        channel: &ResistanceChannel,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        let create =
            self.api.constructor.resistance.ok_or_else(|| {
                wire::invalid("NI-DAQmx selected resistance constructor is missing")
            })?;
        check(deadline, cutoff)?;
        let name = CString::new(channel.physical.as_bytes())?;
        let code = invoke!(
            create,
            self.handle,
            name.as_ptr(),
            c"".as_ptr(),
            channel.min_ohms,
            channel.max_ohms,
            OHMS,
            resistance_config(channel.resistance_config),
            excitation_source(channel.excitation.source),
            channel.excitation.amperes,
            std::ptr::null()
        );
        record_status(&self.api, code, "CreateAIResistanceChan", deadline, notices)?;
        self.set_coupling(&name, channel.coupling, deadline, cutoff, notices)
    }
    fn create_thermistor_current(
        &self,
        channel: &ThermistorCurrentChannel,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        let create = self.api.constructor.thermistor_current.ok_or_else(|| {
            wire::invalid("NI-DAQmx selected current thermistor constructor is missing")
        })?;
        check(deadline, cutoff)?;
        let name = CString::new(channel.physical.as_bytes())?;
        // Iex ends with A/B/C: there is no R1 or custom-scale slot.
        let code = invoke!(
            create,
            self.handle,
            name.as_ptr(),
            c"".as_ptr(),
            channel.min_temperature,
            channel.max_temperature,
            temperature_units(channel.units),
            resistance_config(channel.resistance_config),
            excitation_source(channel.excitation.source),
            channel.excitation.amperes,
            channel.coefficients.a,
            channel.coefficients.b,
            channel.coefficients.c
        );
        record_status(&self.api, code, "CreateAIThrmstrChanIex", deadline, notices)?;
        self.set_coupling(&name, channel.coupling, deadline, cutoff, notices)
    }
    fn create_thermistor_voltage(
        &self,
        channel: &ThermistorVoltageChannel,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        let create = self.api.constructor.thermistor_voltage.ok_or_else(|| {
            wire::invalid("NI-DAQmx selected voltage thermistor constructor is missing")
        })?;
        check(deadline, cutoff)?;
        let name = CString::new(channel.physical.as_bytes())?;
        // Vex adds its reference resistor after A/B/C, with no scale pointer.
        let code = invoke!(
            create,
            self.handle,
            name.as_ptr(),
            c"".as_ptr(),
            channel.min_temperature,
            channel.max_temperature,
            temperature_units(channel.units),
            resistance_config(channel.resistance_config),
            excitation_source(channel.excitation.source),
            channel.excitation.volts,
            channel.coefficients.a,
            channel.coefficients.b,
            channel.coefficients.c,
            channel.r1_ohms
        );
        record_status(&self.api, code, "CreateAIThrmstrChanVex", deadline, notices)?;
        self.set_coupling(&name, channel.coupling, deadline, cutoff, notices)
    }
    fn create_physical_bridge(
        &self,
        request: PhysicalBridgeRequest<'_>,
        create: Option<PhysicalBridgeConstructor>,
        operation: &'static str,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        let create = create.ok_or_else(|| {
            wire::invalid("NI-DAQmx selected physical bridge constructor is missing")
        })?;
        check(deadline, cutoff)?;
        let name = CString::new(request.physical.as_bytes())?;
        let calibration = request.calibration;
        // NI orders the two electrical coordinates together, then their unit,
        // then the two physical coordinates and their independent unit.
        let code = invoke!(
            create,
            self.handle,
            name.as_ptr(),
            c"".as_ptr(),
            request.minimum,
            request.maximum,
            request.output_units,
            request.bridge,
            request.source,
            request.volts,
            request.nominal_ohms,
            calibration.first_electrical,
            calibration.second_electrical,
            calibration.electrical_units,
            calibration.first_physical,
            calibration.second_physical,
            calibration.physical_units,
            std::ptr::null()
        );
        record_status(&self.api, code, operation, deadline, notices)?;
        self.set_coupling(&name, request.coupling, deadline, cutoff, notices)
    }
    fn create_strain(
        &self,
        channel: &StrainChannel,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        let create = self
            .api
            .constructor
            .strain
            .ok_or_else(|| wire::invalid("NI-DAQmx selected strain constructor is missing"))?;
        check(deadline, cutoff)?;
        let name = CString::new(channel.physical.as_bytes())?;
        let code = invoke!(
            create,
            self.handle,
            name.as_ptr(),
            c"".as_ptr(),
            channel.min_strain,
            channel.max_strain,
            STRAIN,
            strain_bridge_type(channel.bridge_type),
            excitation_source(channel.excitation.source),
            channel.excitation.volts,
            channel.gage_factor,
            channel.initial_bridge_volts,
            channel.nominal_gage_ohms,
            channel.poisson_ratio,
            channel.lead_wire_ohms,
            std::ptr::null()
        );
        record_status(&self.api, code, "CreateAIStrainGageChan", deadline, notices)?;
        self.set_coupling(&name, channel.coupling, deadline, cutoff, notices)
    }
    fn create_bridge(
        &self,
        channel: &BridgeRatioChannel,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        let create = self
            .api
            .constructor
            .bridge
            .ok_or_else(|| wire::invalid("NI-DAQmx selected bridge constructor is missing"))?;
        check(deadline, cutoff)?;
        let name = CString::new(channel.physical.as_bytes())?;
        let code = invoke!(
            create,
            self.handle,
            name.as_ptr(),
            c"".as_ptr(),
            channel.min_ratio,
            channel.max_ratio,
            bridge_units(channel.units),
            bridge_config(channel.bridge_config),
            excitation_source(channel.excitation.source),
            channel.excitation.volts,
            channel.nominal_bridge_ohms,
            std::ptr::null()
        );
        record_status(&self.api, code, "CreateAIBridgeChan", deadline, notices)?;
        self.set_coupling(&name, channel.coupling, deadline, cutoff, notices)
    }
    fn create_acceleration(
        &self,
        channel: &AccelerationChannel,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        let create = self.api.constructor.acceleration.ok_or_else(|| {
            wire::invalid("NI-DAQmx selected acceleration constructor is missing")
        })?;
        check(deadline, cutoff)?;
        let name = CString::new(channel.physical.as_bytes())?;
        let code = invoke!(
            create,
            self.handle,
            name.as_ptr(),
            c"".as_ptr(),
            terminal(channel.terminal_config),
            channel.min_acceleration,
            channel.max_acceleration,
            acceleration_units(channel.units),
            channel.sensitivity,
            sensitivity_units(channel.sensitivity_units),
            excitation_source(channel.excitation.source),
            channel.excitation.amperes,
            std::ptr::null()
        );
        record_status(&self.api, code, "CreateAIAccelChan", deadline, notices)?;
        self.set_coupling(&name, channel.coupling, deadline, cutoff, notices)
    }
    fn set_coupling(
        &self,
        name: &CStr,
        request: Option<Coupling>,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        if let Some(request) = request {
            let setter = self
                .api
                .constructor
                .set_coupling
                .ok_or_else(|| wire::invalid("NI-DAQmx selected coupling setter is missing"))?;
            check(deadline, cutoff)?;
            let code = invoke!(setter, self.handle, name.as_ptr(), coupling_code(request));
            record_status(&self.api, code, "SetAICoupling", deadline, notices)?;
        }
        Ok(())
    }
    fn create_period(
        &self,
        channel: &CounterPeriodChannel,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        let create = self
            .api
            .constructor
            .period
            .ok_or_else(|| wire::invalid("NI-DAQmx selected period constructor is missing"))?;
        check(deadline, cutoff)?;
        let name = CString::new(channel.physical.as_bytes())?;
        let code = invoke!(
            create,
            self.handle,
            name.as_ptr(),
            c"".as_ptr(),
            channel.min_seconds,
            channel.max_seconds,
            SECONDS,
            edge_code(channel.edge),
            period_method(channel.method),
            channel.measurement_time_seconds,
            channel.divisor,
            std::ptr::null()
        );
        record_status(&self.api, code, "CreateCIPeriodChan", deadline, notices)?;
        self.set_input_source(
            &name,
            InputSourceRequest {
                source: channel.input_terminal.as_deref(),
                setter: self.api.constructor.set_period_terminal,
                operation: "SetCIPeriodTerm",
            },
            deadline,
            cutoff,
            notices,
        )
    }
    fn create_pulse_width(
        &self,
        channel: &CounterPulseWidthChannel,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        let create =
            self.api.constructor.pulse_width.ok_or_else(|| {
                wire::invalid("NI-DAQmx selected pulse-width constructor is missing")
            })?;
        check(deadline, cutoff)?;
        let name = CString::new(channel.physical.as_bytes())?;
        let code = invoke!(
            create,
            self.handle,
            name.as_ptr(),
            c"".as_ptr(),
            channel.min_seconds,
            channel.max_seconds,
            SECONDS,
            edge_code(channel.starting_edge),
            std::ptr::null()
        );
        record_status(&self.api, code, "CreateCIPulseWidthChan", deadline, notices)?;
        self.set_input_source(
            &name,
            InputSourceRequest {
                source: channel.input_terminal.as_deref(),
                setter: self.api.constructor.set_pulse_width_terminal,
                operation: "SetCIPulseWidthTerm",
            },
            deadline,
            cutoff,
            notices,
        )
    }
    fn create_angular(
        &self,
        channel: &CounterAngularPositionChannel,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        let create = self
            .api
            .constructor
            .angular
            .ok_or_else(|| wire::invalid("NI-DAQmx selected angular constructor is missing"))?;
        check(deadline, cutoff)?;
        let name = CString::new(channel.physical.as_bytes())?;
        // Every explicit Z slot is retained even when disabled or ignored by
        // TwoPulse decoding. PPR is for one A/B signal, with no local multiplier.
        let z = z_arguments(channel.z_index);
        let code = invoke!(
            create,
            self.handle,
            name.as_ptr(),
            c"".as_ptr(),
            encoder_decoding(channel.decoding),
            z.enabled,
            z.value,
            z.phase,
            angular_units(channel.units),
            channel.pulses_per_rev,
            channel.initial_angle,
            std::ptr::null()
        );
        record_status(&self.api, code, "CreateCIAngEncoderChan", deadline, notices)?;
        self.set_encoder_sources(
            &name,
            [
                channel.a_input_terminal.as_deref(),
                channel.b_input_terminal.as_deref(),
                channel.z_input_terminal.as_deref(),
            ],
            deadline,
            cutoff,
            notices,
        )
    }
    fn create_linear(
        &self,
        channel: &CounterLinearPositionChannel,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        let create = self
            .api
            .constructor
            .linear
            .ok_or_else(|| wire::invalid("NI-DAQmx selected linear constructor is missing"))?;
        check(deadline, cutoff)?;
        let name = CString::new(channel.physical.as_bytes())?;
        let z = z_arguments(channel.z_index);
        let code = invoke!(
            create,
            self.handle,
            name.as_ptr(),
            c"".as_ptr(),
            encoder_decoding(channel.decoding),
            z.enabled,
            z.value,
            z.phase,
            linear_units(channel.units),
            channel.distance_per_pulse,
            channel.initial_position,
            std::ptr::null()
        );
        record_status(&self.api, code, "CreateCILinEncoderChan", deadline, notices)?;
        self.set_encoder_sources(
            &name,
            [
                channel.a_input_terminal.as_deref(),
                channel.b_input_terminal.as_deref(),
                channel.z_input_terminal.as_deref(),
            ],
            deadline,
            cutoff,
            notices,
        )
    }
    fn set_input_source(
        &self,
        name: &CStr,
        request: InputSourceRequest<'_>,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        if let Some(source) = request.source {
            let setter = request.setter.ok_or_else(|| {
                wire::invalid("NI-DAQmx selected input-terminal setter is missing")
            })?;
            check(deadline, cutoff)?;
            let source = CString::new(source.as_bytes())?;
            let code = invoke!(setter, self.handle, name.as_ptr(), source.as_ptr());
            record_status(&self.api, code, request.operation, deadline, notices)?;
        }
        Ok(())
    }
    fn set_encoder_sources(
        &self,
        name: &CStr,
        sources: [Option<&str>; 3],
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        let setters = [
            self.api.constructor.set_encoder_a,
            self.api.constructor.set_encoder_b,
            self.api.constructor.set_encoder_z,
        ];
        let operations = [
            "SetCIEncoderAInputTerm",
            "SetCIEncoderBInputTerm",
            "SetCIEncoderZInputTerm",
        ];
        for ((source, setter), operation) in sources.into_iter().zip(setters).zip(operations) {
            self.set_input_source(
                name,
                InputSourceRequest {
                    source,
                    setter,
                    operation,
                },
                deadline,
                cutoff,
                notices,
            )?;
        }
        Ok(())
    }
    fn create_digital(
        &self,
        physical: &str,
        index: usize,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        let create = self
            .api
            .constructor
            .digital
            .ok_or_else(|| wire::invalid("NI-DAQmx selected digital constructor is missing"))?;
        check(deadline, cutoff)?;
        let name = CString::new(physical.as_bytes())?;
        let assigned = CString::new(format!("cfdp{index:04}"))?;
        let code = invoke!(
            create,
            self.handle,
            name.as_ptr(),
            assigned.as_ptr(),
            ALL_LINES
        );
        record_status(&self.api, code, "CreateDIChan", deadline, notices)
    }
    fn create_count(
        &self,
        channel: &CounterCountChannel,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        let create = self
            .api
            .constructor
            .count
            .ok_or_else(|| wire::invalid("NI-DAQmx selected count constructor is missing"))?;
        check(deadline, cutoff)?;
        let name = CString::new(channel.physical.as_bytes())?;
        let code = invoke!(
            create,
            self.handle,
            name.as_ptr(),
            c"".as_ptr(),
            edge_code(channel.edge),
            channel.initial_count,
            count_direction(channel.direction)
        );
        record_status(&self.api, code, "CreateCICountEdgesChan", deadline, notices)?;
        if let Some(request) = &channel.input_terminal {
            let setter = self.api.constructor.set_count_terminal.ok_or_else(|| {
                wire::invalid("NI-DAQmx selected count input-terminal setter is missing")
            })?;
            check(deadline, cutoff)?;
            let terminal = CString::new(request.as_bytes())?;
            // Empty assigned virtual name preserves the requested physical
            // association. Do not rewrite either operator-supplied argument.
            let code = invoke!(setter, self.handle, name.as_ptr(), terminal.as_ptr());
            record_status(&self.api, code, "SetCICountEdgesTerm", deadline, notices)?;
        }
        Ok(())
    }
    fn create_frequency(
        &self,
        channel: &CounterFrequencyChannel,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        let create =
            self.api.constructor.frequency.ok_or_else(|| {
                wire::invalid("NI-DAQmx selected frequency constructor is missing")
            })?;
        check(deadline, cutoff)?;
        let name = CString::new(channel.physical.as_bytes())?;
        let code = invoke!(
            create,
            self.handle,
            name.as_ptr(),
            c"".as_ptr(),
            channel.min_hz,
            channel.max_hz,
            HERTZ,
            edge_code(channel.edge),
            frequency_method(channel.method),
            channel.measurement_time_seconds,
            channel.divisor,
            std::ptr::null()
        );
        record_status(&self.api, code, "CreateCIFreqChan", deadline, notices)?;
        if let Some(request) = &channel.input_terminal {
            let setter = self.api.constructor.set_frequency_terminal.ok_or_else(|| {
                wire::invalid("NI-DAQmx selected frequency input-terminal setter is missing")
            })?;
            check(deadline, cutoff)?;
            let terminal = CString::new(request.as_bytes())?;
            let code = invoke!(setter, self.handle, name.as_ptr(), terminal.as_ptr());
            record_status(&self.api, code, "SetCIFreqTerm", deadline, notices)?;
        }
        Ok(())
    }
    fn create_voltage(
        &self,
        channel: VoltageRequest<'_>,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        let create = self
            .api
            .constructor
            .voltage
            .ok_or_else(|| wire::invalid("NI-DAQmx selected voltage constructor is missing"))?;
        check(deadline, cutoff)?;
        let name = CString::new(channel.physical.as_bytes())?;
        let code = invoke!(
            create,
            self.handle,
            name.as_ptr(),
            c"".as_ptr(),
            terminal(channel.terminal_config),
            channel.min_volts,
            channel.max_volts,
            VOLTS,
            // Volts uses no custom scale, independently frozen in the NULL
            // extension oracle. An empty C string is not this pointer contract.
            std::ptr::null()
        );
        record_status(&self.api, code, "CreateAIVoltageChan", deadline, notices)
    }
    fn create_current(
        &self,
        channel: &CurrentChannel,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        let create = self
            .api
            .constructor
            .current
            .ok_or_else(|| wire::invalid("NI-DAQmx selected current constructor is missing"))?;
        check(deadline, cutoff)?;
        let name = CString::new(channel.physical.as_bytes())?;
        let (shunt_location, external_ohms) = current_shunt(channel.shunt);
        let code = invoke!(
            create,
            self.handle,
            name.as_ptr(),
            c"".as_ptr(),
            terminal(channel.terminal_config),
            channel.min_amperes,
            channel.max_amperes,
            AMPERES,
            shunt_location,
            external_ohms,
            // Amps requires a NULL custom-scale pointer, not an empty string.
            std::ptr::null()
        );
        record_status(&self.api, code, "CreateAICurrentChan", deadline, notices)
    }
    fn create_rtd(
        &self,
        channel: &RtdChannel,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        let create = self
            .api
            .constructor
            .rtd
            .ok_or_else(|| wire::invalid("NI-DAQmx selected RTD constructor is missing"))?;
        check(deadline, cutoff)?;
        let name = CString::new(channel.physical.as_bytes())?;
        let arguments = rtd_arguments(channel);
        let code = invoke!(
            create,
            self.handle,
            name.as_ptr(),
            c"".as_ptr(),
            arguments.minimum,
            arguments.maximum,
            arguments.units,
            arguments.kind,
            arguments.wiring,
            arguments.excitation_source,
            arguments.excitation_amperes,
            arguments.reference_ohms
        );
        record_status(&self.api, code, "CreateAIRTDChan", deadline, notices)
    }
    fn create_thermocouple(
        &self,
        channel: &ThermocoupleChannel,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        let create = self
            .api
            .constructor
            .thermocouple
            .ok_or_else(|| wire::invalid("NI-DAQmx selected TC constructor is missing"))?;
        check(deadline, cutoff)?;
        let name = CString::new(channel.physical.as_bytes())?;
        let (cjc_source, cjc_value) = cold_junction(channel.cjc);
        let code = invoke!(
            create,
            self.handle,
            name.as_ptr(),
            c"".as_ptr(),
            channel.min_temperature,
            channel.max_temperature,
            temperature_units(channel.units),
            thermocouple_type(channel.thermocouple_type),
            cjc_source,
            cjc_value,
            c"".as_ptr()
        );
        record_status(&self.api, code, "CreateAIThrmcplChan", deadline, notices)?;
        if let Some(mode) = terminal_request(channel.terminal_config) {
            let setter = self
                .api
                .constructor
                .set_terminal
                .ok_or_else(|| wire::invalid("NI-DAQmx selected terminal setter is missing"))?;
            check(deadline, cutoff)?;
            let code = invoke!(setter, self.handle, name.as_ptr(), mode);
            record_status(&self.api, code, "SetAITermCfg", deadline, notices)?;
        }
        Ok(())
    }
    fn channel_name(
        &self,
        index: u32,
        deadline: u64,
        cutoff: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<String> {
        check(deadline, cutoff)?;
        let required = invoke!(self.api.nth, self.handle, index, std::ptr::null_mut(), 0);
        if required < 0 {
            record_status(
                &self.api,
                required,
                "GetNthTaskReadChannel",
                deadline,
                notices,
            )?;
        }
        // Positive here is the documented required size, not a native warning.
        wire::check_deadline(deadline)?;
        let required = usize::try_from(required)
            .ok()
            .filter(|size| (2..=MAX_NAME_BYTES).contains(size))
            .ok_or_else(|| wire::invalid("NI-DAQmx returned an invalid channel-name extent"))?;
        let mut bytes = vec![0u8; required];
        check(deadline, cutoff)?;
        let code = invoke!(
            self.api.nth,
            self.handle,
            index,
            bytes.as_mut_ptr().cast(),
            i32::try_from(required)?
        );
        record_status(&self.api, code, "GetNthTaskReadChannel", deadline, notices)?;
        let nul = bytes
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| wire::invalid("NI-DAQmx returned an unterminated channel name"))?;
        if nul + 1 != required || !bytes[..nul].is_ascii() {
            return Err(wire::invalid(
                "NI-DAQmx returned an inconsistent ASCII channel name",
            ));
        }
        Ok(std::str::from_utf8(&bytes[..nul])?.to_owned())
    }
    pub(super) fn read(&mut self, deadline: u64, cutoff: u64) -> wire::Response {
        if let Some(error) = &self.first_failure {
            return wire::Response::Failed {
                error: error.clone(),
            };
        }
        match self.read_inner(deadline, cutoff) {
            Ok(response) => response,
            Err(error) => {
                let error = wire::Failure::from_error(&error);
                self.first_failure = Some(error.clone());
                wire::Response::Failed { error }
            }
        }
    }
    fn read_inner(&mut self, deadline: u64, cutoff: u64) -> Result<wire::Response> {
        let cutoff = cutoff.min(self.cutoff_ns);
        check(deadline, cutoff)?;
        let interval = self.config.timing.poll_interval_ns();
        if let Some(interval) = interval {
            let now = wire::now()?;
            if software_attempt(now, self.next_not_before_ns, interval, deadline, cutoff)?.is_none()
            {
                return Ok(self.software_idle(now));
            }
        }
        let mut notices = Vec::new();
        let available = self.api.clocked.map_or_else(
            || Ok(1),
            |clocked| -> Result<u32> {
                let mut available = 0;
                let code = invoke!(clocked.available, self.handle, &raw mut available);
                self.record_read_status(code, "GetReadAvailSampPerChan", deadline, &mut notices)?;
                check(deadline, cutoff)?;
                Ok(available)
            },
        )?;
        if available == 0 {
            return Ok(wire::Response::Read {
                observed_ns: wire::now()?,
                block: None,
                notices,
                terminal: None,
                pacing: None,
            });
        }
        let channels = self
            .descriptor
            .as_ref()
            .context("NI-DAQmx task descriptor is missing")?
            .channels
            .len();
        let request = available
            .min(self.config.max_samples_per_channel())
            .min(u32::try_from(MAX_WORDS / channels)?);
        let capacity = usize::try_from(request)?
            .checked_mul(channels)
            .context("NI-DAQmx read capacity overflow")?;
        let mut values = ReadBuffer::new(&self.config.measurement, capacity);
        let mut actual = 0i32;
        let request_i32 = i32::try_from(request)?;
        let capacity_u32 = u32::try_from(capacity)?;
        check(deadline, cutoff)?;
        let (pacing, timeout) = if let Some(interval) = interval {
            // This host boundary is immediately before the FFI attempt. It is
            // not an exact physical SDK-entry/conversion timestamp: the OS can
            // preempt after it. Allocation and argument checks precede it.
            let start = wire::now()?;
            let Some((pacing, timeout)) =
                software_attempt(start, self.next_not_before_ns, interval, deadline, cutoff)?
            else {
                return Ok(self.software_idle(start));
            };
            self.next_not_before_ns = pacing.next_not_before_ns;
            (Some(pacing), timeout)
        } else {
            (None, 0.0)
        };
        let code = self.api.read_api.read(
            self.handle,
            request_i32,
            timeout,
            capacity_u32,
            &mut values,
            &mut actual,
        )?;
        // Capture the real native-read completion frontier before diagnostic or
        // other NI calls; this is never a device/sample UTC timestamp.
        let observed = wire::now();
        // Publish the accepted words and raw code before any optional SDK
        // diagnostic can hang. Finish owns all deferred diagnostic calls.
        let notice = self
            .deferred
            .record(self.config.measurement.read_operation(), code);
        if let Some(notice) = &notice {
            notices.push(notice.clone());
        }
        let observed_ns =
            observed.map_err(|error| with_details(error, json!({"read_notices":notices})))?;
        let shape = valid_count(actual, request, channels, values.len());
        let in_time = observed_ns < deadline && observed_ns < cutoff;
        let failure = if !in_time {
            Some(expired())
        } else if shape.is_err() {
            Some(invalid_read_geometry(
                "NI-DAQmx returned an invalid per-channel sample count",
                code,
                actual,
                request,
            ))
        } else if code < 0 {
            notice.as_ref().map(native_error)
        } else if interval.is_some() && actual == 0 {
            Some(invalid_read_geometry(
                "NI-DAQmx successful on-demand read returned no sample",
                code,
                actual,
                request,
            ))
        } else {
            None
        };
        let words = shape.unwrap_or(0);
        let block = if in_time && words > 0 {
            let count = u32::try_from(actual)?;
            let next = self
                .sample_index
                .checked_add(u64::from(count))
                .context("NI-DAQmx sample index overflow")?;
            let block = wire::Block {
                sample_index: self.sample_index,
                samples_per_channel: count,
                bits_le_hex: values.prefix_hex(words)?,
                status_code: code,
            };
            self.sample_index = next;
            Some(block)
        } else {
            None
        };
        let failure = failure.or_else(|| wire::check_deadline(deadline).err());
        let terminal = failure.map(|error| {
            wire::Failure::from_error(&with_details(error, json!({"read_notices":notices})))
        });
        if let Some(error) = &terminal {
            self.first_failure = Some(error.clone());
        }
        Ok(wire::Response::Read {
            observed_ns,
            block,
            notices,
            terminal,
            pacing,
        })
    }
    fn software_idle(&self, observed_ns: u64) -> wire::Response {
        wire::Response::Read {
            observed_ns,
            block: None,
            notices: Vec::new(),
            terminal: None,
            pacing: Some(wire::Pacing {
                attempt_start_ns: None,
                next_not_before_ns: self.next_not_before_ns,
            }),
        }
    }
    fn record_read_status(
        &mut self,
        code: i32,
        operation: &'static str,
        deadline: u64,
        notices: &mut Vec<wire::Notice>,
    ) -> Result<()> {
        if let Some(notice) = self.deferred.record(operation, code) {
            let failure = (code < 0).then(|| native_error(&notice));
            notices.push(notice);
            if let Some(error) = failure {
                return Err(with_details(error, json!({"read_notices":notices})));
            }
        }
        wire::check_deadline(deadline)
            .map_err(|error| with_details(error, json!({"read_notices":notices})))
    }
    fn cleanup(&mut self, deadline: u64) -> (Vec<wire::Notice>, Option<wire::Failure>) {
        let mut notices = Vec::new();
        let mut failure = None;
        if self.handle.is_null() {
            return (notices, failure);
        }
        if self.start_attempted {
            let result = wire::check_deadline(deadline).and_then(|()| {
                let code = invoke!(self.api.stop, self.handle);
                if code >= 0 {
                    self.start_attempted = false;
                }
                record_status(&self.api, code, "StopTask", deadline, &mut notices)
            });
            if let Err(error) = result {
                failure = Some(wire::Failure::from_error(&error));
            }
        }
        let result = wire::check_deadline(deadline).and_then(|()| {
            let code = invoke!(self.api.clear, self.handle);
            if code >= 0 {
                self.handle = std::ptr::null_mut();
                self.start_attempted = false;
            }
            record_status(&self.api, code, "ClearTask", deadline, &mut notices)
        });
        if let Err(error) = result {
            let error = match &failure {
                Some(first) => with_details(
                    first.error(),
                    json!({"clear_error":wire::Failure::from_error(&error)}),
                ),
                None => error,
            };
            failure = Some(wire::Failure::from_error(&error));
        }
        (notices, failure)
    }
    pub(super) fn finish(&mut self, deadline: u64) -> wire::Response {
        let mut deferred = Vec::new();
        let mut diagnostic_failure = None;
        // This must precede even StopTask: another NI call can replace the
        // serial worker thread's last native error. No subsequent read occurred.
        if let Some(pending) = self.deferred.negative.take() {
            match wire::check_deadline(deadline) {
                Ok(()) => {
                    deferred.push(diagnostic(
                        &self.api,
                        pending.operation,
                        pending.code,
                        deadline,
                    ));
                    if let Err(error) = wire::check_deadline(deadline) {
                        diagnostic_failure = Some(wire::Failure::from_error(&error));
                    }
                }
                Err(error) => {
                    deferred.push(pending.placeholder());
                    diagnostic_failure = Some(wire::Failure::from_error(&error));
                }
            }
        }
        let negative_count = deferred.len();
        let (cleanup_notices, cleanup) = self.cleanup(deadline);
        // Positive codes are explicit ErrorString lookups. Do these only after
        // owned task cleanup, so optional warning text cannot starve Stop/Clear.
        for pending in self.deferred.warnings.drain(..) {
            if wire::check_deadline(deadline).is_err() {
                break;
            }
            deferred.push(diagnostic(
                &self.api,
                pending.operation,
                pending.code,
                deadline,
            ));
            if let Err(error) = wire::check_deadline(deadline) {
                diagnostic_failure = Some(wire::Failure::from_error(&error));
                break;
            }
        }
        let terminal = self
            .first_failure
            .as_ref()
            .or(cleanup.as_ref())
            .or(diagnostic_failure.as_ref())
            .map(|first| {
                let mut details = cleanup_details(&cleanup_notices, cleanup.as_ref());
                details["deferred_diagnostics"] = json!(deferred);
                if let Some(error) = &diagnostic_failure {
                    details["diagnostic_error"] = json!(error);
                }
                wire::Failure::from_error(&with_details(first.error(), details))
            });
        let warnings = deferred.split_off(negative_count);
        let mut notices = deferred;
        notices.extend(cleanup_notices);
        notices.extend(warnings);
        if let Some(error) = &terminal {
            self.first_failure = Some(error.clone());
        }
        match wire::now() {
            Ok(observed_ns) => wire::Response::Finished {
                observed_ns,
                notices,
                terminal,
            },
            Err(error) => wire::Response::Failed {
                error: self
                    .first_failure
                    .clone()
                    .unwrap_or_else(|| wire::Failure::from_error(&error)),
            },
        }
    }
}

fn valid_count(actual: i32, requested: u32, channels: usize, capacity: usize) -> Result<usize> {
    let actual = u32::try_from(actual)
        .ok()
        .filter(|actual| *actual <= requested)
        .ok_or_else(|| wire::invalid("NI-DAQmx returned an invalid per-channel sample count"))?;
    let words = usize::try_from(actual)?
        .checked_mul(channels)
        .filter(|words| *words <= capacity && *words <= MAX_WORDS)
        .ok_or_else(|| wire::invalid("NI-DAQmx returned an invalid sample capacity"))?;
    Ok(words)
}

fn cleanup_details(notices: &[wire::Notice], failure: Option<&wire::Failure>) -> serde_json::Value {
    let mut details = json!({"cleanup_notices":notices});
    if let Some(failure) = failure {
        details["cleanup_error"] = json!(failure);
    }
    details
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_count_is_per_channel_and_never_exposes_an_invalid_prefix() {
        assert_eq!(valid_count(1, 3, 2, 6).unwrap(), 2);
        assert_eq!(valid_count(0, 3, 2, 6).unwrap(), 0);
        valid_count(-1, 3, 2, 6).unwrap_err();
        valid_count(4, 3, 2, 6).unwrap_err();
        valid_count(3, 3, 2, 5).unwrap_err();
        valid_count(4096, 4096, 2, 8192).unwrap_err();
    }

    #[test]
    fn software_timeout_uses_original_remaining_and_expiry_never_admits() {
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/nidaqmx/on-demand.json"
        ))
        .unwrap();
        let control = &oracle["timeout_arithmetic_control"];
        let deadline = control["original_deadline_ns"].as_u64().unwrap();
        let cutoff = control["cutoff_ns"].as_u64().unwrap();
        let start = control["attempt_start_ns"].as_u64().unwrap();
        let interval = oracle["host_attempt_pacing"]["interval_control_ns"]
            .as_u64()
            .unwrap();
        let (pacing, timeout) = software_attempt(start, 0, interval, deadline, cutoff)
            .unwrap()
            .unwrap();
        assert_eq!(pacing.attempt_start_ns, Some(start));
        assert_eq!(pacing.next_not_before_ns, start + interval);
        assert_eq!(
            format!("{:016x}", timeout.to_bits()),
            control["timeout_bits_hex"].as_str().unwrap()
        );
        let (_, clipped) = software_attempt(start, 0, interval, cutoff, deadline)
            .unwrap()
            .unwrap();
        assert_eq!(clipped.to_bits(), timeout.to_bits());
        for expired in control["expiry_controls"].as_array().unwrap() {
            let now = expired["now_ns"].as_u64().unwrap();
            let error = software_attempt(now, 0, interval, deadline, cutoff).unwrap_err();
            assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
        }
        let overflow = software_attempt(u64::MAX - 4, 0, 10, u64::MAX, u64::MAX).unwrap_err();
        assert_eq!(
            crate::types::classify(&overflow).kind,
            ErrorKind::Incomplete
        );
    }

    #[test]
    fn software_attempt_frontier_preserves_early_idle_and_skips_catchup_bursts() {
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/nidaqmx/on-demand.json"
        ))
        .unwrap();
        let control = &oracle["host_attempt_pacing"];
        let interval = control["interval_control_ns"].as_u64().unwrap();
        let mut next = 0;
        let mut reads = 0u64;
        for event in control["trace_control"].as_array().unwrap() {
            let start = event["now_ns"].as_u64().unwrap();
            if event["event"] != "ready" {
                let attempt =
                    software_attempt(start, next, interval, 10_000_000_000, 10_000_000_000)
                        .unwrap();
                match attempt {
                    Some((pacing, _)) => {
                        assert_eq!(pacing.attempt_start_ns, Some(start));
                        assert_eq!(event["attempt_start_ns"].as_u64(), Some(start));
                        next = pacing.next_not_before_ns;
                        reads += 1;
                    }
                    None => assert_eq!(event["attempt_start_ns"], serde_json::Value::Null),
                }
            }
            assert_eq!(next, event["next_not_before_ns"].as_u64().unwrap());
            assert_eq!(reads, event["sdk_reads"].as_u64().unwrap());
        }
    }

    #[test]
    fn invalid_native_geometry_keeps_zero_status_and_signed_actual_count() {
        for (status, actual) in [(0, 0), (12_345, 0), (0, -1), (0, 2)] {
            let error = invalid_read_geometry("invalid native geometry", status, actual, 1);
            let tagged = crate::types::classify(&error);
            assert_eq!(tagged.kind, ErrorKind::Incomplete);
            let details = tagged.details.unwrap();
            assert_eq!(details["ni_daqmx_status"], status);
            assert_eq!(details["actual_samples_per_channel"], actual);
            assert_eq!(details["requested_samples_per_channel"], 1);
        }
    }

    #[test]
    fn fixed_terminal_constants_match_independent_abi_input() {
        let oracle: serde_json::Value =
            serde_json::from_str(include_str!("../../../tests/fixtures/nidaqmx/abi.json")).unwrap();
        for (config, name) in [
            (TerminalConfig::Default, "terminal_default"),
            (TerminalConfig::Rse, "terminal_rse"),
            (TerminalConfig::Nrse, "terminal_nrse"),
            (TerminalConfig::Differential, "terminal_differential"),
            (
                TerminalConfig::Pseudodifferential,
                "terminal_pseudodifferential",
            ),
        ] {
            assert_eq!(json!(terminal(config)), oracle["constants"][name]);
        }
        assert_eq!(
            json!(DO_NOT_OVERWRITE),
            oracle["constants"]["do_not_overwrite_unread_samples"]
        );
        assert_eq!(
            json!(GROUP_BY_SCAN),
            oracle["constants"]["group_by_scan_number"]
        );
    }

    #[test]
    fn current_units_terminals_and_shunts_match_independently_frozen_arguments() {
        let oracle: serde_json::Value =
            serde_json::from_str(include_str!("../../../tests/fixtures/nidaqmx/current.json"))
                .unwrap();
        assert_eq!(json!(AMPERES), oracle["constants"]["amperes"]);
        for (name, expected) in oracle["constants"]["terminal_config"].as_object().unwrap() {
            let mode: TerminalConfig = serde_json::from_value(json!(name)).unwrap();
            assert_eq!(&json!(terminal(mode)), expected);
        }
        for (shunt, expected) in [
            (
                CurrentShunt::BuiltIn,
                &oracle["two_channel_constructor_arguments"][0],
            ),
            (
                CurrentShunt::External { ohms: 250.0 },
                &oracle["two_channel_constructor_arguments"][1],
            ),
        ] {
            let (location, external_argument) = current_shunt(shunt);
            assert_eq!(json!(location), expected["shunt_location_code"]);
            assert_eq!(
                format!("{:016x}", external_argument.to_bits()),
                expected["external_argument_bits_hex"].as_str().unwrap()
            );
        }
    }

    #[test]
    fn current_shunt_source_is_explicit_and_constructor_accepts_default_terminal() {
        let (built_in, convention) = current_shunt(CurrentShunt::BuiltIn);
        let (external, requested) = current_shunt(CurrentShunt::External { ohms: 249.0 });
        assert_ne!(built_in, external);
        assert_eq!(convention.to_bits(), requested.to_bits());
        let value = f64::from_bits(0x406f_4000_0000_0001);
        let (_, argument) = current_shunt(CurrentShunt::External { ohms: value });
        assert_eq!(argument.to_bits(), value.to_bits());
        assert_eq!(terminal(TerminalConfig::Default), -1);
        assert_eq!(terminal_request(Some(TerminalConfig::Default)), None);
    }

    #[test]
    fn thermocouple_type_unit_and_cjc_arguments_match_independently_frozen_constants() {
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/nidaqmx/thermocouple.json"
        ))
        .unwrap();
        for (name, expected) in oracle["constants"]["thermocouple_types"]
            .as_object()
            .unwrap()
        {
            let kind: ThermocoupleType = serde_json::from_value(json!(name)).unwrap();
            assert_eq!(&json!(thermocouple_type(kind)), expected);
        }
        for (name, expected) in oracle["constants"]["temperature_units"]
            .as_object()
            .unwrap()
        {
            let units: TemperatureUnits = serde_json::from_value(json!(name)).unwrap();
            assert_eq!(&json!(temperature_units(units)), expected);
        }
        let (code, dummy) = cold_junction(ColdJunction::BuiltIn);
        assert_eq!(json!(code), oracle["constants"]["cjc_sources"]["built_in"]);
        assert_eq!(
            format!("{:016x}", dummy.to_bits()),
            oracle["two_channel_arguments"][0]["cjc"]["abi_dummy_bits_hex"]
                .as_str()
                .unwrap()
        );
        let value = oracle["two_channel_arguments"][1]["cjc"]["value"]
            .as_f64()
            .unwrap();
        let (code, constant) = cold_junction(ColdJunction::Constant { value });
        assert_eq!(json!(code), oracle["constants"]["cjc_sources"]["constant"]);
        assert_eq!(constant.to_bits(), value.to_bits());
        assert_eq!(
            format!("{:016x}", constant.to_bits()),
            oracle["two_channel_arguments"][1]["cjc"]["bits_hex"]
                .as_str()
                .unwrap()
        );
    }

    #[test]
    fn thermocouple_terminal_setter_never_receives_the_voltage_default_sentinel() {
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/nidaqmx/thermocouple.json"
        ))
        .unwrap();
        assert_eq!(terminal_request(None), None);
        assert_eq!(terminal_request(Some(TerminalConfig::Default)), None);
        for (name, expected) in oracle["constants"]["explicit_terminal_setter"]
            .as_object()
            .unwrap()
        {
            let mode: TerminalConfig = serde_json::from_value(json!(name)).unwrap();
            assert_eq!(&json!(terminal_request(Some(mode)).unwrap()), expected);
        }
        assert_eq!(terminal(TerminalConfig::Default), -1);
    }

    #[test]
    fn rtd_type_units_wiring_and_excitation_match_independently_frozen_enums() {
        let oracle: serde_json::Value =
            serde_json::from_str(include_str!("../../../tests/fixtures/nidaqmx/rtd.json")).unwrap();
        for (name, expected) in oracle["enums"]["rtd_type"].as_object().unwrap() {
            let kind: RtdType = serde_json::from_value(json!(name)).unwrap();
            assert_eq!(&json!(rtd_type(kind)), expected);
        }
        for (name, expected) in oracle["enums"]["temperature_units"].as_object().unwrap() {
            let units: TemperatureUnits = serde_json::from_value(json!(name)).unwrap();
            assert_eq!(&json!(temperature_units(units)), expected);
        }
        for (name, expected) in oracle["enums"]["resistance_config"].as_object().unwrap() {
            let wiring: ResistanceConfig = serde_json::from_value(json!(name)).unwrap();
            assert_eq!(&json!(resistance_config(wiring)), expected);
        }
        for (name, excitation) in [
            (
                "internal",
                CurrentExcitation::Internal {
                    amperes: 1.0 / 1024.0,
                },
            ),
            (
                "external",
                CurrentExcitation::External {
                    amperes: 1.0 / 512.0,
                },
            ),
        ] {
            let (source, amperes) = current_excitation(excitation);
            assert_eq!(json!(source), oracle["enums"]["excitation_source"][name]);
            let word_name = format!("{name}_current");
            let word = oracle["manual_argument_words"]
                .as_array()
                .unwrap()
                .iter()
                .find(|word| word["name"] == word_name)
                .unwrap();
            assert_eq!(
                format!("{:016x}", amperes.to_bits()),
                word["bits_hex"].as_str().unwrap()
            );
        }
    }

    #[test]
    fn rtd_constructor_arguments_preserve_asymmetric_requests_and_r0_at_zero_celsius() {
        let oracle: serde_json::Value =
            serde_json::from_str(include_str!("../../../tests/fixtures/nidaqmx/rtd.json")).unwrap();
        let word = |name: &serde_json::Value| {
            let expected = oracle["manual_argument_words"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["name"] == *name)
                .unwrap();
            f64::from_bits(u64::from_str_radix(expected["bits_hex"].as_str().unwrap(), 16).unwrap())
        };
        for control in oracle["asymmetric_two_channel_constructor_controls"]
            .as_array()
            .unwrap()
        {
            let amperes = word(&control["excitation_word"]);
            let excitation = match control["excitation_source"].as_str().unwrap() {
                "internal" => CurrentExcitation::Internal { amperes },
                "external" => CurrentExcitation::External { amperes },
                _ => unreachable!(),
            };
            let channel = RtdChannel {
                physical: control["physical"].as_str().unwrap().into(),
                rtd_type: serde_json::from_value(control["rtd_type"].clone()).unwrap(),
                units: serde_json::from_value(control["units"].clone()).unwrap(),
                min_temperature: word(&control["min_word"]),
                max_temperature: word(&control["max_word"]),
                resistance_config: serde_json::from_value(control["resistance_config"].clone())
                    .unwrap(),
                r0_ohms: word(&control["r0_word"]),
                excitation,
            };
            let arguments = rtd_arguments(&channel);
            assert_eq!(
                arguments.minimum.to_bits(),
                word(&control["min_word"]).to_bits()
            );
            assert_eq!(
                arguments.maximum.to_bits(),
                word(&control["max_word"]).to_bits()
            );
            assert_eq!(json!(arguments.units), control["units_code"]);
            assert_eq!(json!(arguments.kind), control["rtd_code"]);
            assert_eq!(json!(arguments.wiring), control["resistance_code"]);
            assert_eq!(
                json!(arguments.excitation_source),
                control["excitation_source_code"]
            );
            assert_eq!(arguments.excitation_amperes.to_bits(), amperes.to_bits());
            assert_eq!(
                arguments.reference_ohms.to_bits(),
                word(&control["r0_word"]).to_bits()
            );
        }
    }

    fn mixed_oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/nidaqmx/mixed-ai.json"
        ))
        .unwrap()
    }
    fn mixed_argument(oracle: &serde_json::Value, name: &serde_json::Value) -> f64 {
        let word = oracle["manual_argument_words"]
            .as_array()
            .unwrap()
            .iter()
            .find(|word| word["name"] == *name)
            .unwrap();
        f64::from_bits(u64::from_str_radix(word["bits_hex"].as_str().unwrap(), 16).unwrap())
    }
    fn mixed_control(oracle: &serde_json::Value) -> Measurement {
        let controls = oracle["ordered_four_channel_constructor_controls"]
            .as_array()
            .unwrap();
        Measurement::Mixed {
            channels: vec![
                MixedChannel::Voltage(VoltageChannel {
                    physical: controls[0]["physical"].as_str().unwrap().into(),
                    terminal_config: TerminalConfig::Differential,
                    min_volts: mixed_argument(oracle, &controls[0]["min_word"]),
                    max_volts: mixed_argument(oracle, &controls[0]["max_word"]),
                }),
                MixedChannel::Thermocouple(ThermocoupleChannel {
                    physical: controls[1]["physical"].as_str().unwrap().into(),
                    thermocouple_type: ThermocoupleType::K,
                    units: TemperatureUnits::DegC,
                    min_temperature: mixed_argument(oracle, &controls[1]["min_word"]),
                    max_temperature: mixed_argument(oracle, &controls[1]["max_word"]),
                    cjc: ColdJunction::BuiltIn,
                    terminal_config: None,
                }),
                MixedChannel::Current(CurrentChannel {
                    physical: controls[2]["physical"].as_str().unwrap().into(),
                    terminal_config: TerminalConfig::Nrse,
                    min_amperes: mixed_argument(oracle, &controls[2]["min_word"]),
                    max_amperes: mixed_argument(oracle, &controls[2]["max_word"]),
                    shunt: CurrentShunt::External {
                        ohms: mixed_argument(oracle, &controls[2]["external_ohms_word"]),
                    },
                }),
                MixedChannel::Rtd(RtdChannel {
                    physical: controls[3]["physical"].as_str().unwrap().into(),
                    rtd_type: RtdType::Pt3928,
                    units: TemperatureUnits::DegF,
                    min_temperature: mixed_argument(oracle, &controls[3]["min_word"]),
                    max_temperature: mixed_argument(oracle, &controls[3]["max_word"]),
                    resistance_config: ResistanceConfig::Four,
                    r0_ohms: mixed_argument(oracle, &controls[3]["r0_word"]),
                    excitation: CurrentExcitation::External {
                        amperes: mixed_argument(oracle, &controls[3]["excitation_amperes_word"]),
                    },
                }),
            ],
        }
    }
    #[test]
    fn mixed_constructor_union_and_only_nondefault_tc_setter_follow_frozen_selection() {
        let oracle = mixed_oracle();
        for (index, request) in [
            None,
            Some(TerminalConfig::Default),
            Some(TerminalConfig::Differential),
        ]
        .into_iter()
        .enumerate()
        {
            let mut measurement = mixed_control(&oracle);
            let Measurement::Mixed { channels } = &mut measurement else {
                unreachable!()
            };
            let MixedChannel::Thermocouple(tc) = &mut channels[1] else {
                unreachable!()
            };
            tc.terminal_config = request;
            let selected = ConstructorSelection::for_measurement(&measurement);
            let mut names = [
                (ConstructorKind::Voltage, "DAQmxCreateAIVoltageChan"),
                (ConstructorKind::Thermocouple, "DAQmxCreateAIThrmcplChan"),
                (ConstructorKind::Current, "DAQmxCreateAICurrentChan"),
                (ConstructorKind::Rtd, "DAQmxCreateAIRTDChan"),
            ]
            .into_iter()
            .filter_map(|(kind, name)| selected.contains(kind).then_some(name))
            .collect::<Vec<_>>();
            names.sort_unstable();
            assert_eq!(
                json!(names),
                oracle["selected_exports"]["resolve_before_create"]
            );
            assert_eq!(
                json!(selected.contains(ConstructorKind::Terminal)),
                oracle["selected_exports"]["tc_variants"][index]["setter_required"]
            );
        }
    }
    #[test]
    fn same_family_mixed_voltage_keeps_individual_ranges_and_no_unselected_exports() {
        let oracle = mixed_oracle();
        let controls = oracle["same_family_override_control"]["channels"]
            .as_array()
            .unwrap();
        let channels = [TerminalConfig::Differential, TerminalConfig::Rse]
            .into_iter()
            .zip(controls)
            .map(|(terminal_config, control)| {
                MixedChannel::Voltage(VoltageChannel {
                    physical: control["physical"].as_str().unwrap().into(),
                    terminal_config,
                    min_volts: mixed_argument(&oracle, &control["min_word"]),
                    max_volts: mixed_argument(&oracle, &control["max_word"]),
                })
            })
            .collect();
        let measurement = Measurement::Mixed { channels };
        let selected = ConstructorSelection::for_measurement(&measurement);
        assert!(selected.contains(ConstructorKind::Voltage));
        for absent in [
            ConstructorKind::Thermocouple,
            ConstructorKind::Current,
            ConstructorKind::Rtd,
            ConstructorKind::Terminal,
        ] {
            assert!(!selected.contains(absent));
        }
        let Measurement::Mixed { channels } = &measurement else {
            unreachable!()
        };
        for (channel, control) in channels.iter().zip(controls) {
            let MixedChannel::Voltage(channel) = channel else {
                unreachable!()
            };
            let request = voltage_request(channel);
            assert_eq!(request.physical, control["physical"].as_str().unwrap());
            assert_eq!(
                json!(terminal(request.terminal_config)),
                control["terminal_config_code"]
            );
            assert_eq!(
                request.min_volts.to_bits(),
                mixed_argument(&oracle, &control["min_word"]).to_bits()
            );
            assert_eq!(
                request.max_volts.to_bits(),
                mixed_argument(&oracle, &control["max_word"]).to_bits()
            );
        }
    }

    #[test]
    fn deferred_warning_text_is_bounded_without_losing_each_raw_read_code() {
        let mut pending = DeferredDiagnostics::default();
        for code in 1..=i32::try_from(MAX_DEFERRED_WARNINGS + 3).unwrap() {
            let notice = pending.record("ReadAnalogF64", code).unwrap();
            assert_eq!(notice.status_code, code);
            let scheduled = code <= i32::try_from(MAX_DEFERRED_WARNINGS).unwrap();
            assert_eq!(
                notice.diagnostic_source,
                if scheduled { "deferred" } else { "unavailable" }
            );
            assert_eq!(notice.diagnostic_hex, "");
            assert_eq!(notice.diagnostic_truncated, !scheduled);
            assert_eq!(notice.diagnostic_lookup_status, 0);
        }
        assert_eq!(pending.warnings.len(), MAX_DEFERRED_WARNINGS);
        let duplicate = pending.record("ReadAnalogF64", 1).unwrap();
        assert_eq!(duplicate.diagnostic_source, "deferred");
        assert!(!duplicate.diagnostic_truncated);
        let negative = pending.record("ReadAnalogF64", -200_284).unwrap();
        assert_eq!(negative.status_code, -200_284);
        assert_eq!(negative.diagnostic_source, "deferred");
        assert_eq!(pending.negative.unwrap().code, -200_284);
        assert_eq!(pending.warnings.len() + 1 + 2, wire::MAX_NOTICES);
        assert!(pending.record("ReadAnalogF64", 0).is_none());
    }

    #[test]
    fn deferred_warning_identity_includes_operation_and_terminal_error_has_its_own_slot() {
        let mut pending = DeferredDiagnostics::default();
        pending.record("GetReadAvailSampPerChan", 12_345).unwrap();
        pending.record("GetReadAvailSampPerChan", 12_345).unwrap();
        pending.record("ReadAnalogF64", 12_345).unwrap();
        assert_eq!(pending.warnings.len(), 2);
        pending.record("GetReadAvailSampPerChan", -200_279).unwrap();
        assert_eq!(
            pending.negative.unwrap().operation,
            "GetReadAvailSampPerChan"
        );
        assert_eq!(pending.negative.unwrap().code, -200_279);
        assert_eq!(pending.warnings.len(), 2);
        assert_eq!(
            native_error(&pending.negative.unwrap().placeholder())
                .downcast_ref::<Categorized>()
                .unwrap()
                .kind,
            ErrorKind::Incomplete
        );
    }

    fn digital_input_oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/nidaqmx/digital-port.json"
        ))
        .unwrap()
    }
    fn counter_input_oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/nidaqmx/counter-input.json"
        ))
        .unwrap()
    }
    fn count_input_control(oracle: &serde_json::Value, index: usize) -> CounterCountChannel {
        let value = &oracle["count_constructor_controls"][index];
        CounterCountChannel {
            physical: value["physical"].as_str().unwrap().into(),
            edge: serde_json::from_value(value["edge"].clone()).unwrap(),
            initial_count: u32::try_from(value["initial_count"].as_u64().unwrap()).unwrap(),
            direction: serde_json::from_value(value["direction"].clone()).unwrap(),
            input_terminal: value
                .get("input_terminal")
                .map(|v| v.as_str().unwrap().into()),
        }
    }
    fn frequency_input_control(oracle: &serde_json::Value) -> CounterFrequencyChannel {
        let control = &oracle["frequency_constructor_control"];
        let word = |name: &str| {
            let value = oracle["manual_argument_words"]
                .as_array()
                .unwrap()
                .iter()
                .find(|value| value["name"] == name)
                .unwrap();
            f64::from_bits(u64::from_str_radix(value["bits_hex"].as_str().unwrap(), 16).unwrap())
        };
        CounterFrequencyChannel {
            physical: control["physical"].as_str().unwrap().into(),
            min_hz: word(control["min_word"].as_str().unwrap()),
            max_hz: word(control["max_word"].as_str().unwrap()),
            edge: serde_json::from_value(control["edge"].clone()).unwrap(),
            method: serde_json::from_value(control["method"].clone()).unwrap(),
            measurement_time_seconds: word(control["measurement_time_word"].as_str().unwrap()),
            divisor: u32::try_from(control["divisor"].as_u64().unwrap()).unwrap(),
            input_terminal: None,
        }
    }

    #[test]
    fn input_constructor_selection_excludes_unselected_ai_and_terminal_exports() {
        let digital = digital_input_oracle();
        let counter = counter_input_oracle();
        let ports = digital["constructor_and_identity_controls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|control| control["requested_physical"].as_str().unwrap().to_owned())
            .collect();
        let frequency = frequency_input_control(&counter);
        let mut explicit_frequency = frequency.clone();
        explicit_frequency.input_terminal = Some(
            counter["input_terminal_controls"]["frequency_explicit"]["data"]
                .as_str()
                .unwrap()
                .into(),
        );
        let all = [
            ConstructorKind::Voltage,
            ConstructorKind::Thermocouple,
            ConstructorKind::Current,
            ConstructorKind::Rtd,
            ConstructorKind::Terminal,
            ConstructorKind::Digital,
            ConstructorKind::Count,
            ConstructorKind::Frequency,
            ConstructorKind::CountTerminal,
            ConstructorKind::FrequencyTerminal,
        ];
        for (measurement, constructor, setter) in [
            (
                Measurement::DigitalPort { channels: ports },
                ConstructorKind::Digital,
                None,
            ),
            (
                Measurement::CounterCount {
                    channels: vec![count_input_control(&counter, 0)],
                },
                ConstructorKind::Count,
                None,
            ),
            (
                Measurement::CounterCount {
                    channels: vec![count_input_control(&counter, 1)],
                },
                ConstructorKind::Count,
                Some(ConstructorKind::CountTerminal),
            ),
            (
                Measurement::CounterFrequency {
                    channels: vec![frequency],
                },
                ConstructorKind::Frequency,
                None,
            ),
            (
                Measurement::CounterFrequency {
                    channels: vec![explicit_frequency],
                },
                ConstructorKind::Frequency,
                Some(ConstructorKind::FrequencyTerminal),
            ),
        ] {
            let selected = ConstructorSelection::for_measurement(&measurement);
            for kind in all {
                assert_eq!(
                    selected.contains(kind),
                    kind as u8 == constructor as u8
                        || setter.is_some_and(|expected| kind as u8 == expected as u8)
                );
            }
            let expected_read = match constructor {
                ConstructorKind::Digital => &digital["read_control"]["selected_export"],
                ConstructorKind::Count => &counter["read_controls"]["count"]["selected_export"],
                ConstructorKind::Frequency => {
                    &counter["read_controls"]["frequency"]["selected_export"]
                }
                _ => unreachable!(),
            };
            assert_eq!(
                format!("DAQmx{}", measurement.read_operation()),
                expected_read.as_str().unwrap()
            );
            let buffer = ReadBuffer::new(&measurement, measurement.channel_count());
            assert_eq!(buffer.len(), measurement.channel_count());
            assert_eq!(
                buffer.prefix_hex(buffer.len()).unwrap().len(),
                buffer.len() * measurement.word_bytes() * 2
            );
        }
        assert_eq!(json!(ALL_LINES), digital["constants"]["ChanForAllLines"]);
        assert_eq!(
            json!(GROUP_BY_SCAN),
            digital["constants"]["GroupByScanNumber"]
        );
    }

    #[test]
    fn counter_codes_and_asymmetric_constructor_values_follow_frozen_controls() {
        let oracle = counter_input_oracle();
        for index in 0..2 {
            let channel = count_input_control(&oracle, index);
            let control = &oracle["count_constructor_controls"][index];
            assert_eq!(json!(edge_code(channel.edge)), control["edge_code"]);
            assert_eq!(
                json!(count_direction(channel.direction)),
                control["direction_code"]
            );
            assert_eq!(
                wire::hex(&channel.initial_count.to_le_bytes()),
                control["initial_u32_le_hex"].as_str().unwrap()
            );
        }
        for control in oracle["frequency_method_controls"].as_array().unwrap() {
            let method = serde_json::from_value(control["method"].clone()).unwrap();
            assert_eq!(json!(frequency_method(method)), control["code"]);
        }
        let channel = frequency_input_control(&oracle);
        let control = &oracle["frequency_constructor_control"];
        assert_eq!(json!(HERTZ), control["units_code"]);
        assert_eq!(json!(edge_code(channel.edge)), control["edge_code"]);
        assert_eq!(
            json!(frequency_method(channel.method)),
            control["method_code"]
        );
        assert_eq!(channel.min_hz.to_bits(), 0x3fc0_0000_0000_0000);
        assert_eq!(channel.max_hz.to_bits(), 0x40c0_0000_0000_0000);
        assert_eq!(
            channel.measurement_time_seconds.to_bits(),
            0x3fb0_0000_0000_0000
        );
        assert_eq!(json!(channel.divisor), control["divisor"]);
    }

    #[test]
    fn typed_input_buffers_preserve_unsigned_high_bits_zero_and_ieee_payloads() {
        let digital = digital_input_oracle();
        let counter = counter_input_oracle();
        let ports = ReadBuffer::U32(
            digital["manual_scan_major_patterns"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| u32::try_from(value["value"].as_u64().unwrap()).unwrap())
                .collect(),
        );
        assert_eq!(
            ports.prefix_hex(ports.len()).unwrap(),
            digital["raw_extent_control"]["all_words_le_hex"]
                .as_str()
                .unwrap()
        );
        assert_eq!(
            ports.prefix_hex(3).unwrap(),
            digital["raw_extent_control"]["negative_valid_first_scan_le_hex"]
                .as_str()
                .unwrap()
        );
        ports.prefix_hex(ports.len() + 1).unwrap_err();
        let count = ReadBuffer::U32(
            counter["manual_count_snapshots"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| u32::try_from(value["count"].as_u64().unwrap()).unwrap())
                .collect(),
        );
        assert_eq!(
            count.prefix_hex(count.len()).unwrap(),
            counter["count_raw_extent_control"]["all_words_le_hex"]
                .as_str()
                .unwrap()
        );
        assert_eq!(count.prefix_hex(1).unwrap(), "00000000");
        assert_eq!(valid_count(1, 1, 1, count.len()).unwrap(), 1);
        assert_eq!(valid_count(0, 1, 1, count.len()).unwrap(), 0);
        let frequency = ReadBuffer::F64(
            counter["manual_frequency_patterns"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| {
                    f64::from_bits(
                        u64::from_str_radix(value["bits_hex"].as_str().unwrap(), 16).unwrap(),
                    )
                })
                .collect(),
        );
        assert_eq!(
            frequency.prefix_hex(frequency.len()).unwrap(),
            counter["frequency_raw_extent_control"]["all_words_le_hex"]
                .as_str()
                .unwrap()
        );
        assert_eq!(
            frequency.prefix_hex(1).unwrap(),
            counter["frequency_raw_extent_control"]["negative_valid_first_word_le_hex"]
                .as_str()
                .unwrap()
        );
        frequency.prefix_hex(frequency.len() + 1).unwrap_err();
        let count_measurement = Measurement::CounterCount {
            channels: vec![count_input_control(&counter, 0)],
        };
        let frequency_measurement = Measurement::CounterFrequency {
            channels: vec![frequency_input_control(&counter)],
        };
        assert_eq!(
            ReadBuffer::new(&count_measurement, 1)
                .prefix_hex(1)
                .unwrap(),
            counter["read_controls"]["count"]["zero_initial_bytes_hex"]
                .as_str()
                .unwrap()
        );
        assert_eq!(
            ReadBuffer::new(&frequency_measurement, 1)
                .prefix_hex(1)
                .unwrap(),
            counter["read_controls"]["frequency"]["zero_initial_bytes_hex"]
                .as_str()
                .unwrap()
        );
    }

    fn engineering_oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/nidaqmx/engineering-ai.json"
        ))
        .unwrap()
    }
    fn time_position_oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/nidaqmx/counter-time-position.json"
        ))
        .unwrap()
    }
    fn engineering_channels(oracle: &serde_json::Value) -> [MixedChannel; 3] {
        use super::super::config::{AccelerationExcitation, VoltageExcitation};
        let strain = &oracle["strain_pair"][0];
        let ratio = &oracle["bridge_ratio_pair"][0];
        let accel = &oracle["acceleration_three_channel_cycle"][0];
        let word =
            |control: &serde_json::Value, field: &str| mixed_argument(oracle, &control[field]);
        [
            MixedChannel::Strain(StrainChannel {
                physical: strain["physical"].as_str().unwrap().into(),
                min_strain: word(strain, "min_word"),
                max_strain: word(strain, "max_word"),
                bridge_type: serde_json::from_value(strain["bridge_type"].clone()).unwrap(),
                excitation: VoltageExcitation {
                    source: ExcitationSource::Internal,
                    volts: word(strain, "volts_word"),
                },
                gage_factor: word(strain, "gage_factor_word"),
                initial_bridge_volts: word(strain, "initial_bridge_volts_word"),
                nominal_gage_ohms: word(strain, "nominal_gage_ohms_word"),
                poisson_ratio: word(strain, "poisson_ratio_word"),
                lead_wire_ohms: word(strain, "lead_wire_ohms_word"),
                coupling: Some(Coupling::Ac),
            }),
            MixedChannel::BridgeRatio(BridgeRatioChannel {
                physical: ratio["physical"].as_str().unwrap().into(),
                min_ratio: word(ratio, "min_word"),
                max_ratio: word(ratio, "max_word"),
                units: serde_json::from_value(ratio["units"].clone()).unwrap(),
                bridge_config: serde_json::from_value(ratio["bridge_config"].clone()).unwrap(),
                excitation: VoltageExcitation {
                    source: ExcitationSource::Internal,
                    volts: word(ratio, "volts_word"),
                },
                nominal_bridge_ohms: word(ratio, "nominal_bridge_ohms_word"),
                coupling: Some(Coupling::Dc),
            }),
            MixedChannel::Acceleration(AccelerationChannel {
                physical: accel["physical"].as_str().unwrap().into(),
                terminal_config: TerminalConfig::Pseudodifferential,
                min_acceleration: word(accel, "min_word"),
                max_acceleration: word(accel, "max_word"),
                units: serde_json::from_value(accel["units"].clone()).unwrap(),
                sensitivity: word(accel, "sensitivity_word"),
                sensitivity_units: serde_json::from_value(accel["sensitivity_units"].clone())
                    .unwrap(),
                excitation: AccelerationExcitation {
                    source: ExcitationSource::Internal,
                    amperes: word(accel, "amperes_word"),
                },
                coupling: Some(Coupling::Ac),
            }),
        ]
    }

    #[test]
    fn engineering_layout_units_and_optional_coupling_follow_frozen_codes() {
        let oracle = engineering_oracle();
        for control in oracle["strain_layout_controls"].as_array().unwrap() {
            let kind = serde_json::from_value(control["bridge_type"].clone()).unwrap();
            assert_eq!(json!(strain_bridge_type(kind)), control["code"]);
        }
        for control in oracle["bridge_configuration_controls"].as_array().unwrap() {
            let kind = serde_json::from_value(control["bridge_config"].clone()).unwrap();
            assert_eq!(json!(bridge_config(kind)), control["code"]);
        }
        for control in oracle["coupling_controls"].as_array().unwrap() {
            let mut selected = ConstructorSelection::default();
            let request = (control["request"] != "omitted")
                .then(|| serde_json::from_value(control["request"].clone()).unwrap());
            selected.coupling(request);
            assert_eq!(
                json!(selected.contains(ConstructorKind::Coupling)),
                control["selected_setter"]
            );
            if let Some(request) = request {
                assert_eq!(json!(coupling_code(request)), control["code"]);
            }
        }
        for control in oracle["bridge_ratio_pair"].as_array().unwrap() {
            let units = serde_json::from_value(control["units"].clone()).unwrap();
            assert_eq!(json!(bridge_units(units)), control["units_code"]);
        }
        for control in oracle["acceleration_three_channel_cycle"]
            .as_array()
            .unwrap()
        {
            let units = serde_json::from_value(control["units"].clone()).unwrap();
            let sensitivity = serde_json::from_value(control["sensitivity_units"].clone()).unwrap();
            let source = serde_json::from_value(control["excitation_source"].clone()).unwrap();
            assert_eq!(json!(acceleration_units(units)), control["units_code"]);
            assert_eq!(
                json!(sensitivity_units(sensitivity)),
                control["sensitivity_units_code"]
            );
            assert_eq!(json!(excitation_source(source)), control["excitation_code"]);
        }
        assert_eq!(json!(STRAIN), oracle["strain_pair"][0]["units_code"]);
        for control in oracle["none_excitation_controls"].as_array().unwrap() {
            let source = serde_json::from_value(control["source"].clone()).unwrap();
            assert_eq!(json!(excitation_source(source)), control["source_code"]);
        }
    }

    #[test]
    fn engineering_mixed_selection_is_exact_used_union_and_no_unrequested_setter() {
        let oracle = engineering_oracle();
        let mut measurement = mixed_control(&mixed_oracle());
        let Measurement::Mixed { channels } = &mut measurement else {
            unreachable!()
        };
        channels.extend(engineering_channels(&oracle));
        let selected = ConstructorSelection::for_measurement(&measurement);
        let constructors = [
            (ConstructorKind::Voltage, "DAQmxCreateAIVoltageChan"),
            (ConstructorKind::Thermocouple, "DAQmxCreateAIThrmcplChan"),
            (ConstructorKind::Current, "DAQmxCreateAICurrentChan"),
            (ConstructorKind::Rtd, "DAQmxCreateAIRTDChan"),
            (ConstructorKind::Strain, "DAQmxCreateAIStrainGageChan"),
            (ConstructorKind::Bridge, "DAQmxCreateAIBridgeChan"),
            (ConstructorKind::Acceleration, "DAQmxCreateAIAccelChan"),
        ];
        let names = constructors
            .into_iter()
            .filter_map(|(kind, name)| selected.contains(kind).then_some(name))
            .collect::<Vec<_>>();
        assert_eq!(
            json!(names),
            oracle["mixed_seven_channel_control"]["selected_constructor_union"]
        );
        assert!(selected.contains(ConstructorKind::Coupling));
        assert!(!selected.contains(ConstructorKind::Terminal));
        assert_eq!(selected.0.count_ones(), 8);
        let Measurement::Mixed { channels } = &mut measurement else {
            unreachable!()
        };
        for channel in channels {
            match channel {
                MixedChannel::Strain(c) => c.coupling = None,
                MixedChannel::BridgeRatio(c) => c.coupling = None,
                MixedChannel::Acceleration(c) => c.coupling = None,
                _ => {}
            }
        }
        let without_coupling = ConstructorSelection::for_measurement(&measurement);
        assert!(!without_coupling.contains(ConstructorKind::Coupling));
        assert_eq!(without_coupling.0.count_ones(), 7);
    }

    #[test]
    fn time_position_codes_high_selection_bits_and_explicit_z_values_follow_oracle() {
        let oracle = time_position_oracle();
        for control in oracle["period_controls"].as_array().unwrap() {
            let method = serde_json::from_value(control["method"].clone()).unwrap();
            assert_eq!(json!(period_method(method)), control["method_code"]);
        }
        for control in oracle["encoder_decoding_controls"].as_array().unwrap() {
            let decoding = serde_json::from_value(control["decoding"].clone()).unwrap();
            assert_eq!(json!(encoder_decoding(decoding)), control["code"]);
        }
        for control in oracle["z_phase_controls"].as_array().unwrap() {
            let phase = serde_json::from_value(control["phase"].clone()).unwrap();
            assert_eq!(json!(z_phase(phase)), control["code"]);
        }
        for control in oracle["angular_controls"].as_array().unwrap() {
            let units = serde_json::from_value(control["units"].clone()).unwrap();
            assert_eq!(json!(angular_units(units)), control["units_code"]);
        }
        for control in oracle["linear_controls"].as_array().unwrap() {
            let units = serde_json::from_value(control["units"].clone()).unwrap();
            assert_eq!(json!(linear_units(units)), control["units_code"]);
        }
        for control in oracle["z_policy_controls"].as_array().unwrap() {
            let value = mixed_argument(&oracle, &control["value_word"]);
            let z = z_arguments(ZIndex {
                enabled: control["enabled"].as_bool().unwrap(),
                value,
                phase: serde_json::from_value(control["phase"].clone()).unwrap(),
            });
            assert_eq!(json!(z.enabled), control["enabled_bool32"]);
            assert_eq!(z.value.to_bits(), value.to_bits());
            assert_eq!(json!(z.phase), control["phase_code"]);
        }
        for (sources, expected) in [
            ([None, None, None], [false, false, false]),
            ([Some("/D/PFI1"), None, None], [true, false, false]),
            ([None, Some("/D/PFI3"), None], [false, true, false]),
            ([None, None, Some("/D/PFI9")], [false, false, true]),
            (
                [Some("/D/PFI1"), Some("/D/PFI3"), Some("/D/PFI9")],
                [true, true, true],
            ),
        ] {
            let mut selected = ConstructorSelection::default();
            selected.encoder_sources(sources);
            for (kind, expected) in [
                ConstructorKind::EncoderATerminal,
                ConstructorKind::EncoderBTerminal,
                ConstructorKind::EncoderZTerminal,
            ]
            .into_iter()
            .zip(expected)
            {
                assert_eq!(selected.contains(kind), expected);
            }
            assert_eq!(
                selected.0.count_ones(),
                u32::try_from(expected.into_iter().filter(|b| *b).count()).unwrap()
            );
        }
        assert_eq!(json!(SECONDS), oracle["constants"]["seconds"]);
    }

    fn resistive_oracle(physical_bridge: bool) -> serde_json::Value {
        serde_json::from_str(if physical_bridge {
            include_str!("../../../tests/fixtures/nidaqmx/physical-bridge.json")
        } else {
            include_str!("../../../tests/fixtures/nidaqmx/resistive-ai.json")
        })
        .unwrap()
    }
    fn resistive_control(oracle: &serde_json::Value, kind: &str, index: usize) -> MixedChannel {
        let control = &oracle["ordered_constructor_pairs"][kind][index];
        let word = |key: &str| mixed_argument(oracle, &control[key]);
        let mut value = json!({"measurement":kind,"physical":control["physical"]});
        if let Some(prefix) = kind.strip_suffix("_bridge") {
            value[format!("min_{prefix}")] = json!(word("min_word"));
            value[format!("max_{prefix}")] = json!(word("max_word"));
            value["units"] = control["output_units"].clone();
            value["bridge_config"] = control["bridge_config"].clone();
            value["excitation"] =
                json!({"source":control["excitation_source"],"volts":word("volts_word")});
            value["nominal_bridge_ohms"] = json!(word("nominal_bridge_ohms_word"));
            let calibration = &control["calibration"];
            let point = |key: &str| {
                json!({
                "electrical":mixed_argument(oracle,&calibration[key]["electrical_word"]),
                "physical":mixed_argument(oracle,&calibration[key]["physical_word"])})
            };
            value["calibration"] = json!({"electrical_units":calibration["electrical_units"],
                "physical_units":calibration["physical_units"],"first":point("first"),"second":point("second")});
        } else {
            value["resistance_config"] = control["resistance_config"].clone();
            let amount = if kind == "thermistor_voltage" {
                "volts"
            } else {
                "amperes"
            };
            value["excitation"] =
                json!({"source":control["excitation_source"],amount:word("excitation_word")});
            if kind == "resistance" {
                value["min_ohms"] = json!(word("min_word"));
                value["max_ohms"] = json!(word("max_word"));
            } else {
                value["min_temperature"] = json!(word("min_word"));
                value["max_temperature"] = json!(word("max_word"));
                value["units"] = control["units"].clone();
                value["coefficients"] = json!({
                    "a":mixed_argument(oracle,&control["coefficient_words"]["a"]),
                    "b":mixed_argument(oracle,&control["coefficient_words"]["b"]),
                    "c":mixed_argument(oracle,&control["coefficient_words"]["c"])});
                if kind == "thermistor_voltage" {
                    value["r1_ohms"] = json!(word("r1_word"));
                }
            }
        }
        if control["coupling"] != "omitted" {
            value["coupling"] = control["coupling"].clone();
        }
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn resistive_and_calibration_unit_codes_follow_the_independently_frozen_domains() {
        let resistive = resistive_oracle(false);
        let bridge = resistive_oracle(true);
        assert_eq!(json!(OHMS), resistive["constants"]["ohms"]);
        for control in resistive["short_enum_cycles"]["wiring"].as_array().unwrap() {
            let kind = serde_json::from_value(control["name"].clone()).unwrap();
            assert_eq!(json!(resistance_config(kind)), control["code"]);
        }
        for control in resistive["short_enum_cycles"]["temperature_units"]
            .as_array()
            .unwrap()
        {
            let kind = serde_json::from_value(control["name"].clone()).unwrap();
            assert_eq!(json!(temperature_units(kind)), control["code"]);
        }
        macro_rules! units {
            ($family:literal, $convert:ident) => {
                for control in bridge["short_enum_cycles"][$family].as_array().unwrap() {
                    let output = serde_json::from_value(control["units"].clone()).unwrap();
                    let calibration =
                        serde_json::from_value(control["physical_units"].clone()).unwrap();
                    assert_eq!(json!($convert(output)), control["code"]);
                    assert_eq!(json!($convert(calibration)), control["physical_code"]);
                }
            };
        }
        units!("force", force_units);
        units!("pressure", pressure_units);
        units!("torque", torque_units);
    }

    #[test]
    fn physical_bridge_argument_packs_preserve_order_and_all_three_unit_roles() {
        let oracle = resistive_oracle(true);
        for group in ["force_bridge", "pressure_bridge", "torque_bridge"] {
            for index in 0..2 {
                let channel = resistive_control(&oracle, group, index);
                let request = match &channel {
                    MixedChannel::ForceBridge(c) => force_request(c),
                    MixedChannel::PressureBridge(c) => pressure_request(c),
                    MixedChannel::TorqueBridge(c) => torque_request(c),
                    _ => unreachable!(),
                };
                let control = &oracle["ordered_constructor_pairs"][group][index];
                assert_eq!(request.physical, control["physical"].as_str().unwrap());
                assert_eq!(json!(request.output_units), control["output_units_code"]);
                assert_eq!(json!(request.bridge), control["bridge_code"]);
                assert_eq!(json!(request.source), control["excitation_code"]);
                for (value, word) in [
                    (request.minimum, &control["min_word"]),
                    (request.maximum, &control["max_word"]),
                    (request.volts, &control["volts_word"]),
                    (request.nominal_ohms, &control["nominal_bridge_ohms_word"]),
                    (
                        request.calibration.first_electrical,
                        &control["calibration"]["first"]["electrical_word"],
                    ),
                    (
                        request.calibration.second_electrical,
                        &control["calibration"]["second"]["electrical_word"],
                    ),
                    (
                        request.calibration.first_physical,
                        &control["calibration"]["first"]["physical_word"],
                    ),
                    (
                        request.calibration.second_physical,
                        &control["calibration"]["second"]["physical_word"],
                    ),
                ] {
                    assert_eq!(value.to_bits(), mixed_argument(&oracle, word).to_bits());
                }
                assert_eq!(
                    json!(request.calibration.electrical_units),
                    control["calibration"]["electrical_units_code"]
                );
                assert_eq!(
                    json!(request.calibration.physical_units),
                    control["calibration"]["physical_units_code"]
                );
                assert_ne!(request.output_units, request.calibration.physical_units);
            }
        }
        let MixedChannel::ForceBridge(mut c) = resistive_control(&oracle, "force_bridge", 0) else {
            unreachable!()
        };
        let before = force_request(&c).calibration;
        std::mem::swap(&mut c.calibration.first, &mut c.calibration.second);
        let after = force_request(&c).calibration;
        assert_eq!(
            after.first_electrical.to_bits(),
            before.second_electrical.to_bits()
        );
        assert_eq!(
            after.first_physical.to_bits(),
            before.second_physical.to_bits()
        );
        assert_eq!(
            after.second_electrical.to_bits(),
            before.first_electrical.to_bits()
        );
        assert_eq!(
            after.second_physical.to_bits(),
            before.first_physical.to_bits()
        );
        c.calibration.second.physical = c.calibration.first.physical;
        let equal = force_request(&c).calibration;
        assert_eq!(
            equal.first_physical.to_bits(),
            equal.second_physical.to_bits()
        );
        c.excitation.source = ExcitationSource::None;
        c.excitation.volts = -0.0;
        let none = force_request(&c);
        assert_eq!(none.source, excitation_source(ExcitationSource::None));
        assert_eq!(none.volts.to_bits(), (-0.0_f64).to_bits());
    }

    #[test]
    fn mixed_thirteen_constructor_union_and_highest_bit_only_select_requested_exports() {
        let resistive = resistive_oracle(false);
        let physical = resistive_oracle(true);
        let mut measurement = mixed_control(&mixed_oracle());
        let Measurement::Mixed { channels } = &mut measurement else {
            unreachable!()
        };
        channels.extend(engineering_channels(&engineering_oracle()));
        for group in ["resistance", "thermistor_current", "thermistor_voltage"] {
            channels.push(resistive_control(&resistive, group, 0));
        }
        for group in ["force_bridge", "pressure_bridge", "torque_bridge"] {
            channels.push(resistive_control(&physical, group, 0));
        }
        let selected = ConstructorSelection::for_measurement(&measurement);
        let names = [
            (ConstructorKind::Voltage, "DAQmxCreateAIVoltageChan"),
            (ConstructorKind::Thermocouple, "DAQmxCreateAIThrmcplChan"),
            (ConstructorKind::Current, "DAQmxCreateAICurrentChan"),
            (ConstructorKind::Rtd, "DAQmxCreateAIRTDChan"),
            (ConstructorKind::Strain, "DAQmxCreateAIStrainGageChan"),
            (ConstructorKind::Bridge, "DAQmxCreateAIBridgeChan"),
            (ConstructorKind::Acceleration, "DAQmxCreateAIAccelChan"),
            (ConstructorKind::Resistance, "DAQmxCreateAIResistanceChan"),
            (
                ConstructorKind::ThermistorCurrent,
                "DAQmxCreateAIThrmstrChanIex",
            ),
            (
                ConstructorKind::ThermistorVoltage,
                "DAQmxCreateAIThrmstrChanVex",
            ),
            (
                ConstructorKind::ForceBridge,
                "DAQmxCreateAIForceBridgeTwoPointLinChan",
            ),
            (
                ConstructorKind::PressureBridge,
                "DAQmxCreateAIPressureBridgeTwoPointLinChan",
            ),
            (
                ConstructorKind::TorqueBridge,
                "DAQmxCreateAITorqueBridgeTwoPointLinChan",
            ),
        ]
        .into_iter()
        .filter_map(|(kind, name)| selected.contains(kind).then_some(name))
        .collect::<Vec<_>>();
        assert_eq!(
            json!(names),
            resistive["mixed_thirteen_channel_control"]["constructor_order"]
        );
        assert!(selected.contains(ConstructorKind::Coupling));
        assert!(!selected.contains(ConstructorKind::Terminal));
        assert_eq!(selected.0.count_ones(), 14);
        let MixedChannel::TorqueBridge(mut c) = resistive_control(&physical, "torque_bridge", 0)
        else {
            unreachable!()
        };
        c.coupling = None;
        let selected =
            ConstructorSelection::for_measurement(&Measurement::TorqueBridge { channels: vec![c] });
        assert_eq!(ConstructorKind::TorqueBridge as u8, 28);
        assert_eq!(selected.0, 1 << 28);
    }

    fn five_oracle(position: bool) -> serde_json::Value {
        serde_json::from_str(if position {
            include_str!("../../../tests/fixtures/nidaqmx/analog-position.json")
        } else {
            include_str!("../../../tests/fixtures/nidaqmx/dynamic-ai.json")
        })
        .unwrap()
    }
    fn five_word(oracle: &serde_json::Value, name: &serde_json::Value) -> f64 {
        f64::from_bits(
            u64::from_str_radix(
                oracle["manual_argument_words"][name.as_str().unwrap()]["ieee754_hex"]
                    .as_str()
                    .unwrap(),
                16,
            )
            .unwrap(),
        )
    }
    fn five_control_value(
        oracle: &serde_json::Value,
        profile: &str,
        control: &serde_json::Value,
    ) -> serde_json::Value {
        let words = &control["float_argument_words"];
        let word = |role: &str| five_word(oracle, &words[role]);
        let mut value = json!({"measurement":profile,"physical":control["physical"],
            "units":control["units"],"sensitivity":word("sensitivity"),
            "sensitivity_units":control["sensitivity_units"]});
        let range = match profile {
            "velocity_iepe" => "velocity",
            "force_iepe" => "force",
            "charge_acceleration" => "acceleration",
            "lvdt" => "position",
            "rvdt" => "angle",
            _ => unreachable!("only independently frozen five-profile controls"),
        };
        value[format!("min_{range}")] = json!(word("min"));
        value[format!("max_{range}")] = json!(word("max"));
        if let Some(terminal) = control.get("terminal_config") {
            value["terminal_config"] = terminal.clone();
        }
        if profile != "charge_acceleration" {
            let amount = if matches!(profile, "lvdt" | "rvdt") {
                "volts"
            } else {
                "amperes"
            };
            value["excitation"] =
                json!({"source":control["excitation_source"],amount:word(amount)});
        }
        if let Some(leads) = control.get("ac_wire_mode") {
            value["excitation"]["frequency_hz"] = json!(word("frequency_hz"));
            value["ac_wire_mode"] = leads.clone();
        }
        if control["coupling"] != "omitted" {
            value["coupling"] = control["coupling"].clone();
        }
        value
    }
    fn five_argument_values(channel: &MixedChannel) -> Vec<serde_json::Value> {
        let bits = |value: f64| json!(format!("{:016x}", value.to_bits()));
        let iepe = |r: IepeRequest<'_>| {
            vec![
                json!("owned_task"),
                json!(r.physical),
                json!(""),
                json!(r.terminal),
                bits(r.minimum),
                bits(r.maximum),
                json!(r.units),
                bits(r.sensitivity),
                json!(r.sensitivity_units),
                json!(r.source),
                bits(r.amperes),
                json!("NULL"),
            ]
        };
        let position = |r: AcPositionRequest<'_>| {
            vec![
                json!("owned_task"),
                json!(r.physical),
                json!(""),
                bits(r.minimum),
                bits(r.maximum),
                json!(r.units),
                bits(r.sensitivity),
                json!(r.sensitivity_units),
                json!(r.source),
                bits(r.volts),
                bits(r.frequency_hz),
                json!(r.wire_mode),
                json!("NULL"),
            ]
        };
        match channel {
            MixedChannel::VelocityIepe(c) => iepe(velocity_iepe_request(c)),
            MixedChannel::ForceIepe(c) => iepe(force_iepe_request(c)),
            MixedChannel::ChargeAcceleration(c) => {
                let r = charge_request(c);
                vec![
                    json!("owned_task"),
                    json!(r.physical),
                    json!(""),
                    json!(r.terminal),
                    bits(r.minimum),
                    bits(r.maximum),
                    json!(r.units),
                    bits(r.sensitivity),
                    json!(r.sensitivity_units),
                    json!("NULL"),
                ]
            }
            MixedChannel::Lvdt(c) => position(lvdt_request(c)),
            MixedChannel::Rvdt(c) => position(rvdt_request(c)),
            _ => unreachable!("only independently frozen five-profile controls"),
        }
    }
    fn five_expected_arguments(
        oracle: &serde_json::Value,
        control: &serde_json::Value,
    ) -> Vec<serde_json::Value> {
        control["ordered_arguments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|argument| {
                argument
                    .as_str()
                    .and_then(|name| oracle["manual_argument_words"].get(name))
                    .map_or_else(|| argument.clone(), |word| word["ieee754_hex"].clone())
            })
            .collect()
    }
    fn five_measurement(channel: MixedChannel) -> Measurement {
        match channel {
            MixedChannel::VelocityIepe(c) => Measurement::VelocityIepe { channels: vec![c] },
            MixedChannel::ForceIepe(c) => Measurement::ForceIepe { channels: vec![c] },
            MixedChannel::ChargeAcceleration(c) => {
                Measurement::ChargeAcceleration { channels: vec![c] }
            }
            MixedChannel::Lvdt(c) => Measurement::Lvdt { channels: vec![c] },
            MixedChannel::Rvdt(c) => Measurement::Rvdt { channels: vec![c] },
            _ => unreachable!("only independently frozen five-profile controls"),
        }
    }

    #[test]
    fn five_native_argument_roles_and_independent_unit_domains_match_frozen_literals() {
        for position in [false, true] {
            let oracle = five_oracle(position);
            for profile in oracle["profiles"].as_array().unwrap() {
                let profile = profile.as_str().unwrap();
                for control in oracle["ordered_constructor_pairs"][profile]
                    .as_array()
                    .unwrap()
                {
                    let channel: MixedChannel =
                        serde_json::from_value(five_control_value(&oracle, profile, control))
                            .unwrap();
                    assert_eq!(
                        five_argument_values(&channel),
                        five_expected_arguments(&oracle, control)
                    );
                }
            }
            macro_rules! domain {
                ($name:literal, $map:ident) => {
                    for (name, code) in oracle["constants"][$name].as_object().unwrap() {
                        let request = serde_json::from_value(json!(name)).unwrap();
                        assert_eq!(json!($map(request)), *code);
                    }
                };
            }
            domain!("velocity_units", velocity_units);
            domain!("velocity_sensitivity_units", velocity_sensitivity_units);
            domain!("force_units", force_units);
            domain!("force_sensitivity_units", force_sensitivity_units);
            domain!("acceleration_units", acceleration_units);
            domain!("charge_sensitivity_units", charge_sensitivity_units);
            domain!("linear_units", linear_units);
            domain!("lvdt_sensitivity_units", lvdt_sensitivity_units);
            domain!("angular_units", angular_units);
            domain!("rvdt_sensitivity_units", rvdt_sensitivity_units);
            domain!("ac_wire_mode", ac_wire_mode);
        }
    }

    #[test]
    fn five_constructor_union_uses_u64_and_independent_bits_above_u32() {
        let dynamic = five_oracle(false);
        let position = five_oracle(true);
        let cases = [
            (&dynamic, "velocity_iepe", ConstructorKind::VelocityIepe),
            (&dynamic, "force_iepe", ConstructorKind::ForceIepe),
            (
                &dynamic,
                "charge_acceleration",
                ConstructorKind::ChargeAcceleration,
            ),
            (&position, "lvdt", ConstructorKind::Lvdt),
            (&position, "rvdt", ConstructorKind::Rvdt),
        ];
        let mut channels = Vec::new();
        for (oracle, profile, kind) in cases {
            let control = &oracle["ordered_constructor_pairs"][profile][0];
            let channel: MixedChannel =
                serde_json::from_value(five_control_value(oracle, profile, control)).unwrap();
            let bit_index = oracle["selection_width"]["new_profile_bit_indices"][profile]
                .as_u64()
                .unwrap();
            assert_eq!(u64::from(kind as u8), bit_index);
            let selection =
                ConstructorSelection::for_measurement(&five_measurement(channel.clone()));
            assert!(selection.contains(kind));
            let coupling = control["coupling"] != "omitted";
            assert_eq!(
                selection.0,
                (1u64 << bit_index)
                    | if coupling {
                        1 << ConstructorKind::Coupling as u8
                    } else {
                        0
                    }
            );
            channels.push(channel);
        }
        let selected = ConstructorSelection::for_measurement(&Measurement::Mixed { channels });
        let expected = u64::from_str_radix(
            position["selection_width"]["all_five_mask_hex"]
                .as_str()
                .unwrap(),
            16,
        )
        .unwrap();
        assert_eq!(
            selected.0,
            expected | (1 << ConstructorKind::Coupling as u8)
        );
        assert!(selected.contains(ConstructorKind::Lvdt));
        assert!(selected.contains(ConstructorKind::Rvdt));
        assert!(selected.0 > u64::from(u32::MAX));
        for unselected in [
            ConstructorKind::Voltage,
            ConstructorKind::Thermocouple,
            ConstructorKind::Current,
            ConstructorKind::Rtd,
            ConstructorKind::Terminal,
            ConstructorKind::Digital,
            ConstructorKind::Count,
            ConstructorKind::Frequency,
            ConstructorKind::Resistance,
            ConstructorKind::ThermistorCurrent,
            ConstructorKind::ThermistorVoltage,
            ConstructorKind::ForceBridge,
            ConstructorKind::PressureBridge,
            ConstructorKind::TorqueBridge,
        ] {
            assert!(!selected.contains(unselected));
        }
    }

    #[test]
    fn five_signed_zero_sensitivity_and_none_excitation_reach_exact_native_slots() {
        for position in [false, true] {
            let oracle = five_oracle(position);
            for profile in oracle["profiles"].as_array().unwrap() {
                let profile = profile.as_str().unwrap();
                for cycle in oracle["short_request_cycles"][profile].as_array().unwrap() {
                    let control = oracle["ordered_constructor_pairs"][profile]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|c| c["case"] == cycle["base_case"])
                        .unwrap();
                    let mut value = five_control_value(&oracle, profile, control);
                    let overrides = &cycle["overrides"];
                    value["sensitivity"] =
                        json!(five_word(&oracle, &overrides["sensitivity_word"]));
                    value["coupling"] = overrides["coupling"].clone();
                    let mut expected = five_expected_arguments(&oracle, control);
                    let sensitivity_slot = if position { 6 } else { 7 };
                    expected[sensitivity_slot] = oracle["manual_argument_words"]
                        [overrides["sensitivity_word"].as_str().unwrap()]["ieee754_hex"]
                        .clone();
                    if let Some(source) = overrides.get("excitation_source") {
                        let amount = if position { "volts" } else { "amperes" };
                        value["excitation"]["source"] = source.clone();
                        value["excitation"][amount] =
                            json!(five_word(&oracle, &overrides["excitation_amount_word"]));
                        expected[if position { 8 } else { 9 }] =
                            oracle["constants"]["source"][source.as_str().unwrap()].clone();
                        expected[if position { 9 } else { 10 }] = oracle["manual_argument_words"]
                            [overrides["excitation_amount_word"].as_str().unwrap()]["ieee754_hex"]
                            .clone();
                    }
                    if let Some(word) = overrides.get("frequency_word") {
                        value["excitation"]["frequency_hz"] = json!(five_word(&oracle, word));
                        expected[10] =
                            oracle["manual_argument_words"][word.as_str().unwrap()]["ieee754_hex"]
                                .clone();
                    }
                    if let Some(mode) = overrides.get("ac_wire_mode") {
                        value["ac_wire_mode"] = mode.clone();
                        expected[11] =
                            oracle["constants"]["ac_wire_mode"][mode.as_str().unwrap()].clone();
                    }
                    for (name, slot, domain) in [
                        (
                            "units",
                            if position { 5 } else { 6 },
                            if profile == "force_iepe" {
                                "force_units"
                            } else {
                                "acceleration_units"
                            },
                        ),
                        (
                            "sensitivity_units",
                            if position { 7 } else { 8 },
                            "charge_sensitivity_units",
                        ),
                    ] {
                        if let Some(request) = overrides.get(name) {
                            value[name] = request.clone();
                            expected[slot] =
                                oracle["constants"][domain][request.as_str().unwrap()].clone();
                        }
                    }
                    let channel: MixedChannel = serde_json::from_value(value).unwrap();
                    assert_eq!(five_argument_values(&channel), expected);
                }
            }
        }
    }
}
