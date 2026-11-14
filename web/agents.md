# Capturefab for AI agents

Use `capturefab` to find cameras, change settings, save images and record video from the shell.
Use `capturefab schema` to see what the installed version supports.
Run `capturefab schema COMMAND` for that command's arguments, result fields and errors.
The site also publishes a [command reference](reference.html).

## Install, upgrade or uninstall

Install the CLI for the current user without administrator rights.
The CLI starts at version 0.6.0. Earlier releases are the desktop app.

To delegate setup, copy this task into an agent:

```text
Install or upgrade Capturefab for my user account from https://capturefab.com.
Download and read install.sh (macOS/Linux) or install.ps1 (Windows).
Run the script with --upgrade or -Upgrade.
Use my custom install paths if I supplied them. Otherwise, use the defaults.
Make capturefab available in this terminal.
Run capturefab --version and capturefab doctor --json.
Report the installed version, executable path and setup issues.
Read this agent guide and capturefab schema before running camera commands.
```

Download a fresh script to install the latest release. Each script selects a specific version.
Close running Capturefab commands and sessions before removal.
Use the same custom paths that you used to install it.
A saved script can remove the installation offline. User data outside the release folders stays in place.

| Platform | Upgrade | Preview removal | Remove | Custom paths |
| --- | --- | --- | --- | --- |
| macOS and Linux | `sh install.sh --upgrade` | `sh install.sh --uninstall --dry-run` | `sh install.sh --uninstall` | `--prefix DIR`, `--bin-dir DIR` |
| Windows | `.\install.ps1 -Upgrade` | `.\install.ps1 -Uninstall -DryRun` | `.\install.ps1 -Uninstall` | `-Prefix DIR` |

Get the scripts and find platform setup steps on the [downloads page](downloads.html).

## Rules

1. **Pass `--json` for automation.** Most commands write one JSON line to stdout. Exit 0 gives
   a result. Errors have an `error` object with `code`, `message` and optional `hint` and `details`.
   Read diagnostics on stderr. `--help` and `--version` print text.
2. **Use the exit code for the category and `error.code` for the reason.**
   Exit codes are 0 for success, 1 for failure, 2 for invalid input, 3 for unavailable resources,
   4 for timeout, 5 for unsupported operations and 6 for full storage.
   Interruption codes are 130 for SIGINT, 143 for SIGTERM and 129 for SIGHUP.
3. **Use the installed schema to find flags.** For example, run `capturefab schema jobs wait`.
   Unknown flags give exit 2 and `usage`.
4. **Include units in durations.** Use values such as `250ms`, `1.5s`, `5m` or `1h30m`.
   A bare number other than `0` is invalid. Use `--timeout none` where the command supports an unlimited wait.
5. **Choose the camera explicitly.** Pass an ID from `list` with `-c`.
   Quote IDs and feature names with spaces, brackets or other shell characters.
   Examples include `'avfoundation:HD Pro Webcam C920'` and `'TriggerMode[FrameStart]'`.
6. **Choose a new output path.** `capture` and `record` protect existing files.
   Use `{seq}` in the output template or use `--on-exists number` with `capture`.
   Relative input and output paths use your working directory, even when a session runs the command.
   File paths in capture results are absolute.

`watch --json` writes one line per measurement. `shell --json` writes one line per command.
With `capture -o - --json`, stdout carries one image and stderr carries the JSON result or error.
With `--progress`, `capture`, `record` and `jobs wait` write progress JSON lines to stderr.

## Industrial time series

Run `capturefab data protocols --json` to see native adapters, external helper requirements,
and a simulator configuration in `example`. Save that object as `sources.json`, then run:

```sh
capturefab data record --config sources.json --for 5s -o run.jsonl --json
capturefab data export run.jsonl -o run.csv --json
capturefab data export run.jsonl -o press.jsonl --format jsonl --source press --json
```

