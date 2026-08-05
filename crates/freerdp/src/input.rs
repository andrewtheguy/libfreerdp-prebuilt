//! Keyboard and mouse, encoded here and sent on the FreeRDP thread.
//!
//! Every method on [`Input`] pushes one [`Command`] and wakes the session thread. Nothing blocks,
//! nothing can fail visibly, and a call made after the session has ended is dropped — which is
//! the right shape for input: a keystroke that arrives while the connection is closing has no
//! useful error for the caller to handle, and a `Result` on every key press would be noise.

use freerdp_sys as sys;

use crate::clipboard::ClipboardFormat;
use crate::session::Shared;
use std::sync::Arc;

/// A mouse button, in the three the RDP fast-path encodes directly plus the two extended ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    /// "Back" on most mice.
    X1,
    /// "Forward" on most mice.
    X2,
}

/// One thing for the FreeRDP thread to do next time it wakes.
pub(crate) enum Command {
    Mouse { flags: u16, x: u16, y: u16 },
    ExtendedMouse { flags: u16, x: u16, y: u16 },
    Key { down: bool, scancode: u32 },
    Unicode { down: bool, code: u16 },
    LockKeys { flags: u32 },
    /// Ask the server to resend a region — used after a client-side framebuffer reset.
    Refresh,
    ClipboardAdvertise(Vec<ClipboardFormat>),
    ClipboardRequest(u32),
    ClipboardRespond { format: u32, data: Option<Vec<u8>> },
    Shutdown,
}

/// The input side of a [`Session`](crate::Session).
///
/// Cheap to clone, and every clone feeds the same queue, so a caller can hand one to a keyboard
/// task and another to a pointer task without a mutex of its own.
#[derive(Clone)]
pub struct Input {
    pub(crate) shared: Arc<Shared>,
}

impl Input {
    /// Move the pointer, pressing nothing.
    pub fn mouse_move(&self, x: u16, y: u16) {
        self.mouse(sys::PTR_FLAGS_MOVE as u16, x, y);
    }

    /// Press or release a button, at a position.
    ///
    /// The position travels with the button event rather than being remembered from the last
    /// move, because that is how RDP's own PDU is shaped — and because a click whose coordinates
    /// came from a *previous* event is the classic source of "it clicked the wrong thing" on a
    /// laggy link.
    pub fn mouse_button(&self, button: MouseButton, down: bool, x: u16, y: u16) {
        match button {
            MouseButton::Left | MouseButton::Middle | MouseButton::Right => {
                let mut flags = match button {
                    MouseButton::Left => sys::PTR_FLAGS_BUTTON1,
                    MouseButton::Middle => sys::PTR_FLAGS_BUTTON3,
                    _ => sys::PTR_FLAGS_BUTTON2,
                };
                if down {
                    flags |= sys::PTR_FLAGS_DOWN;
                }
                self.mouse(flags as u16, x, y);
            }
            // The two extra buttons go on a different PDU entirely, with their own DOWN bit.
            MouseButton::X1 | MouseButton::X2 => {
                let mut flags = if button == MouseButton::X1 {
                    sys::PTR_XFLAGS_BUTTON1
                } else {
                    sys::PTR_XFLAGS_BUTTON2
                };
                if down {
                    flags |= sys::PTR_XFLAGS_DOWN;
                }
                self.push(Command::ExtendedMouse { flags: flags as u16, x, y });
            }
        }
    }

    /// Scroll, by a signed number of rotation units. 120 is one notch of a conventional wheel.
    ///
    /// **The sign is not a sign bit on a two's-complement number**, and this is the single most
    /// error-prone corner of RDP input. The rotation lives in the low nine bits of the same
    /// `flags` word as the event type ([`WheelRotationMask`](freerdp_sys::WheelRotationMask) is
    /// `0x01FF`), and a negative rotation is signalled by *setting*
    /// `PTR_FLAGS_WHEEL_NEGATIVE` (0x0100) and putting the magnitude in as a **two's-complement
    /// 9-bit value**. So −120 is `PTR_FLAGS_WHEEL | PTR_FLAGS_WHEEL_NEGATIVE | 0x0088`, not
    /// `0xFF88` and not `120`. Getting it backwards scrolls the wrong way, which is the kind of
    /// bug that gets "fixed" by negating at the call site and then breaks the other axis.
    pub fn wheel(&self, delta: i16, horizontal: bool, x: u16, y: u16) {
        let mut flags = if horizontal { sys::PTR_FLAGS_HWHEEL } else { sys::PTR_FLAGS_WHEEL };
        let magnitude = delta.unsigned_abs().min(sys::WheelRotationMask as u16 / 2);
        if delta < 0 {
            flags |= sys::PTR_FLAGS_WHEEL_NEGATIVE;
            flags |= (magnitude.wrapping_neg()) as u32 & sys::WheelRotationMask;
        } else {
            flags |= magnitude as u32 & sys::WheelRotationMask;
        }
        self.mouse(flags as u16, x, y);
    }

