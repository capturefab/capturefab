#!/usr/bin/env python3
"""Validate static pages, documentation search, generated release pages, and published assets."""
import hashlib
import importlib.util
import io
import json
import os
import platform
import re
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import threading
import zipfile
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import xml.etree.ElementTree as ET
from html import unescape
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import urljoin, urlsplit, unquote

ROOT = Path(__file__).resolve().parents[1] / "web"
ATOM = "{http://www.w3.org/2005/Atom}"
TIMESTAMP = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})")


class Page(HTMLParser):
    def __init__(self):
        super().__init__()
        self.ids = set()
        self.links = []
        self.scripts = []
        self.images = []
        # (srcset, width, height, fetchpriority, src) for responsive images.
        self.srcsets = []
        self.videos = []
        self.current_video = None
        self.errors = []
        self.lang = False
        self.main = False

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if tag == "html":
            self.lang = bool(attrs.get("lang"))
        if tag == "main":
            self.main = True
        if attrs.get("id"):
            if attrs["id"] in self.ids:
                self.errors.append(f"duplicate id: {attrs['id']}")
            self.ids.add(attrs["id"])
        if tag == "img":
            if "alt" not in attrs or (not attrs["alt"] and attrs.get("aria-hidden") != "true"):
                self.errors.append("image without descriptive alternative text")
            self.images.append((attrs.get("src", ""), attrs.get("width"), attrs.get("height")))
            if attrs.get("srcset") or attrs.get("fetchpriority"):
                self.srcsets.append((attrs.get("srcset") or "", attrs.get("width"), attrs.get("height"), attrs.get("fetchpriority"), attrs.get("src", "")))
        if tag == "script":
            self.scripts.append(attrs.get("src", ""))
        if tag == "video":
            self.current_video = {"attrs": attrs, "sources": []}
            self.videos.append(self.current_video)
            if attrs.get("src"):
                self.current_video["sources"].append(attrs["src"])
        if tag == "source" and self.current_video is not None and attrs.get("src"):
            self.current_video["sources"].append(attrs["src"])
        runtime_url = None
        if tag in {"img", "script", "iframe", "audio", "video", "source"}:
            runtime_url = attrs.get("src")
        elif tag == "link" and attrs.get("rel") == "stylesheet":
            runtime_url = attrs.get("href")
        if runtime_url and (urlsplit(runtime_url).scheme or urlsplit(runtime_url).netloc):
            self.errors.append(f"remote runtime dependency: {runtime_url}")
        if tag == "video" and attrs.get("poster"):
            poster_url = urlsplit(attrs["poster"])
            if poster_url.scheme or poster_url.netloc:
                self.errors.append(f"remote video poster: {attrs['poster']}")
        for name in ("href", "src", "poster"):
            if attrs.get(name):
                self.links.append(attrs[name])
        for candidate in (attrs.get("srcset") or "").split(","):
            if candidate.strip():
                self.links.append(candidate.split()[0])
        if any(key.startswith("on") for key in attrs):
            self.errors.append("inline event handler")

    def handle_endtag(self, tag):
        if tag == "video":
            self.current_video = None


def image_size(path):
    data = path.read_bytes()
    if data.startswith(b"\x89PNG\r\n\x1a\n"):
        return struct.unpack(">II", data[16:24])
    if path.suffix.lower() == ".svg":
        svg = ET.fromstring(data)
        return int(svg.get("width")), int(svg.get("height"))
    raise ValueError(f"unsupported image format: {path.name}; use PNG or SVG")


def check_pages(root, errors):
    pages = {}
    for path in sorted(root.rglob("*.html")):
        name = path.relative_to(root).as_posix()
        page = pages[name] = Page()
        page.feed(path.read_text(encoding="utf-8"))
        errors.extend(f"{name}: {error}" for error in page.errors)
        if not (page.lang and page.main):
            errors.append(f"{name}: semantic page language/main missing")
    scripts = {"index.html": ["app.js", "docs.js"], "docs.html": ["app.js", "docs.js"], "downloads.html": ["docs.js"],
               "reference.html": ["app.js", "docs.js"], "desktop.html": ["app.js", "docs.js"]}
    for name, page in pages.items():
        if page.scripts != scripts.get(name, []):
            errors.append(f"{name}: unexpected or missing script dependency: {page.scripts}")
    for name, page in pages.items():
        for link in page.links:
            url = urlsplit(link)
            if url.scheme or url.netloc:
                continue
            target = (Path(name).parent / unquote(url.path)).as_posix() if url.path else name
            if not (root / target).is_file():
                errors.append(f"{name}: missing local resource: {link}")
            elif url.fragment and target in pages and unquote(url.fragment) not in pages[target].ids:
                errors.append(f"{name}: missing anchor: {link}")
        for src, width, height in page.images:
            if urlsplit(src).scheme or urlsplit(src).netloc:
                continue
            path = root / Path(name).parent / unquote(urlsplit(src).path)
            try:
                size = image_size(path) if path.is_file() else None
                if size and size != (int(width or 0), int(height or 0)):
                    errors.append(f"{name}: {src} is {size[0]}x{size[1]}, not {width}x{height}")
            except (ValueError, ET.ParseError, struct.error) as error:
                errors.append(f"{name}: cannot validate {src}: {error}")
        for video in page.videos:
            attrs = video["attrs"]
            label = attrs.get("id") or attrs.get("poster") or "video"
            prefix = f"{name}: {label}"
            for attribute in ("muted", "loop", "playsinline", "controls"):
                if attribute not in attrs:
                    errors.append(f"{prefix}: video needs {attribute}")
            if "autoplay" in attrs:
                errors.append(f"{prefix}: HTML autoplay bypasses motion and data preferences")
            dimensions = tuple(attrs.get(key) or "" for key in ("width", "height"))
            if any(not re.fullmatch(r"[1-9][0-9]*", value) for value in dimensions):
                errors.append(f"{prefix}: video needs positive width and height")
            if not attrs.get("poster"):
                errors.append(f"{prefix}: video needs a local poster")
            if not video["sources"]:
                errors.append(f"{prefix}: video has no media source")
            labelled_by = (attrs.get("aria-labelledby") or "").split()
            if not (attrs.get("aria-label") or "").strip() and not labelled_by:
                errors.append(f"{prefix}: video needs an accessible name")
            described_by = (attrs.get("aria-describedby") or "").split()
            if not described_by:
                errors.append(f"{prefix}: video needs an accessible description")
            for reference in labelled_by + described_by:
                if reference not in page.ids:
                    errors.append(f"{prefix}: missing accessible text: #{reference}")
    return pages


