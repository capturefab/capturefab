# capturefab

Capture camera images, video, sensor values and PLC tags on the plant floor, and keep them ready for analysis. Capturefab is a native desktop app and a scriptable CLI for manufacturing and process engineers who work on OT networks. It connects GigE Vision, USB3 Vision, webcams, RTSP/ONVIF cameras and document scanners; reads Modbus TCP, MQTT, OPC UA and MTConnect sources; and catalogs every saved file and sample in one DuckDB dataset. An export gives Parquet or CSV files plus a `README.md` and `manifest.json` that a notebook, a BI tool or an AI assistant such as Claude, ChatGPT or Gemini can read without more explanation.

Everything runs on the local computer, with no cloud service. Files are never overwritten: captures get new names from templates such as `{camera}-{datetime}-{seq}.png`. Open `capturefab` to discover cameras, select a device, inspect its settings, and capture. The same executable runs headless and lets scripts or coding agents control an already visible GUI session.

Capturefab is MIT-licensed. Its GigE Vision, USB3 Vision, GenICam, Modbus TCP, MQTT, OPC UA and MTConnect clients are Rust; each signal source runs in its own helper process. Media decoding, host-driver camera access and encoding use a bundled, source-built FFmpeg executable. Release binaries require no separately installed Aravis, FFmpeg, libusb, vendor SDK or Python. Operating-system graphics and camera drivers remain necessary.

This is an initial implementation, not complete Aravis or vendor-SDK feature parity. The software is under rapid development; camera, PLC and server support and behavior may change between releases.

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
capturefab --camera sim:0 capture -o 'line1/{date}/{camera}-{seq}.png' --context part=A-100 --dataset plant.duckdb
capturefab signals read sim-signal://line1   # simulated line: temperature, pressure, counter, state
capturefab log sim-signal://line1 --duration 1m --dataset plant.duckdb
capturefab dataset export --file plant.duckdb -o plant-export
```

Camera commands automatically use the only running GUI or `serve` session, so the workbench shows what an agent is doing. With no session, camera operations use a temporary worker. Use `--direct --camera ID` to request that explicitly; if several sessions are running, choose `--session NAME`. `--camera` can also come from `CAPTUREFAB_CAMERA`.

Use `--json` for versioned machine-readable results. Human-readable output stays simple; diagnostics go to stderr; JSON success and error envelopes go to stdout; exit status reports failure. `--help`, `--version`, native shell completions and binary stdout follow normal shell conventions. Existing output files are never overwritten; see [name captures quickly](#name-captures-quickly). A single filename or template infers its image format from `.png`, `.jpg`/`.jpeg`, `.raw`, `.pgm` or `.ppm`; directory sequences default to PNG, and `--format` overrides inference.

```sh
capturefab discover --json
capturefab --direct --camera sim:0 capture --set Width=640 --set Height=480 -o - -f ppm > frame.ppm
capturefab completions fish > capturefab.fish
capturefab schema > automation-contract.json
capturefab doctor --json
```

`schema --json` uses the same envelope as other JSON commands; its contract is in `result`, including a command tree with each option, default and accepted value. Plain `schema` prints the contract directly. `--timeout-ms` bounds a frame or transport operation; `--request-timeout 5m` sets the client deadline for a session request. A timed-out request can still finish in the session, so inspect `status` before retrying a write.

Commands work from bash, zsh, fish, PowerShell, tmux, Ghostty, Warp, Claude Code, Codex, OpenCode and other tools that launch processes and read JSON. For repeatable multi-camera setup, use [JSON workflows](docs/cli-workflows.md) to avoid shell quoting and preserve an inspectable plan.

## Plant-floor data for analysis

A typical shift: open the desktop app (or `serve` on a headless line computer), connect the cameras, add the line's PLC or sensor sources, fill in the **Capture tags** (Line, Station, Part, Lot, Note), and capture or record. Each saved image and recording gets a row in the dataset, with its time, camera, path, SHA-256, destination and tags. Samples from every source go into the same file. The `captures_with_signals` view joins each capture with the latest value of every tag at that moment. At the end, `dataset export` writes an analysis bundle to a new folder.

```sh
capturefab serve --name line1 --dataset plant.duckdb
# In another terminal, or from a coding agent:
capturefab --session line1 signals start 'modbus-tcp://10.0.0.20/1/holding/0?type=f32&name=temp_c&unit=degC'
capturefab --session line1 --camera 192.168.1.10 capture -o 'line1/{date}/{camera}-{datetime}-{seq}.png' \
  --context line=L1 --context part=A-100
