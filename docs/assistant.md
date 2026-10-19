# AI camera assistant

The optional assistant uses the same checked camera tools as external agents. It can examine frames, adjust imaging settings, capture files and verify the result. Select a provider and model before you send a request. The `ai` build feature supplies the assistant; builds without it return exit 5 for every `assistant` subcommand.

Everything runs on the local computer. The optional AI assistant and prompt measurements send data to the AI provider you choose, only when you send a request. Provider requests use your account and its data terms.

## Set up a provider

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

When no Google Cloud credential is available, Gemini on Vertex can use `GOOGLE_CLOUD_API_KEY`, then the saved `vertex` express key. Claude on Vertex must have Google Cloud credentials, a project and a location; an express API key cannot authorize it. Access tokens are cached until shortly before expiry. Credential status never performs these token-resolution operations.

For Bedrock, the bearer environment token wins. An explicit saved `profile`, `AWS_PROFILE`, or exported AWS credential fields selects SigV4 ahead of a saved Bedrock bearer token. Without such an AWS selection, the saved bearer token precedes the default AWS chain. Within the SigV4 chain, an explicit `--profile` ignores exported access keys; otherwise exported keys precede `AWS_PROFILE`, then the default profile. Partial or invalid exported keys are errors. SSO and `credential_process` profiles are refused with guidance. Region order is settings, `AWS_REGION`, `AWS_DEFAULT_REGION`, then the profile configuration.

## What leaves this computer

Each provider request can contain your prompt, previous conversation text, proposed plan, tool schemas and redacted session details. These details include camera names, models and IDs, discovered devices, destination names and locations, storage use, job file names and recent activity. Tool results include numeric frame measurements. With images enabled, requests also contain the camera previews the assistant examines, including fresh checkpoint images during long tasks.

Capturefab excludes API keys, passwords and secret form fields from model content. Model-facing data removes camera serial numbers and address fields, reduces network URLs to their scheme and host, and replaces the home-directory prefix with `~`. Provider authentication is sent separately to the selected endpoint. Frames are downscaled previews, not a copy of every saved capture. Scene text, file names, device descriptions and logs are data, never instructions.

Choosing a local endpoint changes where the request goes; it does not change the request's contents. The provider's or local server's data terms and logging configuration still apply. The assistant does not have a general shell, arbitrary file reader, raw register writer or unrestricted network tool.

## Plan before changes

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

```sh
capturefab --session bench assistant run \
  'Examine sim:0, improve exposure if necessary, capture two photos, then verify them.' \
  --camera sim:0 --mode autonomous --approvals deny \
  --max-time 10m --max-rounds 15 --max-captures 2 --max-size 128MiB --events task-events.jsonl --json

capturefab --session bench assistant run --prompt-file task.txt \
  --camera sim:0 --camera sim:1 --destination reviewed-folder \
  --see-images --mode supervised --approvals ask --on-human-change pause
```

Run scope pins one to eight explicit camera IDs and the allowed saved destination names. An empty destination list permits the local capture root only. A model-supplied URL, path or destination cannot add to scope. Discover a camera before selecting it for a task. Camera activity is attributed to `assistant:{pid}:{run8}` with actor kind `agent`.

`--see-images` permits previews; omit it for measurements only. Effective `--max-recordings 0` disables file recording. Effective `--max-captures 0` disables captures and schedules. Streams remain separate actions that always need exact human approval. CLI recordings and schedules must end within the task deadline. The CLI never permits effects to outlive the task.

`--on-human-change pause|inform|stop` controls the response to other participants' activity or settings changes. The default is `pause`. Changes are reported to the model at a legal conversation boundary. Even with `inform`, overwriting a human-set value needs Disruptive approval. Camera-native or Capturefab auto adjustments do not count as human edits.

`wait_for` waits for time, frames, settled auto exposure, completed jobs, stopped recordings or activity. It checks cancellation, human changes and wall time at most every 250 ms. Waiting does not consume another model round. A reply without tool calls ends the CLI run as `needs_user`; it does not poll for an answer in the background.

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
| Setting assignments | 300 | 10,000 | Settings file |
| Consecutive tool errors | 5 | 5 | Settings file |
| Identical consecutive calls | 3 | 3 | Settings file |
| Approval timeout | 10 minutes | 1 second to 24 hours; also bounded by remaining wall time | `--approval-timeout` |
| Interval between model requests | 1 second | At least 1 second | Settings file |

