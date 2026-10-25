# CLI workflows

Use ordinary commands for a quick capture and JSON workflow files for repeatable
setup across cameras, scanners, streams and recordings. Open the desktop to see
commands, acquisition and saved outputs as an agent works:

```sh
capturefab gui --name bench
```

In another terminal:

```sh
capturefab --camera sim:0 capture -o first.jpg --json
capturefab --camera sim:0 get Width Height --json
capturefab --camera sim:0 record -o clip.mkv --duration 30s --json
```

These commands attach to the only active session. A missing camera connects to
that session for inspection, configuration, capture or recording. Existing
`--camera` targets keep each desktop/browser client's local selection; `select ID`
sets the session default for CLI commands without `--camera`. Use
`--session bench` to pin a session, especially when several are running. Use
`serve --name bench` for the same persistent behavior without a window, or
`--direct --camera sim:0` for a temporary worker. `CAPTUREFAB_CAMERA` supplies a
default source; `CAPTUREFAB_SESSION` pins camera/session commands. `--direct`
overrides that environment default and conflicts only with explicit `--session`.
`start`, `auto` and other persistent controls require an already connected camera.

For natural-language requests through the local Apple Intelligence model on a
supported Mac running macOS 26 or later:

```sh
capturefab assistant use apple_foundation_models
capturefab assistant test apple_foundation_models --json
capturefab --session bench assistant ask 'List the connected cameras.' --json
```

Enable Apple Intelligence and let macOS download its model first. This provider
needs no API key or server, processes text on-device, and never falls back to a
remote API. It does not inspect camera images. See [AI capture assistant](assistant.md)
for other providers, plan review and bounded tasks.

## Validate and apply a file

Save this UTF-8 JSON as `workflow.json`:

```json
{
  "version": 1,
  "steps": [
    {"command": {"op": "connect", "camera": "sim:0", "timeout_ms": 2000}},
    {"camera": "sim:0", "command": {"op": "set", "feature": "Width", "value": "640"}},
    {"camera": "sim:0", "command": {"op": "set", "feature": "Height", "value": "480"}},
    {"camera": "sim:0", "command": {
      "op": "capture", "output": "captures/first.png",
      "count": 1, "timeout_ms": 2000, "format": "png"
    }}
  ]
}
```

```sh
capturefab workflow validate workflow.json --json
capturefab workflow apply workflow.json --dry-run --json
capturefab --session bench workflow apply workflow.json --json
```

Validation and dry-run show the resolved, redacted plan without contacting a
camera or requiring a session. Apply uses a persistent GUI or `serve` session;
when there is exactly one, `--session` is optional. Every step passes offline
validation before the first step executes. Device feature access/bounds, named
destinations, encoder availability and output collisions are checked live.

Documents accept only `version` and `steps`; steps accept only optional `camera`
and required `command`. Unknown and duplicate fields fail. The input limit is
1 MiB and the step count is 1–256. `command` is the same tagged JSON command used
by session RPC; inspect `capturefab schema --json` for its fields and limits.
`connect` and `select` use `command.camera` and omit the outer step target.
`discover` and `network_scan` also omit it. Other steps may target an existing
camera, or omit `camera` to use the active selection. Workflow apply rejects a
global `--camera`; unset `CAPTUREFAB_CAMERA` when using workflows.

CLI conveniences are expressed as separate commands: `--set Width=640` becomes
a `set` step; `--auto` becomes an `auto` step. Workflow capture formats are
explicit, durations/timestamps use numeric milliseconds, and storage sizes use
numeric bytes. There is no workflow `wait` step. A `forward` step can carry
`duration_ms` (1 ms to 10 years), the CLI's `--duration`: the session then ends
that recording or stream by itself, as `stop-forward` would, even after Apply
exits.

## Choose source and output paths

| Source or output | Base and behavior |
| --- | --- |
| CLI/RPC `file:relative.mkv`, `sim:replay:relative-folder` source | Calling terminal's directory, including camera arguments and connect/select commands |
| Workflow relative local source | Workflow file's directory, including connect/select commands and outer camera targets |
| CLI/RPC local capture or recording path | Calling terminal's directory, including commands sent to another local session |
| Workflow relative local path | Workflow file's directory; `captures/first.png` above is beside `workflow.json` |
| Workflow read from stdin (`-`) | Calling terminal's directory |
| Absolute local path | Used as supplied |
| Named `destination` | `output` stays a relative name inside the saved folder |
| Forwarding stream URL | Used as supplied, with credentials redacted in plans/status |

