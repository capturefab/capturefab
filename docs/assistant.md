# AI capture assistant

The optional assistant uses the same checked capture tools as external agents. It can examine frames, adjust imaging settings, capture files, acquire sensor values and query or export a dataset. Select a provider and model before you send a request. The `ai` build feature supplies the assistant; builds without it return exit 5 for every `assistant` subcommand.

Everything runs on the local computer. The optional AI assistant and prompt measurements send data to the AI provider you choose, only when you send a request. Provider requests use your account and its data terms.

## Set up a provider

In the desktop app, open Settings and expand **AI assistant**. Choose a provider,
model and any connection fields it needs. Valid settings apply automatically.
Save key stores the secure draft in the OS credential store; Test connection
sends one small tool round trip without camera information or images. Provider
rows show credential-presence hints until the selected provider has been checked.
Its summary then reflects cached credential or connection results. Opening the
section checks credentials only for the selected provider.

OpenAI, Gemini and Claude keys are shared with the separate **Prompt measurement
keys** section. Closing Settings clears unsaved key drafts.

```sh
capturefab assistant providers --json
capturefab assistant use anthropic --model claude-sonnet-5-5 --effort low --vision auto
capturefab assistant key set anthropic --key-stdin < /private/path/provider-key.txt
capturefab assistant test anthropic --json
capturefab assistant test anthropic --vision --json
```

Keep the input key file private and remove it when it is no longer necessary. `key set` reads one key from stdin into the operating system credential store. Never put a key in a prompt, command argument, URL or plan file. `assistant key remove anthropic` removes that provider's saved key. A nonempty environment key still takes precedence after removal.

`providers` lists the catalog, selected model, credential status and vision decision. It does not read the keychain unless you add `--check-keychain`. Status does not run `gcloud`, contact a metadata server or make a provider request. `doctor --json` reports environment presence only. `test` sends a tool round trip; `test --vision` also sends a solid-colour 64×64 JPEG. The probe sends no camera images or session details.

These are Capturefab's catalog entries and configured defaults. They are not provider compatibility or camera testing claims. Use `assistant providers --json` and `schema --json` for the installed build's values.

| Provider ID | Key environment variables, in order | Additional settings |
| --- | --- | --- |
| `openai` | `OPENAI_API_KEY` | Optional model |
| `anthropic` | `ANTHROPIC_API_KEY` | Optional model |
| `gemini` | `GOOGLE_API_KEY`, `GEMINI_API_KEY` | Optional model |
| `vertex` | Google Cloud credentials; `GOOGLE_CLOUD_API_KEY` for express mode | `project`, `location`; see below |
| `bedrock` | `AWS_BEARER_TOKEN_BEDROCK`; AWS credential chain | `region`, optional `profile` |
| `azure` | `AZURE_OPENAI_API_KEY` | `endpoint`; model is the deployment name |
| `openrouter` | `OPENROUTER_API_KEY` | Optional model |
| `groq` | `GROQ_API_KEY` | Optional model |
| `deepseek` | `DEEPSEEK_API_KEY` | Optional model |
| `cloudflare` | `CLOUDFLARE_API_TOKEN` | `account_id`, optional `gateway_id` |
| `mistral` | `MISTRAL_API_KEY` | Optional model |
| `xai` | `XAI_API_KEY` | Optional model |
| `together` | `TOGETHER_API_KEY` | Optional model |
| `fireworks` | `FIREWORKS_API_KEY` | Optional model |
| `cerebras` | `CEREBRAS_API_KEY` | Optional model |
| `huggingface` | `HF_TOKEN` | Optional model |
| `moonshot` | `MOONSHOT_API_KEY` | Optional `base_url` for the allowed regional host |
| `dashscope` | `DASHSCOPE_API_KEY` | Optional `base_url` for the allowed regional host |
| `perplexity` | `PERPLEXITY_API_KEY` | Optional model |
| `ollama` | `OLLAMA_API_KEY` only for `ollama.com` | Optional `base_url`; local servers use no key |
| `lmstudio` | Optional `LM_API_TOKEN` | Model; optional `base_url` |
| `custom` | Optional `CAPTUREFAB_AI_API_KEY` | Model and `base_url` |

