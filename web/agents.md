# Capturefab for agents and shell scripts

Capturefab opens its egui GUI when invoked without a subcommand. Use explicit subcommands for automation. The installed binary's `schema` and `--help` output are authoritative for its supported commands.

```sh
capturefab schema --json
capturefab doctor --json
capturefab --help
```

Successful structured output goes to stdout; diagnostics go to stderr. Check the process exit code before using a result. JSON output is versioned. Keep credentials out of logs, command transcripts, and generated configuration files whenever possible.

## Use an existing visible session

List sessions before opening a separate camera connection. A user may already have the camera connected in the GUI. Camera control is often exclusive.

```sh
capturefab sessions --json
capturefab --session gui status --json
capturefab --session gui connect 192.168.1.10
capturefab --session gui get Width Height ExposureTime --json
capturefab --session gui set ExposureTime=5000
capturefab --session gui capture -n 10 -o frames --json
```

Use the session name returned by `sessions`; it may differ from `gui`. `connect` adds a camera to a persistent session. Use `select` or `--camera` to choose a particular connected camera when multiple cameras are present. Use `rpc` with `schema` for the complete versioned local session interface.

A foreground headless session can also be used:

```sh
capturefab --camera 192.168.1.10 serve --name bench --stream
# In another terminal:
capturefab --session bench status --json
```

## Discover, inspect, configure, acquire

```sh
capturefab discover --json
capturefab --camera 192.168.1.10 features --json
capturefab --camera 192.168.1.10 get Width Height PixelFormat --json
capturefab --camera 192.168.1.10 set ExposureTime=5000
capturefab --camera 192.168.1.10 capture -n 10 -o frames --json
```

Selectors can be a discovered camera ID, unique serial, or GigE IPv4 address. GenICam feature names, access modes, increments, selectors, and bounds depend on the device. Inspect `features` and read back important settings. Capture outputs never silently overwrite existing files. Use `--format raw` when preserving an unsupported pixel format matters.

Use the built-in pattern camera to exercise automation without hardware:

```sh
capturefab --camera sim:0 capture -o pattern.png --json
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

ONVIF locators with credentials use `onvif://USER:PASS@HOST[:PORT]/onvif/device_service`; percent-encode reserved characters in the user name or password. Published session metadata redacts credentials. ONVIF SOAP currently supports HTTP, WS UsernameToken PasswordDigest, and HTTP Basic/Digest authentication. HTTPS SOAP and Media2-only devices are not supported by this implementation. Actual camera coverage is recorded separately in the validation document.

Native camera locators are `avfoundation:0` on macOS, `v4l2:/dev/video0` on Linux, and `dshow:video=Camera Name` on Windows. CSI cameras require an operating system driver exposing a suitable V4L2 device. Native enumeration and capture may require operating system camera permissions.

Native locators accept `?size=WIDTHxHEIGHT&fps=RATE`. Inspect `features` for advertised `VideoMode` choices and set a supported mode, `Width`, `Height`, or `AcquisitionFrameRate` before acquisition; stop a live session before changing them. V4L2 format listings do not establish frame-rate bounds. GigE transport status includes packet size, receive buffer, bounded resend and loss counters; resend recovery has been validated on a physical Basler GigE camera (see docs/hardware-validation.md).

Media inputs and forwarding use bundled FFmpeg in release builds. Inspect `capturefab ffmpeg --json` to see the actual build's protocols and encoders. SRT support depends on the bundled FFmpeg configuration.

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