capturefab --session line1 dataset note "changed nozzle" --context line=L1
capturefab --session line1 dataset query "SELECT path, tag, value_double FROM captures_with_signals LIMIT 10"
capturefab --session line1 dataset export -o exports/line1-shift-b
```

See the [plant-floor guide](docs/plant-floor.md) for a step-by-step task with simulators, and the sections below for [naming](#name-captures-quickly), [signals](#sensors-and-plcs), [datasets](#datasets-and-analysis-exports) and [FTP, SFTP and RustFS destinations](#save-to-drives-folders-s3-ftp-or-sftp).

## One session, many cameras

Each connected camera has its own capture process and three-slot shared-memory frame ring. Control uses bounded queues; live consumers prefer the newest frame. A blocked camera does not hold another camera's transport owner. The default camera limit is 16; `CAPTUREFAB_MAX_CAMERAS` accepts 1–64. The default ring payload is 16 MiB per slot; `CAPTUREFAB_FRAME_BYTES` configures up to 256 MiB. Larger frames preview downscaled, while saved files keep full resolution. Memory and file use remain bounded.

On Linux and macOS rings are anonymous shared memory, so streaming writes no frames to disk and a crash leaves nothing behind; Windows keeps them as files in the session directory, which the next session removes after a crash. `doctor --json` reports `ring_backing` (`memfd`, `posix-shm` or `file`). A worker copies frames into its ring only while a viewer reads it, plus one frame per second and the newest frame when a stream stops or waits for a trigger, and always the frames a capture takes. In `status --json`, `lost` counts frames lost on the camera link or in the worker, and `preview_skipped` counts preview frames a viewer missed because a newer one was ready; captures and recordings still receive those. `dropped` keeps its meaning (worker receive errors plus `preview_skipped`), so a session with no viewer no longer reports skipped previews as drops.

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

Raw `logs` retain `time`, `level` and `message`. New entries also include optional `timestamp: {unix_ms, utc_offset_seconds}`: signed Unix milliseconds identify the absolute event, and the offset records the producer clock's seconds east of UTC. The legacy `time` string has no date/zone and must not be parsed as an instant. Old entries omit metadata; malformed optional metadata leaves the diagnostic readable. Desktop and browser displays convert valid instants to the viewer's local zone, and copied desktop logs include a full date and numeric offset. The version-1 JSON envelope is unchanged.

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

## Automatic capture and review

A capture session is a `.duckdb` file and a `.frames` folder beside it, bound to a camera. Create or resume a session in the desktop capture controls, take a few examples, then enable **Auto capture**. User-initiated captures teach the session what matters. Review suggested frames with **Keep** or **Discard**; every decision updates the session model. Pending suggestions do not train themselves. The review card supports quick decisions while live acquisition continues.

The CLI exposes the same controls through a running GUI or headless session:

```sh
capturefab --session bench connect sim:0
capturefab --session bench --camera sim:0 capture-session new inspection.duckdb
capturefab --session bench --camera sim:0 capture-session teach
capturefab --session bench --camera sim:0 capture-session auto --sensitivity 0.5 --cooldown-ms 2000
capturefab --session bench --camera sim:0 capture-session review --json
capturefab --session bench --camera sim:0 capture-session keep 1
capturefab --session bench --camera sim:0 capture-session discard 2
capturefab --session bench --camera sim:0 capture-session auto --pause
capturefab --session bench --camera sim:0 capture-session close
# Reconnect the same camera, then restore its examples, feedback and review queue:
capturefab --session bench --camera sim:0 capture-session resume inspection.duckdb
```

Use the IDs returned by `review`; `--id ID` retrieves a particular pending suggestion. `--json` includes a base64 RGBA preview. `status --json` exposes `capture_session` for each camera. Capture-session controls require a running session and an already connected camera. Auto capture starts live acquisition and is independent of automatic exposure. `auto --pause` retains learned examples; each `auto` command defaults sensitivity and cooldown to 0.5 and 2000 ms unless supplied.

The database stores descriptors, settings, feedback and pending reviews. Each sensor frame is a raw file in the folder (`inspection.frames/` for `inspection.duckdb`), saved through the storage budget like any other capture. To copy or move a session, close it, then copy the `.duckdb` file and its `.frames` folder together; resuming a database whose folder is missing fails, so nothing is dropped by mistake (create an empty folder to resume without those frames). **Keep** only relabels a capture. **Discard** deletes its frame file, if it is unchanged since Capturefab saved it, and retains the negative training descriptor and decision. Automatic capture pauses suggestions at 200 pending reviews or a 2 GiB stored-pixel limit; review or begin another session to continue. When the storage budget refuses a frame, suggestions stop for a minute and then try again, or at once after a discard or an `auto` command; free space or raise the budget with `storage configure`, since every session shares it. Suggestions are saved in the background, so live acquisition never waits for the disk; when the previous frame is still being saved, or storage is briefly busy, a suggestion is skipped and the session status shows a note for a minute. With delete-oldest retention, old session frames are deleted like other captures, and a pending suggestion whose frame file is gone, or changed after saving, leaves the review queue. Sessions created by earlier versions keep their frames inside the database and still open, review and discard; new frames go to the folder. Reopening restores saved capture settings and restarts acquisition if auto capture was enabled; reconnect the same camera first.

The learner scores frames with nearest-example preference and background novelty. How frames are compared is fixed when a session is created:

- **DINOv2** (default): the CLS embedding of Meta's DINOv2 ViT-S/14, bundled inside Capturefab, so there is nothing to download or install. It runs on the GPU when one is available (Metal on macOS, Vulkan or DX12 elsewhere) and samples the camera at up to 30 frames per second. Without a usable GPU it runs on the CPU and rests after each frame, so embedding averages at most half of one core (a fifth in efficiency mode); on a small computer that can mean one frame every few seconds. It understands objects and scenes far better than the native descriptor and ignores exposure drift, but as a whole-image embedding it can still miss objects that fill only a small part of the frame.
- **Native** (`--encoder native`): small spatial color/edge descriptors sampled at up to four frames per second; limited semantic understanding and sensitive to lighting changes.

`status --json` shows where embeddings run (`capture_session.accelerator`, for example `GPU: Apple M4 Max (Metal)`) and the achieved `samples_per_second`; `doctor --json` reports the bundled model and the GPU it would use. The GPU runs in a separate helper process with a deadline, so a driver fault or hang moves the session to the CPU instead of stopping the camera; `CAPTUREFAB_DINOV2_GPU=0` keeps it on the CPU. `auto` still chooses DINOv2 on a computer without a GPU: whether a GPU works is only known once its helper runs, while a session's encoder never changes. For frequent sampling on a CPU-only small computer, create the session with `--encoder native`, which samples four frames per second with a small fraction of a core. A session records a fingerprint of its model and refuses to resume in a build that bundles a different one. The DINOv2 weights are Apache-2.0; their license is included in `THIRD-PARTY-LICENSES.txt`. The [model research and evaluation plan](docs/auto-capture-models.md) explains the choice.

## Repeatable workflows

Validate an ordered JSON plan before connecting to cameras, then apply it to the visible workbench or a headless session:

```sh
capturefab workflow validate examples/workflows/multi-camera-timelapse.json --json
capturefab workflow apply examples/workflows/multi-camera-timelapse.json --dry-run --json
capturefab --session bench workflow apply examples/workflows/multi-camera-timelapse.json --json
```

Workflow steps use the session RPC commands, can target different cameras, and reject unknown or duplicate fields. Files are limited to 1 MiB and 1–256 steps. Every step is validated before execution; live device capabilities and output availability are checked when each step runs. Execution stops at the first failure and retains earlier effects; JSON `error.details` identifies the failed step and completed results. Jobs and encoders remain in the session after the workflow command exits.

Relative capture and recording paths resolve beside the workflow file; stdin workflows use the calling directory. Local `file:relative.mkv` and `sim:replay:relative-folder` sources use that same base, including workflow camera targets. Bare existing filenames are recognized as file sources. Ordinary CLI/RPC sources and outputs use the calling terminal's directory. Network/device locators and names inside `--destination` keep their meaning. Workflows require file outputs for captures. Workflow steps can also open a dataset (`dataset_open`), start signal sources (`signal_start`), add notes and export; [plant-line.json](examples/workflows/plant-line.json) is an example. See the [workflow guide and runnable examples](docs/cli-workflows.md) for timelapse, feeder scans and bounded recording.

## Remote browser control

Build the WebAssembly client with `./web-client/build.sh`, then run a headless server with `capturefab serve --name remote --listen 100.101.102.103:8787 --web-dir web-client/dist`. Open the server URL from another laptop on the same tailnet to discover cameras, edit settings, preview frames and set up captures or schedules. Anyone who can reach the listener can control captures; tailnet policy and the host firewall provide the access boundary. Capture files and jobs stay on the server; the local CLI can control the same session. See [remote setup and Tailscale HTTPS](docs/remote-control.md).

## Session analytics

The desktop toolbar's **Analytics** button and the browser's **Capture insights** section show acquired frames, payload throughput, missed frames and camera contributions for the current session. DuckDB collects one-second samples in memory, retaining at most 24 hours and 250,000 rows; totals continue across camera reconnects. The HTTP listener exposes JSON readiness at `/health`, Prometheus counters and gauges at `/metrics`, the Plotters chart at `/api/analytics.svg`, and sensor and PLC sources with their latest values at `/api/signals`. See [analytics and monitoring](docs/remote-control.md#analytics-and-monitoring) for the endpoint contract and retention limits.

The desktop also collects numeric video measurements from short prompts using OpenAI (ChatGPT), Gemini or Claude. Store API keys in the OS credential store under **AI provider API keys** in **Settings**, or launch with `OPENAI_API_KEY`, `GEMINI_API_KEY` / `GOOGLE_API_KEY`, or `ANTHROPIC_API_KEY`. In **Analytics**, choose a model, add prompts such as “count the number of tubes” or “estimate the length of ribbon in meters,” and explicitly start collection on a live camera. Structured results feed DuckDB history and per-measurement charts. See [prompt-driven video analytics](docs/video-analytics.md) for setup, sampling and retention.

## GigE network setup

Once a GigE camera is found, connected or tried, **Scan network** appears under the camera list; before that, use **Check network** in the Quick guide's GigE note. It checks GigE interfaces, camera reachability, link speed and MTU, then applies network defaults to connected cameras, and **Report** reopens the last results. The report distinguishes discovery from a working control connection and from observed stream stability. Slow Ethernet, Wi-Fi, routed paths and unknown link speeds stay visibly flagged. Live camera badges show waiting traffic, retries and lost frames.

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

Connecting or starting acquisition does not scan. Each capture requests one page; `-n` requests successive pages, including feeder pages. Multiple pages save as separate image files in a directory or a template with `{frame}` or `{seq}`. Scans appear in the existing preview. Samples normalize to 8-bit RGB/grayscale with a 128 MiB page limit, using the existing PNG, JPEG, PGM, PPM and raw formats. PDF assembly and OCR are not included. Scanner forwarding and camera auto exposure return unsupported errors. Timeout, stop or disconnect terminates acquisition. Inspect `doctor --json` for the compiled feature and runtime availability.

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

Durations and timestamp fractions must resolve to whole milliseconds: `1.001s` is valid; `1.1ms` is rejected instead of rounded. Every frame in a schedule must fall within 10 years. A past timestamp starts immediately, and a zero RPC interval captures consecutive frames. Two active jobs on one camera cannot reserve the same static output (a fixed filename, or a `{frame}`-only template, without `--on-exists number`); directories, `{seq}` templates and numbered outputs can be shared. Capture paths do not accept network URLs; use `forward` for stream outputs.

## Name captures quickly

Capture and local recording outputs are a file, a directory or a template. A template picks a new name for each file, so the same command or button can run again and again:

| Placeholder | Value |
| --- | --- |
| `{camera}` | `model-serial`, limited to `A-Z a-z 0-9 . _ -` and 64 bytes; never the address |
| `{date}` / `{time}` / `{datetime}` | `YYYY-MM-DD` / `HHMMSS` / `YYYYMMDD-HHMMSS`, local time when the frame is requested |
| `{seq}` | Next free number for that folder and pattern, zero-padded to 4 or more digits |
| `{frame}` | 1-based index within a multi-frame request, 6 digits |

```sh
capturefab --camera sim:0 capture -o 'line1/{date}/{camera}-{datetime}-{seq}.png'
capturefab --camera sim:0 capture -n 5 -o 'burst/{camera}-{seq}-{frame}.png'
capturefab --camera sim:0 capture -o shot.png --on-exists number    # shot-0001.png, shot-0002.png, ...
capturefab --camera sim:0 record -o 'video/{camera}-{seq}.mp4' --duration 30s --context line=L1
capturefab --camera sim:0 capture -o 'qa/{camera}-{seq}.png' --context line=L1 --context part=A-100 --json
```

`{seq}` may appear once, only in the file name, and never directly before a digit or `{date}`, `{time}`, `{datetime}` or `{frame}`. Any other `{name}` is a usage error (exit 2). With `-n` above 1, a template needs `{frame}` or `{seq}`, or `--on-exists number`. `--on-exists fail` (the CLI and RPC default) refuses an existing name; `number` writes `stem-0001.ext`, `stem-0002.ext` and so on. A directory output continues `frame-NNNNNN` after the highest existing number, so a rerun into the same folder adds files. If no free name is found after 64 attempts, the capture fails with exit 1.

`--context KEY=VALUE` tags captures and recordings; keys match `^[a-z][a-z0-9_]{0,31}$`, with at most 16 entries of 256 bytes each. The JSON capture result is `{files, frames:[{id,width,height,pixel_format,timestamp_ns,bytes,wall_ms,file}], count, context?, destination?, queued_uploads?, queued?}`; `wall_ms` is UTC milliseconds. `record` returns `{forwarding, since_ms, path}` with the resolved file, and `status` shows `forwarding_since_ms` while it records.

The desktop app saves to `~/Capturefab/{date}/{camera}-{datetime}-{seq}.png` by default, records to the same pattern with `.mp4`, and always numbers names that exist. **Next file** in the Capture panel shows the next file's path from the chosen folder. The toolbar **Record** button and Cmd/Ctrl-Shift-R start and stop a local recording, with an elapsed-time pill; the File menu has the same action. **Show in Finder** (Explorer, or the folder on Linux) opens the last capture or recording. The overview has **Capture all** and **Record all**; each camera writes its own file. **Capture tags** (Line, Station, Part, Lot, Note) are sent with every capture and recording as the context keys `line`, `station`, `part`, `lot` and `note`. The setting **Start the live view when a camera connects** is on by default and never starts a document scanner.

## Sensors and PLCs

Signal sources are URIs. Each source runs in a separate helper process, restarts at most five times after a failure, and never shares a process with a camera.

| Protocol | URI |
| --- | --- |
| Modbus TCP | `modbus-tcp://HOST[:502]/UNIT/coil\|discrete\|input\|holding/ADDRESS[?count=N&type=u16\|i16\|u32\|i32\|f32\|f64\|bool&order=abcd\|cdab\|badc\|dcba&scale=F&offset=F&name=TAG&unit=UNIT]` |
| MQTT | `mqtt[s]://[USER[:PASS]@]BROKER[:1883\|8883]/TOPIC_FILTER[?client_id=ID&qos=0\|1]` |
| OPC UA | `opc.tcp://[USER[:PASS]@]HOST[:4840][/PATH]?node=NODE_ID[&node=...][&root=NODE_ID][&security=none]` |
| MTConnect | `mtconnect[s]://HOST[:5000\|443][/DEVICE]` (polls `/current`) |
| Simulator | `sim-signal://LINE[?tags=1..64]` |

