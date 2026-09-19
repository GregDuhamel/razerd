# razerd

Minimal RGB daemon for Razer peripherals on Linux.

Controls the LED color of the **Razer Mouse Dock Pro** and a wirelessly connected **Razer Basilisk V3 Pro 35K** simultaneously, without requiring OpenRazer or any Razer software. It also sets the mouse sensitivity and publishes the mouse battery to UPower, so it shows up in the KDE / GNOME power applet like any other wireless peripheral.

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
razerd --upower
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
| `--upower` | Expose the mouse battery to UPower (KDE / GNOME power applets) through a virtual HID device (runs until stopped — meant for `razerd-battery.service`) |
| `--sensitivity <value>` (alias `--dpi`) | Set the sensitivity to one fixed DPI value (100–35000), disabling the Cycle Up Sensitivity Stages button |
| `--sensitivity-stages on\|off` | `on`: install the 5-stage table (400/800/1600/3200/6400) and enable the Cycle Up Sensitivity Stages button; `off`: freeze the current DPI and disable it |
| `--check` | Verify devices are detected and accessible |
| `--battery` | Report mouse battery percentage and charging status |
| `--info` | Full device report: serial, firmware, battery, DPI, stages lock state, onboard profile, and the battery exposed by `--upower` |
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

UPower battery (virtual HID device, via --upower)
  Path:     /sys/class/power_supply/hid-razerd-battery-2
  Level:    89%
  Status:   Discharging
```

The `Stages` line is the sensitivity lock indicator: `🔒 off` means the stage table holds a single value and the Cycle Up Sensitivity Stages button is inert; `🔄 on` lists the stages the button cycles, with the active one in brackets (e.g. `400/800/[1600]/3200/6400`).

The last block is the battery a running [`--upower`](#battery-in-the-desktops-power-applet---upower) currently exposes, read back from sysfs — exactly what UPower and the desktop's power applet see, so a mismatch with the mouse's own `Battery` line points at the bridge. It reads `Path: — (not exposed: bridge not running, or mouse silent)` when `razerd-battery.service` is stopped or has withdrawn the battery.

The mouse stores 5 onboard profiles, cycled with the button on its underside; the indicator LED next to it shows the active slot's color (1 white, 2 red, 3 green, 4 blue, 5 cyan). `--info` reports the active slot.

### Holding a color: `--watch`

A wireless mouse forgets its color when it goes to sleep, and the firmware can drift back to its onboard default on its own. `--watch` keeps a long-running process that re-applies the color **the moment the mouse wakes**, instead of re-firing on a fixed timer.

```bash
razerd --watch blue
# Watching /dev/hidraw0 — holding 'blue', re-applying on wake. Ctrl-C to stop.
```

How it works: the dock emits no dedicated wake event, but it resumes forwarding mouse-motion input reports the instant the mouse comes back. `--watch` waits on that input stream and treats *input resuming after a quiet gap* as a wake, re-applying the color within milliseconds. While the mouse is in use it also re-applies on a slow safety cadence (every 60s) to correct any spontaneous drift — and it stays completely idle while the mouse is asleep or absent, so there is no periodic wakeup cost.

Run it as a background service to keep your color persistent — see [Installation](#3-optional-systemd-user-service).

### Battery in the desktop's power applet: `--upower`

Desktop power applets (KDE's *Power and Battery*, GNOME's power panel) list the peripherals **UPower** knows about, and UPower only knows what the kernel registers under `/sys/class/power_supply`. The dock reports the mouse battery over Razer's vendor protocol, which the kernel does not speak — so the mouse is missing there.

`--upower` bridges the gap: it creates a virtual HID device named *Razer Basilisk V3 Pro 35K* through `/dev/uhid` whose report descriptor declares a standard *Battery Strength* field and a *Charging* bit, and mirrors the real readings into it. The kernel's generic HID battery support turns that into a `/sys/class/power_supply/hid-razerd-battery*` entry; UPower and the applet pick it up from there, with the desktop's own low-battery warning for peripherals on top.

```bash
upower -d | grep -A12 Basilisk    # once the service runs
```

Behavior:

- The **level** is refreshed once a minute. **Charging flips** are caught much faster, without polling fast all day: the charging state only changes when the mouse is put on or lifted off the dock, and either one moves it — so `--upower` watches the dock's input stream (sampling it twice a second, never following it report by report) and queries the battery 1 s after the mouse starts moving, then 2 s and 8 s after it comes to rest. Docking or lifting shows up within a few seconds.
- The battery only exists while the mouse answers. It appears with a **first real reading** — never a made-up level.
- A mouse that stops answering is asleep, switched off, out of range or unpaired; the dock reports the same "no RF reply" for all of them. After an unanswered poll `--upower` retries every 3 s, and once the silence has held for **10 seconds** the battery is **withdrawn** rather than left showing a stale level — the way a Bluetooth peripheral's battery vanishes on disconnect. One lost poll never makes it flicker. Switching the mouse off moves it, which triggers a poll: the entry is gone ~10 s later. A mouse that falls asleep is noticed by the next minute refresh. The battery comes back about a second after the mouse is moved again (or within ~10 s if it becomes reachable without moving); a silent mouse costs one ~10 ms query every 10 s.
- When the **dock** is unplugged the process exits and the battery disappears at once; the service brings it back within ~30 s of the dock returning (plus the first reading).
- The virtual device carries an inert pointer collection (no event is ever emitted on it): the kernel drops HID devices without any input capability, and UPower labels a HID battery after its sibling input device — this is what makes it a "mouse". The side effect is a second, silent *Razer Basilisk V3 Pro 35K* entry in the desktop's list of pointing devices.

`--upower` needs a handle on `/dev/uhid`, which is root-only — and should stay so. Run it through the hardened system service rather than by hand: see [Battery bridge](#4-optional-battery-in-the-desktops-power-applet).

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
make build
sudo make install
```