The recording file holds a header, samples and an end row. Command stdout remains one result
or error document. Samples share host monotonic receive times; device times stay separate.
A source failure stops the run and reports the saved path and sample count in `error.details`.
Export rejects incomplete runs unless `--allow-incomplete` is explicit. Exports never overwrite.
Native adapters capture camera stills, Keyence KV Host Link, LS ELECTRIC XGT/XGB FEnet and XGB Cnet rSS scalars/explicit-address arrays, FATEK FBs standard Port0 discrete/native register reads, Yokogawa FA-M3 PC Link, Modbus TCP with gateway unit selection and optional TLS, Modbus RTU and ASCII, classic S7 absolute and indexed reads, Beckhoff ADS, Mitsubishi SLMP 3E binary or ASCII, MC 1E and serial FX Computer Link, Panasonic FP MEWTOCOL-COM, and Omron FINS TCP/UDP, serial Host Link (including explicit legacy CPM/SRM1 native areas) and CompoWay/F reads,
BACnet/IP meter and controller values, IEC 62056-21 and passive SML optical meter readouts, shared-bus SDI-12 environmental sensors through a USB serial bridge, Vaisala WXT530 serial weather measurements, HART process measurements through a managed Microflex modem, SICK TiM/LMS single scans over configured CoLa A/B, passive SICK multiScan100/picoScan100 Compact scan/IMU datagrams and explicit picoScan150 encoder/mixed UDP streams, LRS4000 Compact TCP scans and multiScan200 Compact TCP scans, ambient light and IMU, passive NMEA GNSS GGA/RMC positions, passive Velodyne VLP-16/HDL-32E raw UDP groups, passive Gocator GDP profile and surface archives, ifm IO-Link process input, TCP/serial JSONL streams (Unix terminals or Windows COM ports), and query-only SCPI instruments over raw TCP, native serial, VXI-11, HiSLIP or USBTMC (`usb` feature), METTLER TOLEDO MT-SICS weights over TCP or serial, A&D standard-format Q measurements over configured serial or transparent TCP bridges, Sartorius SBI16/22 manual-print measurements over configured no-flow interfaces, MQTT/TCP or verified TLS (QoS 0–2), and EtherNet/IP for Logix atomic tags, explicit DWORD/packed BOOL reads and STRING members, Micro800 numeric and counted STRING values and Omron NJ/NX variables and CIP Assembly data. SLC/MicroLogix files are available over EtherNet/IP or serial DF1. SLC 5/05 also supports legacy CSPv4. PLC-5/E files use direct EtherNet/IP. Keyence IV inspection results, Cognex In-Sight cells and complete Cognex DataMan ID Reader result attributes are also available. DataMan polling preserves raw bytes/codes and sends no trigger or result acknowledgement; reader buffering is configured externally. Linux can also passively receive CAN and CAN FD frames with explicit signal maps.
EtherCAT/Profinet RT process-data capture is passive and requires Linux with raw-packet permission
and the engineered signal layout. OPC-UA Read services and explicit `mode: "subscription"` are built into the CLI and support
anonymous or pinned SignAndEncrypt sessions. Subscription mode requires a `subscription` object,
retains queued native DataValues and sequence metadata, and repairs bounded gaps with Republish.
Its acknowledgements confirm local receipt; they do not prove JSONL persistence. Custom Unix
`helper` sources and Windows 10/Server 2016+ absolute `.exe` helpers and protocol-specific overrides remain available.
A `camera` source writes full-resolution images and puts file metadata on the same timeline as
sensor values. Set `part_format` to select a native GigE Vision or GenTL multipart still;
add `part_index` (0–31, requires `part_format`) for an exact zero-based part-table ordinal.
Same-selector sources can select different parts from one frame and keep its receive clock.
Its timestamp is frame receipt, before encoding/storage. `delivery_latency_ns`
records that delay. A `video` source records a local clip and a persistent per-frame index. It can share a
camera worker with a `camera` still source using the exact same selector; settings merge
before acquisition. Video samples map zero-based `frame_index` and `video_time_ns` to host
receive times. A final `recording` signal reports finalized video/index metadata. The command
drains encoded-frame observations before finishing its run file. See the guide for codecs,
paths, cutoff/finalization behavior and recovery.
Micro-Epsilon ILD2300 requires a preconfigured single-DIST1 RS422 stream and explicit profile. Its documented configured rates through 4000000 include factory 691200; rates above 115200 need a qualified host platform and supporting adapter/driver. Its passive adapter preserves raw 18-bit codes without converting units or asserting freshness; partial words at cutoff remain failures.

FATEK `fatek_fbs` requires profile `fbs_port0` and configured native RS232 7E1, or an explicit transparent TCP bridge. Only commands 44/46 read native bool/integer scalars and arrays, including W/DW combined views. Direct USB/VCOM is unqualified. A failed transaction retires the connection and publishes none of that poll; replies do not prove freshness or a PLC snapshot.

