# CLI workflows

Try the capture examples with the built-in simulator, `sim:0`. For a real camera, run
`capturefab list` and use the camera ID with `-c`. Each example starts in its own working
directory unless it uses files from an earlier step.

For scripts, add `--json` to get one JSON document on stdout. When `capture -o -` writes an
image to stdout, the JSON goes to stderr. The exit code gives the result category. On
failure, `error.code` gives the specific reason. Run `capturefab schema COMMAND` for the
options and result fields.

## Set up a bench camera

Use `info` to read the current settings. Use `inspect` to measure exposure and focus and get
the path to a JPEG preview. Adjust the exposure and measure again in one command.

```sh
capturefab info -c sim:0
capturefab inspect -c sim:0 --keep 15m
capturefab inspect -c sim:0 --set exposure=6ms
capturefab set -c sim:0 roi=0,0,640,480 fps=30 --dry-run
capturefab set -c sim:0 roi=0,0,640,480 fps=30
capturefab preset save bench -c sim:0 --note 'ring light 60%'
```

`--keep 15m` keeps the camera open for at least 15 idle minutes. Use `--dry-run` to see the
planned writes before you change the settings. If a write fails, Capturefab tries to restore
the previous values. With `--json`, read `error.details.rollback` for the result of that
attempt.

To restore the preset on the same camera or another of the same model, run
`capturefab preset apply bench -c sim:0` with the target camera ID.

## Capture a sequence with names and tags

```sh
capturefab capture -c sim:0 -n 10 -o 'parts/{date}/{tag:part}-{seq:4}.png' --tag part=A-100 --tag station=3 --manifest parts.jsonl
```

This saves ten PNGs with names such as `A-100-0001.png` in a dated folder. The images carry
both tags. The manifest appends one JSON line per image with its path, time, size, SHA-256,
settings and tags.

