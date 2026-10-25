#!/usr/bin/env python3
"""Publish the user guides in docs/ and the example workflows as web/guides/.

The site links to these copies, never to the source repository. Relative links
are rewritten to the copies or to site pages; a link to any other repository
file is an error, so a guide cannot point readers back at the build system."""
import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WEB = ROOT / 'web'
GUIDES = ['assistant', 'camera-compatibility', 'cli-workflows', 'ffmpeg-build', 'line-computer',
          'plant-floor', 'remote-control', 'simulation', 'video-analytics']
EXAMPLES = ROOT / 'examples' / 'workflows'
# README sections and the site page that covers each one.
README = {'build-test-and-release': '../downloads.html#source', 'save-to-drives-and-folders': '../docs.html#destinations',
          'repeatable-workflows': '../docs.html#workflows', 'host-checks-and-efficiency-mode': '../docs.html#resources',
          'gige-network-setup': '../docs.html#discovery'}
LINK = re.compile(r'(\]\()([^)\s]+)(\))')
HEADER = '<!-- Generated from {source} by scripts/site-guides.py. Edit the source, then run the script. -->\n\n'


def rewrite(target, source):
    if re.match(r'[a-z]+:', target) or target.startswith('#'):
        if 'github.com/capturefab/' in target:
            raise ValueError(f'{source}: links to the source repository: {target}')
        return target
    path, _, anchor = target.partition('#')
    fragment = f'#{anchor}' if anchor else ''
    if path.endswith('.md') and '/' not in path and path[:-3] in GUIDES:
        return target
    if path == '../README.md' and anchor in README:
        return README[anchor]
    if path == '../web/agents.md':
        return f'../agents.md{fragment}'
    if path.rstrip('/') == '../examples/workflows':
        return '#start-from-an-example'
    if path.startswith('../examples/workflows/') and (EXAMPLES / path.rsplit('/', 1)[1]).is_file():
        return f'workflows/{path.rsplit("/", 1)[1]}{fragment}'
    raise ValueError(f'{source}: no published page for the link {target}; add it to scripts/site-guides.py')


def pages(web=WEB):
    out = {}
    for name in GUIDES:
        source = f'docs/{name}.md'
        text = (ROOT / source).read_text(encoding='utf-8')
        out[web / 'guides' / f'{name}.md'] = HEADER.format(source=source) + LINK.sub(lambda m: m[1] + rewrite(m[2], source) + m[3], text)
    for example in sorted(EXAMPLES.glob('*.json')):
        out[web / 'guides' / 'workflows' / example.name] = example.read_text(encoding='utf-8')
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true', help='fail when web/guides is missing, stale or has extra files')
    args = parser.parse_args()
    try:
        generated = pages()
    except (ValueError, OSError) as error:
        parser.exit(1, f'site-guides: {error}\n')
    existing = {p for p in (WEB / 'guides').rglob('*') if p.is_file()} if (WEB / 'guides').is_dir() else set()
    stale = sorted(str(p.relative_to(WEB)) for p, text in generated.items() if not p.is_file() or p.read_text(encoding='utf-8') != text)
    extra = sorted(str(p.relative_to(WEB)) for p in existing - set(generated))
    if args.check:
        if stale or extra:
            parser.exit(1, f'site-guides: stale {", ".join(stale + extra)}; run scripts/site-guides.py\n')
        return
    for path in existing - set(generated):
        path.unlink()
    for path, text in generated.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding='utf-8')
    print(f'Published {len(generated)} files in web/guides.', file=sys.stderr)


if __name__ == '__main__':
    main()
