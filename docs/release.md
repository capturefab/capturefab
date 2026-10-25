# Releasing Capturefab

Every release starts as a GitHub draft release candidate with an explicit tag such as `v0.3.0-rc.1`. `Cargo.toml` holds the intended final version (`0.3.0`); the RC tag points to that exact commit. The workflow creates the draft before building, runs the full CI suite and every platform build, smoke-tests and attests the artifacts, then automatically promotes that same release to `v0.3.0`. Promotion creates the final tag on the validated commit, clears the draft and prerelease flags, and updates the downloads site. The RC tag remains as an audit trail. No rebuild or artifact replacement happens during promotion.

Failed or cancelled validation leaves the candidate in draft state and does not create the final tag. Rerunning the workflow on the same commit reuses its draft; a changed commit gets the next unused `-rc.N` tag. Existing tags are never moved and existing final releases are never replaced. Release runs are serialized to prevent concurrent candidate allocation and promotion.
## Workflows

| Workflow | Trigger | Does | Token permissions |
| --- | --- | --- | --- |
| `ci.yml` | push to `main`, pull requests, manual, reusable call from Release | `cargo fmt --check`, strict Clippy with all features, `cargo test` on Linux x64 and ARM64, macOS Apple silicon and Intel, Windows x64 and ARM64 (with a per-runner summary of the hardware accelerators `doctor` finds), the build without the GUI, `scripts/test-site.py`, `node --check web/app.js`, `node --check web/docs.js`, documentation search checks and the script unit tests | `contents: read` |
| `prepare-release.yml` | manual, with a version | Raises the version in `Cargo.toml` and `Cargo.lock` (`release.py set-version`, which refuses a version that is not newer or is already tagged), pushes `release/vVERSION`, opens a pull request labeled `release` and starts CI on it | `contents`, `pull-requests`, `issues` and `actions: write` |
| `release.yml` | a push to `main` that raises the version in `Cargo.toml`; a `vVERSION-rc.N` tag push; manual | Creates an explicitly tagged `-rc.N` draft before building, calls the full CI workflow, builds and smoke-tests every target, signs configured Windows artifacts, assembles and attests the artifacts, uploads to the draft and promotes it automatically only after all checks pass. Manual branch runs are dry runs. Published releases are never modified. | `contents: read`; `contents: write` for candidate, upload, promotion and site jobs; `id-token: write` for Azure signing and attestation; `attestations: write` for attestation; `pull-requests` and `actions: write` for the site job |
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
| Windows ARM64 | `windows-11-arm` | MSVC for Capturefab, MSYS2 CLANGARM64 LLVM for FFmpeg | Windows 11 on ARM; no Visual C++ redistributable needed |

`scripts/release.py build` enforces these floors on both the Capturefab executable and the FFmpeg it embeds: the newest `GLIBC_` symbol version an ELF requires, the minimum macOS a Mach-O declares, and on Windows that no Visual C++ runtime DLL (`VCRUNTIME140.dll`, `MSVCP140.dll` and similar) is imported or delay-loaded. `.cargo/config.toml` links the C runtime statically on Windows for that reason. A dependency or toolchain change that raises a floor fails the build instead of shipping a binary that will not start.

Each target builds one executable with the default features. There is no separate headless build: with no display the same executable lists the CLI commands. When `web-client/dist` exists (`./web-client/build.sh`), it also gets `embedded-web`, which builds the browser client into the executable for `serve --listen`; the release workflow's `web-client` job builds it once and every build job downloads it, so release executables from 0.2.2 on have it. The asset then records `browser_client: true`, and THIRD-PARTY-LICENSES.txt and the SBOM list the client's crates. `scripts/release.py build` embeds the target's FFmpeg, checks that the executable contains exactly that FFmpeg, runs `--version`, `ffmpeg` (SRT input and output must be present) and a two-frame simulator capture, then packages. The GUI renders through wgpu, falling back to tiny-skia without a GPU. Linux builds need no X11, Wayland, Vulkan or OpenGL development packages: the GUI loads those libraries from the system at run time. A target without a successful build is listed as unavailable, with the packaging error when there is one.

