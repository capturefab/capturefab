//! Passive, externally configured ILD2300 single-DIST1 RS422 output.
//! The optoNCDT2300 manual §§8.3/9.1 documents three-byte tagged words.
//! Exactly one output value avoids the manuals' different first/last marker
//! descriptions. No command, scale, sensor timestamp or freshness is inferred.
use super::{Reading, Source, serial};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use std::{
    io,
    time::{Duration, Instant},
};

mod parser;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    profile: Profile,
    device: String,
    #[serde(default = "baud", deserialize_with = "crate::config_number::unsigned")]
    baud: u32,
    #[serde(default = "signal")]
    signal: String,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Profile {
    Ild2300Dist1,
}

impl Profile {
    const fn name(self) -> &'static str {
        match self {
            Self::Ild2300Dist1 => "ild2300_dist1",
        }
    }
}

const fn baud() -> u32 {
    115_200
}
fn signal() -> String {
    "distance1".into()
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    super::validate_name(&config.signal, "Micro-Epsilon signal")?;
    if config.device.is_empty()
        || config.device.as_bytes().contains(&0)
        || !matches!(
            config.baud,
            9600 | 115_200
                | 230_400
                | 460_800
                | 691_200
                | 921_600
                | 1_500_000
                | 2_000_000
                | 2_500_000
                | 3_000_000
                | 3_500_000
                | 4_000_000
        )
    {
        return Err(Usage(
            "Micro-Epsilon ild2300_dist1 requires a nonempty serial device and externally configured 9600, 115200, 230400, 460800, 691200, 921600, 1500000, 2000000, 2500000, 3000000, 3500000 or 4000000 baud, 8N1, DIST1 only; no other RS422 values or commands"
                .into(),
        )
        .into());
    }
    serial::validate(config.baud, 1, 8)?;
    Ok(Box::new(Client {
        config: config.clone(),
        #[cfg(any(unix, windows))]
        port: None,
        framing: parser::Framing::default(),
        observed: None,
        cutoff: None,
        seen: false,
        retired: false,
    }))
}

struct Client {
    config: Config,
    #[cfg(any(unix, windows))]
    port: Option<serial::Port>,
    framing: parser::Framing,
    observed: Option<u64>,
    cutoff: Option<u64>,
    seen: bool,
    retired: bool,
}

impl Client {
    fn next(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        #[cfg(any(unix, windows))]
        {
            if self.port.is_none() {
                remaining(deadline)?;
                // Configuration and receive share the caller's original budget.
                // No purge, write, sensor command or modem-keying step is added.
                self.port = Some(serial::open(
                    &self.config.device,
                    self.config.baud,
                    serial::Parity::None,
                    1,
                    8,
                )?);
                remaining(deadline)?;
            }
            let port = self.port.as_mut().expect("Micro-Epsilon port opened");
            let Some((word, observed)) =
                receive_word(&mut self.framing, deadline, self.cutoff, |buffer| {
                    port.receive(buffer, deadline)
                })?
            else {
                return Ok(Vec::new());
            };
            let value = word.value(self.config.profile.name());
            remaining(deadline)?;
            check_cutoff(observed, self.cutoff)?;
            self.observed = observed;
            self.seen = true;
            Ok(vec![Reading {
                signal: self.config.signal.clone(),
                value,
                source_time: None,
            }])
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = deadline;
            Err(Categorized::error(
                ErrorKind::Unsupported,
                "Micro-Epsilon RS422 requires a Unix terminal or Windows COM port",
            ))
        }
    }

    fn retire(&mut self) {
        self.retired = true;
        #[cfg(any(unix, windows))]
        {
            self.port = None;
        }
    }
}

impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        if self.retired {
            return Err(Categorized::error(
                ErrorKind::Disconnected,
                "Micro-Epsilon RS422 channel retired; a fresh source must be explicitly opened",
            ));
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("Micro-Epsilon timeout is too large".into()))?;
        if timeout.is_zero() {
            return Ok(Vec::new());
        }
        let result = self.next(deadline);
        if result.is_err() {
            self.retire();
        }
        result
    }

    fn ready(&self) -> bool {
        self.seen
    }
    fn observed_ns(&self) -> Option<u64> {
        self.observed
    }
    fn set_acquisition_end(&mut self, end_ns: Option<u64>) {
        self.cutoff = end_ns;
    }
    fn finish(&mut self, _timeout: Duration) -> Result<()> {
        self.retire();
        Ok(())
    }
}

// Reads no bytes beyond one complete word. A later word remains in the kernel,
// so no unbounded pending observations or reassigned receive timestamps exist.
fn receive_word(
    framing: &mut parser::Framing,
    deadline: Instant,
    cutoff: Option<u64>,
    mut receive: impl FnMut(&mut [u8]) -> io::Result<Option<usize>>,
) -> Result<Option<(parser::Word, Option<u64>)>> {
    loop {
        if Instant::now() >= deadline {
            return idle(framing);
        }
        let mut buffer = [0; 3];
        let buffer = buffer
            .get_mut(..framing.wanted())
            .ok_or_else(|| malformed("invalid receive extent"))?;
        let count = match receive(buffer) {
            Ok(Some(0)) => {
                return Err(Categorized::error(
                    ErrorKind::Disconnected,
                    "Micro-Epsilon RS422 stream closed",
                ));
            }
            Ok(Some(count)) => count,
            Ok(None) => return idle(framing),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                return idle(framing);
            }
            Err(error) => return Err(io_error(&error)),
        };
        let observed = crate::host::monotonic_ns();
        remaining(deadline)?;
        check_cutoff(observed, cutoff)?;
        let bytes = buffer
            .get(..count)
            .ok_or_else(|| malformed("serial read exceeded requested extent"))?;
        for &byte in bytes {
            if let Some(word) = framing.push(byte)? {
                remaining(deadline)?;
                return Ok(Some((word, observed)));
            }
        }
    }
}

