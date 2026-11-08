//! Media addresses: their schemes, the checks a source or destination
//! must pass, and the credentials kept out of what is shown and of
//! `FFmpeg`'s arguments.
#![forbid(unsafe_code)]
use super::*;
use std::borrow::Cow;
use std::fmt::Write as _;

/// Credential-bearing query names use several case and separator conventions.
/// Decode the name once, as URL query parsers do, while preserving the original
/// spelling when the URL is redacted.
pub(crate) fn credential_query_key(key: &str) -> bool {
    let normalized = if key
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        Cow::Borrowed(key)
    } else {
        let mut normalized = String::with_capacity(key.len());
        let mut bytes = key.bytes();
        while let Some(mut byte) = bytes.next() {
            if byte == b'%' {
                let hex = |byte| {
                    char::from(byte)
                        .to_digit(16)
                        .and_then(|digit| u8::try_from(digit).ok())
                };
                let high = bytes.next().and_then(hex);
                let low = bytes.next().and_then(hex);
                let (Some(high), Some(low)) = (high, low) else {
                    return false;
                };
                byte = high * 16 + low;
            }
            if !matches!(byte, b'-' | b'_') {
                normalized.push(char::from(byte.to_ascii_lowercase()));
            }
        }
        Cow::Owned(normalized)
    };
    // Cameras name their passwords many ways (pass, passcode, loginpas,
    // userpwd, ...): any name that holds one of these parts is one.
    ["pass", "pwd", "secret", "token", "credential"]
        .iter()
        .any(|part| normalized.contains(part))
        || matches!(
            normalized.as_ref(),
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

const REDACTED: &str = "<redacted>";

pub(crate) fn redact_url(url: &str) -> String {
    redacted_url(url).into_owned()
}

fn redacted_url(url: &str) -> Cow<'_, str> {
    let mut result = Cow::Borrowed(url);
    if let Some(range) = userinfo_range(url) {
        replace_secret(&mut result, range);
    }
    if let Cow::Owned(query) = redact_query(&result) {
        result = Cow::Owned(query);
    }
    if let Some(range) = stream_key_range(&result) {
        replace_secret(&mut result, range);
    }
    result
}

/// The user information before the last @ in the authority.
fn userinfo_range(url: &str) -> Option<std::ops::Range<usize>> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let (userinfo, _) = authority.rsplit_once('@')?;
    if userinfo == REDACTED {
        return None;
    }
    let start = scheme.len() + 3;
    Some(start..start + userinfo.len())
}

/// Copy only the text that survives the replacement; reuse an owned result.
fn replace_secret(result: &mut Cow<'_, str>, range: std::ops::Range<usize>) {
    match result {
        Cow::Borrowed(text) => {
            let (prefix, rest) = text.split_at(range.start);
            let (_, suffix) = rest.split_at(range.end - range.start);
            let mut text = String::with_capacity(prefix.len() + REDACTED.len() + suffix.len());
            text.push_str(prefix);
            text.push_str(REDACTED);
            text.push_str(suffix);
            *result = Cow::Owned(text);
        }
        Cow::Owned(text) => text.replace_range(range, REDACTED),
    }
}

fn redact_query(url: &str) -> Cow<'_, str> {
    let Some((prefix, query)) = url.split_once('?') else {
        return Cow::Borrowed(url);
    };
    let mut result = None;
    let mut copied = 0;
    for (index, item) in query.split('&').enumerate() {
        let pair = item.split_once('=');
        let (key, value) = pair.unwrap_or((item, ""));
        let secret = credential_query_key(key);
        let changes = if secret {
            pair.is_none() || value != REDACTED
        } else {
            pair.is_some() && value.is_empty()
        };
        if changes || result.is_some() {
            let result = result.get_or_insert_with(|| {
                // The first changed field is a credential or an empty value.
                // Reserve its rendered size, without retaining space for its secret.
                let field = key.len() + if secret { 1 + REDACTED.len() } else { 0 };
                let mut text = String::with_capacity(
                    prefix.len() + 1 + copied + usize::from(index != 0) + field,
                );
                text.push_str(prefix);
                text.push('?');
                text.push_str(
                    query
                        .get(..copied)
                        .expect("query fields delimit valid UTF-8"),
                );
                text
            });
            if index != 0 {
                result.push('&');
            }
            result.push_str(key);
            if secret {
                result.push_str("=<redacted>");
            } else if !value.is_empty() {
                result.push('=');
                result.push_str(value);
            }
        }
        copied += item.len() + usize::from(index != 0);
    }
    result.map_or(Cow::Borrowed(url), Cow::Owned)
}

