use std::{
    mem::size_of,
    sync::{
        atomic::{AtomicBool, AtomicU8},
        mpsc::{self, sync_channel, SyncSender},
    },
    thread::JoinHandle,
    time::{Duration, SystemTime},
};

use pipewire as pw;
use pw::{
    context::Context,
    main_loop::MainLoop,
    properties::properties,
    spa::{
        self,
        param::{
            format::{FormatProperties, MediaSubtype, MediaType},
            video::VideoFormat,
            ParamType,
        },
        pod::{Pod, Property},
        sys::{
            spa_buffer, spa_meta_header, SPA_META_Header, SPA_PARAM_META_size, SPA_PARAM_META_type,
        },
        utils::{Direction, SpaTypes},
    },
    stream::{StreamRef, StreamState},
};

use crate::{
    capturer::Options,
    frame::{BGRxFrame, Frame, RGBFrame, RGBxFrame, VideoFrame, XBGRFrame},
};

use self::{error::LinCapError, portal::ScreenCastPortal};

mod error;
mod portal;

/// The authoritative set of video formats advertised to PipeWire in
/// [`pipewire_capturer`]'s format-negotiation block. Every entry must have a
/// decode arm in [`video_frame_for`].
///
/// The advertised and decodable sets used to be maintained independently and
/// disagreed in both directions: `RGBA` was advertised but had no decode arm,
/// while `xBGR` had a decode arm but was never advertised. A compositor that
/// selected `RGBA` therefore reached the fallback and panicked -- reported
/// against COSMIC in #153 and #169, which negotiates `RGBA` where GNOME and
/// KDE negotiate `BGRx`.
///
/// `RGBA` is kept advertised and given a decode arm rather than dropped,
/// precisely so that case keeps working.
///
/// **What is mechanically guaranteed**, stated precisely because the
/// directions are not equally covered:
///
/// - `advertised => decodable` is enforced generally, for every entry here,
///   by `every_advertised_format_has_a_decode_arm`, which also pins the exact
///   [`VideoFrame`] variant each one maps to.
/// - **Growing this array cannot silently under-advertise.** The negotiation
///   block destructures it rather than indexing, so adding an entry fails to
///   compile at that callsite instead of quietly continuing to offer only the
///   previous set.
/// - `decodable => advertised` is **not** enforced generally. Adding a new arm
///   to [`video_frame_for`] without adding it here leaves that format simply
///   unnegotiable. That is quieter than the panic above, but -- as #153 shows
///   -- "a compositor wants a format we do not offer" is a real failure, not a
///   harmless one.
///
/// Closing that last direction would mean generating both this array and the
/// dispatch from one declarative table, or enumerating every `VideoFormat`.
/// Neither seemed warranted at this size; add it here if the set grows.
const SUPPORTED_VIDEO_FORMATS: [VideoFormat; 5] = [
    VideoFormat::RGB,
    VideoFormat::RGBA,
    VideoFormat::RGBx,
    VideoFormat::xBGR,
    VideoFormat::BGRx,
];

/// Build the [`VideoFrame`] for a negotiated `format`, or `None` when this
/// engine has no decode arm for it.
///
/// Split out of [`process_callback`] so the advertised list above can be
/// checked against the dispatch without a live PipeWire stream. See that
/// list's docs for exactly which direction the tests enforce.
fn video_frame_for(
    format: VideoFormat,
    display_time: SystemTime,
    width: i32,
    height: i32,
    data: Vec<u8>,
) -> Option<VideoFrame> {
    match format {
        VideoFormat::RGBx => Some(VideoFrame::RGBx(RGBxFrame {
            display_time,
            width,
            height,
            data,
        })),
        VideoFormat::RGB => Some(VideoFrame::RGB(RGBFrame {
            display_time,
            width,
            height,
            data,
        })),
        // RGBA has the same byte order and width as RGBx; the two differ only
        // in whether the fourth byte carries alpha or is undefined padding.
        // `VideoFrame` has no RGBA variant and scap does not currently expose
        // alpha semantics, so RGBA is represented as RGBx and the fourth byte
        // is treated as unused. Same mapping proposed in #153 and #169.
        VideoFormat::RGBA => Some(VideoFrame::RGBx(RGBxFrame {
            display_time,
            width,
            height,
            data,
        })),
        VideoFormat::xBGR => Some(VideoFrame::XBGR(XBGRFrame {
            display_time,
            width,
            height,
            data,
        })),
        VideoFormat::BGRx => Some(VideoFrame::BGRx(BGRxFrame {
            display_time,
            width,
            height,
            data,
        })),
        _ => None,
    }
}

