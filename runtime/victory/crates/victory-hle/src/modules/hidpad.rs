//! Sony pads (DualShock 4, DualSense, DualSense Edge) read straight from
//! HID, for hosts with no Steam Input or DS4Windows in between. Off unless
//! `VICTORY_PAD_PS=1`.
//!
//! This file is plain data and no I/O: the input report parsers (USB and
//! Bluetooth), the mapping to the console pad, the rumble output reports,
//! and the rules that pick which pads the title sees beside XInput's.
//! `hidpad/win.rs` enumerates and talks to the devices.
//!
//! Report layouts (offsets are into the report as read, report ID first):
//!
//! | model      | report | `common` starts at |
//! |------------|--------|--------------------|
//! | DS4        | 0x01 (USB, or BT before full mode) | 1 |
//! | DS4        | 0x11 (BT full mode)                | 3 |
//! | DualSense  | 0x01 (USB)                         | 1 |
//! | DualSense  | 0x31 (BT full mode)                | 2 |
//!
//! DS4 `common`: LX LY RX RY, three button bytes, L2, R2. DualSense
//! `common`: LX LY RX RY, L2, R2, counter, four button bytes. Bluetooth
//! reports carry a CRC-32 which we do not check (a bad one is a rare glitch
//! the next report repairs), but output reports need a valid one.
//!
//! Nothing here applies a dead zone, as for XInput pads: the title applies
//! its own, and the `[controls] stick_deadzone` setting belongs to the mouse
//! standing in for the right stick.

use std::collections::HashMap;

use victory_core::kbm::{
    A, B, DOWN, LEFT, LEFT_SHOULDER, LEFT_STICK, MENU, PadState, RIGHT, RIGHT_SHOULDER,
    RIGHT_STICK, UP, VIEW, X, Y,
};

#[cfg(windows)]
pub(super) mod win;

pub const SONY: u16 = 0x054C;
const DS4_V1: u16 = 0x05C4;
const DS4_V2: u16 = 0x09CC;
const DUALSENSE: u16 = 0x0CE6;
const DUALSENSE_EDGE: u16 = 0x0DF2;

/// Host pad slots the title can see (the XInput count).
pub const SLOTS: usize = 4;

