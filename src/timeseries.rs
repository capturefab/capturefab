//! Bounded industrial data acquisition on one host-monotonic timeline.
//! Device clocks are preserved separately; receive times do not imply hardware synchronization.
mod aandd_balance;
mod abb_rws;
mod ads;
mod atlas_open_protocol;
mod bacnet_ip;
mod camera;
mod camera_session;
mod can;
mod cognex_insight;
mod epson_remote;
mod ethernet_ip;
mod fatek_fbs;
mod fieldbus;
mod fins;
mod fx_computer_link;
mod gocator;
mod gopxl_ascii;
mod hart;
mod helper;
mod hesai;
mod http;
mod iec62056_21;
mod ifm_iot;
mod iolink_json;
mod jsonl;
mod keyence_iv;
mod keyence_kv;
mod livox;
mod ls_cnet;
mod ls_xgt;
mod mc_1e;
mod mewtocol;
mod microepsilon_rs422;
mod modbus;
mod mqtt;
mod mt_sics;
mod mtconnect;
#[cfg(feature = "usb")]
mod native_worker;
mod net;
pub(crate) mod ni_daqmx;
mod nmea_gnss;
mod omron_compoway_f;
mod opcua;
mod optommp;
mod ouster;
mod pccc;
mod pfeiffer_tpg;
mod robosense;
mod s7;
mod sartorius_sbi;
mod scpi;
mod sdi12;
mod sick_cola;
mod sick_compact;
mod sick_compact_tcp;
mod slmp;
mod sml;
mod ur_rtde;

#[cfg(feature = "usb")]
pub(crate) const USBTMC_WORKER_ARG: &str = "__usbtmc-worker";

#[cfg(feature = "usb")]
pub(crate) fn run_usbtmc_worker() -> Result<()> {
    scpi::usbtmc::run_worker()
}

#[cfg(all(feature = "usb", feature = "testing"))]
pub(crate) fn run_usbtmc_fixture() -> Result<()> {
    scpi::usbtmc::run_fixture()
}

#[cfg(all(feature = "usb", feature = "testing"))]
pub(crate) fn run_usbtmc_probe(input: Value) -> Result<Value> {
    scpi::usbtmc::run_probe(input)
}

#[cfg(test)]
mod acquisition_tests;
#[cfg(test)]
mod config_tests;

mod serial;
mod vaisala_wxt;
mod velodyne_vlp16;
mod video;
mod yaskawa_hses;
mod yokogawa_fam3;

use crate::{
    storage,
    types::{Categorized, ErrorKind, Usage},
};
use anyhow::{Context as _, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{BufRead as _, BufReader, Read as _, Write as _},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const MAX_LINE: usize = 1 << 20;
const MAX_SOURCES: usize = 64;
const QUEUE: usize = 64;

/// The helper wire contract and the value returned by every native adapter.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reading {
    pub signal: String,
    pub value: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_time: Option<String>,
}

pub(crate) trait Source: Send {
    /// An empty batch means idle. Failures stop the recording; none are silently dropped.
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>>;
    /// Whether protocol initialization completed (an idle subscription can still be ready).
    fn ready(&self) -> bool {
        true
    }
    /// Actual host receive time from an isolated worker, before encoding/storage.
    fn observed_ns(&self) -> Option<u64> {
        None
    }
    /// Whether a final read timed out before starting an acquisition operation.
    /// Ordinary passive idle returns an empty batch; an in-flight failure stays strict.
    fn end_timeout_is_idle(&self) -> bool {
        false
    }
    /// Set the common acquisition cutoff; delivery may finish later.
    fn set_acquisition_end(&mut self, _end_ns: Option<u64>) {}
    /// Indexed media can arrive after a later read starts; its receive clock stays authoritative.
    fn buffered_timestamps(&self) -> bool {
        false
    }
    /// Opt in only with one pending nonempty batch, using its original operation deadline.
    /// The writer reports OS file-data sync plus directory sync where supported; this
    /// does not promise survival of every power loss or cancellation of filesystem I/O.
    fn checkpoint_deadline(&self) -> Option<Instant> {
        None
    }
    /// Acknowledge that pending batch after its writer receipt. Implementations must
    /// retain the original deadline and must not start a fresh acknowledgment budget.
    fn checkpointed(&mut self) -> Result<()> {
        Ok(())
    }
    /// Finish owned recordings before the time-series end row is committed.
    fn finish(&mut self, _timeout: Duration) -> Result<()> {
        Ok(())
    }
    /// Deliver remaining observations and finalized-file metadata after acquisition ends.
    fn drain(&mut self) -> Result<Option<Reading>> {
        Ok(None)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub version: u32,
    pub sources: Vec<SourceConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SourceConfig {
    pub id: String,
    #[serde(
        default = "default_every",
        deserialize_with = "crate::config_number::unsigned"
    )]
    pub every_ms: u64,
    #[serde(
        default = "default_timeout",
        deserialize_with = "crate::config_number::unsigned"
    )]
    pub timeout_ms: u64,
    #[serde(flatten)]
    pub adapter: Adapter,
}
const fn default_every() -> u64 {
    100
}
const fn default_timeout() -> u64 {
    1000
}

