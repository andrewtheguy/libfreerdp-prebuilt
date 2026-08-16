//! A redirected microphone, by being the MS-RDPEAI endpoint ourselves.
//!
//! This is the camera's mirror image and its close cousin in construction. Like `rdpecam`, the
//! `audin` channel is compiled **out** of the archives (see build.sh) — FreeRDP's own audin is a
//! capture stack over ALSA/PulseAudio/CoreAudio, and this crate's microphone is none of those: the
//! embedder holds the samples and needs a wire to put them on. What the archives do carry is
//! `drdynvc`, and MS-RDPEAI is nothing but messages over one dynamic virtual channel. So this
//! module implements the protocol itself, in Rust, exactly as [`camera`](crate::camera) does:
//!
//! 1. **The plugin is found through the same process-global addin provider as the camera and the
//!    sound device.** `drdynvc` loads each dynamic channel named in the settings through
//!    `freerdp_load_channel_addin_entry(name, NULL, NULL, …)`; the provider in `audio.rs` answers
//!    for `audin` with [`plugin_entry`] and delegates everything else.
//! 2. **The channel is the server's to create.** The plugin registers one listener — the fixed
//!    `AUDIO_INPUT` name — and the server connects to it when a host-side application wants the
//!    microphone. So the channel opening at all is the "an app is listening" signal, and the
//!    protocol's OPEN is "start capturing".
//! 3. **The plugin finds its way back to Rust through the context**: the DVC entry points carry
//!    `GetRdpContext`, and the bridge on the end of that context holds the session's
//!    [`Microphone`].
//!
//! Where the camera meters every frame against a server credit, the microphone does not: after the
//! host's OPEN the client simply streams PCM until a FormatChange or the channel closes. So this is
//! the simpler of the two — one channel, no credits, no keyframes, and PCM rather than an encoded
//! bitstream. The direction is the same as the camera's: browser to host.
//!
//! Like camera and sound, microphone traffic does not go through [`Event`](crate::Event): the
//! host's open/close decisions are handed to [`MicEvents`] on a FreeRDP thread as they arrive, and
//! the embedder's samples go straight onto the channel from whatever thread calls
//! [`Microphone::sample`].

use freerdp_sys as sys;

use crate::session::guarded;
use std::ffi::CStr;
use std::sync::{Arc, Mutex};

/// The MS-RDPEAI version this endpoint advertises — its own maximum, exactly as FreeRDP's audin
/// client does. The client answers a server VERSION with *this* number (not the server's), and
/// declines to answer only when the server asks for something newer than this. The negotiated value
/// is otherwise unused on the wire, so nothing stores it — the reply and the `negotiated` event both
/// carry this constant.
const SNDIN_VERSION: u32 = 2;

/// The audio-input channel's name — fixed by the protocol, `AUDIN_DVC_CHANNEL_NAME`.
const AUDIO_INPUT_CHANNEL: &CStr = c"AUDIO_INPUT";

/// `WAVE_FORMAT_PCM` — the plain wave format tag this endpoint accepts. The browser produces linear
/// PCM, so a compressed format offered by the host is one this endpoint declines; the host then
/// picks PCM from the intersection, which every host offers.
const WAVE_FORMAT_PCM: u16 = 0x0001;

/// `WAVE_FORMAT_EXTENSIBLE` — how modern Windows enumerates almost all of its capture formats. The
/// tag itself says nothing; the real format is in the SubFormat GUID that follows, and when that
/// GUID is `KSDATAFORMAT_SUBTYPE_PCM` the entry is linear PCM as surely as a `WAVE_FORMAT_PCM` one.
/// A capture endpoint on Windows 11 offers its 44.1/48 kHz rates *only* this way, so an endpoint
/// that recognises just `WAVE_FORMAT_PCM` echoes back a near-empty list and the host, finding
/// nothing it can open the recording application's stream with, never sends an OPEN at all.
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

// The subtype that marks an EXTENSIBLE format as PCM is `KSDATAFORMAT_SUBTYPE_PCM`
// (`{00000001-0000-0010-8000-00aa00389b71}`): its `Data1` is `0x00000001`, so a little-endian read of
// the GUID's first `u16` is `WAVE_FORMAT_PCM` — all `parse_formats` checks to accept the entry.

/// The sample depth this endpoint accepts. The browser converts its capture to signed 16-bit, so a
/// host format at another depth is one we do not advertise back.
const PCM_BITS_PER_SAMPLE: u16 = 16;

/// Message ids, from MS-RDPEAI 2.2 (`MSG_SNDIN_*` in FreeRDP's `audin_main.h`).
mod msg {
    pub const VERSION: u8 = 0x01;
    pub const FORMATS: u8 = 0x02;
    pub const OPEN: u8 = 0x03;
    pub const OPEN_REPLY: u8 = 0x04;
    pub const DATA_INCOMING: u8 = 0x05;
    pub const DATA: u8 = 0x06;
    pub const FORMAT_CHANGE: u8 = 0x07;
}

/// `S_OK` — the OpenReply result for an OPEN this endpoint accepted.
const RESULT_OK: u32 = 0;

/// `E_FAIL` — the OpenReply result for an OPEN this endpoint cannot honour (an index into a format
/// it never advertised). Any failure HRESULT tells the host the stream did not open; a reply is what
/// matters, so the host is not left waiting on one.
const RESULT_FAIL: u32 = 0x8000_4005;

