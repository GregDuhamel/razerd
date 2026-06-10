# razerd

Minimal RGB daemon for Razer peripherals on Linux.

Controls the LED color of the **Razer Mouse Dock Pro** and a wirelessly connected **Razer Basilisk V3 Pro 35K** simultaneously, without requiring OpenRazer or any Razer software.

## Supported devices

| Device | USB ID | Connection |
|---|---|---|
| Razer Mouse Dock Pro | `1532:00A4` | USB |
| Razer Basilisk V3 Pro 35K | via `1532:00A4` | Wireless through dock |

razerd talks exclusively to the dock: every command is sent to it, and the dock routes mouse commands over the RF link — no separate USB device needed. A mouse connected directly over USB cable (`1532:00CC`) or through the standalone dongle (`1532:00CD`) is **not** supported.

## Usage

```
razerd --color <COLOR>
razerd --watch <COLOR>
razerd --sensitivity <DPI>
razerd --sensitivity-stages <on|off>
razerd --check
razerd --battery
razerd --info
razerd --sniff
```

### Options

| Flag | Description |
|---|---|
| `--color red\|green\|blue\|white\|off` | Apply color to dock and mouse once |
| `--watch red\|green\|blue\|white\|off` | Hold a color, re-applying it whenever the mouse wakes (runs until stopped) |
| `--sensitivity <value>` (alias `--dpi`) | Set the sensitivity to one fixed DPI value (100–35000), disabling the Cycle Up Sensitivity Stages button |
| `--sensitivity-stages on\|off` | `on`: install the 5-stage table (400/800/1600/3200/6400) and enable the Cycle Up Sensitivity Stages button; `off`: freeze the current DPI and disable it |
| `--check` | Verify devices are detected and accessible |
| `--battery` | Report mouse battery percentage and charging status |
| `--info` | Full device report: serial, firmware, battery, DPI, stages lock state, onboard profile |
| `--sniff` | Diagnostic: dump timestamped HID input reports from the dock (Ctrl-C to stop) |

### Examples

```bash
razerd --check
razerd --color blue
razerd --battery          # → ✓ Battery: 89%  (or "89% (charging)")
razerd --sensitivity 1800        # → ✓ DPI: 1800 (stages off — Cycle Up Sensitivity Stages button disabled)
razerd --dpi 1800                # same thing (alias)
razerd --sensitivity-stages on   # → ✓ DPI: 1600 (stages on — Cycle Up Sensitivity Stages button cycles 400/800/1600/3200/6400)
razerd --sensitivity-stages off  # freeze the current DPI, disable the button
razerd --info
razerd --color off
```

### Sensitivity: `--sensitivity` and `--sensitivity-stages`

Modeled on Synapse's *Sensitivity* panel. The Cycle Up Sensitivity Stages button behind the scroll wheel cycles an onboard table of up to 5 stages, so a stray press can silently change your sensitivity.

- `--sensitivity <value>` (alias `--dpi`) is the free slider: it pins one DPI value by collapsing the stage table to a single entry — the button has nothing to cycle to and becomes inert.
- `--sensitivity-stages on` installs the 5-stage table (400/800/1600/3200/6400, 1600 active — the firmware caps the table at 5 stages) and gives the button its stages back; `off` freezes whatever DPI is currently active and disables the button.

All writes hit the live slot *and* the persistent slot (so the change applies immediately and survives sleep), then read the result back — what they print is what the sensor actually runs at.

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
  Stages:   🔒 off — locked at 1800
  Profile:  4 (blue) of 5
