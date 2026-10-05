use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=CAPTUREFAB_FFMPEG_BINARY");
    println!("cargo:rerun-if-env-changed=CAPTUREFAB_FFMPEG_LICENSE");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    let target = env::var("TARGET").expect("Cargo TARGET");
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
    let license = if let Some(path) = env::var_os("CAPTUREFAB_FFMPEG_LICENSE") {
        let path = PathBuf::from(path);
        println!("cargo:rerun-if-changed={}", path.display());
        fs::read_to_string(&path).expect("read FFmpeg license notice")
    } else {
        "Capturefab is GPL-3.0-only. Bundled FFmpeg is GPLv3 with static x264 (GPLv2-or-later), SRT (MPLv2) and OpenSSL (Apache-2.0). Source archives, licenses and build configuration accompany releases. See capturefab ffmpeg and https://ffmpeg.org/.".to_string()
    };
    fs::write(out.join("capturefab-ffmpeg-license.txt"), license)
        .expect("write bundled FFmpeg license");
}