static CAPTURER_STATE: AtomicU8 = AtomicU8::new(0);
static STREAM_STATE_CHANGED_TO_ERROR: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
struct ListenerUserData {
    pub tx: mpsc::Sender<Frame>,
    pub format: spa::param::video::VideoInfoRaw,
    /// Whether an unsupported negotiated format has already been reported.
    ///
    /// Without this the fallback in `process_callback` would log once per
    /// captured frame -- potentially dozens of lines per second, indefinitely
    /// -- if a portal delivers a format outside the negotiated set. Reset in
    /// `param_changed_callback` so a genuinely new negotiation is reported
    /// again.
    pub unsupported_format_reported: bool,
}

fn param_changed_callback(
    _stream: &StreamRef,
    user_data: &mut ListenerUserData,
    id: u32,
    param: Option<&Pod>,
) {
    let Some(param) = param else {
        return;
    };
    if id != pw::spa::param::ParamType::Format.as_raw() {
        return;
    }
    let (media_type, media_subtype) = match pw::spa::param::format_utils::parse_format(param) {
        Ok(v) => v,
        Err(_) => return,
    };

    if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
        return;
    }

    user_data
        .format
        .parse(param)
        // TODO: Tell library user of the error
        .expect("Failed to parse format parameter");

    // A new format was negotiated, so allow the unsupported-format warning to
    // fire once more if this one also turns out to be undecodable.
    user_data.unsupported_format_reported = false;
}

