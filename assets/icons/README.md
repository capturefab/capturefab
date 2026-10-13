# Capturefab icon

The open white **C** is a capture frame, the cyan ring is a lens, and the amber
dot is a recording light. Broad shapes and generous gaps keep the mark readable
at favicon size. The blue tile provides its own contrast on light and dark surfaces.

`capturefab.svg` is the editable master. Run `python3 scripts/icons.py` from the
repository root after editing it. Regeneration requires `rsvg-convert` (librsvg);
normal builds use the checked-in exports and need no image tools.

- `capturefab.png`: 256 px RGBA artwork for the app's sidebar, welcome screen,
  and Linux/Windows window icons. The GUI decodes and caches it once.
- `capturefab.icns`: macOS Finder, Dock and app switcher artwork, with 16–1024 px
  representations rendered individually from the vector, including Retina sizes.
  The tile is inset to align with other macOS icons. The GUI also sets this icon
  when launched as a standalone executable.
- `capturefab-macos.png`: 1024 px preview of the macOS artwork.
- `capturefab.ico` and `capturefab.res`: 16–256 px Windows icons, including
  intermediate display scaling sizes. MSVC release builds link the architecture
  independent `.res` into the executable for Explorer and shortcuts.
- `../../web/favicon.svg`, `favicon.ico`, `favicon-32.png`, and
  `apple-touch-icon.png`: browser, website wordmark and home-screen exports.
  The touch icon is opaque because the OS applies its own corner mask.

The Linux desktop archive also includes `share/applications/capturefab.desktop`
and the SVG/PNG artwork in the standard hicolor icon directories. Install those
under `~/.local/share/`, with `capturefab` on `PATH`, to add the app to the launcher.
