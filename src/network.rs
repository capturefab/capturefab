//! Read-only GigE network diagnosis. Discovery, control reachability and stream
//! stability are separate facts: a discovery reply alone establishes none of the
//! others, and even a good wired route cannot prove a loss-free image stream.
use crate::{
    transport::gige,
    types::{CameraInfo, Categorized, ErrorKind, Transport},
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InterfaceKind {
    Wired,
    Wireless,
    Loopback,
    Other,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkInterface {
    pub name: String,
    pub address: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub mtu: Option<u32>,
    pub speed_mbps: Option<u64>,
    pub kind: InterfaceKind,
    pub up: Option<bool>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkHealth {
    Unreachable,
    Limited,
    Unverified,
    Ready,
    Unstable,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkTuning {
    pub packet_size: Option<u32>,
    /// Camera throughput limits are expressed in bytes per second.
    pub throughput_limit_bps: Option<u64>,
    pub changes: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkCamera {
    pub camera: CameraInfo,
    pub active: bool,
    pub interface: Option<String>,
    pub host_address: Option<Ipv4Addr>,
    pub reachable: bool,
    pub health: NetworkHealth,
    pub summary: String,
    pub details: Vec<String>,
    pub tuning: Option<NetworkTuning>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkScan {
    pub interfaces: Vec<NetworkInterface>,
    pub cameras: Vec<NetworkCamera>,
    pub warnings: Vec<String>,
    pub scanned_at_ms: u64,
}

#[derive(Debug, Clone, Default)]
struct LinkMetadata {
    mtu: Option<u32>,
    speed_mbps: Option<u64>,
    kind: Option<InterfaceKind>,
    up: Option<bool>,
}

/// IPv4 addresses and link metadata; never changes host configuration. Unknown
/// metadata is preserved rather than inferring Ethernet from an interface name.
pub fn interfaces() -> Result<Vec<NetworkInterface>> {
    interfaces_with(link_metadata())
}

/// [`interfaces`], waiting at most `limit` for link metadata: on macOS and
/// Windows that runs other programs, and an error already late by a whole
/// probe timeout should not wait seconds more. Without it, kinds are
/// unknown and addresses and masks still come from the OS.
pub(crate) fn interfaces_within(limit: Duration) -> Result<Vec<NetworkInterface>> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    // The thread ends on its own once the bounded commands return.
    std::thread::spawn(move || {
        let _ = sender.send(link_metadata());
    });
    interfaces_with(receiver.recv_timeout(limit).unwrap_or_default())
}

fn interfaces_with(metadata: HashMap<String, LinkMetadata>) -> Result<Vec<NetworkInterface>> {
    let addresses = if_addrs::get_if_addrs().context("enumerating network interfaces")?;
    let mut interfaces = Vec::new();
    for interface in addresses {
        let if_addrs::IfAddr::V4(address) = interface.addr else {
            continue;
        };
        let name = interface.name;
        #[cfg(target_os = "linux")]
        let name = canonical_link_name(&name, &metadata).to_owned();
        let link = metadata.get(&name).cloned().unwrap_or_default();
        interfaces.push(NetworkInterface {
            name,
            address: address.ip,
            netmask: address.netmask,
            mtu: link.mtu,
            speed_mbps: link.speed_mbps,
            kind: if address.ip.is_loopback() {
                InterfaceKind::Loopback
            } else {
                link.kind.unwrap_or(InterfaceKind::Unknown)
            },
            up: link.up,
        });
    }
    interfaces.sort_by(|a, b| (&a.name, a.address).cmp(&(&b.name, b.address)));
    interfaces.dedup_by(|a, b| a.name == b.name && a.address == b.address);
    Ok(interfaces)
}

#[cfg(any(target_os = "linux", test))]
fn canonical_link_name<'a>(name: &'a str, metadata: &HashMap<String, LinkMetadata>) -> &'a str {
    // Linux IPv4 address labels can be eth0:1 even though sysfs and the receive
    // bottleneck are eth0. Preserve real device names and unknown labels.
    if !metadata.contains_key(name)
        && let Some((base, alias)) = name.rsplit_once(':')
        && !alias.is_empty()
        && alias.bytes().all(|byte| byte.is_ascii_digit())
        && metadata.contains_key(base)
    {
        base
    } else {
        name
    }
}

#[cfg(target_os = "linux")]
fn link_metadata() -> HashMap<String, LinkMetadata> {
    let mut links = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
        return links;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let read = |file: &str| {
            std::fs::read_to_string(path.join(file))
                .ok()
                .map(|s| s.trim().to_owned())
        };
        let kind = if name == "lo" {
            InterfaceKind::Loopback
        } else if path.join("wireless").exists() || path.join("phy80211").exists() {
            InterfaceKind::Wireless
        } else if path.join("device").exists() && read("type").as_deref() == Some("1") {
            InterfaceKind::Wired
        } else {
            InterfaceKind::Other
        };
        let speed_mbps = read("speed")
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|s| *s > 0);
        let up = match read("operstate").as_deref() {
            Some("up") => Some(true),
            Some("down" | "dormant" | "lowerlayerdown" | "notpresent") => Some(false),
            _ => None,
        };
        links.insert(
            name,
            LinkMetadata {
                mtu: read("mtu").and_then(|s| s.parse().ok()),
                speed_mbps,
                kind: Some(kind),
                up,
            },
        );
    }
    links
}

#[cfg(target_os = "macos")]
fn link_metadata() -> HashMap<String, LinkMetadata> {
    let mut links = bounded_output("/sbin/ifconfig", &["-a"])
        .map(|output| parse_ifconfig(&output))
        .unwrap_or_default();
    if let Some(output) = bounded_output("/usr/sbin/networksetup", &["-listallhardwareports"]) {
        for (name, kind) in parse_hardware_ports(&output) {
            links.entry(name).or_default().kind = Some(kind);
        }
    }
    links
}

#[cfg(target_os = "windows")]
fn link_metadata() -> HashMap<String, LinkMetadata> {
    // A constant script, with no device-provided text interpolated into commands.
    const SCRIPT: &str = "@((Get-NetAdapter -IncludeHidden -ErrorAction Stop) | ForEach-Object { $a=$_; $ip=Get-NetIPInterface -InterfaceIndex $a.ifIndex -AddressFamily IPv4 -ErrorAction SilentlyContinue | Select-Object -First 1; [pscustomobject]@{name=$a.Name; mtu=$ip.NlMtu; speed_bps=$a.ReceiveLinkSpeed; interface_type=$a.InterfaceType; physical=$a.HardwareInterface; up=($a.Status -eq 'Up')} }) | ConvertTo-Json -Compress";
    bounded_output(
        "powershell.exe",
        &[
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            SCRIPT,
        ],
    )
    .map(|output| parse_windows_links(&output))
    .unwrap_or_default()
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn link_metadata() -> HashMap<String, LinkMetadata> {
    HashMap::new()
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn bounded_output(program: &str, arguments: &[&str]) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut output = String::new();
        stdout
            .take(1024 * 1024)
            .read_to_string(&mut output)
            .ok()
            .map(|_| output)
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    let output = reader.join().ok().flatten();
    if success { output } else { None }
}

#[cfg(any(target_os = "macos", test))]
fn parse_ifconfig(output: &str) -> HashMap<String, LinkMetadata> {
    let mut links = HashMap::<String, LinkMetadata>::new();
    let mut name = None;
    for line in output.lines() {
        if !line.starts_with(char::is_whitespace) {
            let Some((interface, header)) = line.split_once(": ") else {
                continue;
            };
            name = Some(interface.to_owned());
            let link = links.entry(interface.to_owned()).or_default();
            let tokens: Vec<_> = header.split_whitespace().collect();
            link.mtu = tokens
                .windows(2)
                .find(|pair| pair[0] == "mtu")
                .and_then(|pair| pair[1].parse().ok());
            if header.contains("LOOPBACK") {
                link.kind = Some(InterfaceKind::Loopback);
            } else if header.contains("POINTOPOINT") || interface.starts_with("bridge") {
                link.kind = Some(InterfaceKind::Other);
            }
            link.up = Some(
                header
                    .split(['<', '>'])
                    .nth(1)
                    .is_some_and(|flags| flags.split(',').any(|flag| flag == "UP")),
            );
        } else if let Some(name) = &name {
            let link = links.entry(name.clone()).or_default();
            let line = line.trim();
            if let Some(status) = line.strip_prefix("status: ") {
                link.up = match status {
                    "active" => Some(true),
                    "inactive" => Some(false),
                    _ => link.up,
                };
            } else if let Some(media) = line.strip_prefix("media: ") {
                // A negotiated Ethernet medium proves wired; generic autoselect
                // does not (Wi-Fi commonly uses exactly that string).
                for token in media.split(|c: char| c.is_whitespace() || c == '(' || c == ')') {
                    if let Some((speed, _)) = token.split_once("base") {
                        link.speed_mbps = speed
                            .strip_suffix('G')
                            .and_then(|speed| speed.parse::<u64>().ok())
                            .map(|speed| speed.saturating_mul(1000))
                            .or_else(|| speed.parse::<u64>().ok())
                            .filter(|s| *s > 0);
                        if link.kind.is_none() && link.speed_mbps.is_some() {
                            link.kind = Some(InterfaceKind::Wired);
                        }
                        break;
                    }
                }
            }
        }
    }
    links
}

#[cfg(any(target_os = "macos", test))]
fn parse_hardware_ports(output: &str) -> HashMap<String, InterfaceKind> {
    let mut links = HashMap::new();
    let mut port = "";
    for line in output.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("Hardware Port: ") {
            port = value;
        } else if let Some(device) = line.strip_prefix("Device: ") {
            let kind = if port.contains("Wi-Fi") || port.contains("AirPort") {
                InterfaceKind::Wireless
            } else if port.contains("Ethernet") || port.split_whitespace().any(|word| word == "LAN")
            {
                InterfaceKind::Wired
            } else {
                InterfaceKind::Other
            };
            links.insert(device.to_owned(), kind);
        }
    }
    links
}

#[cfg(any(target_os = "windows", test))]
fn parse_windows_links(output: &str) -> HashMap<String, LinkMetadata> {
    let mut links = HashMap::new();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(output) else {
        return links;
    };
    let values = match &value {
        serde_json::Value::Array(values) => values.as_slice(),
        serde_json::Value::Object(_) => std::slice::from_ref(&value),
        _ => return links,
    };
    for value in values {
        let Some(name) = value["name"].as_str() else {
            continue;
        };
        let kind = match value["interface_type"].as_u64() {
            Some(71) => InterfaceKind::Wireless,
            Some(24) => InterfaceKind::Loopback,
            Some(6) if value["physical"].as_bool() == Some(true) => InterfaceKind::Wired,
            Some(_) => InterfaceKind::Other,
            None => InterfaceKind::Unknown,
        };
        links.insert(
            name.to_owned(),
            LinkMetadata {
                mtu: value["mtu"]
                    .as_u64()
                    .and_then(|mtu| u32::try_from(mtu).ok()),
                speed_mbps: value["speed_bps"]
                    .as_u64()
                    .map(|speed| speed / 1_000_000)
                    .filter(|s| *s > 0),
                kind: Some(kind),
                up: value["up"].as_bool(),
            },
        );
    }
    links
}

fn same_camera(left: &CameraInfo, right: &CameraInfo) -> bool {
    left.transport == right.transport
        && ((!left.id.is_empty() && left.id == right.id)
            || (left.address.is_some() && left.address == right.address))
}

fn camera_list(devices: &[CameraInfo], active: &[CameraInfo]) -> Vec<(CameraInfo, bool)> {
    let mut cameras = Vec::new();
    // The open camera's address is the address in use, even when a new discovery
    // reports changed configuration for the same physical device.
    for camera in active
        .iter()
        .chain(devices)
        .filter(|camera| camera.transport == Transport::GigE)
    {
        if cameras
            .iter()
            .any(|(existing, _)| same_camera(existing, camera))
        {
            continue;
        }
        cameras.push((
            camera.clone(),
            active.iter().any(|item| same_camera(item, camera)),
        ));
    }
    cameras
}

/// Read the OS-selected IPv4 source without transmitting a packet.
fn route_host(address: Ipv4Addr) -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect(SocketAddrV4::new(address, 3956)).ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(address) if !address.is_unspecified() => Some(address),
        _ => None,
    }
}