There is no 32-bit ARM target: Raspberry Pi 3 and later run the 64-bit Raspberry Pi OS and the ARM64 build. Releases before 0.2.2 listed Linux ARMv7 as unavailable.

From the release after 0.4.0, macOS releases are a read-only disk image (`.dmg`) whose root holds
exactly `Capturefab.app`, an `Applications` link to `/Applications` and
`README.txt`; users drag the app to Applications and eject the image. The
executable is stored once, at `Capturefab.app/Contents/MacOS/capturefab`, which
is also the CLI (`/Applications/Capturefab.app/Contents/MacOS/capturefab` after
installing). There is no `./capturefab` shell launcher any more, since it could
not work from a read-only image or after the drag. The license files are in
`Capturefab.app/Contents/Resources/licenses/`, written before signing so the
signature seals them and they travel with the app. The image is `ULFO` (LZFSE),
which needs macOS 10.11 while the oldest supported macOS is 10.15; the fallback
would be `UDZO` with `-imagekey zlib-level=9`. Packaging macOS requires
`hdiutil`, so it runs only on macOS, and every image is verified (`hdiutil
verify`) and mounted read-only to check its root and that its executable matches
the packaged one. Repackaging for signing reads the executable through `hdiutil
attach`. Releases up to 0.4.0 were ZIPs with the launcher beside the app.

## One-time repository setup

1. Promotion is automatic after validation. The workflow no longer uses the `release` environment or `CAPTUREFAB_AUTO_PUBLISH`; existing environment reviewers do not gate future releases.
2. **Settings → Actions → General → Workflow permissions**: allow GitHub Actions to create pull requests, so `prepare-release.yml` and the site job can open them. Otherwise they push a branch and say so in the run summary.
3. If tag rulesets restrict who may create `v*` tags, allow GitHub Actions to create both `vVERSION-rc.N` and `vVERSION`, or the release workflow cannot create candidates or promote them.
4. To deploy the site, add a Cloudflare API token with Workers Scripts edit, Workers Routes edit and DNS edit on the capturefab.com zone as the repository secret `CLOUDFLARE_API_TOKEN`, and set `CAPTUREFAB_PUBLISH_SITE=true`. `site/cloudflare.config.ts` names the Worker and its custom domain; `site/wrangler.config.ts` points its assets at `web/`.
5. If `main` is protected, the site job opens a pull request with the `web/` changes instead of pushing; run the Pages workflow after merging it.

## Cutting a release

1. **Actions → Prepare release → Run workflow** with the intended version, for example `0.3.0`. It opens the pull request `Release v0.3.0` and starts CI on it. Do not add an RC suffix to `Cargo.toml`; candidate numbering is managed by the Release workflow.
2. Review and merge the pull request. The Release workflow creates `v0.3.0-rc.1` and a draft prerelease on that tag, then runs CI and the six native platform builds. Generated notes and artifacts are uploaded to this draft. It remains unpublished while validation is pending or failed.
3. When the complete CI suite, every platform build, assembly, configured attestations and draft upload succeed, the workflow verifies the original candidate ID, draft state and commit, creates `v0.3.0`, and publishes that same release under the final tag. It clears the prerelease flag for stable versions and marks them latest. Intentional SemVer prerelease versions retain their prerelease status. No manual approval is required.
4. If validation fails, inspect the workflow logs and rerun failed jobs or the workflow on the RC tag. For a source fix, keep the intended version and run on a new RC tag at the fixed commit; the workflow allocates the next candidate number. The failed draft stays available for inspection. A manual run on a branch remains a dry run and cannot publish.

Directly pushing an RC tag is supported when its base version matches `Cargo.toml`, for example `git tag -a v0.3.0-rc.1 -m "Release candidate" && git push origin v0.3.0-rc.1`. A pre-existing RC tag on the same commit without a release is reused; otherwise the workflow allocates the next free RC tag. Final version tags are created only during promotion and do not start release workflows. Published versions cannot be rebuilt in place.

