# Agentic capture experience: implementation notes

Status: **both agent routes are implemented and simulator-verified**. Current routes and verification
are recorded below. Native sensor-only tasks also support separate datasets with retained-data
inventory, copy, reveal and confirmed removal, including recovery after restart.

The current completion target is:

> Finish implementing first class support for agentic capture using the built in LLM integration
> as well as external tools like Codex and Claude Code calling the CLI.

The earlier experience objective remains context:

> Optimize the UI/UX for agentic use with occasional manual intervention. The UI should be optimized
> for letting either an external harness (Claude Code, Codex, Antigravity etc) drive captures via CLI
> or via the harness and natural language UI that ships in the app (API keys for llm providers) and
> the setup steps, web site, and user interface should provide a world class agentic image and data
> capture experience.

Image capture, data capture, both agent routes, setup, website and manual intervention remain in scope.
Entry-point and camera improvements alone do not establish completion of that objective.

## Current implementation and evidence

| Requirement | Current evidence | Remaining boundary |
| --- | --- | --- |
| External harness discovery and shared capture | [CLI tools](../src/cli/tools.rs): compact index, targeted descriptions, strict schemas, checked calls, actor attribution; [data tools](../src/tools/data.rs) share the coordinator with the built-in assistant. | Installed builds must expose their compiled protocol/format capabilities; recipes and simulator tests cover both routes. |
| Visible session handoff | [Agent setup](../src/gui/agents.rs), welcome and Help menu provide executable/session briefing, distinct-actor guidance, approvals and environment troubleshooting. | Native screenshots reviewed; retain platform quoting, accessibility and session isolation coverage. |
| Built-in provider setup | [Settings](../src/gui/assistant/settings.rs) and [worker](../src/gui/assistant/settings_worker.rs): field validation, draft-key save before test, credential status, connection success and return to Quick Command. | Live provider requests require suitable credentials; profile-only Bedrock region validation now has isolated worker tests. |
| Natural-language image/data acquisition | [Shared registry](../src/tools/mod.rs), [plans](../src/assistant/agent.rs), [runtime](../src/assistant/run.rs), task approvals, independent image/data consent, source scope and reserved allowances. | A scripted model verifies execution paths; real model judgment and real devices need separate evidence. |
| Occasional manual intervention | Activity/status, actor labels, Stop, Pause/Resume, Take over, camera restore, source/dataset identity checks, change tracking and leftovers; see [assistant guide](assistant.md). | Data/mixed render, keyboard and intervention tests are included in the supported-workflow audit below. |
| Website and setup alignment | [Homepage](../web/index.html), [setup guide](../web/docs.html), [agent instructions](../web/agents.md), [README](../README.md). | Keep commands, output bases and installed-build fallbacks synchronized. |
| Sensor/data workflows | Shared source browse/read/capture/wait tools, checked dataset open/close/query/note/export, opaque instance identities, finite coordinator capture limits and capped generated export files; see [plant-floor guide](plant-floor.md). | Serialized payload allowances do not bound physical DuckDB/WAL growth or query scratch files per task. |

Earlier camera/onboarding baseline recorded on 2026-10-19: 803 GUI tests; 11 default-feature tools CLI integration tests;
85 Python 3.14 script tests; all-feature/all-target strict Clippy; formatting, site and search checks
passed. Eight native screenshots were reviewed. These checks establish their covered behavior, not the
then-pending data-assistant requirements. Six headless CLI tool unit tests and strict headless Clippy also passed.

The rebuilt debug binary passed an isolated `serve --name bench` recipe with temporary state/cache
and working directories: compact discovery of 24 tools; exact full-catalog equality for descriptions
of connect/inspect/capture; connect `sim:0`; fresh inspection with verified preview SHA-256 and numeric
exposure; save `first-0001.png` and `first-0002.png`, both valid 640×480 PNGs; confirm succeeded
`capture-agent` / `agent` activity and no scheduled jobs. Temporary session/workers/files were cleaned.
This did not test a model's semantic image assessment, physical cameras or real provider requests.
The compact JSON response measured 3,177 bytes versus 25,009 bytes for the full catalog (87.3% smaller).

Data workflow verification recorded on 2026-10-19:

- Dataset foundation: `cargo test --lib dataset:: --no-default-features --features parquet --offline`
  passed 23 tests. Coverage includes stale slot identities, read-only bounded query replies, close
  cancellation, all four capped export formats, independent CSV/Parquet reads, checksums, selected
  tables and inclusive time filters. Strict headless all-target Clippy passed for that foundation.
- Real mixed assistant route:
  `assistant_run_mixed_signal_and_image_capture_queries_and_exports_verified_data` passed through
  the rebuilt CLI, a real `serve` session, camera worker and signal helper. A scripted provider called
  overview, open, bounded signal capture, wait, photo, query, export and finish. The test verified 12
  persisted samples across six tags, six `captures_with_signals` rows, a decoded 640×480 PNG and its
  catalog hash, every exported Parquet file/checksum, bundle byte totals, exact dataset/source
  identities, reserved task limits, agent activity attribution and no active leftovers or jobs.
- Real data-only route:
  `assistant_run_data_only_scope_persists_samples_without_camera_actions` passed with no selected
  cameras, zero allowed images/captures, six persisted samples and a verified query. Its source was
  terminal with complete persistence and no leftovers.
- `assistant_data_task_requires_consent_before_contacting_model_or_source` passed: missing data
  permission returned exit 2, the local provider received zero requests, no source was registered
  and no dataset opened.

