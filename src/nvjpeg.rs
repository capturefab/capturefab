//! NVIDIA nvJPEG GPU encoding in an isolated helper process (see `accel`).
//!
//! The helper (`capturefab __nvjpeg`) loads the CUDA runtime and nvJPEG at run
//! time, so builds need no CUDA SDK and hosts without NVIDIA drivers simply
//! keep libjpeg-turbo. It page-locks the shared buffer the parent converts
//! frames into, so uploads are a single DMA, and on integrated GPUs (Jetson)
//! nvJPEG reads the pixels in place without an upload.
//! `CAPTUREFAB_NVJPEG=0` disables it; `CAPTUREFAB_NVJPEG_PIN=0` keeps the
//! buffer pageable.
#![allow(unsafe_code)]

use crate::accel::{self, Kind, Request};
use crate::jpeg::QUALITY;
use crate::types::Frame;
use anyhow::{Context, Result, bail, ensure};
use std::{ffi::c_void, sync::OnceLock};

pub static NVJPEG: Kind = Kind {
    name: "nvJPEG",
    argument: "__nvjpeg",
    variable: "CAPTUREFAB_NVJPEG",
    platform: !cfg!(target_os = "macos"),
    environment: &[],
    accelerator: OnceLock::new(),
};

/// Encode on the GPU when the helper is ready; `None` means use the CPU.
pub fn encode(frame: &Frame) -> Option<Vec<u8>> {
    accel::encode(&NVJPEG, frame)
}

/// The nvJPEG version, or why the GPU path is unavailable, for `doctor`.
pub fn probe() -> Result<String> {
    accel::probe(&NVJPEG)
}

/// Hidden `__nvjpeg` mode: encode requests from stdin until it closes.
pub fn run_helper() -> Result<()> {
    accel::serve(Encoder::new)
}

// ---------------------------------------------------------------------------
// Helper side: CUDA + nvJPEG through runtime-loaded symbols.

type Status = i32;
type Opaque = *mut c_void;
const STREAM: Opaque = std::ptr::null_mut();
const CUDA_MEMCPY_HOST_TO_DEVICE: i32 = 1;
const CUDA_HOST_REGISTER_DEFAULT: u32 = 0;
const CUDA_HOST_REGISTER_MAPPED: u32 = 2;
const CUDA_ATTRIBUTE_INTEGRATED: i32 = 18;
const CUDA_ATTRIBUTE_CAN_MAP_HOST_MEMORY: i32 = 19;
const NVJPEG_CSS_420: i32 = 2;
const NVJPEG_CSS_GRAY: i32 = 6;
const NVJPEG_INPUT_RGBI: i32 = 5;
const MAJOR_VERSION: i32 = 0;
const MINOR_VERSION: i32 = 1;

/// nvjpegImage_t: up to four planes and their pitches.
#[repr(C)]
struct NvImage {
    channel: [*mut u8; 4],
    pitch: [usize; 4],
}

struct Api {
    malloc: unsafe extern "C" fn(*mut Opaque, usize) -> i32,
    free: unsafe extern "C" fn(Opaque) -> i32,
    memcpy: unsafe extern "C" fn(Opaque, *const c_void, usize, i32) -> i32,
    synchronize: unsafe extern "C" fn(Opaque) -> i32,
    host_register: unsafe extern "C" fn(*mut c_void, usize, u32) -> i32,
    host_unregister: unsafe extern "C" fn(*mut c_void) -> i32,
    host_device_pointer: unsafe extern "C" fn(*mut Opaque, *mut c_void, u32) -> i32,
    get_device: unsafe extern "C" fn(*mut i32) -> i32,
    device_attribute: unsafe extern "C" fn(*mut i32, i32, i32) -> i32,
    get_property: unsafe extern "C" fn(i32, *mut i32) -> Status,
    create: unsafe extern "C" fn(*mut Opaque) -> Status,
    destroy: unsafe extern "C" fn(Opaque) -> Status,
    state_create: unsafe extern "C" fn(Opaque, *mut Opaque, Opaque) -> Status,
    state_destroy: unsafe extern "C" fn(Opaque) -> Status,
    params_create: unsafe extern "C" fn(Opaque, *mut Opaque, Opaque) -> Status,
    params_destroy: unsafe extern "C" fn(Opaque) -> Status,
    set_quality: unsafe extern "C" fn(Opaque, i32, Opaque) -> Status,
    set_sampling: unsafe extern "C" fn(Opaque, i32, Opaque) -> Status,
    encode_image: unsafe extern "C" fn(
        Opaque,
        Opaque,
        Opaque,
        *const NvImage,
        i32,
        i32,
        i32,
        Opaque,
    ) -> Status,
    encode_yuv: unsafe extern "C" fn(
        Opaque,
        Opaque,
        Opaque,
        *const NvImage,
        i32,
        i32,
        i32,
        Opaque,
    ) -> Status,
    retrieve: unsafe extern "C" fn(Opaque, Opaque, *mut u8, *mut usize, Opaque) -> Status,
    // Keep the libraries loaded for as long as the copied function pointers live.
    _libraries: (libloading::Library, libloading::Library),
}