/// A PCM capture format: what the host asked the microphone to produce.
///
/// Always linear PCM at [`PCM_BITS_PER_SAMPLE`]; only the channel count and sample rate vary, and
/// they are the two numbers the embedder needs to configure its capture. `nBlockAlign` and
/// `nAvgBytesPerSec` fall out of these, so they are computed rather than stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MicFormat {
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
}

impl MicFormat {
    /// Bytes in one sample across every channel — MS-RDPEAI's `nBlockAlign`. Saturating, because
    /// `channels` is the host's untrusted number: an absurd count must not overflow the `u16` field
    /// (a panic on the channel thread in debug, a wrapped value in release) on its way to the wire.
    fn block_align(self) -> u16 {
        self.channels.saturating_mul(self.bits_per_sample / 8)
    }

    /// Bytes a second of this occupies — `nAvgBytesPerSec`. Saturating for the same reason, against a
    /// host `sample_rate` large enough to overflow the `u32` field.
    fn byte_rate(self) -> u32 {
        self.sample_rate.saturating_mul(self.block_align() as u32)
    }
}

/// Where the host's microphone decisions go.
///
/// **Every method runs on a FreeRDP thread** and must not block and must not call back into
/// [`Microphone`] — the events are emitted with the microphone's own lock released, but the thread
/// they run on is the channel's, so a queue whose push cannot wait is the shape this is for,
/// exactly as with [`CameraEvents`](crate::CameraEvents) and [`AudioSink`](crate::AudioSink).
pub trait MicEvents: Send + Sync {
    /// The host opened the audio-input channel and a version was agreed: microphone redirection is
    /// on offer at all. A host that never fires this has the channel disabled — policy, an old
    /// server, or a gateway in the way — and that absence is the only observable difference between
    /// "declined" and "still coming up".
    fn negotiated(&self, version: u32);
    /// The host opened the stream and will read samples in `format` from now on — an application
    /// over there opened the microphone. Samples handed to [`Microphone::sample`] before this are
    /// dropped. May fire again with a new format after a FormatChange.
    fn opened(&self, format: MicFormat);
    /// The host stopped reading — the channel closed. Capture can stop; another `opened` may
    /// follow on the same session if the host reopens.
    fn closed(&self);
}

/// A raw channel pointer that may cross the state lock.
///
/// SAFETY: only dereferenced while the [`MicState`] lock is held, and cleared (under that same
/// lock) by `OnClose`/`Terminated` before dvcman frees the object — so no dereference can outlive
/// the channel. This is the camera module's `Chan`, verbatim in intent.
struct Chan(*mut sys::IWTSVirtualChannel);
// SAFETY: see `Chan` — the pointer is only used under the state lock.
unsafe impl Send for Chan {}

impl Chan {
    /// Write one whole protocol message. A `false` is a channel refusing — closing or closed — and
    /// the caller treats it like the close it is about to observe.
    fn write(&self, buf: &[u8]) -> bool {
        // SAFETY: the caller holds the state lock, so the channel is alive (see `Chan`), and
        // `Write` copies the buffer before returning.
        let ok = unsafe {
            let Some(write) = (*self.0).Write else { return false };
            write(self.0, buf.len() as sys::ULONG, buf.as_ptr(), std::ptr::null_mut())
                == sys::CHANNEL_RC_OK
        };
        // `write` runs per captured buffer, so build the trace string only when tracing is on —
        // otherwise this allocates a formatted line for every buffer and throws it away.
        if trace_enabled() {
            trace(&format!(
                "sent 0x{:02x}, {} byte(s), accepted={ok}",
                buf.first().copied().unwrap_or(0),
                buf.len()
            ));
        }
        ok
    }
}

/// Whether `FREERDP_AUDIN_TRACE` was set, read once. Caching it lets the per-buffer write path skip
/// building a trace string when tracing is off without an env lookup each time.
fn trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("FREERDP_AUDIN_TRACE").is_some())
}

/// Wire tracing for debugging against a real server: `FREERDP_AUDIN_TRACE=1` prints every
/// MS-RDPEAI message either way on stderr — the same lever `FREERDP_ECAM_TRACE` is for the camera.
fn trace(line: &str) {
    if trace_enabled() {
        eprintln!("audin: {line}");
    }
}

/// Everything the protocol remembers, under the one lock.
struct MicState {
    /// The channel, present between its OnOpen and its OnClose.
    channel: Option<Chan>,
    /// The PCM formats this endpoint advertised, in the order it advertised them — an OPEN or a
    /// FormatChange names one by index into this list.
    formats: Vec<MicFormat>,
    /// The format the host opened with, present between OPEN and close.
    open: Option<MicFormat>,
}

impl MicState {
    fn new() -> Self {
        Self { channel: None, formats: Vec::new(), open: None }
    }

    /// Stop streaming. Returns whether a stream was running, which is whether anyone needs telling.
    fn stop(&mut self) -> bool {
        self.open.take().is_some()
    }
}

/// The shared half a [`Microphone`] hands the plugin: configuration outside the lock, protocol
/// state inside it.
struct MicShared {
    events: Arc<dyn MicEvents>,
    state: Mutex<MicState>,
}

