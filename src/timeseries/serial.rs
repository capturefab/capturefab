//! Native serial configuration and bounded I/O for Unix terminals and Windows COM ports.
//! Protocol adapters own framing and decide when stale input may be discarded.
use crate::types::Usage;
use anyhow::Result;
use serde::{
    Deserialize, Deserializer,
    de::{DeserializeOwned, Error as _},
};
#[cfg(any(unix, windows))]
use std::{
    fs::File,
    io::{self, Read as _, Write as _},
    time::Instant,
};

mod speed;

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Parity {
    None,
    #[default]
    Even,
    Odd,
}

/// A protocol signal with an optional device unit. Modbus TCP also uses this
/// wrapper for gateway routing. The signal still checks its protocol fields.
#[derive(Debug, Clone)]
pub(crate) struct SerialSignal<T> {
    pub unit: Option<u8>,
    pub signal: T,
}

impl<T> SerialSignal<T> {
    pub(crate) fn unit(&self, default: u8) -> u8 {
        self.unit.unwrap_or(default)
    }
}

impl<'de, T: DeserializeOwned> Deserialize<'de> for SerialSignal<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let mut value = crate::session::strict_json_value(deserializer)?;
        let unit = value
            .as_object_mut()
            .ok_or_else(|| D::Error::custom("signal must be an object"))?
            .remove("unit")
            .map(crate::config_number::unsigned)
            .transpose()
            .map_err(|error| D::Error::custom(format!("signal unit: {error}")))?;
        let signal = serde_json::from_value(value).map_err(D::Error::custom)?;
        Ok(Self { unit, signal })
    }
}

/// SDI-12 and Vaisala ASCII use the same reflected CRC and wire encoding.
#[cfg(any(unix, windows))]
pub(super) fn crc16_ascii(bytes: &[u8]) -> [u8; 3] {
    let mut crc = 0_u16;
    for &byte in bytes {
        crc ^= u16::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 == 0 { 0 } else { 0xa001 };
        }
    }
    let [low, high] = crc.to_le_bytes();
    [
        0x40 | (high >> 4),
        0x40 | ((high & 15) << 2) | (low >> 6),
        0x40 | (low & 63),
    ]
}

pub(super) fn validate(baud: u32, stop_bits: u8, data_bits: u8) -> Result<()> {
    if !matches!(
        baud,
        300 | 600
            | 1200
            | 2400
            | 4800
            | 9600
            | 19_200
            | 38_400
            | 57_600
            | 115_200
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
    ) {
        return Err(Usage(
            "serial baud must be 300, 600, 1200, 2400, 4800, 9600, 19200, 38400, 57600, 115200, 230400, 460800, 691200, 921600, 1500000, 2000000, 2500000, 3000000, 3500000 or 4000000"
                .into(),
        )
        .into());
    }
    if !matches!(stop_bits, 1 | 2) || !(5..=8).contains(&data_bits) {
        return Err(
            Usage("serial stop_bits must be 1 or 2 and data_bits must be 5..8".into()).into(),
        );
    }
    speed::validate(baud)
}

#[cfg(unix)]
fn open_unix(
    device: &str,
    baud: u32,
    parity: Parity,
    stop_bits: u8,
    data_bits: u8,
) -> Result<File> {
    use crate::types::{Categorized, ErrorKind};
    use std::{
        fs::OpenOptions,
        os::{fd::AsRawFd as _, unix::fs::OpenOptionsExt as _},
    };
    validate(baud, stop_bits, data_bits)?;
    let io_error = |error: io::Error| {
        let kind = match error.kind() {
            io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
            _ => ErrorKind::Unavailable,
        };
        Categorized::error(kind, format!("serial configuration: {error}"))
    };
    // O_RDWR is needed to set termios consistently and share this descriptor
    // with read/write Modbus. A sensor adapter never writes payload bytes.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
        .open(device)
        .map_err(io_error)?;
    fs2::FileExt::try_lock_exclusive(&file).map_err(io_error)?;
    let fd = file.as_raw_fd();
    let mut settings = unix_settings(fd).map_err(io_error)?;
    // SAFETY: settings is initialized and exclusively borrowed.
    unsafe { libc::cfmakeraw(&raw mut settings) };
    // cfmakeraw need not clear inherited IXOFF, which could make the kernel
    // transmit software flow-control bytes on a receive-only sensor stream.
    settings.c_iflag &= !(libc::IXON | libc::IXOFF | libc::IXANY);
    settings.c_cflag |= libc::CLOCAL | libc::CREAD;
    settings.c_cflag &= !(libc::CSIZE | libc::PARENB | libc::PARODD | libc::CSTOPB);
    settings.c_cflag |= match data_bits {
        5 => libc::CS5,
        6 => libc::CS6,
        7 => libc::CS7,
        8 => libc::CS8,
        _ => return Err(Usage("unsupported serial data_bits".into()).into()),
    };
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd"
    ))]
    {
        settings.c_cflag &= !libc::CRTSCTS;
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // A previous mark/space-parity user must not change even/odd parity.
        settings.c_cflag &= !libc::CMSPAR;
    }
    match parity {
        Parity::None => {}
        Parity::Even => settings.c_cflag |= libc::PARENB,
        Parity::Odd => settings.c_cflag |= libc::PARENB | libc::PARODD,
    }
    if stop_bits == 2 {
        settings.c_cflag |= libc::CSTOPB;
    }
    settings.c_cc[libc::VMIN] = 0;
    settings.c_cc[libc::VTIME] = 0;
    set_unix_baud(fd, &mut settings, baud).map_err(io_error)?;
    Ok(file)
}

