//! NVIDIA nvJPEG on a CUDA GPU, in the JPEG helper process (`helper.rs`).
//!
//! The helper loads the CUDA runtime and nvJPEG by their versioned sonames
//! (`CAPTUREFAB_CUDA_LIB_DIR` first, then the loader's paths and the CUDA
//! toolkit's), so a build needs no CUDA SDK. Jetson's Multimedia API ships
//! a different, libjpeg-style `libnvjpeg.so`; unversioned names are tried
//! only in `CAPTUREFAB_CUDA_LIB_DIR`, and a library without
//! `nvjpegCreateSimple` is not CUDA's.
//!
//! Frames nvJPEG reads as they are (RGB8, BGR8, Mono8) are read from the
//! segment the camera wrote them to, page-locked once (`cudaHostRegister`)
//! the first time one is: a discrete GPU copies such a frame in one DMA,
//! and an integrated one (Jetson) reads it in place through its device
//! address. Other formats are converted once, from their rows, into
//! page-locked JFIF `Y'CbCr` 4:2:0 planes (or 8-bit gray), which nvJPEG
//! encodes without a color conversion of its own. The JPEG is retrieved
//! straight into the worker's output segment.
#![deny(clippy::missing_const_for_fn)]
#![cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        reason = "the Linux encoder and its fake CUDA API are compiled on other Unix hosts for tests"
    )
)]
#![deny(clippy::panic_in_result_fn)]
use super::{
    backend::Unsupported,
    helper::{Encoder, Hello, Library, Opened},
};
use crate::{framebuf::shared::Segment, types::Frame};
use anyhow::{Context as _, Result, bail, ensure};
use std::{
    collections::{HashMap, HashSet},
    ffi::{c_int, c_uint, c_void},
};

/// Why this host has no NVIDIA GPU driver, if it has none.
pub(crate) fn absent() -> Option<String> {
    (!nvidia(std::path::Path::new("/"))).then(|| "no NVIDIA GPU driver found".into())
}

/// An NVIDIA driver under `root`: a discrete GPU's `nvidiactl`, Jetson's
/// `nvmap`, `nvhost-ctrl-gpu` or `nvgpu`, or a loaded kernel driver whose
/// nodes CUDA creates on first use, as it does without udev rules or the
/// persistence daemon.
fn nvidia(root: &std::path::Path) -> bool {
    [
        "dev/nvidiactl",
        "dev/nvmap",
        "dev/nvhost-ctrl-gpu",
        "dev/nvgpu",
        "proc/driver/nvidia/version",
        "sys/module/nvidia",
    ]
    .iter()
    .any(|path| root.join(path).exists())
}

/// Load the libraries and open an encoder, checked on a small frame.
pub(crate) fn open() -> Opened {
    let api = Api::load()?;
    let mut gpu = Gpu::new(api)?;
    gpu.probe().context("nvJPEG cannot encode on this GPU")?;
    let hello = Hello {
        ok: true,
        gray: true,
        about: gpu.about(),
        ..Hello::default()
    };
    Ok((Box::new(gpu), hello))
}

type Status = c_int;
type Handle = *mut c_void;
/// The legacy default stream.
const STREAM: *mut c_void = std::ptr::null_mut();
const CUDA_MEMCPY_HOST_TO_DEVICE: c_int = 1;
const CUDA_HOST_REGISTER_DEFAULT: c_uint = 0;
const CUDA_HOST_REGISTER_MAPPED: c_uint = 2;
const CUDA_HOST_ALLOC_DEFAULT: c_uint = 0;
const CUDA_HOST_ALLOC_MAPPED: c_uint = 2;
const CUDA_ATTRIBUTE_INTEGRATED: c_int = 18;
const CUDA_ATTRIBUTE_CAN_MAP_HOST_MEMORY: c_int = 19;
const NVJPEG_CSS_420: c_int = 2;
const NVJPEG_CSS_GRAY: c_int = 6;
const NVJPEG_INPUT_RGBI: c_int = 5;
const NVJPEG_INPUT_BGRI: c_int = 6;
const MAJOR_VERSION: c_int = 0;
const MINOR_VERSION: c_int = 1;
const PATCH_LEVEL: c_int = 2;

/// `nvjpegImage_t`: up to four planes and their pitches.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct Image {
    channel: [*mut u8; 4],
    pitch: [usize; 4],
}
const _: () = assert!(size_of::<Image>() == 4 * size_of::<*mut u8>() + 4 * size_of::<usize>());

/// The CUDA runtime and nvJPEG entry points the encoder uses.
pub(crate) struct Api {
    malloc: unsafe extern "C" fn(*mut *mut c_void, usize) -> c_int,
    free: unsafe extern "C" fn(*mut c_void) -> c_int,
    host_alloc: unsafe extern "C" fn(*mut *mut c_void, usize, c_uint) -> c_int,
    free_host: unsafe extern "C" fn(*mut c_void) -> c_int,
    memcpy: unsafe extern "C" fn(*mut c_void, *const c_void, usize, c_int) -> c_int,
    synchronize: unsafe extern "C" fn(*mut c_void) -> c_int,
    host_register: unsafe extern "C" fn(*mut c_void, usize, c_uint) -> c_int,
    host_unregister: unsafe extern "C" fn(*mut c_void) -> c_int,
    host_device_pointer: unsafe extern "C" fn(*mut *mut c_void, *mut c_void, c_uint) -> c_int,
    get_device: unsafe extern "C" fn(*mut c_int) -> c_int,
    device_attribute: unsafe extern "C" fn(*mut c_int, c_int, c_int) -> c_int,
    runtime_version: unsafe extern "C" fn(*mut c_int) -> c_int,
    get_property: unsafe extern "C" fn(c_int, *mut c_int) -> Status,
    create: unsafe extern "C" fn(*mut Handle) -> Status,
    destroy: unsafe extern "C" fn(Handle) -> Status,
    state_create: unsafe extern "C" fn(Handle, *mut Handle, *mut c_void) -> Status,
    state_destroy: unsafe extern "C" fn(Handle) -> Status,
    params_create: unsafe extern "C" fn(Handle, *mut Handle, *mut c_void) -> Status,
    params_destroy: unsafe extern "C" fn(Handle) -> Status,
    set_quality: unsafe extern "C" fn(Handle, c_int, *mut c_void) -> Status,
    set_sampling: unsafe extern "C" fn(Handle, c_int, *mut c_void) -> Status,
    encode_image: unsafe extern "C" fn(
        Handle,
        Handle,
        Handle,
        *const Image,
        c_int,
        c_int,
        c_int,
        *mut c_void,
    ) -> Status,
    encode_yuv: unsafe extern "C" fn(
        Handle,
        Handle,
        Handle,
        *const Image,
        c_int,
        c_int,
        c_int,
        *mut c_void,
    ) -> Status,
    retrieve: unsafe extern "C" fn(Handle, Handle, *mut u8, *mut usize, *mut c_void) -> Status,
    /// Kept loaded while the entry points are used.
    _libraries: Option<[Library; 2]>,
}

