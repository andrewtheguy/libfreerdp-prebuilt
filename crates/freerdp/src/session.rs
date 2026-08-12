//! The session: its configuration, its thread, and the C callbacks that feed it.
//!
//! Everything unsafe in this crate is here. The shape is FreeRDP's own headless embedder pattern,
//! the same one `guacd` uses:
//!
//! 1. describe a client with `RDP_CLIENT_ENTRY_POINTS`, whose `ContextSize` reserves room for a
//!    pointer back to Rust;
//! 2. `freerdp_client_context_new`, then store that pointer;
//! 3. apply every setting;
//! 4. `freerdp_connect`;
//! 5. loop on `freerdp_get_event_handles` + `WaitForMultipleObjects` +
//!    `freerdp_check_event_handles` until something ends it.
//!
//! Callbacks recover the Rust side by casting `rdpContext*` back to the wrapper context and
//! reading its `bridge` field — the standard trick, and the reason `ContextSize` exists.

use freerdp_sys as sys;

use crate::audio::{self, Audio};
use crate::clipboard::{self, Clipboard, ClipboardEvent, ClipboardFormat};
use crate::error::Error;
use crate::framebuffer::{Framebuffer, Rect};
use crate::input::{Command, Input};
use crate::pointer::{self, Cursor};
use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CString};
use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ------------------------------------------------------------------ configuration

/// How the connection is secured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Security {
    /// Offer everything and let the server choose — what a normal client does.
    #[default]
    Auto,
    /// CredSSP/NLA only. Credentials are proven before the desktop exists, and the TLS public key
    /// is bound into that exchange, so this is the only mode that is not trivially interceptable.
    Nla,
    /// TLS without CredSSP. The credentials go to whoever answered.
    Tls,
    /// The legacy RDP security layer. Present because some appliances still speak nothing else.
    Rdp,
}

/// TCP keepalive, in FreeRDP's own terms.
///
/// FreeRDP applies these itself in `libfreerdp/core/tcp.c` — `TCP_KEEPIDLE`, `TCP_KEEPINTVL`,
/// `TCP_KEEPCNT` and, on Linux, `TCP_USER_TIMEOUT` — so an embedder does not need to reach for
/// the socket. They matter more than they look: without them a session whose peer vanished
/// without a FIN sits in `WaitForMultipleObjects` indefinitely, and the user sees a frozen
/// desktop rather than a disconnection.
#[derive(Clone, Copy, Debug)]
pub struct KeepAlive {
    /// Idle time before the first probe.
    pub idle: Duration,
    /// Gap between probes.
    pub interval: Duration,
    /// Unanswered probes before the connection is declared dead.
    pub retries: u32,
    /// How long unacknowledged data may sit in the send queue before the connection is dropped.
    /// Linux only; ignored elsewhere, by FreeRDP rather than by this crate.
    pub ack_timeout: Duration,
}

impl Default for KeepAlive {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(10),
            interval: Duration::from_secs(5),
            retries: 3,
            ack_timeout: Duration::from_secs(30),
        }
    }
}

/// Everything needed to open a session.
pub struct Connect {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub domain: Option<String>,
    /// The desktop size to ask for. The server may answer with something else, which arrives as
    /// [`Event::Connected`] and, later, as [`Event::Resize`].
    pub width: u32,
    pub height: u32,
    pub security: Security,
    /// Whether to load `cliprdr`. When false there is no [`Session::clipboard`].
    pub clipboard: bool,
    /// Whether to load `rdpsnd` and where its wave buffers go. `None` asks the server for no
    /// sound at all, which is the default.
    ///
    /// Unlike every other output this crate produces, sound does **not** arrive as an [`Event`]:
    /// the sink is called on the FreeRDP thread as each buffer is decoded. That is what keeps it
    /// off the back of a queue of paint rectangles — see [`AudioSink`](crate::AudioSink), which
    /// also says what a sink may not do on that thread.
    pub audio: Option<Audio>,
    /// Whether to load `disp` and advertise DisplayControl, which is what makes
    /// [`Input::resize`](crate::Input::resize) do anything.
    ///
    /// **Off by default**, unlike the clipboard, and for a reason worth knowing before turning it
    /// on: a server answers a monitor layout by *renegotiating the whole session* — a
    /// deactivate/reactivate sequence that tears down the desktop, the capability set and the
    /// framebuffer and builds them again. That is by far the most disruptive thing a client can
    /// ask for, it is where an RDP implementation is most likely to have a bug, and a client that
    /// never asks is a client that never meets it. So it is opt-in, per session, by the embedder
    /// that knows whether its users want the feature more than they want the session to survive.
    ///
    /// FreeRDP handles the reactivation itself, inside `freerdp_check_event_handles` — this crate
    /// sees only a [`Event::Resize`] afterwards. (Under [`Connect::egfx`] there is no
    /// reactivation at all: the layout costs a graphics reset instead.)
    pub resize: bool,
    /// Whether to advertise the graphics pipeline (EGFX), with RemoteFX riding beside it. On by
    /// default, and deliberately independent of [`Connect::resize`] — the two used to be one
    /// switch, and the coupling is now the embedder's trade to make:
    ///
    /// With the pipeline, a monitor layout is answered by a graphics reset — no
    /// Deactivation-Reactivation, channels and sound untouched — but a Windows host then renders
    /// text that stays blurry for the rest of the session (observed repeatedly by the person
    /// using it). Without the pipeline, the same layout costs a full reactivation after which
    /// the server renders the new desktop from scratch, sharp — and one kind of session then
    /// resizes by *reconnecting* rather than by a layout: a server whose sound arrived over the
    /// dynamic `rdpsnd` transport, which is a Windows host, whose audio redirector does not
    /// survive the reactivation. The reconnect renegotiates the channels and the sound with
    /// them, and still surfaces as the one [`Event::Resize`]. See `resize` in this module for
    /// the measurements.
    pub egfx: bool,
    pub connect_timeout: Duration,
    pub keepalive: KeepAlive,
}

impl Default for Connect {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 3389,
            username: String::new(),
            password: String::new(),
            domain: None,
            width: 1024,
            height: 768,
            security: Security::default(),
            clipboard: true,
            audio: None,
            resize: false,
            egfx: true,
            connect_timeout: Duration::from_secs(15),
            keepalive: KeepAlive::default(),
        }
    }
}

// ------------------------------------------------------------------ events

/// Something the session did.
#[derive(Clone, Debug)]
pub enum Event {
    /// The desktop exists and its size is settled. Always the first event of a successful
    /// session; a failed one goes straight to [`Event::Ended`].
    Connected { width: u32, height: u32 },
    /// This rectangle of the framebuffer changed.
    Paint(Rect),
    /// The server finished a frame: every [`Event::Paint`] since the last `Frame` belongs to one
    /// coherent picture, and this is the moment to present it. Sent only when the server says so
    /// itself — a legacy-path frame marker order, a surface frame marker, or the graphics
    /// pipeline's per-frame flush — never guessed from timing. A server that marks no frames
    /// sends none of these, so a consumer keeps whatever pacing it had and treats this as the
    /// upgrade it is.
    Frame,
    /// The server changed the desktop size. The framebuffer has already been resized and
    /// cleared, so everything is about to be repainted.
    ///
    /// Sent whether the change was asked for or not: a server may resize a session on its own —
    /// a console session whose real display mode changed, for instance — and that arrives here
    /// identically to the answer to an [`Input::resize`](crate::Input::resize).
    ///
    /// A server normally repaints after a resize, but it is not obliged to, and the framebuffer
    /// is blank until it does. A caller that cannot show a blank desktop should follow this with
    /// [`Input::refresh`](crate::Input::refresh) — which this crate does *not* do for it, because
    /// the refresh must not be sent from inside the callback: this fires part-way through the
    /// reactivation sequence, before the connection is ready to carry client PDUs. Going through
    /// the queue puts it after that, which is why it is the caller's call and not a hidden one.
    Resize { width: u32, height: u32 },
    /// The server offered DisplayControl, so [`Input::resize`](crate::Input::resize) now has
    /// somewhere to go. Only ever sent on a session configured with [`Connect::resize`], and not
    /// at all by a server that does not implement MS-RDPEDISP — which is the honest signal a
    /// consumer needs before offering the feature to a user.
    ///
    /// It is **not** a promise that the next resize will be honoured: a Windows host sends this
    /// and then ignores layouts for several seconds more, silently. That measurement, and what a
    /// caller has to do about it, are on [`Input::resize`](crate::Input::resize).
    ///
    /// `max_area` is the largest total monitor area the server will accept, in pixels; this crate
    /// asks for one monitor, so it bounds `width * height`.
    ResizeReady { max_monitors: u32, max_area: u64 },
    Cursor(Cursor),
    Clipboard(ClipboardEvent),
    /// The session is over, and the channel is about to close. `Ok(())` is an orderly
    /// disconnection from either side.
    Ended(Result<(), Error>),
}

// ------------------------------------------------------------------ the session handle

/// A live RDP session.
///
/// Dropping this asks the FreeRDP thread to disconnect and waits for it, so a `Session` that has
/// gone out of scope has really stopped — no detached thread still holding a socket open.
pub struct Session {
    input: Input,
    clipboard: Option<Clipboard>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    /// Connect, on a thread of its own.
    ///
    /// Returns immediately: the connection happens on the new thread and its outcome arrives as
    /// the first [`Event`] — [`Event::Connected`] or [`Event::Ended`]. That is worth the slight
    /// awkwardness because connecting takes seconds (DNS, TCP, TLS, CredSSP, licensing, the first
    /// desktop), and a `Session::start` that blocked for them would have to be called from a
    /// thread the caller was willing to lose anyway.
    pub fn start(config: Connect) -> (Self, Receiver<Event>) {
        let (events, receiver) = std::sync::mpsc::channel();
        let clipboard_enabled = config.clipboard;
        let shared = Arc::new(Shared::new());
        let input = Input { shared: Arc::clone(&shared) };

        let thread = std::thread::Builder::new()
            .name("freerdp".into())
            .spawn({
                let shared = Arc::clone(&shared);
                let events = events.clone();
                move || {
                    // A panic here would otherwise take the thread down with no `Ended` event, and
                    // the caller would block on the receiver forever. Reported, then converted
                    // into the disconnection it really is.
                    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        run(config, &shared, &events)
                    }));
                    let result = outcome.unwrap_or_else(|_| {
                        Err(Error::local("the FreeRDP session thread panicked"))
                    });
                    let _ = events.send(Event::Ended(result));
                }
            })
            .expect("cannot spawn the FreeRDP thread");

        let session = Self {
            input: input.clone(),
            clipboard: clipboard_enabled.then(|| Clipboard { input }),
            thread: Some(thread),
        };
        (session, receiver)
    }

    /// Keyboard and mouse.
    pub fn input(&self) -> &Input {
        &self.input
    }

    /// The clipboard, if this session was configured with one.
    pub fn clipboard(&self) -> Option<&Clipboard> {
        self.clipboard.as_ref()
    }

    /// The framebuffer, kept up to date by the session thread.
    pub fn framebuffer(&self) -> &Framebuffer {
        &self.input.shared.framebuffer
    }

    /// Disconnect and wait for the thread. Equivalent to dropping it, and clearer at a call site
    /// that means to end the session rather than to stop using the handle.
    pub fn shutdown(self) {}
}

impl Drop for Session {
    fn drop(&mut self) {
        // **The abort first, and it is not a duplicate of the command below.** `Command::Shutdown`
        // is read by the event loop, and during startup there is no event loop to read it: the
        // thread is inside `freerdp_connect`, in DNS, TCP, TLS or CredSSP, for as long as that
        // takes. A session dropped there would leave this join waiting out the connect timeout —
        // on whichever thread did the dropping, which for an embedder is the one running
        // everything else. This is the same call the event loop makes when it *does* see the
        // command, made from the other side, and it is why the join is bounded on both paths.
        self.input.shared.abort_connect();
        self.input.push(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            // Joined rather than detached. A detached FreeRDP thread outlives the handle that
            // could stop it, holding a socket and a 30 MB framebuffer, and — worse — keeping the
            // session claimed on the server side, so the next connection evicts a session nobody
            // is watching. The thread's own loop is what makes this bounded: it wakes on the
            // command event, sees Shutdown, and returns.
            let _ = thread.join();
        }
    }
}

// ------------------------------------------------------------------ cross-thread plumbing

/// A WinPR `HANDLE` that may cross threads.
///
/// `HANDLE` is `*mut c_void`, so Rust refuses to share it by default, and that default is right
/// for most handles. This one is a manual-reset event, and `SetEvent`/`ResetEvent` on WinPR are
/// implemented over a pipe and a mutex — the same guarantee the Win32 originals give. Sharing it
/// is the entire mechanism by which another thread can wake a blocked
/// `WaitForMultipleObjects`, so there is no version of this design without it.
struct Wake(sys::HANDLE);

// SAFETY: see the note above — a WinPR event object is internally synchronised, and nothing here
// mutates the handle itself after construction.
unsafe impl Send for Wake {}
unsafe impl Sync for Wake {}

impl Drop for Wake {
    fn drop(&mut self) {
        // SAFETY: created by `CreateEventA` below and closed exactly once, here.
        unsafe { sys::CloseHandle(self.0) };
    }
}

/// The live context, or null once there is not one, and whether anybody has asked it to stop.
///
/// Separate from `Wake` because the guarantee is different: a `HANDLE` is internally synchronised
/// and this is a pointer to memory that gets freed, so it is only ever touched under the mutex —
/// which is what stops a caller aborting a connect on a context the session thread is freeing.
///
/// `aborted` is the other half, and it is not belt-and-braces: `Session::start` returns before the
/// thread it spawned has built a context, so a session dropped immediately — which is exactly what
/// a caller that changed its mind does — asks for an abort while there is nothing to abort. The
/// *request* is what is kept here, and `abort_requested` is where an early one is honoured.
/// (Measured: without it, dropping a session connecting to a black hole waited out the whole
/// `connect_timeout` — 60 s of it — on the dropping thread.)
struct ContextPtr {
    ctx: *mut sys::rdpContext,
    aborted: bool,
}

