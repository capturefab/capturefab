# Retained selection in an inactive window

This is the complete published `iced_widget` **0.14.2** crate, extracted from
the crates.io archive without rewriting its sources or manifests. The archive
SHA-256 is `b1596afa0d3109c2618e8bc12bae6c11d3064df8f95c42dfce570397dbe957ab`.
Its recorded upstream commit is
`54abf81d13fec06d4d9ac754b03ce2c3313e8a1f` in `iced-rs/iced`.

The only changed upstream file is `src/text_input.rs`. `UPSTREAM.patch` is its
complete diff against the published archive. The additive
`show_unfocused_selection(bool)` builder defaults to false, retaining iced's
existing inactive-selection behavior for callers that do not opt in.
Capturefab's text input wrapper enables it.

An opted-in, enabled input with retained editor focus and a nonempty selection
uses iced's existing selection drawing branch even while its window is not
key. The branch uses the real cached shaped paragraph, secure bullet value,
selection direction, alignment, horizontal offset and clipping. Text is still
drawn once. Collapsed selections, unfocused widgets and disabled inputs retain
the existing inactive behavior. Caret, blink, IME and keyboard handling are
unchanged. The caller's per-window style supplies a neutral inactive selection
color. Existing single-rectangle selection geometry is unchanged.

The selection layer also intersects the text bounds with the caller's viewport.
Iced renderer layers replace previous clipping bounds; without this explicit
intersection a selected range can paint outside a scrolled or clipped field.
This bounds correction applies to active and inactive selection alike.

`LICENSE` is the MIT license from the upstream repository at the recorded
commit, verified against the license in the published iced 0.14.0 package.
The additional `rustfmt.toml` is the unchanged upstream formatter configuration
at that commit. All other packaged files are byte-for-byte upstream content.

To verify the vendor and reproduce the tiny diff using a downloaded or cached
copy of the exact archive:

```sh
python3 vendor/iced_widget-0.14.2/verify.py /path/to/iced_widget-0.14.2.crate
```

This verifies the archive hash, compares every packaged file, and checks that
`UPSTREAM.patch` exactly describes the sole changed source. Remove the Cargo
patch and this vendor after an upstream release provides the same opt-in; keep
the editor rendering regression tests when upgrading.
