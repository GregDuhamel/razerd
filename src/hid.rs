//! Hidraw transport: device discovery via sysfs, feature-report ioctls, and
//! the send/poll exchange the Razer firmware expects.

use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::protocol::REPORT_LEN;

const RAZER_VENDOR_ID: u16 = 0x1532;
const MOUSE_DOCK_PRO_PRODUCT_ID: u16 = 0x00A4;
const DOCK_INTERFACE: u8 = 0;
const HID_SYSFS_ROOT: &str = "/sys/class/hidraw";
const DEV_ROOT: &str = "/dev";

// Bus type in the `HID_ID=bus:vendor:product` field of a HID device's uevent
// (`BUS_USB` in linux/input.h): the dock must be the USB one, not a
// Bluetooth or uhid device bearing the same vendor/product ids.
const HID_BUS_USB: u16 = 0x0003;

// The firmware writes a status code into byte 0 of the response once it has
// processed a request (and, for mouse queries, completed the RF round-trip).
// We poll for it rather than sleeping a fixed worst-case interval: replies are
// usually ready within a few milliseconds, but a sleeping/absent mouse can take
// longer or never answer.
pub(crate) const RESPONSE_TIMEOUT: Duration = Duration::from_millis(100);
const RESPONSE_POLL_INTERVAL: Duration = Duration::from_millis(2);

// The hidraw driver queues at most this many input reports per open handle
// (HIDRAW_BUFFER_SIZE), dropping the oldest beyond that.
const HIDRAW_BUFFER_REPORTS: usize = 64;

// Response status byte (`response[0]`) values used by the Razer firmware.
const STATUS_NEW: u8 = 0x00; // not processed yet
const STATUS_BUSY: u8 = 0x01; // accepted, still working (RF round-trip pending)
const STATUS_OK: u8 = 0x02; // completed, payload valid

// Linux hidraw ioctl numbers: _IOC(_IOC_WRITE|_IOC_READ, 'H', {0x06,0x07}, len)
const fn hidioc_set_feature(len: usize) -> u64 {
    (3u64 << 30) | ((b'H' as u64) << 8) | 0x06 | ((len as u64) << 16)
}

const fn hidioc_get_feature(len: usize) -> u64 {
    (3u64 << 30) | ((b'H' as u64) << 8) | 0x07 | ((len as u64) << 16)
}

/// Owned handle over a `/dev/hidraw*` node, with typed feature-report I/O.
pub(crate) struct HidrawDevice {
    file: File,
    path: PathBuf,
}

/// The raw handle, for callers that multiplex the input stream with other
/// descriptors (`poll` alongside the uhid device) rather than read it here.
impl AsFd for HidrawDevice {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

impl HidrawDevice {
    /// Open the Mouse Dock Pro's control interface.
    pub(crate) fn open_dock() -> Result<Self> {
        let path = find_hidraw(
            Path::new(HID_SYSFS_ROOT),
            RAZER_VENDOR_ID,
            MOUSE_DOCK_PRO_PRODUCT_ID,
            DOCK_INTERFACE,
        )
        .context("Razer Mouse Dock Pro not detected")?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("cannot open {} — check udev permissions", path.display()))?;
        Ok(Self { file, path })
    }