// SAFETY: the pointer is only dereferenced under `Shared::context`'s mutex, and the session thread
// nulls it — under that same mutex — before `ContextGuard` frees the context. So no other thread
// can hold it across the free.
unsafe impl Send for ContextPtr {}

/// What the caller's threads and the FreeRDP thread share.
pub(crate) struct Shared {
    queue: Mutex<VecDeque<Command>>,
    wake: Wake,
    context: Mutex<ContextPtr>,
    pub(crate) framebuffer: Framebuffer,
}

impl Shared {
    fn new() -> Self {
        // Manual reset, initially unsignalled, unnamed. Manual rather than auto because the
        // session thread resets it itself, immediately before draining — see the ordering note
        // in `drain`.
        // SAFETY: a WinPR call with no preconditions; the returned handle is owned by `Wake`.
        let handle = unsafe { sys::CreateEventA(std::ptr::null_mut(), 1, 0, std::ptr::null()) };
        assert!(!handle.is_null(), "CreateEvent failed");
        Self {
            queue: Mutex::new(VecDeque::new()),
            wake: Wake(handle),
            context: Mutex::new(ContextPtr { ctx: std::ptr::null_mut(), aborted: false }),
            framebuffer: Framebuffer::new(),
        }
    }

    /// Publish the context, so a caller's thread can abort a connect that has not finished.
    ///
    /// Called on the session thread as soon as the context exists, and withdrawn again before it
    /// is freed. Between those two points, and only between them, `abort_connect` has something
    /// to signal; before them there is `abort_requested`.
    fn publish_context(&self, ctx: *mut sys::rdpContext) {
        self.context().ctx = ctx;
    }

    /// Whether somebody has asked this session to stop.
    ///
    /// Read on the session thread immediately before `freerdp_connect`, because setting the abort
    /// event earlier than that does nothing: `freerdp_connect` **resets** it as its first act
    /// (`libfreerdp/core/freerdp.c:102`), so an abort that arrives before the connect starts is
    /// erased by the connect it was meant to stop. The window that remains is between this read
    /// and that reset — a few instructions with nothing blocking in them — and an abort landing
    /// inside it is still caught by the queued `Command::Shutdown` once the connect returns.
    fn abort_requested(&self) -> bool {
        self.context().aborted
    }

    /// Withdraw it. Must happen before the context is freed, on the thread that frees it.
    fn withdraw_context(&self) {
        self.context().ctx = std::ptr::null_mut();
    }

    /// Unblock a connect in progress, from any thread.
    ///
    /// Records the request either way, and acts on it if there is a context yet. The lock is held
    /// across the call, and the session thread must take the same lock to withdraw, so the context
    /// cannot be freed while FreeRDP is inside this.
    pub(crate) fn abort_connect(&self) {
        let mut state = self.context();
        state.aborted = true;
        if !state.ctx.is_null() {
            // SAFETY: the context is live for the duration of this lock, and
            // `freerdp_abort_connect_context` is FreeRDP's own cross-thread cancellation — it
            // signals an event rather than touching the connection.
            unsafe { sys::freerdp_abort_connect_context(state.ctx) };
        }
    }

    /// Poison is recovered for the same reason the queue's is: what is under this lock is one
    /// pointer, and refusing to take it would mean a session that can no longer be stopped.
    fn context(&self) -> std::sync::MutexGuard<'_, ContextPtr> {
        self.context.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn push(&self, command: Command) {
        self.lock().push_back(command);
        // SAFETY: `wake` is a live manual-reset event for as long as this `Shared` exists, and
        // `SetEvent` is safe to call from any thread.
        unsafe { sys::SetEvent(self.wake.0) };
    }

    /// Take everything queued.
    ///
    /// **Reset before draining, not after.** A producer pushes and *then* signals, so a push that
    /// lands between the reset and the drain still signals afterwards and wakes the next
    /// iteration. Resetting after the drain would swallow exactly that push, and the symptom
    /// would be one keystroke arriving a keystroke late — occasionally, under load.
    fn drain(&self) -> VecDeque<Command> {
        // SAFETY: as above.
        unsafe { sys::ResetEvent(self.wake.0) };
        std::mem::take(&mut *self.lock())
    }