Bare existing filenames are recognized as local file sources. Use `file:clip.mkv`
to make a local source explicit when its name could also identify a camera.
Network URLs, device locators and other camera IDs retain their meaning. For
example, a workflow using `sim:replay:frames` reads `frames` beside that workflow
file; the same CLI source reads it from the terminal's current directory.

Use a directory for a frame sequence, or a template such as
`run-1/frame-{frame}.png` or `line1/{date}/{camera}-{datetime}-{seq}.png`.
Templates accept `{camera}`, `{date}`, `{time}`, `{datetime}`, `{seq}`,
`{frame}` and `{tag:KEY}`; `{seq}` picks the next free number, so a rerun saves
new files, and `{tag:KEY}` is the capture's `context` value for KEY as one safe
folder or file name (an empty or missing tag is a usage error).
Captures never overwrite existing files. A fixed filename fails when it exists
unless the command has `"on_exists": "number"`; a directory continues its
`frame-NNNNNN` numbering. A `"context"` object such as `{"line": "L1"}` tags
captures, schedules and recordings. Workflow captures require a file/directory
and cannot send a binary image to stdout. Ordinary CLI captures infer known
single-file/template extensions; directory sequences default to PNG. `--format`
overrides that inference.

A `capture` command completes after its requested images are saved. A `schedule`
command returns a job ID; its frames are acquired later. A `forward` command
starts an encoder and returns while recording/forwarding continues. Keep the
session running and inspect `jobs`/`status`; jobs do not survive session exit.
Stopping acquisition with `stop` cancels pending jobs on that camera.

```sh
capturefab --session bench --camera sim:0 jobs --json
capturefab --session bench status --json
capturefab --session bench --camera sim:0 cancel 1 --json
capturefab --session bench --camera sim:0 stop-forward --json
```

## Start from an example

The [example files](../examples/workflows) validate offline. Run from the repository
root or use their absolute paths. Copy them to your own working directory and
edit camera selectors and output names before applying to hardware.

| File | Workflow |
| --- | --- |
| [single-capture.json](../examples/workflows/single-capture.json) | Configure an area-scan simulator and save one PNG |
| [multi-camera-timelapse.json](../examples/workflows/multi-camera-timelapse.json) | Configure two simulators and schedule 60 frames each at 10-second intervals, with separate outputs and storage limits |
| [scanner-feeder.json](../examples/workflows/scanner-feeder.json) | Configure resolution, grayscale and feeder mode, then save five pages with a 60-second deadline per page |
| [start-recording.json](../examples/workflows/start-recording.json) | Start local H.264 recording with a 64 MiB file cap and a 512 MiB requested storage ceiling |
| [stop-recording.json](../examples/workflows/stop-recording.json) | Finish that recording, then stop its camera acquisition |
| [timed-recording.json](../examples/workflows/timed-recording.json) | Record a numbered H.264 file for one minute; the session ends it, so it needs no stop file |
| [plant-line.json](../examples/workflows/plant-line.json) | Open a dataset, add a note and save one tagged, numbered capture |

```sh
capturefab workflow validate examples/workflows/multi-camera-timelapse.json --json
capturefab --session bench workflow apply examples/workflows/multi-camera-timelapse.json --json
```

Timelapse jobs begin immediately (`first_at_ms: 0`) and run independently on the
two cameras; this does not synchronize their exposures. Their outputs are under
`examples/workflows/captures/` unless you copy the workflow elsewhere. Inspect
each camera's `jobs`; previews can keep running after the last frame.

For feeder scans, replace every `scanner:sane:DEVICE` in the example with the full
ID from `capturefab scanners --json`. Windows IDs use `scanner:wia:ID`; macOS uses
`scanner:ica:UUID`. Actual DPI, mode and feeder availability depend on the driver;
inspect `features` before applying. Connecting does not scan; the capture step
requests the pages. No PDF assembly or OCR is included.

The recording start/stop files are separate because workflow commands execute
without a delay. Applying them consecutively finishes the recording immediately.
Apply the stop file when ready, give the `forward` step a `duration_ms` as
timed-recording.json does, or use a timed CLI recording:

```sh
capturefab --session bench --camera sim:recording record -o video/run-2.mkv --duration 30s --max-file-size 64MiB --json
```

