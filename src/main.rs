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
use std::time::{Duration, Instant};

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

// The 1-byte XOR checksum sits at this offset and covers bytes `2..CRC_INDEX`.
const CRC_INDEX: usize = 88;

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

// First argument of DPI get/set: which storage slot to address. The firmware
// runs from the live (RAM) slot — the Cycle Up Sensitivity Stages button
// cycles that one — while the stored slot survives sleep and power-cycles.
// Writes must hit both to be effective now AND persistent; reads of the live
// slot show what the sensor actually runs at.
const LIVESTORE: u8 = 0x00;
const VARSTORE: u8 = 0x01;

const CLASS_DPI: u8 = 0x04;
const CMD_SET_DPI: u8 = 0x05;
const CMD_GET_DPI: u8 = 0x85;
const CMD_SET_DPI_STAGES: u8 = 0x06;
const CMD_GET_DPI_STAGES: u8 = 0x86;

// Stage table layout: [store, active_stage (1-based), count] then per stage
// 7 bytes: index, X hi/lo, Y hi/lo, 2 reserved. 3 + 5*7 = 38 = 0x26.
const DPI_STAGES_DATA_SIZE: u8 = 0x26;

// The stage table `--sensitivity-stages on` installs — Synapse's defaults for
// "Cycle Up Sensitivity Stages", stage 3 (1600) active. The firmware caps the
// table at 5 stages: a 6th is rejected with status 0x03.
const DEFAULT_DPI_STAGES: [u16; 5] = [400, 800, 1600, 3200, 6400];
const DEFAULT_DPI_ACTIVE_STAGE: u8 = 3;

// Sensor limits of the Basilisk V3 Pro 35K (the "35K" is the max DPI).
const DPI_MIN: u16 = 100;
const DPI_MAX: u16 = 35_000;

// Onboard profile slots, cycled with the button under the mouse. The
// indicator LED next to it shows the active slot's color.
const CLASS_PROFILE: u8 = 0x05;
const CMD_GET_PROFILE_COUNT: u8 = 0x80;
const CMD_GET_ACTIVE_PROFILE: u8 = 0x84;

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

// The firmware writes a status code into byte 0 of the response once it has
// processed a request (and, for mouse queries, completed the RF round-trip).
// We poll for it rather than sleeping a fixed worst-case interval: replies are
// usually ready within a few milliseconds, but a sleeping/absent mouse can take
// longer or never answer.
const RESPONSE_TIMEOUT: Duration = Duration::from_millis(100);
const RESPONSE_POLL_INTERVAL: Duration = Duration::from_millis(2);

// Response status byte (`response[0]`) values used by the Razer firmware.
const STATUS_NEW: u8 = 0x00; // not processed yet
const STATUS_BUSY: u8 = 0x01; // accepted, still working (RF round-trip pending)
const STATUS_OK: u8 = 0x02; // completed, payload valid

// ---------- Watch-mode tuning ----------

// `--watch` re-applies the color when the mouse wakes. The dock emits no
// dedicated wake event — it just resumes forwarding mouse-motion input reports
// the instant the mouse comes back. So we treat "input resumed after a quiet
// gap of at least this long" as a wake. It must sit above an ordinary pause in
// movement yet below the mouse's own sleep timeout (Razer's is much longer).
const WAKE_IDLE_THRESHOLD: Duration = Duration::from_secs(5);

// While the mouse is in use we also re-apply the color on this cadence, as a
// safety net against the firmware drifting back to its onboard default for no
// visible reason. We skip it entirely when the mouse has been idle, so a
// sleeping or absent mouse costs nothing.
const WATCH_SAFETY_INTERVAL: Duration = Duration::from_secs(60);

// Linux hidraw ioctl numbers: _IOC(_IOC_WRITE|_IOC_READ, 'H', {0x06,0x07}, len)
fn hidioc_set_feature(len: usize) -> u64 {
    (3u64 << 30) | ((b'H' as u64) << 8) | 0x06 | ((len as u64) << 16)
}

fn hidioc_get_feature(len: usize) -> u64 {
    (3u64 << 30) | ((b'H' as u64) << 8) | 0x07 | ((len as u64) << 16)
}

// ---------- CLI ----------

// Every flag belongs to the `action` group: clap enforces "exactly one of
// these" (required + non-multiple) so the struct needs no pairwise conflict
// lists and `action()` needs no arity checks.
#[derive(Debug, Parser)]
#[command(author, version, about)]
#[command(group = clap::ArgGroup::new("action").required(true).multiple(false))]
struct Cli {
    /// Verify the dock is detected and accessible.
    #[arg(long, group = "action")]
    check: bool,

