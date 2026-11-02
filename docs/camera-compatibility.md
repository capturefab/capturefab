# Camera requirements and limits

Capturefab chooses an input path per source instead of assuming every camera speaks GenICam.
This page lists what each path needs and what it does not do. It is not a list of tested
cameras: support depends on the device, its firmware, the connection and the operating system,
and it may change between releases while the software develops quickly. Run `capturefab doctor`
for this computer's checks and `capturefab info -c CAM` for what a connected camera offers.

| Connection | `-c` selector | What Capturefab does |
| --- | --- | --- |
| GenICam GigE Vision | `list` ID (`gige:…`), unique serial or IPv4 address | Reads the camera's GenICam XML; every feature it describes can be read, written or run, within the camera's own access rules |
| GenICam USB3 Vision | `list` ID (`u3v:…`) or unique serial | The same, over USB; needs the `usb` build feature (on by default) |
| Webcam, built-in camera or UVC capture device | `list` ID: AVFoundation on macOS, V4L2 on Linux, DirectShow on Windows | Captures through FFmpeg; size and frame rate are configurable, other camera controls are not exposed |
| Linux CSI camera with a usable video node | `v4l2:/dev/videoN` | The same host path, when the node itself delivers a format FFmpeg's V4L2 input reads |
| ONVIF camera | `onvif://HOST` or `onvif:http://HOST/onvif/device_service` | Resolves Media1/Media2 profiles to RTSP and captures that stream; PTZ and imaging settings are not implemented |
| RTSP, SRT, HTTP, RTMP or UDP stream; video or image file | the URL or path | Decodes and captures; the stream's settings belong to its camera or server |
| DSLR or mirrorless camera | its clean HDMI output through a capture device, a UVC mode or a stream | Captures that video; USB/PTP tethering, shutter control and RAW download are not implemented |

USB3 Vision and USB Video Class are different protocols: a UVC webcam or HDMI capture device
uses the host camera path even when its cable is USB 3. Release downloads need no Aravis, libusb,
separately installed FFmpeg or camera vendor SDK.

## Operating system access

- **macOS**: the first camera or network command makes macOS ask for Camera and Local Network
  access for the app the command runs in, such as Terminal or an editor; allow it, or enable that
  app in System Settings › Privacy & Security. USB3 Vision needs an available IOKit interface.
- **Linux**: USB3 Vision needs the udev rule shipped in the archive (`capturefab doctor` prints
  it), webcams need access to `/dev/video*`, and GigE Vision benefits from a larger
  `net.core.rmem_max`. The [line computer guide](line-computer.md) covers each.
- **Windows**: USB3 Vision interfaces must use the WinUSB driver, and Windows Defender Firewall
  must allow Capturefab on private networks for GigE Vision streams to arrive.

## GigE Vision and USB3 Vision

Capturefab parses the camera's GenICam XML (zipped or plain, read from the device) and evaluates
features live against the camera's registers, so names, ranges, choices and locks are the
camera's own. GigE supports packet reordering, duplicate detection, bounded packet resend when the
camera advertises it, and packet-size negotiation; `capturefab network` checks the path to each
camera. GenICam XML hosted on a web server, multipart payloads, GenDC containers, chunk data and
point-cloud payloads are not supported.

