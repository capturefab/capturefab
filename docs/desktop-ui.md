# Native desktop UI implementation audit

The objective is full alignment with Apple's Human Interface Guidelines on
macOS, with equivalent desktop conventions on Windows and Linux. This audit
records the implementation and the evidence needed to establish that objective.
It does not treat appearance changes or a successful build as runtime proof.

The audit began on October 13, 2026. The accessibility bridge described below is
implemented, rather than a proposed toolkit migration. Platform runtime checks
must supplement the source and regression evidence.

The workbench now distinguishes the last window used for commands from the
window currently receiving keyboard input. Losing app focus leaves drafts and
command context intact while selections use a neutral background. macOS also
subdues blue control accents; Windows and Linux retain their control accents.
Each native window has its own cached theme and explicit view palette, including
destination editors and image controls. Status colors and image data retain
their meaning. Inactive styling has source and widget evidence; native inactive
visual inspection remains pending. This follows Apple's
[unemphasized selection guidance](https://developer.apple.com/documentation/appkit/nscolor/unemphasizedselectedcontentbackgroundcolor?language=_8)
and Windows' separate
[inactive selection color](https://learn.microsoft.com/en-us/dotnet/api/system.windows.systemcolors.inactiveselectionhighlightbrush?view=windowsdesktop-10.0).

Camera-list and image selections also follow the focused content area. Actual
widget focus takes priority; a visible pointer press supplies the area when no
widget owns focus. Sheets suspend this tracking and retain the previous area.
Editor context is cached per native window, so editor metadata received before
a delayed activation event still enables the correct native Edit commands.

Shared sheets handle unmodified Page, arrow, Home and End keys after their
children. Editors, open menus and radio groups retain their own navigation;
inactive windows and obscured parent sheets do not scroll. Header and footer
actions remain pinned. Native window-close requests first dismiss the current
sheet or return from its editor, using the same cancellation path as Close Window.
Quit retains its explicit save-and-exit path.

Activity accepts scrolling keys only while a control inside that pane has focus.
Its bottom-anchored history moves toward older entries with Up/Page Up/Home and
newer entries with Down/Page Down/End, while Copy log remains pinned. Other
panes, inactive windows and obscured content retain their own keyboard context.

## Everyday controls and progressive disclosure

The October 15, 2026 interface pass puts live adjustments and saving ahead of
setup and diagnostics. Features starts with search, exposure mode and Quick
controls appropriate to the advertised camera type. Image geometry, acquisition
setup, device information, transport and vendor controls remain in named
expandable groups with feature counts. Search reveals matching controls across
all groups without changing their expanded state. Edited groups retain their
drafts and report an edited count; failed writes reveal their group. Camera
classification guidance and detection evidence are in Camera details.

Ordinary capture puts destination, count, format and Capture & save first. Timeout,
inactive automatic-capture setup and shell access use named disclosures.
Scheduled jobs appear when scheduling is enabled or jobs exist. Active automatic
sessions lead with their state, capacity guidance, counts and Review; their
position stays stable as suggestions arrive. Sensitivity,
minimum gap and model details are in Suggestion settings. Pending suggestions
make Review the primary action. The Video tab exposes both local recording and
stream forwarding; its persisted tab identifier and shortcut remain unchanged.
Encoder selection and per-file limits follow the start action in a disclosure.
Invalid timeout, encoder and file-limit drafts reveal their editor.

The toolbar reserves space for Capture and its destination shortcut. At smaller
widths, destination, analytics and activity use labeled accessible icons; visible
labels return as room permits, followed by the duplicate Undo/Redo controls.
Edit menus and shortcuts retain undo and redo at every width. Disclosures expose
their summaries as accessible descriptions, and retained widget slots preserve
native command identities during resizing and group expansion.

The browser puts preview and Save captures first, with summaries on collapsed
capture options, schedules, jobs, camera settings, insights, session activity and
server logs. The latest session action remains visible. Stream controls follow
the current state, and optional form validation reveals invalid hidden fields.

Verification for this pass includes 517 desktop GUI tests, strict all-target
Clippy, formatting and a rebuilt native app. Actual-widget tests cover all seven
camera kinds without losing or duplicating advertised features, search discovery,
retained drafts/editor identities, invalid advanced-field recovery, keyboard
focus reveal, and toolbar pointer access across 24 window/scale/layout cases.
An isolated macOS native run connected only the built-in simulator, started its
stream, expanded Image, found Device Scan Type through search, saved and decoded
a 640×480 PNG, recovered an invalid timeout, and exposed local recording and
encoder options. A second native run at 900×620 with Larger interface size kept
the complete Capture & save action visible. Both sessions quit normally. These
checks operated the native accessibility tree; they do not establish a complete
VoiceOver workflow or runtime behavior on Windows and Linux.

The browser passes 12 Rust tests, strict Clippy for WebAssembly and its release
build. Live desktop and 390-pixel-wide browser checks saved direct, multi-frame
and timed simulator captures, recovered invalid fields inside closed disclosures,
found features with ordinary search terms, and retained focus after stream/save
commands and polling. Documentation screenshots use isolated simulator sessions;
the explicit `inspector-storage-default` scene demonstrates the default policy
without reading or changing a user's storage ledger.

## October 16, 2026 visual review

Session analytics now keeps Acquisition and Prompt measurements tabs pinned
above the scrolling body. Measurement fields have explicit labels, and result
rows separate values from explanatory notes. Measurement charts use the actual
available width and current palette; appearance and size changes refresh their
rendering. Compact welcome content scrolls while keeping onboarding actions
visible. Video places its start action before the optional encoding editors.

The combined native renderer matrix contains 95 views: baseline screen families
in both appearances, 900×620 compact views at Larger interface size, explicit
scrolled and error fixtures, and purpose-specific camera views. These cover
area-scan, line-scan, depth, microscope, thermal, scanner and unknown profiles;
scanner and unknown profiles use synthetic display fixtures over an isolated
simulator. Additional states include active recording and forwarding, empty
destinations, pending discovery and destination tests, failed connections,
unapplied feature drafts and automatic exposure. An actual-widget Settings test
checks the body, credential actions and pinned footer at Standard, Large and
Larger interface sizes. The current library run reports 919 passed and 11 ignored.

With the desktop binary built and Pillow available, reproduce the review with:

```sh
python3 scripts/ui-screenshots.py --all \
  --binary target/debug/capturefab --output target/ui-review
```

The script writes an `index.html` gallery, contact sheets and `manifest.json` to
the output directory. The manifest records the binary and image SHA-256 values,
appearance, interface size, logical window size, scene and fixture isolation.
Use `--all --list` to inspect the full matrix, or repeat `--only NAME` alongside
`--all` for selected views. The default matrix retains its 61 views.

Headless browser review covered the public site's three pages, 25 documentation
sections and key dialogs, plus the browser client at seven widths from 320 to
1440 pixels. Fixes contain expanded download and schedule forms, preserve feature
field readability, size charts to their available width, close search on Escape
and clear stale documentation navigation highlights. The browser client discards
chart responses from a superseded layout and immediately requests the new size;
a delayed-response check covers resizing from desktop to mobile during a fetch.
Height-only viewport changes preserve the chart without an extra request.

The native macOS main window and its accessibility tree were captured after
desktop access returned. Computer Use then lost its native connection before
the separate Settings window could be inspected. Settings evidence here covers
the rendered fixture and actual-widget body geometry only; OS window behavior
and native Windows and Linux execution remain unverified. This review does not
establish full HIG completion.

## Requirements and current evidence

| Requirement | Implemented source evidence | Runtime gate or remaining gap |
| --- | --- | --- |
| Native windows and display behavior | `gui/mod.rs` uses an iced daemon with per-window event routing, native macOS traffic lights and separate Settings ownership. `gui/prefs/work_area.rs` fits the native frame to the current macOS screen's visible frame; `work_area_windows.rs` uses the monitor work area, native frame/client dimensions and DPI. X11 reads the current desktop's EWMH work area, intersects the selected monitor, subtracts frame extents and clamps the outer origin. Wayland retains the compositor's configured size and placement. Minima adapt to small displays, and macOS double-click Zoom/Fill/Minimize/None preferences are respected. `gui/refresh.rs` follows display refresh and, where the platform does not report it, measures it again once the window stays put after moving or rescaling, never under the software renderer. `gui/pacing.rs` announces frames only of the cameras shown (in the overview, the tiles in view), once a refresh at most and not again until the last one was taken; a main window that has not drawn for 250 ms after a change, or that the platform reports out of sight (macOS occlusion: hidden, minimized, covered or on another Space; elsewhere minimized), takes no frames until it draws again or comes back. Efficient performance draws at most 15 frames a second in a camera's view and 5 per overview tile, and `gui/preview.rs` keeps a picture short of 1:1 to 1280 pixels on its long side, sampling rather than averaging. | Verify dragging, native double-click behavior, full screen, close/minimize/restore and movement between differently scaled displays. X11 work areas can be conservative for partial multi-monitor panels; missing properties retain the estimated-margin fallback, and a window manager can refuse geometry requests. |
| Discoverable commands and native shortcuts | `gui/menus/native.rs` installs AppKit menus. `gui/menus.rs` supplies the shared, validated command model. `gui/menus/desktop.rs` provides Windows/Linux File/Edit/View/Camera/Help menus with F10/Alt access, mnemonics, navigation and disabled commands. `gui/keys.rs` centralizes platform chords. Commands reach the same update/history path. | Verify installed menus, validation, current window routing, text editing, undo/redo and OS commands in built applications. Windows/Linux menus are app-rendered controls; their platform behavior and native semantics require inspection. |
| App settings | `gui/settings.rs` provides System/Light/Dark, Standard/Large/Larger interface sizes, Automatic/Smooth/Efficient performance (`gui/performance.rs`, with what Automatic chose and why, or what overrides the choice) and simulated-camera discovery. On macOS, Settings is a separate native window with closing enabled and minimize/resize disabled. Other desktops and screenshot fixtures use a sheet with a named Close action. Both apply the same preferences automatically. AppKit exposes Settings and Command-comma. | Verify opening, repeated opening, close/Escape, independent main-window focus and persistence. The screenshot sheet is a fixture presentation and is not evidence of the separate macOS window. |
| Persistent preferences | `gui/prefs.rs` serializes writes, rejects stale snapshots across Saver instances and provides an acknowledged flush after file sync and rename. Root close/quit waits for that flush and retains the window if saving fails. `gui/close_error.rs` presents an accessible modal above the focused window's current content, with Retry and Cancel; its error body scrolls while actions remain visible. Retry preserves the intended close/quit, and cancellation retains drafts. | A real failed write on macOS displayed the error above native Settings, focused Retry, contained Tab navigation and cancelled with Escape while retaining Settings. Restoring the destination and pressing Return on Retry saved and quit. Rapid changes and other platform failure presentation need runtime coverage. |
| Platform appearance and display accessibility | `gui/style.rs`, `gui/motion.rs` and `gui/prefs.rs` use platform fonts and live system appearance/accessibility preferences. Increased contrast strengthens text, outlines, glass and scrollbars. Reduced motion snaps movement already in progress; efficient performance snaps fades too. Larger interface sizes apply factors 1.15 and 1.3 independently of display DPI. | Verify light/dark, Increase Contrast, Reduce Motion/Transparency and scrollbar preferences on the actual desktops. Windows contrast themes and Linux desktop settings need platform runtime coverage. |
| Keyboard operation and visible focus | `gui/button.rs` adds Tab/Shift-Tab, Enter, Space release, disabled exclusion and focus rings. Shared checkboxes, radio segments, numeric slider controls and disclosures expose corresponding behavior. `gui/pick_list.rs` adds arrow/Home/End/Page navigation, type-ahead, selection, cancellation and long-menu scrolling. `gui/keyboard_history.rs` guards modal input and forwards native clipboard edits to real editors. Scoped dialog defaults preserve popup confirmation and IME composition. `gui/accessibility/scroll.rs` reveals the freshly focused control after Tab/Shift-Tab and native focus. | The compact macOS workflow revealed Capture fully through Tab at 900×620 with Larger interface size. Complete remaining critical workflows without a pointer on each desktop, including focus restoration and native-menu editing. |
| Native assistive technology | `gui/accessibility.rs` collects actual widget semantics and bounds while an assistive technology listens: from its first request for a tree until AT-SPI reports it gone (macOS and Windows never do). Draws after input or a change of state collect at once; draws of new camera frames alone at most four times a second. `gui/accessibility/native.rs` installs AccessKit adapters on each hidden native window before showing it. Buttons, checkboxes, radio choices, sliders, disclosures, pick lists, options and text fields publish roles, names, states and supported actions. | Inspect the actual native tree and complete workflows with VoiceOver, Narrator and Orca. A source test cannot establish that the operating system exposes or operates the intended control. |
| Validation, failures and progress | Text-field semantics expose the current draft, invalid state and precise validation reason through `gui/text_input.rs`. Capture/recording output and bitrate errors appear inline and in field descriptions. Shared headings and errors use semantic Heading and Alert wrappers. Toolbar notices announce the command outcome with its details once through Alert/Status live regions; important errors remain until a new command, newer error or dismissal, and the activity log retains them. Local file output labels the action Start recording; stream output uses Start forwarding. Preview content has an Image description without a frame-arrival live region. | Verify actionable error recovery, notice announcements and reading order. Pending/completed states must be understandable without color and must not flood announcements during camera refresh. |
| Destructive actions and preservation | Destination removal has an explicit confirmation and leaves files untouched. `storage.rs` protects changed external files, active recordings and queued uploads. `gui/storage_policy.rs` reads the configured policy on a worker, and the inspector distinguishes requested values from actual app-wide/effective limits. Captions explain permanent deletion, write-triggered age enforcement and when app-wide Delete oldest overrides Stop. Loading/unavailable states avoid presenting an unknown policy as confirmed. | Runtime verify configured policy changes, failed policy reads and deletion/removal flows. Source coverage establishes policy disclosure behavior, rather than actual deletion or assistive-technology usability. |
| Layout and high DPI | iced layout uses logical units. Native accessibility bounds apply nested scroll translations and viewport clipping before native display/interface scaling. Sheets measure and reserve their actual header/footer height before allocating the scroll body. Inspector docking adapts to width; settings and sheet bodies scroll. Initial/restored sizes and auxiliary windows are fitted after interface-scale changes. Actual widget layouts cover long titles/errors, pinned actions, compact inspectors and focused-control reveal at every interface size. | Verify welcome, overview, inspector, destination editor, errors and settings at 1x/Retina and Windows/Linux scales, including longer labels. Software geometry and macOS compact checks do not establish other desktops' native hit testing or multi-monitor behavior. |
| Complete native workflows | Camera work continues through the shared session/worker model. Pointer, keyboard, menu and accessibility actions use the same messages, draft validation, command refusal and local undo history. | On each supported platform, test discovery, connect, selection, features, capture, destinations, recording, stop, failures, settings and window closure through keyboard and assistive technology. |

## CLI and agent activity in the workbench

Camera commands automatically attach to the only authenticated active GUI or
headless session. `--session NAME` pins one when several are open; `--direct`
requests a temporary worker. Desktop and browser camera focus is local to each
participant; `select ID` changes the default for CLI calls without `--camera`.
Inspection, configuration, capture and
recording can connect a missing source to the persistent session. Workflow files
apply the same commands in order; see [CLI workflows](cli-workflows.md).

`session/activity.rs` keeps command activity at the coordinator level, outside
per-camera snapshots. Entries record the actor (`id`, `name`, human/agent kind),
origin (`client` or `gui`), target,
operation, feature, label, queued/running/succeeded/failed state, submission/start/
completion timestamps, result detail, total `file_count` and saved-file previews.
Completed capture previews retain at most the first eight paths; schedule previews
advance with the job. Journal strings are capped without truncating the original
command result's file list. The timeline retains 200
completed entries plus active commands and schedules for the session lifetime; changing or
disconnecting the selected camera does not erase it. Status polling and
authenticated session probes do not generate entries. URL credentials and secret
query fields are redacted before publication.

`gui/client_activity.rs` marks new client work in the Activity indicator and
tracks each command's transitions independently so concurrent cameras can finish
out of order. Toolbar progress shows active client work while open panels and
keyboard focus stay in place. A condition-variable activity subscription wakes
the desktop immediately for each shared change; the browser uses the same
revision through the waiting `/api/activity` endpoint. Activity identifies each
participant and retains errors and saved paths. Completed external feature writes update the
normal inspector feedback; external captures update saved-file and shutter
feedback. Scheduled captures update the most recently saved path as jobs advance,
including jobs on other cameras. A completed schedule/start-forward command means
the job/encoder was accepted; schedule entries retain actor attribution and job
status/progress through completion, failure or cancellation. Ongoing forwarding
remains visible in the camera state until stopped. Camera actions explicitly
target the desktop's locally selected camera, including previews and captures.
Other participants' pending captures do not disable the human's own capture
controls; the coordinator queues same-camera commands in submission order.

This behavior can be inspected without a GUI through `status --json`'s
`result.activity`. Camera-targeted status retains all coordinator cameras,
activity, logs and devices alongside the queried camera's fields. `target_camera`
identifies that canonical camera; `active_camera` retains the session default for
CLI calls, independently of a client's local selection. Source tests cover
transition deduplication, out-of-order
completion, preserved camera attribution and redaction; runtime checks should
also exercise an agent changing a nonselected camera, a failed capture and
selection/disconnect while Activity is open on each platform. These additions do
not establish native assistive-technology or hardware compatibility by themselves.

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

`gui/accessibility/group.rs` preserves MenuBar/Menu, Dialog and AlertDialog
parent relationships, including actual controls and static labels. Each shared
SheetFrame publishes a named Dialog owning its header, scrolling body and
footer, with AccessKit's modal flag. The close error retains its independent
modal AlertDialog. The pinned macOS adapter maps Dialog to AXGroup with the
AXDialog subrole, but does not consume AccessKit's modal flag. The native
inspection below establishes the named parent and its children, rather than
verification of an OS AXModal attribute or spoken modality.
Desktop menu headers expose popup/expanded
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
Text fields expose native focus and editing actions without an AXPress submit
action; focusing or selecting a draft must not apply it. Keyboard Return keeps
the actual editor's normal submit callback.

The exact published `iced_widget` 0.14.2 source is vendored with additive,
default-off selection options. Capturefab retains the real editor's selected
range while its window is inactive and derives disconnected visual selection
ranges from the existing shaped paragraph's grapheme clusters and bidi levels.
The same visual endpoint supplies horizontal scrolling, pointer coordinates and
the IME anchor. Selection layers intersect the caller's viewport without
reshaping the text or activating an inactive caret or IME. Native TextRuns share
these cells, split at direction changes or visual discontinuities, and publish
clipped character positions, widths and direction. Ancestor scroll and display
scale apply once to their native range bounds and hit targets. The upstream diff,
license and archive verification are in `vendor/iced_widget-0.14.2/PATCH.md`.
The editor still moves Left/Right by logical grapheme index and does not retain
cursor affinity. Positions within a ligature use proportional cluster advances;
font-provided GDEF caret positions are not implemented.

Network report cards group their visible details beneath concise adapter or
camera identities. The report indicator keeps a stable short name; changing
loss and resend counters are in its description. Software semantic-tree tests
verify child ownership, full detail exactly once, stable identity and native
Click routing. Spoken reading order still requires native assistive technology.

Analytics publishes a Session totals group owning the current measurements,
with Loading represented as Status. Loading/error changes preserve the totals,
chart and Close identities. A visible measurement overview describes first and
last values, minima and peaks. Show history data opens a fixed snapshot with
axis meanings and ranges, aggregation context and eight chronological points per
page. The page chooser reaches every retained page. Refreshes leave these rows
fixed until Update history is selected. The chart and history describe the same
accepted report, including while a newer report awaits rendering. Page changes,
updates and hiding the history restore focus inside the main window without
clearing an editor's focus or selection in Settings.
Activity groups each command's timestamp, actor,
outcome and details beneath its command identity. A bounded ID cache preserves
that identity across reordering; static-text identity is scoped to its containing
group so duplicate paths in another command do not disturb it. Actor/outcome
labels and unbroken paths wrap within the row's remaining width, retaining their
complete native text. Raw worker/coordinator messages wrap the same way, and a
shared content gutter keeps both kinds of entries clear of the overlay scrollbar.

Raw log entries retain their original clock text and add optional validated Unix
milliseconds and producer UTC offset. Old entries and malformed optional metadata
remain readable without inventing an instant. The desktop formats raw and
structured Activity timestamps through one local-time path. macOS uses cached
NSDateFormatter instances and current user preferences; Windows uses
GetTimeFormatEx/GetDateFormatEx with user overrides. Visible clock labels have a
full date/zone description and tooltip, without an extra focus stop or duplicate
native text. Copies include a dated numeric offset. Timestamp identities survive
command reordering and bounded-log eviction; closed panels also prune the ID
caches. Locale changes do not create a new error announcement identity.

Activity now shows date headings for contiguous runs in each recorded source:
Diagnostics for raw logs and Command updates for structured commands. Each row's
clock, copied value and calendar-day identity use the same viewer-local instant;
localized day captions do not determine identity. Legacy logs and invalid times
show Date unavailable. Entries retain their source order and copied contents.
Permanent heading slots preserve row and timestamp identities and focus when a
command completes across midnight. Bounded heading-ID caches are pruned even
while Activity is closed.
macOS caches date captions by local calendar day and revalidates copied formatter
time zones before reusing clock/date strings. Native cache entries with another
viewer offset are rejected, so a time-zone change cannot mix an old native clock
with a newly dated copy.

Linux reads the [GNOME global 12/24-hour clock-format preference](https://raw.githubusercontent.com/GNOME/gsettings-desktop-schemas/master/schemas/org.gnome.desktop.interface.gschema.xml.in)
through optional installed GIO/GSettings and uses KDE's global
[regional LC_TIME locale](https://raw.githubusercontent.com/KDE/plasma-workspace/master/kcms/region_language/regionandlangsettingsbase.kcfg)
from plasma-localerc. The implementation preserves explicit Inherit Language and
follows the live KDE language choice rather than stale launch LC_TIME; its
precedence policy retains LC_ALL as an explicit override.
Individual Plasma clock-widget overrides are not global application settings.
One background worker reads preferences and locale files and formats clock/date
values using separate POSIX locale objects. The UI uses nonblocking cache access
and request submission, bounded to 512 entries with a two-second refresh. Absent
desktop preferences use the locale's format. Unavailable locales, cache misses
or cached values with a different viewer UTC offset use numeric fallback,
keeping dates and clocks coherent with the current local instant until refreshed.
Native GNOME/KDE preference behavior has not been executed or inspected.

Automatic-capture review exposes its actual preview as a passive Image belonging
to the named modal Dialog, with the capture identity and decision instructions.
One persistent body region provides polite loading/saving/ready/caught-up status
or an assertive failure Alert, including the native text value used for
announcements. Full errors wrap and scroll in that body; Keep, Discard and Close
retain their identities and pinned layout. Pointer movement and counter updates
do not change the live outcome. Decision entry points reject keyboard, button
and swipe actions while a settings-save alert covers review or shutdown is in
progress. Actual shared-sheet regressions cover seven outcome phases, preview
ownership, retained Close focus and Retry routing, long errors at compact sizes,
and the normal key-message path under the covering alert. These tests establish
semantics and routing, rather than spoken screen-reader behavior.

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

Native update and focus notifications are raised after releasing the adapter
map's mutable borrow. Detach removes the adapter from that map before dropping
it. This permits synchronous native accessibility callbacks during notification
delivery or teardown without reentering a borrowed map.

A bounded, nonblocking action channel feeds the workbench subscription. Native
activation, focus, increment/decrement, value editing, selected-text replacement,
popup expansion/collapse, option focus and scrolling use existing messages or
widget-local operations. Accessibility callbacks never perform camera I/O.
Disabled, removed and invalidated controls cannot dispatch stale actions.
Modal changes invalidate previous routes immediately; background guards exclude
the hidden parent subtree and close its popups while preserving editor selection.

The outer semantic wrapper collects the laid-out tree on redraw and publishes a
native update when it changes. `gui/accessibility/scroll.rs` tracks each nested
scroll viewport, transforms child coordinates into their visible positions and
clips native bounds to the intersected viewports and window. Full content bounds
are retained separately for reveal actions. Popup rows already use window
coordinates and their own scrolling, so enclosing document offsets are not
applied again.

Tab/Shift-Tab and native Focus/ScrollIntoView use a fresh operation on the current
widget tree. It locates the current control and scroll path, reveals the innermost
viewport first, then computes each outer offset from the resulting position.
Offsets round outward to whole logical pixels, preventing iced's final rounded
translation from leaving part of a focused control behind an edge. Keyboard
reveal preserves the editor's existing focus and selection; window markers keep
the operation scoped to the requested native window. Bounds then use native DPI
multiplied by interface scale. Window movement also requests a sync. The Linux
adapter obtains physical X11 client/frame positions from the borrowed live Xlib
handles. Wayland does not
provide global window positions through the same API; screen-coordinate behavior
must be verified with the compositor and AT-SPI clients rather than inferred from
an X11 result.

Alerts are atomic, assertive live regions; ordinary status is atomic and polite.
Field errors, toolbar notices and headings carry their visible meaning. Preview
images describe the camera/status without announcing every incoming frame.
Alert and Status nodes also expose the notice as their text value, which the
macOS AccessKit consumer requires when constructing an announcement; a label and
live-region metadata alone did not produce that event.

`gui/widgets/pinned_body.rs` allocates the measured sheet header and footer before
shrinking the scroll body. Long destination titles use a single visual line while
retaining their full native Heading label. Completed service/path errors remain
readable in a permanent body slot; the footer keeps a one-line summary and
tooltip, so an asynchronous outcome neither pushes actions out of the sheet nor
replaces an active field's widget state. Compact inspector layouts preserve their
scroll region and selected camera identity beside long model names.

Analytics charts use the sheet's actual logical width and current appearance
profile, with 12-point labels and 16-point captions. Narrow charts stack panels;
wider charts use two columns. The three numeric plots use SVG; the camera-volume
panel uses native app text and bars, with full names, canonical identities and
exact byte values. Its height follows its content. Native text avoids the pinned
SVG shaper's mixed-script LastResort replacement and joined-emoji failure. Actual
cached paragraphs and software pixels verify representative CJK, combining,
Arabic/ZWJ, flag and joined-emoji clusters using installed system fonts. This does
not establish coverage for every font or writing system.
Cached measurements rerender off the UI thread
after resizing or appearance changes. An outdated reply retains its measurements
without installing an obsolete graphic. During a pending resize, current-width
placeholders preserve all three Image identities instead of scaling old text or
collapsing a column. Native camera values and history remain available. A
newer-report or truncation notice occupies a fixed context slot, preserving all
pager identities and keyboard focus through automatic refresh and explicit
Update history actions. Interface chart font fallbacks correct
blank text observed with installed DejaVu fonts on Linux, where a generic font
alias selected an absent family. The default CLI/HTTP SVG export retains its
original rendering profile.

## Source and build verification

Tests establish individual behavior and routing rather than platform usability.
The broad GUI regression command is:

```sh
cargo test --offline --no-default-features --features gui gui:: --lib --quiet
```

Focused coverage includes:

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
- Nested scroll transforms and clipping, native focus/scroll reveal, popup
  coordinates, unrelated-window exclusion and retained editor selection.
- Native-focus tests execute the actual iced task stream for window restoration,
  activation and scoped widget focus in order. Disabled routes cannot restore or activate a
  window, and compound controls receive their real focus callbacks.
- Actual compact destination editors and all inspector tabs, long errors/titles,
  pinned header/footer actions and full focused-control visibility after Tab and
  Shift-Tab at all interface sizes. Pointer activation uses the scrolled geometry.
- Actual Help/network sheet body offsets after Page, arrow, Home and End keys,
  with pinned actions and child editor/popup/radio priority. Modified keys,
  unfocused windows and obscured parent sheets leave their scroll position intact.
- Scoped Save/Retry defaults, platform action order, Return from every enabled
  single-line destination field, Space on the focused button and cancellation
  focus restoration. Popup selection and IME commit do not submit the enclosing
  dialog, and held Return does not submit twice.
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
- Precise capture/recording validation and recovery, stable field identities,
  correct recording/forwarding labels and a single live command announcement
  containing both outcome and details.

The latest UI validation snapshot, based on `d834c3f` plus the UI changes, passed
847 library tests, with 11 ignored (858 total), plus 12 CLI, nine remote and three
simulation integration tests: 871 passing tests. One CLI test and one doctest
were ignored. Concurrent video-analytics changes remained in the shared checkout
and were excluded from this isolated snapshot.
It includes focus recovery, panel-opening, search-field
identity, tab selection, menu semantics, native editing, save-busy forms, compact
geometry, scroll reveal, sheet keyboard navigation/native Close, per-window
focus/style, dialog defaults and X11 work-area regressions. Strict all-feature,
all-target Clippy passed with warnings treated as errors; formatting and
diff-whitespace checks passed. The suite includes the actual
restore→activate→scoped-focus task stream, native live-value consumer behavior,
stable command-group identities, chart profiles and Activity wrapping.

Window-lifecycle regressions deliver focus, geometry, close and accessibility
completion events after Settings has retired. These events preserve the main
editor and menus, cannot start daemon shutdown, and clean up a late adapter
through the live main UI callback. Separate task tests reproduce a failed Quit
while Settings is still hidden: recovery initializes and presents the alert
before focusing Retry, while an already fullscreen window retains its mode.
Late recovery completions also check current window ownership before cleanup.
These are model/task checks, supplemented by the native ordinary Settings and
failed-save workflows recorded below.

Nine actual-editor rendering regressions cover disconnected LTR/RTL selection,
reverse selection, inactive shape retention, clipping, scale, pointer placement
and the IME anchor. An actual Arabic Lam-Alef ligature checks proportional
grapheme-cell hit order. Multiline or partially wrapped paragraphs retain the
toolkit fallback. Five shaped-editor/native-action tests and three Collector
tests check consumer range boxes, hit positions, secure values, cross-run edits,
scrolling, scale and completely clipped text. Native text focus and selection
publish no submission; one keyboard Return still submits once.

The dated Activity regression renders 36 actual panels: three interface sizes,
three widths (360, 480 and 900 native pixels) and four explicit appearance
palettes. It verifies shaped date captions, row spacing, scrollbar clearance,
source order, stable identities and exactly one native Heading per caption.
Ordinary scrolling reveals the complete date while Copy remains pinned; software
pixels verify both date glyphs and the inherited title color. Representative
light and dark artifacts under `/private/tmp/capturefab-activity-dates` were
visually inspected. These are software-rendering checks with installed macOS
fonts and current locale, rather than native desktop or spoken AT verification.

The storage crash-recovery test allows up to five seconds for a fork-inherited
lease descriptor to close before checking the same exact reclaimed-byte and
lease-state assertions. An isolated self-reexec regression deterministically
holds that inherited descriptor before exec, verifies the reservation remains
active, then verifies exact reclamation after exec closes it. This is a test-only
correction; production storage behavior is unchanged.

The focused keyboard-history suite passed 24 tests, including actual pane focus,
visible pointer targeting, modal preservation and delayed window activation.
All 11 editor tests passed on macOS; the three shaped-selection rendering tests
also passed on genuine Linux. These include forward/reverse long ranges,
Unicode, secure text, alignment, actual software pixel clipping at all three
interface scales and preserved refocus behavior. The eight network tests passed.

An offline Linux GUI library check passed in a network-disabled Rust container
with the source and dependency registry mounted read-only. macOS GUI library
checks and the focused accessibility suite also passed. All 20 style tests passed
on the host and in a genuine Linux build, including platform-specific inactive
selection/control behavior and contrast. These are source/widget checks, without
native inactive-window visual evidence.
An earlier responsive chart snapshot passed 20 analytics tests and nine
shared-widget tests on genuine Linux, using the software renderer with installed
DejaVu fonts. These check actual caption/camera-row pixels, intrinsic image
dimensions and ancestor clipping across appearances and interface sizes. That
Linux evidence predates the final native camera-panel and Activity timestamp
changes and does not verify those later changes.
The final Activity wrapping regression passed on macOS; its separate Linux rerun
was not started because the Docker responsiveness probe stalled. No fonts,
toolchain components or display server were downloaded for these checks.
The default SVG fixture was regenerated after the font fallback fix and compared
byte for byte with the retained original; the files were identical. The native
camera-panel change repeated this comparison with the same result.
The actual Linux GUI preference tests passed 31 of 31, including X11 CARDINAL32
property decoding, desktop selection, monitor intersection, native frame fitting
and DPI/interface-scale conversions. The seven compact-layout and focus-reveal
tests passed with the software renderer on macOS and Linux. Measured cases cover
destination sheets down to 640×360 native size and 316-unit-wide inspectors down
to 360 native height, with Standard/Large/Larger scales, long content, actual
scrolling and action bounds. Cached Linux environments include no X11
or Wayland display server, so these checks do not provide native visual or AT-SPI
runtime evidence.

An earlier genuine Windows `x86_64-pc-windows-msvc` check passed for the GUI library,
binary and unit-test targets. The host has Homebrew Rust 1.98.1 and matching
`rust-src`, but no installed Windows standard library or Windows SDK. Cargo's
`build-std` compiled the target standard library from that source; a temporary
configuration supplies its missing cached dependencies from the installed Rust
source's vendor directory. No Windows cfg values were forced onto a macOS build.

That earlier successful snapshot contains `Cargo.toml`, `Cargo.lock`, `build.rs`, `src`,
`assets`, `benches` and `tests` under
`/private/tmp/capturefab-windows-buildstd-source-native-events-final`.
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
work-area fitting, native edit queue, disabled-field, focus, tab, selection,
scroll-reveal and native notification/teardown changes. It checks
Windows-specific code and the AccessKit Windows adapter, including native window
handles, focus, updates and teardown, plus the Win32 monitor work-area, client/frame,
DPI and window-position APIs. It compiles their unit-test targets
without executing them and does not link a Windows executable. Remaining warnings
were existing `media.rs`/`volumes.rs` Windows warnings and a feature-limited JPEG
test import. The adapter's Windows-only unused focus binding was corrected.

A fresh check of the current source, Cargo files and bundled DuckDB/SVG
dependencies used the same genuine MSVC target and scoped configuration from
`/private/tmp/capturefab-windows-window-focus-current`. It ended with exit code
101 in `libduckdb-sys` 1.4.5 before Capturefab's GUI type-check: the Windows-target
C++ compiler could not find `<memory>`, included by DuckDB's `constants.hpp:11`.
The host lacks the Windows-target C++ standard-library/toolchain headers.
The full output is retained in
`/private/tmp/capturefab-windows-window-focus-current.check.log`. The blocked
check was not retried without a toolchain change. The older successful snapshot
does not verify the latest per-window focus/style or bundled-dependency changes.

The separate `gui,s3` cross-check stopped in `ring` 0.17.14's C build because the
Windows CRT/SDK header `assert.h` is unavailable; this is a toolchain prerequisite
failure before the S3-enabled application could be checked. No SDK was downloaded
or replaced with stubs. Native Windows execution, Narrator and the other native
screen-reader workflows remain unverified. These compile/source results do not
establish runtime usability or full HIG completion.

The updated Windows clock/date formatter was checked separately against the
genuine MSVC target with `build-std`, using the actual `gui/time_format/windows.rs` source,
chrono 0.4.45 and windows 0.61.3. The isolated harness is retained under
`/private/tmp/capturefab-time-windows-check`. This verifies the formatter's Windows
types and API calls, without bypassing the full application's missing C++ headers
or establishing Windows execution. The browser timestamp changes also passed a
genuine `wasm32-unknown-unknown` library check using the installed Rust source and
cached dependencies, with artifacts under `/private/tmp/capturefab-wasm-check-target`.
No target libraries or SDK were downloaded. Browser execution remains unverified.

The actual Linux clock/date source and its test targets passed a genuine
`x86_64-unknown-linux-gnu` check and strict Clippy with warnings treated as errors,
using `build-std`, the installed Rust source and cached chrono 0.4.45, libc 0.2.190
and libloading 0.8.9. The isolated harness under
`/private/tmp/capturefab-time-linux-check` includes `gui/time_format/linux.rs`
directly. Portable tests exercise preference precedence, live inheritance,
nonblocking bounded work, paired clock/date refresh and stale-offset rejection.
The Linux-only POSIX locale test was compiled, not executed. These checks do not
establish GIO/GSettings runtime behavior, live GNOME/KDE preference changes or
Linux desktop usability; no Docker, target downloads or OS-setting changes were
used for this verification.

## Verified macOS runtime behavior

The running app's sidebar has no brand/logo block. Its top row contains Cameras,
the camera count and refresh, keeping discovery controls at the start of the
sidebar. A fresh build was visually checked at 900×620 with Larger interface size
after the explicit sidebar-branding request. No Capturefab text, logo or
branding-only space appeared in the sidebar; the top area contained Cameras,
refresh and the functional Scan network/Results row. The isolated app then quit
normally, and its descriptor's PID 14952 was absent from `ps`. VoiceOver was left
unchanged.

The refreshed isolated build also verified the sidebar and new Activity wrapping
at 900×620/Larger in Light appearance. Connecting and starting `sim:0` kept the
camera button's concise identity while its description changed from connected
to streaming. Command labels wrapped onto readable lines rather than extending
past the pane. Analytics exposed its named parent, totals group, chart Image and
camera summary; its labels were readable in the native screenshot. End reached
the last chart panel with Close still focused and pinned, and Command-W dismissed
only the sheet. With Copy activity log reached through Tab, Up revealed older
events and End returned to the latest events while the action stayed focused and
pinned. Quit ended the descriptor's PID 54785, confirmed absent from `ps`.
VoiceOver was left unchanged.

A later isolated macOS run verified that raw worker events and structured
Activity commands displayed the same local clock for reconnecting and starting
`sim:0`, with full date/zone descriptions. Analytics exposed three numeric Image
nodes, native camera names and exact byte values, measured summaries and the
history disclosure. Opening history exposed eight chronological measurements
per page, the frozen snapshot time, axis context and a working second page;
Shift-Tab moved focus from Next to Previous within the sheet. This run found that
inserting a newer-report caption replaced pager identities. The final fix keeps
that caption inside a persistent context child, and actual widget regressions
now preserve Page/Previous/Next identities and focus through refresh, explicit
Update history and truncation transitions. The inspected process (PID 65366)
quit normally. An initial rebuilt-app retest was blocked by `cgWindowNotFound`
and a subsequent computer-use timeout. A later fresh isolated launch resolved
that testing limitation: Page/Previous/Next retained their native identities
when Update history became available; the retained Next action opened page two
of the fixed snapshot, Shift-Tab reached Previous, and Return returned to page
one with focus on Page. VoiceOver was unchanged; these runs used only the
simulator and retained the earlier capture files.

After the raw-log and shared scrollbar-gutter fixes, another fresh build at the
same size and interface scale showed the complete scanner error wrapping before
the scrollbar and the command labels fitting beside it. Up and End retained
focus on Copy activity log and moved between older and newest entries. Native
Quit ended the descriptor's PID 58377 and removed `gui.json`. This check used only
the simulator and left the previous capture/recording files intact.

The built application's native accessibility tree was inspected and operated
during this pass. It exposes meaningful control labels and the preview Image.
Native accessibility actions changed appearance to Light, selected Larger
interface size and toggled simulated-camera discovery. Settings opened as a
separate native window with no minimize action and a disabled zoom control;
Command-W dismissed Settings and retained the main window.
The native Window menu listed both windows and disabled Minimize/Zoom for
Settings. Command-M minimized the main camera window while Settings remained
available. Selecting the camera window's native Window-menu entry restored it,
retained the stream and returned to the previously focused output field.

The native combo-box options selected JPEG through the regular message path.
Native text-field SetValue configured a temporary JPEG output, and native Capture
activation captured a simulator frame and saved an approximately 28 KB JPEG file.

An additional October 14 macOS workflow used an isolated test bundle/session and
the installed FFmpeg executable. Keyboard input connected `sim:0`; native Start
activation began acquisition. Command-3 opened Forward, native SetValue configured
a temporary `.mkv` output, and native actions started and stopped recording. The
saved file decoded as H.264 at 640×480 and 30 fps, with 388 readable frames.
Command-2 opened Capture; native SetValue configured a temporary `.png` output,
and Command-S saved a valid 640×480 PNG. Test outputs stayed in the isolated
session directory. Repeating Command-S refused the existing filename, preserved
the original image bytes and exposed a Details action that opened the same
failure in Activity.

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

At 900×620 native window size with Larger interface size, Tab navigation scrolled
the Capture inspector until the entire Capture button was visible. This checks
the focused-control reveal in the built app as well as the software layout tests.

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

The compact native alert separately verified the distinction between its default
and focused action: Return with Cancel focused invoked Retry, while Space on
Cancel dismissed the alert and restored focus to the destination manager's Close
button. After restoring the temporary preference destination, native Quit ended
PID 48090 and removed the isolated session's `gui.json` IPC descriptor. This
checks process/session cleanup, beyond disappearance of the app's windows.

The final smoke test after the native adapter fix used the refreshed isolated
bundle at 900×620/Larger and confirmed the sidebar had no branding, native editing
followed by keyboard input retained `👩‍💻draftx`, Command-comma/Command-W preserved
the focused address draft across Settings, and native Quit ended PID 68130,
removed `gui.json` and left a regular preferences file without a backup.

The latest fresh bundle/session smoke at 900×620/Larger connected `sim:0` through
Command-L, opened Settings with Command-comma and returned to the focused main
address draft through Command-W. Native SetValue followed by keyboard input again
retained `👩‍💻draftx`. With the guide's Close action focused, six Down arrows
visibly scrolled to lower guide content while the header and Close stayed pinned.
Clicking the actual native OS window-close control dismissed only the guide and
restored the underlying address draft and focus. Command-2, native SetValue of a
new PNG path and Command-S saved a 640×480 PNG verified by `ffprobe`; the native
notice contained the save outcome and full path. Native Quit ended the descriptor's
actual PID 92301, confirmed by its absence from `ps`, removed `gui.json` and left
the preferences intact.

A subsequent isolated 900×620/Larger run inspected the dated Activity headings
and confirmed that raw worker and command clocks described the same local date
and time. A newly created temporary DuckDB session supplied the native Review
auto captures sheet: its named preview Image and persistent status belonged to
the sheet, Close retained focus, and Right kept the displayed suggestion. The
same status node then reported the saved decision and next capture while the
Keep/Discard action identities remained stable. The temporary database was saved
and closed, automatic capture and the simulator stream were stopped, and native
Quit ended PID 879. VoiceOver and real camera devices were left unchanged.

The refreshed bidi/window-lifecycle build at 900×620/Larger displayed separate
selection intervals for `é אב` in a mixed Latin/Hebrew/Arabic/emoji address draft.
Keyboard replacement changed precisely that logical range and retained its
surroundings. This check exposed and removed an AXPress-to-submit mapping on text
fields. The final rebuilt app repeated native selection and replacement without
adding a Connect command; entering `sim:0` and pressing Return added exactly one
successful Connect command. An ordinary Settings open/close retained the draft
and address focus. With the isolated preferences file temporarily replaced by
a directory, Quit presented its named alert in Settings with Retry
focused. Restoring the preferences file and pressing Return saved and quit.
The two inspected processes, PIDs 35232 and 52827, exited and removed `gui.json`;
the preferences file remained intact. VoiceOver was unchanged. Hidden-window
races and fullscreen preservation have task evidence, rather than native timing
or fullscreen evidence from this smoke test.

An app-switch attempt did not yield an inactive-window capture. These native runs
therefore establish the recorded editing, sheet, capture and exit behavior, while
inactive appearance remains source/widget evidence.

Source and regression coverage establish separate Linux Ctrl+W close-window/sheet
and Ctrl+Q quit commands, including from a sheet or Settings. The File menu
exposes the same Quit shortcut; these distinct commands follow the
[GNOME keyboard guidance](https://developer.gnome.org/hig/reference/keyboard.html).
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
vertical layouts put Retry above Cancel. Escape and Command-period cancel the
alert on macOS. Destination Save and close-error Retry are scoped default actions;
Return invokes them on macOS while Space activates the focused control, following
Apple's [button roles](https://developer.apple.com/design/human-interface-guidelines/buttons)
and [keyboard event conventions](https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/EventOverview/HandlingKeyEvents/HandlingKeyEvents.html).
Single-line editor submission emits Save once, while an open popup or active IME
composition retains Return. Destination removal initially focuses Cancel and has
no destructive default; cancellation restores the row's removal trigger without
resetting its scroll position.

Horizontal dismissal actions place the affirmative action before Cancel on
Windows, following Microsoft's [dialog guidance](https://learn.microsoft.com/en-us/windows/apps/develop/ui/controls/dialogs-and-flyouts/dialogs),
and after Cancel on macOS/Linux. The Linux order follows the
[GNOME dialog guidance](https://developer.gnome.org/hig/patterns/feedback/dialogs.html).
On Windows/Linux, focused controls that handle Return retain that action; the
scoped default handles otherwise unconsumed Return. Actual widget regressions
cover these platform branches, including disabled defaults and preserved drafts.

## Runtime acceptance gates

Remaining focused checks include finer per-pane focus and inactive text-selection
appearance on native desktops, spoken network-card hierarchy and descriptions,
announcements, and native focus from background or minimized windows.
Linux GNOME/KDE clock and date preferences require native runtime verification,
including live changes and fallback behavior. Visible multi-day Activity grouping
also needs native inspection across midnight, completion and time-zone changes;
source/widget coverage does not establish the spoken or visual desktop behavior.
Charts expose measured history, axes and mark values through native groups;
Audio Graphs are not implemented, and spoken chart navigation remains unverified.
Native camera names have representative glyph and pixel evidence, rather than
universal coverage for every installed font or writing system. Mixed-direction
selection and native range geometry have shaped-widget and consumer coverage;
native bidi editing still needs inspection, and logical arrow navigation,
cursor affinity and exact ligature caret placement remain limitations.
Widget/rendering/task/consumer tests establish the implemented behavior and
routing; these platform interactions still require native checks.

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