#[derive(Debug, Clone)]
pub(crate) enum Adapter {
    Simulator(SimConfig),
    ModbusTcp(modbus::TcpConfig),
    ModbusRtu(modbus::RtuConfig),
    ModbusAscii(modbus::ascii::Config),
    S7(s7::Config),
    Ads(ads::Config),
    BacnetIp(bacnet_ip::Config),
    Pccc(pccc::Config),
    PcccCsp(pccc::csp::Config),
    Df1(pccc::df1::Config),
    Iec62056_21(iec62056_21::Config),
    SmlSerial(sml::Config),
    Sdi12Serial(sdi12::Config),
    HartSerial(hart::Config),
    VaisalaWxt(vaisala_wxt::Config),
    IfmIot(ifm_iot::Config),
    IolinkJson(iolink_json::Config),
    Slmp(slmp::Config),
    Mc1e(mc_1e::Config),
    FxComputerLink(fx_computer_link::Config),
    Mewtocol(mewtocol::Config),
    FinsTcp(fins::Config),
    FinsUdp(fins::UdpConfig),
    OmronHostLink(fins::host_link::Config),
    OmronCompoWayF(omron_compoway_f::Config),
    KeyenceIv(keyence_iv::Config),
    KeyenceKv(keyence_kv::Config),
    FatekFbs(fatek_fbs::Config),
    UrRtde(ur_rtde::Config),
    AtlasOpenProtocol(atlas_open_protocol::Config),
    OptoMmp(optommp::Config),
    GopxlAscii(gopxl_ascii::Config),
    AbbRws(abb_rws::Config),
    EpsonRemote(epson_remote::Config),
    YaskawaHses(yaskawa_hses::Config),
    MtConnect(mtconnect::Config),
    LsCnet(ls_cnet::Config),
    LsXgt(ls_xgt::Config),
    YokogawaFam3(yokogawa_fam3::Config),
    CognexInsight(cognex_insight::Config),
    CognexDataMan(ethernet_ip::dataman::Config),
    Can(can::Config),
    JsonlTcp(jsonl::TcpConfig),
    JsonlSerial(jsonl::SerialConfig),
    Scpi(scpi::Config),
    SickCola(sick_cola::Config),
    SickCompact(sick_compact::Config),
    SickCompactTcp(sick_compact_tcp::Config),
    MicroEpsilonRs422(microepsilon_rs422::Config),
    MtSics(mt_sics::Config),
    AanddBalance(aandd_balance::Config),
    SartoriusSbi(sartorius_sbi::Config),
    PfeifferTpg(pfeiffer_tpg::Config),
    NmeaGnss(nmea_gnss::Config),
    VelodyneVlp16(velodyne_vlp16::Config),
    VelodyneHdl32e(velodyne_vlp16::Config),
    OusterUdp(ouster::Config),
    HesaiUdp(hesai::Config),
    LivoxUdp(livox::Config),
    RobosenseUdp(robosense::Config),
    GocatorGdp(gocator::Config),
    NiDaqmx(ni_daqmx::Config),
    Helper(helper::Config),
    Mqtt(mqtt::Config),
    OpcUa(Selection<opcua::Config>),
    Ethercat(Selection<fieldbus::EthercatConfig>),
    Profinet(Selection<fieldbus::ProfinetConfig>),
    EthernetIp(Selection<ethernet_ip::Config>),
    EthernetIpAssembly(ethernet_ip::AssemblyConfig),
    Camera(camera::Config),
    Video(video::Config),
}
impl<'de> Deserialize<'de> for Adapter {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        // Reconstruct Number values before selecting a typed adapter. Serde's
        // internally tagged Content buffer otherwise treats exact decimal
        // tokens as maps when arbitrary_precision is enabled.
        let mut value = crate::session::strict_json_value(deserializer)?;
        let protocol = value
            .as_object_mut()
            .and_then(|object| object.remove("protocol"))
            .and_then(|value| value.as_str().map(str::to_owned))
            .ok_or_else(|| serde::de::Error::custom("source needs a string protocol"))?;
        let result = match protocol.as_str() {
            "simulator" => serde_json::from_value(value).map(Self::Simulator),
            "modbus_tcp" => serde_json::from_value(value).map(Self::ModbusTcp),
            "modbus_rtu" => serde_json::from_value(value).map(Self::ModbusRtu),
            "modbus_ascii" => serde_json::from_value(value).map(Self::ModbusAscii),
            "s7" => serde_json::from_value(value).map(Self::S7),
            "ads" => serde_json::from_value(value).map(Self::Ads),
            "bacnet_ip" => serde_json::from_value(value).map(Self::BacnetIp),
            "pccc" => serde_json::from_value(value).map(Self::Pccc),
            "pccc_csp" => serde_json::from_value(value).map(Self::PcccCsp),
            "df1" => serde_json::from_value(value).map(Self::Df1),
            "iec62056_21" => serde_json::from_value(value).map(Self::Iec62056_21),
            "sml_serial" => serde_json::from_value(value).map(Self::SmlSerial),
            "sdi12_serial" => serde_json::from_value(value).map(Self::Sdi12Serial),
            "hart_serial" => serde_json::from_value(value).map(Self::HartSerial),
            "vaisala_wxt" => serde_json::from_value(value).map(Self::VaisalaWxt),
            "ifm_iot" => serde_json::from_value(value).map(Self::IfmIot),
            "iolink_json" => serde_json::from_value(value).map(Self::IolinkJson),
            "abb_rws" => serde_json::from_value(value).map(Self::AbbRws),
            "epson_remote" => serde_json::from_value(value).map(Self::EpsonRemote),
            "yaskawa_hses" => serde_json::from_value(value).map(Self::YaskawaHses),
            "mtconnect" => serde_json::from_value(value).map(Self::MtConnect),
            "slmp" => serde_json::from_value(value).map(Self::Slmp),
            "mc_1e" => serde_json::from_value(value).map(Self::Mc1e),
            "fx_computer_link" => serde_json::from_value(value).map(Self::FxComputerLink),
            "mewtocol" => serde_json::from_value(value).map(Self::Mewtocol),
            "fins_tcp" => serde_json::from_value(value).map(Self::FinsTcp),
            "fins_udp" => serde_json::from_value(value).map(Self::FinsUdp),
            "omron_host_link" => serde_json::from_value(value).map(Self::OmronHostLink),
            "omron_compoway_f" => serde_json::from_value(value).map(Self::OmronCompoWayF),
            "keyence_iv" => serde_json::from_value(value).map(Self::KeyenceIv),
            "keyence_kv" => serde_json::from_value(value).map(Self::KeyenceKv),
            "fatek_fbs" => serde_json::from_value(value).map(Self::FatekFbs),
            "ur_rtde" => serde_json::from_value(value).map(Self::UrRtde),
            "atlas_open_protocol" => serde_json::from_value(value).map(Self::AtlasOpenProtocol),
            "optommp" => serde_json::from_value(value).map(Self::OptoMmp),
            "gopxl_ascii" => serde_json::from_value(value).map(Self::GopxlAscii),
            "ls_cnet" => serde_json::from_value(value).map(Self::LsCnet),
            "ls_xgt" => serde_json::from_value(value).map(Self::LsXgt),
            "yokogawa_fam3" => serde_json::from_value(value).map(Self::YokogawaFam3),
            "cognex_insight" => serde_json::from_value(value).map(Self::CognexInsight),
            "cognex_dataman" => serde_json::from_value(value).map(Self::CognexDataMan),
            "can" => serde_json::from_value(value).map(Self::Can),
            "tcp_jsonl" => serde_json::from_value(value).map(Self::JsonlTcp),
            "serial_jsonl" => serde_json::from_value(value).map(Self::JsonlSerial),
            "scpi" => serde_json::from_value(value).map(Self::Scpi),
            "sick_cola" => serde_json::from_value(value).map(Self::SickCola),
            "sick_compact_udp" => serde_json::from_value(value).map(Self::SickCompact),
            "sick_compact_tcp" => serde_json::from_value(value).map(Self::SickCompactTcp),
            "microepsilon_rs422" => serde_json::from_value(value).map(Self::MicroEpsilonRs422),
            "mt_sics" => serde_json::from_value(value).map(Self::MtSics),
            "aandd_balance" => serde_json::from_value(value).map(Self::AanddBalance),
            "sartorius_sbi" => serde_json::from_value(value).map(Self::SartoriusSbi),
            "pfeiffer_tpg" => serde_json::from_value(value).map(Self::PfeifferTpg),
            "nmea_gnss" => serde_json::from_value(value).map(Self::NmeaGnss),
            "velodyne_vlp16" => serde_json::from_value(value).map(Self::VelodyneVlp16),
            "velodyne_hdl32e" => serde_json::from_value(value).map(Self::VelodyneHdl32e),
            "ouster_udp" => serde_json::from_value(value).map(Self::OusterUdp),
            "hesai_udp" => serde_json::from_value(value).map(Self::HesaiUdp),
            "livox_udp" => serde_json::from_value(value).map(Self::LivoxUdp),
            "robosense_udp" => serde_json::from_value(value).map(Self::RobosenseUdp),
            "gocator_gdp" => serde_json::from_value(value).map(Self::GocatorGdp),
            "ni_daqmx" => serde_json::from_value(value).map(Self::NiDaqmx),
            "helper" => serde_json::from_value(value).map(Self::Helper),
            "mqtt" => serde_json::from_value(value).map(Self::Mqtt),
            "opc_ua" => serde_json::from_value(value).map(Self::OpcUa),
            "ethercat" => serde_json::from_value(value).map(Self::Ethercat),
            "profinet" => serde_json::from_value(value).map(Self::Profinet),
            "ethernet_ip" => serde_json::from_value(value).map(Self::EthernetIp),
            "ethernet_ip_assembly" => serde_json::from_value(value).map(Self::EthernetIpAssembly),
            "camera" => serde_json::from_value(value).map(Self::Camera),
            "video" => serde_json::from_value(value).map(Self::Video),
            _ => {
                return Err(serde::de::Error::custom(format!(
                    "unknown data protocol: {protocol}"
                )));
            }
        };
        result.map_err(serde::de::Error::custom)
    }
}
#[derive(Debug, Clone)]
pub(crate) enum Selection<T> {
    BuiltIn(T),
    External(helper::Config),
}
impl<'de, T: serde::de::DeserializeOwned> Deserialize<'de> for Selection<T> {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        if value.get("executable").is_some() {
            serde_json::from_value(value)
                .map(Self::External)
                .map_err(serde::de::Error::custom)
        } else {
            serde_json::from_value(value)
                .map(Self::BuiltIn)
                .map_err(serde::de::Error::custom)
        }
    }
}
impl<T> Selection<T> {
    fn open(&self, builtin: impl FnOnce(&T) -> Result<Box<dyn Source>>) -> Result<Box<dyn Source>> {
        match self {
            Self::BuiltIn(config) => builtin(config),
            Self::External(config) => helper::open(config),
        }
    }
}
impl Adapter {
    const fn name(&self) -> &'static str {
        match self {
            Self::Video(_) => "video",
            Self::Camera(_) => "camera",
            Self::Simulator(_) => "simulator",
            Self::ModbusTcp(_) => "modbus_tcp",
            Self::ModbusRtu(_) => "modbus_rtu",
            Self::ModbusAscii(_) => "modbus_ascii",
            Self::S7(_) => "s7",
            Self::Ads(_) => "ads",
            Self::BacnetIp(_) => "bacnet_ip",
            Self::Pccc(_) => "pccc",
            Self::PcccCsp(_) => "pccc_csp",
            Self::Df1(_) => "df1",
            Self::Iec62056_21(_) => "iec62056_21",
            Self::SmlSerial(_) => "sml_serial",
            Self::Sdi12Serial(_) => "sdi12_serial",
            Self::HartSerial(_) => "hart_serial",
            Self::VaisalaWxt(_) => "vaisala_wxt",
            Self::IfmIot(_) => "ifm_iot",
            Self::IolinkJson(_) => "iolink_json",
            Self::AbbRws(_) => "abb_rws",
            Self::EpsonRemote(_) => "epson_remote",
            Self::YaskawaHses(_) => "yaskawa_hses",
            Self::MtConnect(_) => "mtconnect",
            Self::Slmp(_) => "slmp",
            Self::Mc1e(_) => "mc_1e",
            Self::FxComputerLink(_) => "fx_computer_link",
            Self::Mewtocol(_) => "mewtocol",
            Self::FinsTcp(_) => "fins_tcp",
            Self::FinsUdp(_) => "fins_udp",
            Self::OmronHostLink(_) => "omron_host_link",
            Self::OmronCompoWayF(_) => "omron_compoway_f",
            Self::KeyenceIv(_) => "keyence_iv",
            Self::KeyenceKv(_) => "keyence_kv",
            Self::FatekFbs(_) => "fatek_fbs",
            Self::UrRtde(_) => "ur_rtde",
            Self::AtlasOpenProtocol(_) => "atlas_open_protocol",
            Self::OptoMmp(_) => "optommp",
            Self::GopxlAscii(_) => "gopxl_ascii",
            Self::LsCnet(_) => "ls_cnet",
            Self::LsXgt(_) => "ls_xgt",
            Self::YokogawaFam3(_) => "yokogawa_fam3",
            Self::CognexInsight(_) => "cognex_insight",
            Self::CognexDataMan(_) => "cognex_dataman",
            Self::Can(_) => "can",
            Self::JsonlTcp(_) => "tcp_jsonl",
            Self::JsonlSerial(_) => "serial_jsonl",
            Self::Scpi(_) => "scpi",
            Self::SickCola(_) => "sick_cola",
            Self::SickCompact(_) => "sick_compact_udp",
            Self::SickCompactTcp(_) => "sick_compact_tcp",
            Self::MicroEpsilonRs422(_) => "microepsilon_rs422",
            Self::MtSics(_) => "mt_sics",
            Self::AanddBalance(_) => "aandd_balance",
            Self::SartoriusSbi(_) => "sartorius_sbi",
            Self::PfeifferTpg(_) => "pfeiffer_tpg",
            Self::NmeaGnss(_) => "nmea_gnss",
            Self::VelodyneVlp16(_) => "velodyne_vlp16",
            Self::VelodyneHdl32e(_) => "velodyne_hdl32e",
            Self::OusterUdp(_) => "ouster_udp",
            Self::HesaiUdp(_) => "hesai_udp",
            Self::LivoxUdp(_) => "livox_udp",
            Self::RobosenseUdp(_) => "robosense_udp",
            Self::GocatorGdp(_) => "gocator_gdp",
            Self::NiDaqmx(_) => "ni_daqmx",
            Self::Helper(_) => "helper",
            Self::Mqtt(_) => "mqtt",
            Self::OpcUa(_) => "opc_ua",
            Self::Ethercat(_) => "ethercat",
            Self::Profinet(_) => "profinet",
            Self::EthernetIp(_) => "ethernet_ip",
            Self::EthernetIpAssembly(_) => "ethernet_ip_assembly",
        }
    }
    const fn is_polled(&self) -> bool {
        matches!(
            self,
            Self::Camera(_)
                | Self::Simulator(_)
                | Self::OptoMmp(_)
                | Self::GopxlAscii(_)
                | Self::ModbusTcp(_)
                | Self::ModbusRtu(_)
                | Self::ModbusAscii(_)
                | Self::S7(_)
                | Self::Ads(_)
                | Self::BacnetIp(_)
                | Self::Pccc(_)
                | Self::PcccCsp(_)
                | Self::Df1(_)
                | Self::Iec62056_21(_)
                | Self::Sdi12Serial(_)
                | Self::HartSerial(_)
                | Self::VaisalaWxt(_)
                | Self::IfmIot(_)
                | Self::IolinkJson(_)
                | Self::AbbRws(_)
                | Self::EpsonRemote(_)
                | Self::YaskawaHses(_)
                | Self::MtConnect(_)
                | Self::Scpi(_)
                | Self::SickCola(_)
                | Self::MtSics(_)
                | Self::AanddBalance(_)
                | Self::SartoriusSbi(_)
                | Self::PfeifferTpg(_)
                | Self::Slmp(_)
                | Self::Mc1e(_)
                | Self::FxComputerLink(_)
                | Self::Mewtocol(_)
                | Self::FinsTcp(_)
                | Self::FinsUdp(_)
                | Self::OmronHostLink(_)
                | Self::OmronCompoWayF(_)
                | Self::KeyenceIv(_)
                | Self::KeyenceKv(_)
                | Self::FatekFbs(_)
                | Self::LsCnet(_)
                | Self::LsXgt(_)
                | Self::YokogawaFam3(_)
                | Self::CognexInsight(_)
                | Self::CognexDataMan(_)
                | Self::EthernetIp(Selection::BuiltIn(_))
                | Self::EthernetIpAssembly(_)
        )
    }
    fn open(
        &self,
        cameras: &mut camera_session::Cameras,
        timeout_ms: u64,
    ) -> Result<Box<dyn Source>> {
        match self {
            Self::Simulator(config) => {
                validate_name(&config.signal, "signal")?;
                Ok(Box::new(Simulator {
                    signal: config.signal.clone(),
                    sequence: 0,
                }))
            }
            Self::ModbusTcp(config) => modbus::tcp(config),
            Self::ModbusRtu(config) => modbus::rtu(config),
            Self::ModbusAscii(config) => modbus::ascii::open(config),
            Self::S7(config) => s7::open(config),
            Self::Ads(config) => ads::open(config),
            Self::BacnetIp(config) => bacnet_ip::open(config, timeout_ms),
            Self::Pccc(config) => pccc::open(config),
            Self::PcccCsp(config) => pccc::csp::open(config),
            Self::Df1(config) => pccc::df1::open(config),
            Self::Iec62056_21(config) => iec62056_21::open(config),
            Self::SmlSerial(config) => sml::open(config),
            Self::Sdi12Serial(config) => sdi12::open(config),
            Self::HartSerial(config) => hart::open(config),
            Self::VaisalaWxt(config) => vaisala_wxt::open(config),
            Self::IfmIot(config) => ifm_iot::open(config),
            Self::IolinkJson(config) => iolink_json::open(config),
            Self::AbbRws(config) => abb_rws::open(config),
            Self::EpsonRemote(config) => epson_remote::open(config),
            Self::YaskawaHses(config) => yaskawa_hses::open(config),
            Self::MtConnect(config) => mtconnect::open(config),
            Self::Slmp(config) => slmp::open(config),
            Self::Mc1e(config) => mc_1e::open(config),
            Self::FxComputerLink(config) => fx_computer_link::open(config),
            Self::Mewtocol(config) => mewtocol::open(config),
            Self::FinsTcp(config) => fins::open(config),
            Self::FinsUdp(config) => fins::open_udp(config, timeout_ms),
            Self::OmronHostLink(config) => fins::host_link::open(config),
            Self::OmronCompoWayF(config) => omron_compoway_f::open(config),
            Self::KeyenceIv(config) => keyence_iv::open(config),
            Self::KeyenceKv(config) => keyence_kv::open(config),
            Self::FatekFbs(config) => fatek_fbs::open(config),
            Self::UrRtde(config) => ur_rtde::open(config),
            Self::AtlasOpenProtocol(config) => atlas_open_protocol::open(config),
            Self::OptoMmp(config) => optommp::open(config),
            Self::GopxlAscii(config) => gopxl_ascii::open(config),
            Self::LsCnet(config) => ls_cnet::open(config),
            Self::LsXgt(config) => ls_xgt::open(config),
            Self::YokogawaFam3(config) => yokogawa_fam3::open(config),
            Self::CognexInsight(config) => cognex_insight::open(config),
            Self::CognexDataMan(config) => ethernet_ip::dataman::open(config),
            Self::Can(config) => can::open(config),
            Self::JsonlTcp(config) => jsonl::tcp(config),
            Self::JsonlSerial(config) => jsonl::serial(config),
            Self::Scpi(config) => scpi::open(config),
            Self::SickCola(config) => sick_cola::open(config),
            Self::SickCompact(config) => sick_compact::open(config),
            Self::SickCompactTcp(config) => sick_compact_tcp::open(config),
            Self::MicroEpsilonRs422(config) => microepsilon_rs422::open(config),
            Self::MtSics(config) => mt_sics::open(config),
            Self::AanddBalance(config) => aandd_balance::open(config),
            Self::SartoriusSbi(config) => sartorius_sbi::open(config),
            Self::PfeifferTpg(config) => pfeiffer_tpg::open(config),
            Self::NmeaGnss(config) => nmea_gnss::open(config),
            Self::VelodyneVlp16(config) => velodyne_vlp16::open(config),
            Self::VelodyneHdl32e(config) => velodyne_vlp16::open_hdl32e(config),
            Self::OusterUdp(config) => ouster::open(config),
            Self::HesaiUdp(config) => hesai::open(config),
            Self::LivoxUdp(config) => livox::open(config),
            Self::RobosenseUdp(config) => robosense::open(config),
            Self::GocatorGdp(config) => gocator::open(config),
            Self::NiDaqmx(config) => ni_daqmx::open(config),
            Self::Helper(config) => helper::open(config),
            Self::Mqtt(config) => mqtt::open(config),
            Self::OpcUa(config) => config.open(opcua::open),
            Self::Ethercat(config) => config.open(fieldbus::open_ethercat),
            Self::Profinet(config) => config.open(fieldbus::open_profinet),
            Self::EthernetIp(config) => config.open(ethernet_ip::open),
            Self::EthernetIpAssembly(config) => ethernet_ip::open_assembly(config),
            Self::Camera(config) => camera::open(config, cameras),
            Self::Video(config) => video::open(config, cameras),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SimConfig {
    pub signal: String,
}
struct Simulator {
    signal: String,
    sequence: u64,
}
impl Source for Simulator {
    fn read(&mut self, _timeout: Duration) -> Result<Vec<Reading>> {
        let value = self.sequence;
        self.sequence = self.sequence.saturating_add(1);
        Ok(vec![Reading {
            signal: self.signal.clone(),
            value: json!(value),
            source_time: None,
        }])
    }
}

fn validate_name(name: &str, what: &str) -> Result<()> {
    if name.is_empty() || name.len() > 1024 || name.chars().any(char::is_control) {
        return Err(Usage(format!(
            "{what} must be nonempty, at most 1024 bytes, without control characters"
        ))
        .into());
    }
    Ok(())
}

pub(crate) fn load(path: &Path) -> Result<Config> {
    let file =
        File::open(path).with_context(|| format!("cannot read data config {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take((MAX_LINE + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_LINE {
        return Err(Usage("data config exceeds 1 MiB".into()).into());
    }
    let config: Config = serde_json::from_slice(&bytes)
        .map_err(|error| Usage(format!("invalid data config: {error}")))?;
    config.validate()?;
    Ok(config)
}
impl Config {
    fn validate(&self) -> Result<()> {
        if self.version != 1 || self.sources.is_empty() || self.sources.len() > MAX_SOURCES {
            return Err(Usage("data config requires version 1 and 1–64 sources".into()).into());
        }
        let mut ids = BTreeSet::new();
        let mut udp_fins_nodes = BTreeSet::new();
        for source in &self.sources {
            validate_name(&source.id, "source id")?;
            match &source.adapter {
                Adapter::Helper(config)
                | Adapter::OpcUa(Selection::External(config))
                | Adapter::Ethercat(Selection::External(config))
                | Adapter::Profinet(Selection::External(config))
                | Adapter::EthernetIp(Selection::External(config)) => config.validate()?,
                Adapter::OpcUa(Selection::BuiltIn(config)) => opcua::validate(config)?,
                Adapter::NiDaqmx(config) => ni_daqmx::validate(config)?,
                Adapter::Scpi(config) => scpi::preflight(config)?,
                Adapter::AtlasOpenProtocol(config) => config.validate()?,
                Adapter::OptoMmp(config) => config.validate()?,
                Adapter::GopxlAscii(config) => config.validate()?,
                _ => {}
            }
            if let Adapter::AbbRws(config) = &source.adapter {
                config.validate()?;
            }
            if let Adapter::EpsonRemote(config) = &source.adapter {
                config.validate()?;
            }
            if let Adapter::Slmp(config) = &source.adapter {
                config.validate()?;
            }
            if let Adapter::YaskawaHses(config) = &source.adapter {
                config.validate_for(source.timeout_ms)?;
            }
            if let Adapter::MtConnect(config) = &source.adapter {
                config.validate()?;
            }
            if let Adapter::LivoxUdp(config) = &source.adapter {
                config.validate()?;
            }
            if let Adapter::SickCompact(config) = &source.adapter {
                config.validate()?;
            }
            if let Adapter::SickCompactTcp(config) = &source.adapter {
                config.validate()?;
            }
            if !ids.insert(&source.id) {
                return Err(Usage(format!("duplicate source id: {}", source.id)).into());
            }
            if let Adapter::FinsUdp(config) = &source.adapter
                && !udp_fins_nodes.insert((
                    config.address,
                    config.source_network,
                    config.source_node,
                ))
            {
                return Err(Usage(
                    "FINS/UDP sources sharing a peer and source network/node must use one signal list"
                        .into(),
                )
                .into());
            }
            if !(1..=3_600_000).contains(&source.every_ms)
                || !(1..=30_000).contains(&source.timeout_ms)
            {
                return Err(
                    Usage("every_ms must be 1–3600000 and timeout_ms 1–30000".into()).into(),
                );
            }
        }
        camera_session::validate_keys(&self.sources)?;
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Row {
    Header {
        version: u32,
        run_id: String,
        started_at: String,
        clock: String,
        sources: Vec<SourceInfo>,
    },
    Sample(Sample),
    End {
        samples: u64,
        status: Status,
        elapsed_ns: u64,
    },
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceInfo {
    id: String,
    protocol: String,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Sample {
    seq: u64,
    source: String,
    signal: String,
    time: String,
    elapsed_ns: u64,
    read_duration_ns: u64,
    #[serde(default)]
    delivery_latency_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_time: Option<String>,
    value: Value,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Complete,
    Interrupted,
    Failed,
}

fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}
fn time_at(origin: DateTime<Utc>, elapsed: Duration) -> String {
    let delta = chrono::Duration::from_std(elapsed).unwrap_or(chrono::Duration::MAX);
    origin
        .checked_add_signed(delta)
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
        .to_rfc3339_opts(SecondsFormat::Nanos, true)
}
fn write_bytes(file: &mut storage::Recording, bytes: &[u8]) -> Result<()> {
    let result = file.write_all(bytes);
    if let Some(error) = file.alarm().error() {
        return Err(error);
    }
    result?;
    Ok(())
}
fn write_row(file: &mut storage::Recording, row: &Row) -> Result<()> {
    let mut bytes = serde_json::to_vec(row)?;
    if bytes.len() > MAX_LINE {
        return Err(Categorized::error(
            ErrorKind::Incomplete,
            "data row exceeds 1 MiB",
        ));
    }
    bytes.push(b'\n');
    write_bytes(file, &bytes)
}

struct Batch {
    source: String,
    elapsed: Duration,
    read_duration: Duration,
    delivery_latency: Duration,
    readings: Vec<Reading>,
    checkpoint: Option<mpsc::SyncSender<()>>,
}
#[derive(Clone, Copy)]
struct AcquisitionClock {
    start: Instant,
    end: Instant,
    host_origin: Option<u64>,
}
struct SourceOutput<'a> {
    source: &'a str,
    clock: AcquisitionClock,
    sender: &'a mpsc::SyncSender<Batch>,
    stop: &'a AtomicBool,
}
impl SourceOutput<'_> {
    fn send(
        &self,
        adapter: &mut dyn Source,
        mut readings: Vec<Reading>,
        began: Instant,
        began_ns: Option<u64>,
    ) -> Result<()> {
        let deadline = adapter.checkpoint_deadline();
        if readings.is_empty() {
            return if deadline.is_some() {
                Err(Categorized::error(
                    ErrorKind::Incomplete,
                    "source requested a writer checkpoint without a nonempty batch",
                ))
            } else {
                Ok(())
            };
        }
        let received = Instant::now();
        let delivered_ns = crate::host::monotonic_ns();
        let (elapsed, read_duration, delivery_latency) =
            match (self.clock.host_origin, began_ns, delivered_ns) {
                (Some(origin), Some(began), Some(delivered)) => {
                    let observed = adapter.observed_ns().unwrap_or(delivered);
                    if observed < origin
                        || (observed < began && !adapter.buffered_timestamps())
                        || observed > delivered
                    {
                        return Err(Categorized::error(
                            ErrorKind::Incomplete,
                            "source returned a host receive time outside its acquisition window",
                        ));
                    }
                    (
                        Duration::from_nanos(observed - origin),
                        Duration::from_nanos(observed.saturating_sub(began)),
                        Duration::from_nanos(delivered - observed),
                    )
                }
                _ if adapter.observed_ns().is_some() => {
                    return Err(Categorized::error(
                        ErrorKind::Incomplete,
                        "host monotonic clock became unavailable",
                    ));
                }
                _ => (
                    received.saturating_duration_since(self.clock.start),
                    received.saturating_duration_since(began),
                    Duration::ZERO,
                ),
            };
        for reading in &mut readings {
            validate_name(&reading.signal, "signal")?;
            if let Some(time) = &mut reading.source_time {
                *time = DateTime::parse_from_rfc3339(time)
                    .context("source_time must be RFC 3339")?
                    .with_timezone(&Utc)
                    .to_rfc3339_opts(SecondsFormat::Nanos, true);
            }
        }
        if readings.len() > 4096 || serde_json::to_vec(&readings)?.len() > MAX_LINE {
            return Err(Categorized::error(
                ErrorKind::Incomplete,
                "source batch exceeds 4096 readings or 1 MiB",
            ));
        }
        let receipt = if let Some(deadline) = deadline {
            self.checkpoint_remaining(deadline)?;
            Some(mpsc::sync_channel(1))
        } else {
            None
        };
        let batch = Batch {
            source: self.source.into(),
            elapsed,
            read_duration,
            delivery_latency,
            readings,
            checkpoint: receipt.as_ref().map(|(send, _)| send.clone()),
        };
        self.sender.try_send(batch).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => Categorized::error(ErrorKind::Incomplete,"data writer fell behind: bounded queue full; recording stopped without silently dropping samples"),
            mpsc::TrySendError::Disconnected(_) => Categorized::error(ErrorKind::Incomplete,"data writer stopped before source observations were delivered"),
        })?;
        if let (Some(deadline), Some((send, receive))) = (deadline, receipt) {
            // The queued batch must own the only sender, so writer loss is observable.
            drop(send);
            loop {
                let left = self.checkpoint_remaining(deadline)?;
                match receive.recv_timeout(left.min(Duration::from_millis(10))) {
                    Ok(()) => {
                        // A receipt that arrived after cancellation/cutoff cannot authorize an ACK.
                        self.checkpoint_remaining(deadline)?;
                        return adapter.checkpointed();
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(Categorized::error(
                            ErrorKind::Incomplete,
                            "data writer stopped before confirming the source checkpoint",
                        ));
                    }
                }
            }
        }
        Ok(())
    }
    fn checkpoint_remaining(&self, deadline: Instant) -> Result<Duration> {
        let deadline = deadline.min(self.clock.end);
        if self.stop.load(Ordering::Acquire) {
            return Err(Categorized::error(
                ErrorKind::Interrupted,
                "source checkpoint cancelled before acknowledgment",
            ));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Categorized::error(
                ErrorKind::Timeout,
                "source checkpoint exceeded its original operation deadline",
            ));
        }
        Ok(left)
    }
}

fn write_batch(
    file: &mut storage::Recording,
    batch: Batch,
    samples: &mut u64,
    origin: DateTime<Utc>,
    stop: &AtomicBool,
) -> Result<()> {
    let result = (|| {
        for reading in batch.readings {
            let sample = Row::Sample(Sample {
                seq: *samples + 1,
                source: batch.source.clone(),
                signal: reading.signal,
                time: time_at(origin, batch.elapsed),
                elapsed_ns: nanos(batch.elapsed),
                read_duration_ns: nanos(batch.read_duration),
                delivery_latency_ns: nanos(batch.delivery_latency),
                source_time: reading.source_time,
                value: reading.value,
            });
            write_row(file, &sample)?;
            *samples += 1;
        }
        if let Some(receipt) = batch.checkpoint {
            file.checkpoint()?;
            // A timed-out source can drop its receiver while the filesystem is syncing.
            let _ = receipt.try_send(());
        }
        Ok(())
    })();
    if result.is_err() {
        // Wake opt-in sources promptly; retain the actual writer failure as primary.
        stop.store(true, Ordering::Release);
    }
    result
}
fn combine_source_results(first: Result<()>, next: Result<()>, phase: &str) -> Result<()> {
    match (first, next) {
        (Ok(()), result) | (result, Ok(())) => result,
        (Err(first), Err(next)) => Err(crate::types::with_details(
            first,
            json!({format!("source_{phase}_error"): crate::types::without_nulls(json!({
                "message": crate::scrub::scrub(&format!("{next:#}")),
                "tag": crate::types::error_tag(&next)
            }))}),
        )),
    }
}

fn run_source(
    mut adapter: Box<dyn Source>,
    source: &SourceConfig,
    sender: &mpsc::SyncSender<Batch>,
    stop: &AtomicBool,
    clock: AcquisitionClock,
) -> Result<()> {
    let every = if source.adapter.is_polled() {
        Duration::from_millis(source.every_ms)
    } else {
        Duration::ZERO
    };
    adapter.set_acquisition_end(clock.host_origin.and_then(|origin| {
        origin.checked_add(nanos(clock.end.saturating_duration_since(clock.start)))
    }));
    let output = SourceOutput {
        source: &source.id,
        clock,
        sender,
        stop,
    };
    let mut next = clock.start;
    let mut successful_read = false;
    let acquired = (|| {
        while !stop.load(Ordering::Acquire) && Instant::now() < clock.end {
            if Instant::now() < next {
                thread::park_timeout(
                    next.saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(10)),
                );
                continue;
            }
            let began = Instant::now();
            let began_ns = crate::host::monotonic_ns();
            let timeout = Duration::from_millis(source.timeout_ms)
                .min(clock.end.saturating_duration_since(began));
            if timeout.is_zero() {
                break;
            }
            let readings = match adapter.read(timeout) {
                Ok(readings) => {
                    successful_read = true;
                    readings
                }
                Err(error)
                    if ((Instant::now() >= clock.end
                        && successful_read
                        && adapter.end_timeout_is_idle())
                        || stop.load(Ordering::Acquire))
                        && crate::types::classify(&error).kind == ErrorKind::Timeout =>
                {
                    break;
                }
                Err(error) => return Err(error),
            };
            output.send(adapter.as_mut(), readings, began, began_ns)?;
            next = (next + every).max(Instant::now());
        }
        Ok(())
    })();
    // Finalize even after a peer failure or interruption; a complete time-series
    // footer can only follow finalized video/index files and all their rows.
    let finalized = adapter.finish(Duration::from_secs(30));
    let drained = (|| {
        loop {
            let began = Instant::now();
            let began_ns = crate::host::monotonic_ns();
            let Some(reading) = adapter.drain()? else {
                break;
            };
            output.send(adapter.as_mut(), vec![reading], began, began_ns)?;
        }
        Ok(())
    })();
    let ready = if !stop.load(Ordering::Acquire) && (!successful_read || !adapter.ready()) {
        Err(Categorized::error(
            ErrorKind::Timeout,
            "source did not become ready before recording ended",
        ))
    } else {
        Ok(())
    };
    let finalized = combine_source_results(acquired, finalized, "finish");
    combine_source_results(finalized, drained, "drain").and(ready)
}
struct Workers {
    stop: Arc<AtomicBool>,
    handles: Vec<thread::JoinHandle<Result<()>>>,
}
impl Workers {
    fn finish(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        let mut failure = None;
        for handle in self.handles.drain(..) {
            let result = handle.join().unwrap_or_else(|_| {
                Err(Categorized::error(
                    ErrorKind::WorkerCrashed,
                    "data source worker panicked",
                ))
            });
            if failure.is_none() {
                failure = result.err();
            }
        }
        failure.map_or(Ok(()), Err)
    }
}
impl Drop for Workers {
    fn drop(&mut self) {
        drop(self.finish());
    }
}