The three CLI cases ran individually using `cargo test --test cli TEST_NAME --offline -- --nocapture`
with default features and local IPC permission. They use finite model responses from a loopback
fixture and establish execution and reporting behavior. Model judgment, real provider requests and
physical industrial devices require their own evidence. Later integration changes still require the
repository's full checks; these targeted results do not substitute for that final verification.

## Earlier data regression evidence

The complete default-feature library suite passed **2,126 tests**, with 20 existing opt-in tests
ignored, using `cargo test --lib --offline -- --test-threads 4`. A concurrent rendering/compilation
run had nine unchanged media-fixture startup timeouts; an isolated media rerun passed 50 tests,
and the complete four-thread rerun passed without modifying timeouts or assertions.

Strict all-feature/all-target Clippy, strict headless Parquet Clippy, formatting, 85 Python script
tests, static site validation, documentation search and site interaction checks passed. The assistant
GUI subset passed 112 tests before the final source-binding work; the complete rerun now includes
119 passing assistant GUI tests. Coverage includes data-only/mixed controls, invalidated dataset
scope, final persistence reporting, string-list editing/focus, local source bindings and the compact
empty-state fix. A later source-aware suggestion adjustment passed all eight Quick layout tests
and strict all-feature/all-target Clippy. Four directory-anchor tests cover parent and final-folder replacement. The final
assistant CLI suite passed all 17 cases, including real data-only and mixed workflows, refusal
before contact without consent, local plan application, cancellation and partial-failure reporting.
The earlier 11 tools CLI cases also passed.

The shared registry contains **36 tools**. The rebuilt binary's compact catalog measured 4,746 bytes
versus 34,202 bytes for the full catalog (86.1% smaller). Human source approvals retain the exact
credential-redacted locator and opaque reference; model facts still redact URI paths. Opening an
already-open dataset refuses a request that would falsely claim a different size policy.

A later consent audit reproduced two history leaks before fixing them: retained plan arguments
could resend sensor-derived text after data permission was revoked, and a vision fallback could
drop the conversation's consent lineage. The 33 assistant agent/plan tests now pass, including
same-provider and changed-provider revocation, restored permission, data-derived camera tags and
vision fallback. Revocation withholds plan arguments and checked details from provider requests;
the reviewed local plan and its stable step identities remain available.

The query audit also reproduced a persistent sequence mutation through `SELECT nextval(...)`:
rolling back a normal transaction did not restore the sequence. Queries now start an engine-enforced
read-only transaction. All 25 dataset tests pass, including scalar/vectorized sequence refusal,
unchanged sequence state and ordinary writer operations after a refused query. Source binding,
approval resources, dataset receipt adoption, metadata and final-report serialization were reviewed
without finding another concrete scope or credential leak. Private audit logs may retain consented
action arguments/results; query observation row bodies are excluded.

Export reservations now cover two copies against both byte allowances and remain charged after a
failed attempt. The real mixed CLI test verifies the initial provider-visible allowance and exact
later decrements after sample capture and export. Large observations retain only typed numeric
remaining-limit metadata when their ordinary result is truncated; extra fields are refused.

Local review artifacts are generated under `target/agentic-data-native` and
`target/agentic-data-forms`. The 39-image native gallery records the exact binary SHA-256 and per-image
hashes. The separate form gallery preserves six real iced-widget renders from passing layout and
keyboard tests. Synthetic display states and simulator execution are identified in their manifests.

## Implemented data-tool surface

| Tool or extension | Current behavior |
| --- | --- |
| Overview and `list_data_sources` | Dataset identity/counts/limits, protocol capabilities, cached source values/units/quality/timestamps, persistence, dropped samples and sink errors; usable opaque references and truncation markers. |
| `browse_data_source` / `read_data_source` | Bounded initial connection using an explicitly approved source reference; checked actions when contacting a new endpoint. |
| `start_signal_capture` / `stop_signal_capture` | Finite interval/duration/sample/serialized-payload limits, writable sink, acknowledged instance token and conditional stop; never implicit `all`. |
| `open_dataset` / `close_dataset` | Open/create an allowed dataset or close the exact approved slot; account for shared signal sink and active readers. |
| `query_dataset` / `annotate_dataset` | Table/column discovery, bounded read-only SELECT and timestamped context notes against the scoped dataset. |
| `export_dataset` | Selected tables/time range/formats to a new allowed folder, returning row counts, paths, manifest and checksums without overwrite. |
| `wait_for_data` | Wait for a persisted count or terminal completion and final flush, with capture identity, cancellation, loss and persistence checks. |

## Runtime seams and remaining limits

[SessionCommand](../src/session.rs), the [dataset coordinator](../src/dataset/coordinator.rs) and
[SignalHub](../src/signals/hub.rs) own execution. [Op and PlanState](../src/tools/exec.rs) support
camera-free coordinator calls and sequential dataset/source binding. Fresh source reads and browses
are checked actions; cached observations and approved read-only queries remain observations. Scope
accepts camera-only, data-only and mixed tasks, and rejects empty scope. Model-visible opaque source
references bind to locally approved endpoints; redacted URLs cannot become substitute selectors.

