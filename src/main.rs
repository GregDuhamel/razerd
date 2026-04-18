//! Minimal RGB daemon for the Razer Mouse Dock Pro and a wirelessly connected
//! Razer Basilisk V3 Pro 35K.
//!
//! All commands go through a single hidraw interface on the dock. The dock
//! firmware routes requests to itself or forwards them to the wireless mouse
//! over the RF link based on the transaction id and report layout.

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

// ---------- USB / HID constants ----------

const RAZER_VENDOR_ID: u16 = 0x1532;
const MOUSE_DOCK_PRO_PRODUCT_ID: u16 = 0x00A4;
const DOCK_INTERFACE: u8 = 0;
const HID_SYSFS_ROOT: &str = "/sys/class/hidraw";
const DEV_ROOT: &str = "/dev";

// HID bus type prefix used by the kernel in /sys/.../device/uevent's HID_ID field.
// "0003" = USB HID.
const HID_ID_USB_PREFIX: &str = "0003";

// Razer HID feature report fixed size.
const REPORT_LEN: usize = 90;

// ---------- Razer protocol constants ----------

// Transaction id conventions observed on this hardware:
// * `TX_ID_DOCK_LED`/`TX_ID_DOCK`   — target the dock itself
// * `TX_ID_MOUSE`                   — dock forwards the request over RF to the mouse
const TX_ID_MOUSE: u8 = 0x1F;
const TX_ID_DOCK: u8 = 0xFF;
const TX_ID_DOCK_LED: u8 = 0xF7;

const CLASS_DEVICE: u8 = 0x00;
const CMD_GET_FIRMWARE: u8 = 0x81;
const CMD_GET_SERIAL: u8 = 0x82;

const CLASS_DPI: u8 = 0x04;
const CMD_GET_DPI: u8 = 0x85;

const CLASS_POWER: u8 = 0x07;
const CMD_GET_BATTERY_LEVEL: u8 = 0x80;
const CMD_GET_CHARGING: u8 = 0x84;

const CLASS_EXTENDED_MATRIX: u8 = 0x0F;
const CMD_SET_MATRIX_EFFECT: u8 = 0x03;

// Dock LED ring layout.
const DOCK_LED_DATA_SIZE: u8 = 0x1D;
const DOCK_LED_ZONES: usize = 8;
const DOCK_LED_COUNT_MINUS_ONE: u8 = (DOCK_LED_ZONES as u8) - 1;

// Basilisk V3 Pro 35K LED layout when routed via the dock.
const MOUSE_LED_DATA_SIZE: u8 = 0x2C;
const MOUSE_LED_ZONES: usize = 13;
const MOUSE_LED_COUNT_MINUS_ONE: u8 = (MOUSE_LED_ZONES as u8) - 1;

// Window given to the dock to forward a request to the mouse over RF and
// queue the reply before we read it back.
const RESPONSE_WAIT: Duration = Duration::from_millis(50);

// Linux hidraw ioctl numbers: _IOC(_IOC_WRITE|_IOC_READ, 'H', {0x06,0x07}, len)
fn hidioc_set_feature(len: usize) -> u64 {
    (3u64 << 30) | ((b'H' as u64) << 8) | 0x06 | ((len as u64) << 16)
}

fn hidioc_get_feature(len: usize) -> u64 {
    (3u64 << 30) | ((b'H' as u64) << 8) | 0x07 | ((len as u64) << 16)
}

// ---------- CLI ----------

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    /// Verify the dock is detected and accessible.
    #[arg(long, conflicts_with_all = ["color", "battery", "info"])]
    check: bool,

    /// Apply a color to the dock and the wireless mouse.
    #[arg(long, value_enum, conflicts_with_all = ["check", "battery", "info"])]
    color: Option<ColorName>,

    /// Print the mouse battery level and charging status.
    #[arg(long, conflicts_with_all = ["check", "color", "info"])]
    battery: bool,

    /// Print a full device report (serial, firmware, battery, DPI, ...).
    #[arg(long, conflicts_with_all = ["check", "color", "battery"])]
    info: bool,
}