`assistant use PROVIDER` accepts `--model`, `--base-url`, `--account-id`, `--gateway-id`, `--region`, `--profile`, `--project`, `--location`, `--endpoint`, `--effort low|medium|high` and `--vision auto|on|off`. Fields apply only to the relevant provider. Environment defaults include `CLOUDFLARE_ACCOUNT_ID`, `AZURE_OPENAI_ENDPOINT`, `GOOGLE_CLOUD_PROJECT`, `GOOGLE_CLOUD_LOCATION`, `AWS_REGION`, `AWS_DEFAULT_REGION`, `AWS_PROFILE` and `OLLAMA_HOST`. Stored settings take precedence over environment defaults.

```sh
capturefab assistant use cloudflare --account-id 0123456789abcdef0123456789abcdef --vision off
capturefab assistant use azure --endpoint https://example.openai.azure.com --model camera-deployment
capturefab assistant use vertex --project plant-project --location global
capturefab assistant use bedrock --region us-east-1 --profile plant
capturefab assistant use ollama --base-url http://127.0.0.1:11434/v1 --model qwen3.8
capturefab assistant use custom --base-url http://127.0.0.1:8080/v1 --model local-camera-model --vision off
```

Cloud endpoints use HTTPS and provider-specific host restrictions. Custom, Ollama and LM Studio can use plain HTTP on loopback or private addresses. Capturefab never sends a key over plain HTTP to a non-loopback host. Plain HTTP and loopback requests bypass environment proxies. A configured custom endpoint can receive a saved key, so review its host before selecting it.

## Credentials and precedence

For ordinary API-key providers, the first nonempty provider environment variable wins, followed by that provider's saved key. A provider never borrows another provider's environment key. An invalid nonempty key is an error, not a reason to use another credential.

The OS credential-store service is `capturefab-ai`. Accounts use provider IDs except Anthropic uses `claude`; OpenAI and Gemini use `openai` and `gemini`. These three accounts are shared with [prompt measurements](video-analytics.md). Other provider keys remain separate. Capturefab does not store keys in `assistant.json` or ordinary preferences. A locked or unavailable credential store produces an error.

For Vertex AI, Capturefab first resolves Google Cloud credentials: the explicit `GOOGLE_APPLICATION_CREDENTIALS` ADC file, the well-known gcloud ADC file, `gcloud` access-token fallback, then eligible Google Compute Engine metadata. An invalid explicit ADC file is an error. Supported ADC types are `service_account` and `authorized_user`; executable, URL and other external-account credential sources are refused. OAuth token exchange uses Google's fixed token endpoint. Metadata access uses the literal link-local address, only on an eligible machine, with no proxy.

When no Google Cloud credential is available, Gemini on Vertex can use `GOOGLE_CLOUD_API_KEY`, then the saved `vertex` express key. Claude on Vertex must have Google Cloud credentials, a project and a location; an express API key cannot authorize it. Access tokens are cached until shortly before expiry. Vertex ADC model requests recheck that cache, including during long tasks. Renewal keeps the endpoint and conversation pinned and never switches an existing ADC session to an express key. Credential status never performs these token-resolution operations.

For Bedrock, the bearer environment token wins. An explicit saved `profile`, `AWS_PROFILE`, or exported AWS credential fields selects SigV4 ahead of a saved Bedrock bearer token. Without such an AWS selection, the saved bearer token precedes the default AWS chain. Within the SigV4 chain, an explicit `--profile` ignores exported access keys; otherwise exported keys precede `AWS_PROFILE`, then the default profile. Partial or invalid exported keys are errors. SSO and `credential_process` profiles are refused with guidance. Region order is settings, `AWS_REGION`, `AWS_DEFAULT_REGION`, then the profile configuration.

## What leaves this computer

Each provider request can contain your prompt, previous conversation text, proposed plan, tool schemas and redacted session details. These details include camera names, models and IDs, discovered devices, destination names and locations, storage use, job file names and recent activity. Tool results include numeric frame measurements. With images enabled, requests also contain the camera previews the assistant examines, including fresh checkpoint images during long tasks.