fn state_changed_callback(
    _stream: &StreamRef,
    _user_data: &mut ListenerUserData,
    _old: StreamState,
    new: StreamState,
) {
    match new {
        StreamState::Error(e) => {
            eprintln!("pipewire: State changed to error({e})");
            STREAM_STATE_CHANGED_TO_ERROR.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        _ => {}
    }
}

unsafe fn get_timestamp(buffer: *mut spa_buffer) -> i64 {
    let n_metas = (*buffer).n_metas;
    if n_metas > 0 {
        let mut meta_ptr = (*buffer).metas;
        let metas_end = (*buffer).metas.wrapping_add(n_metas as usize);
        while meta_ptr != metas_end {
            if (*meta_ptr).type_ == SPA_META_Header {
                let meta_header: &mut spa_meta_header =
                    &mut *((*meta_ptr).data as *mut spa_meta_header);
                return meta_header.pts;
            }
            meta_ptr = meta_ptr.wrapping_add(1);
        }
        0
    } else {
        0
    }
}

fn process_callback(stream: &StreamRef, user_data: &mut ListenerUserData) {
    let buffer = unsafe { stream.dequeue_raw_buffer() };
    if !buffer.is_null() {
        'outside: {
            let buffer = unsafe { (*buffer).buffer };
            if buffer.is_null() {
                break 'outside;
            }
            let timestamp = unsafe { get_timestamp(buffer) };

            let n_datas = unsafe { (*buffer).n_datas };
            if n_datas < 1 {
                return;
            }
            // Check the negotiated format BEFORE copying the buffer. The
            // one-shot flag below bounds the *logging*, but without this the
            // callback would still `to_vec()` and then discard every frame a
            // noncompliant portal delivers -- bounded logs, unbounded wasted
            // copying, which at high resolution is real memory bandwidth.
            // `break 'outside` requeues the PipeWire buffer at the end of the
            // function, same as every other early exit here.
            let negotiated_format = user_data.format.format();
            if !SUPPORTED_VIDEO_FORMATS.contains(&negotiated_format) {
                if !user_data.unsupported_format_reported {
                    user_data.unsupported_format_reported = true;
                    eprintln!("Unsupported frame format received: {negotiated_format:?}");
                }
                break 'outside;
            }

            let frame_size = user_data.format.size();
            let frame_data: Vec<u8> = unsafe {
                std::slice::from_raw_parts(
                    (*(*buffer).datas).data as *mut u8,
                    (*(*buffer).datas).maxsize as usize,
                )
                .to_vec()
            };

            // `timestamp` (spa_meta_header.pts) is a monotonic nanosecond
            // count from an unspecified origin, so it cannot be represented
            // directly as `display_time`'s `SystemTime`.
            //
            // `SystemTime::now()` is the minimal compile repair: it records
            // when this callback processed the frame, NOT when the source
            // captured it. That discards the source capture clock and its
            // inter-frame timing, not merely sub-millisecond precision --
            // callback delivery can be delayed or bursty. Note this is weaker
            // than the Windows engine, which anchors a SystemTime/performance-
            // counter origin and derives each frame's display_time from the
            // capture timestamp delta. Doing the same here needs a PTS origin
            // to anchor against, which is out of scope for a compile fix.
            let _pipewire_pts_ns = timestamp;
            let display_time = SystemTime::now();

            match video_frame_for(
                negotiated_format,
                display_time,
                frame_size.width as i32,
                frame_size.height as i32,
                frame_data,
            ) {
                Some(video_frame) => {
                    if let Err(e) = user_data.tx.send(Frame::Video(video_frame)) {
                        eprintln!("{e}");
                    }
                }
                None => {
                    // Formats outside the advertised set are already rejected
                    // above, so reaching here means SUPPORTED_VIDEO_FORMATS
                    // contains something `video_frame_for` cannot decode --
                    // which `every_advertised_format_has_a_decode_arm` exists
                    // to prevent. Retained rather than made `unreachable!()`
                    // because this runs inside an FFI-driven callback, where
                    // unwinding is not something the C caller is prepared for.
                    if !user_data.unsupported_format_reported {
                        user_data.unsupported_format_reported = true;
                        eprintln!("Advertised format {negotiated_format:?} has no decode arm");
                    }
                }
            }
        }
    } else {
        eprintln!("Out of buffers");
    }

    unsafe { stream.queue_raw_buffer(buffer) };
}

fn pipewire_capturer(
    options: Options,
    tx: mpsc::Sender<Frame>,
    ready_sender: &SyncSender<bool>,
    stream_id: u32,
) -> Result<(), LinCapError> {
    pw::init();

    let mainloop = MainLoop::new(None)?;
    let context = Context::new(&mainloop)?;
    let core = context.connect(None)?;

    let user_data = ListenerUserData {
        tx,
        format: Default::default(),
        unsupported_format_reported: false,
    };

    let stream = pw::stream::Stream::new(
        &core,
        "scap",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let _listener = stream
        .add_local_listener_with_user_data(user_data.clone())
        .state_changed(state_changed_callback)
        .param_changed(param_changed_callback)
        .process(process_callback)
        .register()?;

    // Destructured rather than indexed: if SUPPORTED_VIDEO_FORMATS gains a
    // sixth entry, this fails to compile instead of silently continuing to
    // advertise only the first five. Suggested by review on #187.
    let [fmt0, fmt1, fmt2, fmt3, fmt4] = SUPPORTED_VIDEO_FORMATS;

    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pw::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pw::spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            fmt0,
            fmt1,
            fmt2,
            fmt3,
            fmt4,
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle {
                // Default
                width: 128,
                height: 128,
            },
            pw::spa::utils::Rectangle {
                // Min
                width: 1,
                height: 1,
            },
            pw::spa::utils::Rectangle {
                // Max
                width: 4096,
                height: 4096,
            }
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoMaxFramerate,
            Fraction,
            pw::spa::utils::Fraction {
                num: options.fps,
                denom: 1
            }
        ),
    );

    let metas_obj = pw::spa::pod::object!(
        SpaTypes::ObjectParamMeta,
        ParamType::Meta,
        Property::new(
            SPA_PARAM_META_type,
            pw::spa::pod::Value::Id(pw::spa::utils::Id(SPA_META_Header))
        ),
        Property::new(
            SPA_PARAM_META_size,
            pw::spa::pod::Value::Int(size_of::<pw::spa::sys::spa_meta_header>() as i32)
        ),
    );

    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )?
    .0
    .into_inner();
    let metas_values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(metas_obj),
    )?
    .0
    .into_inner();

    let mut params = [
        pw::spa::pod::Pod::from_bytes(&values).unwrap(),
        pw::spa::pod::Pod::from_bytes(&metas_values).unwrap(),
    ];

    stream.connect(
        Direction::Input,
        Some(stream_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    ready_sender.send(true)?;

    while CAPTURER_STATE.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        std::thread::sleep(Duration::from_millis(10));
    }

    let pw_loop = mainloop.loop_();

    // User has called Capturer::start() and we start the main loop
    while CAPTURER_STATE.load(std::sync::atomic::Ordering::Relaxed) == 1
        && /* If the stream state got changed to `Error`, we exit. TODO: tell user that we exited */
          !STREAM_STATE_CHANGED_TO_ERROR.load(std::sync::atomic::Ordering::Relaxed)
    {
        pw_loop.iterate(Duration::from_millis(100));
    }

    Ok(())
}

