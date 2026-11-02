<!-- Generated from docs/simulation.md by scripts/site-guides.py. Edit the source, then run the script. -->

# Simulated cameras

Capturefab has simulated cameras built in, so every command can be tried, scripted and tested
without hardware. They run in a camera worker like any other camera, so sessions, `--keep`,
background jobs, captures, recordings and `inspect` behave as they do with a real one.

| Selector | Camera |
| --- | --- |
| `sim:0`, `sim:1`, … `sim:NAME` | A moving pattern area-scan camera with a GenICam feature tree: `ExposureTime`, `Gain`, `ExposureAuto`, `AcquisitionFrameRate`, `Width`, `Height`, `PixelFormat` (Mono8, Mono12, Mono12p, Mono14p, Mono16, RGB8, RGB12, BayerRG8, BayerRG12, BayerRG12p) and a `TriggerSelector` bank with `TriggerMode`, `TriggerSource`, `TriggerActivation` and `TriggerSoftware` |
| `sim:line` | A line-scan camera that delivers blocks of lines |
| `sim:depth` | A depth camera with calibrated 16-bit `Coord3D_C16` samples and `Scan3d…` features |
| `sim:microscope` | A microscope camera with focus, magnification and illumination features |
| `sim:thermal` | A thermal camera with 16-bit samples and temperature features |
| `sim:replay:PATH` | Replays a video file, an image, or a folder of images and videos |

Each name is its own camera: `sim:0` and `sim:1` can be captured together
(`capture -c sim:0 -c sim:1`). `list` shows simulators only with `--kind sim`, and a command
without `-c` never picks one, so a real camera is never mistaken for a simulated one.

```sh
capturefab list --kind sim
capturefab info -c sim:0
capturefab set -c sim:0 exposure=20ms PixelFormat=BayerRG8
capturefab inspect -c sim:0 --set exposure=40ms            # verdict: clipped_high, bright
capturefab capture -c sim:0 --set trigger=software -o triggered.png   # capture fires the trigger
capturefab set -c sim:0 trigger=off PixelFormat=Mono8      # back to free running, as it started
capturefab capture -c sim:line -o line-block.png
capturefab get -c sim:depth PixelFormat Scan3dCoordinateScale Scan3dCoordinateOffset
capturefab capture -c sim:depth -o depth.raw               # the 16-bit samples as they are
```

A simulator keeps the settings you give it, as a real camera keeps its own until it powers off:
Capturefab saves them in the runtime folder, so a `set` holds for the commands that follow until
that folder is cleared (for example by a reboot). Commands also keep the camera open for 60
seconds between them (the `auto` session), and `capture --set exposure=5ms` changes settings and
captures in one call.

## Frame loss and hardware triggers

Options after a `?` make an area-scan simulator lose frames, as a busy network does, or fire its
trigger input, as a sensor wired to a real camera does:

```sh
capturefab capture -c 'sim:0?drop=0.1' -n 20 -o lossy/        # dropped, incomplete, gaps
capturefab set -c 'sim:0?line0=every:500ms' trigger=line0:rising
capturefab capture -c 'sim:0?line0=every:500ms' -n 5 -o 'part-{seq}.png'
capturefab set -c 'sim:0?line0=every:500ms' trigger=off
```

- `drop=D`, from 0 to 0.5: that share of frames goes bad, half lost on the way and half arriving
  incomplete and discarded. A capture still saves the frames it asked for; its result counts the
  bad ones (`dropped`, `incomplete`) and, for a burst, the block IDs missing between its first and
  last frame (`gaps`), and `session status` counts them too (`lost`, `incomplete`). `drop=0.5`
  delivers about half the frames.
- `line0=every:DUR`: the Line0 input fires every DUR, so with `trigger=line0:rising` each capture
  takes the next frame it starts. Without this option Line0 never fires, and Line1 never does: a
  capture waiting for either ends at `--timeout` with exit 4 `trigger_timeout`.

Join them with `&`, as in `'sim:0?drop=0.02&line0=every:500ms'`, and quote the selector in the
shell. A selector with options is a camera of its own that starts from the settings its base
(`sim:0`) saved, so use the same selector for every command of a sequence.

## Replay test data

`sim:replay:PATH` turns recorded data into a camera:

```sh
capturefab capture -c sim:replay:/data/clip.mp4 -n 100 -o replay-frames/
capturefab capture -c sim:replay:/data/frames --set Width=1280 --set Height=720 -n 60 -o snapshots/
capturefab record -c sim:replay:/data/frames -o replayed.mp4 --for 30s
```

A replay camera delivers RGB8 frames at 640 × 480 and 30 fps by default; `Width` (64–1920),
`Height` (64–1080) and `AcquisitionFrameRate` (1–120) change that while it is not acquiring.
Frames are scaled to fit, with black borders that keep their aspect ratio. Videos are resampled
to the frame rate; each still image supplies one frame.

A folder is read once, **only at its top level**: subdirectories, symlinks and unsupported files
are ignored. Files play in filename order, case-sensitively, so `10.png` sorts before `2.png`;
use zero-padded names (`0001.png`) for numeric sequences. The playlist repeats without end, and
frame IDs and timestamps continue across files and loops. Restart the camera to pick up added
files. An empty folder, a missing path or a corrupt file is an error.

Images: PNG, JPEG, BMP, TIFF, WebP and PNM (PPM, PGM, PBM). Videos: MP4, M4V, MOV, MKV, WebM, AVI,
MPEG, TS and similar containers, as far as the FFmpeg in use decodes them. Replay reads local
files only; for a network stream, pass its URL to `-c` instead.

## Use simulators in tests

Point the state and cache folders at temporary directories, so a test never touches the user's
sessions, presets or sign-ins:

```sh
export CAPTUREFAB_SESSION_DIR=$(mktemp -d) CAPTUREFAB_CACHE_DIR=$(mktemp -d)
capturefab capture --json -c sim:0 -n 3 -o "$CAPTUREFAB_SESSION_DIR/frames/"
```

The simulators are for exercising commands and pipelines; they do not model a particular camera's
timing, noise or firmware behavior.
