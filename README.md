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
checks this computer and prints a fix for each warning.

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
capturefab inspect -c sim:0 --keep 5m             # keep the camera open, so the next call starts at once
```

`capturefab COMMAND --help` shows each command's options and examples.

## For AI agents and scripts

[web/agents.md](web/agents.md) (also at [capturefab.com/agents.md](https://capturefab.com/agents.md))
gives the call patterns. The contract in short:

- **`--json`** (or `CAPTUREFAB_JSON=1`) prints exactly one single-line JSON document on stdout.
  Exit 0: the result. Non-zero: `{"error":{"code","message","hint"?,"details"?}}`. Diagnostics go
  to stderr. Global flags may follow the command: `capturefab capture --json -c sim:0 -o shot.png`.
- **Exit codes** are the category: `0` ok, `1` failed, `2` usage or input, `3` unavailable,
  `4` timeout, `5` unsupported, `6` storage full, `130` interrupted (143 SIGTERM, 129 SIGHUP).
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
- **Durations** need a unit: `250ms`, `2s`, `1h30m`, and `none` where a wait may be unlimited.
  A bare number such as `5000` is exit 2 `invalid_duration`.
- **Output templates** for `-o`: `{camera}`, `{serial}`, `{seq}`, `{seq:4}`, `{time}`, `{date}`,
  `{tag:KEY}`, `{ext}`. A folder holds `{camera}-{time}-{seq}.{ext}`. An existing file is never
  overwritten: exit 1 `exists`, unless the name has `{seq}` or `--on-exists number` is given.

## Sessions and jobs

Each camera runs in a worker process of its own. A plain command opens the camera, does its work
and closes it again. A **session** keeps cameras open between commands:

- `--keep 5m` on a camera command starts (or reuses) a session that holds the camera until it has
  been unused for five minutes, so repeated calls skip connecting and starting the stream.
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

Support depends on the device, its firmware, the connection and the operating system; this is an
early implementation with less coverage than vendor SDKs. Sign-ins for password-protected streams
are saved in the OS credential store with `capturefab login add`. `capturefab network` checks GigE
network paths. More: [CLI workflows](docs/cli-workflows.md),
[camera compatibility notes](docs/camera-compatibility.md) and
[simulated cameras](docs/simulation.md).

## Configuration

| Variable | Purpose |
| --- | --- |
| `CAPTUREFAB_JSON=1` | `--json` for every command |
| `CAPTUREFAB_CAMERA` | the default `-c` |
| `CAPTUREFAB_SESSION` | the default `--session` |
| `CAPTUREFAB_KEEP` | the default `--keep` |
| `CAPTUREFAB_CAMERA_PASSWORD` | the password for `login add` without `--password-stdin` |
| `CAPTUREFAB_FFMPEG` | an FFmpeg executable to use instead of the bundled one |
| `CAPTUREFAB_SESSION_DIR` | one folder for sessions, presets and saved sign-in records, for isolated runs and tests |
| `CAPTUREFAB_CACHE_DIR` | the folder for the extracted FFmpeg and encoder probes |
| `CAPTUREFAB_THREADS` | at most this many image processing threads |
| `CAPTUREFAB_MAX_CAMERAS` | cameras one session may hold, 1–64 (default 16) |

`capturefab --json doctor` prints the folders in use (`dirs`), including the private folder that
holds `inspect` previews.

## Build from source

Requirements: a stable Rust toolchain, a C compiler and CMake (and NASM on x86, for
libjpeg-turbo). Webcams, streams, files and recording need FFmpeg: `scripts/build-ffmpeg.sh`
builds the pinned GPL build that releases embed (see the [FFmpeg build guide](docs/ffmpeg-build.md)),
or set `CAPTUREFAB_FFMPEG` to one.

```sh
cargo build --release
target/release/capturefab capture -c sim:0 -o first.png
cargo test
cargo clippy --all-targets -- -D warnings
python3 scripts/test-site.py && python3 -m unittest discover -s scripts
```

The default features are `usb` (USB3 Vision), `jpeg` (libjpeg-turbo) and `keychain` (the OS
credential store for `login`); `--no-default-features` builds without them. Releases are built
and packaged as described in [docs/release.md](docs/release.md).

## License

Capturefab is MIT-licensed. Release downloads embed an FFmpeg executable that is GPLv3, with x264,
SRT, OpenSSL and zlib; it runs as a separate process, and each release publishes its
corresponding source. `capturefab licenses` prints both notices.
