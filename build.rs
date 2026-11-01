use std::{env, fs, io::Write, path::PathBuf, process::Command};

fn build_macos_scanner(out: &std::path::Path, target: &str) {
    println!("cargo:rerun-if-changed=src/scanner/macos.m");
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    println!("cargo:rerun-if-env-changed=SDKROOT");
    if !target.contains("apple-darwin") || env::var_os("CARGO_FEATURE_SCANNER").is_none() {
        return;
    }
    let arch = if target.starts_with("aarch64-") {
        "arm64"
    } else if target.starts_with("x86_64-") {
        "x86_64"
    } else {
        panic!("unsupported macOS scanner target: {target}");
    };
    // Keep the isolated helper compatible with the same baseline as releases.
    let minimum = env::var("MACOSX_DEPLOYMENT_TARGET")
        .unwrap_or_else(|_| if arch == "arm64" { "11.0" } else { "10.15" }.to_string());
    let mut compiler = Command::new("xcrun");
    compiler.args([
        "--sdk",
        "macosx",
        "clang",
        "-arch",
        arch,
        "-fobjc-arc",
        "-O2",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-framework",
        "Foundation",
        "-framework",
        "ImageCaptureCore",
        "-framework",
        "ImageIO",
        "-framework",
        "CoreGraphics",
    ]);
    compiler.arg(format!("-mmacosx-version-min={minimum}"));
    // The scanner helper may browse network devices; give its own process the
    // same privacy usage descriptions as the parent executable.
    let plist = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"))
        .join("assets/Info.plist");
    compiler.arg(format!(
        "-Wl,-sectcreate,__TEXT,__info_plist,{}",
        plist.display()
    ));
    if let Some(sdk) = env::var_os("SDKROOT") {
        compiler.arg("-isysroot").arg(sdk);
    }
    let status = compiler
        .arg("src/scanner/macos.m")
        .arg("-o")
        .arg(out.join("capturefab-scanner-ica.bin"))
        .status()
        .expect("run xcrun clang to build the macOS scanner helper");
    assert!(status.success(), "cannot build macOS scanner helper");
}

fn main() {
    println!("cargo:rerun-if-env-changed=CAPTUREFAB_FFMPEG_BINARY");
    println!("cargo:rerun-if-env-changed=CAPTUREFAB_FFMPEG_LICENSE");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    let target = env::var("TARGET").expect("Cargo TARGET");
    if target.ends_with("windows-msvc") {
        // Pre-exported, architecture-independent icon resources. LINK accepts
        // .res directly, so building needs no additional resource compiler.
        let resource =
            PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"))
                .join("assets/icons/capturefab.res");
        println!("cargo:rerun-if-changed={}", resource.display());
        println!("cargo:rustc-link-arg-bin=capturefab={}", resource.display());
    }
    build_macos_scanner(&out, &target);
    if target.contains("apple-darwin") {
        let plist =
            PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"))
                .join("assets/Info.plist");
        println!("cargo:rerun-if-changed={}", plist.display());
        println!(
            "cargo:rustc-link-arg-bin=capturefab=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
    let default =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"))
            .join("target/ffmpeg")
            .join(&target)
            .join("bin")
            .join(if target.contains("windows") {
                "ffmpeg.exe"
            } else {
                "ffmpeg"
            });
    if env::var_os("CAPTUREFAB_FFMPEG_BINARY").is_none() {
        println!("cargo:rerun-if-changed={}", default.display());
    }
    let path = env::var_os("CAPTUREFAB_FFMPEG_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| build_ffmpeg(default));
    println!("cargo:rerun-if-changed={}", path.display());
    let bytes = fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read bundled FFmpeg {}: {e}; run scripts/build-ffmpeg.sh and copy its bin/ffmpeg there, or set CAPTUREFAB_FFMPEG_BINARY",
            path.display()
        )
    });
    // A versioned envelope lets release packaging verify the original executable,
    // independently of the compressor's implementation. Its digest also names the
    // extracted copy, so existing builds continue sharing the same cache file.
    use sha2::Digest;
    let digest = sha2::Sha256::digest(&bytes);
    let mut packed = Vec::new();
    packed.extend_from_slice(b"CAPTUREFAB-FFMPEG-ZLIB\0");
    packed.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    packed.extend_from_slice(&digest);
    let mut encoder = flate2::write::ZlibEncoder::new(packed, flate2::Compression::best());
    encoder.write_all(&bytes).expect("compress bundled FFmpeg");
    let packed = encoder.finish().expect("finish bundled FFmpeg compression");
    fs::write(out.join("capturefab-ffmpeg.bin"), packed).expect("write bundled FFmpeg");
    let license = if let Some(path) = env::var_os("CAPTUREFAB_FFMPEG_LICENSE") {
        let path = PathBuf::from(path);
        println!("cargo:rerun-if-changed={}", path.display());
        fs::read_to_string(&path).expect("read FFmpeg license notice")
    } else {
        "Capturefab is MIT-licensed. Bundled FFmpeg runs as a separate executable and is GPLv3 with static x264 (GPLv2-or-later), SRT (MPLv2), OpenSSL (Apache-2.0) and zlib (zlib License). Source archives, licenses and build configuration accompany releases. See capturefab ffmpeg and https://ffmpeg.org/.".to_string()
    };
    fs::write(out.join("capturefab-ffmpeg-license.txt"), license)
        .expect("write bundled FFmpeg license");
}

fn build_ffmpeg(default: PathBuf) -> PathBuf {
    if default.is_file() {
        return default;
    }
    println!("cargo:rerun-if-changed=scripts/build-ffmpeg.sh");
    let bundle = default
        .ancestors()
        .nth(4)
        .expect("FFmpeg path under target")
        .join("ffmpeg-bundle");
    let status = std::process::Command::new("bash")
        .arg("scripts/build-ffmpeg.sh")
        .arg(&bundle)
        .status()
        .expect("run scripts/build-ffmpeg.sh");
    assert!(status.success(), "scripts/build-ffmpeg.sh failed");
    fs::create_dir_all(default.parent().expect("FFmpeg bin directory"))
        .expect("create FFmpeg bin directory");
    fs::copy(
        bundle
            .join("bin")
            .join(default.file_name().expect("FFmpeg file name")),
        &default,
    )
    .expect("copy built FFmpeg");
    default
}
