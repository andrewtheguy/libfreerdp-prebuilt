//! A *consumer* of the prebuilt archives, built and run on every target by the pipeline.
//!
//! Compiling a test proves the bindings parse. This proves the rest: that the archives link into
//! a real binary, that the linked library is the pinned one rather than a system FreeRDP that won
//! the link, that a context can be built and configured, and — where a server is available — that
//! a session connects, paints real pixels and disconnects cleanly.
//!
//! ```text
//!   freerdp-e2e                          the offline checks; what CI runs on macOS
//!   freerdp-e2e <host> <user> <pass>     the above, then a real connection
//! ```
//!
//! The offline half is what a macOS runner can do: GitHub's macOS runners cannot run Linux
//! containers, so there is no xrdp for them to talk to. That is stated here rather than left to
//! look like full coverage — the connecting half runs on the Linux targets, against the same xrdp
//! container image the consuming project already uses.
//!
//! The resize leg is the one part conditional on the *server* rather than on the archives:
//! MS-RDPEDISP is optional, so a server that does not offer it is reported and skipped rather than
//! failing a build of the libraries over the age of a container image.
//!
//! Exit code 0 means every check that ran passed. Anything else prints why.

use freerdp::{
    Audio, AudioFormat, AudioSink, Camera, CameraEvents, CameraFormat, ClipboardEvent,
    ClipboardFormat, Connect, Event, Session,
};

/// `CF_UNICODETEXT`, Windows' own id for plain text. Named here rather than imported
/// because the engine crate deliberately carries format ids as plain numbers.
const CF_UNICODETEXT: u32 = 13;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    let mut args = std::env::args().skip(1);
    offline_checks();

    match (args.next(), args.next(), args.next()) {
        (Some(host), Some(username), Some(password)) => {
            let port = args.next().and_then(|p| p.parse().ok()).unwrap_or(3389);
            connect_check(&host, port, &username, &password);
        }
        _ => {
            println!();
            println!("no server given, so the connecting half did not run.");
            println!("  freerdp-e2e <host> <user> <pass> [port]");
        }
    }
    println!();
    println!("ok");
}

/// Everything provable without a server.
fn offline_checks() {
    // The check that catches a system FreeRDP winning the link. `PREBUILT_VERSION` comes from
    // freerdp.env by way of build.rs; `version()` asks the archive that actually got linked.
    let linked = freerdp::freerdp_version();
    let pinned = freerdp_sys::PREBUILT_VERSION;
    println!("linked FreeRDP  {linked}");
    println!("pinned FreeRDP  {pinned}");
    assert!(
        linked.starts_with(pinned),
        "the linked FreeRDP is {linked}, but this was built against {pinned} — something other \
         than the prebuilt archive won the link"
    );

    // A context, and an opaque-settings round trip through it. Between them these exercise the
    // allocator, the entry-point table, `ContextSize`, and the settings API that every
    // configuration decision goes through — the parts a link error would not have caught.
    //
    // Through `Session::start` against an address nothing answers on, because that is the same
    // path a real connection takes. `127.0.0.1:1` is chosen for being refused immediately by the
    // kernel rather than timing out, on every platform this builds for.
    let (session, events) = Session::start(Connect {
        host: "127.0.0.1".into(),
        port: 1,
        username: "nobody".into(),
        password: "nothing".into(),
        connect_timeout: Duration::from_secs(5),
        ..Connect::default()
    });
    let event = events.recv_timeout(Duration::from_secs(30)).expect("no event from a refused connect");
    match event {
        Event::Ended(Err(error)) => println!("refused connect  {error}"),
        // Not a soft failure. An `Ok` here would mean a connection to port 1 succeeded, and a
        // `Connected` would mean something is listening that should not be — either way the
        // result below tells us nothing about the archive.
        other => panic!("expected a failure from 127.0.0.1:1, got {other:?}"),
    }
    session.shutdown();
    println!("context         built, configured and torn down");
}

