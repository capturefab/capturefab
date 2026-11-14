//! Receive-only `SocketCAN` capture. Linux ABI and filter semantics:
//! <https://docs.kernel.org/networking/can.html>.
//! Socket receive timestamps are kernel `CLOCK_REALTIME`, not device timestamps.
#[cfg(any(target_os = "linux", test))]
use super::Reading;
use super::Source;
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
#[cfg(any(target_os = "linux", test))]
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[cfg(any(target_os = "linux", test))]
const EFF: u32 = 0x8000_0000;
#[cfg(any(target_os = "linux", test))]
const RTR: u32 = 0x4000_0000;
#[cfg(any(target_os = "linux", test))]
const ERR: u32 = 0x2000_0000;
const ID_MASK: u32 = 0x1fff_ffff;
const SFF_MASK: u32 = 0x7ff;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub interface: String,
    #[serde(default = "yes")]
    pub fd: bool,
    #[serde(default = "yes")]
    pub error_frames: bool,
    #[serde(default = "frame_signal")]
    pub signal: String,
    #[serde(default)]
    pub filters: Vec<Filter>,
    #[serde(default)]
    pub signals: Vec<Signal>,
}
const fn yes() -> bool {
    true
}
fn frame_signal() -> String {
    "frame".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Filter {
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub can_id: u32,
    #[serde(
        default = "id_mask",
        deserialize_with = "crate::config_number::unsigned"
    )]
    pub mask: u32,
    #[serde(default)]
    pub extended: Option<bool>,
}
const fn id_mask() -> u32 {
    ID_MASK
}
impl Filter {
    fn matches(&self, id: u32, extended: bool) -> bool {
        id & self.mask == self.can_id & self.mask
            && self.extended.is_none_or(|expected| expected == extended)
    }
    #[cfg(target_os = "linux")]
    fn kernel(&self) -> libc::can_filter {
        libc::can_filter {
            can_id: self.can_id | if self.extended == Some(true) { EFF } else { 0 },
            can_mask: self.mask | if self.extended.is_some() { EFF } else { 0 },
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Signal {
    pub name: String,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub can_id: u32,
    #[serde(default)]
    pub extended: bool,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub offset: u8,
    #[serde(default)]
    pub data_type: DataType,
    #[serde(default)]
    #[cfg_attr(
        not(any(target_os = "linux", test)),
        allow(dead_code, reason = "decoding is Linux-only")
    )]
    pub byte_order: ByteOrder,
    #[serde(default, deserialize_with = "crate::config_number::optional_unsigned")]
    pub bit: Option<u8>,
    #[serde(default = "one")]
    pub scale: f64,
    #[serde(default)]
    pub offset_value: f64,
}
const fn one() -> f64 {
    1.0
}
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ByteOrder {
    #[default]
    Little,
    Big,
}
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DataType {
    Bool,
    U8,
    I8,
    #[default]
    U16,
    I16,
    U32,
    I32,
    U64,
    I64,
    F32,
    F64,
}
impl DataType {
    const fn width(self) -> usize {
        match self {
            Self::Bool | Self::U8 | Self::I8 => 1,
            Self::U16 | Self::I16 => 2,
            Self::U32 | Self::I32 | Self::F32 => 4,
            Self::U64 | Self::I64 | Self::F64 => 8,
        }
    }
}

fn validate(config: &Config) -> Result<()> {
    if config.interface.is_empty()
        || config.interface.len() >= 16
        || config.interface.chars().any(|c| c.is_control() || c == '/')
    {
        return Err(
            Usage("CAN interface must be a local interface name of 1–15 bytes".into()).into(),
        );
    }
    super::validate_name(&config.signal, "CAN frame signal")?;
    if config.filters.len() > 256 || config.signals.len() > 256 {
        return Err(Usage("CAN capture accepts at most 256 filters and 256 signals".into()).into());
    }
    for filter in &config.filters {
        if filter.can_id > ID_MASK
            || filter.mask > ID_MASK
            || (filter.extended == Some(false) && filter.can_id > SFF_MASK)
        {
            return Err(Usage(
                "CAN filters need 29-bit can_id/mask; a standard can_id must fit 11 bits".into(),
            )
            .into());
        }
    }
    let mut names = BTreeSet::from([config.signal.as_str()]);
    for signal in &config.signals {
        super::validate_name(&signal.name, "CAN mapped signal")?;
        if !names.insert(&signal.name) {
            return Err(Usage(
                "CAN mapped names must be unique and differ from the raw frame signal".into(),
            )
            .into());
        }
        if signal.can_id > if signal.extended { ID_MASK } else { SFF_MASK } {
            return Err(
                Usage("CAN signal identifier exceeds its configured frame format".into()).into(),
            );
        }
        let maximum = if config.fd { 64 } else { 8 };
        if usize::from(signal.offset) + signal.data_type.width() > maximum {
            return Err(Usage(
                "CAN mapped signal byte range exceeds the configured payload size".into(),
            )
            .into());
        }
        if signal.bit.is_some_and(|bit| bit > 7)
            || (signal.bit.is_some() && !matches!(signal.data_type, DataType::Bool))
        {
            return Err(Usage("CAN bit must be 0–7 and only applies to bool".into()).into());
        }
        if !signal.scale.is_finite()
            || !signal.offset_value.is_finite()
            || (matches!(
                signal.data_type,
                DataType::Bool | DataType::U64 | DataType::I64
            ) && (signal.scale != 1.0 || signal.offset_value != 0.0))
        {
            return Err(Usage(
                "CAN scaling must be finite; bool and 64-bit integer signals cannot be scaled"
                    .into(),
            )
            .into());
        }
        if !config.filters.is_empty()
            && !config
                .filters
                .iter()
                .any(|filter| filter.matches(signal.can_id, signal.extended))
        {
            return Err(Usage("CAN signal is excluded by the configured filters".into()).into());
        }
    }
    Ok(())
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    validate(config)?;
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux::Capture::new(config.clone())))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = config.error_frames;
        Err(Categorized::error(
            ErrorKind::Unsupported,
            "native CAN capture requires Linux SocketCAN",
        ))
    }
}

