# Releasing Capturefab

Releases are SemVer tags (`v0.2.0`, `v0.3.0-rc.1`) whose version matches `Cargo.toml`. Automation builds, smoke-tests, checksums and drafts a release. People decide what is published. No workflow publishes a release, deploys the site, or creates a public transparency-log entry unless a repository owner opts in.

## Workflows

| Workflow | Trigger | Does | Token permissions |
| --- | --- | --- | --- |
| `ci.yml` | push to `main`, pull requests, manual | `cargo fmt --check`, strict Clippy with all features, `cargo test` on Ubuntu, macOS and Windows, the headless `--no-default-features --features usb` build, `scripts/test-site.py`, `node --check web/app.js` and the script unit tests | `contents: read` |
| `release.yml` | `v*` tag push, manual | Builds every target below, assembles checksums, manifest, notes and corresponding source, then creates or updates a **draft** release for tag runs. A manual run from a branch is a dry run that only leaves workflow artifacts. A published release is never modified. | `contents: read`; the draft job adds `contents: write`, the opt-in attestation job `id-token: write` and `attestations: write` |
| `pages.yml` | manual only | Validates `web/` and deploys it to GitHub Pages, but only from the default branch and only when the repository variable `CAPTUREFAB_PUBLISH_SITE` is `true` | `pages: write`, `id-token: write` in the deploy job |

All actions are pinned to full commit SHAs. Checkouts do not persist credentials.

## Targets

| Target | Runner | FFmpeg toolchain | Notes |
| --- | --- | --- | --- |
| Linux x86_64 | `ubuntu-24.04`, `ubuntu:20.04` container | GCC 9 | built against glibc 2.31 (Ubuntu 20.04, Debian 11, Jetson Linux R35 and newer) |
| Linux ARM64 | `ubuntu-24.04-arm`, `ubuntu:20.04` container | GCC 9 | built against glibc 2.31 |
| macOS ARM64 | `macos-15` | Apple clang | deployment target 11.0 |
| macOS x86_64 | `macos-15-intel` | Apple clang, NASM | deployment target 10.15 |
| Windows x86_64 | `windows-2025` | MSYS2 UCRT64 GCC | Rust uses the MSVC target; FFmpeg is a separate static MinGW executable |

Each target builds a desktop (`--features wgpu`) and a headless (`--no-default-features --features usb`) variant. `scripts/release.py build` embeds the target's FFmpeg, checks that the executable contains exactly that FFmpeg, runs `--version`, `ffmpeg` (SRT input and output must be present) and a two-frame simulator capture, then packages. Desktop Linux builds need no X11, Wayland or OpenGL development packages: the GUI loads those libraries from the system at run time. Private repositories only get hosted ARM64 runners on plans that offer them; a target without a successful build is listed as unavailable, with the packaging error when there is one.

Not built by the workflow, so the manifest lists them as unavailable:

- **Linux ARMv7.** GitHub has no ARMv7 runner. A cross build would be unexecuted; build natively on the device, or cross-compile and package with `--skip-smoke`, which labels the result `cross-compiled`.
- **Windows ARM64.** `windows-11-arm` exists, but the FFmpeg recipe cannot build there yet: under MSYS2 CLANGARM64, x264's configure detects the emulated x86_64 shell as the host unless `--host` is passed, and OpenSSL needs `OPENSSL_TARGET=mingwarm64`.

## Cutting a release

1. Set `version` in `Cargo.toml`, run the CI checks locally, commit, and push to `main`.
2. Tag and push: `git tag v0.2.0 && git push origin v0.2.0`. To rebuild an existing tag, run the Release workflow manually and choose the tag.
3. Review the draft. Its notes list each artifact's validation (`native-smoke-passed` or `cross-compiled`) and platform signature, and every unavailable target with the reason. Publish it manually.
4. Update the static site from the published manifest and commit `web/`:

   ```sh
   gh release download v0.2.0 --pattern releases.json --dir /tmp/capturefab-v0.2.0
   python3 scripts/site-release.py --merge /tmp/capturefab-v0.2.0/releases.json
   python3 scripts/test-site.py
   ```

5. Deploying the site is a separate decision: select GitHub Actions as the Pages source, set `CAPTUREFAB_PUBLISH_SITE=true`, then run the Pages workflow on `main`. GitHub Pages sites are public, including sites built from a private repository on most plans.

