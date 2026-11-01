# Binary size

The executable stores the complete bundled FFmpeg with lossless zlib compression and streams it
to the user's cache on first use. Extraction checks its length, SHA-256 and compressed stream
before publishing the file, and release packaging independently decompresses and compares the
original bytes. See [FFmpeg build details](ffmpeg-build.md).

Compressing the payload made the stripped macOS ARM64 executable 14.6% smaller in a measurement on
2026-10-30, with identical extracted FFmpeg bytes, versions, protocols, encoders and license
notice. That measurement predates the removal of the desktop app, its fonts and the DuckDB
dataset engine, which shrank the executable further; measure again before quoting a size.

To compare two executables:

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