    /// Apply a color to the dock and the wireless mouse.
    #[arg(long, value_enum, group = "action")]
    color: Option<ColorName>,

    /// Print the mouse battery level and charging status.
    #[arg(long, group = "action")]
    battery: bool,

    /// Print a full device report (serial, firmware, battery, DPI, ...).
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

enum Action {
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
    fn action(&self) -> Action {
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

fn parse_dpi(s: &str) -> Result<u16, String> {
    let n: u16 = s.parse().map_err(|_| format!("'{s}' is not a DPI value"))?;
    if !(DPI_MIN..=DPI_MAX).contains(&n) {
        return Err(format!("{n} is out of range ({DPI_MIN}-{DPI_MAX})"));
    }
    Ok(n)
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

    /// Read the current 90-byte feature report (GET_REPORT with type=feature).
    fn get_feature(&self) -> Result<[u8; REPORT_LEN]> {
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

    /// Block until the device emits an input report, returning its length.
    ///
    /// Unlike feature reports (which we pull on demand via ioctl), hidraw
    /// delivers the device's spontaneous input reports through `read()`. On
    /// this dock those are plain mouse-motion packets — there is no dedicated
    /// wake/sleep event — so `--watch` and `--sniff` use their mere presence
    /// (input flowing vs. silence) as the signal.
    fn read_input_report(&self, buf: &mut [u8]) -> Result<usize> {
        use std::io::Read;
        (&self.file)
            .read(buf)
            .context("reading hidraw input report")
    }

    /// Send a request and poll for the matching 90-byte response, returning it
    /// only once the firmware reports the transaction as completed.
    fn exchange_feature(&self, request: &[u8; REPORT_LEN]) -> Result<[u8; REPORT_LEN]> {
        self.send_feature(request)?;

        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        loop {
            std::thread::sleep(RESPONSE_POLL_INTERVAL);
            let response = self.get_feature()?;

            // The firmware echoes data_size/class/cmd (bytes 5..8) in its
            // response. A mismatch means we read someone else's transaction —
            // e.g. a concurrent `razerd --watch` re-applying the color — so
            // treat it like a pending read and keep polling for our own.
            if response[5..8] != request[5..8] {
                if Instant::now() < deadline {
                    continue;
                }
                bail!("response belongs to another request (is another razerd instance running?)");
            }

            match classify_response_status(response[0]) {
                ResponseStatus::Ready => return Ok(response),
                ResponseStatus::Pending if Instant::now() < deadline => continue,
                ResponseStatus::Pending => {
                    bail!("device did not answer within {RESPONSE_TIMEOUT:?}")
                }
                ResponseStatus::Failed(status) => {
                    bail!("device returned error status 0x{status:02x}")
                }
            }
        }
    }
}

/// Outcome of inspecting a response's status byte (`response[0]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseStatus {
    /// Completed successfully; the payload is valid.
    Ready,
    /// Still being processed — keep polling until the deadline.
    Pending,
    /// Terminal failure (e.g. unsupported, no RF response); carries the raw code.
    Failed(u8),
}

fn classify_response_status(status: u8) -> ResponseStatus {
    match status {
        STATUS_OK => ResponseStatus::Ready,
        STATUS_NEW | STATUS_BUSY => ResponseStatus::Pending,
        other => ResponseStatus::Failed(other),
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

/// XOR of bytes `2..CRC_INDEX`, the Razer HID report checksum.
fn compute_crc(bytes: &[u8; REPORT_LEN]) -> u8 {
    bytes[2..CRC_INDEX].iter().fold(0u8, |acc, b| acc ^ b)
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
    bytes[CRC_INDEX] = compute_crc(&bytes);
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
    bytes[CRC_INDEX] = compute_crc(&bytes);
    bytes
}

// ---------- Protocol: query helpers ----------

/// Build a query report: `[_, tx_id, 0, 0, 0, data_size, class, cmd, args..., _, crc, _]`.
fn build_query(tx_id: u8, class: u8, cmd: u8, data_size: u8, args: &[u8]) -> [u8; REPORT_LEN] {
    assert!(
        8 + args.len() <= CRC_INDEX,
        "query arguments overflow the report body"
    );

    let mut bytes = [0u8; REPORT_LEN];
    bytes[1] = tx_id;
    bytes[5] = data_size;
    bytes[6] = class;
    bytes[7] = cmd;
    bytes[8..8 + args.len()].copy_from_slice(args);
    bytes[CRC_INDEX] = compute_crc(&bytes);
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

/// Razer reports battery as 0..=255; rescale to 0..=100 (truncating).
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProfileInfo {
    active: u8,
    count: u8,
}

impl std::fmt::Display for ProfileInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match profile_color_name(self.active) {
            Some(color) => write!(f, "{} ({}) of {}", self.active, color, self.count),
            None => write!(f, "{} of {}", self.active, self.count),
        }
    }
}

/// Color shown by the mouse's profile indicator LED for each slot.
fn profile_color_name(slot: u8) -> Option<&'static str> {
    match slot {
        1 => Some("white"),
        2 => Some("red"),
        3 => Some("green"),
        4 => Some("blue"),
        5 => Some("cyan"),
        _ => None,
    }
}

fn query_profiles(dock: &HidrawDevice) -> Result<ProfileInfo> {
    let count = dock
        .exchange_feature(&build_query(
            TX_ID_MOUSE,
            CLASS_PROFILE,
            CMD_GET_PROFILE_COUNT,
            0x01,
            &[],
        ))
        .context("profile count query failed")?;
    let active = dock
        .exchange_feature(&build_query(
            TX_ID_MOUSE,
            CLASS_PROFILE,
            CMD_GET_ACTIVE_PROFILE,
            0x01,
            &[],
        ))
        .context("active profile query failed")?;

    Ok(ProfileInfo {
        active: active[8],
        count: count[8],
    })
}

fn query_dpi(dock: &HidrawDevice) -> Result<(u16, u16)> {
    // Read the live slot: it reflects DPI-button presses, the stored slot
    // doesn't. Response carries X/Y as big-endian u16 pairs after the store byte.
    let resp = dock
        .exchange_feature(&build_query(
            TX_ID_MOUSE,
            CLASS_DPI,
            CMD_GET_DPI,
            0x07,
            &[LIVESTORE],
        ))
        .context("DPI query failed")?;
    Ok((
        u16::from_be_bytes([resp[9], resp[10]]),
        u16::from_be_bytes([resp[11], resp[12]]),
    ))
}

/// Write the DPI to the live and stored slots — the report carries X and Y as
/// big-endian u16 pairs (same layout as the GET response); we drive both axes
/// with the same value.
/// Onboard stage table state, shown by `--info` as the lock indicator: one
/// stage means the sensitivity is pinned (the Cycle Up Sensitivity Stages
/// button has nothing to cycle to), several mean the button is live.
struct DpiStages {
    active: u8,
    stages: Vec<u16>,
}

impl std::fmt::Display for DpiStages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.stages[..] {
            [] => return f.write_str("—"), // defensive: a garbled table
            [only] => return write!(f, "🔒 off — locked at {only}"),
            _ => {}
        }
        // Not the open-padlock glyph: it is indistinguishable from 🔒 in
        // some terminal fonts. The cycle arrows say what the button does.
        write!(f, "🔄 on — ")?;
        for (i, dpi) in self.stages.iter().enumerate() {
            if i > 0 {
                f.write_str("/")?;
            }
            if (i + 1) as u8 == self.active {
                write!(f, "[{dpi}]")?;
            } else {
                write!(f, "{dpi}")?;
            }
        }
        Ok(())
    }
}

/// Parse a GET stage-table response: after the status header the args are
/// `[store, active_stage, count]` then per stage `[index, X be, Y be, 2
/// reserved]` — the mirror of `dpi_stages_args`.
fn parse_dpi_stages(resp: &[u8; REPORT_LEN]) -> DpiStages {
    let active = resp[9];
    let count = (resp[10] as usize).min(DEFAULT_DPI_STAGES.len());
    let stages = (0..count)
        .map(|i| {
            let entry = 11 + i * 7;
            u16::from_be_bytes([resp[entry + 1], resp[entry + 2]])
        })
        .collect();
    DpiStages { active, stages }
}

fn query_dpi_stages(dock: &HidrawDevice) -> Result<DpiStages> {
    // Live slot: the table the button actually cycles.
    let resp = dock
        .exchange_feature(&build_query(
            TX_ID_MOUSE,
            CLASS_DPI,
            CMD_GET_DPI_STAGES,
            DPI_STAGES_DATA_SIZE,
            &[LIVESTORE],
        ))
        .context("DPI stage table query failed")?;
    Ok(parse_dpi_stages(&resp))
}

/// "live" / "stored" for error messages, so a failure between the two writes
/// says which slot was left untouched.
fn slot_name(store: u8) -> &'static str {
    if store == LIVESTORE { "live" } else { "stored" }
}

