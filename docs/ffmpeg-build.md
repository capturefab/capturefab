# Building the bundled FFmpeg payload

Capturefab uses FFmpeg for webcams, network streams, video files and recording. Release binaries
include a separate FFmpeg executable inside `capturefab`. Capturefab extracts it when a command
uses it. GigE Vision, USB3 Vision and GenICam use Capturefab's native Rust code.

The embedded copy uses lossless zlib compression. `build.rs` records the original length and
SHA-256. Release packaging decompresses the copy and compares it with the original executable.

## Where Capturefab runs FFmpeg

Capturefab verifies the extracted copy by length and SHA-256. Later processes reuse that copy and
compare its file metadata with a verification marker. Capturefab can extract it again if the
copy changes.

Set `CAPTUREFAB_CACHE_DIR` to select the cache folder. The defaults are:

| System | Default cache |
| --- | --- |
| macOS | `~/Library/Caches/capturefab` |
| Linux | `$XDG_CACHE_HOME/capturefab` or `~/.cache/capturefab` |
| Windows | `%LOCALAPPDATA%\capturefab\cache` |

If that folder is unusable, Capturefab tries the runtime cache and then a temporary cache.
It skips directories on mounts that disallow execution. If the embedded copy cannot run,
Capturefab tries an `ffmpeg` beside its executable, then on `PATH`.
Run `capturefab doctor --json` to see `ffmpeg.source` and any `ffmpeg.bundled_error`.
If no FFmpeg can run, media commands return `ffmpeg_missing`.

To select a specific FFmpeg executable, set `CAPTUREFAB_FFMPEG` to its absolute path.
Capturefab uses that executable directly. On Linux and macOS, it caches successful encoder and
hardware decoder probes for the rest of the boot. A changed FFmpeg binary gets new probes.

`scripts/build-ffmpeg.sh` builds FFmpeg 9.0.2, x264 r3222, SRT 1.5.7, OpenSSL 3.6.4 and zlib
1.3.1. The script pins the source archives by SHA-256. Build output can vary with the compiler,
SDK, build path and operating system.

The script links SRT, x264, OpenSSL and zlib statically. It disables FFmpeg's automatic detection
of system libraries and limits pkg-config to the build's dependencies. The build can still use
system libraries, such as libc on Linux, system frameworks on macOS and system DLLs on Windows.

## Camera-focused component policy

The payload retains all native decoders, demuxers, parsers, bitstream filters,
network protocols and hardware decoding backends. This deliberately preserves
unusual webcam formats, older IP cameras, image sequences and existing media
files rather than limiting inputs to H.264/H.265/MJPEG. Native camera inputs
remain AVFoundation, V4L2 and DirectShow, plus the synthetic lavfi input.

The bundled build enables lossless FFV1, software H.264 (libx264) and platform-specific video
encoders. It also retains raw video, PPM and the test frame wrapper. Capturefab uses its own
JPEG and PNG encoders for stills. The bundled FFmpeg uses the filters necessary for capture,
recording and validation.

HEVC and AV1 encoding use hardware. If no encoder works, Capturefab reports the codecs it can
use. The bundled build omits audio and subtitle encoders, audio and screen capture, and device
playback.

By default, native builds verify the available components and the recording clock. They encode
H.264 to MP4, Matroska, MPEG-TS and FLV. They decode each container to raw RGB and PPM.
Run the validation on the target after a cross build.

### Hardware encoder switches

| Switch | Enabled by default on | FFmpeg support |
| --- | --- | --- |
| `ENABLE_NVENC` | Linux and Windows x86_64 and ARM64 | NVIDIA NVENC, NVDEC and CUVID with `nv-codec-headers` 12.0.16.2. |
| `ENABLE_VULKAN` | Linux and Windows | Vulkan Video with Khronos Vulkan-Headers 1.4.363.0. |
| `ENABLE_QSV` | Windows x86_64 | Intel Quick Sync with libvpl 2.17.0 and D3D11VA/DXVA2. |
| `ENABLE_AMF` | Linux and Windows x86_64 | AMD AMF with AMF 1.5.3 headers. |
| `ENABLE_MF` | Windows | Media Foundation and D3D11VA. |

