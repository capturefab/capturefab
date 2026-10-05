# Camera compatibility

Capturefab chooses an input transport, rather than assuming every camera speaks
GenICam. The commands below capture from operating system camera devices and
network streams as well as industrial cameras. Run `capturefab doctor --json` and
`capturefab ffmpeg --json` to inspect the installed build's capabilities. The
[hardware validation record](hardware-validation.md) distinguishes device tests
from protocol and simulated tests.

| Camera connection | Capturefab path | Configuration available today |
| --- | --- | --- |
| GenICam GigE Vision | Discovered ID, unique serial, or IPv4 address | Camera XML features: read, write, execute; available controls depend on the camera |
| GenICam USB3 Vision | Discovered `usb:` ID or unique serial | Register-backed camera XML features; USB3 Vision transport requires the `usb` build feature |
| Built-in laptop camera or USB webcam/UVC capture device | AVFoundation on macOS, V4L2 on Linux, DirectShow on Windows | Capture; configure native size/frame rate and advertised `VideoMode` choices before acquisition; host device exposure/focus controls are not currently exposed |
| Driver-backed Linux CSI camera | A usable `v4l2:/dev/videoN` endpoint | Same host capture path; platform driver and pipeline setup are required |
| ONVIF camera | `onvif:http://HOST/onvif/device_service` or authenticated locator | Discovery and Media1 profile/RTSP URI resolution; ONVIF PTZ and imaging configuration are not implemented |
| RTSP, SRT, HTTP, RTMP, UDP, or supported video file | Direct URL or file path | Decode and capture; source configuration belongs to its camera/server |
| DSLR/mirrorless clean HDMI | HDMI capture device visible to the operating system | Capture the HDMI video; camera menus configure exposure, focus, and HDMI output |
| DSLR/mirrorless native USB/PTP tethering | No Capturefab implementation | Vendor-specific PTP commands, shutter triggering, RAW download, and tethered live view are not implemented |

USB3 Vision and USB Video Class are different protocols. A UVC webcam or HDMI
capture device uses the host camera path even when its cable uses USB 3. Built-in
camera availability depends on operating system permissions and drivers. This
does not require Aravis, libusb, a separately installed FFmpeg, or a camera vendor
SDK in a release containing the matching embedded FFmpeg.

## Built-in cameras and capture devices

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

# Windows: use the enumerated device name
capturefab --camera 'dshow:video=Camera Name' capture -o webcam.png
```

The GUI's direct locator field accepts these same inputs. On macOS, grant camera
access to the executable or terminal when the operating system requests it. On
Linux, grant your user access to the selected video node. A virtual camera or
capture device must be visible to the host API used by that build.

Native locators accept optional `size` and `fps`, for example
`avfoundation:Camera Name?size=1920x1080&fps=30`. Run `features` to inspect the
device's supported `VideoMode` choices, then use `--set VideoMode=WIDTHxHEIGHT@FPS`
for capture or `set VideoMode=...` in a stopped persistent session. AVFoundation
choices reflect frame rates the bundled FFmpeg input honors; V4L2 lists dimensions
without frame-rate bounds, so its driver still determines accepted rates.
Unavailable cameras report permission, busy, or frame timeout diagnostics.

## Raspberry Pi and Jetson CSI cameras

A CSI connector alone does not define a userspace camera interface. Capturefab
can open a Linux camera pipeline that exposes a capture-capable V4L2 video node.
Some Raspberry Pi/libcamera and Jetson/Argus camera pipelines require their own
platform software, sensor driver, media-controller configuration, or an exported
stream before they expose usable video. Capturefab does not currently implement
libcamera's pipeline API, the NVIDIA Argus API, or CSI sensor/PHY drivers.

Check native discovery, then capture from a node it reports. If the platform only
exports a network stream, use its RTSP or other supported URL. Choose the Linux
release matching the **installed operating system architecture**: `aarch64` for
64-bit ARM or `armv7` for a supported 32-bit ARM system.

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

The egui GUI uses the selected platform graphics renderer. FFmpeg decode and
encoder acceleration depend on the embedded build, codec, operating system,
driver, and available device. Automatic encoder selection tests candidates at
runtime and falls back to software. An encoder being listed does not establish
that a particular NVIDIA, Apple, AMD, Qualcomm, ARM, or Intel device is usable.