#[cfg(any(target_os = "linux", test))]
fn malformed(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, format!("CAN capture: {message}"))
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug)]
struct Frame<'a> {
    raw_id: u32,
    id: u32,
    extended: bool,
    remote: bool,
    error: bool,
    fd: bool,
    flags: u8,
    length: u8,
    len8_dlc: Option<u8>,
    data: &'a [u8],
}
#[cfg(any(target_os = "linux", test))]
impl<'a> Frame<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self> {
        if !matches!(bytes.len(), 16 | 72) {
            return Err(malformed("invalid classic/FD frame size"));
        }
        let (header, payload) = bytes
            .split_first_chunk::<8>()
            .ok_or_else(|| malformed("incomplete CAN frame header"))?;
        let raw_id = u32::from_ne_bytes([header[0], header[1], header[2], header[3]]);
        let fd = bytes.len() == 72;
        let extended = raw_id & EFF != 0;
        let remote = raw_id & RTR != 0;
        let error = raw_id & ERR != 0;
        let id = raw_id & ID_MASK;
        let length = header[4];
        if usize::from(length) > if fd { 64 } else { 8 } {
            return Err(malformed("payload length exceeds frame size"));
        }
        if (!extended && !error && id > SFF_MASK)
            || (fd && (remote || error))
            || (error && (extended || remote || length != 8))
        {
            return Err(malformed("invalid identifier or frame flag combination"));
        }
        Ok(Self {
            raw_id,
            id,
            extended,
            remote,
            error,
            fd,
            flags: if fd { header[5] } else { 0 },
            length,
            len8_dlc: (!fd && length == 8 && (9..=15).contains(&header[7])).then_some(header[7]),
            data: if remote {
                &[]
            } else {
                payload
                    .get(..usize::from(length))
                    .ok_or_else(|| malformed("incomplete CAN payload"))?
            },
        })
    }
    fn accepted(&self, config: &Config) -> bool {
        if self.error {
            config.error_frames
        } else {
            config.filters.is_empty()
                || config
                    .filters
                    .iter()
                    .any(|filter| filter.matches(self.id, self.extended))
        }
    }
    fn readings(
        &self,
        config: &Config,
        local: bool,
        kernel_received_at: Option<&str>,
    ) -> Result<Vec<Reading>> {
        let mut value = json!({
            "kind":"can_frame", "interface":config.interface,
            "can_id":self.id, "raw_can_id":self.raw_id, "extended":self.extended,
            "remote":self.remote, "error":self.error, "fd":self.fd,
            "length":self.length, "data_hex":crate::types::hex(self.data),
            "fd_flags":self.flags, "bitrate_switch":self.fd && self.flags & 1 != 0,
            "error_state_indicator":self.fd && self.flags & 2 != 0,
            "local":local,
        });
        if let Some(dlc) = self.len8_dlc {
            value["len8_dlc"] = json!(dlc);
        }
        if let Some(time) = kernel_received_at {
            value["kernel_received_at"] = json!(time);
            value["timestamp_clock"] = json!("linux_socket_realtime_receive");
        }
        let mut readings = vec![Reading {
            signal: config.signal.clone(),
            value,
            source_time: None,
        }];
        if !self.remote && !self.error {
            for signal in &config.signals {
                if signal.can_id == self.id && signal.extended == self.extended {
                    readings.push(signal.decode(self.data)?);
                }
            }
        }
        Ok(readings)
    }
}

