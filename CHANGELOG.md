# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.15.0] - 2026-10-09

### Fixed

- Unplugging the dock made the daemon exit 1 — `waiting on the dock and the
  virtual battery: waiting on the devices: the wake descriptor is hung up, in
  error or not open`, and `systemctl status` showed `razerd.service` failed,
  though `BindsTo=` was stopping it anyway. The dock going is not a failure
  of the daemon: it now withdraws the battery, logs `dock unplugged —
  stopping` (info; the error the hidraw answered is a debug line) and exits
  0, exactly as on a signal — whether the loop met the gone dock in an
  exchange (`is_device_gone`: ENODEV, EIO on read) or in its wait. uhid-battery
  reports a hung-up wake descriptor as a plain error with no type to match
  on, so after a wait that fails with no battery to blame the daemon asks the
  dock's node itself (`HidrawDevice::is_hung_up`: one `poll()` with no
  timeout, `POLLHUP | POLLERR | POLLNVAL` — the flags that wait refuses) and
  marks the error `HungUp`, which `is_device_gone` recognises. Tested on the
  read end of a pipe whose writer is gone: the wait, and the whole loop,
  return cleanly.
- For `--watch` under `razerd-watch.service` the same exit 0 means the unit
  goes inactive, not failed, and `Restart=on-failure` leaves it there: when
  the dock is back, `systemctl --user start razerd-watch.service` (it used to
  come back by itself, by failing every 5 s while the dock was absent). The
  unit and the README document the `Restart=always` / `RestartSec=30`
  override for whoever prefers that.

### Changed

- `src/actions.rs` (1312 lines) is split, without a change of behavior:
  `src/commands.rs` (the one-shot `run_*`), `src/daemon/mod.rs` (the loop,
  `Bridge`, the wait, `Stop`) and `src/daemon/schedule.rs` (`Schedule`,
  `HoldSchedule`, `PollSchedule`, the cadences and their tests). The loop's
  turn is its own function (`turn`), and `Bridge::after_poll` moves the
  state in place.
- `src/protocol.rs`: the eight queries and writes spelled their four header
  bytes out in a `build_query` call each; they are a `Command` table now
  (transaction id, class, id, data size: one constant per command, the data
  size with the command it belongs to) and `Command::exchange`. Not a byte
  sent changes: a test pins every command's header, and the whole report,
  against the values they used to spell out.
- The release binary is a static build for `x86_64-unknown-linux-musl`
  (razerd links no C library: its system calls go through `rustix`), checked
  with `file`, and the release carries `SHA256SUMS` next to
  `razerd-x86_64-linux`; the README says how to verify and install it.
- `main`'s comment on the absent dock says what a dock that goes while the
  daemon runs does; the README's `--watch`, `--upower`, *Deploying*, *Logs*,
  *The alternative*, *Source layout* and *Dependencies* sections follow.

### Added

- Tests: the hung-up node is told from a quiet one and from pending input;
  `HungUp` anywhere in the chain is a gone device; the wait passes wake-ups
  and deadlines through on a live dock; every command's header bytes.

## [0.14.0] - 2026-10-09

The daemon logs; the commands print. `--upower` and `--watch` used to
`println!` every re-apply and every battery change — several hundred journal
lines a day, all at the journal's default priority, so nothing could be
filtered. They now report through `log`, with journald priorities under
systemd. The one-shot commands (`--check`, `--battery`, `--info`, `--color`,
`--sniff`, `--sensitivity`…) print their result on stdout exactly as before.

### Added