    /// A poisoned queue lock is recovered rather than propagated: there is nothing under it but a
    /// `VecDeque` of commands, so the worst a panicking producer can leave is a queue missing an
    /// entry, and refusing to lock afterwards would silently stop all input for the session.
    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Command>> {
        self.queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

// ------------------------------------------------------------------ the C context

/// The context FreeRDP allocates for us, with a pointer back to Rust on the end.
///
/// `#[repr(C)]` and `rdpClientContext` **first** are both load-bearing: FreeRDP allocates
/// `ContextSize` bytes and hands out the address as an `rdpContext*`, so every field it knows
/// about has to be at the offset it expects, and everything of ours has to be past the end.
#[repr(C)]
struct WrapperContext {
    common: sys::rdpClientContext,
    bridge: *mut Bridge,
}

/// The Rust half, reachable from every callback.
///
/// Only ever touched on the FreeRDP thread — `Session` holds no reference to it — so there are no
/// locks here beyond the ones inside `Shared`.
pub(crate) struct Bridge {
    events: Sender<Event>,
    shared: Arc<Shared>,
    /// Where redirected sound goes, and `None` on a session that asked for none — in which case
    /// `rdpsnd` was never registered and nothing in `audio.rs` can be reached at all.
    pub(crate) audio: Option<Audio>,
    /// Which `rdpsnd` device is playing. See `audio::open`: the channel is registered twice, as a
    /// static channel and a dynamic one, and only one of them may fill the sink.
    pub(crate) audio_device: *mut sys::rdpsndDevicePlugin,
    cliprdr: *mut sys::CliprdrClientContext,
    /// Whether the capability exchange finished. A format list sent before it is discarded by the
    /// peer, so `advertise` calls that arrive early are held until this is true.
    clipboard_ready: bool,
    pending_advertise: Option<Vec<ClipboardFormat>>,
    disp: *mut sys::DispClientContext,
    /// Whether the server's DisplayControl capabilities have arrived. Until they do, the channel
    /// exists but the server has not said it will listen.
    resize_ready: bool,
    /// The most recent size asked for before the channel was ready — only the most recent, since
    /// a resize supersedes every earlier one rather than queueing behind it.
    pending_resize: Option<(u32, u32, u32)>,
    /// How many `rdpsnd` devices the current connect has minted. The channel is registered on
    /// both transports and each loads a device of its own, in a fixed order: the static
    /// instance's arrives during `freerdp_connect`'s channel bring-up, strictly before the event
    /// loop can process the DVC create a dynamic one rides in on. So the first device of a
    /// connect belongs to the static channel and every later one to `AUDIO_PLAYBACK_DVC` —
    /// which is the whole reason to count. Reset by `reconnect_resize`, whose reconnect runs
    /// the bring-up again.
    pub(crate) audio_devices_seen: u32,
    /// Whether redirected sound has negotiated over the dynamic transport this session. This is
    /// the mark of a server whose audio does not survive a Deactivation-Reactivation — see
    /// `resize` — and it is never unset: the transport is the server's choice, and a server
    /// that chose it once chooses it on every reconnect.
    pub(crate) audio_dynamic_negotiated: bool,
    /// A resize the event loop should perform as a reconnect, and when it was last superseded.
    pending_reconnect: Option<PendingReconnect>,
}

/// A resize waiting out `RECONNECT_DEBOUNCE` before it costs a reconnect.
struct PendingReconnect {
    width: u32,
    height: u32,
    scale_percent: u32,
    since: Instant,
}

/// How long a reconnect-resize waits for a newer size before it runs.
///
/// A monitor layout is cheap enough to send on every viewport report; a reconnect is not, and a
/// window drag reports continuously. Superseding within this window collapses a drag into one
/// reconnect after the hand pauses, at the price of every resize landing this much later. The
/// number is xfreerdp's own resize debounce (`RESIZE_MIN_DELAY_NS` in `xf_disp.c` is 200 ms),
/// rounded up for a strategy whose mistake costs a full reconnect rather than a layout.
const RECONNECT_DEBOUNCE: Duration = Duration::from_millis(300);

impl Bridge {
    fn send(&self, event: Event) -> bool {
        // A closed receiver means the caller dropped the `Receiver` while keeping the `Session`.
        // Not an error — some callers only want the framebuffer — so the session carries on and
        // events go nowhere.
        self.events.send(event).is_ok()
    }
}

/// Recover the bridge from any `rdpContext*` FreeRDP hands a callback.
///
/// # Safety
///
/// `ctx` must be a context this crate created, still alive, and with `bridge` already set — which
/// is true from the moment `run` stores it, and therefore in every callback, since none can fire
/// before `freerdp_connect`.
pub(crate) unsafe fn bridge<'a>(ctx: *mut sys::rdpContext) -> Option<&'a mut Bridge> {
    if ctx.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees the context is one of ours, so it is really a
    // `WrapperContext` and reading `bridge` is in bounds.
    let wrapper = ctx as *mut WrapperContext;
    let ptr = unsafe { (*wrapper).bridge };
    // SAFETY: `ptr` came from `Box::into_raw` in `run` and is not freed until `run` returns,
    // after the event loop and every callback that could reach it.
    (!ptr.is_null()).then(|| unsafe { &mut *ptr })
}

/// Run a callback body, converting a panic into a failure return rather than unwinding into C.
///
/// Unwinding across an `extern "C"` frame is undefined behaviour. `catch_unwind` here is not
/// defensive programming — it is the thing that makes these functions sound to hand to a C
/// library at all. The panic is printed rather than swallowed, and the `FALSE` that comes back
/// ends the session, so it does not disappear.
pub(crate) fn guarded<R>(what: &str, fallback: R, body: impl FnOnce() -> R) -> R {
    std::panic::catch_unwind(AssertUnwindSafe(body)).unwrap_or_else(|_| {
        eprintln!("freerdp: a panic escaped the {what} callback; ending the session");
        fallback
    })
}

// ------------------------------------------------------------------ the thread body

fn run(config: Connect, shared: &Arc<Shared>, events: &Sender<Event>) -> Result<(), Error> {
    let mut entry: sys::RDP_CLIENT_ENTRY_POINTS = unsafe { std::mem::zeroed() };
    entry.Size = std::mem::size_of::<sys::RDP_CLIENT_ENTRY_POINTS>() as u32;
    entry.Version = sys::RDP_CLIENT_INTERFACE_VERSION;
    entry.ContextSize = std::mem::size_of::<WrapperContext>() as u32;
    entry.ClientNew = Some(client_new);
    entry.ClientFree = Some(client_free);

    // SAFETY: `entry` is fully initialised above and only read during this call.
    let ctx = unsafe { sys::freerdp_client_context_new(&entry) };
    if ctx.is_null() {
        return Err(Error::local("freerdp_client_context_new failed"));
    }
    // From here on every exit path must free the context, so the body is wrapped and the cleanup
    // is unconditional.
    let guard = ContextGuard(ctx);
    // Published before anything can block: `apply_settings` and `freerdp_connect` are both ahead
    // of the event loop, and a `Session` dropped while they run has nothing else to stop them.
    shared.publish_context(ctx);

    let bridge = Box::into_raw(Box::new(Bridge {
        events: events.clone(),
        shared: Arc::clone(shared),
        audio: config.audio.clone(),
        audio_device: std::ptr::null_mut(),
        cliprdr: std::ptr::null_mut(),
        clipboard_ready: false,
        pending_advertise: None,
        disp: std::ptr::null_mut(),
        resize_ready: false,
        pending_resize: None,
        audio_devices_seen: 0,
        audio_dynamic_negotiated: false,
        pending_reconnect: None,
    }));
    // SAFETY: `ctx` is the context just created, so it is a `WrapperContext` with room for this.
    unsafe { (*(ctx as *mut WrapperContext)).bridge = bridge };

    let result = run_connected(&config, ctx, shared);

    // Withdrawn before the free, and that order is the whole of what makes `abort_connect` sound:
    // after this returns, no other thread holds the pointer and none can take it again.
    shared.withdraw_context();

    // SAFETY: the event loop has returned and no callback can fire after `freerdp_disconnect`
    // below, so nothing else can reach the bridge. Reclaimed exactly once.
    drop(guard);
    unsafe { drop(Box::from_raw(bridge)) };
    result
}

/// Owns the context between `freerdp_client_context_new` and `freerdp_client_context_free`.
struct ContextGuard(*mut sys::rdpContext);

impl Drop for ContextGuard {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from `freerdp_client_context_new` and is freed exactly once.
        // `freerdp_disconnect` is idempotent and safe on a context that never connected.
        unsafe {
            let instance = (*self.0).instance;
            if !instance.is_null() {
                sys::freerdp_disconnect(instance);
                sys::gdi_free(instance);
            }
            sys::freerdp_client_context_free(self.0);
        }
    }
}

fn run_connected(
    config: &Connect,
    ctx: *mut sys::rdpContext,
    shared: &Arc<Shared>,
) -> Result<(), Error> {
    // SAFETY: `ctx` is live; `instance` and `settings` are set by `freerdp_client_context_new`.
    let instance = unsafe { (*ctx).instance };
    assert!(!instance.is_null(), "a context with no instance");

    // The callbacks are installed *after* `freerdp_client_context_new` rather than inside
    // `ClientNew`, which is where FreeRDP's own sample clients put them. Same effect, one fewer
    // ordering question: nothing FreeRDP does later can overwrite a field set after its
    // constructor has returned.
    // SAFETY: `instance` is live and single-threaded here — nothing is running its event loop.
    unsafe {
        (*instance).PreConnect = Some(pre_connect);
        (*instance).PostConnect = Some(post_connect);
        (*instance).PostDisconnect = Some(post_disconnect);
        (*instance).VerifyCertificateEx = Some(verify_certificate);
        (*instance).VerifyChangedCertificateEx = Some(verify_changed_certificate);
    }

    apply_settings(config, ctx)?;
    subscribe_channels(ctx)?;

    // As late as possible, and only when there is sound to carry. FreeRDP's addin provider is a
    // process global that `freerdp_client_context_new` overwrites, so the window in which another
    // session could take it back is the gap between here and `freerdp_connect` loading the
    // channels — narrow rather than closed, and `audio::install_provider` says why it cannot be
    // closed at all.
    if config.audio.is_some() {
        audio::install_provider()?;
    }

    // Dropped before it ever connected — which is not rare, it is what a caller that changed its
    // mind looks like — so there is nothing to connect *to* any more. Checked here rather than
    // relied on through the abort event, because `freerdp_connect` clears that event on entry;
    // see `Shared::abort_requested`.
    if shared.abort_requested() {
        return Err(Error::local("the session was ended before it connected"));
    }

    // SAFETY: everything the connection needs is configured; this blocks until the RDP handshake
    // finishes or fails.
    if unsafe { sys::freerdp_connect(instance) } == 0 {
        // SAFETY: `ctx` is live and this only reads the recorded error.
        return Err(unsafe { Error::from_context(ctx, "connect") });
    }

    event_loop(ctx)
}

// ------------------------------------------------------------------ settings

fn apply_settings(config: &Connect, ctx: *mut sys::rdpContext) -> Result<(), Error> {
    use sys::FreeRDP_Settings_Keys_Bool as B;
    use sys::FreeRDP_Settings_Keys_String as S;
    use sys::FreeRDP_Settings_Keys_UInt32 as U;

    // SAFETY: `ctx` is live and its settings were allocated by `freerdp_client_context_new`.
    let settings = unsafe { (*ctx).settings };
    assert!(!settings.is_null(), "a context with no settings");

    let set_string = |key, value: &str| -> Result<(), Error> {
        let c = CString::new(value)
            .map_err(|_| Error::local("a connection string contains a NUL byte"))?;
        // SAFETY: `settings` is live, and `freerdp_settings_set_string` copies the string, so the
        // CString may be dropped at the end of this call.
        if unsafe { sys::freerdp_settings_set_string(settings, key, c.as_ptr()) } == 0 {
            return Err(Error::local("a connection string was rejected by FreeRDP"));
        }
        Ok(())
    };
    set_string(S::FreeRDP_ServerHostname, &config.host)?;
    set_string(S::FreeRDP_Username, &config.username)?;
    set_string(S::FreeRDP_Password, &config.password)?;
    if let Some(domain) = &config.domain {
        set_string(S::FreeRDP_Domain, domain)?;
    }

    let uints: &[(_, u32)] = &[
        (U::FreeRDP_ServerPort, u32::from(config.port)),
        (U::FreeRDP_DesktopWidth, config.width),
        (U::FreeRDP_DesktopHeight, config.height),
        // 32-bit colour, because the framebuffer is 32-bit and anything less would be a
        // conversion FreeRDP does on every tile for no benefit to a consumer that encodes.
        (U::FreeRDP_ColorDepth, 32),
        (U::FreeRDP_TcpConnectTimeout, millis(config.connect_timeout)),
        (U::FreeRDP_TcpKeepAliveDelay, seconds(config.keepalive.idle)),
        (U::FreeRDP_TcpKeepAliveInterval, seconds(config.keepalive.interval)),
        (U::FreeRDP_TcpKeepAliveRetries, config.keepalive.retries),
        (U::FreeRDP_TcpAckTimeout, millis(config.keepalive.ack_timeout)),
        // **The link is a LAN, and we say so rather than letting the server measure it.**
        //
        // FreeRDP defaults this to `CONNECTION_TYPE_AUTODETECT` with
        // `NetworkAutoDetect` on, which turns on MS-RDPBCGR's Network Characteristics
        // Detection: the server times round trips and a bandwidth payload, and paces
        // its own updates from the answer. Measured against a Windows 11 host over a
        // tunnelled IPv6 link, that pacing collapsed — a five-second scripted drag
        // arrived in as few as 12 batches, with 95% of its 100 ms buckets carrying no
        // update at all, and whole seconds in which the server sent nothing while the
        // client sent 56 input events. The same drag against xrdp, which implements no
        // auto-detect, painted continuously throughout.
        //
        // A gateway is not a laptop on hotel wifi: it sits beside the hosts it serves
        // and re-encodes for whatever link the *browser* is on, which this end already
        // paces itself. So the server's estimate of *this* hop is both wrong and
        // unhelpful, and the honest thing is to declare the link rather than have it
        // guessed at.
        (U::FreeRDP_ConnectionType, sys::CONNECTION_TYPE_LAN),
        // The offscreen bitmap cache — the other half of the client-side caches enabled with
        // `FreeRDP_BitmapCacheEnabled` below, where the shared reasoning lives. Level 1 is the
        // whole of what MS-RDPEGDI defines; `freerdp_settings_new` sets this key to 0 and sizes
        // the cache anyway (7680 KB, 2000 entries), so the switch is the only thing to set.
        (U::FreeRDP_OffscreenSupportLevel, 1),
    ];
    for (key, value) in uints {
        // SAFETY: `settings` is live; these keys are all UInt32 keys by construction.
        if unsafe { sys::freerdp_settings_set_uint32(settings, *key, *value) } == 0 {
            return Err(Error::local("a numeric setting was rejected by FreeRDP"));
        }
    }

    let (nla, tls, rdp) = match config.security {
        Security::Auto => (true, true, true),
        Security::Nla => (true, false, false),
        Security::Tls => (false, true, false),
        Security::Rdp => (false, false, true),
    };
    let bools: &[(_, bool)] = &[
        (B::FreeRDP_NlaSecurity, nla),
        (B::FreeRDP_TlsSecurity, tls),
        (B::FreeRDP_RdpSecurity, rdp),
        (B::FreeRDP_UseRdpSecurityLayer, config.security == Security::Rdp),
        // With exactly one mode selected there is nothing to negotiate, and leaving negotiation
        // on lets a server talk this client into a mode it was explicitly not given.
        (B::FreeRDP_NegotiateSecurityLayer, config.security == Security::Auto),
        (B::FreeRDP_TcpKeepAlive, true),
        // The other half of the LAN declaration above: with auto-detect on, the
        // `ConnectionType` is only a starting guess the server then overrides with what
        // it measured, so leaving this true would give the throttling back.
        (B::FreeRDP_NetworkAutoDetect, false),
        // **And the third half, which declining auto-detect does not buy on its own.**
        //
        // Saying no to network detection stops this client *advertising*
        // `RNS_UD_CS_SUPPORT_NETCHAR_AUTODETECT`, and stops nothing else: the MCS message
        // channel that the detection PDUs travel on is opened by any of these three
        // settings (`gcc_write_client_message_channel_data`), and both of the others
        // default to on. A Windows host then sends a continuous RTT Measure Request down
        // it anyway — and `autodetect_recv_request_packet` answers a request it was not
        // configured for with `STATE_RUN_FAILED`, which ends the session.
        //
        // Measured, and the reason this is here rather than in a note: with `rdpsnd`
        // loaded, a Windows 11 host began that detection within seconds of the desktop
        // going active and killed the session **five times out of five**; the same host,
        // the same build and the same seconds with no audio channel did it in none of
        // three. Turning sound on is what makes the server start caring how fast the link
        // is, so a client that has declined to answer must also decline to be asked.
        //
        // What is given up: multitransport is UDP side-channels this crate never sets up,
        // and the heartbeat is a liveness signal the TCP keepalives above already provide
        // — with `TcpAckTimeout` bounding an unacknowledged write, which is the case a
        // heartbeat would have caught.
        (B::FreeRDP_SupportMultitransport, false),
        (B::FreeRDP_SupportHeartbeatPdu, false),
        (B::FreeRDP_RedirectClipboard, config.clipboard),
        // Sound. Like the clipboard's key this is both the capability and the channel switch —
        // `freerdp_client_load_addins` maps it to `rdpsnd` — but unlike the clipboard's it is not
        // sufficient on its own, because a channel with no `sys:` argument picks its own backend
        // and this build's list ends in `fake`. `register_audio_channels` below puts the name in.
        (B::FreeRDP_AudioPlayback, config.audio.is_some()),
        // Software GDI: FreeRDP decodes into `gdi->primary_buffer` rather than into a hardware
        // surface. That *is* the headless path — `gdi_init` below has nothing to draw on
        // otherwise — and it is what makes `EndPaint` mean "these pixels are ready".
        (B::FreeRDP_SoftwareGdi, true),
        // Decoding on. `DeactivateClientDecoding` permanently nulls the graphics-pipeline
        // callbacks when `gdi_graphics_pipeline_init` reads it, which is a headless server-side
        // recorder's setting, not a client's.
        (B::FreeRDP_DeactivateClientDecoding, false),
        // **The graphics pipeline is the embedder's `egfx` switch, and RemoteFX rides beside it
        // either way it goes.** The pipeline alone was measured broken: against a Windows 11
        // host, FreeRDP decoded 21 surface commands with no errors and produced exactly one
        // `EndPaint` over a framebuffer that summed to pure black. The missing piece was a codec
        // next to the pipeline flag — guacamole-server ships exactly
        // `SupportGraphicsPipeline` + `RemoteFxCodec` against the same Windows generation — and
        // with both advertised the same e2e that measured black measures 3,090,403 non-zero
        // bytes of 3,145,728, resizes, and disconnects cleanly. Keep the two together: the
        // pipeline without a codec beside it is the black screen again.
        //
        // This pair used to read `!config.resize`, gating the pipeline off every resizable
        // session, because of what each path does to the desktop after a size change — see
        // [`Connect::egfx`] for that trade (blurry Windows text under a graphics reset against
        // a reactivation per resize without it). The gate is now the embedder's, made once in
        // config rather than inferred here.
        (B::FreeRDP_SupportGraphicsPipeline, config.egfx),
        (B::FreeRDP_RemoteFxCodec, config.egfx),
        // Dynamic resize. This one key is also what *loads* the channel: the addin table in
        // `client/common/cmdline.c` maps `FreeRDP_SupportDisplayControl` to `disp`, and
        // `freerdp_client_load_channels` — installed as `LoadChannels` by
        // `freerdp_client_context_new` — walks it. So there is no `add_dynamic_channel` call here,
        // exactly as there is none for the clipboard.
        //
        // Not paired with `FreeRDP_DynamicResolutionUpdate`, and that is deliberate rather than an
        // omission: despite the name, that setting is xfreerdp's own — it means "let the *window*
        // drive the resolution", and `xf_disp.c` is the only thing that reads it. A headless
        // client has no window, and resizes when its embedder says so.
        (B::FreeRDP_SupportDisplayControl, config.resize),
        // Refresh and suppress-output are what make `Input::refresh` and a minimised viewer
        // work; both are cheap capability bits.
        (B::FreeRDP_RefreshRect, true),
        (B::FreeRDP_SuppressOutput, true),
        // A disconnection should surface as an error with a cause in it, not as a silent
        // reconnection loop the caller cannot see.
        (B::FreeRDP_AutoReconnectionEnabled, false),
        (B::FreeRDP_SupportErrorInfoPdu, true),
        // Fast path both ways: fewer bytes per input event and per update, universally
        // supported, and what every real client asks for.
        (B::FreeRDP_FastPathInput, true),
        (B::FreeRDP_FastPathOutput, true),
        // Ask the server to say where its frames end. Both are capability bits a server is free
        // to ignore; where honoured, a TS_FRAME_MARKER order or a surface frame marker brackets
        // each frame and the END goes out as [`Event::Frame`]. A consumer coalescing damage on a
        // timer is reconstructing exactly this boundary, so where the fact is on offer it should
        // not have to guess. (EGFX sessions carry their own frame PDUs and use neither — their
        // boundary comes out of `end_paint`.)
        (B::FreeRDP_FrameMarkerCommandEnabled, true),
        (B::FreeRDP_SurfaceFrameMarkerEnabled, true),
        // The performance flags, at guacamole-server's defaults: wallpaper, theming, full-window
        // drag and menu animations all off. Core derives the wire value from these booleans —
        // `freerdp_performance_flags_make`, called as the extended info packet is written — so
        // the booleans are the only thing to set.
        //
        // Full-window drag was **on** until 2026-08-06, argued from the LAN declaration above:
        // the rubber-band outline a disabled drag leaves reads as the drag having stopped
        // working. The source comparison against guacd overturned that: every position of a
        // dragged window is a full window of damage through decode, diff, encode, socket and
        // paint — one gesture priced at more traffic than minutes of typing, all for pixels
        // that are gone the moment the drag ends. It was the single cheapest damage lever the
        // comparison found, and damage that is never created needs no other optimization
        // downstream. Wallpaper and theming go for the same reason at smaller stakes.
        (B::FreeRDP_DisableWallpaper, true),
        (B::FreeRDP_DisableFullWindowDrag, true),
        (B::FreeRDP_DisableMenuAnims, true),
        (B::FreeRDP_DisableThemes, true),
        // **The legacy path's client-side caches, which FreeRDP 3 ships off.** The bitmap cache
        // lets the server store a tile in this client's memory once and repaint it later with a
        // MEMBLT order naming a cache slot; the offscreen cache (its uint32 switch is above) lets
        // it compose into a client-side surface and blit from that. Both replace retransmitted
        // pixels with a reference to pixels the client already holds — repeated UI, re-exposed
        // window regions, scrolled content — and damage that never crosses the wire needs no
        // other optimization downstream. guacd enables both by default; FreeRDP 3's
        // `freerdp_settings_new` leaves this key zeroed (only a Windows registry hook can set
        // it) and explicitly sets `OffscreenSupportLevel` to 0, so without these two lines every
        // session ran with both caches off.
        //
        // Two settings are genuinely the whole change. The order capability writes
        // `settings->OrderSupport` verbatim, but `freerdp_connect_begin` recomputes that table
        // from these settings at connect time (`freerdp_settings_set_default_order_support`,
        // libfreerdp/core/freerdp.c) — MEMBLT and MEM3BLT follow this boolean. And the client
        // half is core's: `gdi_init` below creates the cache module and registers the offscreen
        // callbacks, so a cache hit surfaces to the embedder as ordinary damage.
        //
        // Set unconditionally, not gated on `egfx`: they are capabilities of the legacy orders
        // channel, which is what an EGFX-off session speaks *and* what an EGFX-on session falls
        // back to when a server declines the pipeline. A server driving EGFX ignores them.
        //
        // **Measured** with `E2E_EGFX=0 WLOG_LEVEL=TRACE cargo run -p freerdp-e2e`, reading
        // FreeRDP's own end-of-session `update_dump_stats`, twice each way against a Windows 11
        // host. Without these two settings that host sent **no drawing orders at all** — 71 and 72
        // raw `RDP_STATS_BITMAP_UPDATE`, every one of them pixels. With them it sent no raw bitmap
        // update at all: 1941 and 1945 MemBlt orders drawn out of 574 and 587 cached bitmaps, so
        // roughly two of every three blits repainted from memory the client already held. Denying
        // a server the cache does not make it send the same picture more cheaply some other way;
        // it makes it stop using orders and push pixels.
        //
        // The same four runs against xrdp, a different server family and the check that this is
        // not one vendor's quirk: 0 orders and 5 raw bitmap updates without, 960 MemBlt orders
        // over 156 and 337 cached bitmaps and no raw update at all with. The shape is identical
        // even though that desktop is far quieter — it is the server's *choice of mechanism* that
        // moves, not the amount it had to draw.
        //
        // A second effect, not looked for, and it belongs to the Windows host alone: with the
        // caches off it marked **no frame boundaries** on this path, and with them on it marked
        // 134 and 136. The ALTSEC frame marker is an order, so it cannot arrive on a connection
        // sending no orders — but orders are only necessary and not sufficient, because xrdp
        // marked nothing in any of its four runs while sending 960 of them. So
        // `FrameMarkerCommandEnabled` above reaches the legacy path only through these settings
        // and only on a server that implements the marker; a consumer still needs its own
        // fallback for the boundaries, and xrdp is the server that proves it.
        //
        // The third cache of this family, glyphs, stays at its `GLYPH_SUPPORT_NONE` default on
        // purpose: guacd forces it off regardless of settings (GUACAMOLE-1191), and this
        // FreeRDP's own settings warning calls a non-NONE level "[experimental] … expect visual
        // artefacts".
        (B::FreeRDP_BitmapCacheEnabled, true),
    ];
    for (key, value) in bools {
        // SAFETY: `settings` is live; these keys are all Bool keys by construction.
        if unsafe { sys::freerdp_settings_set_bool(settings, *key, i32::from(*value) as sys::BOOL) }
            == 0
        {
            return Err(Error::local("a boolean setting was rejected by FreeRDP"));
        }
    }

    if config.audio.is_some() {
        register_audio_channels(settings)?;
    }
    Ok(())
}

/// Register `rdpsnd` by hand, naming this crate's device as its subsystem.
///
/// Both ways round, because which transport carries MS-RDPEA is the *server's* choice: a modern
/// Windows host opens `AUDIO_PLAYBACK_DVC` over drdynvc, an older one or a proxy uses the static
/// `rdpsnd` channel, and a client that registered only one of them would simply get no sound from
/// the other kind of server. **Withholding the dynamic offer was tried and does not make Windows
/// fall back**: against a Windows 11 host with only the static channel registered, no audio was
/// negotiated at all — the server's dynamic create was refused and it never tried the static
/// channel it had also joined. FreeRDP's own `freerdp_client_load_addins` registers both for the
/// same reason; what it cannot do is name a subsystem, and both `freerdp_client_add_*_channel`
/// calls are no-ops when the name is already present, so registering here first is what puts the
/// argument in place rather than fighting with it.
fn register_audio_channels(settings: *mut sys::rdpSettings) -> Result<(), Error> {
    let name = CString::new("rdpsnd").expect("a literal with no NUL");
    let params: [*const c_char; 2] = [name.as_ptr(), audio::SUBSYSTEM_ARG.as_ptr()];

    // SAFETY: `settings` is live, `params` outlives both calls, and each copies what it needs.
    let added = unsafe {
        (
            sys::freerdp_client_add_static_channel(settings, params.len(), params.as_ptr()),
            sys::freerdp_client_add_dynamic_channel(settings, params.len(), params.as_ptr()),
        )
    };
    if added.0 == 0 || added.1 == 0 {
        return Err(Error::local("FreeRDP refused the rdpsnd channel"));
    }
    Ok(())
}

fn seconds(duration: Duration) -> u32 {
    duration.as_secs().min(u32::MAX as u64) as u32
}

fn millis(duration: Duration) -> u32 {
    duration.as_millis().min(u32::MAX as u128) as u32
}

/// Subscribe to the channel events, which is how the clipboard and display-control interfaces
/// arrive — and, on the other side, how this crate learns to stop using them.
fn subscribe_channels(ctx: *mut sys::rdpContext) -> Result<(), Error> {
    // SAFETY: `ctx` is live and `pubSub` is created by `freerdp_client_context_new`.
    let pub_sub = unsafe { (*ctx).pubSub };
    assert!(!pub_sub.is_null(), "a context with no pubSub");

    // `PubSub_Subscribe` rather than `PubSub_SubscribeChannelConnected`: WinPR generates the
    // per-event subscribers as `static inline` functions from a macro, so they exist in no
    // archive and bindgen emits none of them. This is what the inline would have done, and the
    // event name is the string the macro stringifies.
    //
    // SAFETY: variadic, and the handler signature must match the event — which is why each name
    // and the function beside it are chosen together and never separately.
    // Cast to a function *pointer*: a Rust function item is zero-sized and cannot be passed
    // through a variadic. The signature it is cast to is the one `pChannel*EventHandler` names,
    // and nothing checks that for us — which is the whole risk of a variadic subscribe.
    let connected: sys::pChannelConnectedEventHandler = Some(channel_connected);
    let disconnected: sys::pChannelDisconnectedEventHandler = Some(channel_disconnected);
    let status = unsafe {
        (
            sys::PubSub_Subscribe(pub_sub, c"ChannelConnected".as_ptr(), connected),
            sys::PubSub_Subscribe(pub_sub, c"ChannelDisconnected".as_ptr(), disconnected),
        )
    };
    if status.0 < 0 || status.1 < 0 {
        return Err(Error::local("could not subscribe to the channel events"));
    }
    Ok(())
}

// ------------------------------------------------------------------ the event loop

/// FreeRDP's own limit on how many handles it will report.
const MAX_HANDLES: usize = 64;

fn event_loop(ctx: *mut sys::rdpContext) -> Result<(), Error> {
    // SAFETY: `ctx` is live and connected.
    let bridge_ref = unsafe { bridge(ctx) }.ok_or_else(|| Error::local("the bridge is missing"))?;
    let shared = Arc::clone(&bridge_ref.shared);

    loop {
        let mut handles: [sys::HANDLE; MAX_HANDLES] = [std::ptr::null_mut(); MAX_HANDLES];
        // Slot zero is the command queue's event, so a keystroke wakes this as promptly as a
        // packet does. Everything after it is FreeRDP's.
        handles[0] = shared.wake.0;
        // SAFETY: `ctx` is live; the array has room for the count passed.
        let count = unsafe {
            sys::freerdp_get_event_handles(ctx, handles.as_mut_ptr().add(1), (MAX_HANDLES - 1) as u32)
        };
        if count == 0 {
            // FreeRDP has no handles left to wait on, which is how a torn-down connection
            // presents itself here rather than as an error.
            // SAFETY: `ctx` is live.
            return match unsafe { sys::freerdp_get_last_error(ctx) } {
                0 => Ok(()),
                // SAFETY: as above.
                _ => Err(unsafe { Error::from_context(ctx, "the session") }),
            };
        }

        // A pending reconnect-resize turns the infinite wait into its debounce remainder, so
        // the loop wakes to perform it even if the wire and the caller both go quiet.
        // SAFETY: `ctx` is live; the borrow ends before any call back into FreeRDP.
        let timeout = match unsafe { bridge(ctx) }.and_then(|b| b.pending_reconnect.as_ref()) {
            Some(pending) => {
                let left = RECONNECT_DEBOUNCE.saturating_sub(pending.since.elapsed());
                (left.as_millis() as u32).max(1)
            }
            None => sys::INFINITE,
        };
        // SAFETY: `handles` holds `count + 1` valid handles, which is what is passed.
        let status = unsafe {
            sys::WaitForMultipleObjects(count + 1, handles.as_ptr(), 0, timeout)
        };
        if status == sys::WAIT_FAILED {
            return Err(Error::local("WaitForMultipleObjects failed"));
        }

        // The queue first, so an input event queued while FreeRDP was busy goes out before the
        // next round of decoding rather than after it.
        for command in shared.drain() {
            if matches!(command, Command::Shutdown) {
                // SAFETY: `ctx` is live; this unblocks FreeRDP's own waits so the disconnect in
                // `ContextGuard` does not have to wait for a network timeout.
                unsafe { sys::freerdp_abort_connect_context(ctx) };
                return Ok(());
            }
            // SAFETY: `ctx` is live and this is the FreeRDP thread, which is the only thread
            // permitted to call into the instance.
            unsafe { execute(ctx, command) };
        }

        // A reconnect-resize runs between iterations, on fresh state, once its debounce has
        // passed with no newer size superseding it. `execute` cannot run it in place: tearing
        // the connection down mid-drain would leave the rest of this iteration checking
        // handles that no longer exist.
        // SAFETY: `ctx` is live and this is the FreeRDP thread.
        let due = unsafe { bridge(ctx) }.and_then(|bridge| {
            let ready = bridge
                .pending_reconnect
                .as_ref()
                .is_some_and(|pending| pending.since.elapsed() >= RECONNECT_DEBOUNCE);
            if ready { bridge.pending_reconnect.take() } else { None }
        });
        if let Some(pending) = due {
            // SAFETY: as above.
            if !unsafe {
                reconnect_resize(ctx, pending.width, pending.height, pending.scale_percent)
            } {
                // The connection is already down; there is nothing to resume.
                return Err(Error::local("a resize-by-reconnect failed"));
            }
            continue;
        }

        // SAFETY: `ctx` is live. This is where every callback in this file is called from.
        if unsafe { sys::freerdp_check_event_handles(ctx) } == 0 {
            // SAFETY: as above.
            return match unsafe { sys::freerdp_get_last_error(ctx) } {
                0 => Ok(()),
                // SAFETY: as above.
                _ => Err(unsafe { Error::from_context(ctx, "the session") }),
            };
        }

        // SAFETY: `ctx` is live.
        if unsafe { sys::freerdp_shall_disconnect_context(ctx) } != 0 {
            return Ok(());
        }
    }
}

/// Perform one queued command. Runs on the FreeRDP thread only.
///
/// # Safety
///
/// `ctx` must be a live, connected context, and this must be the thread running its event loop.
unsafe fn execute(ctx: *mut sys::rdpContext, command: Command) {
    // SAFETY: the caller guarantees a live context; `input` is set by the connection.
    let input = unsafe { (*ctx).input };
    match command {
        // Input failures are dropped rather than propagated, and that is a considered choice: a
        // send that fails is a connection that is already going away, and the event loop's next
        // `freerdp_check_event_handles` will report it with a real cause. Turning it into an
        // error here would race that and produce a worse message.
        Command::Mouse { flags, x, y } => {
            // SAFETY: live input, plain integers.
            unsafe { sys::freerdp_input_send_mouse_event(input, flags, x, y) };
        }
        Command::ExtendedMouse { flags, x, y } => {
            // SAFETY: as above.
            unsafe { sys::freerdp_input_send_extended_mouse_event(input, flags, x, y) };
        }
        Command::Key { down, scancode } => {
            // SAFETY: as above. `repeat` is FALSE — an autorepeating key is a stream of presses
            // from the caller, not a flag, because only the caller knows its own repeat policy.
            unsafe {
                sys::freerdp_input_send_keyboard_event_ex(
                    input,
                    i32::from(down) as sys::BOOL,
                    0,
                    scancode,
                )
            };
        }
        Command::Unicode { down, code } => {
            let flags =
                if down { sys::KBD_FLAGS_DOWN } else { sys::KBD_FLAGS_RELEASE } as u16;
            // SAFETY: as above.
            unsafe { sys::freerdp_input_send_unicode_keyboard_event(input, flags, code) };
        }
        Command::LockKeys { flags } => {
            // SAFETY: as above.
            unsafe { sys::freerdp_input_send_synchronize_event(input, flags) };
        }
        Command::Refresh => {
            // SAFETY: `ctx` is live and connected, so `gdi` exists.
            unsafe { send_refresh(ctx) };
        }
        Command::Resize { width, height, scale_percent } => {
            // SAFETY: as above.
            unsafe { resize(ctx, width, height, scale_percent) };
        }
        Command::ClipboardAdvertise(formats) => {
            // SAFETY: as above.
            unsafe { clipboard_advertise(ctx, formats) };
        }
        Command::ClipboardRequest(format) => {
            // SAFETY: as above.
            unsafe { clipboard_request(ctx, format) };
        }
        Command::ClipboardRespond { format, data } => {
            // SAFETY: as above.
            unsafe { clipboard_respond(ctx, format, data) };
        }
        // Handled by the caller, which returns before reaching here.
        Command::Shutdown => unreachable!("Shutdown is handled by the event loop"),
    }
}

/// Ask the server to resend the whole desktop.
///
/// # Safety
///
/// `ctx` must be live and connected.
unsafe fn send_refresh(ctx: *mut sys::rdpContext) {
    // SAFETY: the caller guarantees a live, connected context.
    unsafe {
        let update = (*ctx).update;
        let gdi = (*ctx).gdi;
        if update.is_null() || gdi.is_null() {
            return;
        }
        let Some(refresh) = (*update).RefreshRect else { return };
        // One rectangle covering everything. `RefreshRect` takes an array so that a client can
        // ask for the parts of a window that were just uncovered; a headless consumer that asks
        // at all wants all of it.
        let rect = sys::RECTANGLE_16 {
            left: 0,
            top: 0,
            right: (*gdi).width.max(0) as u16,
            bottom: (*gdi).height.max(0) as u16,
        };
        refresh(ctx, 1, &rect);
    }
}

/// Route a resize to whichever mechanism this session survives.
///
/// The default is MS-RDPEDISP: a monitor layout, answered by a graphics reset over EGFX and by
/// a Deactivation-Reactivation Sequence on the legacy path, either way with the session, its
/// channels and its sound intact. One server breaks that bargain: a Windows host drives
/// MS-RDPEA over `AUDIO_PLAYBACK_DVC`, and its audio redirector does not survive the
/// reactivation a legacy resize costs — measured mid-playback against a Windows 11 host, five
/// resizes of six left the channel open and mute, the last wave landing within a second of the
/// reactivation, with no close, no re-announced formats, and nothing in MS-RDPEA for a client
/// to restart it with. Every remedy short of reconnecting was tried against that host and
/// measured dead: a client-side channel close is never answered with a new create, withholding
/// wave confirms quiesces nothing, and a server whose dynamic create is refused never falls
/// back to the static channel it also joined.
///
/// So a session bearing that server's mark — no graphics pipeline, and sound negotiated on the
/// dynamic transport — resizes the way Guacamole's `resize-method: reconnect` does: the
/// connection comes down and back up at the new size, renegotiating the channels and the sound
/// with it, in about 800 ms against the same host. Everyone else keeps the layout, which is
/// cheaper and already survives: xrdp's static-channel audio rides out its reactivation, a
/// session with no sound has nothing to lose to one, and an EGFX resize is not a reactivation
/// at all.
///
/// # Safety
///
/// `ctx` must be live and connected.
unsafe fn resize(ctx: *mut sys::rdpContext, width: u32, height: u32, scale_percent: u32) {
    // SAFETY: the caller guarantees the context.
    let Some(bridge) = (unsafe { bridge(ctx) }) else { return };
    // A request for the desktop already on screen does nothing, on either strategy. This is
    // the rule a Windows host applies to monitor layouts on its own — and the reconnect
    // strategy has to apply it for itself, because layouts are never acknowledged and an
    // embedder therefore re-asks on a schedule: the retry that raced the first reconnect
    // would otherwise land after it, equal to the new desktop, and buy a second reconnect
    // for nothing. The scale is part of the comparison — `reconnect_resize` writes it back
    // to settings, so a genuine density change always differs here.
    // SAFETY: settings are live on a live context.
    let same = unsafe {
        use sys::FreeRDP_Settings_Keys_UInt32 as U;
        let settings = (*ctx).settings;
        width == sys::freerdp_settings_get_uint32(settings, U::FreeRDP_DesktopWidth)
            && height == sys::freerdp_settings_get_uint32(settings, U::FreeRDP_DesktopHeight)
            && scale_percent
                == sys::freerdp_settings_get_uint32(settings, U::FreeRDP_DesktopScaleFactor)
    };
    if same {
        return;
    }
    // SAFETY: as above.
    let egfx = unsafe {
        sys::freerdp_settings_get_bool(
            (*ctx).settings,
            sys::FreeRDP_Settings_Keys_Bool::FreeRDP_SupportGraphicsPipeline,
        )
    } != 0;
    if !egfx && bridge.audio_dynamic_negotiated {
        // Only superseded, never sent directly: the event loop performs it once
        // `RECONNECT_DEBOUNCE` passes without a newer size, so a window drag costs one
        // reconnect rather than one per report.
        bridge.pending_reconnect =
            Some(PendingReconnect { width, height, scale_percent, since: Instant::now() });
        return;
    }
    // SAFETY: as above.
    unsafe { request_resize(ctx, width, height, scale_percent) };
}

/// The resize that reconnects: down, the new size into settings, up, and then by hand the same
/// bookkeeping a `DesktopResize` would have driven.
///
/// Three details are load-bearing. **The GDI survives the reconnect** — `freerdp_reconnect`
/// re-runs the connect sequence but never `PostConnect`, so freeing the GDI here leaves
/// `context->cache` null and the first pointer update after the reconnect dereferences it.
/// **No `DesktopResize` fires on the way back up**, because the server activates at exactly the
/// size settings already hold — the `desktop_resize` call below is that event, run by hand, and
/// it reads the size back out of settings so a server that activated at some *other* size is
/// still followed rather than fought. And **the device count restarts**, because the reconnect
/// runs the channel bring-up again and the next `rdpsnd` device minted belongs to the static
/// channel.
///
/// # Safety
///
/// `ctx` must be live and connected, and this must be the FreeRDP thread, between event-loop
/// iterations — nothing else may be inside the instance while it reconnects.
unsafe fn reconnect_resize(
    ctx: *mut sys::rdpContext,
    width: u32,
    height: u32,
    scale_percent: u32,
) -> bool {
    unsafe {
        let instance = (*ctx).instance;
        // One line on stderr, because this is the expensive kind of resize and the embedder's
        // logs should say so rather than leave an ~800 ms freeze unexplained.
        eprintln!(
            "freerdp: resizing to {width}x{height} by reconnect — this session's sound would \
             not survive a reactivation"
        );
        if sys::freerdp_disconnect_before_reconnect_context(ctx) == 0 {
            eprintln!("freerdp: could not take the session down for a resize-by-reconnect");
            return false;
        }
        if let Some(bridge) = bridge(ctx) {
            bridge.audio_devices_seen = 0;
        }
        use sys::FreeRDP_Settings_Keys_UInt32 as U;
        let settings = (*ctx).settings;
        // The density rides the connect itself: DesktopScaleFactor is the field a monitor
        // layout would have carried, and DeviceScaleFactor stays the 100 the layout builder
        // uses. `Input::resize` has already clamped the scale to what the wire allows.
        let sizes = [
            (U::FreeRDP_DesktopWidth, width),
            (U::FreeRDP_DesktopHeight, height),
            (U::FreeRDP_DesktopScaleFactor, scale_percent),
            (U::FreeRDP_DeviceScaleFactor, 100),
        ];
        for (key, value) in sizes {
            if sys::freerdp_settings_set_uint32(settings, key, value) == 0 {
                eprintln!("freerdp: a resize-by-reconnect setting was rejected");
                return false;
            }
        }
        if sys::freerdp_reconnect(instance) == 0 {
            eprintln!("freerdp: the reconnect half of a resize-by-reconnect failed");
            return false;
        }
        desktop_resize(ctx) != 0
    }
}

/// Ask the server for a new desktop size, or hold the request until the channel is ready.
///
/// # Safety
///
/// `ctx` must be live and connected.
unsafe fn request_resize(
    ctx: *mut sys::rdpContext,
    width: u32,
    height: u32,
    scale_percent: u32,
) {
    // SAFETY: the caller guarantees the context.
    let Some(bridge) = (unsafe { bridge(ctx) }) else { return };
    if bridge.disp.is_null() || !bridge.resize_ready {
        // Held rather than sent, and only the most recent one. A caller that sizes its viewport
        // before the channel finishes coming up is doing the normal thing, and dropping that
        // first request would leave the desktop at the size `Connect` asked for with no way to
        // tell why. A session that never gets the channel drops it, which is what
        // `Event::ResizeReady` is for.
        bridge.pending_resize = Some((width, height, scale_percent));
        return;
    }
    // SAFETY: `disp` was stored by `channel_connected` and cleared by `channel_disconnected`, so
    // a non-null one here is live.
    unsafe { send_monitor_layout(bridge.disp, width, height, scale_percent) };
}

/// Build a one-monitor layout and send it.
///
/// # Safety
///
/// `disp` must be a live display-control context whose channel is open.
unsafe fn send_monitor_layout(
    disp: *mut sys::DispClientContext,
    width: u32,
    height: u32,
    scale_percent: u32,
) {
    // SAFETY: the caller guarantees the context; the send is synchronous and copies the layout.
    unsafe {
        let Some(send) = (*disp).SendMonitorLayout else { return };
        let mut layout = monitor_layout(width, height, scale_percent);
        let status = send(disp, 1, &mut layout);
        if status != sys::CHANNEL_RC_OK {
            eprintln!(
                "freerdp: a monitor layout of {width}x{height} at {scale_percent}% was rejected \
                 ({status})"
            );
        }
    }
}

/// One monitor, at the size asked for.
///
/// `PhysicalWidth` and `PhysicalHeight` are **zero**, which MS-RDPEDISP 2.2.2.2.1 defines as
/// "unknown" — the alternative being to invent them, as FreeRDP's X11 client does by assuming 75
/// DPI. A headless client has no display and no honest answer, and the field only feeds the
/// remote's DPI heuristics. Both xrdp and a Windows 11 host accept a layout with them zeroed.
///
/// Neither scale factor is ever zero, and that is not the same decision. The spec constrains them
/// — `DesktopScaleFactor` to 100..=500 and `DeviceScaleFactor` to 100, 140 or 180 — so zero is out
/// of range in a way an unknown physical size is not, and a server that finds *either* out of
/// range must ignore **both**. That is why an invented density costs the whole scaling of a
/// desktop rather than part of it.
///
/// `DesktopScaleFactor` is the caller's, clamped before it gets here. `DeviceScaleFactor` is
/// pinned to 100 whatever it is — what FreeRDP's own SDL clients send for a 2x display, and what
/// IronRDP pins it to unconditionally. The two describe different things: how large the remote
/// should draw its UI, and what a physical device reports about itself. A headless client has an
/// answer for the first and none at all for the second.
fn monitor_layout(
    width: u32,
    height: u32,
    scale_percent: u32,
) -> sys::DISPLAY_CONTROL_MONITOR_LAYOUT {
    sys::DISPLAY_CONTROL_MONITOR_LAYOUT {
        Flags: sys::DISPLAY_CONTROL_MONITOR_PRIMARY,
        Left: 0,
        Top: 0,
        Width: width,
        Height: height,
        PhysicalWidth: 0,
        PhysicalHeight: 0,
        Orientation: 0,
        DesktopScaleFactor: scale_percent,
        DeviceScaleFactor: 100,
    }
}

// ------------------------------------------------------------------ connection callbacks

/// `ClientNew` — required to exist, with nothing to do.
///
/// The instance callbacks are installed in `run_connected` instead, after
/// `freerdp_client_context_new` has returned, so that nothing FreeRDP does during construction
/// can overwrite them. This one returns TRUE so construction succeeds.
unsafe extern "C" fn client_new(
    _instance: *mut sys::freerdp,
    _context: *mut sys::rdpContext,
) -> sys::BOOL {
    1
}

unsafe extern "C" fn client_free(_instance: *mut sys::freerdp, _context: *mut sys::rdpContext) {}

/// `PreConnect` — also nothing, and deliberately.
///
/// FreeRDP's sample clients configure settings here because they parse a command line first. This
/// crate has every setting before `freerdp_connect` is called, which is strictly earlier, so
/// there is nothing left for this hook to do. It exists because `freerdp_connect` expects one.
unsafe extern "C" fn pre_connect(_instance: *mut sys::freerdp) -> sys::BOOL {
    1
}

/// `PostConnect` — the desktop size is settled, so this is where the framebuffer comes from.
unsafe extern "C" fn post_connect(instance: *mut sys::freerdp) -> sys::BOOL {
    guarded("PostConnect", 0, || {
        // SAFETY: FreeRDP passes a live instance whose context is ours.
        let ctx = unsafe { (*instance).context };
        let Some(bridge) = (unsafe { bridge(ctx) }) else { return 0 };

        // `RGBX32`, so the framebuffer is R,G,B,unused in memory — see `Frame`. This allocates
        // `gdi->primary_buffer` and installs the software drawing path.
        // SAFETY: called from PostConnect, which is where gdi_init belongs.
        if unsafe { sys::gdi_init(instance, sys::pixel_format::RGBX32) } == 0 {
            eprintln!("freerdp: gdi_init failed");
            return 0;
        }

        // SAFETY: `gdi_init` just succeeded, so `gdi` and `update` are live.
        let (width, height) = unsafe {
            let gdi = (*ctx).gdi;
            let update = (*ctx).update;
            (*update).BeginPaint = Some(begin_paint);
            (*update).EndPaint = Some(end_paint);
            (*update).DesktopResize = Some(desktop_resize);
            // The frame boundaries `apply_settings` asked for, surfaced as `Event::Frame`.
            // `SurfaceFrameAcknowledge` is deliberately left alone: it is core's *sender*
            // (`update_send_frame_acknowledge`), which `surface_frame_marker` calls.
            (*update).SurfaceFrameMarker = Some(surface_frame_marker);
            (*(*update).altsec).FrameMarker = Some(frame_marker);
            ((*gdi).width.max(0) as u32, (*gdi).height.max(0) as u32)
        };

        bridge.shared.framebuffer.resize(width, height);

        // The pointer prototype. FreeRDP copies it and allocates `size` bytes for each cursor it
        // receives, which is what gives `WrapperPointer` room for its converted image.
        let prototype = sys::rdpPointer {
            size: std::mem::size_of::<WrapperPointer>(),
            New: Some(pointer_new),
            Free: Some(pointer_free),
            Set: Some(pointer_set),
            SetNull: Some(pointer_set_null),
            SetDefault: Some(pointer_set_default),
            SetPosition: Some(pointer_set_position),
            // SAFETY: the remaining fields are per-cursor state FreeRDP fills in; zero is the
            // documented starting value for a prototype.
            ..unsafe { std::mem::zeroed() }
        };
        // SAFETY: `graphics` is live after a successful connect, and the prototype is copied.
        unsafe { sys::graphics_register_pointer((*ctx).graphics, &prototype) };

        bridge.send(Event::Connected { width, height });
        1
    })
}

unsafe extern "C" fn post_disconnect(_instance: *mut sys::freerdp) {}

/// Accept any certificate, for this session only.
///
/// **2, not 1.** FreeRDP documents 1 as "accept and store permanently" and 2 as "accept for this
/// session only". 1 writes a fingerprint into `~/.config/freerdp/known_hosts2`, which for a
/// long-running daemon means touching the filesystem on every connect and — much worse — refusing
/// a host that legitimately regenerated its self-signed certificate, through a file nobody knew
/// existed. A returned 0 means reject.
///
/// Accepting anything is a real posture and it should be read as one. It is defensible under
/// `Security::Nla`, where CredSSP binds the server's TLS public key into the credential exchange
/// so an interceptor cannot replay them; it is **not** defensible under `Security::Tls`, where
/// the credentials go to whoever answered. A caller that needs to pin a certificate needs this
/// crate to grow a verification hook, not a wrapper around this function.
unsafe extern "C" fn verify_certificate(
    _instance: *mut sys::freerdp,
    _host: *const c_char,
    _port: u16,
    _common_name: *const c_char,
    _subject: *const c_char,
    _issuer: *const c_char,
    _fingerprint: *const c_char,
    _flags: u32,
) -> u32 {
    2
}

/// The same, for a certificate that differs from a stored fingerprint.
///
/// Both hooks must be installed. FreeRDP's default for a missing one is its interactive CLI
/// prompt, which in a daemon is a connection that blocks forever on a question nobody can see.
#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn verify_changed_certificate(
    _instance: *mut sys::freerdp,
    _host: *const c_char,
    _port: u16,
    _common_name: *const c_char,
    _subject: *const c_char,
    _issuer: *const c_char,
    _new_fingerprint: *const c_char,
    _old_subject: *const c_char,
    _old_issuer: *const c_char,
    _old_fingerprint: *const c_char,
    _flags: u32,
) -> u32 {
    2
}