/// What the audio leg counts, from the FreeRDP thread.
///
/// Atomics rather than a channel because that is what an [`AudioSink`] is allowed to be: every
/// method here runs on the thread decoding the desktop, and a sink that blocked would stop it.
#[derive(Default)]
struct Recorder {
    /// How many of the server's formats this session accepted, `usize::MAX` until the channel
    /// announced any — which is how "never negotiated" is told from "negotiated nothing".
    accepted: AtomicUsize,
    buffers: AtomicUsize,
    bytes: AtomicUsize,
    /// Wave buffers whose length was not a whole number of samples. Should be zero; anything else
    /// means the bytes are not the PCM they were said to be.
    ragged: AtomicUsize,
}

impl Recorder {
    fn new() -> Self {
        Self { accepted: AtomicUsize::new(usize::MAX), ..Self::default() }
    }
}

impl AudioSink for Recorder {
    fn negotiated(&self, accepted: usize) {
        println!("rdpsnd          negotiated, {accepted} of the server's formats accepted");
        self.accepted.store(accepted, Ordering::Relaxed);
    }

    fn opened(&self, format: AudioFormat) {
        println!(
            "rdpsnd          playing {} Hz, {} channel(s), {}-bit",
            format.sample_rate, format.channels, format.bits_per_sample
        );
    }

