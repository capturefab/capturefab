use std::{env, fs, path::PathBuf, process::Command};

/// The OS model is accessed through a bundled helper, so running the desktop
/// app never needs Xcode or a Swift compiler. Older SDKs keep building the app
/// and report this provider as unavailable instead of raising its OS baseline.
fn build_foundation_models(out: &std::path::Path, target: &str) {
    println!("cargo:rustc-check-cfg=cfg(foundation_models_native)");
    println!("cargo:rerun-if-changed=src/assistant/foundation_models.swift");
    println!("cargo:rerun-if-env-changed=SDKROOT");
    if !target.contains("apple-darwin") || env::var_os("CARGO_FEATURE_AI").is_none() {
        return;
    }
    let sdk = env::var_os("SDKROOT").map(PathBuf::from).or_else(|| {
        let result = Command::new("xcrun")
            .args(["--sdk", "macosx", "--show-sdk-path"])
            .output()
            .ok()?;
        result
            .status
            .success()
            .then(|| PathBuf::from(String::from_utf8_lossy(&result.stdout).trim().to_owned()))
    });
    let Some(sdk) = sdk.filter(|sdk| {
        sdk.join("System/Library/Frameworks/FoundationModels.framework")
            .is_dir()
    }) else {
        println!(
            "cargo:warning=Apple Foundation Models requires a macOS 26 or newer SDK; this provider will be unavailable in this build"
        );
        return;
    };
    let arch = if target.starts_with("aarch64-") {
        "arm64"
    } else if target.starts_with("x86_64-") {
        "x86_64"
    } else {
        panic!("unsupported Foundation Models target: {target}");
    };
    let status = Command::new("xcrun")
        .args(["--sdk", "macosx", "swiftc", "-parse-as-library", "-O"])
        // Only the helper has this baseline; Rust checks the running OS before
        // launching it. The main application keeps its existing deployment target.
        .args(["-target", &format!("{arch}-apple-macosx26.0")])
        .arg("-sdk")
        .arg(sdk)
        .arg("-module-cache-path")
        .arg(out.join("foundation-models-module-cache"))
        .args([
            "-Xlinker",
            "-weak_framework",
            "-Xlinker",
            "FoundationModels",
        ])
        .arg("src/assistant/foundation_models.swift")
        .arg("-o")
        .arg(out.join("capturefab-foundation-models.bin"))
        .status()
        .expect("run xcrun swiftc to build the Foundation Models helper");
    assert!(
        status.success(),
        "cannot build the Foundation Models helper"
    );
    println!("cargo:rustc-cfg=foundation_models_native");
}

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
        // Cargo counts a missing path as changed, rebuilding every time. Release
        // builds must notice a client built after the previous build; others
        // watch the folder once it exists, and until they embed it, serve
        // finds web-client/dist in the working directory.
        let release = env::var("PROFILE").is_ok_and(|p| p == "release");
        if explicit.is_some() || release || dist.exists() {
            println!("cargo:rerun-if-changed={}", dist.display());
        }
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
        } else if release {
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
    build_foundation_models(&out, &target);
    build_task_filesystem();
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
    fs::write(out.join("capturefab-ffmpeg.bin"), &bytes).expect("write bundled FFmpeg");
    // Names the extracted copy, so processes running one build share a file.
    let digest: String = {
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

fn build_task_filesystem() {
    println!("cargo:rerun-if-changed=src/dataset/task_fs.cpp");
    println!("cargo:rerun-if-changed=src/dataset/task_fs.h");
    println!("cargo:rerun-if-changed=src/dataset/task_fs_internal.hpp");
    println!("cargo:rerun-if-changed=src/dataset/task_fs_probes.cpp");
    let include = env::var_os("DEP_DUCKDB_INCLUDE")
        .expect("the pinned bundled DuckDB must provide its C++ headers");
    let mut compiler = cc::Build::new();
    compiler
        .cpp(true)
        .std("c++17")
        .define("DUCKDB_STATIC_BUILD", None)
        .include(include)
        .file("src/dataset/task_fs.cpp")
        .file("src/dataset/task_fs_probes.cpp")
        .warnings(true)
        .flag_if_supported("-Wno-unused-parameter");
    // Match the pinned static engine's MSVC exception policy: Boundary must
    // catch C++ failures rather than allowing them to escape the C ABI.
    if compiler.get_compiler().is_like_msvc() {
        compiler.flag("/EHsc");
    }
    compiler.compile("capturefab_task_fs");
}
