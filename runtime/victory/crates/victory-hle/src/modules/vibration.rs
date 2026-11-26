//! Trigger rumble: Xbox One and Series pads have two more motors, under the
//! triggers, that XInput cannot drive. `Windows.Gaming.Input`'s
//! `Gamepad.Vibration` can, and it works from a desktop process without a
//! package or a redistributable.
//!
//! `gameinput.rs` hands every vibration request here (`apply`) with the
//! motors already scaled (`victory_core::rumble`). XInput stays the path for
//! the two main motors; the gaming API is used only while a trigger motor
//! is in use (and once after, to release it), because the two APIs write the
//! same pad and a stale main-motor value from one would undo the other.
//! With no gaming pad to talk to (a 360 pad, the API refusing, a pad not
//! listed yet) the main motors go through XInput as before.
//!
//! Which `Gamepad` is which XInput slot is not given by the API; both list
//! pads in the order they were connected, so the pad in XInput slot `s` is
//! taken to be the `n`th gamepad where `n` counts the connected XInput slots
//! below `s`. With one pad that is always right.

use std::sync::Mutex;

use victory_core::rumble::{Motors, Route, route};
use windows::Gaming::Input::{Gamepad, GamepadVibration};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows_sys::Win32::UI::Input::XboxController::{
    XINPUT_VIBRATION, XInputGetState, XInputSetState, XINPUT_STATE,
};

/// Per slot: the last request went through the gaming API.
static GAMING: Mutex<[bool; 4]> = Mutex::new([false; 4]);

/// The gaming pad for XInput slot `slot`, if the API lists one.
fn gaming_pad(slot: usize) -> Option<Gamepad> {
    // SAFETY: plain initialisation of this thread's apartment; the answer
    // (already initialised in another mode, say) is of no interest.
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    let pads = Gamepad::Gamepads().ok()?;
    let count = pads.Size().ok()? as usize;
    if count == 0 {
        return None;
    }
    let index = if count == 1 {
        0
    } else {
        (0..slot)
            .filter(|&s| {
                let mut state = XINPUT_STATE::default();
                // SAFETY: a valid state to fill; an empty slot just fails.
                unsafe { XInputGetState(s as u32, &mut state) == 0 }
            })
            .count()
    };
    pads.GetAt(index as u32).ok()
}

/// Sets the motors of the pad in XInput slot `slot`.
pub fn apply(slot: usize, m: &Motors) {
    let mut gaming = GAMING.lock().unwrap_or_else(|e| e.into_inner());
    let was = slot < 4 && gaming[slot];
    // Looked for only when it could be used: most requests are main motors.
    let pad = (m.has_triggers() || was).then(|| gaming_pad(slot)).flatten();
    let used = match route(m, pad.is_some(), was) {
        Route::Gaming => {
            let pad = pad.expect("routed to the gaming API with a pad");
            let v = GamepadVibration {
                LeftMotor: f64::from(m.left) / f64::from(u16::MAX),
                RightMotor: f64::from(m.right) / f64::from(u16::MAX),
                LeftTrigger: f64::from(m.left_trigger),
                RightTrigger: f64::from(m.right_trigger),
            };
            pad.SetVibration(v).is_ok()
        }
        Route::XInput => false,
    };
    if slot < 4 {
        // Left in the gaming state while a trigger motor runs; after the
        // release (all triggers at rest) the main motors are XInput's again.
        gaming[slot] = used && m.has_triggers();
    }
    if !used {
        let speeds = XINPUT_VIBRATION {
            wLeftMotorSpeed: m.left,
            wRightMotorSpeed: m.right,
        };
        // SAFETY: a valid vibration; an empty slot just fails.
        unsafe { XInputSetState(slot as u32, &speeds) };
    }
}
