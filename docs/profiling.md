# Runtime profiling

Internal development measurements on 2026-11-10, on an Apple M4 Max Mac
Studio with 36 GiB RAM, macOS 26.6.2, Rust/Clippy 1.99. These are simulator
and synthetic-frame results, not camera compatibility or throughput claims.

## Findings and changes

- A warm worker's hardware JPEG validation map grew with every new
  `(width, height, format, quality)` combination. It now retains at most
  256 verdicts. An evicted combination undergoes the full accuracy check
  before hardware is trusted again. Native encoder sessions and frame queues
  already have independent bounds; this map bound addresses metadata retained
  between commands.
- Instruments CPU samples placed focus's nine-neighbour pixel reads, clipping,
  and exposure sampling among the hot paths. Focus now dispatches common
  Mono8/RGB8/BGR8 layouts once per measurement. Decoded layouts bypass the outer
  generic pixel dispatch. Sampling coordinates, RGB-to-luma rounding, accumulation
  order and results remain unchanged. Tests compare the optimized and generic
  paths on odd sizes, small frames and regions, including planar and packed YUV.
- Filtering the benchmark to `encode/jpeg-videotoolbox/` or `encode/jpeg-rgb/`
  previously skipped its fixture loop entirely. Both filters now run their
  requested cases. Unselected hardware benchmarks no longer open an engine;
  selected hardware cases print a reason when unavailable.

## CPU timings

Before/after release builds used the same `--no-default-features` configuration
and the existing `runtime` benchmark (median of up to 500 measurements per case).
The before executable was preserved before rebuilding. Values are milliseconds
for the whole inspection measurement: exposure, clipping, auto meter and focus.
These are individual runs, subject to scheduler and thermal variation.

| Layout | Size | Before | After |
| --- | --- | ---: | ---: |
| Mono8 | 640x480 | 0.977 | 0.547 |
| RGB8 | 640x480 | 1.126 | 0.846 |
| yuv420p | 640x480 | 2.436 | 2.149 |
| NV12 | 640x480 | 2.390 | 2.100 |
| UYVY | 640x480 | 2.606 | 2.305 |
| Mono8 | 1920x1080 | 1.490 | 0.744 |
| RGB8 | 1920x1080 | 1.804 | 1.227 |
| yuv420p | 1920x1080 | 3.302 | 2.924 |
| NV12 | 1920x1080 | 3.221 | 2.907 |
| UYVY | 1920x1080 | 3.589 | 3.249 |

The existing YUV/RGB performance ratio assertion remains at 3.0 and passes.
At 1080p a single-thread baseline was slower than the normal clipping path
(about 2.0 ms versus 1.5 ms for Mono8), so clipping parallelism was preserved.

Default-feature 1080p RGB JPEG encoding measured about 3.04 ms with libjpeg-turbo
and 1.71–1.73 ms through VideoToolbox, including normal output validation.
These JPEG measurements establish the hardware path's benefit on this host;
there is no change to engine choice or quality policy.

## Memory and hardware observations

Process RSS and CPU were sampled with `ps`, including workers and FFmpeg rather
than only the CLI. CPU percentages are fractions of one logical CPU (100% is one
CPU), and snapshots include startup effects.

- Three consecutive 150-frame, 640x480 CPU-JPEG captures in one live session:
  the debug worker RSS ranged from 18,656 to 21,552 KiB. Peak RSS by burst was
  21,264, 21,456 and 21,552 KiB. CPU samples during capture reached about 50%.
  This short baseline run shows warm-up and modest retained growth; it cannot
  prove the absence of a slow leak.
- The native VideoToolbox leak test encoded 200 1080p Bayer frames, dropped the
  engine, then ran macOS `leaks`: **0 leaks for 0 total leaked bytes**.
- Three separate optimized hardware-JPEG benchmark processes peaked at roughly
  91 MiB RSS each (includes fixtures, validation reference images and native
  sessions), with sampled CPU peaks of 33–56%. These are separate processes,
  not evidence about retention within one long-lived process.
- A 20-second optimized 640x480 simulator recording selected
  `h264_videotoolbox`, finalized 599 frames at 29.99 fps, and reported two dropped
  frames. Worker RSS stayed at 14,576–14,608 KiB; FFmpeg settled near 43,296 KiB
  (43,376 KiB sampled peak). Steady worker CPU was about 2.6–2.8%, FFmpeg about
  0.5%. The recording was finalized successfully.
- GPU counters from `ioreg -r -c IOAccelerator -d 1` were sampled at approximately
  100 ms during JPEG benchmarks and one second during recording. Device,
  renderer and tiler utilization samples were 0%. These are system-wide,
  coarse counters; VideoToolbox uses dedicated media hardware, whose occupancy
  these counters do not expose. System GPU allocation totals also include
  unrelated desktop applications and are not attributed to Capturefab.

Physical camera transports, Linux CUDA/nvJPEG and VA-API device utilization
were not measured on this Mac. Their fake-driver/shared-memory JPEG tests run
here, but cannot establish real driver resource usage. Long-duration recordings,
network sources and physical camera workloads should be profiled on their target
hosts before making corresponding performance claims.

## Reproducing

Use private, isolated `CAPTUREFAB_SESSION_DIR` and `CAPTUREFAB_CACHE_DIR` folders
for CLI workloads; stop any profiling session afterward. Include the worker,
coordinator and encoder children when sampling processes. Keep output on a disk
with sufficient free space and remove scratch captures after measuring.

```sh
cargo bench --bench runtime --no-default-features -- measure/
cargo bench --bench runtime -- encode/jpeg-videotoolbox/RGB8/1920x1080
cargo bench --bench runtime -- encode/jpeg/RGB8/1920x1080
cargo test --lib jpeg:: -- --test-threads=1
cargo test --lib jpeg::videotoolbox::tests::a_burst_through_the_engine_leaks_nothing -- --ignored --exact --nocapture
cargo test --lib frame::tests::
cargo test --test frame_golden
cargo test --test inspect
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings
```

On macOS, hardware and shared-memory tests need access to native media services
and POSIX shared memory; a restrictive sandbox can produce permission errors.
For Instruments profiling, retain symbols (the release profile normally strips
them), and run the resulting benchmark executable with `--bench`:

```sh
CARGO_PROFILE_BENCH_DEBUG=1 CARGO_PROFILE_BENCH_STRIP=false cargo bench --bench runtime --no-run
# Substitute the executable path printed by Cargo.
xctrace record --template 'Time Profiler' --output /tmp/capturefab-cpu.trace --launch -- target/release/deps/runtime-HASH --bench measure/
```

Generate a matching dSYM with `dsymutil` before rebuilding that executable when
symbolicating the trace. Keep benchmark timing runs separate from profiler runs.
