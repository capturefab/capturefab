# Camera requirements and limits

Use the table to choose a camera selector for your connection. Available features depend on the
camera, its firmware and the host. Run `capturefab doctor` to diagnose the host. Run
`capturefab info -c CAM` to see what a connected camera reports.

| Connection | `-c` selector | What Capturefab does |
| --- | --- | --- |
| GenICam GigE Vision | `list` ID (`gige:…`), unique serial or IPv4 address | Uses the camera's GenICam XML to read, write or run supported features within the camera's access rules. |
| GenICam USB3 Vision | `list` ID (`u3v:…`) or unique serial | Uses GenICam over USB. The default build includes the `usb` feature. |
| Webcam, built-in camera or UVC capture device | `list` ID for AVFoundation on macOS, V4L2 on Linux or DirectShow on Windows | Opens the device through FFmpeg. See the webcam section for video mode settings. |
| Linux CSI camera with a usable video node | `v4l2:/dev/videoN` | Opens the node when it delivers a format FFmpeg's V4L2 input accepts. |
| ONVIF camera | `onvif://HOST` or `onvif:http://HOST/onvif/device_service` | Finds RTSP streams through Media1 or Media2 profiles. |
| RTSP, SRT, HTTP(S), RTMP or UDP stream, or video or image file | The URL or path | Decodes the source. Adjust stream settings on the camera or server. |
| DSLR or mirrorless camera | The ID of an HDMI capture device, a UVC camera mode or a stream URL | Captures video from the selected output. Use the camera's own software for tethering, shutter control or RAW downloads. |

USB3 Vision and USB Video Class (UVC) are different protocols. A UVC device uses the webcam path
even when its cable is USB 3. Release binaries include FFmpeg and handle GigE Vision and USB3
Vision directly.

## Operating system access

- **macOS.** If macOS prompts for Camera or Local Network access, allow it for the app that runs
  Capturefab. You can also grant access in System Settings › Privacy & Security. USB3 Vision
  uses an available IOKit interface.
- **Linux.** Give the Capturefab user access to the USB or video device. The release archive
  includes a udev rule for USB3 Vision. For GigE Vision, examine the receive buffer limit
  (`net.core.rmem_max`). See the [line computer guide](line-computer.md).
- **Windows.** Use the WinUSB driver for USB3 Vision interfaces. Allow Capturefab through the
  firewall on the private network that carries GigE Vision traffic.

## GigE Vision and USB3 Vision

Capturefab reads zipped or plain GenICam XML from the camera. It uses the camera's registers
to evaluate feature values, ranges, choices and locks. GigE Vision supports packet reordering,
duplicate detection and packet-size negotiation. It can request packet resend when the camera
advertises that feature. Run `capturefab network` to examine the path to each camera.

Unsupported inputs include GenICam XML hosted on a web server, multipart payloads, GenDC
containers, chunk data and point-cloud payloads.

