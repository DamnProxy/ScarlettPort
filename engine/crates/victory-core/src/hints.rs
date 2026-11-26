//! Keyboard hints for the title's button prompts.
//!
//! The title draws Xbox button icons from its own UI data and has no query
//! for the kind of device in use (MENUS.md section 4.2: the icon builder at
//! 0x2A52D80 formats a constant `platform_xbsx...Main_Button_%d` class name;
//! no path reads a last-input device), so the icons cannot be switched from
//! here. This module makes the hint layer the overlay draws instead: a small
//! legend in a corner that says which key does what the icons show, built from
//! the keymap in force (so rebinding is reflected).
//!
//! The runtime does not know which buttons the current screen asks for (that
//! needs a hook on the icon builder), so the legend lists the buttons menus
//! use. When it shows:
//!
//! * `VICTORY_KBM_HINTS=auto` (default): for a few seconds when the keyboard
//!   or mouse is used after a quiet spell (and at first use);
//! * `always`: until F2 hides it; `off`: never.
//!
//! F2 pins the legend on or takes it off in `auto` and `always`.

use crate::kbm::{Output, Settings, WHEEL_DOWN, WHEEL_UP, key_name};

/// The key that pins the legend on screen or removes it: F2.
pub const TOGGLE_KEY: u8 = 0x71;

/// How long the legend shows after the keyboard is picked up.
pub const SHOW_MS: u64 = 8_000;
/// How long the keyboard must have been unused for its next use to show it.
pub const IDLE_MS: u64 = 20_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    Off,
    #[default]
    Auto,
    Always,
}

impl Mode {
    /// `VICTORY_KBM_HINTS` (`0`/`off`, `auto`, `1`/`always`/`on`).
    pub fn parse(text: &str) -> Option<Mode> {
        match text.trim().to_ascii_lowercase().as_str() {
            "0" | "off" | "no" | "false" => Some(Mode::Off),
            "" | "auto" => Some(Mode::Auto),
            "1" | "on" | "always" | "yes" | "true" => Some(Mode::Always),
            _ => None,
        }
    }

    /// The mode from the environment and `[game] kbm_hints` (a string or
    /// bool); what cannot be read is returned in the second place.
    pub fn from_sources(
        file: Option<&toml::Table>,
        get: &dyn Fn(&str) -> Option<String>,
    ) -> (Mode, Vec<String>) {
        let mut mode = Mode::Auto;
        let mut problems = Vec::new();
        if let Some(v) = file.and_then(|t| t.get("kbm_hints")) {
            let text = match v {
                toml::Value::String(s) => s.clone(),
                toml::Value::Boolean(b) => b.to_string(),
                _ => String::new(),
            };
            match Mode::parse(&text) {
                Some(m) if !text.is_empty() => mode = m,
                _ => problems.push(format!("[game] kbm_hints = {v}: expected off, auto or always")),
            }
        }
        if let Some(v) = get("VICTORY_KBM_HINTS") {
            match Mode::parse(&v) {
                Some(m) => mode = m,
                None => problems.push(format!("VICTORY_KBM_HINTS={v}: expected off, auto or always")),
            }
        }
        (mode, problems)
    }
}

/// One line of the legend: the pad button the icons show, and the keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hint {
    pub button: &'static str,
    pub keys: String,
}

/// The buttons the legend lists, by their name in key map files.
const BUTTONS: [&str; 8] = ["A", "B", "X", "Y", "LB", "RB", "View", "Menu"];
const DPAD: [&str; 4] = ["DpadUp", "DpadDown", "DpadLeft", "DpadRight"];

/// Keys in the order to show them: the wheel, which is no key, last.
fn shown_order(vks: &[u8]) -> Vec<u8> {
    let mut v = vks.to_vec();
    v.sort_by_key(|&k| k == WHEEL_UP || k == WHEEL_DOWN);
    v
}

fn keys_text(vks: &[u8]) -> String {
    shown_order(vks).iter().take(2).map(|&k| key_name(k)).collect::<Vec<_>>().join("/")
}

/// The legend for the on-foot layout of `s` (menus read the same keys in
/// either layout): a hint for each button that has a key. The direction pad
/// comes last, as its keys in up, down, left, right order.
pub fn legend(s: &Settings) -> Vec<Hint> {
    let by_name = |name: &str| Output::all().find(|o| o.name() == name);
    let mut out = Vec::new();
    for name in BUTTONS {
        let Some(o) = by_name(name) else { continue };
        let keys = s.on_foot.keys_of(o);
        if !keys.is_empty() {
            out.push(Hint {
                button: name,
                keys: keys_text(&keys),
            });
        }
    }
    let dpad: Vec<String> = DPAD
        .iter()
        .filter_map(|n| by_name(n))
        .filter_map(|o| shown_order(&s.on_foot.keys_of(o)).first().map(|&k| key_name(k)))
        .collect();
    if dpad.len() == 4 {
        out.push(Hint {
            button: "Dpad",
            keys: dpad.join(" "),
        });
    }
    out
}

