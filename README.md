# razerd

Minimal RGB daemon for Razer peripherals on Linux.

Controls the LED color of the **Razer Mouse Dock Pro** and a wirelessly connected **Razer Basilisk V3 Pro 35K** simultaneously, without requiring OpenRazer or any Razer software.

## Supported devices

| Device | USB ID | Connection |
|---|---|---|
| Razer Mouse Dock Pro | `1532:00A4` | USB |
| Razer Basilisk V3 Pro 35K | `1532:00CC` | Wired |
| Razer Basilisk V3 Pro 35K | `1532:00CD` | Wireless dongle |
| Razer Basilisk V3 Pro 35K | via `1532:00A4` | Wireless through dock |

When the mouse is connected wirelessly through the dock, razerd sends a single command to the dock which routes it to the mouse over the RF link — no separate USB device needed.

## Usage

```
razerd --color <COLOR>
razerd --check
razerd --battery
razerd --info
```

### Options

| Flag | Description |
|---|---|
| `--color red\|green\|blue\|white\|off` | Apply color to dock and mouse |
| `--check` | Verify devices are detected and accessible |
| `--battery` | Report mouse battery percentage and charging status |
| `--info` | Full device report: serial, firmware, battery, DPI |

### Examples

```bash
razerd --check
razerd --color blue
razerd --battery          # → ✓ Battery: 89%  (or "89% (charging)")
razerd --info
razerd --color off
```

Example `--info` output:
```
Razer Mouse Dock Pro
  Path:     /dev/hidraw0
  Serial:   PM2526U28101432
  Firmware: 2.01

Razer Basilisk V3 Pro 35K (via Dock)
  Path:     /dev/hidraw0
  Serial:   PM2516H33301682
  Firmware: 1.00
  Battery:  89%
  Charging: no
  DPI:      1800
```

## Installation

### 1. Build and install the binary

```bash
make install
```

Installs to `~/.local/bin/razerd`. Override with `PREFIX=/usr/local sudo -E make install` for a system-wide install. Make sure `~/.local/bin` is in your `PATH`.

Remove with `make uninstall`.

### 2. udev rules (grant non-root access to the dock)

```bash
sudo tee /etc/udev/rules.d/99-razerd.rules << 'EOF'
SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", ATTR{idVendor}=="1532", ATTR{idProduct}=="00a4", GROUP="razerd", MODE="0660"
KERNEL=="hidraw*", ATTRS{idVendor}=="1532", ATTRS{idProduct}=="00a4", GROUP="razerd", MODE="0660"
EOF
sudo groupadd -f razerd
sudo usermod -aG razerd $USER
sudo udevadm control --reload-rules && sudo udevadm trigger
```

Log out and back in, then verify:

```bash
razerd --check
```

### 3. (Optional) systemd user service + timer

```bash
make install-service
```

This installs and enables:
- `razerd.service` — applies the color once (blue by default)
- `razerd.timer` — re-fires the service every 30 seconds

The timer is important: when the wireless mouse drops off the dock's RF link (power save, lifted off the dock, etc.) it may forget the color. Re-applying periodically keeps everything in sync, which is exactly what Razer Synapse does on Windows.

To also run at **boot** before you log in:

```bash
sudo loginctl enable-linger $USER
```

**Change the color** without editing the file manually:

```bash
systemctl --user edit razerd.service     # change ExecStart
systemctl --user edit razerd.timer       # change OnUnitActiveSec interval
```

Remove with `make uninstall-service`.

### 4. (Optional) Low-battery desktop notifications

```bash
make install-notify
```

Installs a shell helper (`razerd-battery-notify`) together with a systemd user timer that polls `razerd --battery` every 5 minutes and fires a `notify-send` notification when the level drops below 20% and the mouse is not charging.

Tune the threshold with a drop-in:

```bash
systemctl --user edit razerd-battery-notify.service
# then add:
#   [Service]
#   Environment=RAZERD_LOW_BATTERY=15
```

Remove with `make uninstall-notify`.

## How it works

razerd communicates with the dock via the Linux `hidraw` interface using `HIDIOCSFEATURE` ioctls — no kernel driver detachment, no libusb.

The Razer Mouse Dock Pro (`1532:00A4`) exposes three HID interfaces on USB. All LED commands go through **interface 0** (`/dev/hidraw0`). The dock firmware routes commands to the appropriate target based on the `data_size` field in the 90-byte Razer HID report:

| `data_size` | `byte[12]` | LEDs | Target |
|---|---|---|---|
| `0x1D` (29) | `0x07` | 8 | Dock LED ring |
| `0x2C` (44) | `0x0C` | 13 | Basilisk V3 Pro 35K via RF |

Battery queries use command class `0x07` (power): `cmd=0x80` for level, `cmd=0x84` for charging status. The dock forwards the request over RF and the mouse's reply is read back with `HIDIOCGFEATURE`.

The protocol was reverse-engineered from USB captures of Razer Synapse on Windows using Wireshark.

> **Note:** Do not send HID feature reports to interface 2 (`/dev/hidraw2`) — it causes the dock firmware to reboot.

## Development

```bash
make build          # cargo build --release
make install        # build + copy to ~/.local/bin/
make install-service # install + enable systemd user service
make uninstall
make uninstall-service
make clean          # cargo clean
```

CI runs `cargo fmt --check`, `cargo check`, `cargo clippy -D warnings`, `cargo doc -D warnings`, and a release build on every push and PR.

Releases are cut via the **Release** GitHub Action (`workflow_dispatch`) — pick a semver bump (patch/minor/major), the workflow computes the next version from the latest tag, bumps `Cargo.toml`, tags, builds, and attaches the Linux binary to the GitHub Release.

## Dependencies

- [`clap`](https://github.com/clap-rs/clap) — CLI argument parsing
- [`anyhow`](https://github.com/dtolnay/anyhow) — error handling
- [`libc`](https://github.com/rust-lang/libc) — `ioctl` for `HIDIOCSFEATURE`

## License

MIT