Data tasks require separate permission. **Allow source values and dataset results** in task setup, or `--allow-data` on the CLI, permits selected source tag names, values, units, quality, timestamps, dataset notes and query results to reach the provider. Choosing a data source does not grant image permission. Source locations are redacted; executable source endpoints and credentials stay local.

Capturefab excludes API keys, passwords and secret form fields from model content. Model-facing camera metadata removes serial and address fields, reduces network URLs to their scheme and host, and replaces the home-directory prefix with `~`. Dataset SQL receives the same projection before it can transform these metadata fields. Consented sensor values, notes and file names remain data. Provider authentication is sent separately to the selected endpoint. Frames are downscaled previews, not a copy of every saved capture. Scene text, file names, device descriptions and logs are data, never instructions.

Choosing a local endpoint changes where the request goes; it does not change the request's contents. The provider's or local server's data terms and logging configuration still apply. The assistant does not have a general shell, arbitrary file reader, raw register writer or unrestricted network tool.

## Plan before changes

Open **Quick Command…** from the desktop menu, sparkle toolbar button or welcome
screen. The shortcut is **⌘K** on macOS and **Ctrl+Shift+K** on Windows/Linux.
Type a request and press Return. Plan first lets the assistant run observations;
other tools become editable, checked step cards. Select **See camera images**
only when you want inspected previews sent to the provider.
Thumbnails show the pixels from those exact resized preview bytes.

Check the camera, destination, file name, before/after values, warnings and any
**Leaves this computer** marker. Fill required fields, including separate secure
secrets when needed. **Run plan** executes the displayed revision after a fresh
check. A changed action stops execution for another review. Return commits field
edits and never runs the plan. After a partial failure, completed steps stay in
the transcript and **Run remaining steps** keeps the failed and later steps.

Closing with the close button, Escape or the shortcut keeps the conversation and
active work. **Stop** cancels a model turn and restores its earlier plan, or stops
plan execution after the accepted command finishes. Closing clears unsaved
secrets. **New conversation** clears the plan and history. **Export plan…** writes
a private plan file and offers a `capturefab tools apply --dry-run` command using
the app's executable and current workbench selection. The command uses the reviewed
output folder for relative file names and restores the terminal's working directory
afterward. It clears inherited camera and session overrides that could select a
different target. Inspect the checks before removing `--dry-run` to run approved
actions. Exported
anonymous sensor steps contain their currently approved, credential-free source
endpoints so another CLI process can check them; the in-app plan keeps its opaque
references. Removed or changed bindings block export. Protected source plans must
run in the app, or be reviewed again after binding the saved profile locally in
the target workbench; account epochs and credentials are never exported.

```sh
capturefab serve --name bench
# In another terminal:
capturefab --session bench connect sim:0
capturefab --session bench assistant ask 'Examine sim:0 and plan one verified photo.' --plan-out camera-plan.json --json
capturefab --session bench tools apply camera-plan.json --dry-run --json
capturefab --session bench tools apply camera-plan.json --json
```

`ask` runs Observe tools and proposes other calls as plan steps. It does not apply those steps by default. It can attach to the only active session; with several sessions, select `--session NAME`. Without a session, local read-only context remains available, but camera operations cannot run and `--apply` is refused.

Supply a prompt as positional text or `--prompt-file FILE|-`, never both. Prompts accept at most 4,000 characters. `--provider ID` and `--model NAME` override the selected provider and model for that request. Credential-bearing URLs and recognized secret patterns are refused before a request is sent.

When the plan has steps, `--plan-out FILE` creates a private version-1 tool plan without overwriting. Tool plans hold 1–64 action calls and at most 256 KiB; an empty plan cannot be exported. They reject Observe calls, unknown fields and duplicate fields. Every tool argument key must be present; optional values use `null`. For example:

```json
{"version":1,"calls":[{"tool":"capture_photos","args":{"camera":"sim:0","count":1,"format":"png","file_name":"verified-{seq}.png","destination":null,"tags":null}}]}
```

