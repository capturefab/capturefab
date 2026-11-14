# Capturefab local binary decode-depth patch

This directory contains the published `async-opcua-types` 0.19.0 package from
Capturefab's existing Cargo registry cache, with its normalized `Cargo.toml` and
complete source tree. Upstream copyright/SPDX notices remain unchanged. The
included `LICENSE-MPL-2.0` is the complete license text from the official upstream
repository at the package's recorded revision.

License source:
<https://raw.githubusercontent.com/freeopcua/async-opcua/9ad28fc011002398f2e8a95696a50408a14531d9/LICENSE.md>,
16,725 bytes, SHA-256
`1f256ecad192880510e84ad60474eab7589218784b9a50bc7ceee34c2b91f1d5`.
The license steward's canonical text is <https://www.mozilla.org/en-US/MPL/2.0/>.

Upstream: <https://github.com/freeopcua/async-opcua>.
Published package checksum:
`ed08403ae2d0af9010420612d953f8edf65e1cdd3b0fa092009fc8ec08d82f9e`.
Recorded VCS revision: `9ad28fc011002398f2e8a95696a50408a14531d9`.

Local code changes are exactly one entry-held
`let _depth_lock = ctx.options().depth_lock()?;` in each of:

- `src/data_value.rs`, `BinaryDecodable for DataValue`;
- `src/diagnostic_info.rs`, `BinaryDecodable for DiagnosticInfo`.

These changes make both established recursive binary decoder paths participate
in the SDK's existing RAII depth gauge. The default remains ten simultaneous
active guards, including existing Variant and ExtensionObject guards. This is
separate from Capturefab's postdecode native-to-JSON depth limit. No public type,
signature, encoder, dependency requirement, feature or service behavior changes.
Custom type loaders and other optional encoding paths are not covered by a
universal safety claim.

The parent project supplies bounded decoder regressions in
`src/timeseries/opcua/depth_tests.rs`. Whole-tree source manifests and the exact
two-line local diff are retained with this source proposal. The local `.cargo-ok`
cache marker is not part of the vendored source. `Cargo.toml.orig` and the
published package's `Cargo.lock` remain unchanged reference files; the standalone
normalized `Cargo.toml` is the build manifest.

All modifications to these covered source files remain under MPL-2.0. Source
distributions must retain this directory's license/notices and the modified
source; executable distributors must provide its corresponding source as the
license requires. The larger Capturefab project keeps its own license. The
root release license/source-distribution wiring is reviewed separately.

Remove this local patch after a pinned upstream release implements an equivalent
guard and passes the same decoder regressions. At review time, upstream master
`cabe0c61c9fa92ea175c2cf1858e935be060027a` still had the two unguarded paths; moving
to that revision alone would not supply this fix.
