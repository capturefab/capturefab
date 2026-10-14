# Capturefab for agents and shell scripts

Capturefab opens its iced GUI when invoked without a subcommand. Use explicit subcommands for automation. The installed binary's `schema` and `--help` output are authoritative for its supported commands.

```sh
capturefab schema --json
capturefab doctor --json
capturefab --help
```

Successful structured output goes to stdout; diagnostics go to stderr. Check the process exit code before using a result. Every `--json` data command, including `schema`, returns `{"version":1,"ok":true,"result":...}` or `{"version":1,"ok":false,"error":{"code":"...","message":"..."}}`. Read the automation contract from `schema --json`'s `result`; plain `schema` prints that contract directly. Its `cli` command tree lists subcommands, options, defaults and accepted values. Keep credentials out of logs, command transcripts, and generated configuration files whenever possible.

Exit codes are 1 `operation_failed`, 2 `usage`, 3 `unavailable`, 4 `timeout`, 5 `unsupported`, and 6 `storage_full`. `--timeout-ms` is a per-frame/transport deadline (1–60000 ms); `--request-timeout 5m` is a session request deadline. The default request deadline adapts to capture count. A client timeout does not cancel an already accepted session command; inspect `status` before retrying writes.

Malformed supplied field values return `usage` (exit 2) before connecting or applying capture/forward `--set` and `--auto`. Live camera, missing destination/drive, storage and I/O failures retain their operation categories. Session commands and RPC envelopes reject unknown fields; use `schema` rather than guessing field names. Rejected writes to auto-managed features leave auto mode enabled. Multiple valid assignments still execute sequentially, so a later live failure does not undo earlier successful writes.

Durations and timestamp fractions must resolve to whole milliseconds (`1.001s` is valid, `1.1ms` is rejected); `--max-age` requires whole seconds, at least `1s`, with delete-oldest. All scheduled frames must fall within 10 years; a past timestamp starts immediately and a zero RPC interval captures consecutive frames. A camera rejects overlapping future output files in its active jobs. Capture paths support only `{frame}` and do not accept network URLs. Recording reservations must fit the requested budget; Capturefab does not silently reduce `--max-file-size`. `record`/`forward --duration 30s` works with both temporary workers and persistent sessions. It keeps the CLI alive and stops the encoder at the deadline or Ctrl-C. Without a duration, session forwarding returns immediately and requires `stop-forward`; direct forwarding stays in the foreground.

Explicit `CAPTUREFAB_MAX_CAMERAS` must be an integer in 1–64. `CAPTUREFAB_FRAME_BYTES` must be 4096–268435456 bytes and a multiple of 64. Malformed overrides fail before worker startup; they do not fall back to defaults.

## Use an existing visible session

Camera/session commands, discovery and network scans automatically attach to the only authenticated active GUI or `serve` session. With none, supported camera commands use a temporary worker. With several, choose `--session NAME` or receive `usage` (exit 2); the CLI never guesses. List sessions to identify a user's visible workbench, or use `--direct --camera ID` for a separate temporary worker. Binary stdout captures use a temporary worker and cannot combine with `--session`; use `--direct` to override an environment session. Camera control is often exclusive.

```sh
capturefab sessions --json
capturefab --session gui status --json
capturefab --session gui connect 192.168.1.10
capturefab --session gui get Width Height ExposureTime --json
capturefab --session gui set ExposureTime=5000
capturefab --session gui capture -n 10 -o frames --json
```

Use the session name returned by `sessions`; it may differ from `gui`. `CAPTUREFAB_SESSION` pins camera/session commands; `CAPTUREFAB_CAMERA` supplies a default camera. `--direct` overrides the environment session and conflicts only with explicit `--session`. Inspection, feature writes, capture, recording, XML and memory commands connect a missing `--camera` source to the session. Persistent controls such as `start` and `auto` require an already connected camera; use `connect ID` first. Desktop and browser camera selection is local to each participant; `select ID` sets the default for CLI calls that omit `--camera`. Use explicit targets when collaborating. Use `rpc` with `schema` for the complete versioned local session interface.

