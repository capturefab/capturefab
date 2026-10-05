# Hardware validation — 2026-10-04

The native Rust GigE backend was tested against two independently running Aravis software cameras on macOS ARM64 and three physical Basler cameras on an NVIDIA Jetson running Ubuntu 20.04.6 / Jetson Linux R35.5.0. Capturefab was built natively on the Jetson with Rust 1.99, `cargo build --release --no-default-features --features usb`. The CLI executable was approximately 2 MiB and linked only the operating system's C runtime libraries, with no Aravis or Basler SDK.

| Camera | Transport / address | Result |
| --- | --- | --- |
| Aravis Fake, CF-ARV-001 | GigE, 127.0.0.1 | Discovery, XML, configuration, streaming, heartbeat, release/reopen |
| Aravis Fake, CF-ARV-002 | GigE, 192.168.2.1 | Discovered alongside the loopback fixture with separate stable IDs |
| Basler a2A1920-51gcIP67 (camera A) | GigE, private LAN | Discovery, compressed XML, feature reads, three complete 1920×1200 BayerRG8 frames saved to PNG |
| Basler a2A1920-51gcBAS (camera B) | GigE, private LAN | Discovery, compressed XML, feature reads, three complete 1920×1200 BayerRG8 frames saved to PNG at the camera's configured 1 Hz |
| Basler a2A1920-51gcBAS (camera C) | GigE, private LAN | Discovery and XML inspection; refused to acquire a camera already controlled by another application |

The Aravis fixture produced three complete 320×240 frames in each of Mono8, RGB8, BayerRG8, and Mono16. A four-second idle interval preserved control; dropping the backend released control and reopening acquired it again. Both fixtures were found by a single bounded discovery call after local interface unicast probes were added. The protocol unit tests also cover stale acknowledgements, request retries, malformed packets, out-of-order payloads, missing fragments, duplicate fragments, extended frame IDs, image padding, ZIP ambiguity, and bounded incomplete-frame storage.

The physical Basler tests used Capturefab's own GVCP/GVSP implementation and interpreted the camera's GenICam XML. Camera A initially reported `AcquisitionStart` unavailable because it remained in its standard device-register streaming mode. Executing its advertised `DeviceRegistersStreamingEnd` command made acquisition available. The first saved PNG was visually inspected and contained the real camera scene, with correct dimensions and color.

The application that normally holds camera B was stopped only for this capture test and restarted afterwards, and its original pipelines were confirmed running again. The other camera's active controller was respected throughout.

After the multi-camera rewrite, the final source snapshot was uploaded and rebuilt natively on the same Jetson. The final validation passed discovery with USB3 support compiled in (all three Basler cameras found, no discovery warnings), native V4L2 device enumeration (two ZS CAMERA device nodes), and a persistent headless session with two simulated cameras. The two cameras had distinct operating system worker PIDs, both streamed simultaneously, and two concurrent CLI requests each saved three complete 640×480 PNGs. Camera A then joined as a third distinct worker and saved three further 1920×1200 PNGs while the simulated workers continued streaming. The session exited successfully on SIGINT, all three workers exited, and the camera controller was released. No other applications were stopped for these final tests.

These checks establish real GigE interoperability on macOS ARM64 and Linux ARM64, including a Linux ARM64 session with three independent camera workers. They do not establish USB3 Vision hardware interoperability, Windows hardware interoperability, sustained multi-camera throughput, packet resend recovery, multipart/chunk/GenDC payload support, or vendor-specific NVIDIA hardware encoding. Those require separate hardware tests. Native V4L2 enumeration does not imply a UVC camera is a USB3 Vision camera; no physical USB3 Vision camera was available for capture validation.

## Latest transport and native mode checks

The packet resend and packet-size negotiation additions passed 30 GigE unit tests
on macOS ARM64. A local fake camera exercises standard and extended resend
commands, recovered payload packets, unavailable packets, capability fallback,
negotiated packet sizes, preserved stream flags, stale test packets, and transport
counters over real UDP sockets. Synthetic assembly tests exercise retries,
rate limits, missing leaders/trailers, block-ID wrap, reordering and bounded loss.
These tests validate protocol behavior; the earlier physical Basler checks predate
these changes and do not establish physical resend recovery or jumbo-frame paths.
GenICam bounds and host selector changes passed 14 tests, including Basler-shaped
XML value chains, converters, dynamic limits and indexed selectors.

The native webcam mode additions passed nine media unit tests and five real FFmpeg
integration tests, covering mode listings, configuration, decoder/encoder bounds,
cancellation and recording failures. With the source-built macOS ARM64 FFmpeg,
the Logitech HD Pro Webcam C920 captured a complete 1920×1080 RGB frame using its
default selected mode, then a complete 1280×720 frame after setting
`VideoMode=1280x720@10` and restarting its decoder. Private capture artifacts are
kept outside the repository. FaceTime HD Camera initially produced no frame within
five seconds and returned exit 4 with an actionable timeout message. After the
user opened the MacBook lid, an immediate retry captured a complete 1552×1552 RGB
frame and exited successfully. This establishes built-in capture with the camera
available; it does not establish capture while the lid is closed. Windows
DirectShow and Linux V4L2 mode changes remain unverified on hardware.

## Auto mode and packet resend on a physical Basler camera

On 2026-10-05 the headless release build (Linux ARM64, Debian bullseye toolchain) ran against the free
Basler a2A1920-51gcIP67 (camera A, 1920×1200 BayerRG8, 1 GbE, MTU 1500) on the Jetson, in a dim indoor
scene. Other cameras on the same switch were not touched. Each run used a persistent headless session.

| Mode | Measured fps | Exposure | Gain | `DeviceLinkThroughputLimit` |
|---|---|---|---|---|
| Manual (camera as found) | 28.2 | 10 ms (firmware auto, pinned) | 13.3 dB | 75.7 MB/s |
| Auto, balance 0.5 | 24.2 | 41 ms cap | 15 dB cap | 71.9 MB/s |
| Auto, balance 1 | 40.8 (camera reports 42.7) | 20.6 ms cap | 24 dB cap | raised to 115 MB/s |
| Auto, balance 0 | 11.2 | 82 ms cap | 6 dB cap | — |

Auto mode used the camera's firmware ExposureAuto, GainAuto and BalanceWhiteAuto and wrote their limits
for each balance. The state was reported as `limited` with the "too dark for this balance" note, which
matched the scene. The throughput limit was raised in steps while no frames were lost. Right after auto
mode starts, the camera itself skips a few frames while its firmware reconverges; the controller treats
that as loss, cuts bandwidth once, then raises it again within about 30 s.

Packet resend worked on the physical link. In manual mode the receiver requested 1,467 packets in 10 s
and recovered 1,442 of them, frames the previous receiver would have discarded. During 15 s of 1%
injected packet loss (`iptables` statistic drop, removed afterwards) the receiver recovered about 2,450
of 2,580 requested packets and the controller reduced bandwidth. `manual --revert` restored every
changed feature; `AutoGainUpperLimit` came back within one raw register step (23.999995 vs 24.000003 dB)
because of the camera's dB converter. After each run the camera's controller privilege, heartbeat
timeout (5000 ms) and stream packet size were unchanged.

Not covered here: jumbo frames (the link is MTU 1500, so negotiation selected 1500), several auto-mode
cameras sharing one link, and USB3 Vision hardware.