pub(crate) fn same_subnet(interface: &NetworkInterface, address: Ipv4Addr) -> bool {
    let mask = u32::from(interface.netmask);
    // Missing/invalid masks must not turn every routed destination into a
    // supposedly direct Ethernet path.
    let inverse = !mask;
    mask != 0
        && (inverse & inverse.wrapping_add(1)) == 0
        && (u32::from(interface.address) & mask) == (u32::from(address) & mask)
}

/// The words of [`LOCAL_NETWORK_HINT`], for `concat!`.
macro_rules! local_network_text {
    () => {
        "macOS may be blocking local network access: allow Capturefab in System Settings › Privacy & Security › Local Network (for the CLI, allow the terminal app that runs it)"
    };
}
/// What to try when macOS refuses a route to the local network. A denied
/// Local Network privacy prompt reads as "no route to host", which has
/// genuine causes too, hence "may". Free of the words `cli::error_code`
/// reads as another category, so the error stays `unavailable`.
pub const LOCAL_NETWORK_HINT: &str = local_network_text!();
/// An unreachable reason for errors that carry only static text.
#[cfg(any(feature = "hubs", test))]
pub(crate) const NO_ROUTE_LOCAL_NETWORK: &str =
    concat!("no route to host; ", local_network_text!());

/// [`LOCAL_NETWORK_HINT`] when `error` may come from macOS Local Network
/// privacy: EHOSTUNREACH on the way to a private, link-local, broadcast or
/// multicast address, or to one not known. Never on other systems.
pub fn local_network_hint(error: &std::io::Error, dest: Option<IpAddr>) -> Option<&'static str> {
    local_network_hint_on(cfg!(target_os = "macos"), error, dest)
}
fn local_network_hint_on(
    macos: bool,
    error: &std::io::Error,
    dest: Option<IpAddr>,
) -> Option<&'static str> {
    // EHOSTUNREACH is 65 on macOS.
    let unreachable =
        error.kind() == std::io::ErrorKind::HostUnreachable || error.raw_os_error() == Some(65);
    let local = dest.is_none_or(|dest| match dest {
        IpAddr::V4(v4) => {
            v4.is_private() || v4.is_link_local() || v4.is_broadcast() || v4.is_multicast()
        }
        IpAddr::V6(v6) => v6.is_unique_local() || v6.is_unicast_link_local() || v6.is_multicast(),
    });
    (macos && unreachable && local).then_some(LOCAL_NETWORK_HINT)
}
/// `context`, followed by [`LOCAL_NETWORK_HINT`] when it applies to `error`.
pub(crate) fn with_local_network_hint(
    context: &str,
    error: &std::io::Error,
    dest: Option<IpAddr>,
) -> String {
    match local_network_hint(error, dest) {
        Some(hint) => format!("{context}; {hint}"),
        None => context.to_owned(),
    }
}

