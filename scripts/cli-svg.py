#!/usr/bin/env python3
"""Render web/images/cli.svg from real capturefab output against the built-in simulator."""
import argparse
import os
import shlex
import subprocess
import tempfile
from pathlib import Path
from xml.sax.saxutils import escape

ROOT = Path(__file__).resolve().parents[1]
COMMANDS = [
    ['--simulate', 'discover'],
    ['--camera', 'sim:0', 'get', 'Width', 'Height', 'PixelFormat'],
    ['--camera', 'sim:0', 'set', 'ExposureTime=5000', 'Gain=6'],
    ['--camera', 'sim:0', 'capture', '-o', 'frame.png'],
]
WIDTH, FONT, LINE, PAD, BAR = 1200, 15, 22, 28, 40
COLUMNS = int((WIDTH - 2 * PAD) / (FONT * 0.6))

def simulated_only(discovered):
    return all(row.startswith('sim:') for row in discovered.splitlines()[1:])

def transcript(binary):
    with tempfile.TemporaryDirectory(prefix='capturefab-cli-svg-') as temp:
        env = {k: v for k, v in os.environ.items() if not k.startswith('CAPTUREFAB_')}
        env['CAPTUREFAB_SESSION_DIR'] = str(Path(temp) / 'sessions')
        lines = []
        for args in COMMANDS:
            result = subprocess.run([str(binary), *args], cwd=temp, env=env, capture_output=True, text=True, timeout=120)
            if result.returncode or result.stderr:
                raise SystemExit(f'capturefab {shlex.join(args)} exited {result.returncode}: {result.stderr.strip()}')
            if 'discover' in args and not simulated_only(result.stdout):
                raise SystemExit('discovery found real cameras; render on a machine without cameras so only the simulator appears')
            lines += ['$ capturefab ' + shlex.join(args)] + result.stdout.splitlines()
        return lines + ['$ ']

def height(lines):
    return BAR + 2 * PAD + LINE * len(lines)

def render(lines):
    if any(len(line) > COLUMNS for line in lines):
        raise ValueError(f'a transcript line exceeds {COLUMNS} columns')
    rows = []
    for index, line in enumerate(lines):
        y = BAR + PAD + FONT + index * LINE
        if line.startswith('$ '):
            rows.append(f'<text x="{PAD}" y="{y}" xml:space="preserve" fill="#a0d68d">$ <tspan fill="#e6ece1">{escape(line[2:])}</tspan></text>')
        else:
            rows.append(f'<text x="{PAD}" y="{y}" xml:space="preserve" fill="#a5b19e">{escape(line)}</text>')
    return f'''<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{height(lines)}" viewBox="0 0 {WIDTH} {height(lines)}" role="img" aria-labelledby="title">
<title id="title">capturefab discovery, configuration, and capture commands against the built-in simulator</title>
<rect width="{WIDTH}" height="{height(lines)}" rx="10" fill="#111612"/>
<path d="M0 10a10 10 0 0 1 10-10h{WIDTH - 20}a10 10 0 0 1 10 10v{BAR - 10}h-{WIDTH}z" fill="#1b251a"/>
<circle cx="24" cy="20" r="6" fill="#354030"/><circle cx="44" cy="20" r="6" fill="#354030"/><circle cx="64" cy="20" r="6" fill="#354030"/>
<text x="{WIDTH // 2}" y="25" text-anchor="middle" fill="#a5b19e" font-family="ui-monospace, SFMono-Regular, Menlo, Consolas, monospace" font-size="13">capturefab</text>
<g font-family="ui-monospace, SFMono-Regular, Menlo, Consolas, 'Liberation Mono', monospace" font-size="{FONT}">
{chr(10).join(rows)}
</g>
</svg>
'''

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT / 'target' / 'debug' / ('capturefab.exe' if os.name == 'nt' else 'capturefab'))
    parser.add_argument('--output', type=Path, default=ROOT / 'web' / 'images' / 'cli.svg')
    args = parser.parse_args()
    lines = transcript(args.binary)
    args.output.write_text(render(lines), encoding='utf-8')
    print(f'Wrote {args.output}: {WIDTH}x{height(lines)}')

if __name__ == '__main__':
    main()
