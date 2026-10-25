#!/usr/bin/env python3
"""Build web/try/capturefab.wasm, the WebAssembly half of Capturefab in the browser.

`python3 scripts/build-try.py` builds the `web-try/` crate for
wasm32-unknown-unknown (release, `--locked`, SIMD128), copies the module to
`web/try/capturefab.wasm` and records its provenance in `web/try/manifest.json`.
The site serves the committed module, so its deploy needs no Rust; CI rebuilds
it with `--out DIR` (which touches nothing in web/try) and `scripts/test-site.py`
calls `check_manifest` to catch a module that no longer matches its sources.

Cargo and rustc come from the CARGO and RUSTC environment variables when set,
otherwise from rustup (`rustup which`) when it is installed, since a system
toolchain earlier on PATH often lacks the wasm32 standard library.
"""
import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CRATE = Path("web-try")
WASM = Path("web/try/capturefab.wasm")
MANIFEST = Path("web/try/manifest.json")
TARGET = "wasm32-unknown-unknown"
RUSTFLAGS = "-C target-feature=+simd128"
# The home page downloads the module on demand; keep it small.
BUDGET = 512 * 1024
MAGIC = b"\0asm\x01\0\0\0"
REBUILD = "run python3 scripts/build-try.py"


def source_files(repo_root: Path) -> list[Path]:
    """The inputs of the module, relative to the repository: the manifest,
    lockfile and everything under src/."""
    crate = repo_root / CRATE
    files = [crate / "Cargo.toml", crate / "Cargo.lock"]
    files += [path for path in (crate / "src").rglob("*") if path.is_file()]
    return sorted(path.relative_to(repo_root) for path in files)


def source_sha256(repo_root: Path) -> str:
    """SHA-256 over each source file in sorted order: its relative POSIX path,
    a NUL, its byte length in decimal, a NUL, then its bytes. CRLF line endings
    count as LF, so a Windows checkout hashes the same."""
    digest = hashlib.sha256()
    for relative in source_files(repo_root):
        data = (repo_root / relative).read_bytes().replace(b"\r\n", b"\n")
        digest.update(relative.as_posix().encode() + b"\0" + str(len(data)).encode() + b"\0")
        digest.update(data)
    return digest.hexdigest()


def abi_version(repo_root: Path) -> int:
    """`pub const ABI: u32 = N;` from web-try/src/abi.rs."""
    text = (repo_root / CRATE / "src/abi.rs").read_text(encoding="utf-8")
    match = re.search(r"^pub const ABI: u32 = (\d+);", text, re.M)
    if not match:
        raise SystemExit("web-try/src/abi.rs has no `pub const ABI: u32 = N;`")
    return int(match[1])


def check_manifest(repo_root: Path) -> list[str]:
    """Problems with the committed module and its manifest, or none."""
    wasm_path, manifest_path = repo_root / WASM, repo_root / MANIFEST
    missing = [str(path) for path in (WASM, MANIFEST) if not (repo_root / path).is_file()]
    if missing:
        return [f"{name} is missing; {REBUILD}" for name in missing]
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        return [f"{MANIFEST} is not valid JSON ({error}); {REBUILD}"]
    if not isinstance(manifest, dict):
        return [f"{MANIFEST} must hold a JSON object; {REBUILD}"]
    errors = []
    expected_keys = {"file", "sha256", "size_bytes", "source_sha256", "rustc", "abi"}
    if set(manifest) != expected_keys:
        errors.append(f"{MANIFEST} must have exactly the keys {', '.join(sorted(expected_keys))}; {REBUILD}")
    if manifest.get("file") != WASM.name:
        errors.append(f"{MANIFEST} must name {WASM.name}")
    data = wasm_path.read_bytes()
    if not data.startswith(MAGIC):
        errors.append(f"{WASM} is not a WebAssembly 1.0 module; {REBUILD}")
    if manifest.get("size_bytes") != len(data) or manifest.get("sha256") != hashlib.sha256(data).hexdigest():
        errors.append(f"{WASM} does not match the size and SHA-256 in {MANIFEST}; {REBUILD}")
    if len(data) > BUDGET:
        errors.append(f"{WASM} is {len(data):,} bytes, over the {BUDGET:,}-byte budget")
    if not str(manifest.get("rustc", "")).startswith("rustc "):
        errors.append(f"{MANIFEST} must record the rustc version that built the module; {REBUILD}")
    if manifest.get("abi") != abi_version(repo_root):
        errors.append(f"{MANIFEST} records ABI {manifest.get('abi')}, but web-try/src/abi.rs is ABI {abi_version(repo_root)}; {REBUILD}")
    if manifest.get("source_sha256") != source_sha256(repo_root):
        errors.append(f"{WASM} is stale: the web-try sources changed since it was built; {REBUILD}")
    return errors


