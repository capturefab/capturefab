<!-- Generated from docs/camera-compatibility.md by scripts/site-guides.py. Edit the source, then run the script. -->

# Camera compatibility

Capturefab chooses an input transport, rather than assuming every camera speaks
GenICam. The commands below capture from operating system camera devices and
network streams as well as industrial cameras. Run `capturefab doctor --json` and
`capturefab ffmpeg --json` to inspect the installed build's capabilities. Camera support and behavior may change between releases during rapid development.

| Camera connection | Capturefab path | Configuration available today |
| --- | --- | --- |
| GenICam GigE Vision | Discovered ID, unique serial, or IPv4 address | Camera XML features: read, write, execute; available controls depend on the camera |
| GenICam USB3 Vision | Discovered `usb:` ID or unique serial | Register-backed camera XML features; USB3 Vision transport requires the `usb` build feature |
| Document scanner | SANE `scanner:sane:DEVICE` on Linux, WIA `scanner:wia:ID` on Windows, ImageCaptureCore `scanner:ica:UUID` on macOS | On-demand page capture with resolution, mode and source settings; installed scanner driver and `scanner` build feature required; physical scanner compatibility has not been validated |
| Built-in laptop camera or USB webcam/UVC capture device | AVFoundation on macOS, V4L2 on Linux, DirectShow on Windows | Capture; configure native size/frame rate and advertised `VideoMode` choices before acquisition; host device exposure/focus controls are not currently exposed |
| Driver-backed Linux CSI camera | A usable `v4l2:/dev/videoN` endpoint | Same host capture path when the node delivers a format FFmpeg's V4L2 input supports; Raspberry Pi camera modules and Jetson Bayer sensors usually need a libcamera or Argus stream instead |
| ONVIF camera | `onvif:http://HOST/onvif/device_service` or authenticated locator | Discovery and Media1/Media2 profile/RTSP URI resolution; ONVIF PTZ and imaging configuration are not implemented |
| Camera on a Home Assistant, Frigate, go2rtc or UniFi Protect server | `hub:NAME/CAMERA` after `capturefab hub add-…` saves the server | Read-only frames from the hub's MJPEG stream, snapshots or RTSP restream; camera settings stay in the hub; `hubs` build feature required; not validated against physical cameras or particular hub versions |
| RTSP, SRT, HTTP, RTMP, UDP, or supported video file | Direct URL or file path | Decode and capture; source configuration belongs to its camera/server |
| DSLR/mirrorless clean HDMI | HDMI capture device visible to the operating system | Capture the HDMI video; camera menus configure exposure, focus, and HDMI output |
| DSLR/mirrorless native USB/PTP tethering | No Capturefab implementation | Vendor-specific PTP commands, shutter triggering, RAW download, and tethered live view are not implemented |

USB3 Vision and USB Video Class are different protocols. A UVC webcam or HDMI
capture device uses the host camera path even when its cable uses USB 3. Built-in
camera availability depends on operating system permissions and drivers. This
does not require Aravis, libusb, a separately installed FFmpeg, or a camera vendor
SDK in a release containing the matching embedded FFmpeg.

## Camera types and stream limits

Camera purpose and transport are separate. Area scan, line scan, depth, microscope, thermal and document scanner profiles adapt the interface using controls the source advertises:

| Profile | Useful controls when advertised | Display behavior |
| --- | --- | --- |
| Area scan | Image region, exposure, gain, frame timing | Complete image frames |
| Line scan | Line width, block height in lines, line rate, triggers, encoders | Recent received rows; gaps mark skipped blocks |
| Depth | Component selection, output mode, depth range, coordinate units/scale/offset | Scalar range colors with automatic raw sample bounds |
| Microscope | Focus, objective/magnification, illumination, stage | Image preview with focus region and sharpness tools |
| Thermal | Temperature output, range, emissivity, radiometric calibration | Scalar heatmap with automatic raw sample bounds |
| Document scanner | Resolution, mode, paper source | Page acquisition on demand |

Other control names remain accessible through the full feature list. Controls do not imply writable access: use `features`, choices in feature metadata and live bounds. Native host cameras expose their existing size/frame-rate/video-mode controls; identifying a microscope or thermal source does not add host-driver focus or radiometric controls.

Every camera identity includes `profile: {kind, source, evidence, capabilities}` in discovery, info and session status JSON. `kind` is `unknown`, `area_scan`, `line_scan`, `depth`, `microscope`, `thermal` or `document_scanner`; `source` is `unknown`, `transport`, `identity`, `features` or `device_metadata`. `evidence` explains the classification and `capabilities` lists advertised control families. A discovery profile may be provisional until connection reads GenICam features. Detection uses this priority:

1. A document scanner transport.
2. An explicit device class in `DeviceType`, `CameraType` or `SensorType`.
3. Explicit `DeviceScanType=Linescan` geometry.
4. Specialty feature families: depth/Scan3d output, radiometric temperature output, microscope focus with objective/magnification, or line-rate controls.
5. Remaining declared scan geometry, model identity, then conventional image dimensions and frame timing.

Absent evidence leaves the type unknown. Vendor branding, high bit depth, or `Height=1` alone do not identify depth, thermal or line scan cameras. Capabilities come from features independently of classification, and feature metadata remains the authority for current availability and settings.

The transport pipeline delivers one raster per acquired frame or line block. Depth cameras must expose a compatible single-component image stream, for example a scalar range plane; component controls only configure the device's advertised output. Recognizing depth metadata does not decode multipart payloads, GenDC containers, XYZ point-cloud payloads or proprietary vendor SDK protocols. Those paths remain unsupported. Configure a compatible single-component stream when the device offers one. No physical camera compatibility is implied by a detected profile.

The automatic GUI previews are display transformations. **Sensor pixels** returns to the ordinary sensor preview. Line history contains recently received display rows, can downsample large blocks, and marks skipped blocks because live consumers prefer the newest frame; it is not a complete encoder-aligned reconstruction. Saving captures each acquired block rather than the stitched history. Depth/thermal legends show raw sample bounds and automatically remap colors per image. They do not apply the device's measurement calibration. Coordinate previews honor an advertised Scan3dInvalidDataFlag/Value for the C coordinate; otherwise only nonfinite samples are marked invalid.

For quantitative depth or temperature data, capture `--format raw` and retain the selected component's calibration features, units, scale and offset. Raw files preserve acquired bytes and precision but do not embed that metadata; also retain frame dimensions and pixel format. PNG, JPEG, PGM, PPM and forwarded video are 8-bit visualizations without measurement calibration. Forwarding does not carry the GUI color map, including when a higher-bit-depth sensor format is passed to the encoder. Generic auto exposure is supported for image cameras with suitable controls, including microscopes; line scan, depth, thermal and document scanner sources return unsupported and need their specific timing or measurement controls.

## Typed simulators

`sim:line[:name]`, `sim:depth[:name]`, `sim:microscope[:name]` and `sim:thermal[:name]` exercise the specialized interface and automation without hardware. `sim:0` and arbitrary other simulator names retain the area-scan pattern camera.

```sh
capturefab --camera sim:line capture --set Height=1 \
  --set AcquisitionLineRate=2000 --set ExposureTime=100 -o line.png
capturefab --camera sim:depth get ComponentSelector Scan3dOutputMode \
  Scan3dCoordinateScale Scan3dCoordinateOffset Scan3dCoordinateUnit --json
capturefab --camera sim:depth capture --format raw -o range.raw
capturefab --camera sim:microscope capture --set FocusPosition=50 \
  --set Magnification=4 --set IlluminationIntensity=70 -o micrograph.png
capturefab --camera sim:thermal get TemperatureLinearResolution TemperatureUnit --json
capturefab --camera sim:thermal capture --format raw -o temperature.raw
```

The line simulator accepts single lines or blocks and schedules them using line rate and exposure. The depth simulator supplies `Coord3D_C16` samples with a millimeter scale. The microscope's focus, magnification and illumination change its image. The thermal simulator supplies `Mono16` samples at 0.01 Kelvin per raw value. These are synthetic sources, not hardware compatibility tests.

## Document scans

Run `capturefab scanners --json` for scanner IDs, or `discover --json` for all sources. Linux requires SANE's `scanimage` and suitable backends; Windows requires Windows PowerShell, the WIA service and a WIA driver; macOS uses a bundled ImageCaptureCore helper. Set a longer transport deadline for physical scanning:

```sh
capturefab --camera 'scanner:sane:DEVICE' --timeout-ms 60000 capture \
  --set ScanResolution=300 --set ScanMode=Gray -o document.png
```

Connecting and `start` do not initiate physical scanning. Each capture requests one page; `-n` saves successive pages as separate images, including feeder pages with `ScanSource=Feeder`. The default source preserves the driver's source choice. Exact supported modes, DPI and paper-source names depend on the driver. SANE's explicit source names are `Flatbed` and `Automatic Document Feeder`; drivers using other source names can use `ScanSource=Default`. Capture timeouts terminate the helper. The GUI has scanner controls, a separate 60-second default timeout and the existing preview. Samples normalize to 8-bit RGB/grayscale with a 128 MiB page bound. Scans share the existing destinations and storage ledger; scanner video forwarding, camera auto exposure, PDF assembly and OCR are not included.

