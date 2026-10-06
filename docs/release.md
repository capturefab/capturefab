# Releasing Capturefab

Releases are SemVer tags (`v0.2.0`, `v0.3.0-rc.1`) whose version matches `Cargo.toml`. A release starts from the GitHub UI and runs end to end in GitHub Actions: a pull request raises the version, merging it tags the commit, every target is built and smoke-tested, the artifacts are attested, a draft release is created with generated notes, and after a person approves the `release` environment the release is published and the downloads page is updated. People still decide what is published: publishing waits for that approval, and stays a draft when the environment has no required reviewers.

## Workflows

| Workflow | Trigger | Does | Token permissions |
| --- | --- | --- | --- |
| `ci.yml` | push to `main`, pull requests, manual | `cargo fmt --check`, strict Clippy with all features, `cargo test` on Linux x64 and ARM64, macOS Apple silicon and Intel, Windows x64 and ARM64 (with a per-runner summary of the hardware accelerators `doctor` finds), the headless build, `scripts/test-site.py`, `node --check web/app.js` and the script unit tests | `contents: read` |
| `prepare-release.yml` | manual, with a version | Raises the version in `Cargo.toml` and `Cargo.lock` (`release.py set-version`, which refuses a version that is not newer or is already tagged), pushes `release/vVERSION`, opens a pull request labeled `release` and starts CI on it | `contents`, `pull-requests`, `issues` and `actions: write` |
| `release.yml` | a push to `main` that raises the version in `Cargo.toml`; a `v*` tag push; manual | Tags the commit when the version is new, builds every target below, assembles checksums, manifest, SBOM, notes and corresponding source, attests them, creates or updates a **draft** with GitHub-generated notes, then publishes it once the `release` environment is approved and commits the new release to `web/`. A manual run from a branch is a dry run that only leaves workflow artifacts; a push to `main` that does not raise the version does nothing. A published release is never modified. | `contents: read`; `contents: write` for the tag, draft, publish and site jobs; `id-token` and `attestations: write` for attestation; `pull-requests` and `actions: write` for the site job |
| `pages.yml` | manual, and started by the release workflow after it updates `web/` | Validates `web/` and deploys it to capturefab.com as a Cloudflare Worker with static assets (`site/`, `cf deploy`), but only from the default branch and only when the repository variable `CAPTUREFAB_PUBLISH_SITE` is `true` | `contents: read`; the `CLOUDFLARE_API_TOKEN` secret |

All actions are pinned to full commit SHAs. Checkouts do not persist credentials; the jobs that push pass the job token for that one command.

## Targets and compatibility

| Target | Runner | Toolchain | Runs on |
| --- | --- | --- | --- |
| Linux x86_64 | `ubuntu-24.04`, `manylinux_2_28_x86_64` container | GCC 14 | glibc 2.28 and newer: RHEL, Rocky and Alma 8, Debian 10, Ubuntu 18.10 and newer, Jetson Linux R34 and newer (not Ubuntu 18.04 or Jetson Linux R32, which have glibc 2.27) |
| Linux ARM64 | `ubuntu-24.04-arm`, `manylinux_2_28_aarch64` container | GCC 14 | glibc 2.28 and newer |
| macOS ARM64 | `macos-15` | Apple clang | macOS 11.0 and newer |
| macOS x86_64 | `macos-15-intel` | Apple clang, NASM | macOS 10.15 and newer |
| Windows x86_64 | `windows-2025` | MSVC for Capturefab, MSYS2 UCRT64 GCC for FFmpeg | Windows 10 and newer, and Windows 11 on ARM through emulation; no Visual C++ redistributable needed |

`scripts/release.py build` enforces these floors on both the Capturefab executable and the FFmpeg it embeds: the newest `GLIBC_` symbol version an ELF requires, the minimum macOS a Mach-O declares, and on Windows that no Visual C++ runtime DLL (`VCRUNTIME140.dll`, `MSVCP140.dll` and similar) is imported or delay-loaded. `.cargo/config.toml` links the C runtime statically on Windows for that reason. A dependency or toolchain change that raises a floor fails the build instead of shipping a binary that will not start.

