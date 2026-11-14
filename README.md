# Capturefab

Capturefab is a camera capture command-line tool for people, scripts and AI agents. One
`capturefab` executable finds, configures, captures from and records GigE Vision, USB3 Vision
and GenICam cameras (implemented natively in Rust, with no vendor SDK), plus webcams, RTSP/SRT
streams, ONVIF cameras, video files and simulated cameras. Installed vendor GenTL/CTI drivers
add another camera path, including compatible frame grabbers; see the
[camera guide](docs/camera-compatibility.md) for setup and payload limits. Native GigE Vision
synchronized multipart and GenTL multipart local stills retain all components in indexed raw
companions. GenTL supports explicit selection of a 2D image, a supported 16-bit measurement map
or a raw point cloud. A GenTL 1.6 producer can deliver supported GenDC containers for local
still capture. Native GigE Vision also saves ordinary `Coord3D_ABC32f` profiles as RAW. Standard unpacked BiColor RGBG/BGRG 8/10/12 images also convert for capture, inspection and recording.
With `--json`, every command prints
exactly one JSON document and exits with a typed code, so an agent can drive a camera in a few
calls and always knows what happened. It runs on macOS, Windows and Linux and is MIT-licensed.

Industrial signals use `capturefab data record --config sources.json --for 30s -o run.jsonl`.
LMI GoPxL sensor measurements are polled over Ethernet ASCII with declared framing, keeping native ×1000 values and decision bits.
Opto 22 groov EPIC/RIO input capture uses explicit OptoMMP model and channel-type checks, preserving native values and quality codes.
Atlas Copco PF6000 tightening results use the explicit `atlas_open_protocol` profile, preserving native values and raw frames with a JSONL checkpoint before acknowledgment.
Native adapters capture Keyence KV Host Link, LS ELECTRIC XGT/XGB FEnet, Yokogawa FA-M3 PC Link, Modbus TCP with gateway unit selection and optional TLS, Modbus RTU and ASCII, classic S7, Beckhoff ADS, Mitsubishi SLMP 3E binary or ASCII, MC 1E and serial FX Computer Link, Panasonic FP MEWTOCOL-COM, and Omron FINS TCP/UDP, serial Host Link (including explicit legacy CPM/SRM1 areas) and CompoWay/F reads, BACnet/IP meter and controller values, IEC 62056-21 and passive SML optical meter readouts, shared-bus SDI-12 environmental sensors through a USB serial bridge, Vaisala WXT530 serial weather measurements, HART process measurements through a managed Microflex modem, SICK TiM/LMS single scans over configured CoLa A/B, passive SICK multiScan100/picoScan100 Compact scan/IMU datagrams and explicit picoScan150 encoder/mixed UDP streams, LRS4000 Compact TCP scans and multiScan200 Compact TCP scans, ambient light and IMU, passive NMEA GNSS GGA/RMC positions, passive Micro-Epsilon ILD2300 single-DIST1 RS422 raw codes, passive Velodyne VLP-16/HDL-32E raw UDP groups, passive Gocator GDP profile and surface archives, TCP/serial JSONL sensors (Unix terminals or Windows COM ports), and SCPI instruments over raw TCP, native serial, VXI-11, HiSLIP or USBTMC (`usb` feature), METTLER TOLEDO MT-SICS weights over TCP or serial, A&D standard-format Q measurements over configured serial or transparent TCP bridges, Sartorius SBI16/22 manual-print measurements over configured no-flow interfaces,
MQTT/TCP or TLS, Logix atomic tags, explicit DWORD/packed BOOL reads and STRING members, Micro800 numeric and counted STRING values and Omron NJ/NX variables over EtherNet/IP, CIP Assembly data, SLC/MicroLogix PCCC files including whole timer/counter and ST string elements over EtherNet/IP or serial DF1, SLC 5/05 files including whole timer/counter and ST string elements over legacy CSPv4, PLC-5/E PCCC files over EtherNet/IP, OPC-UA reads or explicit queued subscriptions, ifm
IO-Link process data and the standard IO-Link JSON GET interface, camera stills and indexed video. Keyence IV inspection results, Cognex In-Sight cells, Cognex DataMan result bytes, output-only Universal Robots RTDE v2 telemetry, read-only ABB RWS controller/RAPID state, Epson Remote Ethernet monitoring, Yaskawa HSES controller/variable data and native MTConnect agent observations can share the same recording. Passive Livox UDP capture supports explicit SDK2 Mid360/S/L and HAP TX/T1 or legacy Horizon/Avia profiles, with native point/IMU values and immutable raw archives. Mitsubishi SLMP also captures whole iQ-R long-timer records. A camera can save JPEGs while recording video in the same run.
Camera and video sources can configure a supported write-only Action key once on
their shared worker before acquisition, with no automatic key replay after owner loss.
Linux can passively capture EtherCAT, Profinet RT, CAN and CAN FD process data. Camera chunks stay beside local images and video as original payloads with layout and frame indexes,
plus values and units interpreted from the camera XML when available. Recordings share a host
monotonic timeline and
export to CSV or JSONL. See the [industrial data guide](docs/industrial-data.md) for configuration,
clock semantics and current limits. An explicit `helper` source also accepts JSONL readings
from user-supplied vendor SDKs and custom protocol bridges on Unix or Windows. Windows helpers require an absolute `.exe` path and Windows 10/Server 2016 or later.