    /// The `/dev/hidraw*` node this handle was opened on.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Send a 90-byte HID feature report (`SET_REPORT` with type=feature, id=0).
    ///
    /// The hidraw ioctl buffer is `[report_id, ...90 bytes...]`; the kernel
    /// strips the report id and issues the USB control transfer.
    #[expect(unsafe_code, reason = "libc::ioctl until the rustix migration")]
    pub(crate) fn send_feature(&self, report: &[u8; REPORT_LEN]) -> Result<()> {
        let mut buf = [0u8; REPORT_LEN + 1];
        buf[1..].copy_from_slice(report);

        // SAFETY: `self.file` is an owned, valid fd; `buf` is a unique mutable
        // array of exactly the byte-length we pass to the ioctl; the kernel
        // hidraw driver accepts the call and returns either ≥0 on success or
        // -1 on failure (with errno set).
        let ret = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                hidioc_set_feature(buf.len()),
                buf.as_mut_ptr(),
            )
        };
        if ret < 0 {
            // Keep the io::Error in the chain: callers tell a vanished dock
            // (ENODEV) from a mouse that merely does not answer.
            return Err(std::io::Error::last_os_error()).context("HIDIOCSFEATURE failed");
        }
        Ok(())
    }

    /// Read the current 90-byte feature report (`GET_REPORT` with type=feature).
    #[expect(unsafe_code, reason = "libc::ioctl until the rustix migration")]
    fn get_feature(&self) -> Result<[u8; REPORT_LEN]> {
        let mut buf = [0u8; REPORT_LEN + 1];

        // SAFETY: same invariants as `send_feature`; `HIDIOCGFEATURE` writes at
        // most `buf.len()` bytes into `buf`.
        let ret = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                hidioc_get_feature(buf.len()),
                buf.as_mut_ptr(),
            )
        };
        if ret < 0 {
            return Err(std::io::Error::last_os_error()).context("HIDIOCGFEATURE failed");
        }

        let mut response = [0u8; REPORT_LEN];
        response.copy_from_slice(&buf[1..]);
        Ok(response)
    }

    /// Block until the device emits an input report, returning its length.
    ///
    /// Unlike feature reports (which we pull on demand via ioctl), hidraw
    /// delivers the device's spontaneous input reports through `read()`. On
    /// this dock those are plain mouse-motion packets — there is no dedicated
    /// wake/sleep event — so `--watch` and `--sniff` use their mere presence
    /// (input flowing vs. silence) as the signal.
    pub(crate) fn read_input_report(&self, buf: &mut [u8]) -> Result<usize> {
        use std::io::Read;
        (&self.file)
            .read(buf)
            .map_err(|err| {
                // The kernel answers `read()` on a hidraw whose device was
                // unplugged with EIO (`hidraw_read`, `!list->hidraw->exist`)
                // rather than ENODEV: on this path EIO is the node's
                // obituary, not a transfer error, so mark it as such.
                if err.raw_os_error() == Some(libc::EIO) {
                    anyhow::Error::new(ReadOnGoneDevice(err))
                } else {
                    anyhow::Error::new(err)
                }
            })
            .context("reading hidraw input report")
    }

    /// Wait until an input report is queued or `deadline` passes; `true` means
    /// input is pending. Long-running actions use it as a "the mouse is being
    /// moved" signal without consuming the stream report by report.
    pub(crate) fn wait_for_input(&self, deadline: Instant) -> Result<bool> {
        loop {
            // Recomputed each turn: a signal cuts a poll short.
            let remaining = deadline.saturating_duration_since(Instant::now());
            if self.poll_input(remaining)? {
                return Ok(true);
            }
            if remaining.is_zero() {
                return Ok(false);
            }
        }
    }

    /// Discard every queued input report without blocking. The kernel keeps at
    /// most 64 per open handle, so this is bounded even while the mouse moves.
    pub(crate) fn drain_input_reports(&self) -> Result<()> {
        let mut buf = [0u8; 64];
        for _ in 0..HIDRAW_BUFFER_REPORTS {
            if !self.poll_input(Duration::ZERO)? {
                break;
            }
            self.read_input_report(&mut buf)?;
        }
        Ok(())
    }

    /// One `poll(POLLIN)` on the handle; `false` on timeout or a benign signal.
    #[expect(unsafe_code, reason = "libc::poll until the rustix migration")]
    fn poll_input(&self, timeout: Duration) -> Result<bool> {
        let mut pfd = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // Rounded up: a sub-millisecond remainder must sleep, not spin.
        let timeout_ms = libc::c_int::try_from(timeout.as_nanos().div_ceil(1_000_000))
            .unwrap_or(libc::c_int::MAX);
        // SAFETY: one valid pollfd over an owned fd; the kernel only writes
        // `revents`. A negative return means error, with errno set.
        let ret = unsafe { libc::poll(&raw mut pfd, 1, timeout_ms) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(err).with_context(|| format!("poll on {} failed", self.path.display()));
        }
        Ok(ret > 0)
    }

    /// Send a request and poll for the matching 90-byte response, returning it
    /// only once the firmware reports the transaction as completed.
    pub(crate) fn exchange_feature(&self, request: &[u8; REPORT_LEN]) -> Result<[u8; REPORT_LEN]> {
        // What the firmware's report buffer held before our request: the
        // previous transaction's reply. Our request is supposed to overwrite
        // it (status NEW), but should the firmware drop the request, a poll
        // would read this leftover back — and when the previous transaction
        // was the same query, it is indistinguishable from a fresh reply by
        // content alone (same header, and GET replies carry data where the
        // arguments were, so no argument echo can be required). Hence a
        // reply byte-identical to this snapshot only counts once a NEW/BUSY
        // status has been seen in between — proof the buffer was rewritten.
        let leftover = self.get_feature().context("reading the report buffer")?;
        self.send_feature(request)?;

        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        let mut saw_pending = false;
        loop {
            std::thread::sleep(RESPONSE_POLL_INTERVAL);
            let response = self.get_feature()?;

            // The firmware echoes data_size/class/cmd (bytes 5..8) in its
            // response. A mismatch means we read someone else's transaction —
            // e.g. a concurrent `razerd --watch` re-applying the color — so
            // treat it like a pending read and keep polling for our own.
            if response[5..8] != request[5..8] {
                if Instant::now() < deadline {
                    continue;
                }
                bail!("response belongs to another request (is another razerd instance running?)");
            }

            match classify_response_status(response[0]) {
                ResponseStatus::Ready if is_fresh_reply(&response, &leftover, saw_pending) => {
                    return Ok(response);
                }
                ResponseStatus::Ready if Instant::now() < deadline => {} // maybe stale: keep polling
                ResponseStatus::Ready => {
                    bail!("device never took the request (only the previous reply was read back)")
                }
                ResponseStatus::Pending if Instant::now() < deadline => saw_pending = true,
                ResponseStatus::Pending => {
                    bail!("device did not answer within {RESPONSE_TIMEOUT:?}")
                }
                ResponseStatus::Failed(status) => {
                    bail!("device returned error status 0x{status:02x}")
                }
            }
        }
    }
}

