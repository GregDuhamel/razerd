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

// HIDIOC{S,G}FEATURE(len) = _IOC(_IOC_WRITE|_IOC_READ, 'H', {0x06,0x07}, len)
fn hidiocsfeature(len: usize) -> u64 {
    (3u64 << 30) | (b'H' as u64) << 8 | 0x06 | ((len as u64) << 16)
}

fn hidiocgfeature(len: usize) -> u64 {
    (3u64 << 30) | (b'H' as u64) << 8 | 0x07 | ((len as u64) << 16)
}

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    #[arg(long, conflicts_with_all = ["color", "battery", "info"])]
    check: bool,

    #[arg(long, value_enum, conflicts_with_all = ["check", "battery", "info"])]
    color: Option<ColorName>,

    #[arg(long, conflicts_with_all = ["check", "color", "info"])]
    battery: bool,

    #[arg(long, conflicts_with_all = ["check", "color", "battery"])]
    info: bool,
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
        let path = find_hidraw(
            self.spec.vendor_id,
            self.spec.product_id,
            self.spec.interface,
        )?;
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
        let path = self.open_hidraw_path()?;
        let file = open_hidraw(&path)?;
        send_feature_report(&file, &(self.build_report)(color))
    }

    fn open_hidraw_path(&self) -> Result<PathBuf> {
        find_hidraw(
            self.spec.vendor_id,
            self.spec.product_id,
            self.spec.interface,
        )
    }
}

fn open_hidraw(path: &Path) -> Result<std::fs::File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("cannot open {} — check udev permissions", path.display()))
}