impl MicShared {
    /// The state, poison-proof for the same reason as the camera's: the lock only guards plain
    /// state, and refusing to lock after a panicking writer would silently end the microphone for
    /// the session.
    fn lock(&self) -> std::sync::MutexGuard<'_, MicState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A session's microphone: feed it PCM and hear the host's decisions.
///
/// Cloneable and callable from any thread. One per [`Connect`](crate::Connect). Unlike the camera
/// there is no plug/unplug: MS-RDPEAI has no device layer, so the microphone is present on the host
/// for as long as the channel is registered, and whether real audio flows is simply whether the
/// embedder is feeding [`Self::sample`].
#[derive(Clone)]
pub struct Microphone {
    shared: Arc<MicShared>,
}

impl std::fmt::Debug for Microphone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Microphone").finish_non_exhaustive()
    }
}

impl Microphone {
    /// A microphone whose host-side decisions go to `events`.
    pub fn new(events: Arc<dyn MicEvents>) -> Self {
        Self { shared: Arc::new(MicShared { events, state: Mutex::new(MicState::new()) }) }
    }

    /// One buffer of PCM, in the format last announced by [`MicEvents::opened`]. Returns whether it
    /// was sent — a `false` means no stream is open or the channel is gone, and the caller can drop
    /// the buffer, since a microphone sample is worthless late.
    ///
    /// Each buffer is two PDUs on the wire — a DataIncoming header then the Data itself — which is
    /// what FreeRDP's own audin sends per captured buffer.
    pub fn sample(&self, pcm: &[u8]) -> bool {
        let st = self.shared.lock();
        if st.open.is_none() {
            return false;
        }
        let Some(ch) = &st.channel else { return false };
        // The incoming-data header first, then the data. A refused write is the close the caller is
        // about to observe; report it as a drop rather than pretending the sample landed.
        if !ch.write(&wire::data_incoming()) {
            return false;
        }
        ch.write(&wire::data(pcm))
    }
}

// ------------------------------------------------------------------ wire format
//
// Builders and parsers for the handful of MS-RDPEAI messages this endpoint exchanges. Pure byte
// work, which is what makes the protocol testable without a server. Little-endian throughout, one
// message per channel write, and — unlike the camera's [version, id] — a bare one-byte MessageId
// header, because MS-RDPEAI carries its version once in the VERSION PDU rather than on every
// message.

mod wire {
    use super::*;

    /// The client's VERSION reply: MessageId then a UINT32 version.
    pub fn version(v: u32) -> Vec<u8> {
        let mut out = vec![msg::VERSION];
        out.extend_from_slice(&v.to_le_bytes());
        out
    }

    /// One AUDIO_FORMAT on the wire, the 18-byte WAVEFORMATEX with no extra bytes (PCM has none).
    fn audio_format(f: &MicFormat) -> [u8; 18] {
        let mut out = [0u8; 18];
        out[0..2].copy_from_slice(&WAVE_FORMAT_PCM.to_le_bytes());
        out[2..4].copy_from_slice(&f.channels.to_le_bytes());
        out[4..8].copy_from_slice(&f.sample_rate.to_le_bytes());
        out[8..12].copy_from_slice(&f.byte_rate().to_le_bytes());
        out[12..14].copy_from_slice(&f.block_align().to_le_bytes());
        out[14..16].copy_from_slice(&f.bits_per_sample.to_le_bytes());
        // cbSize = 0: no extra format bytes follow.
        out[16..18].copy_from_slice(&0u16.to_le_bytes());
        out
    }

    /// The client's Sound Formats PDU: the formats this endpoint accepts, from the host's list.
    ///
    /// `cbSizeFormatsPacket` is the length of the **whole PDU** — the 9-byte header (MessageId,
    /// NumFormats, cbSizeFormatsPacket) plus every AUDIO_FORMAT. This is not cosmetic: Windows sends
    /// its own list this way (an observed `cbSizeFormatsPacket` of 829 for 820 format bytes is
    /// `820 + 9`) and validates the value on the reply, so an endpoint that writes only the
    /// formats' length has its Sound Formats PDU rejected and is never sent an OPEN.
    pub fn formats(list: &[MicFormat]) -> Vec<u8> {
        let total = (9 + list.len() * 18) as u32;
        let mut out = vec![msg::FORMATS];
        out.extend_from_slice(&(list.len() as u32).to_le_bytes());
        out.extend_from_slice(&total.to_le_bytes());
        for f in list {
            out.extend_from_slice(&audio_format(f));
        }
        out
    }

    /// The client's OpenReply PDU: a MessageId and an HRESULT.
    pub fn open_reply(result: u32) -> Vec<u8> {
        let mut out = vec![msg::OPEN_REPLY];
        out.extend_from_slice(&result.to_le_bytes());
        out
    }

    /// The client's FormatChange acknowledgement: a MessageId and the new format's index.
    pub fn format_change(index: u32) -> Vec<u8> {
        let mut out = vec![msg::FORMAT_CHANGE];
        out.extend_from_slice(&index.to_le_bytes());
        out
    }

    /// The DataIncoming PDU: a bare MessageId, sent before each Data PDU.
    pub fn data_incoming() -> Vec<u8> {
        vec![msg::DATA_INCOMING]
    }

