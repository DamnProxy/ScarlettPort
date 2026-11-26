# Keyboard and mouse

Keyboard and mouse play is on by default. The title reads gamepads (and
keyboards for text), so the keyboard and mouse act as one more pad. All
sources feed one virtual pad, so none of them disables another:

* a physical gamepad (XInput): buttons OR, triggers max; a stick it deflects
  past 0.2 takes that stick from the keyboard and mouse (priority);
* keyboard and mouse (below);
* scripted input for testing (`VICTORY_AUTO_INPUT`, `VICTORY_PRESS_FILE`,
  `VICTORY_FAST_BOOT`, `VICTORY_KEYS`): joins the same pad, by the old
  stand-in pad's table, whether or not a real pad is connected.

With `VICTORY_KBM=0` the keyboard and mouse stop acting as a pad and the old
stand-in pad (W A S D / I J K L sticks, Enter, Backspace, X, Y) is back; a real
pad and the scripted input keep working.

## Mouse

Click in the window to capture the mouse: the cursor is hidden, held inside
the window, and raw motion (no pointer acceleration) turns the camera through
the right stick. The click that captures is not a shot. Pressing Alt, Alt-Tab,
or any loss of focus frees the mouse; a click captures it again. While the
mouse is free the window caption says so.

| Mouse | Pad |
|-------|-----|
| motion | right stick (a rate, back to rest when the mouse stops) |
| left button | RT |
| right button | LT |
| middle button | right stick click |
| wheel up / down | D-pad up / down |

Motion is counted on its own thread and read at each read of the title: the
title takes the stick as a turn rate, so the counts since its last read are
divided by the time since then, and the same hand movement turns the same
angle at any frame rate (spans up to 1 s). At the default sensitivity 2,000
counts per second is a full stick.

## Keys

| Key | Pad |
|-----|-----|
| W A S D | left stick (on foot); A D steer, W / S are RT / LT in a vehicle |
| Left or Right Shift | A (sprint) |
| Space | X |
| E, F | Y |
| Q | LB |
| R | RB |
| Left or Right Ctrl | left stick click |
| C | right stick click |
| Enter | A |
| Esc, Backspace | B |
| Arrow keys | D-pad |
| Tab | View |
| P | Menu |
| Z (held) | walk: left stick at half deflection |
| G | switch between the on-foot and the vehicle layout |
| Left or Right Alt | free the mouse |

## Settings

Environment variables (they win over `[controls]` in `victory.toml`):

| Variable | Effect |
|----------|--------|
| `VICTORY_KBM=0` | no keyboard and mouse pad |
| `VICTORY_MOUSE_SENS=<0.01 to 100>` | mouse sensitivity, 1.0 default |
| `VICTORY_MOUSE_INVERT_Y=1` | invert vertical look |
| `VICTORY_KEYMAP=<file>` | rebind keys from a text file |
| `VICTORY_KBM_SCRIPT=<s>:<key>,...` | (diagnostics build) play keys and mouse motion |

`VICTORY_KEYMAP` is one `key=button` per line, `#` starts a comment; a list
after `=` presses several outputs, nothing after `=` unbinds the key.
`[on_foot]` and `[vehicle]` lines choose a layout; bindings before the first
section go to both.

```
# keys.txt
Space = A
MouseLeft = RT
LShift =
[vehicle]
W = RT
```

Keys are `A`-`Z`, `0`-`9`, `F1`-`F24`, `Num0`-`Num9`, and names such as
`Space`, `Enter`, `Esc`, `Tab`, `Backspace`, `Up`, `LShift`, `LCtrl`, `LAlt`,
`MouseLeft`, `MouseRight`, `MouseMiddle`, `Mouse4`, `Mouse5`, `WheelUp`,
`WheelDown`. Outputs are `A B X Y LB RB LT RT LS RS View Menu DpadUp DpadDown
DpadLeft DpadRight LeftUp LeftDown LeftLeft LeftRight RightUp RightDown
RightLeft RightRight`, and the actions `Walk`, `ToggleVehicle`,
`ReleaseCursor`. A line that cannot be read is logged and skipped.

## The F1 settings overlay

F1 opens a menu drawn by the runtime over the game (F1 or Esc closes it). While
it is open the title sees every pad at rest and no key or mouse button down;
the mouse is let go (the menu has its own pointer) and captured again when it
closes. Pages:

* **Controls**: mouse sensitivity, vertical sensitivity, invert Y, walk speed,
  a controller dead zone, and every key binding of the on-foot and the vehicle
  layout (three keys to an action). Click a box, press the new key or mouse
  button or turn the wheel; Esc cancels, Delete clears. A key that did something
  else loses it, and the menu says so. F1 and Esc are kept for the menu.
* **Display**: window mode (windowed, borderless), interface size, the frame-time
  graph on screen, and the pacing switches `VICTORY_PACE`, `_FPS`, `_INPUT`
  (live) and `_LATENCY`, `_TEARING`, `_FEEDBACK` (shape the swap chain: marked
  `[restart]`, and a "Restart required" note says so).
* **Performance**: fps, frame time average, 99th percentile and worst, the
  frame-time graph, RAM (this program and the PC), VRAM (card and shared), and
  which speed switches the environment has on (read only; they are read at
  start, so changing one needs a restart).

Changes apply at once and are saved: sticks, mouse and the rest to `[controls]`,
`[display]` and `[pace]` of `victory.toml` (a line at a time; comments and other
settings stay), bindings to the key map file (`VICTORY_KEYMAP`, else
`keymap.txt` under the data folder; a hand-written file there is kept once as
`.bak`). `[controls] pad_deadzone` is the controller dead zone.

## Low frame rates

Keys and mouse buttons are sampled every 8 ms on a thread of their own, mouse
motion and the wheel as raw input messages arrive. A press stays down until the
title has read it once, and mouse motion adds up between reads, so a tap
shorter than a frame (frames can take 300 ms) is never lost. Scripted presses
are latched the same way.

The logic is in `crates/victory-core/src/kbm.rs`, which is plain data and is
unit-tested on any host; `crates/victory-hle/src/modules/gameinput.rs` feeds it
from the host.
## Vibration

The title sets four levels per pad (low and high frequency, left and right
trigger). Low goes to the left (large) motor and high to the right (small)
motor through XInput. The trigger motors of Xbox One and Series pads are driven
through `Windows.Gaming.Input` (`Gamepad.Vibration`) while a trigger level is
above zero, and released once after; a pad without them, or an API that does
not list the pad, falls back to the two main motors on XInput. Which gamepad is
which XInput slot is assumed from connection order (right with one pad).

| Setting | Effect |
|---------|--------|
| `VICTORY_RUMBLE=0` or `[game] rumble = false` | no vibration |
| `VICTORY_RUMBLE_STRENGTH=<0 to 2>` or `[game] rumble_strength` | scale every level (1.0 as the title asks; scaled levels stop at full) |

## Keyboard hints

The title's button icons are fixed Xbox glyphs (it has no query for the device
in use, so they cannot be switched from here). A small legend in the bottom
left corner says which key does what the icons show (A, B, X, Y, LB, RB, View,
Menu, D-pad), from the key map in force. It shows for 8 s when the keyboard or
mouse is used after 20 s without; F2 pins it on or takes it off.
`VICTORY_KBM_HINTS=off|auto|always` (`[game] kbm_hints`).

## Frame cap

The F1 Display page's Frame cap (Title, 30, 45, 60, 90, 120, 144, 240) is the
pacer's rate (`VICTORY_PACE_FPS`); choosing one turns even pacing on.
