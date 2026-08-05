//! What a session failure is, and where its words come from.

use std::ffi::CStr;
use std::fmt;

/// Why a session did not start, or did not continue.
///
/// The `message` is FreeRDP's own — `freerdp_get_last_error_string` over the code in the context —
/// because it is the only place in the system that knows the difference between "the host refused
/// the connection", "the credentials were rejected" and "the server closed the channel". A
/// wrapper that replaced it with categories of its own would be throwing that away and asking
/// the caller to guess.
#[derive(Clone, PartialEq, Eq)]
pub struct Error {
    /// FreeRDP's `ERRINFO`/`ERRCONNECT` code, or 0 where the failure was on this side.
    pub code: u32,
    /// A sentence naming the cause, ready to show someone.
    pub message: String,
}

impl Error {
    /// An error raised by this crate rather than by FreeRDP.
    pub(crate) fn local(message: impl Into<String>) -> Self {
        Self { code: 0, message: message.into() }
    }

    /// Whatever FreeRDP last recorded on this context, with `context` naming what was being
    /// attempted.
    ///
    /// # Safety
    ///
    /// `ctx` must be a live `rdpContext` from `freerdp_client_context_new`.
    pub(crate) unsafe fn from_context(ctx: *mut freerdp_sys::rdpContext, doing: &str) -> Self {
        // SAFETY: the caller guarantees `ctx`. `freerdp_get_last_error` only reads a field.
        let code = unsafe { freerdp_sys::freerdp_get_last_error(ctx) };
        // SAFETY: `freerdp_get_last_error_string` returns a pointer to a static string table
        // compiled into the library, or a fallback for an unknown code. Never null, never owned.
        let text = unsafe {
            let ptr = freerdp_sys::freerdp_get_last_error_string(code);
            if ptr.is_null() {
                String::new()
            } else {
                CStr::from_ptr(ptr).to_string_lossy().into_owned()
            }
        };
        let message = if text.is_empty() {
            format!("{doing} failed (0x{code:08X})")
        } else {
            // The code alongside the sentence, not instead of it. FreeRDP's strings are short
            // enough to be ambiguous between causes — "ERRINFO_RPC_INITIATED_DISCONNECT" reads
            // the same whoever initiated it — and the code is what a bug report can be searched
            // for.
            format!("{doing} failed: {text} (0x{code:08X})")
        };
        Self { code, message }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

// Debug prints the same sentence rather than a struct dump: this ends up inside a
// `Result<(), Error>` in an `Event`, and `{:?}` on that is what most callers will log.
impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}
