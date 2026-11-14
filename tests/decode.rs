//! Video decoding against fixtures with known content:
//! 1 s red, 1 s green, 1 s blue at 25 fps, 320x240.

use std::fs::File;
use std::path::{Path, PathBuf};

use media_play::media::video::{DecodedFrame, make_decoder};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Timestamp;

pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// Returns frames with PTS converted to seconds, sorted by PTS.
fn decode_video(name: &str) -> anyhow::Result<Vec<(f64, DecodedFrame)>> {
    let path = fixture(name);
    let mss = MediaSourceStream::new(Box::new(File::open(&path)?), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(path.extension().unwrap().to_str().unwrap());
    let mut format =
        symphonia::default::get_probe().probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())?;
    let track = format.default_track(TrackType::Video).expect("video track").clone();
    let tb = track.time_base.unwrap();
    let mut dec = make_decoder(track.codec_params.as_ref().unwrap().video().unwrap())?;
    let mut frames = Vec::new();
    while let Some(packet) = format.next_packet()? {
        if packet.track_id == track.id {
            frames.extend(dec.decode(&packet.data, packet.pts.get())?);
        }
    }
    frames.extend(dec.flush()?);
    let mut out: Vec<_> =
        frames.into_iter().map(|f| (tb.calc_time(Timestamp::new(f.pts)).unwrap().as_secs_f64(), f)).collect();
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    Ok(out)
}

fn centre_rgb(f: &DecodedFrame) -> (u8, u8, u8) {
    let i = ((f.height / 2 * f.width + f.width / 2) * 4) as usize;
    (f.bgra[i + 2], f.bgra[i + 1], f.bgra[i])
}

fn assert_red_green_blue(name: &str) {
    let frames = decode_video(name).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(frames.len(), 75, "{name}: frame count");
    let start = frames[0].0;
    for (i, (pts, f)) in frames.iter().enumerate() {
        assert_eq!((f.width, f.height), (320, 240), "{name}: frame size");
        let expected = start + i as f64 / 25.0;
        assert!((pts - expected).abs() < 0.002, "{name}: frame {i} pts {pts:.3}, want {expected:.3}");
        let (r, g, b) = centre_rgb(f);
        let want = match i / 25 {
            0 => (255, 0, 0),
            1 => (0, 255, 0),
            _ => (0, 0, 255),
        };
        let close = |a: u8, b: u8| (a as i32 - b as i32).abs() <= 6;
        assert!(
            close(r, want.0) && close(g, want.1) && close(b, want.2),
            "{name}: frame {i} centre is ({r},{g},{b}), want {want:?}"
        );
    }
}

#[test]
fn h264_in_mp4_with_b_frames() {
    assert_red_green_blue("h264_aac.mp4");
}

#[test]
fn h264_in_matroska() {
    assert_red_green_blue("h264_mp3.mkv");
}

#[test]
fn h264_video_only() {
    assert_red_green_blue("video_only.mp4");
}

#[test]
fn av1_in_webm() {
    assert_red_green_blue("av1_opus.webm");
}

#[test]
fn av1_ten_bit_in_matroska() {
    assert_red_green_blue("av1_10bit_opus51.mkv");
}

#[test]
fn hevc_is_reported_as_unsupported() {
    let err = decode_video("hevc_aac.mp4").err().expect("HEVC should be rejected");
    assert!(err.to_string().contains("HEVC"), "{err}");
}
