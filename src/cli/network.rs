//! `capturefab network`: `GigE` interface checks, and with `--tune` the
//! per-NIC bandwidth sharing applied to cameras.
#![forbid(unsafe_code)]
use super::{Global, output::Report};
use crate::session::{SessionCommand, SessionHandle};
use anyhow::Result;
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::{net::Ipv4Addr, time::Duration};

#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub(crate) struct NetworkArgs {
    #[command(subcommand)]
    pub action: Option<NetworkCommand>,
    /// Apply bandwidth sharing (packet delay, throughput limits) to the cameras given with -c, or a session's cameras; they keep it until power-off
    #[arg(long)]
    pub tune: bool,
    /// `GigE` camera to tune, repeatable (with --tune; not read from `CAPTUREFAB_CAMERA`, so the read-only checks stay read-only)
    #[arg(
        help = "GigE camera to tune, repeatable (with --tune; not read from CAPTUREFAB_CAMERA, so the read-only checks stay read-only)",
        short = 'c',
        long = "camera",
        value_name = "CAM",
        requires = "tune"
    )]
    pub cameras: Vec<String>,
    /// How long `GigE` discovery collects answers
    #[arg(help = "How long GigE discovery collects answers", long, default_value = "1s", value_name = "DUR", value_parser = super::args::duration)]
    pub timeout: Duration,
}

#[derive(Debug, Subcommand)]
pub(crate) enum NetworkCommand {
    /// Send one immediate or scheduled `GigE` Vision action
    #[command(
        after_help = "Runs locally alongside already-held acquisition sessions. Explicit --session is rejected; ambient CAPTUREFAB_SESSION is ignored. Configure and arm cameras separately. Acknowledged mode requires 1..32 explicit --expect peers; sent-only rejects --expect. Optional --action-time-ticks sends an absolute device timestamp without reading or qualifying camera clocks. Zero or a target already in the past may execute immediately. One datagram is attempted, with no retry, discovery, controller acquisition or cleanup mutation. An ACK or successful send does not prove execution, frame arrival or synchronization; a post-send failure may leave a future action queued."
    )]
    Action(ActionArgs),
    /// Temporarily assign one native `GigE` camera's IPv4 configuration, without opening it
    #[command(
        after_help = "Runs locally without opening a session or camera. Explicit --session is rejected; ambient CAPTUREFAB_SESSION is ignored. The target must have no open camera object. The mandatory MAC lock is advisory: opaque GenTL aliases, other applications/runtimes and owners opened without locks are outside its coverage. One assignment datagram is sent, with no retry or rollback; power cycling restores the camera's startup configuration."
    )]
    ForceIp(ForceIpArgs),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ActionMode {
    Acknowledged,
    SentOnly,
}

#[derive(Debug, Args)]
pub(crate) struct ActionArgs {
    /// Require strict replies from each expected peer, or report only the local send
    #[arg(long, value_enum)]
    pub mode: ActionMode,
    /// Current local IPv4 address selecting exactly one outgoing interface
    #[arg(long, value_name = "IP")]
    pub interface: Ipv4Addr,
    /// Explicit action device key, unsigned 32-bit decimal
    #[arg(long, value_name = "U32")]
    pub device_key: u32,
    /// Explicit action group key, unsigned 32-bit decimal
    #[arg(long, value_name = "U32")]
    pub group_key: u32,
    /// Nonzero unsigned 32-bit decimal action group mask
    #[arg(long, value_name = "NONZERO")]
    pub group_mask: u32,
    /// Optional absolute device timestamp in camera ticks; zero/past may execute immediately
    #[arg(long, value_name = "U64_TICKS")]
    pub action_time_ticks: Option<u64>,
    /// Expected camera IPv4 address, repeatable in acknowledged mode
    #[arg(long = "expect", value_name = "IP")]
    pub expected: Vec<Ipv4Addr>,
    /// One total operation deadline, greater than zero and at most 30s
    #[arg(long, default_value = "1s", value_name = "DUR", value_parser = super::args::duration)]
    pub timeout: Duration,
}

impl ActionArgs {
    fn request(&self) -> crate::network::action::Request {
        crate::network::action::Request {
            mode: match self.mode {
                ActionMode::Acknowledged => crate::network::action::Mode::Acknowledged,
                ActionMode::SentOnly => crate::network::action::Mode::SentOnly,
            },
            interface: self.interface,
            device_key: self.device_key,
            group_key: self.group_key,
            group_mask: self.group_mask,
            action_time_ticks: self.action_time_ticks,
            expected: self.expected.clone(),
            timeout: self.timeout,
        }
    }
}