def boxes(data, start=0, end=None):
    """ISO BMFF boxes in data[start:end] as (kind, body start, box end)."""
    end = len(data) if end is None else end
    found = []
    offset = start
    while offset < end:
        if end - offset < 8:
            raise ValueError("incomplete MP4 box header")
        size, kind = struct.unpack_from(">I4s", data, offset)
        header = 8
        if size == 1:
            if end - offset < 16:
                raise ValueError("incomplete extended MP4 box header")
            size = struct.unpack_from(">Q", data, offset + 8)[0]
            header = 16
        elif size == 0:
            size = end - offset
        if size < header or offset + size > end:
            raise ValueError("invalid MP4 box size")
        found.append((kind, offset + header, offset + size))
        offset += size
    return found


def mp4_boxes(path):
    """Read top-level ISO BMFF boxes without a platform media dependency."""
    return [kind for kind, _, _ in boxes(path.read_bytes())]


def mp4_child(data, start, end, *path):
    for kind, body, box_end in boxes(data, start, end):
        if kind == path[0]:
            return (body, box_end) if len(path) == 1 else mp4_child(data, body, box_end, *path[1:])
    raise ValueError(f"missing MP4 {b'/'.join(path).decode()} box")


def mp4_video_timing(path):
    """The first video track's size, codec, timescale, duration, stts entries
    and sample sizes, read from moov without a media dependency."""
    data = Path(path).read_bytes()
    moov = mp4_child(data, 0, len(data), b"moov")
    for kind, start, end in boxes(data, *moov):
        if kind != b"trak":
            continue
        hdlr, _ = mp4_child(data, start, end, b"mdia", b"hdlr")
        if data[hdlr + 8:hdlr + 12] != b"vide":
            continue
        tkhd, _ = mp4_child(data, start, end, b"tkhd")
        # Width and height are the last two 16.16 fields of tkhd.
        width, height = struct.unpack_from(">II", data, tkhd + (88 if data[tkhd] == 1 else 76))
        mdhd, _ = mp4_child(data, start, end, b"mdia", b"mdhd")
        if data[mdhd] == 1:
            timescale, duration = struct.unpack_from(">IQ", data, mdhd + 20)
        else:
            timescale, duration = struct.unpack_from(">II", data, mdhd + 12)
        stbl = mp4_child(data, start, end, b"mdia", b"minf", b"stbl")
        stsd, _ = mp4_child(data, *stbl, b"stsd")
        codec = data[stsd + 12:stsd + 16].decode("latin-1")
        stts, _ = mp4_child(data, *stbl, b"stts")
        count = struct.unpack_from(">I", data, stts + 4)[0]
        entries = [struct.unpack_from(">II", data, stts + 8 + 8 * index) for index in range(count)]
        stsz, _ = mp4_child(data, *stbl, b"stsz")
        fixed, samples = struct.unpack_from(">II", data, stsz + 4)
        sizes = ([fixed] * samples if fixed else
                 [struct.unpack_from(">I", data, stsz + 12 + 4 * index)[0] for index in range(samples)])
        return {"width": width >> 16, "height": height >> 16, "codec": codec, "timescale": timescale,
                "duration": duration, "stts": entries, "samples": samples, "sizes": sizes}
    raise ValueError("no video track")


MIN_CLIP_FPS = 24
CODED_CHANGE_BYTES = 400
CODED_CHANGE_FRACTION = 0.8
CLIP_BYTE_BUDGET = 1_500_000
TOTAL_BYTE_BUDGET = 3_500_000


def check_clip_timing(prefix, path, clip, errors):
    """A clip must play at its manifest rate, constantly, and show new images.

    Samples under CODED_CHANGE_BYTES are x264 skip frames that repeat the last
    image. This assumes these scenes always move the camera preview and the
    pinned encoder settings: no B-frames, which code new images in a few
    hundred bytes, and a fixed GOP that keeps I-frames from skewing the
    count: a clip whose frames mostly repeat was not captured at its rate."""
    timing = mp4_video_timing(path)
    fps = clip.get("fps")
    duration = clip.get("duration_seconds")
    seconds = timing["duration"] / timing["timescale"] if timing["timescale"] else 0
    if len(timing["stts"]) != 1:
        errors.append(f"{prefix}: expected constant-frame-rate video")
    played = timing["samples"] / seconds if seconds else 0
    if type(fps) in (int, float) and abs(played - fps) > 0.05:
        errors.append(f"{prefix}: video plays at {played:.1f} fps; manifest says {fps}")
    if type(fps) not in (int, float) or fps < MIN_CLIP_FPS:
        errors.append(f"{prefix}: demonstration must play at {MIN_CLIP_FPS} fps or more")
    if timing["samples"] != clip.get("frame_count"):
        errors.append(f"{prefix}: video has {timing['samples']} frames; manifest says {clip.get('frame_count')}")
    if type(duration) in (int, float) and abs(seconds - duration) > 0.05:
        errors.append(f"{prefix}: video lasts {seconds:.2f} s; manifest says {duration}")
    if (timing["width"], timing["height"]) != (clip.get("width"), clip.get("height")):
        errors.append(f"{prefix}: video is {timing['width']}x{timing['height']}; manifest says {clip.get('width')}x{clip.get('height')}")
    if seconds and type(fps) in (int, float):
        changes = sum(1 for size in timing["sizes"] if size >= CODED_CHANGE_BYTES) / seconds
        if changes < CODED_CHANGE_FRACTION * fps:
            errors.append(f"{prefix}: only {changes:.1f} fps of the {fps} fps clip show new images (repeated frames)")
    source = clip.get("source") if isinstance(clip.get("source"), dict) else {}
    for value, label in ((source.get("observed_fps"), "renderer capture rate"),
                         (clip.get("preview_motion_fps"), "camera preview change rate")):
        if type(value) not in (int, float) or value < MIN_CLIP_FPS:
            errors.append(f"{prefix}: {label} must be {MIN_CLIP_FPS} fps or more")
    size = path.stat().st_size
    if size != clip.get("size_bytes"):
        errors.append(f"{prefix}: file is {size:,} bytes; manifest says {clip.get('size_bytes')}")
    if size > CLIP_BYTE_BUDGET:
        errors.append(f"{prefix}: {size:,} bytes exceeds the {CLIP_BYTE_BUDGET:,}-byte clip budget")


