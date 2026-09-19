//! uhid transport: a virtual HID device whose only purpose is to make the
//! kernel register a `power_supply` battery for the wireless mouse. UPower
//! picks that up, and through it the KDE/GNOME power applets.
//!
//! The kernel side is `hid-input`'s battery support: a *Battery Strength*
//! usage in a report descriptor yields `/sys/class/power_supply/hid-*-battery`,
//! fed by the input reports we push. Nothing here emits buttons or motion —
//! the descriptor is a compile-time constant and the only values that ever
//! reach the kernel are a percentage and a charging bit.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

const UHID_PATH: &str = "/dev/uhid";

// `/dev/uhid` is the misc device (major 10) with minor 239 (`UHID_MINOR`).
const UHID_RDEV: libc::dev_t = libc::makedev(10, 239);

// systemd's fd-passing protocol (sd_listen_fds(3)): passed fds start at 3.
const SD_LISTEN_FDS_START: i32 = 3;

// `enum uhid_event_type` from <linux/uhid.h>.
const UHID_DESTROY: u32 = 1;
const UHID_START: u32 = 2;
const UHID_GET_REPORT: u32 = 9;
const UHID_GET_REPORT_REPLY: u32 = 10;
const UHID_CREATE2: u32 = 11;
const UHID_INPUT2: u32 = 12;
const UHID_SET_REPORT: u32 = 13;
const UHID_SET_REPORT_REPLY: u32 = 14;

// `enum uhid_report_type`.
const UHID_INPUT_REPORT: u8 = 2;

// Byte offsets inside the packed `struct uhid_event`: a u32 `type`, then the
// per-type payload. The kernel accepts short writes (it zero-fills the rest),
// so events are serialized by hand instead of mirroring the 4 KiB union.
const EV_PAYLOAD: usize = 4;
const CREATE2_NAME: std::ops::Range<usize> = 4..132;
const CREATE2_PHYS: std::ops::Range<usize> = 132..196;
const CREATE2_UNIQ: std::ops::Range<usize> = 196..260;
const CREATE2_RD_SIZE: usize = 260;
const CREATE2_BUS: usize = 262;
const CREATE2_VENDOR: usize = 264;
const CREATE2_PRODUCT: usize = 268;
const CREATE2_RD_DATA: usize = 280;
const HID_MAX_DESCRIPTOR_SIZE: usize = 4096;

// BUS_VIRTUAL rather than BUS_USB: vendor-specific kernel drivers (hid-razer)
// and userspace matchers (libratbag, hwdb) key on `usb:1532:*`, so only
// hid-generic ever binds to this device.
const BUS_VIRTUAL: u16 = 0x06;
const RAZER_VENDOR_ID: u32 = 0x1532;
// The Basilisk V3 Pro 35K's own (wired) product id.
const BASILISK_V3_PRO_35K_PRODUCT_ID: u32 = 0x00CC;

const DEVICE_NAME: &str = "Razer Basilisk V3 Pro 35K";
const DEVICE_PHYS: &str = "razerd";
// Names the power supply (`hid-razerd-battery*`). A constant rather than the
// mouse serial: no string that came from the dock is ever handed to the kernel.
const DEVICE_UNIQ: &str = "razerd";

const REPORT_ID_BATTERY: u8 = 0x02;

// uhid registers the device from a worker, and hid-core drops input reports
// until the driver has finished probing. UHID_START arrives mid-probe, so the
// first reading is pushed this long after it. Should that still be too early,
// nothing breaks: the kernel queries us (GET_REPORT) until a push lands, and
// the next periodic update is one.
const START_SETTLE_DELAY: Duration = Duration::from_millis(250);

// hid-input rate-limits the uevents it raises for battery reports: a level
// that did not change is only re-announced 30 s after the last announcement.
// Whether a charging flip at an unchanged level escapes that window depends on
// the kernel version (7.2 announces it at once; older ones update sysfs
// silently, so UPower never re-reads it). Pushing the same report again once
// the window has closed costs one redundant uevent at worst.
const CHARGE_FLIP_REPUSH_DELAY: Duration = Duration::from_secs(31);