fn idle(framing: &parser::Framing) -> Result<Option<(parser::Word, Option<u64>)>> {
    if framing.partial() {
        return Err(timeout());
    }
    Ok(None)
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(timeout());
    }
    Ok(left)
}
fn check_cutoff(observed: Option<u64>, cutoff: Option<u64>) -> Result<()> {
    if let (Some(observed), Some(cutoff)) = (observed, cutoff)
        && observed > cutoff
    {
        return Err(timeout());
    }
    Ok(())
}
fn timeout() -> anyhow::Error {
    Categorized::error(
        ErrorKind::Timeout,
        "Micro-Epsilon RS422 acquisition deadline expired",
    )
}
fn malformed(message: &str) -> anyhow::Error {
    Categorized::error(
        ErrorKind::OperationFailed,
        format!("Micro-Epsilon single-DIST1 RS422: {message}"),
    )
}
fn io_error(error: &io::Error) -> anyhow::Error {
    let kind = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        io::ErrorKind::InvalidData => ErrorKind::OperationFailed,
        _ => ErrorKind::Disconnected,
    };
    Categorized::error(kind, format!("Micro-Epsilon RS422 I/O: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::classify;
    use std::io::{Cursor, Read as _};

    #[test]
    fn idle_and_incomplete_words_are_distinct_without_extending_the_deadline() {
        let mut framing = parser::Framing::default();
        let past = Instant::now();
        assert!(
            receive_word(&mut framing, past, None, |_| panic!("expired read"))
                .unwrap()
                .is_none()
        );
        let _ = framing.push(0).unwrap();
        let error = receive_word(&mut framing, past, None, |_| panic!("expired read"))
            .err()
            .unwrap();
        assert_eq!(classify(&error).kind, ErrorKind::Timeout);
        let error = receive_word(
            &mut framing,
            Instant::now() + Duration::from_secs(1),
            None,
            |_| Ok(None),
        )
        .err()
        .unwrap();
        assert_eq!(classify(&error).kind, ErrorKind::Timeout);
    }

    #[test]
    fn fixed_read_extent_preserves_the_next_word_and_last_receive_clock() {
        let mut input = Cursor::new([0x7e, 0xbf, 0x38, 0x7f, 0x87, 0, 0x40, 0x80]);
        let mut framing = parser::Framing::default();
        let (word, observed) = receive_word(
            &mut framing,
            Instant::now() + Duration::from_secs(1),
            None,
            |buffer| input.read(buffer).map(Some),
        )
        .unwrap()
        .unwrap();
        assert_eq!(input.position(), 5);
        assert_eq!(word.raw, [0x38, 0x7f, 0x87]);
        assert_eq!(word.alignment(), &[0x7e, 0xbf]);
        assert!(observed.is_some());
        let (word, _) = receive_word(
            &mut framing,
            Instant::now() + Duration::from_secs(1),
            None,
            |buffer| input.read(buffer).map(Some),
        )
        .unwrap()
        .unwrap();
        assert_eq!(word.raw, [0, 0x40, 0x80]);
        assert_eq!(word.alignment(), &[] as &[u8]);
    }

    #[test]
    fn cutoff_and_io_errors_remain_strict() {
        let mut framing = parser::Framing::default();
        let error = receive_word(
            &mut framing,
            Instant::now() + Duration::from_secs(1),
            Some(0),
            |buffer| {
                *buffer
                    .first_mut()
                    .ok_or_else(|| io::Error::other("empty fixture buffer"))? = 0;
                Ok(Some(1))
            },
        )
        .err()
        .unwrap();
        assert_eq!(classify(&error).kind, ErrorKind::Timeout);
        let error = receive_word(
            &mut framing,
            Instant::now() + Duration::from_secs(1),
            None,
            |_| Ok(Some(0)),
        )
        .err()
        .unwrap();
        assert_eq!(classify(&error).kind, ErrorKind::Disconnected);
    }

    #[test]
    fn config_checks_precede_lazy_open_and_failed_open_is_terminal() {
        let mut config: Config = serde_json::from_value(serde_json::json!({
            "profile":"ild2300_dist1", "device":"/capturefab-does-not-exist"
        }))
        .unwrap();
        let mut source = open(&config).unwrap();
        assert_eq!(source.read(Duration::ZERO).unwrap().len(), 0);
        let _error = source.read(Duration::from_secs(1)).err().unwrap();
        let error = source.read(Duration::from_secs(1)).err().unwrap();
        assert_eq!(classify(&error).kind, ErrorKind::Disconnected);
        config.baud = 57_600;
        assert_eq!(
            classify(&open(&config).err().unwrap()).kind,
            ErrorKind::Usage
        );
        for baud in [
            230_400, 460_800, 691_200, 921_600, 1_500_000, 2_000_000, 2_500_000, 3_000_000,
            3_500_000, 4_000_000,
        ] {
            config.baud = baud;
            let result = open(&config);
            if cfg!(any(
                windows,
                all(
                    target_os = "linux",
                    target_pointer_width = "64",
                    any(target_arch = "x86_64", target_arch = "aarch64"),
                    any(target_env = "gnu", target_env = "musl")
                ),
                all(
                    target_os = "macos",
                    target_pointer_width = "64",
                    any(target_arch = "x86_64", target_arch = "aarch64")
                )
            )) {
                result.map(drop).unwrap(); // Validation only; opening remains lazy.
            } else {
                assert_eq!(
                    classify(&result.err().unwrap()).kind,
                    ErrorKind::Unsupported
                );
            }
        }
        config.baud = 9600;
        config.device.clear();
        assert_eq!(
            classify(&open(&config).err().unwrap()).kind,
            ErrorKind::Usage
        );
    }
}