/// `VICTORY_PAD_PS`: `1` (or `true`) turns it on. Off by default: switching
/// a Bluetooth pad to full mode changes what other programs reading the
/// same pad receive, so it is opt-in.
pub fn enabled(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some("1") | Some("true") | Some("on"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Model {
    Ds4,
    /// DualSense and DualSense Edge: one report layout.
    DualSense,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bus {
    Usb,
    Bluetooth,
}

pub fn model_of(vendor: u16, product: u16) -> Option<Model> {
    if vendor != SONY {
        return None;
    }
    match product {
        DS4_V1 | DS4_V2 => Some(Model::Ds4),
        DUALSENSE | DUALSENSE_EDGE => Some(Model::DualSense),
        _ => None,
    }
}

/// The bus from the longest input report the device declares: 64 bytes over
/// USB for both models, more over Bluetooth (DS4 547, DualSense 78).
pub fn bus_of(input_report_len: usize) -> Bus {
    if input_report_len > 64 { Bus::Bluetooth } else { Bus::Usb }
}

/// Vendor and product from a device path: USB `...hid#vid_054c&pid_09cc#...`,
/// Bluetooth `...hid#{00001124-...}_vid&0002054c_pid&09cc#...`.
pub fn ids_from_path(path: &str) -> Option<(u16, u16)> {
    let path = path.to_ascii_lowercase();
    let field = |name: &str| -> Option<u16> {
        let at = path.find(name)? + name.len();
        let rest = path.get(at + 1..)?; // the separator ('_' or '&')
        let digits: String = rest.chars().take_while(char::is_ascii_hexdigit).collect();
        // Bluetooth prefixes the vendor with its source (0002); the ID is
        // the last four digits.
        let tail = &digits[digits.len().checked_sub(4)?..];
        u16::from_str_radix(tail, 16).ok()
    };
    Some((field("vid")?, field("pid")?))
}

/// One decoded input report.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Input {
    pub pad: PadState,
    /// The PS button. The console pad has no Guide bit, so nothing maps it;
    /// kept for the log and for a later overlay.
    pub guide: bool,
}

/// A stick axis: 0..=255 with the centre at 128, to -1..=1. Exactly 0 at
/// the centre; 0 maps to -1 (128 below, 127 above, as XInput's -32768).
fn axis(v: u8) -> f32 {
    ((f32::from(v) - 128.0) / 127.0).clamp(-1.0, 1.0)
}

/// Pads report y growing downward; the console pad has up positive.
fn axis_up(v: u8) -> f32 {
    0.0 - axis(v)
}

fn trigger(v: u8) -> f32 {
    f32::from(v) / 255.0
}

/// The d-pad hat: 0 north, clockwise to 7 north-west; 8 and up released.
fn hat(h: u8) -> u32 {
    match h & 0x0F {
        0 => UP,
        1 => UP | RIGHT,
        2 => RIGHT,
        3 => DOWN | RIGHT,
        4 => DOWN,
        5 => DOWN | LEFT,
        6 => LEFT,
        7 => UP | LEFT,
        _ => 0,
    }
}

/// The button bytes shared by both models (DS4 bytes 0 and 1; DualSense
/// bytes 0 and 1): hat and face buttons, then shoulders, share, options,
/// sticks.
fn map_buttons(b0: u8, b1: u8, b2: u8) -> (u32, bool) {
    let mut out = hat(b0);
    for (bit, button) in [(0x10, X), (0x20, A), (0x40, B), (0x80, Y)] {
        // Square, cross, circle, triangle.
        if b0 & bit != 0 {
            out |= button;
        }
    }
    for (bit, button) in [
        (0x01, LEFT_SHOULDER),
        (0x02, RIGHT_SHOULDER),
        (0x10, VIEW), // Share / Create
        (0x20, MENU), // Options
        (0x40, LEFT_STICK),
        (0x80, RIGHT_STICK),
    ] {
        if b1 & bit != 0 {
            out |= button;
        }
    }
    // The touchpad click is a second View.
    if b2 & 0x02 != 0 {
        out |= VIEW;
    }
    (out, b2 & 0x01 != 0)
}

fn ds4(c: &[u8]) -> Input {
    let (buttons, guide) = map_buttons(c[4], c[5], c[6]);
    Input {
        pad: PadState {
            buttons,
            left_trigger: trigger(c[7]),
            right_trigger: trigger(c[8]),
            left_x: axis(c[0]),
            left_y: axis_up(c[1]),
            right_x: axis(c[2]),
            right_y: axis_up(c[3]),
        },
        guide,
    }
}

fn dualsense(c: &[u8]) -> Input {
    let (buttons, guide) = map_buttons(c[7], c[8], c[9]);
    Input {
        pad: PadState {
            buttons,
            left_trigger: trigger(c[4]),
            right_trigger: trigger(c[5]),
            left_x: axis(c[0]),
            left_y: axis_up(c[1]),
            right_x: axis(c[2]),
            right_y: axis_up(c[3]),
        },
        guide,
    }
}

/// Decodes one input report as `ReadFile` returns it (report ID first, zero
/// padded to the declared length). `None`: a report we do not use (audio,
/// too short, an unknown ID).
pub fn parse(model: Model, bus: Bus, report: &[u8]) -> Option<Input> {
    let at = |start: usize, need: usize| report.get(start..start + need);
    match (model, *report.first()?) {
        // DS4 USB, and the DS4's short Bluetooth report (same leading bytes).
        (Model::Ds4, 0x01) => at(1, 9).map(ds4),
        (Model::Ds4, 0x11) => at(3, 9).map(ds4),
        (Model::DualSense, 0x01) if bus == Bus::Usb => at(1, 11).map(dualsense),
        // A Bluetooth DualSense before full mode: the DS4 short layout.
        (Model::DualSense, 0x01) => at(1, 9).map(ds4),
        (Model::DualSense, 0x31) => at(2, 11).map(dualsense),
        _ => None,
    }
}

/// Whether a report is the short Bluetooth one: the pad still has to be
/// switched to full mode.
pub fn needs_full_mode(model: Model, bus: Bus, report: &[u8]) -> bool {
    // Both models start a Bluetooth link in the short report 0x01.
    let _ = model;
    bus == Bus::Bluetooth && report.first() == Some(&0x01)
}

/// CRC-32 (IEEE, reflected), as Bluetooth output reports carry.
pub fn crc32(seed: &[u8], data: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for &byte in seed.iter().chain(data) {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// The title's motor speeds (0..=65535) as a pad byte.
pub fn motor(speed: u16) -> u8 {
    (speed >> 8) as u8
}

/// DS4 light bar colour per player slot (the console's blue, red, green,
/// pink).
const DS4_COLOURS: [[u8; 3]; SLOTS] = [[0, 0, 64], [64, 0, 0], [0, 64, 0], [48, 0, 24]];
/// DualSense player LED masks for slots 0 to 3.
const DS_PLAYER_LEDS: [u8; SLOTS] = [0x04, 0x0A, 0x15, 0x1B];

/// What an output report sets: both rumble motors, the slot's indicator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Output {
    /// Low-frequency (left, heavy) and high-frequency (right) speeds.
    pub low: u16,
    pub high: u16,
    pub slot: usize,
    /// Bluetooth DualSense sequence number (low nibble used).
    pub seq: u8,
}

/// The output report that sets rumble and the slot indicator. Adaptive
/// triggers stay off: the DualSense report carries both trigger blocks
/// zeroed (effect mode "off"). The size is the one the device declares.
pub fn output_report(model: Model, bus: Bus, out: Output) -> Vec<u8> {
    let slot = out.slot.min(SLOTS - 1);
    let (weak, strong) = (motor(out.high), motor(out.low));
    match (model, bus) {
        (Model::Ds4, Bus::Usb) => {
            let mut r = vec![0_u8; 32];
            r[0] = 0x05;
            r[1] = 0x07; // rumble, light bar, flash
            r[4] = weak;
            r[5] = strong;
            r[6..9].copy_from_slice(&DS4_COLOURS[slot]);
            r
        }
        (Model::Ds4, Bus::Bluetooth) => {
            let mut r = vec![0_u8; 78];
            r[0] = 0x11;
            r[1] = 0xC0 | 4; // HID + CRC flags, poll every 4th interval
            r[3] = 0x07;
            r[6] = weak;
            r[7] = strong;
            r[8..11].copy_from_slice(&DS4_COLOURS[slot]);
            sign(&mut r);
            r
        }
        (Model::DualSense, Bus::Usb) => {
            let mut r = vec![0_u8; 48];
            r[0] = 0x02;
            dualsense_common(&mut r[1..48], slot, motor(out.high), motor(out.low));
            r
        }
        (Model::DualSense, Bus::Bluetooth) => {
            let mut r = vec![0_u8; 78];
            r[0] = 0x31;
            r[1] = (out.seq & 0x0F) << 4;
            r[2] = 0x10; // output tag
            dualsense_common(&mut r[3..50], slot, motor(out.high), motor(out.low));
            sign(&mut r);
            r
        }
    }
}

/// The DualSense output body (47 bytes after the report ID, on both buses).
fn dualsense_common(c: &mut [u8], slot: usize, right: u8, left: u8) {
    // Valid flags 0: compatible vibration, haptics select, and both trigger
    // blocks (left all zero: effect off).
    c[0] = 0x0F;
    // Valid flags 1: player LEDs.
    c[1] = 0x10;
    c[2] = right;
    c[3] = left;
    // Valid flags 2: compatible vibration 2 (new firmware).
    c[38] = 0x04;
    c[43] = DS_PLAYER_LEDS[slot];
}

/// Appends the Bluetooth output CRC: over 0xA2 and the report before it.
fn sign(report: &mut [u8]) {
    let end = report.len() - 4;
    let crc = crc32(&[0xA2], &report[..end]);
    report[end..].copy_from_slice(&crc.to_le_bytes());
}

// ---------------------------------------------------------------------------
// Which pads the title sees.

/// Who fills a host slot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Owner {
    #[default]
    Empty,
    XInput,
    /// A HID pad, by the id the device layer gave it.
    Hid(u64),
}

/// How sure we are that a HID pad is also an XInput pad (Steam Input or
/// DS4Windows reading it and presenting an Xbox pad).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Verdict {
    /// No evidence yet: hidden while any XInput pad is connected.
    #[default]
    Unknown,
    /// Moved with no XInput pad following it: a pad of its own.
    Separate,
    /// An XInput pad has followed it long enough: the same pad.
    Mirrored,
}

/// Needed polls (8 ms each) of the HID pad in use with an XInput pad
/// doing the same, to call it the same pad (about half a second).
const MIRROR_POLLS: u32 = 60;
/// How long the HID pad may be in use with no XInput pad following it
/// before it is called a separate pad.
const SEPARATE_AFTER_MS: u64 = 300;

fn active(p: &PadState) -> bool {
    p.buttons != 0
        || p.left_trigger > 0.25
        || p.right_trigger > 0.25
        || p.left_x.hypot(p.left_y) > 0.4
        || p.right_x.hypot(p.right_y) > 0.4
}

/// `x` is doing what `h` does: the same buttons, or a trigger or stick
/// pushed the same way (Steam and DS4Windows reshape sticks, so only the
/// direction counts).
fn follows(h: &PadState, x: &PadState) -> bool {
    let same_way = |(ax, ay): (f32, f32), (bx, by): (f32, f32)| {
        let (am, bm) = (ax.hypot(ay), bx.hypot(by));
        am > 0.4 && bm > 0.2 && (ax * bx + ay * by) > 0.5 * am * bm
    };
    (h.buttons != 0 && h.buttons == x.buttons)
        || (h.left_trigger > 0.25 && x.left_trigger > 0.2)
        || (h.right_trigger > 0.25 && x.right_trigger > 0.2)
        || same_way((h.left_x, h.left_y), (x.left_x, x.left_y))
        || same_way((h.right_x, h.right_y), (x.right_x, x.right_y))
}

#[derive(Default)]
struct Mirror {
    verdict: Verdict,
    followed: u32,
    /// Since when the pad has been in use with nothing following it.
    unmatched_since: Option<u64>,
}

impl Mirror {
    fn observe(&mut self, now_ms: u64, hid: &PadState, xinput: &[PadState]) {
        // Both verdicts are final.
        if self.verdict != Verdict::Unknown {
            return;
        }
        if xinput.is_empty() || !active(hid) {
            // Nothing to compare with, or nothing happening.
            self.unmatched_since = None;
            return;
        }
        if xinput.iter().any(|x| follows(hid, x)) {
            self.unmatched_since = None;
            self.followed += 1;
            if self.followed >= MIRROR_POLLS {
                self.verdict = Verdict::Mirrored;
            }
        } else {
            let since = *self.unmatched_since.get_or_insert(now_ms);
            if now_ms.saturating_sub(since) >= SEPARATE_AFTER_MS {
                self.verdict = Verdict::Separate;
            }
        }
    }

    /// Whether the title may see the pad. `xinput_pads`: connected XInput
    /// pads. A mirrored pad never; one of its own always; one nobody knows
    /// yet only while XInput has no pad it could be hiding behind.
    fn visible(&self, xinput_pads: usize) -> bool {
        match self.verdict {
            Verdict::Mirrored => false,
            Verdict::Separate => true,
            Verdict::Unknown => xinput_pads == 0,
        }
    }
}

/// XInput pads and HID pads into the four host slots. XInput keeps its own
/// slot numbers (the title's player order); a HID pad takes the lowest
/// slot XInput is not using and keeps it while it can, so a pad plugged in
/// later never moves one already playing.
#[derive(Default)]
pub struct Merger {
    owners: [Owner; SLOTS],
    mirrors: HashMap<u64, Mirror>,
}

impl Merger {
    pub fn owner(&self, slot: usize) -> Owner {
        self.owners.get(slot).copied().unwrap_or_default()
    }

    /// The slot a HID pad holds now.
    pub fn slot_of(&self, id: u64) -> Option<usize> {
        self.owners.iter().position(|o| *o == Owner::Hid(id))
    }

    /// One poll: `xinput` per slot, `hid` the live pads in connection order.
    pub fn merge(
        &mut self,
        now_ms: u64,
        xinput: [Option<PadState>; SLOTS],
        hid: &[(u64, PadState)],
    ) -> [Option<PadState>; SLOTS] {
        let present: Vec<PadState> = xinput.iter().flatten().copied().collect();
        let mut shown = Vec::new();
        for (id, state) in hid {
            let mirror = self.mirrors.entry(*id).or_default();
            mirror.observe(now_ms, state, &present);
            if mirror.visible(present.len()) {
                shown.push((*id, *state));
            }
        }
        // Forget unplugged pads: a pad plugged in again is judged afresh.
        self.mirrors.retain(|id, _| hid.iter().any(|(h, _)| h == id));

        let mut out = xinput;
        let mut owners = xinput.map(|p| if p.is_some() { Owner::XInput } else { Owner::Empty });
        let mut placed = Vec::new();
        for slot in 0..SLOTS {
            if let (Owner::Hid(id), Owner::Empty) = (self.owners[slot], owners[slot])
                && let Some((_, state)) = shown.iter().find(|(h, _)| *h == id)
            {
                out[slot] = Some(*state);
                owners[slot] = Owner::Hid(id);
                placed.push(id);
            }
        }
        for (id, state) in &shown {
            if placed.contains(id) {
                continue;
            }
            if let Some(slot) = owners.iter().position(|o| *o == Owner::Empty) {
                out[slot] = Some(*state);
                owners[slot] = Owner::Hid(*id);
            }
        }
        self.owners = owners;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DS4 `common` block: sticks, three button bytes, triggers.
    fn ds4_common(sticks: [u8; 4], b: [u8; 3], l2: u8, r2: u8) -> Vec<u8> {
        let mut c = sticks.to_vec();
        c.extend(b);
        c.extend([l2, r2]);
        c
    }

    fn ds4_usb(common: &[u8]) -> Vec<u8> {
        let mut r = vec![0x01];
        r.extend(common);
        r.resize(64, 0);
        r
    }

    fn ds4_bt(common: &[u8]) -> Vec<u8> {
        let mut r = vec![0x11, 0xC0, 0x20];
        r.extend(common);
        r.resize(78, 0);
        r
    }

    fn ds_common(sticks: [u8; 4], l2: u8, r2: u8, b: [u8; 3]) -> Vec<u8> {
        let mut c = sticks.to_vec();
        c.extend([l2, r2, 0x42]); // counter
        c.extend(b);
        c.push(0); // b3
        c
    }

    fn ds_usb(common: &[u8]) -> Vec<u8> {
        let mut r = vec![0x01];
        r.extend(common);
        r.resize(64, 0);
        r
    }

    fn ds_bt(common: &[u8]) -> Vec<u8> {
        let mut r = vec![0x31, 0x01];
        r.extend(common);
        r.resize(78, 0);
        r
    }

    const CENTRE: [u8; 4] = [128; 4];

    #[test]
    fn idle_reports_are_an_idle_pad_on_every_model_and_bus() {
        let c = ds4_common(CENTRE, [0x08, 0, 0], 0, 0);
        let want = Input::default();
        assert_eq!(parse(Model::Ds4, Bus::Usb, &ds4_usb(&c)), Some(want));
        assert_eq!(parse(Model::Ds4, Bus::Bluetooth, &ds4_bt(&c)), Some(want));
        assert_eq!(parse(Model::Ds4, Bus::Bluetooth, &ds4_usb(&c)), Some(want), "short BT report");
        let c = ds_common(CENTRE, 0, 0, [0x08, 0, 0]);
        assert_eq!(parse(Model::DualSense, Bus::Usb, &ds_usb(&c)), Some(want));
        assert_eq!(parse(Model::DualSense, Bus::Bluetooth, &ds_bt(&c)), Some(want));
    }

    #[test]
    fn buttons_map_the_same_on_both_models() {
        // Cross + Circle, hat north-east, L1 + R3, Options + touchpad click, PS.
        let want = A | B | UP | RIGHT | LEFT_SHOULDER | RIGHT_STICK | MENU | VIEW;
        let ds4 = ds4_common(CENTRE, [0x01 | 0x20 | 0x40, 0x01 | 0x80 | 0x20, 0x03], 0, 0);
        let ds = ds_common(CENTRE, 0, 0, [0x01 | 0x20 | 0x40, 0x01 | 0x80 | 0x20, 0x03]);
        for input in [
            parse(Model::Ds4, Bus::Usb, &ds4_usb(&ds4)),
            parse(Model::Ds4, Bus::Bluetooth, &ds4_bt(&ds4)),
            parse(Model::DualSense, Bus::Usb, &ds_usb(&ds)),
            parse(Model::DualSense, Bus::Bluetooth, &ds_bt(&ds)),
        ] {
            let input = input.expect("parsed");
            assert_eq!(input.pad.buttons, want);
            assert!(input.guide, "PS button");
        }
    }

    #[test]
    fn square_triangle_share_and_sticks_click() {
        let c = ds4_common(CENTRE, [0x08 | 0x10 | 0x80, 0x10 | 0x40 | 0x02 | 0x04 | 0x08, 0], 0, 0);
        let got = parse(Model::Ds4, Bus::Usb, &ds4_usb(&c)).unwrap();
        // L2/R2 digital bits are not buttons: the analog triggers carry them.
        assert_eq!(got.pad.buttons, X | Y | VIEW | LEFT_STICK | RIGHT_SHOULDER);
        assert!(!got.guide);
    }

    #[test]
    fn the_hat_covers_all_eight_directions_and_released() {
        let want = [
            UP,
            UP | RIGHT,
            RIGHT,
            DOWN | RIGHT,
            DOWN,
            DOWN | LEFT,
            LEFT,
            UP | LEFT,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        for (h, want) in want.into_iter().enumerate() {
            assert_eq!(hat(h as u8), want, "hat {h}");
            // The upper nibble (face buttons) never leaks into the hat.
            assert_eq!(hat(h as u8 | 0xF0), want);
        }
    }

    #[test]
    fn sticks_and_triggers_scale_to_the_console_ranges() {
        // Left stick far left and up (y 0 is up), right far right and down.
        let c = ds4_common([0, 0, 255, 255], [0x08, 0, 0], 255, 51);
        let p = parse(Model::Ds4, Bus::Usb, &ds4_usb(&c)).unwrap().pad;
        assert_eq!((p.left_x, p.left_y), (-1.0, 1.0));
        assert_eq!((p.right_x, p.right_y), (1.0, -1.0));
        assert_eq!((p.left_trigger, p.right_trigger), (1.0, 0.2));
        // The centre is exactly zero, and a nudge is a small value.
        assert_eq!(axis(128), 0.0);
        assert!((axis(129) - 1.0 / 127.0).abs() < 1e-6);
        assert_eq!(axis(255), 1.0);
        assert_eq!(axis(0), -1.0);
    }

    #[test]
    fn dualsense_triggers_come_before_the_buttons() {
        let c = ds_common([10, 20, 30, 40], 100, 200, [0x08, 0, 0]);
        let p = parse(Model::DualSense, Bus::Usb, &ds_usb(&c)).unwrap().pad;
        assert_eq!(p.left_trigger, 100.0 / 255.0);
        assert_eq!(p.right_trigger, 200.0 / 255.0);
        assert_eq!(p.left_x, axis(10));
        assert_eq!(p.right_y, axis_up(40));
    }

    #[test]
    fn short_and_foreign_reports_are_ignored() {
        let c = ds4_common(CENTRE, [0x08, 0, 0], 0, 0);
        assert_eq!(parse(Model::Ds4, Bus::Usb, &[]), None);
        assert_eq!(parse(Model::Ds4, Bus::Usb, &ds4_usb(&c)[..9]), None);
        assert_eq!(parse(Model::Ds4, Bus::Bluetooth, &ds4_bt(&c)[..11]), None);
        assert_eq!(parse(Model::Ds4, Bus::Usb, &[0x07; 64]), None);
        assert_eq!(parse(Model::DualSense, Bus::Usb, &[0x31; 78][..5]), None);
        // DS4's BT report is not a DualSense's.
        assert_eq!(parse(Model::DualSense, Bus::Bluetooth, &ds4_bt(&c)), None);
    }

    #[test]
    fn the_short_bluetooth_report_asks_for_full_mode() {
        let c = ds4_common(CENTRE, [0x08, 0, 0], 0, 0);
        assert!(needs_full_mode(Model::Ds4, Bus::Bluetooth, &ds4_usb(&c)));
        assert!(!needs_full_mode(Model::Ds4, Bus::Usb, &ds4_usb(&c)));
        assert!(!needs_full_mode(Model::Ds4, Bus::Bluetooth, &ds4_bt(&c)));
        assert!(needs_full_mode(Model::DualSense, Bus::Bluetooth, &[0x01; 78]));
        assert!(!needs_full_mode(Model::DualSense, Bus::Bluetooth, &ds_bt(&[0; 20])));
    }

    #[test]
    fn identifies_the_sony_models_and_the_bus() {
        assert_eq!(model_of(SONY, 0x05C4), Some(Model::Ds4));
        assert_eq!(model_of(SONY, 0x09CC), Some(Model::Ds4));
        assert_eq!(model_of(SONY, 0x0CE6), Some(Model::DualSense));
        assert_eq!(model_of(SONY, 0x0DF2), Some(Model::DualSense));
        assert_eq!(model_of(SONY, 0x0BA0), None);
        assert_eq!(model_of(0x045E, 0x09CC), None);
        assert_eq!(bus_of(64), Bus::Usb);
        assert_eq!(bus_of(78), Bus::Bluetooth);
        assert_eq!(bus_of(547), Bus::Bluetooth);
    }

    #[test]
    fn ids_come_out_of_usb_and_bluetooth_paths() {
        assert_eq!(
            ids_from_path(r"\\?\HID#VID_054C&PID_09CC&MI_03#7&1a2b#{4d1e55b2-f16f-11cf-88cb-001111000030}"),
            Some((0x054C, 0x09CC))
        );
        assert_eq!(
            ids_from_path(
                r"\\?\hid#{00001124-0000-1000-8000-00805f9b34fb}_vid&0002054c_pid&0ce6#9&2c#{4d1e55b2}"
            ),
            Some((0x054C, 0x0CE6))
        );
        assert_eq!(ids_from_path(r"\\?\hid#vid_046d&pid_c52b#"), Some((0x046D, 0xC52B)));
        assert_eq!(ids_from_path(r"\\?\root#system#0000"), None);
    }

    #[test]
    fn the_flag_is_opt_in() {
        assert!(!enabled(None));
        assert!(!enabled(Some("0")));
        assert!(!enabled(Some("")));
        assert!(enabled(Some("1")));
        assert!(enabled(Some(" true ")));
    }

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(&[], b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b"1234", b"56789"), 0xCBF4_3926);
    }

    fn crc_ok(report: &[u8]) -> bool {
        let end = report.len() - 4;
        report[end..] == crc32(&[0xA2], &report[..end]).to_le_bytes()
    }

    #[test]
    fn rumble_goes_out_in_each_models_output_report() {
        let out = Output { low: 0xFFFF, high: 0x8000, slot: 1, seq: 0x13 };

        let r = output_report(Model::Ds4, Bus::Usb, out);
        assert_eq!((r.len(), r[0], r[1]), (32, 0x05, 0x07));
        assert_eq!((r[4], r[5]), (0x80, 0xFF), "weak/right is the high motor");
        assert_eq!(&r[6..9], &[64, 0, 0], "slot 1: red");

        let r = output_report(Model::Ds4, Bus::Bluetooth, out);
        assert_eq!((r.len(), r[0], r[1] & 0xC0, r[3]), (78, 0x11, 0xC0, 0x07));
        assert_eq!((r[6], r[7]), (0x80, 0xFF));
        assert!(crc_ok(&r));

        let r = output_report(Model::DualSense, Bus::Usb, out);
        assert_eq!((r.len(), r[0]), (48, 0x02));
        assert_eq!((r[1], r[2]), (0x0F, 0x10), "vibration and trigger blocks, player LEDs");
        assert_eq!((r[3], r[4]), (0x80, 0xFF), "right motor, then left");
        assert_eq!(r[39], 0x04);
        assert_eq!(r[44], 0x0A, "slot 1 LEDs");
        assert!(r[11..33].iter().all(|b| *b == 0), "adaptive trigger blocks stay off");

        let r = output_report(Model::DualSense, Bus::Bluetooth, out);
        assert_eq!((r.len(), r[0], r[1], r[2]), (78, 0x31, 0x30, 0x10));
        assert_eq!((r[5], r[6]), (0x80, 0xFF));
        assert_eq!(r[3], 0x0F);
        assert!(crc_ok(&r));
    }

    #[test]
    fn silence_is_all_zero_motors() {
        let off = Output::default();
        let r = output_report(Model::DualSense, Bus::Usb, off);
        assert_eq!((r[3], r[4]), (0, 0));
        let r = output_report(Model::Ds4, Bus::Usb, off);
        assert_eq!((r[4], r[5]), (0, 0));
        assert_eq!(motor(0), 0);
        assert_eq!(motor(65535), 255);
        // A slot past the last stays on the last.
        assert_eq!(output_report(Model::Ds4, Bus::Usb, Output { slot: 9, ..off })[6..9], DS4_COLOURS[3]);
    }

    // --- merging with XInput

    fn pressed(buttons: u32) -> PadState {
        PadState { buttons, ..Default::default() }
    }

    const IDLE: PadState = PadState {
        buttons: 0,
        left_trigger: 0.0,
        right_trigger: 0.0,
        left_x: 0.0,
        left_y: 0.0,
        right_x: 0.0,
        right_y: 0.0,
    };

    #[test]
    fn a_lone_hid_pad_takes_slot_zero() {
        let mut m = Merger::default();
        let out = m.merge(0, [None; 4], &[(7, pressed(A))]);
        assert_eq!(out, [Some(pressed(A)), None, None, None]);
        assert_eq!(m.owner(0), Owner::Hid(7));
        assert_eq!(m.slot_of(7), Some(0));
    }

    #[test]
    fn hid_pads_fill_the_slots_xinput_leaves() {
        let mut m = Merger::default();
        let x = [Some(IDLE), None, Some(IDLE), None];
        // No XInput-hiding verdict possible yet: the pads are unknown, so
        // hidden while XInput pads exist...
        let out = m.merge(0, x, &[(1, IDLE), (2, IDLE)]);
        assert_eq!(out, x);
        // ...until they move on their own for a while.
        let go = pressed(A);
        let mut out = x;
        for t in 0..=40 {
            out = m.merge(t * 8, x, &[(1, go), (2, go)]);
        }
        assert_eq!(out[1], Some(go));
        assert_eq!(out[3], Some(go));
        assert_eq!((m.owner(0), m.owner(1), m.owner(2), m.owner(3)),
            (Owner::XInput, Owner::Hid(1), Owner::XInput, Owner::Hid(2)));
    }

    #[test]
    fn a_pad_xinput_mirrors_stays_hidden_for_good() {
        let mut m = Merger::default();
        let go = pressed(A | UP);
        // Steam Input: the same buttons arrive on an XInput pad as well.
        for t in 0..200 {
            let out = m.merge(t * 8, [Some(go), None, None, None], &[(5, go)]);
            assert_eq!(out, [Some(go), None, None, None], "poll {t}");
        }
        // Even after it idles and a press the XInput side lags on: still the
        // same pad (the verdict is final).
        let out = m.merge(2000, [Some(IDLE), None, None, None], &[(5, pressed(B))]);
        assert_eq!(out[1], None);
        // Unplugged and back, it is judged afresh.
        m.merge(2100, [Some(IDLE), None, None, None], &[]);
        let out = m.merge(2200, [None; 4], &[(5, IDLE)]);
        assert_eq!(out[0], Some(IDLE));
    }

    #[test]
    fn a_pad_is_separate_when_it_moves_and_xinput_does_not_follow() {
        let mut m = Merger::default();
        let xbox = [Some(IDLE), None, None, None];
        let mut shown = false;
        for t in 0..60 {
            let out = m.merge(t * 8, xbox, &[(3, pressed(Y))]);
            shown = out[1].is_some();
            if t * 8 < 300 {
                assert!(!shown, "judged too soon at {} ms", t * 8);
            }
        }
        assert!(shown);
        // A different XInput pad doing something else changes nothing.
        let out = m.merge(500, [Some(pressed(B)), None, None, None], &[(3, pressed(Y))]);
        assert_eq!(out[1], Some(pressed(Y)));
    }

    #[test]
    fn sticks_pushed_the_same_way_count_as_following() {
        let hid = PadState { left_x: 1.0, ..Default::default() };
        let steam = PadState { left_x: 0.6, left_y: 0.1, ..Default::default() };
        assert!(follows(&hid, &steam));
        assert!(!follows(&hid, &PadState { left_x: -0.9, ..Default::default() }));
        assert!(!follows(&hid, &IDLE));
        let trig = PadState { right_trigger: 0.9, ..Default::default() };
        assert!(follows(&trig, &PadState { right_trigger: 0.5, ..Default::default() }));
        // Stick drift is not use.
        assert!(!active(&PadState { left_x: 0.1, ..Default::default() }));
    }

    #[test]
    fn slots_are_kept_and_never_stolen() {
        let mut m = Merger::default();
        // Two HID pads, no XInput: slots 0 and 1.
        m.merge(0, [None; 4], &[(1, IDLE), (2, IDLE)]);
        assert_eq!((m.slot_of(1), m.slot_of(2)), (Some(0), Some(1)));
        // The first one leaves: the other keeps slot 1.
        m.merge(8, [None; 4], &[(2, IDLE)]);
        assert_eq!((m.slot_of(1), m.slot_of(2)), (None, Some(1)));
        // A newcomer takes the free slot 0.
        m.merge(16, [None; 4], &[(2, IDLE), (3, IDLE)]);
        assert_eq!((m.slot_of(3), m.slot_of(2)), (Some(0), Some(1)));
    }

    #[test]
    fn xinput_arriving_moves_a_hid_pad_to_the_next_free_slot() {
        let mut m = Merger::default();
        m.merge(0, [None; 4], &[(1, IDLE)]);
        assert_eq!(m.slot_of(1), Some(0));
        // A pad is plugged in and takes slot 0 (XInput's own number). The
        // HID pad, unknown beside it, steps aside entirely (hidden) until
        // it proves itself; once it is separate it takes slot 1.
        let x = [Some(IDLE), None, None, None];
        let out = m.merge(8, x, &[(1, IDLE)]);
        assert_eq!((out[0], m.slot_of(1)), (Some(IDLE), None));
        let go = pressed(X);
        for t in 0..60 {
            m.merge(16 + t * 8, x, &[(1, go)]);
        }
        assert_eq!((m.owner(0), m.slot_of(1)), (Owner::XInput, Some(1)));
    }

    #[test]
    fn at_most_four_pads_are_seen() {
        let mut m = Merger::default();
        let hid: Vec<_> = (1..=6).map(|id| (id, IDLE)).collect();
        let out = m.merge(0, [None; 4], &hid);
        assert!(out.iter().all(Option::is_some));
        assert_eq!(m.slot_of(5), None);
        assert_eq!(m.slot_of(4), Some(3));
    }
}
