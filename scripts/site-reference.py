#!/usr/bin/env python3
"""Generate web/reference.html from the CLI's own description of itself.

`capturefab schema`, `capturefab schema COMMAND` and each command's `-h`
examples are the contract. `--binary PATH` records them in docs/cli-schema.json
and writes the page; without it the page is written from that snapshot, so the
site builds without compiling Capturefab. `--check` fails when the page (and,
with `--binary`, the snapshot) is stale; CI runs both.
"""
import argparse
import html
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SNAPSHOT = ROOT / 'docs' / 'cli-schema.json'
PAGE = ROOT / 'web' / 'reference.html'
SITE = 'https://capturefab.com/'
# Sections of the page, in order, and the commands each holds. A command the
# CLI gains later lands in "More commands" until it is placed here.
GROUPS = [
    ('cli-cameras', 'Cameras and settings', 'Find a camera, read what it can do and change its settings.',
     ['list', 'info', 'features', 'get', 'set', 'exec']),
    ('cli-capture', 'Capture, record and measure', 'Save images and video. Measure brightness and focus or set auto exposure.',
     ['capture', 'record', 'inspect', 'watch', 'auto']),
    ('cli-presets', 'Presets', 'Save a camera\'s settings on this computer and restore them as one change.',
     ['preset']),
    ('cli-sessions', 'Sessions and jobs', 'Keep cameras open between commands and run captures and recordings in the background.',
     ['session', 'serve', 'jobs']),
    ('cli-network', 'Sign-ins and networks', 'Save camera passwords, find ONVIF stream URLs and diagnose GigE connections.',
     ['login', 'onvif', 'network']),
    ('cli-tools', 'Diagnostics and tools', 'Diagnose this computer, see the CLI contract, use the Capturefab shell or access camera registers.',
     ['doctor', 'schema', 'shell', 'completions', 'debug', 'licenses']),
]
OTHER = ('cli-more', 'More commands', 'Commands without a section of their own yet.')
GROUP_IDS = [group[0] for group in GROUPS] + [OTHER[0]]
HEADERS = ('Usage:', 'Commands:', 'Options:', 'Arguments:')


def run(binary, *args, env):
    result = subprocess.run([str(binary), *args], capture_output=True, text=True, env=env, timeout=60)
    if result.returncode:
        raise ValueError(f'capturefab {" ".join(args)} exited with {result.returncode}: {result.stderr.strip()}')
    return result.stdout


def blocks(text):
    """Paragraphs of a help text: lists of lines separated by blank lines."""
    found, current = [], []
    for line in text.splitlines():
        if line.strip():
            current.append(line.rstrip())
        elif current:
            found.append(current)
            current = []
    if current:
        found.append(current)
    return found


def parse_help(text):
    """The about line, notes, examples, global options and aliases of `-h` output."""
    about, notes, examples, options, aliases = '', [], [], [], {}
    section = None
    for index, block in enumerate(blocks(text)):
        head = block[0]
        if index == 0:
            about = ' '.join(line.strip() for line in block)
            continue
        if head.startswith(HEADERS):
            section = head.split(':')[0]
            lines = block[1:] if head.endswith(':') else []
        elif head == 'Examples:':
            examples = [line.strip() for line in block[1:]]
            section = None
            continue
        elif head.startswith(' '):
            lines = block
        else:
            notes.append(' '.join(line.strip() for line in block))
            section = None
            continue
        for line in lines:
            if section == 'Commands':
                match = re.match(r'\s+(\S+)\s{2,}.*\[alias(?:es)?: ([^\]]+)\]', line)
                if match:
                    aliases[match[1]] = [alias.strip() for alias in match[2].split(',')]
            elif section == 'Options':
                match = re.match(r'\s+(?:(-\w), )?(--[\w-]+)(?: <([^>]+)>)?\s{2,}(.*?)(?: \[env: ([A-Z_]+)=?[^\]]*\])?$', line)
                if match:
                    options.append(dict(short=match[1], long=match[2], value_name=match[3], help=match[4], env=match[5]))
    return dict(about=about, notes=notes, examples=examples, options=options, aliases=aliases)


