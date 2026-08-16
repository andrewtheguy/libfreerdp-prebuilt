//! A redirected camera, by being the MS-RDPECAM endpoint ourselves.
//!
//! `rdpecam` is unlike both channel shapes this crate already binds. It is not a client context
//! to subscribe to (`cliprdr`, `disp`) and not a device slot inside a compiled-in channel
//! (`rdpsnd`): the archives build the channel **out** entirely, because FreeRDP's own
//! implementation is a V4L capture stack and an H.264 encoder, and this crate's camera is
//! neither — the embedder already holds encoded frames and needs a wire to put them on. What the
//! archives do carry is `drdynvc`, and MS-RDPECAM is nothing but messages over two dynamic
//! virtual channels. So this module implements the protocol itself, in Rust, as a generic DVC
//! plugin:
//!
//! 1. **The plugin is found through the same process-global addin provider as the sound
//!    device.** `drdynvc` loads each dynamic channel named in the settings through
//!    `freerdp_load_channel_addin_entry(name, NULL, NULL, …)`; the provider in `audio.rs`
//!    answers for `rdpecam` with [`plugin_entry`] and delegates everything else. The same
//!    global-provider caveat applies and is documented there.
//! 2. **The channels are the server's to create.** The plugin only registers listeners — the
//!    fixed `RDCamera_Device_Enumerator` name and one device channel name — and the server
//!    connects to them: the enumeration channel as soon as the client announces a device on it
//!    is worth having, the device channel in response to a DeviceAdded on the first.
//! 3. **The plugin finds its way back to Rust through the context**: the DVC entry points carry
//!    `GetRdpContext`, and the bridge on the end of that context holds the session's [`Camera`].
//!
//! Like sound, camera traffic does not go through [`Event`](crate::Event): the host's
//! start/stop/credit decisions are handed to [`CameraEvents`] on a FreeRDP thread as they
//! arrive, and the embedder's frames go straight onto the channel from whatever thread calls
//! [`Camera::sample`] — `dvcman_write_channel` serialises writers, which is the same license
//! FreeRDP's own capture thread writes under. The state below has one lock and every touch of a
//! channel pointer holds it, so a channel that closes under one thread cannot be written by
//! another.

use freerdp_sys as sys;

use crate::session::guarded;
use std::collections::VecDeque;
use std::ffi::CStr;
use std::sync::{Arc, Mutex};

/// The MS-RDPECAM version this endpoint speaks; the negotiated version is the server's when
/// lower. Version 2 is what FreeRDP 3 and every current Windows host speak.
const ECAM_PROTO_VERSION: u8 = 2;

/// The device enumeration channel's name — fixed by the protocol, `RDPECAM_CONTROL_DVC_CHANNEL_NAME`.
const ENUMERATOR_CHANNEL: &CStr = c"RDCamera_Device_Enumerator";

/// The one device channel's name. The protocol lets the endpoint pick any name — it travels in
/// DeviceAdded and the server creates a channel by it — and one camera is all this module
/// offers, so the name is a constant rather than configuration.
const DEVICE_CHANNEL: &CStr = c"RustCam_0";

/// How many samples may wait for the server's credit before the queue is declared behind.
///
/// MS-RDPECAM meters samples: one SampleResponse per SampleRequest, and a host pipelines
/// requests (FreeRDP's own headroom constant is 8 for 30 fps at 250 ms RTT). A queue deeper
/// than that pipeline is not latency worth keeping — it is a camera falling behind the link —
/// so overflow drops the queue whole and asks for a keyframe instead; see [`Camera::sample`].
const PENDING_SAMPLES_MAX: usize = 8;

/// Message ids, from MS-RDPECAM 2.2 (`CAM_MSG_ID` in FreeRDP's `rdpecam.h`).
mod msg {
    pub const SUCCESS_RESPONSE: u8 = 0x01;
    pub const ERROR_RESPONSE: u8 = 0x02;
    pub const SELECT_VERSION_REQUEST: u8 = 0x03;
    pub const SELECT_VERSION_RESPONSE: u8 = 0x04;
    pub const DEVICE_ADDED_NOTIFICATION: u8 = 0x05;
    pub const DEVICE_REMOVED_NOTIFICATION: u8 = 0x06;
    pub const ACTIVATE_DEVICE_REQUEST: u8 = 0x07;
    pub const DEACTIVATE_DEVICE_REQUEST: u8 = 0x08;
    pub const STREAM_LIST_REQUEST: u8 = 0x09;
    pub const STREAM_LIST_RESPONSE: u8 = 0x0A;
    pub const MEDIA_TYPE_LIST_REQUEST: u8 = 0x0B;
    pub const MEDIA_TYPE_LIST_RESPONSE: u8 = 0x0C;
    pub const CURRENT_MEDIA_TYPE_REQUEST: u8 = 0x0D;
    pub const CURRENT_MEDIA_TYPE_RESPONSE: u8 = 0x0E;
    pub const START_STREAMS_REQUEST: u8 = 0x0F;
    pub const STOP_STREAMS_REQUEST: u8 = 0x10;
    pub const SAMPLE_REQUEST: u8 = 0x11;
    pub const SAMPLE_RESPONSE: u8 = 0x12;
    pub const PROPERTY_LIST_REQUEST: u8 = 0x14;
    pub const PROPERTY_LIST_RESPONSE: u8 = 0x15;
}

