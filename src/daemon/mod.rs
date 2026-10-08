//! The daemon behind `--upower` (with `--hold`) and `--watch`: one loop that
//! owns the dock's hidraw, bridges the mouse battery to UPower and holds a
//! color, on the calendar of [`schedule`].
//!
//! It prints nothing: it logs — info for its transitions, debug for each poll
//! and re-apply, warn for a transient error it retries — and under systemd
//! that is the journal. It ends on a signal, or when the dock is unplugged;
//! neither is an error of its own ([`Stop`]), and the exit status is 0.

mod schedule;

use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use log::{debug, info, trace, warn};
use signal_hook::consts::{SIGINT, SIGTERM};

use crate::cli::ColorName;
use crate::commands::apply_color;
use crate::hid::{HidrawDevice, HungUp, is_device_gone};
use crate::protocol::{BatteryStatus, query_battery};
use crate::uhid::{self, Battery, CreateErrorKind, Handle, Kind, Wakeup};
use schedule::{BATTERY_WITHDRAW_AFTER, PollOutcome, Schedule, should_withdraw_battery};

/// One re-apply of the held color, `reason` saying which. Only a vanished
/// dock is an error: a transient transfer failure (EPIPE, ETIMEDOUT while the
/// firmware is busy) is a warning and left to the next re-apply, as
/// `poll_battery` does for the battery — restarting the service over it would
/// gain nothing.
fn reapply_color(dock: &HidrawDevice, color: ColorName, reason: &str) -> Result<()> {
    match apply_color(dock, color) {
        Ok(()) => {
            debug!("re-applied '{}' ({reason})", color.as_str());
            Ok(())
        }
        Err(err) if is_device_gone(&err) => Err(err.context("dock disconnected")),
        Err(err) => {
            warn!("re-apply ({reason}) failed, will retry: {err:#}");
            Ok(())
        }
    }
}

/// One battery poll for the daemon loop: `None` when the mouse does not
/// answer (asleep, out of range), an error only when the dock itself is gone.
/// Each outcome is a debug line, with the firmware's reason for a miss
/// (status 0x04 is its "no RF reply").
fn poll_battery(dock: &HidrawDevice) -> Result<Option<BatteryStatus>> {
    match query_battery(dock) {
        Ok(status) => {
            debug!("battery poll: {status}");
            Ok(Some(status))
        }
        Err(err) if is_device_gone(&err) => Err(err.context("dock disconnected")),
        Err(err) => {
            debug!("battery poll unanswered: {err:#}");
            Ok(None)
        }
    }
}

/// The UPower side of the daemon: the `/dev/uhid` handle, and the virtual
/// battery while one is exposed. The battery exists only while the mouse
/// answers: it appears with a first real reading — never a made-up level —
/// and is withdrawn once the mouse has been silent for
/// `BATTERY_WITHDRAW_AFTER`. The handle moves between the two states — the
/// one systemd passed cannot be opened again.
enum Bridge {
    /// `--watch`: no bridge at all.
    Off,
    /// Nothing exposed: the handle waits for a first reading.
    Absent(Handle),
    Exposed {
        battery: Battery,
        // The last reading, so a charging flip is told from a refresh.
        last: BatteryStatus,
        // When the current run of unanswered polls began.
        silent_since: Option<Instant>,
    },
}

impl Bridge {
    fn is_on(&self) -> bool {
        !matches!(self, Self::Off)
    }

    fn is_exposed(&self) -> bool {
        matches!(self, Self::Exposed { .. })
    }

    fn battery_mut(&mut self) -> Option<&mut Battery> {
        match self {
            Self::Exposed { battery, .. } => Some(battery),
            Self::Off | Self::Absent(_) => None,
        }
    }

    /// The battery was polled at `now`: a first reading exposes it, a new
    /// one is mirrored, a held silence withdraws it. The states move the
    /// handle between them, so the transition takes the bridge by value; on
    /// an error nothing of it is left — the kernel refused the device, or the
    /// update failed and the battery is dropped — and the bridge is `Off`.
    fn after_poll(&mut self, status: Option<BatteryStatus>, now: Instant) -> Result<()> {
        *self = std::mem::replace(self, Self::Off).step(status, now)?;
        Ok(())
    }

