//! The verb behind each CLI flag: one `run_*` function per action.

use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use signal_hook::consts::{SIGINT, SIGTERM};

use crate::cli::ColorName;
use crate::hid::{HidrawDevice, is_device_gone};
use crate::protocol::{
    BatteryStatus, DEFAULT_DPI_ACTIVE_STAGE, DEFAULT_DPI_STAGES, TX_ID_DOCK, TX_ID_MOUSE,
    dock_rgb_report, format_dpi, mouse_via_dock_rgb_report, query_battery, query_dpi,
    query_dpi_stages, query_firmware, query_profiles, query_serial, set_dpi, set_dpi_stages,
};
use crate::uhid::{self, Battery, CreateErrorKind, Handle, Kind, Wakeup};

// A held color (`--hold`, `--watch`) is re-applied when the mouse wakes. The
// dock emits no dedicated wake event — it just resumes forwarding mouse-motion
// input reports the instant the mouse comes back. So we treat "input resumed
// after a quiet gap of at least this long" as a wake. It must sit above an
// ordinary pause in movement yet below the mouse's own sleep timeout (Razer's
// is much longer).
const WAKE_IDLE_THRESHOLD: Duration = Duration::from_secs(5);

// While the mouse is in use we also re-apply the color on this cadence, as a
// safety net against the firmware going back to its onboard lighting without
// a wake we can see (a pause shorter than `WAKE_IDLE_THRESHOLD`, a profile
// switch). A tick with no input since the previous one is skipped, so a
// sleeping or absent mouse costs nothing.
const WATCH_SAFETY_INTERVAL: Duration = Duration::from_secs(60);

// A wake re-apply can land too early: lifting the mouse off the dock wakes it
// while it still shows its charging lighting, and when the firmware then
// switches to battery power it reloads the onboard profile's lighting over the
// color just sent. So every wake re-apply is followed by a second one, once
// that transition is over.
const WATCH_FOLLOW_UP_DELAY: Duration = Duration::from_secs(2);

// `--upower` refreshes the battery on this slow cadence — enough for the level,
// which moves slowly. Charging flips are caught by the motion-driven polls
// below instead; this is the net under them (and what notices "full").
const BATTERY_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

// The charging state only changes when the mouse is put on or lifted off the
// dock, and either one moves it. So rather than polling fast all day, poll
// around motion: shortly after the mouse starts moving (lifted), and once it
// has come to rest (docked) — twice, the firmware's charging flag can trail
// the contacts by a few seconds.
const BATTERY_MOTION_START_DELAY: Duration = Duration::from_secs(1);
const BATTERY_REST_DELAYS: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(8)];

// A pause shorter than this is still the same movement.
const MOTION_GAP: Duration = Duration::from_secs(2);

// While the mouse moves the dock streams ~1000 reports/s. We only need to know
// *that* it moves: look at the stream this often and discard the backlog.
const MOTION_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

// While no battery is exposed (no reading yet, or withdrawn), moving the mouse
// triggers a query at once — it is typically asleep when the service starts at
// boot. This cadence only covers a mouse that becomes reachable without moving.
const BATTERY_ABSENT_RETRY: Duration = Duration::from_secs(10);

// A mouse that stopped answering is asleep, switched off, out of range or
// unpaired — the dock reports the same "no RF reply" (status 0x04, within
// ~10 ms) for all of them. Once a poll goes unanswered, retry on a short
// cadence; if the silence holds this long the battery is withdrawn rather than
// left showing a stale level, the way a Bluetooth peripheral's battery vanishes
// on disconnect. Several retries, so one lost poll never flaps it — and
// withdrawing is cheap to undo: the battery is back ~1 s after the mouse moves.
const BATTERY_UNANSWERED_RETRY: Duration = Duration::from_secs(3);
const BATTERY_WITHDRAW_AFTER: Duration = Duration::from_secs(10);

// `run_check` and `run_info` cannot fail, but every action keeps the same
// signature so `main` dispatches them uniformly.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn run_check(dock: &HidrawDevice) -> Result<()> {
    println!(
        "✓ Razer Mouse Dock Pro ({}) accessible",
        dock.path().display()
    );
    match query_battery(dock) {
        Ok(_) => println!("✓ Razer Basilisk V3 Pro 35K (via Dock) responding over RF"),
        Err(_) => println!("⚠ Mouse not responding — is it paired and awake?"),
    }
    Ok(())
}

/// Push `color` to the dock ring and, via RF, to the mouse. Silent so it can be
/// called repeatedly by the daemon.
fn apply_color(dock: &HidrawDevice, color: ColorName) -> Result<()> {
    let label = color.as_str();
    dock.send_feature(&dock_rgb_report(color.rgb()))
        .with_context(|| format!("failed to set dock color '{label}'"))?;
    // Sent through the dock; if the mouse is not paired, the dock drops it silently.
    dock.send_feature(&mouse_via_dock_rgb_report(color.rgb()))
        .with_context(|| format!("failed to set mouse color '{label}'"))?;
    Ok(())
}

pub(crate) fn run_color(dock: &HidrawDevice, color: ColorName) -> Result<()> {
    apply_color(dock, color)?;
    let label = color.as_str();
    println!("✓ Dock: {label}");
    println!("✓ Mouse: {label}");
    Ok(())
}

/// Diagnostic: print every HID input report the dock emits, with a relative
/// timestamp. Run it, then exercise the mouse (let it sleep, then move it to
/// wake it) and watch whether reports appear at the sleep/wake moments. Runs
/// until interrupted with Ctrl-C.
pub(crate) fn run_sniff(dock: &HidrawDevice) -> Result<()> {
    println!("Sniffing input reports from {}.", dock.path().display());
    println!("Exercise the mouse: let it sleep, then move it to wake it.");
    println!("Press Ctrl-C to stop.\n");

    let start = Instant::now();
    let mut buf = [0u8; 256];
    loop {
        let n = dock.read_input_report(&mut buf)?;
        if n == 0 {
            continue;
        }
        let elapsed = start.elapsed().as_secs_f64();
        let hex: Vec<String> = buf[..n].iter().map(|b| format!("{b:02x}")).collect();
        println!("[{elapsed:8.3}s] {n:3} bytes: {}", hex.join(" "));
    }
}

