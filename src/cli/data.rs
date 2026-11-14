//! Industrial, still-image and indexed-video capture on one host timeline.
use super::{Global, args, output::Report, signals};
use crate::timeseries::{self, Format};
use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::json;
use std::{path::PathBuf, time::Duration};

#[derive(Debug, Subcommand)]
pub(crate) enum DataCommand {
    /// Record signals on one host timeline
    Record(RecordArgs),
    /// Export samples as CSV or JSONL
    Export(ExportArgs),
    /// Show adapters and an example config
    Protocols,
}

#[derive(Debug, Args)]
pub(crate) struct RecordArgs {
    /// Version 1 JSON source configuration (see data protocols)
    #[arg(long, value_name = "JSON")]
    config: PathBuf,
    /// New JSON Lines recording file; existing files are never overwritten
    #[arg(short, long, value_name = "PATH")]
    output: PathBuf,
    /// Recording duration, such as 30s or 1h
    #[arg(long = "for", value_parser = args::duration, value_name = "DUR")]
    duration: Duration,
}
#[derive(Debug, Args)]
pub(crate) struct ExportArgs {
    /// Recording written by data record
    input: PathBuf,
    /// New export file; existing files are never overwritten
    #[arg(short, long, value_name = "PATH")]
    output: PathBuf,
    /// CSV quotes all text cells; JSONL emits one sample object per line
    #[arg(long, value_enum, default_value = "csv")]
    format: Format,
    /// Export complete rows from a failed/interrupted recording or one missing its end row
    #[arg(long)]
    allow_incomplete: bool,
    /// Include only this source id
    #[arg(long)]
    source: Option<String>,
    /// Include only this signal name
    #[arg(long)]
    signal: Option<String>,
}

pub(super) const RECORD_FIELDS: &[(&str, &str)] = &[
    ("path", "string — JSONL recording file"),
    ("samples", "int — complete samples written"),
    ("bytes", "int — file bytes"),
    (
        "status",
        "complete; failed/interrupted work is in error.details",
    ),
    (
        "clock",
        "host_monotonic_receive — comparable host timestamps, no hardware clock synchronization implied",
    ),
];
pub(super) const EXPORT_FIELDS: &[(&str, &str)] = &[
    ("path", "string — export file"),
    ("samples", "int — exported samples"),
    ("bytes", "int — export bytes"),
    ("complete", "bool — source recording ended successfully"),
    ("format", "csv|jsonl"),
];
pub(super) const PROTOCOL_FIELDS: &[(&str, &str)] = &[
    ("version", "1 — configuration and recording version"),
    (
        "protocols",
        "[{protocol,implementation,requirements}] — native, native_passive or external_helper",
    ),
    (
        "example",
        "{version,sources} — runnable simulator configuration",
    ),
];

#[derive(serde::Serialize)]
struct ProtocolDescription {
    protocol: &'static str,
    implementation: &'static str,
    requirements: &'static str,
}

