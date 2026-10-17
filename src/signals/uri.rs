//! Signal source URIs, parsed by hand. The authority ends exactly where
//! `media::redact_url` ends it, so redaction always covers the credentials.
use super::Protocol;
use anyhow::{Context, Result, bail, ensure};

/// Longest accepted source URI.
pub const MAX_URI: usize = 8 * 1024;
/// Most OPC UA nodes one source reads.
pub const MAX_NODES: usize = 1024;

#[derive(Clone, PartialEq)]
pub struct SignalUri {
    pub protocol: Protocol,
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub password: Option<String>,
    pub target: Target,
}
impl std::fmt::Debug for SignalUri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignalUri")
            .field("protocol", &self.protocol)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user.as_ref().map(|_| "<redacted>"))
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Modbus(ModbusTarget),
    Mqtt {
        tls: bool,
        filter: String,
        client_id: Option<String>,
        qos: u8,
    },
    Opcua {
        /// `opc.tcp://host:port[/path]`, without credentials or query.
        endpoint: String,
        nodes: Vec<String>,
        root: Option<String>,
    },
    Mtconnect {
        /// The agent's `/current` URL.
        url: String,
        device: Option<String>,
    },
    Sim {
        line: String,
        tags: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    Coil,
    Discrete,
    Input,
    Holding,
}
impl Table {
    pub fn name(self) -> &'static str {
        match self {
            Self::Coil => "coil",
            Self::Discrete => "discrete",
            Self::Input => "input",
            Self::Holding => "holding",
        }
    }
    pub fn function(self) -> u8 {
        match self {
            Self::Coil => 1,
            Self::Discrete => 2,
            Self::Holding => 3,
            Self::Input => 4,
        }
    }
    pub fn is_bits(self) -> bool {
        matches!(self, Self::Coil | Self::Discrete)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegType {
    U16,
    I16,
    U32,
    I32,
    F32,
    F64,
    Bool,
}
impl RegType {
    /// Registers per value.
    pub fn words(self) -> usize {
        match self {
            Self::U16 | Self::I16 | Self::Bool => 1,
            Self::U32 | Self::I32 | Self::F32 => 2,
            Self::F64 => 4,
        }
    }
}

/// Byte order of multi-register values, named by where bytes A (most
/// significant) to D land: `abcd` is big-endian with the high word first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Abcd,
    Cdab,
    Badc,
    Dcba,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModbusTarget {
    pub unit_id: u8,
    pub table: Table,
    pub address: u16,
    pub count: u16,
    pub kind: RegType,
    pub order: Order,
    pub scale: f64,
    pub offset: f64,
    pub name: Option<String>,
    pub unit: Option<String>,
}
impl ModbusTarget {
    /// Registers (or bits) requested per poll.
    pub fn span(&self) -> usize {
        if self.table.is_bits() {
            self.count as usize
        } else {
            self.count as usize * self.kind.words()
        }
    }
    pub fn tag(&self, index: usize) -> String {
        let base = self
            .name
            .clone()
            .unwrap_or_else(|| format!("{}/{}", self.table.name(), self.address));
        if self.count > 1 {
            format!("{base}[{index}]")
        } else {
            base
        }
    }
}

const SCHEMES: &str = "use modbus-tcp://, mqtt://, mqtts://, opc.tcp://, mtconnect://, mtconnects:// or sim-signal://";

/// Parse a source URI's shape. No I/O and no check that the protocol is
/// compiled in (see `require_compiled`).
pub fn parse(uri: &str) -> Result<SignalUri> {
    ensure!(!uri.is_empty(), "signal source URI is empty");
    ensure!(
        uri.len() <= MAX_URI,
        "signal source URI is longer than {MAX_URI} bytes"
    );
    ensure!(
        !uri.chars().any(|c| c.is_control() || c == ' '),
        "signal source URI contains spaces or control characters; percent-encode them"
    );
    let (scheme, rest) = uri
        .split_once("://")
        .ok_or_else(|| anyhow::anyhow!("signal source URI needs a scheme; {SCHEMES}"))?;
    let scheme = scheme.to_ascii_lowercase();
    let protocol = match scheme.as_str() {
        "modbus-tcp" => Protocol::ModbusTcp,
        "mqtt" | "mqtts" => Protocol::Mqtt,
        "opc.tcp" => Protocol::Opcua,
        "mtconnect" | "mtconnects" => Protocol::Mtconnect,
        "sim-signal" => Protocol::SimSignal,
        _ => bail!("unknown signal source scheme '{scheme}'; {SCHEMES}"),
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    ensure!(
        !tail.contains('@'),
        "percent-encode '@' as %40 in topics and node IDs; put credentials before the host"
    );
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(at) => (Some(&authority[..at]), &authority[at + 1..]),
        None => (None, authority),
    };
    let (user, password) = match userinfo {
        Some(info) => {
            let (user, password) = match info.split_once(':') {
                Some((u, p)) => (u, Some(p)),
                None => (info, None),
            };
            ensure!(
                !user.is_empty(),
                "signal source URI has credentials without a user name"
            );
            (Some(decode(user)?), password.map(decode).transpose()?)
        }
        None => (None, None),
    };
    if user.is_some() {
        ensure!(
            matches!(protocol, Protocol::Mqtt | Protocol::Opcua),
            "{scheme}:// sources take no credentials; only mqtt, mqtts and opc.tcp use them"
        );
    }
    let (path, query) = match tail.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (tail, None),
    };
    if protocol != Protocol::Mqtt {
        ensure!(
            !tail.contains('#'),
            "percent-encode '#' as %23 in {scheme}:// URIs"
        );
    }
    let params = query.map(parse_query).transpose()?.unwrap_or_default();
    let mut q = Query {
        params,
        scheme: &scheme,
    };
    if protocol == Protocol::SimSignal {
        ensure!(
            !hostport.is_empty()
                && hostport.len() <= 64
                && hostport
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "sim-signal:// needs a line name of 1..64 letters, digits, '-' or '_'"
        );
        ensure!(
            path.is_empty() || path == "/",
            "sim-signal:// takes no path; use sim-signal://LINE[?tags=N]"
        );
        let tags = q.number("tags", 1, 64)?.unwrap_or(6) as usize;
        q.finish()?;
        return Ok(SignalUri {
            protocol,
            host: hostport.to_string(),
            port: 0,
            user: None,
            password: None,
            target: Target::Sim {
                line: hostport.to_string(),
                tags,
            },
        });
    }
    let default_port = match scheme.as_str() {
        "modbus-tcp" => 502,
        "mqtt" => 1883,
        "mqtts" => 8883,
        "opc.tcp" => 4840,
        "mtconnect" => 5000,
        _ => 443,
    };
    let (host, port) = host_port(hostport, default_port)?;
    let target = match protocol {
        Protocol::ModbusTcp => Target::Modbus(modbus(path, &mut q)?),
        Protocol::Mqtt => {
            let filter = decode(path.strip_prefix('/').unwrap_or(path))?;
            validate_filter(&filter)?;
            let client_id = q.text("client_id")?;
            if let Some(id) = &client_id {
                ensure!(
                    !id.is_empty() && id.len() <= 64,
                    "MQTT client_id must be 1..64 bytes"
                );
            }
            let qos = q.number("qos", 0, 1)?.unwrap_or(0) as u8;
            Target::Mqtt {
                tls: scheme == "mqtts",
                filter,
                client_id,
                qos,
            }
        }
        Protocol::Opcua => {
            let nodes = q.all("node");
            ensure!(
                nodes.len() <= MAX_NODES,
                "opc.tcp:// sources read at most {MAX_NODES} nodes"
            );
            for node in &nodes {
                validate_node_id(node)?;
            }
            let root = q.text("root")?;
            if let Some(root) = &root {
                validate_node_id(root)?;
            }
            if let Some(security) = q.text("security")? {
                ensure!(
                    security.eq_ignore_ascii_case("none"),
                    "opc.tcp:// security must be none; signed and encrypted sessions are planned"
                );
            }
            ensure!(
                !path.contains(['?', '#']),
                "opc.tcp:// endpoint path has reserved characters"
            );
            Target::Opcua {
                endpoint: format!("opc.tcp://{}{path}", join_host(&host, port)),
                nodes,
                root,
            }
        }
        Protocol::Mtconnect => {
            let device = path.trim_matches('/');
            let device = if device.is_empty() {
                None
            } else {
                ensure!(
                    device.len() <= 128
                        && device
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
                    "MTConnect device name must use letters, digits, '-', '_' or '.'"
                );
                Some(device.to_string())
            };
            let http = if scheme == "mtconnects" {
                "https"
            } else {
                "http"
            };
            let device_path = device.as_ref().map(|d| format!("/{d}")).unwrap_or_default();
            Target::Mtconnect {
                url: format!("{http}://{}{device_path}/current", join_host(&host, port)),
                device,
            }
        }
        Protocol::SimSignal => unreachable!(),
    };
    q.finish()?;
    Ok(SignalUri {
        protocol,
        host,
        port,
        user,
        password,
        target,
    })
}