def check_media(root, pages, errors):
    """Anchor recorded-media metadata to checked-in files and accessible embeds."""
    directory = root / "media"
    total = sum(path.stat().st_size for path in directory.glob("*.mp4"))
    if total > TOTAL_BYTE_BUDGET:
        errors.append(f"media: clips total {total:,} bytes, over the {TOTAL_BYTE_BUDGET:,}-byte budget")
    manifest_path = directory / "manifest.json"
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        errors.append(f"media/manifest.json: cannot read recording provenance: {error}")
        return
    if not isinstance(manifest, dict):
        errors.append("media/manifest.json: expected a recording manifest object")
        return
    for key in ("method", "source_note", "demo"):
        if not isinstance(manifest.get(key), str) or not manifest[key].strip():
            errors.append(f"media/manifest.json: missing recording provenance: {key}")
    binary_digest = manifest.get("binary_sha256")
    if not isinstance(binary_digest, str) or not re.fullmatch(r"[0-9a-f]{64}", binary_digest):
        errors.append("media/manifest.json: missing recording binary SHA-256")
    clips = manifest.get("clips")
    if not isinstance(clips, list) or len(clips) < 2:
        errors.append("media/manifest.json: expected at least two software demonstration clips")
        return

    recorded = {}
    for clip in clips:
        if not isinstance(clip, dict):
            errors.append("media/manifest.json: invalid clip entry")
            continue
        filename = clip.get("file", "")
        prefix = f"media/{filename or 'manifest.json'}"
        if not isinstance(filename, str) or Path(filename).name != filename or Path(filename).suffix != ".mp4":
            errors.append(f"{prefix}: expected a local MP4 filename")
            continue
        if filename in recorded:
            errors.append(f"{prefix}: duplicate recorded clip")
        recorded[filename] = clip
        for key, hash_key, suffix in (("file", "sha256", ".mp4"), ("poster", "poster_sha256", ".png")):
            value = clip.get(key, "")
            if not isinstance(value, str) or Path(value).name != value or Path(value).suffix != suffix:
                errors.append(f"{prefix}: {key} must name a local {suffix} file")
                continue
            path = directory / value
            digest = clip.get(hash_key, "")
            if not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest):
                errors.append(f"{prefix}: invalid {hash_key}")
            elif not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != digest:
                errors.append(f"{prefix}: missing or modified {key}: {value}")
        dimensions = (clip.get("width"), clip.get("height"))
        if any(type(value) is not int or value <= 0 for value in dimensions):
            errors.append(f"{prefix}: invalid video dimensions")
        poster = directory / str(clip.get("poster") or "")
        if poster.is_file():
            try:
                if image_size(poster) != dimensions:
                    errors.append(f"{prefix}: poster dimensions differ from recorded video")
            except (ValueError, struct.error) as error:
                errors.append(f"{prefix}: cannot validate poster: {error}")
        duration = clip.get("duration_seconds")
        if type(duration) not in (int, float) or not 3 <= duration <= 12:
            errors.append(f"{prefix}: demonstration must be 3–12 seconds long")
        if type(clip.get("fps")) not in (int, float) or not 1 <= clip["fps"] <= 60:
            errors.append(f"{prefix}: invalid recorded frame rate")
        if clip.get("codec") != "h264" or clip.get("pixel_format") != "yuv420p" or clip.get("audio") is not False:
            errors.append(f"{prefix}: expected silent H.264 video with yuv420p pixels")
        if not isinstance(clip.get("description"), str) or not clip["description"].strip():
            errors.append(f"{prefix}: missing clip description")
        source = clip.get("source", {})
        if not isinstance(source, dict) or any(not source.get(key) for key in ("method", "scene")):
            errors.append(f"{prefix}: missing native recording source details")
        path = directory / filename
        if path.is_file():
            try:
                top = mp4_boxes(path)
                if not top or top[0] != b"ftyp" or b"moov" not in top or b"mdat" not in top:
                    errors.append(f"{prefix}: invalid MP4 container")
                elif top.index(b"moov") > top.index(b"mdat"):
                    errors.append(f"{prefix}: MP4 metadata must precede media for progressive playback")
                else:
                    check_clip_timing(prefix, path, clip, errors)
            except (ValueError, struct.error) as error:
                errors.append(f"{prefix}: cannot validate MP4 container: {error}")

    embedded = set()
    for name, page in pages.items():
        for video in page.videos:
            attrs = video["attrs"]
            for src in video["sources"]:
                url = urlsplit(src)
                path = (root / Path(name).parent / unquote(url.path)).resolve()
                filename = path.name
                if url.scheme or url.netloc or path.parent != directory.resolve() or filename not in recorded:
                    errors.append(f"{name}: video source lacks local recording provenance: {src}")
                    continue
                clip = recorded[filename]
                embedded.add(filename)
                if (attrs.get("width"), attrs.get("height")) != (str(clip.get("width")), str(clip.get("height"))):
                    errors.append(f"{name}: {src}: HTML dimensions differ from recording")
                poster = (root / Path(name).parent / unquote(urlsplit(attrs.get("poster") or "").path)).resolve()
                if poster != (directory / str(clip.get("poster") or "")).resolve():
                    errors.append(f"{name}: {src}: poster differs from recording manifest")
    for filename in recorded.keys() - embedded:
        errors.append(f"media/{filename}: recorded demonstration is not embedded in a page")
    check_clip_copy(root, pages, recorded, errors)


NUMBERS = {"one": 1, "two": 2, "three": 3, "four": 4, "five": 5, "six": 6, "seven": 7, "eight": 8}


def plain_text(markup):
    return re.sub(r"\s+", " ", unescape(re.sub(r"<[^>]+>", " ", markup))).strip()


def check_clip_copy(root, pages, recorded, errors):
    """The copy beside a clip must count the cameras the recording actually shows."""
    for name in pages:
        markup = (root / name).read_text(encoding="utf-8")
        for block in re.findall(r"<article\b.*?</article>", markup, re.S):
            for filename, clip in recorded.items():
                if f"media/{filename}" not in block:
                    continue
                cameras = (clip.get("source") or {}).get("cameras")
                caption = plain_text(" ".join(re.findall(r"<figcaption\b.*?</figcaption>", block, re.S)))
                body = plain_text(re.sub(r"<figure\b.*?</figure>", " ", block, flags=re.S))
                stated = re.findall(r"\b(\w+) simulator cameras?\b", caption, re.I)
                if "simulator camera" in caption.lower() and not stated:
                    errors.append(f"media/{filename}: caption mentions simulator cameras without a count")
                stated += re.findall(r"\b(\w+) (?:simulator )?cameras\b", body, re.I)
                for word in stated:
                    count = int(word) if word.isdigit() else NUMBERS.get(word.lower())
                    if count is None:
                        errors.append(f"media/{filename}: cannot read the camera count in '{word} ... cameras'")
                    elif count != cameras:
                        errors.append(f"media/{filename}: {name} says {word} simulator camera(s) but manifest source.cameras is {cameras}")