/// Matching CUDA runtime and nvJPEG names, newest major first.
fn library_pairs() -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let named = std::env::var_os("CAPTUREFAB_CUDA_LIB_DIR")
        .map(|dir| format!("{}/", dir.to_string_lossy()));
    let mut directories: Vec<String> = named.iter().cloned().collect();
    directories.push(String::new());
    directories.extend(
        [
            "/usr/local/cuda/lib64/",
            "/usr/local/cuda/targets/sbsa-linux/lib/",
            "/usr/local/cuda/targets/aarch64-linux/lib/",
            "/usr/local/cuda/targets/x86_64-linux/lib/",
        ]
        .map(String::from),
    );
    for (cudart, nvjpeg) in [
        ("libcudart.so.13", "libnvjpeg.so.13"),
        ("libcudart.so.12", "libnvjpeg.so.12"),
        ("libcudart.so.11.0", "libnvjpeg.so.11"),
    ] {
        for dir in &directories {
            pairs.push((format!("{dir}{cudart}"), format!("{dir}{nvjpeg}")));
        }
    }
    if let Some(dir) = named {
        pairs.push((format!("{dir}libcudart.so"), format!("{dir}libnvjpeg.so")));
    }
    pairs
}

impl Api {
    fn load() -> Result<Self> {
        let mut last = None;
        for (cudart, nvjpeg) in library_pairs() {
            match (Library::open(&cudart), Library::open(&nvjpeg)) {
                (Ok(cudart), Ok(nvjpeg)) => return Self::bind(cudart, nvjpeg),
                (Err(error), _) | (_, Err(error)) => last = Some(error),
            }
        }
        bail!(
            "the CUDA runtime and nvJPEG libraries were not found{}",
            last.map(|error| format!(" ({error:#})"))
                .unwrap_or_default()
        )
    }

    fn bind(cudart: Library, nvjpeg: Library) -> Result<Self> {
        Ok(Self {
            // SAFETY: the field type matches cudaMalloc in the CUDA runtime or nvjpeg.h C API.
            malloc: unsafe { cudart.symbol(c"cudaMalloc") }?,
            // SAFETY: the field type matches cudaFree in the CUDA runtime or nvjpeg.h C API.
            free: unsafe { cudart.symbol(c"cudaFree") }?,
            // SAFETY: the field type matches cudaHostAlloc in the CUDA runtime or nvjpeg.h C API.
            host_alloc: unsafe { cudart.symbol(c"cudaHostAlloc") }?,
            // SAFETY: the field type matches cudaFreeHost in the CUDA runtime or nvjpeg.h C API.
            free_host: unsafe { cudart.symbol(c"cudaFreeHost") }?,
            // SAFETY: the field type matches cudaMemcpy in the CUDA runtime or nvjpeg.h C API.
            memcpy: unsafe { cudart.symbol(c"cudaMemcpy") }?,
            // SAFETY: the field type matches cudaStreamSynchronize in the CUDA runtime or nvjpeg.h C API.
            synchronize: unsafe { cudart.symbol(c"cudaStreamSynchronize") }?,
            // SAFETY: the field type matches cudaHostRegister in the CUDA runtime or nvjpeg.h C API.
            host_register: unsafe { cudart.symbol(c"cudaHostRegister") }?,
            // SAFETY: the field type matches cudaHostUnregister in the CUDA runtime or nvjpeg.h C API.
            host_unregister: unsafe { cudart.symbol(c"cudaHostUnregister") }?,
            // SAFETY: the field type matches cudaHostGetDevicePointer in the CUDA runtime or nvjpeg.h C API.
            host_device_pointer: unsafe { cudart.symbol(c"cudaHostGetDevicePointer") }?,
            // SAFETY: the field type matches cudaGetDevice in the CUDA runtime or nvjpeg.h C API.
            get_device: unsafe { cudart.symbol(c"cudaGetDevice") }?,
            // SAFETY: the field type matches cudaDeviceGetAttribute in the CUDA runtime or nvjpeg.h C API.
            device_attribute: unsafe { cudart.symbol(c"cudaDeviceGetAttribute") }?,
            // SAFETY: the field type matches cudaRuntimeGetVersion in the CUDA runtime or nvjpeg.h C API.
            runtime_version: unsafe { cudart.symbol(c"cudaRuntimeGetVersion") }?,
            // SAFETY: the field type matches nvjpegGetProperty in the CUDA runtime or nvjpeg.h C API.
            get_property: unsafe { nvjpeg.symbol(c"nvjpegGetProperty") }?,
            // SAFETY: the field type matches nvjpegCreateSimple in the CUDA runtime or nvjpeg.h C API.
            create: unsafe { nvjpeg.symbol(c"nvjpegCreateSimple") }
                .context("this libnvjpeg is not CUDA's nvJPEG")?,
            // SAFETY: the field type matches nvjpegDestroy in the CUDA runtime or nvjpeg.h C API.
            destroy: unsafe { nvjpeg.symbol(c"nvjpegDestroy") }?,
            // SAFETY: the field type matches nvjpegEncoderStateCreate in the CUDA runtime or nvjpeg.h C API.
            state_create: unsafe { nvjpeg.symbol(c"nvjpegEncoderStateCreate") }?,
            // SAFETY: the field type matches nvjpegEncoderStateDestroy in the CUDA runtime or nvjpeg.h C API.
            state_destroy: unsafe { nvjpeg.symbol(c"nvjpegEncoderStateDestroy") }?,
            // SAFETY: the field type matches nvjpegEncoderParamsCreate in the CUDA runtime or nvjpeg.h C API.
            params_create: unsafe { nvjpeg.symbol(c"nvjpegEncoderParamsCreate") }?,
            // SAFETY: the field type matches nvjpegEncoderParamsDestroy in the CUDA runtime or nvjpeg.h C API.
            params_destroy: unsafe { nvjpeg.symbol(c"nvjpegEncoderParamsDestroy") }?,
            // SAFETY: the field type matches nvjpegEncoderParamsSetQuality in the CUDA runtime or nvjpeg.h C API.
            set_quality: unsafe { nvjpeg.symbol(c"nvjpegEncoderParamsSetQuality") }?,
            // SAFETY: the field type matches nvjpegEncoderParamsSetSamplingFactors in the CUDA runtime or nvjpeg.h C API.
            set_sampling: unsafe { nvjpeg.symbol(c"nvjpegEncoderParamsSetSamplingFactors") }?,
            // SAFETY: the field type matches nvjpegEncodeImage in the CUDA runtime or nvjpeg.h C API.
            encode_image: unsafe { nvjpeg.symbol(c"nvjpegEncodeImage") }?,
            // SAFETY: the field type matches nvjpegEncodeYUV in the CUDA runtime or nvjpeg.h C API.
            encode_yuv: unsafe { nvjpeg.symbol(c"nvjpegEncodeYUV") }?,
            // SAFETY: the field type matches nvjpegEncodeRetrieveBitstream in the CUDA runtime or nvjpeg.h C API.
            retrieve: unsafe { nvjpeg.symbol(c"nvjpegEncodeRetrieveBitstream") }?,
            _libraries: Some([cudart, nvjpeg]),
        })
    }
}

