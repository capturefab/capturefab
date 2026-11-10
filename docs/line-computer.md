# Run Capturefab on a line computer

Use a small Linux computer beside the cameras to run Capturefab without a screen. This
guide starts a camera session at boot with systemd. You can control the session over SSH.

The examples use the account `pi`, its home folder `/home/pi`, UID `1000` and the session
name `line1`. Run `id -u` to find your account's UID. Replace these example values with yours.

## 1. Install

On Debian, Ubuntu or Raspberry Pi OS, use `dpkg --print-architecture` to find the installed
system's architecture.

| Output | Archive |
| --- | --- |
| `arm64` | `linux-aarch64` |
| `amd64` | `linux-x86_64` |
| `armhf` | Install a 64-bit operating system that your board supports. Capturefab releases use 64-bit Linux. |

The Linux release also uses glibc 2.28 or newer. Download the install script as the
account that will run Capturefab.

```sh
curl -fsSLo install-capturefab.sh https://capturefab.com/install.sh
sh install-capturefab.sh --dry-run
sh install-capturefab.sh
"$HOME/.local/bin/capturefab" doctor
```

The dry run prints the selected archive, URL and SHA-256 checksum. The install verifies
that checksum before it extracts the release. The script installs into
`${XDG_DATA_HOME:-$HOME/.local/share}/capturefab/VERSION` and links the command in
`~/.local/bin`. Follow the script's PATH instructions if your shell cannot find
`capturefab`.

