//! Native Darwin64 `IOSSIOSPEED` from Apple's typed `_IOW('T', 2, speed_t)`.
//! Successful/cached configuration never proves the physical UART clock.
use std::{io, os::fd::RawFd};

// Darwin64 speed_t is unsigned long64. The 32-bit command is 0x8004_5402;
// Apple supports both, but this path deliberately uses the native typed form.
const IOSSIOSPEED: libc::c_ulong = 0x8008_5402;
const _: () = assert!(size_of::<libc::speed_t>() == 8);

pub(super) fn configure(fd: RawFd, settings: &libc::termios, baud: u32) -> io::Result<()> {
    let mut settings = *settings;
    configure_with(
        || super::super::set_unix_baud(fd, &mut settings, 115_200),
        || change(fd, baud),
    )
}

fn configure_with(
    base: impl FnOnce() -> io::Result<()>,
    custom: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    // Apply raw/parity/no-flow flags first. This traditional base is never a
    // success fallback: failure of the final custom request fails the open.
    base()?;
    custom()
}

pub(super) fn change(fd: RawFd, baud: u32) -> io::Result<()> {
    request(baud, |command, value| {
        // SAFETY: fd is caller-owned, command is native IOSSIOSPEED, and value
        // has the speed_t width required by that ioctl for this target.
        if unsafe { libc::ioctl(fd, command, &raw const value) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })
}

fn request(
    baud: u32,
    send: impl FnOnce(libc::c_ulong, libc::speed_t) -> io::Result<()>,
) -> io::Result<()> {
    send(IOSSIOSPEED, libc::speed_t::from(baud))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn native_request_has_a_speed_t_payload_and_no_rate_approximation() {
        assert_eq!(size_of::<libc::speed_t>(), 8);
        assert_eq!(
            IOSSIOSPEED,
            0x8000_0000 | (8 << 16) | (u64::from(b'T') << 8) | 2
        );
        for baud in [
            230_400, 460_800, 691_200, 921_600, 1_500_000, 2_000_000, 2_500_000, 3_000_000,
            3_500_000, 4_000_000,
        ] {
            request(baud, |command, value| {
                assert_eq!(command, IOSSIOSPEED);
                assert_eq!(value, libc::speed_t::from(baud));
                Ok(())
            })
            .unwrap();
        }
    }

    #[test]
    fn failed_base_or_custom_configuration_never_reports_a_base_rate_success() {
        let custom = Cell::new(0);
        let error = configure_with(
            || {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "base refused",
                ))
            },
            || {
                custom.set(custom.get() + 1);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(custom.get(), 0);
        let error = configure_with(
            || Ok(()),
            || {
                custom.set(custom.get() + 1);
                request(691_200, |_, _| {
                    Err(io::Error::new(io::ErrorKind::Unsupported, "no IOSSIOSPEED"))
                })
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert_eq!(custom.get(), 1);
    }
}
