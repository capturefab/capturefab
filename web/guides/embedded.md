<!-- Generated from docs/embedded.md by scripts/site-guides.py. Edit the source, then run the script. -->

# Build and run Capturefab on embedded Linux

Capturefab runs on Yocto-style and other immutable Linux images: a read-only root file system, a home that may be read-only or unset, no `XDG_*` variables, often root, with only `/tmp`, `/run`, `/var/volatile` (tmpfs) and explicit data mounts writable. It needs no writable folder to capture: `capture -o /data/shot.png` and `record -o /data/clip.mkv` work wherever `-o` points. Everything else degrades one feature at a time, and `capturefab doctor` says which.

## Build

Build without the features that need C libraries or desktop services:

```sh
cargo build --release --target aarch64-unknown-linux-gnu --no-default-features --features usb
```

| Feature | Default | On an embedded image |
|---|---|---|
| `usb` | on | Keep for USB3 Vision cameras. Pure Rust; the image needs read/write access to `/dev/bus/usb` (see the udev rule `capturefab doctor` prints). |
| `jpeg` | on | libjpeg-turbo, built from source with CMake (and NASM on x86). Keep it when the cross toolchain has them; without it captures are PNG or raw and previews PNG. |
| `nvjpeg`, `vaapi` | on | GPU JPEG encoders (NVIDIA's nvJPEG, including on Jetson, and VA-API), pure Rust: their libraries are loaded when the host has the device, in a helper process the camera worker starts. Their output is checked against libjpeg-turbo, so they need `jpeg` to be used. |
| `keychain` | on | Saves camera passwords in the Secret Service over D-Bus, which images rarely run. Without it, `login add` is unsupported: put credentials in the camera address. |

The same features build for 32-bit ARM (`armv7-unknown-linux-gnueabihf`) and musl (`aarch64-unknown-linux-musl`, `x86_64-unknown-linux-musl`), for a static executable with no glibc dependency. In a Yocto layer, build it with the `cargo` class and pass the features through `CARGO_BUILD_FLAGS`, or cross-compile it outside and install the executable with its license files.

## FFmpeg

Webcams, RTSP/SRT streams, video files and `record` use an FFmpeg executable. Capturefab looks for it in this order:

1. `CAPTUREFAB_FFMPEG`, an absolute path;
2. the copy built into the executable, when the build embedded one (`CAPTUREFAB_FFMPEG_BINARY` at build time);
3. an `ffmpeg` next to the `capturefab` executable;
4. the first `ffmpeg` on `PATH`.

On an image, ship FFmpeg as a package of the image and leave it out of Capturefab's build: a build without `CAPTUREFAB_FFMPEG_BINARY` embeds none, and finds `/usr/bin/ffmpeg` on `PATH`, or set `CAPTUREFAB_FFMPEG=/usr/bin/ffmpeg` in the service. Captures and inspection of GigE Vision, USB3 Vision and simulated cameras need no FFmpeg.

A build that embeds FFmpeg extracts it once, on the first command that needs it, to the cache folder below, else a `cache` folder in the runtime folder, else `/tmp/capturefab-<uid>/cache`, and every later command reuses that copy (a marker beside it records the copy that was checked, so it is not hashed again). A folder mounted `noexec` is skipped, and a copy the system will not start, as under an execution policy, is marked so later commands only try to start it. Where none is writable and allows running programs, an FFmpeg on `PATH` is used instead, or the command fails with `ffmpeg_missing` naming each folder and why; `CAPTUREFAB_CACHE_DIR` names a folder that works.

## Writable folders

Capturefab keeps three kinds of files, each in the first folder of its list that is, or can be made, private to the user (mode 0700, owned, not a symbolic link) and writable. A variable is used as given, with no fallback; a cache folder of the user's that others may enter is made private, while a runtime or state folder like that is refused with a `chmod 700` hint.

| Folder | Holds | Looked for in |
|---|---|---|
| Runtime | session descriptors and locks, camera locks, previews, images passed to stdout through a session | `CAPTUREFAB_SESSION_DIR`; else `$XDG_RUNTIME_DIR/capturefab`; without it `/run/user/<uid>/capturefab` for a lingering user; `~/.cache/capturefab/sessions`; `/run/capturefab-0` for root; `$TMPDIR/capturefab-<uid>` (`/tmp` without `TMPDIR`) |
| State | presets, saved sign-ins, sessions' jobs and logs | `CAPTUREFAB_STATE_DIR`; else `CAPTUREFAB_SESSION_DIR`; else `$XDG_STATE_HOME/capturefab` or `~/.local/state/capturefab` |
| Cache | the extracted FFmpeg, GenICam XML | `CAPTUREFAB_CACHE_DIR`; else `$XDG_CACHE_HOME/capturefab` or `~/.cache/capturefab`; else the runtime folder's `cache` |

Folders are looked for only by the commands that need them, so a read-only system costs nothing to commands that never write. Without a folder:

- no runtime folder: no session starts, so each camera command opens its camera alone (no warm cameras, about as fast as `--keep 0`), camera locks are skipped and previews go to `$TMPDIR/capturefab-<uid>/inspect`, or need `--preview FILE` or `--preview none`;
- no state folder, or a read-only one: saving presets and sign-ins fails with `permission_denied` and a hint naming `CAPTUREFAB_STATE_DIR`; a read-only one baked into the image can still be read. A session keeps its jobs in memory while it runs; once it ends they are gone, and `jobs show` gives `session_not_found` with the same hint;
- no cache: GenICam XML is read from each camera every time.

A systemd service on such an image gives each a folder of its own, kept by systemd with the right owner and mode:

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

A shell or script that uses such a service's sessions sets the same `CAPTUREFAB_SESSION_DIR` to find them, and the same `CAPTUREFAB_STATE_DIR` to share its presets, saved sign-ins and the jobs of sessions that ended: the camera worker reads sign-ins from the service's state folder. `preset apply` sends the preset it read to the session, so it applies what the shell saved either way.

## Memory and CPUs

Capturefab sizes its work from what the host and its cgroups leave it: `doctor` shows the CPUs (the affinity mask and cgroup CPU quota) and the memory available under cgroup v2 `memory.max` or v1 `memory.limit_in_bytes`. Encoders and FFmpeg use one thread per available CPU, up to eight; `CAPTUREFAB_THREADS` caps them. A burst holds at most a quarter of the memory left, up to 1 GiB, and counts any frame it could not hold as dropped instead of exhausting memory. A camera's worker keeps at most two frames' buffers for the frames to come, holding no more than a twentieth of the memory left, and gives the rest back to the system once they are freed, so a warm camera does not keep a burst's peak for the commands after it.

## Check the image

```sh
capturefab doctor
capturefab --json doctor   # folders, fallbacks, read_only_root, host CPUs and memory, FFmpeg source
```