// A typed table keeps catalogue growth out of json!'s recursive token expansion.
const PROTOCOLS: &[ProtocolDescription] = &[
    ProtocolDescription {
        protocol: "optommp",
        implementation: "native",
        requirements: "explicit groov_expanded_inputs profile and EPIC PR1/PR2 or RIO MM1/MM2 model; numeric TCP endpoint; externally cleared PUC and stable channel configuration; closed input module/type allowlist; native F32/U32 and quality codes with original response clocks; no writes, read-clear, calibration, retry or reconnect",
    },
    ProtocolDescription {
        protocol: "gopxl_ascii",
        implementation: "native",
        requirements: "explicit gopxl_1_3_measurement profile; numeric Ethernet ASCII TCP endpoint with asynchronous output disabled; declared delimiter, terminator, invalid token and trailing-delimiter policy; connection-map measurement IDs; native x1000 values and decision bits; measurement query only, no start/stop/trigger/job/property commands, retry or reconnect",
    },
    ProtocolDescription {
        protocol: "atlas_open_protocol",
        implementation: "native",
        requirements: "explicit pf6000_v2_8 profile; numeric Virtual Station TCP endpoint; operator-provisioned Disconnect=None; startup5/result3; native scaled values and raw frames; OS-reported JSONL checkpoint before application ACK under original deadline; no history, reconnect or tool commands",
    },
    ProtocolDescription {
        protocol: "simulator",
        implementation: "native",
        requirements: "none; deterministic counter",
    },
    ProtocolDescription {
        protocol: "modbus_tcp",
        implementation: "native",
        requirements: "numeric IP:port for read-only scalar and contiguous array reads, optional per-signal gateway unit and verified TLS with client certificates (mqtt-tls feature)",
    },
    ProtocolDescription {
        protocol: "modbus_rtu",
        implementation: "native",
        requirements: "Unix terminal or Windows COM port for read-only scalar and contiguous array reads of bits and registers, with optional per-signal unit",
    },
    ProtocolDescription {
        protocol: "modbus_ascii",
        implementation: "native",
        requirements: "Unix terminal or Windows COM port with ASCII framing and LRC; read-only scalar and contiguous arrays with optional per-signal unit",
    },
    ProtocolDescription {
        protocol: "s7",
        implementation: "native",
        requirements: "numeric IP:port; classic S7 ReadVar for absolute scalars/contiguous typed arrays, fixed-layout temporal values, DB STRING/WSTRING and indexed timer/counter words; configured rack/slot or TSAPs",
    },
    ProtocolDescription {
        protocol: "ads",
        implementation: "native",
        requirements: "numeric IP:port, configured AMS Net IDs and route; read-only Beckhoff ADS values by index group/offset or symbol",
    },
    ProtocolDescription {
        protocol: "bacnet_ip",
        implementation: "native",
        requirements: "numeric unicast IPv4 UDP endpoint; direct read-only BACnet analog, binary and multi-state properties with optional units/status and local bind",
    },
    ProtocolDescription {
        protocol: "slmp",
        implementation: "native",
        requirements: "numeric IP:port; Mitsubishi MC/SLMP 3E binary or ASCII TCP device reads with configured route and device format; S and STS/STC/STN step/retentive devices, plus explicit iq_r long_timer LTN/LSTN whole four-word records with raw current pair and coil/contact bits. No inferred timebase or CPU capability; failed sources retire without reconnecting",
    },
    ProtocolDescription {
        protocol: "mc_1e",
        implementation: "native",
        requirements: "numeric TCP IP:port; MELSEC-F FX3U-ENET or ENET-ADP A-compatible 1E binary or ASCII read-only word and bit devices",
    },
    ProtocolDescription {
        protocol: "fx_computer_link",
        implementation: "native",
        requirements: "Unix terminal or Windows COM port for configured MELSEC-F Computer Link formats 1 and 4, BR/WR/QR device reads and optional per-signal station",
    },
    ProtocolDescription {
        protocol: "mewtocol",
        implementation: "native",
        requirements: "Panasonic FP MEWTOCOL-COM bare TCP or native serial; read-only registers and contacts, configured unit and frame size",
    },
    ProtocolDescription {
        protocol: "fins_tcp",
        implementation: "native",
        requirements: "numeric IP:port; Omron FINS/TCP node negotiation and read-only CS/CJ/CP-style DM/CIO/WR/HR/AR, explicit EM-bank memory access and logical timer/counter native 16-bit PVs or completion flags",
    },
    ProtocolDescription {
        protocol: "fins_udp",
        implementation: "native",
        requirements: "numeric unicast IPv4:port; explicit Omron FINS source/target nodes and optional local UDP bind; read-only CS/CJ/CP-style DM/CIO/WR/HR/AR, explicit EM banks and logical timer/counter native 16-bit PVs or completion flags",
    },
    ProtocolDescription {
        protocol: "omron_host_link",
        implementation: "native",
        requirements: "Unix terminal or Windows COM port; configured Omron 1:N Host Link C-mode RD, direct FINS FA including logical native timer/counter PVs and completion flags, or explicit c_mode_cpm/model for CPM1/1A/2A/2C/SRM1 native RR/RL/RH/RC/RG/RD/RJ reads; optional per-signal unit, no inferred timer units",
    },
    ProtocolDescription {
        protocol: "omron_compoway_f",
        implementation: "native",
        requirements: "Unix terminal or Windows COM port; configured Omron E5CC/E5EC or G3ZA/G3PW RS-485 CompoWay/F; read-only Variable Area Read with optional per-signal unit",
    },
    ProtocolDescription {
        protocol: "keyence_kv",
        implementation: "native",
        requirements: "numeric TCP IP:port; configured Keyence KV Host Link for read-only native RD/RDS registers, contacts and timer/counter values",
    },
    ProtocolDescription {
        protocol: "ur_rtde",
        implementation: "native",
        requirements: "numeric TCP endpoint with RTDE enabled; explicit v2 output recipe and expected native field types, requested 1–500 Hz; output synchronization only, original receive clocks and raw IEEE words",
    },
    ProtocolDescription {
        protocol: "fatek_fbs",
        implementation: "native",
        requirements: "configured FBs standard Port0 fixed 7E1 serial or explicit transparent TCP bridge; read-only 44/46 bool/native16/native32 scalars and arrays, literal W/DW views and optional per-signal station",
    },
    ProtocolDescription {
        protocol: "ls_cnet",
        implementation: "native",
        requirements: "configured XGB dedicated Cnet serial server; read-only rSS native integers/bools and explicit-address arrays, mandatory BCC and optional per-signal station",
    },
    ProtocolDescription {
        protocol: "ls_xgt",
        implementation: "native",
        requirements: "LS ELECTRIC XGT/XGB configured FEnet dedicated TCP server (normally port 2004); read-only literal direct variables and typed byte-backed arrays",
    },
    ProtocolDescription {
        protocol: "yokogawa_fam3",
        implementation: "native",
        requirements: "Yokogawa FA-M3 configured higher-level PC Link TCP server on F3LE01/F3LE11; read-only binary or ASCII bits/words, explicit CPU and project device addresses",
    },
    ProtocolDescription {
        protocol: "keyence_iv",
        implementation: "native",
        requirements: "numeric TCP IP:port; Keyence IV2/IV3 or IV4 in Run with Nonprocedural Command enabled and automatic output disabled. IV4 requires standard or standard_master results.",
    },
    ProtocolDescription {
        protocol: "cognex_insight",
        implementation: "native",
        requirements: "numeric TCP IP:port; enabled In-Sight Native Mode Telnet, environment credentials, and configured read-only GV spreadsheet addresses or published names",
    },
    ProtocolDescription {
        protocol: "cognex_dataman",
        implementation: "native",
        requirements: "numeric TCP IP:port (normally 44818), enabled DataMan EtherNet/IP and explicit cip_result profile; read-only complete ID Reader DecodeResults attribute with exact binary data and raw result codes. Acquisition/buffering are configured externally; no trigger or result acknowledgement writes",
    },
    ProtocolDescription {
        protocol: "abb_rws",
        implementation: "native",
        requirements: "numeric unicast IPv4:port, explicit rws1 or rws2 API and environment credentials; read-only ABB controller state, mode, system, RAPID execution and named symbol values/properties. RWS1 uses Digest; RWS2 requires verified TLS (mqtt-tls feature) and Basic. No motion, mastership or program writes",
    },
    ProtocolDescription {
        protocol: "epson_remote",
        implementation: "native",
        requirements: "externally enabled Epson Remote Ethernet in monitor-only mode, preserving the existing control-device owner; explicit documented profile, numeric TCP endpoint, line terminator, environment password and named GET reads. Native status, alarms, I/O and variable text; Login plus qualified GETs only, original poll deadline and sticky failure; no Logout, reset, motion or control commands",
    },
    ProtocolDescription {
        protocol: "yaskawa_hses",
        implementation: "native",
        requirements: "numeric unicast IPv4 UDP endpoint (normally 10040), enabled Yaskawa HSES and explicit yrc1000_hw1483358_rev12 profile with reply_horizon_ms at least timeout_ms; read-only status, current base position and explicit B/I/D/raw-R/whole32-byte-S variables. UDP correlation relies on an operator delay bound within this source epoch; no motion or configuration writes",
    },
    ProtocolDescription {
        protocol: "mtconnect",
        implementation: "native",
        requirements: "numeric unicast HTTP agent endpoint and optional base_path; read-only probe/current/sample XML for published namespaces 1.2-1.8 and 2.0-2.7, exact observations/conditions/structured values with agent epoch and cursor checks. Optional verified TLS, TLS Basic/Bearer or Digest via environment credentials; no machine setup/control or automatic history-loss recovery",
    },
    ProtocolDescription {
        protocol: "can",
        implementation: "native_passive",
        requirements: "Linux SocketCAN interface already configured for the bus; receive-only Classical CAN/CAN FD frames and optional mapped values",
    },
    ProtocolDescription {
        protocol: "iec62056_21",
        implementation: "native",
        requirements: "transparent serial optical probe for IEC 62056-21 Mode C; configured OBIS codes, 300-baud sign-on and negotiated readout",
    },
    ProtocolDescription {
        protocol: "sml_serial",
        implementation: "native_passive",
        requirements: "receive-only 9600 8N1 optical probe on a Unix terminal or Windows COM port; standard SML v1 push files and explicit numeric OBIS mappings",
    },
    ProtocolDescription {
        protocol: "sdi12_serial",
        implementation: "native",
        requirements: "Infwin SDI12ELF20 transparent USB COM interface at 9600 8N1; one or more addressed M/MC sensor measurements with optional CRC",
    },
    ProtocolDescription {
        protocol: "hart_serial",
        implementation: "native",
        requirements: "Microflex 101-0035 managed HART modem in preconfigured raw mode with matching primary/secondary role; read-only universal 0 identity and 1/2/3 process values over native serial",
    },
    ProtocolDescription {
        protocol: "vaisala_wxt",
        implementation: "native",
        requirements: "Vaisala WXT530-series in serial ASCII polled mode; addressed R0 composite data, optional CRC and explicit field names",
    },
    ProtocolDescription {
        protocol: "ifm_iot",
        implementation: "native",
        requirements: "numeric IP:port and IO-Link port on an ifm IoT Core master; read-only process input with explicit IODD-derived mappings",
    },
    ProtocolDescription {
        protocol: "iolink_json",
        implementation: "native",
        requirements: "numeric IP:port and device alias; read-only JSON Integration for IO-Link GET, raw response plus explicit byte mappings or JSON pointers",
    },
    ProtocolDescription {
        protocol: "tcp_jsonl",
        implementation: "native",
        requirements: "numeric IP:port; newline-delimited readings or JSON values with a configured signal",
    },
    ProtocolDescription {
        protocol: "serial_jsonl",
        implementation: "native",
        requirements: "Unix terminal or Windows COM port; newline-delimited readings or JSON values with a configured signal",
    },
    ProtocolDescription {
        protocol: "scpi",
        implementation: "native",
        requirements: "explicit measurement queries over raw TCP, native VXI-11 core RPC, native HiSLIP 1.0, native serial or USBTMC/USB488; USBTMC requires the usb feature, exact VID/PID/serial/interface and externally provisioned exclusive USB access; exact scalar/vector responses and staged local IEEE488.2 definite-length binary artifacts",
    },
    ProtocolDescription {
        protocol: "sick_cola",
        implementation: "native",
        requirements: "numeric TCP endpoint; fixed sRN LMDscandata reads in default CoLa A, explicit LMS1xx/LMS5xx CoLa A or model-specific CoLa B TiM5xx/LMS1xx/LMS5xx mode, retaining raw channels, status and clocks",
    },
    ProtocolDescription {
        protocol: "sick_compact_udp",
        implementation: "native_passive",
        requirements: "externally configured SICK multiScan100/picoScan100 points/imu or explicit picoScan150 points/imu/encoder/mixed Compact UDP; IPv4 listen/source filter and new local output_directory/max_bytes; complete CRC-valid Type1 v3/v4, Type2 v1 or picoScan150 Type4 v1/v2 telegram per datagram; raw words, unsigned ticks and immutable archives, no device writes, inferred encoder units or full-frame/clock-sync inference",
    },
    ProtocolDescription {
        protocol: "sick_compact_tcp",
        implementation: "native_passive",
        requirements: "externally configured SICK LRS4000 Type1 v3/v4 scans or multiScan200 Type6 v1 scans/Type3 v1 ambient/Type7 v1 IMU; explicit profile, points/ambient/imu stream, numeric TCP endpoint and new local output_directory/max_bytes; CRC-valid complete telegram archives and bounded native parts, no device writes or full-frame/clock-sync inference",
    },
    ProtocolDescription {
        protocol: "nmea_gnss",
        implementation: "native_passive",
        requirements: "configured GNSS NMEA GGA/RMC output on numeric TCP endpoint or native serial; checksum-verified raw coordinates, time/date, fix status and exact measurement fields",
    },
    ProtocolDescription {
        protocol: "velodyne_vlp16",
        implementation: "native_passive",
        requirements: "modern VLP-16/Puck LITE product 0x22, passive IPv4 UDP; explicit listen/source endpoints and local output/max_bytes for immutable raw groups with original receive clocks",
    },
    ProtocolDescription {
        protocol: "velodyne_hdl32e",
        implementation: "native_passive",
        requirements: "explicit HDL-32E raw UDP profile for documented product 0x21; local listen/source endpoints and output/max_bytes, with immutable groups and preserved receive clocks",
    },
    ProtocolDescription {
        protocol: "ouster_udp",
        implementation: "native_passive",
        requirements: "preconfigured Ouster OS0/OS1/OS2 lidar or IMU UDP stream; explicit peer and original metadata file, documented packet profile, local output/max_bytes; native fields and immutable raw/metadata groups",
    },
    ProtocolDescription {
        protocol: "hesai_udp",
        implementation: "native_passive",
        requirements: "preconfigured Hesai PandarXT32 v6.1 lidar or GPS UDP stream; explicit packet profile/listen/source peer, local output/max_bytes; native fields and immutable raw groups, optional exact context",
    },
    ProtocolDescription {
        protocol: "livox_udp",
        implementation: "native_passive",
        requirements: "externally enabled Livox UDP sampling stream, explicit SDK2 Mid360/S/L or HAP TX/T1 versus legacy Horizon/Avia profile and points/imu stream; listen/source peer, local immutable raw archive and max_bytes. Native dense arrays/raw clock/tag/header bytes, mandatory SDK2 timestamp+data CRC; no control or heartbeat ownership, scan/sync/device-identity inference",
    },
    ProtocolDescription {
        protocol: "robosense_udp",
        implementation: "native_passive",
        requirements: "preconfigured RoboSense Helios-16P or RS16 MSOP/DIFOP UDP stream; explicit packet profile/listen/source peer, local output/max_bytes; native fields and immutable raw groups, optional exact context and explicit RS16 distance units",
    },
    ProtocolDescription {
        protocol: "pfeiffer_tpg",
        implementation: "native",
        requirements: "dedicated Pfeiffer TPG361/362/366 command interface on numeric TCP IP:port or native USB serial 8N1; read-only identity, unit and selected gauge pressures/statuses with explicit model",
    },
    ProtocolDescription {
        protocol: "microepsilon_rs422",
        implementation: "native_passive",
        requirements: "preconfigured Micro-Epsilon ILD2300 single-DIST1 RS422 stream on a native serial/VCOM endpoint, 9600 or 115200 baud 8N1; passive tagged18-bit raw codes and application errors, no sensor commands or inferred physical units",
    },
    ProtocolDescription {
        protocol: "mt_sics",
        implementation: "native",
        requirements: "dedicated METTLER TOLEDO MT-SICS command interface over numeric TCP IP:port or native serial; immediate net weight and status via SI",
    },
    ProtocolDescription {
        protocol: "aandd_balance",
        implementation: "native",
        requirements: "dedicated A&D standard-format command interface over native serial or an explicitly configured transparent TCP bridge; fixed Q reads retain exact decimal fields, units and application states",
    },
    ProtocolDescription {
        protocol: "sartorius_sbi",
        implementation: "native",
        requirements: "dedicated Sartorius SBI16/22 manual-print interface over configured no-flow serial or transparent TCP bridge, with autoprint and post-print tare disabled; explicit line format and exact values/states",
    },
    ProtocolDescription {
        protocol: "gocator_gdp",
        implementation: "native_passive",
        requirements: "running legacy Gocator profile or snapshot sensor with GDP data output enabled; numeric TCP endpoint and local directory for complete raw groups",
    },
    ProtocolDescription {
        protocol: "ni_daqmx",
        implementation: "native",
        requirements: "ni-daqmx feature and separately installed NI-DAQmx on a supported Linux/Windows host; owned continuous internal-clock AI voltage input by default, explicit thermocouple input with per-channel type/units/CJC or current input with per-channel ampere range/terminal/shunt requests; RTD input with per-channel type/temperature units/wire/R0/excitation requests (internal excitation can energize the sensor); strain/bridge_ratio/acceleration input with explicit sensor constants, all seven strain layouts, both ratio units, all three acceleration units and excitation/coupling requests (none/ground/Stop-Clear are not power-off guarantees); resistance/thermistor_current/thermistor_voltage with explicit wiring, excitation, finite coefficients and Vex reference R1, plus force_bridge/pressure_bridge/torque_bridge with independent output/electrical-calibration/physical-calibration units and exact point requests; velocity_iepe/force_iepe/charge_acceleration/lvdt/rvdt use separate output/sensitivity units and family-specific excitation, AC frequency and wire requests; all eighteen AI kinds also work in MixedAI18 without local conversion or performed calibration claims; explicit mixed measurement with required per-channel measurement tags, same-family range/terminal overrides and one-task channel/quantity/unit/request metadata, preserving legacy rows and repeated values without simultaneous-conversion claims; software-only digital_port whole-port U32 state or counter_count/counter_frequency/counter_period/counter_pulse_width/counter_angular_position/counter_linear_position single-counter input with explicit time/encoder/Z/source requests and no masks, unwrap or local Hz/position; explicit software on-demand timing with poll_interval_ms and one scan per read, without hardware-rate/buffer settings or freshness guarantee; 1–64 individual AI or digital-port channels, exactly one counter per CI task, bounded scan-major blocks with exact native F64 or U32 words and native status; no local unit or V/R conversion, inferred built-in resistance, AO/DO/counter-output tasks, named MAX tasks, device reset, external clock or inferred hardware timestamps",
    },
    ProtocolDescription {
        protocol: "helper",
        implementation: "external_helper",
        requirements: "explicit Unix executable or Windows 10/Server 2016+ absolute .exe emitting bounded JSONL readings; Windows requires creation-time Job Object containment; supports user-supplied vendor SDK and protocol bridges",
    },
    ProtocolDescription {
        protocol: "mqtt",
        implementation: "native",
        requirements: "MQTT 3.1.1, QoS 0–2, numeric IP:port; optional environment credentials and verified TLS (mqtt-tls feature)",
    },
    ProtocolDescription {
        protocol: "camera",
        implementation: "native",
        requirements: "camera selector and image filename template; isolated worker saves full-resolution stills; optional exact action_device_key configures a supported write-only key once on the shared worker before arming, without reconnect/replay; native GigE/GenTL multipart part_format: Mono8, RGB8, YCbCr411_8, Coord3D_C16, Coord3D_C10p, Coord3D_C12p, Confidence8, Confidence16, Mono16, Coord3D_ABC16 or Coord3D_ABC32f; optional part_index 0..31 requires format; exact type/PFNC checked per frame, GenTL standard namespaces 1/4 only, explicit URI bindings remain authoritative; local RAW/PNG, XYZ RAW only",
    },
    ProtocolDescription {
        protocol: "video",
        implementation: "native",
        requirements: "FFmpeg, camera selector, literal local video output; optional exact action_device_key configures a supported write-only key once on the shared worker before arming, without reconnect/replay; per-frame index maps presentation frames to host receive times",
    },
    ProtocolDescription {
        protocol: "opc_ua",
        implementation: "native",
        requirements: "opc.tcp endpoint and named nodes with optional namespace URI resolution; default Read or explicit subscription mode with queued native DataValues, sequence checks and bounded Republish recovery; revised queue >=2 and compatible request timeout required; receipt-only ACK has a crash gap before JSONL persistence; optional pinned SignAndEncrypt",
    },
    ProtocolDescription {
        protocol: "ethercat",
        implementation: "native_passive",
        requirements: "Linux raw packet permission, visible EtherCAT traffic and configured logical PDO map",
    },
    ProtocolDescription {
        protocol: "profinet",
        implementation: "native_passive",
        requirements: "Linux raw packet permission, visible Profinet RT traffic and engineered IO layout",
    },
    ProtocolDescription {
        protocol: "ethernet_ip",
        implementation: "native",
        requirements: "numeric TCP IP:port; read-only Logix atomic tags, explicit DWORD/packed BOOL, STRING and opaque structures, Micro800 typed/STRING and Omron NJ/NX values; optional one-hop slot route",
    },
    ProtocolDescription {
        protocol: "ethernet_ip_assembly",
        implementation: "native",
        requirements: "numeric IP:port; direct unconnected CIP Assembly Data read with configured instance, length and field offsets",
    },
    ProtocolDescription {
        protocol: "pccc",
        implementation: "native",
        requirements: "numeric IP:port; read-only EtherNet/IP Execute_PCCC; SLC/MicroLogix typed files, whole T/C and ST string elements with optional backplane slot; max_read_bytes >=6 for T/C, >=84 for ST; direct PLC-5/E N/B/F only with profile plc5; failed polls retire TCP source",
    },
    ProtocolDescription {
        protocol: "pccc_csp",
        implementation: "native",
        requirements: "numeric IP:port; direct SLC 5/05 legacy CSPv4 TCP, usually port 2222; read-only N/B/F, whole T/C and ST string elements; max_read_bytes >=6 for T/C, >=84 for ST; failed polls retire TCP source",
    },
    ProtocolDescription {
        protocol: "df1",
        implementation: "native",
        requirements: "Unix terminal or Windows COM port; direct Rockwell DF1 Full Duplex RS-232 with no handshaking, explicit node IDs and CRC or BCC; read-only SLC/MicroLogix typed files, whole T/C and ST string elements; max_read_bytes >=6 for T/C, >=84 for ST",
    },
];