## Install

```sh
curl -fsSL https://capturefab.com/install.sh | sh        # macOS and Linux
irm https://capturefab.com/install.ps1 | iex             # Windows PowerShell
```

The script checks the download's SHA-256, installs for your account without sudo and links
`capturefab` in `~/.local/bin` (on Windows it adds its folder to your user `PATH`). To install by
hand, download the archive for your system from
[capturefab.com/downloads.html](https://capturefab.com/downloads.html), extract it and put
`capturefab` on your `PATH`. Every download includes FFmpeg. Then run `capturefab doctor`, which
checks this computer and prints a fix for each warning. This README describes the command-line
releases after 0.5.0: `capturefab --version` must report a later version, because 0.5.0 and
earlier are the desktop app, with other commands and JSON.

On macOS the first camera or network command asks for Camera and Local Network access for the
app you run it from, such as Terminal. On Linux, USB3 Vision cameras need the udev rule in the
archive (`INSTALL-USB.txt`).

Download the current installer again to upgrade, or use its uninstall mode:

```sh
curl -fsSL https://capturefab.com/install.sh | sh -s -- --upgrade
curl -fsSL https://capturefab.com/install.sh | sh -s -- --uninstall
```

On Windows, download `install.ps1` and run it with `-Upgrade` or `-Uninstall`.
Use the same `--prefix` and `--bin-dir` (`-Prefix` on Windows) for a custom install.
Uninstall removes recognized release folders and their command link or PATH entries;
presets, sign-ins, caches and files outside those folders stay. `--dry-run` (`-DryRun`)
previews either operation. Close running commands and sessions before uninstalling.

Copy this into an AI agent, or use the copy button on the
[downloads page](https://capturefab.com/downloads.html#install-cli):

```text
Install or upgrade Capturefab for my user account from https://capturefab.com.
Download and inspect install.sh (macOS/Linux) or install.ps1 (Windows) from that site.
Run it with --upgrade (PowerShell: -Upgrade), without administrator rights.
Preserve captures, presets and saved sign-ins. Make capturefab available in this terminal.
Verify capturefab --version and capturefab --json doctor, then read
https://capturefab.com/agents.md and capturefab schema before using camera commands.
Report the installed version, path and any setup fixes needed.
```

## Quick start

`sim:0` is a built-in pattern camera, so every command below works without hardware.

```sh
capturefab list --kind sim                        # cameras; plain `list` finds real ones
capturefab info -c sim:0                          # size, format, exposure, gain, trigger, controls
capturefab capture -c sim:0 -o shot.png           # one frame; never overwrites a file
capturefab capture -c sim:0 -n 10 -o frames/      # a burst into a folder
capturefab inspect -c sim:0                       # exposure, clipping, focus and a preview image
capturefab set -c sim:0 exposure=5ms gain=3       # one atomic change; a failure restores every value
capturefab record -c sim:0 -o clip.mp4 --for 10s  # H.264 video
capturefab watch -c sim:0 --show                  # live measurements and the frame, redrawn in a terminal
capturefab shell -c sim:0                         # a prompt that keeps the camera connected
```

`capturefab COMMAND --help` shows each command's options and examples.

## For AI agents and scripts

[web/agents.md](web/agents.md) (also at [capturefab.com/agents.md](https://capturefab.com/agents.md))
gives the call patterns. The contract in short:

- **`--json`** (or `CAPTUREFAB_JSON=1`) prints exactly one single-line JSON document on stdout.
  Exit 0: the result. Non-zero: `{"error":{"code","message","hint"?,"details"?}}`. Diagnostics go
  to stderr. Global flags may follow the command: `capturefab capture --json -c sim:0 -o shot.png`.
  The two streaming commands print one JSON line each instead: `watch` per measurement and
  `shell` per command.
- **Exit codes** are the category: `0` ok, `1` failed, `2` usage or input, `3` unavailable,
  `4` timeout, `5` unsupported, `6` storage full, `130` interrupted (143 SIGTERM, 129 SIGHUP).
  Storage is full when a write would take the disk below its free-space floor: 1% of the disk,
  at least 512 MiB and at most 20 GiB, or `CAPTUREFAB_MIN_FREE` (`0` turns it off).
- **`error.code`** is the specific reason, such as `camera_not_found`, `camera_ambiguous`
  (`details.candidates`), `feature_not_found` (`details.did_you_mean`), `out_of_range`
  (`details.min`, `max`, `nearest`), `exists`, `frame_timeout` or `trigger_timeout`.
- **`capturefab schema`** lists every command with its effect (`read`, `camera_write`,
  `file_write` or `disruptive`), the conventions, value grammars, exit codes and error codes;
  `capturefab schema COMMAND` gives one command's arguments, result fields and errors. The
  [command reference](https://capturefab.com/reference.html) is generated from it.
- **Cameras**: `-c SELECTOR` takes a `list` ID (`gige:…`, `u3v:…`, `avfoundation:…`, `v4l2:…`,
  `dshow:…`), a serial number, an IP address, `rtsp://`, `srt://`, `http(s)://`, `onvif://host`,
  a file path, `sim:N` or `sim:replay:PATH`. Without `-c` the only camera found is used; several
  give exit 2 `camera_ambiguous` with the candidates (at a terminal without `--json`, a numbered
  list asks which).

- **Opaque registers**: `debug read --feature NAME` reads a complete buffer and
  `debug write --feature NAME --hex HEX` writes it, up to 64 KiB. Ordinary `get` reports
  only the declared length. For large writes, send a `write_register` JSON request on
  stdin with `debug rpc` to avoid command-line size limits. See the [camera guide](docs/camera-compatibility.md#opaque-genicam-registers).

- **Names**: GenICam features (`ExposureTime`), banks (`TriggerMode[FrameStart]`) and portable
  controls (`exposure`, `gain`, `fps`, `roi`, `format`, `trigger`, `auto_exposure`, `auto_wb`).
  Quote banks in the shell (`'TriggerMode[FrameStart]'`; zsh expands brackets), and quote camera
  IDs with spaces (`-c 'avfoundation:HD Pro Webcam C920'`).
- **Durations** need a unit: `250ms`, `2s`, `1h30m`, and `none` where a wait may be unlimited.
  A bare number such as `5000` is exit 2 `invalid_duration`.
- **Captures** report which frame they saved (`time`, and `device_ts_ns` from the camera's clock)
  and what the camera delivered and lost meanwhile (`frames`, `dropped`, `incomplete`). The first
  frame saved is one exposed after the last settings change; `--fresh N` lets N more pass, and a
  camera that sets its own exposure gets `--warmup` (default 2s) to settle. `--inspect` adds the
  last frame's measurements, `--manifest FILE` appends a JSON line per file (path, time, size,
  SHA-256, settings, tags), and `--sync none` skips flushing scratch files to the disk. With `-c`
  repeated, the cameras are asked for a frame together and `skew_ms` says how far apart their
  first frames arrived.
- **Output templates** for `-o`: `{camera}`, `{serial}`, `{seq}`, `{seq:4}`, `{time}`, `{date}`,
  `{tag:KEY}`, `{ext}`. A folder holds `{camera}-{time}-{seq}.{ext}`. An existing file is never
  overwritten: exit 1 `exists`, unless the name has `{seq}` or `--on-exists number` is given.

## Sessions and jobs

Each camera runs in a worker process of its own. A **session** keeps cameras open between
commands, so repeated calls skip connecting and starting the stream:

- Every camera command keeps its camera warm by default: it starts (or reuses) the `auto` session,
  which holds the camera until it has been unused for 60 seconds. `--keep 5m` holds it longer;
  `--keep 0`, `CAPTUREFAB_KEEP=0` or `--no-session` open and close the camera in the command
  alone, as does a computer without a writable runtime folder. Results are the same either way.
- `capturefab shell` runs many commands with the cameras kept connected: a prompt with history and
  Tab completion in a terminal, or one command per line of stdin (or `-e`), one result per line.
- `capture --background` and `record --background` return a job once it is armed or has its
  first frame, which on a triggered camera waits for a trigger up to `--timeout`: add
  `--timeout none` to return without one. `capturefab jobs` lists jobs (`jobs list ID` one of
  them), `jobs wait ID...` waits for them and `jobs stop ID` ends one; files saved so far stay.
- `capturefab session start -c CAM --live` starts a named session in the background, and
  `capturefab serve` runs one in the foreground, for example under systemd (see the
  [line computer guide](docs/line-computer.md)). Commands use a session that holds their camera
  on their own; `--session NAME` picks one and `--no-session` never uses one. A session holds
  the cameras it was started with as long as it runs, and a camera a command added with `--keep`
  until that camera has been unused that long. `session stop` ends a session at once: its running
  jobs end as `interrupted`, keeping the files saved so far.

## Cameras and sources

| Source | `-c` selector |
| --- | --- |
| GigE Vision and USB3 Vision (GenICam) | `list` ID, serial number or IP address |
| Receive an externally controlled GigE multicast stream | `gige-monitor://CAMERA_IP?interface=HOST_IP&mac=12_HEX_DIGITS` (quote the selector; see the camera guide) |
| Webcams and capture cards (AVFoundation, V4L2, DirectShow) | `list` ID |
| RTSP, SRT and HTTP streams; ONVIF cameras | `rtsp://…`, `srt://…`, `http://…`, `onvif://host` |
| Video and image files | a path |
| Simulated cameras | `sim:0`, `sim:line`, `sim:depth`, `sim:microscope`, `sim:thermal`, `sim:replay:PATH` |

Frames in every common PFNC pixel format are converted: monochrome from 1 to 16 bits (packed or
not), Bayer at 8 to 16 bits with its packed variants, RGB/BGR at 8 to 16 bits (interleaved,
planar, packed, 565 and alpha-padded) and YUV/YCbCr 4:4:4, 4:2:2 and 4:1:1. PNG keeps monochrome
samples wider than 8 bits as 16-bit gray; color and Bayer PNG and JPEG are 8-bit, and the result
says so (`bit_depth`, `source_bits`). `-f raw` saves the frame's exact bytes with a `NAME.json`
sidecar naming the pixel format, its PFNC code and the row size (each plane's for planar
layouts). Clipping counts and auto exposure metering use the frame's own bit depth.

Webcams, streams and files arrive in the format FFmpeg decodes them to: gray, YUV 4:2:0, 4:2:2
or 4:4:4 (planar, NV12 or YUYV/UYVY) with its color range and matrix, or RGB for RGB sources, so
`-f raw` saves those bytes. Where PFNC has no code for a layout (planar Y'CbCr, limited-range
NV12, full-range BT.709) the pixel format is one of Capturefab's own, named in PFNC's style:
`YCbCr601_420_8_Planar` is limited-range BT.601 4:2:0 in three planes, `YCbCr709_…` limited-range
BT.709, `YCbCr709Full_…` full-range BT.709 and plain `YCbCr…` full-range BT.601. Other color
spaces are read as RGB. In a session, the warm one included, compressed network streams and
webcams in a compressed mode other than Motion JPEG are decoded in hardware where it works
(`CAPTUREFAB_HWDEC`), as the decoder runs on between commands; files, and commands with
`--no-session`, decode in software, which delivers one frame sooner.

JPEG stills of color frames go to Apple's JPEG engine on macOS where it is faster and as accurate
as libjpeg-turbo (checked beside the first frame of each size and format, which libjpeg-turbo
writes; frames under 512K pixels, monochrome frames and `--quality` other than 90 stay on
libjpeg-turbo); `CAPTUREFAB_JPEG` chooses. On Linux, NVIDIA's nvJPEG (CUDA) or a VA-API
driver's JPEG encoder takes frames of at least 512K pixels, of any format and quality, on the
same terms: the camera worker starts a helper process that loads the vendor's libraries, reads
each frame from the shared memory the camera wrote it to, and is stopped after a minute unused.
Under `auto`, a helper that crashes or stops answering is ended, its frame and later ones are
encoded by libjpeg-turbo, and the worker says so on stderr.
A Motion JPEG webcam, stream or file is read as the JPEGs it sends, decoded with libjpeg-turbo:
a JPEG still of it without `--quality` is that JPEG as it was, with the provenance comment
(`"passthrough": true`).
`record --codec ffv1` of a Bayer camera stores the mosaic itself, losslessly, as 8- or 16-bit
gray, which players show in gray. `record` of an H.264 or HEVC stream, file or webcam mode copies
its video as it arrives (`"encoder":"copy"`, in the source's codec), starting at its last key
frame, unless `--codec` names another codec or `--fps` or `--bitrate` is given; on Windows it is
encoded. The source is still decoded meanwhile, for captures and inspection.

Support depends on the device, its firmware, the connection and the operating system; this is an
early implementation with less coverage than vendor SDKs. Sign-ins for password-protected streams
are saved in the OS credential store with `capturefab login add`. `capturefab network` checks GigE
network paths. `capturefab network force-ip` assigns a closed camera an explicit temporary
IPv4 configuration by MAC on a selected interface; see the compatibility guide for its
configuration and ownership limits. `capturefab network action` sends one action
on an explicit interface, with acknowledged and sent-only modes. It is immediate by default;
`--action-time-ticks` requests an exact absolute device timestamp without clock qualification.
A held camera's write-only
action key uses the separate `set --write-only` operation; configure and arm capture before
issuing an action. More: [CLI workflows](docs/cli-workflows.md),
[camera compatibility notes](docs/camera-compatibility.md) (with the
[pixel format table](docs/camera-compatibility.md#pixel-formats)),
[simulated cameras](docs/simulation.md) and [embedded and read-only hosts](docs/embedded.md).

## Configuration

| Variable | Purpose |
| --- | --- |
| `CAPTUREFAB_JSON=1` | `--json` for every command |
| `CAPTUREFAB_CAMERA` | the default `-c` |
| `CAPTUREFAB_SESSION` | the default `--session` |
| `CAPTUREFAB_KEEP` | the default `--keep` (60s); `0` turns the warm default off |
| `CAPTUREFAB_CAMERA_PASSWORD` | the password for `login add` without `--password-stdin` |
| `CAPTUREFAB_FFMPEG` | an FFmpeg executable to use instead of the bundled one |
| `CAPTUREFAB_SESSION_DIR` | one folder for sessions, presets and saved sign-in records, for isolated runs and tests; it must be private (mode 0700, as `mktemp -d` makes) or not exist yet |
| `CAPTUREFAB_STATE_DIR` | the folder for presets, saved sign-in records and the jobs of ended sessions alone, such as a data partition on a read-only system |
| `CAPTUREFAB_CACHE_DIR` | the folder for the extracted FFmpeg, encoder probes and GenICam XML |
| `CAPTUREFAB_MIN_FREE` | the free space a write must leave on a disk, in bytes or a size such as `2G`; `0` turns the floor off |
| `CAPTUREFAB_THREADS` | at most this many image processing threads |
| `CAPTUREFAB_IMAGES` | how `--show` draws frames: `kitty`, `iterm`, `truecolor`, `256` or `none` |
| `CAPTUREFAB_MAX_CAMERAS` | cameras one session may hold, 1–64 (default 16) |
| `CAPTUREFAB_GVSP_SCATTER=0` | GigE Vision packets are received whole and copied into their frames, rather than received straight into them |
| `CAPTUREFAB_JPEG` | the JPEG encoder: `auto` (default), `cpu` (libjpeg-turbo), `videotoolbox` (macOS), `nvjpeg` or `vaapi` (Linux); a named engine that is missing or fails its accuracy check is exit 3 `unavailable` |
| `CAPTUREFAB_CUDA_LIB_DIR` | a folder holding the CUDA runtime and nvJPEG libraries, tried before the loader's paths |
| `CAPTUREFAB_VAAPI_DEVICE` | the DRM render node VA-API JPEG encoding uses, instead of the first `/dev/dri/renderD*` that encodes JPEG |
| `CAPTUREFAB_HWDEC` | hardware decoding of H.264, HEVC and other compressed video: `auto` (default: streams and webcams, in a session), `none`, or a method such as `videotoolbox`, `cuda`, `vulkan` or `d3d11va`, used for files and `--no-session` too; one this FFmpeg lacks or that fails its probe is exit 3 `unavailable`. Motion JPEG is decoded with libjpeg-turbo whatever it says |

A session's cameras keep the `CAPTUREFAB_JPEG`, `CAPTUREFAB_HWDEC` and `CAPTUREFAB_FFMPEG` the
session started with, the warm `auto` session included: stop it (`capturefab session stop auto`)
or use `--no-session` for a command to use other values. A command's own `CAPTUREFAB_MIN_FREE`
raises the free-space floor for the files a session writes for it, but does not lower the floor
the session started with.

`capturefab --json doctor` prints the folders in use and the fallbacks it passed over
(`folders`), the private folder that holds `inspect` previews (`inspect_dir`), the JPEG encoders
(`jpeg`) and the hardware decoding methods that work here (`hwdec`). Where a default
folder cannot be written, as on a read-only root file system, Capturefab falls back to a private
folder in a tmpfs and turns off only what needs the missing folder; see
[embedded and read-only hosts](docs/embedded.md).

## Build from source

Requirements: a stable Rust toolchain, a C compiler and CMake (and NASM on x86, for
libjpeg-turbo). Webcams, streams, files and recording need FFmpeg. `build.rs` never builds it: a
build embeds the FFmpeg named by `CAPTUREFAB_FFMPEG_BINARY` at build time, and otherwise a release
build embeds `target/ffmpeg/<target>/bin/ffmpeg` when it exists, which
`scripts/build-ffmpeg.sh` builds from the pinned GPL sources that releases embed (see the
[FFmpeg build guide](docs/ffmpeg-build.md)); debug builds embed nothing. At run time Capturefab
uses `CAPTUREFAB_FFMPEG`, else the embedded copy, else an `ffmpeg` next to the executable or on
`PATH`; `capturefab doctor` says which, and commands that need FFmpeg without one fail with
`ffmpeg_missing`.

```sh
cargo build --release
target/release/capturefab capture -c sim:0 -o first.png
cargo test
cargo clippy --all-targets -- -D warnings
python3 scripts/test-site.py && python3 -m unittest discover -s scripts
```

The default features are `usb` (USB3 Vision), `gentl` (installed CTI camera drivers), `ni-daqmx` (installed NI input drivers on supported Linux/Windows hosts), `jpeg` (libjpeg-turbo), `videotoolbox` (Apple's
JPEG engine on macOS; nothing elsewhere), `nvjpeg` and `vaapi` (the GPU JPEG encoders on Linux,
whose libraries are loaded at run time, so the build needs no SDK; nothing elsewhere), `keychain`
(the OS credential store for `login`), `shell` (line editing, history and completion in
`capturefab shell`) and `mqtt-tls` (verified TLS for industrial data sources); `--no-default-features` builds without them. Releases are built
and packaged as described in [docs/release.md](docs/release.md).

The `ni-daqmx` feature, included by default, adds continuous internal-clock or
explicit software on-demand voltage, thermocouple, current, RTD, strain,
bridge-ratio, acceleration, resistance, thermistor, physical bridge or mixed AI
input through an installed NI-DAQmx
runtime on supported Linux/Windows hosts. An isolated worker preserves exact
F64 words, native statuses and explicit per-channel units, CJC, shunt, R0,
wiring, sensor and excitation requests. Mixed mode binds each column's actual
channel, quantity, units and settings without changing legacy rows or removing
repeated values; a shared scan does not prove simultaneous or fresh conversions.

Strain and bridge ratio retain native scaling; acceleration supports g, m/s²
and in/s² without local unit or sensitivity conversion. Optional AC/DC/ground
coupling and explicit internal/external/none excitation remain requests, not
readbacks or calibration. Internal excitation can energize the sensor, and
Stop/Clear or none does not guarantee physical power-off.

The six additional AI profiles `resistance`, `thermistor_current`,
`thermistor_voltage`, `force_bridge`, `pressure_bridge` and `torque_bridge` also
work in MixedAI, with the same per-channel request contract. Resistance keeps
native ohms; thermistors require explicit manufacturer coefficients and the
voltage-excited mode adds reference R1 ohms. Force/pressure/torque requests keep
returned units, electrical calibration units and physical calibration units
separate. The native driver performs the scaling; Capturefab does no local
thermal equation or interpolation. All settings and declared calibration points
remain requests, with no physical sensor, calibration or device qualification.

Additional `velocity_iepe`, `force_iepe`, `charge_acceleration`, `lvdt` and `rvdt` measurements support both AI timing modes and MixedAI, now with eighteen channel kinds. Output units and sensitivity units are separate explicit requests: velocity m/s can use mV/(mm/s), charge acceleration uses pC sensitivity, LVDT includes mV/V per milli-inch, and RVDT uses angular sensitivity. The first three require terminal configuration; LVDT/RVDT instead require explicit AC excitation frequency and four/five/six-wire mode. All five preserve driver F64 words and finite velocity/force/acceleration/position/angle without local conversion. Sensitivity accepts finite signed and zero requests with native rejection preserved; older acceleration keeps its positive-only rule. Excitation and optional coupling remain requests, with no inferred calibration, delivered power or physical device qualification.

Software-only `digital_port`, `counter_count`, `counter_frequency`,
`counter_period`, `counter_pulse_width`, `counter_angular_position` and
`counter_linear_position` modes retain native U32 states/count snapshots or
F64 frequency/time/position values. Each counter task has one explicit typed
channel. Encoder PPR, distance, initial position and all Z-index fields remain
requests; enabled Z can reset the measurement, with phase ignored in two-pulse
mode. No masks, count unwrapping or host-derived rate/position is inferred.
On-demand requires an explicit poll interval and one scan per read, without
hardware-rate, freshness or device-qualification claims. See [NI input configuration](docs/industrial-data.md#ni-daqmx-analog-voltage-acquisition).

## License

Capturefab is MIT-licensed. Release downloads embed an FFmpeg executable that is GPLv3, with x264,
SRT, OpenSSL and zlib; it runs as a separate process, and each release publishes its
corresponding source. `capturefab licenses` prints both notices.