#[cfg(any(target_os = "linux", test))]
impl Signal {
    fn decode(&self, payload: &[u8]) -> Result<Reading> {
        let start = usize::from(self.offset);
        let bytes = payload
            .get(start..start + self.data_type.width())
            .ok_or_else(|| {
                malformed("matching frame is shorter than its configured signal mapping")
            })?;
        macro_rules! number {
            ($kind:ty, $width:expr) => {{
                let array: [u8; $width] = bytes.try_into()?;
                match self.byte_order {
                    ByteOrder::Little => <$kind>::from_le_bytes(array),
                    ByteOrder::Big => <$kind>::from_be_bytes(array),
                }
            }};
        }
        let value = match self.data_type {
            DataType::Bool => Value::Bool(bytes[0] & (1 << self.bit.unwrap_or(0)) != 0),
            DataType::U8 => json!(number!(u8, 1)),
            DataType::I8 => json!(number!(i8, 1)),
            DataType::U16 => json!(number!(u16, 2)),
            DataType::I16 => json!(number!(i16, 2)),
            DataType::U32 => json!(number!(u32, 4)),
            DataType::I32 => json!(number!(i32, 4)),
            DataType::U64 => json!(number!(u64, 8)),
            DataType::I64 => json!(number!(i64, 8)),
            DataType::F32 => {
                let v = number!(f32, 4);
                if !v.is_finite() {
                    return Err(malformed("non-finite mapped float"));
                }
                json!(v)
            }
            DataType::F64 => {
                let v = number!(f64, 8);
                if !v.is_finite() {
                    return Err(malformed("non-finite mapped float"));
                }
                json!(v)
            }
        };
        let value = if self.scale == 1.0 && self.offset_value == 0.0 {
            value
        } else {
            let scaled = value
                .as_f64()
                .ok_or_else(|| malformed("cannot scale mapped value"))?
                * self.scale
                + self.offset_value;
            if !scaled.is_finite() {
                return Err(malformed("non-finite scaled value"));
            }
            json!(scaled)
        };
        Ok(Reading {
            signal: self.name.clone(),
            value,
            source_time: None,
        })
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{Config, Frame, ID_MASK, malformed};
    use crate::{
        timeseries::{Reading, Source},
        types::{Categorized, ErrorKind},
    };
    use anyhow::Result;
    use chrono::{DateTime, SecondsFormat, Utc};
    use std::{
        collections::BTreeSet,
        ffi::CString,
        io, mem,
        os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd},
        time::{Duration, Instant},
    };

