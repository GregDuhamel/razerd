//! Minimal RGB daemon for the Razer Mouse Dock Pro and a wirelessly connected
//! Razer Basilisk V3 Pro 35K.
//!
//! All commands go through a single hidraw interface on the dock. The dock
//! firmware routes requests to itself or forwards them to the wireless mouse
//! over the RF link based on the transaction id and report layout.
//!
//! Layout: [`hid`] is the dock's side of the hidraw transport (which node is
//! the dock, the send/poll exchange) over the shared `hidraw` crate,
//! [`protocol`] the Razer report format and typed queries, [`cli`] the flag
//! surface, and [`actions`] the verb behind each flag — among them the daemon
//! loop of `--upower`, the one process meant to own the dock. [`uhid`] is the
//! one piece that does not talk to the dock: the virtual HID device `--upower`
//! uses to hand the mouse battery to the kernel, over the shared
//! `uhid-battery` crate.
//!
//! Two kinds of output: a one-shot command prints its result on stdout
//! (`println!`, what scripts read), the daemon reports through [`log`] on
//! stderr — the journal, under systemd — and prints nothing.

mod actions;
mod cli;
mod hid;
mod protocol;
mod uhid;

use std::io::Write;
use std::process::ExitCode;

use log::{Level, LevelFilter, error};

use cli::{Action, Cli};
use hid::HidrawDevice;

fn main() -> ExitCode {
    let cli = Cli::try_parse_checked(std::env::args_os()).unwrap_or_else(|err| err.exit());
    init_logging(cli.verbose());
    let action = cli.action();
    let daemon = action.is_daemon();

    // An absent dock is an error here, for every action: the daemon does not
    // wait for it either — udev starts razerd.service when the dock appears
    // (contrib/70-razerd.rules), and `systemctl status` shows this message.
    let result = HidrawDevice::open_dock().and_then(|dock| match action {
        Action::Check => actions::run_check(&dock),
        Action::Color(c) => actions::run_color(&dock, c),
        Action::Battery => actions::run_battery(&dock),
        Action::Info => actions::run_info(&dock),
        Action::Sniff => actions::run_sniff(&dock),
        Action::Watch(c) => actions::run_watch(&dock, c),
        Action::Upower { hold } => actions::run_upower(&dock, hold),
        Action::Sensitivity(d) => actions::run_sensitivity(&dock, d),
        Action::SensitivityStages(on) => actions::run_sensitivity_stages(&dock, on),
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        // The daemon's last word goes to the journal at its priority, where
        // `journalctl -p err` finds it.
        Err(err) if daemon => {
            error!("{err:#}");
            ExitCode::FAILURE
        }
        // A command's: the line `main() -> Result` would have printed.
        Err(err) => {
            eprintln!("Error: {err:?}");
            ExitCode::FAILURE
        }
    }
}

/// Sets logging up: info by default, what `RUST_LOG` says otherwise, and
/// what `-v` says above all (debug, `-vv` trace, for this crate). Under
/// systemd, whose journal timestamps every line already, the lines carry its
/// priority prefix instead.
fn init_logging(verbose: u8) {
    let mut builder = env_logger::Builder::new();
    builder.filter_level(LevelFilter::Info);
    match verbose {
        0 => {
            builder.parse_default_env();
        }
        1 => {
            builder.filter_module(env!("CARGO_CRATE_NAME"), LevelFilter::Debug);
        }
        _ => {
            builder.filter_module(env!("CARGO_CRATE_NAME"), LevelFilter::Trace);
        }
    }
    // systemd sets JOURNAL_STREAM when stderr is the journal.
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        builder.format(|out, record| {
            writeln!(
                out,
                "<{}>{}",
                journal_priority(record.level()),
                record.args()
            )
        });
    }
    builder.init();
}

/// The `<N>` journald reads a priority from at the start of a stderr line,
/// as `sd-daemon(3)` numbers them.
const fn journal_priority(level: Level) -> u8 {
    match level {
        Level::Error => 3,
        Level::Warn => 4,
        Level::Info => 6,
        Level::Debug | Level::Trace => 7,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `journalctl -p warning` keeps: error and warn, nothing from info
    /// down. Debug and trace share the journal's lowest priority.
    #[test]
    fn journal_priorities_follow_sd_daemon() {
        assert_eq!(journal_priority(Level::Error), 3);
        assert_eq!(journal_priority(Level::Warn), 4);
        assert_eq!(journal_priority(Level::Info), 6);
        assert_eq!(journal_priority(Level::Debug), 7);
        assert_eq!(journal_priority(Level::Trace), 7);
    }
}