// ------------------------------------------------------------------ paint callbacks

/// Clear the invalid region so `EndPaint` sees only what this frame touched.
unsafe extern "C" fn begin_paint(ctx: *mut sys::rdpContext) -> sys::BOOL {
    guarded("BeginPaint", 0, || {
        // SAFETY: FreeRDP only calls this on a connected context, so the GDI chain is live.
        unsafe {
            let Some(invalid) = invalid_region(ctx) else { return 0 };
            (*invalid).null = 1;
            let hwnd = (*(*(*(*ctx).gdi).primary).hdc).hwnd;
            (*hwnd).ninvalid = 0;
        }
        1
    })
}

/// Copy the damaged rectangle out and report it.
unsafe extern "C" fn end_paint(ctx: *mut sys::rdpContext) -> sys::BOOL {
    guarded("EndPaint", 0, || {
        // SAFETY: as `begin_paint`.
        let Some(bridge) = (unsafe { bridge(ctx) }) else { return 0 };
        // SAFETY: as above.
        unsafe {
            let Some(invalid) = invalid_region(ctx) else { return 0 };
            // An `EndPaint` with nothing in it is normal, not an error: FreeRDP 3.8 and later
            // emit one during display updates, and a surface command whose invalid region came
            // out empty produces one too. Distinct from "no EndPaint at all", which is a fault.
            if (*invalid).null != 0 {
                return 1;
            }
            let rect = Rect {
                x: (*invalid).x.max(0) as u32,
                y: (*invalid).y.max(0) as u32,
                width: (*invalid).w.max(0) as u32,
                height: (*invalid).h.max(0) as u32,
            };
            (*invalid).null = 1;
            let hwnd = (*(*(*(*ctx).gdi).primary).hdc).hwnd;
            (*hwnd).ninvalid = 0;

            if rect.is_empty() {
                return 1;
            }
            let gdi = (*ctx).gdi;
            // SAFETY: `primary_buffer` is the software GDI's own framebuffer, at `stride` bytes
            // per row in the format `gdi_init` was given, and this is the one moment it is not
            // being written to — FreeRDP is inside this callback.
            bridge.shared.framebuffer.blit(
                (*gdi).primary_buffer,
                (*gdi).stride as usize,
                rect,
            );
            bridge.send(Event::Paint(rect));
            // On EGFX this paint *is* a frame. The pipeline flushes its surfaces to the GDI
            // once per frame PDU — `gdi_EndFrame` calls `gdi_OutputUpdate`, which brackets the
            // frame's whole invalid region in one Begin/EndPaint — and it does so before
            // clearing `inGfxFrame`, so the flag here means "that flush and nothing else".
            // The legacy frame markers never arrive on a session whose graphics ride the
            // pipeline, which is why this is the pipeline's boundary and not a duplicate.
            if (*gdi).inGfxFrame != 0 {
                bridge.send(Event::Frame);
            }
        }
        1
    })
}