/// Matching CUDA runtime and nvJPEG library names, newest major first.
fn library_pairs() -> Vec<(String, String)> {
    let mut directories = vec![String::new()];
    if let Some(dir) = std::env::var_os("CAPTUREFAB_CUDA_LIB_DIR") {
        directories.push(format!("{}/", dir.to_string_lossy()));
    }
    let mut pairs = Vec::new();
    if cfg!(windows) {
        if let Some(cuda) = std::env::var_os("CUDA_PATH") {
            let cuda = cuda.to_string_lossy().into_owned();
            directories.extend([format!("{cuda}\\bin\\x64\\"), format!("{cuda}\\bin\\")]);
        }
        for (cudart, nvjpeg) in [
            ("cudart64_13.dll", "nvjpeg64_13.dll"),
            ("cudart64_12.dll", "nvjpeg64_12.dll"),
            ("cudart64_110.dll", "nvjpeg64_11.dll"),
        ] {
            for dir in &directories {
                pairs.push((format!("{dir}{cudart}"), format!("{dir}{nvjpeg}")));
            }
        }
    } else {
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
            ("libcudart.so", "libnvjpeg.so"),
        ] {
            for dir in &directories {
                pairs.push((format!("{dir}{cudart}"), format!("{dir}{nvjpeg}")));
            }
        }
    }
    pairs
}

impl Api {
    fn load() -> Result<Self> {
        let mut last = None;
        for (cudart, nvjpeg) in library_pairs() {
            // SAFETY: loading the vendor's CUDA runtime and nvJPEG runs their
            // initializers, which is their documented use.
            match unsafe {
                (
                    libloading::Library::new(&cudart),
                    libloading::Library::new(&nvjpeg),
                )
            } {
                (Ok(cudart), Ok(nvjpeg)) => return Self::bind(cudart, nvjpeg),
                (Err(e), _) | (_, Err(e)) => last = Some(e),
            }
        }
        bail!(
            "CUDA runtime and nvJPEG libraries not found{}",
            last.map(|e| format!(" ({e})")).unwrap_or_default()
        )
    }
    fn bind(cudart: libloading::Library, nvjpeg: libloading::Library) -> Result<Self> {
        // SAFETY: each signature mirrors the CUDA runtime / nvjpeg.h C prototype
        // (all handles are opaque pointers, enums are C ints, size_t is usize).
        unsafe {
            macro_rules! symbol {
                ($library:expr, $name:literal) => {
                    *$library
                        .get(concat!($name, "\0").as_bytes())
                        .with_context(|| concat!("missing symbol ", $name))?
                };
            }
            Ok(Self {
                malloc: symbol!(cudart, "cudaMalloc"),
                free: symbol!(cudart, "cudaFree"),
                memcpy: symbol!(cudart, "cudaMemcpy"),
                synchronize: symbol!(cudart, "cudaStreamSynchronize"),
                host_register: symbol!(cudart, "cudaHostRegister"),
                host_unregister: symbol!(cudart, "cudaHostUnregister"),
                host_device_pointer: symbol!(cudart, "cudaHostGetDevicePointer"),
                get_device: symbol!(cudart, "cudaGetDevice"),
                device_attribute: symbol!(cudart, "cudaDeviceGetAttribute"),
                get_property: symbol!(nvjpeg, "nvjpegGetProperty"),
                create: symbol!(nvjpeg, "nvjpegCreateSimple"),
                destroy: symbol!(nvjpeg, "nvjpegDestroy"),
                state_create: symbol!(nvjpeg, "nvjpegEncoderStateCreate"),
                state_destroy: symbol!(nvjpeg, "nvjpegEncoderStateDestroy"),
                params_create: symbol!(nvjpeg, "nvjpegEncoderParamsCreate"),
                params_destroy: symbol!(nvjpeg, "nvjpegEncoderParamsDestroy"),
                set_quality: symbol!(nvjpeg, "nvjpegEncoderParamsSetQuality"),
                set_sampling: symbol!(nvjpeg, "nvjpegEncoderParamsSetSamplingFactors"),
                encode_image: symbol!(nvjpeg, "nvjpegEncodeImage"),
                encode_yuv: symbol!(nvjpeg, "nvjpegEncodeYUV"),
                retrieve: symbol!(nvjpeg, "nvjpegEncodeRetrieveBitstream"),
                _libraries: (cudart, nvjpeg),
            })
        }
    }
}

