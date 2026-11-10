# Simulated cameras

Use a built-in simulator to try Capturefab without a camera. You can test captures,
recordings, measurements, sessions and background jobs.

| Selector | Camera |
| --- | --- |
| `sim:0`, `sim:1`, or `sim:NAME` | An area-scan camera with a moving pattern, GenICam controls and software or line triggers |
| `sim:line` | A line-scan camera that returns blocks of lines |
| `sim:depth` | A depth camera with 16-bit `Coord3D_C16` samples and calibration features |
| `sim:microscope` | A microscope camera with focus, magnification and illumination controls |
| `sim:thermal` | A thermal camera with 16-bit samples and temperature features |
| `sim:replay:PATH` | A camera that replays a local video, an image or a folder of media |

Pass the selector with `-c`. Each simulator name identifies a separate camera, so one
command can capture from both `sim:0` and `sim:1`. Use `list --kind sim` to list the
default simulator, `sim:0`.

```sh
capturefab list --kind sim
capturefab info -c sim:0
capturefab set -c sim:0 exposure=20ms PixelFormat=BayerRG8
capturefab inspect -c sim:0 --set exposure=40ms
capturefab capture -c sim:0 --set trigger=software -o triggered.png
capturefab set -c sim:0 trigger=off PixelFormat=Mono8
capturefab capture -c sim:line -o line-block.png
capturefab get -c sim:depth PixelFormat Scan3dCoordinateScale Scan3dCoordinateOffset
capturefab capture -c sim:depth -o depth.raw
```

The simulator saves its settings in the runtime folder. A setting stays in effect for
later commands until you change it or clear that folder. By default the `auto` session
also keeps the camera open for 60 seconds after the last use. Use `capture --set` to
change a setting and capture in one command.

## Frame loss and hardware triggers

Add options after `?` to simulate frame loss or a signal on the `Line0` input. Quote the
selector in the shell.

```sh
capturefab capture --json -c 'sim:0?drop=0.1' -n 20 -o lossy/
capturefab set -c 'sim:0?line0=every:500ms' trigger=line0:rising
capturefab capture -c 'sim:0?line0=every:500ms' -n 5 -o 'part-{seq}.png'
capturefab set -c 'sim:0?line0=every:500ms' trigger=off
```

- Use `drop=D` with a value from 0 to 0.5. For example, `drop=0.1` simulates about 10%
  frame loss. Capturefab counts lost frames in `dropped` and discarded partial frames in
  `incomplete`. When available for a burst, `gaps` counts missing frame IDs between the
  first and last saved frames.
- Use `line0=every:DUR` with an interval of at least 1 ms. With `trigger=line0:rising`,
  each pulse starts a frame. Without the option, `Line0` stays quiet. `Line1` stays quiet
  in all cases. A capture that times out waiting for a trigger returns exit code `4` and
  the error code `trigger_timeout`.

Use `&` to combine options, as in `'sim:0?drop=0.02&line0=every:500ms'`. The options
change the connection. Saved settings belong to the base selector, such as `sim:0`.
Use the full selector throughout a sequence to keep the same connection options.

The simulator's `Freerun` and `FixedRate` trigger sources use the camera's own timing.
They run at `AcquisitionFrameRate` even with `TriggerMode=On`.

## Replay test data

Replay uses FFmpeg to read local media files. Replace the sample paths with your own.
Quote the full selector if a path contains spaces.

```sh
capturefab capture -c sim:replay:/data/clip.mp4 -n 100 -o replay-frames/
capturefab capture -c sim:replay:/data/frames --set Width=1280 --set Height=720 -n 60 -o snapshots/
capturefab record -c sim:replay:/data/frames -o replayed.mp4 --for 30s
```

Replay delivers RGB8 at 640 × 480 and 30 frames per second by default. Set `Width` to
64–1920 pixels, `Height` to 64–1080 pixels and `AcquisitionFrameRate` to 1–120 Hz.
Replay scales frames to fit and adds black borders to keep their aspect ratio.

Replay reads supported regular files at the top level of a folder in case-sensitive
filename order. Use zero-padded names such as `0001.png` and `0002.png` for numbered
sequences. Each still image supplies one frame per loop. Replay resamples videos to
the selected frame rate. The playlist loops while the camera runs, and frame IDs and
timestamps continue across loops. To pick up new files, reopen the camera.

Use PNG, JPEG, BMP, TIFF, WebP or PNM for images. Video formats include MP4, M4V, MOV,
MKV, WebM, AVI, MPEG and TS. A missing source, a folder with no supported files or a
file FFmpeg cannot decode causes an error. For a network stream, pass its URL directly
to `-c`.

## Use simulators in tests

Use a private temporary directory for sessions, state and cache. This keeps a test
separate from your usual presets, jobs and cached files.

```sh
scratch=$(mktemp -d)
export CAPTUREFAB_SESSION_DIR="$scratch/session"
export CAPTUREFAB_STATE_DIR="$scratch/state"
export CAPTUREFAB_CACHE_DIR="$scratch/cache"
capturefab capture --json --no-session -c sim:0 -n 3 -o "$scratch/frames/"
```

For tests with background jobs, stop the sessions you started before removing the
temporary directory. Use a real camera to validate its timing, noise and firmware
behavior.