- `-v` / `--verbose`, cumulative: `-v` logs at debug (every battery poll with
  its reading or the firmware's reason for a miss, every color re-apply with
  its reason), `-vv` at trace (each sample of the dock's input stream), for
  this crate. `RUST_LOG` does the same (`RUST_LOG=debug`,
  `RUST_LOG=razerd=trace`), `-v` taking precedence. `--help` ends with a note
  on the two. No `--quiet`: at the default level nothing is logged per poll.
- Under systemd (`JOURNAL_STREAM` set) each line carries its journald
  priority (`<3>` error, `<4>` warn, `<6>` info, `<7>` debug and trace) and
  no timestamp of its own, so `journalctl -p warning -u razerd` filters.
  On a terminal the lines are timestamped and colored (`env_logger` with the
  `auto-color` and `humantime` features alone).
- `Environment=RUST_LOG=info` in `razerd.service` and `razerd-watch.service`,
  with a comment on `journalctl -p warning` and on `-v` through
  `/etc/razerd/razerd.conf` (`RAZERD_ARGS=--hold blue -v`); `contrib/razerd.conf`
  documents `-v`.
- README: a *Logs* section (the levels, `-v`, `RUST_LOG`, `journalctl -p`,
  a day at `-v`), `-v` in the options table.
- Tests: `-v` counts and is no action; which actions are daemons; the
  journald priority table.

### Changed

- The daemon's messages and their levels: start ("bridging the mouse battery
  from … to UPower, holding '…'", "watching … — holding '…'"), battery
  exposed, a charging flip ("battery: 100% (charging)"), battery withdrawn
  and the stop are `info`; each poll (answered or not), each re-apply and the
  initial color are `debug`; a re-apply the firmware refused is `warn`
  (it was `eprintln!`); a level change alone is no longer its own line — the
  poll's debug line has it.
- The daemon's fatal error (dock disconnected, no `/dev/uhid`, dock absent at
  start) is logged at `error` — `journalctl -p err` finds it — instead of the
  `Error: …` a `main() -> Result` printed. A command's fatal error is printed
  on stderr as before, same text, same exit status.
- `main` returns an `ExitCode` and holds the logger; the dispatch is unchanged.

## [0.13.1] - 2026-10-09

### Fixed

- `razerd.service` failed to start with `217/USER`: with `DynamicUser=yes` the
  dynamic user takes the unit's name, and `razerd` is already the system group
  of the udev rule. The unit now sets `User=razerd-daemon`.

## [0.13.0] - 2026-10-09

One daemon owns the dock. `razerd-battery.service` (system, `--upower`) and
`razerd-watch.service` (user, `--watch`) each opened the dock's hidraw, and the
firmware has one report buffer and no transaction ids: a color written during
a battery query left the query reading a foreign header (a poll counted
unanswered), and a query in flight had the color command refused (`EPIPE`) —
both triggered by the same mouse movement (color at T+0 and T+2 s, battery at
T+1, T+2 and T+8 s). `razerd.service` now runs `--upower --hold <color>`: one
process, one calendar, the two exchanges ordered rather than concurrent.

### Added

- `--hold <COLOR>`, an option of `--upower`: the daemon also holds the color,
  re-applying it on wake (with the 2 s follow-up) and on the 60 s safety
  cadence, exactly as `--watch` does, from the same loop as the battery polls.
  `--hold` without `--upower` is refused (clap's `requires` would have let
  `--watch red --hold blue` through, so the rule is checked after parsing).
- `contrib/razerd.service`, replacing `razerd-battery.service`:
  `ExecStart=razerd --upower $RAZERD_ARGS` with
  `EnvironmentFile=-/etc/razerd/razerd.conf`; `contrib/razerd.conf` is the
  example (`RAZERD_ARGS=--hold blue`). Same hardening as before.
- Activation by udev: `contrib/70-razerd.rules` tags the dock's control
  interface (USB interface 0) for systemd, names it `/dev/razer-dock` and has
  its device unit want `razerd.service`; the unit is `BindsTo=`/`After=` that
  device. The service starts when the dock is plugged in (or found at boot)
  and stops when it goes; nothing loops while the dock is absent, and
  `Restart=on-failure` (10 s) is left for transient failures. No `WantedBy=`.