def collect(binary):
    """Ask the binary for its schema and help, in isolated folders."""
    with tempfile.TemporaryDirectory(prefix='capturefab-reference-') as temp:
        env = {k: v for k, v in os.environ.items() if not k.startswith('CAPTUREFAB_')}
        env.update(CAPTUREFAB_SESSION_DIR=str(Path(temp) / 'sessions'), CAPTUREFAB_CACHE_DIR=str(Path(temp) / 'cache'))
        bare = json.loads(run(binary, 'schema', env=env))
        top = parse_help(run(binary, '-h', env=env))
        group_names = {name.rpartition(' ')[0] for name in bare['commands'] if ' ' in name}
        help_by_name = {'': top}
        for name in sorted(set(bare['commands']) | group_names):
            help_by_name[name] = parse_help(run(binary, *name.split(), '-h', env=env))
        commands = {}
        for name in bare['commands']:
            detail = json.loads(run(binary, 'schema', *name.split(), env=env))
            help_ = help_by_name[name]
            detail.update(examples=help_['examples'], notes=help_['notes'])
            parent, _, child = name.rpartition(' ')
            if aliases := help_by_name[parent]['aliases'].get(child):
                detail['aliases'] = aliases
            commands[name] = detail
        groups = {}
        for name in sorted(group_names - set(commands)):
            help_ = help_by_name[name]
            groups[name] = dict(summary=help_['about'], notes=help_['notes'], examples=help_['examples'])
    return dict(generated_by='scripts/site-reference.py --binary target/debug/capturefab', about=top['about'],
                global_options=top['options'], schema=bare, commands=commands, groups=groups)


def dumps(snapshot):
    return json.dumps(snapshot, indent=1, ensure_ascii=False, sort_keys=True) + '\n'


def slug(name):
    return 'command-' + re.sub(r'[^a-z0-9]+', '-', name.lower()).strip('-')


def esc(text):
    return html.escape(str(text), quote=True)


def code(text):
    return f'<code>{esc(text)}</code>'


def prose(text):
    """Help text with `quoted` spans set as code."""
    parts = re.split(r'(`[^`]+`)', str(text))
    out = []
    for part in parts:
        if part.startswith('`') and part.endswith('`') and len(part) > 1:
            out.append(code(part[1:-1]))
        else:
            out.append(esc(part))
    return ''.join(out)


def spelling(arg):
    if arg.get('long') or arg.get('short'):
        names = ', '.join(code(x) for x in (arg.get('short'), arg.get('long')) if x)
        if arg.get('type') != 'flag':
            names += f' {esc(arg.get("value_name") or "VALUE")}'
        return names
    label = (arg.get('value_name') or arg['name']).upper()
    label = label + '...' if arg.get('repeatable') else label
    return code(label if arg.get('required') else f'[{label}]')


def facts(arg, effect):
    items = []
    if arg.get('choices'):
        items.append('one of ' + ', '.join(code(choice) for choice in arg['choices']))
    if arg.get('default') is not None and arg.get('type') != 'flag':
        items.append('default ' + code(arg['default']))
    if arg.get('env'):
        items.append('env ' + code(arg['env']))
    if arg.get('repeatable') and (arg.get('long') or arg.get('short')):
        items.append('repeatable')
    if arg.get('required') and (arg.get('long') or arg.get('short')):
        items.append('required')
    if effect:
        items.append('effect ' + code(effect))
    return '; '.join(items) or '—'


def table(caption, headers, rows):
    head = ''.join(f'<th scope="col">{h}</th>' for h in headers)
    body = '\n'.join('<tr>' + ''.join(f'<td>{cell}</td>' for cell in row) + '</tr>' for row in rows)
    return f'<div class="table-scroll"><table><caption>{caption}</caption><thead><tr>{head}</tr></thead><tbody>\n{body}\n</tbody></table></div>'


# A word with [ that is not quoted, such as TriggerMode[FrameStart]: zsh, the
# default macOS shell, takes it as a glob and stops when nothing matches.
GLOB_WORD = re.compile(r'(?<!\S)([^\s\'"]*\[[^\s\'"]*)(?!\S)')


def shell_safe(line):
    """An example line that copies into zsh as well as bash: bracketed names quoted."""
    return GLOB_WORD.sub(lambda match: f"'{match[1]}'", line)


def examples(lines, label):
    if not lines:
        return ''
    body = esc('\n'.join(shell_safe(line) for line in lines))
    return f'<div class="code-wrap"><pre><code>{body}</code></pre><button class="copy-button" type="button" aria-label="Copy the {esc(label)} examples">Copy</button></div>'