fn join_host(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn host_port(hostport: &str, default_port: u16) -> Result<(String, u16)> {
    let (host, port) = if let Some(rest) = hostport.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .context("signal source IPv6 host needs a closing ']'")?;
        ensure!(
            host.parse::<std::net::Ipv6Addr>().is_ok(),
            "signal source host [{host}] is not an IPv6 address"
        );
        let port = match after {
            "" => None,
            p => Some(
                p.strip_prefix(':')
                    .context("signal source host has text after ']'")?,
            ),
        };
        (host.to_string(), port)
    } else {
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (hostport, None),
        };
        ensure!(
            !host.is_empty()
                && host.len() <= 253
                && host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_'),
            "signal source needs a host name or IP address"
        );
        (host.to_ascii_lowercase(), port)
    };
    let port = match port {
        None => default_port,
        Some(p) => p
            .parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
            .with_context(|| format!("signal source port '{p}' must be 1..65535"))?,
    };
    Ok((host, port))
}

fn modbus(path: &str, q: &mut Query) -> Result<ModbusTarget> {
    const SHAPE: &str = "modbus-tcp:// path must be /UNIT_ID/coil|discrete|input|holding/ADDRESS";
    let parts: Vec<&str> = path
        .strip_prefix('/')
        .unwrap_or(path)
        .trim_end_matches('/')
        .split('/')
        .collect();
    let [unit, table, address] = parts.as_slice() else {
        bail!(SHAPE)
    };
    let unit_id = unit
        .parse::<u8>()
        .ok()
        .context("Modbus unit ID must be 0..255")?;
    let table = match table.to_ascii_lowercase().as_str() {
        "coil" | "coils" => Table::Coil,
        "discrete" => Table::Discrete,
        "input" => Table::Input,
        "holding" => Table::Holding,
        _ => bail!(SHAPE),
    };
    let address = address
        .parse::<u16>()
        .ok()
        .context("Modbus address must be 0..65535 (0-based protocol address)")?;
    let kind = match q.text("type")?.as_deref() {
        None => {
            if table.is_bits() {
                RegType::Bool
            } else {
                RegType::U16
            }
        }
        Some("u16") => RegType::U16,
        Some("i16") => RegType::I16,
        Some("u32") => RegType::U32,
        Some("i32") => RegType::I32,
        Some("f32") => RegType::F32,
        Some("f64") => RegType::F64,
        Some("bool") => RegType::Bool,
        Some(other) => bail!("Modbus type '{other}' must be u16, i16, u32, i32, f32, f64 or bool"),
    };
    let order = match q.text("order")?.as_deref() {
        None | Some("abcd") => Order::Abcd,
        Some("cdab") => Order::Cdab,
        Some("badc") => Order::Badc,
        Some("dcba") => Order::Dcba,
        Some(other) => bail!("Modbus order '{other}' must be abcd, cdab, badc or dcba"),
    };
    let count = q.number("count", 1, 2000)?.unwrap_or(1) as u16;
    let scale = q.float("scale")?.unwrap_or(1.0);
    let offset = q.float("offset")?.unwrap_or(0.0);
    let name = q.text("name")?;
    let unit = q.text("unit")?;
    for text in [&name, &unit].into_iter().flatten() {
        ensure!(
            !text.is_empty() && text.len() <= 128 && !text.chars().any(char::is_control),
            "Modbus name and unit must be 1..128 bytes without control characters"
        );
    }
    let target = ModbusTarget {
        unit_id,
        table,
        address,
        count,
        kind,
        order,
        scale,
        offset,
        name,
        unit,
    };
    if table.is_bits() {
        ensure!(
            kind == RegType::Bool,
            "Modbus {} tables hold bits; leave out type or use type=bool",
            table.name()
        );
    } else {
        ensure!(
            target.span() <= 125,
            "Modbus reads at most 125 registers at once; lower count (count x registers per value = {})",
            target.span()
        );
    }
    ensure!(
        address as usize + target.span() <= 65536,
        "Modbus address range runs past 65535"
    );
    Ok(target)
}