/// Is a completed `response` ours, given what the buffer held before the
/// request (`leftover`) and whether a NEW/BUSY status was observed since?
/// Any byte that differs from the leftover proves the buffer was rewritten;
/// an identical reply is only trusted after a witnessed transition.
fn is_fresh_reply(
    response: &[u8; REPORT_LEN],
    leftover: &[u8; REPORT_LEN],
    saw_pending: bool,
) -> bool {
    saw_pending || response != leftover
}

/// Marker for an EIO from `read()`: there, unlike on the ioctls, it means the
/// device behind the node is gone (see `HidrawDevice::read_input_report`).
#[derive(Debug)]
struct ReadOnGoneDevice(std::io::Error);

impl std::fmt::Display for ReadOnGoneDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "hidraw device gone: {}", self.0)
    }
}

impl std::error::Error for ReadOnGoneDevice {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// Did this exchange fail because the hidraw node itself is gone (dock
/// unplugged)? Long-running actions must exit on that — the handle will never
/// work again — whereas a timeout, a firmware error status or a transient
/// transfer error (EPIPE, ETIMEDOUT) only means the mouse is asleep, out of
/// range, or the firmware busy.
///
/// The ioctls report an unplugged device as ENODEV; `read()` reports it as
/// EIO, which `read_input_report` tags as [`ReadOnGoneDevice`].
pub(crate) fn is_device_gone(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause.downcast_ref::<ReadOnGoneDevice>().is_some()
            || cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.raw_os_error() == Some(libc::ENODEV))
    })
}

/// Outcome of inspecting a response's status byte (`response[0]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseStatus {
    /// Completed successfully; the payload is valid.
    Ready,
    /// Still being processed — keep polling until the deadline.
    Pending,
    /// Terminal failure (e.g. unsupported, no RF response); carries the raw code.
    Failed(u8),
}

const fn classify_response_status(status: u8) -> ResponseStatus {
    match status {
        STATUS_OK => ResponseStatus::Ready,
        STATUS_NEW | STATUS_BUSY => ResponseStatus::Pending,
        other => ResponseStatus::Failed(other),
    }
}