fn is_activity(vk: u8) -> bool {
    // Mouse buttons (1, 2, 4, 5, 6) and the keys, but not the function keys
    // (F1 is the overlay's and F2 this layer's) or the lock and modifier
    // keys that are held by habit.
    !matches!(vk, 0x70..=0x87 | 0x14 | 0x90 | 0x91)
}

/// When the legend shows, from the keys held each present.
#[derive(Debug)]
pub struct Tracker {
    mode: Mode,
    pinned: bool,
    shown_until: u64,
    last_activity: Option<u64>,
    prev: Vec<u8>,
}

impl Tracker {
    pub fn new(mode: Mode) -> Tracker {
        Tracker {
            mode,
            pinned: mode == Mode::Always,
            shown_until: 0,
            last_activity: None,
            prev: Vec::new(),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// One present: `down` is every key and mouse button held now, `now_ms`
    /// a steady clock. Returns whether to draw the legend.
    pub fn update(&mut self, now_ms: u64, down: &[u8]) -> bool {
        let pressed: Vec<u8> = down.iter().copied().filter(|k| !self.prev.contains(k)).collect();
        self.prev = down.to_vec();
        if self.mode == Mode::Off {
            return false;
        }
        if pressed.contains(&TOGGLE_KEY) {
            self.pinned = !self.pinned;
        }
        if down.iter().any(|&k| is_activity(k)) {
            let quiet = self.last_activity.is_none_or(|t| now_ms.saturating_sub(t) >= IDLE_MS);
            if quiet {
                self.shown_until = now_ms + SHOW_MS;
            }
            self.last_activity = Some(now_ms);
        }
        self.pinned || (self.mode == Mode::Auto && now_ms < self.shown_until)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_legend_follows_the_keymap() {
        let s = Settings::default();
        let l = legend(&s);
        let a = l.iter().find(|h| h.button == "A").expect("A has a key");
        assert!(a.keys.contains(&key_name(0x0D)), "Enter does A: {}", a.keys);
        let b = l.iter().find(|h| h.button == "B").unwrap();
        assert!(!b.keys.is_empty());
        assert_eq!(l.last().unwrap().button, "Dpad");
        assert_eq!(l.last().unwrap().keys, "Up Down Left Right");
        // Rebinding is reflected; an unbound button is left out.
        let mut s = Settings::default();
        let problems = s.apply_keymap("Enter=\nB=A\n");
        assert!(problems.is_empty(), "{problems:?}");
        let l = legend(&s);
        assert!(l.iter().find(|h| h.button == "A").unwrap().keys.contains('B'));
    }

    #[test]
    fn modes_parse() {
        assert_eq!(Mode::parse("auto"), Some(Mode::Auto));
        assert_eq!(Mode::parse("0"), Some(Mode::Off));
        assert_eq!(Mode::parse("ALWAYS"), Some(Mode::Always));
        assert_eq!(Mode::parse("sometimes"), None);
        let none = |_: &str| None;
        assert_eq!(Mode::from_sources(None, &none).0, Mode::Auto);
        let file: toml::Table = toml::from_str("kbm_hints = \"always\"").unwrap();
        assert_eq!(Mode::from_sources(Some(&file), &none).0, Mode::Always);
        let env = |n: &str| (n == "VICTORY_KBM_HINTS").then(|| "off".to_string());
        assert_eq!(Mode::from_sources(Some(&file), &env).0, Mode::Off);
        let bad: toml::Table = toml::from_str("kbm_hints = 3").unwrap();
        assert_eq!(Mode::from_sources(Some(&bad), &none).1.len(), 1);
    }

    #[test]
    fn auto_shows_on_first_use_then_after_a_quiet_spell() {
        let mut t = Tracker::new(Mode::Auto);
        assert!(!t.update(0, &[]));
        assert!(t.update(100, &[b'W']));
        assert!(t.update(100 + SHOW_MS - 1, &[]));
        assert!(!t.update(100 + SHOW_MS, &[]));
        // Playing on does not bring it back...
        assert!(!t.update(10_000, &[b'W']));
        assert!(!t.update(25_000, &[b'W']));
        // ...a quiet spell does.
        assert!(!t.update(25_000 + IDLE_MS - 1, &[]));
        assert!(t.update(60_000, &[0x01]));
    }

    #[test]
    fn function_keys_are_not_use_and_f2_pins() {
        let mut t = Tracker::new(Mode::Auto);
        assert!(!t.update(0, &[0x70]));
        assert!(t.update(10, &[TOGGLE_KEY]), "F2 pins it");
        assert!(t.update(10_000_000, &[]), "pinned stays");
        assert!(!t.update(10_000_001, &[TOGGLE_KEY]), "F2 again takes it off");
        // Held F2 does not toggle again.
        assert!(!t.update(10_000_002, &[TOGGLE_KEY]));
    }

    #[test]
    fn always_starts_on_and_off_never_shows() {
        let mut t = Tracker::new(Mode::Always);
        assert!(t.update(0, &[]));
        assert!(!t.update(1, &[TOGGLE_KEY]));
        let mut t = Tracker::new(Mode::Off);
        assert!(!t.update(0, &[b'W']));
        assert!(!t.update(1, &[TOGGLE_KEY]));
    }
}