def check_image_provenance(root, pages, errors):
    """Responsive candidates exist at their stated widths and derived images match their manifest."""
    manifest_path = root / "images" / "docs" / "manifest.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8")) if manifest_path.is_file() else {}
    for entry in manifest.get("derived", []):
        path = manifest_path.parent / str(entry.get("file", ""))
        source = manifest_path.parent / str(entry.get("derived_from", ""))
        if not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != entry.get("sha256"):
            errors.append(f"images/docs/{entry.get('file')}: missing or modified derived image; run scripts/docs-screenshots.py --derive-only")
        elif not source.is_file() or hashlib.sha256(source.read_bytes()).hexdigest() != entry.get("derived_from_sha256"):
            errors.append(f"images/docs/{entry.get('file')}: its source {entry.get('derived_from')} changed; run scripts/docs-screenshots.py --derive-only")
    for name, page in pages.items():
        for srcset, width, height, priority, src in page.srcsets:
            for candidate in filter(None, (part.strip() for part in srcset.split(","))):
                parts = candidate.split()
                path = root / Path(name).parent / unquote(urlsplit(parts[0]).path)
                if len(parts) != 2 or not re.fullmatch(r"[1-9][0-9]*w", parts[1]):
                    errors.append(f"{name}: srcset candidate needs a width descriptor: {candidate}")
                    continue
                if not path.is_file():
                    continue  # Reported as a missing local resource.
                try:
                    size = image_size(path)
                except (ValueError, ET.ParseError, struct.error) as error:
                    errors.append(f"{name}: cannot validate {parts[0]}: {error}")
                    continue
                if size[0] != int(parts[1][:-1]):
                    errors.append(f"{name}: {parts[0]} is {size[0]} pixels wide, not {parts[1]}")
                if width and height and abs(size[0] * int(height) - size[1] * int(width)) > size[0]:
                    errors.append(f"{name}: {parts[0]} does not match the {width}x{height} aspect ratio")
            path = root / Path(name).parent / unquote(urlsplit(src).path)
            if priority == "high" and path.is_file() and path.stat().st_size > 300_000:
                print(f"warning: {name}: high-priority image {src} is {path.stat().st_size // 1000} KB", file=sys.stderr)


def code_lines(markup):
    for block in re.findall(r"<pre><code>(.*?)</code></pre>", markup, re.S):
        yield from (line.strip() for line in plain_lines(block))


def plain_lines(block):
    return unescape(re.sub(r"<[^>]+>", "", block)).splitlines()


def check_landing(root, errors):
    """Keep the landing page's first impression and examples honest."""
    index = (root / "index.html").read_text(encoding="utf-8")
    docs = (root / "docs.html").read_text(encoding="utf-8")
    title = re.search(r"<h1\b[^>]*>(.*?)</h1>", index, re.S)
    if title and "<br" in title[1]:
        errors.append("index.html: the hero heading must wrap on its own; remove <br>")
    quickstart = re.search(r'<section id="quickstart".*?</section>', index, re.S)
    for line in code_lines(quickstart[0] if quickstart else ""):
        if re.match(r"capturefab\b.*\bdiscover\b", line) and "--simulate" not in line:
            errors.append(f"index.html: the simulator quickstart must discover with --simulate: {line}")
    # Landing examples are copied from documented ones, so docs coverage keeps them current.
    documented = set(code_lines(docs))
    for line in code_lines(index):
        if line.startswith("capturefab ") and line not in documented:
            errors.append(f"index.html: CLI example is not in a docs.html example: {line}")
    faq = re.search(r'<div class="faq">\s*<details><summary>(.*?)</summary>', index, re.S)
    if faq and re.search(r"\b(version|upgrad\w*|changed)\b|\d+\.\d+", faq[1], re.I):
        errors.append(f"index.html: the FAQ should open with a newcomer question, not release news: {faq[1]}")
    if index.count("development build") > 1:
        errors.append("index.html: say 'development build' once at most; captions already link their provenance")
    for manifest in (root / "media" / "manifest.json", root / "images" / "docs" / "manifest.json"):
        version = json.loads(manifest.read_text(encoding="utf-8")).get("app_version") if manifest.is_file() else None
        note = re.search(r'<p class="media-note">(.*?)</p>', index, re.S)
        if version and (not note or version not in note[1]):
            errors.append(f"index.html: the media note must name version {version}, recorded in {manifest.relative_to(root)}")


def check_unused_css(root, errors):
    """Every class selector in the site stylesheets is used by a page, a script or the release template."""
    # Classes that appear only in the markup docs.js or app.js builds at runtime.
    runtime = set()
    sources = [*root.glob("*.html"), *root.glob("*.js"), Path(__file__).with_name("site-release.py")]
    used = "\n".join(path.read_text(encoding="utf-8") for path in sources)
    for sheet in ("style.css", "docs.css"):
        css = re.sub(r"/\*.*?\*/", "", (root / sheet).read_text(encoding="utf-8"), flags=re.S)
        selectors = re.sub(r"\{[^{}]*\}", "{}", css)
        for name in sorted(set(re.findall(r"\.(-?[A-Za-z_][\w-]*)", selectors)) - runtime):
            if not re.search(rf"(?<![\w-]){re.escape(name)}(?![\w-])", used):
                errors.append(f"{sheet}: unused class selector .{name}")


# References a browser resolves against the module that holds them. Each pattern
# captures the quote and the literal specifier.
MODULE_REFERENCES = (
    # import … from './x.mjs' and export … from './x.mjs', also across lines.
    re.compile(r"""(?m)^[ \t]*(?:import|export)\b[^;'"`]*?\bfrom\s*(['"])([^'"\n]*)\1"""),
    # import './x.mjs'
    re.compile(r"""(?m)^[ \t]*import\s*(['"])([^'"\n]*)\1"""),
    # import('./x.mjs')
    re.compile(r"""\bimport\s*\(\s*(['"])([^'"\n]*)\1\s*[,)]"""),
)
# new URL('./x', import.meta.url): workers, styles, WebAssembly and other assets.
ASSET_REFERENCE = re.compile(r"""\bnew\s+URL\s*\(\s*(['"])([^'"\n]*)\1\s*,\s*import\.meta\.url\s*\)""")
# A module's own file paths, also when a helper resolves them: fetchText('./x.css').
ASSET_LITERAL = re.compile(r"""(['"])(\.\.?/[^'"\s]+\.(?:mjs|js|css|wasm|json))\1""")
COMPUTED_IMPORT = re.compile(r"""\bimport\((?!\s*['")])""")
# In a module, a literal fetch() or Worker URL resolves against the page, not the module.
PAGE_RELATIVE = re.compile(r"""\b(fetch|new\s+(?:Shared)?Worker)\s*\(\s*(['"`])""")


