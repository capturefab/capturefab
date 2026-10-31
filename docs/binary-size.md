# Binary size

The executable stores the complete bundled FFmpeg with lossless zlib
compression and streams it to the existing cache on first use. Extraction
checks its length, SHA-256 and compressed stream before publishing the file.
Release packaging independently decompresses and compares the original bytes.
See [FFmpeg build details](ffmpeg-build.md).

The icon fonts contain all 48 icon mappings used by the GUI. Their outlines,
names, advances and line metrics match the complete upstream fonts. The
[font regeneration instructions](../assets/fonts/README.md) explain how to
reproduce and verify the subsets. Normal builds need no font tooling.

## Measured comparison

On 2026-10-30, macOS ARM64 with Rust 1.99.0, `cargo build --release --locked`
with default features, comparing commit
`80595db1b9a1bbecced8a3b23908dbbe5e16b97e` with these changes:

| Measurement | Before (bytes) | After (bytes) | Reduction |
| --- | ---: | ---: | ---: |
| Stripped executable | 95,174,240 | 81,238,048 | 14.6% |
| Gzip-compressed executable, level 9 | 38,762,858 | 38,215,525 | 1.4% |
| Stored FFmpeg payload | 24,625,192 | 11,729,649 | 52.4% |
| Two icon fonts | 937,888 | 14,512 | 98.5% |

Both builds use the same feature set, FFmpeg input, compiler settings and
database engine. Sizes vary with platform and toolchain. These measurements
cover the executable; release packages also contain installation and license
materials. A separate experiment compiling DuckDB for size made a large hash
join 38% slower, so the existing optimization settings remain in place.

To measure another pair of executables:

```sh
python3 - /path/to/before /path/to/after <<'PY'
import gzip
from pathlib import Path
import sys
for name in sys.argv[1:]:
    data = Path(name).read_bytes()
    print(name, len(data), len(gzip.compress(data, compresslevel=9, mtime=0)))
PY
```

## Verification

The comparison verified identical extracted FFmpeg bytes (SHA-256
`a8b024bdf7753a1161c96d91d4fc634d2b55759e71c1291e0b790b3ee5b36b7d`),
version, protocols, encoders and license notice. The public schema matched
after normalizing the scheduled-capture example's current timestamp.
The release smoke check produced two simulated PNG captures.

Checks passed: the complete all-feature Rust test suite, all 11 ignored
FFmpeg media integration tests, strict all-feature/all-target Clippy,
formatting, Python script tests, and byte-identical font regeneration with
outline and metric comparisons. Dataset comparisons exercised event inserts,
JSON grouping, joins, aggregates, protected Arrow snapshots and both native
and bounded CSV/Parquet/JSON/JSONL exports.
