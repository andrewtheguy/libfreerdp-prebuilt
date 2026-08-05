//! Raw FFI for FreeRDP 3's client API, linked from prebuilt static archives.
//!
//! This crate builds no C. `build.rs` finds archives that were already built — by `./build.sh`
//! locally, or by this repository's release pipeline — verifies each against the MANIFEST beside
//! them, and emits the link flags in the order that MANIFEST records. A consumer needs no cmake,
//! no pkg-config, no OpenSSL installed, and no LLVM.
//!
//! Everything public comes from [`bindings`], which is bindgen's output over the headers
//! committed in `include/` and re-exported at the crate root, so `freerdp_sys::freerdp_connect`
//! reads the way the C does. For a safe interface, use the `freerdp` crate beside this one; this
//! is the layer it is built on.
//!
//! # Which library is linked
//!
//! [`version`] returns what the archives themselves report. FreeRDP compiles its own version
//! string in, so there is nothing to transcribe by hand and nothing to keep in step: asking the
//! library is how a consumer checks it is talking to the FreeRDP this crate was generated against
//! rather than to a system one that won the link.
//!
//! # What is in the archives, and what is not
//!
//! The client side of FreeRDP 3, with six channels compiled in — `cliprdr`, `disp`, `drdynvc`,
//! `rdpdr`, `rdpgfx` and `rdpsnd` — and a static OpenSSL beside it. **No server, no shadow, no
//! proxy, no X11 or Wayland or SDL client, no FFmpeg, no OpenH264, no smartcard, no Kerberos.**
//! The channel set is frozen at cmake *configure* time (it generates the addin table from it), so
//! it is deliberately wider than what any one consumer calls: static linking is per-object, and a
//! binary that never mentions `rdpsnd` pulls none of it in.
//!
//! # Safety
//!
//! Nothing here is safe, and FreeRDP's shape makes that sharper than usual:
//!
//! - `freerdp_client_context_new` returns a context that must be freed exactly once, with
//!   `freerdp_client_context_free`, and never while its event loop is running.
//! - The callbacks in `rdpContext`, `rdpUpdate` and `rdpInput` are raw function pointers that
//!   FreeRDP calls **from its own thread**. A Rust panic crossing one of them is undefined
//!   behaviour; every `extern "C"` callback needs its own `catch_unwind`.
//! - `gdi->primary_buffer` is a raw framebuffer owned by FreeRDP and reallocated on a desktop
//!   resize, so a slice built over it is invalidated by an event that arrives on another thread.
//! - `freerdp_settings_set_string` copies, but most other settings accessors do not; the pointer
//!   families in `settings.h` differ from one another and the header is the only authority.
//! - `PubSub_Subscribe` is variadic and unchecked — the handler signature has to match the event
//!   name, and a mismatch compiles.

// bindgen's own header already carries the allow attributes these names need.
//
// **Two generated files, selected here.** Not duplication: `winpr/wtypes.h` typedefs `BOOL` to
// `int32_t` off Apple and to `bool`/`signed char` on it, so the same headers describe a four-byte
// value on Linux and a one-byte value on macOS — in the return type of nearly every FreeRDP
// function and in every callback signature. One committed file would be an ABI mismatch on one of
// the two platforms, and not a mismatch that fails to compile. `gen-bindings.sh` explains the
// measurement and CI checks each file on its own platform.
//
// `target_vendor` rather than `target_os`, so iOS and the simulator select the Apple ABI too —
// there are no archives for them, but the wrong bindings would be a worse error message than the
// one `build.rs` gives.
// **`unnecessary_transmutes` is a rustc lint, not a clippy one**, so the `#![allow(clippy::all)]`
// bindgen writes into each generated file does not cover it and `-D warnings` turns it into a
// failed build. It fires in `bindings_linux.rs` and *not* in `bindings_apple.rs`, which is the
// two-ABI split above showing up in a place nobody would predict: `BOOL` is `int32_t` on Linux, so
// bindgen's bitfield accessors transmute `i32` to `u32`, while on Apple it is a one-byte type and
// no transmute is generated at all. Generated code is not ours to lint, and this is the item that
// covers both files. `unknown_lints` first, so a toolchain predating the lint is not itself a
// warning — this crate's MSRV is older than the lint.
#[allow(unknown_lints, unnecessary_transmutes)]
#[cfg_attr(target_vendor = "apple", path = "bindings_apple.rs")]
#[cfg_attr(not(target_vendor = "apple"), path = "bindings_linux.rs")]
mod bindings;

pub use bindings::*;