pub(super) const ACTION_FIELDS: &[(&str, &str)] = &[
    ("mode", "acknowledged|sent-only"),
    (
        "action",
        "immediate|scheduled — explicit packet timing request",
    ),
    (
        "action_time_ticks?",
        "u64 — requested absolute device timestamp for scheduled packets, including zero; clock and future-time validity are not verified",
    ),
    (
        "outcome",
        "acknowledged|sent_without_ack_confirmation — failures retain an unconfirmed partial summary in error.details.action",
    ),
    (
        "interface",
        "{name,index,address,netmask} — selected local interface",
    ),
    ("request_id", "int — nonzero GVCP request ID"),
    (
        "expected",
        "[IPv4] — explicit acknowledged peers; empty for sent-only",
    ),
    (
        "responses",
        "[{source,status_raw,status}] — exact correlated eight-byte ACKs; full u16 status",
    ),
    (
        "missing",
        "[IPv4] — expected peers without an admitted reply",
    ),
    (
        "verification",
        "strict_action_acknowledgements|not_requested",
    ),
    ("mutation_send_attempted", "bool"),
    (
        "mutation_datagrams",
        "int — exactly one on success; no retry",
    ),
    (
        "bytes",
        "int — 20 immediate or 28 scheduled on successful send",
    ),
    (
        "action_may_have_been_issued",
        "bool — conservative once send is attempted, including errors",
    ),
    (
        "received_datagrams",
        "int — all received traffic, at most 1024",
    ),
    ("ignored_datagrams", "int — unrelated or filtered traffic"),
    (
        "duplicate_datagrams",
        "int — identical admitted peer replies",
    ),
    ("frame_arrival", "not_verified"),
    ("execution", "not_verified"),
    ("synchronization", "not_verified"),
];

#[derive(Debug, Args)]
pub(crate) struct ForceIpArgs {
    /// Target unicast MAC, twelve hex digits or colon-separated hex pairs
    #[arg(long, value_name = "MAC", value_parser = crate::network::force_ip::parse_mac)]
    pub mac: [u8; 6],
    /// Current local IPv4 address selecting exactly one outgoing interface
    #[arg(long, value_name = "IP")]
    pub interface: Ipv4Addr,
    /// Temporary camera IPv4 address
    #[arg(long, value_name = "IP")]
    pub address: Ipv4Addr,
    /// Temporary contiguous IPv4 subnet mask
    #[arg(long, value_name = "MASK")]
    pub netmask: Ipv4Addr,
    /// Temporary gateway; pass 0.0.0.0 for none
    #[arg(long, value_name = "IP")]
    pub gateway: Ipv4Addr,
    /// Total discovery, assignment and verification deadline
    #[arg(long, default_value = "3s", value_name = "DUR", value_parser = super::args::duration)]
    pub timeout: Duration,
}

pub(super) const FORCE_IP_FIELDS: &[(&str, &str)] = &[
    ("camera", "string — canonical gige:MAC identity"),
    ("mac", "string — twelve lowercase hex digits"),
    (
        "interface",
        "{name,index,address,netmask} — selected local interface",
    ),
    (
        "requested",
        "{address,netmask,gateway} — explicit temporary configuration",
    ),
    (
        "before",
        "{configuration:{address,netmask,gateway},reply_source} — preflight discovery",
    ),
    (
        "observed",
        "{configuration:{address,netmask,gateway},reply_source} — fresh matching discovery",
    ),
    ("verification", "fresh_mac_and_configuration_discovery"),
    (
        "ack",
        "received|not_observed — ACK alone does not prove assignment",
    ),
    ("ack_source?", "string — correlated ACK sender"),
    ("mutation_send_attempted", "bool"),
    ("mutation_datagrams", "int — exactly one on success"),
    (
        "assignment_may_have_applied",
        "bool — also retained in error.details.force_ip after send",
    ),
    (
        "received_datagrams",
        "int — includes the packet that trips the 1024-packet bound",
    ),
    (
        "ignored_datagrams",
        "int — filtered/unrelated bootstrap traffic",
    ),
    ("reachability", "not_probed — no controller acquisition"),
    ("temporary", "true"),
    (
        "reservation",
        "{scope:advisory_selected_runtime_native_mac,no_other_open_camera_required:true}",
    ),
    (
        "limitations",
        "[string] — advisory ownership and unauthenticated discovery limits",
    ),
];

