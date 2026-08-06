//! Redirected sound, by being FreeRDP's audio *device*.
//!
//! `rdpsnd` is unlike the other two channels this crate binds. `cliprdr` and `disp` publish a
//! client context on the ChannelConnected event, and a consumer fills in callbacks on it.
//! `rdpsnd` publishes nothing: it loads a **device plugin** — the piece an ordinary client points
//! at ALSA, PulseAudio or CoreAudio — and hands the decoded wave buffers to that. So the way to
//! receive sound headlessly is not to subscribe to the channel, it is to *be* a sound card.
//!
//! Three things make that reachable from Rust, and all three are load-bearing:
//!
//! 1. **A device is found through a process-global addin provider.** `rdpsnd_load_device_plugin`
//!    asks `freerdp_load_channel_addin_entry("rdpsnd", <subsystem>, …)`, which calls whatever
//!    `freerdp_register_addin_provider` last installed. [`install_provider`] chains in front of
//!    FreeRDP's own and answers for one name; everything else is delegated, so nothing else about
//!    channel loading changes.
//! 2. **The subsystem has to be named.** With none named, `rdpsnd_process_connect` walks its
//!    compiled-in backends and the archives in this repository end that list with `fake` — a
//!    device that accepts every format and discards every buffer. A build with no audio backend
//!    therefore does not fail loudly, it plays silence. The name goes in as the channel's
//!    `sys:` argument, which is why the channel is registered here by hand rather than left to
//!    `freerdp_client_load_addins`.
//! 3. **The device finds its way back to Rust through the context**, not through its own struct:
//!    `freerdp_rdpsnd_get_context(device->rdpsnd)` is the `rdpContext*` every other callback in
//!    this crate already recovers its bridge from.
//!
//! **Sound does not go through [`Event`](crate::Event).** A wave buffer is handed straight to the
//! sink on the FreeRDP thread, so it cannot queue behind a backlog of paint rectangles — a lost
//! tile is repainted on the next frame and a lost wave buffer is a hole in the sound. The cost is
//! that [`AudioSink`] runs on that thread and must not block: whatever it does, the desktop stops
//! being decoded until it returns.

use freerdp_sys as sys;

use crate::error::Error;
use crate::session::{bridge, guarded, Bridge};
use std::ffi::CStr;
use std::sync::{Arc, OnceLock};

/// Linear PCM: the only thing this device accepts and the only thing a sink is handed.
///
/// PCM because it is the one format MS-RDPEA requires both ends to support, so asking for a
/// compressed one would make a session depend on what a particular Windows version happens to
/// offer. FreeRDP will happily transcode — `freerdp_dsp_decode` sits behind
/// [`AudioSink`](AudioSink) for formats a device refuses — but only into codecs the archive was
/// built with, and this one was built with none of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
}

impl AudioFormat {
    /// 44 100 Hz, 16-bit, stereo — what every RDP server offers, and the safe default ask.
    pub const CD: Self = Self { channels: 2, sample_rate: 44_100, bits_per_sample: 16 };

    /// Bytes in one sample across every channel — MS-RDPEA's `nBlockAlign`.
    pub const fn block_align(self) -> u16 {
        self.channels * (self.bits_per_sample / 8)
    }

    /// Bytes a second of this occupies — `nAvgBytesPerSec`, and 176 400 for [`Self::CD`].
    pub const fn byte_rate(self) -> u32 {
        self.sample_rate * self.block_align() as u32
    }

    /// Whether an offered format is exactly this one.
    fn matches(self, format: &sys::AUDIO_FORMAT) -> bool {
        format.wFormatTag == sys::WAVE_FORMAT_PCM as u16
            && format.nChannels == self.channels
            && format.nSamplesPerSec == self.sample_rate
            && format.wBitsPerSample == self.bits_per_sample
    }
}

/// Where a session's redirected sound goes.
///
/// **Every method runs on the FreeRDP thread**, inside `freerdp_check_event_handles`, and must
/// not block: the same thread decodes the desktop. A queue whose push cannot wait is the shape
/// this is for.
pub trait AudioSink: Send + Sync {
    /// The server announced the formats it can send, and `accepted` is how many of them this
    /// session will take — which, since [`Audio::format`] names exactly one, is 0 or 1.
    ///
    /// This is the signal that redirected sound is **negotiated**, as distinct from arriving. It
    /// fires as the channel comes up, whether or not the remote ever makes a noise, so it is the
    /// only thing that separates "set up and the desktop is quiet" from "never set up at all" —
    /// and a `0` here says the session will stay silent however loud the remote gets, because the
    /// server offers nothing in the format that was asked for.
    fn negotiated(&self, accepted: usize);

