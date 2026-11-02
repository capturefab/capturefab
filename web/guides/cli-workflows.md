<!-- Generated from docs/cli-workflows.md by scripts/site-guides.py. Edit the source, then run the script. -->

# CLI workflows

Shell recipes for whole tasks. Each one runs on the simulator (`sim:0`); replace `-c sim:0` with
your camera's `list` ID. Add `--json` when a script or an agent reads the output: stdout then
holds exactly one JSON document, and the exit code says what kind of outcome it was (0 ok,
1 failed, 2 usage, 3 unavailable, 4 timeout, 5 unsupported, 6 storage full, 130 interrupted).
`capturefab schema COMMAND` describes any command's arguments and results.

## Set up a bench camera

```sh
capturefab list                                  # find the camera's ID
capturefab info -c sim:0                         # size, format, exposure, gain, trigger
capturefab inspect -c sim:0 --keep 15m           # exposure verdict, focus and a preview path
capturefab inspect -c sim:0 --set exposure=6ms   # change and measure in one call
capturefab set -c sim:0 roi=0,0,640,480 fps=30 --dry-run
capturefab set -c sim:0 roi=0,0,640,480 fps=30
capturefab preset save bench -c sim:0 --note 'ring light 60%'
```

`--keep 15m` keeps the camera open in a session, so the commands after it start at once. `set`
writes every value as one change, ordered so that geometry and frame rate stay valid, and puts
every value back if one write fails. `--dry-run` shows the writes in order without changing
anything. A preset restores the setup later, also on another camera of the same model:
`capturefab preset apply bench -c CAMERA`.

## Capture a sequence with names and tags

```sh
capturefab capture -c sim:0 -n 10 -o 'parts/{date}/{tag:part}-{seq:4}.png' --tag part=A-100 --tag station=3 --manifest parts.jsonl
```

Templates: `{camera}`, `{serial}`, `{seq}` (`{seq:4}` pads to four digits), `{time}`, `{date}`,
`{tag:KEY}` and `{ext}`. A folder (an existing one, or a name ending in `/`) gets
`{camera}-{time}-{seq}.{ext}`. Tags also go into each image's metadata. Existing files are never
overwritten; a name without `{seq}` that exists fails with `exists`, or is numbered with
`--on-exists number`. `--manifest` appends one JSON line per saved file (path, time, size,
SHA-256, settings and tags) for whatever reads the files later.

## A time lapse that outlives the terminal

```sh
capturefab capture -c sim:0 --every 30s --for 8h -o 'lapse/{seq:5}.jpg' --background
capturefab jobs                         # progress: saved, planned, next_at
capturefab jobs wait auto-1 --timeout 1m
capturefab jobs stop auto-1             # end early; files saved so far stay
```

`--background` starts a session if none holds the camera, submits the job and returns its ID.
A session started this way ends once it has had no jobs and no requests for a minute. `--at 06:00` or `--in 10m`
delays the first frame; `-n 100 --every 2s` bounds the series by count instead of time.

## A triggered inspection station

```sh
capturefab session start --name station -c sim:0 --live --idle 8h
capturefab set -c sim:0 trigger=line0:rising exposure=2ms
capturefab capture -c sim:0 -n 1000 -o 'station/{time}.png' --timeout none --background
capturefab session status station
capturefab jobs stop all                # at the end of the shift; files saved so far stay
capturefab session stop station
capturefab set -c sim:0 trigger=off     # back to free running
```

With a hardware trigger, `capture` saves one frame per trigger and waits for the next;
`--timeout none` waits without limit, and a bounded `--timeout` ends with exit 4
`trigger_timeout` and the trigger configuration in `error.details.trigger`. With
`trigger=software`, `capture` fires the software trigger itself, and `exec -c CAM
TriggerSoftware` fires one by hand. `--live` keeps the camera streaming between commands.

## Several cameras at once

