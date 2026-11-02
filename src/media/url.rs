//! Media addresses: their schemes, the checks a source or destination
//! must pass, and the credentials kept out of what is shown and of
//! FFmpeg's arguments.
use super::*;

/// Credential-bearing query names use several case and separator conventions.
/// Decode the name once, as URL query parsers do, while preserving the original
/// spelling when the URL is redacted.
pub(crate) fn credential_query_key(key: &str) -> bool {
    let mut normalized = String::with_capacity(key.len());
    let mut bytes = key.bytes();
    while let Some(mut byte) = bytes.next() {
        if byte == b'%' {
            let high = bytes.next().and_then(|byte| (byte as char).to_digit(16));
            let low = bytes.next().and_then(|byte| (byte as char).to_digit(16));
            let (Some(high), Some(low)) = (high, low) else {
                return false;
            };
            byte = (high * 16 + low) as u8;
        }
        if !matches!(byte, b'-' | b'_') {
            normalized.push(char::from(byte.to_ascii_lowercase()));
        }
    }
    // Cameras name their passwords many ways (pass, passcode, loginpas,
    // userpwd, ...): any name that holds one of these parts is one.
    ["pass", "pwd", "secret", "token", "credential"]
        .iter()
        .any(|part| normalized.contains(part))
        || matches!(
            normalized.as_str(),
            "pw" | "psw"
                | "loginpas"
                | "key"
                | "auth"
                | "authkey"
                | "authorization"
                | "authsig"
                | "signature"
                | "streamid"
                | "apikey"
                | "accesskey"
                | "privatekey"
                | "xamzsignature"
                | "xgoogsignature"
                | "sig"
        )
}

pub fn redact_url(url: &str) -> String {
    let mut result = url.to_string();
    if let Some(scheme_end) = result.find("://") {
        let authority_start = scheme_end + 3;
        let authority_end = result[authority_start..]
            .find(['/', '?', '#'])
            .map(|n| authority_start + n)
            .unwrap_or(result.len());
        if let Some(at) = result[authority_start..authority_end].rfind('@') {
            result.replace_range(authority_start..authority_start + at, "<redacted>");
        }
    }
    if let Some(query) = result.find('?') {
        let path = result[..query].to_string();
        let params = result[query + 1..]
            .split('&')
            .map(|item| {
                let (key, value) = item.split_once('=').unwrap_or((item, ""));
                if credential_query_key(key) {
                    format!("{key}=<redacted>")
                } else if value.is_empty() {
                    key.to_string()
                } else {
                    format!("{key}={value}")
                }
            })
            .collect::<Vec<_>>()
            .join("&");
        result = format!("{path}?{params}");
    }
    // RTMP publishes use the path after the application name as a stream
    // key. Redact it here so worker status, session logs and FFmpeg
    // diagnostics never expose a typed stream password.
    if let Some((protocol, rest)) = result.split_once("://")
        && (protocol.eq_ignore_ascii_case("rtmp") || protocol.eq_ignore_ascii_case("rtmps"))
        && let Some(path) = rest.find('/')
    {
        let start = protocol.len() + 3 + path + 1;
        let end = result[start..]
            .find(['?', '#'])
            .map_or(result.len(), |offset| start + offset);
        if let Some(slash) = result[start..end].find('/') {
            let key = start + slash + 1;
            if key < end {
                result.replace_range(key..end, "<redacted>");
            }
        }
    }
    result
}
/// Whether `url` carries credentials, a user name and password or a key in
/// its query or path, which [`redact_url`] would hide.
pub(super) fn credentialed(url: &str) -> bool {
    redact_url(url) != url
}
/// The protocols a concat script's source may use: network ones only, so
/// the script cannot name a local file.
pub(super) const CONCAT_PROTOCOLS: &str =
    "pipe,tcp,udp,rtp,srtp,tls,http,https,httpproxy,rtmp,rtmps,rtmpt,rtmpts,srt,crypto,data";