def module_reference_error(root, path, specifier, module):
    """Why the browser cannot load this reference from the site, or None."""
    url = urlsplit(specifier)
    if url.scheme or url.netloc:
        return f"remote module or asset: {specifier}"
    if specifier.startswith("/"):
        return f"root-relative path: {specifier}; use a path relative to the module"
    if module and not specifier.startswith(("./", "../")):
        return f"bare module specifier: {specifier}; browsers load only paths such as ./{specifier}"
    target = (path.parent / unquote(url.path)).resolve()
    if not target.is_relative_to(root.resolve()):
        return f"path outside web/: {specifier}"
    if target.is_file() or (target.is_dir() and (url.path in (".", "") or url.path.endswith("/"))):
        return None
    return f"missing module or asset: {specifier}"


def check_modules(root, errors):
    """Every module and asset the site's scripts load resolves to a file in web/.

    Classic scripts load modules only with import(); modules also use static imports,
    new URL(…, import.meta.url) and literal ./ paths to their own files. Nothing else
    validates these paths, and a deployed page would only find a broken one when a
    visitor clicks."""
    sources = [(path, False) for path in sorted(root.glob("*.js"))] + [(path, True) for path in sorted(root.rglob("*.mjs"))]
    for path, module in sources:
        name = path.relative_to(root).as_posix()
        text = path.read_text(encoding="utf-8")
        references = [(match[2], True) for pattern in (MODULE_REFERENCES if module else MODULE_REFERENCES[2:]) for match in pattern.finditer(text)]
        if module:
            references += [(match[2], False) for pattern in (ASSET_REFERENCE, ASSET_LITERAL) for match in pattern.finditer(text)]
        # A path that is also imported follows the stricter module rules, and is reported once.
        imported = {}
        for specifier, is_import in references:
            imported[specifier] = imported.get(specifier, False) or is_import
        for specifier, is_import in sorted(imported.items()):
            error = module_reference_error(root, path, specifier, is_import)
            if error:
                errors.append(f"{name}: {error}")
        if COMPUTED_IMPORT.search(text):
            errors.append(f"{name}: import() with a computed specifier; use a string literal so this check can resolve it")
        if module:
            for match in PAGE_RELATIVE.finditer(text):
                call = re.sub(r"\s+", " ", match[1])
                errors.append(f"{name}: {call}() with a literal URL resolves against the page; pass new URL('./…', import.meta.url)")


class TryLaunchers(HTMLParser):
    """The markup app.js expects for "Try with your webcam"."""

    def __init__(self):
        super().__init__()
        self.launchers = []
        self.notes = []
        self.statuses = set()
        self.scripts = []

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if "data-try-launch" in attrs:
            self.launchers.append((tag, attrs))
        if "data-try-note" in attrs:
            self.notes.append((tag, attrs))
        if attrs.get("role") == "status" and attrs.get("id"):
            self.statuses.add(attrs["id"])
        if tag == "script":
            self.scripts.append(attrs.get("src", ""))


def check_try_launchers(root, errors):
    """Launchers stay hidden until app.js finds that the workbench can load, and each
    names the role="status" element where app.js reports why it could not start."""
    for path in sorted(root.glob("*.html")):
        page = TryLaunchers()
        page.feed(path.read_text(encoding="utf-8"))
        if (page.launchers or page.notes) and "app.js" not in page.scripts:
            errors.append(f"{path.name}: in-browser launchers need app.js")
        for tag, attrs in page.launchers:
            if tag != "button" or attrs.get("type") != "button" or "hidden" not in attrs:
                errors.append(f"{path.name}: a data-try-launch element must be <button type=\"button\" hidden>")
            if attrs.get("data-try-launch") not in ("", None, "welcome"):
                errors.append(f"{path.name}: data-try-launch is empty or \"welcome\", not {attrs.get('data-try-launch')!r}")
            if attrs.get("aria-describedby") not in page.statuses:
                errors.append(f"{path.name}: a launcher's aria-describedby must name a role=\"status\" element")
        for tag, attrs in page.notes:
            if "hidden" not in attrs:
                errors.append(f"{path.name}: a data-try-note <{tag}> must start hidden")


def check_try_build(repo, errors):
    """The committed in-browser WebAssembly matches its manifest and the web-try sources."""
    path = Path(__file__).with_name("build-try.py")
    if not path.is_file():
        errors.append("in-browser try: scripts/build-try.py is missing, so web/try/capturefab.wasm cannot be verified")
        return
    try:
        spec = importlib.util.spec_from_file_location("build_try", path)
        build = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(build)
        problems = build.check_manifest(repo)
    except (Exception, SystemExit) as error:  # A broken checker fails the site check with its reason.
        errors.append(f"in-browser try: scripts/build-try.py cannot verify web/try/capturefab.wasm: {type(error).__name__}: {error}")
        return
    if not isinstance(problems, list) or not all(isinstance(problem, str) for problem in problems):
        errors.append(f"in-browser try: scripts/build-try.py check_manifest returned {type(problems).__name__}, not a list of messages")
        return
    errors.extend(f"in-browser try: {problem}" for problem in problems)


def check_svgs(root, errors):
    for path in sorted(root.rglob("*.svg")):
        for element in ET.parse(path).iter():
            tag = element.tag.rsplit("}", 1)[-1]
            remote = [v for k, v in element.attrib.items() if k.rsplit("}", 1)[-1] == "href" and urlsplit(v).scheme]
            if tag in ("script", "foreignObject") or remote or any(k.startswith("on") for k in element.attrib):
                errors.append(f"{path.name}: active or remote SVG content in <{tag}>")


def check_manifest(root, errors):
    manifest = json.loads((root / "releases.json").read_text(encoding="utf-8"))
    if manifest.get("schema_version") != 1 or not isinstance(manifest.get("releases"), list):
        errors.append("releases.json: expected schema_version 1 with a releases list")
        return []
    versions = [r["version"] for r in manifest["releases"]]
    if manifest["latest"] is not None and manifest["latest"] not in versions:
        errors.append("releases.json: latest is not a listed release")
    for release in manifest["releases"]:
        notable = release.get("highlights", [])
        if not isinstance(notable, list) or len(notable) > 6 or any(
            not isinstance(line, str) or not line.strip() or len(line) > 200
            or any(ord(c) < 32 or 0x7F <= ord(c) < 0xA0 or 0x202A <= ord(c) <= 0x202E or 0x2066 <= ord(c) <= 0x2069 for c in line)
            for line in notable
        ):
            errors.append(f"releases.json {release.get('version')}: highlights must be at most 6 plain lines of at most 200 characters")
        targets = set()
        for asset in release["assets"]:
            identity = (asset["os"], asset["arch"])
            if identity in targets:
                errors.append(f"duplicate download selection: {identity}")
            targets.add(identity)
            if asset["os"] not in {"macos", "windows", "linux"} or asset["arch"] not in {"x86_64", "aarch64", "armv7"}:
                errors.append(f"unknown download selection: {identity}")
            if not asset.get("available"):
                continue
            digest = asset.get("sha256", "")
            if not re.fullmatch(r"[0-9a-fA-F]{64}", digest) or not asset.get("size", 0) > 0 or not asset.get("name"):
                errors.append(f"{asset.get('name')}: available asset needs a name, size and SHA-256")
            url = urlsplit(asset.get("url", ""))
            if url.scheme not in {"", "https", "http"}:
                errors.append(f"{asset.get('name')}: unsafe download URL")
            elif not url.scheme:
                path = root / unquote(url.path)
                if not path.is_file() or path.stat().st_size != asset["size"] or hashlib.sha256(path.read_bytes()).hexdigest() != digest.lower():
                    errors.append(f"missing or modified published asset: {path}")
    return versions


