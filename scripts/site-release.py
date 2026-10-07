#!/usr/bin/env python3
"""Generate web/downloads.html and the web/feed.xml Atom feed from web/releases.json."""
import argparse
import html
import json
import re
import sys
from pathlib import Path
from urllib.parse import urlsplit

from release import SEMVER, precedence

WEB = Path(__file__).resolve().parents[1] / 'web'
FEED_EPOCH = '2026-10-04T00:00:00Z'
OS = {'macos': 'macOS', 'windows': 'Windows', 'linux': 'Linux'}
ARCH = {'x86_64': 'x64', 'aarch64': 'ARM64', 'armv7': 'ARMv7'}
NOTES = {'native-smoke-passed': 'Smoke-tested on its own platform', 'cross-compiled': 'Cross-compiled; not executed', 'unsigned': 'Unsigned', 'apple-developer-id': 'Apple Developer ID', 'apple-notarized': 'Apple notarized', 'authenticode': 'Authenticode'}
PAGE = '''<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="color-scheme" content="light dark">
  <meta name="theme-color" content="#ffffff" media="(prefers-color-scheme: light)">
  <meta name="theme-color" content="#1e1e20" media="(prefers-color-scheme: dark)">
  <meta name="description" content="Published Capturefab builds with SHA-256 checksums, signatures, and corresponding source.">
  <title>Capturefab downloads</title>
  <link rel="stylesheet" href="style.css">
  <link rel="icon" href="favicon.svg" type="image/svg+xml">
  <link rel="canonical" href="{site}downloads.html">
  <link rel="alternate" type="application/atom+xml" title="Capturefab releases" href="feed.xml">
</head>
<body>
  <a class="skip-link" href="#main">Skip to content</a>
  <div class="shell">
    <header class="site-header">
      <a class="wordmark" href="index.html" aria-label="Capturefab home"><svg class="logo" viewBox="0 0 32 32" aria-hidden="true"><rect width="32" height="32" rx="8" fill="#2f7bf5"/><circle cx="16" cy="16" r="8" fill="none" stroke="#fff" stroke-width="3"/><circle cx="16" cy="16" r="3" fill="#fff"/></svg>Capturefab</a>
      <nav aria-label="Main navigation">
        <a href="index.html#features">Features</a>
        <a href="index.html#how">How it works</a>
        <a href="index.html#download">Download</a>
        <a href="index.html#quickstart">Quickstart</a>
        <a href="index.html#automation">Automate</a>
        <a href="index.html#faq">FAQ</a>
        <a href="https://github.com/capturefab/capturefab">GitHub</a>
      </nav>
    </header>
    <main id="main">
      <section class="page-head" aria-labelledby="downloads-title">
        <h1 id="downloads-title">All downloads</h1>
        <p class="lead">Every published build with its SHA-256 checksum. Desktop builds include the app and the CLI; headless builds are the CLI alone, for servers and devices without a display. Latest release: {status}.</p>
      </section>
{body}    </main>
    <footer class="site-footer">
      <div class="brand"><p><svg class="logo" viewBox="0 0 32 32" aria-hidden="true"><rect width="32" height="32" rx="8" fill="#2f7bf5"/><circle cx="16" cy="16" r="8" fill="none" stroke="#fff" stroke-width="3"/><circle cx="16" cy="16" r="3" fill="#fff"/></svg>Capturefab</p><small>MIT licensed. The bundled FFmpeg is GPLv3 and ships with its source. This site sets no cookies and loads nothing from third parties.</small></div>
      <div><h4>Project</h4><ul><li><a href="https://github.com/capturefab/capturefab">Source</a></li><li><a href="https://github.com/capturefab/capturefab/issues">Issues</a></li><li><a href="downloads.html">All downloads</a></li><li><a href="feed.xml">Releases feed</a></li></ul></div>
      <div><h4>Documentation</h4><ul><li><a href="https://github.com/capturefab/capturefab#readme">CLI reference</a></li><li><a href="https://github.com/capturefab/capturefab/blob/main/docs/hardware-validation.md">Hardware validation</a></li><li><a href="https://github.com/capturefab/capturefab/blob/main/docs/camera-compatibility.md">Camera compatibility</a></li><li><a href="https://github.com/capturefab/capturefab/blob/main/docs/ffmpeg-build.md">FFmpeg build and licensing</a></li><li><a href="https://github.com/capturefab/capturefab/blob/main/docs/release.md">Release process</a></li></ul></div>
      <div><h4>For agents</h4><ul><li><a href="agents.md">Agent instructions</a></li><li><a href="llms.txt">llms.txt</a></li><li><a href="releases.json">Release manifest</a></li></ul></div>
    </footer>
  </div>
</body>
</html>
'''
EMPTY = '''      <section class="section" aria-labelledby="none-title">
        <div class="section-heading"><h2 id="none-title">No published binaries yet.</h2><p>No release has been published, so the <a href="releases.json">release manifest</a> is empty and the <a href="feed.xml">Atom release feed</a> has no entries. Until then, build from source.</p></div>
        <div class="source-build">
          <div><h3>Build it yourself</h3><p>Builds need a stable Rust toolchain, a C compiler and CMake, plus NASM on x86, because the JPEG encoder compiles libjpeg-turbo from source. Industrial cameras work without FFmpeg; webcams, streams and recording need a bundled or <code>CAPTUREFAB_FFMPEG</code>-supplied binary. The <a href="https://github.com/capturefab/capturefab#build-test-and-release">build guide</a> covers both.</p></div>
          <div class="code-wrap"><pre><code>git clone https://github.com/capturefab/capturefab
cd capturefab
# desktop app and CLI
cargo build --release
# headless CLI
cargo build --release --no-default-features --features usb,jpeg
target/release/capturefab --camera sim:0 capture -o first.png</code></pre></div>
        </div>
      </section>
'''
FEED = '''<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <id>{site}feed.xml</id>
  <title>Capturefab releases</title>
  <subtitle>Published Capturefab builds with SHA-256 checksums.</subtitle>
  <link rel="self" type="application/atom+xml" href="{site}feed.xml"/>
  <link rel="alternate" type="text/html" href="{site}downloads.html"/>
  <updated>{updated}</updated>
  <author><name>Capturefab contributors</name></author>
{entries}</feed>
'''

