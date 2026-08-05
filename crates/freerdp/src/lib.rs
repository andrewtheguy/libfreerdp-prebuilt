//! A safe, **headless** RDP client over FreeRDP 3.
//!
//! Screen, cursor, keyboard, mouse, clipboard and resize, and nothing else. There is no window, no
//! toolkit and no drawing: [`Session::start`] connects, keeps a complete framebuffer up to date
//! in Rust-owned memory, and posts an [`Event`] whenever a rectangle of it changes. What the
//! caller does with those pixels — encode them, diff them, throw them away — is not this crate's
//! business.
//!
//! ```no_run
//! use freerdp::{Connect, Event, Session};
//!
//! let (session, events) = Session::start(Connect {
//!     host: "desktop.example".into(),
//!     username: "andrew".into(),
//!     password: std::env::var("RDP_PASSWORD").unwrap(),
//!     width: 1280,
//!     height: 800,
//!     ..Connect::default()
//! });
//!
//! for event in events {
//!     match event {
//!         Event::Connected { width, height } => println!("{width}x{height}"),
//!         Event::Paint(rect) => session.framebuffer().with(|frame| {
//!             let _damaged = frame.rows(rect);
//!         }),
//!         Event::Ended(result) => { println!("{result:?}"); break }
//!         _ => {}
//!     }
//! }
//! ```
//!
//! # The three threads
//!
//! ```text
//!   your thread  ──  Session, Input, Clipboard      Receiver<Event>  ──  your thread
//!         │  command queue + a WinPR event                  ▲
//!         ▼                                                 │
//!   the FreeRDP thread: WaitForMultipleObjects, then freerdp_check_event_handles
//! ```
//!
//! [`Session::start`] takes an OS thread and never gives it back until the session ends. That is
//! not a stylistic choice: FreeRDP's event loop is a blocking `WaitForMultipleObjects` over
//! handles it owns, and every callback — paint, cursor, clipboard — is called from inside
//! `freerdp_check_event_handles` on that thread. So input cannot be a direct call. It goes onto a
//! queue, the queue's WinPR event is first in the handle array, and the FreeRDP thread drains it
//! and makes the `freerdp_input_send_*` calls itself. Nothing outside this crate ever touches a
//! FreeRDP pointer.
//!
//! The framebuffer is **copied**, not shared. On each `EndPaint` the damaged rectangle is copied
//! out of FreeRDP's buffer into Rust-owned memory under a short lock. That costs one memcpy of
//! the damaged region — negligible beside encoding it — and buys the property that matters: a
//! reader holding [`Frame`] cannot be looking at memory FreeRDP is reallocating underneath it,
//! which is exactly what a desktop resize does.
//!
//! # What this does not do
//!
//! - **No audio.** The archives carry `rdpsnd`, and nothing here binds it.
//! - **No graphics pipeline.** `FreeRDP_SupportGraphicsPipeline` is set to `FALSE`, deliberately
//!   and with a measurement behind it — see [`Connect`].
//! - **No certificate verification.** Also deliberate, also documented on [`Connect`]. Read that
//!   before using this on a network you do not control.
//! - **No file-transfer clipboard.** Formats and their bytes cross; `CB_STREAM_FILECLIP_ENABLED`
//!   is not advertised, so a copied *file* stays where it is.
//! - **No multiple monitors.** [`Input::resize`] sends a layout of exactly one, and the desktop is
//!   one framebuffer. MS-RDPEDISP carries up to sixteen and this crate uses one of them.
//! - **No resize debouncing.** [`Input::resize`] sends what it is given, and each one costs the
//!   remote a full session renegotiation — a caller driving it from a window has to rate-limit,
//!   and only the caller knows what its own idle looks like.
//!
//! # Panics and aborts
//!
//! Every `extern "C"` callback wraps its body in [`std::panic::catch_unwind`], because unwinding
//! through a C frame is undefined behaviour. A panic inside one is reported on stderr, the
//! callback returns failure to FreeRDP, and the session ends — it is not swallowed. A consumer
//! built with `panic = "abort"` gets an abort instead, which is also sound.
//!
//! A **segfault inside FreeRDP's C ends the process**, and no amount of Rust in front of it
//! changes that. If one session's fault must not take the host down, this crate has to run in its
//! own process; that is a decision for the embedder, and this crate cannot make it.

mod clipboard;
mod error;
mod framebuffer;
mod input;
mod pointer;
mod session;

pub use clipboard::{Clipboard, ClipboardEvent, ClipboardFormat};
pub use error::Error;
pub use framebuffer::{Frame, Framebuffer, Rect};
pub use input::{Input, MouseButton};
pub use pointer::{Cursor, CursorImage};
pub use session::{Connect, Event, KeepAlive, Security, Session};

/// The FreeRDP this is linked against, e.g. `3.30.0`.
///
/// Read from the library rather than from a constant, so it answers what actually got linked.
pub fn freerdp_version() -> &'static str {
    freerdp_sys::version()
}
