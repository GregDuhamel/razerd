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

mod actions;
mod cli;
mod hid;
mod protocol;
mod uhid;

use anyhow::Result;

use cli::{Action, Cli};
use hid::HidrawDevice;

fn main() -> Result<()> {
    let cli = Cli::try_parse_checked(std::env::args_os()).unwrap_or_else(|err| err.exit());
    let action = cli.action();
    // An absent dock is an error here, for every action: the daemon does not
    // wait for it either — udev starts razerd.service when the dock appears
    // (contrib/70-razerd.rules), and `systemctl status` shows this message.
    let dock = HidrawDevice::open_dock()?;

    match action {
        Action::Check => actions::run_check(&dock),
        Action::Color(c) => actions::run_color(&dock, c),
        Action::Battery => actions::run_battery(&dock),
        Action::Info => actions::run_info(&dock),
        Action::Sniff => actions::run_sniff(&dock),
        Action::Watch(c) => actions::run_watch(&dock, c),
        Action::Upower { hold } => actions::run_upower(&dock, hold),
        Action::Sensitivity(d) => actions::run_sensitivity(&dock, d),
        Action::SensitivityStages(on) => actions::run_sensitivity_stages(&dock, on),
    }
}
