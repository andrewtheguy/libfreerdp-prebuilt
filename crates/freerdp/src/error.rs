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

    /// Whether this is a failure to *reach* the host, as opposed to one the host answered with.
    ///
    /// The distinction matters to an embedder because the two have different audiences: a rejected
    /// password is for the person who typed it, and an unreachable address is for whoever owns the
    /// network — or, on macOS 15 and later, for whoever has not yet granted the app local network
    /// access, which is refused in a way indistinguishable from an address with no route.
    ///
    /// Only the two codes that mean the transport never came up. DNS failures are deliberately not
    /// included: a name that does not resolve is a decided answer, and calling it "unreachable"
    /// would send a reader to check a network that is fine.
    pub fn is_unreachable(&self) -> bool {
        // The class is in the high half and the reason in the low half — `0x0002000B` is
        // `ERRCONNECT_CONNECT_CANCELLED`. Comparing the whole word against a bare constant is the
        // mistake this masking exists to avoid.
        self.code >> 16 == freerdp_sys::FREERDP_ERROR_CONNECT_CLASS
            && matches!(
                self.code & 0xFFFF,
                freerdp_sys::ERRCONNECT_CONNECT_FAILED
                    | freerdp_sys::ERRCONNECT_CONNECT_TRANSPORT_FAILED
            )
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The masking, pinned against the one code seen in a real log — a session this crate aborted
    /// itself reports `0x0002000B`, which is in the connect class and is *not* unreachable.
    #[test]
    fn only_a_transport_that_never_came_up_is_unreachable() {
        let of = |code| Error { code, message: String::new() };
        let connect = freerdp_sys::FREERDP_ERROR_CONNECT_CLASS << 16;

        assert!(of(connect | freerdp_sys::ERRCONNECT_CONNECT_FAILED).is_unreachable());
        assert!(of(connect | freerdp_sys::ERRCONNECT_CONNECT_TRANSPORT_FAILED).is_unreachable());

        // Answered by the host, so not a network question.
        assert!(!of(connect | freerdp_sys::ERRCONNECT_LOGON_FAILURE).is_unreachable());
        assert!(!of(connect | freerdp_sys::ERRCONNECT_AUTHENTICATION_FAILED).is_unreachable());
        assert!(!of(connect | freerdp_sys::ERRCONNECT_DNS_NAME_NOT_FOUND).is_unreachable());
        assert_eq!(0x0002_000B, connect | freerdp_sys::ERRCONNECT_CONNECT_CANCELLED);
        assert!(!of(0x0002_000B).is_unreachable());

        // A different class whose low half collides with one of the two above.
        let errinfo = freerdp_sys::FREERDP_ERROR_ERRINFO_CLASS << 16;
        assert!(!of(errinfo | freerdp_sys::ERRCONNECT_CONNECT_FAILED).is_unreachable());
        // And this crate's own failures, which carry no code at all.
        assert!(!Error::local("no").is_unreachable());
    }
}