/// A CUDA runtime or nvJPEG status as an error naming the call.
fn check(what: &str, status: c_int) -> Result<()> {
    const NVJPEG: [&str; 11] = [
        "success",
        "not initialized",
        "invalid parameter",
        "bad JPEG",
        "JPEG not supported",
        "allocator failure",
        "execution failed",
        "architecture mismatch",
        "internal error",
        "implementation not supported",
        "incomplete bitstream",
    ];
    if status == 0 {
        return Ok(());
    }
    match usize::try_from(status).ok().and_then(|i| NVJPEG.get(i)) {
        Some(name) if what.starts_with("nvjpeg") => bail!("{what} failed: {name} ({status})"),
        _ => bail!("{what} failed with status {status}"),
    }
}

/// A segment page-locked for the GPU.
struct Locked {
    base: *mut u8,
    len: usize,
    /// Its device address, where the GPU reads it in place (integrated).
    device: *mut c_void,
}

/// Page-locked host memory the encoder converts frames into.
#[derive(Clone, Copy)]
struct Staging {
    host: *mut u8,
    len: usize,
    /// Its device address on an integrated GPU, else null.
    device: *mut c_void,
}

struct Gpu {
    api: Api,
    handle: Handle,
    state: Handle,
    params: Handle,
    /// The GPU shares system memory (Jetson) and can address page-locked
    /// host memory: it reads frames in place.
    integrated: bool,
    /// The quality the parameters hold.
    quality: c_int,
    locked: HashMap<u64, Locked>,
    /// Segments that could not be page-locked, by id.
    unlockable: HashSet<u64>,
    locked_bytes: usize,
    /// At most this much is page-locked: it cannot be swapped.
    lock_limit: usize,
    /// Device memory frames are copied into on a discrete GPU.
    device: (*mut c_void, usize),
    staging: Option<Staging>,
}

impl Gpu {
    fn new(api: Api) -> Result<Self> {
        let mut gpu = Self {
            api,
            handle: std::ptr::null_mut(),
            state: std::ptr::null_mut(),
            params: std::ptr::null_mut(),
            integrated: false,
            quality: 0,
            locked: HashMap::new(),
            unlockable: HashSet::new(),
            locked_bytes: 0,
            lock_limit: crate::host::memory_budget().map_or(1 << 30, |budget| {
                usize::try_from(budget / 4).unwrap_or(usize::MAX)
            }),
            device: (std::ptr::null_mut(), 0),
            staging: None,
        };
        let api = &gpu.api;
        // SAFETY: the output field is initialized; Drop releases any handle created here.
        let status = unsafe { (api.create)(&raw mut gpu.handle) };
        check("nvjpegCreateSimple", status)?;
        // SAFETY: the handle is live; Drop owns the initialized state output field.
        let status = unsafe { (api.state_create)(gpu.handle, &raw mut gpu.state, STREAM) };
        check("nvjpegEncoderStateCreate", status)?;
        // SAFETY: the handle is live; Drop owns the initialized parameter output field.
        let status = unsafe { (api.params_create)(gpu.handle, &raw mut gpu.params, STREAM) };
        check("nvjpegEncoderParamsCreate", status)?;
        let (mut device, mut integrated, mut mappable) = (0, 0, 0);
        // SAFETY: CUDA writes the current device ID to an initialized local.
        gpu.integrated = unsafe { (api.get_device)(&raw mut device) } == 0
            // SAFETY: the queried device is valid; the output is an initialized local.
            && unsafe { (api.device_attribute)(&raw mut integrated, CUDA_ATTRIBUTE_INTEGRATED, device) } == 0
            // SAFETY: the same valid device and a separate initialized output local.
            && unsafe { (api.device_attribute)(
                    &raw mut mappable,
                    CUDA_ATTRIBUTE_CAN_MAP_HOST_MEMORY,
                    device,
                ) } == 0
                && integrated == 1
                && mappable == 1;
        Ok(gpu)
    }

    /// What `doctor` shows: the versions, and how frames reach the GPU.
    fn about(&self) -> serde_json::Map<String, serde_json::Value> {
        let api = &self.api;
        let (mut major, mut minor, mut patch, mut cuda) = (0, 0, 0, 0);
        // SAFETY: nvJPEG writes the major version to an initialized local.
        unsafe { (api.get_property)(MAJOR_VERSION, &raw mut major) };
        // SAFETY: nvJPEG writes the minor version to an initialized local.
        unsafe { (api.get_property)(MINOR_VERSION, &raw mut minor) };
        // SAFETY: nvJPEG writes the patch version to an initialized local.
        unsafe { (api.get_property)(PATCH_LEVEL, &raw mut patch) };
        // SAFETY: CUDA writes its runtime version to an initialized local.
        unsafe { (api.runtime_version)(&raw mut cuda) };
        let upload = if self.integrated { "in-place" } else { "dma" };
        serde_json::json!({
            "version": format!("nvJPEG {major}.{minor}.{patch}, CUDA {}.{}", cuda / 1000, cuda % 1000 / 10),
            "integrated": self.integrated,
            "upload": upload,
        })
        .as_object()
        .cloned()
        .unwrap_or_default()
    }

    /// Encode a small gray frame, as a driver that cannot (an unsupported
    /// architecture) fails here rather than on a camera's frame.
    fn probe(&mut self) -> Result<()> {
        let (width, height) = (64usize, 48usize);
        let staging = self.staging(width * height)?;
        // SAFETY: the staging buffer holds width * height bytes.
        unsafe { std::slice::from_raw_parts_mut(staging.host, width * height) }
            .iter_mut()
            .enumerate()
            .for_each(|(i, value)| {
                *value = u8::try_from(i % width * 4).expect("the 64-pixel probe gradient fits u8");
            });
        let source = self.device_view(staging.host, width * height, staging.device)?;
        let mut out = vec![0; 64 * 1024];
        let image = Image {
            channel: [
                source,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ],
            pitch: [width, 0, 0, 0],
        };
        self.run(
            Layout::Gray,
            &image,
            width,
            height,
            super::QUALITY,
            &mut out,
        )?;
        Ok(())
    }

