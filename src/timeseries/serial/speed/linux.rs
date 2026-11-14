//! Linux64 generic `termios2` ABI, pinned to libc 0.2.190 and Linux v6.12.
//! Never cast libc `termios` (32 cc slots) to kernel `termios2` (19 slots).
use std::{io, os::fd::RawFd};

pub(super) fn configure(fd: RawFd, settings: &libc::termios, baud: u32) -> io::Result<()> {
    configure_with(baud, Some(settings), || get(fd), |value| set(fd, value))
}

pub(super) fn change(fd: RawFd, baud: u32) -> io::Result<()> {
    configure_with(baud, None, || get(fd), |value| set(fd, value))
}

fn configure_with(
    baud: u32,
    settings: Option<&libc::termios>,
    mut get: impl FnMut() -> io::Result<libc::termios2>,
    set: impl FnOnce(&libc::termios2) -> io::Result<()>,
) -> io::Result<()> {
    let mut value = get()?;
    if let Some(settings) = settings {
        // Transfer individual flag fields, never incompatible structure bytes.
        value.c_iflag = settings.c_iflag;
        value.c_oflag = settings.c_oflag;
        value.c_cflag = settings.c_cflag;
        value.c_lflag = settings.c_lflag;
        value.c_cc[libc::VMIN] = 0;
        value.c_cc[libc::VTIME] = 0;
    }
    select(&mut value, baud);
    set(&value)?;
    check(&get()?, baud)
}

fn select(value: &mut libc::termios2, baud: u32) {
    // CIBAUD is independent of CBAUD; stale input selection must not survive.
    value.c_cflag &= !(libc::CBAUD | libc::CIBAUD);
    value.c_cflag |= libc::BOTHER | (libc::BOTHER << libc::IBSHIFT);
    value.c_ispeed = baud;
    value.c_ospeed = baud;
}

fn check(value: &libc::termios2, baud: u32) -> io::Result<()> {
    let output = rate(value.c_cflag & libc::CBAUD, value.c_ospeed)?;
    let input_code = (value.c_cflag & libc::CIBAUD) >> libc::IBSHIFT;
    let input = if input_code == libc::B0 {
        output // Input B0 means inherit the output rate.
    } else {
        rate(input_code, value.c_ispeed)?
    };
    if input != baud || output != baud {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "serial driver reports input/output {input}/{output} baud after requesting {baud}"
            ),
        ));
    }
    // This is driver-reported configuration, not physical clock verification.
    Ok(())
}

fn rate(code: libc::tcflag_t, numeric: libc::speed_t) -> io::Result<u32> {
    match code {
        libc::BOTHER => Ok(numeric),
        libc::B300 => Ok(300),
        libc::B600 => Ok(600),
        libc::B1200 => Ok(1200),
        libc::B2400 => Ok(2400),
        libc::B4800 => Ok(4800),
        libc::B9600 => Ok(9600),
        libc::B19200 => Ok(19_200),
        libc::B38400 => Ok(38_400),
        libc::B57600 => Ok(57_600),
        libc::B115200 => Ok(115_200),
        libc::B230400 => Ok(230_400),
        libc::B460800 => Ok(460_800),
        libc::B921600 => Ok(921_600),
        libc::B1500000 => Ok(1_500_000),
        libc::B2000000 => Ok(2_000_000),
        libc::B2500000 => Ok(2_500_000),
        libc::B3000000 => Ok(3_000_000),
        libc::B3500000 => Ok(3_500_000),
        libc::B4000000 => Ok(4_000_000),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("serial driver reports an unexpected speed encoding 0x{code:x}"),
        )),
    }
}

