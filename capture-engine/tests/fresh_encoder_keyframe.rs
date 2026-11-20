//! A rebuilt encoder must be able to produce its keyframe from a frame it
//! did not see before the rebuild.
//!
//! The worker rebuilds the encoder on every keyframe request (PLI) and
//! settings change. On a static desktop the capture only yields placeholder
//! ticks, so the only picture available is the last content frame, which
//! may carry a small dirty-rect hint from when it was captured. The rebuilt
//! encoder's `encode_cached` is empty by design; the worker therefore feeds
//! that last frame through `encode(.., false)` until the encoder emits its
//! first access unit, which has to be a keyframe.

use desk_capture_engine::model::image_capture::{DirtyRect, ImageInfo, ImageType};
use desk_capture_engine::model::video_encoder::VideoEncoder;
#[cfg(av1_supported)]
use desk_capture_engine::video_encoder::av1_encoder::Av1Encoder;
use desk_capture_engine::video_encoder::h264_encoder::H264Encoder;
use desk_capture_engine::video_encoder::vpx_encoder::VpxEncoder;
use desk_capture_engine::video_encoder::x264_encoder::X264Encoder;
#[cfg(av1_supported)]
use desk_signal_facade::model::desk_settings::Av1EncoderSettings;
use desk_signal_facade::model::desk_settings::{
    H264EncoderSettings, VpxEncoderSettings, X264EncoderSettings,
};
use desk_signal_facade::model::image_capture::{DisplayInfo, DisplayRect};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;
/// Upper bound on re-feeding the same frame to an encoder that buffers
/// input (VP9 lag-in-frames, AV1). The worker re-feeds at the connection's
/// fps, so this is well under a second of real time at 30-60 fps.
const MAX_FEEDS: usize = 30;

/// A captured frame as the shared capture publishes it: the whole surface
/// in `data`, plus the dirty-rect hint the backend reported for it.
struct CapturedFrame {
    data: Vec<u8>,
    dirty_rects: Option<Vec<DirtyRect>>,
}

impl CapturedFrame {
    fn new(seed: u32, dirty_rects: Option<Vec<DirtyRect>>) -> Self {
        let mut data = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
        let block_x = (seed * 37) % (WIDTH - 64);
        let block_y = (seed * 23) % (HEIGHT - 64);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let idx = ((y * WIDTH + x) * 4) as usize;
                let in_block = x >= block_x && x < block_x + 64 && y >= block_y && y < block_y + 64;
                if in_block {
                    data[idx] = 255;
                    data[idx + 1] = (seed * 41) as u8;
                    data[idx + 2] = 32;
                } else {
                    data[idx] = (x & 0xFF) as u8;
                    data[idx + 1] = (y & 0xFF) as u8;
                    data[idx + 2] = ((x + y + seed) & 0xFF) as u8;
                }
                data[idx + 3] = 255;
            }
        }
        Self { data, dirty_rects }
    }
}

impl ImageInfo for CapturedFrame {
    fn get_type(&self) -> ImageType {
        ImageType::BGRA
    }
    fn get_data(&self) -> &[u8] {
        &self.data
    }
    fn get_width(&self) -> u32 {
        WIDTH
    }
    fn get_height(&self) -> u32 {
        HEIGHT
    }
    fn get_dirty_rects(&self) -> Option<&[DirtyRect]> {
        self.dirty_rects.as_deref()
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

/// Runs one encoder through the static-desktop recovery sequence.
fn assert_rebuilt_encoder_recovers(mut build: impl FnMut() -> Box<dyn VideoEncoder>, label: &str) {
    // A running encoder that has already streamed some content.
    let mut running = build();
    for seed in 1..=MAX_FEEDS as u32 {
        running
            .encode(&CapturedFrame::new(seed, None), true)
            .unwrap_or_else(|e| panic!("{label}: warm-up encode failed: {e:?}"));
    }

    // Keyframe request: the encoder is replaced and holds no picture.
    let mut rebuilt = build();
    assert!(
        rebuilt
            .encode_cached()
            .unwrap_or_else(|e| panic!("{label}: encode_cached failed: {e:?}"))
            .is_empty(),
        "{label}: a fresh encoder has nothing cached; static ticks cannot rely on it"
    );

    // The latest content frame arrived with only a small region marked
    // dirty; the rebuilt encoder must still get the whole picture.
    let latest = CapturedFrame::new(
        99,
        Some(vec![DirtyRect {
            x: 8,
            y: 8,
            width: 16,
            height: 16,
        }]),
    );
    for feed in 1..=MAX_FEEDS {
        let nals = rebuilt
            .encode(&latest, false)
            .unwrap_or_else(|e| panic!("{label}: re-encode {feed} failed: {e:?}"));
        if nals.is_empty() {
            continue;
        }
        assert!(
            nals.iter().any(|nal| nal.is_keyframe),
            "{label}: first access unit after the rebuild must be a keyframe"
        );
        return;
    }
    panic!("{label}: no output after re-feeding the latest frame {MAX_FEEDS} times");
}

#[test]
fn x264_rebuilt_encoder_emits_keyframe_from_latest_frame() {
    assert_rebuilt_encoder_recovers(
        || {
            Box::new(
                X264Encoder::new(X264EncoderSettings::default(), &display_info(), 30)
                    .expect("x264 build failed"),
            )
        },
        "x264",
    );
}

#[test]
fn openh264_rebuilt_encoder_emits_keyframe_from_latest_frame() {
    assert_rebuilt_encoder_recovers(
        || {
            Box::new(H264Encoder::new(H264EncoderSettings {
                gop: 120,
                ..Default::default()
            }))
        },
        "openh264",
    );
}

#[test]
fn vp8_rebuilt_encoder_emits_keyframe_from_latest_frame() {
    assert_rebuilt_encoder_recovers(
        || {
            Box::new(
                VpxEncoder::new(
                    vpx_encode::VideoCodecId::VP8,
                    VpxEncoderSettings::default(),
                    &display_info(),
                )
                .expect("vp8 build failed"),
            )
        },
        "vp8",
    );
}

#[test]
fn vp9_rebuilt_encoder_emits_keyframe_from_latest_frame() {
    assert_rebuilt_encoder_recovers(
        || {
            Box::new(
                VpxEncoder::new(
                    vpx_encode::VideoCodecId::VP9,
                    VpxEncoderSettings::default(),
                    &display_info(),
                )
                .expect("vp9 build failed"),
            )
        },
        "vp9",
    );
}

#[cfg(av1_supported)]
#[test]
fn av1_rebuilt_encoder_emits_keyframe_from_latest_frame() {
    assert_rebuilt_encoder_recovers(
        || {
            Box::new(
                Av1Encoder::new(
                    Av1EncoderSettings {
                        rtc: true,
                        ..Default::default()
                    },
                    &display_info(),
                    30,
                )
                .expect("av1 build failed"),
            )
        },
        "av1",
    );
}
