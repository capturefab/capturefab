# Building the bundled FFmpeg payload

Capturefab's camera control, GenICam interpreter, GigE transport, and USB3 Vision transport are Rust. Compressed media and RTSP/SRT forwarding use an optional FFmpeg executable embedded into the release executable at build time. The end user installs one Capturefab executable; the embedded payload is extracted to the user's cache when needed. There is no runtime dependency on a separately installed FFmpeg or camera SDK.

`scripts/build-ffmpeg.sh` builds FFmpeg 9.0.2, x264 commit `b35605ace3ddf7c1a5d67a2eb553f034aef41d55` (r3222), SRT 1.5.7, and OpenSSL 3.6.4 from their official upstream archives. Every archive has a pinned SHA-256 in the script. This is a repeatable source recipe with fixed inputs, not a promise of identical binary bytes across different compilers, SDKs, build paths, or operating systems. [FFmpeg releases](https://ffmpeg.org/releases/), [x264](https://www.videolan.org/developers/x264.html), [SRT release](https://github.com/Haivision/srt/releases/tag/v1.5.7), [OpenSSL source](https://openssl-library.org/source/).

The build enables GPLv3, H.264 encoding with libx264, SRT with OpenSSL encryption, HTTPS, RTSP, UDP/RTP, FFmpeg's native decoders, and its native format support. It enables AVFoundation and VideoToolbox on macOS, V4L2 and V4L2 mem2mem on Linux, and DirectShow/Media Foundation on Windows. External library autodetection is disabled and pkg-config is isolated to the freshly built static libraries, preventing an installed package from silently becoming a runtime dependency. macOS retains its operating system libraries/frameworks; Linux normally retains libc and its related operating system libraries; Windows retains operating system DLLs. SRT, x264, and OpenSSL are linked statically.

`ENABLE_NVENC=1` enables NVIDIA NVENC/NVDEC/CUVID using pinned FFmpeg `nv-codec-headers` 11.1.5.4. It defaults to 1 on Linux/Windows x86_64 and ARM64, and 0 elsewhere. These are build-only MIT-licensed headers; FFmpeg loads the installed GPU driver's libraries when that backend is used. The older API is deliberately selected to match FFmpeg's supported minimum and preserve compatibility with drivers starting at Linux 470.57.02 / Windows 471.41. A build that lists an encoder still needs matching hardware and a working driver. This does not establish Jetson NVENC support: the portable Jetson path is V4L2 where the driver exposes a compatible encoder, with software encoding available otherwise. [Header release requirements](https://github.com/FFmpeg/nv-codec-headers/blob/n11.1.5.4/README).

`ENABLE_AMF=1` enables AMD AMF using pinned upstream AMF 1.5.3 headers and defaults to 1 on Windows x86_64. Only the headers and license are extracted from the upstream SDK archive; SDK sample executables are never installed or bundled. This enables an optional driver API and adds no linked AMD runtime library. `ENABLE_MF=1` defaults to 1 on Windows and enables system Media Foundation encoders such as `h264_mf`, including where a Windows ARM64/Qualcomm device exposes an appropriate system encoder. Set any of these switches to 0 to omit it. Their actual GPU/device behavior must be tested on the deployment hardware. [AMF release](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/releases/tag/v1.5.3), [FFmpeg platform features](https://ffmpeg.org/ffmpeg-codecs.html).

## Native build

Build prerequisites are Bash 3.2 or newer, C and C++ compilers, make, CMake 3.15 or newer, pkg-config, Perl, curl, tar with xz support, and `sha256sum` or `shasum`. NASM is optional on x86: without it the script selects portable scalar code. These tools are build requirements only.

```sh
JOBS=8 scripts/build-ffmpeg.sh "$PWD/target/ffmpeg-bundle"
CAPTUREFAB_FFMPEG_BINARY="$PWD/target/ffmpeg-bundle/bin/ffmpeg" \
CAPTUREFAB_FFMPEG_LICENSE="$PWD/target/ffmpeg-bundle/licenses/NOTICE.txt" \
    cargo build --release
```

Without `CAPTUREFAB_FFMPEG_BINARY`, release-profile builds embed `target/ffmpeg/<target>/bin/ffmpeg[.exe]` when it exists. Dev and test builds never embed that file; they run it in place, so debug binaries stay small and relink quickly. Set `CAPTUREFAB_FFMPEG_BINARY` to embed FFmpeg in any profile.

On Windows, run from an MSYS2 MinGW/UCRT shell with the matching target compiler and CMake available, using `CC=gcc CXX=g++`. The output is `bin/ffmpeg.exe`; use that filename for `CAPTUREFAB_FFMPEG_BINARY`. Do not use the MSYS compiler, which adds a dependency on `msys-2.0.dll`. Native Windows ARM64 requires an ARM64 MinGW toolchain, such as LLVM-MinGW, with matching CMake compiler settings. The recipe does not use MSVC.

The script recognizes macOS Intel/ARM64, Linux x86_64/ARM64/ARMv7 (including Raspberry Pi and Jetson), and MinGW x86_64/ARM64. A native build on the oldest supported deployment system gives the clearest libc/SDK compatibility baseline. macOS defaults to deployment target 11.0 on ARM64 and 10.15 on Intel. Raspberry Pi 32-bit builds use ARMv7; 64-bit Raspberry Pi and Jetson builds use ARM64. Hardware encoder availability still depends on the operating system and device driver. Jetson-specific NVIDIA encoder support is not asserted by this portable recipe.

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

Confirm `srt` appears under both input and output, and `libx264` appears in the encoders. Inspect linked libraries with `otool -L` on macOS, `readelf -d` on Linux, or the target's `objdump -p` on Windows. Native builds perform these checks automatically. Protocol enumeration and an encoder smoke test do not substitute for an actual RTSP/SRT network roundtrip on the deployment target.

## Distribution and validation

The combined payload is GPLv3 or later. Keep the exact source archives, Capturefab source, build script, configuration, and upstream notices with each published release's corresponding-source materials. FFmpeg is built with `--enable-gpl --enable-version3`; SRT is MPLv2, OpenSSL is Apache 2.0, and x264 is GPLv2 or later. [FFmpeg licensing](https://ffmpeg.org/legal.html), [SRT license](https://github.com/Haivision/srt/blob/v1.5.7/LICENSE), [OpenSSL license](https://github.com/openssl/openssl/blob/openssl-3.6.4/LICENSE.txt).

As of 2026-10-04, a macOS ARM64 FFmpeg 9.0.2 build has been verified to provide both SRT directions, encode with libx264, and link only macOS operating system libraries/frameworks. Other target recipes need target-specific build and runtime validation before release; they are not claimed as tested binaries.