/// TS_FRAME_MARKER, the legacy path's frame boundary. Only the END matters: the START clears
/// nothing and promises nothing, and guacamole-server ignores it the same way.
unsafe extern "C" fn frame_marker(
    ctx: *mut sys::rdpContext,
    marker: *const sys::FRAME_MARKER_ORDER,
) -> sys::BOOL {
    guarded("FrameMarker", 0, || {
        let Some(bridge) = (unsafe { bridge(ctx) }) else { return 0 };
        // SAFETY: FreeRDP passes a marker it just parsed, on a live context.
        if unsafe { (*marker).action } == sys::FRAME_END {
            bridge.send(Event::Frame);
        }
        1
    })
}

/// The surface-command flavour of the same boundary — and the one that owes the server an
/// answer. `FrameAcknowledge` (FreeRDP's default is 2) is the client's advertised in-flight
/// window; a server that negotiated it stops sending after that many unacknowledged frames,
/// so the acknowledgment is what keeps the stream moving, not a courtesy.
unsafe extern "C" fn surface_frame_marker(
    ctx: *mut sys::rdpContext,
    marker: *const sys::SURFACE_FRAME_MARKER,
) -> sys::BOOL {
    guarded("SurfaceFrameMarker", 0, || {
        let Some(bridge) = (unsafe { bridge(ctx) }) else { return 0 };
        // SAFETY: as `frame_marker`; `update` and `settings` are live on a connected context.
        unsafe {
            if (*marker).frameAction != sys::SURFCMD_FRAMEACTION_SURFACECMD_FRAMEACTION_END {
                return 1;
            }
            if sys::freerdp_settings_get_uint32(
                (*ctx).settings,
                sys::FreeRDP_Settings_Keys_UInt32::FreeRDP_FrameAcknowledge,
            ) > 0
            {
                if let Some(ack) = (*(*ctx).update).SurfaceFrameAcknowledge {
                    ack(ctx, (*marker).frameId);
                }
            }
        }
        bridge.send(Event::Frame);
        1
    })
}