#[cfg(unix)]
fn unix_settings(fd: std::os::fd::RawFd) -> io::Result<libc::termios> {
    let mut settings = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: tcgetattr initializes this allocation on success and rejects non-terminals.
    if unsafe { libc::tcgetattr(fd, settings.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful tcgetattr initialized settings.
    Ok(unsafe { settings.assume_init() })
}

#[cfg(unix)]
fn set_unix_baud(
    fd: std::os::fd::RawFd,
    settings: &mut libc::termios,
    baud: u32,
) -> io::Result<()> {
    if speed::extended(baud) {
        return speed::configure(fd, settings, baud);
    }
    let speed = match baud {
        300 => libc::B300,
        600 => libc::B600,
        1200 => libc::B1200,
        2400 => libc::B2400,
        4800 => libc::B4800,
        9600 => libc::B9600,
        19_200 => libc::B19200,
        38_400 => libc::B38400,
        57_600 => libc::B57600,
        115_200 => libc::B115200,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported serial baud rate",
            ));
        }
    };
    // SAFETY: settings is initialized and exclusively borrowed.
    if unsafe { libc::cfsetispeed(settings, speed) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: settings is initialized and exclusively borrowed.
    if unsafe { libc::cfsetospeed(settings, speed) } != 0 {
        return Err(io::Error::last_os_error());
    }
    #[cfg(all(
        target_os = "linux",
        target_pointer_width = "64",
        any(target_arch = "x86_64", target_arch = "aarch64"),
        any(target_env = "gnu", target_env = "musl")
    ))]
    {
        // Older glibc setters leave CIBAUD untouched. Clear an inherited
        // independent/BOTHER input rate so it follows the requested output,
        // including a traditional reopen after another extended-rate owner.
        settings.c_cflag &= !libc::CIBAUD;
    }
    // TCSANOW neither waits indefinitely for TX nor discards already received input.
    // SAFETY: fd is live and settings points to an initialized termios value.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, settings) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// An exclusively owned terminal/COM handle. Calls share one caller deadline;
/// an idle receive returns None, whereas a closed Unix terminal returns Some(0).
#[cfg(any(unix, windows))]
pub(super) struct Port {
    file: File,
}

#[cfg(any(unix, windows))]
pub(super) fn open(
    device: &str,
    baud: u32,
    parity: Parity,
    stop_bits: u8,
    data_bits: u8,
) -> Result<Port> {
    #[cfg(unix)]
    let file = open_unix(device, baud, parity, stop_bits, data_bits)?;
    #[cfg(windows)]
    let file = windows::open(device, baud, parity, stop_bits, data_bits)?;
    Ok(Port { file })
}

#[cfg(any(unix, windows))]
fn timeout() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "serial operation deadline expired")
}

