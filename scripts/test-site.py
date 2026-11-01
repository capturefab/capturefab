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
        if tag in {"video", "audio"}:
            self.errors.append(f"<{tag}> element: the site publishes no recorded media")
        runtime_url = None
        if tag in {"img", "script", "iframe", "audio", "video", "source"}:
            runtime_url = attrs.get("src")
        elif tag == "link" and attrs.get("rel") == "stylesheet":
            runtime_url = attrs.get("href")
        if runtime_url and (urlsplit(runtime_url).scheme or urlsplit(runtime_url).netloc):
            self.errors.append(f"remote runtime dependency: {runtime_url}")
        for name in ("href", "src"):
            if attrs.get(name):
                self.links.append(attrs[name])
        for candidate in (attrs.get("srcset") or "").split(","):
            if candidate.strip():
                self.links.append(candidate.split()[0])
        if any(key.startswith("on") for key in attrs):
            self.errors.append("inline event handler")


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
               "reference.html": ["app.js", "docs.js"]}
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
    return pages


def plain_text(markup):
    return re.sub(r"\s+", " ", unescape(re.sub(r"<[^>]+>", " ", markup))).strip()


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
    commands = [line for line in code_lines(quickstart[0] if quickstart else "") if line.startswith("capturefab ")]
    if not commands or any("-c " in line and "-c sim:" not in line for line in commands):
        errors.append("index.html: the quickstart must run on the simulator camera (-c sim:0) so it works without hardware")
    # Landing examples are copied from documented ones, so docs coverage keeps them current.
    documented = set(code_lines(docs))
    for line in code_lines(index):
        if line.startswith("capturefab ") and line not in documented:
            errors.append(f"index.html: CLI example is not in a docs.html example: {line}")
    faq = re.search(r'<div class="faq">\s*<details><summary>(.*?)</summary>', index, re.S)
    if faq and re.search(r"\b(version|upgrad\w*|changed)\b|\d+\.\d+", faq[1], re.I):
        errors.append(f"index.html: the FAQ should open with a newcomer question, not release news: {faq[1]}")


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
    """The generated installers offer exactly the manifest's published builds, with their checksums,
    and nothing while the latest release is the desktop app (0.5.0 and earlier)."""
    manifest = json.loads((root / "releases.json").read_text(encoding="utf-8"))
    canonical = re.search(r'<link rel="canonical" href="([^"]+)"', (root / "index.html").read_text(encoding="utf-8"))
    site = canonical[1].rstrip("/") + "/" if canonical else ""
    latest = [r for r in manifest["releases"] if r["version"] == manifest.get("latest") and not site_release().is_desktop_release(r)]
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

    kind "dmg" is a macOS disk image as releases up to 0.5.0 shipped; the
    installer must refuse it before downloading anything."""
    stem = f"capturefab-9.9.9-{system}-{arch}"
    if kind == "dmg":
        path = directory / f"{stem}.dmg"
        path.write_bytes(b"not a disk image")
        return path
    stub = b"#!/bin/sh\necho 'capturefab 9.9.9'\n"
    path = directory / f"{stem}.tar.gz"
    with tarfile.open(path, "w:gz") as archive:
        for name, data, mode in ((f"{stem}/capturefab", stub, 0o755), (f"{stem}/README.txt", b"Capturefab 9.9.9\n", 0o644)):
            info = tarfile.TarInfo(name)
            info.size, info.mode = len(data), mode
            archive.addfile(info, io.BytesIO(data))
    return path


def check_installer_runs(errors):
    """Install from a local server, migrate an earlier desktop install, then refuse a
    corrupted download, a disk image, a desktop release (0.5.0 and earlier) and an empty release."""
    system = {"Darwin": "macos", "Linux": "linux"}.get(platform.system())
    if not system or not shutil.which("sh"):
        return
    check_installer_run(errors, system)


def installer(site, temp, system, kind=None, version="9.9.9"):
    served = temp / "served"
    served.mkdir(exist_ok=True)
    assets = []
    for arch in ("x86_64", "aarch64"):
        path = fixture_archive(served, system, arch, kind)
        assets.append(dict(name=path.name, os=system, arch=arch, available=True, size=path.stat().st_size,
                           sha256=hashlib.sha256(path.read_bytes()).hexdigest(), url=f"https://example.invalid/{path.name}"))
    manifest = dict(schema_version=1, latest=version, releases=[dict(version=version, date="2026-01-01", assets=assets)])
    script = temp / f"install-{kind or 'tar'}-{version}.sh"
    script.write_text(site.installers(manifest, "https://capturefab.example/")[0], encoding="utf-8")
    return served, script


def check_installer_run(errors, system):
    site = site_release()
    label = "install.sh"
    with tempfile.TemporaryDirectory() as temp:
        temp = Path(temp)
        served, script = installer(site, temp, system)
        server = ThreadingHTTPServer(("127.0.0.1", 0), partial(QuietHandler, directory=str(served)))
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            def install(name, prefix=None, setup=None, script=script):
                home = temp / name
                home.mkdir()
                # The script's own temporary folder, which must be gone afterwards.
                (home / "tmp").mkdir()
                if setup:
                    setup(home)
                env = dict(os.environ, HOME=str(home), TMPDIR=str(home / "tmp"), XDG_DATA_HOME=str(home / "share"),
                           CAPTUREFAB_INSTALL_BASE_URL=f"http://127.0.0.1:{server.server_port}")
                arguments = ["sh", str(script), "--bin-dir", str(home / "bin")] + (["--prefix", prefix] if prefix else [])
                result = subprocess.run(arguments, capture_output=True, text=True, env=env, timeout=120, cwd=home)
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
            elif command.resolve() != (home / "apps/9.9.9/capturefab").resolve():
                errors.append(f"{label}: the command must link to the release's executable, not {os.readlink(command)}")

            def desktop_install(home):
                # What 0.5 and earlier left: a command link into the app and a Linux launcher entry.
                app = home / "Applications/Capturefab.app/Contents/MacOS"
                app.mkdir(parents=True)
                (app / "capturefab").write_text("#!/bin/sh\necho 'capturefab 0.5.0'\n")
                (home / "bin").mkdir()
                (home / "bin/capturefab").symlink_to(app / "capturefab")
                launcher = home / "share/applications"
                launcher.mkdir(parents=True)
                (launcher / "capturefab.desktop").write_text("[Desktop Entry]\nExec=\"/home/user/.local/bin/capturefab\"\n")
                icons = home / "share/icons/hicolor/256x256/apps"
                icons.mkdir(parents=True)
                (icons / "capturefab.png").write_bytes(b"png")
            home, result = install("upgrade", setup=desktop_install)
            command = home / "bin" / "capturefab"
            if result.returncode or command.resolve() != (home / "share/capturefab/9.9.9/capturefab").resolve():
                errors.append(f"{label}: an earlier desktop install must not stop the upgrade: {result.stderr.strip() or result.stdout.strip()}")
            elif "no longer runs the desktop app" not in result.stdout:
                errors.append(f"{label}: replacing a command link into Capturefab.app must say what happened to the app")
            elif (home / "share/applications/capturefab.desktop").exists() or (home / "share/icons/hicolor/256x256/apps/capturefab.png").exists():
                errors.append(f"{label}: the launcher entry and icons of an earlier desktop install must be removed")
            elif not (home / "Applications/Capturefab.app").is_dir():
                errors.append(f"{label}: an earlier desktop app must stay until its user deletes it")

            for path in served.iterdir():
                path.write_bytes(path.read_bytes() + b"tampered")
            home, result = install("corrupt")
            if result.returncode == 0 or "checksum mismatch" not in result.stderr:
                errors.append(f"{label}: a corrupted download must fail with a checksum mismatch")
            if (home / "bin").exists() or (home / "share").exists():
                errors.append(f"{label}: a corrupted download must install nothing")

            _, legacy = installer(site, temp, system, "dmg")
            home, result = install("legacy", script=legacy)
            if result.returncode == 0 or "downloads.html" not in result.stderr or (home / "bin").exists():
                errors.append(f"{label}: a disk-image release must fail, pointing to the downloads page, and install nothing")

            # The latest release is still the desktop app: install nothing, and
            # keep the launcher entry it added.
            _, desktop = installer(site, temp, system, version="0.5.0")
            home, result = install("desktop", script=desktop, setup=desktop_install)
            if result.returncode == 0 or "no command-line release" not in result.stderr or "downloads.html" not in result.stderr:
                errors.append(f"{label}: a desktop release (0.5.0 and earlier) must fail with 'no command-line release', pointing to the downloads page")
            elif not (home / "share/applications/capturefab.desktop").exists() or not (home / "bin/capturefab").is_symlink() or (home / "share/capturefab").exists():
                errors.append(f"{label}: a desktop release must install nothing and leave the earlier install as it was")
        finally:
            server.shutdown()
            server.server_close()
        empty = temp / "empty.sh"
        empty.write_text(site.installers(dict(schema_version=1, latest=None, releases=[]), "https://capturefab.example/")[0], encoding="utf-8")
        result = subprocess.run(["sh", str(empty)], capture_output=True, text=True, env=dict(os.environ, HOME=str(temp)), timeout=30)
        if result.returncode == 0 or "no command-line release" not in result.stderr:
            errors.append("install.sh: without a release it must fail with 'no command-line release'")


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
    check_pages(root, errors)
    check_landing(root, errors)
    check_unused_css(root, errors)
    check_modules(root, errors)
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
    guides = subprocess.run([sys.executable, str(Path(__file__).with_name("site-guides.py")), "--check"], capture_output=True, text=True)
    if guides.returncode:
        errors.append(guides.stderr.strip() or "web/guides is stale; run scripts/site-guides.py")
    reference = subprocess.run([sys.executable, str(Path(__file__).with_name("site-reference.py")), "--check"], capture_output=True, text=True)
    if reference.returncode:
        errors.append(reference.stderr.strip() or "web/reference.html is stale; run scripts/site-reference.py")
    if errors:
        sys.exit("\n".join(errors))
    print(f"Static site validated; installers checked and run against a local release, including an upgrade from the desktop app, the command reference against the CLI schema snapshot, light/dark color contrast, current documentation search, {len(versions)} releases, downloads page and Atom feed, resolvable module paths, no remote runtime dependencies.")


if __name__ == "__main__":
    main()