    /// The channel negotiated and is about to deliver buffers in `format`.
    ///
    /// The format is always the one [`Audio::format`] asked for — a device that is offered
    /// anything else refuses it — so this is a signal that sound is really set up, rather than a
    /// value to branch on. It can arrive more than once in a session, since a server may close
    /// and reopen the device.
    fn opened(&self, format: AudioFormat);

    /// One complete wave buffer, in the format last announced by [`Self::opened`].
    fn wave(&self, samples: &[u8]);

    /// The device closed. Sound may resume later on the same session, through another
    /// [`Self::opened`].
    fn closed(&self);
}

/// A session's sound: what to ask for, and where to put it.
#[derive(Clone)]
pub struct Audio {
    /// The one format to accept. A server that offers nothing matching gets an empty client
    /// format list, and the session plays no sound rather than sound at the wrong rate.
    pub format: AudioFormat,
    pub sink: Arc<dyn AudioSink>,
}

impl std::fmt::Debug for Audio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Audio").field("format", &self.format).finish_non_exhaustive()
    }
}

/// The subsystem name this crate's device answers to, and the value of the channel's `sys:`
/// argument.
///
/// It only has to be a name no FreeRDP backend uses; `rust` reads correctly in the line rdpsnd
/// logs on success — "Loaded rust backend for rdpsnd".
const SUBSYSTEM: &CStr = c"rust";

/// The `sys:` argument spelled the way `rdpsnd_process_addin_args` parses it.
///
/// Spelled out rather than built from [`SUBSYSTEM`], because it has to be a `CStr` a channel
/// argument list can point at and there is no const way to concatenate one. The name is therefore
/// written twice, and the assertion below is what stops the two drifting: a mismatch is the
/// failure that loads FreeRDP's `fake` device and plays silence without saying so, so it is worth
/// catching at compile time rather than by ear.
pub(crate) const SUBSYSTEM_ARG: &CStr = c"sys:rust";

const _: () = {
    let (arg, name) = (SUBSYSTEM_ARG.to_bytes(), SUBSYSTEM.to_bytes());
    assert!(arg.len() == 4 + name.len(), "SUBSYSTEM_ARG is not `sys:` followed by SUBSYSTEM");
    assert!(
        arg[0] == b's' && arg[1] == b'y' && arg[2] == b's' && arg[3] == b':',
        "SUBSYSTEM_ARG does not start with the `sys:` prefix rdpsnd parses"
    );
    let mut i = 0;
    while i < name.len() {
        assert!(arg[4 + i] == name[i], "SUBSYSTEM_ARG names a different subsystem to SUBSYSTEM");
        i += 1;
    }
};

/// FreeRDP's own provider, kept so everything that is not this device still loads.
///
/// Read **once**, and that is the whole of why this is a `OnceLock` rather than a field:
/// `freerdp_client_context_new` reinstalls `freerdp_channels_load_static_addin_entry` on every
/// call, so the first read is FreeRDP's and a later one could be ours — which would make the
/// delegation below call itself for ever.
static UPSTREAM: OnceLock<sys::FREERDP_LOAD_CHANNEL_ADDIN_ENTRY_FN> = OnceLock::new();

/// Chain this crate's addin provider in front of FreeRDP's, so `sys:rust` resolves.
///
/// Called once per session that wants sound, as late as possible — the channels are loaded during
/// `freerdp_connect`, and the provider is a **process global**. That global is the one thing here
/// this crate cannot make per-session: a second `freerdp_client_context_new` overwrites it, so
/// starting another session while this one is still inside `freerdp_connect` can cost this one
/// its device, and its sound. Sessions that overlap only after both have connected are unaffected,
/// because by then each has already loaded its own.
pub(crate) fn install_provider() -> Result<(), Error> {
    // SAFETY: reads the process-global provider FreeRDP installed in
    // `freerdp_client_context_new`. No context is touched.
    UPSTREAM.get_or_init(|| unsafe { sys::freerdp_get_current_addin_provider() });

    // SAFETY: `addin_provider` matches `FREERDP_LOAD_CHANNEL_ADDIN_ENTRY_FN` and lives for the
    // lifetime of the process.
    if unsafe { sys::freerdp_register_addin_provider(Some(addin_provider), 0) } != 0 {
        return Err(Error::local("FreeRDP refused this crate's audio addin provider"));
    }
    Ok(())
}

