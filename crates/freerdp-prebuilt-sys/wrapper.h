// What bindgen reads.
//
// Not "every header FreeRDP installs". `include/` holds 261 of them, most reachable only from
// code this repository does not build — the server side, RAIL, smartcard, the codecs behind
// options that are OFF. Including them all would put a surface in bindings.rs that no consumer
// can call and that every consumer would have to keep compiling.
//
// What is here is the headless-embedder surface, plus the channels the wrapper crate binds:
// connect and drive the event loop, receive paint and pointer updates through the GDI, send
// input, exchange clipboard formats, and receive redirected sound.
//
// `rdpgfx.h` is included though nothing calls it yet, and that is deliberate rather than sloppy:
// the archives carry that channel (see build.sh's note on tables.c being generated at configure
// time), so the declarations should be reachable from the same release rather than requiring a
// regeneration when the graphics pipeline is picked up.

// The connection, its settings, and the client entry points that build a context around it.
#include <freerdp/freerdp.h>
#include <freerdp/client.h>
#include <freerdp/settings.h>
#include <freerdp/error.h>
#include <freerdp/version.h>

// The event loop's building blocks: WinPR handles, waits and the pub/sub the channel-connected
// callback arrives through.
#include <winpr/synch.h>
#include <winpr/thread.h>
#include <winpr/collections.h>
#include <freerdp/event.h>

// Pixels. `gdi.h` is the headless framebuffer — `gdi_init` allocates it, `gdi->primary_buffer`
// holds it, and the invalid region on `gdi->primary->hdc->hwnd` is where the damaged rectangle
// comes from. `color.h` is where PIXEL_FORMAT_RGBX32 is defined.
#include <freerdp/gdi/gdi.h>
#include <freerdp/gdi/gfx.h>
#include <freerdp/codec/color.h>
#include <freerdp/update.h>

// The mouse cursor, which arrives as a registered pointer type rather than as an update.
#include <freerdp/graphics.h>
#include <freerdp/pointer.h>

// Input, and the scancode constants the keymap maps onto.
#include <freerdp/input.h>
#include <freerdp/scancode.h>
#include <winpr/input.h>

// Channels: the loader, and the client interfaces bound in a ChannelConnected handler.
#include <freerdp/channels/channels.h>
#include <freerdp/client/channels.h>
#include <freerdp/client/cliprdr.h>
#include <freerdp/client/disp.h>
#include <freerdp/client/rdpgfx.h>

// Sound, which arrives through neither of those routes. `rdpsnd` has no client context to
// subscribe to: it loads a *device* — the thing an ordinary client points at ALSA or CoreAudio —
// and this crate's device is Rust. `rdpsnd.h` is that interface, and `audio.h` is the
// `AUDIO_FORMAT` it negotiates in.
#include <freerdp/client/rdpsnd.h>
#include <freerdp/codec/audio.h>