- Clean stop: SIGTERM and SIGINT raise a flag (`signal-hook`, flag module
  only); the loop leaves its wait on `Wakeup::Interrupted`, destroys the
  virtual battery — a clean removal for UPower — and logs that it withdrew it.
- `make install-daemon` / `uninstall-daemon`: SELinux module, migration from
  `razerd-battery.service` (disabled, removed), `/etc/razerd/razerd.conf`
  installed if absent, unit, udev rule, trigger, and a start or restart when
  the dock is present. `make install-battery` is an alias; `uninstall-battery`
  removes the old unit alone.
- README: a *Deploying* section (the daemon, the configuration file, the
  udev activation, the migration from the two units, the user unit as the
  alternative), `--hold` in the options table, the tests of the merged
  calendar in *Development*.
- Tests of the merged calendar: a wake colors at T+0, polls at T+1 s, and at
  T+2 s takes the follow-up color and the at-rest poll in one turn (poll
  first); the safety tick and the refresh on their own clocks, and together
  when they coincide; each half alone; the absent-cadence retry.

### Changed

- The `--upower` and `--watch` loops are one, `run_daemon`: a `Schedule`
  holding a `HoldSchedule` (the color: wake, follow-up, safety tick) and a
  `PollSchedule` (the battery: refresh, motion polls, retries), each optional;
  the next wake-up is the earliest of the two, and a turn takes what both owe —
  the battery poll before the color, since the poll reads the firmware's reply
  back while the color is two writes nobody reads back. The virtual battery's
  states (absent, exposed, silent) are a `Bridge` enum. `--watch` is the same
  loop without a bridge. Constants and their cadences are unchanged.