Frames convert from every common PFNC pixel format (see [Pixel formats](#pixel-formats)). PNG
keeps monochrome samples wider than 8 bits as 16-bit gray; `raw` keeps the camera's bytes at full
bit depth. An unsupported format fails with a message naming it and can still be saved as `raw`.

## Pixel formats

Captures, inspections and recordings convert these PFNC layouts:

| Family | Converted layouts |
| --- | --- |
| Monochrome | Mono1p/2p/4p, Mono8/8s/10/12/14/16, Mono10p/12p/14p, Mono10/12Packed |
| Bayer | GR/RG/GB/BG at 8/10/12/14/16 bits, 10p/12p/14p and 10/12Packed |
| Interleaved color | RGB/BGR at 8/10/12/14/16 bits, RGB/BGR10p/12p, RGB10p32, RGB/BGR565p |
| Planar color | RGB8/10/12/16_Planar |
| Alpha/alignment color | RGBa/BGRa at 8/10/12/14/16 bits, RGBa/BGRa10p/12p, RGB8a32/BGR8a32; alpha/alignment is omitted from opaque images |
| YUV/YCbCr | Generic 8-bit 4:4:4, 4:2:2 and 4:1:1; YCbCr601/709 at those subsamplings, in the PFNC default and explicit Cb-first orders; semiplanar YCbCr420/422 `_YY_CbCr` and `_YY_CrCb` |
| Decoded video | Capturefab's own codes for what PFNC has none for: planar 4:2:0, 4:2:2 and 4:4:4 (`YCbCr420_8_Planar` and so on), and semiplanar 4:2:0 (NV12) in limited-range BT.601/709 and full-range BT.709 |
| Scalar measurements | Coord3D_C8/C16, Confidence8/16 as grayscale; floating-point depth (Coord3D_C32f) as raw only |

Names and component orders follow the
[EMVA pixel format values](https://www.emva.org/wp-content/uploads/GenICamPixelFormatValues.pdf)
and [PFNC 2.4](https://www.emva.org/wp-content/uploads/GenICam_PFNC_2_4.pdf). Generic YUV/YCbCr
uses full-range BT.601; explicitly named YCbCr601/709 uses video-range luma/chroma and its named
matrix, and Capturefab's `YCbCr709Full` full-range BT.709. Capturefab's own codes set PFNC's
custom bit (31) and follow its naming, for example `YCbCr601_420_8_Planar` (limited-range BT.601
4:2:0 in three planes) and `YCbCr709_420_8_YY_CbCr_Semiplanar` (limited-range BT.709 NV12); webcams,
streams and files arrive in them as FFmpeg decodes them. 4:1:1 requires widths divisible by four. A lone last 4:2:2 pixel uses neutral Cr. Packed
rows start on byte boundaries in the acquired frame; GigE also normalizes continuous LSB packing
when the shorter payload identifies it unambiguously. If payload padding or chunks hide that
distinction, rows retain the byte-aligned interpretation. Unpacked words are little-endian.

PNG keeps monochrome samples wider than eight bits, packed or not, as 16-bit gray with the samples
in the top bits and an sBIT chunk naming how many are significant. JPEG, PGM, PPM, color and Bayer
PNG, and inspection previews are opaque 8-bit images: monochrome samples below eight bits span
black to white, and higher-depth samples use their high eight significant bits; a capture result
reports `bit_depth` and, when the file keeps fewer bits than the frame, `source_bits`. Exposure
and auto metering use bounded, stratified samples to avoid locking onto one phase of stripes or
checkerboards. Auto metering and clipping counts retain native precision for packed mono/Bayer and
wide RGB, including cropped regions. Raw capture preserves the acquired frame buffer, including
its native component precision, and its sidecar names the PFNC format and code. Unsupported
layouts fail explicitly and can still be saved as raw.

Recording passes the sensor layouts FFmpeg accepts and converts the remaining supported formats to
gray/RGB24. `--codec ffv1` stores a Bayer mosaic itself as 8- or 16-bit gray (10- to 16-bit
mosaics as 16-bit), which decodes back to the exact mosaic; players show it in gray. It stores
RGB and BGR deeper than 8 bits, packed or not, as planar GBR of their own depth (`gbrp10le` to
`gbrp16le`), every bit kept. These checks establish conversion
behavior with synthetic payloads; they do not establish compatibility with a physical camera
model.

## Webcams and capture devices

`capturefab list --kind webcam` lists them. Their selectors accept `size` and `fps` options, such
as `avfoundation:Camera Name?size=1920x1080&fps=30`; quote the selector in a shell.
`capturefab features -c CAM VideoMode` lists the size and rate combinations the device offers, and
`--set VideoMode=1280x720@30` chooses one for a capture. On Linux the `/dev/v4l/by-id/` link of a
camera stays bound to it when node numbers change. Exposure, focus and white balance of host
cameras are not exposed; such cameras adjust themselves, so let them settle (`inspect` reports
`not_settled` while they do).

A video file used as a camera plays at its own frame rate for recordings, which `info` reports and
`record` uses by default, and a recording that encodes it waits for its encoder rather than drop a
frame; a capture reads it as fast as it asks. A network stream is already running when a command
connects to it, so it gets no `--warmup`. Streams reconnect by themselves when they end, backing off from 1 s to 30 s.

## Raspberry Pi and Jetson CSI cameras

A CSI connector alone is not a userspace camera interface. Capturefab captures from a V4L2 video
node through FFmpeg, so the node must deliver a format that input reads (YUV, RGB, greyscale,
MJPEG or H.264, or 8-bit Bayer without ISP processing) and take its format from the video node
rather than a media-controller pipeline. Raspberry Pi camera modules (libcamera) and Jetson Bayer
sensors (Argus) usually do not qualify. Publish those cameras as a stream with the platform's own
software, such as `rpicam-vid`, MediaMTX's Raspberry Pi camera source or `nvarguscamerasrc`, and
open its RTSP or SRT URL.

Choose the Linux release by the installed operating system: `dpkg --print-architecture` prints
`arm64` for the ARM64 release. There is no 32-bit ARM release.

## ONVIF streams

`capturefab onvif resolve HOST` lists every profile with a usable stream URI, trying Media1
profiles first and Media2 when the camera has no working Media1 service. Opening `onvif://HOST`
tries the resolved streams in the camera's order until one delivers a frame. Discovery and
resolution support WS-Security, HTTP Basic and Digest authentication; HTTPS SOAP endpoints are not
supported. Save a sign-in with `capturefab login add onvif://HOST --user USER --password-stdin`.

## Line scan, depth, thermal and microscope cameras

The camera's own features stay authoritative: Capturefab identifies a line scan, depth, thermal or
microscope camera from what it advertises, but adds no controls it lacks. Captures save each
acquired frame or line block. For quantitative depth or temperature data, capture `raw` and keep
the camera's calibration features (unit, scale, offset) with the files. PNG keeps 16-bit depth and
thermal samples as 16-bit gray but carries no calibration; JPEG and video are 8-bit
visualizations. `auto` exposure is for image cameras and returns `unsupported` (exit 5) for line
scan, depth and thermal sources.

## Video encoding

`record` probes encoders at the frame size and uses the first that works: Apple VideoToolbox,
NVIDIA NVENC, NVIDIA Jetson through GStreamer, AMD AMF, Intel Quick Sync, Vulkan Video, Windows
Media Foundation or Linux V4L2 where the build and drivers have them, then libx264 for H.264.
The bundled FFmpeg has no software HEVC or AV1 encoder, so `--codec hevc` or `av1` without a
working hardware encoder fails before recording with exit 5. An encoder being listed is not a
statement that a particular device works; the result's `encoder` field says which one ran.