/// Error codes, from MS-RDPECAM 2.2.3.2 (`CAM_ERROR_CODE`).
mod err {
    pub const INVALID_MESSAGE: u32 = 0x02;
    pub const INVALID_STREAM_NUMBER: u32 = 0x05;
    pub const INVALID_MEDIA_TYPE: u32 = 0x06;
    pub const OPERATION_NOT_SUPPORTED: u32 = 0x0A;
}

/// `CAM_MEDIA_FORMAT_H264` — the one format this endpoint advertises and accepts.
///
/// H.264 because the embedder's frames arrive already encoded and every Windows camera stack
/// decodes it; advertising anything raw would put megabytes a second on a channel that meters
/// samples one credit at a time. An endpoint with a different codec needs a different module,
/// not a parameter here.
const FORMAT_H264: u8 = 0x01;

/// `CAM_MEDIA_TYPE_DESCRIPTION_FLAG_DecodingRequired` — H.264 is a format the host must decode.
const FLAG_DECODING_REQUIRED: u8 = 0x01;

/// What the camera sends: H.264 access units of this geometry and rate.
///
/// The denominator exists because MS-RDPECAM's media type carries a rational frame rate, and
/// browsers report rates like 29.97; an integer rate is `{ numerator: fps, denominator: 1 }`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CameraFormat {
    pub width: u32,
    pub height: u32,
    pub fps_numerator: u32,
    pub fps_denominator: u32,
}

/// Where the host's streaming decisions go.
///
/// **Every method runs on a FreeRDP thread** and must not block and must not call back into
/// [`Camera`] — the events are emitted with the camera's own lock released, but the thread they
/// run on is the channel's, so a queue whose push cannot wait is the shape this is for, exactly
/// as with [`AudioSink`](crate::AudioSink).
pub trait CameraEvents: Send + Sync {
    /// The host opened the enumeration channel and a version was agreed: camera redirection is
    /// on offer at all. A host that never fires this has the channel disabled — policy, an old
    /// server, or a gateway in the way — and that absence is the only observable difference
    /// between "declined" and "still coming up".
    fn negotiated(&self, version: u8);
    /// The host connected the device channel and is querying the device — the virtual camera
    /// is being installed on the far side. Streaming still needs an application there to open
    /// it, which arrives as [`Self::started`].
    fn attached(&self);
    /// The host started the stream and will meter samples from now on. Samples handed to
    /// [`Camera::sample`] before this are dropped. The stream must (re)start at a keyframe.
    fn started(&self, format: CameraFormat);
    /// The host stopped the stream — StopStreams, DeactivateDevice, or the channel closing.
    /// Encoding can stop; the device stays plugged and another `started` may follow.
    fn stopped(&self);
    /// Samples were dropped — the queue overflowed, or arrived while a keyframe was awaited —
    /// and decoding cannot continue mid-GOP, so the next sample the embedder sends must be a
    /// keyframe. Idempotent: it may fire once per dropped sample.
    fn keyframe_needed(&self);
}

/// A raw channel pointer that may cross the state lock.
///
/// SAFETY: only dereferenced while the [`CamState`] lock is held, and cleared (under that same
/// lock) by `OnClose`/`Terminated` before dvcman frees the object — so no dereference can
/// outlive the channel.
struct Chan(*mut sys::IWTSVirtualChannel);
// SAFETY: see `Chan` — the pointer is only used under the state lock.
unsafe impl Send for Chan {}

impl Chan {
    /// Write one whole protocol message. A `false` is a channel refusing — closing or closed —
    /// and the caller treats it like the close it is about to observe.
    fn write(&self, buf: &[u8]) -> bool {
        // SAFETY: the caller holds the state lock, so the channel is alive (see `Chan`), and
        // `Write` copies the buffer before returning — FreeRDP's own ecam reuses its response
        // buffer immediately after this call.
        let ok = unsafe {
            let Some(write) = (*self.0).Write else { return false };
            write(self.0, buf.len() as sys::ULONG, buf.as_ptr(), std::ptr::null_mut())
                == sys::CHANNEL_RC_OK
        };
        trace(&format!(
            "sent 0x{:02x}, {} byte(s), accepted={ok}",
            buf.get(1).copied().unwrap_or(0),
            buf.len()
        ));
        ok
    }
}

/// Wire tracing for debugging against a real server: `FREERDP_ECAM_TRACE=1` prints every
/// MS-RDPECAM message either way on stderr. The protocol has no observable state on the far
/// side — a server that dislikes a response simply stops asking — so which request came last
/// is the whole diagnosis, and this is how it is read.
fn trace(line: &str) {
    if std::env::var_os("FREERDP_ECAM_TRACE").is_some() {
        eprintln!("ecam: {line}");
    }
}

/// Everything the protocol remembers, under the one lock.
struct CamState {
    /// The negotiated protocol version, stamped on every message this endpoint writes.
    version: u8,
    /// The enumeration channel, present between its OnOpen and its OnClose.
    enumerator: Option<Chan>,
    /// Whether SelectVersion finished — DeviceAdded before that is a message the server has
    /// not yet agreed on a version for, so announcements wait for it.
    enum_ready: bool,
    /// The device channel, present between its OnOpen and its OnClose.
    device: Option<Chan>,
    /// Whether the embedder wants the device visible. Survives channel teardown — a
    /// reconnect-resize rebuilds the channels and the device re-announces itself from this.
    plugged: bool,
    /// Whether DeviceAdded went out for the current plug.
    announced: bool,
    /// The format the embedder plugged with, and the whole of the media type list.
    format: Option<CameraFormat>,
    /// Between StartStreams and StopStreams/Deactivate/close.
    streaming: bool,
    /// Outstanding SampleRequests: how many SampleResponses the server is owed.
    credits: u32,
    /// Samples waiting for credit, oldest first.
    pending: VecDeque<Vec<u8>>,
    /// Set when samples were dropped: everything is discarded until the next keyframe, because
    /// a decoder handed the far side of a gap shows artifacts until one arrives anyway.
    awaiting_keyframe: bool,
}