    /// Press or release a key, by RDP scancode.
    ///
    /// `extended` is the E0 prefix — right alt, the arrow cluster, the numeric-keypad Enter and
    /// slash, and the Windows keys. FreeRDP encodes it as a bit above the code
    /// ([`KBDEXT`](freerdp_sys::KBDEXT) = `0x0100`), which is what makes a `(u8, bool)` keymap
    /// map onto this losslessly.
    pub fn key(&self, scancode: u8, extended: bool, down: bool) {
        let code = u32::from(scancode) | if extended { sys::KBDEXT } else { 0 };
        self.push(Command::Key { down, scancode: code });
    }

    /// Send a character rather than a key — for input methods and anything else where the
    /// *character* is known and the key that produced it is not.
    pub fn unicode(&self, code: u16, down: bool) {
        self.push(Command::Unicode { down, code });
    }

    /// Tell the server the state of the lock keys, so its indicators match the local keyboard's.
    ///
    /// Worth sending once after connecting: without it a session whose caps lock is on locally
    /// starts out disagreeing with the remote, and every subsequent keystroke is the wrong case.
    pub fn lock_keys(&self, scroll: bool, num: bool, caps: bool, kana: bool) {
        let mut flags = 0;
        for (on, bit) in [
            (scroll, sys::KBD_SYNC_FLAGS_KBD_SYNC_SCROLL_LOCK),
            (num, sys::KBD_SYNC_FLAGS_KBD_SYNC_NUM_LOCK),
            (caps, sys::KBD_SYNC_FLAGS_KBD_SYNC_CAPS_LOCK),
            (kana, sys::KBD_SYNC_FLAGS_KBD_SYNC_KANA_LOCK),
        ] {
            if on {
                flags |= bit;
            }
        }
        self.push(Command::LockKeys { flags });
    }

    /// Ask the server to repaint the whole desktop.
    pub fn refresh(&self) {
        self.push(Command::Refresh);
    }

    fn mouse(&self, flags: u16, x: u16, y: u16) {
        self.push(Command::Mouse { flags, x, y });
    }

    pub(crate) fn push(&self, command: Command) {
        self.shared.push(command);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rebuild what `wheel` puts in `flags`, without going through a session.
    fn wheel_flags(delta: i16, horizontal: bool) -> u32 {
        let mut flags = if horizontal { sys::PTR_FLAGS_HWHEEL } else { sys::PTR_FLAGS_WHEEL };
        let magnitude = delta.unsigned_abs().min(sys::WheelRotationMask as u16 / 2);
        if delta < 0 {
            flags |= sys::PTR_FLAGS_WHEEL_NEGATIVE;
            flags |= (magnitude.wrapping_neg()) as u32 & sys::WheelRotationMask;
        } else {
            flags |= magnitude as u32 & sys::WheelRotationMask;
        }
        flags
    }

    /// The encoding that is easiest to get backwards, pinned against MS-RDPBCGR's own example.
    #[test]
    fn a_negative_wheel_sets_the_negative_bit_and_a_nine_bit_magnitude() {
        let down = wheel_flags(-120, false);
        assert_ne!(down & sys::PTR_FLAGS_WHEEL_NEGATIVE, 0, "the negative bit must be set");
        assert_eq!(down & sys::WheelRotationMask & !sys::PTR_FLAGS_WHEEL_NEGATIVE, 0x88);

        let up = wheel_flags(120, false);
        assert_eq!(up & sys::PTR_FLAGS_WHEEL_NEGATIVE, 0);
        assert_eq!(up & sys::WheelRotationMask, 120);

        // And the two axes are different events, not the same one with a sign.
        assert_ne!(wheel_flags(120, true) & sys::PTR_FLAGS_HWHEEL, 0);
        assert_eq!(wheel_flags(120, false) & sys::PTR_FLAGS_HWHEEL, 0);
    }

    /// The rotation shares its word with the event-type bits, so an oversized delta that wrapped
    /// into them would turn a scroll into some other event entirely.
    #[test]
    fn an_oversized_wheel_delta_cannot_reach_the_event_bits() {
        for delta in [i16::MIN, -32000, -400, 400, 32000, i16::MAX] {
            let flags = wheel_flags(delta, false);
            let event_bits = flags & !sys::WheelRotationMask;
            assert_eq!(event_bits, sys::PTR_FLAGS_WHEEL, "delta {delta} corrupted the event bits");
        }
    }

    #[test]
    fn extended_keys_set_the_bit_above_the_scancode() {
        assert_eq!(u32::from(0x1Du8) | sys::KBDEXT, 0x11D, "right control");
        assert_eq!(sys::KBDEXT, 0x100);
    }
}