    fn wave(&self, samples: &[u8]) {
        self.buffers.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(samples.len(), Ordering::Relaxed);
        if !samples.len().is_multiple_of(AudioFormat::CD.block_align() as usize) {
            self.ragged.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn closed(&self) {
        println!("rdpsnd          the device closed");
    }
}

/// What the camera leg counts, from the FreeRDP thread. Atomics for the same reason as
/// [`Recorder`]: every [`CameraEvents`] method runs on a thread that must not wait.
#[derive(Default)]
struct CamRecorder {
    /// The negotiated MS-RDPECAM version, 0 until the host opened the enumeration channel.
    negotiated: AtomicUsize,
    /// Whether the host connected the device channel — the virtual camera installing.
    attached: AtomicUsize,
    started: AtomicUsize,
    stopped: AtomicUsize,
}

impl CameraEvents for CamRecorder {
    fn negotiated(&self, version: u8) {
        println!("rdpecam         negotiated, protocol version {version}");
        self.negotiated.store(version as usize, Ordering::Relaxed);
    }

    fn attached(&self) {
        println!("rdpecam         the host attached the device channel");
        self.attached.fetch_add(1, Ordering::Relaxed);
    }

    fn started(&self, format: CameraFormat) {
        println!(
            "rdpecam         streaming started, {}x{} at {}/{} fps",
            format.width, format.height, format.fps_numerator, format.fps_denominator
        );
        self.started.fetch_add(1, Ordering::Relaxed);
    }

    fn stopped(&self) {
        println!("rdpecam         streaming stopped");
        self.stopped.fetch_add(1, Ordering::Relaxed);
    }

    fn keyframe_needed(&self) {}
}

/// The half that needs a server: connect, paint, disconnect.
fn connect_check(host: &str, port: u16, username: &str, password: &str) {
    println!();
    println!("connecting to {host}:{port} as {username}");

    let recorder = Arc::new(Recorder::new());
    let cam_recorder = Arc::new(CamRecorder::default());
    // Plugged before the connect, which is the documented license: the announcement waits for
    // the enumeration channel and goes out by itself. No sample is ever fed here — streaming
    // needs an application on the host to open the camera — so what this leg proves is the
    // handshake: enumeration, announcement, and the host installing the device.
    let camera = Camera::new("FreeRDP E2E Camera", cam_recorder.clone());
    camera.plug(CameraFormat { width: 640, height: 480, fps_numerator: 30, fps_denominator: 1 });
    let (session, events) = Session::start(Connect {
        host: host.into(),
        port,
        username: username.into(),
        password: password.into(),
        width: 1024,
        height: 768,
        // On for the resize leg below, and this is the only place in the repository that turns it
        // on — `Connect::resize` says why it is off by default.
        resize: true,
        // On so the touch leg can *report*: whether the host opens MS-RDPEI is a property of the
        // host (Windows 8 and later do; xrdp never does), so it is printed rather than asserted,
        // like the clipboard's and audio's "not offered".
        touch: true,
        // `E2E_EGFX=0` runs the same legs down the **legacy orders path** instead of the graphics
        // pipeline. The caches in `apply_settings` take effect only on that path, and until this
        // knob existed this program hardcoded itself out of reaching it — a test that reaches one
        // of the two paths cannot exercise a setting that lives on the other. That is the whole
        // reason it is here, and it is *not* that the measurement needed it: an embedder reaches
        // the same path through its own configuration, and remotex printed the same
        // `update_dump_stats` counters under `WLOG_LEVEL=TRACE` with `egfx = false` on a target.
        egfx: std::env::var("E2E_EGFX").as_deref() != Ok("0"),
        audio: Some(Audio { format: AudioFormat::CD, sink: recorder.clone() }),
        camera: Some(camera.clone()),
        ..Connect::default()
    });

    // A deadline rather than a blocking receive, so a server that accepts the TCP connection and
    // then says nothing fails as a timeout with a message instead of hanging the CI job until the
    // runner's own limit kills it with none.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut connected = None;
    let mut painted = 0usize;
    let mut pixels = 0usize;
    let mut frames = 0usize;
    let mut resize_ready = false;
    let mut touch_ready = false;
    let mut clipboard_ready = false;

    while Instant::now() < deadline {
        let event = match events.recv_timeout(Duration::from_secs(5)) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => continue,
            // A closed channel is not a quiet server, it is a gone one: the sender lives on the
            // session thread, so this means that thread has ended. Treating it as a timeout would
            // spin the deadline out and then blame whichever assertion below noticed first.
            Err(RecvTimeoutError::Disconnected) => break,
        };
        match event {
            Event::Connected { width, height } => {
                println!("connected       {width}x{height}");
                connected = Some((width, height));
            }
            Event::Paint(rect) => {
                painted += 1;
                pixels += rect.width as usize * rect.height as usize;
                // Enough to say the decode path works end to end. More would be measuring the
                // server's idea of how busy its desktop is, which is not this program's business.
                if painted >= 5 {
                    break;
                }
            }
            Event::Frame => frames += 1,
            Event::Cursor(cursor) => println!("cursor          {cursor:?}"),
            Event::ResizeReady { max_monitors, max_area } => {
                println!("displaycontrol  up to {max_monitors} monitors, {max_area} pixels");
                resize_ready = true;
            }
            Event::TouchReady => {
                println!("rdpei           the host opened the touch channel");
                touch_ready = true;
            }
            Event::Clipboard(ClipboardEvent::Ready) => {
                println!("cliprdr         capability exchange finished");
                clipboard_ready = true;
                // Advertise something, which is the half of the clipboard a test can
                // drive on its own — the other half needs a person to press paste on
                // the remote. Worth doing here rather than nowhere: the one clipboard
                // bug this crate has had was a callback returning the wrong kind of
                // success, and it presented as the *session* ending a second after
                // connecting rather than as anything clipboard-shaped. So what is
                // being checked is that the session survives its own clipboard.
                if let Some(clipboard) = session.clipboard() {
                    clipboard.advertise(vec![ClipboardFormat::new(CF_UNICODETEXT)]);
                }
            }
            Event::Clipboard(other) => println!("cliprdr         {other:?}"),
            Event::Ended(result) => {
                panic!("the session ended before it painted: {result:?}");
            }
            _ => {}
        }
    }

    let (width, height) = connected.expect("the session never reported a desktop size");
    assert!(painted > 0, "connected to {host} but nothing ever painted");
    println!("painted         {painted} rectangles, {pixels} pixels");
    // Reported rather than asserted, because marking frames is the *server's* property: a
    // server that ignores both marker capabilities is legitimate, and a consumer falls back
    // to its own pacing. What this line settles is which kind of server the run was against.
    match frames {
        0 => println!("frames          this server marked no frame boundaries"),
        n => println!("frames          {n} boundaries marked by the server"),
    }
    // Not an assertion: a server may not offer the clipboard at all, and this
    // program runs against whatever the pipeline could start. Reported so that a
    // silent absence is visible rather than looking like coverage.
    if !clipboard_ready {
        println!("cliprdr         not offered by this server");
    }
    // The same shape for touch, and the same reason: xrdp has no MS-RDPEI, so against the
    // pipeline's container this line is the expected answer rather than a failure.
    if !touch_ready {
        println!("rdpei           not offered by this server");
    }

    // The framebuffer is not merely allocated: something wrote to it. A connected session whose
    // buffer is entirely zero is exactly the black-screen failure that decided this crate would
    // not advertise the graphics pipeline, so it is worth a check rather than a comment.
    let non_zero = session.framebuffer().with(|frame| {
        assert_eq!((frame.width, frame.height), (width, height));
        frame.pixels.iter().filter(|byte| **byte != 0).count()
    });
    assert!(
        non_zero > 0,
        "the framebuffer is entirely zero after {painted} paints — the session connected and \
         decoded, but no pixels reached it"
    );
    println!("framebuffer     {non_zero} non-zero bytes of {}", width * height * 4);

    // And that input reaches the server without erroring. There is nothing to assert about the
    // *effect* — that would be asserting on a remote desktop's behaviour — but a send that tears
    // the connection down would show up as the `Ended` below.
    session.input().mouse_move(width as u16 / 2, height as u16 / 2);
    session.input().key(0x1C, false, true);
    session.input().key(0x1C, false, false);

    // The resize leg runs first because it is usually the long one, and the sound channel needs
    // that time: `rdpsnd` comes up after the first paints, and a desktop has to make a noise before
    // there is anything to count. Usually, not always — hence `await_audio` after it, for the host
    // that resizes at once. The resize verdict is *held* rather than thrown, so that a host which
    // ignores layouts — which is a fact about the host, and a known one — does not swallow the
    // audio report on the way out.
    let resize_failure = resize_check(&session, &events, resize_ready, width, height);
    await_audio(&recorder, &events);
    audio_check(&recorder);
    camera_check(&cam_recorder);
    if let Some(why) = resize_failure {
        panic!("{why}");
    }

    session.shutdown();
    println!("disconnected    cleanly");
}

/// Give `rdpsnd` until a deadline to say whether it negotiated, so that "not offered" below is a
/// verdict rather than a race.
///
/// The resize leg is the long one, but only when the server makes it long: it returns the moment a
/// resize arrives, which against a prompt host can be a second in — early enough that a sound
/// channel still coming up would be reported as one the server never offered at all. So the wait
/// belongs here, where it can end as soon as `negotiated` fires.
///
/// Events are drained rather than slept through. The session runs on its own thread and this is the
/// only reader of its queue; letting that queue grow for the length of the wait would leave the
/// shutdown below with a backlog to walk and prove nothing.
fn await_audio(recorder: &Recorder, events: &std::sync::mpsc::Receiver<Event>) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while recorder.accepted.load(Ordering::Relaxed) == usize::MAX && Instant::now() < deadline {
        match events.recv_timeout(Duration::from_secs(1)) {
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            // The session thread is gone, so nothing more can negotiate. Nothing is reported here:
            // an `Ended` is the resize leg's to complain about, and what this leg has to say about
            // a session that never came up is exactly what `audio_check` says next.
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// What the sound channel did, and what can honestly be asserted about it.
///
/// Two of the three parts are the server's to decide, so they are reported rather than asserted:
/// whether `rdpsnd` is offered at all, and whether the remote made a noise while this ran. A
/// desktop sitting at a login screen plays nothing, and failing a build of the archives over that
/// would be a claim about the wrong thing — the same reasoning as the resize and clipboard legs.
///
/// What *is* asserted belongs to this crate. If the channel came up, this crate's device was the
/// one loaded rather than FreeRDP's `fake`, which discards every buffer — and the way to know is
/// that `negotiated` fired at all, since `fake` never calls it. And if any wave arrived, every
/// buffer must be a whole number of samples in the format that was asked for.
///
/// **To exercise the part that only ears can settle**, run this against a host and make it play
/// something while it connects.
fn audio_check(recorder: &Recorder) {
    let accepted = recorder.accepted.load(Ordering::Relaxed);
    if accepted == usize::MAX {
        println!("rdpsnd          not offered by this server");
        return;
    }
    assert_eq!(
        accepted, 1,
        "the server offered rdpsnd but nothing in 44.1 kHz 16-bit stereo PCM, which MS-RDPEA \
         requires both ends to support"
    );

    let buffers = recorder.buffers.load(Ordering::Relaxed);
    let bytes = recorder.bytes.load(Ordering::Relaxed);
    if buffers == 0 {
        println!("rdpsnd          negotiated, but this desktop played nothing while we watched");
        return;
    }
    assert_eq!(
        recorder.ragged.load(Ordering::Relaxed),
        0,
        "{buffers} wave buffers arrived and some were not a whole number of 4-byte samples — \
         these bytes are not the PCM the format says they are"
    );
    println!(
        "rdpsnd          {buffers} wave buffers, {bytes} bytes, {:.2}s of sound",
        bytes as f64 / f64::from(AudioFormat::CD.byte_rate())
    );
}

/// The camera leg's verdicts, all reported rather than asserted, because every one is the
/// server's to decide: whether camera redirection is enabled at all (policy can turn it off),
/// whether the announced device gets installed, and whether anything on that desktop opened the
/// camera while we watched. What the leg proves when the host cooperates is the whole
/// MS-RDPECAM handshake this crate implements — enumeration, announcement, installation — with
/// no sample ever fed, since samples need an application on the far side asking for them.
fn camera_check(recorder: &CamRecorder) {
    match recorder.negotiated.load(Ordering::Relaxed) {
        0 => {
            println!("rdpecam         not offered by this server");
            return;
        }
        v => println!("rdpecam         negotiated at version {v}"),
    }
    match recorder.attached.load(Ordering::Relaxed) {
        0 => println!("rdpecam         announced, but the host never attached the device"),
        _ => println!("rdpecam         the announced device was attached by the host"),
    }
    let (started, stopped) = (
        recorder.started.load(Ordering::Relaxed),
        recorder.stopped.load(Ordering::Relaxed),
    );
    if started > 0 || stopped > 0 {
        println!("rdpecam         streams started {started} time(s), stopped {stopped} time(s)");
    }
}

/// Ask for a different desktop size, and see whether one arrives.
///
/// **Conditional on the server, and deliberately so.** MS-RDPEDISP is optional, older xrdp builds
/// do not implement it, and this program runs against whatever RDP server the pipeline could
/// start — so a hard assertion here would turn "the container image is old" into a failed build of
/// the archives, which is a claim about the wrong thing entirely. What *is* asserted is the part
/// that belongs to this crate: if the server said it would listen, a resize must produce a
/// framebuffer of the new size. A server that never offered the channel is reported and skipped,
/// the same way the offline half reports what a macOS runner cannot do.
///
/// A failure comes back as a sentence rather than a panic so that the audio leg is still reported
/// on the way out — see the call site. Everything below that *is* an invariant of this crate
/// rather than of the server still panics where it is found.
fn resize_check(
    session: &Session,
    events: &std::sync::mpsc::Receiver<Event>,
    ready: bool,
    width: u32,
    height: u32,
) -> Option<String> {
    // Sent before knowing whether the channel is up, on purpose. The paint loop above stops after
    // five rectangles, which may well be before the display-control capabilities have arrived, so
    // reading its `ready` flag as final would skip this check on a race rather than on a fact. The
    // wrapper holds a request made too early and sends it when the channel opens, which is exactly
    // the behaviour worth exercising here.
    //
    // Smaller than the connect size, so it fits inside any desktop the server might have, and
    // deliberately *odd* — 801 is what a real viewport produces, and the width must be even on the
    // wire. If `sanitise_size` did not round it down, this is where a Windows host silently
    // ignores the layout and the resize never arrives.
    // 200% rather than the neutral 100, and that is an assertion rather than a flourish: a server
    // that finds either scale factor out of range must ignore *both*, which means discarding the
    // whole layout — so a resize that lands at all is proof the density reached the right field
    // with a legal value in it. There is nothing else to check it against, since no PDU reports
    // back what scale a server settled on.
    let (want_width, want_height, want_scale) = (801, 600, 200);
    println!("resize          asking for {want_width}x{want_height} at {want_scale}%");
    session.input().resize(want_width, want_height, want_scale);
    let mut asked = Instant::now();

    let mut ready = ready;
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        // The re-ask below is paced by the wall clock, not by the event queue going quiet: a
        // desktop that happens to be animating delivers paints continuously, and a retry that
        // waited for a 5-second gap in events would never fire against it — measured, against a
        // host that was playing a video, where the one early layout was dropped and the "retry"
        // starved for the whole 45-second deadline.
        if ready && asked.elapsed() > Duration::from_secs(5) {
            println!("resize          asking again, {:?} in", asked.elapsed());
            session.input().resize(want_width, want_height, want_scale);
            asked = Instant::now();
        }
        let event = match events.recv_timeout(Duration::from_secs(5)) {
            Ok(event) => event,
            // The session thread always sends `Ended` before it drops the sender, and the arm
            // below panics on that — so reaching here means it went without saying so, and asking
            // a dead session to resize once every five seconds is not a better answer.
            Err(RecvTimeoutError::Disconnected) => {
                return Some(
                    "the event channel closed during the resize — the session thread is gone"
                        .into(),
                )
            }
            // **Asking again is not belt-and-braces, it is the protocol.** A Windows 11 host
            // ignores a monitor layout sent while it is still bringing the session up, and
            // says nothing about having done so — measured here: the same 800x600 layout was
            // dropped 400 ms after the capabilities PDU and honoured 6.7 s in, on the same
            // host in the same session. There is no observable "ready now", so the only thing
            // a client can do is ask again — paced above, where an animating desktop cannot
            // starve it — which is what an embedder driving this from a viewport must also do.
            // See `Input::resize`.
            Err(RecvTimeoutError::Timeout) => continue,
        };
        match event {
            Event::ResizeReady { max_monitors, max_area } => {
                println!("displaycontrol  up to {max_monitors} monitors, {max_area} pixels");
                ready = true;
            }
            Event::Resize { width, height } => {
                println!("resized         {width}x{height}");
                // The size the *server* chose, which need not be the one asked for — but the
                // framebuffer must agree with it, because that is this crate's own invariant and
                // the thing a reallocation under a reader would break.
                session.framebuffer().with(|frame| {
                    assert_eq!(
                        (frame.width, frame.height),
                        (width, height),
                        "the framebuffer disagrees with the resize that was just announced"
                    );
                });
                assert_ne!(
                    (width, height),
                    (0, 0),
                    "a resize to nothing is not a resize"
                );
                return None;
            }
            Event::Ended(result) => {
                return Some(format!("the session ended during the resize: {result:?}"))
            }
            _ => {}
        }
    }

    if ready {
        return Some(format!(
            "the server offered DisplayControl and then ignored a layout for \
             {want_width}x{want_height} at {want_scale}% — the desktop is still {width}x{height} \
             after 45 s"
        ));
    }
    println!("resize          skipped — this server does not offer DisplayControl");
    None
}