Source captures have opaque instance identities, coordinator deadlines and finite sample/payload
allowances. Helpers may restart while retaining the capture identity; replacements receive another
identity. Conditional stop and sink-generation/slot checks preserve another participant's work.
Dataset `slot_id` changes on every close/reopen, independently of persistent `dataset_id`.
Checked notes, queries, exports and sample writes compare that token under the writer lock. Closing
interrupts readers and prevents new readers from attaching during the close. Approval resource
bindings, change tracking, cleanup and leftovers use the acknowledged identities.

[Query replies](../src/dataset/query.rs) count the complete JSON response, including columns and
truncation markers. Assistant queries request 16 KiB; ordinary dataset queries allow up to 8 MiB.
Read-only session SQL parsing needs the `parquet` feature; unsupported builds report that capability.
[Bounded exports](../src/dataset/export/bounded.rs) stream CSV, Parquet, JSON or JSON Lines through
a capped writer, including metadata, before creating the final folder. Parquet uses the Arrow version
shared with DuckDB; the capped Rust writer is uncompressed and adds no native codec dependency.

[Limits](../src/assistant/limits_defaults.rs) and [ToolImpact](../src/assistant/limits.rs) reserve source
attempts, duration, samples, serialized signal payload and complete export bytes. These allowances
also consume the task's data reservation. They do **not** establish a per-task physical storage ceiling:

- DuckDB inserts and WAL/checkpoint allocation can grow before a write returns. Comparing file sizes
  afterward, or changing the shared dataset policy, cannot guarantee that a task never exceeds its
  allowance. Dataset-wide checks and disk floors remain separate from serialized payload limits.
- DuckDB scratch/spill uses the session's separate 1 GiB policy. The bundled engine exposes
  `temp_directory` and `max_temp_directory_size` as global settings, and the sandbox configuration is
  locked. Changing them for one reader would affect other work. Per-task scratch isolation needs an
  appropriate storage or reader design; it is not implemented by the generated-file export cap.
- Checked exports reserve twice their complete bundle limit for staged files and published copies.
  Both the total and data allowances charge this maximum cumulatively, including failed attempts
  and retained staging files. The command's bundle limit remains unchanged;
  `remaining.export_bundle_bytes` is half the smaller remaining byte allowance. This bounds
  generated export file extents during publication; database/scratch/ledger files and filesystem
  allocation overhead remain outside that export reservation.

These are disclosed boundaries of the supported shared-dataset workflow. Its limits bound
acquisition payloads and generated export files, and do not promise an OS-level per-task disk quota.
Setup, website, UI, intervention and workflow evidence is indexed in the audit below.

Plan-first now has a local **Sources…** control for explicit credential-free endpoint bindings.
The provider uses opaque references and redacted locations; new browse/read/start operations are
checked plan steps and wait for **Run plan**. Bindings survive context refresh and follow-up turns.
Source removal or data-consent changes reset provider history and invalidate the reviewed revision;
**New conversation** clears bindings. Cached observations never supply an executable endpoint.
Targeted verification passed 36 agent/privacy tests, 19 Quick state tests, eight Quick layout tests
and seven screenshot-matrix tests. Both 480×620 light/dark source panels were rendered and inspected.
Saved source profiles now support explicit local credential binding for MQTT and OPC UA. A common
protected editor serves Plan first, bounded tasks and manual logging. Metadata listing never resolves
credentials. Save uses a private CAS registry and immutable OS-store revisions; Use resolves locally
and returns a coordinator/profile-revision/credential-epoch receipt. Helper launch and restart check
the original epoch and current material before stdin transfer, without an anonymous fallback.
Usernames, passwords and environment names stay out of model DTOs; cached values require the same
binding that produced them. The CLI exposes `signals profiles list|save|remove|bind` and explicit
`assistant run --source-profile NAME --allow-data`. These profile and assistant setup paths reject credential-bearing endpoints; protected
stdin JSON is bounded, zeroized and never echoed. Changing a binding invalidates task grants and
uses the existing pause/inform/stop human-change policy. New native auth scenes cover all three entry
points. The full default library passes 2,175 tests (20 existing ignored tests), and all 18 assistant
CLI workflows pass. Strict Clippy passes for all features/targets and headless Parquet all-targets.
The actual-helper MQTT integration verifies protected stdin delivery, no credential leakage in
argv/environment/status/log/model output, echoed-value scrubbing, and refused restart after
password rotation. All 85 Python script tests and static site/search/interaction checks pass.
The 32-image native gallery was recaptured and visually reviewed, including the actual Capture
inspector at compact and regular sizes. All per-image hashes match its manifest; the reviewed
binary SHA-256 is `b8995c832546e93b8c0786632993cd96c6121372f269053efa4164d96cdd5ec4`.
Eleven scene regressions and strict all-feature/all-target Clippy also pass after the manual
fixture correction. These artifacts establish this checkpoint; subsequent runtime and UI work
requires fresh verification.

## Manual data handoff and account selection

The sidebar now opens manual Data sources before the first camera or source is connected. Data-only
Take over waits for runtime completion and owned-data flush, then opens those controls; camera tasks
retain camera selection. Focused tests exercise the real sidebar action, native endpoint focus and
the completion gate. Screenshot boot preserves an explicit zero-camera count, and two matrix scenes
cover a true camera-free workbench.

Task setup pins a cached source's account/credential epoch when its checkbox is selected. Refreshing
context cannot adopt another account at the same source reference. Start requires reselection after
an account change, and the runtime rechecks the pinned identity before provider setup. These
cached-only bindings carry no executable URI, stay out of serialized scope/logs, and cannot collide
with a fresh source binding. The complete default library passed 2,202 tests (20 existing ignored
tests), including portable query projection, file-URL decoding and cached-account regressions.

