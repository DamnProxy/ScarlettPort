//! Vibration settings and the mapping from what the title asks for to what
//! a pad gets.
//!
//! The title calls `IGameInputDevice::SetRumbleState` with four levels
//! (`GameInputRumbleParams`): low frequency, high frequency, left trigger,
//! right trigger. XInput's left motor is the big low-frequency one and its
//! right motor the small high-frequency one, so low goes left and high goes
//! right. The trigger motors of Xbox One and Series pads exist only through
//! Windows.Gaming.Input (`Gamepad.Vibration`).
//!
//! Switches (the environment wins over `[game]` of the config file):
//!
//! * `VICTORY_RUMBLE=0` turns vibration off (`rumble = false`);
//! * `VICTORY_RUMBLE_STRENGTH=<0 to 2>` scales every level, 1.0 as the title
//!   asks (`rumble_strength`); levels are clamped to the motors' range after
//!   scaling, so above 1.0 weak rumble is boosted and strong rumble stays
//!   at full.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Strength limit.
pub const MAX_STRENGTH: f32 = 2.0;

/// The vibration switches.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub strength: f32,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            enabled: true,
            strength: 1.0,
        }
    }
}

fn flag(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

impl Settings {
    /// `[game]` of the config file (`rumble`, `rumble_strength`), then the
    /// environment over it. Returns what could not be used.
    pub fn from_sources(
        file: Option<&toml::Table>,
        get: &dyn Fn(&str) -> Option<String>,
    ) -> (Settings, Vec<String>) {
        let mut s = Settings::default();
        let mut problems = Vec::new();
        if let Some(t) = file {
            match t.get("rumble") {
                Some(toml::Value::Boolean(b)) => s.enabled = *b,
                Some(_) => problems.push("[game] rumble: expected true or false".into()),
                None => {}
            }
            if let Some(v) = t.get("rumble_strength") {
                let n = match v {
                    toml::Value::Float(f) => Some(*f as f32),
                    toml::Value::Integer(i) => Some(*i as f32),
                    _ => None,
                };
                match n {
                    Some(n) if (0.0..=MAX_STRENGTH).contains(&n) => s.strength = n,
                    _ => problems.push(format!(
                        "[game] rumble_strength = {v}: expected 0 to {MAX_STRENGTH}"
                    )),
                }
            }
        }
        if let Some(v) = get("VICTORY_RUMBLE") {
            match flag(&v) {
                Some(on) => s.enabled = on,
                None => problems.push(format!("VICTORY_RUMBLE={v}: expected 0 or 1")),
            }
        }
        if let Some(v) = get("VICTORY_RUMBLE_STRENGTH") {
            match v.trim().parse::<f32>() {
                Ok(n) if (0.0..=MAX_STRENGTH).contains(&n) => s.strength = n,
                _ => problems.push(format!(
                    "VICTORY_RUMBLE_STRENGTH={v}: expected 0 to {MAX_STRENGTH}"
                )),
            }
        }
        (s, problems)
    }
}

/// What a pad's motors are set to: the two main motors as XInput takes them
/// and the trigger motors as levels from 0 to 1.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Motors {
    /// XInput `wLeftMotorSpeed` (the low-frequency motor).
    pub left: u16,
    /// XInput `wRightMotorSpeed` (the high-frequency motor).
    pub right: u16,
    pub left_trigger: f32,
    pub right_trigger: f32,
}

impl Motors {
    pub fn is_off(&self) -> bool {
        self.left == 0 && self.right == 0 && !self.has_triggers()
    }

    /// Whether a trigger motor is in use (only then does the pad need the
    /// path that has them).
    pub fn has_triggers(&self) -> bool {
        self.left_trigger > 0.0 || self.right_trigger > 0.0
    }
}

fn level(v: f32, strength: f32) -> f32 {
    if v.is_nan() {
        return 0.0;
    }
    (v.clamp(0.0, 1.0) * strength).clamp(0.0, 1.0)
}

/// The motors for the title's `params` (low, high, left trigger, right
/// trigger) under `s`.
pub fn plan(params: [f32; 4], s: &Settings) -> Motors {
    if !s.enabled {
        return Motors::default();
    }
    let st = if s.strength.is_nan() {
        0.0
    } else {
        s.strength.clamp(0.0, MAX_STRENGTH)
    };
    let speed = |v: f32| (level(v, st) * f32::from(u16::MAX)).round() as u16;
    Motors {
        left: speed(params[0]),
        right: speed(params[1]),
        left_trigger: level(params[2], st),
        right_trigger: level(params[3], st),
    }
}

/// Which host API drives a pad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// `XInputSetState`: the two main motors.
    XInput,
    /// `Windows.Gaming.Input` `Gamepad.Vibration`: all four motors.
    Gaming,
}

/// The API for `m`. The gaming API is used while a trigger motor is in use,
/// and once more after it (`was_gaming`) to set the triggers back to rest
/// and hand the main motors back to XInput; with no gaming pad to talk to
/// (`gaming_pad`) it is XInput, whose main motors still work.
pub fn route(m: &Motors, gaming_pad: bool, was_gaming: bool) -> Route {
    if gaming_pad && (m.has_triggers() || was_gaming) {
        Route::Gaming
    } else {
        Route::XInput
    }
}