impl CamState {
    fn new() -> Self {
        Self {
            version: ECAM_PROTO_VERSION,
            enumerator: None,
            enum_ready: false,
            device: None,
            plugged: false,
            announced: false,
            format: None,
            streaming: false,
            credits: 0,
            pending: VecDeque::new(),
            awaiting_keyframe: false,
        }
    }

    /// Stop streaming and forget everything metered. Returns whether a stream was running,
    /// which is whether anyone needs telling.
    fn stop_stream(&mut self) -> bool {
        let was = self.streaming;
        self.streaming = false;
        self.credits = 0;
        self.pending.clear();
        self.awaiting_keyframe = false;
        was
    }
}

/// The shared half a [`Camera`] hands the plugin: configuration outside the lock, protocol
/// state inside it.
struct CamShared {
    device_name: String,
    events: Arc<dyn CameraEvents>,
    state: Mutex<CamState>,
}

impl CamShared {
    /// The state, poison-proof for the same reason as the input queue: the lock only guards
    /// plain state, and refusing to lock after a panicking writer would silently end the
    /// camera for the session.
    fn lock(&self) -> std::sync::MutexGuard<'_, CamState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A session's camera: plug it, feed it samples, and hear the host's decisions.
///
/// Cloneable and callable from any thread. One per [`Connect`](crate::Connect); the plug/unplug
/// state outlives the channels, so a session that reconnects mid-stream (a reconnect-resize)
/// re-announces the device by itself.
#[derive(Clone)]
pub struct Camera {
    shared: Arc<CamShared>,
}

impl std::fmt::Debug for Camera {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Camera").field("device_name", &self.shared.device_name).finish_non_exhaustive()
    }
}

impl Camera {
    /// A camera named `device_name` — the string Windows shows beside the redirected device —
    /// whose host-side decisions go to `events`.
    pub fn new(device_name: impl Into<String>, events: Arc<dyn CameraEvents>) -> Self {
        Self {
            shared: Arc::new(CamShared {
                device_name: device_name.into(),
                events,
                state: Mutex::new(CamState::new()),
            }),
        }
    }

    /// Announce the device to the server, as a camera producing H.264 in `format`.
    ///
    /// Idempotent while plugged; a format change needs an [`Self::unplug`] first, because the
    /// far side caches the media type list against the device. If the enumeration channel is
    /// not up yet — the session is still connecting — the announcement is made the moment it
    /// is, so calling this before the session connects is fine.
    pub fn plug(&self, format: CameraFormat) {
        let mut st = self.shared.lock();
        st.plugged = true;
        if st.format.is_none() {
            st.format = Some(format);
        }
        self.announce(&mut st);
    }

    /// Withdraw the device. The server sees the camera unplug; a running stream ends without a
    /// [`CameraEvents::stopped`], since the caller is the one ending it.
    pub fn unplug(&self) {
        let mut st = self.shared.lock();
        st.plugged = false;
        st.format = None;
        st.stop_stream();
        if st.announced {
            st.announced = false;
            let version = st.version;
            if let Some(ch) = &st.enumerator {
                ch.write(&wire::device_removed(version, DEVICE_CHANNEL));
            }
        }
    }

    /// One encoded H.264 access unit. Returns whether it was accepted — sent or queued.
    ///
    /// Samples are metered by the server's credits. With none left the sample queues, at most
    /// [`PENDING_SAMPLES_MAX`] deep; past that the queue is dropped whole and everything is
    /// refused until the next `keyframe`, with [`CameraEvents::keyframe_needed`] saying so —
    /// H.264 cannot resume mid-GOP, so dropping less would only ship frames the far decoder
    /// shows as artifacts.
    pub fn sample(&self, data: &[u8], keyframe: bool) -> bool {
        let mut st = self.shared.lock();
        if !st.streaming {
            return false;
        }
        if st.awaiting_keyframe {
            if !keyframe {
                drop(st);
                self.shared.events.keyframe_needed();
                return false;
            }
            st.awaiting_keyframe = false;
        }
        if st.credits > 0 && st.pending.is_empty() {
            let version = st.version;
            // The credit is spent only by a write the channel took: a refused
            // write is the close the caller is about to observe, and burning
            // the credit on it would leave one fewer for a stream that then
            // survives. Refused rather than queued for the same reason — the
            // queue is for samples a live channel will drain.
            let sent = st
                .device
                .as_ref()
                .is_some_and(|ch| ch.write(&wire::sample_response(version, 0, data)));
            if !sent {
                return false;
            }
            st.credits -= 1;
            return true;
        }
        if st.pending.len() < PENDING_SAMPLES_MAX {
            st.pending.push_back(data.to_vec());
            return true;
        }
        st.pending.clear();
        st.awaiting_keyframe = true;
        drop(st);
        self.shared.events.keyframe_needed();
        false
    }