`ask --apply` is an explicit request to execute a restricted plan. Every step must be ready, every tool must be in the allowlist, and no step may send data off the computer. The default allowlist is `connect_camera,start_recording,stop_recording,capture_photos,schedule_captures,cancel_job`. `--allow-tools LIST` replaces it; it never permits egress. A refused plan exits 2 before execution. Review `error.details` for its plan and issues. Apply rechecks live state and stops at its first failure; earlier effects remain.

## Modes and approvals

| Mode | Observe | Reversible and Bounded | Disruptive | HumanOnly |
| --- | --- | --- | --- | --- |
| Plan first (`ask`) | Runs reads | Adds plan steps | Adds flagged steps | Adds flagged steps |
| Ask before each change (`run --mode supervised`) | Runs reads | Asks | Asks | Asks once |
| Run on its own (`run --mode autonomous`) | Runs reads | Runs within scope and limits | Asks | Asks once |

Observe reads state. Reversible actions change camera state. Bounded actions acquire a fixed amount of data. Disruptive actions can interrupt work or send video elsewhere. HumanOnly actions create destinations or need a person's direct decision. Live circumstances can raise an action's risk. Only changes with an acknowledged history token can use camera Undo.

The assistant always asks before a Disruptive or HumanOnly action. This includes streaming to a URL, creating a folder or S3 destination, starting physical scanner acquisition, stopping another participant's work, restarting busy acquisition, and overwriting a value a person changed during the task. Unsafe transport, firmware, persistent-network, user-set and raw-register operations are refused by the camera tool registry.

An approval is for the displayed action. The runtime rechecks its camera, destination, output, risk and other facts before execution. Approval for a task covers the same tool, camera and risk, with the same egress target; HumanOnly approval is always once. A human change or replacement camera worker invalidates earlier grants. Denied or unanswered actions return an error to the model. Three denials or timeouts end the task as `needs_user`.

Run defaults to supervised mode. Approval handling defaults to `ask` when stdin and stderr are TTYs, otherwise `deny`. `--approvals ask` displays prompts on stderr and must have a TTY. `--approvals deny` denies proposals that need approval. A denial does not authorize a different URL, destination or unsafe operation.

## Run a bounded camera task

In Quick Command, select **Ask before each change** or **Run on its own**.
Choose the cameras and allowed destinations, describe the goal, review the
Short, Standard or Long limits, and select **Start task**. Image permission is
off for a new task. The setup shows the selected model's vision capability;
numeric measurements remain available without images. Modes, scope and consent
are fixed for the running task. A new conversation returns to **Plan first**.

Approvals show Capturefab's checked action facts and expire if unanswered.
Secret fields belong only to that exact action. **Approve for this task** cannot
extend the chosen cameras, destinations or egress target. Activity and Changes
show progress and before/after values; the latest-view thumbnail shows the
preview actually sent to the model. Budget meters turn amber at 80% and red
at the limit. Cost is an estimate when prices are known; otherwise tokens are
shown.

**Stop**, **Pause/Resume**, and **Take over** remain available while working.
Take over stops the task and waits for its current command and owned-data flush to finish. It
selects the task's camera, or opens manual Data sources for a data-only task. The camera
picker's Data sources action is available whenever no camera is shown in the window.
Human camera controls stay enabled. Messages reach the assistant at the next round. Closing
the sheet keeps the task running; the toolbar shows progress, Stop and pending
approval. Quitting asks whether to keep running or stop and quit. After a task,
review remaining recordings, schedules and auto modes, or review a settings
restore before applying it. Restore skips reconnects and settings changed since
the assistant's last write. Selector-dependent settings require inspector
review. Export log writes a private, redacted JSONL file. An unfinished task
found on the next open offers its log and never resumes automatically.

```sh
capturefab --session bench assistant run \
  'Examine sim:0, improve exposure if necessary, capture two photos, then verify them.' \
  --camera sim:0 --mode autonomous --approvals deny \
  --max-time 10m --max-rounds 15 --max-captures 2 --max-size 128MiB --events task-events.jsonl --json

capturefab --session bench assistant run --prompt-file task.txt \
  --camera sim:0 --camera sim:1 --destination reviewed-folder \
  --see-images --mode supervised --approvals ask --on-human-change pause
```