/// Resolve `/dev/hidrawN` for the given USB vendor/product/interface by
/// walking the hidraw class directory (`/sys/class/hidraw`; `sysfs_root` is a
/// parameter so a fake tree can stand in for it under test).
fn find_hidraw(
    sysfs_root: &Path,
    vendor_id: u16,
    product_id: u16,
    interface: u8,
) -> Result<PathBuf> {
    let mut entries: Vec<_> = std::fs::read_dir(sysfs_root)
        .with_context(|| format!("cannot read {}", sysfs_root.display()))?
        .collect::<std::io::Result<_>>()
        .with_context(|| format!("cannot enumerate {}", sysfs_root.display()))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let Ok(uevent) = std::fs::read_to_string(entry.path().join("device/uevent")) else {
            continue;
        };
        if hid_id(&uevent) != Some((HID_BUS_USB, u32::from(vendor_id), u32::from(product_id))) {
            continue;
        }
        if usb_interface_number(&entry.path()) == Some(interface) {
            return Ok(Path::new(DEV_ROOT).join(entry.file_name()));
        }
    }

    bail!(
        "no hidraw for {vendor_id:04x}:{product_id:04x} interface {interface} — device not connected?"
    )
}

/// The `HID_ID=bus:vendor:product` line of a HID device's uevent, as the
/// kernel writes it (`hid_uevent`: three hex fields, `%04X:%08X:%08X`). Parsed
/// field by field rather than matched as text, so a stray substring can never
/// pass for the dock.
fn hid_id(uevent: &str) -> Option<(u16, u32, u32)> {
    let value = uevent
        .lines()
        .find_map(|line| line.strip_prefix("HID_ID="))?;
    let mut fields = value.trim().split(':');
    let bus = u16::from_str_radix(fields.next()?, 16).ok()?;
    let vendor = u32::from_str_radix(fields.next()?, 16).ok()?;
    let product = u32::from_str_radix(fields.next()?, 16).ok()?;
    if fields.next().is_some() {
        return None;
    }
    Some((bus, vendor, product))
}