def ordered(manifest):
    if manifest.get('schema_version') != 1 or not isinstance(manifest.get('releases'), list):
        raise ValueError('expected a schema_version 1 release manifest')
    return sorted(manifest['releases'], key=lambda r: precedence(r['version']), reverse=True)

def merge(manifest, addition):
    releases = {r['version']: r for r in ordered(manifest)}
    releases.update((r['version'], r) for r in ordered(addition))
    newest = ordered(dict(schema_version=1, releases=list(releases.values())))
    stable = [r for r in newest if not precedence(r['version'])[4]]
    return dict(schema_version=1, latest=(stable or newest)[0]['version'] if newest else None, releases=newest)

def href(url):
    if urlsplit(url).scheme not in ('', 'https', 'http') or url.startswith('//'):
        raise ValueError(f'unsafe link in release manifest: {url}')
    return html.escape(url)

def size(value):
    return f'{value / 1048576:.1f} MiB'

def row(asset):
    cells = [OS.get(asset['os'], asset['os']), ARCH.get(asset['arch'], asset['arch']), asset['variant']]
    head = ''.join(f'<td>{html.escape(c)}</td>' for c in cells)
    if not asset.get('available'):
        return f'<tr>{head}<td class="unavailable">Not published</td><td>—</td><td>{html.escape(asset.get("reason", ""))}</td></tr>'
    notes = ' · '.join(NOTES.get(asset.get(k), asset.get(k)) for k in ('validation', 'platform_signature') if asset.get(k))
    return f'<tr>{head}<td><a href="{href(asset["url"])}">{html.escape(asset["name"])}</a><br>{size(asset["size"])}</td><td><code class="checksum">{html.escape(asset["sha256"])}</code></td><td>{html.escape(notes)}</td></tr>'

