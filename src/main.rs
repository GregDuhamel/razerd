//! Minimal RGB daemon for the Razer Mouse Dock Pro and a wirelessly connected
//! Razer Basilisk V3 Pro 35K.
//!
//! All commands go through a single hidraw interface on the dock. The dock
//! firmware routes requests to itself or forwards them to the wireless mouse
//! over the RF link based on the transaction id and report layout.
//!
//! Layout: [`hid`] is the hidraw transport (discovery, ioctls, send/poll
//! exchange), [`protocol`] the Razer report format and typed queries,
//! [`cli`] the flag surface, and [`actions`] the verb behind each flag.

mod actions;
mod cli;
mod hid;
mod protocol;

use anyhow::Result;
use clap::Parser;

use cli::{Action, Cli};
use hid::HidrawDevice;

fn main() -> Result<()> {
    let cli = Cli::parse();
    let action = cli.action();
    let dock = HidrawDevice::open_dock()?;

    match action {
        Action::Check => actions::run_check(&dock),
        Action::Color(c) => actions::run_color(&dock, c),
        Action::Battery => actions::run_battery(&dock),
        Action::Info => actions::run_info(&dock),
        Action::Sniff => actions::run_sniff(&dock),
        Action::Watch(c) => actions::run_watch(&dock, c),
        Action::Sensitivity(d) => actions::run_sensitivity(&dock, d),
        Action::SensitivityStages(on) => actions::run_sensitivity_stages(&dock, on),
    }
}
