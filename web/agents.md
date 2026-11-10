# Capturefab for AI agents

Use `capturefab` to find cameras, change settings, save images and record video from the shell.
Use `capturefab schema` to see what the installed version supports.
Run `capturefab schema COMMAND` for that command's arguments, result fields and errors.
The site also publishes a [command reference](reference.html).

## Install, upgrade or uninstall

Install the CLI for the current user without administrator rights.
The CLI starts at version 0.6.0. Earlier releases are the desktop app.

To delegate setup, copy this task into an agent:

```text
Install or upgrade Capturefab for my user account from https://capturefab.com.
Download and read install.sh (macOS/Linux) or install.ps1 (Windows).
Run the script with --upgrade or -Upgrade.
Use my custom install paths if I supplied them. Otherwise, use the defaults.
Make capturefab available in this terminal.
Run capturefab --version and capturefab doctor --json.
Report the installed version, executable path and setup issues.
Read this agent guide and capturefab schema before running camera commands.
```

Download a fresh script to install the latest release. Each script selects a specific version.
Close running Capturefab commands and sessions before removal.
Use the same custom paths that you used to install it.
A saved script can remove the installation offline. User data outside the release folders stays in place.

| Platform | Upgrade | Preview removal | Remove | Custom paths |
| --- | --- | --- | --- | --- |
| macOS and Linux | `sh install.sh --upgrade` | `sh install.sh --uninstall --dry-run` | `sh install.sh --uninstall` | `--prefix DIR`, `--bin-dir DIR` |
| Windows | `.\install.ps1 -Upgrade` | `.\install.ps1 -Uninstall -DryRun` | `.\install.ps1 -Uninstall` | `-Prefix DIR` |

Get the scripts and find platform setup steps on the [downloads page](downloads.html).

## Rules

1. **Pass `--json` for automation.** Most commands write one JSON line to stdout. Exit 0 gives
   a result. Errors have an `error` object with `code`, `message` and optional `hint` and `details`.
   Read diagnostics on stderr. `--help` and `--version` print text.
2. **Use the exit code for the category and `error.code` for the reason.**
   Exit codes are 0 for success, 1 for failure, 2 for invalid input, 3 for unavailable resources,
   4 for timeout, 5 for unsupported operations and 6 for full storage.
   Interruption codes are 130 for SIGINT, 143 for SIGTERM and 129 for SIGHUP.
3. **Use the installed schema to find flags.** For example, run `capturefab schema jobs wait`.
   Unknown flags give exit 2 and `usage`.
4. **Include units in durations.** Use values such as `250ms`, `1.5s`, `5m` or `1h30m`.
   A bare number other than `0` is invalid. Use `--timeout none` where the command supports an unlimited wait.
5. **Choose the camera explicitly.** Pass an ID from `list` with `-c`.
   Quote IDs and feature names with spaces, brackets or other shell characters.
   Examples include `'avfoundation:HD Pro Webcam C920'` and `'TriggerMode[FrameStart]'`.
6. **Choose a new output path.** `capture` and `record` protect existing files.
   Use `{seq}` in the output template or use `--on-exists number` with `capture`.
   Relative input and output paths use your working directory, even when a session runs the command.
   File paths in capture results are absolute.

`watch --json` writes one line per measurement. `shell --json` writes one line per command.
With `capture -o - --json`, stdout carries one image and stderr carries the JSON result or error.
With `--progress`, `capture`, `record` and `jobs wait` write progress JSON lines to stderr.

## Permissions

Put the full command and subcommand before the flags.
For example, run `capturefab jobs list --json`.
In `capturefab schema`, `commands` maps names to `[effect, summary]`.
`effect_with` lists the flags that increase the effect.

| Effect | Examples | Suggested policy |
| --- | --- | --- |
| `read` | `list`, `info`, `features`, `get`, `inspect`, `watch`, `jobs list`, `jobs wait`, `doctor`, `schema` | Allow. |
| `file_write` | `capture`, `record`, `preset save`, `preset import`, `preset export`, `preset rm`, `login add`, `login rm` | Limit file paths to the task directory. Treat stream destinations, preset changes and credential store changes as separate permissions. |
| `camera_write` | `set`, `auto`, `preset apply`, `jobs stop` | Limit to the task's cameras and job IDs. |
| `disruptive` | `exec`, `session start`, `session stop`, `serve`, `shell`, `debug write`, `debug rpc` | Ask before running. |

- Allow only the intended subcommands. A prefix rule for `capturefab jobs` also matches `jobs stop all`.
  That command stops active jobs across sessions unless `--session NAME` limits it.
- Treat `--set` on `inspect`, `capture` or `record` as a camera write.
  The same applies to `--auto` on `capture` or `record`, and to `network --tune`.
  These changes can affect later commands.
- Treat `inspect --preview PATH` and `debug xml -o PATH` as file writes.
  The default `inspect` preview goes to the private `inspect_dir` reported by `doctor --json`.
  Use `--preview none` to skip the image.

