# razerd

Minimal RGB daemon for Razer peripherals on Linux.

Controls the LED color of the **Razer Mouse Dock Pro** and a wirelessly connected **Razer Basilisk V3 Pro 35K** simultaneously, without requiring OpenRazer or any Razer software. It also sets the mouse sensitivity and publishes the mouse battery to UPower, so it shows up in the desktop's power applet like any other wireless peripheral (tested with KDE Plasma). Deployed, it is one daemon (`razerd --upower --hold <color>`) that udev starts when the dock is plugged in — see [Deploying](#deploying).

## Supported devices

| Device | USB ID | Connection |
|---|---|---|
| Razer Mouse Dock Pro | `1532:00A4` | USB |
| Razer Basilisk V3 Pro 35K | via `1532:00A4` | Wireless through dock |

razerd talks exclusively to the dock: every command is sent to it, and the dock routes mouse commands over the RF link — no separate USB device needed. A mouse connected directly over USB cable (`1532:00CC`) or through the standalone dongle (`1532:00CD`) is **not** supported.

## Usage

```
razerd --color <COLOR>
razerd --upower [--hold <COLOR>] [-v|-vv]
razerd --watch <COLOR> [-v|-vv]
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
| `--upower` | The daemon: expose the mouse battery to UPower, and so to the desktop's power applet, through a virtual HID device (runs until stopped — meant for `razerd.service`) |
| `--hold red\|green\|blue\|white\|off` | With `--upower`: also hold a color, re-applying it whenever the mouse wakes — `--watch`, in the same process as the battery bridge |
| `--watch red\|green\|blue\|white\|off` | Hold a color, re-applying it whenever the mouse wakes, without the battery bridge (runs until stopped — the standalone alternative to `--upower --hold`) |
| `--sensitivity <value>` (alias `--dpi`) | Set the sensitivity to one fixed DPI value (100–35000), disabling the Cycle Up Sensitivity Stages button |
| `--sensitivity-stages on\|off` | `on`: install the 5-stage table (400/800/1600/3200/6400) and enable the Cycle Up Sensitivity Stages button; `off`: freeze the current DPI and disable it |
| `--check` | Verify the dock is detected and accessible, and that the mouse answers over RF |
| `--battery` | Report mouse battery percentage and charging status |
| `--info` | Full device report: serial, firmware, battery, DPI, stages lock state, onboard profile, and the battery exposed by `--upower` |
| `--sniff` | Diagnostic: dump timestamped HID input reports from the dock (Ctrl-C to stop) |
| `-v`, `--verbose` | Log more, for `--upower` and `--watch`: `-v` every battery poll and color re-apply (debug), `-vv` the trace level — see [Logs](#logs) |

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

The last block is the battery a running [`--upower`](#battery-in-the-desktops-power-applet---upower) currently exposes, read back from sysfs — exactly what UPower and the desktop's power applet see, so a mismatch with the mouse's own `Battery` line points at the bridge. It reads `Path: — (not exposed: bridge not running, or mouse silent)` when `razerd.service` is stopped or has withdrawn the battery.

The mouse stores 5 onboard profiles, cycled with the button on its underside; the indicator LED next to it shows the active slot's color (1 white, 2 red, 3 green, 4 blue, 5 cyan). `--info` reports the active slot.

### Holding a color: `--hold` and `--watch`

A wireless mouse forgets its color when it goes to sleep, and its firmware restores the onboard profile's lighting over it at other moments too. Holding a color means a long-running process that re-applies it **the moment the mouse wakes**, instead of re-firing on a fixed timer. The daemon does it with `--upower --hold <color>` (what `razerd.service` runs — see [Deploying](#deploying)); `--watch <color>` does the same thing alone, without the battery bridge, for a system without `/dev/uhid` or a user unit of your own.

```bash
razerd --watch blue      # Ctrl-C to stop
# [2026-10-09T10:00:00Z INFO  razerd::daemon] watching /dev/hidraw0 — holding 'blue', re-applying on wake
```

Like `--upower`, it reports through the [log](#logs) on stderr — nothing on stdout — and like it, it ends with exit status 0 on Ctrl-C and when the dock is unplugged (*dock unplugged — stopping*): the dock going is not a failure of the daemon.

How it works: the dock emits no dedicated wake event, but it resumes forwarding mouse-motion input reports the instant the mouse comes back. The daemon waits on that input stream and treats *input resuming after a quiet gap* (5 s or more) as a wake, re-applying the color at once. Each wake re-apply is followed by a second one 2 s later: lifting the mouse off the dock wakes it while it still shows its charging lighting, and the firmware reloads its onboard lighting when it switches to battery power — over the color just sent. While the mouse is in use it also re-applies on a slow safety cadence (every 60 s), the net for what wake detection cannot see — a pause shorter than that gap, a profile switch — and it stays completely idle while the mouse is asleep or absent, so there is no periodic wakeup cost.

**Why one process.** The dock firmware has a single report buffer and no transaction ids, so two processes talking to it corrupt each other: a color written during a battery query leaves the query reading a foreign header, and a query in flight has the color command refused (`EPIPE`). Both are triggered by the same mouse movement, so they collided often. `--hold` puts the color in the battery daemon's own loop, where the two are ordered rather than concurrent: when a poll and a re-apply fall due together (2 s after a wake: the follow-up color and the first at-rest battery poll), the poll runs first — it reads its reply back and must find it intact — and the color last, two writes nobody reads back. **Do not run `--watch` alongside `razerd.service --hold`.**

**If the color is lost every time you touch the mouse** (and comes back a couple of seconds later), check the onboard profile's lighting power-saving in Razer Synapse. With the option that dims the lighting when the mouse is idle enabled on a profile (dim to 25 % here), the firmware restores that profile's own lighting over razerd's color on every motion, even after a short pause. Disable it and save the profile. The setting is per profile — `razerd --info` shows the active one — and it is not visible through the commands razerd uses, so razerd cannot detect it.

### Battery in the desktop's power applet: `--upower`

Desktop power applets list the peripherals **UPower** knows about (tested with KDE Plasma's *Power and Battery*), and UPower only knows what the kernel registers under `/sys/class/power_supply`. The dock reports the mouse battery over Razer's vendor protocol, which the kernel does not speak — so the mouse is missing there.

`--upower` bridges the gap: it creates a virtual HID device named *Razer Basilisk V3 Pro 35K* through `/dev/uhid` whose report descriptor declares a standard *Battery Strength* field and a *Charging* bit, and mirrors the real readings into it. The kernel's generic HID battery support turns that into a `/sys/class/power_supply/hid-razerd-battery*` entry; UPower and the applet pick it up from there, with the desktop's own low-battery warning for peripherals on top (seen under KDE: "Mouse Battery Low").

```bash
upower -d | grep -A12 Basilisk    # once the service runs
```

Behavior:

- The **level** is refreshed once a minute. **Charging flips** are caught much faster, without polling fast all day: the charging state only changes when the mouse is put on or lifted off the dock, and either one moves it — so `--upower` watches the dock's input stream (sampling it twice a second, never following it report by report) and queries the battery 1 s after the mouse starts moving, then 2 s and 8 s after it comes to rest. Docking or lifting shows up within a few seconds.
- The battery only exists while the mouse answers. It appears with a **first real reading** — never a made-up level.
- A mouse that stops answering is asleep, switched off, out of range or unpaired — razerd cannot tell which: all it sees is a query left unanswered. After an unanswered poll `--upower` retries every 3 s, and once the silence has held for **10 seconds** the battery is **withdrawn** rather than left showing a stale level — the way a Bluetooth peripheral's battery vanishes on disconnect. One lost poll never makes it flicker. Switching the mouse off moves it, which triggers a poll: the entry is gone ~10 s later. A mouse that falls asleep is noticed by the next minute refresh. The battery comes back about a second after the mouse is moved again (or within ~10 s if it becomes reachable without moving); a silent mouse costs one short query every 10 s (the dock gives up in about 10 ms when the mouse is switched off).
- When the **dock** is unplugged the daemon withdraws the battery and exits — with status 0, logging *dock unplugged — stopping*: the dock going is not a failure of the daemon, and `systemctl status` shows the unit inactive, not failed. udev starts the service again the moment the dock returns (plus the first reading).
- On `systemctl stop` (SIGTERM) or Ctrl-C the daemon withdraws the battery itself before exiting, so UPower sees a clean removal rather than a device vanishing with its process.
- The virtual device carries an inert pointer collection (no event is ever emitted on it): the kernel drops HID devices without any input capability, and UPower labels a HID battery after its sibling input device — this is what makes it a "mouse". The side effect is a second, silent *Razer Basilisk V3 Pro 35K* input device on the system (it shows in `/proc/bus/input/devices`).

`--upower` needs a handle on `/dev/uhid`, which is root-only — and should stay so. Run it through the hardened system service rather than by hand: see [Deploying](#deploying).

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

Installs `razerd` to `/usr/local/bin`, root-owned. `make install` never invokes cargo, so nothing is compiled as root. Override the location with `PREFIX=...`; the systemd units follow it.

Remove with `sudo make uninstall`.

**Or the release binary.** Each [GitHub release](https://github.com/GregDuhamel/razerd/releases) carries `razerd-x86_64-linux`, a statically linked binary (musl target: no glibc version to match, it runs on any x86_64 Linux) and `SHA256SUMS`, the checksum of the asset. Verify the download before installing it:

```bash
sha256sum -c SHA256SUMS            # razerd-x86_64-linux: OK
install -Dm 0755 razerd-x86_64-linux target/release/razerd && sudo make install
```

(`make install` copies `target/release/razerd`; placing the binary there lets it do the install the usual way, root-owned under `/usr/local/bin`.)

Why system-wide: `razerd.service` is a system service holding a `/dev/uhid` descriptor. If it ran a binary from your home directory, any process of yours could replace that binary and inherit the descriptor.

**Upgrading from a `~/.local` install (≤ 0.9.3):** the user units used to point at `~/.local/bin`. After `sudo make install`, re-run `make install-watch` to refresh it, then drop the old copy: `rm ~/.local/bin/razerd`.

### 2. udev rules (grant non-root access to the dock)

```bash
sudo make install-udev
sudo usermod -aG razerd $USER
```

`make install-udev` installs [`contrib/70-razerd.rules`](contrib/70-razerd.rules) to `/etc/udev/rules.d/`, creates the `razerd` system group, and reloads udev. The rule gives that group read/write access to the dock (`1532:00a4`) and its hidraw nodes, and hands the dock's control interface to systemd as `/dev/razer-dock` so that [`razerd.service`](#deploying) starts with it. Remove with `sudo make uninstall-udev`. (`sudo make install-daemon` below installs the rule too.)

If you installed the rule by hand as `99-razerd.rules` from an older README, delete that copy: `sudo rm /etc/udev/rules.d/99-razerd.rules`.

Log out and back in, then verify:

```bash
razerd --check
```

`-r` makes `razerd` a **system group** (GID < 1000). `systemd-udevd` ≥ 258 warns at every boot that device-node ownership by a non-system group is deprecated and will stop working in a future release, so a plain user group is not an option any more.

**Already installed with a user group?** If `getent group razerd` shows a GID ≥ 1000, recreate it as a system group, then **reboot**:

```bash
sudo groupdel razerd && sudo groupadd -r razerd && sudo usermod -aG razerd $USER
```

The reboot is not optional: udevd resolves `GROUP="razerd"` to a numeric GID when it loads the rules, your session and a lingering user manager (`user@<uid>.service`) keep the old GID in their supplementary groups, and `udevadm trigger` alone just re-applies the cached GID. Only a reboot brings udevd, the device nodes and the services back in agreement.

Why a group rather than `TAG+="uaccess"`: the ACL granted by `uaccess` only exists while you have an active seat session, so a lingering `razerd-watch.service` started at boot would have no access to the dock until you log in.

On distributions whose initramfs is built with dracut in `hostonly` mode (Fedora), the rule file is copied into the initrd verbatim, where the `razerd` group does not exist. The initrd's udevd then logs two `Failed to resolve group 'razerd', ignoring` lines very early in the boot. They are harmless — the dock is not needed before the root filesystem is mounted.

## Deploying

One daemon owns the dock: `razerd.service`, a **system** unit running `razerd --upower $RAZERD_ARGS`, started by udev when the dock is plugged in. It bridges the battery to UPower and, with `--hold <color>` in its configuration file, holds the color.

```bash
sudo make install-daemon
```

What it does, in order: loads the SELinux module when SELinux is enabled, retires a ≤ 0.12 `razerd-battery.service` if one is installed, installs `/etc/razerd/razerd.conf` if there is none (the example holds blue), installs the unit, then installs the udev rule and triggers it — which starts the service if the dock is plugged in. Logs: `journalctl -u razerd.service` (see [Logs](#logs)). Remove with `sudo make uninstall-daemon`.

**The configuration file**, `/etc/razerd/razerd.conf` ([`contrib/razerd.conf`](contrib/razerd.conf)), is a systemd `EnvironmentFile=`: one line, `RAZERD_ARGS=--hold blue`. Change the color there, or drop the line for the battery bridge alone, then `sudo systemctl restart razerd.service`.

**Activation by udev, not by a target.** The udev rule tags the dock's control interface (USB interface 0, the one node razerd talks to) for systemd, names it `/dev/razer-dock`, and has its device unit pull `razerd.service` in; the unit is bound to that device (`BindsTo=` + `After=`). So:

- plug the dock in → the service starts; at boot it starts during device coldplug if the dock is there;
- unplug it → the daemon exits with status 0 (*dock unplugged — stopping*: the hidraw is gone, which is no failure of its own), `BindsTo=` stops the unit, and `systemctl status` shows it inactive rather than failed. Nothing loops while the dock is absent; `Restart=on-failure` (10 s) is only for a transient failure, and an exit 0 does not trigger it;
- there is no `WantedBy=` and nothing to `systemctl enable`: with the dock unplugged, `systemctl status razerd` shows it inactive, and a manual `systemctl start` waits for the device until systemd's device timeout, then fails. `razerd --check` by hand says why: *Razer Mouse Dock Pro not detected*.

**Migrating from ≤ 0.12** (two units, `razerd-battery.service` as system and `razerd-watch.service` as user, which stepped on each other's dock exchanges):

```bash
systemctl --user disable --now razerd-watch.service   # as yourself: the user unit goes
make uninstall-watch                                   # (same thing, plus the file)
sudo make install-daemon                               # retires razerd-battery.service, installs razerd.service
sudo systemctl status razerd.service                   # active, "bridging … holding 'blue'"
```

Change the held color in `/etc/razerd/razerd.conf` if it was not blue. `sudo make uninstall-battery` is kept for removing the old unit alone.

**Permissions — what the unit does and does not grant.** Whoever can open `/dev/uhid` can create arbitrary input devices: inject keystrokes, or feed crafted descriptors to the kernel's HID parsers. So razerd grants that to nobody:

- `/dev/uhid` stays `root:root 0600`. No udev rule, no group — your account and the `razerd` group gain nothing.
- The service manager opens the node itself and passes the descriptor to the service (`OpenFile=/dev/uhid:uhid`; razerd takes it by that name). Closing it — the process exiting for any reason — makes the kernel remove the virtual device. On SELinux systems (Fedora) the stock policy does not let systemd do that, so `make install-daemon` also loads a one-rule policy module, `contrib/razerd-uhid.cil`: `init_t` — PID 1's own domain, nothing else — may `open read write` `uhid_device_t` (what one `O_RDWR` open is checked against). It is not what keeps your account out of uhid (that is the node's `0600`, untouched), and a root process in `init_t` could already reach uhid by exec'ing into an unconfined domain — so the rule removes no effective barrier.
- The process runs as a throwaway unprivileged user (`DynamicUser=yes`) with no capabilities, no network, no sockets (not even D-Bus), a read-only filesystem, and a closed device allow-list: the dock's hidraw (through the `razerd` group of step 2), plus `/dev/uhid` itself — required for systemd to open it on the service's behalf, and useless to the process, which has neither the ownership nor a capability to get past `0600`. Check the result with `systemd-analyze security razerd.service`.
- System calls are an allow-list (`@default @basic-io @io-event @file-system @signal` + `ioctl`) rather than the usual broad `@system-service` — `@signal` covers the SIGTERM/SIGINT handlers — and the unit is capped at 4 tasks and 32 MB.
- In the code, the report descriptor and the device identity are compile-time constants, and the only values ever written to the virtual device are a percentage and a charging bit — no string from the dock, and no code path that emits a key, a button or motion. Its only inputs are fixed-size replies from the dock and fixed-size events from the kernel.

### Logs

The daemon — `--upower`, `--watch` — prints nothing on stdout. It reports through the log on stderr, which under systemd is the journal: `journalctl -u razerd.service`, `journalctl --user -u razerd-watch.service`. The one-shot commands (`--check`, `--battery`, `--info`, `--color`…) keep printing their result on stdout, unchanged, for the person or script that ran them.

| Level | What |
|---|---|
| `error` | The daemon's last word before it exits 1: the virtual battery refused by the kernel, no `/dev/uhid` handle, a wait that failed with the dock present — the line `systemctl status` shows |
| `warn` | A transient error, retried on the next turn: a color re-apply the firmware refused (`EPIPE` while it forwards something over RF) |
| `info` | The transitions: start; battery exposed to UPower (the first reading); docked or lifted (the charging flip); battery withdrawn (mouse silent for 10 s); stop — on a signal, or because the dock was unplugged (exit 0 either way) |
| `debug` | Every battery poll — the reading, or why it went unanswered: status `0x04` is the dock's "no RF reply" for a sleeping or absent mouse — and every color re-apply with its reason (wake, follow-up, safety refresh) |
| `trace` | Each sample of the dock's input stream (the mouse moving) |

The default is `info`: a handful of lines a day. `-v` (`--verbose`) adds `debug`, `-vv` adds `trace`; `RUST_LOG` does the same (`RUST_LOG=debug`, `RUST_LOG=razerd=trace`), `-v` taking precedence over it. For the service, put `-v` in `/etc/razerd/razerd.conf` (`RAZERD_ARGS=--hold blue -v`), or override the unit's `Environment=RUST_LOG=info`; for the user unit, `-v` on the `ExecStart=` line with `systemctl --user edit razerd-watch.service`. There is no `--quiet`: at `info` nothing is logged per poll.

Under systemd (`JOURNAL_STREAM` set) each line carries its journald priority in front — `<3>` error, `<4>` warn, `<6>` info, `<7>` debug and trace — and no timestamp of its own, so the journal files it at that priority: `journalctl -p warning -u razerd.service` shows the warnings and errors alone, `-p err` the errors. On a terminal the lines are timestamped and colored instead.

What a day looks like at `-v`, as `journalctl -o cat -u razerd.service` prints it:

```
bridging the mouse battery from /dev/hidraw0 to UPower, holding 'blue' — waiting for a first reading
applied 'blue' at start
battery poll unanswered: battery level query failed: device returned error status 0x04
battery poll: 89%
battery exposed to UPower: 89%
re-applied 'blue' (mouse woke after 312s idle)
battery poll: 89%
re-applied 'blue' (follow-up)
battery poll: 100% (charging)
battery: 100% (charging)
battery poll unanswered: battery level query failed: device returned error status 0x04
battery withdrawn — mouse silent for 10 s (asleep, off or out of range)
stopping on signal
battery withdrawn
```

Unplugging the dock ends the same way, with `dock unplugged — stopping` in place of `stopping on signal` (the error the hidraw answered is a `debug` line above it).

### The alternative: a user service for `--watch`

For a system without `/dev/uhid`, or if you want the color held by a unit of your own without the battery bridge:

```bash
make install-watch
```

Installs and enables `razerd-watch.service`, a user unit running `razerd --watch blue`. **Not alongside `razerd.service` with `--hold`**: one process must own the dock (see [why](#holding-a-color---hold-and---watch)). It is the alternative, not the recommended deployment.

The unit is sandboxed: no capabilities, no sockets, a read-only filesystem, a system-call allow-list (`systemd-analyze --user security razerd-watch.service`). A user unit gets that from unprivileged user namespaces; the `razerd` group survives them, so the dock stays reachable. If your system forbids user namespaces the start fails with `status=226/NAMESPACE` — drop the `Protect*`/`Private*` lines with `systemctl --user edit razerd-watch.service`.

Change the color with `systemctl --user edit razerd-watch.service`; remove with `make uninstall-watch`; to also run at **boot** before you log in, `sudo loginctl enable-linger $USER`.

**When the dock is unplugged** the daemon exits 0 (*dock unplugged — stopping*) and the unit goes inactive — not failed, so `Restart=on-failure` leaves it there. A user unit cannot be bound to the dock's device unit the way `razerd.service` is, so when the dock is back the color is not held until you start the unit again: `systemctl --user start razerd-watch.service`. To have it come back by itself, at the cost of a start attempt (and a *not detected* line in the journal) every 30 s while the dock is absent, override the unit:

```bash
systemctl --user edit razerd-watch.service
# [Service]
# Restart=always
# RestartSec=30
```

## How it works

razerd talks to the dock through the Linux `hidraw` interface — no kernel driver detachment, no libusb — and hands the mouse battery to the kernel through `uhid`. Both mechanisms live in crates shared with the other daemons of this account; razerd keeps only what is specific to Razer:

- [`hidraw`](https://github.com/GregDuhamel/hidraw) talks to the dock: `hidraw::discover` lists `/sys/class/hidraw` and keeps the node whose HID device is the dock's control interface — vendor `1532`, product `00A4`, on the **USB** bus (the mouse paired over Bluetooth and the virtual battery below bear the same ids) and USB interface 0 (`bInterfaceNumber` in sysfs: the dock is a composite device with one node per interface). `Device::set_feature` / `get_feature` are the `HIDIOCSFEATURE` / `HIDIOCGFEATURE` ioctls, with the report-ID byte (`0`, the dock declares none) in front of each 90-byte report; `wait_readable` / `read` / `drain` watch the dock's input stream for `--watch`, `--upower` and `--sniff`; `is_gone` tells an unplugged dock (`ENODEV` from an ioctl, `EIO` from `read`) from a mouse that merely did not answer. What stays in razerd (`src/hid.rs`) is the exchange: send the request, poll the report buffer every 2 ms for up to 100 ms until the firmware's status byte says the transaction is complete, and check that the reply carries the request's header and is not the previous transaction's reply read back.
- [`uhid-battery`](https://github.com/GregDuhamel/uhid-battery) publishes what was read: the virtual HID device behind [`--upower`](#battery-in-the-desktops-power-applet---upower). See below.

The Razer Mouse Dock Pro (`1532:00A4`) exposes three HID interfaces on USB. All LED commands go through **interface 0** (`/dev/hidraw0`). The dock firmware routes commands to the appropriate target based on the `data_size` field in the 90-byte Razer HID report:

| `data_size` | `byte[12]` | LEDs | Target |
|---|---|---|---|
| `0x1D` (29) | `0x07` | 8 | Dock LED ring |
| `0x2C` (44) | `0x0C` | 13 | Basilisk V3 Pro 35K via RF |

Battery queries use command class `0x07` (power): `cmd=0x80` for level, `cmd=0x84` for charging status. Onboard profile queries use class `0x05`: `cmd=0x80` for the slot count, `cmd=0x84` for the active slot. DPI uses class `0x04`: `cmd=0x85` reads and `cmd=0x05` writes X/Y as big-endian u16 pairs behind a storage-slot byte (`0x00` = live/RAM — what the sensor runs at and what the Cycle Up Sensitivity Stages button updates; `0x01` = persistent). The Cycle Up Sensitivity Stages button's stage table is `cmd=0x86`/`0x06`: active stage, stage count, then up to 5 × (index, X, Y, 2 reserved); `--sensitivity` writes it with a single stage. The dock forwards the request over RF and the mouse's reply is read back from the same report buffer (`HIDIOCGFEATURE`), once its status byte says the round-trip is complete (`0x02`; `0x00`/`0x01` still pending, `0x04` no RF reply, `0x03`/`0x05` rejected).

`--upower` is the one feature that does not talk to the dock alone: through the [`uhid-battery`](https://github.com/GregDuhamel/uhid-battery) crate it writes `uhid` events (`UHID_CREATE2`, then one `UHID_INPUT2` per reading: `[report id, strength 0–100, charging bit]`) to a `/dev/uhid` descriptor inherited from systemd, and answers the kernel's `UHID_GET_REPORT` when the level is read before a report landed. The device sits on `BUS_VIRTUAL`, so only `hid-generic` binds to it — never a Razer-specific driver or userspace matcher keyed on `usb:1532:*`.

The protocol was reverse-engineered from USB captures of Razer Synapse on Windows using Wireshark.

> **Note:** Do not send HID feature reports to interface 2 (`/dev/hidraw2`) — it causes the dock firmware to reboot.

## Development

### Source layout

| File | Role |
|---|---|
| `src/main.rs` | Module wiring, the flag → action dispatch, the logger (`init_logging`: `-v`, `RUST_LOG`, journald priorities under systemd) and the exit: the daemon logs its fatal error, a command prints it |
| `src/hid.rs` | The dock's side of the hidraw transport: which node is the dock (a `hidraw::Filter`), the report-ID byte in front of each report, the send/poll exchange with its status codes and response-correlation check, and which errors mean the dock is gone (hidraw's verdict on an exchange, and `is_hung_up`, one `poll()` on the node after the daemon's wait failed). The transport itself (sysfs discovery, ioctls, poll) is the [`hidraw`](https://github.com/GregDuhamel/hidraw) crate |
| `src/protocol.rs` | The Razer report layer: 90-byte format, the command table (`Command`: transaction id, class, id, data size — one constant per command), typed queries/writes (battery, serial, firmware, DPI, stages, profiles) |
| `src/uhid.rs` | The razerd side of the virtual HID battery behind `--upower`: device identity, where the `/dev/uhid` handle comes from, and reading the exposed battery back for `--info`. The uhid mechanism itself is the [`uhid-battery`](https://github.com/GregDuhamel/uhid-battery) crate |
| `src/cli.rs` | The clap surface: flags, parsers, flag-to-action mapping |
| `src/commands.rs` | The one-shot commands, one `run_*` function per flag (`--check`, `--color`, `--battery`, `--info`, `--sniff`, `--sensitivity`, `--sensitivity-stages`), printing on stdout |
| `src/daemon/mod.rs` | The daemon loop behind `--upower`/`--hold` and `--watch`: the `Bridge` state of the virtual battery, the wait on the dock and the battery, the signal flag, and why the loop ends (`Stop`: a signal or the dock unplugged — exit 0 either way) |
| `src/daemon/schedule.rs` | The daemon's calendar: one `Schedule` merging the color's re-applies (`HoldSchedule`) and the battery's polls (`PollSchedule`), the cadences, and their tests |
| `contrib/70-razerd.rules` | udev rule giving the `razerd` system group access to the dock, and handing its control interface to systemd as `/dev/razer-dock` to start `razerd.service` (installed by `sudo make install-udev` or `install-daemon`) |
| `contrib/razerd.service` | Hardened systemd **system** unit running `razerd --upower $RAZERD_ARGS`, bound to the dock's device unit (installed by `sudo make install-daemon`) |
| `contrib/razerd.conf` | Example `/etc/razerd/razerd.conf`: `RAZERD_ARGS=--hold blue` |
| `contrib/razerd-watch.service` | systemd user unit running `razerd --watch` — the standalone alternative (installed by `make install-watch`) |
| `contrib/razerd-uhid.cil` | One-rule SELinux module letting systemd open `/dev/uhid` for the daemon (loaded by `install-daemon` when SELinux is enabled) |

Unit tests live next to what they test (`mod tests` per module) and need no hardware: the hidraw layer is exercised on a socket pair, device-gone detection on errors shaped like the kernel's, the unplugged dock on the read end of a pipe whose writer is gone (`poll()` reports it hung up, as `hidraw_poll` does a vanished device: the daemon's wait and the whole loop return cleanly on it), every command's header against the bytes it always sent, and the daemon's calendar — pure bookkeeping over `Instant`s — is driven through a wake (color at T+0, battery at T+1 s, follow-up color and at-rest poll together at T+2 s, in that order), the two minute cadences, the retries and the withdrawal. One hardware-gated smoke test is excluded from CI — run it with the dock connected and the mouse awake: `cargo test -- --ignored`.

```bash
make build                # cargo build --release
make check                # what CI runs: fmt, check, clippy, test, doc
sudo make install         # copy to /usr/local/bin (never builds)
sudo make install-udev    # install contrib/70-razerd.rules and create the razerd group
sudo make install-daemon  # razerd.service (--upower, --hold from /etc/razerd/razerd.conf), udev-activated
make install-watch        # the alternative: a --watch user service, without the bridge
sudo make uninstall
sudo make uninstall-udev
sudo make uninstall-daemon
make uninstall-watch
sudo make uninstall-battery   # ≤ 0.12: remove the old razerd-battery.service alone
make clean                # cargo clean
```

Changes are listed in [CHANGELOG.md](CHANGELOG.md).

CI runs `cargo fmt --check`, `cargo check`, `cargo clippy -D warnings`, `cargo test`, `cargo doc -D warnings`, and a release build on every push to `main` and every PR targeting it.

Releases are made in two steps. The pull request bumps the version in `Cargo.toml` and `Cargo.lock` (`cargo update --workspace`), with its CHANGELOG entry. Once it is on `main`, the **Release** GitHub Action (`workflow_dispatch`) checks the two agree, refuses a version that is already tagged, builds for `x86_64-unknown-linux-musl` (`--locked`), checks with `file` that the binary is statically linked, tags `main`, and attaches `razerd-x86_64-linux` and its `SHA256SUMS` to the GitHub Release (see [installing the release binary](#1-build-and-install-the-binary)). It never commits: `main` only takes signed commits through pull requests.

## Dependencies

- [`clap`](https://github.com/clap-rs/clap) — CLI argument parsing
- [`anyhow`](https://github.com/dtolnay/anyhow) — error handling
- [`hidraw`](https://github.com/GregDuhamel/hidraw) — the hidraw transport: sysfs discovery, feature-report ioctls, poll with timeout, device-gone detection (git dependency, pinned to a release tag)
- [`uhid-battery`](https://github.com/GregDuhamel/uhid-battery) — the virtual HID battery behind `--upower` (git dependency, pinned to a release tag)
- [`signal-hook`](https://github.com/vorner/signal-hook) (the `flag` module alone, no default features) — SIGTERM/SIGINT raise a flag the daemon loop reads, so it withdraws the battery before exiting
- [`log`](https://github.com/rust-lang/log) and [`env_logger`](https://github.com/rust-cli/env_logger) (`auto-color` and `humantime` only, no regex) — the daemon's [log](#logs): `-v`, `RUST_LOG`, and journald priorities under systemd

razerd calls no `libc` itself: the system calls go through the crates above (`rustix` under the first two; the `libc` crate under `signal-hook` is bindings, not a C library), and it builds with `unsafe_code = "deny"`. No C library at all is what lets the release binary be a static musl build. The one `unsafe` block, explicitly allowed in `src/uhid.rs`, is the call that takes the `/dev/uhid` descriptor systemd passed and removes the `LISTEN_*` variables from the environment — sound only because razerd is single-threaded at that point.

## License

MIT
