//! Razer report layer: the 90-byte feature-report format, the commands this
//! hardware understands, and typed queries/writes built on [`HidrawDevice`].

use anyhow::{Context, Result};

use crate::hid::HidrawDevice;

// Razer HID feature report fixed size.
pub(crate) const REPORT_LEN: usize = 90;

// The 1-byte XOR checksum sits at this offset and covers bytes `2..CRC_INDEX`.
pub(crate) const CRC_INDEX: usize = 88;

// Transaction id conventions observed on this hardware:
// * `TX_ID_DOCK_LED`/`TX_ID_DOCK`   — target the dock itself
// * `TX_ID_MOUSE`                   — dock forwards the request over RF to the mouse
pub(crate) const TX_ID_MOUSE: u8 = 0x1F;
pub(crate) const TX_ID_DOCK: u8 = 0xFF;
pub(crate) const TX_ID_DOCK_LED: u8 = 0xF7;

pub(crate) const CLASS_DEVICE: u8 = 0x00;
pub(crate) const CMD_GET_FIRMWARE: u8 = 0x81;
pub(crate) const CMD_GET_SERIAL: u8 = 0x82;

// First argument of DPI get/set: which storage slot to address. The firmware
// runs from the live (RAM) slot — the Cycle Up Sensitivity Stages button
// cycles that one — while the stored slot survives sleep and power-cycles.
// Writes must hit both to be effective now AND persistent; reads of the live
// slot show what the sensor actually runs at.
pub(crate) const LIVESTORE: u8 = 0x00;
pub(crate) const VARSTORE: u8 = 0x01;

pub(crate) const CLASS_DPI: u8 = 0x04;
pub(crate) const CMD_SET_DPI: u8 = 0x05;
pub(crate) const CMD_GET_DPI: u8 = 0x85;
pub(crate) const CMD_SET_DPI_STAGES: u8 = 0x06;
pub(crate) const CMD_GET_DPI_STAGES: u8 = 0x86;

// Stage table layout: [store, active_stage (1-based), count] then per stage
// 7 bytes: index, X hi/lo, Y hi/lo, 2 reserved. 3 + 5*7 = 38 = 0x26.
pub(crate) const DPI_STAGES_DATA_SIZE: u8 = 0x26;

// The stage table `--sensitivity-stages on` installs — Synapse's defaults for
// "Cycle Up Sensitivity Stages", stage 3 (1600) active. The firmware caps the
// table at 5 stages: a 6th is rejected with status 0x03.
pub(crate) const DEFAULT_DPI_STAGES: [u16; 5] = [400, 800, 1600, 3200, 6400];
pub(crate) const DEFAULT_DPI_ACTIVE_STAGE: u8 = 3;

// Sensor limits of the Basilisk V3 Pro 35K (the "35K" is the max DPI).
pub(crate) const DPI_MIN: u16 = 100;
pub(crate) const DPI_MAX: u16 = 35_000;

// Onboard profile slots, cycled with the button under the mouse. The
// indicator LED next to it shows the active slot's color.
pub(crate) const CLASS_PROFILE: u8 = 0x05;
pub(crate) const CMD_GET_PROFILE_COUNT: u8 = 0x80;
pub(crate) const CMD_GET_ACTIVE_PROFILE: u8 = 0x84;

pub(crate) const CLASS_POWER: u8 = 0x07;
pub(crate) const CMD_GET_BATTERY_LEVEL: u8 = 0x80;
pub(crate) const CMD_GET_CHARGING: u8 = 0x84;

pub(crate) const CLASS_EXTENDED_MATRIX: u8 = 0x0F;
pub(crate) const CMD_SET_MATRIX_EFFECT: u8 = 0x03;

// Dock LED ring layout.
pub(crate) const DOCK_LED_DATA_SIZE: u8 = 0x1D;
pub(crate) const DOCK_LED_ZONES: usize = 8;
pub(crate) const DOCK_LED_COUNT_MINUS_ONE: u8 = (DOCK_LED_ZONES as u8) - 1;

// Basilisk V3 Pro 35K LED layout when routed via the dock.
pub(crate) const MOUSE_LED_DATA_SIZE: u8 = 0x2C;
pub(crate) const MOUSE_LED_ZONES: usize = 13;
pub(crate) const MOUSE_LED_COUNT_MINUS_ONE: u8 = (MOUSE_LED_ZONES as u8) - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rgb {
    pub(crate) red: u8,
    pub(crate) green: u8,
    pub(crate) blue: u8,
}

impl Rgb {
    pub(crate) const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }
}

// ---------- Report builders ----------

