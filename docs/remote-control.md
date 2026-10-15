# Remote control over a tailnet

Capturefab's WebAssembly client runs in a laptop's browser and controls a native, headless Capturefab session on another computer. Camera connections, acquisition, scheduled jobs, files and uploads run on that server. The laptop needs a browser and access to the server; it does not need camera drivers or a native Capturefab installation.

The HTTP listener is opt-in. Ordinary `capturefab serve --name bench` keeps its private local IPC interface. With a remote listener, the local CLI and the browser control the same session and cameras.

## Build the server and browser client

Install the browser build tools once, using the same Rust toolchain as the client build:

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.129 --locked
```

Then build from the repository root:

```sh
cargo build --release --no-default-features --features usb,scanner,jpeg
./web-client/build.sh
```

The native build needs the usual [Capturefab build dependencies](../README.md#build-test-and-release) and produces `target/release/capturefab`. Add `s3` to its features if the server will upload to bucket destinations. The browser build produces `web-client/dist`, which must be copied alongside the native executable on the capture server, or supplied with an absolute `--web-dir` path. The server does not compile the client at startup. The pinned `wasm-bindgen-cli` version must match the client's Rust dependency; no Node.js or npm is required.

## Start a server reachable through Tailscale

Install and sign in to Tailscale on the capture server and laptop, then allow the laptop to reach the server's TCP port 8787 in your tailnet policy. On the server, find its Tailscale IPv4 address:

```sh
tailscale ip -4
```

Start Capturefab with the address printed by `tailscale ip -4`, replacing the example below:

```sh
capturefab serve --name remote \
  --listen 100.101.102.103:8787 \
  --web-dir /path/to/web-client/dist
```

The server reports the browser URL when ready. Keep the process running; camera connections and schedules belong to this process. Use your normal service manager for unattended operation.

On the laptop, open `http://100.101.102.103:8787/` to control the session directly. Anyone who can reach the listener can control captures; tailnet policy and the host firewall provide the access boundary. Binding directly to the server's Tailscale address restricts the listener to that interface.

## Set up captures from the browser

Connect a discovered camera, or expand **More connection options** to enter a camera ID, IP address or supported stream URL. Use `sim:0` to try the workflow without hardware. Select the camera and start its stream to see a live preview. Expand **Camera settings** to inspect or edit its advertised controls.

Capture output paths refer to the server's filesystem. A relative path resolves from the server's working directory. A saved destination refers to a folder or bucket configured on the server, and its output is a name inside that destination. The server enforces the same file ownership, never-overwrite, quota and retention rules as native Capturefab.

Use **Save frame** beside the live preview for an immediate capture. Expand **Capture options** to choose a format, frame count and storage limits; **Save frames** starts a background job when the count exceeds one. Expand **Timelapse schedule** to set an interval and optional start time. **Capture jobs** opens when you schedule a sequence. Jobs continue when the browser tab closes or the laptop disconnects. Reopen the page to inspect progress or cancel a job. Stopping the server ends its camera connections and jobs; schedules do not survive a server restart.

Local scripts can keep using the same session:

```sh
capturefab --session remote status --json
capturefab --session remote --camera sim:0 set ExposureTime=5000
capturefab --session remote --camera sim:0 jobs --json
```

Several browser users and AI agents can drive the same cameras at once. Each tab keeps its own camera selection and drafts. The latest shared action stays visible above the preview. Expand **Session activity** for the history, including who submitted each command, its camera, queued/running/succeeded/failed state, saved files and scheduled job progress. Closing or reconnecting a tab does not erase the session history. Change your participant name inside that section to make your work recognizable; CLI agents can use `--actor focus-agent --actor-kind agent`. Same-camera commands run in order, while cameras operate independently and overlapping scheduled jobs share frames.

## Optional HTTPS with Tailscale Serve

For a Tailscale HTTPS URL, bind Capturefab to loopback instead:

```sh
capturefab serve --name remote \
  --listen 127.0.0.1:8787 \
  --web-dir /path/to/web-client/dist
tailscale serve --bg http://127.0.0.1:8787
tailscale serve status
```