Installs `razerd` (and the `razerd-battery-notify` helper) to `/usr/local/bin`, root-owned. `make install` never invokes cargo, so nothing is compiled as root. Override the location with `PREFIX=...`; the systemd units follow it.

Remove with `sudo make uninstall`.

Why system-wide: `razerd-battery.service` is a system service holding a `/dev/uhid` descriptor. If it ran a binary from your home directory, any process of yours could replace that binary and inherit the descriptor.

**Upgrading from a `~/.local` install (≤ 0.9.3):** the user units used to point at `~/.local/bin`. After `sudo make install`, re-run `make install-watch` (and `make install-notify` if you use it) to refresh them, then drop the old copy: `rm ~/.local/bin/razerd ~/.local/bin/razerd-battery-notify`.

### 2. udev rules (grant non-root access to the dock)

```bash
sudo tee /etc/udev/rules.d/99-razerd.rules << 'EOF'
SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", ATTR{idVendor}=="1532", ATTR{idProduct}=="00a4", GROUP="razerd", MODE="0660"
KERNEL=="hidraw*", ATTRS{idVendor}=="1532", ATTRS{idProduct}=="00a4", GROUP="razerd", MODE="0660"
EOF
sudo groupadd -rf razerd
sudo usermod -aG razerd $USER
sudo udevadm control --reload-rules && sudo udevadm trigger
```

Log out and back in, then verify:

```bash
razerd --check
```

`-r` makes `razerd` a **system group** (GID < 1000). `systemd-udevd` ≥ 258 warns at every boot that device-node ownership by a non-system group is deprecated and will stop working in a future release, so a plain user group is not an option any more.

**Already installed with a user group?** If `getent group razerd` shows a GID ≥ 1000, recreate it as a system group, then **reboot**:

```bash
sudo groupdel razerd && sudo groupadd -r razerd && sudo usermod -aG razerd $USER
```

The reboot is not optional: udevd resolves `GROUP="razerd"` to a numeric GID when it loads the rules, your session and a lingering user manager (`user@<uid>.service`) keep the old GID in their supplementary groups, and `udevadm trigger` alone just re-applies the cached GID. Only a reboot brings udevd, the device nodes and `razerd-watch.service` back in agreement.

Why a group rather than `TAG+="uaccess"`: the ACL granted by `uaccess` only exists while you have an active seat session, so a lingering `razerd-watch.service` started at boot would have no access to the dock until you log in.

On distributions whose initramfs is built with dracut in `hostonly` mode (Fedora), the rule file is copied into the initrd verbatim, where the `razerd` group does not exist. The initrd's udevd then logs two `Failed to resolve group 'razerd', ignoring` lines very early in the boot. They are harmless — the dock is not needed before the root filesystem is mounted.

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

### 4. (Optional) Battery in the desktop's power applet

```bash
sudo make install-battery
```