/// An FFmpeg concat script naming `source`, with the input `options`
/// (`-name value` pairs) its demuxer and protocol are opened with.
pub(super) fn concat_script<'a>(source: &str, options: impl Iterator<Item = &'a str>) -> String {
    let quote = |text: &str| format!("'{}'", text.replace('\'', "'\\''"));
    let mut script = format!("ffconcat version 1.0\nfile {}\n", quote(source));
    let options = options.collect::<Vec<_>>();
    for pair in options.chunks(2) {
        if let [name, value] = pair {
            let name = name.trim_start_matches('-');
            script.push_str(&format!("option {name} {}\n", quote(value)));
        }
    }
    script
}
pub(super) fn redact_log(message: &str, source: &str) -> String {
    let canonical = canonical_scheme(source);
    message
        .replace(source, &redact_url(source))
        .replace(&canonical, &redact_url(&canonical))
}
pub(super) fn scheme(url: &str) -> Option<String> {
    if url.len() >= 3 && url.as_bytes()[1] == b':' && matches!(url.as_bytes()[2], b'\\' | b'/') {
        return None;
    }
    let (name, _) = url.split_once(':')?;
    if name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    {
        Some(name.to_ascii_lowercase())
    } else {
        None
    }
}
/// FFmpeg protocol names are case-sensitive even though URL schemes are not.
/// Preserve the authority, path and query exactly as supplied.
pub(super) fn canonical_scheme(url: &str) -> String {
    match scheme(url) {
        Some(protocol) => format!("{protocol}:{}", url.split_once(':').unwrap().1),
        None => url.to_owned(),
    }
}
/// Validate the authority and escaping of a network URL without resolving DNS
/// or contacting the service. Error messages never repeat credential-bearing URLs.
pub(crate) fn validate_network_url(url: &str, label: &str) -> Result<()> {
    ensure!(
        url.len() <= 16384 && !url.chars().any(|c| c.is_control() || c.is_whitespace()),
        "{label} must not contain whitespace or control characters; percent-encode URL values"
    );
    let (protocol, rest) = url.split_once("://").context(format!(
        "{label} must include a protocol and host, for example rtsp://camera/live"
    ))?;
    let protocol = protocol.to_ascii_lowercase();
    ensure!(
        scheme(url).as_deref() == Some(protocol.as_str()),
        "{label} protocol is invalid"
    );
    let mut bytes = url.as_bytes().iter();
    while let Some(&byte) = bytes.next() {
        if byte == b'%' {
            ensure!(
                bytes.next().is_some_and(u8::is_ascii_hexdigit)
                    && bytes.next().is_some_and(u8::is_ascii_hexdigit),
                "{label} contains invalid percent escaping; use % followed by two hexadecimal digits"
            );
        }
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = if matches!(protocol.as_str(), "udp" | "rtp") {
        authority.strip_prefix('@').unwrap_or(authority)
    } else if let Some((userinfo, host)) = authority.rsplit_once('@') {
        ensure!(
            !userinfo.is_empty() && !userinfo.contains('@'),
            "{label} credentials are invalid"
        );
        host
    } else {
        authority
    };
    let (host, port) = if let Some(ipv6) = host_port.strip_prefix('[') {
        let (host, suffix) = ipv6.split_once(']').context(format!(
            "{label} IPv6 address is invalid; enclose it in brackets"
        ))?;
        let address = host.split_once("%25").map_or(host, |(ip, _)| ip);
        ensure!(
            address.parse::<std::net::Ipv6Addr>().is_ok(),
            "{label} IPv6 address is invalid"
        );
        if let Some((_, zone)) = host.split_once("%25") {
            ensure!(
                !zone.is_empty()
                    && zone
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
                "{label} IPv6 zone is invalid"
            );
        }
        let port = if suffix.is_empty() {
            None
        } else {
            Some(
                suffix
                    .strip_prefix(':')
                    .context(format!("{label} port is invalid"))?,
            )
        };
        (host, port)
    } else if let Some((host, port)) = host_port.rsplit_once(':') {
        ensure!(
            !host.contains(':'),
            "{label} IPv6 addresses must be enclosed in brackets"
        );
        (host, Some(port))
    } else {
        (host_port, None)
    };
    let binding = matches!(protocol.as_str(), "udp" | "tcp" | "srt" | "rtp") && port.is_some();
    ensure!(!host.is_empty() || binding, "{label} needs a host");
    if !host_port.starts_with('[') {
        ensure!(
            host.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b)),
            "{label} host is invalid; use a DNS name or an IP address"
        );
    }
    if let Some(port) = port {
        ensure!(
            !port.is_empty()
                && port.bytes().all(|b| b.is_ascii_digit())
                && port.parse::<u16>().is_ok_and(|port| port > 0),
            "{label} port must be an integer within 1..65535"
        );
    }
    Ok(())
}