def command_block(name, detail):
    out = [f'<h3 id="{slug(name)}"><code>capturefab {esc(name)}</code></h3>']
    parent = name.rpartition(' ')[0]
    aliases = [' '.join(part for part in (parent, alias) if part) for alias in detail.get('aliases', [])]
    alias = f' Alias: {", ".join(code(a) for a in aliases)}.' if aliases else ''
    out.append(f'<p>{prose(detail["summary"])}. Effect: {code(detail["effect"])}.{alias}</p>')
    out += [f'<p>{prose(note)}</p>' for note in detail.get('notes', [])]
    args = detail.get('args') or []
    if args:
        effects = detail.get('effect_with', {})
        out.append(table(f'capturefab {esc(name)} arguments', ['Argument or option', 'Meaning', 'Details'],
                         [(spelling(arg), prose(arg.get('help', '')), facts(arg, effects.get(arg.get('long')))) for arg in args]))
    out.append(examples(detail.get('examples'), f'capturefab {name}'))
    result = detail.get('result')
    if result:
        out.append(table(f'capturefab {esc(name)} JSON result', ['Field', 'Type and meaning'],
                         [(code(field), prose(meaning)) for field, meaning in sorted(result.items())]))
    errors = detail.get('errors') or []
    if errors:
        out.append('<p>Errors: ' + ', '.join(f'<a href="#error-{esc(e)}">{code(e)}</a>' for e in errors) + '.</p>')
    return '\n'.join(part for part in out if part)


def group_block(name, detail):
    out = [f'<h3 id="{slug(name)}"><code>capturefab {esc(name)}</code></h3>', f'<p>{prose(detail["summary"])}.</p>']
    out += [f'<p>{prose(note)}</p>' for note in detail.get('notes', [])]
    out.append(examples(detail.get('examples'), f'capturefab {name}'))
    return '\n'.join(part for part in out if part)


def article(identity, title, summary, tags, body):
    return (f'<article class="doc-section" id="{identity}" data-doc-section data-doc-title="{esc(title)}" data-doc-category="Command reference" data-doc-tags="{esc(",".join(tags))}">'
            f'<h2>{esc(title)}</h2><p class="doc-summary">{esc(summary)}</p>\n{body}</article>')


