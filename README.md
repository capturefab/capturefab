# Capturefab

Capturefab is a camera capture command-line tool for people, scripts and AI agents. One
`capturefab` executable finds, configures, captures from and records GigE Vision, USB3 Vision
and GenICam cameras (implemented natively in Rust, with no vendor SDK), plus webcams, RTSP/SRT
streams, ONVIF cameras, video files and simulated cameras. With `--json`, every command prints
exactly one JSON document and exits with a typed code, so an agent can drive a camera in a few
calls and always knows what happened. It runs on macOS, Windows and Linux and is MIT-licensed.

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
  give exit 2 `camera_ambiguous` with the candidates.
- **Names**: GenICam features (`ExposureTime`), banks (`TriggerMode[FrameStart]`) and portable
  controls (`exposure`, `gain`, `fps`, `roi`, `format`, `trigger`, `auto_exposure`, `auto_wb`).
  Quote banks in the shell (`'TriggerMode[FrameStart]'`; zsh expands brackets), and quote camera
  IDs with spaces (`-c 'avfoundation:HD Pro Webcam C920'`).
- **Durations** need a unit: `250ms`, `2s`, `1h30m`, and `none` where a wait may be unlimited.
  A bare number such as `5000` is exit 2 `invalid_duration`.
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
- `capture --background` and `record --background` return a job at once. `capturefab jobs` lists
  jobs, `jobs wait ID` waits for one and `jobs stop ID` ends it; files saved so far stay.
- `capturefab session start -c CAM --live` starts a named session in the background, and
  `capturefab serve` runs one in the foreground, for example under systemd (see the
  [line computer guide](docs/line-computer.md)). Commands use a session that holds their camera
  on their own; `--session NAME` picks one and `--no-session` never uses one.

## Cameras and sources

| Source | `-c` selector |
| --- | --- |
| GigE Vision and USB3 Vision (GenICam) | `list` ID, serial number or IP address |
| Webcams and capture cards (AVFoundation, V4L2, DirectShow) | `list` ID |
| RTSP, SRT and HTTP streams; ONVIF cameras | `rtsp://…`, `srt://…`, `http://…`, `onvif://host` |
| Video and image files | a path |
| Simulated cameras | `sim:0`, `sim:line`, `sim:depth`, `sim:microscope`, `sim:thermal`, `sim:replay:PATH` |

Frames in every common PFNC pixel format are converted: monochrome from 1 to 16 bits (packed or
not), Bayer at 8 to 16 bits with its packed variants, RGB/BGR at 8 to 16 bits (interleaved,
planar, packed, 565 and alpha-padded) and YUV/YCbCr 4:4:4, 4:2:2 and 4:1:1. PNG keeps monochrome
samples wider than 8 bits as 16-bit gray; color and Bayer PNG and JPEG are 8-bit, and the result
says so (`bit_depth`, `source_bits`). `-f raw` saves the frame's exact bytes with a `NAME.json`
sidecar naming the pixel format, its PFNC code and the row size. Clipping counts and auto
exposure metering use the frame's own bit depth.

Webcams, streams and files arrive in the format FFmpeg decodes them to: gray, YUV 4:2:0, 4:2:2
or 4:4:4 (planar, NV12 or YUYV/UYVY) with its color range and matrix, or RGB for RGB sources, so
`-f raw` saves those bytes. Where PFNC has no code for a layout (planar Y'CbCr, limited-range
NV12, full-range BT.709) the pixel format is one of Capturefab's own, named in PFNC's style:
`YCbCr601_420_8_Planar` is limited-range BT.601 4:2:0 in three planes, `YCbCr709_…` limited-range
BT.709, `YCbCr709Full_…` full-range BT.709 and plain `YCbCr…` full-range BT.601. Other color
spaces are read as RGB. In a session, the warm one included, compressed network streams and
webcams in a compressed mode are decoded in hardware where it works (`CAPTUREFAB_HWDEC`), as
the decoder runs on between commands; files, and commands with `--no-session`, decode in software,
which delivers one frame sooner.

JPEG stills of color frames go to Apple's JPEG engine on macOS where it is faster and as accurate
as libjpeg-turbo (checked beside the first frame of each size and format, which libjpeg-turbo
writes; frames under 512K pixels, monochrome frames and `--quality` other than 90 stay on
libjpeg-turbo); `CAPTUREFAB_JPEG` chooses.
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
network paths. More: [CLI workflows](docs/cli-workflows.md),
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
| `CAPTUREFAB_JPEG` | the JPEG encoder: `auto` (default), `cpu` (libjpeg-turbo) or `videotoolbox`; a named engine that is missing or fails its accuracy check is exit 3 `unavailable` |
| `CAPTUREFAB_HWDEC` | hardware decoding: `auto` (default: streams and webcams, in a session), `none`, or a method such as `videotoolbox`, `cuda`, `vulkan` or `d3d11va`, used for files and `--no-session` too; one this FFmpeg lacks or that fails its probe is exit 3 `unavailable` |

A session's cameras keep the `CAPTUREFAB_JPEG` and `CAPTUREFAB_HWDEC` the session started with,
the warm `auto` session included: stop it (`capturefab session stop auto`) or use `--no-session`
for a command to use other values.

`capturefab --json doctor` prints the folders in use (`dirs`, and under `folders` the fallbacks
it passed over), including the private folder that holds `inspect` previews, the JPEG encoders
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

The default features are `usb` (USB3 Vision), `jpeg` (libjpeg-turbo), `videotoolbox` (Apple's
JPEG engine on macOS; nothing elsewhere), `keychain` (the OS credential store for `login`) and
`shell` (line editing, history and completion in `capturefab shell`); `--no-default-features`
builds without them. Releases are built
and packaged as described in [docs/release.md](docs/release.md).

## License

Capturefab is MIT-licensed. Release downloads embed an FFmpeg executable that is GPLv3, with x264,
SRT, OpenSSL and zlib; it runs as a separate process, and each release publishes its
corresponding source. `capturefab licenses` prints both notices.