/// Answer for `rdpsnd`'s `rust` subsystem; delegate everything else unchanged.
unsafe extern "C" fn addin_provider(
    name: sys::LPCSTR,
    subsystem: sys::LPCSTR,
    kind: sys::LPCSTR,
    flags: sys::DWORD,
) -> sys::PVIRTUALCHANNELENTRY {
    guarded("the addin provider", None, || {
        // SAFETY: FreeRDP passes NUL-terminated strings or null, and both are handled.
        let named = |ptr: sys::LPCSTR, want: &CStr| {
            !ptr.is_null() && unsafe { CStr::from_ptr(ptr) } == want
        };
        if named(name, c"rdpsnd") && named(subsystem, SUBSYSTEM) {
            // The device entry has its own signature — `PFREERDP_RDPSND_DEVICE_ENTRY` — and the
            // provider returns the generic channel one, so rdpsnd casts it back before calling.
            // This is `WINPR_FUNC_PTR_CAST` in `rdpsnd_load_device_plugin`, from the other side.
            //
            // SAFETY: sound only because the pair is chosen together: rdpsnd casts whatever it
            // gets for this name to `PFREERDP_RDPSND_DEVICE_ENTRY`, and `subsystem_entry` is
            // declared with exactly that signature.
            return Some(unsafe {
                std::mem::transmute::<
                    unsafe extern "C" fn(sys::PFREERDP_RDPSND_DEVICE_ENTRY_POINTS) -> sys::UINT,
                    unsafe extern "C" fn(sys::PCHANNEL_ENTRY_POINTS) -> sys::BOOL,
                >(subsystem_entry)
            });
        }
        match UPSTREAM.get() {
            // SAFETY: FreeRDP's own provider, called with the arguments it was given.
            Some(Some(upstream)) => unsafe { upstream(name, subsystem, kind, flags) },
            _ => None,
        }
    })
}

/// The device, with FreeRDP's struct first so the two pointers are interchangeable.
///
/// Everything else about the device lives on the [`Bridge`], reached through the context, so this
/// is the whole of it: the callbacks below are stateless and the session already has the state.
#[repr(C)]
struct Device {
    plugin: sys::rdpsndDevicePlugin,
}

/// Recover the session's bridge and audio configuration from a device pointer.
///
/// The [`Audio`] comes back **cloned** rather than borrowed, which is one `Arc` bump: the
/// callbacks below need the sink and the bridge at the same time, and the sink lives inside the
/// bridge.
///
/// # Safety
///
/// `device` must be one this crate registered, and still alive — true in every callback, since
/// rdpsnd calls none of them after `Free`.
unsafe fn audio<'a>(device: *mut sys::rdpsndDevicePlugin) -> Option<(&'a mut Bridge, Audio)> {
    if device.is_null() {
        return None;
    }
    // SAFETY: `rdpsnd` is set by `rdpsnd_register_device_plugin` before any callback fires.
    let plugin = unsafe { (*device).rdpsnd };
    if plugin.is_null() {
        return None;
    }
    // SAFETY: `plugin` is rdpsnd's own, and this only reads the context it was created with.
    let ctx = unsafe { sys::freerdp_rdpsnd_get_context(plugin) };
    // SAFETY: that context is the one this crate created — rdpsnd was loaded onto it.
    let bridge = unsafe { bridge(ctx) }?;
    // A device on a session with no audio configured cannot happen: the channel is only
    // registered when there is. Handled rather than asserted, since the cost is one `?`.
    let config = bridge.audio.clone()?;
    Some((bridge, config))
}