fn set_dpi(dock: &HidrawDevice, dpi: u16) -> Result<()> {
    let [hi, lo] = dpi.to_be_bytes();
    for store in [LIVESTORE, VARSTORE] {
        dock.exchange_feature(&build_query(
            TX_ID_MOUSE,
            CLASS_DPI,
            CMD_SET_DPI,
            0x07,
            &[store, hi, lo, hi, lo],
        ))
        .with_context(|| format!("DPI set failed ({} slot)", slot_name(store)))?;
    }
    Ok(())
}

/// Write the onboard stage table (the one the Cycle Up Sensitivity Stages
/// button cycles) to both the live and stored slots.
fn set_dpi_stages(dock: &HidrawDevice, active: u8, stages: &[u16]) -> Result<()> {
    for store in [LIVESTORE, VARSTORE] {
        dock.exchange_feature(&build_query(
            TX_ID_MOUSE,
            CLASS_DPI,
            CMD_SET_DPI_STAGES,
            DPI_STAGES_DATA_SIZE,
            &dpi_stages_args(store, active, stages),
        ))
        .with_context(|| format!("DPI stage table write failed ({} slot)", slot_name(store)))?;
    }
    Ok(())
}

/// `[store, active_stage, count]`, then 7 bytes per stage: 1-based index,
/// X and Y big-endian (same value on both axes), two reserved zero bytes.
fn dpi_stages_args(store: u8, active: u8, stages: &[u16]) -> Vec<u8> {
    let mut args = vec![store, active, stages.len() as u8];
    for (i, dpi) in stages.iter().enumerate() {
        let [hi, lo] = dpi.to_be_bytes();
        args.extend_from_slice(&[(i + 1) as u8, hi, lo, hi, lo, 0x00, 0x00]);
    }
    args
}