To update the site by hand after publication:

```sh
gh release download v0.3.0 --pattern releases.json --dir /tmp/capturefab-v0.3.0
python3 scripts/site-release.py --merge /tmp/capturefab-v0.3.0/releases.json
python3 scripts/test-site.py
```

A release contains, per target, `capturefab-VERSION-OS-ARCH` archives (`.tar.gz` on Linux, `.zip` on Windows, `.dmg` holding `Capturefab.app` on macOS) and `capturefab-VERSION-OS-ARCH-media-sources.tar.gz`, plus `capturefab-VERSION-source.tar.gz` (the exact committed tree, all `Cargo.lock` dependency sources in `vendor/`, and an offline Cargo configuration), `releases.json`, `capturefab-VERSION-sbom.cdx.json` (a CycloneDX bill of materials: the Rust crates linked into the executable with their `Cargo.lock` checksums, and the pinned FFmpeg, x264, SRT, OpenSSL and GPU header versions with their source checksums), and `SHA256SUMS` covering every file. Assembly refuses uncommitted changes and missing or modified corresponding-source archives. The FFmpeg source archives must match the build recipe's pinned checksums, including enabled GPU headers. Each Linux and Windows package holds `LICENSE`, `THIRD-PARTY-LICENSES.txt` (license texts of the Rust crates compiled into that executable), `media-licenses/` and the FFmpeg build manifest; on macOS these are in `Capturefab.app/Contents/Resources/licenses/`.

### Release highlights and the update check

The desktop app's update check (`src/updates.rs`) reads the published `releases.json` from `https://capturefab.com/releases.json`, so the file must stay at the site root: the client does not follow redirects. To show a few lines in the Software Update sheet and in `release-notes.md`, write up to six plain-text lines of at most 200 characters in `docs/releases/VERSION.txt` and pass `--highlights docs/releases/VERSION.txt` to `scripts/release.py assemble`; the release entry then carries an optional `highlights` list (`releases.json` stays schema version 1, and `scripts/test-site.py` validates it). The checker picks the single asset for its operating system and architecture, so each (os, arch) keeps one asset: from 0.5.0 the macOS disk image (`.dmg`) replaces the macOS `.zip` entry rather than being added beside it.

## Agent iteration

`AGENTS.md` defines the release loop for coding agents. `scripts/release-agent.py` provides JSON status and explicit actions, using the local Git/SSH credentials to push candidates and `gh` to inspect GitHub. Starting a candidate requires a clean, committed worktree and refuses an existing final tag. It validates the push remote, allocates the next unused RC number across local and remote tags, and pushes only that candidate tag. It never force-pushes or moves a tag.

```sh
# After committing the release or a candidate fix:
python3 scripts/release-agent.py start
# Use the commit returned by start; once discovered, follow the exact run ID:
python3 scripts/release-agent.py status --commit COMMIT_SHA
python3 scripts/release-agent.py status --run RUN_ID
python3 scripts/release-agent.py logs --run RUN_ID
# Only for transient infrastructure failures on unchanged source:
python3 scripts/release-agent.py retry --run RUN_ID
```

For build or test failures, inspect all failed jobs, fix and locally validate the cause, commit the fix, then start the next RC. Do not rerun an old candidate expecting it to include local source changes. Existing failed drafts and tags remain available for diagnosis. A new RC must pass the whole promotion gate, including targets that passed on the previous candidate.

For a transient runner or service failure, `retry` reruns failed jobs on the same commit. It needs Actions write access; inspection needs read access, and `start` needs Git push access. Missing credentials, signing failures or unavailable infrastructure are reported as blockers rather than bypassed. Agents monitor with bounded polls, report progress, and continue through automatic promotion and verification of the final release. JSON status distinguishes a candidate failure from a site failure after publication, so a site repair does not create another binary release. The script does not generate or execute source fixes itself; the coding agent diagnoses and edits using the job evidence.