    /// Send DeviceAdded when everything it waits for holds: the embedder plugged, the
    /// enumeration channel negotiated, and no announcement is already out.
    fn announce(&self, st: &mut CamState) {
        if !(st.plugged && st.enum_ready && !st.announced && st.format.is_some()) {
            return;
        }
        let version = st.version;
        let added = wire::device_added(version, &self.shared.device_name, DEVICE_CHANNEL);
        if let Some(ch) = &st.enumerator {
            if ch.write(&added) {
                st.announced = true;
            }
        }
    }
}

// ------------------------------------------------------------------ wire format
//
// Builders and parsers for the handful of MS-RDPECAM messages this endpoint exchanges. Pure
// byte work, which is what makes the protocol testable without a server. Little-endian
// throughout, one message per channel write.

mod wire {
    use super::*;

    fn header(version: u8, id: u8) -> Vec<u8> {
        vec![version, id]
    }

    pub fn select_version_request(version: u8) -> Vec<u8> {
        header(version, msg::SELECT_VERSION_REQUEST)
    }

    pub fn success_response(version: u8) -> Vec<u8> {
        header(version, msg::SUCCESS_RESPONSE)
    }

    pub fn error_response(version: u8, code: u32) -> Vec<u8> {
        let mut out = header(version, msg::ERROR_RESPONSE);
        out.extend_from_slice(&code.to_le_bytes());
        out
    }

    pub fn property_list_response(version: u8) -> Vec<u8> {
        // An empty list: this camera has no adjustable properties, and MS-RDPECAM's list is
        // simply absent members after the header.
        header(version, msg::PROPERTY_LIST_RESPONSE)
    }

    /// DeviceName as null-terminated UTF-16LE, then the channel name as null-terminated ASCII.
    pub fn device_added(version: u8, device_name: &str, channel: &CStr) -> Vec<u8> {
        let mut out = header(version, msg::DEVICE_ADDED_NOTIFICATION);
        for unit in device_name.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(channel.to_bytes_with_nul());
        out
    }

    pub fn device_removed(version: u8, channel: &CStr) -> Vec<u8> {
        let mut out = header(version, msg::DEVICE_REMOVED_NOTIFICATION);
        out.extend_from_slice(channel.to_bytes_with_nul());
        out
    }

    /// One stream: color, capture category, selected, not shareable — the fixed shape FreeRDP's
    /// own endpoint sends, and the only one a single-stream camera has.
    pub fn stream_list_response(version: u8) -> Vec<u8> {
        let mut out = header(version, msg::STREAM_LIST_RESPONSE);
        out.extend_from_slice(&1u16.to_le_bytes()); // FrameSourceTypes: Color
        out.push(0x01); // StreamCategory: Capture
        out.push(1); // Selected
        out.push(0); // CanBeShared
        out
    }

    /// The 26-byte CAM_MEDIA_TYPE_DESCRIPTION.
    fn media_type(format: &CameraFormat) -> [u8; 26] {
        let mut out = [0u8; 26];
        out[0] = FORMAT_H264;
        out[1..5].copy_from_slice(&format.width.to_le_bytes());
        out[5..9].copy_from_slice(&format.height.to_le_bytes());
        out[9..13].copy_from_slice(&format.fps_numerator.to_le_bytes());
        out[13..17].copy_from_slice(&format.fps_denominator.to_le_bytes());
        out[17..21].copy_from_slice(&1u32.to_le_bytes()); // PixelAspectRatioNumerator
        out[21..25].copy_from_slice(&1u32.to_le_bytes()); // PixelAspectRatioDenominator
        out[25] = FLAG_DECODING_REQUIRED;
        out
    }

    /// The whole list is the one media type: the embedder's encoder produces one geometry, so
    /// offering the server a menu would only invite a StartStreams this endpoint refuses.
    pub fn media_type_list_response(version: u8, format: &CameraFormat) -> Vec<u8> {
        let mut out = header(version, msg::MEDIA_TYPE_LIST_RESPONSE);
        out.extend_from_slice(&media_type(format));
        out
    }

    pub fn current_media_type_response(version: u8, format: &CameraFormat) -> Vec<u8> {
        let mut out = header(version, msg::CURRENT_MEDIA_TYPE_RESPONSE);
        out.extend_from_slice(&media_type(format));
        out
    }

    pub fn sample_response(version: u8, stream_index: u8, sample: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(3 + sample.len());
        out.push(version);
        out.push(msg::SAMPLE_RESPONSE);
        out.push(stream_index);
        out.extend_from_slice(sample);
        out
    }

