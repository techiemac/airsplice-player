//! End-to-end tests through the real engine: demux and decode threads, the
//! clock, pause, seek and end of stream. Fixture video is 1 s red, 1 s green,
//! 1 s blue at 25 fps.
//!
//! Tests on files with audio need an output device; without one they skip
//! (and say so) rather than fail.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use airsplice_player::media::{Player, VideoFrame};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn open(name: &str) -> Option<Player> {
    match Player::open(&fixture(name), 0.0, true) {
        Ok(p) => Some(p),
        Err(e) if e.to_string() == "audio output unavailable" => {
            eprintln!("skipping {name}: {e:#}");
            None
        }
        Err(e) => panic!("{name}: {e}"),
    }
}

fn colour(f: &VideoFrame) -> &'static str {
    let i = ((f.height / 2 * f.width + f.width / 2) * 4) as usize;
    match (f.bgra[i + 2] > 128, f.bgra[i + 1] > 128, f.bgra[i] > 128) {
        (true, false, false) => "red",
        (false, true, false) => "green",
        (false, false, true) => "blue",
        _ => "other",
    }
}

fn expected_colour(pts: f64) -> &'static str {
    match pts {
        p if p < 0.99 => "red",
        p if p < 1.99 => "green",
        _ => "blue",
    }
}

struct Shown {
    position: f64,
    pts: f64,
    colour: &'static str,
}

/// Poll like the UI does until the player reports the end, within `budget`.
fn play_to_end(player: &Player, budget: Duration) -> Vec<Shown> {
    let deadline = Instant::now() + budget;
    let mut shown = Vec::new();
    loop {
        if let Some(f) = player.take_frame() {
            shown.push(Shown { position: player.position(), pts: f.pts, colour: colour(&f) });
        }
        if player.is_finished() {
            return shown;
        }
        assert!(Instant::now() < deadline, "did not finish within {budget:?}; error: {:?}", player.error());
        thread::sleep(Duration::from_millis(4));
    }
}

/// Poll until a frame satisfying `want` is shown, within `budget`.
fn wait_for_frame(player: &Player, budget: Duration, want: impl Fn(&VideoFrame) -> bool) -> VideoFrame {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(f) = player.take_frame()
            && want(&f)
        {
            return f;
        }
        assert!(Instant::now() < deadline, "no matching frame within {budget:?}");
        thread::sleep(Duration::from_millis(4));
    }
}

fn assert_in_sync(name: &str, shown: &[Shown]) {
    assert!(shown.len() >= 60, "{name}: only {} of 75 frames shown", shown.len());
    for s in shown {
        assert_eq!(s.colour, expected_colour(s.pts), "{name}: frame at {:.3}s", s.pts);
        // A frame is never shown early, and never more than ~3 frames late.
        assert!(s.pts <= s.position + 0.01, "{name}: frame {:.3}s shown early at {:.3}s", s.pts, s.position);
        assert!(s.position - s.pts < 0.15, "{name}: frame {:.3}s shown late at {:.3}s", s.pts, s.position);
    }
    assert!(shown.windows(2).all(|w| w[1].pts > w[0].pts), "{name}: frames out of order");
}

#[test]
fn video_only_plays_in_sync_on_the_wall_clock() {
    let player = open("video_only.mp4").unwrap();
    assert!(player.info.audio.is_none());
    let v = player.info.video.as_ref().unwrap();
    assert_eq!((v.codec, v.width, v.height), ("H.264", 320, 240));
    let d = player.info.duration.unwrap();
    assert!((d - 3.0).abs() < 0.05, "duration {d}");
    let started = Instant::now();
    let shown = play_to_end(&player, Duration::from_secs(8));
    assert_in_sync("video_only.mp4", &shown);
    let elapsed = started.elapsed().as_secs_f64();
    assert!((2.8..4.0).contains(&elapsed), "3 s of video took {elapsed:.2}s");
}

#[test]
fn seek_while_paused_shows_the_target_frame_and_holds_position() {
    let mut player = open("video_only.mp4").unwrap();
    player.set_paused(true);
    player.seek(2.3);
    let f = wait_for_frame(&player, Duration::from_secs(5), |f| f.pts > 2.2);
    assert!((f.pts - 2.28).abs() < 0.05, "showed {:.3}s for a seek to 2.3s", f.pts);
    assert_eq!(colour(&f), "blue");
    let pos = player.position();
    assert!((pos - 2.3).abs() < 1e-6, "position {pos} while paused");
    assert!(!player.is_finished());

    // Seeking backwards while paused works too, through a keyframe.
    player.seek(0.5);
    let f = wait_for_frame(&player, Duration::from_secs(5), |f| f.pts < 1.0 && f.pts > 0.4);
    assert_eq!(colour(&f), "red");
}