```sh
capturefab capture -c sim:0 -c sim:1 -o 'multi/{camera}-{seq}.png'
capturefab get -c sim:0 -c sim:1 ExposureTime Width
capturefab inspect -c sim:0 -c sim:1 --preview none
```

With `-c` repeated, the JSON result holds `cameras`, one entry per camera. A capture arms every
camera first, then asks all of them for a frame at once; `skew_ms` says how far apart their first
frames arrived, and each entry's `time` and `device_ts_ns` say when its own frame was taken.

## Record and stream video

```sh
capturefab record -c sim:0 -o clip.mp4 --for 30s
capturefab record -c sim:0 -o clip.mkv --codec ffv1 --for 2m          # lossless
capturefab record -c sim:0 -o 'srt://localhost:8890?streamid=publish:cam' --background
capturefab jobs stop all
```

Recordings go to `.mkv`, `.mp4`, `.mov`, `.ts` or `.webm` files, or to an `rtsp://`, `srt://`,
`rtmp://` or `udp://` receiver such as MediaMTX. `--codec h264` (the default) uses a hardware
encoder where one works (VideoToolbox, NVENC, Jetson's GStreamer encoder, AMF, Quick Sync,
Vulkan, Media Foundation or V4L2) and libx264 otherwise; `hevc` and `av1` need a hardware
encoder. An H.264 or HEVC stream, file or webcam mode is copied as it arrives, without
re-encoding or an encoder (`"encoder":"copy"`), unless `--codec` names another codec or `--fps`
or `--bitrate` is given; Windows encodes. `--max-size 2G` ends a file before that size, leaving
FFmpeg room to finish it; Ctrl-C ends an open-ended recording normally. `.mkv` files get their
length and seek index once the recording ends.

## Password-protected cameras

```sh
printf '%s\n' "$PASSWORD" | capturefab login add rtsp://192.168.1.20/stream --user admin --password-stdin
capturefab onvif resolve 192.168.1.10                     # an ONVIF camera's RTSP streams
capturefab capture -c rtsp://192.168.1.20/stream -o door.png
capturefab login list
capturefab login rm rtsp://192.168.1.20/stream
```

`login add` connects once and saves the sign-in only if the camera accepts it (`--no-check` saves
without connecting). The password comes from `--password-stdin` or `CAPTUREFAB_CAMERA_PASSWORD`,
never the command line, and goes to the OS credential store; addresses and user names are listed
in the state folder without secrets. Every later command for that address, `onvif://` included,
uses the saved sign-in. Know the trade-offs before saving one:

- FFmpeg's RTSP reader takes credentials only in its input URL. Capturefab hands FFmpeg that
  URL on its standard input, and a session the cameras it holds the same way, so neither process's
  command line shows them; a command you type with `-c rtsp://USER:PASS@...` still shows the
  password in that command's own command line while it runs.
- A saved password is sent to whatever answers at that host, port and path. Plain RTSP Basic
  authentication and ONVIF over `http://` expose it on the network; prefer `rtsps://`, Digest
  authentication and fixed addresses on trusted networks.

## A script that checks every result

```sh
#!/bin/sh
set -u
out=$(capturefab capture --json -c sim:0 -o 'shots/{seq}.png')
status=$?
case $status in
  0) printf '%s\n' "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin)["files"][0])' ;;
  3|4) echo "camera unavailable or slow; retry later: $out" >&2; exit "$status" ;;
  *) echo "capture failed: $out" >&2; exit "$status" ;;
esac
```

## GigE network checks

```sh
capturefab network
capturefab network --tune -c gige:00c0df123456 -c gige:00c0df654321
```

`network` lists the computer's IPv4 adapters and each GigE camera found, with how it is reached
(link speed, MTU, subnet) and a summary of anything that limits it. `--tune` shares the link's
bandwidth between the named cameras with packet delay or throughput limits; cameras keep it until
they power off. Host addresses, routes and MTUs are never changed.
