<!-- Generated from docs/line-computer.md by scripts/site-guides.py. Edit the source, then run the script. -->

# Run Capturefab on a line computer

A line computer is a small Linux machine beside the cameras, such as a Raspberry Pi 4 or 5, a
Jetson board or a fanless industrial PC, that runs Capturefab without a screen. This guide
installs the release, gives it access to the cameras, runs a `capturefab serve` session as a
service that starts at boot and keeps the cameras open, and controls it over SSH.

The examples use the account you log in with over SSH, here `pi` with UID 1000 (`id -u` prints
yours), and the session name `line1`. Replace them with your own.

## 1. Install

The archive you need matches the installed operating system, not the processor:

```sh
dpkg --print-architecture
```

`arm64` takes the `linux-aarch64` archive and `amd64` the `linux-x86_64` one. `armhf` is a 32-bit
system, for which there is no build: Raspberry Pi 3 and later run the 64-bit Raspberry Pi OS, so
reinstall with that. Do not go by `uname -m`, which prints `aarch64` on 32-bit Raspberry Pi OS
too. Releases need glibc 2.28 or newer: Debian 10, Ubuntu 18.10, Jetson Linux R34 and later.

The install script does the choosing and checking. It reads `dpkg --print-architecture`, refuses
`armhf`, downloads the matching archive from capturefab.com, checks its SHA-256 before extracting,
installs it in `~/.local/share/capturefab/VERSION` and links `~/.local/bin/capturefab`. It does
not use `sudo`:

```sh
curl -fsSL https://capturefab.com/install.sh | sh -s -- --dry-run   # the file and checksum it would use
curl -fsSL https://capturefab.com/install.sh | sh
capturefab doctor
```