```

The `Stages` line is the sensitivity lock indicator: `🔒 off` means the stage table holds a single value and the Cycle Up Sensitivity Stages button is inert; `🔄 on` lists the stages the button cycles, with the active one in brackets (e.g. `400/800/[1600]/3200/6400`).

The mouse stores 5 onboard profiles, cycled with the button on its underside; the indicator LED next to it shows the active slot's color (1 white, 2 red, 3 green, 4 blue, 5 cyan). `--info` reports the active slot.

### Holding a color: `--watch`

A wireless mouse forgets its color when it goes to sleep, and the firmware can drift back to its onboard default on its own. `--watch` keeps a long-running process that re-applies the color **the moment the mouse wakes**, instead of re-firing on a fixed timer.

```bash
razerd --watch blue
# Watching /dev/hidraw0 — holding 'blue', re-applying on wake. Ctrl-C to stop.
```

How it works: the dock emits no dedicated wake event, but it resumes forwarding mouse-motion input reports the instant the mouse comes back. `--watch` waits on that input stream and treats *input resuming after a quiet gap* as a wake, re-applying the color within milliseconds. While the mouse is in use it also re-applies on a slow safety cadence (every 60s) to correct any spontaneous drift — and it stays completely idle while the mouse is asleep or absent, so there is no periodic wakeup cost.

Run it as a background service to keep your color persistent — see [Installation](#3-optional-systemd-user-service).

### Diagnostics: `--sniff`

`--sniff` opens the dock's hidraw interface and prints every HID **input** report it emits, with a timestamp relative to start. It is a read-only diagnostic — it sends nothing — used to observe how the dock behaves over time (e.g. what it reports when the wireless mouse sleeps and wakes).

```bash
razerd --sniff
# Sniffing input reports from /dev/hidraw0.
# Exercise the mouse: let it sleep, then move it to wake it.
# Press Ctrl-C to stop.
#
# [  12.767s]   8 bytes: 00 00 00 00 03 00 02 00
# [  12.768s]   8 bytes: 00 00 00 00 04 00 02 00
# ...
```

The reports are standard 8-byte mouse-motion packets — bytes 4–5 are the signed little-endian X delta, bytes 6–7 the Y delta. While the mouse is asleep the device stays silent (the read simply blocks); reports resume the instant the mouse wakes. Reading these reports is a parallel tap on hidraw and does **not** interfere with normal cursor movement.

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

### 3. (Optional) systemd user service

Keeping the color in sync after the wireless mouse sleeps or drops off the dock's RF link needs a background job:

```bash
make install-watch
```

Installs and enables `razerd-watch.service`, a single long-running process (`razerd --watch blue`) that re-applies the color **the moment the mouse wakes** and stays idle otherwise — no fixed-interval churn. See [Holding a color: `--watch`](#holding-a-color---watch).

**Change the color**:

```bash
systemctl --user edit razerd-watch.service   # change --watch <color>
```

Remove with `make uninstall-watch`.

To also run at **boot** before you log in:

```bash
sudo loginctl enable-linger $USER
```

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

Battery queries use command class `0x07` (power): `cmd=0x80` for level, `cmd=0x84` for charging status. Onboard profile queries use class `0x05`: `cmd=0x80` for the slot count, `cmd=0x84` for the active slot. DPI uses class `0x04`: `cmd=0x85` reads and `cmd=0x05` writes X/Y as big-endian u16 pairs behind a storage-slot byte (`0x00` = live/RAM — what the sensor runs at and what the Cycle Up Sensitivity Stages button updates; `0x01` = persistent). The Cycle Up Sensitivity Stages button's stage table is `cmd=0x86`/`0x06`: active stage, stage count, then up to 5 × (index, X, Y, 2 reserved); `--sensitivity` writes it with a single stage. The dock forwards the request over RF and the mouse's reply is read back with `HIDIOCGFEATURE`.

The protocol was reverse-engineered from USB captures of Razer Synapse on Windows using Wireshark.

> **Note:** Do not send HID feature reports to interface 2 (`/dev/hidraw2`) — it causes the dock firmware to reboot.

## Development

```bash
make build         # cargo build --release
make install       # build + copy to ~/.local/bin/
make install-watch # install + enable the --watch systemd service
make uninstall
make uninstall-watch
make clean         # cargo clean
```

CI runs `cargo fmt --check`, `cargo check`, `cargo clippy -D warnings`, `cargo doc -D warnings`, and a release build on every push and PR.

Releases are cut via the **Release** GitHub Action (`workflow_dispatch`) — pick a semver bump (patch/minor/major), the workflow computes the next version from the latest tag, bumps `Cargo.toml`, tags, builds, and attaches the Linux binary to the GitHub Release.

## Dependencies

- [`clap`](https://github.com/clap-rs/clap) — CLI argument parsing
- [`anyhow`](https://github.com/dtolnay/anyhow) — error handling
- [`libc`](https://github.com/rust-lang/libc) — `ioctl` for `HIDIOCSFEATURE`

## License

MIT