## Built-in cameras and capture devices

General discovery runs network and host camera enumeration concurrently. If a
host backend cannot launch, is missing from FFmpeg, or times out, `capturefab
discover --json` retains other results and reports the failure in `warnings`.
The desktop keeps these warnings visible in its status notice and activity log;
hover over a clipped notice to read the full message. For slow host enumeration,
retry with a larger `--timeout-ms`.

List native camera locators first:

```sh
capturefab native --json
```

Use the locator returned for your device:

```sh
# macOS: video device index 0, without an audio input
capturefab --camera avfoundation:0 capture -o webcam.png

# Linux: select the actual capture node from native discovery
capturefab --camera v4l2:/dev/video0 capture -o webcam.png

# Windows: use the enumerated device name; cameras that share a name are
# listed by their DirectShow moniker, with its ? written %3F
capturefab --camera 'dshow:video=Camera Name' capture -o webcam.png
```

The GUI's direct locator field accepts these same inputs. On macOS, grant camera
access to the executable or terminal when the operating system requests it. On
Linux, grant your user access to the selected video node; its `/dev/v4l/by-id/`
or `/dev/v4l/by-path/` link also works and stays bound to the same camera when
node numbers change. A virtual camera or capture device must be visible to the
host API used by that build.

Native locators accept optional `size` and `fps`, for example
`avfoundation:Camera Name?size=1920x1080&fps=30`. Run `features` to inspect the
device's supported `VideoMode` choices, then use `--set VideoMode=WIDTHxHEIGHT@FPS`
for capture or `set VideoMode=...` in a stopped persistent session. AVFoundation
choices reflect frame rates the bundled FFmpeg input honors, and V4L2 choices the
frame intervals its driver reports for each format and size; a driver that reports
none leaves the accepted rates to the driver. V4L2 keeps sizes advertised across
raw and compressed inputs and selects the matching input format when the mode or
frame rate changes; when both offer the same size, raw is chosen whenever it
delivers the requested frame rate, and the fastest input when no rate is requested,
such as MJPEG at HD sizes on USB 2.0 webcams.
Unavailable cameras report permission, busy, non-capture node, or frame timeout
diagnostics.
Discovered host and ONVIF devices can also be selected by their unique serial;
Capturefab uses the discovered locator and the matching transport to open them.

## Raspberry Pi and Jetson CSI cameras

A CSI connector alone does not define a userspace camera interface. Capturefab
captures from a V4L2 video node through FFmpeg's V4L2 input, so the node must
deliver a format that input supports (YUV, RGB, 8- or 16-bit greyscale,
MJPEG/H.264, or 8-bit Bayer without ISP processing such as auto exposure or white
balance), and its driver must take the format from the video node rather than a
media-controller pipeline. Raspberry Pi camera modules and Jetson Bayer sensors
usually do not qualify: on Jetson, V4L2 delivers their raw 10- or 12-bit Bayer
data, and Raspberry Pi OS runs the CSI receiver as a media-controller pipeline
for libcamera (the only mode on Raspberry Pi 5). Capturefab does not currently
implement libcamera's pipeline API, the NVIDIA Argus API, or CSI sensor/PHY
drivers.

Publish those cameras as a stream with the platform software, for example
`rpicam-vid` or MediaMTX's Raspberry Pi camera source, or `nvarguscamerasrc` on
Jetson, and open its RTSP or SRT URL. For a YUV sensor or HDMI-to-CSI bridge
that qualifies, check native discovery, then capture from a node it reports.

Choose the Linux release matching the **installed operating system
architecture**: run `dpkg --print-architecture`, and `arm64` takes the ARM64
(`aarch64`) release. There is no 32-bit ARM release; an `armhf` system, such as
32-bit Raspberry Pi OS, needs the 64-bit OS instead, which Raspberry Pi 3 and
later run. `uname -m` cannot tell them apart there: it prints `aarch64` on 32-bit
Raspberry Pi OS too.

## Panasonic LUMIX GX85 through HDMI