/// The adapter list in a GigE connect error: name, address/prefix and kind.
fn adapter_summary(interface: &NetworkInterface) -> String {
    let prefix = u32::from(interface.netmask).count_ones();
    let kind = match interface.kind {
        InterfaceKind::Wired => ", Ethernet",
        InterfaceKind::Wireless => ", Wi-Fi",
        _ => "",
    };
    format!("{} {}/{prefix}{kind}", interface.name, interface.address)
}

/// The advice a GigE connect error ends with when no adapter shares the
/// camera's subnet. The desktop shows its Scan network button instead.
pub(crate) const SCAN_ADVICE: &str = "run `capturefab network-scan`";

/// What to do when the GigE Vision camera typed as `ip` did not answer within
/// `waited`, or `refused` its control port, using this computer's adapters
/// the way `network-scan` does.
pub(crate) fn explain_gige_unreachable(
    ip: Ipv4Addr,
    interfaces: &[NetworkInterface],
    refused: bool,
    waited: Duration,
) -> String {
    let waited = wait_text(waited);
    if ip.is_loopback() {
        // Nothing on this computer is a camera; a simulator may have been meant.
        let what = if refused {
            format!("Nothing at {ip} accepts GigE Vision control traffic.")
        } else {
            format!("No GigE Vision camera simulator answered at {ip} within {waited}.")
        };
        return format!(
            "{what} Start one with `capturefab simulate run --preset gige --source PATH`, or connect sim:0 for the built-in test camera."
        );
    }
    let adapters: Vec<&NetworkInterface> = interfaces
        .iter()
        .filter(|interface| {
            interface.kind != InterfaceKind::Loopback
                && !interface.address.is_loopback()
                && interface.up != Some(false)
        })
        .collect();
    let shared = adapters.iter().find(|interface| same_subnet(interface, ip));
    // VPN tunnels and virtual adapters are left out of the list: a camera
    // is not plugged into them.
    let physical: Vec<&&NetworkInterface> = adapters
        .iter()
        .filter(|interface| interface.kind != InterfaceKind::Other)
        .collect();
    let listed = if physical.is_empty() {
        "it has no IPv4 network adapter up".to_owned()
    } else {
        let mut names: Vec<String> = physical
            .iter()
            .take(3)
            .map(|a| adapter_summary(a))
            .collect();
        if physical.len() > 3 {
            names.push(format!("{} more", physical.len() - 3));
        }
        names.join("; ")
    };
    if refused {
        // A router answers for an address off every local subnet too.
        let subnet = match shared {
            Some(_) => String::new(),
            None => format!(
                " No network adapter on this computer shares its subnet ({listed}), so the refusal may come from a router."
            ),
        };
        return format!(
            "Something at {ip} refused GigE Vision control traffic; it may not be a GigE Vision camera.{subnet} For IP cameras, try onvif://{ip} or an rtsp:// URL."
        );
    }
    if let Some(adapter) = shared {
        return format!(
            "No GigE Vision camera answered at {ip} within {waited} on {}. Check camera power and the Ethernet cable, allow GigE Vision UDP port 3956 through the firewall, and close other camera applications.",
            adapter_summary(adapter)
        );
    }
    // The subnet leads: it is the likely cause, and the desktop's one-line
    // slot shows only the start.
    format!(
        "No network adapter on this computer shares {ip}'s subnet ({listed}), and no GigE Vision camera answered there within {waited}. Give the wired adapter the camera is plugged into a compatible IPv4 address and subnet mask, then retry, or {SCAN_ADVICE}."
    )
}
/// "2 s", "1.5 s", "300 ms".
fn wait_text(duration: Duration) -> String {
    let ms = duration.as_millis();
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms.is_multiple_of(1000) {
        format!("{} s", ms / 1000)
    } else {
        format!("{:.1} s", duration.as_secs_f64())
    }
}

/// A GigE probe of `ip` that went unanswered or was refused, explained and
/// categorized like the raw error it replaces: no answer is `timeout`
/// (exit 4), a refused port `unavailable` (exit 3). Other failures, such as
/// a camera that answered with an error, pass through unchanged. The
/// protocol detail follows the explanation for whoever debugs it.
pub(crate) fn explain_probe_failure(
    ip: Ipv4Addr,
    waited: Duration,
    error: anyhow::Error,
) -> anyhow::Error {
    // Windows reports the ICMP port-unreachable answer to a UDP probe as a
    // connection reset (WSAECONNRESET) rather than a refusal.
    let refused = error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset
            )
        })
    });
    let unanswered = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<gige::GvcpTimeout>())
        .map(|timeout| format!("GVCP command 0x{:04x} went unanswered", timeout.command));
    if !refused && unanswered.is_none() {
        return error;
    }
    let interfaces = if ip.is_loopback() {
        Vec::new()
    } else {
        interfaces_within(Duration::from_millis(500)).unwrap_or_default()
    };
    let message = explain_gige_unreachable(ip, &interfaces, refused, waited);
    let kind = if refused {
        ErrorKind::Unavailable
    } else {
        ErrorKind::Timeout
    };
    let detail = unanswered.unwrap_or_else(|| format!("GVCP: {}", error.root_cause()));
    Categorized::error(kind, format!("{message} ({detail})"))
}