/// MQTT topic filter rules: `#` only as the whole last level, `+` only as a
/// whole level.
fn validate_filter(filter: &str) -> Result<()> {
    ensure!(
        !filter.is_empty(),
        "mqtt:// needs a topic filter after the host, for example mqtt://broker/plant/#"
    );
    ensure!(
        filter.len() <= 1024,
        "MQTT topic filter is longer than 1024 bytes"
    );
    let levels: Vec<&str> = filter.split('/').collect();
    for (i, level) in levels.iter().enumerate() {
        if level.contains('#') {
            ensure!(
                *level == "#" && i + 1 == levels.len(),
                "MQTT '#' may only be the whole last topic level"
            );
        }
        if level.contains('+') {
            ensure!(*level == "+", "MQTT '+' must be a whole topic level");
        }
    }
    ensure!(
        !filter.contains('\0'),
        "MQTT topic filter contains a NUL byte"
    );
    Ok(())
}

/// The shape of an OPC UA NodeId: `[ns=N;](i=N|s=TEXT|g=GUID|b=BASE64)`.
pub fn validate_node_id(node: &str) -> Result<()> {
    const SHAPE: &str =
        "OPC UA node IDs look like ns=2;s=Line1.Temp, i=2258, ns=1;g=<GUID> or ns=1;b=<base64>";
    let body = match node.strip_prefix("ns=") {
        Some(rest) => {
            let (ns, body) = rest.split_once(';').context(SHAPE)?;
            ensure!(
                !ns.is_empty() && ns.len() <= 5 && ns.bytes().all(|b| b.is_ascii_digit()),
                SHAPE
            );
            body
        }
        None => node,
    };
    let ok = if let Some(id) = body.strip_prefix("i=") {
        id.parse::<u32>().is_ok()
    } else if let Some(id) = body.strip_prefix("s=") {
        !id.is_empty() && id.len() <= 4096
    } else if let Some(id) = body.strip_prefix("g=") {
        id.len() == 36
            && id.bytes().enumerate().all(|(i, b)| {
                if matches!(i, 8 | 13 | 18 | 23) {
                    b == b'-'
                } else {
                    b.is_ascii_hexdigit()
                }
            })
    } else if let Some(id) = body.strip_prefix("b=") {
        !id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b))
    } else {
        false
    };
    ensure!(ok, SHAPE);
    Ok(())
}