Run scope pins up to eight explicit camera IDs, approved data sources, an optional open dataset instance and allowed saved destination names. Select at least one camera, source or dataset; a data-only task needs no camera. An empty destination list permits the local capture root only. A model-supplied URL, path or destination cannot add to scope. Discover a camera before selecting it for a task. Capture activity is attributed to `assistant:{pid}:{run8}` with actor kind `agent`.

`--see-images` permits previews; omit it for measurements only. Effective `--max-recordings 0` disables file recording. Effective `--max-captures 0` disables captures and schedules. Streams remain separate actions that always need exact human approval. CLI recordings and schedules must end within the task deadline. The CLI never permits effects to outlive the task.

`--on-human-change pause|inform|stop` controls the response to other participants' activity or settings changes. The default is `pause`. Changes are reported to the model at a legal conversation boundary. Even with `inform`, overwriting a human-set value needs Disruptive approval. Camera-native or Capturefab auto adjustments do not count as human edits.

`wait_for` waits for time, frames, settled auto exposure, completed jobs, stopped recordings or activity. It checks cancellation, human changes and wall time at most every 250 ms. Waiting does not consume another model round. A reply without tool calls ends the CLI run as `needs_user`; it does not poll for an answer in the background.

## Sensor data and mixed captures

In task setup, select cached sources or add credential-free source endpoints, select an existing dataset when needed, and enable data permission. On the CLI, repeat `--source URI` to approve sources. `--dataset SLOT_ID` selects the exact open instance reported by `tools call get_dataset --args '{}' --json`; it is not a database filename. The assistant can instead use `open_dataset` to create a catalog inside the capture folder. Close the current dataset before selecting a different file. Closing the shared dataset or stopping another participant's signal capture requires approval.

In **Plan first**, open **Sources…** and add a credential-free endpoint, then enable **Read sensor values and dataset rows**. The endpoint stays local; the provider receives an opaque reference and redacted location. Adding a source does not contact it: fresh reads, browsing and captures become reviewable plan steps and wait for **Run plan**. Bindings survive context refreshes and follow-up requests in that conversation; removing a binding or changing data consent resets provider history and invalidates the reviewed plan revision. **New conversation** clears bindings and consent. Cached source references remain available for observations with data permission and never become executable endpoints without a local binding. Task setup also supports explicit sources for supervised or autonomous runs.

For authenticated MQTT or OPC UA sources, use the saved-source editor in **Sources…**, task setup or the manual Data sources inspector. Enter the endpoint without credentials, then choose a local username and either the OS credential store or a `CAPTUREFAB_SIGNAL_` password environment reference. **Save profile** keeps credentials locally; **Use source** explicitly resolves them in the selected workbench. A password is never read back into the editor. Choosing a source for an assistant does not contact its endpoint; the resulting network action still needs review. In manual controls, **Start logging** binds the profile and starts the source. OPC UA credentials currently use the protocol's existing security policy; review the editor's transport warning.

The CLI exposes the same flow with `signals profiles list|save|remove|bind`. `save --credentials-stdin` reads a JSON object with `username` and `password` from protected stdin and saves the password in the OS store. Add `--password-env CAPTUREFAB_SIGNAL_NAME` and omit `password` from that JSON to use the workbench's environment instead. Keep credentials out of shell command arguments and history. Updating or removing a profile requires its exact listed revision. For a bounded assistant task, repeat `--source-profile NAME --allow-data` to bind saved profiles explicitly; external harnesses can run `signals profiles bind NAME`, then use its opaque reference with data tools.

Bindings belong to one workbench session, saved-profile revision and credential epoch. Editing a profile, changing its password or restarting the workbench requires **Use source** again. Existing tasks cannot adopt a replacement account at the same endpoint. Checking a cached source in task setup pins the account binding immediately; an account change before Start requires reselection. Cached selections do not grant a fresh connection.

```sh
capturefab --session bench assistant run \
  'Create mixed.duckdb with a 16 MiB limit. Log 12 values from the approved sensor at 100ms intervals, for at most 5 seconds and 8 KiB of sample payload. Wait for final persistence, photograph sim:0, query captures_with_signals, and export a new mixed-bundle folder with a 1 MiB limit.' \
  --camera sim:0 --source sim-signal://line1 --allow-data \
  --mode autonomous --approvals deny --max-sources 1 --max-samples 12 \
  --max-signal-time 5s --max-data-size 3MiB --max-captures 1 --json
```