Panasonic lists the DMC-GX85 as supporting clean HDMI and HDMI capture devices,
without direct streaming. Its specified HDMI connector is micro HDMI Type D.
Connect that output to a host-compatible HDMI capture device, then select the
capture device through `capturefab native --json`.
[Panasonic streaming compatibility](https://help.na.panasonic.com/answers/specifications-lumix-dsc-cameras-and-panasonic-camcorders-live-video-streaming-compatibility/),
[GX85 specifications](https://help.na.panasonic.com/answers/features-and-specifications-lumix-g-series-dmc-gx85/).

On the GX85, use **Setup → TV Connection → HDMI Info Display (Rec) → OFF** to
remove display information from the HDMI image. Panasonic's advanced manual
describes live HDMI monitoring on page 293, including mode and output limitations.
Match the camera output to a format the capture device accepts; validate a frame
before a long recording. Use suitable power and check the camera's own recording,
sleep, and thermal behavior for the intended duration.
[GX85 advanced manual, page 293](https://help.na.panasonic.com/wp-content/uploads/2023/02/DMCGX85_SQW0669_ENG.pdf).

```sh
# Substitute the host locator for the HDMI capture device.
capturefab --camera avfoundation:1 capture -o gx85.png
capturefab --camera avfoundation:1 forward \
  --output gx85.mp4 --encoder auto --fps 30 --bitrate 4M --duration 60
```

This route captures the video output. It does not expose the GX85's proprietary
USB tethering commands or make its exposure controls into GenICam features.
Other DSLR and mirrorless models may provide clean HDMI, a native UVC mode, a
vendor webcam driver, or a network stream; use whichever documented route is
actually exposed by that model and operating system.

## ONVIF stream profiles

Automatic ONVIF resolution tries every advertised Media1 profile, or Media2
profile when the camera has no working Media1 service, and returns the ones with
usable stream URIs. A failed profile does not prevent connection through
another profile. When inspecting a specific profile with `capturefab onvif resolve
http://HOST/onvif/device_service --profile TOKEN`, its failure is reported directly. If no profile resolves, the
error includes the last stream-resolution failure. Connecting automatically tries
resolved streams in the camera's advertised order until one supplies a decodable
frame. If every stream fails to open, the error retains the last opening failure.

## Camera hubs

A camera hub is a Home Assistant, Frigate, go2rtc or UniFi Protect server that
already shows the cameras. Capturefab reads frames from the hub's HTTP API and
RTSP restream; it never contacts the cameras behind it, so it reaches only the
cameras and streams the hub itself offers. Save the server, then list its
cameras:

```sh
capturefab hub add-frigate nvr --url http://nvr.local:5000
capturefab hub cameras nvr --json
capturefab --camera 'hub:nvr/front?stream=snapshot&fps=0.5' capture -o front.png
```

| Hub | Frames, in `stream=auto` order | Notes |
| --- | --- | --- |
| Home Assistant | MJPEG camera stream, then snapshots | Long-lived access token; the stream sends nothing while the image is unchanged, so use `stream=snapshot` for static scenes |
| Frigate | RTSP restream when the camera has one, then snapshots; MJPEG on request | Port 5000 without a user, or 8971 with `--user`; the restream port (8554, `--rtsp-port`) must allow unauthenticated RTSP |
| go2rtc | RTSP restream when its RTSP server is reachable, then snapshots; MJPEG on request for JPEG sources | Optional API user and password |
| UniFi Protect | Snapshots only, at most one per second | API key for the integration API over HTTPS; Capturefab does not create RTSPS streams on the console |

Hub cameras are read-only: `features` lists their size and frame rate, and
settings are changed in the hub. JPEG and PNG stills and MJPEG are decoded in
the camera worker; RTSP restreams are decoded by FFmpeg. Each stream attempt allows
at least 12 seconds for its first frame (15 seconds over RTSP); API checks
and automatic fallback can extend the total connection time. Streams reconnect with backoff;
a refused secret or certificate stops acquisition. Hub recordings, events and
PTZ are not implemented. Listing a hub kind here is not a compatibility claim
for any hub version or camera; check a frame before relying on a hub camera.

## Storage and acceleration

Image capture and local video recording share a Capturefab-owned file budget
across cameras, processes, and output directories using the same private session
directory. A persisted global ceiling initially stops at 10 GiB or 10,000 files;
per-command limits can lower it. Existing files are never
silently overwritten. Explicit oldest-file retention only removes completed,
unchanged Capturefab files, and protects active recordings and files whose
ownership cannot be verified after interruption. `capturefab storage --json`
reports the allocation ledger. `capturefab storage configure --max-space 2GiB
--max-files 1000 --on-full stop` saves a ceiling for future allocations. A local
recording also has a bounded per-file reservation and stops when that reservation
is reached; it does not currently roll into another segment automatically.

The iced GUI renders through wgpu on the platform graphics API, or in software without a usable GPU. FFmpeg decode and
encoder acceleration depend on the embedded build, codec, operating system,
driver, and available device. Automatic encoder selection tests candidates at
runtime and falls back to software. An encoder being listed does not establish
that a particular NVIDIA, Apple, AMD, Qualcomm, ARM, or Intel device is usable.
