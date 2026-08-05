//! The clipboard channel, `cliprdr`.
//!
//! RDP's clipboard is not a value that gets copied; it is a **negotiation**. Whoever copies
//! something advertises a list of format ids; whoever pastes asks for one of them by id and gets
//! the bytes back. Either side can be either party, and the round trip is asynchronous — so this
//! crate carries the negotiation to the caller rather than pretending it is a `String`.
//!
//! That is deliberate. A wrapper that decoded `CF_UNICODETEXT` into a `String` here would have to
//! choose a line-ending policy, a fallback order between text formats, and a behaviour for the
//! formats it did not understand — three decisions that belong to whoever is bridging this to a
//! real clipboard, and all three of which differ between a browser, a terminal and a native app.
//! What crosses this boundary is a format id and a `Vec<u8>`.

use freerdp_sys as sys;

use crate::input::{Command, Input};
use std::ffi::CStr;

/// One advertised clipboard format.
///
/// `id` is a Windows clipboard format id: the standard ones are small constants
/// (`CF_TEXT` = 1, `CF_UNICODETEXT` = 13, `CF_DIB` = 8), and anything above 0xC000 is a
/// registered format whose meaning is entirely in `name`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardFormat {
    pub id: u32,
    /// The registered name, for formats that have one — `"HTML Format"`, `"PNG"`, `"text/uri-list"`.
    pub name: Option<String>,
}

impl ClipboardFormat {
    pub fn new(id: u32) -> Self {
        Self { id, name: None }
    }
}

/// Something the remote clipboard did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardEvent {
    /// The channel finished its capability exchange. Nothing may be advertised before this
    /// arrives, and the server sends it once per connection.
    Ready,
    /// The remote copied something, and these are the formats it can supply.
    RemoteFormats(Vec<ClipboardFormat>),
    /// The answer to a [`Clipboard::request`].
    RemoteData { format: u32, data: Vec<u8> },
    /// The remote refused a [`Clipboard::request`] — the data went stale, or it changed its mind
    /// about being able to supply that format.
    RemoteDataFailed { format: u32 },
    /// The remote wants to paste something this side advertised, and **is waiting**.
    ///
    /// Every one of these must be answered by exactly one [`Clipboard::respond`], including with
    /// `None`. A request left unanswered is a remote application blocked in its paste handler,
    /// which on Windows means a frozen window rather than an error.
    LocalDataRequest { format: u32 },
}

/// The clipboard side of a [`Session`](crate::Session).
///
/// Cheap to clone; every clone feeds the same queue as [`Input`].
#[derive(Clone)]
pub struct Clipboard {
    pub(crate) input: Input,
}

impl Clipboard {
    /// Tell the remote what this side can supply, replacing any previous advertisement.
    ///
    /// Call this when the *local* clipboard changes. An empty list is meaningful: it says this
    /// side has nothing, which is how a remote paste menu gets greyed out.
    pub fn advertise(&self, formats: Vec<ClipboardFormat>) {
        self.input.push(Command::ClipboardAdvertise(formats));
    }

    /// Ask the remote for one of the formats it advertised.
    ///
    /// The answer arrives later as [`ClipboardEvent::RemoteData`] or
    /// [`ClipboardEvent::RemoteDataFailed`].
    pub fn request(&self, format: u32) {
        self.input.push(Command::ClipboardRequest(format));
    }

    /// Answer a [`ClipboardEvent::LocalDataRequest`]. `None` means "I cannot supply that".
    pub fn respond(&self, format: u32, data: Option<Vec<u8>>) {
        self.input.push(Command::ClipboardRespond { format, data });
    }
}

/// Read a `CLIPRDR_FORMAT_LIST` into owned Rust values.
///
/// # Safety
///
/// `list` must be the pointer FreeRDP passed to a `ServerFormatList` callback, valid for the
/// duration of that callback.
pub(crate) unsafe fn read_format_list(
    list: *const sys::CLIPRDR_FORMAT_LIST,
) -> Vec<ClipboardFormat> {
    // SAFETY: the caller guarantees `list` for the duration of the callback.
    let (count, formats) = unsafe { ((*list).numFormats as usize, (*list).formats) };
    if formats.is_null() {
        return Vec::new();
    }
    (0..count)
        .map(|index| {
            // SAFETY: FreeRDP guarantees `formats` points to `numFormats` entries.
            let format = unsafe { &*formats.add(index) };
            let name = if format.formatName.is_null() {
                None
            } else {
                // FreeRDP has already converted the wire's UTF-16 to UTF-8 here, so this is a
                // plain C string. `to_string_lossy` rather than a hard failure: a malformed name
                // is a reason to show mojibake, not to drop a format the caller might still be
                // able to use by id.
                // SAFETY: non-null and NUL-terminated, owned by the CLIPRDR_FORMAT.
                Some(unsafe { CStr::from_ptr(format.formatName) }.to_string_lossy().into_owned())
            };
            ClipboardFormat { id: format.formatId, name }
        })
        .collect()
}

/// The capability set this client advertises, as raw bytes for FreeRDP's `ClientCapabilities`.
///
/// `CB_USE_LONG_FORMAT_NAMES` and nothing else. Long format names are what carry a registered
/// format's name as UTF-16 rather than as a truncated 32-byte ASCII field, and every peer since
/// Windows Vista supports them.
///
/// What is deliberately **not** here is `CB_STREAM_FILECLIP_ENABLED`. Advertising it commits this
/// side to serving `FileGroupDescriptorW` and a whole file-contents protocol on top of the
/// clipboard, and a peer that takes the offer and finds nothing behind it hangs rather than
/// falling back.
pub(crate) fn general_capability_flags() -> u32 {
    sys::CB_USE_LONG_FORMAT_NAMES
}