## Call patterns

Run the simulator examples in a scratch directory. For a physical camera, use its ID in place of `sim:0`.
Controls and measurement fields depend on the source.

### Find a camera

```sh
capturefab list --json
capturefab list --json --kind sim
```

Use `cameras[].id` with `-c`. If several cameras match and you omit `-c`, expect exit 2 and `camera_ambiguous`.
Use `list --kind gige,u3v` to limit discovery to GigE Vision and USB3 Vision.
Increase `--timeout` to give GigE Vision and ONVIF discovery more time.
To open a known stream, pass its URL directly with `-c`.

### Learn what a camera can do

```sh
capturefab info --json -c sim:0
capturefab features --json -c sim:0 'Exposure*'
```

Start with `info` for the image size, pixel format and supported controls.
Use `features` with a pattern to find a feature's value, range or choices.
Use `--all` to include all visibility levels. If the response has `truncated`, narrow the search.

### Capture a well-exposed photo

```sh
capturefab inspect --json -c sim:0 --preview none
capturefab capture --json -c sim:0 -o 'shots/{camera}-{seq}.png'
```

Read `verdict` as a list. For example, `["ok"]` means exposure is within the measured limits.
If the scene is dark, bright or clipped, use `suggest.exposure_us` when present.
Add the `us` unit when you send that value back as `exposure=VALUEus`.
If the camera is still settling, measure again.
To adjust exposure before capture on a supported camera, add `--auto`.

The result gives absolute paths in `files`, plus the first saved frame's `time` and `settings`.
When present, `device_ts_ns` is the camera's clock in nanoseconds.
`frames` counts frames received, including skipped frames. `dropped` and `incomplete` report losses.
Use `--fresh N` to skip N more frames after the last settings change.
Use `--inspect` to add the last saved frame's measurements.
Use `--manifest shots.jsonl` to append each saved file's path, time, size, SHA-256, settings and tags.

| Output | What to expect |
| --- | --- |
| PNG | Monochrome above 8 bits uses 16-bit gray. Color and Bayer use 8-bit color. |
| JPEG | `--quality` sets encoding quality. Without that flag, an MJPEG source can keep its original JPEG data. The result says `"passthrough": true`. |
| Raw | The frame's bytes at full bit depth. For example, `shot.raw` gets a `shot.raw.json` sidecar that describes the layout. |