/// What the linked FreeRDP says it is, e.g. `3.30.0`.
///
/// Reads `freerdp_get_version_string()`, which FreeRDP compiles in from its own source tree. The
/// version this crate's bindings were generated against is [`PREBUILT_VERSION`] — comparing the
/// two is how a consumer notices it is linked against something other than the archives this
/// repository publishes.
pub fn version() -> &'static str {
    // SAFETY: `freerdp_get_version_string` takes no arguments, cannot fail, and returns a pointer
    // to a static string constant compiled into the library — so the lifetime is genuinely
    // 'static and there is nothing to free.
    let ptr = unsafe { freerdp_get_version_string() };
    assert!(!ptr.is_null(), "freerdp_get_version_string returned null");
    // SAFETY: as above — a NUL-terminated string constant in the archive's read-only data.
    unsafe { std::ffi::CStr::from_ptr(ptr) }
        .to_str()
        .expect("FreeRDP's version string is ASCII")
}

/// The FreeRDP version this crate's bindings and archives are built from, from `freerdp.env`.
pub const PREBUILT_VERSION: &str = env!("FREERDP_PREBUILT_VERSION");

/// FreeRDP's `PIXEL_FORMAT_*` constants, which bindgen cannot emit.
///
/// They are *function-like* macros — `#define PIXEL_FORMAT_RGBX32 FREERDP_PIXEL_FORMAT(32,
/// FREERDP_PIXEL_FORMAT_TYPE_RGBA, 0, 8, 8, 8)` — and bindgen only evaluates object-like ones. So
/// this is the one piece of `freerdp/codec/color.h` that has to be written by hand.
///
/// It is written as the *macro*, not as its answers: [`format`] is `FREERDP_PIXEL_FORMAT` field
/// for field, and every constant below is the same argument list its header line has, over the
/// `FREERDP_PIXEL_FORMAT_TYPE_*` values bindgen *did* emit. Transcribing the six resulting
/// integers instead would be four bytes shorter and silently wrong the day FreeRDP changed the
/// packing — and "silently" is the operative word, because a wrong pixel format is not a
/// compile error or a crash. It is a picture with the red and blue channels swapped.
pub mod pixel_format {
    /// `FREERDP_PIXEL_FORMAT(bpp, type, a, r, g, b)`, transcribed from `codec/color.h`.
    pub const fn format(bpp: u32, kind: u32, a: u32, r: u32, g: u32, b: u32) -> u32 {
        (bpp << 24) | (kind << 16) | (a << 12) | (r << 8) | (g << 4) | b
    }

    use super::{
        FREERDP_PIXEL_FORMAT_TYPE_ABGR, FREERDP_PIXEL_FORMAT_TYPE_ARGB,
        FREERDP_PIXEL_FORMAT_TYPE_BGRA, FREERDP_PIXEL_FORMAT_TYPE_RGBA,
    };

    /// Bytes in memory are R, G, B, unused. What `gdi_init` should be given for a consumer that
    /// wants to walk the framebuffer three bytes at a time without a swizzle.
    pub const RGBX32: u32 = format(32, FREERDP_PIXEL_FORMAT_TYPE_RGBA, 0, 8, 8, 8);
    /// R, G, B, A.
    pub const RGBA32: u32 = format(32, FREERDP_PIXEL_FORMAT_TYPE_RGBA, 8, 8, 8, 8);
    /// B, G, R, unused — FreeRDP's own default for most clients.
    pub const BGRX32: u32 = format(32, FREERDP_PIXEL_FORMAT_TYPE_BGRA, 0, 8, 8, 8);
    /// B, G, R, A.
    pub const BGRA32: u32 = format(32, FREERDP_PIXEL_FORMAT_TYPE_BGRA, 8, 8, 8, 8);
    /// A, R, G, B with the alpha byte unused.
    pub const XRGB32: u32 = format(32, FREERDP_PIXEL_FORMAT_TYPE_ARGB, 0, 8, 8, 8);
    /// A, B, G, R with the alpha byte unused.
    pub const XBGR32: u32 = format(32, FREERDP_PIXEL_FORMAT_TYPE_ABGR, 0, 8, 8, 8);

    #[cfg(test)]
    mod tests {
        /// The packing, against values read out of `codec/color.h` by hand.
        ///
        /// Circular-looking and not: the constants above are built from the *macro*, and these
        /// are what the macro's arithmetic comes to. A change to `format()` that still compiles
        /// — an operator precedence slip, a shift off by four — is exactly what this catches, and
        /// it is the only thing standing between that and a swapped colour channel.
        #[test]
        fn the_packing_matches_the_header() {
            assert_eq!(super::RGBX32, 0x2003_0888);
            assert_eq!(super::RGBA32, 0x2003_8888);
            assert_eq!(super::BGRX32, 0x2004_0888);
            assert_eq!(super::XRGB32, 0x2001_0888);
        }
    }
}

#[cfg(test)]
mod tests {
    /// The archives report the version the bindings were generated against.
    ///
    /// The one test that catches a system FreeRDP winning the link, and it costs a function call.
    /// `starts_with` rather than equality because FreeRDP appends a suffix to prereleases.
    #[test]
    fn the_linked_library_is_the_pinned_one() {
        let linked = super::version();
        assert!(
            linked.starts_with(super::PREBUILT_VERSION),
            "linked FreeRDP reports {linked}, but this crate was generated against {}",
            super::PREBUILT_VERSION,
        );
    }
}
