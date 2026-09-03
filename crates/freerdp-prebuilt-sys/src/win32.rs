//! The handful of Win32 names the generated Windows bindings cannot carry.
//!
//! Off Windows, WinPR *is* the Win32 API: `winpr/synch.h` declares `CreateEventA`, `winpr/handle.h`
//! declares `CloseHandle`, `winpr/error.h` defines `ERROR_INTERNAL_ERROR`, and bindgen emits every
//! one of them because they are declared in FreeRDP's own installed headers. On `_WIN32` each of
//! those headers steps aside — `#ifndef _WIN32` around the declarations, `#include <winerror.h>`
//! in place of the defines — so the names come from the Windows SDK, which
//! `--allowlist-file` deliberately excludes. The Windows bindings therefore describe FreeRDP's API
//! and nothing of the platform's, and this file supplies exactly the platform names the wrapper
//! crate calls: measured by grepping `crates/freerdp/src` for `sys::` uses that the generated
//! `bindings_windows.rs` lacks, and asserted by `gen-bindings.sh`, which refuses to write a
//! Windows bindings file that declares any of them (a duplicate would be a compile error here).
//!
//! Hand-written rather than a `windows-sys` dependency: this crate's `[dependencies]` is empty,
//! and that is a stated property of it. The signatures are the SDK's, with the same type aliases
//! the bindings emit for the FreeRDP signatures that reach them, so a `HANDLE` from
//! `freerdp_get_event_handles` is the `HANDLE` `WaitForMultipleObjects` takes.
//!
//! The values are the SDK's too: `INFINITE` and `WAIT_FAILED` are both `0xFFFFFFFF`
//! (`synchapi.h`, `winbase.h`) and `ERROR_INTERNAL_ERROR` is 1359 (`winerror.h`). Each is the
//! number the Linux and Apple bindings carry for the same name, which is what keeps the wrapper's
//! code identical across the three files. The `CHANNEL_RC_*` codes are *not* here: `wtsapi.h`
//! defines them on Windows as well (measured on the first Windows generation, which refused a
//! duplicate), so the generated file carries them on every platform.

use super::{BOOL, DWORD, HANDLE};

pub const INFINITE: u32 = 0xFFFF_FFFF;
pub const WAIT_FAILED: u32 = 0xFFFF_FFFF;
pub const ERROR_INTERNAL_ERROR: u32 = 0x0000_054F;

#[link(name = "kernel32")]
extern "system" {
    pub fn CreateEventA(
        lpEventAttributes: *mut ::std::os::raw::c_void,
        bManualReset: BOOL,
        bInitialState: BOOL,
        lpName: *const ::std::os::raw::c_char,
    ) -> HANDLE;
    pub fn SetEvent(hEvent: HANDLE) -> BOOL;
    pub fn ResetEvent(hEvent: HANDLE) -> BOOL;
    pub fn CloseHandle(hObject: HANDLE) -> BOOL;
    pub fn WaitForMultipleObjects(
        nCount: DWORD,
        lpHandles: *const HANDLE,
        bWaitAll: BOOL,
        dwMilliseconds: DWORD,
    ) -> DWORD;
}
