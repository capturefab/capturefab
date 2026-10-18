# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Capturefab is an MIT-licensed Rust camera workbench: one binary that is both an iced desktop app (no subcommand) and a scriptable CLI. It implements GigE Vision, USB3 Vision and GenICam natively in Rust; webcams, RTSP/SRT/ONVIF, files, decoding and encoding go through a bundled FFmpeg executable (not linked), except JPEG/PNG stills and MJPEG from camera hubs, which are decoded in the camera worker with libjpeg-turbo/png. Aravis is currently used only as an external test reference.

## Vendor and native dependencies

Linking vendor SDKs and native libraries (camera SDKs, CUDA/nvJPEG, platform media frameworks, libusb, Aravis) is permitted when it fits the repo's structure:
- Gate each one behind an optional Cargo feature so the default and headless builds keep working without it, and so `doctor` can report whether it was compiled in.
- Keep it in the process that uses it: camera SDKs belong in the per-camera worker (`session::run_worker`), never in the GUI or coordinator.
- Isolate it in a separate helper process, the way FFmpeg is run, when its license cannot be combined with Capturefab's MIT code in release binaries (for example GPL), when it can crash, hang or leak and would take the camera worker down with it, or when it would bloat or complicate every build. Talk to helpers over the existing patterns: bounded stdio JSON for control, shared memory or pipes for pixels.
- Check the vendor's redistribution terms before bundling its runtime, and add its notices to the release license output.

FFmpeg stays a separate executable. Never enable nonfree FFmpeg components: the bundled build is GPL, so a nonfree build could not be redistributed.

## Commands

```sh
cargo build                                                   # desktop (default features: gui, usb, scanner, jpeg, nvjpeg, vaapi, videotoolbox, s3, ftp, sftp, parquet, modbus, mqtt, opcua, mtconnect, dinov2, hubs)
cargo build --release --no-default-features --features usb,scanner,jpeg,nvjpeg,vaapi,videotoolbox,s3,ftp,sftp,parquet,modbus,mqtt,opcua,mtconnect,dinov2,hubs  # headless, as CI and scripts/release.py build it
cargo test
cargo test --test cli                                         # end-to-end binary tests (tests/cli.rs)
cargo test --lib storage::                                    # unit tests in one module
cargo test --lib <test_name> -- --exact
cargo clippy --all-features --all-targets -- -D warnings
cargo bench --bench runtime --no-default-features -- rgb/      # optional substring filters
python3 scripts/test-site.py                                  # validate static site in web/
python3 scripts/test-media.py --binary target/debug/capturefab [--srt]  # needs MediaMTX on :18554/:18890
```