```sh
capturefab signals read sim-signal://line1
capturefab signals read 'modbus-tcp://10.0.0.20/1/holding/0?type=f32&name=temp_c&unit=degC' --json
capturefab signals browse opc.tcp://10.0.0.30:4840 --duration 5s
capturefab signals browse 'mqtt://broker.local/plant/#' --duration 10s --limit 200
capturefab log sim-signal://line1 'mtconnect://10.0.0.40:5000/Mill1' --interval 1s --duration 10m \
  --dataset plant.duckdb --csv line1.csv
capturefab --session line1 signals start 'opc.tcp://10.0.0.30:4840?node=ns=2;s=Line1.Temp' --interval 500ms
capturefab --session line1 signals --json
capturefab --session line1 signals stop all
```

Modbus addresses are 0-based protocol addresses (Modicon `40001` is `holding/0`); a source reads at most 125 registers or 2000 bits, and `value = raw*scale+offset`. MQTT accepts numbers, `true`/`false`, JSON objects (flattened to `topic/field`, depth 4, 256 leaves; a `ts`, `timestamp` or `time` field in Unix seconds, milliseconds or RFC 3339 becomes `source_ts_ms`) and text; binary payloads become `hex:...` text, with one sample per tag per interval. OPC UA uses SecurityPolicy None only, so a password travels unencrypted and the helper prints a warning; browse starts at `i=85` and goes four levels deep. MTConnect tags are `DEVICE/DATA_ITEM_ID`, and `UNAVAILABLE` has quality `bad`. Percent-encode `@` in topics and node IDs, and `/ ? # @` in credentials. Credentials are redacted in every output; source IDs are `signal:<16 hex>` from the redacted URI.