pub(super) fn run(global: &Global, command: DataCommand) -> Result<Report> {
    match command {
        DataCommand::Record(args) => {
            let config = timeseries::load(&args.config)?;
            signals::take_over();
            global.output().note(format!("Recording industrial data to {}", args.output.display()));
            let result = timeseries::record(config, &args.output, args.duration, signals::interrupted)?;
            let text = format!("Saved {} samples to {}", result["samples"], args.output.display());
            Ok(Report::json(result).text(text))
        }
        DataCommand::Export(args) => {
            let result = timeseries::export(&args.input, &args.output, args.format, args.allow_incomplete, args.source.as_deref(), args.signal.as_deref())?;
            let text = format!("Exported {} samples to {}", result["samples"], args.output.display());
            Ok(Report::json(result).text(text))
        }
        DataCommand::Protocols => Ok(Report::json(json!({
            "version": 1,
            "protocols": PROTOCOLS,
            "example": {"version":1,"sources":[{"id":"press","protocol":"simulator","signal":"cycle","every_ms":100}]}
        })).text("Native: Atlas Copco PF6000 tightening results, camera stills, indexed video, Keyence KV PLC data and inspection results, LS ELECTRIC XGT/XGB FEnet and XGB Cnet rSS, FATEK FBs standard Port0 reads, Universal Robots RTDE output telemetry, Yokogawa FA-M3 PC Link, Cognex In-Sight cells, simulator, Modbus TCP/RTU/ASCII, S7, ADS, Mitsubishi SLMP, MC 1E and FX Computer Link, Panasonic MEWTOCOL-COM, Omron FINS, Host Link and CompoWay/F, BACnet/IP, IEC 62056-21 and passive SML optical meters, SDI-12 sensor reads through a serial bridge, HART process measurements through a managed modem, Vaisala WXT530, IO-Link process input, TCP/serial JSONL, SCPI instruments, MT-SICS scales, A&D standard-format and Sartorius SBI balances, SICK CoLa A/B scans, passive SICK Compact scan/IMU datagrams, NMEA GNSS positions, VLP-16/HDL-32E raw UDP groups, MQTT, Logix, Micro800 and Omron NJ/NX EtherNet/IP variables and Assembly data, SLC/MicroLogix and PLC-5/E PCCC files over EtherNet/IP, legacy CSPv4 for SLC 5/05, serial DF1 for SLC/MicroLogix, OPC-UA reads and explicit queued subscriptions.\nNI-DAQmx feature (included by default): owned continuous internal-clock or explicit software on-demand voltage, thermocouple, current, RTD, strain, bridge_ratio, acceleration, resistance, thermistor_current, thermistor_voltage, force_bridge, pressure_bridge, torque_bridge, velocity_iepe, force_iepe, charge_acceleration, lvdt, rvdt or mixed AI input through an installed Linux/Windows driver, preserving exact F64 words, per-channel measurement/quantity/units/CJC/shunt/R0/excitation requests and native status in one task, with explicit coefficients/reference resistance and independent returned/electrical/physical calibration units, no local physics or calibration claims; software-only digital_port/counter_count/counter_frequency/counter_period/counter_pulse_width/counter_angular_position/counter_linear_position captures native U32 states/count snapshots or F64 Hz/seconds/position with explicit encoder Z and source requests, without pin masks, unwrap, local conversion or physical qualification.\nPassive Gocator GDP over TCP.\nLinux passive capture: CAN/CAN FD, EtherCAT and Profinet RT.\nExplicit Unix or Windows helpers accept user-supplied vendor SDK and protocol bridges.\nRun with --json to get an example config. See docs/industrial-data.md for adapter options.")),
    }
}