Each target builds a desktop (`--features wgpu`) and a headless (`--no-default-features --features usb,jpeg,nvjpeg,vaapi,videotoolbox`) variant. `scripts/release.py build` embeds the target's FFmpeg, checks that the executable contains exactly that FFmpeg, runs `--version`, `ffmpeg` (SRT input and output must be present) and a two-frame simulator capture, then packages. The `wgpu` feature is kept for these build commands; the GUI always renders through wgpu. Desktop Linux builds need no X11, Wayland, Vulkan or OpenGL development packages: the GUI loads those libraries from the system at run time. A target without a successful build is listed as unavailable, with the packaging error when there is one.

Not built by the workflow, so the manifest lists them as unavailable:

- **Linux ARMv7.** GitHub has no ARMv7 runner. A cross build would be unexecuted; build natively on the device, or cross-compile and package with `--skip-smoke`, which labels the result `cross-compiled`.
- **Windows ARM64.** CI tests Capturefab on `windows-11-arm`, but the FFmpeg recipe cannot build there yet: under MSYS2 CLANGARM64, x264's configure detects the emulated x86_64 shell as the host unless `--host` is passed, and OpenSSL needs `OPENSSL_TARGET=mingwarm64`. The x86_64 build runs on Windows 11 on ARM through emulation meanwhile.

## One-time repository setup

1. **Settings → Environments → New environment `release`**: add required reviewers (and optionally prevent self-review). Without required reviewers the publish job leaves the release as a draft, unless the repository variable `CAPTUREFAB_AUTO_PUBLISH` is `true`.
2. **Settings → Actions → General → Workflow permissions**: allow GitHub Actions to create pull requests, so `prepare-release.yml` and the site job can open them. Otherwise they push a branch and say so in the run summary.
3. If tag rulesets restrict who may create `v*` tags, allow GitHub Actions to create them, or the release workflow cannot tag the merged version.
4. To deploy the site, add a Cloudflare API token with Workers Scripts edit, Workers Routes edit and DNS edit on the capturefab.com zone as the repository secret `CLOUDFLARE_API_TOKEN`, and set `CAPTUREFAB_PUBLISH_SITE=true`. `site/cloudflare.config.ts` names the Worker and its custom domain; `site/wrangler.config.ts` points its assets at `web/`.
5. If `main` is protected, the site job opens a pull request with the `web/` changes instead of pushing; run the Pages workflow after merging it.

## Cutting a release

1. **Actions → Prepare release → Run workflow** with the version, for example `0.2.0` or `0.3.0-rc.1`. It opens the pull request `Release v0.2.0` and starts CI on it.
2. Review and merge the pull request. The Release workflow tags the merge commit `v0.2.0`, builds, attests and drafts the release. Notes start with GitHub's generated list of merged pull requests since the previous tag, grouped by label as `.github/release.yml` configures, followed by each artifact's validation (`native-smoke-passed` or `cross-compiled`), platform signature, unavailable targets and verification instructions.
3. Review the draft from the link in the run summary, then approve the waiting `release` deployment. The workflow publishes the release (marked latest unless it is a prerelease), commits it to `web/releases.json`, `web/downloads.html` and `web/feed.xml`, and starts the Pages deployment.

Pushing a tag yourself (`git tag v0.2.0 && git push origin v0.2.0`) still works when `Cargo.toml` already has that version, and runs the same jobs from the build onwards. To rebuild an existing tag, run the Release workflow manually and choose the tag; a published release is never modified. To update the site by hand instead:

```sh
gh release download v0.2.0 --pattern releases.json --dir /tmp/capturefab-v0.2.0
python3 scripts/site-release.py --merge /tmp/capturefab-v0.2.0/releases.json
python3 scripts/test-site.py
```

A release contains, per target, `capturefab-VERSION-OS-ARCH-{desktop,headless}` archives (`.tar.gz` on Linux, `.zip` elsewhere; macOS desktop adds `Capturefab.app`) and `capturefab-VERSION-OS-ARCH-media-sources.tar.gz`, plus `capturefab-VERSION-source.tar.gz` (the exact committed tree, all `Cargo.lock` dependency sources in `vendor/`, and an offline Cargo configuration), `releases.json`, `capturefab-VERSION-sbom.cdx.json` (a CycloneDX bill of materials: the Rust crates linked into any variant with their `Cargo.lock` checksums, and the pinned FFmpeg, x264, SRT, OpenSSL and GPU header versions with their source checksums), and `SHA256SUMS` covering every file. Assembly refuses uncommitted changes and missing or modified corresponding-source archives. The FFmpeg source archives must match the build recipe's pinned checksums, including enabled GPU headers. Each package holds `LICENSE`, `THIRD-PARTY-LICENSES.txt` (license texts of the Rust crates compiled into that executable), `media-licenses/` and the FFmpeg build manifest.