fn status(what: &str, code: i32) -> Result<()> {
    ensure!(code == 0, "{what} failed with status {code}");
    Ok(())
}

struct Encoder {
    api: Api,
    handle: Opaque,
    state: Opaque,
    params: Opaque,
    device: Opaque,
    capacity: usize,
    /// The GPU shares system memory (Jetson) and can address page-locked
    /// host memory, so it reads mapped pixels in place.
    integrated: bool,
    /// Start of the page-locked shared buffer, else null. Uploads from
    /// page-locked memory are a single DMA rather than a copy through the
    /// driver's staging buffer.
    pinned: *mut u8,
    /// Device address of `pinned` on an integrated GPU, else null.
    in_place: Opaque,
}

impl Encoder {
    fn new() -> Result<(Self, String)> {
        let api = Api::load()?;
        let mut encoder = Self {
            api,
            handle: std::ptr::null_mut(),
            state: std::ptr::null_mut(),
            params: std::ptr::null_mut(),
            device: std::ptr::null_mut(),
            capacity: 0,
            integrated: false,
            pinned: std::ptr::null_mut(),
            in_place: std::ptr::null_mut(),
        };
        // SAFETY: out-pointers are valid locals; created objects are owned by
        // `encoder` and released in Drop even if a later step fails.
        let version = unsafe {
            let api = &encoder.api;
            let (mut major, mut minor) = (0, 0);
            (api.get_property)(MAJOR_VERSION, &mut major);
            (api.get_property)(MINOR_VERSION, &mut minor);
            status("nvjpegCreateSimple", (api.create)(&mut encoder.handle))?;
            status(
                "nvjpegEncoderStateCreate",
                (api.state_create)(encoder.handle, &mut encoder.state, STREAM),
            )?;
            status(
                "nvjpegEncoderParamsCreate",
                (api.params_create)(encoder.handle, &mut encoder.params, STREAM),
            )?;
            status(
                "nvjpegEncoderParamsSetQuality",
                (api.set_quality)(encoder.params, QUALITY, STREAM),
            )?;
            let (mut device, mut integrated, mut mappable) = (0, 0, 0);
            encoder.integrated = (api.get_device)(&mut device) == 0
                && (api.device_attribute)(&mut integrated, CUDA_ATTRIBUTE_INTEGRATED, device) == 0
                && (api.device_attribute)(
                    &mut mappable,
                    CUDA_ATTRIBUTE_CAN_MAP_HOST_MEMORY,
                    device,
                ) == 0
                && integrated == 1
                && mappable == 1;
            format!("{major}.{minor}")
        };
        Ok((encoder, version))
    }
    /// Encode `pixels` and write the JPEG into `out`, returning its length.
    /// `in_place` is the device address of `pixels` on an integrated GPU,
    /// which then reads them without an upload.
    fn encode_pixels(
        &mut self,
        request: &Request,
        pixels: &[u8],
        in_place: Opaque,
        out: &mut [u8],
    ) -> Result<usize> {
        let api = &self.api;
        let (width, height) = (request.width as i32, request.height as i32);
        // SAFETY: the image descriptor points either at `in_place`, the
        // device view of `pixels`, or into `device`, which holds at least
        // `pixels.len()` bytes after the (re)allocation below; all calls use
        // the legacy default stream, synchronized before retrieval.
        unsafe {
            let mut image = NvImage {
                channel: [std::ptr::null_mut(); 4],
                pitch: [0; 4],
            };
            image.pitch[0] = request.width as usize * request.channels;
            if !in_place.is_null() {
                image.channel[0] = in_place.cast();
            } else {
                if pixels.len() > self.capacity {
                    if !self.device.is_null() {
                        (api.free)(self.device);
                        self.device = std::ptr::null_mut();
                        self.capacity = 0;
                    }
                    status("cudaMalloc", (api.malloc)(&mut self.device, pixels.len()))?;
                    self.capacity = pixels.len();
                }
                status(
                    "cudaMemcpy",
                    (api.memcpy)(
                        self.device,
                        pixels.as_ptr().cast(),
                        pixels.len(),
                        CUDA_MEMCPY_HOST_TO_DEVICE,
                    ),
                )?;
                image.channel[0] = self.device.cast();
            }
            if request.channels == 1 {
                status(
                    "nvjpegEncoderParamsSetSamplingFactors",
                    (api.set_sampling)(self.params, NVJPEG_CSS_GRAY, STREAM),
                )?;
                status(
                    "nvjpegEncodeYUV",
                    (api.encode_yuv)(
                        self.handle,
                        self.state,
                        self.params,
                        &image,
                        NVJPEG_CSS_GRAY,
                        width,
                        height,
                        STREAM,
                    ),
                )?;
            } else {
                status(
                    "nvjpegEncoderParamsSetSamplingFactors",
                    (api.set_sampling)(self.params, NVJPEG_CSS_420, STREAM),
                )?;
                status(
                    "nvjpegEncodeImage",
                    (api.encode_image)(
                        self.handle,
                        self.state,
                        self.params,
                        &image,
                        NVJPEG_INPUT_RGBI,
                        width,
                        height,
                        STREAM,
                    ),
                )?;
            }
            status("cudaStreamSynchronize", (api.synchronize)(STREAM))?;
            let mut length = 0usize;
            status(
                "nvjpegEncodeRetrieveBitstream",
                (api.retrieve)(
                    self.handle,
                    self.state,
                    std::ptr::null_mut(),
                    &mut length,
                    STREAM,
                ),
            )?;
            ensure!(length <= out.len(), "JPEG exceeds the shared buffer");
            status(
                "nvjpegEncodeRetrieveBitstream",
                (api.retrieve)(
                    self.handle,
                    self.state,
                    out.as_mut_ptr(),
                    &mut length,
                    STREAM,
                ),
            )?;
            status("cudaStreamSynchronize", (api.synchronize)(STREAM))?;
            Ok(length)
        }
    }
}
impl accel::Encoder for Encoder {
    /// Page-lock a new mapping for transfers; on failure (some kernels refuse
    /// to pin file-backed pages) it stays pageable and is copied as before.
    fn attach(&mut self, map: &mut memmap2::MmapMut) {
        if std::env::var_os("CAPTUREFAB_NVJPEG_PIN").is_some_and(|v| v == "0") {
            return;
        }
        let api = &self.api;
        let flags = if self.integrated {
            CUDA_HOST_REGISTER_MAPPED
        } else {
            CUDA_HOST_REGISTER_DEFAULT
        };
        // SAFETY: the range is this process's live mapping, unregistered in
        // `detach` before it is unmapped.
        unsafe {
            let base = map.as_mut_ptr();
            if (api.host_register)(base.cast(), map.len(), flags) != 0 {
                return;
            }
            self.pinned = base;
            let mut device = std::ptr::null_mut();
            if self.integrated && (api.host_device_pointer)(&mut device, base.cast(), 0) == 0 {
                self.in_place = device;
            }
        }
    }
    fn detach(&mut self, map: &mut memmap2::MmapMut) {
        if !self.pinned.is_null() && self.pinned == map.as_mut_ptr() {
            // SAFETY: registered by `attach` and still mapped.
            unsafe { (self.api.host_unregister)(self.pinned.cast()) };
            (self.pinned, self.in_place) = (std::ptr::null_mut(), std::ptr::null_mut());
        }
    }
    fn encode(&mut self, request: &Request, pixels: &[u8], out: &mut [u8]) -> Result<usize> {
        // The pixels start the shared buffer; on an integrated GPU nvJPEG
        // reads them through its device view.
        let in_place = if pixels.as_ptr() == self.pinned.cast_const() {
            self.in_place
        } else {
            std::ptr::null_mut()
        };
        self.encode_pixels(request, pixels, in_place, out)
    }
}
impl Drop for Encoder {
    fn drop(&mut self) {
        let api = &self.api;
        // SAFETY: each object was created by this encoder and is freed once.
        unsafe {
            if !self.device.is_null() {
                (api.free)(self.device);
            }
            if !self.params.is_null() {
                (api.params_destroy)(self.params);
            }
            if !self.state.is_null() {
                (api.state_destroy)(self.state);
            }
            if !self.handle.is_null() {
                (api.destroy)(self.handle);
            }
        }
    }
}
