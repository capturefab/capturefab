# Capturefab for agents and shell scripts

Capturefab opens its desktop app without a subcommand. Use explicit subcommands for automation. It acquires camera images and video, reads sensor and PLC signals (Modbus TCP, MQTT, OPC UA, MTConnect), catalogs saved files and samples in a DuckDB dataset, and exports analysis bundles. The installed binary's `schema` and `--help` output define its supported commands.

```sh
capturefab schema --json
capturefab doctor --json
capturefab --help
```

Successful structured output goes to stdout; diagnostics go to stderr. Read the process exit code before you use a result. Every `--json` data command, including `schema`, returns `{"version":1,"ok":true,"result":...}` or `{"version":1,"ok":false,"error":{"code":"...","message":"..."}}`. Read the automation contract from the `result` field of `schema --json`. Plain `schema` prints that contract directly. Its `cli` command tree lists subcommands, options, defaults and accepted values. Contract sections such as `result.capture`, `result.dataset` and `result.signals` sit at the top level of `result`; `result.rpc.commands` has an example of each session operation. `doctor --json` lists compiled build features in `features` and the compiled signal protocols in a separate top-level `signals` map. Keep credentials out of logs, command transcripts and generated configuration files where possible.

Exit codes are 1 `operation_failed`, 2 `usage`, 3 `unavailable`, 4 `timeout`, 5 `unsupported`, and 6 `storage_full`. `--timeout-ms` sets a per-frame or transport timeout of 1–60000 ms. `--request-timeout 5m` sets a session request deadline. Its default depends on the capture count. A client timeout does not cancel an accepted session command. Examine `status` before you retry a write.

Malformed supplied field values return `usage` (exit 2) before connection or capture/forward `--set` and `--auto` changes. Live camera, absent destination/drive, storage and I/O failures keep their operation categories. Session commands and RPC envelopes reject unknown fields. Use `schema` to find the field names. Rejected writes to Auto-managed features keep Auto enabled. Valid assignments run in order; a later live failure does not reverse earlier successful writes.

Durations and timestamp fractions must resolve to whole milliseconds. `1.001s` is valid; Capturefab rejects `1.1ms`. With delete-oldest, `--max-age` must use whole seconds, with a minimum of `1s`. All scheduled frames must occur within 10 years. A past timestamp starts capture immediately; a zero RPC interval captures consecutive frames. Two active jobs on one camera cannot target the same static output (a fixed filename or a `{frame}`-only template in fail mode). Directories, `{seq}` templates and `on_exists: number` outputs can be shared.