#[test]
fn seeking_after_the_end_resumes_playback() {
    let mut player = open("video_only.mp4").unwrap();
    player.seek(2.6);
    play_to_end(&player, Duration::from_secs(5));
    player.seek(1.2);
    assert!(!player.is_finished(), "a seek must clear the finished state");
    let f = wait_for_frame(&player, Duration::from_secs(5), |_| true);
    assert_eq!(colour(&f), "green", "frame at {:.3}s", f.pts);
    let shown = play_to_end(&player, Duration::from_secs(5));
    assert!(shown.iter().any(|s| s.colour == "blue"));
}

#[test]
fn h264_with_aac_plays_in_sync_on_the_audio_clock() {
    let Some(player) = open("h264_aac.mp4") else { return };
    let a = player.info.audio.as_ref().unwrap();
    assert_eq!((a.codec.as_str(), a.sample_rate, a.channels), ("AAC", 44_100, 2));
    let shown = play_to_end(&player, Duration::from_secs(8));
    assert_in_sync("h264_aac.mp4", &shown);
    // Pre-roll: audio waits for the first frame, so it is not late.
    let first = &shown[0];
    assert!(first.pts < 0.01 && first.position - first.pts < 0.02, "first frame {:.3}s shown at {:.3}s", first.pts, first.position);
    let end = player.position();
    assert!((end - 3.0).abs() < 0.1, "ended at {end:.3}s");
}

#[test]
fn av1_ten_bit_with_surround_opus_plays_in_sync() {
    let Some(player) = open("av1_10bit_opus51.mkv") else { return };
    let a = player.info.audio.as_ref().unwrap();
    assert_eq!((a.codec.as_str(), a.channels), ("Opus", 6));
    assert_eq!(player.info.video.as_ref().unwrap().codec, "AV1");
    let shown = play_to_end(&player, Duration::from_secs(8));
    assert_in_sync("av1_10bit_opus51.mkv", &shown);
}

