#!/usr/bin/env bash
# Build the optional GPLv3 media payload from pinned upstream source.
# No third-party shared libraries are installed or linked.
set -euo pipefail

FFMPEG_VERSION=9.0.2
OPENSSL_VERSION=3.6.4
SRT_VERSION=1.5.7
X264_COMMIT=b35605ace3ddf7c1a5d67a2eb553f034aef41d55
FFMPEG_SHA256=8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e
OPENSSL_SHA256=9bffaa1ad1e07b354c21bd3324ec02fa15579f45a7d0494b3e74bc449b7333ef
SRT_SHA256=017cd1e437ef2073a4dd10ddf7b55e86bc3d6ebac0393d13bd22f6a57055d32b
X264_SHA256=cd71a7515b0e9a012e1ac9b1f8415bebcaf6fc97d4db32286642ac4c0fbe24f9
NV_HEADERS_VERSION=11.1.5.4
NV_HEADERS_SHA256=cbad7c68365ae50b03fe4cfbea05975c94406bdcc0a995bd094a3ea355656ffb
AMF_VERSION=1.5.3
AMF_COMMIT=8c648005e07d4309033282bfd9947df2c7e76104
AMF_SHA256=65e06bbbc515c3125cffd89fe0a3639a2fedc4d8c7423fc82a60218295a3cc31
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

die() { printf 'build-ffmpeg: %s\n' "$*" >&2; exit 1; }
if [[ ${1:-} == --help ]]; then
    cat <<'HELP'
Usage: scripts/build-ffmpeg.sh [OUTPUT_DIRECTORY]

Build pinned FFmpeg, x264, SRT, and OpenSSL from source. Output: bin/ffmpeg,
source archives, licenses, build logs, and a manifest. Requires Bash, a C/C++
compiler, make, CMake >= 3.15, pkg-config, Perl, curl, tar, and xz.

Environment: BUILD_DIR, DOWNLOAD_DIR, JOBS, CC, CXX, AR, RANLIB, STRIP,
TARGET_OS (darwin/linux/mingw32), TARGET_ARCH (x86_64/aarch64/arm),
CROSS_PREFIX, HOST_TRIPLE, OPENSSL_TARGET, CMAKE_TOOLCHAIN_FILE,
MACOSX_DEPLOYMENT_TARGET, FULL_STATIC (Linux, default 0), RUN_CHECKS (0/1).
ENABLE_NVENC (default 1 on Linux/Windows x64/ARM64), ENABLE_AMF (default
1 on Windows x64), ENABLE_MF (default 1 on Windows), all accept 0 or 1.
Cross builds require all matching toolchains; see docs/ffmpeg-build.md.
HELP
    exit 0
fi
[[ $# -le 1 ]] || die 'expected one optional output directory'
for tool in curl tar make cmake pkg-config perl; do
    command -v "$tool" >/dev/null || die "missing build tool: $tool"
done

OUTPUT_DIR=${1:-"$PWD/target/ffmpeg-bundle"}
mkdir -p "$OUTPUT_DIR"
OUTPUT_DIR=$(cd "$OUTPUT_DIR" && pwd)
BUILD_DIR=${BUILD_DIR:-"$OUTPUT_DIR/build"}
DOWNLOAD_DIR=${DOWNLOAD_DIR:-"$OUTPUT_DIR/sources"}
mkdir -p "$BUILD_DIR" "$DOWNLOAD_DIR" "$OUTPUT_DIR/bin" "$OUTPUT_DIR/licenses" "$OUTPUT_DIR/logs"
BUILD_DIR=$(cd "$BUILD_DIR" && pwd)
DOWNLOAD_DIR=$(cd "$DOWNLOAD_DIR" && pwd)
PREFIX="$BUILD_DIR/deps"
mkdir -p "$PREFIX"

case ${TARGET_OS:-$(uname -s)} in
    Darwin|darwin) TARGET_OS=darwin ;;
    Linux|linux) TARGET_OS=linux ;;
    MINGW*|MSYS*|mingw32|windows) TARGET_OS=mingw32 ;;
    *) die 'set TARGET_OS to darwin, linux, or mingw32' ;;