/// The server changed the desktop size.
unsafe extern "C" fn desktop_resize(ctx: *mut sys::rdpContext) -> sys::BOOL {
    guarded("DesktopResize", 0, || {
        // SAFETY: FreeRDP calls this on a live connected context.
        let Some(bridge) = (unsafe { bridge(ctx) }) else { return 0 };
        // SAFETY: as above; the new size is already in settings when this fires.
        let (width, height) = unsafe {
            let settings = (*ctx).settings;
            (
                sys::freerdp_settings_get_uint32(
                    settings,
                    sys::FreeRDP_Settings_Keys_UInt32::FreeRDP_DesktopWidth,
                ),
                sys::freerdp_settings_get_uint32(
                    settings,
                    sys::FreeRDP_Settings_Keys_UInt32::FreeRDP_DesktopHeight,
                ),
            )
        };
        // The decoder contexts first, because FreeRDP forgets them: `rdp_client_reset_codecs`
        // sizes them to the desktop exactly once, at connect, and `gdi_resize` below touches
        // only the framebuffer. The planar decoder refuses any bitmap wider or taller than the
        // size it was prepared with, so after a resize to a *larger* desktop the first
        // full-width strip a server sends fails to decompress and `update_recv` ends the
        // session — measured against xorgxrdp, which sends exactly such strips, as a session
        // that survived every shrink and died on the first grow. The graphics pipeline resets
        // codecs on its own resize path (`gdi/gfx.c`), which is why clients running EGFX never
        // see this; the legacy bitmap path just forgot.
        //
        // SAFETY: context, settings and codecs are live on a connected context; the flags
        // getter reads the same settings this callback already reads.
        unsafe {
            let flags = sys::freerdp_settings_get_codecs_flags((*ctx).settings);
            if sys::freerdp_client_codecs_reset((*ctx).codecs, flags, width, height) == 0 {
                eprintln!("freerdp: codecs_reset to {width}x{height} failed");
                return 0;
            }
        }
        // FreeRDP's buffer next, then ours, then the event. In that order: a `Resize` the caller
        // acts on before the framebuffer has grown would read a stale size.
        // SAFETY: `gdi` is live on a connected context.
        if unsafe { sys::gdi_resize((*ctx).gdi, width, height) } == 0 {
            eprintln!("freerdp: gdi_resize to {width}x{height} failed");
            return 0;
        }
        bridge.shared.framebuffer.resize(width, height);
        bridge.send(Event::Resize { width, height });
        1
    })
}

/// `gdi->primary->hdc->hwnd->invalid`, with every step of the chain checked.
///
/// # Safety
///
/// `ctx` must be a live context whose `gdi_init` succeeded.
unsafe fn invalid_region(ctx: *mut sys::rdpContext) -> Option<sys::HGDI_RGN> {
    // SAFETY: the caller guarantees the context. Each dereference is guarded, because a paint
    // callback firing before or after `gdi_init` would otherwise be a null dereference inside C.
    unsafe {
        let gdi = (*ctx).gdi;
        if gdi.is_null() || (*gdi).primary.is_null() {
            return None;
        }
        let hdc = (*(*gdi).primary).hdc;
        if hdc.is_null() || (*hdc).hwnd.is_null() {
            return None;
        }
        let invalid = (*(*hdc).hwnd).invalid;
        (!invalid.is_null()).then_some(invalid)
    }
}

// ------------------------------------------------------------------ pointer callbacks

/// One cursor, with room for the RGBA this crate converted it into.
///
/// `rdpPointer` first, for the same reason `WrapperContext` puts `rdpClientContext` first:
/// FreeRDP allocates `size` bytes and treats the address as an `rdpPointer*`.
#[repr(C)]
struct WrapperPointer {
    base: sys::rdpPointer,
    /// A `Box<CursorImage>` leaked here for FreeRDP to hold, reclaimed in `pointer_free`. Null
    /// when the cursor could not be decoded.
    image: *mut pointer::CursorImage,
}

unsafe extern "C" fn pointer_new(
    _ctx: *mut sys::rdpContext,
    ptr: *mut sys::rdpPointer,
) -> sys::BOOL {
    guarded("Pointer_New", 0, || {
        // SAFETY: FreeRDP allocated `size_of::<WrapperPointer>()` bytes here, so the `image`
        // field is in bounds, and it has just filled in the mask fields.
        unsafe {
            let wrapper = ptr as *mut WrapperPointer;
            (*wrapper).image = match pointer::to_rgba(ptr) {
                Some(image) => Box::into_raw(Box::new(image)),
                // A cursor this crate will not decode is not a session failure: the previous
                // cursor stays on screen, which is what every RDP client does with one it cannot
                // render. TRUE, so FreeRDP keeps the pointer object and its own bookkeeping.
                None => std::ptr::null_mut(),
            };
        }
        1
    })
}

unsafe extern "C" fn pointer_free(_ctx: *mut sys::rdpContext, ptr: *mut sys::rdpPointer) {
    guarded("Pointer_Free", (), || {
        // SAFETY: as `pointer_new`. The box was created there and is reclaimed exactly once here;
        // the null is written back so a double free would be a null read instead.
        unsafe {
            let wrapper = ptr as *mut WrapperPointer;
            let image = std::mem::replace(&mut (*wrapper).image, std::ptr::null_mut());
            if !image.is_null() {
                drop(Box::from_raw(image));
            }
        }
    })
}

unsafe extern "C" fn pointer_set(
    ctx: *mut sys::rdpContext,
    ptr: *mut sys::rdpPointer,
) -> sys::BOOL {
    guarded("Pointer_Set", 0, || {
        // SAFETY: FreeRDP passes a context of ours and a pointer it allocated.
        let Some(bridge) = (unsafe { bridge(ctx) }) else { return 0 };
        // SAFETY: as `pointer_new`.
        let image = unsafe { (*(ptr as *mut WrapperPointer)).image };
        if image.is_null() {
            // Decoding failed in `pointer_new`; leave whatever is on screen.
            return 1;
        }
        // SAFETY: non-null means it came from `Box::into_raw` in `pointer_new` and has not been
        // freed — `pointer_free` is the only thing that frees it, and FreeRDP does not call it
        // while the cursor is current.
        let image = unsafe { &*image };
        bridge.send(Event::Cursor(Cursor::Image(image.clone())));
        1
    })
}

unsafe extern "C" fn pointer_set_null(ctx: *mut sys::rdpContext) -> sys::BOOL {
    guarded("Pointer_SetNull", 0, || {
        // SAFETY: FreeRDP passes a context of ours.
        let Some(bridge) = (unsafe { bridge(ctx) }) else { return 0 };
        bridge.send(Event::Cursor(Cursor::Hidden));
        1
    })
}

