//! The Rust-owned copy of the remote desktop.

use std::sync::{Mutex, MutexGuard};

/// A rectangle of the desktop, in pixels from the top left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// One complete frame, in `RGBX32`: four bytes per pixel, red first, the fourth byte unused.
///
/// That byte order is a decision made at `gdi_init` time and it is the one place this crate has
/// an opinion about the caller's format. It was chosen because the consumers of a headless RDP
/// framebuffer overwhelmingly want to *encode* it — as PNG, as VP9, as JPEG — and every one of
/// those wants R,G,B in memory order. FreeRDP's own clients use `BGRX32` because that is what X11
/// and Quartz want to blit; nothing here blits.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Bytes per row. Equal to `width * 4` — the buffer is packed — but read it rather than
    /// recomputing it, so that a future stride does not silently corrupt a caller.
    pub stride: usize,
    pub pixels: Vec<u8>,
}

impl Frame {
    /// The rows of one rectangle, top to bottom, each already narrowed to the rectangle's width.
    ///
    /// The shape a damage-driven encoder wants, and the reason it exists here rather than in
    /// every caller: getting `stride` into the arithmetic by hand is the classic way to produce a
    /// picture that is *nearly* right — sheared by a few pixels a row — and that is a bug people
    /// stare at for an hour.
    ///
    /// Returns nothing for a rectangle that is not wholly inside the frame. A damage rectangle
    /// from FreeRDP always is, but a resize the caller has not processed yet could make one stale,
    /// and an empty iterator is a better answer to that than a panic in an encoder.
    pub fn rows(&self, rect: Rect) -> impl Iterator<Item = &[u8]> {
        let fits = rect.x.saturating_add(rect.width) <= self.width
            && rect.y.saturating_add(rect.height) <= self.height;
        let (start, count) = if fits { (rect.y as usize, rect.height as usize) } else { (0, 0) };
        let left = rect.x as usize * 4;
        let right = left + rect.width as usize * 4;
        (start..start + count).map(move |row| {
            let base = row * self.stride;
            &self.pixels[base + left..base + right]
        })
    }
}

/// The framebuffer the FreeRDP thread writes and the caller reads.
///
/// Locked for the duration of one damaged-rectangle copy on the writing side, and for the
/// duration of whatever the caller does inside [`Framebuffer::with`] on the reading side. So a
/// caller that spends a long time in there is holding up the RDP thread's next paint — which is
/// the right trade for a headless encoder, where the alternative is double-buffering a
/// multi-megabyte image to save a copy nobody was waiting on.
pub struct Framebuffer {
    frame: Mutex<Frame>,
}

impl Framebuffer {
    pub(crate) fn new() -> Self {
        Self { frame: Mutex::new(Frame { width: 0, height: 0, stride: 0, pixels: Vec::new() }) }
    }

    /// Read the frame. The lock is held for the duration of `f`.
    pub fn with<R>(&self, f: impl FnOnce(&Frame) -> R) -> R {
        f(&self.lock())
    }

    /// The current size, without copying anything.
    pub fn size(&self) -> (u32, u32) {
        let frame = self.lock();
        (frame.width, frame.height)
    }

    /// A poisoned lock is not a reason to fail here.
    ///
    /// Poisoning means a panic happened while some *other* thread held this lock, and the only
    /// thing under it is a pixel buffer: there are no invariants across fields to have been left
    /// half-updated, so the worst case is a frame with a stale rectangle in it. Propagating the
    /// poison instead would turn one panic in a caller's `with` closure into a session that can
    /// never paint again.
    fn lock(&self) -> MutexGuard<'_, Frame> {
        self.frame.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Resize and clear. Called on connect and on a desktop resize.
    pub(crate) fn resize(&self, width: u32, height: u32) {
        let mut frame = self.lock();
        frame.width = width;
        frame.height = height;
        frame.stride = width as usize * 4;
        frame.pixels.clear();
        let bytes = frame.stride * height as usize;
        frame.pixels.resize(bytes, 0);
    }

