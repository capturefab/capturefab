# Capturefab for AI agents

`capturefab` is a camera capture command-line tool. An agent drives it through the shell:
every command takes `--json`, prints exactly one JSON document on stdout and exits with a code
that says what kind of outcome it was. This page gives the fastest correct call for common
tasks. The installed binary is the source of truth: `capturefab schema` describes every command,
and `capturefab schema COMMAND` gives one command's arguments, result fields and error codes.
The [command reference](reference.html) is generated from the same output.

If `capturefab` is not installed: `curl -fsSL https://capturefab.com/install.sh | sh` (macOS,
Linux) or `irm https://capturefab.com/install.ps1 | iex` (Windows). It installs for the current
user without sudo. `capturefab --version` and `capturefab --json doctor` confirm it runs. This
page describes the command-line releases after 0.5.0; version 0.5.0 and earlier are the desktop
app, whose commands and JSON differ, so update a `capturefab` that reports one of them.

## Rules

1. **Always pass `--json`.** Parse stdout as one JSON document. Exit 0 means it is the result;
   any other exit means it is `{"error":{"code","message","hint"?,"details"?}}`. Results never
   have a top-level `error` key. Logs and progress go to stderr; ignore them. The two streaming
   commands print one JSON document per line instead: `watch` per measurement, `shell` per
   command.
2. **Branch on the exit code, then `error.code`.** The exit code is the category: `0` ok,
   `1` failed, `2` usage or input (fix the command), `3` unavailable (camera, session or
   FFmpeg), `4` timeout, `5` unsupported, `6` storage full, `130` interrupted. `error.code` is
   the specific reason, and `error.hint` says what to do next.
3. **Do not guess flags.** Read `capturefab schema` once (about 8 KB, roughly 2,500 tokens), then
   `capturefab schema COMMAND` for the commands you use. Unknown flags are exit 2 `usage`.
4. **Durations need a unit**: `250ms`, `2s`, `1.5s`, `5m`, `1h30m`. `--timeout none` waits
   without limit. A bare `5000` is exit 2 `invalid_duration`.
5. **Name the camera** with `-c` on every camera command. `list` IDs work everywhere. Quote
   arguments for the shell: webcam IDs contain spaces (`-c 'avfoundation:HD Pro Webcam C920'`),
   and zsh, the macOS default shell, expands bracketed names unless they are quoted
   (`'TriggerMode[FrameStart]'`).
6. **Files are never overwritten.** A taken name is exit 1 `exists` with `details.path`; add
   `{seq}` to `-o` or pass `--on-exists number`.
7. **Paths in results are absolute.** Relative `-o` paths resolve against your working
   directory, also when a session does the work.

## Permissions

Put the command first and flags after it, so a harness can allow commands by prefix:
`capturefab capture --json -c sim:0 -o shot.png`. `capturefab schema` gives each command's
`effect` when it runs without the flags below (`commands` maps a name to `[effect, summary]`,
`effect_with` the widening flags, and `errors` each `error.code` to `[exit, meaning]`):

| Effect | Commands | Suggested policy |
| --- | --- | --- |
| `read` | `list`, `info`, `features`, `get`, `inspect`, `watch`, `jobs`, `jobs list`, `jobs wait`, `session status`, `session list`, `preset list`, `preset show`, `network`, `doctor`, `schema`, `onvif resolve`, `login list`, `debug xml`, `debug read`, `licenses`, `completions` | allow |
| `file_write` | `capture`, `record`, `preset save`, `preset rm`, `preset import`, `preset export`, `login add`, `login rm` | allow in the task's folder |
| `camera_write` | `set`, `auto`, `preset apply`, `jobs stop` | allow for cameras the task owns |
| `disruptive` | `exec`, `session start`, `session stop`, `serve`, `shell`, `debug write`, `debug rpc` | ask the user |

