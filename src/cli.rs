//! Command-line surface: one mutually-exclusive action per invocation.

use std::ffi::OsString;

use clap::error::ErrorKind;
use clap::{CommandFactory, Parser, ValueEnum};

use crate::protocol::{DPI_MAX, DPI_MIN, Rgb};

// Every flag belongs to the `action` group: clap enforces "exactly one of
// these" (required + non-multiple) so the struct needs no pairwise conflict
// lists and `action()` needs no arity checks.
#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about,
    after_help = "A command prints its result on stdout. The daemon (--upower, --watch) \
                  reports on stderr through the log: info for its transitions (start, \
                  battery exposed or withdrawn, stop), warn for a transient error it \
                  retries; -v adds every battery poll and color re-apply (debug), -vv the \
                  trace level. RUST_LOG=debug (or RUST_LOG=razerd=trace) does the same, \
                  -v taking precedence over it. Under systemd the lines carry journald \
                  priorities, so `journalctl -p warning -u razerd` shows the warnings alone."
)]
#[command(group = clap::ArgGroup::new("action").required(true).multiple(false))]
#[allow(clippy::struct_excessive_bools)] // one bool per valueless flag — clap's model
pub(crate) struct Cli {
    /// Verify the dock is detected and accessible.
    #[arg(long, group = "action")]
    check: bool,

    /// Apply a color to the dock and the wireless mouse.
    #[arg(long, value_enum, group = "action")]
    color: Option<ColorName>,

    /// Print the mouse battery level and charging status.
    #[arg(long, group = "action")]
    battery: bool,

    /// Print a full device report: serial, firmware, battery, DPI, stages
    /// lock state, onboard profile, and the battery exposed by --upower.
    #[arg(long, group = "action")]
    info: bool,

    /// Dump timestamped HID input reports from the dock (diagnostic).
    #[arg(long, group = "action")]
    sniff: bool,

    /// Hold a color, re-applying it whenever the mouse wakes, without the
    /// UPower bridge (runs until stopped; the daemon is --upower --hold).
    #[arg(long, value_enum, value_name = "COLOR", group = "action")]
    watch: Option<ColorName>,

    /// Run the daemon: expose the mouse battery to UPower (KDE/GNOME power
    /// applets) through a virtual HID device, and hold a color with --hold
    /// (runs until stopped; see razerd.service).
    #[arg(long, group = "action")]
    upower: bool,

    /// With --upower: also hold this color, re-applying it whenever the mouse
    /// wakes — what --watch does, in the same process as the battery bridge.
    #[arg(long, value_enum, value_name = "COLOR")]
    hold: Option<ColorName>,

    /// Set the sensitivity to one fixed DPI value (the free slider): collapses
    /// the onboard stage table to it, so the Cycle Up Sensitivity Stages
    /// button can't change it.
    #[arg(long, visible_alias = "dpi", value_parser = parse_dpi, value_name = "DPI", group = "action")]
    sensitivity: Option<u16>,

    /// on: install the default 5-stage table (400/800/1600/3200/6400) and
    /// enable the Cycle Up Sensitivity Stages button; off: freeze the current
    /// DPI and disable the button.
    #[arg(long, value_parser = clap::builder::BoolishValueParser::new(), value_name = "on|off", group = "action")]
    sensitivity_stages: Option<bool>,

    /// Log more: -v for debug (every battery poll and color re-apply), -vv
    /// for trace. Not an action: it goes with --upower or --watch.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,
}

pub(crate) enum Action {
    Check,
    Color(ColorName),
    Battery,
    Info,
    Sniff,
    Watch(ColorName),
    Upower { hold: Option<ColorName> },
    Sensitivity(u16),
    SensitivityStages(bool),
}

impl Action {
    /// Runs until stopped, and reports through the log rather than stdout:
    /// its fatal error goes to the journal too.
    pub(crate) const fn is_daemon(&self) -> bool {
        matches!(self, Self::Watch(_) | Self::Upower { .. })
    }
}