    fn step(self, status: Option<BatteryStatus>, now: Instant) -> Result<Self> {
        match (self, status) {
            (Self::Absent(handle), Some(first)) => {
                let battery = Battery::create(handle, &uhid::identity(), Kind::Mouse, first.into())
                    .map_err(|err| {
                        // The identity is a constant (`uhid::identity`), so the
                        // kernel refusing it is a bug in razerd — no retry
                        // would help.
                        let context = if err.kind() == CreateErrorKind::InvalidIdentity {
                            "the virtual battery identity is invalid — this is a bug in razerd"
                        } else {
                            "cannot create the virtual battery device"
                        };
                        anyhow::Error::new(err).context(context)
                    })?;
                info!("battery exposed to UPower: {first}");
                Ok(Self::Exposed {
                    battery,
                    last: first,
                    silent_since: None,
                })
            }
            (
                Self::Exposed {
                    mut battery, last, ..
                },
                Some(status),
            ) => {
                // Pushed even when unchanged: each push past the kernel's 30 s
                // rate-limit re-announces the battery to UPower.
                battery
                    .update(status.into())
                    .context("updating the virtual battery")?;
                // Docked or lifted: a transition. A level that moved is the
                // poll's own debug line.
                if status.charging != last.charging {
                    info!("battery: {status}");
                }
                Ok(Self::Exposed {
                    battery,
                    last: status,
                    silent_since: None,
                })
            }
            (
                Self::Exposed {
                    battery,
                    last,
                    silent_since,
                },
                None,
            ) => {
                let since = silent_since.unwrap_or(now);
                if should_withdraw_battery(now.duration_since(since)) {
                    let handle = battery.destroy();
                    info!(
                        "battery withdrawn — mouse silent for {} s (asleep, off or out of range)",
                        BATTERY_WITHDRAW_AFTER.as_secs()
                    );
                    return Ok(Self::Absent(handle));
                }
                // A lost poll so far: keep the last reading, retry shortly.
                Ok(Self::Exposed {
                    battery,
                    last,
                    silent_since: Some(since),
                })
            }
            // Still absent, or no bridge at all (the loop never polls then).
            (bridge, _) => Ok(bridge),
        }
    }

    /// Stopping: take the battery out of UPower now rather than leave that to
    /// the kernel when the handle closes with the process.
    fn withdraw(self) {
        if let Self::Exposed { battery, .. } = self {
            drop(battery.destroy());
            info!("battery withdrawn");
        }
    }
}

/// Why the loop ended. Neither is a failure of the daemon: the battery is
/// withdrawn, the reason is an info line, and the exit status is 0 — under
/// `razerd.service` the dock going is the `BindsTo=` stop, and a unit with
/// `Restart=on-failure` (`razerd-watch.service`) is not restarted over it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// SIGTERM (`systemctl stop`) or SIGINT (Ctrl-C).
    Signal,
    /// The dock's hidraw node is gone: an exchange or a read met it
    /// ([`is_device_gone`]), or the wait found it hung up ([`HungUp`]).
    DockUnplugged,
}

/// One wait of the daemon loop: on the virtual battery's device when there
/// is one — the kernel's requests must be answered meanwhile — and on the
/// dock's input stream when the schedule looks at it, until `deadline`. With
/// neither battery nor deadline it is a plain `poll()`, which a signal still
/// cuts short.
///
/// A wait that fails with no battery to blame — `poll()` refused, or the
/// descriptor it watched the dock on is "hung up, in error or not open", as
/// uhid-battery words it with no type to match on — has the dock asked
/// whether it is gone ([`HidrawDevice::is_hung_up`]): an unplugged dock is
/// [`HungUp`] in the error, the loop's cue to stop, and anything else is the
/// failure it was.
fn wait(
    dock: &HidrawDevice,
    bridge: &mut Bridge,
    deadline: Option<Instant>,
    input: Option<BorrowedFd<'_>>,
) -> Result<Wakeup> {
    let batteries: &mut [Battery] = match bridge.battery_mut() {
        Some(battery) => std::slice::from_mut(battery),
        None => &mut [],
    };
    let waited = match uhid::serve_all(batteries, deadline, input) {
        Ok(wakeup) => Ok(wakeup),
        Err(err) if err.index().is_none() && dock.is_hung_up()? => {
            Err(anyhow::Error::new(err).context(HungUp))
        }
        Err(err) => Err(anyhow::Error::new(err)),
    };
    waited.context("waiting on the dock and the virtual battery")
}