The manual-control audit also found that Stop used a reusable source ID and saving status relied on
an attached sink. Stop now targets the capture token rendered with the control. The inspector shows
written and dropped samples, helper drops and restarts, the last sample time and sink failures;
a running helper cannot imply healthy saving. Runtime tests cover source, sink and dataset changes
under pause, inform and stop policies, plus grant invalidation before another read.

All 21 assistant CLI workflows pass. The new cases verify model-query metadata projection before
SQL, refusals when a scripted provider repeats instructions stored as dataset values, and an actual
SIGKILL of the assistant while the coordinator completes and flushes its finite capture. Unfinished
logs remain discoverable without inventing a completion or resuming work. These tests establish
the structured role boundary and runtime refusals, not a live model's resistance to every prompt
injection. Strict Clippy passes for all features/targets and headless Parquet all-targets.
The 36-image native gallery was recaptured and reviewed with binary SHA-256
`2cfe2867913e1f2fc9dfa6a34494c236a59e6e9565dd97a6d8c27476699db6fa`; every image matches its
manifest hash. Four scenes use zero cameras. Compact and regular entry scenes show both the
endpoint and protected-profile controls above the toolbar, and persistence-failure scenes keep
loss and saving errors visible while acquisition remains running. This supersedes the earlier
32-image gallery; the six form renders remain separate historical evidence.

## Portable plan handoff and source completion

Local plan export now replaces anonymous source references with their exact currently approved,
credential-free endpoints in a separate version-1 document. The in-app/model plan stays unchanged.
Missing, changed, ambiguous, cached-only and protected bindings refuse export; protected account
epochs never become portable connection authority. Tests export a ready source plan, parse it in
a fresh CLI context and check the exact endpoint without contacting a source. Removing the binding
blocks the next export.

The copied preview command uses the running executable, actual session, absolute export path and
reviewed capture folder, with `--dry-run`. Working-directory changes are scoped. Cached command
context handles relative or empty custom session directories, clears conflicting inherited camera
and session overrides, and quotes POSIX and PowerShell arguments. Session values use the attached
`--session=...` form, including valid names beginning with a dash. Native control-to-clipboard tests
exercise the actual copy action. A real Unix shell fixture verifies literal arguments, environment,
the reviewed output root, restored caller directory and no execution after a failed directory change.
PowerShell has source and exact-string tests; no Windows runtime validation was available.

Signal history refresh now derives from current inspector visibility, Capture tab and disclosure
state. Hiding or switching away stops copying historical vectors immediately; latest values,
counters, source setup and saving status continue refreshing. The event regression changes tabs,
inspector state, image mode and disclosure without rendering Live signals again.

Deterministic reader gates reproduced two completion races. A bounded reader could apply a buffered
drop notice after successful final flush, leaving `persistence_complete=true` with changed loss
counters. An unbounded reader delayed beyond Stop's join wait could enqueue another sample after
the stop receipt and flush, when the helper's credential scrubber might already be gone. All source
admission now closes under the same mutex before helper teardown; buffered values, metadata and
restart bookkeeping cannot enter afterward. Completed capture errors remain immutable. Already
admitted batches still flush. Four regressions cover these boundaries and preserve admitted helper
drop failures. All 18 hub tests, the real RPC lifecycle and protected MQTT helper integration pass.

The complete default library passes 2,220 tests with 20 existing opt-in tests ignored, and all 21
assistant CLI workflows pass. Strict all-feature/all-target and headless Parquet all-target Clippy
checks pass. All 85 Python script tests pass; the screenshot matrix contains 135 base cases and 170
combined cases. The final 39-image native gallery was recaptured and reviewed with binary SHA-256
`9d99cb5be139a66b760bb7f12eb9ad964a0e56a720f80d914362784e4369bc31`; every image matches its
manifest hash. Three new exported-plan scenes cover both regular appearances and the compact
window at the largest interface size. The compact command view is explicitly scrolled; Copy and
the pinned task controls remain reachable. Four scenes retain zero connected cameras. This
supersedes the 36-image gallery without changing the separate historical form renders.

## Next storage isolation work

The shared DuckDB instance cannot enforce a task-specific disk budget through a per-reader setting.
The engine has no maximum-database-size setting, and its spill configuration is global. A complete
solution needs task-owned storage and a capped filesystem adapter covering extending database,
WAL and spill writes. Queries against shared catalogs need an isolated reader and a consistent,
bounded scan interface; changing the shared engine configuration would affect other participants.
Specify logical file growth separately from filesystem block and metadata overhead. A size check
after writing or a payload reservation is not a substitute for that enforcement.

The bundled engine is DuckDB 1.5.6 (`069cc9f9b5`). Its C API exposes filesystem use, but not custom
filesystem registration. The version-pinned C++ `DBConfig.file_system` interface and
`DEP_DUCKDB_INCLUDE` support the new isolated `TaskDataset` adapter, which links against the actual
bundled engine and wraps anchored local handles inside `VirtualFileSystem` while preserving
`IsLocalFileSystem()`. A shared lease reserves combined logical EOF growth before extending DB,
WAL, checkpoint/recovery and spill files. Native handles stay wrapped through unlink, rename and
close; uncertain failures retain their charges. Fourteen focused headless tests pass, including
real database/WAL growth refusal, real sort spill, two engines sharing one lease, concurrent
extensions, partial/uncertain writes/truncates/closes, open-unlinked handles, recovery renames,
symlink/hardlink refusal, root replacement and poisoned accounting. The complete affected dataset
suite passes 38 tests. This evidence comes from macOS; it does not establish Linux behavior. The
Windows task API fails explicitly before creating a namespace, without changing existing supported
product paths. Windows static-link declarations, exception flags and platform guards have only
received a source audit; no Windows SDK or runtime validation was available. The adapter assumes a
fresh private namespace with no external same-user native
mutation; attaching an existing namespace would require accounting reconciliation first.

