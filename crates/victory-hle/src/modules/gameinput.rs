//! `gameinput.dll`: input devices from the host. Gamepads come from XInput
//! (host slots 0-3); while none is connected, the keyboard and the mouse
//! drive one stand-in gamepad ([`KEYS`]). The keyboard is a device of its own
//! too. A poll thread turns host state into readings (a new one per change)
//! and runs device callbacks on connect and disconnect.
//!
//! The mouse (this fork, 2026-11-26). A console title knows pads only: it
//! shows a pad's buttons and aims with a trigger and a stick. So the mouse is
//! part of the stand-in pad (`VICTORY_MOUSE`, on unless `0`): while a window
//! of the title is in front its pointer is taken (not shown, kept in the
//! window), its travel pushes the right stick and its buttons are the pad's
//! (the right one the left trigger, the left one the right trigger). F1
//! gives the pointer back and takes it again. The keys are laid out as Grand
//! Theft Auto V's ([`KEYS`]); in a vehicle W and S are the triggers and some
//! keys do other things ([`DRIVING`]). The title does not say when its player
//! is in a vehicle: F (get in, get out) switches to those keys and back, and
//! P (or F2) switches alone, to put it right.
//!
//! shortcut: a stick says how fast the view turns and a mouse how far. The
//! pointer's travel in the title's last frame of input is given as that
//! frame's speed ([`Look`]); how far the title turns for it is the title's
//! own (its dead zone, its response curve, its slow-down while aiming), which
//! `VICTORY_MOUSE` takes numbers against. Not measured in the title: the
//! defaults are worked out, not tried.
#![allow(non_snake_case)]
// Title code calls these through the objects, passing pointers the real API
// would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use super::xasync::{E_INVALIDARG, E_NOTIMPL};
use super::xgameruntime::{HRESULT, S_OK};
use crate::{HleExport, HleModule};

/// The documented errors.
pub const DEVICE_DISCONNECTED: HRESULT = 0x838A_0001_u32 as i32;
pub const DEVICE_NOT_FOUND: HRESULT = 0x838A_0002_u32 as i32;
pub const READING_NOT_FOUND: HRESULT = 0x838A_0003_u32 as i32;
pub const REFERENCE_TOO_OLD: HRESULT = 0x838A_0004_u32 as i32;
const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;

/// Input kinds (bits) our devices have.
const KEYBOARD_KIND: u32 = 0x10;
const GAMEPAD_KIND: u32 = 0x4_0000;
/// Device status of a connected device: connected, input enabled.
const STATUS_CONNECTED: u32 = 0x3;
const ASYNC_ENUMERATION: u32 = 1;

/// Gamepad buttons; the title maps each bit to one of its pad buttons.
const MENU: u32 = 0x1;
const VIEW: u32 = 0x2;
const A: u32 = 0x4;
const B: u32 = 0x8;
const X: u32 = 0x10;
const Y: u32 = 0x20;
const UP: u32 = 0x40;
const DOWN: u32 = 0x80;
const LEFT: u32 = 0x100;
const RIGHT: u32 = 0x200;
const LEFT_SHOULDER: u32 = 0x400;
const RIGHT_SHOULDER: u32 = 0x800;
const LEFT_STICK: u32 = 0x1000;
const RIGHT_STICK: u32 = 0x2000;

/// Devices: host pad slots, the keyboard's stand-in pad, the keyboard.
const PADS: usize = 4;
const FALLBACK: usize = PADS;
const KEYBOARD: usize = PADS + 1;
const DEVICES: usize = PADS + 2;

/// Readings kept per device for next/previous lookups.
const HISTORY: usize = 256;
const POLL: Duration = Duration::from_millis(8);

/// The mouse's buttons, as virtual keys: left, right, middle and the two at
/// the side. They are no keys of the keyboard device (its keys start at 0x08).
const MOUSE_LEFT: u8 = 0x01;
const MOUSE_RIGHT: u8 = 0x02;
const MOUSE_MIDDLE: u8 = 0x04;
const MOUSE_BACK: u8 = 0x05;
const MOUSE_FORWARD: u8 = 0x06;
/// The keys that switch something of the stand-in pad itself ([`Switches`]).
const F1: u8 = 0x70;
const F2: u8 = 0x71;
/// P switches the driving keys too, as F2 does: easier to reach.
const P: u8 = b'P';

/// What a key or a mouse button does on the stand-in pad.
#[derive(Clone, Copy, Debug, PartialEq)]
enum To {
    Button(u32),
    LeftTrigger,
    RightTrigger,
    /// A stick pushed one way: the axis (left x, left y, right x, right y)
    /// and the direction on it (up and right are positive).
    Stick(usize, f32),
}

/// The left stick's y axis, whose keys are the triggers while driving.
const LEFT_Y: usize = 1;

/// The keyboard's and the mouse's stand-in gamepad (virtual key, what it
/// does); README lists it, and `VICTORY_PAD_KEYS` changes it ([`rebound`]).
/// Sticks: W/A/S/D left, I/J/K/L and the mouse right; triggers the mouse's
/// buttons (and Z).
///
/// On foot it is laid out as Grand Theft Auto V's keyboard is, onto the pad
/// buttons that game gives those actions (the user asked for that layout,
/// 2026-11-26): Shift sprints (A), Space jumps (X), C crouches (the left
/// stick's click), R reloads (B), F gets in and out (Y), Q takes cover (RB),
/// Tab is the weapon wheel (LB), E the context action (D-pad right), V the
/// camera (View), Esc pauses (Menu). In a vehicle some keys do other
/// things ([`DRIVING`]).
const KEYS: [(u8, To); 33] = [
    (0x1B, To::Button(MENU)), // Esc
    (b'V', To::Button(VIEW)),
    (0x0D, To::Button(A)), // Enter
    (0x08, To::Button(B)), // Backspace
    (b'X', To::Button(X)),
    (b'Y', To::Button(Y)),
    (0x26, To::Button(UP)), // arrows
    (0x28, To::Button(DOWN)),
    (0x25, To::Button(LEFT)),
    (0x27, To::Button(RIGHT)),
    (0x09, To::Button(LEFT_SHOULDER)), // Tab
    (b'Q', To::Button(RIGHT_SHOULDER)),
    (b'E', To::Button(RIGHT)),
    (b'H', To::Button(RIGHT_STICK)),
    (b'Z', To::LeftTrigger),
    (b'C', To::Button(LEFT_STICK)),
    (b'A', To::Stick(0, -1.0)),
    (b'D', To::Stick(0, 1.0)),
    (b'W', To::Stick(LEFT_Y, 1.0)),
    (b'S', To::Stick(LEFT_Y, -1.0)),
    (b'J', To::Stick(2, -1.0)),
    (b'L', To::Stick(2, 1.0)),
    (b'I', To::Stick(3, 1.0)),
    (b'K', To::Stick(3, -1.0)),
    (0x20, To::Button(X)),          // Space
    (0xA0, To::Button(A)),          // left Shift
    (0xA2, To::Button(LEFT_STICK)), // left Ctrl
    (b'R', To::Button(B)),
    (b'F', To::Button(Y)),
    (MOUSE_RIGHT, To::LeftTrigger),
    (MOUSE_LEFT, To::RightTrigger),
    (MOUSE_MIDDLE, To::Button(RIGHT_STICK)),
    (MOUSE_BACK, To::Button(LEFT_SHOULDER)),
];

/// What some keys do instead in a vehicle (driving, [`Switches`]), as in
/// Grand Theft Auto V: W and S are the throttle and the brake (the right and
/// the left trigger; A and D steer with the stick), Space the handbrake (RB),
/// E the horn (the left stick's click), C looks behind (the right stick's).
/// A key `VICTORY_PAD_KEYS` binds anew keeps what it is bound to there.
const DRIVING: [(u8, To); 5] = [
    (b'W', To::RightTrigger),
    (b'S', To::LeftTrigger),
    (0x20, To::Button(RIGHT_SHOULDER)), // Space
    (b'E', To::Button(LEFT_STICK)),
    (b'C', To::Button(RIGHT_STICK)),
];

/// A key of `VICTORY_PAD_KEYS` by its name: a letter, a digit, or one of the
/// names below (`Mouse1` to `Mouse5`: left, right, middle, back, forward).
fn key_named(name: &str) -> Option<u8> {
    let name = name.to_ascii_lowercase();
    Some(match name.as_str() {
        "space" => 0x20,
        "enter" => 0x0D,
        "esc" => 0x1B,
        "tab" => 0x09,
        "back" => 0x08,
        "caps" => 0x14,
        "up" => 0x26,
        "down" => 0x28,
        "left" => 0x25,
        "right" => 0x27,
        "shift" | "lshift" => 0xA0,
        "rshift" => 0xA1,
        "ctrl" | "lctrl" => 0xA2,
        "rctrl" => 0xA3,
        "alt" | "lalt" => 0xA4,
        "ralt" => 0xA5,
        "mouse1" => MOUSE_LEFT,
        "mouse2" => MOUSE_RIGHT,
        "mouse3" => MOUSE_MIDDLE,
        "mouse4" => MOUSE_BACK,
        "mouse5" => MOUSE_FORWARD,
        k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphanumeric() => {
            k.as_bytes()[0].to_ascii_uppercase()
        }
        _ => return None,
    })
}

/// What `VICTORY_PAD_KEYS` binds a key to, by its name: a button (`A`, `B`,
/// `X`, `Y`, `LB`, `RB`, `LS`, `RS` for the sticks' clicks, `Up`, `Down`,
/// `Left`, `Right`, `Menu`, `View`), a trigger (`LT`, `RT`), a stick's
/// direction (`LUp`, `LDown`, `LLeft`, `LRight`, `RUp`...), or `none`.
fn to_named(name: &str) -> Option<Option<To>> {
    let name = name.to_ascii_lowercase();
    Some(Some(match name.as_str() {
        "none" => return Some(None),
        "a" => To::Button(A),
        "b" => To::Button(B),
        "x" => To::Button(X),
        "y" => To::Button(Y),
        "lb" => To::Button(LEFT_SHOULDER),
        "rb" => To::Button(RIGHT_SHOULDER),
        "ls" => To::Button(LEFT_STICK),
        "rs" => To::Button(RIGHT_STICK),
        "up" => To::Button(UP),
        "down" => To::Button(DOWN),
        "left" => To::Button(LEFT),
        "right" => To::Button(RIGHT),
        "menu" => To::Button(MENU),
        "view" => To::Button(VIEW),
        "lt" => To::LeftTrigger,
        "rt" => To::RightTrigger,
        "lleft" => To::Stick(0, -1.0),
        "lright" => To::Stick(0, 1.0),
        "lup" => To::Stick(LEFT_Y, 1.0),
        "ldown" => To::Stick(LEFT_Y, -1.0),
        "rleft" => To::Stick(2, -1.0),
        "rright" => To::Stick(2, 1.0),
        "rup" => To::Stick(3, 1.0),
        "rdown" => To::Stick(3, -1.0),
        _ => return None,
    }))
}

/// `table` with what `asked` changes of it (`VICTORY_PAD_KEYS`:
/// `<key>=<what>,...`, as `F=LS,G=Y,Mouse4=none`): a key named does only
/// what is said for it there. Beside it, the entries not understood.
fn rebound<'a>(table: &[(u8, To)], asked: &'a str) -> (Vec<(u8, To)>, Vec<&'a str>) {
    let mut table = table.to_vec();
    let mut refused = Vec::new();
    for entry in asked.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let bound = entry
            .split_once('=')
            .and_then(|(key, to)| Some((key_named(key.trim())?, to_named(to.trim())?)));
        match bound {
            Some((key, to)) => {
                table.retain(|&(k, _)| k != key);
                table.extend(to.map(|to| (key, to)));
            }
            None => refused.push(entry),
        }
    }
    (table, refused)
}

/// The stand-in pad's table in a vehicle for this run: [`DRIVING`] over the
/// keys [`bindings`] has, except those `VICTORY_PAD_KEYS` names.
fn driving_bindings() -> &'static [(u8, To)] {
    static TABLE: LazyLock<Vec<(u8, To)>> = LazyLock::new(|| {
        let asked = std::env::var("VICTORY_PAD_KEYS").unwrap_or_default();
        driving_table(bindings(), &asked)
    });
    &TABLE
}

/// `on_foot` with [`DRIVING`] in place of what its keys do, but for the keys
/// `asked` (`VICTORY_PAD_KEYS`) binds.
fn driving_table(on_foot: &[(u8, To)], asked: &str) -> Vec<(u8, To)> {
    let named: Vec<u8> = asked
        .split(',')
        .filter_map(|e| key_named(e.split_once('=')?.0.trim()))
        .collect();
    let mut table = on_foot.to_vec();
    for &(key, to) in &DRIVING {
        if named.contains(&key) {
            continue;
        }
        table.retain(|&(k, _)| k != key);
        table.push((key, to));
    }
    table
}