/// RTMP publishes use the path after the application name as a stream key.
fn stream_key_range(url: &str) -> Option<std::ops::Range<usize>> {
    let (protocol, rest) = url.split_once("://")?;
    if !protocol.eq_ignore_ascii_case("rtmp") && !protocol.eq_ignore_ascii_case("rtmps") {
        return None;
    }
    let (_, tail) = rest.split_once('/')?;
    let start = url.len() - tail.len();
    let end = tail.find(['?', '#']).unwrap_or(tail.len());
    let (path, _) = tail.split_at(end);
    let (_, key) = path.split_once('/')?;
    if key.is_empty() || key == REDACTED {
        return None;
    }
    let end = start + path.len();
    Some(end - key.len()..end)
}
/// Whether `url` carries credentials, a user name and password or a key in
/// its query or path, which [`redact_url`] would hide.
pub(super) fn credentialed(url: &str) -> bool {
    redacted_url(url).as_ref() != url
}
/// The protocols a concat script's source may use: network ones only, so
/// the script cannot name a local file.
pub(super) const CONCAT_PROTOCOLS: &str =
    "pipe,tcp,udp,rtp,srtp,tls,http,https,httpproxy,rtmp,rtmps,rtmpt,rtmpts,srt,crypto,data";
/// An `FFmpeg` concat script naming `source`, with the input `options`
/// (`-name value` pairs) its demuxer and protocol are opened with.
#[allow(
    single_use_lifetimes,
    reason = "Rust 1.89 requires a named lifetime for references in impl Trait"
)]
pub(super) fn concat_script<'a>(
    source: &str,
    mut options: impl Iterator<Item = &'a str>,
) -> String {
    let mut script = String::from("ffconcat version 1.0\nfile ");
    concat_quote(&mut script, source);
    script.push('\n');
    while let Some(name) = options.next() {
        let Some(value) = options.next() else {
            break;
        };
        let name = name.trim_start_matches('-');
        let _ = write!(script, "option {name} ");
        concat_quote(&mut script, value);
        script.push('\n');
    }
    script
}

fn concat_quote(out: &mut String, text: &str) {
    out.push('\'');
    for (index, part) in text.split('\'').enumerate() {
        if index != 0 {
            out.push_str("'\\''");
        }
        out.push_str(part);
    }
    out.push('\'');
}
pub(super) fn redact_log(message: &str, source: &str) -> String {
    let canonical = canonical_scheme(source);
    message
        .replace(source, &redact_url(source))
        .replace(&canonical, &redact_url(&canonical))
}
pub(super) fn scheme(url: &str) -> Option<Cow<'_, str>> {
    scheme_parts(url).map(|(name, _)| {
        if name.bytes().any(|byte| byte.is_ascii_uppercase()) {
            Cow::Owned(name.to_ascii_lowercase())
        } else {
            Cow::Borrowed(name)
        }
    })
}

fn scheme_parts(url: &str) -> Option<(&str, &str)> {
    if url.len() >= 3 && url.as_bytes()[1] == b':' && matches!(url.as_bytes()[2], b'\\' | b'/') {
        return None;
    }
    let (name, rest) = url.split_once(':')?;
    (name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')))
    .then_some((name, rest))
}
/// `FFmpeg` protocol names are case-sensitive even though URL schemes are not.
/// Preserve the authority, path and query exactly as supplied.
#[deny(clippy::unwrap_used)]
pub(super) fn canonical_scheme(url: &str) -> String {
    scheme_parts(url).map_or_else(
        || url.to_owned(),
        |(protocol, rest)| {
            let mut canonical = String::with_capacity(url.len());
            canonical.push_str(protocol);
            canonical.make_ascii_lowercase();
            canonical.push(':');
            canonical.push_str(rest);
            canonical
        },
    )
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
    ensure!(
        scheme_parts(url).is_some_and(|(name, _)| name == protocol),
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
    let host_port = if protocol.eq_ignore_ascii_case("udp") || protocol.eq_ignore_ascii_case("rtp")
    {
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
    let binding = port.is_some()
        && ["udp", "tcp", "srt", "rtp"]
            .iter()
            .any(|name| protocol.eq_ignore_ascii_case(name));
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
pub(crate) fn validate_source_shape(url: &str) -> Result<()> {
    ensure!(
        !url.is_empty() && url.len() <= 16384 && !url.chars().any(char::is_control),
        "invalid media source URL"
    );
    if let Some(protocol) = scheme(url) {
        ensure!(
            matches!(
                protocol.as_ref(),
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
            protocol.as_ref(),
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

pub(crate) fn validate_source(url: &str) -> Result<()> {
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
pub(crate) fn is_recording(output: &str) -> bool {
    matches!(scheme(output).as_deref(), None | Some("file"))
}
/// Whether `url` names a source: by its scheme, or as the absolute path of
/// a file. A relative path never does here: it is from the folder of the
/// command that typed it, which makes it absolute (`cli::paths`), and this
/// process's own folder, a session's, may hold another file of that name.
pub(crate) fn is_source(url: &str) -> bool {
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
pub(crate) fn is_url_source(url: &str) -> bool {
    scheme(url).is_some_and(|s| {
        matches!(
            s.as_ref(),
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
pub(crate) fn info(url: &str) -> Result<CameraInfo> {
    validate_source(url)?;
    let safe_url = redact_url(url);
    let hash = safe_url.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
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
        profile: crate::camera_profile::CameraProfile::default(),
    })
}
