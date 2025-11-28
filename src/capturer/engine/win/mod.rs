use crate::{
    capturer::{Area, Options, Point, Resolution, Size},
    frame::{AudioFormat, AudioFrame, BGRAFrame, Frame, FrameType, VideoFrame},
    targets::{self, get_scale_factor, Target},
};
use ::windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    StreamInstant,
};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{cmp, time::Duration};
use std::{
    os::windows,
    ptr::null_mut,
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
};
use windows_capture::{
    capture::{CaptureControl, Context, GraphicsCaptureApiHandler},
    frame::Frame as WCFrame,
    graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl},
    monitor::Monitor as WCMonitor,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings as WCSettings,
    },
    window::Window as WCWindow,
};

#[derive(Debug)]
struct Capturer {
    pub tx: mpsc::Sender<Frame>,
    pub crop: Option<Area>,
    pub start_time: (i64, SystemTime),
    pub perf_freq: i64,
}

#[derive(Clone)]
enum Settings {
    Window(WCSettings<FlagStruct, WCWindow>),
    Display(WCSettings<FlagStruct, WCMonitor>),
}

pub struct WCStream {
    settings: Settings,
    capture_control: Option<CaptureControl<Capturer, Box<dyn std::error::Error + Send + Sync>>>,
    audio_stream: Option<AudioStreamHandle>,
}

impl GraphicsCaptureApiHandler for Capturer {
    type Flags = FlagStruct;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(context: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            tx: context.flags.tx,
            crop: context.flags.crop,
            start_time: (
                unsafe {
                    let mut time = 0;
                    QueryPerformanceCounter(&mut time);
                    time
                },
                SystemTime::now(),
            ),
            perf_freq: unsafe {
                let mut freq = 0;
                QueryPerformanceFrequency(&mut freq);
                freq
            },
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut WCFrame,
        _: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let elapsed = frame.timestamp().Duration - self.start_time.0;
        let display_time = self
            .start_time
            .1
            .checked_add(Duration::from_secs_f64(
                elapsed as f64 / self.perf_freq as f64,
            ))
            .unwrap();

        match &self.crop {
            Some(cropped_area) => {
                // get the cropped area
                let start_x = cropped_area.origin.x as u32;
                let start_y = cropped_area.origin.y as u32;
                let end_x = (cropped_area.origin.x + cropped_area.size.width) as u32;
                let end_y = (cropped_area.origin.y + cropped_area.size.height) as u32;

                // crop the frame
                let mut cropped_buffer = frame
                    .buffer_crop(start_x, start_y, end_x, end_y)
                    .expect("Failed to crop buffer");

                // get raw frame buffer
                let raw_frame_buffer = match cropped_buffer.as_nopadding_buffer() {
                    Ok(buffer) => buffer,
                    Err(_) => return Err(("Failed to get raw buffer").into()),
                };

                let current_time = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("Failed to get current time")
                    .as_nanos() as u64;

                let bgr_frame = BGRAFrame {
                    display_time,
                    width: cropped_area.size.width as i32,
                    height: cropped_area.size.height as i32,
                    data: raw_frame_buffer.to_vec(),
                };

                let _ = self.tx.send(Frame::Video(VideoFrame::BGRA(bgr_frame)));
            }
            None => {
                // get raw frame buffer
                let mut frame_buffer = frame.buffer().unwrap();
                let raw_frame_buffer = frame_buffer.as_raw_buffer();
                let frame_data = raw_frame_buffer.to_vec();
                let current_time = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("Failed to get current time")
                    .as_nanos() as u64;
                let bgr_frame = BGRAFrame {
                    display_time,
                    width: frame.width() as i32,
                    height: frame.height() as i32,
                    data: frame_data,
                };

                let _ = self.tx.send(Frame::Video(VideoFrame::BGRA(bgr_frame)));
            }
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        println!("Closed");
        Ok(())
    }
}

impl WCStream {
    pub fn start_capture(&mut self) {
        let cc = match &self.settings {
            Settings::Display(st) => Capturer::start_free_threaded(st.to_owned()).unwrap(),
            Settings::Window(st) => Capturer::start_free_threaded(st.to_owned()).unwrap(),
        };

        if let Some(audio_stream) = &self.audio_stream {
            let _ = audio_stream.ctrl_tx.send(AudioStreamControl::Start);
        }

        self.capture_control = Some(cc)
    }