`TaskDataset` also copies the six allowlisted base tables from one shared-engine read-only snapshot
through fallible Arrow stepping. Row, transfer-byte, batch-byte, time and cancellation bounds abort
the entire target transaction. Source-slot closure cancels the transfer. ASOF views and arbitrary
model SQL run only in the isolated target. Tests cover snapshot consistency during a concurrent
source commit, rollback on each limit, substituted source views, target extent refusal and query
cancellation. Source scans still use the shared buffer manager, so spill indirectly caused there by
memory pressure remains outside this task lease's guarantee.

The built-in assistant's `query_dataset` now uses `dataset_model_query_if`, bound to the exact open
slot. A separate portable engine copies all six standard tables from one source transaction,
validating every table's canonical SQL and exact column metadata before scanning any row. This
rejects substituted views, generated columns, changed defaults and user macro shadowing. Projection
nulls stored camera serials and normalizes URI, source, path, remote URL and object-key metadata
before arbitrary SQL can transform them. Network URLs become scheme and host; the current home
prefix becomes `~`. Consented camera/source IDs, file names, sensor values/tags and notes remain
data. Caller-supplied SQL literals can still echo text already supplied in the request.

The model engine has no source attachment and disables target spill before open. Fixed setup clears
path allowlists, inherited proxy/account settings and secret locations, then disables external
access and locks configuration before source data or arbitrary SQL is evaluated. The complete copy
is capped at 100,000 rows, 16 MiB of Arrow transfers and 8 MiB per batch; one deadline includes setup,
copying and SQL. Limit failures, cancellation, unsupported reduced-feature builds and schema
mismatches have no raw-query fallback. The 32 MiB engine setting is not an RSS cap; Arrow transfer
accounting occurs after fetch, and the shared source buffer manager can still spill independently.
The integrated dataset filter passed 46 tests; final portable assertions passed ten tests and a
reduced-feature refusal test passed. A real IPC/scripted-provider CLI regression also passes: it
preserves authorized measurements, rejects private metadata under aliases and SQL transforms,
sanitizes inherited engine settings, and confirms the trusted raw query is identical before and
after. The complete 2,202-test default library and 21 assistant CLI workflows pass with this routing,
as do strict all-feature and headless all-target Clippy checks.

Trusted local CLI queries retain lossless catalog contents. The on-disk `TaskDataset` APIs remain a
storage foundation: projecting stored rows there does not sanitize its engine settings or workspace
paths. That API is not the provider-facing model-query boundary. A lossless copy remains available
for future local artifact work.

The coordinator now has the local runtime slice described below. The shared Slot and shared signal
sink still serve existing manual and assistant commands; those product routes have not yet adopted
the private capability. Camera catalog rows still need coordinator-issued task ownership. Export
staging, published copies and retained artifacts must share task/global ownership accounting; the
current generated-bundle byte cap does not bound their simultaneous physical disk growth. Never
lift a cap to finish a failing checkpoint. Imported snapshots remain separate from fresh acquisition
catalogs, avoiding sequence conflicts rather than adopting imported IDs for new writes.

A portable extent cap excludes filesystem block rounding, journals, copy-on-write and metadata;
covering those requires an OS quota or volume backend. The shared-engine scan limitation and pending
task routing mean this foundation does not establish a complete physical disk quota for agent runs.

## Local task-data runtime

[TaskDataHandle](../src/session/task_data.rs) is a local, nonserializable capability issued by one
coordinator. A task begins with a fresh canonical catalog and its own native extent budget. The
coordinator retains it after client-handle loss, closes admission at an absolute deadline, and
includes it in shutdown. Actor names, dataset IDs and public task IDs cannot select or create this
route through `SessionCommand`. The native assistant now has a capability-bearing
[ToolHost](../src/tools/context/task_data.rs) for an explicitly selected separate dataset. It binds
checked data commands to that private catalog and capture route; shared or foreign dataset/sink
identities refuse rather than falling back to the shared Slot. Existing manual, external CLI and
default assistant workflows retain their shared route. These local sensor-only integrations
supersede the earlier unwired-foundation checkpoint; they do not establish complete task isolation.

The [runtime](../src/assistant/run.rs) accepts a local `TaskDataRequest`, checks native-host support,
data consent, fresh approved sources, a camera-free scope, no selected shared dataset and no effects
outliving the run before provider setup. It reserves the complete dataset cap against the overall
run byte allowance before acquisition. The [task UI](../src/gui/assistant/tasks.rs) offers an explicit,
default-off **Separate dataset for this task** choice, mutually exclusive with the current dataset.
Its cap is the smaller of the data allowance and half the overall byte allowance, leaving room for
payload and export reservations. Camera selection or unavailable capability keeps the choice
visible and blocks Start with a reason. It never silently chooses shared storage. Mixed private
camera/data acquisition is not supported in this slice.