Humans and agents share capture sessions: commands are ordered per camera, cameras run independently, and overlapping schedules share acquisition. Use `--actor NAME --actor-kind agent` (or `CAPTUREFAB_ACTOR` and `CAPTUREFAB_ACTOR_KIND`) to identify an agent; human is the default kind. Desktop and browser activity updates immediately without moving camera selection, drafts or focus. Activity includes `id`, `origin` (`client`/`gui`), `actor` (`id`, `name`, `kind`), `operation`, `camera`, `feature`, `label`, `state`, timestamps, detail and saved files. JSON states are `queued`, `running`, `succeeded`, `failed`; timestamps are Unix milliseconds. `status --json` exposes global activity and `activity_revision`, including when targeted to a busy camera. Read-only observations and authenticated session probes do not create entries. The session retains 200 completed entries plus active commands/jobs across selection and disconnect.

Journal strings/path previews are capped: completed captures retain at most the first eight paths in `files`, while `file_count` gives the actual total; schedule previews advance with acquired frames. The capture command's result still returns its complete file list. Targeted `--camera ID status --json` keeps full coordinator context (`cameras`, `devices`, `network_scan`, logs and activity) with queried-camera fields at the top level. `target_camera` identifies the canonical queried ID; `active_camera` retains the session default for untargeted CLI calls, independently of desktop/browser local selection.

For immediate private IPC observation without an HTTP listener, call `capturefab --session NAME activity --json`, then `activity --after REV --wait 20s --json` with the returned revision. Each call returns one versioned `{revision,activity}` response, including on timeout, bypasses camera queues and creates no activity entry. RPC accepts `{"op":"activity","after":12,"timeout_ms":20000}`.