`signals read` uses the global `--timeout-ms`; `browse` takes `--duration` (100 ms to 60 s) and `--limit` (1 to 5000) and is unsupported for Modbus (exit 5). `signals start` (interval 100 ms to 1 h, optional repeatable `--tag`) and `signals stop ID|all` need a running GUI or `serve` session. `log` takes 1 to 16 URIs, runs until `--duration` or Ctrl-C, and needs `--csv` (a new file) or `--dataset`. A session holds at most 16 sources and 4096 tags per source. `ts_ms` is the host clock (UTC milliseconds) when a value was read; `source_ts_ms` is the device clock. Without a dataset or CSV, samples are kept only as latest values and `signals` reports `persisting:false`. Dropped samples are always counted. Exit codes: 2 for a bad URI, 3 for a refused connection or unknown source ID, 4 for a timeout, 5 for a protocol not compiled into this build.

In the desktop app, **Live signals** in the Capture panel has an **Add data source** field, recent URIs (only those without credentials are saved) and per-tag values with sparklines. The sidebar lists **Data sources** with a stop button. `GET /api/signals?history=1` returns the same status over HTTP. `doctor --json` reports the compiled protocols in `signals`.

## Datasets and analysis exports

A dataset is one `.duckdb` file owned by one session. It catalogs every saved capture and finished local recording, every signal tag and sample, and notes. `--dataset FILE.duckdb` (or `CAPTUREFAB_DATASET`) on `capture`, `record`, `serve`, `gui` and `log` creates or opens it; a session already recording to a different file refuses with an error.

```sh
capturefab --session line1 dataset open plant.duckdb --max-bytes 8GiB   # --new or --existing to be strict
capturefab --session line1 dataset status
capturefab --session line1 dataset note "shift B start" --context line=L1
capturefab --session line1 dataset query "SELECT camera_id, count(*) FROM captures GROUP BY 1"
capturefab --session line1 dataset export -o exports/shift-b --format parquet,csv
capturefab --session line1 dataset close
# A file no session holds:
capturefab dataset status --file plant.duckdb
capturefab dataset query --file plant.duckdb "SELECT * FROM captures_with_signals LIMIT 20" --json
capturefab dataset export --file plant.duckdb -o plant-export --since 2026-10-17T06:00:00-04:00
```

Tables are `captures`, `recordings`, `signals`, `samples`, `events` and the view `captures_with_signals` (each capture with the latest sample of every tag at or before its `wall_ms`, plus `sample_age_ms`). Every `*_ms` column is UTC Unix milliseconds from the host clock; `context` is JSON text. A queued upload adds its `upload_id`, `object_key` and `remote_url` to the capture row.

`dataset query` runs one read-only `SELECT` or `WITH` statement that cannot reach other files. It returns `{columns, rows, row_count, truncated}`: `--limit` 1 to 10,000 (default 1000), at most 8 MiB of JSON, a 30 s timeout, SQL up to 64 KiB. Large integers and decimals come back as strings. Without `--file`, `status`, `query` and `export` need a session; `--file` on a file a running session holds fails with exit 3 and says to reach it with `--session NAME`; `capturefab sessions` lists the running sessions.

