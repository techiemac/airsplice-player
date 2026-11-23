//! Video decoding against fixtures with known content:
//! 1 s red, 1 s green, 1 s blue at 25 fps, 320x240 unless a test says otherwise.

use std::fs::File;
use std::path::{Path, PathBuf};

use airsplice_player::media::mp4_timing;
use airsplice_player::media::video::{DecodedFrame, VideoDecoder, make_hardware_decoder, make_software_decoder};
use symphonia::core::codecs::video::VideoCodecParameters;
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
    decode_video_limited(name, backend, None)
}

/// As `decode_video`, with the decoder told frames show no larger than `limit`.
fn decode_video_limited(name: &str, backend: Backend, limit: Option<(u32, u32)>) -> anyhow::Result<Option<Vec<(f64, DecodedFrame)>>> {
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
    dec.set_output_limit(limit);
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
    assert_red_green_blue_sized(name, backend, (320, 240))
}

fn assert_red_green_blue_sized(name: &str, backend: Backend, size: (u32, u32)) -> bool {
    assert_red_green_blue_limited(name, backend, None, size)
}

fn assert_red_green_blue_limited(name: &str, backend: Backend, limit: Option<(u32, u32)>, size: (u32, u32)) -> bool {
    let decoded = decode_video_limited(name, backend, limit).unwrap_or_else(|e| panic!("{name} ({backend:?}): {e:#}"));
    let Some(frames) = decoded else {
        eprintln!("skipping {name} on {backend:?}: not available on this platform");
        return false;
    };
    let name = format!("{name} ({backend:?})");
    assert_eq!(frames.len(), 75, "{name}: frame count");
    let start = frames[0].0;
    for (i, (pts, f)) in frames.iter().enumerate() {
        assert_eq!((f.width, f.height), size, "{name}: frame size");
        assert_eq!(f.bgra.len(), (size.0 * size.1 * 4) as usize, "{name}: frame {i} buffer");
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

/// Software decoders shrink frames to the size they are shown at (a 4K
/// picture in a 1080p window), never below it: a quarter of the conversion
/// and of the upload for each halving.
#[test]
fn software_decoders_shrink_frames_to_the_screen() {
    for name in ["vp8_opus.webm", "av1_opus.webm", "av1_10bit_opus51.mkv", "h264_aac.mp4"] {
        assert!(assert_red_green_blue_limited(name, Backend::Software, Some((160, 120)), (160, 120)), "{name}");
        assert!(assert_red_green_blue_limited(name, Backend::Software, Some((80, 60)), (80, 60)), "{name}");
        // A screen even a pixel bigger than half keeps the full picture.
        assert!(assert_red_green_blue_limited(name, Backend::Software, Some((161, 120)), (320, 240)), "{name}");
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

/// A fixture's video parameters and its video packets (timestamp, data) in
/// file order.
fn video_packets(name: &str) -> (VideoCodecParameters, Vec<(i64, Vec<u8>)>) {
    let path = fixture(name);
    let mss = MediaSourceStream::new(Box::new(File::open(&path).unwrap()), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(path.extension().unwrap().to_str().unwrap());
    let mut format =
        symphonia::default::get_probe().probe(&hint, mss, FormatOptions::default(), MetadataOptions::default()).unwrap();
    let track = format.default_track(TrackType::Video).unwrap().clone();
    let mut packets = Vec::new();
    while let Some(p) = format.next_packet().unwrap() {
        if p.track_id == track.id {
            packets.push((p.pts.get(), p.data.to_vec()));
        }
    }
    (track.codec_params.as_ref().unwrap().video().unwrap().clone(), packets)
}

/// Whether a VP8 frame is shown (`show_frame`, bit 4 of the frame tag).
fn vp8_shown(data: &[u8]) -> bool {
    data[0] & 0x10 != 0
}

#[test]
fn vp8_in_webm() {
    assert_red_green_blue("vp8_opus.webm", Backend::Software);
}

#[test]
fn vp8_has_no_hardware_decoder_and_that_is_not_an_error() {
    // An error here would be logged as a failed hardware decoder on every
    // VP8 file opened.
    let (params, _) = video_packets("vp8_opus.webm");
    assert!(matches!(make_hardware_decoder(&params), Ok(None)));
}

/// 321x181: the decoder works in whole 16x16 blocks and chroma rounds up to
/// 161x91, so the picture must be cropped to exactly the stored size.
#[test]
fn vp8_with_an_odd_frame_size() {
    assert_red_green_blue_sized("vp8_odd_size.webm", Backend::Software, (321, 181));
    // The last column and row, the ones without a full chroma sample pair.
    let frames = decode_video("vp8_odd_size.webm", Backend::Software).unwrap().unwrap();
    for (i, (_, f)) in frames.iter().enumerate() {
        let (b, g, r) = (f.bgra[f.bgra.len() - 4], f.bgra[f.bgra.len() - 3], f.bgra[f.bgra.len() - 2]);
        let want = [(255, 0, 0), (0, 255, 0), (0, 0, 255)][i / 25];
        let close = |a: u8, b: u8| a.abs_diff(b) <= 6;
        assert!(close(r, want.0) && close(g, want.1) && close(b, want.2), "frame {i} corner is ({r},{g},{b}), want {want:?}");
    }
}

/// Alternate reference frames are packets of their own that show nothing,
/// stamped with the time of the frame after them: a file has more packets
/// than pictures, and each picture keeps its own packet's timestamp.
#[test]
fn vp8_hidden_frames_are_decoded_but_not_shown() {
    let (params, packets) = video_packets("vp8_altref.webm");
    let hidden = packets.iter().filter(|(_, data)| !vp8_shown(data)).count();
    assert_eq!((packets.len(), hidden), (78, 3), "the fixture has hidden frames");
    let mut dec = make_software_decoder(&params).unwrap();
    let mut shown = Vec::new();
    for (pts, data) in &packets {
        let frames = dec.decode(data, *pts).unwrap();
        assert_eq!(frames.len(), usize::from(vp8_shown(data)), "packet at {pts} ms");
        shown.extend(frames.iter().map(|f| f.pts));
    }
    assert!(dec.flush().unwrap().is_empty(), "nothing is held back");
    // 25 fps on Matroska's millisecond clock.
    assert_eq!(shown, (0..75).map(|i| i * 40).collect::<Vec<i64>>());
    // The pictures themselves depend on the hidden frames being decoded.
    assert_red_green_blue("vp8_altref.webm", Backend::Software);
}

#[test]
fn vp8_resumes_at_a_keyframe_after_a_reset() {
    let (params, packets) = video_packets("vp8_altref.webm");
    let mut dec = make_software_decoder(&params).unwrap();
    for (pts, data) in packets.iter().filter(|p| p.0 < 500) {
        dec.decode(data, *pts).unwrap();
    }
    // A seek to 1.4 s, fed from the middle of the first GOP: inter frames
    // with no keyframe behind them are skipped (not errors), and frames from
    // the keyframe at 1 s up to the target are decoded but not returned.
    dec.reset(1400).unwrap();
    let mut shown = Vec::new();
    for (pts, data) in packets.iter().filter(|p| p.0 >= 600) {
        shown.extend(dec.decode(data, *pts).unwrap());
    }
    assert_eq!(shown.iter().map(|f| f.pts).collect::<Vec<_>>(), (35..75).map(|i| i * 40).collect::<Vec<i64>>());
    let (r, g, b) = centre_rgb(&shown[0]);
    assert!(r < 6 && g > 249 && b < 6, "first frame after the seek is ({r},{g},{b}), want green");
}

#[test]
fn codecs_without_a_software_decoder_say_so() {
    for name in ["hevc_aac.mp4", "vp9_opus.webm"] {
        let err = decode_video(name, Backend::Software).err().unwrap_or_else(|| panic!("{name} has no software decoder"));
        assert!(err.to_string().ends_with("video is not supported"), "{name}: {err}");
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

/// FNV-1a over each frame's BGRA, in display order. Taken from the Raspberry
/// Pi 5's V4L2 decode of `bframes_hevc.mkv`, which was verified bit-identical
/// to ffmpeg's software HEVC decoder followed by this player's BT.601
/// converter. Decoders that convert colour themselves (VideoToolbox) differ
/// slightly and are not held to it.
const BFRAMES_HEVC_GOLDEN: [u64; 50] = [
    0x09159aafc5400be9, 0x98c0440074356ea3, 0xcbc1cf1689d519c5, 0x180a3ff90910a59f, 0x3a5dc6137edf5e40,
    0x702558f8443bfbad, 0xfa0bbf3fc295657e, 0xe01070cb86953eee, 0x9cde897f188e8159, 0x4e3156d08c76e57c,
    0x51ec9037e9d28a94, 0xd594832f0cfebf91, 0x813a3ee068a56349, 0x16803b621d9d4548, 0x5ab66da4536daaca,
    0x3893596585fb84b7, 0xd4f960ba48fe1368, 0xcd54ec273946becf, 0xe9df5a41a9133e7b, 0x7a3582b6a5ee5d18,
    0x0bb1a6da2d1266f9, 0x0d79a51b0fba9b71, 0xfa9e99dfb68e5633, 0x75cdf1af169ca3be, 0x1642ad543798d032,
    0xfaba43ef81013ef1, 0x050c3da4d6f885ca, 0xd9551db811e49b43, 0x6c65024c676a31d0, 0xae444a26415bebe8,
    0xb0cc4c45b3c37943, 0x748c82133138606e, 0x382aff419dc34ff3, 0xd249f4e4b0ad25d4, 0xf6cdf4eaa1674e25,
    0xc501de67b995ec42, 0xb2d299bf7ede49db, 0x4565fe7064de77c7, 0x99c84318626d0b61, 0x3b2b87c09a09cb6f,
    0xe5ed978db2c26c75, 0x0c16a77db9bbe9d1, 0xad2feb57e0ffb773, 0x5b0f3886a9d46ed2, 0x4121c73cdd695927,
    0xdbf0436b999df1b2, 0x6d46eed0cd533282, 0x9d066c125b2037ab, 0x53087d7e2498fdc5, 0xbd6678f537edcd65,
];

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ *b as u64).wrapping_mul(0x0100_0000_01b3))
}

#[test]
fn v4l2_hevc_decode_is_bit_exact() {
    let path = fixture("bframes_hevc.mkv");
    let mss = MediaSourceStream::new(Box::new(File::open(&path).unwrap()), Default::default());
    let mut format = symphonia::default::get_probe()
        .probe(Hint::new().with_extension("mkv"), mss, FormatOptions::default(), MetadataOptions::default())
        .unwrap();
    let track = format.default_track(TrackType::Video).unwrap().clone();
    let Ok(Some(mut dec)) = make_hardware_decoder(track.codec_params.as_ref().unwrap().video().unwrap()) else {
        eprintln!("skipping: no hardware HEVC decoder here");
        return;
    };
    if !dec.name().contains("V4L2") {
        eprintln!("skipping: {} converts colour itself", dec.name());
        return;
    }
    let mut frames = Vec::new();
    while let Some(p) = format.next_packet().unwrap() {
        if p.track_id == track.id {
            frames.extend(dec.decode(&p.data, p.pts.get()).unwrap());
        }
    }
    frames.extend(dec.flush().unwrap());
    let hashes: Vec<u64> = frames.iter().map(|f| fnv1a(&f.bgra)).collect();
    assert_eq!(hashes.len(), 50);
    for (i, (got, want)) in hashes.iter().zip(BFRAMES_HEVC_GOLDEN).enumerate() {
        assert_eq!(*got, want, "frame {i} differs from the verified decode");
    }
}