pub struct LinuxCapturer {
    capturer_join_handle: Option<JoinHandle<Result<(), LinCapError>>>,
    // The pipewire stream is deleted when the connection is dropped.
    // That's why we keep it alive
    _connection: dbus::blocking::Connection,
}

impl LinuxCapturer {
    // TODO: Error handling
    pub fn new(options: &Options, tx: mpsc::Sender<Frame>) -> Self {
        let connection =
            dbus::blocking::Connection::new_session().expect("Failed to create dbus connection");
        let stream_id = ScreenCastPortal::new(&connection)
            .show_cursor(options.show_cursor)
            .expect("Unsupported cursor mode")
            .create_stream()
            .expect("Failed to get screencast stream")
            .pw_node_id();

        // TODO: Fix this hack
        let options = options.clone();
        let (ready_sender, ready_recv) = sync_channel(1);
        let capturer_join_handle = std::thread::spawn(move || {
            let res = pipewire_capturer(options, tx, &ready_sender, stream_id);
            if res.is_err() {
                ready_sender.send(false)?;
            }
            res
        });

        if !ready_recv.recv().expect("Failed to receive") {
            panic!("Failed to setup capturer");
        }

        Self {
            capturer_join_handle: Some(capturer_join_handle),
            _connection: connection,
        }
    }

