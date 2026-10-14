# Native desktop UI implementation audit

The objective is full alignment with Apple's Human Interface Guidelines on
macOS, with equivalent desktop conventions on Windows and Linux. This audit
records the implementation and the evidence needed to establish that objective.
It does not treat appearance changes or a successful build as runtime proof.

The audit began on October 13, 2026. The accessibility bridge described below is
implemented, rather than a proposed toolkit migration. Platform runtime checks
must supplement the source and regression evidence.

## Requirements and current evidence

| Requirement | Implemented source evidence | Runtime gate or remaining gap |
| --- | --- | --- |
| Native windows and display behavior | `gui/mod.rs` uses an iced daemon with per-window event routing, native macOS traffic lights and separate Settings ownership. `gui/prefs/work_area.rs` fits the native frame to the current macOS screen's visible frame; `work_area_windows.rs` uses the monitor work area, native frame/client dimensions and DPI. X11 reads the current desktop's EWMH work area, intersects the selected monitor, subtracts frame extents and clamps the outer origin. Wayland retains the compositor's configured size and placement. Minima adapt to small displays, and macOS double-click Zoom/Fill/Minimize/None preferences are respected. `gui/refresh.rs` follows display refresh. | Verify dragging, native double-click behavior, full screen, close/minimize/restore and movement between differently scaled displays. X11 work areas can be conservative for partial multi-monitor panels; missing properties retain the estimated-margin fallback, and a window manager can refuse geometry requests. |
| Discoverable commands and native shortcuts | `gui/menus/native.rs` installs AppKit menus. `gui/menus.rs` supplies the shared, validated command model. `gui/menus/desktop.rs` provides Windows/Linux File/Edit/View/Camera/Help menus with F10/Alt access, mnemonics, navigation and disabled commands. `gui/keys.rs` centralizes platform chords. Commands reach the same update/history path. | Verify installed menus, validation, current window routing, text editing, undo/redo and OS commands in built applications. Windows/Linux menus are app-rendered controls; their platform behavior and native semantics require inspection. |
| App settings | `gui/settings.rs` provides System/Light/Dark, Standard/Large/Larger interface sizes and simulated-camera discovery. On macOS, Settings is a separate native window with closing enabled and minimize/resize disabled. Other desktops and screenshot fixtures use a sheet with a named Close action. Both apply the same preferences automatically. AppKit exposes Settings and Command-comma. | Verify opening, repeated opening, close/Escape, independent main-window focus and persistence. The screenshot sheet is a fixture presentation and is not evidence of the separate macOS window. |
| Persistent preferences | `gui/prefs.rs` serializes writes, rejects stale snapshots across Saver instances and provides an acknowledged flush after file sync and rename. Root close/quit waits for that flush and retains the window if saving fails. `gui/close_error.rs` presents an accessible modal above the focused window's current content, with Retry and Cancel; its error body scrolls while actions remain visible. Retry preserves the intended close/quit, and cancellation retains drafts. | A real failed write on macOS displayed the error above native Settings, focused Retry, contained Tab navigation and cancelled with Escape while retaining Settings. Restoring the destination and pressing Return on Retry saved and quit. Rapid changes and other platform failure presentation need runtime coverage. |
| Platform appearance and display accessibility | `gui/style.rs`, `gui/motion.rs` and `gui/prefs.rs` use platform fonts and live system appearance/accessibility preferences. Increased contrast strengthens text, outlines, glass and scrollbars. Reduced motion snaps movement already in progress. Larger interface sizes apply factors 1.15 and 1.3 independently of display DPI. | Verify light/dark, Increase Contrast, Reduce Motion/Transparency and scrollbar preferences on the actual desktops. Windows contrast themes and Linux desktop settings need platform runtime coverage. |
| Keyboard operation and visible focus | `gui/button.rs` adds Tab/Shift-Tab, Enter, Space release, disabled exclusion and focus rings. Shared checkboxes, radio segments, numeric slider controls and disclosures expose corresponding behavior. `gui/pick_list.rs` adds arrow/Home/End/Page navigation, type-ahead, selection, cancellation and long-menu scrolling. `gui/keyboard_history.rs` guards modal input and forwards native clipboard edits to real editors. | Complete critical workflows without a pointer. Check focus visibility, compound-control navigation, native-menu editing, dropdown cancellation inside a sheet and focus restoration. |
| Native assistive technology | `gui/accessibility.rs` collects actual widget semantics and bounds. `gui/accessibility/native.rs` installs AccessKit adapters on each hidden native window before showing it. Buttons, checkboxes, radio choices, sliders, disclosures, pick lists, options and text fields publish roles, names, states and supported actions. | Inspect the actual native tree and complete workflows with VoiceOver, Narrator and Orca. A source test cannot establish that the operating system exposes or operates the intended control. |
| Validation, failures and progress | Text-field semantics expose the current draft, invalid state and validation description through `gui/text_input.rs`. Shared headings and errors use semantic Heading and Alert wrappers. Toolbar notices use Alert/Status live regions; important errors remain until a new command, newer error or dismissal, and the activity log retains them. Preview content has an Image description without a frame-arrival live region. | Verify actionable error recovery, notice announcements and reading order. Pending/completed states must be understandable without color and must not flood announcements during camera refresh. |
| Destructive actions and preservation | Destination removal has an explicit confirmation and leaves files untouched. `storage.rs` protects changed external files, active recordings and queued uploads. `gui/storage_policy.rs` reads the configured policy on a worker, and the inspector distinguishes requested values from actual app-wide/effective limits. Captions explain permanent deletion, write-triggered age enforcement and when app-wide Delete oldest overrides Stop. Loading/unavailable states avoid presenting an unknown policy as confirmed. | Runtime verify configured policy changes, failed policy reads and deletion/removal flows. Source coverage establishes policy disclosure behavior, rather than actual deletion or assistive-technology usability. |
| Layout and high DPI | iced layout uses logical units. Root accessibility bounds multiply native display scale by interface scale. Inspector docking adapts to width; settings and sheet bodies scroll. Initial/restored sizes and auxiliary windows are fitted after interface-scale changes. | Verify welcome, overview, inspector, destination editor, errors and settings at minimum sizes, 1x/Retina and Windows/Linux scales, including longer labels. Check clipping, offscreen control bounds and hit testing after scrolling. |
| Complete native workflows | Camera work continues through the shared session/worker model. Pointer, keyboard, menu and accessibility actions use the same messages, draft validation, command refusal and local undo history. | On each supported platform, test discovery, connect, selection, features, capture, destinations, recording, stop, failures, settings and window closure through keyboard and assistive technology. |

