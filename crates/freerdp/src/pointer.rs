//! The remote mouse cursor.
//!
//! RDP sends a cursor as a pair of 1-bit AND and n-bit XOR masks — the encoding Windows has used
//! since it drew cursors by XORing into the framebuffer — and nothing outside a compositor wants
//! to receive that. So the conversion to straight-alpha RGBA happens here, on FreeRDP's own
//! thread, using FreeRDP's own `freerdp_image_copy_from_pointer_data`. That function is the only
//! correct implementation of the inverted-AND, XOR-inverted, colour-keyed cases, and reproducing
//! it in Rust would be reproducing a decade of Windows cursor edge cases.

use freerdp_sys as sys;

use std::fmt;

/// A cursor bitmap, in straight-alpha `RGBA`.
#[derive(Clone, PartialEq, Eq)]
pub struct CursorImage {
    pub width: u32,
    pub height: u32,
    /// Where the click actually lands, relative to the top left of the image.
    pub hotspot_x: u32,
    pub hotspot_y: u32,
    /// `width * height * 4` bytes: R, G, B, A.
    pub rgba: Vec<u8>,
}

/// Hand-written, because the derived one prints every byte.
///
/// `Cursor` is carried inside an `Event`, and the obvious thing to do with an unexpected event is
/// `{:?}` it into a log. A 384×384 cursor derives to about 2.3 MB of comma-separated integers —
/// enough to make the one line somebody needed unfindable, and to cost real time in a hot path.
/// The size and hotspot are what a reader wants; the pixels are not.
impl fmt::Debug for CursorImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CursorImage {{ {}x{}, hotspot {},{}, {} bytes }}",
            self.width,
            self.height,
            self.hotspot_x,
            self.hotspot_y,
            self.rgba.len()
        )
    }
}

/// What the pointer should look like now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cursor {
    /// The server asked for no cursor at all — a full-screen video player, say.
    Hidden,
    /// The server asked for the system default arrow, without sending a bitmap for it. There is
    /// nothing to draw here; show whatever the local platform calls a default pointer.
    Default,
    /// A bitmap.
    Image(CursorImage),
}

/// The largest cursor this will convert.
///
/// RDP's own limit is 384×384 and FreeRDP enforces it, so this is not a second gate on protocol
/// validity — it is a gate on *allocation*, sitting in front of a `Vec` sized from numbers that
/// arrived over the network. A server sending 384×384 would produce 590 KB per cursor change,
/// and cursor changes can arrive as fast as the pointer moves.
const MAX_DIMENSION: u32 = 384;

/// Convert one `rdpPointer`'s masks into RGBA.
///
/// # Safety
///
/// `pointer` must be a live `rdpPointer` inside a pointer callback, with its mask fields as
/// FreeRDP filled them in.
pub(crate) unsafe fn to_rgba(pointer: *const sys::rdpPointer) -> Option<CursorImage> {
    // SAFETY: the caller guarantees a live pointer; these are plain integer fields.
    let (width, height, hotspot_x, hotspot_y, xor_bpp) = unsafe {
        (
            (*pointer).width,
            (*pointer).height,
            (*pointer).xPos,
            (*pointer).yPos,
            (*pointer).xorBpp,
        )
    };

    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        eprintln!("freerdp: refusing a {width}x{height} cursor (limit {MAX_DIMENSION})");
        return None;
    }

    let stride = width as usize * 4;
    let mut rgba = vec![0u8; stride * height as usize];

    // SAFETY: `rgba` is exactly `stride * height` bytes, which is what the width, height and
    // stride arguments describe. The mask pointers and their lengths come from the same
    // `rdpPointer` FreeRDP just populated, and the function reads them read-only.
    let ok = unsafe {
        sys::freerdp_image_copy_from_pointer_data(
            rgba.as_mut_ptr(),
            sys::pixel_format::RGBA32,
            stride as u32,
            0,
            0,
            width,
            height,
            (*pointer).xorMaskData,
            (*pointer).lengthXorMask,
            (*pointer).andMaskData,
            (*pointer).lengthAndMask,
            xor_bpp,
            // No palette. FreeRDP only consults one for the paletted `xorBpp` values (1 and 8),
            // and a server that sends those without having sent a palette update is sending a
            // cursor nobody can render — better a null here, which FreeRDP reports as a failure,
            // than a silently wrong colour table.
            std::ptr::null(),
        )
    };
    if ok == 0 {
        eprintln!("freerdp: could not decode a {width}x{height} cursor at {xor_bpp} bpp");
        return None;
    }

    Some(CursorImage { width, height, hotspot_x, hotspot_y, rgba })
}