static LIVE_ON: AtomicBool = AtomicBool::new(true);
static LIVE_STRENGTH: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0

/// Takes the settings at start (call once, before the title runs). Returns
/// what could not be used.
pub fn configure(file: Option<&toml::Table>) -> Vec<String> {
    let (s, problems) = Settings::from_sources(file, &|n| std::env::var(n).ok());
    set_live(s);
    problems
}

/// The settings in force (the overlay can change them).
pub fn current() -> Settings {
    Settings {
        enabled: LIVE_ON.load(Ordering::Relaxed),
        strength: f32::from_bits(LIVE_STRENGTH.load(Ordering::Relaxed)),
    }
}

/// Makes `s` the settings in force, now.
pub fn set_live(s: Settings) {
    LIVE_ON.store(s.enabled, Ordering::Relaxed);
    LIVE_STRENGTH.store(
        s.strength.clamp(0.0, MAX_STRENGTH).to_bits(),
        Ordering::Relaxed,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| pairs.iter().find(|p| p.0 == k).map(|p| p.1.to_string())
    }

    #[test]
    fn low_goes_to_the_left_motor_and_high_to_the_right() {
        let m = plan([0.5, 1.0, 0.25, 0.75], &Settings::default());
        assert_eq!((m.left, m.right), (32768, 65535));
        assert_eq!((m.left_trigger, m.right_trigger), (0.25, 0.75));
        let m = plan([1.0, 0.0, 0.0, 0.0], &Settings::default());
        assert_eq!((m.left, m.right), (65535, 0));
    }

    #[test]
    fn off_and_nonsense_levels_are_silent() {
        let off = Settings {
            enabled: false,
            strength: 1.0,
        };
        assert!(plan([1.0; 4], &off).is_off());
        let m = plan([f32::NAN, -3.0, f32::INFINITY, 7.0], &Settings::default());
        assert_eq!(
            (m.left, m.right, m.left_trigger, m.right_trigger),
            (0, 0, 1.0, 1.0)
        );
        let zero = Settings {
            enabled: true,
            strength: 0.0,
        };
        assert!(plan([1.0; 4], &zero).is_off());
    }

    #[test]
    fn strength_scales_and_clamps() {
        let half = Settings {
            enabled: true,
            strength: 0.5,
        };
        let m = plan([1.0, 0.5, 1.0, 0.5], &half);
        assert_eq!((m.left, m.right), (32768, 16384));
        assert_eq!((m.left_trigger, m.right_trigger), (0.5, 0.25));
        let more = Settings {
            enabled: true,
            strength: 2.0,
        };
        let m = plan([0.25, 0.75, 0.25, 0.75], &more);
        assert_eq!((m.left, m.right), (32768, 65535));
        assert_eq!((m.left_trigger, m.right_trigger), (0.5, 1.0));
        let wild = Settings {
            enabled: true,
            strength: 99.0,
        };
        assert_eq!(plan([0.25; 4], &wild).left, 32768);
    }

    #[test]
    fn the_gaming_api_is_used_for_triggers_and_to_release_them() {
        let on = plan([0.5, 0.5, 0.0, 1.0], &Settings::default());
        let main_only = plan([0.5, 0.5, 0.0, 0.0], &Settings::default());
        assert_eq!(route(&on, true, false), Route::Gaming);
        assert_eq!(route(&main_only, true, false), Route::XInput);
        assert_eq!(route(&main_only, true, true), Route::Gaming, "the release");
        assert_eq!(route(&on, false, false), Route::XInput, "no gaming pad");
        assert_eq!(route(&main_only, false, true), Route::XInput);
    }

    #[test]
    fn triggers_are_noted_only_when_used() {
        assert!(!plan([1.0, 1.0, 0.0, 0.0], &Settings::default()).has_triggers());
        assert!(plan([0.0, 0.0, 0.0, 0.1], &Settings::default()).has_triggers());
    }

    #[test]
    fn the_environment_wins_over_the_file_and_bad_values_are_named() {
        let file: toml::Table = toml::from_str("rumble = false\nrumble_strength = 0.5").unwrap();
        let (s, p) = Settings::from_sources(Some(&file), &get(&[]));
        assert_eq!(
            s,
            Settings {
                enabled: false,
                strength: 0.5
            }
        );
        assert!(p.is_empty());
        let (s, _) = Settings::from_sources(
            Some(&file),
            &get(&[("VICTORY_RUMBLE", "1"), ("VICTORY_RUMBLE_STRENGTH", "1.5")]),
        );
        assert_eq!(
            s,
            Settings {
                enabled: true,
                strength: 1.5
            }
        );
        let (s, p) = Settings::from_sources(
            None,
            &get(&[("VICTORY_RUMBLE", "maybe"), ("VICTORY_RUMBLE_STRENGTH", "9")]),
        );
        assert_eq!(s, Settings::default());
        assert_eq!(p.len(), 2);
        let bad: toml::Table = toml::from_str("rumble = 1\nrumble_strength = \"x\"").unwrap();
        assert_eq!(Settings::from_sources(Some(&bad), &get(&[])).1.len(), 2);
        let (s, _) = Settings::from_sources(None, &get(&[("VICTORY_RUMBLE", "0")]));
        assert!(!s.enabled);
    }
}