#[cfg(any(unix, windows))]
impl Port {
    pub(super) fn receive(
        &mut self,
        bytes: &mut [u8],
        deadline: Instant,
    ) -> io::Result<Option<usize>> {
        if bytes.is_empty() {
            return Ok(Some(0));
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            loop {
                if !wait(self.file.as_raw_fd(), libc::POLLIN, deadline)? {
                    return Ok(None);
                }
                match self.file.read(bytes) {
                    Ok(count) => return Ok(Some(count)),
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        #[cfg(windows)]
        {
            windows::check_errors(&self.file)?;
            windows::set_timeouts(&self.file, deadline)?;
            match self.file.read(bytes) {
                Ok(0) => Ok(None), // Windows communications read timeout, not EOF.
                Ok(count) => {
                    windows::check_errors(&self.file)?;
                    Ok(Some(count))
                }
                Err(error) => Err(error),
            }
        }
    }

    pub(super) fn read_exact(&mut self, mut bytes: &mut [u8], deadline: Instant) -> io::Result<()> {
        while !bytes.is_empty() {
            match self.receive(bytes, deadline)? {
                None => return Err(timeout()),
                Some(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "serial device disconnected",
                    ));
                }
                Some(count) => bytes = &mut bytes[count..],
            }
        }
        Ok(())
    }

    pub(super) fn write_all(&mut self, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
        while !bytes.is_empty() {
            #[cfg(unix)]
            {
                use std::os::fd::AsRawFd as _;
                if !wait(self.file.as_raw_fd(), libc::POLLOUT, deadline)? {
                    return Err(timeout());
                }
            }
            #[cfg(windows)]
            {
                windows::check_errors(&self.file)?;
                windows::set_timeouts(&self.file, deadline)?;
            }
            match self.file.write(bytes) {
                Ok(0) => return Err(timeout()),
                Ok(count) => bytes = &bytes[count..],
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        #[cfg(windows)]
        windows::check_errors(&self.file)?;
        Ok(())
    }

    /// Change only the speed on the existing exclusive handle. The caller
    /// owns protocol timing and must ensure its earlier transmission is out.
    pub(super) fn set_baud(&self, baud: u32) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            let fd = self.file.as_raw_fd();
            if speed::extended(baud) {
                speed::change(fd, baud)
            } else {
                set_unix_baud(fd, &mut unix_settings(fd)?, baud)
            }
        }
        #[cfg(windows)]
        {
            windows::set_baud(&self.file, baud)
        }
    }

    /// Check the driver's transmit queue without a blocking drain. It can
    /// empty before the UART's final stop bit, so callers also allow wire time.
    pub(super) fn output_empty(&self) -> io::Result<bool> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            // These BSD targets do not export TIOCOUTQ in libc.
            // Their sys/ttycom.h defines it as _IOR('t', 115, int).
            #[cfg(any(target_os = "netbsd", target_os = "openbsd"))]
            let request: libc::c_ulong = 0x4004_7473;
            #[cfg(not(any(target_os = "netbsd", target_os = "openbsd")))]
            let request = libc::TIOCOUTQ;
            let mut pending: libc::c_int = 0;
            // SAFETY: the port owns a live terminal fd and pending is writable.
            if unsafe { libc::ioctl(self.file.as_raw_fd(), request, &raw mut pending) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(pending == 0)
        }
        #[cfg(windows)]
        {
            windows::output_empty(&self.file)
        }
    }

    /// Only request/response protocols call this to abandon stale framing.
    /// Never purge a receive-only stream when opening/configuring its port.
    pub(super) fn discard_input(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            // SAFETY: the port owns a live terminal file descriptor.
            if unsafe { libc::tcflush(self.file.as_raw_fd(), libc::TCIFLUSH) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        #[cfg(windows)]
        windows::discard_input(&self.file)?;
        Ok(())
    }
}

#[cfg(unix)]
fn wait(fd: std::os::fd::RawFd, events: libc::c_short, deadline: Instant) -> io::Result<bool> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        let millis = i32::try_from(left.as_millis().saturating_add(1)).unwrap_or(i32::MAX);
        let mut descriptor = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: descriptor points to one initialized pollfd and the caller owns fd.
        let status = unsafe { libc::poll(&raw mut descriptor, 1, millis) };
        if status < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if status == 0 {
            return Ok(false);
        }
        // A hung-up terminal may still hold complete buffered records.
        if descriptor.revents & events != 0 {
            return Ok(true);
        }
        if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "serial device disconnected",
            ));
        }
    }
}

