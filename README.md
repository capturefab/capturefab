# capturefab

A camera workbench with a native egui desktop and a scriptable CLI. Open `capturefab` to discover cameras, select a device, inspect its settings, and capture. The same executable runs headless and lets scripts or coding agents control an already visible GUI session.

Capturefab is MIT-licensed. Its GigE Vision, USB3 Vision and GenICam implementation is Rust; media decoding, host-driver camera access and encoding use a bundled, source-built FFmpeg executable. Release binaries require no separately installed Aravis, FFmpeg, libusb, vendor SDK or Python. Operating-system graphics and camera drivers remain necessary.

This is an initial implementation, not complete Aravis or vendor-SDK feature parity. Real Basler GigE acquisition and Aravis simulator interoperability have been tested. USB3 Vision, ONVIF hardware and Windows hardware remain validation targets. See [hardware validation](docs/hardware-validation.md) and [camera compatibility](docs/camera-compatibility.md).

## Start here

Download the matching desktop or headless binary from [GitHub Releases](https://github.com/capturefab/capturefab/releases). The [static project site](https://capturefab.github.io/capturefab/) selects from published artifacts, with explicit architecture choices and SHA-256 checksums. Until a release is actually published, the site displays that state instead of fabricated download links.

```sh
capturefab                         # desktop; follows system light/dark mode
capturefab discover                # network, USB3 Vision and host cameras
capturefab native                  # USB webcams, built-in webcams, capture cards
capturefab compatibility GX85       # connection guidance for a Lumix GX85
capturefab --camera sim:0 capture -o first.png
capturefab --camera 192.168.1.10 features
capturefab --camera 192.168.1.10 set ExposureTime=5000 Gain=0
capturefab --camera 192.168.1.10 capture -n 10 -o frames
```

Use `--json` for versioned machine-readable results. Human-readable output stays simple; diagnostics go to stderr; JSON success and error envelopes go to stdout; exit status reports failure. `--help`, `--version`, native shell completions and binary stdout follow normal Unix conventions. Existing output files are never overwritten.

```sh
capturefab discover --json
capturefab --camera sim:0 capture --set Width=640 --set Height=480 -o - -f ppm > frame.ppm
capturefab completions fish > capturefab.fish
capturefab schema > automation-contract.json
capturefab doctor --json
```

Commands work from bash, zsh, fish, PowerShell, tmux, Ghostty, Warp, Claude Code, Codex, OpenCode and other tools that launch processes and read JSON. Terminal integrations do not require a particular emulator, agent framework, shell plugin or escape sequence.

## One session, many cameras

Each connected camera has its own capture process and three-slot shared-memory frame ring. Control uses bounded queues; live consumers prefer the newest frame. A blocked camera does not hold another camera's transport owner. The default camera limit is 16; `CAPTUREFAB_MAX_CAMERAS` accepts 1–64. The default ring payload is 16 MiB per slot; `CAPTUREFAB_FRAME_BYTES` configures up to 256 MiB. Memory and file use remain bounded.

```sh
capturefab gui --name bench
# In another terminal, or from a coding agent:
capturefab --session bench connect sim:0
capturefab --session bench connect sim:1
capturefab --session bench --camera sim:0 set ExposureTime=2500
capturefab --session bench --camera sim:1 capture -o second.png
capturefab --session bench status --json
```

Use `serve --name bench` for a headless session. `--camera` targets a connected camera without changing the visible selection; `select ID` changes the inspector selection. Session clients authenticate with a private local token. Commands and errors appear in the GUI activity view. `capturefab sessions` lists available sessions. Set `CAPTUREFAB_SESSION` to avoid repeating `--session`.

```sh
printf '%s\n' '{"op":"get","feature":"Width"}' | capturefab --session bench rpc --json
capturefab --session bench select sim:0
capturefab --session bench start
capturefab --session bench stop
```

## Auto exposure and frame rate

```sh
capturefab --camera 192.168.1.10 capture --auto -o tuned.png
capturefab --camera 192.168.1.10 forward --auto --balance 1 -o rtsp://localhost:8554/line
capturefab --session bench auto --balance 0.25
capturefab --session bench manual
```

Cameras start in manual mode; connecting never enables auto. Auto mode prefers the camera's own auto exposure, gain and white balance and tunes their limits; otherwise Capturefab adjusts exposure and gain itself. `--balance` runs from 0 (image quality: longer exposure, least gain) to 1 (frame rate: shorter exposure, more gain), initially 0.5; `quality`, `balanced` and `frame-rate` also work. It may raise the frame-rate limiter and GigE throughput limit when the link has headroom, and backs off on packet loss. `status` lists its recent decisions with their previous values. Setting a managed feature switches that camera to manual; `manual` turns the camera's auto functions off so current values hold, and `manual --revert` restores what auto mode changed. Settings written in auto mode stay on the camera after `manual`, like `set`; disconnecting, closing the session or the end of a direct `--camera` command restores them where possible. `capture --auto` waits briefly for exposure to settle before saving, and `serve --auto` enables auto mode for a headless session.

## Capture now, later, or at intervals

```sh
capturefab --camera sim:0 capture -o now.png
capturefab --camera sim:0 capture --delay 30s -o later.png
capturefab --camera sim:0 capture --interval 10s -n 360 -o timelapse
capturefab --session bench capture --at 2026-10-05T09:00:00-04:00 -n 100 --interval 1m -o dawn
capturefab --session bench jobs --json
capturefab --session bench cancel 1
```

Schedules return a job ID in a persistent session. A direct CLI waits for its job to finish. Jobs expose progress, the next capture time, errors and cancellation. Keep the GUI, headless session or direct command running; schedules do not survive process exit. Timestamps require an explicit timezone. Durations accept `ms`, `s`, `m`, `h`, `d`, or seconds without a suffix. Each frame has a bounded transport timeout. `stop` cancels pending capture jobs on its camera.

## Bounded storage and retention

Every image capture and local recording goes through a shared, OS-locked ownership ledger. The default total budget is **10 GiB and 10,000 files**, across output directories and capture processes using the same session directory. Active recording reservations count toward that budget. A local video file also has a configurable maximum size, initially **512 MiB**.

```sh
capturefab --camera sim:0 capture -n 100 -o frames --max-space 2GiB --max-files 1000
capturefab --camera sim:0 capture --interval 1m -n 10000 -o history --max-space 1GiB --delete-oldest --max-age 7d
capturefab storage --json
capturefab storage configure --max-space 2GiB --max-files 1000 --on-full stop
```

`storage configure` persists a global ceiling across restarts; per-command limits can lower it. The default policy stops with an actionable `storage_full` error (exit 6) instead of silently deleting data. `--delete-oldest` explicitly permits retention to remove Capturefab-owned files. The ledger verifies file identity and SHA-256 before deleting: existing files, user replacements, altered captures and active recordings are protected. Age retention is enforced when allocating new captures. Missing files release their ledger usage; interrupted reservations are reconciled through crash-released OS locks. Ordinary disk-full and write errors stop capture and remove safely identifiable failed output.

The budget covers capture/recording data, not unrelated files on the disk. Runtime frame rings, queues and extracted FFmpeg are separately bounded. Use the same `CAPTUREFAB_SESSION_DIR` for sessions that should share a storage budget. Changing it creates a separate ledger and budget.

## Webcams, streams and recording

```sh
capturefab native
capturefab --camera avfoundation:0 capture -o webcam.png          # macOS
capturefab --camera v4l2:/dev/video0 capture -o webcam.png         # Linux
capturefab --camera 'dshow:video=USB Camera' capture -o webcam.png  # Windows
capturefab --camera rtsp://camera/stream capture -n 5 -o snapshots
capturefab --camera video.mkv capture -n 5 -o decoded
capturefab --camera sim:0 record -o video.mkv --duration 30s --max-file-size 256MiB
capturefab --camera sim:0 forward -o rtsp://localhost:8554/camera --encoder auto
capturefab --camera sim:0 forward -o 'srt://localhost:8890?streamid=publish:camera' --encoder libx264
capturefab --session bench stop-forward
```

Host-driver discovery includes standard USB and built-in webcams, virtual camera drivers and compatible HDMI capture devices. CSI cameras work when the host exposes an accessible supported video driver; libcamera-only sensor pipelines and proprietary tether/USB protocols are not implemented. The GX85 can use its clean HDMI output through a capture card; see the compatibility guide for camera settings and the manufacturer source.

Native cameras accept `?size=1280x720&fps=30` locator options. Before acquisition, use `features` to inspect advertised `VideoMode` choices and set a supported mode, `Width`, `Height`, or `AcquisitionFrameRate`. The selected dimensions and frame rate depend on the host driver; V4L2 format listings do not provide frame-rate bounds. Stop acquisition before changing a mode.

ONVIF discovery and profile resolution support WS-Security, HTTP Basic and Digest authentication. HTTPS SOAP endpoints are currently unsupported. ONVIF media ultimately uses the selected RTSP URI.

```sh
capturefab onvif discover --json
capturefab onvif resolve http://192.168.1.10/onvif/device_service --username admin --json
# Supply the password with CAPTUREFAB_CAMERA_PASSWORD; credentials are redacted in UI/status.
capturefab --camera 'onvif://admin:password@192.168.1.10/onvif/device_service' capture -o ip-camera.png
```

Forwarding uses a bounded latest-frame queue and probes an encoder before selecting it. Apple VideoToolbox, NVIDIA NVENC, Intel/AMD VAAPI, Windows AMF/Media Foundation and Linux V4L2 encoders are candidates where the build and host drivers support them; software H.264 is the fallback. `--encoder` selects explicitly and `--hwaccel` controls FFmpeg decoding. Hardware availability is established by a real probe, not chipset branding. Frames reach FFmpeg in their sensor format (gray, 10/12/16-bit gray, Bayer or packed RGB), so color conversion runs in FFmpeg's SIMD scaler or the encoder hardware rather than on the camera thread; NVENC and AMF receive packed RGB for color sources and convert it on the GPU when their probe accepts that input. See [FFmpeg builds](docs/ffmpeg-build.md). `.mkv`, fragmented `.mp4`/`.mov`, `.ts` and suitable `.webm` recordings use a bounded Rust writer. Serve HLS through MediaMTX rather than local files outside the retention manager.

## Debugging and limits

`features`, `get`, `set`, `execute`, `xml`, `memory read`, `memory write` and `doctor` expose camera state and transport diagnostics. Raw register writes are advanced operations; settings apply sequentially and a later failure does not undo earlier successful settings. Camera feature access, bounds and availability are evaluated live.

Supported image conversion includes Mono8, unpacked Mono10/12/16, RGB/BGR8 and Bayer8. With the default OpenGL renderer the GUI uploads sensor bytes and demosaics/converts them in a shader that matches the CPU conversion; `--wgpu`, OpenGL contexts without integer textures, or `CAPTUREFAB_GPU_PREVIEW=0` use CPU conversion. The simulator offers Mono8, RGB8, Mono12 and BayerRG8. PNG, raw, PGM and PPM are available. Unsupported packed/chunk/multipart/GenDC formats fail explicitly. GigE supports packet reordering, duplicate detection, bounded packet resend when the camera advertises support, and packet-size negotiation. Transport status reports packet size, receive buffer, resend and loss counters. HTTP-hosted GenICam XML is unsupported. USB3 Vision requires userspace access: USB permissions on Linux, an available IOKit interface on macOS, and WinUSB for the camera interfaces on Windows.

## Build, test and release

```sh
cargo build --release                              # Rust desktop; bundles FFmpeg when supplied
cargo build --release --no-default-features --features usb # headless
cargo test
cargo clippy --all-features --all-targets -- -D warnings
```

Source development builds can use `CAPTUREFAB_FFMPEG=/absolute/path/to/ffmpeg`. Release builds embed `CAPTUREFAB_FFMPEG_BINARY`, or the automatic path `target/ffmpeg/<target>/bin/ffmpeg[.exe]`. `CAPTUREFAB_FFMPEG_LICENSE` embeds the matching notice. The source build script pins and checksums FFmpeg, SRT, OpenSSL and x264, and includes source/license/build metadata with distributed binaries.

The release tooling uses SemVer, a cross-platform build matrix, checksums, authenticated signing, actual rendered GUI and terminal screenshots, GitHub Releases and a static GitHub Pages site with an Atom feed. Build tools are development dependencies; they are not runtime dependencies. Platform-specific signing credentials are optional configuration, not keys fabricated by this repository. See the release scripts and workflows for the exact configured build matrix.

Local automated verification includes public CLI calls, isolated camera subprocesses, shared-memory cleanup, concurrent acquisition, scheduling, retention, ONVIF HTTP/auth mocks and media decoding/encoding. The [hardware validation report](docs/hardware-validation.md) separates these from actual camera tests. Aravis is used only as an independent test simulator/reference, never linked into Capturefab.

## License

Capturefab is licensed under the [MIT License](LICENSE), SPDX `MIT`. Release binaries also bundle a separately executed FFmpeg payload that remains GPLv3 (with x264 GPLv2-or-later, SRT MPLv2 and OpenSSL Apache-2.0); it is never linked into Capturefab. Redistribute FFmpeg's complete corresponding source, build scripts and notices with binaries that include it. FFmpeg source archives and its exact build manifest accompany releases; never substitute an untraceable binary or enable nonfree FFmpeg components. See [FFmpeg compliance notes](docs/ffmpeg-build.md).