Mitsubishi `slmp` supports step relay `S` and retentive `STS`/`STC`/`STN` devices. Whole `LTN`/`LSTN` records require `device_format: "iq_r"` and explicit `data_type: "long_timer"`. Each logical timer retains all four raw words, the current-word pair and reported coil/contact bits; no current-word ordering or timebase is inferred. `elements` is 1–240 and sends four requested words per timer. Standard ASCII uses the documented SS/SC/SN aliases internally. The format selector does not prove CPU support; remote end codes remain visible. Any failed poll retires the source. See the [SLMP guide](guides/industrial-data.md#mitsubishi-melsec-mcslmp-reads).

Whole SLC/MicroLogix timer/counter elements use `T4:0`, `C5:0` or matching user files 9–255 through `pccc`, `pccc_csp` or `df1`. `elements` is 1–256 within element 0–255; `max_read_bytes` must be at least 6. Optional `data_type` is matching `timer`/`counter`; `/bit`, member suffixes, numeric overrides and nonidentity scaling are refused. Values retain the raw control/three words, signed PRE/ACC and portable flags, with no timebase/units/UA or snapshot/freshness inference. `plc5` remains N/B/F-only. A failed `pccc` or `pccc_csp` poll retires the TCP source instead of reconnecting/retrying application reads.

Whole ST strings use case-insensitive `ST9:0` or configured files 3–255/elements 0–255 through the same SLC `pccc`, `pccc_csp` and `df1` paths. Count 1–256 must end at 255 or earlier; `max_read_bytes` must be at least 84. Infer or supply matching `data_type: "string"`; refuse bit/member suffixes and nonidentity scaling. Each object preserves the complete 84-byte native element and unsigned `length_word_raw`; 0–82 exposes pair-swapped active bytes, while larger lengths produce `invalid_length` with no active/text fields. Optional ST-only `encoding: "ascii"`/`"utf8"` requests strict text; defaults preserve bytes without a code-page guess. Invalid text fails the whole poll. Embedded NUL and unused native bytes remain recoverable. Caps 84–167 read one element, 168–224 at most two, advancing by native elements under the same deadline/1 MiB bound. SLC ST files require 9–255, MicroLogix allows 3–255; CPU/file/interface support is required without model inference or address remapping. PLC-5 remains N/B/F-only.

ABB `abb_rws` requires explicit `api` (`rws1` or `rws2`), numeric `address`, environment credentials and 1–32 named `reads`. Resources are `controller_state`, `operation_mode`, `system_info`, `rapid_execution` and `rapid_symbol` with task/module/symbol identifiers. RWS1 uses Digest; RWS2 requires verified TLS and Basic. It sends only finite GETs and retains exact XHTML/state text; symbol properties and value have separate host observation windows. Requests are paced at least 50 ms apart under one poll deadline. A failed poll emits no partial batch and retires the source. Controller states and RAPID clocks do not establish physical readiness or UTC source time. See the [ABB guide](guides/industrial-data.md#abb-robot-web-services-controller-and-rapid-state).

Epson `epson_remote` requires an explicit `rc_plus_7_5`, `rc_plus_8_0` or `rc_plus_8_0_rc800` profile, numeric TCP endpoint, `password_env`, selected `terminator` and 1–32 named `reads`. Establish monitor-only mode externally while preserving the existing control-device owner. Private Login plus qualified GETs capture status, alarms, I/O, current robot and non-String variable text within documented profile limits. Raw 10/11 flag tokens remain exact; only 11 has named flag semantics. Replies retain complete framed bytes and separate host observation windows. Any failure retires the source; finish closes the socket without Logout or control commands. See the [Epson guide](guides/industrial-data.md#epson-remote-ethernet-monitoring).

Yaskawa `yaskawa_hses` requires profile `yrc1000_hw1483358_rev12`, enabled HSES at a numeric IPv4 UDP endpoint, explicit named `reads`, and operator-declared `reply_horizon_ms` from `timeout_ms` through 30000. Read resources are status, current `base_position` (group 1–8), `byte`, `integer`, `double_integer`, `real_raw` and `string32` (explicit u16 index). It sends read-only packets and retains exact responses/native values. Default ephemeral-port rotation and fixed-bind ID quarantine limit correlation to this source epoch and the declared delay horizon. R encoding, disputed status bits and unknown coordinate units remain unqualified; whole S32 storage bytes survive NULs. No controller setup, motion, source-clock or fresh/synchronized-state assertion follows. See the [Yaskawa guide](guides/industrial-data.md#yaskawa-high-speed-ethernet-server).

MTConnect `mtconnect` requires a numeric agent endpoint and optional safe `base_path`; it reads only probe/current/sample XML for namespace versions 1.2–1.8 and 2.0–2.7. Default sample mode starts with current state, then follows the agent header cursor under one poll deadline; `buffer_begin` or an epoch-bound resume object selects retained history. `count` is 1–1024, capped to the reported buffer size with `effective_count` retained. Whole native condition activations, structured deltas, exact text/attributes and original XML remain captured; there is no active-condition reconstruction, numeric rescaling or UTC source-clock inference. Buffer loss, epoch/model change or malformed data retires the source without automatic rebase. Optional auth is explicit; Basic/Bearer require verified TLS. See the [MTConnect guide](guides/industrial-data.md#mtconnect-machine-observations).

Livox `livox_udp` passively receives an externally enabled sampling stream. Choose an explicit SDK2 Mid360/S/L or HAP TX/T1 versus legacy Horizon/Avia profile and required `stream` (`points` or `imu`; T1 has no qualified IMU). Supply IPv4 `listen`/`source_ip`, optional `source_port`, literal local `output` and `max_bytes`; optional multicast requires an explicit group/interface and is excluded for Mid360L. Native dense returns, coordinates, tags, clocks and IEEE words remain exact in bounded immutable raw groups. SDK2 CRC covers timestamp plus payload and is mandatory; legacy packets have no sample CRC. No control or heartbeat commands are sent; source/model declarations do not authenticate a device or prove complete scans or synchronized time. See the [Livox guide](guides/industrial-data.md#livox-sdk2-and-legacy-udp-groups).

Universal Robots `ur_rtde` requires profile `rtde_v2`, a numeric enabled RTDE TCP endpoint and explicit ordered `fields` with exact `wire_type` values. It negotiates one output recipe and synchronizes output only (V/v/O/S/P); it never sends robot input or program commands. Requested `frequency_hz` is 1–500, default 125, without delivered-rate inference. All ten RTDE types retain exact native values and raw packets; floats retain IEEE words including NaN/Inf and negative zero. Robot timestamps remain fields, not UTC `source_time`. Queued samples retain original host receive clocks; failed exchanges retire the channel and preserve only the completed prefix.

`ni_daqmx` requires the `ni-daqmx` feature (included by default) and an installed NI driver on a supported Linux/Windows host. It creates one owned AI task with 1–64 individual physical channels. Omitted `timing` preserves continuous internal-clock input and bounded scan-major blocks. Explicit `timing:"on_demand"` requires integer `poll_interval_ms` 1..3600000 and must omit rate_hz/max_samples_per_channel/host_buffer_samples_per_channel, including nulls. It leaves fresh-task timing unmodified and requests one scan per read; host pre-call spacing is not an exact SDK-entry gap, hardware rate or fresh-conversion guarantee. Ready consumes no interval, slow reads cause no catchup burst, and repeats remain. On-demand rows retain host completion clocks and omit hardware-rate/buffer metadata; ADC/auto-zero stay untouched. Voltage configs remain the default. Explicit `measurement:"thermocouple"` uses per-channel type, units, range and built-in/constant CJC. Explicit `measurement:"current"` requires per-channel terminal, ampere range and built-in/external shunt selection; external `ohms` must be finite and positive. Explicit `measurement:"rtd"` requires per-channel type, temperature unit/range, 2/3/4-wire request, positive sensor `r0_ohms` at zero Celsius and positive `excitation.amperes` from an internal/external source. Internal excitation can energize the input sensor; external excitation is supplied by the operator. No wiring/type/current defaults or physical readbacks are inferred. RTD uses either timing mode, finite `temperature` and exact IEEE words without local resistance/unit conversion. Generic requests do not qualify all NI-9216/9217 modes or any chassis/OS/driver combination. Explicit `measurement:"mixed"` accepts 1–64 tagged per-channel voltage/thermocouple/current/RTD/strain/bridge_ratio/acceleration/resistance/thermistor_current/thermistor_voltage/force_bridge/pressure_bridge/torque_bridge objects, including same-family range/terminal overrides, in one owned task. Each entry requires its own `measurement`; existing homogeneous configs and rows remain unchanged. Mixed rows use `mixed_analog_block`/`driver_scaled_f64_mixed_analog` and `measurement:"mixed"`/`requested_not_read_back`. One `channel_measurements` vector binds zero-based `channel_index`, actual `physical`, `measurement`, quantity, units and family requests to scan-major values. Each word retains raw F64 plus finite `volts`/`amperes`/`temperature`/`strain`/`ratio`/`acceleration`/`ohms`/`force`/`pressure`/`torque`, with no repeated tags or global unit. Resolve the selected constructor union and only needed nondefault TC setters before CreateTask; no task split, fallback, per-channel timestamp, repeat suppression or simultaneous/fresh-conversion claim. Explicit `digital_port` accepts 1–64 whole-port strings with on-demand timing; actual `cfdpNNNN` read names and queried 1–32 line counts remain separate from requested physical ports. Preserve unsigned U32/LE words without masks or pin labels. `counter_count` and `counter_frequency` each require one typed counter object and explicit software timing. Count snapshots keep unsigned `count`/`u32_le_hex` without unwrapping, accumulation or hardware-register-width claims; frequency keeps raw F64/finite `frequency_hz` in Hz, without host-derived rates. Required constructor fields and optional non-null `input_terminal` remain requests, not measured routing or coerced windows. Frequency methods can reserve paired counters; input resource configuration is not passive monitoring. No hardware timing fallback, output route or device qualification is implied. Defaults remain unqueried. Exact scaled F64 bits, native statuses and worker read-completion clocks survive recording. Temperature/current rows use finite `temperature`/`amperes`, never `volts` or local unit/V-R conversion. Channel settings are requests, not physical readbacks; built-in resistance is not inferred. The nominal rate does not supply per-sample timestamps. It preserves valid partial reads before failure and does not reset devices or take over tasks. Hardware qualification is separate.

New AI measurements strain, bridge_ratio and acceleration also work in MixedAI with their own channel fields and units. Strain exposes seven bridge layouts and explicit gage/offset/ohms/Poisson requests; ratio supports volts_per_volt or millivolts_per_volt, and acceleration supports g/m_s2/in_s2 with separate sensitivity units. All three require explicit internal/external/none excitation amounts (volts or amperes); none is not a physical-disable guarantee. Optional ac/dc/ground coupling is request-only; ground measures ground, not sensor zero. Preserve raw IEEE plus finite strain/ratio/acceleration without local scaling. counter_period, counter_pulse_width, counter_angular_position and counter_linear_position are single-counter/on_demand-only F64 profiles with explicit time/edge/method or encoder PPR/distance/initial/Z requests. All Z fields are required even disabled; enabled Z can reset the measurement, and TwoPulse ignores phase. Omitted routes are unqueried and Z routes are rejected when disabled. Native time/position is never recomputed from host clocks or counts. Excitation may coerce/persist after Stop/Clear; no physical device, routing, power-off or OS/driver qualification is implied. See [NI input configuration](guides/industrial-data.md#ni-daqmx-analog-voltage-acquisition).

Additional `resistance`, `thermistor_current`, `thermistor_voltage`, `force_bridge`, `pressure_bridge` and `torque_bridge` profiles use1–64 typed AI channels, either timing mode and MixedAI18. Resistance returns ohms with explicit wiring/excitation. Thermistors require finite A/B/C for NI's Kelvin/ln(R) model without a guessed curve/sign rule; only voltage excitation needs positive reference `r1_ohms`, not RTD R0. Physical bridges keep output range/units separate from electrical and physical calibration units and preserve declared coordinate pairs. Equal electrical coordinates are rejected; descending/equal-physical requests retain any original native error. Per-channel `calibration_provenance:declared_two_point_linear_request` labels declared mapping, not measured/performed calibration. Native F64 and finite ohms/temperature/force/pressure/torque undergo no host equation, interpolation or unit math. Explicit excitation and optional ac/dc/ground coupling are request-only; none/Stop-Clear do not guarantee physical-off or native acceptance. All old rows remain unchanged, without sensor/module/driver/OS qualification. See [NI input configuration](guides/industrial-data.md#ni-daqmx-analog-voltage-acquisition).

Additional `velocity_iepe`, `force_iepe`, `charge_acceleration`, `lvdt` and `rvdt` measurements support both AI timing modes and MixedAI, now with eighteen channel kinds. Output units and sensitivity units are separate explicit requests: velocity m/s can use mV/(mm/s), charge acceleration uses pC sensitivity, LVDT includes mV/V per milli-inch, and RVDT uses angular sensitivity. The first three require terminal configuration; LVDT/RVDT instead require explicit AC excitation frequency and four/five/six-wire mode. All five preserve driver F64 words and finite velocity/force/acceleration/position/angle without local conversion. Sensitivity accepts finite signed and zero requests with native rejection preserved; older acceleration keeps its positive-only rule. Excitation and optional coupling remain requests, with no inferred calibration, delivered power or physical device qualification.

LMI GoPxL `gopxl_ascii` requires profile `gopxl_1_3_measurement`, a numeric Ethernet ASCII endpoint (asynchronous output disabled externally), an explicit `delimiter`, `terminator` (`crlf`/`cr`/`lf`), `invalid_token`, `trailing_delimiter` (`absent`/`present`/`either`) and 1–64 named connection-map `measurements` IDs. Each poll sends only `measurement,<ids>` and requires one `M,V,D` triplet per ID in order. Values keep raw text, `scaled_i32` and an exact three-decimal `value`; decisions give `pass`/`decision_invalid` bits. `ERROR` replies, malformed or extra bytes withhold the whole poll with no retry or reconnect. Binary GoPxL GDP is not supported. See the [GoPxL guide](guides/industrial-data.md#lmi-gopxl-measurements-over-ethernet-ascii).

Opto 22 `optommp` requires profile `groov_expanded_inputs`, model `epic_pr1`, `epic_pr2`, `rio_mm1` or `rio_mm2`, a numeric TCP endpoint and 1–64 named channels, each with module, channel, `analog_eu` or `digital_state`, and exact `expected_module_id`/`expected_channel_type`. Selectors must also pass the documented input allowlist; outputs, energy, serial, disabled and FastADC layouts are rejected. Each poll checks unit type and Powerup Clear (clear it externally), rechecks every module/type, then reads native values and quality codes with ReadQuadlet only, under one original deadline. A failed poll is withheld whole; earlier polls stay recoverable. Analog keeps the exact binary32 word (numeric `value` only when finite), digital stays a U32, quality is a native code; units are `externally_configured_unqueried`. No writes, read-clear, retry or reconnect. See the [OptoMMP guide](guides/industrial-data.md#opto-22-groov-inputs-over-optommp).

Atlas Copco `atlas_open_protocol` requires profile `pf6000_v2_8`, a numeric Virtual Station TCP endpoint and externally configured Disconnect=None. It requests startup5/result3, preserves native scaled integers, local-time text, codes and raw frames, and checkpoints each JSONL result before its application ACK under the original read deadline. Directory sync is best effort where unsupported; neither checkpoint nor ACK proves universal power-loss safety or exactly-once capture. Duplicate IDs remain separate; no history, reconnect or tool commands. See the [PF6000 guide](guides/industrial-data.md#atlas-copco-pf6000-tightening-results).

See the [industrial data guide](guides/industrial-data.md).

Standard BiColor RGBG/BGRG 8/10/12 unpacked and 10p/12p packed formats support color conversion. Packed 10p requires even source width; 12p allows any positive width. A software region can have odd x/width within an admitted source. Missing red/blue components use documented horizontal interpolation within the selected region; RAW follows native transport normalization, and clipping uses only received pairs.

## Permissions

Put the full command and subcommand before the flags.
For example, run `capturefab jobs list --json`.
In `capturefab schema`, `commands` maps names to `[effect, summary]`.
`effect_with` lists the flags that increase the effect.

| Effect | Examples | Suggested policy |
| --- | --- | --- |
| `read` | `list`, `info`, `features`, `get`, `inspect`, `watch`, `jobs list`, `jobs wait`, `doctor`, `schema` | Allow. |
| `file_write` | `capture`, `record`, `preset save`, `preset import`, `preset export`, `preset rm`, `login add`, `login rm` | Limit file paths to the task directory. Treat stream destinations, preset changes and credential store changes as separate permissions. |
| `camera_write` | `set`, `auto`, `preset apply`, `jobs stop` | Limit to the task's cameras and job IDs. |
| `disruptive` | `exec`, `session start`, `session stop`, `serve`, `shell`, `debug write`, `debug rpc`, `network force-ip`, `network action` | Ask before running. |

- Allow only the intended subcommands. A prefix rule for `capturefab jobs` also matches `jobs stop all`.
  That command stops active jobs across sessions unless `--session NAME` limits it.
- Treat `--set` on `inspect`, `capture` or `record` as a camera write.
  The same applies to `--auto` on `capture` or `record`, and to `network --tune`.
  These changes can affect later commands.
- `network force-ip` is an explicit temporary network reconfiguration of one closed camera.
  Use a supplied MAC/interface/address/mask/gateway, and inspect its post-send uncertainty
  before deciding whether to repeat it. Its runtime reservation is advisory.
- `network action` sends one action on an explicit interface and may drive every matching configured sink. It is immediate by default; `--action-time-ticks U64` sends an explicit absolute device timestamp without clock qualification. Zero/past targets may act immediately, and post-send failure may leave a future action queued. Use explicit keys/mask and acknowledged expected peers, or explicit sent-only mode. It runs locally beside held acquisition and never retries. ACKs do not establish execution, saved frames or synchronization.
- `set --write-only ActionDeviceKey=U32` is a single non-restorable write on an explicit existing named session/camera. It does not read back, roll back, save a checkpoint or reopen a missing worker. Preserve post-write uncertainty on failure; ordinary read/write set transactions are unchanged.
- Industrial `camera`/`video` sources can declare exact `action_device_key` once for their shared selector. Conflicting keys or a simultaneous `ActionDeviceKey` RW setting fail before opening adapters/outputs. Startup and subsequent acquisition stay on the acquired worker; failure does not reconnect or replay the key. This does not send an action or signal physical readiness.
- Treat `inspect --preview PATH` and `debug xml -o PATH` as file writes.
  The default `inspect` preview goes to the private `inspect_dir` reported by `doctor --json`.
  Use `--preview none` to skip the image.

## Call patterns

Run the simulator examples in a scratch directory. For a physical camera, use its ID in place of `sim:0`.
Controls and measurement fields depend on the source.

### Find a camera

```sh
capturefab list --json
capturefab list --json --kind sim
```

Use `cameras[].id` with `-c`. If several cameras match and you omit `-c`, expect exit 2 and `camera_ambiguous`.
Use `list --kind gige,u3v` to limit discovery to GigE Vision and USB3 Vision.
For a camera already streaming multicast under another controller, use the explicit quoted
`gige-monitor://CAMERA_IP?interface=HOST_IP&mac=12_HEX_DIGITS` selector. It reads channel 0
configuration and receives frames without acquiring control or changing camera settings.
`info` reports `camera_access: "read_only_monitor"` and `acquisition_control: "external"`;
feature rows retain XML `access` declarations and add effective `writable: false`.
Omit settings, auto, positive `--warmup` and capture `--fresh` options. See the
[monitor requirements](guides/camera-compatibility.md#receive-an-existing-gige-multicast-stream).
For an installed GenTL/CTI producer, set `CAPTUREFAB_GENTL_PRODUCERS` to its absolute
library path (use the host path separator for several) and run `list --kind gentl`.

Use its complete opaque `gentl:` ID. See the [camera guide](guides/camera-compatibility.md)
for driver and payload requirements. Multipart local stills retain every component in raw companions. GenTL 1.6 stills can archive a supported complete GenDC container.

GenICam `Register` values are metadata descriptors in `get` and `features`; their buffers
are never read by an ordinary feature walk. Use `debug read --feature 'NAME[SELECTOR=VALUE]'`
or `debug write --feature 'NAME[SELECTOR=VALUE]' --hex HEX` for an explicit complete raw transfer
(up to 64 KiB, default 5s operation budget). Only declared selectors are accepted. A failed raw
write can leave a committed prefix; it does not run file/LUT commands or roll back.
For large writes, use `capturefab --json debug rpc -c CAMERA < write-register.json`; the
file carries `{"op":"write_register","name":{"name":"FEATURE"},"hex":"EXACT_HEX_BYTES","timeout_ms":5000}`.
This keeps the raw hex off argv and supports the full 64 KiB cap within the 1 MiB stdin limit.

Native GigE Vision also receives unchunked IMAGE All-in datagrams when the full metadata and image fit the negotiated packet size; a larger SCPS/MTU may be required. Native GigE Vision multipart requires a configured unchunked stream. Supported unpadded row parts can end early when the trailer proves every delivered prefix; padded or opaque shortened parts remain unsupported. For a
default still, enable one supported intensity component. To save a component explicitly,
use `capture --part-format Coord3D_C16 -c CAMERA -o part.raw`. Ordinary multipart
selections are `Mono8`, `RGB8` and `YCbCr411_8` for local RAW/PNG. Add `--part-index N`
(0–31) for an exact table ordinal; use the base selector for different same-owner
jobs and match any explicit GenTL URI binding. Selected ordinary `--auto` meters
that part in software; maps and Mono16 reject auto before settings are applied.
Measurement selections are
`Coord3D_C16`, `Confidence8`, `Confidence16`, `Mono16`,
`Coord3D_C10p` and `Coord3D_C12p` for local RAW or PNG.
Use `Coord3D_ABC16` or `Coord3D_ABC32f` for interleaved A/B/C point words saved as local RAW.
Specify a part format when the stream has no intensity image. Mono16 keeps the component
purpose unspecified and omits exposure and focus measurements.
For GenTL, if image selection is ambiguous, retry a complete ID from
`error.details.image_selectors[].camera`; inspect `error.details.parts` for the component table.
Capture results and failed-capture details expose bounded per-image `multipart` archive references.
For GenTL `Coord3D_C10p`, `Coord3D_C12p`, `Coord3D_C16`, `Confidence16` and
`Coord3D_ABC16` and `Coord3D_ABC32f`, copy a selector from the successful still's multipart companion JSON. Start the capture sequence with `--no-session` to switch
selectors on an exclusive producer. Save 16-bit maps as RAW or grayscale PNG. For GenTL point
clouds and ordinary GigE Vision/GenTL `Coord3D_ABC16` or native GigE Vision `Coord3D_ABC32f` profiles, save as RAW and use
`inspect --preview none` for frame metadata.
Native GigE legacy image-with-chunk payloads (`0x0005`) retain the original complete
payload in chunk companions. Chunk fields are interpreted only when the device XML declares
their chunk ports; otherwise their bytes remain opaque.
Increase `--timeout` to give GigE Vision and ONVIF discovery more time.
To open a known stream, pass its URL directly with `-c`.

### Learn what a camera can do

```sh
capturefab info --json -c sim:0
capturefab features --json -c sim:0 'Exposure*'
```

Start with `info` for the image size, pixel format and supported controls.
Use `features` with a pattern to find a feature's value, range or choices.
Use `--all` to include all visibility levels. If the response has `truncated`, narrow the search.

### Capture a well-exposed photo

```sh
capturefab inspect --json -c sim:0 --preview none
capturefab capture --json -c sim:0 -o 'shots/{camera}-{seq}.png'
```

Read `verdict` as a list. For example, `["ok"]` means exposure is within the measured limits.
If the scene is dark, bright or clipped, use `suggest.exposure_us` when present.
Add the `us` unit when you send that value back as `exposure=VALUEus`.
If the camera is still settling, measure again.
To adjust exposure before capture on a supported camera, add `--auto`.

The result gives absolute paths in `files`, plus the first saved frame's `time` and `settings`.
When present, `device_ts_ns` is the camera's clock in nanoseconds.
Saved-file metadata and frame indexes also retain `device_timestamp.ticks` and, when
reported, `frequency_hz`. These device clocks do not imply UTC or synchronization.
Native GigE/USB chunk artifacts keep the original bytes and XML-derived values,
units and per-feature errors when available.
`frames` counts frames received, including skipped frames. `dropped` and `incomplete` report losses.
Use `--fresh N` to skip N more frames after the last settings change.
Use `--inspect` to add the last saved frame's measurements.
Use `--manifest shots.jsonl` to append each saved file's path, time, size, SHA-256, settings and tags.

| Output | What to expect |
| --- | --- |
| PNG | Monochrome above 8 bits uses 16-bit gray. Color and Bayer use 8-bit color. |
| JPEG | `--quality` sets encoding quality. Without that flag, an MJPEG source can keep its original JPEG data. The result says `"passthrough": true`. |
| Raw | The frame's bytes at full bit depth. For example, `shot.raw` gets a `shot.raw.json` sidecar that describes the layout. |

`bit_depth` gives bits per sample in the saved image. `source_bits` appears when the conversion loses bit depth.
Raw output from webcams, streams and files uses the decoded format.
Qualified IDS `Mono10g40IDS`/`Mono12g24IDS` frames carry a closed `pixel_encoding`
with their original vendor name/code and reported GenTL namespace. They are not PFNC
packing aliases: RAW keeps grouped bytes, PNG keeps full sample precision, and JPEG
reduces to 8 bits. Use the camera's XML-declared format; bare custom codes are not decoded.
Native GigE and partial IDS groups remain unsupported.
LUCID `Coord3D_ABCY16` (custom `0x82400403`, A/B/C coordinate codes plus Y intensity, four
little-endian u16 words) is accepted on native GigE and GenTL only with LUCID identity and an
exact XML declaration; save it as RAW, whose `pixel_encoding` and four-component
`sample_layout` describe it. Coordinates stay unscaled.
See the [pixel format table](guides/camera-compatibility.md#pixel-formats).

### Set and verify in one call

```sh
capturefab inspect --json -c sim:0 --set exposure=5ms --preview none
capturefab set --json -c sim:0 exposure=5ms gain=3 --dry-run
```

`inspect --set` applies the change, then measures a frame exposed after it.
`set --dry-run` shows the planned writes and warns when a value can only be validated during the write.
Run `set` without `--dry-run` to apply the change. Read `changed[].to` for the values read back.
If a write fails, Capturefab tries to restore the prior values.
Examine `error.details.rollback.failed` before you assume the camera is back in its previous state.

### Focus or aim loop

```sh
capturefab inspect --json -c sim:0 --keep 5m --size 384
capturefab watch --json -c sim:0 --for 2s
```

Compare `focus.sharpness` for the same camera and scene. Higher values indicate sharper focus.
Use `--region 0.25,0.25,0.5,0.5` to measure the central half of the image in each direction.
At a terminal, use `capturefab watch -c sim:0 --show` to see the frame and measurements.

The automatic session keeps a camera open for 60 seconds by default.
Use `--keep 5m` for a longer loop. Use `--no-session` to open the camera for one command.

### Burst

```sh
capturefab capture --json -c sim:0 -n 20 -o frames/
```

Read `consecutive` and `gaps` when they are present and frame continuity matters.
With repeated `-c`, `capture` arms the cameras together.
Read each result in `cameras` and use `skew_ms` to compare when the first frames arrived.

### Time lapse in the background

```sh
capturefab capture --json -c sim:0 --every 1s --for 5s -o 'lapse/{seq:5}.png' --background
```

Pass the returned `job.id` to `capturefab jobs wait --json ID --timeout 10s`.
The session keeps the job running after the capture command exits.
`jobs wait` succeeds when all requested jobs finish successfully.
A failed job returns its own error code. If the wait itself times out, it returns exit 4 and the job can continue.
Call `jobs wait` again to continue waiting.

Use `jobs list --json --running` for a snapshot of active jobs.
Use `jobs stop --json ID` to stop one job and keep the files it saved.
Finished jobs stay in the list for an hour. Use the ID the command returned.
Use `capture --in 5m` or `capture --at 14:30` to schedule a later start.

### Triggered capture

```sh
capturefab capture --json -c 'sim:0?line0=every:500ms' --set trigger=software -o software.png
capturefab capture --json -c 'sim:0?line0=every:500ms' --set trigger=line0:rising -o 'part-{seq}.png' --timeout 3s
capturefab set --json -c 'sim:0?line0=every:500ms' trigger=off
```

For `trigger=software`, `capture` sends the software trigger.
For `trigger=line0:rising`, it waits for a rising edge on Line0.
This simulator produces a Line0 pulse every 500 ms. Plain `sim:0` has no Line0 pulses.
If the wait expires, read `error.details.trigger` with exit 4 and `trigger_timeout`.
Trigger settings can carry over to later calls. Set `trigger=off` to return to free-running capture.

For network Action triggers, retain a named session, configure its WO action key with `set --write-only`, then set the actual selector/group/mask and trigger features before streaming. Arm a background capture first. Run `network action --mode acknowledged --interface IP --device-key U32 --group-key U32 --group-mask U32 --expect CAMERA_IP --timeout 3s` without explicit `--session`. Add `--action-time-ticks U64` only for an independently chosen absolute device timestamp; the sender does not inspect clocks or wait until execution. Wait for the capture job and saved file separately; sent-only omits `--expect` and reports only a successful send. Action names/selectors vary by device, and no PTP or exposure-synchronization claim follows. See [the compatibility guide](guides/camera-compatibility.md#immediate-network-actions).

### Record video

```sh
capturefab record --json -c sim:0 -o clip.mp4 --for 2s
capturefab record --json -c sim:0 -o clip.mkv --codec ffv1 --for 2s
```

Read `output`, `ended` and `finalized` to find the recording and see how it finished.
With `--max-size`, a cap too small for FFmpeg to finish the container can give `finalized: false`.
FFV1 preserves samples without loss. It stores a Bayer mosaic as grayscale samples.
HEVC and AV1 encoding depend on an available hardware encoder.
On Linux and macOS, compatible H.264 or HEVC input can be copied without re-encoding.
Look for `"encoder": "copy"`. A different `--codec`, or use of `--fps` or `--bitrate`, disables copy mode.

Without `--for`, stop a background recording with `jobs stop --json ID`.
For streaming output, start a receiver that accepts a publisher before you use its URL with `-o`.
See the [recording and streaming workflow](guides/cli-workflows.md#record-and-stream-video).

### Keep settings for later

```sh
capturefab preset save line1 --json -c sim:0
capturefab preset apply line1 --json -c sim:0 --dry-run
capturefab preset apply line1 --json -c sim:0
```

A preset stores settings on this computer. Review the dry run before you apply it.
On a camera with writable user sets, `--persist UserSet1` also saves the settings there and makes that set the power-on default.

### Password-protected streams

```sh
printf '%s\n' "$PASSWORD" | capturefab login add --json rtsp://cam.local/stream --user admin --password-stdin
capturefab capture --json -c rtsp://cam.local/stream -o door.png
```

Replace the example URL and user with the camera's values. Supply the password from your secret store.
Capturefab saves the password in the operating system's credential store and redacts credentials in output.
For ONVIF, use `capturefab onvif resolve --json HOST` to list profiles and their RTSP URLs.

## Errors and what to do

| `error.code` | Exit | Next action |
| --- | --- | --- |
| `camera_ambiguous` | 2 | Choose an ID from `error.details.candidates`. Pass it with `-c`. |
| `camera_not_found` | 3 | Run `list`. Examine the cable, network or address. |
| `camera_busy` | 3 | Look for `error.details.held_by`. Use the session that holds the camera or wait for the active job. |
| `feature_not_found` | 2 | Search with `features -c CAM PATTERN`. Look for suggestions in `error.details.did_you_mean`. |
| `out_of_range`, `invalid_choice` | 2 | Follow the hint or use the range or choices in `error.details`. |
| `not_writable`, `feature_locked` | 2, 1 | Find the controlling setting in `error.details.locked_by` when present. |
| `invalid_duration`, `usage` | 2 | Read `capturefab schema COMMAND`. Correct the input. |
| `exists` | 1 | Choose a new path. Use a sequence template for captures or recordings. |
| `frame_timeout`, `trigger_timeout` | 4 | Examine exposure, frame rate and trigger settings with `info`. Increase the timeout or supply the expected trigger. |
| `auth_required`, `auth_rejected` | 3 | Save or update the sign-in with `login add`. |
| `ffmpeg_missing` | 3 | Run `doctor`. Use `CAPTUREFAB_FFMPEG` to select a working executable. |
| `permission_denied` | 3 | Follow the hint. On macOS it can name Camera or Local Network access for the terminal app. |
| `disconnected`, `worker_crashed` | 3 | Examine `error.details` for files or jobs left behind before retrying. |
| `unsupported` | 5 | Use an operation the source and build support. |
| `storage_full` | 6 | Free space or select another disk. `error.details` gives the path and can report free bytes. |
| `interrupted` | 130, 143, 129 | Look in `error.details` for partial work such as `files` or `job`. |

The default free-space floor is 1% of the disk, with a minimum of 512 MiB and a maximum of 20 GiB.
`CAPTUREFAB_MIN_FREE` changes the floor. A command can raise an existing session's floor for its own output.
To change the session's FFmpeg, JPEG or hardware decoder settings, restart the session with the new environment.
Use `doctor --json` to examine the available tools and methods.

## Saving tokens

- Use `info` or `get NAME…` for specific values.
- Use `inspect --preview none` for measurements alone or `--size 384` for a smaller preview.
- Use `capture --inspect` when you also want the saved frame's measurements.
- Use `capturefab shell --json -c sim:0 -e 'inspect --preview none' -e 'capture -o shell.png'` to run several commands in one process.
  Batch mode stops at the first failed command unless you pass `--keep-going`.