Omit `--camera` for a data-only task. Fresh network reads and browses require approval; the simulator can run within the displayed bounds. `list_data_sources` uses cached status and never connects. Other source tools use references returned by that observation, bound to the endpoints approved during setup.

`start_signal_capture` requires interval, duration, sample and serialized-payload byte limits. The coordinator enforces those limits after the client disconnects. `wait_for_data` with `samples:null` waits for completion and final flush; check `persistence_complete`, `written`, `dropped`, `helper_dropped` and errors before using the dataset. A stopped helper alone does not prove its samples were saved. Cleanup uses the acknowledged capture identity and preserves a person's replacement of the same source.

Queries accept one read-only SELECT, return at most 1,000 rows and 16 KiB including metadata, and mark omitted rows with `truncated` and `truncation_reason`. Before built-in assistant SQL runs, Capturefab copies the six standard tables into a private snapshot. Stored camera serials become null, URLs lose private credentials and paths, and the current home-directory prefix becomes `~`; SQL aliases and transforms cannot recover the originals. Consented sensor values, tags, notes and file names remain data. The complete snapshot is limited to 100,000 rows, 16 MiB of Arrow transfers and 8 MiB per batch. The same timeout covers setup, copying and SQL. Large or modified-schema datasets fail explicitly; use local CLI analysis for those datasets. Builds without the `parquet` feature cannot provide this model-query boundary and refuse the operation. Direct trusted CLI dataset queries retain local catalog contents.

Exports write selected tables and an inclusive time range to a new folder, with manifest, loading examples and checksums. CSV, Parquet, JSON and JSON Lines are available when compiled; Parquet and JSON require the `parquet` feature. The complete generated export bundle, including its staged data and metadata, must fit its declared byte limit. The dataset remains open for review after a successful task.

Signal byte limits count serialized sample payload. Each export reserves **twice its bundle limit** for staged files and published copies. Both reservations consume `--max-data-size` and `--max-size`; failed attempts retain their charges. For example, a 16 MiB bundle needs a 32 MiB reservation. The model receives `remaining.export_bundle_bytes`, the largest bundle allowed by the remaining reservations. This covers combined logical file sizes during export publication, including retained staged files; it does not cap physical DuckDB/WAL growth, shared query scratch, filesystem overhead or ledger files. Dataset size checks and the session's separate scratch policy still apply; full per-task storage isolation remains an implementation limitation.

For native sensor-only tasks, **Separate dataset for this task** creates an isolated catalog with
the logical file extent cap shown in setup. Its full reservation remains charged after completion.
The option is available on supported Unix builds with Parquet; mixed image/data tasks and external
capture tools use the shared dataset workflow above.

Use **Retained datasets…** in Quick Command to review previous work, including after starting a new
task or restarting Capturefab. **Show in Finder/folder** reveals the original workspace. **Save a copy…**
writes `task.duckdb` and any `task.duckdb.wal` into a new folder, charged separately to the storage
budget. Keep those files together and open the copied database for analysis or crash recovery.
Copying does not modify the retained original. **Remove…** asks for confirmation before permanently
deleting that workspace; its reservation is released only after anchored deletion is acknowledged.
An active owner, changed directory, extra hard links or uncertain close blocks removal and keeps
the reservation visible. All inventory, copy and removal work runs off the UI thread.

External tools can recover orphan workspaces through the local CLI:

```sh
capturefab storage task-data list --json
capturefab storage task-data copy RESERVATION_ID recovered-data --json
# Only after the user approves deleting this exact workspace:
capturefab storage task-data remove RESERVATION_ID --confirmed-by-user --json
```

Use the reservation ID returned by the inventory. A live session retains ownership; close its task
and use the GUI's retained-data controls, or exit the owning session before CLI recovery. Paths,
action logs and actor labels cannot authorize removal. A failed copy can leave a partial output
folder; inspect it before retrying with a new folder name.

## Images and vision

