<!-- Generated from docs/camera-compatibility.md by scripts/site-guides.py. Edit the source, then run the script. -->

# Camera requirements and limits

Use the table to choose a camera selector for your connection. Available features depend on the
camera, its firmware and the host. Run `capturefab doctor` to diagnose the host. Run
`capturefab info -c CAM` to see what a connected camera reports.

| Connection | `-c` selector | What Capturefab does |
| --- | --- | --- |
| GenICam GigE Vision | `list` ID (`gige:…`), unique serial or IPv4 address | Uses the camera's GenICam XML to read, write or run supported features within the camera's access rules. |
| GenICam USB3 Vision | `list` ID (`u3v:…`) or unique serial | Uses GenICam over USB. The default build includes the `usb` feature. |
| Installed GenTL producer | `list` ID (`gentl:…`) | Uses an installed vendor CTI producer and its drivers through an isolated helper. The default build includes the `gentl` feature. |
| Webcam, built-in camera or UVC capture device | `list` ID for AVFoundation on macOS, V4L2 on Linux or DirectShow on Windows | Opens the device through FFmpeg. See the webcam section for video mode settings. |
| Linux CSI camera with a usable video node | `v4l2:/dev/videoN` | Opens the node when it delivers a format FFmpeg's V4L2 input accepts. |
| ONVIF camera | `onvif://HOST` or `onvif:http://HOST/onvif/device_service` | Finds RTSP streams through Media1 or Media2 profiles. |
| RTSP, SRT, HTTP(S), RTMP or UDP stream, or video or image file | The URL or path | Decodes the source. Adjust stream settings on the camera or server. |
| DSLR or mirrorless camera | The ID of an HDMI capture device, a UVC camera mode or a stream URL | Captures video from the selected output. Use the camera's own software for tethering, shutter control or RAW downloads. |

USB3 Vision and USB Video Class (UVC) are different protocols. A UVC device uses the webcam path
even when its cable is USB 3. Release binaries include FFmpeg and handle GigE Vision and USB3
Vision directly.

## Receive an existing GigE multicast stream

Use an explicit monitor selector when another application configures the camera and
starts its multicast acquisition:

```sh
capturefab --json info -c 'gige-monitor://192.0.2.20?interface=192.0.2.10&mac=020000000001'
capturefab capture -c 'gige-monitor://192.0.2.20?interface=192.0.2.10&mac=020000000001' -o frame.png
```

Replace the example addresses and MAC with the camera and a local host interface. Both
numeric IPv4 addresses and the camera's 12 hexadecimal MAC digits are required. Obtain
the MAC through ordinary discovery or your device inventory. The MAC is checked against
the camera's reported identity; it is not authentication. Query ordering and hexadecimal
case are normalized to one connection identity. The selector accepts no credentials,
authority port, additional query keys or interface aliases. The camera must be on the
selected interface's IPv4 subnet.

The controller must enable multicast on stream channel 0, start acquisition, and keep
its configuration stable while a monitor is connected. Capturefab reads the existing
destination group, destination port, packet size and advertised stream source port.
It requires a multicast destination, nonzero ports, a packet size of 576–9000 bytes, an
advertised readable source port, and a declared payload size of 1–128 MiB. A controller
that forbids other applications from reading camera parameters cannot be monitored
through this mode. Configure the switch and host firewall to deliver the multicast
stream on the chosen interface.

The monitor sends bounded GVCP register/memory reads and joins the multicast group.
It never acquires control privilege, sends a heartbeat or resend request, changes stream
configuration, latches a device clock, fires triggers, or starts/stops camera acquisition.
Host start/stop operations manage only this receiver. Raw frames, supported multipart
components and chunks use the ordinary capture formats and storage paths. Chunk values
requiring live selector state remain unresolved; their original bytes are retained.

`info` adds `camera_access: "read_only_monitor"` and `acquisition_control: "external"`.
`features` retains each XML `access` declaration and adds effective `writable: false`.
Unbanked feature, XML and bounded memory/register reads remain available. Settings,
commands, banked reads, saved-setup reads/comparisons, auto exposure, network tuning and
write-only action keys are unsupported. Default or zero warmup does no device settling;
explicit positive warmup and capture freshness requests are unsupported. Inspection
frame-discard counts refer only to received frames and do not prove sensor freshness.
Recording `--fps` controls the local encoder's output rate.

Every accepted datagram must match the observed camera IP/source port, multicast
destination and interface index. Foreign traffic has a finite packet/byte allowance
under the frame wait's deadline. Missing packet-interface evidence or a selected decode
failure retires the receiver rather than changing settings or taking control. The
connection uses its observed route and payload bounds without polling for configuration
changes; it cannot detect a change and restoration between observations. Raw device
ticks do not establish clock synchronization or an acquisition epoch.

Multiple monitor processes can coexist with a controller. A monitor holds a shared
local presence reservation keyed by physical MAC. Capturefab ForceIP takes an exclusive
reservation and is refused while any cooperating monitor remains connected. This
coordination covers processes using the same host/user/runtime, with the existing
controller reservation retained; it does not exclude other SDKs, runtimes or computers.