fn diagnose(
    camera: CameraInfo,
    active: bool,
    interfaces: &[NetworkInterface],
    address: Option<Ipv4Addr>,
    host: Option<Ipv4Addr>,
    result: Result<()>,
) -> NetworkCamera {
    let interface = host.and_then(|host| {
        let mut matching = interfaces
            .iter()
            .filter(|interface| interface.address == host);
        let first = matching.next()?;
        // Source IPv4 alone cannot identify an egress adapter when the same IP
        // is configured on multiple adapters. Never choose a fast NIC by luck.
        matching.next().is_none().then_some(first)
    });
    let checks_passed = result.is_ok();
    let rejected = result
        .as_ref()
        .err()
        .is_some_and(gige::is_control_rejection);
    let reachable = checks_passed || rejected;
    let mut details = Vec::new();
    if let Some(interface) = interface {
        details.push(format!(
            "{} · host {} · subnet mask {}",
            interface.name, interface.address, interface.netmask
        ));
        details.push(match interface.speed_mbps {
            Some(speed) => format!("Host adapter's negotiated link speed: {speed} Mb/s. Camera and switch ports may have lower speeds."),
            None => {
                "Link speed could not be verified; using a conservative bandwidth limit.".into()
            }
        });
        details.push(match interface.mtu {
            Some(mtu) => {
                format!("Host MTU: {mtu} bytes. Jumbo packets require end-to-end verification.")
            }
            None => "Host MTU could not be read; standard-size packets are safest.".into(),
        });
    } else {
        details.push("The route could not be matched to a local IPv4 interface.".into());
        if host.is_some_and(|host| {
            interfaces
                .iter()
                .filter(|interface| interface.address == host)
                .count()
                > 1
        }) {
            details.push("Multiple adapters use the route's source IPv4 address. Give each adapter a unique address before trusting its link speed.".into());
        }
    }
    let direct = address
        .zip(interface)
        .is_some_and(|(address, interface)| same_subnet(interface, address));
    let (health, summary) = if let Err(error) = result {
        details.push(format!("Camera control check failed: {error:#}"));
        if rejected {
            details.push("The camera replied to unicast control traffic, but rejected the register read. Close other camera applications if access is denied; an unsupported command may require a camera firmware or compatibility update. Stream stability remains unverified.".into());
            (
                NetworkHealth::Limited,
                "Reachable · camera rejected control check".into(),
            )
        } else {
            details.push("Discovery can succeed even when unicast control and image traffic cannot reach the camera.".into());
            if let Some(address) = address {
                if !interfaces
                    .iter()
                    .any(|interface| same_subnet(interface, address))
                {
                    details.push(format!("No host interface shares the camera's subnet ({address}). Give the camera and a wired adapter compatible IPv4 addresses and subnet masks, then scan again."));
                } else {
                    details.push("Check the Ethernet cable, camera power and firewall access for GigE Vision UDP traffic; another application may own camera control.".into());
                }
            } else {
                details.push("The camera has no valid IPv4 address. Configure an address on the wired adapter's subnet.".into());
            }
            (
                NetworkHealth::Unreachable,
                "Unreachable · camera control did not respond".into(),
            )
        }
    } else if !direct {
        details.push("Camera traffic uses a routed or unknown path. Connect the camera and host to the same wired subnet for predictable GigE throughput.".into());
        (
            NetworkHealth::Limited,
            "Limited · routed or unknown camera path".into(),
        )
    } else {
        let interface = interface.expect("direct requires an interface");
        if interface.up == Some(false) {
            details.push("The operating system reports the adapter as disconnected; control replies alone do not establish a usable image stream.".into());
            (
                NetworkHealth::Limited,
                "Limited · adapter reports disconnected".into(),
            )
        } else if interface.kind == InterfaceKind::Wireless {
            details.push("Wi-Fi has variable latency and shared bandwidth. Use a wired Gigabit Ethernet adapter for reliable camera capture.".into());
            (
                NetworkHealth::Limited,
                "Limited · camera traffic uses Wi-Fi".into(),
            )
        } else if interface.kind != InterfaceKind::Wired {
            details.push("A physical wired Ethernet link could not be verified. Virtual adapters, tunnels and bridges can hide slower links.".into());
            (
                NetworkHealth::Limited,
                "Limited · wired connection unverified".into(),
            )
        } else if interface.speed_mbps.is_some_and(|speed| speed < 1000) {
            details.push("The link negotiated below Gigabit speed. Check the cable, switch port and adapter; reduce frame rate or image size while using this link.".into());
            (
                NetworkHealth::Limited,
                "Limited · Ethernet link below 1 Gb/s".into(),
            )
        } else if interface.speed_mbps.is_none() {
            (
                NetworkHealth::Unverified,
                "Unverified · Ethernet link speed unknown".into(),
            )
        } else {
            (
                NetworkHealth::Unverified,
                "Host Gigabit Ethernet · stream stability unverified".into(),
            )
        }
    };
    if checks_passed {
        details.push("Repeated camera control checks passed. Image-stream stability must be confirmed during capture using packet loss, resend and incomplete-frame counters.".into());
    }
    NetworkCamera {
        camera,
        active,
        interface: interface.map(|i| i.name.clone()),
        host_address: host,
        reachable,
        health,
        summary,
        details,
        tuning: None,
    }
}

/// Diagnose discovered and open GigE cameras with at most four simultaneous
/// read-only probes. The probe budget is shared, so a long list cannot block the
/// scan indefinitely. No IP addresses, routes or operating-system MTUs change.
pub fn scan(
    devices: &[CameraInfo],
    active: &[CameraInfo],
    timeout: Duration,
) -> Result<NetworkScan> {
    let interfaces = interfaces()?;
    Ok(scan_with(
        devices,
        active,
        interfaces,
        timeout,
        gige::probe_network,
    ))
}

fn scan_with<F>(
    devices: &[CameraInfo],
    active: &[CameraInfo],
    interfaces: Vec<NetworkInterface>,
    timeout: Duration,
    probe: F,
) -> NetworkScan
where
    F: Fn(Ipv4Addr, Duration) -> Result<Ipv4Addr> + Sync,
{
    let cameras = camera_list(devices, active);
    let next = AtomicUsize::new(0);
    let deadline =
        Instant::now() + timeout.clamp(Duration::from_millis(1), Duration::from_secs(30));
    let mut results = std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for _ in 0..cameras.len().min(4) {
            workers.push(scope.spawn(|| {
                let mut results = Vec::new();
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some((camera, active)) = cameras.get(index) else { break };
                    let address = camera.address.as_deref().and_then(|value| value.parse::<Ipv4Addr>().ok())
                        .filter(|address| !address.is_unspecified() && !address.is_multicast() && !address.is_broadcast());
                    let host = address.and_then(route_host);
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    let result = match address {
                        None => Err(anyhow::anyhow!("no valid camera IPv4 address")),
                        Some(_) if remaining.is_zero() => {
                            let mut item = diagnose(camera.clone(), *active, &interfaces, address, host,
                                Err(anyhow::anyhow!("scan time budget exhausted")));
                            item.health = NetworkHealth::Unverified;
                            item.summary = "Not checked · scan timed out".into();
                            item.details = vec!["The scan ended before this camera could be checked. Scan again with fewer cameras or a longer timeout.".into()];
                            results.push((index, item));
                            continue;
                        }
                        Some(address) => probe(address, remaining),
                    };
                    let (host, result) = match result {
                        Ok(host) => (Some(host), Ok(())),
                        Err(error) => (host, Err(error)),
                    };
                    results.push((index, diagnose(camera.clone(), *active, &interfaces, address, host, result)));
                }
                results
            }));
        }
        workers
            .into_iter()
            .filter_map(|worker| worker.join().ok())
            .flatten()
            .collect::<Vec<_>>()
    });
    results.sort_by_key(|(index, _)| *index);
    let mut warnings = Vec::new();
    if interfaces
        .iter()
        .all(|interface| interface.kind == InterfaceKind::Loopback)
    {
        warnings.push("No usable IPv4 network interface was found. Connect a wired Ethernet adapter and configure its IPv4 address.".into());
    }
    if results.len() != cameras.len() {
        warnings.push("A network scan worker stopped; some cameras could not be checked.".into());
    }
    NetworkScan {
        interfaces,
        cameras: results.into_iter().map(|(_, camera)| camera).collect(),
        warnings,
        scanned_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64,
    }
}

/// Reserve 20% of the adapter's negotiated line rate for packet headers,
/// control, resends and other traffic; share the remainder across active cameras
/// on that adapter. Routed, wireless and unknown paths get conservative caps.
pub fn budget_bytes_per_second(
    camera: &NetworkCamera,
    interfaces: &[NetworkInterface],
    sharers: usize,
) -> u64 {
    if !camera.reachable {
        return 0;
    }
    let interface = camera.interface.as_deref().and_then(|name| {
        interfaces.iter().find(|interface| {
            interface.name == name
                && camera
                    .host_address
                    .is_none_or(|host| interface.address == host)
        })
    });
    let mut budget = interface
        .and_then(|interface| interface.speed_mbps)
        .map(|speed| speed.saturating_mul(100_000))
        .unwrap_or(10_000_000);
    if let Some(interface) = interface {
        if interface.kind == InterfaceKind::Wireless {
            budget = budget.min(5_000_000);
        } else if interface.kind != InterfaceKind::Wired {
            budget = budget.min(10_000_000);
        }
        let direct = camera
            .camera
            .address
            .as_deref()
            .and_then(|value| value.parse().ok())
            .is_some_and(|address| same_subnet(interface, address));
        if !direct || interface.up == Some(false) {
            budget = budget.min(10_000_000);
        }
    } else {
        budget = budget.min(10_000_000);
    }
    budget / sharers.max(1) as u64
}

/// SFNC DeviceLinkSpeed uses bytes/s; the deprecated GevLinkSpeed uses Mb/s.
/// Explicit GenApi units take precedence for cameras that use vendor units.
/// See https://www.emva.org/wp-content/uploads/GenICam_SFNC_v2_7.pdf.
pub(crate) fn camera_link_bytes_per_second(camera: &mut crate::camera::Camera) -> Option<f64> {
    for (name, default_multiplier) in [("DeviceLinkSpeed", 1.0), ("GevLinkSpeed", 125_000.0)] {
        let Some(value) = camera.get(name).ok().and_then(|value| value.as_f64()) else {
            continue;
        };
        if let Some(bytes) =
            link_bytes_per_second(value, camera.feature_unit(name), default_multiplier)
        {
            return Some(bytes);
        }
    }
    None
}

