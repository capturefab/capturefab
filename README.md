# capturefab

Capture camera images and video, and keep them ready for analysis. Capturefab is a native desktop app and a scriptable CLI for manufacturing and process engineers who work on OT networks. It connects GigE Vision, USB3 Vision, webcams, RTSP/ONVIF cameras, document scanners and the cameras saved in Home Assistant, Frigate, go2rtc or UniFi Protect and catalogs every saved file in one DuckDB dataset. An export gives Parquet or CSV files plus a `README.md` and `manifest.json` that a notebook, a BI tool or an AI assistant such as Claude, ChatGPT or Gemini can read without more explanation.

Everything runs on the local computer. The optional AI assistant uses the provider you choose, only when you send a request. Apple Intelligence keeps assistant requests on-device; remote providers and prompt measurements send data to the selected API. Files are never overwritten: captures get new names from templates such as `{camera}-{datetime}-{seq}.png`. Open `capturefab` to discover cameras, select a device, inspect its settings, and capture. The same executable runs headless and lets scripts or coding agents control an already visible GUI session.

Capturefab is MIT-licensed. Its GigE Vision, USB3 Vision and GenICam clients are Rust. Media decoding, host-driver camera access and encoding use a bundled, source-built FFmpeg executable. Release binaries require no separately installed Aravis, FFmpeg, libusb, vendor SDK or Python. Operating-system graphics and camera drivers remain necessary.

This is an initial implementation, not complete Aravis or vendor-SDK feature parity. The software is under rapid development; camera and server support and behavior may change between releases.

## Upgrade from an earlier version