The workflow is documented in Basler's [multicast sample](https://docs.baslerweb.com/pylonapi/pylon-sdk-samples-manual#grab_multicast)
and the Blackfly S [BFS-PGE-70S7 network guide](https://softwareservices.flir.com/BFS-PGE-70S7/latest/40-Installation/ConfiguringNetwork.htm).
These describe the controller/receiver arrangement; device firmware, parameter access,
payload format and network configuration still determine whether a particular camera
meets the requirements above.

## Opaque GenICam registers

`features` and `get` show an opaque XML `Register` as a small
`{"kind":"register","length_bytes":N}` descriptor. These commands do not read the
buffer body, and register descriptors are excluded from scalar presets and checkpoints.
Lengths above the transfer cap remain metadata within the interpreter's existing 1 MiB
register-layout limit.
For an explicit whole-buffer transfer over native GigE Vision, USB3 Vision or an installed
GenTL producer, use:

```sh
capturefab --json debug read -c CAMERA --feature 'LUTValueAll[LUTSelector=LUT1]' --timeout 5s
capturefab --json debug write -c CAMERA --feature 'LUTValueAll[LUTSelector=LUT1]' --hex "$HEX_BYTES"
```

Set `HEX_BYTES` to exactly the declared number of hexadecimal bytes. Named transfers
accept 1–65536 bytes. Read JSON contains
`feature`, the resolved `bank`, `address`, `bytes` and exact lowercase `hex`; write JSON
contains `feature`, `bank`, `address` and `written`. Bytes retain their original order and
encoding. Native GigE named writes require a four-byte aligned address and length;
unaligned whole buffers are refused rather than implicitly read and rewritten. USB and
GenTL retain byte-granular writes. The existing numeric-address debug forms remain available.

Large hex values may exceed the operating system's command-line limit. Use the same
bounded write through JSON on stdin for the full 64 KiB cap:

```sh
capturefab --json debug rpc -c CAMERA < write-register.json
```

The file contains one request object, replacing `EXACT_HEX_BYTES` with the complete raw
hex sequence (131072 characters for 65536 bytes):

```json
{"op":"write_register","name":{"name":"LUTValueAll"},"hex":"EXACT_HEX_BYTES","timeout_ms":5000}
```

Optional banks use `"bank":[{"selector":"LUTSelector","value":"LUT1"}]` inside `name`.
The RPC reads at most 1 MiB from stdin and returns the same `feature`, `bank`, `address`
and `written` fields. A script may pipe the JSON instead of using file redirection; in
PowerShell, use `Get-Content -Raw write-register.json | capturefab --json debug rpc -c CAMERA`.
Run this directly: Capturefab's `shell` command owns stdin and refuses `debug rpc`.

Only XML-declared selectors may be supplied in a named register bank. The selected
register's live access, lock, address and length are checked after bank selection. One
operation budget (default 5s, at most 30s) covers selector and dependency I/O, body transfer
and selector restoration after the camera is opened. No new remote work starts after its
cutoff. Unsupported transports refuse the bounded operation. A bounded USB control stall
fails immediately; its ordinary unbounded endpoint recovery is not entered. If selector
restoration cannot finish, the command reports that failure and may leave the selected bank.

A raw write can leave a committed prefix when it fails. Capturefab performs no backup,
rollback, implicit readback, file operation or flash commit. Follow the camera's required
setup: [FLIR's LUT guide](https://softwareservices.flir.com/BFS-U3-51S5/latest/Model/public/LUTControl.html)
requires disabling the LUT before customization, and
[Oryx FileAccessBuffer](https://softwareservices.flir.com/ORX-10G-51S5/latest/Model/public/FileAccessControl.html)
is an intermediate buffer separate from file open/read/write/close commands. These commands
provide raw buffer access, without implementing either workflow automatically. Register
length/address and raw byte semantics follow the manufacturer's
[IRegister API](https://docs.baslerweb.com/pylonapi/cpp/struct_gen_api_1_1_i_register).

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

Capturefab reads zipped or plain GenICam XML from camera memory (`Local:` URLs) or from an
HTTP URL the camera advertises. HTTP retrieval bounds the encoded and decoded file to 16 MiB
and shares one connection deadline across DNS, downloads and up to five redirects. Redirects
may lead to another HTTP host; credentials and other URL schemes are rejected. HTTP files are
fetched again when a camera is reopened, so an updated vendor file cannot stay stale in the
decoded XML cache. It uses the camera's registers to evaluate feature values, ranges, choices and locks. GigE Vision supports packet reordering,
duplicate detection and packet-size negotiation. It can request packet resend when the camera
advertises that feature. Run `capturefab network` to examine the path to each camera.

### Temporary IPv4 assignment

A discovered GigE camera can have an address outside the computer's subnet.
`network force-ip` assigns one explicitly selected camera a temporary IPv4
configuration. Use its MAC from fresh discovery and the current local IPv4
address of the interface carrying that camera. Replace the example addresses
with usable addresses on that interface's subnet:

```sh
capturefab network force-ip --mac 020000000001 \
  --interface 192.0.2.10 --address 192.0.2.20 \
  --netmask 255.255.255.0 --gateway 0.0.0.0 --timeout 3s
```

Close the camera in Capturefab and other camera applications first. The
mandatory native-MAC reservation catches cooperating owners in the selected
runtime; it cannot exclude opaque GenTL aliases, owners opened without locks,
other runtimes or external applications. The command runs locally, ignores an
ambient session default and rejects explicit `--session` routing.

The address, mask, gateway and interface are all explicit. Preflight discovery
must observe that MAC on the selected interface; a responding different MAC at
the requested address is a collision error. The operation sends the assignment
once, then verifies fresh discovery with the requested MAC/address/mask/gateway.
Its default three-second deadline covers the entire operation, up to a maximum
of 30 seconds. A broadcast ACK alone does not prove assignment, and missing ACK
can still accompany matching observed configuration.

If verification or interruption fails after the send attempt,
`error.details.force_ip.assignment_may_have_applied` records the uncertainty.
Inspect fresh discovery before deciding whether to repeat the command. The
operation sends no assignment retry or rollback. The configuration is temporary;
a power cycle restores the camera's startup configuration. No control or stream
connection is acquired during assignment. Use the new address with `info`, then
`capture`, to check the resulting camera connection. Discovery is unauthenticated,
and a silent address conflict cannot be excluded. Physical multi-interface and
device qualification remain pending.

[Basler temporary addresses](https://docs.baslerweb.com/assigning-a-temporary-ip-address-to-a-camera) ·
[Basler ForceIp API and no-open-object requirement](https://docs.baslerweb.com/pylonapi/cpp/struct_pylon_1_1_i_gig_e_transport_layer#function-forceip)

### Immediate network actions

By default, `network action` sends one immediate GigE Vision Action Command on one explicit
local IPv4 interface. It can run alongside the camera's existing capture session.
The camera must support actions and have matching device/group keys and an
intersecting group mask. Its configured action can drive several sinks, including
counters or timers; a frame requires the camera's explicit frame-trigger setup.
[Basler action configuration](https://docs.baslerweb.com/action-commands) and
[transport operation](https://docs.baslerweb.com/pylonapi/cpp/struct_pylon_1_1_i_gig_e_transport_layer#function-issueactioncommand)
describe these separate steps.

The standard `ActionDeviceKey` is write-only. Use a named session that already
holds the camera, with an explicit `-c`, for its separate single-assignment route:

```sh
capturefab session start --name action -c gige:020000000001 --idle 30m
capturefab --session action set -c gige:020000000001 --write-only \
  ActionDeviceKey=305419896 --write-timeout 3s
```

The result reports transport/producer completion, unavailable readback, no
rollback and current control-epoch scope. It does not return a readable key
value or save a checkpoint. The route requires an available effective WO integer
path to a supported directly declared register, with no converter, copy target,
masked read-modify-write or host value. Unsupported layouts fail before writing.
Ordinary read/write settings retain their existing transaction behavior.
The operation stays on the exact live worker admitted by the session; a missing,
replaced or retired worker is not reopened. One expiry covers coordinator waiting,
worker queuing, metadata dependencies and write completion. An error after writing
may leave the key changed; inspect `error.details` before repeating it. A new
connection requires explicit key configuration again.
`debug rpc` refuses this key operation because its generic routing can open a
camera; use the existing-only `set --write-only` route above.

Configure the actual selector/group/trigger features offered by the camera with
ordinary `set`, before streaming. For a camera exposing Basler-style Action1:

```sh
capturefab --session action set -c gige:020000000001 \
  ActionSelector=1 ActionGroupKey=2309737967 ActionGroupMask=2 \
  TriggerSelector=FrameStart TriggerSource=Action1 TriggerMode=On
capturefab --session action capture -c gige:020000000001 \
  --count 1 --background --timeout none -o action.png
```

Check that the host capture job has started before issuing the action. The
returned job status is not a physical sensor-readiness measurement. Action signal
names and selector values vary; inspect the camera instead of assuming Action1
or selector 1 on every model. Keep its primary channel open. Neither command
implicitly enables unconditional actions, PTP or another trigger source.

Replace these documentation addresses with the selected interface and camera:

```sh
capturefab network action --mode acknowledged --interface 192.0.2.10 \
  --device-key 305419896 --group-key 2309737967 --group-mask 2 \
  --expect 192.0.2.20 --timeout 3s
```

Acknowledged mode requires 1–32 distinct expected camera IPs on that interface's
subnet. It checks each reply's source endpoint, receiving interface, request ID,
opcode and exact extent. Only raw status zero is success; full nonzero statuses,
observed responses and missing peers remain available on failure. Collection is
bounded to 1024 datagrams and one deadline of at most 30 seconds. The sender does
not discover recipients, open a camera, send a retry or perform remote cleanup.
An explicit `--session` is refused; ambient session defaults are ignored.

For a send result without collecting replies, select `--mode sent-only` and omit
`--expect`. Its outcome is `sent_without_ack_confirmation`. Acknowledged timeout
never becomes sent-only success. Both modes retain post-send uncertainty on
failure. An ACK establishes neither an image nor synchronization: wait for the
capture job and its saved file independently. Immediate actions do not claim a
common exposure instant; scheduled/PTP actions are a separate contract.

### Explicit scheduled network actions

Add `--action-time-ticks U64_TICKS` to request a scheduled Action Command with an
absolute camera timestamp. Omitting it keeps the immediate packet. The value is
an exact unsigned 64-bit decimal integer, including zero; it is sent unchanged.
For example, after arranging the camera's clock and trigger configuration:

```sh
capturefab network action --mode acknowledged --interface 192.0.2.10 \
  --device-key 305419896 --group-key 2309737967 --group-mask 2 \
  --expect 192.0.2.20 --action-time-ticks 100000000000 --timeout 3s
```

The timestamp belongs to the camera's clock. Capturefab does not read or qualify
that clock, convert host/UTC times, choose a delay, enable PTP, or check that the
target is in the future. Configure and verify the device clocks separately.
Basler documents ticks as nanoseconds while PTP is enabled; use the actual
device's timestamp units and supported range. Its pylon API restricts the target
to `INT64_MAX`; this raw sender preserves the full wire field and does not claim
that every camera accepts every 64-bit value.

Zero and timestamps already in the past may execute immediately. A future action
may remain queued after this command exits or reports an error. The operation's
`--timeout` covers the send and ACK collection, independently of the requested
device timestamp; it does not wait for execution. `action_late`, queue overflow,
no reference time and other nonzero remote statuses remain failures with their
complete raw status. An error never causes a retry or a replacement timestamp.

Both acknowledged and sent-only modes send one scheduled packet. Results and
post-send error details retain `action: "scheduled"`, `action_time_ticks`, and
`execution: "not_verified"`; ACKs do not prove a queue entry, execution, exposure,
saved pixels or synchronization. The expected-peer list only selects ACKs to
collect. The broadcast can also reach other cameras with matching action keys
and masks. Keep capture-job and saved-file evidence separate.

[Basler scheduled-action timestamps and queues](https://docs.baslerweb.com/scheduled-action-commands) ·
[Basler scheduled-action API and range](https://docs.baslerweb.com/pylonapi/cpp/struct_pylon_1_1_i_gig_e_transport_layer#function-issuescheduledactioncommand)

The wire policy uses published manufacturer behavior and unchanged protocol
implementations. Synthetic protocol or producer fixtures do not qualify physical
camera firmware, model timing or hardware synchronization.


Native GigE control keeps the camera's actual heartbeat timeout. A connection retires
permanently when its last verified heartbeat interval expires, including time the host
sleeps or the process is paused. A later global `GevCCP` value cannot reopen that ownership
epoch. Retirement stops further register writes, resends and remote cleanup; locally owned
frames and completed files remain valid. An explicit new connection acquires control again
and can report that another application holds it. The device's access rules still enforce
ownership; a userspace clock check cannot cancel a datagram already sent or make a send
atomic with a process pause. Heartbeat configuration must remain stable across initial
claim and verification. In particular, a device without owner endpoint registers cannot
prove a concurrent timeout-change/takeover cycle that restores the same global values.
The initial claim is sent once; a lost or late acknowledgement causes no automatic claim
retry or unverified release. Camera access control remains authoritative.

For a retired camera held by a session, explicitly reconnect with
`capturefab session start --name NAME -c CAMERA`. Jobs still needing frames fail;
accepted immutable frames finish saving before the old worker closes.

Finite `GevHeartbeatTimeout` value changes and controller-mode/switchover changes are
unsupported on an owned native connection. Exact whole managed-register memory writes use
the camera's declared register byte order; partial or mixed writes are rejected. Verified
heartbeat-disable permits a deliberate pause and still releases control on normal finish.
Re-enabling is supported only before the last reset proved **before** disable would have
expired; reads while disabled do not extend that permission. An initially disabled camera
or a longer pause needs an explicit controller release before a new connection. Release is
sent once after permitted driver-owned restoration; an unconfirmed release is reported
without a retry that could affect a replacement controller. These limits do not change the
separate vendor GenTL producer's ownership behavior.

Native GigE Vision can save synchronized multipart blocks as local stills when
the camera sends extended IDs, one top-down zone per part, and a supported image or
measurement part. The default still uses a single unpadded Mono8, RGB8 or YCbCr411_8 intensity
image when available. Its `IMAGE.multipart.raw` companion
contains every component in leader-table order. The companion JSON identifies native part
types with `data_type_namespace: "gvsp"` and records both `gvsp.address_offset` in the wire
block and `stored_offset` in the archive. The archive retains depth, confidence, and custom
components as original bytes. It carries no inferred calibration or units.

For native GigE Vision and installed GenTL producers, a local still job can select
an exact part with `--part-format FORMAT --part-index N`, where `N` is a zero-based
table ordinal from 0 through 31. For example, two jobs can save the left and right
`Mono8` images from one acquired multipart frame. Supported ordinary image choices
are `Mono8`, `RGB8` and `YCbCr411_8`, in addition to the explicit map/cloud formats
below. RAW and PNG are supported for ordinary images; point clouds require RAW.
Without an index, the format must identify exactly one supported part. Duplicate
matching formats remain ambiguous.

The ordinal and expected type/PFNC format are checked again for every frame.
An ordinal is not a semantic component identity: a producer can change the order
of two equal-format components without changing their ordinals' format checks.
Source, region and purpose IDs remain separate metadata. Invalid geometry,
padding, unknown namespaces and conflicting explicit GenTL URI bindings are
rejected. Use the base `gentl:` camera ID for jobs selecting different components;
a URI already bound to `part=N` keeps that binding. GenDC uses its own component
selectors and cannot be selected with these multipart flags. Multipart stdout
and video remain unsupported. Every saved selection retains the full archive
and reports both its requested selection and actual selected part.

With an explicit ordinary part and `capture --auto`, software metering uses that
selected component. Firmware one-shot
metering is bypassed and available firmware auto controls are held off. Maps and
unclassified Mono16 parts reject `--auto` before camera settings are applied.
Missing or changed selected descriptors fail automatic tuning explicitly. Changing
the metered part in a warm worker releases the prior controller and starts fresh.

For the rc_visard and rc_cube synchronized mode, configure the camera's advertised multipart
stream and extended-ID controls, select `AcquisitionMultiPartMode=SynchronizedComponents`,
and set `ChunkModeActive=False`. Enable a single `Intensity` or `IntensityCombined` stream
when you want a default intensity still. Capturefab uses the advertised controls when
explicitly set. Synthetic protocol fixtures cover this layout. Device and firmware qualification
is pending. [rc_visard stream options](https://docs.baslerweb.com/stereovisard/rc_visard/en/gigevision) ·
[rc_cube stream options](https://docs.baslerweb.com/stereovisard/rc_cube/en/gigevision)

Choose a native multipart measurement as the primary still with
`capture --part-format Coord3D_C16 -c CAMERA -o disparity.raw`. The supported choices are
`Coord3D_C16`, `Confidence8`, and `Confidence16` for local RAW or PNG output. The selected
format must identify one unpadded part of the corresponding native wire type, with a
confirmed delivered height and exact byte length. Selected 16-bit RAW samples use
little-endian byte order. The companion preserves every part in its original order and
byte order. The choice belongs to the capture job, so another job can save the intensity
or a different map from the same camera connection. Coordinate values retain their
original codes. Disparity scaling and calibration come from the camera configuration.
[rc_visard stream formats](https://docs.baslerweb.com/stereovisard/rc_visard/en/gigevision#provided-image-streams) ·
[rc_cube stream formats](https://docs.baslerweb.com/stereovisard/rc_cube/en/gigevision#provided-image-streams) ·
[EMVA PFNC codes](https://www.emva.org/wp-content/uploads/GenICamPixelFormatValues.pdf)

For a measurement-only synchronized rc_visard or rc_cube stream, disable both `Intensity`
and `IntensityCombined`, and enable `Disparity` and `Confidence`. Choose an explicit
`--part-format` for each still. A capture without that choice reports that the block lacks
an intensity image. An inspection labels the measurement it displays and omits intensity
metrics. These rules apply to multipart blocks. A camera can send an ordinary image when
only one component is enabled.
[rc_visard multipart mode](https://docs.baslerweb.com/stereovisard/rc_visard/en/gigevision) ·
[rc_cube multipart mode](https://docs.baslerweb.com/stereovisard/rc_cube/en/gigevision)

Use `capture --part-format Mono16 -c CAMERA -o component.raw` to select a unique
unpadded 2D Mono16 part. The selected RAW or 16-bit grayscale PNG preserves its sample
values, and the companion keeps the original bytes and Data Purpose ID. Mono16 can
represent brightness, range or confidence. Capturefab leaves its component role
unspecified and omits intensity measurements. A default still requires a supported
ordinary intensity part. Physical qualification remains pending for native blaze and
ShapeDrive G4.
[Basler blaze pixel formats](https://docs.baslerweb.com/pixel-format-%28blaze%29) ·
[Wenglor ShapeDrive G4 GigE Vision manual](https://www.wenglor.com/medias/__secure__?attachment=true&mediaPK=9078397829150#page=7)

Use `capture --part-format Coord3D_ABC32f -c CAMERA -o cloud.raw` for a unique
unpadded 3D image part with interleaved 32-bit A, B and C coordinates. RAW retains
the exact little-endian float words, including invalid values. The RAW sidecar
describes the sample layout and links to the complete multipart archive, which
retains the original byte order. Capturefab leaves calibration and units to the
camera configuration. Use `inspect --preview none` for a frame represented by
the point cloud.

Wenglor documents simultaneous Mono16 Intensity and Coord3D_ABC32f Range via its
ShapeDrive G4 GigE Vision server. Select either format from that server as a native
GigE Vision client. Basler documents the same point format on blaze with GenDC
disabled. Hardware qualification of both paths remains pending.
[Wenglor ShapeDrive G4 GigE Vision server manual, pp. 3–7](https://www.wenglor.com/medias/__secure__?attachment=true&mediaPK=9078397829150#page=7) ·
[Basler blaze pixel formats](https://docs.baslerweb.com/pixel-format-%28blaze%29) ·
[Basler blaze GenDC setting](https://docs.baslerweb.com/python-prog-guide)

SICK Ranger3 documents `Coord3D_C10p` and `Coord3D_C12p` Range with optional
Mono8 reflectance. For unchunked native multipart blocks, use `--part-format
Coord3D_C10p` or `--part-format Coord3D_C12p` to select one matching 3D part.
These packed parts require confirmed little-endian order and byte-aligned
rows. Width must be a multiple of four for C10p or two for C12p. The Ranger3
full sensor widths of 1536 and 2560 meet both constraints. RAW and the archive
keep the packed bytes, and a 16-bit gray PNG keeps the 10 or 12 significant
sample bits. Use the camera's range scale and invalid-data settings when
interpreting codes. Native wire and firmware qualification remains pending.
[SICK Ranger3 operating instructions, §§8.2.3–4 and 13](https://cdn.sick.com/media/docs/3/63/063/operating_instructions_ranger3_3d_vision_en_im0080063.pdf) ·
[EMVA PFNC 2.5, §§6.3.1 and 6.7](https://www.emva.org/wp-content/uploads/GenICam_PFNC_2_5.pdf)

Native multipart uses contiguous maximum part ranges declared by the leader. A supported
unpadded row image or map may end early when the trailer reports a positive delivered height
within the maximum and an exact row byte count. Every delivered prefix and every packet ID
through the trailer must be received. Other components must keep their original length.
The archive contains only the validated received part bytes, compacted in leader order.
`gvsp.address_offset` retains the original maximum-block address; `stored_offset` locates the
part in the compact archive. A shortened part also records `gvsp.declared_length`, maximum
`height` and actual `delivered_height`. Queue limits charge the maximum backing allocation,
including untransmitted tail capacity, but those tail bytes never enter an image or archive.
Received padding/tail packets, shortened opaque or padded parts, multipart chunks, multiple
or bottom-up zones, stdout and video remain unsupported.

SICK Ranger3 documents `AcquisitionStopMode=Immediate` for variable-length objects, transmitting
only the profiles acquired before the frame trigger ends. Its `Complete` mode retains the full
reported height. Configure unchunked streams for this native subset. The receive profile above
has deterministic protocol fixtures; Ranger3's actual shortened packet layout and firmware
qualification remain pending until a device trace is available.
[SICK Ranger3 acquisition stop modes, §8.4.4](https://www.sick.com/media/docs/3/63/063/operating_instructions_ranger3_3d_vision_en_im0080063.pdf#page=58)

A shortened block cannot prove the terminal packet ID when its trailer is lost and maximum
rows were never transmitted. Capturefab keeps it incomplete instead of guessing that ID.
Native USB multipart, native GenDC transport, legacy chunk-only payloads, and GenICam XML that
requires HTTPS or HTTP authentication are unsupported.
Installed GenTL producers can retain the multipart layouts described below.

Native GigE Vision accepts ordinary `Coord3D_ABC32f` image payloads and image-with-chunk
payloads with a verified image boundary. Save them as RAW. The sidecar records the PFNC code,
geometry and three interleaved little-endian binary32 words per point in A, B, C order. Capture
preserves sentinel values and signed zeros. Use `inspect --preview none` for frame metadata.
Coordinate units and calibration come from the sensor configuration.

Micro-Epsilon documents GigE Vision profile output for scanCONTROL 85xx. An independent report
for the 850050 with firmware 3.0.0 observed `0x4001` with `Coord3D_ABC32f` in
`RegionSelector=Scan3dExtraction0` and `ComponentSelector=Range`. Select these features
when your sensor advertises them. Physical qualification with Capturefab is pending.
[Micro-Epsilon profile guide](https://www.micro-epsilon.com/fileadmin/download/manuals/maq--scanCONTROL-8xxx--en.pdf#page=18) ·
[850050 wire report](https://github.com/VitalyVorobyev/viva-genicam/pull/93#issuecomment-5162882332)

Standard PFNC `Coord3D_ABC16` (`0x023000B9`) is supported for ordinary GigE Vision
and GenTL image buffers, explicit multipart selections and supported GenDC Range parts.
Use `capture --part-format Coord3D_ABC16 -c CAMERA -o cloud.raw` for native multipart;
for GenTL, copy the format-bound component ID from the companion JSON. Save as RAW and
use `inspect --preview none`. The RAW sidecar describes three interleaved little-endian
unsigned 16-bit words in A, B, C order. Every sample code is preserved. The multipart
archive retains the original byte order; it is normalized using the transport's reported
sample order for the selected RAW image. Units, coordinate axes, scale, offsets and invalid
codes require the device's settings. PNG, JPEG, video and intensity measurements do not
interpret these point arrays.

LUCID's 3D format guide documents this six-byte layout for its time-of-flight cameras.
Its distinct `Coord3D_ABCY16` format adds an intensity word and uses LUCID's custom
code `0x82400403`, not the standard ABC16 code. Native GigE Vision and GenTL ordinary
images accept it only from a camera that reports the vendor "Lucid Vision Labs" and whose
static XML `PixelFormat` entry `Coord3D_ABCY16` has exactly that literal value; the bare
custom code, the signed `0x82400400` sibling and other vendors stay unqualified. Each pixel
is four little-endian unsigned 16-bit words in A, B, C, Y order. GigE requires a
little-endian stream (a big-endian stream leaves the frame opaque) and GenTL requires
producer little-endian metadata. Save it as RAW: the sidecar's `pixel_encoding` names the
LUCID layout, and `sample_layout` lists the four components with Y as intensity. Codes are
kept exactly, without Scan3d scale or offset and without filtering invalid points. PNG,
JPEG, video, intensity previews and auto exposure do not interpret it. These are format
compatibility paths; physical camera qualification remains pending.
[LUCID 3D pixel formats](https://support.thinklucid.com/knowledgebase/pixel-formats-3d/) ·
[EMVA PFNC naming and unsigned sample rules](https://www.emva.org/wp-content/uploads/GenICam_PFNC_2_4.pdf) ·
[EMVA PFNC numeric values](https://www.emva.org/wp-content/uploads/GenICamPixelFormatValues.pdf)

Native GigE Vision also accepts ordinary PFNC `Coord3D_AC16` profiles. The RAW sidecar
describes two interleaved little-endian unsigned 16-bit words per point in A, C order.
Use `inspect --preview none` for frame metadata. The same format is supported through
GenTL ordinary image buffers and format-bound Range selections in multipart and GenDC stills.

Teledyne documents this format for the Z-Trak2, LP2C and Express families with
`DeviceScanType=Linescan3D` and `Scan3dOutputMode=CalibratedAC` (XZ). The recorded words retain the sensor’s original
codes, including invalid values. Apply the device’s X and Z scaling separately when
converting to physical measurements. Physical qualification with Capturefab is pending.
[Teledyne Z-Trak family manual](https://flir.netx.net/file/asset/68263/original/attachment/#page=104) ·
[EMVA PFNC layout](https://www.emva.org/wp-content/uploads/GenICam_PFNC_2_4.pdf) ·
[PFNC numeric values](https://www.emva.org/wp-content/uploads/GenICamPixelFormatValues.pdf)

Native GigE Vision accepts unchunked IMAGE payloads (`0x0001`) sent as a single
All-in GVSP datagram, with standard or extended block IDs. The datagram must contain
its complete image leader, image trailer and exact delivered image bytes within the
negotiated packet size and configured payload limit. The normal row padding, packed
sample and stream byte-order rules apply. Duplicate blocks are delivered once; no
separate-packet resend is inferred from the All-in header’s packet ID. All-in chunks,
multiple image fields, multi-zone, multipart and GenDC payloads remain unsupported.
Capturefab preserves the camera’s configured transmission mode.

Balluff documents `GevSCCFGAllInTransmission` for the BVS CA-GX0-0064ZC /
mvBlueCOUGAR-X106C and its monochrome X106G counterpart. The X106C’s documented
minimum 376 × 4 BayerRG8 ROI already needs a packet larger than 1500 bytes; the
camera’s packet size and network MTU must fit the complete block. The receive
fixtures are synthetic and independently follow the upstream Wireshark layout;
physical device qualification remains pending.
[Balluff X106C feature list](https://assets.balluff.com/documents/DRF_957327_AA_000/Sensors_CMOS_model_106_DeviceFeaturesAndPropertyLists_ColorVersion.html) ·
[EMVA SFNC 2.7 §27.4.58](https://www.emva.org/wp-content/uploads/GenICam_SFNC_v2_7.pdf) ·
[Wireshark All-in layout](https://github.com/wireshark/wireshark/blob/cd59fea3baff8444a2645ba928e6967673c5b54a/epan/dissectors/packet-gvsp.c)

Native GigE Vision also accepts legacy extended-chunk image payloads (`0x0005`). Their
trailer reports total payload bytes, delivered height and layout ID in that order. Capturefab
requires exact image geometry and a validated first image chunk footer before selecting pixels.
The original image, padding, chunks and footers are preserved in the same chunk companions.
XML interpretation uses only declared chunk ports; vendor ancillary fields without that mapping
remain opaque. Pure chunk-only payloads and unproved legacy variants remain unsupported.

An independent Manta G-125B capture (GigE Vision 1.2, firmware 00.01.44.28514) provides
literal headers for this layout. Allied Vision also documents legacy chunk support for its
Manta, Mako, Goldeye and Prosilica GT families. This does not establish Capturefab hardware
qualification for those models.
[Captured Manta headers](https://github.com/VitalyVorobyev/viva-genicam/blob/cdfedcc9d52694b1f471029bf585b0e50d04073b/crates/viva-gige/src/gvsp.rs) ·
[Allied Vision legacy chunk support](https://docs.alliedvision.com/vimba_x/Linux.html#changes-in-vimba-x-2025-2)

Image-with-chunk payloads (`0x4001`) from GigE Vision and USB3 Vision retain their complete
original bytes, including image padding and opaque vendor chunks. Local still captures write
`IMAGE.chunks.raw` and `IMAGE.chunks.raw.json`; the JSON describes the original image layout,
chunk IDs, offsets and lengths. GigE Vision accepts complete-payload trailer lengths when
the exact image geometry and a first image chunk tag agree. Local video records them in `VIDEO.chunks.raw` with a
`VIDEO.chunks.jsonl` frame index. Saved-file metadata and manifests reference these artifacts.
Pixel conversion uses a separate normalized image buffer. When the camera's XML exposes chunk
features, metadata includes interpreted scalar `values`, explicit XML `units`, per-feature
`errors` and `truncated`. Interpretation is bounded to 256 exposed features and 64 KiB of
serialized metadata. Missing units stay absent; unsupported or unresolved features report
errors while the original bytes remain intact. The interpreter resolves static or dynamic XML
`ChunkID` declarations and preserves opaque vendor IDs; vendor meanings are never inferred.

Chunk evaluation uses the retained payload and a frozen feature context. Preparing that context
allows at most 32 device reads and 64 KiB of live dependencies, reuses cached reads and stops
after a transport failure. Snapshot evaluation performs no further device I/O. Captures
to stdout and network video outputs require disabling camera chunks or choosing local storage.
USB3 Vision and GigE Vision normalize row padding for supported image layouts.
Padded planar USB3 Vision images require an explicit component layout and remain unsupported.
USB3 Vision accepts shorter frames when a short packet or zero-length packet establishes
the payload boundary. The trailer must confirm the received byte count before any image is
delivered. Reduced heights must also be confirmed within the leader's maximum, including
padded and chunked images. Legacy common-only trailers are accepted only for a complete
image matching the leader's geometry. Reduced payloads coalesced with a trailer without an
independently known boundary remain unsupported. See [Basler's USB3 Vision transport description](https://machinevisionstore.com/content/downloads/basler/Setting-Up-Stable-USB3-Camera-System.pdf).
Video recordings require a fixed frame size; a change in actual height fails explicitly.
The payload limit is 128 MiB and the descriptor limit is 4096 chunks per frame.
See [Basler's chunk documentation](https://docs.baslerweb.com/data-chunks) and
[FLIR's Blackfly S chunk features](https://softwareservices.flir.com/BFS-PGE-123S6P/latest/Model/public/ChunkDataControl.html)
for examples of the metadata these cameras provide.

PNG keeps monochrome samples wider than 8 bits as 16-bit gray. Use `raw` to keep the acquired
bytes at full bit depth or to save an image that uses a format Capturefab cannot convert.
See [Pixel formats](#pixel-formats).

## Installed GenTL producers

Install the vendor's CTI producer, runtime and device or frame-grabber driver for your operating
system and Capturefab's CPU architecture. Vendor license terms apply to those components;
Capturefab bundles no vendor CTI runtimes. Producer-based access covers interfaces exposed by
the installed SDK, including frame grabbers and legacy cameras with a GenTL adapter. It requires
a producer and device mode that deliver the payloads described below. Physical devices remain
unverified for this path.

Set `CAPTUREFAB_GENTL_PRODUCERS` to explicit producer file paths. Multiple paths use the host's
path-list separator (`:` on Unix, `;` on Windows). Capturefab also searches CTI files in the
directories listed by the standard `GENICAM_GENTL64_PATH` variable. For example, in a POSIX shell:

```sh
export CAPTUREFAB_GENTL_PRODUCERS='/opt/vendor/lib/Producer.cti'
capturefab list --kind gentl
capturefab capture -c 'gentl:ID_FROM_LIST' -o frame.raw
```

Copy the complete opaque `gentl:…` ID printed by `list`; it retains the absolute producer path
and the producer's interface and device identifiers. Moving the CTI installation requires a
fresh ID. `info`, `features`, supported GenICam settings, capture and recording use this selector.
Source builds can disable this path by omitting the `gentl` feature.

All CTI loading and native SDK calls run in `__gentl-helper`, separate from the CLI, coordinator
and camera worker. Bounded control messages and a pixel pipe connect the helper to its owner.
Producer stdout is redirected away from the protocol. Deadlines retire and terminate a helper
that hangs or crashes. Discovery runs up to four producers concurrently within one timeout;
failures appear as warnings alongside cameras found through healthy producers. Ordinary frame
waits, including external-trigger waits, keep the helper open.

A GenTL producer can declare the data stream's required buffer size, as frame grabbers can
do when they preprocess pixels or adjust line-scan buffer height. Capturefab queries that
size after locking the camera's transport settings. When the producer delegates sizing to
the camera or omits the stream flag, Capturefab reads the remote `PayloadSize` feature.
[EMVA GenTL 1.6 §5.2.1](https://www.emva.org/wp-content/uploads/GenICam_GenTL_1_6.pdf#page=44)

The current path accepts complete `IMAGE` payloads up to 128 MiB. Image offsets and row padding
are removed from the pixel buffer; supported big-endian words are normalized to little-endian.
Known unpadded planar layouts are retained. Padded planar layouts, incomplete or oversized
images, composite buffers and chunk-only payloads are rejected.
The accepted pixel namespaces are GigE Vision and PFNC 32-bit. PFNC 16-bit namespaces and
vendor-specific codes require an explicit mapping and currently return `unsupported`.
Wide samples with unknown byte order, and unknown multi-byte formats requiring byte-order
normalization, also return `unsupported`. A `raw` capture contains the normalized source samples
at their acquired bit depth.

Image-with-chunks buffers retain their complete original payload in the same local companion
artifacts described above. Producer-provided descriptors preserve 64-bit chunk IDs, byte offsets
and lengths, including chunks before or overlapping the image. The image layout records its
original offset, padding and available pixel byte order. A provided nonzero 64-bit layout ID is
retained; unavailable layout IDs and unknown byte orders remain absent. Chunk register byte
order comes from the camera's XML rather than the pixel format.

When a producer explicitly marks chunk data but cannot supply descriptors, Capturefab preserves
the opaque payload and records `descriptor_error` alongside unavailable interpreted features.
It infers no vendor IDs or footer layout. Malformed successful descriptor results, contradictory
presence flags and out-of-range indexes fail acquisition. Legacy chunk-data payload type 4 is
accepted only when the producer explicitly confirms that an image is present.

GenTL `MULTI_PART` buffers can be saved as local stills when they contain a supported ordinary
2D image, selected 16-bit measurement map or selected interleaved point cloud. Capturefab
retains **every** component, including uninterpreted confidence and vendor-specific data, in
`IMAGE.multipart.raw`; `IMAGE.multipart.raw.json` records the complete
part table, original source offsets, consecutive stored offsets, dimensions, pixel formats and
namespaces, available source/region/purpose IDs, and the original reported byte order. The
archive concatenates component ranges in producer index order and reads no allocation gaps.
The selected component is copied and normalized separately.

If exactly one nonempty ordinary 2D image exists it is selected automatically. Per-job
`--part-format` and optional `--part-index` can select alternatives through the base
camera ID without opening another device owner. When selection
is ambiguous or the selected part is unsupported, the
`unsupported` error includes `error.details.parts` and ready-to-copy
`error.details.image_selectors[].camera` IDs for supported alternatives. When alternatives
are available, Capturefab uses the remaining frame deadline to stop and unlock the camera
before closing or retiring its helper. Once the driver releases the device, retry with a
complete alternative ID as `-c`; other cameras in the warm session stay connected.
Selector choices have a 128 KiB
output budget; `image_selectors_truncated` counts omitted choices. The entire bounded part table
is retained in the error even when some selectors are omitted. With several cameras in one
capture, find each failed camera's selection details under its entry in `error.details.cameras`.

This path requires 1–32 disjoint components totaling at most 128 MiB, with their summed sizes
exactly accounting for `BUFFER_INFO_SIZE_FILLED`. Producers that report additional written bytes
outside their part ranges are rejected; Capturefab does not read gaps to fill the difference.
The selected part must be unpadded data in its supported pixel namespace and have consistent
dimensions and delivered height. Part X padding is reported in **pixels** and
preserved as such. Multipart buffers with common chunks, composite buffers and buffers without
a supported image or map remain unsupported. Stdout capture and video reject multipart frames rather
than discard their other components. Physical multipart cameras remain unverified.

Complete PFNC32 `Coord3D_C10p`, `Coord3D_C12p` and `Coord3D_C16` coordinate maps, `Confidence16` confidence maps,
interleaved `Coord3D_AC16` profiles and `Coord3D_ABC16`/`Coord3D_ABC32f` point clouds can be selected explicitly. A multipart still's companion JSON
lists copyable IDs for supported measurement components in `image_selectors[].camera`. These IDs bind the part index to its pixel
format. For exclusive producers, start with `capturefab --no-session capture -c 'gentl:ID' -o intensity.raw`,
then use `--no-session` with the selected component ID. A warm session can hold the original camera
open until it releases the device.

The packed Range formats require a 3D image part in the PFNC32 namespace,
confirmed little-endian packing, zero X padding and an exact byte count.
Widths are multiples of four for C10p or two for C12p. SICK documents these
Range formats for Ranger3 and provides an installed GenTL acquisition path
for Ranger3 and Ruler cameras. Hardware qualification remains pending.
[SICK Ranger3 instructions](https://cdn.sick.com/media/docs/3/63/063/operating_instructions_ranger3_3d_vision_en_im0080063.pdf) ·
[SICK GenTL acquisition examples](https://github.com/SICKAG/Ranger3PythonExamples)

Save maps as RAW or 16-bit grayscale PNG. The output preserves the unsigned samples and their
PFNC format. Packed maps keep their packed bytes in RAW and declare their significant bits in PNG. RAW uses little-endian samples, and the companion archive retains the producer's
original byte order. The files contain the camera's sample codes. Save the applicable device
calibration separately when physical units are needed. Inspection omits exposure and focus
measurements for these formats, and explicit measurement selectors return `unsupported` for
`auto`.

Save `Coord3D_ABC16` and `Coord3D_ABC32f` point clouds as RAW. The raw sidecar's `sample_layout` describes three
interleaved little-endian unsigned 16-bit or 32-bit float words per pixel in PFNC A, B, C order. Capture preserves
the exact word bits, including device sentinel values and signed zeros. The archive retains
the original bytes and reported byte order. Use `inspect --preview none` to inspect the frame
metadata. Coordinate axes, physical units and invalid values come from the device's settings.

Basler documents the `Coord3D_ABC32f` Range component with GenDC disabled in its
[blaze acquisition guide](https://docs.baslerweb.com/python-prog-guide).

Saved-file events, manifests, raw sidecars and camera time-series observations include the
archive reference. Capture results and failed-capture details retain small references in
`multipart`, keyed by the listed primary image path, up to 64 KiB; job-list rows use a separate
4 KiB bound. `multipart_truncated` counts omitted references. Full part descriptors remain in
each local companion JSON regardless of summary truncation. Companion publication uses the
same no-overwrite storage path as the image and rolls back owned artifacts on failure.

### GenDC stills through GenTL

A GenTL 1.6 producer can deliver a complete GenDC 1.1 or 1.2 container in a contiguous
buffer. Use the producer's installed CTI and enable the camera's documented GenDC mode.
Start with `capturefab --no-session capture -c 'gentl:ID' -o intensity.raw`. For example,
a Zebra AltiZ 4200 advertises GenDC 1.1, and STEMMER CVB's GevTL producer documents GenDC
support through its GevSD SOCKET interface. That camera and producer combination still
needs physical qualification.
[Zebra AltiZ 4200 specifications](https://www.rauscher.de/fileadmin/user_upload/Matrox/AltiZ4200/altiz-4200-3d-sensors-spec-sheet-en-us.pdf)
[STEMMER CVB GevTL interfaces](https://help.commonvisionblox.com/NextGen/15.1/md_documentation__sections__image_acquisition__device_discover.html)

Capturefab selects a unique supported unpadded Mono8 or Mono16 Intensity image automatically.
A padded image is eligible for automatic selection when it is the only supported Intensity.
Copy a measurement component's or padded alternate's `image_selectors[].camera` ID from the
companion JSON. When an explicit choice is required, copy a supported component ID from
`error.details.image_selectors`. Selectors bind the GenDC component and part indexes, type, format, group, source
and region. The selected part must be a single-part component with exact byte geometry.
For supported 2D images, the selected output omits padding after each row and at the end
of the part. A 1D measurement array is saved as one row with its trailing padding omitted.
The GenDC companion preserves these padding bytes. Supported measurements are Range
`Coord3D_C16`, `Coord3D_AC16`, `Coord3D_ABC16` and `Coord3D_ABC32f`, and `Confidence16`. Use RAW for point arrays
and RAW or lossless 16-bit PNG for scalar maps. Use `inspect --preview none` for point arrays.

Each saved image has an `IMAGE.gendc` companion containing the original final descriptor
and every data part in their original byte positions. The companion JSON uses
`storage: "linear_gendc_container"` and `data_type_namespace: "gendc"`. It retains
component timestamps and the PTP flag separately from the GenTL acquisition clocks.
PTP component timestamps use the 1970 TAI epoch. Embedded metadata, XML and unknown or
compressed parts are archived as bytes. Saved-file and capture results link the companion
through the existing bounded `multipart` references.

The descriptor and non-overlapping parts must account for every byte reported written, up
to 128 MiB. Capturefab rejects preliminary descriptors, composite buffers, and containers
with gaps outside validated parts. GenDC stdout capture and video return `unsupported`.
[EMVA GenTL 1.6](https://www.emva.org/wp-content/uploads/GenICam_GenTL_1_6.pdf)
[EMVA GenDC 1.1](https://www.emva.org/wp-content/uploads/GenICam_GenDC_v1_1.pdf)
[EMVA GenDC 1.2](https://www.emva.org/wp-content/uploads/GenICam_GenDC_v1_2.pdf)

Device XML can be plain or zipped, retrieved from camera memory (`Local:`), an absolute local
`file:` URL or an advertised plain HTTP URL. File URLs support percent-encoded spaces and
Unicode paths, including Windows drive paths. XML is bounded to 16 MiB, and HTTP retrieval shares
the remaining helper-call deadline after the native URL lookup. HTTPS, authenticated HTTP and
remote file URL authorities are unsupported.

See the [EMVA GenTL specification](https://www.emva.org/wp-content/uploads/GenICam_GenTL_1_6.pdf),
[IDS peak](https://en.ids-imaging.com/ids-peak.html) and
[Basler's GenTL producer setup](https://docs.baslerweb.com/frame-grabbers/using-the-gen-tl-interface)
for the interface and examples of vendor installation requirements.

## Source clocks

`device_ts_ns` retains a source timestamp explicitly expressed in nanoseconds. GenTL also
retains the original `device_timestamp.ticks` when available, with `frequency_hz` only when the
producer supplies a nonzero clock frequency. Missing frequencies stay absent, and clock ticks
are never relabeled as nanoseconds. Both fields can be present in still metadata, raw sidecars,
manifests and indexed video or industrial-data references.

These source clocks are counters in a device's clock domain, not UTC dates. Compare them only
within a known clock domain or a configured synchronization arrangement. Host receive clocks
remain separate; a shared host timeline does not establish hardware synchronization.

## Pixel formats

Captures, inspections and recordings convert these PFNC layouts:

| Family | Converted layouts |
| --- | --- |
| Monochrome | Mono1p/2p/4p, Mono8/8s/10/12/14/16, Mono10p/12p/14p, Mono10/12Packed |
| Bayer | GR/RG/GB/BG at 8/10/12/14/16 bits, 10p/12p/14p and 10/12Packed |
| Interleaved color | RGB/BGR at 8/10/12/14/16 bits, RGB/BGR10p/12p, RGB10p32, RGB/BGR565p |
| BiColor | Unpacked BiColorRGBG8/10/12 and BiColorBGRG8/10/12; packed BiColorRGBG10p/12p and BiColorBGRG10p/12p, with the source-width rules below |
| Planar color | RGB8/10/12/16_Planar |
| Alpha/alignment color | RGBa/BGRa at 8/10/12/14/16 bits, RGBa/BGRa10p/12p, RGB8a32/BGR8a32; alpha/alignment is omitted from opaque images |
| YUV/YCbCr | Generic 8-bit 4:4:4, 4:2:2 and 4:1:1; YCbCr601/709 at those subsamplings, in the PFNC default and explicit Cb-first orders; semiplanar YCbCr420/422 `_YY_CbCr` and `_YY_CrCb` |
| Decoded video | Capturefab's own codes for what PFNC has none for: planar 4:2:0, 4:2:2 and 4:4:4 (`YCbCr420_8_Planar` and so on), and semiplanar 4:2:0 (NV12) in limited-range BT.601/709 and full-range BT.709 |
| Scalar measurements | Coord3D_C8/C16, Confidence8/16 as grayscale; floating-point depth (Coord3D_C32f) as raw only |

BiColor stores RG/BG or BG/RG pairs at alternating horizontal positions. Each
row restarts the same phase, including after an admitted odd-width row. The 8-bit formats
carry two bytes per position; unpacked 10/12-bit formats carry two separately
LSB-aligned 16-bit components. Transport byte order is normalized per component,
with the same authoritative GigE/USB/GenTL rules as other wide formats. A wide
unpacked GenTL buffer with unknown byte order fails explicitly.

The four standard packed `10p`/`12p` formats use contiguous LSB-first component
bits. Packed 10-bit sources require a positive even width: two positions use
five bytes. Packed 12-bit sources allow any positive width: each position uses
three bytes. Packed bytes are not swapped as 16-bit components. Software views
may start at an odd x-coordinate or use an odd width inside an admitted source;
interpolation stays within that view. Odd-width `10p` sources fail explicitly,
including single-row images, because their row and final partial-group boundary
rules have not been qualified. Buffer capacity or padding does not select a
guessed layout.

Display conversion preserves each measured green and red/blue component and
interpolates only the absent red/blue horizontally within the selected region.
It averages two available neighbors, uses one at an edge, and uses measured
green when a one-column region has no opposite-color sample. This is a software
rendering policy, without a physical registration or calibrated-color claim.
Native clipping counts only the two received components. PNG/JPEG use the
existing 8-bit color conversion; FFV1 can retain 10/12-bit reconstructed RGB
components. RAW follows the existing normalized frame representation, while
original chunk archives retain producer bytes, byte order and padding.

Basler documents the 8-bit pair on racer2S models such as `r2L2048-29gc` and
`r2L2048-62g5c`, and 10/12-bit choices on particular CXP models such as
`r2L16384-60cc`. CXP uses an installed GenTL producer. These are documented
format examples, with physical model/driver qualification still pending.
JAI's SW-4005BL-5GE manual v1.4 documents the four packed choices, with `ReverseX`
selecting BGRG and width increments of eight. Its documented widths satisfy the
10-bit packing rule. The manual lists eBUS 6.5.4 or later for packed BiColor;
this is manufacturer compatibility information, without an observed producer
buffer or physical model qualification. Vendor aliases remain unsupported.
[Basler BiColor layout and model table](https://docs.baslerweb.com/pixel-format#bicolor-bgrg-and-rgbg-formats) ·
[JAI SW-4005BL-5GE](https://www.jai.com/products/sw-4005bl-5ge) ·
[JAI manual v1.4, manufacturer-authored distributor copy, pp43–47](https://www.bhphotovideo.com/lit_files/1268774.pdf#page=43) ·
[EMVA PFNC BiColor layout](https://www.emva.org/wp-content/uploads/GenICam_PFNC_2_5.pdf) ·
[PFNC numeric values](https://www.emva.org/wp-content/uploads/GenICamPixelFormatValues.pdf)

Qualified IDS USB3 Vision and GenTL ordinary images also support `Mono10g40IDS`
and `Mono12g24IDS`, plus the color mosaics `BayerRG10g40IDS`/`BayerRG12g24IDS`,
`BayerGR10g40IDS`/`BayerGR12g24IDS`, `BayerGB10g40IDS`/`BayerGB12g24IDS` and
`BayerBG10g40IDS`/`BayerBG12g24IDS`.
These manufacturer encodings differ from PFNC packed formats. Capturefab requires
an IDS device identity and an exact static
XML format name/code match. Width must be divisible by four for 10-bit groups or two
for 12-bit groups. Other vendor layouts and partial groups remain unsupported.

RAW keeps the original grouped samples; its sidecar's `pixel_encoding` retains the IDS
namespace, name and code, with the reported `gentl_namespace` when applicable.
The custom code is not labeled `pfnc_code`. The declaration stays with chunk image
layouts, manifests, industrial camera samples and video indexes. A grouped Bayer RAW
sidecar identifies the declared RGGB, GRBG, GBRG or BGGR color filter. The
delivered format determines the filter; ROI, Mirror or Reverse settings do not
substitute for a format declaration.
Mono PNG keeps every sample as 16-bit gray with sBIT. Bayer PNG, JPEG and inspection
previews use the ordinary 8-bit color conversion, with `source_bits` recording the
original precision. Auto exposure meters native values. FFV1 retains exact unpacked
10/12-bit samples, with Bayer mosaics stored as 16-bit gray and their original source
layout retained in metadata; players display the mosaic in gray. Other video encoders
can reduce precision as documented below.
When chunks are present, their archive retains original image padding and payload bytes.

The IDS manual defines these bytes for USB transfer and transport-layer delivery.
Native GigE wire representation differs and is not inferred from these codes.
GenTL namespaces 1, 4 and custom marker 1000 are recognized only under the same joint
IDS qualification; other custom namespace IDs remain unsupported. No actual IDS producer
namespace or physical camera was observed in Capturefab qualification. Producers must
deliver the XML-declared IDS byte layout. The manufacturer calls the names/values
preliminary, so a changed XML mapping fails explicitly. Published model sheets including
U3-38J4XLS-M-NO Rev.1.2 (now discontinued) and U3-34F0XCP-M-GL Rev.1.2 demonstrate format
availability. Color sheets for U3-34F0XCP-C-HQ Rev.1.2 and U3-38C1XLS-C Rev.1.2 list
the grouped RG Bayer pair. U3-38J6XLE-C-NO Rev.1.1 (firmware 2.11 or newer) and
the U3-3680XCP Rev.1.2 color family advertise the GR pair. GB/BG support qualifies
the documented representation conditionally; no specific model availability has
been established. These are format-availability examples, not whole-model qualification.
[IDS byte-layout scope](https://en.ids-imaging.com/manuals/ids-peak/ids-peak-user-manual/26.06.2/en/basics-image-color-formats.html) ·
[IDS monochrome formats](https://en.ids-imaging.com/manuals/ids-peak/ids-peak-user-manual/26.06.2/en/basics-monochrome-pixel-formats.html) ·
[IDS Bayer layout reference](https://www.1stvision.com/cameras/IDS/IDS-manuals/en/basics-raw-bayer-pixel-formats.html) ·
[IDS mono model sheet](https://en.ids-imaging.com/IDS/pdfsheet_pdf.php?sku=1009044) ·
[IDS color XCP sheet](https://en.ids-imaging.com/IDS/pdfsheet_pdf.php?sku=1010606) ·
[IDS color XLS sheet](https://en.ids-imaging.com/IDS/pdfsheet_pdf.php?sku=1010294) ·
[IDS GR model sheet](https://en.ids-imaging.com/IDS/pdfsheet_pdf.php?sku=1007278) ·
[IDS GR XCP family](https://en.ids-imaging.com/store/u3-3680xcp-rev-1-2.html)

Names and component orders of the PFNC layouts follow the EMVA specifications.
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
Thin Bayer regions retain the sensor's color phase at odd origins.
Raw capture preserves the acquired frame buffer. The sidecar gives the PFNC format and code, or an explicit vendor encoding declaration.

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