/// XOR of bytes `2..CRC_INDEX`, the Razer HID report checksum.
pub(crate) fn compute_crc(bytes: &[u8; REPORT_LEN]) -> u8 {
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
pub(crate) fn dock_rgb_report(color: Rgb) -> [u8; REPORT_LEN] {
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
pub(crate) fn mouse_via_dock_rgb_report(color: Rgb) -> [u8; REPORT_LEN] {
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

/// Build a query report: `[_, tx_id, 0, 0, 0, data_size, class, cmd, args..., _, crc, _]`.
pub(crate) fn build_query(
    tx_id: u8,
    class: u8,
    cmd: u8,
    data_size: u8,
    args: &[u8],
) -> [u8; REPORT_LEN] {
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

// ---------- Queries ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BatteryStatus {
    pub(crate) percent: u8,
    pub(crate) charging: bool,
}

pub(crate) fn query_battery(dock: &HidrawDevice) -> Result<BatteryStatus> {
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

pub(crate) fn query_serial(dock: &HidrawDevice, tx_id: u8) -> Result<String> {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FirmwareVersion {
    major: u8,
    minor: u8,
}

impl std::fmt::Display for FirmwareVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{:02}", self.major, self.minor)
    }
}

pub(crate) fn query_firmware(dock: &HidrawDevice, tx_id: u8) -> Result<FirmwareVersion> {
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
pub(crate) struct ProfileInfo {
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

pub(crate) fn query_profiles(dock: &HidrawDevice) -> Result<ProfileInfo> {
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

pub(crate) fn query_dpi(dock: &HidrawDevice) -> Result<(u16, u16)> {
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

/// `1800` when both axes match, `1600 / 800` when they differ.
pub(crate) fn format_dpi((x, y): (u16, u16)) -> String {
    if x == y {
        format!("{x}")
    } else {
        format!("{x} / {y}")
    }
}

// ---------- DPI stage table ----------

/// Onboard stage table state, shown by `--info` as the lock indicator: one
/// stage means the sensitivity is pinned (the Cycle Up Sensitivity Stages
/// button has nothing to cycle to), several mean the button is live.
pub(crate) struct DpiStages {
    pub(crate) active: u8,
    pub(crate) stages: Vec<(u16, u16)>,
}

impl std::fmt::Display for DpiStages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.stages[..] {
            [] => return f.write_str("—"), // defensive: a garbled table
            [only] => return write!(f, "🔒 off — locked at {}", format_dpi(only)),
            _ => {}
        }
        // Not the open-padlock glyph: it is indistinguishable from 🔒 in
        // some terminal fonts. The cycle arrows say what the button does.
        write!(f, "🔄 on — ")?;
        for (i, &stage) in self.stages.iter().enumerate() {
            if i > 0 {
                f.write_str("/")?;
            }
            if (i + 1) as u8 == self.active {
                write!(f, "[{}]", format_dpi(stage))?;
            } else {
                f.write_str(&format_dpi(stage))?;
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
            (
                u16::from_be_bytes([resp[entry + 1], resp[entry + 2]]),
                u16::from_be_bytes([resp[entry + 3], resp[entry + 4]]),
            )
        })
        .collect();
    DpiStages { active, stages }
}

pub(crate) fn query_dpi_stages(dock: &HidrawDevice) -> Result<DpiStages> {
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

/// Write the DPI to the live and stored slots — the report carries X and Y as
/// big-endian u16 pairs (same layout as the GET response); we drive both axes
/// with the same value.
pub(crate) fn set_dpi(dock: &HidrawDevice, dpi: u16) -> Result<()> {
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
pub(crate) fn set_dpi_stages(dock: &HidrawDevice, active: u8, stages: &[u16]) -> Result<()> {
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
            stages: vec![(1800, 1800)],
        };
        assert_eq!(locked.to_string(), "🔒 off — locked at 1800");

        // Asymmetric axes must not be silently collapsed.
        let asymmetric = DpiStages {
            active: 1,
            stages: vec![(1600, 800)],
        };
        assert_eq!(asymmetric.to_string(), "🔒 off — locked at 1600 / 800");

        let unlocked = DpiStages {
            active: 3,
            stages: vec![
                (400, 400),
                (800, 800),
                (1600, 1600),
                (3200, 3200),
                (6400, 6400),
            ],
        };
        assert_eq!(unlocked.to_string(), "🔄 on — 400/800/[1600]/3200/6400");

        // Defensive arm: a garbled (empty) table renders as the same "absent"
        // marker print_field uses.
        let garbled = DpiStages {
            active: 0,
            stages: vec![],
        };
        assert_eq!(garbled.to_string(), "—");
    }

    #[test]
    fn parse_dpi_stages_mirrors_the_args_layout() {
        // Round-trip: a response whose args are exactly what we would write.
        let mut resp = [0u8; REPORT_LEN];
        resp[8..8 + 3 + 2 * 7].copy_from_slice(&dpi_stages_args(LIVESTORE, 2, &[400, 800]));
        let parsed = parse_dpi_stages(&resp);
        assert_eq!(parsed.active, 2);
        assert_eq!(parsed.stages, vec![(400, 400), (800, 800)]);

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
}
