//! Native `GenTL` C ABI, used exclusively by the isolated helper.
//! ABI and ownership: EMVA GenTL1.6 sections3,4,5 and6.
//! <https://www.emva.org/wp-content/uploads/GenICam_GenTL_1_6.pdf>
use super::{MAX_DEVICES, MAX_FRAME, MAX_NAME, MAX_XML, Selector, wire};
use crate::{
    transport::{
        chunks::ByteOrder,
        gendc,
        multipart::{Layout, MAX_PARTS, Part},
    },
    types::{CameraInfo, Categorized, ErrorKind, Transport},
};
use anyhow::{Context as _, Result, ensure};
use serde_json::json;
use std::{
    ffi::{CStr, CString, c_char, c_void},
    path::Path,
    ptr::NonNull,
    time::{Duration, Instant},
};

type Handle = *mut c_void;
type Error = i32;
type DeviceInfo =
    unsafe extern "system" fn(Handle, i32, *mut i32, *mut c_void, *mut usize) -> Error;
// GenTL1.6 section6.5.1.1 specifies byte-aligned chunk descriptors.
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
struct RawChunk {
    id: u64,
    offset: isize,
    length: usize,
}
type BufferChunks = unsafe extern "system" fn(Handle, Handle, *mut RawChunk, *mut usize) -> Error;
type NumParts = unsafe extern "system" fn(Handle, Handle, *mut u32) -> Error;
type PartInfo =
    unsafe extern "system" fn(Handle, Handle, u32, i32, *mut i32, *mut c_void, *mut usize) -> Error;
const TOO_SMALL: Error = -1016;
const STRING: i32 = 1;
const U64: i32 = 8;
const PTR: i32 = 10;
const BOOL: i32 = 11;
const SIZET: i32 = 12;
const I32: i32 = 5;
const MAX_BUFFERS: usize = 32;

fn check(code: Error, operation: &str) -> Result<()> {
    if code == 0 {
        return Ok(());
    }
    let kind = match code {
        -1011 => ErrorKind::Timeout,
        -1003 | -1014 => ErrorKind::Unsupported,
        -1004 | -1022 => ErrorKind::CameraBusy,
        -1005 => ErrorKind::PermissionDenied,
        _ => ErrorKind::OperationFailed,
    };
    Err(Categorized::new(kind, format!("GenTL {operation} failed"))
        .details(json!({"gentl_error":code}))
        .into())
}
fn handle(value: Handle, name: &str) -> Result<Handle> {
    ensure!(!value.is_null(), "GenTL {name} returned an invalid handle");
    Ok(value)
}
macro_rules! invoke {
    ($function:expr $(, $argument:expr)* $(,)?) => {{
        // SAFETY: native calls are confined to this helper. Their handles and
        // sized storage are held by the caller for the documented C ABI call.
        unsafe { ($function)($($argument),*) }
    }};
}
macro_rules! api {
    ($($field:ident: $symbol:literal ($($argument:ty),*)),+ $(,)?) => {
        struct Api { _library: Library, initialized: bool, dev_info: Option<DeviceInfo>, buffer_chunks: Option<BufferChunks>, parts_count: Option<NumParts>, part_info: Option<PartInfo>, $($field: unsafe extern "system" fn($($argument),*) -> Error),+ }
        impl Api {
            fn load(path: &Path) -> Result<Self> {
                let library = Library::open(path)?;
                let mut api = Self {
                    $($field: {
                        // SAFETY: each symbol's type is its named GenTL C ABI
                        // signature. Library remains owned by the returned Api.
                        unsafe { library.symbol(concat!($symbol, "\0").as_bytes())? }
                    }),+,
                    dev_info: {
                        // SAFETY: this optional symbol uses the exact GenTL DevGetInfo C ABI.
                        unsafe { library.optional_symbol(b"DevGetInfo\0")? }
                    },
                    buffer_chunks: {
                        // SAFETY: exact DSGetBufferChunkData C ABI. Missing
                        // legacy exports leave explicit opaque chunk metadata.
                        unsafe { library.optional_symbol(b"DSGetBufferChunkData\0")? }
                    },
                    parts_count: {
                        // SAFETY: exact optional GenTL multipart count ABI.
                        unsafe { library.optional_symbol(b"DSGetNumBufferParts\0")? }
                    },
                    part_info: {
                        // SAFETY: exact optional GenTL multipart metadata ABI.
                        unsafe { library.optional_symbol(b"DSGetBufferPartInfo\0")? }
                    },
                    _library: library, initialized: false,
                };
                check(invoke!(api.init), "initialize library")?;
                api.initialized = true;
                Ok(api)
            }
        }
    };
}
api! {
    init: "GCInitLib"(), close: "GCCloseLib"(),
    tl_open: "TLOpen"(*mut Handle), tl_close: "TLClose"(Handle),
    tl_update: "TLUpdateInterfaceList"(Handle, *mut u8, u64),
    tl_count: "TLGetNumInterfaces"(Handle, *mut u32),
    tl_id: "TLGetInterfaceID"(Handle, u32, *mut c_char, *mut usize),
    tl_interface: "TLOpenInterface"(Handle, *const c_char, *mut Handle),
    if_close: "IFClose"(Handle), if_update: "IFUpdateDeviceList"(Handle, *mut u8, u64),
    if_count: "IFGetNumDevices"(Handle, *mut u32),
    if_id: "IFGetDeviceID"(Handle, u32, *mut c_char, *mut usize),
    if_info: "IFGetDeviceInfo"(Handle, *const c_char, i32, *mut i32, *mut c_void, *mut usize),
    if_open: "IFOpenDevice"(Handle, *const c_char, i32, *mut Handle),
    dev_close: "DevClose"(Handle), dev_port: "DevGetPort"(Handle, *mut Handle),
    dev_count: "DevGetNumDataStreams"(Handle, *mut u32),
    dev_id: "DevGetDataStreamID"(Handle, u32, *mut c_char, *mut usize),
    dev_stream: "DevOpenDataStream"(Handle, *const c_char, *mut Handle),
    read: "GCReadPort"(Handle, u64, *mut c_void, *mut usize),
    write: "GCWritePort"(Handle, u64, *const c_void, *mut usize),
    url: "GCGetPortURL"(Handle, *mut c_char, *mut usize),
    ds_info: "DSGetInfo"(Handle, i32, *mut i32, *mut c_void, *mut usize),
    ds_alloc: "DSAllocAndAnnounceBuffer"(Handle, usize, *mut c_void, *mut Handle),
    ds_queue: "DSQueueBuffer"(Handle, Handle),
    ds_revoke: "DSRevokeBuffer"(Handle, Handle, *mut *mut c_void, *mut *mut c_void),
    ds_flush: "DSFlushQueue"(Handle, i32),
    ds_start: "DSStartAcquisition"(Handle, i32, u64),
    ds_stop: "DSStopAcquisition"(Handle, i32), ds_close: "DSClose"(Handle),
    buffer_info: "DSGetBufferInfo"(Handle, Handle, i32, *mut i32, *mut c_void, *mut usize),
    event_register: "GCRegisterEvent"(Handle, i32, *mut Handle),
    event_unregister: "GCUnregisterEvent"(Handle, i32),
    event_get: "EventGetData"(Handle, *mut c_void, *mut usize, u64),
}
impl Drop for Api {
    fn drop(&mut self) {
        if self.initialized {
            let _ = invoke!(self.close);
        }
    }
}
struct Library(NonNull<c_void>);
impl Library {
    #[cfg(unix)]
    fn open(path: &Path) -> Result<Self> {
        use std::os::unix::ffi::OsStrExt as _;
        ensure!(path.is_absolute(), "GenTL producer path must be absolute");
        let name = CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: user-selected native library, loaded only in helper process.
        NonNull::new(unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) })
            .map(Self)
            .context("cannot load the configured GenTL producer")
    }
    #[cfg(windows)]
    fn open(path: &Path) -> Result<Self> {
        use std::os::windows::ffi::OsStrExt as _;
        use windows_sys::Win32::System::LibraryLoader::{
            LOAD_WITH_ALTERED_SEARCH_PATH, LoadLibraryExW,
        };
        ensure!(path.is_absolute(), "GenTL producer path must be absolute");
        let name = path
            .as_os_str()
            .encode_wide()
            .chain([0])
            .collect::<Vec<_>>();
        // SAFETY: name is a live NUL-terminated absolute Windows path.
        // The altered search path resolves the producer's adjacent DLLs, while
        // retaining the vendor runtime's configured standard/PATH locations.
        NonNull::new(
            unsafe {
                LoadLibraryExW(
                    name.as_ptr(),
                    std::ptr::null_mut(),
                    LOAD_WITH_ALTERED_SEARCH_PATH,
                )
            }
            .cast(),
        )
        .map(Self)
        .context("cannot load the configured GenTL producer")
    }
    unsafe fn symbol<T: Copy>(&self, name: &[u8]) -> Result<T> {
        // SAFETY: forwarded exact C ABI signature, retained by the owning Api.
        unsafe { self.optional_symbol(name) }?.with_context(|| {
            format!(
                "GenTL producer is missing {}",
                String::from_utf8_lossy(name).trim_end_matches('\0')
            )
        })
    }
    unsafe fn optional_symbol<T: Copy>(&self, name: &[u8]) -> Result<Option<T>> {
        ensure!(
            size_of::<T>() == size_of::<*mut c_void>(),
            "incompatible GenTL function pointer size"
        );
        let name = CStr::from_bytes_with_nul(name)?;
        #[cfg(unix)]
        // SAFETY: live library and NUL-terminated symbol name.
        let symbol = unsafe { libc::dlsym(self.0.as_ptr(), name.as_ptr()) };
        #[cfg(windows)]
        // SAFETY: live module and NUL-terminated ASCII symbol name.
        let symbol = unsafe {
            windows_sys::Win32::System::LibraryLoader::GetProcAddress(
                self.0.as_ptr().cast(),
                name.as_ptr().cast(),
            )
        }
        .map_or(std::ptr::null_mut(), |function| function as *mut c_void);
        if symbol.is_null() {
            return Ok(None);
        }
        // SAFETY: caller supplies the exact function signature and retains Api.
        Ok(Some(unsafe { std::mem::transmute_copy(&symbol) }))
    }
}
impl Drop for Library {
    fn drop(&mut self) {
        #[cfg(unix)]
        // SAFETY: opened once and exclusively owned by this object.
        let _ = unsafe { libc::dlclose(self.0.as_ptr()) };
        #[cfg(windows)]
        // SAFETY: opened once and exclusively owned by this object.
        let _ = unsafe { windows_sys::Win32::Foundation::FreeLibrary(self.0.as_ptr().cast()) };
    }
}