Capturefab checks projected usage before each request and reserves tool resources before effects. It accounts for reported usage after the response. At 80 percent, it emits a notice. A reached limit stops new actions and produces a Capturefab-written `limit` report without another model call. Failed operations do not refund their reservations. Storage rules and no-overwrite checks still apply.

Token usage is normalized across protocols, including cache and reasoning fields. When a provider omits usage, estimates use request text and image sizes. Cost estimates use user-entered USD prices per million tokens in `assistant.json`; no prices are supplied by default. Reported OpenRouter cost is used when present. Estimates are not a bill, and `--max-cost` must have known prices for the selected model.

Long conversations roll into new segments at round boundaries. A bounded checkpoint carries the task, scope, remaining limits, settings, experiments, human decisions and notes. Fresh scoped images are included when permitted. Checkpoints do not carry provider thinking or stale tool-call bindings, and the log records their SHA-256.

## Stop, take over and restore

Ctrl-C or SIGTERM stops a CLI task. No new requests or actions start; a late provider reply is discarded. A blocking HTTP request can finish later, and an accepted camera command can finish. An internal live-view restart still attempts its final Start so cancellation does not leave the camera stopped by that internal sequence.

Stop does not undo work. The final report lists remaining recordings, schedules, auto mode and changed settings owned by the task. Examine these before stopping or changing other participants' work. To take over from the CLI, stop the task and explicitly address the camera with normal camera commands.

The runtime's Restore proposal contains baseline assignments only where the same worker is present and its live value still equals the task's last write. It lists human-changed, reconnected and selector-dependent settings for review instead of restoring them. Restoration is a separate human action. External agents can use an acknowledged `camera_change.token` with `undo_camera_change`; a task can only undo within its settings budget and with known history metadata.

## Settings and logs

`<session_dir>/assistant.json` is version 1, at most 64 KiB, written atomically under a lock. On Unix it has mode 0600. It is a regular private file, not a symlink. Invalid entries are ignored with a warning on load. It contains provider selection, non-secret fields, a saved-key hint, autonomy defaults and optional prices:

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

Action logs are private JSONL files under `<session_dir>/assistant/runs/<run_id>.jsonl`. Each record has `v`, `run`, `seq`, `t` (UTC Unix milliseconds) and `kind`. Kinds are `start`, `request`, `text`, `tool`, `observation`, `approval`, `human_change`, `rollover`, `limit`, `stop` and `end`. Logs exclude keys, passwords, raw provider thinking, image bytes and full provider response bodies. Observation records can include image dimensions, byte counts and SHA-256 values. If a dataset is open, state-changing tools also add a dataset note.

`--events FILE` tails the exact action-log records to a new file. `--events -` writes them to stderr, leaving stdout for the result envelope. Logs have a 16 MiB per-run cap; retention keeps at most 100 runs and 256 MiB. A run without an end record can be reported as interrupted on the next open; a currently locked live log is not a crash.

## JSON results and failures

`--json` uses the standard `{"version":1,"ok":true,"result":...}` envelope. `ask` returns `{text,plan:{version,calls},steps:[{step,tool,title,summary,issues,risk,leaves_computer}],applied?}`. `run` returns `{run_id,outcome,summary,totals,leftovers,log}`. `providers`, `runs` and `log` return arrays. Plan export remains a version-1 tool document, separate from the envelope.

Stopped and limited runs exit 1 with `ok:false`; their error details retain the run result. Ask failures retain the proposed plan in error details when available. An Ask apply failure keeps `text`, `plan` and `steps`, plus `applied:{completed,failed_step,step_count}` for execution failures or `applied:{issues,checked}` for recheck failures. Other exits are 1 operation failure, 2 refused configuration or arguments, 3 unavailable, 4 timeout, 5 unsupported build, and 6 storage full. Scrubbed provider text is available only in `error.details.provider_message`. Read the exit status and completed effects before retrying.

## External agents

Claude Code, Codex and other agents can call `capturefab tools list|call|apply` without configuring an in-process provider. See [Let an agent drive cameras](../web/agents.md#let-an-agent-drive-cameras). Use `--actor NAME --actor-kind agent` for each external-agent call. The assistant runtime uses its own agent actor regardless of the caller's display name. Actor metadata provides attribution, not authentication.

Pass `tools --confirmed-by-user` only after the user approved that exact Disruptive or HumanOnly action. Never use that flag as a blanket permission. Tool images are private ephemeral diagnostics by default; explicit `--image-out` saves through the storage ledger. Read tool schemas from the installed catalog rather than inventing argument names.