/// The stand-in pad's table for this run.
fn bindings() -> &'static [(u8, To)] {
    static TABLE: LazyLock<Vec<(u8, To)>> = LazyLock::new(|| {
        let asked = std::env::var("VICTORY_PAD_KEYS").unwrap_or_default();
        let (table, refused) = rebound(&KEYS, &asked);
        for entry in refused {
            tracing::warn!(entry, "VICTORY_PAD_KEYS: not understood; expected <key>=<button>");
        }
        table
    });
    &TABLE
}

/// What the mouse gives the stand-in pad while its pointer is taken.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Mouse {
    /// The buttons held: bit `n` is virtual key `n` ([`MOUSE_LEFT`]...).
    buttons: u8,
    /// The right stick its travel stands for ([`Look`]).
    look: (f32, f32),
}

/// `VICTORY_MOUSE`, the numbers of [`Look`]: `<speed>[,<dead zone>[,<curve>]]`.
#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq)]
struct Tune {
    /// The pointer's speed, in counts a second, that is the whole stick.
    full: f32,
    /// The least the stick is pushed for any travel: the title takes a stick
    /// nearer its middle than its own dead zone for one at rest.
    dead: f32,
    /// The power the share of the full speed is raised to (1: none; below 1
    /// against a title that squares the stick).
    curve: f32,
}

/// The whole stick at speed 1: 4,000 counts a second, 12.7 cm of an 800 dpi
/// mouse. shortcut: worked out and not tried in the title: with a view that
/// turns 150 degrees a second at the whole stick, a turn right round is 30 cm.
#[cfg(any(windows, test))]
const FULL_SPEED: f32 = 4000.0;
/// shortcut: the title's own dead zone is not known; a little over the 0.2
/// most titles have, so that slow travel still turns the view.
#[cfg(any(windows, test))]
const DEAD_ZONE: f32 = 0.22;

/// `None`: the mouse is nobody's (`VICTORY_MOUSE=0`, as before this fork had
/// it). Numbers left out or not understood are the defaults.
#[cfg(any(windows, test))]
fn tune_from(value: Option<&str>) -> Option<Tune> {
    let value = value.unwrap_or_default().trim();
    if value == "0" || value.eq_ignore_ascii_case("off") {
        return None;
    }
    let mut numbers = value.split(',').map(|n| n.trim().parse::<f32>().ok());
    let mut next = |least: f32, most: f32, unset: f32| {
        numbers
            .next()
            .flatten()
            .filter(|n| (least..=most).contains(n))
            .unwrap_or(unset)
    };
    Some(Tune {
        full: FULL_SPEED / next(0.01, 100.0, 1.0),
        dead: next(0.0, 0.9, DEAD_ZONE),
        curve: next(0.2, 5.0, 1.0),
    })
}

/// The pointer's travel as a stick: of the last `window` microseconds, as a
/// speed. The title reads the stick once for each of its frames and turns
/// the view by it for the frame's whole length, so the travel of one frame's
/// time, given as that frame's speed, turns the view as far as the pointer
/// went, at 30 frames a second and at 5 alike. (A shorter time, and travel
/// between two readings would turn nothing; a longer one, and it would turn
/// twice.)
#[cfg(any(windows, test))]
#[derive(Default)]
struct Look {
    /// Travel no older than the window: when, counts right, counts down.
    travel: VecDeque<(u64, i32, i32)>,
}

#[cfg(any(windows, test))]
impl Look {
    /// The right stick (up positive) at `now`, the pointer having gone
    /// `moved` counts since the last asking.
    fn stick(&mut self, now: u64, moved: (i32, i32), window: u64, tune: &Tune) -> (f32, f32) {
        if moved != (0, 0) {
            self.travel.push_back((now, moved.0, moved.1));
        }
        while self.travel.front().is_some_and(|&(at, ..)| at + window <= now) {
            self.travel.pop_front();
        }
        let (x, y) = self
            .travel
            .iter()
            .fold((0, 0), |(x, y), &(_, right, down)| (x + i64::from(right), y + i64::from(down)));
        if (x, y) == (0, 0) {
            return (0.0, 0.0);
        }
        let seconds = window as f32 / 1e6;
        let (x, y) = (x as f32 / seconds, y as f32 / seconds);
        let speed = x.hypot(y);
        let share = (speed / tune.full).min(1.0);
        let pushed = tune.dead + (1.0 - tune.dead) * share.powf(tune.curve);
        (x / speed * pushed, -y / speed * pushed)
    }
}

/// When the title last asked for a pad's reading, and the time between its
/// last two askings (microseconds of [`now`]): the length of its frame of
/// input ([`look_window`]).
static ASKED: AtomicU64 = AtomicU64::new(0);
static ASKED_EVERY: AtomicU64 = AtomicU64::new(0);

fn asked() {
    let now = now();
    let before = ASKED.swap(now, Ordering::Relaxed);
    // Several askings in one frame (per device, per kind) are one.
    if before != 0 && now.saturating_sub(before) >= 4_000 {
        ASKED_EVERY.store(now - before, Ordering::Relaxed);
    }
}

/// The time whose travel [`Look`] takes: the title's frame of input, no
/// shorter than a frame of the 30 a second it is made for (a title that
/// asks more often still turns the view once a frame) and no longer than a
/// quarter of a second (it stands still, or loads).
#[cfg(any(windows, test))]
fn look_window() -> u64 {
    ASKED_EVERY.load(Ordering::Relaxed).clamp(33_333, 250_000)
}

/// What F1, F2 and the "get in" key switch, each at the moment it goes
/// down (with a window of ours in front: other keys are not seen,
/// [`Source::keys`]).
#[derive(Default)]
struct Switches {
    down: [bool; 3],
    /// F1: the pointer is the user's, not the stand-in pad's.
    pointer_free: bool,
    /// The keys are those of a vehicle ([`DRIVING`]).
    driving: bool,
    /// Until when the title's trigger rumble says a vehicle is driven
    /// ([`Switches::rumbled`]), if it switched the keys.
    rumble_until: Option<Instant>,
}

/// Whether the keys switch to a vehicle's by themselves (F, the title's
/// trigger rumble): unless `VICTORY_AUTO_DRIVE=0`.
fn auto_drive() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| std::env::var_os("VICTORY_AUTO_DRIVE").is_none_or(|v| v != "0"));
    *ON
}

/// The key that gets in and out of vehicles (F, while it is bound to Y): it
/// switches the driving keys too, as the title does not say when it has put
/// its player in a vehicle (`VICTORY_AUTO_DRIVE=0`: it does not; F2 alone
/// does).
/// shortcut: a press that gets nobody in or out (no vehicle near) switches
/// as well; F2 puts it right.
fn get_in_key() -> Option<u8> {
    static KEY: LazyLock<Option<u8>> = LazyLock::new(|| {
        if !auto_drive() {
            return None;
        }
        bindings().iter().any(|&(key, to)| key == b'F' && to == To::Button(Y)).then_some(b'F')
    });
    *KEY
}

/// How long the driving keys stay after the title last rumbled a trigger.
const RUMBLE_HOLDS: Duration = Duration::from_secs(3);

impl Switches {
    fn note(&mut self, keys: &[KeyState]) {
        self.note_with(keys, get_in_key());
        if self.rumble_until.is_some_and(|until| Instant::now() > until) {
            self.rumble_until = None;
            if self.driving {
                self.driving = false;
                tracing::info!("the title stopped rumbling the triggers: the stand-in pad's keys are those on foot");
            }
        }
    }

    /// The title rumbles the stand-in pad's triggers: it does so for a
    /// vehicle's engine and brakes (its maker's earlier games on this
    /// console did), so its player drives one, and the keys are those of a
    /// vehicle until it stops for [`RUMBLE_HOLDS`] (`VICTORY_AUTO_DRIVE=0`:
    /// not). A session that begins in a boat or a car has had no F pressed.
    /// shortcut: not seen in the title yet: it may rumble the triggers for
    /// other things too, or not at all (F and F2 still switch).
    fn rumbled(&mut self, now: Instant) {
        if !auto_drive() {
            return;
        }
        if !self.driving {
            self.driving = true;
            tracing::info!("the title rumbles the triggers: the stand-in pad's keys are those of a vehicle");
        }
        self.rumble_until = Some(now + RUMBLE_HOLDS);
    }

    fn note_with(&mut self, keys: &[KeyState], get_in: Option<u8>) {
        let held = |key: Option<u8>| key.is_some_and(|key| keys.iter().any(|k| k.virtual_key == key));
        let down = [held(Some(F1)), held(Some(F2)) || held(Some(P)), held(get_in)];
        if down[0] && !self.down[0] {
            self.pointer_free = !self.pointer_free;
            tracing::info!(free = self.pointer_free, "F1: the mouse's pointer");
        }
        if down[1] && !self.down[1] {
            self.driving = !self.driving;
            tracing::info!(driving = self.driving, "P or F2: the stand-in pad's driving keys");
        }
        if down[2] && !self.down[2] {
            self.driving = !self.driving;
            tracing::info!(driving = self.driving, "F (get in or out): the stand-in pad's driving keys");
        }
        self.down = down;
    }
}

/// A gamepad reading: triggers 0 to 1, sticks -1 to 1 (up positive).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PadState {
    buttons: u32,
    left_trigger: f32,
    right_trigger: f32,
    left_x: f32,
    left_y: f32,
    right_x: f32,
    right_y: f32,
}

/// One key held down: scan code (0xE0xx when extended), character, key.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct KeyState {
    scan_code: u32,
    code_point: u32,
    virtual_key: u8,
    dead_key: bool,
}

const _: () = assert!(size_of::<KeyState>() == 12 && size_of::<PadState>() == 28);

/// Host input, or a fake in tests.
trait Source: Send {
    /// The pad in host slot `slot`, `None` when the slot is empty.
    fn pad(&mut self, slot: usize) -> Option<PadState>;
    /// Keys held down; none while the title's window lacks focus.
    fn keys(&mut self) -> Vec<KeyState>;
    /// The mouse, its pointer taken for the stand-in pad if `take` and a
    /// window of the title is in front; else given back, and nothing.
    fn mouse(&mut self, take: bool) -> Mouse;
    /// Motor speeds: low frequency (left), high frequency (right).
    fn rumble(&mut self, slot: usize, low: u16, high: u16);
}

/// The stand-in pad for the keys and mouse buttons held, by `table`.
/// `driving`: the keys of the left stick's up and down are the right and the
/// left trigger instead ([`Switches`]).
fn stand_in(table: &[(u8, To)], keys: &[KeyState], mouse: Mouse, driving: bool) -> PadState {
    let held = |key: u8| {
        keys.iter().any(|k| k.virtual_key == key) || (key < 8 && mouse.buttons & (1 << key) != 0)
    };
    let mut pad = PadState::default();
    let mut sticks = [0.0_f32; 4];
    for &(key, to) in table {
        if !held(key) {
            continue;
        }
        let to = match to {
            To::Stick(LEFT_Y, way) if driving && way > 0.0 => To::RightTrigger,
            To::Stick(LEFT_Y, _) if driving => To::LeftTrigger,
            to => to,
        };
        match to {
            To::Button(button) => pad.buttons |= button,
            To::LeftTrigger => pad.left_trigger = 1.0,
            To::RightTrigger => pad.right_trigger = 1.0,
            To::Stick(axis, way) => sticks[axis] += way,
        }
    }
    let [left_x, left_y, right_x, right_y] = sticks.map(|s| s.clamp(-1.0, 1.0));
    // The right stick is the keys' or the mouse's, whichever pushes further.
    let look = PadState {
        right_x: mouse.look.0,
        right_y: mouse.look.1,
        ..Default::default()
    };
    merged(PadState { left_x, left_y, right_x, right_y, ..pad }, look)
}

fn fallback(keys: &[KeyState], mouse: Mouse, driving: bool) -> PadState {
    if driving {
        stand_in(driving_bindings(), keys, mouse, true)
    } else {
        stand_in(bindings(), keys, mouse, false)
    }
}

/// Two pads as one: buttons of both, and of each axis and trigger the one
/// pushed further.
fn merged(a: PadState, b: PadState) -> PadState {
    let further = |x: f32, y: f32| if y.abs() > x.abs() { y } else { x };
    PadState {
        buttons: a.buttons | b.buttons,
        left_trigger: further(a.left_trigger, b.left_trigger),
        right_trigger: further(a.right_trigger, b.right_trigger),
        left_x: further(a.left_x, b.left_x),
        left_y: further(a.left_y, b.left_y),
        right_x: further(a.right_x, b.right_x),
        right_y: further(a.right_y, b.right_y),
    }
}

/// Whether the title sees one pad that is always there, fed by the keyboard
/// and by every host pad (on by default; `VICTORY_ONE_PAD=0` shows each host
/// pad as a device of its own, and the stand-in only while there is none).
/// The title keeps to the pad it started with: with the stand-in taken away
/// when a pad was switched on, it showed its controller-disconnected screen
/// until the pad was off again. It also keeps a pad the host lists twice (its
/// own slot and a remapper's) from being two players.
fn one_pad() -> bool {
    std::env::var_os("VICTORY_ONE_PAD").is_none_or(|v| v != "0")
}