## Implemented accessibility architecture

Capturefab uses iced 0.14 for rendered controls and adds an explicit native
semantic layer. Apple's [Accessibility guidelines](https://developer.apple.com/design/human-interface-guidelines/accessibility)
provide the relevant design requirements; actual assistive-technology testing
remains necessary.

`Control<M>` markers live in widget state. Their semantic IDs survive ordinary
view diffs, and their fallback focus IDs refer to the same focusable widget used
by keyboard traversal. The collector obtains roles, names, descriptions, current
values, selected/checked/expanded/invalid/disabled states, focus, supported actions
and actual layout bounds through iced widget operations. It collects visible text
as static labels, excludes decorative private-use icon glyphs, supplies fallback
button names from content and tooltips, and avoids duplicate text beneath named
controls. Headings, live regions and preview images use stable wrapper state.
Pick-list options and text runs retain parent relationships instead of also
appearing as unrelated root children.

`gui/accessibility/group.rs` preserves native MenuBar/Menu and AlertDialog
parent relationships, including actual controls and static labels. The close
error publishes the modal state. Desktop menu headers expose popup/expanded
states; items expose checked/radio states, shortcuts and access keys. Native
focus updates menu selection through its own message route, so F10/arrow and
assistive-technology focus agree while document editor focus remains intact.

`gui/accessibility/decorate.rs` preserves semantic actions when a component's
messages are mapped into the workbench. Destination controls therefore use the
same semantic IDs and routes as root-message controls. Tooltip naming replaces
missing and generic Button/Options names while retaining explicit names.

