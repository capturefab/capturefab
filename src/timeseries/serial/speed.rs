//! Closed extended rates; traditional rates retain the original termios path.
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;

#[cfg(all(
    target_os = "linux",
    target_pointer_width = "64",
    any(target_arch = "x86_64", target_arch = "aarch64"),
    any(target_env = "gnu", target_env = "musl")
))]
mod linux;
#[cfg(all(
    target_os = "macos",
    target_pointer_width = "64",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod macos;

pub(super) const fn extended(baud: u32) -> bool {
    matches!(
        baud,
        230_400
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
}

const fn supported_target() -> bool {
    cfg!(any(
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
    ))
}

pub(super) fn validate(baud: u32) -> Result<()> {
    validate_target(baud, supported_target())
}

fn validate_target(baud: u32, supported: bool) -> Result<()> {
    if extended(baud) && !supported {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            "extended serial rates require qualified Linux64 x86_64/aarch64 GNU or musl, macOS64 x86_64/aarch64, or Windows; driver support is still required",
        ));
    }
    Ok(())
}

#[cfg(all(
    target_os = "linux",
    target_pointer_width = "64",
    any(target_arch = "x86_64", target_arch = "aarch64"),
    any(target_env = "gnu", target_env = "musl")
))]
use linux as platform;
#[cfg(all(
    target_os = "macos",
    target_pointer_width = "64",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
use macos as platform;

#[cfg(unix)]
pub(super) fn configure(
    fd: std::os::fd::RawFd,
    settings: &libc::termios,
    baud: u32,
) -> std::io::Result<()> {
    #[cfg(any(
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
    ))]
    {
        platform::configure(fd, settings, baud)
    }
    #[cfg(not(any(
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
    )))]
    {
        let _ = (fd, settings, baud);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "extended serial rates are not qualified on this Unix target",
        ))
    }
}

#[cfg(unix)]
pub(super) fn change(fd: std::os::fd::RawFd, baud: u32) -> std::io::Result<()> {
    #[cfg(any(
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
    ))]
    {
        platform::change(fd, baud)
    }
    #[cfg(not(any(
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
    )))]
    {
        let _ = (fd, baud);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "extended serial rates are not qualified on this Unix target",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::classify;

    #[test]
    fn closed_rates_and_target_rejection_preserve_traditional_validation() {
        for baud in [
            230_400, 460_800, 691_200, 921_600, 1_500_000, 2_000_000, 2_500_000, 3_000_000,
            3_500_000, 4_000_000,
        ] {
            assert!(extended(baud));
            validate_target(baud, true).unwrap();
            assert_eq!(
                classify(&validate_target(baud, false).unwrap_err()).kind,
                ErrorKind::Unsupported
            );
            if supported_target() {
                super::super::validate(baud, 1, 8).unwrap();
            } else {
                assert_eq!(
                    classify(&super::super::validate(baud, 1, 8).unwrap_err()).kind,
                    ErrorKind::Unsupported
                );
            }
        }
        for baud in [
            300, 600, 1200, 2400, 4800, 9600, 19_200, 38_400, 57_600, 115_200,
        ] {
            assert!(!extended(baud));
            validate_target(baud, false).unwrap();
            super::super::validate(baud, 2, 7).unwrap();
        }
        for baud in [0, 50, 500_000, 576_000, 1_000_000, u32::MAX] {
            assert_eq!(
                classify(&super::super::validate(baud, 1, 8).unwrap_err()).kind,
                ErrorKind::Usage
            );
        }
        for (stop, data) in [(0, 8), (3, 8), (1, 4), (1, 9)] {
            assert_eq!(
                classify(&super::super::validate(691_200, stop, data).unwrap_err()).kind,
                ErrorKind::Usage
            );
        }
    }
}