// The errno carried by the GET/SET_REPORT replies we do not serve.
const EIO: u16 = libc::EIO as u16;

/// Report descriptor: one Mouse application collection holding
///
/// * report 1 — a minimal pointer (3 buttons, X/Y). Never sent. It exists
///   because `hid-input` drops a device with no populated input node (taking
///   the battery with it), and because UPower types a HID battery after its
///   sibling input device: `ID_INPUT_MOUSE` makes this one a "mouse".
/// * report 2 — `[battery strength 0..100, charging bit]`. Strength comes
///   first: when the kernel has to *query* the level (`GET_REPORT`) it reads the
///   byte right after the report id.
const BATTERY_MOUSE_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x02, // Usage (Mouse)
    0xA1, 0x01, // Collection (Application)
    0x85, 0x01, //   Report ID (1)
    0x09, 0x01, //   Usage (Pointer)
    0xA1, 0x00, //   Collection (Physical)
    0x05, 0x09, //     Usage Page (Button)
    0x19, 0x01, //     Usage Minimum (1)
    0x29, 0x03, //     Usage Maximum (3)
    0x15, 0x00, //     Logical Minimum (0)
    0x25, 0x01, //     Logical Maximum (1)
    0x75, 0x01, //     Report Size (1)
    0x95, 0x03, //     Report Count (3)
    0x81, 0x02, //     Input (Data,Var,Abs)
    0x75, 0x05, //     Report Size (5)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x03, //     Input (Const) — padding
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x30, //     Usage (X)
    0x09, 0x31, //     Usage (Y)
    0x15, 0x81, //     Logical Minimum (-127)
    0x25, 0x7F, //     Logical Maximum (127)
    0x75, 0x08, //     Report Size (8)
    0x95, 0x02, //     Report Count (2)
    0x81, 0x06, //     Input (Data,Var,Rel)
    0xC0, //         End Collection
    0x85, 0x02, //   Report ID (2)
    0x05, 0x06, //   Usage Page (Generic Device Controls)
    0x09, 0x20, //   Usage (Battery Strength)
    0x15, 0x00, //   Logical Minimum (0)
    0x26, 0x64, 0x00, // Logical Maximum (100)
    0x75, 0x08, //   Report Size (8)
    0x95, 0x01, //   Report Count (1)
    0x81, 0x02, //   Input (Data,Var,Abs)
    0x05, 0x85, //   Usage Page (Battery System)
    0x09, 0x44, //   Usage (Charging)
    0x25, 0x01, //   Logical Maximum (1)
    0x75, 0x01, //   Report Size (1)
    0x81, 0x02, //   Input (Data,Var,Abs)
    0x75, 0x07, //   Report Size (7)
    0x81, 0x03, //   Input (Const) — padding
    0xC0, //       End Collection
];

const _: () = assert!(
    BATTERY_MOUSE_DESCRIPTOR.len() <= HID_MAX_DESCRIPTOR_SIZE,
    "report descriptor exceeds the uhid limit"
);

/// Get a read-write handle on `/dev/uhid`.
///
/// Under systemd the node stays `root:root 0600` and the service manager hands
/// us the descriptor (`OpenFile=/dev/uhid` in the unit), so the process itself
/// needs no privilege. Without that — a root shell, for debugging — open the
/// node directly.
pub(crate) fn open() -> Result<File> {
    let file = match inherited_fd() {
        // SAFETY: systemd passed us this descriptor for our exclusive use
        // (LISTEN_PID matches); nothing else in the process owns fd 3.
        Some(fd) => unsafe { File::from_raw_fd(fd) },
        None => OpenOptions::new()
            .read(true)
            .write(true)
            .open(UHID_PATH)
            .with_context(|| {
                format!("cannot open {UHID_PATH} — run via razerd-battery.service (or as root)")
            })?,
    };

    // An inherited descriptor could be anything; refuse to write uhid events
    // into something that is not the uhid node.
    let meta = file.metadata().context("cannot stat the uhid handle")?;
    if !meta.file_type().is_char_device() || meta.rdev() != UHID_RDEV {
        bail!("inherited file descriptor is not {UHID_PATH}");
    }
    Ok(file)
}