Open the HTTPS URL printed by Tailscale to control Capturefab directly. Tailscale Serve provides a tailnet-only reverse proxy with an automatically provisioned TLS certificate; `--bg` preserves its configuration across restarts. See the [Tailscale Serve CLI reference](https://tailscale.com/docs/reference/tailscale-cli/serve) for permissions and setup. Capturefab still needs to remain running, including after host reboots.

## HTTP automation

The client uses same-origin HTTP endpoints without authentication headers. Requests with a browser origin must match the server URL. Serve the client and API together at the root of the same origin.

| Endpoint | Result |
| --- | --- |
| `GET /api/session` | Session and connected camera status |
| `GET /api/activity` | Shared `{revision, activity}` history, including actor and scheduled job progress |
| `GET /api/activity?after=REV&timeout_ms=20000` | Wait for an activity change; all watchers wake when commands or jobs change |
| `POST /api/command` | A native session command result |
| `GET /api/preview?camera=ID` | Latest camera preview as PNG, or HTTP 204 while no frame is available |
| `GET /api/destinations` | Saved server destinations without credentials |
| `GET /api/storage` | Server storage usage and policy |
| `GET`, `HEAD /health` | JSON readiness, uptime, camera counts and analytics health; HTTP 503 when stopping or analytics is unavailable |
| `GET`, `HEAD /metrics` | Prometheus text metrics for session and camera acquisition |
| `GET`, `HEAD /api/analytics` | Current session totals, camera summaries and bounded time series |
| `GET`, `HEAD /api/analytics.svg` | Rust Plotters dashboard with frame rate, throughput, missed frames and camera data volume comparisons |

Command requests wrap an existing `SessionCommand`:

```json
{
  "version": 1,
  "camera": "sim:0",
  "actor": {"id": "focus-agent", "name": "Focus agent", "kind": "agent"},
  "command": {"op": "set", "feature": "ExposureTime", "value": "5000"}
}
```

Omit `camera` for session-wide operations such as `{"op":"connect","camera":"sim:0","timeout_ms":2000}`. JSON responses use `{ "version": 1, "ok": true, "result": ... }`, or `ok: false` with an error message. Run `capturefab schema` for the session command contract. Captures write files on the server; the preview endpoint supplies a bounded display image rather than the original measurement data.

`actor` is optional display attribution, not authentication. Supply a stable `id` (1–128 ASCII letters, digits, `-`, `_`, `.`, `:`), a nonempty `name` of up to 128 UTF-8 bytes without control characters, and `kind` (`human` or `agent`). Legacy HTTP calls without it appear as **HTTP client**. The activity history retains 200 completed entries plus active commands and schedules across camera selection and disconnect. Read-only observations do not create entries. A schedule's command state becomes `succeeded` when accepted; its `job` object carries `id`, `worker_pid`, `status`, `count`, `captured`, `last_file` and `error` as acquisition continues.

Fetch `/api/activity` once, then repeat the waiting request with the returned revision. The server returns immediately when that revision differs from the current one; otherwise it waits up to `timeout_ms` (0–25000, default 20000). After timeout, use the returned revision and wait again. A reconnect can resume with the last revision, including after a server restart; the response always includes the current retained history. Keep watching while your own command runs so other participants' queued work stays visible. `/api/session` also includes `activity` and `activity_revision`; targeted local `status` includes global activity without waiting behind a capture.

Local agents can observe the same immediate changes without enabling HTTP: `capturefab --session NAME activity --json` returns `{revision,activity}`, then `activity --after REV --wait 20s --json` waits for the next change. Each call prints one ordinary versioned response, including on timeout. The equivalent private RPC command is `{"op":"activity","after":12,"timeout_ms":20000}`. This observer bypasses camera queues and does not create a timeline entry. A local IPC session permits eight waiting observers within its 16 connection slots; HTTP permits 32 within 64 connections. Extra waits return an explicit error and can be retried, leaving capacity for commands. Browser waits are shorter with a brief gap between requests to keep commands responsive across several tabs sharing HTTP connections.

## Analytics and monitoring

Raw `logs` in session snapshots retain `time`, `level` and `message` and may add
`timestamp:{unix_ms,utc_offset_seconds}`. Signed Unix milliseconds identify the
absolute event, while the offset describes the producer clock's seconds east of
UTC. The legacy `time` string has no recoverable date/zone. Older entries omit
metadata, and malformed optional metadata leaves the diagnostic readable.
Desktop/browser displays use the viewer's local zone; browser log labels also
include ISO UTC, and copied desktop logs include a dated numeric offset. This
additive metadata preserves the version-1 envelope and original fields.

Analytics collection runs with the session, including when no browser is open and no metrics scraper is connected. DuckDB stores the time series in memory, sampling once per second and retaining at most 24 hours or 250,000 rows. Its engine memory limit is 128 MB, with disk spilling and external access disabled. Camera metadata is capped at 256 entries, and inactive entries expire within 24 hours. Session counters accumulate across stop/start and camera disconnects as constant-size lifetime totals; time-series history expires independently. A new server process starts with fresh analytics. Frame and received-byte totals describe acquisition, while missed-frame metrics expose transport or shared-ring losses; they do not count files written by capture jobs.

Open `/api/analytics.svg` to view or save the dashboard for the current session. The native desktop window's **Analytics** button shows the same measurements with charts that follow the app's appearance, readable camera names and browsable history data. **Show history data** opens a fixed snapshot; **Update history** loads the latest charted report while preserving the page when possible. The dark exported SVG includes an empty state before the first frame and scales its plots to the collected history. The JSON report includes `retention_seconds`, `sample_interval_seconds`, sample counts, session totals, camera summaries and time-series points. Its API and SVG routes enforce the same browser-origin checks as other `/api/` routes.

Monitoring tools can fetch `/health` and `/metrics` directly without a browser origin. `/health` returns a plain JSON object with `status`, `ready`, `version`, `uptime_seconds`, `cameras` and `analytics`. An idle session with no connected cameras is ready. A previous camera command error stays in diagnostics without making readiness fail. `/metrics` uses Prometheus text format with session counters such as `capturefab_session_frames_total` and per-camera counters labeled by `camera_id` and `name`. `HEAD` returns the same status and headers as `GET` with no body; unsupported methods on these four routes return HTTP 405 with `Allow: GET, HEAD`.

```sh
curl http://100.101.102.103:8787/health
curl http://100.101.102.103:8787/metrics
curl http://100.101.102.103:8787/api/analytics
curl -o session-analytics.svg http://100.101.102.103:8787/api/analytics.svg
```

If the laptop cannot load the page, check the listener address, host firewall and tailnet policy. If WASM fails to load, rebuild the client and copy the whole `dist` directory, including its `.wasm` and JavaScript files.
