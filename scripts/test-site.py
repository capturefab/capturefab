#!/usr/bin/env python3
"""Validate static pages, documentation search, generated release pages, and published assets."""
import hashlib
import importlib.util
import json
import re
import struct
import sys
import xml.etree.ElementTree as ET
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import urlsplit, unquote

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
    scripts = {"index.html": ["app.js", "docs.js"], "docs.html": ["app.js", "docs.js"], "downloads.html": ["docs.js"]}
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
            identity = (asset["os"], asset["arch"], asset["variant"])
            if identity in targets:
                errors.append(f"duplicate download selection: {identity}")
            targets.add(identity)
            if asset["os"] not in {"macos", "windows", "linux"} or asset["arch"] not in {"x86_64", "aarch64", "armv7"} or asset["variant"] not in {"desktop", "headless"}:
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
    check_svgs(root, errors)
    for path in sorted(root.rglob("*.css")):
        css = path.read_text(encoding="utf-8")
        if "@import" in css or "url(" in css:
            errors.append(f"{path.name}: remote runtime dependency")
        if path.name == "style.css" and ("prefers-color-scheme" not in css or "focus-visible" not in css):
            errors.append("style.css: missing color-scheme/focus styles")
    versions = check_manifest(root, errors)
    check_feed(root, versions, errors)
    check_generated(root, errors)
    check_search_index(root, errors)
    check_theme_contrast(root, errors)
    return errors, versions


def main():
    errors, versions = validate()
    if errors:
        sys.exit("\n".join(errors))
    print(f"Static site validated; accessible recorded videos and posters, light/dark color contrast, current documentation search, {len(versions)} releases, downloads page and Atom feed, no remote runtime dependencies.")


if __name__ == "__main__":
    main()
