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
/// `stream_params()`. Every entry must have a decode arm in
/// [`video_frame_for`].
///
/// Previously the advertised and decodable sets were maintained independently
/// and disagreed in both directions: `RGBA` was advertised but had no decode
/// arm, while `xBGR` had a decode arm but was never advertised. A compositor
/// that selected `RGBA` therefore reached the fallback and panicked, despite
/// scap having offered that format itself.
///
/// **What is mechanically guaranteed**, stated precisely because the
/// directions are not equally covered:
///
/// - `advertised => decodable` is enforced generally, for every entry here,
///   by `every_advertised_format_has_a_decode_arm`. This is the direction
///   that matters: advertising a format the engine cannot decode is what
///   caused the panic.
/// - **Growing this array cannot silently under-advertise.** `stream_params`
///   destructures it (`let [fmt0, fmt1, fmt2, fmt3] = ...`) rather than
///   indexing, so adding a fifth entry fails to compile at that callsite
///   instead of quietly continuing to offer only the first four.
/// - `decodable => advertised` is **not** enforced generally. Only the
///   historical `xBGR` omission is pinned, by
///   `xbgr_is_both_decodable_and_advertised`. Adding a new arm to
///   [`video_frame_for`] without adding it here would leave that format
///   simply unnegotiable — harmless, but silent.
///
/// Closing that last direction would mean generating both this array and the
/// dispatch from one declarative table, or enumerating every `VideoFormat`.
/// Neither is warranted for the four formats this engine supports; add it
/// here if the set grows.
const SUPPORTED_VIDEO_FORMATS: [VideoFormat; 4] = [
    VideoFormat::RGB,
    VideoFormat::RGBx,
    VideoFormat::xBGR,
    VideoFormat::BGRx,
];

/// Build the [`VideoFrame`] for a negotiated `format`, or `None` when this
/// engine has no decode arm for it.
///
/// Split out of the `on_process` callback so the advertised list above can be
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
            let frame_size = user_data.format.size();
            let frame_data: Vec<u8> = unsafe {
                std::slice::from_raw_parts(
                    (*(*buffer).datas).data as *mut u8,
                    (*(*buffer).datas).maxsize as usize,
                )
                .to_vec()
            };

            // `timestamp` (from spa_meta_header.pts) is a PipeWire monotonic
            // nanosecond count since an arbitrary reference — not wall-clock.
            // display_time's SystemTime contract is wall-clock, so we use
            // SystemTime::now() here (matches what the macOS and Windows
            // engines do today).  Relative frame ordering survives via
            // channel-send order; sub-millisecond buffer timing is lost.
            let _pts_ns = timestamp; // TODO: plumb PipeWire PTS through frame metadata
            let display_time = SystemTime::now();

            match video_frame_for(
                user_data.format.format(),
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
                    // Unreachable by construction: PipeWire can only negotiate
                    // a format we advertised, and everything in
                    // SUPPORTED_VIDEO_FORMATS has a decode arm. Reported
                    // rather than panicking because this runs inside an
                    // FFI-driven callback, where unwinding is not something
                    // the C caller is prepared for.
                    eprintln!(
                        "Unsupported frame format received: {:?}",
                        user_data.format.format()
                    );
                }
            }
        }
    } else {
        eprintln!("Out of buffers");
    }

    unsafe { stream.queue_raw_buffer(buffer) };
}

// TODO: Format negotiation
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
    // fifth entry, this fails to compile instead of silently continuing to
    // advertise only the first four. Suggested by review on #187.
    let [fmt0, fmt1, fmt2, fmt3] = SUPPORTED_VIDEO_FORMATS;

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

    fn sample(format: VideoFormat) -> Option<VideoFrame> {
        video_frame_for(format, SystemTime::UNIX_EPOCH, 1, 1, vec![0u8; 4])
    }

    /// Anything offered to PipeWire must be something this engine can decode.
    /// Adding a format to `SUPPORTED_VIDEO_FORMATS` without a matching arm in
    /// `video_frame_for` fails here rather than at runtime on a user's
    /// compositor.
    #[test]
    fn every_advertised_format_has_a_decode_arm() {
        for format in SUPPORTED_VIDEO_FORMATS {
            assert!(
                sample(format).is_some(),
                "advertised format {format:?} has no decode arm in video_frame_for"
            );
        }
    }

    /// Pins the specific regression: `RGBA` was advertised with no decode arm.
    #[test]
    fn rgba_is_not_advertised_while_undecodable() {
        assert!(
            sample(VideoFormat::RGBA).is_none(),
            "video_frame_for gained an RGBA arm -- add RGBA to \
             SUPPORTED_VIDEO_FORMATS and delete this test"
        );
        assert!(
            !SUPPORTED_VIDEO_FORMATS.contains(&VideoFormat::RGBA),
            "RGBA is advertised but video_frame_for cannot decode it"
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