/// The loop behind `--upower` (with `--hold`) and `--watch`: one owner of the
/// dock's hidraw, one calendar, and so one feature-report exchange at a time
/// (see [`Schedule`] and [`schedule::Due`]). Runs until the process is
/// signalled — SIGTERM from `systemctl stop`, Ctrl-C — or the dock is
/// unplugged, and then withdraws the battery before returning; both are an
/// `Ok`, see [`Stop`]. An `Err` is a failure of the daemon's own (the kernel
/// refusing the virtual battery, a wait that failed with the dock present).
fn run_daemon(dock: &HidrawDevice, hold: Option<ColorName>, mut bridge: Bridge) -> Result<()> {
    // The handlers only raise the flag. The wait below returns on the signal
    // (`Wakeup::Interrupted`), and the flag is read before every wait, so a
    // signal that lands during an exchange — whose sleeps and ioctls are
    // restarted — is seen before the next wait rather than after it.
    let signalled = Arc::new(AtomicBool::new(false));
    for signal in [SIGTERM, SIGINT] {
        signal_hook::flag::register(signal, Arc::clone(&signalled))
            .context("installing the signal handler")?;
    }

    if let Some(color) = hold {
        apply_color(dock, color).context("initial color apply failed")?;
        debug!("applied '{}' at start", color.as_str());
    }
    let mut schedule = Schedule::new(Instant::now(), hold.is_some(), bridge.is_on());
    dock.drain_input_reports()?;

    let stop = loop {
        if signalled.load(Ordering::Relaxed) {
            break Stop::Signal;
        }
        match turn(dock, hold, &mut bridge, &mut schedule) {
            Ok(()) => {}
            // The dock going is not an error of the daemon — only the handle
            // is dead. The chain says where it was met, for the curious.
            Err(err) if is_device_gone(&err) => {
                debug!("{err:#}");
                break Stop::DockUnplugged;
            }
            Err(err) => return Err(err),
        }
    };

    match stop {
        Stop::Signal => info!("stopping on signal"),
        Stop::DockUnplugged => info!("dock unplugged — stopping"),
    }
    bridge.withdraw();
    Ok(())
}

/// One turn of the loop: what the calendar owes — the battery poll first, the
/// color last, see [`schedule::Due`] — then the wait until the next thing
/// due, or input from the dock, or a signal.
fn turn(
    dock: &HidrawDevice,
    hold: Option<ColorName>,
    bridge: &mut Bridge,
    schedule: &mut Schedule,
) -> Result<()> {
    let due = schedule.take_due(Instant::now());
    if due.poll {
        let exposed = bridge.is_exposed();
        let status = poll_battery(dock)?;
        let now = Instant::now();
        schedule.on_polled(now, PollOutcome::of(status.is_some(), exposed));
        bridge.after_poll(status, now)?;
    }
    if let (Some(color), Some(reason)) = (hold, due.reapply.reason()) {
        reapply_color(dock, color, &reason)?;
    }

    let now = Instant::now();
    let input = schedule.watches_input(now).then(|| dock.as_fd());
    match wait(dock, bridge, schedule.next_wakeup(now), input)? {
        Wakeup::Wake => {
            trace!("input from the dock — the mouse moves");
            dock.drain_input_reports()?;
            schedule.on_input(Instant::now());
        }
        // Interrupted: the loop looks at the flag.
        Wakeup::Interrupted | Wakeup::Deadline => {}
    }
    Ok(())
}