/// Should we re-apply the color when an input report arrives after `idle_gap`
/// of silence? Only when the gap is long enough to mean the mouse actually
/// slept — a brief pause in movement must not trigger a re-apply.
fn should_reapply_on_wake(idle_gap: Duration) -> bool {
    idle_gap >= WAKE_IDLE_THRESHOLD
}

/// Which re-applies the held color owes at a given instant. One re-apply
/// settles them all — the same bytes go out whatever the reason — and the
/// log line names every reason that fell due.
#[derive(Debug, Default, PartialEq, Eq)]
struct DueReapplies {
    /// The mouse woke, after this long idle.
    wake: Option<Duration>,
    follow_up: bool,
    safety: bool,
}

impl DueReapplies {
    /// Why the color is re-applied, or `None` when nothing is owed.
    fn reason(&self) -> Option<String> {
        let mut reasons = Vec::new();
        if let Some(idle_gap) = self.wake {
            reasons.push(format!(
                "mouse woke after {:.0}s idle",
                idle_gap.as_secs_f64()
            ));
        }
        if self.follow_up {
            reasons.push("follow-up".to_owned());
        }
        if self.safety {
            reasons.push("safety refresh".to_owned());
        }
        (!reasons.is_empty()).then(|| reasons.join(", "))
    }
}

/// When a held color is re-applied: on a wake, once more after it, and on
/// the safety tick. Pure bookkeeping over instants, so the policy is testable
/// without hardware — the tick that never fired during use lived in the I/O
/// loop, out of any test's reach.
struct HoldSchedule {
    last_input: Instant,
    active_since_safety: bool,
    // A wake seen by `on_input`, owed until `take_due` hands it out.
    wake: Option<Duration>,
    follow_up_at: Option<Instant>,
    // A fixed tick, not "this long after the last input": input arrives by
    // the thousand per second while the mouse moves, and must not push it back.
    safety_at: Instant,
}

impl HoldSchedule {
    fn new(now: Instant) -> Self {
        Self {
            last_input: now,
            active_since_safety: false,
            wake: None,
            follow_up_at: None,
            safety_at: now + WATCH_SAFETY_INTERVAL,
        }
    }

    /// What is due at `now`; taking it re-arms the schedule. A safety tick
    /// with no input since the previous one owes nothing, so a sleeping or
    /// absent mouse costs nothing.
    fn take_due(&mut self, now: Instant) -> DueReapplies {
        let mut due = DueReapplies {
            wake: self.wake.take(),
            ..DueReapplies::default()
        };
        if self.follow_up_at.is_some_and(|at| now >= at) {
            self.follow_up_at = None;
            due.follow_up = true;
        }
        if now >= self.safety_at {
            self.safety_at = now + WATCH_SAFETY_INTERVAL;
            due.safety = std::mem::take(&mut self.active_since_safety);
        }
        due
    }

    /// Until when to wait for input before something is due: at once while a
    /// wake is owed.
    fn next_deadline(&self, now: Instant) -> Instant {
        if self.wake.is_some() {
            return now;
        }
        self.follow_up_at
            .map_or(self.safety_at, |at| at.min(self.safety_at))
    }

    /// An input report arrived at `now`. After a real idle gap it is a wake:
    /// the color is owed now, and a follow-up is scheduled.
    fn on_input(&mut self, now: Instant) {
        let idle_gap = now.duration_since(self.last_input);
        self.last_input = now;
        self.active_since_safety = true;
        if should_reapply_on_wake(idle_gap) {
            self.wake = Some(idle_gap);
            self.follow_up_at = Some(now + WATCH_FOLLOW_UP_DELAY);
        }
    }
}

/// One re-apply of the held color, `reason` saying which. Only a vanished
/// dock is an error: a transient transfer failure (EPIPE, ETIMEDOUT while the
/// firmware is busy) is logged and left to the next re-apply, as
/// `poll_battery` does for the battery — restarting the service over it would
/// gain nothing.
fn reapply_color(dock: &HidrawDevice, color: ColorName, reason: &str) -> Result<()> {
    match apply_color(dock, color) {
        Ok(()) => {
            println!("re-applied '{}' ({reason})", color.as_str());
            Ok(())
        }
        Err(err) if is_device_gone(&err) => Err(err.context("dock disconnected")),
        Err(err) => {
            eprintln!("re-apply ({reason}) failed, will retry: {err:#}");
            Ok(())
        }
    }
}

/// One battery poll for the daemon loop: `None` when the mouse does not
/// answer (asleep, out of range), an error only when the dock itself is gone.
fn poll_battery(dock: &HidrawDevice) -> Result<Option<BatteryStatus>> {
    match query_battery(dock) {
        Ok(status) => Ok(Some(status)),
        Err(err) if is_device_gone(&err) => Err(err.context("dock disconnected")),
        Err(_) => Ok(None),
    }
}

/// Should the exposed battery be withdrawn, `unanswered` after the first poll
/// of the current silence failed? Not on one lost poll — only once the retries
/// have kept failing for `BATTERY_WITHDRAW_AFTER`.
fn should_withdraw_battery(unanswered: Duration) -> bool {
    unanswered >= BATTERY_WITHDRAW_AFTER
}

/// How a battery poll went, as far as the next one is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollOutcome {
    /// The mouse answered: back to the slow refresh.
    Answered,
    /// No answer while a battery is exposed: retry shortly
    /// (`BATTERY_UNANSWERED_RETRY`), the battery goes if the silence holds.
    Unanswered,
    /// No answer and nothing exposed: the slow retry (`BATTERY_ABSENT_RETRY`).
    Absent,
}