fn string(mut get: impl FnMut(*mut c_char, *mut usize) -> Error) -> Result<String> {
    let mut length = 0;
    let result = get(std::ptr::null_mut(), &raw mut length);
    if result != TOO_SMALL {
        check(result, "query text size")?;
    }
    ensure!(
        (1..=MAX_NAME + 1).contains(&length),
        "invalid GenTL text size"
    );
    let mut bytes = vec![0; length];
    let capacity = length;
    check(get(bytes.as_mut_ptr().cast(), &raw mut length), "read text")?;
    ensure!(
        length > 0 && length <= capacity && bytes[length - 1] == 0,
        "invalid GenTL text termination or returned size"
    );
    bytes.truncate(length - 1);
    ensure!(!bytes.contains(&0), "GenTL text contains an embedded NUL");
    String::from_utf8(bytes).context("GenTL module identifier is not UTF-8")
}
fn info_string(mut get: impl FnMut(*mut i32, *mut c_void, *mut usize) -> Error) -> Result<String> {
    let mut kind = 0;
    let value = string(|bytes, length| get(&raw mut kind, bytes.cast(), length))?;
    ensure!(
        kind == STRING,
        "GenTL text information has an invalid data type"
    );
    Ok(value)
}
fn parse_url_list(bytes: &[u8]) -> Result<Vec<String>> {
    ensure!(
        bytes.len() <= MAX_NAME * 8 + 2,
        "GenTL XML URL list exceeds its size limit"
    );
    let mut content = bytes
        .strip_suffix(&[0])
        .context("GenTL XML URL list is not terminated")?;
    while let Some(stripped) = content.strip_suffix(&[0]) {
        content = stripped;
    }
    ensure!(!content.is_empty(), "GenTL XML URL list is empty");
    let mut urls = Vec::new();
    for bytes in content.split(|byte| *byte == 0) {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_NAME && urls.len() < 8,
            "invalid GenTL XML URL list entry or count"
        );
        urls.push(
            std::str::from_utf8(bytes)
                .context("GenTL XML URL is not UTF-8")?
                .to_owned(),
        );
    }
    Ok(urls)
}
fn url_list(mut get: impl FnMut(*mut c_char, *mut usize) -> Error) -> Result<Vec<String>> {
    let mut length = 0;
    let result = get(std::ptr::null_mut(), &raw mut length);
    if result != TOO_SMALL {
        check(result, "query XML URL list size")?;
    }
    ensure!(
        (1..=MAX_NAME * 8 + 2).contains(&length),
        "invalid GenTL XML URL list size"
    );
    let capacity = length;
    let mut bytes = vec![0; capacity];
    check(
        get(bytes.as_mut_ptr().cast(), &raw mut length),
        "read XML URL list",
    )?;
    ensure!(
        length > 0 && length <= capacity,
        "invalid GenTL XML URL list returned size"
    );
    parse_url_list(&bytes[..length])
}
fn optional_info_string(
    mut get: impl FnMut(*mut i32, *mut c_void, *mut usize) -> Error,
) -> Result<Option<String>> {
    let mut unavailable = false;
    let result = info_string(|kind, bytes, length| {
        let code = get(kind, bytes, length);
        unavailable = matches!(code, -1003 | -1008 | -1014);
        code
    });
    if unavailable {
        Ok(None)
    } else {
        result.map(Some)
    }
}
fn scalar<T: Default + Copy>(
    expected: i32,
    mut get: impl FnMut(*mut i32, *mut c_void, *mut usize) -> Error,
) -> Result<T> {
    let mut value = T::default();
    let mut kind = 0;
    let mut length = size_of::<T>();
    check(
        get(&raw mut kind, (&raw mut value).cast(), &raw mut length),
        "read typed information",
    )?;
    ensure!(
        kind == expected && length == size_of::<T>(),
        "GenTL information type or size differs from its ABI"
    );
    Ok(value)
}

fn optional_scalar<T: Default + Copy>(
    expected: i32,
    mut get: impl FnMut(*mut i32, *mut c_void, *mut usize) -> Error,
) -> Result<Option<T>> {
    let mut value = T::default();
    let mut kind = 0;
    let mut length = size_of::<T>();
    let result = get(&raw mut kind, (&raw mut value).cast(), &raw mut length);
    if matches!(result, -1003 | -1008 | -1014) {
        return Ok(None);
    }
    check(result, "read optional typed information")?;
    ensure!(
        kind == expected && length == size_of::<T>(),
        "GenTL optional information type or size differs from its ABI"
    );
    Ok(Some(value))
}

fn file_path(location: &str) -> Result<std::path::PathBuf> {
    let location = location.split(['?', '#']).next().unwrap_or("");
    let path = if let Some(rest) = location.strip_prefix("//") {
        if rest.starts_with('/') {
            rest
        } else if let Some(local) = rest.strip_prefix("localhost/") {
            return file_path(&format!("/{local}"));
        } else if cfg!(windows) && rest.as_bytes().get(1) == Some(&b':') {
            rest
        } else {
            return Err(Categorized::error(
                ErrorKind::Unsupported,
                "GenTL XML file URL must refer to the local host",
            ));
        }
    } else {
        location
    };
    let mut bytes = Vec::with_capacity(path.len());
    let mut input = path.as_bytes().iter().copied();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let high = input
                .next()
                .and_then(|value| char::from(value).to_digit(16))
                .context("invalid GenTL XML file URL escape")?;
            let low = input
                .next()
                .and_then(|value| char::from(value).to_digit(16))
                .context("invalid GenTL XML file URL escape")?;
            bytes.push(u8::try_from(high * 16 + low)?);
        } else {
            bytes.push(byte);
        }
    }
    ensure!(!bytes.contains(&0), "GenTL XML file URL contains a NUL");
    let path = String::from_utf8(bytes).context("GenTL XML file URL is not UTF-8")?;
    let path = if cfg!(windows) && path.starts_with('/') && path.as_bytes().get(2) == Some(&b':') {
        path.strip_prefix('/')
            .context("invalid GenTL Windows file URL")?
    } else {
        &path
    };
    let path = std::path::PathBuf::from(path);
    ensure!(path.is_absolute(), "GenTL XML file URL must be absolute");
    Ok(path)
}