`dataset export` writes into a new folder (an existing one is refused with exit 1) through the storage budget, which it checks before writing anything. It writes each table in Parquet and CSV by default (`--format csv,parquet,json,jsonl`, `--tables`, `--since`, `--until`), plus `captures_wide` with one column per tag (1 to 256 tags), a `manifest.json` (columns, types, descriptions, SHA-256 of each file, cameras, tag statistics) and a `README.md` with the time base, loading code for pandas, DuckDB and polars, and a short summary for AI tools. Images and videos stay at the paths in the tables.

The dataset file plus its write-ahead log is limited to 8 GiB by default. Writes stop with `Storage full` (exit 6) below 512 MiB of free disk space. In the desktop app, the **Dataset** section of the Capture panel has **New…**, **Open…**, **Export…**, **Show in Finder** and **Close**. With **Catalog captures automatically** (on by default), the app creates `~/Capturefab/capturefab.duckdb` on the first saved file or signal sample and reopens the chosen dataset at the next start.

Session RPC and workflow files use the same operations: `dataset_open {path, mode: auto|create|open, max_bytes?}`, `dataset_close`, `dataset_status`, `dataset_note {text, context?, camera?}`, `dataset_query {sql, limit, timeout_ms}`, `dataset_export {output, formats?, tables?, since_ms?, until_ms?}`, and for signals `signals`, `signal_read {uri, timeout_ms}`, `signal_browse {uri, duration_ms, limit}`, `signal_start {uri, interval_ms, tags?}` and `signal_stop {id}`. RPC paths must be absolute; `schema --json` has an example of each in `result.rpc.commands`, and its `result.dataset` and `result.signals` sections list the tables, URI forms, limits and exit codes.

## Bounded storage and retention

Every image capture and local recording goes through a shared, OS-locked ownership ledger. The default total budget is **10 GiB and 10,000 files**, across output directories and capture processes using the same session directory. Active recording reservations count toward that budget. A local video file also has a configurable maximum size, initially **512 MiB**.

```sh
capturefab --camera sim:0 capture -n 100 -o frames --max-space 2GiB --max-files 1000
capturefab --camera sim:0 capture --interval 1m -n 10000 -o history --max-space 1GiB --delete-oldest --max-age 7d
capturefab storage --json
capturefab storage configure --max-space 2GiB --max-files 1000 --on-full stop
```

`storage configure` persists a global ceiling across restarts; per-command limits can lower it. The default policy stops with an actionable `storage_full` error (exit 6) instead of silently deleting data. `--delete-oldest` explicitly permits retention to remove Capturefab-owned files. The ledger verifies file identity and SHA-256 before deleting: existing files, user replacements, altered captures and active recordings are protected. Age retention is enforced when allocating new captures, at most 32 expired files per save while the budget has room; protected files past the age limit never block a save that fits. Missing files release their ledger usage; interrupted reservations are reconciled through crash-released OS locks. Ordinary disk-full and write errors stop capture and remove safely identifiable failed output.

Every save must also leave 512 MiB free on the disk it writes to, the same floor datasets keep, counting what active recordings may still write. A recording reserves its maximum file size, so on a nearly full card lower `--max-file-size`. With `--delete-oldest` the oldest Capturefab files on that disk are removed to keep the floor; otherwise the save fails with `storage_full` (exit 6). If removing them does not free space, for example while a snapshot holds it, no more are removed for the floor until a save on that disk fits again. Windows cannot tell which drive a tracked file is on, so there nothing is removed for the floor. `storage --json` reports `free_bytes` for the disk of the default capture folder, `~/Capturefab`.

`--max-age` requires whole seconds, at least `1s`, and a delete-oldest policy. Recording reservations must fit the requested storage budget; an oversized `--max-file-size` is rejected rather than reduced. `record`/`forward --duration 30s` works in temporary workers and persistent sessions: the CLI waits, then stops the encoder, including on Ctrl-C. Without a duration, persistent sessions return immediately and continue until `stop-forward`; direct commands stay in the foreground until Ctrl-C.

The budget covers capture/recording data, not unrelated files on the disk. Runtime frame rings, queues and extracted FFmpeg are separately bounded: release builds extract their FFmpeg once per build into the user cache (`~/Library/Caches/capturefab` on macOS, `$XDG_CACHE_HOME/capturefab` or `~/.cache/capturefab` elsewhere, `%LOCALAPPDATA%\capturefab\cache` on Windows) and remove copies of older builds that no process is using. Use the same `CAPTUREFAB_SESSION_DIR` for sessions that should share a storage budget. Changing it creates a separate ledger and budget. The ledger is `storage.json` plus an append-only `storage.journal` in that directory, so a save writes about a kilobyte of bookkeeping however many files are kept; older Capturefab versions refuse this ledger as unsupported (exit 5) rather than misread it.

## Save to drives, folders, S3, FTP or SFTP

Captures, schedules and local recordings can go to a saved **destination**: a folder (on an external, removable or network drive, or anywhere on disk), an S3-compatible bucket (AWS S3, Cloudflare R2, Backblaze B2, MinIO, RustFS, Wasabi), an FTP or FTPS server, or an SFTP server. In the GUI, **Save to** in the Capture and Video panels lists destinations and connected external drives (one click saves to a `Capturefab` folder on the drive); **Manage…** adds folders with the system folder picker and buckets with provider presets (**MinIO, RustFS or self-hosted** for a server on the plant network), then tests them. FTP and SFTP servers are added and changed with the CLI; the GUI lists them and uses them.

```sh
capturefab volumes --json                                   # external, removable and network volumes
capturefab destination add-folder usb "/Volumes/CAMDISK/Capturefab"
capturefab destination add-s3 archive --bucket camera-archive --region us-east-1 \
  --prefix line-1/ --access-key-id AKIA... --secret-stdin < secret.txt
capturefab destination add-s3 nas --bucket frames --endpoint http://nas.local:9000 --preset rustfs --profile lab
printf '%s' "$FTP_PASSWORD" | capturefab destination add-ftp qa-ftp --host ftp.plant.local \
  --user camera --dir images --password-stdin
capturefab destination add-sftp qa-sftp --host files.plant.local --user camera --dir /srv/images \
  --host-key SHA256:... --key-file /home/me/.ssh/id_ed25519
capturefab destination test nas --probe-writes
capturefab --camera sim:0 capture -n 10 -o run-1 --destination archive
capturefab --camera sim:0 capture -o '{date}/{camera}-{seq}.png' --destination qa-sftp
capturefab --camera sim:0 record -o clip.mkv --duration 10 --destination usb
capturefab uploads --json && capturefab uploads retry --now
```

