//! The one-shot commands: one `run_*` function per flag, each printing its
//! result on stdout for the person or script that ran it. The daemon behind
//! `--upower` and `--watch` is [`crate::daemon`].

use std::io::Write as _;
use std::time::Instant;

use anyhow::{Context, Result};

use crate::cli::ColorName;
use crate::hid::HidrawDevice;
use crate::protocol::{
    DEFAULT_DPI_ACTIVE_STAGE, DEFAULT_DPI_STAGES, TX_ID_DOCK, TX_ID_MOUSE, dock_rgb_report,
    format_dpi, mouse_via_dock_rgb_report, query_battery, query_dpi, query_dpi_stages,
    query_firmware, query_profiles, query_serial, set_dpi, set_dpi_stages,
};
use crate::uhid;

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
pub(crate) fn apply_color(dock: &HidrawDevice, color: ColorName) -> Result<()> {
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

    // Written with `writeln!` rather than `println!`: a reader that goes away
    // (`razerd --sniff | head`) closes the pipe, and `println!` would panic on
    // the broken pipe where ending quietly is what the user meant.
    let mut out = std::io::stdout().lock();
    let start = Instant::now();
    let mut buf = [0u8; 256];
    loop {
        let n = dock.read_input_report(&mut buf)?;
        if n == 0 {
            continue;
        }
        let elapsed = start.elapsed().as_secs_f64();
        let hex: Vec<String> = buf[..n].iter().map(|b| format!("{b:02x}")).collect();
        if let Err(err) = writeln!(out, "[{elapsed:8.3}s] {n:3} bytes: {}", hex.join(" ")) {
            if err.kind() == std::io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(err).context("writing to stdout");
        }
    }
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