def check_processors(root, errors):
    """The download selector offers only processors scripts/release.py builds."""
    spec = importlib.util.spec_from_file_location("release", Path(__file__).with_name("release.py"))
    release = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(release)
    built = {arch for _, arch in release.TARGETS}
    select = re.search(r'<select id="arch-select"[^>]*>(.*?)</select>', (root / "index.html").read_text(encoding="utf-8"), re.S)
    offered = set(re.findall(r'<option value="([^"]+)"', select[1])) if select else set()
    if not offered or offered - built:
        errors.append(f"index.html: the processor selector offers builds release.py does not make: {sorted(offered - built) or 'none offered'}")


def check_feed(root, versions, errors):
    feed = ET.parse(root / "feed.xml").getroot()
    if feed.tag != f"{ATOM}feed":
        errors.append("feed.xml: not an Atom feed")
        return
    if any(feed.find(path) is None for path in (f"{ATOM}author/{ATOM}name", f"{ATOM}link[@rel='self']")):
        errors.append("feed.xml: feed author or self link missing")
    entries = feed.findall(f"{ATOM}entry")
    if len(entries) != len(versions):
        errors.append(f"feed.xml: {len(entries)} entries for {len(versions)} releases")
    for element in [feed, *entries]:
        if any(element.find(ATOM + name) is None for name in ("id", "title", "updated")):
            errors.append("feed.xml: feed or entry id, title or updated missing")
        elif not TIMESTAMP.fullmatch(element.findtext(f"{ATOM}updated")):
            errors.append("feed.xml: updated is not an RFC 3339 timestamp")


def check_generated(root, errors):
    spec = importlib.util.spec_from_file_location("site_release", Path(__file__).with_name("site-release.py"))
    site = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(site)
    try:
        pages = site.pages(root)
    except (ValueError, KeyError) as error:
        errors.append(f"cannot generate release pages: {error}")
        return
    for path, text in pages.items():
        if not path.is_file() or path.read_text(encoding="utf-8") != text:
            errors.append(f"{path.name} is stale; run scripts/site-release.py")


def site_release():
    spec = importlib.util.spec_from_file_location("site_release", Path(__file__).with_name("site-release.py"))
    site = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(site)
    return site


def check_installers(root, errors):
    """The generated installers offer exactly the manifest's published builds, with their checksums."""
    manifest = json.loads((root / "releases.json").read_text(encoding="utf-8"))
    canonical = re.search(r'<link rel="canonical" href="([^"]+)"', (root / "index.html").read_text(encoding="utf-8"))
    site = canonical[1].rstrip("/") + "/" if canonical else ""
    latest = [r for r in manifest["releases"] if r["version"] == manifest.get("latest")]
    available = [a for r in latest for a in r["assets"] if a.get("available")]
    for name, systems, pattern in (("install.sh", {"macos", "linux"}, r"url='([^']+)' expected='([0-9a-f]{64})'"),
                                   ("install.ps1", {"windows"}, r"Url = '([^']+)'; Sha256 = '([0-9a-f]{64})'")):
        path = root / name
        if not path.is_file():
            errors.append(f"{name}: missing; run scripts/site-release.py")
            continue
        offered = set(re.findall(pattern, path.read_text(encoding="utf-8")))
        expected = {(urljoin(site, a.get("url", "")), a.get("sha256", "").lower()) for a in available if a["os"] in systems}
        if offered != expected:
            errors.append(f"{name}: offers {sorted(offered - expected)} and omits {sorted(expected - offered)}")
    script = root / "install.sh"
    if script.is_file() and shutil.which("sh"):
        result = subprocess.run(["sh", "-n", str(script)], capture_output=True, text=True)
        if result.returncode:
            errors.append(f"install.sh: shell syntax error: {result.stderr.strip()}")
        if shutil.which("shellcheck"):
            result = subprocess.run(["shellcheck", "-s", "sh", str(script)], capture_output=True, text=True)
            if result.returncode:
                errors.append(f"install.sh: shellcheck: {result.stdout.strip()}")
    if "sudo" in re.sub(r"#.*", "", script.read_text(encoding="utf-8") if script.is_file() else ""):
        errors.append("install.sh: must never run sudo")


def fixture_archive(directory, system, arch, kind=None):
    """A release archive whose capturefab prints a version, laid out like scripts/release.py's.

    macOS takes kind "zip" (releases up to 0.4.0) or "dmg" (later releases, built with hdiutil)."""
    stem = f"capturefab-9.9.9-{system}-{arch}"
    stub = b"#!/bin/sh\necho 'capturefab 9.9.9'\n"
    if system == "macos" and kind == "dmg":
        stage = directory / f"{stem}-stage"
        executable = stage / "Capturefab.app/Contents/MacOS/capturefab"
        executable.parent.mkdir(parents=True)
        executable.write_bytes(stub)
        executable.chmod(0o755)
        os.symlink("/Applications", stage / "Applications")
        path = directory / f"{stem}.dmg"
        subprocess.run(["hdiutil", "create", "-quiet", "-volname", "Capturefab 9.9.9", "-srcfolder", str(stage), "-format", "UDZO", "-ov", str(path)],
                       check=True, capture_output=True, timeout=120)
        shutil.rmtree(stage)
    elif system == "macos":
        path = directory / f"{stem}.zip"
        with zipfile.ZipFile(path, "w") as archive:
            info = zipfile.ZipInfo(f"{stem}/Capturefab.app/Contents/MacOS/capturefab")
            info.external_attr = 0o100755 << 16
            archive.writestr(info, stub)
    else:
        path = directory / f"{stem}.tar.gz"
        with tarfile.open(path, "w:gz") as archive:
            for name, data, mode in ((f"{stem}/capturefab", stub, 0o755),
                                     (f"{stem}/share/applications/capturefab.desktop", b"[Desktop Entry]\nExec=capturefab\n", 0o644)):
                info = tarfile.TarInfo(name)
                info.size, info.mode = len(data), mode
                archive.addfile(info, io.BytesIO(data))
    return path