/// The single descriptor systemd passed us, if any (`sd_listen_fds(3)`).
fn inherited_fd() -> Option<i32> {
    let pid: u32 = std::env::var("LISTEN_PID").ok()?.parse().ok()?;
    let count: u32 = std::env::var("LISTEN_FDS").ok()?.parse().ok()?;
    (pid == std::process::id() && count == 1).then_some(SD_LISTEN_FDS_START)
}

/// A live virtual HID device exposing the mouse battery. The kernel tears the
/// device (and its `power_supply`) down when the handle is closed, so dropping
/// this — or the process dying — is all the cleanup there is.
pub(crate) struct BatteryDevice {
    file: File,
    report: [u8; 3],
    // When to push `report` again: after the kernel device (re)started, or
    // after a charging flip the kernel may not have announced.
    repush_at: Option<Instant>,
}

impl BatteryDevice {
    /// Create the device with an initial reading.
    pub(crate) fn create(file: File, percent: u8, charging: bool) -> Result<Self> {
        let device = Self {
            file,
            report: battery_report(percent, charging),
            repush_at: None,
        };
        // Asynchronous: the reading is pushed once the kernel reports the
        // device started — see `START_SETTLE_DELAY`.
        device
            .write_event(&create2_event(
                DEVICE_NAME,
                DEVICE_PHYS,
                DEVICE_UNIQ,
                BATTERY_MOUSE_DESCRIPTOR,
            ))
            .context("UHID_CREATE2 failed")?;
        Ok(device)
    }

    /// Remove the device from the kernel and hand the uhid handle back, ready
    /// for a later `create`. (Simply dropping `self` closes the handle, which
    /// also removes the device — but the handle cannot be reopened.)
    pub(crate) fn destroy(self) -> Result<File> {
        self.write_event(&UHID_DESTROY.to_ne_bytes())
            .context("UHID_DESTROY failed")?;
        Ok(self.file)
    }

    /// Publish a new reading.
    pub(crate) fn update(&mut self, percent: u8, charging: bool) -> Result<()> {
        let report = battery_report(percent, charging);
        if report[2] != self.report[2] {
            self.schedule_repush(CHARGE_FLIP_REPUSH_DELAY);
        }
        self.report = report;
        self.push()
    }

    fn schedule_repush(&mut self, delay: Duration) {
        let at = Instant::now() + delay;
        self.repush_at = Some(self.repush_at.map_or(at, |pending| pending.min(at)));
    }

    fn push(&self) -> Result<()> {
        let event = input2_event(&self.report);
        self.write_event(&event).context("UHID_INPUT2 failed")
    }