// HIDIOCSFEATURE expects [report_id=0x00, ...90 bytes...] = 91 bytes total.
// The kernel strips report_id and issues USB SET_REPORT(feature, id=0) with our 90 bytes.
fn send_feature_report(file: &std::fs::File, report: &[u8; REPORT_LEN]) -> Result<()> {
    let mut buf = [0u8; REPORT_LEN + 1];
    buf[1..].copy_from_slice(report);
    let ret = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            hidiocsfeature(buf.len()),
            buf.as_mut_ptr(),
        )
    };
    if ret < 0 {
        bail!("HIDIOCSFEATURE failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

// Symmetric to send_feature_report: writes request, reads the 90-byte response back.
fn exchange_feature_report(
    file: &std::fs::File,
    request: &[u8; REPORT_LEN],
) -> Result<[u8; REPORT_LEN]> {
    send_feature_report(file, request)?;
    // Let the dock forward the request over RF and queue the mouse's reply.
    std::thread::sleep(std::time::Duration::from_millis(50));

    let mut buf = [0u8; REPORT_LEN + 1];
    let ret = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            hidiocgfeature(buf.len()),
            buf.as_mut_ptr(),
        )
    };
    if ret < 0 {
        bail!("HIDIOCGFEATURE failed: {}", std::io::Error::last_os_error());
    }

    let mut response = [0u8; REPORT_LEN];
    response.copy_from_slice(&buf[1..]);
    Ok(response)
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

// Razer HID query (SET_REPORT + GET_REPORT):
//   byte[1] = tx_id, byte[5] = data_size, byte[6] = class, byte[7] = cmd,
//   byte[8..8+args.len()] = request arguments, byte[88] = XOR CRC.
// Mouse-targeted queries use tx_id in the mouse range (< 0xF7); dock-targeted use 0xFF.
fn build_query(tx_id: u8, class: u8, cmd: u8, data_size: u8, args: &[u8]) -> [u8; REPORT_LEN] {
    let mut bytes = [0u8; REPORT_LEN];
    bytes[1] = tx_id;
    bytes[5] = data_size;
    bytes[6] = class;
    bytes[7] = cmd;
    bytes[8..8 + args.len()].copy_from_slice(args);
    bytes[88] = compute_crc(&bytes);
    bytes
}

#[derive(Debug)]
struct BatteryStatus {
    percent: u8,
    charging: bool,
}

fn query_battery(file: &std::fs::File) -> Result<BatteryStatus> {
    let level_resp = exchange_feature_report(file, &build_query(0x1F, 0x07, 0x80, 0x02, &[]))
        .context("battery level query failed")?;
    let charge_resp = exchange_feature_report(file, &build_query(0x1F, 0x07, 0x84, 0x02, &[]))
        .context("charging status query failed")?;

    // Response arguments[1] = byte[9]: 0-255 for level, 0/1 for charging.
    let percent = ((level_resp[9] as u32 * 100) / 255) as u8;
    let charging = charge_resp[9] != 0;

    Ok(BatteryStatus { percent, charging })
}

fn query_serial(file: &std::fs::File, tx_id: u8) -> Result<String> {
    let resp = exchange_feature_report(file, &build_query(tx_id, 0x00, 0x82, 0x16, &[]))
        .context("serial query failed")?;
    // Serial is an ASCII string in arguments[0..22] = bytes[8..30], zero-terminated.
    let raw = &resp[8..30];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    Ok(String::from_utf8_lossy(&raw[..end]).trim().to_string())
}

fn query_firmware(file: &std::fs::File, tx_id: u8) -> Result<(u8, u8)> {
    let resp = exchange_feature_report(file, &build_query(tx_id, 0x00, 0x81, 0x02, &[]))
        .context("firmware query failed")?;
    // arguments[0] = major, arguments[1] = minor
    Ok((resp[8], resp[9]))
}

fn query_dpi(file: &std::fs::File) -> Result<(u16, u16)> {
    // arg[0] = VARSTORE (0x01); response returns DPI X/Y as big-endian u16 pairs.
    let resp = exchange_feature_report(file, &build_query(0x1F, 0x04, 0x85, 0x07, &[0x01]))
        .context("DPI query failed")?;
    let dpi_x = u16::from_be_bytes([resp[9], resp[10]]);
    let dpi_y = u16::from_be_bytes([resp[11], resp[12]]);
    Ok((dpi_x, dpi_y))
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

    match (cli.check, cli.color, cli.battery, cli.info) {
        (true, None, false, false) => run_check(&dock, mouse.as_ref()),
        (false, Some(color_name), false, false) => {
            run_set_color(&dock, mouse.as_ref(), color_name)
        }
        (false, None, true, false) => run_battery(mouse.as_ref()),
        (false, None, false, true) => run_info(&dock, mouse.as_ref()),
        _ => bail!("specify exactly one action: --check, --color, --battery, or --info"),
    }
}

fn run_battery(mouse: Option<&DeviceController>) -> Result<()> {
    let ctrl = mouse.context("mouse not detected — battery query requires the Basilisk")?;
    let path = ctrl.open_hidraw_path()?;
    let file = open_hidraw(&path)?;
    let status = query_battery(&file)?;

    let charging = if status.charging { " (charging)" } else { "" };
    println!("✓ Battery: {}%{}", status.percent, charging);
    Ok(())
}

fn run_info(dock: &DeviceController, mouse: Option<&DeviceController>) -> Result<()> {
    // Dock info — queried directly with dock-range tx_id.
    let dock_path = dock.open_hidraw_path()?;
    let dock_file = open_hidraw(&dock_path)?;
    println!("{}", dock.spec.name);
    println!("  Path:     {}", dock_path.display());
    print_field("Serial", query_serial(&dock_file, 0xFF).ok());
    print_field("Firmware", query_firmware(&dock_file, 0xFF).ok().map(fw_str));

    // Mouse info — queries forwarded by the dock over RF.
    println!();
    match mouse {
        Some(ctrl) => {
            let mouse_path = ctrl.open_hidraw_path()?;
            let mouse_file = open_hidraw(&mouse_path)?;
            println!("{}", ctrl.spec.name);
            println!("  Path:     {}", mouse_path.display());
            print_field("Serial", query_serial(&mouse_file, 0x1F).ok());
            print_field("Firmware", query_firmware(&mouse_file, 0x1F).ok().map(fw_str));
            match query_battery(&mouse_file) {
                Ok(s) => {
                    println!("  Battery:  {}%", s.percent);
                    println!("  Charging: {}", if s.charging { "yes" } else { "no" });
                }
                Err(_) => println!("  Battery:  —"),
            }
            print_field(
                "DPI",
                query_dpi(&mouse_file).ok().map(|(x, y)| {
                    if x == y {
                        format!("{x}")
                    } else {
                        format!("{x} / {y}")
                    }
                }),
            );
        }
        None => println!("Razer Basilisk V3 Pro 35K — not detected"),
    }

    Ok(())
}

fn print_field<T: std::fmt::Display>(label: &str, value: Option<T>) {
    match value {
        Some(v) => println!("  {:<10}{}", format!("{label}:"), v),
        None => println!("  {:<10}—", format!("{label}:")),
    }
}

fn fw_str((major, minor): (u8, u8)) -> String {
    format!("{major}.{minor:02}")
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
