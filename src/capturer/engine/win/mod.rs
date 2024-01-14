use std::sync::mpsc;
use std::{borrow::Borrow, error::Error};

use crate::{
    capturer::Options,
    frame::{Frame, YUVFrame},
};
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
        let width = frame_buffer.width();
        let height = frame_buffer.height();

        let raw_frame_buffer = frame_buffer.as_raw_buffer();
        let frame_data = raw_frame_buffer.to_vec();

        let yuv_data = to_yuv(&raw_frame_buffer, width, height);

        self.tx
            .send(Frame::YUVFrame(yuv_data))
            .expect("Failed to send data");

        self.tx
            .send(Frame::BGR0(frame_data))
            .expect("Failed to send data");
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        println!("Closed");
        Ok(())
    }
}

impl WinStream {
    pub fn start_capture(&self) {
        // TODO: Prevent cloning the transmitter
        Capturer::start_free_threaded(self.settings.clone());
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

    return WinStream { settings };
}

pub fn to_yuv(img: &[u8], width: u32, height: u32) -> YUVFrame {
    let mut luminance_bytes: Vec<u8> = Vec::new();
    let mut chrominance_bytes: Vec<u8> = Vec::new();

    let mut index: usize = 0;
    for y in 0..height {
        for x in 0..width {
            let r = img[index] as f64;
            let g = img[index + 1] as f64;
            let b = img[index + 2] as f64;
            index += 1;

            let y = 0.299 * r + 0.587 * g + 0.114 * b;
            luminance_bytes.push(y as u8);

            let u = -0.14713 * r + -0.28886 * g + 0.436 * b;
            let v = 0.615 * r + -0.51499 * g + -0.10001 * b;

            chrominance_bytes.push(u as u8);
            chrominance_bytes.push(v as u8);
        }
    }

    YUVFrame {
        display_time: 0,
        width: width as i32,
        height: height as i32,
        luminance_bytes,
        luminance_stride: width as i32,
        chrominance_bytes,
        chrominance_stride: width as i32,
    }
}
