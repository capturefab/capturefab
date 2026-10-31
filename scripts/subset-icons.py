#!/usr/bin/env python3
"""Regenerate checked-in icon fonts from the pinned, complete upstream fonts.

Requires fonttools==4.60.1 only when regenerating assets, never when building
Capturefab. See assets/fonts/README.md for source and invocation details.
"""
import argparse
import hashlib
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]
SOURCES = {
    'REGULAR': ('Phosphor.ttf', '06b91e022b7ee899a63efced879392a74f0bacbda54e4467e9f663220d173a10'),
    'FILL': ('Phosphor-Fill.ttf', 'a53f5d2630cab5e3b7536ecb9d69d71519a2190298c22b1f8d770dd37bc2940a'),
}


def codepoints(source):
    """Read every exhaustive Icon::glyph arm, rejecting incomplete parsing."""
    enum = source.split('pub enum Icon {', 1)[1].split('\n}', 1)[0]
    variants = set(re.findall(r'^\s*(\w+),\s*$', enum, re.M))
    glyph = source.split('fn glyph(self)', 1)[1].split('pub fn transport', 1)[0]
    arms = re.findall(r"Icon::(\w+)\s*=>\s*\((REGULAR|FILL),\s*'\\u\{([0-9a-fA-F]+)\}'\)", glyph)
    if {name for name, _, _ in arms} != variants or len(arms) != len(variants):
        raise ValueError('every Icon variant must have one parsed font/codepoint mapping')
    return {weight: {int(code, 16) for _, font, code in arms if font == weight} for weight in SOURCES}


def verify(original, reduced, codes):
    """Keep each used outline, advance, family name and line metric unchanged."""
    before, after = original.getBestCmap(), reduced.getBestCmap()
    for code in codes:
        name = before[code]
        if after.get(code) != name:
            raise ValueError(f'U+{code:04X} lost its glyph name')
        if original['hmtx'][name] != reduced['hmtx'][name]:
            raise ValueError(f'U+{code:04X} changed horizontal metrics')
        first = original['glyf'][name].getCoordinates(original['glyf'])
        second = reduced['glyf'][name].getCoordinates(reduced['glyf'])
        if first != second:
            raise ValueError(f'U+{code:04X} changed its outline')
    for table, fields in [('head', ['unitsPerEm']), ('hhea', ['ascent', 'descent', 'lineGap']),
                          ('OS/2', ['sTypoAscender', 'sTypoDescender', 'sTypoLineGap', 'usWinAscent', 'usWinDescent'])]:
        for field in fields:
            if getattr(original[table], field) != getattr(reduced[table], field):
                raise ValueError(f'{table}.{field} changed')
    if original['name'].compile(original) != reduced['name'].compile(reduced):
        raise ValueError('font names or notices changed')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source-dir', type=Path, required=True, help='directory containing the complete upstream Phosphor.ttf and Phosphor-Fill.ttf')
    parser.add_argument('--output-dir', type=Path, default=ROOT / 'assets/fonts')
    args = parser.parse_args()
    import fontTools
    from fontTools import subset
    from fontTools.ttLib import TTFont
    if fontTools.__version__ != '4.60.1':
        parser.error('reproducible regeneration requires fonttools==4.60.1')
    codes = codepoints((ROOT / 'src/gui/icon.rs').read_text())
    args.output_dir.mkdir(parents=True, exist_ok=True)
    for weight, (name, digest) in SOURCES.items():
        source = args.source_dir / name
        if hashlib.sha256(source.read_bytes()).hexdigest() != digest:
            parser.error(f'{source} does not match @phosphor-icons/web 2.1.2')
        original = TTFont(source, recalcTimestamp=False)
        reduced = TTFont(source, recalcTimestamp=False)
        options = subset.Options()
        options.glyph_names = True
        options.name_IDs = ['*']
        options.name_languages = ['*']
        options.name_legacy = True
        # GUI icons use explicit Unicode glyphs with Basic shaping. Keeping
        # text ligature rules would drag in icons the GUI never requests.
        options.layout_features = []
        subsetter = subset.Subsetter(options=options)
        subsetter.populate(unicodes=codes[weight])
        subsetter.subset(reduced)
        output = args.output_dir / name
        reduced.save(output)
        # Check the saved file too, including its rebuilt cmap/loca tables.
        verify(original, TTFont(output, recalcTimestamp=False), codes[weight])
        print(f'{name}: {source.stat().st_size:,} -> {output.stat().st_size:,} bytes; {len(codes[weight])} icon glyphs verified')


if __name__ == '__main__':
    main()
