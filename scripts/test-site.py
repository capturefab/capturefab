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


def mp4_boxes(path):
    """Read top-level ISO BMFF boxes without a platform media dependency."""
    data = path.read_bytes()
    boxes = []
    offset = 0
    while offset < len(data):
        if len(data) - offset < 8:
            raise ValueError("incomplete MP4 box header")
        size, kind = struct.unpack_from(">I4s", data, offset)
        header = 8
        if size == 1:
            if len(data) - offset < 16:
                raise ValueError("incomplete extended MP4 box header")
            size = struct.unpack_from(">Q", data, offset + 8)[0]
            header = 16
        elif size == 0:
            size = len(data) - offset
        if size < header or offset + size > len(data):
            raise ValueError("invalid MP4 box size")
        boxes.append(kind)
        offset += size
    return boxes


def check_media(root, pages, errors):
    """Anchor recorded-media metadata to checked-in files and accessible embeds."""
    directory = root / "media"
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
                boxes = mp4_boxes(path)
                if not boxes or boxes[0] != b"ftyp" or b"moov" not in boxes or b"mdat" not in boxes:
                    errors.append(f"{prefix}: invalid MP4 container")
                elif boxes.index(b"moov") > boxes.index(b"mdat"):
                    errors.append(f"{prefix}: MP4 metadata must precede media for progressive playback")
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


def fixture_archive(directory, system, arch):
    """A release archive whose capturefab prints a version, laid out like scripts/release.py's."""
    stem = f"capturefab-9.9.9-{system}-{arch}"
    stub = b"#!/bin/sh\necho 'capturefab 9.9.9'\n"
    if system == "macos":
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
    site = site_release()
    with tempfile.TemporaryDirectory() as temp:
        temp = Path(temp)
        served = temp / "served"
        served.mkdir()
        assets = []
        for arch in ("x86_64", "aarch64"):
            path = fixture_archive(served, system, arch)
            assets.append(dict(name=path.name, os=system, arch=arch, available=True, size=path.stat().st_size,
                               sha256=hashlib.sha256(path.read_bytes()).hexdigest(), url=f"https://example.invalid/{path.name}"))
        manifest = dict(schema_version=1, latest="9.9.9", releases=[dict(version="9.9.9", date="2026-01-01", assets=assets)])
        script = temp / "install.sh"
        script.write_text(site.installers(manifest, "https://capturefab.example/")[0], encoding="utf-8")
        server = ThreadingHTTPServer(("127.0.0.1", 0), partial(QuietHandler, directory=str(served)))
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            def install(name):
                home = temp / name
                home.mkdir()
                env = dict(os.environ, HOME=str(home), XDG_DATA_HOME=str(home / "share"), CAPTUREFAB_INSTALL_BASE_URL=f"http://127.0.0.1:{server.server_port}")
                result = subprocess.run(["sh", str(script), "--prefix", str(home / "apps"), "--bin-dir", str(home / "bin")],
                                        capture_output=True, text=True, env=env, timeout=60)
                return home, result
            home, result = install("good")
            command = home / "bin" / "capturefab"
            if result.returncode or not command.is_symlink():
                errors.append(f"install.sh: fixture install failed: {result.stderr.strip() or result.stdout.strip()}")
            elif "capturefab 9.9.9" not in result.stdout:
                errors.append("install.sh: did not run the installed capturefab --version")
            elif system == "linux" and 'Exec="' not in (home / "share/applications/capturefab.desktop").read_text():
                errors.append("install.sh: the launcher entry must name the installed command")
            for path in served.iterdir():
                path.write_bytes(path.read_bytes() + b"tampered")
            home, result = install("corrupt")
            if result.returncode == 0 or "checksum mismatch" not in result.stderr:
                errors.append("install.sh: a corrupted download must fail with a checksum mismatch")
            if (home / "bin").exists() or (home / "apps").exists():
                errors.append("install.sh: a corrupted download must install nothing")
        finally:
            server.shutdown()
            server.server_close()
        empty = temp / "empty.sh"
        empty.write_text(site.installers(dict(schema_version=1, latest=None, releases=[]), "https://capturefab.example/")[0], encoding="utf-8")
        result = subprocess.run(["sh", str(empty)], capture_output=True, text=True, env=dict(os.environ, HOME=str(temp)), timeout=30)
        if result.returncode == 0 or "no published release" not in result.stderr:
            errors.append("install.sh: without a release it must fail with 'no published release'")


class QuietHandler(SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass


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
    check_search_index(root, errors)
    check_theme_contrast(root, errors)
    return errors, versions


def main():
    errors, versions = validate()
    check_installer_runs(errors)
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
    print(f"Static site validated; installers checked and run against a local release, source-based documentation coverage, accessible recorded videos and posters, light/dark color contrast, current documentation search, {len(versions)} releases, downloads page and Atom feed, no remote runtime dependencies.")


if __name__ == "__main__":
    main()
