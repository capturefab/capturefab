//! Capture context: short key/value tags (label, line, station, part, lot,
//! note) carried with captures, recordings and dataset notes, and usable in
//! output names as {tag:KEY}.
use anyhow::{Result, ensure};

pub type Context = std::collections::BTreeMap<String, String>;

pub const MAX_ENTRIES: usize = 16;
/// Keys match `^[a-z][a-z0-9_]{0,31}$`.
pub const MAX_KEY: usize = 32;
/// Values are at most this many bytes, without control characters.
pub const MAX_VALUE: usize = 256;

pub fn validate(context: &Context) -> Result<()> {
    ensure!(
        context.len() <= MAX_ENTRIES,
        "context accepts at most {MAX_ENTRIES} entries"
    );
    for (key, value) in context {
        validate_key(key)?;
        validate_value(key, value)?;
    }
    Ok(())
}

/// Whether `key` matches `^[a-z][a-z0-9_]{0,31}$`.
pub fn is_valid_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    key.len() <= MAX_KEY
        && bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn validate_key(key: &str) -> Result<()> {
    ensure!(
        is_valid_key(key),
        "context key '{}' must start with a lowercase letter and use only a-z, 0-9 and _ (at most {MAX_KEY} characters)",
        key.escape_debug()
    );
    Ok(())
}

fn validate_value(key: &str, value: &str) -> Result<()> {
    ensure!(
        value.len() <= MAX_VALUE && !value.chars().any(char::is_control),
        "context value for '{key}' must be at most {MAX_VALUE} bytes without control characters"
    );
    Ok(())
}

/// Parse one `KEY=VALUE` pair for clap, splitting on the first '='.
pub fn parse_pair(s: &str) -> Result<(String, String), String> {
    let (key, value) = s
        .split_once('=')
        .ok_or_else(|| "context must look like KEY=VALUE, such as line=L1".to_string())?;
    validate_key(key).map_err(|e| e.to_string())?;
    validate_value(key, value).map_err(|e| e.to_string())?;
    Ok((key.to_owned(), value.to_owned()))
}

/// Collect parsed pairs, refusing a key given twice.
pub fn from_pairs(pairs: &[(String, String)]) -> Result<Context> {
    let mut context = Context::new();
    for (key, value) in pairs {
        ensure!(
            context.insert(key.clone(), value.clone()).is_none(),
            "context key '{key}' is given more than once"
        );
    }
    validate(&context)?;
    Ok(context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_values_and_limits() {
        assert_eq!(parse_pair("line=L1=a"), Ok(("line".into(), "L1=a".into())));
        assert_eq!(parse_pair("note="), Ok(("note".into(), String::new())));
        for bad in ["=x", "Line=x", "1a=x", "a-b=x", "noequals", "a=x\ny"] {
            assert!(parse_pair(bad).is_err(), "{bad}");
        }
        assert!(parse_pair(&format!("{}=x", "a".repeat(33))).is_err());
        assert!(parse_pair(&format!("a={}", "x".repeat(257))).is_err());
        let many: Context = (0..17).map(|i| (format!("k{i}"), "v".into())).collect();
        assert!(validate(&many).is_err());
        let pairs = vec![("a".to_string(), "1".to_string()); 2];
        assert!(from_pairs(&pairs).is_err());
    }
}