/// Hold `color` persistently, without the battery bridge: re-apply it the
/// moment the mouse wakes (detected as input resuming after a quiet gap) and,
/// while the mouse is in use, on a slow safety cadence to correct any
/// spontaneous drift. For a setup without `/dev/uhid` or a user unit alone;
/// otherwise `--upower --hold` does the same from the one daemon. Runs until
/// the process is signalled (Ctrl-C, or `systemctl stop`) or the dock is
/// unplugged.
pub(crate) fn run_watch(dock: &HidrawDevice, color: ColorName) -> Result<()> {
    info!(
        "watching {} — holding '{}', re-applying on wake",
        dock.path().display(),
        color.as_str()
    );
    run_daemon(dock, Some(color), Bridge::Off)
}

/// The daemon: bridge the mouse battery to UPower — mirror it into a virtual
/// HID device whose battery the kernel registers as a `power_supply` — and
/// hold a color when one is given (see [`Bridge`] for when the battery
/// exists, [`run_watch`] for the color). Runs until the process is signalled
/// (or the dock unplugged); the battery is withdrawn on the way out.
pub(crate) fn run_upower(dock: &HidrawDevice, hold: Option<ColorName>) -> Result<()> {
    // Fail on a missing uhid handle now, not after waiting on the mouse.
    let handle = uhid::open()?;
    let holding = hold.map_or(String::new(), |color| {
        format!(", holding '{}'", color.as_str())
    });
    info!(
        "bridging the mouse battery from {} to UPower{holding} — waiting for a first reading",
        dock.path().display()
    );
    run_daemon(dock, hold, Bridge::Absent(handle))
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixDatagram;
    use std::time::Duration;

    use super::*;

    /// The read end of a pipe whose writer is gone: `poll()` reports it hung
    /// up and never readable, as `hidraw_poll` does an unplugged dock.
    fn unplugged_dock() -> HidrawDevice {
        let (reader, writer) = rustix::pipe::pipe().unwrap();
        drop(writer);
        HidrawDevice::from_fd(reader)
    }

    /// The wait passes a wake-up and a deadline through untouched.
    #[test]
    fn wait_returns_the_wakeup_on_a_live_dock() {
        let (node, peer) = UnixDatagram::pair().unwrap();
        let dock = HidrawDevice::from_fd(node);
        let mut bridge = Bridge::Off;
        let soon = || Some(Instant::now() + Duration::from_millis(20));

        assert_eq!(
            wait(&dock, &mut bridge, soon(), Some(dock.as_fd())).unwrap(),
            Wakeup::Deadline
        );
        peer.send(&[0; 8]).unwrap();
        assert_eq!(
            wait(&dock, &mut bridge, soon(), Some(dock.as_fd())).unwrap(),
            Wakeup::Wake
        );
        // Not watching the input: the report queued there is not a wake-up.
        assert_eq!(
            wait(&dock, &mut bridge, soon(), None).unwrap(),
            Wakeup::Deadline
        );
    }

    /// Regression: the wait on an unplugged dock failed as "the wake
    /// descriptor is hung up, in error or not open" — exit 1, a failed unit
    /// under `systemctl status` — where the dock going is no failure. The
    /// error now carries the verdict the loop stops on.
    #[test]
    fn wait_on_an_unplugged_dock_is_the_dock_gone() {
        let dock = unplugged_dock();
        let mut bridge = Bridge::Off;
        let err = wait(&dock, &mut bridge, None, Some(dock.as_fd())).unwrap_err();
        assert!(is_device_gone(&err), "{err:#}");
        // The chain keeps uhid-battery's wording for the debug line.
        assert!(format!("{err:#}").contains("hung up"), "{err:#}");
    }

    /// The whole loop: nothing to hold, no bridge, the dock gone — it
    /// returns `Ok`, the way it does on a signal, rather than the error that
    /// made `main` exit 1.
    #[test]
    fn the_daemon_stops_cleanly_when_the_dock_is_unplugged() {
        let dock = unplugged_dock();
        run_daemon(&dock, None, Bridge::Off).unwrap();
    }
}