fn link_bytes_per_second(value: f64, unit: Option<&str>, default_multiplier: f64) -> Option<f64> {
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let bytes = value * bytes_per_second_scale(unit, default_multiplier)?;
    (bytes.is_finite() && bytes > 0.0).then_some(bytes)
}

/// Bytes per second in one unit of a GenApi bandwidth `<Unit>`, or
/// `default_multiplier` when the node declares none.
pub(crate) fn bytes_per_second_scale(unit: Option<&str>, default_multiplier: f64) -> Option<f64> {
    // Unit case matters: B is byte, b is bit. An unfamiliar explicit unit
    // cannot safely be interpreted as the standard's default.
    Some(match unit.map(str::trim).unwrap_or("") {
        "" => default_multiplier,
        "Bps" | "B/s" | "byte/s" | "bytes/s" => 1.0,
        "bps" | "b/s" | "bit/s" | "bits/s" => 1.0 / 8.0,
        "Mbps" | "Mb/s" | "Mbit/s" | "Mbits/s" => 125_000.0,
        "MBps" | "MB/s" | "Mbyte/s" | "Mbytes/s" => 1_000_000.0,
        "kbps" | "Kbps" | "kb/s" | "kbit/s" => 125.0,
        "kBps" | "KBps" | "kB/s" | "KB/s" => 1000.0,
        "Gbps" | "Gb/s" | "Gbit/s" => 125_000_000.0,
        "GBps" | "GB/s" | "Gbyte/s" => 1_000_000_000.0,
        _ => return None,
    })
}

/// Plain name for an adapter type, shared by the CLI and the desktop dialog.
pub fn kind_label(kind: InterfaceKind) -> &'static str {
    match kind {
        InterfaceKind::Wired => "Wired Ethernet",
        InterfaceKind::Wireless => "Wi-Fi",
        InterfaceKind::Loopback => "Loopback",
        InterfaceKind::Other => "Other adapter",
        InterfaceKind::Unknown => "Adapter type unknown",
    }
}

fn health_label(health: NetworkHealth) -> &'static str {
    match health {
        NetworkHealth::Unreachable => "Unreachable",
        NetworkHealth::Limited => "Limited",
        NetworkHealth::Unverified => "Unverified",
        NetworkHealth::Ready => "Ready",
        NetworkHealth::Unstable => "Unstable",
    }
}