PNG keeps monochrome samples wider than 8 bits as 16-bit gray. Use `raw` to keep the acquired
bytes at full bit depth or to save an image that uses a format Capturefab cannot convert.
See [Pixel formats](#pixel-formats).

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

Names and component orders follow the EMVA specifications.
See [pixel format values](https://www.emva.org/wp-content/uploads/GenICamPixelFormatValues.pdf)
and [PFNC 2.4](https://www.emva.org/wp-content/uploads/GenICam_PFNC_2_4.pdf).

Generic YUV and YCbCr use full-range BT.601. Formats named YCbCr601 or YCbCr709 use video
range and the named matrix. Capturefab's `YCbCr709Full` uses full-range BT.709.
Capturefab sets PFNC bit 31 in its custom codes for decoded formats.
For example, `YCbCr601_420_8_Planar` is limited-range BT.601 with three planes.
`YCbCr709_420_8_YY_CbCr_Semiplanar` is limited-range BT.709 NV12.

Widths for 4:1:1 must be divisible by four. A lone last 4:2:2 pixel uses neutral Cr.
Packed rows start on byte boundaries. When the payload length identifies continuous LSB packing,
Capturefab converts it to byte-aligned rows. If the payload length is ambiguous, Capturefab
uses the byte-aligned interpretation. Unpacked words are little-endian.

PNG stores monochrome samples above 8 bits in the top bits of 16-bit gray. The sBIT chunk gives
the number of significant bits. JPEG, color and Bayer PNG, and inspection previews use 8-bit
samples. Lower-depth monochrome samples scale from black to white. Higher-depth samples use the
high eight bits.

The capture result gives `bit_depth` and, if conversion loses bit depth, `source_bits`.
Metering samples across the image to avoid favoring one part of a repeating pattern.
Metering and clipping counts keep native precision for packed mono, Bayer and wide RGB.
Raw capture preserves the acquired frame buffer. The sidecar gives the PFNC format and code.

Recording passes sensor layouts that FFmpeg accepts. Other supported formats convert to gray or RGB.
With `--codec ffv1`, a Bayer mosaic stays intact as 8- or 16-bit gray. Video players display
the mosaic in gray. FFV1 also preserves RGB and BGR samples above 8 bits as planar GBR
(`gbrp10le` through `gbrp16le`).

## Webcams and capture devices

Run `capturefab list --kind webcam` to get device IDs. Webcam selectors accept `size` and
`fps` options, such as `'avfoundation:Camera Name?size=1920x1080&fps=30'`. Quote the selector
in a shell. Run `capturefab features -c CAM VideoMode` to see the available sizes and rates.
To choose a mode for capture, pass an available value with `--set VideoMode=1280x720@30`.
On Linux, use a `/dev/v4l/by-id/` link when available to keep the selector stable.

Let the webcam adjust its own exposure, focus and white balance. If `inspect` reports
`not_settled`, measure again after the image stabilizes.

For a video file, `record` uses the source frame rate by default. Run `info` to see that rate.
When encoding a file source, recording waits for the encoder to accept each frame. Still capture
reads frames on demand. Live streams skip `--warmup`. If a live stream ends, Capturefab tries
to reconnect with delays from 1 to 30 seconds.

## Raspberry Pi and Jetson CSI cameras

A CSI connector alone is not a userspace camera interface. Capturefab captures from a V4L2 video
node through FFmpeg, so the node must deliver a format that input reads (YUV, RGB, greyscale,
MJPEG or H.264, or 8-bit Bayer without ISP processing) and take its format from the video node
rather than a media-controller pipeline. Raspberry Pi camera modules (libcamera) and Jetson Bayer
sensors (Argus) usually do not qualify. Publish those cameras as a stream with the platform's own
software, such as `rpicam-vid`, MediaMTX's Raspberry Pi camera source or `nvarguscamerasrc`, and
open its RTSP or SRT URL.

Use the Linux ARM64 release on a 64-bit ARM operating system. On Debian-based systems,
`dpkg --print-architecture` prints `arm64` for that architecture. Build from source for 32-bit ARM.
See the [embedded Linux guide](embedded.md).

## ONVIF streams

`capturefab onvif resolve HOST` lists profiles with usable stream URIs. It tries Media1 first,
with Media2 as a fallback. Opening `onvif://HOST` tries the resolved streams in the camera's
order until one delivers a frame.

Profile resolution uses HTTP SOAP and supports WS-Security, HTTP Basic and HTTP Digest sign-in.
Use the camera's HTTP device-service URL for SOAP requests. Save a sign-in with
`capturefab login add onvif://HOST --user USER --password-stdin`. Use the camera's own controls for
pan, tilt, zoom and imaging settings.

## Line scan, depth, thermal and microscope cameras

Capturefab identifies line scan, depth, thermal and microscope cameras from the features they
report. Captures save each acquired frame or line block. For quantitative depth or temperature
data, capture `raw`. Save the camera's calibration values, such as unit, scale and offset, with
the files. PNG can store 16-bit depth and thermal samples as 16-bit gray without calibration.
For line scan, depth and thermal sources, set acquisition controls directly. `auto` returns
`unsupported` (exit 5) for these sources.

## Video encoding

`record` probes available encoders for the input format and frame size. For H.264, it can fall
back to libx264. HEVC and AV1 encoding use a working hardware encoder with the bundled FFmpeg.
If the selected codec has no usable encoder, `record` returns exit 5.

On Linux and macOS, `record` can also copy compatible H.264 or HEVC input directly. This bypasses
encoding. Read the result's `encoder` field to see which path it used.
