# Run Capturefab on a line computer

A line computer is a small Linux machine beside the cameras, such as a Raspberry Pi 4 or 5, a Jetson board or a fanless industrial PC, that runs Capturefab without a screen. This guide installs the headless release, gives it access to the cameras, runs `capturefab serve` as a service that starts at boot, and controls it from a laptop's browser and over SSH.

The examples use the account you log in with over SSH, here `pi` with UID 1000 (`id -u` prints yours), the session name `line1` and the Tailscale address `100.101.102.103`. Replace them with your own.

## 1. Choose the archive and check the computer

The release you need matches the installed operating system, not the processor:

```sh
dpkg --print-architecture
```

`arm64` takes the `linux-aarch64` archive and `amd64` the `linux-x86_64` one. `armhf` is a 32-bit system, for which there is no build: Raspberry Pi 3 and later run the 64-bit Raspberry Pi OS, so reinstall with that. Do not go by `uname -m`, which prints `aarch64` on 32-bit Raspberry Pi OS too. Releases need glibc 2.28 or newer: Debian 10, Ubuntu 18.10, Jetson Linux R34 and later.

Take the **headless** archive. It is the CLI without the desktop window, so it is smaller and loads no display libraries, and it includes the browser client for `serve --listen` and uploads to S3, FTP and SFTP. Download it and `SHA256SUMS` from [GitHub Releases](https://github.com/capturefab/capturefab/releases), then check and install it:

```sh
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf capturefab-VERSION-linux-aarch64-headless.tar.gz
sudo install -m 755 capturefab-VERSION-linux-aarch64-headless/capturefab /usr/local/bin/capturefab
capturefab doctor
```

Keep the extracted folder's license files with the executable if you copy it to other machines. `capturefab doctor` checks the computer and prints a copyable fix under each warning; the next sections explain the ones a line computer usually needs. Run it again after each step.

## 2. Give the service account the cameras

**USB3 Vision.** A camera's `/dev/bus/usb` node must be readable and writable by the account that runs Capturefab. With the camera plugged in, `capturefab doctor` names any it cannot open and prints the rule for its vendor. By hand, find the vendor ID (the four hex digits before the colon) with `lsusb`, then:

```sh
echo 'SUBSYSTEM=="usb", ATTR{idVendor}=="VVVV", MODE="0660", GROUP="plugdev", TAG+="uaccess"' \
  | sudo tee /etc/udev/rules.d/60-capturefab-VVVV.rules >/dev/null
sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=usb
sudo usermod -aG plugdev pi
```

`TAG+="uaccess"` covers someone logged in at the computer; a service has no seat, so it relies on the `plugdev` group. A new group applies to new logins and to the service once it restarts. Unplugging and replugging the camera also applies the rule.

**GigE Vision.** Linux caps every socket's receive buffer at `net.core.rmem_max`, usually 208 KiB, which loses packets from larger frames. Raise it now and at every boot:

```sh
sudo sysctl -w net.core.rmem_max=33554432
echo net.core.rmem_max=33554432 | sudo tee /etc/sysctl.d/60-capturefab.conf
```

32 MiB holds two 16 MiB frames. Give the camera network its own interface where you can, and see [GigE network setup](../README.md#gige-network-setup) for addressing and jumbo frames.

## 3. Know where Capturefab keeps things

| What | Where on Linux | Notes |
| --- | --- | --- |
| Runtime: running sessions and their descriptors | `$XDG_RUNTIME_DIR/capturefab`, normally `/run/user/<uid>/capturefab` | Memory (tmpfs), so nothing here wears the SD card; gone at reboot |
| State: settings, saved destinations, the upload queue and the storage ledger | `$XDG_STATE_HOME/capturefab`, normally `~/.local/state/capturefab` | On disk; survives reboots |
| Cache: the extracted FFmpeg and encoder probe results | `$XDG_CACHE_HOME/capturefab`, normally `~/.cache/capturefab` | Safe to delete |
| Captures saved to a relative path | The directory the CLI command ran in; for the browser, the service's working directory | `doctor` and `storage` measure free space on the disk of `~/Capturefab` |

Camera frame buffers are anonymous shared memory, not files in these folders. `CAPTUREFAB_SESSION_DIR=/some/folder` puts runtime and state together in that one folder instead; every process that should see the same sessions and storage budget must then set the same value, including your SSH shell.

A session is found by name in the runtime folder, so the service and your SSH login must agree on it. An SSH login sets `XDG_RUNTIME_DIR=/run/user/1000`, but a system service does not, and `/run/user/1000` exists only while the user is logged in unless the account lingers. Let it linger, so the folder exists from boot and survives logout:

```sh
sudo loginctl enable-linger pi
```

## 4. Run serve as a service

Save this as `/etc/systemd/system/capturefab-line1.service`, with your account, UID and address:

```ini
[Unit]
Description=Capturefab session line1
Wants=network-online.target user-runtime-dir@1000.service
After=network-online.target user-runtime-dir@1000.service tailscaled.service

[Service]
User=pi
# The folder an SSH login uses, so `capturefab --session line1` finds the session.
Environment=XDG_RUNTIME_DIR=/run/user/1000
# Relative capture paths land in the account's home folder.
WorkingDirectory=~
ExecStart=/usr/local/bin/capturefab serve --name line1 --listen 100.101.102.103:8787
# serve stops cleanly on SIGINT: recordings are finished and the upload queue saved.
KillSignal=SIGINT
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

Keep `KillSignal=SIGINT`: systemd's default SIGTERM ends `serve` without that cleanup. `Restart=on-failure` also retries when the Tailscale address is not up yet at boot. To connect a camera and start streaming when the service starts, put the camera before the command: `capturefab --camera ID serve --name line1 --stream ...`. Camera connections and schedules belong to the running process and do not survive a restart; reapply a [workflow file](../README.md#repeatable-workflows) to set them up again.

Over SSH, as the same account, every CLI command can use the running session:

```sh
capturefab --session line1 status
capturefab --session line1 connect sim:0
capturefab --session line1 --camera sim:0 capture -o check.png
```

## 5. Open it from a laptop

`--listen` serves the browser interface and its HTTP API. Open `http://100.101.102.103:8787/` on a laptop in the same tailnet to connect cameras, change settings, watch the live preview and set up captures and schedules; the files stay on the line computer.

There is no login: anyone who can reach the listener can control the cameras and write files. Bind it to the computer's Tailscale address (`tailscale ip -4`), or to `127.0.0.1` behind Tailscale Serve for HTTPS, and limit who reaches the port with your tailnet policy and the host firewall. Never bind it to `0.0.0.0` on a shared plant network. See [remote control over a tailnet](remote-control.md) for Tailscale Serve and the HTTP API.

The browser preview is light on the line computer: each new frame is encoded once as JPEG for all open tabs together, an unchanged frame costs only a short 304 reply, and a hidden tab stops asking.

## 6. Size the storage budget to the card

Capturefab never stores more than its storage budget, 10 GiB and 10,000 files by default, and every save leaves 512 MiB free on its disk. On a 16 or 32 GB card that budget can be larger than the free space. `capturefab doctor` warns when it is and its fix prints a size that fits; check the free space and set it yourself:

```sh
df -h ~
capturefab storage configure --max-space 12GiB
capturefab storage --json
```

When the budget is full, captures stop with a `storage_full` error (exit 6) rather than deleting anything. If losing the oldest captures is acceptable, for example because the line computer uploads everything to a server, `--on-full delete-oldest` deletes them instead; files still waiting to upload are kept. Upload destinations on a computer without a desktop login need `--password-env`, an SFTP `--key-file` or S3 `--env` or `--profile`, because no OS credential store runs there; see [copy files to a server](plant-floor.md#7-copy-files-to-a-server). An SFTP key, or an AWS profile in the service account's `~/.aws/credentials`, stays in a file only that account reads. Give the service a password or AWS keys from a file only root reads, not with `Environment=` lines: every account can read unit files, and their `Environment=` through `systemctl show`.

```sh
sudo install -d -m 700 /etc/capturefab
sudo install -m 600 /dev/null /etc/capturefab/line1.env
sudoedit /etc/capturefab/line1.env
```

Put one `NAME=value` per line in it, such as `CAPTUREFAB_SFTP_PASSWORD=...`, or `AWS_ACCESS_KEY_ID=...` and `AWS_SECRET_ACCESS_KEY=...`, then add `EnvironmentFile=/etc/capturefab/line1.env` to the unit's `[Service]` section and restart the service. systemd reads the file before it switches to `User=`.

## 7. Efficiency mode

Efficiency mode spends less CPU on camera workers: fewer FFmpeg decoding threads for webcams and network streams, and a faster libx264 preset for video, at some cost in smoothness and video file size. Captures and recordings keep every frame in every mode. The default, `auto`, chooses efficient on computers with at most two CPU cores or 2 GiB of memory, or with the Linux `low-power` power profile, so a 4 GB Raspberry Pi 4 or 5 stays smooth. With several cameras on a small board, choose it in the unit:

```ini
Environment=CAPTUREFAB_EFFICIENCY=efficient
```

`capturefab doctor` shows the mode in force and why. See [host checks and efficiency mode](../README.md#host-checks-and-efficiency-mode).
