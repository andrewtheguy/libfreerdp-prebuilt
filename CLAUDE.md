# Working in this repository

Read `README.md` first — it says what the repository is for and what the build asserts. This file
is only the things that are easy to get wrong.

## General

- The comments carry the reasoning, and that is the point rather than a style. Almost every flag
  in `build.sh` is there because something *measured* went wrong without it, and a flag with no
  citation and no measurement beside it is one nobody can safely change later. When you add one,
  say which it is.
- **Do not run `cargo fmt`.** `rustfmt.toml` exists for *bindgen*: it is what shapes the two
  committed `src/bindings_*.rs`, so that they are the same on any machine that regenerates them.
  Running `cargo fmt` over the workspace would reflow the hand-written code — which is laid out
  to be read alongside its comments — and there is no CI gate asking for it. Reformatting the
  bindings by hand would make `gen-bindings.sh --check` fail everywhere.
- After Rust changes: `cargo clippy --all-targets -- -D warnings` and `cargo test`. Both need
  archives, so run `./build.sh <this machine's target> && ./sync-prebuilt.sh` first.
- After shell changes: `shellcheck -x build.sh sync-prebuilt.sh check-static.sh source.sh
  crates/freerdp-prebuilt-sys/gen-bindings.sh`.
- Nothing built is committed. `build/`, `dist/`, `prebuilt/` and `target/` are gitignored, and a
  committed `.a` is one nobody can tell apart from the one CI made.

## Things that will bite

- **`cmake` does not fail on an option it never read.** It prints a warning at the very end and
  carries on, which is how `FREERDP_EXTERNAL_SSL_PATH` was silently ignored while the build linked
  Homebrew's OpenSSL and passed. `build.sh` greps the configure log for that warning and fails on
  it, so a `-D` you add that cmake does not consume will fail the build — that is working as
  intended, not a bug in the check.
- **A channel can disappear without a warning**, through its own `cmake_dependent_option`:
  `CHANNEL_DISP` and `CHANNEL_RDPGFX` are silently dropped if `CHANNEL_DRDYNVC` goes off. The
  build asserts each channel's entry-point symbol for that reason.
- **The archives' channel set is frozen at configure time.** `channels/client/CMakeLists.txt`
  generates the addin table from the enabled set, so a consumer cannot turn one on later. That is
  why `rdpgfx` is built in even though the wrapper calls it not at all — and why `rdpsnd` and
  `disp` needed no new archive when they were picked up.
- **Sound is not a channel you subscribe to, it is a device you register.** `rdpsnd` publishes no
  client context: it loads a backend through the process-global addin provider, so `audio.rs`
  chains in front of FreeRDP's own and answers for one subsystem name. Two consequences. The name
  has to reach the channel as a `sys:` argument — with none, `rdpsnd_process_connect` walks its
  compiled-in backends and this build's list ends in `fake`, which accepts every format and
  discards every buffer, so the failure is *silence with no error anywhere*. And the provider
  being global means a second `freerdp_client_context_new` takes it back; a session starting while
  another is still inside `freerdp_connect` can cost that one its device.
- **Turning sound on makes a Windows host start measuring the link.** Measured: with `rdpsnd`
  loaded, a Windows 11 host began continuous network characteristics detection within seconds and
  the session died **five times out of five** — `autodetect_recv_request_packet` answers a request
  it was not configured for with `STATE_RUN_FAILED`. Declining `NetworkAutoDetect` does not stop
  the asking, because the MCS message channel those PDUs arrive on is opened by
  `SupportMultitransport` or `SupportHeartbeatPdu` too. **All three move together**: on, which is
  FreeRDP's default and what guacamole-server ships, and the session survives being asked because
  it answers. The arrangement that must never exist is half of each — a message channel open with
  `NetworkAutoDetect` false is the configuration that died 5/5. `apply_settings` carries the rest
  of the argument, including the drag that detection throttled into 12 batches.
- **cliprdr and disp callbacks return a channel error code, where 0 is success** — the opposite of
  every other callback in the wrapper. Returning 1 from `MonitorReady` tears the session down and
  surfaces to the caller as an orderly `Ended(Ok(()))` a second after connecting.
- **A static channel and a dynamic one arrive under different names.** `cliprdr` is matched on its
  8-character SVC name and `disp` on `Microsoft::Windows::RDS::DisplayControl`, which is what its
  plugin registered. Matching a DVC on its short name compiles and never fires.
- **A Windows host ignores a monitor layout sent early in a session, and says nothing.** Measured:
  the same 800x600 layout dropped 400 ms after that host's own DisplayControl capabilities PDU and
  honoured 6.7 s into the same session. There is no observable "ready now", so the retry belongs to
  the caller — `crates/freerdp` does not have one, and `freerdp-e2e` shows the shape.
- **`BOOL` is a different size on Apple and Linux**, so there are two committed bindings files and
  each can only be regenerated on its own platform. `gen-bindings.sh --check` says out loud that
  it did not check the other one. That split shows up in unpredictable places: `bindings_linux.rs`
  trips rustc's `unnecessary_transmutes` and `bindings_apple.rs` does not, because a four-byte
  `BOOL` makes bindgen transmute in its bitfield accessors and a one-byte one does not. **A local
  `cargo clippy` on this Mac cannot see anything wrong with the Linux bindings**, and that is the
  general case rather than that one lint.
- **bindgen carries on without rustfmt**, emitting a complete but unformatted file — 16 enormous
  lines instead of 30,000 — with only a warning. `gen-bindings.sh` requires rustfmt and asserts a
  floor on the output size for that reason.
- **`sync-prebuilt.sh --headers` takes the committed headers from `dist/linux-x86_64`.** They are
  generated by cmake and host-dependent, so committing a Mac's set would hand every Linux consumer
  a header tree that never built on their platform.

## Testing against a real server

`crates/freerdp-e2e` is the whole test:

```sh
cargo run --release -p freerdp-e2e                               # no server needed
cargo run --release -p freerdp-e2e -- <host> <user> <pw>         # connect, paint, disconnect
cargo run --release -p freerdp-e2e -- <host> <user> <pw> <port>  # …on a port other than 3389
```

The second form is what proves the wrapper, and there is no substitute for it: connecting,
decoding, cursors, clipboard and resize are all things a unit test can only pretend to exercise.
Run it against both a Linux xrdp and a real Windows host — they fail differently, and the Windows
path is the one with CredSSP, NTLM, the graphics-pipeline decision and the layout timing in it.
Three legs report rather than assert, because all three are properties of the *server*: resize
skips itself against a host with no DisplayControl, the clipboard says so if the channel is never
offered, and audio says so if `rdpsnd` is not offered or the desktop simply made no noise. The
audio leg's one real assertion is that `negotiated` fired at all, which is how the `fake` device
is told from this crate's — and to exercise the half only ears can settle, make the remote play
something while it runs. The clipboard leg only drives the half a program can — it advertises a format and
carries on — because the other half needs somebody to press paste on the remote. That is still
worth running: the one clipboard bug this crate has had presented as the *session* ending a
second after connecting, so what it really checks is that a session survives its own clipboard.

## Scope

This repository builds archives and wraps them. It is not where RDP features get designed: if a
consumer needs a channel the archives already carry, the work is new code in `crates/freerdp`, not
a new build. Resize and audio both went in that way and neither needed a new archive — audio did
need the `rdpsnd` headers added to `wrapper.h` and the bindings regenerated on **both** platforms,
which is the one step that cannot be done from one machine.
