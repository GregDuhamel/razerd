//! Hidraw transport: device discovery via sysfs, feature-report ioctls, and
//! the send/poll exchange the Razer firmware expects.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::protocol::REPORT_LEN;

const RAZER_VENDOR_ID: u16 = 0x1532;
const MOUSE_DOCK_PRO_PRODUCT_ID: u16 = 0x00A4;
const DOCK_INTERFACE: u8 = 0;
const HID_SYSFS_ROOT: &str = "/sys/class/hidraw";
const DEV_ROOT: &str = "/dev";

// HID bus type prefix used by the kernel in /sys/.../device/uevent's HID_ID field.
// "0003" = USB HID.
const HID_ID_USB_PREFIX: &str = "0003";

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
    pub(crate) file: File,
    pub(crate) path: PathBuf,
}

impl HidrawDevice {
    /// Open the Mouse Dock Pro's control interface.
    pub(crate) fn open_dock() -> Result<Self> {
        let path = find_hidraw(RAZER_VENDOR_ID, MOUSE_DOCK_PRO_PRODUCT_ID, DOCK_INTERFACE)
            .context("Razer Mouse Dock Pro not detected")?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("cannot open {} — check udev permissions", path.display()))?;
        Ok(Self { file, path })
    }

    /// Send a 90-byte HID feature report (`SET_REPORT` with type=feature, id=0).
    ///
    /// The hidraw ioctl buffer is `[report_id, ...90 bytes...]`; the kernel
    /// strips the report id and issues the USB control transfer.
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
        self.send_feature(request)?;

        let deadline = Instant::now() + RESPONSE_TIMEOUT;
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
                ResponseStatus::Ready => return Ok(response),
                ResponseStatus::Pending if Instant::now() < deadline => {} // keep polling
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

/// Did this exchange fail because the hidraw node itself is gone (dock
/// unplugged)? Long-running actions must exit on that — the handle will never
/// work again — whereas a timeout or a firmware error status only means the
/// mouse is asleep or out of range.
pub(crate) fn is_device_gone(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|io| io.raw_os_error() == Some(libc::ENODEV))
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
/// walking `/sys/class/hidraw`.
fn find_hidraw(vendor_id: u16, product_id: u16, interface: u8) -> Result<PathBuf> {
    let hid_id = format!("{HID_ID_USB_PREFIX}:{vendor_id:08X}:{product_id:08X}");

    let mut entries: Vec<_> = std::fs::read_dir(HID_SYSFS_ROOT)
        .with_context(|| format!("cannot read {HID_SYSFS_ROOT}"))?
        .collect::<std::io::Result<_>>()
        .with_context(|| format!("cannot enumerate {HID_SYSFS_ROOT}"))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let uevent_path = entry.path().join("device/uevent");
        let Ok(uevent) = std::fs::read_to_string(&uevent_path) else {
            continue;
        };
        if !uevent.contains(&hid_id) {
            continue;
        }

        if hidraw_interface_number(&entry.path()) == Some(interface) {
            return Ok(Path::new(DEV_ROOT).join(entry.file_name()));
        }
    }

    bail!(
        "no hidraw for {vendor_id:04x}:{product_id:04x} interface {interface} — device not connected?"
    )
}

/// Extract the USB interface number from the hidraw's sysfs symlink.
///
/// The `device` symlink resolves to the HID device node; its parent is the
/// USB interface directory named like `3-2:1.N` where `N` is the interface
/// number.
fn hidraw_interface_number(hidraw_sysfs_path: &Path) -> Option<u8> {
    let canonical = std::fs::canonicalize(hidraw_sysfs_path.join("device")).ok()?;
    let usb_iface = std::fs::canonicalize(canonical.join("..")).ok()?;
    usb_iface
        .file_name()?
        .to_str()?
        .rsplit_once('.')?
        .1
        .parse()
        .ok()
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