fn get(fd: RawFd) -> io::Result<libc::termios2> {
    let mut value = std::mem::MaybeUninit::<libc::termios2>::uninit();
    // SAFETY: this qualified target exposes the exact kernel termios2 ABI;
    // TCGETS2 writes this allocation on success, and fd is the caller's live fd.
    if unsafe { libc::ioctl(fd, libc::TCGETS2, value.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful TCGETS2 initialized the full value.
    Ok(unsafe { value.assume_init() })
}

fn set(fd: RawFd, value: &libc::termios2) -> io::Result<()> {
    // TCSETS2 is immediate and does not discard queued RX or drain TX.
    // SAFETY: fd is caller-owned, and value has the qualified kernel ABI.
    if unsafe { libc::ioctl(fd, libc::TCSETS2, value) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, mem::offset_of};

    fn settings() -> libc::termios2 {
        libc::termios2 {
            c_iflag: libc::IGNPAR,
            c_oflag: libc::OPOST,
            c_cflag: libc::CS7
                | libc::PARENB
                | libc::CSTOPB
                | libc::B9600
                | (libc::B115200 << libc::IBSHIFT),
            c_lflag: libc::ECHO,
            c_line: 7,
            c_cc: [31; 19],
            c_ispeed: 115_200,
            c_ospeed: 9600,
        }
    }

    #[test]
    fn qualified_kernel_abi_is_not_the_libc_termios_layout() {
        assert_eq!(size_of::<libc::termios2>(), 44);
        assert_eq!(align_of::<libc::termios2>(), 4);
        assert_eq!(offset_of!(libc::termios2, c_iflag), 0);
        assert_eq!(offset_of!(libc::termios2, c_oflag), 4);
        assert_eq!(offset_of!(libc::termios2, c_cflag), 8);
        assert_eq!(offset_of!(libc::termios2, c_lflag), 12);
        assert_eq!(offset_of!(libc::termios2, c_line), 16);
        assert_eq!(offset_of!(libc::termios2, c_cc), 17);
        assert_eq!(offset_of!(libc::termios2, c_ispeed), 36);
        assert_eq!(offset_of!(libc::termios2, c_ospeed), 40);
        #[cfg(target_env = "gnu")]
        let get_request = libc::c_ulong::from(0x802c_542a_u32);
        #[cfg(target_env = "musl")]
        let get_request = libc::c_int::from_ne_bytes(0x802c_542a_u32.to_ne_bytes());
        assert_eq!(libc::TCGETS2, get_request);
        assert_eq!(libc::TCSETS2, 0x402c_542b);
        assert_eq!(libc::CBAUD, 0x100f);
        assert_eq!(libc::CIBAUD, 0x100f_0000);
        assert_eq!(libc::BOTHER, 0x1000);
        assert_eq!(libc::IBSHIFT, 16);
        assert_eq!(libc::NCCS, 32);
    }

    #[test]
    fn both_speed_masks_change_without_modifying_other_flags_or_cc() {
        for baud in [
            230_400, 460_800, 691_200, 921_600, 1_500_000, 2_000_000, 2_500_000, 3_000_000,
            3_500_000, 4_000_000,
        ] {
            let mut value = settings();
            let original = settings();
            select(&mut value, baud);
            assert_eq!(value.c_cflag & libc::CBAUD, libc::BOTHER);
            assert_eq!(value.c_cflag & libc::CIBAUD, libc::BOTHER << libc::IBSHIFT);
            assert_eq!(
                value.c_cflag & !(libc::CBAUD | libc::CIBAUD),
                original.c_cflag & !(libc::CBAUD | libc::CIBAUD)
            );
            assert_eq!(value.c_ispeed, baud);
            assert_eq!(value.c_ospeed, baud);
            assert_eq!(value.c_iflag, original.c_iflag);
            assert_eq!(value.c_oflag, original.c_oflag);
            assert_eq!(value.c_lflag, original.c_lflag);
            assert_eq!(value.c_cc, original.c_cc);
            assert_eq!(value.c_line, original.c_line);
            check(&value, baud).unwrap();
        }
    }

    #[test]
    fn readback_accepts_effective_input_inheritance_and_rejects_substitution() {
        let mut value = settings();
        select(&mut value, 691_200);
        value.c_cflag &= !libc::CIBAUD;
        value.c_ispeed = 0;
        check(&value, 691_200).unwrap();
        value.c_ospeed = 9600;
        assert_eq!(
            check(&value, 691_200).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        for (code, baud) in [
            (libc::B230400, 230_400),
            (libc::B460800, 460_800),
            (libc::B921600, 921_600),
            (libc::B1500000, 1_500_000),
            (libc::B2000000, 2_000_000),
            (libc::B2500000, 2_500_000),
            (libc::B3000000, 3_000_000),
            (libc::B3500000, 3_500_000),
            (libc::B4000000, 4_000_000),
        ] {
            value.c_cflag = code | (code << libc::IBSHIFT);
            check(&value, baud).unwrap();
        }
        value.c_cflag = libc::B230400 | (libc::B230400 << libc::IBSHIFT);
        check(&value, 230_400).unwrap();
        value.c_cflag |= libc::CBAUD;
        assert_eq!(
            check(&value, 230_400).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        select(&mut value, 691_200);
        value.c_ispeed = 460_800;
        assert_eq!(
            check(&value, 691_200).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn setup_uses_one_kernel_write_and_preserves_unrelated_cc_slots() {
        // SAFETY: zero is a valid representation for the termios integer fields.
        let mut raw = unsafe { std::mem::MaybeUninit::<libc::termios>::zeroed().assume_init() };
        raw.c_iflag = libc::IGNBRK;
        raw.c_oflag = 0;
        raw.c_cflag = libc::CLOCAL | libc::CREAD | libc::CS8;
        raw.c_lflag = 0;
        let reads = Cell::new(0);
        let writes = Cell::new(0);
        configure_with(
            691_200,
            Some(&raw),
            || {
                reads.set(reads.get() + 1);
                let mut value = settings();
                if reads.get() == 2 {
                    select(&mut value, 691_200);
                }
                Ok(value)
            },
            |value| {
                writes.set(writes.get() + 1);
                assert_eq!(value.c_iflag, raw.c_iflag);
                assert_eq!(value.c_oflag, raw.c_oflag);
                assert_eq!(value.c_lflag, raw.c_lflag);
                assert_eq!(value.c_cflag & !(libc::CBAUD | libc::CIBAUD), raw.c_cflag);
                assert_eq!(value.c_line, 7);
                for (index, &byte) in value.c_cc.iter().enumerate() {
                    assert_eq!(
                        byte,
                        if matches!(index, libc::VMIN | libc::VTIME) {
                            0
                        } else {
                            31
                        }
                    );
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(reads.get(), 2);
        assert_eq!(writes.get(), 1);
    }

    #[test]
    fn get_set_and_readback_failures_never_fall_back_or_retry() {
        let reads = Cell::new(0);
        let writes = Cell::new(0);
        let error = configure_with(
            691_200,
            None,
            || {
                reads.set(reads.get() + 1);
                Err(io::Error::new(io::ErrorKind::Unsupported, "no TCGETS2"))
            },
            |_| {
                writes.set(writes.get() + 1);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert_eq!((reads.get(), writes.get()), (1, 0));
        reads.set(0);
        let error = configure_with(
            691_200,
            None,
            || {
                reads.set(reads.get() + 1);
                Ok(settings())
            },
            |_| {
                writes.set(writes.get() + 1);
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "driver refused",
                ))
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!((reads.get(), writes.get()), (1, 1));
        reads.set(0);
        writes.set(0);
        let error = configure_with(
            691_200,
            None,
            || {
                reads.set(reads.get() + 1);
                if reads.get() == 2 {
                    Err(io::Error::new(io::ErrorKind::BrokenPipe, "lost device"))
                } else {
                    Ok(settings())
                }
            },
            |_| {
                writes.set(writes.get() + 1);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!((reads.get(), writes.get()), (2, 1));
    }

    #[test]
    fn native_pty_keeps_queued_bytes_across_extended_rate_changes() {
        use crate::timeseries::serial::{self, Parity};
        use std::{
            ffi::CStr,
            fs::File,
            io::{Read as _, Write as _},
            os::fd::{AsRawFd as _, FromRawFd as _},
            time::{Duration, Instant},
        };
        let (mut master, mut slave) = (-1, -1);
        let mut name = [0; 128];
        // SAFETY: output pointers/name storage are valid; null options select defaults.
        let status = unsafe {
            libc::openpty(
                &raw mut master,
                &raw mut slave,
                name.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(status, 0);
        // SAFETY: successful openpty transferred a new owned master descriptor.
        let mut master = unsafe { File::from_raw_fd(master) };
        // SAFETY: successful openpty transferred the other owned descriptor.
        let slave = unsafe { File::from_raw_fd(slave) };
        for file in [&master, &slave] {
            // SAFETY: the fixture owns this live descriptor; query its descriptor flags.
            let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
            assert!(flags >= 0);
            // SAFETY: preserve flags and avoid leaking fixture handles into other tests' children.
            let status =
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) };
            assert_eq!(status, 0);
        }
        // SAFETY: openpty returned a NUL-terminated pathname in name.
        let name = unsafe { CStr::from_ptr(name.as_ptr()) }.to_str().unwrap();
        // SAFETY: master is live; read its existing status flags.
        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        // SAFETY: preserve flags while making fixture reads bounded/nonblocking.
        let status =
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
        assert_eq!(status, 0);
        for baud in [
            230_400, 460_800, 691_200, 921_600, 1_500_000, 2_000_000, 2_500_000, 3_000_000,
            3_500_000, 4_000_000,
        ] {
            let mut port = serial::open(name, baud, Parity::None, 1, 8).unwrap();
            let before = get(slave.as_raw_fd()).unwrap();
            check(&before, baud).unwrap();
            assert_eq!(
                before.c_cflag
                    & (libc::CSIZE | libc::PARENB | libc::CSTOPB | libc::CRTSCTS | libc::CMSPAR),
                libc::CS8
            );
            assert_eq!(before.c_iflag & (libc::IXON | libc::IXOFF | libc::IXANY), 0);
            assert_eq!(before.c_lflag & (libc::ICANON | libc::ECHO), 0);
            let queued = [0, 0x11, 0x13, 0xff];
            master.write_all(&queued).unwrap();
            port.set_baud(691_200).unwrap();
            let after = get(slave.as_raw_fd()).unwrap();
            check(&after, 691_200).unwrap();
            assert_eq!(before.c_iflag, after.c_iflag);
            assert_eq!(before.c_oflag, after.c_oflag);
            assert_eq!(before.c_lflag, after.c_lflag);
            assert_eq!(
                before.c_cflag & !(libc::CBAUD | libc::CIBAUD),
                after.c_cflag & !(libc::CBAUD | libc::CIBAUD)
            );
            assert_eq!(before.c_cc, after.c_cc);
            let mut received = [0; 4];
            port.read_exact(&mut received, Instant::now() + Duration::from_secs(1))
                .unwrap();
            assert_eq!(received, queued);
            master.write_all(&queued).unwrap();
            port.set_baud(115_200).unwrap();
            let traditional = serial::unix_settings(slave.as_raw_fd()).unwrap();
            // SAFETY: traditional is an initialized, readable termios value.
            let input = unsafe { libc::cfgetispeed(&raw const traditional) };
            // SAFETY: traditional remains initialized/readable for this getter.
            let output = unsafe { libc::cfgetospeed(&raw const traditional) };
            assert_eq!((input, output), (libc::B115200, libc::B115200));
            let traditional_kernel = get(slave.as_raw_fd()).unwrap();
            check(&traditional_kernel, 115_200).unwrap();
            assert_eq!(
                (traditional_kernel.c_ispeed, traditional_kernel.c_ospeed),
                (115_200, 115_200)
            );
            assert_eq!(traditional.c_iflag, before.c_iflag);
            assert_eq!(traditional.c_oflag, before.c_oflag);
            assert_eq!(traditional.c_lflag, before.c_lflag);
            assert_eq!(
                traditional.c_cflag & !(libc::CBAUD | libc::CIBAUD),
                before.c_cflag & !(libc::CBAUD | libc::CIBAUD)
            );
            port.read_exact(&mut received, Instant::now() + Duration::from_secs(1))
                .unwrap();
            assert_eq!(received, queued);
            port.set_baud(691_200).unwrap();
            master.write_all(&queued).unwrap();
            drop(port);
            let mut reopened = serial::open(name, 115_200, Parity::None, 1, 8).unwrap();
            let reopened_kernel = get(slave.as_raw_fd()).unwrap();
            check(&reopened_kernel, 115_200).unwrap();
            assert_eq!(
                (reopened_kernel.c_ispeed, reopened_kernel.c_ospeed),
                (115_200, 115_200)
            );
            reopened
                .read_exact(&mut received, Instant::now() + Duration::from_secs(1))
                .unwrap();
            assert_eq!(received, queued);
            assert_eq!(
                master.read(&mut [0]).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        }
        // This proves kernel PTY configuration/queue behavior, not UART hardware.
    }
}
