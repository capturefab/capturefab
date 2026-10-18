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

/// Release builds embed the DINOv2 weights from scripts/fetch-dinov2.py (or
/// `CAPTUREFAB_DINOV2_WEIGHTS`); development builds read that file in place,
/// keeping their binaries small, as with FFmpeg.
fn bundle_dinov2() {
    println!("cargo:rustc-check-cfg=cfg(dinov2_embedded)");
    println!("cargo:rerun-if-env-changed=CAPTUREFAB_DINOV2_WEIGHTS");
    let default =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"))
            .join("target/models/dinov2-small/model.safetensors");
    let release = env::var("PROFILE").is_ok_and(|p| p == "release");
    if release && env::var_os("CAPTUREFAB_DINOV2_WEIGHTS").is_none() {
        println!("cargo:rerun-if-changed={}", default.display());
    }
    let source = env::var_os("CAPTUREFAB_DINOV2_WEIGHTS")
        .map(PathBuf::from)
        .or_else(|| (release && default.is_file()).then(|| default.clone()));
    if let Some(path) = source {
        assert!(path.is_file(), "missing DINOv2 weights {}", path.display());
        println!("cargo:rerun-if-changed={}", path.display());
        println!("cargo:rustc-env=CAPTUREFAB_DINOV2_EMBED={}", path.display());
        println!("cargo:rustc-cfg=dinov2_embedded");
    } else {
        if release {
            println!(
                "cargo:warning=DINOv2 weights are not bundled; run scripts/fetch-dinov2.py before building releases"
            );
        }
        println!(
            "cargo:rustc-env=CAPTUREFAB_DINOV2_PATH={}",
            default.display()
        );
    }
}

/// The `embedded-web` feature builds the browser client into the executable,
/// so `serve --listen` needs no --web-dir: every file of `CAPTUREFAB_WEB_DIST`
/// (release builds set it), or else of web-client/dist when it has been
/// built. Without the feature, or a built client, the table is empty.
fn embed_web(out: &std::path::Path) {
    println!("cargo:rerun-if-env-changed=CAPTUREFAB_WEB_DIST");
    let mut files = Vec::new();
    if env::var_os("CARGO_FEATURE_EMBEDDED_WEB").is_some() {
        let explicit = env::var_os("CAPTUREFAB_WEB_DIST").map(PathBuf::from);
        let dist = explicit.clone().unwrap_or_else(|| {
            PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"))
                .join("web-client/dist")
        });
        println!("cargo:rerun-if-changed={}", dist.display());
        let built = ["index.html", "capturefab_web_bg.wasm"]
            .iter()
            .all(|name| dist.join(name).is_file());
        if built {
            // include_bytes! needs absolute paths.
            let dist = dist
                .canonicalize()
                .expect("resolve the browser client folder");
            web_files(&dist, &dist, &mut files);
            files.sort();
        } else if explicit.is_some() {
            panic!(
                "CAPTUREFAB_WEB_DIST {} has no built browser client; run web-client/build.sh",
                dist.display()
            );
        } else if env::var("PROFILE").is_ok_and(|p| p == "release") {
            println!(
                "cargo:warning=embedded-web found no browser client in {}; run web-client/build.sh",
                dist.display()
            );
        }
    }
    let entries: String = files
        .iter()
        .map(|(name, path)| {
            println!("cargo:rerun-if-changed={}", path.display());
            format!(
                "({name:?}, include_bytes!({:?})),",
                path.display().to_string()
            )
        })
        .collect();
    fs::write(out.join("web-assets.rs"), format!("&[{entries}]"))
        .expect("write embedded web assets");
}

/// Files under `dir`, named by their path inside `root` with `/` separators.
fn web_files(root: &std::path::Path, dir: &std::path::Path, files: &mut Vec<(String, PathBuf)>) {
    for entry in fs::read_dir(dir).expect("read the browser client folder") {
        let path = entry.expect("read the browser client folder").path();
        if path.is_dir() {
            web_files(root, &path, files);
        } else if path.is_file() {
            let name = path
                .strip_prefix(root)
                .expect("inside the browser client folder")
                .components()
                .map(|part| part.as_os_str().to_str().expect("UTF-8 web asset name"))
                .collect::<Vec<_>>()
                .join("/");
            files.push((name, path));
        }
    }
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
    embed_web(&out);
    if env::var_os("CARGO_FEATURE_DINOV2").is_some() {
        bundle_dinov2();
    }
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
