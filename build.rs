use std::{env, fs, path::PathBuf, process::Command};

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
    let release = env::var("PROFILE").is_ok_and(|p| p == "release");
    // Release builds must notice a bundle created after the previous build.
    // Development builds resolve this path at runtime and need no rebuild.
    if release && env::var_os("CAPTUREFAB_FFMPEG_BINARY").is_none() {
        println!("cargo:rerun-if-changed={}", default.display());
    }
    let source = env::var_os("CAPTUREFAB_FFMPEG_BINARY")
        .map(PathBuf::from)
        .or_else(|| (release && default.is_file()).then(|| default.clone()));
    let bytes = if let Some(path) = source {
        println!("cargo:rerun-if-changed={}", path.display());
        fs::read(&path)
            .unwrap_or_else(|e| panic!("cannot read bundled FFmpeg {}: {e}", path.display()))
    } else {
        println!(
            "cargo:rustc-env=CAPTUREFAB_LOCAL_FFMPEG={}",
            default.display()
        );
        Vec::new()
    };
    fs::write(out.join("capturefab-ffmpeg.bin"), &bytes).expect("write bundled FFmpeg");
    // Names the extracted copy, so processes running one build share a file.
    let digest = if bytes.is_empty() {
        String::new()
    } else {
        use sha2::Digest;
        sha2::Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    println!("cargo:rustc-env=CAPTUREFAB_FFMPEG_SHA256={digest}");
    let license = if let Some(path) = env::var_os("CAPTUREFAB_FFMPEG_LICENSE") {
        let path = PathBuf::from(path);
        println!("cargo:rerun-if-changed={}", path.display());
        fs::read_to_string(&path).expect("read FFmpeg license notice")
    } else {
        "Capturefab is MIT-licensed. Bundled FFmpeg runs as a separate executable and is GPLv3 with static x264 (GPLv2-or-later), SRT (MPLv2) and OpenSSL (Apache-2.0). Source archives, licenses and build configuration accompany releases. See capturefab ffmpeg and https://ffmpeg.org/.".to_string()
    };
    fs::write(out.join("capturefab-ffmpeg-license.txt"), license)
        .expect("write bundled FFmpeg license");
}