def toolchain(name: str) -> str:
    """CARGO or RUSTC from the environment, else rustup's, else PATH's."""
    variable = name.upper()
    if os.environ.get(variable):
        return os.environ[variable]
    rustup = shutil.which("rustup")
    if rustup:
        found = subprocess.run([rustup, "which", name], cwd=ROOT, capture_output=True, text=True)
        if found.returncode == 0 and found.stdout.strip():
            return found.stdout.strip()
    path = shutil.which(name)
    if not path:
        raise SystemExit(f"{name} not found; install Rust with rustup and run: rustup target add {TARGET}")
    return path


def build() -> tuple[bytes, str]:
    """Build the module; its bytes and the `rustc -V` that built it."""
    cargo, rustc = toolchain("cargo"), toolchain("rustc")
    env = dict(os.environ, RUSTC=rustc, RUSTFLAGS=RUSTFLAGS)
    # These take precedence over RUSTFLAGS; the module's flags are fixed here.
    for variable in ("CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS", f"CARGO_TARGET_{TARGET.upper().replace('-', '_')}_RUSTFLAGS"):
        env.pop(variable, None)
    version = subprocess.run([rustc, "-V"], capture_output=True, text=True, env=env)
    if version.returncode != 0:
        raise SystemExit(f"{rustc} -V failed: {version.stderr.strip()}")
    command = [cargo, "build", "--locked", "--release", "--target", TARGET, "--manifest-path", str(ROOT / CRATE / "Cargo.toml")]
    print("+", " ".join(command), flush=True)
    if subprocess.run(command, cwd=ROOT, env=env).returncode != 0:
        raise SystemExit(f"the wasm build failed; is the {TARGET} target installed? (rustup target add {TARGET})")
    target_dir = Path(env.get("CARGO_TARGET_DIR") or ROOT / CRATE / "target")
    if not target_dir.is_absolute():
        target_dir = ROOT / target_dir
    data = (target_dir / TARGET / "release" / "capturefab_try.wasm").read_bytes()
    if not data.startswith(MAGIC):
        raise SystemExit("the build did not produce a WebAssembly module")
    return data, version.stdout.strip()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", type=Path, help="write DIR/capturefab.wasm only, leaving web/try untouched")
    parser.add_argument("--check", action="store_true", help="only check the committed module against its sources")
    args = parser.parse_args()
    if args.check:
        errors = check_manifest(ROOT)
        for error in errors:
            print(error, file=sys.stderr)
        return 1 if errors else 0
    data, rustc = build()
    size = f"{len(data):,} bytes ({len(data) / 1024:.0f} KiB of the {BUDGET // 1024} KiB budget)"
    if args.out:
        args.out.mkdir(parents=True, exist_ok=True)
        (args.out / WASM.name).write_bytes(data)
        print(f"wrote {args.out / WASM.name}: {size}")
    else:
        (ROOT / WASM).write_bytes(data)
        manifest = {
            "file": WASM.name,
            "sha256": hashlib.sha256(data).hexdigest(),
            "size_bytes": len(data),
            "source_sha256": source_sha256(ROOT),
            "rustc": rustc,
            "abi": abi_version(ROOT),
        }
        (ROOT / MANIFEST).write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
        print(f"wrote {WASM} and {MANIFEST}: {size}")
    if len(data) > BUDGET:
        print(f"the module is over the {BUDGET:,}-byte budget", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
