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

### Build

```bash
cargo build --release
sudo cp target/release/razerd /usr/local/bin/
```

### udev rules (required to run without sudo)

```bash
sudo tee /etc/udev/rules.d/99-razerd.rules << 'EOF'
SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", ATTR{idVendor}=="1532", ATTR{idProduct}=="00a4", GROUP="razerd", MODE="0660"
KERNEL=="hidraw*", ATTRS{idVendor}=="1532", ATTRS{idProduct}=="00a4", GROUP="razerd", MODE="0660"
EOF
sudo udevadm control --reload-rules && sudo udevadm trigger
```

Create the group and add your user:

```bash
sudo groupadd -f razerd
sudo usermod -aG razerd $USER
```

Log out and back in, then verify:

```bash
razerd --check
```

## How it works

razerd communicates with the dock via the Linux `hidraw` interface using `HIDIOCSFEATURE` ioctls — no kernel driver detachment, no libusb.

The Razer Mouse Dock Pro (`1532:00A4`) exposes three HID interfaces on USB. All LED commands go through **interface 0** (`/dev/hidraw0`). The dock firmware routes commands to the appropriate target based on the `data_size` field in the 90-byte Razer HID report:

| `data_size` | `byte[12]` | LEDs | Target |
|---|---|---|---|
| `0x1D` (29) | `0x07` | 8 | Dock LED ring |
| `0x2C` (44) | `0x0C` | 13 | Basilisk V3 Pro 35K via RF |

The protocol was reverse-engineered from USB captures of Razer Synapse on Windows using Wireshark.

> **Note:** Do not send HID feature reports to interface 2 (`/dev/hidraw2`) — it causes the dock firmware to reboot.

## Dependencies

- [`clap`](https://github.com/clap-rs/clap) — CLI argument parsing
- [`anyhow`](https://github.com/dtolnay/anyhow) — error handling
- [`libc`](https://github.com/rust-lang/libc) — `ioctl` for `HIDIOCSFEATURE`

## License

MIT
