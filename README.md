# capturefab

A camera workbench with a native iced desktop and a scriptable CLI. Open `capturefab` to discover cameras, select a device, inspect its settings, and capture. The same executable runs headless and lets scripts or coding agents control an already visible GUI session.

Capturefab is MIT-licensed. Its GigE Vision, USB3 Vision and GenICam implementation is Rust; media decoding, host-driver camera access and encoding use a bundled, source-built FFmpeg executable. Release binaries require no separately installed Aravis, FFmpeg, libusb, vendor SDK or Python. Operating-system graphics and camera drivers remain necessary.

This is an initial implementation, not complete Aravis or vendor-SDK feature parity. The software is under rapid development; camera support and behavior may change between releases.

## Start here

Download the matching desktop or headless binary from [GitHub Releases](https://github.com/capturefab/capturefab/releases). The [project site](https://capturefab.com/) selects from published artifacts, with explicit architecture choices and SHA-256 checksums. Until a release is actually published, the site displays that state instead of fabricated download links.

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

Camera commands automatically use the only running GUI or `serve` session, so the workbench shows what an agent is doing. With no session, camera operations use a temporary worker. Use `--direct --camera ID` to request that explicitly; if several sessions are running, choose `--session NAME`. `--camera` can also come from `CAPTUREFAB_CAMERA`.

Use `--json` for versioned machine-readable results. Human-readable output stays simple; diagnostics go to stderr; JSON success and error envelopes go to stdout; exit status reports failure. `--help`, `--version`, native shell completions and binary stdout follow normal shell conventions. Existing output files are never overwritten. A single filename or `{frame}` template infers its image format from `.png`, `.jpg`/`.jpeg`, `.raw`, `.pgm` or `.ppm`; directory sequences default to PNG, and `--format` overrides inference.

```sh
capturefab discover --json
capturefab --direct --camera sim:0 capture --set Width=640 --set Height=480 -o - -f ppm > frame.ppm
capturefab completions fish > capturefab.fish
capturefab schema > automation-contract.json
capturefab doctor --json
```

`schema --json` uses the same envelope as other JSON commands; its contract is in `result`, including a command tree with each option, default and accepted value. Plain `schema` prints the contract directly. `--timeout-ms` bounds a frame or transport operation; `--request-timeout 5m` sets the client deadline for a session request. A timed-out request can still finish in the session, so inspect `status` before retrying a write.

Commands work from bash, zsh, fish, PowerShell, tmux, Ghostty, Warp, Claude Code, Codex, OpenCode and other tools that launch processes and read JSON. For repeatable multi-camera setup, use [JSON workflows](docs/cli-workflows.md) to avoid shell quoting and preserve an inspectable plan.

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

Use `serve --name bench` for a headless session. Desktop windows and browser tabs keep their own selected camera. `select ID` sets the session default for CLI calls that omit `--camera`; explicitly target a camera when collaborating. Inspection, configuration, capture and recording commands connect a missing `--camera` source to the persistent session. `start`, `stop`, `auto` and other persistent controls address an already connected camera. Session clients authenticate with a private local token. `capturefab sessions` lists available sessions; set `CAPTUREFAB_SESSION` to pin one without repeating `--session`. `--direct` overrides that environment default and conflicts only with an explicit `--session`.

People and AI agents can capture from the same cameras together. Commands are ordered per camera, and different cameras work independently. Overlapping scheduled jobs share acquisition. The desktop toolbar and browser's **Session activity** show who acted, on which camera, with queued/running/completed/failed states, errors, saved files and ongoing job progress. Activity updates wake clients immediately and preserve their camera selection, drafts and focus. Camera selection and disconnect do not erase the history. `status --json`, including targeted status during captures, exposes the same timeline in `result.activity` and its revision in `result.activity_revision`. Status polling does not create entries.

Journal strings and saved-path previews are capped: completed captures keep at most the first eight paths, while `file_count` gives the total and command results retain the complete file list. Targeted `--camera ID status` keeps all cameras and session activity; `target_camera` identifies the queried camera and `active_camera` retains the coordinator's selection for CLI defaults.

Give automation a recognizable name with `--actor NAME --actor-kind agent`; people use `--actor NAME` (human is the default). `CAPTUREFAB_ACTOR` and `CAPTUREFAB_ACTOR_KIND` supply these defaults. Names are display attribution, not authentication. Browser tabs have their own participant identity and editable name. HTTP automation can include an `actor` object and watch `/api/activity`; see [remote control](docs/remote-control.md).

```sh
capturefab --session bench --camera sim:0 --actor focus-agent --actor-kind agent capture -o agent.png
capturefab --session bench --camera sim:0 --actor Alice capture -o alice.png
capturefab --session bench activity --json
# Wait for changes using the revision returned above:
capturefab --session bench activity --after 12 --wait 20s --json
```

```sh
printf '%s\n' '{"op":"get","feature":"Width"}' | capturefab --session bench rpc --json
capturefab --session bench select sim:0
capturefab --session bench start
capturefab --session bench stop
```

## Repeatable workflows

Validate an ordered JSON plan before connecting to cameras, then apply it to the visible workbench or a headless session:

```sh
capturefab workflow validate examples/workflows/multi-camera-timelapse.json --json
capturefab workflow apply examples/workflows/multi-camera-timelapse.json --dry-run --json
capturefab --session bench workflow apply examples/workflows/multi-camera-timelapse.json --json
```

Workflow steps use the session RPC commands, can target different cameras, and reject unknown or duplicate fields. Files are limited to 1 MiB and 1–256 steps. Every step is validated before execution; live device capabilities and output availability are checked when each step runs. Execution stops at the first failure and retains earlier effects; JSON `error.details` identifies the failed step and completed results. Jobs and encoders remain in the session after the workflow command exits.

Relative capture and recording paths resolve beside the workflow file; stdin workflows use the calling directory. Local `file:relative.mkv` and `sim:replay:relative-folder` sources use that same base, including workflow camera targets. Bare existing filenames are recognized as file sources. Ordinary CLI/RPC sources and outputs use the calling terminal's directory. Network/device locators and names inside `--destination` keep their meaning. Workflows require file outputs for captures. See the [workflow guide and runnable examples](docs/cli-workflows.md) for timelapse, feeder scans and bounded recording.

## Remote browser control

Build the WebAssembly client with `./web-client/build.sh`, then run a headless server with `capturefab serve --name remote --listen 100.101.102.103:8787 --web-dir web-client/dist`. Open the server URL from another laptop on the same tailnet to discover cameras, edit settings, preview frames and set up captures or schedules. Anyone who can reach the listener can control captures; tailnet policy and the host firewall provide the access boundary. Capture files and jobs stay on the server; the local CLI can control the same session. See [remote setup and Tailscale HTTPS](docs/remote-control.md).

## Session analytics

The desktop toolbar's **Analytics** button and the browser's **Session analytics** panel show acquired frames, payload throughput, missed frames and camera contributions for the current session. DuckDB collects one-second samples in memory, retaining at most 24 hours and 250,000 rows; totals continue across camera reconnects. The HTTP listener exposes JSON readiness at `/health`, Prometheus counters and gauges at `/metrics`, and the Plotters chart at `/api/analytics.svg`. See [analytics and monitoring](docs/remote-control.md#analytics-and-monitoring) for the endpoint contract and retention limits.

## GigE network setup

Use **Scan network** in the camera sidebar to check GigE interfaces, camera reachability, link speed and MTU, then apply network defaults to connected cameras. The report distinguishes discovery from a working control connection and from observed stream stability. Slow Ethernet, Wi-Fi, routed paths and unknown link speeds stay visibly flagged. Live camera badges show waiting traffic, retries and lost frames.

The scan retests packet sizes, requests a larger receive buffer and shares a conservative bandwidth budget between connected cameras on each receive interface. It uses a camera throughput limiter or packet delay when supported, with a frame-rate fallback. Auto mode respects the scanned budget. Preview streams briefly pause and resume; recording, forwarding and scheduled captures must be stopped before their cameras can be tuned. Unsupported settings and host buffer limits appear in the report. Host IP addresses, routes and MTUs are not changed.

```sh
capturefab network-scan --json                       # inspect discovered cameras
capturefab --session bench network-scan --json       # also tune connected GigE cameras
```

RPC accepts `{"op":"network_scan","timeout_ms":1000}` and returns `network_scan`, also retained in session status. The per-camera `{"op":"optimize_network","bytes_per_second":100000000}` operation applies a supplied budget; prefer the session scan to account for cameras sharing an interface. Network defaults are camera configuration writes, persist for the connection, and are outside camera undo history. Packet probing and pacing use supported camera features; see [Basler's network parameter reference](https://docs.baslerweb.com/network-related-parameters) for packet-size and inter-packet-delay behavior.

## Camera types and controls

Capturefab detects area scan, line scan, depth, microscope, thermal and document scanner sources independently of their transport. The GUI shows the detected type and its evidence, then puts the relevant advertised controls first. Unknown cameras keep their ordinary feature list. Device access, choices, limits and selector-dependent settings remain authoritative; identifying a camera type does not add controls the device lacks.

| Type | Controls shown first | Preview |
| --- | --- | --- |
| Area scan | Image region, exposure, gain and frame timing | Complete image frames |
| Line scan | Line width, lines per block, line rate, triggers and encoders | Recent received rows; gaps mark skipped blocks |
| Depth | Components, output mode, depth range and spatial calibration | Scalar range colors with an automatic raw-value range |
| Microscope | Focus, objective/magnification, illumination and stage controls | Image preview with the focus region and sharpness tools |
| Thermal | Temperature output, range, emissivity and calibration | Scalar heatmap with an automatic raw-value range |
| Document scanner | Resolution, scan mode and page source | A page acquired on demand |

Line history and depth/thermal color maps are display aids. **Sensor pixels** switches back to the ordinary sensor preview. Line history contains only recently received display rows, may downsample wide or tall blocks, and is not a complete encoder-aligned scan. Captures save individual acquired blocks. Depth/thermal legends report raw sample values, not calibrated distances or temperatures; interpret measurements using the camera's component, unit, scale, offset and calibration features.

`discover --json`, `info --json` and connected identities in `status --json` include `CameraInfo.profile`: `kind`, `source`, `evidence` and `capabilities`. Kinds are `unknown`, `area_scan`, `line_scan`, `depth`, `microscope`, `thermal` and `document_scanner`. Discovery may have only identity hints; connection refines the profile from readable features. Detection prioritizes the scanner transport, explicit device class metadata, explicit line-scan geometry, specialty feature families, remaining declared scan geometry, model identity, and conventional frame dimensions/timing. A one-row ROI, high bit depth or vendor name alone does not identify a specialty camera.

Try the typed simulators without hardware. Each accepts an optional name, such as `sim:line:conveyor`; `sim:0` and arbitrary other names retain the area-scan pattern camera.

Use `sim:replay:PATH` to loop test videos or a single-level folder of image frames/videos. `capturefab simulate run --preset onvif --source PATH` hosts a virtual network camera for an NVR or computer vision pipeline; `simulate presets` lists GigE Vision, RTSP, ONVIF and vision presets. See [simulation and integration testing](docs/simulation.md) for setup and examples.

```sh
capturefab --camera sim:line capture --set Height=64 \
  --set AcquisitionLineRate=2000 --set ExposureTime=100 -o line-block.png
capturefab --camera sim:depth get PixelFormat Scan3dCoordinateScale \
  Scan3dCoordinateOffset Scan3dCoordinateUnit --json
capturefab --camera sim:depth capture --format raw -o depth.raw --json
capturefab --camera sim:microscope capture --set FocusPosition=50 \
  --set Magnification=4 --set IlluminationIntensity=70 -o micrograph.png
capturefab --camera sim:thermal get TemperatureLinearResolution TemperatureUnit --json
capturefab --camera sim:thermal capture --format raw -o thermal.raw --json
```

Raw capture preserves acquired sample bytes and precision. PNG, JPEG, PGM, PPM and forwarded video produce 8-bit visualizations; forwarding carries no measurement calibration or GUI color map. Save raw data, its dimensions and pixel format, and the device's calibration features for quantitative work. Depth support uses compatible single-component raster streams: multipart, GenDC, point-cloud payloads and devices accessible only through a vendor SDK remain unsupported. See [camera compatibility](docs/camera-compatibility.md) for transport limits.

## Document scanners

Document scanners use the existing capture commands, GUI, schedules, destinations and storage quotas. The `scanner` build feature is enabled by default. Linux uses an installed SANE `scanimage` executable and scanner backends (including network scanners exposed by those backends); Windows uses Windows Image Acquisition (WIA) through Windows PowerShell; macOS uses a bundled native ImageCaptureCore helper. Install a compatible scanner driver on the host. Physical scanner compatibility has not been validated.

```sh
capturefab scanners --json
# Use the full scanner ID returned by scanners or discover:
capturefab --camera 'scanner:sane:DEVICE' --timeout-ms 60000 capture -o document.png
capturefab --camera 'scanner:wia:ID' --timeout-ms 60000 capture -o document.png
capturefab --camera 'scanner:ica:UUID' --timeout-ms 60000 capture -o document.png
capturefab --camera 'scanner:sane:DEVICE' --timeout-ms 60000 capture \
  --set ScanResolution=300 --set ScanMode=Gray --set ScanSource=Feeder -n 5 -o pages
```

In the GUI, discover and connect a scanner, choose **Scan resolution**, **Scan mode** and **Scan source** in Features, then **Scan & save** in Capture. Scanner captures have a separate GUI timeout, initially 60 seconds; use `--timeout-ms 60000` for CLI scans. `ScanResolution` accepts 75–600 dpi (default 150), `ScanMode` is `Color`, `Gray` or `Lineart` (default `Color`), and `ScanSource` is `Default`, `Flatbed` or `Feeder` (default `Default`, keeping the driver's paper source). Drivers can support a subset of these settings; rejected settings fail without silently switching modes. SANE's explicit source names are `Flatbed` and `Automatic Document Feeder`; use `Default` for drivers with other source names. `CAPTUREFAB_SCANIMAGE` can point to a specific executable.

Connecting or starting acquisition does not scan. Each capture requests one page; `-n` requests successive pages, including feeder pages. Multiple pages save as separate image files in a directory or `{frame}` template. Scans appear in the existing preview. Samples normalize to 8-bit RGB/grayscale with a 128 MiB page limit, using the existing PNG, JPEG, PGM, PPM and raw formats. PDF assembly and OCR are not included. Scanner forwarding and camera auto exposure return unsupported errors. Timeout, stop or disconnect terminates acquisition. Inspect `doctor --json` for the compiled feature and runtime availability.

Scanner IDs and previously discovered scanner serials receive a 128 MiB frame ring per slot (cameras default to 16 MiB). Use full scanner IDs when opening scanners directly, or set `CAPTUREFAB_FRAME_BYTES` when connecting by an undiscovered serial. Each worker keeps three slots, and the existing 256 MiB maximum override remains available.

## Undo and redo

The desktop provides Undo and Redo throughout its editors and settings: **⌘Z / ⇧⌘Z** on macOS, **Ctrl+Z / Ctrl+Shift+Z** elsewhere, with **Ctrl+Y** also available for redo. The guide lists these bindings. Continuous edits are grouped into one change, and a new change clears redo.

Camera workers also keep history for reversible feature writes, auto/manual changes and streaming changes, shared by GUI, CLI and RPC clients. Each connected camera has its own history; `--camera` addresses that camera without changing the visible selection.

```sh
capturefab --session bench --camera sim:0 history
capturefab --session bench --camera sim:0 undo --json
capturefab --session bench --camera sim:0 redo --json
```

`status.history` and `cameras[].history` report the next undo/redo labels, or `null` when unavailable. History is bounded and lasts for the camera connection. Replay checks camera state and reports conflicts or failures without discarding the entry. Captures, uploads, recordings, schedules, connection changes, arbitrary camera commands and raw memory writes have external effects that camera history cannot reverse; stopping a stream while it has schedules or forwarding is also excluded.

Scanner settings are reversible. Scanner acquisition, including Start and Stop, may move or consume physical pages and cannot be undone or replayed.

## Auto exposure and frame rate

```sh
capturefab --camera 192.168.1.10 capture --auto -o tuned.png
capturefab --camera 192.168.1.10 forward --auto --balance 1 -o rtsp://localhost:8554/line
capturefab --session bench auto --balance 0.25
capturefab --session bench manual
```

Cameras start in manual mode; connecting never enables auto. Auto mode prefers the camera's own auto exposure, gain and white balance and tunes their limits; otherwise Capturefab adjusts exposure and gain itself. `--balance` runs from 0 (image quality: longer exposure, least gain) to 1 (frame rate: shorter exposure, more gain), initially 0.5; `quality`, `balanced` and `frame-rate` also work. It may raise the frame-rate limiter and GigE throughput limit when the link has headroom, and backs off on packet loss. `status` lists its recent decisions with their previous values. Setting a managed feature switches that camera to manual; `manual` turns the camera's auto functions off so current values hold, and `manual --revert` restores what auto mode changed. Settings written in auto mode stay on the camera after `manual`, like `set`; disconnecting, closing the session or the end of a direct `--camera` command restores them where possible. `capture --auto` waits briefly for exposure to settle before saving, and `serve --auto` enables auto mode for a headless session.

Generic auto exposure supports image cameras, including microscopes when suitable controls are advertised. Line scan, depth, thermal and document scanner sources return unsupported; configure their line timing or measurement controls directly.

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

Durations and timestamp fractions must resolve to whole milliseconds: `1.001s` is valid; `1.1ms` is rejected instead of rounded. Every frame in a schedule must fall within 10 years. A past timestamp starts immediately, and a zero RPC interval captures consecutive frames. Active jobs cannot reserve the same future output file on one camera. Capture paths support only the `{frame}` placeholder; use `forward` for network stream outputs.

## Bounded storage and retention

Every image capture and local recording goes through a shared, OS-locked ownership ledger. The default total budget is **10 GiB and 10,000 files**, across output directories and capture processes using the same session directory. Active recording reservations count toward that budget. A local video file also has a configurable maximum size, initially **512 MiB**.

```sh
capturefab --camera sim:0 capture -n 100 -o frames --max-space 2GiB --max-files 1000
capturefab --camera sim:0 capture --interval 1m -n 10000 -o history --max-space 1GiB --delete-oldest --max-age 7d
capturefab storage --json
capturefab storage configure --max-space 2GiB --max-files 1000 --on-full stop
```

`storage configure` persists a global ceiling across restarts; per-command limits can lower it. The default policy stops with an actionable `storage_full` error (exit 6) instead of silently deleting data. `--delete-oldest` explicitly permits retention to remove Capturefab-owned files. The ledger verifies file identity and SHA-256 before deleting: existing files, user replacements, altered captures and active recordings are protected. Age retention is enforced when allocating new captures. Missing files release their ledger usage; interrupted reservations are reconciled through crash-released OS locks. Ordinary disk-full and write errors stop capture and remove safely identifiable failed output.

`--max-age` requires whole seconds, at least `1s`, and a delete-oldest policy. Recording reservations must fit the requested storage budget; an oversized `--max-file-size` is rejected rather than reduced. `record`/`forward --duration 30s` works in temporary workers and persistent sessions: the CLI waits, then stops the encoder, including on Ctrl-C. Without a duration, persistent sessions return immediately and continue until `stop-forward`; direct commands stay in the foreground until Ctrl-C.

The budget covers capture/recording data, not unrelated files on the disk. Runtime frame rings, queues and extracted FFmpeg are separately bounded. Use the same `CAPTUREFAB_SESSION_DIR` for sessions that should share a storage budget. Changing it creates a separate ledger and budget.

## Save to external drives, folders or S3

Captures, schedules and local recordings can go to a saved **destination**: a folder (on an external, removable or network drive, or anywhere on disk) or an S3-compatible bucket (AWS S3, Cloudflare R2, Backblaze B2, MinIO, Wasabi). In the GUI, **Save to** in the Capture and Forward panels lists destinations and connected external drives (one click saves to a `Capturefab` folder on the drive); **Manage…** adds folders with the system folder picker and buckets with provider presets, then tests them.

```sh
capturefab volumes --json                                   # external, removable and network volumes
capturefab destination add-folder usb "/Volumes/CAMDISK/Capturefab"
capturefab destination add-s3 archive --bucket camera-archive --region us-east-1 \
  --prefix line-1/ --access-key-id AKIA... --secret-stdin < secret.txt
capturefab destination add-s3 nas --bucket frames --endpoint http://nas.local:9000 --path-style --profile lab
capturefab --camera sim:0 capture -n 10 -o run-1 --destination archive
capturefab --camera sim:0 record -o clip.mkv --duration 10 --destination usb
capturefab uploads --json && capturefab uploads retry
```

With a destination, `--output` is a name inside it (a file, a folder or a `{frame}` template). Every file is first written locally through the storage ledger, so budgets, retention and never-overwrite still apply. A folder on an external drive that is not mounted fails with `unavailable` (exit 3) rather than writing to the system disk. Bucket destinations stage files under the platform's data directory (`CAPTUREFAB_STAGING_DIR` overrides it) and a persistent queue uploads them with Signature Version 4 over HTTPS, trusting the operating system's certificates. Payloads are signed with their SHA-256 and writes are conditional, so an existing object is never replaced. Files over 64 MiB use multipart upload. Network and service errors are retried with backoff; refused credentials, a missing bucket or an existing key wait for `uploads retry`. After a verified upload the local copy is deleted, only if unchanged, unless the destination keeps local copies. The GUI and `serve` upload in the background; a direct CLI capture waits for its uploads (up to ten minutes) and reports them.

Secrets never go into Capturefab's files or commands. `--access-key-id` keeps the secret in the operating system's credential store (macOS Keychain, Windows Credential Manager, Secret Service on Linux), read from stdin with `--secret-stdin` or typed into the GUI. Headless hosts can use a profile from the AWS shared credentials file (`--profile`) or `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` in the uploading process (`--env`). `http://` endpoints are allowed for local object stores: requests are still signed and the secret is never sent, but data is not encrypted.

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

Forwarding uses a bounded latest-frame queue and probes an encoder before selecting it. Candidates, in order, are the vendor encoders (Apple VideoToolbox, NVIDIA NVENC including AV1, NVIDIA Jetson through its GStreamer `nvv4l2h264enc`/`nvv4l2h265enc`/`nvv4l2av1enc` elements, AMD AMF, Intel QSV, VAAPI), then cross-vendor Vulkan Video (`h264_vulkan`, `hevc_vulkan`, `av1_vulkan`), Windows Media Foundation and Linux V4L2, where the build and host drivers support them; software H.264/HEVC is the fallback. On Jetson the GStreamer encoder runs as a separate process and FFmpeg remuxes and delivers its stream. `--encoder` selects explicitly and `--hwaccel` controls decoding for new direct, GUI or `serve` workers; existing sessions retain their decoding setting. Hardware availability is established by a real probe, not chipset branding. Frames reach FFmpeg in their sensor format (gray, 10/12/16-bit gray, Bayer or packed RGB), handed from the camera worker to the writer thread without a copy, so color conversion runs in FFmpeg's SIMD scaler or the encoder hardware rather than on the camera thread; NVENC and AMF receive packed RGB for color sources and convert it on the GPU when their probe accepts that input. See [FFmpeg builds](docs/ffmpeg-build.md). `.mkv`, fragmented `.mp4`/`.mov`, `.ts` and suitable `.webm` recordings use a bounded Rust writer. Serve HLS through MediaMTX rather than local files outside the retention manager.

## Debugging and limits

`features`, `get`, `set`, `execute`, `xml`, `memory read`, `memory write` and `doctor` expose camera state and transport diagnostics. Raw register writes are advanced operations; settings apply sequentially and a later failure does not undo earlier successful settings. Camera feature access, bounds and availability are evaluated live.

CLI field validation runs before connection and before capture/forward `--set` or `--auto` changes. Malformed supplied field values return `usage` (exit 2); live camera, destination, storage and I/O failures retain their operation categories. RPC rejects unknown fields, invalid ranges and malformed commands with a description of the rejected input. A rejected value for an auto-managed feature leaves auto mode enabled. Explicit `CAPTUREFAB_MAX_CAMERAS` values must be integers from 1 to 64; `CAPTUREFAB_FRAME_BYTES` must be 4096–268435456 bytes and a multiple of 64. Invalid overrides fail before worker startup.

Supported image conversion includes Mono8, unpacked Mono10/12/16, RGB/BGR8 and Bayer8; large Bayer frames are demosaiced across CPU cores. JPEG stills (`--format jpeg`, quality 90, 4:2:0 or grayscale) use the fastest encoder on the host. On Apple silicon, color frames of 0.5 MP and up are encoded by the dedicated JPEG engine through VideoToolbox: Capturefab writes full-range BT.601 YCbCr 4:2:0 directly into the engine's IOSurface buffers, because the engine's own RGB conversion shifts colors; `CAPTUREFAB_VIDEOTOOLBOX_JPEG=0` disables it. With CUDA and nvJPEG installed, NVIDIA GPUs encode in a separate helper process with a per-frame deadline that falls back to libjpeg-turbo's SIMD encoder if the GPU fails or stalls. Frames are converted straight into memory shared with the helper, which page-locks it so the upload is one DMA, or, on integrated GPUs such as Jetson, lets nvJPEG read it in place without an upload. `CAPTUREFAB_NVJPEG=0` disables nvJPEG, `CAPTUREFAB_NVJPEG_PIN=0` keeps pageable copies, and `CAPTUREFAB_CUDA_LIB_DIR` names a non-standard CUDA library directory. On Linux, a VA-API driver with JPEG encoding (typically Intel's iHD) is used through a helper process that loads `libva.so.2` at run time, so no libva is linked; it writes the same tables libjpeg-turbo uses and converts frames straight into the GPU surface. Before reporting ready it encodes a test image and checks that libjpeg-turbo decodes it correctly, and it is not used otherwise. `CAPTUREFAB_VAAPI_DEVICE` selects a render node and `CAPTUREFAB_VAAPI_JPEG=0` disables it. Monochrome frames (unless the driver encodes gray), small frames and other hosts use libjpeg-turbo. `doctor` reports which JPEG encoders are available. The GUI renders through wgpu (Metal, Vulkan, Direct3D 12 or OpenGL), uploads sensor bytes and demosaics/converts them in a WGSL shader that matches the CPU conversion. Without a usable GPU it falls back to software rendering (`ICED_BACKEND=tiny-skia` forces it) with CPU conversion; `CAPTUREFAB_GPU_PREVIEW=0` keeps the GPU renderer but converts frames on the CPU. `CAPTUREFAB_APPEARANCE=light` or `dark` overrides the system appearance. The GUI remembers its appearance, panel settings and recently connected cameras and stream URLs in `gui.prefs.json` in the session directory; addresses containing credentials or secret query parameters are not saved. Icons are Phosphor Icons (MIT). The area-scan simulator offers Mono8, RGB8, Mono12 and BayerRG8; typed depth and thermal simulators supply calibrated 16-bit samples. PNG, raw, PGM and PPM are available. Unsupported packed/chunk/multipart/GenDC formats fail explicitly. GigE supports packet reordering, duplicate detection, bounded packet resend when the camera advertises support, and packet-size negotiation. Transport status reports packet size, receive buffer, resend and loss counters. HTTP-hosted GenICam XML is unsupported. USB3 Vision requires userspace access: USB permissions on Linux, an available IOKit interface on macOS, and WinUSB for the camera interfaces on Windows.

Single-component image conversion also accepts `Coord3D_C8`, `Coord3D_C16`, `Confidence8` and `Confidence16`. Scalar floating-point depth can use the GUI color or grayscale preview and raw capture; unsupported image export still fails explicitly. Coordinate previews honor advertised Scan3d invalid-data metadata for the C coordinate without guessing zero is invalid. Use the device's calibration features when processing raw measurements.

## Build, test and release

```sh
cargo build --release                              # Rust desktop; bundles FFmpeg when supplied
cargo build --release --no-default-features --features usb,scanner,jpeg,nvjpeg,vaapi,videotoolbox # headless
cargo test
cargo clippy --all-features --all-targets -- -D warnings
```

The `jpeg` feature, enabled in both builds above, compiles libjpeg-turbo from source and needs a C compiler and CMake, plus NASM on x86 for its SIMD code. Source development builds can use `CAPTUREFAB_FFMPEG=/absolute/path/to/ffmpeg`. Release builds embed `CAPTUREFAB_FFMPEG_BINARY`, or the automatic path `target/ffmpeg/<target>/bin/ffmpeg[.exe]`. `CAPTUREFAB_FFMPEG_LICENSE` embeds the matching notice. The source build script pins and checksums FFmpeg, SRT, OpenSSL and x264, and includes source/license/build metadata with distributed binaries.

The release tooling uses SemVer, a cross-platform build matrix, checksums, authenticated signing, actual rendered GUI and terminal screenshots, GitHub Releases and a static site on Cloudflare Workers with an Atom feed. Build tools are development dependencies; they are not runtime dependencies. Platform-specific signing credentials are optional configuration, not keys fabricated by this repository. See the release scripts and workflows for the exact configured build matrix.

Local automated verification includes public CLI calls, isolated camera subprocesses, shared-memory cleanup, concurrent acquisition, scheduling, retention, ONVIF HTTP/auth mocks and media decoding/encoding. Aravis is used only as an independent test simulator/reference, never linked into Capturefab.

## License

Capturefab is licensed under the [MIT License](LICENSE), SPDX `MIT`. Release binaries also bundle a separately executed FFmpeg payload that remains GPLv3 (with x264 GPLv2-or-later, SRT MPLv2 and OpenSSL Apache-2.0); it is never linked into Capturefab. Redistribute FFmpeg's complete corresponding source, build scripts and notices with binaries that include it. FFmpeg source archives and its exact build manifest accompany releases; never substitute an untraceable binary or enable nonfree FFmpeg components. See [FFmpeg compliance notes](docs/ffmpeg-build.md).
