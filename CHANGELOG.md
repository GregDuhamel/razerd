# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[0.12.0]: https://github.com/GregDuhamel/razerd/compare/v0.11.2...v0.12.0
[0.11.2]: https://github.com/GregDuhamel/razerd/compare/v0.11.1...v0.11.2
[0.11.1]: https://github.com/GregDuhamel/razerd/compare/v0.11.0...v0.11.1
[0.11.0]: https://github.com/GregDuhamel/razerd/compare/v0.10.9...v0.11.0
