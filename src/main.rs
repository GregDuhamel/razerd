use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

const RAZER_VENDOR_ID: u16 = 0x1532;
const MOUSE_DOCK_PRO_PRODUCT_ID: u16 = 0x00A4;
const BASILISK_V3_PRO_35K_WIRED_PRODUCT_ID: u16 = 0x00CC;
const BASILISK_V3_PRO_35K_WIRELESS_PRODUCT_ID: u16 = 0x00CD;
const REPORT_LEN: usize = 90;

// HIDIOCSFEATURE(len) = _IOC(_IOC_WRITE|_IOC_READ, 'H', 0x06, len)
fn hidiocsfeature(len: usize) -> u64 {
    (3u64 << 30) | (b'H' as u64) << 8 | 0x06 | ((len as u64) << 16)
}

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    #[arg(long, conflicts_with = "color")]
    check: bool,

    #[arg(long, value_enum, conflicts_with = "check")]
    color: Option<ColorName>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ColorName {
    Red,
    Green,
    Blue,
    White,
    Off,
}

impl ColorName {
    fn rgb(self) -> Rgb {
        match self {
            Self::Red => Rgb::new(0xC0, 0x00, 0x00),
            Self::Green => Rgb::new(0x00, 0xC0, 0x00),
            Self::Blue => Rgb::new(0x00, 0x00, 0xC0),
            Self::White => Rgb::new(0xFF, 0xFF, 0xFF),
            Self::Off => Rgb::new(0x00, 0x00, 0x00),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Red => "red",
            Self::Green => "green",
            Self::Blue => "blue",
            Self::White => "white",
            Self::Off => "off",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Rgb {
    red: u8,
    green: u8,
    blue: u8,
}

impl Rgb {
    const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }
}

#[derive(Debug, Clone, Copy)]
struct DeviceSpec {
    vendor_id: u16,
    product_id: u16,
    interface: u8,
    name: &'static str,
}

impl DeviceSpec {
    const fn mouse_dock_pro() -> Self {
        Self {
            vendor_id: RAZER_VENDOR_ID,
            product_id: MOUSE_DOCK_PRO_PRODUCT_ID,
            interface: 0,
            name: "Razer Mouse Dock Pro",
        }
    }

    const fn basilisk_v3_pro_35k_wired() -> Self {
        Self {
            vendor_id: RAZER_VENDOR_ID,
            product_id: BASILISK_V3_PRO_35K_WIRED_PRODUCT_ID,
            interface: 0,
            name: "Razer Basilisk V3 Pro 35K (Wired)",
        }
    }

    const fn basilisk_v3_pro_35k_wireless() -> Self {
        Self {
            vendor_id: RAZER_VENDOR_ID,
            product_id: BASILISK_V3_PRO_35K_WIRELESS_PRODUCT_ID,
            interface: 0,
            name: "Razer Basilisk V3 Pro 35K (Wireless)",
        }
    }

}

struct DeviceController {
    spec: DeviceSpec,
    build_report: fn(Rgb) -> [u8; REPORT_LEN],
}

impl DeviceController {
    fn new(spec: DeviceSpec, build_report: fn(Rgb) -> [u8; REPORT_LEN]) -> Self {
        Self { spec, build_report }
    }

    fn check(&self) -> Result<PathBuf> {
        let path = find_hidraw(self.spec.vendor_id, self.spec.product_id, self.spec.interface)?;
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| {
                format!(
                    "found {} at {} but cannot open — check udev permissions",
                    self.spec.name,
                    path.display()
                )
            })?;
        Ok(path)
    }

    fn set_color(&self, color: Rgb) -> Result<()> {
        let path = find_hidraw(self.spec.vendor_id, self.spec.product_id, self.spec.interface)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("cannot open {} — check udev permissions", path.display()))?;

        let report = (self.build_report)(color);

        // HIDIOCSFEATURE expects [report_id=0x00, ...report data (90 bytes)...] = 91 bytes total.
        // The kernel strips report_id and issues USB SET_REPORT(feature, id=0) with our 90 bytes.
        let mut buf = [0u8; REPORT_LEN + 1];
        buf[1..].copy_from_slice(&report);

        let ret = unsafe { libc::ioctl(file.as_raw_fd(), hidiocsfeature(buf.len()), buf.as_mut_ptr()) };
        if ret < 0 {
            bail!("HIDIOCSFEATURE failed: {}", std::io::Error::last_os_error());
        }

        Ok(())
    }
}

// Locate /dev/hidrawN for the given USB vendor/product/interface via sysfs.
fn find_hidraw(vendor_id: u16, product_id: u16, interface: u8) -> Result<PathBuf> {
    // HID_ID format in uevent: "0003:VVVVVVVV:PPPPPPPP" (uppercase hex, zero-padded to 8 digits)
    let hid_id = format!("0003:{vendor_id:08X}:{product_id:08X}");

    let mut entries: Vec<_> = fs::read_dir("/sys/class/hidraw")
        .context("cannot read /sys/class/hidraw")?
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let uevent = entry.path().join("device/uevent");
        let Ok(content) = fs::read_to_string(&uevent) else {
            continue;
        };
        if !content.contains(&hid_id) {
            continue;
        }

        // The symlink /sys/class/hidraw/hidrawN/device resolves to the HID device node.
        // Its parent is the USB interface dir, named like "3-2:1.N" where N = interface number.
        let canonical = match fs::canonicalize(entry.path().join("device")) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let usb_iface = match fs::canonicalize(canonical.join("..")) {
            Ok(p) => p,
            Err(_) => continue,
        };

        let iface_num: u8 = usb_iface
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|s| s.split('.').next_back())
            .and_then(|s| s.parse().ok())
            .unwrap_or(255);

        if iface_num == interface {
            return Ok(Path::new("/dev").join(entry.file_name()));
        }
    }

    bail!(
        "{} not found (no hidraw for {:04x}:{:04x} interface {})",
        "device",
        vendor_id,
        product_id,
        interface
    )
}