To install by hand, download the archive and `SHA256SUMS` from the
[downloads page](https://capturefab.com/downloads.html), then (replace `VERSION`):

```sh
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf capturefab-VERSION-linux-aarch64.tar.gz
install -Dm755 capturefab-VERSION-linux-aarch64/capturefab ~/.local/bin/capturefab
capturefab doctor
```

The service below runs `/usr/local/bin/capturefab`; give it a system path with
`sudo ln -sf ~/.local/bin/capturefab /usr/local/bin/capturefab`. The link follows the script's
link, so rerunning the script upgrades the service after `sudo systemctl restart capturefab-line1`.
`capturefab doctor` prints a copyable fix under each warning; the next sections explain the ones a
line computer usually needs. Run it again after each step.

## 2. Give the account the cameras

**USB3 Vision.** A camera's `/dev/bus/usb` node must be readable and writable by the account that
runs Capturefab. One udev rule covers every USB3 Vision camera by matching the USB3 Vision
interface class. The Linux archive ships it as `share/udev/rules.d/70-capturefab-usb3vision.rules`;
install it and add the service account to `plugdev`:

```sh
sudo install -m 644 ~/.local/share/capturefab/VERSION/share/udev/rules.d/70-capturefab-usb3vision.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=usb --action=add
sudo usermod -aG plugdev pi
```

With a camera plugged in that it cannot open, `capturefab doctor` names it and prints the same
rule as a one-line fix. On a distribution without a `plugdev` group, create one
(`sudo groupadd plugdev`) or put a group of your own in the rule. `TAG+="uaccess"` covers someone
logged in at the computer; a service has no seat, so it relies on the group, which applies to new
logins and to the service once it restarts.

Linux charges every USB transfer against the kernel's usbfs memory, 16 MB by default and shared
by all USB cameras. If other USB software uses it up, frames stop and the log says
`usbfs memory is exhausted`. To give several cameras room, raise it now and at every boot, for
example to 256 MB:

```sh
echo 256 | sudo tee /sys/module/usbcore/parameters/usbfs_memory_mb
echo 'options usbcore usbfs_memory_mb=256' | sudo tee /etc/modprobe.d/60-capturefab.conf
```

Where usbcore is built into the kernel, as on Raspberry Pi OS and Jetson Linux, add
`usbcore.usbfs_memory_mb=256` to the kernel command line instead: `/boot/firmware/cmdline.txt` on
Raspberry Pi OS, the `APPEND` line of `/boot/extlinux/extlinux.conf` on Jetson.

**GigE Vision.** Linux caps every socket's receive buffer at `net.core.rmem_max`, usually
208 KiB, which loses packets from larger frames. A process allowed to administer the network
(root, or `CAP_NET_ADMIN`, as a systemd service can grant with `AmbientCapabilities`) gets the
buffer it asks for regardless; for any other, raise the limit now and at every boot:

```sh
sudo sysctl -w net.core.rmem_max=33554432
echo net.core.rmem_max=33554432 | sudo tee /etc/sysctl.d/60-capturefab.conf
```

Give the camera network its own interface where you can. `capturefab network` checks how each
GigE camera is reached (link speed, MTU, subnet), and `capturefab network --tune -c CAM -c CAM`
shares the link's bandwidth between cameras.

## 3. Know where Capturefab keeps things

| What | Where on Linux | Notes |
| --- | --- | --- |
| Runtime: running sessions and their descriptors | `$XDG_RUNTIME_DIR/capturefab`; without `XDG_RUNTIME_DIR`, `/run/user/<uid>/capturefab` for an account that lingers, otherwise `~/.cache/capturefab/sessions` | Under `/run/user` it is memory, so nothing here wears the SD card; gone at reboot |
| State: presets, the list of saved sign-ins and the logs of background sessions | `$XDG_STATE_HOME/capturefab`, normally `~/.local/state/capturefab` | On disk; survives reboots |
| Cache: the extracted FFmpeg and encoder probe results | `$XDG_CACHE_HOME/capturefab`, normally `~/.cache/capturefab` | Safe to delete |
| Captures | Where `-o` says, relative to the directory the command ran in | Never overwritten |

`capturefab --json doctor` prints these folders in `folders`. `CAPTUREFAB_SESSION_DIR=/some/folder`
puts runtime and state together in one folder; every process that should see the same sessions
must then set the same value, including your SSH shell. `CAPTUREFAB_CACHE_DIR` moves the cache.

A session is found by name in the runtime folder, so the service and your SSH login must agree on
it. An SSH login sets `XDG_RUNTIME_DIR=/run/user/1000`, but a system service does not, and logind
keeps `/run/user/1000` only while the user is logged in unless the account lingers. Let it
linger, so the folder exists from boot and survives logout:

```sh
sudo loginctl enable-linger pi
```

`capturefab doctor` in your SSH login says where a service without `XDG_RUNTIME_DIR` would keep
its sessions, and warns when that is somewhere your login does not look.

## 4. Run a session as a service

Save this as `/etc/systemd/system/capturefab-line1.service`, with your account, UID and camera:

```ini
[Unit]
Description=Capturefab session line1
Wants=network-online.target user@1000.service
After=network-online.target user@1000.service

[Service]
User=pi
# The folder an SSH login uses, so commands over SSH find the session.
Environment=XDG_RUNTIME_DIR=/run/user/1000
# Relative capture paths land in the account's home folder.
WorkingDirectory=~
ExecStart=/usr/local/bin/capturefab serve --name line1 -c gige:00c0df123456 --live
# systemctl stop sends SIGTERM to serve alone; serve then finishes its
# recordings and captures and stops its camera workers itself.
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
journalctl -u capturefab-line1 -f
```

`serve` runs the session in the foreground with no idle limit and logs to stderr, which the
journal keeps. `-c` (repeatable) opens cameras when it starts, and `--live` keeps them streaming,
so a capture takes the newest frame at once. `Wants=` and `After=user@1000.service` start the
service once logind has made `/run/user/1000`.

`serve` stops on SIGTERM, SIGHUP and Ctrl-C alike, and finishes its recordings first. Keep
`KillMode=mixed`: by default systemd signals every process of the service at once, so a camera
worker would end before `serve` could finish a recording. Jobs belong to the running process and
do not survive a restart; start them again after one.

Over SSH, as the same account, every command for a camera the session holds goes through it:

```sh
capturefab session status line1
capturefab inspect -c gige:00c0df123456
capturefab capture -c gige:00c0df123456 -o 'checks/{time}.png'
capturefab capture -c gige:00c0df123456 --every 1m --for 8h -o 'shift/{seq:4}.jpg' --background
capturefab jobs
```

`--session line1` names the session explicitly, and `CAPTUREFAB_SESSION=line1` makes it the
default. For an occasional session without systemd, `capturefab session start --name line1 -c CAM
--live --idle 8h` starts one in the background that ends after eight unused hours.

## 5. Plan for storage

Captures go where `-o` says, and Capturefab never overwrites a file. When the disk fills, a
capture or recording stops with exit 6 `storage_full` rather than deleting anything. Check the
free space on an SD card before a long time lapse (`df -h ~`), and save to a removable drive by
pointing `-o` at a folder on it once it is mounted.

Capturefab stops a write before it would take a disk below its free-space floor, so the system
and other services keep room to work: 1% of the disk, at least 512 MiB and at most 20 GiB.
`error.details` gives the `path` and its `free_bytes`. Set `CAPTUREFAB_MIN_FREE` (bytes or a
size such as `2G`; `0` turns the floor off) in the service's environment to change it, and
`capturefab doctor` warns when the disk of the folder it runs in is below it.

## 6. Small boards

`CAPTUREFAB_THREADS=2` limits image processing to two threads when the board also runs other
work, such as inference. `capturefab record` uses the Jetson hardware encoder through GStreamer
where one works, and libx264 otherwise; the result's `encoder` field says which.