/// The USB interface number behind a hidraw, read from sysfs.
///
/// The hidraw's `device` symlink resolves to the HID device directory; the
/// USB interface directory is the ancestor that carries `bInterfaceNumber`
/// (`3-2:1.N` under USB, but the attribute is what defines it, not the name).
fn usb_interface_number(hidraw_sysfs_path: &Path) -> Option<u8> {
    let device = std::fs::canonicalize(hidraw_sysfs_path.join("device")).ok()?;
    let attribute = device
        .ancestors()
        .map(|dir| dir.join("bInterfaceNumber"))
        .find(|path| path.exists())?;
    let text = std::fs::read_to_string(attribute).ok()?;
    u8::from_str_radix(text.trim(), 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_response_status_maps_codes() {
        assert_eq!(classify_response_status(STATUS_OK), ResponseStatus::Ready);
        assert_eq!(
            classify_response_status(STATUS_NEW),
            ResponseStatus::Pending
        );
        assert_eq!(
            classify_response_status(STATUS_BUSY),
            ResponseStatus::Pending
        );
        // 0x03 failure, 0x04 no-response, 0x05 unsupported — all terminal.
        assert_eq!(classify_response_status(0x03), ResponseStatus::Failed(0x03));
        assert_eq!(classify_response_status(0x05), ResponseStatus::Failed(0x05));
    }

    #[test]
    fn device_gone_is_enodev_anywhere_in_the_chain() {
        let gone = anyhow::Error::from(std::io::Error::from_raw_os_error(libc::ENODEV))
            .context("HIDIOCSFEATURE failed")
            .context("battery level query failed");
        assert!(is_device_gone(&gone));

        // A stalled control transfer is transient, not a missing device.
        let stalled = anyhow::Error::from(std::io::Error::from_raw_os_error(libc::EPIPE))
            .context("HIDIOCSFEATURE failed");
        assert!(!is_device_gone(&stalled));

        // Timeouts and firmware error statuses carry no io::Error at all.
        assert!(!is_device_gone(&anyhow::anyhow!("device did not answer")));
    }

    /// `read()` on an unplugged hidraw fails with EIO, not ENODEV; the same
    /// errno from an ioctl is a transfer error and must stay transient.
    #[test]
    fn device_gone_is_eio_on_read_but_not_on_ioctl() {
        let eio = || std::io::Error::from_raw_os_error(libc::EIO);
        let gone_on_read = anyhow::Error::new(ReadOnGoneDevice(eio()))
            .context("reading hidraw input report")
            .context("watching for wake");
        assert!(is_device_gone(&gone_on_read));
        // The marker keeps the errno reachable for whoever prints the chain.
        assert!(
            gone_on_read
                .chain()
                .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
                .any(|io| io.raw_os_error() == Some(libc::EIO))
        );

        let eio_on_ioctl = anyhow::Error::from(eio()).context("HIDIOCGFEATURE failed");
        assert!(!is_device_gone(&eio_on_ioctl));
    }

    /// A completed reply identical to what the buffer held before the request
    /// is only trusted once a NEW/BUSY status proved the buffer was rewritten.
    #[test]
    fn identical_reply_needs_a_witnessed_transition() {
        let mut leftover = [0u8; REPORT_LEN];
        leftover[0] = STATUS_OK;
        leftover[5..8].copy_from_slice(&[0x02, 0x07, 0x80]);
        leftover[9] = 0xC0;

        // Same bytes as before the request: ambiguous until a transition.
        assert!(!is_fresh_reply(&leftover, &leftover, false));
        assert!(is_fresh_reply(&leftover, &leftover, true));

        // Any differing byte — here the battery level — proves a rewrite.
        let mut changed = leftover;
        changed[9] = 0xB0;
        assert!(is_fresh_reply(&changed, &leftover, false));
    }

    #[test]
    fn hid_id_is_parsed_field_by_field() {
        let uevent = "DRIVER=hid-generic\nHID_ID=0003:00001532:000000A4\nHID_NAME=Razer\n";
        assert_eq!(hid_id(uevent), Some((HID_BUS_USB, 0x1532, 0x00A4)));

        // Bluetooth (0005) and uhid (0006) devices carry the same ids.
        assert_eq!(
            hid_id("HID_ID=0005:00001532:000000A4\n"),
            Some((0x0005, 0x1532, 0x00A4))
        );
        // Not a HID_ID line, malformed, or too many fields.
        assert_eq!(hid_id("MODALIAS=hid:b0003g0001v00001532p000000A4\n"), None);
        assert_eq!(hid_id("HID_ID=0003:00001532\n"), None);
        assert_eq!(hid_id("HID_ID=0003:0000zz32:000000A4\n"), None);
        assert_eq!(hid_id("HID_ID=0003:00001532:000000A4:0000\n"), None);
    }

    /// A fake `/sys/class/hidraw` in a temporary directory, laid out like
    /// the kernel's: `hidrawN/device` is a symlink into the device tree,
    /// where the USB interface directory carries `bInterfaceNumber` and the
    /// HID device directory its `uevent`.
    struct FakeSysfs {
        root: PathBuf,
    }

    impl FakeSysfs {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("razerd-test-sysfs-{name}-{}", std::process::id()));
            std::fs::create_dir_all(root.join("class/hidraw")).unwrap();
            Self { root }
        }

        fn class_dir(&self) -> PathBuf {
            self.root.join("class/hidraw")
        }

        /// Add `hidrawN` for a HID device on `bus` with the given ids, on USB
        /// interface `interface` of device `port`.
        fn add(&self, n: u32, port: &str, interface: u8, bus: u16, vendor: u32, product: u32) {
            let iface_dir = self
                .root
                .join(format!("devices/usb1/{port}/{port}:1.{interface}"));
            let hid_dir = iface_dir.join(format!("{bus:04X}:{vendor:04X}:{product:04X}.{n:04}"));
            std::fs::create_dir_all(&hid_dir).unwrap();
            std::fs::write(
                iface_dir.join("bInterfaceNumber"),
                format!("{interface:02x}\n"),
            )
            .unwrap();
            std::fs::write(
                hid_dir.join("uevent"),
                format!("DRIVER=hid-generic\nHID_ID={bus:04X}:{vendor:08X}:{product:08X}\n"),
            )
            .unwrap();
            let hidraw_dir = self.class_dir().join(format!("hidraw{n}"));
            std::fs::create_dir_all(&hidraw_dir).unwrap();
            std::os::unix::fs::symlink(&hid_dir, hidraw_dir.join("device")).unwrap();
        }
    }

    impl Drop for FakeSysfs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn find_hidraw_picks_the_usb_interface_by_sysfs_attributes() {
        let sysfs = FakeSysfs::new("dock");
        // The dock's three interfaces, interleaved with lookalikes: the same
        // ids over Bluetooth, another Razer product, a uhid device.
        sysfs.add(0, "1-3", 1, HID_BUS_USB, 0x1532, 0x00A4);
        sysfs.add(1, "1-4", 0, 0x0005, 0x1532, 0x00A4);
        sysfs.add(2, "1-5", 0, HID_BUS_USB, 0x1532, 0x02B3);
        sysfs.add(3, "1-3", 0, HID_BUS_USB, 0x1532, 0x00A4);
        sysfs.add(4, "1-3", 2, HID_BUS_USB, 0x1532, 0x00A4);
        sysfs.add(5, "1-6", 0, 0x0006, 0x1532, 0x00A4);

        let found = find_hidraw(&sysfs.class_dir(), 0x1532, 0x00A4, 0).unwrap();
        assert_eq!(found, Path::new("/dev/hidraw3"));
        let found = find_hidraw(&sysfs.class_dir(), 0x1532, 0x00A4, 2).unwrap();
        assert_eq!(found, Path::new("/dev/hidraw4"));

        // Right ids, but no such interface.
        let err = find_hidraw(&sysfs.class_dir(), 0x1532, 0x00A4, 3).unwrap_err();
        assert!(err.to_string().contains("interface 3"), "{err}");
    }

    #[test]
    fn find_hidraw_skips_entries_without_a_readable_uevent() {
        let sysfs = FakeSysfs::new("partial");
        // A node whose device vanished between the listing and the read.
        std::fs::create_dir_all(sysfs.class_dir().join("hidraw0")).unwrap();
        sysfs.add(1, "1-3", 0, HID_BUS_USB, 0x1532, 0x00A4);

        let found = find_hidraw(&sysfs.class_dir(), 0x1532, 0x00A4, 0).unwrap();
        assert_eq!(found, Path::new("/dev/hidraw1"));
    }

    #[test]
    fn find_hidraw_reports_an_unreadable_class_directory() {
        let err = find_hidraw(Path::new("/nonexistent/hidraw"), 0x1532, 0x00A4, 0).unwrap_err();
        assert!(err.to_string().contains("cannot read"), "{err}");
    }

    /// Regression test: HIDIOCSFEATURE for a 91-byte buffer must match what
    /// the Linux kernel expects (`_IOC(_IOC_WRITE|_IOC_READ, 'H', 0x06, 91)`).
    #[test]
    fn hidioc_codes_match_kernel_encoding() {
        assert_eq!(hidioc_set_feature(91), 0xC05B_4806);
        assert_eq!(hidioc_get_feature(91), 0xC05B_4807);
    }

    /// Hardware smoke test — needs the dock connected and the mouse awake.
    /// Run explicitly with `cargo test -- --ignored`.
    ///
    /// Sends a brightness GET for led 0x05, which does not exist on this
    /// mouse: the firmware must answer status 0x03 *with the request header
    /// echoed*. If error responses did not echo the header, the correlation
    /// check in `exchange_feature` would misread the error as someone else's
    /// transaction and convert it into a slow, misleading timeout — this test
    /// pins the fast path.
    #[test]
    #[ignore = "requires the dock and an awake mouse"]
    fn hardware_error_status_is_fast_and_correctly_attributed() {
        use crate::protocol::{CLASS_EXTENDED_MATRIX, TX_ID_MOUSE, VARSTORE, build_query};

        let dock = HidrawDevice::open_dock().expect("dock not connected");
        // 0x84 = extended-matrix brightness GET.
        let req = build_query(
            TX_ID_MOUSE,
            CLASS_EXTENDED_MATRIX,
            0x84,
            0x03,
            &[VARSTORE, 0x05],
        );

        let start = Instant::now();
        let err = dock
            .exchange_feature(&req)
            .expect_err("led 0x05 must be rejected by the firmware");
        let elapsed = start.elapsed();

        assert!(
            elapsed < RESPONSE_TIMEOUT,
            "error took a full timeout ({elapsed:?}) — error responses may not echo the header"
        );
        assert!(
            err.to_string().contains("0x03"),
            "expected firmware error status 0x03, got: {err}"
        );
    }
}