CLI `ask` uses numeric measurements only. Run sends camera previews only with `--see-images`; the provider's `--vision` setting controls support, not consent. `auto` uses capability metadata when available, then the built-in model table; unknown capability stays off. `on` forces image use and `off` disables it. Capability results are cached in memory for 24 hours. If a provider rejects an image request, Capturefab reports the change and continues with measurements only.

Previews are JPEG or PNG, at most 1.5 MiB each and at most 1,568 pixels per dimension. The usual request budget is eight images and 8 MiB of encoded image data. Capturefab's catalog uses three images for Groq, two for Cerebras, and 30 images / 9 MB for Fireworks. Long tasks start a fresh segment before request limits are exceeded; they do not remove images from an already sent conversation.

## Hard limits and cost

| Resource | Default | Ceiling | CLI override |
| --- | --- | --- | --- |
| Model rounds | 40 | 1,000 | `--max-rounds` |
| Wall time | 30 minutes | 1 minute to 24 hours | `--max-time` |
| Input tokens, including cache use | 3,000,000 | 50,000,000 | `--max-input-tokens` |
| Output tokens, including reasoning | 150,000 | 5,000,000 | `--max-output-tokens` |
| Cost | Unset | US$500, only with known prices | `--max-cost` |
| New preview images | 60 | 2,000 | `--max-images` |
| Captured files, including schedules | 100 | 100,000 | `--max-captures` |
| Data reservation | 2 GiB | Remaining storage budget and free disk | `--max-size` |
| File recordings | 2 | 32 | `--max-recordings` |
| Each recording or stream duration | 30 minutes | 7 days; CLI also enforces the task deadline | `--max-recording` |
| Source capture attempts | 8 | 256 | `--max-sources` |
| Reserved signal samples | 1,000,000 | 100,000,000 | `--max-samples` |
| Signal payload and export peak reservations | 512 MiB | Remaining storage budget; exports reserve twice their bundle limit | `--max-data-size` |
| Cumulative signal capture duration | 30 minutes | 7 days; CLI also enforces the task deadline | `--max-signal-time` |
| Setting assignments | 300 | 10,000 | Settings file |
| Consecutive tool errors | 5 | 5 | Settings file |
| Identical consecutive calls | 3 | 3 | Settings file |
| Approval timeout | 10 minutes | 1 second to 24 hours; also bounded by remaining wall time | `--approval-timeout` |
| Interval between model requests | 1 second | At least 1 second | Settings file |

Wall time includes credential lookup, provider retries and approval waits. Capturefab checks projected usage before each request and reserves tool resources before effects. It accounts for reported usage after the response. At 80 percent, it emits a notice. A reached limit stops new actions and produces a Capturefab-written `limit` report without another model call. Failed operations do not refund their reservations. Storage rules and no-overwrite checks still apply.

Token usage is normalized across protocols, including cache and reasoning fields. When a provider omits usage, estimates use request text and image sizes. Cost estimates use user-entered USD prices per million tokens in `assistant.json`; no prices are supplied by default. Reported OpenRouter cost is used when present. Estimates are not a bill, and `--max-cost` must have known prices for the selected model.

Long conversations roll into new segments at round boundaries. A bounded checkpoint carries the task, scope, remaining limits, settings, experiments, human decisions and notes. Fresh scoped images are included when permitted. Checkpoints do not carry provider thinking or stale tool-call bindings, and the log records their SHA-256.

## Stop, take over and restore

Ctrl-C or SIGTERM stops a CLI task. No new requests or actions start; a late provider reply is discarded. A blocking HTTP request or OS credential lookup can finish later; late results are discarded. An accepted camera command can finish. An internal live-view restart still attempts its final Start so cancellation does not leave the camera stopped by that internal sequence.

Stop does not undo work. The final report lists remaining recordings, schedules, auto mode and changed settings owned by the task. Examine these before stopping or changing other participants' work. To take over from the CLI, stop the task and explicitly address the camera with normal camera commands.

The runtime's Restore proposal contains baseline assignments only where the same worker is present and its live value still equals the task's last write. It lists human-changed, reconnected and selector-dependent settings for review instead of restoring them. Restoration is a separate human action. External agents can use an acknowledged `camera_change.token` with `undo_camera_change`; a task can only undo within its settings budget and with known history metadata.