## Signing and provenance

Releases require Apple signing and notarization: without the `MACOS_CERTIFICATE_*` and `MACOS_NOTARY_*` secrets the Release workflow stops before building, because Gatekeeper blocks unsigned or unnotarized downloads ("cannot be verified" / "Apple could not verify"). Set the repository variable `CAPTUREFAB_ALLOW_UNSIGNED_MACOS=true` to release unsigned macOS builds anyway. Windows signing is optional until configured. Set `CAPTUREFAB_WINDOWS_SIGNING=azure` to require Azure Artifact Signing for releases; missing configuration, failed signing or failed verification blocks publication. An empty value retains the existing optional PFX signing behavior; other nonempty values are rejected. Identities come only from the configured credentials and certificate profile.

| Secret | Effect |
| --- | --- |
| `MACOS_CERTIFICATE_P12_BASE64`, `MACOS_CERTIFICATE_P12_PASSWORD` | Imports a Developer ID Application certificate into a temporary keychain and signs the executable and `Capturefab.app` with the hardened runtime and the camera entitlement, and the disk image |
| `MACOS_NOTARY_KEY`, `MACOS_NOTARY_KEY_ID`, `MACOS_NOTARY_ISSUER` | App Store Connect API key (`.p8` contents) for `notarytool`; the desktop app is stapled |
| `WINDOWS_CERTIFICATE_PFX_BASE64`, `WINDOWS_CERTIFICATE_PASSWORD` | Optional legacy Authenticode signing with `signtool` and a DigiCert RFC 3161 timestamp when `CAPTUREFAB_WINDOWS_SIGNING` is empty. Requires a certificate with an exportable key. Azure signing does not use these secrets |
| `RELEASE_GPG_PRIVATE_KEY`, `RELEASE_GPG_PASSPHRASE` | Detached OpenPGP signature `SHA256SUMS.asc`. Publish the public key separately |

The workflow compiles, smoke-tests and packages before signing. Apple and legacy PFX signing use `build --repackage` to verify and reuse each target's existing archive without invoking a compiler. Azure signing runs separately on Windows x64 after both native Windows builds, preserving their smoke-test results and corresponding-source archives; it can sign ARM64 executables without executing them. The prepare/finish steps verify the input archives, replace only the signed executable and signature description, then refresh package hashes. No application code runs after Azure login. Assembly uses only the signed Windows replacements, and a signing failure cannot fall back to unsigned artifacts. Rerunning a draft removes generated platform archives that are no longer available and an obsolete checksum signature, then uploads the newly verified files. Unrelated draft attachments are preserved. Platform signatures appear per artifact as `unsigned`, `apple-developer-id`, `apple-notarized` or `authenticode`.