unsafe extern "C" fn pointer_set_default(ctx: *mut sys::rdpContext) -> sys::BOOL {
    guarded("Pointer_SetDefault", 0, || {
        // SAFETY: as above.
        let Some(bridge) = (unsafe { bridge(ctx) }) else { return 0 };
        bridge.send(Event::Cursor(Cursor::Default));
        1
    })
}

/// The server moved the pointer itself.
///
/// Reported to FreeRDP as handled and otherwise ignored. A headless consumer has no local pointer
/// to warp, and a client that echoed this back as a mouse event would fight the user for control
/// of the cursor.
unsafe extern "C" fn pointer_set_position(
    _ctx: *mut sys::rdpContext,
    _x: u32,
    _y: u32,
) -> sys::BOOL {
    1
}

// ------------------------------------------------------------------ clipboard

/// The channel-connected handler, subscribed in `subscribe_channels`.
///
/// Only `cliprdr` and `disp` are claimed here. Everything else falls through to
/// `freerdp_client_OnChannelConnectedEventHandler`, which is what binds `rdpgfx` to the GDI and
/// wires up the channels this crate does not touch — so falling through is not "ignoring", it is
/// letting FreeRDP's own client-common do the part it already does correctly.
///
/// The two names are matched differently for a reason that is not a style: `cliprdr` is a static
/// virtual channel and arrives under its 8-character SVC name, while `disp` is a *dynamic* one and
/// arrives under the long `Microsoft::Windows::RDS::DisplayControl` its plugin registered
/// (`channels/disp/client/disp_main.c`). Matching a DVC on its short name silently never fires.
unsafe extern "C" fn channel_connected(
    context: *mut c_void,
    e: *const sys::ChannelConnectedEventArgs,
) {
    guarded("ChannelConnected", (), || {
        let ctx = context as *mut sys::rdpContext;
        // SAFETY: FreeRDP passes the context this subscription was made on, and a live event.
        unsafe {
            let name = std::ffi::CStr::from_ptr((*e).name);
            // The constants are byte arrays including their NUL; `CStr::to_bytes` excludes one.
            let name = name.to_bytes_with_nul();
            let Some(bridge_ref) = bridge(ctx) else { return };

            if name == sys::CLIPRDR_SVC_CHANNEL_NAME.as_slice() {
                let cliprdr = (*e).pInterface as *mut sys::CliprdrClientContext;
                if cliprdr.is_null() {
                    return;
                }
                // `custom` is cliprdr's own field for exactly this — the pointer a callback uses
                // to find its owner. guacd does the same.
                (*cliprdr).custom = ctx as *mut c_void;
                (*cliprdr).MonitorReady = Some(clipboard_monitor_ready);
                (*cliprdr).ServerCapabilities = Some(clipboard_server_capabilities);
                (*cliprdr).ServerFormatList = Some(clipboard_server_format_list);
                (*cliprdr).ServerFormatDataRequest = Some(clipboard_server_format_data_request);
                (*cliprdr).ServerFormatDataResponse = Some(clipboard_server_format_data_response);
                bridge_ref.cliprdr = cliprdr;
            } else if name == sys::DISP_DVC_CHANNEL_NAME.as_slice() {
                let disp = (*e).pInterface as *mut sys::DispClientContext;
                if disp.is_null() {
                    return;
                }
                (*disp).custom = ctx as *mut c_void;
                (*disp).DisplayControlCaps = Some(display_control_caps);
                bridge_ref.disp = disp;
                // No `resize_ready` here. The channel being open is this client's half; the
                // server's half is the capabilities PDU, and a layout sent before it goes to a
                // server that has not agreed to listen.
            } else {
                sys::freerdp_client_OnChannelConnectedEventHandler(context, e);
            }
        }
    })
}

/// The other half, and it exists to prevent a use-after-free rather than to tidy up.
///
/// A channel can close while the session lives on — the peer drops it, or the plugin fails — and
/// closing it frees the interface struct. Without this, `bridge.cliprdr` and `bridge.disp` would
/// go on pointing at freed memory, and the next `advertise` or `resize` would write through it.
/// That is a use-after-free that would usually *work*, which is the worst kind.
unsafe extern "C" fn channel_disconnected(
    context: *mut c_void,
    e: *const sys::ChannelDisconnectedEventArgs,
) {
    guarded("ChannelDisconnected", (), || {
        let ctx = context as *mut sys::rdpContext;
        // SAFETY: as in `channel_connected` — the subscription's own context and a live event.
        unsafe {
            let name = std::ffi::CStr::from_ptr((*e).name).to_bytes_with_nul();
            let Some(bridge_ref) = bridge(ctx) else { return };

            if name == sys::CLIPRDR_SVC_CHANNEL_NAME.as_slice() {
                bridge_ref.cliprdr = std::ptr::null_mut();
                bridge_ref.clipboard_ready = false;
            } else if name == sys::DISP_DVC_CHANNEL_NAME.as_slice() {
                bridge_ref.disp = std::ptr::null_mut();
                bridge_ref.resize_ready = false;
            } else {
                sys::freerdp_client_OnChannelDisconnectedEventHandler(context, e);
            }
        }
    })
}

/// The server's DisplayControl capabilities — and the moment [`Input::resize`] starts working.
///
/// Like the cliprdr callbacks below, this returns a **channel error code where zero is success**.
///
/// The two area factors multiply to give the largest total monitor area the server will accept
/// (MS-RDPEDISP 2.2.2.1). They are reported rather than enforced: this crate asks for one monitor
/// whose size the caller chose, and a server that dislikes it says so by not resizing, which the
/// caller sees as the absence of an [`Event::Resize`].
unsafe extern "C" fn display_control_caps(
    disp: *mut sys::DispClientContext,
    max_monitors: sys::UINT32,
    area_factor_a: sys::UINT32,
    area_factor_b: sys::UINT32,
) -> sys::UINT {
    guarded("DisplayControlCaps", sys::ERROR_INTERNAL_ERROR, || {
        if disp.is_null() {
            return sys::ERROR_INTERNAL_ERROR;
        }
        // SAFETY: `custom` holds the rdpContext `channel_connected` stored, and that context
        // outlives the channel.
        let ctx = unsafe { (*disp).custom } as *mut sys::rdpContext;
        let Some(bridge) = (unsafe { bridge(ctx) }) else { return sys::CHANNEL_RC_OK };

        bridge.resize_ready = true;
        bridge.send(Event::ResizeReady {
            max_monitors,
            max_area: u64::from(area_factor_a) * u64::from(area_factor_b),
        });

        // Whatever was asked for before the channel existed goes back on the queue rather than
        // being sent from here. This callback runs on the channel's own thread, part-way through
        // handling an inbound PDU, and every other FreeRDP call in this crate is made from the
        // event loop — putting it on the queue keeps that true for one more case rather than
        // making this the exception.
        //
        // It does not make the resize *land*. A Windows host ignores a layout sent this early in
        // a session whatever thread it came from; that is measured, and `Input::resize` carries
        // the numbers and says whose job the retry is.
        if let Some((width, height, scale_percent)) = bridge.pending_resize.take() {
            bridge.shared.push(Command::Resize { width, height, scale_percent });
        }
        sys::CHANNEL_RC_OK
    })
}

/// **The cliprdr callbacks return a channel error code, where zero means success.**
///
/// The opposite convention to every other callback in this file: `PostConnect`, `EndPaint` and
/// the pointer hooks return `BOOL`, where non-zero means success. Getting it backwards here is
/// not a subtle failure — a `1` returned from `MonitorReady` is `ERROR_INVALID_FUNCTION`, and
/// FreeRDP tears the whole session down over it:
///
/// ```text
/// [ERROR][...cliprdr.client] cliprdr_process_monitor_ready: MonitorReady failed with error 1!
/// [ERROR][...channels.addin] channel_client_thread_proc: msg_handler failed with error 1!
/// [ERROR][com.freerdp.core] checkChannelErrorEvent: cliprdr_virtual_channel_client_thread ...
/// ```
///
/// which reaches the caller as an *orderly* `Ended(Ok(()))` a second after connecting, with no
/// hint that the clipboard was the cause. That is measured rather than hypothetical: it is what
/// the first run of `freerdp-e2e` against a real xrdp did. Hence `CHANNEL_RC_OK` spelled out at
/// every return below rather than a bare `0`.
///
/// Recover the bridge from a cliprdr callback's context.
///
/// # Safety
///
/// `cliprdr` must be the context FreeRDP passed to one of its callbacks, with `custom` as
/// `channel_connected` set it.
unsafe fn cliprdr_bridge<'a>(
    cliprdr: *mut sys::CliprdrClientContext,
) -> Option<(&'a mut Bridge, *mut sys::CliprdrClientContext)> {
    if cliprdr.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `custom` holds the rdpContext stored above.
    let ctx = unsafe { (*cliprdr).custom } as *mut sys::rdpContext;
    // SAFETY: that context is one of ours and outlives the channel.
    unsafe { bridge(ctx) }.map(|bridge| (bridge, cliprdr))
}

/// The server's capability set. Answering with ours is what completes the exchange.
unsafe extern "C" fn clipboard_server_capabilities(
    cliprdr: *mut sys::CliprdrClientContext,
    _capabilities: *const sys::CLIPRDR_CAPABILITIES,
) -> u32 {
    guarded("cliprdr ServerCapabilities", sys::ERROR_INTERNAL_ERROR, || {
        // SAFETY: FreeRDP passes a live context whose `custom` is set.
        unsafe { send_client_capabilities(cliprdr) }
    })
}

/// The server is ready. Nothing may be advertised before this.
unsafe extern "C" fn clipboard_monitor_ready(
    cliprdr: *mut sys::CliprdrClientContext,
    _ready: *const sys::CLIPRDR_MONITOR_READY,
) -> u32 {
    guarded("cliprdr MonitorReady", sys::ERROR_INTERNAL_ERROR, || {
        // SAFETY: as above.
        let Some((bridge, cliprdr)) = (unsafe { cliprdr_bridge(cliprdr) }) else { return sys::CHANNEL_RC_OK };
        // Capabilities again, because MS-RDPECLIP allows the server to send MonitorReady without
        // a preceding capability PDU and FreeRDP's own clients answer both. Sending twice is
        // harmless; sending never leaves the channel half-open.
        // SAFETY: as above.
        unsafe { send_client_capabilities(cliprdr) };
        bridge.clipboard_ready = true;
        bridge.send(Event::Clipboard(ClipboardEvent::Ready));
        // Anything the caller advertised before the channel was ready goes out now, rather than
        // being silently lost — which is what a caller who sets the clipboard immediately after
        // connecting would otherwise see.
        if let Some(formats) = bridge.pending_advertise.take() {
            // SAFETY: as above.
            unsafe { send_format_list(cliprdr, &formats) };
        }
        sys::CHANNEL_RC_OK
    })
}

/// # Safety
///
/// `cliprdr` must be a live client context.
unsafe fn send_client_capabilities(cliprdr: *mut sys::CliprdrClientContext) -> u32 {
    // SAFETY: the caller guarantees the context; every pointer below is to a local that outlives
    // the call, and FreeRDP serialises the structures rather than retaining them.
    unsafe {
        let Some(send) = (*cliprdr).ClientCapabilities else { return sys::CHANNEL_RC_OK };
        let mut general = sys::CLIPRDR_GENERAL_CAPABILITY_SET {
            capabilitySetType: sys::CB_CAPSTYPE_GENERAL as u16,
            capabilitySetLength: sys::CB_CAPSTYPE_GENERAL_LEN as u16,
            version: sys::CB_CAPS_VERSION_2,
            generalFlags: clipboard::general_capability_flags(),
        };
        let mut capabilities: sys::CLIPRDR_CAPABILITIES = std::mem::zeroed();
        capabilities.cCapabilitiesSets = 1;
        capabilities.capabilitySets =
            &mut general as *mut _ as *mut sys::CLIPRDR_CAPABILITY_SET;
        send(cliprdr, &capabilities)
    }
}

unsafe extern "C" fn clipboard_server_format_list(
    cliprdr: *mut sys::CliprdrClientContext,
    list: *const sys::CLIPRDR_FORMAT_LIST,
) -> u32 {
    guarded("cliprdr ServerFormatList", sys::ERROR_INTERNAL_ERROR, || {
        // SAFETY: FreeRDP passes a live context and a list valid for this call.
        let Some((bridge, cliprdr)) = (unsafe { cliprdr_bridge(cliprdr) }) else { return sys::CHANNEL_RC_OK };
        // SAFETY: as above.
        let formats = unsafe { clipboard::read_format_list(list) };

        // The response goes first, and unconditionally. MS-RDPECLIP has the server *waiting* for
        // it: until this arrives the peer's clipboard owner is blocked, so an implementation that
        // answered only after deciding what to do with the formats would stall a remote
        // application for as long as the caller took to think.
        // SAFETY: as above.
        unsafe {
            if let Some(respond) = (*cliprdr).ClientFormatListResponse {
                let mut response: sys::CLIPRDR_FORMAT_LIST_RESPONSE = std::mem::zeroed();
                response.common.msgFlags = sys::CB_RESPONSE_OK as u16;
                respond(cliprdr, &response);
            }
        }
        bridge.send(Event::Clipboard(ClipboardEvent::RemoteFormats(formats)));
        sys::CHANNEL_RC_OK
    })
}

