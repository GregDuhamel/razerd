//! The razerd side of the virtual HID battery behind `--upower`.
//!
//! The mechanism — a uhid device whose descriptor declares a battery, which
//! makes `hid-input` register a `power_supply` that UPower and the desktop's
//! power applet pick up — lives in the [`uhid_battery`] crate. What stays here
//! is what is specific to razerd: how the device presents itself, where the
//! `/dev/uhid` handle comes from, and reading the exposed battery back for
//! `--info`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use uhid_battery::{DEV_UHID, Handle, Identity};

pub(crate) use uhid_battery::{Battery, Kind};

// Name of the descriptor in the unit: `OpenFile=/dev/uhid:uhid`.
const INHERITED_FD_NAME: &str = "uhid";

const RAZER_VENDOR_ID: u32 = 0x1532;
// The Basilisk V3 Pro 35K's own (wired) product id.
const BASILISK_V3_PRO_35K_PRODUCT_ID: u32 = 0x00CC;

// Names the power supply (`hid-razerd-battery*`). A constant rather than the
// mouse serial: no string that came from the dock is ever handed to the kernel.
const DEVICE_UNIQ: &str = "razerd";

/// How the virtual device presents itself. All constants: the only values that
/// ever reach the kernel from the dock are a percentage and a charging bit.
pub(crate) fn identity() -> Identity {
    Identity {
        name: "Razer Basilisk V3 Pro 35K".into(),
        phys: "razerd".into(),
        uniq: DEVICE_UNIQ.into(),
        vendor: RAZER_VENDOR_ID,
        product: BASILISK_V3_PRO_35K_PRODUCT_ID,
    }
}

/// Get a handle on `/dev/uhid`.
///
/// Under systemd the node stays `root:root 0600` and the service manager hands
/// us the descriptor (`OpenFile=/dev/uhid:uhid` in the unit), so the process
/// itself needs no privilege. Without that — a root shell, for debugging —
/// open the node directly.
pub(crate) fn open() -> Result<Handle> {
    if let Some(handle) = Handle::inherited(INHERITED_FD_NAME).pop() {
        return Ok(handle);
    }
    Handle::open(DEV_UHID).with_context(|| {
        format!("cannot open {DEV_UHID} — run via razerd-battery.service (or as root)")
    })
}

/// The battery the kernel registered for our virtual device, read back from
/// sysfs — the very view UPower has of it.
pub(crate) struct ExposedBattery {
    pub(crate) path: PathBuf,
    pub(crate) percent: Option<String>,
    pub(crate) status: Option<String>,
}

/// Look up the power supply behind a running `--upower`, if any. For `--info`,
/// a separate process: the bridge itself must never read these attributes —
/// the kernel may answer a read by querying the bridge (`GET_REPORT`), which
/// would then be waiting on itself.
pub(crate) fn exposed_battery() -> Option<ExposedBattery> {
    uhid_battery::find_power_supply(DEVICE_UNIQ).map(|path| read_exposed_battery(&path))
}

fn read_exposed_battery(path: &Path) -> ExposedBattery {
    let attribute = |name: &str| {
        std::fs::read_to_string(path.join(name))
            .ok()
            .map(|value| value.trim().to_owned())
    };
    ExposedBattery {
        percent: attribute("capacity"),
        status: attribute("status"),
        path: path.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The identity is what keeps the power supply named `hid-razerd-battery*`
    /// across versions (UPower keys its history on it) — and nothing in it may
    /// come from the dock.
    #[test]
    fn identity_is_constant_and_keeps_the_power_supply_name() {
        let id = identity();
        assert_eq!(id.name, "Razer Basilisk V3 Pro 35K");
        assert_eq!(id.uniq, "razerd");
        assert_eq!((id.vendor, id.product), (0x1532, 0x00CC));
    }

    /// `[report id 2, strength, charging]` is what kernels in the field have
    /// been fed since 0.10.0; the crate must keep serving the same device.
    #[test]
    fn mouse_kind_keeps_the_battery_report_id() {
        assert_eq!(Kind::Mouse.report_id(), 2);
    }

    #[test]
    fn exposed_battery_attributes_are_read_and_trimmed() {
        let dir = std::env::temp_dir().join(format!(
            "razerd-test-exposed-battery-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("capacity"), "56\n").unwrap();
        std::fs::write(dir.join("status"), "Discharging\n").unwrap();

        let battery = read_exposed_battery(&dir);
        assert_eq!(battery.path, dir);
        assert_eq!(battery.percent.as_deref(), Some("56"));
        assert_eq!(battery.status.as_deref(), Some("Discharging"));

        // A vanished attribute (battery withdrawn mid-read) is just absent.
        std::fs::remove_file(dir.join("status")).unwrap();
        assert_eq!(read_exposed_battery(&dir).status, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
