# Capturefab icon

The open white **C** is a capture frame, the cyan ring is a lens, and the amber
dot is a recording light. Broad shapes and generous gaps keep the mark readable
at favicon size. The blue tile provides its own contrast on light and dark surfaces.

`capturefab.svg` is the editable master. Run `python3 scripts/icons.py` from the
repository root after editing it. Regeneration requires `rsvg-convert` (librsvg);
normal builds use the checked-in exports and need no image tools.

- `capturefab.ico` and `capturefab.res`: 16–256 px Windows icons, including
  intermediate display scaling sizes. MSVC builds link the architecture
  independent `.res` into `capturefab.exe` for Explorer.
- `../../web/favicon.svg`, `favicon.ico`, `favicon-32.png`, and
  `apple-touch-icon.png`: browser, website wordmark and home-screen exports.
  The touch icon is opaque because the OS applies its own corner mask.

`../Info.plist` is linked into the macOS executable's `__info_plist` section by
`build.rs`. It carries the Camera and Local Network usage strings macOS shows
when a camera or network command first runs. Keep its `CFBundleIdentifier`
(`org.capturefab.desktop`): macOS stores camera and network permissions under
it, so changing it would ask every user again.