With a destination, `--output` is a name inside it (a file, a folder or a template). Every file is first written locally through the storage ledger, so budgets, retention and never-overwrite still apply. A folder on an external drive that is not mounted fails with `unavailable` (exit 3) rather than writing to the system disk; on Linux that includes any output under a mount point `/etc/fstab` declares, such as a `nofail` SSD under `/mnt`. Remote destinations stage files under the platform's data directory (`CAPTUREFAB_STAGING_DIR` overrides it; an explicit `CAPTUREFAB_SESSION_DIR` stages under its `staging` folder) and a persistent queue uploads them. Network and service errors are retried with backoff. Once a destination fails, the uploads it already tried wait for it as a whole, so an offline server is tried once per wait rather than once per file; new and retried uploads still go ahead, and `uploads retry --now` ends the wait. Refused credentials, a missing bucket or folder, a changed host key or an existing remote file wait for `uploads retry`. After a verified upload the local copy is deleted, only if unchanged, unless the destination keeps local copies (`--keep-local`). The GUI and `serve` upload in the background; a direct CLI capture waits for its uploads (up to ten minutes) and reports them.

| Destination | Transport and trust | Never-overwrite rule |
| --- | --- | --- |
| S3-compatible bucket | Signature Version 4 over HTTPS, trusting the operating system's certificates; `http://` endpoints for local stores | Conditional writes (`If-None-Match`); `--check-first` looks the key up first, which leaves a short race |
| FTP / FTPS | Explicit FTPS (`AUTH TLS`) by default; `--plain` sends files and password unencrypted | Uploads `.NAME.HASH.part`, checks size, then renames; most servers let a rename replace a file, so use one writer per folder |
| SFTP | SSH with the host key pinned by its SHA256 fingerprint | Exclusive temporary file, then an SFTPv3 rename that refuses an existing name |

S3 payloads are signed with their SHA-256 and files over 64 MiB use multipart upload. Some self-hosted services ignore conditional writes. `--preset minio|rustfs` (which needs `--endpoint` and uses path-style addressing, as an IP address or `localhost` endpoint always does) tests this when the destination is saved, as does `--probe-writes` on `add-s3` or `destination test`; when the service does not refuse a replacement, the destination switches to `check_first`. FTP servers without `SIZE` and `MLST` are refused, and plain FTP is checked by size only. An SFTP host key that changes stops the upload with exit 1 and names both fingerprints; `--accept-host-key` trusts the key the server presents at save time and prints its fingerprint. FTP and SFTP `destination test` writes and removes a small `.capturefab-write-test-*` file.

Secrets never go into Capturefab's files or commands. `--access-key-id` and `--password-stdin` keep the secret in the operating system's credential store (macOS Keychain, Windows Credential Manager, Secret Service on Linux), read from stdin or typed into the GUI. Headless hosts can use a profile from the AWS shared credentials file (`--profile`), `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` (`--env`), `CAPTUREFAB_FTP_PASSWORD` or `CAPTUREFAB_SFTP_PASSWORD` (`--password-env`) in the uploading process. With `--key-file`, the SFTP password options give the key's passphrase. `--anonymous` signs in to FTP without a password. Locations and URLs never contain the user name: `ftps://host:21/dir`, `sftp://host:22/dir`, `s3://bucket/key`. `uploads --json` lists `pending`, `uploading` and `failed` items, per-destination `destinations` counts with `waiting_bytes`, `next_attempt` (Unix seconds) and `last_error`, plus `completed_seq` and the last 20 `recent_completed` uploads with their SHA-256. An item queued while the storage ledger was busy has `unheld:true` until the uploader holds it back from retention. `destination remove NAME --forget-secret` keeps a secret that another destination still uses. Older Capturefab versions refuse an upload queue that holds FTP or SFTP items.

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

Forwarding uses a bounded latest-frame queue and probes encoders at the frame size before selecting one; working encoders are remembered by the camera worker and, for the bundled FFmpeg on Linux and macOS, until the next reboot, so later recordings start without probing. A failed probe is retried after five minutes, since a busy device can refuse once, and a probe that times out is not remembered. Candidates, in order, are the vendor encoders (Apple VideoToolbox, NVIDIA NVENC including AV1, NVIDIA Jetson through its GStreamer `nvv4l2h264enc`/`nvv4l2h265enc`/`nvv4l2av1enc` elements, AMD AMF, Intel QSV, VAAPI), then cross-vendor Vulkan Video (`h264_vulkan`, `hevc_vulkan`, `av1_vulkan`), Windows Media Foundation and Linux V4L2, where the build and host drivers support them; software H.264/HEVC is the fallback. The bundled FFmpeg has no software HEVC or AV1 encoder, so on a host without a working hardware encoder `--codec hevc` or `av1` fails before recording with `unsupported` (exit 5) and names the codecs that work. An automatically chosen encoder that exits in the first seconds of a local recording before writing any video, for example on a frame size its hardware rejects, is replaced once by the next candidate in a new file. On Jetson the GStreamer encoder runs as a separate process and FFmpeg remuxes and delivers its stream. `--encoder` selects explicitly and `--hwaccel` controls decoding for new direct, GUI or `serve` workers; existing sessions retain their decoding setting. Hardware availability is established by a real probe, not chipset branding. Frames reach FFmpeg in their sensor format (gray, 10/12/16-bit gray, Bayer or packed RGB), handed from the camera worker to the writer thread without a copy, so color conversion runs in FFmpeg's SIMD scaler or the encoder hardware rather than on the camera thread; NVENC and AMF receive packed RGB for color sources and convert it on the GPU when their probe accepts that input. See [FFmpeg builds](docs/ffmpeg-build.md). `.mkv`, fragmented `.mp4`/`.mov`, `.ts` and suitable `.webm` recordings use a bounded Rust writer. Serve HLS through MediaMTX rather than local files outside the retention manager.