    /// The Data PDU: a MessageId then the raw PCM buffer.
    pub fn data(pcm: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + pcm.len());
        out.push(msg::DATA);
        out.extend_from_slice(pcm);
        out
    }

    /// The host's VERSION: a single UINT32.
    pub fn parse_version(body: &[u8]) -> Option<u32> {
        (body.len() >= 4).then(|| u32::from_le_bytes(body[0..4].try_into().unwrap()))
    }

    /// The host's Sound Formats PDU: NumFormats, cbSizeFormatsPacket, then NumFormats AUDIO_FORMATs.
    /// Returns every PCM format the host offered — plain `WAVE_FORMAT_PCM` and `WAVE_FORMAT_EXTENSIBLE`
    /// carrying the PCM subtype alike — so the caller can filter to the depths it accepts.
    pub fn parse_formats(body: &[u8]) -> Vec<MicFormat> {
        let mut out = Vec::new();
        if body.len() < 8 {
            return out;
        }
        let num = u32::from_le_bytes(body[0..4].try_into().unwrap()) as usize;
        // body[4..8] is cbSizeFormatsPacket, which we do not need to trust — the format walk below
        // is bounded by the buffer, not by that count.
        let mut at = 8;
        for _ in 0..num {
            if at + 18 > body.len() {
                break;
            }
            let u16at = |o: usize| u16::from_le_bytes(body[o..o + 2].try_into().unwrap());
            let u32at = |o: usize| u32::from_le_bytes(body[o..o + 4].try_into().unwrap());
            let tag = u16at(at);
            let channels = u16at(at + 2);
            let sample_rate = u32at(at + 4);
            let bits = u16at(at + 14);
            let cb_size = u16at(at + 16) as usize;
            // EXTENSIBLE hides its real format in the SubFormat GUID at record+24 (18 base + 6 into
            // the extra bytes, past wValidBitsPerSample and dwChannelMask); its first `u16` is the
            // wave tag. Guard the read against a truncated or lying cbSize.
            let is_pcm = tag == WAVE_FORMAT_PCM
                || (tag == WAVE_FORMAT_EXTENSIBLE
                    && cb_size >= 22
                    && at + 26 <= body.len()
                    && u16at(at + 24) == WAVE_FORMAT_PCM);
            trace(&format!(
                "host format: tag=0x{tag:04x} ch={channels} rate={sample_rate} bits={bits} \
                 cbSize={cb_size} pcm={is_pcm}"
            ));
            if is_pcm {
                out.push(MicFormat { channels, sample_rate, bits_per_sample: bits });
            }
            at += 18 + cb_size;
        }
        out
    }

    /// The host's OPEN PDU: FramesPerPacket then the initialFormat index. There is **no** WAVEFORMATEX
    /// here — the format is the one at `initialFormat` in the list this endpoint advertised, so the
    /// index is the whole payload that matters (FramesPerPacket is a capture hint the browser does
    /// not need). Returns that index; the caller resolves it against its own formats.
    pub fn parse_open(body: &[u8]) -> Option<u32> {
        (body.len() >= 8).then(|| u32::from_le_bytes(body[4..8].try_into().unwrap()))
    }

    /// The host's FormatChange PDU: a single UINT32 index into the client's format list.
    pub fn parse_format_change(body: &[u8]) -> Option<u32> {
        (body.len() >= 4).then(|| u32::from_le_bytes(body[0..4].try_into().unwrap()))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const MONO_16K: MicFormat =
            MicFormat { channels: 1, sample_rate: 16_000, bits_per_sample: 16 };
        const STEREO_44K: MicFormat =
            MicFormat { channels: 2, sample_rate: 44_100, bits_per_sample: 16 };

        #[test]
        fn version_reply_is_id_then_le_u32() {
            assert_eq!(version(1), [0x01, 1, 0, 0, 0]);
        }

        #[test]
        fn an_audio_format_computes_block_align_and_byte_rate() {
            let f = audio_format(&STEREO_44K);
            assert_eq!(u16::from_le_bytes([f[0], f[1]]), WAVE_FORMAT_PCM);
            assert_eq!(u16::from_le_bytes([f[2], f[3]]), 2); // channels
            assert_eq!(u32::from_le_bytes([f[4], f[5], f[6], f[7]]), 44_100);
            assert_eq!(u32::from_le_bytes([f[8], f[9], f[10], f[11]]), 176_400); // byte rate
            assert_eq!(u16::from_le_bytes([f[12], f[13]]), 4); // block align
            assert_eq!(u16::from_le_bytes([f[14], f[15]]), 16); // bits
            assert_eq!(u16::from_le_bytes([f[16], f[17]]), 0); // cbSize
        }

        #[test]
        fn the_formats_pdu_counts_formats_and_the_whole_packet() {
            let pdu = formats(&[MONO_16K, STEREO_44K]);
            assert_eq!(pdu[0], msg::FORMATS);
            assert_eq!(u32::from_le_bytes(pdu[1..5].try_into().unwrap()), 2);
            // cbSizeFormatsPacket is the whole PDU: the 9-byte header plus both 18-byte formats.
            assert_eq!(u32::from_le_bytes(pdu[5..9].try_into().unwrap()), 45);
            assert_eq!(pdu.len(), 45);
        }

        #[test]
        fn data_is_id_then_the_raw_buffer() {
            assert_eq!(data(&[9, 8, 7]), [msg::DATA, 9, 8, 7]);
            assert_eq!(data_incoming(), [msg::DATA_INCOMING]);
        }

        /// The parser keeps the host's PCM formats and drops the rest, which is the whole of the
        /// format negotiation on this side.
        #[test]
        fn parse_formats_keeps_pcm_and_skips_compressed() {
            let mut body = Vec::new();
            body.extend_from_slice(&3u32.to_le_bytes()); // NumFormats
            body.extend_from_slice(&0u32.to_le_bytes()); // cbSizeFormatsPacket (untrusted)
            body.extend_from_slice(&audio_format(&MONO_16K));
            // A compressed format with a nonzero cbSize, to prove the walk steps over extra bytes.
            let mut compressed = audio_format(&STEREO_44K);
            compressed[0..2].copy_from_slice(&0x0002u16.to_le_bytes()); // ADPCM
            compressed[16..18].copy_from_slice(&4u16.to_le_bytes()); // cbSize = 4
            body.extend_from_slice(&compressed);
            body.extend_from_slice(&[0, 0, 0, 0]); // the 4 extra bytes
            body.extend_from_slice(&audio_format(&STEREO_44K));
            let parsed = parse_formats(&body);
            assert_eq!(parsed, vec![MONO_16K, STEREO_44K]);
        }

        /// Windows 11 enumerates its 44.1/48 kHz capture rates as WAVE_FORMAT_EXTENSIBLE with the
        /// PCM subtype; the parser must see through the GUID or the host is offered nothing it can
        /// open. A non-PCM subtype (here a zeroed GUID) is still declined.
        #[test]
        fn parse_formats_accepts_extensible_pcm() {
            // An EXTENSIBLE record: 18-byte base, then cbSize=22 of {valid bits, channel mask, GUID}.
            let extensible = |f: &MicFormat, pcm: bool| {
                let mut rec = audio_format(f).to_vec();
                rec[0..2].copy_from_slice(&WAVE_FORMAT_EXTENSIBLE.to_le_bytes());
                rec[16..18].copy_from_slice(&22u16.to_le_bytes()); // cbSize
                rec.extend_from_slice(&f.bits_per_sample.to_le_bytes()); // wValidBitsPerSample
                rec.extend_from_slice(&3u32.to_le_bytes()); // dwChannelMask
                let mut guid = [0u8; 16];
                if pcm {
                    guid[0..2].copy_from_slice(&WAVE_FORMAT_PCM.to_le_bytes());
                }
                rec.extend_from_slice(&guid);
                rec
            };
            let mut body = Vec::new();
            body.extend_from_slice(&2u32.to_le_bytes()); // NumFormats
            body.extend_from_slice(&0u32.to_le_bytes()); // cbSizeFormatsPacket (untrusted)
            body.extend_from_slice(&extensible(&STEREO_44K, true));
            body.extend_from_slice(&extensible(&MONO_16K, false)); // non-PCM subtype
            assert_eq!(parse_formats(&body), vec![STEREO_44K]);
        }

        /// A record claiming `cbSize = 22` but truncated before its SubFormat GUID must be skipped,
        /// not read out of bounds: `parse_formats` guards the GUID read with `at + 26 <= body.len()`.
        #[test]
        fn parse_formats_skips_a_truncated_extensible_record() {
            // The 18-byte base tagged EXTENSIBLE with cbSize=22, then only the 6 bytes of valid-bits
            // and channel-mask — the buffer ends before the GUID at record+24 would begin.
            let mut rec = audio_format(&STEREO_44K).to_vec();
            rec[0..2].copy_from_slice(&WAVE_FORMAT_EXTENSIBLE.to_le_bytes());
            rec[16..18].copy_from_slice(&22u16.to_le_bytes()); // cbSize claims 22 bytes follow
            rec.extend_from_slice(&16u16.to_le_bytes()); // wValidBitsPerSample
            rec.extend_from_slice(&3u32.to_le_bytes()); // dwChannelMask — GUID would start here
            let mut body = Vec::new();
            body.extend_from_slice(&1u32.to_le_bytes()); // NumFormats
            body.extend_from_slice(&0u32.to_le_bytes()); // cbSizeFormatsPacket (untrusted)
            body.extend_from_slice(&rec);
            assert!(parse_formats(&body).is_empty());
        }

        #[test]
        fn parse_open_reads_the_format_index() {
            let mut body = Vec::new();
            body.extend_from_slice(&441u32.to_le_bytes()); // FramesPerPacket (ignored)
            body.extend_from_slice(&1u32.to_le_bytes()); // initialFormat index
            assert_eq!(parse_open(&body), Some(1));
        }

        #[test]
        fn a_short_open_is_refused() {
            assert!(parse_open(&[0, 0, 0, 0]).is_none());
        }
    }
}

