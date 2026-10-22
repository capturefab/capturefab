# Build and test performance

The CI suite keeps all six native runner/architecture combinations, strict
all-feature/all-target Clippy, the no-GUI feature checks, browser tests, embedded
browser integration tests, and site/script validation. Performance changes reuse
build inputs and stop waiting once a test's required observation is available.
They do not disable assertions, overflow checks, protocols, or tests.

## Reuse expensive native inputs

Every native CI job uses `.github/actions/setup-ffmpeg` before invoking Cargo.
The action restores the same recipe-keyed, runner-specific FFmpeg bundle that
the native test matrix already used. On a miss it runs the existing pinned,
checksum-verified source build and saves the result. It exports
`CAPTUREFAB_FFMPEG_BINARY` for subsequent Cargo invocations. The bundle stays in
`runner.temp`, outside Cargo's target directory: the Rust cache removes files it
does not recognize as Cargo artifacts. Windows installs MSYS2 only on a bundle
cache miss; Rust dependencies still use their original MSVC and NASM setup.
Every bundle must successfully execute a version query before Cargo uses it,
including cache hits. This catches loader or DLL failures that optional media
tests can skip.
Release builds retain their separate platform compatibility and signing checks.

The WASM job caches both `web-client/target` and the root native `target`.
Its final remote integration test builds the native app with the embedded client,
so caching only the browser crate left the native dependencies uncached. Native
Rust caches also save successfully compiled dependencies when a later check fails.
Keys retain the cache action's job, platform, toolchain, manifest and lockfile
separation; this does not share incompatible feature builds between jobs.