def section(release, latest):
    version = html.escape(release['version'])
    links = [(release.get(k), label) for k, label in (('url', 'Release notes'), ('checksums_url', 'SHA256SUMS'), ('checksums_signature_url', 'Signature'), ('source_url', 'Corresponding source'), ('sbom_url', 'SBOM'))]
    rows = '\n'.join(f'          {row(asset)}' for asset in release['assets'])
    return f'''      <section class="section" id="v{version}" aria-labelledby="v{version}-title">
        <div class="section-heading"><p class="eyebrow">{html.escape(release['date'])}{' / latest' if latest else ''}</p><h2 id="v{version}-title">capturefab {version}</h2></div>
        <p class="section-intro">{' · '.join(f'<a href="{href(url)}">{label}</a>' for url, label in links if url)}</p>
        <div class="table-scroll"><table class="release-assets"><caption>capturefab {version} release assets</caption><thead><tr><th scope="col">System</th><th scope="col">Processor</th><th scope="col">Build</th><th scope="col">File</th><th scope="col">SHA-256</th><th scope="col">Notes</th></tr></thead><tbody>
{rows}
        </tbody></table></div>
      </section>
'''

def downloads(manifest, site):
    releases = ordered(manifest)
    body = ''.join(section(r, r['version'] == manifest.get('latest')) for r in releases) or EMPTY
    status = f'v{html.escape(manifest["latest"])}' if manifest.get('latest') else 'none published yet'
    return PAGE.format(site=html.escape(site), status=status, body=body)

def entry(release, site):
    page = f'{site}downloads.html#v{release["version"]}'
    published = [f'{OS.get(a["os"], a["os"])} {ARCH.get(a["arch"], a["arch"])} {a["variant"]}' for a in release['assets'] if a.get('available')]
    summary = f'Published downloads: {", ".join(published)}.' if published else 'No published downloads.'
    return f'''  <entry>
    <id>{href(release.get('url') or page)}</id>
    <title>capturefab {html.escape(release['version'])}</title>
    <updated>{html.escape(release['date'])}T00:00:00Z</updated>
    <link rel="alternate" type="text/html" href="{href(release.get('url') or page)}"/>
    <link rel="related" type="text/html" href="{href(page)}"/>
    <summary>{html.escape(summary)}</summary>
  </entry>
'''

def feed(manifest, site):
    releases = ordered(manifest)
    updated = max((f'{r["date"]}T00:00:00Z' for r in releases), default=FEED_EPOCH)
    return FEED.format(site=html.escape(site), updated=html.escape(updated), entries=''.join(entry(r, site) for r in releases))

def pages(web):
    site = re.search(r'<link rel="canonical" href="([^"]+)"', (web / 'index.html').read_text(encoding='utf-8'))[1].rstrip('/') + '/'
    manifest = json.loads((web / 'releases.json').read_text(encoding='utf-8'))
    return {web / 'downloads.html': downloads(manifest, site), web / 'feed.xml': feed(manifest, site)}

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--web', type=Path, default=WEB)
    parser.add_argument('--merge', type=Path, help='add or replace releases from a release.py assemble releases.json')
    parser.add_argument('--check', action='store_true', help='fail when generated files are missing or stale')
    args = parser.parse_args()
    try:
        if args.merge:
            manifest = merge(json.loads((args.web / 'releases.json').read_text(encoding='utf-8')), json.loads(args.merge.read_text(encoding='utf-8')))
            (args.web / 'releases.json').write_text(json.dumps(manifest, indent=2) + '\n', encoding='utf-8')
        generated = pages(args.web)
    except (ValueError, KeyError, OSError) as error:
        parser.exit(1, f'site-release: {error}\n')
    stale = [path.name for path, text in generated.items() if not path.is_file() or path.read_text(encoding='utf-8') != text]
    if args.check:
        if stale:
            parser.exit(1, f'site-release: stale {", ".join(stale)}; run scripts/site-release.py\n')
        return
    for path, text in generated.items():
        path.write_text(text, encoding='utf-8')
    print(f'Generated {", ".join(path.name for path in generated)}.', file=sys.stderr)

if __name__ == '__main__':
    main()