/// Whether this crate's device would accept a format, which is also how the client's format list
/// is built: `rdpsnd_select_supported_audio_formats` keeps exactly the server formats this
/// returns true for.
unsafe extern "C" fn format_supported(
    device: *mut sys::rdpsndDevicePlugin,
    format: *const sys::AUDIO_FORMAT,
) -> sys::BOOL {
    guarded("rdpsnd FormatSupported", 0, || {
        if format.is_null() {
            return 0;
        }
        // SAFETY: the caller's device and a format rdpsnd owns for the duration of the call.
        let Some((_, audio)) = (unsafe { audio(device) }) else { return 0 };
        // SAFETY: as above.
        if audio.format.matches(unsafe { &*format }) {
            1
        } else {
            0
        }
    })
}

/// The server's format list, which arrives as the channel comes up.
///
/// rdpsnd calls this from `rdpsnd_recv_server_audio_formats_pdu`, before any wave and whether or
/// not one ever follows, which is what makes it the honest "sound is set up" signal. It is also
/// the one observation that distinguishes this device from the `fake` one a build with no named
/// subsystem would have loaded instead.
unsafe extern "C" fn server_format_announce(
    device: *mut sys::rdpsndDevicePlugin,
    formats: *const sys::AUDIO_FORMAT,
    count: usize,
) -> sys::UINT {
    guarded("rdpsnd ServerFormatAnnounce", sys::CHANNEL_RC_OK, || {
        // SAFETY: as `format_supported`.
        let Some((_, audio)) = (unsafe { audio(device) }) else { return sys::CHANNEL_RC_OK };
        let offered: &[sys::AUDIO_FORMAT] = if formats.is_null() || count == 0 {
            &[]
        } else {
            // SAFETY: rdpsnd owns this array for the duration of the call and `count` is its
            // length.
            unsafe { std::slice::from_raw_parts(formats, count) }
        };
        audio.sink.negotiated(offered.iter().filter(|f| audio.format.matches(f)).count());
        sys::CHANNEL_RC_OK
    })
}

/// The channel settled on a format and is about to send buffers.
unsafe extern "C" fn open(
    device: *mut sys::rdpsndDevicePlugin,
    format: *const sys::AUDIO_FORMAT,
    _latency: sys::UINT32,
) -> sys::BOOL {
    guarded("rdpsnd Open", 0, || {
        if format.is_null() {
            return 0;
        }
        // SAFETY: as `format_supported`.
        let Some((bridge, audio)) = (unsafe { audio(device) }) else { return 0 };
        // Checked again, and not defensively. `rdpsnd_ensure_device_is_open` opens a device with
        // a format of its own invention when `FormatSupported` said no — PCM at the *server's*
        // rate, to be transcoded on the way in — and accepting that would hand the sink 48 kHz
        // where it asked for 44.1 and said nothing. Refusing is the honest failure: the wave is
        // dropped, `opened` is never called, and the caller sees no negotiated format.
        //
        // SAFETY: as above.
        if !audio.format.matches(unsafe { &*format }) {
            return 0;
        }
        // One device at a time. rdpsnd is registered as both a static channel and a dynamic one,
        // because which of the two a server drives is the server's choice, and each loads a
        // device of its own. Nothing in MS-RDPEA says a server may drive both at once and none
        // does — but if one ever did, both would push buffers into the same sink and the result
        // would be interleaved noise with nothing anywhere to explain it. First open wins.
        if !bridge.audio_device.is_null() && bridge.audio_device != device {
            return 0;
        }
        bridge.audio_device = device;
        audio.sink.opened(audio.format);
        1
    })
}

