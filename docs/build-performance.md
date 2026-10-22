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
Release builds retain their separate platform compatibility and signing checks.

The WASM job caches both `web-client/target` and the root native `target`.
Its final remote integration test builds the native app with the embedded client,
so caching only the browser crate left the native dependencies uncached. Native
Rust caches also save successfully compiled dependencies when a later check fails.
Keys retain the cache action's job, platform, toolchain, manifest and lockfile
separation; this does not share incompatible feature builds between jobs.

The pinned [Rust cache implementation](https://github.com/Swatinem/rust-cache/tree/6323deb102c322ba6fcbdcafc7e3dddab59af2b6)
describes workspace selection, Cargo binary caching and failure caching.

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
Tests whose elapsed interval is itself under test keep their real waits.
On the same local app and source snapshot, three runs before and after the test
change had median wall times of 7.163 and 2.860 seconds (about 60% lower).
All six runs passed, and the complete CLI test lists remained identical at
80 tests. First runs in both groups were slower because of process/cache startup;
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