// ------------------------------------------------------------------ the DVC plugin

/// What one channel's callbacks talk to. `#[repr(C)]` with FreeRDP's struct first, so the pointer
/// FreeRDP holds and the pointer Rust reads are the same address.
#[repr(C)]
struct ChannelCallback {
    iface: sys::IWTSVirtualChannelCallback,
    shared: *const MicShared,
    channel: *mut sys::IWTSVirtualChannel,
}

#[repr(C)]
struct ListenerCallback {
    iface: sys::IWTSListenerCallback,
    shared: *const MicShared,
}

#[repr(C)]
struct MicPlugin {
    iface: sys::IWTSPlugin,
    shared: *const MicShared,
    mgr: *mut sys::IWTSVirtualChannelManager,
    listener_cb: *mut ListenerCallback,
    listener: *mut sys::IWTSListener,
}

/// Recover the shared state from a callback struct's `shared` field.
///
/// # Safety
///
/// `shared` must be the pointer a [`MicPlugin`] or its callbacks carry — an `Arc` the plugin holds
/// a count on until `Terminated`, so it is alive in every callback.
unsafe fn shared<'a>(shared: *const MicShared) -> Option<&'a MicShared> {
    // SAFETY: per the contract above.
    unsafe { shared.as_ref() }
}

/// The addin provider's answer for `audin` — see `audio.rs` for the provider itself.
///
/// # Safety
///
/// Called by drdynvc with its live entry points; the context they carry must be one this crate
/// created, which holds because the channel is only registered by [`Connect`](crate::Connect)s
/// built by this crate.
pub(crate) unsafe extern "C" fn plugin_entry(
    entry_points: *mut sys::IDRDYNVC_ENTRY_POINTS,
) -> sys::UINT {
    guarded("audin plugin entry", sys::CHANNEL_RC_INITIALIZATION_ERROR, || {
        if entry_points.is_null() {
            return sys::CHANNEL_RC_INITIALIZATION_ERROR;
        }
        // SAFETY: `entry_points` is live for this call; both members are drdynvc's own.
        let already = unsafe {
            match (*entry_points).GetPlugin {
                Some(get) => !get(entry_points, c"audin".as_ptr()).is_null(),
                None => false,
            }
        };
        if already {
            return sys::CHANNEL_RC_ALREADY_INITIALIZED;
        }
        // SAFETY: as above.
        let ctx = unsafe {
            match (*entry_points).GetRdpContext {
                Some(get) => get(entry_points),
                None => std::ptr::null_mut(),
            }
        };
        // SAFETY: the context is one this crate created — see the function's contract.
        let Some(bridge) = (unsafe { crate::session::bridge(ctx) }) else {
            return sys::CHANNEL_RC_INITIALIZATION_ERROR;
        };
        // A session that registered the channel without configuring a microphone cannot happen —
        // `register_mic_channel` is gated on the config — so this is a refusal, not a path.
        let Some(mic) = bridge.microphone.clone() else {
            return sys::CHANNEL_RC_INITIALIZATION_ERROR;
        };
        trace("plugin_entry: audin plugin registering");

        let mut plugin = Box::new(MicPlugin {
            // SAFETY: `IWTSPlugin` is function pointers and one interface pointer, for which
            // all-zero is null; drdynvc reads each through IFCALL, which null-checks.
            iface: unsafe { std::mem::zeroed() },
            shared: Arc::into_raw(Arc::clone(&mic.shared)),
            mgr: std::ptr::null_mut(),
            listener_cb: std::ptr::null_mut(),
            listener: std::ptr::null_mut(),
        });
        plugin.iface.Initialize = Some(plugin_initialize);
        plugin.iface.Terminated = Some(plugin_terminated);

        // Leaked to drdynvc, reclaimed in `plugin_terminated`.
        let plugin = Box::into_raw(plugin);
        // SAFETY: `entry_points` is live and `RegisterPlugin` is drdynvc's own.
        let rc = unsafe {
            match (*entry_points).RegisterPlugin {
                Some(register) => {
                    register(entry_points, c"audin".as_ptr(), plugin as *mut sys::IWTSPlugin)
                }
                None => sys::CHANNEL_RC_INITIALIZATION_ERROR,
            }
        };
        if rc != sys::CHANNEL_RC_OK {
            // SAFETY: refused, so drdynvc holds no pointer to it; reclaim both allocations.
            unsafe {
                drop(Arc::from_raw((*plugin).shared));
                drop(Box::from_raw(plugin));
            }
        }
        rc
    })
}

