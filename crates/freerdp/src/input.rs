//! Keyboard, mouse and touch, encoded here and sent on the FreeRDP thread.
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

/// What a touch contact did, in the four transitions MS-RDPEI's state machine has
/// (3.1.1.1): down, an update while down, up, and an abort that is not a tap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchPhase {
    Down,
    Move,
    Up,
    /// The contact was lost rather than lifted — the finger left the digitizer's surface area,
    /// the window lost it, the client went away. A host treats it as "forget this gesture",
    /// where an [`Up`](Self::Up) in the same place would be a tap.
    Cancel,
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
    /// Ask the server for a new desktop size, over `disp`.
    Resize { width: u32, height: u32, scale_percent: u32 },
    /// One touch contact transition, over `rdpei`.
    Touch { phase: TouchPhase, id: i32, x: i32, y: i32 },
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

    /// Ask the server to change the desktop size.
    ///
    /// Needs [`Connect::resize`](crate::Connect::resize) and a server that offers DisplayControl —
    /// which is announced as [`Event::ResizeReady`](crate::Event::ResizeReady). A request made
    /// before that arrives is *held*, not dropped, and the most recent one is sent as soon as the
    /// channel comes up; a request made on a session that never gets the channel is dropped, like
    /// every other input on a session that cannot carry it.
    ///
    /// **Nothing happens synchronously.** The server answers by renegotiating the desktop, which
    /// arrives as [`Event::Resize`](crate::Event::Resize) with the size it actually chose — which
    /// may not be the size asked for, and on a server that declines is never sent at all. So the
    /// framebuffer's size is what `Event::Resize` says, never what was requested here.
    ///
    /// **A single request is not enough, and a caller that sends one and waits will sometimes wait
    /// forever.** A Windows 11 host ignores a monitor layout that arrives while it is still
    /// bringing the session up, and reports nothing at all about having ignored it — measured:
    /// the same 800x600 layout was dropped 400 ms after that host's own capabilities PDU and
    /// honoured 6.7 s into the same session, byte for byte identical on the wire both times. There
    /// is no "ready now" to wait for, so **the retry belongs to the caller**: ask again until
    /// [`Event::Resize`](crate::Event::Resize) arrives or the caller gives up. This crate does not
    /// do it, because a retry ladder needs a clock and a policy, and both belong to whoever owns
    /// the timers — not to a queue drained by FreeRDP's event loop.
    ///
    /// `scale_percent` is the desktop's **DesktopScaleFactor**: 100 for an ordinary display and
    /// 200 for a 2x one, being how large the remote should draw its own UI. It rides the same PDU
    /// as the size and cannot be sent without one, which is why it is an argument here rather than
    /// a call of its own — a size sent without it tells the server to forget a scale it is already
    /// applying, and a client that alternated the two would fight itself.
    ///
    /// The size and the scale are adjusted to what MS-RDPEDISP permits before they are queued; see
    /// [`sanitise_size`] and [`sanitise_scale`].
    ///
    /// This does not rate-limit. A window being dragged produces a resize per frame, and every
    /// one of them costs the remote a full desktop renegotiation — so a caller driving this from
    /// a viewport needs to debounce, and only the caller knows what its own idle looks like.
    /// FreeRDP's own X11 client debounces at 200 ms (`xf_disp.c`, `RESIZE_MIN_DELAY_NS`), which is
    /// a reasonable place to start.
    pub fn resize(&self, width: u32, height: u32, scale_percent: u32) {
        let (width, height) = sanitise_size(width, height);
        self.push(Command::Resize { width, height, scale_percent: sanitise_scale(scale_percent) });
    }

    /// Move one touch contact through a transition, at a position in desktop pixels.
    ///
    /// Needs [`Connect::touch`](crate::Connect::touch) and a server that opens the touch channel —
    /// announced as [`Event::TouchReady`](crate::Event::TouchReady). Before that, and on a
    /// session that never gets the channel, the contact is **dropped**, not held: a finger is a
    /// moment, and replaying one later would be a gesture nobody made. Dropped too while the host
    /// has *suspended* touch — see [`Event::TouchReady`](crate::Event::TouchReady) — and a
    /// contact that was down when the host did so is cancelled on its behalf.
    ///
    /// `id` is the caller's name for the finger, and it is the only thing tying a `Down` to the
    /// `Move`s, and the `Up` or `Cancel`, that follow it: the plugin keeps the contact table
    /// (ten contacts, `MAX_CONTACTS` in rdpei), allocates a slot on `Down`, finds it by this id
    /// afterwards, and frees it on `Up` and `Cancel`. Any `i32` does; reusing an id for a new
    /// finger after its `Up` is fine, and a `Down` on an id that is still down takes a second
    /// slot — so a caller whose source can lose an up (a browser's `touchcancel`, a window that
    /// blurred mid-gesture) should cancel what it still has down rather than rely on the next
    /// down to tidy up.
    ///
    /// `x` and `y` are desktop pixels, like the mouse — the host takes them as absolute
    /// digitizer positions, and a contact outside the desktop is the host's to clamp. A
    /// `Down`…`Up` at one position is a tap; the host's own gesture recogniser does the rest,
    /// which is the point of sending contacts rather than interpreting them here.
    pub fn touch(&self, phase: TouchPhase, id: i32, x: i32, y: i32) {
        self.push(Command::Touch { phase, id, x, y });
    }

    fn mouse(&self, flags: u16, x: u16, y: u16) {
        self.push(Command::Mouse { flags, x, y });
    }

    pub(crate) fn push(&self, command: Command) {
        self.shared.push(command);
    }
}