Fresh catalogs now expose `TaskDataset::query_model_bounded` through the same
[portable model snapshot](../src/dataset/model_snapshot.rs) boundary as shared Reader queries.
The owning writer remains borrowed under the task sink mutex. Fixed read-only scans validate every
canonical table before transferring any row; stored metadata projection precedes arbitrary SQL.
The separate in-memory target retains neutral settings, no target spill or external access, locked
configuration, transfer/result bounds, cancellation and one deadline covering setup, copy and SQL.
Schema, cap, timeout, cancellation and unsupported-feature failures have no raw-query fallback.
The on-disk `query_bounded` helper remains for trusted local analysis and must not reach providers.

[Private exports](../src/dataset/task_export.rs) borrow the same owned catalog and reuse bounded
CSV, JSON, JSONL and Parquet serializers, selected tables/time filters and checksummed manifests.
Generated staging bytes have their own durable global namespace reservation and a cumulative
pre-write cap; published copies use the ordinary anchored storage writers. Overall task allowance
accounts for staging and publication as two copies. Definite checked-close/removal permits staging
refund; uncertainty retains its full reservation and local recovery metadata. This handles the
simultaneous copies in the logical byte budget without claiming a physical block quota.

Final local `RunOutcome.task_data` receipts contain the exact catalog identity, full local path,
sample/signal/event counters, extent use, retained reservation and shutdown/persistence errors.
The UI distinguishes confirmed closure from uncertainty and warns on dropped writes. Provider
context and query results do not receive the local workspace path or the native capability. Native
scenes now cover separate sensor setup and a closed retained receipt; their current render review
and the new GUI regressions are pending the coordinated verification pass.

Signal instances bind immutably to either the shared sink or one private route. Each private route
has its own writer, queue-byte accounting, errors, dropped counts and capture flush barrier.
Registrations, samples, gaps and source errors follow the same route. Shared sink changes exclude
private sources; stalled private writes do not block shared flushes. Reader and startup fences
precede engine close. A close timeout preserves the engine and reservation; the coordinator's
deadline cleanup still runs after a timed-out client attempt. Late start acknowledgments cannot
resurrect a removed workspace. Local failure counters remain available after the hub owner exits.

[Storage reservations](../src/storage/namespace.rs) charge the entire logical extent cap before
publishing a fresh directory or admitting an engine. The ledger upgrades from version 2 to version
3 only when this ownership kind is used, after replaying the complete old journal; older binaries
refuse that upgraded ledger. Ordinary writable
dataset opens and capture writers refuse reserved namespaces, including aliases and renamed roots.
Native admission compares the original held directory identity before opening engine files. Typed
catalog writes quarantine failed storage operations; they never enlarge the cap for checkpointing.

Close, client loss, missing paths and file scans never refund an active or retained namespace. Only
native close/removal with zero uncertain extents, proof that the original held directory was
unlinked, and a durable ledger release can refund it. macOS APFS retains positive link counts on
unlinked open directories, so its proof uses a fresh child-creation attempt through the held fd:
only `ENOENT` proves removal; a live or renamed original rejects the receipt after removing the
temporary empty marker. Replacement names also refuse proof. A lost ledger acknowledgment can
retry the same receipt. Unused Pending admissions are reclaimed only after both publication and
liveness locks are clear, the original parent matches and no child was published. Active/retained
orphans stay charged and appear in storage inventory; there is no automatic deletion or close-time
reservation shrink. Setup failures attempt the same confirmed cleanup or identify retained state.
The catalog's `closed` receipt requires definite native shutdown; uncertainty remains quarantined
and charged even after the client connection has gone away.

Wait budgets bound mutex, startup, reader and queue waits, and cleanup refuses to begin another
native/ledger step once its budget is exhausted. Synchronous local filesystem/engine calls cannot
be forcibly interrupted and storage locks have their own finite timeout. These are logical EOF
bounds, not physical block, metadata, filesystem-journal, copy-on-write or process-memory quotas.
Global ledger and lease bookkeeping stays outside the native engine cap. The APFS proof
marker is transient filesystem metadata outside the extent cap. Linux remains unverified; Windows
task storage explicitly refuses admission. The new native-host route is limited to supported
in-process sensor tasks; it does not extend the capability through public CLI/IPC JSON.

The retained lifecycle now uses real local capabilities: GUI inventory obtains live handles from
the coordinator, while restart recovery acquires an exclusive owner lock under the durable ledger
lock and checks the original parent, directory identity and owner permissions. No action-log JSON,
actor label or arbitrary path becomes authority. Quick Command exposes inventory, reveal, copy and
confirmed removal on workers. New tasks discard pending confirmations without losing access to
previous work. Matching live receipts update only for the same task/catalog identity after removal.

Copies preserve the original database and WAL together in a new ordinary folder, with separate
storage charges and no overwrite. A real subprocess-exit integration verifies that the copied WAL
recovers a committed note. Removal requires native anchored deletion and the original-directory
proof before releasing the full cap; failed or uncertain operations stay charged and visible.
The same orphan inventory/copy/confirmed-removal path is available through `storage task-data`.
The temporary `task-data-preview` feature gate has been removed.

Separate datasets are explicitly native and sensor-only. Mixed image/data acquisition and external
capture agents use the supported shared dataset workflow. Private camera ownership and wire
transport of private capture capabilities are future extensions, rather than implicit fallbacks.

