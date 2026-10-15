# Retained and bidirectional text selection

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
the existing inactive behavior. Focus, blink and input-method gating are unchanged. The
caller's per-window style supplies a neutral inactive selection color.

The additive `visual_geometry(fn)` builder defaults to `None`. Its optional
adapter reads the real cached paragraph and supplies separate visual advance
intervals, bidi embedding levels and the logical focus endpoint. The input draws each interval behind
one paragraph; the same endpoint supplies horizontal scroll, pointer-coordinate
conversion, caret placement and input-method anchoring. Other callers keep the
upstream geometry. Read-only `State::paragraph`, `State::paragraph_text` and
`TextInput::text_origin` expose that same cached text and its unclipped logical
origin for native text accessibility, without exposing or replacing editor state.
`State::visual_selection` exposes retained anchor/focus affinities; callers can
resolve both endpoints with `VisualGeometry::caret_x` for native text bounds.

Capturefab's adapter uses cached cosmic-text glyph cluster byte ranges, advance
bounds and bidi levels. It maps extended graphemes into those advances and merges
only touching selected visual intervals, preserving unselected visual gaps. Secure
inputs use their actual masked paragraph. There is no second shaping operation.
The adapter requires a complete single-line layout and exact grapheme/byte
coverage. Multiline values, wrapped or partial layouts, and unsynchronized cursor
ranges return `None`, retaining upstream geometry without truncating editor state.

For opted-in, nonsecure fields, unmodified Left/Right move through actual visual
caret positions. Shift extends the real logical range while retaining both
endpoints' strong/weak affinity; pointer hits retain the shaped edge they hit.
Strong positions follow insertion in the paragraph's base direction; weak
positions follow the opposite direction. Adjacent cluster edges and their
embedding levels distinguish the two, including virtual base-direction edges
at the start/end of a paragraph. Movement scans the resolved positions in the
requested screen direction, skipping coincident positions rather than getting
stuck at a run boundary. It does not sort logical indices into an apparent order.
The same retained focus positions the caret, horizontal scroll and IME anchor.
Actual nonempty IME composition, secure fields, word modifiers, Home/End and
deletion retain the upstream paths. IME support merely being opened does not
disable ordinary visual navigation. Editors without the adapter retain their
existing behavior; stale affinity is invalidated by text/cursor changes.

The strong/weak boundary model and visual-movement behavior are based on the
primary [Pango cursor-position documentation](https://docs.gtk.org/Pango/method.Layout.get_cursor_pos.html)
and [visual-movement documentation](https://docs.gtk.org/Pango/method.Layout.move_cursor_visually.html),
checked against [Pango's direction/embedding-level implementation](https://github.com/GNOME/pango/blob/1.58.2/pango/pango-layout.c).
No Pango dependency or second shaping operation is introduced.

This remains a bounded single-line editor correction, not full bidirectional
editor conformance. Cached shaping
does not expose font GDEF ligature carets; positions inside a shaping cluster that
contains multiple graphemes use proportional advances, matching cosmic-text's
existing proportional hit-test fallback. Pointer hits use those same cells in
their actual bidi order. Those limitations remain explicit.

The selection layer also intersects the text bounds with the caller's viewport.
Iced renderer layers replace previous clipping bounds; without this explicit
intersection a selected range can paint outside a scrolled or clipped field.
This bounds correction applies to active and inactive selection alike.

`LICENSE` is the MIT license from the upstream repository at the recorded
commit, verified against the license in the published iced 0.14.0 package.
The additional `rustfmt.toml` is the unchanged upstream formatter configuration
at that commit. All other packaged files are byte-for-byte upstream content.

To verify the vendor and reproduce the source diff using a downloaded or cached
copy of the exact archive:

```sh
python3 vendor/iced_widget-0.14.2/verify.py /path/to/iced_widget-0.14.2.crate
```

This verifies the archive hash, compares every packaged file, and checks that
`UPSTREAM.patch` exactly describes the sole changed source. The diff has zero
context lines so blank context markers do not create trailing whitespace in the
patch artifact. Apply it only to the verified exact archive, using
`git apply --unidiff-zero UPSTREAM.patch` from its extracted crate directory.
Remove the Cargo
patch and this vendor after an upstream release provides the same selection APIs; keep
the editor rendering regression tests when upgrading.