## Settings and logs

Assistant settings and action logs live in the persistent state directory reported by `doctor --json`; `CAPTUREFAB_SESSION_DIR` overrides it together with the runtime directory. Existing Linux settings migrate from the old session directory on first use; inactive action logs move without overwriting an existing run, while active logs stay readable in their original directory until their writer closes.

`<state_dir>/assistant.json` is version 1, at most 64 KiB, written atomically under a lock. On Unix it has mode 0600. It is a regular private file, not a symlink. Invalid entries are ignored with a warning on load. It contains provider selection, non-secret fields, a saved-key hint, autonomy defaults and optional prices:

```json
{
  "version": 1,
  "provider": "anthropic",
  "providers": {
    "anthropic": {"model":"claude-sonnet-5-5","effort":"low","vision":"auto","saved_key":true}
  },
  "autonomy": {"see_images":false,"mode":"plan"},
  "prices": {}
}
```

The `saved_key` flag is a hint, not proof that a key exists. Optional provider fields are `model`, `base_url`, `account_id`, `gateway_id`, `region`, `profile`, `project`, `location`, `endpoint`, `effort` and `vision`. `autonomy.limits` uses the millisecond-based field names listed in `schema --json`. The `prices` map uses `provider/model` keys and `input`, `output`, `cache_read`, `cache_write` values.

```sh
capturefab assistant runs --json
capturefab assistant log RUN_ID --json
```

Action logs are private JSONL files under `<state_dir>/assistant/runs/<run_id>.jsonl`. Each record has `v`, `run`, `seq`, `t` (UTC Unix milliseconds) and `kind`. Kinds are `start`, `request`, `text`, `tool`, `observation`, `approval`, `human_change`, `rollover`, `limit`, `stop`, `task_data` and `end`. Logs exclude keys, passwords, raw provider thinking, image bytes and full provider response bodies. Observation records can include image dimensions, byte counts and SHA-256 values. Native separate-dataset receipts record local recovery paths, reserved bytes and acknowledged shutdown state; these receipts do not go to the provider. A quit record may show closure still pending. If a dataset is open, state-changing tools also add a dataset note.

`--events FILE` tails the exact action-log records to a new file. `--events -` writes them to stderr, leaving stdout for the result envelope. Logs have a 16 MiB per-run cap; retention keeps at most 100 runs and 256 MiB. A run without an end record can be reported as interrupted on the next open; a currently locked live log is not a crash.

## JSON results and failures

`--json` uses the standard `{"version":1,"ok":true,"result":...}` envelope. `ask` returns `{text,plan:{version,calls},steps:[{step,tool,title,summary,issues,risk,leaves_computer}],applied?}`. `run` returns `{run_id,outcome,summary,totals,leftovers,log}`. `providers`, `runs` and `log` return arrays. Plan export remains a version-1 tool document, separate from the envelope.

Stopped and limited runs exit 1 with `ok:false`; their error details retain the run result. Ask failures retain the proposed plan in error details when available. An Ask apply failure keeps `text`, `plan` and `steps`, plus `applied:{completed,failed_step,step_count}` for execution failures or `applied:{issues,checked}` for recheck failures. Other exits are 1 operation failure, 2 refused configuration or arguments, 3 unavailable, 4 timeout, 5 unsupported build, and 6 storage full. Scrubbed provider text is available only in `error.details.provider_message`. Read the exit status and completed effects before retrying.

## External agents

Claude Code, Codex and other agents can call `capturefab tools list|call|apply` without configuring an in-process provider. See [Let an agent drive cameras](../web/agents.md#let-an-agent-drive-cameras). Use `--actor NAME --actor-kind agent` for each external-agent call. The assistant runtime uses its own agent actor regardless of the caller's display name. Actor metadata provides attribution, not authentication.

Pass `tools --confirmed-by-user` only after the user approved that exact Disruptive or HumanOnly action. Never use that flag as a blanket permission. Tool images are private ephemeral diagnostics by default; explicit `--image-out` saves through the storage ledger. Read tool schemas from the installed catalog rather than inventing argument names.