A release contains, per target, `capturefab-VERSION-OS-ARCH-{desktop,headless}` archives (`.tar.gz` on Linux, `.zip` elsewhere; macOS desktop adds `Capturefab.app`) and `capturefab-VERSION-OS-ARCH-media-sources.tar.gz`, plus `capturefab-VERSION-source.tar.gz` (the exact committed tree, all `Cargo.lock` dependency sources in `vendor/`, and an offline Cargo configuration), `releases.json`, and `SHA256SUMS` covering every file. Assembly refuses uncommitted changes and missing or modified corresponding-source archives. The FFmpeg source archives must match the build recipe's pinned checksums, including enabled GPU headers. Each package holds `LICENSE`, `THIRD-PARTY-LICENSES.txt` (license texts of the Rust crates compiled into that executable), `media-licenses/` and the FFmpeg build manifest.

## Optional signing and provenance

Nothing is signed unless these repository secrets exist. Identities come only from the supplied credentials. The platform signing steps have not yet run with real credentials, so inspect the first signed draft before publishing it.

| Secret | Effect |
| --- | --- |
| `MACOS_CERTIFICATE_P12_BASE64`, `MACOS_CERTIFICATE_PASSWORD` | Imports a Developer ID Application certificate into a temporary keychain and signs the executable and `Capturefab.app` with the hardened runtime and the camera entitlement |
| `MACOS_NOTARY_KEY`, `MACOS_NOTARY_KEY_ID`, `MACOS_NOTARY_ISSUER` | App Store Connect API key (`.p8` contents) for `notarytool`; the desktop app is stapled |
| `WINDOWS_CERTIFICATE_PFX_BASE64`, `WINDOWS_CERTIFICATE_PASSWORD` | Authenticode signing with `signtool` and a DigiCert RFC 3161 timestamp. Requires a certificate with an exportable key; HSM-only certificates need a different signing step |
| `RELEASE_GPG_PRIVATE_KEY`, `RELEASE_GPG_PASSPHRASE` | Detached OpenPGP signature `SHA256SUMS.asc`. Publish the public key separately |

The workflow compiles and packages before importing any signing material; the signing pass uses `build --repackage` to verify and reuse each variant's existing archive without invoking a compiler. Rerunning a draft removes generated platform archives that are no longer available and an obsolete checksum signature, then uploads the newly verified files. Unrelated draft attachments are preserved. Platform signatures appear per artifact as `unsigned`, `apple-developer-id`, `apple-notarized` or `authenticode`.

Setting the repository variable `CAPTUREFAB_ATTEST=true` adds GitHub artifact attestations for every file in `SHA256SUMS`, verifiable with `gh attestation verify FILE --repo OWNER/REPO`. Private repositories need a GitHub plan that supports attestations. In a public repository the attestation, including the repository identity, is recorded in the public Sigstore transparency log, so leave it unset until that is wanted.

## Local packaging

```sh
JOBS=8 BUILD_DIR=/tmp/ffmpeg-build scripts/build-ffmpeg.sh "$PWD/target/ffmpeg-bundle"
python3 scripts/release.py build --target aarch64-apple-darwin --os macos --arch aarch64 \
  --ffmpeg-bundle target/ffmpeg-bundle --output dist/macos-aarch64
python3 scripts/release.py assemble --input dist --output dist/release --repository capturefab/capturefab
```

`build` needs Python 3.8 or newer (the Linux release containers ship 3.8). `--binary PATH --variants headless` packages an existing executable. `assemble` needs Cargo to vendor the locked dependencies; without `--download-base` it writes file names as URLs. To build from a source archive without a registry connection, use `cargo --config .cargo/vendor-config.toml build --offline --locked` from its root.

## Screenshots and site pages

- `python3 scripts/cli-svg.py --binary target/debug/capturefab` runs real commands against the simulator in an isolated session directory and writes `web/images/cli.svg`. It refuses to render if discovery finds a non-simulated camera.
- `capturefab gui --screenshot web/images/gui.png --screenshot-cameras 3` saves a real renderer screenshot of simulated cameras.
- `python3 scripts/site-release.py` regenerates `web/downloads.html` and `web/feed.xml` from `web/releases.json`; `--check` fails when they are stale.
- `python3 scripts/test-site.py` validates both pages, local links and anchors, image dimensions, SVG content, the manifest, the Atom feed and generated-page freshness.
