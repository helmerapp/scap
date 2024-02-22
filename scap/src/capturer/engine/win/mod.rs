use crate::{
    capturer::Options,
    frame::{BGRFrame, Frame},
};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::{env::temp_dir, io::ErrorKind};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFOEXW};
use windows_capture::{
    capture::{CaptureControl, WindowsCaptureHandler},
    frame::Frame as Wframe,
    graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl},
    monitor::Monitor,
    settings::{ColorFormat, Settings},
    window::Window,
};

struct Capturer {
    pub tx: mpsc::Sender<Frame>,
}

impl Capturer {
    pub fn new(tx: mpsc::Sender<Frame>) -> Self {
        Capturer { tx }
    }
}

pub struct WinStream {
    settings: Settings<mpsc::Sender<Frame>>,
    capture_control: Option<CaptureControl<Capturer, Box<dyn std::error::Error + Send + Sync>>>,
}

impl WindowsCaptureHandler for Capturer {
    type Flags = mpsc::Sender<Frame>;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(tx: Self::Flags) -> Result<Self, Self::Error> {
        Ok(Self { tx })
    }

    fn on_frame_arrived(
        &mut self,
        mut frame: &mut Wframe,
        _: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let mut frame_buffer = frame.buffer().unwrap();
        let raw_frame_buffer = frame_buffer.as_raw_buffer();
        let frame_data = raw_frame_buffer.to_vec();
        let bgr_frame = BGRFrame {
            display_time: 0,
            width: frame.width() as i32,
            height: frame.height() as i32,
            data: frame_data,
        };
        self.tx
            .send(Frame::BGR0(bgr_frame))
            .expect("Failed to send data");
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        println!("Closed");
        Ok(())
    }
}

impl WinStream {
    pub fn start_capture(&mut self) -> Result<(), io::Error> {
        let mut lock_file_path = temp_dir();
        lock_file_path.push("screen_capture.lock");

        if fs::metadata(&lock_file_path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Another capture instance is already running.",
            ));
        }

        fs::write(&lock_file_path, "")?;
        // TODO: Prevent cloning the transmitter
        let capture_control = Capturer::start_free_threaded(self.settings.clone()).unwrap();
        self.capture_control = Some(capture_control);
        Ok(())
    }

    pub fn stop_capture(&mut self) -> Result<(), io::Error> {
        let mut lock_file_path = temp_dir();
        lock_file_path.push("screen_capture.lock");
        fs::remove_file(&lock_file_path)?;

        let mut capture_control = self.capture_control.take().unwrap();
        capture_control.stop();
        Ok(())
    }
}

pub fn create_capturer(options: &Options, tx: mpsc::Sender<Frame>) -> WinStream {
    let settings = Settings::new(
        Monitor::primary().unwrap(),
        Some(true),
        None,
        ColorFormat::Rgba8,
        tx,
    )
    .unwrap();

    return WinStream {
        settings,
        capture_control: None,
    };
}
