# Simulated cameras

Capturefab has simulated cameras built in, so every command can be tried, scripted and tested
without hardware. They run in a camera worker like any other camera, so sessions, `--keep`,
background jobs, captures, recordings and `inspect` behave as they do with a real one.

| Selector | Camera |
| --- | --- |
| `sim:0`, `sim:1`, … `sim:NAME` | A moving pattern area-scan camera with a GenICam feature tree: `ExposureTime`, `Gain`, `ExposureAuto`, `AcquisitionFrameRate`, `Width`, `Height`, `PixelFormat` (Mono8, RGB8, Mono12, BayerRG8) and a `TriggerSelector` bank with `TriggerMode`, `TriggerSource`, `TriggerActivation` and `TriggerSoftware` |
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
capturefab capture -c sim:line -o line-block.png
capturefab get -c sim:depth PixelFormat Scan3dCoordinateScale Scan3dCoordinateOffset
capturefab capture -c sim:depth -o depth.raw               # the 16-bit samples as they are
```

Settings live in the camera worker. A plain command starts a new worker, so a simulator starts
from its defaults each time; to change settings and capture with them, use one call
(`capture --set exposure=5ms`), or keep the camera open with `--keep 5m` or a session.

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
