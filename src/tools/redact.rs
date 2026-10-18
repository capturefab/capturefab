//! Redaction for everything the tools layer shows or sends.
//!
//! `for_output` is for people and command-line callers: the CLI, GUI facts,
//! activity labels and plan files. It removes URL credentials from every
//! string and RTMP stream keys, which sit in the URL path. Values (strings
//! without spaces: IDs, selectors, URLs, paths) are redacted whole, as
//! workflows redact command fields; text is redacted word by word, as the
//! session's activity log is, so the words after a URL survive.
//! `for_model` is for everything sent to an AI provider. It also reduces
//! network URLs to `scheme://host[:port]`, drops access key IDs, AWS profile
//! names, serial numbers and addresses, and writes the home folder as `~`.
use serde_json::Value;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};

/// Keys `for_model` leaves out wherever they appear: identifiers that tell a
/// provider about the user's accounts, hardware or network.
const MODEL_DROPPED: [&str; 4] = ["access_key_id", "profile", "serial", "address"];
/// UI-only Secret fields must never survive serialization, even when a
/// caller accidentally puts them in a value before redacting it.
const SECRET_FIELDS: [&str; 2] = ["secret", "password"];

/// `value` with URL credentials and stream keys removed from every string.
pub fn for_output(mut value: Value) -> Value {
    hide_secret_fields(&mut value);
    map_strings(&mut value, &output_string);
    value
}

/// `text` with the credentials of every URL in it removed: user info,
/// credential query parameters and RTMP stream keys.
pub fn for_output_text(text: &str) -> String {
    hide_access_key_ids(&words(&hide_userinfo(text), &|word| {
        if word.contains("://") {
            map_urls(word, &|url| rtmp_key(&crate::media::redact_url(url)))
        } else if word.contains('?') {
            crate::media::redact_url(word)
        } else {
            word.to_owned()
        }
    }))
}

/// An error message for people: `for_output_text`, after hiding every
/// string of `known` (the call's own arguments or command) that carries
/// credentials even where the message does not show it as a URL.
pub fn for_output_error(message: &str, known: &Value) -> String {
    for_output_text(&crate::workflow::redact_error(message, known))
}

/// `value` as an AI provider may see it: `for_output`, without identifying
/// keys, with network URLs reduced to their host and the home folder as `~`.
pub fn for_model(value: Value) -> Value {
    model_value(value, home().as_deref())
}

/// `text` as an AI provider may see it.
pub fn for_model_text(text: &str) -> String {
    reduce(&for_output_text(text), home().as_deref())
}

/// An error message for an AI provider; see `for_output_error`.
pub fn for_model_error(message: &str, known: &Value) -> String {
    for_model_text(&crate::workflow::redact_error(message, known))
}

/// `path` with the home folder written `~`, as locations are shown to
/// anyone: "~/Captures/line-1".
pub fn home_path(path: &Path) -> String {
    shorten_home(&path.display().to_string(), home().as_deref())
}

fn model_value(mut value: Value, home: Option<&Path>) -> Value {
    drop_keys(&mut value);
    map_strings(&mut value, &|text| reduce(&output_string(text), home));
    value
}

/// One string for people: a value whole, as workflows redact command
/// fields, then word by word.
fn output_string(text: &str) -> String {
    if text.contains(char::is_whitespace) {
        return for_output_text(text);
    }
    let mut value = Value::String(text.to_owned());
    crate::workflow::redact_value(&mut value);
    for_output_text(value.as_str().unwrap_or(text))
}

/// Already redacted `text` with network URLs reduced to their host and the
/// home folder written `~`. File URLs keep their (shortened) path.
fn reduce(text: &str, home: Option<&Path>) -> String {
    let reduced = words(text, &|word| {
        let word = map_urls(word, &|url| {
            if scheme_of(url).eq_ignore_ascii_case("file") {
                url.to_owned()
            } else {
                host_only(url)
            }
        });
        hide_access_key_ids(&word)
    });
    shorten_home(&reduced, home)
}