impl PollOutcome {
    fn of(answered: bool, exposed: bool) -> Self {
        match (answered, exposed) {
            (true, _) => Self::Answered,
            (false, true) => Self::Unanswered,
            (false, false) => Self::Absent,
        }
    }
}

/// When the battery is queried: the slow refresh cadence, plus polls placed
/// around mouse movement (see `BATTERY_MOTION_START_DELAY`) and the retries
/// after an unanswered one. Pure bookkeeping over instants, so the policy is
/// testable without hardware.
struct PollSchedule {
    refresh_at: Instant,
    // Pending "the mouse started moving" poll.
    motion_start_at: Option<Instant>,
    last_motion: Option<Instant>,
    // How many of `BATTERY_REST_DELAYS` were served since the last motion.
    rest_polls_done: usize,
    // Pending retry after an unanswered poll.
    retry_at: Option<Instant>,
}

impl PollSchedule {
    /// A first reading is wanted at once — typically unanswered, the mouse
    /// being asleep when the service starts at boot, and then the retries and
    /// the motion polls take over.
    fn new(now: Instant) -> Self {
        Self {
            refresh_at: now,
            motion_start_at: None,
            last_motion: None,
            rest_polls_done: 0,
            retry_at: None,
        }
    }

    /// The input stream showed activity at `now`.
    fn on_motion(&mut self, now: Instant) {
        let started = self
            .last_motion
            .is_none_or(|last| now.duration_since(last) >= MOTION_GAP);
        if started {
            self.motion_start_at = Some(now + BATTERY_MOTION_START_DELAY);
        }
        self.last_motion = Some(now);
        self.rest_polls_done = 0;
    }

    /// The battery was queried at `now` (whatever triggered it).
    fn on_polled(&mut self, now: Instant, outcome: PollOutcome) {
        self.refresh_at = now + BATTERY_REFRESH_INTERVAL;
        self.retry_at = match outcome {
            PollOutcome::Answered => None,
            PollOutcome::Unanswered => Some(now + BATTERY_UNANSWERED_RETRY),
            PollOutcome::Absent => Some(now + BATTERY_ABSENT_RETRY),
        };
        self.motion_start_at = None;
        if let Some(last) = self.last_motion {
            // Every rest delay that has elapsed is covered by this poll.
            self.rest_polls_done = BATTERY_REST_DELAYS
                .iter()
                .filter(|delay| now >= last + **delay)
                .count();
        }
    }

    fn next_poll_at(&self) -> Instant {
        let rest_at = self
            .last_motion
            .zip(BATTERY_REST_DELAYS.get(self.rest_polls_done))
            .map(|(last, delay)| last + *delay);
        [self.motion_start_at, rest_at, self.retry_at]
            .into_iter()
            .flatten()
            .fold(self.refresh_at, Instant::min)
    }
}

/// What one turn of the daemon loop owes, in the order it is done: the
/// battery poll first, the color last.
///
/// The firmware has one report buffer and no transaction ids. A poll reads
/// its reply back out of that buffer and must find it intact, whereas the
/// color is two writes nobody reads back — sent last, they overwrite a reply
/// already taken. The other way round, the LED command still being forwarded
/// over RF could have the firmware refuse the poll's request (EPIPE) and the
/// poll be counted unanswered: the very collision the two separate processes
/// used to have, and the reason there is one loop.
#[derive(Debug, Default, PartialEq, Eq)]
struct Due {
    poll: bool,
    reapply: DueReapplies,
}

/// The daemon's calendar: the held color's re-applies and the battery's
/// polls, whichever the invocation has, over the one input stream (sampled,
/// never followed report by report — see `MOTION_SAMPLE_INTERVAL`). The next
/// wake-up is the earliest thing either owes, and a turn takes what both owe
/// at once, so the two exchanges are never concurrent, only ordered ([`Due`]).
struct Schedule {
    hold: Option<HoldSchedule>,
    battery: Option<PollSchedule>,
    // The input stream is not looked at before this instant.
    input_muted_until: Instant,
}

impl Schedule {
    fn new(now: Instant, hold: bool, battery: bool) -> Self {
        Self {
            hold: hold.then(|| HoldSchedule::new(now)),
            battery: battery.then(|| PollSchedule::new(now)),
            input_muted_until: now,
        }
    }

    /// What is due at `now`; taking it re-arms the schedules.
    fn take_due(&mut self, now: Instant) -> Due {
        Due {
            poll: self
                .battery
                .as_ref()
                .is_some_and(|battery| now >= battery.next_poll_at()),
            reapply: self
                .hold
                .as_mut()
                .map(|hold| hold.take_due(now))
                .unwrap_or_default(),
        }
    }

    /// The input stream showed activity at `now`: a wake for the color after
    /// a real gap, the start of a movement for the battery.
    fn on_input(&mut self, now: Instant) {
        if let Some(hold) = &mut self.hold {
            hold.on_input(now);
        }
        if let Some(battery) = &mut self.battery {
            battery.on_motion(now);
        }
        self.input_muted_until = now + MOTION_SAMPLE_INTERVAL;
    }

    fn on_polled(&mut self, now: Instant, outcome: PollOutcome) {
        if let Some(battery) = &mut self.battery {
            battery.on_polled(now, outcome);
        }
    }

    fn watches_input(&self, now: Instant) -> bool {
        now >= self.input_muted_until
    }

