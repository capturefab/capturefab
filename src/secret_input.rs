//! Secrets piped to the CLI (`--secret-stdin`, `--password-stdin` and the
//! like), so they never appear in arguments, shell history or process lists.
use anyhow::{Result, ensure};
use std::io::Read;
use zeroize::Zeroizing;

/// Read a secret from stdin: at most 4096 bytes plus one terminal newline
/// (LF or CRLF), which is removed. Other bytes, including spaces, are kept.
/// The bytes are cleared from memory when the secret drops.
pub fn read_stdin_secret(reader: impl Read, what: &str) -> Result<Zeroizing<String>> {
    // Allow a maximum-size secret followed by CRLF, plus one byte to detect
    // an oversized input without reading an unbounded pipe into memory.
    // Reserving it all up front keeps the buffer from growing and leaving
    // copies of the secret behind.
    let mut data = Zeroizing::new(Vec::with_capacity(4099));
    reader.take(4099).read_to_end(&mut data)?;
    ensure!(
        data.len() <= 4098,
        "{what} stdin exceeds 4096 bytes plus a terminal newline"
    );
    if data.last() == Some(&b'\n') {
        data.pop();
        if data.last() == Some(&b'\r') {
            data.pop();
        }
    }
    let secret = String::from_utf8(std::mem::take(&mut *data)).map_err(|error| {
        let invalid = error.utf8_error();
        drop(Zeroizing::new(error.into_bytes()));
        anyhow::Error::new(invalid).context(format!("{what} stdin must be UTF-8"))
    })?;
    let secret = Zeroizing::new(secret);
    ensure!(
        !secret.is_empty() && secret.len() <= 4096,
        "{what} must contain 1..4096 bytes"
    );
    Ok(secret)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_secrets_keep_bytes_and_bound_reads() {
        for (input, expected) in [
            (" key \t", " key \t"),
            (" key \t\r\n", " key \t"),
            ("clé \n", "clé "),
            ("key\n\n", "key\n"),
        ] {
            assert_eq!(
                read_stdin_secret(input.as_bytes(), "x").unwrap().as_str(),
                expected
            );
        }
        assert!(read_stdin_secret("\n".as_bytes(), "x").is_err());
        assert!(read_stdin_secret("x".repeat(5000).as_bytes(), "x").is_err());
        let invalid = read_stdin_secret(&[0xff, 0xfe][..], "x").unwrap_err();
        assert!(
            format!("{invalid:#}").starts_with("x stdin must be UTF-8: invalid utf-8"),
            "{invalid:#}"
        );
    }
}
