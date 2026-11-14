//! Bounded Microsoft CRT argv encoding; no shell or ambiguous executable search.
use super::Config;
use crate::types::Usage;
use anyhow::Result;
use zeroize::Zeroizing;

const MAX_COMMAND: usize = 32_767;
pub(super) struct Prepared {
    pub(super) application: Zeroizing<Vec<u16>>,
    pub(super) command: Zeroizing<Vec<u16>>,
}
fn invalid() -> anyhow::Error {
    Usage("Windows industrial helper requires a drive-qualified absolute .exe path and bounded NUL-free CRT arguments".into()).into()
}
fn executable(path: &str) -> bool {
    let bytes = path.as_bytes();
    let Some(prefix) = bytes.first_chunk::<3>() else {
        return false;
    };
    if !prefix[0].is_ascii_alphabetic() || prefix[1] != b':' || !matches!(prefix[2], b'\\' | b'/') {
        return false;
    }
    let Some(rest) = path.get(3..) else {
        return false;
    };
    !rest.is_empty()
        && rest
            .get(rest.len().saturating_sub(4)..)
            .is_some_and(|extension| extension.eq_ignore_ascii_case(".exe"))
        && !rest
            .chars()
            .any(|c| c.is_control() || matches!(c, ':' | '"' | '<' | '>' | '|' | '?' | '*'))
        && rest.split(['\\', '/']).all(|part| {
            !part.is_empty() && !matches!(part, "." | "..") && !part.ends_with([' ', '.'])
        })
}
fn push(output: &mut Vec<u16>, value: u16) -> Result<()> {
    if output.len() >= MAX_COMMAND - 1 {
        return Err(invalid());
    }
    output.push(value);
    Ok(())
}
fn argument(output: &mut Vec<u16>, value: &str) -> Result<()> {
    if value.contains('\0') {
        return Err(invalid());
    }
    push(output, u16::from(b'"'))?;
    let mut slashes = 0_usize;
    for unit in value.encode_utf16() {
        if unit == u16::from(b'\\') {
            slashes += 1;
            continue;
        }
        let count = if unit == u16::from(b'"') {
            slashes.saturating_mul(2).saturating_add(1)
        } else {
            slashes
        };
        for _ in 0..count {
            push(output, u16::from(b'\\'))?;
        }
        slashes = 0;
        push(output, unit)?;
    }
    for _ in 0..slashes.saturating_mul(2) {
        push(output, u16::from(b'\\'))?;
    }
    push(output, u16::from(b'"'))
}
pub(super) fn prepare(config: &Config) -> Result<Prepared> {
    let path = config
        .executable
        .to_str()
        .filter(|path| executable(path))
        .ok_or_else(invalid)?;
    if config.args.len() > 1024 {
        return Err(invalid());
    }
    let mut command = Zeroizing::new(Vec::new());
    argument(&mut command, path)?;
    for value in &config.args {
        push(&mut command, u16::from(b' '))?;
        argument(&mut command, value)?;
    }
    command.push(0);
    let mut application = Zeroizing::new(path.encode_utf16().collect::<Vec<_>>());
    application.push(0);
    Ok(Prepared {
        application,
        command,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_crt_literals_keep_empty_unicode_quote_and_trailing_backslash() {
        let config = Config {
            executable: r"C:\plant tools\é-reader.exe".into(),
            args: vec![
                String::new(),
                "a b".into(),
                "ab\"c".into(),
                r"tail\".into(),
                "😀".into(),
            ],
        };
        let prepared = prepare(&config).unwrap();
        let text = String::from_utf16(prepared.command.strip_suffix(&[0]).unwrap()).unwrap();
        assert_eq!(
            text,
            "\"C:\\plant tools\\é-reader.exe\" \"\" \"a b\" \"ab\\\"c\" \"tail\\\\\" \"😀\""
        );
        assert_eq!(prepared.application.last(), Some(&0));
    }
    #[test]
    fn no_implicit_shell_or_path_search_and_utf16_extent_is_exact() {
        for path in [
            "helper.exe",
            "C:helper.exe",
            r"\helper.exe",
            r"\\server\share\helper.exe",
            r"\\?\C:\helper.exe",
            r"C:\helper.cmd",
            r"C:\a\..\helper.exe",
            r"C:\a\helper.exe ",
            "C:\\helper\0.exe",
        ] {
            let error = prepare(&Config {
                executable: path.into(),
                args: Vec::new(),
            })
            .err()
            .unwrap();
            assert_eq!(
                crate::types::classify(&error).kind,
                crate::types::ErrorKind::Usage
            );
        }
        let mut config = Config {
            executable: r"C:\x.exe".into(),
            args: vec!["\0".into()],
        };
        prepare(&config).err().unwrap();
        config.args = vec!["x".repeat(32_750)];
        let prepared = prepare(&config).unwrap();
        assert_eq!(prepared.command.len(), 32_764);
        config.args = vec!["x".repeat(32_754)];
        prepare(&config).err().unwrap();
    }
}