    /// When to stop waiting: the earliest thing due, or sooner to look at the
    /// input stream again. `None` only with nothing scheduled at all.
    fn next_wakeup(&self, now: Instant) -> Option<Instant> {
        [
            self.hold.as_ref().map(|hold| hold.next_deadline(now)),
            self.battery.as_ref().map(PollSchedule::next_poll_at),
            (!self.watches_input(now)).then_some(self.input_muted_until),
        ]
        .into_iter()
        .flatten()
        .min()
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
        // The last reading, so only changes are logged.
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
    /// one is mirrored, a held silence withdraws it.
    fn after_poll(self, status: Option<BatteryStatus>, now: Instant) -> Result<Self> {
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
                println!("battery: {first}");
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
                if status != last {
                    println!("battery: {status}");
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
                    println!(
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
            println!("battery withdrawn — stopping");
        }
    }
}

/// One wait of the daemon loop: on the virtual battery's device when there
/// is one — the kernel's requests must be answered meanwhile — and on the
/// dock's input stream when the schedule looks at it, until `deadline`. With
/// neither battery nor deadline it is a plain `poll()`, which a signal still
/// cuts short.
fn wait(
    bridge: &mut Bridge,
    deadline: Option<Instant>,
    input: Option<BorrowedFd<'_>>,
) -> Result<Wakeup> {
    let batteries: &mut [Battery] = match bridge.battery_mut() {
        Some(battery) => std::slice::from_mut(battery),
        None => &mut [],
    };
    uhid::serve_all(batteries, deadline, input)
        .context("waiting on the dock and the virtual battery")
}

/// The loop behind `--upower` (with `--hold`) and `--watch`: one owner of the
/// dock's hidraw, one calendar, and so one feature-report exchange at a time
/// (see [`Schedule`] and [`Due`]). Runs until the dock is unplugged or the
/// process is signalled — SIGTERM from `systemctl stop`, Ctrl-C — and then
/// withdraws the battery before exiting.
fn run_daemon(dock: &HidrawDevice, hold: Option<ColorName>, mut bridge: Bridge) -> Result<()> {
    // The handlers only raise the flag. The wait below returns on the signal
    // (`Wakeup::Interrupted`), and the flag is read before every wait, so a
    // signal that lands during an exchange — whose sleeps and ioctls are
    // restarted — is seen before the next wait rather than after it.
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [SIGTERM, SIGINT] {
        signal_hook::flag::register(signal, Arc::clone(&stop))
            .context("installing the signal handler")?;
    }

    if let Some(color) = hold {
        apply_color(dock, color).context("initial color apply failed")?;
    }
    let mut schedule = Schedule::new(Instant::now(), hold.is_some(), bridge.is_on());
    dock.drain_input_reports()?;

    while !stop.load(Ordering::Relaxed) {
        let due = schedule.take_due(Instant::now());
        if due.poll {
            let exposed = bridge.is_exposed();
            let status = poll_battery(dock)?;
            let now = Instant::now();
            schedule.on_polled(now, PollOutcome::of(status.is_some(), exposed));
            bridge = bridge.after_poll(status, now)?;
        }
        if let (Some(color), Some(reason)) = (hold, due.reapply.reason()) {
            reapply_color(dock, color, &reason)?;
        }

        let now = Instant::now();
        let input = schedule.watches_input(now).then(|| dock.as_fd());
        match wait(&mut bridge, schedule.next_wakeup(now), input)? {
            Wakeup::Wake => {
                dock.drain_input_reports()?;
                schedule.on_input(Instant::now());
            }
            // Interrupted: the loop condition looks at the flag.
            Wakeup::Interrupted | Wakeup::Deadline => {}
        }
    }

    bridge.withdraw();
    Ok(())
}

/// Hold `color` persistently, without the battery bridge: re-apply it the
/// moment the mouse wakes (detected as input resuming after a quiet gap) and,
/// while the mouse is in use, on a slow safety cadence to correct any
/// spontaneous drift. For a setup without `/dev/uhid` or a user unit alone;
/// otherwise `--upower --hold` does the same from the one daemon. Runs until
/// the process is signalled (Ctrl-C, or `systemctl stop`).
pub(crate) fn run_watch(dock: &HidrawDevice, color: ColorName) -> Result<()> {
    println!(
        "Watching {} — holding '{}', re-applying on wake. Ctrl-C to stop.",
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
    println!(
        "Bridging the mouse battery from {} to UPower{holding} — waiting for a first reading.",
        dock.path().display()
    );
    run_daemon(dock, hold, Bridge::Absent(handle))
}

pub(crate) fn run_battery(dock: &HidrawDevice) -> Result<()> {
    println!("✓ Battery: {}", query_battery(dock)?);
    Ok(())
}

#[allow(clippy::unnecessary_wraps)] // see `run_check`
pub(crate) fn run_info(dock: &HidrawDevice) -> Result<()> {
    println!("Razer Mouse Dock Pro");
    println!("  Path:     {}", dock.path().display());
    print_field("Serial", query_serial(dock, TX_ID_DOCK).ok());
    print_field("Firmware", query_firmware(dock, TX_ID_DOCK).ok());

    println!();
    println!("Razer Basilisk V3 Pro 35K (via Dock)");
    println!("  Path:     {}", dock.path().display());

    // The serial doubles as a liveness probe: a mouse that is asleep or off
    // answers no RF query. After a first miss, report the remaining fields as
    // absent without further round-trips.
    let serial = query_serial(dock, TX_ID_MOUSE).ok();
    let awake = serial.is_some();
    print_field("Serial", serial);
    print_field(
        "Firmware",
        awake
            .then(|| query_firmware(dock, TX_ID_MOUSE).ok())
            .flatten(),
    );
    match awake.then(|| query_battery(dock).ok()).flatten() {
        Some(s) => {
            println!("  Battery:  {}%", s.percent);
            println!("  Charging: {}", if s.charging { "yes" } else { "no" });
        }
        None => println!("  Battery:  —"),
    }
    print_field(
        "DPI",
        awake
            .then(|| query_dpi(dock).ok())
            .flatten()
            .map(format_dpi),
    );
    print_field(
        "Stages",
        awake.then(|| query_dpi_stages(dock).ok()).flatten(),
    );
    print_field(
        "Profile",
        awake.then(|| query_profiles(dock).ok()).flatten(),
    );

    // What a running `--upower` currently hands to the kernel, read back from
    // sysfs — i.e. what UPower and the desktop's power applet see.
    println!();
    println!("UPower battery (virtual HID device, via --upower)");
    match uhid::exposed_battery() {
        Some(battery) => {
            println!("  Path:     {}", battery.path.display());
            print_field(
                "Level",
                battery.percent.map(|percent| format!("{percent}%")),
            );
            print_field("Status", battery.status);
        }
        None => println!("  Path:     — (not exposed: bridge not running, or mouse silent)"),
    }

    Ok(())
}

fn print_field<T: std::fmt::Display>(label: &str, value: Option<T>) {
    match value {
        Some(v) => println!("  {:<10}{}", format!("{label}:"), v),
        None => println!("  {:<10}—", format!("{label}:")),
    }
}

/// The free slider: pin the sensitivity to one DPI value and collapse the
/// stage table to it, so the Cycle Up Sensitivity Stages button is inert —
/// nothing on the mouse can change the value anymore.
pub(crate) fn run_sensitivity(dock: &HidrawDevice, dpi: u16) -> Result<()> {
    set_dpi_stages(dock, 1, &[dpi])?;
    set_dpi(dock, dpi)?;
    let applied = query_dpi(dock).context("DPI readback failed")?;
    println!(
        "✓ DPI: {} (stages off — Cycle Up Sensitivity Stages button disabled)",
        format_dpi(applied)
    );
    if applied != (dpi, dpi) {
        println!("⚠ requested {dpi}, firmware adjusted it");
    }
    Ok(())
}

/// The Synapse-style "Sensitivity Stages" toggle. On: install the default
/// stage table and give the Cycle Up Sensitivity Stages button its stages
/// back. Off: freeze the current DPI as the only stage, disabling the button.
pub(crate) fn run_sensitivity_stages(dock: &HidrawDevice, enabled: bool) -> Result<()> {
    if enabled {
        let active = DEFAULT_DPI_STAGES[DEFAULT_DPI_ACTIVE_STAGE as usize - 1];
        set_dpi_stages(dock, DEFAULT_DPI_ACTIVE_STAGE, &DEFAULT_DPI_STAGES)?;
        set_dpi(dock, active)?;
        let applied = query_dpi(dock).context("DPI readback failed")?;
        let stages: Vec<String> = DEFAULT_DPI_STAGES.iter().map(u16::to_string).collect();
        println!(
            "✓ DPI: {} (stages on — Cycle Up Sensitivity Stages button cycles {})",
            format_dpi(applied),
            stages.join("/")
        );
    } else {
        // Freeze whatever the sensor currently runs at.
        let (x, y) = query_dpi(dock).context("DPI query failed")?;
        if x != y {
            println!("⚠ axes differ ({x} / {y}) — freezing both at {x}");
        }
        run_sensitivity(dock, x)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAKE: DueReapplies = DueReapplies {
        wake: Some(WAKE_IDLE_THRESHOLD),
        follow_up: false,
        safety: false,
    };
    const FOLLOW_UP: DueReapplies = DueReapplies {
        wake: None,
        follow_up: true,
        safety: false,
    };
    const SAFETY: DueReapplies = DueReapplies {
        wake: None,
        follow_up: false,
        safety: true,
    };

    #[test]
    fn poll_schedule_wants_a_first_reading_at_once_then_the_slow_refresh() {
        let t0 = Instant::now();
        let mut schedule = PollSchedule::new(t0);
        assert_eq!(schedule.next_poll_at(), t0);

        schedule.on_polled(t0, PollOutcome::Answered);
        assert_eq!(schedule.next_poll_at(), t0 + BATTERY_REFRESH_INTERVAL);
        let t1 = t0 + BATTERY_REFRESH_INTERVAL;
        schedule.on_polled(t1, PollOutcome::Answered);
        assert_eq!(schedule.next_poll_at(), t1 + BATTERY_REFRESH_INTERVAL);
    }

    #[test]
    fn poll_schedule_polls_after_motion_starts_then_twice_at_rest() {
        let t0 = Instant::now();
        let mut schedule = PollSchedule::new(t0);
        schedule.on_polled(t0, PollOutcome::Answered);

        // Lifted off the dock: one poll shortly after the movement starts.
        schedule.on_motion(t0);
        assert_eq!(schedule.next_poll_at(), t0 + BATTERY_MOTION_START_DELAY);

        // Still moving half a second later: same movement, no new start poll,
        // and the rest polls slide with the last motion.
        let t1 = t0 + MOTION_SAMPLE_INTERVAL;
        schedule.on_motion(t1);
        assert_eq!(schedule.next_poll_at(), t0 + BATTERY_MOTION_START_DELAY);
        schedule.on_polled(t0 + BATTERY_MOTION_START_DELAY, PollOutcome::Answered);
        assert_eq!(schedule.next_poll_at(), t1 + BATTERY_REST_DELAYS[0]);

        // Put down (docked): first rest poll, then the late confirmation…
        schedule.on_polled(t1 + BATTERY_REST_DELAYS[0], PollOutcome::Answered);
        assert_eq!(schedule.next_poll_at(), t1 + BATTERY_REST_DELAYS[1]);
        // …then back to the slow refresh until it moves again.
        let t2 = t1 + BATTERY_REST_DELAYS[1];
        schedule.on_polled(t2, PollOutcome::Answered);
        assert_eq!(schedule.next_poll_at(), t2 + BATTERY_REFRESH_INTERVAL);

        // A new movement after a real pause starts the cycle over.
        let t3 = t2 + MOTION_GAP;
        schedule.on_motion(t3);
        assert_eq!(schedule.next_poll_at(), t3 + BATTERY_MOTION_START_DELAY);
    }

    #[test]
    fn a_late_poll_covers_every_rest_delay_already_elapsed() {
        let t0 = Instant::now();
        let mut schedule = PollSchedule::new(t0);
        schedule.on_motion(t0);
        // One poll long after the mouse came to rest: nothing left to confirm.
        let late = t0 + BATTERY_REST_DELAYS[1] + Duration::from_secs(1);
        schedule.on_polled(late, PollOutcome::Answered);
        assert_eq!(schedule.next_poll_at(), late + BATTERY_REFRESH_INTERVAL);
    }

    #[test]
    fn battery_survives_lost_polls_but_not_a_held_silence() {
        // The first failed poll, and the retries right after it, keep the battery.
        assert!(!should_withdraw_battery(Duration::ZERO));
        assert!(!should_withdraw_battery(BATTERY_UNANSWERED_RETRY * 2));
        assert!(!should_withdraw_battery(
            BATTERY_WITHDRAW_AFTER.saturating_sub(Duration::from_millis(1))
        ));
        assert!(should_withdraw_battery(BATTERY_WITHDRAW_AFTER));
        assert!(should_withdraw_battery(Duration::from_secs(3600)));
        // The silence is measured from the first failure, so it takes several
        // retries — never a single one — to get there.
        assert!(BATTERY_WITHDRAW_AFTER >= BATTERY_UNANSWERED_RETRY * 3);
    }

    #[test]
    fn poll_schedule_retries_quickly_while_the_mouse_is_silent() {
        let t0 = Instant::now();
        let mut schedule = PollSchedule::new(t0);
        schedule.on_polled(t0, PollOutcome::Answered);

        // An unanswered poll is retried shortly, not a refresh interval later…
        let t1 = t0 + BATTERY_REFRESH_INTERVAL;
        schedule.on_polled(t1, PollOutcome::Unanswered);
        assert_eq!(schedule.next_poll_at(), t1 + BATTERY_UNANSWERED_RETRY);
        let t2 = t1 + BATTERY_UNANSWERED_RETRY;
        schedule.on_polled(t2, PollOutcome::Unanswered);
        assert_eq!(schedule.next_poll_at(), t2 + BATTERY_UNANSWERED_RETRY);

        // …and an answer drops back to the slow cadence.
        let t3 = t2 + BATTERY_UNANSWERED_RETRY;
        schedule.on_polled(t3, PollOutcome::Answered);
        assert_eq!(schedule.next_poll_at(), t3 + BATTERY_REFRESH_INTERVAL);
    }

    /// Nothing exposed (at boot, or after a withdrawal): the slow retry, and
    /// a poll a second after the mouse moves — a sleeping mouse wakes that way.
    #[test]
    fn poll_schedule_retries_slowly_while_nothing_is_exposed() {
        let t0 = Instant::now();
        let mut schedule = PollSchedule::new(t0);
        schedule.on_polled(t0, PollOutcome::Absent);
        assert_eq!(schedule.next_poll_at(), t0 + BATTERY_ABSENT_RETRY);
        assert!(BATTERY_ABSENT_RETRY > BATTERY_UNANSWERED_RETRY);

        let t1 = t0 + Duration::from_secs(4);
        schedule.on_motion(t1);
        assert_eq!(schedule.next_poll_at(), t1 + BATTERY_MOTION_START_DELAY);
    }

    #[test]
    fn hold_wake_reapplies_and_owes_one_follow_up() {
        let t0 = Instant::now();
        let mut schedule = HoldSchedule::new(t0);

        // Ordinary movement: no wake, nothing scheduled.
        let t1 = t0 + Duration::from_secs(1);
        schedule.on_input(t1);
        assert_eq!(schedule.take_due(t1), DueReapplies::default());
        assert_eq!(schedule.next_deadline(t1), t0 + WATCH_SAFETY_INTERVAL);

        // Input after a real gap is a wake, owed at once, with its idle time.
        let wake = t1 + WAKE_IDLE_THRESHOLD;
        schedule.on_input(wake);
        assert_eq!(schedule.next_deadline(wake), wake);
        assert_eq!(schedule.take_due(wake), WAKE);
        assert_eq!(schedule.next_deadline(wake), wake + WATCH_FOLLOW_UP_DELAY);

        // The follow-up is owed once, at its time — not before, not twice.
        assert_eq!(
            schedule.take_due(wake + Duration::from_secs(1)),
            DueReapplies::default()
        );
        assert_eq!(schedule.take_due(wake + WATCH_FOLLOW_UP_DELAY), FOLLOW_UP);
        assert_eq!(
            schedule.take_due(wake + WATCH_FOLLOW_UP_DELAY),
            DueReapplies::default()
        );
    }

    /// Regression: the dock streams ~1000 reports/s while the mouse moves, and
    /// the tick used to be re-armed by each of them — it never fired during use.
    #[test]
    fn hold_safety_tick_is_not_postponed_by_continuous_input() {
        let t0 = Instant::now();
        let mut schedule = HoldSchedule::new(t0);
        let step = Duration::from_millis(50);
        let mut now = t0;
        while now + step < t0 + WATCH_SAFETY_INTERVAL {
            now += step;
            schedule.on_input(now);
            assert_eq!(schedule.take_due(now), DueReapplies::default());
            assert_eq!(schedule.next_deadline(now), t0 + WATCH_SAFETY_INTERVAL);
        }

        let tick = t0 + WATCH_SAFETY_INTERVAL;
        assert_eq!(schedule.take_due(tick), SAFETY);
        // Re-armed from the tick, and the next one is just as punctual.
        assert_eq!(schedule.next_deadline(tick), tick + WATCH_SAFETY_INTERVAL);
        schedule.on_input(tick + step);
        assert_eq!(schedule.take_due(tick + WATCH_SAFETY_INTERVAL), SAFETY);
    }

    #[test]
    fn hold_safety_tick_owes_nothing_without_input() {
        let t0 = Instant::now();
        let mut schedule = HoldSchedule::new(t0);
        // Asleep or absent: the tick passes, nothing is sent, and it re-arms.
        let tick = t0 + WATCH_SAFETY_INTERVAL;
        assert_eq!(schedule.take_due(tick), DueReapplies::default());
        assert_eq!(schedule.next_deadline(tick), tick + WATCH_SAFETY_INTERVAL);

        // One input is enough for the next tick to refresh — once. (After a
        // minute of silence it is a wake as well, owed at once.)
        schedule.on_input(tick + Duration::from_secs(1));
        assert!(
            schedule
                .take_due(tick + Duration::from_secs(1))
                .wake
                .is_some()
        );
        assert!(schedule.take_due(tick + WATCH_SAFETY_INTERVAL).safety);
        assert!(!schedule.take_due(tick + WATCH_SAFETY_INTERVAL * 2).safety);
    }

    #[test]
    fn hold_follow_up_and_safety_fall_due_together_as_one_reapply() {
        let t0 = Instant::now();
        let mut schedule = HoldSchedule::new(t0);
        let wake = t0 + WATCH_SAFETY_INTERVAL.saturating_sub(Duration::from_secs(1));
        schedule.on_input(wake);
        assert!(schedule.take_due(wake).wake.is_some());
        // The tick (t0 + 60 s) comes before the follow-up (wake + 2 s).
        assert_eq!(schedule.next_deadline(wake), t0 + WATCH_SAFETY_INTERVAL);
        let due = schedule.take_due(wake + WATCH_FOLLOW_UP_DELAY);
        assert!(due.follow_up && due.safety);
        assert_eq!(due.reason().as_deref(), Some("follow-up, safety refresh"));
    }

    #[test]
    fn hold_reapplies_only_after_a_real_idle_gap() {
        // Brief pauses in movement must not re-apply.
        assert!(!should_reapply_on_wake(Duration::from_millis(500)));
        assert!(!should_reapply_on_wake(
            WAKE_IDLE_THRESHOLD.saturating_sub(Duration::from_millis(1))
        ));
        // A gap at/above the threshold means the mouse slept → re-apply.
        assert!(should_reapply_on_wake(WAKE_IDLE_THRESHOLD));
        assert!(should_reapply_on_wake(Duration::from_secs(120)));
    }

    #[test]
    fn reapply_reason_names_everything_due() {
        assert_eq!(DueReapplies::default().reason(), None);
        assert_eq!(WAKE.reason().as_deref(), Some("mouse woke after 5s idle"));
        assert_eq!(FOLLOW_UP.reason().as_deref(), Some("follow-up"));
        assert_eq!(SAFETY.reason().as_deref(), Some("safety refresh"));
    }

    /// The daemon's turn after a wake: the color at once, the battery a
    /// second later, and at T+2 s the follow-up color and the first rest poll
    /// fall due in the same turn — one `take_due`, both owed, the poll done
    /// before the color — rather than in two turns with nothing to order them.
    #[test]
    fn daemon_wake_colors_at_once_polls_a_second_later_and_orders_the_rest() {
        let t0 = Instant::now();
        let mut schedule = Schedule::new(t0, true, true);

        // Start: a first reading is wanted, the color was applied by the loop.
        assert_eq!(schedule.next_wakeup(t0), Some(t0));
        assert_eq!(
            schedule.take_due(t0),
            Due {
                poll: true,
                ..Due::default()
            }
        );
        schedule.on_polled(t0, PollOutcome::Answered);
        assert_eq!(schedule.take_due(t0), Due::default());

        // The mouse wakes after a long sleep: the color is owed at once, the
        // stream is muted for a sample, and the start poll is a second away.
        let wake = t0 + Duration::from_secs(30);
        schedule.on_input(wake);
        assert_eq!(schedule.next_wakeup(wake), Some(wake));
        assert_eq!(
            schedule.take_due(wake),
            Due {
                poll: false,
                reapply: DueReapplies {
                    wake: Some(Duration::from_secs(30)),
                    ..DueReapplies::default()
                },
            }
        );
        assert!(!schedule.watches_input(wake));
        assert_eq!(
            schedule.next_wakeup(wake),
            Some(wake + MOTION_SAMPLE_INTERVAL)
        );
        let sample = wake + MOTION_SAMPLE_INTERVAL;
        assert!(schedule.watches_input(sample));
        assert_eq!(schedule.take_due(sample), Due::default());
        assert_eq!(
            schedule.next_wakeup(sample),
            Some(wake + BATTERY_MOTION_START_DELAY)
        );

        // T+1 s: the battery alone.
        let t1 = wake + BATTERY_MOTION_START_DELAY;
        assert_eq!(
            schedule.take_due(t1),
            Due {
                poll: true,
                ..Due::default()
            }
        );
        schedule.on_polled(t1, PollOutcome::Answered);

        // T+2 s: the follow-up color and the first rest poll, together.
        let t2 = wake + WATCH_FOLLOW_UP_DELAY;
        assert_eq!(t2, wake + BATTERY_REST_DELAYS[0]);
        assert_eq!(schedule.next_wakeup(t1), Some(t2));
        assert_eq!(
            schedule.take_due(t2),
            Due {
                poll: true,
                reapply: FOLLOW_UP
            }
        );
        schedule.on_polled(t2, PollOutcome::Answered);
        assert_eq!(schedule.take_due(t2), Due::default());

        // T+8 s: the late rest poll, then the slow cadences.
        let t3 = wake + BATTERY_REST_DELAYS[1];
        assert_eq!(schedule.next_wakeup(t2), Some(t3));
        assert_eq!(
            schedule.take_due(t3),
            Due {
                poll: true,
                ..Due::default()
            }
        );
        schedule.on_polled(t3, PollOutcome::Answered);
        assert_eq!(schedule.next_wakeup(t3), Some(t0 + WATCH_SAFETY_INTERVAL));
    }

    /// The two minute cadences keep their own clocks: the safety tick counts
    /// from the start, the refresh from the last poll. Each is served at its
    /// time, and when they do coincide one turn takes both.
    #[test]
    fn daemon_serves_the_safety_tick_and_the_refresh_on_their_own_clocks() {
        let t0 = Instant::now();
        let mut schedule = Schedule::new(t0, true, true);
        schedule.take_due(t0);
        schedule.on_polled(t0, PollOutcome::Answered);

        // A short movement at T+1 s: no wake (the gap is under the
        // threshold), but input for the safety tick and polls at T+2, 3, 9.
        let moved = t0 + Duration::from_secs(1);
        schedule.on_input(moved);
        assert_eq!(schedule.take_due(moved), Due::default());
        let just_before = |at: Instant| at.checked_sub(Duration::from_millis(1)).unwrap();
        for delay in [
            BATTERY_MOTION_START_DELAY,
            BATTERY_REST_DELAYS[0],
            BATTERY_REST_DELAYS[1],
        ] {
            let at = moved + delay;
            assert_eq!(schedule.next_wakeup(just_before(at)), Some(at));
            assert_eq!(
                schedule.take_due(at),
                Due {
                    poll: true,
                    ..Due::default()
                }
            );
            schedule.on_polled(at, PollOutcome::Answered);
        }
        let last_poll = moved + BATTERY_REST_DELAYS[1];

        // T+60 s: the safety tick alone; T+69 s: the refresh alone.
        let tick = t0 + WATCH_SAFETY_INTERVAL;
        assert_eq!(schedule.next_wakeup(last_poll), Some(tick));
        assert_eq!(
            schedule.take_due(tick),
            Due {
                poll: false,
                reapply: SAFETY
            }
        );
        let refresh = last_poll + BATTERY_REFRESH_INTERVAL;
        assert_eq!(schedule.next_wakeup(tick), Some(refresh));
        assert_eq!(
            schedule.take_due(refresh),
            Due {
                poll: true,
                ..Due::default()
            }
        );

        // Made to coincide: a poll clocked on the previous tick, so that its
        // refresh lands on the next one, and input a second before — a wake,
        // taken at once, whose motion poll lands on the tick too. One turn,
        // both owed, then nothing.
        let tick2 = tick + WATCH_SAFETY_INTERVAL;
        schedule.on_polled(tick, PollOutcome::Answered);
        let woke = tick + Duration::from_secs(59);
        schedule.on_input(woke);
        assert!(schedule.take_due(woke).reapply.wake.is_some());
        assert_eq!(
            schedule.next_wakeup(woke),
            Some(woke + MOTION_SAMPLE_INTERVAL)
        );
        assert_eq!(schedule.next_wakeup(just_before(tick2)), Some(tick2));
        assert_eq!(
            schedule.take_due(tick2),
            Due {
                poll: true,
                reapply: SAFETY
            }
        );
        schedule.on_polled(tick2, PollOutcome::Answered);
        assert_eq!(schedule.take_due(tick2), Due::default());
    }

    #[test]
    fn daemon_samples_the_input_stream_instead_of_following_it() {
        let t0 = Instant::now();
        let mut schedule = Schedule::new(t0, false, true);
        schedule.take_due(t0);
        schedule.on_polled(t0, PollOutcome::Answered);
        schedule.on_input(t0);

        // Muted right after a sample: wake up to look again, not per report.
        assert!(!schedule.watches_input(t0));
        assert_eq!(schedule.next_wakeup(t0), Some(t0 + MOTION_SAMPLE_INTERVAL));
        let t1 = t0 + MOTION_SAMPLE_INTERVAL;
        assert!(schedule.watches_input(t1));
        assert_eq!(
            schedule.next_wakeup(t1),
            Some(t0 + BATTERY_MOTION_START_DELAY)
        );
    }

    /// `--watch`: the color alone, no poll ever; `--upower` without `--hold`:
    /// the battery alone, no re-apply ever — not even on a wake.
    #[test]
    fn each_half_of_the_daemon_runs_alone() {
        let t0 = Instant::now();
        let mut watch = Schedule::new(t0, true, false);
        assert_eq!(watch.take_due(t0), Due::default());
        assert_eq!(watch.next_wakeup(t0), Some(t0 + WATCH_SAFETY_INTERVAL));
        let wake = t0 + Duration::from_secs(10);
        watch.on_input(wake);
        assert_eq!(
            watch.take_due(wake),
            Due {
                poll: false,
                reapply: DueReapplies {
                    wake: Some(Duration::from_secs(10)),
                    ..DueReapplies::default()
                },
            }
        );
        // Later: the follow-up and the safety tick, still no poll.
        assert_eq!(
            watch.take_due(wake + Duration::from_secs(3600)),
            Due {
                poll: false,
                reapply: DueReapplies {
                    wake: None,
                    follow_up: true,
                    safety: true,
                },
            }
        );

        let mut bridge = Schedule::new(t0, false, true);
        assert_eq!(
            bridge.take_due(t0),
            Due {
                poll: true,
                ..Due::default()
            }
        );
        bridge.on_polled(t0, PollOutcome::Absent);
        // The same gap, which a held color would take as a wake; the retry
        // of the absent cadence is not due for another few seconds.
        let moved = t0 + Duration::from_secs(6);
        bridge.on_input(moved);
        assert_eq!(bridge.take_due(moved), Due::default());
        assert_eq!(
            bridge.take_due(moved + BATTERY_MOTION_START_DELAY),
            Due {
                poll: true,
                ..Due::default()
            }
        );
    }

    #[test]
    fn poll_outcome_depends_on_what_is_exposed() {
        assert_eq!(PollOutcome::of(true, true), PollOutcome::Answered);
        assert_eq!(PollOutcome::of(true, false), PollOutcome::Answered);
        assert_eq!(PollOutcome::of(false, true), PollOutcome::Unanswered);
        assert_eq!(PollOutcome::of(false, false), PollOutcome::Absent);
    }
}
