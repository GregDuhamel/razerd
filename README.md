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
```

### Options

| Flag | Description |
|---|---|
| `--color red\|green\|blue\|white\|off` | Apply color to dock and mouse |
| `--check` | Verify devices are detected and accessible |

### Examples

```bash
razerd --check
razerd --color blue
razerd --color off
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

### 3. (Optional) systemd user service for auto-apply at login/boot

```bash
make install-service
```

This installs `~/.config/systemd/user/razerd.service` and enables it so the color (blue by default) is applied automatically on session start.

To also run at **boot** before you log in:

```bash
sudo loginctl enable-linger $USER
```

**Change the color** without editing the file manually:

```bash
systemctl --user edit razerd.service
# add an override with a new ExecStart, or replace the color
```

Remove with `make uninstall-service`.

## How it works

razerd communicates with the dock via the Linux `hidraw` interface using `HIDIOCSFEATURE` ioctls — no kernel driver detachment, no libusb.

The Razer Mouse Dock Pro (`1532:00A4`) exposes three HID interfaces on USB. All LED commands go through **interface 0** (`/dev/hidraw0`). The dock firmware routes commands to the appropriate target based on the `data_size` field in the 90-byte Razer HID report:

| `data_size` | `byte[12]` | LEDs | Target |
|---|---|---|---|
| `0x1D` (29) | `0x07` | 8 | Dock LED ring |
| `0x2C` (44) | `0x0C` | 13 | Basilisk V3 Pro 35K via RF |

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