struct Query<'a> {
    params: Vec<(String, String)>,
    scheme: &'a str,
}
impl Query<'_> {
    fn take(&mut self, key: &str) -> Vec<String> {
        let mut values = Vec::new();
        self.params.retain(|(k, v)| {
            if k == key {
                values.push(v.clone());
                false
            } else {
                true
            }
        });
        values
    }
    fn all(&mut self, key: &str) -> Vec<String> {
        self.take(key)
    }
    fn text(&mut self, key: &str) -> Result<Option<String>> {
        let mut values = self.take(key);
        ensure!(values.len() <= 1, "signal source URI repeats '{key}'");
        Ok(values.pop())
    }
    fn number(&mut self, key: &str, min: u64, max: u64) -> Result<Option<u64>> {
        self.text(key)?
            .map(|v| {
                v.parse::<u64>()
                    .ok()
                    .filter(|n| (min..=max).contains(n))
                    .with_context(|| format!("'{key}' must be a whole number {min}..{max}"))
            })
            .transpose()
    }
    fn float(&mut self, key: &str) -> Result<Option<f64>> {
        self.text(key)?
            .map(|v| {
                v.parse::<f64>()
                    .ok()
                    .filter(|n| n.is_finite())
                    .with_context(|| format!("'{key}' must be a finite number"))
            })
            .transpose()
    }
    fn finish(self) -> Result<()> {
        if let Some((key, _)) = self.params.first() {
            bail!("{}:// sources take no '{key}' parameter", self.scheme);
        }
        Ok(())
    }
}

fn parse_query(query: &str) -> Result<Vec<(String, String)>> {
    query
        .split('&')
        .filter(|item| !item.is_empty())
        .map(|item| {
            let (key, value) = item.split_once('=').unwrap_or((item, ""));
            Ok((decode(key)?, decode(value)?))
        })
        .collect()
}