The pinned [Rust cache implementation](https://github.com/Swatinem/rust-cache/tree/6323deb102c322ba6fcbdcafc7e3dddab59af2b6)
describes workspace selection, Cargo binary caching and failure caching.

## Keep native build dependencies stable

The no-GUI job still checks the base, FTP, SFTP and hubs configurations
separately. Previously those configurations could each rebuild bundled DuckDB
even when DuckDB's own features were identical. On Linux, the FTP keyring's
proc-macros enabled additional `syn` features used by `serde_derive`, a dependency
of DuckDB's build script. SFTP also enabled the host `once_cell` default feature.
Those changes produced different build-script dependency graphs and caused Cargo
to compile the same C++ engine again. The [no-GUI job in run 39644858046](https://github.com/capturefab/capturefab/actions/runs/39644858046/job/119377615658)
spent 6 minutes 48 seconds on the base check and another 6 minutes 53 seconds on
the FTP check; nearly all of each interval preceded DuckDB's Rust wrapper check.

The explicit `syn` and `once_cell` build dependencies request the host feature
union already used by the full app. They add no runtime dependencies, application
features, new packages or dependency versions. In an isolated snapshot, comparing
Cargo's unit graphs for seven configurations across Linux x64, Windows x64 and
macOS ARM targets showed identical transitive DuckDB build dependencies after
this change. Every target compilation unit and its runtime feature set remained
the same. These cross-target graph checks ran on a macOS host; native CI remains
the verification of the actual platform builds and their elapsed times.

The engine with JSON/Parquet enabled remains a separate build from the engine
without those features. Reuse never substitutes one engine configuration for
another. To inspect reuse, use one target directory and the same FFmpeg input,
then run the existing checks with verbose Cargo output:

```sh
cargo check --locked --no-default-features -vv
cargo check --locked --no-default-features --features ftp -vv
cargo check --locked --no-default-features --features sftp -vv
cargo check --locked --no-default-features --features hubs -vv
```

After the first check, the remaining checks should report `Fresh libduckdb-sys`
while still compiling and checking their own feature-specific code. A source,
toolchain, compiler flag or DuckDB feature change may require rebuilding it.

Native reuse was also checked on macOS ARM with a separate target directory.
The initial minimal check took 49.73 seconds. The following FTP, SFTP and hubs
checks took 8.34, 9.38 and 8.32 seconds, and each reported `Fresh libduckdb-sys`.
These figures verify reuse on that host; they are not a comparison of total CI
wall time across different runners.

## Hosted measurements

The first fork run populated empty dependency caches. The following run restored
those dependencies on the same hosted Ubuntu image and retained all original
commands and tests. Completed job outputs show:

| Measurement | Empty dependencies | Restored dependencies |
| --- | --- | --- |
| Strict Clippy compilation | [9m04s](https://github.com/shntu/capturefab/actions/runs/39767775922/job/119787410085) | [54.59s](https://github.com/shntu/capturefab/actions/runs/39770864443/job/119797751097) |
| Linux x64 test compilation | [10m37s](https://github.com/shntu/capturefab/actions/runs/39767775922/job/119787409945) | [1m36s](https://github.com/shntu/capturefab/actions/runs/39770864443/job/119797751233) |
| Complete Linux x64 test job | [19m38s](https://github.com/shntu/capturefab/actions/runs/39767775922/job/119787409945) | [5m27s](https://github.com/shntu/capturefab/actions/runs/39770864443/job/119797751233) |

The comparison runs are `84c1aee` and `cdc9f16`. The latter adds host dependency
stabilization and the deterministic ring regression; runtime features and
compiler profiles remain identical. These measurements cover the named jobs and
steps. Cache availability and runner load affect elapsed time, and individual
test runtimes varied between the runs.

The no-GUI job also completed with exactly two DuckDB engine compilations:
one with JSON/Parquet, and one without them. Its subsequent FTP, SFTP and hubs
checks took 33.00, 37.09 and 22.17 seconds, with no further native engine builds.
An existing Windows Modbus CLI test exposed a refusal/timeout race during
validation. It now checks refusal with a longer deadline and independently
checks timeout against a held-open silent listener, without platform-specific
expected exit codes.
Intel macOS also exceeded a three-second test fixture budget while setting up a
private snapshot for a semantic privacy check. That helper now uses the existing
production default budget. The explicit one-millisecond deadline, cancellation
timing and resource-limit fixtures retain their original bounds and assertions.

A subsequent [warm run of `0c2db78`](https://github.com/shntu/capturefab/actions/runs/39775422837)
restored the exact current cache keys. The [WASM job](https://github.com/shntu/capturefab/actions/runs/39775422837/job/119813021289)
completed in 1 minute 10 seconds, compared with 10 minutes 28 seconds in the
preceding run. Its native remote integration-test compilation took 32.86 seconds
instead of 7 minutes 30 seconds. All 12 browser tests and 13 native integration
tests passed; both client builds and strict WASM Clippy still ran. Earlier jobs
cached only the browser target directory, so their native test dependencies were
rebuilt each time.
In this warm run, the root target cache reused those native dependencies.
These are observed cold-versus-warm timings; the WASM runner image also changed
between the runs.

The [warm no-GUI job](https://github.com/shntu/capturefab/actions/runs/39775422837/job/119813021915)
completed in 3 minutes 11 seconds with no native engine compilations. The no-GUI
build took 40.86 seconds; base, FTP, SFTP and hubs checks each took
15.55–16.05 seconds. Both engine configurations were restored from the dependency
cache, while each application configuration was still built or checked.

These runs used a temporary public fork. Once the host dependency change created
new Rust cache keys, six obsolete keys from the initial run were removed to keep the
comparison within the repository cache quota. Current Rust keys, FFmpeg bundles
and MSYS2 caches were retained. No upstream repository caches were changed.

## Baseline evidence

Before this change, the [WASM job in run 39621917184](https://github.com/capturefab/capturefab/actions/runs/39621917184/job/119297340378)
compiled its native remote test in 9 minutes 38 seconds, then ran all 13 tests
in 3.83 seconds. The [same job in run 39644858046](https://github.com/capturefab/capturefab/actions/runs/39644858046/job/119377615913)
spent 14 minutes 58 seconds compiling and 3.71 seconds running those tests.
These are baseline observations, not a promised reduction in total CI duration.
The first run with a new dependency-cache key still needs to populate it, and
runner load, cache eviction and source changes affect later timings.

The viewerless CLI stream test previously slept for five seconds before checking
that more than 30 frames arrived. It now polls for that same threshold with a
10-second deadline. The loss, drop-ratio, ring-backing and disk checks remain.
A deterministic shared-memory regression separately fills an unwatched ring and replaces slots
while it stays full, checks the counters and reads back the newest frame.
This preserves the overwrite coverage of the former five-second observation
without waiting for real keep-alive intervals. Tests whose elapsed interval is
itself under test keep their real waits.
On the same local app and source snapshot, three runs before and after the test
change had median wall times of 6.689 and 2.668 seconds (about 60% lower).
All eight runs (including separate warmups) passed, and the complete CLI test
lists remained identical at 80 tests. First runs in both groups were slower because of process/cache startup;
these numbers describe this test, not the complete suite.

## Reproduce measurements

Use a stable source snapshot and the same feature set and FFmpeg input on both
sides of a comparison. Do not run competing Cargo builds or other heavy work
while measuring. The commands below retain the ordinary default-feature suite:

```sh
cargo test --locked --no-run --timings
cargo test --locked --no-fail-fast
cargo test --locked --test cli viewerless_streams_report_no_losses_and_keep_frames_off_disk -- --exact
cargo fmt --check
cargo clippy --locked --all-features --all-targets -- -D warnings
python3 -m unittest discover -s scripts
python3 scripts/test-site.py
actionlint
```

`cargo test --no-run --timings` records compilation separately from test runtime
in `target/cargo-timings/`. Compare a dependency-cold run separately from a warm
run and a source-edit rebuild. In GitHub Actions, compare individual setup and
Cargo steps as well as total job time, and confirm that the expected caches hit.
Do not change the bundled FFmpeg payload between measurements or use a local
development override in a distributed release.
