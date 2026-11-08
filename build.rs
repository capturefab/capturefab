#![deny(clippy::doc_markdown, clippy::cast_possible_truncation)]
use std::{
    env, fs,
    io::Write as _,
    path::{Path, PathBuf},
};

fn main() {
    println!("cargo:rerun-if-env-changed=CAPTUREFAB_FFMPEG_BINARY");
    println!("cargo:rerun-if-env-changed=CAPTUREFAB_FFMPEG_LICENSE");
    // `jpeg_helper`: the GPU JPEG encoders' helper process, and the shared
    // memory frames reach it in (the nvjpeg or vaapi feature, on Unix).
    println!("cargo:rustc-check-cfg=cfg(jpeg_helper)");
    if env::var_os("CARGO_CFG_UNIX").is_some()
        && ["CARGO_FEATURE_NVJPEG", "CARGO_FEATURE_VAAPI"]
            .iter()
            .any(|feature| env::var_os(feature).is_some())
    {
        println!("cargo:rustc-cfg=jpeg_helper");
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    let target = env::var("TARGET").expect("Cargo TARGET");
    let manifest =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"));
    if target.ends_with("windows-msvc") {
        // Pre-exported, architecture-independent icon resources. LINK accepts
        // .res directly, so building needs no additional resource compiler.
        let resource = manifest.join("assets/icons/capturefab.res");
        println!("cargo:rerun-if-changed={}", resource.display());
        println!("cargo:rustc-link-arg-bin=capturefab={}", resource.display());
    }
    if target.contains("apple-darwin") {
        let plist = manifest.join("assets/Info.plist");
        println!("cargo:rerun-if-changed={}", plist.display());
        println!(
            "cargo:rustc-link-arg-bin=capturefab=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
    // FFmpeg is never built here: that takes minutes and the network, and
    // would run for any target checked. CAPTUREFAB_FFMPEG_BINARY (which
    // scripts/release.py sets) is embedded in any build, and a release build
    // embeds what scripts/build-ffmpeg.sh left in target/ffmpeg/<target>/bin.
    // Other builds embed nothing, which keeps them small and quick to link,
    // and find FFmpeg when they run.
    let bundled = match env::var_os("CAPTUREFAB_FFMPEG_BINARY").filter(|path| !path.is_empty()) {
        Some(path) => Some(PathBuf::from(path)),
        None if env::var("PROFILE").as_deref() == Ok("release") => {
            let default = manifest
                .join("target/ffmpeg")
                .join(&target)
                .join("bin")
                .join(if target.contains("windows") {
                    "ffmpeg.exe"
                } else {
                    "ffmpeg"
                });
            println!("cargo:rerun-if-changed={}", default.display());
            default.is_file().then_some(default)
        }
        None => None,
    };
    let packed = bundled.as_ref().map_or_else(Vec::new, |path| {
        println!("cargo:rerun-if-changed={}", path.display());
        let bytes = fs::read(path).unwrap_or_else(|e| {
            panic!(
                "cannot read FFmpeg {} to embed: {e}; build it with scripts/build-ffmpeg.sh, or unset CAPTUREFAB_FFMPEG_BINARY",
                path.display()
            )
        });
        pack(&bytes)
    });
    write_if_changed(&out.join("capturefab-ffmpeg.bin"), &packed);
    let license = env::var_os("CAPTUREFAB_FFMPEG_LICENSE").map_or_else(
        || {
            if bundled.is_some() {
                "Capturefab is MIT-licensed. Bundled FFmpeg runs as a separate executable and is GPLv3 with static x264 (GPLv2-or-later), SRT (MPLv2), OpenSSL (Apache-2.0) and zlib (zlib License). Source archives, licenses and build configuration accompany releases. See `capturefab licenses` and https://ffmpeg.org/.".to_owned()
            } else {
                "Capturefab is MIT-licensed. This build has no FFmpeg built in: it runs the FFmpeg that CAPTUREFAB_FFMPEG names, or one next to capturefab or on PATH, as a separate executable under that FFmpeg's own license. See https://ffmpeg.org/.".to_owned()
            }
        },
        |path| {
            let path = PathBuf::from(path);
            println!("cargo:rerun-if-changed={}", path.display());
            fs::read_to_string(&path).expect("read FFmpeg license notice")
        },
    );
    write_if_changed(
        &out.join("capturefab-ffmpeg-license.txt"),
        license.as_bytes(),
    );
}

/// `bytes` in a versioned envelope that lets release packaging verify the
/// original executable, independently of the compressor's implementation.
/// Its digest also names the extracted copy, so builds of the same `FFmpeg`
/// share one cache file.
fn pack(bytes: &[u8]) -> Vec<u8> {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(bytes);
    let mut packed = Vec::new();
    packed.extend_from_slice(b"CAPTUREFAB-FFMPEG-ZLIB\0");
    packed.extend_from_slice(
        &u64::try_from(bytes.len())
            .expect("embedded FFmpeg length fits its envelope")
            .to_le_bytes(),
    );
    packed.extend_from_slice(&digest);
    let mut encoder = flate2::write::ZlibEncoder::new(packed, flate2::Compression::best());
    encoder.write_all(bytes).expect("compress bundled FFmpeg");
    encoder.finish().expect("finish bundled FFmpeg compression")
}

/// Leave an unchanged output alone, so that rerunning this script does not
/// make Cargo compile the crate again.
fn write_if_changed(path: &Path, bytes: &[u8]) {
    if fs::read(path).is_ok_and(|old| old == bytes) {
        return;
    }
    fs::write(path, bytes).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}