#[repr(C)]
struct Table<const N: usize>([*const (); N]);
// SAFETY: function addresses only.
unsafe impl<const N: usize> Sync for Table<N> {}

/// The input object; one per hub.
#[repr(C)]
pub struct Object {
    vtable: &'static Table<22>,
    hub: *const Hub,
}

#[repr(C)]
pub struct Device {
    vtable: &'static Table<21>,
    hub: *const Hub,
    index: usize,
    info: DeviceInfo,
}

/// What a device says about itself. The title reads `id` (+0x20); the rest
/// follows the public layout and is zero where we have nothing to say.
#[repr(C)]
#[derive(Default)]
#[allow(dead_code)] // read by title code
struct DeviceInfo {
    size: u32,
    vendor: u16,
    product: u16,
    revision: u16,
    interface: u8,
    collection: u8,
    usage: [u16; 2],
    versions: [u16; 8],
    id: [u8; 32],
    root_id: [u8; 32],
    family: i32,
    capabilities: u32,
    supported_input: u32,
    rumble_motors: u32,
    /// Counts and optional detail pointers.
    rest: [u64; 26],
}

const _: () = assert!(std::mem::offset_of!(DeviceInfo, id) == 0x20);

fn kind(index: usize) -> u32 {
    if index == KEYBOARD {
        KEYBOARD_KIND
    } else {
        GAMEPAD_KIND
    }
}

impl Device {
    fn new(index: usize) -> Device {
        let mut id = [0; 32];
        id[..7].copy_from_slice(b"victory");
        id[31] = index as u8 + 1;
        let info = DeviceInfo {
            size: size_of::<DeviceInfo>() as u32,
            id,
            root_id: id,
            supported_input: kind(index),
            // Low and high frequency; the stand-in pad's triggers too, so
            // that the title says when it rumbles them ([`Switches::rumbled`]).
            rumble_motors: if index < FALLBACK {
                0x3
            } else if index == FALLBACK {
                0xF
            } else {
                0
            },
            ..Default::default()
        };
        Device {
            vtable: &DEVICE,
            hub: std::ptr::null(),
            index,
            info,
        }
    }

    fn hub(&self) -> &'static Hub {
        // SAFETY: set by `Hub::new`; hubs are never freed.
        unsafe { &*self.hub }
    }
}

#[derive(PartialEq)]
enum Value {
    Pad(PadState),
    Keys(Vec<KeyState>),
}

#[repr(C)]
pub struct Reading {
    vtable: &'static Table<25>,
    refs: AtomicU32,
    device: *const Device,
    sequence: u64,
    timestamp: u64,
    value: Value,
}

impl Reading {
    fn kind(&self) -> u32 {
        match self.value {
            Value::Pad(_) => GAMEPAD_KIND,
            Value::Keys(_) => KEYBOARD_KIND,
        }
    }
}

/// A counted reference to a reading.
struct Ref(NonNull<Reading>);

impl Ref {
    fn new(reading: Reading) -> Ref {
        Ref(NonNull::from(Box::leak(Box::new(reading))))
    }

    /// Another reference, for the title.
    fn share(&self) -> *const Reading {
        reading_add_ref(self.0.as_ptr());
        self.0.as_ptr()
    }
}

impl Deref for Ref {
    type Target = Reading;
    fn deref(&self) -> &Reading {
        // SAFETY: we hold a reference, so it is alive.
        unsafe { self.0.as_ref() }
    }
}

impl Drop for Ref {
    fn drop(&mut self) {
        reading_release(self.0.as_ptr());
    }
}

type DeviceCallback = extern "system" fn(u64, *mut c_void, *const Device, u64, u32, u32);

#[derive(Clone, Copy)]
struct Registration {
    token: u64,
    /// One device's address, or 0 for any.
    device: usize,
    kind: u32,
    status: u32,
    context: usize,
    callback: DeviceCallback,
}

impl Registration {
    fn matches(&self, devices: &[Device; DEVICES], index: usize) -> bool {
        kind(index) & self.kind != 0
            && (self.device == 0 || self.device == &devices[index] as *const Device as usize)
    }

    fn event(&self, device: usize, timestamp: u64, current: u32, previous: u32) -> Event {
        Event {
            registration: *self,
            device,
            timestamp,
            current,
            previous,
        }
    }
}

struct Event {
    registration: Registration,
    device: usize,
    timestamp: u64,
    current: u32,
    previous: u32,
}

#[derive(Default)]
struct Slot {
    connected: bool,
    sequence: u64,
    history: VecDeque<Ref>,
}

struct State {
    slots: [Slot; DEVICES],
    callbacks: Vec<Registration>,
    /// Asynchronous enumerations, delivered by the next poll.
    pending: Vec<Event>,
    /// Device callbacks running now: token and thread.
    running: Vec<(u64, ThreadId)>,
}

pub struct Hub {
    object: Object,
    devices: [Device; DEVICES],
    /// Apart from the state: XInput can be slow, and the title reads
    /// readings every frame.
    source: Mutex<Box<dyn Source>>,
    /// Taken with the source's lock held.
    switches: Mutex<Switches>,
    /// The title sees one pad ([`one_pad`]).
    one_pad: bool,
    state: Mutex<State>,
    /// A device callback returned.
    returned: Condvar,
}

fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// SAFETY: the back pointers are set before the hub is shared and never
// change; what changes sits behind the mutex or is atomic.
unsafe impl Send for Hub {}
// SAFETY: as above.
unsafe impl Sync for Hub {}

static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Microseconds, the clock of every timestamp we hand out.
fn now() -> u64 {
    START.elapsed().as_micros() as u64
}

impl Hub {
    fn new(source: Box<dyn Source>, one_pad: bool) -> &'static Hub {
        let hub = Box::into_raw(Box::new(Hub {
            object: Object {
                vtable: &INPUT,
                hub: std::ptr::null(),
            },
            devices: std::array::from_fn(Device::new),
            source: Mutex::new(source),
            switches: Mutex::default(),
            one_pad,
            state: Mutex::new(State {
                slots: Default::default(),
                callbacks: Vec::new(),
                pending: Vec::new(),
                running: Vec::new(),
            }),
            returned: Condvar::new(),
        }));
        // SAFETY: just allocated and never freed; nothing else sees it yet.
        unsafe {
            (*hub).object.hub = hub;
            for device in &mut (*hub).devices {
                device.hub = hub;
            }
            &*hub
        }
    }

    /// A hub over the host's input, polled from now on.
    fn start(source: Box<dyn Source>) -> &'static Hub {
        let hub = Hub::new(source, one_pad());
        hub.poll();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(POLL);
                hub.poll();
            }
        });
        hub
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Reads the host once; device callbacks run on the calling thread.
    fn poll(&self) {
        let (pads, keys, mouse, driving) = {
            let mut source = lock(&self.source);
            let pads: [_; PADS] = std::array::from_fn(|slot| source.pad(slot));
            let keys = source.keys();
            let mut switches = lock(&self.switches);
            switches.note(&keys);
            let mouse = source.mouse(!switches.pointer_free);
            (pads, keys, mouse, switches.driving)
        };
        let stand_in = fallback(&keys, mouse, driving);
        let events = self.state().poll(&self.devices, self.one_pad, pads, keys, stand_in);
        self.deliver(events);
    }

    /// Runs callbacks, never under our lock: they call back into us. One
    /// unregistered since the event was taken does not run, and
    /// `UnregisterCallback` waits for one that runs.
    fn deliver(&self, events: Vec<Event>) {
        let me = std::thread::current().id();
        for e in events {
            let r = e.registration;
            {
                let mut state = self.state();
                if !state.callbacks.iter().any(|c| c.token == r.token) {
                    continue;
                }
                state.running.push((r.token, me));
            }
            (r.callback)(
                r.token,
                r.context as *mut c_void,
                &self.devices[e.device],
                e.timestamp,
                e.current,
                e.previous,
            );
            let mut state = self.state();
            if let Some(i) = state.running.iter().position(|&x| x == (r.token, me)) {
                state.running.swap_remove(i);
            }
            self.returned.notify_all();
        }
    }

    fn index(&self, device: *const Device) -> Option<usize> {
        self.devices.iter().position(|d| std::ptr::eq(d, device))
    }
}

impl State {
    fn poll(
        &mut self,
        devices: &[Device; DEVICES],
        one_pad: bool,
        pads: [Option<PadState>; PADS],
        keys: Vec<KeyState>,
        // What the keys and the mouse make of the stand-in pad ([`fallback`]).
        stand_in: PadState,
    ) -> Vec<Event> {
        let now = now();
        let mut events = std::mem::take(&mut self.pending);
        if one_pad {
            let pad = pads.into_iter().flatten().fold(stand_in, merged);
            self.update(devices, FALLBACK, Some(Value::Pad(pad)), now, &mut events);
            self.update(devices, KEYBOARD, Some(Value::Keys(keys)), now, &mut events);
            return events;
        }
        let any_pad = pads.iter().any(Option::is_some);
        for (slot, pad) in pads.into_iter().enumerate() {
            self.update(devices, slot, pad.map(Value::Pad), now, &mut events);
        }
        let fallback = (!any_pad).then_some(Value::Pad(stand_in));
        self.update(devices, FALLBACK, fallback, now, &mut events);
        self.update(devices, KEYBOARD, Some(Value::Keys(keys)), now, &mut events);
        events
    }

    /// Records a device's state: a reading when it changed, callbacks when
    /// the device came or went.
    fn update(
        &mut self,
        devices: &[Device; DEVICES],
        index: usize,
        value: Option<Value>,
        now: u64,
        events: &mut Vec<Event>,
    ) {
        let slot = &mut self.slots[index];
        let was = slot.connected;
        slot.connected = value.is_some();
        if let Some(value) = value
            && (!was || slot.history.back().is_none_or(|r| r.value != value))
        {
            slot.sequence += 1;
            if slot.history.len() == HISTORY {
                slot.history.pop_front();
            }
            slot.history.push_back(Ref::new(Reading {
                vtable: &READING,
                refs: AtomicU32::new(1),
                device: &devices[index],
                sequence: slot.sequence,
                timestamp: now,
                value,
            }));
        }
        if was != slot.connected {
            tracing::info!(device = index, connected = !was, "input device");
            let (current, previous) = if was {
                (0, STATUS_CONNECTED)
            } else {
                (STATUS_CONNECTED, 0)
            };
            events.extend(
                self.callbacks
                    .iter()
                    .filter(|r| r.matches(devices, index) && r.status & STATUS_CONNECTED != 0)
                    .map(|r| r.event(index, now, current, previous)),
            );
        }
    }
}

#[cfg(windows)]
fn host() -> Box<dyn Source> {
    Box::new(host::Host::default())
}

#[cfg(not(windows))]
fn host() -> Box<dyn Source> {
    struct Nothing;
    impl Source for Nothing {
        fn pad(&mut self, _: usize) -> Option<PadState> {
            None
        }
        fn keys(&mut self) -> Vec<KeyState> {
            Vec::new()
        }
        fn mouse(&mut self, _: bool) -> Mouse {
            Mouse::default()
        }
        fn rumble(&mut self, _: usize, _: u16, _: u16) {}
    }
    Box::new(Nothing)
}

/// How the pointer stands over the title's picture, for the window's own
/// procedure (`lifecycle`): 0, the mouse is not read (nothing changes for the
/// window); 1, the pointer is the user's; 2, it is taken and none is shown.
#[cfg(windows)]
static POINTER: AtomicU32 = AtomicU32::new(0);
#[cfg(windows)]
const POINTER_FREE: u32 = 1;
#[cfg(windows)]
const POINTER_TAKEN: u32 = 2;

/// Whether the stand-in pad has taken the pointer; `None` while the mouse is
/// not read at all.
#[cfg(windows)]
pub(crate) fn pointer_taken() -> Option<bool> {
    match POINTER.load(Ordering::Relaxed) {
        0 => None,
        state => Some(state == POINTER_TAKEN),
    }
}

static HUB: LazyLock<&'static Hub> = LazyLock::new(|| Hub::start(host()));

type This = *const Object;
type Out = *mut *const c_void;

/// The DLL's only export (ordinal 1): the input object, static and shared.
pub extern "system" fn GameInputCreate(out: *mut This) -> HRESULT {
    tracing::info!("GameInputCreate");
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title passes room for a pointer.
    unsafe { out.write(&HUB.object) };
    S_OK
}

fn hub(this: This) -> &'static Hub {
    // SAFETY: `this` is a hub's object; hubs are never freed.
    unsafe { &*(*this).hub }
}

/// Clears a COM out pointer and returns `hr`.
fn fail(out: Out, hr: HRESULT) -> HRESULT {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a pointer.
        unsafe { out.write(std::ptr::null()) };
    }
    hr
}