/// One wave buffer, already decoded and in the negotiated format.
///
/// The return value is a **latency in milliseconds**, not a status: rdpsnd adds it to the
/// timestamp of the second Wave Confirm PDU, which is how a server measures how far behind its
/// client's playback is. Zero is the truthful answer here — this device does not play anything,
/// it hands the bytes on — and the real playback delay is in whatever the consumer feeds, which
/// the RDP server can do nothing about anyway.
unsafe extern "C" fn play(
    device: *mut sys::rdpsndDevicePlugin,
    data: *const sys::BYTE,
    size: usize,
) -> sys::UINT {
    guarded("rdpsnd Play", 0, || {
        if data.is_null() || size == 0 {
            return 0;
        }
        // SAFETY: as `format_supported`.
        let Some((bridge, audio)) = (unsafe { audio(device) }) else { return 0 };
        if bridge.audio_device != device {
            // A device that lost the claim to another stays silent — but a *vacant* claim is
            // adopted, because the device rdpsnd plays after reloading the channel is one it
            // will never call `Open` on. `rdpsnd_on_close` frees the old device and nulls
            // `rdpsnd->device`, but never resets the plugin's `isOpen` or `wCurrentFormatNo`;
            // when the channel comes back — a resize's Deactivation-Reactivation Sequence is
            // the trigger that surfaced this, with the server reopening the audio channel on
            // the other side of it — the first wave finds `isOpen` still true and the format
            // number unchanged (with exactly one advertised format it is always 0), so
            // `rdpsnd_ensure_device_is_open` skips `Open` entirely and calls `Play` on a
            // device that owns nothing. Refusing that wave was a session whose sound never
            // came back after a resize.
            //
            // Adopting is sound because the format is not in doubt: a wave's format number
            // indexes the client format list this same device built through `FormatSupported`,
            // which accepts nothing but `audio.format`. And it cannot race a legitimate owner:
            // only `open` and this line claim, both on the FreeRDP thread, and both only when
            // the claim is empty.
            if !bridge.audio_device.is_null() {
                return 0;
            }
            bridge.audio_device = device;
            audio.sink.opened(audio.format);
        }
        // SAFETY: rdpsnd owns this buffer for the duration of the call and `size` is its length.
        audio.sink.wave(unsafe { std::slice::from_raw_parts(data, size) });
        0
    })
}

/// The server stopped playing, or is about to reopen with another format.
unsafe extern "C" fn close(device: *mut sys::rdpsndDevicePlugin) {
    guarded("rdpsnd Close", (), || {
        // SAFETY: as `format_supported`.
        let Some((bridge, audio)) = (unsafe { audio(device) }) else { return };
        // Only the device that opened reports closing. rdpsnd calls `Close` unconditionally
        // before every `Open`, including the first, so an unclaimed device closing is the normal
        // case rather than an error.
        if bridge.audio_device != device {
            return;
        }
        bridge.audio_device = std::ptr::null_mut();
        audio.sink.closed();
    })
}

/// Reclaim the device. rdpsnd calls this once, when the channel is torn down.
unsafe extern "C" fn free(device: *mut sys::rdpsndDevicePlugin) {
    guarded("rdpsnd Free", (), || {
        if device.is_null() {
            return;
        }
        // Withdrawn from the bridge *before* the allocation goes, because rdpsnd frees an open
        // device — the teardown path calls `Free` without a matching `Close` — and what would be
        // left behind is a dangling pointer that the next device's `Open` compares itself against
        // and loses to. That is a session whose sound never comes back after the channel is
        // reloaded. Only when it still refers to this device, for the same reason `close` checks:
        // a device that never won the claim must not clear somebody else's.
        //
        // SAFETY: the caller's device, still alive until the drop below.
        if let Some((bridge, _)) = unsafe { audio(device) } {
            if bridge.audio_device == device {
                bridge.audio_device = std::ptr::null_mut();
            }
        }
        // SAFETY: `Device` starts with the plugin struct, so the two pointers are the same
        // address, and this is the box `subsystem_entry` leaked. Reclaimed exactly once: rdpsnd
        // makes no call on a device after freeing it.
        drop(unsafe { Box::from_raw(device as *mut Device) });
    })
}

/// Volume is accepted and discarded.
///
/// A consumer that re-encodes for somebody else's speakers has its own volume control, and
/// scaling the samples here would only make that one lossier. Refusing instead is not an option
/// worth taking: `rdpsnd_recv_volume_pdu` treats a `SetVolume` that returns false as a channel
/// error and tears the session down.
unsafe extern "C" fn set_volume(
    _device: *mut sys::rdpsndDevicePlugin,
    _value: sys::UINT32,
) -> sys::BOOL {
    1
}

/// Full volume on both channels — the two 16-bit values MS-RDPEA packs into one word.
unsafe extern "C" fn get_volume(_device: *mut sys::rdpsndDevicePlugin) -> sys::UINT32 {
    0xFFFF_FFFF
}