For a manual install, download the matching archive and `SHA256SUMS` into the same
directory. [Capturefab downloads](https://capturefab.com/downloads.html)

Replace `VERSION` with the release version. Use `linux-x86_64` in the archive and
folder names if you have an x86-64 system.

```sh
package=capturefab-VERSION-linux-aarch64
awk -v archive="$package.tar.gz" '$2 == archive' SHA256SUMS | sha256sum --check --strict - &&
  tar -xzf "$package.tar.gz" &&
  install -Dm755 "$package/capturefab" "$HOME/.local/bin/capturefab"
```

Run `"$HOME/.local/bin/capturefab" doctor` after the install. For each reported problem,
use the applicable instructions below and run `doctor` again.

## 2. Give the account camera access

### USB3 Vision

The service account must have read and write access to the camera's USB device. The
Linux archive contains the udev rule at `share/udev/rules.d/70-capturefab-usb3vision.rules`.
For a script install, set `package` to the installed release folder. For a manual install,
set it to the extracted archive folder.

```sh
package="${XDG_DATA_HOME:-$HOME/.local/share}/capturefab/VERSION"
sudo install -m 644 "$package/share/udev/rules.d/70-capturefab-usb3vision.rules" /etc/udev/rules.d/
sudo udevadm control --reload
sudo udevadm trigger --subsystem-match=usb --action=add
sudo usermod -aG plugdev pi
```

If the distribution has no `plugdev` group, create it with `sudo groupadd plugdev`
before you add the service account. The group takes effect on a new login or when the
service restarts. Run `capturefab doctor` with the camera connected to find access problems.

Linux limits the memory available to USB transfers. If the log reports
`usbfs memory is exhausted`, increase the limit. For example, set it to 256 MiB on the
running system.

```sh
echo 256 | sudo tee /sys/module/usbcore/parameters/usbfs_memory_mb
```

To keep the setting across reboots, add `usbcore.usbfs_memory_mb=256` to the kernel
command line when the kernel has built-in usbcore. Follow your distribution's boot
configuration instructions. If your distribution loads usbcore as a module, put
`options usbcore usbfs_memory_mb=256` in `/etc/modprobe.d/60-capturefab.conf`.

### GigE Vision

Run `capturefab network` to see link speed, MTU and subnet details for each camera.
To share bandwidth between cameras on a link, use `capturefab network --tune` with
`-c` for each camera.

A small UDP receive buffer can cause packet loss. If `capturefab doctor` reports a low
`net.core.rmem_max`, use its suggested setting. These commands set the limit to
32 MiB now and at each boot.

```sh
sudo sysctl -w net.core.rmem_max=33554432
echo net.core.rmem_max=33554432 | sudo tee /etc/sysctl.d/60-capturefab.conf
```

## 3. Use the same runtime folder over SSH

The service and SSH commands must run as the same user and use the same runtime
folder to find each other. Enable lingering so logind keeps `/run/user/1000` available
from boot and after logout.

```sh
sudo loginctl enable-linger pi
```

| Data | Usual location on Linux |
| --- | --- |
| Running sessions and their descriptors | `$XDG_RUNTIME_DIR/capturefab`, usually `/run/user/1000/capturefab` for this example |
| Presets, job history, sign-in index and background session logs | `$XDG_STATE_HOME/capturefab` or `~/.local/state/capturefab` |
| Extracted FFmpeg, encoder probes and cached GenICam XML | `$XDG_CACHE_HOME/capturefab` or `~/.cache/capturefab` |
| Images and recordings | The output path from `-o`, relative to the client command's working directory |

Run `capturefab --json doctor` and read `folders` for the actual paths. If you set
`CAPTUREFAB_SESSION_DIR`, use the same private directory in the service and in your SSH
shell. It also holds state unless you set `CAPTUREFAB_STATE_DIR`. Set
`CAPTUREFAB_CACHE_DIR` to move the cache.

## 4. Run a session as a service

Save this unit as `/etc/systemd/system/capturefab-line1.service`. Use the path to your
installed executable in `ExecStart`. Start with `sim:0` to test the service without
hardware. For a real camera, run `capturefab list` and use its ID in place of `sim:0`.

```ini
[Unit]
Description=Capturefab session line1
Wants=network-online.target user@1000.service
After=network-online.target user@1000.service

[Service]
User=pi
Environment=XDG_RUNTIME_DIR=/run/user/1000
WorkingDirectory=/home/pi
ExecStart=/home/pi/.local/bin/capturefab serve --name line1 -c sim:0 --live
KillMode=mixed
TimeoutStopSec=45
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now capturefab-line1
systemctl status capturefab-line1
```

Use `sudo journalctl -u capturefab-line1 -f` to see the service log. `serve` opens
the cameras named with `-c` and runs in the foreground. `--live` keeps acquisition
active between commands. Use a separate `-c` for each camera.

Keep `KillMode=mixed` so systemd first signals only `serve`. This gives Capturefab time
to finish recordings and shut down its camera workers. After a restart, submit the
jobs again. If you upgrade the installed binary, use
`sudo systemctl restart capturefab-line1` to start the new version.

In your SSH shell, run these commands as `pi`. Use the camera ID from the unit with `-c`.

```sh
capturefab session status line1
capturefab inspect --session line1 -c sim:0
capturefab capture --session line1 -c sim:0 -o 'checks/{time}.png'
capturefab capture --session line1 -c sim:0 --every 1m --for 8h -o 'shift/{seq:4}.jpg' --background
capturefab jobs
```

The files go under the directory from which you ran `capturefab capture` over SSH.
Use an absolute path with `-o` to use the same destination from each login. Set
`CAPTUREFAB_SESSION=line1` to select the session by default in your shell.

For a session that starts on demand without systemd, use
`capturefab session start --name line1 -c sim:0 --live --idle 8h`. The session
ends after eight hours without requests or running jobs.

## 5. Plan for storage

Before a long capture, use `df -h` with the destination folder to see its available
space. For a removable drive, mount it before you start the capture.

Capturefab stops writes that would take the disk below its free-space floor. The
default is 1% of the disk, with a minimum of 512 MiB and a maximum of 20 GiB. The
command reports exit code `6` and error code `storage_full`. The JSON error details
include the path and may include `free_bytes`.

To set the floor for a service, add `Environment=CAPTUREFAB_MIN_FREE=2G` under
`[Service]`. Use bytes or a size such as `2G`. A value of `0` disables that floor.
Reload systemd and restart the service after the change. A client can raise the
service's floor by setting a larger value for its own command.

## 6. Limit CPU use on a small computer

Set `CAPTUREFAB_THREADS=2` in the service environment to limit the image processing
thread count Capturefab uses. On Jetson, H.264 recording can use the board's encoder
through GStreamer when available. Read `encoder` in the JSON result to see which
encoder it used.