/// `word` with any AWS access key ID in it (`AKIA…`/`ASIA…` and 16 more
/// capitals or digits) replaced, for messages that quote one outside an
/// `access_key_id` field.
fn hide_access_key_ids(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut rest = word;
    while let Some(at) = ["AKIA", "ASIA"].iter().filter_map(|p| rest.find(p)).min() {
        let candidate = &rest[at..];
        let length = candidate
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit()))
            .unwrap_or(candidate.len());
        let boundary = rest[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        out.push_str(&rest[..at]);
        if boundary && length == 20 {
            out.push_str("<redacted>");
        } else {
            out.push_str(&candidate[..length.max(4)]);
        }
        rest = &candidate[length.max(4)..];
    }
    out.push_str(rest);
    out
}

/// Apply `map` to each whitespace-separated word of `text`.
fn words(text: &str, map: &dyn Fn(&str) -> String) -> String {
    text.split_inclusive(char::is_whitespace)
        .map(|part| {
            let word = part.trim_end_matches(char::is_whitespace);
            format!("{}{}", map(word), &part[word.len()..])
        })
        .collect()
}

/// `text` with the user info of every URL replaced, as `media::redact_url`
/// finds it: the authority runs to the next `/`, `?` or `#` even across
/// spaces, so a password with an unencoded space is hidden whole.
fn hide_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(marker) = rest.find("://") {
        let start = marker + 3;
        let end = rest[start..]
            .find(['/', '?', '#'])
            .map_or(rest.len(), |n| start + n);
        match rest[start..end].rfind('@') {
            // Already redacted user info stays as it is.
            Some(at) if &rest[start..start + at] != "<redacted>" => {
                out.push_str(&rest[..start]);
                out.push_str("<redacted>");
                rest = &rest[start + at..];
            }
            _ => {
                out.push_str(&rest[..start]);
                rest = &rest[start..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn map_strings(value: &mut Value, map: &dyn Fn(&str) -> String) {
    match value {
        Value::String(text) => *text = map(text),
        Value::Array(values) => values.iter_mut().for_each(|value| map_strings(value, map)),
        Value::Object(fields) => fields
            .values_mut()
            .for_each(|value| map_strings(value, map)),
        _ => {}
    }
}

fn drop_keys(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(drop_keys),
        Value::Object(fields) => {
            fields.retain(|key, _| {
                !MODEL_DROPPED.contains(&key.as_str()) && !SECRET_FIELDS.contains(&key.as_str())
            });
            fields.values_mut().for_each(drop_keys);
        }
        _ => {}
    }
}

fn hide_secret_fields(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(hide_secret_fields),
        Value::Object(fields) => {
            for (key, value) in fields {
                if SECRET_FIELDS.contains(&key.as_str()) || key == "access_key_id" {
                    *value = Value::String("<redacted>".into());
                } else {
                    hide_secret_fields(value);
                }
            }
        }
        _ => {}
    }
}

/// Apply `map` to every `scheme://…` URL inside `text`. A URL runs to the
/// next whitespace or quote (not `<`, which redacted URLs contain); trailing
/// sentence punctuation stays outside it.
fn map_urls(text: &str, map: &dyn Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(marker) = rest.find("://") {
        let start = rest[..marker]
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            .last()
            .map_or(marker, |(at, _)| at);
        let after = marker + 3;
        let end = rest[after..]
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`'))
            .map_or(rest.len(), |n| after + n);
        if start == marker {
            // "://" with no scheme before it is not a URL.
            out.push_str(&rest[..after]);
            rest = &rest[after..];
            continue;
        }
        let url = &rest[start..end];
        let trimmed = url.trim_end_matches(['.', ',', ';', ':', ')', '!', '?']);
        out.push_str(&rest[..start]);
        out.push_str(&map(trimmed));
        out.push_str(&url[trimmed.len()..]);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn scheme_of(url: &str) -> &str {
    url.split_once("://").map_or("", |(scheme, _)| scheme)
}

/// The authority of `url`: where it ends and where its host starts, after
/// any user info.
fn authority(url: &str) -> Option<(usize, usize, usize)> {
    let start = url.find("://")? + 3;
    let end = url[start..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |n| start + n);
    let host = url[start..end]
        .rfind('@')
        .map_or(start, |at| start + at + 1);
    Some((start, host, end))
}

/// `rtmp://host/app/KEY` as `rtmp://host/app/<redacted>`: everything after
/// an RTMP URL's application name is its stream key.
fn rtmp_key(url: &str) -> String {
    if !matches!(
        scheme_of(url).to_ascii_lowercase().as_str(),
        "rtmp" | "rtmps"
    ) {
        return url.to_owned();
    }
    let Some((_, _, path)) = authority(url) else {
        return url.to_owned();
    };
    let tail = url[path..].find(['?', '#']).map_or(url.len(), |n| path + n);
    let segments = url[path..tail].trim_start_matches('/');
    match segments.split_once('/') {
        Some((app, key)) if !app.is_empty() && !key.is_empty() => {
            format!("{}/{app}/<redacted>{}", &url[..path], &url[tail..])
        }
        _ => url.to_owned(),
    }
}

/// `scheme://host[:port]`, without user info, path, query or fragment.
fn host_only(url: &str) -> String {
    match authority(url) {
        Some((_, host, end)) => format!("{}://{}", scheme_of(url), &url[host..end]),
        None => url.to_owned(),
    }
}

/// The user's home folder, which `for_model` writes `~`.
fn home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .filter(|home| home.parent().is_some())
}

/// `text` with each path under `home` written from `~`. Only whole path
/// components match, so `/Users/ann` never shortens `/Users/anna`.
fn shorten_home(text: &str, home: Option<&Path>) -> String {
    let Some(home) = home.and_then(Path::to_str) else {
        return text.to_owned();
    };
    let home = home.trim_end_matches(['/', '\\']);
    if home.is_empty() {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(home) {
        let after = &rest[at + home.len()..];
        let whole = after
            .chars()
            .next()
            .is_none_or(|c| c == MAIN_SEPARATOR || c == '/' || !is_path_char(c));
        let starts = rest[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !is_path_char(c) || c == ':');
        out.push_str(&rest[..at]);
        out.push_str(if whole && starts { "~" } else { home });
        rest = after;
    }
    out.push_str(rest);
    out
}

fn is_path_char(c: char) -> bool {
    !(c.is_whitespace() || matches!(c, '"' | '\'' | '`' | ',' | ';' | '(' | ')' | '[' | ']'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PLANTED: [&str; 5] = [
        "rtsp://admin:hunter2@10.0.0.9/stream",
        "srt://relay.example:9000?passphrase=sesame&latency=200",
        "rtmp://a.rtmp.youtube.com/live2/abcd-efgh-ijkl",
        "AKIAIOSFODNN7EXAMPLE",
        "/home/ana/Captures/line-1/shot.png",
    ];

    fn planted() -> Value {
        json!({
            "url": PLANTED[0],
            "nested": [{"target": PLANTED[1]}, PLANTED[2]],
            "message": format!("could not open {}: refused", PLANTED[0]),
            "credentials": {"source": "keychain", "access_key_id": PLANTED[3]},
            "camera": {"serial": "SN-77", "address": "10.0.0.9", "id": "gige:10.0.0.9"},
            "path": PLANTED[4],
        })
    }

    #[test]
    fn output_redaction_hides_credentials_and_stream_keys_but_keeps_hosts() {
        let text = for_output(planted()).to_string();
        for secret in ["hunter2", "admin:", "sesame", "abcd-efgh-ijkl"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        assert!(text.contains("rtsp://<redacted>@10.0.0.9/stream"));
        assert!(text.contains("passphrase=<redacted>&latency=200"));
        assert!(text.contains("rtmp://a.rtmp.youtube.com/live2/<redacted>"));
        // Paths remain useful to local callers; access key IDs do not
        // belong in dry runs, diagnostics or execution reports.
        assert!(!text.contains(PLANTED[3]) && text.contains(PLANTED[4]));
        assert_eq!(
            for_output_text("forwarding to rtmps://h/app/inst/KEY123, then stopped."),
            "forwarding to rtmps://h/app/<redacted>, then stopped."
        );
        // Idempotent, and a bare application name is not a key.
        let once = for_output_text(PLANTED[2]);
        assert_eq!(for_output_text(&once), once);
        assert_eq!(for_output_text("rtmp://h/live"), "rtmp://h/live");
    }

    #[test]
    fn model_redaction_reduces_urls_and_drops_identifying_keys() {
        let value = model_value(planted(), Some(Path::new("/home/ana")));
        let text = value.to_string();
        for secret in [
            "hunter2",
            "sesame",
            "abcd-efgh-ijkl",
            "live2",
            "/stream",
            PLANTED[3],
            "SN-77",
            "/home/ana",
        ] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        assert_eq!(value["url"], "rtsp://10.0.0.9");
        assert_eq!(value["nested"][0]["target"], "srt://relay.example:9000");
        assert_eq!(value["nested"][1], "rtmp://a.rtmp.youtube.com");
        assert_eq!(value["message"], "could not open rtsp://10.0.0.9: refused");
        assert_eq!(value["camera"], json!({"id": "gige:10.0.0.9"}));
        assert_eq!(value["path"], "~/Captures/line-1/shot.png");
        assert_eq!(value["credentials"], json!({"source": "keychain"}));
        assert_eq!(
            reduce(
                "key AKIAIOSFODNN7EXAMPLE, not AKIA1 or XAKIAIOSFODNN7EXAMPLE",
                None
            ),
            "key <redacted>, not AKIA1 or XAKIAIOSFODNN7EXAMPLE"
        );
        // A discovered ONVIF device keeps a usable, reduced ID.
        assert_eq!(
            reduce(
                &for_output_text("onvif:http://10.0.0.5/onvif/device_service"),
                None
            ),
            "onvif:http://10.0.0.5"
        );
    }

    #[test]
    fn home_shortening_matches_whole_components_only() {
        let home = Some(Path::new("/Users/ann"));
        assert_eq!(shorten_home("/Users/ann", home), "~");
        assert_eq!(
            shorten_home("saved /Users/ann/x.png.", home),
            "saved ~/x.png."
        );
        assert_eq!(shorten_home("/Users/anna/x.png", home), "/Users/anna/x.png");
        assert_eq!(shorten_home("/data/Users/ann/x", home), "/data/Users/ann/x");
        assert_eq!(
            shorten_home("file:/Users/ann/a.mp4", home),
            "file:~/a.mp4",
            "a file selector is shortened after its scheme"
        );
        assert_eq!(shorten_home("/Users/ann/x", None), "/Users/ann/x");
    }

    #[test]
    fn ui_only_secret_fields_never_reach_output_or_models() {
        let value = json!({
            "password": "stream-password",
            "nested": [{"secret": "aws-secret", "token": "camera-undo-token"}],
        });
        let output = for_output(value.clone());
        assert_eq!(output["password"], "<redacted>");
        assert_eq!(output["nested"][0]["secret"], "<redacted>");
        let model = for_model(value);
        assert!(model.get("password").is_none());
        assert!(model["nested"][0].get("secret").is_none());
        assert_eq!(model["nested"][0]["token"], "camera-undo-token");
        for text in [output.to_string(), model.to_string()] {
            assert!(!text.contains("stream-password") && !text.contains("aws-secret"));
        }
    }

    #[test]
    fn urls_inside_text_are_found_without_a_scheme_false_positive() {
        assert_eq!(map_urls("a :// b", &|_| "X".into()), "a :// b");
        assert_eq!(
            map_urls("(see rtsp://u:p@h/s).", &|url| format!("<{url}>")),
            "(see <rtsp://u:p@h/s>)."
        );
        assert_eq!(
            map_urls("onvif:http://h/x and srt://h?streamid=1", &|url| host_only(
                url
            )),
            "onvif:http://h and srt://h"
        );
        assert_eq!(host_only("rtsp://[::1]:8554/s"), "rtsp://[::1]:8554");
    }
}