The historical macOS checkpoint before the native assistant/GUI integration passed **2,257 library
tests**, with 20 existing opt-in tests ignored. Strict all-feature/all-target and headless Parquet
all-target Clippy, formatting and diff checks passed. Focused checks passed 25 task-dataset tests,
51 storage tests, 26 signal-hub tests and seven coordinator tests. All 21 assistant CLI workflows
also passed. The coordinator integration at that checkpoint used a
test-only reader seam: admission, source supervision, private writing, native engine operations and
global storage accounting are real, while helper output comes from finite NDJSON or an OS pipe.
Existing real CLI workflows exercised the shared product route; that checkpoint did not establish
a private CLI/GUI storage route. The uncertain-close receipt test injects that state after a real
successful close; it verifies refusal and retention without claiming a reproduced native failure.
The real bounded-capture RPC and protected MQTT helper regressions also pass with isolated local
fixtures. A later focused headless Parquet task filter passed 36 tests, including fresh model-safe
queries and bounded private exports. The subsequently integrated native host, GUI choice, receipts,
new scenes and full end-to-end routing require the current coordinated tests, strict checks and
native render review; they are not covered by the historical totals.

The current native integration checkpoint passed **2,292 library tests**, with 20 existing opt-in
tests ignored, all 21 assistant CLI tests, the actual native task runtime integration, bounded-capture
RPC and protected MQTT helper regressions. Strict all-feature/all-target and headless Parquet
all-target Clippy passed. All 85 Python script tests, static-site validation and formatting/diff
checks passed. Snapshot/export cancellation regressions verify that a watchdog is joined before
transaction rollback so a cancelled query does not poison the reusable source reader.

Native review now includes 45 screenshots in `target/agentic-task-storage-native/index.html` and
six explicit scrolled disclosure/receipt views in
`target/agentic-task-storage-details-native/index.html`. Image hashes were independently verified.
The latter use binary SHA-256 `c5a889abe7150270fb06f59fab8479bfd45f82da29f4c747bb7a773488de74e7`;
the former retain their separate manifest and binary hash. The compact scrolled setup shows the
logical extent cap and retained reservation disclosure; the completed result shows counts,
extent usage and retained cap. These are actual desktop renderer pixels, with synthetic fixtures.
The screenshot harness-only scroll change passed its scoped Rust test and seven Python tests.

No live provider credentials or physical devices were used.

The external-agent CLI suite now passes **12 tests**, including a direct `tools call` mixed workflow
with no model stub: a real session, camera worker and signal helper persist 12 samples, capture a
PNG, query six joined rows and export Parquet. The test independently checks every file hash and
byte count, table row counts, the captured image hash and successful agent-attributed activity.
The CLI help now describes image and data tools. Strict all-feature/all-target Clippy passed after
these changes. The original 11 camera/discovery/plan CLI regressions also remain green.

## Supported-workflow simulator audit

The audit below covers the shared image/data routes; separate native sensor datasets additionally
use the ownership and restart-recovery checks recorded above.

| Audit area | Executable evidence |
| --- | --- |
| Both agent routes; camera-only, data-only and mixed acquisition | `tests/cli.rs`: 21 assistant workflows and 12 external `tools_` workflows; mixed cases independently read exported tables and verify PNG/export hashes. |
| Scope, consent, source binding and human changes | `src/assistant/run.rs`, `src/tools/data.rs`, `src/gui/assistant/quick_tests.rs` and `tasks_tests.rs`: exact source/account/dataset identities, refusal before provider setup, reviewed revisions, grant invalidation, pause and takeover. |
| Finite acquisition and persistence | `src/signals/hub.rs` bounded-capture tests and actual CLI bounded-capture RPC: sample/byte limits, flush, sink failures, helper restarts, replacement-safe conditional stop and incomplete-persistence receipts. |
| Query and export correctness | `src/dataset/tests.rs`, model snapshot tests and `src/tools/data.rs`: read-only queries, row/byte/time limits, projection before SQL, close cancellation, overwrite/path refusal, formats, inclusive time filters and checksums. |
| Limits and privacy | `src/assistant/limits.rs`, action-log tests, model snapshot tests and protected MQTT helper integration: cumulative reservations, failed-attempt charges, deadlines, credential redaction and model/local metadata boundaries. |
| Intervention, recovery and accessible UI | `src/gui/assistant/task_integration_tests.rs`, `tasks_tests.rs` and `task_layout_tests.rs`: hide/reopen, cancellation, quit journaling, unfinished logs, takeover after flush, replacement-safe cleanup, keyboard traversal and compact data/mixed states; native galleries verify rendered layouts. |

The individual cases to retain in future changes are:

- Mixed task: log `sim-signal://line1`, capture `sim:0`, query `captures_with_signals`, export and independently verify rows/checksums.
- Data-only and camera-only tasks; reject empty scope, invented endpoints, wrong datasets and escaped output roots before effects.
- Cached observations make no connection; fresh browse/read has consent; denial cannot authorize a substitute endpoint.
- Time/sample/byte limits stop and flush after client loss; stalled sinks/storage failures report incomplete persistence accurately.
- Conditional stop survives helper restart and refuses a person's replacement of the same URI, including PID reuse.
- Human source/dataset/sink changes invalidate grants and apply pause/inform/stop while preserving other participants' work.
- Dataset close/reopen between check and execution is detected; foreign shared sink changes require the correct approval.
- Activity/status/UI expose readiness, quality, timestamps, restarts, drops, persisted counts, failures and leftovers.
- Queries enforce scope, read-only single statements, output/row/time bounds, truncation and unsupported-feature behavior.
- Exports reject existing folders/traversal, honor table/time/format/task/global limits, and verify reported file checksums.
- Credentials/private URI details stay out of model payloads/logs; data text cannot become instructions; disclosures match transmission.
- Sheet close/reopen, cancellation, quit, crash recovery and takeover preserve state; render/accessibility tests cover data-only, mixed, approval, paused, error and complete states.