esac
case ${TARGET_ARCH:-$(uname -m)} in
    arm64|aarch64) TARGET_ARCH=aarch64 ;;
    x86_64|amd64) TARGET_ARCH=x86_64 ;;
    arm|armv7l|armv7*) TARGET_ARCH=arm ;;
    *) die 'set TARGET_ARCH to x86_64, aarch64, or arm' ;;
esac
CROSS_PREFIX=${CROSS_PREFIX:-}
CC=${CC:-"${CROSS_PREFIX}cc"}
CXX=${CXX:-"${CROSS_PREFIX}c++"}
AR=${AR:-"${CROSS_PREFIX}ar"}
RANLIB=${RANLIB:-"${CROSS_PREFIX}ranlib"}
STRIP=${STRIP:-"${CROSS_PREFIX}strip"}
for tool in "$CC" "$CXX" "$AR" "$RANLIB"; do
    command -v "$tool" >/dev/null || die "missing target tool: $tool"
done
export CC CXX AR RANLIB
JOBS=${JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || printf 2)}
[[ $JOBS =~ ^[1-9][0-9]*$ ]] || die 'JOBS must be a positive integer'
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-1750003200}
export LC_ALL=C TZ=UTC
# Isolate pkg-config from installed shared libraries on the build host.
export PKG_CONFIG_PATH=
export PKG_CONFIG_LIBDIR="$PREFIX/lib/pkgconfig:$PREFIX/lib64/pkgconfig"
nvenc_default=0
amf_default=0
mf_default=0
if [[ $TARGET_OS != darwin && $TARGET_ARCH != arm ]]; then nvenc_default=1; fi
if [[ $TARGET_OS == mingw32 ]]; then
    mf_default=1
    [[ $TARGET_ARCH != x86_64 ]] || amf_default=1
fi
ENABLE_NVENC=${ENABLE_NVENC:-$nvenc_default}
ENABLE_AMF=${ENABLE_AMF:-$amf_default}
ENABLE_MF=${ENABLE_MF:-$mf_default}
for toggle in "$ENABLE_NVENC" "$ENABLE_AMF" "$ENABLE_MF"; do
    [[ $toggle == 0 || $toggle == 1 ]] || die 'hardware switches must be 0 or 1'
done
[[ $ENABLE_NVENC == 0 || ( $TARGET_OS != darwin && $TARGET_ARCH != arm ) ]] || die 'NVENC requires Linux/Windows x64 or ARM64'
[[ $ENABLE_AMF == 0 || $TARGET_OS != darwin ]] || die 'AMF requires Linux or Windows'
[[ $ENABLE_MF == 0 || $TARGET_OS == mingw32 ]] || die 'Media Foundation requires Windows'

digest() {
    if command -v sha256sum >/dev/null; then sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null; then shasum -a 256 "$1" | awk '{print $1}'
    else die 'sha256sum or shasum is required'; fi
}
fetch() {
    local url=$1 path=$2 expected=$3
    if [[ ! -f $path ]]; then
        curl --fail --location --retry 3 --proto '=https' --tlsv1.2 "$url" -o "$path.partial"
        [[ $(digest "$path.partial") == "$expected" ]] || die "SHA-256 mismatch: $url"
        mv "$path.partial" "$path"
    fi
    [[ $(digest "$path") == "$expected" ]] || die "SHA-256 mismatch: $path"
}
fetch "https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz" "$DOWNLOAD_DIR/ffmpeg-$FFMPEG_VERSION.tar.xz" "$FFMPEG_SHA256"
fetch "https://github.com/openssl/openssl/releases/download/openssl-$OPENSSL_VERSION/openssl-$OPENSSL_VERSION.tar.gz" "$DOWNLOAD_DIR/openssl-$OPENSSL_VERSION.tar.gz" "$OPENSSL_SHA256"
fetch "https://github.com/Haivision/srt/archive/refs/tags/v$SRT_VERSION.tar.gz" "$DOWNLOAD_DIR/srt-$SRT_VERSION.tar.gz" "$SRT_SHA256"
fetch "https://code.videolan.org/videolan/x264/-/archive/$X264_COMMIT/x264-$X264_COMMIT.tar.gz" "$DOWNLOAD_DIR/x264-$X264_COMMIT.tar.gz" "$X264_SHA256"
for archive in "$DOWNLOAD_DIR/ffmpeg-$FFMPEG_VERSION.tar.xz" "$DOWNLOAD_DIR/openssl-$OPENSSL_VERSION.tar.gz" "$DOWNLOAD_DIR/srt-$SRT_VERSION.tar.gz" "$DOWNLOAD_DIR/x264-$X264_COMMIT.tar.gz"; do
    tar -xf "$archive" -C "$BUILD_DIR"
