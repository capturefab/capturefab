# Plant-floor data capture

This guide is for manufacturing and process engineers who must collect images,
video, sensor values and PLC tags on a line, then give the data to an analysis
tool later: a notebook, a BI tool, or an AI assistant such as Claude, ChatGPT or
Gemini. Everything runs on one computer on the OT network. No cloud service is
necessary.

The result of a session is one DuckDB dataset file and the image and video files
it catalogs. Each capture row records the camera, the time, the file path, its
SHA-256 and the tags you typed (line, station, part, lot, note). Signal samples go
into the same file, so each image can be matched to the sensor and PLC values at
that moment. An export writes Parquet or CSV files, a `manifest.json` and a
`README.md` that an analysis tool can read without more explanation.

Try every step with the simulators first: `sim:0` is a pattern camera and
`sim-signal://line1` is a simulated line with temperature, pressure, a counter,
a running flag, a state and vibration. Capturefab is under rapid development;
these steps do not claim compatibility with a specific camera, PLC, broker or
server.

## 1. Capture without naming every file

The desktop app saves to `~/Capturefab/{date}/{camera}-{datetime}-{seq}.png` by
default, and records to the same pattern with `.mp4`. Each capture gets a new
name, so you can push **Capture & save** (Cmd/Ctrl-S) or **Record**
(Cmd/Ctrl-Shift-R) again and again. The Capture panel shows the next name as
**Next:**. **Show in Finder** (Explorer on Windows, folder on Linux) opens the
last saved file. In the overview, **Capture all** and **Record all** act on
every connected camera; each camera gets its own file.

From a terminal, use the same placeholders:

| Placeholder | Value |
| --- | --- |
| `{camera}` | `model-serial`, limited to `A-Z a-z 0-9 . _ -` and 64 bytes; never the address |
| `{date}` | `YYYY-MM-DD` |
| `{time}` | `HHMMSS` |
| `{datetime}` | `YYYYMMDD-HHMMSS` |
| `{seq}` | Next free number for that folder and pattern, 4 or more digits |
| `{frame}` | 1-based index in a multi-frame request, 6 digits |

Times are the computer's local time when the frame is requested. `{seq}` can
occur once, only in the file name, and not directly before a digit or another
time placeholder. Other names in braces are a usage error (exit 2).

```sh
capturefab --camera sim:0 capture -o 'line1/{date}/{camera}-{seq}.png'
capturefab --camera sim:0 capture -o shot.png --on-exists number   # shot-0001.png, shot-0002.png, ...
capturefab --camera sim:0 record -o 'line1/{camera}-{datetime}.mp4' --duration 30s
```

Files are never overwritten. Without a placeholder, an existing name fails
unless you add `--on-exists number`. A directory output continues
`frame-NNNNNN` after the highest existing number.

## 2. Tag what you capture

Context tags tell the analysis what each image shows. In the desktop app, open
**Capture tags** in the Capture panel and fill in **Line**, **Station**,
**Part**, **Lot** and **Note**. They are sent with every capture and recording
as the context keys `line`, `station`, `part`, `lot` and `note`.

```sh
capturefab --camera sim:0 capture -o 'line1/{camera}-{seq}.png' \
  --context line=L1 --context station=S2 --context part=A-100
```

Keys match `^[a-z][a-z0-9_]{0,31}$`. A capture accepts at most 16 keys and 256
bytes for each value. A duplicate key is a usage error.

## 3. Read sensors and PLCs

Each data source is one URI. Read it once to make sure the address and tags
are correct:

```sh
capturefab signals read sim-signal://line1
capturefab signals read 'modbus-tcp://10.0.0.20/1/holding/0?type=f32&name=temp_c&unit=degC'
capturefab signals browse opc.tcp://10.0.0.30:4840 --duration 5s
capturefab signals read 'opc.tcp://10.0.0.30:4840?node=ns=2;s=Line1.Temp'
capturefab signals browse 'mqtt://broker.local/plant/line1/#' --duration 10s
capturefab signals read mtconnect://10.0.0.40:5000/Mill1
```

| Protocol | URI |
| --- | --- |
| Modbus TCP | `modbus-tcp://HOST[:502]/UNIT/coil\|discrete\|input\|holding/ADDRESS[?count=N&type=u16\|i16\|u32\|i32\|f32\|f64\|bool&order=abcd\|cdab\|badc\|dcba&scale=F&offset=F&name=TAG&unit=UNIT]` |
| MQTT | `mqtt[s]://[USER[:PASS]@]BROKER[:1883\|8883]/TOPIC_FILTER[?client_id=ID&qos=0\|1]` |
| OPC UA | `opc.tcp://[USER[:PASS]@]HOST[:4840][/PATH]?node=NODE_ID[&node=...][&root=NODE_ID]` |
| MTConnect | `mtconnect[s]://HOST[:5000\|443][/DEVICE]` |
| Simulator | `sim-signal://LINE[?tags=1..64]` |

Modbus addresses are 0-based protocol addresses: Modicon `40001` is
`holding/0`. A value is `raw * scale + offset`. MQTT accepts numbers,
`true`/`false`, JSON objects (flattened to `topic/field`) and text. OPC UA uses
SecurityPolicy None only, so a password travels unencrypted; the helper prints a
warning. Percent-encode `@` in topics and node IDs, and `/ ? # @` in
credentials. Capturefab removes credentials from every output.