These tests establish simulator and local execution behavior. Live provider judgment, physical
devices and platform-specific behavior outside the macOS verification remain unverified.

Historical supported-build checkpoint: the default library rerun passed **2,292 tests** (20 existing
opt-in tests ignored); strict all-feature/all-target and headless Parquet all-target Clippy passed
after the preview gate. All 85 Python tests, static-site validation, formatting and diff checks
passed. A temporary preview gate at that checkpoint has since been removed after implementing
the retained lifecycle. Both agent routes use the shared
image/data registry; no release, commit or publication was requested or performed.

Retained-lifecycle completion checkpoint: **2,296 library tests passed** (20 existing opt-in tests
ignored), with a final full rerun after the copy and confirmation changes. The complete CLI suite
passed **75 tests** (five existing opt-in tests ignored). Real native-runtime, authenticated MQTT
helper and restart/WAL recovery integrations passed. The recovery case covers clean shutdown and
abrupt process exit, verifies unchanged originals and byte-identical DB/WAL copies, reopens the copy
to recover a committed note, rejects unconfirmed deletion and confirms the exact reservation refund.
The GUI case uses actual coordinator handles to verify copy integrity, cancellation, new-task access,
stale inventory/review rejection, asynchronous removal and preservation of the saved copy. Three
storage cases cover owner exclusion, anchored identity, symlink/hard-link refusal and retained charges.

All-feature/all-target and headless Parquet all-target strict Clippy, 85 Python tests, static-site validation, formatting and diff
checks passed. Twelve native software-renderer frames in `target/agentic-retained-layout/index.html`
cover inventory and permanent-removal review in both appearances at every UI scale; their SHA-256
manifest is beside the gallery. Compact keyboard reachability is checked against real enabled
controls. No live provider credentials or physical capture devices were used.

## Provider setup regression fixed

Bedrock profile-only region validation now runs on the settings worker. The nonsecret region
fallback uses explicit settings, `AWS_REGION`, `AWS_DEFAULT_REGION`, then the selected profile.
Inherited regions remain absent from saved preferences. Test connection refreshes the profile;
missing regions block the request, and changed/closed settings discard stale replies. Nineteen
GUI settings tests passed, including isolated config-file, precedence, off-thread lookup and
late-reply cases. No live AWS credentials or provider request were used.

## UI, UX and implementation verification — 2026-10-19

The independent audit found and fixed the following issues:

- The default Plan-first sheet hid retained-dataset worker failures. It now exposes them as
  accessible alerts, avoids a misleading empty-inventory message on failure, and clears a stale
  failure when a valid retry starts. Retained rows preserve persistence, sink and dropped-sample
  warnings; copy eligibility follows confirmed native closure. Closing the sheet invalidates a
  pending permanent-removal review.
- A disconnected source-profile worker could leave Refresh and reopening bound to a dead worker.
  Both disconnect paths now retire the worker and advance the reply-generation fence before retry,
  so a queued old binding cannot authorize a new scope.
- Provider summaries inferred readiness from API keys and misreported keyless/local and AWS/ADC
  configurations. The selected provider now uses the asynchronous credential result and connection
  test receipt, with explicit pending states. Unchecked providers show credential presence without
  claiming readiness; edits invalidate cached results.
- Unix-valid paths rejected by the native dataset adapter could strand an empty namespace and its
  reservation. Native-compatible absolute UTF-8 paths are checked before publication and admission,
  including the resolved physical parent. Recovery permits deletion of an old incompatible path
  only with an exclusive recovered owner lock and proof that the anchored original is empty;
  nonempty or replaced workspaces remain protected and charged.

The current verification passed **2,307 library tests** (20 existing opt-in tests ignored),
**75 CLI tests** (five existing opt-in tests ignored), and four native runtime, authenticated-helper
and restart-recovery integration tests. The recovery integrations include a live owner: copy and
confirmed removal refuse without changing DB/WAL bytes or the reservation; after that owner exits,
copy, reopen and removal succeed. All 85 Python tests, static-site validation, formatting, diff checks,
and strict all-feature/all-target and headless Parquet all-target Clippy passed.

`target/agentic-ui-verified/index.html` contains **492 actual iced software-renderer frames** with a
SHA-256 manifest, including both appearances, supported interface scales, compact task controls,
setup, consent, errors, retained inventory and removal review. Retained views now render through the
actual Plan-first sheet composition. Accessibility and layout assertions check alerts, keyboard
reachability and viewport bounds. Visual inspection also caught a test-harness theme mismatch in
the external-agent setup screenshots; its renderer now receives the matching appearance, and the
focused rendering test passed after regeneration. This correction changes test evidence only.

The Mac was locked, so live desktop interaction, native window chrome and OS file pickers were not
verified. These checks use isolated local datasets, simulators and scripted model responses; live
provider judgment and physical devices remain unverified. No release, commit or publication was
requested or performed.