Use an existing folder or end the output path with `/` to get the default name
`{camera}-{time}-{seq}.{ext}`. Capturefab picks a free name for templates that use `{seq}`
or `{time}`. For a fixed name such as `part.png`, use `--on-exists number` to add a suffix
if the file exists. See the [file naming guide](plant-floor.md#name-files-for-analysis)
for the placeholders.

## Run a time lapse in the background

Run these commands in a POSIX shell with Python 3 available.

```sh
result=$(capturefab capture --json -c sim:0 --every 30s --for 8h -o 'lapse/{seq:5}.jpg' --background) &&
  job=$(printf '%s\n' "$result" | python3 -c 'import json,sys; print(json.load(sys.stdin)["job"]["id"])')
```

`--background` submits the job to a session and returns `job.id`, for example `auto-3`.
If Capturefab starts the session for this job, the session ends after a minute without
requests or running jobs. The capture can continue after you close the terminal.

Use the returned ID to see the job's progress or wait up to one minute for it to finish.
The wait can time out while the job continues.

```sh
capturefab jobs list "$job"
capturefab jobs wait "$job" --timeout 1m
```

To end the time lapse early, run `capturefab jobs stop "$job"`. The saved images stay on disk.
Use `--at 06:00` or `--in 10m` to delay the first frame. Use `-n 100 --every 2s` to set
a frame count instead of a duration.

## Set up a triggered inspection station

This simulator sends a rising edge to `Line0` every 500 ms, like a sensor wired to a camera.
Use the same selector in each command.

```sh
cam='sim:0?line0=every:500ms'
capturefab session start --name station -c "$cam" --idle 8h
capturefab set --session station -c "$cam" trigger=line0:rising exposure=2ms
capturefab capture --session station -c "$cam" -n 1000 -o 'station/{seq:4}.png' --timeout none --background
capturefab session status station
```

The capture waits for a trigger for each frame. With `--timeout none`, it keeps waiting until
a trigger arrives or you stop it. If the wait reaches a finite timeout, the job fails with
`trigger_timeout`. A foreground capture returns exit code `4` and gives the trigger settings
in `error.details.trigger` with `--json`.

To stop the station, run `capturefab session stop station`. Capturefab keeps the images it
already saved. Run `capturefab set -c "$cam" trigger=off` to return the camera to free-running
mode. With `trigger=software`, `capture` sends the trigger itself.

## Capture from several cameras

```sh
capturefab capture --json -c sim:0 -c sim:1 -o 'multi/{camera}-{seq}.png'
capturefab get -c sim:0 -c sim:1 ExposureTime Width
capturefab inspect -c sim:0 -c sim:1 --preview none
```

Capturefab arms the cameras before it requests their frames in parallel. For a successful
foreground capture, the JSON result has one entry per camera in `cameras`. The host clock
gives each entry's `time` and the difference between the earliest and latest first-frame
times in `skew_ms`. When available, `device_ts_ns` gives that camera's timestamp in
nanoseconds. For synchronized exposures, use cameras with a shared hardware trigger.

## Record and stream video

```sh
capturefab record -c sim:0 -o clip.mp4 --for 30s
capturefab record -c sim:0 -o clip.mkv --codec ffv1 --for 2m
```

Use `.mkv`, `.mp4`, `.mov`, `.ts` or `.webm` for a file. Use FFV1 with `.mkv` for
lossless video. For H.264, Capturefab uses a hardware encoder when available and libx264
otherwise. HEVC and AV1 use hardware encoders. Run `capturefab doctor` to see the FFmpeg in
use.

On macOS and Linux, Capturefab can copy H.264 and HEVC video from supported streams, files and
webcam modes. An output that accepts the source codec can use this path when `--fps` and
`--bitrate` are absent. The JSON result shows `"encoder":"copy"` when it does. To choose a
different codec, use `--codec`.

Use `--max-size 2G` to set a file size limit. Without `--for`, push Ctrl+C to finish a
foreground recording. For background recordings, use `capturefab jobs stop JOB_ID`. The
Matroska duration and seek index are available after the file closes.

For a stream, start a compatible receiver first. The next example sends video to a local
receiver listening on UDP port 8890.

```sh
capturefab record -c sim:0 -o udp://127.0.0.1:8890 --background
```

Capturefab also accepts `rtsp://`, `srt://` and `rtmp://` output URLs. Use the URL and
stream parameters for your receiver. The FFmpeg in use must support that protocol.

## Connect to a camera with a password

Replace the example RTSP URL and user name with your camera's details. Supply the password
in the shell variable `PASSWORD`.

```sh
printf '%s\n' "$PASSWORD" | capturefab login add rtsp://192.168.1.20/stream --user admin --password-stdin
capturefab capture -c rtsp://192.168.1.20/stream -o door.png
capturefab login list
```

`login add` verifies the sign-in before it saves it. Use `--no-check` to save it without
connecting. Supply the password through stdin or `CAPTUREFAB_CAMERA_PASSWORD`. Capturefab
stores it in the OS credential store. The local sign-in index holds the address and user name.

For ONVIF, save the sign-in under the ONVIF address, such as `onvif://192.168.1.20`. Then use
`capturefab onvif resolve 192.168.1.20` to see the camera's profiles and stream URLs.
To remove a sign-in, use `capturefab login rm ADDRESS` with the address you used to save it.

Keep passwords out of camera URLs on the command line. A URL you type with a password can
appear in the process list and shell history. Use encrypted connections when the camera
supports them. RTSP Basic authentication on an unencrypted link exposes the password.

## Use the result in a script

This POSIX shell script prints the path to the saved image. On failure it writes the JSON
error to stderr and returns Capturefab's exit code. It uses Python 3 to read the path.

```sh
#!/bin/sh
set -u
out=$(capturefab capture --json -c sim:0 -o 'shots/{seq}.png')
status=$?
case $status in
  0) printf '%s\n' "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin)["files"][0])' ;;
  *) printf '%s\n' "$out" >&2; exit "$status" ;;
esac
```

Use exit code `3` for an unavailable camera and `4` for a timeout. Use `error.code` if the
script must tell a trigger timeout from another timeout.

## Diagnose a GigE Vision connection

```sh
capturefab network
capturefab network --tune -c gige:00c0df123456 -c gige:00c0df654321
```

`network` shows the host's IPv4 interfaces and the route to each GigE Vision camera, with
link speed, MTU and subnet details. To share bandwidth, replace the example IDs with the
cameras on your link. `--tune` writes packet delay or throughput limits to those cameras.
The command changes camera settings only.
