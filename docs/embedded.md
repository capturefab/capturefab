# Build and run Capturefab on embedded Linux

You can run Capturefab on a read-only Linux image. Put captures and recordings on a writable data
mount, such as `/data`. Provide writable runtime, state and cache folders for features such as
sessions, presets and bundled FFmpeg. Run `capturefab doctor` on the target to see which paths it uses.

## Build

To build a small binary with USB3 Vision support, use only the `usb` feature:

```sh
cargo build --release --target aarch64-unknown-linux-gnu --no-default-features --features usb
```

| Feature | Default | On an embedded image |
|---|---|---|
| `usb` | on | Keep for USB3 Vision cameras. Give the process read and write access to `/dev/bus/usb`. See the udev rule reported by `capturefab doctor`. |
| `jpeg` | on | Builds libjpeg-turbo with CMake and, on x86, NASM. Without this feature, use PNG or raw for captures. Previews use PNG. |
| `nvjpeg`, `vaapi` | on | Enable GPU JPEG encoders where the driver and runtime libraries are available. Also enable `jpeg` so Capturefab can verify the encoder output with libjpeg-turbo. |
| `keychain` | on | Enables `login add` for saved camera passwords. On Linux it uses the Secret Service over D-Bus. |

Choose a target that matches the operating system. For 32-bit ARM with glibc, use
`armv7-unknown-linux-gnueabihf`. To build a static executable without glibc, use a musl target such
as `aarch64-unknown-linux-musl` or `x86_64-unknown-linux-musl`. Supply the matching cross toolchain.
In a Yocto layer, you can build with the `cargo` class and pass features through `CARGO_BUILD_FLAGS`.
Install the executable and its license files.

## FFmpeg

Webcams, RTSP/SRT streams, video files and `record` use an FFmpeg executable. Capturefab looks for it in this order:

1. `CAPTUREFAB_FFMPEG`, an absolute path (a relative one is a usage error, and an empty one counts as unset);
2. the copy built into the executable, when the build embedded one (`CAPTUREFAB_FFMPEG_BINARY` at build time);
3. an `ffmpeg` next to the `capturefab` executable;
4. the first `ffmpeg` on `PATH`.

To use the image's FFmpeg package, set `CAPTUREFAB_FFMPEG=/usr/bin/ffmpeg` in the service.
To omit the embedded copy, leave `CAPTUREFAB_FFMPEG_BINARY` unset and build without an FFmpeg
binary in `target/ffmpeg/<target>/bin/`. Cargo release builds otherwise pick up that binary.
Captures and inspections of GigE Vision, USB3 Vision and pattern simulators work without FFmpeg.

Capturefab extracts the embedded copy to the cache, runtime cache or temporary cache directory.
Each later command can reuse the extracted copy. Capturefab skips folders mounted `noexec`.
If extraction fails, it looks next to the executable and on `PATH`. If those lookups also fail,
it reports `ffmpeg_missing`. Set `CAPTUREFAB_CACHE_DIR` to a writable folder that permits execution.

## Writable folders

Capturefab uses the first suitable folder for each role. Keep these folders private to the
Capturefab user with mode 0700. Use real directories with the correct owner. If you set one of the
`CAPTUREFAB_*_DIR` variables, Capturefab uses that path for its role.

| Folder | Holds | Looked for in |
|---|---|---|
| Runtime | session descriptors and locks, camera locks, previews, images passed to stdout through a session | `CAPTUREFAB_SESSION_DIR`; else `$XDG_RUNTIME_DIR/capturefab`; without it `/run/user/<uid>/capturefab` for a lingering user; `~/.cache/capturefab/sessions`; `/run/capturefab-0` for root; `$TMPDIR/capturefab-<uid>` (`/tmp` without `TMPDIR`) |
| State | presets, saved sign-ins, sessions' jobs and logs | `CAPTUREFAB_STATE_DIR`; else `CAPTUREFAB_SESSION_DIR`; else `$XDG_STATE_HOME/capturefab` or `~/.local/state/capturefab` |
| Cache | the extracted FFmpeg, GenICam XML | `CAPTUREFAB_CACHE_DIR`; else `$XDG_CACHE_HOME/capturefab` or `~/.cache/capturefab`; else the runtime folder's `cache` |

- Without a runtime folder, ordinary camera commands can open their own camera workers.
  Sessions and camera locks are unavailable. Give `inspect` a writable `--preview FILE`, or use
  `--preview none` to skip the image.
- Without writable state, you can read presets stored in the image. Saving presets and sign-ins
  fails with `permission_denied`. Sessions can keep job records in memory until they end.
- Without a cache, Capturefab reads GenICam XML again when it opens a camera.

To let systemd create these folders with the correct owner and mode, add the following service settings.

```ini
[Service]
RuntimeDirectory=capturefab
RuntimeDirectoryMode=0700
StateDirectory=capturefab
StateDirectoryMode=0700
CacheDirectory=capturefab
CacheDirectoryMode=0700
Environment=CAPTUREFAB_SESSION_DIR=/run/capturefab
Environment=CAPTUREFAB_STATE_DIR=/var/lib/capturefab
Environment=CAPTUREFAB_CACHE_DIR=/var/cache/capturefab
```

Point `CAPTUREFAB_STATE_DIR` at the data partition when `/var/lib` does not outlive a reboot on the image.

When a shell or script uses this service, set the same `CAPTUREFAB_SESSION_DIR` to find its sessions.
Set the same `CAPTUREFAB_STATE_DIR` to share presets, saved sign-ins and finished jobs. Camera workers
read sign-ins from the service's state folder.

## Memory and CPUs

Capturefab sizes processing pools and queues from the CPU and memory available to the process.
The default processing thread limit is the available CPU count, up to eight. Set
`CAPTUREFAB_THREADS` to override it. The still capture queue uses at most a quarter of the
available memory, up to 1 GiB. When the queue is full, the result counts dropped frames.
Run `doctor --json` to read the CPU count, thread limit and memory budget under `host`.

## Check the image

```sh
capturefab doctor
capturefab --json doctor   # folders, fallbacks, read_only_root, host CPUs and memory, FFmpeg source
```
