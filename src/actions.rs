//! The verb behind each CLI flag: one `run_*` function per action.

use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::cli::ColorName;
use crate::hid::{HidrawDevice, is_device_gone};
use crate::protocol::{
    BatteryStatus, DEFAULT_DPI_ACTIVE_STAGE, DEFAULT_DPI_STAGES, TX_ID_DOCK, TX_ID_MOUSE,
    dock_rgb_report, format_dpi, mouse_via_dock_rgb_report, query_battery, query_dpi,
    query_dpi_stages, query_firmware, query_profiles, query_serial, set_dpi, set_dpi_stages,
};
use crate::uhid::{self, BatteryDevice};

// `--watch` re-applies the color when the mouse wakes. The dock emits no
// dedicated wake event — it just resumes forwarding mouse-motion input reports
// the instant the mouse comes back. So we treat "input resumed after a quiet
// gap of at least this long" as a wake. It must sit above an ordinary pause in
// movement yet below the mouse's own sleep timeout (Razer's is much longer).
const WAKE_IDLE_THRESHOLD: Duration = Duration::from_secs(5);

// While the mouse is in use we also re-apply the color on this cadence, as a
// safety net against the firmware drifting back to its onboard default for no
// visible reason. We skip it entirely when the mouse has been idle, so a
// sleeping or absent mouse costs nothing.
const WATCH_SAFETY_INTERVAL: Duration = Duration::from_secs(60);

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
        dock.path.display()
    );
    match query_battery(dock) {
        Ok(_) => println!("✓ Razer Basilisk V3 Pro 35K (via Dock) responding over RF"),
        Err(_) => println!("⚠ Mouse not responding — is it paired and awake?"),
    }
    Ok(())
}

/// Push `color` to the dock ring and, via RF, to the mouse. Silent so it can be
/// called repeatedly by `--watch`.
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
    println!("Sniffing input reports from {}.", dock.path.display());
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

/// Hold `color` persistently: re-apply it the moment the mouse wakes (detected
/// as input resuming after a quiet gap) and, while the mouse is in use, on a
/// slow safety cadence to correct any spontaneous drift. Runs until the process
/// is signalled (Ctrl-C, or `systemctl stop`).
pub(crate) fn run_watch(dock: &HidrawDevice, color: ColorName) -> Result<()> {
    apply_color(dock, color).context("initial color apply failed")?;
    println!(
        "Watching {} — holding '{}', re-applying on wake. Ctrl-C to stop.",
        dock.path.display(),
        color.as_str()
    );

    let mut buf = [0u8; 256];
    let mut last_input = Instant::now();
    let mut active_since_safety = false;

    loop {
        if !dock.wait_for_input(Instant::now() + WATCH_SAFETY_INTERVAL)? {
            // Safety cadence elapsed. Re-apply only if the mouse has been used
            // since the last safety apply, so a sleeping/absent mouse is free.
            if active_since_safety {
                apply_color(dock, color).context("safety re-apply failed")?;
                active_since_safety = false;
                println!("re-applied '{}' (safety refresh)", color.as_str());
            }
            continue;
        }

        // Input is ready, so this read won't block. Handling one report per
        // iteration is fine — a burst just makes the next poll return at once.
        let n = dock.read_input_report(&mut buf)?;
        if n == 0 {
            continue;
        }
        let now = Instant::now();
        let idle_gap = now.duration_since(last_input);
        last_input = now;
        active_since_safety = true;

        if should_reapply_on_wake(idle_gap) {
            apply_color(dock, color).context("wake re-apply failed")?;
            println!(
                "re-applied '{}' (mouse woke after {:.0}s idle)",
                color.as_str(),
                idle_gap.as_secs_f64()
            );
        }
    }
}

/// One battery poll for a long-running loop: `None` when the mouse does not
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

/// When `--upower` should next query the battery: the slow refresh cadence,
/// plus polls placed around mouse movement (see `BATTERY_MOTION_START_DELAY`).
/// Pure bookkeeping over instants, so the policy is testable without hardware.
struct PollSchedule {
    refresh_at: Instant,
    // Pending "the mouse started moving" poll.
    motion_start_at: Option<Instant>,
    last_motion: Option<Instant>,
    // How many of `BATTERY_REST_DELAYS` were served since the last motion.
    rest_polls_done: usize,
    // Pending retry after an unanswered poll.
    retry_at: Option<Instant>,
    // The input stream is not looked at before this instant.
    input_muted_until: Instant,
}

