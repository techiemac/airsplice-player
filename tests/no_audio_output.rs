//! With no audio output connected, video still plays (without sound) and
//! audio-only files say why they can't. Its own test binary: it sets an
//! environment variable for the whole process.

use std::path::Path;
use std::time::{Duration, Instant};

use airsplice_player::media::{Player, audio};

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

#[test]
fn video_plays_without_sound_when_no_audio_output_is_connected() {
    // SAFETY: the only test in this binary; nothing else reads the
    // environment concurrently.
    unsafe { std::env::set_var(audio::NO_OUTPUT_ENV, "1") };
    for name in ["vp8_opus.webm", "av1_opus.webm", "h264_aac.mp4"] {
        let player = Player::open(&fixture(name), 0.0, true).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        assert!(player.info.audio.is_none(), "{name}");
        assert_eq!(player.info.audio_unavailable.as_deref(), Some("no audio output"), "{name}");
        // It plays: frames keep coming on the wall clock.
        let start = Instant::now();
        let mut last = -1.0;
        while start.elapsed() < Duration::from_secs(5) && last < 1.0 {
            if let Some(f) = player.take_frame() {
                last = f.pts;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(last >= 1.0, "{name}: reached {last:.2} s");
    }
    let err = Player::open(&fixture("stereo.ogg"), 0.0, true).err().expect("audio only cannot play").to_string();
    assert_eq!(err, "audio output unavailable");
}