Flags can make a command a camera write: `--set` on `inspect`, `capture` and `record`, and
`--auto` on `capture` and `record`, change camera settings as `set` and `auto` do and leave them
changed; `network --tune` writes camera network settings that last until the camera powers off.
Two read commands become file writes with a path: `inspect --preview PATH` and `debug xml -o FILE`
write that file (never over an existing one), so treat them as `file_write`.
A prefix also covers subcommands: a rule for `capturefab jobs` matches `capturefab jobs stop all`,
and one for `capturefab preset` matches `preset apply`. So allow exact prefixes
(`capturefab jobs list`, `capturefab jobs wait`, `capturefab preset list`), and ask before any
call with `--set`, `--auto` or `--tune` unless the task owns the camera.

`inspect` writes a small JPEG preview into a private folder, `inspect_dir` in
`capturefab --json doctor`; allow reading files there, or pass `--preview PATH` to put it in
the task's folder, or `--preview none` for no image. Captures go where `-o` says (the working
directory by default).

## Call patterns

The examples use the simulator `sim:0`; replace it with your camera's ID.

### Find a camera

```sh
capturefab list --json
```

```json
{"cameras":[{"id":"gige:00c0df123456","kind":"gige","vendor":"…","model":"…","serial":"…","address":"192.168.1.20"}]}
```

Use `id` with `-c` (without it, several cameras are exit 2 `camera_ambiguous`; only a person at
a terminal without `--json` is asked to pick one). `list --kind gige,u3v` narrows the search; `--timeout 3s` gives GigE and
ONVIF broadcasts longer to answer. `list` leaves out simulators and screen captures unless
`--kind sim` or `--kind screen` names them. A camera on another machine's stream needs no
`list`: pass its `rtsp://`, `srt://` or `onvif://` address to `-c`.

### Learn what a camera can do

```sh
capturefab info --json -c sim:0
```

```json
{"bits":8,"camera":"sim:0","controls":{"exposure":{"feature":"ExposureTime","max":1000000.0,"min":10.0,"unit":"us","value":"10ms"},"gain":{"feature":"Gain","max":24.0,"min":0.0,"unit":"dB","value":0.0},"…":"…"},"exposure_us":10000.0,"fps_limit":30.0,"gain_db":0.0,"height":480,"kind":"sim","model":"Pattern camera","pixel_format":"Mono8","trigger":{"activation":"RisingEdge","mode":"Off","selector":"FrameStart","source":"Software"},"width":640}
```

`info` is enough for most tasks. `features -c CAM PATTERN` searches the feature tree
(`features -c sim:0 'Exposure*'`); `features` without a pattern lists the common ones, and
`--all` lists everything (bounded; `truncated` counts the rest).

### Capture a well-exposed photo

```sh
capturefab inspect --json -c sim:0 --preview none
capturefab capture --json -c sim:0 -o 'shots/{camera}-{seq}.png'
```

`inspect` returns `verdict`, a list of one or more of `ok`, `dark`, `bright`, `clipped_high`,
`clipped_low`, `not_settled` and `not_measured`, such as `["ok"]` or `["clipped_high","bright"]`,
and, when exposure is off, `suggest.exposure_us`. Proceed when `verdict` is exactly `["ok"]`.
Otherwise either set the suggested exposure (next pattern) or let the camera settle first:
`capture --json -c sim:0 --auto -o shot.png` runs auto exposure, holds it and captures.

