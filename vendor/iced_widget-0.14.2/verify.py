"""Verify the exact published source and reproduce the sole upstream diff."""
import difflib
import hashlib
from pathlib import Path
import sys
import tarfile

ARCHIVE_SHA256 = "b1596afa0d3109c2618e8bc12bae6c11d3064df8f95c42dfce570397dbe957ab"
SOURCE = "src/text_input.rs"
root = Path(__file__).resolve().parent
archive = Path(sys.argv[1])
assert hashlib.sha256(archive.read_bytes()).hexdigest() == ARCHIVE_SHA256
changed = []
with tarfile.open(archive) as upstream:
    for member in upstream.getmembers():
        if not member.isfile():
            continue
        relative = Path(member.name).relative_to("iced_widget-0.14.2")
        original = upstream.extractfile(member).read()
        current = (root / relative).read_bytes()
        if current != original:
            changed.append(str(relative))
        if str(relative) == SOURCE:
            patch = "".join(difflib.unified_diff(
                original.decode().splitlines(keepends=True),
                current.decode().splitlines(keepends=True),
                fromfile=f"a/{SOURCE}", tofile=f"b/{SOURCE}",
            ))
assert changed == [SOURCE], changed
assert patch == (root / "UPSTREAM.patch").read_text()
print("Published iced_widget 0.14.2 verified; only the documented text-input patch differs.")