Use the simulator (`--camera sim:0`, `sim:1`, ...) to exercise acquisition without hardware. Set `CAPTUREFAB_SESSION_DIR` and `CAPTUREFAB_CACHE_DIR` to isolated temp dirs when running the binary manually or in tests (tests set both for every binary they spawn; unit tests isolate themselves). Three directories (`ipc::runtime_dir`, `state_dir`, `cache_dir`): runtime holds descriptors, locks and other files needed only while processes run (`$XDG_RUNTIME_DIR/capturefab`, else a lingering Linux login's `/run/user/<uid>/capturefab`, else `~/.cache/capturefab/sessions`); state holds `gui.prefs.json`, saved hubs, destinations, the upload queue and the storage ledger (`$XDG_STATE_HOME/capturefab` on Linux, the runtime dir elsewhere); cache holds the extracted FFmpeg, encoder probes and Linux dataset staging. `CAPTUREFAB_SESSION_DIR` overrides runtime and state together, `CAPTUREFAB_CACHE_DIR` the cache. Worker frame rings live in anonymous shared memory, with files in the runtime dir only as a fallback and on Windows.

DINOv2 (auto capture): `python3 scripts/fetch-dinov2.py` downloads the pinned, checksummed `facebook/dinov2-small` weights to `target/models/dinov2-small/`; release builds embed them (or `CAPTUREFAB_DINOV2_WEIGHTS`), dev/test builds read that file in place, and unit tests use the tiny fixtures from `scripts/dinov2-fixtures.py`. GPU inference is wgpu compute (`src/autocapture/dinov2/dinov2.wgsl`) in the isolated `__embed` helper; the CPU path is the reference.

FFmpeg: `build.rs` embeds `CAPTUREFAB_FFMPEG_BINARY` when set at build time, and `target/ffmpeg/<target>/bin/ffmpeg` only in release-profile builds; dev/test builds instead run that file in place when it exists, which keeps their binaries and links small. At runtime `CAPTUREFAB_FFMPEG=/abs/path/ffmpeg` overrides. Media tests requiring FFmpeg are `#[ignore]`d; run them with `-- --ignored` once FFmpeg is available. `scripts/build-ffmpeg.sh` builds the pinned, checksummed GPL FFmpeg; `scripts/release.py` packages releases.

## Architecture

**Process model.** `main.rs` dispatches the hidden `__worker fd:N` (or `__worker <ring-path>` for a file ring) argv to `session::run_worker`; everything else goes through `cli::run`. Each connected camera runs in its own child process (re-exec of the same binary, overridable via `CAPTUREFAB_EXECUTABLE`):
- `session::SessionHandle` / `Coordinator` (parent) owns one `Process` per camera, sends `SessionCommand` as newline-delimited JSON over the child's stdin and reads `{ok,result,error,snapshot}` lines from stdout.
- Frames bypass the pipe: the worker writes into a `shared_memory::SharedRing` (mmap'd three-slot, newest-frame-wins ring, in a memfd or unlinked POSIX shm object passed as an inherited descriptor); the parent `take_latest()`s from it. The worker copies every frame only while the parent polls, otherwise one keep-alive per second; one-off readers use `SessionHandle::fresh_frame_for`, and captures are always published (see the `shared_memory` module docs).
- Inside the worker, `engine::WorkerHandle` runs a single worker thread that exclusively owns the `camera::Camera` (and its transport), forwarder and scheduled capture jobs (`scheduling.rs`).

**Sessions / IPC.** `gui --name X` and `serve --name X` start an `ipc::Server` (loopback TCP, token-authenticated, descriptor file in the private runtime dir). `--session X` clients use `ipc::call` to send the same `SessionCommand`s. Without `--session`, the CLI creates a local `SessionHandle` in-process, so direct commands and session commands share one code path. The GUI (`gui/`, behind the `gui` feature) is just another `SessionHandle` consumer and shows IPC commands in its activity log; it keeps settings and recent cameras in `gui.prefs.json` in the state dir (`gui/prefs.rs`, never credential-bearing addresses) and uses embedded Phosphor icon fonts (`assets/fonts`).

**Camera layers.** `types.rs` defines the `RegisterIo` and `Backend` traits (xml/start/next_frame/stop). Implementations live in `transport/` (`gige.rs` GVCP/GVSP, `usb.rs` U3V via `nusb` behind the `usb` feature, `simulator.rs`), while `media.rs` provides FFmpeg-backed sources (host webcams, RTSP/SRT, files), recording and forwarding with encoder probing. `genicam.rs` parses GenICam XML (incl. zipped) into a feature node map evaluated live against `RegisterIo`. `camera.rs` ties discovery + backend + GenICam together. `onvif.rs` does WS-Discovery and SOAP profile resolution to an RTSP URI.

**Storage.** All captures and recordings go through `storage.rs`: an OS-locked (`fs2`) ledger (snapshot plus append-only journal) in the state dir enforcing a global byte/file budget (default 10 GiB / 10,000 files), per-recording max size, reservations, and safe deletion (identity + SHA-256 checks). Write output via `storage::save` / `create_writer`, never directly. Existing output files must never be overwritten.

**Destinations and uploads.** `destination.rs` holds named folder and S3 destinations (no secrets) in the state dir; captures, schedules and recordings resolve `--destination` to a local path and, for buckets, queue the finished file in `upload.rs` (a locked, persistent queue drained by whichever GUI/`serve`/CLI process holds `uploader.lock`). `s3.rs` is a SigV4 client over `ureq`/rustls with conditional writes; secrets come from the OS keychain (`keyring`) or the AWS credential chain. Queued files are held back from retention in the storage ledger until uploaded. `volumes.rs` lists external drives and refuses writes under an unmounted mount point. All of it is behind the `s3` feature except folders and volumes.

**Hubs.** `hubs/` saves Home Assistant, Frigate, go2rtc and UniFi Protect servers so their cameras open as `hub:NAME/CAMERA` (a selector names a saved hub and camera, never a host or credential). `hubs.json` in the state dir holds no secrets (`registry.rs`); secrets live in the OS keychain (service `capturefab-hub`, account `KIND:NAME@ORIGIN` including any user and base path, so a changed URL never reads another server's secret; `keychain` feature) or in a `CAPTUREFAB_HUB_*` variable, and are resolved in the process that opens or lists the camera (`secret.rs`, zeroized and registered with `scrub.rs`; `media::command` strips those variables from FFmpeg). Hub listing for `discover` runs only in the coordinator (`discovery.rs`, cached); a worker only opens the one hub camera it owns. In the worker, `hubs::connect` builds the kind's sources and the frame pump (`pump.rs`, one thread per camera) fetches over the hand-rolled HTTP/1.1 + rustls client (`http.rs`, `tls.rs`), decodes JPEG/PNG stills and MJPEG parts (`image.rs`, `multipart.rs`) and reconnects with backoff; RTSP restreams go through FFmpeg (`sources.rs`). Hub errors are `types::Categorized`, which `cli::error_code` maps by category before reading message text. Selector parsing and the saved records build without the `hubs` feature; the network clients need it.

## CLI contract

The CLI is a public, versioned automation contract (see `capturefab schema`, `web/agents.md`, `web/llms.txt`):
- `--json` prints `{"version":1,"ok":true,"result":...}` or `{"version":1,"ok":false,"error":{"code","message"}}` to stdout; diagnostics go to stderr.
- Exit codes come from `cli::error_code`, which checks typed error categories before classifying by error message text: 1 operation_failed, 2 usage, 3 unavailable, 4 timeout, 5 unsupported, 6 storage_full. Wording of error messages therefore affects exit codes.
- Credentials in URLs must be redacted (`media::redact_url`) in logs, status and UI.

When changing commands or output shapes, keep `schema` output, `README.md`, `web/agents.md` and `web/llms.txt` consistent. Avoid publishing camera testing results or compatibility claims while the software is under rapid development.