#[test]
fn pause_freezes_the_audio_clock() {
    let Some(mut player) = open("h264_mp3.mkv") else { return };
    wait_for_frame(&player, Duration::from_secs(3), |f| f.pts > 0.5);
    player.set_paused(true);
    let at_pause = player.position();
    let deadline = Instant::now() + Duration::from_millis(400);
    while Instant::now() < deadline {
        // At most one in-flight device callback (~10-20 ms) after pausing.
        assert!(player.position() - at_pause < 0.03, "clock moved while paused");
        thread::sleep(Duration::from_millis(10));
    }
    player.set_paused(false);
    let resumed = Instant::now();
    while player.position() < at_pause + 0.2 {
        assert!(resumed.elapsed() < Duration::from_secs(2), "clock did not resume");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn unsupported_video_falls_back_to_audio() {
    // VP8: no decoder on any platform.
    let Some(player) = open("vp8_opus.webm") else { return };
    assert!(player.info.video.is_none());
    assert_eq!(player.info.video_unsupported.as_deref(), Some("VP8 video is not supported"));
    assert!(player.info.audio.is_some());
    play_to_end(&player, Duration::from_secs(8));
}

/// VP9 4:4:4: VideoToolbox accepts it and returns nothing, which used to
/// leave the picture black without a word. It must say why, here or (with
/// no VP9 decoder at all) when the file opens.
#[test]
fn vp9_four_four_four_says_why_it_has_no_picture() {
    let Some(player) = open("vp9_444_opus.webm") else { return };
    if let Some(why) = &player.info.video_unsupported {
        assert!(why.contains("VP9"), "{why}");
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let why = loop {
        if let Some(e) = player.error() {
            break e;
        }
        assert!(Instant::now() < deadline, "no error for undecodable VP9 4:4:4");
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(why, "Can't decode the video: VP9 4:4:4 video (profile 1) is not supported");
    assert!(player.info.audio.is_some(), "the sound still plays");
}

#[test]
fn audio_only_files_play_to_the_end() {
    for name in ["tagged.mp3", "mono_96k.flac", "stereo.opus", "stereo.ogg", "stereo_22k.wav"] {
        let Some(player) = open(name) else { return };
        assert!(player.info.video.is_none());
        // Measure speed from when sound starts: how long a device takes to
        // start pulling audio varies (about 2 s through PulseAudio's ALSA
        // plugin), and the clock correctly holds until it does.
        let deadline = Instant::now() + Duration::from_secs(8);
        while player.position() < 0.1 {
            assert!(Instant::now() < deadline, "{name}: playback never started");
            thread::sleep(Duration::from_millis(5));
        }
        let started = Instant::now();
        let from = player.position();
        play_to_end(&player, Duration::from_secs(6));
        let (elapsed, played) = (started.elapsed().as_secs_f64(), player.position() - from);
        assert!((played - (2.0 - from)).abs() < 0.1, "{name}: ended at {:.3}s", player.position());
        let speed = played / elapsed;
        assert!((0.9..1.1).contains(&speed), "{name}: played {played:.2}s of audio in {elapsed:.2}s");
    }
}

/// Symphonia's MP4 and Matroska readers cannot seek after end of stream;
/// the engine reopens the file. Exercise every container.
#[test]
fn seek_back_after_the_end_works_in_every_container() {
    for name in
        ["h264_aac.mp4", "h264_mp3.mkv", "av1_opus.webm", "tagged.mp3", "mono_96k.flac", "stereo.ogg", "stereo_22k.wav"]
    {
        let Some(mut player) = open(name) else { return };
        let dur = player.info.duration.unwrap_or_else(|| panic!("{name}: no duration"));
        player.seek(dur - 0.3);
        play_to_end(&player, Duration::from_secs(5));
        player.seek(0.5);
        assert!(!player.is_finished(), "{name}: finished right after seeking back");
        if player.info.video.is_some() {
            let f = wait_for_frame(&player, Duration::from_secs(5), |_| true);
            assert!((0.4..1.0).contains(&f.pts), "{name}: first frame after seek at {:.3}s", f.pts);
            assert_eq!(colour(&f), "red", "{name}");
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while player.position() < 0.8 {
            assert!(Instant::now() < deadline, "{name}: stuck at {:.3}s after seeking back", player.position());
            thread::sleep(Duration::from_millis(10));
        }
        assert!(player.error().is_none(), "{name}: {:?}", player.error());
    }
}

/// HEVC and VP9 play through the OS hardware decoder where there is one;
/// elsewhere they fall back to audio (covered above).
#[test]
fn hardware_decoded_video_plays_in_sync_and_seeks() {
    for name in ["hevc_aac.mp4", "hevc_10bit_opus.mkv", "vp9_opus.webm"] {
        let Some(mut player) = open(name) else { return };
        let Some(video) = player.info.video.clone() else {
            eprintln!("skipping {name}: {:?}", player.info.video_unsupported);
            if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
                panic!("{name} should decode on Apple silicon");
            }
            continue;
        };
        assert!(video.decoder.contains("hardware"), "{name}: {}", video.decoder);
        let shown = play_to_end(&player, Duration::from_secs(8));
        assert_in_sync(name, &shown);
        player.set_paused(true);
        player.seek(1.4);
        let f = wait_for_frame(&player, Duration::from_secs(5), |f| f.pts > 1.3);
        assert_eq!(colour(&f), "green", "{name}: frame at {:.3}s after seek", f.pts);
    }
}

#[test]
fn a_paused_file_shows_its_first_frame_when_video_starts_after_zero() {
    // No edit list: the first frame is stamped 0.08 s while the clock
    // starts at 0. Paused there, the picture must still appear.
    let mut player = open("late_start.mp4").unwrap();
    player.set_paused(true);
    let f = wait_for_frame(&player, Duration::from_secs(2), |_| true);
    assert!((f.pts - 0.08).abs() < 0.01, "first frame at {:.3}s", f.pts);
    assert_eq!(colour(&f), "red");
    assert!(player.position() < 0.01, "still at the start: {:.3}s", player.position());
}

#[test]
fn video_that_outlasts_its_audio_plays_to_the_end_and_finishes() {
    // Audio stops at 2 s, video runs to 3 s. The clock keeps running after
    // the audio ends, so the last second still plays and the file finishes
    // (which is what moves a playlist on).
    let Some(player) = open("audio_ends_first.mp4") else { return };
    let shown = play_to_end(&player, Duration::from_secs(8));
    let last = shown.last().expect("frames shown");
    assert!(last.pts > 2.9, "stopped at {:.3}s, short of the 3 s of video", last.pts);
    assert_eq!(last.colour, "blue");
    assert!(shown.iter().filter(|s| s.pts > 2.0).count() >= 20, "the last second was skipped, not played");
}
