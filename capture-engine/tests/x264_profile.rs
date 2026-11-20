//! The x264 stream must stay decodable by browsers at every `video_quality`.
//!
//! CRF 0 makes an 8-bit x264 code losslessly, which needs the High 4:4:4
//! Predictive profile (profile_idc 244); browsers render that stream as
//! garbage once predicted frames arrive. The adaptive quality loop reaches
//! quality 0 on any clean link, so the encoder must never emit it.

use desk_capture_engine::model::image_capture::{ImageInfo, ImageType};
use desk_capture_engine::model::video_encoder::VideoEncoder;
use desk_capture_engine::video_encoder::x264_encoder::X264Encoder;
use desk_signal_facade::model::desk_settings::X264EncoderSettings;
use desk_signal_facade::model::image_capture::{DisplayInfo, DisplayRect};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const HIGH_444_PREDICTIVE: u8 = 244;

struct Gradient(Vec<u8>);

impl Gradient {
    fn new(seed: u32) -> Self {
        let mut data = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let idx = ((y * WIDTH + x) * 4) as usize;
                data[idx] = (x & 0xFF) as u8;
                data[idx + 1] = (y & 0xFF) as u8;
                data[idx + 2] = ((x + y + seed) & 0xFF) as u8;
                data[idx + 3] = 255;
            }
        }
        Self(data)
    }
}

impl ImageInfo for Gradient {
    fn get_type(&self) -> ImageType {
        ImageType::BGRA
    }
    fn get_data(&self) -> &[u8] {
        &self.0
    }
    fn get_width(&self) -> u32 {
        WIDTH
    }
    fn get_height(&self) -> u32 {
        HEIGHT
    }
}

fn display_info() -> DisplayInfo {
    DisplayInfo {
        device_name: r"\\.\TESTDISPLAY".to_string(),
        display_device_name: None,
        desktop_coordinates: DisplayRect {
            left: 0,
            top: 0,
            right: WIDTH as i32,
            bottom: HEIGHT as i32,
        },
        resolutions: vec![],
        attached_to_desktop: true,
        rotation: 0,
        current_capture_resolution: None,
    }
}

/// profile_idc of the first SPS in an Annex B byte stream.
fn sps_profile(stream: &[u8]) -> Option<u8> {
    stream
        .windows(5)
        .find(|w| w[..3] == [0, 0, 1] && w[3] & 0x1F == 7)
        .map(|w| w[4])
}

fn first_sps_profile(quality: u32) -> u8 {
    let settings = X264EncoderSettings {
        quality,
        ..X264EncoderSettings::default()
    };
    let mut encoder = X264Encoder::new(settings, &display_info(), 30).expect("x264 build failed");
    let mut stream = Vec::new();
    for seed in 0..3 {
        for nal in encoder
            .encode(&Gradient::new(seed), false)
            .expect("x264 encode failed")
        {
            stream.extend_from_slice(&nal.nal_bytes);
        }
    }
    sps_profile(&stream).expect("x264 emitted no SPS")
}

#[test]
fn quality_zero_does_not_switch_x264_to_lossless() {
    assert_ne!(first_sps_profile(0), HIGH_444_PREDICTIVE);
}

#[test]
fn every_quality_keeps_a_browser_decodable_profile() {
    for quality in [1, 22, 51, 63] {
        assert_ne!(
            first_sps_profile(quality),
            HIGH_444_PREDICTIVE,
            "quality {quality}"
        );
    }
}
