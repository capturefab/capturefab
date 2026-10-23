# Agentic camera capture: implementation notes

The built-in assistant and external agents share the checked camera-tool registry. Plans expose camera settings, capture outputs and risks before changes run. Bounded tasks pin camera IDs, camera dataset identity, allowed destinations and resource limits.

## Camera tools and datasets

Observation tools inspect connected cameras, features, frames, jobs, storage and destinations. Action tools configure cameras, capture photos, record video and schedule captures. Dataset tools query saved image and recording metadata, add notes and export analysis bundles.

Image and dataset permissions are separate. Model-facing metadata removes private serials and URL details before SQL can transform them. Trusted local CLI queries retain the original catalog.

Stop prevents new actions and waits for accepted work to finish. Camera controls remain available to a person; human changes can pause a task. Completion reports remaining recordings, schedules and reviewable settings restores.

## Provider setup regression fixed

Bedrock profile-only region validation now runs on the settings worker. The nonsecret region
fallback uses explicit settings, `AWS_REGION`, `AWS_DEFAULT_REGION`, then the selected profile.
Inherited regions remain absent from saved preferences. Test connection refreshes the profile;
missing regions block the request, and changed/closed settings discard stale replies. Nineteen
GUI settings tests passed, including isolated config-file, precedence, off-thread lookup and
late-reply cases. No live AWS credentials or provider request were used.

## UI, UX and implementation verification — 2026-10-19

The independent audit found and fixed the following issues:

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