    /// Answer the kernel's requests until `deadline`. A `GET_REPORT`/`SET_REPORT` blocks
    /// its caller in the kernel (up to 5 s) until we reply, so the handle must
    /// be serviced whenever we are otherwise idle.
    ///
    /// `wake_fd`, if given, is watched too: the call returns `true` as soon as
    /// it is readable (it is never read here), `false` at the deadline.
    pub(crate) fn serve_until(
        &mut self,
        deadline: Instant,
        wake_fd: Option<RawFd>,
    ) -> Result<bool> {
        let readable = |fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // poll() skips negative descriptors.
        let mut pfds = [
            readable(self.file.as_raw_fd()),
            readable(wake_fd.unwrap_or(-1)),
        ];
        loop {
            let now = Instant::now();
            if self.repush_at.is_some_and(|at| now >= at) {
                self.repush_at = None;
                self.push()?;
            }
            if now >= deadline {
                return Ok(false);
            }
            let wake = self.repush_at.map_or(deadline, |at| at.min(deadline));
            // Round up so we never spin on a sub-millisecond remainder.
            let timeout_ms = wake
                .saturating_duration_since(now)
                .as_millis()
                .saturating_add(1)
                .min(i32::MAX as u128);

            // SAFETY: two valid pollfds (ours owned, the caller's borrowed for
            // the call); the kernel only writes `revents`. A negative return
            // means error, errno set.
            let ret = unsafe { libc::poll(pfds.as_mut_ptr(), 2, timeout_ms as libc::c_int) };
            if ret < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == ErrorKind::Interrupted {
                    continue;
                }
                return Err(err).context("poll on uhid failed");
            }
            if pfds[0].revents != 0 {
                self.handle_event()?;
            }
            if pfds[1].revents != 0 {
                return Ok(true);
            }
        }
    }

    /// Read one kernel event and reply if it is a request.
    fn handle_event(&mut self) -> Result<()> {
        // Large enough for every header we look at; the kernel truncates the
        // event to the buffer and still consumes it whole.
        let mut ev = [0u8; 16];
        match (&self.file).read(&mut ev) {
            Ok(_) => {}
            // The handle may be non-blocking (systemd opens it that way).
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(e) => return Err(e).context("reading uhid event"),
        }

        let kind = u32::from_ne_bytes([ev[0], ev[1], ev[2], ev[3]]);
        let id = [ev[4], ev[5], ev[6], ev[7]];
        let (rnum, rtype) = (ev[8], ev[9]);
        match kind {
            UHID_START => {
                self.schedule_repush(START_SETTLE_DELAY);
                Ok(())
            }
            UHID_GET_REPORT => {
                let ours = rnum == REPORT_ID_BATTERY && rtype == UHID_INPUT_REPORT;
                let reply = get_report_reply(id, ours.then_some(&self.report[..]));
                self.write_event(&reply).context("GET_REPORT reply failed")
            }
            // Nothing on this device is writable.
            UHID_SET_REPORT => self
                .write_event(&set_report_reply(id, EIO))
                .context("SET_REPORT reply failed"),
            _ => Ok(()),
        }
    }

    fn write_event(&self, event: &[u8]) -> Result<()> {
        // One event per write(); uhid never accepts a partial one.
        let n = (&self.file).write(event)?;
        if n != event.len() {
            bail!("short write to uhid ({n} of {} bytes)", event.len());
        }
        Ok(())
    }
}

/// `[report id, strength, charging]`. The kernel ignores a strength of 0
/// ("no reading"), so an empty battery is reported as 1 %.
fn battery_report(percent: u8, charging: bool) -> [u8; 3] {
    [REPORT_ID_BATTERY, percent.clamp(1, 100), u8::from(charging)]
}

fn create2_event(name: &str, phys: &str, uniq: &str, descriptor: &[u8]) -> Vec<u8> {
    let mut ev = vec![0u8; CREATE2_RD_DATA + descriptor.len()];
    ev[..EV_PAYLOAD].copy_from_slice(&UHID_CREATE2.to_ne_bytes());
    copy_c_string(&mut ev[CREATE2_NAME], name);
    copy_c_string(&mut ev[CREATE2_PHYS], phys);
    copy_c_string(&mut ev[CREATE2_UNIQ], uniq);
    ev[CREATE2_RD_SIZE..CREATE2_RD_SIZE + 2]
        .copy_from_slice(&(descriptor.len() as u16).to_ne_bytes());
    ev[CREATE2_BUS..CREATE2_BUS + 2].copy_from_slice(&BUS_VIRTUAL.to_ne_bytes());
    ev[CREATE2_VENDOR..CREATE2_VENDOR + 4].copy_from_slice(&RAZER_VENDOR_ID.to_ne_bytes());
    ev[CREATE2_PRODUCT..CREATE2_PRODUCT + 4]
        .copy_from_slice(&BASILISK_V3_PRO_35K_PRODUCT_ID.to_ne_bytes());
    ev[CREATE2_RD_DATA..].copy_from_slice(descriptor);
    ev
}

