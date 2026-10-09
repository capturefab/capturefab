# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Capturefab is an MIT-licensed Rust camera workbench: one binary that is both an iced desktop app (no subcommand) and a scriptable CLI. It implements GigE Vision, USB3 Vision and GenICam natively in Rust; webcams, RTSP/SRT/ONVIF, files, decoding and encoding go through a bundled FFmpeg executable (not linked). Aravis is currently used only as an external test reference.

## Vendor and native dependencies

Linking vendor SDKs and native libraries (camera SDKs, CUDA/nvJPEG, platform media frameworks, libusb, Aravis) is permitted when it fits the repo's structure:
- Gate each one behind an optional Cargo feature so the default and headless builds keep working without it, and so `doctor` can report whether it was compiled in.
- Keep it in the process that uses it: camera SDKs belong in the per-camera worker (`session::run_worker`), never in the GUI or coordinator.
- Isolate it in a separate helper process, the way FFmpeg is run, when its license cannot be combined with Capturefab's MIT code in release binaries (for example GPL), when it can crash, hang or leak and would take the camera worker down with it, or when it would bloat or complicate every build. Talk to helpers over the existing patterns: bounded stdio JSON for control, shared memory or pipes for pixels.
- Check the vendor's redistribution terms before bundling its runtime, and add its notices to the release license output.

FFmpeg stays a separate executable. Never enable nonfree FFmpeg components: the bundled build is GPL, so a nonfree build could not be redistributed.

## Commands

```sh
cargo build                                                   # desktop (default features: gui, usb, jpeg, nvjpeg, vaapi, videotoolbox)
cargo build --release --no-default-features --features usb,jpeg,nvjpeg,vaapi,videotoolbox  # headless
cargo test
cargo test --test cli                                         # end-to-end binary tests (tests/cli.rs)
cargo test --lib storage::                                    # unit tests in one module
cargo test --lib <test_name> -- --exact
cargo clippy --all-features --all-targets -- -D warnings
cargo bench --bench runtime --no-default-features -- rgb/      # optional substring filters
python3 scripts/test-site.py                                  # validate static site in web/
python3 scripts/test-media.py --binary target/debug/capturefab [--srt]  # needs MediaMTX on :18554/:18890
```

Use the simulator (`--camera sim:0`, `sim:1`, ...) to exercise acquisition without hardware. Set `CAPTUREFAB_SESSION_DIR` to an isolated temp dir when running the binary manually or in tests; it holds session descriptors, worker shared-memory rings and the storage ledger.

FFmpeg: `build.rs` embeds `CAPTUREFAB_FFMPEG_BINARY` when set at build time, and `target/ffmpeg/<target>/bin/ffmpeg` only in release-profile builds; dev/test builds instead run that file in place when it exists, which keeps their binaries and links small. At runtime `CAPTUREFAB_FFMPEG=/abs/path/ffmpeg` overrides. Media tests requiring FFmpeg are `#[ignore]`d; run them with `-- --ignored` once FFmpeg is available. `scripts/build-ffmpeg.sh` builds the pinned, checksummed GPL FFmpeg; `scripts/release.py` packages releases.

## Architecture

**Process model.** `main.rs` dispatches the hidden `__worker <ring-path>` argv to `session::run_worker`; everything else goes through `cli::run`. Each connected camera runs in its own child process (re-exec of the same binary, overridable via `CAPTUREFAB_EXECUTABLE`):
- `session::SessionHandle` / `Coordinator` (parent) owns one `Process` per camera, sends `SessionCommand` as newline-delimited JSON over the child's stdin and reads `{ok,result,error,snapshot}` lines from stdout.
- Frames bypass the pipe: the worker writes into a `shared_memory::SharedRing` (mmap'd three-slot, newest-frame-wins ring); the parent `take_latest()`s from it.
- Inside the worker, `engine::WorkerHandle` runs a single worker thread that exclusively owns the `camera::Camera` (and its transport), forwarder and scheduled capture jobs (`scheduling.rs`).

**Sessions / IPC.** `gui --name X` and `serve --name X` start an `ipc::Server` (loopback TCP, token-authenticated, descriptor file in the private session dir). `--session X` clients use `ipc::call` to send the same `SessionCommand`s. Without `--session`, the CLI creates a local `SessionHandle` in-process, so direct commands and session commands share one code path. The GUI (`gui/`, behind the `gui` feature) is just another `SessionHandle` consumer and shows IPC commands in its activity log; it keeps settings and recent cameras in `gui.prefs.json` in the session dir (`gui/prefs.rs`, never credential-bearing addresses) and uses embedded Phosphor icon fonts (`assets/fonts`).

**Camera layers.** `types.rs` defines the `RegisterIo` and `Backend` traits (xml/start/next_frame/stop). Implementations live in `transport/` (`gige.rs` GVCP/GVSP, `usb.rs` U3V via `nusb` behind the `usb` feature, `simulator.rs`), while `media.rs` provides FFmpeg-backed sources (host webcams, RTSP/SRT, files), recording and forwarding with encoder probing. `genicam.rs` parses GenICam XML (incl. zipped) into a feature node map evaluated live against `RegisterIo`. `camera.rs` ties discovery + backend + GenICam together. `onvif.rs` does WS-Discovery and SOAP profile resolution to an RTSP URI.

**Storage.** All captures and recordings go through `storage.rs`: an OS-locked (`fs2`) ledger in the session dir enforcing a global byte/file budget (default 10 GiB / 10,000 files), per-recording max size, reservations, and safe deletion (identity + SHA-256 checks). Write output via `storage::save` / `create_writer`, never directly. Existing output files must never be overwritten.

**Destinations and uploads.** `destination.rs` holds named folder and S3 destinations (no secrets) in the session dir; captures, schedules and recordings resolve `--destination` to a local path and, for buckets, queue the finished file in `upload.rs` (a locked, persistent queue drained by whichever GUI/`serve`/CLI process holds `uploader.lock`). `s3.rs` is a SigV4 client over `ureq`/rustls with conditional writes; secrets come from the OS keychain (`keyring`) or the AWS credential chain. Queued files are held back from retention in the storage ledger until uploaded. `volumes.rs` lists external drives and refuses writes under an unmounted mount point. All of it is behind the `s3` feature except folders and volumes.

## CLI contract

The CLI is a public, versioned automation contract (see `capturefab schema`, `web/agents.md`, `web/llms.txt`):
- `--json` prints `{"version":1,"ok":true,"result":...}` or `{"version":1,"ok":false,"error":{"code","message"}}` to stdout; diagnostics go to stderr.
- Exit codes come from `cli::error_code`, which classifies by error message text: 1 operation_failed, 2 usage, 3 unavailable, 4 timeout, 5 unsupported, 6 storage_full. Wording of error messages therefore affects exit codes.
- Credentials in URLs must be redacted (`media::redact_url`) in logs, status and UI.

When changing commands or output shapes, keep `schema` output, `README.md`, `web/agents.md` and `web/llms.txt` consistent. Avoid publishing camera testing results or compatibility claims while the software is under rapid development.
