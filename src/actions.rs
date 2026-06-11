//! The verb behind each CLI flag: one `run_*` function per action.

use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::cli::ColorName;
use crate::hid::HidrawDevice;
use crate::protocol::{
    DEFAULT_DPI_ACTIVE_STAGE, DEFAULT_DPI_STAGES, TX_ID_DOCK, TX_ID_MOUSE, dock_rgb_report,
    format_dpi, mouse_via_dock_rgb_report, query_battery, query_dpi, query_dpi_stages,
    query_firmware, query_profiles, query_serial, set_dpi, set_dpi_stages,
};

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

    let mut pfd = libc::pollfd {
        fd: dock.file.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = WATCH_SAFETY_INTERVAL.as_millis() as libc::c_int;

    let mut buf = [0u8; 256];
    let mut last_input = Instant::now();
    let mut active_since_safety = false;

    loop {
        // SAFETY: one valid pollfd over an owned fd; the kernel only writes
        // `revents`. A negative return means error, with errno set.
        let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue; // EINTR (benign signal) — just retry the poll
            }
            bail!("poll on {} failed: {err}", dock.path.display());
        }

        if ret == 0 {
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

pub(crate) fn run_battery(dock: &HidrawDevice) -> Result<()> {
    let status = query_battery(dock)?;
    let suffix = if status.charging { " (charging)" } else { "" };
    println!("✓ Battery: {}%{}", status.percent, suffix);
    Ok(())
}

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
    fn watch_reapplies_only_after_a_real_idle_gap() {
        // Brief pauses in movement must not re-apply.
        assert!(!should_reapply_on_wake(Duration::from_millis(500)));
        assert!(!should_reapply_on_wake(
            WAKE_IDLE_THRESHOLD - Duration::from_millis(1)
        ));
        // A gap at/above the threshold means the mouse slept → re-apply.
        assert!(should_reapply_on_wake(WAKE_IDLE_THRESHOLD));
        assert!(should_reapply_on_wake(Duration::from_secs(120)));
    }
}