/// Bring a requested desktop size inside what MS-RDPEDISP allows.
///
/// Public because a caller that decides *whether to ask at all* has to compare against the size
/// that would really be sent. Asking a server for the desktop it already has is not free — it
/// answers with a full session renegotiation — so a client that compares an unadjusted 1281 against
/// a live 1280 asks again on every viewport report, forever.
///
/// Adjusted rather than refused, and that is the considered choice. A caller sizing a desktop to
/// a viewport has an arbitrary number of pixels, and every constraint below is one it cannot do
/// anything about — refusing a resize because a window happens to be 1281 pixels wide would be a
/// bug with no fix at the call site. The server's answer arrives as
/// [`Event::Resize`](crate::Event::Resize) and carries the size it really chose, so nothing here
/// is the caller's last word on the matter.
///
/// - **The width must be even** (MS-RDPEDISP 2.2.2.2.1). Rounded *down*, so the desktop stays
///   inside the viewport that asked for it rather than growing a scrollbar by one pixel.
/// - Both dimensions are clamped to 200..=8192, the `DISPLAY_CONTROL_MIN/MAX_MONITOR_*` bounds.
pub fn sanitise_size(width: u32, height: u32) -> (u32, u32) {
    let width = width
        .clamp(sys::DISPLAY_CONTROL_MIN_MONITOR_WIDTH, sys::DISPLAY_CONTROL_MAX_MONITOR_WIDTH)
        & !1;
    let height =
        height.clamp(sys::DISPLAY_CONTROL_MIN_MONITOR_HEIGHT, sys::DISPLAY_CONTROL_MAX_MONITOR_HEIGHT);
    (width, height)
}

/// Bring a requested `DesktopScaleFactor` inside the 100..=500 MS-RDPEDISP allows.
///
/// Clamped rather than refused for the same reason as the size, and with a sharper consequence: a
/// server that finds *either* scale factor out of range must ignore **both**, so an invented
/// density does not cost part of the request — it costs the whole scaling of the desktop, silently.
/// A caller that means "no scaling" should pass 100; 0 is the same request, badly spelled, and
/// becomes 100 here.
pub fn sanitise_scale(percent: u32) -> u32 {
    percent.clamp(100, 500)
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

    /// Every one of these is a size a viewport really produces, and an odd width is the one that
    /// fails *silently* on a Windows host — the layout is accepted and the desktop never changes.
    #[test]
    fn a_requested_size_is_brought_inside_what_the_protocol_allows() {
        assert_eq!(sanitise_size(1281, 800), (1280, 800), "an odd width rounds down");
        assert_eq!(sanitise_size(1920, 1080), (1920, 1080), "a legal size is left alone");
        assert_eq!(sanitise_size(0, 0), (200, 200), "below the minimum");
        assert_eq!(sanitise_size(u32::MAX, u32::MAX), (8192, 8192), "above the maximum");
        // The clamp must not reintroduce an odd width at either end.
        for (width, _) in [sanitise_size(0, 600), sanitise_size(u32::MAX, 600), sanitise_size(1, 1)]
        {
            assert_eq!(width % 2, 0, "the clamp produced an odd width");
        }
    }

    /// Out of range is not "the density is ignored" but "the desktop is not scaled at all", so
    /// nothing may leave here outside the window the spec allows.
    #[test]
    fn a_scale_factor_cannot_leave_the_range_that_makes_it_legal() {
        assert_eq!(sanitise_scale(100), 100, "no scaling");
        assert_eq!(sanitise_scale(200), 200, "a 2x display");
        assert_eq!(sanitise_scale(0), 100, "\"unset\" is the same request, badly spelled");
        assert_eq!(sanitise_scale(u32::MAX), 500);
        for percent in [0, 1, 99, 100, 250, 500, 501, 10_000, u32::MAX] {
            assert!((100..=500).contains(&sanitise_scale(percent)), "{percent}");
        }
    }

    #[test]
    fn extended_keys_set_the_bit_above_the_scancode() {
        assert_eq!(u32::from(0x1Du8) | sys::KBDEXT, 0x11D, "right control");
        assert_eq!(sys::KBDEXT, 0x100);
    }
}
