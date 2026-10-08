//! The dock's hidraw interface: finding and opening it, and the feature-report
//! exchange the Razer firmware expects.
//!
//! The transport is the [`hidraw`] crate: it lists the nodes through sysfs,
//! issues the feature-report ioctls, waits for input with a timeout, and
//! tells an unplugged device from a transfer that merely failed. What stays
//! here is the Razer side of it: which node is the dock's control interface,
//! the report-ID byte in front of every 90-byte report, and the send/poll
//! exchange with its status codes and response-correlation check.

use std::os::fd::{AsFd, BorrowedFd};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use hidraw::{Bus, Device, Filter, Node};

use crate::protocol::REPORT_LEN;

pub(crate) const RAZER_VENDOR_ID: u16 = 0x1532;
const MOUSE_DOCK_PRO_PRODUCT_ID: u16 = 0x00A4;
const DOCK_INTERFACE: u8 = 0;

// The dock's control interface among the nodes sysfs lists: the USB one —
// not the mouse paired over Bluetooth nor the virtual battery `--upower`
// publishes through uhid, which bear the same vendor/product ids — and the
// first of its three interfaces.
const DOCK_FILTER: Filter = Filter::new()
    .bus(Bus::Usb)
    .vendor(RAZER_VENDOR_ID)
    .product(MOUSE_DOCK_PRO_PRODUCT_ID)
    .interface(DOCK_INTERFACE);

// The firmware writes a status code into byte 0 of the response once it has
// processed a request (and, for mouse queries, completed the RF round-trip).
// We poll for it rather than sleeping a fixed worst-case interval: replies are
// usually ready within a few milliseconds, but a sleeping/absent mouse can take
// longer or never answer.
pub(crate) const RESPONSE_TIMEOUT: Duration = Duration::from_millis(100);
const RESPONSE_POLL_INTERVAL: Duration = Duration::from_millis(2);

// Response status byte (`response[0]`) values used by the Razer firmware.
const STATUS_NEW: u8 = 0x00; // not processed yet
const STATUS_BUSY: u8 = 0x01; // accepted, still working (RF round-trip pending)
const STATUS_OK: u8 = 0x02; // completed, payload valid

/// The open control interface of the dock, with typed feature-report I/O.
///
/// A thin wrapper over [`hidraw::Device`]: it adds the report-ID byte the
/// kernel wants in front of a Razer report, keeps the `anyhow` contexts the
/// actions print, and carries the exchange protocol.
pub(crate) struct HidrawDevice {
    device: Device,
}

/// The raw handle, for callers that multiplex the input stream with other
/// descriptors (`poll` alongside the uhid device) rather than read it here.
impl AsFd for HidrawDevice {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.device.as_fd()
    }
}

impl HidrawDevice {
    /// Open the Mouse Dock Pro's control interface.
    pub(crate) fn open_dock() -> Result<Self> {
        let node = find_dock().context("Razer Mouse Dock Pro not detected")?;
        let device = Device::open(&node.path).with_context(|| {
            format!(
                "cannot open {} — check udev permissions",
                node.path.display()
            )
        })?;
        Ok(Self { device })
    }

    /// The `/dev/hidraw*` node this handle was opened on.
    pub(crate) fn path(&self) -> &Path {
        self.device.path()
    }

    /// Send a 90-byte HID feature report (`SET_REPORT` with type=feature, id=0).
    ///
    /// The hidraw ioctl buffer is `[report_id, ...90 bytes...]`; the kernel
    /// strips the report id and issues the USB control transfer.
    pub(crate) fn send_feature(&self, report: &[u8; REPORT_LEN]) -> Result<()> {
        let mut buf = [0u8; REPORT_LEN + 1];
        buf[1..].copy_from_slice(report);
        // Keep the io::Error in the chain: callers tell a vanished dock
        // (ENODEV) from a mouse that merely does not answer.
        self.device
            .set_feature(&buf)
            .context("HIDIOCSFEATURE failed")
    }