impl PollSchedule {
    fn new(now: Instant) -> Self {
        Self {
            refresh_at: now + BATTERY_REFRESH_INTERVAL,
            motion_start_at: None,
            last_motion: None,
            rest_polls_done: 0,
            retry_at: None,
            input_muted_until: now,
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
        self.input_muted_until = now + MOTION_SAMPLE_INTERVAL;
    }

    /// The battery was queried at `now` (whatever triggered it); `answered`
    /// tells whether the mouse replied.
    fn on_polled(&mut self, now: Instant, answered: bool) {
        self.refresh_at = now + BATTERY_REFRESH_INTERVAL;
        self.retry_at = (!answered).then_some(now + BATTERY_UNANSWERED_RETRY);
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

    fn watches_input(&self, now: Instant) -> bool {
        now >= self.input_muted_until
    }

    /// When to stop waiting: the next poll, or sooner to look at the input
    /// stream again.
    fn next_wakeup(&self, now: Instant) -> Instant {
        if self.watches_input(now) {
            self.next_poll_at()
        } else {
            self.next_poll_at().min(self.input_muted_until)
        }
    }
}

/// Absent state: nothing is exposed until the mouse answers. Retry on a slow
/// cadence, or right away when the mouse moves — a sleeping mouse wakes that way.
fn wait_for_first_reading(dock: &HidrawDevice) -> Result<BatteryStatus> {
    loop {
        if let Some(status) = poll_battery(dock)? {
            return Ok(status);
        }
        if dock.wait_for_input(Instant::now() + BATTERY_ABSENT_RETRY)? {
            // Let the RF link settle — and never spin on a stream of motion.
            std::thread::sleep(BATTERY_MOTION_START_DELAY);
            dock.drain_input_reports()?;
        }
    }
}

/// Present state: mirror readings into `device` until the mouse has been
/// silent for `BATTERY_WITHDRAW_AFTER`.
fn mirror_until_silent(
    dock: &HidrawDevice,
    device: &mut BatteryDevice,
    first: BatteryStatus,
) -> Result<()> {
    let mut last = first;
    // When the current run of unanswered polls began.
    let mut silent_since: Option<Instant> = None;
    let mut schedule = PollSchedule::new(Instant::now());
    dock.drain_input_reports()?;

    loop {
        let now = Instant::now();
        let input = schedule.watches_input(now).then_some(dock.file.as_raw_fd());
        if device.serve_until(schedule.next_wakeup(now), input)? {
            dock.drain_input_reports()?;
            schedule.on_motion(Instant::now());
            continue;
        }
        if Instant::now() < schedule.next_poll_at() {
            continue; // woke only to look at the input stream again
        }

        let status = poll_battery(dock)?;
        let now = Instant::now();
        schedule.on_polled(now, status.is_some());
        if let Some(status) = status {
            // Pushed even when unchanged: each push past the kernel's 30 s
            // rate-limit re-announces the battery to UPower.
            device.update(status.percent, status.charging)?;
            silent_since = None;
            if status != last {
                println!("battery: {status}");
                last = status;
            }
        } else {
            let since = *silent_since.get_or_insert(now);
            if should_withdraw_battery(now.duration_since(since)) {
                return Ok(());
            }
            // A lost poll so far: keep the last reading, retry shortly.
        }
    }
}

/// Bridge the mouse battery to UPower: mirror it into a virtual HID device
/// whose battery the kernel registers as a `power_supply`. The battery exists
/// only while the mouse answers: it appears with a first real reading — never
/// a made-up level — and is withdrawn once the mouse has been silent for
/// `BATTERY_WITHDRAW_AFTER`. Runs until the process is signalled (or the dock
/// unplugged); the kernel removes the device when the process exits.
pub(crate) fn run_upower(dock: &HidrawDevice) -> Result<()> {
    // Fail on a missing uhid handle now, not after waiting on the mouse.
    let mut uhid = uhid::open()?;
    println!(
        "Bridging the mouse battery from {} to UPower — waiting for a first reading.",
        dock.path.display()
    );

    loop {
        let first = wait_for_first_reading(dock)?;
        let mut device = BatteryDevice::create(uhid, first.percent, first.charging)
            .context("cannot create the virtual battery device")?;
        println!("battery: {first}");

        mirror_until_silent(dock, &mut device, first)?;

        uhid = device
            .destroy()
            .context("cannot withdraw the virtual battery device")?;
        println!(
            "battery withdrawn — mouse silent for {} s (asleep, off or out of range)",
            BATTERY_WITHDRAW_AFTER.as_secs()
        );
    }
}

pub(crate) fn run_battery(dock: &HidrawDevice) -> Result<()> {
    println!("✓ Battery: {}", query_battery(dock)?);
    Ok(())
}

#[allow(clippy::unnecessary_wraps)] // see `run_check`
pub(crate) fn run_info(dock: &HidrawDevice) -> Result<()> {
    println!("Razer Mouse Dock Pro");
    println!("  Path:     {}", dock.path.display());
    print_field("Serial", query_serial(dock, TX_ID_DOCK).ok());
    print_field("Firmware", query_firmware(dock, TX_ID_DOCK).ok());

    println!();
    println!("Razer Basilisk V3 Pro 35K (via Dock)");
    println!("  Path:     {}", dock.path.display());

    // The serial doubles as a liveness probe: a sleeping mouse answers no RF
    // query, so each one would burn its full timeout. After a first miss,
    // report the remaining fields as absent without further round-trips.
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

    #[test]
    fn schedule_without_motion_is_the_slow_refresh() {
        let t0 = Instant::now();
        let mut schedule = PollSchedule::new(t0);
        assert_eq!(schedule.next_poll_at(), t0 + BATTERY_REFRESH_INTERVAL);
        assert!(schedule.watches_input(t0));

        let t1 = t0 + BATTERY_REFRESH_INTERVAL;
        schedule.on_polled(t1, true);
        assert_eq!(schedule.next_poll_at(), t1 + BATTERY_REFRESH_INTERVAL);
    }

    #[test]
    fn schedule_polls_after_motion_starts_then_twice_at_rest() {
        let t0 = Instant::now();
        let mut schedule = PollSchedule::new(t0);

        // Lifted off the dock: one poll shortly after the movement starts.
        schedule.on_motion(t0);
        assert_eq!(schedule.next_poll_at(), t0 + BATTERY_MOTION_START_DELAY);

        // Still moving half a second later: same movement, no new start poll,
        // and the rest polls slide with the last motion.
        let t1 = t0 + MOTION_SAMPLE_INTERVAL;
        schedule.on_motion(t1);
        assert_eq!(schedule.next_poll_at(), t0 + BATTERY_MOTION_START_DELAY);
        schedule.on_polled(t0 + BATTERY_MOTION_START_DELAY, true);
        assert_eq!(schedule.next_poll_at(), t1 + BATTERY_REST_DELAYS[0]);

        // Put down (docked): first rest poll, then the late confirmation…
        schedule.on_polled(t1 + BATTERY_REST_DELAYS[0], true);
        assert_eq!(schedule.next_poll_at(), t1 + BATTERY_REST_DELAYS[1]);
        // …then back to the slow refresh until it moves again.
        let t2 = t1 + BATTERY_REST_DELAYS[1];
        schedule.on_polled(t2, true);
        assert_eq!(schedule.next_poll_at(), t2 + BATTERY_REFRESH_INTERVAL);

        // A new movement after a real pause starts the cycle over.
        let t3 = t2 + MOTION_GAP;
        schedule.on_motion(t3);
        assert_eq!(schedule.next_poll_at(), t3 + BATTERY_MOTION_START_DELAY);
    }

    #[test]
    fn schedule_samples_the_input_stream_instead_of_following_it() {
        let t0 = Instant::now();
        let mut schedule = PollSchedule::new(t0);
        schedule.on_motion(t0);

        // Muted right after a sample: wake up to look again, not per report.
        assert!(!schedule.watches_input(t0));
        assert_eq!(schedule.next_wakeup(t0), t0 + MOTION_SAMPLE_INTERVAL);
        let t1 = t0 + MOTION_SAMPLE_INTERVAL;
        assert!(schedule.watches_input(t1));
        assert_eq!(schedule.next_wakeup(t1), schedule.next_poll_at());
    }

    #[test]
    fn a_late_poll_covers_every_rest_delay_already_elapsed() {
        let t0 = Instant::now();
        let mut schedule = PollSchedule::new(t0);
        schedule.on_motion(t0);
        // One poll long after the mouse came to rest: nothing left to confirm.
        let late = t0 + BATTERY_REST_DELAYS[1] + Duration::from_secs(1);
        schedule.on_polled(late, true);
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
    fn schedule_retries_quickly_while_the_mouse_is_silent() {
        let t0 = Instant::now();
        let mut schedule = PollSchedule::new(t0);

        // An unanswered poll is retried shortly, not a refresh interval later…
        let t1 = t0 + BATTERY_REFRESH_INTERVAL;
        schedule.on_polled(t1, false);
        assert_eq!(schedule.next_poll_at(), t1 + BATTERY_UNANSWERED_RETRY);
        let t2 = t1 + BATTERY_UNANSWERED_RETRY;
        schedule.on_polled(t2, false);
        assert_eq!(schedule.next_poll_at(), t2 + BATTERY_UNANSWERED_RETRY);

        // …and an answer drops back to the slow cadence.
        let t3 = t2 + BATTERY_UNANSWERED_RETRY;
        schedule.on_polled(t3, true);
        assert_eq!(schedule.next_poll_at(), t3 + BATTERY_REFRESH_INTERVAL);
    }

    #[test]
    fn watch_reapplies_only_after_a_real_idle_gap() {
        // Brief pauses in movement must not re-apply.
        assert!(!should_reapply_on_wake(Duration::from_millis(500)));
        assert!(!should_reapply_on_wake(
            WAKE_IDLE_THRESHOLD.saturating_sub(Duration::from_millis(1))
        ));
        // A gap at/above the threshold means the mouse slept → re-apply.
        assert!(should_reapply_on_wake(WAKE_IDLE_THRESHOLD));
        assert!(should_reapply_on_wake(Duration::from_secs(120)));
    }
}
