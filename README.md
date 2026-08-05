# libfreerdp-prebuilt

Static **FreeRDP 3.30.0** and the **OpenSSL 3.5.7** it needs, built once so that nothing which
*links* them needs a build system at all — plus a safe, headless RDP client on top.

```toml
freerdp = { git = "https://github.com/andrewtheguy/libfreerdp-prebuilt", tag = "v3.30.0-…" }
```

No cmake, no pkg-config, no OpenSSL to install, no libclang, and nothing to set in the
environment — not in a Dockerfile, not in a packaging script, not in CI. `build.rs` downloads the
archives for its target from this repository's latest release, checks them, and emits the link
flags in the order the build *measured*. There is one variable, `FREERDP_PREBUILT_DIR`, and it is
an opt-in override for archives you built yourself ([below](#local-loop)); the default path needs
none.

That is the whole point. The alternatives all move work onto every consumer:

| approach | what a consumer must have |
|---|---|
| a system FreeRDP via `pkg-config` | FreeRDP 3 installed and matching, OpenSSL, pkg-config — and runtime dependencies in the finished binary |
| vendored source built by a `build.rs` | cmake, a C toolchain, OpenSSL headers, and several minutes per clean build |
| **this** | curl and tar |

## Layout

```
freerdp.env                      the pins: two versions, two tarball checksums, the release repo
source.sh                        download + verify + unpack (sourced by the scripts below)
build.sh <target>                OpenSSL -> FreeRDP -> dist/<target>/{lib,include} + MANIFEST
sync-prebuilt.sh                 dist/ -> the crate's cache; --headers; --check; --fetch
check-static.sh <binary>         assert a finished binary carries FreeRDP and links none
header-drift.allow               the generated headers permitted to differ between targets
crates/freerdp-prebuilt-sys/     the FFI crate: committed headers, committed bindings, build.rs
crates/freerdp/                  the safe wrapper: a headless RDP client
crates/freerdp-e2e/              a consumer that connects to a real server, run on every target
```

Targets: `macos-arm64`, `linux-x86_64`, `linux-aarch64`. **No Windows** — FreeRDP builds fine on
MSVC, but the OpenSSL half would need its own toolchain setup and no consumer of this repository
targets it. **No musl**, and that is a refusal rather than an oversight: WinPR reaches into
`dlopen`, `getpwuid_r` and the NSS resolver, which is exactly where a glibc-built archive linked
into a musl binary compiles and then misbehaves at run time.

## The chain

Every link is checked, and CI checks all of them:

```
freerdp.env pins two tarball checksums
  -> source.sh refuses anything else, on every run, cached copies included
    -> build.sh compiles them and writes sha256(lib/…) per archive into a MANIFEST
      -> the release publishes the archives plus SHA256SUMS
        -> build.rs verifies the download against SHA256SUMS
          -> and every extracted .a against the MANIFEST beside it, on every path
```

Separately, and this is the part a reviewer can read:

```
include/         is what linux-x86_64's `cmake --install` produced, for the 100 headers
                 of its 261 that `wrapper.h` reaches — asked of the compiler, not curated
src/bindings_apple.rs   is what bindgen 0.72.1 makes of those headers on Apple
src/bindings_linux.rs   … and on Linux                    (both checked, each on its own platform)
```

Tarball checksums rather than a commit pin: both projects publish real release artifacts with
checksums beside them, so the pin covers the exact bytes that get compiled, needs no clone, and
cannot be moved by a retagged branch.

## What the build asserts, rather than assumes

Each of these is a defect that was found by measurement here, not a hypothetical:

- **cmake ignored an option.** `FREERDP_EXTERNAL_SSL_PATH` sits inside an `if(WIN32)` block, so
  off Windows it is silently dropped — and the build then found *Homebrew's* OpenSSL 3.6.3 and
  succeeded. `build.sh` now fails on cmake's "Manually-specified variables were not used"
  warning, and separately greps the configure log for the pinned OpenSSL's path and version.
- **The archives were LLVM bitcode.** FreeRDP turns `CMAKE_INTERPROCEDURAL_OPTIMIZATION` on
  wherever it is supported, and every member of the resulting `.a` is bitcode rather than object
  code — linkable only by a toolchain whose LTO plugin understands it, which is precisely the
  dependency this repository exists to remove. LTO is off, and the build asserts real object code.
- **`no-legacy` broke NTLM.** OpenSSL's legacy provider is where MD4 lives, MD4 is NTLM, and NTLM
  is CredSSP — so every Windows target. The build asserts `ossl_md4_functions` and
  `ossl_rc4128_functions` are in the finished libcrypto.
- **A second zlib or Opus** would be a duplicate-symbol failure in a consumer that links its own.
  `WITH_SMARTCARD_EMULATE` and `WITH_OPUS` are off, and the build asserts neither is defined.
- **The system libraries are measured, then proved.** Undefined symbols minus defined symbols
  across all five archives gives ~300 names that must come from outside; those are mapped to
  libraries and frameworks, and then a C probe is compiled and linked against *exactly* that set
  and run. A framework nobody noticed fails there, on the machine that built it. (That is how
  CoreServices was found: rdpdr's drive-hotplug thread calls `FSEventStreamCreate`.)
- **The channel set** is asserted by symbol, because cmake generates the addin table at configure
  time and a channel dropped by its own `cmake_dependent_option` leaves no warning at all.
- **The deployment target** is read back off the finished archives (`minos 11.0` on every member),
  not trusted from the flag.

## Two bindings files, one per ABI

`winpr/wtypes.h`:

```c
#ifndef __APPLE__
typedef int32_t BOOL;
#else
… typedef bool BOOL;  /* or signed char */
#endif
```

`BOOL` is **four bytes on Linux and one on Apple**, in the return type of nearly every FreeRDP
function and in every callback signature — 866 occurrences in the generated file. So there are two
committed bindings files, selected by `cfg` in `lib.rs`, and each is generated and checked on its
own platform: cross-generating would need each side's sysroot. This is the one place this
repository is structurally different from its siblings, and it is measured rather than argued —
the two files differ in 626 lines.

## The wrapper

`crates/freerdp` is a headless RDP client: screen, cursor, keyboard, mouse, clipboard, resize and
sound.

```rust
let (session, events) = Session::start(Connect { host, username, password, ..Default::default() });
for event in events {
    match event {
        Event::Paint(rect) => session.framebuffer().with(|frame| encode(frame.rows(rect))),
        Event::Ended(result) => break,
        _ => {}
    }
}
```

It lives here rather than in the project that uses it because FreeRDP's embedder API is not a call
but a *lifecycle*: a context whose callbacks fire on FreeRDP's own thread, a framebuffer
reallocated under you on resize, an event loop driven by `WaitForMultipleObjects`, and channel
interfaces that arrive through a pub/sub. Getting that wrong is a use-after-free rather than a
wrong pixel, and it is the same shape for everybody who embeds it.

Dynamic resize is there but **off by default** (`Connect::resize`), because a server answers a
monitor layout by renegotiating the whole session — the most disruptive thing a client can ask
for, and a session that never asks never meets it.

Sound is off by default too (`Connect::audio`), and arrives differently from everything else: not
as an `Event` but through an `AudioSink` called on the FreeRDP thread, so a wave buffer never
queues behind a backlog of paint rectangles. The way in is that `crates/freerdp` **is** the audio
device — `rdpsnd` loads a backend the way an ordinary client loads ALSA, and this one is Rust. Two
things about that are worth knowing before touching it: the subsystem has to be *named*, because a
build with no audio backend falls through to FreeRDP's `fake` device and plays silence without
saying so; and turning sound on makes a Windows host start measuring the link, which is why
`SupportMultitransport` and `SupportHeartbeatPdu` are off — measured, five session deaths out of
five, in `apply_settings`.

What it does **not** do, deliberately: no microphone, no graphics pipeline, no certificate
verification, no file-transfer clipboard, no multiple monitors, and no retrying of a resize. Each
of those is documented where it is decided, with the reason. The EGFX one is worth repeating: against a Windows 11 host with the
graphics pipeline advertised, FreeRDP decoded 21 surface commands with no errors and produced a
framebuffer that summed to *exactly* black; with `SupportGraphicsPipeline = FALSE` the same host,
the same build and the same second painted a real desktop.

## Local loop

```sh
./build.sh <target>             # -> dist/<target>/{lib,include,MANIFEST}
./sync-prebuilt.sh              # -> crates/freerdp-prebuilt-sys/prebuilt/, what cargo will link
cargo run --release -p freerdp-e2e                        # the offline checks
cargo run --release -p freerdp-e2e -- <host> <user> <pw>   # …and a real connection
./check-static.sh target/release/freerdp-e2e
```

`<target>` is the one this machine *is* — `build.sh` does not cross-compile, because cmake and
OpenSSL's `Configure` target the machine they run on. The other targets are a `workflow_dispatch`
on **Build FreeRDP** away, or a container: the two Linux archives here were first built in
`debian:trixie` under Podman and Docker respectively.

`./sync-prebuilt.sh --fetch` pulls the latest release's archives instead, for working offline
afterwards or for a target this machine cannot build. `FREERDP_PREBUILT_DIR=/prefix` is the one
variable a consumer ever sets, and it is never required.

## Bootstrapping

The download-based paths cannot pass before the first release exists, so the order for a fresh
fork is: run **Build FreeRDP** by hand (`workflow_dispatch`, `targets: all`), then **Release
FreeRDP archives**. CI is green from that point on.

`sync-prebuilt.sh --headers` normally takes the committed headers from `dist/linux-x86_64`,
because they should be a property of the project rather than of a developer's laptop —
`SYNC_HEADERS_FROM=macos-arm64` overrides that for the very first commit and says so loudly.

## Which library got linked

```sh
cargo build -vv 2>&1 | grep 'FreeRDP '
```

`build.rs` emits the provenance, the version, the checksum results, the channel set and the
measured system libraries as `cargo:info` lines — not warnings, because this is the normal case
and a warning on every build teaches people to ignore warnings. At run time
`freerdp::freerdp_version()` returns what the archives themselves report and
`freerdp_sys::PREBUILT_VERSION` is what this repository pinned; the e2e binary asserts they agree,
which is how a system library winning the link would be caught.

## Licensing

FreeRDP is **Apache-2.0**, and OpenSSL 3 is **Apache-2.0**. Both licence texts ship inside every
archive and sit at the repository root as `LICENSE.FreeRDP` and `LICENSE.OpenSSL`. Whoever links
these redistributes both, and each licence requires its notice to travel along.