unsafe extern "C" fn plugin_initialize(
    plugin: *mut sys::IWTSPlugin,
    mgr: *mut sys::IWTSVirtualChannelManager,
) -> sys::UINT {
    guarded("audin Initialize", sys::CHANNEL_RC_INITIALIZATION_ERROR, || {
        if plugin.is_null() || mgr.is_null() {
            return sys::CHANNEL_RC_INITIALIZATION_ERROR;
        }
        let plugin = plugin as *mut MicPlugin;
        // SAFETY: `plugin` is the box `plugin_entry` registered, alive until Terminated.
        let shared_ptr = unsafe { (*plugin).shared };

        let mut cb = Box::new(ListenerCallback {
            // SAFETY: one function pointer and one interface pointer; all-zero is null.
            iface: unsafe { std::mem::zeroed() },
            shared: shared_ptr,
        });
        cb.iface.OnNewChannelConnection = Some(on_new_channel_connection);
        let cb = Box::into_raw(cb);
        let mut listener: *mut sys::IWTSListener = std::ptr::null_mut();
        // SAFETY: `plugin` and `mgr` are live and single-threaded here — drdynvc initialises
        // plugins before any channel traffic; the callback box outlives the listener, freed in
        // `plugin_terminated` after `DestroyListener`.
        let rc = unsafe {
            (*plugin).mgr = mgr;
            match (*mgr).CreateListener {
                Some(create) => create(
                    mgr,
                    AUDIO_INPUT_CHANNEL.as_ptr(),
                    0,
                    cb as *mut sys::IWTSListenerCallback,
                    &mut listener,
                ),
                None => sys::CHANNEL_RC_INITIALIZATION_ERROR,
            }
        };
        if rc != sys::CHANNEL_RC_OK {
            // SAFETY: refused, so dvcman holds no pointer to the box.
            unsafe { drop(Box::from_raw(cb)) };
            return rc;
        }
        // SAFETY: `plugin` is live; storing the listener and its callback for `plugin_terminated`.
        unsafe {
            (*plugin).listener_cb = cb;
            (*plugin).listener = listener;
        }
        trace("plugin_initialize: AUDIO_INPUT listener created");
        sys::CHANNEL_RC_OK
    })
}

