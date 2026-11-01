#!/usr/bin/env python3
"""Export the Windows executable icon and the website icons from assets/icons/capturefab.svg.

Requires rsvg-convert (librsvg). The generated files are checked in; normal
builds and release builds need no image tools or Python packages.
"""

from pathlib import Path
import struct
import subprocess

ROOT = Path(__file__).resolve().parents[1]
ICONS = ROOT / "assets/icons"


def render(svg, size):
    return subprocess.run(
        ["rsvg-convert", "--width", str(size), "--height", str(size)],
        input=svg.encode(), capture_output=True, check=True,
    ).stdout


def ico(images):
    directory = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    for size, data in images.items():
        directory += struct.pack("<BBBBHHII", size % 256, size % 256, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    return directory + b"".join(images.values())


def windows_resource(images):
    # Architecture-independent .res input accepted by the MSVC/LLD linker.
    # https://learn.microsoft.com/windows/win32/menurc/resourceheader
    def record(kind, name, data, flags=0x1030):
        header = struct.pack("<IIHHHHIHHII", len(data), 32, 0xffff, kind,
                             0xffff, name, 0, flags, 0, 0, 0)
        return header + data + bytes(-len(data) % 4)

    resources = record(0, 0, b"", flags=0)
    group = struct.pack("<HHH", 0, 1, len(images))
    for index, (size, data) in enumerate(images.items(), 1):
        resources += record(3, index, data)  # RT_ICON
        group += struct.pack("<BBBBHHIH", size % 256, size % 256, 0, 0, 1, 32, len(data), index)
    return resources + record(14, 1, group)  # RT_GROUP_ICON


def main():
    svg = (ICONS / "capturefab.svg").read_text()
    sizes = (16, 20, 24, 32, 40, 48, 64, 128, 256)
    images = {size: render(svg, size) for size in sizes}
    (ICONS / "capturefab.ico").write_bytes(ico(images))
    (ICONS / "capturefab.res").write_bytes(windows_resource(images))

    web = ROOT / "web"
    (web / "favicon.svg").write_text(svg)
    (web / "favicon.ico").write_bytes(ico({size: images[size] for size in (16, 32, 48)}))
    (web / "favicon-32.png").write_bytes(images[32])
    # iOS supplies its own corner mask; give it an opaque, full-bleed tile.
    touch = svg.replace('viewBox="0 0 128 128"', 'viewBox="4 4 120 120"').replace('rx="28"', 'rx="0"')
    (web / "apple-touch-icon.png").write_bytes(render(touch, 180))
    print("Exported Capturefab icons for Windows and the web.")


if __name__ == "__main__":
    main()
