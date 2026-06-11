//! Command-line surface: one mutually-exclusive action per invocation.

use clap::{Parser, ValueEnum};

use crate::protocol::{DPI_MAX, DPI_MIN, Rgb};

// Every flag belongs to the `action` group: clap enforces "exactly one of
// these" (required + non-multiple) so the struct needs no pairwise conflict
// lists and `action()` needs no arity checks.
#[derive(Debug, Parser)]
#[command(author, version, about)]
#[command(group = clap::ArgGroup::new("action").required(true).multiple(false))]
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
    /// lock state, onboard profile.
    #[arg(long, group = "action")]
    info: bool,

    /// Dump timestamped HID input reports from the dock (diagnostic).
    #[arg(long, group = "action")]
    sniff: bool,

    /// Hold a color, re-applying it whenever the mouse wakes (runs until stopped).
    #[arg(long, value_enum, value_name = "COLOR", group = "action")]
    watch: Option<ColorName>,

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
}

pub(crate) enum Action {
    Check,
    Color(ColorName),
    Battery,
    Info,
    Sniff,
    Watch(ColorName),
    Sensitivity(u16),
    SensitivityStages(bool),
}

impl Cli {
    pub(crate) fn action(&self) -> Action {
        [
            self.check.then_some(Action::Check),
            self.color.map(Action::Color),
            self.battery.then_some(Action::Battery),
            self.info.then_some(Action::Info),
            self.sniff.then_some(Action::Sniff),
            self.watch.map(Action::Watch),
            self.sensitivity.map(Action::Sensitivity),
            self.sensitivity_stages.map(Action::SensitivityStages),
        ]
        .into_iter()
        .flatten()
        .next()
        .expect("clap's `action` group guarantees exactly one flag is set")
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
    pub(crate) fn rgb(self) -> Rgb {
        match self {
            Self::Red => Rgb::new(0xC0, 0x00, 0x00),
            Self::Green => Rgb::new(0x00, 0xC0, 0x00),
            Self::Blue => Rgb::new(0x00, 0x00, 0xC0),
            Self::White => Rgb::new(0xFF, 0xFF, 0xFF),
            Self::Off => Rgb::new(0x00, 0x00, 0x00),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
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

    #[test]
    fn cli_maps_flags_to_actions() {
        let action = |args: &[&str]| Cli::try_parse_from(args).unwrap().action();
        assert!(matches!(action(&["razerd", "--check"]), Action::Check));
        assert!(matches!(
            action(&["razerd", "--color", "blue"]),
            Action::Color(ColorName::Blue)
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
}