/// A bounded recording: callers own signal policy, all adapters share this origin and deadline.
pub(crate) fn record(
    config: Config,
    path: &Path,
    duration: Duration,
    interrupted: impl Fn() -> bool,
) -> Result<Value> {
    config.validate()?;
    if duration.is_zero() || duration > Duration::from_secs(31_536_000) {
        return Err(
            Usage("data recording duration must be positive and at most 365 days".into()).into(),
        );
    }
    // Prepare adapters before creating the output. RTU setup opens its serial port;
    // network connections and camera acquisition start in their source workers.
    let mut cameras = camera_session::Cameras::default();
    let adapters = config
        .sources
        .iter()
        .map(|source| source.adapter.open(&mut cameras, source.timeout_ms))
        .collect::<Result<Vec<_>>>()?;
    drop(cameras);
    let mut file = storage::Recording::create(path, None, None)?;
    let origin = Utc::now();
    let start = Instant::now();
    let host_origin = crate::host::monotonic_ns();
    let end = start + duration;
    let sources = config
        .sources
        .iter()
        .map(|source| SourceInfo {
            id: source.id.clone(),
            protocol: source.adapter.name().into(),
        })
        .collect();
    write_row(
        &mut file,
        &Row::Header {
            version: 1,
            run_id: crate::types::random_hex(16)?,
            started_at: time_at(origin, Duration::ZERO),
            clock: "host_monotonic_receive".into(),
            sources,
        },
    )?;
    let (send, receive) = mpsc::sync_channel(QUEUE);
    let mut workers = Workers {
        stop: Arc::new(AtomicBool::new(false)),
        handles: Vec::new(),
    };
    for (source, adapter) in config.sources.into_iter().zip(adapters) {
        let send = send.clone();
        let stop = workers.stop.clone();
        let clock = AcquisitionClock {
            start,
            end,
            host_origin,
        };
        let handle = thread::Builder::new()
            .name(format!("data-{}", source.id))
            .spawn(move || {
                let id = source.id.clone();
                run_source(adapter, &source, &send, &stop, clock)
                    .with_context(|| format!("source {id}"))
            })?;
        workers.handles.push(handle);
    }
    drop(send);
    let mut samples = 0;
    let mut failure = None;
    let mut status = Status::Complete;
    loop {
        if Instant::now() < end && workers.handles.iter().any(thread::JoinHandle::is_finished) {
            workers.stop.store(true, Ordering::Release);
        }
        if interrupted() {
            status = Status::Interrupted;
            workers.stop.store(true, Ordering::Release);
        }
        match receive.recv_timeout(Duration::from_millis(20)) {
            Ok(batch) => {
                if let Err(error) =
                    write_batch(&mut file, batch, &mut samples, origin, &workers.stop)
                {
                    failure = Some(error);
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if workers.handles.iter().any(thread::JoinHandle::is_finished) {
                    // A worker can finish normally only at the common deadline or after cancellation.
                    if Instant::now() < end && status == Status::Complete {
                        workers.stop.store(true, Ordering::Release);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let joined = workers.finish();
    if failure.is_none() {
        failure = joined.err();
    }
    if failure.is_some() {
        status = Status::Failed;
    }
    // Even on source failure, complete rows stay readable, with an explicit failed footer.
    let footer = write_row(
        &mut file,
        &Row::End {
            samples,
            status,
            elapsed_ns: host_origin.zip(crate::host::monotonic_ns()).map_or_else(
                || nanos(start.elapsed()),
                |(origin, now)| now.saturating_sub(origin),
            ),
        },
    );
    if failure.is_none() {
        failure = footer.err();
    }
    let finished = file.finish();
    let bytes = match finished {
        Ok(bytes) => bytes,
        Err(error) => {
            if failure.is_none() {
                failure = Some(error);
            }
            0
        }
    };
    if failure.is_some() {
        status = Status::Failed;
    }
    let result = json!({"path": path, "samples": samples, "bytes": bytes, "status": status, "clock": "host_monotonic_receive"});
    if let Some(error) = failure {
        return Err(crate::types::with_details(error, result));
    }
    if status == Status::Interrupted {
        return Err(crate::types::with_details(
            Categorized::error(
                ErrorKind::Interrupted,
                "data recording interrupted; complete samples were saved",
            ),
            result,
        ));
    }
    Ok(result)
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum Format {
    Csv,
    Jsonl,
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, message)
}

/// Read one bounded JSON line, never allocating in proportion to an untrusted file.
fn line(reader: &mut impl std::io::BufRead, allow_unfinished: bool) -> Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    reader
        .take((MAX_LINE + 2) as u64)
        .read_until(b'\n', &mut bytes)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() > MAX_LINE + 1 {
        return Err(invalid("data file has an oversized row"));
    }
    if bytes.last() != Some(&b'\n') {
        if allow_unfinished {
            return Ok(None);
        }
        return Err(invalid("data file has an unfinished row"));
    }
    Ok(Some(bytes))
}
fn csv_field(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

pub(crate) fn export(
    input: &Path,
    output: &Path,
    format: Format,
    allow_incomplete: bool,
    source: Option<&str>,
    signal: Option<&str>,
) -> Result<Value> {
    let mut reader = BufReader::new(
        File::open(input).with_context(|| format!("cannot read {}", input.display()))?,
    );
    let header = line(&mut reader, false)?.ok_or_else(|| invalid("empty data file"))?;
    let header: Row = serde_json::from_slice(&header).context("invalid data header")?;
    let Row::Header {
        version: 1,
        sources,
        started_at,
        clock,
        run_id,
    } = header
    else {
        return Err(invalid("expected data header version 1"));
    };
    if run_id.len() != 32 || !run_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("invalid data run id"));
    }
    let origin = DateTime::parse_from_rfc3339(&started_at)
        .context("invalid started_at")?
        .with_timezone(&Utc);
    if clock != "host_monotonic_receive" || sources.is_empty() || sources.len() > MAX_SOURCES {
        return Err(invalid("invalid data clock or sources"));
    }
    for source in &sources {
        validate_name(&source.id, "source id")?;
        if ![
            "camera",
            "video",
            "simulator",
            "modbus_tcp",
            "modbus_rtu",
            "modbus_ascii",
            "s7",
            "ads",
            "bacnet_ip",
            "pccc",
            "pccc_csp",
            "df1",
            "iec62056_21",
            "sml_serial",
            "sdi12_serial",
            "hart_serial",
            "vaisala_wxt",
            "ifm_iot",
            "iolink_json",
            "abb_rws",
            "epson_remote",
            "yaskawa_hses",
            "mtconnect",
            "slmp",
            "mc_1e",
            "fx_computer_link",
            "mewtocol",
            "fins_tcp",
            "fins_udp",
            "omron_host_link",
            "omron_compoway_f",
            "keyence_iv",
            "keyence_kv",
            "fatek_fbs",
            "ur_rtde",
            "atlas_open_protocol",
            "optommp",
            "gopxl_ascii",
            "ls_cnet",
            "ls_xgt",
            "yokogawa_fam3",
            "cognex_insight",
            "cognex_dataman",
            "can",
            "tcp_jsonl",
            "serial_jsonl",
            "scpi",
            "sick_cola",
            "sick_compact_udp",
            "sick_compact_tcp",
            "microepsilon_rs422",
            "mt_sics",
            "aandd_balance",
            "sartorius_sbi",
            "pfeiffer_tpg",
            "nmea_gnss",
            "velodyne_vlp16",
            "velodyne_hdl32e",
            "ouster_udp",
            "hesai_udp",
            "livox_udp",
            "robosense_udp",
            "gocator_gdp",
            "ni_daqmx",
            "helper",
            "mqtt",
            "opc_ua",
            "ethercat",
            "profinet",
            "ethernet_ip",
            "ethernet_ip_assembly",
        ]
        .contains(&source.protocol.as_str())
        {
            return Err(invalid("unknown source protocol"));
        }
    }
    let ids = sources
        .iter()
        .map(|source| source.id.as_str())
        .collect::<BTreeSet<_>>();
    if ids.len() != sources.len() {
        return Err(invalid("duplicate data source ids"));
    }
    let mut file = storage::Recording::create(output, None, None)?;
    let result = (|| {
        if matches!(format, Format::Csv) {
            write_bytes(
                &mut file,
                b"seq,source,signal,time,elapsed_ns,read_duration_ns,delivery_latency_ns,source_time,value_json\n",
            )?;
        }
        let mut source_elapsed = BTreeMap::new();
        let mut max_elapsed = 0;
        let mut seen = 0;
        let mut exported = 0;
        let mut ended = false;
        let mut complete = false;
        while let Some(bytes) = line(&mut reader, allow_incomplete && !ended)? {
            if ended {
                return Err(invalid("data follows the end row"));
            }
            let row: Row = serde_json::from_slice(&bytes).context("invalid data row")?;
            match row {
                Row::Sample(sample) => {
                    seen += 1;
                    if sample.seq != seen
                        || !ids.contains(sample.source.as_str())
                        || sample.signal.is_empty()
                    {
                        return Err(invalid("invalid sample sequence, source or signal"));
                    }
                    validate_name(&sample.signal, "signal")?;
                    let received = DateTime::parse_from_rfc3339(&sample.time)
                        .context("invalid sample time")?
                        .with_timezone(&Utc);
                    let expected = origin
                        .checked_add_signed(chrono::Duration::nanoseconds(
                            i64::try_from(sample.elapsed_ns)
                                .map_err(|_| invalid("sample elapsed time is too large"))?,
                        ))
                        .ok_or_else(|| invalid("sample time is out of range"))?;
                    if received != expected || sample.read_duration_ns > sample.elapsed_ns {
                        return Err(invalid(
                            "sample clock does not match its recording origin or read duration",
                        ));
                    }
                    if source_elapsed
                        .insert(sample.source.clone(), sample.elapsed_ns)
                        .is_some_and(|previous| previous > sample.elapsed_ns)
                    {
                        return Err(invalid("source elapsed time moved backwards"));
                    }
                    max_elapsed = max_elapsed.max(
                        sample
                            .elapsed_ns
                            .checked_add(sample.delivery_latency_ns)
                            .ok_or_else(|| invalid("sample delivery time overflows"))?,
                    );
                    if let Some(time) = &sample.source_time {
                        DateTime::parse_from_rfc3339(time).context("invalid source_time")?;
                    }
                    if source.is_some_and(|source| source != sample.source)
                        || signal.is_some_and(|signal| signal != sample.signal)
                    {
                        continue;
                    }
                    match format {
                        Format::Jsonl => {
                            let mut bytes = serde_json::to_vec(&sample)?;
                            bytes.push(b'\n');
                            write_bytes(&mut file, &bytes)?;
                        }
                        Format::Csv => {
                            let value = serde_json::to_string(&sample.value)?;
                            let row = format!(
                                "{},{},{},{},{},{},{},{},{}\n",
                                sample.seq,
                                csv_field(&sample.source),
                                csv_field(&sample.signal),
                                csv_field(&sample.time),
                                sample.elapsed_ns,
                                sample.read_duration_ns,
                                sample.delivery_latency_ns,
                                csv_field(sample.source_time.as_deref().unwrap_or_default()),
                                csv_field(&value)
                            );
                            write_bytes(&mut file, row.as_bytes())?;
                        }
                    }
                    exported += 1;
                }
                Row::End {
                    samples,
                    status,
                    elapsed_ns,
                } => {
                    if elapsed_ns < max_elapsed {
                        return Err(invalid("end row elapsed time precedes a sample"));
                    }
                    if samples != seen {
                        return Err(invalid("end row sample count differs from data"));
                    }
                    complete = status == Status::Complete;
                    ended = true;
                }
                Row::Header { .. } => return Err(invalid("unexpected second data header")),
            }
        }
        if !complete && !allow_incomplete {
            return Err(invalid(
                "recording is incomplete; use --allow-incomplete to export its complete rows",
            ));
        }
        Ok((exported, complete))
    })();
    let (samples, complete) = match result {
        Ok(result) => result,
        Err(error) => {
            file.abort()?;
            return Err(error);
        }
    };
    let bytes = file.finish()?;
    Ok(
        json!({"path": output, "samples": samples, "bytes": bytes, "complete": complete, "format": match format { Format::Csv => "csv", Format::Jsonl => "jsonl" }}),
    )
}