done
if [[ $ENABLE_NVENC == 1 ]]; then
    fetch "https://github.com/FFmpeg/nv-codec-headers/archive/refs/tags/n$NV_HEADERS_VERSION.tar.gz" "$DOWNLOAD_DIR/nv-codec-headers-$NV_HEADERS_VERSION.tar.gz" "$NV_HEADERS_SHA256"
    tar -xf "$DOWNLOAD_DIR/nv-codec-headers-$NV_HEADERS_VERSION.tar.gz" -C "$BUILD_DIR"
    make -C "$BUILD_DIR/nv-codec-headers-n$NV_HEADERS_VERSION" PREFIX="$PREFIX" install > "$OUTPUT_DIR/logs/nv-codec-headers.log" 2>&1
    for header in "$BUILD_DIR/nv-codec-headers-n$NV_HEADERS_VERSION"/include/ffnvcodec/*.h; do
        sed -n '1,/^ \*\//p' "$header"
    done > "$OUTPUT_DIR/licenses/NVIDIA-headers-MIT.txt"
fi
if [[ $ENABLE_AMF == 1 ]]; then
    # Unmodified official headers and license, archived from AMF_COMMIT. The
    # full SDK contains hundreds of MiB of unrelated sample executables.
    amf_source="$SCRIPT_DIR/../third_party/amf-headers-$AMF_VERSION.tar.gz"
    [[ -f $amf_source && $(digest "$amf_source") == "$AMF_SHA256" ]] || die 'vendored AMF headers missing or checksum mismatch'
    cp "$amf_source" "$DOWNLOAD_DIR/amf-headers-$AMF_VERSION.tar.gz"
    tar -xf "$DOWNLOAD_DIR/amf-headers-$AMF_VERSION.tar.gz" -C "$BUILD_DIR"
    mkdir -p "$PREFIX/include/AMF"
    cp -R "$BUILD_DIR/amf-headers-$AMF_VERSION/amf/public/include/." "$PREFIX/include/AMF/"
    cp "$BUILD_DIR/amf-headers-$AMF_VERSION/LICENSE.txt" "$OUTPUT_DIR/licenses/AMD-AMF-MIT.txt"
fi

openssl_flags=(no-shared no-module no-tests --prefix="$PREFIX" --openssldir="$PREFIX/ssl" --libdir=lib)
ffmpeg_flags=(--prefix="$OUTPUT_DIR" --arch="$TARGET_ARCH" --target-os="$TARGET_OS"
    --cc="$CC" --cxx="$CXX" --ar="$AR" --ranlib="$RANLIB" --strip="$STRIP"
    --disable-autodetect --disable-shared --enable-static --enable-gpl --enable-version3
    --enable-libx264 --enable-libsrt --enable-openssl --pkg-config-flags=--static
    --disable-ffplay --disable-ffprobe --disable-doc --disable-debug)
if [[ $ENABLE_NVENC == 1 ]]; then ffmpeg_flags+=(--enable-ffnvcodec --enable-nvenc --enable-nvdec --enable-cuvid); fi
if [[ $ENABLE_AMF == 1 ]]; then ffmpeg_flags+=(--enable-amf); fi
if [[ $ENABLE_MF == 1 ]]; then ffmpeg_flags+=(--enable-mediafoundation); fi
x264_flags=(--prefix="$PREFIX" --enable-static --enable-pic --disable-cli --disable-opencl)
cmake_flags=(-DCMAKE_INSTALL_PREFIX="$PREFIX" -DCMAKE_INSTALL_LIBDIR=lib -DCMAKE_BUILD_TYPE=Release
    -DCMAKE_C_COMPILER="$CC" -DCMAKE_CXX_COMPILER="$CXX" -DENABLE_SHARED=OFF
    -DENABLE_STATIC=ON -DENABLE_APPS=OFF -DENABLE_ENCRYPTION=ON -DENABLE_TESTING=OFF
    -DSRT_USE_OPENSSL_STATIC_LIBS=ON -DOPENSSL_ROOT_DIR="$PREFIX" -DUSE_OPENSSL_PC=OFF)
link_flags="-L$PREFIX/lib"
case $TARGET_OS:$TARGET_ARCH in
    darwin:aarch64)
        OPENSSL_TARGET=${OPENSSL_TARGET:-darwin64-arm64-cc}
        export MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-11.0}
        ffmpeg_flags+=(--enable-videotoolbox --enable-avfoundation) ;;
    darwin:x86_64)
        OPENSSL_TARGET=${OPENSSL_TARGET:-darwin64-x86_64-cc}
        export MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-10.15}
        ffmpeg_flags+=(--enable-videotoolbox --enable-avfoundation) ;;
    linux:aarch64) OPENSSL_TARGET=${OPENSSL_TARGET:-linux-aarch64} ;;
    linux:x86_64) OPENSSL_TARGET=${OPENSSL_TARGET:-linux-x86_64} ;;
    linux:arm) OPENSSL_TARGET=${OPENSSL_TARGET:-linux-armv4} ;;
    mingw32:*)
        OPENSSL_TARGET=${OPENSSL_TARGET:-mingw64}
        link_flags="$link_flags -static -static-libgcc -static-libstdc++"
        ffmpeg_flags+=(--enable-indev=dshow)
        cmake_flags+=(-DENABLE_STDCXX_SYNC=ON)
        [[ $TARGET_ARCH != aarch64 ]] || openssl_flags+=(no-asm) ;;
    *) die 'unsupported OS/architecture pair' ;;
esac
if [[ $TARGET_OS == linux ]]; then ffmpeg_flags+=(--enable-indev=v4l2 --enable-v4l2-m2m); fi
if [[ -n $CROSS_PREFIX ]]; then
    [[ -n ${HOST_TRIPLE:-} ]] || die 'cross builds require HOST_TRIPLE'
    [[ -n ${CMAKE_TOOLCHAIN_FILE:-} ]] || die 'cross builds require CMAKE_TOOLCHAIN_FILE'
    ffmpeg_flags+=(--enable-cross-compile --cross-prefix="$CROSS_PREFIX")
    x264_flags+=(--host="$HOST_TRIPLE" --cross-prefix="$CROSS_PREFIX")
fi
if [[ -n ${CMAKE_TOOLCHAIN_FILE:-} ]]; then cmake_flags+=(-DCMAKE_TOOLCHAIN_FILE="$CMAKE_TOOLCHAIN_FILE"); fi
if [[ $TARGET_OS == darwin ]]; then
    cmake_flags+=(-DCMAKE_OSX_DEPLOYMENT_TARGET="$MACOSX_DEPLOYMENT_TARGET")
fi
if [[ $TARGET_OS == linux && ${FULL_STATIC:-0} == 1 ]]; then link_flags="$link_flags -static"; fi
# NASM is optional: scalar x86 builds remain portable when it is unavailable.
if [[ $TARGET_ARCH == x86_64 ]] && ! command -v nasm >/dev/null; then
    ffmpeg_flags+=(--disable-x86asm)
    x264_flags+=(--disable-asm)
fi

printf 'Building OpenSSL %s (log: %s)\n' "$OPENSSL_VERSION" "$OUTPUT_DIR/logs/openssl.log"
(
    cd "$BUILD_DIR/openssl-$OPENSSL_VERSION"
    perl ./Configure "$OPENSSL_TARGET" "${openssl_flags[@]}"
    make -j "$JOBS"
    make install_sw
) >"$OUTPUT_DIR/logs/openssl.log" 2>&1
printf 'Building x264 %s (log: %s)\n' "$X264_COMMIT" "$OUTPUT_DIR/logs/x264.log"
(
    cd "$BUILD_DIR/x264-$X264_COMMIT"
    bash ./configure "${x264_flags[@]}"
    make -j "$JOBS"
    make install
) >"$OUTPUT_DIR/logs/x264.log" 2>&1
printf 'Building SRT %s (log: %s)\n' "$SRT_VERSION" "$OUTPUT_DIR/logs/srt.log"
(
    cmake -S "$BUILD_DIR/srt-$SRT_VERSION" -B "$BUILD_DIR/srt-cmake" "${cmake_flags[@]}"
    cmake --build "$BUILD_DIR/srt-cmake" --parallel "$JOBS"
    cmake --install "$BUILD_DIR/srt-cmake"
) >"$OUTPUT_DIR/logs/srt.log" 2>&1
if [[ $TARGET_OS == linux ]]; then
    # SRT's generated pc file normally adds a shared C++ runtime. Substitute
    # the target compiler's static runtime archive; libc remains an OS library.
    cxx_archive=$("$CXX" -print-file-name=libstdc++.a)
    unwind_archive=$("$CXX" -print-file-name=libgcc_eh.a)
    [[ -f $cxx_archive ]] || die 'target compiler needs a static libstdc++.a'
    [[ -f $unwind_archive ]] || die 'target compiler needs a static libgcc_eh.a'
    cp "$cxx_archive" "$PREFIX/lib/libstdc++.a"
    cp "$unwind_archive" "$PREFIX/lib/libgcc_eh.a"
    # FFmpeg classifies a bare archive path as a compiler flag, which moves it
    # before libsrt. GNU -l: syntax preserves static selection and link order.
    sed 's|-lstdc++|-l:libstdc++.a|g; s|-lgcc_s|-l:libgcc_eh.a|g' "$PREFIX/lib/pkgconfig/srt.pc" > "$PREFIX/lib/pkgconfig/srt.pc.tmp"
    mv "$PREFIX/lib/pkgconfig/srt.pc.tmp" "$PREFIX/lib/pkgconfig/srt.pc"
    link_flags="$link_flags -static-libgcc"
fi
ffmpeg_flags+=(--extra-cflags="-I$PREFIX/include" --extra-ldflags="$link_flags")
printf 'Building FFmpeg %s (log: %s)\n' "$FFMPEG_VERSION" "$OUTPUT_DIR/logs/ffmpeg.log"
(
    cd "$BUILD_DIR/ffmpeg-$FFMPEG_VERSION"
    bash ./configure "${ffmpeg_flags[@]}"
    make -j "$JOBS" ffmpeg
    make install-progs
) >"$OUTPUT_DIR/logs/ffmpeg.log" 2>&1

cp "$BUILD_DIR/ffmpeg-$FFMPEG_VERSION/COPYING.GPLv3" "$OUTPUT_DIR/licenses/FFmpeg-GPLv3.txt"
cp "$BUILD_DIR/x264-$X264_COMMIT/COPYING" "$OUTPUT_DIR/licenses/x264-GPLv2-or-later.txt"
cp "$BUILD_DIR/srt-$SRT_VERSION/LICENSE" "$OUTPUT_DIR/licenses/SRT-MPLv2.txt"
cp "$BUILD_DIR/openssl-$OPENSSL_VERSION/LICENSE.txt" "$OUTPUT_DIR/licenses/OpenSSL-Apache2.txt"
{
    printf 'Capturefab bundled FFmpeg media payload\nCombined distribution: GPL version 3 or later.\n'
    printf 'FFmpeg %s; OpenSSL %s; SRT %s; x264 %s.\n\n' "$FFMPEG_VERSION" "$OPENSSL_VERSION" "$SRT_VERSION" "$X264_COMMIT"
    for notice in "$OUTPUT_DIR/licenses/FFmpeg-GPLv3.txt" "$OUTPUT_DIR/licenses/x264-GPLv2-or-later.txt" "$OUTPUT_DIR/licenses/SRT-MPLv2.txt" "$OUTPUT_DIR/licenses/OpenSSL-Apache2.txt"; do
        printf '\n===== %s =====\n' "${notice##*/}"
        cat "$notice"
    done
    for notice in "$OUTPUT_DIR/licenses/NVIDIA-headers-MIT.txt" "$OUTPUT_DIR/licenses/AMD-AMF-MIT.txt"; do
        if [[ -f $notice ]]; then printf '\n===== %s =====\n' "${notice##*/}"; cat "$notice"; fi
    done
} > "$OUTPUT_DIR/licenses/NOTICE.txt"
cp "$BUILD_DIR/ffmpeg-$FFMPEG_VERSION/ffbuild/config.log" "$OUTPUT_DIR/logs/ffmpeg-config.log"
binary="$OUTPUT_DIR/bin/ffmpeg"
[[ $TARGET_OS != mingw32 ]] || binary="$binary.exe"
[[ -f $binary ]] || die "missing output: $binary"
{
    printf 'FFmpeg %s\nOpenSSL %s\nSRT %s\nx264 %s\nOS %s\nArchitecture %s\n' "$FFMPEG_VERSION" "$OPENSSL_VERSION" "$SRT_VERSION" "$X264_COMMIT" "$TARGET_OS" "$TARGET_ARCH"
    printf 'SHA256 %s\n' "$(digest "$binary")"
    printf 'NVENC/NVDEC headers enabled: %s (%s)\nAMF headers enabled: %s (%s)\nMedia Foundation enabled: %s\n' "$ENABLE_NVENC" "$NV_HEADERS_VERSION" "$ENABLE_AMF" "$AMF_VERSION" "$ENABLE_MF"
    "$CC" --version
    printf '\nFFmpeg configure arguments:\n'; printf '%q ' "${ffmpeg_flags[@]}"; printf '\n'
} > "$OUTPUT_DIR/build-manifest.txt"