/// Gives the title a reference to `reading`, or fails as with none.
fn hand(out: Out, reading: Option<&Ref>) -> HRESULT {
    match reading {
        _ if out.is_null() => E_INVALIDARG,
        Some(reading) => {
            // SAFETY: checked non-null; the title passes room for a pointer.
            unsafe { out.write(reading.share().cast()) };
            S_OK
        }
        None => fail(out, READING_NOT_FOUND),
    }
}

extern "system" fn no_interface(_: *const c_void, _iid: *const c_void, out: Out) -> HRESULT {
    tracing::warn!("input QueryInterface not implemented");
    fail(out, E_NOINTERFACE)
}
/// The input object and devices live as long as the process.
extern "system" fn add_ref(_: *const c_void) -> u32 {
    1
}

extern "system" fn timestamp(_: This) -> u64 {
    now()
}

extern "system" fn current_reading(
    this: This,
    kind: u32,
    device: *const Device,
    out: Out,
) -> HRESULT {
    if kind & GAMEPAD_KIND != 0 {
        asked();
    }
    let hub = hub(this);
    let state = hub.state();
    let newest = |i: usize| {
        state.slots[i]
            .history
            .back()
            .filter(|r| r.kind() & kind != 0)
    };
    let found = if device.is_null() {
        (0..DEVICES)
            .filter(|&i| state.slots[i].connected)
            .filter_map(newest)
            .max_by_key(|r| r.timestamp)
    } else {
        match hub.index(device) {
            None => return fail(out, E_INVALIDARG),
            Some(i) if !state.slots[i].connected => return fail(out, DEVICE_DISCONNECTED),
            Some(i) => newest(i),
        }
    };
    tracing::trace!(
        kind = format_args!("{kind:#x}"),
        device = ?hub.index(device),
        sequence = found.map(|r| r.sequence),
        "current reading"
    );
    hand(out, found)
}

/// The reading after (`forward`) or before `reference` on its device.
fn relative(
    this: This,
    reference: *const Reading,
    kind: u32,
    device: *const Device,
    out: Out,
    forward: bool,
) -> HRESULT {
    if reference.is_null() {
        return fail(out, E_INVALIDARG);
    }
    let hub = hub(this);
    // SAFETY: a reading we handed out and the title still holds.
    let reference = unsafe { &*reference };
    // shortcut: with no device given only the reference's device is walked,
    // not every device's readings in time order; the title walks
    // only keyboard readings (one device) and the current pad's previous one.
    let Some(index) = hub.index(reference.device) else {
        return fail(out, E_INVALIDARG);
    };
    if !device.is_null() && !std::ptr::eq(device, reference.device) {
        return fail(out, E_INVALIDARG);
    }
    let state = hub.state();
    let history = &state.slots[index].history;
    if history
        .front()
        .is_none_or(|oldest| reference.sequence < oldest.sequence)
    {
        return fail(out, REFERENCE_TOO_OLD);
    }
    let wanted = |r: &&Ref| r.kind() & kind != 0;
    let found = if forward {
        history
            .iter()
            .filter(|r| r.sequence > reference.sequence)
            .find(wanted)
    } else {
        history
            .iter()
            .rev()
            .filter(|r| r.sequence < reference.sequence)
            .find(wanted)
    };
    hand(out, found)
}
extern "system" fn next_reading(
    this: This,
    reference: *const Reading,
    kind: u32,
    device: *const Device,
    out: Out,
) -> HRESULT {
    relative(this, reference, kind, device, out, true)
}
extern "system" fn previous_reading(
    this: This,
    reference: *const Reading,
    kind: u32,
    device: *const Device,
    out: Out,
) -> HRESULT {
    relative(this, reference, kind, device, out, false)
}
/// The device's reading at `time`, among those still kept.
extern "system" fn temporal_reading(
    this: This,
    time: u64,
    device: *const Device,
    out: Out,
) -> HRESULT {
    let hub = hub(this);
    let Some(index) = hub.index(device) else {
        return fail(out, E_INVALIDARG);
    };
    let state = hub.state();
    let history = &state.slots[index].history;
    hand(out, history.iter().rev().find(|r| r.timestamp <= time))
}

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

fn token(out: *mut u64) -> HRESULT {
    if !out.is_null() {
        // SAFETY: checked non-null.
        unsafe { out.write(NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)) };
    }
    S_OK
}

// shortcut: reading callbacks never run; the title registers none
// (it polls).
extern "system" fn register_reading(
    _: This,
    _device: *const c_void,
    _kind: u32,
    _threshold: f32,
    _context: *mut c_void,
    _callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    tracing::warn!("input reading callbacks are never called");
    token(out)
}

/// Device callbacks run on our poll thread when a device comes or goes.
/// Enumeration reports the devices already there: blocking on the caller's
/// thread before we return (the title registers holding a lock its callback
/// takes again), asynchronous on the next poll.
extern "system" fn register_device(
    this: This,
    device: *const Device,
    kind: u32,
    status: u32,
    enumeration: u32,
    context: *mut c_void,
    callback: Option<DeviceCallback>,
    out: *mut u64,
) -> HRESULT {
    let Some(callback) = callback else {
        return E_INVALIDARG;
    };
    let hub = hub(this);
    let registration = Registration {
        token: NEXT_TOKEN.fetch_add(1, Ordering::Relaxed),
        device: device as usize,
        kind,
        status,
        context: context as usize,
        callback,
    };
    let mut found = Vec::new();
    {
        let mut state = hub.state();
        if enumeration != 0 && status & STATUS_CONNECTED != 0 {
            let now = now();
            found = (0..DEVICES)
                .filter(|&i| state.slots[i].connected && registration.matches(&hub.devices, i))
                .map(|i| registration.event(i, now, STATUS_CONNECTED, 0))
                .collect();
        }
        tracing::info!(
            kind = format_args!("{kind:#x}"),
            enumeration,
            devices = found.len(),
            "input device callback"
        );
        state.callbacks.push(registration);
        if enumeration == ASYNC_ENUMERATION {
            state.pending.append(&mut found);
        }
    }
    if !out.is_null() {
        // SAFETY: checked non-null.
        unsafe { out.write(registration.token) };
    }
    hub.deliver(found);
    S_OK
}
/// Guide button and keyboard layout callbacks: neither ever changes here.
extern "system" fn register_other(
    _: This,
    _device: *const c_void,
    _context: *mut c_void,
    _callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    token(out)
}

fn forget(this: This, token: u64) {
    let mut state = hub(this).state();
    state.callbacks.retain(|r| r.token != token);
    state.pending.retain(|e| e.registration.token != token);
}
extern "system" fn stop_callback(this: This, token: u64) {
    forget(this, token);
}
/// Stops the callback, then waits up to `timeout` µs for a call of it that
/// is running; false on time-out, or from inside that callback (it cannot
/// wait for itself), as documented. Either way it is never called again.
extern "system" fn unregister_callback(this: This, token: u64, timeout: u64) -> bool {
    forget(this, token);
    let hub = hub(this);
    let me = std::thread::current().id();
    let deadline = Instant::now().checked_add(Duration::from_micros(timeout));
    let mut state = hub.state();
    loop {
        let running = state.running.iter().filter(|r| r.0 == token);
        if running.clone().any(|r| r.1 == me) {
            return false;
        }
        if running.count() == 0 {
            return true;
        }
        state = match deadline {
            None => hub.returned.wait(state).unwrap_or_else(PoisonError::into_inner),
            Some(deadline) => {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    return false;
                };
                let waited = hub.returned.wait_timeout(state, left);
                waited.unwrap_or_else(PoisonError::into_inner).0
            }
        };
    }
}

/// Dispatcher and aggregate device: no title of ours asks for them.
extern "system" fn create_dispatcher(_: This, out: Out) -> HRESULT {
    tracing::warn!("input dispatcher not implemented");
    fail(out, E_NOTIMPL)
}
extern "system" fn create_aggregate(_: This, _kind: u32, out: Out) -> HRESULT {
    tracing::warn!("aggregate input device not implemented");
    fail(out, E_NOTIMPL)
}
extern "system" fn find_by_id(this: This, id: *const [u8; 32], out: Out) -> HRESULT {
    if id.is_null() || out.is_null() {
        return fail(out, E_INVALIDARG);
    }
    // SAFETY: checked non-null; the title passes a device id.
    let id = unsafe { &*id };
    match hub(this).devices.iter().find(|d| d.info.id == *id) {
        Some(device) => {
            // SAFETY: checked non-null.
            unsafe { out.write((device as *const Device).cast()) };
            S_OK
        }
        None => fail(out, DEVICE_NOT_FOUND),
    }
}
/// Lookup by object, platform handle or platform string: our devices have
/// none of these.
extern "system" fn find_device(_: This, _value: *const c_void, out: Out) -> HRESULT {
    fail(out, DEVICE_NOT_FOUND)
}
extern "system" fn enable_oem_device(
    _: This,
    _vendor: u16,
    _product: u16,
    _interface: u8,
    _collection: u8,
) -> HRESULT {
    S_OK
}
extern "system" fn set_focus_policy(_: This, _policy: u32) {}

static INPUT: Table<22> = Table([
    no_interface as *const (),        // 0x00
    add_ref as *const (),             // 0x08 AddRef
    add_ref as *const (),             // 0x10 Release
    timestamp as *const (),           // 0x18
    current_reading as *const (),     // 0x20
    next_reading as *const (),        // 0x28
    previous_reading as *const (),    // 0x30
    temporal_reading as *const (),    // 0x38
    register_reading as *const (),    // 0x40
    register_device as *const (),     // 0x48
    register_other as *const (),      // 0x50 guide button
    register_other as *const (),      // 0x58 keyboard layout
    stop_callback as *const (),       // 0x60
    unregister_callback as *const (), // 0x68
    create_dispatcher as *const (),   // 0x70
    create_aggregate as *const (),    // 0x78
    find_by_id as *const (),          // 0x80
    find_device as *const (),         // 0x88 by object
    find_device as *const (),         // 0x90 by platform handle
    find_device as *const (),         // 0x98 by platform string
    enable_oem_device as *const (),   // 0xa0
    set_focus_policy as *const (),    // 0xa8
]);

extern "system" fn device_info(this: *const Device) -> *const DeviceInfo {
    // SAFETY: one of our devices.
    unsafe { &(*this).info }
}
extern "system" fn device_status(this: *const Device) -> u32 {
    // SAFETY: one of our devices.
    let device = unsafe { &*this };
    if device.hub().state().slots[device.index].connected {
        STATUS_CONNECTED
    } else {
        0
    }
}
// shortcut: trigger motors are dropped (XInput has none). The host API that
// drives them without a redistributable is Windows.Gaming.Input (WinRT
// `Gamepad.Vibration`), which needs WinRT bindings this crate lacks outside
// diagnostics builds; the title sends trigger levels in slot 0x50.
extern "system" fn set_rumble(this: *const Device, params: *const [f32; 4]) {
    // SAFETY: one of our devices.
    let device = unsafe { &*this };
    let one_pad = device.hub().one_pad;
    if device.index >= PADS && !(one_pad && device.index == FALLBACK) {
        return;
    }
    let [low, high, left, right] = if params.is_null() {
        [0.0; 4]
    } else {
        // SAFETY: checked non-null; four motor levels.
        unsafe { *params }
    };
    // What the title rumbles, said for the first calls with any motor on and
    // then every power of two of them: which device, and the four levels
    // (it is how a vehicle could be told apart, [`Switches::rumbled`]).
    if [low, high, left, right].iter().any(|&l| l > 0.0) {
        static CALLS: AtomicU64 = AtomicU64::new(0);
        let n = CALLS.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 24 || n.is_power_of_two() {
            tracing::info!(device = device.index, n, low, high, left, right, "the title rumbles");
        }
    }
    // The stand-in pad's triggers rumbled: a vehicle ([`Switches::rumbled`]).
    if device.index == FALLBACK && (left > 0.01 || right > 0.01) {
        lock(&device.hub().switches).rumbled(Instant::now());
    }
    let speed = |level: f32| (level.clamp(0.0, 1.0) * f32::from(u16::MAX)) as u16;
    let mut source = lock(&device.hub().source);
    if one_pad {
        // The one pad is every host pad.
        for slot in 0..PADS {
            if source.pad(slot).is_some() {
                source.rumble(slot, speed(low), speed(high));
            }
        }
    } else {
        source.rumble(device.index, speed(low), speed(high));
    }
}
extern "system" fn unsupported<const SLOT: usize>(_: *const c_void) -> HRESULT {
    tracing::warn!(
        slot = format_args!("{SLOT:#x}"),
        "input device method not implemented"
    );
    E_NOTIMPL
}