Each source runs in its own helper process. A failed source restarts at most
five times and does not stop the cameras.

## 4. Log signals and captures into one dataset

For a fixed run, `log` writes samples until the duration ends or you push
Ctrl-C:

```sh
capturefab log sim-signal://line1 --interval 1s --duration 10m \
  --dataset plant.duckdb --csv line1.csv
```

For a shift with cameras and signals together, start a session and keep it
running:

```sh
capturefab serve --name line1 --dataset plant.duckdb
# In another terminal:
capturefab --session line1 signals start sim-signal://line1 --interval 1s
capturefab --session line1 --camera sim:0 capture \
  -o 'line1/{date}/{camera}-{datetime}-{seq}.png' --context part=A-100
capturefab --session line1 --camera sim:0 capture --interval 1m -n 480 \
  -o 'line1/{date}/{camera}-{seq}.png' --context line=L1
capturefab --session line1 dataset note "shift B start" --context line=L1
capturefab --session line1 signals
```

The desktop app does the same thing. **Live signals** in the Capture panel has
an **Add data source** field and shows each tag with a short trend line. The
sidebar lists running **Data sources** with a stop button. The **Dataset**
section shows the open file. With **Catalog captures automatically** (on by
default), the app creates `~/Capturefab/capturefab.duckdb` on the first saved
file or signal sample.

A dataset file is limited to 8 GiB by default (`--max-bytes`). Writes stop with
`Storage full` (exit 6) when the disk has less than 512 MiB free. Captures still
obey the shared storage budget.

## 5. Look at the data

```sh
capturefab --session line1 dataset status
capturefab --session line1 dataset query \
  "SELECT path, context, tag, value_double, sample_age_ms FROM captures_with_signals LIMIT 20"
# After the session closes the file, or for a copy:
capturefab dataset query --file plant.duckdb "SELECT count(*) FROM samples"
```

Queries are one read-only `SELECT` or `WITH` statement. They cannot read or
write other files. A query returns at most 10,000 rows and 8 MiB of JSON, and
stops after 30 s by default. While a session holds the file, `--file` fails
with exit 3; use `--session NAME` instead.

`captures_with_signals` joins each capture with the latest sample of every tag
at or before the capture time. `sample_age_ms` tells you how old that sample
was.

## 6. Export for analysis

```sh
capturefab --session line1 dataset export -o exports/line1-shift-b
capturefab dataset export --file plant.duckdb -o exports/line1-csv --format csv \
  --since 2026-10-17T06:00:00-04:00 --until 2026-10-17T14:00:00-04:00
```

The export folder must not exist. It contains:

| File | Content |
| --- | --- |
| `captures.*` | One row per saved image: time, camera, file, SHA-256, destination, context |
| `recordings.*` | One row per finished local recording |
| `signals.*` | One row per tag: source, redacted URI, unit, kind |
| `samples.*` | Every logged sample |
| `events.*` | Notes, dataset open and close, data gaps and signal errors |
| `captures_with_signals.*` | Each capture with the latest value of every tag |
| `captures_wide.*` | One row per capture, one column per tag (1 to 256 tags) |
| `manifest.json` | Columns with types and descriptions, file SHA-256 values, cameras, tag statistics |
| `README.md` | Time base, file list, pandas, DuckDB and polars examples, and a summary for AI tools |

Every `*_ms` column is UTC Unix milliseconds from the capturing computer.
`context` is JSON text. To ask an AI assistant about a run, give it the
`README.md`, `manifest.json` and the CSV or Parquet files it can read, plus the
images you want it to examine. The images stay at the paths in `captures.path`.

Default formats are Parquet and CSV; `--format json,jsonl` is also available.
An export that does not fit the storage budget fails before it writes a file.

## 7. Copy files to a server

Captures and recordings can go to a saved destination. Files are first written
locally, then a queue that survives reboots uploads them and never replaces a
remote file.

```sh
# FTPS (explicit TLS); the password is kept in the OS credential store
printf '%s' "$PASSWORD" | capturefab destination add-ftp qa-ftp \
  --host ftp.plant.local --user camera --dir images --password-stdin
# SFTP with a pinned host key
capturefab destination add-sftp qa-sftp --host files.plant.local --user camera \
  --dir /srv/images --host-key SHA256:... --key-file /home/me/.ssh/id_ed25519
# RustFS or MinIO on the plant network
capturefab destination add-s3 nas --bucket line1 --endpoint http://nas.plant.local:9000 \
  --preset rustfs --access-key-id KEY --secret-stdin < secret.txt

capturefab --camera sim:0 capture --destination qa-sftp -o '{date}/{camera}-{seq}.png'
capturefab uploads --json
```

A queued upload's ID, remote key and URL are stored in the capture row. See the
[README](../README.md#save-to-drives-folders-s3-ftp-or-sftp) for the
no-overwrite rules of each server type.

## 8. Repeat the setup with a workflow file

[plant-line.json](../examples/workflows/plant-line.json) opens a dataset, starts
`sim-signal://line1`, connects `sim:0`, adds a note and saves one tagged
capture. Paths are relative to the workflow file.

```sh
capturefab workflow validate examples/workflows/plant-line.json --json
capturefab serve --name line1
# In another terminal:
capturefab --session line1 workflow apply examples/workflows/plant-line.json --json
```

Signal sources and the dataset stay open in the session after the workflow
ends. Stop them with `signals stop all` and `dataset close`.