- The input stream is sampled (one look, a drain, half a second muted) in
  `--watch` too, as `--upower` already did; wake detection is unchanged (a
  sleeping mouse's stream is watched continuously).
- A first battery reading is asked for at once at start; after a withdrawal
  the next poll comes on the absent cadence (10 s) or a second after the
  mouse moves, rather than immediately after the one that withdrew it.
- When a follow-up and a safety tick fall due together, one re-apply is sent
  and logged with both reasons, instead of two.
- `razerd-watch.service` stays as the standalone alternative; its unit and
  the README say not to run it alongside `razerd.service` with `--hold`.
- `uhid::open`'s error names `razerd.service`; the SELinux module's comment
  too.
- `HidrawDevice::wait_for_input` is gone: the daemon waits on the handle
  through `AsFd` in uhid-battery's `serve_all`, which a signal cuts short
  (hidraw's `wait_readable` takes the wait up again after one).

### Removed

- `contrib/razerd-battery.service` (renamed to `razerd.service`, see above)
  and its `WantedBy=multi-user.target`.

## [0.12.0] - 2026-10-08

The transport moved to the shared [hidraw](https://github.com/GregDuhamel/hidraw)
crate. Nothing changes for the dock: same node, same 91-byte feature-report
ioctls, same exchange (status codes, response correlation, leftover check),
same timeouts.

### Changed

- The dock's control interface is found and driven through `hidraw` v0.1.0:
  `discover` with a USB, vendor, product and interface filter (the nodes now
  come in numeric order, `hidraw10` after `hidraw2`, which with one dock
  changes nothing); `Device::set_feature` / `get_feature` for the 90-byte
  reports, behind the report-ID byte; `wait_readable` / `read` / `drain` for
  the input stream `--watch`, `--upower` and `--sniff` watch.
- `is_device_gone` is `hidraw::is_gone` applied to every `io::Error` in the
  chain: `ENODEV` from an ioctl as before, and the `EIO` that `read(2)`
  answers for an unplugged dock, which hidraw's `Device::read` now marks
  instead of razerd's own marker type.
- `RAZER_VENDOR_ID` is defined once (`hid.rs`), not in `uhid.rs` as well.

### Removed

- The `libc` dependency, and with it the crate's three `unsafe` blocks (the
  two report ioctls and `poll(2)`), the hand-rolled `_IOC` opcode encoding and
  its regression test, and the sysfs walk (`find_hidraw`, `hid_id`,
  `usb_interface_number`) with its fake-sysfs tests — all of it lives in
  hidraw now and is tested there. The crate builds with
  `unsafe_code = "deny"`; the one `unsafe` left, `uhid::open`, is explicitly
  allowed and documented.

### Added

- This changelog.
- Unit tests of the hid layer on a socket pair (wait, read, drain), and a
  device-gone test whose `EIO` comes out of hidraw's `Device::read` rather
  than a hand-built error.
- README: the architecture (the two shared crates, `hidraw` to talk to the
  dock and `uhid-battery` to publish the battery), the udev targets in the
  make block, and `contrib/70-razerd.rules` in the source layout.

## [0.11.2] - 2026-10-08

### Changed

- `uhid-battery` v0.4.0: `Reading` for create and update (`BatteryStatus`
  converts into it and keeps its CLI `Display`), a `Wakeup` match in the
  `--upower` loop, the identity built with the 0.4 builders (`u16` vendor and
  product), and an `InvalidIdentity` reported as a bug in razerd rather than
  something to retry.

## [0.11.1] - 2026-10-08

### Changed

- `uhid-battery` v0.3.0: `Handle::inherited` is `unsafe` there now (it unsets
  `LISTEN_*` from the environment); razerd is single-threaded and calls it
  from `main` before anything else, which the SAFETY comment records.
- `libc` 0.2.190 (dependabot).

## [0.11.0] - 2026-10-08

Phase 0: the review fixes before the transport moves out.

### Changed

- `--watch` only exits when the dock is gone: a transient `SET_REPORT` error
  (`EPIPE`, `ETIMEDOUT` while the firmware is busy) is logged and left to the
  next re-apply, as `--upower` already did.
- An `EIO` from `read(2)` is classified as the device being gone, as the
  kernel means it on that path; the same errno from an ioctl stays transient.
- `exchange_feature` refuses a reply byte-identical to what the report buffer
  held before the request unless a NEW/BUSY status was witnessed in between:
  a dropped request no longer passes for an answer.
- `find_hidraw` parses `HID_ID` strictly, field by field, keeps the USB bus
  only (the mouse over Bluetooth and the virtual battery bear the same ids),
  reads `bInterfaceNumber` from sysfs, and is testable on a fake tree.
- `HidrawDevice` has private fields, `AsFd` and `path()`.
- Lints: clippy `pedantic`, `unsafe_code = "warn"` until the move to rustix.
- The release workflow builds `--locked`; dependabot is configured.

### Added

- `contrib/70-razerd.rules`, installed by `sudo make install-udev` (which
  also creates the `razerd` system group); `sudo make uninstall-udev`.

## Older releases

Releases before 0.11.0 — 0.1.0 to 0.4.0 (2026-04-18), 0.5.0 to 0.8.0
(2026-06-08), 0.9.0 to 0.9.3 (2026-06-10 and 11), 0.10.0 to 0.10.9
(2026-09-19 to 21) — predate this file; their notes are on the
[GitHub releases page](https://github.com/GregDuhamel/razerd/releases).

[0.15.0]: https://github.com/GregDuhamel/razerd/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/GregDuhamel/razerd/compare/v0.13.1...v0.14.0
[0.13.1]: https://github.com/GregDuhamel/razerd/compare/v0.13.0...v0.13.1
[0.13.0]: https://github.com/GregDuhamel/razerd/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/GregDuhamel/razerd/compare/v0.11.2...v0.12.0
[0.11.2]: https://github.com/GregDuhamel/razerd/compare/v0.11.1...v0.11.2
[0.11.1]: https://github.com/GregDuhamel/razerd/compare/v0.11.0...v0.11.1
[0.11.0]: https://github.com/GregDuhamel/razerd/compare/v0.10.9...v0.11.0