/// Validate media source syntax without opening files, devices or sockets.
pub fn validate_source_shape(url: &str) -> Result<()> {
    ensure!(
        !url.is_empty() && url.len() <= 16384 && !url.chars().any(char::is_control),
        "invalid media source URL"
    );
    if let Some(protocol) = scheme(url) {
        ensure!(
            matches!(
                protocol.as_str(),
                "rtsp"
                    | "rtsps"
                    | "srt"
                    | "rtmp"
                    | "rtmps"
                    | "http"
                    | "https"
                    | "udp"
                    | "tcp"
                    | "rtp"
                    | "file"
                    | "avfoundation"
                    | "v4l2"
                    | "dshow"
            ),
            "unsupported media protocol {protocol}"
        );
        if !matches!(
            protocol.as_str(),
            "file" | "avfoundation" | "v4l2" | "dshow"
        ) {
            validate_network_url(url, "media source URL")?;
        } else if protocol == "file" {
            local_file_path(url)?;
        } else {
            native_source(url)?;
        }
    }
    Ok(())
}

pub fn validate_source(url: &str) -> Result<()> {
    validate_source_shape(url)?;
    if is_recording(url) {
        let path = local_file_path(url)?;
        absolute_source(&path, "a media file source")?;
        let metadata = fs::metadata(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                return crate::camera::missing_path(format!(
                    "media file not found: {}",
                    path.display()
                ));
            }
            anyhow::Error::new(error)
                .context(format!("cannot inspect media file {}", path.display()))
        })?;
        ensure!(
            metadata.is_file(),
            "media source path must name a regular file"
        );
    }
    Ok(())
}
/// Whether `output` is a local file a forward records to (a path or a
/// `file:` URL) rather than a URL it streams to.
pub fn is_recording(output: &str) -> bool {
    matches!(scheme(output).as_deref(), None | Some("file"))
}
/// Whether `url` names a source: by its scheme, or as the absolute path of
/// a file. A relative path never does here: it is from the folder of the
/// command that typed it, which makes it absolute (`cli::paths`), and this
/// process's own folder, a session's, may hold another file of that name.
pub fn is_source(url: &str) -> bool {
    let path = std::path::Path::new(url);
    is_url_source(url) || path.is_absolute() && path.is_file()
}
/// `what`, a local source at `path`, named absolutely: a relative path
/// would be from this process's folder, which for a session is not that of
/// the command that asked for it. A command makes the paths typed to it
/// absolute; a request that sends a relative one is wrong.
pub(crate) fn absolute_source(path: &std::path::Path, what: &str) -> Result<()> {
    if path.is_absolute() {
        return Ok(());
    }
    Err(Categorized::new(
        ErrorKind::Usage,
        format!("{what} must be an absolute path, not {}", path.display()),
    )
    .hint("a session opens no file from its own folder; name the file by its absolute path")
    .into())
}
/// Whether `url` names a source by its scheme alone, without asking the
/// disk whether it is a file.
pub fn is_url_source(url: &str) -> bool {
    scheme(url).is_some_and(|s| {
        matches!(
            s.as_str(),
            "rtsp"
                | "rtsps"
                | "srt"
                | "rtmp"
                | "rtmps"
                | "http"
                | "https"
                | "udp"
                | "tcp"
                | "rtp"
                | "file"
                | "avfoundation"
                | "v4l2"
                | "dshow"
        )
    })
}
pub fn info(url: &str) -> Result<CameraInfo> {
    validate_source(url)?;
    let safe_url = redact_url(url);
    let hash = safe_url.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    });
    Ok(CameraInfo {
        id: format!("media:{hash:016x}"),
        transport: Transport::Media,
        vendor: "Media".into(),
        model: format!(
            "{} input",
            scheme(url).unwrap_or_else(|| "file".into()).to_uppercase()
        ),
        serial: format!("{hash:016x}"),
        address: Some(safe_url),
        profile: Default::default(),
    })
}