`--duration` works with sessions and direct workers. The CLI sends it as
`duration_ms`, and the camera's worker stops the encoder at `until_ms` (in the
reply, and as `forwarding_until_ms` in `status`), then commits and catalogs the
file. The CLI stays alive until then and reports
`"stopped": {"forwarding": null, "by": "duration"}`; it never stops a recording
that another participant started on that camera afterwards. A recording that
stops early or with a new camera error fails the command with that error. Ctrl-C
stops the recording early. Without a duration, a session recording returns
immediately and continues until stopped; a direct command remains in the
foreground. Acquisition keeps running after a recording ends; stop it separately
when done.

Timed recordings count elapsed time and computer sleep. Moving the system clock
backward does not extend them. Forward replies and active status include
`forwarding_id`, an opaque 32-character lowercase hexadecimal identity. RPC
`stop_forward_if` with that identity refuses a replacement recording. Timed CLI
fallback and assistant cleanup use it; explicit human `stop-forward` still stops
the current recording.

The per-file cap remains enforced regardless of elapsed time. Hitting it stops
recording, removes the partial file and reports a storage error, so size
`--max-file-size` (`max_file_bytes`) for the whole duration: at `4M`, about
36 MiB per minute plus 16 MiB of margin. Encoder support depends on the FFmpeg
build and host; `ffmpeg --json` and `doctor --json` describe availability.

## Catalog captures

Workflow steps can open a camera dataset and add notes without an outer `camera` target:

```json
{"command": {"op": "dataset_open", "path": "plant/line1.duckdb", "mode": "auto"}}
{"command": {"op": "dataset_note", "text": "shift A start", "context": {"line": "L1"}}}
{"command": {"op": "dataset_export", "output": "exports/shift-a", "formats": ["csv"]}}
```

`dataset_open.path` and `dataset_export.output` resolve beside the workflow file. The dataset stays open after Apply; close it with `{"op":"dataset_close"}`.

## Recover and observe

Workflow execution stops on the first error. Earlier settings, saved files and
started jobs/encoders remain; there is no rollback. JSON errors include
`error.details.completed`, `completed_steps`, a one-based `failed_step`, the
failed command/target and its message. Inspect those results plus `status` and
`jobs`, then correct and run the remaining steps. Replaying the whole file can
encounter existing outputs or an already active encoder.

`--timeout-ms` bounds frame/transport work. `--request-timeout 5m` bounds a client's
wait for a session request; it does not cancel work already accepted by the
session. Inspect status before retrying a write after a client timeout. JSON
commands put one versioned success/error envelope on stdout and diagnostics on
stderr; check the exit code before consuming `result`. `schema --json` wraps its
contract in `result`; plain `schema` emits that contract directly.

The GUI toolbar shows active client work and marks new actions in Activity while
preserving open panels and keyboard focus. Activity distinguishes **CLI / API**
from **GUI** and shows the target and queued/running/done/failed states, errors and
saved paths across all cameras, including after selection/disconnect. Captures
and scheduled frames update saved-file feedback. The same timeline is in
`status --json`'s `result.activity`; status polling does not add entries. Journal
strings and saved-path previews are capped: completed captures retain the first
eight paths, `file_count` reports the total, and capture command results still
return the complete file list. Scheduled job previews advance with acquired frames.

Camera-targeted status retains the full coordinator context and queried camera
fields together:

```sh
capturefab --session bench --camera sim:0 status --json
```

`result.target_camera` identifies the queried canonical camera;
`result.active_camera` retains the coordinator's selection for untargeted CLI
commands. `cameras`, `devices`, logs and activity remain available, so an agent can
observe one camera without losing session context or changing client selections.

## Use the same files in PowerShell

File arguments work unchanged across platforms and avoid JSON quoting:

```powershell
capturefab workflow validate .\workflow.json --json
capturefab --session bench workflow apply .\workflow.json --json
```

To supply stdin instead, use:

```powershell
Get-Content -Raw .\workflow.json | capturefab workflow validate - --json
```

The stdin form resolves outputs in the current terminal directory. Shell users
can similarly redirect stdin with `capturefab workflow validate - --json < workflow.json`.

## Resource budgets

Use `capturefab resources --json` for current usage, automatic or chosen policy, effective limits and headroom. These settings apply locally across sessions sharing the same state directory, without connecting a camera.

```sh
capturefab resources configure --cpu-threads 2 --camera-buffers 1GiB
capturefab resources reset
```

`auto` restores one field to its automatic safeguard. CPU threads govern image operations and new codec instances; camera buffers are shared frame-ring allocations. Automatic pressure protection can reduce work below a user ceiling. Use `capturefab storage` for disk quotas and retention.
