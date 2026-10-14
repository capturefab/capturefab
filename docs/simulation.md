# Simulated cameras and integration testing

Replay a video file or a directory of still frames or videos as a camera. Host it over GigE Vision, RTSP or ONVIF to test acquisition, NVR onboarding, recording, reconnects, and computer vision pipelines without camera hardware.

```sh
capturefab simulate presets
capturefab simulate run --preset onvif --source ./test-data
```

The command prints its ready state and connection endpoints, then runs until Ctrl-C. `--duration 30s` ends a bounded test run. Omit `--source` to use the moving RGB pattern camera.

| Preset | Protocol | Default format | Use |
| --- | --- | --- | --- |
| `gige` | GigE Vision / GenICam | 640 × 480 RGB8, 30 fps | Industrial camera acquisition and feature control |
| `vision` | GigE Vision / GenICam | 640 × 480 RGB8, 60 fps | Lossless test data for computer vision pipelines |
| `rtsp` | RTSP / H.264 | 1920 × 1080, 30 fps, 4 Mbit/s | NVR recording and video stream ingestion |
| `onvif` | ONVIF device/media + RTSP / H.264 | 1280 × 720, 15 fps, 2 Mbit/s | Camera discovery, media profiles and NVR onboarding |

Override `--width`, `--height`, `--fps`, and `--bitrate` as needed. H.264 dimensions must be even. Frames fit within the selected camera dimensions with black padding to preserve their aspect ratio. Videos are resampled to the selected camera frame rate; a directory of still frames delivers one image per camera frame.

## Input selection and looping

`--source` accepts a video or still image file, or a directory containing supported image/video files. A folder is listed once, **only at its top level**. Subdirectories, symlinks and unrelated files are ignored. Supported files are sorted lexicographically by filename, case sensitively, then replayed in that order. `10.png` sorts before `2.png`; use zero-padded filenames (`0001.png`, `0002.png`) for numeric sequences. The playlist repeats indefinitely, including a single selected file. Frame IDs and camera timestamps continue across file boundaries and loop restarts.

The playlist remains fixed for the lifetime of the connection. Stop and restart to pick up added or renamed files. Empty folders, missing sources and corrupt selected media return errors. Media is decoded with the bundled FFmpeg, or `CAPTUREFAB_FFMPEG=/absolute/path/to/ffmpeg` for a development build. Local replay does not open stream URLs or playlist manifests.

Supported images are PNG, JPEG, BMP, TIFF, WebP and PNM (PPM/PGM/PBM). Supported video extensions are MP4/M4V/MOV, MKV/WebM, AVI, MPG/MPEG/VOB, TS/MTS/M2TS, FLV, WMV and OGV. Decoder availability depends on the FFmpeg build. Reopening videos or image sequences at a playlist boundary can introduce a short decoding gap; camera timestamps remain monotonic.

Local replay also works with the existing GUI, sessions, captures and forwarding:

```sh
capturefab --camera 'sim:replay:/data/clip.mp4' capture -n 100 -o replay-frames
capturefab --camera 'sim:replay:/data/image-frames' capture \
  --set Width=1280 --set Height=720 --set AcquisitionFrameRate=15 \
  -n 60 -o snapshots
capturefab --camera 'sim:replay:/data/video-clips' gui
```

In the desktop address field, enter `sim:replay:` followed by the file or folder path. Local replay defaults to 640 × 480 RGB8 at 30 fps. Width, height and frame rate can be changed while acquisition is stopped. Pattern selectors (`sim:0`, `sim:line`, `sim:depth`, `sim:microscope`, `sim:thermal`) remain available.

## RTSP into an NVR or computer vision pipeline

```sh
capturefab simulate run --preset rtsp --source ./videos --duration 5m
ffmpeg -rtsp_transport tcp -i rtsp://127.0.0.1:8554/live -t 10 -f null -
```

Configure the NVR or pipeline with the printed RTSP URL and select TCP transport. The built-in listener serves H.264 directly; a separate stream server is not required. Each client receives the same ongoing camera timeline. Slow consumers receive recent frames through bounded queues.

## ONVIF onboarding

```sh
capturefab simulate run --preset onvif --source ./image-frames \
  --bind 192.168.1.50 --name 'Loading dock test camera'
capturefab onvif resolve http://192.168.1.50:8080/onvif/device_service --json
capturefab --camera 'onvif:http://192.168.1.50:8080/onvif/device_service' \
  capture -o onvif-test.png
```

Use the printed ONVIF device endpoint in the NVR's manual add flow, or WS-Discovery on the same subnet. The media profile resolves to the camera's RTSP stream. The simulation implements a useful subset of ONVIF Device and Media operations; it is not a certified ONVIF product. Authentication, PTZ, audio, motion events and vendor extensions are outside this simulation.

JPEG snapshots are available in builds with the `jpeg` feature (enabled by default). The ready state reports whether WS-Discovery could start; direct device endpoint connections work on loopback as well.

## GigE Vision acquisition

```sh
capturefab simulate run --preset gige --source ./image-frames \
  --bind 192.168.1.50
capturefab --camera 192.168.1.50 info
capturefab --camera 192.168.1.50 capture -n 30 -o gige-test
```

The virtual device responds to GigE discovery and exposes GenICam XML, control registers and GVSP image packets. An acquisition client controls start/stop and the stream destination. Use the standard UDP control port 3956 for ordinary camera clients. `--gige-port` supports isolated tests with a client that accepts a custom port. This is a test device, with a subset of GigE Vision rather than a compliance suite or a simulation of USB3 Vision hardware.

## Network configuration

Listeners default to loopback. For an NVR on another machine, set `--bind` to a reachable local LAN address. To listen on all interfaces, provide `--bind 0.0.0.0 --advertise LAN_IP`, so discovery and stream URLs contain the correct address. RTSP defaults to TCP port 8554 and ONVIF to HTTP port 8080; `--rtsp-port` and `--onvif-port` override them. The virtual camera has no credentials and is intended for a trusted integration test network.

Ctrl-C, duration expiry or a source failure shuts down listeners and owned decoder/encoder processes. Capture/recording storage policies continue to apply to files produced by Capturefab clients.
