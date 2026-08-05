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
//! Exit code 0 means every check that ran passed. Anything else prints why.

use freerdp::{Connect, Event, Session};
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

/// The half that needs a server: connect, paint, disconnect.
fn connect_check(host: &str, port: u16, username: &str, password: &str) {
    println!();
    println!("connecting to {host}:{port} as {username}");

    let (session, events) = Session::start(Connect {
        host: host.into(),
        port,
        username: username.into(),
        password: password.into(),
        width: 1024,
        height: 768,
        ..Connect::default()
    });

    // A deadline rather than a blocking receive, so a server that accepts the TCP connection and
    // then says nothing fails as a timeout with a message instead of hanging the CI job until the
    // runner's own limit kills it with none.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut connected = None;
    let mut painted = 0usize;
    let mut pixels = 0usize;

    while Instant::now() < deadline {
        let Ok(event) = events.recv_timeout(Duration::from_secs(5)) else { continue };
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
            Event::Cursor(cursor) => println!("cursor          {cursor:?}"),
            Event::Ended(result) => {
                panic!("the session ended before it painted: {result:?}");
            }
            _ => {}
        }
    }

    let (width, height) = connected.expect("the session never reported a desktop size");
    assert!(painted > 0, "connected to {host} but nothing ever painted");
    println!("painted         {painted} rectangles, {pixels} pixels");

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

    session.shutdown();
    println!("disconnected    cleanly");
}