static DEVICE: Table<21> = Table([
    no_interface as *const (),        // 0x00
    add_ref as *const (),             // 0x08 AddRef
    add_ref as *const (),             // 0x10 Release
    device_info as *const (),         // 0x18
    device_status as *const (),       // 0x20
    unsupported::<0x28> as *const (), // battery
    unsupported::<0x30> as *const (), // force feedback
    unsupported::<0x38> as *const (), //
    unsupported::<0x40> as *const (), //
    unsupported::<0x48> as *const (), // haptics
    set_rumble as *const (),          // 0x50
    unsupported::<0x58> as *const (), // input synchronization
    unsupported::<0x60> as *const (), //
    unsupported::<0x68> as *const (), // power off
    unsupported::<0x70> as *const (), // raw device access
    unsupported::<0x78> as *const (), //
    unsupported::<0x80> as *const (), //
    unsupported::<0x88> as *const (), //
    unsupported::<0x90> as *const (), //
    unsupported::<0x98> as *const (), //
    unsupported::<0xa0> as *const (), //
]);

fn reading<'a>(this: *const Reading) -> &'a Reading {
    // SAFETY: a reading we handed out and the title still holds.
    unsafe { &*this }
}
extern "system" fn reading_add_ref(this: *const Reading) -> u32 {
    reading(this).refs.fetch_add(1, Ordering::Relaxed) + 1
}
extern "system" fn reading_release(this: *const Reading) -> u32 {
    let left = reading(this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        // SAFETY: the last reference is gone; `Ref::new` boxed it.
        drop(unsafe { Box::from_raw(this.cast_mut()) });
    }
    left
}
extern "system" fn input_kind(this: *const Reading) -> u32 {
    reading(this).kind()
}
extern "system" fn sequence_number(this: *const Reading, kind: u32) -> u64 {
    let reading = reading(this);
    if reading.kind() & kind != 0 {
        reading.sequence
    } else {
        0
    }
}
extern "system" fn reading_timestamp(this: *const Reading) -> u64 {
    reading(this).timestamp
}
extern "system" fn reading_device(this: *const Reading, out: *mut *const Device) {
    if !out.is_null() {
        // SAFETY: checked non-null; devices need no reference.
        unsafe { out.write(reading(this).device) };
    }
}
extern "system" fn key_count(this: *const Reading) -> u32 {
    match &reading(this).value {
        Value::Keys(keys) => keys.len() as u32,
        Value::Pad(_) => 0,
    }
}
extern "system" fn key_state(this: *const Reading, count: u32, out: *mut KeyState) -> u32 {
    let Value::Keys(keys) = &reading(this).value else {
        return 0;
    };
    if out.is_null() {
        return 0;
    }
    let n = keys.len().min(count as usize);
    // SAFETY: the title passes room for `count` keys.
    unsafe { std::ptr::copy_nonoverlapping(keys.as_ptr(), out, n) };
    n as u32
}
extern "system" fn gamepad_state(this: *const Reading, out: *mut PadState) -> bool {
    match &reading(this).value {
        Value::Pad(state) if !out.is_null() => {
            // SAFETY: checked non-null.
            unsafe { out.write(*state) };
            true
        }
        _ => false,
    }
}
/// Kinds our devices do not have: no count, no state.
extern "system" fn nothing(_: *const c_void) -> usize {
    0
}

static READING: Table<25> = Table([
    no_interface as *const (),      // 0x00
    reading_add_ref as *const (),   // 0x08
    reading_release as *const (),   // 0x10
    input_kind as *const (),        // 0x18
    sequence_number as *const (),   // 0x20
    reading_timestamp as *const (), // 0x28
    reading_device as *const (),    // 0x30
    nothing as *const (),           // 0x38 raw report
    nothing as *const (),           // 0x40 controller axes, buttons, switches
    nothing as *const (),           // 0x48
    nothing as *const (),           // 0x50
    nothing as *const (),           // 0x58
    nothing as *const (),           // 0x60
    nothing as *const (),           // 0x68
    key_count as *const (),         // 0x70
    key_state as *const (),         // 0x78
    nothing as *const (),           // 0x80 mouse
    nothing as *const (),           // 0x88 touch
    nothing as *const (),           // 0x90
    nothing as *const (),           // 0x98 motion
    nothing as *const (),           // 0xa0 arcade stick
    nothing as *const (),           // 0xa8 flight stick
    gamepad_state as *const (),     // 0xb0
    nothing as *const (),           // 0xb8 racing wheel
    nothing as *const (),           // 0xc0 UI navigation
]);

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "gameinput",
        dlls: &["gameinput.dll"],
        // Titles import it by ordinal only.
        exports: vec![HleExport {
            name: "#1",
            address: GameInputCreate as *const () as usize,
        }],
    }
}