On macOS the signing pass signs the executable and `Capturefab.app` with the hardened runtime, a secure timestamp and the camera entitlement and verifies them with `codesign --verify --strict`. Notarization then runs twice, and each submission fails unless it is `Accepted` (printing the notary log otherwise). First the app goes to the notary service in a temporary `ditto` ZIP that never ships; it is stapled and must pass `stapler validate` and `spctl --assess --type execute`, so the copy dragged to Applications carries its own ticket and opens offline. Only then are `README.txt` (which reports the app's signature) and the disk image built. The image is signed (`codesign --timestamp`, no entitlements), notarized, stapled and must pass `spctl --assess --type open --context context:primary-signature`. Stapling the app after notarizing the image would change the image, hence the order. Every `hdiutil` call has a 600 s timeout that fails the build with a recorded failure; `hdiutil create` is retried at most three times, five seconds apart, only for `Resource busy`. Stapling and ticket validation each allow at most three attempts, five seconds apart, only for the observed Apple CloudKit connection-loss error: exit 68 with `NSURLErrorDomain Code=-1005` and an `https://api.apple-cloudkit.com/` URL. Both operations must succeed; invalid tickets, other errors and a rejected Gatekeeper assessment stop immediately. A bare command-line executable cannot hold a stapled ticket, so Gatekeeper looks up its notarization online the first time it runs.

### Setting up Azure Artifact Signing for Windows

1. Follow Microsoft's [Artifact Signing quickstart](https://learn.microsoft.com/en-us/azure/artifact-signing/quickstart): register `Microsoft.CodeSigning`, create an Artifact Signing account, complete public identity validation in the Azure portal, and create a **Public Trust** certificate profile using that validated identity. Record the account name, profile name and regional endpoint, for example `https://eus.codesigning.azure.net/` for East US. Use the endpoint for the account's region.
2. Create a Microsoft Entra app registration and service principal for this repository, following [Azure login with OpenID Connect](https://learn.microsoft.com/en-us/azure/developer/github/connect-from-azure-openid-connect). Record the application (client) ID, directory (tenant) ID and subscription ID. Create a federated credential with issuer `https://token.actions.githubusercontent.com`, audience `api://AzureADTokenExchange`, and subject `repo:OWNER/REPO:environment:windows-signing` (for this repository, `repo:capturefab/capturefab:environment:windows-signing`). No client secret is required.
3. Assign the app's service principal the **Artifact Signing Certificate Profile Signer** role at the specific certificate profile scope. Microsoft's [role assignment instructions](https://learn.microsoft.com/en-us/azure/artifact-signing/tutorial-assign-roles) include the scope format `/subscriptions/SUBSCRIPTION_ID/resourceGroups/RESOURCE_GROUP/providers/Microsoft.CodeSigning/codeSigningAccounts/ACCOUNT_NAME/certificateProfiles/PROFILE_NAME`. Owner or Contributor alone does not grant signing permission.
4. In GitHub **Settings → Environments**, create `windows-signing`. Restrict its deployment branches and tags to `main` and `v*` tags. Both are needed: a version-bump release starts on `main`, while an explicit candidate starts on its tag. The federated credential and workflow environment name must match exactly. Add required reviewers only if a manual signing gate is desired.
5. Store the following as **repository variables**, then enable Azure signing last. Replace the example IDs and names with your values; the IDs are identifiers, not client secrets or signing keys.

```sh
gh variable set AZURE_CLIENT_ID --repo capturefab/capturefab --body APPLICATION-CLIENT-ID
gh variable set AZURE_TENANT_ID --repo capturefab/capturefab --body DIRECTORY-TENANT-ID
gh variable set AZURE_SUBSCRIPTION_ID --repo capturefab/capturefab --body SUBSCRIPTION-ID
gh variable set AZURE_ARTIFACT_SIGNING_ENDPOINT --repo capturefab/capturefab --body https://eus.codesigning.azure.net/
gh variable set AZURE_ARTIFACT_SIGNING_ACCOUNT_NAME --repo capturefab/capturefab --body ACCOUNT-NAME
gh variable set AZURE_ARTIFACT_SIGNING_CERTIFICATE_PROFILE --repo capturefab/capturefab --body PROFILE-NAME
gh variable set CAPTUREFAB_WINDOWS_SIGNING --repo capturefab/capturefab --body azure
```

Actual releases use the pinned official Azure login and Artifact Signing actions with OIDC in the `windows-signing` environment. The x64 and ARM64 executables receive SHA-256 Authenticode signatures and Microsoft's RFC 3161 timestamp (`http://timestamp.acs.microsoft.com`). SignTool verifies trust and the presence of a timestamp before repackaging; Artifact Signing certificates are short lived, so timestamping is required for signatures to remain valid after certificate expiry. See Microsoft's [signing integration instructions](https://learn.microsoft.com/en-us/azure/artifact-signing/how-to-signing-integrations).

Manual runs from a branch remain dry runs: with Azure enabled they retain unsigned Windows artifacts and do not enter the Azure signing job or request its OIDC token. Azure account setup, identity validation and the first signing run require the real account; local checks cannot validate those external permissions. Check an enabled candidate's signing job and its reported `authenticode` artifacts before relying on the setup.

### Setting up Apple signing

Needs an Apple Developer Program membership; the Account Holder creates the certificate.

1. In Keychain Access → Certificate Assistant → Request a Certificate From a Certificate Authority, save a CSR to disk. At [developer.apple.com → Certificates](https://developer.apple.com/account/resources/certificates/add) choose **Developer ID Application** (not Apple Development or Apple Distribution), upload the CSR and open the downloaded certificate so it joins your login keychain with its private key.
2. Check it with `security find-identity -v -p codesigning`; it reads `Developer ID Application: NAME (TEAMID)`. In Keychain Access, export that identity (certificate with its key) as a `.p12` with a strong password.
3. At [App Store Connect → Users and Access → Integrations → Team Keys](https://appstoreconnect.apple.com/access/integrations/api), create a key with the Developer role. Note its Key ID and the Issuer ID shown above the list, and download the `.p8` (it can be downloaded only once).
4. As a repository admin, store the signing secrets:

```sh
base64 -i DeveloperID.p12 | gh secret set MACOS_CERTIFICATE_P12_BASE64 --repo capturefab/capturefab
gh secret set MACOS_CERTIFICATE_P12_PASSWORD --repo capturefab/capturefab        # prompts for the .p12 password
gh secret set MACOS_NOTARY_KEY --repo capturefab/capturefab < AuthKey_KEYID.p8
gh secret set MACOS_NOTARY_KEY_ID --repo capturefab/capturefab --body KEYID
gh secret set MACOS_NOTARY_ISSUER --repo capturefab/capturefab --body ISSUER-UUID
```

Successful candidates are promoted automatically after all validation passes. Set `CAPTUREFAB_PUBLISH_SITE=true` and the `CLOUDFLARE_API_TOKEN` secret so the downloads page deploys after publishing.

To check notarization locally before relying on CI: `xcrun notarytool store-credentials capturefab-notary --key AuthKey_KEYID.p8 --key-id KEYID --issuer ISSUER-UUID`, then run `release.py build` with `MACOS_SIGN_IDENTITY` set to the identity hash and `MACOS_NOTARY_PROFILE=capturefab-notary`. After downloading a release, `spctl --assess --type open --context context:primary-signature -vv capturefab-VERSION-macos-ARCH.dmg` and, once installed, `spctl --assess --type execute -vv /Applications/Capturefab.app` should report `source=Notarized Developer ID`; `xcrun stapler validate` should pass on both the `.dmg` and `/Applications/Capturefab.app`. Without credentials, `release.py build` on a Mac produces an unsigned image with the same layout and checks; `codesign --sign -` gives an ad-hoc signature for rehearsing the commands, which Gatekeeper rejects.

Every release is attested by default: GitHub build provenance and an SBOM attestation (the CycloneDX file above) for every file in `SHA256SUMS`, verifiable with `gh attestation verify FILE --repo OWNER/REPO`, adding `--predicate-type https://cyclonedx.org/bom` for the SBOM. Dry runs are not attested. In a public repository the attestations, including the repository identity, are recorded in the public Sigstore transparency log; set the repository variable `CAPTUREFAB_ATTEST=false` to turn them off. Private repositories need a GitHub plan that supports attestations, or that variable.

## Local packaging

```sh
JOBS=8 BUILD_DIR=/tmp/ffmpeg-build scripts/build-ffmpeg.sh "$PWD/target/ffmpeg-bundle"
python3 scripts/release.py build --target aarch64-apple-darwin --os macos --arch aarch64 \
  --ffmpeg-bundle target/ffmpeg-bundle --output dist/macos-aarch64
python3 scripts/release.py assemble --input dist --output dist/release --repository capturefab/capturefab
```

`build` needs Python 3.8 or newer; the Linux release containers use their CPython 3.12. `python3 scripts/release.py set-version 0.2.0` raises the version locally the way the Prepare release workflow does. `--binary PATH` packages an existing executable. `assemble` needs Cargo to vendor the locked dependencies; without `--download-base` it writes file names as URLs. To build from a source archive without a registry connection, use `cargo --config .cargo/vendor-config.toml build --offline --locked` from its root.

## Screenshots and site pages

- `python3 scripts/cli-svg.py --binary target/debug/capturefab` runs real commands against the simulator in an isolated session directory and writes `web/images/cli.svg`. It refuses to render if discovery finds a non-simulated camera.
- `capturefab gui --screenshot web/images/gui.png --screenshot-cameras 3` saves a real renderer screenshot of simulated cameras.
- `python3 scripts/docs-screenshots.py --binary target/debug/capturefab` refreshes the nine documentation illustrations from seven real desktop simulator scenes. It requires Pillow and a desktop large enough for a 1100 × 700 window. The script uses isolated session data, preserves full renders in `web/images/docs/source/`, and records crop bounds, dimensions and hashes in `web/images/docs/manifest.json`; `--crop-only` reuses those renders. Update the HTML image dimensions and regenerate the search index after changing a crop or caption.
- `uv run --with pillow scripts/site-media.py` refreshes the two eight-second, 30 fps site MP4s and their PNG posters in `web/media/`. It runs on a desktop with a display, not in CI, and needs Pillow and the pinned FFmpeg from `scripts/build-ffmpeg.sh` (found in `target/ffmpeg/<host>/bin`, or given with `CAPTUREFAB_FFMPEG` or `--ffmpeg`; it refuses a nonfree build and records whether the one used is pinned). FFprobe is not needed. Without `--binary` it first builds an optimized binary with `cargo build --release`; a debug binary is refused unless `--allow-debug` is passed, because it cannot read frames back fast enough. Each clip uses actual native renderer frames from built-in simulator cameras in an isolated session with `CAPTUREFAB_EFFICIENCY=smooth`. Real CLI commands change exposure and save PNG frames. The script captures renderer frames in real time at up to 30 fps, each staged once the preview shows a new camera frame, times them from the renderer's staging clock, only repeats frames (never interpolates), and fails unless at least 24 frames/s were captured, no gap exceeds 100 ms and the preview shows new camera frames; a recording that fails this is retried up to `--attempts` times (3) and `web/media/` changes only when every clip passes. It encodes silent constant-rate H.264 with `yuv420p`, fast start, no B-frames, a fixed 60-frame GOP and bitexact flags, stepping the CRF from 21 to 27 until each clip fits 1,500,000 bytes, with at most 3,500,000 bytes for all clips, and checks that the camera image changes and that no frame's text columns came out blank. It does not record the display, open a physical camera or use saved user data. `web/media/manifest.json` records the build hash, encoder and FFmpeg version, commands, capture timing, dimensions, duration and media hashes. The internal screenshot sequence uses `CAPTUREFAB_SCREENSHOT_FRAMES` (1–900) and `CAPTUREFAB_SCREENSHOT_FPS` (1–60) and also writes `<stem>-timing.jsonl` beside the frames; a normal `--screenshot` still saves one PNG and exits.
- `python3 scripts/site-release.py` regenerates `web/downloads.html`, `web/feed.xml` and the `web/install.sh` / `web/install.ps1` installers (from `scripts/install-template.*`, with the latest release's URLs and SHA-256 checksums written in) from `web/releases.json`; `--check` fails when they are stale.
- `python3 scripts/docs-search.py` regenerates `web/search-index.json` from the documentation articles, including their commands and captions. Run it after editing `web/docs.html`; `--check` detects a stale index.
- `node scripts/test-docs-search.js` checks search relevance, command names, multiword queries and the real documentation corpus.
- `python3 scripts/test-site.py` validates every HTML page, local links and anchors, image dimensions, SVG content, recorded clip frame rate, repeated-frame share and size budget, the manifest, the Atom feed, the documentation search index and generated-page freshness.