#[cfg(any(windows, test))]
fn windows_device(device: &str) -> Result<String> {
    // The device namespace handles COM10 and higher without legacy DOS parsing.
    // Namespaced virtual COM names are accepted too; GetCommState subsequently
    // validates that the opened handle is a communications device.
    let name = device
        .strip_prefix(r"\\.\")
        .or_else(|| device.strip_prefix(r"\\?\"));
    if let Some(name) = name {
        if !name.is_empty() && !name.contains(['\\', '/', '\0', ':']) {
            return Ok(format!(r"\\.\{name}"));
        }
    } else if let Some(number) = device
        .get(..3)
        .filter(|prefix| prefix.eq_ignore_ascii_case("COM"))
        .and_then(|_| device.get(3..))
        && !number.is_empty()
        && number.bytes().all(|byte| byte.is_ascii_digit())
        && number.bytes().any(|byte| byte != b'0')
    {
        return Ok(format!(r"\\.\COM{number}"));
    }
    Err(Usage("Windows serial device must be COM followed by a nonzero number, or a namespaced COM device path".into()).into())
}

#[cfg(any(windows, test))]
fn windows_timeout_ms(deadline: Instant) -> io::Result<u32> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "serial operation deadline expired",
        ));
    }
    // MAXDWORD is a special sentinel in COMMTIMEOUTS. Keep a positive finite
    // constant and round up so a sub-millisecond budget cannot disable timeouts.
    Ok(u32::try_from(remaining.as_millis().saturating_add(1))
        .unwrap_or(u32::MAX - 1)
        .min(u32::MAX - 1))
}

#[cfg(windows)]
mod windows {
    use super::{
        File, Instant, Parity, Result, io, speed, validate, windows_device, windows_timeout_ms,
    };
    use crate::types::{Categorized, ErrorKind, Usage};
    use std::os::windows::{fs::OpenOptionsExt as _, io::AsRawHandle as _};
    use windows_sys::Win32::Devices::Communication::{
        CE_BREAK, CE_FRAME, CE_OVERRUN, CE_RXOVER, CE_RXPARITY, COMMTIMEOUTS, COMSTAT,
        ClearCommError, DCB, EVENPARITY, GetCommState, NOPARITY, ODDPARITY, ONESTOPBIT,
        PURGE_RXCLEAR, PurgeComm, SetCommState, SetCommTimeouts, TWOSTOPBITS,
    };