    /// The staging buffer, holding at least `len` bytes.
    fn staging(&mut self, len: usize) -> Result<Staging> {
        if let Some(staging) = &self.staging
            && staging.len >= len
        {
            return Ok(*staging);
        }
        self.free_staging()?;
        let api = &self.api;
        let flags = if self.integrated {
            CUDA_HOST_ALLOC_MAPPED
        } else {
            CUDA_HOST_ALLOC_DEFAULT
        };
        let mut host = std::ptr::null_mut();
        let mut device = std::ptr::null_mut();
        // SAFETY: CUDA writes an allocation into initialized pointer storage;
        // the allocation is retained in `self.staging` and released by `free_staging`.
        check("cudaHostAlloc", unsafe {
            (api.host_alloc)(&raw mut host, len, flags)
        })?;
        if self.integrated
            // SAFETY: host is a live CUDA allocation and device is initialized output storage.
            && unsafe { (api.host_device_pointer)(&raw mut device, host, 0) } != 0
        {
            device = std::ptr::null_mut();
        }
        let staging = Staging {
            host: host.cast(),
            len,
            device,
        };
        self.staging = Some(staging);
        Ok(staging)
    }

    fn free_staging(&mut self) -> Result<()> {
        if let Some(staging) = self.staging.take() {
            // SAFETY: allocated by `staging`, freed once.
            check("cudaFreeHost", unsafe {
                (self.api.free_host)(staging.host.cast())
            })?;
        }
        Ok(())
    }

    /// Page-lock `segment` the first time a frame is read from it, within
    /// the limit; whether it is.
    fn lock(&mut self, segment: &Segment) -> Option<&Locked> {
        let id = segment.id();
        if !self.locked.contains_key(&id)
            && !self.unlockable.contains(&id)
            && self.locked_bytes + segment.len() <= self.lock_limit
        {
            let api = &self.api;
            let base = segment.as_ptr();
            let flags = if self.integrated {
                CUDA_HOST_REGISTER_MAPPED
            } else {
                CUDA_HOST_REGISTER_DEFAULT
            };
            // SAFETY: this process's live mapping of the segment,
            // unregistered in `detach` before it is unmapped.
            let registered = unsafe { (api.host_register)(base.cast(), segment.len(), flags) } == 0;
            if registered {
                let mut device = std::ptr::null_mut();
                if self.integrated
                    // SAFETY: base is the live registered segment mapping;
                    // device is writable storage for the returned pointer.
                    && unsafe { (api.host_device_pointer)(&raw mut device, base.cast(), 0) } != 0
                {
                    device = std::ptr::null_mut();
                }
                self.locked_bytes += segment.len();
                self.locked.insert(
                    id,
                    Locked {
                        base,
                        len: segment.len(),
                        device,
                    },
                );
            } else {
                self.unlockable.insert(id);
            }
        }
        self.locked.get(&id)
    }

    /// Where the GPU reads `len` bytes at `host`: `device`, the memory's own
    /// device address, else a copy in device memory.
    fn device_view(&mut self, host: *const u8, len: usize, device: *mut c_void) -> Result<*mut u8> {
        if !device.is_null() {
            return Ok(device.cast());
        }
        let api = &self.api;
        if self.device.1 < len {
            if !self.device.0.is_null() {
                // A CUDA error can report an earlier asynchronous failure;
                // relinquish the old address before freeing so Drop cannot retry it.
                let (old, _) = std::mem::replace(&mut self.device, (std::ptr::null_mut(), 0));
                // SAFETY: `old` is the device allocation owned by this encoder.
                check("cudaFree", unsafe { (api.free)(old) })?;
            }
            let mut buffer = std::ptr::null_mut();
            // SAFETY: CUDA writes a new allocation into initialized pointer storage.
            check("cudaMalloc", unsafe { (api.malloc)(&raw mut buffer, len) })?;
            self.device = (buffer, len);
        }
        // SAFETY: the device allocation holds `len` bytes, and the caller's
        // host buffer remains live for the synchronous copy.
        check("cudaMemcpy", unsafe {
            (api.memcpy)(self.device.0, host.cast(), len, CUDA_MEMCPY_HOST_TO_DEVICE)
        })?;
        Ok(self.device.0.cast())
    }

    /// Encode `image` and retrieve the JPEG into `out`; its length.
    fn run(
        &mut self,
        layout: Layout,
        image: &Image,
        width: usize,
        height: usize,
        quality: i32,
        out: &mut [u8],
    ) -> Result<usize> {
        let api = &self.api;
        let w = c_int::try_from(width).context("JPEG width exceeds encoder range")?;
        let h = c_int::try_from(height).context("JPEG height exceeds encoder range")?;
        if self.quality != quality {
            // SAFETY: the parameter object is live and the default stream is valid.
            check("nvjpegEncoderParamsSetQuality", unsafe {
                (api.set_quality)(self.params, quality, STREAM)
            })?;
            self.quality = quality;
        }
        let sampling = match layout {
            Layout::Gray => NVJPEG_CSS_GRAY,
            _ => NVJPEG_CSS_420,
        };
        // SAFETY: the live parameter object receives a supported sampling mode.
        check("nvjpegEncoderParamsSetSamplingFactors", unsafe {
            (api.set_sampling)(self.params, sampling, STREAM)
        })?;
        match layout {
            Layout::Interleaved(format) => check(
                "nvjpegEncodeImage",
                // SAFETY: the handles are live; `image` describes the device-readable
                // frame at its pitches, with dimensions checked to fit the API.
                unsafe {
                    (api.encode_image)(
                        self.handle,
                        self.state,
                        self.params,
                        image,
                        format,
                        w,
                        h,
                        STREAM,
                    )
                },
            )?,
            Layout::Gray | Layout::Yuv420 => check(
                "nvjpegEncodeYUV",
                // SAFETY: the live handles encode device-readable planes at their
                // supplied pitches and checked dimensions on the default stream.
                unsafe {
                    (api.encode_yuv)(
                        self.handle,
                        self.state,
                        self.params,
                        image,
                        sampling,
                        w,
                        h,
                        STREAM,
                    )
                },
            )?,
        }
        // SAFETY: the default stream is valid; wait before retrieving its output.
        check("cudaStreamSynchronize", unsafe {
            (api.synchronize)(STREAM)
        })?;
        let mut length = 0usize;
        check(
            "nvjpegEncodeRetrieveBitstream",
            // SAFETY: the live state writes only its required size to initialized
            // length storage when given a null destination.
            unsafe {
                (api.retrieve)(
                    self.handle,
                    self.state,
                    std::ptr::null_mut(),
                    &raw mut length,
                    STREAM,
                )
            },
        )?;
        if length > out.len() {
            return Err(Unsupported(format!(
                "its {length}-byte JPEG is longer than the room for it"
            ))
            .into());
        }
        check(
            "nvjpegEncodeRetrieveBitstream",
            // SAFETY: the required length fits `out`, and the live state writes
            // within that supplied capacity; both outputs outlive synchronization.
            unsafe {
                (api.retrieve)(
                    self.handle,
                    self.state,
                    out.as_mut_ptr(),
                    &raw mut length,
                    STREAM,
                )
            },
        )?;
        // SAFETY: wait for the default stream before returning borrowed output storage.
        check("cudaStreamSynchronize", unsafe {
            (api.synchronize)(STREAM)
        })?;
        Ok(length)
    }
}