    /// Read the current 90-byte feature report (`GET_REPORT` with type=feature).
    fn get_feature(&self) -> Result<[u8; REPORT_LEN]> {
        let mut buf = [0u8; REPORT_LEN + 1];
        self.device
            .get_feature(&mut buf)
            .context("HIDIOCGFEATURE failed")?;

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
    ///
    /// An unplugged dock is EIO on this path, which hidraw marks as the
    /// device being gone (see [`is_device_gone`]).
    pub(crate) fn read_input_report(&self, buf: &mut [u8]) -> Result<usize> {
        self.device.read(buf).context("reading hidraw input report")
    }

    /// Wait until an input report is queued or `deadline` passes; `true` means
    /// input is pending. Long-running actions use it as a "the mouse is being
    /// moved" signal without consuming the stream report by report.
    pub(crate) fn wait_for_input(&self, deadline: Instant) -> Result<bool> {
        // One `poll(POLLIN)`, taken up again with the time left when a signal
        // cuts it short — hidraw's business.
        let remaining = deadline.saturating_duration_since(Instant::now());
        self.device
            .wait_readable(remaining)
            .with_context(|| format!("poll on {} failed", self.path().display()))
    }

    /// Discard every queued input report without blocking. The kernel keeps at
    /// most [`hidraw::INPUT_QUEUE_LEN`] (64) per open handle, and one pass is
    /// bounded by that, so this is bounded even while the mouse moves.
    pub(crate) fn drain_input_reports(&self) -> Result<()> {
        self.device
            .drain()
            .context("draining hidraw input reports")?;
        Ok(())
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

/// The dock's control-interface node, as sysfs lists it — the first one, as
/// before, should two docks ever be plugged in.
fn find_dock() -> Result<Node> {
    hidraw::discover(&DOCK_FILTER)
        .with_context(|| format!("cannot read {}", hidraw::SYSFS_ROOT))?
        .into_iter()
        .next()
        .ok_or_else(|| {
            anyhow!(
                "no hidraw for {RAZER_VENDOR_ID:04x}:{MOUSE_DOCK_PRO_PRODUCT_ID:04x} \
                 interface {DOCK_INTERFACE} — device not connected?"
            )
        })
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

/// Did this exchange fail because the hidraw node itself is gone (dock
/// unplugged)? Long-running actions must exit on that — the handle will never
/// work again — whereas a timeout, a firmware error status or a transient
/// transfer error (EPIPE, ETIMEDOUT) only means the mouse is asleep, out of
/// range, or the firmware busy.
///
/// The verdict is [`hidraw::is_gone`]'s — ENODEV from an ioctl, or the EIO
/// that `read()` answers for an unplugged device and hidraw marks as such —
/// applied to every `io::Error` in the chain.
pub(crate) fn is_device_gone(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(hidraw::is_gone)
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

#[cfg(test)]
mod tests {
    use std::io;
    use std::os::unix::net::UnixDatagram;

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
        let enodev = rustix::io::Errno::NODEV.raw_os_error();
        let gone = anyhow::Error::from(io::Error::from_raw_os_error(enodev))
            .context("HIDIOCSFEATURE failed")
            .context("battery level query failed");
        assert!(is_device_gone(&gone));

        // A stalled control transfer is transient, not a missing device.
        let epipe = rustix::io::Errno::PIPE.raw_os_error();
        let stalled = anyhow::Error::from(io::Error::from_raw_os_error(epipe))
            .context("HIDIOCSFEATURE failed");
        assert!(!is_device_gone(&stalled));

        // Timeouts and firmware error statuses carry no io::Error at all.
        assert!(!is_device_gone(&anyhow::anyhow!("device did not answer")));
    }

    /// `read()` on an unplugged hidraw fails with EIO, not ENODEV; the same
    /// errno from an ioctl is a transfer error and must stay transient. The
    /// marking is hidraw's; what is pinned here is that it survives razerd's
    /// error chain, and that a bare EIO does not pass for it.
    #[test]
    fn device_gone_is_eio_on_read_but_not_on_ioctl() {
        let eio = rustix::io::Errno::IO.raw_os_error();

        // No dock to unplug under test, and a socket cannot produce EIO; but
        // `/proc/self/mem` at address 0, which is never mapped, answers
        // `read(2)` with that very errno. What matters is that it comes out
        // of `Device::read`, the path hidraw marks.
        let mem = std::fs::File::open("/proc/self/mem").unwrap();
        let dock = HidrawDevice {
            device: Device::from_fd(mem, "/dev/hidraw-fake"),
        };
        let gone_on_read = dock
            .read_input_report(&mut [0u8; 8])
            .expect_err("read(2) at an unmapped address must fail")
            .context("watching for wake");
        assert!(is_device_gone(&gone_on_read), "{gone_on_read:#}");
        // The errno stays reachable for whoever prints the chain.
        assert!(
            gone_on_read
                .chain()
                .filter_map(|cause| cause.downcast_ref::<io::Error>())
                .any(|io| io.raw_os_error() == Some(eio)),
            "{gone_on_read:?}"
        );

        let eio_on_ioctl =
            anyhow::Error::from(io::Error::from_raw_os_error(eio)).context("HIDIOCGFEATURE failed");
        assert!(!is_device_gone(&eio_on_ioctl));
    }

    /// A datagram socket pair stands in for the node: like hidraw, it
    /// delivers one message per `read(2)`.
    fn fake() -> (HidrawDevice, UnixDatagram) {
        let (node, peer) = UnixDatagram::pair().unwrap();
        let dock = HidrawDevice {
            device: Device::from_fd(node, "/dev/hidraw-fake"),
        };
        (dock, peer)
    }

    #[test]
    fn input_is_waited_for_then_read_or_drained() {
        let (dock, peer) = fake();
        assert_eq!(dock.path(), Path::new("/dev/hidraw-fake"));

        // Nothing queued: a deadline already passed answers at once, a short
        // one waits it out.
        assert!(!dock.wait_for_input(Instant::now()).unwrap());
        let deadline = Instant::now() + Duration::from_millis(5);
        assert!(!dock.wait_for_input(deadline).unwrap());
        assert!(Instant::now() >= deadline);

        let mut buf = [0u8; 8];
        peer.send(&[0, 0, 0, 0, 3, 0, 2, 0]).unwrap();
        assert!(dock.wait_for_input(Instant::now()).unwrap());
        assert_eq!(dock.read_input_report(&mut buf).unwrap(), 8);
        assert_eq!(buf, [0, 0, 0, 0, 3, 0, 2, 0]);

        // A burst of motion is thrown away in one call, not read one by one.
        for _ in 0..10 {
            peer.send(&[0; 8]).unwrap();
        }
        dock.drain_input_reports().unwrap();
        assert!(!dock.wait_for_input(Instant::now()).unwrap());
    }

    /// One drain pass was bounded by the kernel's per-handle queue (64) so a
    /// moving mouse could not keep it going; hidraw's bound must stay that.
    #[test]
    fn a_drain_pass_is_bounded_by_the_kernel_queue() {
        assert_eq!(hidraw::INPUT_QUEUE_LEN, 64);
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