## Optional signing and provenance

Nothing is signed unless these repository secrets exist. Identities come only from the supplied credentials. The platform signing steps have not yet run with real credentials, so inspect the first signed draft before publishing it.

| Secret | Effect |
| --- | --- |
| `MACOS_CERTIFICATE_P12_BASE64`, `MACOS_CERTIFICATE_PASSWORD` | Imports a Developer ID Application certificate into a temporary keychain and signs the executable and `Capturefab.app` with the hardened runtime and the camera entitlement |
| `MACOS_NOTARY_KEY`, `MACOS_NOTARY_KEY_ID`, `MACOS_NOTARY_ISSUER` | App Store Connect API key (`.p8` contents) for `notarytool`; the desktop app is stapled |
| `WINDOWS_CERTIFICATE_PFX_BASE64`, `WINDOWS_CERTIFICATE_PASSWORD` | Authenticode signing with `signtool` and a DigiCert RFC 3161 timestamp. Requires a certificate with an exportable key; HSM-only certificates need a different signing step |
| `RELEASE_GPG_PRIVATE_KEY`, `RELEASE_GPG_PASSPHRASE` | Detached OpenPGP signature `SHA256SUMS.asc`. Publish the public key separately |

The workflow compiles and packages before importing any signing material; the signing pass uses `build --repackage` to verify and reuse each variant's existing archive without invoking a compiler. Rerunning a draft removes generated platform archives that are no longer available and an obsolete checksum signature, then uploads the newly verified files. Unrelated draft attachments are preserved. Platform signatures appear per artifact as `unsigned`, `apple-developer-id`, `apple-notarized` or `authenticode`.

Every release is attested by default: GitHub build provenance and an SBOM attestation (the CycloneDX file above) for every file in `SHA256SUMS`, verifiable with `gh attestation verify FILE --repo OWNER/REPO`, adding `--predicate-type https://cyclonedx.org/bom` for the SBOM. Dry runs are not attested. In a public repository the attestations, including the repository identity, are recorded in the public Sigstore transparency log; set the repository variable `CAPTUREFAB_ATTEST=false` to turn them off. Private repositories need a GitHub plan that supports attestations, or that variable.

## Local packaging

```sh
JOBS=8 BUILD_DIR=/tmp/ffmpeg-build scripts/build-ffmpeg.sh "$PWD/target/ffmpeg-bundle"
python3 scripts/release.py build --target aarch64-apple-darwin --os macos --arch aarch64 \
  --ffmpeg-bundle target/ffmpeg-bundle --output dist/macos-aarch64
python3 scripts/release.py assemble --input dist --output dist/release --repository capturefab/capturefab
```

`build` needs Python 3.8 or newer; the Linux release containers use their CPython 3.12. `python3 scripts/release.py set-version 0.2.0` raises the version locally the way the Prepare release workflow does. `--binary PATH --variants headless` packages an existing executable. `assemble` needs Cargo to vendor the locked dependencies; without `--download-base` it writes file names as URLs. To build from a source archive without a registry connection, use `cargo --config .cargo/vendor-config.toml build --offline --locked` from its root.

## Screenshots and site pages

- `python3 scripts/cli-svg.py --binary target/debug/capturefab` runs real commands against the simulator in an isolated session directory and writes `web/images/cli.svg`. It refuses to render if discovery finds a non-simulated camera.
- `capturefab gui --screenshot web/images/gui.png --screenshot-cameras 3` saves a real renderer screenshot of simulated cameras.
- `python3 scripts/site-release.py` regenerates `web/downloads.html` and `web/feed.xml` from `web/releases.json`; `--check` fails when they are stale.
- `python3 scripts/test-site.py` validates both pages, local links and anchors, image dimensions, SVG content, the manifest, the Atom feed and generated-page freshness.