unsafe extern "C" fn plugin_terminated(plugin: *mut sys::IWTSPlugin) -> sys::UINT {
    guarded("audin Terminated", sys::CHANNEL_RC_OK, || {
        if plugin.is_null() {
            return sys::CHANNEL_RC_OK;
        }
        let plugin = plugin as *mut MicPlugin;
        // SAFETY: the box from `plugin_entry`, freed exactly once — drdynvc calls Terminated once
        // and touches the plugin never again.
        unsafe {
            let p = &mut *plugin;
            if !p.mgr.is_null() {
                if let Some(destroy) = (*p.mgr).DestroyListener {
                    if !p.listener.is_null() {
                        destroy(p.mgr, p.listener);
                    }
                }
            }
            if !p.listener_cb.is_null() {
                drop(Box::from_raw(p.listener_cb));
            }
            // The channel dies with the plugin whether or not its OnClose fired first; clear it so
            // no later `sample` writes into freed dvcman memory, and report the stop the host now
            // implicitly made.
            let mut stopped = false;
            let mut events: Option<Arc<dyn MicEvents>> = None;
            if let Some(shared) = shared(p.shared) {
                let mut st = shared.lock();
                st.channel = None;
                stopped = st.stop();
                drop(st);
                events = Some(Arc::clone(&shared.events));
            }
            drop(Arc::from_raw(p.shared));
            drop(Box::from_raw(plugin));
            if stopped {
                if let Some(events) = events {
                    events.closed();
                }
            }
        }
        sys::CHANNEL_RC_OK
    })
}

unsafe extern "C" fn on_new_channel_connection(
    listener_cb: *mut sys::IWTSListenerCallback,
    channel: *mut sys::IWTSVirtualChannel,
    _data: *mut sys::BYTE,
    _accept: *mut sys::BOOL,
    callback_out: *mut *mut sys::IWTSVirtualChannelCallback,
) -> sys::UINT {
    guarded("audin OnNewChannelConnection", sys::CHANNEL_RC_INITIALIZATION_ERROR, || {
        if listener_cb.is_null() || channel.is_null() || callback_out.is_null() {
            return sys::CHANNEL_RC_INITIALIZATION_ERROR;
        }
        let listener = listener_cb as *mut ListenerCallback;
        let mut cb = Box::new(ChannelCallback {
            // SAFETY: function pointers and one interface pointer; all-zero is null.
            iface: unsafe { std::mem::zeroed() },
            // SAFETY: the listener box is the plugin's, alive until Terminated.
            shared: unsafe { (*listener).shared },
            channel,
        });
        cb.iface.OnDataReceived = Some(on_data_received);
        cb.iface.OnOpen = Some(on_open);
        cb.iface.OnClose = Some(on_close);
        // Leaked to dvcman, reclaimed in `on_close`.
        // SAFETY: `callback_out` is dvcman's out-parameter for this call.
        unsafe { *callback_out = Box::into_raw(cb) as *mut sys::IWTSVirtualChannelCallback };
        sys::CHANNEL_RC_OK
    })
}

unsafe extern "C" fn on_open(cb: *mut sys::IWTSVirtualChannelCallback) -> sys::UINT {
    guarded("audin OnOpen", sys::CHANNEL_RC_OK, || {
        let cb = cb as *mut ChannelCallback;
        if cb.is_null() {
            return sys::CHANNEL_RC_OK;
        }
        // SAFETY: the box from `on_new_channel_connection`, alive until its OnClose.
        let (shared_ptr, channel) = unsafe { ((*cb).shared, (*cb).channel) };
        // SAFETY: the plugin holds the Arc until Terminated.
        let Some(shared) = (unsafe { shared(shared_ptr) }) else {
            return sys::CHANNEL_RC_OK;
        };
        // Store the channel and wait: the host speaks first, with a VERSION PDU. There is nothing
        // to send until it does.
        shared.lock().channel = Some(Chan(channel));
        sys::CHANNEL_RC_OK
    })
}

unsafe extern "C" fn on_close(cb: *mut sys::IWTSVirtualChannelCallback) -> sys::UINT {
    guarded("audin OnClose", sys::CHANNEL_RC_OK, || {
        let cb = cb as *mut ChannelCallback;
        if cb.is_null() {
            return sys::CHANNEL_RC_OK;
        }
        // SAFETY: the box from `on_new_channel_connection`; dvcman calls OnClose once, and this is
        // where it is reclaimed.
        let cb = unsafe { Box::from_raw(cb) };
        // SAFETY: the plugin holds the Arc until Terminated.
        let Some(shared) = (unsafe { shared(cb.shared) }) else {
            return sys::CHANNEL_RC_OK;
        };
        let mut st = shared.lock();
        let stopped = if st.channel.as_ref().map(|c| c.0) == Some(cb.channel) {
            st.channel = None;
            st.formats.clear();
            st.stop()
        } else {
            false
        };
        drop(st);
        if stopped {
            shared.events.closed();
        }
        sys::CHANNEL_RC_OK
    })
}

/// The bytes still unread in a wStream — see `camera::stream_bytes` for the identical contract.
///
/// # Safety
///
/// `s` must be a live wStream for the duration of the borrow.
unsafe fn stream_bytes<'a>(s: *mut sys::wStream) -> Option<&'a [u8]> {
    if s.is_null() {
        return None;
    }
    // SAFETY: `s` is live per the contract.
    let stream = unsafe { &*s };
    if stream.buffer.is_null() || stream.pointer.is_null() {
        return None;
    }
    let consumed = unsafe { stream.pointer.offset_from(stream.buffer) };
    if consumed < 0 || consumed as usize > stream.length {
        return None;
    }
    let remaining = stream.length - consumed as usize;
    // SAFETY: `pointer..pointer+remaining` is inside the stream's buffer by the checks above.
    Some(unsafe { std::slice::from_raw_parts(stream.pointer, remaining) })
}