    pub fn start_capture(&self) {
        CAPTURER_STATE.store(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn stop_capture(&mut self) {
        CAPTURER_STATE.store(2, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.capturer_join_handle.take() {
            if let Err(e) = handle.join().expect("Failed to join capturer thread") {
                eprintln!("Error occured capturing: {e}");
            }
        }
        CAPTURER_STATE.store(0, std::sync::atomic::Ordering::Relaxed);
        STREAM_STATE_CHANGED_TO_ERROR.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

pub fn create_capturer(options: &Options, tx: mpsc::Sender<Frame>) -> LinuxCapturer {
    LinuxCapturer::new(options, tx)
}

#[cfg(test)]
mod format_negotiation_tests {
    use super::*;

    const W: i32 = 3;
    const H: i32 = 2;

    /// Bytes per pixel for a given negotiated format.
    ///
    /// Deliberately exhaustive over the supported set rather than falling back
    /// to 4: a catch-all would silently hand a future 3-byte format a 4-byte
    /// fixture, which is exactly the false test signal this helper exists to
    /// prevent. Adding a format requires consciously declaring its layout.
    fn bytes_per_pixel(format: VideoFormat) -> usize {
        match format {
            VideoFormat::RGB => 3,
            VideoFormat::RGBA | VideoFormat::RGBx | VideoFormat::xBGR | VideoFormat::BGRx => 4,
            other => panic!("no test fixture size defined for {other:?}"),
        }
    }

    /// A correctly-sized sample buffer for `format`.
    ///
    /// Sized per format rather than a fixed length so that if
    /// `video_frame_for` ever validates buffer size, these tests fail for the
    /// reason under test rather than because RGB was handed a 4-byte-per-pixel
    /// buffer.
    fn sample_data(format: VideoFormat) -> Vec<u8> {
        vec![7u8; (W * H) as usize * bytes_per_pixel(format)]
    }

    fn sample(format: VideoFormat) -> Option<VideoFrame> {
        video_frame_for(format, SystemTime::UNIX_EPOCH, W, H, sample_data(format))
    }

    /// Anything offered to PipeWire must be something this engine can decode,
    /// AND must map to the variant callers expect. `is_some()` alone would
    /// still pass if every format accidentally decoded as `RGB`, so each
    /// mapping is pinned explicitly.
    #[test]
    fn every_advertised_format_has_a_decode_arm() {
        for format in SUPPORTED_VIDEO_FORMATS {
            assert!(
                sample(format).is_some(),
                "advertised format {format:?} has no decode arm in video_frame_for"
            );
        }

        assert!(matches!(sample(VideoFormat::RGB), Some(VideoFrame::RGB(_))));
        assert!(matches!(
            sample(VideoFormat::RGBx),
            Some(VideoFrame::RGBx(_))
        ));
        assert!(matches!(
            sample(VideoFormat::xBGR),
            Some(VideoFrame::XBGR(_))
        ));
        assert!(matches!(
            sample(VideoFormat::BGRx),
            Some(VideoFrame::BGRx(_))
        ));
        // RGBA is deliberately represented as RGBx -- same byte order and
        // width, alpha ignored. See video_frame_for.
        assert!(matches!(
            sample(VideoFormat::RGBA),
            Some(VideoFrame::RGBx(_))
        ));
    }

    /// The frame payload must survive the dispatch unchanged -- a decode arm
    /// that mapped to the right variant but dropped or reordered the buffer
    /// would still satisfy the mapping assertions above.
    ///
    /// Covers `BGRx` as a representative branch, not all five: the exact
    /// variant mapping for every format is already asserted in
    /// `every_advertised_format_has_a_decode_arm`, and all arms construct
    /// their frame identically.
    #[test]
    fn bgrx_dispatch_preserves_dimensions_timestamp_and_data() {
        let Some(VideoFrame::BGRx(frame)) = sample(VideoFormat::BGRx) else {
            panic!("BGRx did not decode to a BGRx frame");
        };
        assert_eq!(frame.width, W);
        assert_eq!(frame.height, H);
        assert_eq!(frame.display_time, SystemTime::UNIX_EPOCH);
        assert_eq!(frame.data, sample_data(VideoFormat::BGRx));
    }

    /// `RGBA` and its advertisement must change together.
    ///
    /// COSMIC negotiates `RGBA` where GNOME/KDE negotiate `BGRx` (#153, #169),
    /// so dropping either half silently breaks that desktop: removing the
    /// decode arm reintroduces the original panic, and removing it from the
    /// advertised set leaves COSMIC without a compatible offer.
    ///
    /// Written as an equality rather than "must be absent" so it stays
    /// meaningful if the representation changes, instead of needing deletion.
    #[test]
    fn rgba_is_decodable_and_advertised_together() {
        assert_eq!(
            sample(VideoFormat::RGBA).is_some(),
            SUPPORTED_VIDEO_FORMATS.contains(&VideoFormat::RGBA),
            "RGBA decode support and RGBA advertisement must change together"
        );
        assert!(
            sample(VideoFormat::RGBA).is_some(),
            "RGBA support was removed -- this regresses COSMIC, see #153/#169"
        );
    }

    /// `xBGR` had a decode arm but was never advertised, so it could never be
    /// negotiated. Pins that one historical omission -- it does NOT generalize
    /// to "every decodable format is advertised", which no test here checks.
    #[test]
    fn xbgr_is_both_decodable_and_advertised() {
        assert!(sample(VideoFormat::xBGR).is_some());
        assert!(SUPPORTED_VIDEO_FORMATS.contains(&VideoFormat::xBGR));
    }
}