The `capture` result lists `files` (absolute paths) with `width`, `height`, `pixel_format`,
`bit_depth` and the `settings` the frame was taken with. PNG keeps monochrome frames wider than
8 bits (Mono10 to Mono16, packed or not) as 16-bit gray; color and Bayer frames wider than 8 bits
become 8-bit PNG or JPEG, and `source_bits` then says how many bits the frame had.
`-f jpg --quality 85` or a `.jpg` name gives JPEG; from a Motion JPEG webcam, stream or file,
`-f jpg` without `--quality` stores the JPEG it sent as it was (`"passthrough": true`), with the
provenance comment and, when it lacks them, the standard Huffman tables. `.raw` gives the frame's
exact bytes with a `NAME.json` sidecar (`pixel_format`, `pfnc_code`, `width`, `height`,
`row_bytes`, or for a planar layout `planes`, each with its `offset`, `row_bytes` and `rows`). Every common PFNC pixel format converts; see the
[pixel format table](guides/camera-compatibility.md#pixel-formats). Webcams, streams and files
arrive as FFmpeg decodes them (gray, YUV such as `YCbCr709_420_8_Planar`, or RGB8 for RGB
sources), so their `.raw` bytes are in that format. JPEGs may come from Apple's JPEG engine on
macOS, or nvJPEG or VA-API on Linux, each checked against libjpeg-turbo first
(`CAPTUREFAB_JPEG=cpu` keeps libjpeg-turbo; `doctor` reports them under `jpeg`).
Compressed streams and webcams other than Motion JPEG, which libjpeg-turbo decodes, are decoded
in hardware where it works in a session, and files and `--no-session` commands in software
(`CAPTUREFAB_HWDEC=auto|none|METHOD`; `doctor --json` reports the methods under `hwdec`). A
session's cameras keep the `CAPTUREFAB_JPEG`, `CAPTUREFAB_HWDEC` and `CAPTUREFAB_FFMPEG` the
session started with: stop it, or use `--no-session`, to change them. A command's own
`CAPTUREFAB_MIN_FREE` raises the free-space floor for the files a session writes for it, but does
not lower the floor the session started with.

The result also says which frame it saved: `time` (RFC 3339) and `device_ts_ns` (the camera's
own clock, never a date), with `frames` the camera delivered while it ran, `dropped` frames lost
and `incomplete` ones damaged on the way. The first frame saved is always one exposed after the
camera's last settings change; `--fresh N` lets N more pass. `--inspect` adds the last frame's
measurements to the result (`inspect.verdict`, `exposure`, `focus`) instead of a second call.
`--manifest shots.jsonl` appends one JSON line per file with its path, time, size, SHA-256,
settings and tags. With `-c` repeated, the cameras are armed, then asked for a frame together:
the result holds `cameras`, one entry each, and `skew_ms`, how far apart their first frames came.

### Set and verify in one call

```sh
capturefab inspect --json -c sim:0 --set exposure=2788us --preview none
```

`inspect --set` applies the change atomically, then measures a frame exposed after it. `set`
alone is for changes you do not need to measure: `capturefab set --json -c sim:0 exposure=5ms gain=3`
returns `changed: [{name, from, to}]` with the values read back. `--dry-run` checks every value
and shows the writes in order without changing anything. A failed `set` restores every value it
wrote, and the rest of the image geometry a write moved (binning rescales size and offsets);
`error.details.rollback` lists `restored` and `failed`.

### Focus or aim loop

```sh
capturefab inspect --json -c sim:0 --keep 5m --size 384
```

Repeat while someone adjusts the lens; compare `focus.sharpness` between calls (higher is
sharper for the same scene) and look at the preview only when needed. Every camera command keeps
its camera open in the `auto` session for 60 seconds after its last use, so later calls skip
connecting and starting the stream; `--keep 5m` holds it longer. `--region 0.25,0.25,0.5,0.5`
measures only the middle of the frame. For a person at a terminal,
`capturefab watch -c sim:0 --show` redraws the measurements and the frame in place; without a
terminal, `watch --json --for 10s` prints one JSON line per measurement.

### Burst

```sh
capturefab capture --json -c sim:0 -n 20 -o frames/
```

### Time lapse in the background

```sh
capturefab capture --json -c sim:0 --every 10s --for 1h -o 'lapse/{seq:5}.jpg' --background
capturefab jobs wait --json ID --timeout 10m     # ID: the job.id the line above returned
```

`--background` returns `{"job":{"id","state","saved","planned","next_at",…}}` once the job is
armed or has saved its first frame, and a session (started on demand) keeps running it; on a
triggered camera that first frame waits for a trigger, up to `--timeout`. Pass that `job.id` to
`jobs wait`, `jobs list` and `jobs stop`: IDs count on across a session's jobs and its runs
(`auto-1`, `auto-2`, …), so an ID you did not get back may name an earlier, finished job.
`jobs wait ID...` exits 0 when the jobs are done (`job`, or `jobs` for several IDs, as a capture
of several cameras returns), with a failed job's own error code, and 4 if one is still running
when `--timeout` ends; call it again to keep waiting. `jobs list ID...` shows jobs as they are
now without waiting, and `--running` only those that have not ended; `jobs --json` lists every
job, finished ones included. `jobs stop ID` ends one and keeps the files saved so far, and
`session stop` ends its session's running jobs at once, as `interrupted`. A job's `state` is
`armed`, `running` or `stopping`, then `done`, `failed`, `cancelled`, `interrupted`, or `lost`
when its session died. `--at 14:30` or `--in 5m` delays the start.

### Triggered capture

```sh
capturefab capture --json -c 'sim:0?line0=every:500ms' --set trigger=software -o part.png
capturefab set --json -c 'sim:0?line0=every:500ms' trigger=line0:rising --keep 5m
capturefab capture --json -c 'sim:0?line0=every:500ms' -o 'part-{seq}.png' --timeout 30s
```

`sim:0?line0=every:500ms` is a simulator whose Line0 input fires every 500 ms; on plain `sim:0`
the line never fires. With a software trigger, `capture` fires `TriggerSoftware` itself. For a
hardware trigger (`trigger=line0:rising`), `capture` waits for the line: `--timeout 30s` bounds
the wait, and expiry is exit 4 `trigger_timeout` with the trigger configuration in
`details.trigger`. The trigger setting stays for the captures that follow: the camera stays open
in a session between commands, and a simulator or webcam keeps the settings Capturefab emulates
for it until its runtime folder is cleared, as a real camera keeps its own until it powers off.
To capture every trigger for a while, combine `--timeout none -n 100 --background`, then
`jobs wait` or `jobs stop`. `set -c CAM trigger=off` returns to free running.

### Record video

```sh
capturefab record --json -c sim:0 -o clip.mp4 --for 30s
capturefab record --json -c sim:0 -o clip.mkv --codec ffv1 --for 1m
capturefab record --json -c sim:0 -o rtsp://localhost:8554/cam --background
```

The result has `output`, `bytes`, `frames`, `dropped`, `fps_actual`, `codec`, `encoder`, `ended`
(`duration`, `signal`, `max_size`, `stopped`, or `source_ended` when a video file being recorded
came to its end) and `finalized`. `--max-size 2G` ends the file
successfully before that size, leaving FFmpeg room to finish it; a limit too small for that cuts the
file at the size, with `finalized: false`, no `frames` and a warning. `hevc` and `av1` need a
hardware encoder unless the source is copied, and fail with exit 5 before recording otherwise;
`ffv1` is lossless, keeps every bit of a deep RGB format (as planar GBR), and stores a Bayer
camera's mosaic as gray samples. An H.264 or HEVC stream, file or webcam mode is copied as it arrives, without
re-encoding (`"encoder":"copy"`, `codec` the source's), from its last key frame, unless `--codec`
names another codec or `--fps` or `--bitrate` is given (Linux and macOS; Windows encodes). Without
`--for`, a recording started with `--background` runs until `jobs stop ID`; while it runs, its job
shows the file it writes as `last_file` and its size so far as `bytes`. A recording that ends
before its first frame, as on a camera still waiting for a trigger, recorded nothing and left no
file: a signal then exits 130 with `interrupted`, and `jobs stop` makes it exit 1 with
`incomplete`. When the camera's worker dies, the error's `details.job` and `output` name the file
it left, which plays up to its last complete part (`finalized: false`).

### Keep settings for later

```sh
capturefab preset save line1 --json -c sim:0
capturefab preset apply line1 --json -c sim:0 --dry-run
capturefab preset apply line1 --json -c sim:0
```

A preset is a JSON document on this computer; `apply` writes it as one change and restores
every value if any write fails. `--persist` then also saves it in the camera's user set.

### Password-protected streams

```sh
printf '%s\n' "$PASSWORD" | capturefab login add --json rtsp://cam.local/stream --user admin --password-stdin
capturefab capture --json -c rtsp://cam.local/stream -o door.png
```

The password goes to the OS credential store, never to a file or the command line, and reaches
FFmpeg on its standard input. Results and errors never show credentials. `onvif resolve HOST` lists an ONVIF camera's RTSP streams.

## Errors and what to do

| `error.code` | Exit | Do this |
| --- | --- | --- |
| `camera_ambiguous` | 2 | Pick an `id` from `details.candidates` and pass it with `-c`. |
| `camera_not_found` | 3 | Run `list`; check the cable, network or address. `details.candidates` lists what answered. |
| `camera_busy` | 3 | Another process or job holds it (`details.held_by`); use its session (`--session`), wait, or `jobs stop`. |
| `feature_not_found` | 2 | Use a name from `details.did_you_mean`, or search with `features -c CAM PATTERN`. |
| `out_of_range` | 2 | Send the value `hint` names, which `set` takes as written (a time is a duration such as `1s`), or `details.nearest` in `details.unit`, or a value between `details.min` and `details.max`. |
| `invalid_choice` | 2 | Use one of `details.choices`. |
| `not_writable`, `feature_locked` | 2, 1 | The feature is read-only, or `details.locked_by` locks it now: change that first. |
| `invalid_duration`, `usage` | 2 | Fix the command; read `capturefab schema COMMAND`. |
| `exists` | 1 | Choose a new name, add `{seq}` to `-o`, or pass `--on-exists number`. |
| `frame_timeout` | 4 | Raise `--timeout`; check `info` for the frame rate, exposure and trigger. |
| `trigger_timeout` | 4 | Send the trigger, raise `--timeout`, or `set trigger=off`. |
| `unavailable`, `timeout` | 3, 4 | The camera, stream, session or service did not answer. Follow `error.hint`, check the address and network, then retry once. |
| `auth_required`, `auth_rejected` | 3 | Save the sign-in with `login add`. |
| `ffmpeg_missing` | 3 | Run `doctor`; set `CAPTUREFAB_FFMPEG` to an FFmpeg executable. |
| `permission_denied` | 3 | Follow `error.hint` (on macOS: allow Camera or Local Network for the terminal app). |
| `disconnected`, `worker_crashed` | 3 | Retry once; then run `doctor` and report `error.details`. |
| `unsupported` | 5 | The camera or build cannot do it; do not retry. |
| `storage_full` | 6 | A write would take the disk below its free-space floor (1% of the disk, at least 512 MiB and at most 20 GiB; `CAPTUREFAB_MIN_FREE` changes it, `0` turns it off). `details` has the `path` and its `free_bytes`; free disk space or write to another disk. |
| `interrupted` | 130 | Stopped by a signal; `error.details.files` lists what was saved. |

Partial work of a failed or interrupted command is in `error.details` (for example `files` and
`job`).

## Saving tokens

- Prefer `info` and `get NAME…` over `features --all`; filter `features` with a pattern.
- `inspect --preview none` when the numbers are enough; `--size 384` for a smaller image.
- Cameras stay open for 60 seconds between commands; `--keep 5m` for longer loops.
- `capturefab shell --json -c CAM -e 'inspect --preview none' -e 'capture -o shot.png'` runs
  several commands in one start, one JSON line each (or one command per line of stdin).
- One call instead of two: `inspect --set`, `capture --set NAME=VALUE`, `capture --auto`,
  `capture --inspect`.
- `-q` silences progress on stderr.