def check_installer_runs(errors):
    """Install from a local server, then refuse a corrupted download and an empty release."""
    system = {"Darwin": "macos", "Linux": "linux"}.get(platform.system())
    if not system or not shutil.which("sh") or (system == "macos" and not shutil.which("ditto")):
        return
    kinds = [None]
    if system == "macos":
        # A disk image needs hdiutil to build and mount; elsewhere only the .zip case runs.
        kinds = ["zip"] + (["dmg"] if shutil.which("hdiutil") else [])
    for kind in kinds:
        check_installer_run(errors, system, kind)


def check_installer_run(errors, system, kind):
    site = site_release()
    label = f"install.sh ({kind})" if kind else "install.sh"
    with tempfile.TemporaryDirectory() as temp:
        temp = Path(temp)
        served = temp / "served"
        served.mkdir()
        assets = []
        for arch in ("x86_64", "aarch64"):
            try:
                path = fixture_archive(served, system, arch, kind)
            except (OSError, subprocess.SubprocessError) as error:
                # hdiutil can be present but unusable, as in some sandboxes.
                print(f"skipping the {label} fixture: {error}", file=sys.stderr)
                return
            assets.append(dict(name=path.name, os=system, arch=arch, available=True, size=path.stat().st_size,
                               sha256=hashlib.sha256(path.read_bytes()).hexdigest(), url=f"https://example.invalid/{path.name}"))
        manifest = dict(schema_version=1, latest="9.9.9", releases=[dict(version="9.9.9", date="2026-01-01", assets=assets)])
        script = temp / "install.sh"
        script.write_text(site.installers(manifest, "https://capturefab.example/")[0], encoding="utf-8")
        server = ThreadingHTTPServer(("127.0.0.1", 0), partial(QuietHandler, directory=str(served)))
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            def install(name, prefix=None):
                home = temp / name
                home.mkdir()
                # The script's own temporary folder, which must be gone (and any image detached) afterwards.
                (home / "tmp").mkdir()
                env = dict(os.environ, HOME=str(home), TMPDIR=str(home / "tmp"), XDG_DATA_HOME=str(home / "share"),
                           CAPTUREFAB_INSTALL_BASE_URL=f"http://127.0.0.1:{server.server_port}")
                result = subprocess.run(["sh", str(script), "--prefix", prefix or str(home / "apps"), "--bin-dir", str(home / "bin")],
                                        capture_output=True, text=True, env=env, timeout=120, cwd=home)
                if kind == "dmg":
                    # A disk image still mounted in the temporary folder would also stop its removal.
                    root = str((home / "tmp").resolve())
                    mounts = re.findall(r" on (.+?) \(", subprocess.run(["mount"], capture_output=True, text=True).stdout)
                    for mount in [m for m in mounts if m.startswith(root + "/")]:
                        errors.append(f"{label}: left the disk image mounted at {mount}")
                        subprocess.run(["hdiutil", "detach", "-force", mount], capture_output=True)
                if any((home / "tmp").iterdir()):
                    errors.append(f"{label}: left {sorted(p.name for p in (home / 'tmp').iterdir())} in its temporary folder")
                return home, result
            # A relative prefix must still leave a working command symlink.
            home, result = install("good", "apps")
            command = home / "bin" / "capturefab"
            if result.returncode or not command.is_symlink():
                errors.append(f"{label}: fixture install failed: {result.stderr.strip() or result.stdout.strip()}")
            elif "capturefab 9.9.9" not in result.stdout:
                errors.append(f"{label}: did not run the installed capturefab --version")
            elif system == "macos" and command.resolve() != (home / "apps/Capturefab.app/Contents/MacOS/capturefab").resolve():
                errors.append(f"{label}: the command must link to the app's executable, not {os.readlink(command)}")
            elif system == "linux" and 'Exec="' not in (home / "share/applications/capturefab.desktop").read_text():
                errors.append(f"{label}: the launcher entry must name the installed command")
            for path in served.iterdir():
                path.write_bytes(path.read_bytes() + b"tampered")
            home, result = install("corrupt")
            if result.returncode == 0 or "checksum mismatch" not in result.stderr:
                errors.append(f"{label}: a corrupted download must fail with a checksum mismatch")
            if (home / "bin").exists() or (home / "apps").exists():
                errors.append(f"{label}: a corrupted download must install nothing")
        finally:
            server.shutdown()
            server.server_close()
        if kind == "dmg":
            return
        empty = temp / "empty.sh"
        empty.write_text(site.installers(dict(schema_version=1, latest=None, releases=[]), "https://capturefab.example/")[0], encoding="utf-8")
        result = subprocess.run(["sh", str(empty)], capture_output=True, text=True, env=dict(os.environ, HOME=str(temp)), timeout=30)
        if result.returncode == 0 or "no published release" not in result.stderr:
            errors.append("install.sh: without a release it must fail with 'no published release'")