def render(snapshot):
    schema = snapshot['schema']
    commands, groups = snapshot['commands'], snapshot['groups']
    conventions = schema['conventions']
    sections = []

    exit_rows = [(code(key), esc(value)) for key, value in sorted(schema['exit_codes'].items(), key=lambda item: int(item[0]))]
    errors = sorted(schema['errors'].items(), key=lambda item: (item[1][0], item[0]))
    error_rows = [(f'<code id="error-{esc(name)}">{esc(name)}</code>', code(exit_), prose(meaning)) for name, (exit_, meaning) in errors]
    basics = [f'<p>{prose(conventions["json"])}. Use the exit code for the category and <code>error.code</code> for the specific reason.</p>',
              f'<p>{prose(conventions["streams"])}. NDJSON means one JSON object per line. <code>--help</code> and <code>--version</code> print text.</p>',
              '<p>Use <code>--progress</code> with <code>capture</code>, <code>record</code> or <code>jobs wait</code> to follow job progress. Each line has a <code>progress</code> object.</p>',
              f'<p>{prose(conventions["results"])}. A result can include up to 10 distinct warnings. {prose(conventions["credentials"])}.</p>',
              f'<p>{prose(conventions["describe"])}. This reference uses that output and the CLI help. The contract version is {code(schema["contract_version"])}.</p>',
              table('Exit codes', ['Exit code', 'Category'], exit_rows),
              table('Error codes in <code>error.code</code>', ['Code', 'Exit', 'Meaning'], error_rows)]
    sections.append(article('cli-basics', 'JSON results, exit codes and errors', 'Read JSON results and progress lines. Use exit codes to handle errors.',
                            ['json', 'ndjson', 'exit code', 'error code', 'schema', 'stdout', 'stderr', '--progress'], '\n'.join(basics)))

    option_rows = [(', '.join(code(x) for x in (o.get('short'), o.get('long')) if x) + (f' {esc(o["value_name"])}' if o.get('value_name') else ''),
                    prose(o['help']), code(o['env']) if o.get('env') else '—') for o in snapshot['global_options']]
    globals_body = [f'<p>{prose(conventions["globals"])}.</p>', table('Global options', ['Option', 'Meaning', 'Environment'], option_rows),
                    f'<p>{prose(conventions["camera"])}.</p>', f'<p>{prose(conventions["routing"])}.</p>',
                    '<p><code>--no-session</code> overrides <code>CAPTUREFAB_SESSION</code> and <code>CAPTUREFAB_KEEP</code>. '
                    'It conflicts with an explicit <code>--session</code>, <code>--background</code> or positive <code>--keep</code>.</p>',
                    f'<p>{prose(conventions["jobs"])}.</p>',
                    '<p>Each command lists its default effect. Look for options that add a file or camera write in the argument table.</p>',
                    '<dl><dt><code>read</code></dt><dd>Reads data. It can write a temporary preview.</dd>'
                    '<dt><code>file_write</code></dt><dd>Can write files or update saved data on this computer.</dd>'
                    '<dt><code>camera_write</code></dt><dd>Can change camera settings or jobs.</dd>'
                    '<dt><code>disruptive</code></dt><dd>Can hold cameras, stop streams or run camera commands.</dd></dl>']
    sections.append(article('cli-global-options', 'Global options, cameras and sessions', 'Choose a camera, control session use and see what commands can change.',
                            ['global options', 'environment', 'session', 'camera', '--keep', '--background'], '\n'.join(globals_body)))

    grammar_rows = [(esc(key), prose(value)) for key, value in sorted(schema['grammars'].items())]
    grammars = table('Accepted forms', ['Value', 'Form'], grammar_rows)
    grammars += ('\n<p>In a shell, put quotes around names, selectors and templates with special characters. '
                 'For example, use <code>\'TriggerMode[FrameStart]\'</code> or <code>\'frames/{seq}.png\'</code>.</p>'
                 '\n<p>Use an existing folder or end the path with a separator to save numbered files. '
                 'Use <code>{seq:N}</code> to pad the number to N digits, from 1 to 9. '
                 'Use <code>--on-exists number</code> to add a number when a fixed capture filename already exists.</p>')
    sections.append(article('cli-grammars', 'Selectors, names, durations and templates', 'See what to type for camera selectors, feature names, durations and output templates.',
                            ['selector', 'feature name', 'bank', 'duration', 'template', 'controls'], grammars))

    placed = set()
    for identity, title, summary, roots in GROUPS + [(OTHER[0], OTHER[1], OTHER[2], None)]:
        names = [name for name in commands if (name.split()[0] in roots if roots else name not in placed)]
        parents = [name for name in groups if (name in roots if roots else name not in placed)]
        if not names and not parents:
            continue
        body = []
        for root in (roots or sorted({n.split()[0] for n in names} | set(parents))):
            if root in groups:
                body.append(group_block(root, groups[root]))
                placed.add(root)
            for name in [n for n in names if n.split()[0] == root]:
                body.append(command_block(name, commands[name]))
                placed.add(name)
        tags = sorted({name for name in names} | set(parents))
        sections.append(article(identity, title, summary, tags, '\n'.join(body)))

    nav = [('cli-basics', 'JSON results and errors'), ('cli-global-options', 'Global options'), ('cli-grammars', 'Value grammars')]
    nav += [(identity, title) for identity, title, *_ in GROUPS + [OTHER] if f'id="{identity}"' in ''.join(sections)]
    links = ''.join(f'<a href="#{identity}">{esc(title)}</a>' for identity, title in nav)
    return TEMPLATE.format(site=SITE, links=links, about=prose(snapshot['about']), sections='\n'.join(sections))