// ---------- Actions ----------

fn main() -> Result<()> {
    let cli = Cli::parse();
    let action = cli.action();
    let dock = HidrawDevice::open_dock()?;

    match action {
        Action::Check => run_check(&dock),
        Action::Color(c) => run_color(&dock, c),
        Action::Battery => run_battery(&dock),
        Action::Info => run_info(&dock),
        Action::Sniff => run_sniff(&dock),
        Action::Watch(c) => run_watch(&dock, c),
        Action::Sensitivity(d) => run_sensitivity(&dock, d),
        Action::SensitivityStages(on) => run_sensitivity_stages(&dock, on),
    }
}

/// The free slider: pin the sensitivity to one DPI value and collapse the
/// stage table to it, so the Cycle Up Sensitivity Stages button is inert —
/// nothing on the mouse can change the value anymore.
fn run_sensitivity(dock: &HidrawDevice, dpi: u16) -> Result<()> {
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
fn run_sensitivity_stages(dock: &HidrawDevice, enabled: bool) -> Result<()> {
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

/// Push `color` to the dock ring and, via RF, to the mouse. Silent so it can be
/// called repeatedly by `--watch`.
fn apply_color(dock: &HidrawDevice, color: ColorName) -> Result<()> {
    let label = color.as_str();
    dock.send_feature(&dock_rgb_report(color.rgb()))
        .with_context(|| format!("failed to set dock color '{label}'"))?;
    // Sent through the dock; if the mouse is not paired, the dock drops it silently.
    dock.send_feature(&mouse_via_dock_rgb_report(color.rgb()))
        .with_context(|| format!("failed to set mouse color '{label}'"))?;
    Ok(())
}

fn run_color(dock: &HidrawDevice, color: ColorName) -> Result<()> {
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
fn run_sniff(dock: &HidrawDevice) -> Result<()> {
    println!("Sniffing input reports from {}.", dock.path.display());
    println!("Exercise the mouse: let it sleep, then move it to wake it.");
    println!("Press Ctrl-C to stop.\n");

    let start = Instant::now();
    let mut buf = [0u8; 256];
    loop {
        let n = dock.read_input_report(&mut buf)?;
        if n == 0 {
            continue;
        }
        let elapsed = start.elapsed().as_secs_f64();
        let hex: Vec<String> = buf[..n].iter().map(|b| format!("{b:02x}")).collect();
        println!("[{elapsed:8.3}s] {n:3} bytes: {}", hex.join(" "));
    }
}

/// Should we re-apply the color when an input report arrives after `idle_gap`
/// of silence? Only when the gap is long enough to mean the mouse actually
/// slept — a brief pause in movement must not trigger a re-apply.
fn should_reapply_on_wake(idle_gap: Duration) -> bool {
    idle_gap >= WAKE_IDLE_THRESHOLD
}

/// Hold `color` persistently: re-apply it the moment the mouse wakes (detected
/// as input resuming after a quiet gap) and, while the mouse is in use, on a
/// slow safety cadence to correct any spontaneous drift. Runs until the process
/// is signalled (Ctrl-C, or `systemctl stop`).
fn run_watch(dock: &HidrawDevice, color: ColorName) -> Result<()> {
    apply_color(dock, color).context("initial color apply failed")?;
    println!(
        "Watching {} — holding '{}', re-applying on wake. Ctrl-C to stop.",
        dock.path.display(),
        color.as_str()
    );

    let mut pfd = libc::pollfd {
        fd: dock.file.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = WATCH_SAFETY_INTERVAL.as_millis() as libc::c_int;

    let mut buf = [0u8; 256];
    let mut last_input = Instant::now();
    let mut active_since_safety = false;

    loop {
        // SAFETY: one valid pollfd over an owned fd; the kernel only writes
        // `revents`. A negative return means error, with errno set.
        let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue; // EINTR (benign signal) — just retry the poll
            }
            bail!("poll on {} failed: {err}", dock.path.display());
        }

        if ret == 0 {
            // Safety cadence elapsed. Re-apply only if the mouse has been used
            // since the last safety apply, so a sleeping/absent mouse is free.
            if active_since_safety {
                apply_color(dock, color).context("safety re-apply failed")?;
                active_since_safety = false;
                println!("re-applied '{}' (safety refresh)", color.as_str());
            }
            continue;
        }

        // Input is ready, so this read won't block. Handling one report per
        // iteration is fine — a burst just makes the next poll return at once.
        let n = dock.read_input_report(&mut buf)?;
        if n == 0 {
            continue;
        }
        let now = Instant::now();
        let idle_gap = now.duration_since(last_input);
        last_input = now;
        active_since_safety = true;

        if should_reapply_on_wake(idle_gap) {
            apply_color(dock, color).context("wake re-apply failed")?;
            println!(
                "re-applied '{}' (mouse woke after {:.0}s idle)",
                color.as_str(),
                idle_gap.as_secs_f64()
            );
        }
    }
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
    print_field("Stages", query_dpi_stages(dock).ok());
    print_field("Profile", query_profiles(dock).ok());

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
        let q = build_query(
            TX_ID_MOUSE,
            CLASS_DPI,
            CMD_GET_DPI,
            0x07,
            &[0x01, 0x02, 0x03],
        );
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
    fn set_dpi_query_layout_is_big_endian_both_axes() {
        let q = build_query(
            TX_ID_MOUSE,
            CLASS_DPI,
            CMD_SET_DPI,
            0x07,
            &[VARSTORE, 0x07, 0x08, 0x07, 0x08],
        );
        assert_eq!(q[1], TX_ID_MOUSE);
        assert_eq!(q[5], 0x07);
        assert_eq!(q[6], CLASS_DPI);
        assert_eq!(q[7], CMD_SET_DPI);
        // varstore, then 1800 (0x0708) big-endian on both axes.
        assert_eq!(&q[8..13], &[0x01, 0x07, 0x08, 0x07, 0x08]);
        assert_eq!(q[88], compute_crc(&q));
    }

    #[test]
    fn lock_collapses_stage_table_to_one_entry() {
        // 1800 (0x0708) as the only stage: store, active=1, count=1, then
        // stage index 1 with X/Y big-endian and two reserved bytes.
        let args = dpi_stages_args(VARSTORE, 1, &[1800]);
        assert_eq!(
            args,
            vec![0x01, 0x01, 0x01, 0x01, 0x07, 0x08, 0x07, 0x08, 0x00, 0x00]
        );

        let q = build_query(
            TX_ID_MOUSE,
            CLASS_DPI,
            CMD_SET_DPI_STAGES,
            DPI_STAGES_DATA_SIZE,
            &args,
        );
        assert_eq!(q[5], 0x26);
        assert_eq!(q[6], CLASS_DPI);
        assert_eq!(q[7], CMD_SET_DPI_STAGES);
        assert_eq!(q[88], compute_crc(&q));
    }

    #[test]
    fn unlock_writes_the_default_stage_table() {
        let args = dpi_stages_args(LIVESTORE, DEFAULT_DPI_ACTIVE_STAGE, &DEFAULT_DPI_STAGES);
        assert_eq!(args.len(), DPI_STAGES_DATA_SIZE as usize);
        assert_eq!(&args[..3], &[0x00, 3, 5]); // live slot, stage 3 of 5 active
        // Stage 3 entry: index 3, 1600 (0x0640) on both axes.
        assert_eq!(
            &args[3 + 2 * 7..3 + 3 * 7],
            &[3, 0x06, 0x40, 0x06, 0x40, 0, 0]
        );
        // Stage 5 entry: index 5, 6400 (0x1900).
        assert_eq!(&args[3 + 4 * 7..], &[5, 0x19, 0x00, 0x19, 0x00, 0, 0]);
    }

    #[test]
    fn dpi_stages_display_shows_lock_state() {
        let locked = DpiStages {
            active: 1,
            stages: vec![1800],
        };
        assert_eq!(locked.to_string(), "🔒 off — locked at 1800");

        let unlocked = DpiStages {
            active: 3,
            stages: vec![400, 800, 1600, 3200, 6400],
        };
        assert_eq!(unlocked.to_string(), "🔄 on — 400/800/[1600]/3200/6400");
    }

    #[test]
    fn parse_dpi_stages_mirrors_the_args_layout() {
        // Round-trip: a response whose args are exactly what we would write.
        let mut resp = [0u8; REPORT_LEN];
        resp[8..8 + 3 + 2 * 7].copy_from_slice(&dpi_stages_args(LIVESTORE, 2, &[400, 800]));
        let parsed = parse_dpi_stages(&resp);
        assert_eq!(parsed.active, 2);
        assert_eq!(parsed.stages, vec![400, 800]);

        // A garbage count must not read past the 5-entry table.
        resp[10] = 0xFF;
        assert_eq!(parse_dpi_stages(&resp).stages.len(), 5);
    }

    #[test]
    fn profile_info_formats_with_indicator_color() {
        assert_eq!(
            ProfileInfo {
                active: 4,
                count: 5
            }
            .to_string(),
            "4 (blue) of 5"
        );
        assert_eq!(
            ProfileInfo {
                active: 1,
                count: 5
            }
            .to_string(),
            "1 (white) of 5"
        );
        // Unknown slots fall back to the bare number.
        assert_eq!(
            ProfileInfo {
                active: 9,
                count: 5
            }
            .to_string(),
            "9 of 5"
        );
    }

    #[test]
    fn format_dpi_collapses_identical_axes() {
        assert_eq!(format_dpi((1800, 1800)), "1800");
        assert_eq!(format_dpi((1600, 800)), "1600 / 800");
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

    #[test]
    fn watch_reapplies_only_after_a_real_idle_gap() {
        // Brief pauses in movement must not re-apply.
        assert!(!should_reapply_on_wake(Duration::from_millis(500)));
        assert!(!should_reapply_on_wake(
            WAKE_IDLE_THRESHOLD - Duration::from_millis(1)
        ));
        // A gap at/above the threshold means the mouse slept → re-apply.
        assert!(should_reapply_on_wake(WAKE_IDLE_THRESHOLD));
        assert!(should_reapply_on_wake(Duration::from_secs(120)));
    }

    #[test]
    fn classify_response_status_maps_codes() {
        assert_eq!(classify_response_status(STATUS_OK), ResponseStatus::Ready);
        assert_eq!(
            classify_response_status(STATUS_NEW),
            ResponseStatus::Pending
        );
        assert_eq!(
            classify_response_status(STATUS_BUSY),
            ResponseStatus::Pending
        );
        // 0x03 failure, 0x04 no-response, 0x05 unsupported — all terminal.
        assert_eq!(classify_response_status(0x03), ResponseStatus::Failed(0x03));
        assert_eq!(classify_response_status(0x05), ResponseStatus::Failed(0x05));
    }

    /// Regression test: HIDIOCSFEATURE for a 91-byte buffer must match what
    /// the Linux kernel expects (`_IOC(_IOC_WRITE|_IOC_READ, 'H', 0x06, 91)`).
    #[test]
    fn hidioc_codes_match_kernel_encoding() {
        assert_eq!(hidioc_set_feature(91), 0xC05B_4806);
        assert_eq!(hidioc_get_feature(91), 0xC05B_4807);
    }
}