    /// The first CAM_START_STREAM_INFO of a StartStreamsRequest: stream index and media type.
    /// The message may carry more infos, but a one-stream endpoint only ever selected one.
    pub fn parse_start_streams(body: &[u8]) -> Option<(u8, u8, CameraFormat)> {
        if body.len() < 27 {
            return None;
        }
        let stream_index = body[0];
        let le32 = |at: usize| u32::from_le_bytes(body[at..at + 4].try_into().unwrap());
        let format = body[1];
        let media = CameraFormat {
            width: le32(2),
            height: le32(6),
            fps_numerator: le32(10),
            fps_denominator: le32(14),
        };
        if media.fps_numerator == 0 || media.fps_denominator == 0 {
            return None;
        }
        Some((stream_index, format, media))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const FORMAT: CameraFormat =
            CameraFormat { width: 1280, height: 720, fps_numerator: 30, fps_denominator: 1 };

        #[test]
        fn every_message_leads_with_version_and_id() {
            assert_eq!(select_version_request(2), [2, 0x03]);
            assert_eq!(success_response(2), [2, 0x01]);
            assert_eq!(property_list_response(2), [2, 0x15]);
            assert_eq!(error_response(2, err::OPERATION_NOT_SUPPORTED), [2, 0x02, 0x0A, 0, 0, 0]);
        }

        #[test]
        fn device_added_is_utf16_name_then_ascii_channel() {
            let added = device_added(2, "Cam", c"Ch0");
            assert_eq!(
                added,
                [2, 0x05, b'C', 0, b'a', 0, b'm', 0, 0, 0, b'C', b'h', b'0', 0]
            );
        }

        #[test]
        fn device_removed_names_the_channel() {
            assert_eq!(device_removed(2, c"Ch0"), [2, 0x06, b'C', b'h', b'0', 0]);
        }

        #[test]
        fn the_stream_list_is_one_color_capture_stream() {
            assert_eq!(stream_list_response(2), [2, 0x0A, 1, 0, 0x01, 1, 0]);
        }

        #[test]
        fn the_media_type_is_h264_at_the_plugged_geometry() {
            let list = media_type_list_response(2, &FORMAT);
            assert_eq!(list.len(), 2 + 26);
            assert_eq!(&list[..3], [2, 0x0C, FORMAT_H264]);
            assert_eq!(u32::from_le_bytes(list[3..7].try_into().unwrap()), 1280);
            assert_eq!(u32::from_le_bytes(list[7..11].try_into().unwrap()), 720);
            assert_eq!(u32::from_le_bytes(list[11..15].try_into().unwrap()), 30);
            assert_eq!(u32::from_le_bytes(list[15..19].try_into().unwrap()), 1);
            assert_eq!(list[27], FLAG_DECODING_REQUIRED);
        }

        #[test]
        fn a_sample_response_is_three_bytes_of_header_then_the_unit() {
            assert_eq!(sample_response(2, 0, &[9, 8, 7]), [2, 0x12, 0, 9, 8, 7]);
        }

        /// The parser reads back what the builder writes: a StartStreams carrying the media
        /// type this endpoint advertised is the normal case, so the two must agree.
        #[test]
        fn start_streams_round_trips_the_advertised_media_type() {
            let mut body = vec![0u8]; // stream index
            body.extend_from_slice(&media_type(&FORMAT));
            let (index, format, media) = parse_start_streams(&body).expect("parses");
            assert_eq!(index, 0);
            assert_eq!(format, FORMAT_H264);
            assert_eq!(media, FORMAT);
        }

        #[test]
        fn a_zero_frame_rate_is_refused_not_divided_by() {
            let mut body = vec![0u8];
            let mut broken = media_type(&FORMAT);
            broken[9..13].copy_from_slice(&0u32.to_le_bytes());
            body.extend_from_slice(&broken);
            assert!(parse_start_streams(&body).is_none());
        }

        #[test]
        fn a_truncated_start_streams_is_refused() {
            assert!(parse_start_streams(&[0; 26]).is_none());
        }
    }
}

// ------------------------------------------------------------------ the DVC plugin

/// What one channel's callbacks talk to. `#[repr(C)]` with FreeRDP's struct first, so the
/// pointer FreeRDP holds and the pointer Rust reads are the same address.
#[repr(C)]
struct ChannelCallback {
    iface: sys::IWTSVirtualChannelCallback,
    shared: *const CamShared,
    channel: *mut sys::IWTSVirtualChannel,
    enumerator: bool,
}

#[repr(C)]
struct ListenerCallback {
    iface: sys::IWTSListenerCallback,
    shared: *const CamShared,
    enumerator: bool,
}

#[repr(C)]
struct CamPlugin {
    iface: sys::IWTSPlugin,
    shared: *const CamShared,
    mgr: *mut sys::IWTSVirtualChannelManager,
    enum_listener_cb: *mut ListenerCallback,
    dev_listener_cb: *mut ListenerCallback,
    enum_listener: *mut sys::IWTSListener,
    dev_listener: *mut sys::IWTSListener,
}

/// Recover the shared state from a callback struct's `shared` field.
///
/// # Safety
///
/// `shared` must be the pointer a [`CamPlugin`] or its callbacks carry — an `Arc` the plugin
/// holds a count on until `Terminated`, so it is alive in every callback.
unsafe fn shared<'a>(shared: *const CamShared) -> Option<&'a CamShared> {
    // SAFETY: per the contract above.
    unsafe { shared.as_ref() }
}

