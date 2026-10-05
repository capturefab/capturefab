#!/usr/bin/env python3
"""Validate the dependency-free static site, generated release pages, Atom feed, and published-asset manifest."""
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
            if not attrs.get("alt"):
                self.errors.append("image without descriptive alternative text")
            self.images.append((attrs.get("src", ""), attrs.get("width"), attrs.get("height")))
        if tag == "script":
            self.scripts.append(attrs.get("src", ""))
        for name in ("href", "src"):
            if attrs.get(name):
                self.links.append(attrs[name])
        if any(key.startswith("on") for key in attrs):
            self.errors.append("inline event handler")


def image_size(path):
    data = path.read_bytes()
    if data.startswith(b"\x89PNG\r\n\x1a\n"):
        return struct.unpack(">II", data[16:24])
    svg = ET.fromstring(data)
    return int(svg.get("width")), int(svg.get("height"))


def check_pages(root, errors):
    pages = {}
    for name in ("index.html", "downloads.html"):
        page = pages[name] = Page()
        page.feed((root / name).read_text(encoding="utf-8"))
        errors.extend(f"{name}: {error}" for error in page.errors)
        if not (page.lang and page.main):
            errors.append(f"{name}: semantic page language/main missing")
    if pages["index.html"].scripts != ["app.js"] or pages["downloads.html"].scripts:
        errors.append("unexpected script dependency")
    for name, page in pages.items():
        for link in page.links:
            url = urlsplit(link)
            if url.scheme or url.netloc:
                continue
            target = unquote(url.path) or name
            if not (root / target).is_file():
                errors.append(f"{name}: missing local resource: {link}")
            elif url.fragment and target in pages and url.fragment not in pages[target].ids:
                errors.append(f"{name}: missing anchor: {link}")
        for src, width, height in page.images:
            path = root / unquote(urlsplit(src).path)
            size = image_size(path) if path.is_file() else None
            if size and size != (int(width or 0), int(height or 0)):
                errors.append(f"{name}: {src} is {size[0]}x{size[1]}, not {width}x{height}")


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


def validate(root=ROOT):
    errors = []
    check_pages(root, errors)
    check_svgs(root, errors)
    css = (root / "style.css").read_text(encoding="utf-8")
    if "@import" in css or "url(" in css or "prefers-color-scheme" not in css or "focus-visible" not in css:
        errors.append("style.css: remote dependency or missing color-scheme/focus styles")
    versions = check_manifest(root, errors)
    check_feed(root, versions, errors)
    check_generated(root, errors)
    return errors, versions


def main():
    errors, versions = validate()
    if errors:
        sys.exit("\n".join(errors))
    print(f"Static site validated; {len(versions)} releases, current downloads page and Atom feed, no remote runtime dependencies.")


if __name__ == "__main__":
    main()