/// How nvJPEG reads a frame.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Layout {
    /// Interleaved RGB or BGR, which nvJPEG converts to 4:2:0 itself.
    Interleaved(c_int),
    Gray,
    /// JFIF `Y'CbCr` 4:2:0 planes.
    Yuv420,
}

impl Encoder for Gpu {
    fn detach(&mut self, segment: &Segment) {
        self.unlockable.remove(&segment.id());
        if let Some(locked) = self.locked.remove(&segment.id()) {
            self.locked_bytes -= locked.len;
            // SAFETY: registered by `lock` and still mapped.
            unsafe { (self.api.host_unregister)(locked.base.cast()) };
        }
    }

    fn encode(
        &mut self,
        frame: &Frame,
        input: &Segment,
        quality: i32,
        out: &mut [u8],
    ) -> Result<usize> {
        crate::frame::validate_source(frame)?;
        let (width, height) = (frame.width as usize, frame.height as usize);
        if width > 65_535 || height > 65_535 {
            return Err(Unsupported(format!("{width}x{height} is larger than a JPEG")).into());
        }
        let (layout, direct) = match frame.pixel_format {
            crate::types::RGB8 => (Layout::Interleaved(NVJPEG_INPUT_RGBI), 3),
            0x0218_0015 => (Layout::Interleaved(NVJPEG_INPUT_BGRI), 3),
            crate::types::MONO8 => (Layout::Gray, 1),
            format if crate::frame::monochrome(format) => (Layout::Gray, 0),
            _ => (Layout::Yuv420, 0),
        };
        let mut image = Image {
            channel: [std::ptr::null_mut(); 4],
            pitch: [0; 4],
        };
        if direct > 0 {
            // Read where the camera wrote it: the frame starts its segment.
            ensure!(
                frame.data.as_ptr() == input.as_ptr().cast_const(),
                "the frame is not at the start of its segment"
            );
            let len = width * height * direct;
            let device = self
                .lock(input)
                .map_or(std::ptr::null_mut(), |locked| locked.device);
            image.channel[0] = self.device_view(input.as_ptr(), len, device)?;
            image.pitch[0] = width * direct;
        } else if layout == Layout::Gray {
            let staging = self.staging(width * height)?;
            // SAFETY: the staging buffer holds width * height bytes and is
            // this encoder's alone.
            let gray = unsafe { std::slice::from_raw_parts_mut(staging.host, width * height) };
            crate::frame::decode_rows(
                crate::frame::FrameRef::whole(frame),
                crate::frame::Channels::Gray,
                gray,
            );
            crate::copies::record(crate::copies::Site::StillTransform, gray.len());
            image.channel[0] = self.device_view(staging.host, gray.len(), staging.device)?;
            image.pitch[0] = width;
        } else {
            let planes = super::Planes::of(width, height);
            let staging = self.staging(planes.len())?;
            // SAFETY: as for gray, for the planes' length.
            let buffer = unsafe { std::slice::from_raw_parts_mut(staging.host, planes.len()) };
            planes.write(crate::frame::FrameRef::whole(frame), buffer);
            crate::copies::record(crate::copies::Site::StillTransform, planes.len());
            let base = self.device_view(staging.host, planes.len(), staging.device)?;
            image.channel[0] = base;
            // SAFETY: the first chroma plane follows luma within the staging allocation.
            image.channel[1] = unsafe { base.add(planes.luma_len()) };
            // SAFETY: the second chroma plane follows the first within the same allocation.
            image.channel[2] = unsafe { base.add(planes.luma_len() + planes.chroma_len()) };
            image.pitch[..3].copy_from_slice(&[
                planes.luma_width,
                planes.chroma_width(),
                planes.chroma_width(),
            ]);
        }
        self.run(layout, &image, width, height, quality, out)
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // Destruction cannot report driver errors; the allocation is removed
        // before release, so an ambiguous failure is never retried.
        let _ = self.free_staging();
        let api = &self.api;
        for locked in self.locked.values() {
            // SAFETY: this registered segment remains mapped during encoder destruction.
            unsafe { (api.host_unregister)(locked.base.cast()) };
        }
        if !self.device.0.is_null() {
            // SAFETY: this encoder owns the non-null device allocation.
            unsafe { (api.free)(self.device.0) };
        }
        if !self.params.is_null() {
            // SAFETY: these parameters were created here and have not been destroyed.
            unsafe { (api.params_destroy)(self.params) };
        }
        if !self.state.is_null() {
            // SAFETY: this state was created here and has not been destroyed.
            unsafe { (api.state_destroy)(self.state) };
        }
        if !self.handle.is_null() {
            // SAFETY: all dependent objects were released before this owned handle.
            unsafe { (api.destroy)(self.handle) };
        }
    }
}

#[cfg(all(test, feature = "jpeg"))]
mod tests {
    //! nvJPEG and the CUDA runtime faked by functions that record what they
    //! are given and encode with libjpeg-turbo, as the GPU would.
    use super::*;
    use crate::{
        framebuf::PoolVec,
        jpeg::QUALITY,
        types::{MONO8, RGB8},
    };
    use std::sync::{Arc, Mutex, MutexGuard};

    #[derive(Default)]
    struct Fake {
        integrated: bool,
        /// Ranges registered: (start, length, flags).
        registered: Vec<(usize, usize, c_uint)>,
        unregistered: Vec<usize>,
        /// Host memory mapped for the device: (device, host, length).
        aliases: Vec<(usize, usize, usize)>,
        /// Host-to-device copies: (destination, source, length).
        copies: Vec<(usize, usize, usize)>,
        /// Encodes: whether through `EncodeImage`, the input format or
        /// sampling, the first channel and pitch.
        encodes: Vec<(bool, c_int, usize, usize)>,
        bitstream: Vec<u8>,
        quality: c_int,
        sampling: c_int,
        allocations: HashMap<usize, Vec<u8>>,
        released: Vec<&'static str>,
    }

    static FAKE: Mutex<Option<Fake>> = Mutex::new(None);
    /// The fake is one per process: its tests take turns.
    static TURN: Mutex<()> = Mutex::new(());

    fn fake<R>(f: impl FnOnce(&mut Fake) -> R) -> R {
        f(FAKE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert_with(Fake::default))
    }

    /// The host memory behind a device address the fake handed out.
    fn host(address: *const u8) -> *const u8 {
        let address = address as usize;
        fake(|fake| {
            for &(device, host, len) in &fake.aliases {
                if (device..device + len).contains(&address) {
                    return (host + address - device) as *const u8;
                }
            }
            address as *const u8
        })
    }

    /// A device address that is not the host's: the top bit flipped.
    const fn alias(host: usize) -> usize {
        host ^ (1 << (usize::BITS - 1))
    }