    /// Copy one damaged rectangle out of FreeRDP's buffer.
    ///
    /// # Safety
    ///
    /// `src` must point to at least `src_stride * (rect.y + rect.height)` readable bytes in the
    /// same `RGBX32` format, and stay valid for the call. In practice it is
    /// `gdi->primary_buffer`, read inside an `EndPaint` callback, which is the one window in
    /// which FreeRDP is not writing to it.
    pub(crate) unsafe fn blit(&self, src: *const u8, src_stride: usize, rect: Rect) {
        let mut frame = self.lock();
        // A rectangle that does not fit is dropped rather than clamped, and it is *reported*
        // rather than ignored: the only way to get one is for the framebuffer and the GDI to
        // disagree about the desktop size, which means a resize was missed. Clamping would paint
        // a sheared image and hide that.
        if rect.x.saturating_add(rect.width) > frame.width
            || rect.y.saturating_add(rect.height) > frame.height
        {
            eprintln!(
                "freerdp: dropping a {}x{}+{}+{} damage rectangle that does not fit a {}x{} \
                 framebuffer — a resize was missed",
                rect.width, rect.height, rect.x, rect.y, frame.width, frame.height
            );
            return;
        }

        let bytes = rect.width as usize * 4;
        let left = rect.x as usize * 4;
        let dst_stride = frame.stride;
        for row in 0..rect.height as usize {
            let src_offset = (rect.y as usize + row) * src_stride + left;
            let dst_offset = (rect.y as usize + row) * dst_stride + left;
            // SAFETY: `src` is guaranteed by the caller to cover this rectangle at this stride,
            // and the bounds check above guarantees the destination range is inside `pixels`.
            // The two buffers cannot overlap — one is FreeRDP's, one is this Vec.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    src.add(src_offset),
                    frame.pixels.as_mut_ptr().add(dst_offset),
                    bytes,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled(width: u32, height: u32, byte: u8) -> Vec<u8> {
        vec![byte; width as usize * height as usize * 4]
    }

    #[test]
    fn a_blit_lands_where_the_rectangle_says() {
        let fb = Framebuffer::new();
        fb.resize(4, 4);
        let src = filled(4, 4, 0xAB);
        // SAFETY: `src` is 4x4 RGBX32 with a stride of 16, which covers the rectangle below.
        unsafe { fb.blit(src.as_ptr(), 16, Rect { x: 1, y: 2, width: 2, height: 1 }) };

        fb.with(|frame| {
            // Row 2, columns 1 and 2 — and nothing else.
            for (index, byte) in frame.pixels.iter().enumerate() {
                let expected = if (36..44).contains(&index) { 0xAB } else { 0 };
                assert_eq!(*byte, expected, "byte {index}");
            }
        });
    }

    /// The failure this guards is a *silent* one: a stale rectangle against a resized frame would
    /// otherwise read past the end of a row and shear the picture.
    #[test]
    fn a_rectangle_that_does_not_fit_is_dropped_rather_than_clamped() {
        let fb = Framebuffer::new();
        fb.resize(2, 2);
        let src = filled(4, 4, 0xFF);
        // SAFETY: `src` is 4x4 with a stride of 16; the rectangle is inside *it* but not inside
        // the 2x2 framebuffer, which is the case being tested.
        unsafe { fb.blit(src.as_ptr(), 16, Rect { x: 0, y: 0, width: 4, height: 4 }) };
        fb.with(|frame| assert!(frame.pixels.iter().all(|b| *b == 0)));
    }

    #[test]
    fn rows_narrows_to_the_rectangle() {
        let fb = Framebuffer::new();
        fb.resize(3, 2);
        fb.with(|frame| {
            let rows: Vec<_> = frame.rows(Rect { x: 1, y: 0, width: 2, height: 2 }).collect();
            assert_eq!(rows.len(), 2);
            assert!(rows.iter().all(|row| row.len() == 8));
            // And a rectangle hanging off the edge yields nothing rather than panicking.
            assert_eq!(frame.rows(Rect { x: 2, y: 0, width: 2, height: 1 }).count(), 0);
        });
    }
}