/// Copy `s` into a fixed-size, NUL-terminated C string field, truncating on a
/// character boundary if needed. `field` must be zeroed.
fn copy_c_string(field: &mut [u8], s: &str) {
    let mut len = s.len().min(field.len() - 1);
    while !s.is_char_boundary(len) {
        len -= 1;
    }
    field[..len].copy_from_slice(&s.as_bytes()[..len]);
}

/// `struct uhid_input2_req`: `[u16 size, data...]`.
fn input2_event(report: &[u8]) -> Vec<u8> {
    let mut ev = Vec::with_capacity(EV_PAYLOAD + 2 + report.len());
    ev.extend_from_slice(&UHID_INPUT2.to_ne_bytes());
    ev.extend_from_slice(&(report.len() as u16).to_ne_bytes());
    ev.extend_from_slice(report);
    ev
}

/// `struct uhid_get_report_reply_req`: `[u32 id, u16 err, u16 size, data...]`.
/// `None` rejects the request with EIO.
fn get_report_reply(id: [u8; 4], report: Option<&[u8]>) -> Vec<u8> {
    let (err, data) = match report {
        Some(data) => (0, data),
        None => (EIO, &[][..]),
    };
    let mut ev = Vec::with_capacity(EV_PAYLOAD + 8 + data.len());
    ev.extend_from_slice(&UHID_GET_REPORT_REPLY.to_ne_bytes());
    ev.extend_from_slice(&id);
    ev.extend_from_slice(&err.to_ne_bytes());
    ev.extend_from_slice(&(data.len() as u16).to_ne_bytes());
    ev.extend_from_slice(data);
    ev
}