class QuietHandler(SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass


def check_signature_labels(root, errors):
    """The download card in app.js names each signature as downloads.html does."""
    site = site_release()
    script = (root / "app.js").read_text(encoding="utf-8")
    found = re.search(r"const signatureNotes = \{(.*?)\};", script)
    labels = dict(re.findall(r"'?([a-z-]+)'?: '([^']*)'", found[1])) if found else {}
    expected = {key: site.NOTES[key] for key in ("apple-notarized", "apple-developer-id", "authenticode", "unsigned")}
    if labels != expected:
        errors.append(f"app.js: signatureNotes {labels} must match the signature labels of NOTES in site-release.py {expected}")


def check_min_os(root, errors):
    """The download cards repeat docs.html's minimum operating system table."""
    site = site_release()
    docs = (root / "docs.html").read_text(encoding="utf-8")
    table = re.search(r"<caption>Minimum operating system versions</caption>(.*?)</table>", docs, re.S)
    rows = {plain_text(build): plain_text(minimum) for build, minimum in re.findall(r"<tr><td>(.*?)</td><td>(.*?)</td></tr>", table[1] if table else "", re.S)}
    names = {("macos", "aarch64"): "macOS ARM64", ("macos", "x86_64"): "macOS x64", ("windows", "x86_64"): "Windows x64",
             ("windows", "aarch64"): "Windows ARM64", ("linux", "x86_64"): "Linux x64 and ARM64", ("linux", "aarch64"): "Linux x64 and ARM64"}
    for key, card in site.MIN_OS.items():
        documented = rows.get(names[key], "")
        # The card's leading version must appear in the docs row (macOS 11+ ↔ macOS 11.0, glibc 2.28+ ↔ glibc 2.28).
        version = re.search(r"[\d.]+|Windows 11 on ARM", card)[0]
        if not documented or version not in documented:
            errors.append(f"site-release.py: MIN_OS {key} says '{card}', but docs.html#installation says '{documented}'")
    downloads = (root / "downloads.html").read_text(encoding="utf-8")
    mac = re.search(r'<article class="platform-download-card" data-os="macos">.*?</article>', downloads, re.S)
    if mac and mac[0].find("Apple silicon") > mac[0].find("Intel Mac"):
        errors.append("downloads.html: the macOS card must list Apple silicon before Intel")
    for card in re.findall(r"<li data-os=.*?</li>", downloads, re.S):
        if "Unsigned" in card and 'href="docs.html#first-launch"' not in card:
            errors.append("downloads.html: an unsigned build must link to its first-launch steps")


def check_search_index(root, errors):
    spec = importlib.util.spec_from_file_location("docs_search", Path(__file__).with_name("docs-search.py"))
    search = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(search)
    try:
        text = search.generated(root)
    except (ValueError, OSError) as error:
        errors.append(f"cannot generate documentation search: {error}")
        return
    path = root / "search-index.json"
    if not path.is_file() or path.read_text(encoding="utf-8") != text:
        errors.append("search-index.json is stale; run scripts/docs-search.py")


def check_distribution(root, errors):
    """The site distributes every download itself: GitHub only builds releases."""
    for path in sorted(root.rglob("*")):
        if path.is_file() and path.suffix in {".html", ".md", ".txt", ".json", ".xml", ".js", ".mjs", ".css", ".sh", ".ps1"}:
            if "github.com/capturefab" in path.read_text(encoding="utf-8", errors="replace"):
                errors.append(f"{path.relative_to(root)}: links to the source repository; link to a page on the site")
    manifest = json.loads((root / "releases.json").read_text(encoding="utf-8"))
    for release in manifest.get("releases", []):
        urls = [release.get(k) for k in ("url", "checksums_url", "checksums_signature_url", "source_url", "sbom_url")]
        urls += [item.get("url") for item in [*release.get("assets", []), release.get("source") or {}, *release.get("media_sources", [])]]
        for url in filter(None, urls):
            if not url.startswith("https://capturefab.com/"):
                errors.append(f"releases.json {release.get('version')}: {url} is not served by capturefab.com")


def check_theme_contrast(root, errors):
    """Check real theme tokens used by small text, buttons and control outlines."""
    css = (root / "style.css").read_text(encoding="utf-8")
    palettes = re.findall(r":root\{([^{}]*)\}", css)
    if len(palettes) != 2:
        errors.append("style.css: expected one light palette and one dark palette")
        return

    def luminance(color):
        channels = [int(color[i:i + 2], 16) / 255 for i in (1, 3, 5)]
        linear = [c / 12.92 if c <= 0.04045 else ((c + 0.055) / 1.055) ** 2.4 for c in channels]
        return sum(c * weight for c, weight in zip(linear, (0.2126, 0.7152, 0.0722)))

    for mode, palette in zip(("light", "dark"), palettes):
        tokens = dict(re.findall(r"--([\w-]+):(#[0-9a-fA-F]{6})(?=;|$)", palette))
        # Tokens inherited from light mode still apply in dark mode.
        if mode == "light":
            inherited = tokens.copy()
        else:
            tokens = inherited | tokens
        surfaces = ("bg", "panel", "field", "accent-soft")
        pairs = [(text, surface, 4.5) for text in ("text", "muted", "faint", "code-text", "accent-text") for surface in surfaces]
        pairs += [("on-accent", background, 4.5) for background in ("accent", "accent-hover")]
        pairs += [("success", "bg", 4.5), ("warning", "warning-soft", 4.5), ("warning", "bg", 4.5)]
        pairs += [("canvas-text", "canvas", 4.5), ("canvas-muted", "canvas", 4.5)]
        pairs += [("control-line", surface, 3) for surface in ("bg", "panel", "field")]
        pairs += [("accent", surface, 3) for surface in surfaces]
        for foreground, background, minimum in pairs:
            if foreground not in tokens or background not in tokens:
                errors.append(f"style.css: {mode} palette is missing {foreground} or {background}")
                continue
            low, high = sorted((luminance(tokens[foreground]), luminance(tokens[background])))
            ratio = (high + 0.05) / (low + 0.05)
            if ratio < minimum:
                errors.append(f"style.css: {mode} {foreground} on {background} is {ratio:.2f}:1; expected {minimum}:1")


def validate(root=ROOT):
    errors = []
    pages = check_pages(root, errors)
    check_media(root, pages, errors)
    check_image_provenance(root, pages, errors)
    check_landing(root, errors)
    check_unused_css(root, errors)
    check_modules(root, errors)
    check_try_launchers(root, errors)
    check_svgs(root, errors)
    for path in sorted(root.rglob("*.css")):
        css = path.read_text(encoding="utf-8")
        if "@import" in css or "url(" in css:
            errors.append(f"{path.name}: remote runtime dependency")
        if path.name == "style.css" and ("prefers-color-scheme" not in css or "focus-visible" not in css):
            errors.append("style.css: missing color-scheme/focus styles")
    versions = check_manifest(root, errors)
    check_processors(root, errors)
    check_feed(root, versions, errors)
    check_generated(root, errors)
    check_installers(root, errors)
    check_min_os(root, errors)
    check_signature_labels(root, errors)
    check_search_index(root, errors)
    check_theme_contrast(root, errors)
    check_distribution(root, errors)
    return errors, versions


def main():
    errors, versions = validate()
    check_installer_runs(errors)
    # Needs the web-try sources beside web/, so it is not part of validate(root).
    check_try_build(ROOT.parent, errors)
    guides = subprocess.run([sys.executable, str(Path(__file__).with_name("site-guides.py")), "--check"], capture_output=True, text=True)
    if guides.returncode:
        errors.append(guides.stderr.strip() or "web/guides is stale; run scripts/site-guides.py")
    spec = importlib.util.spec_from_file_location("docs_coverage", Path(__file__).with_name("docs-coverage.py"))
    coverage = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(coverage)
    try:
        report = coverage.validate(ROOT.parent)
        errors.extend(f"documentation coverage: {error}" for error in report["errors"])
    except (OSError, ValueError, KeyError, TypeError) as error:
        errors.append(f"cannot validate documentation coverage: {error}")
    if errors:
        sys.exit("\n".join(errors))
    print(f"Static site validated; installers checked and run against a local release, source-based documentation coverage, accessible recorded videos and posters, light/dark color contrast, current documentation search, {len(versions)} releases, downloads page and Atom feed, the in-browser WebAssembly build against its sources, resolvable module paths, no remote runtime dependencies.")


if __name__ == "__main__":
    main()