pub(super) const FIELDS: &[(&str, &str)] = &[
    (
        "interfaces",
        "[{name,address,netmask,mtu?,speed_mbps?,kind:wired|wireless|loopback|other|unknown,up?}] — IPv4 adapters",
    ),
    (
        "cameras",
        "[{camera:{id,kind,vendor,model,serial?,address?},active,interface?,host_address?,reachable,health:unreachable|limited|unverified|ready|unstable,summary,details,tuning?}] — GigE cameras and how each is reached; active: a session holds it; host_address: this computer's address toward it",
    ),
    ("warnings", "[string]"),
    ("time", "string — RFC 3339 UTC time of the scan"),
];

pub(super) fn run(global: &Global, args: &NetworkArgs) -> Result<Report> {
    if let Some(NetworkCommand::Action(args)) = &args.action {
        if global.session.is_some() && !global.session_from_env {
            return Err(crate::types::Usage(
                "network action runs locally and cannot target an explicit --session or RPC".into(),
            )
            .into());
        }
        let request = args.request();
        request.validate()?;
        super::signals::take_over();
        let value = crate::network::action::run(&request, super::signals::interrupted)?;
        let timing = if args.action_time_ticks.is_some() {
            "scheduled"
        } else {
            "immediate"
        };
        let text = if matches!(args.mode, ActionMode::SentOnly) {
            format!(
                "Sent one {timing} action via {}; no acknowledgement was requested. Execution, frame arrival and synchronization are not verified.\n",
                args.interface
            )
        } else {
            format!(
                "Received strict {timing} action acknowledgements from {} expected peers via {}. Execution, frame arrival and synchronization are not verified.\n",
                args.expected.len(),
                args.interface
            )
        };
        return Ok(Report::json(value).text(text));
    }
    if let Some(NetworkCommand::ForceIp(args)) = &args.action {
        if global.session.is_some() && !global.session_from_env {
            return Err(crate::types::Usage(
                "network force-ip runs locally and cannot target an explicit --session or RPC"
                    .into(),
            )
            .into());
        }
        super::signals::take_over();
        let value = crate::network::force_ip::run(
            crate::network::force_ip::Request {
                mac: args.mac,
                interface: args.interface,
                configuration: crate::network::force_ip::Configuration {
                    address: args.address,
                    netmask: args.netmask,
                    gateway: args.gateway,
                },
                timeout: args.timeout,
            },
            super::signals::interrupted,
        )?;
        let text = format!(
            "Observed temporary IPv4 assignment for {}: {} via {}. No control connection was opened.\n",
            value["camera"].as_str().unwrap_or_default(),
            args.address,
            args.interface
        );
        return Ok(Report::json(value).text(text));
    }
    let command = SessionCommand::NetworkScan {
        timeout_ms: u64::try_from(args.timeout.as_millis()).unwrap_or(u64::MAX),
        tune: args.tune,
    };
    command.validate()?;
    let value = if let (Some(session), false) = (&global.session, global.no_session) {
        crate::ipc::call(session, command, Duration::from_secs(120))?
    } else {
        let local = SessionHandle::new();
        let result = (|| -> Result<Value> {
            for camera in &args.cameras {
                let camera = super::caller_selector(camera)?;
                local.request(SessionCommand::connect(camera, 2000))?;
            }
            local.request(command)
        })();
        local.shutdown();
        result?
    };
    let mut scan = value["network_scan"].clone();
    let text = serde_json::from_value::<crate::network::NetworkScan>(scan.clone())
        .ok()
        .map(|scan| {
            let mut out = Vec::new();
            let _ = crate::network::write_human(&scan, &mut out);
            String::from_utf8_lossy(&out).into_owned()
        });
    if let Some(object) = scan.as_object_mut()
        && let Some(ms) = object.remove("scanned_at_ms").and_then(|ms| ms.as_i64())
        && let Some(time) = chrono::DateTime::from_timestamp_millis(ms)
    {
        object.insert(
            "time".into(),
            json!(time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        );
    }
    // Cameras in the shape `list` prints them.
    for camera in scan["cameras"].as_array_mut().into_iter().flatten() {
        if let Ok(info) =
            serde_json::from_value::<crate::types::CameraInfo>(camera["camera"].clone())
        {
            camera["camera"] = serde_json::to_value(super::camera::row(&info))?;
        }
    }
    let mut report = Report::json(scan);
    if let Some(text) = text {
        report = report.text(text);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser as _;

    fn words(mode: &str) -> Vec<&str> {
        vec![
            "capturefab",
            "network",
            "action",
            "--mode",
            mode,
            "--interface",
            "192.168.50.1",
            "--device-key",
            "305419896",
            "--group-key",
            "2309737967",
            "--group-mask",
            "65538",
        ]
    }

    fn action(cli: &Cli) -> &ActionArgs {
        let Some(Command::Network(args)) = &cli.command else {
            panic!("not network");
        };
        let Some(NetworkCommand::Action(args)) = &args.action else {
            panic!("not action");
        };
        args
    }

    #[test]
    fn action_modes_exact_keys_and_repeatable_peers_parse_without_opening() {
        let mut args = words("acknowledged");
        args.extend([
            "--expect",
            "192.168.50.10",
            "--expect",
            "192.168.50.11",
            "--timeout",
            "30s",
        ]);
        let cli = Cli::try_parse_from(args).unwrap();
        let request = action(&cli).request();
        request.validate().unwrap();
        assert_eq!(request.device_key, 0x1234_5678);
        assert_eq!(request.group_key, 0x89ab_cdef);
        assert_eq!(request.group_mask, 0x0001_0002);
        assert_eq!(request.action_time_ticks, None);
        assert_eq!(request.expected.len(), 2);
        assert_eq!(request.timeout, Duration::from_secs(30));
        let mut args = words("sent-only");
        args.push("--no-session");
        let cli = Cli::try_parse_from(args).unwrap();
        assert_eq!(
            action(&cli).request().mode,
            crate::network::action::Mode::SentOnly
        );
        action(&cli).request().validate().unwrap();
        Cli::try_parse_from(["capturefab", "network", "action"]).unwrap_err();
        for mode in ["scheduled", "auto"] {
            Cli::try_parse_from(words(mode)).unwrap_err();
        }
        for value in ["4294967296", "-1", "1.5"] {
            let mut args = words("sent-only");
            args[8] = value;
            Cli::try_parse_from(args).unwrap_err();
        }
        for mode in ["acknowledged", "sent-only"] {
            let mut args = words(mode);
            if mode == "sent-only" {
                args.extend(["--expect", "192.168.50.10"]);
            }
            let cli = Cli::try_parse_from(args).unwrap();
            action(&cli).request().validate().unwrap_err();
        }
    }

    #[test]
    fn action_time_ticks_preserves_exact_unsigned_wire_intent() {
        for (text, expected) in [
            ("0", 0),
            ("9007199254740993", 9_007_199_254_740_993),
            ("9223372036854775808", 1_u64 << 63),
            ("18446744073709551615", u64::MAX),
        ] {
            let mut args = words("sent-only");
            args.extend(["--action-time-ticks", text]);
            let cli = Cli::try_parse_from(args).unwrap();
            let request = action(&cli).request();
            request.validate().unwrap();
            assert_eq!(request.action_time_ticks, Some(expected));
        }
        for text in ["-1", "18446744073709551616", "1.5", "1e3", "0x10"] {
            let mut args = words("sent-only");
            args.extend(["--action-time-ticks", text]);
            Cli::try_parse_from(args).unwrap_err();
        }
        let mut args = words("sent-only");
        args.push("--action-time-ticks");
        Cli::try_parse_from(args).unwrap_err();
    }

    #[test]
    fn action_is_cold_local_and_rejects_explicit_session_before_io() {
        let mut args = words("sent-only");
        args.extend(["--session", "held"]);
        let mut cli = Cli::try_parse_from(args).unwrap();
        cli.global.session_from_env = false;
        let Err(error) = crate::cli::run(cli) else {
            panic!("explicit session accepted");
        };
        assert_eq!(
            crate::types::classify(&error).kind,
            crate::types::ErrorKind::Usage
        );
        assert!(error.to_string().contains("explicit --session"));
        for no_session in [false, true] {
            let mut args = words("sent-only");
            args[12] = "0";
            if no_session {
                args.push("--no-session");
            }
            let mut cli = Cli::try_parse_from(args).unwrap();
            cli.global.session = Some("invalid/ambient/session".into());
            cli.global.session_from_env = true;
            let Err(error) = crate::cli::run(cli) else {
                panic!("zero group mask accepted");
            };
            assert!(
                error.to_string().contains("nonzero group mask"),
                "{error:#}"
            );
        }
    }
}