/// The addin provider's answer for `rdpecam` — see `audio.rs` for the provider itself.
///
/// # Safety
///
/// Called by drdynvc with its live entry points; the context they carry must be one this crate
/// created, which holds because the channel is only registered by [`Connect`](crate::Connect)s
/// built by this crate.
pub(crate) unsafe extern "C" fn plugin_entry(
    entry_points: *mut sys::IDRDYNVC_ENTRY_POINTS,
) -> sys::UINT {
    guarded("rdpecam plugin entry", sys::CHANNEL_RC_INITIALIZATION_ERROR, || {
        if entry_points.is_null() {
            return sys::CHANNEL_RC_INITIALIZATION_ERROR;
        }
        // SAFETY: `entry_points` is live for this call; both members are drdynvc's own.
        let already = unsafe {
            match (*entry_points).GetPlugin {
                Some(get) => !get(entry_points, c"rdpecam".as_ptr()).is_null(),
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
        // A session that registered the channel without configuring a camera cannot happen —
        // `register_camera_channel` is gated on the config — so this is a refusal, not a path.
        let Some(camera) = bridge.camera.clone() else {
            return sys::CHANNEL_RC_INITIALIZATION_ERROR;
        };

        let mut plugin = Box::new(CamPlugin {
            // SAFETY: `IWTSPlugin` is function pointers and one interface pointer, for which
            // all-zero is null; drdynvc reads each through IFCALL, which null-checks.
            iface: unsafe { std::mem::zeroed() },
            shared: Arc::into_raw(Arc::clone(&camera.shared)),
            mgr: std::ptr::null_mut(),
            enum_listener_cb: std::ptr::null_mut(),
            dev_listener_cb: std::ptr::null_mut(),
            enum_listener: std::ptr::null_mut(),
            dev_listener: std::ptr::null_mut(),
        });
        plugin.iface.Initialize = Some(plugin_initialize);
        plugin.iface.Terminated = Some(plugin_terminated);

        // Leaked to drdynvc, reclaimed in `plugin_terminated` — the same custody the sound
        // device passes through `Free`.
        let plugin = Box::into_raw(plugin);
        // SAFETY: `entry_points` is live and `RegisterPlugin` is drdynvc's own.
        let rc = unsafe {
            match (*entry_points).RegisterPlugin {
                Some(register) => {
                    register(entry_points, c"rdpecam".as_ptr(), plugin as *mut sys::IWTSPlugin)
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

/// Build one listener: a boxed callback struct handed to `CreateListener`.
///
/// # Safety
///
/// `mgr` must be drdynvc's live channel manager and `shared` the plugin's state pointer.
unsafe fn create_listener(
    mgr: *mut sys::IWTSVirtualChannelManager,
    shared_ptr: *const CamShared,
    name: &CStr,
    enumerator: bool,
) -> Result<(*mut ListenerCallback, *mut sys::IWTSListener), sys::UINT> {
    let mut cb = Box::new(ListenerCallback {
        // SAFETY: one function pointer and one interface pointer; all-zero is null.
        iface: unsafe { std::mem::zeroed() },
        shared: shared_ptr,
        enumerator,
    });
    cb.iface.OnNewChannelConnection = Some(on_new_channel_connection);
    let cb = Box::into_raw(cb);
    let mut listener: *mut sys::IWTSListener = std::ptr::null_mut();
    // SAFETY: `mgr` is live per the contract; the callback box outlives the listener — it is
    // freed in `plugin_terminated`, after `DestroyListener`.
    let rc = unsafe {
        match (*mgr).CreateListener {
            Some(create) => create(
                mgr,
                name.as_ptr(),
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
        return Err(rc);
    }
    Ok((cb, listener))
}

unsafe extern "C" fn plugin_initialize(
    plugin: *mut sys::IWTSPlugin,
    mgr: *mut sys::IWTSVirtualChannelManager,
) -> sys::UINT {
    guarded("rdpecam Initialize", sys::CHANNEL_RC_INITIALIZATION_ERROR, || {
        if plugin.is_null() || mgr.is_null() {
            return sys::CHANNEL_RC_INITIALIZATION_ERROR;
        }
        let plugin = plugin as *mut CamPlugin;
        // SAFETY: `plugin` is the box `plugin_entry` registered, alive until Terminated.
        let shared_ptr = unsafe { (*plugin).shared };
        // Both listeners up front. The device listener could wait for the DeviceAdded that
        // names it, but a listener with no channel costs nothing and a race with the server's
        // create costs the device — the server may create the moment the notification lands.
        //
        // Each listener is stored on the plugin the moment it exists, *before* the next one
        // is attempted, so a failure partway through leaves everything already created where
        // `plugin_terminated` reclaims it — dvcman calls Terminated on a plugin whose
        // Initialize failed, so nothing is orphaned. Storing after both (the obvious shape)
        // was a leak: the first listener's box would be unreachable when the second refused.
        //
        // SAFETY: `plugin` is live and single-threaded here — drdynvc initialises plugins
        // before any channel traffic; `mgr` is drdynvc's, live for the call; `shared_ptr`
        // is the plugin's.
        unsafe {
            (*plugin).mgr = mgr;
            match create_listener(mgr, shared_ptr, ENUMERATOR_CHANNEL, true) {
                Ok((cb, listener)) => {
                    (*plugin).enum_listener_cb = cb;
                    (*plugin).enum_listener = listener;
                }
                Err(rc) => return rc,
            }
            match create_listener(mgr, shared_ptr, DEVICE_CHANNEL, false) {
                Ok((cb, listener)) => {
                    (*plugin).dev_listener_cb = cb;
                    (*plugin).dev_listener = listener;
                }
                Err(rc) => return rc,
            }
        }
        sys::CHANNEL_RC_OK
    })
}

unsafe extern "C" fn plugin_terminated(plugin: *mut sys::IWTSPlugin) -> sys::UINT {
    guarded("rdpecam Terminated", sys::CHANNEL_RC_OK, || {
        if plugin.is_null() {
            return sys::CHANNEL_RC_OK;
        }
        let plugin = plugin as *mut CamPlugin;
        // SAFETY: the box from `plugin_entry`, freed exactly once — drdynvc calls Terminated
        // once and touches the plugin never again.
        unsafe {
            let p = &mut *plugin;
            if !p.mgr.is_null() {
                if let Some(destroy) = (*p.mgr).DestroyListener {
                    if !p.enum_listener.is_null() {
                        destroy(p.mgr, p.enum_listener);
                    }
                    if !p.dev_listener.is_null() {
                        destroy(p.mgr, p.dev_listener);
                    }
                }
            }
            if !p.enum_listener_cb.is_null() {
                drop(Box::from_raw(p.enum_listener_cb));
            }
            if !p.dev_listener_cb.is_null() {
                drop(Box::from_raw(p.dev_listener_cb));
            }
            // The channels die with the plugin whether or not their OnClose fired first;
            // clear them so no later `plug`/`sample` writes into freed dvcman memory, and
            // reset the announcement so a reconnected session announces afresh.
            let mut stopped = false;
            let mut events: Option<Arc<dyn CameraEvents>> = None;
            if let Some(shared) = shared(p.shared) {
                let mut st = shared.lock();
                st.enumerator = None;
                st.enum_ready = false;
                st.device = None;
                st.announced = false;
                stopped = st.stop_stream();
                drop(st);
                events = Some(Arc::clone(&shared.events));
            }
            drop(Arc::from_raw(p.shared));
            drop(Box::from_raw(plugin));
            if stopped {
                if let Some(events) = events {
                    events.stopped();
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
    guarded("rdpecam OnNewChannelConnection", sys::CHANNEL_RC_INITIALIZATION_ERROR, || {
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
            // SAFETY: as above.
            enumerator: unsafe { (*listener).enumerator },
        });
        cb.iface.OnDataReceived = Some(on_data_received);
        cb.iface.OnOpen = Some(on_open);
        cb.iface.OnClose = Some(on_close);
        // Leaked to dvcman, reclaimed in `on_close` — dvcman calls OnClose exactly once per
        // channel it opened, then never touches the callback again.
        // SAFETY: `callback_out` is dvcman's out-parameter for this call.
        unsafe { *callback_out = Box::into_raw(cb) as *mut sys::IWTSVirtualChannelCallback };
        sys::CHANNEL_RC_OK
    })
}

unsafe extern "C" fn on_open(cb: *mut sys::IWTSVirtualChannelCallback) -> sys::UINT {
    guarded("rdpecam OnOpen", sys::CHANNEL_RC_OK, || {
        let cb = cb as *mut ChannelCallback;
        if cb.is_null() {
            return sys::CHANNEL_RC_OK;
        }
        // SAFETY: the box from `on_new_channel_connection`, alive until its OnClose.
        let (shared_ptr, channel, enumerator) =
            unsafe { ((*cb).shared, (*cb).channel, (*cb).enumerator) };
        // SAFETY: the plugin holds the Arc until Terminated.
        let Some(shared) = (unsafe { shared(shared_ptr) }) else {
            return sys::CHANNEL_RC_OK;
        };
        let mut st = shared.lock();
        if enumerator {
            st.enumerator = Some(Chan(channel));
            // The version request opens the conversation; the response is what arms
            // DeviceAdded, in `on_data_received`.
            let version = st.version;
            if let Some(ch) = &st.enumerator {
                ch.write(&wire::select_version_request(version));
            }
            drop(st);
        } else {
            st.device = Some(Chan(channel));
            drop(st);
            shared.events.attached();
        }
        sys::CHANNEL_RC_OK
    })
}

unsafe extern "C" fn on_close(cb: *mut sys::IWTSVirtualChannelCallback) -> sys::UINT {
    guarded("rdpecam OnClose", sys::CHANNEL_RC_OK, || {
        let cb = cb as *mut ChannelCallback;
        if cb.is_null() {
            return sys::CHANNEL_RC_OK;
        }
        // SAFETY: the box from `on_new_channel_connection`; dvcman calls OnClose once, and
        // this is where it is reclaimed.
        let cb = unsafe { Box::from_raw(cb) };
        // SAFETY: the plugin holds the Arc until Terminated.
        let Some(shared) = (unsafe { shared(cb.shared) }) else {
            return sys::CHANNEL_RC_OK;
        };
        let mut st = shared.lock();
        let stopped = if cb.enumerator {
            // The enumeration channel closing takes the announcement with it: whatever the
            // server still shows, a new channel means a new SelectVersion and a new
            // DeviceAdded.
            if st.enumerator.as_ref().map(|c| c.0) == Some(cb.channel) {
                st.enumerator = None;
                st.enum_ready = false;
                st.announced = false;
            }
            false
        } else if st.device.as_ref().map(|c| c.0) == Some(cb.channel) {
            st.device = None;
            st.stop_stream()
        } else {
            false
        };
        drop(st);
        if stopped {
            shared.events.stopped();
        }
        sys::CHANNEL_RC_OK
    })
}

/// The bytes still unread in a wStream: dvcman hands the whole message positioned at zero, but
/// reading through `pointer` keeps this honest against a caller that already consumed some.
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
    guarded("rdpecam OnDataReceived", sys::CHANNEL_RC_OK, || {
        let cb = cb as *mut ChannelCallback;
        if cb.is_null() {
            return sys::CHANNEL_RC_OK;
        }
        // SAFETY: the box from `on_new_channel_connection`, alive until its OnClose.
        let (shared_ptr, channel, enumerator) =
            unsafe { ((*cb).shared, Chan((*cb).channel), (*cb).enumerator) };
        // SAFETY: the plugin holds the Arc until Terminated.
        let Some(shared) = (unsafe { shared(shared_ptr) }) else {
            return sys::CHANNEL_RC_OK;
        };
        // SAFETY: dvcman owns the stream for the duration of this call.
        let Some(bytes) = (unsafe { stream_bytes(data) }) else {
            return sys::CHANNEL_RC_OK;
        };
        if bytes.len() < 2 {
            return sys::CHANNEL_RC_OK;
        }
        let (server_version, id, body) = (bytes[0], bytes[1], &bytes[2..]);
        trace(&format!(
            "recv 0x{id:02x} on the {} channel, {} body byte(s)",
            if enumerator { "enumerator" } else { "device" },
            body.len()
        ));
        if enumerator {
            handle_enumerator(shared, &channel, server_version, id);
        } else {
            handle_device(shared, &channel, id, body);
        }
        sys::CHANNEL_RC_OK
    })
}

/// The enumeration channel speaks exactly one message to us: the version response. Everything
/// else is unexpected and answered as such, mirroring FreeRDP's endpoint.
fn handle_enumerator(shared: &CamShared, channel: &Chan, server_version: u8, id: u8) {
    let mut st = shared.lock();
    match id {
        msg::SELECT_VERSION_RESPONSE => {
            // Versions start at 1, so 0 is a malformed response rather than a
            // negotiable floor; above our own is the server's choice we cannot
            // speak. Either way the default stands and enumeration proceeds.
            if (1..=ECAM_PROTO_VERSION).contains(&server_version) {
                st.version = server_version;
            }
            st.enum_ready = true;
            // The announce below duplicates `Camera::announce` because the lock is already
            // held here; the conditions are the method's own.
            if st.plugged && !st.announced && st.format.is_some() {
                let version = st.version;
                let name = &shared.device_name;
                if channel.write(&wire::device_added(version, name, DEVICE_CHANNEL)) {
                    st.announced = true;
                }
            }
            let version = st.version;
            drop(st);
            shared.events.negotiated(version);
        }
        // A response is never answered: replying to a reply is how two ends
        // volley errors at each other forever.
        msg::SUCCESS_RESPONSE | msg::ERROR_RESPONSE => {}
        _ => {
            let version = st.version;
            channel.write(&wire::error_response(version, err::OPERATION_NOT_SUPPORTED));
        }
    }
}

/// The device channel: the whole MS-RDPECAM device conversation.
fn handle_device(shared: &CamShared, channel: &Chan, id: u8, body: &[u8]) {
    let mut st = shared.lock();
    let version = st.version;
    // Requests that name a stream index; this endpoint has exactly stream 0.
    let stream_index_ok = |body: &[u8]| -> bool {
        if body.first().copied() == Some(0) {
            return true;
        }
        channel.write(&wire::error_response(version, err::INVALID_STREAM_NUMBER));
        false
    };
    let mut event: Option<Started> = None;
    match id {
        msg::ACTIVATE_DEVICE_REQUEST => {
            channel.write(&wire::success_response(version));
        }
        msg::DEACTIVATE_DEVICE_REQUEST | msg::STOP_STREAMS_REQUEST => {
            if st.stop_stream() {
                event = Some(Started::No);
            }
            channel.write(&wire::success_response(version));
        }
        msg::STREAM_LIST_REQUEST => {
            channel.write(&wire::stream_list_response(version));
        }
        msg::MEDIA_TYPE_LIST_REQUEST => {
            if stream_index_ok(body) {
                match st.format {
                    Some(format) => {
                        channel.write(&wire::media_type_list_response(version, &format));
                    }
                    None => {
                        channel.write(&wire::error_response(version, err::INVALID_MESSAGE));
                    }
                }
            }
        }
        msg::CURRENT_MEDIA_TYPE_REQUEST => {
            if stream_index_ok(body) {
                match st.format {
                    Some(format) => {
                        channel.write(&wire::current_media_type_response(version, &format));
                    }
                    None => {
                        channel.write(&wire::error_response(version, err::INVALID_MESSAGE));
                    }
                }
            }
        }
        msg::START_STREAMS_REQUEST => match wire::parse_start_streams(body) {
            Some((0, FORMAT_H264, media)) => {
                st.stop_stream();
                st.streaming = true;
                // Nothing sent until the next keyframe: the far decoder starts from
                // parameter sets, and the embedder restarts its encoder on `started`.
                st.awaiting_keyframe = true;
                channel.write(&wire::success_response(version));
                event = Some(Started::Yes(media));
            }
            Some((0, _, _)) => {
                channel.write(&wire::error_response(version, err::INVALID_MEDIA_TYPE));
            }
            Some((_, _, _)) => {
                channel.write(&wire::error_response(version, err::INVALID_STREAM_NUMBER));
            }
            None => {
                channel.write(&wire::error_response(version, err::INVALID_MESSAGE));
            }
        },
        msg::SAMPLE_REQUEST => {
            if stream_index_ok(body) && st.streaming {
                st.credits = st.credits.saturating_add(1);
                while st.credits > 0 {
                    let Some(sample) = st.pending.pop_front() else { break };
                    st.credits -= 1;
                    channel.write(&wire::sample_response(version, 0, &sample));
                }
            }
        }
        msg::PROPERTY_LIST_REQUEST => {
            channel.write(&wire::property_list_response(version));
        }
        // A response is never answered — the enumerator's rule, for the same
        // reason: replying to a reply is a volley with no last word.
        msg::SUCCESS_RESPONSE | msg::ERROR_RESPONSE => {}
        _ => {
            channel.write(&wire::error_response(version, err::OPERATION_NOT_SUPPORTED));
        }
    }
    drop(st);
    match event {
        Some(Started::Yes(media)) => shared.events.started(media),
        Some(Started::No) => shared.events.stopped(),
        None => {}
    }
}

/// What `handle_device` tells the embedder after the lock is released.
enum Started {
    Yes(CameraFormat),
    No,
}