TEMPLATE = '''<!doctype html>
<!-- Generated by scripts/site-reference.py from docs/cli-schema.json. Do not edit by hand. -->
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="color-scheme" content="light dark">
  <meta name="theme-color" content="#ffffff" media="(prefers-color-scheme: light)">
  <meta name="theme-color" content="#1e1e20" media="(prefers-color-scheme: dark)">
  <meta name="description" content="Every capturefab command, option, JSON result field, exit code and error code, generated from capturefab schema.">
  <meta property="og:title" content="Capturefab command reference">
  <meta property="og:description" content="Every capturefab command, option, JSON result field, exit code and error code, generated from capturefab schema.">
  <meta property="og:type" content="website">
  <meta property="og:url" content="{site}reference.html">
  <title>Command reference — Capturefab</title>
  <link rel="stylesheet" href="style.css">
  <link rel="stylesheet" href="docs.css">
  <link rel="icon" href="favicon.ico" sizes="16x16 32x32 48x48">
  <link rel="icon" href="favicon-32.png" type="image/png" sizes="32x32">
  <link rel="icon" href="favicon.svg" type="image/svg+xml" sizes="any">
  <link rel="apple-touch-icon" href="apple-touch-icon.png" sizes="180x180">
  <link rel="canonical" href="{site}reference.html">
  <link rel="alternate" type="application/atom+xml" title="Capturefab releases" href="feed.xml">
  <script src="app.js" defer></script>
  <script src="docs.js" defer></script>
</head>
<body class="docs-page">
  <a class="skip-link" href="#main">Skip to command reference</a>
  <div class="shell">
    <header class="site-header">
      <a class="wordmark" href="index.html" aria-label="Capturefab home"><img class="logo" src="favicon.svg" width="128" height="128" alt="" aria-hidden="true">Capturefab</a>
      <nav aria-label="Main navigation"><a href="index.html">Product</a><a href="docs.html">Documentation</a><a href="downloads.html">Downloads</a><a href="downloads.html#source">Source</a></nav>
      <a class="search-trigger" href="docs.html#search" aria-label="Search documentation" aria-keyshortcuts="Control+K Meta+K"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" aria-hidden="true"><circle cx="10.5" cy="10.5" r="6.5"/><path d="m16 16 5 5"/></svg><span>Search docs</span><kbd>Ctrl K</kbd></a>
    </header>
    <main id="main" class="docs-layout"><aside class="docs-sidebar" aria-label="Command reference navigation"><div class="sidebar-group"><p>Command reference</p>{links}</div><a class="sidebar-source" href="docs.html">Back to the guides</a></aside><div class="docs-content">
<details class="docs-mobile-nav"><summary>Browse the command reference</summary><nav aria-label="Command reference topics">{links}</nav></details>
<header class="docs-head"><p class="eyebrow">Reference</p><h1>Commands, options and results</h1><p class="lead">{about} Follow a task in the <a href="docs.html">guides</a> or use the <a href="agents.md">agent instructions</a> for automation.</p></header>
{sections}
</div></main>
    <footer class="site-footer">
      <div class="brand"><p><img class="logo" src="favicon.svg" width="128" height="128" alt="" aria-hidden="true">Capturefab</p><small>Capturefab uses the MIT license. The included FFmpeg uses GPLv3. Releases include its source. This site sets no cookies and loads nothing from third parties.</small></div>
      <div><h2 class="footer-heading">Project</h2><ul><li><a href="downloads.html#source">Source code</a></li><li><a href="downloads.html">Downloads</a></li><li><a href="feed.xml">Release feed</a></li></ul></div>
      <div><h2 class="footer-heading">Documentation</h2><ul><li><a href="docs.html#getting-started">Quickstart</a></li><li><a href="#cli-basics">JSON and exit codes</a></li><li><a href="docs.html#troubleshooting">Troubleshooting</a></li></ul></div>
      <div><h2 class="footer-heading">For agents</h2><ul><li><a href="agents.md">Agent instructions</a></li><li><a href="llms.txt">llms.txt</a></li><li><a href="releases.json">Release manifest</a></li></ul></div>
    </footer>
  </div>
</body>
</html>
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--binary', type=Path, help='a built capturefab: record its schema and help in docs/cli-schema.json first')
    parser.add_argument('--check', action='store_true', help='fail when the snapshot or the page is stale; change nothing')
    parser.add_argument('--snapshot', type=Path, default=SNAPSHOT)
    parser.add_argument('--page', type=Path, default=PAGE)
    args = parser.parse_args()
    try:
        if args.binary:
            snapshot = collect(args.binary)
            text = dumps(snapshot)
            if args.check:
                if not args.snapshot.is_file() or args.snapshot.read_text(encoding='utf-8') != text:
                    parser.exit(1, f'site-reference: {args.snapshot.name} differs from {args.binary} schema; run scripts/site-reference.py --binary {args.binary}\n')
            else:
                args.snapshot.write_text(text, encoding='utf-8')
        snapshot = json.loads(args.snapshot.read_text(encoding='utf-8'))
        page = render(snapshot)
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        parser.exit(1, f'site-reference: {error}\n')
    if args.check:
        if not args.page.is_file() or args.page.read_text(encoding='utf-8') != page:
            parser.exit(1, 'site-reference: web/reference.html is stale; run scripts/site-reference.py\n')
        return
    args.page.write_text(page, encoding='utf-8')
    print(f'Wrote {args.page.name} ({len(snapshot["commands"])} commands)', file=sys.stderr)


if __name__ == '__main__':
    main()
