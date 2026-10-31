# Embedded icon fonts

`Phosphor.ttf` and `Phosphor-Fill.ttf` are subsets of the MIT-licensed
`@phosphor-icons/web` 2.1.2 fonts. The original license remains in
`Phosphor-LICENSE.txt`; the font name tables also retain their original notices.
Only the 46 regular and 2 filled Unicode glyphs used by `src/gui/icon.rs` are
included. Their names, outlines, horizontal metrics and font line metrics match
the complete fonts. The two files total 14,512 bytes instead of 937,888 bytes.

The normal Rust build uses these checked-in fonts and needs no font tooling.
When adding an icon, update its mapping, extend `every_glyph_is_in_its_font`, and
regenerate both subsets. That test checks every enum variant, so a missing
new glyph fails the GUI test suite.

## Regeneration

Use Python with `fonttools==4.60.1` in a development virtual environment:

```sh
python3 -m venv /tmp/capturefab-fonttools
/tmp/capturefab-fonttools/bin/pip install fonttools==4.60.1
mkdir -p /tmp/phosphor-full
git show 4d1030d1aebd308bb0650093e9aac064d82afec2:assets/fonts/Phosphor.ttf > /tmp/phosphor-full/Phosphor.ttf
git show 4d1030d1aebd308bb0650093e9aac064d82afec2:assets/fonts/Phosphor-Fill.ttf > /tmp/phosphor-full/Phosphor-Fill.ttf
/tmp/capturefab-fonttools/bin/python scripts/subset-icons.py --source-dir /tmp/phosphor-full
cargo test --lib gui::icon::tests::every_glyph_is_in_its_font
```

The full fonts can also be obtained from the upstream npm package's
`src/regular/Phosphor.ttf` and `src/fill/Phosphor-Fill.ttf`. The regeneration
script checks their SHA-256 against the original files documented in
`third_party/README.md`, reads every icon mapping, and checks each saved subset
against its original outline, name and metrics. It preserves original timestamps
and font names, with pinned FontTools output for deterministic regeneration.

Subset SHA-256:

- `Phosphor.ttf`: `3410920e2efbc4544567052c1e25c5fee5405372bef84626f221491a7a9b8f05`
- `Phosphor-Fill.ttf`: `5264214ffc9877a2f1fb98405b52a0728957d7611d70d165e06db155c0f89a95`