    pub fn stop_capture(&mut self) {
        let capture_control = self.capture_control.take().unwrap();
        let _ = capture_control.stop();

        if let Some(audio_stream) = &self.audio_stream {
            let _ = audio_stream.ctrl_tx.send(AudioStreamControl::Stop);
        }
    }
}

#[derive(Clone, Debug)]
struct FlagStruct {
    pub tx: mpsc::Sender<Frame>,
    pub crop: Option<Area>,
}

#[derive(Debug)]
pub enum CreateCapturerError {
    AudioStreamConfig(cpal::DefaultStreamConfigError),
    BuildAudioStream(cpal::BuildStreamError),
}

pub fn create_capturer(
    options: &Options,
    tx: mpsc::Sender<Frame>,
) -> Result<WCStream, CreateCapturerError> {
    let target = options
        .target
        .clone()
        .unwrap_or_else(|| Target::Display(targets::get_main_display()));

    let color_format = match options.output_type {
        FrameType::BGRAFrame => ColorFormat::Bgra8,
        _ => ColorFormat::Rgba8,
    };

    let show_cursor = match options.show_cursor {
        true => CursorCaptureSettings::WithCursor,
        false => CursorCaptureSettings::WithoutCursor,
    };

    let draw_border = if GraphicsCaptureApi::is_border_settings_supported().unwrap_or(false) {
        options
            .show_highlight
            .then_some(DrawBorderSettings::WithBorder)
            .unwrap_or(DrawBorderSettings::WithoutBorder)
    } else {
        DrawBorderSettings::Default
    };

    let settings = match target {
        Target::Display(display) => Settings::Display(WCSettings::new(
            WCMonitor::from_raw_hmonitor(display.raw_handle.0),
            show_cursor,
            draw_border,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            color_format,
            FlagStruct {
                tx: tx.clone(),
                crop: Some(get_crop_area(options)),
            },
        )),
        Target::Window(window) => Settings::Window(WCSettings::new(
            WCWindow::from_raw_hwnd(window.raw_handle.0),
            show_cursor,
            draw_border,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            color_format,
            FlagStruct {
                tx: tx.clone(),
                crop: Some(get_crop_area(options)),
            },
        )),
    };

    let audio_stream = if options.captures_audio {
        let (ctrl_tx, ctrl_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();

        spawn_audio_stream(tx.clone(), ready_tx, ctrl_rx);

        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => panic!("Audio spawn panicked"),
        }

        Some(AudioStreamHandle { ctrl_tx })
    } else {
        None
    };

    Ok(WCStream {
        settings,
        capture_control: None,
        audio_stream,
    })
}

pub fn get_output_frame_size(options: &Options) -> [u32; 2] {
    let target = options
        .target
        .clone()
        .unwrap_or_else(|| Target::Display(targets::get_main_display()));

    let crop_area = get_crop_area(options);

    let mut output_width = (crop_area.size.width) as u32;
    let mut output_height = (crop_area.size.height) as u32;

    match options.output_resolution {
        Resolution::Captured => {}
        _ => {
            let [resolved_width, resolved_height] = options
                .output_resolution
                .value((crop_area.size.width as f32) / (crop_area.size.height as f32));
            // 1280 x 853
            output_width = cmp::min(output_width, resolved_width);
            output_height = cmp::min(output_height, resolved_height);
        }
    }

    output_width -= output_width % 2;
    output_height -= output_height % 2;

    [output_width, output_height]
}

fn get_absolute_value(value: f64, scale_factor: f64) -> f64 {
    let value = (value).floor();
    value + value % 2.0
}

pub fn get_crop_area(options: &Options) -> Area {
    let target = options
        .target
        .clone()
        .unwrap_or_else(|| Target::Display(targets::get_main_display()));

    let (width, height) = targets::get_target_dimensions(&target);

    let scale_factor = targets::get_scale_factor(&target);
    options
        .crop_area
        .as_ref()
        .map(|val| {
            // WINDOWS: limit values [input-width, input-height] = [146, 50]
            Area {
                origin: Point {
                    x: get_absolute_value(val.origin.x, scale_factor),
                    y: get_absolute_value(val.origin.y, scale_factor),
                },
                size: Size {
                    width: get_absolute_value(val.size.width, scale_factor),
                    height: get_absolute_value(val.size.height, scale_factor),
                },
            }
        })
        .unwrap_or_else(|| Area {
            origin: Point { x: 0.0, y: 0.0 },
            size: Size {
                width: width as f64,
                height: height as f64,
            },
        })
}

struct AudioStreamHandle {
    ctrl_tx: mpsc::Sender<AudioStreamControl>,
}

#[derive(Debug)]
enum AudioStreamControl {
    Start,
    Stop,
}

fn build_audio_stream(
    sample_tx: mpsc::Sender<
        Result<(Vec<u8>, cpal::InputCallbackInfo, SystemTime), cpal::StreamError>,
    >,
) -> Result<(cpal::Stream, cpal::SupportedStreamConfig), CreateCapturerError> {
    let host = cpal::default_host();
    let output_device =
        host.default_output_device()
            .ok_or(CreateCapturerError::AudioStreamConfig(
                cpal::DefaultStreamConfigError::DeviceNotAvailable,
            ))?;
    let supported_config = output_device
        .default_output_config()
        .map_err(CreateCapturerError::AudioStreamConfig)?;
    let config = supported_config.clone().into();

    let stream = output_device
        .build_input_stream_raw(
            &config,
            supported_config.sample_format(),
            {
                let sample_tx = sample_tx.clone();
                move |data, info: &cpal::InputCallbackInfo| {
                    sample_tx
                        .send(Ok((data.bytes().to_vec(), info.clone(), SystemTime::now())))
                        .unwrap();
                }
            },
            move |e| {
                let _ = sample_tx.send(Err(e));
            },
            None,
        )
        .map_err(CreateCapturerError::BuildAudioStream)?;

    Ok((stream, supported_config))
}

fn spawn_audio_stream(
    tx: Sender<Frame>,
    ready_tx: Sender<Result<(), CreateCapturerError>>,
    ctrl_rx: Receiver<AudioStreamControl>,
) {
    std::thread::spawn(move || {
        let (sample_tx, sample_rx) = mpsc::channel();

        let res = build_audio_stream(sample_tx);

        let (stream, config) = match res {
            Ok(stream) => {
                let _ = ready_tx.send(Ok(()));
                stream
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };

        let Ok(ctrl) = ctrl_rx.recv() else {
            return;
        };

        match ctrl {
            AudioStreamControl::Start => {
                stream.play().unwrap();
            }
            AudioStreamControl::Stop => {
                return;
            }
        }

        let audio_format = AudioFormat::from(config.sample_format());

        loop {
            match ctrl_rx.try_recv() {
                Ok(AudioStreamControl::Stop) => return,
                Ok(_) | Err(mpsc::TryRecvError::Empty) => {}
                Err(_) => return,
            };

            let (data, info, timestamp) = match sample_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(data)) => data,
                Err(RecvTimeoutError::Timeout) => {
                    continue;
                }
                _ => {
                    // let _ = tx.send(Err(e));
                    return;
                }
            };

            let sample_count =
                data.len() / (audio_format.sample_size() * config.channels() as usize);
            let frame = AudioFrame::new(
                audio_format,
                config.channels(),
                false,
                data,
                sample_count,
                config.sample_rate().0,
                timestamp,
            );

            if let Err(_) = tx.send(Frame::Audio(frame)) {
                return;
            };
        }
    });
}