Set a switch to `0` to omit that backend. An enabled encoder still depends on the target's GPU
and driver. Verify it with a recording on the target. See the [NVENC header requirements](https://github.com/FFmpeg/nv-codec-headers/blob/n12.0.16.2/README)
and [FFmpeg codec documentation](https://ffmpeg.org/ffmpeg-codecs.html).

Capturefab can use NVIDIA's GStreamer elements for encoding on Jetson. Linux nvJPEG and VA-API
support for stills comes from Capturefab's own JPEG helper process.

## Native build

Build prerequisites are Bash 3.2 or newer, C and C++ compilers, make, CMake 3.15 or newer, pkg-config, Perl, curl, tar with xz support, and `sha256sum` or `shasum`. NASM is optional on x86: without it the script selects portable scalar code. These tools are build requirements only.

```sh
JOBS=8 scripts/build-ffmpeg.sh "$PWD/target/ffmpeg-bundle"
CAPTUREFAB_FFMPEG_BINARY="$PWD/target/ffmpeg-bundle/bin/ffmpeg" \
CAPTUREFAB_FFMPEG_LICENSE="$PWD/target/ffmpeg-bundle/licenses/NOTICE.txt" \
    cargo build --release
```

`CAPTUREFAB_FFMPEG_BINARY` selects the binary to embed in a Cargo build. If this variable is unset,
a release build embeds `target/ffmpeg/<target>/bin/ffmpeg[.exe]` when that file exists.
`build.rs` can embed a prebuilt executable. Run the build script first to produce one.

For Windows x86_64, run the script from an MSYS2 MinGW or UCRT shell with the target compiler and
CMake available. Set `CC=gcc CXX=g++`. For Windows ARM64, use the MSYS2 CLANGARM64 environment:

```sh
TARGET_ARCH=aarch64 CC=clang CXX=clang++ AR=llvm-ar RANLIB=llvm-ranlib \
    STRIP=llvm-strip OBJDUMP=llvm-objdump \
    scripts/build-ffmpeg.sh "$PWD/target/ffmpeg-bundle"
```

Set `TARGET_ARCH` explicitly because `uname -m` in the emulated shell can report x86_64.
On Windows, set `CAPTUREFAB_FFMPEG_BINARY` to the resulting `bin/ffmpeg.exe`.

The script accepts macOS Intel and ARM64, Linux x86_64, ARM64 and ARMv7, and MinGW x86_64 and ARM64.
The default macOS deployment targets are 11.0 on ARM64 and 10.15 on Intel. Choose ARMv7 for a
32-bit Raspberry Pi system. Choose ARM64 for a 64-bit Raspberry Pi or Jetson system.

The output includes:

- `bin/ffmpeg` or `bin/ffmpeg.exe` for embedding;
- `sources/` with the exact corresponding source archives, unless `DOWNLOAD_DIR` points to a shared archive cache;
- `licenses/` with upstream licenses and GPU header notices when enabled;
- `logs/` with configuration, compiler, protocol, encoder, and linked-library checks;
- `build-manifest.txt` with versions, target, compiler information, configure flags, and the final binary hash.

`BUILD_DIR`, `DOWNLOAD_DIR`, and `JOBS` select build storage, source cache, and parallelism. Use a separate build/output directory for each target. The script intentionally rebuilds dependencies so that an old installed archive cannot masquerade as the requested version.

## Linux cross builds

All dependencies must use the same target toolchain. Supply the cross prefix, target triple, and a CMake toolchain file. For example, an ARM64 GNU/Linux build from an x86_64 Linux host:

```sh
TARGET_OS=linux TARGET_ARCH=aarch64 \
CROSS_PREFIX=aarch64-linux-gnu- HOST_TRIPLE=aarch64-linux-gnu \
CC=aarch64-linux-gnu-gcc CXX=aarch64-linux-gnu-g++ \
CMAKE_TOOLCHAIN_FILE="$PWD/aarch64-linux.cmake" \
    scripts/build-ffmpeg.sh "$PWD/target/ffmpeg-aarch64"
```

Example `aarch64-linux.cmake`:

```cmake
set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR aarch64)
set(CMAKE_C_COMPILER aarch64-linux-gnu-gcc)
set(CMAKE_CXX_COMPILER aarch64-linux-gnu-g++)
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
```

For ARMv7 use `TARGET_ARCH=arm`, `arm-linux-gnueabihf-`, and an ARM toolchain file. For Windows cross builds use `TARGET_OS=mingw32`, a matching MinGW prefix/triple, and `CMAKE_SYSTEM_NAME Windows`. Windows ARM64 disables OpenSSL assembly so the portable C implementation is used. For macOS builds use native macOS build hosts and the desired SDK/architecture; Apple SDK cross compilation from Linux is not provided.

`FULL_STATIC=1` requests a completely static Linux payload when the entire target toolchain supplies static libc and C++ runtime libraries. A musl toolchain is suitable for this mode. The default Linux build statically links the third-party libraries and C++ runtime while retaining the operating system's libc. It checks that the result has no shared SRT, OpenSSL, x264, C++ runtime, or GCC runtime dependency.

Cross builds default to `RUN_CHECKS=0` because a host cannot execute a different target's binary. Run the resulting binary on its target and check:

```sh
./ffmpeg -hide_banner -protocols
./ffmpeg -hide_banner -encoders
./ffmpeg -hide_banner -loglevel error -f lavfi \
    -i testsrc2=size=64x64:rate=2 -frames:v 2 -c:v libx264 -f null -
```

Make sure `srt` appears under input and output protocols. Make sure `libx264` appears in the
encoders. Examine linked libraries with `otool -L` on macOS, `readelf -d` on Linux or `objdump -p`
on Windows. Test RTSP or SRT on the target if you use it for streaming.

## Distribution and validation

The combined FFmpeg payload is GPLv3 or later. Capturefab uses the MIT license and runs FFmpeg
as a separate executable. Keep the exact source archives, build script, configuration and
upstream notices in each release's corresponding-source materials.

The build uses `--enable-gpl --enable-version3`. SRT uses MPLv2. OpenSSL uses Apache 2.0.
zlib uses the zlib License. x264 uses GPLv2 or later.
[FFmpeg licensing](https://ffmpeg.org/legal.html), [SRT license](https://github.com/Haivision/srt/blob/v1.5.7/LICENSE), [OpenSSL license](https://github.com/openssl/openssl/blob/openssl-3.6.4/LICENSE.txt).