/// Host input: XInput pads, the keyboard while our process has focus, the
/// mouse while a window of the title is in front.
#[cfg(windows)]
mod host {
    use std::time::{Duration, Instant};

    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, MAPVK_VK_TO_VSC_EX, MapVirtualKeyW,
    };
    use windows_sys::Win32::UI::Input::XboxController::{
        XINPUT_GAMEPAD, XINPUT_GAMEPAD_A, XINPUT_GAMEPAD_B, XINPUT_GAMEPAD_BACK,
        XINPUT_GAMEPAD_DPAD_DOWN, XINPUT_GAMEPAD_DPAD_LEFT, XINPUT_GAMEPAD_DPAD_RIGHT,
        XINPUT_GAMEPAD_DPAD_UP, XINPUT_GAMEPAD_LEFT_SHOULDER, XINPUT_GAMEPAD_LEFT_THUMB,
        XINPUT_GAMEPAD_RIGHT_SHOULDER, XINPUT_GAMEPAD_RIGHT_THUMB, XINPUT_GAMEPAD_START,
        XINPUT_GAMEPAD_X, XINPUT_GAMEPAD_Y, XINPUT_STATE, XINPUT_VIBRATION, XInputGetState,
        XInputSetState,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };

    use super::*;

    /// Asking XInput about an empty slot is slow; look again this often.
    const EMPTY_SLOT_RECHECK: Duration = Duration::from_secs(1);

    const BUTTONS: [(u16, u32); 14] = [
        (XINPUT_GAMEPAD_START, MENU),
        (XINPUT_GAMEPAD_BACK, VIEW),
        (XINPUT_GAMEPAD_A, A),
        (XINPUT_GAMEPAD_B, B),
        (XINPUT_GAMEPAD_X, X),
        (XINPUT_GAMEPAD_Y, Y),
        (XINPUT_GAMEPAD_DPAD_UP, UP),
        (XINPUT_GAMEPAD_DPAD_DOWN, DOWN),
        (XINPUT_GAMEPAD_DPAD_LEFT, LEFT),
        (XINPUT_GAMEPAD_DPAD_RIGHT, RIGHT),
        (XINPUT_GAMEPAD_LEFT_SHOULDER, LEFT_SHOULDER),
        (XINPUT_GAMEPAD_RIGHT_SHOULDER, RIGHT_SHOULDER),
        (XINPUT_GAMEPAD_LEFT_THUMB, LEFT_STICK),
        (XINPUT_GAMEPAD_RIGHT_THUMB, RIGHT_STICK),
    ];

    /// Raw values, no dead zone: titles apply their own.
    pub(super) fn convert(pad: &XINPUT_GAMEPAD) -> PadState {
        let stick = |v: i16| (f32::from(v) / 32767.0).max(-1.0);
        PadState {
            buttons: BUTTONS
                .iter()
                .filter(|(host, _)| pad.wButtons & host != 0)
                .fold(0, |all, (_, bit)| all | bit),
            left_trigger: f32::from(pad.bLeftTrigger) / 255.0,
            right_trigger: f32::from(pad.bRightTrigger) / 255.0,
            left_x: stick(pad.sThumbLX),
            left_y: stick(pad.sThumbLY),
            right_x: stick(pad.sThumbRX),
            right_y: stick(pad.sThumbRY),
        }
    }

    #[derive(Default)]
    pub(super) struct Host {
        empty_until: [Option<Instant>; PADS],
        pointer: pointer::Pointer,
    }

    impl Source for Host {
        fn pad(&mut self, slot: usize) -> Option<PadState> {
            if self.empty_until[slot].is_some_and(|until| Instant::now() < until) {
                return None;
            }
            let mut state = XINPUT_STATE::default();
            // SAFETY: room for the state.
            if unsafe { XInputGetState(slot as u32, &mut state) } != 0 {
                self.empty_until[slot] = Some(Instant::now() + EMPTY_SLOT_RECHECK);
                return None;
            }
            self.empty_until[slot] = None;
            #[allow(unused_mut)]
            let mut pad = convert(&state.Gamepad);
            // The scripted keys (`VICTORY_KEYS`) also press the first
            // pad's buttons, by the stand-in pad's table: with a real pad
            // connected the stand-in is not there, and a title that listens
            // to the pad (the title's start screen) saw nothing.
            if slot == 0 {
                for vk in scripted() {
                    for &(key, to) in super::bindings() {
                        if let (true, To::Button(button)) = (key == vk, to) {
                            pad.buttons |= button;
                        }
                    }
                }
            }
            Some(pad)
        }

        fn mouse(&mut self, take: bool) -> Mouse {
            self.pointer.read(take)
        }

        fn keys(&mut self) -> Vec<KeyState> {
            let mut down = scripted();
            if focused() {
                // Left/right modifier keys only, as devices report them; no
                // mouse buttons (below 0x08).
                let held: Vec<u8> = (0x08..=0xFE_u8)
                    .filter(|vk| !(0x10..=0x12).contains(vk) && !down.contains(vk))
                    // SAFETY: plain queries.
                    .filter(|&vk| unsafe { GetAsyncKeyState(i32::from(vk)) } < 0)
                    .collect();
                down.extend(held);
            }
            down.into_iter()
                .map(|vk| KeyState {
                    // SAFETY: plain query.
                    scan_code: unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC_EX) },
                    virtual_key: vk,
                    // shortcut: no characters (code point 0);
                    // the title reads scan codes and virtual keys only.
                    ..Default::default()
                })
                .collect()
        }

        fn rumble(&mut self, slot: usize, low: u16, high: u16) {
            let speeds = XINPUT_VIBRATION {
                wLeftMotorSpeed: low,
                wRightMotorSpeed: high,
            };
            // SAFETY: a valid vibration; an empty slot just fails.
            unsafe { XInputSetState(slot as u32, &speeds) };
        }
    }

    /// Debugging aid: `VICTORY_KEYS=<seconds>:<key>,...` holds each key for
    /// 0.2 s that long after input starts (`<from>-<to>:<key>`: all that
    /// time), with or without focus. A key is a
    /// letter, a digit, or `Enter`, `Esc`, `Tab`, `Back`, `Up`, `Down`,
    /// `Left`, `Right` (the stand-in pad's table maps them to buttons).
    fn scripted() -> Vec<u8> {
        static SCRIPT: LazyLock<(Instant, Vec<(Duration, Duration, u8)>)> = LazyLock::new(|| {
            let list = std::env::var("VICTORY_KEYS").unwrap_or_default();
            let keys = list.split(',').filter_map(|entry| {
                let (at, key) = entry.split_once(':')?;
                let vk = match key.trim() {
                    "Enter" => 0x0D,
                    "Esc" => 0x1B,
                    "Tab" => 0x09,
                    "Back" => 0x08,
                    "Up" => 0x26,
                    "Down" => 0x28,
                    "Left" => 0x25,
                    "Right" => 0x27,
                    k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphanumeric() => {
                        k.as_bytes()[0].to_ascii_uppercase()
                    }
                    _ => return None,
                };
                let time = |t: &str| Duration::try_from_secs_f64(t.trim().parse().ok()?).ok();
                let (from, to) = match at.split_once('-') {
                    Some((from, to)) => (time(from)?, time(to)?),
                    None => (time(at)?, time(at)? + Duration::from_millis(200)),
                };
                Some((from, to, vk))
            });
            (Instant::now(), keys.collect())
        });
        let (start, keys) = &*SCRIPT;
        let now = start.elapsed();
        keys.iter()
            .filter(|&&(from, to, _)| (from..to).contains(&now))
            .map(|&(_, _, vk)| vk)
            .collect()
    }

    /// The foreground window is one of ours (the title's or our fallback).
    fn focused() -> bool {
        let mut process = 0;
        // SAFETY: plain queries; a null window gives process 0.
        unsafe {
            GetWindowThreadProcessId(GetForegroundWindow(), &mut process);
            process == GetCurrentProcessId()
        }
    }

    /// The mouse for the stand-in pad: its reports as raw input (travel in
    /// the mouse's own counts, whatever the pointer does and wherever it
    /// stands), and the pointer taken while a window of the title is in front.
    mod pointer {
        use std::sync::LazyLock;
        use std::sync::atomic::{AtomicI32, AtomicU8, Ordering};

        use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
        use windows_sys::Win32::UI::Input::{
            GetRawInputData, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE,
            RAWINPUTHEADER, RAWMOUSE, RID_INPUT, RIDEV_INPUTSINK, RIM_TYPEMOUSE,
            RegisterRawInputDevices,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging as wm;
        use windows_sys::core::w;

        use super::super::{
            Look, MOUSE_BACK, MOUSE_FORWARD, MOUSE_LEFT, MOUSE_MIDDLE, MOUSE_RIGHT, Mouse, POINTER,
            POINTER_FREE, POINTER_TAKEN, Tune, look_window, now, tune_from,
        };
        use crate::modules::lifecycle;

        /// Counts the mouse went right and down since they were last taken.
        static TRAVEL: [AtomicI32; 2] = [AtomicI32::new(0), AtomicI32::new(0)];
        /// The buttons held, as [`Mouse::buttons`].
        static HELD: AtomicU8 = AtomicU8::new(0);

        /// A button's flags in a report (down, up) and its virtual key.
        const BUTTONS: [(u32, u32, u8); 5] = [
            (wm::RI_MOUSE_LEFT_BUTTON_DOWN, wm::RI_MOUSE_LEFT_BUTTON_UP, MOUSE_LEFT),
            (wm::RI_MOUSE_RIGHT_BUTTON_DOWN, wm::RI_MOUSE_RIGHT_BUTTON_UP, MOUSE_RIGHT),
            (wm::RI_MOUSE_MIDDLE_BUTTON_DOWN, wm::RI_MOUSE_MIDDLE_BUTTON_UP, MOUSE_MIDDLE),
            (wm::RI_MOUSE_BUTTON_4_DOWN, wm::RI_MOUSE_BUTTON_4_UP, MOUSE_BACK),
            (wm::RI_MOUSE_BUTTON_5_DOWN, wm::RI_MOUSE_BUTTON_5_UP, MOUSE_FORWARD),
        ];

        /// One report of a mouse.
        ///
        /// shortcut: a pointer that reports places, not travel (a tablet, a
        /// remote desktop's), turns nothing; its buttons count.
        pub(super) fn note(mouse: &RAWMOUSE) {
            // SAFETY: both views of the union are plain numbers.
            let flags = u32::from(unsafe { mouse.Anonymous.Anonymous.usButtonFlags });
            for (down, up, key) in BUTTONS {
                if flags & down != 0 {
                    HELD.fetch_or(1 << key, Ordering::Relaxed);
                }
                if flags & up != 0 {
                    HELD.fetch_and(!(1 << key), Ordering::Relaxed);
                }
            }
            if mouse.usFlags & MOUSE_MOVE_ABSOLUTE == 0 {
                TRAVEL[0].fetch_add(mouse.lLastX, Ordering::Relaxed);
                TRAVEL[1].fetch_add(mouse.lLastY, Ordering::Relaxed);
            }
        }

        /// The procedure of the window the reports are sent to.
        extern "system" fn reports(
            window: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            if message == wm::WM_INPUT {
                let mut raw = RAWINPUT::default();
                let mut size = size_of::<RAWINPUT>() as u32;
                // SAFETY: room for one mouse's report, and its size said.
                let got = unsafe {
                    GetRawInputData(
                        lparam as HRAWINPUT,
                        RID_INPUT,
                        (&raw mut raw).cast(),
                        &mut size,
                        size_of::<RAWINPUTHEADER>() as u32,
                    )
                };
                if got != u32::MAX && raw.header.dwType == RIM_TYPEMOUSE {
                    // SAFETY: a mouse's report, by its type.
                    note(unsafe { &raw.data.mouse });
                }
            }
            // SAFETY: the default handling, which a raw input message needs too.
            unsafe { wm::DefWindowProcW(window, message, wparam, lparam) }
        }

        /// A window for messages only, with every mouse's reports sent to it
        /// whichever window is in front (what is in front is asked when they
        /// are used, [`Pointer::read`]).
        fn listen() -> bool {
            // SAFETY: a window of a system class made, given our procedure
            // and named as the target, all on the thread that then serves it.
            unsafe {
                let window = wm::CreateWindowExW(
                    0,
                    w!("STATIC"),
                    std::ptr::null(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    wm::HWND_MESSAGE,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                );
                if window.is_null() {
                    return false;
                }
                wm::SetWindowLongPtrW(window, wm::GWLP_WNDPROC, reports as *const () as isize);
                let mice = RAWINPUTDEVICE {
                    // Generic desktop controls, mouse.
                    usUsagePage: 1,
                    usUsage: 2,
                    dwFlags: RIDEV_INPUTSINK,
                    hwndTarget: window,
                };
                RegisterRawInputDevices(&mice, 1, size_of::<RAWINPUTDEVICE>() as u32) != 0
            }
        }

        /// Whether the mouse is read: `VICTORY_MOUSE` is not `0`, and a
        /// thread of ours takes its reports (started at the first asking).
        fn tune() -> Option<&'static Tune> {
            static TUNE: LazyLock<Option<Tune>> = LazyLock::new(|| {
                let tune = tune_from(std::env::var("VICTORY_MOUSE").ok().as_deref())?;
                let (said, hears) = std::sync::mpsc::channel();
                let serve = move || {
                    let listening = listen();
                    let _ = said.send(listening);
                    let mut message = wm::MSG::default();
                    let none = std::ptr::null_mut();
                    // SAFETY: the message loop of this thread's window.
                    while listening && unsafe { wm::GetMessageW(&mut message, none, 0, 0) } > 0 {
                        // SAFETY: a message just taken.
                        unsafe { wm::DispatchMessageW(&message) };
                    }
                };
                let started = std::thread::Builder::new()
                    .name("victory-mouse".into())
                    .spawn(serve);
                if started.is_err() || !hears.recv().unwrap_or(false) {
                    tracing::warn!("the mouse's reports could not be asked for; it is not read");
                    return None;
                }
                POINTER.store(POINTER_FREE, Ordering::Relaxed);
                tracing::info!(?tune, "the mouse is part of the stand-in pad (F1 frees its pointer)");
                Some(tune)
            });
            TUNE.as_ref()
        }

        /// Has the window's own thread look at the pointer again (it is that
        /// thread's to show or not: `lifecycle`'s procedure).
        fn show_again(window: HWND) {
            let over_the_picture = (wm::HTCLIENT | (wm::WM_MOUSEMOVE << 16)) as LPARAM;
            // SAFETY: a message to a window; one that is gone takes none.
            unsafe {
                wm::PostMessageW(window, wm::WM_SETCURSOR, window as WPARAM, over_the_picture);
            }
        }

        #[derive(Default)]
        pub(in super::super) struct Pointer {
            /// The window the pointer is taken for (as a number; 0: none).
            window: usize,
            /// Where it is kept: left, top, right, bottom on the screen.
            kept: [i32; 4],
            look: Look,
        }

        impl Pointer {
            pub(in super::super) fn read(&mut self, take: bool) -> Mouse {
                let Some(tune) = tune() else {
                    return Mouse::default();
                };
                // Travel is taken whether it is used or not: what the mouse
                // did while the pointer was the user's turns nothing later.
                let mut moved = (
                    TRAVEL[0].swap(0, Ordering::Relaxed),
                    TRAVEL[1].swap(0, Ordering::Relaxed),
                );
                let held = HELD.load(Ordering::Relaxed);
                // SAFETY: a plain query.
                let front = unsafe { wm::GetForegroundWindow() };
                if !take || !lifecycle::is_title_window(front) {
                    self.give_back();
                    return Mouse::default();
                }
                if self.window != front as usize {
                    self.give_back();
                    // Not with a button down: that is the click that brought
                    // the window to the front, and it may hold the window by
                    // its bar or its edge. A pointer moved to the middle of
                    // the picture then drags the window after it, and the
                    // middle with the window, at every asking.
                    if held != 0 {
                        return Mouse::default();
                    }
                    self.window = front as usize;
                    self.look = Look::default();
                    moved = (0, 0);
                    POINTER.store(POINTER_TAKEN, Ordering::Relaxed);
                    show_again(front);
                }
                self.keep(front);
                Mouse {
                    buttons: held,
                    look: self.look.stick(now(), moved, look_window(), tune),
                }
            }

            /// Keeps the pointer in the middle of the window's picture: a
            /// click then goes to that window and to no other, and to none
            /// of its edges or buttons. Again at every asking, as the system
            /// lets a pointer go when another window comes to the front.
            fn keep(&mut self, window: HWND) {
                let mut info = wm::WINDOWINFO {
                    cbSize: size_of::<wm::WINDOWINFO>() as u32,
                    ..Default::default()
                };
                // SAFETY: room for the window's places, and its size said.
                if unsafe { wm::GetWindowInfo(window, &mut info) } == 0 {
                    return;
                }
                let picture = info.rcClient;
                if picture.right <= picture.left || picture.bottom <= picture.top {
                    return;
                }
                let (x, y) = ((picture.left + picture.right) / 2, (picture.top + picture.bottom) / 2);
                self.kept = [x, y, x + 1, y + 1];
                let [left, top, right, bottom] = self.kept;
                // SAFETY: a rectangle of ours.
                unsafe { wm::ClipCursor(&RECT { left, top, right, bottom }) };
            }

            /// Gives the pointer back, if it is taken.
            fn give_back(&mut self) {
                if self.window == 0 {
                    return;
                }
                let mut held = RECT::default();
                // SAFETY: room for the rectangle. Only our own keeping is
                // ended: another window that came to the front may have
                // taken the pointer for itself since.
                unsafe {
                    if wm::GetClipCursor(&mut held) != 0
                        && [held.left, held.top, held.right, held.bottom] == self.kept
                    {
                        wm::ClipCursor(std::ptr::null());
                    }
                }
                POINTER.store(POINTER_FREE, Ordering::Relaxed);
                show_again(self.window as HWND);
                self.window = 0;
            }
        }

        #[test]
        fn a_mouse_report_gives_travel_and_buttons() {
            use windows_sys::Win32::UI::Input::{RAWMOUSE_0, RAWMOUSE_0_0};
            let report = |flags: u32, right: i32, down: i32, absolute: bool| {
                let buttons = RAWMOUSE_0_0 {
                    usButtonFlags: flags as u16,
                    usButtonData: 0,
                };
                note(&RAWMOUSE {
                    usFlags: if absolute { MOUSE_MOVE_ABSOLUTE } else { 0 },
                    Anonymous: RAWMOUSE_0 { Anonymous: buttons },
                    lLastX: right,
                    lLastY: down,
                    ..Default::default()
                });
            };
            let travel = || (TRAVEL[0].load(Ordering::Relaxed), TRAVEL[1].load(Ordering::Relaxed));
            report(wm::RI_MOUSE_RIGHT_BUTTON_DOWN, 5, -3, false);
            report(wm::RI_MOUSE_LEFT_BUTTON_DOWN | wm::RI_MOUSE_BUTTON_5_DOWN, 2, 1, false);
            assert_eq!(travel(), (7, -2));
            let held = |key: u8| HELD.load(Ordering::Relaxed) & (1 << key) != 0;
            assert!(held(MOUSE_RIGHT) && held(MOUSE_LEFT) && held(MOUSE_FORWARD));
            assert!(!held(MOUSE_MIDDLE) && !held(MOUSE_BACK));
            // A place on the screen is no travel; a button let go is let go.
            report(wm::RI_MOUSE_RIGHT_BUTTON_UP, 30_000, 20_000, true);
            assert_eq!(travel(), (7, -2));
            assert!(!held(MOUSE_RIGHT) && held(MOUSE_LEFT));
        }

        /// The system takes the asking: a window for messages only, and the
        /// mice's reports sent to it (on a thread of its own, as in a run).
        #[test]
        fn the_mouses_reports_can_be_asked_for() {
            assert!(std::thread::spawn(listen).join().unwrap());
        }

        #[test]
        fn no_pointer_is_shown_over_the_picture_while_it_is_taken() {
            let none = std::ptr::null_mut();
            let nothing = std::ptr::null();
            let title =
                lifecycle::CreateWindowExW(0, w!("STATIC"), w!("picture"), 0, 0, 0, 10, 10, none, none, none, nothing);
            assert!(lifecycle::is_title_window(title));
            assert!(!lifecycle::is_title_window(none));
            // SAFETY: windows of this thread, so each message is handled
            // before `SendMessageW` returns; the class's own pointer is put
            // back before the windows go.
            unsafe {
                let ours =
                    wm::CreateWindowExW(0, w!("STATIC"), w!("ours"), 0, 0, 0, 10, 10, none, none, none, nothing);
                assert!(!lifecycle::is_title_window(ours));
                let over_the_picture = wm::HTCLIENT as LPARAM;
                let asked = |window: HWND| wm::SendMessageW(window, wm::WM_SETCURSOR, window as WPARAM, over_the_picture);
                wm::SetCursor(wm::LoadCursorW(none, wm::IDC_ARROW));
                // While the mouse is not read, the title's procedure alone
                // says what is shown.
                asked(title);
                assert!(!wm::GetCursor().is_null());
                POINTER.store(POINTER_TAKEN, Ordering::Relaxed);
                assert_eq!(asked(title), 1);
                assert!(wm::GetCursor().is_null());
                // Given back, there is a pointer again: also over a window
                // whose class names none, as a console title's may.
                let class_pointer = wm::SetClassLongPtrW(title, wm::GCLP_HCURSOR, 0);
                POINTER.store(POINTER_FREE, Ordering::Relaxed);
                asked(title);
                let shown = !wm::GetCursor().is_null();
                POINTER.store(0, Ordering::Relaxed);
                wm::SetClassLongPtrW(title, wm::GCLP_HCURSOR, class_pointer as isize);
                assert!(shown);
                wm::DestroyWindow(ours);
                wm::DestroyWindow(title);
            }
        }
    }

    #[test]
    fn host_pads_convert_to_console_pads() {
        let pad = XINPUT_GAMEPAD {
            wButtons: XINPUT_GAMEPAD_A | XINPUT_GAMEPAD_START | XINPUT_GAMEPAD_DPAD_LEFT,
            bLeftTrigger: 255,
            sThumbLX: -32768,
            sThumbLY: 32767,
            ..Default::default()
        };
        let state = convert(&pad);
        assert_eq!(state.buttons, A | MENU | LEFT);
        assert_eq!(
            (
                state.left_trigger,
                state.left_x,
                state.left_y,
                state.right_x
            ),
            (1.0, -1.0, 1.0, 0.0)
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::HleResolver;
    use crate::HostPolicy;
    use victory_loader::Provenance;
    use victory_pe::ImportSymbol;

    #[derive(Default)]
    struct FakeHost {
        pads: [Option<PadState>; PADS],
        keys: Vec<KeyState>,
        /// What the mouse gives while its pointer is taken.
        mouse: Mouse,
        rumble: Vec<(usize, u16, u16)>,
    }

    struct Fake(Arc<Mutex<FakeHost>>);

    impl Source for Fake {
        fn pad(&mut self, slot: usize) -> Option<PadState> {
            self.0.lock().unwrap().pads[slot]
        }
        fn keys(&mut self) -> Vec<KeyState> {
            self.0.lock().unwrap().keys.clone()
        }
        fn mouse(&mut self, take: bool) -> Mouse {
            if take {
                self.0.lock().unwrap().mouse
            } else {
                Mouse::default()
            }
        }
        fn rumble(&mut self, slot: usize, low: u16, high: u16) {
            self.0.lock().unwrap().rumble.push((slot, low, high));
        }
    }

    fn fake() -> (&'static Hub, Arc<Mutex<FakeHost>>) {
        fake_with(false)
    }

    fn fake_with(one_pad: bool) -> (&'static Hub, Arc<Mutex<FakeHost>>) {
        let host = Arc::new(Mutex::new(FakeHost::default()));
        let hub = Hub::new(Box::new(Fake(Arc::clone(&host))), one_pad);
        hub.poll();
        (hub, host)
    }

    fn key(virtual_key: u8) -> KeyState {
        KeyState {
            scan_code: u32::from(virtual_key) + 0x100,
            virtual_key,
            ..Default::default()
        }
    }

    fn slot<F: Copy, T>(object: *const T, offset: usize) -> F {
        // SAFETY: our objects start with their table, which has the slot.
        let f = unsafe { *(*(object as *const *const *const ())).add(offset / 8) };
        // SAFETY: the caller names the slot's signature.
        unsafe { std::mem::transmute_copy(&f) }
    }

    /// Device callbacks seen: device index, current and previous status.
    type Log = Mutex<Vec<(usize, u32, u32)>>;

    extern "system" fn record(
        _token: u64,
        context: *mut c_void,
        device: *const Device,
        _timestamp: u64,
        current: u32,
        previous: u32,
    ) {
        // SAFETY: the test passes its log and one of the hub's devices.
        let (log, index) = unsafe { (&*(context as *const Log), (*device).index) };
        log.lock().unwrap().push((index, current, previous));
    }

    fn register(hub: &Hub, kind: u32, enumeration: u32, log: &Log) -> u64 {
        type Register = extern "system" fn(
            This,
            *const Device,
            u32,
            u32,
            u32,
            *mut c_void,
            Option<DeviceCallback>,
            *mut u64,
        ) -> HRESULT;
        let mut token = 0;
        let context = log as *const Log as *mut c_void;
        let register = slot::<Register, _>(&hub.object, 0x48);
        let hr = register(
            &hub.object,
            std::ptr::null(),
            kind,
            1,
            enumeration,
            context,
            Some(record),
            &mut token,
        );
        assert_eq!(hr, S_OK);
        assert_ne!(token, 0);
        token
    }

    fn result(hr: HRESULT, out: *const c_void) -> Result<*const Reading, HRESULT> {
        if hr == S_OK {
            Ok(out.cast())
        } else {
            assert!(out.is_null());
            Err(hr)
        }
    }

    fn current(hub: &Hub, kind: u32, device: usize) -> Result<*const Reading, HRESULT> {
        type Current = extern "system" fn(This, u32, *const Device, Out) -> HRESULT;
        let mut out = std::ptr::null();
        let hr = slot::<Current, _>(&hub.object, 0x20)(
            &hub.object,
            kind,
            &hub.devices[device],
            &mut out,
        );
        result(hr, out)
    }

    fn walk(hub: &Hub, offset: usize, from: *const Reading) -> Result<*const Reading, HRESULT> {
        type Walk = extern "system" fn(This, *const Reading, u32, *const Device, Out) -> HRESULT;
        let mut out = std::ptr::null();
        let device = reading(from).device;
        let hr =
            slot::<Walk, _>(&hub.object, offset)(&hub.object, from, GAMEPAD_KIND, device, &mut out);
        result(hr, out)
    }

    fn pad(reading: *const Reading) -> PadState {
        let mut state = PadState::default();
        let get =
            slot::<extern "system" fn(*const Reading, *mut PadState) -> bool, _>(reading, 0xb0);
        assert!(get(reading, &mut state));
        state
    }

    fn stamp(reading: *const Reading) -> (u64, u64) {
        let time = slot::<extern "system" fn(*const Reading) -> u64, _>(reading, 0x28);
        let sequence = slot::<extern "system" fn(*const Reading, u32) -> u64, _>(reading, 0x20);
        (time(reading), sequence(reading, GAMEPAD_KIND))
    }

    fn release(reading: *const Reading) {
        slot::<extern "system" fn(*const Reading) -> u32, _>(reading, 0x10)(reading);
    }

    #[test]
    fn ordinal_one_resolves_to_create() {
        let r = HleResolver::new(HostPolicy::none());
        assert_eq!(
            r.classify("GameInput.dll", &ImportSymbol::Ordinal(1)),
            Some(Provenance::Hle)
        );
        assert_eq!(r.classify("GameInput.dll", &ImportSymbol::Ordinal(2)), None);
    }

    #[test]
    fn pads_connect_report_and_disconnect() {
        let (hub, host) = fake();
        let log = Log::default();
        register(hub, GAMEPAD_KIND, 2, &log);
        // Blocking enumeration ran before registration returned: with no
        // host pad the keyboard's stand-in is the only one.
        assert_eq!(*log.lock().unwrap(), [(FALLBACK, STATUS_CONNECTED, 0)]);

        host.lock().unwrap().pads[1] = Some(PadState {
            buttons: A,
            left_x: 0.5,
            ..Default::default()
        });
        hub.poll();
        assert_eq!(
            log.lock().unwrap()[1..],
            [(1, STATUS_CONNECTED, 0), (FALLBACK, 0, STATUS_CONNECTED)]
        );

        let first = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(pad(first).buttons, A);
        assert_eq!(pad(first).left_x, 0.5);
        // No change, no new reading.
        hub.poll();
        let again = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(again, first);
        release(again);

        std::thread::sleep(Duration::from_millis(2));
        host.lock().unwrap().pads[1].as_mut().unwrap().buttons = B;
        hub.poll();
        let second = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(pad(second).buttons, B);
        let (t1, s1) = stamp(first);
        let (t2, s2) = stamp(second);
        assert!(t2 > t1 && s2 == s1 + 1);
        assert!(t2 <= now());

        // The title's edge check: previous of current, and back again.
        assert_eq!(walk(hub, 0x30, second), Ok(first));
        assert_eq!(walk(hub, 0x28, first), Ok(second));
        assert_eq!(walk(hub, 0x28, second), Err(READING_NOT_FOUND));
        assert_eq!(walk(hub, 0x30, first), Err(READING_NOT_FOUND));
        for r in [first, first, second, second] {
            release(r);
        }

        host.lock().unwrap().pads[1] = None;
        hub.poll();
        assert_eq!(
            log.lock().unwrap()[3..],
            [(1, 0, STATUS_CONNECTED), (FALLBACK, STATUS_CONNECTED, 0)]
        );
        assert_eq!(current(hub, GAMEPAD_KIND, 1), Err(DEVICE_DISCONNECTED));
        // The stand-in is back, at rest.
        let rest = current(hub, GAMEPAD_KIND, FALLBACK).unwrap();
        assert_eq!(pad(rest), PadState::default());
        release(rest);
    }

    #[test]
    fn keys_feed_the_keyboard_and_the_stand_in_pad() {
        let (hub, host) = fake();
        let log = Log::default();
        register(hub, KEYBOARD_KIND, 2, &log);
        assert_eq!(*log.lock().unwrap(), [(KEYBOARD, STATUS_CONNECTED, 0)]);

        let keys = vec![key(b'W'), key(0x0D), key(b'L'), key(b'C')];
        host.lock().unwrap().keys = keys.clone();
        hub.poll();
        let r = current(hub, KEYBOARD_KIND, KEYBOARD).unwrap();
        let count = slot::<extern "system" fn(*const Reading) -> u32, _>(r, 0x70);
        let state =
            slot::<extern "system" fn(*const Reading, u32, *mut KeyState) -> u32, _>(r, 0x78);
        assert_eq!(count(r), 4);
        let mut got = [KeyState::default(); 8];
        assert_eq!(state(r, 8, got.as_mut_ptr()), 4);
        assert_eq!(got[..4], keys[..]);
        release(r);

        let r = current(hub, GAMEPAD_KIND, FALLBACK).unwrap();
        assert_eq!(
            pad(r),
            PadState {
                buttons: A | LEFT_STICK,
                left_y: 1.0,
                right_x: 1.0,
                ..Default::default()
            }
        );
        release(r);
        // A keyboard reading is no gamepad reading.
        assert_eq!(current(hub, GAMEPAD_KIND, KEYBOARD), Err(READING_NOT_FOUND));
    }

    /// The stand-in pad as the title reads it now.
    fn stand_in_pad(hub: &Hub) -> PadState {
        let r = current(hub, GAMEPAD_KIND, FALLBACK).unwrap();
        let state = pad(r);
        release(r);
        state
    }

    #[test]
    fn the_mouse_aims_fires_and_turns_the_view() {
        let (hub, host) = fake();
        // The right button aims (the left trigger); of the mouse and the
        // keys, each axis of the right stick is the one pushed further.
        host.lock().unwrap().mouse = Mouse {
            buttons: 1 << MOUSE_RIGHT,
            look: (0.5, -0.25),
        };
        host.lock().unwrap().keys = vec![key(b'W'), key(b'L'), key(0x20)];
        hub.poll();
        assert_eq!(
            stand_in_pad(hub),
            PadState {
                buttons: X,
                left_trigger: 1.0,
                left_y: 1.0,
                right_x: 1.0,
                right_y: -0.25,
                ..Default::default()
            }
        );
        // The left button fires (the right trigger); the middle one is the
        // right stick's click.
        host.lock().unwrap().mouse.buttons = (1 << MOUSE_LEFT) | (1 << MOUSE_MIDDLE);
        host.lock().unwrap().keys.clear();
        hub.poll();
        let with_the_mouse = PadState {
            buttons: RIGHT_STICK,
            right_trigger: 1.0,
            right_x: 0.5,
            right_y: -0.25,
            ..Default::default()
        };
        assert_eq!(stand_in_pad(hub), with_the_mouse);

        // F1 gives the pointer back, once for each time it goes down.
        host.lock().unwrap().keys = vec![key(F1)];
        hub.poll();
        assert_eq!(stand_in_pad(hub), PadState::default());
        hub.poll();
        assert_eq!(stand_in_pad(hub), PadState::default());
        host.lock().unwrap().keys.clear();
        hub.poll();
        assert_eq!(stand_in_pad(hub), PadState::default());
        // And takes it again.
        host.lock().unwrap().keys = vec![key(F1)];
        hub.poll();
        assert_eq!(stand_in_pad(hub), with_the_mouse);
    }

    #[test]
    fn f2_makes_the_left_sticks_keys_the_triggers() {
        let (hub, host) = fake();
        let held = |keys: &[u8]| {
            host.lock().unwrap().keys = keys.iter().map(|&k| key(k)).collect();
            hub.poll();
            let p = stand_in_pad(hub);
            (p.left_x, p.left_y, p.left_trigger, p.right_trigger)
        };
        assert_eq!(held(b"WD"), (1.0, 1.0, 0.0, 0.0));
        // Driving: forward is the right trigger, back the left one; left and
        // right stay the stick's.
        assert_eq!(held(&[b'W', b'D', F2]), (1.0, 0.0, 0.0, 1.0));
        assert_eq!(held(b"SA"), (-1.0, 0.0, 1.0, 0.0));
        // On foot again.
        assert_eq!(held(&[b'S', F2]), (0.0, -1.0, 0.0, 0.0));
    }

    #[test]
    fn the_get_in_key_switches_to_the_driving_keys() {
        let key = |k: u8| KeyState { virtual_key: k, ..Default::default() };
        let mut switches = Switches::default();
        let press = |switches: &mut Switches, keys: &[u8]| {
            let keys: Vec<KeyState> = keys.iter().map(|&k| key(k)).collect();
            switches.note_with(&keys, Some(b'F'));
        };
        press(&mut switches, b"F");
        assert!(switches.driving, "F gets in");
        // Held, it switches nothing more.
        press(&mut switches, b"FW");
        assert!(switches.driving);
        press(&mut switches, b"");
        press(&mut switches, b"F");
        assert!(!switches.driving, "and out again");
        // F2 puts it right without F, and so does P.
        press(&mut switches, &[F2]);
        assert!(switches.driving);
        press(&mut switches, b"");
        press(&mut switches, b"P");
        assert!(!switches.driving);
        // Without a get-in key (`VICTORY_AUTO_DRIVE=0`) F does nothing.
        let mut alone = Switches::default();
        alone.note_with(&[key(b'F')], None);
        assert!(!alone.driving);
    }

    #[test]
    fn trigger_rumble_switches_to_the_driving_keys_while_it_lasts() {
        let mut switches = Switches::default();
        switches.rumbled(Instant::now());
        assert!(switches.driving, "rumbled triggers: a vehicle");
        switches.note_with(&[], None);
        assert!(switches.driving, "still within the time it holds");
        switches.rumble_until = Some(Instant::now() - Duration::from_millis(1));
        switches.note(&[]);
        assert!(!switches.driving, "on foot once the rumble has stopped");
    }

    #[test]
    fn in_a_vehicle_the_keys_are_grand_theft_auto_vs() {
        let keys = |held: &[u8]| -> Vec<KeyState> {
            held.iter().map(|&k| KeyState { virtual_key: k, ..Default::default() }).collect()
        };
        let on_foot = KEYS.to_vec();
        let driving = driving_table(&on_foot, "");
        let pad = |table: &[(u8, To)], held: &[u8], driving: bool| {
            stand_in(table, &keys(held), Mouse::default(), driving)
        };
        // On foot: walk, sprint, jump, crouch.
        let p = pad(&on_foot, &[b'W', 0xA0, 0x20, b'C'], false);
        assert_eq!((p.left_y, p.buttons), (1.0, A | X | LEFT_STICK));
        // Driving: throttle, steer, handbrake, horn, look behind.
        let p = pad(&driving, &[b'W', b'A', 0x20, b'E', b'C'], true);
        assert_eq!((p.right_trigger, p.left_x, p.left_y), (1.0, -1.0, 0.0));
        assert_eq!(p.buttons, RIGHT_SHOULDER | LEFT_STICK | RIGHT_STICK);
        let p = pad(&driving, b"S", true);
        assert_eq!((p.left_trigger, p.left_y), (1.0, 0.0));
        // A key bound anew keeps its own binding in a vehicle too.
        let driving = driving_table(&on_foot, "E=Y");
        assert_eq!(driving.iter().filter(|(k, _)| *k == b'E').count(), 1);
        assert!(driving.contains(&(b'E', To::Button(RIGHT))), "the on-foot table is the caller's here");
    }

    #[test]
    fn pad_keys_are_bound_anew_by_name() {
        let asked = "F=LS, g = y ,Mouse4=none,Space=RT,J=LUp,oops,Q=wheel,=A";
        let (table, refused) = rebound(&KEYS, asked);
        assert_eq!(refused, ["oops", "Q=wheel", "=A"]);
        let does = |key: u8| -> Vec<To> {
            table.iter().filter(|(k, _)| *k == key).map(|&(_, to)| to).collect()
        };
        assert_eq!(does(b'F'), [To::Button(LEFT_STICK)]);
        assert_eq!(does(b'G'), [To::Button(Y)]);
        assert!(does(MOUSE_BACK).is_empty());
        assert_eq!(does(0x20), [To::RightTrigger]);
        assert_eq!(does(b'J'), [To::Stick(LEFT_Y, 1.0)]);
        // A key not named, or named with something not understood, does
        // what it did.
        assert_eq!(does(b'Q'), [To::Button(RIGHT_SHOULDER)]);
        assert_eq!(does(MOUSE_RIGHT), [To::LeftTrigger]);
        // The table itself binds each key once, and nothing to F1 and F2.
        let mut keys: Vec<u8> = KEYS.iter().map(|k| k.0).collect();
        keys.sort_unstable();
        assert!(keys.windows(2).all(|w| w[0] != w[1]), "{keys:?}");
        assert!(!keys.contains(&F1) && !keys.contains(&F2) && !keys.contains(&P));
    }

    #[test]
    fn the_pointers_travel_is_the_right_sticks_push() {
        let tune = tune_from(None).unwrap();
        let unset = Tune {
            full: FULL_SPEED,
            dead: DEAD_ZONE,
            curve: 1.0,
        };
        assert_eq!(tune, unset);
        // 50 ms: a frame of the title's input at 20 frames a second.
        let window = 50_000;
        let mut look = Look::default();
        assert_eq!(look.stick(1_000_000, (0, 0), window, &tune), (0.0, 0.0));
        // 20 counts in it are 400 a second, a tenth of the full speed: that
        // much of the stick's way past the dead zone.
        let tenth = DEAD_ZONE + (1.0 - DEAD_ZONE) * 0.1;
        let (x, y) = look.stick(1_010_000, (20, 0), window, &tune);
        assert!((x - tenth).abs() < 1e-5 && y == 0.0, "{x} {y}");
        // It is the stick's for the window's time, and then no more.
        assert_eq!(look.stick(1_050_000, (0, 0), window, &tune), (x, y));
        assert_eq!(look.stick(1_060_000, (0, 0), window, &tune), (0.0, 0.0));
        // Travel of several askings in one window adds up.
        look.stick(2_000_000, (12, 0), window, &tune);
        let (x, _) = look.stick(2_008_000, (8, 0), window, &tune);
        assert!((x - tenth).abs() < 1e-5, "{x}");
        // Up on the desk (counts go down the screen) is up on the stick, and
        // the fastest travel is the whole stick, no more.
        let (x, y) = look.stick(3_000_000, (3000, -4000), window, &tune);
        assert!((x - 0.6).abs() < 1e-5 && (y - 0.8).abs() < 1e-5, "{x} {y}");
        assert!((33_333..=250_000).contains(&look_window()));
    }

    #[test]
    fn victory_mouse_is_off_or_numbers() {
        assert_eq!(tune_from(Some("0")), None);
        assert_eq!(tune_from(Some("off")), None);
        let unset = tune_from(None);
        assert_eq!(tune_from(Some("1")), unset);
        assert_eq!(tune_from(Some("")), unset);
        let asked = Tune {
            full: FULL_SPEED / 2.0,
            dead: 0.3,
            curve: 0.5,
        };
        assert_eq!(tune_from(Some("2, 0.3,0.5")), Some(asked));
        // What is no number, or out of bounds, is as unset.
        assert_eq!(tune_from(Some("fast,2")), unset);
    }

    #[test]
    fn a_host_pad_feeds_the_one_pad_which_never_goes() {
        let (hub, host) = fake_with(true);
        let log = Log::default();
        register(hub, GAMEPAD_KIND, 2, &log);
        assert_eq!(*log.lock().unwrap(), [(FALLBACK, STATUS_CONNECTED, 0)]);
        let pad = |hub: &Hub| match &hub.state().slots[FALLBACK].history.back().unwrap().value {
            Value::Pad(p) => *p,
            Value::Keys(_) => unreachable!(),
        };
        host.lock().unwrap().keys = vec![key(b'D')];
        hub.poll();
        assert_eq!(pad(hub).left_x, 1.0);
        // A pad is switched on, in any slot: its input arrives, beside the
        // keyboard's, and no device comes or goes.
        host.lock().unwrap().pads[2] = Some(PadState { buttons: A, left_y: -0.5, ..Default::default() });
        hub.poll();
        let p = pad(hub);
        assert_eq!((p.buttons, p.left_x, p.left_y), (A, 1.0, -0.5));
        let device: *const Device = &hub.devices[FALLBACK];
        set_rumble(device, &[1.0, 0.0, 0.0, 0.0]);
        assert_eq!(host.lock().unwrap().rumble, [(2, u16::MAX, 0)]);
        // And off again.
        host.lock().unwrap().pads[2] = None;
        host.lock().unwrap().keys.clear();
        hub.poll();
        assert_eq!(pad(hub), PadState::default());
        assert_eq!(log.lock().unwrap().len(), 1);
        assert!(!hub.state().slots[2].connected);
    }

    #[test]
    fn async_enumeration_waits_for_the_poll_and_unregistering_stops_callbacks() {
        let (hub, host) = fake();
        let log = Log::default();
        let token = register(hub, GAMEPAD_KIND, ASYNC_ENUMERATION, &log);
        assert!(log.lock().unwrap().is_empty());
        hub.poll();
        assert_eq!(*log.lock().unwrap(), [(FALLBACK, STATUS_CONNECTED, 0)]);

        let unregister = slot::<extern "system" fn(This, u64, u64) -> bool, _>(&hub.object, 0x68);
        assert!(unregister(&hub.object, token, 5000));
        host.lock().unwrap().pads[0] = Some(PadState::default());
        hub.poll();
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    #[test]
    fn unregistering_waits_for_a_running_callback() {
        use std::sync::atomic::AtomicBool;
        static INSIDE: AtomicBool = AtomicBool::new(false);
        static LEAVE: AtomicBool = AtomicBool::new(false);
        static FROM_INSIDE: AtomicU32 = AtomicU32::new(0);
        extern "system" fn slow(
            token: u64,
            context: *mut c_void,
            _: *const Device,
            _: u64,
            _: u32,
            _: u32,
        ) {
            // From inside its own callback, unregistering fails at once.
            let unregister = slot::<extern "system" fn(This, u64, u64) -> bool, _>(context, 0x68);
            if !unregister(context.cast(), token, 0) {
                FROM_INSIDE.fetch_add(1, Ordering::SeqCst);
            }
            INSIDE.store(true, Ordering::SeqCst);
            while !LEAVE.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let (hub, host) = fake();
        type Register = extern "system" fn(
            This,
            *const Device,
            u32,
            u32,
            u32,
            *mut c_void,
            Option<DeviceCallback>,
            *mut u64,
        ) -> HRESULT;
        let register = slot::<Register, _>(&hub.object, 0x48);
        let mut token = 0;
        let context = (&raw const hub.object).cast_mut().cast();
        assert_eq!(
            register(&hub.object, std::ptr::null(), GAMEPAD_KIND, 1, 0, context, Some(slow), &mut token),
            S_OK
        );
        host.lock().unwrap().pads[1] = Some(PadState::default());
        let poller = std::thread::spawn(|| hub.poll());
        while !INSIDE.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        let unregister = slot::<extern "system" fn(This, u64, u64) -> bool, _>(&hub.object, 0x68);
        assert!(!unregister(&hub.object, token, 5000), "times out while it runs");
        let leave = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            LEAVE.store(true, Ordering::SeqCst);
        });
        let start = Instant::now();
        assert!(unregister(&hub.object, token, u64::MAX), "waits for it");
        assert!(start.elapsed() >= Duration::from_millis(40));
        assert_eq!(FROM_INSIDE.load(Ordering::SeqCst), 1);
        poller.join().unwrap();
        leave.join().unwrap();
    }

    #[test]
    fn devices_are_found_by_id_and_rumble_reaches_the_host() {
        let (hub, host) = fake();
        host.lock().unwrap().pads[2] = Some(PadState::default());
        hub.poll();
        let device = &hub.devices[2] as *const Device;
        let info = slot::<extern "system" fn(*const Device) -> *const u8, _>(device, 0x18);
        // SAFETY: the info has the id at +0x20.
        let id = unsafe { *(info(device).add(0x20) as *const [u8; 32]) };
        assert_ne!(id, [0; 32]);

        type Find = extern "system" fn(This, *const [u8; 32], Out) -> HRESULT;
        let find = slot::<Find, _>(&hub.object, 0x80);
        let mut out = std::ptr::null();
        assert_eq!(find(&hub.object, &id, &mut out), S_OK);
        assert_eq!(out, device.cast());
        assert_eq!(find(&hub.object, &[1; 32], &mut out), DEVICE_NOT_FOUND);
        assert!(out.is_null());

        type Rumble = extern "system" fn(*const Device, *const [f32; 4]);
        let rumble = slot::<Rumble, _>(device, 0x50);
        rumble(device, &[0.5, 1.0, 1.0, 1.0]);
        rumble(&hub.devices[FALLBACK], &[1.0; 4]);
        rumble(device, std::ptr::null());
        assert_eq!(host.lock().unwrap().rumble, [(2, 32767, 65535), (2, 0, 0)]);
    }
}