Capture paths are a file, a directory or a template, and do not accept network URLs; see [name outputs and tag captures](#name-outputs-and-tag-captures). Recording reservations must stay within the requested budget. Capturefab does not reduce `--max-file-size` automatically. `record`/`forward --duration 30s` works with temporary workers and persistent sessions. It keeps the CLI active and stops the encoder at the deadline or Ctrl-C. Without a duration, session forwarding returns immediately and runs until `stop-forward`; direct forwarding stays in the foreground.

An explicit `CAPTUREFAB_MAX_CAMERAS` must be an integer from 1 to 64. `CAPTUREFAB_FRAME_BYTES` must be 4096–268435456 bytes and a multiple of 64. Malformed overrides fail before worker startup. They do not select the default values.

## Use an existing visible session

Camera/session commands, discovery and network scans automatically attach to the only authenticated active GUI or `serve` session. With none, supported camera commands use a temporary worker. With several, choose `--session NAME` or receive `usage` (exit 2); the CLI never guesses. `signals read` and `signals browse` need no workbench, so with several sessions they run in the CLI process instead. List sessions to identify a user's visible workbench, or use `--direct --camera ID` for a separate temporary worker. Binary stdout captures use a temporary worker and cannot combine with `--session`; use `--direct` to override an environment session. Camera control is often exclusive.

```sh
capturefab sessions --json
capturefab --session gui status --json
capturefab --session gui connect 192.168.1.10
capturefab --session gui get Width Height ExposureTime --json
capturefab --session gui set ExposureTime=5000
capturefab --session gui capture -n 10 -o frames --json
```

Use the session name returned by `sessions`; it may differ from `gui`. `CAPTUREFAB_SESSION` pins camera/session commands; `CAPTUREFAB_CAMERA` supplies a default camera. `--direct` overrides the environment session and conflicts only with explicit `--session`. Inspection, feature writes, capture, recording, XML and memory commands connect a missing `--camera` source to the session. Persistent controls such as `start` and `auto` require an already connected camera; use `connect ID` first. Desktop and browser camera selection is local to each participant; `select ID` sets the default for CLI calls that omit `--camera`. Use explicit targets when collaborating. Use `rpc` with `schema` for the complete versioned local session interface.

Humans and agents share capture sessions: commands are ordered per camera, cameras run independently, and overlapping schedules share acquisition. Use `--actor NAME --actor-kind agent` (or `CAPTUREFAB_ACTOR` and `CAPTUREFAB_ACTOR_KIND`) to identify an agent; human is the default kind. Desktop and browser activity updates immediately without moving camera selection, drafts or focus. Activity includes `id`, `origin` (`client`/`gui`), `actor` (`id`, `name`, `kind`), `operation`, `camera`, `feature`, `label`, `state`, timestamps, detail and saved files. JSON states are `queued`, `running`, `succeeded`, `failed`; timestamps are Unix milliseconds. `status --json` exposes global activity and `activity_revision`, including when targeted to a busy camera. Read-only observations and authenticated session probes do not create entries. The session retains 200 completed entries plus active commands/jobs across selection and disconnect.

Journal strings/path previews are capped: completed captures retain at most the first eight paths in `files`, while `file_count` gives the actual total; schedule previews advance with acquired frames. The capture command's result still returns its complete file list. Targeted `--camera ID status --json` keeps full coordinator context (`cameras`, `devices`, `network_scan`, logs and activity) with queried-camera fields at the top level. `target_camera` identifies the canonical queried ID; `active_camera` retains the session default for untargeted CLI calls, independently of desktop/browser local selection.

For immediate private IPC observation without an HTTP listener, call `capturefab --session NAME activity --json`, then `activity --after REV --wait 20s --json` with the returned revision. Each call returns one versioned `{revision,activity}` response, including on timeout, bypasses camera queues and creates no activity entry. RPC accepts `{"op":"activity","after":12,"timeout_ms":20000}`.

Raw `logs` retain `time`, `level`, and `message` and may add `timestamp:{unix_ms,utc_offset_seconds}`. `unix_ms` is signed Unix milliseconds for the absolute instant; `utc_offset_seconds` records the producer clock's seconds east of UTC (-86399..86399). Prefer this metadata to `time`, which is only legacy clock text without a date/zone. Old entries omit it; malformed optional metadata remains unavailable without discarding the diagnostic. Desktop/browser clocks use the viewer's local zone, and copied desktop logs include a dated numeric offset. This is an additive version-1 output field; envelopes and existing fields are unchanged.

For immediate agent observation over HTTP, fetch `/api/activity`, then repeat `/api/activity?after=REV&timeout_ms=20000` with the returned `result.revision`. All watchers wake on command and scheduled job changes. The response includes retained history; timeouts return current history, and stale or future revisions return immediately. Maximum timeout is 25000ms. `POST /api/command` optionally accepts `actor:{id,name,kind}` alongside `version`, `camera`, `command`; actor metadata is display attribution, not authentication. See [remote control](https://github.com/capturefab/capturefab/blob/main/docs/remote-control.md). Schedule state `succeeded` acknowledges creation; `entry.job` tracks `id`, `worker_pid`, `status`, `count`, `captured`, `last_file`, `error` through completion/cancellation/failure. Forward success acknowledges encoder start; consult camera status for ongoing forwarding.

Reversible camera changes from GUI, CLI and RPC clients share each camera worker's bounded history. Use `capturefab --session NAME [--camera ID] history --json` to inspect the next undo/redo labels, and `undo` or `redo` to apply them. RPC accepts `{"op":"undo"}` and `{"op":"redo"}`; `status.history` and `cameras[].history` hold the labels. Replay detects conflicting state and retains the entry on failure. History lasts for the camera connection; a new change clears redo. Captures, uploads, recordings, schedules, connection changes, arbitrary commands and raw memory writes cannot be reversed by this camera history. Stopping a stream with schedules or forwarding is also excluded. Inspect the labels before undoing someone else's changes.

Scanner settings are reversible. Scanner acquisition, including Start and Stop, may move or consume physical pages and cannot be undone or replayed.

A foreground headless session can also be used:

```sh
capturefab --camera 192.168.1.10 serve --name bench --stream
# In another terminal:
capturefab --session bench status --json
```

## Apply repeatable JSON workflows

Use workflow files for ordered multi-camera setup and acquisition without shell escaping. [The workflow guide](https://github.com/capturefab/capturefab/blob/main/docs/cli-workflows.md) includes [validated examples](https://github.com/capturefab/capturefab/tree/main/examples/workflows) for timelapse, scanner feeders and bounded recording.

```sh
capturefab workflow validate workflow.json --json
capturefab workflow apply workflow.json --dry-run --json
capturefab --session bench workflow apply workflow.json --json
```

The document is `{"version":1,"steps":[{"command":{"op":"connect","camera":"sim:0","timeout_ms":2000}},{"camera":"sim:0","command":{"op":"capture","output":"shot.png","count":1,"timeout_ms":2000,"format":"png"}}]}`. Each optional step `camera` addresses an existing connection; `connect` and `select` use `command.camera` and must omit the outer target. `discover` and `network_scan` also omit it. Command fields are the same strict RPC fields; CLI conveniences such as `--set`, inferred formats, `--duration` and unit-bearing strings are not workflow fields.

Validation and dry-run are offline, resolve outputs and redact URL credentials. Files/stdin are capped at 1 MiB and 1–256 steps; unknown and duplicate fields fail. Every step passes offline validation before any command runs. Live feature bounds, destinations and output availability remain execution checks. Apply requires a persistent session (automatically selected when there is exactly one), rejects global `--camera`, executes sequentially and stops at the first failure. Earlier effects remain; `error.details.completed`, `completed_steps` and one-based `failed_step` support recovery. Do not blindly replay the full file after a partial failure: captures never overwrite.

Relative local sources and outputs use the workflow file directory, or caller directory for `workflow ... -`. `file:relative.mkv` and `sim:replay:relative-folder` sources resolve in connect/select commands and outer camera targets; bare existing filenames are recognized as file sources. Ordinary CLI/RPC source and output paths use the caller directory. Prefer explicit `file:` for local-source names that could also identify a camera. Network/device locators, destination-relative names and stream URLs stay unchanged. Workflow captures cannot use binary stdout. Scheduling and encoder startup return immediately; keep the session alive and stop them explicitly. For timed recording, use the CLI's `record --duration` rather than adding a nonexistent workflow duration field. Steps can also use `dataset_open`, `dataset_note`, `dataset_export`, `dataset_close`, `signal_start` and `signal_stop` without an outer `camera`; `dataset_open.path` and `dataset_export.output` resolve like capture outputs. [plant-line.json](https://github.com/capturefab/capturefab/blob/main/examples/workflows/plant-line.json) opens a dataset, starts `sim-signal://line1` and saves a tagged `{seq}` capture.

PowerShell can use the same files directly. For stdin, use `Get-Content -Raw workflow.json | capturefab workflow validate - --json`; this changes the output base to the calling directory.

## Discover, inspect, configure, acquire

```sh
capturefab discover --json
capturefab --session bench network-scan --json
capturefab --camera 192.168.1.10 features --json
capturefab --camera 192.168.1.10 get Width Height PixelFormat --json
capturefab --camera 192.168.1.10 set ExposureTime=5000
capturefab --camera 192.168.1.10 capture -n 10 -o frames --json
```

Selectors can be a discovered camera ID, unique serial, or GigE IPv4 address. GenICam feature names, access modes, increments, selectors, and bounds depend on the device. Inspect `features` and read back important settings. Capture outputs never overwrite existing files. Known single-file or template extensions infer PNG/JPEG/raw/PGM/PPM; directory sequences default to PNG. `--format` overrides inference. Use `--format raw` when preserving an unsupported pixel format matters.

`network-scan` inventories IPv4 interfaces, rediscovers GigE cameras, checks control reachability and tunes connected GigE cameras with a budget shared per receive interface. RPC: `{"op":"network_scan","timeout_ms":1000}`; result and status retain `network_scan` with `interfaces`, `cameras`, `warnings` and `scanned_at_ms`. Camera entries expose `reachable`, `health` (`unreachable`, `limited`, `unverified`, `ready`, `unstable`), `summary`, `details` and optional `tuning`. Discovery alone never proves a fast or stable stream. Preview streams pause/resume for packet probes; recording, forwarding and active schedules prevent tuning, with an actionable report. Host IPs, routes and MTUs stay unchanged. Per-camera RPC `{"op":"optimize_network","bytes_per_second":100000000}` applies a supplied budget; use `network_scan` for shared-link accounting. These writes are outside undo history; auto mode honors the budget during the connection.

## Camera profiles and specialized acquisition

Camera identity JSON now includes `profile: {kind, source, evidence, capabilities}`. Read it in `discover` result `devices[]`, `info` result `camera`, and `status` result `connected` or `cameras[].info`. Discovery may have only identity hints; connection refines the profile from readable features. Kinds are `unknown`, `area_scan`, `line_scan`, `depth`, `microscope`, `thermal` and `document_scanner`. Sources are `unknown`, `transport`, `identity`, `features` and `device_metadata`. `evidence` is a string array explaining the classification; `capabilities` is an array of advertised control families. `schema --json`'s `result.camera_profiles` describes the enum values and fields. Profiles add metadata to identities; the JSON envelope and capture formats are unchanged.

Detection prioritizes scanner transport, explicit `DeviceType`/`CameraType`/`SensorType` class, explicit `DeviceScanType=Linescan`, specialty feature families, remaining declared scan geometry, model identity, then conventional frame dimensions/timing. `Height=1`, high bit depth and a vendor name alone are not type evidence. A profile does not guarantee a feature exists or is writable: inspect the live feature list, selector values, choices and bounds before setting it.

The GUI puts line geometry/timing and triggers/encoders first for line scan; components, depth range and calibration first for depth; optics, illumination and stage first for microscopes; temperature/radiometric controls first for thermal; and scan controls first for document scanners. Microscope image previews keep the focus region and sharpness tools. Line previews show recent received display rows and gaps for skipped blocks; they can downsample large blocks and are not complete encoder-aligned reconstructions. Scalar depth/thermal previews automatically color the raw sample range and show its bounds. **Sensor pixels** returns to the ordinary preview. Display transformations do not change acquired or raw captured bytes, and captures save individual line blocks rather than their preview history.

For depth and temperature measurements use `--format raw` and separately retain the selected component, unit, scale, offset and calibration features. Raw capture preserves sample bytes and precision; it does not embed dimensions, pixel format or calibration metadata, so retain those separately. PNG, JPEG, PGM, PPM and forwarded video are 8-bit visualizations; forwarding lacks measurement calibration and the GUI color map. A raw-value preview legend is not a calibrated distance or temperature.

Existing transports acquire single rasters. Depth sources need a compatible single-component stream; multipart, GenDC, point-cloud payloads and vendor SDK-only access remain unsupported. Type detection does not establish physical camera compatibility. Generic auto exposure is unsupported for line scan, depth, thermal and document scanner sources; configure their timing or measurement controls directly. Image cameras, including microscopes with suitable advertised controls, retain auto exposure support.

Use the built-in typed cameras to exercise automation without hardware. Selectors `sim:line[:name]`, `sim:depth[:name]`, `sim:microscope[:name]` and `sim:thermal[:name]` accept optional names. `sim:0` and arbitrary other names retain the area-scan pattern camera:

```sh
capturefab --camera sim:0 capture -o pattern.png --json
capturefab --camera sim:line capture --set Height=64 \
  --set AcquisitionLineRate=2000 --set ExposureTime=100 -o line-block.png --json
capturefab --camera sim:depth get PixelFormat Scan3dCoordinateScale \
  Scan3dCoordinateOffset Scan3dCoordinateUnit --json
capturefab --camera sim:depth capture --format raw -o depth.raw --json
capturefab --camera sim:microscope capture --set FocusPosition=50 \
  --set Magnification=4 --set IlluminationIntensity=70 -o micrograph.png --json
capturefab --camera sim:thermal get TemperatureLinearResolution TemperatureUnit --json
capturefab --camera sim:thermal capture --format raw -o thermal.raw --json
```

## Name outputs and tag captures

`capture` and local `record`/`forward` outputs accept a file, a directory or a template. Placeholders: `{camera}` (`model-serial`, limited to `A-Z a-z 0-9 . _ -` and 64 bytes, never the address), `{date}` (`YYYY-MM-DD`), `{time}` (`HHMMSS`), `{datetime}` (`YYYYMMDD-HHMMSS`), `{seq}` (next free number for that folder and pattern, 4 or more digits) and `{frame}` (1-based, 6 digits). Times are local time when the frame is requested. Any other `{name}` is `usage` (exit 2). `{seq}` may appear once, only in the file name, and never directly before a digit or `{date}`, `{time}`, `{datetime}` or `{frame}`. With `-n` above 1, a template without `{frame}` or `{seq}` needs `--on-exists number`.

```sh
capturefab --session gui --camera sim:0 capture -o 'line1/{date}/{camera}-{datetime}-{seq}.png' \
  --context line=L1 --context part=A-100 --json
capturefab --session gui --camera sim:0 capture -o shot.png --on-exists number --json
capturefab --session gui --camera sim:0 record -o 'video/{camera}-{seq}.mp4' --duration 30s --json
```

`--on-exists fail` is the CLI and RPC default and refuses an existing name; `number` writes `stem-0001.ext`, `stem-0002.ext` and so on. `{seq}` always picks a free number. A directory continues `frame-NNNNNN` after its highest existing number. Nothing is overwritten; after 64 lost races the command fails with exit 1 (`no free file name for OUTPUT after 64 attempts; choose another output`). The GUI always numbers, and saves to `~/Capturefab/{date}/{camera}-{datetime}-{seq}.png` (`.mp4` for recordings) by default.

`--context KEY=VALUE` (RPC `context:{...}`) tags captures, schedules and recordings. Keys match `^[a-z][a-z0-9_]{0,31}$`; at most 16 entries with 256-byte values; a duplicate key is exit 2. The GUI's Capture tags send `line`, `station`, `part`, `lot` and `note`. Read the saved names from the result, not from the template: capture returns `{files:[path],frames:[{id,width,height,pixel_format,timestamp_ns,bytes,wall_ms,file?}],count,context?,destination?,queued_uploads?,queued?:[{file,upload_id,key,url}]}` with `wall_ms` in UTC milliseconds; `file` is absent for stdout. `record`/`forward` returns `{forwarding,since_ms,path?}`, where `path` is the resolved local recording file; `status` shows `forwarding_since_ms` per camera and at the top level while forwarding. RPC example: `{"op":"capture","output":"line1/{date}/{camera}-{datetime}-{seq}.png","count":1,"timeout_ms":2000,"format":"png","on_exists":"number","context":{"line":"L1","part":"A-100"}}`.

## Read and log sensor and PLC signals

```sh
capturefab signals read sim-signal://line1 --json
capturefab signals read 'modbus-tcp://10.0.0.20/1/holding/0?type=f32&name=temp_c&unit=degC' --json
capturefab signals browse opc.tcp://10.0.0.30:4840 --duration 5s --limit 500 --json
capturefab log sim-signal://line1 --interval 1s --duration 10m --dataset plant.duckdb --csv line1.csv --json
capturefab --session gui signals start 'mqtt://broker.local/plant/line1/#' --interval 1s --json
capturefab --session gui signals --json
capturefab --session gui signals stop all --json
```

URI forms (see `result.signals.uris` in `schema --json`):

- `modbus-tcp://HOST[:502]/UNIT_ID/coil|discrete|input|holding/ADDRESS[?count=N&type=u16|i16|u32|i32|f32|f64|bool&order=abcd|cdab|badc|dcba&scale=F&offset=F&name=TAG&unit=UNIT]`. Addresses are 0-based protocol addresses (Modicon 40001 is `holding/0`); at most 125 registers or 2000 bits; `value = raw*scale+offset`. Tags are `name` or `table/address`, with `[i]` when `count>1`.
- `mqtt[s]://[USER[:PASS]@]BROKER[:1883|8883]/FILTER[?client_id=ID&qos=0|1]`. `#` only as the whole last level, `+` as a whole level. JSON objects flatten to `topic/field` (depth 4, 256 leaves); a top-level `ts`, `timestamp` or `time` (Unix seconds below 1e11, milliseconds, or RFC 3339 text) becomes `source_ts_ms`; the others stay tags. Binary payloads (not UTF-8, or with control characters other than tab, CR and LF) become bounded `hex:...` text. Payloads up to 256 KiB; one sample per tag per interval.
- `opc.tcp://[USER[:PASS]@]HOST[:4840][/PATH]?node=NODE_ID[&node=...][&root=NODE_ID][&security=none]`. `read` and `start` need 1..1024 nodes; `browse` starts at `root` (default `i=85`) and goes 4 levels deep. SecurityPolicy None only: a password travels unencrypted and the helper warns on stderr.
- `mtconnect[s]://HOST[:5000|443][/DEVICE]` polls `/current`; tags are `DEVICE/DATA_ITEM_ID`; `UNAVAILABLE` has quality `bad`.
- `sim-signal://LINE[?tags=1..64]` gives `LINE/temp_c`, `pressure_bar`, `count`, `running`, `state` and `vibration` without hardware.

Percent-encode `@` in topics and node IDs, and `/`, `?`, `#` or `@` inside credentials. Credentials are redacted everywhere; prefer brokers and servers that do not need a password in the URI, and never echo one into a transcript. Source IDs are `signal:<16 hex>` from the redacted URI. `signals read` uses `--timeout-ms`; `browse` takes `--duration` (100ms..60s) and `--limit` (1..5000) and returns `unsupported` (exit 5) for Modbus. `signals start` (`--interval` 100ms..1h, repeatable `--tag`) and `signals stop ID|all` need a running GUI or `serve` session, otherwise exit 2. `log` takes 1..16 URIs, runs until `--duration` or Ctrl-C, needs `--csv` (a new file; an existing one is exit 1 and untouched, like an existing capture or export output, and a run that fails before logging a sample removes its empty CSV) and/or `--dataset`, and returns `{sources,dataset,csv,samples,written,dropped,duration_ms,warnings?}`; `warnings` says when nothing was logged, for example because no `--tag` matched. The CSV header is `ts_ms,source_ts_ms,source_id,tag,value,quality`.

RPC: `signal_read {uri,timeout_ms}` returns `{id,uri,protocol,tags:[{tag,unit,kind}],samples:[{ts_ms,source_ts_ms?,tag,value,quality}],dropped}`; `signal_browse {uri,duration_ms,limit}` returns `{id,uri,protocol,items:[{id,name,kind,data_type?,unit?,value?}],truncated}`; `signal_start {uri,interval_ms,tags?}` returns a SourceStatus; `signal_stop {id}` returns `{stopped:[SourceStatus]}`; `signals` returns `{sources:[SourceStatus],persisting,dropped,sink_error}`. SourceStatus is `{id,protocol,uri,interval_ms,state,started_ms,last_sample_ms,samples,written,dropped,helper_dropped,restarts,error,helper_pid,tags,tags_total,tags_truncated}`, with `state` one of `connecting`, `running`, `reconnecting`, `failed`, `stopped`. Session snapshots carry at most 32 tags per source; `signals` returns up to 256. HTTP `GET /api/signals?history=1` adds up to 120 recent points per tag.

`ts_ms` is the host clock (UTC ms) when the helper read the value; `source_ts_ms` is the device clock. A session holds at most 16 sources and 4096 tags per source. Without an attached dataset or CSV, samples are kept only as latest values and `persisting` is false; that is not counted as dropped. A helper restarts at most 5 times before its source is `failed`. Exit codes: 2 bad URI or arguments; 3 connection refused or unknown source ID; 4 timeout; 5 protocol not compiled in; 6 storage full while writing CSV; 1 for `already logging; stop it first` or the 16-source limit. Ask the user before connecting to production PLCs; Capturefab only reads values and never writes to a device.

## Catalog captures and export datasets

```sh
capturefab --session gui dataset open /data/plant.duckdb --json
capturefab --session gui --camera sim:0 capture -o 'line1/{camera}-{seq}.png' --context part=A-100 --json
capturefab --session gui dataset note "nozzle changed" --context line=L1 --json
capturefab --session gui dataset status --json
capturefab --session gui dataset query "SELECT path, tag, value_double, sample_age_ms FROM captures_with_signals LIMIT 50" --json
capturefab --session gui dataset export -o /data/exports/shift-b --json
capturefab dataset query --file /data/plant.duckdb "SELECT count(*) FROM samples" --json
```

One `.duckdb` file is one dataset, owned by one session's coordinator. `--dataset FILE.duckdb` (env `CAPTUREFAB_DATASET`) on `capture`, `record`, `serve`, `gui` and `log` creates or opens it; a session that already records to another file fails with `session NAME already records to PATH; close it first`. `dataset open FILE [--new|--existing] [--max-bytes 8GiB]` needs a session; `--new` refuses an existing file and `--existing` a missing one. Existing files are never overwritten, and a non-Capturefab file is refused unchanged.

Tables: `captures` (one row per saved image: `wall_ms`, `camera_id`, identity, frame metadata, `format`, `bytes`, `path`, `sha256`, `origin`, `job_id`, `destination`, `object_key`, `upload_id`, `remote_url`, `uploaded_ms`, `context`), `recordings`, `signals`, `samples` (`ts_ms`, `source_ts_ms`, `source_id`, `tag`, `value_double`, `value_text`, `quality`), `events` (notes, open/close, data and signal gaps, signal errors) and the view `captures_with_signals` (each capture × each tag with the latest sample at or before `wall_ms`, plus `sample_age_ms`). Every `*_ms` column is UTC Unix ms from the host clock. `context` is JSON text; cast with `::JSON`. Booleans are `value_double` 0/1 plus `value_text` `true`/`false`. `uploaded_ms` currently stays null; read upload completion from `uploads --json`.

`dataset query [--file FILE] SQL [--limit 1..10000]` accepts one `SELECT` or `WITH` statement, read-only, with external file access blocked, and always rolls back. It returns `{columns:[{name,type}],rows:[[...]],row_count,truncated}`; results are capped at 8 MiB of JSON, SQL at 64 KiB, and the default timeout is 30 s (RPC `timeout_ms` up to 300000; timeout is exit 4). Integers beyond ±2^53 and decimals are strings, timestamps RFC 3339, blobs base64. Without `--file`, `status`, `query` and `export` need `--session` (else exit 2). `--file` on a file a session holds is exit 3: `dataset X is locked by another process; reach it with --session NAME`.

`dataset export [--file FILE] -o NEW_FOLDER [--format csv,parquet,json,jsonl] [--tables captures,...] [--since RFC3339] [--until RFC3339]` returns `{output,files:[{path,table,format,rows,bytes,sha256}],tables:{name:rows},formats}`. Defaults are Parquet and CSV (CSV only without the `parquet` build feature; other formats are then exit 5). It also writes `captures_wide` (one column per tag, only for 1..256 tags), `manifest.json` (`format_version` 1, columns with types and descriptions, file SHA-256s, cameras, per-tag statistics) and `README.md` (time base, file table, pandas/DuckDB/polars snippets and a summary for AI tools of at most 8 KiB). An existing output folder is exit 1. A budget preflight fails with `storage_full` (exit 6) before writing, so an export never triggers retention deletion. Give the bundle's `README.md` and `manifest.json` to an analysis model before its data files.

Limits: the dataset file plus WAL defaults to 8 GiB (`--max-bytes`, minimum 16 MiB; a new dataset already takes about 1 MiB); inserts stop below 512 MiB free disk (`Storage full: dataset ...`, exit 6); notes are 1..4096 bytes. Exit codes: 2 wrong extension, unknown table, multi-statement or non-SELECT query, `--limit 0`, `--new` with `--existing`; 3 locked; 4 timeout; 5 format unsupported in this build; 6 storage full. RPC: `dataset_open {path,mode:auto|create|open,max_bytes?}`, `dataset_close`, `dataset_status`, `dataset_note {text,context?,camera?}`, `dataset_query {sql,limit,timeout_ms}`, `dataset_export {output,formats?,tables?,since_ms?,until_ms?,storage?}`; RPC paths must be absolute. `result.dataset` in `schema --json` lists tables, formats, limits and exit codes. The GUI's Dataset section catalogs into `~/Capturefab/capturefab.duckdb` automatically unless the user turns that off.

## Let Capturefab tune exposure and frame rate

```sh
capturefab --session gui auto --balance 0.5 --json
capturefab --session gui status --json
capturefab --session gui manual --json
capturefab --camera 192.168.1.10 capture --auto -o tuned.png --json
```

`auto` is null in manual mode. `stable` and `limited` are settled states; `waiting` means acquisition is stopped. A `set` of a feature in `auto.managed` switches to manual first, and its result includes `"auto": null`. `--set` assignments apply before `--auto`; with `--session`, auto mode stays on afterwards. `capture --auto` still saves if exposure has not settled within half of `--timeout-ms` (at most 3 s); check `result.auto.state`. Settings written by auto mode remain after `manual`; `manual --revert`, disconnect and session exit restore them where possible. Ask the user before enabling auto mode on a camera another application depends on; it can change frame rate, throughput limit and white balance.

## Network and operating system cameras

```sh
capturefab --camera 'rtsp://camera/live' capture -o rtsp.png --json
capturefab onvif discover --json
capturefab onvif resolve 'http://camera/onvif/device_service' --json
capturefab --camera 'onvif:http://camera/onvif/device_service' capture -o onvif.png --json
capturefab native --json
```

ONVIF locators with credentials use `onvif://USER:PASS@HOST[:PORT]/onvif/device_service`; percent-encode reserved characters in the user name or password. Published session metadata redacts credentials. ONVIF SOAP currently supports HTTP, WS UsernameToken PasswordDigest, and HTTP Basic/Digest authentication. HTTPS SOAP and Media2-only devices are not supported by this implementation.

Native camera locators are `avfoundation:0` on macOS, `v4l2:/dev/video0` on Linux, and `dshow:video=Camera Name` on Windows. CSI cameras require an operating system driver exposing a suitable V4L2 device. Native enumeration and capture may require operating system camera permissions.

Native locators accept `?size=WIDTHxHEIGHT&fps=RATE`. Inspect `features` for advertised `VideoMode` choices and set a supported mode, `Width`, `Height`, or `AcquisitionFrameRate` before acquisition; stop a live session before changing them. V4L2 format listings do not establish frame-rate bounds. GigE transport status includes packet size, receive buffer, bounded resend and loss counters.

Media inputs and forwarding use bundled FFmpeg in release builds. Inspect `capturefab ffmpeg --json` to see the actual build's protocols and encoders. SRT support depends on the bundled FFmpeg configuration. `--hwaccel` configures decoding in new direct, GUI or `serve` workers; an existing session retains its setting.

## Scan documents

`capturefab scanners --json` lists platform document scanners; `discover` includes them with `transport: "scanner"`. Use the returned full `scanner:sane:DEVICE` (Linux), `scanner:wia:ID` (Windows), or `scanner:ica:UUID` (macOS) ID. Linux requires SANE `scanimage` and an appropriate backend; Windows requires a WIA driver and Windows PowerShell; macOS uses a bundled ImageCaptureCore helper and compatible driver. `doctor` reports the compiled `scanner` feature and runtime availability. Physical scanner compatibility has not been verified.

```sh
capturefab scanners --json
capturefab --camera 'scanner:sane:DEVICE' --timeout-ms 60000 capture \
  --set ScanResolution=300 --set ScanMode=Gray --set ScanSource=Feeder -n 5 -o pages --json
```

Scanner controls are `ScanResolution` (75–600 dpi, default 150), `ScanMode` (`Color`, `Gray`, `Lineart`, default `Color`) and `ScanSource` (`Default`, `Flatbed`, `Feeder`, default `Default`). Drivers may reject unsupported choices. SANE source strings vary between drivers; `Default` leaves that option unset. `CAPTUREFAB_SCANIMAGE` overrides the executable path. Connecting or `start` never scans; capture requests one page per count, including feeder pages. Use `--timeout-ms 60000` for physical scanning; RPC respects the requested per-page deadline. A timeout cancels the helper. The GUI keeps a separate scanner timeout, initially 60 seconds.

Scans use existing image formats, schedules, destinations and quotas. Multi-page captures save separate files; samples normalize to 8-bit RGB/grayscale with a 128 MiB page bound. PDF/OCR conversion, forwarding and camera auto exposure are not provided for scanner sources. Use full scanner IDs or discover before connecting by serial so the preview ring has its 128 MiB scanner capacity.

## Forward to an existing recorder

```sh
capturefab --camera 192.168.1.10 forward \
  --output rtsp://localhost:8554/camera \
  --encoder auto --fps 30 --bitrate 4M --duration 60
```

A persistent session continues forwarding until stopped:

```sh
capturefab --session gui forward --output rtsp://localhost:8554/camera
capturefab --session gui stop-forward
```

Forwarding probes usable hardware encoders and falls back to software. Destinations include RTSP, SRT, RTMP, UDP, HTTP, and bounded local video files. A destination server or recorder must already exist. Local HLS output is not implemented; an existing server such as MediaMTX can provide HLS from a published stream.

## Save to saved destinations

```sh
capturefab destination list --json
capturefab volumes --json
capturefab --session gui capture -n 10 -o run-1 --destination archive --json
capturefab uploads --json
```

A destination is a named folder, S3-compatible bucket, FTP/FTPS server or SFTP server the user saved; with `--destination NAME`, `-o` is a relative name (file, folder or template) inside it, and the result names the destination and, for remote destinations, `queued_uploads` and `queued:[{file,upload_id,key,url}]`. Direct `--camera` captures to a remote destination wait for their uploads and add `uploads` to the result. `uploads --json` reports `pending`, `uploading`, `failed` (each with `error`), `uploaded` totals, whether an uploader is running, `completed_seq` and up to 20 `recent_completed` entries `{seq,id,destination,kind,path,key,url,bytes,sha256,uploaded_ms}` (the log keeps 1000 in `<session_dir>/uploads-completed.json`). S3 queue items keep their `bucket` field; FTP and SFTP items carry `remote:{kind,...}`. `uploads retry` and `uploads forget ID` act on failures. Ask the user before creating destinations or storing credentials; never pass secrets on the command line. A missing external drive is exit code 3.

```sh
capturefab destination add-ftp qa-ftp --host ftp.plant.local --user camera --dir images --password-stdin --json < password.txt
capturefab destination add-sftp qa-sftp --host files.plant.local --user camera --dir /srv/images \
  --host-key SHA256:... --key-file /home/me/.ssh/id_ed25519 --json
capturefab destination add-s3 nas --bucket line1 --endpoint http://nas.plant.local:9000 --preset rustfs --profile lab --json
capturefab destination test nas --probe-writes --json
```

`add-ftp NAME --host H [--port 21] --user U [--dir D] [--plain] [--password-stdin|--password-env|--anonymous] [--keep-local] [--no-check]` uses explicit FTPS by default; without a password option it reuses a keychain password for `user@host:port`; `--password-env` reads `CAPTUREFAB_FTP_PASSWORD` in the uploading process. `add-sftp NAME --host H [--port 22] --user U [--dir D] (--host-key SHA256:...|--accept-host-key) [--key-file /abs/path] [--password-stdin|--password-env] [--keep-local] [--no-check]` pins the host key; `--accept-host-key` trusts the key presented now, prints its fingerprint to stderr and conflicts with `--no-check`; ask the user to confirm that fingerprint with the server administrator. `--password-env` reads `CAPTUREFAB_SFTP_PASSWORD`; with `--key-file` the password is the key passphrase. `add-s3 ... [--preset minio|rustfs] [--probe-writes] [--check-first]`: a preset needs `--endpoint`, forces path-style addressing and, unless `--no-check` or `--check-first`, probes conditional writes. `destination test NAME --probe-writes` (S3 only) returns `{ok,conditional_writes:enforced|ignored|unsupported,no_overwrite:if_none_match|check_first,saved,probe_removed}` and switches the saved destination to `check_first` when the service does not refuse replacements. FTP/SFTP `test` returns `{ok:true,kind,probe}` after writing and removing a test file.

Destination JSON: FTP `{name,kind:"ftp",host,port,user,dir,tls:"explicit"|"plain",password:"keychain"|"environment"|"none",keep_local}`; SFTP `{name,kind:"sftp",host,port,user,dir,host_key,key_file?,password,keep_local}`; S3 buckets may add `no_overwrite:"check_first"`. List and add output adds `location`, `insecure` and `secret_saved`; add output adds `check {ok}` or `{ok:false,warning}`, and SFTP adds `host_key {fingerprint,trust:"pinned"|"accepted_on_first_use"}`. Locations and URLs never contain the user: `ftps://host:21/dir`, `sftp://host:22/dir` (an absolute folder shows `//`), `s3://bucket/key`. A retryable check failure exits with `cannot reach the server: ...; fix the settings or save with --no-check` (exit 3 when refused or the host name does not resolve). Invalid hosts, users, folders, ports, host keys or relative key files are exit 2; a kind not compiled in is exit 5; a changed SFTP host key is exit 1.

No-overwrite: S3 uses `If-None-Match`, or a HEAD check first in `check_first` mode (a short race window). SFTP writes an exclusive temporary file, then an SFTPv3 rename that refuses an existing target. FTP uploads `.NAME.HASH.part`, checks size, then renames; many servers let a rename replace a file, so keep one writer per FTP folder. FTP servers without SIZE and MLST are refused. Older Capturefab binaries reject upload queues holding FTP or SFTP items.

## Schedule captures and respect storage budgets

```sh
capturefab --session gui capture -n 60 -o timed-frames --delay 5s --interval 1m --json
capturefab storage --json
capturefab storage configure --max-space 2GiB --max-files 1000 --on-full stop
```

Schedules belong to the running session and do not survive its exit. The shared private session directory has a persisted storage ceiling, initially 10 GiB and 10,000 Capturefab files. Per-capture limits can lower that ceiling. Recording reservations count against the same budget and a local recording has a separate maximum file size, initially 512 MiB. A storage-full failure uses exit code 6; inspect the status before requesting more space.

Explicit `--delete-oldest` retention only removes completed Capturefab files whose identity and SHA-256 still match. Existing user files, altered captures, active recordings, and unverified interrupted files are protected. Do not raise the global ceiling or enable deletion without the user's authorization. Use the same `CAPTUREFAB_SESSION_DIR` for processes that should share this budget.

## Install the correct release

Read [`releases.json`](releases.json). Its schema is version 1. Each available asset identifies its operating system, architecture, desktop/headless variant, file name, download URL, SHA-256 checksum, and optional signature/application bundle. An empty releases list is not a published release. Assets marked `available: false` must not be offered as downloads. A release may also link `sbom_url`, a CycloneDX software bill of materials, and its files carry GitHub build-provenance and SBOM attestations (`gh attestation verify FILE --repo capturefab/capturefab`).

Use `uname -m` on Linux and macOS or the operating system's system information on Windows. `x86_64` is x64, `aarch64` is ARM64, and `armv7l` is ARMv7. Apple Silicon uses ARM64. Raspberry Pi and Jetson builds must match the installed OS architecture; a 64-bit processor can run a 32-bit OS. Verify the release checksum before running a downloaded binary.

```sh
# Linux
sha256sum DOWNLOADED_FILE
# macOS
shasum -a 256 DOWNLOADED_FILE
# Windows PowerShell
Get-FileHash DOWNLOADED_FILE -Algorithm SHA256
```

Capturefab source is MIT-licensed. The bundled FFmpeg executable is GPLv3; its source, build configuration, and licensing notices are part of the release provenance. See the repository's source and FFmpeg build guide when producing a new platform binary.