For immediate agent observation over HTTP, fetch `/api/activity`, then repeat `/api/activity?after=REV&timeout_ms=20000` with the returned `result.revision`. All watchers wake on command and scheduled job changes. The response includes retained history; timeouts return current history, and stale or future revisions return immediately. Maximum timeout is 25000ms. `POST /api/command` optionally accepts `actor:{id,name,kind}` alongside `version`, `camera`, `command`; actor metadata is display attribution, not authentication. See [remote control](https://github.com/capturefab/capturefab/blob/main/docs/remote-control.md). Schedule state `succeeded` acknowledges creation; `entry.job` tracks `id`, `worker_pid`, `status`, `count`, `captured`, `last_file`, `error` through completion/cancellation/failure. Forward success acknowledges encoder start; consult camera status for ongoing forwarding.

Reversible camera changes from GUI, CLI and RPC clients share each camera worker's bounded history. Use `capturefab --session NAME [--camera ID] history --json` to inspect the next undo/redo labels, and `undo` or `redo` to apply them. RPC accepts `{"op":"undo"}` and `{"op":"redo"}`; `status.history` and `cameras[].history` hold the labels. Replay detects conflicting state and retains the entry on failure. History lasts for the camera connection; a new change clears redo. Captures, uploads, recordings, schedules, connection changes, arbitrary commands and raw memory writes cannot be reversed by this camera history. Stopping a stream with schedules or forwarding is also excluded. Inspect the labels before undoing someone else's changes.

Scanner settings are reversible. Scanner acquisition, including Start and Stop, may move or consume physical pages and cannot be undone or replayed.

A foreground headless session can also be used:

```sh
capturefab --camera 192.168.1.10 serve --name bench --stream
# In another terminal:
capturefab --session bench status --json
```

## Apply repeatable JSON workflows

Use workflow files for ordered multi-camera setup and acquisition without shell escaping. [The workflow guide](https://github.com/capturefab/capturefab/blob/main/docs/cli-workflows.md) includes [validated examples](https://github.com/capturefab/capturefab/tree/main/examples/workflows) for timelapse, scanner feeders and bounded recording.

```sh
capturefab workflow validate workflow.json --json
capturefab workflow apply workflow.json --dry-run --json
capturefab --session bench workflow apply workflow.json --json
```

The document is `{"version":1,"steps":[{"command":{"op":"connect","camera":"sim:0","timeout_ms":2000}},{"camera":"sim:0","command":{"op":"capture","output":"shot.png","count":1,"timeout_ms":2000,"format":"png"}}]}`. Each optional step `camera` addresses an existing connection; `connect` and `select` use `command.camera` and must omit the outer target. `discover` and `network_scan` also omit it. Command fields are the same strict RPC fields; CLI conveniences such as `--set`, inferred formats, `--duration` and unit-bearing strings are not workflow fields.

Validation and dry-run are offline, resolve outputs and redact URL credentials. Files/stdin are capped at 1 MiB and 1–256 steps; unknown and duplicate fields fail. Every step passes offline validation before any command runs. Live feature bounds, destinations and output availability remain execution checks. Apply requires a persistent session (automatically selected when there is exactly one), rejects global `--camera`, executes sequentially and stops at the first failure. Earlier effects remain; `error.details.completed`, `completed_steps` and one-based `failed_step` support recovery. Do not blindly replay the full file after a partial failure: captures never overwrite.

Relative local sources and outputs use the workflow file directory, or caller directory for `workflow ... -`. `file:relative.mkv` and `sim:replay:relative-folder` sources resolve in connect/select commands and outer camera targets; bare existing filenames are recognized as file sources. Ordinary CLI/RPC source and output paths use the caller directory. Prefer explicit `file:` for local-source names that could also identify a camera. Network/device locators, destination-relative names and stream URLs stay unchanged. Workflow captures cannot use binary stdout. Scheduling and encoder startup return immediately; keep the session alive and stop them explicitly. For timed recording, use the CLI's `record --duration` rather than adding a nonexistent workflow duration field.

PowerShell can use the same files directly. For stdin, use `Get-Content -Raw workflow.json | capturefab workflow validate - --json`; this changes the output base to the calling directory.

## Discover, inspect, configure, acquire

```sh
capturefab discover --json
capturefab --session bench network-scan --json
capturefab --camera 192.168.1.10 features --json
capturefab --camera 192.168.1.10 get Width Height PixelFormat --json
capturefab --camera 192.168.1.10 set ExposureTime=5000
capturefab --camera 192.168.1.10 capture -n 10 -o frames --json
```

Selectors can be a discovered camera ID, unique serial, or GigE IPv4 address. GenICam feature names, access modes, increments, selectors, and bounds depend on the device. Inspect `features` and read back important settings. Capture outputs never silently overwrite existing files. Known single-file or `{frame}` template extensions infer PNG/JPEG/raw/PGM/PPM; directory sequences default to PNG. `--format` overrides inference. Use `--format raw` when preserving an unsupported pixel format matters.

`network-scan` inventories IPv4 interfaces, rediscovers GigE cameras, checks control reachability and tunes connected GigE cameras with a budget shared per receive interface. RPC: `{"op":"network_scan","timeout_ms":1000}`; result and status retain `network_scan` with `interfaces`, `cameras`, `warnings` and `scanned_at_ms`. Camera entries expose `reachable`, `health` (`unreachable`, `limited`, `unverified`, `ready`, `unstable`), `summary`, `details` and optional `tuning`. Discovery alone never proves a fast or stable stream. Preview streams pause/resume for packet probes; recording, forwarding and active schedules prevent tuning, with an actionable report. Host IPs, routes and MTUs stay unchanged. Per-camera RPC `{"op":"optimize_network","bytes_per_second":100000000}` applies a supplied budget; use `network_scan` for shared-link accounting. These writes are outside undo history; auto mode honors the budget during the connection.

## Camera profiles and specialized acquisition

Camera identity JSON now includes `profile: {kind, source, evidence, capabilities}`. Read it in `discover` result `devices[]`, `info` result `camera`, and `status` result `connected` or `cameras[].info`. Discovery may have only identity hints; connection refines the profile from readable features. Kinds are `unknown`, `area_scan`, `line_scan`, `depth`, `microscope`, `thermal` and `document_scanner`. Sources are `unknown`, `transport`, `identity`, `features` and `device_metadata`. `evidence` is a string array explaining the classification; `capabilities` is an array of advertised control families. `schema --json`'s `result.camera_profiles` describes the enum values and fields. Profiles add metadata to identities; the JSON envelope and capture formats are unchanged.

Detection prioritizes scanner transport, explicit `DeviceType`/`CameraType`/`SensorType` class, explicit `DeviceScanType=Linescan`, specialty feature families, remaining declared scan geometry, model identity, then conventional frame dimensions/timing. `Height=1`, high bit depth and a vendor name alone are not type evidence. A profile does not guarantee a feature exists or is writable: inspect the live feature list, selector values, choices and bounds before setting it.

The GUI puts line geometry/timing and triggers/encoders first for line scan; components, depth range and calibration first for depth; optics, illumination and stage first for microscopes; temperature/radiometric controls first for thermal; and scan controls first for document scanners. Microscope image previews keep the focus region and sharpness tools. Line previews show recent received display rows and gaps for skipped blocks; they can downsample large blocks and are not complete encoder-aligned reconstructions. Scalar depth/thermal previews automatically color the raw sample range and show its bounds. **Sensor pixels** returns to the ordinary preview. Display transformations do not change acquired or raw captured bytes, and captures save individual line blocks rather than their preview history.

For depth and temperature measurements use `--format raw` and separately retain the selected component, unit, scale, offset and calibration features. Raw capture preserves sample bytes and precision; it does not embed dimensions, pixel format or calibration metadata, so retain those separately. PNG, JPEG, PGM, PPM and forwarded video are 8-bit visualizations; forwarding lacks measurement calibration and the GUI color map. A raw-value preview legend is not a calibrated distance or temperature.

Existing transports acquire single rasters. Depth sources need a compatible single-component stream; multipart, GenDC, point-cloud payloads and vendor SDK-only access remain unsupported. Type detection does not establish physical camera compatibility. Generic auto exposure is unsupported for line scan, depth, thermal and document scanner sources; configure their timing or measurement controls directly. Image cameras, including microscopes with suitable advertised controls, retain auto exposure support.

Use the built-in typed cameras to exercise automation without hardware. Selectors `sim:line[:name]`, `sim:depth[:name]`, `sim:microscope[:name]` and `sim:thermal[:name]` accept optional names. `sim:0` and arbitrary other names retain the area-scan pattern camera:

```sh
capturefab --camera sim:0 capture -o pattern.png --json
capturefab --camera sim:line capture --set Height=64 \
  --set AcquisitionLineRate=2000 --set ExposureTime=100 -o line-block.png --json
capturefab --camera sim:depth get PixelFormat Scan3dCoordinateScale \
  Scan3dCoordinateOffset Scan3dCoordinateUnit --json
capturefab --camera sim:depth capture --format raw -o depth.raw --json
capturefab --camera sim:microscope capture --set FocusPosition=50 \
  --set Magnification=4 --set IlluminationIntensity=70 -o micrograph.png --json
capturefab --camera sim:thermal get TemperatureLinearResolution TemperatureUnit --json
capturefab --camera sim:thermal capture --format raw -o thermal.raw --json
```

## Let Capturefab tune exposure and frame rate

```sh
capturefab --session gui auto --balance 0.5 --json
capturefab --session gui status --json
capturefab --session gui manual --json
capturefab --camera 192.168.1.10 capture --auto -o tuned.png --json
```

`auto` is null in manual mode. `stable` and `limited` are settled states; `waiting` means acquisition is stopped. A `set` of a feature in `auto.managed` switches to manual first, and its result includes `"auto": null`. `--set` assignments apply before `--auto`; with `--session`, auto mode stays on afterwards. `capture --auto` still saves if exposure has not settled within half of `--timeout-ms` (at most 3 s); check `result.auto.state`. Settings written by auto mode remain after `manual`; `manual --revert`, disconnect and session exit restore them where possible. Ask the user before enabling auto mode on a camera another application depends on; it can change frame rate, throughput limit and white balance.

## Network and operating system cameras

```sh
capturefab --camera 'rtsp://camera/live' capture -o rtsp.png --json
capturefab onvif discover --json
capturefab onvif resolve 'http://camera/onvif/device_service' --json
capturefab --camera 'onvif:http://camera/onvif/device_service' capture -o onvif.png --json
capturefab native --json
```

ONVIF locators with credentials use `onvif://USER:PASS@HOST[:PORT]/onvif/device_service`; percent-encode reserved characters in the user name or password. Published session metadata redacts credentials. ONVIF SOAP currently supports HTTP, WS UsernameToken PasswordDigest, and HTTP Basic/Digest authentication. HTTPS SOAP and Media2-only devices are not supported by this implementation.

Native camera locators are `avfoundation:0` on macOS, `v4l2:/dev/video0` on Linux, and `dshow:video=Camera Name` on Windows. CSI cameras require an operating system driver exposing a suitable V4L2 device. Native enumeration and capture may require operating system camera permissions.

Native locators accept `?size=WIDTHxHEIGHT&fps=RATE`. Inspect `features` for advertised `VideoMode` choices and set a supported mode, `Width`, `Height`, or `AcquisitionFrameRate` before acquisition; stop a live session before changing them. V4L2 format listings do not establish frame-rate bounds. GigE transport status includes packet size, receive buffer, bounded resend and loss counters.

Media inputs and forwarding use bundled FFmpeg in release builds. Inspect `capturefab ffmpeg --json` to see the actual build's protocols and encoders. SRT support depends on the bundled FFmpeg configuration. `--hwaccel` configures decoding in new direct, GUI or `serve` workers; an existing session retains its setting.

## Scan documents

`capturefab scanners --json` lists platform document scanners; `discover` includes them with `transport: "scanner"`. Use the returned full `scanner:sane:DEVICE` (Linux), `scanner:wia:ID` (Windows), or `scanner:ica:UUID` (macOS) ID. Linux requires SANE `scanimage` and an appropriate backend; Windows requires a WIA driver and Windows PowerShell; macOS uses a bundled ImageCaptureCore helper and compatible driver. `doctor` reports the compiled `scanner` feature and runtime availability. Physical scanner compatibility has not been verified.

```sh
capturefab scanners --json
capturefab --camera 'scanner:sane:DEVICE' --timeout-ms 60000 capture \
  --set ScanResolution=300 --set ScanMode=Gray --set ScanSource=Feeder -n 5 -o pages --json
```

Scanner controls are `ScanResolution` (75–600 dpi, default 150), `ScanMode` (`Color`, `Gray`, `Lineart`, default `Color`) and `ScanSource` (`Default`, `Flatbed`, `Feeder`, default `Default`). Drivers may reject unsupported choices. SANE source strings vary between drivers; `Default` leaves that option unset. `CAPTUREFAB_SCANIMAGE` overrides the executable path. Connecting or `start` never scans; capture requests one page per count, including feeder pages. Use `--timeout-ms 60000` for physical scanning; RPC respects the requested per-page deadline. A timeout cancels the helper. The GUI keeps a separate scanner timeout, initially 60 seconds.

Scans use existing image formats, schedules, destinations and quotas. Multi-page captures save separate files; samples normalize to 8-bit RGB/grayscale with a 128 MiB page bound. PDF/OCR conversion, forwarding and camera auto exposure are not provided for scanner sources. Use full scanner IDs or discover before connecting by serial so the preview ring has its 128 MiB scanner capacity.

## Forward to an existing recorder

```sh
capturefab --camera 192.168.1.10 forward \
  --output rtsp://localhost:8554/camera \
  --encoder auto --fps 30 --bitrate 4M --duration 60
```

A persistent session continues forwarding until stopped:

```sh
capturefab --session gui forward --output rtsp://localhost:8554/camera
capturefab --session gui stop-forward
```

Forwarding probes usable hardware encoders and falls back to software. Destinations include RTSP, SRT, RTMP, UDP, HTTP, and bounded local video files. A destination server or recorder must already exist. Local HLS output is not implemented; an existing server such as MediaMTX can provide HLS from a published stream.

## Save to saved destinations

```sh
capturefab destination list --json
capturefab volumes --json
capturefab --session gui capture -n 10 -o run-1 --destination archive --json
capturefab uploads --json
```

A destination is a named folder or S3-compatible bucket the user saved; with `--destination NAME`, `-o` is a relative name inside it, and the result names the destination and, for buckets, `queued_uploads`. Direct `--camera` captures to a bucket wait for their uploads and add `uploads` to the result. `uploads --json` reports `pending`, `uploading`, `failed` (each with `error`), `uploaded` totals and whether an uploader is running; `uploads retry` and `uploads forget ID` act on failures. Ask the user before creating destinations or storing credentials; never pass secrets on the command line. A missing external drive is exit code 3.

## Schedule captures and respect storage budgets

```sh
capturefab --session gui capture -n 60 -o timed-frames --delay 5s --interval 1m --json
capturefab storage --json
capturefab storage configure --max-space 2GiB --max-files 1000 --on-full stop
```

Schedules belong to the running session and do not survive its exit. The shared private session directory has a persisted storage ceiling, initially 10 GiB and 10,000 Capturefab files. Per-capture limits can lower that ceiling. Recording reservations count against the same budget and a local recording has a separate maximum file size, initially 512 MiB. A storage-full failure uses exit code 6; inspect the status before requesting more space.

Explicit `--delete-oldest` retention only removes completed Capturefab files whose identity and SHA-256 still match. Existing user files, altered captures, active recordings, and unverified interrupted files are protected. Do not raise the global ceiling or enable deletion without the user's authorization. Use the same `CAPTUREFAB_SESSION_DIR` for processes that should share this budget.

## Install the correct release

Read [`releases.json`](releases.json). Its schema is version 1. Each available asset identifies its operating system, architecture, desktop/headless variant, file name, download URL, SHA-256 checksum, and optional signature/application bundle. An empty releases list is not a published release. Assets marked `available: false` must not be offered as downloads. A release may also link `sbom_url`, a CycloneDX software bill of materials, and its files carry GitHub build-provenance and SBOM attestations (`gh attestation verify FILE --repo capturefab/capturefab`).

Use `uname -m` on Linux and macOS or the operating system's system information on Windows. `x86_64` is x64, `aarch64` is ARM64, and `armv7l` is ARMv7. Apple Silicon uses ARM64. Raspberry Pi and Jetson builds must match the installed OS architecture; a 64-bit processor can run a 32-bit OS. Verify the release checksum before running a downloaded binary.

```sh
# Linux
sha256sum DOWNLOADED_FILE
# macOS
shasum -a 256 DOWNLOADED_FILE
# Windows PowerShell
Get-FileHash DOWNLOADED_FILE -Algorithm SHA256
```

Capturefab source is MIT-licensed. The bundled FFmpeg executable is GPLv3; its source, build configuration, and licensing notices are part of the release provenance. See the repository's source and FFmpeg build guide when producing a new platform binary.