run_checks=${RUN_CHECKS:-1}
[[ -z $CROSS_PREFIX ]] || run_checks=${RUN_CHECKS:-0}
if [[ $run_checks == 1 ]]; then
    "$binary" -hide_banner -protocols > "$OUTPUT_DIR/logs/protocols.txt" 2>&1
    [[ $(grep -c '^[[:space:]]*srt$' "$OUTPUT_DIR/logs/protocols.txt") -ge 2 ]] || die 'SRT input/output missing'
    "$binary" -hide_banner -encoders > "$OUTPUT_DIR/logs/encoders.txt" 2>&1
    grep -q 'libx264' "$OUTPUT_DIR/logs/encoders.txt" || die 'libx264 encoder missing'
    "$binary" -hide_banner -loglevel error -f lavfi -i testsrc2=size=64x64:rate=2 -frames:v 2 -c:v libx264 -f null -
    case $TARGET_OS in
        darwin)
            otool -L "$binary" > "$OUTPUT_DIR/logs/linked-libraries.txt"
            if grep -E '/opt/|/usr/local/|@rpath|@loader_path|@executable_path' "$OUTPUT_DIR/logs/linked-libraries.txt"; then die 'non-system shared library found'; fi ;;
        linux)
            if command -v readelf >/dev/null; then
                readelf -d "$binary" > "$OUTPUT_DIR/logs/linked-libraries.txt"
                if grep -E 'NEEDED.*(libsrt|libssl|libcrypto|libx264|libstdc\+\+|libgcc_s)' "$OUTPUT_DIR/logs/linked-libraries.txt"; then die 'third-party shared library found'; fi
            fi ;;
        mingw32)
            "${CROSS_PREFIX}objdump" -p "$binary" > "$OUTPUT_DIR/logs/linked-libraries.txt"
            if grep -Ei 'DLL Name:.*(libstdc|libgcc|libwinpthread|libsrt|libssl|libcrypto|libx264)' "$OUTPUT_DIR/logs/linked-libraries.txt"; then die 'non-system DLL found'; fi ;;
    esac
fi
printf 'FFmpeg ready: %s\nEmbed with: CAPTUREFAB_FFMPEG_BINARY=%q CAPTUREFAB_FFMPEG_LICENSE=%q cargo build --release\n' "$binary" "$binary" "$OUTPUT_DIR/licenses/NOTICE.txt"