unsafe extern "C" fn clipboard_server_format_data_request(
    cliprdr: *mut sys::CliprdrClientContext,
    request: *const sys::CLIPRDR_FORMAT_DATA_REQUEST,
) -> u32 {
    guarded("cliprdr ServerFormatDataRequest", sys::ERROR_INTERNAL_ERROR, || {
        // SAFETY: FreeRDP passes a live context and a request valid for this call.
        let Some((bridge, _)) = (unsafe { cliprdr_bridge(cliprdr) }) else { return sys::CHANNEL_RC_OK };
        // SAFETY: as above.
        let format = unsafe { (*request).requestedFormatId };
        if !bridge.send(Event::Clipboard(ClipboardEvent::LocalDataRequest { format })) {
            // Nobody is listening, so nobody will answer — and an unanswered request leaves the
            // remote application blocked in its paste handler. Refuse it here instead.
            // SAFETY: as above.
            unsafe { respond_with(cliprdr, None) };
        }
        sys::CHANNEL_RC_OK
    })
}

unsafe extern "C" fn clipboard_server_format_data_response(
    cliprdr: *mut sys::CliprdrClientContext,
    response: *const sys::CLIPRDR_FORMAT_DATA_RESPONSE,
) -> u32 {
    guarded("cliprdr ServerFormatDataResponse", sys::ERROR_INTERNAL_ERROR, || {
        // SAFETY: FreeRDP passes a live context and a response valid for this call.
        let Some((bridge, cliprdr)) = (unsafe { cliprdr_bridge(cliprdr) }) else { return sys::CHANNEL_RC_OK };
        // `lastRequestedFormatId` is cliprdr's own record of what this is an answer to. The
        // response PDU does not carry the format id — MS-RDPECLIP allows only one outstanding
        // request — so this field is the only thing that says which one came back.
        // SAFETY: as above.
        let format = unsafe { (*cliprdr).lastRequestedFormatId };
        // SAFETY: as above; `dataLen` and `requestedFormatData` describe the same buffer.
        let event = unsafe {
            let ok = (*response).common.msgFlags & sys::CB_RESPONSE_FAIL as u16 == 0;
            let length = (*response).common.dataLen as usize;
            let data = (*response).requestedFormatData;
            if ok && !data.is_null() {
                ClipboardEvent::RemoteData {
                    format,
                    data: std::slice::from_raw_parts(data, length).to_vec(),
                }
            } else {
                ClipboardEvent::RemoteDataFailed { format }
            }
        };
        bridge.send(Event::Clipboard(event));
        sys::CHANNEL_RC_OK
    })
}

/// # Safety
///
/// `ctx` must be a live connected context.
unsafe fn clipboard_advertise(ctx: *mut sys::rdpContext, formats: Vec<ClipboardFormat>) {
    // SAFETY: the caller guarantees the context.
    let Some(bridge) = (unsafe { bridge(ctx) }) else { return };
    if bridge.cliprdr.is_null() {
        return;
    }
    if !bridge.clipboard_ready {
        // Held rather than sent. A format list before MonitorReady is discarded by the peer, so
        // sending it now would look like it worked and leave the remote clipboard empty.
        bridge.pending_advertise = Some(formats);
        return;
    }
    // SAFETY: `cliprdr` was stored by `channel_connected` and lives as long as the channel.
    unsafe { send_format_list(bridge.cliprdr, &formats) };
}

/// # Safety
///
/// `cliprdr` must be a live client context.
unsafe fn send_format_list(cliprdr: *mut sys::CliprdrClientContext, formats: &[ClipboardFormat]) {
    // The names have to outlive the call, so they are held in this vector rather than built
    // inside the map below — a `CString` created and dropped per iteration would leave the
    // `CLIPRDR_FORMAT` pointing at freed memory, and it would usually still work.
    let names: Vec<Option<CString>> = formats
        .iter()
        .map(|format| format.name.as_deref().and_then(|name| CString::new(name).ok()))
        .collect();
    let mut entries: Vec<sys::CLIPRDR_FORMAT> = formats
        .iter()
        .zip(&names)
        .map(|(format, name)| sys::CLIPRDR_FORMAT {
            formatId: format.id,
            formatName: name
                .as_ref()
                .map_or(std::ptr::null_mut(), |name| name.as_ptr() as *mut c_char),
        })
        .collect();

    // SAFETY: the caller guarantees the context. `entries` and `names` outlive the call, and
    // FreeRDP serialises the list rather than retaining it.
    unsafe {
        let Some(send) = (*cliprdr).ClientFormatList else { return };
        let mut list: sys::CLIPRDR_FORMAT_LIST = std::mem::zeroed();
        list.numFormats = entries.len() as u32;
        list.formats = entries.as_mut_ptr();
        send(cliprdr, &list);
    }
}

/// # Safety
///
/// `ctx` must be a live connected context.
unsafe fn clipboard_request(ctx: *mut sys::rdpContext, format: u32) {
    // SAFETY: the caller guarantees the context.
    let Some(bridge) = (unsafe { bridge(ctx) }) else { return };
    if bridge.cliprdr.is_null() {
        return;
    }
    // SAFETY: `cliprdr` lives as long as the channel; the request is serialised, not retained.
    unsafe {
        let cliprdr = bridge.cliprdr;
        let Some(send) = (*cliprdr).ClientFormatDataRequest else { return };
        let mut request: sys::CLIPRDR_FORMAT_DATA_REQUEST = std::mem::zeroed();
        request.requestedFormatId = format;
        send(cliprdr, &request);
    }
}

/// # Safety
///
/// `ctx` must be a live connected context.
unsafe fn clipboard_respond(ctx: *mut sys::rdpContext, _format: u32, data: Option<Vec<u8>>) {
    // SAFETY: the caller guarantees the context.
    let Some(bridge) = (unsafe { bridge(ctx) }) else { return };
    if bridge.cliprdr.is_null() {
        return;
    }
    // SAFETY: as above.
    unsafe { respond_with(bridge.cliprdr, data) };
}

/// Answer an outstanding format-data request, with bytes or with a refusal.
///
/// # Safety
///
/// `cliprdr` must be a live client context.
unsafe fn respond_with(cliprdr: *mut sys::CliprdrClientContext, data: Option<Vec<u8>>) {
    // SAFETY: the caller guarantees the context. `payload` outlives the call, and FreeRDP copies
    // the bytes into the PDU rather than retaining the pointer.
    unsafe {
        let Some(send) = (*cliprdr).ClientFormatDataResponse else { return };
        // The refusal is `None`, not emptiness. `Clipboard::respond(Some(vec![]))` is a caller
        // saying "this format, and it is empty" — an empty selection, or text that really is zero
        // bytes — and answering that with CB_RESPONSE_FAIL tells the peer the request could not be
        // served, which is a different sentence and leaves whatever it had on its own clipboard.
        let refused = data.is_none();
        let payload = data.unwrap_or_default();
        let mut response: sys::CLIPRDR_FORMAT_DATA_RESPONSE = std::mem::zeroed();
        response.common.msgFlags =
            if refused { sys::CB_RESPONSE_FAIL as u16 } else { sys::CB_RESPONSE_OK as u16 };
        response.common.dataLen = payload.len() as u32;
        response.requestedFormatData = payload.as_ptr();
        send(cliprdr, &response);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout promise the whole callback design rests on.
    ///
    /// FreeRDP allocates `ContextSize` bytes and hands back the address as an `rdpContext*`;
    /// every callback then casts it to `WrapperContext*` to reach `bridge`. If the common context
    /// were not first, or the wrapper were smaller than what FreeRDP writes, that cast would
    /// silently read FreeRDP's own state as a pointer.
    #[test]
    fn the_wrapper_context_starts_with_freerdps_own() {
        assert_eq!(std::mem::offset_of!(WrapperContext, common), 0);
        assert!(
            std::mem::size_of::<WrapperContext>() > std::mem::size_of::<sys::rdpClientContext>()
        );
        assert_eq!(std::mem::offset_of!(WrapperPointer, base), 0);
        assert!(std::mem::size_of::<WrapperPointer>() > std::mem::size_of::<sys::rdpPointer>());
    }

    /// Dropping a session that is still connecting returns promptly.
    ///
    /// The event loop is what reads `Command::Shutdown`, and during startup there is no event loop
    /// — so before `Session::drop` aborted the connect, this join waited out the whole
    /// `connect_timeout` on the caller's thread. Measured rather than reasoned, because the
    /// difference between the two versions is entirely one of *time*.
    ///
    /// `10.255.255.1` is RFC 1918 space with nothing on it, chosen because a connect there hangs
    /// rather than being refused. A network that refuses it anyway makes this pass for a different
    /// reason, which is why the bound is 10 s against a 60 s timeout rather than something tight:
    /// the test can be uninformative, but it cannot fail for being on the wrong network.
    #[test]
    fn dropping_a_connecting_session_does_not_wait_out_the_timeout() {
        let started = std::time::Instant::now();
        let (session, _events) = Session::start(Connect {
            host: "10.255.255.1".into(),
            port: 3389,
            connect_timeout: Duration::from_secs(60),
            ..Connect::default()
        });
        drop(session);
        let took = started.elapsed();
        assert!(
            took < Duration::from_secs(10),
            "dropping a connecting session took {took:?} — the abort did not reach freerdp_connect"
        );
    }

    /// The same, for a drop that arrives once the connect is already in flight.
    ///
    /// A different mechanism from the test above and worth its own name: this one goes through
    /// `freerdp_abort_connect_context` and the abort event that `freerdp_tcp_connect_timeout`
    /// waits on beside the socket (`libfreerdp/core/tcp.c:836`), where the other never let the
    /// connect start. A second of head start is what puts the thread inside that wait.
    #[test]
    fn dropping_a_session_mid_connect_does_not_wait_out_the_timeout() {
        let (session, _events) = Session::start(Connect {
            host: "10.255.255.1".into(),
            port: 3389,
            connect_timeout: Duration::from_secs(60),
            ..Connect::default()
        });
        std::thread::sleep(Duration::from_secs(1));
        let started = std::time::Instant::now();
        drop(session);
        let took = started.elapsed();
        assert!(
            took < Duration::from_secs(10),
            "dropping a session mid-connect took {took:?} — the abort event did not reach the \
             connect's wait"
        );
    }

    /// The default posture, stated as a test so that a change to it is a change to a file
    /// somebody has to read.
    #[test]
    fn the_defaults_are_the_ones_documented() {
        let connect = Connect::default();
        assert_eq!(connect.port, 3389);
        assert_eq!(connect.security, Security::Auto);
        assert!(connect.clipboard);
        // Off, and the opposite of the clipboard on purpose — a resize renegotiates the session.
        assert!(!connect.resize);
        // Also off: sound a caller never asked for is bandwidth it never asked for, and unlike
        // the clipboard there is nowhere for it to go by default.
        assert!(connect.audio.is_none());

        let keepalive = KeepAlive::default();
        assert_eq!(seconds(keepalive.idle), 10);
        assert_eq!(seconds(keepalive.interval), 5);
        assert_eq!(millis(keepalive.ack_timeout), 30_000);
    }

    /// The one monitor this crate ever asks for, pinned field by field.
    ///
    /// Worth a test rather than a read-through because `DISPLAY_CONTROL_MONITOR_LAYOUT` is ten
    /// same-typed integers in a row: a field set in the wrong position still compiles, still
    /// sends, and comes back as a desktop of some other size.
    #[test]
    fn the_monitor_layout_describes_one_primary_monitor() {
        let layout = monitor_layout(1280, 800, 200);
        assert_eq!(layout.Flags, sys::DISPLAY_CONTROL_MONITOR_PRIMARY);
        assert_eq!((layout.Width, layout.Height), (1280, 800));
        assert_eq!((layout.Left, layout.Top), (0, 0));
        // Zero is the protocol's "unknown", and the reason is on `monitor_layout`.
        assert_eq!((layout.PhysicalWidth, layout.PhysicalHeight), (0, 0));
        // The caller's density, and the pinned companion that is not the same field.
        assert_eq!(layout.DesktopScaleFactor, 200);
        assert_eq!(layout.DeviceScaleFactor, 100);
        assert_eq!(std::mem::size_of_val(&layout) as u32, sys::DISPLAY_CONTROL_MONITOR_LAYOUT_SIZE);
    }

    /// The `sys:` argument really reaches the settings, on both transports.
    ///
    /// This is the one assertion that separates working sound from silence, and it is worth a
    /// test because the failure is invisible: with no subsystem named, `rdpsnd_process_connect`
    /// walks its compiled-in backends, the last of which is `fake` — a device that accepts every
    /// format and throws every buffer away. A session that lost this argument would negotiate
    /// audio, report no error, log "Loaded fake backend for rdpsnd" at a level nobody reads, and
    /// play nothing.
    ///
    /// Both transports, and the *both* was re-measured: withholding the dynamic offer to force
    /// Windows onto the reactivation-surviving static channel produced no audio at all — the
    /// server never fell back.
    #[test]
    fn the_audio_channel_names_this_crates_subsystem() {
        // SAFETY: a standalone settings object, used and freed here; nothing else refers to it.
        unsafe {
            let settings = sys::freerdp_settings_new(0);
            assert!(!settings.is_null());
            register_audio_channels(settings).expect("FreeRDP refused the rdpsnd channel");

            for found in [
                sys::freerdp_static_channel_collection_find(settings, c"rdpsnd".as_ptr()),
                sys::freerdp_dynamic_channel_collection_find(settings, c"rdpsnd".as_ptr()),
            ] {
                assert!(!found.is_null(), "rdpsnd was not registered");
                assert_eq!((*found).argc, 2, "the channel was registered with no argument");
                let arg = std::ffi::CStr::from_ptr(*(*found).argv.offset(1));
                assert_eq!(arg, audio::SUBSYSTEM_ARG);
            }
            sys::freerdp_settings_free(settings);
        }
    }

    /// A duration too large for FreeRDP's `UINT32` saturates rather than wrapping — a wrapped
    /// `Duration::MAX` would come out as a *short* timeout, which is the opposite of what the
    /// caller asked for.
    #[test]
    fn oversized_durations_saturate() {
        assert_eq!(seconds(Duration::MAX), u32::MAX);
        assert_eq!(millis(Duration::MAX), u32::MAX);
        assert_eq!(millis(Duration::from_secs(1)), 1000);
    }
}
