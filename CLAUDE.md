# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Capturefab is an MIT-licensed Rust camera capture command-line tool for people, scripts and AI agents: one `capturefab` executable that lists, configures, captures from and records cameras, with a versioned JSON contract (`--json`, typed exit codes, `capturefab schema`). It implements GigE Vision, USB3 Vision and GenICam natively in Rust; webcams, RTSP/SRT/ONVIF streams, files and video encoding go through a bundled FFmpeg executable (not linked). JPEG and PNG stills are encoded in process (libjpeg-turbo, png). Aravis is used only as an external test reference. There is no GUI, browser client, in-app assistant or update check.

## Vendor and native dependencies

Linking vendor SDKs and native libraries (camera SDKs, platform media frameworks, libusb, Aravis) is permitted when it fits the repo's structure:
- Gate each one behind an optional Cargo feature so the default and `--no-default-features` builds keep working without it, and so `doctor` can report whether it was compiled in.
- Keep it in the process that uses it: camera SDKs belong in the per-camera worker (`session::run_worker`), never in the CLI client or the session coordinator.
- Isolate it in a separate helper process, the way FFmpeg is run, when its license cannot be combined with Capturefab's MIT code in release binaries (for example GPL), when it can crash, hang or leak and would take the camera worker down with it, or when it would bloat or complicate every build. Talk to helpers over bounded stdio JSON for control and pipes for pixels.
- Check the vendor's redistribution terms before bundling its runtime, and add its notices to the release license output.

FFmpeg stays a separate executable. Never enable nonfree FFmpeg components: the bundled build is GPL, so a nonfree build could not be redistributed.

## Commands

```sh
cargo build                                           # default features: usb, jpeg, keychain
cargo build --no-default-features                     # CI checks it, and each feature alone
cargo test
cargo test --test capture                             # end-to-end binary tests (tests/*.rs, helpers in tests/support)
cargo test --lib <test_name> -- --exact
cargo test -- --ignored                               # FFmpeg media tests, once FFmpeg is available
CAPTUREFAB_BLESS=1 cargo test --test frame_golden     # rewrite tests/golden/frame.json after an intended change
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings
cargo bench --bench runtime --no-default-features -- rgb/   # optional substring filters
python3 scripts/test-site.py                          # site, generated pages, installers, reference snapshot
python3 -m unittest discover -s scripts               # release, site and installer script tests
python3 scripts/site-reference.py --binary target/debug/capturefab   # after changing commands or result shapes
python3 scripts/test-media.py --binary target/debug/capturefab [--srt]  # needs MediaMTX on :18554/:18890
```

