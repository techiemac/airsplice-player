//! Video decoding against fixtures with known content:
//! 1 s red, 1 s green, 1 s blue at 25 fps, 320x240.

use std::fs::File;
use std::path::{Path, PathBuf};

use media_play::media::mp4_timing;
use media_play::media::video::{DecodedFrame, VideoDecoder, make_hardware_decoder, make_software_decoder};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Timestamp;

pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

#[derive(Clone, Copy, Debug)]
enum Backend {
    Software,
    Hardware,
}

/// Returns frames with PTS converted to seconds, sorted by PTS, or `None`
/// when this platform has no hardware decoder for the codec.
fn decode_video(name: &str, backend: Backend) -> anyhow::Result<Option<Vec<(f64, DecodedFrame)>>> {
    let path = fixture(name);
    let mss = MediaSourceStream::new(Box::new(File::open(&path)?), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(path.extension().unwrap().to_str().unwrap());
    let mut format =
        symphonia::default::get_probe().probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())?;
    let track = format.default_track(TrackType::Video).expect("video track").clone();
    let tb = track.time_base.unwrap();
    let params = track.codec_params.as_ref().unwrap().video().unwrap();
    let mut dec: Box<dyn VideoDecoder> = match backend {
        Backend::Software => make_software_decoder(params)?,
        Backend::Hardware => match make_hardware_decoder(params) {
            Ok(Some(d)) => d,
            Ok(None) => return Ok(None),
            Err(e) => {
                eprintln!("{name}: no hardware decoder here: {e:#}");
                return Ok(None);
            }
        },
    };
    let mut frames = Vec::new();
    // Presentation times, as the engine computes them (Symphonia reports
    // raw decode times for MP4).
    let timing = mp4_timing::read(&path).remove(&track.id).unwrap_or_default();
    while let Some(packet) = format.next_packet()? {
        if packet.track_id == track.id {
            frames.extend(dec.decode(&packet.data, timing.pts(packet.pts.get()))?);
        }
    }
    frames.extend(dec.flush()?);
    let mut out: Vec<_> =
        frames.into_iter().map(|f| (tb.calc_time(Timestamp::new(f.pts)).unwrap().as_secs_f64(), f)).collect();
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    Ok(Some(out))
}

fn centre_rgb(f: &DecodedFrame) -> (u8, u8, u8) {
    let i = ((f.height / 2 * f.width + f.width / 2) * 4) as usize;
    (f.bgra[i + 2], f.bgra[i + 1], f.bgra[i])
}

/// Returns false if the backend is not available on this platform.
fn assert_red_green_blue(name: &str, backend: Backend) -> bool {
    let Some(frames) = decode_video(name, backend).unwrap_or_else(|e| panic!("{name} ({backend:?}): {e:#}")) else {
        eprintln!("skipping {name} on {backend:?}: not available on this platform");
        return false;
    };
    let name = format!("{name} ({backend:?})");
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
    true
}

/// Hardware decoders macOS guarantees on Apple silicon; elsewhere they are
/// optional and skip.
fn expect_hardware(available: bool, what: &str) {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        assert!(available, "{what} should have a hardware decoder on Apple silicon");
    }
}

#[test]
fn h264_in_mp4_with_b_frames() {
    assert_red_green_blue("h264_aac.mp4", Backend::Software);
    expect_hardware(assert_red_green_blue("h264_aac.mp4", Backend::Hardware), "H.264");
}

#[test]
fn h264_in_matroska() {
    assert_red_green_blue("h264_mp3.mkv", Backend::Software);
    expect_hardware(assert_red_green_blue("h264_mp3.mkv", Backend::Hardware), "H.264");
}

#[test]
fn h264_video_only() {
    assert_red_green_blue("video_only.mp4", Backend::Software);
    expect_hardware(assert_red_green_blue("video_only.mp4", Backend::Hardware), "H.264");
}

#[test]
fn av1_in_webm() {
    assert_red_green_blue("av1_opus.webm", Backend::Software);
    // AV1 hardware decoding needs M3 or later; not asserted.
    assert_red_green_blue("av1_opus.webm", Backend::Hardware);
}

#[test]
fn av1_ten_bit_in_matroska() {
    assert_red_green_blue("av1_10bit_opus51.mkv", Backend::Software);
    assert_red_green_blue("av1_10bit_opus51.mkv", Backend::Hardware);
}

#[test]
fn hevc_in_mp4() {
    expect_hardware(assert_red_green_blue("hevc_aac.mp4", Backend::Hardware), "HEVC");
}

#[test]
fn hevc_ten_bit_in_matroska() {
    expect_hardware(assert_red_green_blue("hevc_10bit_opus.mkv", Backend::Hardware), "HEVC Main 10");
}

#[test]
fn vp9_in_webm() {
    expect_hardware(assert_red_green_blue("vp9_opus.webm", Backend::Hardware), "VP9");
}

#[test]
fn codecs_without_a_software_decoder_say_so() {
    for name in ["hevc_aac.mp4", "vp9_opus.webm"] {
        let err = decode_video(name, Backend::Software).err().unwrap_or_else(|| panic!("{name} has no software decoder"));
        assert!(err.to_string().contains("no software decoder"), "{name}: {err}");
    }
}

/// Frames as each decoder emits them, before any sorting.
fn emitted_pts(name: &str, backend: Backend) -> Option<Vec<i64>> {
    let path = fixture(name);
    let mss = MediaSourceStream::new(Box::new(File::open(&path).unwrap()), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(path.extension().unwrap().to_str().unwrap());
    let mut format =
        symphonia::default::get_probe().probe(&hint, mss, FormatOptions::default(), MetadataOptions::default()).unwrap();
    let track = format.default_track(TrackType::Video).unwrap().clone();
    let params = track.codec_params.as_ref().unwrap().video().unwrap();
    let mut dec = match backend {
        Backend::Software => make_software_decoder(params).ok()?,
        Backend::Hardware => make_hardware_decoder(params).ok()??,
    };
    let timing = mp4_timing::read(&path).remove(&track.id).unwrap_or_default();
    let mut out = Vec::new();
    while let Some(p) = format.next_packet().unwrap() {
        if p.track_id == track.id {
            out.extend(dec.decode(&p.data, timing.pts(p.pts.get())).unwrap().into_iter().map(|f| f.pts));
        }
    }
    out.extend(dec.flush().unwrap().into_iter().map(|f| f.pts));
    Some(out)
}

/// With B-frames, decode order differs from display order. Decoders must
/// emit display order, or playback steps backwards.
#[test]
fn decoders_emit_frames_in_display_order() {
    let cases = [
        ("bframes_h264.mp4", Backend::Software),
        ("bframes_h264.mp4", Backend::Hardware),
        ("bframes_hevc.mkv", Backend::Hardware),
    ];
    for (name, backend) in cases {
        let Some(pts) = emitted_pts(name, backend) else {
            eprintln!("skipping {name} on {backend:?}: not available");
            continue;
        };
        assert_eq!(pts.len(), 50, "{name} ({backend:?}): frame count");
        assert!(pts.windows(2).all(|w| w[1] > w[0]), "{name} ({backend:?}) emitted out of order: {pts:?}");
    }
}