enum Action {
    Check,
    Color(ColorName),
    Battery,
    Info,
}

impl Cli {
    fn action(&self) -> Result<Action> {
        match (self.check, self.color, self.battery, self.info) {
            (true, None, false, false) => Ok(Action::Check),
            (false, Some(c), false, false) => Ok(Action::Color(c)),
            (false, None, true, false) => Ok(Action::Battery),
            (false, None, false, true) => Ok(Action::Info),
            _ => bail!("specify exactly one action: --check, --color, --battery, or --info"),
        }
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

// ---------- Hidraw device ----------

/// Owned handle over a `/dev/hidraw*` node, with typed feature-report I/O.
struct HidrawDevice {
    file: File,
    path: PathBuf,
}

impl HidrawDevice {
    /// Open the Mouse Dock Pro's control interface.
    fn open_dock() -> Result<Self> {
        let path = find_hidraw(RAZER_VENDOR_ID, MOUSE_DOCK_PRO_PRODUCT_ID, DOCK_INTERFACE)
            .context("Razer Mouse Dock Pro not detected")?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("cannot open {} — check udev permissions", path.display()))?;
        Ok(Self { file, path })
    }

    /// Send a 90-byte HID feature report (SET_REPORT with type=feature, id=0).
    ///
    /// The hidraw ioctl buffer is `[report_id, ...90 bytes...]`; the kernel
    /// strips the report id and issues the USB control transfer.
    fn send_feature(&self, report: &[u8; REPORT_LEN]) -> Result<()> {
        let mut buf = [0u8; REPORT_LEN + 1];
        buf[1..].copy_from_slice(report);

        // SAFETY: `self.file` is an owned, valid fd; `buf` is a unique mutable
        // array of exactly the byte-length we pass to the ioctl; the kernel
        // hidraw driver accepts the call and returns either ≥0 on success or
        // -1 on failure (with errno set).
        let ret = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                hidioc_set_feature(buf.len()),
                buf.as_mut_ptr(),
            )
        };
        if ret < 0 {
            bail!("HIDIOCSFEATURE failed: {}", std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Send a request and read back the matching 90-byte response.
    fn exchange_feature(&self, request: &[u8; REPORT_LEN]) -> Result<[u8; REPORT_LEN]> {
        self.send_feature(request)?;
        std::thread::sleep(RESPONSE_WAIT);

        let mut buf = [0u8; REPORT_LEN + 1];

        // SAFETY: same invariants as `send_feature`; `HIDIOCGFEATURE` writes at
        // most `buf.len()` bytes into `buf`.
        let ret = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                hidioc_get_feature(buf.len()),
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
}

// ---------- Hidraw discovery ----------

/// Resolve `/dev/hidrawN` for the given USB vendor/product/interface by
/// walking `/sys/class/hidraw`.
fn find_hidraw(vendor_id: u16, product_id: u16, interface: u8) -> Result<PathBuf> {
    let hid_id = format!("{HID_ID_USB_PREFIX}:{vendor_id:08X}:{product_id:08X}");

    let mut entries: Vec<_> = std::fs::read_dir(HID_SYSFS_ROOT)
        .with_context(|| format!("cannot read {HID_SYSFS_ROOT}"))?
        .collect::<std::io::Result<_>>()
        .with_context(|| format!("cannot enumerate {HID_SYSFS_ROOT}"))?;
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let uevent_path = entry.path().join("device/uevent");
        let Ok(uevent) = std::fs::read_to_string(&uevent_path) else {
            continue;
        };
        if !uevent.contains(&hid_id) {
            continue;
        }

        if hidraw_interface_number(&entry.path()) == Some(interface) {
            return Ok(Path::new(DEV_ROOT).join(entry.file_name()));
        }
    }

    bail!(
        "no hidraw for {:04x}:{:04x} interface {} — device not connected?",
        vendor_id,
        product_id,
        interface
    )
}

/// Extract the USB interface number from the hidraw's sysfs symlink.
///
/// The `device` symlink resolves to the HID device node; its parent is the
/// USB interface directory named like `3-2:1.N` where `N` is the interface
/// number.
fn hidraw_interface_number(hidraw_sysfs_path: &Path) -> Option<u8> {
    let canonical = std::fs::canonicalize(hidraw_sysfs_path.join("device")).ok()?;
    let usb_iface = std::fs::canonicalize(canonical.join("..")).ok()?;
    usb_iface
        .file_name()?
        .to_str()?
        .rsplit_once('.')?
        .1
        .parse()
        .ok()
}

// ---------- Protocol: report builders ----------

/// XOR of bytes 2..=87, the Razer HID report checksum.
fn compute_crc(bytes: &[u8; REPORT_LEN]) -> u8 {
    bytes[2..88].iter().fold(0u8, |acc, b| acc ^ b)
}

/// Populate the common fixed-size header used by all LED matrix reports and
/// return the byte index at which the per-LED RGB triplets begin.
fn write_matrix_header(
    bytes: &mut [u8; REPORT_LEN],
    tx_id: u8,
    data_size: u8,
    led_count_minus_one: u8,
) -> usize {
    bytes[1] = tx_id;
    bytes[5] = data_size;
    bytes[6] = CLASS_EXTENDED_MATRIX;
    bytes[7] = CMD_SET_MATRIX_EFFECT;
    bytes[12] = led_count_minus_one;
    13
}

fn fill_leds(bytes: &mut [u8; REPORT_LEN], start: usize, zones: usize, color: Rgb) {
    for i in 0..zones {
        let o = start + i * 3;
        bytes[o] = color.red;
        bytes[o + 1] = color.green;
        bytes[o + 2] = color.blue;
    }
}

/// Build the dock's own 8-LED ring command.
fn dock_rgb_report(color: Rgb) -> [u8; REPORT_LEN] {
    let mut bytes = [0u8; REPORT_LEN];
    let start = write_matrix_header(
        &mut bytes,
        TX_ID_DOCK_LED,
        DOCK_LED_DATA_SIZE,
        DOCK_LED_COUNT_MINUS_ONE,
    );
    fill_leds(&mut bytes, start, DOCK_LED_ZONES, color);
    bytes[88] = compute_crc(&bytes);
    bytes
}

/// Build the Basilisk V3 Pro 35K 13-zone command. When sent to the dock's
/// hidraw, the firmware forwards it to the mouse over the RF link.
fn mouse_via_dock_rgb_report(color: Rgb) -> [u8; REPORT_LEN] {
    let mut bytes = [0u8; REPORT_LEN];
    let start = write_matrix_header(
        &mut bytes,
        TX_ID_MOUSE,
        MOUSE_LED_DATA_SIZE,
        MOUSE_LED_COUNT_MINUS_ONE,
    );
    fill_leds(&mut bytes, start, MOUSE_LED_ZONES, color);
    bytes[88] = compute_crc(&bytes);
    bytes
}

// ---------- Protocol: query helpers ----------

/// Build a query report: `[_, tx_id, 0, 0, 0, data_size, class, cmd, args..., _, crc, _]`.
fn build_query(tx_id: u8, class: u8, cmd: u8, data_size: u8, args: &[u8]) -> [u8; REPORT_LEN] {
    assert!(
        8 + args.len() <= 88,
        "query arguments overflow the report body"
    );

    let mut bytes = [0u8; REPORT_LEN];
    bytes[1] = tx_id;
    bytes[5] = data_size;
    bytes[6] = class;
    bytes[7] = cmd;
    bytes[8..8 + args.len()].copy_from_slice(args);
    bytes[88] = compute_crc(&bytes);
    bytes
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BatteryStatus {
    percent: u8,
    charging: bool,
}

fn query_battery(dock: &HidrawDevice) -> Result<BatteryStatus> {
    let level = dock
        .exchange_feature(&build_query(
            TX_ID_MOUSE,
            CLASS_POWER,
            CMD_GET_BATTERY_LEVEL,
            0x02,
            &[],
        ))
        .context("battery level query failed")?;
    let charge = dock
        .exchange_feature(&build_query(
            TX_ID_MOUSE,
            CLASS_POWER,
            CMD_GET_CHARGING,
            0x02,
            &[],
        ))
        .context("charging status query failed")?;

    Ok(BatteryStatus {
        percent: parse_battery_percent(level[9]),
        charging: charge[9] != 0,
    })
}

/// Razer reports battery as 0..=255; scale to 0..=100 saturating.
fn parse_battery_percent(raw: u8) -> u8 {
    ((raw as u32 * 100) / 255) as u8
}

fn query_serial(dock: &HidrawDevice, tx_id: u8) -> Result<String> {
    let resp = dock
        .exchange_feature(&build_query(tx_id, CLASS_DEVICE, CMD_GET_SERIAL, 0x16, &[]))
        .context("serial query failed")?;
    Ok(parse_serial(&resp))
}

/// Serial is an ASCII string in the 22-byte argument block, zero-terminated.
fn parse_serial(response: &[u8; REPORT_LEN]) -> String {
    let raw = &response[8..30];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end]).trim().to_string()
}

fn query_firmware(dock: &HidrawDevice, tx_id: u8) -> Result<FirmwareVersion> {
    let resp = dock
        .exchange_feature(&build_query(
            tx_id,
            CLASS_DEVICE,
            CMD_GET_FIRMWARE,
            0x02,
            &[],
        ))
        .context("firmware query failed")?;
    Ok(FirmwareVersion {
        major: resp[8],
        minor: resp[9],
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FirmwareVersion {
    major: u8,
    minor: u8,
}

impl std::fmt::Display for FirmwareVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{:02}", self.major, self.minor)
    }
}

fn query_dpi(dock: &HidrawDevice) -> Result<(u16, u16)> {
    // arg[0] = VARSTORE (0x01); response carries DPI X/Y as big-endian u16 pairs.
    let resp = dock
        .exchange_feature(&build_query(
            TX_ID_MOUSE,
            CLASS_DPI,
            CMD_GET_DPI,
            0x07,
            &[0x01],
        ))
        .context("DPI query failed")?;
    Ok((
        u16::from_be_bytes([resp[9], resp[10]]),
        u16::from_be_bytes([resp[11], resp[12]]),
    ))
}

// ---------- Actions ----------

fn main() -> Result<()> {
    let cli = Cli::parse();
    let action = cli.action()?;
    let dock = HidrawDevice::open_dock()?;

    match action {
        Action::Check => run_check(&dock),
        Action::Color(c) => run_color(&dock, c),
        Action::Battery => run_battery(&dock),
        Action::Info => run_info(&dock),
    }
}

fn run_check(dock: &HidrawDevice) -> Result<()> {
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

fn run_color(dock: &HidrawDevice, color: ColorName) -> Result<()> {
    let label = color.as_str();

    dock.send_feature(&dock_rgb_report(color.rgb()))
        .with_context(|| format!("failed to set dock color '{label}'"))?;
    println!("✓ Dock: {label}");

    // Sent through the dock; if the mouse is not paired, the dock drops it silently.
    dock.send_feature(&mouse_via_dock_rgb_report(color.rgb()))
        .with_context(|| format!("failed to set mouse color '{label}'"))?;
    println!("✓ Mouse: {label}");

    Ok(())
}

fn run_battery(dock: &HidrawDevice) -> Result<()> {
    let status = query_battery(dock)?;
    let suffix = if status.charging { " (charging)" } else { "" };
    println!("✓ Battery: {}%{}", status.percent, suffix);
    Ok(())
}

fn run_info(dock: &HidrawDevice) -> Result<()> {
    println!("Razer Mouse Dock Pro");
    println!("  Path:     {}", dock.path.display());
    print_field("Serial", query_serial(dock, TX_ID_DOCK).ok());
    print_field("Firmware", query_firmware(dock, TX_ID_DOCK).ok());

    println!();
    println!("Razer Basilisk V3 Pro 35K (via Dock)");
    println!("  Path:     {}", dock.path.display());
    print_field("Serial", query_serial(dock, TX_ID_MOUSE).ok());
    print_field("Firmware", query_firmware(dock, TX_ID_MOUSE).ok());

    match query_battery(dock) {
        Ok(s) => {
            println!("  Battery:  {}%", s.percent);
            println!("  Charging: {}", if s.charging { "yes" } else { "no" });
        }
        Err(_) => println!("  Battery:  —"),
    }
    print_field("DPI", query_dpi(dock).ok().map(format_dpi));

    Ok(())
}

fn print_field<T: std::fmt::Display>(label: &str, value: Option<T>) {
    match value {
        Some(v) => println!("  {:<10}{}", format!("{label}:"), v),
        None => println!("  {:<10}—", format!("{label}:")),
    }
}

fn format_dpi((x, y): (u16, u16)) -> String {
    if x == y {
        format!("{x}")
    } else {
        format!("{x} / {y}")
    }
}

// ---------- Tests ----------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_xors_body_only() {
        let mut bytes = [0u8; REPORT_LEN];
        bytes[0] = 0xAB; // outside the XOR range, ignored
        bytes[1] = 0xCD; // outside the XOR range, ignored
        bytes[2] = 0x12;
        bytes[5] = 0x34;
        bytes[87] = 0x56;
        bytes[88] = 0xFF; // outside the XOR range, ignored
        bytes[89] = 0xEE; // outside the XOR range, ignored
        assert_eq!(compute_crc(&bytes), 0x12 ^ 0x34 ^ 0x56);
    }

    /// Matches the dock LED command captured live from Razer Synapse on
    /// Windows: red (0xC0, 0x00, 0x00) produces CRC 0x16.
    #[test]
    fn dock_rgb_report_matches_wireshark_capture() {
        let bytes = dock_rgb_report(Rgb::new(0xC0, 0x00, 0x00));
        let expected_prefix = [
            0x00, 0xF7, 0x00, 0x00, 0x00, 0x1D, 0x0F, 0x03, 0x00, 0x00, 0x00, 0x00, 0x07,
        ];
        assert_eq!(&bytes[..expected_prefix.len()], &expected_prefix);
        for i in 0..DOCK_LED_ZONES {
            assert_eq!(bytes[13 + i * 3], 0xC0);
            assert_eq!(bytes[14 + i * 3], 0x00);
            assert_eq!(bytes[15 + i * 3], 0x00);
        }
        assert_eq!(bytes[88], 0x16);
        assert_eq!(bytes[89], 0x00);
    }

    #[test]
    fn mouse_via_dock_rgb_report_structure() {
        let bytes = mouse_via_dock_rgb_report(Rgb::new(0x00, 0x00, 0xC0));
        assert_eq!(bytes[1], TX_ID_MOUSE);
        assert_eq!(bytes[5], MOUSE_LED_DATA_SIZE);
        assert_eq!(bytes[6], CLASS_EXTENDED_MATRIX);
        assert_eq!(bytes[7], CMD_SET_MATRIX_EFFECT);
        assert_eq!(bytes[12], MOUSE_LED_COUNT_MINUS_ONE);
        for i in 0..MOUSE_LED_ZONES {
            assert_eq!(bytes[13 + i * 3], 0x00);
            assert_eq!(bytes[14 + i * 3], 0x00);
            assert_eq!(bytes[15 + i * 3], 0xC0);
        }
        // Zones past MOUSE_LED_ZONES must remain zero.
        assert_eq!(bytes[13 + MOUSE_LED_ZONES * 3], 0x00);
        assert_eq!(bytes[88], compute_crc(&bytes));
    }

    #[test]
    fn build_query_places_fields_correctly() {
        let q = build_query(TX_ID_MOUSE, CLASS_POWER, CMD_GET_BATTERY_LEVEL, 0x02, &[]);
        assert_eq!(q[1], TX_ID_MOUSE);
        assert_eq!(q[5], 0x02);
        assert_eq!(q[6], CLASS_POWER);
        assert_eq!(q[7], CMD_GET_BATTERY_LEVEL);
        assert_eq!(q[88], compute_crc(&q));
    }

    #[test]
    fn build_query_copies_arguments() {
        let q = build_query(TX_ID_MOUSE, CLASS_DPI, CMD_GET_DPI, 0x07, &[0x01, 0x02, 0x03]);
        assert_eq!(&q[8..11], &[0x01, 0x02, 0x03]);
    }

    #[test]
    fn color_name_rgb_table() {
        assert_eq!(ColorName::Red.rgb(), Rgb::new(0xC0, 0x00, 0x00));
        assert_eq!(ColorName::Green.rgb(), Rgb::new(0x00, 0xC0, 0x00));
        assert_eq!(ColorName::Blue.rgb(), Rgb::new(0x00, 0x00, 0xC0));
        assert_eq!(ColorName::White.rgb(), Rgb::new(0xFF, 0xFF, 0xFF));
        assert_eq!(ColorName::Off.rgb(), Rgb::new(0x00, 0x00, 0x00));
    }

    #[test]
    fn battery_percent_scales_0_255_to_0_100() {
        assert_eq!(parse_battery_percent(0), 0);
        assert_eq!(parse_battery_percent(255), 100);
        assert_eq!(parse_battery_percent(127), 49); // integer truncation
    }

    #[test]
    fn parse_serial_strips_trailing_zeros_and_whitespace() {
        let mut resp = [0u8; REPORT_LEN];
        let serial = b"PM2516H33301682";
        resp[8..8 + serial.len()].copy_from_slice(serial);
        // bytes past `serial.len()` stay zero, simulating a zero-terminated C string.
        assert_eq!(parse_serial(&resp), "PM2516H33301682");
    }

    #[test]
    fn parse_serial_handles_non_ascii_gracefully() {
        let mut resp = [0u8; REPORT_LEN];
        resp[8] = 0xFF; // invalid UTF-8 start byte
        resp[9] = b'A';
        // Should not panic; lossy conversion replaces invalid bytes.
        let _ = parse_serial(&resp);
    }

    #[test]
    fn firmware_version_formats_with_zero_padded_minor() {
        let fw = FirmwareVersion { major: 2, minor: 1 };
        assert_eq!(fw.to_string(), "2.01");
    }

    #[test]
    fn format_dpi_collapses_identical_axes() {
        assert_eq!(format_dpi((1800, 1800)), "1800");
        assert_eq!(format_dpi((1600, 800)), "1600 / 800");
    }

    #[test]
    fn cli_action_requires_exactly_one_flag() {
        let mut cli = Cli {
            check: false,
            color: None,
            battery: false,
            info: false,
        };
        assert!(cli.action().is_err());

        cli.check = true;
        assert!(matches!(cli.action().unwrap(), Action::Check));

        cli.check = false;
        cli.color = Some(ColorName::Blue);
        assert!(matches!(cli.action().unwrap(), Action::Color(ColorName::Blue)));
    }

    /// Regression test: HIDIOCSFEATURE for a 91-byte buffer must match what
    /// the Linux kernel expects (`_IOC(_IOC_WRITE|_IOC_READ, 'H', 0x06, 91)`).
    #[test]
    fn hidioc_codes_match_kernel_encoding() {
        assert_eq!(hidioc_set_feature(91), 0xC05B_4806);
        assert_eq!(hidioc_get_feature(91), 0xC05B_4807);
    }
}