    pub(super) struct Capture {
        config: Config,
        socket: Option<OwnedFd>,
        index: u32,
        observed: Option<u64>,
        seen: bool,
        unobserved: BTreeSet<String>,
    }
    impl Capture {
        pub(super) fn new(config: Config) -> Self {
            let unobserved = config
                .signals
                .iter()
                .map(|signal| signal.name.clone())
                .collect();
            Self {
                config,
                socket: None,
                index: 0,
                observed: None,
                seen: false,
                unobserved,
            }
        }
        fn connect(&mut self) -> Result<()> {
            let interface = CString::new(self.config.interface.as_bytes())?;
            // SAFETY: interface is NUL-terminated and live throughout the call.
            let index = unsafe { libc::if_nametoindex(interface.as_ptr()) };
            if index == 0 {
                return Err(failure("find CAN interface", &io::Error::last_os_error()));
            }
            // SAFETY: creates a new independent socket; no pointers are passed.
            let fd = unsafe {
                libc::socket(
                    libc::AF_CAN,
                    libc::SOCK_RAW | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                    libc::CAN_RAW,
                )
            };
            if fd < 0 {
                return Err(failure("open CAN socket", &io::Error::last_os_error()));
            }
            // SAFETY: fd is a newly returned descriptor with ownership transferred here.
            let socket = unsafe { OwnedFd::from_raw_fd(fd) };
            set(fd, libc::SOL_SOCKET, libc::SO_RXQ_OVFL, &1_i32)?;
            set(fd, libc::SOL_SOCKET, libc::SO_TIMESTAMPNS, &1_i32)?;
            if self.config.fd {
                set(fd, libc::SOL_CAN_RAW, libc::CAN_RAW_FD_FRAMES, &1_i32)?;
            }
            if !self.config.filters.is_empty() {
                let filters = self
                    .config
                    .filters
                    .iter()
                    .map(super::Filter::kernel)
                    .collect::<Vec<_>>();
                // SAFETY: filters points to initialized can_filter entries copied by the kernel.
                if unsafe {
                    libc::setsockopt(
                        fd,
                        libc::SOL_CAN_RAW,
                        libc::CAN_RAW_FILTER,
                        filters.as_ptr().cast(),
                        libc::socklen_t::try_from(size_of_val(filters.as_slice()))?,
                    )
                } < 0
                {
                    return Err(failure("set CAN filters", &io::Error::last_os_error()));
                }
            }
            set(
                fd,
                libc::SOL_CAN_RAW,
                libc::CAN_RAW_ERR_FILTER,
                &if self.config.error_frames { ID_MASK } else { 0 },
            )?;
            // SAFETY: sockaddr_can is a C ABI structure valid when initialized with zeroes.
            let mut address: libc::sockaddr_can = unsafe { mem::zeroed() };
            address.can_family = u16::try_from(libc::AF_CAN)?;
            address.can_ifindex = i32::try_from(index)?;
            // SAFETY: address is initialized, with pointer and length matching its ABI.
            if unsafe {
                libc::bind(
                    fd,
                    (&raw const address).cast(),
                    libc::socklen_t::try_from(size_of_val(&address))?,
                )
            } < 0
            {
                return Err(failure("bind CAN interface", &io::Error::last_os_error()));
            }
            // Require an end-of-run counter as well as ancillary overflow reports:
            // a drop after the last delivered frame must not disappear at cutoff.
            dropped(fd)?;
            self.index = index;
            self.socket = Some(socket);
            Ok(())
        }
    }
    fn set<T>(fd: i32, level: i32, option: i32, value: &T) -> Result<()> {
        // SAFETY: value points to one initialized T; kernel copies its bytes during the call.
        if unsafe {
            libc::setsockopt(
                fd,
                level,
                option,
                std::ptr::from_ref(value).cast(),
                libc::socklen_t::try_from(size_of::<T>())?,
            )
        } < 0
        {
            return Err(failure(
                "configure CAN capture socket",
                &io::Error::last_os_error(),
            ));
        }
        Ok(())
    }
    fn failure(action: &str, error: &io::Error) -> anyhow::Error {
        let kind = match error.kind() {
            io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
            _ => ErrorKind::Unavailable,
        };
        Categorized::error(kind, format!("cannot {action}: {error}"))
    }
    fn dropped(fd: i32) -> Result<()> {
        let mut information = [0_u32; 9];
        let mut length = libc::socklen_t::try_from(size_of_val(&information))?;
        // SO_MEMINFO exposes SK_MEMINFO_DROPS (index 8 in linux/sock_diag.h).
        // SAFETY: both output pointers name initialized writable allocations.
        if unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_MEMINFO,
                information.as_mut_ptr().cast(),
                &raw mut length,
            )
        } < 0
        {
            return Err(failure(
                "read CAN socket loss counter",
                &io::Error::last_os_error(),
            ));
        }
        if usize::try_from(length)? < size_of_val(&information) {
            return Err(malformed("kernel returned an incomplete CAN loss counter"));
        }
        lost(information[8])
    }
    fn lost(count: u32) -> Result<()> {
        if count != 0 {
            return Err(Categorized::new(
                ErrorKind::Incomplete,
                "CAN receive queue lost frames; recording stopped",
            )
            .details(serde_json::json!({"dropped_frames":count}))
            .into());
        }
        Ok(())
    }

    #[repr(align(16))]
    struct Control([u8; 128]);
    #[allow(
        clippy::useless_conversion,
        reason = "Linux libc uses different integer widths for cmsghdr and time_t across targets"
    )]
    fn ancillary(control: &[u8]) -> Result<Option<String>> {
        let mut timestamp = None;
        let mut position = 0;
        // SAFETY: CMSG_LEN computes an ABI size without accessing memory.
        let header_length = usize::try_from(unsafe { libc::CMSG_LEN(0) })?;
        let alignment = size_of::<usize>();
        while control.len().saturating_sub(position) >= size_of::<libc::cmsghdr>() {
            let bytes = &control[position..];
            // SAFETY: bytes contains the entire header; unaligned reads are permitted.
            let value = unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<libc::cmsghdr>()) };
            let length = usize::try_from(value.cmsg_len)?;
            if length < header_length || length > bytes.len() {
                return Err(malformed("invalid CAN control message length"));
            }
            let data = &bytes[header_length..length];
            if value.cmsg_level == libc::SOL_SOCKET && value.cmsg_type == libc::SO_RXQ_OVFL {
                if data.len() != size_of::<u32>() {
                    return Err(malformed("invalid CAN overflow control message"));
                }
                lost(u32::from_ne_bytes(data.try_into()?))?;
            } else if value.cmsg_level == libc::SOL_SOCKET
                && value.cmsg_type == libc::SCM_TIMESTAMPNS
            {
                if data.len() != size_of::<libc::timespec>() || timestamp.is_some() {
                    return Err(malformed("invalid CAN receive timestamp control message"));
                }
                // SAFETY: checked ancillary payload size; unaligned reads are allowed.
                let time =
                    unsafe { std::ptr::read_unaligned(data.as_ptr().cast::<libc::timespec>()) };
                let ns = u32::try_from(time.tv_nsec)
                    .ok()
                    .filter(|ns| *ns < 1_000_000_000)
                    .ok_or_else(|| malformed("invalid CAN kernel timestamp"))?;
                let instant = DateTime::<Utc>::from_timestamp(i64::from(time.tv_sec), ns)
                    .ok_or_else(|| malformed("CAN kernel timestamp is out of range"))?;
                timestamp = Some(instant.to_rfc3339_opts(SecondsFormat::Nanos, true));
            }
            position += (length + alignment - 1) & !(alignment - 1);
        }
        Ok(timestamp)
    }
    impl Source for Capture {
        fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
            let deadline = Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| malformed("read timeout overflow"))?;
            if self.socket.is_none() {
                self.connect()?;
            }
            let fd = self
                .socket
                .as_ref()
                .ok_or_else(|| malformed("socket missing"))?
                .as_raw_fd();
            for _ in 0..256 {
                dropped(fd)?;
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Ok(Vec::new());
                }
                let mut event = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let millis = i32::try_from(left.as_millis().saturating_add(1)).unwrap_or(i32::MAX);
                // SAFETY: event is one initialized pollfd and its descriptor stays open.
                let ready = unsafe { libc::poll(&raw mut event, 1, millis) };
                if ready < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(failure("wait for CAN frame", &error));
                }
                if ready == 0 {
                    dropped(fd)?;
                    return Ok(Vec::new());
                }
                if event.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    return Err(Categorized::error(
                        ErrorKind::Disconnected,
                        "CAN interface became unavailable",
                    ));
                }
                let mut frame = [0_u8; 72];
                let mut control = Control([0; 128]);
                // SAFETY: zero initializes the plain C socket address and message structures.
                let mut address: libc::sockaddr_can = unsafe { mem::zeroed() };
                let mut vector = libc::iovec {
                    iov_base: frame.as_mut_ptr().cast(),
                    iov_len: frame.len(),
                };
                // SAFETY: msghdr has no invalid bit patterns when zeroed.
                let mut message: libc::msghdr = unsafe { mem::zeroed() };
                message.msg_name = (&raw mut address).cast();
                message.msg_namelen = libc::socklen_t::try_from(size_of_val(&address))?;
                message.msg_iov = &raw mut vector;
                message.msg_iovlen = 1;
                message.msg_control = control.0.as_mut_ptr().cast();
                message.msg_controllen = control.0.len();
                // SAFETY: all receive buffers and output structures are writable for their lengths.
                let count = unsafe {
                    libc::recvmsg(fd, &raw mut message, libc::MSG_TRUNC | libc::MSG_DONTWAIT)
                };
                if count < 0 {
                    let error = io::Error::last_os_error();
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) {
                        continue;
                    }
                    return Err(failure("receive CAN frame", &error));
                }
                let observed = crate::host::monotonic_ns();
                if message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0
                    || usize::try_from(count)? > frame.len()
                {
                    return Err(malformed("CAN frame or metadata was truncated"));
                }
                if usize::try_from(message.msg_namelen)? < size_of_val(&address)
                    || address.can_family != u16::try_from(libc::AF_CAN)?
                    || address.can_ifindex != i32::try_from(self.index)?
                {
                    return Err(malformed(
                        "CAN receive address does not match the selected interface",
                    ));
                }
                let control = control
                    .0
                    .get(..message.msg_controllen)
                    .ok_or_else(|| malformed("invalid CAN ancillary buffer length"))?;
                let timestamp = ancillary(control)?
                    .ok_or_else(|| malformed("kernel omitted CAN receive timestamp"))?;
                dropped(fd)?;
                let frame = Frame::parse(&frame[..usize::try_from(count)?])?;
                if frame.fd && !self.config.fd {
                    return Err(malformed("unexpected CAN FD frame"));
                }
                if !frame.accepted(&self.config) {
                    continue;
                }
                let readings = frame.readings(
                    &self.config,
                    message.msg_flags & libc::MSG_DONTROUTE != 0,
                    Some(&timestamp),
                )?;
                self.observed = observed;
                self.seen = true;
                for reading in &readings {
                    self.unobserved.remove(&reading.signal);
                }
                return Ok(readings);
            }
            Ok(Vec::new())
        }
        fn ready(&self) -> bool {
            self.seen && self.unobserved.is_empty()
        }
        fn observed_ns(&self) -> Option<u64> {
            self.observed
        }
        fn finish(&mut self, _timeout: Duration) -> Result<()> {
            self.socket
                .as_ref()
                .map_or(Ok(()), |socket| dropped(socket.as_raw_fd()))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn control<T: Copy>(kind: i32, value: T) -> Vec<u8> {
            // SAFETY: CMSG_LEN only computes the platform ABI size.
            let header_length = usize::try_from(unsafe { libc::CMSG_LEN(0) }).unwrap();
            let length = header_length + size_of::<T>();
            let mut bytes = vec![0; length];
            let header = libc::cmsghdr {
                cmsg_len: length,
                cmsg_level: libc::SOL_SOCKET,
                cmsg_type: kind,
            };
            // SAFETY: the allocation contains room for the complete C header.
            unsafe {
                bytes
                    .as_mut_ptr()
                    .cast::<libc::cmsghdr>()
                    .write_unaligned(header);
            };
            // SAFETY: the remaining allocation contains room for this initialized payload.
            unsafe {
                bytes[header_length..]
                    .as_mut_ptr()
                    .cast::<T>()
                    .write_unaligned(value);
            };
            bytes
        }

        #[test]
        fn receive_metadata_validates_timestamp_overflow_and_lengths() {
            let timestamp = libc::timespec {
                tv_sec: 1_700_000_000,
                tv_nsec: 123,
            };
            let bytes = control(libc::SCM_TIMESTAMPNS, timestamp);
            assert_eq!(
                ancillary(&bytes).unwrap().as_deref(),
                Some("2023-11-14T22:13:20.000000123Z")
            );
            assert!(
                ancillary(&control(libc::SO_RXQ_OVFL, 0_u32))
                    .unwrap()
                    .is_none()
            );
            let error = ancillary(&control(libc::SO_RXQ_OVFL, 4_u32)).unwrap_err();
            let error = error.downcast_ref::<Categorized>().unwrap();
            assert_eq!(error.kind, ErrorKind::Incomplete);
            assert_eq!(error.details.as_ref().unwrap()["dropped_frames"], 4);
            ancillary(&control(libc::SCM_TIMESTAMPNS, 0_u32)).unwrap_err();
            let invalid = libc::timespec {
                tv_sec: 0,
                tv_nsec: 1_000_000_000,
            };
            ancillary(&control(libc::SCM_TIMESTAMPNS, invalid)).unwrap_err();
            ancillary(&bytes[..size_of::<libc::cmsghdr>()]).unwrap_err();
            let mut duplicate = bytes.clone();
            duplicate.resize(bytes.len().next_multiple_of(size_of::<usize>()), 0);
            duplicate.extend_from_slice(&bytes);
            ancillary(&duplicate).unwrap_err();
        }

        #[test]
        fn kernel_loss_counter_detects_drops_without_a_following_receive() {
            let receiver = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
            set(
                receiver.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                &1024_i32,
            )
            .unwrap();
            dropped(receiver.as_raw_fd()).unwrap();
            let sender = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
            for _ in 0..512 {
                sender
                    .send_to(&[0; 128], receiver.local_addr().unwrap())
                    .unwrap();
            }
            let error = dropped(receiver.as_raw_fd()).unwrap_err();
            assert_eq!(
                error.downcast_ref::<Categorized>().unwrap().kind,
                ErrorKind::Incomplete
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_integer_configuration_preserves_exact_values_and_defaults() {
        let raw = r#"{"interface":"vcan0","filters":[{"can_id":2.91e2,"mask":2047.0}],"signals":[{"name":"ready","can_id":291.0,"offset":1e0,"data_type":"bool","bit":7.0}]}"#;
        let configured: Config = serde_json::from_str(raw).unwrap();
        let buffered: Config = serde_json::from_value(serde_json::from_str(raw).unwrap()).unwrap();
        for configured in [configured, buffered] {
            assert_eq!(configured.filters[0].can_id, 291);
            assert_eq!(configured.filters[0].mask, 2047);
            assert_eq!(configured.signals[0].can_id, 291);
            assert_eq!(configured.signals[0].offset, 1);
            assert_eq!(configured.signals[0].bit, Some(7));
            assert!(configured.fd && configured.error_frames);
        }
        let omitted: Filter = serde_json::from_str(r#"{"can_id":1e0}"#).unwrap();
        assert_eq!(omitted.mask, ID_MASK);
        let nullable: Signal =
            serde_json::from_str(r#"{"name":"ready","can_id":1e0,"offset":0.0,"bit":null}"#)
                .unwrap();
        assert_eq!(nullable.bit, None);
        for raw in [r#"{"can_id":4294967296.0}"#, r#"{"can_id":1.01}"#] {
            serde_json::from_str::<Filter>(raw).unwrap_err();
        }
    }

    fn config(value: Value) -> Config {
        serde_json::from_value(value).unwrap()
    }
    fn basic() -> Config {
        config(json!({"interface":"vcan0"}))
    }
    fn frame(raw_id: u32, payload: &[u8], fd: bool) -> Vec<u8> {
        let mut bytes = vec![0; if fd { 72 } else { 16 }];
        bytes[..4].copy_from_slice(&raw_id.to_ne_bytes());
        bytes[4] = u8::try_from(payload.len()).unwrap();
        bytes[8..8 + payload.len()].copy_from_slice(payload);
        bytes
    }
    fn mapped(data_type: &str, extra: Value) -> Signal {
        let mut value = json!({"name":"value","can_id":291,"offset":0,"data_type":data_type});
        let extra: serde_json::Map<_, _> = serde_json::from_value(extra).unwrap();
        value.as_object_mut().unwrap().extend(extra);
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn classic_fd_remote_and_error_frames_preserve_wire_values() {
        let mut bytes = frame(0x123, &[0, 1, 2, 3, 4, 5, 6, 255], false);
        bytes[7] = 15;
        let readings = Frame::parse(&bytes)
            .unwrap()
            .readings(&basic(), false, None)
            .unwrap();
        assert_eq!(readings.len(), 1);
        assert_eq!(readings[0].value["data_hex"], "00010203040506ff");
        assert_eq!(readings[0].value["len8_dlc"], 15);
        assert!(readings[0].value.get("kernel_received_at").is_none());
        assert!(readings[0].source_time.is_none());

        let payload: Vec<u8> = (0..64).collect();
        let mut bytes = frame(EFF | 0x1abc_def0, &payload, true);
        bytes[5] = 0x87; // Preserve future FD flags as well as BRS, ESI and FDF.
        let readings = Frame::parse(&bytes)
            .unwrap()
            .readings(&basic(), true, Some("2023-01-01T00:00:00Z"))
            .unwrap();
        let raw = &readings[0].value;
        assert_eq!(raw["data_hex"], crate::types::hex(payload));
        assert_eq!(raw["fd_flags"], 0x87);
        assert_eq!(raw["bitrate_switch"], true);
        assert_eq!(raw["error_state_indicator"], true);
        assert_eq!(raw["local"], true);
        assert_eq!(raw["timestamp_clock"], "linux_socket_realtime_receive");
        assert!(readings[0].source_time.is_none());

        let configured = config(json!({"interface":"vcan0","signals":[
            {"name":"value","can_id":291,"offset":0,"data_type":"u64"}
        ]}));
        for raw_id in [RTR | 0x123, ERR | 0x123] {
            let bytes = frame(raw_id, &[7; 8], false);
            let readings = Frame::parse(&bytes)
                .unwrap()
                .readings(&configured, false, None)
                .unwrap();
            assert_eq!(
                readings.len(),
                1,
                "status frames cannot become typed sensor values"
            );
            if raw_id & RTR != 0 {
                assert_eq!(readings[0].value["data_hex"], "");
                assert_eq!(readings[0].value["length"], 8);
            } else {
                assert_eq!(readings[0].value["data_hex"], "0707070707070707");
            }
        }
    }

    #[test]
    fn malformed_frame_sizes_flags_and_mapped_payloads_fail_explicitly() {
        for length in [0, 15, 17, 71, 73] {
            Frame::parse(&vec![0; length]).unwrap_err();
        }
        for (raw_id, fd) in [
            (0x800, false),
            (RTR | 1, true),
            (ERR | 1, true),
            (ERR | EFF | 1, false),
        ] {
            Frame::parse(&frame(raw_id, &[0; 8], fd)).unwrap_err();
        }
        for (fd, length) in [(false, 9), (true, 65)] {
            let mut bytes = frame(1, &[], fd);
            bytes[4] = length;
            Frame::parse(&bytes).unwrap_err();
        }
        Frame::parse(&frame(ERR | 1, &[0; 7], false)).unwrap_err();
        let signal = mapped("u64", json!({}));
        signal.decode(&[0; 7]).unwrap_err();
        mapped("f32", json!({}))
            .decode(&f32::NAN.to_le_bytes())
            .unwrap_err();
        mapped("f64", json!({"scale":2.0}))
            .decode(&f64::MAX.to_le_bytes())
            .unwrap_err();
    }

    #[test]
    fn explicit_mappings_preserve_signed_and_full_width_integers() {
        assert_eq!(
            mapped("u64", json!({}))
                .decode(&u64::MAX.to_le_bytes())
                .unwrap()
                .value
                .as_u64(),
            Some(u64::MAX)
        );
        assert_eq!(
            mapped("i64", json!({"byte_order":"big"}))
                .decode(&i64::MIN.to_be_bytes())
                .unwrap()
                .value
                .as_i64(),
            Some(i64::MIN)
        );
        assert_eq!(
            mapped(
                "i16",
                json!({"byte_order":"big","scale":0.25,"offset_value":1.5})
            )
            .decode(&(-20_i16).to_be_bytes())
            .unwrap()
            .value,
            -3.5
        );
        assert_eq!(
            mapped("bool", json!({"bit":7}))
                .decode(&[0x80])
                .unwrap()
                .value,
            true
        );
        assert_eq!(
            mapped("bool", json!({"bit":0}))
                .decode(&[0x80])
                .unwrap()
                .value,
            false
        );
        assert_eq!(
            mapped("f32", json!({"byte_order":"big"}))
                .decode(&1.25_f32.to_be_bytes())
                .unwrap()
                .value,
            1.25
        );
    }

    #[test]
    fn config_rejects_ambiguous_mappings_and_filters_distinguish_frame_formats() {
        validate(&basic()).unwrap();
        let configured = config(
            json!({"interface":"vcan0","filters":[{"can_id":288,"mask":2032,"extended":false}]}),
        );
        assert!(
            Frame::parse(&frame(0x123, &[], false))
                .unwrap()
                .accepted(&configured)
        );
        assert!(
            !Frame::parse(&frame(EFF | 0x123, &[], false))
                .unwrap()
                .accepted(&configured)
        );
        assert!(
            !Frame::parse(&frame(0x133, &[], false))
                .unwrap()
                .accepted(&configured)
        );
        assert!(
            Frame::parse(&frame(ERR | 1, &[0; 8], false))
                .unwrap()
                .accepted(&configured)
        );
        let no_errors = config(json!({"interface":"vcan0","error_frames":false}));
        assert!(
            !Frame::parse(&frame(ERR | 1, &[0; 8], false))
                .unwrap()
                .accepted(&no_errors)
        );
        for value in [
            json!({"interface":""}),
            json!({"interface":"vcan0","filters":[{"can_id":2048,"extended":false}]}),
            json!({"interface":"vcan0","signals":[{"name":"frame","can_id":1,"offset":0}]}),
            json!({"interface":"vcan0","fd":false,"signals":[{"name":"x","can_id":1,"offset":7}]}),
            json!({"interface":"vcan0","signals":[{"name":"x","can_id":1,"offset":0,"data_type":"u64","scale":2.0}]}),
            json!({"interface":"vcan0","signals":[{"name":"x","can_id":1,"offset":0,"bit":2}]}),
            json!({"interface":"vcan0","filters":[{"can_id":1}],"signals":[{"name":"x","can_id":2,"offset":0}]}),
        ] {
            validate(&config(value)).unwrap_err();
        }
    }
}