/// `struct uhid_set_report_reply_req`: `[u32 id, u16 err]`.
fn set_report_reply(id: [u8; 4], err: u16) -> Vec<u8> {
    let mut ev = Vec::with_capacity(EV_PAYLOAD + 6);
    ev.extend_from_slice(&UHID_SET_REPORT_REPLY.to_ne_bytes());
    ev.extend_from_slice(&id);
    ev.extend_from_slice(&err.to_ne_bytes());
    ev
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Offsets must match the packed `struct uhid_create2_req`: name[128],
    /// phys[64], uniq[64], u16 rd_size, u16 bus, u32 vendor/product/version/
    /// country, then rd_data — all behind the u32 event type.
    #[test]
    fn create2_event_matches_the_kernel_layout() {
        let ev = create2_event("name", "phys", "SERIAL", &[0xAA, 0xBB, 0xCC]);
        assert_eq!(ev.len(), 4 + 128 + 64 + 64 + 2 + 2 + 4 * 4 + 3);
        assert_eq!(ev[..4], UHID_CREATE2.to_ne_bytes());
        assert_eq!(&ev[4..9], b"name\0");
        assert_eq!(&ev[132..137], b"phys\0");
        assert_eq!(&ev[196..203], b"SERIAL\0");
        assert_eq!(ev[260..262], 3u16.to_ne_bytes());
        assert_eq!(ev[262..264], BUS_VIRTUAL.to_ne_bytes());
        assert_eq!(ev[264..268], 0x1532u32.to_ne_bytes());
        assert_eq!(ev[268..272], 0x00CCu32.to_ne_bytes());
        assert_eq!(ev[272..280], [0u8; 8]); // version, country
        assert_eq!(&ev[280..], &[0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn c_string_fields_stay_nul_terminated() {
        let mut field = [0u8; 8];
        copy_c_string(&mut field, "0123456789");
        assert_eq!(&field, b"0123456\0");

        // Never split a multi-byte character: 'é' would straddle the limit.
        let mut field = [0u8; 4];
        copy_c_string(&mut field, "abé");
        assert_eq!(&field, b"ab\0\0");
    }

    #[test]
    fn battery_report_clamps_to_what_the_kernel_accepts() {
        assert_eq!(battery_report(89, false), [REPORT_ID_BATTERY, 89, 0]);
        assert_eq!(battery_report(100, true), [REPORT_ID_BATTERY, 100, 1]);
        // hid-input drops a strength of 0 as "no reading".
        assert_eq!(battery_report(0, false), [REPORT_ID_BATTERY, 1, 0]);
        assert_eq!(battery_report(250, false), [REPORT_ID_BATTERY, 100, 0]);
    }

    #[test]
    fn input2_event_prefixes_the_report_with_its_size() {
        let ev = input2_event(&[0x02, 42, 1]);
        assert_eq!(ev[..4], UHID_INPUT2.to_ne_bytes());
        assert_eq!(ev[4..6], 3u16.to_ne_bytes());
        assert_eq!(&ev[6..], &[0x02, 42, 1]);
    }

    #[test]
    fn get_report_reply_serves_or_rejects() {
        let id = 7u32.to_ne_bytes();
        let ok = get_report_reply(id, Some(&[0x02, 42, 0]));
        assert_eq!(ok[..4], UHID_GET_REPORT_REPLY.to_ne_bytes());
        assert_eq!(ok[4..8], id);
        assert_eq!(ok[8..10], 0u16.to_ne_bytes()); // err
        assert_eq!(ok[10..12], 3u16.to_ne_bytes()); // size
        assert_eq!(&ok[12..], &[0x02, 42, 0]);

        let rejected = get_report_reply(id, None);
        assert_eq!(rejected[8..10], EIO.to_ne_bytes());
        assert_eq!(rejected[10..12], 0u16.to_ne_bytes());
        assert_eq!(rejected.len(), 12);
    }

    #[test]
    fn set_report_reply_layout() {
        let ev = set_report_reply(9u32.to_ne_bytes(), EIO);
        assert_eq!(ev[..4], UHID_SET_REPORT_REPLY.to_ne_bytes());
        assert_eq!(ev[4..8], 9u32.to_ne_bytes());
        assert_eq!(ev[8..10], EIO.to_ne_bytes());
        assert_eq!(ev.len(), 10);
    }

    /// Walk the descriptor's short items: collections must balance, and the
    /// battery report must be `[id, strength, charging]` with the strength
    /// directly after the id — the kernel's GET_REPORT path reads `buf[1]`.
    #[test]
    fn descriptor_is_well_formed_and_strength_comes_first() {
        let d = BATTERY_MOUSE_DESCRIPTOR;
        let (mut i, mut depth, mut page, mut report_id) = (0usize, 0i32, 0u32, 0u32);
        let mut battery_usages = Vec::new();
        while i < d.len() {
            let prefix = d[i];
            let size = [0, 1, 2, 4][(prefix & 0x03) as usize];
            let data = d[i + 1..i + 1 + size]
                .iter()
                .rev()
                .fold(0u32, |acc, b| (acc << 8) | *b as u32);
            match prefix & 0xFC {
                0xA0 => depth += 1,       // Collection
                0xC0 => depth -= 1,       // End Collection
                0x04 => page = data,      // Usage Page
                0x84 => report_id = data, // Report ID
                0x08 if report_id == REPORT_ID_BATTERY as u32 => {
                    battery_usages.push((page, data)); // Usage
                }
                _ => {}
            }
            assert!(depth >= 0, "unbalanced End Collection at byte {i}");
            i += 1 + size;
        }
        assert_eq!(i, d.len(), "truncated trailing item");
        assert_eq!(depth, 0, "unclosed collection");
        // Generic Device Controls / Battery Strength, then Battery System / Charging.
        assert_eq!(battery_usages, vec![(0x06, 0x20), (0x85, 0x44)]);
    }
}