    pub(super) fn open(
        device: &str,
        baud: u32,
        parity: Parity,
        stop_bits: u8,
        data_bits: u8,
    ) -> Result<File> {
        validate(baud, stop_bits, data_bits)?;
        if data_bits == 5 && stop_bits == 2 {
            return Err(
                Usage("Windows serial cannot combine 5 data bits with 2 stop bits".into()).into(),
            );
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(windows_device(device)?)
            .map_err(|error| config_error(&error))?;
        let mut settings = DCB {
            DCBlength: u32::try_from(size_of::<DCB>())?,
            ..DCB::default()
        };
        // SAFETY: file is a live exclusive handle; settings is initialized and writable.
        if unsafe { GetCommState(file.as_raw_handle(), &raw mut settings) } == 0 {
            return Err(config_error(&io::Error::last_os_error()));
        }
        settings.BaudRate = baud;
        settings.ByteSize = data_bits;
        settings.Parity = match parity {
            Parity::None => NOPARITY,
            Parity::Even => EVENPARITY,
            Parity::Odd => ODDPARITY,
        };
        settings.StopBits = if stop_bits == 1 {
            ONESTOPBIT
        } else {
            TWOSTOPBITS
        };
        // fBinary; fParity when configured; stable enabled DTR/RTS; abort on
        // line errors. Disable CTS/DSR/XON/XOFF, NULL stripping, replacement,
        // DSR sensitivity and all inherited driver flow-control behavior.
        settings._bitfield = 1
            | (u32::from(!matches!(parity, Parity::None)) << 1)
            | (1 << 4)
            | (1 << 12)
            | (1 << 14);
        settings.XonChar = 0x11;
        settings.XoffChar = 0x13;
        settings.wReserved = 0;
        settings.wReserved1 = 0;
        // SAFETY: the live COM handle and initialized settings remain valid.
        if unsafe { SetCommState(file.as_raw_handle(), &raw const settings) } == 0 {
            return Err(config_error(&io::Error::last_os_error()));
        }
        if speed::extended(baud) {
            check_baud(&file, baud).map_err(|error| config_error(&error))?;
        }
        // Deliberately no SetupComm, PurgeComm or FlushFileBuffers here: setup
        // must retain queued sensor bytes, and flushing output has no deadline.
        Ok(file)
    }

    fn config_error(error: &io::Error) -> anyhow::Error {
        let kind = if error.kind() == io::ErrorKind::PermissionDenied {
            ErrorKind::PermissionDenied
        } else {
            ErrorKind::Unavailable
        };
        Categorized::error(kind, format!("serial configuration: {error}"))
    }

    pub(super) fn set_timeouts(file: &File, deadline: Instant) -> io::Result<()> {
        let millis = windows_timeout_ms(deadline)?;
        // Microsoft's first-byte timeout mode returns already-buffered bytes
        // immediately, or waits for one byte up to the remaining total budget.
        // https://learn.microsoft.com/windows/win32/api/winbase/ns-winbase-commtimeouts
        let settings = COMMTIMEOUTS {
            ReadIntervalTimeout: u32::MAX,
            ReadTotalTimeoutMultiplier: u32::MAX,
            ReadTotalTimeoutConstant: millis,
            WriteTotalTimeoutMultiplier: 0,
            WriteTotalTimeoutConstant: millis,
        };
        // SAFETY: the COM handle and settings are live for this synchronous call.
        if unsafe { SetCommTimeouts(file.as_raw_handle(), &raw const settings) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn set_baud(file: &File, baud: u32) -> io::Result<()> {
        let mut settings = DCB {
            DCBlength: u32::try_from(size_of::<DCB>()).map_err(io::Error::other)?,
            ..DCB::default()
        };
        // SAFETY: file is a live exclusive handle; settings is initialized and writable.
        if unsafe { GetCommState(file.as_raw_handle(), &raw mut settings) } == 0 {
            return Err(io::Error::last_os_error());
        }
        settings.BaudRate = baud;
        // SAFETY: the live COM handle and initialized settings remain valid.
        if unsafe { SetCommState(file.as_raw_handle(), &raw const settings) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if speed::extended(baud) {
            check_baud(file, baud)?;
        }
        Ok(())
    }

    fn check_baud(file: &File, baud: u32) -> io::Result<()> {
        let mut actual = DCB {
            DCBlength: u32::try_from(size_of::<DCB>()).map_err(io::Error::other)?,
            ..DCB::default()
        };
        // SAFETY: file owns the live COM handle and actual is writable.
        if unsafe { GetCommState(file.as_raw_handle(), &raw mut actual) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // This is driver configuration, not a measurement of physical UART timing.
        if actual.BaudRate != baud {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "serial driver reports {} baud after requesting {baud}",
                    actual.BaudRate
                ),
            ));
        }
        Ok(())
    }

    pub(super) fn output_empty(file: &File) -> io::Result<bool> {
        let mut status = COMSTAT::default();
        comm_errors(file, &raw mut status)?;
        Ok(status.cbOutQue == 0)
    }

    pub(super) fn check_errors(file: &File) -> io::Result<()> {
        comm_errors(file, std::ptr::null_mut())
    }

    fn comm_errors(file: &File, status: *mut COMSTAT) -> io::Result<()> {
        let mut errors = 0;
        // SAFETY: file is live, errors is writable, status is null or points to writable COMSTAT.
        if unsafe { ClearCommError(file.as_raw_handle(), &raw mut errors, status) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if errors & (CE_BREAK | CE_FRAME | CE_OVERRUN | CE_RXOVER | CE_RXPARITY) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("serial line error flags 0x{errors:x}"),
            ));
        }
        Ok(())
    }

    pub(super) fn discard_input(file: &File) -> io::Result<()> {
        // SAFETY: the exclusive COM handle is live and no operation is outstanding.
        if unsafe { PurgeComm(file.as_raw_handle(), PURGE_RXCLEAR) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{windows_device, windows_timeout_ms};
    use std::time::{Duration, Instant};

    #[test]
    fn windows_com_names_include_high_ports_and_explicit_virtual_devices() {
        for (name, expected) in [
            ("COM1", r"\\.\COM1"),
            ("com12", r"\\.\COM12"),
            (r"\\.\COM127", r"\\.\COM127"),
            (r"\\.\CNCA0", r"\\.\CNCA0"),
        ] {
            assert_eq!(windows_device(name).unwrap(), expected);
        }
        for name in [
            "",
            "COM0",
            "COM",
            "COM1.txt",
            "ordinary.txt",
            "C:\\file",
            r"\\.\",
            r"\\.\COM1\child",
        ] {
            windows_device(name).unwrap_err();
        }
    }

    #[test]
    fn windows_timeout_never_disables_deadlines_or_uses_special_sentinel() {
        windows_timeout_ms(Instant::now()).unwrap_err();
        assert!(
            (1..=2)
                .contains(&windows_timeout_ms(Instant::now() + Duration::from_millis(1)).unwrap())
        );
        assert_eq!(
            windows_timeout_ms(Instant::now() + Duration::from_secs(u64::from(u32::MAX))).unwrap(),
            u32::MAX - 1
        );
    }
}