/// The readable `network-scan` summary for a terminal. Report warnings are
/// diagnostics, so callers print `scan.warnings` to stderr themselves.
pub fn write_human(scan: &NetworkScan, out: &mut impl std::io::Write) -> std::io::Result<()> {
    writeln!(out, "Adapters")?;
    let adapters: Vec<_> = scan
        .interfaces
        .iter()
        .filter(|interface| interface.kind != InterfaceKind::Loopback)
        .collect();
    if adapters.is_empty() {
        writeln!(out, "  No IPv4 network adapters found.")?;
    }
    let width = adapters
        .iter()
        .map(|interface| interface.name.chars().count())
        .max()
        .unwrap_or(0);
    for interface in &adapters {
        let speed = interface
            .speed_mbps
            .map_or_else(|| "speed unknown".into(), |speed| format!("{speed} Mbps"));
        let mtu = interface
            .mtu
            .map_or_else(|| "MTU unknown".into(), |mtu| format!("MTU {mtu}"));
        writeln!(
            out,
            "  {:<width$}  {} · {} / {} · {mtu} · {speed}{}",
            interface.name,
            kind_label(interface.kind),
            interface.address,
            interface.netmask,
            if interface.up == Some(false) {
                " · down"
            } else {
                ""
            }
        )?;
    }
    writeln!(out, "GigE cameras")?;
    if scan.cameras.is_empty() {
        writeln!(
            out,
            "  No GigE Vision cameras answered. Check the power and Ethernet cable, then scan again."
        )?;
        // Unknown adapters may well be wired; only say so when none can be.
        if !adapters.iter().any(|interface| {
            matches!(
                interface.kind,
                InterfaceKind::Wired | InterfaceKind::Unknown
            )
        }) {
            writeln!(
                out,
                "  No wired Ethernet adapter is up; GigE cameras need a wired adapter on the camera's subnet."
            )?;
        }
    }
    for camera in &scan.cameras {
        let name = [camera.camera.vendor.as_str(), camera.camera.model.as_str()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let address = camera
            .camera
            .address
            .as_deref()
            .unwrap_or(camera.camera.id.as_str());
        let via = camera
            .interface
            .as_deref()
            .map(|interface| format!(" via {interface}"))
            .unwrap_or_default();
        writeln!(
            out,
            "  {name} · {address}{via} · {} ({})",
            health_label(camera.health),
            if camera.active {
                "Connected"
            } else {
                "Discovered"
            }
        )?;
        if !camera.summary.is_empty() {
            writeln!(out, "    {}", camera.summary)?;
        }
        for detail in &camera.details {
            writeln!(out, "    {detail}")?;
        }
        if let Some(tuning) = &camera.tuning {
            if let Some(size) = tuning.packet_size {
                writeln!(out, "    Packet size {size} bytes")?;
            }
            if let Some(limit) = tuning.throughput_limit_bps {
                writeln!(
                    out,
                    "    Throughput limit {:.1} MB/s",
                    limit as f64 / 1_000_000.0
                )?;
            }
            for change in &tuning.changes {
                writeln!(out, "    Changed: {change}")?;
            }
            for warning in &tuning.warnings {
                writeln!(out, "    Warning: {warning}")?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(name: &str, address: [u8; 4], prefix: u32, kind: InterfaceKind) -> NetworkInterface {
        NetworkInterface {
            name: name.into(),
            address: address.into(),
            netmask: Ipv4Addr::from(u32::MAX.checked_shl(32 - prefix).unwrap_or(0)),
            mtu: Some(1500),
            speed_mbps: None,
            kind,
            up: Some(true),
        }
    }

    fn human(scan: &NetworkScan) -> String {
        let mut out = Vec::new();
        write_human(scan, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }
    fn scan(interfaces: Vec<NetworkInterface>, cameras: Vec<NetworkCamera>) -> NetworkScan {
        NetworkScan {
            interfaces,
            cameras,
            warnings: Vec::new(),
            scanned_at_ms: 0,
        }
    }

    #[test]
    fn an_unanswered_gige_address_names_the_subnet_fix() {
        let wait = Duration::from_secs(2);
        let wifi = adapter("en1", [192, 168, 68, 56], 22, InterfaceKind::Wireless);
        let camera = Ipv4Addr::new(192, 168, 77, 5);
        let elsewhere = explain_gige_unreachable(
            camera,
            &[
                adapter("lo0", [127, 0, 0, 1], 8, InterfaceKind::Loopback),
                wifi.clone(),
            ],
            false,
            wait,
        );
        assert_eq!(
            elsewhere,
            "No network adapter on this computer shares 192.168.77.5's subnet (en1 192.168.68.56/22, Wi-Fi), and no GigE Vision camera answered there within 2 s. Give the wired adapter the camera is plugged into a compatible IPv4 address and subnet mask, then retry, or run `capturefab network-scan`."
        );
        // A /22 covers the camera: the adapter is named with the next steps.
        let near = explain_gige_unreachable(
            Ipv4Addr::new(192, 168, 69, 9),
            std::slice::from_ref(&wifi),
            false,
            wait,
        );
        assert!(
            near.starts_with("No GigE Vision camera answered at 192.168.69.9 within 2 s on en1 192.168.68.56/22, Wi-Fi. Check camera power"),
            "{near}"
        );
        assert!(near.contains("UDP port 3956"), "{near}");
        let none = explain_gige_unreachable(camera, &[], false, Duration::from_millis(1500));
        assert!(none.contains("within 1.5 s"), "{none}");
        assert!(
            none.contains("(it has no IPv4 network adapter up)"),
            "{none}"
        );
        let vpn = adapter("utun4", [100, 120, 212, 103], 32, InterfaceKind::Other);
        let tunnel = explain_gige_unreachable(camera, &[wifi.clone(), vpn], false, wait);
        assert!(tunnel.contains("(en1 192.168.68.56/22, Wi-Fi)"), "{tunnel}");
        let mut down = adapter("en5", [192, 168, 77, 1], 24, InterfaceKind::Wired);
        down.up = Some(false);
        let many: Vec<NetworkInterface> = (0..5)
            .map(|n| adapter(&format!("en{n}"), [10, 0, n, 1], 24, InterfaceKind::Wired))
            .chain([down])
            .collect();
        let listed = explain_gige_unreachable(camera, &many, false, wait);
        assert!(
            listed.contains("en2 10.0.2.1/24, Ethernet; 2 more)"),
            "{listed}"
        );
        // Something answered with a refusal: on the camera's subnet it is
        // the device, off every subnet perhaps a router.
        let on = Ipv4Addr::new(192, 168, 69, 9);
        let refused = explain_gige_unreachable(on, std::slice::from_ref(&wifi), true, wait);
        assert_eq!(
            refused,
            "Something at 192.168.69.9 refused GigE Vision control traffic; it may not be a GigE Vision camera. For IP cameras, try onvif://192.168.69.9 or an rtsp:// URL."
        );
        let routed = explain_gige_unreachable(camera, std::slice::from_ref(&wifi), true, wait);
        assert!(
            routed.contains("it may not be a GigE Vision camera. No network adapter on this computer shares its subnet (en1 192.168.68.56/22, Wi-Fi), so the refusal may come from a router. For IP cameras, try onvif://192.168.77.5"),
            "{routed}"
        );
        // Locally, only a simulator can answer.
        for refused in [false, true] {
            let message = explain_gige_unreachable(
                Ipv4Addr::LOCALHOST,
                std::slice::from_ref(&wifi),
                refused,
                wait,
            );
            assert!(
                message.ends_with("Start one with `capturefab simulate run --preset gige --source PATH`, or connect sim:0 for the built-in test camera."),
                "{message}"
            );
            assert!(!message.contains("onvif"), "{message}");
        }
    }

    #[test]
    fn human_scan_explains_an_empty_wireless_only_result() {
        let mut vpn = nic(InterfaceKind::Other, None);
        vpn.name = "utun3".into();
        let output = human(&scan(
            vec![nic(InterfaceKind::Wireless, None), vpn],
            Vec::new(),
        ));
        assert!(output.starts_with("Adapters\n"), "{output}");
        assert!(output.contains("Wi-Fi · 192.168.5.1 / 255.255.255.0 · MTU 1500"));
        assert!(
            output.contains("No GigE Vision cameras answered"),
            "{output}"
        );
        assert!(
            output.contains("No wired Ethernet adapter is up"),
            "{output}"
        );
        for kind in [InterfaceKind::Wired, InterfaceKind::Unknown] {
            let output = human(&scan(vec![nic(kind, Some(1000))], Vec::new()));
            assert!(
                output.contains("No GigE Vision cameras answered"),
                "{output}"
            );
            assert!(!output.contains("No wired Ethernet"), "{kind:?}: {output}");
        }
    }

    #[test]
    fn gige_probe_failures_keep_their_exit_codes() {
        let wait = Duration::from_secs(2);
        let refused =
            anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
                .context("receiving GVCP acknowledgement");
        let explained = explain_probe_failure(Ipv4Addr::LOCALHOST, wait, refused);
        assert_eq!(crate::cli::error_code(&explained), ("unavailable", 3));
        assert!(
            explained.to_string().contains("capturefab simulate"),
            "{explained}"
        );
        let other = explain_probe_failure(
            Ipv4Addr::LOCALHOST,
            wait,
            anyhow::anyhow!("invalid discovery acknowledgement"),
        );
        assert_eq!(other.to_string(), "invalid discovery acknowledgement");
    }

    #[test]
    fn local_network_privacy_is_suggested_only_for_local_addresses_on_macos() {
        let unreachable = [
            std::io::Error::from(std::io::ErrorKind::HostUnreachable),
            std::io::Error::from_raw_os_error(65),
        ];
        let local: [Option<IpAddr>; 5] = [
            Some(Ipv4Addr::new(192, 168, 1, 10).into()),
            Some(Ipv4Addr::new(169, 254, 3, 4).into()),
            Some(Ipv4Addr::BROADCAST.into()),
            Some("fe80::1".parse().unwrap()),
            None,
        ];
        for error in &unreachable {
            for dest in local {
                assert_eq!(
                    local_network_hint_on(true, error, dest),
                    Some(LOCAL_NETWORK_HINT),
                    "{dest:?}"
                );
                assert_eq!(local_network_hint_on(false, error, dest), None);
            }
            assert_eq!(
                local_network_hint_on(true, error, Some(Ipv4Addr::new(8, 8, 8, 8).into())),
                None
            );
        }
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert_eq!(local_network_hint_on(true, &refused, None), None);
        assert_eq!(
            local_network_hint(&unreachable[0], None).is_some(),
            cfg!(target_os = "macos")
        );
        // The hint keeps an unreachable error unavailable.
        assert_eq!(
            crate::cli::error_code(&anyhow::anyhow!("x: {NO_ROUTE_LOCAL_NETWORK}")),
            ("unavailable", 3)
        );
        #[cfg(target_os = "macos")]
        {
            let error = anyhow::Error::new(std::io::Error::from_raw_os_error(65))
                .context(format!("sending GVCP command; {LOCAL_NETWORK_HINT}"));
            assert_eq!(crate::cli::error_code(&error), ("unavailable", 3));
        }
    }

    #[test]
    fn human_scan_indents_camera_details_and_tuning() {
        let interface = nic(InterfaceKind::Wired, Some(100));
        let mut camera = checked(&interface, "192.168.5.2");
        camera.health = NetworkHealth::Limited;
        camera.interface = Some("camera0".into());
        camera.tuning = Some(NetworkTuning {
            packet_size: Some(1500),
            throughput_limit_bps: Some(12_000_000),
            changes: vec!["GevSCPSPacketSize 1500".into()],
            warnings: vec!["Jumbo frames are off".into()],
        });
        let output = human(&scan(vec![interface], vec![camera]));
        assert!(
            output.contains("  Test Camera · 192.168.5.2 via camera0 · Limited (Connected)"),
            "{output}"
        );
        assert!(
            output.contains("\n    Warning: Jumbo frames are off"),
            "{output}"
        );
        assert!(output.contains("\n    Packet size 1500 bytes"), "{output}");
        assert!(!output.contains('{'), "{output}");
    }

    fn nic(kind: InterfaceKind, speed: Option<u64>) -> NetworkInterface {
        NetworkInterface {
            name: "camera0".into(),
            address: Ipv4Addr::new(192, 168, 5, 1),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            mtu: Some(1500),
            speed_mbps: speed,
            kind,
            up: Some(true),
        }
    }
    fn camera(id: &str, address: &str) -> CameraInfo {
        CameraInfo {
            id: id.into(),
            transport: Transport::GigE,
            vendor: "Test".into(),
            model: "Camera".into(),
            serial: id.into(),
            address: Some(address.into()),
            profile: Default::default(),
        }
    }
    fn checked(interface: &NetworkInterface, address: &str) -> NetworkCamera {
        diagnose(
            camera("one", address),
            true,
            std::slice::from_ref(interface),
            address.parse().ok(),
            Some(interface.address),
            Ok(()),
        )
    }

    #[test]
    fn control_checks_do_not_claim_stream_stability() {
        let camera = checked(&nic(InterfaceKind::Wired, Some(1000)), "192.168.5.2");
        assert!(camera.reachable);
        assert_eq!(camera.health, NetworkHealth::Unverified);
        assert!(camera.summary.contains("stream stability unverified"));
        assert!(camera.summary.starts_with("Host Gigabit Ethernet"));
        assert!(
            camera
                .details
                .iter()
                .any(|detail| detail.contains("Camera and switch ports may have lower speeds"))
        );
        assert!(
            camera
                .details
                .iter()
                .any(|detail| detail.contains("incomplete-frame"))
        );
    }

    #[test]
    fn address_aliases_use_the_same_physical_link_metadata() {
        let mut links = HashMap::new();
        links.insert("eth0".into(), LinkMetadata::default());
        assert_eq!(canonical_link_name("eth0:1", &links), "eth0");
        assert_eq!(canonical_link_name("eth0:2", &links), "eth0");
        assert_eq!(canonical_link_name("eth1:1", &links), "eth1:1");
        assert_eq!(canonical_link_name("eth0:label", &links), "eth0:label");
        assert_eq!(canonical_link_name("eth0:", &links), "eth0:");
        links.insert("eth0:1".into(), LinkMetadata::default());
        assert_eq!(canonical_link_name("eth0:1", &links), "eth0:1");
    }

    #[test]
    fn duplicate_host_addresses_do_not_choose_a_fast_adapter_arbitrarily() {
        let wired = nic(InterfaceKind::Wired, Some(1000));
        let mut wifi = nic(InterfaceKind::Wireless, Some(2400));
        wifi.name = "wifi0".into();
        let interfaces = [wired.clone(), wifi];
        let checked = diagnose(
            camera("one", "192.168.5.2"),
            true,
            &interfaces,
            Some(Ipv4Addr::new(192, 168, 5, 2)),
            Some(wired.address),
            Ok(()),
        );
        assert!(checked.reachable);
        assert_eq!(checked.health, NetworkHealth::Limited);
        assert_eq!(checked.interface, None);
        assert_eq!(
            budget_bytes_per_second(&checked, &interfaces, 1),
            10_000_000
        );
        assert!(
            checked
                .details
                .iter()
                .any(|detail| detail.contains("Multiple adapters"))
        );
    }

    #[test]
    fn camera_link_units_distinguish_bits_from_bytes_and_standard_defaults() {
        assert_eq!(
            link_bytes_per_second(125_000_000.0, None, 1.0),
            Some(125_000_000.0)
        );
        assert_eq!(
            link_bytes_per_second(1000.0, None, 125_000.0),
            Some(125_000_000.0)
        );
        for unit in ["Bps", "B/s", "bytes/s"] {
            assert_eq!(
                link_bytes_per_second(125_000_000.0, Some(unit), 1.0),
                Some(125_000_000.0)
            );
        }
        for unit in ["bps", "bit/s", "bits/s"] {
            assert_eq!(
                link_bytes_per_second(1_000_000_000.0, Some(unit), 1.0),
                Some(125_000_000.0)
            );
        }
        for unit in ["Mbps", "Mb/s", "Mbit/s"] {
            assert_eq!(
                link_bytes_per_second(1000.0, Some(unit), 1.0),
                Some(125_000_000.0)
            );
        }
        assert_eq!(
            link_bytes_per_second(125.0, Some("MB/s"), 1.0),
            Some(125_000_000.0)
        );
        assert_eq!(
            link_bytes_per_second(1.0, Some("Gb/s"), 1.0),
            Some(125_000_000.0)
        );
        for value in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(link_bytes_per_second(value, None, 1.0), None);
        }
        assert_eq!(link_bytes_per_second(f64::MAX, Some("Mbps"), 1.0), None);
        assert_eq!(link_bytes_per_second(1000.0, Some("Hz"), 1.0), None);
    }

    #[test]
    fn wifi_slow_virtual_routed_and_down_paths_cannot_be_ready() {
        for (kind, speed, address) in [
            (InterfaceKind::Wireless, Some(2400), "192.168.5.2"),
            (InterfaceKind::Wired, Some(100), "192.168.5.2"),
            (InterfaceKind::Other, Some(10000), "192.168.5.2"),
            (InterfaceKind::Unknown, Some(10000), "192.168.5.2"),
            (InterfaceKind::Wired, Some(1000), "10.0.0.2"),
        ] {
            assert_eq!(
                checked(&nic(kind, speed), address).health,
                NetworkHealth::Limited
            );
        }
        let mut down = nic(InterfaceKind::Wired, Some(1000));
        down.up = Some(false);
        assert_eq!(checked(&down, "192.168.5.2").health, NetworkHealth::Limited);
        assert_eq!(
            checked(&nic(InterfaceKind::Wired, None), "192.168.5.2").health,
            NetworkHealth::Unverified
        );
    }

    #[test]
    fn discovered_unreachable_camera_explains_wrong_subnet() {
        let interface = nic(InterfaceKind::Wired, Some(1000));
        let camera = diagnose(
            camera("one", "10.0.0.2"),
            false,
            std::slice::from_ref(&interface),
            Some(Ipv4Addr::new(10, 0, 0, 2)),
            Some(interface.address),
            Err(anyhow::anyhow!("timed out")),
        );
        assert!(!camera.reachable);
        assert_eq!(camera.health, NetworkHealth::Unreachable);
        assert!(
            camera
                .details
                .iter()
                .any(|detail| detail.contains("No host interface shares"))
        );
        assert!(
            camera
                .details
                .iter()
                .any(|detail| detail.contains("Discovery can succeed"))
        );
        assert_eq!(budget_bytes_per_second(&camera, &[interface], 1), 0);
    }

    #[test]
    fn rejected_control_reads_are_reachable_without_claiming_passed_checks() {
        let interface = nic(InterfaceKind::Wired, Some(1000));
        for status in [0x8001, 0x8006, 0x8007] {
            let error = anyhow::Error::new(gige::GvcpStatus(status))
                .context("camera control path failed check 1 of 3");
            let checked = diagnose(
                camera("one", "192.168.5.2"),
                true,
                std::slice::from_ref(&interface),
                Some(Ipv4Addr::new(192, 168, 5, 2)),
                Some(interface.address),
                Err(error),
            );
            assert!(checked.reachable);
            assert_eq!(checked.health, NetworkHealth::Limited);
            assert_eq!(checked.summary, "Reachable · camera rejected control check");
            assert!(
                checked
                    .details
                    .iter()
                    .any(|detail| detail.contains("Close other camera applications")
                        && detail.contains("unsupported command"))
            );
            assert!(
                !checked
                    .details
                    .iter()
                    .any(|detail| detail.contains("Repeated camera control checks passed"))
            );
            assert!(budget_bytes_per_second(&checked, std::slice::from_ref(&interface), 1) > 0);
        }
    }

    #[test]
    fn subnet_masks_reject_unknown_and_noncontiguous_masks() {
        let mut interface = nic(InterfaceKind::Wired, Some(1000));
        assert!(same_subnet(&interface, Ipv4Addr::new(192, 168, 5, 250)));
        assert!(!same_subnet(&interface, Ipv4Addr::new(192, 168, 6, 1)));
        interface.netmask = Ipv4Addr::UNSPECIFIED;
        assert!(!same_subnet(&interface, Ipv4Addr::new(192, 168, 5, 2)));
        interface.netmask = Ipv4Addr::new(255, 0, 255, 0);
        assert!(!same_subnet(&interface, Ipv4Addr::new(192, 168, 5, 2)));
        interface.netmask = Ipv4Addr::new(255, 255, 255, 255);
        assert!(!same_subnet(&interface, Ipv4Addr::new(192, 168, 5, 2)));
    }

    #[test]
    fn bandwidth_reserves_headroom_and_shares_the_link() {
        let interface = nic(InterfaceKind::Wired, Some(1000));
        let camera = checked(&interface, "192.168.5.2");
        assert_eq!(
            budget_bytes_per_second(&camera, std::slice::from_ref(&interface), 1),
            100_000_000
        );
        assert_eq!(
            budget_bytes_per_second(&camera, std::slice::from_ref(&interface), 4),
            25_000_000
        );
        assert_eq!(
            budget_bytes_per_second(&camera, &[interface], 0),
            100_000_000
        );
        for (kind, speed, cap) in [
            (InterfaceKind::Wireless, Some(2400), 5_000_000),
            (InterfaceKind::Unknown, Some(10000), 10_000_000),
            (InterfaceKind::Wired, None, 10_000_000),
        ] {
            let interface = nic(kind, speed);
            assert_eq!(
                budget_bytes_per_second(&checked(&interface, "192.168.5.2"), &[interface], 2),
                cap / 2
            );
        }
        let interface = nic(InterfaceKind::Wired, Some(10000));
        assert_eq!(
            budget_bytes_per_second(&checked(&interface, "10.0.0.2"), &[interface], 1),
            10_000_000
        );
    }

    #[test]
    fn active_cameras_are_included_and_current_address_takes_precedence() {
        let discovered = camera("one", "192.168.5.20");
        let active = camera("one", "192.168.5.2");
        let missing = camera("two", "192.168.5.3");
        let mut usb = camera("usb", "192.168.5.4");
        usb.transport = Transport::Usb3;
        let list = camera_list(&[discovered, usb], &[active.clone(), missing]);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0], (active, true));
        assert!(list[1].1);
    }

    #[test]
    fn scanning_bounds_parallel_probes_and_preserves_camera_order() {
        let devices: Vec<_> = (2..14)
            .map(|i| camera(&i.to_string(), &format!("192.168.5.{i}")))
            .collect();
        let in_flight = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let count = AtomicUsize::new(0);
        let interface = nic(InterfaceKind::Wired, Some(1000));
        let report = scan_with(
            &devices,
            &[],
            vec![interface.clone()],
            Duration::from_secs(2),
            |_, _| {
                let running = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(running, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(5));
                in_flight.fetch_sub(1, Ordering::SeqCst);
                count.fetch_add(1, Ordering::SeqCst);
                Ok(interface.address)
            },
        );
        assert_eq!(count.load(Ordering::SeqCst), devices.len());
        assert!(peak.load(Ordering::SeqCst) <= 4);
        assert_eq!(
            report
                .cameras
                .iter()
                .map(|camera| &camera.camera)
                .collect::<Vec<_>>(),
            devices.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn malformed_addresses_are_reported_without_network_probes() {
        let devices = [
            camera("bad", "not an address"),
            camera("broadcast", "255.255.255.255"),
            camera("unset", "0.0.0.0"),
        ];
        let report = scan_with(
            &devices,
            &[],
            vec![nic(InterfaceKind::Wired, Some(1000))],
            Duration::from_secs(1),
            |_, _| panic!("invalid camera addresses must not be probed"),
        );
        assert_eq!(report.cameras.len(), devices.len());
        assert!(
            report
                .cameras
                .iter()
                .all(|camera| camera.health == NetworkHealth::Unreachable && !camera.reachable)
        );
    }

    #[test]
    fn scan_deadline_does_not_label_unchecked_cameras_as_unreachable() {
        let devices: Vec<_> = (2..10)
            .map(|i| camera(&i.to_string(), &format!("192.168.5.{i}")))
            .collect();
        let interface = nic(InterfaceKind::Wired, Some(1000));
        let report = scan_with(
            &devices,
            &[],
            vec![interface.clone()],
            Duration::from_millis(1),
            |_, _| {
                std::thread::sleep(Duration::from_millis(5));
                Ok(interface.address)
            },
        );
        assert_eq!(report.cameras.len(), devices.len());
        assert!(report.cameras.iter().any(|camera| !camera.reachable
            && camera.health == NetworkHealth::Unverified
            && camera.summary.contains("scan timed out")));
        assert!(
            report
                .cameras
                .iter()
                .all(|camera| camera.health != NetworkHealth::Unreachable)
        );
    }

    #[test]
    fn ifconfig_reads_negotiated_ethernet_but_does_not_assume_wifi_is_wired() {
        let links = parse_ifconfig(
            "en5: flags=8863<UP,BROADCAST,RUNNING> mtu 9000\n\tmedia: autoselect (2500baseT <full-duplex>)\n\tstatus: active\nen0: flags=8863<UP,BROADCAST,RUNNING> mtu 1500\n\tmedia: autoselect\n\tstatus: active\nbridge0: flags=8863<UP,BROADCAST,RUNNING> mtu 1500\n\tmedia: autoselect (1000baseT <full-duplex>)\n\tstatus: inactive\n",
        );
        assert_eq!(links["en5"].speed_mbps, Some(2500));
        assert_eq!(links["en5"].mtu, Some(9000));
        assert_eq!(links["en5"].kind, Some(InterfaceKind::Wired));
        assert_eq!(links["en0"].kind, None);
        assert_eq!(links["bridge0"].kind, Some(InterfaceKind::Other));
        assert_eq!(links["bridge0"].up, Some(false));
        let ports = parse_hardware_ports(
            "Hardware Port: Wi-Fi\nDevice: en0\nEthernet Address: 00:00:00:00:00:00\n\nHardware Port: USB 10/100/1000 LAN\nDevice: en5\n",
        );
        assert_eq!(ports["en0"], InterfaceKind::Wireless);
        assert_eq!(ports["en5"], InterfaceKind::Wired);
        let ten_gigabit = parse_ifconfig(
            "en9: flags=8863<UP,BROADCAST,RUNNING> mtu 9000\n\tmedia: autoselect (10Gbase-T <full-duplex>)\n\tstatus: active\n",
        );
        assert_eq!(ten_gigabit["en9"].speed_mbps, Some(10000));
    }

    #[test]
    fn windows_metadata_distinguishes_physical_ethernet_wifi_and_virtual() {
        let links = parse_windows_links(
            r#"[{"name":"Ethernet","mtu":9000,"speed_bps":1000000000,"interface_type":6,"physical":true,"up":true},{"name":"Wi-Fi","mtu":1500,"speed_bps":2400000000,"interface_type":71,"physical":true,"up":true},{"name":"Virtual","mtu":1500,"speed_bps":10000000000,"interface_type":6,"physical":false,"up":true}]"#,
        );
        assert_eq!(links["Ethernet"].kind, Some(InterfaceKind::Wired));
        assert_eq!(links["Ethernet"].speed_mbps, Some(1000));
        assert_eq!(links["Wi-Fi"].kind, Some(InterfaceKind::Wireless));
        assert_eq!(links["Virtual"].kind, Some(InterfaceKind::Other));
        let one = parse_windows_links(
            r#"{"name":"Ethernet","mtu":1500,"speed_bps":null,"interface_type":6,"physical":true,"up":false}"#,
        );
        assert_eq!(one["Ethernet"].speed_mbps, None);
        assert_eq!(one["Ethernet"].up, Some(false));
    }
}