Version 0.4.0 removes built-in S3, FTP and SFTP uploads and industrial signal sources. Legacy transfers do not resume, and the upgrade does not delete their queued local files. Local folders and mounted network drives remain supported. Use external tools for pending transfers and signal collection. Read the [upgrade notes](https://capturefab.com/docs.html#upgrade-040) before replacing a working installation.

## Start here

Install the latest release for your account with one command. The script picks the build for this computer, downloads it from GitHub Releases, checks its SHA-256 before extracting and never uses `sudo` or administrator rights:

```sh
curl -fsSL https://capturefab.com/install.sh | sh     # macOS and Linux: ~/Applications or ~/.local/share/capturefab, command in ~/.local/bin
irm https://capturefab.com/install.ps1 | iex          # Windows PowerShell: %LOCALAPPDATA%\Programs\Capturefab, on your user PATH
```

To read the script before it runs, download it first: `curl -fsSLO https://capturefab.com/install.sh; less install.sh; sh install.sh`. `--dry-run` prints the file, URL and checksum it would use. Or download the matching binary from [GitHub Releases](https://github.com/capturefab/capturefab/releases) or the [project site](https://capturefab.com/downloads.html), which selects from published artifacts, with explicit architecture choices and SHA-256 checksums. Until a release is actually published, the site displays that state instead of fabricated download links. Newer macOS downloads are a disk image: drag Capturefab to Applications. The [first-launch steps](https://capturefab.com/docs.html#first-launch) cover SmartScreen, firewall, WinUSB and macOS Local Network prompts.

First session, with the built-in simulator and no camera hardware:

```sh
capturefab discover --simulate                              # lists sim:0, sim:1 ... beside any real cameras
capturefab --camera sim:0 features                          # Width, Height, PixelFormat ...
capturefab --camera sim:0 capture -o 'shot-{seq}.png'       # shot-0001.png; run again for shot-0002.png
capturefab --camera sim:0 record -o clip.mp4 --duration 2s  # a 2 s video; clip.mp4 must not exist yet
capturefab storage                                          # usage counted against the storage budget
```

Choose how to drive the capture. Each route can share a visible desktop session for review and manual intervention.

| Driver | Setup | First task |
| --- | --- | --- |
| Your coding agent, such as Claude Code, Codex or Antigravity | Select **Connect an agent** on the welcome screen, or **Help → Connect an Agent…**. Copy its executable path and session briefing to your harness. | Reuse the harness's model access; Capturefab assistant credentials are unnecessary. See [agent instructions](web/agents.md#start-with-an-external-harness) and [setup](https://capturefab.com/docs.html#agent-setup). |
| Capturefab's built-in assistant | Choose a provider in **Settings → AI assistant**. Apple Intelligence uses the built-in local model without credentials on supported Macs. | Open **Quick Command…**. Describe the task, review its plan or choose a bounded run. See [assistant setup](docs/assistant.md). |
| You, with direct controls | Open the desktop app. Connect a camera or use `sim:0`. | Examine the preview and capture directly. See [your first capture](https://capturefab.com/docs.html#getting-started). |

For a first agent task, use only `sim:0`, request two numbered PNG files, and keep the session open for review. In **Activity**, follow the agent's named actions and saved paths. Stop the harness task before manual takeover, then examine session status and active jobs; accepted commands can still finish.

Camera commands automatically use the only running GUI or `serve` session, so the workbench shows what an agent is doing. With no session, camera operations use a temporary worker. Use `--direct --camera ID` to request that explicitly; if several sessions are running, choose `--session NAME`. `--camera` can also come from `CAPTUREFAB_CAMERA`.

Use `--json` for versioned machine-readable results. Human-readable output stays simple; diagnostics go to stderr; JSON success and error envelopes go to stdout; exit status reports failure. `--help`, `--version`, native shell completions and binary stdout follow normal shell conventions. Existing output files are never overwritten; see [name captures quickly](#name-captures-quickly). A single filename or template infers its image format from `.png`, `.jpg`/`.jpeg`, `.raw`, `.pgm` or `.ppm`; directory sequences default to PNG, and `--format` overrides inference.

```sh
capturefab discover --json
capturefab --direct --camera sim:0 capture -o 'shot-{seq}.png'
capturefab completions fish > capturefab.fish
capturefab schema > automation-contract.json
capturefab doctor --json
```

`schema --json` uses the same envelope as other JSON commands; its contract is in `result`, including a command tree with each option, default and accepted value. Plain `schema` prints the contract directly. `--timeout-ms` bounds a frame or transport operation; `--request-timeout 5m` sets the client deadline for a session request. A timed-out request can still finish in the session, so inspect `status` before retrying a write.

Commands work from bash, zsh, fish, PowerShell, tmux, Ghostty, Warp, Claude Code, Codex, OpenCode and other tools that launch processes and read JSON. For repeatable multi-camera setup, use [JSON workflows](docs/cli-workflows.md) to avoid shell quoting and preserve an inspectable plan.

## Plant-floor data for analysis

A typical shift: open the desktop app (or `serve` on a headless [line computer](docs/line-computer.md)), connect the cameras, fill in the **Capture tags** (Line, Station, Part, Lot, Note), and capture or record. Each saved image and recording gets a row in the dataset, with its time, camera, path, SHA-256, destination and tags. At the end, `dataset export` writes an analysis bundle to a new folder.

```sh
capturefab serve --name line1 --dataset plant.duckdb
# In another terminal, or from a coding agent:
capturefab --session line1 --camera 192.168.1.10 capture -o 'line1/{date}/{camera}-{datetime}-{seq}.png' \
  --context line=L1 --context part=A-100
capturefab --session line1 dataset note "changed nozzle" --context line=L1
capturefab --session line1 dataset query "SELECT camera_id, path, wall_ms FROM captures LIMIT 10"
capturefab --session line1 dataset export -o exports/line1-shift-b
```

See the [plant-floor guide](docs/plant-floor.md) for a step-by-step task with simulators, and the sections below for [naming](#name-captures-quickly), [datasets](#datasets-and-analysis-exports) and [saving to drives and folders](#save-to-drives-and-folders).

## One session, many cameras

Each connected camera has its own capture process and three-slot shared-memory frame ring. Control uses bounded queues; live consumers prefer the newest frame. A blocked camera does not hold another camera's transport owner. The default camera limit is 16; `CAPTUREFAB_MAX_CAMERAS` accepts 1–64. The default ring payload is 16 MiB per slot (32 MiB for hub cameras); `CAPTUREFAB_FRAME_BYTES` configures up to 256 MiB. Larger frames preview downscaled, while saved files keep full resolution. Memory and file use remain bounded.

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

Raw `logs` contain `timestamp: {unix_ms, utc_offset_seconds}`, `level` and `message`. Signed Unix milliseconds identify the absolute event; the offset records the producer clock's seconds east of UTC (-86399..86399). Desktop and browser displays use the viewer's local zone, and copied desktop logs include a full date and numeric offset.

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

Agents can look at a camera without saving files. RPC `{"op":"frame","fresh":1,"max_dimension":1024}` returns the camera's newest frame after the commands sent before it: exposure, auto-meter and focus measurements, and a JPEG preview of at most 1.5 MiB as base64. A stopped camera acquires one frame and stays stopped; `region` crops the full-resolution frame. `{"op":"set_many","values":[{"feature":"ExposureTime","value":"8000"},{"feature":"Gain","value":"6"}]}` writes several features as one change: a failure restores all of them, and `undo` reverts them together. `schema --json` describes `frame` in `result.frame` and `set_many` in `result.writes`, with examples of both in `result.rpc.commands`.

## Camera tools for agents

Claude Code, Codex and other external agents can use the same checked image and data tools as Capturefab. The registry covers observation, camera controls, captures, recordings, schedules, datasets, exports and destination creation. It is available in desktop and headless builds.

```sh
capturefab serve --name bench
# In another terminal:
capturefab tools list --compact --json
capturefab tools describe inspect_frame --json
capturefab tools describe set_camera_features --json
capturefab --session bench --actor codex --actor-kind agent tools call get_overview --json
capturefab --session bench --actor codex --actor-kind agent tools call connect_camera --args '{"camera":"sim:0","size":null,"fps":null}' --json
capturefab --session bench --actor codex --actor-kind agent tools call start_live_view --args '{"camera":"sim:0"}' --json
capturefab --session bench --actor codex --actor-kind agent tools call inspect_frame --args '{"camera":"sim:0","fresh":1,"max_dimension":1024,"region":null,"include_image":true}' --json
capturefab --session bench --actor codex --actor-kind agent tools call set_camera_features --args '{"camera":"sim:0","values":[{"feature":"ExposureTime","value":"8000","expect":"10000"}],"restart_stream":false}' --dry-run --json
```

Read `result.image.path` with your image viewer. Default inspection images are private, temporary diagnostics outside the capture budget; `--image-out PATH` saves a budgeted file without overwriting. `--inline-image` returns `image.base64` in JSON. A stopped camera acquires one frame and stays stopped. With images off, `inspect_frame` still returns exposure and focus measurements.

Use `tools list --compact --json` for a small index of tools, risks and session requirements. Read one schema with `tools describe NAME --json`; both work offline. The full catalog remains available in `tools list --json` or `schema --json` under `result.tools`. Every argument key is required; use `null` for defaults. Tool calls choose their camera in `args.camera`; omit global `--camera` and `CAPTUREFAB_CAMERA`. Calls and dry runs can attach to a running session. Offline observation tools and destination creation also work without one; camera tools do not create a temporary session.

Action dry runs return `{checked,commands}` without executing. Review issues, risk and output locations first. Pass `--confirmed-by-user` for Disruptive or HumanOnly actions only after the user approved that exact action. `--secret-stdin` supplies one stream password separately from arguments. Never put secrets in prompts, command lines or plan files.

`tools apply FILE --dry-run --json` checks a strict version-1 document: `{"version":1,"calls":[{"tool":"start_live_view","args":{"camera":"sim:0"}}]}`. Plans accept 1–64 calls and at most 256 KiB. Apply rechecks the plan, executes in order and stops at the first failure; earlier effects remain. See [the external-agent recipe](web/agents.md#let-an-agent-drive-cameras) for observation, optimistic setting writes, waiting, capture, undo and typed error recovery.

## AI capture assistant

The optional `ai` feature adds `capturefab assistant`. Select one of 23 provider entries, including OpenAI, Anthropic, Gemini, Vertex AI, Bedrock, OpenRouter, Cloudflare, Groq and DeepSeek. Apple Intelligence runs locally through the Foundation Models framework on supported Macs with macOS 26 or later. Local Ollama, LM Studio and custom endpoints are also configurable. These are catalog entries, not provider compatibility claims.

In the desktop app, set up the provider in **Settings → AI assistant**, then open
**Quick Command…** with **⌘K** on macOS or **Ctrl+Shift+K** on Windows/Linux. In
Plan first mode, ask for a task, inspect its checked steps and edit their fields.
Select **Run plan** to execute the reviewed revision. Return sends a request or
commits an edit; it never runs a plan. Closing the sheet keeps the conversation
and work in progress. Use Stop to cancel a turn or stop after the current step.
For an camera task, choose **Ask before each change** or **Run on its own**, then
review its cameras and dataset, allowed destinations, separate image/data permission and capture allowances.
The sheet shows approvals, the exact preview sent, budget meters and setting
changes. Stop and Take over remain available while the task runs. Quitting
requires a separate decision; new conversations start in Plan first mode.

```sh
capturefab assistant providers --json
capturefab assistant use anthropic --model claude-sonnet-5-5 --vision auto
capturefab assistant key set anthropic --key-stdin < /private/path/provider-key.txt
capturefab assistant test anthropic --json
capturefab --session bench assistant ask 'Examine sim:0 and plan one verified photo.' --plan-out camera-plan.json --json
capturefab --session bench assistant run 'Examine sim:0 and capture two verified photos.' \
  --camera sim:0 --mode autonomous --approvals deny --max-time 10m --max-captures 2 --max-size 128MiB --json
```

`ask` runs observations and returns a plan. `run` uses explicit camera, dataset and destination scope with limits. Select an open dataset and add `--allow-data` to permit query results to reach the model. Supervised mode asks before each change; autonomous mode runs Reversible and Bounded actions within those limits. Disruptive actions always ask, and HumanOnly approval is once. `--approvals ask` must have a TTY; `deny` is the noninteractive default. Ctrl-C stops new work and discards late provider replies. Existing accepted commands finish, and the report lists leftovers instead of undoing them.

Requests include your prompt, conversation and redacted session details. Camera previews and data rows each require separate permission. Keys stay outside model content, in provider environment variables or the OS credential store. `assistant providers` does not read the keychain unless `--check-keychain` is supplied. Settings live in private `assistant.json`; private action logs support `assistant runs` and `assistant log RUN_ID`. See the [assistant guide](docs/assistant.md) for credentials, data sent, approvals, limits, cost estimates, restore rules and JSON results. External agents can use the [camera tool registry](#camera-tools-for-agents) without an in-process AI provider.

## Repeatable workflows

Validate an ordered JSON plan before connecting to cameras, then apply it to the visible workbench or a headless session:

```sh
capturefab workflow validate examples/workflows/multi-camera-timelapse.json --json
capturefab workflow apply examples/workflows/multi-camera-timelapse.json --dry-run --json
capturefab --session bench workflow apply examples/workflows/multi-camera-timelapse.json --json
```

Workflow steps use the session RPC commands, can target different cameras, and reject unknown or duplicate fields. Files are limited to 1 MiB and 1–256 steps. Every step is validated before execution; live device capabilities and output availability are checked when each step runs. Execution stops at the first failure and retains earlier effects; JSON `error.details` identifies the failed step and completed results. Jobs and encoders remain in the session after the workflow command exits.

Relative capture and recording paths resolve beside the workflow file; stdin workflows use the calling directory. Local `file:relative.mkv` and `sim:replay:relative-folder` sources use that same base, including workflow camera targets. Bare existing filenames are recognized as file sources. Ordinary CLI/RPC sources and outputs use the calling terminal's directory. Network/device locators and names inside `--destination` keep their meaning. Workflows require file outputs for captures. Workflow steps can also open a dataset (`dataset_open`), add notes and export; [plant-line.json](examples/workflows/plant-line.json) is an example. See the [workflow guide and runnable examples](docs/cli-workflows.md) for timelapse, feeder scans and bounded or timed recording.

## Remote browser control

Release executables from 0.2.2 on have the WebAssembly browser client built in. Run a headless server with `capturefab serve --name remote --listen 100.101.102.103:8787` and open the server URL from another laptop on the same tailnet to discover cameras, edit settings, preview frames and set up captures or schedules. Anyone who can reach the listener can control captures; tailnet policy and the host firewall provide the access boundary. Capture files and jobs stay on the server; the local CLI can control the same session. Each tab costs the server one JPEG encode per new frame, shared with every other tab, and nothing while the frame is unchanged or the tab is hidden. Source builds build the client with `./web-client/build.sh` and either the `embedded-web` feature or `--web-dir web-client/dist`; an older release needs that `--web-dir` too. See [remote setup and Tailscale HTTPS](docs/remote-control.md), and the [line computer guide](docs/line-computer.md) to run it as a service on a Raspberry Pi, Jetson or industrial PC: the Linux archive, camera permissions, a systemd unit, the storage budget and efficiency mode.

## Session analytics

The desktop toolbar's **Analytics** button and the browser's **Capture insights** section show acquired frames, payload throughput, missed frames and camera contributions for the current session. Missed frames are frames lost on the camera link or in a camera's worker; preview frames a viewer skipped are counted separately. DuckDB collects one-second samples of all cameras together in memory, so it keeps 24 hours whatever the camera count (at most 250,000 rows); it starts with the first connected camera, and time without one charts as idle. Totals continue across camera reconnects. The HTTP listener exposes JSON readiness at `/health`, Prometheus counters and gauges at `/metrics`, the Plotters chart at `/api/analytics.svg`. See [analytics and monitoring](docs/remote-control.md#analytics-and-monitoring) for the endpoint contract and retention limits.

The desktop also collects numeric video measurements from short prompts using OpenAI (ChatGPT), Gemini or Claude. Store API keys in the OS credential store under **Prompt measurement keys** in **Settings**, or launch with `OPENAI_API_KEY`, `GEMINI_API_KEY` / `GOOGLE_API_KEY`, or `ANTHROPIC_API_KEY`. In **Analytics**, choose a model, add prompts such as “count the number of tubes” or “estimate the length of ribbon in meters,” and explicitly start collection on a live camera. Structured results feed DuckDB history and per-measurement charts. See [prompt-driven video analytics](docs/video-analytics.md) for setup, sampling and retention.

## GigE network setup

Once a GigE camera is found, connected or tried, **Scan network** appears on the camera picker; before that, use **Check network** in the Quick guide's GigE note. It checks GigE interfaces, camera reachability, link speed and MTU, then applies network defaults to connected cameras, and **Report** reopens the last results. The report distinguishes discovery from a working control connection and from observed stream stability. Slow Ethernet, Wi-Fi, routed paths and unknown link speeds stay visibly flagged. Live camera badges show waiting traffic, retries and lost frames.

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

The desktop preview converts only as many pixels as the window shows, and every pixel at 1:1 and closer zooms. A scaled-down preview averages the pixels each screen pixel covers, reading Bayer frames in whole color quads, so it neither aliases nor shows false color. With a GPU, frames are decoded there; a frame too large for a GPU texture is shrunk for it, except at 1:1. In efficiency mode a fitted preview keeps at most 1280 pixels on its long side, with or without a GPU, and a scaled-down one keeps the middle pixel or color quad of each block it covers instead of averaging them, which costs a fraction as much but can alias. Captures, recordings and the histogram and focus scopes always read the camera's full frame.

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

`status.history` and `cameras[].history` report the next undo/redo labels, or `null` when unavailable. A separate `tools call undo_camera_change` invocation requires `--confirmed-by-user` because it cannot prove ownership of the token; assistant tasks track their own tokens. History entries include `setting_changes`, the number of feature assignments restored by a checkpoint; older workers may omit it. History is bounded and lasts for the camera connection. Replay checks camera state and reports conflicts or failures without discarding the entry. Captures, recordings, schedules, connection changes, arbitrary camera commands and raw memory writes have external effects that camera history cannot reverse; stopping a stream while it has schedules or forwarding is also excluded.

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

Schedules return a job ID in a persistent session. A direct CLI waits for its job to finish. Jobs expose progress, the next capture time, errors and cancellation. Scheduled frames are encoded and saved on their own thread, so previews, recordings and forwarding keep running meanwhile. That thread holds at most two frames: a shot that finds it full is counted in the job's `skipped` and taken at a later interval. A shot that fails because the storage ledger stays busy or the disk times out is counted in `retries` and taken again at the next interval, with the message in `error` until a later shot saves. Each skipped or retried shot moves the next capture time one interval later. Storage full and other save errors stop the job. Keep the GUI, headless session or direct command running; schedules do not survive process exit. Timestamps require an explicit timezone. Durations accept `ms`, `s`, `m`, `h`, `d`, or seconds without a suffix. Each frame has a bounded transport timeout. `stop` cancels pending capture jobs on its camera.

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
capturefab --direct --camera sim:0 capture --set Width=640 --set Height=480 -o - -f ppm > frame.ppm   # image bytes on stdout
```

`{seq}` may appear once, only in the file name, and never directly before a digit or `{date}`, `{time}`, `{datetime}` or `{frame}`. Any other `{name}` is a usage error (exit 2). With `-n` above 1, a template needs `{frame}` or `{seq}`, or `--on-exists number`. `--on-exists fail` (the CLI and RPC default) refuses an existing name; `number` writes `stem-0001.ext`, `stem-0002.ext` and so on. A directory output continues `frame-NNNNNN` after the highest existing number, so a rerun into the same folder adds files. If no free name is found after 64 attempts, the capture fails with exit 1.

`--context KEY=VALUE` tags captures and recordings; keys match `^[a-z][a-z0-9_]{0,31}$`, with at most 16 entries of 256 bytes each. The JSON capture result is `{files, frames:[{id,width,height,pixel_format,timestamp_ns,bytes,wall_ms,file}], count, context?, destination?}`; `wall_ms` is UTC milliseconds. `record` returns `{forwarding, forwarding_id, since_ms, path}` with the resolved file, plus `until_ms` when it has a duration, and `status` shows its opaque `forwarding_id`, `forwarding_since_ms`, optional `forwarding_until_ms` and, for a local recording, `recording_path` with the resolved file. Timed fallback and assistant cleanup stop only that identity, so they cannot stop a replacement recording. Timed recordings count elapsed time and computer sleep; a backward system-clock change does not extend them.

The desktop app saves to `~/Capturefab/{date}/{camera}-{datetime}-{seq}.png` by default, records to the same pattern with `.mp4`, and always numbers names that exist. **Next file** in the Capture panel shows the next file's path from the chosen folder. The toolbar **Record** button and Cmd/Ctrl-Shift-R start and stop a local recording, with an elapsed-time pill (the time left, for a recording started with a duration); the File menu has the same action. **Show in Finder** (Explorer, or the folder on Linux) opens the last capture or recording. Each desktop window shows one camera. **File › Connect to Camera…** (Cmd/Ctrl-N) opens another window on the camera picker; picking a camera that another window shows brings that window forward. Closing a window disconnects its camera, after asking if it is recording, and closing the last window quits. **Capture** saves from this window's camera, and **Capture All** (Cmd/Ctrl-Shift-S) saves one frame from every connected camera at once; the Camera menu also has **Record All Cameras**. Each camera writes its own file. **Capture tags** (Line, Station, Part, Lot, Note) are sent with every capture and recording as the context keys `line`, `station`, `part`, `lot` and `note`. The setting **Start the live view when a camera connects** is on by default and never starts a document scanner.

## Datasets and analysis exports

A dataset is one `.duckdb` file owned by one session. It catalogs every saved capture and finished local recording, and notes. `--dataset FILE.duckdb` (or `CAPTUREFAB_DATASET`) on `capture`, `record`, `serve`, and `gui` creates or opens it; a session already recording to a different file refuses with an error.

```sh
capturefab --session line1 dataset open plant.duckdb --max-bytes 8GiB   # --new or --existing to be strict
capturefab --session line1 dataset status
capturefab --session line1 dataset note "shift B start" --context line=L1
capturefab --session line1 dataset query "SELECT camera_id, count(*) FROM captures GROUP BY 1"
capturefab --session line1 dataset export -o exports/shift-b --format parquet,csv
capturefab --session line1 dataset close
# A file no session holds:
capturefab dataset status --file plant.duckdb
capturefab dataset query --file plant.duckdb "SELECT * FROM captures LIMIT 20" --json
capturefab dataset export --file plant.duckdb -o plant-export --since 2026-10-17T06:00:00-04:00
```

Tables are `captures`, `recordings` and `events`. Every `*_ms` column is UTC Unix milliseconds from the host clock; `context` is JSON text.

`dataset query` runs one read-only `SELECT` or `WITH` statement that cannot reach other files. It returns `{columns, rows, row_count, truncated}`: `--limit` 1 to 10,000 (default 1000), at most 8 MiB of JSON, a 30 s timeout, SQL up to 64 KiB. Large integers and decimals come back as strings. Without `--file`, `status`, `query` and `export` need a session; `--file` on a file a running session holds fails with exit 3 and says to reach it with `--session NAME`; `capturefab sessions` lists the running sessions.

`dataset export` writes into a new folder (an existing one is refused with exit 1) through the storage budget, which it checks before writing anything. It writes each table in Parquet and CSV by default (`--format csv,parquet,json,jsonl`, `--tables`, `--since`, `--until`), a `manifest.json` (columns, types, descriptions, SHA-256 of each file, cameras and capture statistics) and a `README.md` with the time base, loading code for pandas, DuckDB and polars, and a short summary for AI tools. Images and videos stay at the paths in the tables.

The dataset file plus its write-ahead log is limited to 8 GiB by default. Writes stop with `Storage full` (exit 6) below 512 MiB of free disk space. In the desktop app, the **Dataset** section of the Capture panel has **New…**, **Open…**, **Export…**, **Show in Finder** and **Close**. With **Catalog captures automatically** (on by default), the app creates `~/Capturefab/capturefab.duckdb` on the first saved file and reopens the chosen dataset at the next start.

Session RPC and workflow files use the same operations: `dataset_open {path, mode: auto|create|open, max_bytes?}`, `dataset_close`, `dataset_status`, `dataset_note {text, context?, camera?}`, `dataset_query {sql, limit, timeout_ms}`, `dataset_export {output, formats?, tables?, since_ms?, until_ms?}`. RPC paths must be absolute; `schema --json` has an example of each in `result.rpc.commands`, and its `result.dataset` section lists the tables, limits and exit codes.

## Bounded storage and retention


Every image capture and local recording goes through a shared, OS-locked ownership ledger. The default total budget is **10 GiB and 10,000 files**, across output directories and capture processes sharing a [state directory](#host-checks-and-efficiency-mode). Active recording reservations count toward that budget. A local video file also has a configurable maximum size, initially **512 MiB**; a recording that reaches it stops there with `storage_full` and keeps the video written until then.

```sh
capturefab --camera sim:0 capture -n 100 -o frames --max-space 2GiB --max-files 1000
capturefab --camera sim:0 capture --interval 1m -n 10000 -o history --max-space 1GiB --delete-oldest --max-age 7d
capturefab storage --json
capturefab storage configure --max-space 2GiB --max-files 1000 --on-full stop
```

`storage configure` persists a global ceiling across restarts; per-command limits can lower it. The default policy stops with an actionable `storage_full` error (exit 6) instead of silently deleting data. `--delete-oldest` explicitly permits retention to remove Capturefab-owned files. The ledger verifies file identity and SHA-256 before deleting: existing files, user replacements, altered captures and active recordings are protected. Age retention is enforced when allocating new captures, at most 32 expired files per save while the budget has room; protected files past the age limit never block a save that fits. Missing files release their ledger usage; interrupted reservations are reconciled through crash-released OS locks. Ordinary disk-full and write errors stop capture. A still capture's partial file is removed when it is safely identifiable; a recording keeps the video it wrote before the error, recorded in the ledger, since its container plays up to its last complete part.

Every save must also leave 512 MiB free on the disk it writes to, the same floor datasets keep, counting what active recordings may still write. A recording reserves its maximum file size, so on a nearly full card lower `--max-file-size`. With `--delete-oldest` the oldest Capturefab files on that disk are removed to keep the floor; otherwise the save fails with `storage_full` (exit 6). If removing them does not free space, for example while a snapshot holds it, no more are removed for the floor until a save on that disk fits again. Windows cannot tell which drive a tracked file is on, so there nothing is removed for the floor. `storage --json` reports `free_bytes` for the disk of the default capture folder, `~/Capturefab`.

`--max-age` requires whole seconds, at least `1s`, and a delete-oldest policy. Recording reservations must fit the requested storage budget; an oversized `--max-file-size` is rejected rather than reduced. `record`/`forward --duration 30s` works in temporary workers and persistent sessions: the CLI sends `duration_ms`, the camera worker stops the encoder at that time and commits the file, and the CLI waits for it (`"stopped": {"forwarding": null, "by": "duration"}`); Ctrl-C stops it early. Session RPC and workflow `forward` steps take `duration_ms` too, so a workflow can start a timed recording that ends without a client. Size `--max-file-size` for the whole duration (about 36 MiB a minute at `4M`, plus 16 MiB): a recording that reaches it stops with storage_full and keeps the video written until then, recorded in the ledger. Without a duration, persistent sessions return immediately and continue until `stop-forward`; direct commands stay in the foreground until Ctrl-C.

The budget covers capture/recording data, not unrelated files on the disk. Runtime frame rings, queues and extracted FFmpeg are separately bounded: release builds extract their FFmpeg once per build into the user cache (`~/Library/Caches/capturefab` on macOS, `$XDG_CACHE_HOME/capturefab` or `~/.cache/capturefab` elsewhere, `%LOCALAPPDATA%\capturefab\cache` on Windows; `CAPTUREFAB_CACHE_DIR` overrides it) and remove copies of older builds that no process is using. Processes of one user share a storage budget through the state directory; a different `CAPTUREFAB_SESSION_DIR` creates a separate ledger and budget. The ledger is `storage.json` plus an append-only `storage.journal` in the state directory, so a save writes about a kilobyte of bookkeeping however many files are kept.

## Recover retained camera catalogs

Retained camera catalog workspaces stay charged to storage until explicitly removed. List them by reservation ID, save a copy into a new folder, or remove an available workspace after reviewing it:

```sh
capturefab storage task-data list --json
capturefab storage task-data copy RESERVATION_ID recovered-catalog --json
# Only after approving deletion of this exact workspace:
capturefab storage task-data remove RESERVATION_ID --confirmed-by-user --json
```

Recovery refuses a running owner. Copies preserve the database and any WAL together and consume their own storage allowance; keep `task.duckdb.wal` beside `task.duckdb`. Changed directory identities, extra hard links or uncertain shutdown keep the reservation charged. Failed copies can leave partial output; inspect it before retrying.

## Save to drives and folders

Captures, schedules and local recordings can go to a saved **destination**: a folder on your disk, an external or removable drive, or a mounted network drive. In the GUI, **Save to** in the Capture and Video panels lists saved folders and connected external drives. Choosing a drive saves to its `Capturefab` folder. **Manage…** adds a folder with the system folder picker and tests access. On macOS and Linux, `capturefab volumes` omits read-only volumes, such as mounted disk images.

```sh
capturefab volumes --json
capturefab destination add-folder usb "/Volumes/CAMDISK/Capturefab"
capturefab destination list --json
capturefab destination test usb
capturefab --camera sim:0 capture -n 10 -o run-1 --destination usb
capturefab --camera sim:0 record -o clip.mkv --duration 10s --destination usb
```

With a destination, `--output` is a name inside that folder: a file, a directory or a template. Files pass through the shared storage ledger, so budgets, retention and never-overwrite checks still apply. A folder on a drive that is not mounted fails with `unavailable` (exit 3) rather than writing to the system disk. On Linux this includes outputs under mount points declared in `/etc/fstab`, such as a `nofail` SSD under `/mnt`. Removing a destination does not remove its files.

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

## Resource limits and headroom

Open **Resources** in the toolbar or Settings to see live CPU load, shared camera-buffer usage and capture-disk headroom. The status indicator calls out resource pressure and recent frame loss. **Camera frames lost** and **preview frames skipped** are separate: skipping an old preview keeps the display current and does not mean a saved capture was lost.

Automatic protection is on by default. Image operations and new codecs use at most eight processing threads, and fewer on smaller or busy machines; high CPU load or low available memory reduces parallel work further. Automatic preview mode also draws less under pressure. New shared camera buffers must fit within one quarter of physical memory (between 512 MiB and 2 GiB) while leaving a system-memory reserve. Existing cameras are not disconnected when a budget is lowered: the UI shows any overage and refuses new allocations until there is room.


Each resource has a simple adjustment and an explicit effective limit. Processing-thread changes affect subsequent image operations and new codec instances; running recordings keep their settings. A thread limit is not a hard operating-system CPU percentage cap, and the buffer budget does not include every decoder or GPU allocation. The live device-memory reading shows the remaining system headroom separately.

```sh
capturefab resources --json
capturefab resources configure --cpu-threads 2 --camera-buffers 1GiB
capturefab resources reset
```

Settings are shared through `resources.json` in the state directory. `GET /api/resources` exposes read-only status. Limits can reduce throughput on demanding cameras; the Resources view keeps that context beside observed losses instead of attributing every missed frame to one cause. Saved images are not silently resized to satisfy these limits.

## Host checks and efficiency mode

`capturefab doctor` checks this computer and prints one line per check, with a copyable fix under each warning:

- **Efficiency mode**: the setting in force and why.
- **GPU** (builds with the GUI): the adapter the desktop window would get, asked in a helper process with a 3-second limit. A software rasterizer such as llvmpipe is a warning.
- **Session state**: on Linux, sessions are found in the runtime directory, `$XDG_RUNTIME_DIR/capturefab`, a tmpfs that is fast and spares SD cards. Without `XDG_RUNTIME_DIR`, which system services often lack, Capturefab uses `/run/user/<uid>/capturefab` when the user lingers (`loginctl enable-linger`) and `/run/user/<uid>` belongs to them with mode 0700, so a service and an SSH shell find the same sessions; otherwise it falls back to `~/.cache/capturefab/sessions` on disk, and doctor warns where logind runs. From a login that has `XDG_RUNTIME_DIR`, such as SSH, doctor's `runtime_dir` line says where a service without it would keep its sessions, and warns once one runs there; `sessions` and `--session` name a session they find there. Without lingering, logind removes `/run/user/<uid>` at logout, and with it any session a service kept there, so a service needs `loginctl enable-linger` and `Wants=` and `After=user@<uid>.service`; a service no login needs to reach with `--session` can instead set `RuntimeDirectory=capturefab`, `RuntimeDirectoryMode=0700` and `Environment=XDG_RUNTIME_DIR=/run/capturefab`. Settings, saved hubs and destinations, and the storage ledger live in the state directory, `$XDG_STATE_HOME/capturefab` or `~/.local/state/capturefab`, so they survive logout and reboot. macOS and Windows keep both in one session directory on disk, and `CAPTUREFAB_SESSION_DIR` sets both at once. Camera frame rings are anonymous shared memory, not files (except on Windows or where shared memory is unavailable), but JPEG captures encoded by nvJPEG or VA-API pass their pixels through a mapped file in the runtime directory. A state directory in memory, such as a `CAPTUREFAB_SESSION_DIR` on tmpfs, is noted without a warning.
- **Private directories**: a runtime or state directory that exists but is not private to this user (mode 0700, owned by them), which sessions refuse; the fix is `chmod 700`.
- **Camera network**: the wired Ethernet adapters a GigE Vision camera could use, with address, prefix and MTU (jumbo frames or not), noting link-local addresses. Never a warning: webcams, USB3 and network streams need no wired adapter. `capturefab network-scan` checks a camera link.
- **FFmpeg**: whether the bundled FFmpeg (or `CAPTUREFAB_FFMPEG`) runs `-version` within 3 seconds; webcams, RTSP/SRT, files and recording need it.
- **GigE receive buffer** (Linux): `net.core.rmem_max` caps every socket buffer, and the usual 208 KiB loses packets from larger frames. The fix raises it now and keeps it in `/etc/sysctl.d`.
- **USB3 Vision access** (Linux): a connected camera whose `/dev/bus/usb` node this user cannot open, with a single USB3 Vision udev rule (also shipped in the Linux download under `share/udev/rules.d`).
- **Storage budget**: the unused part of the [storage budget](#bounded-storage-and-retention) against the free space on the disk of the default capture folder (`~/Capturefab`, where `storage --json` measures it), less the 512 MiB every save leaves free. A suggested budget never drops below what is already stored, so retention neither deletes nor stops captures to meet it.

`serve` prints the warnings on one line when it starts. `doctor --json` reports `host` (CPU cores, memory, battery, low-power mode), `efficiency`, `runtime_directory`, `state_directory` and `cache_directory`, `paths` (file system type, RAM-backed and free space of each), `limits` (`rmem_max` and `usbfs_memory_mb` on Linux), `graphics` and `checks` (`{id,level,message,fix}`).

Efficiency mode is `auto` (the default), `smooth` or `efficient`; preview reductions preserve saved image quality and recording settings. Auto chooses efficient with at most two CPU cores or 2 GiB of memory, in macOS Low Power Mode, Windows battery saver or the Linux `low-power` power profile, or while the desktop window reports a hint: software rendering, or a live preview that keeps falling behind its camera (it then says so once). Running on battery alone keeps it smooth. The global `--efficiency` flag wins over `CAPTUREFAB_EFFICIENCY`, which wins over **Settings › Performance** (Automatic, Smooth or Efficient) in the desktop window; only the desktop window reads that setting, so `serve` and direct commands do not follow it, and doctor says when the window runs otherwise. The mode applies to the process that reads it, such as the desktop window, `serve` or a direct command, and to the camera workers that process starts, where it sets FFmpeg's decoding threads, and the libx264 preset; a running session keeps its own, and its workers follow when it changes, for decoders they start afterwards. In efficient mode a session also asks its camera workers for their status half as often. In the desktop window, efficient draws at most 15 camera frames a second, keeps a scaled-down picture to 1280 pixels on its long side, checks for new frames less often and skips animations. Scaled-down previews, in the desktop window and the browser alike, then keep one pixel of each block instead of averaging them.

```sh
capturefab doctor
capturefab --efficiency efficient serve --name line1
```

Run without a command, a Linux login without `DISPLAY` or `WAYLAND_DISPLAY` (an SSH session, a line computer) lists the CLI commands and explains how to start a `serve` session instead of opening a window. A source build compiled without the `gui` feature exits 5 with the same explanation.

## Cameras from Home Assistant, Frigate, go2rtc and UniFi Protect

A **hub** is a saved Home Assistant, Frigate, go2rtc or UniFi Protect server. Save it once with `capturefab hub add-…`; its cameras then appear in `discover` (after local cameras) and on the desktop camera picker, and open as `hub:NAME/CAMERA` wherever `--camera` takes a source. Capturefab reads frames from the hub, through its HTTP API or RTSP restream, and never contacts the cameras behind it.

```sh
capturefab hub add-home-assistant home --url http://homeassistant.local:8123 --secret-stdin < token.txt
capturefab hub add-frigate nvr --url http://nvr.local:5000
capturefab hub add-go2rtc streams --url http://go2rtc.local:1984
capturefab hub add-unifi-protect protect --url https://192.168.1.1 --secret-stdin --accept-certificate < api-key.txt
capturefab hub list
capturefab hub cameras home                 # IDs to use with --camera
capturefab hub test home
capturefab --camera 'hub:home/camera.front_door' capture -o front.png
capturefab --camera 'hub:home/camera.porch?stream=snapshot' capture --interval 1m -n 60 -o porch
capturefab hub remove home --forget-secret
```

| Hub | Camera ID | Streams (auto first) |
| --- | --- | --- |
| Home Assistant | `hub:NAME/camera.ENTITY` | `mjpeg`, `snapshot` |
| Frigate | `hub:NAME/CAMERA` (the camera name) | `rtsp` (Frigate's restream), `snapshot`; also `mjpeg` |
| go2rtc | `hub:NAME/STREAM` | `rtsp`, `snapshot`; also `mjpeg` for JPEG sources |
| UniFi Protect | `hub:NAME/CAMERA_ID` (24 hexadecimal digits) | `snapshot` |

`hub` (alias `hubs`) has `list`, `cameras [NAME]`, `test NAME`, `add-home-assistant` (alias `add-ha`), `add-frigate`, `add-go2rtc`, `add-unifi-protect` (alias `add-unifi`) and `remove NAME [--forget-secret]`. Each `add-…` takes `NAME --url URL`, checks that the server answers and accepts the secret (`--no-check` saves without contacting it), and replaces a hub with the same name. Frigate uses `http://HOST:5000` without a user, or `https://HOST:8971` with `--user U` and a password; `--rtsp-port` (default 8554) names its restream port. go2rtc takes `--user U` with a password when its API has one. UniFi Protect needs `https://CONSOLE` and an API key from **Settings > Control Plane > Integrations**.

Selectors are `hub:NAME/CAMERA[?stream=auto|mjpeg|snapshot|rtsp&fps=F]`; quote them, because shells treat `?` and `&` specially. `stream=auto` (the default) tries the streams in the order above when connecting and keeps the first that delivers a frame. `fps` sets the polling rate of `stream=snapshot`: 0.01–2 per second for Home Assistant and go2rtc (default 1), up to 5 for Frigate (default 1), and up to 1 for UniFi Protect (default 0.5); Frigate's `stream=mjpeg` takes a whole `fps` from 1 to 10. Percent-encode characters in CAMERA other than `A-Z a-z 0-9 - . _ ~` as `%XX`. The camera ID is `hub:NAME/CAMERA` without options and its serial is `NAME/CAMERA`. Disconnect a connected hub camera before connecting it again with other options.

Secrets never go into Capturefab's files or commands; `hubs.json` in the state directory records only where each one comes from. `--secret-stdin` reads the long-lived access token (Home Assistant: **Profile > Security**), password (Frigate, go2rtc) or API key (UniFi Protect) and keeps it in the operating system's credential store (service `capturefab-hub`, one entry per hub and server), where the GUI, `serve` and the CLI all find it. `--secret-env CAPTUREFAB_HUB_HOME_TOKEN` saves only the variable's name (`CAPTUREFAB_HUB_` followed by 1–48 of `A-Z`, `0-9` and `_`); the variable is read by the process that opens or lists the camera (a GUI or `serve` session for the cameras it discovers and connects; the CLI for `hub` commands and direct `--camera` commands), and it must also be set where you add the hub unless you use `--no-check`. A GUI started from the Dock, Finder or Start menu does not see shell variables, so use `--secret-stdin` for the desktop app. Builds without the credential store take `--secret-env` only. Re-adding a keychain hub for the same server without a secret option keeps its saved secret. FFmpeg never inherits `CAPTUREFAB_HUB_*` variables. Give Home Assistant the token of a dedicated non-admin user: the token carries that user's access. A secret the server refuses is recorded, and discovery skips that hub, without sending the secret again, until `hub test NAME` or an explicit camera connection succeeds, or the hub is re-added. Home Assistant can ban an address after repeated failed logins when IP banning is configured.

`https://` hubs trust the operating system's certificates. For a self-signed certificate (UniFi consoles, and Frigate's port 8971 by default), `--accept-certificate` trusts the certificate the server presents when the hub is saved and prints its fingerprint, so confirm it with the administrator; it conflicts with `--no-check`. `--certificate SHA256:AB:CD:…` (as `openssl x509 -noout -fingerprint -sha256` prints it) pins a known certificate, and `--tls-server-name NAME` checks a trusted certificate against NAME when the URL uses an IP address; use at most one of these options. A pinned certificate that changes stops the camera and names both fingerprints. Over `http://`, a token or password crosses the network unencrypted and the CLI warns; Frigate passwords are only sent over `https://`.

`serve --listen` refuses to connect hub cameras for remote clients, because they would use this computer's hub secrets, unless it was started with `--allow-hubs`.

JPEG and PNG stills and MJPEG are fetched and decoded in the camera worker; RTSP restreams are decoded by FFmpeg and opened without a password, so Frigate's restream port and go2rtc's RTSP server must allow unauthenticated reading. Each stream attempt allows at least 12 s for its first frame (15 s for RTSP); `--timeout-ms` can raise that allowance. API checks and automatic fallback can extend the total connection time. Home Assistant's MJPEG stream sends nothing while the image is unchanged; use `stream=snapshot` for static scenes and time-lapses. Hub camera settings are read-only; change them in the hub. `HTTP_PROXY` and `HTTPS_PROXY` are ignored. Hub cameras get a 32 MiB frame ring per slot, enough for 4096×2160 RGB frames; `CAPTUREFAB_FRAME_BYTES` overrides it. Streams reconnect automatically with backoff; a refused secret or certificate stops acquisition. Exit codes: 2 for invalid names, URLs, flags or selectors; 3 when a hub or camera is not found, offline or unreachable; 4 for a timeout; 5 when hubs or the credential store are not compiled into the build; 1 for a refused secret or certificate or an unexpected answer. `schema --json` describes each kind in `result.hubs`. Compatibility with particular hub versions and cameras has not been validated.

## Update checks

The desktop app looks for a new version at most once a day by downloading the public release list, `https://capturefab.com/releases.json`. The request carries no account, device, usage or version information and no cookies; like any website visit, the server sees your IP address. Capturefab never downloads or installs anything itself: **Check for Updates…** (in the application menu on macOS, the Help menu elsewhere) shows the release notes, a download link for this computer and the file's SHA-256 to check against `SHA256SUMS`, and can skip a version. Turn automatic checks off in **Settings › Updates**. `CAPTUREFAB_UPDATE_CHECK=off`, or `{"update_check":"off"}` in `/Library/Application Support/Capturefab/policy.json` (macOS), `/etc/capturefab/policy.json` or `%ProgramData%\Capturefab\policy.json` (Windows), turns off every check for managed installs. The CLI and `serve` never check on their own: `capturefab version` prints the version and what the last check found without a network request, and `capturefab version --check` reads the list once. `doctor` warns when a newer version is known.

## Debugging and limits

`features`, `get`, `set`, `execute`, `xml`, `memory read`, `memory write` and `doctor` expose camera state and transport diagnostics. Raw register writes are advanced operations; settings apply sequentially and a later failure does not undo earlier successful settings. Camera feature access, bounds and availability are evaluated live.

CLI field validation runs before connection and before capture/forward `--set` or `--auto` changes. Malformed supplied field values return `usage` (exit 2); live camera, destination, storage and I/O failures retain their operation categories. RPC rejects unknown fields, invalid ranges and malformed commands with a description of the rejected input. A rejected value for an auto-managed feature leaves auto mode enabled. Explicit `CAPTUREFAB_MAX_CAMERAS` values must be integers from 1 to 64; `CAPTUREFAB_FRAME_BYTES` must be 4096–268435456 bytes and a multiple of 64. Invalid overrides fail before worker startup.

Supported image conversion includes Mono8, unpacked Mono10/12/16, RGB/BGR8 and Bayer8; large Bayer frames are demosaiced across all but one CPU core, which stays free for the camera workers. JPEG stills (`--format jpeg`, quality 90, 4:2:0 or grayscale) use the fastest encoder on the host. On Apple silicon, color frames of 0.5 MP and up are encoded by the dedicated JPEG engine through VideoToolbox: Capturefab writes full-range BT.601 YCbCr 4:2:0 directly into the engine's IOSurface buffers, because the engine's own RGB conversion shifts colors; `CAPTUREFAB_VIDEOTOOLBOX_JPEG=0` disables it. With CUDA and nvJPEG installed, NVIDIA GPUs encode in a separate helper process with a per-frame deadline that falls back to libjpeg-turbo's SIMD encoder if the GPU fails or stalls. Frames are converted straight into memory shared with the helper, which page-locks it so the upload is one DMA, or, on integrated GPUs such as Jetson, lets nvJPEG read it in place without an upload. `CAPTUREFAB_NVJPEG=0` disables nvJPEG, `CAPTUREFAB_NVJPEG_PIN=0` keeps pageable copies, and `CAPTUREFAB_CUDA_LIB_DIR` names a non-standard CUDA library directory. On Linux, a VA-API driver with JPEG encoding (typically Intel's iHD) is used through a helper process that loads `libva.so.2` at run time, so no libva is linked; it writes the same tables libjpeg-turbo uses and converts frames straight into the GPU surface. Before reporting ready it encodes a test image and checks that libjpeg-turbo decodes it correctly, and it is not used otherwise. `CAPTUREFAB_VAAPI_DEVICE` selects a render node and `CAPTUREFAB_VAAPI_JPEG=0` disables it. Monochrome frames (unless the driver encodes gray), small frames and other hosts use libjpeg-turbo. `doctor --json` reports which JPEG encoders are available. The GUI renders through wgpu (Metal, Vulkan, Direct3D 12 or OpenGL), uploads sensor bytes and demosaics/converts them in a WGSL shader that matches the CPU conversion. Without a usable GPU it falls back to software rendering (`ICED_BACKEND=tiny-skia` forces it) with CPU conversion, which Automatic [efficiency mode](#host-checks-and-efficiency-mode) answers with efficient previews; `CAPTUREFAB_GPU_PREVIEW=0` keeps the GPU renderer but converts frames on the CPU. The live image takes only the frames of the cameras it shows, at most one per display refresh, and none while the window is minimized or, where the system tells (macOS, most Wayland desktops), hidden or covered. When a camera's preview draws clearly fewer frames than the camera sends, its controls show `Preview 9 of 30 fps`, with a button that turns on Efficient mode. A network stream being reopened shows **Reconnecting…** over its picture and in the title bar's status until frames return. `CAPTUREFAB_APPEARANCE=light` or `dark` overrides the system appearance. The GUI remembers its appearance, performance setting, panel settings and recently connected cameras and stream URLs in `gui.prefs.json` in the [state directory](#host-checks-and-efficiency-mode); addresses containing credentials or secret query parameters are not saved. Icons are Phosphor Icons (MIT). The area-scan simulator offers Mono8, RGB8, Mono12 and BayerRG8; typed depth and thermal simulators supply calibrated 16-bit samples. PNG, raw, PGM and PPM are available. Unsupported packed/chunk/multipart/GenDC formats fail explicitly. GigE supports packet reordering, duplicate detection, bounded packet resend when the camera advertises support, and packet-size negotiation. Transport status reports packet size, receive buffer, resend and loss counters. HTTP-hosted GenICam XML is unsupported. USB3 Vision requires userspace access: USB permissions on Linux, an available IOKit interface on macOS, and WinUSB for the camera interfaces on Windows.

Single-component image conversion also accepts `Coord3D_C8`, `Coord3D_C16`, `Confidence8` and `Confidence16`. Scalar floating-point depth can use the GUI color or grayscale preview and raw capture; unsupported image export still fails explicitly. Coordinate previews honor advertised Scan3d invalid-data metadata for the C coordinate without guessing zero is invalid. Use the device's calibration features when processing raw measurements.

## Build, test and release

```sh
cargo build --release                              # Rust desktop; bundles FFmpeg when supplied
cargo build --release --no-default-features \
  --features usb,scanner,jpeg,nvjpeg,vaapi,videotoolbox,parquet,hubs,updates   # no GUI: CLI and serve only
cargo test
cargo clippy --all-features --all-targets -- -D warnings
```

Default features add Parquet and JSON dataset exports (`parquet`), cameras from Home Assistant, Frigate, go2rtc and UniFi Protect (`hubs`) and [update checks](#update-checks) (`updates`). The desktop's AI provider keys bring the OS credential store (`keychain`); `doctor --json` lists what a binary has. Both release sets use the credential store for hub secrets; builds without `keychain` use `--secret-env` only. A build without `parquet` exports CSV only. Release builds also add `embedded-web` once `./web-client/build.sh` has built the browser client, so `serve --listen` needs no `--web-dir`. The `jpeg` feature, enabled in both builds above, compiles libjpeg-turbo from source and needs a C compiler and CMake, plus NASM on x86 for its SIMD code. Every build embeds `CAPTUREFAB_FFMPEG_BINARY`, or the automatic path `target/ffmpeg/<target>/bin/ffmpeg[.exe]`, building it with `scripts/build-ffmpeg.sh` when absent; `CAPTUREFAB_FFMPEG=/absolute/path/to/ffmpeg` overrides it at runtime. `CAPTUREFAB_FFMPEG_LICENSE` embeds the matching notice. The source build script pins and checksums FFmpeg, SRT, OpenSSL and x264, and includes source/license/build metadata with distributed binaries.

The release tooling uses SemVer, a cross-platform build matrix, checksums, authenticated signing, actual rendered GUI and terminal screenshots, GitHub Releases and a static site on Cloudflare Workers with an Atom feed. Build tools are development dependencies; they are not runtime dependencies. Platform-specific signing credentials are optional configuration, not keys fabricated by this repository. See the release scripts and workflows for the exact configured build matrix.

Local automated verification includes public CLI calls, isolated camera subprocesses, shared-memory cleanup, concurrent acquisition, scheduling, retention, ONVIF HTTP/auth mocks, camera hub HTTP/TLS fakes and media decoding/encoding. Aravis is used only as an independent test simulator/reference, never linked into Capturefab.

## License

Capturefab is licensed under the [MIT License](LICENSE), SPDX `MIT`. Release binaries also bundle a separately executed FFmpeg payload that remains GPLv3 (with x264 GPLv2-or-later, SRT MPLv2 and OpenSSL Apache-2.0); it is never linked into Capturefab. Redistribute FFmpeg's complete corresponding source, build scripts and notices with binaries that include it. The release license output also lists the linked Rust crates. FFmpeg source archives and its exact build manifest accompany releases; never substitute an untraceable binary or enable nonfree FFmpeg components. See [FFmpeg compliance notes](docs/ffmpeg-build.md).