Recordings keep wall-clock time: each new frame goes to FFmpeg once, at most `--fps` a second, and keeps the time FFmpeg read it, so a file lasts as long as its recording even when the camera is slower than `--fps` or the encoder falls behind, and a camera stall leaves a gap rather than copies of the last frame. Streams get the last frame again at `--fps` while the camera stalls, so their receivers stay connected. libx264 picks `veryfast`, `superfast` or `ultrafast` from the pixel rate per available core, one step faster when [efficiency mode](#host-checks-and-efficiency-mode) is efficient, and concurrent software recordings share the cores. `stop-forward` returns `{forwarding:null, stats}` with `accepted`, `written`, `dropped`, `late` (frames lost because the encoder was still busy), `stalled` (no new camera frame for 2 s), `encoder`, `error` and the finished `recording`; `record --duration` adds it as `stopped`. A camera that changes resolution, for example after reconnecting, stops the recording and keeps the file. A recording whose encoder fails, or has to be stopped, keeps the video it already wrote; a full disk or the size limit still removes it.

RTSP, SRT, RTMP, HTTP, UDP, TCP and RTP sources reconnect by themselves when their stream ends, backing off from 1 s to 30 s between attempts until the stream stays up for 10 s; the camera reports `Stream lost; reconnecting to ...` until frames return, and five rejections of its credentials in a row stop acquisition. A missing stream path, which MediaMTX reports while its publisher is away, keeps being retried. An HTTP(S) URL naming a video file, such as `.mp4` or `.mkv`, ends like a local file instead. Live decoders use up to 4 threads, shared between the host's streams (1 when efficiency mode is efficient). `--hwaccel auto` decodes in software on ARM Linux, where the bundled FFmpeg has no hardware decoder, and on hosts where no FFmpeg hardware decoder initializes (checked once per boot). A video file used as a camera plays at its own frame rate in live view and recordings, and stops with `Video file finished`. A capture or scheduled shot that starts acquisition itself reads the file as fast as it asks, so `capturefab --camera clip.mkv capture -n 100 -o frames` extracts frames at decoding speed; set `PlaybackSpeed=Fastest` to decode as fast as possible in live view too.

## Host checks and efficiency mode

`capturefab doctor` checks this computer and prints one line per check, with a copyable fix under each warning:

- **Efficiency mode**: the setting in force and why.
- **GPU** (desktop builds): the adapter the desktop window would get, asked in a helper process with a 3-second limit. A software rasterizer such as llvmpipe is a warning.
- **Session state**: settings, saved destinations, the upload queue and the storage ledger live in the session directory; camera frame rings are anonymous shared memory, not files there (except on Windows or where shared memory is unavailable), but JPEG captures encoded by nvJPEG or VA-API pass their pixels through a mapped file there. On Linux it is under `/run/user`, a tmpfs that is fast and spares SD cards but is cleared at reboot; doctor notes this without a warning. Without `XDG_RUNTIME_DIR`, which system services often lack, the session directory falls back to `~/.cache`, where logins that set it do not look for sessions. Setting the variable moves the session directory, leaving saved destinations, the upload queue and the storage budget behind, and a service also needs `loginctl enable-linger` so `/run/user/<uid>` exists at boot and survives logout.
- **GigE receive buffer** (Linux): `net.core.rmem_max` caps every socket buffer, and the usual 208 KiB loses packets from larger frames. The fix raises it now and keeps it in `/etc/sysctl.d`.
- **USB3 Vision access** (Linux): a connected camera whose `/dev/bus/usb` node this user cannot open, with a udev rule file per camera vendor.
- **Storage budget**: the unused part of the [storage budget](#bounded-storage-and-retention) against the free space on the disk of the default capture folder (`~/Capturefab`, where `storage --json` measures it), less the 512 MiB every save leaves free. A suggested budget never drops below what is already stored, so retention neither deletes nor stops captures to meet it.

`serve` prints the warnings on one line when it starts. `doctor --json` keeps its existing keys and adds `host` (CPU cores, memory, battery, low-power mode), `efficiency`, `paths` (file system type, RAM-backed, free space), `limits` (`rmem_max` and `usbfs_memory_mb` on Linux), `graphics` and `checks` (`{id,level,message,fix}`).

Efficiency mode is `auto` (the default), `smooth` or `efficient`; captures and recordings keep every frame in every mode. Auto chooses efficient with at most two CPU cores or 2 GiB of memory, or in macOS Low Power Mode, Windows battery saver or the Linux `low-power` power profile. Running on battery alone keeps it smooth. The global `--efficiency` flag wins over `CAPTUREFAB_EFFICIENCY`. It applies to the process that reads it, such as the desktop window, `serve` or a direct command, and to the camera workers that process starts, where it sets FFmpeg's decoding threads, the libx264 preset and how much of a core automatic capture may spend embedding frames on the CPU; a running session keeps its own.

```sh
capturefab doctor
capturefab --efficiency efficient serve --name line1
```

Run without a command, a headless build (exit 5) or a Linux login without `DISPLAY` or `WAYLAND_DISPLAY` explains how to start a `serve` session instead of opening a window.

## Debugging and limits

`features`, `get`, `set`, `execute`, `xml`, `memory read`, `memory write` and `doctor` expose camera state and transport diagnostics. Raw register writes are advanced operations; settings apply sequentially and a later failure does not undo earlier successful settings. Camera feature access, bounds and availability are evaluated live.

CLI field validation runs before connection and before capture/forward `--set` or `--auto` changes. Malformed supplied field values return `usage` (exit 2); live camera, destination, storage and I/O failures retain their operation categories. RPC rejects unknown fields, invalid ranges and malformed commands with a description of the rejected input. A rejected value for an auto-managed feature leaves auto mode enabled. Explicit `CAPTUREFAB_MAX_CAMERAS` values must be integers from 1 to 64; `CAPTUREFAB_FRAME_BYTES` must be 4096–268435456 bytes and a multiple of 64. Invalid overrides fail before worker startup.

Supported image conversion includes Mono8, unpacked Mono10/12/16, RGB/BGR8 and Bayer8; large Bayer frames are demosaiced across CPU cores. JPEG stills (`--format jpeg`, quality 90, 4:2:0 or grayscale) use the fastest encoder on the host. On Apple silicon, color frames of 0.5 MP and up are encoded by the dedicated JPEG engine through VideoToolbox: Capturefab writes full-range BT.601 YCbCr 4:2:0 directly into the engine's IOSurface buffers, because the engine's own RGB conversion shifts colors; `CAPTUREFAB_VIDEOTOOLBOX_JPEG=0` disables it. With CUDA and nvJPEG installed, NVIDIA GPUs encode in a separate helper process with a per-frame deadline that falls back to libjpeg-turbo's SIMD encoder if the GPU fails or stalls. Frames are converted straight into memory shared with the helper, which page-locks it so the upload is one DMA, or, on integrated GPUs such as Jetson, lets nvJPEG read it in place without an upload. `CAPTUREFAB_NVJPEG=0` disables nvJPEG, `CAPTUREFAB_NVJPEG_PIN=0` keeps pageable copies, and `CAPTUREFAB_CUDA_LIB_DIR` names a non-standard CUDA library directory. On Linux, a VA-API driver with JPEG encoding (typically Intel's iHD) is used through a helper process that loads `libva.so.2` at run time, so no libva is linked; it writes the same tables libjpeg-turbo uses and converts frames straight into the GPU surface. Before reporting ready it encodes a test image and checks that libjpeg-turbo decodes it correctly, and it is not used otherwise. `CAPTUREFAB_VAAPI_DEVICE` selects a render node and `CAPTUREFAB_VAAPI_JPEG=0` disables it. Monochrome frames (unless the driver encodes gray), small frames and other hosts use libjpeg-turbo. `doctor --json` reports which JPEG encoders are available. The GUI renders through wgpu (Metal, Vulkan, Direct3D 12 or OpenGL), uploads sensor bytes and demosaics/converts them in a WGSL shader that matches the CPU conversion. Without a usable GPU it falls back to software rendering (`ICED_BACKEND=tiny-skia` forces it) with CPU conversion; `CAPTUREFAB_GPU_PREVIEW=0` keeps the GPU renderer but converts frames on the CPU. `CAPTUREFAB_APPEARANCE=light` or `dark` overrides the system appearance. The GUI remembers its appearance, panel settings and recently connected cameras and stream URLs in `gui.prefs.json` in the session directory; addresses containing credentials or secret query parameters are not saved. Icons are Phosphor Icons (MIT). The area-scan simulator offers Mono8, RGB8, Mono12 and BayerRG8; typed depth and thermal simulators supply calibrated 16-bit samples. PNG, raw, PGM and PPM are available. Unsupported packed/chunk/multipart/GenDC formats fail explicitly. GigE supports packet reordering, duplicate detection, bounded packet resend when the camera advertises support, and packet-size negotiation. Transport status reports packet size, receive buffer, resend and loss counters. HTTP-hosted GenICam XML is unsupported. USB3 Vision requires userspace access: USB permissions on Linux, an available IOKit interface on macOS, and WinUSB for the camera interfaces on Windows.

Single-component image conversion also accepts `Coord3D_C8`, `Coord3D_C16`, `Confidence8` and `Confidence16`. Scalar floating-point depth can use the GUI color or grayscale preview and raw capture; unsupported image export still fails explicitly. Coordinate previews honor advertised Scan3d invalid-data metadata for the C coordinate without guessing zero is invalid. Use the device's calibration features when processing raw measurements.

## Build, test and release

```sh
cargo build --release                              # Rust desktop; bundles FFmpeg when supplied
cargo build --release --no-default-features \
  --features usb,scanner,jpeg,nvjpeg,vaapi,videotoolbox,parquet,modbus,mqtt,opcua,mtconnect,dinov2   # headless release set
cargo test
cargo clippy --all-features --all-targets -- -D warnings
```

Default features add S3, FTP/FTPS and SFTP destinations (`s3`, `ftp`, `sftp`), Parquet and JSON dataset exports (`parquet`) the `modbus`, `mqtt`, `opcua` and `mtconnect` signal protocols, and bundled DINOv2 embeddings for automatic capture (`dinov2`: wgpu compute shaders with a CPU fallback; `python3 scripts/fetch-dinov2.py` supplies the pinned weights that release builds embed); `doctor --json` lists what a binary has. The headless release set above leaves out the network upload destinations, and a build without `parquet` exports CSV only. The `jpeg` feature, enabled in both builds above, compiles libjpeg-turbo from source and needs a C compiler and CMake, plus NASM on x86 for its SIMD code. Source development builds can use `CAPTUREFAB_FFMPEG=/absolute/path/to/ffmpeg`. Release builds embed `CAPTUREFAB_FFMPEG_BINARY`, or the automatic path `target/ffmpeg/<target>/bin/ffmpeg[.exe]`. `CAPTUREFAB_FFMPEG_LICENSE` embeds the matching notice. The source build script pins and checksums FFmpeg, SRT, OpenSSL and x264, and includes source/license/build metadata with distributed binaries.

The release tooling uses SemVer, a cross-platform build matrix, checksums, authenticated signing, actual rendered GUI and terminal screenshots, GitHub Releases and a static site on Cloudflare Workers with an Atom feed. Build tools are development dependencies; they are not runtime dependencies. Platform-specific signing credentials are optional configuration, not keys fabricated by this repository. See the release scripts and workflows for the exact configured build matrix.

Local automated verification includes public CLI calls, isolated camera subprocesses, shared-memory cleanup, concurrent acquisition, scheduling, retention, ONVIF HTTP/auth mocks and media decoding/encoding. Aravis is used only as an independent test simulator/reference, never linked into Capturefab.

## License

Capturefab is licensed under the [MIT License](LICENSE), SPDX `MIT`. Release binaries also bundle a separately executed FFmpeg payload that remains GPLv3 (with x264 GPLv2-or-later, SRT MPLv2 and OpenSSL Apache-2.0); it is never linked into Capturefab. Redistribute FFmpeg's complete corresponding source, build scripts and notices with binaries that include it. The release license output also lists the linked Rust crates; the MPL-2.0 async-opcua crates carry a link to their source. FFmpeg source archives and its exact build manifest accompany releases; never substitute an untraceable binary or enable nonfree FFmpeg components. See [FFmpeg compliance notes](docs/ffmpeg-build.md).