impl Cli {
    /// Parse `args`, with the one rule the groups cannot express: `--hold`
    /// goes with `--upower`. A `requires = "upower"` on the flag would let
    /// `--watch red --hold blue` through — clap excuses a missing required
    /// flag when it conflicts with a flag that is present — so it is checked
    /// here, as a clap error like the others.
    pub(crate) fn try_parse_checked<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let cli = Self::try_parse_from(args)?;
        if cli.hold.is_some() && !cli.upower {
            return Err(Self::command().error(
                ErrorKind::MissingRequiredArgument,
                "'--hold <COLOR>' goes with '--upower'",
            ));
        }
        Ok(cli)
    }

    pub(crate) fn action(&self) -> Action {
        [
            self.check.then_some(Action::Check),
            self.color.map(Action::Color),
            self.battery.then_some(Action::Battery),
            self.info.then_some(Action::Info),
            self.sniff.then_some(Action::Sniff),
            self.watch.map(Action::Watch),
            self.upower.then_some(Action::Upower { hold: self.hold }),
            self.sensitivity.map(Action::Sensitivity),
            self.sensitivity_stages.map(Action::SensitivityStages),
        ]
        .into_iter()
        .flatten()
        .next()
        .expect("clap's `action` group guarantees exactly one flag is set")
    }

    /// How many `-v` were given.
    pub(crate) const fn verbose(&self) -> u8 {
        self.verbose
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ColorName {
    Red,
    Green,
    Blue,
    White,
    Off,
}

impl ColorName {
    pub(crate) const fn rgb(self) -> Rgb {
        match self {
            Self::Red => Rgb::new(0xC0, 0x00, 0x00),
            Self::Green => Rgb::new(0x00, 0xC0, 0x00),
            Self::Blue => Rgb::new(0x00, 0x00, 0xC0),
            Self::White => Rgb::new(0xFF, 0xFF, 0xFF),
            Self::Off => Rgb::new(0x00, 0x00, 0x00),
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Red => "red",
            Self::Green => "green",
            Self::Blue => "blue",
            Self::White => "white",
            Self::Off => "off",
        }
    }
}

fn parse_dpi(s: &str) -> Result<u16, String> {
    let n: u16 = s.parse().map_err(|_| format!("'{s}' is not a DPI value"))?;
    if !(DPI_MIN..=DPI_MAX).contains(&n) {
        return Err(format!("{n} is out of range ({DPI_MIN}-{DPI_MAX})"));
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_name_rgb_table() {
        assert_eq!(ColorName::Red.rgb(), Rgb::new(0xC0, 0x00, 0x00));
        assert_eq!(ColorName::Green.rgb(), Rgb::new(0x00, 0xC0, 0x00));
        assert_eq!(ColorName::Blue.rgb(), Rgb::new(0x00, 0x00, 0xC0));
        assert_eq!(ColorName::White.rgb(), Rgb::new(0xFF, 0xFF, 0xFF));
        assert_eq!(ColorName::Off.rgb(), Rgb::new(0x00, 0x00, 0x00));
    }

    #[test]
    fn parse_dpi_accepts_in_range_values() {
        assert_eq!(parse_dpi("1800").unwrap(), 1800);
        assert_eq!(parse_dpi("100").unwrap(), 100);
        assert_eq!(parse_dpi("35000").unwrap(), 35000);
    }

    #[test]
    fn parse_dpi_rejects_out_of_range_and_garbage() {
        assert!(parse_dpi("99").is_err());
        assert!(parse_dpi("35001").is_err());
        assert!(parse_dpi("0").is_err());
        assert!(parse_dpi("fast").is_err());
        assert!(parse_dpi("-100").is_err());
        assert!(parse_dpi("1600x800").is_err());
    }

    #[test]
    fn cli_requires_exactly_one_action_flag() {
        // Zero flags, two booleans, two valued flags: all rejected by clap.
        assert!(Cli::try_parse_from(["razerd"]).is_err());
        assert!(Cli::try_parse_from(["razerd", "--check", "--battery"]).is_err());
        assert!(
            Cli::try_parse_from([
                "razerd",
                "--sensitivity",
                "1800",
                "--sensitivity-stages",
                "on"
            ])
            .is_err()
        );
    }

    /// `--hold` is an option of the daemon, not an action of its own: alone
    /// there is nothing to hold the color in, and `--watch` already holds one.
    #[test]
    fn hold_goes_with_upower_only() {
        let parse = |args: &[&str]| Cli::try_parse_checked(args);
        assert!(parse(&["razerd", "--hold", "blue"]).is_err());
        assert!(parse(&["razerd", "--watch", "red", "--hold", "blue"]).is_err());
        assert!(parse(&["razerd", "--check", "--hold", "blue"]).is_err());
        assert!(parse(&["razerd", "--upower", "--hold", "blue"]).is_ok());
        assert!(parse(&["razerd", "--upower"]).is_ok());
        // The other rules still come from clap.
        assert!(parse(&["razerd", "--upower", "--watch", "red"]).is_err());
    }

    #[test]
    fn cli_maps_flags_to_actions() {
        let action = |args: &[&str]| Cli::try_parse_checked(args).unwrap().action();
        assert!(matches!(action(&["razerd", "--check"]), Action::Check));
        assert!(matches!(
            action(&["razerd", "--color", "blue"]),
            Action::Color(ColorName::Blue)
        ));
        assert!(matches!(
            action(&["razerd", "--upower"]),
            Action::Upower { hold: None }
        ));
        assert!(matches!(
            action(&["razerd", "--upower", "--hold", "blue"]),
            Action::Upower {
                hold: Some(ColorName::Blue)
            }
        ));
        assert!(matches!(
            action(&["razerd", "--sensitivity", "1800"]),
            Action::Sensitivity(1800)
        ));
        // --dpi is a visible alias of --sensitivity.
        assert!(matches!(
            action(&["razerd", "--dpi", "1800"]),
            Action::Sensitivity(1800)
        ));
        assert!(matches!(
            action(&["razerd", "--sensitivity-stages", "on"]),
            Action::SensitivityStages(true)
        ));
        // BoolishValueParser also takes off/false/no, case-insensitively.
        assert!(matches!(
            action(&["razerd", "--sensitivity-stages", "OFF"]),
            Action::SensitivityStages(false)
        ));
    }

    /// `-v` counts, cumulates, and is no action: alone it is still "no
    /// action given".
    #[test]
    fn verbose_counts_and_is_not_an_action() {
        let verbose = |args: &[&str]| Cli::try_parse_checked(args).unwrap().verbose();
        assert_eq!(verbose(&["razerd", "--check"]), 0);
        assert_eq!(verbose(&["razerd", "--upower", "-v"]), 1);
        assert_eq!(verbose(&["razerd", "-vv", "--watch", "blue"]), 2);
        assert_eq!(verbose(&["razerd", "--upower", "-v", "-v"]), 2);
        assert_eq!(verbose(&["razerd", "--upower", "--verbose", "-vv"]), 3);
        assert!(Cli::try_parse_checked(["razerd", "-vv"]).is_err());
    }

    /// The two long-running actions log; every other prints.
    #[test]
    fn only_the_long_running_actions_are_daemons() {
        let action = |args: &[&str]| Cli::try_parse_checked(args).unwrap().action();
        assert!(action(&["razerd", "--upower"]).is_daemon());
        assert!(action(&["razerd", "--upower", "--hold", "red"]).is_daemon());
        assert!(action(&["razerd", "--watch", "blue"]).is_daemon());
        for args in [
            ["razerd", "--check"],
            ["razerd", "--battery"],
            ["razerd", "--info"],
            ["razerd", "--sniff"],
        ] {
            assert!(!action(&args).is_daemon(), "{args:?}");
        }
        assert!(!action(&["razerd", "--color", "off"]).is_daemon());
        assert!(!action(&["razerd", "--dpi", "1800"]).is_daemon());
    }
}