/// Percent-decode to UTF-8.
pub fn decode(text: &str) -> Result<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .context("signal source URI has a malformed %-escape")?;
            out.push(hex);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let text = String::from_utf8(out).context("signal source URI is not UTF-8 after decoding")?;
    ensure!(
        !text.chars().any(|c| c.is_control()),
        "signal source URI decodes to control characters"
    );
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(uri: &str) -> String {
        format!("{:#}", parse(uri).unwrap_err())
    }

    #[test]
    fn parses_each_scheme() {
        let u = parse(
            "modbus-tcp://10.0.0.5/1/holding/100?count=2&type=f32&order=cdab&scale=0.5&unit=degC",
        )
        .unwrap();
        assert_eq!((u.host.as_str(), u.port), ("10.0.0.5", 502));
        let Target::Modbus(m) = u.target else {
            panic!()
        };
        assert_eq!(
            (m.unit_id, m.table, m.address, m.count, m.span()),
            (1, Table::Holding, 100, 2, 4)
        );
        assert_eq!(m.tag(1), "holding/100[1]");
        let u = parse("mqtts://user:p%40ss@broker.local/plant/+/temp?qos=1").unwrap();
        assert_eq!(
            (u.port, u.user.as_deref(), u.password.as_deref()),
            (8883, Some("user"), Some("p@ss"))
        );
        assert!(
            matches!(&u.target, Target::Mqtt { tls: true, filter, qos: 1, .. } if filter == "plant/+/temp")
        );
        let u = parse("mqtt://broker/plant/#").unwrap();
        assert!(matches!(&u.target, Target::Mqtt { filter, .. } if filter == "plant/#"));
        let u = parse("mqtt://broker/a/%23").unwrap();
        assert!(matches!(&u.target, Target::Mqtt { filter, .. } if filter == "a/#"));
        let u = parse("opc.tcp://[::1]:4841/server?node=ns=2;s=Line1.Temp&node=i=2258").unwrap();
        let Target::Opcua {
            endpoint, nodes, ..
        } = u.target
        else {
            panic!()
        };
        assert_eq!(endpoint, "opc.tcp://[::1]:4841/server");
        assert_eq!(nodes, ["ns=2;s=Line1.Temp", "i=2258"]);
        let u = parse("mtconnect://agent/mill").unwrap();
        assert!(
            matches!(&u.target, Target::Mtconnect { url, .. } if url == "http://agent:5000/mill/current")
        );
        let u = parse("sim-signal://line1?tags=9").unwrap();
        assert!(matches!(&u.target, Target::Sim { line, tags: 9 } if line == "line1"));
    }

    #[test]
    fn rejects_bad_shapes() {
        for (uri, text) in [
            ("ftp://x/y", "unknown signal source scheme 'ftp'"),
            ("modbus-tcp://h/256/holding/0", "unit ID"),
            ("modbus-tcp://h/1/holding/65536", "address"),
            ("modbus-tcp://h/1/holding/65535?count=2", "runs past"),
            (
                "modbus-tcp://h/1/holding/0?count=63&type=f32",
                "125 registers",
            ),
            ("modbus-tcp://h/1/coil/0?count=2001", "count"),
            ("modbus-tcp://h/1/coil/0?type=f32", "hold bits"),
            ("modbus-tcp://h/1/holding/0?order=xyzw", "order"),
            ("modbus-tcp://h/1/holding/0?bogus=1", "no 'bogus' parameter"),
            ("modbus-tcp://h:0/1/holding/0", "port"),
            ("mqtt://b/a/#/b", "'#'"),
            ("mqtt://b/a+/b", "'+'"),
            ("mqtt://b", "topic filter"),
            ("mqtt://b/t?qos=2", "qos"),
            ("opc.tcp://h?node=x=1", "node IDs"),
            ("opc.tcp://h?node=i=1&security=sign", "security"),
            ("mqtt://b/a@b", "%40"),
            ("mqtt://u:p/w@b/t", "%40"),
            ("mqtt://u:p#w@b/t", "%40"),
            ("modbus-tcp://u:p@h/1/holding/0", "take no credentials"),
            ("sim-signal://bad.line", "line name"),
            ("sim-signal://l?tags=65", "tags"),
        ] {
            let message = err(uri);
            assert!(message.contains(text), "{uri}: {message}");
            let lower = message.to_ascii_lowercase();
            assert!(
                !["not found", "disabled", "unsupported", "not supported"]
                    .iter()
                    .any(|w| lower.contains(w)),
                "{uri}: {message}"
            );
        }
    }
}