`gui/text_input.rs` delegates rendering and editing to the real iced text
editor. The native tree contains its current draft, selection and grapheme text
run. Native selection updates the editor itself. Value changes and selected-text
replacement use the normal input callback and validation. Native edits wait for
the workbench to acknowledge that callback and the actual editor to observe the
resulting draft before releasing the next native edit. Accepted replacements
collapse the selection at the inserted text; refused changes preserve the original
selection. Native selection operations also reserve the queue until the actual
editor consumes them, so a following replacement uses the new selection. Each
internal replay has a distinct token; invalidation cancels pending and queued
callbacks. Disabled fields remain readable and leave focus traversal. Password fields expose
masked values in both the field and text run, without publishing the secret.

Each window is created hidden. The root `WindowOpened` branch attaches the native
adapter through `window::run`, then shows and focuses the window. Adapters are
stored, updated and dropped on that window's UI thread. macOS uses
`accesskit_macos::SubclassingAdapter`; Windows uses
`accesskit_windows::SubclassingAdapter`; Linux uses `accesskit_unix::Adapter`.
The root detaches adapters before closing native windows or exiting. Versions and
lifecycle APIs are documented by the adapter sources:
[macOS 0.22.2](https://docs.rs/accesskit_macos/0.22.2/accesskit_macos/),
[Windows 0.29.2](https://docs.rs/accesskit_windows/0.29.2/accesskit_windows/),
[Linux 0.17.2](https://docs.rs/accesskit_unix/0.17.2/accesskit_unix/).

A bounded, nonblocking action channel feeds the workbench subscription. Native
activation, focus, increment/decrement, value editing, selected-text replacement,
popup expansion/collapse, option focus and scrolling use existing messages or
widget-local operations. Accessibility callbacks never perform camera I/O.
Disabled, removed and invalidated controls cannot dispatch stale actions.
Modal changes invalidate previous routes immediately; background guards exclude
the hidden parent subtree and close its popups while preserving editor selection.

The outer semantic wrapper collects the laid-out tree on redraw and publishes a
native update when it changes. Bounds use native DPI multiplied by interface
scale. Window movement also requests a sync. The Linux adapter obtains physical
X11 client/frame positions from the borrowed live Xlib handles. Wayland does not
provide global window positions through the same API; screen-coordinate behavior
must be verified with the compositor and AT-SPI clients rather than inferred from
an X11 result.

Alerts are atomic, assertive live regions; ordinary status is atomic and polite.
Field errors, toolbar notices and headings carry their visible meaning. Preview
images describe the camera/status without announcing every incoming frame.

## Source and build verification

Tests establish individual behavior and routing rather than platform usability.
The broad GUI regression command is:

```sh
cargo test --offline --no-default-features --features gui gui:: --lib --quiet
```

A completed broad run passed 344 tests, with zero failures. Focused
coverage includes:

- Native menu keyboard navigation, disabled/separator skipping, editor-context
  preservation and routing through the workbench command/history path.
- Modal input/IME/clipboard exclusion, real clipboard edit forwarding and Escape
  ordering when a child popup opens or closes before a new frame.
- Button activation once per gesture, disabled traversal, compound control
  commits and stable semantic IDs across view diffs.
- Grouped radio segments use cyclic arrows and Home/End to move focus and select,
  skipping disabled choices. Sliders support stepped, large-step and bound keys.
- Pick-list keyboard and native actions, option selection through normal messages,
  prefix search, Alt-arrow opening/cancellation, long-menu reveal and option
  bounds following actual native scrollbar dragging.
- Semantic bounds at a non-unit scale, parent relationships, static-label naming
  and refusal of removed/disabled native actions.
- Component message mapping, tooltip fallback naming, stable Alert identity and
  exclusion of duplicate error text or inappropriate focus actions.
- Text selection/replacement with multi-code-point graphemes, real editor focus,
  stable text-field IDs and masked password semantics. Native edit tests exercise
  acknowledgement before cursor changes, normalized/refused drafts, consecutive
  replacements, queued selection, distinct replay tokens and stale cancellation.
  Disabled fields preserve focus recovery without overriding a newer focus choice.
- Retention wording, invalid drafts, platform key conventions, four-appearance
  contrast checks, reduced-motion behavior, scaled sizing and acknowledged
  preference persistence.

A full `cargo test --offline --all-features --quiet` run passed 577 library
tests and nine CLI integration tests; nine library tests and one doctest were
ignored. This includes the final focus recovery, panel-opening, search-field
identity, tab-selection, menu semantics, native editing, save-busy form and X11
work-area regressions. Final all-feature, all-target Clippy passed with warnings
treated as errors, and formatting and diff-whitespace checks passed.

An offline Linux GUI library check passed in a network-disabled Rust container
with the source and dependency registry mounted read-only. macOS GUI library
checks and the focused accessibility suite also passed.
The actual Linux GUI preference tests passed 31 of 31, including X11 CARDINAL32
property decoding, desktop selection, monitor intersection, native frame fitting
and DPI/interface-scale conversions. Cached Linux environments include no X11
or Wayland display server, so these checks do not provide native visual or AT-SPI
runtime evidence.

A genuine Windows `x86_64-pc-windows-msvc` check passed for the GUI library,
binary and unit-test targets. The host has Homebrew Rust 1.98.1 and matching
`rust-src`, but no installed Windows standard library or Windows SDK. Cargo's
`build-std` compiled the target standard library from that source; a temporary
configuration supplies its missing cached dependencies from the installed Rust
source's vendor directory. No Windows cfg values were forced onto a macOS build.

The isolated snapshot contains `Cargo.toml`, `Cargo.lock`, `build.rs`, `src`,
`assets` and `benches` under `/private/tmp/capturefab-windows-buildstd-source`.
The scoped overrides are retained in
`/private/tmp/capturefab-windows-std-scoped-config.toml`, with compiler artifacts
under `/private/tmp/capturefab-windows-check-target`. From that snapshot, the
successful command was:

```sh
RUSTC_BOOTSTRAP=1 CARGO_TARGET_DIR=/private/tmp/capturefab-windows-check-target \
  cargo --config /private/tmp/capturefab-windows-std-scoped-config.toml \
  -Z build-std=std,panic_abort check --offline \
  --target x86_64-pc-windows-msvc --no-default-features --features gui \
  --lib --bins --tests --quiet
```

This configuration enables the GUI while disabling the other default features.
The snapshot was refreshed and the check repeated after the native Windows
work-area fitting and the final native edit queue, disabled-field, focus, tab
and selection changes. It checks
Windows-specific code and the AccessKit Windows adapter, including native window
handles, focus, updates and teardown, plus the Win32 monitor work-area, client/frame,
DPI and window-position APIs. It compiles their unit-test targets
without executing them and does not link a Windows executable. Remaining warnings
were existing `media.rs`/`volumes.rs` Windows warnings and a feature-limited JPEG
test import. The adapter's Windows-only unused focus binding was corrected.

The separate `gui,s3` cross-check stopped in `ring` 0.17.14's C build because the
Windows CRT/SDK header `assert.h` is unavailable; this is a toolchain prerequisite
failure before the S3-enabled application could be checked. No SDK was downloaded
or replaced with stubs. Native Windows execution, Narrator and the other native
screen-reader workflows remain unverified. These compile/source results do not
establish runtime usability or full HIG completion.

## Verified macOS runtime behavior

The running app's sidebar has no brand/logo block. Its top row contains Cameras,
the camera count and refresh, keeping discovery controls at the start of the
sidebar.

The built application's native accessibility tree was inspected and operated
during this pass. It exposes meaningful control labels and the preview Image.
Native accessibility actions changed appearance to Light, selected Larger
interface size and toggled simulated-camera discovery. Settings opened as a
separate native window with no minimize action and a disabled zoom control;
Command-W dismissed Settings and retained the main window.

The native combo-box options selected JPEG through the regular message path.
Native text-field SetValue configured a temporary JPEG output, and native Capture
activation captured a simulator frame and saved an approximately 28 KB JPEG file.

Keyboard-only navigation connected the simulator through Command-L and Return,
then reached and activated Start through Tab and Space. Command-F opened feature
search with focus in the field; typing the complete query retained focus and
the same native identity when the clear action appeared. Tab navigation reached
Exposure Time, Return applied a new value, and Command-Z/Shift-Command-Z undid
and redid the committed camera change. This pass caught and fixed loss of search
focus during panel opening and during the empty-to-nonempty view change.

The final built application also accepted native SetValue in the camera address
field with a composed emoji draft. Subsequent keyboard input appended at the
correct end. Native selection of that composed character followed by keyboard
replacement changed only the selected character, retaining the surrounding text.

In the S3 destination editor, Escape first closed only the dropdown. Subsequent
Escape presses returned from the draft to the destination manager, then from the
manager to the main workbench. The native folder destination picker also opened
and cancelled successfully.

An isolated preferences-write failure was exercised through the native Quit
command. The application retained its main window and exposed “Could not save
settings: Is a directory” with activity and dismissal actions. Restoring the
preferences destination and retrying native Quit flushed the file and exited.
Only the temporary test session's preferences were changed for this check.
The final alert was also tested with native Settings frontmost. Its native tree
groups the heading, error and action buttons; background Settings controls are
excluded while it is active. Retry receives focus, Tab wraps to Cancel,
Escape dismisses only the alert, and keyboard Retry exits after write recovery.
The final build separately verified Command-period cancellation, the Cancel/Retry
labels and action layout, and successful save-and-quit through Return on Retry.
This final smoke test was repeated on October 14, 2026.

On Linux, Ctrl+W closes the current window or sheet and Ctrl+Q quits the app,
including from a sheet or Settings. The File menu exposes the same Quit shortcut;
these distinct commands follow the [GNOME keyboard guidance](https://developer.gnome.org/hig/reference/keyboard.html).
Destination editor draft fields, selectors, checkboxes, folder selection and
save/navigation actions disable during a pending save. Advanced disclosure stays
available so users can read the frozen values. Actual widget tests cover folder
and S3 credential variants, disabled native actions, Tab exclusion, retained
values and stable IDs when saving completes.

These checks establish working macOS accessibility semantics and action routing
for the inspected controls. VoiceOver was left unchanged at the user's request.
VoiceOver, Narrator and Orca workflows remain
unverified, as does native Windows/Linux application execution. The recorded
checks do not establish full HIG completion.

The close error follows Apple's [Alerts guidelines](https://developer.apple.com/design/human-interface-guidelines/alerts):
Cancel dismisses the failed close, Retry is the default action, and compact
vertical layouts put the default above Cancel. Escape and Command-period cancel
the alert on macOS.

## Runtime acceptance gates

The full objective is verified only when the following checks pass on each
supported desktop and their results are recorded:

1. The built app exposes native controls with correct names, values, descriptions,
   states, relationships and bounds. Decorative glyphs and hidden panels do not
   appear as usable controls. Camera refreshes preserve meaningful control
   identity and do not move screen-reader focus unexpectedly.
2. A screen-reader user can discover/connect/select a camera, start and stop it,
   edit a feature, capture, configure an output destination, record and stop,
   inspect a failure, change settings and close sheets/windows.
3. Pointer, keyboard, menu and native accessibility actions reach the same command
   validation and refusal paths. Focus stays within an open sheet and returns to
   useful content when it closes. The separate Settings window retains ordinary
   multiwindow behavior and routes commands by focused window.
4. Text drafts, cursor/selection, replacement, clipboard edits and invalid fields
   remain usable through native editing. Checkboxes, radio segments, sliders,
   combo boxes, options, disclosures and tabs report their actual state.
5. Errors and pending/completed operations are perceivable without color alone.
   Notices announce at an appropriate priority and frequent frame/discovery
   updates do not interrupt reading.
6. Layout, hit testing and native bounds remain correct at 1x/high DPI and all
   interface sizes, after scrolling and moving between monitors. Small display
   work areas do not clip window controls or essential actions.
7. macOS Accessibility Inspector with VoiceOver, Windows UI Automation inspection
   with Narrator, and Linux AT-SPI inspection with Orca establish actual native
   behavior. Voice Control and platform alternative-input checks supplement
   these workflows.

Apple's [Settings guidelines](https://developer.apple.com/design/human-interface-guidelines/settings)
are the reference for the App-menu Settings command, Command-comma and the
separate macOS settings presentation. Interface text scaling and display DPI are
separate concerns; both implementations require the layout checks above.