/// What the addin provider hands back: rdpsnd calls this to build a device.
///
/// Mirrors `fake_freerdp_rdpsnd_client_subsystem_entry` and every other backend's entry point.
/// The channel's arguments are not parsed — the only one this device could take is the `sys:`
/// that selected it.
unsafe extern "C" fn subsystem_entry(
    entry_points: sys::PFREERDP_RDPSND_DEVICE_ENTRY_POINTS,
) -> sys::UINT {
    guarded("rdpsnd subsystem entry", sys::CHANNEL_RC_INITIALIZATION_ERROR, || {
        if entry_points.is_null() {
            return sys::CHANNEL_RC_INITIALIZATION_ERROR;
        }
        let mut device = Box::new(Device {
            // SAFETY: `rdpsndDevicePlugin` is all pointers and function pointers, for which the
            // all-zero pattern is a valid null. rdpsnd reads every one through `IFCALL`, which
            // checks for null first.
            plugin: unsafe { std::mem::zeroed() },
        });
        device.plugin.FormatSupported = Some(format_supported);
        device.plugin.ServerFormatAnnounce = Some(server_format_announce);
        device.plugin.Open = Some(open);
        device.plugin.Play = Some(play);
        device.plugin.Close = Some(close);
        device.plugin.Free = Some(free);
        device.plugin.SetVolume = Some(set_volume);
        device.plugin.GetVolume = Some(get_volume);

        // Leaked on purpose: from here the device belongs to rdpsnd, which returns it through
        // `Free`. The one path that would leak it is `pRegisterRdpsndDevice` refusing — it does
        // that only when a device is already registered on this plugin, and `rdpsnd_process_connect`
        // loads exactly one. FreeRDP's own backends leak in the same case.
        let device = Box::into_raw(device);
        // SAFETY: `entry_points` is live for this call and its register function is rdpsnd's own.
        unsafe {
            let register = (*entry_points).pRegisterRdpsndDevice;
            let Some(register) = register else {
                drop(Box::from_raw(device));
                return sys::CHANNEL_RC_INITIALIZATION_ERROR;
            };
            register((*entry_points).rdpsnd, device as *mut sys::rdpsndDevicePlugin);
        }
        sys::CHANNEL_RC_OK
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offered(tag: u16, channels: u16, rate: u32, bits: u16) -> sys::AUDIO_FORMAT {
        sys::AUDIO_FORMAT {
            wFormatTag: tag,
            nChannels: channels,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: 0,
            nBlockAlign: 0,
            wBitsPerSample: bits,
            cbSize: 0,
            data: std::ptr::null_mut(),
        }
    }

    #[test]
    fn cd_quality_is_what_its_name_says() {
        assert_eq!(AudioFormat::CD.block_align(), 4);
        assert_eq!(AudioFormat::CD.byte_rate(), 176_400);
    }

    /// The exact-match rule, which is the whole of the format negotiation: rdpsnd keeps the
    /// server formats this accepts and sends that list as the client's.
    #[test]
    fn only_the_asked_for_pcm_is_accepted() {
        let want = AudioFormat::CD;
        assert!(want.matches(&offered(sys::WAVE_FORMAT_PCM as u16, 2, 44_100, 16)));
        // A rate the sink did not ask for is refused rather than resampled — see `open`.
        assert!(!want.matches(&offered(sys::WAVE_FORMAT_PCM as u16, 2, 48_000, 16)));
        assert!(!want.matches(&offered(sys::WAVE_FORMAT_PCM as u16, 1, 44_100, 16)));
        assert!(!want.matches(&offered(sys::WAVE_FORMAT_PCM as u16, 2, 44_100, 8)));
        // Everything compressed. WAVE_FORMAT_ADPCM is what a Windows host offers first, and
        // accepting it would need a codec the archives are not built with.
        assert!(!want.matches(&offered(0x0002, 2, 44_100, 16)));
    }

    /// A sink that asks for something other than CD quality gets that and only that.
    #[test]
    fn the_format_is_the_sinks_to_choose() {
        let want = AudioFormat { channels: 1, sample_rate: 22_050, bits_per_sample: 16 };
        assert_eq!(want.block_align(), 2);
        assert!(want.matches(&offered(sys::WAVE_FORMAT_PCM as u16, 1, 22_050, 16)));
        assert!(!want.matches(&offered(sys::WAVE_FORMAT_PCM as u16, 2, 44_100, 16)));
    }
}