unsafe extern "C" fn on_data_received(
    cb: *mut sys::IWTSVirtualChannelCallback,
    data: *mut sys::wStream,
) -> sys::UINT {
    guarded("audin OnDataReceived", sys::CHANNEL_RC_OK, || {
        let cb = cb as *mut ChannelCallback;
        if cb.is_null() {
            return sys::CHANNEL_RC_OK;
        }
        // SAFETY: the box from `on_new_channel_connection`, alive until its OnClose.
        let (shared_ptr, channel) = unsafe { ((*cb).shared, Chan((*cb).channel)) };
        // SAFETY: the plugin holds the Arc until Terminated.
        let Some(shared) = (unsafe { shared(shared_ptr) }) else {
            return sys::CHANNEL_RC_OK;
        };
        // SAFETY: dvcman owns the stream for the duration of this call.
        let Some(bytes) = (unsafe { stream_bytes(data) }) else {
            return sys::CHANNEL_RC_OK;
        };
        if bytes.is_empty() {
            return sys::CHANNEL_RC_OK;
        }
        let (id, body) = (bytes[0], &bytes[1..]);
        trace(&format!("recv 0x{id:02x}, {} body byte(s)", body.len()));
        handle(shared, &channel, id, body);
        sys::CHANNEL_RC_OK
    })
}

/// The whole MS-RDPEAI conversation the host drives.
fn handle(shared: &MicShared, channel: &Chan, id: u8, body: &[u8]) {
    let mut st = shared.lock();
    let mut event: Option<MicEvent> = None;
    match id {
        msg::VERSION => {
            if let Some(server) = wire::parse_version(body) {
                // Answer the server's VERSION with our own maximum, exactly as FreeRDP's audin
                // client does — never the server's number — and decline (no reply) only when the
                // server asks for a version newer than we speak. The negotiated value drives nothing
                // on the wire, so nothing remembers it; the reply and the event both carry the one
                // number this endpoint speaks.
                if server > SNDIN_VERSION {
                    trace(&format!("host VERSION {server} is newer than {SNDIN_VERSION}; not answering"));
                } else {
                    channel.write(&wire::version(SNDIN_VERSION));
                    event = Some(MicEvent::Negotiated(SNDIN_VERSION));
                }
            }
        }
        msg::FORMATS => {
            // Keep the host's PCM-16 formats, advertise exactly those back, and remember them so an
            // OPEN or FormatChange index resolves. A host that offers nothing this endpoint accepts
            // gets an empty list and never opens the stream.
            let accepted: Vec<MicFormat> = wire::parse_formats(body)
                .into_iter()
                .filter(|f| f.bits_per_sample == PCM_BITS_PER_SAMPLE)
                .collect();
            trace(&format!("advertising {} PCM-16 format(s) back to the host", accepted.len()));
            // A DATA_INCOMING precedes the Sound Formats reply, as FreeRDP's audin client sends it;
            // then the formats themselves.
            channel.write(&wire::data_incoming());
            channel.write(&wire::formats(&accepted));
            st.formats = accepted;
        }
        msg::OPEN => {
            // OPEN names a format by index into the list this endpoint advertised — there is no
            // format on the wire. Confirm the choice with a FormatChange, then acknowledge the OPEN,
            // in that order (FreeRDP's `audin_process_open`).
            if let Some(index) = wire::parse_open(body) {
                if let Some(&format) = st.formats.get(index as usize) {
                    trace(&format!(
                        "OPEN index={index} -> ch={} rate={} bits={}",
                        format.channels, format.sample_rate, format.bits_per_sample
                    ));
                    st.open = Some(format);
                    channel.write(&wire::format_change(index));
                    channel.write(&wire::open_reply(RESULT_OK));
                    event = Some(MicEvent::Opened(format));
                } else {
                    // An index into a list we never advertised: acknowledge the OPEN with a failure
                    // rather than silence, so every parsed OPEN gets a reply. The unadvertised format
                    // is otherwise ignored — no FormatChange, no open, no event.
                    trace(&format!("OPEN names unadvertised format index {index}; declining"));
                    channel.write(&wire::open_reply(RESULT_FAIL));
                }
            }
        }
        msg::FORMAT_CHANGE => {
            // A mid-stream format switch: re-confirm with a FormatChange only — no OpenReply, which
            // belongs to OPEN alone. The format is the indexed one from our advertised list.
            if let Some(index) = wire::parse_format_change(body) {
                if let Some(&format) = st.formats.get(index as usize) {
                    trace(&format!("FORMATCHANGE index={index}"));
                    st.open = Some(format);
                    channel.write(&wire::format_change(index));
                    event = Some(MicEvent::Opened(format));
                }
            }
        }
        // The host does not send DataIncoming/Data/OpenReply to the client, and anything else is
        // unknown; MS-RDPEAI has no error PDU, so silence is the only answer.
        _ => {}
    }
    drop(st);
    match event {
        Some(MicEvent::Negotiated(v)) => shared.events.negotiated(v),
        Some(MicEvent::Opened(f)) => shared.events.opened(f),
        None => {}
    }
}

/// What `handle` tells the embedder after the lock is released.
enum MicEvent {
    Negotiated(u32),
    Opened(MicFormat),
}