`bit_depth` gives bits per sample in the saved image. `source_bits` appears when the conversion loses bit depth.
Raw output from webcams, streams and files uses the decoded format.
See the [pixel format table](guides/camera-compatibility.md#pixel-formats).

### Set and verify in one call

```sh
capturefab inspect --json -c sim:0 --set exposure=5ms --preview none
capturefab set --json -c sim:0 exposure=5ms gain=3 --dry-run
```

`inspect --set` applies the change, then measures a frame exposed after it.
`set --dry-run` shows the planned writes and warns when a value can only be validated during the write.
Run `set` without `--dry-run` to apply the change. Read `changed[].to` for the values read back.
If a write fails, Capturefab tries to restore the prior values.
Examine `error.details.rollback.failed` before you assume the camera is back in its previous state.

### Focus or aim loop

```sh
capturefab inspect --json -c sim:0 --keep 5m --size 384
capturefab watch --json -c sim:0 --for 2s
```

Compare `focus.sharpness` for the same camera and scene. Higher values indicate sharper focus.
Use `--region 0.25,0.25,0.5,0.5` to measure the central half of the image in each direction.
At a terminal, use `capturefab watch -c sim:0 --show` to see the frame and measurements.

The automatic session keeps a camera open for 60 seconds by default.
Use `--keep 5m` for a longer loop. Use `--no-session` to open the camera for one command.

### Burst

```sh
capturefab capture --json -c sim:0 -n 20 -o frames/
```

Read `consecutive` and `gaps` when they are present and frame continuity matters.
With repeated `-c`, `capture` arms the cameras together.
Read each result in `cameras` and use `skew_ms` to compare when the first frames arrived.

### Time lapse in the background

```sh
capturefab capture --json -c sim:0 --every 1s --for 5s -o 'lapse/{seq:5}.png' --background
```

Pass the returned `job.id` to `capturefab jobs wait --json ID --timeout 10s`.
The session keeps the job running after the capture command exits.
`jobs wait` succeeds when all requested jobs finish successfully.
A failed job returns its own error code. If the wait itself times out, it returns exit 4 and the job can continue.
Call `jobs wait` again to continue waiting.

Use `jobs list --json --running` for a snapshot of active jobs.
Use `jobs stop --json ID` to stop one job and keep the files it saved.
Finished jobs stay in the list for an hour. Use the ID the command returned.
Use `capture --in 5m` or `capture --at 14:30` to schedule a later start.

### Triggered capture

```sh
capturefab capture --json -c 'sim:0?line0=every:500ms' --set trigger=software -o software.png
capturefab capture --json -c 'sim:0?line0=every:500ms' --set trigger=line0:rising -o 'part-{seq}.png' --timeout 3s
capturefab set --json -c 'sim:0?line0=every:500ms' trigger=off
```

For `trigger=software`, `capture` sends the software trigger.
For `trigger=line0:rising`, it waits for a rising edge on Line0.
This simulator produces a Line0 pulse every 500 ms. Plain `sim:0` has no Line0 pulses.
If the wait expires, read `error.details.trigger` with exit 4 and `trigger_timeout`.
Trigger settings can carry over to later calls. Set `trigger=off` to return to free-running capture.

### Record video

```sh
capturefab record --json -c sim:0 -o clip.mp4 --for 2s
capturefab record --json -c sim:0 -o clip.mkv --codec ffv1 --for 2s
```

Read `output`, `ended` and `finalized` to find the recording and see how it finished.
With `--max-size`, a cap too small for FFmpeg to finish the container can give `finalized: false`.
FFV1 preserves samples without loss. It stores a Bayer mosaic as grayscale samples.
HEVC and AV1 encoding depend on an available hardware encoder.
On Linux and macOS, compatible H.264 or HEVC input can be copied without re-encoding.
Look for `"encoder": "copy"`. A different `--codec`, or use of `--fps` or `--bitrate`, disables copy mode.

Without `--for`, stop a background recording with `jobs stop --json ID`.
For streaming output, start a receiver that accepts a publisher before you use its URL with `-o`.
See the [recording and streaming workflow](guides/cli-workflows.md#record-and-stream-video).

### Keep settings for later

```sh
capturefab preset save line1 --json -c sim:0
capturefab preset apply line1 --json -c sim:0 --dry-run
capturefab preset apply line1 --json -c sim:0
```

A preset stores settings on this computer. Review the dry run before you apply it.
On a camera with writable user sets, `--persist UserSet1` also saves the settings there and makes that set the power-on default.

### Password-protected streams

```sh
printf '%s\n' "$PASSWORD" | capturefab login add --json rtsp://cam.local/stream --user admin --password-stdin
capturefab capture --json -c rtsp://cam.local/stream -o door.png
```

Replace the example URL and user with the camera's values. Supply the password from your secret store.
Capturefab saves the password in the operating system's credential store and redacts credentials in output.
For ONVIF, use `capturefab onvif resolve --json HOST` to list profiles and their RTSP URLs.

## Errors and what to do

| `error.code` | Exit | Next action |
| --- | --- | --- |
| `camera_ambiguous` | 2 | Choose an ID from `error.details.candidates`. Pass it with `-c`. |
| `camera_not_found` | 3 | Run `list`. Examine the cable, network or address. |
| `camera_busy` | 3 | Look for `error.details.held_by`. Use the session that holds the camera or wait for the active job. |
| `feature_not_found` | 2 | Search with `features -c CAM PATTERN`. Look for suggestions in `error.details.did_you_mean`. |
| `out_of_range`, `invalid_choice` | 2 | Follow the hint or use the range or choices in `error.details`. |
| `not_writable`, `feature_locked` | 2, 1 | Find the controlling setting in `error.details.locked_by` when present. |
| `invalid_duration`, `usage` | 2 | Read `capturefab schema COMMAND`. Correct the input. |
| `exists` | 1 | Choose a new path. Use a sequence template for captures or recordings. |
| `frame_timeout`, `trigger_timeout` | 4 | Examine exposure, frame rate and trigger settings with `info`. Increase the timeout or supply the expected trigger. |
| `auth_required`, `auth_rejected` | 3 | Save or update the sign-in with `login add`. |
| `ffmpeg_missing` | 3 | Run `doctor`. Use `CAPTUREFAB_FFMPEG` to select a working executable. |
| `permission_denied` | 3 | Follow the hint. On macOS it can name Camera or Local Network access for the terminal app. |
| `disconnected`, `worker_crashed` | 3 | Examine `error.details` for files or jobs left behind before retrying. |
| `unsupported` | 5 | Use an operation the source and build support. |
| `storage_full` | 6 | Free space or select another disk. `error.details` gives the path and can report free bytes. |
| `interrupted` | 130, 143, 129 | Look in `error.details` for partial work such as `files` or `job`. |

The default free-space floor is 1% of the disk, with a minimum of 512 MiB and a maximum of 20 GiB.
`CAPTUREFAB_MIN_FREE` changes the floor. A command can raise an existing session's floor for its own output.
To change the session's FFmpeg, JPEG or hardware decoder settings, restart the session with the new environment.
Use `doctor --json` to examine the available tools and methods.

## Saving tokens

- Use `info` or `get NAME…` for specific values.
- Use `inspect --preview none` for measurements alone or `--size 384` for a smaller preview.
- Use `capture --inspect` when you also want the saved frame's measurements.
- Use `capturefab shell --json -c sim:0 -e 'inspect --preview none' -e 'capture -o shell.png'` to run several commands in one process.
  Batch mode stops at the first failed command unless you pass `--keep-going`.
