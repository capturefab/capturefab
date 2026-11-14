<!-- Generated from docs/industrial-data.md by scripts/site-guides.py. Edit the source, then run the script. -->

# Record industrial time series

`capturefab data` records several data sources in one timestamped JSON Lines file,
then exports samples as CSV or JSON Lines. Run `capturefab data protocols --json`
to inspect the available adapters and get an example configuration.

| Protocol | Current acquisition path |
| --- | --- |
| Camera stills | Built-in isolated camera worker; JPEG, PNG or raw files and timestamped references |
| Video | Built-in FFmpeg encoding with a per-frame index linking presentation frames to host receive times |
| Universal Robots RTDE | Native output-only RTDE v2 synchronization with an explicit typed output recipe and exact raw packets |
| ABB Robot Web Services | Native read-only RWS1/RWS2 controller, mode, system, RAPID execution and named symbol state over authenticated HTTP/verified HTTPS |
| Yaskawa HSES | Native read-only controller status, current base position and B/I/D/raw-R/whole-S32 variables under an explicit UDP reply-horizon policy |
| Epson Remote Ethernet | Native externally configured monitor-only Login/GET sessions with explicit RC+7.5, RC+8 or RC800 profile and exact controller observations |
| MTConnect | Native read-only agent probe/current/sample XML, model-bound observations and explicit epoch/cursor checks |
| Keyence IV2, IV3 and IV4 | Built-in read-only polling of inspection results over nonprocedural TCP |
| Cognex In-Sight | Built-in read-only Native Mode polling of spreadsheet cells over Telnet |
| Cognex DataMan | Built-in complete ID Reader result attribute reads over EtherNet/IP, preserving native result codes and binary bytes |
| Gocator profilers and snapshot sensors | Passive legacy Gocator GDP TCP groups with lossless raw artifacts and typed profile/surface indexes |
| Atlas Copco PF6000 | Built-in finite Open Protocol tightening-result subscription with checkpoint-before-ACK |
| Simulator | Built-in deterministic counter; no hardware needed |
| Modbus TCP | Built-in read-only scalars and arrays, per-signal gateway units and optional verified TLS |
| Modbus RTU and ASCII | Built-in read-only scalars and arrays across configured stations on Unix terminals or Windows COM ports |
| S7 | Built-in read-only classic S7 absolute scalars/arrays, DB text and indexed timer/counter words |
| Beckhoff ADS | Built-in, read-only values by index group/offset or symbol over AMS/TCP |
| Mitsubishi MC/SLMP | Built-in read-only 3E binary or ASCII step/retentive devices, numeric words/bits and whole iQ-R long-timer records |
| Mitsubishi MC 1E | Built-in, read-only binary or ASCII TCP access for compatible MELSEC-F Ethernet modules |
| Mitsubishi FX Computer Link | Built-in read-only MELSEC-F serial BR/WR/QR in formats 1 and 4 |
| Keyence KV Host Link | Built-in read-only TCP register, contact and timer/counter values |
| LS ELECTRIC XGT/XGB | Built-in read-only FEnet dedicated TCP direct variables and typed byte-backed arrays |
| LS ELECTRIC XGB Cnet | Built-in read-only dedicated serial rSS scalars and explicit-address arrays |
| FATEK FBs | Built-in standard Port0 command 44/46 discrete and native register reads |
| Yokogawa FA-M3 | Built-in read-only binary or ASCII higher-level PC Link TCP bits and words |
| Panasonic MEWTOCOL-COM | Built-in read-only FP register and contact reads over bare TCP or serial |
| Omron FINS/TCP and FINS/UDP | Built-in, read-only CS/CJ/CP-style memory areas with TCP node negotiation or explicit UDP routes |
| Omron serial Host Link | Built-in read-only 1:N C-mode RD, direct FINS FA and explicit legacy CPM/SRM1 native-area reads on Unix terminals and Windows COM ports |
| Omron CompoWay/F | Built-in read-only serial variable access for E5CC/E5EC temperature and G3ZA/G3PW power controllers |
| CAN / CAN FD | Built-in passive Linux SocketCAN frames and optional mapped values |
| IEC 62056-21 | Built-in Mode C optical meter readout with explicit OBIS mappings |
| SML serial | Built-in passive binary SML optical meter readout with exact scaled values |
| SDI-12 serial | Built-in addressed environmental measurements through a compatible transparent USB bridge |
| HART serial | Read-only universal process measurements through a configured Microflex 101-0035 managed modem |
| BACnet/IP | Built-in direct unicast ReadProperty for analog, binary and multi-state values |
| ifm IO-Link | Built-in read-only process input through an ifm IoT Core master, with explicit mappings |
| JSON Integration for IO-Link | Built-in read-only process-data GET; raw response, explicit byte mappings and JSON pointers |
| TCP JSONL | Built-in newline-delimited sensor readings over TCP |
| Serial JSONL | Built-in newline-delimited sensor readings on Unix terminals or Windows COM ports |
| SCPI instruments | Query-only raw TCP/VXI-11/HiSLIP/serial/USBTMC sessions, exact scalar/vector replies and local definite-length binary artifacts |
| Pfeiffer TPG gauges | Explicit TPG361/362/366 native ACK/ENQ queries over dedicated TCP or USB serial, retaining units, identity and gauge status |
| MT-SICS scales | Immediate net weight, reported unit and weighing status over dedicated TCP or serial command interfaces |
| A&D balances | Fixed standard-format Q replies over configured serial or a transparent TCP bridge, retaining exact decimal fields and device states |
| Sartorius SBI balances | Explicit SBI16/22 manual-print replies over configured serial or a transparent bridge, retaining exact numeric fields and nonnumeric states |
| Vaisala WXT530 | Polled ASCII composite weather measurements with reported units, invalid-data status and optional CRC |
| SICK LiDAR | Single-layer CoLa A or model-specific CoLa B scans with raw distance/RSSI channels and scanner metadata |
| SICK Compact UDP | Passive multiScan100/picoScan100 Type 1 v3/v4 scan or Type 2 v1 IMU telegrams, with exact raw archives and native packed columns |
| Micro-Epsilon ILD2300 | Passive single-DIST1 RS422 words with exact tagged raw codes and application errors |
| NMEA GNSS receivers | Passive GGA/RMC position, fix status and raw time/date over TCP or serial |
| Velodyne VLP-16 | Passive modern 1206-byte UDP packets in bounded raw groups with exact receive clocks |
| Velodyne HDL-32E | Explicit product-0x21 UDP profile with the shared bounded archive and independent model metadata |
| Ouster OS0/OS1/OS2 | Passive metadata-qualified lidar or IMU UDP packets, native fields and immutable metadata/raw groups |
| Hesai PandarXT32 | Passive protocol 6.1 lidar or companion GPS UDP packets, native fields and immutable raw groups |
| Livox SDK2 and legacy | Passive explicit Mid360/S/L, HAP TX/T1, Horizon or Avia sampling profiles, native point/IMU values and immutable raw groups |
| MQTT | Built-in MQTT 3.1.1 over TCP or verified TLS, with optional credentials/client certificates |
| NI-DAQmx | Owned continuous internal-clock or explicit software on-demand voltage, thermocouple, current, RTD, strain, bridge ratio, acceleration, resistance, thermistor, physical force/pressure/torque bridge or mixed AI; software-only digital-port, counter count/frequency/period/pulse-width/encoder-position input through an installed Linux/Windows driver, retaining exact native words and explicit requests |
| OPC-UA | Built-in native Read client; anonymous or pinned SignAndEncrypt sessions |
| EtherCAT | Built-in passive Linux Ethernet capture using a configured logical PDO map |
| Profinet | Built-in passive Linux RT capture using an engineered IO layout |
| EtherNet/IP | Built-in read-only Logix atomic tags, explicit DWORD/packed BOOL reads, STRING members and opaque structures, Micro800 atomic tags and counted STRING values and published Omron NJ/NX variables |
| EtherNet/IP Assembly | Built-in explicit read of a configured Assembly Data attribute, with raw bytes and typed field mappings |
| Rockwell PCCC | Built-in EtherNet/IP typed reads of SLC/MicroLogix files, including whole timer/counter and ST string elements, direct or through one backplane slot; direct PLC-5/E N/B/F files |
| Rockwell CSPv4 | Built-in direct SLC 5/05 legacy TCP typed reads, including whole timer/counter and ST string elements |
| Rockwell DF1 | Built-in direct RS-232 Full Duplex reads of SLC/MicroLogix typed files, including whole timer/counter and ST string elements |
| Helper | Explicit Unix executable or Windows 10/Server 2016+ absolute `.exe` for a vendor SDK or custom protocol, using bounded JSONL readings |

Camera stills, video and sensor values can share one recording. Stills and video
sources with the same camera selector share one isolated camera worker. Receive
times do not establish hardware clock synchronization. OPC-UA reads run in the CLI;
passive fieldbus capture requires visible traffic and the plant's configured signal layout. Custom Unix or Windows executable helpers use the `helper` protocol, or remain available
as overrides for the four industrial stacks.

A timeout during an unfinished operation fails the recording even at its common
cutoff. Earlier completed polls remain recoverable; a failed poll publishes no partial
siblings. Ordinary passive idle is successful. Modbus RTU can also stop cleanly while
waiting for its initial quiet gap before a new poll sends any request; a gap between
siblings or a partial reply belongs to an in-flight poll and remains a failure.

### PLC family configuration paths

Use the protocol and access mode enabled in the PLC project. A family name alone
does not identify its Ethernet module, firmware, security settings or memory map.

| PLC family | Configured acquisition path |
| --- | --- |
| Siemens SIMATIC S7-1200 / S7-1500 | `s7` for permitted classic absolute access to non-optimized memory; `opc_ua` when the CPU exposes the required nodes |
| Siemens SIMATIC S7-300 / S7-400 | `s7` for classic absolute memory, indexed timer and counter words, and S5TIME values |
| Rockwell CompactLogix / ControlLogix | `ethernet_ip` for atomic symbolic tags and explicit STRING members, with a backplane slot when needed; see [Rockwell's data-access guide](https://literature.rockwellautomation.com/idc/groups/literature/documents/pm/1756-pm020_-en-p.pdf) |
| Rockwell Micro820 / Micro850 / Micro870 | `ethernet_ip` with `profile: "micro800"` for global numeric variables or explicit counted STRING reads over a direct Class 3 connection |
| Rockwell SLC 500 / MicroLogix | `pccc` where the CPU/Ethernet interface exposes EtherNet/IP `Execute_PCCC` typed logical reads; N/B/F files, model-dependent MicroLogix L files, whole T/C elements and whole ST string elements where the CPU supports them. `df1` reads these files on a direct RS-232 port configured for DF1 Full Duplex. Use `pccc_csp` for SLC 5/05 with legacy CSPv4 on its direct Ethernet channel. |
| Rockwell PLC-5/20E, PLC-5/40E, PLC-5/80E | `pccc` with `profile: "plc5"` for direct EtherNet/IP typed N/B/F reads on supported firmware |
| Schneider Modicon M340 / M580 | `modbus_tcp` with mapped registers; [Schneider specifies zero-based addresses and low-word-first multiword values](https://www.se.com/uk/en/faqs/FA363035/) |
| Delta DVP-ES2 / EX2 / EC5 / SS2 / SA2 / SX2 / SE | `modbus_ascii` or `modbus_rtu` on a configured serial port with the model's register map. [Delta programming manual](https://filecenter.deltaww.com/Products/download/06/060302/Manual/DELTA_IA-PLC_DVP_ES2-EX2-EC5-SS2-SA2-SX2-SE-TP_PM_EN_20231221.pdf) |
| Mitsubishi MELSEC Q / L / iQ-R / iQ-F | `slmp` where the Ethernet interface supports configured 3E binary or ASCII device access. Select its supported device format and address ranges |
| Mitsubishi MELSEC-F FX3S / FX3G / FX3GC / FX3U / FX3UC | `mc_1e` with a compatible FX3U-ENET or FX3U-ENET-ADP module and an enabled TCP MC port, or `fx_computer_link` on a configured Computer Link serial interface |
| Keyence KV-8000 / KV-7500 / KV-5500 and compatible Host Link units | `keyence_kv` for configured native RD/RDS TCP access |
| LS ELECTRIC XGK / XGI / XGR / XGB | `ls_xgt` on a configured FEnet dedicated TCP server with literal project memory addresses |
| LS ELECTRIC XGB with a dedicated Cnet interface | `ls_cnet` with `profile: "xgb_dedicated"` on a configured native serial server |
| FATEK FBs with native RS232 Port0 | `fatek_fbs` with `profile: "fbs_port0"`, the documented FATEK cable and configured fixed 7E1 serial settings |
| Yokogawa FA-M3 F3LE01 / F3LE11 | Configured binary or ASCII higher-level PC Link TCP service with literal project device addresses |
| Panasonic FP7 / FP-XH / FP0R / FP0H | `mewtocol` on a configured FP7 or FP-XH bare TCP connection, or a supported serial COM port |
| Omron CS / CJ / CP | `fins_tcp` or `fins_udp` according to the CPU or Ethernet module |
| Omron NJ / NX | `ethernet_ip` with `profile: "omron_nj_nx"` for published variables, FINS with the configured memory map, or `opc_ua` where the CPU serves the required nodes. Use `fins_udp` for NX1P2 FINS |
| Omron CP1E / CP2E and serial CS / CJ / CP | `omron_host_link` on a configured Host Link-capable CPU port or serial board |
| Omron CPM1 / CPM1A / CPM2A / CPM2C / SRM1 / SRM1-V2 | `omron_host_link` with explicit `mode: "c_mode_cpm"` and configured `model` on a matching serial interface |
| Beckhoff TwinCAT PLC runtimes | `ads` with the configured AMS route, runtime port and typed address/symbol map |

The Omron model-specific services are listed in its
[NJ/NX Ethernet manual](https://files.omron.eu/downloads/latest/manual/en/w506_nj_nx-series_cpu_unit_built-in_ethernet_ip_port_users_manual_en.pdf).
Each example below needs the actual device's engineered settings and values.

## Try a complete recording without hardware

Save this as `signals.json`:

```json
{
  "version": 1,
  "sources": [
    {"id": "press", "protocol": "simulator", "signal": "cycle", "every_ms": 100},
    {"id": "conveyor", "protocol": "simulator", "signal": "tick", "every_ms": 250}
  ]
}
```

```sh
capturefab data record --config signals.json --for 5s -o run.jsonl --json
capturefab data export run.jsonl -o run.csv
capturefab data export run.jsonl -o press.jsonl --format jsonl --source press
```

Each counter starts at zero. Recording and export paths must be new files; commands
do not overwrite existing data. `--json` returns one command result on stdout.
Measurements go into the requested file.

## Record images beside sensor values

Add a `camera` source to the same `sources` array as your sensor adapters:

```json
{
  "version": 1,
  "sources": [
    {"id":"press", "protocol":"simulator", "signal":"cycle", "every_ms":10},
    {"id":"inspection", "protocol":"camera", "camera":"sim:0",
     "output":"frames/{seq}.jpg", "every_ms":100, "timeout_ms":2000,
     "settings":["Width=640", "Height=480"]}
  ]
}
```

The camera stays open in its own worker throughout the run. `output` is a local
filename template ending in `.jpg`, `.jpeg`, `.png` or `.raw`; `{seq}` picks new
names without replacing images. Raw output includes its layout sidecar as usual.
`signal` defaults to `frame`. Optional `quality` is 1–100, `warmup_ms` defaults to
zero, and `settings` applies camera assignments once before acquisition. Increase
`timeout_ms` for camera connection, exposure or warmup requirements.

For native GigE Vision or GenTL multipart stills, set `part_format` to a format accepted
by `capture --part-format`, such as `"Coord3D_C16"` with `"output":"frames/{seq}.raw"`.
Choose a local output format supported by the selected part. Omit `part_format`
to use the camera's default still selection. Still sources with the same camera
selector share one worker and can choose distinct part formats and output names.
Add `part_index` from 0 through 31 to choose an exact table ordinal when multiple
parts have the same format; it requires `part_format`. Ordinary selections include
`Mono8`, `RGB8` and `YCbCr411_8`. The expected type and PFNC format are checked on
every frame. An ordinal does not infer a semantic component identity. Use an
unbound base camera selector for different per-source choices.
Jobs due for the same frame carry the same receive timestamp. Sources on different
schedules can capture different frames.
[Native GigE Vision multipart formats](camera-compatibility.md#gige-vision-and-usb3-vision)

Each camera sample's `value` references the saved file and includes its size,
frame ID, dimensions, pixel format, available camera `device_ts_ns`, optional raw
`device_timestamp`, and the worker's wall-clock `received_at`. `device_timestamp`
preserves exact `ticks`, with `frequency_hz` only when known; see
[source clocks](camera-compatibility.md#source-clocks) for clock domains and units.
`pfnc_code` identifies a PFNC sensor layout. Qualified IDS grouped mono or RG/GR/GB/BG Bayer frames
instead carry `pixel_encoding` with the original IDS name/code and reported GenTL
namespace; the declaration stays in RAW sidecars and indexed video. RAW keeps grouped
sample bytes. Mono PNG and lossless FFV1 preserve decoded sample precision; Bayer PNG
uses 8-bit color and reports the original `source_bits`. Bayer FFV1 stores the exact
unpacked mosaic as gray, with its source color-filter layout retained in metadata.

`bit_depth` describes the stored sample precision, with `source_bits` when encoding
reduces it (for example, a 12-bit sensor saved as an 8-bit JPEG). Raw references
also include `sidecar_path` so consumers can find their layout metadata.
For multipart stills, `multipart` references the full component archive and its
layout JSON, including which part supplied the primary still.
When a frame includes vendor chunk data, `chunks` references its complete original
payload in `PATH.chunks.raw` and layout/offsets in `PATH.chunks.raw.json`. Image
pixels remain normalized for the selected image encoding. When the camera's XML
exposes chunk features, the companion JSON's `chunks.values` contains decoded
scalar `values`, explicit XML `units`, per-feature `errors` and `truncated`.
The original bytes remain available when a feature cannot be resolved. See
[camera chunk handling](camera-compatibility.md#gige-vision-and-usb3-vision) for
interpretation limits and the frozen feature context.
The sample's main `time` and `elapsed_ns` use
the shared host monotonic frame-receive clock. Pixel bytes stay in image files;
CSV/JSONL exports preserve references and metadata alongside sensor values.

Acquisition stops at the common cutoff. An already received image gets up to five
additional seconds to finish encoding, sync to storage and deliver its metadata.
A delivery timeout fails the run instead of silently omitting its final image.
A frame arriving after the cutoff fails with `late_file` details if a file was
already saved. On interruption, complete samples stay recoverable. This command
uses its own camera workers; it does not borrow or modify a running camera session.

## Record video, JPEGs and sensor values together

Use `video` for indexed video, and optionally add a `camera` still source with the
same selector. This runnable example needs FFmpeg and a build with JPEG support:

```json
{
  "version": 1,
  "sources": [
    {"id":"press", "protocol":"simulator", "signal":"cycle", "every_ms":10},
    {"id":"video", "protocol":"video", "camera":"sim:0", "output":"clip.mkv",
     "codec":"h264", "fps":30, "timeout_ms":5000,
     "settings":["Width=640", "Height=480"]},
    {"id":"images", "protocol":"camera", "camera":"sim:0",
     "output":"frames/{seq}.jpg", "every_ms":500, "timeout_ms":5000}
  ]
}
```

```sh
capturefab data record --config sources.json --for 10s -o run.jsonl
capturefab data export run.jsonl -o signals.csv
capturefab data export run.jsonl -o video-frames.jsonl --format jsonl --source video --signal frame
```

The video `output` is a new literal local `.mkv`, `.mp4`, `.mov`, `.ts` or `.webm`
file. Templates and network destinations are not accepted for indexed video.
The optional `index` selects a new JSONL frame-index file; its default is the
video filename followed by `.frames.jsonl`. Both files use no-overwrite storage.
`fps` defaults to 30 (0.1–240); `codec` defaults to `h264`, `encoder` to `auto`,
`bitrate` to `8M`, `warmup_ms` to zero, and the per-frame `signal` to `frame`.
`ffv1` with `.mkv` keeps supported input samples losslessly; WebM needs AV1.
Encoder discovery happens before the run's common acquisition clock starts.

Each `video_frame` sample references `path` and `index_path`, its zero-based
`frame_index`, presentation position `video_time_ns`, camera `frame_id`, available
`device_ts_ns`, and input dimensions/pixel format. The sample's `time` and
`elapsed_ns` use frame receipt in the camera worker, before encoding. These are
also the basis for still and sensor correlation. Input dimensions describe the
sensor frame; an encoder may pad odd dimensions in the video.

Indexed video uses frame-count timestamps at the requested FPS, passes through
every frame handed to the encoder, and disables duplicate filling, compressed
stream copy and encoder restarts. Container time bases can round presentation
times. A camera stall therefore leaves a gap in the host timeline while video
positions remain consecutive. Match analysis to `frame_index` and host `time`,
rather than assuming video position equals elapsed acquisition time. Frames that
the encoder queue cannot take are counted in the final `dropped` counter; they do
not get index entries. The requested FPS sets the video sampling rate.

The persistent index has one row per encoded input with `frame_index`,
`video_time_ns`, `frame_id`, `received_monotonic_ns`, optional `device_ts_ns` and
raw `device_timestamp`, `width`, `height` and `pixel_format`. Its host clock has an arbitrary boot epoch;
the unified run rows supply its UTC mapping. Index rows and queues are bounded;
the index is streamed to storage, not accumulated in memory.

At the common cutoff the source stops recording, finalizes the video and index,
and drains remaining index rows before the run's end row is committed. This can
take up to 30 additional seconds. A final `recording` signal carries
`kind: video_recording`, both paths, frame count, FPS, codec, encoder, accepted and
dropped counts, bytes and `finalized: true`. A source failure or Ctrl-C also
attempts this finalization; the run itself still reports failed or interrupted.
An index/storage/finalization error fails explicitly. Export of an incomplete run
still needs `--allow-incomplete`, and its video should be treated as partial.
If the source received the original worker's index-ownership receipt, it can stop
that exact job and drain its complete valid index prefix even when the startup
reply is lost. A configured path or an existing file
alone never establishes ownership. A final recording sample requires the actual
worker outcome, valid finalized-file metadata and a matching indexed frame count;
the overall run still retains its startup failure. Malformed, oversized,
out-of-order or unfinished final index rows fail explicitly and stop further
index reads. Index rows are limited to 4096 bytes including the newline.
When indexed video input frames contain vendor chunks, the final recording value's `chunks` references
`VIDEO.chunks.jsonl` and `VIDEO.chunks.raw`. Entries use the successful encoder
input's `frame_index`, preserving raw vendor chunks beside the video. Chunk-bearing
streaming or stdout output requires a companion sink and is rejected when no such
sink is available.

Give still and video sources exactly the same camera selector to share a worker.
Their settings are merged and applied once before either starts. Contradictory
assignments to one feature are rejected before acquisition. Use one still source
and one video source per camera; external running sessions are not borrowed.

For a camera exposing a supported write-only `ActionDeviceKey`, either source can
declare `"action_device_key":305419896`. It is an exact unsigned 32-bit integer;
zero is a declared key. Sources with exactly the same selector share the
declaration: an omitted or null field inherits it, equal values request one write,
and conflicting values are rejected before adapters or recording outputs open.
Do not also assign `ActionDeviceKey` in `settings`, including a banked assignment.

The run opens its own camera worker, applies the merged ordinary settings, then
assigns the key once before admitting still or video acquisition. Later camera
commands stay on that same worker. Initialization failure or owner loss fails the
shared source; it does not reconnect or apply the key to a replacement. Accepted
still-file receipts and a received video-ownership receipt can still support draining
after failure. The key is not
read back, restored on release, saved as a checkpoint or copied into generated
recordings/exports. The user-authored input configuration retains its literal.

The original source `timeout_ms` covers startup, setup waiting, key assignment and
first-arm admission. It is not restarted between those steps. Already-running
ordinary settings/backend calls keep their existing cancellation and rollback
semantics. A sibling's independent timeout does not renew or cancel the active
initializer's cutoff. Inspect typed errors and possible-write details before
starting a new run.
Initialization's `write_attempted` and `write_may_have_applied` details refer to
the key assignment. `camera_operation_may_have_applied` can separately describe
an already-dispatched ordinary settings call.

Configure the actual selector/group/trigger features in `settings` for the chosen
camera. This field does not send an Action Command or provide an acquisition-ready
signal; the recording header is not proof that the camera is armed. Send actions
only after arranging readiness for that device. See the
[camera Action guide](camera-compatibility.md#immediate-network-actions) for the
separate sender, supported key paths and ACK limitations. External named-session
key configuration is for manual capture; an industrial run owns a separate worker.

## LMI GoPxL measurements over Ethernet ASCII

`gopxl_ascii` polls GoPxL sensor measurement results with the documented Ethernet
ASCII `measurement` command. Select `profile: "gopxl_1_3_measurement"`; it follows
the GoPxL 1.3 polling commands, which the 1.4 and 1.5 manuals repeat. Configure the
sensor's Ethernet ASCII protocol externally: disable asynchronous output, map the
measurements in its connection map, and keep the delimiter, terminator and invalid
value that the recording declares. The default port is 8190, but the endpoint is
always explicit.

```json
{
  "version": 1,
  "sources": [
    {
      "id": "gauge",
      "protocol": "gopxl_ascii",
      "profile": "gopxl_1_3_measurement",
      "address": "192.0.2.41:8190",
      "delimiter": ",",
      "terminator": "crlf",
      "invalid_token": "INVALID",
      "trailing_delimiter": "either",
      "every_ms": 100,
      "timeout_ms": 1000,
      "measurements": [
        { "name": "gap_width", "id": 0 },
        { "name": "flush_height", "id": 1 }
      ]
    }
  ]
}
```

Each poll sends exactly one `measurement` query listing the configured IDs, for
example `measurement,0,1` plus the terminator, and reads one reply up to that
terminator. `terminator` is `crlf`, `cr` or `lf` and applies to both directions.
The manufacturer's examples disagree on a trailing delimiter, so
`trailing_delimiter` is `absent`, `present` or `either`; each sample records
whether one was seen. IDs are 0–65535 (a Capturefab bound) and must come back in
request order; leading zeros are accepted and kept as raw text.

Each measurement keeps its raw ID, value and decision text. A valid value adds the
signed 32-bit `scaled_i32` (the documented measurement multiplied by 1000) and
`value`, its exact decimal with three fractional digits, computed without floating
point. `invalid_token` marks an invalid value, with no number. The decision byte
gives `pass` (bit 0) and `decision_invalid` (bit 1); other bits are kept unexplained
in `decision_reserved_bits`. Units come from the sensor's job and are not queried.

An `ERROR` reply keeps the sensor's message verbatim and classifies only exact
GoPxL 1.3 texts. Any error, malformed or mismatched reply, a reply over 64 KiB,
bytes after the terminator (asynchronous output left on) or a late reply withholds
the whole poll; earlier polls stay recoverable. No retry or reconnect follows, and
no start, stop, trigger, job, alignment, property or stamp command is ever sent.
Sample time is the host receive time of the complete reply; a poll is not proof of
a fresh scan, and no sensor timestamp becomes `source_time`.

The binary GoPxL Data Protocol (TCP 3601) is not supported: LMI publishes its
serialization only inside the GoPxL SDK. The existing `gocator_gdp` adapter covers
legacy Gocator GDP only.
[GoPxL 1.3 polling commands](https://ap.lmi3d.com/manuals/gopxl/gopxl-1.3/LMILaserLineProfiler/Content/Protocols/ASCIIProtocol/EthernetCommunication/PollingCommands.htm) ·
[GoPxL Ethernet ASCII settings](https://ap.lmi3d.com/manuals/gopxl/gopxl-1.3/LMILaserLineProfiler/Content/WebInterface/Control/ASCIIProtocol/Ethernet_ASCII_Protocol.htm)
Documented compatibility does not qualify a physical sensor or firmware.

## Opto 22 groov inputs over OptoMMP

`optommp` uses the explicit `groov_expanded_inputs` profile with model
`epic_pr1`, `epic_pr2`, `rio_mm1` or `rio_mm2`. Supply a numeric TCP endpoint
and named channels with exact expected module and configured channel-type IDs.
The adapter checks the documented input allowlist in addition to those IDs;
output, energy, serial, disabled and unsupported FastADC layouts are excluded.
RIO accepts module zero only, with input channels zero through seven. EPIC
module indices are zero through fifteen, subject to actual module readback.

This example is an explicit selector request; match the configured input type
and endpoint to the actual installation.

```json
{
  "version": 1,
  "sources": [
    {
      "id": "edge_io",
      "protocol": "optommp",
      "profile": "groov_expanded_inputs",
      "model": "rio_mm1",
      "address": "192.0.2.22:2001",
      "every_ms": 100,
      "timeout_ms": 1000,
      "channels": [
        {
          "name": "channel0",
          "module": 0,
          "channel": 0,
          "resource": "analog_eu",
          "expected_module_id": 4026531874,
          "expected_channel_type": 1610612757
        }
      ]
    }
  ]
}
```

Clear Powerup Clear externally before capture and keep channel configuration
stable. Coordinate existing watchdog and line-owner policy: host communication
can participate in watchdog timing. Capture sends no configuration, PUC,
watchdog or output writes. Its address allowlist also excludes reads that clear
latches/minima or calculate calibration.

Each poll checks unit identity and PUC, validates selected module/channel types,
and reads native values and quality codes under one original deadline. Only a
complete successful poll becomes a batch; previous completed polls remain
recoverable after failure. No retry or reconnect follows an ambiguous exchange.
Native error replies preserve the validated response-header prefix without
assuming an unseen error payload.

Analog engineering values preserve the exact IEEE binary32 word, including
signed zero, NaN and infinity; only finite values have a numeric JSON value.
Digital state stays an unsigned 32-bit word. Quality is a native code, including
unknown codes. Device scaling and application-managed labels do not establish
physical units or calibration, so units provenance remains
`externally_configured_unqueried`.

Generic sample time is the completed-poll frontier. Individual value and quality
response clocks remain in each structured value; sequential requests do not
provide an atomic snapshot or hardware synchronization. No device uptime or
local clock becomes `source_time`. Finish closes TCP without device commands.

The wire and map authority is the manufacturer’s [OptoMMP Protocol Guide,
form 1465-260805](https://documents.opto22.com/1465_OptoMMP_Protocol_Guide.pdf).
Documented admission does not qualify a physical device or firmware installation.

## Atlas Copco PF6000 tightening results

`atlas_open_protocol` subscribes to last-tightening results from an explicitly
configured PF6000 Virtual Station TCP endpoint. Select `profile: "pf6000_v2_8"`;
this finite profile follows the PF6000 Open Protocol 2.8 appendix, valid from
controller software 2.8.14. It requests startup revision 5 (system type `003`)
and result MID 0061 revision 3, with application acknowledgments. No vendor
runtime or optional Cargo feature is needed.

```json
{
  "version": 1,
  "sources": [{
    "id": "tightener",
    "protocol": "atlas_open_protocol",
    "profile": "pf6000_v2_8",
    "address": "192.0.2.60:4545",
    "signal": "tightening",
    "timeout_ms": 2000
  }]
}
```

Provision the unique Virtual Station port and set its disconnect action to
**None** externally, coordinating the connection with the line owner. The
adapter does not change controller settings. Other disconnect actions can lock
the tool or remove its task. Confirm the actual firmware and sharing policy;
the profile name is a requested wire contract, not a firmware readback or a
physical-device qualification.

Each event produces one `atlas_open_protocol_result` value with the complete
420-byte frame in `raw_hex`, qualified startup metadata in `controller`, and
the finite native fields in `result`. Torque fields retain their unsigned
decimal integers with `torque_scale: 100` and native `torque_unit_code`; known
codes have a unit label, including `percent`, without a physical conversion.
Angles retain native degrees. Blank supported numeric values are listed in
`unavailable_fields`; model-unsupported spans remain opaque in the raw frame
and their IDs appear in `unsupported_fields`. Unknown numeric codes remain
native codes. Local controller time strings stay in the value; they do not
become UTC `source_time` or establish clock synchronization.

Tightening IDs retain leading zeros and their numeric value. They are
controller-global and can be nonconsecutive. Equal IDs produce separate rows;
there is no uniqueness, missing-cycle or exactly-once inference. Pset values
are native tightening-menu indices. This subscription does not fetch historical
results or reconnect after ambiguity.

The writer checkpoints each result's JSONL row before the source sends MID
0062. Checkpoint means successful OS file-data sync plus the existing directory
sync where supported; unsupported or permission-denied directory sync can be
skipped. It is not a universal pathname or power-loss guarantee. Disk sync
cannot be forcibly cancelled by a protocol deadline. A receipt that arrives
late, a writer failure or cancellation does not authorize an ACK. The ACK send
uses the original read deadline, which also covers initialization when needed,
housekeeping and the checkpoint wait. An OS-accepted ACK write does not prove
controller processing. A saved result can repeat after a crash or lost ACK;
the event row's `ack_policy` describes this policy and never claims ACK success.

Idle subscriptions are ready without a tightening. Partial frames and in-flight
control timeouts fail the recording while preserving earlier complete rows.
Keepalive runs after five seconds of inactivity under the current operation's
deadline. Acquisition clocks are captured at complete-frame receipt, before
decoding or disk work. Finish unsubscribes under one cleanup budget and closes
the connection; a result first received during shutdown is retained in failure
details without an ACK or post-cutoff sample. No program selection, tool
enable/disable, reset, calibration, link-mode fallback or automatic retry is sent.

Wire authority: the manufacturer-authored [Open Protocol 2.8 specification
(mirrored copy)](https://s3.amazonaws.com/co.tulip.cdn/OpenProtocolSpecification_R280.pdf)
and [official PF6000 appendix](https://servaid.atlascopco.com/AssertWeb/en-US/AtlasCopco/Document/10268853/GetFile).
Mirror authorship and document identity are corroborated; the mirror is not a
vendor-authenticated distribution. Operator setup follows the [PF6000 Virtual
Station configuration manual](https://picontent.atlascopco.com/cont/external/dir/ef/21870220171__html5_external/en-us/91215627.html).

## Universal Robots RTDE output telemetry

Use `ur_rtde` with the required `rtde_v2` profile for an enabled RTDE TCP service
on an e-Series or ur-Series controller. Supply a numeric unicast endpoint and the
exact field names and types advertised for that controller's software version:

```json
{
  "version": 1,
  "sources": [{
    "id": "robot", "protocol": "ur_rtde", "profile": "rtde_v2",
    "address": "192.168.1.20:30004", "frequency_hz": 125,
    "timeout_ms": 2000, "signal": "state",
    "fields": [
      {"field": "timestamp", "wire_type": "DOUBLE"},
      {"field": "actual_q", "wire_type": "VECTOR6D"},
      {"field": "robot_status_bits", "wire_type": "UINT32"}
    ]
  }]
}
```

This adapter negotiates protocol v2, reads the controller software version, sets
up one output recipe and starts output synchronization. It sends only RTDE
`V`, `v`, `O`, `S` and `P` commands. The final pause stops output synchronization;
it does not pause a robot program. Input recipes, register writes, URScript and
motion commands are absent. There is no version fallback or automatic field/type
substitution. A recipe or type refusal fails explicitly.

`fields` contains 1–128 unique ASCII identifiers with at most 2048 bytes in the
comma-separated name list. Supported exact `wire_type` values are `BOOL`, `UINT8`,
`UINT32`, `UINT64`, `INT32`, `DOUBLE`, `VECTOR3D`, `VECTOR6D`, `VECTOR6INT32` and
`VECTOR6UINT32`. `frequency_hz` defaults to 125 and must be finite from 1 through
500. This is the requested frequency; firmware, field availability, controller
load and the actual delivery rate remain device-specific. `every_ms` does not
poll this streaming source. Setup and each read use one shared operation budget.

Each complete output packet becomes one `state` sample containing the ordered
fields, exact raw packet in base64, accepted recipe ID, observed controller
software version, requested frequency and host output index. Integer widths stay
exact, including `UINT64`. A BOOL retains its raw byte and normalized truth value.
Each floating-point word retains `ieee754_hex` and `classification`; finite words
also have a numeric `value`. NaNs, infinities and negative zero retain their bit
patterns. Vectors retain their declared shape. Units, coordinate frames, enum
meanings and application clocks are not inferred from field names. In particular,
the robot's `timestamp` field does not become a UTC `source_time`.

Bounded v1/v2 asynchronous text packets retain their original message/source
bytes and raw severity. Fatal/error messages fail the source. Frames are capped
at 8192 bytes; startup/control queues admit at most 32 packets and 64 KiB of raw
data. Original host receive times survive queuing. A malformed/partial packet,
EOF or failed control exchange retires the channel, with no reconnect/retry.
Completed samples remain recoverable through `--allow-incomplete`; partial
packets and traffic received during final cleanup are excluded. An accepted
startup followed by healthy idle can finish with zero samples.

The [current manufacturer RTDE guide](https://docs.universal-robots.com/tutorials/communication-protocol-tutorials/rtde-guide.html)
documents the v2 recipe and e-Series/ur-Series frequency ceiling. Older firmware
documentation describes lower ceilings; configure the actual controller rather
than assuming every request is delivered at the requested rate.

## ABB Robot Web Services controller and RAPID state

Use `abb_rws` with an explicit `rws1` or `rws2` API and enabled controller UAS
credentials. ABB documents the older RWS API for RobotWare 6 and the versioned
RWS2 API for RobotWare 7. Configure the API actually offered by the controller;
the adapter does not infer it from the controller name or fall back between APIs.

```json
{
  "version": 1,
  "sources": [{
    "id": "robot", "protocol": "abb_rws", "api": "rws2",
    "address": "192.0.2.30:443", "every_ms": 1000, "timeout_ms": 3000,
    "tls": {"server_name": "robot.example", "ca_certificate": "plant-ca.pem"},
    "username_env": "ABB_USER", "password_env": "ABB_PASSWORD",
    "reads": [
      {"signal": "controller", "resource": "controller_state"},
      {"signal": "mode", "resource": "operation_mode"},
      {"signal": "system", "resource": "system_info"},
      {"signal": "execution", "resource": "rapid_execution"},
      {"signal": "count", "resource": "rapid_symbol",
       "task": "T_ROB1", "module": "Main", "symbol": "Count"}
    ]
  }]
}
```

RWS1 uses the offered HTTP Digest challenge. RWS2 uses Basic authentication
inside verified TLS, requiring the `mqtt-tls` build feature shared by industrial
TLS transports. Custom plant CAs and the configured server name are verified.
The numeric unicast IPv4 endpoint and port are explicit. Server-issued session cookies remain
private to this source and are reused within their scope; use one source with
several reads for one controller.

The adapter issues only the GET paths for the five resources above. RAPID
symbols use separate bounded task/module/symbol identifiers. Whole array and
record values remain exact logical text, including quotes, whitespace and XML
entity decoding, alongside observed datatype/dimension metadata. It does not
guess a numeric representation, units or record layout. Symbol properties and
value are separate observations with separate host windows, so their pairing is
not an atomic controller snapshot. Every response retains its bounded original
XHTML and SHA-256. Unknown future state strings remain captured with their known
classification; controller stop/fault states are valid observations.

There are 1–32 reads per source, at most 256 KiB per HTTP body and 64 KiB per
symbol value. One poll has a bounded response/output budget. Requests, including
authentication and successive polls, are sequential and paced at least 50 ms
apart. Set `timeout_ms` for all selected GETs and authentication together; it is
one deadline. A failed poll publishes no partial vector and retires the source
without hidden reconnects. Completed earlier samples can be exported with
`--allow-incomplete`. Host observation windows provide timing; system start time,
HTTP dates and RAPID clock values do not become UTC `source_time`.

This acquisition path sends no mastership, motion, program, configuration or
subscription commands. It does not establish readiness, safety, fresh producer
data, synchronized robot/camera clocks or real-time trajectory delivery. Physical
controller/firmware qualification remains device-specific. The wire fixtures use
synthetic responses derived from ABB's [RWS1 manual](https://developercenter.robotstudio.com/api/rwsApi/)
and [RWS2 introduction](https://developercenter.robotstudio.com/api/RWS/Swagger_Doc/Introduction.yaml).

## Yaskawa High-Speed Ethernet Server

`yaskawa_hses` reads status, current base position and variables through the
controller's HSES UDP endpoint. The explicit `yrc1000_hw1483358_rev12` profile
follows the YRC1000 Ethernet manual; it does not qualify every firmware or the
DX200 wire contract. Enable/configure HSES externally and select installed
variable numbers. Capturefab changes no controller settings, jobs, motion or
files. DX200 support is manufacturer-declared, but its separate HSES manual
requires qualification before claiming equivalent decoding.

```json
{"version":1,"sources":[{"id":"robot","every_ms":1000,"timeout_ms":1000,
  "protocol":"yaskawa_hses","profile":"yrc1000_hw1483358_rev12",
  "address":"192.0.2.30:10040","reply_horizon_ms":3000,
  "reads":[
    {"signal":"status","resource":"status"},
    {"signal":"position","resource":"base_position","group":1},
    {"signal":"byte","resource":"byte","index":7},
    {"signal":"integer","resource":"integer","index":8},
    {"signal":"double","resource":"double_integer","index":9},
    {"signal":"real_bits","resource":"real_raw","index":10},
    {"signal":"string_bytes","resource":"string32","index":11}
  ]}]}
```

`reply_horizon_ms` is required, positive, at most 30,000, and at least the source's
`timeout_ms`. It declares an operator-assumed maximum reply delay. By default,
the source rotates its ephemeral UDP port before 8-bit request IDs repeat and
excludes recently retired ports. An optional fixed `bind` quarantines IDs and
drains stale queued packets before reuse. Protection is limited to that source
epoch and declared horizon; it provides neither authentication nor an absolute
freshness or cross-process guarantee.

Status retains both raw words and unknown bits; disputed bits remain unlabeled.
Flags are controller reports, not independent safety measurements. `base_position`
groups 1–8 request robot Cartesian/base data. Only returned type 16 adds exact
integer `position_um` and `orientation_1e4_degree`; the final two slots and other
coordinate types remain raw. B/I/D values are u8/i16/i32. `real_raw` preserves the
32-bit word without assuming IEEE encoding. `string32` preserves all 32 storage
bytes, including NULs, without guessing character encoding or substituting the
legacy 16-byte prefix read.

One original poll deadline covers ID admission, sends, receives and decoding.
A complete poll is published atomically; failure retires the source without
retry/reopen, preserving earlier recorder samples. Raw replies, host windows and
native fields remain available in JSONL recordings and CSV/JSONL exports. Separate reads
are not a synchronized snapshot, and no `source_time` is inferred.

Sources: [YRC1000 HW1483358 rev12](https://www.motoman.com/getmedia/38CD89D5-C90D-4C5A-8628-0551C44C9A6C/178942-1CD.pdf.aspx?ext=.pdf),
[HSES headers](https://knowledge.motoman.com/hc/en-us/articles/21740538183959-High-Speed-Ethernet-Server-Packet-Header),
[DX200 Ethernet/HSES distinction](https://www.motoman.com/getmedia/D58DEC21-B432-4DEE-B642-24CF2A3D1213/165303-1CD.pdf.aspx?ext=.pdf).

## Epson Remote Ethernet monitoring

Use `epson_remote` for an externally enabled Remote Ethernet interface with
**Use only for monitoring** selected. Keep the controller's existing control-device
owner. Capturefab opens a private monitor connection, logs in, then sends only
the selected GET methods. It does not change the monitoring setting or control
owner, and closing a source sends no Logout, reset or motion command.

```json
{"version":1,"sources":[{"id":"robot","protocol":"epson_remote",
  "profile":"rc_plus_8_0","address":"192.0.2.82:5000",
  "password_env":"EPSON_PASSWORD","terminator":"crlf",
  "every_ms":250,"timeout_ms":1000,
  "reads":[
    {"signal":"state","resource":"status"},
    {"signal":"alarms","resource":"alarms"},
    {"signal":"robot_number","resource":"current_robot"},
    {"signal":"input_word","resource":"io_word","address":1},
    {"signal":"array_values","resource":"variable","name":"gby2",
      "declared_type":"Byte","indices":[3,0],"count":3}
  ]}]}
```

Set the password in the named environment variable. The capture bound allows an
empty password or at most 256 printable ASCII bytes without commas or control
characters; match the password configured on the controller. `address` is a numeric
unicast endpoint with an explicit nonzero port. The example's port 5000 is an
operator-configured setting. `terminator` is required: `crlf`, `cr` or `lf`.
The native transport is TCP. Match the actual controller's enabled interface,
password and terminator; these settings are not discovered or attested.

| Required `profile` | Qualified manual/controller table | Resource limits |
| --- | --- | --- |
| `rc_plus_7_5` | RC+7.5 Rev.2, RC90/RC700/T/VT | `current_robot` is refused because its response remains unqualified |
| `rc_plus_8_0` | RC+8 Rev.2, RC90/RC700/T/VT | All resources below |
| `rc_plus_8_0_rc800` | RC+8 Rev.2, separate RC800 table | `alarms` is absent from that table and refused |

`reads` contains 1–32 unique signal names. Resources are `status` (`GetStatus`),
`alarms` (`GetAlm`), `current_robot` (`GetCurRobot`), `io`/`io_byte`/`io_word`
and `memory_io`/`memory_io_byte`/`memory_io_word`, plus `variable` (`GetVariable`).
Only I/O resources take `address`, bounded to an unsigned 16-bit selector; actual
allocated controller I/O may be smaller. Their replies retain exact bit/hex text
and the native 1-, 8- or 16-bit integer value. Alarm replies preserve their count
and ordered decimal code strings, with a capture limit of 1024 codes. Robot
number replies retain both the original decimal text and unsigned integer.

Variables require a 1–128-byte ASCII identifier `name` and explicit
`declared_type`: `Boolean`, `Byte`, `UByte`, `Short`, `UShort`, `Integer`, `Long`,
`Int32`, `UInt32`, `Int64`, `UInt64`, `Real` or `Double`. Scalars omit
`indices` and `count`. Arrays require 1–3 unsigned 16-bit starting indices and
`count` 1–100; values follow the server's native order. Counts and selectors
accept exact integral JSON decimal/exponent notation. The returned
`values_text` preserves whitespace, signs and leading zeros, with
`datatype_provenance: "configured_no_numeric_conversion"`; widths, array shape,
numeric conversion and engineering units are not inferred. `String` is
recognized but unsupported: the inspected manuals do not establish scalar
escaping or embedded-terminator behavior, and they expressly forbid String arrays.

Status replies retain `flags_raw`, `flag_width` and the exact four-digit
`code_text`. The manuals' explicit definition has 11 flags, while published
examples also contain 10. Both documented widths are captured. Only 11 receives
`named_flags` using the documented positions: test, teach, auto, warning, serror,
safeguard, estop, error, paused, running, ready. Ten is marked
`unqualified_documented_width`, without a guessed mapping. These are controller
reports and do not establish physical safety, readiness to move or current mode
outside the observation window.

Every successful value also retains its profile, method and configured
selection, `raw_reply_text` without the terminator, and the complete framed
`raw_reply_base64`/`reply_bytes`. `host_window_ns` spans the command write through
the complete reply on the host monotonic clock; initial connect/Login is outside
each GET window. Queries observe separate windows. No UTC `source_time`, robot
pose, freshness or simultaneous controller snapshot is inferred.

One original poll deadline covers connect, Login, all GETs, framing, parsing
and serialized-size admission. Each framed reply is limited to 16 KiB, received
bytes to 512 KiB per poll including Login, and the escaped Reading batch to
1 MiB with 16 KiB reserved for the recorder wrapper. Only fully validated poll
results are returned. A failure retires the socket with its original typed cause;
there is no reconnect, Login replay, skipped unmatched line or control cleanup.
Native GET error codes remain in `error.details`; private Login diagnostics
exclude unknown numeric text. Already recorded rows remain exportable through
the ordinary incomplete-run contract.

Primary: [RC+7.5 User's Guide Rev.2, §12.2](https://files.support.epson.com/far/docs/epson_rc_pl_70_users_guide-rc700_rc90%28v75r2%29.pdf),
[RC+8 User's Guide Rev.2, §13.2, response tables pp.539/541](https://files.support.epson.com/far/docs/epsonrc_pl_usersguide80.pdf),
[Robot Management System setup](https://download.epson-europe.com/robotics/Manuals/Manipulator/EM202S4229F_Epson-Robot-Management-System-Users-Guide_Rev.1.pdf).
Manual and fixture qualification does not establish physical robot/firmware validation.

## MTConnect machine observations

Use `mtconnect` for an already enabled, reachable agent. This adapter reads the
agent's XML with `probe`, `current` and `sample` GET requests. It admits the
published XML namespace versions 1.2–1.8 and 2.0–2.7; agent software versions and
machine firmware are retained as reported, with no inferred equivalence.

```json
{
  "version": 1,
  "sources": [{
    "id": "mill", "protocol": "mtconnect", "address": "192.0.2.40:8082",
    "base_path": "/UMC", "every_ms": 250, "timeout_ms": 1500,
    "mode": "sample", "start": "current", "count": 256,
    "signal": "observations"
  }]
}
```

The endpoint and optional base path are explicit. Capturefab sends no enablement,
machine-control, asset or arbitrary XPath requests. Each poll reads a bounded
probe and current checkpoint; a sample poll then reads at most one available
history page. `count` is 1–1024. `effective_count` records the positive request
limit after capping it to the reported buffer size. A poll uses one deadline for
connections, authentication, XML parsing and output admission.

Default `start: "current"` records an initial `baseline_current` document, then
continues from its agent cursor. `start: "buffer_begin"` requests retained
history. An explicit resume uses `start: {"instance_id": 7, "sequence": 105}`
with the actual saved epoch and cursor. `mode: "current"` repeatedly records
current state and requires `start: "current"`. Current state can include values
older than the retained history; it does not contain every intervening change.

The cursor follows the validated header's `nextSequence`, including filtered
empty pages. Native sequences can skip and appear grouped by component/category.
A `caught_up` checkpoint preserves the cursor and emits no invented observations.
A lost buffer interval, changed agent instance or changed probe model fails
explicitly and retires the source. It does not silently reset to the newest
state. A failed poll publishes no partial batch; earlier complete recordings
remain exportable with `--allow-incomplete`.

Each document retains its original XML, SHA-256 and host observation window.
The catalogue binds native DataItem IDs to device, component, category and
published attributes. Observations retain exact logical text, namespaced
attributes and structured children. Multiple condition activations, native codes,
removals, resets and DataSet/Table deltas stay separate. Capturefab does not
reconstruct an active-condition table or interpret each delta as a full state.
Published `units`, `nativeUnits` and `nativeScale` remain distinct; values are not
rescaled or coerced into floating-point numbers. Agent timestamps stay native
attributes and do not become verified UTC `source_time` values.

Authentication defaults to `auth: "none"`. Explicit `basic` or `digest` uses
`username_env` and `password_env`; `bearer` uses `bearer_token_env`. Basic and
Bearer require verified `tls`, using the shared `mqtt-tls` feature and plant-CA
options. Digest answers the actual offered challenge without Basic fallback.
Cookies, interactive login and OAuth exchanges are not implemented.

Haas documents an NGC agent at port 8082, including `/UMC` routes and a published
UMC-750SS probe using namespace 1.2. This is producer documentation, not physical
machine qualification by Capturefab. Agent reachability, software and any machine
adapter remain installation-specific. See [Haas NGC documentation](https://www.haascnc.com/service/troubleshooting-and-how-to/how-to/machine-data-collection---ngc.html),
[MTConnect 1.2 Part 1](https://docs.mtconnect.org/MTC_Part_1_Overview%2BV1.2.pdf)
and [MTConnect 2.5 Part 1](https://docs.mtconnect.org/MBSD_MTConnect_Part_1_2-5-0.pdf).

## Keyence IV2, IV3 and IV4 inspection results

Use `keyence_iv` with an IV2, IV3 or IV4 in Run mode with Nonprocedural Command
enabled. Disable automatic status-result transmission. Keyence uses the same
`RT` frame for automatic results and replies to polls. The default command
port is TCP 8500. IV2 and IV3 allow one client, and IV4 allows two. Keep the
sensor's normal inspection triggers configured.

```json
{
  "version": 1,
  "sources": [{
    "id": "vision",
    "protocol": "keyence_iv",
    "address": "192.0.2.40:8500",
    "format": "standard",
    "every_ms": 200,
    "timeout_ms": 1500
  }]
}
```

The adapter reads model and firmware with `VI` before it sends `RT` on a new
connection. `RT` reads the latest completed result, so polling can repeat a
result or miss results between polls. Set `format` to the sensor's configured
output format. `standard` is the default. IV2 and IV3 support `detailed` with
logic outputs, processing time and program number. Use `standard_master` for
IV3 or IV4 when the sensor is configured for standard multi-master results.
IV3 also supports `detailed_master`. IV4 detailed formats are unsupported and
rejected before the first `RT` request. `signal` defaults to `inspection`.

IV4 standard supports ordinary judgments, sorting and AI Through Count. A
through-count result contains `result.through_counts` with each tool's current
count and today's total, both as integers and in their original seven-digit
fields. The device caps reported values at 9999999. The supported IV4 models
are the 400/500/600 CA and MA smart cameras and G400/G500/G600 CA and MA sensor
heads. IV4-H wire compatibility remains unverified.

Records preserve the exact `raw_reply` including the CR delimiter, the result
counter and judgment, and each tool's status and unscaled measurement code.
Use the configured job's tool type and units to interpret the measurement.
Detailed tool fields are stored as fixed-width `details_raw`, including OCR
commas and padding. `ER,RT,03` produces `state: "unavailable"`. The device
uses that response both before a result exists and when automatic transmission
is enabled. See the [Keyence IV2 Field Network manual](https://studylib.net/doc/25709387/keyence-iv2-as-106197-iv2-um-869gb-ww-gb-2010-2),
[IV3 communication guide](https://www.keyence.com/support/user/vision/iv3/communication/),
and [IV4 user manual, pages 9-147 and 9-157 through 9-165](https://www.scribd.com/document/878835580/IV4).

## Cognex In-Sight spreadsheet reads

Use `cognex_insight` with the In-Sight Native Mode Telnet server enabled and
a user permitted to read the requested cells. In In-Sight Vision Suite,
configure the service in Utilities → Industrial Ethernet → Industrial Ethernet
Configuration. The usual port is TCP 23. In-Sight 2000 uses Serial Native for
`GV`.

```json
{
  "version": 1,
  "sources": [{
    "id": "vision",
    "protocol": "cognex_insight",
    "address": "192.0.2.41:23",
    "username_env": "VISION_USER",
    "password_env": "VISION_PASSWORD",
    "every_ms": 500,
    "timeout_ms": 2500,
    "signals": [{"name":"width","cell":"Output","format":"number"}]
  }]
}
```

Set the named environment variables before running Capturefab. Set the password
variable to an empty string for an account with a blank password. Telnet sends
credentials in plaintext. The adapter checks the In-Sight banner before login
and sends only read-only `GV` requests after login. Use a legacy cell address
such as `A001`, a published name such as `Output`, or a supported ASCII dotted
EasyBuilder tag such as `Job.Robot.FormatString`.

The default `text` format retains the value text and spaces. Non-text values
use base64 in `raw.data`. A complete empty value line has `state: "empty"`.
`number` retains the original text and stores the reported number at its wire
precision. The exact device markers `#` and `#ERR` have `state: "device_marker"`
in numeric mode. The adapter expects CRLF-framed replies. Connect, login and all
cell reads share one deadline. A failed cell discards the entire poll. Sequential
reads can span multiple inspections. See Cognex's [legacy Spreadsheet GV reference](https://docs.cognex.com/is_651/web/EN/ise/Content/Communications_Reference/GetValue_Spreadsheet.htm)
and [published-cell GV reference](https://docs.cognex.com/isvs_500/web/EN/InSight_Sheet/Content/Topics/IndustrialCommunications/GetValue.htm).

## Cognex DataMan result bytes

Use `cognex_dataman` with the explicit `cip_result` profile on a configured
EtherNet/IP-capable DataMan 280, 370 or 470 Ethernet reader. Enable EtherNet/IP
and arrange acquisition and result formatting externally. The usual port is
TCP 44818. This reads the currently exposed ID Reader `DecodeResults` attribute;
it does not trigger acquisition or acknowledge, dequeue or reconfigure results.

```json
{
  "version": 1,
  "sources": [{
    "id": "reader", "protocol": "cognex_dataman",
    "profile": "cip_result", "address": "192.0.2.42:44818",
    "signal": "decode", "every_ms": 100, "timeout_ms": 1000,
    "max_result_bytes": 8164
  }]
}
```

Each sample retains the unsigned native `result_id_raw`, `result_code_raw`,
`result_extended_raw`, `result_length_raw` and `returned_storage_bytes`.
`raw` preserves the entire returned attribute as Base64, including its native
header. `active` preserves exactly the declared result bytes, and `tail` keeps
all other returned storage without inferring padding. Each carries its own
`encoding`, `scope` and `data`. Embedded NUL, CRLF and non-text bytes remain
unchanged. Add `"encoding":"utf8"` only to request a strict UTF-8 `text`
presentation alongside the bytes; invalid UTF-8 then fails the poll. Omit it for
byte-only capture. Empty/NoRead observations and unknown result-code bits are
retained as data. Result codes are separate from transport/CIP errors.

For continuously updating latest-result sampling, disable Result Buffering in
the reader's configuration. With buffering enabled, the exposed result can remain
unchanged until another client acknowledges it. Capturefab does not verify that
setting or send an acknowledgement. Repeated or wrapping result IDs remain
ordinary observations. Polling can miss overwritten results and does not establish
fresh decodes, a lossless stream, a device timestamp or trigger association.
Disable Result Fragmentation externally when whole decode results are required.
If another consumer advances fragments, the exposed attribute may contain one
fragment even when its local length is consistent. Capturefab preserves that
attribute and does not claim it reconstructs the whole original barcode or
assemble and acknowledge fragments. Neither external setting is verified.

One fixed read obtains the complete attribute; changing latest-result fragments
are never stitched together. A partial CIP reply, a short header, or a declared
length larger than the returned storage fails explicitly. In particular, a reader
may truncate an oversized result while still reporting its larger original length.
Cognex's documentation gives differing storage capacities in its operation and
assembly descriptions; the adapter uses the actual returned extent and does not
infer a universal capacity. `max_result_bytes` limits the active result to 1–8164
bytes (default 8164), within the existing 8192-byte transport bound. It does not
increase the reader's own capacity or trim an oversized result into success.

Connection, registration, the attribute read, decoding and final publication share
one original timeout. A failed poll publishes no sample and permanently retires
that source connection; earlier completed samples remain recoverable. The only
application operation is `Get_Attribute_Single` for class `0x79`, instance 1,
attribute `0x10`. No acquisition, DMCC command, result ACK, setting write or cyclic
I/O service is sent. This is a native result observation, distinct from In-Sight
spreadsheet reads and from the delimiter-framed DataMan text protocol.

[Cognex ID Reader object](https://docs.cognex.com/dmst_2620/web/EN/Industrial_Protocols_Manual/Content/Topics/PDF/DIPM/eip-object-model.htm) ·
[Cognex buffering and truncation](https://docs.cognex.com/dmst_2620/web/EN/Industrial_Protocols_Manual/Content/Topics/PDF/DIPM/eip-operation.htm) ·
[EtherNet/IP model configuration](https://docs.cognex.com/dmst_2620/web/EN/ReaderCodes_Manual/Content/RPCodes_Topics/CommunicationSettings.htm)

## Configure several sources

A configuration requires `version: 1` and between 1 and 64 sources. Each source has
a unique `id`, a `protocol`, and the adapter options described below. Source IDs and
signal names are nonempty text without control characters.

Integer settings accept exact integral JSON decimal and exponent forms: for
example, `version: 1.0`, `baud: 1.152e5` and `elements: 2.56e2`. Existing field
ranges still apply; fractions, numeric strings and overflow are rejected without
rounding. Sample values retain their original JSON numeric precision.

`every_ms` defaults to 100 and sets the polling interval for camera stills, simulator,
Modbus, S7, ADS, PCCC, SLMP, FINS/TCP, FINS/UDP, ifm IO-Link, IO-Link JSON, SCPI, Pfeiffer TPG
and native EtherNet/IP sources, from 1 to 3,600,000 milliseconds. Missed polls are skipped rather than
queued for later. Video, MQTT, JSONL, CAN, passive fieldbus and helper sources are read continuously; `every_ms` does not
pace their event streams. `timeout_ms` defaults to 1,000 and must be between 1 and
30,000 milliseconds. It bounds a read, including connection work; still-image delivery has its stated
five-second grace, and video finalization has its separate 30-second budget. A Modbus, S7, ADS, PCCC, SLMP, MC 1E, FINS or Host Link read
must obtain all configured signals within that budget; an idle MQTT, JSONL or helper
read can return no samples and continue waiting.

Each source runs independently so a slow source does not hold up acquisition from
its peers. A source error stops the recording and preserves completed rows. The
writer has a bounded queue; if it falls behind, recording fails explicitly rather
than silently dropping samples. Output uses Capturefab's storage free-space checks.

## Modbus TCP, RTU and ASCII

This configuration reads four scaled holding registers and a discrete input from
a Modbus TCP device. Replace the address and signal definitions with the device's
register map:

```json
{
  "version": 1,
  "sources": [{
    "id": "press",
    "protocol": "modbus_tcp",
    "address": "192.0.2.20:502",
    "unit": 1,
    "every_ms": 100,
    "timeout_ms": 1000,
    "signals": [
      {"name": "pressure_bar", "table": "holding", "address": 0, "data_type": "u16", "elements": 4, "scale": 0.01},
      {"name": "part_present", "table": "discrete", "address": 7, "data_type": "bool"}
    ]
  }]
}
```

Addresses are **zero-based PDU addresses**, from 0 to 65535. For a device manual
whose first holding register is numbered 40001, that first register is address 0.
TCP `address` requires a numeric IP and port, such as `192.0.2.20:502` or
`[::1]:502`; hostnames are not resolved. `unit` defaults to 1. A signal can
override it to read a different station through the same TCP or TLS connection.
Both TCP settings accept 0–255. Use the gateway's configured station IDs, which
are usually 1–247 for downstream RTU. The MBAP response unit must match the
request, and all signals share the poll deadline.

For example, an AutomationDirect MB-GATEWAY can route a holding-register read
to unit 5 at address 10 and serve its local Automatic Read cache through unit 255.
Configure the cache entries on the gateway. The cache's refresh interval governs
the age of its values. [MB-GATEWAY manual, chapter 5](https://cdn.automationdirect.com/static/manuals/mbgatewaym/ch5.pdf)

```json
{
  "id": "gateway",
  "protocol": "modbus_tcp",
  "address": "192.0.2.20:502",
  "signals": [
    {"name":"station_five","unit":5,"address":10},
    {"name":"gateway_cache","unit":255,"address":0,"elements":2}
  ]
}
```

For a TLS-enabled Modbus TCP endpoint, set its configured port, typically 802,
and add a `tls` object to the source:

```json
{
  "address": "192.0.2.20:802",
  "tls": {
    "server_name": "meter.plant.example",
    "ca_certificate": "plant-ca.pem",
    "client_certificate": "reader-chain.pem",
    "client_key": "reader-key.pem"
  }
}
```

`server_name` must match the server certificate's DNS or IP identity. If omitted,
`ca_certificate` defaults to the bundled public trust roots. Set it to a PEM CA
file to use your plant trust roots. For a server that requires mutual TLS, supply
both the PEM client chain and its matching unencrypted PEM private key. Use a
certificate provisioned with the required role if the server uses
certificate-based authorization.

Modbus TCP over TLS requires the `mqtt-tls` build feature. It uses standard
Modbus TCP messages inside verified TLS 1.2 or later, with connection setup,
the handshake, and all replies sharing the poll's `timeout_ms`. The endpoint
must accept standard TLS record sizes.

The KOSTAL Smart Energy Meter G1/G2 and Advantech ICR-OS MODBUS-TCP2RTU
document TLS on port 802. Configure the meter or gateway to trust the reader
certificate when it requires client authentication. Use the device's
published register map.
[KOSTAL interface description](https://www.kostal-solar-electric.com/download/schnittstellenprotokolle/ba_kostal_interface_ksem_en.pdf) ·
[Advantech gateway documentation](https://icr.advantech.com/docs/router-apps/protocol-modbus-tcp2rtu.html)

For RTU, replace the source's `protocol` and TCP `address` with these options:

```json
{
  "id": "press",
  "protocol": "modbus_rtu",
  "device": "/dev/ttyUSB0",
  "baud": 19200,
  "parity": "even",
  "stop_bits": 1,
  "unit": 1,
  "signals": [{"name": "pressure_bar", "address": 0, "scale": 0.01}]
}
```

RTU uses a serial device the caller can open: a Unix terminal path or a Windows
COM port, such as `"device":"COM12"`. Supported baud rates
are 300, 600, 1200, 2400, 4800, 9600, 19200, 38400, 57600 and 115200. Defaults are 19200 baud,
8 data bits, even parity and one stop bit. `parity` accepts `none`, `even` or `odd`;
`stop_bits` accepts 1 or 2.

Use `modbus_ascii` for a port configured for hexadecimal ASCII frames with
standard CRLF termination and LRC. The defaults are 9600 baud, 7 data bits,
even parity and one stop bit, matching the documented Delta DVP serial
settings. Set `data_bits` to 7 or 8 for the configured device. Delta's
DVP02TK-S temperature controller also supports ASCII, with device-specific
register and quantity limits. [Modbus serial line guide](https://www.modbus.org/file/secure/modbusoverserial.pdf) ·
[Delta DVP02TK-S manual](https://filecenter.deltaww.com/Products/download/06/060301/Manual/DELTA_IA-PLC_DVP02TU-S_DVP02TK-S_OM_EN_20230425.pdf)

For RTU and ASCII, give one source exclusive use of each terminal. All stations
on that bus must use the same framing and UART settings. The source's default
`unit` is 1, and a signal can set its own `unit` from 1 through 247. A single
poll reads those stations in signal order under one `timeout_ms`. If any
response fails, that poll publishes no samples. Use an RS-485 adapter that
handles line direction and has local echo disabled.

```json
{
  "id": "plc_bus",
  "protocol": "modbus_ascii",
  "device": "/dev/ttyUSB0",
  "unit": 1,
  "signals": [
    {"name": "line_count", "address": 4196, "data_type": "u32"},
    {"name": "spare_count", "unit": 2, "address": 4196, "data_type": "u32"}
  ]
}
```

The example reads D100–D101 at wire address 4196 on two configured Delta DVP
stations. Set word order according to the PLC program's value layout.

Every Modbus source needs 1–256 signals with unique names:

| Signal option | Meaning |
| --- | --- |
| `name` | Output signal name |
| `unit` | Optional TCP gateway unit (0–255) or RTU/ASCII station (1–247) override |
| `address` | Zero-based address of the first register or bit |
| `table` | `holding` (default), `input`, `coil` or `discrete` |
| `data_type` | `u16` (default), `i16`, `u32`, `i32`, `u64`, `i64`, `f32`, `f64` or `bool` |
| `elements` | Number of consecutive typed values in one read. Defaults to 1. |
| `word_order` | `big` (default) or `little` for most- or least-significant 16-bit register first |
| `byte_order` | `big` (default) or `little` for most- or least-significant byte first within each register |
| `scale`, `offset` | Numeric output is `decoded_value * scale + offset`; defaults 1 and 0 |

Coil and discrete signals require `bool`; holding and input registers require a
numeric type. An `elements` count of 1 returns a scalar. Larger counts return an
array in ascending address order. A read can include up to 2,000 coils or discrete
inputs, or 125 holding or input registers. Each 32-bit element uses two registers,
and each 64-bit element uses four. That allows up to 62 or 31 elements respectively.
The last address must fit in 0–65535, and the range must be readable on the device.
Packed bit replies start at the least significant bit of the first byte, with
zero padding in unused high bits of the last byte.

For register arrays, word order, byte order and scaling apply to each element.
Set `word_order` and `byte_order` from the device's register map. For example,
`0x12345678` uses bytes `12 34 56 78` with both orders `big`, `56 78 12 34` with
only `word_order: "little"`, `34 12 78 56` with only `byte_order: "little"`, and
`78 56 34 12` when both are `little`. Byte swapping changes the device value
layout; the Modbus request and response framing retains its standard byte order.

Boolean and 64-bit integer signals cannot be scaled. The full signed and unsigned
64-bit integer ranges are preserved as exact JSON integers. Use `f64` when the
device stores an IEEE 754 double, including when a floating point value requires
scaling. Non-finite floating point samples or results fail explicitly. Acquisition
sends read functions 1–4. All signals in a poll share one `timeout_ms` deadline.
The poll is published after all signals validate and fit the 1 MiB serialized
batch limit. An error discards that poll, including earlier signals.

Read quantities and bit packing follow sections 6.1–6.4 of the
[Modbus application protocol](https://www.modbus.org/file/secure/modbusprotocolspecification.pdf).
RTU frame sizes and CRC follow section 2.5.1 of the
[Modbus serial line guide](https://www.modbus.org/file/secure/modbusoverserial.pdf).

## Siemens S7 absolute reads

The native `s7` adapter uses classic S7 ReadVar over ISO-on-TCP. It reads fixed
byte/bit addresses from data blocks, inputs, outputs and merker memory:

```json
{
  "version": 1,
  "sources": [{
    "id": "press",
    "protocol": "s7",
    "address": "192.0.2.50:102",
    "rack": 0,
    "slot": 2,
    "every_ms": 100,
    "timeout_ms": 1000,
    "signals": [
      {"name":"pressure_bar", "area":"db", "db":1, "address":0, "data_type":"f32"},
      {"name":"part_present", "area":"input", "address":2, "data_type":"bool", "bit":3}
    ]
  }]
}
```

`address` at the source level is a numeric IP and port; 102 is the usual
ISO-on-TCP port. `rack` defaults to 0 and `slot` to 2. Match them to the CPU's
actual connection settings. Optional `local_tsap` and `remote_tsap` select explicit
16-bit transport service access points instead of the computed defaults. They
must be supplied together; `rack` is 0–7 and `slot` is 0–31.

Each source accepts 1–256 uniquely named signals. A signal's `address` is a
zero-based **byte** offset; `bit` selects 0–7 for `bool`. `area` is `db` (default),
`input`, `output` or `merker`. `db` selects data block 1–65535 and is zero or omitted for
non-DB areas. Supported `data_type` values are `bool`, `u8`, `i8`, `u16` (default),
`i16`, `u32`, `i32`, `u64`, `i64`, `f32`, `f64`, `s5time`, `date`, `time`,
`tod` (alias `time_of_day`), `date_and_time` (alias `dt`), `dtl`, `ltime`,
`ltod` (alias `ltime_of_day`), `ldt` (alias `date_and_ltime`), `string` and `wstring`.
Multi-byte numeric values use big endian S7 representation. Optional numeric `scale` and `offset` use the same formula as
Modbus. Booleans and full-width 64-bit integers cannot be scaled; the integers
remain exact. Non-finite floating point values/results are rejected.

`elements` defaults to 1 and accepts 1–65535. More than one records an ordered
JSON array from configured contiguous storage. Numeric addresses advance by the
type width. BOOL advances from the configured byte/bit through bits 0–7 and
then the next byte; unselected adjacent bits are ignored. For example:

```json
[
  {"name":"temperatures","db":1,"address":128,"data_type":"f32","elements":16},
  {"name":"alarms","db":1,"address":256,"data_type":"bool","bit":6,"elements":20},
  {"name":"counts","db":1,"address":512,"data_type":"u64","elements":8},
  {"name":"timers","area":"timer","address":7,"data_type":"s5time","elements":4}
]
```

One element retains the scalar output. STRING/WSTRING require one element.
Indexed timer/counter addresses and counts select successive timer/counter
numbers. Scaling applies separately to each numeric element with the existing
exact-type restrictions. Storage is limited to 131072 bytes per signal; the
shared 1 MiB serialized poll budget can allow less. Reads split across the
negotiated PDU under the original deadline, retaining complete indexed T/C words
even for an odd PDU size. A malformed or rejected wire item, nonfinite numeric
value, exhausted budget or deadline discards the entire poll before any later
signal is queried. Invalid temporal application values retain their raw observation
as described below.

Configure the actual standard/nonoptimized absolute layout and readable range.
This does not discover symbols, dimensions or strides, and multiple requests
can span PLC scans. Optimized S7-1500 BOOL arrays can use byte-sized elements;
the packed BOOL profile here must not be used for that layout.
[Siemens ARRAY reference](https://docs.tia.siemens.cloud/r/en-us/v20/data-types/array/basic-information-on-array) ·
[Standard-access BOOL overlay, Easy Book p.68](https://cache.industry.siemens.com/dl/files/145/39710145/att_5787/v1/s71200_easy_book_en-US_en-US.pdf).

Set `area: "timer"` or `area: "counter"` on S7-300/400 classic indexed memory.
Here the signal's `address` is the timer or counter number, so T7 uses
`"address": 7`. Use the range supported by the CPU. Omit `db` and `bit`.
The default `u16` captures the returned word. For a timer value in milliseconds,
set `"data_type": "s5time"`. That type also decodes a configured S5TIME word at
a byte address in DB or other byte-addressed memory. It uses the documented
time base and three BCD digits, rejects invalid digits, and requires default
scale and offset. [Siemens S5TIME reference](https://docs.tia.siemens.cloud/r/en-us/v20/data-types/timers/s5time-duration-s7-300-s7-400)

For temporal storage, choose the actual declared fixed layout:

| `data_type` | Bytes per element | Decoded value |
| --- | --- | --- |
| `date` | 2 | Exact unsigned `days_since_1990` and civil `date` |
| `time` | 4 | Exact signed `milliseconds`, `unit: "ms"`, `semantic: "duration"` |
| `tod` / `time_of_day` | 4 | Exact unsigned `milliseconds` since midnight and valid `time_of_day` |
| `date_and_time` / `dt` | 8 | BCD civil fields, with three fractional second digits |
| `dtl` | 12 | Binary civil fields, with nine fractional second digits |
| `ltime` | 8 | Exact signed `nanoseconds`, `unit: "ns"`, `semantic: "duration"` |
| `ltod` / `ltime_of_day` | 8 | Exact unsigned `nanoseconds` since midnight and valid `time_of_day` |
| `ldt` / `date_and_ltime` | 8 | Exact unsigned `nanoseconds_since_1970` and valid civil `calendar` |

```json
{"name":"event_times","db":1,"address":100,"data_type":"dtl","elements":4}
```

Each element is an object with `kind: "s7_temporal"`, canonical `data_type`,
`decode_status`, and exact complete bytes in `raw: {encoding,bytes,data}` using
base64. One element records that object; multiple elements record an ordered array.
Temporal types require default scale/offset and no bit or text options. They can
read configured byte-addressed DB/input/output/merker storage; indexed timer and
counter areas retain their separate word layouts. The configured type and readable
range must match the project. Reads can span scans and do not discover declared types.

TOD, DT, DTL, LTOD and LDT carry `clock_basis: "unspecified"`. Valid DT/DTL values include
numeric `fields` and an unzoned `calendar` string. Fractions remain exact, including
DTL nanoseconds beyond the precision of ordinary date-time libraries. These bytes
do not establish UTC, a timezone or DST, and do not populate `source_time`.
Siemens system-clock and local-clock instructions can both write these layouts.

Malformed BCD, impossible dates, out-of-range TOD/LTOD/LDT counts or component fields produce
`decode_status: "invalid_value"` with bounded `invalid_fields` and the original raw
bytes. Validly decoded component fields remain available; malformed BCD omits them.
No formatted calendar is emitted for an invalid value. Weekdays use 1–7, Sunday
through Saturday. A valid-range weekday that disagrees with the date is reported
as `weekday_matches_date: false` while retaining the calendar. Protocol, extent,
budget and deadline failures still discard the complete poll.

DT uses the documented 1990–2089 BCD year pivot. The DTL decoder classifies years
against the documented 1970–2554 structure profile; other years retain raw bytes
and fields as invalid observations. `within_documented_year_range` records that
profile check, not universal CPU validity. `within_recommended_conversion_range`
separately checks the recommended endpoint 2262-04-11T23:47:16.854775807. Valid
later values within the profile remain decoded with that flag false. CPU, editor
and project limits may be narrower. DATE retains the full 16-bit day count through
2169-06-06, even though older controller/editor documentation has a narrower range.

The three long types use eight-byte nanosecond counts. LTIME preserves the full
signed 64-bit duration range, including its asymmetric minimum. LTOD preserves an
unsigned count and formats values below 86400000000000 as `HH:MM:SS.nnnnnnnnn`.
LDT preserves an unsigned count from the civil origin 1970-01-01 00:00:00 and
formats values through 9223372036854775807 as an unzoned calendar, ending at
2262-04-11T23:47:16.854775807. Larger LTOD/LDT patterns retain their positive exact
counts and raw bytes as invalid observations, without wrapping or inventing
negative dates. LDT's endpoint is its documented scalar range; DTL's separate
conversion recommendation does not extend it. LTOD and LDT keep all nine
fractional digits and do not populate `source_time`.

STEP 7 V21 documents these long types on S7-1500 and S7-1200 G2. Their availability
does not establish classic read access, a declared type at the configured address,
or a native long-type declaration on classic S7-1200/300/400 CPUs. The existing
standard-memory and access-permission requirements still apply. See Siemens'
[LTIME](https://docs.tia.siemens.cloud/r/en-us/v21/data-types/timers/ltime-iec-time-s7-1500-s7-1200-g2),
[LTOD](https://docs.tia.siemens.cloud/r/en-us/v20/data-types/date-and-time/ltod-ltime_of_day-s7-1500),
[LDT](https://docs.tia.siemens.cloud/r/en-us/v21/data-types/date-and-time/ldt-date_and_ltime-s7-1500-s7-1200-g2?contentId=oCICtfsNsZkx8NWzouaYhg),
[signed/unsigned ODK type mapping](https://docs.tia.siemens.cloud/r/simatic_s7_1500_et_200mp_manual_collection_enus_21/comprehensive-information/cpu-function-manuals/open-development-kit-1500s-v2.5-sp4/syntax-interface-file-project-.odk-for-cpu-function-libraries/data-types/syntax-interface-data-types?contentId=z3HjN1vu_siFXi6EzYrD9g)
and [type availability](https://docs.tia.siemens.cloud/r/en-us/v21/data-types/overview-of-the-valid-data-types).

See Siemens' [DATE](https://docs.tia.siemens.cloud/r/en-us/v21/data-types/date-and-time/date),
[DATE_AND_TIME](https://docs.tia.siemens.cloud/r/en-us/v20/data-types/date-and-time/date_and_time-date-and-time-of-day),
[DTL](https://docs.tia.siemens.cloud/r/en-us/v20/data-types/date-and-time/dtl-s7-1200-s7-1500),
[time/date storage and conversion recommendation](https://docs.tia.siemens.cloud/r/simatic_s7_1200_manual_collection_eses_20/plc-concepts/data-types/time-and-date-data-types?contentId=o22lRYJ1hiMTDUaVMNlQsg)
and [HMI DTL structure](https://docs.tia.siemens.cloud/r/en-us/v20/communicating-with-plcs-basic-panels-panels-comfort-panels-rt-advanced-rt-professional/communicating-with-simatic-s7-1200-basic-panels-panels-comfort-panels-rt-advanced-rt-professional/data-exchange-basic-panels-panels-comfort-panels-rt-advanced-rt-professional/data-exchange-using-area-pointers-basic-panels-panels-comfort-panels-rt-advanced/date/time-plc-area-pointer-basic-panels-panels-comfort-panels-rt-advanced).

For text, use an absolute DB signal and supply the declared `capacity`, excluding
its header. A `string` capacity is 0–254 bytes; a `wstring` capacity is 0–65534
16-bit WCHAR units (Capturefab's bounded supported range). The actual permitted
capacity depends on the CPU and project; for example, STEP 7 V21 documents a
smaller maximum of 16382 for its WSTRING declarations. These options do not
apply to numeric signals. Text requires `bit: 0`, `scale: 1` and `offset: 0`,
which are already their defaults. For example:

```json
[
  {"name":"lot", "db":1, "address":32, "data_type":"string", "capacity":10, "encoding":"ascii"},
  {"name":"description", "db":1, "address":64, "data_type":"wstring", "capacity":300}
]
```

Siemens STRING begins with its maximum and current byte lengths, followed by
single-byte data in the configured Windows codepage. Select `encoding: "ascii"`,
`"utf8"` or `"windows1252"` explicitly when that matches the PLC application;
omitting it captures raw bytes with `decode_status: "raw_only"`. Capturefab does
not guess the Windows codepage. UTF-8 is an explicit application choice, not an
assumption about Siemens STRING. CP1252 decoding uses the strict Microsoft table;
its undefined bytes 81, 8D, 8F, 90 and 9D (hex) produce `invalid_encoding`.

WSTRING has big endian 16-bit maximum/current lengths followed by UTF-16BE data.
Its output encoding is always `utf16be`; it does not accept an `encoding` option.
The decoder preserves invalid UTF-16 as raw data with `invalid_encoding` rather
than inserting replacement characters. Siemens documents WSTRING for supporting
S7-1200/1500 CPUs; successful UTF-16 decoding does not imply that a CPU/editor
accepts every Unicode character. The current `length` counts bytes for STRING
and 16-bit units for WSTRING, rather than decoded Unicode characters. Empty text
and embedded NUL/control characters are preserved without trimming.

Each text value is an object with `kind: "s7_text"`, `data_type`, `capacity`,
`length`, `storage_bytes`, `decode_status` and `raw`. When decoded, it also has
`text`; an explicit or fixed encoding is recorded as `encoding`, otherwise that
field is omitted. `raw` contains `encoding: "base64"`, `scope:
"header_and_active"`, its exact byte count in `bytes`, and base64 `data`. These
bytes include the length header and active characters. Unused slot tails are
undefined and are excluded from the recorded raw bytes and text. For example,
STRING[10] containing ABC yields:

```json
{"kind":"s7_text","data_type":"string","capacity":10,"length":3,"storage_bytes":12,"encoding":"ascii","decode_status":"decoded","raw":{"encoding":"base64","scope":"header_and_active","bytes":5,"data":"CgNBQkM="},"text":"ABC"}
```

The returned maximum must equal `capacity`, and the current length cannot exceed
it. Capturefab reads the complete declared slot (up to 131072 bytes), splitting
byte reads to fit the negotiated S7 PDU. A 240-byte PDU admits 222 data bytes per
read. For a segmented text slot it rereads the header once and fails if either
length changed. Same-length content changes can still span scans. Header,
transport or later-signal failures publish no part of the poll; encoding failures
retain an explicitly tagged raw reading. Serialized poll values are limited to
1 MiB incrementally, before a later signal is queried.

The layouts follow Siemens' [STRING structure](https://docs.tia.siemens.cloud/r/en-us/v21/data-types/character-strings/character-strings/structure-of-a-string-tag)
and [WSTRING/WCHAR documentation](https://docs.tia.siemens.cloud/r/en-us/v21/data-types/character-strings/character-strings/wstring-s7-1200-s7-1500-s7-1200-g2),
and the [S7-1200 character/string storage limits](https://support.industry.siemens.com/cs/attachments/109977302/s71200_system_manual_en-US.pdf)
(Table 6-37); the byte interpretation is cross-checked against the maintained
[python-snap7 getters](https://github.com/gijzelaerr/python-snap7/blob/master/snap7/util/getters.py).
Protocol fixtures cover the text requests and exports; this is not a claim of
physical-device validation.

Each poll reads every configured signal within one deadline, including TCP,
transport connection and PDU negotiation. A poll is published only after all
signals succeed. That does not establish an atomic PLC scan snapshot: separate
reads can observe different scans. The adapter sends no WriteVar, CPU control or
program-download requests. It does not implement S7comm Plus, symbolic/optimized
DB access, PLC sign-in or transport encryption.

The CPU must already expose the classic absolute-read service and fixed DB
layout. For S7-1200/1500, this depends on the engineered access settings;
optimized remote DBs cannot be read by this path. See the
[Siemens GET/PUT documentation](https://docs.tia.siemens.cloud/r/simatic_s7_1200_manual_collection_enus_20/communication/s7-communication/get-and-put-read-and-write-from-a-remote-cpu).
Capturefab does not change those CPU settings. OPC-UA is a separate acquisition
path when the controller exposes the desired values there.

## Mitsubishi MELSEC MC/SLMP reads

The native `slmp` adapter implements Mitsubishi MC/SLMP 3E device reads over
TCP. Select `encoding: "binary"` (default) or `"ascii"` to match the PLC's
configured communication data code. Use the TCP port, routing and device
ranges from the PLC project and Ethernet module manual.

```json
{
  "version": 1,
  "sources": [{
    "id": "melsec",
    "protocol": "slmp",
    "address": "192.0.2.70:5000",
    "encoding": "binary",
    "device_format": "standard",
    "network": 0,
    "station": 255,
    "module_io": 1023,
    "multidrop": 0,
    "monitoring_timer": 4,
    "every_ms": 100,
    "signals": [
      {"name": "pressure", "device": "D", "address": 100, "data_type": "i16", "scale": 0.01},
      {"name": "cycles", "device": "D", "address": 200, "data_type": "u64"},
      {"name": "present", "device": "M", "address": 10, "data_type": "bool"}
    ]
  }]
}
```

`address` requires a numeric IP and explicit TCP port. The route defaults to the
connected CPU: `network: 0`, `station: 255`, `module_io: 1023` (`0x03ff`),
`multidrop: 0`. Other routes require the engineered network/station/module I/O
numbers. `network` accepts 0–239, `station` 1–120, 125, 126 or 255, and `multidrop`
0–31. `monitoring_timer` defaults to 4 and is a nonzero PLC-side waiting period in
250 ms units; it does not extend the host's `timeout_ms` deadline.

`device_format: "standard"` uses the Q/L-compatible device format: a 24-bit device
number and one-byte binary device code with subcommands 0000/0001. In ASCII,
it uses a two-character device code and six address digits. `"iq_r"` uses
32-bit numbers and two-byte binary codes with subcommands 0002/0003, or
four-character codes and eight address digits in ASCII. Select the format
supported by the target. The module may impose smaller ranges.
[Mitsubishi SLMP reference, §§4.1–4.2 and 5.2](https://dl.mitsubishielectric.com/dl/fa/document/manual/plc/sh080956eng/sh080956engn.pdf)

Each source accepts 1–256 uniquely named signals. Word devices are `D`, `W`, `R`,
`ZR`, `SD`, `SW`, `TN`, `CN`, `STN` and `Z`. Bit devices are `M`, `X`, `Y`, `L`, `F`, `V`,
`B`, `SM`, `SB`, `S`, `TS`, `TC`, `STS`, `STC`, `CS` and `CC`. The signal's `address` is a numeric
device number. Convert hexadecimal project notation to its numeric value: X100
in hexadecimal uses `"device":"X","address":256`. D100 uses address 100.
ASCII formats X, Y, B, W, SB, SW and ZR addresses as hexadecimal and the other
supported devices as decimal. A decimal range must fit the encoding's six or
eight digits. The full configured range must exist in the target project.

Bit devices require `data_type: "bool"`. Word devices accept `u16` (default),
`i16`, `u32`, `i32`, `u64`, `i64`, `f32` and `f64`. Adjacent 16-bit words use
low-word-first order. Binary sends each word little-endian. ASCII sends each
word as four hexadecimal characters. `elements` defaults to 1 and selects
consecutive typed values. More than one produces an array. A read is limited
to 960 words, including all words of a wide type. The bit limit is 7168 in
binary and 3584 in ASCII. Binary bit data is high-nibble first. ASCII bit data
is one `0` or `1` per point.
Integers preserve their full precision. Optional `scale` and `offset` apply
`value * scale + offset`; booleans and 64-bit integers cannot be scaled.
Non-finite values fail explicitly.

Step relay `S` and retentive contact/coil/current devices `STS`/`STC`/`STN`
use their canonical names in configuration. Standard ASCII encodes the retentive
names as `SS`/`SC`/`SN`; extended ASCII uses `STS*`/`STC*`/`STN*`.

For whole long timers, select `device_format: "iq_r"` and explicit
`data_type: "long_timer"` on `LTN` or `LSTN`:

```json
{"version":1,"sources":[{"id":"timers","protocol":"slmp",
  "address":"192.0.2.70:5000","device_format":"iq_r","encoding":"binary",
  "signals":[
    {"name":"step","device":"S","address":10,"data_type":"bool"},
    {"name":"retentive_contact","device":"STS","address":0,"data_type":"bool"},
    {"name":"long_timers","device":"LTN","address":0,"data_type":"long_timer","elements":2}
  ]}]}
```

`elements` is 1–240 logical timers. The request asks for four words per timer;
the response retains each complete four-word record as `raw_words`,
`current_word_pair_raw`, `coil` and `contact`. System words and unknown status
bits remain intact. One timer produces an object, multiple timers an ordered
array. Generic numeric reinterpretation and nonidentity scaling or offsets are refused for
this structured type. The current-word pair stays in wire order; its assembly
into an integer, signedness, duration units and timebase remain unqualified. Address ranges advance by logical timers,
independently of the four-word request count.

The format selector describes the wire layout and does not identify a CPU.
Use devices allocated by the actual project; the FX5 manual marks long timers
unavailable. A controller rejection preserves its native end code. Standalone
`LTS`/`LTC`/`LSTS`/`LSTC` and `LZ` are not admitted by command 0401.
[Mitsubishi SLMP reference, §§4.2 and 5.2](https://dl.mitsubishielectric.com/dl/fa/document/manual/plc/sh080956eng/sh080956engn.pdf),
[FX5 Ethernet manual, device table](https://www.mitsubishielectric.com/dl/fa/document/manual/plcf/jy997d56201/jy997d56201u.pdf).

Capturefab sends only Device Read command `0x0401`; it sends no write, remote
run/stop/reset, password or project services. TCP connection and every signal in
one poll share `timeout_ms`. Responses must have matching 3E routing, valid
bounded lengths and a successful end code. A PLC rejection reports
`slmp_end_code` and the hex bytes of any error information. A poll's serialized
readings are limited to 1 MiB and returned only after every selected read succeeds. A failed poll
closes and retires the TCP connection, with no automatic reconnect. The poll uses
the shared host read-completion time. Structured long-timer batches also reserve
the minimum remaining JSON size before issuing later reads.
UDP, 4E, label access, device extensions and automatic type discovery are
outside this adapter's scope.

## Mitsubishi MELSEC-F MC 1E reads

Use `mc_1e` for A-compatible 1E reads from a configured FX3U-ENET or
FX3U-ENET-ADP Ethernet module. Match the module's TCP endpoint and code format.
Set `encoding` to `binary` (default) or `ascii`. `monitoring_timer` defaults to
4 in units of 250 ms. A value of 0 leaves the PLC's monitoring timer unlimited.
Capturefab still applies its own `timeout_ms` to the entire poll.

```json
{
  "version": 1,
  "sources": [{
    "id": "fx",
    "protocol": "mc_1e",
    "address": "192.0.2.45:5000",
    "encoding": "ascii",
    "monitoring_timer": 10,
    "signals": [
      {"name": "words", "device": "D", "address": 0, "elements": 5},
      {"name": "inputs", "device": "X", "address": 15, "data_type": "bool", "elements": 3}
    ]
  }]
}
```

Each source accepts 1–256 uniquely named signals. Choose `D`, `R`, `TN`, `CN`,
`X`, `Y`, `M`, `S`, `TS` or `CS` from the CPU's device map. Convert FX `X` and
`Y` labels from octal to numeric addresses. `elements` defaults to 1 and larger
counts return arrays. Numeric types are `u16` (default), `i16`, `u32`, `i32`,
`u64`, `i64`, `f32` and `f64`. Word values use the protocol's low-word-first
order. Finite `scale` and `offset` apply to numeric values through 32-bit
integers and floating-point types. Booleans and 64-bit integers retain their
exact values.

A `bool` signal reads up to 256 bits from a bit device. Numeric reads of bit
devices start at an address divisible by 16 and read up to 32 word points.
Numeric word-device reads use up to 64 words per request. `CN` reads use
16-bit integer values below C200 and 32-bit integer values from C200 through
C255. Keep the entire signal inside the applicable device range. The installed
CPU and module may have lower limits.

The adapter sends only local station `FF` batch bit and word reads. Each TCP
connection has one outstanding request at a time. A failed request closes the
connection, and an incomplete or invalid poll publishes no samples. The two
encodings share the same typed decode and poll limits. Consult the module
manual for CPU and firmware compatibility and the configured MC port.
[FX3U-ENET-ADP manual](https://dl.mitsubishielectric.com/dl/fa/document/manual/plc_fx/jy997d45801/jy997d45801h.pdf)
· [FX3U-ENET manual](https://dl.mitsubishielectric.com/dl/fa/document/manual/plc_fx/jy997d18101/jy997d18101h.pdf)

## Mitsubishi FX Computer Link serial reads

Use `fx_computer_link` on a MELSEC-F CPU with a compatible serial board or
adapter configured for Computer Link. Examples include FX3U-232-BD and
FX3U-485-BD on supported FX3U models. Use a Unix terminal or Windows COM port
connected by RS-232 or an automatic-direction RS-485 interface without local
echo. Give one source exclusive use of the port and turn off unsolicited
output. Select the CPU's actual baud, parity, data bits, stop bits, protocol
format and sum-check setting.

```json
{
  "version": 1,
  "sources": [{
    "id": "fx_serial",
    "protocol": "fx_computer_link",
    "device": "/dev/ttyUSB0",
    "baud": 9600,
    "data_bits": 7,
    "parity": "even",
    "stop_bits": 1,
    "format": "format_4",
    "sum_check": true,
    "unit": 5,
    "message_wait_ms": 70,
    "inter_frame_ms": 2,
    "ack_delay_ms": 150,
    "every_ms": 1000,
    "timeout_ms": 900,
    "signals": [
      {"name": "inputs", "device": "X", "address": 32, "data_type": "bool", "elements": 5},
      {"name": "count", "device": "D", "address": 100, "data_type": "u32"},
      {"name": "timer", "unit": 6, "device": "TN", "address": 123}
    ]
  }]
}
```

Serial defaults are 9600 baud, 7 data bits, even parity and one stop bit.
Check the CPU firmware before using 38400 baud. Mitsubishi lists this rate
for FX3U/FX3UC from version 2.41 and for FX3S/FX3G/FX3GC. `format` defaults to `format_1`. Use `format_4` for the CRLF form. `sum_check`
defaults to true. The source's `unit` defaults to 0. A signal may override it
with its own station in 0–15. The PLC number field is the direct `FF` value.
`message_wait_ms` defaults to 0 and encodes the PLC's reply delay in 10 ms
steps up to 150. For Mitsubishi's one-pair 1:N converter wiring, configure
at least 70 ms and above the maximum PLC scan time. `inter_frame_ms`
defaults to 1, with an allowed range of 1–1000 ms. Set it to meet the
hardware's turnaround and the CPU's minimum interval after ACK. Older
FX2/FX2C, FX0N, FX1S and FX1N/FX1NC CPUs require at least two scan times.
`ack_delay_ms` is the response-to-ACK interval. It defaults to
`inter_frame_ms` and accepts 1–30000 ms. Mitsubishi specifies at least
one scan before ACK. For 1:N wiring, use more than the configured PLC
inter-character timeout (D8129 for channel 1) plus one scan. The example's
150 ms assumes that sum is below 150 ms. Size `timeout_ms` for wire time,
PLC response time and these intervals across all reads in a poll.

Each source accepts 1–256 uniquely named signals. `BR` reads `X`, `Y`, `M`
and `S` as booleans. Convert `X` and `Y` labels from octal to numeric
addresses. The example's `address: 32` starts at `X0040`. `WR` reads `D`
and `TN`. `QR` reads `R` only on models with a documented file-register
mapping. Mitsubishi lists R0–R23999 for FX3G/FX3GC and R0–R32767 for
FX3U/FX3UC. Use the CPU's actual device
ranges and keep each signal within one contiguous normal or special
device range. `TN` returns 16-bit integer current values. `D` and `R`
use `u16` (default), `i16`, `u32`, `i32`, `u64`, `i64`, `f32`
or `f64`. Multiword values are low-word first. Finite `scale` and
`offset` apply to numeric values through 32-bit integers and floating-point
types. Booleans and 64-bit integers retain their exact values.
`elements` defaults to 1 and larger counts up to 4096 return arrays.

Capturefab segments signals to fit the request limits. `max_bits`
defaults to 256 and `max_words` to 64. For FX0N and FX1S use no more
than 54 bits and 13 words per request. A word request contains whole
typed values. Each validated read response receives the protocol's link ACK.
The same `timeout_ms` covers every request, response and ACK in a poll.
A PLC NAK reports `fx_error_code`. Any malformed or incomplete response
fails the entire poll, closes the serial link and publishes no samples.
The serialized readings in one poll are limited to 1 MiB. Segmented reads
can span several PLC scan cycles. Replies carry a station
and PLC number without a transaction ID, so use a dedicated bus master.
[Data Communication Edition, JY997D16901R, Computer Link §§6–8](https://dl.mitsubishielectric.com/dl/fa/document/manual/plc_fx/jy997d16901/jy997d16901r.pdf)

## FATEK FBs standard Port0 reads

Use `fatek_fbs` with the required `fbs_port0` profile for an externally configured
FBs standard-protocol connection. Native serial scope is a non-U-suffix FBs
Port0 RS232 interface using FATEK's FBs-232P0-9F-150 or FBs-232P0-9M-400 cable.
Match the configured baud; Port0 framing is fixed at 7 data bits, even parity
and one stop bit. The documented USB connector does not establish a virtual
COM driver for direct USB capture.

```json
{
  "version": 1,
  "sources": [{
    "id": "plc", "protocol": "fatek_fbs", "profile": "fbs_port0",
    "serial": {"device": "/dev/ttyUSB0", "baud": 9600},
    "unit": 1, "every_ms": 200, "timeout_ms": 500,
    "signals": [
      {"name": "registers", "address": "R00012", "elements": 3},
      {"name": "states", "address": "X0050", "data_type": "bool", "elements": 6},
      {"name": "signed_pair", "address": "DR00012", "data_type": "i32", "elements": 2},
      {"name": "contact_words", "address": "WX0000", "elements": 2}
    ]
  }]
}
```

Use exactly one `serial` or `address`. Serial defaults are 9600 baud and 7E1;
the other allowed rates are 19200, 38400, 57600 and 115200. An `address` is an
explicit numeric IP and nonzero port for a separately configured transparent
TCP bridge carrying this exact byte stream. Capturefab does not discover or
configure a bridge, assume an embedded network service or select a default
TCP port. The dedicated link must require no application flow/direction keying
and must have no echo, unsolicited output or competing clients.

Source `unit` defaults to 1; each signal can override it. Only unicast stations
1–254 are accepted. Use unique names and literal uppercase, zero-padded addresses:

| `data_type` | Address forms | Maximum `elements` |
| --- | --- | --- |
| `bool` | X/Y/M/S/T/C followed by 4 decimal digits | 256 |
| `u16` (default), `i16` | R/D/F + 5 digits, RT/RC + 4, or W + X/Y/M/S/T/C + 4 | 64 |
| `u32`, `i32` | DR/DD/DF + 5 digits, DRT/DRC + 4, or DW + X/Y/M/S/T/C + 4 | 32 |

`elements` defaults to 1, producing a scalar; larger counts return ordered
arrays of the server's native values. Signed types interpret the whole native
16/32-bit two's-complement value. Addresses are not converted to Modbus, aliased
or split into locally generated requests. W/DW combined views retain native
words without assigning labels to their individual bits or guessing the next
view address. X/Y/M/S combined-view starts must be multiples of 8. The protocol
table limits W starts to 9984 and DW starts to 9968; whole-run availability for
these views belongs to the server. Ordinary register/discrete reads also check
their documented protocol span. Actual CPU/project capacities may be smaller.

Only read commands 44 and 46 are sent. Station, command, error, exact length,
ASCII data and additive LRC are checked, with selected replies bounded to 265
bytes. One deadline covers lazy connection setup and all signals in a poll.
Any failed transaction permanently drops the owner and discards that poll's
staged values; earlier complete polls remain recoverable. There is no automatic
retry, hidden reopen, RUN/STOP, force, write or setup command. Replies have no
transaction ID/address echo, so correlation does not prove freshness or a PLC
snapshot. No units, floating-point scaling or device timestamp are inferred.

[FATEK FBs communication functions, chapter 12](https://file.fatek.com/en/PLC/Document/Manual_2_en/Chapter_12.pdf) ·
[FATEK standard protocol, Appendix 1](https://file.fatek.com/en/PLC/Document/Manual_2_en/Appendix1.pdf)

## LS ELECTRIC XGB Cnet serial reads

Use `ls_cnet` with the required `xgb_dedicated` profile for an XGB built-in or
XBL-C21A/C41A serial interface configured externally as an XGT dedicated server.
Select the actual CPU/project addresses and serial settings before recording.
The adapter sends read-only `rSS` requests with mandatory BCC checksums.

```json
{
  "version": 1,
  "sources": [{
    "id": "plc", "protocol": "ls_cnet", "profile": "xgb_dedicated",
    "device": "/dev/ttyUSB0", "baud": 9600, "parity": "none",
    "data_bits": 8, "stop_bits": 1, "unit": 1,
    "every_ms": 1000, "timeout_ms": 500,
    "signals": [
      {"name": "word", "variable": "%MW100"},
      {"name": "channels", "variables": ["%MW020", "%PW001"]},
      {"name": "flag", "unit": 2, "variable": "%MX00A", "data_type": "bool"},
      {"name": "wide", "variable": "%ML004", "data_type": "u64"}
    ]
  }]
}
```

Configured defaults are 9600 baud, 8N1 and station `unit: 1`; these are not
factory-setting claims. Allowed rates are 1200, 2400, 4800, 9600, 19200, 38400,
57600 and 115200. Data bits may be 7 or 8, stop bits 1 or 2, and parity `none`,
`even` or `odd`. A source or individual signal can select a station from 0–255.
Use a no-flow interface dedicated to one client. RS232 needs the appropriate
TX/RX/SG cable; RS422/485 needs a configured converter that controls direction
without application RTS/CTS keying. Windows accepts a configured COM path.

Each uniquely named signal has either `variable` for a scalar or `variables`
for an ordered array of 1–16 explicit addresses. A one-entry list still returns
an array. Non-contiguous and repeated addresses are allowed; no address
progression is inferred. `data_type` defaults to `u16` and also accepts `bool`,
`u8`, `i8`, `i16`, `u32`, `i32`, `u64` and `i64`. All addresses in a list must
match that native type: X for bool, B for 8 bits, W for 16, D for 32 and L for
64. Signed values use the corresponding native two's-complement width.

Direct names are 4–16 ASCII bytes, including `%`, area and native type. Areas
P/M/L/K/F/T/C/D/R/I/Q/W are admitted, with D limited to B/W and R requiring the
applicable XBC-H memory map. Address components retain their literal hex
digits; dots are accepted only for I/Q. The actual controller owns address
existence, capacities and module interpretation. Both complete request and
reply must fit the documented 256-byte frame, which further limits wide arrays.

One deadline covers lazy serial open and all queries in a poll. Only a complete
poll publishes samples; a malformed reply, NAK, partial timeout or cutoff
retires the connection permanently. Earlier complete polls remain recoverable.
Cnet has no transaction ID or address echo, and sequential reads do not establish
freshness or a PLC snapshot. `rSB`, writes, monitoring, setup, automatic address
generation and embedded TCP services are outside this profile.

[LS ELECTRIC XBL-C21A/C41A V2.0, chapter 7](https://sol.ls-electric.com/uploads/document/16411757089260/XBL-C21A_T9_Manual_V2.0_202012_EN.pdf)

## LS ELECTRIC XGT/XGB FEnet reads

Use `ls_xgt` with the XGT dedicated TCP server enabled in XG5000. The usual server port is 2004. This protocol addresses PLC memory directly; an existing Modbus memory map is not required. Configure the CPU, Ethernet module, dedicated service, permitted host table and available connection channels before capture. The adapter does not provision those settings.

```json
{
  "version": 1,
  "sources": [{
    "id": "line",
    "protocol": "ls_xgt",
    "address": "192.0.2.40:2004",
    "cpu": "xgk",
    "max_read_bytes": 256,
    "every_ms": 100,
    "timeout_ms": 1000,
    "signals": [
      {"name": "counter", "variable": "%DW100", "data_type": "u16"},
      {"name": "contacts", "variable": "%MX2764", "data_type": "bool"},
      {"name": "counts", "variable": "%DB202", "data_type": "u64", "elements": 4}
    ]
  }]
}
```

Supported CPU identifiers are `xgk` (default, A0), `xgi` (A4), `xgr` (A8), `xgb_mk` (B0) and `xgb_iec` (B4). This describes the documented XGT/XGB dedicated protocol, including XGL-EFMT(B)/EFMF(B)/EH5T and XBL-EMTA interfaces. Actual memory availability, permitted variable forms, sizes, firmware and server configuration depend on the CPU and project. `LGIS-GLOFA`, symbolic variables, routed services, UDP and writes are outside this adapter.

Each signal uses the literal 4–16 byte direct-variable string from its memory map. Upper- and lowercase ASCII letters are accepted and sent unchanged. Flat variables use the documented area letter, type `X/B/W/D/L` and decimal index. IEC CPU profiles also accept dotted I/Q `base.slot.point` and U `module.channel[.bit]` variables. The parser checks these forms and forbids control characters, other punctuation and arbitrary commands. It does not translate ladder bit addresses or guess a CPU's bit-number convention. For example, the XBL-EMTA manual expresses M172.C as `%MX2764`; send the documented address for the actual controller. Device D accepts byte/word forms; use its byte form for wider software decoding.

`data_type` is `bool`, `u8`, `i8`, `u16` (default), `i16`, `u32`, `i32`, `u64`, `i64`, `f32` or `f64`. Individual reads use the matching native width: X is one bool, B is one byte, W is 16 bits, D is 32 bits and L is 64 bits. Only the low bit of a native BIT reply is effective, as specified by the manufacturer. A flat B variable can also hold any numeric scalar or 1–4096 contiguous typed elements: these use continuous byte reads, advance the literal byte index, and decode little-endian values. For example D1234 is `%DW1234` as a word and `%DB2468` as bytes. Arrays must use a flat byte variable; dotted I/O variables remain individual scalars. One element remains a scalar, multiple elements produce a JSON array. `u64`/`i64` stay exact through recording and CSV/JSONL exports. Optional `scale`/`offset` default to 1/0; bool and 64-bit integers require identity scaling. Non-finite floating or scaled values fail explicitly.

`max_read_bytes` is 1–1400, defaults to 256 and must fit one configured value. The adapter splits larger arrays at whole-value boundaries. Lower it to the target's actual firmware/module limit; the maximum is documented for XGT FEnet and is not universal across compact XGB modules. The 16-byte address bound is a wire-format/software limit, not a claim that every CPU implements that full address range. Sequential reads, including fragments, are not a PLC snapshot.

Requests always include a header byte-sum. A response checksum of zero is accepted as the documented reserved/omitted form; every nonzero checksum must match. Set `require_header_checksum: true` to require a matching sum even when the response field is zero. Response company, direction, Invoke ID, instruction, type, block/data counts and exact bounded lengths are validated. Response PLC/CPU information, module position and application reserved bytes are server-owned metadata rather than request echoes; nonzero reserved bytes appear in the vendor's published response. Documented one-byte and two-byte NAK codes produce `failed` with exact `ls_xgt_status` and `ls_xgt_error_code` details.

Connection setup, every query, every fragment and final validation share one poll deadline. A failed poll closes the TCP connection. Invoke IDs are not reused on a connection; exhausting the 16-bit sequence opens a fresh connection within the same deadline. The serialized complete poll is limited to 1 MiB and is checked before the next signal is queried. No partial poll is published if a sibling fails.

Wire framing and literal replay coverage come from [LS ELECTRIC XBL-EMTA v2.1, §§5.2.3–5.2.6](https://sol.ls-electric.com/uploads/document/17743368081670/User_s%20Manual_XBL-EMTA_ENG_V2.1_20260324.pdf), [XGT FEnet v2.30, §§8.1–8.2 and 10.4](https://www.ls-electric.com/upload/customer/download/f8a96bfb-6212-4a0f-8d07-a1ba8868240f/User%27s%20Manual_XGT%20FEnet_V2.30.pdf) and [XEC U/UA/UP v1.2, §1.5.1](https://www.ls-electric.com/upload/customer/download/e062ad9a-91d9-4817-8510-319d795d48d1/Manual_XEC-DN%28R%29xxU%28UA%29%28UP%29_10310001407_Eng_V1.2_190822.pdf). These are protocol/fixture checks; physical-controller validation remains outstanding.


## Yokogawa FA-M3 higher-level PC Link

Use `yokogawa_fam3` with the higher-level PC Link TCP service configured on a supported F3LE01/F3LE11 Ethernet module. Select its actual binary or ASCII listener. The documented ports are 12289 and 12291, but SW1 selects which encoding uses each port, and the second listener is module-dependent. The adapter does not change the module configuration; a PLC communication program is not required. Module write protection can remain enabled.

```json
{
  "version": 1,
  "sources": [
    {
      "id": "line_plc",
      "protocol": "yokogawa_fam3",
      "address": "192.168.1.20:12291",
      "encoding": "binary",
      "cpu": 1,
      "every_ms": 100,
      "timeout_ms": 1000,
      "signals": [
        {
          "name": "temperature",
          "device": "D",
          "address": 123,
          "data_type": "i16",
          "scale": 0.1
        },
        {
          "name": "inputs",
          "device": "X",
          "address": 217,
          "data_type": "bool",
          "elements": 16
        },
        {
          "name": "counter",
          "device": "D",
          "address": 200,
          "data_type": "u64",
          "word_order": "little"
        },
        {
          "name": "link_bank_1",
          "device": "W",
          "address": 10001,
          "elements": 16,
          "link_points": 2048
        }
      ]
    }
  ]
}
```

`encoding` defaults to `binary`; `ascii` uses BRD/WRD. `cpu` defaults to 1 and supports slots 1–4. Each source has 1–256 unique signals. Addresses are the manufacturer's one-based **decimal** device numbers, without a device prefix. Supported device prefixes are `X`, `Y`, `I`, `E`, `M`, `TU`, `CU`, `L`, `D`, `B`, `R`, `V`, `Z`, `W`, `TS`, `TP`, `TI`, `CS`, `CP`, `CI`; availability and valid extents depend on the CPU/project. Configure and verify those regions before capture. ASCII names have six characters (for example `D00123`, `TP0499`); addresses that do not fit that native field require binary mode.

`data_type` defaults to `u16`; other types are `bool`, `i16`, `u32`, `i32`, `u64`, `i64`, `f32`, `f64`. `elements` defaults to 1 and supports 1–4096; one element remains a scalar, otherwise the value is an array. Bool reads use native bit devices `X/Y/I/E/M/TU/CU/L`. Numeric reads use words, including groups of 16 relays on native bit devices. Wire word bytes are big-endian. For values wider than one word, explicitly choose `word_order: "little"` (lowest word first) or `"big"`; this interpretation must match the PLC program. Integers up to full u64/i64 remain exact through recording and exports. `scale`/`offset` follow the existing typed numeric rules; bool and 64-bit integer values require identity scaling.

X/Y numbering contains unit and slot gaps: unit 0–7, slot 01–16, point 01–64, with the main unit beginning at slot 02. Arrays advance over the documented 64-point slot allocation, including unused positions, rather than treating the printed decimal numbers as a flat integer range. L/W banks are project-dependent: `link_points` is required, 16–8192 in multiples of 16, and the entire signal must fit one bank (0–7). Bank 1 begins at 10001. Use separate signals for other banks. E/R arrays must stay in one configured shared or extended section (extended E starts at 2049, extended R at 1025); no unconfigured gap is automatically skipped. Requests outside the actual project region remain target errors.

Only read commands are emitted. Each signal is segmented into native limits of 256 bits or 64 words under one total poll deadline. A malformed reply, target error, timeout or serialized poll over 1MiB fails the entire poll and retires its socket; no earlier sibling is published. Multi-request values are not a PLC snapshot. Target rejections include exact `yokogawa_exit_code` and `yokogawa_error_parameters`; `yokogawa_detail_code` appears only for exit codes that define it. No automatic CPU, firmware, device-map or network provisioning is performed. This profile is protocol/fixture qualified for the cited Ethernet modules and sequence CPU documentation, without physical-device validation.

Sources: [Ethernet Interface Modules IM34M06H24-01E, part A5](https://www.yokogawa.com/pdf/provide/E/GW/IM/0000014291/0/IM34M06H24-01E.pdf), [PC Link commands IM34M6P41-01E, §§1.2, 2.1–2.2, 3.1–3.2](https://web-material3.yokogawa.com/IM_34M6P41-01E.us.pdf), [Sequence CPU Functions IM34M06P13-01E, device allocation and configuration](https://web-material3.yokogawa.com/IM34M06P13-01E.us.pdf).


## Keyence KV Host Link PLC reads

Use `keyence_kv` on the PLC's configured Host Link TCP server. Keyence lists
this function for the KV-8000/KV-7500/KV-5500 built-in Ethernet port and
KV-EP21V or KV-NC1EP units with compatible CPUs. The usual Host Link port
is 8501. Match the actual module, CPU and port settings.

```json
{
  "version": 1,
  "sources": [{
    "id": "kv",
    "protocol": "keyence_kv",
    "address": "192.0.2.47:8501",
    "every_ms": 200,
    "timeout_ms": 1000,
    "signals": [
      {"name": "inputs", "device": "R", "address": 16, "data_type": "bool", "elements": 4},
      {"name": "readings", "device": "DM", "address": 200, "data_type": "i16", "elements": 3},
      {"name": "timer", "device": "TC", "address": 100, "data_type": "u32"}
    ]
  }]
}
```

Each source accepts 1–256 unique signal names. Contacts `R`, `B`, `MR`,
`LR`, `CR` and `VB` require `data_type: "bool"`. For R/MR/LR/CR, the
numeric `address` is channel number times 16 plus the bit within the channel.
The first example selects native R100 at numeric address 16. Native R15
uses numeric address 15 and the next contact is R100. For B/VB and W,
convert hexadecimal project notation to its numeric value.

Word devices `DM`, `EM`, `ZF`, `W`, `TM`, `CM` and `VM` accept
`u16` (default), `i16`, `u32`, `i32`, `u64`, `i64`, `f32` and
`f64`. Timer/counter current values `TC` and `CC`, and set values
`TS` and `CS`, support `u32` or `i32` for the complete value and `u16` or
`i16` for its lower 16 bits. Select the actual device range
for the installed CPU and access module. For example, KV-NC1EP supports
smaller ranges and device sets than the KV-8000 built-in port.

Native `.U`, `.S`, `.D` and `.L` responses decode exact 16-bit or 32-bit
integers. Floating-point and 64-bit mappings read adjacent `.H` words
and interpret them low-word first. Integer precision is preserved and
non-finite float values fail. Optional finite `scale` and `offset` apply
to smaller integers and floating-point values. Booleans and 64-bit
integers keep their exact values.

`elements` defaults to 1. Up to 4096 elements return an array. The
`max_points` option defaults to 256 and accepts 1–1000. It caps ordinary
word or bit points in each request. Native timer/counter reads use up to
64 indexed values and TM reads use at most 512 words. Lower this option
when required by the PLC. Longer arrays split at whole typed values.
The configured file access uses ZF's absolute addresses.

Capturefab sends only RD and RDS. Commands end in CR and responses must
end in CRLF with the exact documented token widths, separators and ranges.
A target rejection reports `keyence_error_code`. TCP setup and every
query in one poll share `timeout_ms`. A failed poll closes the socket
and publishes none of its readings. Output batches are limited to 1 MiB.
Multiple reads can span PLC scans. Keyence guarantees paired native
32-bit data simultaneity only for even starting word addresses.
[Keyence Ethernet unit specifications](https://www.keyence.co.in/products/controls/plc-building-block-type/kv-5000_3000/models/kv-le21v/)
· [Keyence communication manual, chapter 8](https://www.scribd.com/document/495951207/KvsEtherNetIP-2)
· [Official manual downloads](https://www.keyence.com/support/user/controls/plc/manual/building/)

## Panasonic FP MEWTOCOL-COM reads

Use `mewtocol` with the PLC port configured for MEWTOCOL-COM slave/server
communication. On an FP7 or FP-XH Ethernet connection, select TCP and the
bare command-data format. On an FP7, choose **Not connect with FP2-ET-LAN**
and use unit 1. For an FP0R, FP0H or FP-XH serial COM port, choose Computer
Link and match the PLC's line settings. An RS-485 host interface must handle
direction and present the serial data without local echo.

```json
{
  "version": 1,
  "sources": [{
    "id": "fp",
    "protocol": "mewtocol",
    "address": "192.0.2.46:9094",
    "unit": 1,
    "frame": "standard",
    "signals": [
      {"name": "count", "area": "DT", "address": 100, "data_type": "u32"},
      {"name": "inputs", "area": "X", "address": 31, "data_type": "bool", "elements": 4}
    ]
  }]
}
```

Set exactly one connection option. A TCP `address` includes a numeric IP
and the PLC's configured port. For a serial connection, replace `address`
with `"serial": {"device": "/dev/ttyUSB0", "baud": 9600, "parity": "odd",
"data_bits": 8, "stop_bits": 1}`. The serial fields shown after `device` are
the defaults. A Windows COM device name also works. Each source uses one
exclusive serial port and can list signals on multiple configured stations.
`unit` defaults to 1 and accepts 1–99. A signal can override it. Check the
actual port and CPU's lower station limit before using a station above 32.

The default `frame: "standard"` uses the percent header and reads up to
27 words in one response. Set `"extended"` for a supported PLC's
less-than header, which carries up to 509 words. Use `max_words` to lower
the request limit for the target. The optional `request_bcc: false` sends
the documented two-asterisk request check field. Responses always need
a correct XOR BCC and CR terminator. The adapter uses single-frame reads.

Each source accepts 1–256 unique signals. Data areas `DT`, `LD`, and `FL`
use decimal word addresses. Use `u16` (default), `i16`, `u32`, `i32`,
`u64`, `i64`, `f32`, or `f64` to decode low-word-first values. Optional
finite `scale` and `offset` apply to floating-point and smaller integer
types. The adapter preserves exact 64-bit integers. Contacts `X`, `Y`,
`R`, `L`, `T`, and `C` require `data_type: "bool"`. For X/Y/R/L, compute
the numeric address as decimal word number times 16 plus hexadecimal
bit number. The example starts at X1F and crosses to X20. Timer and counter
contact numbers are decimal. Select addresses implemented in the PLC.

`elements` defaults to 1. Larger counts up to 4096 return arrays. The
adapter divides long register ranges into whole typed values and reads
contacts in groups of up to eight. Each request and reply shares the
poll's `timeout_ms`. Frames identify a station and command without a
transaction number. Use a dedicated master on a configured serial bus.
An unsuccessful reply reports
`mewtocol_error_code` when the PLC supplies one. A failed poll closes its
connection and publishes none of its readings. Segmented reads can span
PLC scans. This profile sends only RD, RCS and RCP reads.
[MEWTOCOL communication manual, WUME-MEWCP-03](https://industry.panasonic.eu/storage/custom-upload/Factory%20%26%20Automation/PLC/Manuals/mn_all_plcs_mewtocol_user_pid_en.pdf)
· [FP7 LAN manual, §8.3](https://mediap.industry.panasonic.eu/assets/download-files/import/mn_fp7_cpu_lan_user_pidsx_en.pdf)
· [FP-XH communication manual](https://industry.panasonic.eu/storage/download-files/import/mn_fpxh_communication_user_pid_en.pdf)

## Omron FINS/TCP and FINS/UDP memory reads

Choose the transport enabled on the CPU or Ethernet module and configure its FINS
route and CS/CJ/CP-style memory map. Omron lists FINS/UDP for NX1P2 in W506 section
5-2. Expose the required NJ/NX values in the CPU's configured FINS memory areas.
Both adapters read those areas by absolute address.

`fins_tcp` negotiates client and server node addresses over TCP:

```json
{
  "version": 1,
  "sources": [{
    "id": "omron",
    "protocol": "fins_tcp",
    "address": "192.0.2.90:9600",
    "source_node": 0,
    "target_network": 0,
    "target_unit": 0,
    "every_ms": 100,
    "signals": [
      {"name": "pressure", "area": "dm", "address": 100, "data_type": "i16", "scale": 0.01},
      {"name": "cycles", "area": "dm", "address": 200, "data_type": "u64", "word_order": "little"},
      {"name": "present", "area": "cio", "address": 10, "bit": 3, "data_type": "bool"},
      {"name": "recipe", "area": "em", "bank": 3, "address": 100, "data_type": "f32"}
    ]
  }]
}
```

The numeric TCP endpoint normally uses port 9600. `source_network`,
`target_network`, `source_unit` and `target_unit` default to 0. Network addresses
are 0–127, with 0 selecting the local network; unit addresses are 0–254, with 0
normally addressing the CPU. Supply the engineered addresses for routed access.
`source_node: 0` requests automatic node assignment in the FINS/TCP handshake;
1–254 requests that particular client node and requires the server to confirm it.
Omit `target_node` to use the negotiated server node, or specify 1–254 for the
configured target route.

`fins_udp` uses one IPv4 datagram per request and requires both FINS node numbers:

```json
{
  "version": 1,
  "sources": [{
    "id": "nx1p2",
    "protocol": "fins_udp",
    "address": "192.0.2.100:9600",
    "source_node": 50,
    "target_node": 100,
    "signals": [{"name": "pressure", "area": "dm", "address": 100, "data_type": "i16", "scale": 0.01}]
  }]
}
```

Use the peer's configured UDP port, normally 9600. `source_node` and `target_node`
are 1–254 and must match the engineered route and IP address conversion method.
The network and unit settings have the same defaults and ranges as `fins_tcp`.
Omit `bind` for an ephemeral source port when the peer accepts dynamic UDP port
conversion. `bind: "192.0.2.50:0"` also selects an ephemeral port and pins the
local interface address. For a fixed-port setup, supply the required local UDP
endpoint, for example `bind: "192.0.2.50:9600"`. Omron W421 section 7-3 specifies
matching local and remote FINS ports for the static conversion modes described
there. Use the configured PLC or gateway port rules for your network. Each
fixed binding needs its own local socket address.

A peer and FINS source network/node pair identify one source. Put all signals
for that identity in one source's signal list. Reserve the FINS source node
for this client in the engineered network so address conversion routes its
responses to the correct host and port.

FINS/UDP matches replies from the configured peer to a single outstanding read.
The eight-bit service ID (SID) is unique until it wraps. An ephemeral binding
rotates the source port before reusing IDs and keeps recently retired ports out
of use for the reply horizon. A fixed binding waits out that horizon before
reusing an ID and drains queued datagrams before sending the new request. Set
`reply_horizon_ms` to cover the expected maximum delayed reply lifetime from send.
It defaults to the configured `timeout_ms` and can be increased up to 30,000.
Waiting for safe reuse counts against the same poll deadline. A fixed binding
needs exclusive use of the local endpoint and a quiet period at least as long as
the horizon after any prior client. The guard covers replies within that horizon
for the current recording. UDP has no field to distinguish a much later duplicate
after both the source port and service ID are reused.

For either transport, `gateway_count` defaults to 2 and also accepts 7. Use 7
only for an engineered route whose CPUs and intermediate nodes support and are
configured for Omron's eight-level routing. Older devices
and routes can reject this setting or return no response. Non-CPU targets remain
limited to three network levels. This setting changes only the request GCT byte;
response GCT remains system-owned and ignored. Capturefab does not change routing
tables or PLC state.

Each source accepts 1–256 uniquely named signals. `area` accepts `dm` (default),
`cio`, `wr`, `hr`, `ar`, `em`, `timer` or `counter`. EM requires an explicit `bank` from 0 to 24;
other areas do not accept `bank`. These are decimal bank indices: 16–24 correspond
to the manual's hexadecimal banks 10–18. Banks 13–24 require a supporting CJ2 or
CS1D-CPU68HA, and installed bank availability depends on the CPU. Each EM bank's
word addresses are 0–32767; the full signal range must stay inside that bank.
The adapter addresses the specified bank directly and does not switch or follow
the PLC's current EM bank. For DM/CIO/WR/HR/AR, `address` is a zero-based word
address from 0 to 65535; the actual permitted range depends on the CPU and area.
`data_type` accepts
`bool`, `u16` (default), `i16`, `u32`, `i32`, `u64`, `i64`, `f32` and `f64`.
Booleans use a `bit` from 0 to 15, default 0, and read individual bits. Consecutive
boolean elements can cross word boundaries. Numeric signals do not accept `bit`.
`elements` defaults to 1; more than one returns an array of consecutive typed
values. One read is limited to 999 words, including the words of wide numeric
types, or 1998 individual bits. The full range must fit the address space.

For `timer` and `counter`, `address` is the logical T/C number 0–4095, not an
ordinary word/bit pair. `u16` (the default) reads the native present-value word
with code `89`; `bool` reads its completion flag with code `09`. These areas do
not accept `bit`, `bank` or wide data types. Explicit `i16` reads the same PV word
with the operator-requested signed interpretation. Each element advances one
logical T/C number, including completion flags across T15–T16; the wire bit-address byte
is always zero. The complete range must stay within T0–T4095 or C0–C4095.
Counters use wire addresses `8000`–`8FFF`, separate from timer addresses
`0000`–`0FFF`. PVs retain raw u16 values by default; existing explicit byte order,
scale and offset remain available. No BCD, instruction variant, duration or
physical timer unit is inferred. The two views are separate reads and do not
claim one internal PLC snapshot. No forced-status code `49` is sent: W342 lists
it under the separate `01 04` command. Named CPU range evidence includes
[Omron CJ2M CPU1x/CPU3x specifications](https://www.ia.omron.com/products/family/2712/specification.html)
and the CJ2H/CJ2M W472 hardware manual; other CPUs can expose smaller areas or
reject unsupported addresses. Capturefab's 1,998-flag ceiling is a bounded
response-capacity policy, not a
manufacturer guarantee that every CPU or route accepts that quantity. W342 lists
999 words for Ethernet, while W421 bounds response data to 1,998 bytes; reduce
the configured count when the target returns a native count/range error.
The same native T/C reads are available in serial Host Link `fins_direct`; its
existing 269-word/538-flag response-capacity limit still applies.
C-mode RD remains DM-only, and `c_mode_cpm` keeps its distinct legacy layout.

`byte_order` defaults to `big`, matching high-byte-first FINS memory words, and
can be `little`. `word_order` defaults to `little`, placing the least-significant
16-bit word first, and can be `big`. Set both from the PLC's application layout
for multiword integers and floating-point values. Integer values retain their
exact precision. Optional `scale` and `offset` apply `value * scale + offset`;
booleans and 64-bit integers cannot be scaled. Bit bytes must be 0 or 1, and
non-finite numeric results fail explicitly.

Both adapters send only FINS MEMORY AREA READ (`01 01`) after transport setup.
TCP setup includes node negotiation. UDP setup binds and selects a single peer.
Connection setup and every read in a poll share one `timeout_ms` deadline. The
adapters check framing, command, swapped response route, SID, FINS end code and
exact data width. UDP also checks the peer endpoint and bounds datagrams to 2012
bytes. Response ICF reserved bits, RSV and gateway count (GCT) are system-owned
and ignored as
W342 section 3-3-3 requires; configured newer CS/CJ/CP routes can return gateway
counts starting at seven. Meaningful ICF response bits remain checked. Errors
report `fins_tcp_error` or `fins_end_code` when supplied by the target. The complete
serialized poll is bounded to 1 MiB and published atomically. No device timestamp
is supplied; samples use the shared host read-completion time.

These adapters follow Omron's [W421 UDP and TCP framing](https://files.omron.eu/downloads/latest/manual/en/w421_cj1w-etn21_cs1w-etn21_ethernet_units_-_construction_of_applications_operation_manual_en.pdf)
and [W342 memory commands](https://assets.omron.eu/downloads/latest/manual/en/w342_cs_cj_cp_nsj_communications_commands_reference_manual_en.pdf).
Check the CPU's supported transports, unit version and memory configuration in
its Ethernet manual. The [NJ/NX W506 manual](https://files.omron.eu/downloads/latest/manual/en/w506_nj_nx-series_cpu_unit_built-in_ethernet_ip_port_users_manual_en.pdf)
lists the applicable models in section 5-2. EM bank codes and bounds follow W342
sections 5-2-2 and 5-3-2. Validation uses protocol fixtures and covers explicitly
addressed DM/CIO/WR/HR/AR, EM and native timer/counter memory reads and recorded exports.
Timer/counter wire fixtures follow W342 sections 5-2-1, 5-2-2 and 5-3-2; these
protocol fixtures do not establish physical PLC qualification.

## Omron serial Host Link reads

Use `omron_host_link` for a PLC serial port configured for 1:N Host Link. Select
`c_mode_rd` for C-mode DM reads or `fins_direct` for direct FINS memory reads.
Check the CPU, port and installed board for the supported mode and device areas.
For example, Omron documents both modes on Host Link-enabled CP1E N/NA(S)
and supported CP2E ports. [CP1E manual](https://files.omron.eu/downloads/latest/manual/en/w480_cp1e_users_manual_en.pdf?v=1)
· [CP2E manual](https://www.omron.com.tw/data_pdf/mnu/w614-e1-03_cp2e.pdf?id=3777)

```json
{
  "version": 1,
  "sources": [{
    "id": "plc",
    "protocol": "omron_host_link",
    "device": "/dev/ttyUSB0",
    "mode": "fins_direct",
    "unit": 0,
    "response_wait_ms": 20,
    "timeout_ms": 1000,
    "signals": [
      {"name": "count", "address": 100, "data_type": "u64"},
      {"name": "ready", "unit": 2, "area": "wr", "address": 10, "bit": 3, "data_type": "bool"}
    ]
  }]
}
```

`device` is a Unix terminal path or Windows COM port. Defaults are 9600 baud,
7 data bits, even parity, 2 stop bits and unit 0. Set `baud`, `data_bits`,
`parity`, `stop_bits` and unit 0–31 to match the configured port. A two-wire
RS-485 adapter must handle line direction and should have echo disabled.
Only `fins_direct` accepts `response_wait_ms`, from 0 to 150 in 10 ms steps.
Allow for serial transmission and the configured wait within `timeout_ms`.

`c_mode_rd` accepts DM word signals from D0 through D9999, with up to 30 words
per signal. Use the lower limits of the installed CPU. `fins_direct` uses the
same typed signal map as the FINS network adapters, with up to 269 words or 538
bits per signal. Both modes use single-frame replies from directly addressed PLCs.
Use one source per terminal and set an optional `unit` on each signal to read
different Host Link stations from 0 through 31. The stations must share the
mode and serial settings. The adapter checks the Host
Link unit, command, XOR FCS, terminator and status. Direct FA replies also
match the FINS service ID and end code. All reads in one poll share a deadline
and a failed signal discards the poll. [Omron communications manual](https://assets.omron.eu/downloads/manual/en/v6/w342_cs_cj_cp_nsj_communications_commands_reference_manual_en.pdf)

### Legacy CPM and SRM1 native areas

Select `mode: "c_mode_cpm"` and a mandatory `model` of `cpm1`, `cpm1a`, `cpm2a`,
`cpm2c`, `srm1` or `srm1_v2`. The model selects address limits; it is not detected.
The configured interface must already use Host Link with matching station/framing,
no CTS control or terminal echo, and the manufacturer-specified cable.
See [Omron W353 sections 4-2 through 4-5](https://assets.omron.eu/downloads/latest/manual/en/w353_cpm1_cpm1a_cpm2a_cpm2c_srm1_programming_manual_en.pdf).

```json
{
  "version": 1,
  "sources": [{
    "id": "legacy_plc", "protocol": "omron_host_link",
    "device": "/dev/ttyUSB0", "mode": "c_mode_cpm", "model": "cpm2a",
    "unit": 0, "baud": 9600, "data_bits": 7, "parity": "even", "stop_bits": 2,
    "every_ms": 250, "timeout_ms": 1000,
    "signals": [
      {"name": "inputs_raw", "area": "ir_sr", "address": 0, "elements": 4},
      {"name": "holding", "area": "hr", "address": 1, "data_type": "i16", "elements": 3},
      {"name": "pv_raw", "area": "tc_pv", "address": 7, "elements": 3},
      {"name": "completion", "area": "tc_status", "address": 0, "data_type": "bool", "elements": 5}
    ]
  }]
}
```

| Area / native command | CPM1 / CPM1A | CPM2A / CPM2C | SRM1 / SRM1-V2 |
| --- | --- | --- | --- |
| `ir_sr` / RR | 0–19 or 200–255 | 0–49 or 200–255 | 0–19 or 200–255 |
| `lr` / RL | 0–15 | 0–15 | 0–15 |
| `hr` / RH | 0–19 | 0–19 | 0–19 |
| `dm` / RD | 0–1023 or 6144–6655 | 0–2047 or 6144–6655 | 0–2047 or 6144–6655 |
| `ar` / RJ | 0–15 | 0–23 | 0–15 |
| `tc_pv` / RC | 0–127 | 0–255 | 0–127 |
| `tc_status` / RG | 0–127 | 0–255 | 0–127 |

Word areas default to `u16`; the existing numeric types and explicit word/byte
order apply to consecutive native words. Defaults are `elements: 1`,
`word_order: "little"` and `byte_order: "big"`. `elements` counts typed values, so wider
types consume multiple words. A complete span must fit one table interval.
`tc_status` requires `bool`, with one native completion flag per index, identity
scaling and default order. It does not accept a `bit` selector. PV words are raw:
no BCD conversion, timer/counter alias, timebase, units or source clock is inferred.

The client splits reads into complete requests of at most 30 words or 120 flags,
without splitting a typed value. The flag cap stays within 131-byte complete replies
and avoids relying on the manual's conflicting 123-flag division statement.
Sequential requests are not a PLC snapshot. All chunks and signals share one
original deadline and a 1 MiB serialized-poll limit; a failed poll publishes no
partial values and permanently retires that source's port. Earlier completed
polls remain available. Nonzero remote status is retained as `host_link_end_code`.

Supported baud rates are 1200, 2400, 4800, 9600 and 19200, with 7/8 data bits,
1/2 stop bits and even/odd/none parity. Defaults are 9600/7E2 and station 0;
per-signal `unit` overrides stay within 0–31. This mode uses native serial,
with no TCP service, mode switching, response continuation or automatic reconnect.
The existing `c_mode_rd` and `fins_direct` contracts remain unchanged.

## Omron CompoWay/F controller reads

Use `omron_compoway_f` on the two-wire RS-485 option of an E5CC or E5EC
temperature controller, G3ZA power controller or G3PW `-FLK` controller.
Configure the controller for CompoWay/F and give Capturefab exclusive use of
the serial master port. Set the serial parameters and unit to match the
controller. The example reads the E5CC/E5EC process value from area `0xC0`,
address `0`.

```json
{
  "version": 1,
  "sources": [{
    "id": "temperature",
    "protocol": "omron_compoway_f",
    "device": "/dev/ttyUSB0",
    "unit": 1,
    "baud": 9600,
    "parity": "even",
    "data_bits": 7,
    "stop_bits": 2,
    "signals": [{"name":"pv","variable_type":192,"address":0,"data_type":"i32"}]
  }]
}
```

`device` can also be a Windows COM port such as `COM3`. Adapter defaults are
unit 1 and 9600 baud, even parity, 7 data bits and 2 stop bits. Match these to
the device. For example, G3PW defaults to 57600 baud. Supported rates are
9600, 19200, 38400 and 57600. A signal can override `unit` on the same bus.
Choose unit 0–99, within the controller's range. G3ZA supports 0–31.

Only Variable Area Read (`0101`) is sent, with subaddress `00` and bit position
`00`. `variable_type` uses decimal JSON integers for the manual's hexadecimal
type codes. Choose `u16` or `i16` for `0x80`–`0x8F` and `u32` or `i32` for
`0xC0`–`0xCF`, using the controller's variable map. `address` is the zero-based
variable address. `elements` defaults to 1 and supports up to 8 consecutive
values. One element yields an integer and more yield an array. Values retain
the controller's raw integer units. Use the controller's configured decimal
point and sensor type to interpret them.

Register maps and setup are in Omron's [E5CC/E5EC H175](https://files.omron.eu/downloads/latest/manual/en/h175_e5_c_communications_manual_en.pdf?v=1),
[G3ZA Z200](https://files.omron.eu/downloads/latest/manual/en/z200_g3za_users_manual_en.pdf?v=1)
and [G3PW Z280](https://files.omron.eu/downloads/latest/manual/en/z280_g3pw_power_controllers_users_manual_en.pdf?v=1)
manuals.

## Beckhoff ADS PLC reads

The native `ads` adapter reads PLC values over plain AMS/TCP. Configure the TCP
endpoint, the target PLC runtime's AMS Net ID and port, and the source identity
that the target's ADS router already permits:

```json
{
  "version": 1,
  "sources": [{
    "id": "beckhoff",
    "protocol": "ads",
    "address": "192.0.2.50:48898",
    "target_net_id": "192.0.2.50.1.1",
    "target_port": 851,
    "source_net_id": "192.0.2.60.1.1",
    "source_port": 32905,
    "every_ms": 100,
    "timeout_ms": 1000,
    "signals": [
      {"name": "cycles", "symbol": "MAIN.nCycles", "data_type": "u64"},
      {"name": "pressure", "index_group": 16416, "index_offset": 0, "data_type": "f32"},
      {"name": "label", "symbol": "MAIN.sLabel", "data_type": "string", "length": 81}
    ]
  }]
}
```

`address` requires a numeric IP and explicit TCP port, normally 48898. AMS Net IDs
are six decimal octets and are separate logical addresses; use the configured IDs
rather than deriving them from the TCP address. AMS ports must be nonzero.
`target_port` defaults to 851 and `source_port` to 32905. Select the actual PLC
runtime port, such as 801 for a configured TwinCAT 2 runtime. Provision the ADS
route and access policy before recording. Capturefab does not add routes, register
a local ADS router or change the PLC's operating state. This adapter needs no
locally installed TwinCAT runtime or vendor library.

Each source takes 1–256 uniquely named signals. Supply either `symbol` or both
`index_group` and `index_offset`, using the PLC project's type and address map.
The default `data_type` is `u16`; supported types are `bool`, `u8`, `i8`, `u16`,
`i16`, `u32`, `i32`, `u64`, `i64`, `f32`, `f64`, `bytes`, `string` and `wstring`.
Numeric fields use ADS little-endian representation. `elements` defaults to 1 and
accepts 1–4096 contiguous scalar elements; more than one produces a JSON array.
Fixed-width types do not accept `length`. Integers retain their full precision,
including signed and unsigned 64-bit values. BOOL values must be encoded as 0 or 1.
Optional `scale` and `offset` apply `value * scale + offset` to numeric types
through 32-bit integers and floating-point types. Boolean, 64-bit integer and
variable-width values cannot be scaled. Non-finite results fail explicitly.

For `bytes`, supply `length` in bytes and use `elements: 1`. The value contains
the entire buffer as `{"encoding":"base64","data":"..."}`.

For `string` or `wstring`, `length` is the byte width of one slot, including its
terminator. Use `elements` for a contiguous array of equal-width slots. The
adapter reads all slots in one request and returns a string for one slot or an
array of strings for several. For example, two `STRING(80)` values use
`length: 81` and `elements: 2`.

`string` accepts `encoding: "latin1"` for TwinCAT's default STRING storage or
`encoding: "utf8"` for UTF-8 variables. Omitting `encoding` keeps Capturefab's
UTF-8 default. Each slot must contain a NUL terminator. `wstring` uses an even
byte width and little-endian UTF-16 with a two-byte NUL terminator. The JSON
strings contain the text before each terminator. Read `bytes` to retain the
whole buffer or use a different encoding. A missing terminator or invalid text
in any slot fails the entire poll.
[Beckhoff STRING storage](https://infosys.beckhoff.com/content/1033/tc3_plc_intro/2529410443.html)

Symbol lookup obtains a handle using ADS ReadWrite with index group `0xF003`, then
reads values using `0xF005`. A connection reuses handles for repeated symbols.
At completion, interruption or failure, cleanup releases handles using ADS Write
to `0xF006`, within a separate budget of at most one second. These are symbol
metadata operations; Capturefab sends no PLC value, state, program or route writes.
A release error fails an otherwise successful run; an already failed connection
can prevent remote handle cleanup. PLC changes that invalidate a handle fail the
read explicitly. Secure ADS, notifications, automatic symbol/type discovery and
structured type decoding are not implemented; use a configured `bytes` buffer
when the application needs the original representation.

All requests in a poll, including TCP connection and handle creation, share one
`timeout_ms` deadline. A response must match the AMS addresses, command and invoke
ID, with valid lengths and successful AMS/ADS statuses. Individual reads are
limited to 65536 bytes and the serialized poll to 1 MiB, including JSON and base64
expansion. A malformed response, overflow or rejected signal fails the entire poll
without publishing its earlier signals. ADS reads supply no device timestamp;
the poll uses the shared host read-completion time.

## Passive CAN and CAN FD capture

The native `can` adapter receives frames from an existing Linux SocketCAN
interface. Bring up and configure the interface for the bus before recording,
including its bitrate and CAN FD mode when required. Capturefab opens a receive
socket; it does not transmit frames or configure the controller. Other platforms
return `unsupported` for this adapter.

```json
{
  "version": 1,
  "sources": [{
    "id": "can_bus",
    "protocol": "can",
    "interface": "can0",
    "fd": true,
    "error_frames": true,
    "signal": "frame",
    "filters": [{"can_id": 291, "mask": 2047, "extended": false}],
    "signals": [
      {"name": "rpm", "can_id": 291, "offset": 0, "data_type": "u16", "byte_order": "little", "scale": 0.25},
      {"name": "ready", "can_id": 291, "offset": 2, "data_type": "bool", "bit": 3}
    ]
  }]
}
```

`interface` is a local name of 1–15 bytes, such as `can0` or `vcan0`. `fd` defaults
to true and enables both Classical CAN and CAN FD reception; false selects
Classical CAN only. `error_frames` defaults to true. `signal` names the raw frame
signal and defaults to `frame`; raw frames are always preserved, even when typed
mappings are configured. This is an event stream; `every_ms` does not throttle it.

Up to 256 optional `filters` select data and remote-request frames. Each has a
numeric `can_id`, a 29-bit `mask` defaulting to 536870911 (`0x1fffffff`), and an
optional `extended` boolean. A frame matches when `(frame_id & mask) ==
(can_id & mask)` and its standard/extended format matches when specified. Filters
are ORed; an empty array accepts all IDs. Standard IDs fit 11 bits and extended
IDs fit 29 bits. Error-frame reception is controlled independently by
`error_frames`, so an ID filter does not hide controller error reports.

Every accepted frame emits a raw object with `kind: "can_frame"`, `interface`,
numeric `can_id` and `raw_can_id`, `extended`, `remote`, `error`, `fd`, `length`,
`data_hex`, `fd_flags`, `bitrate_switch`, `error_state_indicator` and `local`.
The raw ID retains Linux's flag bits; `can_id` contains the identifier or the
error-class bits. `data_hex` preserves the payload bytes as hexadecimal text.
Remote-request frames have an empty payload but retain their requested `length`.
Classical eight-byte frames can also include `len8_dlc` when the original wire
DLC is 9–15. `local` marks a frame generated on the local host.

Optional `signals` supplies up to 256 named mappings, using the engineered byte
layout. Each selects `can_id`, `extended` (default false), and a byte `offset`.
`data_type` accepts `bool`, `u8`, `i8`, `u16` (default), `i16`, `u32`, `i32`,
`u64`, `i64`, `f32` and `f64`. `byte_order` is `little` by default or `big`.
Boolean mappings use `bit` 0–7, default 0. Numeric scaling uses
`value * scale + offset_value`, with defaults 1 and 0; booleans and 64-bit integers
cannot be scaled. Integers retain their exact precision. Mappings must fit within
8 bytes with `fd: false`, or 64 with `fd: true`. A matching frame shorter than its
mapping, or a non-finite numeric result, fails explicitly. Remote-request and
error frames produce raw observations without typed decoding. DBC parsing,
arbitrary packed bitfields, multiplexing and higher-level protocols such as
CANopen, J1939 or ISO-TP are not decoded automatically.

Raw and mapped readings from a frame share one host-monotonic observation on the
recording timeline. When the kernel supplies it, raw `kernel_received_at` carries
its separate wall-clock receive timestamp, labeled with
`timestamp_clock: "linux_socket_realtime_receive"`. It can reflect host wall-clock
adjustments and is not device or hardware time; it is not copied to `source_time`.
Use the sample's `elapsed_ns` to correlate CAN with cameras and other sources.
Neither timestamp establishes a bus-wide hardware synchronization guarantee.

Before completion the source must have observed a raw frame and every configured
mapped signal. An unmatched mapping therefore fails at cutoff rather than
claiming a complete capture. Socket receive-queue loss, truncated frames and
malformed matching frames fail explicitly; queues and batches remain bounded.

## BACnet/IP meters and controllers

Use `bacnet_ip` to poll configured BACnet object instances at a direct unicast
IPv4 UDP endpoint. On Schneider PowerLogic PM5500 meters with BACnet/IP enabled,
firmware 2.3.0 or later exposes total active power in kW as Analog Input instance
3060. Accuenergy Acuvim II with the AXM-WEB2 module exposes total watts as Analog
Value instance 17. Siemens Desigo PXC4.E16.A, PXC5.E24.A and PXC7.E400.A support
ReadProperty. Use the engineered object map for each controller. [Schneider BACnet/IP firmware support](https://www.se.com/us/en/faqs/FA180129/) · [Schneider PM5500 object map](https://productinfo.se.com/pm5500/595e2aa946e0fb0001f715da/PM5500%20user%20manual/English/PM5500SeriesUserManualv02_0000044889.ditamap/%24/R_Communications_BACnetAnalogInputObjects_PM5500_0000077686) · [Accuenergy Acuvim II manual](https://www.accuenergy.com/wp-content/uploads/Acuvim-II-Power-Meter-User-Manual-1040E1303.pdf) · [Siemens Desigo interoperability blocks](https://sid.siemens.com/r/A6V13495554/21610799243___en-US_21610935819)

```json
{
  "version": 1,
  "sources": [{
    "id": "meter",
    "protocol": "bacnet_ip",
    "address": "192.0.2.32:47808",
    "every_ms": 250,
    "timeout_ms": 500,
    "signals": [{
      "name": "active_power",
      "object_type": "analog_input",
      "instance": 3060,
      "units": true,
      "status_flags": true
    }]
  }]
}
```

Choose `analog_input`, `analog_output`, `analog_value`, `binary_input`,
`binary_output`, `binary_value`, `multi_state_input`, `multi_state_output` or
`multi_state_value`. Instances range from 0 to 4194302. A signal reads the
scalar Present_Value as an analog number, binary boolean or positive multi-state
integer. Set `units` for an analog object's numeric BACnet engineering-units
code. Set `status_flags` for the `in_alarm`, `fault`, `overridden` and
`out_of_service` booleans. With either property enabled, the reading is an object
containing `value` and the requested properties.

The default `bind` is `0.0.0.0:0`. Set an explicit local interface or UDP port,
such as `"bind": "192.0.2.12:47808"`, to match the installation. With a fixed
port, `reply_horizon_ms` governs when invoke IDs can be reused after a request.
It defaults to `timeout_ms` and accepts values from that timeout to 30000 ms.
With an ephemeral port, Capturefab rotates the port at invoke-ID rollover.
Each poll shares one deadline and may include up to 256 property reads,
counting each requested Units or Status_Flags value. Samples are published once
all reads succeed and carry host receive timestamps.

Use Annex J Original-Unicast-NPDU and confirmed, unsegmented ReadProperty.
Discovery, routed networks, BBMD, BACnet/SC, MS/TP and subscriptions are outside
this profile. [BACnet/IP Annex J](https://bacnet.org/wp-content/uploads/sites/4/2022/08/Add-1995-135a.pdf)

## IEC 62056-21 optical meter readout

Use `iec62056_21` with a transparent serial optical probe and a meter
configured for Mode C ASCII readout. Landis+Gyr E350 Series 2 supports
its standard C-a readout, and Iskraemeco MT174 documents Mode C on its
optical port. Select OBIS codes from the meter's configured readout list.
Some E350 status flags can clear on readout according to the meter
configuration. [E350 Series 2 manual](https://www.elvaco.com/Image/GetDocument/en/209/landis%20gyr%20e350%20users%20manual%20english.pdf#page=46) ·
[MT174 technical description](https://www.camax.co.uk/downloads/MT174-Techical-Description.pdf#page=21)

```json
{
  "version": 1,
  "sources": [{
    "id": "meter",
    "protocol": "iec62056_21",
    "device": "/dev/ttyUSB0",
    "timeout_ms": 10000,
    "every_ms": 15000,
    "signals": [
      {"name": "import_energy", "obis": "1.8.0", "format": "decimal"},
      {"name": "meter_serial", "obis": "C.1.1"}
    ]
  }]
}
```

Set `device` to a Unix terminal or Windows COM port such as `COM12`. The probe
must preserve eight data bits in 8N1 mode so Capturefab can transmit and check
even parity in bit 7. This produces 7E1 on the optical link. The sign-on and
readout acknowledgement use 300 baud, and the readout uses the offered rate
up to `maximum_baud` (19200 by default). Valid rates are 300, 600, 1200, 2400,
4800, 9600 and 19200. An offer above the configured maximum selects the
standard 300-baud fallback. Allow enough time for the full readout. At
300 baud each character takes about 33 ms. Omit `meter_address` for anonymous
sign-on, or supply the meter's configured address with up to 32 ASCII
letters, digits or spaces, subject to the meter's own limits.

Each sample includes `obis`, `identification`, `baud`, `block_check` and
`values` in wire order. Each value keeps its exact `raw` text and the meter's
`unit` when supplied. `format: "decimal"` also emits an exact JSON `number` for
each explicitly identified primary value. Additional parenthesized fields
remain text. A missing OBIS code or malformed readout fails the entire poll.
The default requires a verified block check. Set `allow_unchecked: true`
to permit the IEC unframed readout form and tag it `unchecked`. Parity is
checked for both forms. The adapter sends sign-on and readout commands.
Programming mode, Mode E HDLC, and fixed-rate RS-485 are unsupported.
Validation uses protocol fixtures and pseudo-terminals. [IEC 62056-21](https://webstore.iec.ch/en/publication/6398)

## Binary SML optical meter readout

Use `sml_serial` with a transparent optical receiver and a meter configured to
push standard binary SML on its front INFO port. Set `device` to the receiver's
Unix terminal or Windows COM port. The adapter receives at 9600 baud with eight
data bits, no parity and one stop bit. It sends zero bytes to the meter.

Holley DTZ541 and EMH eHZ-P and eHZM document this front INFO mode. Some meters
omit instantaneous power in their reduced dataset. Use the full INFO dataset
and the OBIS map for your meter when capturing power. [Holley DTZ541 manual](https://www.holleytech.de/wp-content/uploads/2022/05/Benutzerhandbuch_mME-DTZ541_V3.0.pdf#page=15) · [EMH eHZ-P manual](https://emh-metering.com/wp-content/uploads/2025/01/eHZ-P-BIA-D-1.45-compressed.pdf) · [EMH eHZM manual](https://emh-metering.com/wp-content/uploads/2022/11/eHZM-BIA-D-1.11.pdf)

```json
{
  "version": 1,
  "sources": [{
    "id": "meter",
    "protocol": "sml_serial",
    "device": "/dev/ttyUSB0",
    "timeout_ms": 1000,
    "signals": [
      {"name": "import_energy", "obis": "1-0:1.8.0*255"},
      {"name": "active_power", "obis": "1-0:16.7.0*255"}
    ]
  }]
}
```

Supply all six decimal OBIS bytes in `A-B:C.D.E*F` form with unique codes and
signal names. Each sample contains `obis`, the signed or unsigned `raw` integer,
and the exact decimal `value` after scaling. When transmitted, `scaler`, the
numeric SML `unit` and unsigned `status` are preserved. Samples use host receive
timestamps.

The adapter synchronizes at the next start marker and checks the transport and
message CRCs of each Open/GetList/Close push file. A valid file publishes all
configured signals together. A malformed synchronized frame or missing OBIS
fails the file. Record for at least one full push cycle. This profile covers
standard SML transport v1 and CRC-16/X-25 on front INFO ports. [BSI SML specification](https://www.bsi.bund.de/SharedDocs/Downloads/DE/BSI/Publikationen/TechnischeRichtlinien/TR03109/TR-03109-1_Anlage_Feinspezifikation_Drahtgebundene_LMN-Schnittstelle_Teilb.pdf?__blob=publicationFile&v=1) · [VDE FNN EDL specification](https://www.vde.com/resource/blob/2346190/b0c754439ffbfdd95287d1ae50f9f3d9/fnn-lastenheft-edl---funktionale-merkmale-und-protokolle--2016--data.pdf)

## SDI-12 environmental sensors

Use `sdi12_serial` with an Infwin SDI12ELF20 in its default transparent USB
serial configuration at 9600 baud, eight data bits, no parity and one stop
bit. The bridge handles the SDI-12 bus's inverted 1200-baud signalling,
wake break and marking. Its manual gives 20 ms and 10 ms defaults for break
and marking respectively. Use one `sdi12_serial` source per bridge. Capturefab
sends addressed measurement and data commands.
[Infwin converter manual, §§3, 5, 6.2 and 7.1](https://www.infwin.com/wp-content/uploads/SDI12ELF20-SDI-12-to-USB-Converter-User-Manual.pdf)

```json
{
  "version": 1,
  "sources": [{
    "id": "soil",
    "protocol": "sdi12_serial",
    "device": "/dev/ttyUSB0",
    "address": "0",
    "measurement": 0,
    "crc": true,
    "every_ms": 5000,
    "timeout_ms": 2500,
    "signals": [
      {"name": "vwc_counts", "index": 0},
      {"name": "soil_temperature", "index": 1},
      {"name": "conductivity", "index": 2}
    ]
  }]
}
```

This mapping follows the METER TEROS 12 measurement order. Its first result
is the raw calibrated VWC count, followed by temperature and conductivity.
TEROS 11 omits conductivity. Campbell Scientific CS650 and CS655 have their
own documented M measurement order and additional M1 through M4 mappings.
Use the manual for the exact sensor and command. [METER TEROS 11/12 Integrator Guide, Table 2](https://publications.metergroup.com/Integrator%20Guide/18224%20TEROS%2011-12%20Integrator%20Guide.pdf) · [Campbell CS650/CS655 SDI-12 measurement maps](https://help.campbellsci.com/cs650-cs655/cs650-cs655/operation/sdi12-measurements.htm)

For one sensor, set `address` at the source level. For several sensors on one
bridge, replace the top-level `address`, `measurement`, `crc` and `signals`
with a `sensors` array. Each entry has those same fields.

```json
{
  "version": 1,
  "sources": [{
    "id": "soil", "protocol": "sdi12_serial", "device": "/dev/ttyUSB0",
    "every_ms": 5000, "timeout_ms": 2500,
    "sensors": [
      {"address":"A","crc":true,"signals":[{"name":"teros_counts","index":0}]},
      {"address":"0","measurement":4,"signals":[{"name":"cs650_vwc","index":0}]}
    ]
  }]
}
```

Each address is a unique ASCII letter or digit. METER recommends a nonzero
TEROS 11/12 address to suppress its DDI startup output on a mixed bus. Signal
names are unique across the source, and indices are unique per sensor.
[METER TEROS 11/12 Integrator Guide, pp. 5 and 8](https://publications.metergroup.com/Integrator%20Guide/18224%20TEROS%2011-12%20Integrator%20Guide.pdf) · [Campbell CS650/CS655 manual, two sensors on one port, p. 37](https://s.campbellsci.com/documents/us/manuals/cs650.pdf)

`measurement` defaults to 0 for M. Values 1 through 9 select the corresponding
numbered M command. Use the sensor's command map to select a number assigned
to measurement. Some sensors assign other numbers to calibration or control.
Enable `crc` for an MC variant on a compatible sensor.
Capturefab validates the address, announced value count, data response CRC
when selected, and the signed decimal data format. It waits for a readiness
service request or for the sensor's announced readiness time, then retrieves
the full ordered result using D0 through D9 as needed. Select readings by
zero-based index, from 0 through 8. Capturefab completes every measurement and
data exchange before addressing the next sensor. Configure a timeout long
enough for the entire sequence. A poll publishes all readings together with
the host receive timestamp for the completed sequence.

Each sample has `raw` for the exact transmitted signed decimal text and
`value` for the exact JSON number. Numeric sensor error codes stay in the
result as reported. Model-specific units and calibration remain in the
sensor's mapping. This profile implements addressed M/MC measurements.
Concurrent, continuous, high-volume and vendor-specific commands, direct
electrical SDI-12 wiring, and alternate USB command wrappers are outside
this profile. [SDI-12 v1.4, §§4.4.5, 4.4.8–9 and 4.4.12](https://www.sdi-12.org/specification.php?file_id=1)

## ifm IO-Link process input

The native `ifm_iot` adapter reads process input from one physical IO-Link port
through an ifm master that exposes the documented IoT Core JSON interface.
Configure each required port as its own source, using the master's numeric IP and
HTTP(S) port. Provision its network, IO-Link port mode and security settings
before recording; Capturefab does not change them.

```json
{
  "version": 1,
  "sources": [{
    "id": "temperature",
    "protocol": "ifm_iot",
    "address": "192.0.2.80:80",
    "port": 1,
    "every_ms": 100,
    "signal": "process_data",
    "signals": [
      {"name": "temperature_c", "offset": 0, "data_type": "u16", "byte_order": "big", "right_shift": 2, "scale": 0.1}
    ]
  }]
}
```

`port` is 1–8 and selects the documented path
`/iolinkmaster/port[N]/iolinkdevice/pdin/getdata`. A read-only HTTP POST requests
that data; no output or parameter-writing service is emitted. `code_format`
defaults to `"text"`, which sends JSON `code: "request"`; select `"numeric"`
for a master configured for numeric request code 10. The adapter validates HTTP
status, response code and correlation ID, then decodes the returned hexadecimal
process-input buffer, limited to 32 bytes. A request, including TCP/TLS setup and
the entire HTTP response, shares the source's `timeout_ms` deadline.

Every read preserves a raw signal, named `process_data` by default, with
`kind: "iolink_process_data"`, `port`, `length` and original `data_hex`. Up to 256
optional `signals` interpret that same buffer. Derive these mappings from the
specific sensor's IODD and configuration; the example's packed 16-bit temperature
layout is not a universal IO-Link layout. For example, bytes `03C9` decode to 969,
then `right_shift: 2` and `scale: 0.1` produce 24.2.

Mappings use byte `offset`, `data_type` (`bool`, `u8`, `i8`, `u16` default, `i16`,
`u32`, `i32`, `u64`, `i64`, `f32` or `f64`) and `byte_order` (`big` default or
`little`). Booleans can select `bit` 0–7, default 0. Integer `right_shift` defaults
to zero; it is arithmetic for signed integers and logical for unsigned integers.
Numeric scaling is `value * scale + offset_value`, with defaults 1 and 0.
Booleans and 64-bit integers cannot be scaled; exact integer precision is retained.
Short payloads, non-finite values and invalid mappings fail the entire read
explicitly. IODD discovery, arbitrary packed fields and port reconfiguration are
not implemented. Masters exposing JSON Integration for IO-Link use the separate
`iolink_json` adapter below.

Optional `tls` uses the same verified TLS settings as MQTT: `server_name`, optional
`ca_certificate`, and paired `client_certificate`/`client_key` when mutual TLS is required.
Authenticated ifm security mode requires TLS and paired `username_env` and
`password_env` naming environment variables. Credentials use the master's JSON
authentication fields and are not embedded in the configuration, endpoint or
recording. TLS requires the `mqtt-tls` build feature. The raw and mapped values
share the host-monotonic observation when their complete response arrives; no
sensor timestamp is invented. Consult the [ifm master manual](https://www.ifm.com/mounting/80284138UK.pdf)
for the endpoint and authentication supported by the particular master.

## JSON Integration for IO-Link process input

The native `iolink_json` adapter polls the process-input endpoint defined by JSON
Integration for IO-Link. Configure one source per device alias on a master that
exposes this interface. For example, [SICK documents this endpoint and response
for SIG200/SIG350](https://support.sick.com/sick-knowledgebase/article/?id=714b64cd-aaaa-4704-b049-63ac9a679143).
The master must already have its network, port modes and any required IODD
configured. Capturefab sends GET requests and does not change parameters, process
outputs or port configuration.

```json
{
  "version": 1,
  "sources": [{
    "id": "sensor",
    "protocol": "iolink_json",
    "address": "192.0.2.81:80",
    "device": "master1port1",
    "format": "byte_array",
    "every_ms": 100,
    "timeout_ms": 1000,
    "signal": "process_data",
    "signals": [
      {"name": "input_word", "offset": 0, "data_type": "u16", "byte_order": "big"}
    ],
    "json_signals": [
      {"name": "valid", "pointer": "/iolink/valid"}
    ]
  }]
}
```

`address` is the numeric IP and HTTP(S) port. The required `device` alias accepts
1–32 ASCII letters, digits or underscores; use the alias provisioned on the master.
`base_path` defaults to `/iolink/v1`, so the example requests:

```text
GET /iolink/v1/devices/master1port1/processdata/getdata/value?format=byteArray
```

Set `base_path` for a gateway prefix or another supported API base path. The
[2.0 specification moves its base path to `/iolink/v2`](https://github.com/iolinkcommunity/JSON_for_IO-Link/releases/tag/2.0.0);
setting that path requires the master to return the response contract described
here. There is no API-version negotiation or endpoint discovery. The path must be
an absolute ASCII path without query, fragment or dot segments. Optional `host`
sets the HTTP Host authority; it defaults to `address` and does not change the
numeric TCP destination or TLS certificate identity.

`format` defaults to `byte_array`, which requests the wire parameter `byteArray`.
The response must be a JSON object. When it contains `iolink`, that member must
have `valid: true` and a `value` array of 0–32 integer bytes, each 0–255. Optional
`cqValue` and `iqValue` must be booleans. Digital-input responses may omit `iolink`
when at least one of those booleans is present; use `json_signals` for them because
no process-byte buffer is available. Invalid or missing quality, malformed bytes,
short mappings and missing selected fields fail the entire poll.

Every successful poll includes a raw signal, named `process_data` by default,
whose value contains `kind: "iolink_json_process_data"`, `device` and the complete
original JSON object under `response`. It retains quality, digital values, units
and vendor fields. Optional byte `signals` use the same offset, data type, byte
order, bit, shift and scaling options documented for `ifm_iot` above. Derive those
mappings from the connected sensor's IODD; `input_word` is only an example layout.
The sum of byte mappings and `json_signals` is limited to 256, with unique names
distinct from the raw signal.

For a master that provides IODD-decoded process values, use `format: "iodd"` and
`json_signals` instead of byte mappings. For example, if its `iolink.value` contains
`{"Distance":{"value":55,"unit":"cm"}}`, this source preserves both the decoded
value and its unit in `distance`:

```json
{
  "id": "distance",
  "protocol": "iolink_json",
  "address": "192.0.2.81:443",
  "device": "master1port1",
  "format": "iodd",
  "every_ms": 100,
  "timeout_ms": 1000,
  "host": "iolink.example.test",
  "tls": {
    "server_name": "iolink.example.test",
    "ca_certificate": "/etc/capturefab/plant-ca.pem"
  },
  "bearer_token_env": "IOLINK_TOKEN",
  "json_signals": [
    {"name": "distance", "pointer": "/iolink/value/Distance"},
    {"name": "distance_value", "pointer": "/iolink/value/Distance/value"}
  ]
}
```

JSON pointers address the complete response object, before the raw signal's
`response` wrapper. They use RFC 6901 text syntax: `~1` escapes `/`, `~0` escapes
`~`, and the empty pointer selects the whole response. A selection retains its
JSON type and exact number representation; select an object to retain its unit
and other metadata together. The client does not infer field names, convert units
or download IODDs. In `iodd` mode, `iolink.value` must be present and non-null;
its scalar, array or object structure comes from the master's decoding.

Optional `username_env` and `password_env` select HTTP Basic authentication and
must be supplied together. Alternatively, `bearer_token_env` selects one HTTP
Bearer token; it cannot be combined with Basic authentication. These fields name
environment variables, and the credentials are sent in Authorization headers.
This differs from ifm's JSON authentication fields. Basic usernames cannot contain
a colon, and bearer tokens must be nonempty ASCII without spaces or controls.
Configure the authentication scheme enabled on the master; no login flow is
performed. Credentials are not stored in recordings.

Optional `tls` verifies the server using `server_name` for certificate identity
and SNI, independent of `host`. A configured `ca_certificate` replaces public
roots with the plant CA; omit it to use public roots. Mutual TLS uses paired
`client_certificate` and `client_key`. TLS requires the `mqtt-tls` build feature.
The total `timeout_ms` deadline covers TCP/TLS setup, request and complete HTTP
response. Redirects and compressed HTTP bodies are unsupported. The decoded HTTP
body and the serialized raw-plus-selected poll each have a 1 MiB limit; selecting
large subtrees can reach the poll limit before the body limit.

Raw and selected values share the host-monotonic observation when the complete
response arrives. Vendor timestamps remain ordinary response fields and are not
promoted to `source_time`. Endpoint, quality, mapping, TLS, HTTP chunking and
atomic-failure behavior are exercised by protocol fixtures; device-specific
firmware and engineering settings still determine the usable interface.

## Gocator profile and surface groups

Use `gocator_gdp` for a running legacy Gocator sensor configured to send
Gocator Data Protocol results on its data port (TCP 3196 by default). Configure
and start the sensor before recording. The adapter only receives data.

```json
{"version":1,"sources":[
  {"id":"profiler","protocol":"gocator_gdp","address":"192.0.2.50:3196",
   "output_directory":"scan-groups","max_group_bytes":16777216,"timeout_ms":500}
]}
```

Each complete group is stored byte for byte as a numbered `.gdp` file, starting
with `group-00000001.gdp`. Choose a fresh output directory for each run. The
time-series sample gives the path, byte count, checksum and message boundaries.
Uniform Profile messages with 32-, 40- or 48-byte attributes expose row count,
width, signed 16-bit sample layout,
and the sensor's original scale and offset values. Profile Point Cloud type 5
messages with 32-, 40- or 48-byte attributes expose their interleaved signed
16-bit X/Z sample layout and original scale and offset values. The index gives
the sample byte offset in the complete GDP archive. Type 5 attribute layouts
outside these sizes remain opaque. Stamp messages expose the
stamp-array offset, count and stride, plus the first stamp's original clock and
encoder fields. Other message bodies remain in the file with their numeric type
and byte range. Stamp strides of 56 and 64 bytes receive typed indexes. Other
stamp strides and unknown attribute versions remain opaque records with their
original type, offset and byte count. Known layouts require exact array lengths.
Device timestamps
are in microseconds in their own clock domain. A 64-byte stamp also exposes
`ptp_timestamp_us`, measured from the PTP epoch, whose timescale depends on the
sensor's PTP configuration. Arrayed count and index fields in 48-byte profile
attributes retain their signed values. All typed Uniform Profile and Point Cloud
exposure fields use `exposure_raw` because LMI's wire manual and SDK API
documentation specify differing units for that field. Empty profile arrays
retain their declared dimensions and require an empty sample region. A zero-count
Stamp message retains its array metadata and omits `first`.

Surface mode records expose indexed layouts for Uniform Surface type 8
(signed 16-bit height samples), Surface Intensity type 9 (unsigned 8-bit samples),
and Surface Point Cloud type 28 (interleaved signed 16-bit X/Y/Z words). Each
index gives the sample offset in the GDP archive, dimensions, row stride,
original scale and offset fields, source ID, raw exposure and available signed
stream context. Point clouds also retain
the original adjacency flag. The complete GDP archive preserves invalid sample
codes and every original coordinate word. The supported attribute sizes are
44/52/60 bytes for type 8, 32/40/48 for type 9 and 46/54 for type 28. Other sizes
remain opaque. These layouts are documented in LMI's SDK for G2/G3 sensors and
the Gocator Snapshot manual. Physical surface acquisition remains unverified.

`max_group_bytes` limits the complete group, including message headers. It defaults
to 4 MiB and accepts 6–134217728 bytes (128 MiB). Set it explicitly for larger
surface arrays. The receiver checks declared lengths before allocating or reading
their bodies and keeps its buffer capacity between groups. The message limit is
128. Partial reads survive idle timeouts.
Capture keeps the first terminal-delimited group and sets its sample field
`group_start_confirmed` to false. Each later group has that field set to true
after the preceding group ending establishes the boundary. The header marks
only the end of a group, so the first fragment can be partial even when it has
a valid ending.
A group that ends early, exceeds the limits or remains partial at cutoff fails
the run. Previously completed files and samples remain readable. This framing
is documented in LMI's firmware 5.1.6.134 manual. Physical qualification remains
pending. GoPxL's TCP 3601 protocol requires separate validation.
[LMI Gocator Line Profile Sensors manual, pp. 528 and 572–575](https://lmi3d.com/wp-content/uploads/2018-12/15159-5.1.6.134_MANUAL_User_Gocator_Line_Profile_Sensors.pdf#page=573)
[LMI-authored GoSdk message writer and reader](https://github.com/Logan-Shi/GO_SDK/blob/ff648a46f55b0acbba8fb35337cd1c23e13be777/Gocator/GoSdk/GoSdk/Messages/GoDataTypes.c)
[LMI GoSdk field descriptions](https://github.com/Logan-Shi/GO_SDK/blob/ff648a46f55b0acbba8fb35337cd1c23e13be777/Gocator/GoSdk/GoSdk/Messages/GoDataTypes.h)
[LMI surface receive sample](https://github.com/Logan-Shi/GO_SDK/blob/ff648a46f55b0acbba8fb35337cd1c23e13be777/samples/C/ReceiveSurface/src/ReceiveSurface.c)
[LMI Gocator Snapshot manual](https://lmi3d.com/wp-content/uploads/2020-12/15199-6.0.21.13_MANUAL_User_Gocator_Snapshot_Sensors_EN-US.pdf)
[GoPxL required ports](https://ap.lmi3d.com/manuals/gopxl/gopxl-1.1/LMILaserLineProfiler/Content/GettingStarted/NetworkSetup/RequiredPorts.htm)

## HART reads through a managed serial modem

`hart_serial` reads native HART universal commands through a **Microflex 101-0035 MicroLink-HM** configured as a raw HART modem. Its documented managed mode buffers complete packets and handles HART carrier detect, transmitter keying and primary/secondary timing internally. Turn off Fill Modbus Registers and configure the host port and master role beforehand. Capturefab does not write modem configuration, transmitter polling addresses, ranges, triggers, reset commands or status-clearing commands.

```json
{
  "version":1,
  "sources":[{
  "id":"pressure_loop",
  "protocol":"hart_serial",
  "device":"/dev/ttyUSB0",
  "modem":"microlink_hm_101_0035",
  "master":"secondary",
  "baud":1200,
  "parity":"odd",
  "every_ms":1000,
  "timeout_ms":5000,
  "signals":[
    {"name":"pressure","polling_address":0,"read":"primary_variable","unique_address":"2606bc614e"},
    {"name":"loop","polling_address":0,"read":"loop_current_and_percent"}
  ]
  }]
}
```

Use the actual terminal or Windows COM path and configured instrument address. Host bytes use 8 data bits and 1 stop bit. Baud defaults to 1200 and parity to odd; supported configured host rates are 1200,2400,4800,9600,19200,38400,57600, with odd/even/none parity. The HART loop remains1200 baud odd parity. The documented default host setting is1200 baud odd parity 1 stop. An RS-485 converter must handle its own direction without local echo. The managed modem's raw mode and `master` value must agree; each master role is exclusive to its client. This profile does not implement ordinary RTS-keyed VIATOR/MicroLink/budget USB modem keying, nor claim arbitrary HART adapter support. Connect the managed modem according to its electrical/loop-load manual.

Each source has 1–256 unique signal names. `polling_address` is 0–63, with 0–15 required after identifying a HART 5 device. Address 0 is for a single-device loop. Command 0 discovers and caches each address's identity before any value read. Optional `unique_address` pins its 38-bit identity as ten hexadecimal digits, excluding master/burst bits. This is the address built from command 0 bytes 1/2 and 9–11; HART 7's separate manufacturer field 17/18 is not an address. Raw identity bytes and their original host receive time remain in each value. The adapter supports ordinary asynchronous zero-expansion frames for universal revisions 5–7; older/future revisions and unsupported delimiters fail explicitly.

`read` defaults to`dynamic_variables` (command 3), returning loop current and 1–4 transmitted unit/value pairs. `primary_variable` (command 1) returns one value and unit code. `loop_current_and_percent` (command 2) returns mA and percent-of-range values. Multiple mappings for one address share its cached identity. Conflicting pinned identities are rejected. The adapter sends only commands 0–3, with no retries. Discovery uses 20 preambles; later reads use the declared minimum with a 5-byte floor and 20-byte cap.

Each result is an object with `command`, `response_code`, `command_status`, `device_status`, `raw_reply`, `unique_address`, `polling_address`, `identity` and exact host `received_monotonic_ns`. A numeric field has `ieee754_bits` (exact unsigned32-bit wire representation), `decode_status`, and a finite decoded `value` when available. Transmitted engineering units remain exact `unit_code` integers; no guessed conversion or device-specific meaning is applied. Non-finite IEEE payloads, including HART's unsupported-variable NaN, keep their original bits and omit `value`. Command warning 8 is retained as `command_status:"update_failure"`. A command success is not a declaration of good process quality: the raw device-status byte is preserved even when nonzero. Communication/command errors fail the poll with raw HART details.

Every complete reply's XOR, byte count, delimiter and correlation fields are checked. Valid foreign-master/device and burst frames are discarded under the original deadline and a 64 KiB per-query input budget. A normal response from a device configured for burst mode can set its address burst flag; it is distinguished from a burst notification by the delimiter. Own-device wrong-command replies fail. HART has no transaction ID; one request is outstanding and any error permanently retires the source's port. This avoids retrying an incomplete transaction; it does not provide a freshness guarantee across a later new recording or arbitrary modem replay. All reads, including identity, share one total poll deadline, and no earlier sibling is published when a later mapping fails. Several queries are not a device snapshot. First acquisition and multiple devices can take seconds at 1200 baud; size timeout and recording duration accordingly.

Evidence is protocol/PTY qualified, without physical-device validation. The published Fisher-Rosemount 3051C transaction returns 5.5 PSI; other fixtures derive frames from documented layouts. Sources: [MicroLink-HM 101-0035 manual 620-0021 R3, pp. 1–2,5,15](https://cdn.shopify.com/s/files/1/0555/4461/files/101-0035_Manual_Rev_3.pdf?v=1761163074), [Fisher-Rosemount HART Technical Overview, second edition, chapters 3–4](https://www.documentation.emersonprocess.com/groups/public/documents/book/ag13457685.pdf?xBusinessUnit=Fisher+Controls+Valves+Division), [ABB FEX300/FEX500 command layouts](https://library.e.abb.com/public/9d09d9d99c97d0acc12577c000517b01/COM_FEX300_FEX500_HART-EN-A-09_2010.pdf), [FieldComm HART revision comparison](https://dev-support.fieldcommgroup.org/support/solutions/articles/8000048834-hart-5-7-comparisons).

## Livox SDK2 and legacy UDP groups

`livox_udp` passively receives an already configured Livox stream. An external
owner must enable sampling, choose destinations/return format and maintain any
required heartbeat. Capturefab sends no discovery, handshake, control, clock or
heartbeat command. Select the documented generation/model and `points` or `imu`
explicitly; a configured profile and source-IP/port filter are not authenticated
device identity. Unicast and supported multicast require explicit network setup.

```json
{"version":1,"sources":[{"id":"lidar","protocol":"livox_udp",
  "profile":"sdk2_mid360_v0","stream":"points",
  "listen":"0.0.0.0:56301","source_ip":"192.0.2.30","source_port":56300,
  "output":"mid360.livox","max_bytes":104857600,"packets_per_group":32,
  "every_ms":100,"timeout_ms":120}]}
```

The ports above are an externally configured example, not discovered defaults.
`output` is a literal local no-overwrite archive; `max_bytes` must fit its context
and a maximum-size selected packet. Optional `min_free_bytes` supplements the
storage floor. Optional `context` is an exact opaque file of at most 1 MiB,
including empty, with `configured_unverified` provenance; it is not calibration or
model proof. Optional multicast needs `listen:"0.0.0.0:port"` and explicit group
and interface. Mid360L multicast is refused.

| Profile | Points stream | IMU stream |
| --- | --- | --- |
| `sdk2_mid360_v0`, `sdk2_mid360s_v0` | Types 1/2 Cartesian i32/i16 and 3 spherical; 96 records | Type 0, one 24-byte record |
| `sdk2_mid360l_v0` | Types 1/2 only | Type 0 |
| `sdk2_hap_tx_v0` | Types 1/2 only | Type 0 |
| `sdk2_hap_t1_v0` | Types 1/2 only | Unsupported |
| `legacy_horizon_v5` | Types 2–5, single/dual returns | Type 6 |
| `legacy_avia_v5` | Types 2–5 and 7/8 triple returns | Type 6 |

SDK2 has a 36-byte prefix and mandatory CRC32 over **timestamp plus data only**:
bytes 28–35 and the following native records. Bytes 0–23 and the stored CRC word
at 24–27 are outside that checksum. Legacy uses an 18-byte prefix with no sample
CRC. Wrong generation/layout/model/stream and malformed selected packets fail
explicitly. Other SDK enums, including SDK2 Avia2/double echo, are not inferred.

Each `livox_udp_group` retains dense `datagrams` with `profile`, `generation`,
`stream`, `packet_version_raw`, `data_type_raw`, `header_base64`, `checksum_status`,
`checksum_scope`, `clock` and `native`. Cartesian `x_raw`/`y_raw`/`z_raw`,
`reflectivity`/`tag_raw` keep record/return-slot grouping; `coordinate_unit_mm`
is 1 for i32 and 10 for SDK2 i16. Spherical depths/centidegree angles stay native;
IMU retains gyro rad/s, acceleration g, `gyro_raw_words`/`acceleration_raw_words` and
finite/nonfinite/sign metadata (`classification`, `sign_bit`, optional `value`). Nonfinite values are not converted to JSON NaN or zero. Unknown
status/tag/reserved bytes remain raw. Mid360 tag-selector semantics remain
unqualified; HAP reported trust bits do not independently establish safety.

Hardware clock words/types, signed legacy PPS offsets and raw calendar fields
remain inside each packet. They do not prove UTC, current sync, freshness,
trajectory or whole scans. No group `source_time` or per-point timestamps are
invented; original host receive clocks remain authoritative for recorder timing.

Groups preserve raw payload references (`raw_offset`, `raw_length`), packet
indices, peer endpoints and receive clocks. Context, raw groups and checksummed
commit footers are synced before high-water marks advance. Exact serialized
reading admission leaves 16 KiB for the recorder wrapper; normal storage admission splits large groups
without dropping an accepted overflow packet. Cutoff/finish drains accepted
owned data. Failure retires reception; no control retry or overwrite occurs.

Final `livox_udp_file` and `error.details.livox_udp` expose `committed_bytes`,
`sha256_committed_prefix`, `groups`, `packets`, `uncommitted_tail_bytes`,
`uncommitted_packets` and `pending_accepted_packets`. `archive_committed_prefix_only`
and `jsonl_publication_not_proven` distinguish disk commitment from recorder
publication. Prior committed groups remain usable after a later failure.
Normal JSON/JSONL/CSV exports retain the native values and artifact references.

Primary formats: [Mid360 protocol](https://livox-wiki-en.readthedocs.io/en/latest/tutorials/new_product/mid360/livox_eth_protocol_mid360.html),
[HAP protocol](https://github.com/Livox-SDK/Livox-SDK2/wiki/Livox-SDK-Communication-Protocol-HAP%28English%29),
[legacy V1.3.0 protocol revision](https://github.com/Livox-SDK/Livox-SDK/wiki/Livox-SDK-Communication-Protocol/3e9ef1d90391a038c6a59dfe154fab65df31e915),
and the manufacturer's [SDK2 packed header](https://github.com/Livox-SDK/Livox-SDK2/blob/c0796f04c143143899c87a773d9f6b7136453c0b/include/livox_lidar_def.h).
Source/manual qualification does not claim physical sensor validation.


## RoboSense Helios-16P and RS16 UDP groups

`robosense_udp` passively receives an already configured Helios-16P or RS16
stream. Choose one explicit packet profile per source; configure the device's
destination separately. Reception sends no discovery, UCWP, configuration or
trigger command. The source IP/optional source port filters packets but does not
authenticate the device or verify its serial number.

| Profile | Selected 1248-byte UDP payload |
| --- | --- |
| `helios16p_msop` | Helios-16P measurement packets, twelve blocks of 32 channel entries |
| `helios16p_difop` | Helios-16P device information and raw 16-channel calibration fields |
| `rs16_msop` | RS16 measurement packets, twelve blocks of 32 channel entries |
| `rs16_difop` | RS16 device information and raw 16-channel vertical calibration |

```json
{
  "version": 1,
  "sources": [{
    "id": "helios",
    "protocol": "robosense_udp",
    "profile": "helios16p_msop",
    "listen": "0.0.0.0:6699",
    "source_ip": "192.0.2.62",
    "output": "helios.robosense",
    "max_bytes": 1073741824,
    "packets_per_group": 32,
    "context": "calibration.csv"
  }, {
    "id": "helios_info",
    "protocol": "robosense_udp",
    "profile": "helios16p_difop",
    "listen": "0.0.0.0:7788",
    "source_ip": "192.0.2.62",
    "output": "helios-info.robosense",
    "max_bytes": 1048576,
    "packets_per_group": 1
  }]
}
```

Run `capturefab data record --config robosense.json --for 10s -o run.jsonl`.
Use different listener/output paths for MSOP and DIFOP. A source becomes ready
after a valid selected packet arrives; omit a source for a stream the device is
not transmitting. Optional `source_port` pins the sender's nonzero port, distinct
from the listener's destination port. Optional `multicast` requires an explicit
`group` and local unicast `interface`, with `listen` set to `0.0.0.0:PORT`.

Optional `context` archives up to 1 MiB of exact opaque bytes once, with SHA-256.
CSV, JSON, binary and an explicitly supplied empty file remain distinct from
absent context. Context is `configured_unverified`; it does not prove identity,
calibration validity or synchronization. Existing output files are never replaced.

MSOP admission checks exact length, the selected header, all twelve `FF EE`
blocks and the `00 FF` tail. Helios requires protocol `00 01` and either the
older shared `07` identity at zero-based offset 31, or type `06`/model `03` at
offsets 31/32. The older marker also covers Helios-1615 and is labeled
`legacy_shared_family`; configured profile and peer remain part of qualification.
The RS16 marker is `55 AA 05 0A 5A A5 50 A0`, with model `01` at offset 30.
Neighboring reserved bytes are preserved without alternate-offset detection.

Every MSOP packet retains all 384 distance and reflectivity entries, raw azimuths
in hundredths of a degree, the exact header and reserved tail bytes. Nonnominal
azimuths keep their raw values and an explicit range state. Helios resolution flag
0 means 5000 micrometres per distance count, and 1 means 2500; unknown flags
retain raw distances without a scaled array. RS16 distance units depend on
firmware. By default its `unit_status` is `unavailable_firmware_dependent` and
there is no scaled array. Only `rs16_msop` accepts optional
`rs16_distance_unit_um: 5000` or `10000`, producing exact integer `distance_um`
with `configured_unverified` unit status. The declaration is also stored in the
raw archive. No SDK default supplies this setting.

DIFOP admission checks its distinct `A5 FF 00 5A 11 11 55 55` header and `0F F0`
tail. It retains every data byte, raw return/synchronization modes, serial,
firmware, network, diagnostics, GPRMC and calibration fields. Unknown modes and
calibration signs remain raw. Helios return mode 6 has conflicting manual/SDK
labels, both retained with `unresolved_vendor_documentation`. DIFOP has no
equivalent MSOP model discriminator. Separately received DIFOP packets are not
associated with an MSOP packet or used to infer its return mode or calibration.
No XYZ, filtering, destaggering, inferred scan boundary or return ranking occurs.

Clock bytes always remain exact. RS16 calendar and millisecond/microsecond words
are retained as raw fields, with reserved-bit and calendar status. A valid
calendar with zero reserved bits can additionally have an unzoned formatted
value; unknown reserved bits or invalid dates/fractions withhold that value.
Helios keeps its 48-bit seconds and 32-bit fractional word separately. The newer `06`/`03` header qualifies documented microseconds;
the older header and DIFOP retain the conflicting vendor unit descriptions.
Invalid fractions remain explicit. Neither stream adds `source_time`, converts
an epoch, or infers UTC lock or a sensor-to-host clock mapping.

These payloads have no application checksum. `no_checksum_in_format` does not
verify Ethernet FCS, sensor identity or authenticity. Archive hashes verify
stored bytes. Passive UDP cannot recover network or kernel packet loss.

A `robosense_udp_group` contains native datagrams, original host receive clocks
and source sockets, absolute raw offsets/lengths and an immutable
`{path,offset,length,sha256}` range after syncing. Groups are bounded batches:
32 datagrams, 12288 decoded entries, 4 MiB raw and the exact serialized Reading
budget, with 16 KiB reserved below the recorder's 1 MiB row limit. An accepted
overflow packet retains its original clock for the next group or healthy cutoff
drain. Foreign traffic is bounded under the original read deadline. `max_bytes`
includes context and all archive framing; the storage free-space floor applies.

Final `robosense_udp_file` observations and `error.details.robosense_udp` expose
the committed high-water mark, prefix hash, group/packet counts, physical tail
and pending/uncommitted counts. Failed append/sync/rollback never advances that
mark, and a failed source never retries pending writes during finalization.
Archives with no committed group are removed. `archive_committed_prefix_only`
and `jsonl_publication_not_proven` distinguish archive durability from delivery
to the separate JSONL writer. Readers must honor the reported high-water mark.

The version 1 archive has an 80-byte `CFROBO1\0` header, optional exact context,
32-byte `CFRGRP1\0` group headers, 32-byte `PKT1` record headers with unchanged
UDP payloads, and 56-byte `CFRCMT1\0` footers. Integers are little-endian.
Header profile IDs 1–4 follow the table order above; context flag bit 0
distinguishes supplied empty context. Header bytes 72–75 store the configured
RS16 distance unit as u32 (zero means absent); bytes 76–79 are reserved zero.
The footer hashes the group header and records; the published range hashes the
whole group including the footer. JSONL/CSV exports preserve exact references.

Primary formats: [Helios-16P English v1.0.1, §§6.2–6.3](https://cdn.robosense.cn/20220629181832_42540.pdf),
[Helios Chinese v1.0.2, Table 9 and Appendix B.13](https://cdn.robosense.cn/20220923161259_62013.pdf),
and [RS16 v4.3.3, §5 and Appendix B.10](https://cdn.robosense.cn/20200723161607_95470.pdf).
The manufacturer [rs_driver at 897b14d3](https://github.com/RoboSense-LiDAR/rs_driver/tree/897b14d3bdb6186a75df27ba51b65b5bd5557723)
provides an independent raw-structure reference. Its generic model, default
distance unit, clock conversion and 32-channel calibration handling do not expand
these selected formats. Capturefab requires no SDK runtime.

## Hesai PandarXT32 lidar and GPS UDP groups

`hesai_udp` passively receives an already configured PandarXT32 stream. Select the
packet profile explicitly; Capturefab sends no discovery, PTC, configuration or
trigger command. These profiles do not identify or authenticate a device serial.
Use an exact source IP and, when known, an exact source port.

| Profile | Selected UDP payload |
| --- | --- |
| `pandar_xt32_v6_1` | 1080-byte protocol 6.1 lidar packets, eight blocks of 32 channels |
| `pandar_xt32_gps` | 512-byte companion GPS packets with original time/NMEA bytes and status |

```json
{
  "version": 1,
  "sources": [{
    "id": "xt32",
    "protocol": "hesai_udp",
    "profile": "pandar_xt32_v6_1",
    "listen": "0.0.0.0:2368",
    "source_ip": "192.0.2.61",
    "output": "xt32.hesai",
    "max_bytes": 1073741824,
    "packets_per_group": 16,
    "context": "angle-correction.csv"
  }, {
    "id": "xt32_gps",
    "protocol": "hesai_udp",
    "profile": "pandar_xt32_gps",
    "listen": "0.0.0.0:10110",
    "source_ip": "192.0.2.61",
    "output": "xt32-gps.hesai",
    "max_bytes": 1048576,
    "packets_per_group": 1
  }]
}
```

Run `capturefab data record --config hesai.json --for 10s -o run.jsonl`.
Configure the sensor's destination addresses/ports separately. The listener port
is the destination port; optional `source_port` pins the sender's nonzero port.
Use separate listener/output paths for lidar and GPS. The GPS source requires an
active GPS UDP stream: this model does not send those packets when configured to
use PTP. Omit that source when it is not transmitting. A source must receive a
valid selected packet before it becomes ready.

Optional `context` archives up to 1 MiB of exact opaque bytes once, with SHA-256.
CSV calibration, JSON, binary and an explicitly supplied empty file remain
distinct from no context. Context is `configured_unverified`; it does not prove
identity, calibration validity or clock synchronization. Native measurements do
not require a calibration file. Optional `multicast` requires an explicit
`group` and local unicast `interface`, with `listen` set to `0.0.0.0:PORT`; no
interface is guessed. Existing files are never replaced.

Lidar admission checks exact length, `EE FF`, version 6.1, 32 channels, eight
blocks, 4 mm distance units, maximum two returns, sequence-present bit 0 and
factory byte `0x42`. Reserved bytes/bits remain opaque. All six documented return
modes are retained: first, strongest, last, last/strongest, last/first and
first/strongest. Adjacent blocks in dual mode must have equal raw azimuth. Blocks
stay in wire order, including identical pairs. A paired second block can contain
the second-strongest return; no unconditional return ranking is attached to it.
Other selected Hesai models and layouts fail explicitly.

Each lidar datagram exposes all 256 raw distances, exact integer millimetres
(`distance_raw * 4`), reflectivity codes and fourth channel bytes, plus block
indices and raw azimuths in hundredths of a degree. Nonnominal azimuth values stay
raw with an explicit range state. Reflectivity mapping is configurable, so no
physical reflectance is inferred. The manufacturer manual reserves the fourth
channel byte and first ten tail bytes; broader SDK confidence/status names are
not assigned to this profile. No XYZ, deskewing, inferred scan boundary or return
deduplication is performed.

Raw return mode, motor RPM, factory byte, sequence word and calendar/microsecond
fields remain separate. Invalid calendar or fractional values retain their bytes
and explicit application-state labels. A valid calendar may additionally have an
unzoned formatted value. Sequence observations report the previous word and
modulo delta, distinguishing equal/next/forward-or-gap/backward-or-large-forward;
they do not prove a loss count or distinguish every restart from reordering.

The GPS profile requires exact length and its distinct `FF EE` header. It retains
the six date ASCII bytes, six time ASCII bytes, all 84 NMEA bytes, positioning
status, PPS lock byte and every reserved byte. Unknown PPS values and unexpected
reserved padding remain explicit raw observations. Date bytes are not reversed
or expanded into an inferred century, and raw NMEA is not a decoded position or
validated sentence. Lidar/GPS observations never add `source_time` or infer a
sensor-to-host clock mapping. Startup time can be virtual, lost synchronization
can drift, and PTP output can depend on the master's configured offset. PPS lock
alone does not establish a valid calendar or synchronization between the streams.

These UDP payloads have no application CRC. `checksum_status` says
`no_checksum_in_format`; Ethernet FCS is not part of UDP socket data. Archive
hashes verify stored bytes, not sensor identity or on-wire authenticity. Passive
reception cannot recover network/kernel packet loss.

A `hesai_udp_group` observation contains native datagrams, original host receive
clocks/source sockets, absolute raw offsets/lengths and an immutable
`{path,offset,length,sha256}` reference after the group is synced. Supplied context
has its own exact reference. Groups are batches, not complete rotations. Limits
are 32 datagrams, 8192 decoded elements, 4 MiB raw and the exact serialized Reading
budget, with 16 KiB reserved below the recorder's 1 MiB row limit. An accepted
overflow packet stays owned with its original clock for the next group or healthy
cutoff drain. Foreign traffic has bounded allowances under the original read
deadline. `max_bytes` includes archive/context/framing bytes; the normal storage
free-space floor also applies.

Final `hesai_udp_file` observations and `error.details.hesai_udp` expose the
archive's committed high-water mark, prefix hash, group/packet counts, physical
tail length, foreign traffic and pending/uncommitted packet counts. Failed
append/sync/rollback never advances the advertised committed prefix; a failed
source does not retry pending writes during finalization. Files with no committed
group are removed. `archive_committed_prefix_only` and
`jsonl_publication_not_proven` distinguish raw archive commits from delivery to
the separate JSONL writer. Readers must honor that high-water mark even if a
physical tail appears structurally complete.

The distinct version 1 archive has an 80-byte `CFHESA1\0` header, optional exact
context bytes, and groups with a 32-byte `CFHGRP1\0` header, 32-byte `PKT1` record
headers plus unchanged UDP payloads, and a 56-byte `CFHCMT1\0` footer. Integers are
little-endian. Header profile IDs are 1 for lidar and 2 for GPS; context flag bit 0
distinguishes a supplied empty file from absent context. Group and record framing
carry sequence/count/extents and original receive clocks/source sockets. The
footer hashes the group header and records; the published group reference hashes
the whole group including the footer.

Primary format: [PandarXT X01-en-250220 manual, §§3.1–3.2 and Appendix B](https://www.hesaitech.com/wp-content/uploads/2025/02/PandarXT_User_Manual_X01-en-250220.pdf).
The manufacturer [SDK at fbcd94a1](https://github.com/HesaiTechnology/HesaiLidar_SDK_2.0/tree/fbcd94a1efb7af0133f9512e14afdde85e5bc06b)
provides an independent reference for raw lidar accessors. Its broader model,
reserved-byte and epoch-conversion behavior does not expand this selected wire
contract. No SDK runtime is required by Capturefab.

## Ouster OS0/OS1/OS2 UDP groups

`ouster_udp` listens for an already configured Ouster lidar or IMU stream. Supply
the sensor's original metadata JSON, exact source IP and an explicit local listener.
Capturefab sends no configuration or discovery packets. One source selects either
`lidar` or `imu`; a group is a bounded batch of datagrams, not a complete scan.

```json
{
  "version": 1,
  "sources": [{
    "id": "ouster",
    "protocol": "ouster_udp",
    "listen": "0.0.0.0:7502",
    "source_ip": "192.0.2.60",
    "stream": "lidar",
    "metadata": "sensor-metadata.json",
    "header_type": "STANDARD",
    "output": "lidar.ouster",
    "max_bytes": 1073741824,
    "packets_per_group": 8
  }]
}
```

Run `capturefab data record --config ouster.json --for 10s -o run.jsonl`.
`source_port` optionally pins a nonzero sender port; omitting it accepts any port
at the configured source IP. This is distinct from the destination/listen port.
For multicast, explicitly add `"multicast":{"group":"239.1.2.3","interface":"192.0.2.10"}`
and set `listen` to `0.0.0.0:PORT`. Capturefab does not guess an
interface or share one bound port between source entries. A sensor already sending
both streams to one port can use the declared other stream's qualified framing for
filtering; `filtered_other_packets` and the validation level remain explicit.

Supported lidar profiles are `RNG19_RFL8_SIG16_NIR16`, its `_DUAL` variant,
`RNG15_RFL8_NIR8`, `RNG15_RFL8_NIR8_DUAL`, and `LEGACY`. These use 16 columns
per datagram, 32/64/128 channels and frame widths 512/1024/2048; LEGACY also
accepts 16 channels. Modern packets require `STANDARD` headers, matching 40-bit
serial and 24-bit initialization ID. A changed initialization ID ends the metadata
epoch explicitly. Unsupported selected header/profile layouts fail before acquisition.

Metadata may use the documented nested or older flat layout. Original bytes,
including calibration, clock, return and unknown fields, are archived once with
their SHA-256. No calibration or SDK defaults are synthesized. Optional `profile`,
`header_type`, `pixels_per_column`, `columns_per_packet` and `columns_per_frame`
declarations can supply missing facts; contradictions fail. LEGACY's fixed 16
columns may come from the wire definition, but missing height/width must be
declared. IMU sources do not accept lidar dimension declarations. Metadata is
bounded to 1 MiB and must have no duplicate JSON keys.

Lidar observations retain every column, timestamp, measurement ID, status and
native range/reflectivity/signal/NIR channel that exists in the selected profile,
including both returns. Low-rate ranges scale exactly to millimetres by 8 and NIR
by 16; raw codes remain separate. Invalid or unknown-status columns remain dense,
with validity and bounds stated explicitly. No XYZ, destaggering, deskewing,
return ranking or sensor-to-host time conversion is performed. Raw timestamps
remain unsigned sensor nanoseconds with `clock_basis:"unspecified"`; `source_time`
is absent. Reported timestamp mode and lock bits do not establish UTC or hardware
synchronization.

CRC is mandatory where defined. Literal firmware 2.2–2.4 and 3.0 qualify the older
reserved footer; its full bytes are retained and `checksum_status` states that a
checksum is unavailable. Firmware 2.5+ and 3.1+, and unrecognized build versions,
require CRC64. A zero footer never disables validation. Version-dependent header
status fields are interpreted only when qualified; remaining bytes stay raw.
LEGACY has neither CRC nor packet identity/type, so exact metadata and peer
filtering are essential. Its 48-byte IMU framing cannot distinguish genuine IMU
data from a deliberately shortened lidar datagram of the same length and peer.

For IMU, set `"stream":"imu"`, its destination listener and a separate output.
`LEGACY` retains all three timestamps and exact IEEE-754 accelerometer/gyro words
in `acceleration_g` and `angular_velocity_dps`. `ACCEL32_GYRO32_NMEA` retains its
declared number of measurements, timestamps, raw correlation word, proved column
and validity bits, and all 88 NMEA bytes plus raw lock status. Frame correlation
decoding remains unavailable. Synchronous measurements use `acceleration_wire`
and `angular_velocity_wire`, with `units_status:"unresolved_vendor_documentation"`:
the firmware manuals state g/degrees per second while the official SDK treats
these wire values as SI. Both interpretations are reported without choosing or
converting either. Each float keeps its raw word and finite/NaN/infinity status;
only finite values additionally have an exact f32-to-f64 numeric value.

An `ouster_udp_group` observation includes decoded datagrams, their original host
receive clocks and source sockets, absolute raw offsets/lengths, and immutable
`{path,offset,length,sha256}` references for its synced group and original metadata.
Admission limits are 32 packets, 4 MiB raw, 16,384 decoded elements and the exact
1 MiB recording-row budget (the complete Reading is capped 16 KiB below it for
the recorder wrapper). A received packet that exceeds a nonempty group stays
owned with its original clock for the next group or final drain. Foreign and
unselected traffic have bounded allowances under the original read deadline.
UDP/network/kernel packet loss cannot be recovered by this passive listener.

The final `ouster_udp_file` and `error.details.ouster_udp` report `committed_bytes`,
group/packet counts, `sha256_committed_prefix`, physical file/tail lengths and
pending/uncommitted counts. Honor the advertised committed prefix; a structurally
complete physical tail is not proof of durability. `archive_committed_prefix_only`
and `jsonl_publication_not_proven` distinguish archive commits from delivery to
the separate JSONL writer. A later storage or malformed-packet failure preserves
earlier committed groups. An owned file with no committed group is removed;
existing files are never replaced. `max_bytes` and the normal free-space floor
apply to the archive, including metadata and framing.

The version 1 archive uses little-endian integers: an 80-byte `CFOUST1\0` header
and exact metadata bytes, then groups consisting of a 32-byte `CFOGRP1\0` header,
32-byte `PKT1` record headers plus unchanged UDP payloads, and a 56-byte
`CFOCMT1\0` footer. Group headers carry sequence/count/record extent/first packet
index; each record carries length/index/receive clock/source IP/port. The commit
footer hashes the group header and records; the observation hashes the whole
group including the footer. Readers must validate lengths, counts, reserved bytes,
digests and the advertised high-water mark.

Primary formats: [lidar](https://docs.ouster.com/sensor-docs/firmware/3.2/lidar-data),
[LEGACY lidar](https://docs.ouster.com/sensor-docs/firmware/3.2/lidar-data-legacy),
[IMU](https://docs.ouster.com/sensor-docs/firmware/3.2/imu-data), and
[LEGACY IMU](https://docs.ouster.com/sensor-docs/firmware/3.2/imu-data-legacy).
The [2.2 manual, p. 23](https://data.ouster.io/downloads/software-user-manual/software-user-manual-v2.2.x.pdf#page=23)
shows the reserved footer. CRC introduction is documented separately in the
[2.5 changelog](https://static.ouster.dev/sensor-docs/image_route1/image_route2/firmware_changelog/firmware_changelog.html)
and [3.1 changelog](https://docs.ouster.com/sensor-docs/firmware/3.2/changelog/2024/5/1).
The synchronous unit conflict is visible in the IMU wire manual and
[SDK ImuPacket API](https://docs.ouster.com/sdk-docs/cpp/api_cpp/structouster_1_1sdk_1_1core_1_1ImuPacket.html).
The official [SDK at 46b7bcee](https://github.com/ouster-lidar/ouster-sdk/tree/46b7bceee7995914e2f3f3facf4d78418335b2d7)
is an independent comparison reference, not a runtime dependency.

## Velodyne VLP-16 raw UDP groups

`velodyne_vlp16` passively receives the documented 1206-byte data packet format from a configured IPv4 source. This first profile accepts modern VLP-16/Puck LITE product byte `0x22`, with strongest `0x37`, last `0x38` or dual `0x39` returns. Configure the sensor's destination address/port separately; Capturefab sends no packet, trigger or configuration command. Broadcast reception uses an unconnected wildcard listener plus exact source IP/port filtering. Factory data destination port is 2368.

```json
{
  "version": 1,
  "sources": [{
    "id": "lidar",
    "protocol": "velodyne_vlp16",
    "listen": "0.0.0.0:2368",
    "source": "192.168.1.201:2368",
    "output": "lidar.vlp16",
    "max_bytes": 1e9,
    "packets_per_group": 128,
    "timeout_ms": 250,
    "signal": "datagrams"
  }]
}
```

`listen`, `source`, literal local `output` and `max_bytes` are required. Ports must be nonzero; the source must be an exact unicast IPv4 endpoint. `listen` may use `0.0.0.0`. `packets_per_group` defaults to 128 and permits 1..128; it is a maximum, not a sensor-frame size. `max_bytes` includes every file/group/record/footer byte and must be at least 1366. Optional `min_free_bytes` raises the shared storage free-space floor. All typed counts accept exact integral JSON decimal/exponent notation. The default signal is `datagrams`. Passive acquisition ignores `every_ms`.

The adapter validates each packet's length, twelve FFEE flags, azimuth range, raw hourly timestamp, factory mode/product and dual-return paired azimuths. Original distances and reflectivities, including zero or maximum raw words, stay unchanged. Duplicate and out-of-order packets remain in host receive order. No source sequence number exists in this packet format, so the host packet index is not a sensor sequence or packet-loss counter. This profile makes no lossless UDP claim. Foreign-source traffic has a per-read bound; excessive traffic fails explicitly. An oversized datagram is rejected rather than decoded from a truncated prefix. Operating systems that report truncation as a socket error may terminate before exposing that packet's source endpoint.

Each accepted packet retains its exact host receive monotonic nanoseconds in the archive before later packet reception or filesystem work. The sensor timestamp remains an unsigned microseconds-since-hour raw counter with unknown epoch and synchronization. No UTC, point firing clock, XYZ, beam calibration, rotation completion or hardware synchronization is inferred. Position/telemetry port 8308 and old product-byte conventions are outside this profile. Support is protocol/fixture verified; physical sensor qualification remains pending.

One new container holds the run's raw packets. It never replaces an existing file and is created only when the first valid group is ready to write. There are no per-packet files or run-sized observation lists. At most 128 packets are held in one ~155 KiB group. A group flushes when full, at its bounded 100 ms batching window, or at an idle/read boundary. A configured-peer packet received inside acquisition is included before an expired batching window ends the group; only packets whose host receive clock is past the actual acquisition cutoff are discarded. Network operations use the original read deadline; synchronous filesystem flushes retain the shared storage layer's operating-system limitations.

A `velodyne_vlp16_group` observation references an exact immutable `{path,offset,length,sha256}` range after its bytes and the first group's directory entry have been synced. It contains group number, packet count, host-assigned first/last packet indices, first/last host receive clocks, first/last raw sensor timestamps and unique raw return modes. Every individual receive clock and exact datagram is recoverable inside that range. JSON/CSV exports retain these references and integer metadata; raw packet bytes live only in the container.

A final `velodyne_vlp16_file` observation reports `committed_bytes`, `groups`, `packets`, `sha256_committed_prefix`, `file_bytes`, `unpublished_tail_bytes`, `storage_finalized` and the last group's small descriptor. Source errors carry the same committed prefix in `error.details.velodyne_vlp16`. A later malformed packet discards the current uncommitted group and stops acquisition. A later append/storage failure retains earlier committed ranges; a file with no committed groups is removed. Hitting `max_bytes` stops explicitly with the committed prefix retained. Readers must honor the reported `committed_bytes` high-water mark or previously published group ranges. If append sync and rollback truncation both fail, physical bytes beyond that mark can include a structurally complete unadvertised group. A footer/hash scan is only structural recovery and cannot prove publication or successful durability. `storage_finalized` describes the final storage close/sync, not complete acquisition or publication of a tail. Whole-run status remains the time-series footer.

The version 1 container stores all local metadata integers little-endian:

| Record | Fields in byte order |
| --- | --- |
| File header, 40 bytes | `CFVLP16\0` for VLP or `CFHDL32E` for HDL (8), version:u16=1, header_bytes:u16=40, udp_payload_bytes:u32=1206, source_ipv4 (4), source_port:u16, listen_port:u16, source_open_monotonic_ns:u64, reserved zero (8) |
| Group header, 32 bytes | `CFVGRP1\0` (8), group:u64, record_count:u32, record_bytes:u32, first_host_packet_index:u64 |
| Packet record, 1238 bytes | `PKT1` (4), payload_bytes:u32=1206, host_packet_index:u64, received_monotonic_ns:u64, sensor_timestamp_us_raw:u32, return_mode:u8, product:u8, reserved_zero:u16, exact UDP payload (1206) |
| Group footer, 56 bytes | `CFVCMT1\0` (8), group:u64, group_header_plus_records_bytes:u64, SHA256 of group header and records (32) |

The observation's group SHA256 includes its footer. The final prefix SHA256 covers the file header and every committed group. Raw UDP bytes keep their manufacturer's original encoding; the file header's integer byte order does not rewrite them. Structural readers must validate version, lengths, counts, reserved fields and hashes before trusting any range, and must never read beyond the advertised prefix when processing a published capture.

Primary format: [Velodyne VLP-16 User Manual Rev F, §9.3.1.7–9.3.2, pp. 60–64](https://data.ouster.io/downloads/velodyne/user-manual/vlp-16-user-manual-revf.pdf). The independently maintained [ROS Velodyne driver at 8cdc0f37](https://github.com/ros-drivers/velodyne/tree/8cdc0f37c8d6df1614bdbc424dd09f40a3c29251) corroborates fixed packet length, FFEE blocks, little-endian fields and configured-source reception. Its XYZ/calibration/GPS conversion is not part of this adapter.

## HDL-32E raw UDP profile

`velodyne_hdl32e` is an explicit read-only profile for the HDL-32E packet layout with documented factory bytes (firmware 2.2.20.0 and later). Its required/optional config and committed-prefix storage policy are the same as `velodyne_vlp16`. It requires product `0x21`; the existing VLP-16/Puck LITE profile still requires `0x22` and does not autodetect or fall back.

```json
{
  "version": 1,
  "sources": [{
    "id": "lidar32",
    "protocol": "velodyne_hdl32e",
    "listen": "0.0.0.0:2368",
    "source": "192.168.1.200:2368",
    "output": "lidar.hdl32e",
    "max_bytes": 1e9,
    "packets_per_group": 128,
    "timeout_ms": 250
  }]
}
```

Use the sensor's actual source endpoint; these network addresses are illustrative. Set its destination separately. Capturefab sends no discovery, trigger or configuration command. The HDL profile preserves every 1206-byte datagram, all 32 raw channel records per block, all 12 blocks, return mode `0x37`/`0x38`/`0x39`, exact host receive clocks and the raw hourly timestamp. Consecutive dual-return blocks must share their azimuth. Neither XYZ/calibration nor per-laser firing times, UTC epoch, physical model identity, packet loss or full rotations are inferred. Older blank/status-byte firmware and position/telemetry packets are outside this profile.

HDL containers use the exact 8-byte magic `CFHDL32E`. The 40-byte file header, 1238-byte packet records, generic Velodyne group/footer layouts and checksum/high-water-mark rules stay the same. Group/file observation kinds are `velodyne_hdl32e_group` and `velodyne_hdl32e_file`; HDL metadata adds selected `model_profile:"hdl32e"` and `product_id_raw:33`. Error prefix metadata uses `error.details.velodyne_hdl32e`. Existing VLP magic/kinds/fields stay unchanged. File extensions are examples, not model detectors. CSV/JSONL preserve the exact references and integer fields.

Primary: [HDL-32E User Manual 63-9113 Rev.M, §9.3.1–9.3.2, pp58–62](https://ustc-flicar.github.io/pdf/MANUAL_USERS_HDL32E.pdf), including Fig. 9-3's same-azimuth 32-channel block pairs. This is a university-hosted copy of the manufacturer-authored manual. The unchanged independently captured product21 packet is positive wire-format evidence under the selected profile and remains a VLP rejection control. Its catalogue filename is not treated as physical sensor identity. Hardware qualification remains pending.

## SCPI measurement instruments

The native `scpi` source sends configured queries over raw TCP, VXI-11 core RPC,
HiSLIP 1.0, USBTMC/USB488 or a native serial port. One source owns its connection and
polls its query mappings sequentially. It does not require VISA, a vendor SDK or an
interpreter. Use the instrument's documented interface settings and choose the
matching transport. USBTMC uses the `usb` feature and externally provisioned USB
access; Telnet is not implemented by this adapter.

Each source accepts 1–256 uniquely named query mappings. TCP addresses are numeric
IP:port values. Serial defaults are 9600 baud, no parity, 8 data bits and one stop
bit, with flow control disabled; provision the matching instrument interface first.

```json
{
  "version": 1,
  "sources": [{
    "id": "bench_meter",
    "protocol": "scpi",
    "connection": {"transport": "tcp", "address": "192.0.2.40:5025"},
    "every_ms": 1000,
    "timeout_ms": 5000,
    "signals": [
      {"name": "voltage", "query": "MEASure:VOLTage:DC?", "format": "number", "unit": "V"},
      {"name": "errors", "query": "SYSTem:ERRor?", "format": "error_queue"}
    ]
  }]
}
```

A serial connection uses `{"transport":"serial","device":"/dev/ttyUSB0","baud":9600,
"parity":"none","data_bits":8,"stop_bits":1}` (a Windows COM path is also supported).
Configure all queries for the same serial port in one source. `write_terminator` defaults
to `lf`; `crlf` and `cr` are available. Response terminators are independently configured
per signal, with `lf` as the default. Binary blocks can explicitly select `none` when the
instrument sends no terminator. VXI-11, HiSLIP and USBTMC additionally permit `none` for writes
and line replies because they provide an explicit END boundary.

`format` is `number` by default, or `numbers`, `text`, `error_queue` or `binary_block`.
Numeric parsing retains exact decimal/exponent values and the original reply; it does
not pass through a floating-point approximation. Literal infinity/NaN values are tagged
explicitly. Vendor-specific numeric sentinel codes are only annotated when a matching
`sentinels` entry supplies its `token` and `status`; no threshold or unit is inferred.
`unit` is an explicit user annotation. Sentinel tokens match the trimmed reply token
exactly, including its sign, case and exponent spelling; for example,
`"sentinels":[{"token":"9.9E37","status":"overload"}]`. A match becomes a tagged
`scpi_sentinel` value rather than an ordinary measurement number. Literal infinity/NaN
values use `scpi_special` tags.

A sample value has `kind: "scpi_reply"`, `query`, `format` and `received_monotonic_ns`.
Line replies retain every original byte, including the terminator, in
`raw: {"encoding":"base64","data":"..."}`. The decoded reading is under `value`.
UTF-8 text becomes a JSON string; legacy 8-bit text uses an explicit base64 object.
`error_queue` produces `value: {"code":integer,"message":text-or-base64}` and
`status: "no_error"` or `"instrument_error"`. Nonzero instrument codes remain recorded
observations, separately from transport/acquisition failures.

Each query must be a single SCPI query program unit. Quoted arguments support escaped
quotes and embedded semicolons; extra unquoted program units and injected line endings
are rejected. Capturefab sends no setup/reset/output commands. Queries themselves can
trigger measurements or dequeue instrument errors: use the instrument's documented
semantics and preconfigure acquisition/format/trigger settings as needed. All replies
share one source read deadline. Per-query `received_monotonic_ns` remains available;
sequential replies do not establish simultaneous device acquisition or a hardware clock.
An unfinished in-flight reply fails the poll even at recording cutoff. Completed replies
from earlier polls remain recoverable.

### VXI-11 network instruments

Use `connection.transport: "vxi11"` for an enabled VXI-11 instrument interface,
such as the LAN interface on a Keysight Truevolt 34461A/34465A/34470A (LAN is
optional on the 34460A). The SCPI queries and output formats are the same as
for raw TCP or serial. Capturefab implements the core RPC transport directly;
VISA, RPC system libraries and a local rpcbind service are not required.

```json
{
  "version": 1,
  "sources": [{
    "id": "bench_meter", "protocol": "scpi",
    "connection": {"transport": "vxi11", "host": "192.0.2.40", "device": "inst0"},
    "write_terminator": "none", "every_ms": 1000, "timeout_ms": 5000,
    "signals": [
      {"name": "voltage", "query": "FETCh?", "response_terminator": "none", "unit": "V"}
    ]
  }]
}
```

`host` is a numeric IP address and `device` defaults to `inst0`. The source
asks that host's TCP portmapper on port 111 for its core service; an explicit
nonzero `core_port` bypasses the lookup when the service port is already known.
No hosts are scanned. One connection and link serve all queries in the source.
The session expects a dedicated SCPI conversation and does not request a
VXI-11 device lock.

The transport creates a link, writes only each configured query and its
configured terminator, reads its reply and destroys the link at normal finish.
Query fragments respect the server's advertised maximum write size, with END
set only on the final fragment. No trigger, clear, remote/local, status-byte,
SRQ or vendor-command RPC is issued. Failed polls close the core channel and
cannot replay queries. VXI-11 requires servers to release that connection's
link resources when the core channel closes.

All link creation, port lookup, connection, writes, read chunks and END
confirmation share the source's single poll deadline. Each I/O RPC receives
only the remaining timeout budget. RPC transaction IDs, accepted replies,
XDR widths/padding, lengths and device error statuses are checked. Successful
reason-zero reads continue until END; a short read alone is not completion.
Each read is bounded to 64 KiB and a record to 1024 fragments, while the usual
line, batch and streamed binary artifact limits still apply.

VXI-11, HiSLIP and USBTMC can select `none` for a line response terminator or for
`write_terminator`: its transport END supplies the message boundary. The raw
reply contains only the received bytes, with no synthetic newline. A configured
LF/CR/CRLF must still appear, followed by END without extra payload. Binary
blocks retain their exact definite-length framing and optional terminator,
then require END. Indefinite `#0` blocks remain unsupported. Raw TCP and serial
keep their existing terminator requirements and defaults.

The exact RPC fixture and a pinned independent Python VXI-11 server cover
link lifecycle, END, odd XDR padding, numeric precision, binary artifacts and
exports. Physical instrument/firmware qualification remains unverified.
[Keysight Truevolt interface specification](https://go.keysight.com/us/en/assets/7018-03846/data-sheets/5991-1983.pdf) ·
[VXI Consortium specifications, distributed by permission through ICS](https://www.icselect.com/vxi_spec.html) ·
[ONC RPC record marking](https://datatracker.ietf.org/doc/html/rfc5531#section-11) ·
[XDR encoding](https://datatracker.ietf.org/doc/html/rfc4506)

### HiSLIP network instruments

Use `connection.transport: "hislip"` for an enabled HiSLIP 1.0 interface. Keysight
added HiSLIP to the 34460A/34461A in firmware 2.08; the actual instrument still needs
its LAN interface and matching service configuration. R&S documents HiSLIP for
instruments including FSW and RTO/RTE. These are manufacturer interface examples,
not physical-device qualification by Capturefab.

```json
{
  "version": 1,
  "sources": [{
    "id": "bench_meter", "protocol": "scpi",
    "connection": {"transport": "hislip", "address": "192.0.2.40:4880", "device": "hislip0"},
    "write_terminator": "none", "every_ms": 1000, "timeout_ms": 5000,
    "signals": [
      {"name": "voltage", "query": "FETCh?", "response_terminator": "none", "unit": "V"}
    ]
  }]
}
```

`address` is a specific numeric IP and nonzero port, normally 4880. `device`
defaults to `hislip0`; an empty name selects the default virtual instrument. Names
are bounded to 256 graphic ASCII bytes. The adapter creates paired synchronous and
asynchronous TCP channels and associates them with the returned session. It honors
the server's initial synchronized or overlap mode, while issuing queries sequentially.
The transport requires no VISA runtime, vendor SDK or interpreter.

Data/DataEnd carry only the configured queries and terminators. Reply DataEnd supplies
END without adding a newline. Configured LF/CR/CRLF must still match, and END must
follow without extra payload. Definite binary blocks keep their existing exact-length
artifact staging rules. The client validates headers, message IDs and negotiated
message sizes; synchronous payloads are capped at 64 KiB per message. Its single poll
deadline includes both connections, setup, all fragments and final END.

A bounded asynchronous reader drains service requests without issuing status queries
or rearming SRQ. Peer errors, interruption messages, malformed headers and channel
failures abort the poll and retire both channels; no failed query is replayed. The
reader has no event queue or callbacks and is shut down and joined when the source
finishes. No automatic trigger, clear, remote/local, device lock, status polling,
TLS/SASL or discovery is implemented. Use a dedicated instrument conversation; this
query transport is not a full VISA or HiSLIP conformance claim.

[IVI HiSLIP specification](https://www.ivifoundation.org/downloads/Protocol%20Specifications/IVI-6.1_HiSLIP-2.0-2020-04-23.pdf) ·
[Keysight Truevolt firmware notes](https://www.keysight.com/no/en/assets/9018-07020/release-notes/9018-07020.pdf) ·
[R&S HiSLIP application note](https://scdn.rohde-schwarz.com/ur/pws/dl_downloads/dl_application/application_notes/1ma208/1MA208_2e_HiSLIP.pdf)

### USBTMC instruments

Use `connection.transport: "usbtmc"` for a configured USBTMC or USB488 interface.
The `usb` Cargo feature is required. Select exact numeric `vendor_id`, `product_id`
and `interface_number`, plus the exact `serial_number`; no index, bus-address,
VISA-resource or alternate-interface fallback is inferred. The following identity
is illustrative: replace all four fields with your instrument's observed identity.
Choose the query and interface setup from its manual.

```json
{
  "version": 1,
  "sources": [{
    "id": "usb_instrument", "protocol": "scpi",
    "connection": {
      "transport": "usbtmc", "vendor_id": 4660, "product_id": 22136,
      "serial_number": "METER-EXAMPLE-1", "interface_number": 3
    },
    "write_terminator": "lf", "every_ms": 1000, "timeout_ms": 5000,
    "signals": [{"name": "identity", "query": "*IDN?", "format": "text"}]
  }]
}
```

Serial comparison is case-sensitive and preserves interior spaces. USBTMC serials
are1–63 printable ASCII characters without edge spaces, double quote, star, slash,
colon, question mark or backslash. Angle brackets and vertical bar are admitted.
PID0 is valid; VID must be nonzero. The selected active interface must use the
USBTMC class/subclass and protocol0 or1, alternate0 and one bulk endpoint each way.
The adapter rechecks identity/configuration/interface after opening and claiming;
it does not change configuration or alternate setting. Windows uses OS-cached USB
identity for candidate filtering before claim, then verifies the actual descriptor,
US-English serial, configuration and alternate through the claimed interface before
queries. A known different cached serial does not cause that instrument to be claimed.

Provision exclusive USB access externally. On Linux, usbfs permissions and any
kernel `usbtmc` driver binding must permit a plain raw-interface claim. Windows
needs WinUSB on the selected interface; a VISA driver association is different.
macOS uses IOKit and cannot claim an interface held by another driver. Capturefab
does not detach drivers, replace associations or take over another client.
Use an exclusive, quiescent instrument conversation: claiming the interface and
matching fresh host tags do not prove that application response queues are empty.

A full configured query plus its terminator is sent as one EOM-bounded command.
Only validated USBTMC EOM completes the instrument message; newline, a short USB
packet or cancellation alone does not. The transport checks tags, declared size,
headers, capabilities and actual short termination. USB headers and legal padding
are excluded from SCPI data. Successful positive-length responses can continue
until final EOM. `none` terminators use this boundary without adding a newline;
configured terminators must still match, with no extra payload. Definite blocks
retain their exact existing raw/sidecar bytes, limits and atomic publication.

All setup, sibling queries, transfer progress and IPC share one original poll
deadline and the acquisition cutoff. Receive clocks are worker host completion
observations before decoding/storage, not USB device or sample clocks. Native
calls and release remain in an owned worker so parent waits do not inherit an
unbounded library cancellation. Finish uses its caller's separate cleanup budget;
late acquisition bytes cannot become another query's reply or a partial artifact.

Failure permanently retires the conversation. Cleanup can abort pending owned
transfers, but does not issue another application read, retry, reconnect, reset,
trigger, remote/local command or class Clear. USB488 Clear has Selected Device Clear
effects and is never automatic. Transfer abort and host-handle release do not
synchronize an unfinished instrument message; cleanup reports that uncertainty.
Normal Finish after completed EOM only releases owned resources.

The known Revision1.0 framing/capability subset admits valid advertised BCD versions
at least0100; unknown extensions are unsupported rather than guessed. This does
not qualify a future specification or every instrument/driver combination. USB-IF
wire/selector/control literals and deterministic fake USB/worker tests provide
protocol evidence; physical instruments and installed drivers remain unqualified.
Tektronix documents a USBTMC device port for the TBS1000C family, which illustrates
this connection class without establishing Capturefab hardware validation.

[USB-IF USBTMC/USB488 specifications](https://www.usb.org/document-library/test-measurement-class-specification) ·
[Tektronix TBS1000C datasheet](https://download.tek.com/datasheet/TBS1000C-Datasheet-3GW616744.pdf)

### Large binary waveform replies

```json
{
  "version": 1,
  "sources": [{
    "id": "scope",
    "protocol": "scpi",
    "connection": {"transport": "tcp", "address": "192.0.2.41:4000"},
    "timeout_ms": 10000,
    "signals": [
      {"name": "preamble", "query": "WFMOutpre?", "format": "text"},
      {"name": "trace", "query": "CURVe?", "format": "binary_block",
       "output": "waveforms/trace-{seq}.scpi", "max_bytes": 268435456,
       "data_type": "i16", "byte_order": "big"}
    ]
  }]
}
```

Preconfigure a Tektronix socket server for raw `NONE` protocol rather than terminal
banners/prompts. The waveform example also requires the instrument's actual binary
encoding and terminator. `data_type` and `byte_order` are optional explicit descriptors;
opaque blocks can be saved without either. A declared type checks whole-element
alignment and reports its count; the adapter does not decode or scale binary samples.
Use `response_prefix` for an exact required prefix when appropriate. Optional ASCII
headers/preambles before the block marker are retained, bounded to 16 KiB. Separate
queries can retain scaling/preamble information without inferred calibration.

IEEE 488.2 definite-length blocks (`#<digits><length><payload>`) stream into a bounded
hidden artifact, including the original prefix, header and terminator. The sample carries
file size/SHA-256, payload offset/length and an optional explicit type/order/element count.
The value's `raw` object uses `encoding: "file"`, `path`, `bytes` and `sha256`;
`block` reports `header`, `payload_offset`, `payload_bytes` and optional descriptors.
Read payload bytes at that offset, since the raw file includes its SCPI framing.
`metadata_path` names the appended `.json` companion, whose version-1 wrapper retains
`signal`, `poll_sequence` and the complete `response` value for standalone recovery.
`{seq}` expands the successful poll sequence, starting at `000001`. Files are never
overwritten. Replies and serialized sample bounds are
checked before a poll's artifacts are published; incomplete or failed polls remove their
unpublished stages and roll back only files they created. Publication of multiple files
is not a filesystem-wide atomic transaction.

Line responses default to 64 KiB including their terminator (maximum 1 MiB); binary
payloads default to 256 MiB (maximum 1 GiB, also limited by the protocol's nine-digit
length field). Numeric vectors accept at most 65,536 values, each token at most 256
bytes. The combined serialized poll is limited to 1 MiB, including inline raw replies
and decoded values; this can be reached before the line limit. The limit is checked
after each reply, and no later query is issued once it is exceeded. The time-series
file contains references rather than a truncated inline waveform. Indefinite `#0` blocks
require transport end-of-message signaling and are explicitly unsupported on raw
TCP/serial. Header/byte-count mismatches, truncation and invalid numeric replies fail the
poll; raw sentinel/error observations remain explicit values.

Manufacturer interface references: [Keysight DMM programming](https://www.keysight.com/us/en/assets/9925-01339/programming-guides/DM34460-Series-DMM-Programming-Guide.pdf),
[Keithley DMM6500](https://download.tek.com/manual/DMM6500-901-01B_Sept_2019_Ref.pdf),
[Tektronix TBS2000](https://download.tek.com/manual/TBS2000-Programmer-077114902.pdf),
and [R&S trace formats](https://scdn.rohde-schwarz.com/ur/pws/dl_downloads/dl_manuals/dl_manual/FSH-K40_OperatingManual_en_FW3.60.pdf).
Model, firmware and configured acquisition mode determine which queries are valid.

## Vaisala WXT530 serial weather transmitters

Use `vaisala_wxt` for a WXT530-series transmitter such as the WXT536 on a
native serial terminal or Windows COM port. The transmitter must be in ASCII
polled mode (`P` or `p`) with automatic composite output disabled (`I=0`).
Match the serial settings on the transmitter. Capturefab defaults to address
`0`, 19200 baud, eight data bits, no parity and one stop bit. Set `crc: true`
for mode `p`, including the 19200 8N1 service interface. For RS-485, use an
adapter that handles transmit direction and suppresses local echo. On a shared
RS-485 bus, Vaisala calls for disabling supervisor error messaging on each
transmitter. [Vaisala serial settings](https://docs.vaisala.com/r/M211840EN-G/en-US/GUID-5FB6E20B-B9EC-40E6-B139-9288C05296D9/GUID-E91F66C4-3B90-4DCD-8C3D-53228100A47C) · [Vaisala RS-485 configuration](https://docs.vaisala.com/r/M211840EN-G/en-US/GUID-023F715C-6B31-4E99-B858-4F4D5FBE03B0/GUID-A523F1DE-F6D7-49C6-A7F0-DE09A21E4082)

```json
{
  "version": 1,
  "sources": [{
    "id": "weather", "protocol": "vaisala_wxt", "device": "/dev/ttyUSB0",
    "address": "0", "crc": true, "every_ms": 1000, "timeout_ms": 800,
    "signals": [
      {"name":"wind_speed","field":"Sm"},
      {"name":"air_temperature","field":"Ta"},
      {"name":"air_pressure","field":"Pa"}
    ]
  }]
}
```

Enable these case-sensitive fields in the transmitter's composite message.
Each poll sends one addressed `R0` or CRC-protected `r0` request. The reply
must contain every configured field. Samples preserve `field`, its exact
`raw` text, the numeric `value` and the reported `unit_code`. A `#` suffix
produces `status: "invalid"` with the raw text. `Vh` reports its heating
status code separately, and `Id` preserves the device's text. The source
publishes the complete validated reply on the host receive timeline.
[Composite query](https://docs.vaisala.com/r/M211840EN-G/en-US/GUID-314E0A6B-ED3E-4B09-BDD0-1864CF924817/GUID-79A2B4EC-8207-4B2A-B823-31218EDABB2F/GUID-C403FA74-77D2-49DD-A07B-F802DB66EC9C) · [Field codes and units](https://docs.vaisala.com/r/M211840EN-G/en-US/GUID-314E0A6B-ED3E-4B09-BDD0-1864CF924817/GUID-79A2B4EC-8207-4B2A-B823-31218EDABB2F/GUID-2192AE50-9A50-4B31-ACC1-77CEEDD33E4E)

Capturefab sends only the composite query. For a precipitation-enabled
composite, use the transmitter's manual counter mode (`Z=M`) to preserve
accumulated precipitation across polls. Vaisala's automatic counter mode can
reset counts when precipitation data is reported. Changing the counter mode
also resets existing counters. [Precipitation settings](https://docs.vaisala.com/r/M211840EN-G/en-US/GUID-5C3927B5-A979-4B35-8519-86DC222B1E81/GUID-BA0E07F5-F425-4314-BBEB-68DC26D69787/GUID-23B5AA40-CC55-4768-BD70-4ADC835A4D3A)

## GNSS receivers with NMEA output

Use `nmea_gnss` to receive GGA and RMC sentences from a GNSS receiver already
configured to output NMEA on a serial port or a raw TCP server endpoint.
Capturefab sends no bytes and does not change receiver settings. This covers
the documented GGA/RMC layouts of NovAtel OEM7/PwrPak7/SMART7, Trimble OEM
receivers such as BD990/BX992 and u-blox ZED-F9P. Physical receiver/firmware
qualification remains unverified; the regression fixtures use manufacturer
literal sentences and specification-derived edge cases.

```json
{
  "version": 1,
  "sources": [{
    "id": "position", "protocol": "nmea_gnss",
    "serial": {"device": "/dev/ttyUSB0", "baud": 38400},
    "timeout_ms": 100,
    "signals": [
      {"name": "fix", "sentence": "GGA"},
      {"name": "motion", "sentence": "RMC"}
    ]
  }]
}
```

Use one or both sentence mappings, with unique names/types, on the same owned
connection. Replace `serial` with an explicit numeric `address`, for example
`"192.168.10.52:5017"`, for a receiver TCP server. Select the actual configured
port; Trimble firmware 6.43 and newly activated receivers removed the old
factory 5017/5018 defaults. TCP authentication prompts and NTRIP are outside
this raw stream profile. Serial baud is explicit; parity defaults to `none`,
`data_bits` to 8 and `stop_bits` to 1. Use an electrical interface appropriate
for the receiver, such as a USB UART or RS-232 adapter. Existing native serial
baud limits apply. This is a push source: `every_ms` does not set receiver rate.

Each selected sentence produces one reading after its XOR checksum and CRLF
are verified. `talker`, `sentence`, two-character `checksum` and the original
`raw` sentence are retained. Coordinates stay as exact decimal strings in
`latitude_dm` (`ddmm.mmm...`) and `longitude_dm` (`dddmm.mmm...`), with separate
`latitude_hemisphere` and `longitude_hemisphere`. They are not decimal degrees.
`utc_time` retains the original `hhmmss[.fraction]` string; RMC's
`date_ddmmyy` retains its two-digit year without a century assumption. There
is no coordinate conversion, datum inference or GGA/RMC epoch fusion.

GGA retains numeric `quality`, optional `satellites`, `hdop`, `altitude`,
`geoid_separation` and `differential_age_s`, plus reported `altitude_unit`,
`geoid_unit` and `station_id`. RMC retains its `status`, speed in `speed_knots`,
`course_true_deg`, magnetic variation and optional `mode` and
`navigation_status`. Numeric decimals use exact JSON numbers; `raw` also keeps
leading zeros and signs. Empty fields are absent. Quality zero and RMC status
`V` remain observations with their reported fields, including absent position
or time; no fix is invented. RMC navigation status is separate from its
position-validity status.

Positions and heights depend on receiver datum/geoid configuration. NovAtel
also documents UTC output using default parameters before a valid almanac is
available. Receiver time/date fields remain data in the value object, while
the recording timestamp is host receive time. No GNSS discipline, PPS capture
or hardware synchronization is inferred.

Frames are bounded to 512 bytes, allowing documented high-precision GGA
sentences longer than the historical 82-byte limit. A leading partial sentence
is skipped when attaching to the stream. After synchronization, malformed
complete sentences fail the recording. Valid unselected sentences are checked
and skipped. Partial sentences survive idle read deadlines; a trailing partial
sentence at the requested cutoff produces no observation. Complete buffered
sentences retain their receive timestamps. Binary UBX/RTCM, proprietary position
sentences, NMEA2000 and UDP are outside this profile.

[NovAtel GPGGA](https://docs.novatel.com/OEM7/Content/Logs/GPGGA.htm) ·
[NovAtel extra-precision GGA](https://docs.novatel.com/OEM7/Content/Logs/GPGGALONG.htm) ·
[NovAtel GPRMC](https://docs.novatel.com/OEM7/Content/Logs/GPRMC.htm) ·
[u-blox F9 HPG 1.32 interface description, §§2.1–2.7](https://content.u-blox.com/sites/default/files/documents/u-blox-F9-HPG-1.32_InterfaceDescription_UBX-22008968.pdf#page=22) ·
[Trimble NMEA framing](https://receiverhelp.trimble.com/oem-gnss/nmea0183-messages-overview.html) ·
[Trimble TCP/serial output configuration](https://receiverhelp.trimble.com/oem-gnss/help-io-config-configuration.html)

## Micro-Epsilon ILD2300 displacement sensors

Use `microepsilon_rs422` with `profile: "ild2300_dist1"` for a dedicated serial
endpoint carrying an externally configured ILD2300 single-DIST1 RS422 stream.
The manufacturer documents IF2001/USB with an installed FTDI Virtual COM Port
driver and model-specific RS422/power wiring. Match the configured baud:

```json
{
  "version": 1,
  "sources": [{
    "id": "laser", "protocol": "microepsilon_rs422",
    "profile": "ild2300_dist1", "device": "/dev/ttyUSB0", "baud": 691200,
    "signal": "distance1", "timeout_ms": 1000
  }]
}
```

Configure `OUTPUT RS422`, `OUTDIST_RS422 DIST1`, and
`OUTTHICK_RS422`, `OUTSTATISTIC_RS422`, `OUTADD_RS422` as `NONE` before recording.
Choose a suitable output reduction externally. Supported configured rates are
9600, 115200, 230400, 460800, 691200, 921600, 1500000, 2000000, 2500000,
3000000, 3500000 and 4000000 baud, fixed 8N1. The adapter default remains 115200;
the example selects the sensor's documented 691200 factory rate. Rates above
115200 require the host platforms described under serial JSONL and an adapter
and driver that accept the selected rate. Windows accepts a configured COM
device path. Capturefab receives continuously, sends
no sensor commands, and does not change measurement, laser, trigger or mode
settings. `every_ms` does not throttle this passive source.

Each sample retains the original three bytes as base64, the complete unsigned
18-bit `raw_code`, and configured profile provenance. The ten documented
application errors remain observations with `decode_status: "device_error"`
and a named `error`. Other codes remain raw; no distance conversion, physical
resolution, unit, valid flag or sensor timestamp is inferred. The upper two
payload bits matter for documented material-dependent values and errors.

Opening midword accepts only an initial H or M,H suffix, retained once in
`initial_alignment`. Subsequent words require exact L/M/H tags; extra outputs,
bad ordering or a set H marker fail without resynchronization. A complete word
is never read together with bytes from its successor. One original deadline
covers serial setup, fragmented input and parsing. Idle with no partial word
can end normally; an incomplete word or alignment at timeout/cutoff fails
strictly, including after earlier success, and retires the local port.
Earlier complete observations remain exportable with `--allow-incomplete`.

The timestamp is the last host read of that word. Buffered input, `OUTHOLD`,
undetectable whole-word loss and the absence of a word checksum prevent any
freshness or losslessness guarantee. Multi-value streams and Ethernet are
outside this profile. Physical sensor and bridge qualification remain pending.

[optoNCDT 2300 manual, RS422 sections 8.3 and 9.1](https://www.micro-epsilon.pl/download/man--optoncdt-2300--en.pdf) ·
[IF2001/USB setup guide](https://www.micro-epsilon.com/fileadmin/download/manuals/set--IF2001-USB--en.pdf)

## SICK Compact UDP scans, IMU and encoder data

Use `sick_compact_udp` for an externally configured multiScan100 or picoScan100
sending complete Compact telegrams over IPv4 UDP. The source supports Type 1
versions 3/4 for `points`, or Type 2 version 1 for `imu`. The explicit
`picoscan150` profile also supports Type 4 versions 1/2 with `encoder`, or all
three types on one listener with `mixed`. Select the profile and stream
explicitly. Use `sick_compact_tcp` below for LRS4000 or multiScan200 TCP output;
MSGPACK requires its own adapter.
Configure Compact output, destination
and measurement state with the device's existing owner before recording.
Capturefab receives datagrams and sends no setup, activation or control command.
The listen port below is an example; match the externally configured destination.

```json
{
  "id": "lidar",
  "protocol": "sick_compact_udp",
  "listen": "0.0.0.0:7500",
  "source_ip": "192.0.2.60",
  "profile": "multiscan100",
  "stream": "points",
  "output_directory": "sick-capture-01",
  "max_bytes": 1073741824,
  "packets_per_group": 16
}
```

Profiles are `multiscan100`, `picoscan100` and `picoscan150`. They record the configured family;
the IP filter and native sender number do not authenticate model or identity.
Only `picoscan150` admits `encoder` or `mixed`; the older profiles continue to
admit only points and IMU. Optional `source_port` restricts the peer further.
Known unselected Compact traffic is ignored under the same bounded foreign-work budget, without claiming
its CRC or schema was validated. Unknown types, bad magic, invalid CRC and
malformed selected telegrams fail explicitly. Every receive uses a full-sized
buffer; foreign work is bounded by actual bytes and datagram count under one
original read deadline.

For `picoscan150`, a single-stream source recognizes Types 1/2/4 for this
selection rule. The older profiles recognize only Types 1/2, preserving their
unknown-Type4 failure behavior. In `mixed`, every Type 1/2/4 datagram is selected
and must pass its own complete CRC and native-layout validation. Mixed mode
preserves reception order, repeated native counters and each packet's actual
source endpoint and host receive clock; it does not associate an encoder tick
with a scan, assemble frames or deduplicate packets.

An encoder-only source can use the externally configured destination below:

```json
{
  "id": "encoder",
  "protocol": "sick_compact_udp",
  "listen": "0.0.0.0:7504",
  "source_ip": "192.0.2.60",
  "profile": "picoscan150",
  "stream": "encoder",
  "output_directory": "pico-encoder-01",
  "max_bytes": 1073741824,
  "packets_per_group": 16
}
```

Use `stream: "mixed"` on one source when the external owner deliberately routes
scan, IMU and encoder outputs to the same destination. These outputs can have
different sender ports; normally omit `source_port` for that setup. A specified
port remains an intentional filter across all selected datagrams. Separate
destinations can instead use separate sources and new output directories.

Type 4 retains the frame number, three unsigned 32-bit tick counts, the exact
IEEE32 speed word and three separate declared Unix-microsecond change clocks.
The speed value is included only when finite, with signed zero and nonfinite
payload bits preserved. Its conversion rate, physical units, encoder input mode
and reference-signal validity are not carried by this telegram. Capturefab does
not infer them, reinterpret high-bit counters as signed values, derive an angle
or promote a reference timestamp to a valid-position flag. Version 1's declared
payload length includes the sender identifier; version 2's excludes it, while
both complete telegrams are 88 bytes.

Each selected datagram must contain one whole telegram, including its CRC32.
The OS handles IP fragmentation; the adapter does not concatenate application
fragments, infer missing segments or assemble complete scans. Native segment and
frame counters remain metadata. Repeated or out-of-order native counters are
retained rather than treated as authenticated freshness or silently deduplicated.
A valid Type 1 telegram can have zero modules.

Native output retains linked module boundaries, all conditional echo/beam fields,
raw masks and reserved bytes. Dense columns use Base64 of little-endian words:
`base64_le_u16` for distance/RSSI/azimuth, `base64_le_u64` for clocks and
`ieee754_le_u32_bits` for float words. Shape and order distinguish line metadata,
`beam_line` properties/azimuth, and `beam_line_echo` returns. Omitted fields stay
omitted. Version 3 distance uses raw millimetres; version 4 retains raw distance
and the module's exact scale word. No coordinate conversion is inferred.
IMU acceleration, angular velocity and quaternion columns preserve all ten
IEEE32 words, including nonfinite values and signed zero. Quaternions are not
normalized or promoted to a pose.

Transmit/IMU timestamps retain their declared Unix-microsecond basis; per-line
start/stop clocks retain their separate declared device-microsecond timebase.
Clock synchronization is unverified. These fields stay separate from completed
host receive times and do not populate an invented top-level `source_time`.
Decoded work is checked before iteration, including zero-content-mask cases.
Per-packet and grouped JSON admission stays below 1 MiB with sample-wrapper
headroom; a group may be smaller than `packets_per_group` to fit its bounds.
That option accepts 1–32 and defaults to 32.

`output_directory` must be a new literal local directory. Accepted datagrams are
stored exactly in `datagrams.sickcompact`, with a versioned header, optional
configured context, peer/receive-clock record headers and independently committed
hashed groups. A `sick_compact_udp_group` sample appears only after append and
sync succeed. Its offsets, lengths and SHA-256 allow readback of the full raw
input. A final archive observation describes the committed prefix; those hashes
are integrity checks, not proof of durable publication of the recording JSONL.

Optional `context` names an opaque local file of at most 1 MiB. Its exact bytes
are frozen into the archive with a hash and a configured/unverified label;
Capturefab does not infer calibration or units from it. Absent and explicit empty
context remain distinct. Accepted pending data drains with original host clocks
at cutoff, without another receive. Storage or terminal receive errors preserve
the committed prefix and the first typed failure.

A selected malformed packet is retained as exact `rejected-datagram.raw` bytes
plus a bounded `rejected-datagram.json` sidecar containing its peer, clock,
context reference, hash and typed cause. If it is the first input, the archive
contains context evidence only: zero valid groups and no fabricated sample.
If storage prevents evidence, the original cause remains and bounded
`evidence_write_error` metadata includes the actual typed code and message. Preexisting directories are never replaced or cleaned up.

`max_bytes` includes archive framing, context, groups and diagnostic files.
The adapter reserves 81,891 bytes for one maximum-size rejected datagram and its
sidecar. The no-context minimum is 82,127 bytes for points or mixed, 82,155 for
IMU, or 82,179 for encoder;
actual context bytes add to it. The optional `min_free_bytes` and the normal
storage free-space floor also apply. Exhaustion stops capture without overwriting
prior committed groups. These are software capacity limits, not manufacturer
geometry or timing guarantees.

See SICK's [Compact guide](https://sickag.github.io/sick_lidar_developer_guide/data-streaming/compact-format/)
and pinned [authoritative format definitions](https://github.com/SICKAG/KaitaiStructDescriptions/tree/7ea18d7afce430a3ec02b1c20ca57aa86392809a/formats).
The fixtures are specification-derived and do not qualify physical hardware.
The published picoScan150 encoder example uses fixed-velocity mode, so replay
of that example does not qualify a physical encoder or its conversion settings.

## SICK Compact TCP scans, ambient light and IMU

Use `sick_compact_tcp` to receive externally configured LRS4000 or multiScan200
Compact output on an explicit numeric IPv4 TCP endpoint. The `lrs4000` profile accepts
Type 1 versions 3/4 with `stream: "points"`. The `multiscan200` profile accepts
Type 6 version 1 with `points`, Type 3 version 1 with `ambient`, or Type 7 version
1 with `imu`. Select the profile and stream explicitly. Device output and
measurement state must already be configured by its existing owner. Capturefab
connects and receives; it sends no application, login or activation commands.

```json
{
  "id": "front_scan",
  "protocol": "sick_compact_tcp",
  "address": "192.0.2.61:2115",
  "profile": "lrs4000",
  "stream": "points",
  "output_directory": "lrs-capture-01",
  "max_bytes": 1073741824,
  "timeout_ms": 1000
}
```

For multiScan200, configure a separate source and new output directory for each
required stream. Its documented TCP ports are 2115 for scans, 2116 for ambient
light and 2117 for IMU. Ports remain explicit so the configuration can match the
externally prepared device. A configured family or native sender number is not
authenticated device identity. Native frame numbers remain available for the
consumer; Capturefab does not join streams or claim synchronized acquisition.

TCP reads can split or coalesce telegrams arbitrarily. The receiver follows
declared extents and Type 1 module links, verifies the entire telegram's CRC32,
and validates its native layout before publishing any part. It does not scan
payload bytes for another start marker, reconnect after a failure or treat a
socket read as a packet boundary. One original deadline covers connection,
framing, verification and admission. Idle at a clean boundary differs from an
expired or closed partial telegram, which is a permanent failure for that source.

The software admits complete telegrams up to 4 MiB. This covers the documented
9,000-beam, three-echo LRS4000 example and the 592-column, 192-layer, three-echo
multiScan200 example without reusing the UDP size limit. A large native result
is emitted as bounded parts after the complete telegram is validated and
committed to its local raw archive. Parts retain native ranges and the same
original host completion clock, including those delivered at the acquisition
cutoff. A completed host read does not measure arrival at the NIC. Finalization
drains already accepted data without reading another telegram.
Native admission allows at most 64 parts, each with at most 768 KiB of native
JSON, and 16 MiB in total. All parts are precharged before the archive commit;
the receiver keeps the raw telegram and a bounded plan, materializing one part
at a time. These are software capacity limits rather than manufacturer geometry
or throughput guarantees.

Native output preserves optional fields, masks, reserved bytes, original
integer words and IEEE float bits, including nonfinite values and signed zero.
No coordinates, pose or physical units are inferred from an opaque context file.
Type 6 packed intensity bytes remain opaque with explicit source ranges and bit
counts; the published odd-count packing leaves a padding ambiguity, so the
adapter does not fabricate a final intensity sample. Ambient words retain flat
native ranges where the format does not establish their spatial ordering.
Raw device and declared Unix-microsecond clocks stay distinct, with unverified
synchronization and no invented top-level `source_time`.

`output_directory` must be a new literal local directory. Optional `context`
freezes at most 1 MiB of opaque bytes with its hash; absence and an explicit
empty context remain distinct. The archive retains exact complete telegrams
and independently synced records with hashes. An archive commit and JSONL
publication are separate events: a crash can leave archived parts that were
not yet emitted into the recording. Storage failure retains the committed
prefix and the original typed cause. Partial-stream or malformed-telegram
evidence records exactly the bytes consumed, without claiming the unread
remainder was captured. Evidence and archive framing count toward `max_bytes`;
the normal free-space floor and optional `min_free_bytes` also apply.

The raw file is `telegrams.sickcompacttcp`. A `sick_compact_tcp_part` reading
identifies its telegram, part index/count, exact raw archive range/hash and
stream offset. Its `native` value contains the bounded native part. Every native
part repeats `native_elements_declared` for the complete telegram, labelled
`native_elements_scope: "complete_telegram_not_per_part"`; do not sum this count
across parts. It counts declared return positions for scans, ambient words for
ambient-light telegrams, or one IMU sample. A final
`sick_compact_tcp_file` observation distinguishes committed telegrams, admitted
parts and part readings released by this adapter; it does not certify JSONL
publication. The default signal is `telegrams`.

Failure evidence uses `rejected-stream.raw` and `rejected-stream.json`, labelled
as a complete framed telegram or a consumed stream prefix. The adapter reserves
4,210,688 bytes for one maximum-sized failure and its bounded sidecar. Including
minimum archive framing, the no-context `max_bytes` minimum is 4,210,892 for
LRS4000 points, 4,211,024 for multiScan200 points, 4,210,942 for ambient light or
4,210,944 for IMU. Actual context bytes add to these minima.

See SICK's [Compact transport and format guide](https://sickag.github.io/sick_lidar_developer_guide/data-streaming/compact-format/)
and the pinned [authoritative format definitions](https://github.com/SICKAG/KaitaiStructDescriptions/tree/7ea18d7afce430a3ec02b1c20ca57aa86392809a/formats).
The fixtures are specification-derived and do not qualify physical hardware.

## SICK single-scan LiDAR

Use `sick_cola` on an explicitly configured TCP endpoint. The default `cola_a`
profile uses the bounded TiM CoLa A layout; select an explicit LMS ASCII or
model-specific CoLa B profile below. Each ASCII
poll sends only `STX sRN LMDscandata ETX`, requesting the latest stored scan.
The scanner must already be measuring and continuous output must be disabled
on that connection. Capturefab does not log in, start measurement, change
configuration or subscribe to scan events. Ports are explicit because CoLa
mode and port assignment depend on the device configuration.

```json
{
  "version": 1,
  "sources": [{
    "id": "lidar", "protocol": "sick_cola", "address": "192.168.10.51:2111",
    "signal": "front_scan", "every_ms": 100, "timeout_ms": 1000
  }]
}
```

The default ASCII profile accepts version-1 `sRA LMDscandata` replies with exactly
one 16-bit `DIST1` channel and an optional 8-bit `RSSI1` channel with the same
sample count, start angle and angular step. Layer angle, encoder count,
position, comment, calendar-time and event extensions must all be zero. A
length-prefixed device name may contain up to 15 ASCII characters, including
spaces. Frames are limited to 64 KiB and channels to 4096 points; these are
Capturefab software bounds. Other channel arrangements and nonzero extensions
fail explicitly. Unsolicited `sSN` scans, multiple echoes/layers and
sector-specific layouts are outside this profile.

Each poll publishes one value object containing `distance.values` and optional
`rssi.values`, raw IEEE754 `scale_bits` and `offset_bits`, signed
`start_angle_raw`, unsigned `angle_step_raw`, device number/serial/status,
digital I/O bytes, counters and raw frequency fields. All wire widths are
checked: samples are u16/u8, step and counters are u16, clocks/frequencies and
scale/offset bits are u32, and start angle retains its signed 32-bit value.
No physical range or model geometry is inferred. Distance codes below 16
remain exact invalid/reserved codes; they are not converted into distances.
A scan reporting a device error retains its raw `device_status` alongside its
samples. Both JSONL and CSV exports preserve the complete value object.

The documented TiM angle unit is 1/10000 degree, scan frequency is 1/100 Hz,
and measurement frequency is in 100 Hz. The adapter preserves these integer
fields without conversion: the TiM4xx literal example contains an inconsistent
start-angle description. `startup_us` and `transmit_us` are wrapping u32
microseconds since device startup, not UTC. Host receive time remains the
recording timestamp; no hardware synchronization is claimed.

The request is documented for TiM2xx, and the shared TiM24x response layout
appears in SICK's telegram listing. TiM4xx supplies the complete literal ASCII
reply used by the regression fixture. TiM24x geometry is covered by a
specification-derived fixture. For this default ASCII profile, actual hardware/firmware qualification and
broader LMS/MRS/TiM5xx/TiM7xx compatibility remain unverified; a matching command
name alone does not qualify those devices.

[SICK TiM2xx operating instructions, §7.2.1](https://www.sick.com/media/docs/7/67/467/operating_instructions_tim2xx_2d_lidar_sensors_en_im0090467.pdf#page=30) ·
[SICK TiM4xx operating instructions, Tables 70 and 76–78](https://www.sick.com/media/docs/4/64/664/operating_instructions_tim4xx_2d_lidar_sensors_en_im0093664.pdf#page=68) ·
[SICK common telegram listing, §§4.3.6–4.3.7](https://cdn.sick.com/media/docs/7/27/927/telegram_listing_telegram_listing_ranging_sensors_lms1xx_lms5xx_tim2xx_tim5xx_tim7xx_lms1000_mrs1000_mrs6000_nav310_ld_oem15xx_ld_lrs36xx_lms4000_lrs4000_multiscan100_en_im0045927.pdf#page=94)

### CoLa A LMS1xx and LMS5xx

For an LMS interface already configured for ASCII, select `cola_a_lms1xx` or
`cola_a_lms5xx`. The port is explicitly configured. Capturefab sends only
`STX sRN LMDscandata ETX`, requesting the latest stored scan; measurement must
already be active and continuous output disabled.

```json
{
  "version": 1,
  "sources": [{
    "id": "lidar", "protocol": "sick_cola", "address": "192.0.2.40:2111",
    "profile": "cola_a_lms5xx", "signal": "front_scan",
    "every_ms": 100, "timeout_ms": 1000,
    "max_frame_bytes": 131072, "max_points": 8192
  }]
}
```

These profiles publish `encoding: "cola_a"`, `model_profile`, exact
`raw_frame_hex` including STX/ETX, `channels` in wire order, raw
status/I/O/counters/frequencies, `encoders` and `events`. Each channel retains
its name, sample width, raw IEEE754 `scale_bits`/`offset_bits`, signed
`start_angle_raw`, unsigned `angle_step_raw` and exact u16/u8 `values`.
Geometries and counts remain independent. DIST's `scaled_unit: "mm"` describes
the quantity after applying its transmitted factor/offset; stored samples remain
raw counts, including reserved codes 0–15. RSSI units remain unspecified.
JSONL and CSV preserve the complete object. The default `cola_a` reply shape
is unchanged.

LMS1xx accepts version 1 and echo names 1–2. LMS5xx accepts version 1 and the
documented legacy version 0 PRO body; DIST/RSSI1–5 depends on software V1.10 or
newer. The common telegram listing includes 16-bit RSSI; the model eight-bit
tables permit RSSI only. Up to three raw encoder records, a length-prefixed
ASCII name of up to 15 bytes (including spaces), and raw calendar fields are
supported. Position/comment slots must be zero. LMS5xx PRO permits up to three
FDIN events, newest first; external encoders and events cannot coexist.
Other versions, names, layers or tails fail explicitly.

`max_frame_bytes` defaults to 128 KiB and permits 128–262144 bytes;
`max_points` defaults to 8192 per channel and permits 1–65535. Both accept exact
integral decimal/exponent JSON and require an explicit model profile. One
original deadline covers transport, parsing and serialization. Raw hex and
arrays are charged before allocation, with a 1 MiB serialized value ceiling and
room for sample metadata. Malformed, oversized, truncated or unsolicited replies
retire the source without retry or a partial poll; earlier complete polls remain.

Startup/transmit/event microseconds wrap and are not UTC. Calendar components
retain the documented bounds (including years 1970–2037 and raw zero month/day),
without inferring timezone, synchronization, `source_time` or an epoch. No
coordinates, return ranking or across-channel snapshot guarantee is inferred.
The profiles follow the model telegram tables; matching a command name alone
does not qualify other devices, firmware or layouts. Physical validation remains
pending.

[LMS1xx instructions, telegram tables](https://www.sick.com/media/docs/1/31/331/operating_instructions_lms1xx_2d_lidar_sensor_en_im0031331.pdf#page=120) ·
[LMS5xx instructions, telegram tables](https://www.sick.com/media/docs/4/14/514/operating_instructions_lms5xx_2d_lidar_sensors_en_im0037514.pdf#page=153) ·
[SICK PRO external events](https://support.sick.com/sick-knowledgebase/article/?code=KA-08002)

### CoLa B TiM5xx, LMS1xx and LMS5xx

Select `cola_b_tim5xx`, `cola_b_lms1xx` or `cola_b_lms5xx` for an interface
already configured for binary CoLa B. The documented profiles cover TiM55x/56x/57x,
LMS1xx and LMS5xx/LMS531 telegram families respectively; check the model manual's
firmware and scan-content settings. Capturefab sends the fixed binary
`sRN LMDscandata` query. It accepts only a matching `sRA` reply and does not switch
protocols, log in, start measurement or subscribe to streaming scans.

```json
{
  "version": 1,
  "sources": [{
    "id": "lidar", "protocol": "sick_cola", "address": "192.0.2.40:2112",
    "profile": "cola_b_lms5xx", "signal": "front_scan",
    "every_ms": 100, "timeout_ms": 1000,
    "max_frame_bytes": 131072, "max_points": 8192
  }]
}
```

The port is explicit and must match the scanner's configured service. Binary
`max_frame_bytes` defaults to 128 KiB and permits 128–262144 bytes;
`max_points` defaults to 8192 per channel and permits 1–65535. These options require
an explicit model profile (CoLa A LMS or CoLa B) and accept exact integral JSON
decimal/exponent notation.
LMS5xx high-resolution scans with multiple echoes and 16-bit remission can exceed
the default ASCII profile's 64 KiB and 4096-point limits. The limits bound software resources;
they do not prescribe the device's angular settings.

Each value contains `encoding: "cola_b"`, `model_profile`, the complete original
`raw_frame_hex` including header/checksum, status and digital-I/O byte arrays,
counters, raw frequencies and boot-relative clocks. `channels` keeps wire order
and each name, `sample_bits`, `scale_bits`, `offset_bits`, signed `start_angle_raw`,
unsigned `angle_step_raw` and raw u16/u8 `values`. Channel counts and geometries may
differ. No coordinates, range filtering or floating-point conversion are inferred.
DIST's `scaled_unit: "mm"` describes the quantity after applying the transmitted
scale/offset; stored samples remain raw counts, including reserved values 0–15.
RSSI units remain unspecified.

TiM supports the DIST1/RSSI1 namespace, LMS1xx names 1–2, and LMS5xx names 1–5.
LMS model manuals restrict the eight-bit table to RSSI. Up to three LMS encoder
records preserve raw position/speed. An optional name and LMS calendar retain
their bounded wire fields; calendar values supply no timezone or synchronization
proof. Position/comment slots must be zero. LMS5xx PRO can carry up to three raw
FDIN events, with external encoders and events mutually exclusive. Unsupported
layers, channel names, versions or tails fail explicitly.

Four-STX framing, big-endian lengths and payload XOR are checked under one total
poll deadline. The raw hex plus typed value has a 1 MiB output ceiling: raw bytes
are charged first, then each array's exact serialized size before allocation.
Malformed, oversized, truncated, unsolicited or mismatched replies retire the
connection without retry. An incomplete in-flight poll fails at cutoff; earlier
completed polls remain recoverable. A latest stored scan can be stale, so its
scan counter and raw clocks stay visible.

Positive poll fixtures are specification-derived. An unchanged official SICK
emulator `sSN` validates the common binary body and is rejected as a poll reply;
the printed manual's inconsistent-checksum `sRA` is retained as a negative fixture.
Physical-device and firmware qualification remains pending. Multilayer/MRS, NAV
sectors and compact protocols are outside these profiles.

[SICK common telegram listing](https://cdn.sick.com/media/docs/7/27/927/telegram_listing_telegram_listing_ranging_sensors_lms1xx_lms5xx_tim2xx_tim5xx_tim7xx_lms1000_mrs1000_mrs6000_nav310_ld_oem15xx_ld_lrs36xx_lms4000_lrs4000_multiscan100_en_im0045927.pdf) ·
[LMS1xx model instructions](https://www.sick.com/media/docs/1/31/331/operating_instructions_lms1xx_2d_lidar_sensor_en_im0031331.pdf) ·
[LMS5xx model instructions](https://www.sick.com/media/docs/4/14/514/operating_instructions_lms5xx_2d_lidar_sensors_en_im0037514.pdf) ·
[SICK external events](https://support.sick.com/sick-knowledgebase/article/?code=KA-08002)

## METTLER TOLEDO scales and balances

Use `mt_sics` to poll a dedicated MT-SICS command port for immediate net
weight. On the IND360, enable PC Application access and connect to the
service Ethernet interface, normally port 1026. The IND360 accepts one
MT-SICS connection at a time. [IND360 user guide, Appendix C](https://www.mt.com/dam/product_organizations/industry/Load_Cells/Downloads/Transmitter/ind360/manuals/30654701_04_MAN_UG_IND360_EN.pdf)

```json
{
  "version": 1,
  "sources": [{
    "id": "scale", "protocol": "mt_sics", "address": "192.168.10.50:1026",
    "signal": "net_weight", "every_ms": 100, "timeout_ms": 500
  }]
}
```

For a compatible serial balance, such as a NewClassic ML, replace `address`
with `"serial":{"device":"/dev/ttyUSB0"}` or use a Windows COM port. Serial
defaults are 9600 baud, eight data bits, no parity and one stop bit. Set the
balance to command mode, CRLF and handshake OFF. Match any other line settings
with `baud`, `data_bits`, `parity` and `stop_bits` inside `serial`. The
NewClassic ML factory configuration uses software flow control.
[NewClassic ML operating instructions, serial settings](https://www.mt.com/dam/MT-TW/PDFs/ML_OI_en_11781287F.pdf)

Each poll sends `SI` and reads one bounded CRLF reply on a single deadline.
Use an idle command channel dedicated to the recorder. `SI` can end an
existing continuous-output command on some balances. The default signal is
`net_weight`. A `stable` or `dynamic` sample carries `raw` with the signed
decimal as received, the exact JSON number `value`, and the reported ASCII
`unit`. `unavailable`, `overload` and `underload` report a `status` alone.
Malformed replies and command errors fail the poll. CSV and JSONL exports
preserve the same value object. [NewClassic MT-SICS reference, pp. 9, 103–104](https://www.mt.com/dam/product_organizations/laboratory_weighing/basic_weighing/Products/MT-SICS/MANUALS/en/MT-SICS_NewClassic_en_30014405.pdf)

## A&D standard-format balances

Use `aandd_balance` for an A&D balance already configured for standard-format
command replies. The published GX-A/GF-A communication manual documents both
RS-232C and bidirectional USB Virtual COM. Quick USB keyboard mode uses a
different interface. Use a dedicated command channel with continuous output,
extra ID/date lines and alternate output formats disabled. Disable automatic
zero/tare after data output (including `Ar-d`, where available).

```json
{
  "version": 1,
  "sources": [{
    "id": "balance", "protocol": "aandd_balance",
    "serial": {"device": "/dev/ttyACM0", "baud": 2400,
               "data_bits": 7, "parity": "even", "stop_bits": 1},
    "terminator": "crlf", "signal": "measurement",
    "every_ms": 1000, "timeout_ms": 1000
  }]
}
```

Use a Unix terminal or Windows COM device. Match the balance's line settings;
serial defaults are 2400 baud, seven data bits, even parity and one stop bit.
Configured 7O1 and 8N1 are also supported, at the documented 600–38400 baud
rates. The interface must operate without software or hardware flow control;
USB Virtual COM ignores the balance's serial bit-rate settings. A compatible
cable and ready interface remain prerequisites. No modem handshake or balance
mode is discovered or changed.

Alternatively, replace `serial` with a numeric TCP `address` for an explicitly
configured transparent serial bridge. A&D also documents `Q` plus CRLF through
the optional FXi-08 Ethernet interface on FZ/FX balances, using port 10001 in
its factory configuration; select A&D standard output on the balance. The
address and port remain explicit, and model names do not select a service.
[FXi-08 LAN setup and standard-format settings](https://www.aandd.jp/products/manual/balances/fz-wp_fx-wp.pdf)

Select `terminator: "crlf"` (the default) or `"cr"` to match the balance. Each
poll sends only `Q` with that terminator and accepts one complete bounded reply.
It never sends tare, zero, calibration or stream commands.

The value object retains the complete `raw` reply including its terminator,
`status_raw`, the nine-character `value_raw` and three-character `unit_raw`.
Stable, unstable and stable-counting replies additionally expose an exact JSON
number `value`; sign, leading zeros and decimal spelling remain in `value_raw`.
`unit` is the received nonempty trimmed code, with no mass or counting-unit
conversion or net/gross inference. Overload replies retain their raw fields and
polarity but omit numeric `value` and `unit`: the documented `+19` suffix belongs
to the overload sentinel. Complete `EC,Edd` replies are nonnumeric device-error
observations; `E02` additionally identifies `not_ready`.

One original poll deadline covers connection, query, reply and parsing.
Malformed, oversized, truncated or late replies close the source permanently,
without a partial poll or automatic reconnect. Earlier completed observations
remain exportable, and CSV/JSONL retain the entire value object. Reply fields
provide no source clock, hardware synchronization or freshness guarantee.
Support follows the configured fixed format; matching a family name alone does
not qualify every firmware or alternate layout. Physical-device validation
remains pending.

[A&D communication manual, base edition](https://weighing.andonline.com/wp-content/uploads/2024/01/Apollo_Communication_Manual_ENG.pdf) ·
[A&D communication manual, revision B](https://www.aandd.jp/products/manual/balances/1WMPD4003801B_GXGF-A_Communication.pdf) ·
[A&D current model and firmware manuals](https://www.aandd.jp/products/weighing/balance/toploader/gx-ae_gf-a.htm)

## Sartorius SBI balances

Use `sartorius_sbi` for a dedicated, preconfigured SBI command interface.
Choose `format: "sbi16"` or `"sbi22"` explicitly; the lengths include CRLF.
The Entris II / Quintix Pro interface note documents these fixed layouts.

```json
{
  "version": 1,
  "sources": [{
    "id": "balance", "protocol": "sartorius_sbi", "format": "sbi22",
    "serial": {"device": "/dev/ttyUSB0", "baud": 9600,
               "data_bits": 8, "parity": "odd", "stop_bits": 1},
    "signal": "measurement", "every_ms": 1000, "timeout_ms": 1000
  }]
}
```

Configure SBI, manual output without stability (`IND.NO`), `HANDSHK.NONE`,
`AUTO.TAR.OFF` and `TAR./PRT.OFF` on the balance before capture. Disable
automatic/interval/load-change output, GLP/date/time and application extra lines.
USB must expose bidirectional Virtual COM rather than keyboard or printer mode.
These settings matter: the print command can toggle automatic output or trigger
post-print tare under other configurations. Capturefab does not discover or
change the balance's mode.

Serial defaults are 9600 baud, eight data bits, odd parity and one stop bit.
Match the actual configured interface using `baud`, `data_bits`, `parity` and
`stop_bits`. The documented 600–115200 baud choices, seven/eight data bits and
one stop bit are supported without hardware/software flow control. A numeric
TCP `address` can replace `serial` for an explicitly configured transparent
bridge; no native Ethernet service is inferred.

Each poll sends only `ESC P CR LF` and reads one bounded ASCII line. A numeric
sample retains the full `raw` reply, `sign_raw`, all nine `value_raw` slots and
three `unit_raw` slots. The exact JSON `value` is constructed without a floating
point round trip. SBI22 additionally retains its six-character `id_raw` and
optional trimmed `id`; these identify the following value, not the device.
Units and IDs remain received codes. Numeric lines do not establish stability,
net/gross role or hardware synchronization. Printer verification brackets and
alternate decimal/layout formats fail explicitly.

`High`, `Low`, `Cal.Ext.`, blank replies and documented error layouts remain
nonnumeric observations. Error numbers remain raw strings. Such samples omit
numeric `value` and `unit`, and a later ordinary poll can still succeed.
Malformed, oversized, truncated or late replies retire the channel permanently
without a partial value or retry. One original deadline covers connection,
request, fragmented reply and parsing. CSV/JSONL preserve the complete value
object and earlier completed polls remain recoverable. No source timestamp or
freshness guarantee is inferred from this protocol without transaction IDs.
Physical balance, driver and whole-firmware qualification remain pending.

[Sartorius SBI interface note, 09/2024](https://api.sartorius.com/document-hub/dam/download/295216/Interface-Description-Entris-II-Quintix-Pro-Technical-Note-en-Sartorius.pdf) ·
[Entris II operating instructions, interface and print settings](https://api.sartorius.com/document-hub/dam/download/62849/Manual_Entris-BCE_PrecisionBalances_WBC6001ca.pdf)

## Pfeiffer TPG vacuum gauge controllers

Use `pfeiffer_tpg` for a dedicated TPG361, TPG362 or TPG366 command interface.
Select the exact model and up to 1, 2 or 6 unique gauge channels respectively.
The native Ethernet interface normally uses TCP port 8000:

```json
{
  "version": 1,
  "sources": [{
    "id":"vacuum", "protocol":"pfeiffer_tpg", "model":"tpg366",
    "address":"192.0.2.10:8000", "every_ms":1000, "timeout_ms":1000,
    "signals":[{"name":"chamber","channel":1},{"name":"foreline","channel":6}]
  }]
}
```

For the USB command port, replace `address` with
`"serial":{"device":"/dev/ttyACM0","baud":9600}` or a Windows COM port.
The serial profile uses eight data bits, no parity, one stop bit and no flow
control. Supported baud rates are 9600, 19200, 38400, 57600 and 115200. Select
AUTO or mnemonic (MNE) mode on the controller. PV telegrams and RS485 multidrop
selection/direction control use different profiles.

The client checks `AYT` identity against the configured model, then brackets
selected `PRn` measurements with two `UNI` queries. Every query follows the
native CR → ACK/CRLF → ENQ → data/CRLF exchange. One poll deadline covers
connection, identity, unit checks and all channels. A changed unit, rejection,
malformed reply or timeout fails the entire poll and closes the connection.
There is no automatic reconnect. Equal unit checks do not prove a snapshot or
exclude a change and return between queries; keep measurement settings stable.

Each value retains `channel`, `status_code`, `status`, `reading_raw`, the full
`raw` reply, `reported_unit_code`, `reported_unit` and exact device identity.
Only status zero exposes a numeric `value`; all decimals avoid a floating-point
round trip. Other statuses remain observations: underrange, overrange,
sensor_error, sensor_off, no_sensor or identification_error. The documented
`5,2.0000E-2` no-sensor placeholder remains raw data without a valid numeric
measurement. Unit code zero stays `mbar/bar` and code five stays `Volt`;
Capturefab does not infer a pressure conversion. CSV/JSONL retain the complete
object. Replies carry no source timestamp.

Only parameter-free identity, unit and measurement queries are sent. The first
query character stops initial continuous output as the manuals describe. At
most 4096 queued printable/CR/LF bytes may be discarded before the first ACK;
`startup_bytes_discarded` records that count. Later framing is strict. Data
bodies are limited to 256 ASCII bytes. Continuous mode is not restarted at close.

This profile follows the vendor's TPG361/362 firmware V010400 and TPG366 firmware
V010300 manuals; physical CPU/firmware qualification remains pending.
[TPG361/362 communication protocol](https://www.shop.buschgroup.com/api/empolis/resource/environment/project1_p/documents/pfeifferSharepointProd/2181-bg5510ben_d.pdf) ·
[TPG366 communication protocol](https://www.idealvac.com/files/manuals/Pfeiffer_MaxiGauge_TPG366_Communication_Protocol.pdf.pdf).

## TCP and serial JSON Lines sensors

Use `tcp_jsonl` for a device or gateway that streams newline-delimited JSON over
a TCP connection. The adapter connects to the numeric `address` and only reads;
it sends no login, subscription or polling commands:

```json
{
  "version": 1,
  "sources": [{
    "id":"scale",
    "protocol":"tcp_jsonl",
    "address":"192.0.2.60:9000",
    "timeout_ms":1000
  }]
}
```

By default, each LF- or CRLF-terminated line must contain a reading object:

```jsonl
{"signal":"weight_kg","value":12.345,"source_time":"2026-11-10T14:00:00.123Z"}
{"signal":"stable","value":true}
```

Only `signal`, `value` and optional RFC 3339 `source_time` are accepted. `value`
can be any JSON value, with the same exact number preservation as recording and
export. To read one JSON value per line instead (for example, a scale sending
`12.345` followed by a newline), add `"signal":"weight_kg"` to the source config.
Every line then becomes that signal's value; an object is retained as a value,
without interpreting any of its keys as metadata. Plain non-JSON text, CSV and
binary device framing need a protocol-specific adapter or helper.

For a serial device, replace the TCP connection options with:

```json
{
  "id":"scale",
  "protocol":"serial_jsonl",
  "device":"/dev/ttyUSB0",
  "baud":115200,
  "data_bits":8,
  "parity":"none",
  "stop_bits":1,
  "signal":"weight_kg"
}
```

On Windows, set `device` to a COM name such as `"COM12"` instead of the Unix
terminal path. Namespaced virtual COM device paths are also accepted.

Serial defaults are 115200 baud, 8 data bits, no parity and one stop bit.
`data_bits` accepts 5–8; parity accepts `none`, `even` or `odd`; stop bits accepts
1 or 2; Windows rejects 5 data bits with 2 stop bits. Baud rates are 300, 600, 1200, 2400,
4800, 9600, 19200, 38400, 57600 and 115200. Extended rates are 230400, 460800,
691200, 921600, 1500000, 2000000, 2500000, 3000000, 3500000 and 4000000 on
64-bit Linux x86_64/aarch64 GNU or musl, 64-bit macOS x86_64/aarch64, and Windows.
The selected interface and driver must support the exact rate; setup failure
does not fall back to a different speed. Other targets reject extended rates
before opening the port. Individual protocol adapters may allow fewer rates.
Flow control is disabled. Input is not flushed when acquisition begins. The
serial port must be a terminal/COM device. Unix uses an advisory lock against
another cooperating Capturefab source; Windows opens the COM device exclusively.
Windows line/overrun/parity errors fail explicitly. No measurement requests or configuration bytes are sent.

These sources run continuously; `every_ms` does not throttle them. Idle read
timeouts retain partial lines and continue waiting. At least one complete valid
reading is required before a run can complete successfully. Complete records already buffered at the acquisition cutoff are drained before
the run footer. A closed connection, malformed/oversized line, or unfinished
trailing line fails explicitly. Lines and serialized batches are limited to 1 MiB; bounded
batches avoid unbounded memory use when a sensor sends continuously. TCP JSONL
has no TLS or application authentication; use MQTT/TLS or OPC-UA for those
protocols' verified connections.

## MQTT subscriptions

```json
{
  "version": 1,
  "sources": [{
    "id": "line1",
    "protocol": "mqtt",
    "broker": "192.0.2.30:1883",
    "topics": ["line1/temperature", "line1/press/+"],
    "timeout_ms": 1000
  }]
}
```

`broker` is a numeric IP and port. `topics` contains 1–256 MQTT topic filters;
`+` and a final `#` follow MQTT wildcard rules. An optional `client_id` selects the
client identifier; otherwise Capturefab generates a unique identifier and opens
a clean session. Optional `qos` selects the requested subscription QoS (0, 1 or 2),
defaulting to 1. The adapter accepts publications at QoS 0–2 and completes their
acknowledgements. QoS 2 retransmissions with the same in-flight packet identifier
produce one sample; identifier reuse after PUBCOMP is a new message. This protocol
deduplication does not deduplicate separate application publications. A QoS 2
sample is delivered from the initial PUBLISH without waiting for PUBREL; its
timestamp retains the adapter read-completion semantics described below. A run can end
between handshake steps; recordings are bounded captures, not persistent broker
sessions.

Anonymous plain TCP remains the default. Set `username_env` and optionally
`password_env` to the names of UTF-8 environment variables for broker sign-in.
`password_env` requires `username_env`. Empty credential values are allowed;
missing variables fail with `auth_required`. Credentials are not written to the
recording. Plain TCP sends them without encryption; configure TLS when needed.

For TLS, add this object to the MQTT source:

```json
{
  "tls": {
    "server_name": "broker.plant.example",
    "ca_certificate": "/etc/plant/ca.pem",
    "client_certificate": "/etc/plant/reader.pem",
    "client_key": "/etc/plant/reader-key.pem"
  },
  "username_env": "MQTT_USER",
  "password_env": "MQTT_PASSWORD"
}
```

`server_name` is the certificate DNS name (also used for SNI) or IP address; it
is verified against the peer certificate. The numeric `broker` address still
selects the connection destination without DNS resolution. `ca_certificate` is an
optional PEM CA bundle which replaces the built-in public roots. Omit it to use
the public trust store. Omit both client fields unless the broker requires mutual
TLS. A client certificate chain and its matching unencrypted PKCS#1, PKCS#8 or
SEC1 PEM key must be supplied together. PEM files are limited to 1 MiB each.
Certificate verification cannot be disabled. TLS needs the `mqtt-tls` Cargo
feature, enabled in default builds; `doctor --json` reports `features.mqtt_tls`.
The read deadline includes TCP connection and all TLS handshake I/O.

MQTT 5 and persistent sessions are not implemented. A broker rejection,
disconnect or malformed packet stops recording. Packets are bounded to 1 MiB.

Each publication's topic becomes its `signal`. A valid JSON payload becomes the
sample's JSON `value`; other UTF-8 payloads become strings. Non-UTF-8 payloads
become `{"encoding":"base64","data":"..."}`. The payload is not flattened or
interpreted as a timestamp; any time inside the payload stays part of `value`.

## NI-DAQmx analog voltage acquisition

`ni_daqmx` captures analog voltage, thermocouple or RTD temperature, current,
strain, bridge ratio, acceleration, resistance, thermistor temperature, physical
force/pressure/torque bridges, IEPE velocity/force, charge acceleration, LVDT/RVDT or explicit mixed AI input through a separately installed NI-DAQmx runtime. Continuous internal-clock input
is the default; explicit `timing: "on_demand"` selects software-timed reads. Build with
`--features ni-daqmx` on a Linux or Windows host supported by the installed NI
driver and device. The feature is included in default builds and can be added explicitly to custom
minimal builds; `doctor --json` reports whether it was compiled. Capturefab
loads the runtime in its own contained worker process only when this source runs.
No Python, shell bridge or redistributed NI driver is required. This profile
uses NI-DAQmx 8.9+ pointer-sized task handles and the documented fixed C API;
the installed driver's device and host support still determines availability.
Explicit software-only `digital_port`, `counter_count`, `counter_frequency`,
`counter_period`, `counter_pulse_width`, `counter_angular_position` and
`counter_linear_position` measurements use the same isolated worker and owned
input-task lifecycle.

Voltage is the default measurement. Existing voltage configs remain valid;
`"measurement":"voltage"` can also be explicit. Configure individual physical
input channels and the input range:

```json
{
  "id": "daq_voltage",
  "protocol": "ni_daqmx",
  "signal": "voltage",
  "channels": ["Dev1/ai0", "Dev1/ai1"],
  "terminal_config": "differential",
  "min_volts": -10.0,
  "max_volts": 10.0,
  "rate_hz": 1000.0,
  "max_samples_per_channel": 256,
  "host_buffer_samples_per_channel": 8192,
  "timeout_ms": 2000
}
```

Use the names assigned by NI, such as `Dev1/ai0` or `cDAQ1Mod1/ai0`.
Voltage `channels` accepts 1–64 distinct individual ASCII AI names, each at most
256 bytes. Ranges, comma lists, wildcards, virtual aliases and named MAX tasks are
excluded. The driver must confirm the physical channel count and read order
before acquisition. `terminal_config` is `default`, `rse`, `nrse`,
`differential` or `pseudodifferential`; the device decides which modes and
voltage ranges it supports. A continuous rate must be finite and positive, and
`min_volts` must be finite and smaller than `max_volts`.

For thermocouples, choose `"measurement":"thermocouple"` and use channel
objects with explicit type, units, range and cold-junction compensation (CJC),
following NI's [thermocouple channel API](https://www.ni.com/docs/en-US/bundle/ni-daqmx-c-api-ref/page/group__ni-daqmx__c__functions__channel__configuration__creation__create__analog__input__channels_1gab4d78206aa204316fecb740a123b38f7.html):

```json
{
  "id": "daq_temperature",
  "protocol": "ni_daqmx",
  "measurement": "thermocouple",
  "signal": "temperature",
  "channels": [
    {
      "physical": "cDAQ1Mod1/ai0",
      "thermocouple_type": "k",
      "units": "deg_c",
      "min_temperature": -50,
      "max_temperature": 500,
      "cjc": {"source": "built_in"}
    },
    {
      "physical": "cDAQ1Mod1/ai1",
      "thermocouple_type": "t",
      "units": "deg_f",
      "min_temperature": -50,
      "max_temperature": 300,
      "cjc": {"source": "constant", "value": 77}
    }
  ],
  "rate_hz": 10,
  "max_samples_per_channel": 8,
  "host_buffer_samples_per_channel": 128,
  "cleanup_timeout_ms": 500,
  "timeout_ms": 1000
}
```

Thermocouple `channels` accepts 1–64 objects whose `physical` fields follow the
same individual-name limits. Types are `j`, `k`, `n`, `r`, `s`, `t`, `b`, `e`,
`a` or `c`; units are `deg_c`, `deg_f`, `kelvin` or `deg_r`. Each finite
`min_temperature` must be smaller than its finite `max_temperature`. Channel
types, units, ranges and CJC settings may differ within one task. String and
object channels cannot be mixed, and voltage-only range fields are rejected in
the thermocouple measurement.

CJC is either `{"source":"built_in"}` or
`{"source":"constant","value":77}` with a finite value in that channel's
units, as specified by NI's [constant CJC value property](https://www.ni.com/docs/en-US/bundle/ni-daqmx-c-api-ref/page/group__ni-daqmx__c__properties__channel__analog__input__temperature__thermocouple_1ga891928460b33d382f9f8dd718f75d871.html).
In the Fahrenheit channel above, 77 means 77 degrees Fahrenheit;
Capturefab does not convert it to Celsius. Built-in CJC does not expose a
configured or measured junction temperature. External CJC channels, custom
scales and named MAX tasks are excluded.

A thermocouple channel may request its own `terminal_config`: `rse`, `nrse`,
`differential` or `pseudodifferential`. Omission or `"default"` leaves the
driver default unqueried and sends no terminal-setting request. The module and
driver determine which wiring modes, temperature ranges, CJC sources and rates
are supported; accepting the config does not qualify a particular module.

For current input, choose `"measurement":"current"`. Each channel explicitly
declares its terminal mode, ampere range and built-in or external shunt:

```json
{
  "id": "daq_current",
  "protocol": "ni_daqmx",
  "measurement": "current",
  "signal": "loop_current",
  "channels": [
    {
      "physical": "Dev1/ai0",
      "terminal_config": "default",
      "min_amperes": 0,
      "max_amperes": 0.020,
      "shunt": {"source": "built_in"}
    },
    {
      "physical": "Dev1/ai1",
      "terminal_config": "rse",
      "min_amperes": 0.004,
      "max_amperes": 0.020,
      "shunt": {"source": "external", "ohms": 250}
    }
  ],
  "rate_hz": 1000,
  "max_samples_per_channel": 256,
  "host_buffer_samples_per_channel": 8192,
  "timeout_ms": 2000
}
```

Current `channels` accepts 1–64 objects with the same individual physical-name
limits. Every channel requires `terminal_config`: `default`, `rse`, `nrse`,
`differential` or `pseudodifferential`. The current-channel constructor receives
that request; `default` delegates to NI without querying the actual wiring.
Each finite `min_amperes` must be smaller than its finite `max_amperes`.
The device and driver determine supported terminals, ranges, shunts and rates;
there is no generic 20 mA software ceiling.

Shunt selection is required: `{"source":"built_in"}` or
`{"source":"external","ohms":250}` with finite positive external resistance
in ohms. Built-in mode accepts no configured resistance value and publishes no
inferred or queried built-in resistance. External resistance remains an
operator request, not a device readback. No retry with another shunt selection
or resistor value conceals a native rejection.

Current values use amperes only, with no `units` choice, custom scale or TEDS
mode in this profile. A 4–20 mA range is `0.004` to `0.020` amperes.
Capturefab preserves the already-scaled driver values without V/R arithmetic,
resistor multiplication or conversion to milliamps. It does not configure
sensor excitation or loop power. See NI's [current-channel API](https://www.ni.com/docs/en-US/bundle/ni-daqmx-c-api-ref/page/group__ni-daqmx__c__functions__channel__configuration__creation__create__analog__input__channels_1gadc9361c4cad612aeae3abe022076f5eb.html)
for the native constructor contract.

For resistance temperature detectors, select `"measurement":"rtd"`. Every
channel explicitly declares its RTD type, output unit, expected range, wiring,
sensor reference resistance and excitation. The following values illustrate
operator requests; choose wiring and excitation from the actual sensor and
device documentation before running them. This is not a named-module preset:

```json
{
  "id": "daq_rtd",
  "protocol": "ni_daqmx",
  "measurement": "rtd",
  "signal": "temperature",
  "channels": [
    {
      "physical": "Dev1/ai0",
      "rtd_type": "pt3851",
      "units": "deg_c",
      "min_temperature": -50,
      "max_temperature": 150,
      "resistance_config": "three_wire",
      "r0_ohms": 100,
      "excitation": {"source": "internal", "amperes": 0.001}
    },
    {
      "physical": "Dev1/ai1",
      "rtd_type": "pt3928",
      "units": "deg_f",
      "min_temperature": -40,
      "max_temperature": 300,
      "resistance_config": "four_wire",
      "r0_ohms": 1000,
      "excitation": {"source": "external", "amperes": 0.0005}
    }
  ],
  "rate_hz": 10,
  "max_samples_per_channel": 8,
  "host_buffer_samples_per_channel": 128,
  "timeout_ms": 1000,
  "cleanup_timeout_ms": 500
}
```

RTD `channels` accepts 1–64 objects with the same individual physical-name
limits. Required `rtd_type` is `pt3750`, `pt3851`, `pt3911`, `pt3916`, `pt3920`
or `pt3928`; required `units` is `deg_c`, `deg_f`, `kelvin` or `deg_r`.
Each channel needs finite `min_temperature < max_temperature` in that unit,
`resistance_config` of `two_wire`, `three_wire` or `four_wire`, and positive
finite `r0_ohms`. R0 is the sensor resistance in ohms at zero degrees Celsius,
even when output uses Fahrenheit, Kelvin or Rankine. Type names do not supply
a PT100/100-ohm default; wiring, R0 and excitation are never inferred.

`excitation` must specify `source: "internal"` or `"external"` and positive
finite `amperes`. Internal excitation requests the device's physical input
current source and can energize the connected sensor. External excitation
declares the operator-supplied source and amount; Capturefab does not create
that external source. These are settings requests, not measured or guaranteed
applied currents. RTD input is not physically passive. There are no AO, DO or
counter-output tasks, device resets or task takeovers.

RTD excludes `terminal_config`, CJC, shunt, custom-coefficient, custom-scale
and TEDS fields. The profile exposes six standard types and explicit positive
excitation; it does not expose NI's broader custom-RTD or no-excitation modes.
The native [RTD channel API](https://www.ni.com/docs/en-US/bundle/ni-daqmx-c-api-ref/page/group__ni-daqmx__c__functions__channel__configuration__creation__create__analog__input__channels_1ga3111dddf4f6eef35801096d09db7d574.html)
receives the requested R0 and excitation directly, with no local resistance or
Callendar–Van Dusen calculation. RTD uses the same continuous or on-demand
timing choices below and no thermocouple terminal setter.

[NI-9217](https://www.ni.com/docs/en-US/bundle/ni-9217-getting-started/page/overview.html)
and [NI-9216](https://www.ni.com/docs/en-US/bundle/ni-9216-getting-started/page/overview.html)
documentation describes 3/4-wire autodetection and per-channel excitation. That does not qualify every generic RTD type, R0,
2-wire API mode, external source, excitation amount or timing request on those
modules. It also does not prove physical 2-wire attachment is impossible;
adapted wiring requires separate manufacturer guidance. The actual module,
sensor, chassis, driver and OS determine acceptance. Native rejection retains
its original status without another wiring, current, type or timing choice.

For strain, bridge ratio or acceleration, select the corresponding measurement
and provide 1–64 individual AI channel objects. Each example below illustrates
the required fields, not a qualified sensor, module or wiring preset. Choose
the range, bridge arrangement, sensor constants, excitation and coupling from
the actual sensor and device documentation before connecting the input.

For strain, all range, gage and bridge-scaling requests are explicit:

```json
{
  "id": "daq_strain",
  "protocol": "ni_daqmx",
  "measurement": "strain",
  "signal": "strain",
  "channels": [
    {
      "physical": "Dev1/ai0",
      "min_strain": -0.01,
      "max_strain": 0.01,
      "bridge_type": "half_bridge_i",
      "excitation": {"source": "internal", "volts": 2.5},
      "gage_factor": 2,
      "initial_bridge_volts": 0.001,
      "nominal_gage_ohms": 350,
      "poisson_ratio": 0.3,
      "lead_wire_ohms": 1,
      "coupling": "dc"
    }
  ],
  "rate_hz": 1000,
  "max_samples_per_channel": 128,
  "host_buffer_samples_per_channel": 4096,
  "timeout_ms": 2000,
  "cleanup_timeout_ms": 500
}
```

`bridge_type` is `full_bridge_i`, `full_bridge_ii`, `full_bridge_iii`,
`half_bridge_i`, `half_bridge_ii`, `quarter_bridge_i` or `quarter_bridge_ii`.
The finite strain range requires `min_strain < max_strain`. `gage_factor` and
`nominal_gage_ohms` must be positive and finite; `lead_wire_ohms` must be finite
and nonnegative. `initial_bridge_volts` is the explicit unloaded bridge voltage,
not an automatically measured or tared offset. `poisson_ratio` must be finite;
Capturefab supplies no assumed material constant or universal material range.
NI's [strain channel API](https://www.ni.com/docs/en-US/bundle/ni-daqmx-c-api-ref/page/group__ni-daqmx__c__functions__channel__configuration__creation__create__analog__input__channels_1gab03c96b23cedb809011edc2df6fbef3e.html)
applies its own scaling and unloaded-voltage subtraction. Capturefab preserves
the resulting strain without subtracting again or converting to microstrain.

For a Wheatstone bridge voltage ratio, select the requested native ratio unit:

```json
{
  "id": "daq_bridge_ratio",
  "protocol": "ni_daqmx",
  "measurement": "bridge_ratio",
  "signal": "bridge_ratio",
  "timing": "on_demand",
  "poll_interval_ms": 250,
  "channels": [
    {
      "physical": "Dev1/ai0",
      "min_ratio": -25,
      "max_ratio": 25,
      "units": "millivolts_per_volt",
      "bridge_config": "full_bridge",
      "excitation": {"source": "internal", "volts": 2.5},
      "nominal_bridge_ohms": 350
    }
  ],
  "timeout_ms": 2000,
  "cleanup_timeout_ms": 500
}
```

`units` is `volts_per_volt` or `millivolts_per_volt`; the finite requested
`min_ratio < max_ratio` uses that unit. `bridge_config` is `full_bridge`,
`half_bridge` or `quarter_bridge`, distinct from the seven strain layouts.
`nominal_bridge_ohms` is positive finite unloaded bridge resistance, not an
inferred individual gage resistance. The [bridge channel API](https://www.ni.com/docs/en-US/bundle/ni-daqmx-c-api-ref/page/group__ni-daqmx__c__functions__channel__configuration__creation__create__analog__input__channels_1gab4d2a28bcc4212e7787808315471db67.html)
returns the selected ratio. Capturefab does not divide by excitation, change
V/V to mV/V, or infer strain, force, pressure or torque from it.

For a voltage-sensitivity accelerometer, declare output and sensitivity units
separately:

```json
{
  "id": "daq_acceleration",
  "protocol": "ni_daqmx",
  "measurement": "acceleration",
  "signal": "acceleration",
  "timing": "on_demand",
  "poll_interval_ms": 250,
  "channels": [
    {
      "physical": "Dev1/ai0",
      "terminal_config": "default",
      "min_acceleration": -100,
      "max_acceleration": 100,
      "units": "m_s2",
      "sensitivity": 100,
      "sensitivity_units": "mv_per_g",
      "excitation": {"source": "internal", "amperes": 0.002},
      "coupling": "ac"
    }
  ],
  "timeout_ms": 2000,
  "cleanup_timeout_ms": 500
}
```

Output `units` is `g`, `m_s2` or `in_s2`; the finite range requires
`min_acceleration < max_acceleration` in that unit. Positive finite
`sensitivity` uses `mv_per_g` or `v_per_g`, independently of output units.
The required `terminal_config` uses the existing terminal enum; `default` is
driver-chosen and unqueried. NI's [acceleration units property](https://www.ni.com/docs/en-US/bundle/ni-daqmx-c-api-ref/page/group__ni-daqmx__c__properties__channel__analog__input__acceleration_1ga199533108ad6f83e277b0fc13273d73e.html)
and its [NI-owned channel wrapper](https://github.com/ni/nidaqmx-python/blob/1d60b6c53ef11dead0ba6aa9d6e2855351835ad7/generated/nidaqmx/task/collections/_ai_channel_collection.py)
support these three unit requests, although the C constructor's unit table
lists g only. The installed driver and device determine acceptance. This is
the voltage-sensitivity/IEPE constructor, not charge- or bridge-based
acceleration. Capturefab performs no sensitivity or g-to-SI conversion.

These three AI profiles require `excitation.source` of `internal`, `external`
or `none` and an explicit amount: `volts` for strain/ratio, `amperes` for
acceleration. Internal/external amounts must be positive and finite; none
amounts must be finite and nonnegative. The exact requested amount is always
passed, without a guessed dummy value or a claim that none ignores it. None
cannot supply excitation to a sensor that requires it, and unsupported values
retain their native error. Internal excitation can energize connected hardware;
an external source is supplied by the operator.

Optional `coupling` is `ac`, `dc` or `ground` on each new AI channel, including
mixed channels. An explicit request selects the fixed
[AI coupling setter](https://www.ni.com/docs/en-US/bundle/ni-daqmx-c-api-ref/page/group__ni-daqmx__c__functions__property__manipulation__setter_1ga09a94b1b31d76695e28d7d7c6c96027d.html)
before acquisition; omission makes no coupling call or query, and explicit
`null` is rejected. AC removes the DC component, DC retains it, and ground
removes the signal and measures ground. Ground is not a sensor zero or
calibration. The device decides which source, coupling and timing combinations
it supports; there is no alternate-setting retry.

Requested excitation is not measured delivered power or voltage/current.
For example, the [NI-9237 guide](https://www.ni.com/docs/en-US/bundle/ni-9237-getting-started/page/overview.html)
documents automatic voltage reduction under its total excitation-power limit,
and the [NI-9234 guide](https://www.ni.com/docs/en-US/bundle/ni-9234-getting-started/page/overview.html)
restricts enabled IEPE to AC coupling. These examples are not qualified module
presets or guarantees of all bridge arrangements. NI also documents that
[excitation can persist after Stop/Clear](https://knowledge.ni.com/KnowledgeArticleDetails?id=kA00Z000000P69oSAC&l=en-US).
Neither normal cleanup, none nor ground promises physical power-off. Capturefab
does not add a device reset, excitation-zero write, offset nulling, shunt
calibration or TEDS lookup. Sensor wiring, bridge completion, electrical limits
and actual device/chassis/driver/OS acceptance remain operator qualifications.

Direct resistance, current-excited thermistors, voltage-excited thermistors and
physical force/pressure/torque bridges use six additional measurement tags.
Each accepts 1–64 distinct individual AI channel objects, either timing mode,
and optional per-channel `coupling`. They also work as tagged entries in
MixedAI. The examples below illustrate required request shapes, not qualified
sensors, modules, excitation settings or calibration data. Replace the sensor
constants and wiring/excitation requests with those specified by the actual
sensor and device before running them.

For ordinary resistance, choose `"measurement":"resistance"`. Its output is
fixed to ohms; this is not RTD temperature and has no RTD type or R0 field:

```json
{
  "id": "daq_resistance",
  "protocol": "ni_daqmx",
  "measurement": "resistance",
  "signal": "resistance",
  "channels": [{
    "physical": "Dev1/ai0",
    "min_ohms": 0,
    "max_ohms": 512,
    "resistance_config": "three_wire",
    "excitation": {"source": "internal", "amperes": 0.0009765625},
    "coupling": "dc"
  }],
  "rate_hz": 10,
  "max_samples_per_channel": 8,
  "host_buffer_samples_per_channel": 128,
  "timeout_ms": 1000,
  "cleanup_timeout_ms": 500
}
```

`min_ohms` and `max_ohms` must be finite with `min_ohms < max_ohms`.
`resistance_config` is `two_wire`, `three_wire` or `four_wire`; the device
chooses which requests it supports. Excitation is an explicit current in
amperes. The profile accepts no `units`, coefficients, RTD type, R0 or custom
scale. It preserves the native resistance without host V/I calculation or
clipping returned words to the requested range. See NI's
[resistance constructor](https://www.ni.com/docs/en-US/bundle/ni-daqmx-c-api-ref/page/group__ni-daqmx__c__functions__channel__configuration__creation__create__analog__input__channels_1gaa8454a1de839ded8669cce2b4b25cca4.html).

For a current-excited thermistor, choose `"measurement":"thermistor_current"`
and supply all three sensor coefficients explicitly. These synthetic coefficient
values illustrate the fields; they are not a recommended universal sensor fit:

```json
{
  "id": "daq_thermistor_current",
  "protocol": "ni_daqmx",
  "measurement": "thermistor_current",
  "signal": "temperature",
  "channels": [{
    "physical": "Dev1/ai0",
    "min_temperature": -32,
    "max_temperature": 256,
    "units": "deg_c",
    "resistance_config": "three_wire",
    "excitation": {"source": "internal", "amperes": 0.0009765625},
    "coefficients": {
      "a": 0.0010986328125,
      "b": 0.000244140625,
      "c": 1.1920928955078125e-7
    }
  }],
  "rate_hz": 10,
  "max_samples_per_channel": 8,
  "host_buffer_samples_per_channel": 128,
  "timeout_ms": 1000,
  "cleanup_timeout_ms": 500
}
```

For a voltage-excited thermistor, select
`"measurement":"thermistor_voltage"`. It requires excitation in volts and a
separate reference resistor `r1_ohms`:

```json
{
  "id": "daq_thermistor_voltage",
  "protocol": "ni_daqmx",
  "measurement": "thermistor_voltage",
  "signal": "temperature",
  "timing": "on_demand",
  "poll_interval_ms": 250,
  "channels": [{
    "physical": "Dev1/ai0",
    "min_temperature": -16,
    "max_temperature": 128,
    "units": "deg_f",
    "resistance_config": "two_wire",
    "excitation": {"source": "external", "volts": 2.5},
    "coefficients": {
      "a": 0.0010986328125,
      "b": 0.000244140625,
      "c": 1.1920928955078125e-7
    },
    "r1_ohms": 10000,
    "coupling": "dc"
  }],
  "timeout_ms": 1000,
  "cleanup_timeout_ms": 500
}
```

Both thermistor profiles require finite `min_temperature < max_temperature`,
`units` of `deg_c`, `deg_f`, `kelvin` or `deg_r`, an explicit
`resistance_config` and a closed `coefficients` object containing finite
`a`, `b` and `c`. Capturefab adds no coefficient positivity or nonzero rule;
signed values and signed zero pass unchanged. NI's current
[thermistor model](https://www.ni.com/docs/en-US/bundle/ni-daqmx/page/thermistors.html)
is `1/T = A + B ln(R) + C[ln(R)]^3`, with T in Kelvin and R the measured
resistance. Coefficients must match that NI convention and the actual sensor's
valid fit interval. Selecting Fahrenheit, Celsius, Kelvin or Rankine output
does not trigger local coefficient fitting, temperature conversion or a
Steinhart–Hart calculation in Capturefab; the native driver scales the values.

`r1_ohms` must be positive and finite as a software admission rule. It describes
the voltage-divider reference resistor in ohms, not RTD R0 or an inferred
thermistor resistance at zero Celsius. It is absent from current-excited
thermistor configuration and its constructor. Both thermistor profiles reject
custom-scale, CJC, shunt, RTD and terminal-configuration fields. The native
[Iex](https://www.ni.com/docs/en-US/bundle/ni-daqmx-c-api-ref/page/group__ni-daqmx__c__functions__channel__configuration__creation__create__analog__input__channels_1gaf7dc57c48b676e22c788574fecb4f512.html)
and [Vex](https://www.ni.com/docs/en-US/bundle/ni-daqmx-c-api-ref/page/group__ni-daqmx__c__functions__channel__configuration__creation__create__analog__input__channels_1ga79514ce1961338c419f0d6e1157e7b83.html)
constructors are separate fixed APIs; no dummy R1 or custom-scale pointer is
appended to the current-excited API.

For physical force, pressure or torque from a Wheatstone bridge, use
`force_bridge`, `pressure_bridge` or `torque_bridge`. Each channel requires a
declared two-point mapping from electrical coordinates to physical coordinates.
The three unit roles are independent: `units` governs returned values and the
requested range, `calibration.electrical_units` describes electrical
coordinates, and `calibration.physical_units` describes the sensor's physical
calibration coordinates. The following synthetic mappings demonstrate those
roles; they are not sensor calibration certificates or wiring presets.

```json
{
  "id": "daq_force_bridge",
  "protocol": "ni_daqmx",
  "measurement": "force_bridge",
  "signal": "force",
  "timing": "on_demand",
  "poll_interval_ms": 250,
  "channels": [{
    "physical": "Dev1/ai0",
    "min_force": -64,
    "max_force": 1024,
    "units": "newtons",
    "bridge_config": "full_bridge",
    "excitation": {"source": "internal", "volts": 2.5},
    "nominal_bridge_ohms": 350,
    "calibration": {
      "electrical_units": "volts_per_volt",
      "physical_units": "pounds",
      "first": {"electrical": -0.0009765625, "physical": -4},
      "second": {"electrical": 0.005859375, "physical": 128}
    }
  }],
  "timeout_ms": 1000,
  "cleanup_timeout_ms": 500
}
```

```json
{
  "id": "daq_pressure_bridge",
  "protocol": "ni_daqmx",
  "measurement": "pressure_bridge",
  "signal": "pressure",
  "channels": [{
    "physical": "Dev1/ai0",
    "min_pressure": -0.125,
    "max_pressure": 64,
    "units": "psi",
    "bridge_config": "full_bridge",
    "excitation": {"source": "internal", "volts": 1.25},
    "nominal_bridge_ohms": 350,
    "calibration": {
      "electrical_units": "volts_per_volt",
      "physical_units": "pascals",
      "first": {"electrical": 0, "physical": 16},
      "second": {"electrical": 0.001953125, "physical": 2048}
    },
    "coupling": "dc"
  }],
  "rate_hz": 10,
  "max_samples_per_channel": 8,
  "host_buffer_samples_per_channel": 128,
  "timeout_ms": 1000,
  "cleanup_timeout_ms": 500
}
```

```json
{
  "id": "daq_torque_bridge",
  "protocol": "ni_daqmx",
  "measurement": "torque_bridge",
  "signal": "torque",
  "timing": "on_demand",
  "poll_interval_ms": 250,
  "channels": [{
    "physical": "Dev1/ai0",
    "min_torque": -8,
    "max_torque": 64,
    "units": "newton_meters",
    "bridge_config": "quarter_bridge",
    "excitation": {"source": "internal", "volts": 1.25},
    "nominal_bridge_ohms": 120,
    "calibration": {
      "electrical_units": "volts_per_volt",
      "physical_units": "inch_ounces",
      "first": {"electrical": -0.000244140625, "physical": -16},
      "second": {"electrical": 0.0029296875, "physical": 512}
    }
  }],
  "timeout_ms": 1000,
  "cleanup_timeout_ms": 500
}
```

Force `units` and calibration `physical_units` are `newtons`, `pounds` or
`kilogram_force`. Pressure uses `pascals`, `psi` or `bar`; torque uses
`newton_meters`, `inch_ounces`, `inch_pounds` or `foot_pounds`. Output and
calibration physical units may differ within that family, as shown above, but
cross-quantity units are rejected. `electrical_units` is `volts_per_volt` or
`millivolts_per_volt`. Capturefab passes all coordinates and unit choices to
NI without converting or interpolating them. See the native
[force](https://www.ni.com/docs/en-US/bundle/ni-daqmx-c-api-ref/page/group__ni-daqmx__c__functions__channel__configuration__creation__create__analog__input__channels_1ga2e610b43e3c00dac59d46de4c609b2ad.html),
[pressure](https://www.ni.com/docs/en-US/bundle/ni-daqmx-c-api-ref/page/group__ni-daqmx__c__functions__channel__configuration__creation__create__analog__input__channels_1ga58a8c28527b09fa4182a2fab25fbfe57.html)
and [torque](https://www.ni.com/docs/en-US/bundle/ni-daqmx-c-api-ref/page/group__ni-daqmx__c__functions__channel__configuration__creation__create__analog__input__channels_1gac0648326f7b53707894030813e538950.html)
constructors for the fixed request contracts.

Each family requires a finite `min_* < max_*` range, positive finite
`nominal_bridge_ohms` for the unloaded total bridge resistance, and
`bridge_config` of `full_bridge`, `half_bridge` or `quarter_bridge`. It has no
strain-gage-factor, Poisson ratio, individual-gage resistance or terminal field.
All four calibration coordinates must be finite. Electrical coordinates must
differ numerically; `+0` and `-0` coincide and are rejected by this software
rule. Declared pair order is preserved, with no sorting or relinking. Descending
pairs, negative slopes and equal physical coordinates are passed unchanged;
this does not promise NI will accept every mapping, and its original rejection
is preserved. Declaring two points is a scaling request, not a calibration
performed or verified by Capturefab.

For all six profiles, excitation requires explicit `source` and amount:
`internal`/`external` amounts are positive finite; `none` amounts are finite
and nonnegative. Resistance and current-excited thermistors use `amperes`;
voltage-excited thermistors and physical bridges use `volts`. The amount is
passed unchanged, including allowed signed zero for none, with no claim that
it is ignored or universally accepted. None cannot provide excitation to a
sensor that needs it. Internal excitation can energize the sensor; an external
source must be supplied by the operator. Optional `coupling: "ac"|"dc"|"ground"`
is missing-only, never null, and follows the explicit-request/no-query rules
above, including in MixedAI. Generic API support does not qualify every
wiring, coupling, sensor, module, chassis, timing mode, driver or OS combination.
The existing excitation-coercion/persistence and no-physical-off limitations
apply; there is no device reset, hidden excitation-zero write or calibration
call.

For different AI measurements in one owned task, select `"measurement":"mixed"`.
Each of 1–64 channel objects requires its own `measurement` tag: `voltage`,
`thermocouple`, `current`, `rtd`, `strain`, `bridge_ratio`, `acceleration`,
`resistance`, `thermistor_current`, `thermistor_voltage`, `force_bridge`,
`pressure_bridge` or `torque_bridge`, followed by that family's fields. Same-family
channels are also allowed, including voltage channels with different ranges or
terminal requests. Existing homogeneous configs and rows keep their defaults
and formats; Capturefab does not infer mixed mode from channel fields.

This example illustrates the shape, not a qualified four-sensor device or
wiring preset. Select channels, terminals, shunts and excitation from the
actual device and sensor documentation:

```json
{
  "id": "daq_mixed",
  "protocol": "ni_daqmx",
  "measurement": "mixed",
  "signal": "mixed_ai",
  "timing": "on_demand",
  "poll_interval_ms": 250,
  "channels": [
    {
      "measurement": "voltage",
      "physical": "Dev1/ai0",
      "terminal_config": "differential",
      "min_volts": -10,
      "max_volts": 10
    },
    {
      "measurement": "thermocouple",
      "physical": "Dev1/ai1",
      "thermocouple_type": "k",
      "units": "deg_c",
      "min_temperature": -50,
      "max_temperature": 150,
      "cjc": {"source": "built_in"}
    },
    {
      "measurement": "current",
      "physical": "Dev1/ai2",
      "terminal_config": "default",
      "min_amperes": 0,
      "max_amperes": 0.020,
      "shunt": {"source": "external", "ohms": 250}
    },
    {
      "measurement": "rtd",
      "physical": "Dev1/ai3",
      "rtd_type": "pt3851",
      "units": "deg_f",
      "min_temperature": -40,
      "max_temperature": 300,
      "resistance_config": "three_wire",
      "r0_ohms": 100,
      "excitation": {"source": "internal", "amperes": 0.001}
    }
  ],
  "timeout_ms": 1000,
  "cleanup_timeout_ms": 500
}
```

Mixed channels use the same distinct individual physical-name limits and
family-specific ranges, units, CJC, shunt, RTD and engineering-input requirements above. A voltage
entry requires its own `terminal_config`, `min_volts` and `max_volts`; these
fields must not appear at task level in mixed mode, even as `null`. No task-wide
unit applies. Both timing modes are supported. Capturefab resolves the union of
selected constructors before creating the task, requiring the thermocouple
terminal setter only for nondefault TC terminal requests. It adds each channel
to that one fresh task and verifies the actual driver read count and original
order before acquisition. An unsupported combination retains its native error;
there is no split into separate tasks, reordered sample mapping, retry with
different settings or timing fallback.

NI documents [mixed same-I/O task measurements](https://knowledge.ni.com/KnowledgeArticleDetails?id=kA00Z0000004APOSA2&l=en-US)
and [separate channel-creation calls for different measurements, ranges or units](https://www.ni.com/en/support/documentation/supplemental/21/signal-based-synchronization-of-analog-input-c-series-modules-wi.html).
That API support does not qualify a device, module, chassis, driver or OS
combination. A shared task can include driver-repeated slow-channel samples;
Capturefab preserves repeats and does not infer simultaneous or fresh physical
conversions from a returned scan.

For whole digital ports, select `"measurement":"digital_port"` with explicit
on-demand timing. `channels` contains 1–64 distinct individual port strings:

```json
{
  "id": "daq_ports",
  "protocol": "ni_daqmx",
  "measurement": "digital_port",
  "signal": "digital_state",
  "timing": "on_demand",
  "poll_interval_ms": 100,
  "channels": ["Dev1/port0", "Dev1/port1"],
  "timeout_ms": 1000,
  "cleanup_timeout_ms": 500
}
```

Port names have one ASCII device segment containing letters, digits, `_`, `-`
or `.`, one slash and lowercase `port` followed by a decimal index fitting
`u32`; each name is at most 256 bytes. Device-case and numeric-index aliases
cannot duplicate a column. No line suffix, range, comma list, wildcard or MAX
alias is accepted. Capturefab creates one AllLines channel per requested port,
assigning `cfdp0000` through `cfdp0063` in request order. Before starting, it
checks that exact driver read-name order and queries each channel's line count,
accepting 1–32. These names and widths are separate from the physical requests.

Digital input returns complete unsigned U32 words. Capturefab preserves every
bit without a line-count mask, signed cast, per-line expansion or invented pin
label. This profile does not expose individual-line byte reads, packed multiple
ports in one channel, filters, inversion or threshold settings. Input direction
and resource configuration can affect hardware; this is not passive monitoring.

For edge-count snapshots, choose `"measurement":"counter_count"`. `channels`
must be an array containing exactly one object, with explicit edge, initial
count and direction:

```json
{
  "id": "daq_counter",
  "protocol": "ni_daqmx",
  "measurement": "counter_count",
  "signal": "edge_count",
  "timing": "on_demand",
  "poll_interval_ms": 100,
  "channels": [{
    "physical": "Dev1/ctr0",
    "edge": "rising",
    "initial_count": 0,
    "direction": "up"
  }],
  "timeout_ms": 1000,
  "cleanup_timeout_ms": 500
}
```

Counter physical names use the same device alphabet and length bound, followed
by one slash and lowercase `ctr` plus a decimal `u32` index. The driver must
confirm exactly one normalized counter identity before starting. `edge` is
`rising` or `falling`, `initial_count` is an exact unsigned 32-bit integer, and
`direction` is `up` or `down`. The device decides which choices it supports;
there is no external-direction mode or fallback. A fresh owned task configures
its requested initial count and reserves counter resources. It creates its own
task; counter initialization and resource reservation can affect hardware.

Each returned count is a U32 snapshot, including zero and repeated values.
An API word width of 32 bits does not discover the hardware register width.
Capturefab does not unwrap decreases, count rollovers, accumulate deltas, infer
missed edges, compute a rate or reset a counter when its value decreases.

For frequency, choose `"measurement":"counter_frequency"` with exactly one
typed counter object. The following settings are illustrative requests, not a
device or routing preset:

```json
{
  "id": "daq_frequency",
  "protocol": "ni_daqmx",
  "measurement": "counter_frequency",
  "signal": "frequency",
  "timing": "on_demand",
  "poll_interval_ms": 250,
  "channels": [{
    "physical": "Dev1/ctr0",
    "min_hz": 1,
    "max_hz": 10000,
    "edge": "rising",
    "method": "low_frequency_one_counter",
    "measurement_time_seconds": 0.1,
    "divisor": 1,
    "input_terminal": "/Dev1/PFI1"
  }],
  "timeout_ms": 1000,
  "cleanup_timeout_ms": 500
}
```

Frequency requires finite `0 <= min_hz < max_hz`, explicit edge and method,
finite nonnegative `measurement_time_seconds` and exact `u32` `divisor`.
Methods are `low_frequency_one_counter`, `high_frequency_two_counter`,
`large_range_two_counter` or `dynamic_averaging`. These methods use different
measurement strategies and can reserve paired counters even though the task
has one channel. The driver and device determine supported methods and argument
values; supplied windows/divisors are requests, not queried or coerced settings.
The measurement window is not the host poll interval or the read timeout.
Capturefab preserves driver-scaled Hz without deriving frequency from host time,
count differences or local divisor arithmetic. See NI's
[frequency-method guidance](https://www.ni.com/en/support/documentation/supplemental/21/choosing-a-counter-frequency-measurement-method0.html).

Both counter measurements allow optional `input_terminal`. Omission performs
no terminal setter or getter and leaves the driver default unqueried; explicit
`null` is rejected. A supplied path is nonempty ASCII, at most 256 bytes, with
optional leading slash and nonempty slash-separated segments containing only
letters, digits, `_`, `-` or `.`. Capturefab passes that exact request to the
selected counter input-terminal setter before starting, without an output
route, guessed PFI pin or retry using the default. Whether the path exists or
is electrically/routably valid is a device/driver decision, not a readback of
the connected signal.

The fixed counter array read APIs have seven argument slots and no `fillMode`;
digital U32 reads have eight, including `fillMode`. Capturefab uses the matching
typed read and initialized buffer, with one requested sample and a separate
actual count. Frequency requests Hz with a mandatory NULL custom-scale pointer.
Only each selected constructor/read and a requested matching terminal setter
are required, not unused AI/DI/CI or hardware-timing exports. Native rejection
retains its original status. No counter output, output routing, device reset,
task takeover or hardware/software timing fallback is added.

These three input profiles require explicit `timing:"on_demand"` and reject
continuous clock/rate/buffer settings, including `null`, plus unrelated analog
fields. They cannot be entries in a mixed-AI task. They share original deadline,
host-completion clock, partial-read, deferred-diagnostic and cleanup rules.
Polling digital states cannot recover intervening transitions. Counter methods,
direction, routing, electrical limits and actual module/chassis/driver/OS
acceptance require separate manufacturer qualification; no physical hardware
validation or portable internal digital sample clock is implied.

Four further counter measurements use exactly one typed individual counter
object and explicit on-demand timing. These examples illustrate API requests,
not verified counters, encoder wiring or terminal routes. They use native
`ReadCounterF64` values rather than deriving time or position locally.

For period in seconds:

```json
{
  "id": "daq_period",
  "protocol": "ni_daqmx",
  "measurement": "counter_period",
  "signal": "period",
  "timing": "on_demand",
  "poll_interval_ms": 250,
  "channels": [
    {
      "physical": "Dev1/ctr0",
      "min_seconds": 0.00001,
      "max_seconds": 1,
      "edge": "rising",
      "method": "low_frequency_one_counter",
      "measurement_time_seconds": 0.000001,
      "divisor": 4
    }
  ],
  "timeout_ms": 2000,
  "cleanup_timeout_ms": 500
}
```

The finite range requires `0 <= min_seconds < max_seconds`. `edge` is
`rising` or `falling`; `method` is `low_frequency_one_counter`,
`high_frequency_two_counter` or `large_range_two_counter`.
`measurement_time_seconds` is explicitly finite and nonnegative, and `divisor`
is an exact unsigned 32-bit integer. The current
[Period C reference](https://www.ni.com/docs/en-US/bundle/ni-daqmx-c-api-ref/page/group__ni-daqmx__c__functions__channel__configuration__creation__create__counter__input__channels_1gafff33171661e94431afbd5a0abf4f73e.html)
specifically requires 0.000001/4 for the low method, despite describing these
slots as ignored; NI's pinned Python wrapper defaults to 0.001/4. The example
follows the C reference. Capturefab passes each operator request unchanged and
preserves native rejection, without rewriting the window/divisor or inheriting
frequency's dynamic-averaging method. Methods can reserve additional counters;
accepted resources and ranges remain device-specific.

For pulse width in seconds:

```json
{
  "id": "daq_pulse_width",
  "protocol": "ni_daqmx",
  "measurement": "counter_pulse_width",
  "signal": "pulse_width",
  "timing": "on_demand",
  "poll_interval_ms": 250,
  "channels": [
    {
      "physical": "Dev1/ctr1",
      "min_seconds": 0.0001,
      "max_seconds": 1,
      "starting_edge": "falling"
    }
  ],
  "timeout_ms": 2000,
  "cleanup_timeout_ms": 500
}
```

The same finite nonnegative ordered time range applies. Required
`starting_edge` is `rising` or `falling`; NI's
[pulse-width constructor](https://www.ni.com/docs/en-US/bundle/ni-daqmx-c-api-ref/page/group__ni-daqmx__c__functions__channel__configuration__creation__create__counter__input__channels_1gab7c2c214f995c0b0a065940d739b3168.html)
uses it to select the high or low pulse. Capturefab does not reconstruct edges,
calculate duty cycle or return a guaranteed paired high/low measurement.

For angular encoder position:

```json
{
  "id": "daq_angle",
  "protocol": "ni_daqmx",
  "measurement": "counter_angular_position",
  "signal": "angle",
  "timing": "on_demand",
  "poll_interval_ms": 250,
  "channels": [
    {
      "physical": "Dev1/ctr0",
      "decoding": "x4",
      "units": "degrees",
      "pulses_per_rev": 1024,
      "initial_angle": 0,
      "z_index": {"enabled": true, "value": 0, "phase": "a_high_b_high"}
    }
  ],
  "timeout_ms": 2000,
  "cleanup_timeout_ms": 500
}
```

`units` is `degrees` or `radians`; `pulses_per_rev` is an exact positive
unsigned 32-bit integer and `initial_angle` is finite in that unit. NI defines
PPR for one A or B signal, not their combined edge count. Capturefab passes it
unchanged rather than multiplying by the decoding factor or inferring encoder
resolution, velocity, homing or absolute position.

For linear encoder position:

```json
{
  "id": "daq_position",
  "protocol": "ni_daqmx",
  "measurement": "counter_linear_position",
  "signal": "position",
  "timing": "on_demand",
  "poll_interval_ms": 250,
  "channels": [
    {
      "physical": "Dev1/ctr1",
      "decoding": "x2",
      "units": "meters",
      "distance_per_pulse": 0.001,
      "initial_position": 0,
      "z_index": {"enabled": false, "value": 0, "phase": "a_high_b_high"}
    }
  ],
  "timeout_ms": 2000,
  "cleanup_timeout_ms": 500
}
```

`units` is `meters` or `inches`, `distance_per_pulse` is positive and finite in
that unit, and `initial_position` is finite. Angular PPR and linear distance
are different native parameter types; Capturefab performs no unit conversion
or local position calculation.

For either encoder, `decoding` is `x1`, `x2`, `x4` or `two_pulse_counting`.
The first three are quadrature modes; two-pulse counting uses A rising edges
to increment and B rising edges to decrement. Required `z_index` always
contains Boolean `enabled`, finite `value` in the output unit, and `phase` of
`a_high_b_high`, `a_high_b_low`, `a_low_b_high` or `a_low_b_low`, even when
disabled. No reset value or phase is supplied implicitly. Enabled Z indexing
can reset the measurement; it is not merely observation or proof of homing.
Both current NI [angular](https://www.ni.com/docs/en-US/bundle/ni-daqmx-labview-api-ref/page/vi-lib/daqmx/create/channels-llb/daqmx-create-channel-ci-position-angular-encoder-vi.html)
and [linear](https://www.ni.com/docs/en-US/bundle/ni-daqmx-labview-api-ref/page/vi-lib/daqmx/create/channels-llb/daqmx-create-channel-ci-position-linear-encoder-vi.html)
references say phase is ignored for two-pulse counting. That mode permits Z
indexing while retaining the explicit phase request without claiming a
quadrature-phase-qualified reset.

Period/pulse width permit optional `input_terminal`. Encoders permit optional
`a_input_terminal`, `b_input_terminal` and `z_input_terminal`; each uses the
same bounded, individual ASCII path rules as count/frequency. Omission leaves
the default unqueried, explicit `null` is rejected, and only supplied matching
input setters are resolved and called before Start. A Z-terminal request is
rejected when Z indexing is disabled. There is no guessed PFI path, route
probe, getter, default retry or output route. Actual encoder signals, electrical
levels and device/driver routing and counter resources remain unqualified.
These four profiles reject continuous fields and cannot enter MixedAI. Their
seven-slot reads contain no fill argument, request one sample into one F64
element and retain native partial/error/host-clock semantics. Time values are
native seconds, not reciprocal frequency or elapsed host time; position values
are native scaled values, not a reconstructed hardware counter or loss total.

For AI input, omit `timing` to preserve continuous acquisition and its existing rows. For
software-timed input, select `"timing":"on_demand"` and supply an exact integer
`poll_interval_ms` from 1 to 3600000. Reuse any homogeneous or mixed
AI channel definitions above; digital-port and counter inputs require this
explicit software mode. On-demand configs must omit `rate_hz`,
`max_samples_per_channel` and `host_buffer_samples_per_channel`; even explicit
`null` is rejected. For example:

```json
{
  "id": "daq_on_demand",
  "protocol": "ni_daqmx",
  "measurement": "thermocouple",
  "timing": "on_demand",
  "poll_interval_ms": 250,
  "signal": "temperature",
  "channels": [
    {
      "physical": "Dev1/ai0",
      "thermocouple_type": "k",
      "units": "deg_c",
      "min_temperature": -50,
      "max_temperature": 500,
      "cjc": {"source": "built_in"}
    }
  ],
  "timeout_ms": 2000,
  "cleanup_timeout_ms": 500
}
```

On-demand creates one fresh owned task and leaves its timing unmodified, following
NI's documented [software-timing route](https://www.ni.com/docs/en-US/bundle/ni-daqmx/page/hardwresoftwretiming.html).
It configures no sample clock, input buffer or overwrite policy and uses no
availability or nominal-rate getter. Each native read requests one scan across
all validated channels, with a positive timeout clipped to the original
operation deadline and recording cutoff. A successful or warning response with
zero scans is incomplete; a negative response with one valid scan preserves
that data before the error. Waiting does not call the SDK or renew that budget.

The interval spaces host pre-call admissions using the worker's own monotonic
frontier; it is not a hardware sample period. Initial configuration consumes
no interval. A slow read or delayed scheduler produces no catchup burst, and
repeated values are retained. Preemption between the host timestamp and SDK
entry prevents an exact SDK-entry-gap or real-time guarantee. The USB-TC01
[specification](https://download.ni.com/support/manuals/374918b.pdf) lists 250 ms
conversion and at most 4 S/s software-timed sampling; those model facts do not
prove a fresh conversion every 250 ms, generic NI rates, or a tested
USB-TC01/driver/OS combination. ADC mode and auto-zero remain unmodified and
unqueried. Unsupported settings retain their native status without a timing
fallback or task restart.

For continuous acquisition, `max_samples_per_channel` is 1–4096. Each returned block is additionally capped
at 4096 total channel-samples, so 64 channels return at most 64 scans per block.
`host_buffer_samples_per_channel` is 1–1048576, with at most 4194304 configured
channel-samples across the task. It controls the NI host input buffer, not the
device FIFO or all driver allocations. The task explicitly stops rather than
overwriting unread host samples. Hardware FIFO overflow and other native
errors also stop recording; Capturefab never seeks to newer samples or restarts
a failed task to conceal a gap.

An optional `library` selects an absolute installed-library path. The default
Linux loader uses `libnidaqmx.so`; Windows uses `nicai_utf8.dll`. Windows paths
must select that UTF-8 API, with no locale-encoded `nicaiu.dll` fallback.
macOS, NI-DAQmx Base and devices requiring another driver API are unsupported.
Use NI's [hardware and OS compatibility information](https://www.ni.com/en/support/documentation/compatibility/21/ni-hardware-and-operating-system-compatibility.html)
to check the actual module, chassis, driver version and host combination.

The first observation has `event: "configured"` and records the actual channel
order. Continuous observations retain the requested rate and driver-returned
`nominal_rate_hz`. On-demand observations instead include
`timing: {"mode":"on_demand","poll_interval_ms":250,"basis":"host_software_request_spacing"}`
and `clock: "software_on_demand_host_completion"`. They omit hardware-rate and
buffer fields, and contain one scan per analog block; `sample_index` counts
returned scans in this task epoch, not physical conversions. Voltage observations
have `event: "analog_voltage_block"`, a zero-based `sample_index`,
`samples_per_channel`, and a scan-major `samples` array. Each sample retains
`ieee754_le_hex`, the exact eight bytes of the driver-scaled F64 voltage in
little-endian hex. Finite samples also include numeric `volts`; infinities and
NaNs omit `volts`, retaining their original sign and payload bits. These are
scaled driver values, not raw ADC integer words.

Thermocouple observations instead have `event: "thermocouple_temperature_block"`
and `representation: "driver_scaled_f64_temperature"`. They retain the same
scan-major indexing and exact `ieee754_le_hex`; finite samples include
`temperature` and never `volts`. The matching `channel_measurements` vector
records each channel's configured type, units, range, CJC and terminal request.
`settings_provenance` is `requested_not_read_back`; `terminal_config_provenance` is
`driver_default_unqueried` or `explicit_request_not_read_back`.
These are requests, not queried sensor identities, wiring readbacks, measured
CJC temperatures or calibration facts. Capturefab does not convert between
temperature units: equal driver-returned F64 words stay equal under different
unit metadata. A finite temperature is not proof of sensor continuity or
accuracy. Nonfinite values omit `temperature` while retaining every raw bit.

Current observations have `event: "analog_current_block"` and
`representation: "driver_scaled_f64_current"`. Finite samples include
`amperes`; nonfinite samples omit that numeric field and retain every raw word,
including sign and NaN payload. Current samples never use `volts` or
`temperature`. Their ordered `channel_measurements` retain `units: "amps"`,
configured ranges, shunt and terminal requests with request-only provenance.
Neither these settings nor finite returned values establish physical shunt
resistance, loop power, sensor health or calibration.

RTD observations have `event: "rtd_temperature_block"` and
`representation: "driver_scaled_f64_rtd_temperature"`. They preserve the same
scan-major indexing and every `ieee754_le_hex` word. Finite values include
`temperature` in that channel's requested output unit; nonfinite values omit
that numeric field while retaining sign and payload bits. They do not use
`volts` or `amperes` as sample values and perform no local temperature-unit or
resistance conversion. `measurement: "rtd"`,
`settings_provenance: "requested_not_read_back"` and ordered
`channel_measurements` retain actual driver channel names with the requested
type, units, range, wiring, R0 and excitation. These settings do not establish
sensor identity, detected wiring, measured resistance/current, calibration or
ADC mode. Both timing modes retain the same host-clock and partial-read rules.

These measurements retain every `ieee754_le_hex` word, adding
the indicated numeric field only when finite. Values are not clipped or
converted, and nonfinite sign/payload bits remain available:

| Measurement | Event | Representation | Finite field / units |
|---|---|---|---|
| `strain` | `strain_block` | `driver_scaled_f64_strain` | `strain` / strain |
| `bridge_ratio` | `bridge_ratio_block` | `driver_scaled_f64_bridge_ratio` | `ratio` / selected ratio unit |
| `acceleration` | `acceleration_block` | `driver_scaled_f64_acceleration` | `acceleration` / selected acceleration unit |
| `resistance` | `resistance_block` | `driver_scaled_f64_resistance` | `ohms` / ohms |
| `thermistor_current` | `thermistor_current_block` | `driver_scaled_f64_thermistor_current` | `temperature` / selected temperature unit |
| `thermistor_voltage` | `thermistor_voltage_block` | `driver_scaled_f64_thermistor_voltage` | `temperature` / selected temperature unit |
| `force_bridge` | `force_bridge_block` | `driver_scaled_f64_force_bridge` | `force` / selected force unit |
| `pressure_bridge` | `pressure_bridge_block` | `driver_scaled_f64_pressure_bridge` | `pressure` / selected pressure unit |
| `torque_bridge` | `torque_bridge_block` | `driver_scaled_f64_torque_bridge` | `torque` / selected torque unit |
| `counter_period` | `counter_period_block` | `driver_scaled_f64_counter_period` | `period_seconds` / seconds |
| `counter_pulse_width` | `counter_pulse_width_block` | `driver_scaled_f64_counter_pulse_width` | `pulse_width_seconds` / seconds |
| `counter_angular_position` | `counter_angular_position_block` | `driver_scaled_f64_counter_angular_position` | `angle` / selected angular unit |
| `counter_linear_position` | `counter_linear_position_block` | `driver_scaled_f64_counter_linear_position` | `position` / selected linear unit |

Each has `measurement`, `settings_provenance:"requested_not_read_back"` and
ordered `channel_measurements` with zero-based `channel_index`, actual driver
`physical` and the exact typed range/unit/scientific requests. Coupling is
recorded as `coupling_request` only when explicit, with
`coupling_provenance:"explicit_request_not_read_back"`; omission records
`driver_default_unqueried`. Acceleration records `terminal_config_request` and
`terminal_config_provenance`; a `default` request remains explicit while its
driver-selected terminal mode stays unqueried. CI also retains `requested_physical` and
`input_terminal_request`/`input_terminal_provenance`, or encoder
`a_input_terminal_request`, `b_input_terminal_request`, `z_input_terminal_request`
and their matching provenance keys. An omitted terminal has no request value;
disabled Z uses `z_input_terminal_provenance:"not_requested_z_disabled"`.
`z_index_phase_applicability` is `z_index_disabled`,
`ignored_for_two_pulse_counting` or `quadrature_phase_request`, derived only
from the configuration. These fields do not establish actual excitation,
coupling, routes, reset occurrence, sensor calibration or physical position.

Resistance metadata adds fixed `units:"ohms"`; thermistor metadata retains
requested units, coefficients and, for voltage excitation only, `r1_ohms`.
These are operator requests, not measured resistance, verified curve fits or
sensor identities. Each physical-bridge channel keeps the original nested
`calibration` and adjacent
`calibration_provenance:"declared_two_point_linear_request"`, preserving
independent electrical/calibration/output units. This records a declared
mapping, not a measured or performed sensor calibration. All six use
`settings_provenance:"requested_not_read_back"` and the coupling provenance
rules above, with no invented terminal field.

Mixed observations have `event: "mixed_analog_block"`,
`representation: "driver_scaled_f64_mixed_analog"`, `measurement: "mixed"` and
`settings_provenance: "requested_not_read_back"`. One ordered
`channel_measurements` array binds each zero-based `channel_index` to the
actual driver `physical` name, its `measurement`, `quantity`, `units` and
family-specific requests. `quantity` is `voltage`, `current`, `temperature`,
`strain`, `bridge_ratio`, `acceleration`, `resistance`, `force`, `pressure` or
`torque`; TC, RTD and both thermistor modes retain their distinct measurement
tags. Voltage/current terminal
requests and optional TC terminal requests retain the provenance rules above;
RTD retains type, range, wiring, R0 and excitation without an invented terminal
field. Engineering channels retain the scientific and coupling requests above.
Units are `volts`, `amps`, `strain` or the channel's selected temperature,
ratio, acceleration, force, pressure or torque unit; resistance uses `ohms`.
The six new families retain their typed requests and calibration provenance
above. These settings remain requests rather than hardware readbacks.

`samples[scan][channel_index]` keeps each exact `ieee754_le_hex` word and the
finite family value: `volts`, `amperes`, `temperature`, `strain`, `ratio`,
`acceleration`, `ohms`, `force`, `pressure` or `torque`. Nonfinite words omit
that numeric field. Measurement tags and units are not repeated per word;
use the matching column descriptor, with no global unit or local conversion.
All columns retain the block's original host completion clock, with no
per-channel physical timestamp or suppression of repeated values. One task
does not establish interchannel simultaneity or a fresh conversion per scan.

Digital-port observations use `event:"digital_port_block"` and
`representation:"sdk_u32_digital_port"`. Each scan-major sample has unsigned
`value` and canonical four-byte `u32_le_hex`. `channels` lists the actual
assigned read names, while `channel_measurements` records zero-based
`channel_index`, original `physical`,
`physical_provenance:"constructor_request_not_read_back"`, `read_channel`,
queried `line_count`, `line_count_provenance:"driver_channel_property"` and
`line_grouping:"chan_for_all_lines"`. The observed width is not a mask or a
physical port readback. No voltage units, booleans, named bit labels or
per-port timestamp are inferred.

Count observations use `counter_count_block`/
`driver_unscaled_u32_counter_count`, with unsigned `count` and `u32_le_hex`.
Frequency observations use `counter_frequency_block`/
`driver_scaled_f64_counter_frequency`, with every `ieee754_le_hex` word and
finite `frequency_hz`; nonfinite words omit that numeric field and retain sign
and payload bits. Values are not clipped to requested ranges. Both retain
actual counter identity, `requested_physical` and family requests in
`channel_measurements`, with `settings_provenance:"requested_not_read_back"`.
Count's `api_word_bits:32` describes the API representation, not measured
hardware width. Frequency records `units:"hz"` and requested range, edge,
method, measurement time and divisor. An explicit source appears as
`input_terminal_request` with `explicit_request_not_read_back` provenance;
omission leaves no terminal value and `driver_default_unqueried` provenance.
Received value zero is valid data; an actual native sample count of zero is
incomplete on a successful or warning software read. Sample indices count
returned observations, not physical edges or unique frequency measurements.

The time-series `samples` result counts recorded observations, including
metadata events; it is not the number of ADC conversions.

Each native input block uses the worker's host monotonic clock immediately after the
native read completes, before serialization or pipe delivery. Delivery latency
is recorded separately. All samples in that block share this host observation
window. The nominal clock setting and scan indices do not establish actual
per-sample timestamps, interchannel simultaneity, clock accuracy or UTC
synchronization. `source_time` is absent in both timing modes. Continuous input
configures only the internal sample clock; on-demand input uses the host
completion clock without hardware-rate metadata. External clocks and finite
tasks are excluded.

Native warning/status codes remain visible. Read rows publish their data and raw
codes before optional diagnostic calls, marking diagnostic text as `deferred`.
Finish retrieves the first negative error's thread-local information before
other SDK calls, then stops/clears the owned task and performs bounded, deduplicated
positive warning lookups. Diagnostic lookup capacity or failure leaves every raw
read status intact; unscheduled text is marked unavailable. Retrieved diagnostics
retain a bounded raw prefix with its encoding and truncation state. If a failed native read returns
a valid partial block, that block is retained before the original terminal
error, including when recording ends before another read. A parent deadline or
worker crash cannot guarantee recovery of an incomplete IPC response. Complete
admitted responses keep their original clocks when drained.

Finish stops and clears only the task Capturefab created, with no further
acquisition read. Unread device/driver buffers are not claimed to be drained.
The optional `cleanup_timeout_ms` is 1–30000 (default 30000); one total budget
covers native stop/clear and worker completion. A shorter budget bounds the wait
for a hung driver and can leave cleanup unconfirmed. Once ready, neither timing
mode admits another acquisition RPC in the last 2 ms before the recording
cutoff; this software polling margin leaves an unread tail rather than starting
a new operation with an exhausted budget. It does not guarantee that a physical
conversion finishes in the remaining time. Every admitted read keeps its
original operation deadline and cutoff; an in-flight timeout is not swallowed
or given a fresh budget.
Forced worker termination leaves hardware cleanup unconfirmed. Busy devices
fail with their native status; no device reset, task takeover, AO/DO or
counter-output task, calibration or saved-task mutation is attempted. RTD
and engineering-AI excitation remain explicit physical input settings; Stop/Clear
does not promise to remove them. Digital
input and counters require separate acquisition profiles.

The owned task/read contract follows NI's
[text-based programming guide](https://www.ni.com/en/support/documentation/supplemental/21/using-ni-daqmx-in-text-based-programming-environments.html)
and [C read reference](https://github.com/ni/grpc-device/wiki/NI-DAQMX-Read-Functions).
Validation uses an independent synthetic C ABI fixture on Linux and strict
cross-compilation for Windows; it does not establish physical device or
installed-driver qualification.

### IEPE velocity and force, charge acceleration, LVDT and RVDT

Choose a timing mode supported by the actual module. [NI's C Series timing guide](https://www.ni.com/docs/en-US/bundle/ni-daqmx/page/cdaqtimingconsiderations.html) states that C Series Delta-Sigma devices require hardware timing and do not support on-demand input. The first three examples use continuous timing; the LVDT/RVDT examples illustrate explicit on-demand requests for a device/driver that supports them. The driver's original result remains authoritative.

These five measurements support continuous and on-demand input, including per-channel tags in `measurement: "mixed"` with any of the eighteen AI kinds. The examples below show complete source shapes; choose the physical range, sensor sensitivity, excitation and wiring from the actual sensor and device documentation. Their numbers are illustrative requests, not hardware defaults or calibration results.

`measurement: "velocity_iepe"`:

```json
{
  "id": "daq_velocity_iepe",
  "protocol": "ni_daqmx",
  "signal": "velocity_iepe",
  "measurement": "velocity_iepe",
  "channels": [
    {
      "physical": "Dev1/ai0",
      "terminal_config": "differential",
      "min_velocity": -0.375,
      "max_velocity": 1.625,
      "units": "m_s",
      "sensitivity": 0.3125,
      "sensitivity_units": "mv_per_mm_s",
      "excitation": {
        "source": "internal",
        "amperes": 0.001953125
      }
    }
  ],
  "timeout_ms": 2000,
  "rate_hz": 1000.0,
  "max_samples_per_channel": 256,
  "host_buffer_samples_per_channel": 8192
}
```

`measurement: "force_iepe"`:

```json
{
  "id": "daq_force_iepe",
  "protocol": "ni_daqmx",
  "signal": "force_iepe",
  "measurement": "force_iepe",
  "channels": [
    {
      "physical": "Dev1/ai0",
      "terminal_config": "nrse",
      "min_force": -2.5,
      "max_force": 9.5,
      "units": "newtons",
      "sensitivity": 0.875,
      "sensitivity_units": "mv_per_newton",
      "excitation": {
        "source": "internal",
        "amperes": 0.00244140625
      }
    }
  ],
  "timeout_ms": 2000,
  "rate_hz": 1000.0,
  "max_samples_per_channel": 256,
  "host_buffer_samples_per_channel": 8192
}
```

`measurement: "charge_acceleration"`:

```json
{
  "id": "daq_charge_acceleration",
  "protocol": "ni_daqmx",
  "signal": "charge_acceleration",
  "measurement": "charge_acceleration",
  "channels": [
    {
      "physical": "Dev1/ai0",
      "terminal_config": "differential",
      "min_acceleration": -0.5625,
      "max_acceleration": 4.0625,
      "units": "g",
      "sensitivity": 0.34375,
      "sensitivity_units": "pc_per_g"
    }
  ],
  "timeout_ms": 2000,
  "rate_hz": 1000.0,
  "max_samples_per_channel": 256,
  "host_buffer_samples_per_channel": 8192
}
```

`measurement: "lvdt"`:

```json
{
  "id": "daq_lvdt",
  "protocol": "ni_daqmx",
  "signal": "lvdt",
  "measurement": "lvdt",
  "channels": [
    {
      "physical": "Dev1/ai0",
      "min_position": -0.40625,
      "max_position": 0.84375,
      "units": "meters",
      "sensitivity": 0.234375,
      "sensitivity_units": "mv_per_v_per_mm",
      "excitation": {
        "source": "internal",
        "volts": 2.125,
        "frequency_hz": 320.5
      },
      "ac_wire_mode": "four_wire"
    }
  ],
  "timing": "on_demand",
  "poll_interval_ms": 1000,
  "timeout_ms": 2000
}
```

`measurement: "rvdt"`:

```json
{
  "id": "daq_rvdt",
  "protocol": "ni_daqmx",
  "signal": "rvdt",
  "measurement": "rvdt",
  "channels": [
    {
      "physical": "Dev1/ai0",
      "min_angle": -1.0625,
      "max_angle": 5.6875,
      "units": "degrees",
      "sensitivity": 0.1796875,
      "sensitivity_units": "mv_per_v_per_radian",
      "excitation": {
        "source": "internal",
        "volts": 3.125,
        "frequency_hz": 320.125
      },
      "ac_wire_mode": "five_wire"
    }
  ],
  "timing": "on_demand",
  "poll_interval_ms": 1000,
  "timeout_ms": 2000
}
```

Velocity output `m_s` or `in_s` is independent of sensitivity `mv_per_mm_s` or `mv_per_in_s`; the millimeter sensitivity does not cause a local factor of1000. Force output `newtons`, `pounds` or `kilogram_force` is independent of `mv_per_newton` or `mv_per_pound`. The ordinary C constructor table lists N/lbf; the generic NI force property/wrapper additionally supports requesting kgf, with the actual driver result preserved.

Charge output `g`, `m_s2` or `in_s2` uses a separately selected `pc_per_g`, `pc_per_m_s2` or `pc_per_in_s2`. It needs a charge-capable front end and has no excitation field. LVDT output `meters` or `inches` is independent of `mv_per_v_per_mm` or `mv_per_v_per_milli_inch` (per0.001inch). RVDT output `degrees` or `radians` is independent of `mv_per_v_per_degree` or `mv_per_v_per_radian`. LVDT/RVDT require appropriate AC conditioning and `four_wire`, `five_wire` or `six_wire`; they have no `terminal_config` field.

Every range must be finite and ordered. New-family sensitivity admits every finite signed/zero value unchanged, subject to the original native result; this does not relax the older acceleration profile. Current/voltage excitation requires explicit source and amount: internal/external amounts are positive, none amounts nonnegative. AC `frequency_hz` stays explicit and positive even with source none. None is not an ignored-value or power-off promise; an IEPE sensor requiring excitation cannot be qualified by selecting none. Optional non-null `coupling` is `ac`, `dc` or `ground`; ground measures ground rather than calibrating sensor zero.

Rows preserve exact native IEEE words and finite `velocity`, `force`, `acceleration`, `position` or `angle`, with per-channel output/sensitivity/excitation requests and provenance. No local integration, differentiation or unit conversion occurs. Mixed scans retain per-column quantities and units, without establishing physical simultaneity, fresh conversion or sensor calibration. The existing deadline, status, negative-prefix and owned Finish contract applies.

## Connect an industrial protocol helper

Use `"protocol":"helper"` with an explicit `executable` and optional `args` to
run a user-supplied vendor SDK or protocol bridge. This path accepts readings from
any device the helper can acquire, without labeling it as a built-in protocol.
For example, if your installed reader accepts these arguments:

```json
{
  "id": "vendor_sensor",
  "protocol": "helper",
  "executable": "/opt/plant/bin/vendor-reader",
  "args": ["--device", "sensor-01"],
  "timeout_ms": 1000
}
```

The existing `opc_ua`, `ethercat`, `profinet` and `ethernet_ip` helper overrides
also remain available. An OPC-UA-specific bridge could use:

```json
{
  "id": "plc",
  "protocol": "opc_ua",
  "executable": "/opt/plant/bin/opcua-reader",
  "args": ["--endpoint", "opc.tcp://192.0.2.10:4840", "--node", "ns=2;s=Pressure"],
  "timeout_ms": 1000
}
```

`opcua-reader` in this example is a user-supplied program, not a Capturefab command.
`executable` is executed directly and `args` defaults to an empty array; there is no
shell expansion. Acquisition starts when recording begins. The helper owns all
protocol-specific settings, security, subscriptions and device timestamps. Helpers
run on Unix or Windows. On Windows, use a drive-qualified absolute `.exe` path
such as `"C:\\Plant Tools\\reader.exe"`; relative paths, PATH lookup, UNC/device
paths and script-file handlers are refused. Arguments follow Windows CRT quoting;
spaces, quotes, backslashes, Unicode and empty arguments retain their boundaries.
Windows 10/Server 2016 or later with creation-time Job Object assignment is required.
Capturefab starts the process suspended in its own kill-on-close job and checks
containment before allowing execution. This controls cooperating descendants in
that job; externally managed SDK services remain the helper operator's responsibility.

The helper emits one JSON object followed by a newline for each measurement:

```jsonl
{"signal":"pressure_bar","value":12.5,"source_time":"2026-11-10T14:00:00.123Z"}
{"signal":"part_present","value":true}
```

Only `signal`, `value` and optional `source_time` are accepted. `source_time`, when
provided, must be RFC 3339. Omit it if the source does not provide a reliable
timestamp. `value` may be any JSON value. Do not put banners or diagnostic output
on stdout. Each line is limited to 1 MiB; a malformed or oversized line, an
unterminated final line, or closed stdout is a source failure. An idle helper may
remain silent between readings. Keep it running until Capturefab stops it.
Serialized batches are also limited to 1 MiB: large readings use separate batches,
and an individual reading that exceeds that budget fails explicitly. Complete
readings already read from stdout into the bounded capture buffer are drained at acquisition
cutoff before the run footer; an unfinished trailing line fails explicitly.
Buffered readings retain their original host receive clock rather than being
assigned the later drain time.

Helper stdin is closed and stderr is discarded so external SDK diagnostics cannot
bypass Capturefab's output redaction. On completion, interruption or failure,
Capturefab terminates the Unix process group or the exact Windows Job Object.
On Windows, cleanup separately waits for the primary process, an empty job and
the bounded stdout reader; an unconfirmed cleanup deadline fails explicitly.
The pipe applies backpressure instead of accumulating an unbounded queue of output.
Windows uses one 8 KiB reader and one mailbox slot, with at most 1 MiB plus
8 KiB of pending raw bytes; normalized batches remain limited to 256 rows and
1 MiB. These are application bounds, not an assertion about kernel pipe allocation.

## Understand the timeline and file format

All sources in a recording share a host monotonic clock origin. `elapsed_ns`
measures time since that origin, and `time` adds the elapsed duration to the
run's starting UTC time. A change to the host's wall clock during the run does not
jump this timeline. The clock label is `host_monotonic_receive`.

These are host receive timestamps. They include transport delay, buffering and
scheduling; they do not establish hardware clock synchronization, simultaneous
sampling or a fixed timing accuracy. Readings returned by a sensor adapter share its read completion time. JSONL streams and helpers retain the host receive clock of the bytes
completing each line, including when buffered records are drained at cutoff. Camera
stills instead carry the frame-receive time from the camera worker on the same
host-wide monotonic clock, before image encoding or storage. `read_duration_ns`
measures the interval from read start to that receive event; `delivery_latency_ns`
measures the remaining delivery/encoding/storage delay (zero for ordinary sensor
reads). These intervals do not bound the age of buffered or device-side samples. Device-provided `source_time` stays separate from the
host clock. Compare it across devices only when their clocks are synchronized.

The version 1 recording has these row types:

- One `header` row with `version`, `run_id`, `started_at`, `clock` and the source
  IDs and protocol names. Connection arguments and credentials are not copied
  into the header.
- `sample` rows with `seq`, `source`, `signal`, `time`, `elapsed_ns`,
  `read_duration_ns`, `delivery_latency_ns`, optional `source_time`, and `value`.
- One `end` row with the total `samples`, final `elapsed_ns`, and a `status` of
  `complete`, `interrupted` or `failed`.

`seq` starts at 1 and tracks file order. Concurrent sources can arrive at the
writer in a different order from their timestamps. For time-ordered analysis,
sort samples by `elapsed_ns`, with `seq` as a tie breaker. Source identifiers keep
equal signal names from different devices distinct.

## Export and recover partial recordings

```sh
capturefab data export run.jsonl -o pressure.csv --source press --signal pressure_bar
capturefab data export run.jsonl -o samples.jsonl --format jsonl
capturefab data export interrupted.jsonl -o recovered.csv --allow-incomplete
```

CSV columns are `seq,source,signal,time,elapsed_ns,read_duration_ns,delivery_latency_ns,source_time,value_json`.
The `value_json` cell contains JSON text, including for scalar numbers, booleans
and strings. Parse that cell as JSON to recover its type; objects and arrays stay
intact. Text fields are CSV-quoted. Missing `source_time` produces an empty cell.
JSON numbers in generic payloads retain their precision, including integers
outside 64-bit ranges and long decimal literals. Choose an exact numeric type in
your analysis tool when reading those JSON cells.
JSON Lines export writes sample objects without the recording's `kind` field,
header or end row. Filters retain the original sequence numbers and timestamps.

Export validates the header, contiguous sample sequence, source IDs, timestamps
and final sample count. By default it requires a successful `end` row. Use
`--allow-incomplete` for an interrupted or failed run, or a file without its final
row. That option preserves complete rows and can discard an unfinished last line;
it does not accept malformed complete rows or data after an `end` row. The export
result reports `complete: false` when the source recording did not finish
successfully. A failed export removes its new output file.

Ctrl-C stops acquisition and saves completed rows, with an interrupted status and
a nonzero command exit. Source failures likewise return a structured error; with
`--json`, `error.details` includes the recording path and saved sample count.

## EtherNet/IP symbolic tags

The default `logix` profile polls atomic tags through unconnected CIP Read
Tag requests and bounded Read Tag Fragmented continuations over plain TCP. Configure a numeric IP and port (normally 44818),
a unique output name for each tag, and the tag's symbolic path:

```json
{
  "version": 1,
  "sources": [{
    "id": "press",
    "protocol": "ethernet_ip",
    "address": "192.0.2.40:44818",
    "slot": 0,
    "every_ms": 100,
    "timeout_ms": 1000,
    "signals": [
      {"name": "pressure", "tag": "Pressure"},
      {"name": "cycles", "tag": "Program:Main.CycleCount"},
      {"name": "temperatures", "tag": "Temperatures[0]", "elements": 4}
    ]
  }]
}
```

Use `slot` for one backplane hop through port 1 to the processor in that slot.
Omit it when the TCP endpoint accepts the tag request directly. Multi-hop routes
are not implemented. The adapter establishes an encapsulation session lazily and
reuses it across polls; it closes the session when acquisition ends. The Logix
profile uses unconnected reads.

Each source accepts 1–256 signals. `tag` supports dot-separated members, program
scope such as `Program:Main.Tag`, and array indices such as `Samples[3]` or
`Matrix[1,2]`. Names must use ASCII symbolic identifiers. `elements` defaults to 1
and accepts 1–256 contiguous elements beginning at the selected tag/index. One
element produces a scalar value; more than one produces a JSON array. The reply's
atomic type determines decoding: BOOL, SINT, INT, DINT, LINT, USINT, UINT, UDINT,
ULINT, REAL, and LREAL are supported. Integer precision is preserved, and
non-finite floating point values fail explicitly.

All tag requests in one poll share `timeout_ms`, including TCP connection and
session registration. A controller rejection stops acquisition and reports its
CIP general and additional status. A valid partial-transfer reply automatically
continues with Read Tag Fragmented (`0x52`), retaining the original path/count and
advancing by value bytes. Every fragment must preserve the atomic type and make
progress; the final successful reply must complete the exact requested width.
Reassembly for atomic signals remains bounded to 256 elements (at most 2048 raw value bytes). Any
failed fragment or sibling tag prevents that poll from publishing samples.
Multiple requests do not freeze live PLC data or establish a device snapshot.
Decoded whole structures, cyclic I/O and CIP
Security require a different acquisition path. Explicit opaque Logix structures
use the bounded RAW option below. SLC/MicroLogix PCCC files use the
separate adapter below. An explicit external helper remains available by configuring
`executable` and `args` instead of `address` and `signals`.

### Rockwell Logix DWORDs and packed BOOL arrays

Select `data_type: "logix_dword"` for exact unsigned 32-bit scalar/array values,
or `"logix_bool_array"` for the original DWORDs, bytes and all their bits:

```json
{
  "version": 1,
  "sources": [{
    "id": "press", "protocol": "ethernet_ip",
    "address": "192.0.2.40:44818", "slot": 0,
    "every_ms": 100, "timeout_ms": 1000,
    "signals": [
      {"name": "words", "tag": "Flags[3]", "data_type": "logix_dword", "elements": 2},
      {"name": "flags", "tag": "Flags[3]", "data_type": "logix_bool_array", "elements": 2}
    ]
  }]
}
```

For these modes, `elements` is a DWORD count from 1 through 256, default 1.
In a one-dimensional Logix BOOL array, `Flags[3]` selects the fourth DWORD,
starting at source bit 96; two DWORDs cover bits 96–159. Indices and counts are
not individual BOOL positions. The two signals above perform separate reads
and need not agree in a changing controller.

`logix_dword` returns a JSON integer for one element, otherwise an integer array.
The packed mode returns `kind: "logix_bool_array"`, `type_descriptor_raw: [211,0]`,
`dword_count`, `bit_count`, exact `words`, a dense boolean `bits` array, and the
original base64 `raw` bytes. `bit_order: "lsb0"` means bit `32*w+b` is bit `b`
of returned DWORD `w`. `bit_index_base: "requested_range"` makes output indices
relative to this read. The project supplies the BOOL-array interpretation;
Capturefab does not discover declarations or decode packed UDT members.

Both modes require the Logix profile and exact `D3 00` type descriptors on every
fragment. Fragment offsets count value bytes; final extent must match the
requested DWORD count. Default atomic decoding and other profiles keep their
existing admission rules. `capacity`, `encoding` and `max_bytes` are invalid
for these modes. One read is bounded to 1024 raw bytes or 8192 bits, with the
shared 1 MiB serialized poll budget checked before another query and before
large output arrays are made.
Every continuation shares the original deadline. A malformed, late or
over-budget sibling discards the complete current poll; previous successful
polls remain recoverable. These reads send no write service or per-bit query,
and establish no snapshot or controller timestamp.

[Rockwell Logix data access, 1756-PM020I](https://literature.rockwellautomation.com/idc/groups/literature/documents/pm/1756-pm020_-en-p.pdf) ·
[libplctag bit and BOOL indexing](https://github.com/libplctag/libplctag/wiki/Bits-and-Booleans)

### Rockwell Logix STRING members

For a ControlLogix or CompactLogix STRING, select `data_type: "logix_string"` and
set positive `capacity` to the declared DATA array capacity, such as 82 for the
built-in STRING or the project's custom string capacity 1–65535. The default
`data_type: "atomic"` retains numeric behavior. `capacity` and `encoding` apply
only to explicit string signals; select the matching profile and `data_type`.
Micro800 and Omron string formats are different.

```json
{
  "version": 1,
  "sources": [{
    "id": "packaging",
    "protocol": "ethernet_ip",
    "address": "192.0.2.42:44818",
    "slot": 0,
    "every_ms": 500,
    "timeout_ms": 2000,
    "signals": [
      {"name":"lot","tag":"LotNumber","data_type":"logix_string","capacity":82},
      {"name":"labels","tag":"Labels[0]","data_type":"logix_string","capacity":82,"elements":2,"encoding":"ascii"}
    ]
  }]
}
```

Capturefab reads `.LEN` as DINT, all configured `.DATA[0]` bytes as SINT, and
`.LEN` again. Full DATA reads may be fragmented. Configure a readable bound:
an overlarge capacity can fail, and readability does not establish the exact
underlying declaration or a matching whole-structure layout. Negative,
over-bound or changed LEN, wrong member types/lengths, and failed fragments
reject the complete poll. All member reads, array elements and siblings share
the original poll deadline; failed polls publish no partial samples.

The default value is an object preserving every active byte, including NUL and
non-UTF8 data. For LEN3 containing `A`, NUL, `B`, it is:

```json
{"kind":"logix_string","length":3,"configured_capacity":82,"raw":{"encoding":"base64","scope":"active_data","data":"QQBC"}}
```

Add explicit `encoding: "ascii"` or `"utf8"` for strict decoded `text` and an
`encoding` field. Invalid text fails the poll; no replacement characters or
implicit code page are used. `length` counts active DATA bytes. Unused slot
tails are excluded; no terminator, structure handle, padding or stride is
assumed. Source data is not modified.

For an array, set `elements` 1–256 and an explicit last index, for example
`Labels[0]`. Each element remains a separate object in a JSON array; one
produces an object. Nested/member paths and program scope keep their existing
syntax. The last explicit array index advances; no multidimensional carry is
inferred. Serialized output is capped at 1 MiB incrementally before another
unaffordable element/sibling can be acquired.

These sequential member reads are best-effort observations. A matching LEN
reread detects some changes but cannot detect same-length DATA changes or LEN
ABA; it does not prove a coherent snapshot or freshness. The client requires
external read access. Generic whole UDTs and CIP Security remain outside this option.

[Rockwell ASCII strings](https://literature.rockwellautomation.com/idc/groups/literature/documents/pm/1756-pm013_-en-p.pdf),
[design considerations](https://literature.rockwellautomation.com/idc/groups/literature/documents/rm/1756-rm094_-en-p.pdf),
and [data access](https://literature.rockwellautomation.com/idc/groups/literature/documents/pm/1756-pm020_-en-p.pdf).

### Opaque Logix whole-structure RAW
Explicit `data_type: "logix_struct_raw"` records a whole structure's returned bytes with its exact four-byte tag-type descriptor. It requires the Logix profile and `max_bytes` from 1 through 1048576. This is a transfer ceiling, not an expected object size. The usable raw payload can be lower because every source poll has a shared 1 MiB serialized JSON limit including base64, metadata and sibling values. Numeric reads remain the default; there is no automatic raw fallback. `capacity` and `encoding` are invalid for this option; `max_bytes` is invalid for other data types.

```json
{
  "version": 1,
  "sources": [{
    "id": "controller",
    "protocol": "ethernet_ip",
    "profile": "logix",
    "address": "192.0.2.42:44818",
    "slot": 0,
    "timeout_ms": 2000,
    "signals": [
      {"name":"machine","tag":"MachineSummary","data_type":"logix_struct_raw","max_bytes":65536},
      {"name":"records","tag":"Records[3]","data_type":"logix_struct_raw","elements":2,"max_bytes":131072}
    ]
  }]
}
```

The value is one object, including when multiple elements were requested:

```json
{"kind":"logix_struct_raw","layout":"opaque","elements_requested":2,"tag_type_hex":"a002cd9e","structure_handle":40653,"bytes":64,"configured_max_bytes":131072,"raw":{"encoding":"base64","scope":"structure_data","data":"..."}}
```

`tag_type_hex` preserves all four descriptor bytes. `structure_handle` is the opaque little-endian 16-bit handle; it is not a unique type identifier and equal handles do not prove equal or stable layouts. `raw.data` preserves every returned structure-data byte, including padding, unused storage, NULs and arbitrary bit patterns, without the descriptor repeated in each reply. No structure name, template identity, member type, string encoding, units, byte order or element stride is inferred. Arrays remain one contiguous opaque blob: `elements_requested` describes the request and does not independently prove a complete set of UDT instances or their boundaries.

Existing symbolic/member/program and indexed-address rules apply. `elements` defaults to 1 and accepts 1–256; configure the intended readable array range and all dimensions where required by the controller. One backplane `slot` is optional; omit it for direct tag access. External read access must be allowed.

The client accepts exact `A0 02` plus a two-byte handle and requires all four bytes to repeat across fragments. Read Tag starts the transfer; Read Tag Fragmented retains the path/count and advances by structure-data bytes only. Each accepted reply, including the final successful reply, must contain positive data. Partial-at-ceiling, excess bytes, a changed/malformed descriptor, bad status or a failed/expired fragment rejects the complete poll before another query. Exact serialized output is checked before assembly growth or continuation. The original poll/acquisition deadline covers setup, all fragments and siblings; failed polls publish no samples. Existing source cleanup and correlation remain unchanged.

A successful final CIP reply ends the observed byte transfer. Without template discovery, the client cannot validate the intended layout length, distinguish a falsely short successful reply, decode the object, prove snapshot consistency/freshness or detect same-handle layout changes. Whole predefined, module-defined and add-on-defined layouts are not qualified as understood formats. Use explicit decoded atomic members or Logix STRING members when their semantics are needed. CIP Security and template provisioning are outside this option. Capturefab issues no value/state writes.

[Rockwell Data Access, 1756-PM020I pp.12–13,18–21,58–60](https://literature.rockwellautomation.com/idc/groups/literature/documents/pm/1756-pm020_-en-p.pdf).

### Rockwell Micro800 global variables

Select `profile: "micro800"` for a direct EtherNet/IP connection to a
Micro820, Micro850 or Micro870 CPU with CIP Symbolic access. Configure the
CPU's built-in Ethernet port and use global variables accessible to external
clients. Omit `slot`. A program-local, system or reserved variable is
outside this profile's scope.

```json
{
  "version": 1,
  "sources": [{
    "id": "micro850",
    "protocol": "ethernet_ip",
    "profile": "micro800",
    "address": "192.0.2.42:44818",
    "every_ms": 200,
    "timeout_ms": 1200,
    "signals": [
      {"name": "cycles", "tag": "CycleCount"},
      {"name": "temperature", "tag": "Temperatures[0]", "elements": 4}
    ]
  }]
}
```

The adapter opens a 504-byte Class 3 explicit connection to the CPU's message
router. Reads use CIP Read Tag Fragmented, starting at byte offset zero.
Atomic numeric types, exact integers, element limits and batch publication
follow the rules above. Scalar and explicitly indexed BOOL values use one
byte. Whole structured and packed BOOL values are outside this profile.
`originator_vendor_id` is an optional 16-bit value for sites that configure
an assigned originator identity. The default is 0. Capturefab uses fresh
nonzero connection identifiers and originator serial values.

Registration, connection opening and all tag fragments share one poll
deadline. The adapter reuses the connection during active polling and opens
a fresh session after an idle interval based on the negotiated watchdog.
Normal teardown attempts a bounded Forward Close and unregisters the TCP
session. A failed exchange closes its connection and publishes no part of
the poll. The Class 3 connection carries explicit reads.
[Micro820 manual, chapter 5](https://literature.rockwellautomation.com/idc/groups/literature/documents/um/2080-um005_-en-e.pdf)
· [Micro850 and Micro870 manual, chapter 5](https://literature.rockwellautomation.com/idc/groups/literature/documents/um/2080-um002_-en-e.pdf)

### Micro800 counted STRING values

Use `data_type: "micro800_string"` with `profile: "micro800"` for a readable global STRING. Configure the actual declared character `capacity` from 1 through 252; it is required and bounds the returned length. The default `data_type: "atomic"` and all numeric behavior stay unchanged. The PLC variable must permit external read access. This is the published SHORT_STRING representation, not a Logix LEN/DATA structure or the controller's internal storage layout. The cited Micro800 HMI guide documents firmware 7 and later; qualification against physical hardware is still required for the selected CPU/firmware.

```json
{
  "version": 1,
  "sources": [{
    "id": "micro850",
    "protocol": "ethernet_ip",
    "profile": "micro800",
    "address": "192.0.2.42:44818",
    "every_ms": 500,
    "timeout_ms": 2000,
    "signals": [
      {"name":"lot","tag":"LotNumber","data_type":"micro800_string","capacity":80},
      {"name":"labels","tag":"Labels[0]","data_type":"micro800_string","capacity":252,"elements":2,"encoding":"ascii"}
    ]
  }]
}
```

Each value is an object containing the exact byte length, configured capacity and every active byte as base64. For three bytes A, NUL, B:

```json
{"kind":"micro800_string","length":3,"configured_capacity":80,"raw":{"encoding":"base64","scope":"active_data","data":"QQBC"}}
```

There is no default text encoding. Explicit `encoding: "ascii"` or `"utf8"` adds strict `text` and `encoding` fields; invalid bytes fail the entire poll. UTF-8 is an explicit interpretation of received bytes, not a claim that the controller implements Unicode string operations. NUL is preserved within the counted data, without truncation, replacement or inferred units. No terminator, maximum-capacity field, slot padding or implicit code page is accepted from the wire.

`elements` defaults to 1 and supports 1–256. Multiple elements require a fully indexed first element such as `Labels[0]`; the last explicit index advances without multidimensional carry, with each element queried separately using count 1. One element produces an object, multiple elements an array of separate objects. The project must declare every addressed index; the adapter cannot infer array dimensions. Existing Micro800 symbolic-name and global-variable restrictions apply.

Read replies must carry exact type `DA 00`, one byte of length and exactly that many bytes. The type and total extent remain checked across positive-progress fragments; offsets include the count byte and exclude the repeated type descriptor. Registration, Class 3 setup, every element/fragment and all siblings share the original poll/acquisition deadline. Malformed data, a failed fragment, bad strict text, or an output over the incremental 1 MiB serialized poll budget publishes no samples from that poll. These sequential reads do not freeze the PLC or establish a cross-element snapshot/freshness guarantee. Existing Class 3 identity, watchdog renewal and cleanup remain unchanged. The client does not write PLC variables.

Primary references: [Micro800 symbolic types and paths, 2080-RM001M p.174](https://literature.rockwellautomation.com/idc/groups/literature/documents/rm/2080-rm001_-en-e.pdf), [SHORT_STRING byte format, 2080-QS002B pp.29,44](https://literature.rockwellautomation.com/idc/groups/literature/documents/qs/2080-qs002_-en-e.pdf), [array support, 2080-QS003A pp.59–60](https://literature.rockwellautomation.com/idc/groups/literature/documents/qs/2080-qs003_-en-e.pdf), [declared STRING capacity/bytes](https://www.rockwellautomation.com/en-au/docs/factorytalk-design-workbench/1-00-00/ftdw-help-ditamap/micro800-controller/program-elements/data-types-overview/iec-61131-3-data-types/string-data-type.html), and [FactoryTalk Linx supported types](https://www.rockwellautomation.com/en-ie/docs/factorytalk-services-platform/6-60/factorytalk-services-platform-help-ditamap/factorytalk-linx-help/work-with-micro800/supported-data-types-m800.html).

### Omron NJ/NX published variables

Use `profile: "omron_nj_nx"` to read published global variables on the built-in
EtherNet/IP port of a Sysmac NJ or NX CPU, including NJ101/301/501 and
NX1P2/102/502/701. Set the variable's Network Publish setting in Sysmac Studio,
for example to Publish Only. Use a direct CPU address and omit `slot`.

```json
{
  "version": 1,
  "sources": [{
    "id": "press",
    "protocol": "ethernet_ip",
    "profile": "omron_nj_nx",
    "address": "192.0.2.42:44818",
    "signals": [
      {"name":"ready","tag":"MachineReady"},
      {"name":"pressure","tag":"Pressure[0]","elements":4}
    ]
  }]
}
```

Use the project's exact ASCII variable names, member paths and array indices.
Numeric ranges require an explicit first index. The profile decodes CIP integer
and floating-point atomics and IEC BYTE/WORD/DWORD/LWORD values. Use a scalar
variable or a fully indexed element for an individual BOOL. A BOOL reading is
an object with separate `value` and `forced` booleans. Multi-element BOOL arrays,
strings and whole structures are outside this profile. The direct CIP service
has a 502-byte request and reply limit. Split larger arrays into smaller indexed
ranges. See [Omron's W506 Ethernet manual](https://assets.omron.eu/downloads/latest/manual/en/w506_nj_nx-series_cpu_unit_built-in_ethernet_ip_port_users_manual_en.pdf).

## EtherNet/IP Assembly data

Use `ethernet_ip_assembly` when a device exposes a documented Assembly instance
through unconnected `Get_Attribute_Single`. Capturefab reads the Data attribute
of Assembly class `0x04`, attribute `0x03`, once per poll. Supply the device's
expected response length and the byte offsets from its map. Use `ethernet_ip`
for Logix and Micro800 symbolic tags or published Omron NJ/NX variables.

The Keyence DL-EP1 IL Monitor Data example reads the current value and its
three validity flags for head ID 01, plus the complete 168-byte snapshot.

```json
{
  "version": 1,
  "sources": [{
    "id": "laser",
    "protocol": "ethernet_ip_assembly",
    "address": "192.0.2.40:44818",
    "instance": 100,
    "data_length": 168,
    "every_ms": 100,
    "raw_signal": "monitor_bytes",
    "signals": [
      {"name": "pv_raw", "offset": 48, "data_type": "i32"},
      {"name": "over_range", "offset": 6, "data_type": "bool", "bit": 0},
      {"name": "under_range", "offset": 8, "data_type": "bool", "bit": 0},
      {"name": "invalid", "offset": 10, "data_type": "bool", "bit": 0}
    ]
  }]
}
```

`address` is a numeric TCP endpoint, usually port 44818. `instance` is 1–65535
and `data_length` is 1–4096 bytes. Configure 1–256 unique output names across
`signals` and optional `raw_signal`. The raw reading contains the full Data
attribute as `{ "encoding": "base64", "data": "..." }`.

Mapped offsets count from byte zero. Types are `bool`, `i8`, `u8`, `i16`,
`u16`, `i32`, `u32`, `i64`, `u64`, `f32` and `f64`. Numeric fields use
`byte_order: "little"` by default and accept `"big"`. Booleans select `bit`
0–7, with 0 the default. Numeric scaling uses `raw * scale + offset_value`,
with defaults of 1 and 0. Booleans and 64-bit integers do not allow scaling.
All fields must fit the configured response. The full poll shares one
`timeout_ms` deadline and malformed, partial or incorrectly sized replies
fail before publishing samples.

The Keyence IL manual documents that the device can retain the previous P.V.
when a range or invalid flag is set. Capture those flags alongside the value.
Head and display settings determine engineering units. Other devices use their
own instance and field layouts. The adapter uses explicit reads over TCP.
[Keyence DL-EP1 specification](https://www.keyence.com/products/sensor/network-communication/dl/models/dl-ep1/)
· [Keyence IL DL-EP1 manual](https://forge.codesys.com/forge/talk/Deutsch/thread/dda5c0abc3/54bc/attachment/AS_16491_DL-EP1_UM_96132E_WW_GB_2045_7.pdf)

## Rockwell SLC/MicroLogix PCCC files

The native `pccc` adapter reads SLC/MicroLogix typed logical files through
EtherNet/IP `Execute_PCCC`. Use it when the particular CPU and Ethernet interface
provide this service; support depends on their firmware and configured file map.
The wire path follows [Rockwell's PCCC typed logical read command
0F/A2](https://literature.rockwellautomation.com/idc/groups/literature/documents/rm/1770-rm516_-en-p.pdf)
and the [documented Execute PCCC service and requestor
identity](https://literature.rockwellautomation.com/idc/groups/literature/documents/um/22comm-um004_-en-p.pdf).
It sends read requests and session metadata, with no PLC value or operating-mode
writes.

```json
{
  "version": 1,
  "sources": [{
    "id": "legacy_press",
    "protocol": "pccc",
    "address": "192.0.2.42:44818",
    "every_ms": 100,
    "timeout_ms": 1000,
    "max_read_bytes": 220,
    "signals": [
      {"name": "cycles", "address": "N7:0", "data_type": "u16"},
      {"name": "ready", "address": "B3:0/5"},
      {"name": "temperature_c", "address": "F8:0"},
      {"name": "counts", "address": "N7:10", "elements": 16}
    ]
  }]
}
```

`address` is the numeric IP and TCP port, normally 44818. Omit `slot` for a direct
endpoint, or set it for one port-1 backplane hop to the processor. Arbitrary
multi-hop routes and DH+ bridge commands are not implemented. A session is
registered lazily and reused across polls; acquisition cleanup closes it.

Each source accepts 1–256 uniquely named `signals`. A signal's `address` uses
decimal file and element numbers, for example `N7:0`, `B3:0` or `F8:0`. For numeric
N/B/F/L files, the parser accepts file/element numbers 0–65535, including extended
logical addressing; the actual file and element must exist on the controller.
Whole T/C and ST elements have the narrower ranges below. Supported numeric file types are:

| File | Default output type | Explicit `data_type` options |
| --- | --- | --- |
| `N` integer | Signed 16-bit integer | `i16`, `u16`, `bool` |
| `B` binary | Unsigned 16-bit word | `i16`, `u16`, `bool` |
| `F` floating point | 32-bit floating point | `f32` |
| `L` long integer | Signed 32-bit integer | `i32`, `u32`, `bool` |

`L` files are model-dependent MicroLogix functionality, documented as type `0x91`
in [Rockwell's data-access guide](https://literature.rockwellautomation.com/idc/groups/literature/documents/pm/1756-pm020_-en-p.pdf).
Configure them only where the CPU supports Long files; they do not extend the
generic SLC N/B/F scope. Signed/unsigned choices reinterpret the native word's
bits. A `/bit` suffix selects one boolean, with bits 0–15 for N/B words or 0–31
for L values. The suffix implies `bool`; an explicit `bool` without a suffix
selects bit 0. Boolean selections are scalar and cannot be scaled.

For numeric files, `elements` defaults to 1 and accepts 1–4096 contiguous elements,
with the final element at most 65535. One element produces a scalar; multiple elements produce
a JSON array. Numeric `scale` and `offset` apply `value * scale + offset`, with
defaults 1 and 0. Unscaled integers retain their exact signed/unsigned values.
Floating-point values and scaling must remain finite. Whole ST strings use the
separate native format below. Status files and other undocumented file formats
are not decoded.

`max_read_bytes` defaults to 220 and accepts 4–224 bytes. Set a smaller value when
the device's supported transfer size requires it. Arrays are split into whole
native elements within this bound. All requests in a poll, including connection,
session registration, routing and array segments, share one `timeout_ms` deadline.
Session, transaction, requestor identity, CIP/PCCC status and exact response sizes
are checked before a complete poll is published. The serialized poll is limited
to 1 MiB. A failure in any signal or segment publishes none of that poll's values.
Signals and segments are read sequentially, so the PLC can change values between
requests; atomic publication does not provide a controller-wide snapshot.
A failed poll permanently retires the `pccc` TCP source, for numeric and structured
signals alike: it does not reconnect or retry an application read. Earlier completed
polls remain in the recording.

### Whole SLC/MicroLogix timers and counters

The SLC/MicroLogix `pccc` profile, `pccc_csp` and `df1` also accept whole timer or
counter addresses: `T4:0`, `C5:7`, or matching T/C user files 9–255. Elements are
0–255; `elements` defaults to 1, accepts 1–256, and the final requested element
must remain at most 255. The file and range must exist on the configured CPU.
Optional `data_type: "timer"` or `"counter"` must match the address; numeric type
overrides, `/bit` and member suffixes are refused. Only identity `scale: 1` and
`offset: 0` are allowed. This does not extend the `plc5` profile beyond N/B/F.

```json
{
  "version": 1,
  "sources": [{
    "id": "native_tc", "protocol": "pccc", "address": "192.0.2.42:44818",
    "every_ms": 100, "timeout_ms": 1000, "max_read_bytes": 220,
    "signals": [
      {"name": "cycle", "address": "T4:0", "data_type": "timer"},
      {"name": "counts", "address": "C5:7", "elements": 2}
    ]
  }]
}
```

Each native element is six little-endian bytes: control, PRE, ACC. One element
returns an object; multiple elements return an ordered object array. For example:

```json
{"kind":"pccc_timer","control_word_raw":49157,"preset":100,"accumulator":17,"native_words_raw":[49157,100,17],"flags":{"en":true,"tt":true,"dn":false}}
```

`preset` and `accumulator` preserve signed 16-bit native words, including negative
timer application states. The three unsigned `native_words_raw` preserve all six
bytes and unknown/internal control bits. Timer flags are `en`, `tt`, `dn`; counter
flags are `cu`, `cd`, `dn`, `ov`, `un`. Bit 10 is not universally UA and is not
assigned that meaning. No timebase, engineering units, `source_time`, PLC
snapshot or reply-freshness guarantee is inferred.

T/C signals require `max_read_bytes` at least 6, within the existing 4–224 range.
Segments contain `floor(max_read_bytes / 6)` whole native elements, with addresses
advancing by that element count, not by three words. The default cap 220 therefore
fits at most 36 T/C elements per request. Actual controller transfer capacity is
still a device/firmware prerequisite. Siblings, segments, decoding and exact
serialized values share the original poll deadline and 1 MiB batch bound; a
failure publishes none of the current poll. Existing numeric ranges, values and
scaling remain as described above.

Primary: [Rockwell typed logical read, 1770-RM516 §7-17](https://literature.rockwellautomation.com/idc/groups/literature/documents/rm/1770-rm516_-en-p.pdf) ·
[SLC instruction reference, 1747-RM001G tables 1.4–1.9](https://literature.rockwellautomation.com/idc/groups/literature/documents/rm/1747-rm001_-en-p.pdf).
Protocol fixtures are specification-derived; physical CPU/interface qualification
and an unchanged-stock semantic decoder comparison remain pending.

### Whole SLC/MicroLogix ST strings

Use `ST9:0` or another whole `STf:e` address through the SLC `pccc` profile,
`pccc_csp` or `df1`. The configured address range is files 3–255, elements 0–255;
`elements` defaults to 1 and accepts 1–256 with the final element at most 255.
The configured CPU must support and allocate that file. Rockwell documents ST
for SLC 5/03 OS301 and later, SLC 5/04 and SLC 5/05; MicroLogix 1100/1400
also document this native layout. SLC ST files must be 9–255; MicroLogix permits
3–255. Select the file supported by the configured CPU; Capturefab does not infer
a model or remap the address. This does not qualify other CPU revisions or
extend the PLC-5 profile. [SLC ST files, 1747-RM001G pp. 10-3–10-4](https://literature.rockwellautomation.com/idc/groups/literature/documents/rm/1747-rm001_-en-p.pdf) ·
[MicroLogix 1400, 1766-RM001J table 141](https://literature.rockwellautomation.com/idc/groups/literature/documents/rm/1766-rm001_-en-p.pdf).

```json
{
  "version": 1,
  "sources": [{
    "id": "part_labels", "protocol": "pccc", "address": "192.0.2.42:44818",
    "every_ms": 500, "timeout_ms": 2000, "max_read_bytes": 220,
    "signals": [
      {"name": "native_label", "address": "ST9:0"},
      {"name": "text_labels", "address": "ST9:10", "elements": 3,
       "data_type": "string", "encoding": "ascii"}
    ]
  }]
}
```

The case-insensitive address implies `data_type: "string"`; an explicit type
must match. Only identity `scale: 1` and `offset: 0` are allowed. Bit and member
suffixes such as `/0`, `.LEN` and `.DATA[0]` are refused. Optional `encoding`
is `ascii` or `utf8`, applies only to ST signals, and requests strict text
decoding. Omit it to capture arbitrary character bytes without guessing a code
page. Embedded NUL bytes remain active data; no terminator or padding is inferred.

Each native element is 84 bytes: a little-endian unsigned length word followed
by 41 character words. The earlier character occupies each word's upper byte,
so the wire character pairs are swapped before selecting the active length.
All 84 original bytes, including the unused tail and an odd character's inactive
pair mate, remain recoverable. The output has `kind: "pccc_string"`,
`length_word_raw`, `capacity: 82`, `length_status`, and
`raw: {"encoding":"base64","scope":"native_element","data":"..."}`.
One element produces an object; multiple elements produce an ordered object array.

For a length of 0–82, `length_status` is `valid` and `active` contains
`{"encoding":"base64","scope":"active_data","data":"..."}` in character
order. Explicit text adds `encoding` and `text`. Invalid ASCII/UTF-8 fails the
entire poll without substituting characters. A native length of 83–65535 instead
produces `length_status: "invalid_length"` with the original raw element and
length word, omitting `active`, `encoding` and `text`. It is an observed
application-memory state; Capturefab neither clamps the length nor infers a CPU
error bit. Short/extra reply bytes and framing/status errors still fail the poll.

ST signals require `max_read_bytes` at least 84 within the existing 4–224 range.
Caps 84–167 read one whole element per request; 168–224 read at most two.
The default 220 therefore reads a three-element range in two requests, advancing
by two native elements for the second request. The adapter does not split a
native string to fit a smaller interface. A 256-element range needs 128 requests
at the default cap; configure a total deadline that accommodates the transport.
All siblings, segments, decoding and serialized values share the original poll
deadline and 1 MiB batch bound. Any failure publishes none of that poll; earlier
completed polls remain. No controller snapshot, freshness, text encoding or
source timestamp is inferred.

The PCCC requestor identity uses optional `vendor_id` (16-bit, default 0) and
`serial_number` (32-bit, nonzero when supplied). An omitted serial number is
randomized to a nonzero value for the capture source. Vendor 0 is an unspecified
software requestor, with no universal acceptance guarantee; configure the
requestor identity required by the controller or gateway. These fields identify
the requestor and do not provide authentication or encryption. The transport is
plain EtherNet/IP TCP; CIP Security is not implemented.

Use `pccc_csp` for legacy CSP Ethernet on a direct SLC 5/05. DH+ routing
and connected/cyclic I/O require a separate acquisition path. Protocol fixtures cover the implemented
direct/routed reads and malformed replies; the optional
`scripts/test-pccc.py` also cross-checks an independent SLC/MicroLogix reference
simulator and client. These checks do not qualify physical CPU/module models.

## Rockwell SLC 5/05 legacy CSPv4

Use `pccc_csp` for direct Channel 1 on an SLC 5/05 (1747-L551, L552 or L553)
with legacy AB/Ethernet CSPv4. This includes SLC 5/05 units from before
EtherNet/IP support was introduced in OS501 Series A FRN5. The controller
must expose the configured CSP service. The adapter uses SLC protected typed
logical reads of N/B/F files and the whole T/C and ST elements described above.
[Rockwell SLC 5/05 quick start](https://literature.rockwellautomation.com/idc/groups/literature/documents/qs/1747-qs001_-en-p.pdf)
[Rockwell explicit messaging guide](https://scadahacker.com/library/Documents/ICS_Protocols/Rockwell%20-%20Communicating%20with%20RA%20Products%20Using%20EtherNetIP%20Explicit%20Messaging.pdf)
[Direct SLC CSP model coverage in Ipesoft D2000](https://d2doc.atlassian.net/wiki/spaces/D2DOCV25EN/pages/26348147)

```json
{
  "version": 1,
  "sources": [{
    "id": "old_slc",
    "protocol": "pccc_csp",
    "address": "192.0.2.55:2222",
    "every_ms": 500,
    "timeout_ms": 1000,
    "signals": [
      {"name": "words", "address": "N7:0", "elements": 3},
      {"name": "running", "address": "B3:0/5"},
      {"name": "pressure", "address": "F8:0"}
    ]
  }]
}
```

Supply the numeric endpoint and TCP port, usually 2222. The signal map uses
the same SLC typed address, scalar bit and scaling settings as `pccc` above.
Numeric files are 3–255 and each requested range must fit elements 0–255. Whole
T/C files use T4/C5 or user files 9–255 and the same element limit. ST files use
the SLC range 9–255 and the same element limit, with the native string restrictions above. Use the
controller project's actual file types and extents. `max_read_bytes` is 4–224
(default 220). The direct profile uses local LSAP with zero node bytes and
optional `lsap_control` 0 (default) or 5. It supports one local target with
one request in flight. [Rockwell SLC addressing](https://literature.rockwellautomation.com/idc/groups/literature/documents/rm/5000-rm005_-en-p.pdf)
[Reference CSP transport](https://github.com/kumajaya/PCCCComm/blob/223cfa29cc969a55efaeb85289fc84dc156fa25b/src/PCCCComm/Core/CSPTransport.cs)

Connection, registration and every read segment share one poll deadline.
Capturefab validates the registration echo, session, transaction and exact
response width before publishing the poll. It opens a fresh session before
a transaction number can repeat. Any failed poll permanently retires the TCP
source, including numeric polls, without application reconnect/retry; earlier
completed polls remain. The fixture checks the published CSP
registration probe and a source-derived read exchange, plus invalid replies
and timeout. [Redpoint registration](https://github.com/digitalbond/Redpoint/blob/23ef36bf8416b3e71efb7abbcbd6e5717ad9471f/cspv4-info.nse#L55-L85)

## Rockwell PLC-5/E PCCC files

Use `pccc` with `profile: "plc5"` for direct EtherNet/IP reads of N/B/F
files on a PLC-5/20E, PLC-5/40E or PLC-5/80E with EtherNet/IP messaging
enabled. Rockwell lists Series C N.1, Series D E.1 and Series E D.1 as the
first PLC-5/E firmware revisions with EtherNet/IP support. Its later
installation instructions list the three Series F controllers.
[Rockwell Explicit Messaging Rev. 1.2, §4 and packets 5–6](https://scadahacker.com/library/Documents/ICS_Protocols/Rockwell%20-%20Communicating%20with%20RA%20Products%20Using%20EtherNetIP%20Explicit%20Messaging.pdf#page=6)
[Rockwell 1785-IN063B](https://literature.rockwellautomation.com/idc/groups/literature/documents/in/1785-in063_-en-p.pdf)

```json
{
  "version": 1,
  "sources": [{
    "id": "legacy_press",
    "protocol": "pccc",
    "profile": "plc5",
    "address": "192.0.2.44:44818",
    "every_ms": 500,
    "timeout_ms": 1000,
    "signals": [
      {"name": "words", "address": "N7:0", "elements": 10},
      {"name": "active", "address": "B3:1/5"},
      {"name": "pressure", "address": "F8:0"}
    ]
  }]
}
```

`profile` defaults to `slc`, the SLC/MicroLogix path above. The `plc5`
profile uses direct Execute_PCCC with typed read 0F/68 and
logical ASCII addresses. It supports N/B words, scalar bit selection
within a word and 32-bit F values. Files are 3–999 and requested ranges
must fit elements 0–1999. Older PLC-5 revisions allow only 0–999.
The actual file extent comes from the controller project. The requestor
identity, per-request byte cap and whole-poll deadline above still apply.
Omit `slot` for this direct profile.
[Rockwell 1770-RM516 §§7-28, 7-36–37, 13-15](https://literature.rockwellautomation.com/idc/groups/literature/documents/rm/1770-rm516_-en-p.pdf#page=115)
[Rockwell 1785-UM012D, pp. 4-12–4-14](https://literature.rockwellautomation.com/idc/groups/literature/documents/um/1785-um012_-en-p.pdf)

The adapter validates each reply's type and array lengths before publishing a
complete poll. Each segment is a separate PLC request. The fixture replays
Rockwell's published ten-word PLC-5/E response and exercises independent
scalar and segmented array replies.

## Rockwell DF1 serial reads

Use `df1` on an exclusive RS-232 link to an SLC 5/03, 5/04 or 5/05
Channel 0, or a MicroLogix 1100 or 1400 serial port configured for DF1 Full
Duplex and No Handshaking. Select the direct port configuration without
channel passthru. The adapter uses the [same protected typed logical read
0F/A2](https://literature.rockwellautomation.com/idc/groups/literature/documents/rm/1770-rm516_-en-p.pdf#page=104)
and [signal settings](#rockwell-slcmicrologix-pccc-files) as `pccc`.

```json
{
  "version": 1,
  "sources": [{
    "id": "serial_plc",
    "protocol": "df1",
    "device": "/dev/ttyUSB0",
    "source_node": 0,
    "destination_node": 1,
    "baud": 19200,
    "checksum": "crc",
    "every_ms": 500,
    "timeout_ms": 3000,
    "signals": [
      {"name": "parts", "address": "N7:0", "data_type": "u16"},
      {"name": "running", "address": "B3:0/5"},
      {"name": "values", "address": "F8:0", "elements": 4}
    ]
  }]
}
```

Set `device` to a Unix terminal or Windows COM port. Match the configured
`baud` (default 19200), `parity` (`none`, the default, or `even`),
`stop_bits` (1 or 2, default 1), and `checksum` (`crc` or `bcc`, default
`crc`). Data bytes use eight bits. Set `destination_node` explicitly.
`source_node` defaults to 0. Node values are 0–254. Rockwell documents a
default controller source ID of 9 for [SLC
5/03–5/05](https://literature.rockwellautomation.com/idc/groups/literature/documents/rm/1747-rm001_-en-p.pdf)
and 1 for [MicroLogix
1100](https://literature.rockwellautomation.com/idc/groups/literature/documents/um/1763-um001_-en-p.pdf)
and [MicroLogix
1400](https://literature.rockwellautomation.com/idc/groups/literature/documents/um/1766-um001_-en-p.pdf)
Channel 0. Reply addresses and the transaction number correlate the read.
Use the controller's configured file layout. Rockwell enables duplicate
packet detection in the documented defaults.

The adapter handles standalone and embedded acknowledgments, bounded
ENQ recovery and exact retransmission after NAK. `ack_timeout_ms` is
20–10000 (default 1000) for the link ACK wait. Give the source's total
`timeout_ms` enough room for all signal reads and any link recovery.
`max_read_bytes` is 4–224 (default 220), at least 6 for a whole T/C signal and
84 for a whole ST string. Larger arrays are read in segments under the same poll
deadline. Whole T/C and ST addresses and native objects follow the restrictions above; existing bounded
DF1 ACK/NAK/ENQ recovery remains unchanged. A failed or incomplete poll publishes
none of its signals. Sequential reads do not establish a controller snapshot.
This profile is for a dedicated point-to-point connection with one outstanding
read. Half-duplex, radio modem and PLC-driven application requests require
their own transport. PTY fixtures validate framing and read recovery.

## Native OPC-UA reads and subscriptions

An OPC-UA source with `endpoint` and `nodes` uses the client built into Capturefab.
It needs no separately installed interpreter, SDK or helper executable. Configure
the server and the Value nodes to read:

```json
{
  "version": 1,
  "sources": [{
    "id": "press",
    "protocol": "opc_ua",
    "endpoint": "opc.tcp://192.0.2.10:4840/",
    "sampling_ms": 100,
    "request_timeout_ms": 1000,
    "nodes": [
      {"name": "pressure_bar", "node_id": "ns=2;s=Pressure"},
      {"name": "part_present", "node_id": "ns=2;s=PartPresent"}
    ]
  }]
}
```

Use the server's NodeIds; namespace indexes and identifiers come from its address
space. There must be 1–256 nodes with unique names. Existing configurations should
remove the former `python` option; it is no longer accepted.

For a namespace whose numeric index can change after a server restart, supply its
exact URI alongside an identifier-only NodeId:

```json
{"name":"pressure_bar","namespace_uri":"urn:BeckhoffAutomation:Ua:PLC1","node_id":"s=MAIN.Pressure"}
```

The URI must match the server's NamespaceArray exactly, including case. It accepts
1–4096 UTF-8 bytes without whitespace or control characters; Capturefab does not
normalize or percent-decode it. With `namespace_uri`, `node_id` must begin with
`i=`, `s=`, `g=` or `b=` and cannot include a namespace/server prefix. Without that
field, existing numeric `ns=` NodeIds retain their meaning. Both forms may share
one source. The combined `nsu=` NodeId string form is not accepted.

Before the first application poll, the client reads namespace-zero Value node
`i=2255` once in that session and resolves every URI to its current index. This
shares the setup/request timeout and original recording cutoff. A fresh explicit
source session resolves again; the client does not automatically retry or
reconnect. No application values are published if any URI is absent or the table
is malformed. The table must be a Good, nonempty String array with at most 65536
unique, nonnull, nonempty bounded URIs, the standard OPC-UA URI at index zero,
and a checked encoded URI-string budget of 1 MiB. Existing wire-message limits
also apply. Values, signal names and source timestamps keep their usual output
shape; the table and temporary numeric mapping are not exported.
[Namespace indexes](https://reference.opcfoundation.org/specs/OPC-10000-3/8.2.2),
[Beckhoff namespace URI resolution](https://infosys.beckhoff.com/content/1033/tf6100_tc3_opcua_client/537637003.html).

`sampling_ms` defaults to 100 and accepts 1–3,600,000 milliseconds. It sets the
reader's poll cadence independently of `every_ms`. Missed polls do not accumulate.
`request_timeout_ms` defaults to 1,000 and accepts 1–30,000 milliseconds; it bounds
session setup and each Read operation. The source's ordinary `timeout_ms` bounds
how long one recorder read may wait for connection setup or a poll. A recording
that ends before the client has returned any successful values fails rather than
reporting a ready connection.

Each poll uses one OPC-UA Read request for the configured Value attributes. It
publishes the poll only when every node returned a Good status and a supported
value. A non-Good status stops recording with the node's zero-based index and
numeric OPC-UA status in the error details. The client does not write values,
invoke methods, browse or change server configuration.

JSON numbers, booleans, strings, null and one-dimensional arrays retain their
types, including exact signed and unsigned 64-bit integers. ByteStrings become
`{"encoding":"base64","data":"..."}`; null and empty ByteStrings remain distinct.
The native SDK normalizes null arrays to empty JSON arrays (`[]`).
DateTimes use UTC RFC 3339 strings, preserving submillisecond precision. GUIDs,
NodeIds and ExpandedNodeIds use `{"type":"guid|NodeId|ExpandedNodeId","value":"..."}`
with the corresponding type name. Matrices use
`{"type":"Matrix","element_type":"Int32","dimensions":[2,3],"values":[1,2,3,4,5,6]}`;
values retain OPC-UA's flat row-major order and explicit dimensions.

QualifiedName, LocalizedText, StatusCode, DataValue and DiagnosticInfo values use
`{"type":"...","fields":{...}}` with OPC-UA field names. Common extension
structures (Range, EUInformation, Argument, EnumValueType, complex numbers and
XVType) use the same form. Other extension structures retain their OPC-UA binary
body; undecoded binary, XML or JSON extension bodies retain their original bytes:
`{"type":"ExtensionObject","encoding":"binary|xml|json","encoding_type_id":"ns=2;i=1001","body":{"encoding":"base64","data":"..."}}`.
The `encoding` field contains one encoding name; a null extension has a null body.
Opaque bodies preserve their bytes without interpreting embedded values.

Unsupported values and non-finite JSON numbers fail explicitly. Nested values
are bounded to 32 levels and a poll's JSON output to 1 MiB; exceeding either bound
fails the poll. `source_time` comes from the server's SourceTimestamp when
available, separately from the recorder's host timeline. No timestamp is invented
when the server omits it.

### Queued OPC-UA subscriptions

Omitting `mode`, or using `"mode":"read"`, keeps the Read behavior and output
above. For queued data changes, explicitly select `"mode":"subscription"` and
provide the nested `subscription` object:

```json
{
  "id": "press",
  "protocol": "opc_ua",
  "endpoint": "opc.tcp://192.0.2.10:4840/",
  "mode": "subscription",
  "sampling_ms": 20,
  "request_timeout_ms": 1000,
  "timeout_ms": 2000,
  "subscription": {
    "signal": "subscription",
    "publishing_ms": 100,
    "queue_size": 128,
    "max_notifications_per_publish": 1024
  },
  "nodes": [{"name":"pressure_bar","node_id":"ns=2;s=Pressure"}]
}
```

The nested defaults match this example. `publishing_ms` accepts 1–3,600,000,
`queue_size` 2–4096 and `max_notifications_per_publish` 1–1024. The subscription
block is invalid in Read mode. The same node, namespace URI, credential and
security configuration applies to both modes. Setup and the first response share
one original source deadline; each background service operation also obeys
`request_timeout_ms`. A revised publishing/keepalive cadence that cannot fit that
request timeout with a 10 ms margin is refused with its reported numbers.
Choose a timeout compatible with the server's actual revisions.

Capturefab requests Reporting/Value items, both timestamps,
StatusValueTimestamp changes, no deadband and `discard_oldest:false`. It records
the requested settings and actual server revisions in the first envelope. A
revised queue below two, rejected node/filter or malformed result fails setup.
A queue does not establish a physical sample rate or prevent server-side loss.
Server sampling and the configured trigger determine which changes exist.

Each source reading contains one complete native `opcua_publish` envelope,
including ordered notification blocks, node/client handles, sequence number,
Publish or Republish delivery, native publish/header ticks, service status,
diagnostics and Publish acknowledgement results. Repeated changes for the same
node remain separate. Bad and Uncertain DataValue quality stays in the record;
it is not replaced with a Good value. Overflow retains the full envelope marked
`lossy:true`, then ends the source with an incomplete error.

Subscription DataValues use `kind:"opcua_data_value"` and
`representation:"async_opcua_0_19_decoded"`. They retain value/status presence,
raw status bits, both decoded timestamp ticks and available raw/effective
picosecond remainders. Values carry explicit native types. Float/Double scalars
retain IEEE bits, classification and sign; NaN and infinity remain representable.
Homogeneous float arrays use packed little-endian IEEE words in `data_base64`
with their native element type, length, encoding and dimensions. Other arrays
retain ordered typed values and explicit dimensions when present. ByteStrings,
opaque extensions, identifiers and nullable strings preserve their distinctions.
SDK decoding can normalize original wire distinctions, including null versus
empty arrays, orphan picoseconds and DateTime endpoint sentinels; this is a
native decoded representation, not an original packet archive. Unknown opaque
structures remain bounded encoded bodies without invented field semantics.

Sequence zero is invalid. Real sequences start at one and wrap from UInt32 max
to one; keepalives advertise the next real sequence and consume no number.
Capturefab repairs forward gaps of at most 64 messages with exact Republish
requests under the same operation deadline. Unavailable history, a larger gap,
backwards sequence or malformed repair fails the source. It does not reconnect,
recreate or silently move to a newer value. Unknown notification kinds, including
events outside this Value-only mode, fail explicitly.

Host observation clocks refer to decoded service-response completion. A repaired
message naturally arrives after a later Publish already held for ordering.
The repaired message keeps its own receive clock; release of the held message
uses an explicit `opcua_recovery_release` record at the recovery completion
frontier. Its original Publish and original receive clock remain nested unchanged.
This preserves export order without asserting a new device observation or
replacing original clocks. Device timestamps remain separate native fields.

The local FIFO holds at most eight envelopes and 4 MiB of counted JSON, plus one
bounded pending response. Per-envelope JSON stays below 1 MiB with metadata
headroom; JSON materialization is limited to 32 levels after SDK decoding.
The native binary decoder also bounds recursive DataValue and DiagnosticInfo entries before service dispatch, including direct response-header diagnostics. Its shared gauge counts active native decoders (default ten), independently of JSON nesting. Other existing Variant/ExtensionObject guards share that gauge. Full queues and crossed bounds
fail explicitly. A real message is eligible for acknowledgement only after
bounded local admission. These ACKs confirm local receipt, **not** a JSONL write,
fsync or crash-durable handoff. A crash between ACK and recording publication can
lose already-acknowledged data. If the SDK ends its event loop without retaining
a native failure status, the source remains unavailable and reports
`sdk_error_status_present:false` with its SDK completion value; it omits
`opcua_status` rather than inventing a Good error response. The server's own retransmission queue also has
finite retention. Acquisition cutoff preserves admitted earlier responses and
never presents a response received after cutoff as an earlier observation.

See the OPC Foundation's [subscription model](https://reference.opcfoundation.org/specs/OPC-10000-4/5.14.1),
[Publish](https://reference.opcfoundation.org/specs/OPC-10000-4/5.14.5),
[Republish](https://reference.opcfoundation.org/specs/OPC-10000-4/5.14.6) and
[monitored-item queue rules](https://reference.opcfoundation.org/specs/OPC-10000-4/5.13.1.5).

### Secure OPC-UA sessions

To authenticate with a username and password, configure environment variable
names and a SignAndEncrypt channel. Keep the credentials out of the endpoint URL
and the JSON file:

```json
{
  "id": "press",
  "protocol": "opc_ua",
  "endpoint": "opc.tcp://192.0.2.10:4840/",
  "username_env": "PLANT_OPCUA_USER",
  "password_env": "PLANT_OPCUA_PASSWORD",
  "security": {
    "policy": "basic256_sha256",
    "certificate": "certs/capturefab-client.der",
    "private_key": "certs/capturefab-client.pem",
    "server_certificate": "certs/press-server.der"
  },
  "nodes": [{"name": "pressure_bar", "node_id": "ns=2;s=Pressure"}]
}
```

Both credential environment variables must be set in the caller's environment.
The client reads them directly; they are not passed as command arguments.
For an encrypted private key, `security.private_key_password_env` names another
environment variable. File paths are relative to the command's working directory.

The security policy accepts `basic256_sha256`, `aes128_sha256_rsa_oaep` or
`aes256_sha256_rsa_pss`. Security always selects SignAndEncrypt and requires the
client certificate, private key and an explicitly pinned server certificate.
Provision and trust the client certificate on the OPC-UA server as required by
that server. The client's ApplicationUri comes from the first URI in the client
certificate's Subject Alternative Name extension, falling back to
`urn:capturefab:opcua` when no URI is present.
Anonymous sessions can also use this security configuration. Omitting
`security` selects an anonymous unencrypted session; username/password sign-in
requires security. A source with `executable` and `args` instead continues to use
the explicit external-helper contract.

For development, run `cargo test --lib timeseries::opcua` for configuration, value
conversion and key handling, and `cargo test --test data_opcua` for native
client/server fixtures. These exercise reads, source timestamps, bad statuses,
encrypted sessions and certificate pinning without an external runtime.

## Native passive EtherCAT and PROFINET RT capture

On Linux, these adapters observe the cyclic process data already exchanged by an
existing controller. They do not become an EtherCAT master or PROFINET controller,
configure devices, discover PDO/IO layouts, or transmit Ethernet frames. Connect
the capture interface to a network TAP or a switch mirror port that exposes the
required traffic. Capture on the controller host also works when its network
interface and driver make the frames visible to Linux packet sockets. Ordinary
connections to another switched port do not expose all device traffic.

Native capture uses Linux `AF_PACKET` and requires `CAP_NET_RAW` or root. On other
platforms the native adapter returns `unsupported`; an explicit `executable`/`args`
source can still use the external-helper contract. The configured interface name
must be local and at most 15 bytes, such as `eth1`.

Both adapters accept `direction: "incoming"` (the default), `"outgoing"` or
`"both"`. Incoming excludes frames sent by the local host. `promiscuous: true`
requests promiscuous membership for the capture socket; closing the socket removes
that membership. Promiscuous mode cannot make an ordinary switch port mirror other
ports. Selecting both directions can record the same bus transaction more than
once when the capture placement exposes both copies.

### EtherCAT logical process data

```json
{
  "version": 1,
  "sources": [{
    "id": "ethercat_inputs",
    "protocol": "ethercat",
    "interface": "eth1",
    "logical_address": 65536,
    "minimum_working_counter": 1,
    "promiscuous": true,
    "signals": [
      {"name": "temperature_c", "offset": 0, "data_type": "i16", "scale": 0.1},
      {"name": "part_present", "offset": 2, "data_type": "bool", "bit": 3}
    ]
  }]
}
```

Obtain `logical_address` and each signal's byte offset from the existing master's
PDO/FMMU mapping. Signal offsets are relative to `logical_address`. The decoder
observes completed logical-read (`LRD`) and logical-read-write (`LRW`) datagrams
carried directly in Ethernet type `0x88a4`; it accepts untagged frames and up to two
802.1Q/802.1ad VLAN tags. UDP-encapsulated EtherCAT and mailbox protocols are not
decoded. A signal must lie wholly inside one observed datagram; the adapter does
not assemble a value split across datagrams or cycles.

`minimum_working_counter` defaults to 1 and must be positive. This excludes the
usual outgoing read requests whose counter is still zero. Set it from the expected
working counter in the master's mapping when a stronger threshold is appropriate.
A nonzero counter alone does not prove that every expected slave responded. An
optional `source_mac`, such as `"02:00:00:00:00:01"`, filters the Ethernet provider.

### PROFINET cyclic RT data

```json
{
  "version": 1,
  "sources": [{
    "id": "profinet_inputs",
    "protocol": "profinet",
    "interface": "eth1",
    "source_mac": "02:00:00:00:00:10",
    "frame_id": 32768,
    "data_length": 40,
    "promiscuous": true,
    "signals": [
      {"name": "pressure_bar", "offset": 0, "data_type": "u16", "byte_order": "big", "scale": 0.01, "status_offset": 2}
    ]
  }]
}
```

Use the existing controller's communication-relation mapping to supply the exact
provider `source_mac`, cyclic `frame_id`, and byte layout. Frame IDs must be in
`0x8000`–`0xfbff` (32768–64511). The adapter reads cyclic RT Ethernet type `0x8892`,
with or without one or two VLAN tags. It does not establish application relations,
parse GSDML automatically, or decode IRT dynamic frame packing, DCP or acyclic
records.

`data_length` is the complete RT data area between the two-byte FrameID and the
four-byte APDU status. It must include the engineered IOPS/IOCS bytes and any
padding before that status; valid lengths are 40–1440 bytes. Signal offsets start
immediately after FrameID. Supplying the application's value-byte count instead of
the complete RT area would locate the status incorrectly.

The decoder requires the global DataStatus `DataValid` bit and a successful zero
TransferStatus. A signal may also provide `status_offset`, the byte offset of its
IOPS status in that RT data area; the IOPS good-data bit must be set. Use the actual
submodule mapping to select this offset. The global valid-data flag alone does not
establish the quality of every submodule. Invalid matching frames or bad configured
IOPS statuses fail the recording explicitly.

### Process-data signal options and limits

Each native fieldbus source accepts 1–256 uniquely named signals. `offset` selects
the first byte and `data_type` accepts `bool`, `u8`, `i8`, `u16`, `i16`, `u32`, `i32`,
`u64`, `i64`, `f32` or `f64`; the default type is `u16`. `byte_order` is `little` by
default and can be `big`. Set it explicitly for PROFINET values according to the
engineered data representation. Boolean signals use `bit` 0–7, defaulting to 0.

Numeric scaling is `decoded_value * scale + offset_value`, with defaults 1 and 0.
Boolean and 64-bit integer signals cannot be scaled; their original values,
including the full 64-bit integer range, are preserved. Non-finite floats or scaled
results are rejected. `status_offset` is only valid for PROFINET.

These are event streams, so `every_ms` does not throttle them. Host receive time is
recorded on the shared timeline; the adapters do not turn the EtherCAT distributed
clock or PROFINET cycle counter into UTC device timestamps. Every configured signal
must be observed before the source becomes ready. If the run ends before that,
recording fails with a timeout instead of claiming that an unmatched mapping was
complete. Malformed matching frames and kernel packet-queue loss also stop the run
explicitly. Raw traffic and process-data buffers remain bounded.