Use the simulator (`-c sim:0`, `sim:1`, `sim:line`, `sim:depth`, `sim:microscope`, `sim:thermal`, `sim:replay:PATH`) to exercise acquisition without hardware. Set `CAPTUREFAB_SESSION_DIR` and `CAPTUREFAB_CACHE_DIR` to isolated temp dirs when running the binary manually or in tests (tests set both for every binary they spawn; unit tests isolate themselves). The session dir must be private (mode 0700, as `mktemp -d` makes) or not exist yet; camera commands fail when its group or others have any access. Three directories (`ipc::runtime_dir`, `state_dir`, `cache_dir`): runtime holds session descriptors, locks and other files needed only while processes run (`$XDG_RUNTIME_DIR/capturefab`, else a lingering Linux login's `/run/user/<uid>/capturefab`, else `~/.cache/capturefab/sessions`); state holds presets (`setups/`), the saved sign-in index, background session logs (`sessions/NAME.log`) and the storage ledger (`$XDG_STATE_HOME/capturefab` on Linux, the runtime dir elsewhere); cache holds the extracted FFmpeg and encoder probes. `CAPTUREFAB_SESSION_DIR` overrides runtime and state together, `CAPTUREFAB_CACHE_DIR` the cache. `inspect` previews go to the private `<runtime>/inspect/` folder (0700, newest 64 files / 64 MiB).

FFmpeg: every build, in every profile, embeds an FFmpeg executable (zlib-compressed, extracted to the cache dir on first use): `CAPTUREFAB_FFMPEG_BINARY` when set at build time, otherwise `target/ffmpeg/<target>/bin/ffmpeg`, which `build.rs` builds with `scripts/build-ffmpeg.sh` when it is missing (a long build that downloads the pinned sources). `CAPTUREFAB_FFMPEG_LICENSE` supplies the notice `licenses` prints. At runtime `CAPTUREFAB_FFMPEG=/abs/path/ffmpeg` overrides the embedded copy; it does not skip the build-time FFmpeg. Media tests requiring FFmpeg are `#[ignore]`d. `scripts/build-ffmpeg.sh` builds the pinned, checksummed GPL FFmpeg; `scripts/release.py` packages releases. On macOS `build.rs` links `assets/Info.plist` into the executable's `__info_plist` section (camera and local network usage strings); keep its `CFBundleIdentifier`, which existing camera permissions are stored under.

## Architecture

**CLI.** `main.rs` calls `lib::run`, which dispatches the hidden `__worker` argv to `session::run_worker` and everything else to `cli::main` (parse, then `cli::run`, then print and exit). `src/cli.rs` holds global flags, dispatch and routing; `src/cli/` has one module per command family (`camera` for list/info/features/get/set/exec, `capture`, `record`, `inspect`, `auto`, `preset`, `jobs`, `session`, `serve`, `login`, `onvif`, `network`, `doctor`, `debug`, `schema`, and `storage`, the storage budget command hidden from help and the schema until lean storage replaces the ledger), plus `args` (camera selection, durations, times), `paths` (client-side path and template resolution: a path belongs to the caller even when a session does the work), `output` (the only place that prints results and errors) and `signals`. `controls.rs` parses feature names, banks (`TriggerMode[FrameStart]`) and the canonical controls (`exposure`, `gain`, `fps`, `roi`, `format`, `trigger`, `auto_exposure`, `auto_wb`). `naming.rs` renders output templates; `context.rs` holds capture tags.

**Process model.** Each camera runs in its own worker process (a re-exec of the same binary, overridable via `CAPTUREFAB_EXECUTABLE`). `session::SessionHandle` / `Coordinator` owns one `Process` per camera, sends `SessionCommand`s as newline-delimited JSON over the worker's stdin and reads replies from its stdout. Inside the worker, `engine::WorkerHandle` runs one thread that exclusively owns the `camera::Camera` (and its transport), recordings and scheduled capture jobs (`scheduling.rs`); `engine/history.rs` checkpoints values so a multi-value `set` or preset apply rolls back on failure, and `engine/saved.rs` reports saved files. Frames reach the coordinator through `shared_memory.rs` rings. Without a session, a command starts its own coordinator and workers in-process, so direct and session commands share one code path.

**Sessions and jobs.** `serve --name X` (foreground) and `session start` (a detached `serve`) run an `ipc::Server`: loopback TCP, token-authenticated, with a descriptor file in the private runtime dir. Clients send the same `SessionCommand`s through `ipc::call`. A camera command uses a session when `--session` names one, when a running session holds the camera, or when `--keep`/`--background` needs one (auto-started as `auto`); `--no-session` forces a worker of the command itself. Long operations are jobs (`<session>-<n>`), listed by `jobs`.

**Camera layers.** `types.rs` defines the `RegisterIo` and `Backend` traits (xml/start/next_frame/stop) and `types::Categorized`, the typed error category. Implementations live in `transport/` (`gige.rs` GVCP/GVSP, `usb.rs` U3V via `nusb` behind the `usb` feature, `simulator.rs` with `simulator/replay.rs`), while `media.rs` (+ `media/v4l2.rs`) provides FFmpeg-backed sources (host webcams, RTSP/SRT, files) and recording with encoder probing, including the Jetson GStreamer encoder. `genicam.rs` parses GenICam XML (incl. zipped) into a feature node map evaluated live against `RegisterIo`. `camera.rs` ties discovery, backend and GenICam together; `camera_profile.rs` classifies area scan, line scan, depth, microscope and thermal cameras from what they advertise. `onvif.rs` does WS-Discovery and SOAP profile resolution to an RTSP URI. `network.rs` and `engine/network_defaults.rs` diagnose GigE paths and apply bandwidth sharing. `test_cameras/` holds unauthenticated GigE and ONVIF fakes for unit tests only.

**Frames and measurements.** `frame.rs` converts pixel formats and encodes PNG/raw; `jpeg.rs` wraps libjpeg-turbo (`jpeg` feature). `inspect.rs` measures exposure, clipping and focus and renders the bounded preview; `auto.rs` runs the host auto-exposure loop. `tests/frame_golden.rs` pins their outputs against `tests/golden/frame.json`.

**Storage.** Captures and recordings go through `storage.rs`, never directly to disk: existing output files are never overwritten (publish without replace; `exists` otherwise), and `volumes.rs` refuses writes under an unmounted mount point. `storage.rs` also keeps a locked ledger in the state dir that counts every saved output that still exists against a global budget (default 10 GiB and 10,000 files, stop when full): reaching it is exit 6 `storage_full` even with free disk space. The hidden `capturefab storage status|configure` shows and changes it. `setup.rs` stores presets.

**Sign-ins.** `logins.rs`, `keychain.rs` (`keychain` feature) and `secret_input.rs` keep camera passwords in the OS credential store (service `capturefab-camera`), with a secret-free index in the state dir; the worker that opens the camera reads them. `scrub.rs` redacts secrets from every outgoing string.

## CLI contract

The CLI is a public, versioned automation contract (`capturefab schema`, `web/reference.html`, `web/agents.md`, `web/llms.txt`):
- `--json` prints exactly one single-line JSON document on stdout: the result on exit 0, `{"error":{"code","message","hint"?,"details"?}}` otherwise, also for usage errors. Results never contain a top-level `error` key; partial work of a failed command goes in `error.details`. Diagnostics go to stderr.
- The exit code is the category (0 ok, 1 failed, 2 usage, 3 unavailable, 4 timeout, 5 unsupported, 6 storage full, 130/143/129 interrupted); `error.code` is the specific reason. Both come from typed errors (`types::Categorized` plus reason), never from message text, and survive worker and session IPC hops.
- Keys are snake_case with units in names (`exposure_us`), no null values, RFC 3339 UTC times, bounded lists with `truncated`.
- Credentials in URLs must be redacted (`media::redact_url`, `scrub`) in output, logs and errors.

When changing commands or output shapes, run `python3 scripts/site-reference.py --binary target/debug/capturefab` (CI fails when `docs/cli-schema.json` differs from the binary), and keep `README.md`, `web/agents.md`, `web/llms.txt` and `web/docs.html` consistent; then `python3 scripts/docs-search.py`. Avoid publishing camera testing results or compatibility claims while the software is under rapid development.