/// Native enumeration waits share one operation budget, including library
/// loading. Leave a tail for serialized results and bounded teardown.
struct EnumerationBudget {
    deadline: Instant,
}
impl EnumerationBudget {
    fn new(timeout_ms: u64) -> Result<Self> {
        let timeout = Duration::from_millis(timeout_ms);
        let tail = (timeout / 4).min(Duration::from_millis(50));
        Ok(Self {
            deadline: Instant::now()
                .checked_add(timeout.saturating_sub(tail))
                .context("GenTL enumeration deadline overflow")?,
        })
    }
    fn remaining(&self) -> Result<Duration> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        ensure!(
            !remaining.is_zero(),
            Categorized::error(
                ErrorKind::Timeout,
                "GenTL enumeration exhausted its operation budget"
            )
        );
        Ok(remaining)
    }
    fn wait_ms(&self, pending: u32) -> Result<u64> {
        let wait = self.remaining()? / pending.saturating_add(1).max(1);
        Ok(u64::try_from(wait.as_millis())?)
    }
}
pub(super) struct Discovered {
    pub(super) report: wire::Discovery,
    api: Api,
    tl: Handle,
    interfaces: Vec<Handle>,
}
impl Drop for Discovered {
    fn drop(&mut self) {
        for iface in self.interfaces.drain(..).rev() {
            let _ = invoke!(self.api.if_close, iface);
        }
        if !self.tl.is_null() {
            let _ = invoke!(self.api.tl_close, self.tl);
        }
        // Api subsequently calls GCCloseLib and unloads the library.
    }
}
fn discovery_warning(report: &mut wire::Discovery, error: &anyhow::Error) {
    if report.warnings.len() < 8 {
        report.warnings.push(
            crate::scrub::scrub(&format!("{error:#}"))
                .chars()
                .take(MAX_NAME)
                .collect(),
        );
    }
}
pub(super) fn discover(path: &Path, timeout_ms: u64) -> Result<Discovered> {
    let budget = EnumerationBudget::new(timeout_ms)?;
    let api = Api::load(path)?;
    let mut discovered = Discovered {
        report: wire::Discovery::default(),
        api,
        tl: std::ptr::null_mut(),
        interfaces: Vec::new(),
    };
    budget.remaining()?;
    check(
        invoke!(discovered.api.tl_open, &raw mut discovered.tl),
        "open system",
    )?;
    handle(discovered.tl, "system")?;
    let api = &discovered.api;
    let mut changed = 0;
    check(
        invoke!(
            api.tl_update,
            discovered.tl,
            &raw mut changed,
            budget.wait_ms(2)?
        ),
        "update interfaces",
    )?;
    let mut interfaces = 0;
    budget.remaining()?;
    check(
        invoke!(api.tl_count, discovered.tl, &raw mut interfaces),
        "count interfaces",
    )?;
    ensure!(
        interfaces <= 64,
        "GenTL provider reports more than64 interfaces"
    );
    for index in 0..interfaces {
        let result: Result<()> = (|| {
            budget.remaining()?;
            let interface =
                string(|text, length| invoke!(api.tl_id, discovered.tl, index, text, length))?;
            let name = CString::new(interface.as_str())?;
            let mut iface = std::ptr::null_mut();
            check(
                invoke!(
                    api.tl_interface,
                    discovered.tl,
                    name.as_ptr(),
                    &raw mut iface
                ),
                "open interface",
            )?;
            handle(iface, "interface")?;
            discovered.interfaces.push(iface);
            check(
                invoke!(
                    api.if_update,
                    iface,
                    &raw mut changed,
                    budget.wait_ms(interfaces - index)?
                ),
                "update devices",
            )?;
            let mut count = 0;
            budget.remaining()?;
            check(
                invoke!(api.if_count, iface, &raw mut count),
                "count devices",
            )?;
            ensure!(
                usize::try_from(count)?
                    <= MAX_DEVICES.saturating_sub(discovered.report.cameras.len()),
                "GenTL provider reports too many cameras"
            );
            for index in 0..count {
                budget.remaining()?;
                let camera = (|| {
                    let device =
                        string(|text, length| invoke!(api.if_id, iface, index, text, length))?;
                    let selector = Selector {
                        producer: path.to_owned(),
                        interface: interface.clone(),
                        device,
                        stream: None,
                        part: None,
                        part_format: None,
                        gendc: None,
                    };
                    camera_info(api, iface, &selector)
                })();
                match camera {
                    Ok(camera) => discovered.report.cameras.push(camera),
                    Err(error) => discovery_warning(&mut discovered.report, &error),
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            discovery_warning(&mut discovered.report, &error);
            if budget.remaining().is_err() {
                break;
            }
        }
    }
    Ok(discovered)
}

fn camera_info(api: &Api, iface: Handle, selector: &Selector) -> Result<CameraInfo> {
    let id = CString::new(selector.device.as_str())?;
    let text = |command| {
        info_string(|kind, bytes, length| {
            invoke!(
                api.if_info,
                iface,
                id.as_ptr(),
                command,
                kind,
                bytes,
                length
            )
        })
    };
    Ok(CameraInfo {
        id: selector.encode()?,
        transport: Transport::GenTl,
        vendor: text(1)?,
        model: text(2)?,
        serial: optional_info_string(|kind, bytes, length| {
            invoke!(api.if_info, iface, id.as_ptr(), 7, kind, bytes, length)
        })?
        .unwrap_or_else(|| selector.device.clone()),
        address: Some(selector.producer.display().to_string()),
        profile: crate::camera_profile::CameraProfile::default(),
    })
}
struct Buffer {
    handle: Handle,
    base: usize,
    size: usize,
}
pub(super) struct Device {
    api: Api,
    tl: Handle,
    iface: Handle,
    device: Handle,
    port: Handle,
    stream: Handle,
    event: Handle,
    buffers: Vec<Buffer>,
    running: bool,
    sequence: u64,
    pixel_encodings: Vec<crate::types::PixelEncoding>,
    multipart_projection: bool,
    selected_part: Option<u32>,
    selected_format: Option<u32>,
    selected_gendc: Option<gendc::Selection>,
    pub(super) info: CameraInfo,
}
pub(super) struct FrameView<'a> {
    pub(super) image: wire::Image,
    pub(super) bytes: Vec<&'a [u8]>,
    pub(super) handle: Handle,
}
impl Device {
    pub(super) fn open(selector: &Selector, timeout_ms: u64) -> Result<Self> {
        let budget = EnumerationBudget::new(timeout_ms)?;
        selector.validate()?;
        let api = Api::load(&selector.producer)?;
        let mut opened = Self {
            api,
            tl: std::ptr::null_mut(),
            iface: std::ptr::null_mut(),
            device: std::ptr::null_mut(),
            port: std::ptr::null_mut(),
            stream: std::ptr::null_mut(),
            event: std::ptr::null_mut(),
            buffers: Vec::new(),
            running: false,
            sequence: 0,
            pixel_encodings: Vec::new(),
            multipart_projection: false,
            selected_part: selector.part,
            selected_format: selector.part_format,
            selected_gendc: selector.gendc,
            info: CameraInfo {
                id: selector.encode()?,
                transport: Transport::GenTl,
                vendor: String::new(),
                model: String::new(),
                serial: String::new(),
                address: None,
                profile: crate::camera_profile::CameraProfile::default(),
            },
        };
        check(
            invoke!(opened.api.tl_open, &raw mut opened.tl),
            "open system",
        )?;
        handle(opened.tl, "system")?;
        let mut changed = 0;
        check(
            invoke!(
                opened.api.tl_update,
                opened.tl,
                &raw mut changed,
                budget.wait_ms(2)?
            ),
            "update interfaces",
        )?;
        let interface = CString::new(selector.interface.as_str())?;
        check(
            invoke!(
                opened.api.tl_interface,
                opened.tl,
                interface.as_ptr(),
                &raw mut opened.iface
            ),
            "open interface",
        )?;
        handle(opened.iface, "interface")?;
        check(
            invoke!(
                opened.api.if_update,
                opened.iface,
                &raw mut changed,
                budget.wait_ms(1)?
            ),
            "update devices",
        )?;
        budget.remaining()?;
        opened.info = camera_info(&opened.api, opened.iface, selector)?;
        budget.remaining()?;
        let device = CString::new(selector.device.as_str())?;
        check(
            invoke!(
                opened.api.if_open,
                opened.iface,
                device.as_ptr(),
                3,
                &raw mut opened.device
            ),
            "open device for acquisition",
        )?;
        handle(opened.device, "device")?;
        check(
            invoke!(opened.api.dev_port, opened.device, &raw mut opened.port),
            "get remote port",
        )?;
        handle(opened.port, "remote port")?;
        let stream = if let Some(stream) = &selector.stream {
            stream.clone()
        } else {
            let mut count = 0;
            check(
                invoke!(opened.api.dev_count, opened.device, &raw mut count),
                "count streams",
            )?;
            ensure!(count > 0 && count <= 64, "invalid GenTL data-stream count");
            string(|text, length| invoke!(opened.api.dev_id, opened.device, 0, text, length))?
        };
        let stream = CString::new(stream)?;
        check(
            invoke!(
                opened.api.dev_stream,
                opened.device,
                stream.as_ptr(),
                &raw mut opened.stream
            ),
            "open data stream",
        )?;
        handle(opened.stream, "data stream")?;
        budget.remaining()?;
        Ok(opened)
    }
    pub(super) fn read(&self, address: u64, length: usize) -> Result<Vec<u8>> {
        ensure!(
            length <= MAX_XML && address.checked_add(u64::try_from(length)?).is_some(),
            "GenTL register read exceeds size or address limit"
        );
        let mut bytes = vec![0; length];
        for (index, chunk) in bytes.chunks_mut(64 * 1024).enumerate() {
            let offset = u64::try_from(index)?
                .checked_mul(64 * 1024)
                .and_then(|offset| address.checked_add(offset))
                .context("GenTL register address overflow")?;
            let mut actual = chunk.len();
            check(
                invoke!(
                    self.api.read,
                    self.port,
                    offset,
                    chunk.as_mut_ptr().cast(),
                    &raw mut actual
                ),
                "read remote port",
            )?;
            ensure!(actual == chunk.len(), "short GenTL remote-port read");
        }
        Ok(bytes)
    }
    pub(super) fn write(&self, address: u64, bytes: &[u8]) -> Result<()> {
        ensure!(
            !bytes.is_empty()
                && bytes.len() <= 64 * 1024
                && address.checked_add(u64::try_from(bytes.len())?).is_some(),
            "GenTL register write exceeds size or address limit"
        );
        let mut actual = bytes.len();
        check(
            invoke!(
                self.api.write,
                self.port,
                address,
                bytes.as_ptr().cast(),
                &raw mut actual
            ),
            "write remote port",
        )?;
        ensure!(actual == bytes.len(), "short GenTL remote-port write");
        Ok(())
    }
    pub(super) fn xml(&self, timeout: Duration) -> Result<Vec<u8>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("GenTL XML deadline overflow")?;
        let urls = url_list(|text, length| invoke!(self.api.url, self.port, text, length))?;
        let mut first_error = None;
        for (index, url) in urls.iter().enumerate() {
            let now = Instant::now();
            let remaining = deadline.saturating_duration_since(now);
            ensure!(
                !remaining.is_zero(),
                Categorized::error(
                    ErrorKind::Timeout,
                    "GenTL XML URLs exhausted their shared deadline"
                )
            );
            let pending = u32::try_from(urls.len() - index)?;
            let url_deadline = now
                .checked_add(remaining / pending)
                .context("GenTL XML URL deadline overflow")?;
            match self.xml_url(url, url_deadline) {
                Ok(data) => return Ok(data),
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        Err(first_error
            .context("GenTL device advertised no XML URLs")?
            .context("none of the advertised GenTL XML URLs could be read"))
    }
    fn xml_url(&self, url: &str, deadline: Instant) -> Result<Vec<u8>> {
        let Some((scheme, location)) = url.split_once(':') else {
            anyhow::bail!("malformed GenTL device XML URL")
        };
        if scheme.eq_ignore_ascii_case("local") {
            let pieces = location.split(';').collect::<Vec<_>>();
            ensure!(pieces.len() == 3, "malformed GenTL local XML URL");
            let address = u64::from_str_radix(pieces[1].trim_start_matches("0x"), 16)?;
            let length = usize::from_str_radix(
                pieces[2]
                    .split(['?', '#'])
                    .next()
                    .unwrap_or("")
                    .trim_start_matches("0x"),
                16,
            )?;
            ensure!(
                length > 0 && length <= MAX_XML,
                "GenTL device XML exceeds16 MiB"
            );
            return self.read(address, length);
        }
        if scheme.eq_ignore_ascii_case("file") {
            let path = file_path(location)?;
            use std::io::Read as _;
            let mut data = Vec::new();
            std::fs::File::open(&path)?
                .take(u64::try_from(MAX_XML + 1)?)
                .read_to_end(&mut data)?;
            ensure!(data.len() <= MAX_XML, "GenTL device XML exceeds16 MiB");
            return Ok(data);
        }
        if scheme.eq_ignore_ascii_case("http") {
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                Categorized::error(
                    ErrorKind::Timeout,
                    "GenTL XML URL lookup exhausted its deadline"
                )
            );
            let (_, data) = super::super::gige::fetch_xml_http(url, remaining)?;
            return Ok(data);
        }
        Err(Categorized::error(
            ErrorKind::Unsupported,
            "GenTL device XML requires a Local:, absolute file: or plain HTTP URL; HTTPS and authenticated XML retrieval are unsupported",
        ))
    }
    fn timestamp_frequency_hz(&self) -> Result<Option<u64>> {
        let Some(get) = self.api.dev_info else {
            return Ok(None);
        };
        Ok(optional_scalar::<u64>(U64, |kind, bytes, length| {
            invoke!(get, self.device, 9, kind, bytes, length)
        })?
        .filter(|hz| *hz > 0))
    }
    fn ds<T: Default + Copy>(&self, code: i32, kind: i32) -> Result<T> {
        scalar(kind, |datatype, bytes, length| {
            invoke!(self.api.ds_info, self.stream, code, datatype, bytes, length)
        })
    }
    fn buffer<T: Default + Copy>(&self, buffer: Handle, code: i32, kind: i32) -> Result<T> {
        scalar(kind, |datatype, bytes, length| {
            invoke!(
                self.api.buffer_info,
                self.stream,
                buffer,
                code,
                datatype,
                bytes,
                length
            )
        })
    }
    fn ds_optional<T: Default + Copy>(&self, code: i32, kind: i32) -> Result<Option<T>> {
        optional_scalar(kind, |datatype, bytes, length| {
            invoke!(self.api.ds_info, self.stream, code, datatype, bytes, length)
        })
    }
    fn buffer_optional<T: Default + Copy>(
        &self,
        buffer: Handle,
        code: i32,
        kind: i32,
    ) -> Result<Option<T>> {
        optional_scalar(kind, |datatype, bytes, length| {
            invoke!(
                self.api.buffer_info,
                self.stream,
                buffer,
                code,
                datatype,
                bytes,
                length
            )
        })
    }
    pub(super) fn configure_pixel_encodings(
        &mut self,
        encodings: &[crate::types::PixelEncoding],
    ) -> Result<()> {
        ensure!(
            !self.running
                && encodings.len()
                    <= crate::types::IdsPixelFormat::ALL.len()
                        + crate::types::LucidPixelFormat::ALL.len(),
            "invalid GenTL pixel encoding configuration"
        );
        for &encoding in encodings {
            let (vendor, what) = match encoding {
                crate::types::PixelEncoding::Ids { .. } => (
                    crate::types::IdsPixelFormat::vendor_matches(&self.info.vendor),
                    "IDS encoding requires an IDS device identity",
                ),
                crate::types::PixelEncoding::Lucid { .. } => (
                    crate::types::LucidPixelFormat::vendor_matches(&self.info.vendor),
                    "LUCID encoding requires a LUCID device identity",
                ),
            };
            ensure!(vendor, Categorized::error(ErrorKind::Unsupported, what));
            encoding.check(encoding.code())?;
            ensure!(
                encoding.namespace().is_none(),
                "pixel encoding configuration must not infer a producer namespace"
            );
        }
        self.pixel_encodings = encodings.to_vec();
        Ok(())
    }
    pub(super) fn stream_payload_size(&self) -> Result<Option<usize>> {
        // GenTL §5.2.1: a producer that defines its own payload supplies the
        // required allocation size. Legacy missing information uses the camera.
        if self.ds_optional::<u8>(9, BOOL)?.unwrap_or(0) == 0 {
            return Ok(None);
        }
        let size = self.ds::<usize>(7, SIZET)?;
        ensure!(
            (1..=MAX_FRAME).contains(&size),
            "GenTL stream payload size is zero or exceeds 128 MiB"
        );
        Ok(Some(size))
    }
    pub(super) fn enable_multipart_projection(&mut self) -> Result<()> {
        ensure!(
            self.selected_gendc.is_none(),
            "per-job multipart selection cannot override a GenDC binding"
        );
        self.multipart_projection = true;
        Ok(())
    }

    pub(super) fn start(&mut self, size: usize) -> Result<()> {
        self.stop()?;
        let result = self.begin(size);
        if result.is_err() {
            let _ = self.stop();
        }
        result
    }
    fn begin(&mut self, size: usize) -> Result<()> {
        ensure!(
            size > 0 && size <= MAX_FRAME,
            "GenTL payload exceeds128 MiB"
        );
        let count = self.ds_optional::<usize>(12, SIZET)?.unwrap_or(2).max(2);
        let budget = crate::host::memory_budget().unwrap_or(2 * 1024 * 1024 * 1024) / 4;
        ensure!(
            count <= MAX_BUFFERS
                && u64::try_from(size)?
                    .checked_mul(u64::try_from(count)?)
                    .is_some_and(|total| total <= budget),
            "GenTL required buffers exceed the bounded memory budget"
        );
        let mut allocated = 0u64;
        for _ in 0..count {
            let mut buffer = std::ptr::null_mut();
            check(
                invoke!(
                    self.api.ds_alloc,
                    self.stream,
                    size,
                    std::ptr::null_mut(),
                    &raw mut buffer
                ),
                "allocate acquisition buffer",
            )?;
            handle(buffer, "buffer")?;
            self.buffers.push(Buffer {
                handle: buffer,
                base: 0,
                size: 0,
            });
            let base = self.buffer::<usize>(buffer, 0, PTR)?;
            let actual = self.buffer::<usize>(buffer, 1, SIZET)?;
            ensure!(
                base != 0
                    && actual >= size
                    && actual <= MAX_FRAME
                    && base.checked_add(actual).is_some(),
                "GenTL allocated buffer has invalid base or size"
            );
            allocated = allocated
                .checked_add(u64::try_from(actual)?)
                .context("GenTL allocated buffer budget overflow")?;
            ensure!(
                allocated <= budget,
                "GenTL actual buffer allocations exceed the bounded memory budget"
            );
            if let Some(last) = self.buffers.last_mut() {
                last.base = base;
                last.size = actual;
            }
            check(
                invoke!(self.api.ds_queue, self.stream, buffer),
                "queue acquisition buffer",
            )?;
        }
        check(
            invoke!(self.api.event_register, self.stream, 1, &raw mut self.event),
            "register new-buffer event",
        )?;
        handle(self.event, "event")?;
        check(
            invoke!(self.api.ds_start, self.stream, 0, u64::MAX),
            "start stream acquisition",
        )?;
        self.running = true;
        Ok(())
    }
    fn chunk_layout(
        &self,
        buffer: Handle,
        filled: usize,
        contains: Option<bool>,
        legacy: bool,
    ) -> Result<Option<wire::ChunkLayout>> {
        ensure!(
            !legacy || contains != Some(false),
            Categorized::error(
                ErrorKind::Incomplete,
                "GenTL legacy chunk payload contradicts its chunk-present flag"
            )
        );
        let described = |entries, descriptor_error| -> Result<Option<wire::ChunkLayout>> {
            Ok(Some(wire::ChunkLayout {
                layout_id: self
                    .buffer_optional::<u64>(buffer, 24, U64)?
                    .filter(|id| *id != 0),
                entries,
                descriptor_error,
            }))
        };
        let declared = contains == Some(true) || legacy;
        let Some(get) = self.api.buffer_chunks else {
            return if declared {
                described(Vec::new(), Some("GenTL producer does not export DSGetBufferChunkData; original payload retained without descriptor interpretation".into()))
            } else {
                Ok(None)
            };
        };
        let mut count = 0;
        let status = invoke!(
            get,
            self.stream,
            buffer,
            std::ptr::null_mut(),
            &raw mut count
        );
        if matches!(status, -1003 | -1008 | -1014) {
            return if declared {
                described(
                    Vec::new(),
                    Some(format!(
                        "GenTL chunk descriptors unavailable (error {status}); original payload retained"
                    )),
                )
            } else {
                Ok(None)
            };
        }
        if status != TOO_SMALL {
            check(status, "query chunk descriptor count")?;
        }
        ensure!(count <= 4096, "GenTL chunk descriptor count exceeds4096");
        if count == 0 {
            ensure!(
                !declared,
                Categorized::error(
                    ErrorKind::Incomplete,
                    "GenTL chunk-present flag contradicts its successful zero descriptor count"
                )
            );
            return Ok(None);
        }
        ensure!(
            contains != Some(false),
            Categorized::error(
                ErrorKind::Incomplete,
                "GenTL chunk descriptors contradict its chunk-present flag"
            )
        );
        let mut raw = vec![RawChunk::default(); count];
        let mut actual = count;
        let status = invoke!(get, self.stream, buffer, raw.as_mut_ptr(), &raw mut actual);
        if matches!(status, -1003 | -1008 | -1014) {
            return described(
                Vec::new(),
                Some(format!(
                    "GenTL chunk descriptors unavailable (error {status}); original payload retained"
                )),
            );
        }
        check(status, "read chunk descriptors")?;
        ensure!(
            actual == count,
            "GenTL returned an invalid chunk descriptor count"
        );
        let mut entries = Vec::with_capacity(actual);
        for raw in raw.into_iter().take(actual) {
            let offset = usize::try_from(raw.offset).context("GenTL chunk offset is negative")?;
            let length = raw.length;
            ensure!(
                offset.checked_add(length).is_some_and(|end| end <= filled),
                "GenTL chunk range exceeds its filled buffer"
            );
            entries.push(crate::transport::chunks::Chunk {
                id: raw.id,
                offset,
                length,
            });
        }
        described(entries, None)
    }
    fn part_optional<T: Default + Copy>(
        &self,
        get: PartInfo,
        buffer: Handle,
        index: u32,
        code: i32,
        kind: i32,
    ) -> Result<Option<T>> {
        optional_scalar(kind, |datatype, bytes, length| {
            invoke!(
                get,
                self.stream,
                buffer,
                index,
                code,
                datatype,
                bytes,
                length
            )
        })
    }
    fn multipart_selection_error(&self, message: &str, layout: &Layout) -> anyhow::Error {
        let (choices, truncated) = Selector::parse(&self.info.id)
            .map(|selector| selector.image_selectors(layout, |_, _| true))
            .unwrap_or_default();
        let mut details = crate::types::without_nulls(
            json!({"parts":layout.parts,"gendc":layout.gendc,"byte_order":layout.byte_order,"image_selectors":choices}),
        );
        if truncated > 0 {
            details["image_selectors_truncated"] = json!(truncated);
        }
        Categorized::new(ErrorKind::Unsupported, message)
            .details(details)
            .into()
    }

    fn multipart_next(
        &self,
        buffer: Handle,
        known_base: usize,
        known_size: usize,
    ) -> Result<FrameView<'_>> {
        let unsupported = |message| Categorized::error(ErrorKind::Unsupported, message);
        ensure!(
            self.buffer_optional::<u8>(buffer, 31, BOOL)?.unwrap_or(0) == 0,
            unsupported("GenTL composite multipart buffers are unsupported")
        );
        ensure!(
            self.buffer_optional::<u8>(buffer, 29, BOOL)?.unwrap_or(0) == 0,
            Categorized::error(
                ErrorKind::Incomplete,
                "GenTL multipart exceeds its announced buffer"
            )
        );
        let chunks = self.buffer_optional::<u8>(buffer, 30, BOOL)?;
        ensure!(
            chunks == Some(0),
            unsupported("GenTL multipart capture requires an explicit chunk-absent flag")
        );
        let base = self.buffer::<usize>(buffer, 0, PTR)?;
        let filled = self.buffer::<usize>(buffer, 9, SIZET)?;
        ensure!(
            base == known_base && filled > 0 && filled <= known_size,
            "invalid GenTL multipart buffer range"
        );
        ensure!(
            self.chunk_layout(buffer, filled, Some(false), false)?
                .is_none(),
            "GenTL multipart contains unsupported chunks"
        );
        let count_parts = self
            .api
            .parts_count
            .ok_or_else(|| unsupported("GenTL producer does not export DSGetNumBufferParts"))?;
        let get = self
            .api
            .part_info
            .ok_or_else(|| unsupported("GenTL producer does not export DSGetBufferPartInfo"))?;
        let mut count = 0;
        check(
            invoke!(count_parts, self.stream, buffer, &raw mut count),
            "query multipart count",
        )?;
        ensure!(
            (1..=MAX_PARTS).contains(&usize::try_from(count)?),
            unsupported("GenTL multipart part count is outside 1..32")
        );
        let mut parts = Vec::with_capacity(usize::try_from(count)?);
        let mut stored = 0usize;
        for index in 0..count {
            let pointer = scalar::<usize>(PTR, |kind, bytes, length| {
                invoke!(get, self.stream, buffer, index, 0, kind, bytes, length)
            })?;
            let length = scalar::<usize>(SIZET, |kind, bytes, size| {
                invoke!(get, self.stream, buffer, index, 1, kind, bytes, size)
            })?;
            let original_offset = pointer
                .checked_sub(base)
                .context("GenTL part precedes its announced buffer")?;
            ensure!(
                original_offset
                    .checked_add(length)
                    .is_some_and(|end| end <= known_size),
                "GenTL multipart part exceeds its announced buffer"
            );
            let data_type = scalar::<usize>(SIZET, |kind, bytes, length| {
                invoke!(get, self.stream, buffer, index, 2, kind, bytes, length)
            })?;
            parts.push(Part {
                index,
                original_offset: Some(original_offset),
                stored_offset: stored,
                length,
                data_type,
                format: self.part_optional::<u64>(get, buffer, index, 3, U64)?,
                namespace: self.part_optional::<u64>(get, buffer, index, 4, U64)?,
                width: self.part_optional::<usize>(get, buffer, index, 5, SIZET)?,
                height: self.part_optional::<usize>(get, buffer, index, 6, SIZET)?,
                delivered_height: self.part_optional::<usize>(get, buffer, index, 11, SIZET)?,
                x_offset: self.part_optional::<usize>(get, buffer, index, 7, SIZET)?,
                y_offset: self.part_optional::<usize>(get, buffer, index, 8, SIZET)?,
                x_padding_pixels: self.part_optional::<usize>(get, buffer, index, 9, SIZET)?,
                source_id: self.part_optional::<u64>(get, buffer, index, 10, U64)?,
                region_id: self.part_optional::<u64>(get, buffer, index, 12, U64)?,
                purpose_id: self.part_optional::<u64>(get, buffer, index, 13, U64)?,
                gvsp: None,
                gendc: None,
            });
            stored = stored
                .checked_add(length)
                .context("GenTL multipart archive size overflow")?;
            ensure!(
                stored <= MAX_FRAME,
                "GenTL multipart archive exceeds128 MiB"
            );
        }
        let endian = self.buffer_optional::<i32>(buffer, 26, I32)?.unwrap_or(0);
        ensure!(
            matches!(endian, 0..=2),
            "invalid multipart pixel byte order"
        );
        let byte_order = match endian {
            1 => Some(ByteOrder::Little),
            2 => Some(ByteOrder::Big),
            _ => None,
        };
        let selected = self.selected_part.or_else(|| {
            let mut images = parts.iter().filter(|part| {
                part.data_type == 1 && part.length > 0 && part.measurement_format().is_none()
            });
            let first = images.next()?;
            images.next().is_none().then_some(first.index)
        });
        let selection_required = selected.is_none() && self.multipart_projection;
        let selected = selected.or_else(|| {
            if !selection_required {
                return None;
            }
            parts
                .iter()
                .find(|part| {
                    super::super::multipart::GvspPartFormat::for_part(part).is_some()
                        && part.image_shape(byte_order).is_ok()
                })
                .map(|part| part.index)
        });
        let layout = Layout {
            selection_required,
            projected_format: None,
            data_type_namespace: None,
            byte_order,
            selected: selected.unwrap_or_default(),
            original_buffer_bytes: Some(known_size),
            original_filled_bytes: filled,
            payload_bytes: stored,
            parts,
            gendc: None,
        };
        layout.validate_ranges(stored)?;
        if self.selected_gendc.is_some() {
            return Err(
                self.multipart_selection_error("GenDC selection requires a GenDC payload", &layout)
            );
        }
        if selected.is_none() {
            return Err(self.multipart_selection_error(
                "GenTL multipart requires an explicit image selector when there is not exactly one ordinary 2D image", &layout));
        }
        let Some(part) = layout.parts.get(usize::try_from(layout.selected)?) else {
            return Err(self
                .multipart_selection_error("the selected multipart part is unavailable", &layout));
        };
        let (width, delivered, pixel_format) = match part.image_shape(byte_order) {
            Err(error) if crate::types::classify(&error).kind == ErrorKind::Unsupported => {
                return Err(self.multipart_selection_error(&error.to_string(), &layout));
            }
            other => other?,
        };
        ensure!(
            selection_required || self.selected_format == part.measurement_format(),
            self.multipart_selection_error(
                "GenTL measurement components require a selector for the delivered part and PFNC format",
                &layout,
            )
        );
        let mut bytes = Vec::with_capacity(layout.parts.len());
        for part in &layout.parts {
            if part.length > 0 {
                let pointer = base
                    .checked_add(
                        part.original_offset
                            .context("GenTL part offset is missing")?,
                    )
                    .context("multipart address overflow")?;
                // SAFETY: each range was checked within this announced SDK allocation.
                // The producer owns it until all borrowed part writes finish and requeue.
                bytes
                    .push(unsafe { std::slice::from_raw_parts(pointer as *const u8, part.length) });
            }
        }
        Ok(FrameView {
            image: wire::Image {
                pixel_encoding: None,
                id: self
                    .buffer_optional::<u64>(buffer, 16, U64)?
                    .unwrap_or(self.sequence),
                width,
                height: delivered,
                pixel_format,
                device_ts_ns: self.buffer_optional::<u64>(buffer, 28, U64)?,
                device_timestamp_ticks: self.buffer_optional::<u64>(buffer, 3, U64)?,
                device_timestamp_frequency_hz: self.timestamp_frequency_hz()?,
                offset: 0,
                x_padding: 0,
                y_padding: 0,
                byte_order,
                chunks: None,
                multipart: Some(layout),
            },
            bytes,
            handle: buffer,
        })
    }
    fn gendc_next(&self, buffer: Handle, known: &Buffer) -> Result<FrameView<'_>> {
        ensure!(
            self.buffer_optional::<u8>(buffer, 31, BOOL)?.unwrap_or(0) == 0,
            Categorized::error(ErrorKind::Unsupported, "GenDC requires a contiguous buffer")
        );
        ensure!(
            self.buffer_optional::<u8>(buffer, 29, BOOL)?.unwrap_or(0) == 0,
            Categorized::error(ErrorKind::Incomplete, "GenDC exceeds its announced buffer")
        );
        let base = self.buffer::<usize>(buffer, 0, PTR)?;
        let written = self.buffer::<usize>(buffer, 9, SIZET)?;
        ensure!(
            base == known.base && (gendc::HEADER_BYTES..=known.size).contains(&written),
            "invalid GenDC producer buffer"
        );
        let fill = gendc::Fill {
            announced: known.size,
            written,
            intended: self.buffer_optional::<usize>(buffer, 27, SIZET)?,
        };
        // SAFETY: a completed GenTL GenDC buffer begins with a final descriptor.
        // The announced allocation was checked, and ownership lasts until requeue.
        let prefix = unsafe { std::slice::from_raw_parts(base as *const u8, gendc::HEADER_BYTES) };
        let descriptor_bytes = gendc::descriptor_size(prefix, fill)?;
        // SAFETY: the declared final descriptor fits within the checked allocation
        // and the producer reported at least this many bytes written.
        let descriptor = unsafe { std::slice::from_raw_parts(base as *const u8, descriptor_bytes) };
        let mut layout = gendc::parse(descriptor, fill)?;
        let container = layout
            .gendc
            .as_ref()
            .context("missing GenDC container metadata")?;
        if self.selected_part.is_some() {
            return Err(self.multipart_selection_error(
                "GenTL part selection requires a multipart payload",
                &layout,
            ));
        }
        let selected = if let Some(expected) = self.selected_gendc {
            layout
                .parts
                .iter()
                .find(|part| container.selection(part) == Some(expected))
        } else {
            let mut choices = layout
                .parts
                .iter()
                .filter(|part| container.ordinary_intensity(part));
            // Keep a previously selectable unpadded image as the default.
            let mut unpadded = choices.clone().filter(|part| !part.has_gendc_padding());
            unpadded.next().map_or_else(
                || choices.next().filter(|_| choices.next().is_none()),
                |first| unpadded.next().is_none().then_some(first),
            )
        };
        let Some(selected) = selected else {
            return Err(self.multipart_selection_error(
                "GenDC requires a matching explicit component or exactly one supported intensity image",
                &layout,
            ));
        };
        let (width, height, pixel_format) = match layout.image_shape(selected) {
            Err(error) if crate::types::classify(&error).kind == ErrorKind::Unsupported => {
                return Err(self.multipart_selection_error(&error.to_string(), &layout));
            }
            other => other?,
        };
        layout.selected = selected.index;
        // SAFETY: the validated descriptor and disjoint part ranges account for
        // every byte in this prefix, with no reads of allocation gaps or spare tail.
        let container = unsafe { std::slice::from_raw_parts(base as *const u8, written) };
        Ok(FrameView {
            image: wire::Image {
                pixel_encoding: None,
                id: self
                    .buffer_optional::<u64>(buffer, 16, U64)?
                    .unwrap_or(self.sequence),
                width,
                height,
                pixel_format,
                device_ts_ns: self.buffer_optional::<u64>(buffer, 28, U64)?,
                device_timestamp_ticks: self.buffer_optional::<u64>(buffer, 3, U64)?,
                device_timestamp_frequency_hz: self.timestamp_frequency_hz()?,
                offset: 0,
                x_padding: 0,
                y_padding: 0,
                byte_order: layout.byte_order,
                chunks: None,
                multipart: Some(layout),
            },
            bytes: vec![container],
            handle: buffer,
        })
    }

    pub(super) fn next(&mut self, timeout_ms: u64) -> Result<FrameView<'_>> {
        ensure!(self.running, "GenTL stream is not running");
        let mut event = [0usize; 2];
        let mut length = size_of_val(&event);
        let event_result = invoke!(
            self.api.event_get,
            self.event,
            event.as_mut_ptr().cast(),
            &raw mut length,
            timeout_ms
        );
        if event_result == -1011 {
            return Err(crate::types::FrameWait.into());
        }
        check(event_result, "wait for image")?;
        ensure!(
            length == size_of_val(&event),
            "GenTL new-buffer event has an invalid ABI size"
        );
        let buffer = event[0] as Handle;
        let known = self
            .buffers
            .iter()
            .find(|known| known.handle == buffer)
            .context("GenTL event references an unknown buffer")?;
        let ready = self.ready_payload(buffer).and_then(|payload| {
            let sequence = self
                .sequence
                .checked_add(1)
                .context("GenTL frame counter exhausted")?;
            Ok((payload, sequence))
        });
        let frame = match ready {
            Ok((payload, sequence)) => {
                self.sequence = sequence;
                self.view_buffer(buffer, known, payload)
            }
            Err(error) => Err(error),
        };
        // No slice is returned on error, so the producer can reuse the
        // buffer now. Successful frames are released after the pipe write.
        if frame.is_err() {
            self.release(buffer)?;
        }
        frame
    }
    fn ready_payload(&self, buffer: Handle) -> Result<usize> {
        if self.buffer::<u8>(buffer, 7, BOOL)? != 0
            || self.buffer_optional::<u8>(buffer, 4, BOOL)?.unwrap_or(1) == 0
        {
            return Err(crate::types::FrameWait.into());
        }
        let payload = self.buffer::<usize>(buffer, 19, SIZET)?;
        ensure!(
            matches!(payload, 1 | 4 | 10 | 11),
            Categorized::error(
                ErrorKind::Unsupported,
                format!(
                    "GenTL payload type{payload} requires IMAGE, indexed multipart or linear GenDC data"
                )
            )
        );
        Ok(payload)
    }
    fn view_buffer(&self, buffer: Handle, known: &Buffer, payload: usize) -> Result<FrameView<'_>> {
        if payload == 10 {
            return self.multipart_next(buffer, known.base, known.size);
        }
        if payload == 11 {
            return self.gendc_next(buffer, known);
        }
        ensure!(
            self.selected_part.is_none() && self.selected_gendc.is_none(),
            Categorized::error(
                ErrorKind::Unsupported,
                "GenTL component selection requires a matching multipart or GenDC payload"
            )
        );
        let image_present = self
            .buffer_optional::<u8>(buffer, 17, BOOL)?
            .map(|value| value != 0);
        ensure!(
            image_present != Some(false),
            Categorized::error(ErrorKind::Incomplete, "GenTL buffer contains no image")
        );
        ensure!(
            payload != 4 || image_present == Some(true),
            Categorized::error(
                ErrorKind::Unsupported,
                "GenTL legacy chunk payload requires an explicit image-present flag"
            )
        );
        let contains_chunks = self
            .buffer_optional::<u8>(buffer, 30, BOOL)?
            .map(|value| value != 0);
        ensure!(
            self.buffer_optional::<u8>(buffer, 29, BOOL)?.unwrap_or(0) == 0,
            Categorized::error(ErrorKind::Incomplete, "GenTL image exceeds its buffer")
        );
        ensure!(
            self.buffer_optional::<u8>(buffer, 31, BOOL)?.unwrap_or(0) == 0,
            Categorized::error(
                ErrorKind::Unsupported,
                "GenTL composite buffers require an explicit segment layout"
            )
        );
        let base = self.buffer::<usize>(buffer, 0, PTR)?;
        let filled = self.buffer::<usize>(buffer, 9, SIZET)?;
        ensure!(
            base == known.base && filled > 0 && filled <= known.size,
            "GenTL returned buffer range exceeds its allocation"
        );
        let chunks = self.chunk_layout(buffer, filled, contains_chunks, payload == 4)?;
        let format = self.buffer::<u64>(buffer, 20, U64)?;
        let namespace = self.buffer::<u64>(buffer, 21, U64)?;
        let pixel_format = u32::try_from(format).context("GenTL pixel format exceeds32 bits")?;
        let pixel_encoding = self.pixel_encodings.iter().find_map(|encoding| {
            encoding.check(pixel_format).ok()?;
            Some(encoding.with_namespace(Some(namespace)))
        });
        let lucid = matches!(
            pixel_encoding,
            Some(crate::types::PixelEncoding::Lucid { .. })
        );
        // GenTL1.6 §6.4.4.6 starts producer-specific namespace IDs at1000.
        // This closed profile recognizes only1000; higher custom IDs need separate evidence.
        // Only the independently qualified IDS XML layout supplies its interpretation.
        ensure!(
            matches!(namespace, 1 | 4) || (namespace == 1000 && pixel_encoding.is_some()),
            Categorized::error(
                ErrorKind::Unsupported,
                "GenTL pixel namespace requires a standard format or verified IDS custom layout"
            )
        );
        ensure!(
            pixel_format & 0x8000_0000 == 0 || lucid,
            Categorized::error(
                ErrorKind::Unsupported,
                "GenTL vendor-specific pixel codes require an explicit vendor layout"
            )
        );
        ensure!(
            (pixel_format >> 16) & 0xff > 0 || pixel_encoding.is_some(),
            Categorized::error(
                ErrorKind::Unsupported,
                "GenTL pixel code has no qualified byte layout"
            )
        );
        // GenTL1.6 section6.4.4.7 defines0 as unknown, not native byte order.
        // Keep it unknown for byte/packed pixels; reject unknown wide samples.
        let endian = self.buffer_optional::<i32>(buffer, 26, I32)?.unwrap_or(0);
        ensure!(matches!(endian, 0..=2), "invalid GenTL pixel byte order");
        // LUCID documents each 16-bit word low byte first; unknown order is not inferred.
        ensure!(
            !lucid || endian == 1,
            Categorized::error(
                ErrorKind::Unsupported,
                "LUCID ABCY16 requires producer little-endian metadata"
            )
        );
        if let Some(encoding) = pixel_encoding {
            encoding.check(pixel_format)?;
        }
        ensure!(
            pixel_encoding.is_none() || lucid || endian != 2,
            Categorized::error(
                ErrorKind::Unsupported,
                "IDS grouped layout contradicts producer big-endian metadata"
            )
        );
        ensure!(
            endian != 2 || crate::frame::packed_coordinate_group(pixel_format).is_none(),
            Categorized::error(
                ErrorKind::Unsupported,
                "GenTL packed coordinates require little-endian packing"
            )
        );
        ensure!(
            endian == 1
                || (pixel_format >> 16) & 0xff <= 8
                || crate::frame::pixel_format_name(pixel_format) != "Unknown",
            Categorized::error(
                ErrorKind::Unsupported,
                "GenTL unknown multi-byte pixel format cannot be normalized without a known byte order and packing unit"
            )
        );
        if super::word_bytes(pixel_format).is_some() {
            ensure!(
                endian != 0,
                Categorized::error(
                    ErrorKind::Unsupported,
                    "GenTL wide-sample byte order is not available"
                )
            );
        }
        let width = u32::try_from(self.buffer::<usize>(buffer, 10, SIZET)?)?;
        let height = u32::try_from(self.buffer::<usize>(buffer, 11, SIZET)?)?;
        if let Some(format) = crate::frame::bicolor::Format::from_code(pixel_format) {
            format.validate_width(width)?;
        }
        if let Some(Ok(ids)) = pixel_encoding.map(|encoding| encoding.ids(pixel_format)) {
            ensure!(
                width.is_multiple_of(ids.group().0),
                Categorized::error(
                    ErrorKind::Unsupported,
                    "IDS grouped decoding requires complete byte-aligned rows"
                )
            );
        }
        let delivered = u32::try_from(
            self.buffer_optional::<usize>(buffer, 22, SIZET)?
                .unwrap_or(usize::try_from(height)?),
        )?;
        ensure!(
            delivered > 0 && delivered <= height,
            "invalid GenTL delivered image height"
        );
        let image = wire::Image {
            id: self
                .buffer_optional::<u64>(buffer, 16, U64)?
                .unwrap_or(self.sequence),
            width,
            height: delivered,
            pixel_format,
            pixel_encoding,
            device_ts_ns: self.buffer_optional::<u64>(buffer, 28, U64)?,
            device_timestamp_ticks: self.buffer_optional::<u64>(buffer, 3, U64)?,
            device_timestamp_frequency_hz: self.timestamp_frequency_hz()?,
            offset: self.buffer::<usize>(buffer, 18, SIZET)?,
            x_padding: self.buffer::<usize>(buffer, 14, SIZET)?,
            y_padding: self.buffer::<usize>(buffer, 15, SIZET)?,
            byte_order: match endian {
                1 => Some(ByteOrder::Little),
                2 => Some(ByteOrder::Big),
                _ => None,
            },
            chunks,
            multipart: None,
        };
        let pixels = crate::frame::image_len(pixel_format, pixel_encoding, width, delivered)
            .context("GenTL image dimensions overflow")?;
        let image_span = image
            .x_padding
            .checked_mul(usize::try_from(delivered)?)
            .and_then(|padding| pixels.checked_add(padding))
            .and_then(|bytes| bytes.checked_add(image.y_padding))
            .context("GenTL image span overflow")?;
        let image_end = image
            .offset
            .checked_add(image_span)
            .context("GenTL image offset overflow")?;
        ensure!(
            width > 0 && image_span > 0 && image_end <= filled,
            "GenTL image range exceeds its filled buffer"
        );
        ensure!(
            image.chunks.is_some() || image_end == filled,
            "GenTL IMAGE buffer has unexplained trailing bytes"
        );
        // SAFETY: the conforming native producer owns this validated announced
        // region until release; the helper sends all bytes before requeuing it.
        let bytes = unsafe { std::slice::from_raw_parts(base as *const u8, filled) };
        Ok(FrameView {
            image,
            bytes: vec![bytes],
            handle: buffer,
        })
    }
    pub(super) fn release(&self, buffer: Handle) -> Result<()> {
        check(
            invoke!(self.api.ds_queue, self.stream, buffer),
            "requeue delivered buffer",
        )
    }
    pub(super) fn stop(&mut self) -> Result<()> {
        let mut failure = None;
        if self.running {
            failure = check(
                invoke!(self.api.ds_stop, self.stream, 1),
                "stop stream acquisition",
            )
            .err();
            self.running = false;
        }
        if !self.event.is_null() {
            if let Err(error) = check(
                invoke!(self.api.event_unregister, self.stream, 1),
                "unregister buffer event",
            ) {
                failure.get_or_insert(error);
            }
            self.event = std::ptr::null_mut();
        }
        if !self.buffers.is_empty() {
            if let Err(error) = check(
                invoke!(self.api.ds_flush, self.stream, 4),
                "flush acquisition buffers",
            ) {
                failure.get_or_insert(error);
            }
            for buffer in self.buffers.drain(..) {
                let mut base = std::ptr::null_mut();
                let mut private = std::ptr::null_mut();
                if let Err(error) = check(
                    invoke!(
                        self.api.ds_revoke,
                        self.stream,
                        buffer.handle,
                        &raw mut base,
                        &raw mut private
                    ),
                    "revoke acquisition buffer",
                ) {
                    failure.get_or_insert(error);
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }
}
impl Drop for Device {
    fn drop(&mut self) {
        let _ = self.stop();
        if !self.stream.is_null() {
            let _ = invoke!(self.api.ds_close, self.stream);
        }
        if !self.device.is_null() {
            let _ = invoke!(self.api.dev_close, self.device);
        }
        if !self.iface.is_null() {
            let _ = invoke!(self.api.if_close, self.iface);
        }
        if !self.tl.is_null() {
            let _ = invoke!(self.api.tl_close, self.tl);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EnumerationBudget, RawChunk, file_path, parse_url_list};
    #[test]
    fn chunk_descriptor_abi_is_byte_packed_with_platform_sized_offsets() {
        assert_eq!(align_of::<RawChunk>(), 1);
        assert_eq!(size_of::<RawChunk>(), 8 + 2 * size_of::<usize>());
        assert_eq!(std::mem::offset_of!(RawChunk, id), 0);
        assert_eq!(std::mem::offset_of!(RawChunk, offset), 8);
        assert_eq!(
            std::mem::offset_of!(RawChunk, length),
            8 + size_of::<usize>()
        );
    }
    #[test]
    fn url_lists_accept_spec_double_nuls_and_legacy_single_nul_but_bound_entries() {
        assert_eq!(
            parse_url_list(b"local:one;0;1\0\0").unwrap(),
            ["local:one;0;1"]
        );
        assert_eq!(
            parse_url_list(b"local:one;0;1\0").unwrap(),
            ["local:one;0;1"]
        );
        assert_eq!(
            parse_url_list(b"https://vendor.invalid/one\0local:two;0;1\0\0").unwrap(),
            ["https://vendor.invalid/one", "local:two;0;1"]
        );
        for bytes in [
            b"".as_slice(),
            b"unterminated",
            b"\0\0",
            b"first\0\0hidden\0\0",
        ] {
            parse_url_list(bytes).unwrap_err();
        }
        parse_url_list(&b"local:x\0".repeat(9)).unwrap_err();
    }
    #[test]
    fn enumeration_waits_are_bounded_and_zero_budget_is_rejected() {
        let budget = EnumerationBudget::new(1000).unwrap();
        assert!(budget.wait_ms(1).unwrap() <= 500);
        assert!(budget.wait_ms(64).unwrap() <= 15);
        EnumerationBudget::new(0).unwrap().wait_ms(1).unwrap_err();
    }
    #[test]
    fn local_xml_file_urls_decode_escapes_and_reject_remote_or_invalid_paths() {
        #[cfg(unix)]
        {
            assert_eq!(
                file_path("///tmp/sdk%20cache/%CE%BC.xml?SchemaVersion=1.1").unwrap(),
                std::path::Path::new("/tmp/sdk cache/μ.xml")
            );
            assert_eq!(
                file_path("//localhost/tmp/test.xml").unwrap(),
                std::path::Path::new("/tmp/test.xml")
            );
        }
        #[cfg(windows)]
        {
            assert_eq!(
                file_path("///C:/Program%20Files/camera.xml")
                    .unwrap()
                    .to_string_lossy(),
                "C:/Program Files/camera.xml"
            );
            assert_eq!(
                file_path("//localhost/C:/Program%20Files/camera.xml")
                    .unwrap()
                    .to_string_lossy(),
                "C:/Program Files/camera.xml"
            );
        }
        file_path("//remote-host/share/camera.xml").unwrap_err();
        file_path("relative/camera.xml").unwrap_err();
        file_path("///tmp/camera%00.xml").unwrap_err();
        file_path("///tmp/camera%2.xml").unwrap_err();
    }
}