    unsafe extern "C" fn malloc(out: *mut *mut c_void, len: usize) -> c_int {
        let mut buffer = vec![0u8; len];
        let address = buffer.as_mut_ptr();
        fake(|fake| fake.allocations.insert(address as usize, buffer));
        // SAFETY: The caller supplies writable pointer storage; the Vec remains owned by FAKE.
        unsafe { *out = address.cast() };
        0
    }
    unsafe extern "C" fn free(address: *mut c_void) -> c_int {
        fake(|fake| fake.allocations.remove(&(address as usize)));
        0
    }
    unsafe extern "C" fn host_alloc(out: *mut *mut c_void, len: usize, _: c_uint) -> c_int {
        // SAFETY: Forward the caller's valid output pointer and allocation length.
        unsafe { malloc(out, len) }
    }
    unsafe extern "C" fn host_device_pointer(
        out: *mut *mut c_void,
        host: *mut c_void,
        _: c_uint,
    ) -> c_int {
        let device = fake(|fake| {
            if !fake.integrated {
                return None;
            }
            let len = fake
                .registered
                .iter()
                .find(|range| range.0 == host as usize)
                .map(|range| range.1)
                .or_else(|| fake.allocations.get(&(host as usize)).map(Vec::len))
                .unwrap_or(0);
            let device = alias(host as usize);
            fake.aliases.push((device, host as usize, len));
            Some(device)
        });
        device.map_or(1, |device| {
            // SAFETY: The API caller supplies writable storage for the device pointer.
            unsafe { *out = device as *mut c_void };
            0
        })
    }
    unsafe extern "C" fn memcpy(
        to: *mut c_void,
        from: *const c_void,
        len: usize,
        kind: c_int,
    ) -> c_int {
        assert_eq!(kind, CUDA_MEMCPY_HOST_TO_DEVICE);
        fake(|fake| fake.copies.push((to as usize, from as usize, len)));
        // SAFETY: The encoder supplies distinct live host and device buffers of at least len bytes.
        unsafe { std::ptr::copy_nonoverlapping(from.cast::<u8>(), to.cast::<u8>(), len) };
        0
    }
    const unsafe extern "C" fn synchronize(_: *mut c_void) -> c_int {
        0
    }
    unsafe extern "C" fn host_register(host: *mut c_void, len: usize, flags: c_uint) -> c_int {
        fake(|fake| fake.registered.push((host as usize, len, flags)));
        0
    }
    unsafe extern "C" fn host_unregister(host: *mut c_void) -> c_int {
        fake(|fake| fake.unregistered.push(host as usize));
        0
    }
    unsafe extern "C" fn get_device(device: *mut c_int) -> c_int {
        // SAFETY: The caller supplies writable storage for one device ordinal.
        unsafe { *device = 0 };
        0
    }
    unsafe extern "C" fn device_attribute(value: *mut c_int, attribute: c_int, _: c_int) -> c_int {
        let integrated = fake(|fake| fake.integrated);
        // SAFETY: The caller supplies writable storage for one attribute value.
        unsafe {
            *value = match attribute {
                CUDA_ATTRIBUTE_INTEGRATED => integrated.into(),
                CUDA_ATTRIBUTE_CAN_MAP_HOST_MEMORY => 1,
                _ => 0,
            }
        };
        0
    }
    unsafe extern "C" fn runtime_version(version: *mut c_int) -> c_int {
        // SAFETY: The caller supplies writable storage for one runtime version.
        unsafe { *version = 12040 };
        0
    }
    unsafe extern "C" fn get_property(kind: c_int, value: *mut c_int) -> c_int {
        // SAFETY: The caller supplies writable storage for one property value.
        unsafe { *value = [12, 3, 1][usize::try_from(kind).expect("nonnegative property index")] };
        0
    }
    unsafe extern "C" fn create(handle: *mut Handle) -> c_int {
        // SAFETY: The caller supplies writable handle storage; this sentinel is never dereferenced.
        unsafe { *handle = std::ptr::dangling_mut() };
        0
    }
    const unsafe extern "C" fn destroy(_: Handle) -> c_int {
        0
    }
    unsafe extern "C" fn object_create(_: Handle, object: *mut Handle, _: *mut c_void) -> c_int {
        // SAFETY: The caller supplies writable handle storage; this sentinel is never dereferenced.
        unsafe { *object = std::ptr::without_provenance_mut(2) };
        0
    }
    unsafe extern "C" fn set_quality(_: Handle, quality: c_int, _: *mut c_void) -> c_int {
        fake(|fake| fake.quality = quality);
        0
    }
    unsafe extern "C" fn set_sampling(_: Handle, sampling: c_int, _: *mut c_void) -> c_int {
        fake(|fake| fake.sampling = sampling);
        0
    }
    /// The planes as libjpeg-turbo would compress them, at the fake's
    /// quality.
    unsafe extern "C" fn encode_image(
        _: Handle,
        _: Handle,
        _: Handle,
        image: *const Image,
        format: c_int,
        width: c_int,
        height: c_int,
        _: *mut c_void,
    ) -> c_int {
        // SAFETY: The encoder passes a live initialized Image for this synchronous call.
        let image = unsafe { *image };
        let (width, height) = (
            usize::try_from(width).expect("nonnegative image width"),
            usize::try_from(height).expect("nonnegative image height"),
        );
        let source = host(image.channel[0]);
        // SAFETY: host resolves the registered alias to live storage covering pitch times height.
        let pixels = unsafe { std::slice::from_raw_parts(source, image.pitch[0] * height) };
        let input = format;
        let format = match input {
            NVJPEG_INPUT_RGBI => turbojpeg::PixelFormat::RGB,
            NVJPEG_INPUT_BGRI => turbojpeg::PixelFormat::BGR,
            _ => return 2,
        };
        let quality = fake(|fake| fake.quality);
        let mut compressor = turbojpeg::Compressor::new().unwrap();
        compressor.set_quality(quality).unwrap();
        compressor.set_subsamp(turbojpeg::Subsamp::Sub2x2).unwrap();
        let jpeg = compressor
            .compress_to_vec(turbojpeg::Image {
                pixels,
                width,
                pitch: image.pitch[0],
                height,
                format,
            })
            .unwrap();
        fake(|fake| {
            fake.encodes
                .push((true, input, image.channel[0] as usize, image.pitch[0]));
            fake.bitstream = jpeg;
        });
        0
    }
    unsafe extern "C" fn encode_yuv(
        _: Handle,
        _: Handle,
        _: Handle,
        image: *const Image,
        sampling: c_int,
        width: c_int,
        height: c_int,
        _: *mut c_void,
    ) -> c_int {
        // SAFETY: The encoder passes a live initialized Image for this synchronous call.
        let image = unsafe { *image };
        let (width, height) = (
            usize::try_from(width).expect("nonnegative image width"),
            usize::try_from(height).expect("nonnegative image height"),
        );
        let quality = fake(|fake| fake.quality);
        let mut compressor = turbojpeg::Compressor::new().unwrap();
        compressor.set_quality(quality).unwrap();
        let jpeg = match sampling {
            NVJPEG_CSS_GRAY => {
                compressor.set_subsamp(turbojpeg::Subsamp::Gray).unwrap();
                // SAFETY: The encoder supplies a live grayscale plane covering pitch times height.
                let pixels = unsafe {
                    std::slice::from_raw_parts(host(image.channel[0]), image.pitch[0] * height)
                };
                compressor
                    .compress_to_vec(turbojpeg::Image {
                        pixels,
                        width,
                        pitch: image.pitch[0],
                        height,
                        format: turbojpeg::PixelFormat::GRAY,
                    })
                    .unwrap()
            }
            NVJPEG_CSS_420 => {
                let rows = height.next_multiple_of(2);
                // SAFETY: The encoder supplies padded live YUV planes with these pitches and row counts.
                let plane = |i: usize, rows: usize| unsafe {
                    std::slice::from_raw_parts(host(image.channel[i]), image.pitch[i] * rows)
                };
                compressor
                    .compress_yuv_planes_to_owned(&turbojpeg::YuvPlanesImage {
                        y_plane: plane(0, rows),
                        u_plane: plane(1, rows / 2),
                        v_plane: plane(2, rows / 2),
                        width,
                        height,
                        y_stride: image.pitch[0],
                        u_stride: image.pitch[1],
                        v_stride: image.pitch[2],
                        subsamp: turbojpeg::Subsamp::Sub2x2,
                    })
                    .unwrap()
                    .to_vec()
            }
            _ => return 2,
        };
        fake(|fake| {
            fake.encodes
                .push((false, sampling, image.channel[0] as usize, image.pitch[0]));
            fake.bitstream = jpeg;
        });
        0
    }
    unsafe extern "C" fn retrieve(
        _: Handle,
        _: Handle,
        data: *mut u8,
        length: *mut usize,
        _: *mut c_void,
    ) -> c_int {
        fake(|fake| {
            if !data.is_null() {
                // SAFETY: the caller supplies initialized length storage.
                assert!(unsafe { *length } >= fake.bitstream.len());
                // SAFETY: the distinct output buffer has the checked capacity.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        fake.bitstream.as_ptr(),
                        data,
                        fake.bitstream.len(),
                    );
                };
            }
            // SAFETY: the caller supplies writable length storage.
            unsafe { *length = fake.bitstream.len() };
        });
        0
    }

    fn api() -> Api {
        Api {
            malloc,
            free,
            host_alloc,
            free_host: free,
            memcpy,
            synchronize,
            host_register,
            host_unregister,
            host_device_pointer,
            get_device,
            device_attribute,
            runtime_version,
            get_property,
            create,
            destroy,
            state_create: object_create,
            state_destroy: destroy,
            params_create: object_create,
            params_destroy: destroy,
            set_quality,
            set_sampling,
            encode_image,
            encode_yuv,
            retrieve,
            _libraries: None,
        }
    }

    /// A GPU, integrated or not, on the fake; the GPU is dropped before
    /// the turn ends.
    fn gpu(integrated: bool) -> (MutexGuard<'static, ()>, Gpu) {
        let turn = TURN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *FAKE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Fake {
            integrated,
            ..Fake::default()
        });
        let mut gpu = Gpu::new(api()).unwrap();
        gpu.probe().unwrap();
        // What the probe did is not the test's.
        fake(|fake| {
            fake.copies.clear();
            fake.encodes.clear();
            fake.registered.clear();
        });
        (turn, gpu)
    }

    /// A frame in a shared memory segment, as the camera wrote it.
    fn in_segment(format: u32, width: u32, height: u32) -> (Frame, Arc<Segment>) {
        let source = crate::frame::noise_frame(format, width, height);
        let segment = Arc::new(Segment::create(source.data.len()).unwrap());
        let mut data = PoolVec::mapped(segment.clone(), source.data.len());
        data.copy_from_slice(&source.data);
        let frame = Frame {
            data: data.freeze(),
            ..source
        };
        (frame, segment)
    }

    fn encode(gpu: &mut Gpu, frame: &Frame, segment: &Segment, quality: i32) -> Vec<u8> {
        let mut out = vec![0; crate::jpeg::helper::jpeg_bound(frame)];
        let len = gpu.encode(frame, segment, quality, &mut out).unwrap();
        out.truncate(len);
        out
    }

    /// On Jetson the GPU reads the frame in the very pages the camera wrote,
    /// through their device address: no copy at all.
    #[test]
    fn integrated_gpus_read_frames_in_place() {
        let (_turn, mut gpu) = gpu(true);
        for format in [RGB8, 0x0218_0015, MONO8] {
            let (frame, segment) = in_segment(format, 96, 64);
            let jpeg = encode(&mut gpu, &frame, &segment, QUALITY);
            fake(|fake| {
                let (_, _, channel, pitch) = *fake.encodes.last().unwrap();
                assert_eq!(channel, alias(segment.as_ptr() as usize), "{format:08x}");
                assert_eq!(pitch, frame.data.len() / 64);
                assert!(fake.copies.is_empty(), "{format:08x}: {:?}", fake.copies);
                assert!(
                    fake.registered
                        .iter()
                        .any(|r| r.0 == segment.as_ptr() as usize
                            && r.1 == segment.len()
                            && r.2 == CUDA_HOST_REGISTER_MAPPED)
                );
            });
            assert!(super::super::well_formed(&jpeg, 96, 64));
            assert_eq!(
                jpeg,
                crate::jpeg::encode(&frame, QUALITY).unwrap(),
                "{format:08x}"
            );
        }
    }

    /// A discrete GPU copies the frame once, by DMA from its page-locked
    /// segment; a segment is locked once however many frames it holds.
    #[test]
    fn discrete_gpus_copy_frames_once_from_their_segment() {
        let (_turn, mut gpu) = gpu(false);
        let (frame, segment) = in_segment(RGB8, 96, 64);
        for quality in [QUALITY, 75] {
            let jpeg = encode(&mut gpu, &frame, &segment, quality);
            assert_eq!(jpeg, crate::jpeg::encode(&frame, quality).unwrap());
        }
        let device = gpu.device.0 as usize;
        fake(|fake| {
            assert_eq!(fake.copies.len(), 2);
            for &(to, from, len) in &fake.copies {
                assert_eq!(from, segment.as_ptr() as usize);
                assert_eq!(len, frame.data.len());
                assert_eq!(to, device);
            }
            let locked: Vec<_> = fake
                .registered
                .iter()
                .filter(|r| r.0 == segment.as_ptr() as usize)
                .collect();
            assert_eq!(locked.len(), 1);
            assert_eq!(locked[0].2, CUDA_HOST_REGISTER_DEFAULT);
        });
        gpu.detach(&segment);
        assert_eq!(
            fake(|fake| fake.unregistered.clone()),
            [segment.as_ptr() as usize]
        );
    }

    /// Bayer, packed and wide formats become JFIF planes once, which nvJPEG
    /// encodes as 4:2:0 without converting colors; other monochrome formats
    /// become 8-bit gray.
    #[test]
    fn other_formats_are_converted_once_into_planes() {
        let (_turn, mut gpu) = gpu(true);
        for (format, width, height) in [
            (0x0108_0009, 96, 64),
            (0x010c_0059, 97, 61),
            (0x0110_0005, 33, 17),
        ] {
            let (frame, segment) = in_segment(format, width, height);
            let jpeg = encode(&mut gpu, &frame, &segment, QUALITY);
            assert!(
                super::super::well_formed(&jpeg, width, height),
                "{format:08x}"
            );
            let staging = gpu.staging.unwrap().device as usize;
            fake(|fake| {
                let (image, sampling, channel, _) = *fake.encodes.last().unwrap();
                assert!(!image);
                let gray = crate::frame::monochrome(format);
                assert_eq!(
                    sampling,
                    if gray {
                        NVJPEG_CSS_GRAY
                    } else {
                        NVJPEG_CSS_420
                    }
                );
                assert_eq!(channel, staging, "the staging planes in place");
                assert_eq!(fake.copies, [] as [(usize, usize, usize); 0]);
            });
            // As the CPU writes it from the same planes.
            assert_eq!(
                jpeg,
                crate::jpeg::encode(&frame, QUALITY).unwrap(),
                "{format:08x}"
            );
        }
    }

    #[test]
    fn partial_initialization_releases_only_created_objects_in_dependency_order() {
        const unsafe extern "C" fn fail(_: Handle, _: *mut Handle, _: *mut c_void) -> c_int {
            1
        }
        unsafe extern "C" fn release_handle(_: Handle) -> c_int {
            fake(|fake| fake.released.push("handle"));
            0
        }
        unsafe extern "C" fn release_state(_: Handle) -> c_int {
            fake(|fake| fake.released.push("state"));
            0
        }
        unsafe extern "C" fn release_params(_: Handle) -> c_int {
            fake(|fake| fake.released.push("params"));
            0
        }
        let _turn = TURN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cases: [(bool, &[&str]); 2] = [(true, &["handle"]), (false, &["state", "handle"])];
        for (state_fails, expected) in cases {
            fake(|fake| *fake = Fake::default());
            let mut api = api();
            api.destroy = release_handle;
            api.state_destroy = release_state;
            api.params_destroy = release_params;
            if state_fails {
                api.state_create = fail;
            } else {
                api.params_create = fail;
            }
            let error = Gpu::new(api).err().unwrap();
            let call = if state_fails {
                "nvjpegEncoderStateCreate"
            } else {
                "nvjpegEncoderParamsCreate"
            };
            assert!(error.to_string().contains(call), "{error:#}");
            fake(|fake| assert_eq!(fake.released, expected));
        }
    }

    #[test]
    fn failed_host_free_stops_growth_without_retaining_the_old_pointer() {
        unsafe extern "C" fn free_then_fail(address: *mut c_void) -> c_int {
            // SAFETY: the encoder passes its owned host allocation.
            unsafe { free(address) };
            1
        }
        let (_turn, mut gpu) = gpu(false);
        let old = gpu.staging.unwrap();
        let count = fake(|fake| fake.allocations.len());
        gpu.api.free_host = free_then_fail;
        let error = gpu.staging(old.len + 1).err().unwrap();
        assert!(
            error.to_string().contains("cudaFreeHost failed"),
            "{error:#}"
        );
        assert!(gpu.staging.is_none());
        fake(|fake| {
            assert!(!fake.allocations.contains_key(&(old.host as usize)));
            assert_eq!(fake.allocations.len(), count - 1);
        });
    }

    #[test]
    fn failed_device_free_stops_growth_without_retaining_the_old_pointer() {
        unsafe extern "C" fn free_then_fail(address: *mut c_void) -> c_int {
            // SAFETY: the encoder passes its owned device allocation.
            unsafe { free(address) };
            1
        }
        let (_turn, mut gpu) = gpu(false);
        let old = gpu.device.0;
        let host = vec![0u8; gpu.device.1 + 1];
        gpu.api.free = free_then_fail;
        let error = gpu
            .device_view(host.as_ptr(), host.len(), std::ptr::null_mut())
            .unwrap_err();
        assert!(error.to_string().contains("cudaFree failed"), "{error:#}");
        assert!(gpu.device.0.is_null());
        assert_eq!(gpu.device.1, 0);
        fake(|fake| {
            assert!(!fake.allocations.contains_key(&(old as usize)));
            assert_eq!(fake.copies, [] as [(usize, usize, usize); 0]);
        });
    }

    #[test]
    fn segments_past_the_lock_limit_are_copied_from_pageable_memory() {
        let (_turn, mut gpu) = gpu(true);
        gpu.lock_limit = 0;
        let (frame, segment) = in_segment(RGB8, 32, 16);
        encode(&mut gpu, &frame, &segment, QUALITY);
        fake(|fake| {
            assert_eq!(fake.registered, [] as [(usize, usize, u32); 0]);
            assert_eq!(fake.copies.len(), 1);
            assert_eq!(fake.copies[0].1, segment.as_ptr() as usize);
        });
    }

    #[test]
    fn jpegs_longer_than_their_room_are_declined() {
        let (_turn, mut gpu) = gpu(false);
        let (frame, segment) = in_segment(RGB8, 64, 64);
        let error = gpu
            .encode(&frame, &segment, QUALITY, &mut [0; 100])
            .unwrap_err();
        assert!(error.is::<Unsupported>(), "{error:#}");
    }

    #[test]
    fn hosts_without_an_nvidia_driver_are_skipped() {
        let root = std::env::temp_dir().join(format!(
            "capturefab-nvidia-nodes-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let dev = root.join("dev");
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(dev.join("null"), b"").unwrap();
        assert!(!nvidia(&root));
        // Jetson has its integrated GPU without nvidiactl.
        std::fs::write(dev.join("nvmap"), b"").unwrap();
        assert!(nvidia(&root));
        std::fs::remove_file(dev.join("nvmap")).unwrap();
        // A loaded driver whose nodes appear when CUDA first starts.
        std::fs::create_dir_all(root.join("sys/module/nvidia")).unwrap();
        assert!(nvidia(&root));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn versioned_libraries_come_first_and_unversioned_only_where_named() {
        let pairs = library_pairs();
        assert_eq!(
            pairs[0],
            ("libcudart.so.13".into(), "libnvjpeg.so.13".into())
        );
        assert!(pairs.iter().all(|(_, nvjpeg)| nvjpeg != "libnvjpeg.so"));
    }

    #[test]
    fn about_says_how_frames_reach_the_gpu() {
        let (_turn, gpu) = gpu(true);
        let about = gpu.about();
        assert_eq!(about["upload"], "in-place");
        assert_eq!(about["integrated"], true);
        assert!(
            about["version"]
                .as_str()
                .unwrap()
                .starts_with("nvJPEG 12.3.1, CUDA 12.4")
        );
    }
}