Installs and enables `razerd-battery.service`, a **system** service running `razerd --upower` — see [Battery in the desktop's power applet](#battery-in-the-desktops-power-applet---upower). It starts at boot, no lingering needed. Logs: `journalctl -u razerd-battery.service`. Remove with `sudo make uninstall-battery`.

**Permissions — what this does and does not grant.** Whoever can open `/dev/uhid` can create arbitrary input devices: inject keystrokes, or feed crafted descriptors to the kernel's HID parsers. So razerd grants that to nobody:

- `/dev/uhid` stays `root:root 0600`. No udev rule, no group — your account and the `razerd` group gain nothing.
- The service manager opens the node itself and passes the descriptor to the service (`OpenFile=/dev/uhid`). On SELinux systems (Fedora) the stock policy does not let systemd do that, so `make install-battery` also loads a one-rule policy module, `contrib/razerd-uhid.cil`: `init_t` — PID 1's own domain, nothing else — may `open read write` `uhid_device_t` (what one `O_RDWR` open is checked against). It is not what keeps your account out of uhid (that is the node's `0600`, untouched), and a root process in `init_t` could already reach uhid by exec'ing into an unconfined domain — so the rule removes no effective barrier. Closing it — the process exiting for any reason — makes the kernel remove the virtual device.
- The process runs as a throwaway unprivileged user (`DynamicUser=yes`) with no capabilities, no network, no sockets (not even D-Bus), a read-only filesystem, and a closed device allow-list: the dock's hidraw (through the `razerd` group of step 2), plus `/dev/uhid` itself — required for systemd to open it on the service's behalf, and useless to the process, which has neither the ownership nor a capability to get past `0600`. Check the result with `systemd-analyze security razerd-battery.service`.
- System calls are an allow-list (`@default @basic-io @io-event @file-system @signal` + `ioctl`) rather than the usual broad `@system-service`.
- In the code, the report descriptor and the device identity are compile-time constants, and the only values ever written to the virtual device are a percentage and a charging bit — no string from the dock, and no code path that emits a key, a button or motion. Its only inputs are fixed-size replies from the dock and fixed-size events from the kernel.

### 5. (Optional) Low-battery desktop notifications

With the battery bridge above, KDE and GNOME already warn about a low peripheral battery by themselves — this notifier is for setups without it.

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

`--upower` is the one feature that does not talk to the dock alone: it writes `uhid` events (`UHID_CREATE2`, then one `UHID_INPUT2` per reading: `[report id, strength 0–100, charging bit]`) to a `/dev/uhid` descriptor inherited from systemd, and answers the kernel's `UHID_GET_REPORT` when the level is read before a report landed. The device sits on `BUS_VIRTUAL`, so only `hid-generic` binds to it — never a Razer-specific driver or userspace matcher keyed on `usb:1532:*`.

The protocol was reverse-engineered from USB captures of Razer Synapse on Windows using Wireshark.

> **Note:** Do not send HID feature reports to interface 2 (`/dev/hidraw2`) — it causes the dock firmware to reboot.

## Development

### Source layout

| File | Role |
|---|---|
| `src/main.rs` | Module wiring and the flag → action dispatch — nothing else |
| `src/hid.rs` | Hidraw transport: device discovery via sysfs, feature-report ioctls, the send/poll exchange with its response-correlation check |
| `src/protocol.rs` | The Razer report layer: 90-byte format, command constants, typed queries/writes (battery, serial, firmware, DPI, stages, profiles) |
| `src/uhid.rs` | The virtual HID battery behind `--upower`: report descriptor, hand-serialized `uhid` events, systemd fd inheritance |
| `src/cli.rs` | The clap surface: flags, parsers, flag-to-action mapping |
| `src/actions.rs` | One `run_*` function per flag: the `--watch` loop, the `--upower` loop, the `--info` report, the sensitivity writes |
| `contrib/razerd-watch.service` | systemd user unit running `razerd --watch` (installed by `make install-watch`) |
| `contrib/razerd-battery.service` | Hardened systemd **system** unit running `razerd --upower` (installed by `sudo make install-battery`) |
| `contrib/razerd-uhid.cil` | One-rule SELinux module letting systemd open `/dev/uhid` for that unit (loaded by `install-battery` when SELinux is enabled) |
| `contrib/razerd-battery-notify{,.service,.timer}` | Low-battery desktop notifier: shell helper + systemd timer (installed by `make install-notify`) |

Unit tests live next to what they test (`mod tests` per module). One hardware-gated smoke test is excluded from CI — run it with the dock connected: `cargo test -- --ignored`.

```bash
make build                # cargo build --release
sudo make install         # copy to /usr/local/bin (never builds)
make install-watch        # enable the --watch systemd user service
sudo make install-battery # enable the --upower systemd system service
sudo make uninstall
make uninstall-watch
sudo make uninstall-battery
make clean                # cargo clean
```

CI runs `cargo fmt --check`, `cargo check`, `cargo clippy -D warnings`, `cargo doc -D warnings`, and a release build on every push and PR.

Releases are cut via the **Release** GitHub Action (`workflow_dispatch`) — pick a semver bump (patch/minor/major), the workflow computes the next version from the latest tag, bumps `Cargo.toml`, tags, builds, and attaches the Linux binary to the GitHub Release.

## Dependencies

- [`clap`](https://github.com/clap-rs/clap) — CLI argument parsing
- [`anyhow`](https://github.com/dtolnay/anyhow) — error handling
- [`libc`](https://github.com/rust-lang/libc) — `ioctl` for `HIDIOCSFEATURE`, `poll`

## License

MIT