fn dock_rgb_report(color: Rgb) -> [u8; REPORT_LEN] {
    let mut bytes = [0u8; REPORT_LEN];

    bytes[0] = 0x00;
    bytes[1] = 0xF7;
    bytes[5] = 0x1D;
    bytes[6] = 0x0F;
    bytes[7] = 0x03;
    bytes[12] = 0x07;

    for led_index in 0..8 {
        let offset = 13 + (led_index * 3);
        bytes[offset] = color.red;
        bytes[offset + 1] = color.green;
        bytes[offset + 2] = color.blue;
    }

    bytes[88] = compute_crc(&bytes);
    bytes[89] = 0x00;

    bytes
}

// Basilisk V3 Pro 35K LED report via Mouse Dock Pro RF link.
// Reverse-engineered from Razer Synapse USB capture (Windows):
// - Same command class/id as dock (0x0F/0x03) but data_size=0x2C, 13 LED zones
// - Dock firmware routes ds=0x1D to dock ring, ds=0x2C to connected wireless mouse
fn mouse_via_dock_rgb_report(color: Rgb) -> [u8; REPORT_LEN] {
    let mut bytes = [0u8; REPORT_LEN];

    bytes[0] = 0x00; // status
    bytes[1] = 0x1F; // transaction_id (any value < 0xF7 targets the mouse)
    bytes[5] = 0x2C; // data_size = 44
    bytes[6] = 0x0F; // command_class
    bytes[7] = 0x03; // command_id
    bytes[12] = 0x0C; // LED_COUNT - 1 = 12 (13 zones on the Basilisk)

    for led_index in 0..13 {
        let offset = 13 + led_index * 3;
        bytes[offset] = color.red;
        bytes[offset + 1] = color.green;
        bytes[offset + 2] = color.blue;
    }

    bytes[88] = compute_crc(&bytes);
    bytes[89] = 0x00;

    bytes
}

fn compute_crc(bytes: &[u8; REPORT_LEN]) -> u8 {
    bytes[2..88].iter().fold(0u8, |acc, byte| acc ^ byte)
}

fn find_mouse_controller() -> Option<DeviceController> {
    // Direct USB (wired or dedicated wireless dongle)
    let direct = [
        DeviceSpec::basilisk_v3_pro_35k_wired(),
        DeviceSpec::basilisk_v3_pro_35k_wireless(),
    ];
    if let Some(spec) = direct
        .into_iter()
        .find(|s| find_hidraw(s.vendor_id, s.product_id, s.interface).is_ok())
    {
        return Some(DeviceController::new(spec, mouse_via_dock_rgb_report));
    }

    // Wireless via Mouse Dock Pro — mouse doesn't enumerate as its own USB device.
    // Commands go to dock hidraw0 (interface 0) with ds=0x2C format; dock routes to mouse via RF.
    if find_hidraw(RAZER_VENDOR_ID, MOUSE_DOCK_PRO_PRODUCT_ID, 0).is_ok() {
        return Some(DeviceController::new(
            DeviceSpec {
                vendor_id: RAZER_VENDOR_ID,
                product_id: MOUSE_DOCK_PRO_PRODUCT_ID,
                interface: 0,
                name: "Razer Basilisk V3 Pro 35K (via Dock)",
            },
            mouse_via_dock_rgb_report,
        ));
    }

    None
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let dock = DeviceController::new(DeviceSpec::mouse_dock_pro(), dock_rgb_report);
    let mouse = find_mouse_controller();

    match (cli.check, cli.color) {
        (true, None) => run_check(&dock, mouse.as_ref()),
        (false, Some(color_name)) => run_set_color(&dock, mouse.as_ref(), color_name),
        _ => bail!("specify exactly one action: --check or --color"),
    }
}

fn run_check(dock: &DeviceController, mouse: Option<&DeviceController>) -> Result<()> {
    match dock.check() {
        Ok(path) => println!("✓ {} ({}) accessible", dock.spec.name, path.display()),
        Err(e) => println!("✗ {}: {e}", dock.spec.name),
    }

    match mouse {
        Some(ctrl) => match ctrl.check() {
            Ok(path) => println!("✓ {} ({}) accessible", ctrl.spec.name, path.display()),
            Err(e) => println!("✗ {}: {e}", ctrl.spec.name),
        },
        None => println!("✗ Razer Basilisk V3 Pro 35K not detected"),
    }

    Ok(())
}

fn run_set_color(
    dock: &DeviceController,
    mouse: Option<&DeviceController>,
    color_name: ColorName,
) -> Result<()> {
    let color = color_name.rgb();
    let s = color_name.as_str();

    dock.set_color(color)
        .with_context(|| format!("failed to set dock color '{s}'"))?;
    println!("✓ Dock: {s}");

    match mouse {
        Some(ctrl) => {
            ctrl.set_color(color)
                .with_context(|| format!("failed to set mouse color '{s}'"))?;
            println!("✓ Mouse: {s}");
        }
        None => println!("⚠ Mouse not detected — skipped"),
    }

    Ok(())
}
